//! The headless engine (plan 2.3, 2.4): camera (or a synthetic source) -> convert -> filter chain -> output stage ->
//! encode -> readback -> `frames.bin`.
//!
//! # Threads
//! * The caller owns an [`Engine`] handle and talks to it with [`Msg`]s over an `mpsc` channel (a later phase wires
//!   Tauri commands to [`Engine::send`]).
//! * The **control thread** owns the shader cache and starts and stops render threads.
//! * The **render thread** (one per `Start`) opens the source, owns the D3D11 immediate context while it runs, and
//!   loops: take the newest frame (older ones are dropped by the capture queue and by `drain`), convert, run the
//!   filter chain, fit to the output size, encode, read back, publish.
//!
//! # States
//! This phase has `Stopped` and `Live` only. While the camera opens the file's writer state is `Starting` (the DLL
//! shows its "starting" slate); the first published frame makes it `Live`; stopping leaves it `Closed`.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use vcam_protocol::{advertised_for_output, qpc_to_hns, Writer, WriterState};
use windows::Win32::Media::MediaFoundation::{MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_YUV_MATRIX};

use crate::capture::reader::{CaptureReader, Frame, FrameData, OpenOptions, RecvError};
use crate::capture::{ensure_com, qpc_now_100ns, FormatRequest, PixelFormat, FIRST_VIDEO_STREAM};
use crate::gpu::convert::{
    is_supported, ColorRange, ColorSettings, ColorSpace, Converter, SourceData, SourceFrame,
};
use crate::gpu::output::{FitMode, OutputParams, OutputStage};
use crate::gpu::pattern;
use crate::gpu::{AdapterKind, FilterChain, GpuDevice, GpuError, GpuTimer, Renderer, RgbaTexture};
use crate::output_writer::{FrameTag, OutputWriter, Published};

// ---------------------------------------------------------------------------------------------------------------
// Public types

/// Engine state (plan 2.4). Standby, Reconnecting and Pause come with later phases.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EngineState {
    #[default]
    Stopped,
    Live,
}

/// A physical camera.
#[derive(Clone, Debug)]
pub struct CameraSpec {
    /// Symbolic link (`DeviceKey::symbolic_link`).
    pub link: String,
    pub request: FormatRequest,
    /// Hardware decoders and decoder textures (the reader gets the D3D manager); off means CPU buffers.
    pub hw_decode: bool,
}

/// Generated SMPTE colour bars in a camera format, delivered at a fixed rate through the normal pipeline.
#[derive(Clone, Copy, Debug)]
pub struct SyntheticSpec {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// NV12, YUY2, I420, YV12, P010 or XRGB.
    pub format: PixelFormat,
    pub color: ColorSettings,
}

#[derive(Clone, Debug)]
pub enum SourceSpec {
    Camera(CameraSpec),
    Synthetic(SyntheticSpec),
}

/// Plan 6.3 output settings. `None` size or rate means "same as the camera".
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OutputSettings {
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<u32>,
    pub fit: FitMode,
    pub background: [u8; 3],
}

impl Default for OutputSettings {
    fn default() -> Self {
        OutputSettings {
            width: None,
            height: None,
            fps: None,
            fit: FitMode::Fit,
            background: [0, 0, 0],
        }
    }
}

/// When the render thread maps a finished frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ReadbackPolicy {
    /// Right after the copy: the thread has nothing else to do until the next camera frame, so waiting for the GPU
    /// costs no throughput and saves a frame of latency (66 ms at 15 fps). The GPU is never stalled.
    #[default]
    Immediate,
    /// Plan 4.2 literally: map frame N while frame N+1 renders (never blocks unless the ring is nearly full).
    NextFrame,
}

/// Everything `Start` needs.
#[derive(Clone, Debug)]
pub struct Settings {
    pub source: SourceSpec,
    pub output: OutputSettings,
    /// Mirror vertically in the convert pass (plan 6.1 `flip_vertically`).
    pub flip_vertical: bool,
    /// Overrides for the colour space and range the camera states.
    pub color: ColorSettings,
    pub readback: ReadbackPolicy,
}

/// Called on the render thread with the finished output frame (RGBA, output size) after the output stage. The
/// preview (a later phase) copies and downsizes it there; it must not keep the texture past the call.
pub trait FrameTap: Send {
    fn on_frame(&mut self, renderer: &Renderer, frame: &RgbaTexture, capture_qpc: u64);
}

/// Messages to the engine.
pub enum Msg {
    /// Open the source and start rendering. A running session is stopped first.
    Start(Box<Settings>),
    /// Stop rendering, release the camera, leave the file `Closed`.
    Stop,
    /// Change the output settings; takes effect on the next frame (a new size or rate is advertised again).
    SetOutput(OutputSettings),
    SetFlip(bool),
    SetTap(Option<Box<dyn FrameTap>>),
    /// Stop and end the control thread.
    Shutdown,
}

/// Numbers for the UI footer and the headless example. Times in milliseconds.
#[derive(Clone, Debug, Default)]
pub struct EngineStats {
    pub state: EngineState,
    /// What is open, e.g. `W4GS mjpeg 1920x1080@60 hw decode, texture frames`.
    pub source: String,
    pub capture_fps: f64,
    pub render_fps: f64,
    pub publish_fps: f64,
    /// GPU time of one frame (convert to readback copy), mean of the last second; 0 when the timer is unavailable.
    pub gpu_ms: f64,
    /// Map + copy into the slot, mean of the last second.
    pub readback_ms: f64,
    /// Encode and submit on the CPU side, mean of the last second.
    pub cpu_ms: f64,
    pub captured: u64,
    pub rendered: u64,
    pub published: u64,
    /// Frames the capture queue replaced before they were rendered.
    pub dropped_mailbox: u64,
    /// Frames dropped between the GPU and the file (ring full, publish refused).
    pub dropped_output: u64,
    pub errors: u64,
    /// Capture timestamp to published, last ~240 frames.
    pub latency_p50_ms: f64,
    pub latency_p95_ms: f64,
    pub latency_max_ms: f64,
    /// Arrival in the capture callback to published (the part this process controls).
    pub pipeline_p50_ms: f64,
    pub pipeline_p95_ms: f64,
    /// The same percentiles over the whole run.
    pub total_latency_p95_ms: f64,
    pub total_pipeline_p95_ms: f64,
    pub output_width: u32,
    pub output_height: u32,
    pub output_format: &'static str,
    pub consumer: bool,
}

// ---------------------------------------------------------------------------------------------------------------
// Handle

struct Shared {
    state: Mutex<EngineState>,
    cv: Condvar,
    stats: Mutex<EngineStats>,
    error: Mutex<Option<String>>,
    output: Mutex<OutputSettings>,
    flip: AtomicBool,
    tap: Mutex<Option<Box<dyn FrameTap>>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Handle to the engine threads. Dropping it stops everything.
pub struct Engine {
    tx: Sender<Msg>,
    shared: Arc<Shared>,
    writer: Arc<Writer>,
    control: Option<JoinHandle<()>>,
}

impl Engine {
    /// The real thing: the default hardware adapter and `%ProgramData%\BetterCam\frames.bin`.
    pub fn new() -> Result<Engine, String> {
        let gpu =
            GpuDevice::new(AdapterKind::Hardware).map_err(|e| format!("D3D11 device: {e}"))?;
        let writer = Writer::create_default().map_err(|e| format!("frames.bin: {e}"))?;
        Engine::with_parts(Arc::new(gpu), Arc::new(writer))
    }

    /// Engine on a given device and writer (tests use WARP and a temporary file).
    pub fn with_parts(gpu: Arc<GpuDevice>, writer: Arc<Writer>) -> Result<Engine, String> {
        let renderer = Arc::new(Renderer::new(gpu).map_err(|e| e.to_string())?);
        renderer.shaders.warm_up().map_err(|e| e.to_string())?;
        let shared = Arc::new(Shared {
            state: Mutex::new(EngineState::Stopped),
            cv: Condvar::new(),
            stats: Mutex::new(EngineStats::default()),
            error: Mutex::new(None),
            output: Mutex::new(OutputSettings::default()),
            flip: AtomicBool::new(false),
            tap: Mutex::new(None),
        });
        let (tx, rx) = mpsc::channel();
        let control = {
            let (shared, writer) = (shared.clone(), writer.clone());
            std::thread::Builder::new()
                .name("engine control".into())
                .spawn(move || control_loop(rx, shared, renderer, writer))
                .map_err(|e| e.to_string())?
        };
        Ok(Engine {
            tx,
            shared,
            writer,
            control: Some(control),
        })
    }

    /// Queue a message.
    pub fn send(&self, msg: Msg) {
        let _ = self.tx.send(msg);
    }

    /// `Msg::Start`, after clearing the previous error so [`Engine::wait_live`] sees only this start's outcome.
    pub fn start(&self, settings: Settings) {
        *lock(&self.shared.error) = None;
        self.send(Msg::Start(Box::new(settings)));
    }

    pub fn stop(&self) {
        self.send(Msg::Stop);
    }

    pub fn state(&self) -> EngineState {
        *lock(&self.shared.state)
    }

    pub fn stats(&self) -> EngineStats {
        lock(&self.shared.stats).clone()
    }

    /// The reason the last session ended or failed to start.
    pub fn last_error(&self) -> Option<String> {
        lock(&self.shared.error).clone()
    }

    pub fn writer(&self) -> &Arc<Writer> {
        &self.writer
    }

    /// Block until the state is `Live` (`Ok`), a start fails or the session ends (`Err` with the reason), or the
    /// timeout passes.
    pub fn wait_live(&self, timeout: Duration) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        let mut st = lock(&self.shared.state);
        loop {
            if *st == EngineState::Live {
                return Ok(());
            }
            if let Some(e) = lock(&self.shared.error).clone() {
                return Err(e);
            }
            let now = Instant::now();
            if now >= deadline {
                return Err("timed out waiting for the engine to go live".into());
            }
            st = self
                .shared
                .cv
                .wait_timeout(st, (deadline - now).min(Duration::from_millis(50)))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// Block until the state is `Stopped` or the timeout passes.
    pub fn wait_stopped(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut st = lock(&self.shared.state);
        while *st != EngineState::Stopped {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            st = self
                .shared
                .cv
                .wait_timeout(st, deadline - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        true
    }

    /// Stop the session and end the control thread; the file ends up `Closed`.
    pub fn shutdown(mut self) {
        self.finish();
    }

    fn finish(&mut self) {
        if let Some(h) = self.control.take() {
            let _ = self.tx.send(Msg::Shutdown);
            let _ = h.join();
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.finish();
    }
}

fn set_state(shared: &Shared, state: EngineState) {
    *lock(&shared.state) = state;
    lock(&shared.stats).state = state;
    shared.cv.notify_all();
}

// ---------------------------------------------------------------------------------------------------------------
// Control thread

struct Session {
    stop: Arc<AtomicBool>,
    join: JoinHandle<()>,
}

fn control_loop(
    rx: Receiver<Msg>,
    shared: Arc<Shared>,
    renderer: Arc<Renderer>,
    writer: Arc<Writer>,
) {
    let mut session: Option<Session> = None;
    let end = |session: &mut Option<Session>| {
        if let Some(s) = session.take() {
            s.stop.store(true, Ordering::Release);
            let _ = s.join.join();
        }
    };
    loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Msg::Start(settings)) => {
                end(&mut session);
                *lock(&shared.error) = None;
                *lock(&shared.output) = settings.output;
                shared.flip.store(settings.flip_vertical, Ordering::Relaxed);
                let stop = Arc::new(AtomicBool::new(false));
                let ctx = RenderCtx {
                    settings: *settings,
                    shared: shared.clone(),
                    renderer: renderer.clone(),
                    writer: writer.clone(),
                    stop: stop.clone(),
                };
                match std::thread::Builder::new()
                    .name("render".into())
                    .spawn(move || render_thread(ctx))
                {
                    Ok(join) => session = Some(Session { stop, join }),
                    Err(e) => {
                        *lock(&shared.error) = Some(format!("cannot start the render thread: {e}"));
                        shared.cv.notify_all();
                    }
                }
            }
            Ok(Msg::Stop) => end(&mut session),
            Ok(Msg::SetOutput(o)) => *lock(&shared.output) = o,
            Ok(Msg::SetFlip(f)) => shared.flip.store(f, Ordering::Relaxed),
            Ok(Msg::SetTap(t)) => *lock(&shared.tap) = t,
            Ok(Msg::Shutdown) | Err(RecvTimeoutError::Disconnected) => {
                end(&mut session);
                writer.set_state(WriterState::Closed);
                return;
            }
            Err(RecvTimeoutError::Timeout) => {
                // Reap a session that ended on its own (camera error).
                if session.as_ref().is_some_and(|s| s.join.is_finished()) {
                    end(&mut session);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Sources

/// A frame on its way into the pipeline.
enum InFrame {
    /// The frame and the visible size inside it: decoders pad the coded size to a multiple of 16 (1920x1088 for a
    /// 1080p MJPEG stream), and only the camera's own size is picture.
    Camera(Frame, (u32, u32)),
    Synthetic(SynthFrame),
}

struct SynthFrame {
    data: Arc<Vec<u8>>,
    stride: i32,
    width: u32,
    height: u32,
    format: PixelFormat,
    color: ColorSettings,
    timestamp_100ns: i64,
}

impl InFrame {
    /// `(capture timestamp, arrival)` in 100 ns QPC units. A timestamp that is missing or implausible (more than a
    /// second from the arrival) is replaced by the arrival time.
    fn times(&self) -> (i64, i64) {
        match self {
            InFrame::Camera(f, _) => {
                let ts = if f.timestamp_100ns > 0
                    && (f.arrival_100ns - f.timestamp_100ns).abs() < 10_000_000
                {
                    f.timestamp_100ns
                } else {
                    f.arrival_100ns
                };
                (ts, f.arrival_100ns)
            }
            InFrame::Synthetic(s) => (s.timestamp_100ns, s.timestamp_100ns),
        }
    }

    fn source_frame(&self, color: ColorSettings) -> Result<SourceFrame<'_>, GpuError> {
        match self {
            InFrame::Camera(f, visible) => {
                let format = f.format.filter(|p| is_supported(*p)).ok_or_else(|| {
                    GpuError::Unsupported(format!(
                        "camera frames in {}",
                        crate::capture::subtype_label(&f.subtype)
                    ))
                })?;
                let data = match &f.data {
                    FrameData::Texture {
                        texture,
                        subresource,
                    } => SourceData::Texture {
                        texture,
                        subresource: *subresource,
                    },
                    FrameData::Cpu(b) => SourceData::Cpu {
                        data: b.data(),
                        stride: b.stride(),
                    },
                };
                Ok(SourceFrame {
                    width: visible.0.min(f.width),
                    height: visible.1.min(f.height),
                    format,
                    data,
                    color,
                    // CPU planar buffers keep the chroma after the padded luma rows.
                    luma_rows: f.height,
                })
            }
            InFrame::Synthetic(s) => Ok(SourceFrame {
                width: s.width,
                height: s.height,
                format: s.format,
                data: SourceData::Cpu {
                    data: &s.data,
                    stride: s.stride,
                },
                color: ColorSettings {
                    space: s.color.space.or(color.space),
                    range: s.color.range.or(color.range),
                },
                luma_rows: 0,
            }),
        }
    }
}

enum SourceEvent {
    Frame(InFrame),
    Timeout,
    Stopped(String),
}

struct SourceInfo {
    description: String,
    width: u32,
    height: u32,
    fps: f64,
    /// What the camera's media type states about matrix and range.
    color: ColorSettings,
}

trait Source {
    fn info(&self) -> &SourceInfo;
    fn next(&mut self, timeout: Duration) -> SourceEvent;
    /// Frames the source made / replaced before they were taken.
    fn counters(&self) -> (u64, u64);
}

struct CameraInput {
    reader: CaptureReader,
    info: SourceInfo,
    drained: u64,
}

/// The picture size of a camera stream: the native size, never more than the coded size the reader delivers.
fn visible_size(i: &crate::capture::reader::StreamInfo) -> (u32, u32) {
    (i.native.width.min(i.width), i.native.height.min(i.height))
}

impl Source for CameraInput {
    fn info(&self) -> &SourceInfo {
        &self.info
    }

    fn next(&mut self, timeout: Duration) -> SourceEvent {
        match self.reader.recv_timeout(timeout) {
            Ok(mut f) => {
                // Latest-frame mailbox: keep only the newest.
                while let Ok(newer) = self.reader.try_recv() {
                    self.drained += 1;
                    f = newer;
                }
                SourceEvent::Frame(InFrame::Camera(f, (self.info.width, self.info.height)))
            }
            Err(RecvError::Timeout) => SourceEvent::Timeout,
            Err(RecvError::Stopped(r)) => SourceEvent::Stopped(r.to_string()),
        }
    }

    fn counters(&self) -> (u64, u64) {
        let s = self.reader.stats();
        (s.produced, s.dropped + self.drained)
    }
}

fn open_camera(spec: &CameraSpec, gpu: &Arc<GpuDevice>) -> Result<CameraInput, String> {
    let mut opts = OpenOptions::new(&spec.link, spec.request);
    opts.hw_decode = spec.hw_decode;
    opts.gpu = Some(gpu.clone());
    // A queue of one is the latest-frame mailbox: a newer frame replaces the one the render thread has not taken.
    opts.queue_len = 1;
    let reader = CaptureReader::open(opts).map_err(|e| e.to_string())?;
    let i = reader.info().clone();
    let color = read_color(&reader);
    let (vw, vh) = visible_size(&i);
    let coded = if (vw, vh) != (i.width, i.height) {
        format!(" coded {}x{}", i.width, i.height)
    } else {
        String::new()
    };
    let description = format!(
        "{} {}x{}{coded}@{} ({}{}, {} frames)",
        i.output_format
            .map(|f| f.name().to_string())
            .unwrap_or_else(|| crate::capture::subtype_label(&i.output_subtype)),
        vw,
        vh,
        i.fps,
        i.decode,
        i.decoder_name
            .as_deref()
            .map(|n| format!(" {n}"))
            .unwrap_or_default(),
        if i.d3d_manager { "texture" } else { "cpu" },
    );
    Ok(CameraInput {
        info: SourceInfo {
            description,
            width: vw,
            height: vh,
            fps: i.fps.as_f64(),
            color,
        },
        reader,
        drained: 0,
    })
}

/// `MF_MT_YUV_MATRIX` and `MF_MT_VIDEO_NOMINAL_RANGE` of the type the reader delivers, when the camera states them.
fn read_color(reader: &CaptureReader) -> ColorSettings {
    // SAFETY: plain COM calls.
    let t = unsafe {
        reader
            .source_reader()
            .GetCurrentMediaType(FIRST_VIDEO_STREAM)
    };
    let Ok(t) = t else {
        return ColorSettings::default();
    };
    // SAFETY: plain attribute reads.
    let (matrix, range) = unsafe {
        (
            t.GetUINT32(&MF_MT_YUV_MATRIX).ok(),
            t.GetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE).ok(),
        )
    };
    ColorSettings {
        // MFVideoTransferMatrix: 1 = BT.709, 2 = BT.601 (the others are not handled yet).
        space: match matrix {
            Some(1) => Some(ColorSpace::Bt709),
            Some(2) => Some(ColorSpace::Bt601),
            // Deviation from the plan 6.1 default (709 from 720 lines up): JPEG is BT.601 by definition (JFIF), and
            // Windows' software MJPEG decoder says so (`601 full` on the W4GS), but the NVIDIA hardware decoder
            // states no matrix. Without this the same camera would change colour with the decoder.
            _ if reader.info().native.format == Some(PixelFormat::Mjpeg) => Some(ColorSpace::Bt601),
            _ => None,
        },
        // MFNominalRange: 1 = 0-255, 2 = 16-235.
        range: match range {
            Some(1) => Some(ColorRange::Full),
            Some(2) => Some(ColorRange::Limited),
            _ => None,
        },
    }
}

struct SyntheticInput {
    spec: SyntheticSpec,
    info: SourceInfo,
    data: Arc<Vec<u8>>,
    stride: usize,
    next_at: Instant,
    made: u64,
}

impl SyntheticInput {
    fn new(spec: SyntheticSpec) -> Result<SyntheticInput, String> {
        let (w, h) = (spec.width, spec.height);
        if w == 0 || h == 0 || w % 2 != 0 || h % 2 != 0 || spec.fps == 0 {
            return Err(format!(
                "synthetic source {w}x{h}@{} is not valid",
                spec.fps
            ));
        }
        let rgb = pattern::smpte_bars(w, h);
        let (space, range) = spec.color.resolve(h);
        let (data, stride) = match spec.format {
            PixelFormat::Nv12 => (
                pattern::pack_nv12(&rgb, w, h, w as usize, h, space, range),
                w as usize,
            ),
            PixelFormat::Yuy2 => (
                pattern::pack_yuy2(&rgb, w, h, w as usize * 2, space, range),
                w as usize * 2,
            ),
            PixelFormat::I420 => (
                pattern::pack_i420(&rgb, w, h, w as usize, space, range, false),
                w as usize,
            ),
            PixelFormat::Yv12 => (
                pattern::pack_i420(&rgb, w, h, w as usize, space, range, true),
                w as usize,
            ),
            PixelFormat::P010 => (
                pattern::pack_p010(&rgb, w, h, w as usize * 2, space, range),
                w as usize * 2,
            ),
            PixelFormat::Xrgb => (
                pattern::pack_bgra(&rgb, w, h, w as usize * 4, 255),
                w as usize * 4,
            ),
            other => return Err(format!("synthetic source cannot make {other}")),
        };
        Ok(SyntheticInput {
            info: SourceInfo {
                description: format!("synthetic bars {} {w}x{h}@{}", spec.format, spec.fps),
                width: w,
                height: h,
                fps: spec.fps as f64,
                color: ColorSettings::default(),
            },
            spec,
            data: Arc::new(data),
            stride,
            next_at: Instant::now(),
            made: 0,
        })
    }
}

impl Source for SyntheticInput {
    fn info(&self) -> &SourceInfo {
        &self.info
    }

    fn next(&mut self, timeout: Duration) -> SourceEvent {
        let now = Instant::now();
        if self.next_at > now + timeout {
            std::thread::sleep(timeout);
            return SourceEvent::Timeout;
        }
        if self.next_at > now {
            std::thread::sleep(self.next_at - now);
        }
        let period = Duration::from_secs_f64(1.0 / self.spec.fps as f64);
        self.next_at = (self.next_at + period).max(Instant::now());
        self.made += 1;
        SourceEvent::Frame(InFrame::Synthetic(SynthFrame {
            data: self.data.clone(),
            stride: self.stride as i32,
            width: self.spec.width,
            height: self.spec.height,
            format: self.spec.format,
            color: self.spec.color,
            timestamp_100ns: qpc_now_100ns(),
        }))
    }

    fn counters(&self) -> (u64, u64) {
        (self.made, 0)
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Statistics

/// Latency samples: a sliding window and the whole run.
#[derive(Default)]
struct LatencyTrack {
    window: VecDeque<f32>,
    total: Vec<f32>,
}

const WINDOW: usize = 240;
const TOTAL_CAP: usize = 200_000;

impl LatencyTrack {
    fn push(&mut self, ms: f64) {
        if self.window.len() == WINDOW {
            self.window.pop_front();
        }
        self.window.push_back(ms as f32);
        if self.total.len() < TOTAL_CAP {
            self.total.push(ms as f32);
        }
    }

    /// `(p50, p95, max)` of the window.
    fn window_stats(&self) -> (f64, f64, f64) {
        let mut v: Vec<f32> = self.window.iter().copied().collect();
        v.sort_by(|a, b| a.total_cmp(b));
        (
            percentile(&v, 0.5),
            percentile(&v, 0.95),
            v.last().copied().unwrap_or(0.0) as f64,
        )
    }

    fn total_p95(&self) -> f64 {
        let mut v = self.total.clone();
        v.sort_by(|a, b| a.total_cmp(b));
        percentile(&v, 0.95)
    }
}

fn percentile(sorted: &[f32], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let i = ((sorted.len() - 1) as f64 * q).ceil() as usize;
    sorted[i.min(sorted.len() - 1)] as f64
}

/// Counters for the one-second window.
struct Window {
    started: Instant,
    captured_at_start: u64,
    rendered: u64,
    published: u64,
    gpu_sum: f64,
    gpu_n: u32,
    readback_sum: f64,
    cpu_sum: f64,
    cpu_n: u32,
}

impl Window {
    fn new(captured: u64) -> Window {
        Window {
            started: Instant::now(),
            captured_at_start: captured,
            rendered: 0,
            published: 0,
            gpu_sum: 0.0,
            gpu_n: 0,
            readback_sum: 0.0,
            cpu_sum: 0.0,
            cpu_n: 0,
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Render thread

struct RenderCtx {
    settings: Settings,
    shared: Arc<Shared>,
    renderer: Arc<Renderer>,
    writer: Arc<Writer>,
    stop: Arc<AtomicBool>,
}

fn render_thread(ctx: RenderCtx) {
    ensure_com();
    let shared = ctx.shared.clone();
    let writer = ctx.writer.clone();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| render_session(&ctx)));
    let outcome = match result {
        Ok(r) => r,
        Err(_) => Err("the render thread panicked".to_string()),
    };
    // Released here: the camera (inside render_session) is already closed.
    writer.set_state(WriterState::Closed);
    if let Err(e) = outcome {
        *lock(&shared.error) = Some(e);
    }
    set_state(&shared, EngineState::Stopped);
}

/// The output size and rate in effect: the settings, else the source's, forced even and inside the protocol limits.
fn resolve_output(o: &OutputSettings, info: &SourceInfo) -> (u32, u32, u32) {
    let even = |v: u32, max: u32| (v.clamp(2, max)) & !1;
    let w = even(
        o.width.unwrap_or(info.width),
        vcam_protocol::MAX_WIDTH.min(3840),
    );
    let h = even(
        o.height.unwrap_or(info.height),
        vcam_protocol::MAX_HEIGHT.min(2160),
    );
    let fps = o.fps.unwrap_or(info.fps.round() as u32).clamp(1, 240);
    (w, h, fps)
}

fn render_session(ctx: &RenderCtx) -> Result<(), String> {
    let RenderCtx {
        settings,
        shared,
        renderer,
        writer,
        stop,
    } = ctx;
    writer.set_state(WriterState::Starting);

    let mut source: Box<dyn Source> = match &settings.source {
        SourceSpec::Camera(c) => Box::new(open_camera(c, &renderer.gpu)?),
        SourceSpec::Synthetic(s) => Box::new(SyntheticInput::new(*s)?),
    };
    if stop.load(Ordering::Acquire) {
        return Ok(());
    }
    let (description, src_h, camera_color) = {
        let i = source.info();
        (i.description.clone(), i.height, i.color)
    };
    let color = ColorSettings {
        space: settings.color.space.or(camera_color.space),
        range: settings.color.range.or(camera_color.range),
    };

    let mut converter = Converter::new(renderer.clone()).map_err(|e| e.to_string())?;
    let stage = OutputStage::new(renderer.clone()).map_err(|e| e.to_string())?;
    let mut chain = FilterChain::new();
    let mut output =
        OutputWriter::new(renderer.clone(), writer.clone()).map_err(|e| e.to_string())?;
    let mut timer = GpuTimer::new(&renderer.gpu.device).ok();

    let mut advertised = (0u32, 0u32, 0u32);
    let mut published_any = false;
    let mut window = Window::new(0);
    let mut latency = LatencyTrack::default();
    let mut pipeline = LatencyTrack::default();
    let mut stats = EngineStats {
        state: EngineState::Live,
        source: format!("{description}; color {}", describe_color(&color, src_h)),
        ..EngineStats::default()
    };
    let (mut rendered, mut published) = (0u64, 0u64);
    let mut consecutive_errors = 0u32;
    let mut last_heartbeat = Instant::now();
    let mut first_error: Option<String> = None;

    set_state(shared, EngineState::Live);
    *lock(&shared.stats) = stats.clone();

    let outcome = loop {
        if stop.load(Ordering::Acquire) {
            break Ok(());
        }
        // Keep the DLL's picture of what we advertise current.
        let out_settings = *lock(&shared.output);
        let (ow, oh, ofps) = resolve_output(&out_settings, source.info());
        if advertised != (ow, oh, ofps) {
            writer.set_advertised(&advertised_for_output(ow, oh, ofps));
            advertised = (ow, oh, ofps);
        }

        // Frames that finished on the GPU while we waited.
        if settings.readback == ReadbackPolicy::NextFrame {
            while let Some(p) = output
                .poll(output.pending() >= 2)
                .map_err(|e| e.to_string())?
            {
                note_published(
                    &p,
                    &mut published,
                    &mut published_any,
                    writer,
                    &mut latency,
                    &mut pipeline,
                    &mut window,
                );
            }
        }

        let frame = match source.next(Duration::from_millis(40)) {
            SourceEvent::Frame(f) => f,
            SourceEvent::Timeout => {
                if last_heartbeat.elapsed() >= Duration::from_millis(100) {
                    writer.heartbeat();
                    last_heartbeat = Instant::now();
                }
                maybe_publish_stats(
                    shared,
                    &mut stats,
                    &mut window,
                    source.as_ref(),
                    &latency,
                    &pipeline,
                    &output,
                    rendered,
                    published,
                    &mut timer,
                    renderer,
                );
                continue;
            }
            SourceEvent::Stopped(reason) => break Err(format!("the camera stopped: {reason}")),
        };
        let (capture_100ns, arrival_100ns) = frame.times();
        let cpu_started = Instant::now();

        // Everything from here to the readback submit is one frame of GPU work.
        let timing = timer
            .as_mut()
            .is_some_and(|t| t.begin(&renderer.gpu.context));
        let rendered_frame = (|| -> Result<(), GpuError> {
            let target = output.target((ow, oh));
            let rgba = {
                let src = frame.source_frame(color)?;
                converter.convert(&src, shared.flip.load(Ordering::Relaxed))?
            };
            // The decoder surface was copied by the pass above; give it back before the rest of the pipeline.
            drop(frame);
            let rgba = chain.run(renderer, rgba)?;
            let fitted = stage.run(
                &rgba,
                &OutputParams {
                    width: target.width,
                    height: target.height,
                    fit: out_settings.fit,
                    background: out_settings.background.map(|c| c as f32 / 255.0),
                },
            )?;
            let capture_qpc = vcam_protocol::hns_to_qpc(capture_100ns);
            if let Some(tap) = lock(&shared.tap).as_mut() {
                tap.on_frame(renderer, &fitted, capture_qpc);
            }
            output.submit(
                &fitted,
                target,
                FrameTag {
                    capture_qpc,
                    capture_100ns,
                    arrival_100ns,
                },
            )?;
            stats.output_width = target.width;
            stats.output_height = target.height;
            stats.output_format = target.format_name();
            Ok(())
        })();
        if timing {
            if let Some(t) = timer.as_mut() {
                t.end(&renderer.gpu.context);
                // The end timestamp belongs to the batch the readback submit just flushed; without this flush it
                // would wait for the next frame's work and read as one frame interval.
                renderer.flush();
            }
        }
        match rendered_frame {
            Ok(()) => {
                consecutive_errors = 0;
                rendered += 1;
                window.rendered += 1;
                window.cpu_sum += cpu_started.elapsed().as_secs_f64() * 1000.0;
                window.cpu_n += 1;
            }
            Err(e) => {
                stats.errors += 1;
                consecutive_errors += 1;
                if first_error.is_none() {
                    first_error = Some(e.to_string());
                }
                if consecutive_errors >= 30 {
                    break Err(format!("rendering failed 30 times in a row: {e}"));
                }
                continue;
            }
        }

        match settings.readback {
            ReadbackPolicy::Immediate => match output.poll(true) {
                Ok(Some(p)) => {
                    window.readback_sum += p.copy_ms;
                    note_published(
                        &p,
                        &mut published,
                        &mut published_any,
                        writer,
                        &mut latency,
                        &mut pipeline,
                        &mut window,
                    );
                }
                Ok(None) => {}
                Err(e) => {
                    stats.errors += 1;
                    first_error.get_or_insert(e.to_string());
                }
            },
            ReadbackPolicy::NextFrame => {
                // Anything the GPU already finished goes out now; the rest on the next pass of the loop.
                while let Some(p) = output.poll(false).map_err(|e| e.to_string())? {
                    window.readback_sum += p.copy_ms;
                    note_published(
                        &p,
                        &mut published,
                        &mut published_any,
                        writer,
                        &mut latency,
                        &mut pipeline,
                        &mut window,
                    );
                }
            }
        }
        last_heartbeat = Instant::now();
        stats.dropped_output = output.dropped + output.rejected;
        maybe_publish_stats(
            shared,
            &mut stats,
            &mut window,
            source.as_ref(),
            &latency,
            &pipeline,
            &output,
            rendered,
            published,
            &mut timer,
            renderer,
        );
    };

    // Drain what is still in flight so the last frame is not lost, then release the camera (drop of `source`).
    while let Ok(Some(_)) = output.poll(true) {}
    if let Err(e) = &outcome {
        eprintln!("[engine] {e}");
    }
    if let Some(e) = first_error {
        eprintln!("[engine] first render error: {e}");
    }
    outcome
}

fn describe_color(c: &ColorSettings, height: u32) -> String {
    let (space, range) = c.resolve(height);
    format!(
        "{}{} {}{}",
        match space {
            ColorSpace::Bt601 => "601",
            ColorSpace::Bt709 => "709",
        },
        if c.space.is_none() { " (default)" } else { "" },
        match range {
            ColorRange::Limited => "limited",
            ColorRange::Full => "full",
        },
        if c.range.is_none() { " (default)" } else { "" },
    )
}

#[allow(clippy::too_many_arguments)]
fn note_published(
    p: &Published,
    published: &mut u64,
    published_any: &mut bool,
    writer: &Writer,
    latency: &mut LatencyTrack,
    pipeline: &mut LatencyTrack,
    window: &mut Window,
) {
    if !*published_any {
        *published_any = true;
        writer.set_state(WriterState::Live);
    }
    *published += 1;
    window.published += 1;
    let done = qpc_to_hns(p.publish_qpc);
    latency.push((done - p.tag.capture_100ns) as f64 / 10_000.0);
    pipeline.push((done - p.tag.arrival_100ns) as f64 / 10_000.0);
}

#[allow(clippy::too_many_arguments)]
fn maybe_publish_stats(
    shared: &Shared,
    stats: &mut EngineStats,
    window: &mut Window,
    source: &dyn Source,
    latency: &LatencyTrack,
    pipeline: &LatencyTrack,
    output: &OutputWriter,
    rendered: u64,
    published: u64,
    timer: &mut Option<GpuTimer>,
    renderer: &Renderer,
) {
    if let Some(t) = timer.as_mut() {
        if let Some(ms) = t.poll(&renderer.gpu.context) {
            window.gpu_sum += ms;
            window.gpu_n += 1;
        }
    }
    let elapsed = window.started.elapsed().as_secs_f64();
    if elapsed < 1.0 {
        return;
    }
    let (captured, dropped) = source.counters();
    stats.capture_fps = captured.saturating_sub(window.captured_at_start) as f64 / elapsed;
    stats.render_fps = window.rendered as f64 / elapsed;
    stats.publish_fps = window.published as f64 / elapsed;
    stats.gpu_ms = if window.gpu_n > 0 {
        window.gpu_sum / window.gpu_n as f64
    } else {
        0.0
    };
    stats.readback_ms = if window.published > 0 {
        window.readback_sum / window.published as f64
    } else {
        0.0
    };
    stats.cpu_ms = if window.cpu_n > 0 {
        window.cpu_sum / window.cpu_n as f64
    } else {
        0.0
    };
    stats.captured = captured;
    stats.rendered = rendered;
    stats.published = published;
    stats.dropped_mailbox = dropped;
    stats.dropped_output = output.dropped + output.rejected;
    let (p50, p95, max) = latency.window_stats();
    stats.latency_p50_ms = p50;
    stats.latency_p95_ms = p95;
    stats.latency_max_ms = max;
    let (q50, q95, _) = pipeline.window_stats();
    stats.pipeline_p50_ms = q50;
    stats.pipeline_p95_ms = q95;
    stats.total_latency_p95_ms = latency.total_p95();
    stats.total_pipeline_p95_ms = pipeline.total_p95();
    stats.consumer = output.writer().consumer_active();
    *lock(&shared.stats) = stats.clone();
    *window = Window::new(captured);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gpu::pattern::{bar_edges, bgrx_to_rgb, max_diff, nv12_to_rgb, smpte_bars};
    use std::path::PathBuf;
    use vcam_protocol::{
        Access, Disposition, Format, MappedFile, Read, SUBTYPE_NV12, SUBTYPE_RGB32,
    };

    fn temp_file(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bettercam-engine-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    /// Latest published frame of the file as `(width, height, subtype, bytes)`.
    fn latest(map: &MappedFile) -> Option<(u32, u32, u32, Vec<u8>)> {
        let raw = map.shared().header();
        let v = vcam_protocol::validate_header(&raw, map.len() as u64, None, None).ok()?;
        let mut out = None;
        let _ = map.shared().read_latest(&v, |meta, data| {
            let mut buf = vec![0u8; data.len()];
            data.copy_to(0, &mut buf);
            out = Some((meta.width, meta.height, meta.subtype, buf));
            Disposition::Taken
        });
        out
    }

    fn wait_for(engine: &Engine, frames: u64) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while engine.stats().published < frames {
            assert!(
                Instant::now() < deadline,
                "no frames: {:?} {:?}",
                engine.stats(),
                engine.last_error()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn synthetic_bars_reach_the_file_and_follow_the_negotiated_format() {
        let gpu = Arc::new(GpuDevice::new(AdapterKind::Warp).expect("WARP"));
        let path = temp_file("engine.bin");
        let _ = std::fs::remove_file(&path);
        let writer = Arc::new(Writer::create(&path).unwrap());
        let engine = Engine::with_parts(gpu, writer).unwrap();
        let reader = MappedFile::open(&path, Access::ReadWrite).unwrap();

        engine.start(Settings {
            source: SourceSpec::Synthetic(SyntheticSpec {
                width: 640,
                height: 360,
                fps: 30,
                format: PixelFormat::Nv12,
                color: ColorSettings {
                    space: Some(ColorSpace::Bt709),
                    range: Some(ColorRange::Limited),
                },
            }),
            output: OutputSettings {
                width: Some(640),
                height: Some(360),
                ..OutputSettings::default()
            },
            flip_vertical: false,
            color: ColorSettings::default(),
            readback: ReadbackPolicy::Immediate,
        });
        engine.wait_live(Duration::from_secs(20)).unwrap();
        wait_for(&engine, 5);
        assert_eq!(engine.state(), EngineState::Live);
        assert_eq!(
            reader.shared().header().writer_state,
            WriterState::Live as u32
        );
        // The advertised list follows the output.
        let adv = reader.shared().header();
        assert_eq!(adv.advertised[0], Format::new(640, 360, 30, SUBTYPE_NV12));

        // No consumer: the configured size as NV12, equal to the source bars.
        let (w, h, subtype, buf) = latest(&reader).expect("a published frame");
        assert_eq!((w, h, subtype), (640, 360, SUBTYPE_NV12));
        let got = nv12_to_rgb(
            &buf,
            w,
            h,
            w as usize,
            ColorSpace::Bt709,
            ColorRange::Limited,
        );
        assert!(max_diff(&got, &smpte_bars(w, h), 3, |_| false) <= 3);

        // A consumer asks for 320x180 RGB32: the engine follows the next frame.
        let raw = MappedFile::open(&path, Access::ReadWrite).unwrap();
        raw.shared()
            .set_negotiated(&Format::new(320, 180, 30, SUBTYPE_RGB32));
        raw.shared().set_consumer_active(true);
        let deadline = Instant::now() + Duration::from_secs(10);
        let frame = loop {
            raw.shared()
                .set_consumer_heartbeat(vcam_protocol::qpc_now());
            if let Some(f) = latest(&reader).filter(|f| f.2 == SUBTYPE_RGB32) {
                break f;
            }
            assert!(
                Instant::now() < deadline,
                "the engine never switched to RGB32"
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        let (w, h, _, buf) = frame;
        assert_eq!((w, h), (320, 180));
        let got = bgrx_to_rgb(&buf, w, h, w as usize * 4);
        // The 640x360 bars scale by exactly 1/2: compare away from the edges.
        let (xs, ys) = bar_edges(640, 360);
        let near = |i: usize| {
            let (x, y) = ((i as u32 % w) * 2, (i as u32 / w) * 2);
            xs.iter().any(|e| e.abs_diff(x) <= 4) || ys.iter().any(|e| e.abs_diff(y) <= 4)
        };
        let want = {
            let full = smpte_bars(640, 360);
            let mut v = vec![0u8; (w * h * 3) as usize];
            for y in 0..h {
                for x in 0..w {
                    let s = ((y * 2 * 640 + x * 2) * 3) as usize;
                    v[((y * w + x) * 3) as usize..][..3].copy_from_slice(&full[s..s + 3]);
                }
            }
            v
        };
        assert!(max_diff(&got, &want, 3, near) <= 3);

        engine.send(Msg::Stop);
        assert!(engine.wait_stopped(Duration::from_secs(10)));
        assert_eq!(
            reader.shared().header().writer_state,
            WriterState::Closed as u32
        );
        let s = engine.stats();
        assert!(s.published >= 5 && s.rendered >= s.published);
        engine.shutdown();
    }

    #[test]
    fn an_unusable_source_reports_an_error_and_stays_stopped() {
        let gpu = Arc::new(GpuDevice::new(AdapterKind::Warp).expect("WARP"));
        let path = temp_file("engine-err.bin");
        let writer = Arc::new(Writer::create(&path).unwrap());
        let engine = Engine::with_parts(gpu, writer).unwrap();
        engine.start(Settings {
            source: SourceSpec::Synthetic(SyntheticSpec {
                width: 641,
                height: 360,
                fps: 30,
                format: PixelFormat::Nv12,
                color: ColorSettings::default(),
            }),
            output: OutputSettings::default(),
            flip_vertical: false,
            color: ColorSettings::default(),
            readback: ReadbackPolicy::Immediate,
        });
        let err = engine.wait_live(Duration::from_secs(10)).unwrap_err();
        assert!(err.contains("not valid"), "{err}");
        assert!(engine.wait_stopped(Duration::from_secs(5)));
        let _ = Read::NoFrame;
    }
}
