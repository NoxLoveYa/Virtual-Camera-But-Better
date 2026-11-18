//! Tests of the NVIDIA filters on the real GPU and the real runtime. Each test returns early, saying why, when this PC
//! has no RTX GPU, no runtime or no picture of a person. The picture is `BETTER_CAM_TEST_PERSON`: a PNG or JPEG of a
//! person in the right half of a 16:9 frame (the tests never fetch or generate one). They run one at a time (the game
//! may be using the same card) and for seconds, not minutes.

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde_json::json;

use super::effect::{live_pipelines, Kind, Pipeline, Spec};
use super::filter::{NvFilter, NVIDIA_VENDOR_ID};
use super::Runtime;
use crate::fx::assets::decode_image;
use crate::fx::{Chain, FilterCfg, GpuFilter, Note, Params};
use crate::gpu::pipeline_tests::{fill_rgba, read_rgba, warp};
use crate::gpu::{AdapterKind, GpuDevice, GpuTimer, Pooled, Renderer};

/// One GPU test at a time.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

fn exclusive() -> MutexGuard<'static, ()> {
    ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner())
}

/// The renderer on the real GPU and the runtime, or `None` (the reason printed) when this PC cannot run the effects.
fn hardware(test: &str) -> Option<(Arc<Renderer>, Arc<Runtime>)> {
    let gpu = match GpuDevice::new(AdapterKind::Hardware) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("skipped {test}: no hardware adapter ({e})");
            return None;
        }
    };
    if gpu.vendor_id != NVIDIA_VENDOR_ID {
        eprintln!("skipped {test}: the default adapter is {}, not an NVIDIA GPU", gpu.adapter_name);
        return None;
    }
    let rt = match Runtime::open() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("skipped {test}: {e}");
            return None;
        }
    };
    Some((Arc::new(Renderer::new(Arc::new(gpu)).expect("renderer")), rt))
}

/// A picture of a person.
struct Picture {
    w: u32,
    h: u32,
    rgba: Vec<u8>,
}

impl Picture {
    fn load(test: &str) -> Option<Picture> {
        let Some(path) = std::env::var_os("BETTER_CAM_TEST_PERSON") else {
            eprintln!("skipped {test}: BETTER_CAM_TEST_PERSON is not set (a picture of a person is needed)");
            return None;
        };
        match decode_image(Path::new(&path)) {
            Ok(i) => Some(Picture { w: i.width, h: i.height, rgba: i.rgba }),
            Err(e) => {
                eprintln!("skipped {test}: cannot read {}: {e}", Path::new(&path).display());
                None
            }
        }
    }

    /// The picture scaled to `w` x `h` (nearest neighbour: only the size matters).
    fn scaled(&self, w: u32, h: u32) -> Picture {
        if (w, h) == (self.w, self.h) {
            return Picture { w, h, rgba: self.rgba.clone() };
        }
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                let (sx, sy) = (x * self.w / w, y * self.h / h);
                let i = ((sy * self.w + sx) * 4) as usize;
                rgba.extend_from_slice(&self.rgba[i..i + 4]);
            }
        }
        Picture { w, h, rgba }
    }

    fn upload(&self, r: &Renderer) -> Pooled {
        let t = r.texture(self.w, self.h).expect("texture");
        fill_rgba(r, &t, |x, y| {
            let i = ((y * self.w + x) * 4) as usize;
            [self.rgba[i], self.rgba[i + 1], self.rgba[i + 2], self.rgba[i + 3]]
        });
        t
    }
}

/// A plain gray picture of the same size.
fn gray(r: &Renderer, w: u32, h: u32) -> Pooled {
    let t = r.texture(w, h).expect("texture");
    fill_rgba(r, &t, |_, _| [90, 90, 90, 255]);
    t
}

/// Where things are in the test picture, as fractions of its size.
const FACE: (f32, f32) = (0.63, 0.40);
const SUIT: (f32, f32) = (0.66, 0.85);
const BACKGROUND: [(f32, f32); 3] = [(0.05, 0.50), (0.08, 0.15), (0.97, 0.12)];
/// A rectangle (x0, y0, x1, y1) on the face and one over the moon in the background.
const FACE_BOX: (f32, f32, f32, f32) = (0.56, 0.20, 0.70, 0.45);
const MOON_BOX: (f32, f32, f32, f32) = (0.03, 0.10, 0.30, 0.85);

fn px(buf: &[u8], w: u32, h: u32, at: (f32, f32)) -> [u8; 4] {
    let (x, y) = (((at.0 * w as f32) as u32).min(w - 1), ((at.1 * h as f32) as u32).min(h - 1));
    let i = ((y * w + x) * 4) as usize;
    [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]
}

fn luma(p: &[u8]) -> f32 {
    0.299 * p[0] as f32 + 0.587 * p[1] as f32 + 0.114 * p[2] as f32
}

/// Mean absolute difference of neighbouring pixels (luma, horizontal) in a box: the picture's high-frequency energy.
fn energy(buf: &[u8], w: u32, h: u32, b: (f32, f32, f32, f32)) -> f32 {
    let (x0, y0, x1, y1) = ((b.0 * w as f32) as u32, (b.1 * h as f32) as u32, (b.2 * w as f32) as u32, (b.3 * h as f32) as u32);
    let (mut sum, mut n) = (0.0f64, 0u32);
    for y in y0..y1 {
        for x in x0..x1 - 1 {
            let i = ((y * w + x) * 4) as usize;
            sum += (luma(&buf[i..]) - luma(&buf[i + 4..])).abs() as f64;
            n += 1;
        }
    }
    (sum / n as f64) as f32
}

/// Mean absolute difference of the colour channels of two pictures inside a box, in levels of 255.
fn mean_diff(a: &[u8], b: &[u8], w: u32, h: u32, bx: (f32, f32, f32, f32)) -> f32 {
    let (x0, y0, x1, y1) = ((bx.0 * w as f32) as u32, (bx.1 * h as f32) as u32, (bx.2 * w as f32) as u32, (bx.3 * h as f32) as u32);
    let (mut sum, mut n) = (0.0f64, 0u32);
    for y in y0..y1 {
        for x in x0..x1 {
            let i = ((y * w + x) * 4) as usize;
            for c in 0..3 {
                sum += (a[i + c] as f64 - b[i + c] as f64).abs();
                n += 1;
            }
        }
    }
    (sum / n as f64) as f32
}

/// A filter and the renderer it runs on.
struct Rig {
    r: Arc<Renderer>,
    f: NvFilter,
}

impl Rig {
    fn new(r: &Arc<Renderer>, kind: Kind, params: serde_json::Value) -> Rig {
        let mut f = NvFilter::new(kind);
        f.update(r, params.as_object().unwrap_or(&Params::new()));
        Rig { r: r.clone(), f }
    }

    fn set(&mut self, params: serde_json::Value) {
        self.f.update(&self.r, params.as_object().unwrap_or(&Params::new()));
    }

    /// One frame: the output texture and how long `render` took on the calling thread.
    fn frame(&mut self, input: &Pooled) -> (Pooled, Duration) {
        let out = self.r.texture(input.width, input.height).expect("output texture");
        let t = Instant::now();
        self.f.render(&self.r, input, &out).expect("render");
        (out, t.elapsed())
    }

    fn read(&self, t: &Pooled) -> Vec<u8> {
        read_rgba(&self.r, t)
    }

    /// Frames until a pipeline for the picture is running. The frames before it are the passed through input.
    fn wait_ready(&mut self, input: &Pooled, limit: Duration) -> (u32, Duration, Duration) {
        let started = Instant::now();
        let (mut frames, mut slowest) = (0, Duration::ZERO);
        while !self.f.is_ready((input.width, input.height)) {
            assert!(started.elapsed() < limit, "no pipeline after {limit:?}; failure: {:?}", self.f.failure());
            if let Some(msg) = self.f.failure() {
                panic!("the filter failed: {msg}");
            }
            let (out, took) = self.frame(input);
            slowest = slowest.max(took);
            if frames == 0 {
                // While loading the picture goes through unchanged.
                assert_eq!(self.read(&out), self.read(input), "the loading frame must be the input");
            }
            frames += 1;
            std::thread::sleep(Duration::from_millis(4));
        }
        (frames, started.elapsed(), slowest)
    }
}

fn gpu_memory_mib() -> u64 {
    let out = std::process::Command::new("nvidia-smi").args(["--query-gpu=memory.used", "--format=csv,noheader,nounits"]).output().expect("nvidia-smi");
    String::from_utf8_lossy(&out.stdout).lines().next().and_then(|l| l.trim().parse().ok()).expect("memory.used")
}

/// Waits until the loader threads have dropped everything that was handed to them.
fn wait_for_pipelines(n: usize) {
    let t = Instant::now();
    while live_pipelines() != n {
        assert!(t.elapsed() < Duration::from_secs(20), "{} pipelines alive, expected {n}", live_pipelines());
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn spec(kind: Kind, mode: u32, w: u32, h: u32) -> Spec {
    Spec { kind, mode, width: w, height: h }
}

// ---- the pipeline itself -------------------------------------------------------------------------------------------

#[test]
fn nvidia_the_texture_formats_are_seen_as_rgba_bytes() {
    let _g = exclusive();
    let Some((r, rt)) = hardware("nvidia_the_texture_formats_are_seen_as_rgba_bytes") else { return };
    let p = Pipeline::build(&rt, &r.gpu, spec(Kind::Removal, 1, 640, 360), 0.5).expect("build");
    // (pixelFormat, componentType, width, height): NVCV_RGBA = 6, NVCV_U8 = 1.
    assert_eq!(p.describe_source(), (super::ffi::NVCV_RGBA, super::ffi::NVCV_U8, 640, 360));
    eprintln!("the picture texture is NVCV_RGBA/U8, {:?} to build with its first run", p.load_time);
}

// ---- Background Removal --------------------------------------------------------------------------------------------

#[test]
fn nvidia_removal_runs_100_frames_and_cuts_out_the_person() {
    let _g = exclusive();
    let name = "nvidia_removal_runs_100_frames_and_cuts_out_the_person";
    let Some((r, _rt)) = hardware(name) else { return };
    let Some(pic) = Picture::load(name) else { return };
    assert_eq!((pic.w, pic.h), (1280, 720), "the test is written for a 1280x720 picture");
    let input = pic.upload(&r);
    let mut rig = Rig::new(&r, Kind::Removal, json!({}));

    let (passed, took, slowest) = rig.wait_ready(&input, Duration::from_secs(120));
    eprintln!(
        "removal ready after {took:?} ({passed} frames passed through meanwhile, the slowest took {slowest:?}); model load {:?}",
        rig.f.stats().last_load.unwrap()
    );

    let before = rig.f.stats();
    let started = Instant::now();
    let (mut lowest_face, mut highest_background) = (255u8, 0u8);
    let mut frame_times = Vec::new();
    for n in 0..100 {
        let (out, t) = rig.frame(&input);
        frame_times.push(t.as_secs_f64() * 1000.0);
        assert_eq!((out.width, out.height), (1280, 720), "the output has the input's size");
        let buf = rig.read(&out);
        assert_eq!(buf.len(), 1280 * 720 * 4);
        let face = px(&buf, 1280, 720, FACE);
        let suit = px(&buf, 1280, 720, SUIT);
        lowest_face = lowest_face.min(face[3]).min(suit[3]);
        for corner in BACKGROUND {
            highest_background = highest_background.max(px(&buf, 1280, 720, corner)[3]);
        }
        // The colour of the person is the picture's own.
        let src = px(&pic.rgba, 1280, 720, FACE);
        assert_eq!(&face[..3], &src[..3], "frame {n}: the colour is untouched");
        // The mask is not uniform: both a see-through and a solid region exist.
        let alphas: Vec<u8> = buf.chunks(4).map(|p| p[3]).collect();
        assert!(alphas.iter().any(|&a| a < 26) && alphas.iter().any(|&a| a > 230), "frame {n}: the mask is uniform");
    }
    let total = started.elapsed();
    let stats = rig.f.stats();
    eprintln!(
        "100 frames in {total:?} with read-back ({:.1} ms each); render() alone: mean {:.2} ms, max {:.2} ms; alpha: person >= {lowest_face}/255, background <= {highest_background}/255",
        total.as_secs_f64() * 10.0,
        frame_times.iter().sum::<f64>() / 100.0,
        frame_times.iter().cloned().fold(0.0, f64::max)
    );
    assert!(lowest_face >= 240, "the person keeps alpha near 1: {lowest_face}");
    assert!(highest_background < 13, "the background drops below the threshold: {highest_background}");
    assert_eq!(stats.runs - before.runs, 100, "interval 1 runs the effect on every frame");
    assert!(rig.f.failure().is_none());
}

/// The colour question of P13c: one identical picture through the chain and the output stage, with Background Removal
/// bypassed and enabled. Where the removal leaves a pixel fully opaque, the two results must be identical in every
/// channel (the filter changes alpha only); a translucent pixel is only counted.
#[test]
fn nvidia_removal_leaves_the_colour_of_opaque_pixels_alone() {
    use crate::gpu::output::{FitMode, OutputParams, OutputStage};
    let _g = exclusive();
    let name = "nvidia_removal_leaves_the_colour_of_opaque_pixels_alone";
    let Some((r, _rt)) = hardware(name) else { return };
    let Some(pic) = Picture::load(name) else { return };
    let (w, h) = (pic.w, pic.h);
    let stage = OutputStage::new(r.clone()).expect("output stage");
    let params = OutputParams { width: w, height: h, fit: FitMode::Fit, background: [0.0, 1.0, 0.0] };

    // Bypassed: what the unfiltered picture looks like after the output stage.
    let mut chain = Chain::new();
    chain.apply(&r, &[FilterCfg { enabled: false, ..cfg("a", "nv_greenscreen_filter", json!({})) }]);
    let chain_off = read_rgba(&r, &chain.run(&r, pic.upload(&r)).expect("run"));
    let out_off = read_rgba(&r, &stage.run(&chain.run(&r, pic.upload(&r)).expect("run"), &params).expect("output"));
    assert_eq!(chain_off, pic.rgba, "a bypassed filter hands the picture on untouched");

    // Enabled: frames until the model is loaded and the background is cut out, then a few more.
    chain.apply(&r, &[cfg("a", "nv_greenscreen_filter", json!({}))]);
    let started = Instant::now();
    let mut cut_out = None;
    while started.elapsed() < Duration::from_secs(120) {
        let out = chain.run(&r, pic.upload(&r)).expect("run");
        let buf = read_rgba(&r, &out);
        if px(&buf, w, h, BACKGROUND[0])[3] < 13 {
            cut_out = Some(out);
            break;
        }
        std::thread::sleep(Duration::from_millis(4));
    }
    let mut chain_on = cut_out.expect("the model never produced a cut-out");
    for _ in 0..5 {
        chain_on = chain.run(&r, pic.upload(&r)).expect("run");
    }
    let chain_buf = read_rgba(&r, &chain_on);
    let out_on = read_rgba(&r, &stage.run(&chain_on, &params).expect("output"));

    let (mut opaque, mut translucent, mut clear) = (0u32, 0u32, 0u32);
    // Per channel: sum and max of |filtered - unfiltered| over the opaque pixels, in the chain and after the output stage.
    let (mut chain_sum, mut chain_max, mut out_sum, mut out_max) = ([0u64; 3], [0u8; 3], [0u64; 3], [0u8; 3]);
    for i in (0..(w * h) as usize).map(|p| p * 4) {
        match chain_buf[i + 3] {
            255 => {
                opaque += 1;
                for c in 0..3 {
                    let d = chain_buf[i + c].abs_diff(pic.rgba[i + c]);
                    chain_sum[c] += d as u64;
                    chain_max[c] = chain_max[c].max(d);
                    let d = out_on[i + c].abs_diff(out_off[i + c]);
                    out_sum[c] += d as u64;
                    out_max[c] = out_max[c].max(d);
                }
            }
            0 => clear += 1,
            _ => translucent += 1,
        }
    }
    assert!(opaque > 20_000 && clear > 20_000, "the picture needs a person and a background: {opaque} opaque, {clear} clear");
    let mean = |s: [u64; 3]| s.map(|v| v as f64 / opaque as f64);
    eprintln!(
        "{opaque} opaque, {translucent} translucent, {clear} clear pixels of {}; mean |filtered - unfiltered| of the opaque pixels (R, G, B): in the chain {:.4?} (max {chain_max:?}), after the output stage {:.4?} (max {out_max:?})",
        w * h,
        mean(chain_sum),
        mean(out_sum)
    );
    assert_eq!(chain_max, [0; 3], "the chain changed the colour of an opaque pixel");
    assert_eq!(out_max, [0; 3], "the output stage changed the colour of an opaque pixel");
}

#[test]
fn nvidia_every_mode_loads_and_works() {
    let _g = exclusive();
    let name = "nvidia_every_mode_loads_and_works";
    let Some((r, _rt)) = hardware(name) else { return };
    let Some(pic) = Picture::load(name) else { return };
    let input = pic.upload(&r);
    for mode in 0..4u32 {
        let mut rig = Rig::new(&r, Kind::Removal, json!({ "mode": mode }));
        let (passed, took, _) = rig.wait_ready(&input, Duration::from_secs(120));
        let load = rig.f.stats().last_load.unwrap();
        // A few frames: the output is cut out.
        let mut out = rig.frame(&input).0;
        for _ in 0..4 {
            out = rig.frame(&input).0;
        }
        let buf = rig.read(&out);
        let (face, background) = (px(&buf, 1280, 720, FACE)[3], px(&buf, 1280, 720, BACKGROUND[0])[3]);
        eprintln!("mode {mode}: model load {load:?} (ready after {took:?}, {passed} frames passed through); alpha face {face}, background {background}");
        assert!(face >= 240 && background < 13, "mode {mode}: face {face}, background {background}");
    }
}

#[test]
fn nvidia_a_mode_change_keeps_the_frames_flowing() {
    let _g = exclusive();
    let name = "nvidia_a_mode_change_keeps_the_frames_flowing";
    let Some((r, _rt)) = hardware(name) else { return };
    let Some(pic) = Picture::load(name) else { return };
    let input = pic.upload(&r);
    let mut rig = Rig::new(&r, Kind::Removal, json!({ "mode": 0 }));
    rig.wait_ready(&input, Duration::from_secs(120));
    let builds = rig.f.stats().builds;

    // Mode 0 to 2 (the other big model): the old pipeline keeps cutting out while the new one loads.
    rig.set(json!({ "mode": 2 }));
    let started = Instant::now();
    let (mut frames, mut slowest, mut cut) = (0, Duration::ZERO, 0);
    while rig.f.stats().builds == builds {
        assert!(started.elapsed() < Duration::from_secs(60), "the new pipeline never arrived");
        let (out, took) = rig.frame(&input);
        slowest = slowest.max(took);
        frames += 1;
        if px(&rig.read(&out), 1280, 720, BACKGROUND[0])[3] < 13 {
            cut += 1;
        }
        std::thread::sleep(Duration::from_millis(8));
    }
    eprintln!("mode 0 to 2: new pipeline after {:?}; {frames} frames meanwhile, {cut} of them still cut out, slowest render() {slowest:?}", started.elapsed());
    assert_eq!(cut, frames, "every frame during the change is cut out by the old pipeline");
    assert!(slowest < Duration::from_millis(250), "a frame stalled for {slowest:?}");
    assert!(rig.f.is_ready((1280, 720)));
    wait_for_pipelines(1);
}

#[test]
fn nvidia_interval_4_runs_the_effect_on_every_fourth_frame_only() {
    let _g = exclusive();
    let name = "nvidia_interval_4_runs_the_effect_on_every_fourth_frame_only";
    let Some((r, _rt)) = hardware(name) else { return };
    let Some(pic) = Picture::load(name) else { return };
    let input = pic.upload(&r);
    let mut rig = Rig::new(&r, Kind::Removal, json!({ "processing_interval": 4 }));
    rig.wait_ready(&input, Duration::from_secs(120));

    // The frame that comes next is a counter-0 frame only right after a build; measure from the stats.
    let before = rig.f.stats().runs;
    for _ in 0..100 {
        rig.frame(&input);
    }
    let runs = rig.f.stats().runs - before;
    eprintln!("interval 4: the effect ran {runs} times in 100 frames");
    assert_eq!(runs, 25, "100 frames at interval 4");

    // The mask of a frame that ran is reused for the three after it: line up on a run with the real picture, then show
    // a gray picture; the person's place keeps its alpha until the next run.
    let gray = gray(&r, 1280, 720);
    loop {
        let before = rig.f.stats().runs;
        rig.frame(&input);
        if rig.f.stats().runs > before {
            break;
        }
    }
    let mut stale = Vec::new();
    for _ in 0..3 {
        let before = rig.f.stats().runs;
        let (out, _) = rig.frame(&gray);
        assert_eq!(rig.f.stats().runs, before, "no run between two runs");
        stale.push(px(&rig.read(&out), 1280, 720, FACE)[3]);
    }
    eprintln!("alpha at the face on the 3 gray frames after a run: {stale:?}");
    assert!(stale.iter().all(|&a| a >= 240), "the old mask is reused: {stale:?}");
}

// ---- Blur and Background Blur --------------------------------------------------------------------------------------

#[test]
fn nvidia_blur_softens_everything_and_background_blur_only_the_background() {
    let _g = exclusive();
    let name = "nvidia_blur_softens_everything_and_background_blur_only_the_background";
    let Some((r, _rt)) = hardware(name) else { return };
    let Some(pic) = Picture::load(name) else { return };
    let input = pic.upload(&r);
    let (w, h) = (1280u32, 720u32);
    let flat = (0.0, 0.0, 1.0, 1.0);

    let mut blur = Rig::new(&r, Kind::Blur, json!({ "intensity": 0.5 }));
    let (_, took, _) = blur.wait_ready(&input, Duration::from_secs(120));
    let out = blur.frame(&input).0;
    let buf = blur.read(&out);
    let (e_in, e_out) = (energy(&pic.rgba, w, h, flat), energy(&buf, w, h, flat));
    eprintln!("blur 0.5 (ready after {took:?}): high-frequency energy {e_in:.2} -> {e_out:.2}; face diff {:.1}", mean_diff(&pic.rgba, &buf, w, h, FACE_BOX));
    assert!(e_out < e_in * 0.6, "the whole picture gets softer: {e_in} -> {e_out}");
    assert!(mean_diff(&pic.rgba, &buf, w, h, FACE_BOX) > 2.0, "the face is blurred too");
    // More intensity, more blur. The effect takes the new strength on the render thread: time that frame.
    blur.set(json!({ "intensity": 1.0 }));
    let (out, change_took) = blur.frame(&input);
    let strong = blur.read(&out);
    eprintln!("blur 1.0: energy {:.2}; the frame that applied the new intensity took {change_took:?}", energy(&strong, w, h, flat));
    assert!(energy(&strong, w, h, flat) < e_out * 0.9, "a higher intensity blurs more");
    assert!(change_took < Duration::from_millis(50), "applying an intensity must not stall the frame: {change_took:?}");
    // Intensity 0 is a pass through (OBS skips the filter).
    blur.set(json!({ "intensity": 0.0 }));
    let (out, _) = blur.frame(&input);
    assert_eq!(blur.read(&out), pic.rgba, "intensity 0 passes the picture through");

    let mut bg = Rig::new(&r, Kind::BackgroundBlur, json!({ "intensity": 0.5 }));
    let (_, took, _) = bg.wait_ready(&input, Duration::from_secs(120));
    let mut out = bg.frame(&input).0;
    for _ in 0..4 {
        out = bg.frame(&input).0;
    }
    let buf = bg.read(&out);
    let face_diff = mean_diff(&pic.rgba, &buf, w, h, FACE_BOX);
    let (m_in, m_out) = (energy(&pic.rgba, w, h, MOON_BOX), energy(&buf, w, h, MOON_BOX));
    eprintln!("background blur 0.5 (ready after {took:?}): face differs by {face_diff:.2} levels; background energy {m_in:.2} -> {m_out:.2}");
    assert!(face_diff < 4.0, "the person stays sharp: {face_diff}");
    assert!(m_out < m_in * 0.6, "the background gets softer: {m_in} -> {m_out}");
    assert_eq!(px(&buf, w, h, FACE)[3], 255, "alpha is not touched");
}

// ---- size changes and teardown -------------------------------------------------------------------------------------

#[test]
fn nvidia_a_size_change_mid_stream_rebuilds_without_leaks() {
    let _g = exclusive();
    let name = "nvidia_a_size_change_mid_stream_rebuilds_without_leaks";
    let Some((r, _rt)) = hardware(name) else { return };
    let Some(pic) = Picture::load(name) else { return };
    let mut rig = Rig::new(&r, Kind::Removal, json!({ "mode": 1 }));
    let mut slowest = Duration::ZERO;
    for (w, h) in [(1280, 720), (640, 360), (1920, 1080), (1280, 720)] {
        let scaled = pic.scaled(w, h);
        let input = scaled.upload(&r);
        let (passed, took, slow) = rig.wait_ready(&input, Duration::from_secs(60));
        slowest = slowest.max(slow);
        let mut out = rig.frame(&input).0;
        for _ in 0..5 {
            out = rig.frame(&input).0;
        }
        assert_eq!((out.width, out.height), (w, h));
        let buf = rig.read(&out);
        let (face, background) = (px(&buf, w, h, FACE)[3], px(&buf, w, h, BACKGROUND[0])[3]);
        eprintln!("{w}x{h}: ready after {took:?} ({passed} frames passed through); alpha face {face}, background {background}; pipelines alive {}", live_pipelines());
        assert!(face >= 240 && background < 13, "{w}x{h}: face {face}, background {background}");
        assert!(rig.f.failure().is_none(), "{:?}", rig.f.failure());
        // The pipeline of the old size is released by the loader thread; exactly one stays.
        wait_for_pipelines(1);
    }
    eprintln!("slowest render() while loading: {slowest:?}");
    assert!(slowest < Duration::from_millis(250), "a frame stalled for {slowest:?}");
    drop(rig);
    wait_for_pipelines(0);
}

#[test]
fn nvidia_the_extremes_a_tiny_blur_and_4k() {
    let _g = exclusive();
    let name = "nvidia_the_extremes_a_tiny_blur_and_4k";
    let Some((r, _rt)) = hardware(name) else { return };
    let Some(pic) = Picture::load(name) else { return };
    // Blur has no minimum size (OBS checks only the two segmenting filters).
    let small = pic.scaled(64, 48).upload(&r);
    let mut blur = Rig::new(&r, Kind::Blur, json!({ "intensity": 0.5 }));
    let (_, took, _) = blur.wait_ready(&small, Duration::from_secs(30));
    blur.frame(&small);
    eprintln!("blur at 64x48 is ready after {took:?} and runs");
    drop(blur);
    // 4K: the three filters build and run; print what a frame costs.
    let big = pic.scaled(3840, 2160);
    let input = big.upload(&r);
    for (label, kind) in [("Removal", Kind::Removal), ("Background Blur", Kind::BackgroundBlur), ("Blur", Kind::Blur)] {
        let mut rig = Rig::new(&r, kind, json!({ "mode": 1 }));
        let (_, took, slowest) = rig.wait_ready(&input, Duration::from_secs(60));
        for _ in 0..4 {
            rig.frame(&input);
        }
        let t = Instant::now();
        for _ in 0..20 {
            rig.frame(&input);
        }
        let out = rig.frame(&input).0;
        let buf = rig.read(&out);
        eprintln!("{label} at 3840x2160: ready after {took:?} (slowest frame meanwhile {slowest:?}); {:.2} ms per frame; face alpha {}", t.elapsed().as_secs_f64() * 1000.0 / 20.0, px(&buf, 3840, 2160, FACE)[3]);
        assert!(rig.f.failure().is_none(), "{:?}", rig.f.failure());
    }
}

#[test]
fn nvidia_dropping_the_filter_releases_everything() {
    let _g = exclusive();
    let name = "nvidia_dropping_the_filter_releases_everything";
    let Some((r, _rt)) = hardware(name) else { return };
    let Some(pic) = Picture::load(name) else { return };
    let input = pic.scaled(640, 360).upload(&r);
    let before_all = gpu_memory_mib();
    let mut after_first = 0;
    let started = Instant::now();
    for n in 0..20 {
        let kind = [Kind::Removal, Kind::BackgroundBlur, Kind::Blur][n % 3];
        let mut rig = Rig::new(&r, kind, json!({ "mode": 1 }));
        rig.wait_ready(&input, Duration::from_secs(60));
        for _ in 0..3 {
            rig.frame(&input);
        }
        assert_eq!(live_pipelines(), 1);
        drop(rig);
        wait_for_pipelines(0);
        if n == 0 {
            after_first = gpu_memory_mib();
        }
    }
    // Give the driver a moment to hand memory back.
    std::thread::sleep(Duration::from_millis(500));
    let after = gpu_memory_mib();
    eprintln!("20 create/destroy cycles in {:?}; GPU memory used (whole GPU, the game included): before {before_all} MiB, after the first cycle {after_first} MiB, after 20 cycles {after} MiB", started.elapsed());
    assert!(after <= after_first + 64, "memory grew from {after_first} to {after} MiB over 19 more cycles");
}

// ---- giving the memory back ----------------------------------------------------------------------------------------

/// What this process uses of the GPU's own memory now, in MiB: DXGI's `QueryVideoMemoryInfo(CurrentUsage)` of the
/// adapter the renderer runs on (CUDA and TensorRT allocations of the process count, as the video memory manager owns
/// them).
fn process_gpu_mib(r: &Renderer) -> f64 {
    use windows::core::Interface;
    use windows::Win32::Graphics::Dxgi::{IDXGIAdapter3, IDXGIDevice, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, DXGI_QUERY_VIDEO_MEMORY_INFO};
    // SAFETY: plain COM calls on the live device; the out structure lives through the call.
    unsafe {
        let adapter: IDXGIAdapter3 = r.gpu.device.cast::<IDXGIDevice>().unwrap().GetAdapter().unwrap().cast().unwrap();
        let mut info = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
        adapter.QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut info).unwrap();
        info.CurrentUsage as f64 / (1024.0 * 1024.0)
    }
}

/// A 1280x720 picture with a lot of fine detail, so that a blur is easy to see (it has no person: the memory does not
/// care).
fn detailed(r: &Renderer) -> Pooled {
    let t = r.texture(1280, 720).expect("texture");
    fill_rgba(r, &t, |x, y| {
        let h = x.wrapping_mul(2654435761).wrapping_add(y.wrapping_mul(40503)).wrapping_mul(2246822519);
        let v = (h >> 24) as u8;
        [v, v.wrapping_mul(3), 255 - v, 255]
    });
    t
}

/// The whole path of a filter that is switched off: bypassed in the real chain on the real clock for 30 s (the real
/// `IDLE_FREE_AFTER`), the GPU memory of this process before and after, the rebuild when it is switched on again, and
/// the same for a filter that is removed.
#[test]
fn nvidia_a_filter_switched_off_for_30_s_gives_its_gpu_memory_back_and_rebuilds_on_return() {
    use super::filter::IDLE_FREE_AFTER;
    let _g = exclusive();
    let name = "nvidia_a_filter_switched_off_for_30_s_gives_its_gpu_memory_back_and_rebuilds_on_return";
    let Some((r, rt)) = hardware(name) else { return };
    // The test's own copy of the runtime would keep it loaded: the filter alone must be what holds it.
    drop(rt);
    let kind = "nv_background_blur_filter";
    let params = json!({ "intensity": 0.5 });
    let on = [cfg("b", kind, params.clone())];
    let off = [FilterCfg { enabled: false, ..cfg("b", kind, params.clone()) }];
    let flat = (0.0, 0.0, 1.0, 1.0);
    let src = detailed(&r);
    let src_energy = energy(&read_rgba(&r, &src), 1280, 720, flat);
    // Fresh input every frame, as the engine's chain gets it.
    let frame = |chain: &mut Chain| {
        let started = Instant::now();
        let out = chain.run(&r, detailed(&r)).expect("run");
        (out, started.elapsed())
    };
    let filtered = |out: &Pooled| energy(&read_rgba(&r, out), 1280, 720, flat) < src_energy * 0.8;
    let mib = || process_gpu_mib(&r);

    let baseline = mib();
    let mut chain = Chain::new();
    chain.apply(&r, &on);
    let (mut lines, started, mut passed, mut slowest) = (Vec::new(), Instant::now(), 0, Duration::ZERO);
    loop {
        assert!(started.elapsed() < Duration::from_secs(120), "never finished loading: {lines:?}");
        let (out, took) = frame(&mut chain);
        slowest = slowest.max(took);
        lines.extend(chain.take_status().into_iter().map(|s| s.message));
        if lines.iter().any(|m| m.is_empty()) && filtered(&out) {
            break;
        }
        passed += 1;
        std::thread::sleep(Duration::from_millis(4));
    }
    let first_ready = started.elapsed();
    for _ in 0..20 {
        frame(&mut chain);
    }
    std::thread::sleep(Duration::from_millis(300));
    let running = mib();
    eprintln!("first enable: ready after {first_ready:?} ({passed} frames passed through, slowest chain.run {slowest:?}); process GPU memory: before {baseline:.0} MiB, running {running:.0} MiB (pipeline ~{:.0} MiB)", running - baseline);
    assert!(running - baseline > 100.0, "the pipeline should be visible in the process's GPU memory: {baseline} -> {running}");

    // Off for 5 s (well under the time): the pipeline is kept, and switching on again filters at once, no loading line.
    chain.apply(&r, &off);
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(5) {
        let (out, _) = frame(&mut chain);
        assert!(!filtered(&out), "a bypassed filter passes the picture through");
        std::thread::sleep(Duration::from_millis(33));
    }
    assert_eq!(live_pipelines(), 1, "5 s off keeps the pipeline");
    chain.apply(&r, &on);
    let (out, took) = frame(&mut chain);
    assert!(filtered(&out), "switched on within the time: filtered at once");
    assert!(chain.take_status().is_empty(), "no loading line when the pipeline was kept");
    eprintln!("off 5 s, on again: first frame already filtered, chain.run {took:?}, pipelines alive {}", live_pipelines());

    // Off for the real 30 s: frames at the camera's pace, then every millisecond near the end to time the teardown.
    chain.apply(&r, &off);
    let bypass_started = Instant::now();
    let mut slowest_off = Duration::ZERO;
    let mut zero_at = None;
    let limit = IDLE_FREE_AFTER + Duration::from_secs(20);
    while zero_at.is_none() {
        let elapsed = bypass_started.elapsed();
        assert!(elapsed < limit, "the pipeline was not freed {elapsed:?} after the switch off");
        let (out, took) = frame(&mut chain);
        slowest_off = slowest_off.max(took);
        assert!(!filtered(&out) || elapsed < Duration::from_millis(100), "bypassed: pass through");
        if live_pipelines() == 0 {
            zero_at = Some(Instant::now());
        }
        std::thread::sleep(Duration::from_millis(if elapsed + Duration::from_millis(200) < IDLE_FREE_AFTER { 33 } else { 1 }));
        if elapsed + Duration::from_millis(100) < IDLE_FREE_AFTER {
            assert_eq!(live_pipelines(), 1, "freed too early, {elapsed:?} after the switch off");
        }
    }
    let freed_after = zero_at.unwrap().duration_since(bypass_started);
    let mut memory = mib();
    for _ in 0..40 {
        std::thread::sleep(Duration::from_millis(50));
        memory = mib();
    }
    eprintln!(
        "bypassed: pipeline gone {freed_after:?} after the switch off (limit {IDLE_FREE_AFTER:?}, so teardown on the loader thread <= ~{:?}); slowest chain.run during the 30 s {slowest_off:?}; process GPU memory {running:.0} -> {memory:.0} MiB (freed {:.0} MiB)",
        freed_after.saturating_sub(IDLE_FREE_AFTER),
        running - memory
    );
    assert!(freed_after >= IDLE_FREE_AFTER, "freed before the time: {freed_after:?}");
    assert!(running - memory > (running - baseline) * 0.8, "most of the pipeline's memory is given back: {running} -> {memory} (baseline {baseline})");
    assert!(slowest_off < Duration::from_millis(40), "a frame stalled for {slowest_off:?} while the filter was off");

    // Switched on again: the loading line, the picture passes through meanwhile, then it is filtered again.
    chain.apply(&r, &on);
    let (started, mut lines, mut passed, mut slowest) = (Instant::now(), Vec::new(), 0, Duration::ZERO);
    let first = frame(&mut chain);
    assert!(!filtered(&first.0), "the first frame after the free passes through");
    slowest = slowest.max(first.1);
    lines.extend(chain.take_status().into_iter().map(|s| s.message));
    loop {
        assert!(started.elapsed() < Duration::from_secs(120), "never rebuilt: {lines:?}");
        let (out, took) = frame(&mut chain);
        slowest = slowest.max(took);
        lines.extend(chain.take_status().into_iter().map(|s| s.message));
        if lines.iter().any(|m| m.is_empty()) && filtered(&out) {
            break;
        }
        passed += 1;
        std::thread::sleep(Duration::from_millis(4));
    }
    let rebuilt = started.elapsed();
    for _ in 0..20 {
        frame(&mut chain);
    }
    std::thread::sleep(Duration::from_millis(300));
    let again = mib();
    eprintln!("switched on after the free: card lines {lines:?}; filtered again after {rebuilt:?} ({passed} frames passed through, slowest chain.run {slowest:?}); process GPU memory {memory:.0} -> {again:.0} MiB; pipelines alive {}", live_pipelines());
    assert_eq!(lines.first().map(String::as_str), Some("Loading model…"), "{lines:?}");
    assert_eq!(live_pipelines(), 1);
    assert!(slowest < Duration::from_millis(250), "a frame stalled for {slowest:?} during the rebuild");
    assert!(again - memory > (running - baseline) * 0.7, "the pipeline is back: {memory} -> {again}");

    // Removed from the chain: freed at once (the filter's Drop hands it to the loader thread).
    let removed = Instant::now();
    chain.apply(&r, &[]);
    let apply_took = removed.elapsed();
    wait_for_pipelines(0);
    let gone = removed.elapsed();
    let mut after = mib();
    for _ in 0..20 {
        std::thread::sleep(Duration::from_millis(50));
        after = mib();
    }
    eprintln!("removed from the chain: chain.apply took {apply_took:?}, pipeline gone after {gone:?}; process GPU memory {again:.0} -> {after:.0} MiB");
    assert!(again - after > (running - baseline) * 0.8, "removing frees the memory too: {again} -> {after}");
    assert!(apply_took < Duration::from_millis(40), "removing a filter stalled the chain for {apply_took:?}");
}

/// The runtime (CUDA, TensorRT) unloads when the last pipeline goes and loads again for the next one: eight times over,
/// every kind, with the memory of the process back near its start each time. Needs no picture of a person.
#[test]
fn nvidia_the_runtime_unloads_with_the_last_pipeline_and_loads_again_eight_times() {
    let _g = exclusive();
    let name = "nvidia_the_runtime_unloads_with_the_last_pipeline_and_loads_again_eight_times";
    let Some((r, rt)) = hardware(name) else { return };
    // The test's own copy would keep the runtime loaded.
    drop(rt);
    let flat = (0.0, 0.0, 1.0, 1.0);
    let src = detailed(&r);
    let src_energy = energy(&read_rgba(&r, &src), 1280, 720, flat);
    let mib = || process_gpu_mib(&r);
    let base = mib();
    let (mut first_peak, mut worst_after) = (0.0f64, 0.0f64);
    for n in 0..8 {
        let kind = [Kind::BackgroundBlur, Kind::Removal, Kind::Blur][n % 3];
        let mut rig = Rig::new(&r, kind, json!({ "mode": 1, "intensity": 0.5 }));
        let started = Instant::now();
        rig.wait_ready(&src, Duration::from_secs(60));
        let ready = started.elapsed();
        let mut out = rig.frame(&src).0;
        for _ in 0..5 {
            out = rig.frame(&src).0;
        }
        if kind.blurs() {
            assert!(energy(&read_rgba(&r, &out), 1280, 720, flat) < src_energy * 0.8, "cycle {n}: the blur works after the reload");
        }
        std::thread::sleep(Duration::from_millis(200));
        let peak = mib();
        let dropped = Instant::now();
        drop(rig);
        wait_for_pipelines(0);
        let pipeline_down = dropped.elapsed();
        // The runtime unloads on the loader thread right after: poll until the memory is down.
        while mib() > base + 30.0 {
            assert!(dropped.elapsed() < Duration::from_secs(10), "cycle {n}: the memory never came back");
            std::thread::sleep(Duration::from_millis(2));
        }
        let teardown = dropped.elapsed();
        std::thread::sleep(Duration::from_millis(1500));
        let after = mib();
        eprintln!("cycle {n} {kind:?}: ready after {ready:?}; {base:.0} -> {peak:.0} MiB running -> {after:.0} MiB after the last pipeline went; pipeline down after {pipeline_down:?}, memory back after {teardown:?} (loader thread)");
        if n == 0 {
            first_peak = peak;
        }
        worst_after = worst_after.max(after);
        assert!(after - base < (first_peak - base) * 0.2 + 20.0, "cycle {n}: {after} MiB left over (base {base}, first peak {first_peak})");
    }
    eprintln!("worst memory after a release over 8 cycles: {worst_after:.0} MiB (base {base:.0})");
}

// ---- timing --------------------------------------------------------------------------------------------------------

#[test]
fn nvidia_per_frame_time_of_every_filter_and_mode() {
    let _g = exclusive();
    let name = "nvidia_per_frame_time_of_every_filter_and_mode";
    let Some((r, _rt)) = hardware(name) else { return };
    let Some(pic) = Picture::load(name) else { return };
    let input = pic.upload(&r);
    let mut timer = GpuTimer::new(&r.gpu.device).expect("timer");
    let cases = [
        ("Removal mode 0 (Quality)", Kind::Removal, json!({ "mode": 0 })),
        ("Removal mode 1 (Performance)", Kind::Removal, json!({ "mode": 1 })),
        ("Removal mode 2 (Quality + chair)", Kind::Removal, json!({ "mode": 2 })),
        ("Removal mode 3 (Performance + chair)", Kind::Removal, json!({ "mode": 3 })),
        ("Removal mode 0, interval 4", Kind::Removal, json!({ "mode": 0, "processing_interval": 4 })),
        ("Blur 0.5", Kind::Blur, json!({ "intensity": 0.5 })),
        ("Background Blur 0.5", Kind::BackgroundBlur, json!({ "intensity": 0.5 })),
    ];
    for (label, kind, params) in cases {
        let mut rig = Rig::new(&r, kind, params);
        let (_, took, _) = rig.wait_ready(&input, Duration::from_secs(120));
        for _ in 0..8 {
            rig.frame(&input);
        }
        let (mut gpu_ms, mut cpu_ms) = (Vec::new(), Vec::new());
        for _ in 0..60 {
            let begun = timer.begin(&r.gpu.context);
            let t = Instant::now();
            let (out, _) = rig.frame(&input);
            cpu_ms.push(t.elapsed().as_secs_f64() * 1000.0);
            if begun {
                timer.end(&r.gpu.context);
                r.flush();
            }
            drop(out);
            if let Some(ms) = timer.poll(&r.gpu.context) {
                gpu_ms.push(ms);
            }
        }
        let mean = |v: &[f64]| if v.is_empty() { f64::NAN } else { v.iter().sum::<f64>() / v.len() as f64 };
        eprintln!(
            "{label}: GPU time per frame {:.2} ms (n={}), render() on the CPU {:.2} ms; ready {took:?}",
            mean(&gpu_ms),
            gpu_ms.len(),
            mean(&cpu_ms)
        );
    }
}

// ---- the whole engine, with the real camera, without a window -------------------------------------------------------

/// What the live test measures of a frame: the share of pixels near the output background colour and the picture's
/// high-frequency energy (mean absolute luma difference of horizontal neighbours two pixels apart).
#[derive(Clone, Copy, Debug)]
struct Look {
    bg_share: f32,
    detail: f32,
}

fn look(buf: &[u8], w: u32, h: u32, bg: [u8; 3]) -> Look {
    let (mut near, mut n, mut sum, mut pairs) = (0u32, 0u32, 0.0f64, 0u32);
    for y in (0..h).step_by(2) {
        let mut prev = -1.0f32;
        for x in (0..w).step_by(2) {
            let i = ((y * w + x) * 4) as usize;
            let p = &buf[i..i + 3];
            if p.iter().zip(bg).all(|(&a, b)| (a as i32 - b as i32).abs() < 40) {
                near += 1;
            }
            n += 1;
            let l = luma(p);
            if prev >= 0.0 {
                sum += (l - prev).abs() as f64;
                pairs += 1;
            }
            prev = l;
        }
    }
    Look { bg_share: near as f32 / n as f32, detail: (sum / pairs.max(1) as f64) as f32 }
}

/// The newest sampled frame: width, height, RGBA bytes.
type LastFrame = Arc<Mutex<Option<(u32, u32, Vec<u8>)>>>;
/// What the engine told its sink, with the time it did.
type Events = Arc<Mutex<Vec<(Instant, crate::engine::EngineEvent)>>>;

/// The engine's frame tap of the live test: every frame's arrival time, and a measured copy of every `every`th one
/// (0: none, so the GPU timer of the engine is not disturbed by read-backs).
struct LiveTap {
    arrivals: Arc<Mutex<Vec<Instant>>>,
    looks: Arc<Mutex<Vec<(Instant, Look)>>>,
    last: LastFrame,
    every: Arc<std::sync::atomic::AtomicU32>,
    count: u64,
    bg: [u8; 3],
}

impl crate::engine::FrameTap for LiveTap {
    fn on_frame(&mut self, r: &Renderer, frame: &crate::gpu::RgbaTexture, _capture_qpc: u64) {
        self.count += 1;
        self.arrivals.lock().unwrap().push(Instant::now());
        let every = self.every.load(std::sync::atomic::Ordering::Relaxed) as u64;
        if every > 0 && self.count % every == 0 {
            let buf = read_rgba(r, frame);
            self.looks.lock().unwrap().push((Instant::now(), look(&buf, frame.width, frame.height, self.bg)));
            *self.last.lock().unwrap() = Some((frame.width, frame.height, buf));
        }
    }
}

/// The real camera through the real engine (the same chain, filter threads and statuses as the app, minus the window):
/// Background Removal, the three filters' effects on the picture, the longest gap between frames while the model
/// loads, and the engine's own GPU time with the mask refresh at 1 and at 4. Needs `BETTER_CAM_TEST_LIVE`, the path of
/// a saved app state (`localStorage` `better-cam`, for the camera and its format), because it opens the camera.
/// `BETTER_CAM_TEST_DUMP` is a folder for the frames of the report.
#[test]
fn nvidia_the_engine_live_with_the_real_camera() {
    use crate::config::{OutputCfg, SourceCfg};
    use crate::engine::{Engine, EngineEvent, EngineState, Msg};
    let _g = exclusive();
    let name = "nvidia_the_engine_live_with_the_real_camera";
    let Some(state_path) = std::env::var_os("BETTER_CAM_TEST_LIVE") else {
        eprintln!("skipped {name}: BETTER_CAM_TEST_LIVE is not set (it opens the camera)");
        return;
    };
    let Some((r, _rt)) = hardware(name) else { return };
    let state: serde_json::Value = serde_json::from_slice(&std::fs::read(&state_path).expect("read the state file")).expect("state json");
    let mut source: SourceCfg = serde_json::from_value(state["source"].clone()).expect("source");
    source.deactivate_when_not_showing = false;
    let mut output: OutputCfg = serde_json::from_value(state["output"].clone()).expect("output");
    output.background_color = 0xFF00_FF00; // 0xAABBGGRR: pure green
    let bg = [0u8, 255, 0];

    let path = std::env::temp_dir().join(format!("bc-nv-live-{}.bin", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let writer = Arc::new(vcam_protocol::Writer::create(&path).expect("writer"));
    let engine = Engine::with_parts(r.gpu.clone(), writer).expect("engine");
    let events: Events = Arc::default();
    {
        let events = events.clone();
        engine.set_event_sink(Arc::new(move |e: EngineEvent| {
            if !matches!(e, EngineEvent::Stats(_) | EngineEvent::Controls(_)) {
                events.lock().unwrap().push((Instant::now(), e));
            }
        }));
    }
    let arrivals: Arc<Mutex<Vec<Instant>>> = Arc::default();
    let looks: Arc<Mutex<Vec<(Instant, Look)>>> = Arc::default();
    let last: LastFrame = Arc::default();
    let every = Arc::new(std::sync::atomic::AtomicU32::new(0));
    engine.send(Msg::SetTap(Some(Box::new(LiveTap { arrivals: arrivals.clone(), looks: looks.clone(), last: last.clone(), every: every.clone(), count: 0, bg }))));
    engine.set_window_visible(true);
    engine.start(crate::api::settings_from(&source, &output).expect("settings"));
    engine.wait_live(Duration::from_secs(40)).expect("the engine is live");

    let dump = std::env::var_os("BETTER_CAM_TEST_DUMP").map(std::path::PathBuf::from);
    let save = |label: &str| {
        if let (Some(dir), Some((w, h, buf))) = (&dump, last.lock().unwrap().clone()) {
            let _ = std::fs::write(dir.join(format!("{label}_{w}x{h}.rgba")), buf);
        }
    };
    let chain = |filters: &[(&str, &str, bool, serde_json::Value)]| {
        engine.set_chain(filters.iter().map(|(id, kind, on, p)| FilterCfg { id: (*id).into(), kind: (*kind).into(), enabled: *on, params: p.as_object().cloned().unwrap_or_default() }).collect());
    };
    // Gaps between frame arrivals since `since`: (frames, fps, longest gap).
    let gaps = |since: Instant| {
        let a: Vec<Instant> = arrivals.lock().unwrap().iter().copied().filter(|t| *t >= since).collect();
        let longest = a.windows(2).map(|w| (w[1] - w[0]).as_secs_f64() * 1000.0).fold(0.0, f64::max);
        let span = a.last().zip(a.first()).map_or(1.0, |(l, f)| (*l - *f).as_secs_f64().max(0.001));
        (a.len(), a.len() as f64 / span, longest)
    };
    // Mean Look over `window` of sampled frames.
    let looked = |since: Instant| {
        let l: Vec<Look> = looks.lock().unwrap().iter().filter(|(t, _)| *t >= since).map(|(_, l)| *l).collect();
        let n = l.len().max(1) as f32;
        (l.len(), l.iter().map(|x| x.bg_share).sum::<f32>() / n, l.iter().map(|x| x.detail).sum::<f32>() / n)
    };
    let settle = |ms: u64| std::thread::sleep(Duration::from_millis(ms));
    let notes = |since: Instant| -> Vec<(u128, String)> {
        events
            .lock()
            .unwrap()
            .iter()
            .filter(|(t, _)| *t >= since)
            .filter_map(|(t, e)| match e {
                EngineEvent::FilterStatus(s) => Some((t.duration_since(since).as_millis(), format!("status {:?}", s.message))),
                EngineEvent::FilterError(s) => Some((t.duration_since(since).as_millis(), format!("error {:?}", s.message))),
                _ => None,
            })
            .collect()
    };
    let wait_cleared = |since: Instant, limit: Duration| {
        let t = Instant::now();
        loop {
            let n = notes(since);
            if n.iter().any(|(_, m)| m.starts_with("status \"Load")) && n.iter().any(|(_, m)| m == "status \"\"") {
                return t.elapsed();
            }
            assert!(t.elapsed() < limit, "the model never finished loading: {n:?}");
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    let gpu_ms = |label: &str| {
        every.store(0, std::sync::atomic::Ordering::Relaxed);
        settle(2500);
        let mut xs = Vec::new();
        for _ in 0..6 {
            settle(1000);
            let s = engine.stats();
            xs.push((s.gpu_ms, s.render_fps));
        }
        let gpu = xs.iter().map(|x| x.0).sum::<f64>() / xs.len() as f64;
        let fps = xs.iter().map(|x| x.1).sum::<f64>() / xs.len() as f64;
        eprintln!("{label}: engine GPU time {gpu:.2} ms (samples {:?}), {fps:.1} frames rendered per second", xs.iter().map(|x| (x.0 * 100.0).round() / 100.0).collect::<Vec<_>>());
        (gpu, fps)
    };
    every.store(4, std::sync::atomic::Ordering::Relaxed);

    // Baseline.
    chain(&[]);
    settle(2500);
    let t = Instant::now();
    settle(3000);
    eprintln!("camera: {}", engine.stats().source);
    let (n, fps, gap) = gaps(t);
    let (m, bg0, d0) = looked(t);
    eprintln!("baseline, no filter: {n} frames ({fps:.1}/s), longest gap {gap:.0} ms; {m} looked at: green {:.1} %, detail {d0:.2}", bg0 * 100.0);
    save("live-baseline");

    // 1. Background Removal.
    let t0 = Instant::now();
    chain(&[("r", "nv_greenscreen_filter", true, json!({}))]);
    let waited = wait_cleared(t0, Duration::from_secs(120));
    let (n, fps, gap) = gaps(t0);
    eprintln!("removal added: the card line cleared {waited:?} after the chain call; notes {:?}; frames since the call: {n} ({fps:.1}/s), longest gap {gap:.0} ms", notes(t0));
    settle(1500);
    let t = Instant::now();
    settle(4000);
    let (m, share, d) = looked(t);
    eprintln!("removal on: {m} frames looked at: green {:.1} % (filter off {:.1} %), detail {d:.2}", share * 100.0, bg0 * 100.0);
    save("live-removal");
    chain(&[("r", "nv_greenscreen_filter", false, json!({}))]);
    settle(1500);
    let t = Instant::now();
    settle(2500);
    let (_, share_off, _) = looked(t);
    eprintln!("removal bypassed: green {:.1} %", share_off * 100.0);
    chain(&[("r", "nv_greenscreen_filter", true, json!({}))]);
    settle(1000);

    // 2. Modes while live.
    for (mode, label) in [(1, "Performance"), (2, "Quality + chair removal"), (3, "Performance + chair removal"), (0, "Quality")] {
        let t0 = Instant::now();
        chain(&[("r", "nv_greenscreen_filter", true, json!({ "mode": mode }))]);
        settle(2500);
        let (n, fps, gap) = gaps(t0);
        let (_, share, _) = looked(t0 + Duration::from_millis(800));
        eprintln!("mode {mode} ({label}): {n} frames in 2.5 s ({fps:.1}/s), longest gap {gap:.0} ms, green {:.1} %; card notes {:?}", share * 100.0, notes(t0));
    }

    // 3. The mask refresh: the engine's GPU time.
    let (g1, f1) = gpu_ms("mask refresh 1");
    chain(&[("r", "nv_greenscreen_filter", true, json!({ "processing_interval": 4 }))]);
    let (g4, f4) = gpu_ms("mask refresh 4");
    chain(&[("r", "nv_greenscreen_filter", false, json!({}))]);
    let (g0, f0) = gpu_ms("filter bypassed");
    eprintln!("GPU time: bypassed {g0:.2} ms ({f0:.1} fps), refresh 1 {g1:.2} ms ({f1:.1} fps), refresh 4 {g4:.2} ms ({f4:.1} fps)");
    every.store(4, std::sync::atomic::Ordering::Relaxed);

    // 4. Background Blur, then Blur.
    for (kind, label, tag) in [("nv_background_blur_filter", "background blur 0.5", "live-bgblur"), ("nv_blur_filter", "blur 0.5", "live-blur")] {
        let t0 = Instant::now();
        chain(&[("b", kind, true, json!({ "intensity": 0.5 }))]);
        let waited = wait_cleared(t0, Duration::from_secs(120));
        settle(1500);
        let t = Instant::now();
        settle(3500);
        let (m, share, d_on) = looked(t);
        save(tag);
        chain(&[("b", kind, false, json!({ "intensity": 0.5 }))]);
        settle(1500);
        let t = Instant::now();
        settle(3000);
        let (_, _, d_off) = looked(t);
        eprintln!("{label} (ready {waited:?} after adding): {m} frames looked at; detail {d_off:.2} bypassed -> {d_on:.2} on ({:.0} % lower); green {:.1} %", (1.0 - d_on / d_off) * 100.0, share * 100.0);
        chain(&[("b", kind, true, json!({ "intensity": 1.0 }))]);
        settle(1500);
        let t = Instant::now();
        settle(2500);
        let (_, _, d_full) = looked(t);
        eprintln!("   intensity 1.0: detail {d_full:.2}");
    }
    chain(&[]);
    settle(1000);
    let errors: Vec<_> = events.lock().unwrap().iter().filter_map(|(_, e)| if let EngineEvent::FilterError(f) = e { Some(f.clone()) } else { None }).collect();
    eprintln!("filter-error events over the whole run: {errors:?}; engine state {:?}; engine errors {}", engine.state(), engine.stats().errors);
    assert!(errors.is_empty(), "{errors:?}");
    engine.stop();
    assert!(engine.wait_state(EngineState::Stopped, Duration::from_secs(10)));
    engine.shutdown();
    let _ = std::fs::remove_file(&path);
}

// ---- the chain: cards, statuses, a PC that cannot run it ------------------------------------------------------------

fn cfg(id: &str, kind: &str, params: serde_json::Value) -> FilterCfg {
    FilterCfg { id: id.into(), kind: kind.into(), enabled: true, params: params.as_object().cloned().unwrap_or_default() }
}

#[test]
fn nvidia_the_chain_reports_loading_then_clears_it() {
    let _g = exclusive();
    let name = "nvidia_the_chain_reports_loading_then_clears_it";
    let Some((r, _rt)) = hardware(name) else { return };
    let Some(pic) = Picture::load(name) else { return };
    let mut chain = Chain::new();
    chain.apply(&r, &[cfg("a", "nv_greenscreen_filter", json!({}))]);
    let (mut lines, mut errors, mut passthrough_frames) = (Vec::new(), Vec::new(), 0);
    let started = Instant::now();
    loop {
        assert!(started.elapsed() < Duration::from_secs(120), "never finished loading: {lines:?}");
        let out = chain.run(&r, pic.upload(&r)).expect("run");
        lines.extend(chain.take_status().into_iter().map(|s| (s.id, s.message)));
        errors.extend(chain.take_errors());
        if lines.iter().any(|(_, m)| m.is_empty()) {
            let alpha = px(&read_rgba(&r, &out), 1280, 720, BACKGROUND[0])[3];
            if alpha < 13 {
                break;
            }
        } else {
            passthrough_frames += 1;
        }
        std::thread::sleep(Duration::from_millis(4));
    }
    eprintln!("statuses {lines:?}, errors {errors:?}, {passthrough_frames} frames before the first cut-out");
    assert_eq!(lines.first(), Some(&("a".to_string(), "Loading model…".to_string())));
    assert_eq!(lines.get(1), Some(&("a".to_string(), String::new())), "the line is cleared when the model is ready");
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn nvidia_a_render_adapter_that_is_not_nvidia_passes_through_and_says_why() {
    let _g = exclusive();
    // WARP is Microsoft's software renderer: the effects must refuse it, whatever else this PC has.
    let r = warp();
    let mut f = NvFilter::new(Kind::Removal);
    f.update(&r, &Params::new());
    let input = r.texture(640, 360).unwrap();
    fill_rgba(&r, &input, |x, y| [(x % 251) as u8, (y % 241) as u8, 77, 255]);
    let mut notes: Vec<Note> = Vec::new();
    let started = Instant::now();
    loop {
        let out = r.texture(640, 360).unwrap();
        f.render(&r, &input, &out).unwrap();
        assert_eq!(read_rgba(&r, &out), read_rgba(&r, &input), "the picture passes through");
        f.take_notes(&mut notes);
        if notes.iter().any(|n| matches!(n, Note::Error(m) if !m.is_empty())) {
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(20), "no message: {notes:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
    eprintln!("notes on a software renderer: {notes:?}");
    assert_eq!(notes.first(), Some(&Note::Status("Loading model…".into())));
    let Some(Note::Error(message)) = notes.iter().find(|n| matches!(n, Note::Error(m) if !m.is_empty())) else { unreachable!() };
    // On this PC the runtime is installed, so the adapter is what is wrong; without it the runtime or GPU message comes first.
    assert!(message.contains("rendered on the NVIDIA GPU") || message.contains("NVIDIA") || message.contains("RTX"), "{message}");
    assert!(f.failure().is_some());
}

/// A failure stays on the card and is not retried while frames keep coming; a filter that was not asked to render for a
/// while (switched off, then on again) tries once more, with the "Loading model…" line and a cleared error.
#[test]
fn nvidia_a_failed_filter_tries_again_when_it_comes_back_from_being_off() {
    let _g = exclusive();
    let r = warp();
    let mut f = NvFilter::new(Kind::Removal);
    f.update(&r, &Params::new());
    let input = r.texture(640, 360).unwrap();
    fill_rgba(&r, &input, |x, y| [(x % 251) as u8, (y % 241) as u8, 77, 255]);
    let frame = |f: &mut NvFilter| {
        let out = r.texture(640, 360).unwrap();
        f.render(&r, &input, &out).unwrap();
    };
    let loading = |notes: &[Note]| notes.iter().filter(|n| **n == Note::Status("Loading model…".into())).count();
    let mut notes: Vec<Note> = Vec::new();
    let started = Instant::now();
    while f.failure().is_none() {
        assert!(started.elapsed() < Duration::from_secs(20), "no failure: {notes:?}");
        frame(&mut f);
        std::thread::sleep(Duration::from_millis(10));
        f.take_notes(&mut notes);
    }
    assert_eq!(loading(&notes), 1, "{notes:?}");

    // Frames at the normal pace: nothing happens again.
    notes.clear();
    for _ in 0..30 {
        frame(&mut f);
        std::thread::sleep(Duration::from_millis(10));
        f.take_notes(&mut notes);
    }
    assert!(notes.is_empty() && f.failure().is_some(), "a failure is not retried while the filter renders: {notes:?}");

    // Off for a moment, then on: the card clears the error, says it is loading, and builds again (and fails again here).
    std::thread::sleep(Duration::from_millis(400));
    frame(&mut f);
    f.take_notes(&mut notes);
    assert_eq!(notes.first(), Some(&Note::Status("Loading model…".into())), "{notes:?}");
    assert!(notes.contains(&Note::Error(String::new())), "the old error is cleared: {notes:?}");
    let started = Instant::now();
    while f.failure().is_none() {
        assert!(started.elapsed() < Duration::from_secs(20), "no second failure: {notes:?}");
        frame(&mut f);
        std::thread::sleep(Duration::from_millis(10));
        f.take_notes(&mut notes);
    }
    assert!(notes.iter().any(|n| matches!(n, Note::Error(m) if !m.is_empty())), "{notes:?}");
}
