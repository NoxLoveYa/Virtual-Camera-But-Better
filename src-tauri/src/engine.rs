//! The engine (plan 2.3, 2.4): camera (or a synthetic source) -> convert -> filter chain -> output stage -> encode ->
//! readback -> `frames.bin`, with the state machine around it.
//!
//! # Threads
//! * The caller owns an [`Engine`] handle and talks to it with [`Msg`]s over an `mpsc` channel (the Tauri commands
//!   go through [`crate::api::Service`], which calls the handle's methods).
//! * The **control thread** owns the state machine: states, timers, the hot-plug watcher, the event sink, the
//!   writer's slate states and the heartbeat. It starts and ends sessions.
//! * A **session thread** (one per open camera; the render thread) opens the source, owns the D3D11 immediate context
//!   while it runs, and loops: take the newest frame (older ones are dropped by the capture queue and by `drain`),
//!   convert, run the filter chain, fit to the output size, encode, read back, publish. It reports to the control
//!   thread once when the camera is open and once when it ends, with the reason.
//!
//! # States (plan 2.4)
//! * `Stopped`: no camera. The file's writer state is `Paused` (the DLL shows "Camera paused"); the heartbeat goes on.
//! * `Standby`: engine ready, camera closed or being opened. Writer `Starting` ("Starting camera...") unless paused.
//!   Reasons: `opening` (a camera is being opened), `reopening` (a capture setting changed while live; the last frame
//!   stays on the file), `idle` (`deactivate_when_not_showing`: nobody watches).
//! * `Live`: the camera is open and frames flow. The first published frame sets the writer to `Live`.
//! * `Reconnecting`: the camera is lost or in use. A retry every `retry_interval`, and at once when the same device
//!   arrives. Any other failure is fatal: `Stopped` with `engine-error`.
//!
//! Pause is not a state: the camera stays open, the writer shows `Paused`, frames are read and dropped.
//!
//! Order: the control thread sets the writer's slate (`Paused`, `Starting`) before it changes `Engine::state` and
//! before the event for the new state goes out (`Control::enter_slate`), so an observer of the state or the event
//! never finds the old slate in the file. `Live` is the other way round on purpose: the state says the camera is
//! open, and the writer leaves `Starting` only when the first frame is published.
//!
//! With `deactivate_when_not_showing` the camera is open only while a consumer is attached or the window is visible;
//! after `standby_delay` (5 s) with neither a `Live` or `Reconnecting` engine goes to `Standby` and closes the camera.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde::Serialize;
use vcam_protocol::{qpc_to_hns, Writer, WriterState};
use windows::core::HRESULT;
use windows::Win32::Media::MediaFoundation::{MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_YUV_MATRIX};

use crate::camera_controls::{
    CameraControls, ControlInfoDto, ControlsHub, ControlsOpener, DeviceOpener,
};
use crate::capture::controls::DeviceControls;
use crate::capture::hotplug::{HotplugEvent, HotplugWatcher};
use crate::capture::reader::{
    CaptureReader, DecodeKind, Frame, FrameData, OpenOptions, RecvError, StopReason,
};
use crate::capture::{
    ensure_com, hr_text, links_match, qpc_now_100ns, CaptureError, FormatRequest, PixelFormat,
    FIRST_VIDEO_STREAM,
};
use crate::config::{advertised_list, arrival_matches, resolve_camera, CaptureCfg};
use crate::gpu::convert::{
    is_supported, ColorRange, ColorSettings, ColorSpace, Converter, SourceData, SourceFrame,
};
use crate::gpu::output::{FitMode, OutputParams, OutputStage};
use crate::gpu::pattern;
use crate::fx::assets::{AssetClient, Assets};
use crate::fx::{Chain, FilterCfg, FilterError};
use crate::gpu::{AdapterKind, GpuDevice, GpuError, GpuTimer, Renderer, RgbaTexture};
use crate::output_writer::{FrameTag, OutputWriter, Published};

// ---------------------------------------------------------------------------------------------------------------
// Public types

/// Engine state (plan 2.4, 5.6).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EngineState {
    #[default]
    Stopped,
    Standby,
    Live,
    Reconnecting,
}

impl EngineState {
    pub fn name(self) -> &'static str {
        match self {
            EngineState::Stopped => "stopped",
            EngineState::Standby => "standby",
            EngineState::Live => "live",
            EngineState::Reconnecting => "reconnecting",
        }
    }
}

/// Why a session ended or a camera could not be opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetryKind {
    /// Another process holds the camera.
    InUse,
    /// The camera was unplugged or invalidated.
    Disconnected,
    /// The camera is not among the connected ones.
    NotFound,
}

/// A failure of the camera side. Retryable ones lead to `Reconnecting`, the others are fatal.
#[derive(Clone, Debug, PartialEq)]
pub enum Failure {
    /// `reason` is what the `engine-state` event carries.
    Retry(RetryKind, String),
    /// The message of the `engine-error` event.
    Fatal(String),
}

impl Failure {
    pub fn from_capture(e: &CaptureError) -> Failure {
        match e {
            CaptureError::InUse(err) => Failure::in_use(Some(err.code())),
            CaptureError::DeviceLost(err) => Failure::disconnected(Some(err.code())),
            CaptureError::NoSuchDevice(_) => {
                Failure::Retry(RetryKind::NotFound, "camera not found".into())
            }
            other => Failure::Fatal(other.to_string()),
        }
    }

    pub fn from_stop(r: StopReason) -> Failure {
        match r {
            StopReason::DeviceLost(hr) => Failure::disconnected(hr),
            StopReason::InUse(hr) => Failure::in_use(Some(hr)),
            other => Failure::Fatal(format!("the camera stopped: {other}")),
        }
    }

    pub fn in_use(hr: Option<HRESULT>) -> Failure {
        Failure::Retry(
            RetryKind::InUse,
            match hr {
                Some(h) => format!("camera in use by another app ({})", hr_text(h)),
                None => "camera in use by another app".into(),
            },
        )
    }

    pub fn disconnected(hr: Option<HRESULT>) -> Failure {
        Failure::Retry(
            RetryKind::Disconnected,
            match hr {
                Some(h) => format!("camera disconnected ({})", hr_text(h)),
                None => "camera disconnected".into(),
            },
        )
    }
}

impl From<String> for Failure {
    fn from(s: String) -> Failure {
        Failure::Fatal(s)
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::Retry(_, r) | Failure::Fatal(r) => f.write_str(r),
        }
    }
}

/// A physical camera, fully resolved.
#[derive(Clone, Debug, PartialEq)]
pub struct CameraSpec {
    /// Symbolic link (`DeviceKey::symbolic_link`).
    pub link: String,
    pub request: FormatRequest,
    /// Hardware decoders and decoder textures (the reader gets the D3D manager); off means CPU buffers.
    pub hw_decode: bool,
}

/// Generated SMPTE colour bars in a camera format, delivered at a fixed rate through the normal pipeline.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SyntheticSpec {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// NV12, YUY2, I420, YV12, P010 or XRGB.
    pub format: PixelFormat,
    pub color: ColorSettings,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SourceSpec {
    Camera(CameraSpec),
    Synthetic(SyntheticSpec),
    /// The frontend's capture settings, resolved against the connected cameras every time the camera is opened (so a
    /// camera that comes back on another port, or a new output rate, is picked up).
    Configured(CaptureCfg),
}

/// Plan 6.3 output settings. `None` size or rate means "same as the camera".
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OutputSettings {
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<u32>,
    pub fit: FitMode,
    pub background: [u8; 3],
    /// Advertise 720p and 360p besides the output size.
    pub extra_sizes: bool,
}

impl Default for OutputSettings {
    fn default() -> Self {
        OutputSettings {
            width: None,
            height: None,
            fps: None,
            fit: FitMode::Fit,
            background: [0, 0, 0],
            extra_sizes: true,
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
    /// Plan 6.1 `deactivate_when_not_showing`.
    pub deactivate_when_not_showing: bool,
}

/// The source side of [`Settings`], for [`Msg::SetSource`]: the camera is reopened only when `source` differs from the
/// running one; the other fields apply to the next frame.
#[derive(Clone, Debug)]
pub struct SourceUpdate {
    pub source: SourceSpec,
    pub flip_vertical: bool,
    pub color: ColorSettings,
    pub deactivate_when_not_showing: bool,
}

/// Called on the render thread with the finished output frame (RGBA, output size) after the output stage. The
/// preview (a later phase) copies and downsizes it there; it must not keep the texture past the call.
pub trait FrameTap: Send {
    fn on_frame(&mut self, renderer: &Renderer, frame: &RgbaTexture, capture_qpc: u64);
}

/// Messages to the engine.
pub enum Msg {
    /// Open the source (now or, with `deactivate_when_not_showing`, when somebody watches) and start rendering. A
    /// running session is stopped first.
    Start(Box<Settings>),
    /// Stop rendering, release the camera, show the paused slate.
    Stop,
    /// Apply new source settings; reopens the camera only when the capture settings changed.
    SetSource(Box<SourceUpdate>),
    /// Change the output settings; takes effect on the next frame (a new size or rate is advertised again).
    SetOutput(OutputSettings),
    SetFlip(bool),
    /// Keep the camera open but show the paused slate.
    SetPause(bool),
    /// The app window is visible (shown and not minimized). Counts as "somebody watches".
    SetWindowVisible(bool),
    SetTap(Option<Box<dyn FrameTap>>),
    /// The newest filter chain. It applies to the next frame; the engine keeps it while stopped, for the next start.
    SetChain(Vec<FilterCfg>),
    /// Stop and end the control thread.
    Shutdown,
}

/// Numbers for the UI footer and the headless example. Times in milliseconds.
#[derive(Clone, Debug, Default)]
pub struct EngineStats {
    pub state: EngineState,
    pub paused: bool,
    /// What is open, e.g. `W4GS mjpeg 1920x1080@60 hw decode, texture frames`.
    pub source: String,
    /// The camera's picture size and native format (what `stats.capture` reports).
    pub capture_width: u32,
    pub capture_height: u32,
    pub capture_format: String,
    /// `"hw"`, `"sw"` or `"none"`.
    pub decode: &'static str,
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

/// The `engine-state` event payload (plan 5.6).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct StateEvent {
    pub state: EngineState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// `stats.capture` of the `stats` event.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct CaptureStatsEvent {
    pub width: u32,
    pub height: u32,
    pub format: String,
    pub fps: f64,
    /// `"hw"`, `"sw"` or `"none"`.
    pub decode: String,
}

/// `stats.output` of the `stats` event.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct OutputStatsEvent {
    pub width: u32,
    pub height: u32,
    pub fps: f64,
}

/// The `stats` event payload (plan 5.6), 4 Hz.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct StatsEvent {
    pub capture: CaptureStatsEvent,
    pub output: OutputStatsEvent,
    pub gpu_ms: f64,
    /// Frames lost before the file (capture mailbox replaced + output ring full or refused).
    pub dropped: u64,
    /// Frames the virtual camera repeated because no new one was there (the DLL's counter, cumulative).
    pub repeated: u64,
    pub latency_p95_ms: f64,
    /// Apps reading the virtual camera: 0 or 1 (the header has one consumer flag).
    pub consumers: u32,
}

/// What the engine reports to its owner.
#[derive(Clone, Debug, PartialEq)]
pub enum EngineEvent {
    State(StateEvent),
    /// Fatal; the engine is `Stopped`.
    Error(String),
    Stats(StatsEvent),
    /// A camera arrived or left.
    DevicesChanged,
    /// The camera's controls (`controls-changed`): about once a second while one is on Auto, and after a re-apply.
    Controls(Vec<ControlInfoDto>),
    /// A filter failed to render and is bypassed (`filter-error`, plan 5.6). Sent once per failure, not per frame.
    /// The asset service (`fx::assets`) uses the same event for a file that could not be loaded
    /// (`could not load <file>: <reason>`), and with an **empty `message`** to clear the card's line after a good
    /// load or when the path was cleared.
    FilterError(FilterError),
}

/// Receives [`EngineEvent`]s on the control thread: return quickly.
pub trait EventSink: Send + Sync {
    fn emit(&self, event: EngineEvent);
}

impl<F: Fn(EngineEvent) + Send + Sync> EventSink for F {
    fn emit(&self, event: EngineEvent) {
        self(event)
    }
}

/// Timers of the state machine. The defaults are the product's; tests shorten them.
#[derive(Clone, Copy, Debug)]
pub struct Timings {
    /// No consumer and window hidden for this long: close the camera (`deactivate_when_not_showing`).
    pub standby_delay: Duration,
    /// Between camera open attempts while `Reconnecting`.
    pub retry_interval: Duration,
    /// How often the control thread looks at consumers, timers and messages.
    pub tick: Duration,
    /// Between `stats` events.
    pub stats_interval: Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Timings {
            standby_delay: Duration::from_secs(5),
            retry_interval: Duration::from_secs(3),
            tick: Duration::from_millis(50),
            stats_interval: Duration::from_millis(250),
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Handle

struct Shared {
    state: Mutex<EngineState>,
    reason: Mutex<Option<String>>,
    cv: Condvar,
    stats: Mutex<EngineStats>,
    error: Mutex<Option<String>>,
    output: Mutex<OutputSettings>,
    flip: AtomicBool,
    color: Mutex<ColorSettings>,
    paused: AtomicBool,
    tap: Mutex<Option<Box<dyn FrameTap>>>,
    /// The newest chain and its version: the render thread applies it when the version changed.
    chain: Mutex<Arc<Vec<FilterCfg>>>,
    chain_gen: AtomicU64,
    sink: Mutex<Option<Arc<dyn EventSink>>>,
    /// Symbolic link of the open camera (for hot-plug removal), empty while none is open.
    camera_link: Mutex<Option<String>>,
    /// Last advertised `(width, height, fps)`, for a consumer that attaches while the camera is closed.
    advertised: Mutex<Option<(u32, u32, u32)>>,
    /// Device controls: the open camera's, the stored record, the lease for calls while no camera is open.
    hub: Arc<ControlsHub>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn emit(shared: &Shared, event: EngineEvent) {
    let sink = lock(&shared.sink).clone();
    if let Some(s) = sink {
        s.emit(event);
    }
}

/// Handle to the engine threads. Dropping it stops everything.
pub struct Engine {
    tx: Sender<Msg>,
    shared: Arc<Shared>,
    writer: Arc<Writer>,
    control: Mutex<Option<JoinHandle<()>>>,
    /// Reads the controls that are on Auto once a second and closes an idle lease.
    poller: Mutex<Option<(Arc<AtomicBool>, JoinHandle<()>)>>,
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
        Engine::with_timings(gpu, writer, Timings::default())
    }

    /// [`Engine::with_parts`] with other timers.
    pub fn with_timings(
        gpu: Arc<GpuDevice>,
        writer: Arc<Writer>,
        timings: Timings,
    ) -> Result<Engine, String> {
        Engine::build(
            gpu,
            writer,
            timings,
            Arc::new(DefaultFactory),
            Arc::new(DeviceOpener),
        )
    }

    fn build(
        gpu: Arc<GpuDevice>,
        writer: Arc<Writer>,
        timings: Timings,
        factory: Arc<dyn SourceFactory>,
        opener: Arc<dyn ControlsOpener>,
    ) -> Result<Engine, String> {
        let renderer = Arc::new(Renderer::new(gpu).map_err(|e| e.to_string())?);
        renderer.shaders.warm_up().map_err(|e| e.to_string())?;
        crate::fx::warm_up(&renderer).map_err(|e| e.to_string())?;
        let shared = Arc::new(Shared {
            state: Mutex::new(EngineState::Stopped),
            reason: Mutex::new(None),
            cv: Condvar::new(),
            stats: Mutex::new(EngineStats {
                decode: "none",
                ..EngineStats::default()
            }),
            error: Mutex::new(None),
            output: Mutex::new(OutputSettings::default()),
            flip: AtomicBool::new(false),
            color: Mutex::new(ColorSettings::default()),
            paused: AtomicBool::new(false),
            tap: Mutex::new(None),
            chain: Mutex::new(Arc::new(Vec::new())),
            chain_gen: AtomicU64::new(0),
            sink: Mutex::new(None),
            camera_link: Mutex::new(None),
            advertised: Mutex::new(None),
            hub: Arc::new(ControlsHub::new(opener)),
        });
        let poller = spawn_poller(shared.clone()).map_err(|e| e.to_string())?;
        // The asset loader reports failed loads (and the clears) straight to the sink, from its own thread.
        let assets = {
            let shared = shared.clone();
            Assets::new(
                renderer.gpu.clone(),
                Arc::new(move |e| emit(&shared, EngineEvent::FilterError(e))),
            )
        };
        let (tx, rx) = mpsc::channel();
        let (events_tx, events_rx) = mpsc::channel();
        let control = {
            let c = Control {
                rx,
                events_rx,
                events_tx,
                shared: shared.clone(),
                renderer,
                assets,
                writer: writer.clone(),
                factory,
                timings,
                cfg: None,
                phase: EngineState::Stopped,
                session: None,
                gen: 0,
                window_visible: false,
                idle_since: None,
                next_retry: None,
                attempt_started: Instant::now(),
                retrying: false,
                emitted: None,
                last_heartbeat: Instant::now(),
                last_stats: Instant::now(),
                hotplug: None,
            };
            std::thread::Builder::new()
                .name("engine control".into())
                .spawn(move || c.run())
                .map_err(|e| e.to_string())?
        };
        Ok(Engine {
            tx,
            shared,
            writer,
            control: Mutex::new(Some(control)),
            poller: Mutex::new(Some(poller)),
        })
    }

    /// The device controls (plan 5.5): calls for the camera the engine has open, or a lease on one it has not, and
    /// the record that is re-applied after every open.
    pub fn controls(&self) -> &Arc<ControlsHub> {
        &self.shared.hub
    }

    /// Where the engine reports to (replaces the previous sink). Events come from the control thread.
    pub fn set_event_sink(&self, sink: Arc<dyn EventSink>) {
        *lock(&self.shared.sink) = Some(sink);
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

    /// `Msg::SetSource`.
    pub fn set_source(&self, update: SourceUpdate) {
        self.send(Msg::SetSource(Box::new(update)));
    }

    /// `Msg::SetOutput`.
    pub fn set_output(&self, output: OutputSettings) {
        self.send(Msg::SetOutput(output));
    }

    /// `Msg::SetPause`; the flag is set at once so the render thread follows within a frame.
    pub fn set_pause(&self, on: bool) {
        self.shared.paused.store(on, Ordering::Release);
        self.send(Msg::SetPause(on));
    }

    /// `Msg::SetChain`: the render thread applies it before the next frame; no reopen, no restart.
    pub fn set_chain(&self, chain: Vec<FilterCfg>) {
        self.send(Msg::SetChain(chain));
    }

    /// `Msg::SetWindowVisible`.
    pub fn set_window_visible(&self, visible: bool) {
        self.send(Msg::SetWindowVisible(visible));
    }

    pub fn paused(&self) -> bool {
        self.shared.paused.load(Ordering::Acquire)
    }

    pub fn state(&self) -> EngineState {
        *lock(&self.shared.state)
    }

    /// The reason of the last state change (`reconnecting`: what is wrong; `standby`: `opening`, `reopening`,
    /// `idle`).
    pub fn reason(&self) -> Option<String> {
        lock(&self.shared.reason).clone()
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

    /// Block until the state is `Live` (`Ok`), a start fails fatally (`Err` with the reason), or the timeout passes.
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

    /// Block until the state is `state` or the timeout passes.
    pub fn wait_state(&self, state: EngineState, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut st = lock(&self.shared.state);
        while *st != state {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            st = self
                .shared
                .cv
                .wait_timeout(st, (deadline - now).min(Duration::from_millis(50)))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        true
    }

    /// Block until the state is `Stopped` or the timeout passes.
    pub fn wait_stopped(&self, timeout: Duration) -> bool {
        self.wait_state(EngineState::Stopped, timeout)
    }

    /// Stop the session and end the control thread; the file ends up `Closed`. Safe to call twice.
    pub fn shutdown(self) {
        self.close();
    }

    /// [`Engine::shutdown`] for a shared handle.
    pub fn close(&self) {
        let handle = lock(&self.control).take();
        if let Some(h) = handle {
            let _ = self.tx.send(Msg::Shutdown);
            let _ = h.join();
        }
        let poller = lock(&self.poller).take();
        if let Some((stop, h)) = poller {
            stop.store(true, Ordering::Release);
            let _ = h.join();
        }
        self.shared.hub.release_lease();
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.close();
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Control thread

/// One open camera and its render thread.
struct Session {
    gen: u64,
    stop: Arc<AtomicBool>,
    /// Set by the control thread when the hot-plug watcher reports this camera removed.
    removed: Arc<AtomicBool>,
    join: JoinHandle<()>,
}

struct SessionEvent {
    gen: u64,
    msg: SessionMsg,
}

enum SessionMsg {
    /// The camera is open and the first frame arrived.
    Opened,
    /// The session thread is done (the camera is closed). `Ok` only after the stop flag.
    Ended(Result<(), Failure>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OpenMode {
    /// From `Standby`: the writer shows "starting", the state says `opening`.
    Fresh,
    /// A capture setting changed while live: the last frame stays on the file, the state says `reopening`.
    Reopen,
    /// A retry while `Reconnecting`: the state does not change until it works.
    Retry,
}

struct Control {
    rx: Receiver<Msg>,
    events_rx: Receiver<SessionEvent>,
    events_tx: Sender<SessionEvent>,
    shared: Arc<Shared>,
    renderer: Arc<Renderer>,
    /// The files filters read (pictures, tables): one loader thread for the engine's lifetime.
    assets: Assets,
    writer: Arc<Writer>,
    factory: Arc<dyn SourceFactory>,
    timings: Timings,
    /// What the engine is running; `None` while `Stopped`.
    cfg: Option<Settings>,
    phase: EngineState,
    session: Option<Session>,
    gen: u64,
    window_visible: bool,
    /// Since when nobody watches (`deactivate_when_not_showing`).
    idle_since: Option<Instant>,
    next_retry: Option<Instant>,
    attempt_started: Instant,
    /// The session that is being opened is a retry.
    retrying: bool,
    emitted: Option<(EngineState, Option<String>)>,
    last_heartbeat: Instant,
    last_stats: Instant,
    hotplug: Option<(HotplugWatcher, Receiver<HotplugEvent>)>,
}

impl Control {
    fn run(mut self) {
        ensure_com();
        match HotplugWatcher::start() {
            Ok(h) => self.hotplug = Some(h),
            Err(e) => eprintln!("[engine] hot-plug watcher unavailable: {e}"),
        }
        self.writer.set_state(WriterState::Paused);
        loop {
            let mut next = match self.rx.recv_timeout(self.timings.tick) {
                Ok(m) => Some(m),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => {
                    self.shutdown();
                    return;
                }
            };
            while let Some(m) = next.take() {
                if matches!(m, Msg::Shutdown) {
                    self.shutdown();
                    return;
                }
                self.handle(m);
                next = self.rx.try_recv().ok();
            }
            self.poll_session();
            self.poll_hotplug();
            self.tick();
        }
    }

    fn shutdown(&mut self) {
        self.end_session();
        self.cfg = None;
        self.writer.set_state(WriterState::Closed);
        *lock(&self.shared.state) = EngineState::Stopped;
        lock(&self.shared.stats).state = EngineState::Stopped;
        self.shared.cv.notify_all();
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Start(s) => self.start(*s),
            Msg::Stop => self.stop(),
            Msg::SetSource(u) => self.set_source(*u),
            Msg::SetOutput(o) => {
                *lock(&self.shared.output) = o;
                if self.cfg.is_some() && self.session.is_none() {
                    self.advertise_idle();
                }
            }
            Msg::SetFlip(f) => self.shared.flip.store(f, Ordering::Relaxed),
            Msg::SetPause(on) => self.set_pause(on),
            Msg::SetWindowVisible(v) => self.window_visible = v,
            Msg::SetTap(t) => *lock(&self.shared.tap) = t,
            Msg::SetChain(c) => {
                *lock(&self.shared.chain) = Arc::new(c);
                self.shared.chain_gen.fetch_add(1, Ordering::Release);
            }
            Msg::Shutdown => {}
        }
    }

    // ---- state

    fn set_phase(&mut self, phase: EngineState, reason: Option<String>) {
        self.phase = phase;
        // The event goes out before `Engine::state` changes: whoever sees the state has had the event.
        let now = Some((phase, reason.clone()));
        if self.emitted != now {
            self.emitted = now;
            emit(
                &self.shared,
                EngineEvent::State(StateEvent {
                    state: phase,
                    reason: reason.clone(),
                }),
            );
        }
        *lock(&self.shared.reason) = reason;
        *lock(&self.shared.state) = phase;
        lock(&self.shared.stats).state = phase;
        self.shared.cv.notify_all();
    }

    /// [`Control::set_phase`] for a phase in which no frames flow (`Stopped`, idle `Standby`, `Reconnecting`): the
    /// writer shows its slate first, so whoever sees the state or its event finds the writer already there.
    fn enter_slate(&mut self, phase: EngineState, reason: Option<String>) {
        self.set_slate();
        self.set_phase(phase, reason);
    }

    /// The writer state while no frames flow: `Paused` when stopped or paused, else `Starting`. Depends on `cfg` and
    /// the pause flag, not on the phase.
    fn set_slate(&self) {
        let paused = self.shared.paused.load(Ordering::Acquire);
        self.writer.set_state(if self.cfg.is_none() || paused {
            WriterState::Paused
        } else {
            WriterState::Starting
        });
    }

    fn wants_camera(&self) -> bool {
        match &self.cfg {
            None => false,
            Some(c) => {
                !c.deactivate_when_not_showing
                    || self.window_visible
                    || self.writer.consumer_active()
            }
        }
    }

    fn target_link(&self) -> Option<String> {
        match &self.cfg.as_ref()?.source {
            SourceSpec::Camera(c) => Some(c.link.clone()),
            SourceSpec::Configured(c) => c.device.clone(),
            SourceSpec::Synthetic(_) => None,
        }
    }

    // ---- commands

    fn start(&mut self, s: Settings) {
        self.end_session();
        *lock(&self.shared.error) = None;
        *lock(&self.shared.output) = s.output;
        self.shared.flip.store(s.flip_vertical, Ordering::Relaxed);
        *lock(&self.shared.color) = s.color;
        self.shared.paused.store(false, Ordering::Release);
        self.idle_since = None;
        self.next_retry = None;
        self.retrying = false;
        self.cfg = Some(s);
        if self.wants_camera() {
            self.open_session(OpenMode::Fresh);
        } else {
            self.go_idle();
        }
    }

    fn stop(&mut self) {
        self.end_session();
        self.cfg = None;
        self.shared.paused.store(false, Ordering::Release);
        self.idle_since = None;
        self.next_retry = None;
        self.retrying = false;
        self.enter_slate(EngineState::Stopped, None);
        self.emit_stats();
    }

    /// A fatal failure: `Stopped`, with the message for the owner.
    fn fail(&mut self, message: String) {
        self.end_session();
        eprintln!("[engine] {message}");
        *lock(&self.shared.error) = Some(message.clone());
        self.cfg = None;
        self.shared.paused.store(false, Ordering::Release);
        self.idle_since = None;
        self.next_retry = None;
        self.retrying = false;
        // The slate first: neither the error nor the `Stopped` event may be seen with the writer still `Starting`.
        // The slate first: neither the error nor the `Stopped` event may be seen with the writer still `Starting`.
        self.set_slate();
        emit(&self.shared, EngineEvent::Error(message));
        self.enter_slate(EngineState::Stopped, Some("error".into()));
        self.emit_stats();
    }

    fn set_source(&mut self, u: SourceUpdate) {
        let Some(cfg) = self.cfg.as_mut() else {
            return;
        };
        let reopen = cfg.source != u.source;
        cfg.source = u.source;
        cfg.flip_vertical = u.flip_vertical;
        cfg.color = u.color;
        cfg.deactivate_when_not_showing = u.deactivate_when_not_showing;
        self.shared.flip.store(u.flip_vertical, Ordering::Relaxed);
        *lock(&self.shared.color) = u.color;
        if !reopen {
            return;
        }
        match self.phase {
            EngineState::Live => self.open_session(OpenMode::Reopen),
            EngineState::Reconnecting => self.open_session(OpenMode::Retry),
            EngineState::Standby if self.session.is_some() => self.open_session(OpenMode::Fresh),
            // Idle standby: the next wake-up opens the camera with the new settings.
            EngineState::Standby | EngineState::Stopped => {}
        }
    }

    fn set_pause(&mut self, on: bool) {
        if self.cfg.is_none() {
            self.shared.paused.store(false, Ordering::Release);
            return;
        }
        self.shared.paused.store(on, Ordering::Release);
        if on {
            self.writer.set_state(WriterState::Paused);
        } else if self.phase != EngineState::Live || self.session.is_none() {
            self.set_slate();
        }
        // Live and resumed: the render thread leaves `Paused` with the first frame it publishes.
    }

    // ---- sessions

    fn open_session(&mut self, mode: OpenMode) {
        let Some(cfg) = &self.cfg else {
            return;
        };
        let (source, readback) = (cfg.source.clone(), cfg.readback);
        self.end_session();
        // A lease on the camera (a control call while no camera was open) makes way for the engine.
        self.shared.hub.release_lease();
        self.gen += 1;
        let (stop, removed) = (
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );
        let ctx = SessionCtx {
            source,
            readback,
            gen: self.gen,
            shared: self.shared.clone(),
            renderer: self.renderer.clone(),
            assets: self.assets.client(),
            writer: self.writer.clone(),
            factory: self.factory.clone(),
            stop: stop.clone(),
            removed: removed.clone(),
            events: self.events_tx.clone(),
        };
        self.attempt_started = Instant::now();
        self.retrying = mode == OpenMode::Retry;
        self.next_retry = None;
        if mode == OpenMode::Fresh {
            // Before the session thread exists: its first published frame sets `Live`, and the slate must not
            // overwrite that.
            self.set_slate();
        }
        match std::thread::Builder::new()
            .name("render".into())
            .spawn(move || session_thread(ctx))
        {
            Ok(join) => {
                self.session = Some(Session {
                    gen: self.gen,
                    stop,
                    removed,
                    join,
                })
            }
            Err(e) => {
                self.fail(format!("cannot start the render thread: {e}"));
                return;
            }
        }
        match mode {
            OpenMode::Fresh => self.set_phase(EngineState::Standby, Some("opening".into())),
            OpenMode::Reopen => self.set_phase(EngineState::Standby, Some("reopening".into())),
            OpenMode::Retry => {}
        }
    }

    /// Stop the session thread and wait for it: the camera is closed when this returns.
    fn end_session(&mut self) {
        if let Some(s) = self.session.take() {
            s.stop.store(true, Ordering::Release);
            let _ = s.join.join();
        }
        *lock(&self.shared.camera_link) = None;
    }

    /// `Standby` with the camera closed: nobody watches.
    fn go_idle(&mut self) {
        self.end_session();
        self.idle_since = None;
        self.next_retry = None;
        self.retrying = false;
        self.enter_slate(EngineState::Standby, Some("idle".into()));
        self.advertise_idle();
    }

    fn go_reconnecting(&mut self, reason: String) {
        // A failed retry waits one interval from its own start; a camera lost while live waits from now.
        let base = if self.retrying {
            self.attempt_started
        } else {
            Instant::now()
        };
        self.retrying = false;
        self.next_retry = Some((base + self.timings.retry_interval).max(Instant::now()));
        self.enter_slate(EngineState::Reconnecting, Some(reason));
        self.advertise_idle();
    }

    /// Keep the advertised media types current while no camera is open, so a consumer that attaches then (and wakes
    /// the engine) sees the configured size: an explicit output size, else what the last session advertised.
    fn advertise_idle(&self) {
        let o = *lock(&self.shared.output);
        let last = *lock(&self.shared.advertised);
        let (w, h, fps) = match (o.width, o.height, last) {
            (Some(w), Some(h), l) => (w, h, o.fps.or(l.map(|l| l.2)).unwrap_or(30)),
            (_, _, Some((w, h, f))) => (w, h, o.fps.unwrap_or(f)),
            _ => return,
        };
        self.writer
            .set_advertised(&advertised_list(w, h, fps, o.extra_sizes));
    }

    fn poll_session(&mut self) {
        while let Ok(ev) = self.events_rx.try_recv() {
            if self.session.as_ref().map(|s| s.gen) != Some(ev.gen) {
                continue; // an older session that was replaced
            }
            match ev.msg {
                SessionMsg::Opened => {
                    self.idle_since = None;
                    self.retrying = false;
                    self.next_retry = None;
                    self.set_phase(EngineState::Live, None);
                }
                SessionMsg::Ended(result) => {
                    self.end_session();
                    match result {
                        Ok(()) => self.go_idle(),
                        Err(Failure::Retry(_, reason)) => self.go_reconnecting(reason),
                        Err(Failure::Fatal(message)) => self.fail(message),
                    }
                }
            }
        }
    }

    fn poll_hotplug(&mut self) {
        let events: Vec<HotplugEvent> = match &self.hotplug {
            Some((_, rx)) => rx.try_iter().collect(),
            None => return,
        };
        if events.is_empty() {
            return;
        }
        emit(&self.shared, EngineEvent::DevicesChanged);
        for ev in events {
            match ev {
                HotplugEvent::Arrival { link } => {
                    if self.phase == EngineState::Reconnecting
                        && self.session.is_none()
                        && arrival_matches(self.target_link().as_deref(), &link)
                    {
                        self.next_retry = Some(Instant::now());
                    }
                }
                HotplugEvent::Removal { link } => {
                    let open = lock(&self.shared.camera_link).clone();
                    if let (Some(s), Some(open)) = (&self.session, open) {
                        if links_match(&open, &link) {
                            s.removed.store(true, Ordering::Release);
                        }
                    }
                }
            }
        }
    }

    fn tick(&mut self) {
        let now = Instant::now();
        // The DLL shows "not running" after a second without a heartbeat; the render thread beats while it works,
        // this covers everything else (opening, standby, stopped).
        if now.duration_since(self.last_heartbeat) >= Duration::from_millis(100) {
            self.writer.heartbeat();
            self.last_heartbeat = now;
        }

        if let Some(deactivate) = self.cfg.as_ref().map(|c| c.deactivate_when_not_showing) {
            let wants = self.wants_camera();
            match self.phase {
                EngineState::Standby if self.session.is_none() && wants => {
                    self.open_session(OpenMode::Fresh);
                }
                EngineState::Live | EngineState::Reconnecting if deactivate && !wants => {
                    let since = *self.idle_since.get_or_insert(now);
                    if now.duration_since(since) >= self.timings.standby_delay {
                        self.go_idle();
                    }
                }
                _ => self.idle_since = None,
            }
            if self.phase == EngineState::Reconnecting
                && self.session.is_none()
                && self.next_retry.is_some_and(|t| now >= t)
            {
                self.open_session(OpenMode::Retry);
            }
        }

        if self.phase != EngineState::Stopped
            && now.duration_since(self.last_stats) >= self.timings.stats_interval
        {
            // A fixed cadence (4 Hz), not "interval after the last one": the tick would stretch every period.
            self.last_stats += self.timings.stats_interval;
            if now.duration_since(self.last_stats) >= self.timings.stats_interval {
                self.last_stats = now;
            }
            self.emit_stats();
        }
    }

    fn emit_stats(&self) {
        let s = lock(&self.shared.stats).clone();
        let live = self.phase == EngineState::Live;
        let header = self.writer.shared().header();
        let event = StatsEvent {
            capture: CaptureStatsEvent {
                width: if live { s.capture_width } else { 0 },
                height: if live { s.capture_height } else { 0 },
                format: if live {
                    s.capture_format.clone()
                } else {
                    String::new()
                },
                fps: if live { s.capture_fps } else { 0.0 },
                decode: if live {
                    s.decode.to_string()
                } else {
                    "none".to_string()
                },
            },
            output: OutputStatsEvent {
                width: if live { s.output_width } else { 0 },
                height: if live { s.output_height } else { 0 },
                fps: if live { s.publish_fps } else { 0.0 },
            },
            gpu_ms: if live { s.gpu_ms } else { 0.0 },
            dropped: if live {
                s.dropped_mailbox + s.dropped_output
            } else {
                0
            },
            repeated: header.repeated,
            latency_p95_ms: if live { s.latency_p95_ms } else { 0.0 },
            consumers: self.writer.consumer_active() as u32,
        };
        emit(&self.shared, EngineEvent::Stats(event));
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Session thread

struct SessionCtx {
    source: SourceSpec,
    readback: ReadbackPolicy,
    gen: u64,
    shared: Arc<Shared>,
    renderer: Arc<Renderer>,
    assets: AssetClient,
    writer: Arc<Writer>,
    factory: Arc<dyn SourceFactory>,
    stop: Arc<AtomicBool>,
    removed: Arc<AtomicBool>,
    events: Sender<SessionEvent>,
}

fn session_thread(ctx: SessionCtx) {
    ensure_com();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| render_session(&ctx)));
    let outcome = match result {
        Ok(r) => r,
        Err(_) => Err(Failure::Fatal("the render thread panicked".into())),
    };
    // The camera (inside `render_session`) is closed by now.
    let _ = ctx.events.send(SessionEvent {
        gen: ctx.gen,
        msg: SessionMsg::Ended(outcome),
    });
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
    Stopped(Failure),
}

#[derive(Clone)]
struct SourceInfo {
    description: String,
    width: u32,
    height: u32,
    fps: f64,
    /// What the camera's media type states about matrix and range.
    color: ColorSettings,
    /// The camera's native format name (`mjpeg`, `nv12`, ...).
    format: String,
    /// `"hw"`, `"sw"` or `"none"`.
    decode: &'static str,
    /// Symbolic link of the camera, for hot-plug removal.
    link: Option<String>,
}

/// Opens sources. The engine uses [`DefaultFactory`]; tests plug in sources that fail and recover on command.
trait SourceFactory: Send + Sync {
    /// `output_fps` is the output rate in effect (for `frame_interval` -1).
    fn open(
        &self,
        spec: &SourceSpec,
        output_fps: Option<u32>,
        gpu: &Arc<GpuDevice>,
    ) -> Result<Box<dyn Source>, Failure>;
}

struct DefaultFactory;

impl SourceFactory for DefaultFactory {
    fn open(
        &self,
        spec: &SourceSpec,
        output_fps: Option<u32>,
        gpu: &Arc<GpuDevice>,
    ) -> Result<Box<dyn Source>, Failure> {
        Ok(match spec {
            SourceSpec::Camera(c) => Box::new(open_camera(c, gpu)?),
            SourceSpec::Configured(cap) => {
                let c = resolve_camera(cap, output_fps)?;
                Box::new(open_camera(&c, gpu)?)
            }
            SourceSpec::Synthetic(s) => Box::new(SyntheticInput::new(*s)?),
        })
    }
}

trait Source {
    fn info(&self) -> &SourceInfo;
    fn next(&mut self, timeout: Duration) -> SourceEvent;
    /// Frames the source made / replaced before they were taken.
    fn counters(&self) -> (u64, u64);
    /// The camera's device controls, when it has any (a synthetic source has none).
    fn controls(&self) -> Option<Arc<dyn CameraControls>> {
        None
    }
}

struct CameraInput {
    // Before `reader`: the controls hold the camera's media source and go first (plan 4.1, `DeviceControls`).
    controls: Option<Arc<DeviceControls>>,
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
            Err(RecvError::Stopped(r)) => SourceEvent::Stopped(Failure::from_stop(r)),
        }
    }

    fn counters(&self) -> (u64, u64) {
        let s = self.reader.stats();
        (s.produced, s.dropped + self.drained)
    }

    fn controls(&self) -> Option<Arc<dyn CameraControls>> {
        self.controls
            .as_ref()
            .map(|c| c.clone() as Arc<dyn CameraControls>)
    }
}

fn open_camera(spec: &CameraSpec, gpu: &Arc<GpuDevice>) -> Result<CameraInput, Failure> {
    let mut opts = OpenOptions::new(&spec.link, spec.request);
    opts.hw_decode = spec.hw_decode;
    opts.gpu = Some(gpu.clone());
    // A queue of one is the latest-frame mailbox: a newer frame replaces the one the render thread has not taken.
    opts.queue_len = 1;
    let reader = CaptureReader::open(opts).map_err(|e| Failure::from_capture(&e))?;
    // A camera that offers no controls simply has none to show.
    let controls = match DeviceControls::from_reader(&reader) {
        Ok(c) => Some(Arc::new(c)),
        Err(e) => {
            eprintln!("[engine] no device controls: {e}");
            None
        }
    };
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
            format: i
                .native
                .format
                .map(|f| f.name().to_string())
                .unwrap_or_else(|| crate::capture::subtype_label(&i.native.subtype)),
            decode: match i.decode {
                DecodeKind::None => "none",
                DecodeKind::Hardware => "hw",
                DecodeKind::Software => "sw",
            },
            link: Some(spec.link.clone()),
        },
        controls,
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
                format: spec.format.name().to_string(),
                decode: "none",
                link: None,
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
// Device controls

/// The open camera's controls, handed to the hub while the session runs, and the helper thread that re-applies the
/// stored record. Dropping it (before the camera closes) cancels the helper and waits for it.
struct ControlsGuard {
    hub: Arc<ControlsHub>,
    token: u64,
    cancel: Arc<AtomicBool>,
    apply: Option<JoinHandle<()>>,
}

impl Drop for ControlsGuard {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
        self.hub.detach(self.token);
        if let Some(h) = self.apply.take() {
            let _ = h.join();
        }
    }
}

/// Hand the camera's controls to the hub and re-apply the record on a helper thread: a first write to white
/// balance, focus or exposure takes 200-700 ms, which must not hold up the first frame.
fn attach_controls(shared: &Arc<Shared>, source: &dyn Source) -> Option<ControlsGuard> {
    let device = source.controls()?;
    let link = source.info().link.clone()?;
    let hub = shared.hub.clone();
    let token = hub.attach(&link, device.clone());
    let cancel = Arc::new(AtomicBool::new(false));
    let apply = {
        let (shared, hub, cancel) = (shared.clone(), hub.clone(), cancel.clone());
        std::thread::Builder::new()
            .name("controls apply".into())
            .spawn(move || {
                ensure_com();
                match hub.apply_on_open(token, &link, &device, &cancel) {
                    Ok((report, infos)) => {
                        if !report.applied.is_empty() {
                            eprintln!(
                                "[engine] stored controls applied after open: {}",
                                report
                                    .applied
                                    .iter()
                                    .map(|a| match (a.auto, a.value) {
                                        (Some(on), None) => format!("{}=auto {on}", a.key),
                                        (_, Some(v)) => format!("{}={v}", a.key),
                                        _ => a.key.to_string(),
                                    })
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            );
                        }
                        for (key, why) in &report.skipped {
                            eprintln!("[engine] stored control {key} not applied: {why}");
                        }
                        if !cancel.load(Ordering::Acquire) {
                            emit(&shared, EngineEvent::Controls(infos));
                        }
                    }
                    Err(e) => eprintln!("[engine] device controls: {e}"),
                }
            })
            .ok()
    };
    Some(ControlsGuard {
        hub,
        token,
        cancel,
        apply,
    })
}

/// The engine's background thread for the controls: every second the controls that are on Auto are read again and
/// sent as `controls-changed` (so sliders follow the device), and a lease that has been idle for a while is closed.
fn spawn_poller(shared: Arc<Shared>) -> std::io::Result<(Arc<AtomicBool>, JoinHandle<()>)> {
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let join = std::thread::Builder::new()
        .name("controls poll".into())
        .spawn(move || {
            ensure_com();
            let mut last_poll = Instant::now();
            while !flag.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(100));
                shared.hub.expire_lease();
                if last_poll.elapsed() >= Duration::from_secs(1) {
                    last_poll = Instant::now();
                    if let Some(infos) = shared.hub.poll() {
                        emit(&shared, EngineEvent::Controls(infos));
                    }
                }
            }
        })?;
    Ok((stop, join))
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
// Render loop

/// Replace the published statistics but keep the state the control thread owns.
fn store_stats(shared: &Shared, stats: &EngineStats) {
    let mut s = lock(&shared.stats);
    let state = s.state;
    *s = stats.clone();
    s.state = state;
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

fn render_session(ctx: &SessionCtx) -> Result<(), Failure> {
    let SessionCtx {
        shared,
        renderer,
        writer,
        stop,
        removed,
        ..
    } = ctx;
    let output_fps = lock(&shared.output).fps;
    let mut source: Box<dyn Source> = ctx.factory.open(&ctx.source, output_fps, &renderer.gpu)?;
    if stop.load(Ordering::Acquire) {
        return Ok(());
    }
    // After `source`, so it is dropped first: the controls are handed back before the camera closes.
    let _controls = attach_controls(shared, source.as_ref());
    let (description, src_h, camera_color, capture_info) = {
        let i = source.info();
        (
            i.description.clone(),
            i.height,
            i.color,
            (
                i.width,
                i.height,
                i.format.clone(),
                i.decode,
                i.link.clone(),
            ),
        )
    };
    *lock(&shared.camera_link) = capture_info.4.clone();
    // The colour space and range the user chose win over what the camera states; read again every frame, so a
    // change applies live.
    let effective_color = |user: ColorSettings| ColorSettings {
        space: user.space.or(camera_color.space),
        range: user.range.or(camera_color.range),
    };
    let mut color = effective_color(*lock(&shared.color));

    let mut converter = Converter::new(renderer.clone()).map_err(|e| e.to_string())?;
    let stage = OutputStage::new(renderer.clone()).map_err(|e| e.to_string())?;
    let mut chain = Chain::new();
    chain.set_assets(ctx.assets.clone());
    // The chain version this session has applied; the session starts with none, so the first frame applies it.
    let mut chain_gen = u64::MAX;
    let mut output =
        OutputWriter::new(renderer.clone(), writer.clone()).map_err(|e| e.to_string())?;
    let mut timer = GpuTimer::new(&renderer.gpu.device).ok();

    let mut advertised = (0u32, 0u32, 0u32, true);
    let mut published_any = false;
    let mut was_paused = false;
    let mut window = Window::new(0);
    let mut latency = LatencyTrack::default();
    let mut pipeline = LatencyTrack::default();
    let mut stats = EngineStats {
        source: format!("{description}; color {}", describe_color(&color, src_h)),
        capture_width: capture_info.0,
        capture_height: capture_info.1,
        capture_format: capture_info.2,
        decode: capture_info.3,
        ..EngineStats::default()
    };
    let (mut rendered, mut published) = (0u64, 0u64);
    let mut consecutive_errors = 0u32;
    let mut last_heartbeat = Instant::now();
    let mut first_error: Option<String> = None;

    store_stats(shared, &stats);
    // The camera is open and (for a real camera) the first frame is in: the control thread may call us `Live`.
    let _ = ctx.events.send(SessionEvent {
        gen: ctx.gen,
        msg: SessionMsg::Opened,
    });

    let outcome = loop {
        if stop.load(Ordering::Acquire) {
            break Ok(());
        }
        if removed.load(Ordering::Acquire) {
            break Err(Failure::disconnected(None));
        }
        // Keep the DLL's picture of what we advertise current.
        let out_settings = *lock(&shared.output);
        let (ow, oh, ofps) = resolve_output(&out_settings, source.info());
        if advertised != (ow, oh, ofps, out_settings.extra_sizes) {
            writer.set_advertised(&advertised_list(ow, oh, ofps, out_settings.extra_sizes));
            advertised = (ow, oh, ofps, out_settings.extra_sizes);
            *lock(&shared.advertised) = Some((ow, oh, ofps));
        }
        color = effective_color(*lock(&shared.color));

        // Pause: keep the camera open, read and drop its frames, show the paused slate.
        let paused = shared.paused.load(Ordering::Acquire);
        if paused != was_paused {
            was_paused = paused;
            stats.paused = paused;
            if !paused {
                // Resumed: the writer leaves `Paused` with the first frame published from here on.
                published_any = false;
            }
        }
        if paused {
            writer.set_state(WriterState::Paused);
            match source.next(Duration::from_millis(40)) {
                SourceEvent::Frame(_) | SourceEvent::Timeout => {}
                SourceEvent::Stopped(f) => break Err(f),
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

        // Frames that finished on the GPU while we waited.
        if ctx.readback == ReadbackPolicy::NextFrame {
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
            SourceEvent::Stopped(failure) => break Err(failure),
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
            // The newest chain applies before this frame.
            let wanted = shared.chain_gen.load(Ordering::Acquire);
            if wanted != chain_gen {
                chain_gen = wanted;
                let cfg = lock(&shared.chain).clone();
                chain.apply(renderer, &cfg);
            }
            // The filters that scale to the "base (canvas) resolution" use the output size.
            chain.set_canvas((target.width, target.height));
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
        // Filters that failed this frame were bypassed; each failure is reported once (the chain keeps count).
        for e in chain.take_errors() {
            emit(shared, EngineEvent::FilterError(e));
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
                    break Err(Failure::Fatal(format!(
                        "rendering failed 30 times in a row: {e}"
                    )));
                }
                continue;
            }
        }

        match ctx.readback {
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
        // A pause that arrived while this frame was in flight must not be undone by its first publish.
        if shared.paused.load(Ordering::Acquire) {
            writer.set_state(WriterState::Paused);
        }
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
    store_stats(shared, stats);
    *window = Window::new(captured);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::camera_controls::fake::FakeControls;
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
            deactivate_when_not_showing: false,
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
        // Stopped shows the "paused" slate; only a shutdown closes the file.
        assert_eq!(
            reader.shared().header().writer_state,
            WriterState::Paused as u32
        );
        let s = engine.stats();
        assert!(s.published >= 5 && s.rendered >= s.published);
        engine.shutdown();
        assert_eq!(
            reader.shared().header().writer_state,
            WriterState::Closed as u32
        );
    }

    #[test]
    fn set_chain_applies_live_and_survives_a_stop() {
        let rig = Rig::new("sm-chain.bin", fast());
        let chain = |sat: Option<f64>| match sat {
            Some(s) => crate::fx::parse_chain(&serde_json::json!([
                { "id": "c", "kind": "color_filter", "enabled": true, "params": { "saturation": s } },
                { "id": "u", "kind": "not_a_filter", "enabled": true, "params": {} }
            ]))
            .unwrap(),
            None => Vec::new(),
        };
        // The widest channel spread of the latest published frame: 0 is grey, bars are far above 100.
        let spread = |rig: &Rig| -> Option<u8> {
            let (w, h, _, buf) = latest(&rig.file)?;
            let rgb = nv12_to_rgb(&buf, w, h, w as usize, ColorSpace::Bt709, ColorRange::Limited);
            rgb.chunks(3)
                .map(|p| p.iter().max().unwrap() - p.iter().min().unwrap())
                .max()
        };

        // A chain set while stopped is kept for the start.
        rig.engine.set_chain(chain(Some(-1.0)));
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        rig.wait("grey frames", Duration::from_secs(10), |r| {
            spread(r).is_some_and(|s| s <= 4)
        });

        // Live: the colours come back without a reopen, and go away again.
        rig.engine.set_chain(chain(None));
        rig.wait("colour", Duration::from_secs(10), |r| {
            spread(r).is_some_and(|s| s > 100)
        });
        rig.engine.set_chain(chain(Some(-1.0)));
        rig.wait("grey again", Duration::from_secs(10), |r| {
            spread(r).is_some_and(|s| s <= 4)
        });
        assert_eq!(rig.opens(), 1, "a chain change does not reopen the camera");
        assert_eq!(rig.engine.state(), EngineState::Live);

        // Stopped: the chain is still the engine's, the next start renders it from the first frame.
        rig.engine.stop();
        assert!(rig.engine.wait_stopped(Duration::from_secs(10)));
        rig.engine.set_chain(chain(None));
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        rig.wait("colour after the restart", Duration::from_secs(10), |r| {
            spread(r).is_some_and(|s| s > 100)
        });
        assert!(
            !rig.events
                .lock()
                .unwrap()
                .iter()
                .any(|(_, e)| matches!(e, EngineEvent::FilterError(_))),
            "no filter failed"
        );
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
            deactivate_when_not_showing: false,
        });
        let err = engine.wait_live(Duration::from_secs(10)).unwrap_err();
        assert!(err.contains("not valid"), "{err}");
        assert!(engine.wait_stopped(Duration::from_secs(5)));
        let _ = Read::NoFrame;
    }

    // ---- state machine: a fake camera that can fail, vanish and come back --------------------------------------

    /// Short timers: the product's are 5 s and 3 s.
    fn fast() -> Timings {
        Timings {
            standby_delay: Duration::from_millis(400),
            retry_interval: Duration::from_millis(300),
            tick: Duration::from_millis(20),
            stats_interval: Duration::from_millis(250),
        }
    }

    /// Opens synthetic bars; stands in for the camera.
    struct Fake {
        opens: std::sync::atomic::AtomicU32,
        /// Sources that are open right now (the camera is "busy" while this is above 0).
        alive: Arc<std::sync::atomic::AtomicU32>,
        /// Failures the next opens return, in order.
        failures: Mutex<VecDeque<Failure>>,
        /// The camera is unplugged: open fails and open sources stop.
        gone: Arc<AtomicBool>,
        specs: Mutex<Vec<SourceSpec>>,
        /// The device controls of the fake camera.
        controls: Arc<FakeControls>,
    }

    /// The symbolic link the fake camera has.
    const FAKE_LINK: &str = "\\\\?\\usb#vid_0c45&pid_636d&mi_00#9&3171a958&0&0000#{e5323777-f976-4f5b-9b55-b94699c46e44}\\global";

    struct FakeSource {
        inner: SyntheticInput,
        info: SourceInfo,
        controls: Arc<FakeControls>,
        gone: Arc<AtomicBool>,
        alive: Arc<std::sync::atomic::AtomicU32>,
    }

    impl Drop for FakeSource {
        fn drop(&mut self) {
            self.alive.fetch_sub(1, Ordering::SeqCst);
        }
    }

    impl Source for FakeSource {
        fn info(&self) -> &SourceInfo {
            &self.info
        }
        fn controls(&self) -> Option<Arc<dyn CameraControls>> {
            Some(self.controls.clone())
        }
        fn next(&mut self, timeout: Duration) -> SourceEvent {
            if self.gone.load(Ordering::SeqCst) {
                return SourceEvent::Stopped(Failure::disconnected(None));
            }
            self.inner.next(timeout)
        }
        fn counters(&self) -> (u64, u64) {
            self.inner.counters()
        }
    }

    impl SourceFactory for Fake {
        fn open(
            &self,
            spec: &SourceSpec,
            _output_fps: Option<u32>,
            _gpu: &Arc<GpuDevice>,
        ) -> Result<Box<dyn Source>, Failure> {
            self.opens.fetch_add(1, Ordering::SeqCst);
            self.specs.lock().unwrap().push(spec.clone());
            if let Some(f) = self.failures.lock().unwrap().pop_front() {
                return Err(f);
            }
            if self.gone.load(Ordering::SeqCst) {
                return Err(Failure::Retry(
                    RetryKind::NotFound,
                    "camera not found".into(),
                ));
            }
            let SourceSpec::Synthetic(s) = spec else {
                return Err(Failure::Fatal("the fake opens synthetic sources".into()));
            };
            self.alive.fetch_add(1, Ordering::SeqCst);
            let inner = SyntheticInput::new(*s)?;
            let info = SourceInfo {
                link: Some(FAKE_LINK.into()),
                ..inner.info().clone()
            };
            // A camera that was closed and opened again has forgotten its controls (plan 4.1).
            self.controls.forget();
            Ok(Box::new(FakeSource {
                inner,
                info,
                controls: self.controls.clone(),
                gone: self.gone.clone(),
                alive: self.alive.clone(),
            }))
        }
    }

    struct Rig {
        engine: Engine,
        fake: Arc<Fake>,
        file: MappedFile,
        events: Arc<Mutex<Vec<(Instant, EngineEvent)>>>,
        /// Every state and error event with the file's writer state at the moment the sink got it.
        writer_at: Arc<Mutex<Vec<(EngineEvent, u32)>>>,
        started: Instant,
    }

    impl Rig {
        fn new(name: &str, timings: Timings) -> Rig {
            let gpu = Arc::new(GpuDevice::new(AdapterKind::Warp).expect("WARP"));
            let path = temp_file(name);
            let _ = std::fs::remove_file(&path);
            let writer = Arc::new(Writer::create(&path).unwrap());
            let fake = Arc::new(Fake {
                opens: Default::default(),
                alive: Default::default(),
                failures: Mutex::new(VecDeque::new()),
                gone: Arc::new(AtomicBool::new(false)),
                specs: Mutex::new(Vec::new()),
                controls: crate::camera_controls::fake::camera(),
            });
            let opener = Arc::new(crate::camera_controls::fake::FakeOpener {
                camera: fake.controls.clone(),
                link: FAKE_LINK.into(),
                opened: Default::default(),
                closed: Default::default(),
            });
            let probe = writer.clone();
            let engine = Engine::build(gpu, writer, timings, fake.clone(), opener).unwrap();
            let events: Arc<Mutex<Vec<(Instant, EngineEvent)>>> = Arc::default();
            let writer_at: Arc<Mutex<Vec<(EngineEvent, u32)>>> = Arc::default();
            let (sink, seen) = (events.clone(), writer_at.clone());
            engine.set_event_sink(Arc::new(move |e: EngineEvent| {
                if matches!(e, EngineEvent::State(_) | EngineEvent::Error(_)) {
                    let w = probe.shared().header().writer_state;
                    seen.lock().unwrap().push((e.clone(), w));
                }
                sink.lock().unwrap().push((Instant::now(), e));
            }));
            Rig {
                engine,
                fake,
                file: MappedFile::open(&path, Access::ReadWrite).unwrap(),
                events,
                writer_at,
                started: Instant::now(),
            }
        }

        fn writer_state(&self) -> u32 {
            self.file.shared().header().writer_state
        }

        fn opens(&self) -> u32 {
            self.fake.opens.load(Ordering::SeqCst)
        }

        fn alive(&self) -> u32 {
            self.fake.alive.load(Ordering::SeqCst)
        }

        /// For each state event so far with `state`: its reason and the writer state that was in the file at the
        /// moment the sink got it.
        fn writer_at_state(&self, state: EngineState) -> Vec<(Option<String>, u32)> {
            self.writer_at
                .lock()
                .unwrap()
                .iter()
                .filter_map(|(e, w)| match e {
                    EngineEvent::State(s) if s.state == state => Some((s.reason.clone(), *w)),
                    _ => None,
                })
                .collect()
        }

        /// `(state, reason)` of every state event so far.
        fn states(&self) -> Vec<(EngineState, Option<String>)> {
            self.events
                .lock()
                .unwrap()
                .iter()
                .filter_map(|(_, e)| match e {
                    EngineEvent::State(s) => Some((s.state, s.reason.clone())),
                    _ => None,
                })
                .collect()
        }

        fn wait(&self, what: &str, timeout: Duration, cond: impl Fn(&Rig) -> bool) -> Duration {
            let t = Instant::now();
            while !cond(self) {
                assert!(
                    t.elapsed() < timeout,
                    "timed out after {timeout:?} waiting for {what}: state {:?} reason {:?}, writer {}, states {:?}",
                    self.engine.state(),
                    self.engine.reason(),
                    self.writer_state(),
                    self.states()
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            t.elapsed()
        }

        fn wait_state(&self, state: EngineState, timeout: Duration) -> Duration {
            self.wait(&format!("{state:?}"), timeout, |r| {
                r.engine.state() == state
            })
        }

        /// A consumer attached to the file (what the DLL does while a stream runs), until dropped.
        fn consumer(&self, path_name: &str) -> Consumer {
            Consumer::start(&temp_file(path_name))
        }
    }

    struct Consumer {
        stop: Arc<AtomicBool>,
        join: Option<std::thread::JoinHandle<()>>,
        file: Arc<MappedFile>,
    }

    impl Consumer {
        fn start(path: &std::path::Path) -> Consumer {
            let file = Arc::new(MappedFile::open(path, Access::ReadWrite).unwrap());
            let stop = Arc::new(AtomicBool::new(false));
            file.shared()
                .set_negotiated(&Format::new(320, 180, 30, SUBTYPE_NV12));
            file.shared().set_consumer_active(true);
            let (f, s) = (file.clone(), stop.clone());
            let join = std::thread::spawn(move || {
                while !s.load(Ordering::SeqCst) {
                    f.shared().set_consumer_heartbeat(vcam_protocol::qpc_now());
                    std::thread::sleep(Duration::from_millis(50));
                }
            });
            Consumer {
                stop,
                join: Some(join),
                file,
            }
        }

        fn detach(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(j) = self.join.take() {
                let _ = j.join();
            }
            self.file.shared().set_consumer_active(false);
        }
    }

    impl Drop for Consumer {
        fn drop(&mut self) {
            self.detach();
        }
    }

    fn bars(fps: u32) -> SourceSpec {
        SourceSpec::Synthetic(SyntheticSpec {
            width: 320,
            height: 180,
            fps,
            format: PixelFormat::Nv12,
            color: ColorSettings::default(),
        })
    }

    fn settings(source: SourceSpec, deactivate: bool) -> Settings {
        Settings {
            source,
            output: OutputSettings::default(),
            flip_vertical: false,
            color: ColorSettings::default(),
            readback: ReadbackPolicy::Immediate,
            deactivate_when_not_showing: deactivate,
        }
    }

    const PAUSED: u32 = WriterState::Paused as u32;
    const STARTING: u32 = WriterState::Starting as u32;
    const LIVE: u32 = WriterState::Live as u32;
    const CLOSED: u32 = WriterState::Closed as u32;

    #[test]
    fn start_pause_resume_and_stop() {
        let rig = Rig::new("sm-pause.bin", fast());
        assert_eq!(rig.engine.state(), EngineState::Stopped);
        // an engine that exists but runs nothing shows the paused slate and keeps its heartbeat
        rig.wait("the paused slate", Duration::from_secs(5), |r| {
            r.writer_state() == PAUSED
        });

        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        rig.wait("frames", Duration::from_secs(10), |r| {
            r.writer_state() == LIVE
        });
        assert_eq!(
            rig.opens(),
            1,
            "no standby wait without deactivate_when_not_showing"
        );

        rig.engine.set_pause(true);
        rig.wait("the paused slate", Duration::from_secs(5), |r| {
            r.writer_state() == PAUSED
        });
        assert_eq!(
            rig.engine.state(),
            EngineState::Live,
            "pause is not a state"
        );
        assert_eq!(rig.alive(), 1, "the camera stays open while paused");
        std::thread::sleep(Duration::from_millis(400));
        assert_eq!(
            rig.writer_state(),
            PAUSED,
            "frames must not revive a paused writer"
        );

        rig.engine.set_pause(false);
        rig.wait("video again", Duration::from_secs(10), |r| {
            r.writer_state() == LIVE
        });
        assert_eq!(rig.opens(), 1, "resume does not reopen the camera");

        rig.engine.stop();
        assert!(rig.engine.wait_stopped(Duration::from_secs(10)));
        assert_eq!(rig.alive(), 0, "stop releases the camera");
        rig.wait("the paused slate", Duration::from_secs(5), |r| {
            r.writer_state() == PAUSED
        });
        // Stop clears a pause
        assert!(!rig.engine.paused());

        // start again: video again
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        rig.wait("video", Duration::from_secs(10), |r| {
            r.writer_state() == LIVE
        });

        assert_eq!(
            rig.states(),
            vec![
                (EngineState::Standby, Some("opening".into())),
                (EngineState::Live, None),
                (EngineState::Stopped, None),
                (EngineState::Standby, Some("opening".into())),
                (EngineState::Live, None),
            ]
        );
        let writer_state = rig.file.shared().header().writer_state;
        assert_eq!(writer_state, LIVE);
        rig.engine.close();
        assert_eq!(rig.writer_state(), CLOSED);
    }

    #[test]
    fn pause_before_the_camera_is_open_shows_the_paused_slate() {
        let rig = Rig::new("sm-pause-early.bin", fast());
        rig.engine.start(settings(bars(30), false));
        rig.engine.set_pause(true);
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(rig.writer_state(), PAUSED);
        rig.engine.set_pause(false);
        rig.wait("video", Duration::from_secs(10), |r| {
            r.writer_state() == LIVE
        });
    }

    #[test]
    fn deactivate_follows_the_consumer_and_the_window() {
        let rig = Rig::new("sm-idle.bin", fast());
        let path = "sm-idle.bin";
        // hidden window, no consumer: nothing opens, the slate says "starting" for whoever comes
        rig.engine.start(settings(bars(30), true));
        rig.wait_state(EngineState::Standby, Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(rig.engine.reason().as_deref(), Some("idle"));
        assert_eq!(
            (rig.opens(), rig.alive()),
            (0, 0),
            "the camera was never opened"
        );
        assert_eq!(rig.writer_state(), STARTING);

        // a consumer attaches: Live
        let mut consumer = rig.consumer(path);
        let woke = rig.wait_state(EngineState::Live, Duration::from_secs(5));
        eprintln!("consumer attached -> live in {woke:?}");
        assert_eq!(rig.alive(), 1);

        // it leaves: standby after the delay, camera released
        let left = Instant::now();
        consumer.detach();
        rig.wait_state(EngineState::Standby, Duration::from_secs(5));
        let idle_after = left.elapsed();
        assert!(
            idle_after >= Duration::from_millis(350) && idle_after < Duration::from_secs(3),
            "standby after {idle_after:?}"
        );
        assert_eq!(rig.alive(), 0, "the camera is free in standby");
        assert_eq!(rig.engine.reason().as_deref(), Some("idle"));

        // the window shows: Live
        rig.engine.set_window_visible(true);
        rig.wait_state(EngineState::Live, Duration::from_secs(5));
        // still visible: no standby, however long
        std::thread::sleep(Duration::from_millis(900));
        assert_eq!(rig.engine.state(), EngineState::Live);
        // hidden (or minimized) with a consumer attached: stays live
        let mut again = rig.consumer(path);
        rig.engine.set_window_visible(false);
        std::thread::sleep(Duration::from_millis(900));
        assert_eq!(
            rig.engine.state(),
            EngineState::Live,
            "a consumer keeps it live"
        );
        again.detach();
        rig.wait_state(EngineState::Standby, Duration::from_secs(5));
        assert_eq!(rig.alive(), 0);
        assert_eq!(
            rig.opens(),
            2,
            "one open per wake-up: consumer, then window"
        );

        // a visible window that hides and shows again within the delay never closes the camera
        rig.engine.set_window_visible(true);
        rig.wait_state(EngineState::Live, Duration::from_secs(5));
        rig.engine.set_window_visible(false);
        std::thread::sleep(Duration::from_millis(150));
        rig.engine.set_window_visible(true);
        std::thread::sleep(Duration::from_millis(700));
        assert_eq!(rig.engine.state(), EngineState::Live);
        assert_eq!(rig.opens(), 3);
    }

    #[test]
    fn without_deactivate_a_hidden_window_changes_nothing() {
        let rig = Rig::new("sm-keep.bin", fast());
        rig.engine.set_window_visible(false);
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        std::thread::sleep(Duration::from_millis(1000));
        assert_eq!(rig.engine.state(), EngineState::Live);
        assert_eq!(rig.opens(), 1);
    }

    /// The stored controls of the tests below, as the frontend sends them.
    fn controls_record(gain: i32) -> crate::capture::controls::ControlSnapshot {
        crate::camera_controls::parse_record(Some(&serde_json::json!({
            "id": FAKE_LINK,
            "name": "W4GS",
            "controls": {
                "gain": { "value": gain, "auto": false },
                "exposure": { "value": -3, "auto": false },
                "focus": { "value": 400, "auto": true },
                "brightness": { "value": 25 }
            }
        })))
        .unwrap()
        .unwrap()
    }

    fn applied(gain: i32) -> Vec<String> {
        // Auto flags first (exposure leaves Auto), then the manual values; a control left on Auto is not written
        vec![
            "auto:exposure:false".into(),
            "value:brightness:25".into(),
            format!("value:gain:{gain}"),
            "value:exposure:-3".into(),
        ]
    }

    #[test]
    fn stored_controls_are_applied_after_every_open() {
        let rig = Rig::new("sm-controls.bin", fast());
        let cam = rig.fake.controls.clone();
        let hub = rig.engine.controls().clone();
        hub.set_record(Some(controls_record(40)));
        let wait_applied = |rig: &Rig, want: Vec<String>, what: &str| {
            rig.wait(what, Duration::from_secs(10), |_| cam.log() == want);
        };

        // 1. the first wake-up (the window shows): opened, then the record is applied
        rig.engine.start(settings(bars(30), true));
        rig.wait_state(EngineState::Standby, Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(200));
        assert!(cam.log().is_empty(), "no camera open, nothing written");
        rig.engine.set_window_visible(true);
        rig.wait_state(EngineState::Live, Duration::from_secs(5));
        wait_applied(&rig, applied(40), "the record after the first open");
        // the UI hears of the result
        rig.wait("controls-changed", Duration::from_secs(5), |r| {
            r.events
                .lock()
                .unwrap()
                .iter()
                .any(|(_, e)| matches!(e, EngineEvent::Controls(c) if c.len() == 4))
        });

        // the user changes a value while live: the next open applies that one, not the one that was sent at the start
        let r = hub.set(FAKE_LINK, "gain", 55, false).unwrap();
        assert_eq!(r.value, 55);
        assert_eq!(rig.opens(), 1, "the calls used the open camera");

        // 2. a capture setting changes: the camera reopens
        cam.clear_log();
        rig.engine.set_source(SourceUpdate {
            source: bars(25),
            flip_vertical: false,
            color: ColorSettings::default(),
            deactivate_when_not_showing: true,
        });
        rig.wait("the reopen", Duration::from_secs(5), |r| r.opens() == 2);
        wait_applied(&rig, applied(55), "the record after the format reopen");

        // 3. the camera is unplugged and plugged in again
        rig.fake.gone.store(true, Ordering::SeqCst);
        rig.wait_state(EngineState::Reconnecting, Duration::from_secs(5));
        cam.clear_log();
        rig.fake.gone.store(false, Ordering::SeqCst);
        rig.wait_state(EngineState::Live, Duration::from_secs(5));
        wait_applied(&rig, applied(55), "the record after the reconnect");

        // 4. the window hides, the camera goes to standby, and the next wake-up applies it again
        rig.engine.set_window_visible(false);
        rig.wait_state(EngineState::Standby, Duration::from_secs(5));
        cam.clear_log();
        rig.engine.set_window_visible(true);
        rig.wait_state(EngineState::Live, Duration::from_secs(5));
        wait_applied(&rig, applied(55), "the record after the standby wake-up");
        assert_eq!(rig.opens(), 4);

        // Reset all forgets the record: the next open writes nothing
        hub.reset(FAKE_LINK).unwrap();
        assert!(hub.record().unwrap().is_empty());
        cam.clear_log();
        rig.engine.set_source(SourceUpdate {
            source: bars(20),
            flip_vertical: false,
            color: ColorSettings::default(),
            deactivate_when_not_showing: true,
        });
        rig.wait("the reopen", Duration::from_secs(5), |r| r.opens() == 5);
        rig.wait_state(EngineState::Live, Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(400));
        assert!(cam.log().is_empty(), "{:?}", cam.log());
        assert_eq!(
            cam.get(crate::capture::controls::ControlKey::Gain).value,
            10
        );
    }

    #[test]
    fn the_camera_closes_with_its_controls_and_a_lease_makes_way_for_it() {
        let rig = Rig::new("sm-lease.bin", fast());
        let hub = rig.engine.controls().clone();
        // not running: a call opens the device for a moment
        hub.set(FAKE_LINK, "gain", 70, false).unwrap();
        assert!(hub.has_lease());
        // Start: the lease is closed before the engine opens the camera
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        assert!(!hub.has_lease());
        // and a stop hands the controls back with the camera
        hub.get(FAKE_LINK).unwrap();
        rig.engine.stop();
        assert!(rig.engine.wait_stopped(Duration::from_secs(10)));
        assert_eq!(rig.alive(), 0);
        assert!(hub.poll().is_none(), "no camera, nothing to poll");
        hub.get(FAKE_LINK).unwrap();
        assert!(hub.has_lease());
    }

    #[test]
    fn a_lost_camera_goes_reconnecting_and_comes_back() {
        let rig = Rig::new("sm-lost.bin", fast());
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        rig.wait("video", Duration::from_secs(10), |r| {
            r.writer_state() == LIVE
        });

        rig.fake.gone.store(true, Ordering::SeqCst);
        rig.wait_state(EngineState::Reconnecting, Duration::from_secs(5));
        let reason = rig.engine.reason().unwrap();
        assert!(reason.contains("disconnected"), "{reason}");
        assert_eq!(rig.alive(), 0, "the lost camera is released");
        assert_eq!(
            rig.writer_state(),
            STARTING,
            "the slate says the camera is starting"
        );
        // it keeps trying (the open fails: "not found"), and says so
        let opens_before = rig.opens();
        rig.wait("a retry", Duration::from_secs(5), |r| {
            r.opens() > opens_before
        });
        rig.wait("the new reason", Duration::from_secs(5), |r| {
            r.engine.reason().is_some_and(|x| x.contains("not found"))
        });
        assert_eq!(rig.engine.state(), EngineState::Reconnecting);

        // plugged in again: Live within one retry interval (plus the open)
        rig.fake.gone.store(false, Ordering::SeqCst);
        let back = rig.wait_state(EngineState::Live, Duration::from_secs(5));
        eprintln!("camera back -> live in {back:?}");
        assert!(back < Duration::from_millis(1200), "{back:?}");
        rig.wait("video", Duration::from_secs(10), |r| {
            r.writer_state() == LIVE
        });

        let states = rig.states();
        assert_eq!(
            states[..2],
            [
                (EngineState::Standby, Some("opening".into())),
                (EngineState::Live, None),
            ]
        );
        assert_eq!(states[2].0, EngineState::Reconnecting);
        assert!(states[2].1.as_deref().unwrap().contains("disconnected"));
        assert_eq!(
            states[3],
            (EngineState::Reconnecting, Some("camera not found".into()))
        );
        assert_eq!(states.last(), Some(&(EngineState::Live, None)));
    }

    #[test]
    fn a_camera_in_use_is_retried_until_it_is_free() {
        let rig = Rig::new("sm-inuse.bin", fast());
        for _ in 0..2 {
            rig.fake
                .failures
                .lock()
                .unwrap()
                .push_back(Failure::in_use(Some(HRESULT(0xC00D_3704_u32 as i32))));
        }
        let t = Instant::now();
        rig.engine.start(settings(bars(30), false));
        rig.wait_state(EngineState::Reconnecting, Duration::from_secs(5));
        let reason = rig.engine.reason().unwrap();
        assert!(
            reason.contains("in use") && reason.contains("0xC00D3704"),
            "{reason}"
        );
        rig.engine.wait_live(Duration::from_secs(10)).unwrap();
        // two failed opens, retried every 300 ms
        assert_eq!(rig.opens(), 3);
        assert!(
            t.elapsed() >= Duration::from_millis(550),
            "{:?}",
            t.elapsed()
        );
        assert_eq!(
            rig.states(),
            vec![
                (EngineState::Standby, Some("opening".into())),
                (EngineState::Reconnecting, Some(reason)),
                (EngineState::Live, None),
            ],
            "the repeated failure is not announced twice"
        );
    }

    #[test]
    fn a_new_source_while_reconnecting_retries_at_once() {
        let mut t = fast();
        t.retry_interval = Duration::from_secs(30);
        let rig = Rig::new("sm-retry-now.bin", t);
        rig.fake
            .failures
            .lock()
            .unwrap()
            .push_back(Failure::in_use(None));
        rig.engine.start(settings(bars(30), false));
        rig.wait_state(EngineState::Reconnecting, Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(rig.opens(), 1, "the retry is 30 s away");
        rig.engine.set_source(SourceUpdate {
            source: bars(25),
            flip_vertical: false,
            color: ColorSettings::default(),
            deactivate_when_not_showing: false,
        });
        rig.wait_state(EngineState::Live, Duration::from_secs(5));
        assert_eq!(rig.opens(), 2);
    }

    #[test]
    fn stop_ends_a_reconnect_and_start_goes_on() {
        let rig = Rig::new("sm-stop-reconnect.bin", fast());
        rig.fake.gone.store(true, Ordering::SeqCst);
        rig.engine.start(settings(bars(30), false));
        rig.wait_state(EngineState::Reconnecting, Duration::from_secs(5));
        rig.engine.stop();
        assert!(rig.engine.wait_stopped(Duration::from_secs(5)));
        let opens = rig.opens();
        std::thread::sleep(Duration::from_millis(800));
        assert_eq!(rig.opens(), opens, "no retries after Stop");
        assert_eq!(rig.writer_state(), PAUSED);
    }

    #[test]
    fn reconnecting_without_anyone_watching_gives_up_the_camera() {
        let rig = Rig::new("sm-reconnect-idle.bin", fast());
        rig.fake.gone.store(true, Ordering::SeqCst);
        rig.engine.set_window_visible(true);
        rig.engine.start(settings(bars(30), true));
        rig.wait_state(EngineState::Reconnecting, Duration::from_secs(5));
        rig.engine.set_window_visible(false);
        rig.wait_state(EngineState::Standby, Duration::from_secs(5));
        let opens = rig.opens();
        std::thread::sleep(Duration::from_millis(700));
        assert_eq!(rig.opens(), opens, "standby does not retry");
    }

    #[test]
    fn another_failure_is_fatal() {
        let rig = Rig::new("sm-fatal.bin", fast());
        rig.fake.failures.lock().unwrap().push_back(Failure::Fatal(
            "camera does not list mjpeg 1280x720; it lists: yuy2".into(),
        ));
        rig.engine.start(settings(bars(30), false));
        let err = rig.engine.wait_live(Duration::from_secs(5)).unwrap_err();
        assert!(err.contains("does not list"), "{err}");
        assert!(rig.engine.wait_stopped(Duration::from_secs(5)));
        assert_eq!(rig.engine.last_error().as_deref(), Some(err.as_str()));
        assert_eq!(rig.writer_state(), PAUSED);
        let events: Vec<EngineEvent> = rig
            .events
            .lock()
            .unwrap()
            .iter()
            .map(|e| e.1.clone())
            .collect();
        let error_at = events
            .iter()
            .position(|e| *e == EngineEvent::Error(err.clone()))
            .expect("engine-error");
        let stopped_at = events
            .iter()
            .position(|e| matches!(e, EngineEvent::State(s) if s.state == EngineState::Stopped && s.reason.as_deref() == Some("error")))
            .expect("stopped with reason error");
        assert!(error_at < stopped_at, "the error comes first");
        // a new start works again and clears the error
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(10)).unwrap();
        assert_eq!(rig.engine.last_error(), None);
    }

    /// The writer state is what other apps see ("Camera paused", "starting"); the state event is what the UI shows.
    /// The writer is set first, so no observer sees the new state with the old slate.
    #[test]
    fn the_writer_is_set_before_the_state_that_goes_with_it() {
        // a fatal start: opening (starting), then the error and Stopped (paused)
        let rig = Rig::new("sm-order-fatal.bin", fast());
        rig.fake
            .failures
            .lock()
            .unwrap()
            .push_back(Failure::Fatal("no such mode".into()));
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(5)).unwrap_err();
        assert!(rig.engine.wait_stopped(Duration::from_secs(5)));
        assert_eq!(
            rig.writer_at_state(EngineState::Standby),
            vec![(Some("opening".into()), STARTING)],
            "Standby/opening: the slate says starting"
        );
        assert_eq!(
            rig.writer_at_state(EngineState::Stopped),
            vec![(Some("error".into()), PAUSED)],
            "Stopped/error: the slate says paused"
        );
        let error_seen_with: Vec<u32> = rig
            .writer_at
            .lock()
            .unwrap()
            .iter()
            .filter(|(e, _)| matches!(e, EngineEvent::Error(_)))
            .map(|(_, w)| *w)
            .collect();
        assert_eq!(error_seen_with, vec![PAUSED], "engine-error");

        // Stop while live: the file has gone from live to paused when Stopped is announced
        let rig = Rig::new("sm-order-stop.bin", fast());
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        rig.wait("video", Duration::from_secs(10), |r| {
            r.writer_state() == LIVE
        });
        rig.engine.stop();
        assert!(rig.engine.wait_stopped(Duration::from_secs(5)));
        assert_eq!(
            rig.writer_at_state(EngineState::Stopped),
            vec![(None, PAUSED)]
        );

        // a capture change keeps the last frame on the file: still live when "reopening" is announced
        let rig = Rig::new("sm-order-reopen.bin", fast());
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        rig.wait("video", Duration::from_secs(10), |r| {
            r.writer_state() == LIVE
        });
        rig.engine.set_source(SourceUpdate {
            source: bars(25),
            flip_vertical: false,
            color: ColorSettings::default(),
            deactivate_when_not_showing: false,
        });
        rig.wait("the reopen", Duration::from_secs(5), |r| {
            r.states().len() >= 4
        });
        assert_eq!(
            rig.writer_at_state(EngineState::Standby)[1],
            (Some("reopening".into()), LIVE)
        );

        // a camera lost while live: starting is on the file when Reconnecting is announced
        let rig = Rig::new("sm-order-lost.bin", fast());
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        rig.wait("video", Duration::from_secs(10), |r| {
            r.writer_state() == LIVE
        });
        rig.fake.gone.store(true, Ordering::SeqCst);
        rig.wait_state(EngineState::Reconnecting, Duration::from_secs(5));
        let seen = rig.writer_at_state(EngineState::Reconnecting);
        assert!(!seen.is_empty());
        assert!(
            seen.iter().all(|(_, w)| *w == STARTING),
            "Reconnecting events saw {seen:?}"
        );

        // nobody watching: idle standby says starting for whoever comes
        let rig = Rig::new("sm-order-idle.bin", fast());
        rig.engine.start(settings(bars(30), true));
        rig.wait_state(EngineState::Standby, Duration::from_secs(5));
        assert_eq!(
            rig.writer_at_state(EngineState::Standby),
            vec![(Some("idle".into()), STARTING)]
        );
    }

    #[test]
    fn set_source_reopens_only_for_capture_changes() {
        let rig = Rig::new("sm-source.bin", fast());
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        rig.wait("video", Duration::from_secs(10), |r| {
            r.writer_state() == LIVE
        });
        let update = |source: SourceSpec, flip: bool, space: Option<ColorSpace>| SourceUpdate {
            source,
            flip_vertical: flip,
            color: ColorSettings { space, range: None },
            deactivate_when_not_showing: false,
        };

        // colour and flip apply live
        rig.engine
            .set_source(update(bars(30), true, Some(ColorSpace::Bt601)));
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!((rig.opens(), rig.engine.state()), (1, EngineState::Live));
        assert!(rig.engine.shared.flip.load(Ordering::Relaxed));
        assert_eq!(
            lock(&rig.engine.shared.color).space,
            Some(ColorSpace::Bt601)
        );

        // a capture change reopens, and the last frame stays on the file meanwhile
        let t = Instant::now();
        rig.engine
            .set_source(update(bars(25), true, Some(ColorSpace::Bt601)));
        rig.wait("the reopen", Duration::from_secs(5), |r| r.opens() == 2);
        rig.wait("live again", Duration::from_secs(5), |r| {
            r.engine.state() == EngineState::Live && r.alive() == 1
        });
        eprintln!("reopen took {:?}", t.elapsed());
        assert_eq!(rig.writer_state(), LIVE);
        assert_eq!(
            rig.alive(),
            1,
            "the old camera was released before the new one opened"
        );
        let states = rig.states();
        assert_eq!(states[2], (EngineState::Standby, Some("reopening".into())));
        assert_eq!(states[3], (EngineState::Live, None));
        assert_eq!(rig.fake.specs.lock().unwrap()[1], bars(25));

        // while idle in standby a new source waits for the next wake-up
        let idle = Rig::new("sm-source-idle.bin", fast());
        idle.engine.start(settings(bars(30), true));
        idle.wait_state(EngineState::Standby, Duration::from_secs(5));
        idle.engine.set_source(SourceUpdate {
            source: bars(15),
            flip_vertical: false,
            color: ColorSettings::default(),
            deactivate_when_not_showing: true,
        });
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(idle.opens(), 0);
        idle.engine.set_window_visible(true);
        idle.wait_state(EngineState::Live, Duration::from_secs(5));
        assert_eq!(idle.fake.specs.lock().unwrap()[0], bars(15));
    }

    #[test]
    fn set_output_changes_the_advertised_types_live() {
        let rig = Rig::new("sm-output.bin", fast());
        rig.engine.start(Settings {
            output: OutputSettings {
                width: Some(1920),
                height: Some(1080),
                fps: Some(30),
                ..OutputSettings::default()
            },
            ..settings(bars(30), false)
        });
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        let advertised = |r: &Rig| {
            let h = r.file.shared().header();
            h.advertised[..h.advertised_count as usize]
                .iter()
                .map(|f| (f.width, f.height))
                .collect::<Vec<_>>()
        };
        rig.wait("1080p advertised", Duration::from_secs(5), |r| {
            advertised(r).first() == Some(&(1920, 1080))
        });
        assert!(advertised(&rig).contains(&(1280, 720)) && advertised(&rig).contains(&(640, 360)));

        rig.engine.set_output(OutputSettings {
            width: Some(640),
            height: Some(360),
            fps: Some(30),
            fit: FitMode::Fill,
            extra_sizes: false,
            ..OutputSettings::default()
        });
        rig.wait("640x360 advertised", Duration::from_secs(5), |r| {
            advertised(r).iter().all(|s| *s == (640, 360)) && !advertised(r).is_empty()
        });

        rig.engine.set_output(OutputSettings {
            width: Some(1920),
            height: Some(1080),
            fps: Some(30),
            extra_sizes: false,
            ..OutputSettings::default()
        });
        rig.wait("1080p only", Duration::from_secs(5), |r| {
            advertised(r).iter().all(|s| *s == (1920, 1080)) && !advertised(r).is_empty()
        });
        assert_eq!(rig.engine.state(), EngineState::Live);
        assert_eq!(rig.opens(), 1, "an output change never reopens the camera");
    }

    #[test]
    fn the_idle_engine_advertises_the_configured_output() {
        let rig = Rig::new("sm-output-idle.bin", fast());
        rig.engine.start(Settings {
            output: OutputSettings {
                width: Some(1280),
                height: Some(720),
                fps: Some(30),
                extra_sizes: false,
                ..OutputSettings::default()
            },
            ..settings(bars(30), true)
        });
        rig.wait_state(EngineState::Standby, Duration::from_secs(5));
        rig.wait("the advertised list", Duration::from_secs(5), |r| {
            let h = r.file.shared().header();
            h.advertised_count > 0 && h.advertised[0].width == 1280
        });
        assert_eq!(rig.opens(), 0);
    }

    #[test]
    fn stats_events_come_four_times_a_second_with_every_field() {
        let rig = Rig::new("sm-stats.bin", fast());
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        // let a one-second statistics window complete
        rig.wait("a full window", Duration::from_secs(10), |r| {
            r.engine.stats().publish_fps > 0.0
        });
        let mark = Instant::now();
        let stats_since = |t: Instant, rig: &Rig| -> Vec<StatsEvent> {
            rig.events
                .lock()
                .unwrap()
                .iter()
                .filter(|(at, _)| *at >= t)
                .filter_map(|(_, e)| match e {
                    EngineEvent::Stats(s) => Some(s.clone()),
                    _ => None,
                })
                .collect()
        };
        // The statistics window closes about once a second; the counters are stored with the rates computed from
        // them. Watch two closings: the second window runs from the first closing to the second, so the test can
        // compute the rate the engine should report from its own clock.
        let next_window = |rig: &Rig, after: u64| -> (EngineStats, Instant) {
            let t = Instant::now();
            loop {
                let s = rig.engine.stats();
                if s.published > after {
                    return (s, Instant::now());
                }
                assert!(t.elapsed() < Duration::from_secs(10), "no stats window");
                std::thread::sleep(Duration::from_millis(2));
            }
        };
        let (a, ta) = next_window(&rig, rig.engine.stats().published);
        let (b, tb) = next_window(&rig, a.published);
        // the 4 Hz count is taken over exactly two seconds of the events' own timestamps: a test thread that
        // oversleeps on a busy machine does not add events
        if let Some(rest) = (mark + Duration::from_secs(2)).checked_duration_since(Instant::now()) {
            std::thread::sleep(rest);
        }
        let end = mark + Duration::from_secs(2);
        let none: Vec<StatsEvent> = rig
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|(at, _)| *at >= mark && *at < end)
            .filter_map(|(_, e)| match e {
                EngineEvent::Stats(s) => Some(s.clone()),
                _ => None,
            })
            .collect();
        // 4 per second: 8 in two seconds, give or take the timer
        assert!(
            (7..=9).contains(&none.len()),
            "{} stats events in 2 s",
            none.len()
        );
        // The frame rates are per window over its real elapsed time, so they follow what the machine managed (13 fps
        // for a 30 fps source was seen with every core busy): compare them with the counters and the test's own
        // clock, not with a fixed rate.
        let secs = tb.duration_since(ta).as_secs_f64();
        for (name, reported, frames) in [
            ("capture", b.capture_fps, b.captured - a.captured),
            ("publish", b.publish_fps, b.published - a.published),
        ] {
            let measured = frames as f64 / secs;
            assert!(
                (reported - measured).abs() <= measured * 0.25,
                "{name}: the engine says {reported:.1} fps, {frames} frames in {secs:.2} s are {measured:.1}"
            );
            assert!((3.0..=36.0).contains(&reported), "{name} {reported:.1} fps");
        }
        let s = none.last().unwrap();
        assert_eq!((s.capture.width, s.capture.height), (320, 180));
        assert_eq!(s.capture.format, "nv12");
        assert_eq!(s.capture.decode, "none");
        assert!((3.0..=36.0).contains(&s.capture.fps), "{s:?}");
        assert_eq!((s.output.width, s.output.height), (320, 180));
        assert!((3.0..=36.0).contains(&s.output.fps), "{s:?}");
        assert!(s.latency_p95_ms > 0.0, "{s:?}");
        assert_eq!(s.consumers, 0);
        // every key of plan 5.6 is in the JSON
        let json = serde_json::to_value(s).unwrap();
        for key in [
            "capture",
            "output",
            "gpu_ms",
            "dropped",
            "repeated",
            "latency_p95_ms",
            "consumers",
        ] {
            assert!(json.get(key).is_some(), "missing {key}");
        }
        for key in ["width", "height", "format", "fps", "decode"] {
            assert!(json["capture"].get(key).is_some(), "missing capture.{key}");
        }
        for key in ["width", "height", "fps"] {
            assert!(json["output"].get(key).is_some(), "missing output.{key}");
        }

        // a consumer shows as 1, and as 0 again once it is gone
        let mut consumer = rig.consumer("sm-stats.bin");
        let t = Instant::now();
        rig.wait("consumers = 1", Duration::from_secs(5), |r| {
            stats_since(t, r).last().is_some_and(|s| s.consumers == 1)
        });
        consumer.detach();
        let t = Instant::now();
        rig.wait("consumers = 0", Duration::from_secs(5), |r| {
            stats_since(t, r).last().is_some_and(|s| s.consumers == 0)
        });
        rig.engine.stop();
        assert!(rig.engine.wait_stopped(Duration::from_secs(5)));
        // after Stop one zeroed event, then silence
        std::thread::sleep(Duration::from_millis(100));
        let t = Instant::now();
        std::thread::sleep(Duration::from_millis(700));
        assert!(
            stats_since(t, &rig).is_empty(),
            "stats keep coming while stopped"
        );
        let zero = rig
            .events
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find_map(|(_, e)| match e {
                EngineEvent::Stats(s) => Some(s.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            (zero.capture.width, zero.output.width, zero.consumers),
            (0, 0, 0)
        );
        let _ = rig.started;
    }

    #[test]
    fn failures_carry_the_reasons_the_ui_shows() {
        use windows::core::Error;
        let hr = HRESULT(0xC00D_3704_u32 as i32);
        assert_eq!(
            Failure::from_capture(&CaptureError::InUse(Error::from(hr))),
            Failure::Retry(
                RetryKind::InUse,
                "camera in use by another app (0xC00D3704)".into()
            )
        );
        assert!(matches!(
            Failure::from_capture(&CaptureError::DeviceLost(Error::from(hr))),
            Failure::Retry(RetryKind::Disconnected, r) if r.starts_with("camera disconnected (0x")
        ));
        assert_eq!(
            Failure::from_capture(&CaptureError::NoSuchDevice("x".into())),
            Failure::Retry(RetryKind::NotFound, "camera not found".into())
        );
        assert!(matches!(
            Failure::from_capture(&CaptureError::Timeout("the first frame")),
            Failure::Fatal(m) if m.contains("first frame")
        ));
        assert!(matches!(
            Failure::from_stop(StopReason::DeviceLost(None)),
            Failure::Retry(RetryKind::Disconnected, _)
        ));
        assert!(matches!(
            Failure::from_stop(StopReason::InUse(hr)),
            Failure::Retry(RetryKind::InUse, _)
        ));
        assert!(matches!(
            Failure::from_stop(StopReason::EndOfStream),
            Failure::Fatal(_)
        ));
    }

    #[test]
    fn state_events_serialize_as_plan_5_6() {
        let live = serde_json::to_value(StateEvent {
            state: EngineState::Live,
            reason: None,
        })
        .unwrap();
        assert_eq!(live, serde_json::json!({"state": "live"}));
        let r = serde_json::to_value(StateEvent {
            state: EngineState::Reconnecting,
            reason: Some("camera in use by another app (0xC00D3704)".into()),
        })
        .unwrap();
        assert_eq!(
            r,
            serde_json::json!({"state": "reconnecting", "reason": "camera in use by another app (0xC00D3704)"})
        );
        for (s, n) in [
            (EngineState::Stopped, "stopped"),
            (EngineState::Standby, "standby"),
            (EngineState::Live, "live"),
            (EngineState::Reconnecting, "reconnecting"),
        ] {
            assert_eq!(serde_json::to_value(s).unwrap(), serde_json::json!(n));
            assert_eq!(s.name(), n);
        }
    }
}
