/*****************************************************************************
Copyright (C) OBS Studio contributors (https://github.com/obsproject/obs-studio)

Ported to Rust for Better Cam from OBS Studio, plugins/nv-filters/nvidia-videofx-filter.c: the three filters
`nv_greenscreen_filter`, `nv_blur_filter` and `nv_background_blur_filter` (`nvvfx_filter_update`,
`nvvfx_filter_defaults`, `nvvfx_filter_properties`, `nvvfx_filter_render` and `draw_greenscreen_blur`). The pixel work
is in src/gpu/shaders/{nv_mask,nv_blend}.hlsl, ports of plugins/nv-filters/data/{rtx_greenscreen,rtx_blur}.effect.

This program is free software: you can redistribute it and/or modify
it under the terms of the GNU General Public License as published by
the Free Software Foundation, either version 2 of the License, or
(at your option) any later version.

This program is distributed in the hope that it will be useful,
but WITHOUT ANY WARRANTY; without even the implied warranty of
MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
GNU General Public License for more details.

You should have received a copy of the GNU General Public License
along with this program.  If not, see <http://www.gnu.org/licenses/>.
*****************************************************************************/

//! NVIDIA Background Removal, Blur and Background Blur (plan 4.6, 6.4): [`NvFilter`] is the [`GpuFilter`] of all three.
//!
//! What it does, per frame, follows OBS's `nvvfx_filter_render`:
//! * Background Removal: the segmentation effect makes a mask; the shader scales the picture's alpha by
//!   `smoothstep(threshold - 0.1, threshold, mask)`. `processing_interval` N runs the effect every Nth frame and
//!   reuses the mask in between.
//! * Background Blur: the same effect on the Performance model (mode 1, whatever `mode` says) feeds the blur effect;
//!   the shader mixes blurred and sharp picture by `smoothstep(0.85, 0.95, mask)`.
//! * Blur: the blur effect alone; the shader shows the blurred picture. An `intensity` of 0 passes the picture
//!   through (OBS skips the filter).
//!
//! Where it differs from OBS, on purpose:
//! * OBS builds everything on its graphics thread and so stalls for the model load. Here a loader thread creates the
//!   effects, loads the model and allocates the images ([`Pipeline::build`]); until it is done the filter passes the
//!   picture through and the card says "Loading model…". A change of `mode` builds the new pipeline on the loader
//!   thread while the old one keeps working, then swaps; a change of picture size does the same without the old one.
//! * The picture comes from the chain's pooled texture, so it is copied once per frame into the texture NVIDIA's CUDA
//!   image is registered on; OBS renders into that texture instead.
//! * A failure never takes the engine down: the pipeline is released, the picture passes through and the card shows
//!   the message (as a [`Note::Error`]).

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Direct3D11::ID3D11PixelShader;

use super::effect::{Kind, Pipeline, Spec, MIN_SEGMENTATION_SIZE};
use super::Runtime;
use crate::fx::{pf, pi, GpuFilter, Note, Params};
use crate::gpu::{ConstantBuffer, GpuDevice, GpuResult, Pass, Renderer, RgbaTexture};

pub const REMOVAL_KIND: &str = "nv_greenscreen_filter";
pub const BLUR_KIND: &str = "nv_blur_filter";
pub const BACKGROUND_BLUR_KIND: &str = "nv_background_blur_filter";

/// The settings the filters read (OBS setting names).
pub const REMOVAL_KEYS: &[&str] = &["mode", "threshold", "processing_interval"];
pub const BLUR_KEYS: &[&str] = &["intensity"];

/// `S_MODE_PERF`: Background Blur always runs the segmentation on the Performance model.
const MODE_PERFORMANCE: u32 = 1;
/// How the threshold is capped before the shader gets it (`min(filter->threshold, 0.95f)`).
pub const THRESHOLD_CAP: f32 = 0.95;
/// PCI vendor id of NVIDIA.
pub const NVIDIA_VENDOR_ID: u32 = 0x10DE;
/// What the card says while the loader thread works.
pub const LOADING: &str = "Loading model…";
/// How often a filter that waits for the runtime to be installed looks again.
const RETRY_EVERY: Duration = Duration::from_secs(3);

/// The settings of one filter.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Settings {
    pub mode: u32,
    pub threshold: f32,
    pub interval: u32,
    pub intensity: f32,
}

/// Reads the settings with OBS's defaults (`nvvfx_filter_defaults`) and the ranges of `nvvfx_filter_properties`.
pub fn settings(p: &Params) -> Settings {
    Settings {
        mode: pi(p, "mode", 0, 0, 3) as u32,
        threshold: pf(p, "threshold", 1.0, 0.0, 1.0),
        interval: pi(p, "processing_interval", 1, 1, 4) as u32,
        intensity: pf(p, "intensity", 0.5, 0.0, 1.0),
    }
}

/// `processing_counter % processing_interval == 0` of OBS: whether the segmentation runs on this frame. The counter
/// is the filter's own; the first frame always runs.
pub fn interval_due(counter: &mut u32, interval: u32) -> bool {
    if *counter % interval.max(1) == 0 {
        *counter = 1;
        true
    } else {
        *counter += 1;
        false
    }
}

/// Why the effects cannot run on the adapter the picture is rendered on (`None`: they can).
pub fn adapter_problem(vendor_id: u32, adapter: &str) -> Option<String> {
    if vendor_id == NVIDIA_VENDOR_ID {
        return None;
    }
    let name = if adapter.trim().is_empty() { "another graphics adapter" } else { adapter.trim() };
    Some(format!("NVIDIA's video effects need the picture to be rendered on the NVIDIA GPU, but Better Cam is rendering on {name}."))
}

/// Why the segmentation models cannot take a picture of this size (`None`: they can). The blur effect alone has no
/// minimum (OBS checks only the two segmenting filters).
pub fn size_problem(spec: &Spec) -> Option<String> {
    let (min_w, min_h) = MIN_SEGMENTATION_SIZE;
    (spec.kind.segments() && (spec.width < min_w || spec.height < min_h)).then(|| {
        format!("NVIDIA's background effects need a picture of at least {min_w}×{min_h} pixels; this one is {}×{}.", spec.width, spec.height)
    })
}

// ---- the loader thread ---------------------------------------------------------------------------------------------

struct BuildJob {
    gen: u64,
    spec: Spec,
    strength: f32,
    gpu: Arc<GpuDevice>,
}

enum Job {
    Build(BuildJob),
    /// A pipeline the render thread is done with: dropped here, so freeing GPU memory never stalls a frame.
    Release(Box<Pipeline>),
}

struct Built {
    gen: u64,
    result: Result<Box<Pipeline>, String>,
    /// The failure may go away by itself (the runtime is not installed yet), so look again later.
    retry: bool,
}

/// Once a pipeline was built the runtime stays loaded for the rest of the process, as it does in OBS: unloading CUDA
/// and TensorRT libraries that have run is not something to do for a filter that was removed.
static RESIDENT: OnceLock<Arc<Runtime>> = OnceLock::new();

fn build(job: &BuildJob) -> Built {
    let fail = |message: String, retry: bool| Built { gen: job.gen, result: Err(message), retry };
    // Precedence of the messages: no RTX GPU, runtime missing or too old (both `Runtime::open`), the wrong adapter, the
    // picture size, then NVIDIA's own errors.
    let rt = match Runtime::open() {
        Ok(rt) => rt,
        Err(message) => return fail(message, super::detect_gpu().is_some()),
    };
    if let Some(message) = adapter_problem(job.gpu.vendor_id, &job.gpu.adapter_name) {
        return fail(message, false);
    }
    if let Some(message) = size_problem(&job.spec) {
        return fail(message, false);
    }
    match Pipeline::build(&rt, &job.gpu, job.spec, job.strength) {
        Ok(p) => {
            let _ = RESIDENT.set(rt);
            Built { gen: job.gen, result: Ok(Box::new(p)), retry: false }
        }
        Err(message) => fail(message, false),
    }
}

fn loader(jobs: Receiver<Job>, done: Sender<Built>) {
    while let Ok(first) = jobs.recv() {
        // Everything that queued up meanwhile: releases run, only the newest build matters.
        let mut newest = None;
        for job in std::iter::once(first).chain(std::iter::from_fn(|| jobs.try_recv().ok())) {
            match job {
                Job::Release(pipeline) => drop(pipeline),
                Job::Build(b) => newest = Some(b),
            }
        }
        if let Some(job) = newest {
            let built = catch_unwind(AssertUnwindSafe(|| build(&job)))
                .unwrap_or_else(|_| Built { gen: job.gen, result: Err("NVIDIA's video effect failed unexpectedly while loading.".into()), retry: false });
            if done.send(built).is_err() {
                break;
            }
        }
    }
}

struct Worker {
    jobs: Sender<Job>,
    done: Receiver<Built>,
}

impl Worker {
    fn spawn() -> Result<Worker, String> {
        let (jobs, jobs_rx) = channel();
        let (done_tx, done) = channel();
        // Detached on purpose: the thread ends when the filter (the sender) is gone, after the build it may be in.
        std::thread::Builder::new()
            .name("nvidia effects".into())
            .spawn(move || loader(jobs_rx, done_tx))
            .map_err(|e| format!("Could not start the thread that loads NVIDIA's model: {e}"))?;
        Ok(Worker { jobs, done })
    }
}

// ---- the filter ------------------------------------------------------------------------------------------------------

/// A pipeline in use.
struct Active {
    pipeline: Box<Pipeline>,
    /// `processing_counter` of OBS.
    counter: u32,
    /// The blur intensity the pipeline's effect has.
    strength: f32,
}

/// The build that is in flight.
struct Requested {
    gen: u64,
    spec: Spec,
    strength: f32,
    /// A look to see whether a missing runtime came back: no "Loading model…" on the card.
    quiet: bool,
}

struct Failed {
    spec: Spec,
    message: String,
    retry_at: Option<Instant>,
}

/// What the card shows now, to send only changes.
#[derive(Default)]
struct Shown {
    loading: bool,
    error: Option<String>,
}

/// Counters of one filter (tests and diagnostics).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Stats {
    /// Frames the filter rendered.
    pub frames: u64,
    /// Frames on which an effect ran (not the ones that reused the mask).
    pub runs: u64,
    /// Pipelines built.
    pub builds: u64,
    /// How long the newest build took (model load included).
    pub last_load: Option<Duration>,
}

pub struct NvFilter {
    kind: Kind,
    settings: Settings,
    worker: Option<Worker>,
    gen: u64,
    requested: Option<Requested>,
    active: Option<Active>,
    failed: Option<Failed>,
    shown: Shown,
    notes: Vec<Note>,
    constants: Option<ConstantBuffer>,
    stats: Stats,
}

pub fn make_removal() -> Box<dyn GpuFilter> {
    Box::new(NvFilter::new(Kind::Removal))
}

pub fn make_blur() -> Box<dyn GpuFilter> {
    Box::new(NvFilter::new(Kind::Blur))
}

pub fn make_background_blur() -> Box<dyn GpuFilter> {
    Box::new(NvFilter::new(Kind::BackgroundBlur))
}

impl NvFilter {
    pub fn new(kind: Kind) -> NvFilter {
        NvFilter {
            kind,
            settings: settings(&Params::new()),
            worker: None,
            gen: 0,
            requested: None,
            active: None,
            failed: None,
            shown: Shown::default(),
            notes: Vec::new(),
            constants: None,
            stats: Stats::default(),
        }
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// A pipeline for the newest settings and the picture size it last saw is running.
    pub fn is_ready(&self, size: (u32, u32)) -> bool {
        self.active.as_ref().is_some_and(|a| a.pipeline.spec == self.wanted(size))
    }

    /// The failure the card shows, if any.
    pub fn failure(&self) -> Option<&str> {
        self.failed.as_ref().map(|f| f.message.as_str())
    }

    /// What the pipeline for a picture of `size` is built from.
    fn wanted(&self, size: (u32, u32)) -> Spec {
        let mode = match self.kind {
            Kind::Removal => self.settings.mode,
            Kind::BackgroundBlur => MODE_PERFORMANCE,
            Kind::Blur => 0,
        };
        Spec { kind: self.kind, mode, width: size.0, height: size.1 }
    }

    /// OBS skips a blur filter whose intensity is 0 (`has_blur && filter->strength == 0`).
    fn skipped(&self) -> bool {
        self.kind.blurs() && self.settings.intensity == 0.0
    }

    fn release(&mut self, pipeline: Box<Pipeline>) {
        match &self.worker {
            Some(w) => {
                // If the loader is gone the pipeline comes back in the error and is dropped here.
                let _ = w.jobs.send(Job::Release(pipeline));
            }
            None => drop(pipeline),
        }
    }

    /// Takes the finished builds from the loader thread.
    fn collect(&mut self) {
        loop {
            let Some(worker) = &self.worker else { return };
            let done = match worker.done.try_recv() {
                Ok(done) => done,
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    // The loader thread is gone (it never is, short of a crash inside NVIDIA's code): start another.
                    self.worker = None;
                    if let Some(q) = self.requested.take() {
                        self.failed = Some(Failed { spec: q.spec, message: "NVIDIA's model loader stopped unexpectedly.".into(), retry_at: Some(Instant::now() + RETRY_EVERY) });
                    }
                    return;
                }
            };
            let current = self.requested.as_ref().is_some_and(|q| q.gen == done.gen);
            if !current {
                // Superseded while it was building.
                if let Ok(p) = done.result {
                    self.release(p);
                }
                continue;
            }
            let Some(q) = self.requested.take() else { continue };
            match done.result {
                Ok(pipeline) => {
                    self.stats.builds += 1;
                    self.stats.last_load = Some(pipeline.load_time);
                    self.failed = None;
                    let old = self.active.replace(Active { pipeline, counter: 0, strength: q.strength });
                    if let Some(old) = old {
                        self.release(old.pipeline);
                    }
                }
                Err(message) => {
                    self.failed = Some(Failed { spec: q.spec, message, retry_at: done.retry.then(|| Instant::now() + RETRY_EVERY) });
                }
            }
        }
    }

    fn request(&mut self, r: &Renderer, spec: Spec, quiet: bool) {
        if self.worker.is_none() {
            match Worker::spawn() {
                Ok(w) => self.worker = Some(w),
                Err(message) => {
                    self.failed = Some(Failed { spec, message, retry_at: None });
                    return;
                }
            }
        }
        self.gen += 1;
        let strength = self.settings.intensity;
        let job = Job::Build(BuildJob { gen: self.gen, spec, strength, gpu: r.gpu.clone() });
        let sent = self.worker.as_ref().is_some_and(|w| w.jobs.send(job).is_ok());
        if !sent {
            // The loader thread died; make a new one at the next frame.
            self.worker = None;
            self.failed = Some(Failed { spec, message: "NVIDIA's model loader stopped unexpectedly.".into(), retry_at: Some(Instant::now() + RETRY_EVERY) });
            return;
        }
        if !quiet {
            self.failed = None;
        }
        self.requested = Some(Requested { gen: self.gen, spec, strength, quiet });
    }

    /// Makes sure a pipeline for `want` is running or being built.
    fn ensure(&mut self, r: &Renderer, want: Spec) {
        // A pipeline for another picture size is no use any more; another mode only is: it keeps working until its
        // replacement is ready.
        if self.active.as_ref().is_some_and(|a| (a.pipeline.spec.width, a.pipeline.spec.height) != (want.width, want.height)) {
            if let Some(a) = self.active.take() {
                self.release(a.pipeline);
            }
        }
        if self.active.as_ref().is_some_and(|a| a.pipeline.spec == want) {
            return;
        }
        if self.requested.as_ref().is_some_and(|q| q.spec == want) {
            return;
        }
        let mut quiet = false;
        if let Some(f) = &self.failed {
            if f.spec == want {
                match f.retry_at {
                    Some(t) if Instant::now() >= t => quiet = true,
                    _ => return,
                }
            }
        }
        self.request(r, want, quiet);
    }

    /// One frame on the active pipeline: effects, then the final pass. `Err` is a message for the card.
    fn run_frame(&mut self, r: &Renderer, input: &RgbaTexture, output: &RgbaTexture) -> Result<(), String> {
        let Settings { threshold, interval, intensity, .. } = self.settings;
        let kind = self.kind;
        if self.constants.is_none() {
            self.constants = Some(r.constants(16).map_err(|e| e.to_string())?);
        }
        let Some(a) = self.active.as_mut() else { return Err("no pipeline".into()) };
        if kind.blurs() && a.strength != intensity {
            a.pipeline.set_strength(intensity)?;
            a.strength = intensity;
        }
        let run_mask = if kind == Kind::Removal { interval_due(&mut a.counter, interval) } else { true };
        a.pipeline.process(&r.gpu.context, &input.texture, run_mask)?;
        if run_mask {
            self.stats.runs += 1;
        }

        let (Some(a), Some(constants)) = (self.active.as_ref(), self.constants.as_ref()) else { return Err("no pipeline".into()) };
        match kind {
            Kind::Removal => {
                let ps = shader(r, "nv_mask", include_str!("../../gpu/shaders/nv_mask.hlsl")).map_err(|e| e.to_string())?;
                constants.write(&r.gpu.context, &[threshold.min(THRESHOLD_CAP), 0.0, 0.0, 0.0]).map_err(|e| e.to_string())?;
                let mask = a.pipeline.mask.as_ref().ok_or("the mask texture is missing")?;
                r.draw(&Pass {
                    ps: &ps,
                    srvs: &[Some(input.srv.clone()), Some(mask.srv.clone())],
                    constants: Some(constants),
                    target: &output.rtv,
                    width: output.width,
                    height: output.height,
                });
            }
            Kind::Blur | Kind::BackgroundBlur => {
                let ps = shader(r, "nv_blend", include_str!("../../gpu/shaders/nv_blend.hlsl")).map_err(|e| e.to_string())?;
                let use_mask: f32 = if kind == Kind::BackgroundBlur { 1.0 } else { 0.0 };
                constants.write(&r.gpu.context, &[use_mask, 0.0, 0.0, 0.0]).map_err(|e| e.to_string())?;
                let blurred = a.pipeline.blurred.as_ref().ok_or("the blur texture is missing")?;
                r.draw(&Pass {
                    ps: &ps,
                    srvs: &[Some(input.srv.clone()), Some(blurred.srv.clone()), a.pipeline.mask.as_ref().map(|m| m.srv.clone())],
                    constants: Some(constants),
                    target: &output.rtv,
                    width: output.width,
                    height: output.height,
                });
            }
        }
        Ok(())
    }

    /// Tells the card about the changes since the last call.
    fn sync_notes(&mut self) {
        let loading = self.requested.as_ref().is_some_and(|q| !q.quiet);
        if loading != self.shown.loading {
            self.shown.loading = loading;
            self.notes.push(Note::Status(if loading { LOADING.into() } else { String::new() }));
        }
        let error = self.failed.as_ref().map(|f| f.message.clone());
        if error != self.shown.error {
            self.notes.push(Note::Error(error.clone().unwrap_or_default()));
            self.shown.error = error;
        }
    }
}

fn shader(r: &Renderer, name: &'static str, source: &'static str) -> GpuResult<ID3D11PixelShader> {
    r.shaders.pixel_source(name, source)
}

impl Drop for NvFilter {
    fn drop(&mut self) {
        // The pipeline is freed by the loader thread, so a removed card never stalls a frame.
        if let Some(a) = self.active.take() {
            self.release(a.pipeline);
        }
    }
}

impl GpuFilter for NvFilter {
    fn update(&mut self, _r: &Renderer, p: &Params) {
        self.settings = settings(p);
    }

    fn warm_up(&mut self, r: &Renderer) -> GpuResult<()> {
        shader(r, "nv_mask", include_str!("../../gpu/shaders/nv_mask.hlsl"))?;
        shader(r, "nv_blend", include_str!("../../gpu/shaders/nv_blend.hlsl"))?;
        Ok(())
    }

    fn take_notes(&mut self, out: &mut Vec<Note>) {
        out.append(&mut self.notes);
    }

    fn render(&mut self, r: &Renderer, input: &RgbaTexture, output: &RgbaTexture) -> GpuResult<()> {
        self.stats.frames += 1;
        self.collect();
        if self.skipped() {
            self.sync_notes();
            return r.copy(input, output);
        }
        let want = self.wanted((input.width, input.height));
        self.ensure(r, want);

        let usable = self.active.as_ref().is_some_and(|a| (a.pipeline.spec.width, a.pipeline.spec.height) == (input.width, input.height));
        let mut drawn = false;
        if usable {
            match catch_unwind(AssertUnwindSafe(|| self.run_frame(r, input, output))) {
                Ok(Ok(())) => drawn = true,
                Ok(Err(message)) => self.fail(want, message),
                Err(_) => self.fail(want, "NVIDIA's video effect failed unexpectedly.".into()),
            }
        }
        self.sync_notes();
        if drawn {
            Ok(())
        } else {
            r.copy(input, output)
        }
    }
}

impl NvFilter {
    /// A frame failed: the pipeline goes, the picture passes through and the card shows `message` until the settings
    /// or the picture size change.
    fn fail(&mut self, spec: Spec, message: String) {
        if let Some(a) = self.active.take() {
            self.release(a.pipeline);
        }
        self.failed = Some(Failed { spec, message, retry_at: None });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn params(v: serde_json::Value) -> Params {
        v.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn nvidia_settings_follow_obs_defaults_and_ranges() {
        assert_eq!(settings(&Params::new()), Settings { mode: 0, threshold: 1.0, interval: 1, intensity: 0.5 });
        let s = settings(&params(json!({"mode": 3, "threshold": 0.4, "processing_interval": 4, "intensity": 0.25})));
        assert_eq!((s.mode, s.threshold, s.interval, s.intensity), (3, 0.4, 4, 0.25));
        // Out of range values are clamped, wrong types give the default.
        let s = settings(&params(json!({"mode": 9, "threshold": 5, "processing_interval": 0, "intensity": -1})));
        assert_eq!((s.mode, s.threshold, s.interval, s.intensity), (3, 1.0, 1, 0.0));
        let s = settings(&params(json!({"mode": "x", "threshold": null, "processing_interval": 99, "intensity": "y"})));
        assert_eq!((s.mode, s.threshold, s.interval, s.intensity), (0, 1.0, 4, 0.5));
        assert_eq!(settings(&params(json!({"mode": 1.6}))).mode, 2);
    }

    #[test]
    fn nvidia_the_mask_runs_every_nth_frame() {
        for interval in 1..=4u32 {
            let mut counter = 0;
            let runs: Vec<u32> = (1..=100).filter(|_| interval_due(&mut counter, interval)).collect();
            assert_eq!(runs.len(), 100usize.div_ceil(interval as usize), "interval {interval}");
            assert_eq!(runs[0], 1, "the first frame always runs");
            assert!(runs.windows(2).all(|w| w[1] - w[0] == interval), "evenly spaced: {runs:?}");
        }
        // OBS: counter 0 runs and sets 1; 1, 2, 3 count up; 4 % 4 == 0 runs again.
        let mut c = 0;
        let seq: Vec<bool> = (0..9).map(|_| interval_due(&mut c, 4)).collect();
        assert_eq!(seq, [true, false, false, false, true, false, false, false, true]);
        // A shorter interval picked up mid-way applies at once.
        let mut c = 3;
        assert!(!interval_due(&mut c, 4) || c == 1);
        let mut c = 2;
        assert!(interval_due(&mut c, 2) && c == 1);
    }

    #[test]
    fn nvidia_the_adapter_must_be_nvidia() {
        assert_eq!(adapter_problem(NVIDIA_VENDOR_ID, "NVIDIA GeForce RTX 5080"), None);
        let m = adapter_problem(0x8086, "Intel(R) UHD Graphics").unwrap();
        assert!(m.contains("rendered on the NVIDIA GPU") && m.contains("Intel(R) UHD Graphics"), "{m}");
        assert!(adapter_problem(0x1414, "").unwrap().contains("another graphics adapter"));
    }

    #[test]
    fn nvidia_segmentation_needs_a_big_enough_picture_blur_does_not() {
        let spec = |kind, w, h| Spec { kind, mode: 0, width: w, height: h };
        assert_eq!(size_problem(&spec(Kind::Removal, 512, 288)), None);
        assert_eq!(size_problem(&spec(Kind::BackgroundBlur, 1920, 1080)), None);
        let m = size_problem(&spec(Kind::Removal, 511, 720)).unwrap();
        assert!(m.contains("512×288") && m.contains("511×720"), "{m}");
        assert!(size_problem(&spec(Kind::BackgroundBlur, 640, 287)).is_some());
        assert_eq!(size_problem(&spec(Kind::Blur, 64, 48)), None);
    }

    /// A filter that passes the picture through and says what it is told to say.
    struct Chatty(Vec<Note>);

    impl GpuFilter for Chatty {
        fn update(&mut self, _r: &Renderer, _p: &Params) {}
        fn render(&mut self, r: &Renderer, input: &RgbaTexture, output: &RgbaTexture) -> GpuResult<()> {
            r.copy(input, output)
        }
        fn take_notes(&mut self, out: &mut Vec<Note>) {
            out.append(&mut self.0);
        }
    }

    #[test]
    fn nvidia_notes_of_a_filter_become_status_and_error_lines_of_its_card() {
        use crate::fx::{Chain, FilterCfg, FilterError, FilterStatus};
        let r = crate::gpu::pipeline_tests::warp();
        fn chatty(kind: &str) -> Option<Box<dyn GpuFilter>> {
            (kind == "chatty").then(|| {
                Box::new(Chatty(vec![
                    Note::Status("Loading model…".into()),
                    Note::Error("boom".into()),
                    Note::Status(String::new()),
                    Note::Error(String::new()),
                ])) as Box<dyn GpuFilter>
            })
        }
        let mut chain = Chain::with_factory(chatty);
        chain.apply(&r, &[FilterCfg { id: "card".into(), kind: "chatty".into(), enabled: true, params: Params::new() }]);
        let _ = chain.run(&r, r.texture(8, 8).unwrap()).unwrap();
        let line = |m: &str| FilterStatus { id: "card".into(), message: m.into() };
        let error = |m: &str| FilterError { id: "card".into(), message: m.into() };
        assert_eq!(chain.take_status(), [line("Loading model…"), line("")]);
        assert_eq!(chain.take_errors(), [error("boom"), error("")]);
        // Said once: the next frame has nothing new.
        let _ = chain.run(&r, r.texture(8, 8).unwrap()).unwrap();
        assert!(chain.take_status().is_empty() && chain.take_errors().is_empty());
    }

    #[test]
    fn nvidia_blur_kinds_pick_their_effects() {
        assert!(Kind::Removal.segments() && !Kind::Removal.blurs());
        assert!(!Kind::Blur.segments() && Kind::Blur.blurs());
        assert!(Kind::BackgroundBlur.segments() && Kind::BackgroundBlur.blurs());
        // Background Blur always runs the Performance model; Removal takes the setting.
        let mut f = NvFilter::new(Kind::BackgroundBlur);
        f.settings.mode = 3;
        assert_eq!(f.wanted((1280, 720)).mode, 1);
        let mut f = NvFilter::new(Kind::Removal);
        f.settings.mode = 3;
        assert_eq!(f.wanted((1280, 720)).mode, 3);
    }
}
