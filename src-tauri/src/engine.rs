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
//! * `Live`: the camera is open and frames flow. The first published frame sets the writer to `Live`; with no app
//!   reading the virtual camera nothing is published and the first frame that is rendered does it (see below).
//! * `Reconnecting`: the camera is lost or in use. A retry every `retry_interval`, and at once when the same device
//!   arrives. Any other failure is fatal: `Stopped` with `engine-error`.
//!
//! Pause is not a state: the camera stays open, the writer shows `Paused`, frames are read and dropped. Every change
//! of the flag is reported (`EngineEvent::Paused`), also the one a Start or a Stop makes.
//!
//! The GPU device is looked at before every camera open: one that was lost while nothing used it (a driver update
//! while the app sat in the tray, the camera off in standby, the engine stopped) is rebuilt first, so the camera is
//! never handed a dead device and its failure never taken for a format or camera problem.
//!
//! Order: the control thread sets the writer's slate (`Paused`, `Starting`) before it changes `Engine::state` and
//! before the event for the new state goes out (`Control::enter_slate`), so an observer of the state or the event
//! never finds the old slate in the file. `Live` is the other way round on purpose: the state says the camera is
//! open, and the writer leaves `Starting` only when the first frame is published (or, with nobody reading the
//! camera, rendered).
//!
//! Frames nobody takes cost nothing past the filter chain: with no app reading the virtual camera a frame is
//! rendered for the preview and not encoded, read back or published (the heartbeat goes on, and the first frame after
//! an app attaches is published at once); for an app that reads at a lower rate than the camera, the frames the
//! virtual camera would drop are not published either ([`Pacer`]).
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
use vcam_protocol::{qpc_to_hns, Format, Writer, WriterState};
use windows::core::HRESULT;
use windows::Win32::Media::MediaFoundation::{MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_YUV_MATRIX};

use crate::auto_controls::runner::{self, AutoStatusEvent};
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
use crate::config::{
    advertised_list, arrival_matches, resolve_camera, CaptureCfg, ResType, VideoFormat,
};
use crate::gpu::convert::{
    is_supported, ColorRange, ColorSettings, ColorSpace, Converter, SourceData, SourceFrame,
};
use crate::gpu::output::{FitMode, OutputParams, OutputStage};
use crate::gpu::pattern;
use crate::fx::assets::{AssetClient, Assets};
use crate::fx::{Chain, FilterCfg, FilterError, FilterStatus, PreviewPoint};
use crate::gpu::{AdapterKind, GpuDevice, GpuError, GpuTimer, Renderer, RgbaTexture};
use crate::output_writer::{target_from, FrameTag, OutputWriter, Published};

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
    /// Windows privacy settings keep desktop apps from the camera.
    Privacy,
    /// The open failed with `E_ACCESSDENIED`: a camera that is in use, or Windows' privacy settings. Only this
    /// failure is ever turned into [`RetryKind::Privacy`] (and only when a switch is off); the banner says "in use"
    /// until then.
    AccessDenied,
    /// The camera opened but delivered no picture in time.
    NoFrames,
    /// The video decoder failed at open or at the first frame. [`DefaultFactory`] retries once with hardware
    /// decoding off; one that gets out is retried like any other generic failure.
    Decoder,
    /// Any other error from the camera or its stream. Retried, but only for [`Timings::error_cap`].
    Error,
}

impl RetryKind {
    /// The `kind` of the `engine-state` event: what the banner keys its text and its button on.
    pub fn name(self) -> &'static str {
        match self {
            RetryKind::InUse | RetryKind::AccessDenied => "in_use",
            RetryKind::Disconnected => "disconnected",
            RetryKind::NotFound => "not_found",
            RetryKind::Privacy => "privacy",
            RetryKind::NoFrames | RetryKind::Decoder | RetryKind::Error => "error",
        }
    }

    /// Failures that may be permanent: they are retried for a while only (`Timings::error_cap`). A camera that is in
    /// use, unplugged or blocked by Windows may stay so for as long as the user likes.
    fn is_generic(self) -> bool {
        matches!(self, RetryKind::NoFrames | RetryKind::Decoder | RetryKind::Error)
    }
}

/// A failure of the camera side. Retryable ones lead to `Reconnecting`, the others are fatal.
#[derive(Clone, Debug, PartialEq)]
pub enum Failure {
    /// `reason` is what the `engine-state` event carries.
    Retry(RetryKind, String),
    /// The message of the `engine-error` event.
    Fatal(String),
    /// The camera does not offer the format asked for; the message names it. [`DefaultFactory`] answers a saved
    /// format with one fallback to the device default, so this ends a start only when that fails too.
    Format(String),
    /// The GPU device was removed or reset (`DXGI_ERROR_DEVICE_REMOVED`, ...): the engine builds a new one.
    GpuLost(String),
}

impl Failure {
    pub fn from_capture(e: &CaptureError) -> Failure {
        Failure::from_capture_at(e, true)
    }

    /// [`Failure::from_capture`] for an open that is (`first_open`) or is not the first of a Start. An error that a
    /// second try cannot change stops the engine at once on the first open only: a camera that worked and fails with
    /// such a code after a reconnect is retried like any other error (for `Timings::error_cap`).
    pub fn from_capture_at(e: &CaptureError, first_open: bool) -> Failure {
        // A lost GPU device first: the decoder and the media type share it, so with it gone any call can fail with
        // any code, and a failure to set the media type is not a format problem (it would trigger the device-default
        // fallback and the user's saved format would be replaced).
        if let Some(hr) = e.hresult().filter(|hr| crate::gpu::is_device_lost_hr(*hr)) {
            return Failure::GpuLost(format!("DXGI device removed ({})", hr_text(hr)));
        }
        match e {
            CaptureError::InUse(err) => Failure::in_use(Some(err.code())),
            CaptureError::DeviceLost(err) => Failure::disconnected(Some(err.code())),
            CaptureError::NoSuchDevice(_) => {
                Failure::Retry(RetryKind::NotFound, "camera not found".into())
            }
            CaptureError::FormatUnavailable { .. } => Failure::Format(e.to_string()),
            CaptureError::Timeout(_) => Failure::Retry(
                RetryKind::NoFrames,
                "the camera did not deliver a picture in time".into(),
            ),
            CaptureError::Api { what, error } => {
                let hr = error.code();
                if crate::capture::is_decoder_failure(hr) {
                    Failure::Retry(RetryKind::Decoder, e.to_string())
                } else if what.starts_with("SetCurrentMediaType") {
                    // The camera refused the type that was picked from its own list.
                    Failure::Format(e.to_string())
                } else if first_open && crate::capture::is_deterministic_failure(hr) {
                    // A second try cannot change it: stop at once and name the error.
                    Failure::Fatal(e.to_string())
                } else {
                    Failure::Retry(RetryKind::Error, e.to_string())
                }
            }
            CaptureError::OwnVirtualCamera => Failure::Fatal(e.to_string()),
        }
    }

    /// A stream that stopped for a reason other than "device lost" or "in use" (a stream error, the end of the
    /// stream) is retried like a lost camera: it is often a hiccup of the driver or of the camera service.
    pub fn from_stop(r: StopReason) -> Failure {
        match r {
            StopReason::DeviceLost(hr) => Failure::disconnected(hr),
            StopReason::InUse(hr) => Failure::in_use(Some(hr)),
            StopReason::Error(hr) if crate::gpu::is_device_lost_hr(hr) => {
                Failure::GpuLost(format!("DXGI device removed ({})", hr_text(hr)))
            }
            StopReason::Error(hr) => Failure::Retry(
                RetryKind::Error,
                format!("the camera stream failed ({})", hr_text(hr)),
            ),
            StopReason::EndOfStream | StopReason::Closed => {
                Failure::Retry(RetryKind::Error, "the camera stream ended".into())
            }
        }
    }

    /// Windows' privacy settings keep desktop apps from the camera (`blocked`: which switch).
    pub fn privacy(blocked: crate::capture::privacy::Blocked) -> Failure {
        Failure::Retry(RetryKind::Privacy, crate::capture::privacy::reason(blocked))
    }

    pub fn in_use(hr: Option<HRESULT>) -> Failure {
        let denied = hr == Some(windows::Win32::Foundation::E_ACCESSDENIED);
        Failure::Retry(
            if denied { RetryKind::AccessDenied } else { RetryKind::InUse },
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
            Failure::Retry(_, r) | Failure::Fatal(r) | Failure::Format(r) | Failure::GpuLost(r) => {
                f.write_str(r)
            }
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

/// Called on the render thread once per frame with the picture the preview shows: the finished output frame (RGBA,
/// output size) after the output stage, or, while a [`PreviewPoint`] is set, the picture at that point (its own size).
/// The preview copies and downsizes it there; it must not keep the texture past the call.
pub trait FrameTap: Send {
    fn on_frame(&mut self, renderer: &Renderer, frame: &RgbaTexture, capture_qpc: u64);

    /// Called once per rendered frame after the engine's own readback of it. `wait` says the engine may block on the
    /// GPU for this frame (its readback just did, or nothing is read back): a tap that queued a copy in `on_frame`
    /// maps it here, so what it shows is the frame the apps get, not the one before.
    fn frame_done(&mut self, _wait: bool) {}

    /// The engine built a new GPU device (the old one was lost): drop every resource made on the old one. The next
    /// `on_frame` gets a renderer of the new device.
    fn device_changed(&mut self) {}
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
    /// Where the preview takes its picture ([`PreviewPoint`]). Ordered with [`Msg::SetChain`]: a point that names a
    /// filter of the chain sent just before it is valid when it arrives. It never changes what apps get.
    SetPreviewPoint(PreviewPoint),
    /// Try the camera again now instead of at the next retry (the app window got the focus back: the user may have
    /// changed Windows' privacy settings or closed the other app). Only does something while `Reconnecting`.
    RetryNow,
    /// A camera arrived or left, as the configuration manager's watcher would report it. The engine hears of real
    /// ones from its own watcher; tests and the example scripts use this to stand in for it.
    Hotplug(HotplugEvent),
    /// What the engine is doing, answered by the control thread after every message that was queued before this one
    /// ([`Engine::resume_state`]).
    Snapshot(Sender<Option<ResumeState>>),
    /// Stop and end the control thread.
    Shutdown,
}

/// What an engine was doing, to bring another one to the same state ([`Engine::resume_state`]).
#[derive(Clone, Debug)]
pub struct ResumeState {
    pub settings: Settings,
    pub paused: bool,
    pub chain: Vec<FilterCfg>,
    /// The stored device controls that are re-applied after every open.
    pub record: Option<crate::capture::controls::ControlSnapshot>,
}

/// The pending answer to [`Engine::ask_state`].
pub struct StateAnswer(Option<Receiver<Option<ResumeState>>>);

impl StateAnswer {
    /// The engine's state, `Some(None)` for a stopped one; `None` when the control thread is gone and cannot say.
    pub fn wait(self) -> Option<Option<ResumeState>> {
        self.0?.recv().ok()
    }
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
    /// The frame rate of the media type that is open: what the camera was asked for, and what its own auto exposure
    /// may keep it from (`capture_fps` is what it delivers).
    pub capture_rate: f64,
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
    /// The longest pass of the render loop in the last second (taking a frame to the end of its pass, the statistics
    /// work included), milliseconds: what a stall on the render thread looks like. 0 while paused.
    pub worst_frame_ms: f64,
    /// What closing the last one-second window cost the render thread (the latency percentiles), milliseconds.
    pub stats_ms: f64,
    /// Publishes that came more than half a frame period of the app later, against the frames' capture times, than
    /// the one before: the virtual camera may repeat a frame for each (a stall of the render thread, a slow GPU, a
    /// busy machine). A camera that is itself slow, an app that attaches, a pause or a format change do not count.
    pub late_publishes: u64,
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
    /// The same percentiles over the whole run (a histogram with 0.05 ms bins, the maximum exact).
    pub total_latency_p50_ms: f64,
    pub total_latency_p95_ms: f64,
    pub total_latency_max_ms: f64,
    pub total_pipeline_p95_ms: f64,
    /// Frames rendered but not encoded, read back or published because no app was reading the virtual camera (they
    /// still feed the preview).
    pub skipped_no_consumer: u64,
    /// Frames rendered but not published because the consumer reads at a lower rate than the camera and the
    /// virtual camera would have dropped them ([`Pacer`]).
    pub skipped_rate: u64,
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
    /// What kind of problem `reason` describes, for the banner's text and button: `in_use`, `disconnected`,
    /// `not_found`, `privacy` or `error` while `reconnecting`; `format` when a start failed for want of a format.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<&'static str>,
    /// With `kind` `in_use`: the programs that use a camera now (`["Discord.exe"]`: file names of the executables,
    /// readable names for packaged apps), most recently started first, Better Cam excluded. Missing when none could be
    /// named; the window then says "another app". `reason` has the same names in a plain sentence.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub apps: Option<Vec<String>>,
    /// `apps` may include a program that only reads Better Cam's virtual camera (the record does not say which camera
    /// a program uses): the window says "may be in use by".
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub apps_maybe: bool,
}

impl StateEvent {
    pub fn new(state: EngineState, reason: Option<String>, kind: Option<&'static str>) -> StateEvent {
        StateEvent {
            state,
            reason,
            kind,
            apps: None,
            apps_maybe: false,
        }
    }
}

/// `format-fallback`: the saved format is not on the camera; the engine opened the device default instead and now
/// runs with it (the UI's fields follow).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct FormatFallbackEvent {
    /// What was asked, for the toast: `1920×1080 YUY2 60 fps`.
    pub asked: String,
    /// What is used: `1920×1080 MJPEG 60 fps`.
    pub used: String,
    /// The picture size and frame interval (100 ns units) that are used, for the window's fields.
    pub width: u32,
    pub height: u32,
    pub interval: i64,
    /// The camera's native format that is used (`mjpeg`, `nv12`, ...), for the field that names it. The window keeps
    /// its saved settings: this is what runs until the next Start.
    pub format: String,
}

/// `engine-paused`: pause is not a state, so it has its own report. Sent whenever the pause flag changes, also while
/// the engine is stopped or stopping (a Start or a Stop clears it), but never when the engine is released for a
/// virtual camera change: the pause then travels with [`ResumeState`] and the new engine reports it again.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct PausedEvent {
    pub paused: bool,
}

/// `stats.capture` of the `stats` event.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct CaptureStatsEvent {
    pub width: u32,
    pub height: u32,
    pub format: String,
    pub fps: f64,
    /// The frame rate of the media type that is open (0 while not live): the rate the camera is asked for, whatever
    /// the settings say (Device default has none), against `fps`, the rate it delivers.
    pub rate: f64,
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

/// The `stats` event payload (plan 5.6). Sent while the window is visible and when a number changed: about once a
/// second while live (the figures are refreshed once a second), and once after a state change.
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
    /// The camera's controls (`controls-changed`): when a control that is on Auto moved, once when the window becomes
    /// visible, and after a re-apply. Not while the window is hidden.
    Controls(Vec<ControlInfoDto>),
    /// A filter failed to render and is bypassed (`filter-error`, plan 5.6). Sent once per failure, not per frame.
    /// The asset service (`fx::assets`) uses the same event for a file that could not be loaded
    /// (`could not load <file>: <reason>`), and with an **empty `message`** to clear the card's line after a good
    /// load or when the path was cleared.
    FilterError(FilterError),
    /// A line for a filter's card that is not an error (`filter-status`): "Loading model…" while NVIDIA's model loads.
    /// An empty `message` clears the line.
    FilterStatus(FilterStatus),
    /// The camera does not offer the saved format; the device default is in use (`format-fallback`).
    FormatFallback(FormatFallbackEvent),
    /// The pause flag changed (`engine-paused`).
    Paused(PausedEvent),
    /// What Better Cam auto is doing for the camera's controls (`auto-status`, docs/AUTO-CONTROLS.md 5.4): on every
    /// change of state, at most twice a second while the numbers move. Sent whether or not the window is visible.
    AutoStatus(AutoStatusEvent),
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
    /// Between looks at the numbers for a `stats` event (one goes out only when they changed, see
    /// [`Control::emit_stats`]).
    pub stats_interval: Duration,
    /// Failures that may be permanent (a stream error, a camera that opens but sends nothing) are retried for this
    /// long, counted from the first one; then the engine stops with an error. A camera that is in use, unplugged
    /// or blocked by Windows is waited for without limit.
    pub error_cap: Duration,
    /// A camera that stayed up this long before it failed starts a new count for `error_cap`.
    pub stable_after: Duration,
    /// Device losses closer together than this belong to one run; three in one run stop the engine.
    pub gpu_failure_window: Duration,
    /// After a device loss the last frame stays on the file for this long; a recovery that takes longer shows the
    /// "starting" slate (plan 4.9: 1 s).
    pub gpu_slate_delay: Duration,
    /// A GPU device rebuild that takes longer than this counts as a failed rebuild; its helper thread is left behind.
    pub gpu_rebuild_timeout: Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Timings {
            standby_delay: Duration::from_secs(5),
            retry_interval: Duration::from_secs(3),
            tick: Duration::from_millis(50),
            stats_interval: Duration::from_millis(250),
            error_cap: Duration::from_secs(30),
            stable_after: Duration::from_secs(10),
            gpu_failure_window: Duration::from_secs(30),
            gpu_slate_delay: Duration::from_secs(1),
            gpu_rebuild_timeout: Duration::from_secs(10),
        }
    }
}

/// Device losses in one run that stop the engine.
const GPU_LOSSES_MAX: u32 = 3;

/// After the camera we wait for arrives, this many retries come every [`ARRIVAL_RETRY_INTERVAL`] at most: Windows
/// announces a UVC camera about five seconds before Media Foundation lists it (measured on the W4GS), and the camera
/// should be live a moment after it can be.
const ARRIVAL_RETRIES: u32 = 16;
const ARRIVAL_RETRY_INTERVAL: Duration = Duration::from_millis(500);

/// How long [`Control::end_session`] waits for the render thread to end before it leaves the thread behind (it has
/// seen the stop flag already, so it ends by itself when the call it is stuck in returns). Opening a camera takes 7 to
/// 14 s in the worst case (decoder retry, format fallback, 5 s for the first frame each), and a control write that hangs
/// holds the thread longer: this only bounds a driver that never answers.
const SESSION_END_CAP: Duration = Duration::from_secs(30);

/// The cap of [`Control::end_session`] when the app is quitting ([`Engine::close_for_exit`]): the process exit frees
/// the camera, so a driver that holds the render thread for longer than this must not hold up the exit. (A change of
/// the virtual camera keeps [`SESSION_END_CAP`]: a thread that is left behind keeps `frames.bin` mapped, which the
/// helper that replaces the file cannot have.)
pub(crate) const SHUTDOWN_END_CAP: Duration = Duration::from_secs(3);

/// A camera that sends no picture for [`silent_slate_after`] shows the "starting" slate on the file; after
/// [`silent_reconnect_after`] without a frame its session ends with a retry (the engine reconnects).
const SILENT_SLATE_MIN: Duration = Duration::from_secs(1);
const SILENT_RECONNECT_MIN: Duration = Duration::from_secs(5);

/// How many of the latest gaps between two frames [`FrameGaps`] looks at.
const FRAME_GAPS_KEPT: usize = 8;

/// The gaps between the camera's latest frames, measured on arrival. A camera at a long exposure sends a frame every
/// second or more whatever its nominal rate says, and a gap that long is not silence.
#[derive(Default)]
struct FrameGaps {
    gaps: [Duration; FRAME_GAPS_KEPT],
    next: usize,
}

impl FrameGaps {
    fn record(&mut self, gap: Duration) {
        self.gaps[self.next] = gap;
        self.next = (self.next + 1) % FRAME_GAPS_KEPT;
    }

    /// The largest of the latest gaps (zero before any frame).
    fn largest(&self) -> Duration {
        self.gaps.iter().copied().max().unwrap_or_default()
    }
}

/// How long a live camera may send nothing before the file stops showing its last picture: a second, three frame
/// periods of a camera that is slower than that, or two and a half of the longest gap it was seen to keep.
fn silent_slate_after(fps: f64, observed: Duration) -> Duration {
    let nominal = if fps.is_finite() && fps > 0.0 {
        Duration::from_secs_f64(3.0 / fps)
    } else {
        Duration::ZERO
    };
    SILENT_SLATE_MIN.max(nominal).max(observed.mul_f64(2.5))
}

/// How long the camera may send nothing before its session ends so that the engine reconnects: at least twice the time
/// it took to show the slate, so that a camera that is slow is slated for a while before it is given up.
fn silent_reconnect_after(slate_after: Duration) -> Duration {
    SILENT_RECONNECT_MIN.max(slate_after * 2)
}

/// How often [`Control::end_session`] looks whether the render thread has ended (and beats the file's heartbeat).
const SESSION_END_POLL: Duration = Duration::from_millis(5);

/// How often the engine looks whether `frames.bin` is still the file it has mapped ([`Control::check_frame_file`]).
const FILE_CHECK_INTERVAL: Duration = Duration::from_secs(3);

/// The pause between the two looks at the frame file that a Start makes before it believes "replaced".
const FILE_RECHECK_DELAY: Duration = Duration::from_millis(150);

/// The error when the frame file was replaced or deleted under the running engine. Names the Repair, and holds the
/// words the window clears the error on after one (`runVcam` in `src/store.ts`).
const FRAME_FILE_REPLACED: &str = "Better Cam's frame file was replaced or deleted, so the virtual camera is not installed correctly and apps get no picture. Choose Repair in the virtual camera menu of the title bar.";

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
    /// The app window is visible (a copy of the control thread's flag for the controls poller).
    window_visible: AtomicBool,
    /// Latency samples the render thread adds to its statistics on its next pass ([`Engine::prefill_latency`]).
    prefill_latency: AtomicU64,
    tap: Mutex<Option<Box<dyn FrameTap>>>,
    /// The newest chain and its version: the render thread applies it when the version changed.
    chain: Mutex<Arc<Vec<FilterCfg>>>,
    chain_gen: AtomicU64,
    /// Where the preview takes its picture and its version (UX-PASS L1). The render thread reads the version before it
    /// reads the chain's, so a point that names a filter of a chain sent just before it never sees the older chain.
    preview_point: Mutex<PreviewPoint>,
    preview_point_gen: AtomicU64,
    sink: Mutex<Option<Arc<dyn EventSink>>>,
    /// Symbolic link of the open camera (for hot-plug removal), empty while none is open.
    camera_link: Mutex<Option<String>>,
    /// What the file advertises, `(width, height, fps, extra_sizes)`, and the lock that orders the writers of the list:
    /// the render thread (the real picture size) and the control thread (the configured output while no camera
    /// delivers). The list is written with this held, so the file and this record never disagree, and whoever comes
    /// later sees what the earlier one wrote. Also what a consumer that attaches while the camera is closed gets.
    advertised: Mutex<Option<(u32, u32, u32, bool)>>,
    /// The session (its `gen`) whose first frame has been published. The render thread sets it with the `Live`
    /// writer state; the control thread's slate after a device loss looks at it, so a slate can never replace the
    /// picture of a session that is already live.
    live_gen: Mutex<u64>,
    /// Device controls: the open camera's, the stored record, the lease for calls while no camera is open.
    hub: Arc<ControlsHub>,
    /// What the engine is running (a copy of the control thread's settings; `None` while stopped), so the owner can
    /// bring the engine back to it after releasing it ([`Engine::resume_state`]).
    running: Mutex<Option<Settings>>,
    /// Counts the GPU devices the engine has built: 1 for the first, one more after every device loss.
    gpu_generation: AtomicU64,
    /// The controller wants the camera picture measured before the filter chain ([`Engine::set_metering`]).
    metering: AtomicBool,
    /// The newest measurement the render thread made ([`Engine::meter`]).
    meter: crate::meter::Mailbox,
    /// What the controller reads besides the samples ([`Engine::session_facts`]); the render loop keeps it current.
    /// The cell of the newest session: each session writes its own (a render thread that was left behind cannot
    /// reset the facts of the session that followed it when it ends), and registers it here unless it was abandoned.
    facts: Mutex<Arc<Mutex<crate::meter::SessionFacts>>>,
    /// The app is quitting ([`Engine::close_for_exit`]): the end of a session is waited for [`SHUTDOWN_END_CAP`] at most.
    exiting: AtomicBool,
    /// When the quit is over, whatever is still running ([`Engine::close_for_exit`] sets it, [`SHUTDOWN_END_CAP`] from
    /// the call): every wait of the quit, the control thread's included, ends by then, and what is left is left behind.
    exit_deadline: Mutex<Option<Instant>>,
    /// Test hook: frames that fail with `DXGI_ERROR_DEVICE_REMOVED` before they render. The first one also marks the
    /// current device removed, as a real loss would.
    #[cfg(test)]
    gpu_faults: std::sync::atomic::AtomicU32,
    /// Test hook: how long building a new GPU device takes on top of the real work.
    #[cfg(test)]
    rebuild_delay: Mutex<Duration>,
    /// Test hook: [`SESSION_END_CAP`] for a test.
    #[cfg(test)]
    session_end_cap: Mutex<Option<Duration>>,
    /// Test hook: [`FILE_CHECK_INTERVAL`] for a test.
    #[cfg(test)]
    file_check_interval: Mutex<Option<Duration>>,
    /// Test hook: how long the render thread waits before it takes the lock to go live (a first frame that comes late).
    #[cfg(test)]
    go_live_delay: Mutex<Duration>,
    /// Test hook: how long the render thread waits before it writes the `Paused` slate (a thread that is slow to get
    /// there).
    #[cfg(test)]
    pause_write_delay: Mutex<Duration>,
    /// Test hook: while set, the controls poller does not return to its loop (a read of the controls that hangs in the
    /// driver) and does not look at its stop flag.
    #[cfg(test)]
    poller_stall: AtomicBool,
    /// Test hook: how many looks at the frame file ([`Control::check_frame_file`]) were made, how many of them said
    /// "replaced", and what runs before each look (with the number of the look, from 1).
    #[cfg(test)]
    file_looks: std::sync::atomic::AtomicU32,
    #[cfg(test)]
    file_looks_replaced: std::sync::atomic::AtomicU32,
    #[cfg(test)]
    #[allow(clippy::type_complexity)] // a one-use test hook; a type alias would only move the same type elsewhere
    before_file_look: Mutex<Option<Box<dyn FnMut(u32) + Send>>>,
    /// Test hook: the control thread takes this at the top of every pass, so a test that holds it keeps messages
    /// queued and unprocessed.
    #[cfg(test)]
    control_gate: Mutex<()>,
    /// Test hook: how many [`Msg::Snapshot`]s were sent.
    #[cfg(test)]
    snapshots: std::sync::atomic::AtomicU32,
    /// Test hook: runs in [`Engine::close`] after the writer of a control thread that was left behind is retired and
    /// before `Closed` is written (the moment such a thread wakes and writes its slate).
    #[cfg(test)]
    after_retire: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// How much longer the end of a session that began to be waited for at `started` is waited for: what is left of
/// [`SESSION_END_CAP`], and never more than what is left of the quit once the app is quitting
/// ([`Shared::exit_deadline`]). `ZERO` means: leave it behind now.
fn end_cap(shared: &Shared, started: Instant) -> Duration {
    #[allow(unused_mut)]
    let mut cap = SESSION_END_CAP;
    #[cfg(test)]
    if let Some(test_cap) = *lock(&shared.session_end_cap) {
        cap = test_cap;
    }
    let mut left = cap.saturating_sub(started.elapsed());
    if let Some(deadline) = *lock(&shared.exit_deadline) {
        left = left.min(deadline.saturating_duration_since(Instant::now()));
    }
    left
}

/// How long a quit may take from the call of [`Engine::close_for_exit`]: [`SHUTDOWN_END_CAP`] (a test may shorten it with
/// its `session_end_cap`).
fn exit_budget(shared: &Shared) -> Duration {
    #[cfg(test)]
    if let Some(cap) = *lock(&shared.session_end_cap) {
        return cap.min(SHUTDOWN_END_CAP);
    }
    let _ = shared;
    SHUTDOWN_END_CAP
}

/// Drop `value` on a helper thread, for a value whose drop can wait (a thread that is joined with a cap). When no
/// thread can be started the drop is made here.
fn drop_off_thread<T: Send + 'static>(value: T) {
    let _ = std::thread::Builder::new()
        .name("drop".into())
        .spawn(move || drop(value));
}

fn emit(shared: &Shared, event: EngineEvent) {
    let sink = lock(&shared.sink).clone();
    if let Some(s) = sink {
        s.emit(event);
    }
}

/// Keep the advertised media types current while no camera delivers, so a consumer that attaches then (and wakes the
/// engine) sees the configured size: an explicit output size, else what the last session advertised. Called on the
/// control thread; the render thread writes the list of the real picture with [`advertise_session`].
fn advertise_configured(shared: &Shared, writer: &Writer) {
    let o = *lock(&shared.output);
    let mut last = lock(&shared.advertised);
    let (w, h, fps) = match (o.width, o.height, *last) {
        (Some(w), Some(h), l) => (w, h, o.fps.or(l.map(|l| l.2)).unwrap_or(30)),
        (_, _, Some((w, h, f, _))) => (w, h, o.fps.unwrap_or(f)),
        _ => return,
    };
    write_advertised(writer, &mut last, (w, h, fps, o.extra_sizes));
}

/// The render thread's list: the output size and rate in effect and whether the extra sizes go along. Rewrites the
/// file when it holds anything else, whoever wrote that.
fn advertise_session(shared: &Shared, writer: &Writer, want: (u32, u32, u32, bool)) {
    write_advertised(writer, &mut lock(&shared.advertised), want);
}

/// Write the list unless the file already holds it. The caller holds the `advertised` lock, which orders the writers.
fn write_advertised(
    writer: &Writer,
    record: &mut Option<(u32, u32, u32, bool)>,
    want: (u32, u32, u32, bool),
) {
    if *record != Some(want) {
        writer.set_advertised(&advertised_list(want.0, want.1, want.2, want.3));
        // A retired writer (a render thread that was left behind) wrote nothing, so the record says nothing is known
        // and the next writer writes its list again. (A write that went through just before the retirement leaves a
        // record that differs from the file, which the next writer corrects the same way.)
        *record = if writer.is_retired() { None } else { Some(want) };
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
    /// The `controls auto` thread: runs Better Cam auto for the camera's controls (docs/AUTO-CONTROLS.md 2.8).
    runner: Mutex<Option<(Arc<AtomicBool>, JoinHandle<()>)>>,
    /// [`Engine::close`] has run (or is running): a second call (the `Drop` after an explicit close) returns at once.
    closed: AtomicBool,
}

impl Engine {
    /// The real thing: the default hardware adapter and `%ProgramData%\BetterCam\frames.bin`.
    pub fn new() -> Result<Engine, String> {
        let gpu =
            GpuDevice::new(AdapterKind::Hardware).map_err(|e| format!("D3D11 device: {e}"))?;
        let writer = Writer::create_default().map_err(|e| format!("frames.bin: {e}"))?;
        eprintln!("[engine] frame event: {:?}", writer.frame_event_status());
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
        crate::meter::warm_up(&renderer).map_err(|e| e.to_string())?;
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
            window_visible: AtomicBool::new(false),
            prefill_latency: AtomicU64::new(0),
            tap: Mutex::new(None),
            chain: Mutex::new(Arc::new(Vec::new())),
            chain_gen: AtomicU64::new(0),
            preview_point: Mutex::new(PreviewPoint::Output),
            preview_point_gen: AtomicU64::new(0),
            sink: Mutex::new(None),
            camera_link: Mutex::new(None),
            advertised: Mutex::new(None),
            live_gen: Mutex::new(0),
            hub: Arc::new(ControlsHub::new(opener)),
            running: Mutex::new(None),
            gpu_generation: AtomicU64::new(1),
            metering: AtomicBool::new(false),
            meter: crate::meter::Mailbox::default(),
            facts: Mutex::new(Arc::new(Mutex::new(crate::meter::SessionFacts::CLOSED))),
            exiting: AtomicBool::new(false),
            exit_deadline: Mutex::new(None),
            #[cfg(test)]
            gpu_faults: Default::default(),
            #[cfg(test)]
            rebuild_delay: Mutex::new(Duration::ZERO),
            #[cfg(test)]
            session_end_cap: Mutex::new(None),
            #[cfg(test)]
            file_check_interval: Mutex::new(None),
            #[cfg(test)]
            go_live_delay: Mutex::new(Duration::ZERO),
            #[cfg(test)]
            pause_write_delay: Mutex::new(Duration::ZERO),
            #[cfg(test)]
            poller_stall: AtomicBool::new(false),
            #[cfg(test)]
            file_looks: Default::default(),
            #[cfg(test)]
            file_looks_replaced: Default::default(),
            #[cfg(test)]
            before_file_look: Mutex::new(None),
            #[cfg(test)]
            control_gate: Mutex::new(()),
            #[cfg(test)]
            snapshots: Default::default(),
            #[cfg(test)]
            after_retire: Mutex::new(None),
        });
        let poller = spawn_poller(shared.clone()).map_err(|e| e.to_string())?;
        let auto = runner::spawn(shared.clone()).map_err(|e| e.to_string())?;
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
                backlog: VecDeque::new(),
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
                last_file_check: Instant::now(),
                file_replaced_seen: false,
                last_stats: Instant::now(),
                last_stats_sent: None,
                hotplug: None,
                error_streak: None,
                opened_at: None,
                gpu_dirty: false,
                gpu_losses: 0,
                gpu_last_loss: None,
                reopening_since: None,
                arrival_burst: 0,
                first_open: true,
                paused_reported: false,
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
            runner: Mutex::new(Some(auto)),
            closed: AtomicBool::new(false),
        })
    }

    /// The device controls (plan 5.5): calls for the camera the engine has open, or a lease on one it has not, and
    /// the record that is re-applied after every open.
    pub fn controls(&self) -> &Arc<ControlsHub> {
        &self.shared.hub
    }

    /// Switch the meter (docs/AUTO-CONTROLS.md 2.4): while on, the render thread measures the camera picture before
    /// the filter chain, at most every 66 ms, and puts the result in [`Engine::meter`]. The controller sets it when
    /// any control of the attached camera is in Better Cam auto. Turning it off drops what is in flight and in the
    /// mailbox (on the next frame).
    pub fn set_metering(&self, on: bool) {
        self.shared.metering.store(on, Ordering::Release);
    }

    /// Whether the meter is switched on.
    pub fn metering(&self) -> bool {
        self.shared.metering.load(Ordering::Acquire)
    }

    /// The mailbox of the meter: one slot, the newest [`MeterSample`](crate::meter::MeterSample) wins.
    pub fn meter(&self) -> &crate::meter::Mailbox {
        &self.shared.meter
    }

    /// What the controller reads each tick besides the samples: [`SessionFacts::CLOSED`](crate::meter::SessionFacts)
    /// while no camera session runs.
    pub fn session_facts(&self) -> crate::meter::SessionFacts {
        let cell = lock(&self.shared.facts).clone();
        let facts = *lock(&cell);
        facts
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

    /// `Msg::SetPreviewPoint`: the preview shows the picture at `point` from the next frame. The engine puts it back
    /// to [`PreviewPoint::Output`] by itself when the filter it names is no longer in the chain, and when the window
    /// is hidden.
    pub fn set_preview_point(&self, point: PreviewPoint) {
        self.send(Msg::SetPreviewPoint(point));
    }

    /// Where the preview takes its picture now (tests, diagnostics).
    pub fn preview_point(&self) -> PreviewPoint {
        lock(&self.shared.preview_point).clone()
    }

    /// Measurement hook for the examples: the running render thread adds `samples` made-up latencies (1 to 6 ms) to its
    /// whole-run statistics on its next pass, as an hour of frames would have, so that a stall in how those are kept
    /// shows in [`EngineStats::worst_frame_ms`] without waiting for it.
    pub fn prefill_latency(&self, samples: u64) {
        self.shared.prefill_latency.store(samples, Ordering::Release);
    }

    /// `Msg::SetWindowVisible`.
    pub fn set_window_visible(&self, visible: bool) {
        self.send(Msg::SetWindowVisible(visible));
    }

    /// How many GPU devices the engine has built: 1 until a device is lost, then one more for each rebuild.
    pub fn gpu_generation(&self) -> u64 {
        self.shared.gpu_generation.load(Ordering::Acquire)
    }

    /// `Msg::RetryNow`.
    pub fn retry_now(&self) {
        self.send(Msg::RetryNow);
    }

    /// Whether a start is in effect: the engine runs, or waits for somebody to watch, or reconnects. `false` after
    /// a Stop or a fatal error. Ordered after every command queued before the call, like [`Engine::resume_state`]
    /// (a Start that the control thread has not read yet counts).
    pub fn is_started(&self) -> bool {
        self.resume_state().is_some()
    }

    /// Everything needed to bring a new engine to the state this one is in, or `None` while it is stopped. The
    /// owner takes it before it releases the engine (the virtual camera is being changed) and hands it to
    /// [`Engine::resume`] on the new one.
    ///
    /// The control thread answers, after every message that was queued before this call: a Start, a Stop, a source,
    /// output or chain change that the caller issued just before is part of the answer. The fields the control
    /// thread updates only when it reads a message are not read from here.
    pub fn resume_state(&self) -> Option<ResumeState> {
        if let Some(state) = self.ask_state().wait() {
            return state;
        }
        // The control thread is gone (the engine was closed): what it last published.
        let mut settings = lock(&self.shared.running).clone()?;
        settings.output = *lock(&self.shared.output);
        Some(ResumeState {
            settings,
            paused: self.paused(),
            chain: lock(&self.shared.chain).as_ref().clone(),
            record: self.shared.hub.record(),
        })
    }

    /// Ask the control thread what the engine is doing, without waiting for the answer: the question is queued
    /// behind every message sent before it. [`StateAnswer::wait`] waits.
    pub fn ask_state(&self) -> StateAnswer {
        let (tx, rx) = mpsc::channel();
        #[cfg(test)]
        self.shared.snapshots.fetch_add(1, Ordering::SeqCst);
        StateAnswer(self.tx.send(Msg::Snapshot(tx)).ok().map(|()| rx))
    }

    /// Start with what [`Engine::resume_state`] returned.
    pub fn resume(&self, state: ResumeState) {
        self.shared.hub.set_record(state.record);
        self.set_chain(state.chain);
        self.start(state.settings);
        if state.paused {
            self.set_pause(true);
        }
    }

    /// Test hook: the next `frames` frames fail as if the GPU device was removed (and the device then reports it).
    #[cfg(test)]
    pub(crate) fn inject_device_loss(&self, frames: u32) {
        self.shared.gpu_faults.store(frames, Ordering::SeqCst);
    }

    /// Test hook: what the engine is doing once it has handled everything queued before this call (the same message
    /// [`Engine::resume_state`] sends, whatever the handle's own way of reading is).
    #[cfg(test)]
    pub(crate) fn settled(&self) -> Option<ResumeState> {
        let (tx, rx) = mpsc::channel();
        self.tx.send(Msg::Snapshot(tx)).ok()?;
        rx.recv().ok()?
    }

    /// Test hook: while held, the control thread reads no message.
    #[cfg(test)]
    pub(crate) fn control_gate(&self) -> &Mutex<()> {
        &self.shared.control_gate
    }

    /// Test hook: how many times the engine's state was asked for.
    #[cfg(test)]
    pub(crate) fn snapshot_requests(&self) -> u32 {
        self.shared.snapshots.load(Ordering::SeqCst)
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

    /// [`Engine::close`] for an app that is quitting: the whole quit takes [`SHUTDOWN_END_CAP`] (3 s) at most, not the
    /// [`SESSION_END_CAP`] (30 s) of other stops and not one wait after the other: the exit frees the camera, so a
    /// driver (or a lock that a command holds while it opens a camera for the controls) that never answers must not hold
    /// it up. What does not end by then (the control thread, the poller, the auto runner) is left behind, and the file
    /// says `Closed` whatever it does.
    pub fn close_for_exit(&self) {
        self.close_for_exit_by(Instant::now() + SHUTDOWN_END_CAP);
    }

    /// [`Engine::close_for_exit`] for a quit that began before this call (the caller spent some of its time on its own
    /// waits): the whole quit ends by `deadline`.
    pub fn close_for_exit_by(&self, deadline: Instant) {
        *lock(&self.shared.exit_deadline) = Some(deadline.min(Instant::now() + exit_budget(&self.shared)));
        self.shared.exiting.store(true, Ordering::Release);
        self.close();
    }

    /// [`Engine::shutdown`] for a shared handle. Only the first call does anything (the `Drop` after an explicit close
    /// returns at once, not after a second wait).
    pub fn close(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        let exiting = self.shared.exiting.load(Ordering::Acquire);
        let mut left_behind = false;
        let handle = lock(&self.control).take();
        if let Some(h) = handle {
            let _ = self.tx.send(Msg::Shutdown);
            if exiting {
                // The control thread can be in a call that does not return (the hub's lease lock, the loader's join, a
                // Start that opens a camera): it gets until the deadline of the quit.
                if wait_until_exit_deadline(&self.shared, || h.is_finished()) {
                    let _ = h.join();
                } else {
                    eprintln!("[engine] the control thread did not end within the quit: leaving it behind");
                    left_behind = true;
                }
            } else {
                let _ = h.join();
            }
        }
        let poller = lock(&self.poller).take();
        let runner = lock(&self.runner).take();
        let shared = self.shared.clone();
        let tail = move || {
            if let Some((stop, h)) = poller {
                stop.store(true, Ordering::Release);
                let _ = h.join();
            }
            if let Some((stop, h)) = runner {
                stop.store(true, Ordering::Release);
                let _ = h.join();
            }
            shared.hub.release_lease();
        };
        if !exiting {
            tail();
            return;
        }
        // Quitting: a camera driver that does not answer can hold the poller (a read of the controls), the auto runner
        // (a write, under the hub's gate) and the release of the lease. The exit frees the camera anyway, so they get
        // what is left of the quit and are left behind after that (the app is about to end).
        match std::thread::Builder::new()
            .name("engine close".into())
            .spawn(tail)
        {
            Ok(helper) => {
                if wait_until_exit_deadline(&self.shared, || helper.is_finished()) {
                    let _ = helper.join();
                } else {
                    eprintln!("[engine] the controls did not stop within the quit: leaving them behind");
                }
            }
            Err(e) => {
                eprintln!("[engine] cannot start the thread that ends the controls ({e}): they are left running");
            }
        }
        // A control thread that is stuck writes its slate when the call it is in returns: it writes nothing from here on.
        // This comes first, so that no write of its own can follow the `Closed` below; the `Closed` goes through a lease
        // (a write gate of its own), as the retired writer writes nothing.
        // A control thread that is stuck writes its slate when the call it is in returns: it writes nothing from here on.
        // This comes first, so that no write of its own can follow the `Closed` below; the `Closed` goes through a lease
        // (a write gate of its own), as the retired writer writes nothing.
        if left_behind {
            self.writer.retire();
            #[cfg(test)]
            if let Some(hook) = lock(&self.shared.after_retire).take() {
                hook();
            }
        }
        // The file says closed whatever the control thread did or is still doing. Under the lock the render thread goes
        // live under, which it does not do once the app is quitting, so nothing writes `Live` after this.
        {
            let _live = lock(&self.shared.live_gen);
            self.writer.lease().close();
        }
    }
}

/// Wait until `done` says so or the quit is over ([`Shared::exit_deadline`]); `false` when the deadline came first.
fn wait_until_exit_deadline(shared: &Shared, done: impl Fn() -> bool) -> bool {
    while !done() {
        let left = lock(&shared.exit_deadline)
            .map_or(Duration::ZERO, |d| d.saturating_duration_since(Instant::now()));
        if left.is_zero() {
            return false;
        }
        std::thread::sleep(SESSION_END_POLL.min(left));
    }
    true
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
    /// Set (with the writer retired) when the control thread gives up waiting for the thread and leaves it behind: from
    /// then on it writes nothing to anything the engine shares ([`Control::abandon`]).
    abandoned: Arc<AtomicBool>,
    /// The session's own handle on the frame file: the render thread writes through it, so retiring it ends the
    /// thread's writes to the file whatever the thread is doing ([`vcam_protocol::Writer::lease`]).
    writer: Arc<Writer>,
    join: JoinHandle<()>,
}

/// What the file shows while [`Control::end_session`] waits for the render thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Leave {
    /// The last picture stays (a reopen: the camera comes back at once), for [`Timings::gpu_slate_delay`] at most;
    /// then the "starting" slate.
    Picture,
    /// The slate the engine's settings give ([`Control::slate_state`]) from the moment the stop flag is set.
    Slate,
    /// This state from the moment the stop flag is set (a stop or a failure: nothing is going to run; the app is
    /// quitting: `Closed`).
    State(WriterState),
}

struct SessionEvent {
    gen: u64,
    msg: SessionMsg,
}

enum SessionMsg {
    /// The camera is open and the first frame arrived. `fallback` when the saved format was not there and the
    /// device default was opened instead.
    Opened { fallback: Option<FormatFallback> },
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
    /// Messages read from `rx` while a GPU device was rebuilt, handled before any later one.
    backlog: VecDeque<Msg>,
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
    emitted: Option<StateEvent>,
    last_heartbeat: Instant,
    /// When the frame file was last compared with the writer's mapping ([`Control::check_frame_file`]).
    last_file_check: Instant,
    /// The last look at the frame file said "replaced": the next one that says so stops the engine.
    file_replaced_seen: bool,
    /// When the numbers were last looked at for a `stats` event ([`Timings::stats_interval`]).
    last_stats: Instant,
    /// The last `stats` event that went out: the next one is sent only when it differs, and only while the window
    /// is visible.
    last_stats_sent: Option<StatsEvent>,
    hotplug: Option<(HotplugWatcher, Receiver<HotplugEvent>)>,
    /// Since when the camera has been failing for a reason that may be permanent ([`RetryKind::is_generic`]).
    error_streak: Option<Instant>,
    /// When the current session reported its camera open.
    opened_at: Option<Instant>,
    /// The GPU device was lost: the next session needs a new one.
    gpu_dirty: bool,
    /// Device losses in the current run and when the last one happened (`Timings::gpu_failure_window`).
    gpu_losses: u32,
    gpu_last_loss: Option<Instant>,
    /// Since when the engine has been getting back from a device loss or reopening the camera for a changed source
    /// while live (the last frame stays on the file for [`Timings::gpu_slate_delay`], then the "starting" slate shows:
    /// plan 4.9). Cleared when the new session reports its camera open.
    reopening_since: Option<Instant>,
    /// Retries left that follow the arrival of the camera being waited for at the short interval.
    arrival_burst: u32,
    /// No session has opened the current source yet: a camera that opens but sends no picture in a saved format
    /// counts as lacking that format, and the device default is tried. Set by a Start and by a changed source,
    /// cleared by the first session that opens; a reconnect retry that times out is retried in the same format.
    first_open: bool,
    /// The pause flag as the last `engine-paused` event said it.
    paused_reported: bool,
}

impl Control {
    fn run(mut self) {
        ensure_com();
        match HotplugWatcher::start() {
            Ok(h) => self.hotplug = Some(h),
            Err(e) => eprintln!("[engine] hot-plug watcher unavailable: {e}"),
        }
        self.writer.set_state(WriterState::Paused);
        #[cfg(test)]
        let gate_owner = self.shared.clone();
        loop {
            let mut next = match self.backlog.pop_front() {
                Some(m) => Some(m),
                None => match self.rx.recv_timeout(self.timings.tick) {
                    Ok(m) => Some(m),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => {
                        self.shutdown();
                        return;
                    }
                },
            };
            #[cfg(test)]
            let _gate = lock(&gate_owner.control_gate);
            while let Some(m) = next.take() {
                // A command that was queued before the quit began is not run: it would open a camera (and wait for the
                // end of it) for an app that is leaving.
                if matches!(m, Msg::Shutdown) || self.shared.exiting.load(Ordering::Acquire) {
                    self.shutdown();
                    return;
                }
                self.handle(m);
                next = self.backlog.pop_front().or_else(|| self.rx.try_recv().ok());
            }
            self.poll_session();
            self.poll_hotplug();
            self.tick();
        }
    }

    fn shutdown(&mut self) {
        // The file says "closed" from the start of the wait: the camera may take its time to close, and the apps
        // must not be left on the last picture meanwhile.
        self.end_session(Leave::State(WriterState::Closed));
        self.cfg = None;
        self.sync_cfg();
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
                // The render thread advertises the new list at the top of its loop, within a frame, but only once the
                // camera delivers. Until then (opening, reopening, reconnecting, idle) it is this thread's job: a
                // consumer that attaches in that time must not see the list of the old output. Both write under the
                // `advertised` lock, and the render thread compares with what the file holds (not with what it wrote
                // last), so neither can leave the other's stale list behind.
                if self.cfg.is_some() && self.phase != EngineState::Live {
                    self.advertise_idle();
                }
            }
            Msg::Snapshot(reply) => {
                let _ = reply.send(self.snapshot());
            }
            Msg::SetFlip(f) => self.shared.flip.store(f, Ordering::Relaxed),
            Msg::SetPause(on) => self.set_pause(on),
            Msg::SetWindowVisible(v) => self.set_window_visible(v),
            Msg::SetTap(t) => *lock(&self.shared.tap) = t,
            Msg::SetChain(c) => {
                *lock(&self.shared.chain) = Arc::new(c);
                self.shared.chain_gen.fetch_add(1, Ordering::Release);
            }
            Msg::SetPreviewPoint(p) => self.set_preview_point(p),
            Msg::RetryNow => {
                if self.phase == EngineState::Reconnecting && self.session.is_none() {
                    self.next_retry = Some(Instant::now());
                }
            }
            Msg::Hotplug(ev) => self.on_hotplug(vec![ev]),
            Msg::Shutdown => {}
        }
    }

    // ---- state

    fn set_phase(&mut self, phase: EngineState, reason: Option<String>) {
        self.set_phase_kind(phase, reason, None);
    }

    fn set_phase_kind(
        &mut self,
        phase: EngineState,
        reason: Option<String>,
        kind: Option<&'static str>,
    ) {
        self.set_phase_event(StateEvent::new(phase, reason, kind));
    }

    /// [`Control::set_phase_kind`] with the whole event (the list of programs of an `in_use` wait).
    fn set_phase_event(&mut self, event: StateEvent) {
        self.phase = event.state;
        // The event goes out before `Engine::state` changes: whoever sees the state has had the event.
        if self.emitted.as_ref() != Some(&event) {
            self.emitted = Some(event.clone());
            emit(&self.shared, EngineEvent::State(event.clone()));
        }
        *lock(&self.shared.reason) = event.reason;
        *lock(&self.shared.state) = event.state;
        lock(&self.shared.stats).state = event.state;
        self.shared.cv.notify_all();
    }

    /// [`Control::set_phase`] for a phase in which no frames flow (`Stopped`, idle `Standby`, `Reconnecting`): the
    /// writer shows its slate first, so whoever sees the state or its event finds the writer already there.
    fn enter_slate(&mut self, phase: EngineState, reason: Option<String>) {
        self.set_slate();
        self.set_phase(phase, reason);
    }

    /// Copy the settings into [`Shared::running`] (what [`Engine::resume_state`] falls back to once this thread is
    /// gone).
    fn sync_cfg(&self) {
        *lock(&self.shared.running) = self.cfg.clone();
    }

    /// What this engine is doing: the answer to [`Msg::Snapshot`]. Every message queued before it has been handled.
    fn snapshot(&self) -> Option<ResumeState> {
        let mut settings = self.cfg.clone()?;
        // The output settings change without a message to the source settings.
        settings.output = *lock(&self.shared.output);
        Some(ResumeState {
            settings,
            paused: self.shared.paused.load(Ordering::Acquire),
            chain: lock(&self.shared.chain).as_ref().clone(),
            record: self.shared.hub.record(),
        })
    }

    fn set_preview_point(&mut self, point: PreviewPoint) {
        let mut g = lock(&self.shared.preview_point);
        if *g != point {
            *g = point;
            self.shared.preview_point_gen.fetch_add(1, Ordering::Release);
        }
    }

    fn set_window_visible(&mut self, visible: bool) {
        let was = std::mem::replace(&mut self.window_visible, visible);
        self.shared.window_visible.store(visible, Ordering::Release);
        if !visible {
            // A gesture is over when nobody can see the window: the preview is the output again.
            self.set_preview_point(PreviewPoint::Output);
        }
        if visible && !was {
            // The window heard nothing while it was hidden: it gets the numbers as they are now, once. (The controls
            // poller sees the flag and does the same for the device controls.)
            self.last_stats_sent = None;
            self.emit_stats();
        }
    }

    /// Report the pause flag when it differs from what was reported last.
    fn report_paused(&mut self) {
        let paused = self.shared.paused.load(Ordering::Acquire);
        if paused != self.paused_reported {
            self.paused_reported = paused;
            emit(&self.shared, EngineEvent::Paused(PausedEvent { paused }));
        }
    }

    /// One beat of the file's heartbeat, at most every 100 ms.
    fn beat(&mut self) {
        let now = Instant::now();
        if now.duration_since(self.last_heartbeat) >= Duration::from_millis(100) {
            self.writer.heartbeat();
            self.last_heartbeat = now;
        }
    }

    /// The writer state while no frames flow: `Paused` when stopped or paused, else `Starting`. Depends on `cfg` and
    /// the pause flag, not on the phase.
    fn slate_state(&self) -> WriterState {
        let paused = self.shared.paused.load(Ordering::Acquire);
        if self.cfg.is_none() || paused {
            WriterState::Paused
        } else {
            WriterState::Starting
        }
    }

    /// Show the slate ([`Control::slate_state`]). Written under the lock the render thread takes to go live
    /// (`Shared::live_gen`), so a first frame that is published late cannot put `Live` back over it: the thread looks
    /// at its stop flag under that lock, and the stop flag is set before a slate is written for a session that ends.
    fn set_slate(&self) {
        self.set_writer_state(self.slate_state());
    }

    /// Write `state` under the lock of [`Control::set_slate`].
    fn set_writer_state(&self, state: WriterState) {
        let _live = lock(&self.shared.live_gen);
        self.writer.set_state(state);
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
        self.end_session(Leave::Slate);
        *lock(&self.shared.error) = None;
        // An engine whose frame file was replaced (and stopped for it) must not start again on the file it still has
        // mapped: it would publish for nobody. Looked at here whatever the interval, twice (a moment apart) before it
        // is believed, like the periodic look ([`Control::check_frame_file`]).
        if self.writer.backing_file() == vcam_protocol::BackingFile::Replaced {
            std::thread::sleep(FILE_RECHECK_DELAY);
            if self.writer.backing_file() == vcam_protocol::BackingFile::Replaced {
                self.last_file_check = Instant::now();
                self.file_replaced_seen = false;
                self.fail(FRAME_FILE_REPLACED.into());
                return;
            }
        }
        *lock(&self.shared.output) = s.output;
        self.shared.flip.store(s.flip_vertical, Ordering::Relaxed);
        *lock(&self.shared.color) = s.color;
        self.shared.paused.store(false, Ordering::Release);
        self.idle_since = None;
        self.next_retry = None;
        self.retrying = false;
        self.error_streak = None;
        self.reopening_since = None;
        self.gpu_losses = 0;
        self.gpu_last_loss = None;
        self.first_open = true;
        self.cfg = Some(s);
        self.sync_cfg();
        self.report_paused();
        if self.wants_camera() {
            self.open_session(OpenMode::Fresh);
        } else {
            self.go_idle();
        }
    }

    fn stop(&mut self) {
        // Nothing runs after this: the file shows the paused slate from the first moment of the wait, not the last
        // picture for as long as the camera takes to close.
        self.end_session(Leave::State(WriterState::Paused));
        self.cfg = None;
        self.sync_cfg();
        self.shared.paused.store(false, Ordering::Release);
        self.idle_since = None;
        self.next_retry = None;
        self.retrying = false;
        self.error_streak = None;
        self.reopening_since = None;
        self.report_paused();
        self.enter_slate(EngineState::Stopped, None);
        self.emit_stats();
    }

    /// A fatal failure: `Stopped`, with the message for the owner. `kind` goes with the `Stopped` event (`format`).
    fn fail(&mut self, message: String) {
        self.fail_kind(message, None);
    }

    fn fail_kind(&mut self, message: String, kind: Option<&'static str>) {
        self.end_session(Leave::State(WriterState::Paused));
        eprintln!("[engine] {message}");
        *lock(&self.shared.error) = Some(message.clone());
        self.cfg = None;
        self.sync_cfg();
        self.shared.paused.store(false, Ordering::Release);
        self.idle_since = None;
        self.next_retry = None;
        self.retrying = false;
        self.error_streak = None;
        self.reopening_since = None;
        self.report_paused();
        // The slate first: neither the error nor the `Stopped` event may be seen with the writer still `Starting`.
        self.set_slate();
        emit(&self.shared, EngineEvent::Error(message));
        self.set_slate();
        self.set_phase_kind(EngineState::Stopped, Some("error".into()), kind);
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
        if reopen {
            // A new source is a first open: the fallback to the device default applies to it again.
            self.first_open = true;
        }
        self.sync_cfg();
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
            self.report_paused();
            return;
        }
        self.shared.paused.store(on, Ordering::Release);
        self.report_paused();
        if on {
            self.writer.set_state(WriterState::Paused);
        } else if self.phase != EngineState::Live || self.session.is_none() {
            self.set_slate();
        }
        // Live and resumed: the render thread leaves `Paused` with the first frame it publishes.
    }

    // ---- sessions

    fn open_session(&mut self, mode: OpenMode) {
        // No camera is opened for an app that is quitting (the quit finishes with what is open).
        if self.cfg.is_none() || self.shared.exiting.load(Ordering::Acquire) {
            return;
        }
        // A reopen keeps the last picture while the camera closes (for a second at most); the other opens show the slate.
        self.end_session(match mode {
            OpenMode::Reopen => Leave::Picture,
            OpenMode::Fresh | OpenMode::Retry => Leave::Slate,
        });
        if mode == OpenMode::Reopen {
            // The old picture stays for the delay and then gives way to the slate for however long the camera takes to
            // open (a device loss has set the time already). `tick` does it: the session of this open has not been
            // live, so its `gen` is not `live_gen` until its first frame (`slate_after_gpu_loss`).
            self.reopening_since.get_or_insert_with(Instant::now);
        }
        // The device may have been lost while nothing used it (a driver update while the app sat in the tray, the
        // camera off in standby, the engine stopped): look before every open, so the camera is never handed a dead
        // device, and its failure never mistaken for a format or camera problem.
        if !self.gpu_dirty {
            if let Some(hr) = self.renderer.gpu.removed_reason() {
                if !self.note_gpu_loss(&format!("DXGI device removed ({})", hr_text(hr))) {
                    return;
                }
            }
        }
        // The GPU device was lost: a new one first (a failure counts as another loss and is handled there).
        if self.gpu_dirty {
            // A Stop or Shutdown that came in meanwhile is handled before the camera is opened.
            if !self.rebuild_gpu() || self.stop_queued() {
                return;
            }
        }
        let Some(cfg) = &self.cfg else {
            return;
        };
        let (source, readback) = (cfg.source.clone(), cfg.readback);
        // A lease on the camera (a control call while no camera was open) makes way for the engine.
        self.shared.hub.release_lease();
        // The lease may have been waited for for a long time (a command that opens a camera for the controls holds it):
        // a quit that began meanwhile does not get a session.
        if self.shared.exiting.load(Ordering::Acquire) {
            return;
        }
        self.gen += 1;
        let (stop, removed, abandoned) = (
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );
        // The session writes to the file through a handle of its own, so that it can be cut off if it is left behind.
        let writer = Arc::new(self.writer.lease());
        let ctx = SessionCtx {
            source,
            readback,
            gen: self.gen,
            shared: self.shared.clone(),
            renderer: self.renderer.clone(),
            assets: self.assets.client(),
            writer: writer.clone(),
            factory: self.factory.clone(),
            stop: stop.clone(),
            removed: removed.clone(),
            abandoned: abandoned.clone(),
            events: self.events_tx.clone(),
            first_open: self.first_open,
        };
        self.attempt_started = Instant::now();
        self.retrying = mode == OpenMode::Retry;
        self.next_retry = None;
        if mode == OpenMode::Fresh {
            // Before the session thread exists: its first published frame sets `Live`, and the slate must not
            // overwrite that.
            self.set_slate();
            // A consumer that attaches while the camera opens sees the output that is configured now, not the list
            // the last run left in the file.
            self.advertise_idle();
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
                    abandoned,
                    writer,
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
    ///
    /// The thread checks the stop flag between the steps of opening a camera, not inside one (a camera that takes its
    /// time to open, a control write that hangs), so the wait can last seconds. Nothing else beats the file's
    /// heartbeat meanwhile, so this does: the virtual camera shows "Better Cam is not running" after a second without
    /// one. But a heartbeat on a file that still says `Live` keeps the apps on the last picture, frozen, for as long
    /// as the wait lasts, so `leave` says what the file shows meanwhile: the slate (or another state) from the moment
    /// the stop flag is set, or, for a reopen, the last picture for [`Timings::gpu_slate_delay`] and then the slate.
    ///
    /// After [`SESSION_END_CAP`] (3 s when the app is quitting) the thread is left behind: it ends by itself, its events
    /// are ignored (the session is no longer the current one) and it can write nothing that the engine shares
    /// ([`Control::abandon`]), so a driver that never answers cannot hold up a Stop or the app's exit.
    fn end_session(&mut self, leave: Leave) {
        if let Some(s) = self.session.take() {
            s.stop.store(true, Ordering::Release);
            // After the stop flag, under the lock of the render thread's first frame: that frame is either live
            // already (and covered here) or finds the flag and does not go live.
            match leave {
                Leave::Picture => {}
                Leave::Slate => self.set_slate(),
                Leave::State(state) => self.set_writer_state(state),
            }
            let started = Instant::now();
            let mut slated = leave != Leave::Picture;
            while !s.join.is_finished() {
                self.beat();
                if !slated && started.elapsed() >= self.timings.gpu_slate_delay {
                    // the camera is not back within a second: the last picture gives way to the slate
                    slated = true;
                    self.set_slate();
                }
                // Looked at on every pass: a quit ([`Engine::close_for_exit`]) that comes while this wait runs
                // shortens it.
                if end_cap(&self.shared, started).is_zero() {
                    eprintln!(
                        "[engine] the render thread did not end within {:.1} s of the stop: leaving it behind",
                        started.elapsed().as_secs_f32()
                    );
                    self.abandon(&s);
                    return;
                }
                std::thread::sleep(SESSION_END_POLL);
            }
            let _ = s.join.join();
        }
        *lock(&self.shared.camera_link) = None;
    }

    /// The control thread gives up on a render thread: from here on it writes nothing the engine shares, whatever it
    /// is doing and for however long it lives. Everything it writes is cut off in an order that leaves no gap: the
    /// flag first, then a pass through each lock the thread writes under (a write that began before the flag was set
    /// is over when the pass returns, one that begins later sees the flag), then the frame file ([`Writer::retire`]).
    fn abandon(&self, s: &Session) {
        s.abandoned.store(true, Ordering::Release);
        s.writer.retire();
        // the camera it may still announce, its statistics, its facts
        *lock(&self.shared.camera_link) = None;
        drop(lock(&self.shared.stats));
        *lock(&self.shared.facts) = Arc::new(Mutex::new(crate::meter::SessionFacts::CLOSED));
        // what it may have put in the meter's mailbox is not for the session that follows it
        self.shared.meter.clear();
    }

    /// `Standby` with the camera closed: nobody watches.
    fn go_idle(&mut self) {
        self.end_session(Leave::Slate);
        self.idle_since = None;
        self.next_retry = None;
        self.retrying = false;
        // The camera is closed for good measure: failures counted before the idle time say nothing about the next
        // wake-up, which must not stop the engine on its first failure.
        self.error_streak = None;
        self.opened_at = None;
        self.enter_slate(EngineState::Standby, Some("idle".into()));
        self.advertise_idle();
    }

    fn go_reconnecting(&mut self, kind: RetryKind, reason: String) {
        // A failed retry waits one interval from its own start; a camera lost while live waits from now.
        let base = if self.retrying {
            self.attempt_started
        } else {
            Instant::now()
        };
        self.retrying = false;
        let interval = if self.arrival_burst > 0 {
            self.arrival_burst -= 1;
            self.timings.retry_interval.min(ARRIVAL_RETRY_INTERVAL)
        } else {
            self.timings.retry_interval
        };
        self.next_retry = Some((base + interval).max(Instant::now()));
        self.set_slate();
        let mut event = StateEvent::new(EngineState::Reconnecting, Some(reason), Some(kind.name()));
        if matches!(kind, RetryKind::InUse | RetryKind::AccessDenied) {
            self.name_camera_users(&mut event);
        }
        self.set_phase_event(event);
        self.advertise_idle();
    }

    /// Say which programs use the camera (the registry and the camera service's log are read again with every failed
    /// try, so the names follow reality). The code of the failure goes to the log, not to the window.
    fn name_camera_users(&self, event: &mut StateEvent) {
        let target = self.target_link();
        let found = self.factory.camera_users(target.as_deref(), self.writer.consumer_active());
        let names = found.names;
        let maybe = !names.is_empty() && found.maybe;
        let code = event.reason.take().unwrap_or_default();
        event.reason = Some(crate::capture::in_use::reason(&names, maybe));
        event.apps_maybe = maybe;
        event.apps = (!names.is_empty()).then_some(names);
        if self.emitted.as_ref() != Some(&*event) {
            eprintln!("[engine] {code}; {}", event.reason.as_deref().unwrap_or_default());
        }
    }

    /// The GPU device was lost (`DXGI_ERROR_DEVICE_REMOVED`, a reset): the session ended and its camera is closed.
    /// A new device, renderer, asset loader and, with the next session, filter chain and capture follow at once; the
    /// last frame stays on the file meanwhile. Three losses in a row stop the engine.
    fn gpu_lost(&mut self, why: String) {
        eprintln!("[engine] GPU device lost: {why}");
        if !self.note_gpu_loss(&why) {
            return;
        }
        // (A reopen that was already running keeps its start: the slate comes `gpu_slate_delay` after the first loss.)
        self.reopening_since.get_or_insert_with(Instant::now);
        // The window and the log say what is going on while the device is rebuilt (the last frame stays on the file).
        self.set_phase(EngineState::Standby, Some("reopening".into()));
        self.open_session(OpenMode::Reopen);
    }

    /// The current GPU device is lost: count it and mark it for a rebuild. `false` when it is the third of one run:
    /// the engine stopped with an error (and keeps the dead device marked, so the next Start builds a new one before
    /// it opens the camera).
    fn note_gpu_loss(&mut self, why: &str) -> bool {
        self.gpu_dirty = true;
        if self.count_gpu_loss() {
            return true;
        }
        self.fail(format!(
            "The graphics device was lost {GPU_LOSSES_MAX} times in a row ({why}). Update the graphics driver or restart Windows, then press Start."
        ));
        false
    }

    /// Count a device loss; `false` once it is the third of one run.
    fn count_gpu_loss(&mut self) -> bool {
        let now = Instant::now();
        if self
            .gpu_last_loss
            .is_some_and(|t| now.duration_since(t) > self.timings.gpu_failure_window)
        {
            self.gpu_losses = 0;
        }
        self.gpu_last_loss = Some(now);
        self.gpu_losses += 1;
        self.gpu_losses < GPU_LOSSES_MAX
    }

    /// A new device, renderer and asset loader on the adapter kind of the old one. `false` when it failed: that
    /// counts as another loss (the driver may still be coming back), and the engine either tries again after the
    /// retry interval or stops.
    ///
    /// The device is built on a helper thread (it takes 0.6-0.8 s on WARP and may take longer after a driver reset)
    /// while this thread keeps the file's heartbeat going: the virtual camera shows "Better Cam is not running"
    /// after a second without one. Messages that arrive meanwhile wait in the queue ([`Control::backlog`]); a Stop or
    /// a Shutdown among them ends the wait (`false`, nothing counted). A rebuild that takes longer than
    /// `Timings::gpu_rebuild_timeout` is a failed one; its helper thread is left to finish by itself.
    fn rebuild_gpu(&mut self) -> bool {
        let started = Instant::now();
        let kind = self.renderer.gpu.kind;
        let shared = self.shared.clone();
        let (done, result) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("gpu rebuild".into())
            .spawn(move || {
                ensure_com();
                #[cfg(test)]
                {
                    let delay = *lock(&shared.rebuild_delay);
                    if !delay.is_zero() {
                        std::thread::sleep(delay);
                    }
                }
                let built = (|| -> Result<(Arc<Renderer>, Assets), String> {
                    let gpu = Arc::new(GpuDevice::new(kind).map_err(|e| format!("D3D11 device: {e}"))?);
                    let renderer = Arc::new(Renderer::new(gpu).map_err(|e| e.to_string())?);
                    renderer.shaders.warm_up().map_err(|e| e.to_string())?;
                    crate::fx::warm_up(&renderer).map_err(|e| e.to_string())?;
                    crate::meter::warm_up(&renderer).map_err(|e| e.to_string())?;
                    let events = shared.clone();
                    let assets = Assets::new(
                        renderer.gpu.clone(),
                        Arc::new(move |e| emit(&events, EngineEvent::FilterError(e))),
                    );
                    Ok((renderer, assets))
                })();
                let _ = done.send(built);
            });
        let built = match spawned {
            Ok(_) => loop {
                match result.recv_timeout(self.timings.tick) {
                    Ok(built) => break built,
                    Err(RecvTimeoutError::Timeout) => {
                        self.beat();
                        // `tick` does not run meanwhile: a recovery that takes long shows the slate from here.
                        self.slate_after_gpu_loss(Instant::now(), true);

                        if self.stop_queued() {
                            eprintln!("[engine] GPU device rebuild abandoned: the engine is stopping");
                            return false;
                        }
                        if started.elapsed() >= self.timings.gpu_rebuild_timeout {
                            break Err(format!(
                                "it took longer than {:.1} seconds",
                                self.timings.gpu_rebuild_timeout.as_secs_f32()
                            ));
                        }
                    }
                    Err(RecvTimeoutError::Disconnected) => {
                        break Err("the rebuild thread ended without a result".to_string())
                    }
                }
            },
            Err(e) => Err(format!("cannot start the rebuild thread: {e}")),
        };
        match built {
            Ok((renderer, assets)) => {
                // The old loader thread ends with the old `Assets`; the old device goes when its last user does. The
                // drop waits for a loader that is stuck in a file read (up to 2 s), which would leave the control
                // thread without a heartbeat for that long: it is made on a helper thread.
                drop_off_thread(std::mem::replace(&mut self.assets, assets));
                self.renderer = renderer;
                if let Some(tap) = lock(&self.shared.tap).as_mut() {
                    tap.device_changed();
                }
                self.gpu_dirty = false;
                self.shared.gpu_generation.fetch_add(1, Ordering::AcqRel);
                eprintln!("[engine] GPU device rebuilt in {:?}", started.elapsed());
                true
            }
            Err(e) => {
                eprintln!("[engine] cannot rebuild the GPU device: {e}");
                if !self.count_gpu_loss() {
                    self.fail(format!(
                        "The graphics device was lost and could not be rebuilt ({e}). Update the graphics driver or restart Windows, then press Start."
                    ));
                } else {
                    // Try again in a moment, as for a lost camera.
                    self.retrying = false;
                    self.next_retry = Some(Instant::now() + self.timings.retry_interval);
                    self.set_slate();
                    self.set_phase_kind(
                        EngineState::Reconnecting,
                        Some("the graphics device is being reset".into()),
                        Some(RetryKind::Error.name()),
                    );
                }
                false
            }
        }
    }

    /// Move the messages that wait in the queue to [`Control::backlog`]; `true` when a Stop or a Shutdown is among
    /// them (so the engine must not open a camera first).
    fn stop_queued(&mut self) -> bool {
        while let Ok(m) = self.rx.try_recv() {
            self.backlog.push_back(m);
        }
        self.backlog.iter().any(|m| matches!(m, Msg::Stop | Msg::Shutdown))
    }

    /// Keep the advertised media types current while no camera is open, so a consumer that attaches then (and wakes
    /// the engine) sees the configured size: an explicit output size, else what the last session advertised.
    fn advertise_idle(&self) {
        advertise_configured(&self.shared, &self.writer);
    }

    fn poll_session(&mut self) {
        while let Ok(ev) = self.events_rx.try_recv() {
            if self.session.as_ref().map(|s| s.gen) != Some(ev.gen) {
                continue; // an older session that was replaced
            }
            match ev.msg {
                SessionMsg::Opened { fallback } => {
                    self.idle_since = None;
                    self.retrying = false;
                    self.next_retry = None;
                    self.opened_at = Some(Instant::now());
                    self.reopening_since = None;
                    self.arrival_burst = 0;
                    // This source has opened: from now on a camera that sends no picture is retried in its format.
                    self.first_open = false;
                    if let Some(fb) = fallback {
                        // From now on the engine runs with what it opened, so a later reopen (the camera comes
                        // back) does not ask for the missing format again and the owner's `set_source` with the same
                        // settings is no change.
                        let interval = fb.cfg.frame_interval;
                        if let Some(cfg) = self.cfg.as_mut() {
                            cfg.source = SourceSpec::Configured(fb.cfg);
                        }
                        self.sync_cfg();
                        emit(
                            &self.shared,
                            EngineEvent::FormatFallback(FormatFallbackEvent {
                                asked: fb.asked,
                                used: fb.used,
                                width: fb.width,
                                height: fb.height,
                                interval,
                                format: fb.format,
                            }),
                        );
                    }
                    self.set_phase(EngineState::Live, None);
                }
                SessionMsg::Ended(result) => {
                    // The thread is done: nothing to wait for, and the picture it left stays until the handling below
                    // decides (a lost device keeps it for a second).
                    self.end_session(Leave::Picture);
                    let lived = self.opened_at.take().map(|t| t.elapsed());
                    match result {
                        Ok(()) => self.go_idle(),
                        Err(Failure::Retry(kind, reason)) => {
                            if self.generic_failure_exhausted(kind, lived) {
                                self.fail(format!(
                                    "The camera kept failing for {} seconds ({reason}). Check the camera and its cable, then press Start.",
                                    self.timings.error_cap.as_secs()
                                ));
                            } else {
                                self.go_reconnecting(kind, reason);
                            }
                        }
                        Err(Failure::Format(message)) => self.fail_kind(message, Some("format")),
                        Err(Failure::GpuLost(why)) => self.gpu_lost(why),
                        Err(Failure::Fatal(message)) => self.fail(message),
                    }
                }
            }
        }
    }

    /// Keeps the count for failures that may be permanent: `true` when the camera has been failing for
    /// `Timings::error_cap` (counted from the first failure of the run; a camera that stayed up for
    /// `Timings::stable_after` starts a new run). Other kinds end the run.
    fn generic_failure_exhausted(&mut self, kind: RetryKind, lived: Option<Duration>) -> bool {
        streak_exhausted(&mut self.error_streak, kind, lived, Instant::now(), &self.timings)
    }

    fn poll_hotplug(&mut self) {
        let events: Vec<HotplugEvent> = match &self.hotplug {
            Some((_, rx)) => rx.try_iter().collect(),
            None => return,
        };
        self.on_hotplug(events);
    }

    fn on_hotplug(&mut self, events: Vec<HotplugEvent>) {
        if events.is_empty() {
            return;
        }
        emit(&self.shared, EngineEvent::DevicesChanged);
        for ev in events {
            match ev {
                HotplugEvent::Arrival { link } => {
                    if self.phase == EngineState::Reconnecting
                        && arrival_matches(self.target_link().as_deref(), &link)
                    {
                        // Media Foundation may not list the device for a moment after Windows announced it: the
                        // first retries come faster than the usual interval.
                        self.arrival_burst = ARRIVAL_RETRIES;
                        if self.session.is_none() {
                            self.next_retry = Some(Instant::now());
                        }
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

    /// A device loss, or a reopen of the camera for a changed source, that takes longer than `Timings::gpu_slate_delay`
    /// to get over: the frame that was repeated gives way to the "starting" slate (the first frame of the new session sets `Live` again, and is the only thing that
    /// does: see `live_gen`). Called by [`Control::tick`] and, while the device is rebuilt ([`Control::rebuild_gpu`],
    /// which is where a slow recovery spends its time and `tick` does not run), by the wait loop.
    ///
    /// `rebuilding`: called from the wait of [`Control::rebuild_gpu`], which runs before the next session is made. No
    /// render thread runs then (the one the loss ended is joined, `gen` is still its number, and it did go live), so
    /// nothing can be covered by mistake and the slate shows whatever `live_gen` says.
    fn slate_after_gpu_loss(&mut self, now: Instant, rebuilding: bool) {
        if self
            .reopening_since
            .is_some_and(|t| now.duration_since(t) >= self.timings.gpu_slate_delay)
        {
            self.reopening_since = None;
            // Under the lock the render thread takes to go live: the slate cannot replace the picture of a session
            // that has published its first frame already.
            let live = lock(&self.shared.live_gen);
            if rebuilding || *live != self.gen {
                // (the lock is held: not `set_slate`)
                self.writer.set_state(self.slate_state());
            }
        }
    }

    /// Whether `frames.bin` is still the file the writer has mapped. An installer's Repair, a cleaner or a person that
    /// deletes or replaces it leaves the engine publishing into a file nobody reads (the virtual camera opens the file
    /// by its path): the engine stops with a message that names the Repair, like an engine that finds no frame file
    /// at its start. Looked at every [`FILE_CHECK_INTERVAL`] while the engine runs, and at every Start.
    fn check_frame_file(&mut self, now: Instant) {
        if self.cfg.is_none() || now.duration_since(self.last_file_check) < self.file_check_interval() {
            return;
        }
        self.last_file_check = now;
        #[cfg(test)]
        {
            let n = self.shared.file_looks.fetch_add(1, Ordering::SeqCst) + 1;
            if let Some(hook) = lock(&self.shared.before_file_look).as_mut() {
                hook(n);
            }
        }
        // Two looks in a row say "replaced" before the engine stops: one can be a file that is being swapped by a
        // Repair, and an answer that is not "replaced" (including "could not tell") starts the count again.
        let replaced = self.writer.backing_file() == vcam_protocol::BackingFile::Replaced;
        #[cfg(test)]
        if replaced {
            self.shared.file_looks_replaced.fetch_add(1, Ordering::SeqCst);
        }
        if replaced {
            if std::mem::replace(&mut self.file_replaced_seen, true) {
                self.file_replaced_seen = false;
                self.fail(FRAME_FILE_REPLACED.into());
            }
        } else {
            self.file_replaced_seen = false;
        }
    }

    fn file_check_interval(&self) -> Duration {
        #[cfg(test)]
        if let Some(i) = *lock(&self.shared.file_check_interval) {
            return i;
        }
        FILE_CHECK_INTERVAL
    }

    fn tick(&mut self) {
        let now = Instant::now();
        // The DLL shows "not running" after a second without a heartbeat; the render thread beats while it works,
        // this covers everything else (opening, standby, stopped).
        self.beat();
        self.slate_after_gpu_loss(now, false);
        self.check_frame_file(now);

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

    /// Send a `stats` event when the window is visible and the numbers differ from the last event that went out. The
    /// figures are refreshed once a second by the render thread, and a state change flips them to zero or back, so
    /// that is about one event a second while live and one after a state change; the DLL's `repeated` and the
    /// `consumers` flag are looked at on every call.
    fn emit_stats(&mut self) {
        if !self.window_visible {
            return;
        }
        let event = self.stats_event();
        if self.last_stats_sent.as_ref() == Some(&event) {
            return;
        }
        self.last_stats_sent = Some(event.clone());
        emit(&self.shared, EngineEvent::Stats(event));
    }

    fn stats_event(&self) -> StatsEvent {
        let s = lock(&self.shared.stats).clone();
        let live = self.phase == EngineState::Live;
        let header = self.writer.header();
        StatsEvent {
            capture: CaptureStatsEvent {
                width: if live { s.capture_width } else { 0 },
                height: if live { s.capture_height } else { 0 },
                format: if live {
                    s.capture_format.clone()
                } else {
                    String::new()
                },
                fps: if live { s.capture_fps } else { 0.0 },
                rate: if live { s.capture_rate } else { 0.0 },
                decode: if live {
                    s.decode.to_string()
                } else {
                    "none".to_string()
                },
            },
            output: OutputStatsEvent {
                width: if live { s.output_width } else { 0 },
                height: if live { s.output_height } else { 0 },
                // What the apps get; with none attached nothing is published, and the picture the preview shows comes
                // at the render rate.
                fps: match (live, s.consumer) {
                    (false, _) => 0.0,
                    (true, true) => s.publish_fps,
                    (true, false) => s.render_fps,
                },
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
        }
    }
}

/// The count behind [`Control::generic_failure_exhausted`], with the clock passed in: `first` is when the run of
/// failures began; `true` when this failure (at `now`) is at least `Timings::error_cap` after it. A failure that is not
/// generic, or one of a camera that was up for `Timings::stable_after`, starts a new run.
fn streak_exhausted(
    first: &mut Option<Instant>,
    kind: RetryKind,
    lived: Option<Duration>,
    now: Instant,
    timings: &Timings,
) -> bool {
    if !kind.is_generic() {
        *first = None;
        return false;
    }
    let stable = lived.is_some_and(|l| l >= timings.stable_after);
    let start = match *first {
        Some(first) if !stable => first,
        _ => now,
    };
    *first = Some(start);
    now.duration_since(start) >= timings.error_cap
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
    /// The control thread gave up on this session ([`Control::abandon`]): nothing the engine shares is written any more.
    abandoned: Arc<AtomicBool>,
    events: Sender<SessionEvent>,
    /// No session has opened this source yet (see [`Control::first_open`]).
    first_open: bool,
}

/// The render thread (and the synthetic source that runs on it) registered with the Multimedia Class Scheduler as
/// "Capture" for the session: a thread of that class is scheduled ahead of ordinary work, which the periodic repeats of
/// the virtual camera in a busy machine point at. Best effort: when the call fails (no service, no rights) the
/// thread simply runs at its normal priority.
struct Mmcss(Option<windows::Win32::Foundation::HANDLE>);

impl Mmcss {
    fn capture() -> Mmcss {
        // Not in the tests: they run many engines in parallel, and render threads of a class that is scheduled ahead of
        // everything else starve the test threads that read the file (a consumer that misses its heartbeat).
        #[cfg(test)]
        return Mmcss(None);
        #[cfg(not(test))]
        Mmcss::register()
    }

    #[cfg(not(test))]
    fn register() -> Mmcss {
        let mut task = 0u32;
        // SAFETY: plain call; the task name is a NUL-terminated literal and `task` a local.
        let h = unsafe {
            windows::Win32::System::Threading::AvSetMmThreadCharacteristicsW(
                windows::core::w!("Capture"),
                &mut task,
            )
        };
        Mmcss(h.ok())
    }
}

impl Drop for Mmcss {
    fn drop(&mut self) {
        if let Some(h) = self.0 {
            // SAFETY: the handle came from `AvSetMmThreadCharacteristicsW` on this thread.
            let _ = unsafe { windows::Win32::System::Threading::AvRevertMmThreadCharacteristics(h) };
        }
    }
}

fn session_thread(ctx: SessionCtx) {
    ensure_com();
    let _mmcss = Mmcss::capture();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| render_session(&ctx)));
    let outcome = match result {
        Ok(r) => r,
        Err(_) => Err(Failure::Fatal("the render thread panicked".into())),
    };
    let outcome = outcome_with_privacy(outcome, &*ctx.factory);
    // The camera (inside `render_session`) is closed by now.
    let _ = ctx.events.send(SessionEvent {
        gen: ctx.gen,
        msg: SessionMsg::Ended(outcome),
    });
}

/// An open that failed with "access denied" is Windows' privacy setting when a switch is off. That is the only
/// failure that becomes a privacy wait: a registry value that says `Deny` does not stop every desktop program on
/// every Windows build (it did not on the machine this was measured on), so it never turns an unrelated failure into
/// one. The switches only decide the banner's wording.
fn outcome_with_privacy(
    outcome: Result<(), Failure>,
    factory: &dyn SourceFactory,
) -> Result<(), Failure> {
    match outcome {
        Err(Failure::Retry(RetryKind::AccessDenied, _)) => match factory.privacy_blocked() {
            Some(blocked) => Err(Failure::privacy(blocked)),
            None => outcome,
        },
        other => other,
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
    /// The time the frame was planned for ([`Schedule`]).
    timestamp_100ns: i64,
    /// When the thread made it (never before `timestamp_100ns`).
    arrival_100ns: i64,
}

/// How far ahead of its arrival a camera's capture timestamp may be before it is not believed: the virtual camera
/// clamps capture times at `now + 50 ms` (`crates/vcam-source/src/frames.rs`, `clamp_capture`), so a stamp further
/// ahead than that would only be cut there, and one that is ahead at all is a camera clock that runs fast.
const CAPTURE_AHEAD_MAX: i64 = 500_000;

/// A frame can have been captured up to this long before it arrived (a camera buffers, a USB hub is busy).
const CAPTURE_BEHIND_MAX: i64 = 10_000_000;

/// The capture time of a camera frame (100 ns): the camera's own timestamp when it is plausible, else the arrival.
/// Plausible: positive, at most [`CAPTURE_AHEAD_MAX`] after the arrival (never from the future, apart from the clock
/// jitter of the capture callback), and less than [`CAPTURE_BEHIND_MAX`] before it.
fn plausible_capture(timestamp: i64, arrival: i64) -> i64 {
    if timestamp > 0 && timestamp - arrival <= CAPTURE_AHEAD_MAX && arrival - timestamp < CAPTURE_BEHIND_MAX {
        timestamp
    } else {
        arrival
    }
}

impl InFrame {
    /// `(capture timestamp, arrival)` in 100 ns QPC units. A timestamp that is missing or implausible is replaced by
    /// the arrival time ([`plausible_capture`]).
    fn times(&self) -> (i64, i64) {
        match self {
            InFrame::Camera(f, _) => (
                plausible_capture(f.timestamp_100ns, f.arrival_100ns),
                f.arrival_100ns,
            ),
            InFrame::Synthetic(s) => (s.timestamp_100ns, s.arrival_100ns),
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
    /// The saved format was not on the camera and the device default was opened instead.
    fallback: Option<FormatFallback>,
}

/// The saved format is not offered by the camera: what was asked, what is used, and the settings that are in effect
/// from now on (the device default of the camera chosen).
#[derive(Clone, Debug, PartialEq)]
struct FormatFallback {
    asked: String,
    used: String,
    /// The picture size that is used.
    width: u32,
    height: u32,
    /// The camera's native format that is used.
    format: String,
    cfg: CaptureCfg,
}

/// Opens sources. The engine uses [`DefaultFactory`]; tests plug in sources that fail and recover on command.
trait SourceFactory: Send + Sync {
    /// `output_fps` is the output rate in effect (for `frame_interval` -1). `first_open`: no session has opened this
    /// source yet, so a camera that sends no picture in a saved format may be treated as lacking it (see
    /// [`Control::first_open`]). `stop` is the session's stop flag: an open that takes long (the decoder retry, the
    /// format fallback and the wait for the first frame are up to 5 s each) looks at it between its steps and gives
    /// up when it is set, so ending the session does not wait for all of them ([`Control::end_session`]). What an
    /// open that was stopped returns does not matter: nobody reads it.
    fn open(
        &self,
        spec: &SourceSpec,
        output_fps: Option<u32>,
        gpu: &Arc<GpuDevice>,
        first_open: bool,
        stop: &Arc<AtomicBool>,
    ) -> Result<Box<dyn Source>, Failure>;

    /// Which Windows privacy switch keeps desktop apps from the camera, if any (read after a failure to tell "the
    /// camera is blocked" from "the camera is in use").
    fn privacy_blocked(&self) -> Option<crate::capture::privacy::Blocked> {
        crate::capture::privacy::desktop_access_blocked()
    }

    /// The programs that use the camera `target` (its symbolic link; `None`: the first camera) now (read after a
    /// failure with "in use"): a few registry reads and a bounded read of the camera service's event log. It runs on
    /// the control thread, at most once per retry. `vcam_consumer`: a program reads Better Cam's virtual camera.
    fn camera_users(&self, target: Option<&str>, vcam_consumer: bool) -> crate::capture::in_use::CameraUsers {
        crate::capture::in_use::camera_users(target, vcam_consumer)
    }
}

struct DefaultFactory;

impl SourceFactory for DefaultFactory {
    fn open(
        &self,
        spec: &SourceSpec,
        output_fps: Option<u32>,
        gpu: &Arc<GpuDevice>,
        first_open: bool,
        stop: &Arc<AtomicBool>,
    ) -> Result<Box<dyn Source>, Failure> {
        if !matches!(spec, SourceSpec::Synthetic(_)) && crate::capture::privacy::test_access_denied() {
            return Err(Failure::in_use(Some(windows::Win32::Foundation::E_ACCESSDENIED)));
        }
        Ok(match spec {
            SourceSpec::Camera(c) => Box::new(open_retrying_decoder(c, first_open, |c| {
                open_unless_stopped(stop, || open_camera(c, gpu, first_open, stop))
            })?),
            SourceSpec::Configured(cap) => {
                let (mut camera, fallback) = open_with_fallback(cap, |cap| {
                    // Between the attempts of the format fallback (and the decoder retry below) the stop flag is
                    // looked at, so a stopped session does not go on to the next one.
                    if stop.load(Ordering::Acquire) {
                        return Err(stopped_open());
                    }
                    let c = resolve_camera(cap, output_fps)?;
                    // A camera that cannot deliver a picture in the format it lists is as good as one that lacks it,
                    // but only the first time this source is opened: one that did open and then timed out after a
                    // reconnect is slow, not lacking the format, and is retried in the same format.
                    open_retrying_decoder(&c, first_open, |c| {
                        open_unless_stopped(stop, || open_camera(c, gpu, first_open, stop))
                    })
                    .map_err(|f| format_problem_unless_stopped(f, cap, first_open, stop))
                })?;
                camera.info.fallback = finish_fallback(fallback, &camera.info);
                Box::new(camera)
            }
            SourceSpec::Synthetic(s) => Box::new(SyntheticInput::new(*s)?),
        })
    }
}

/// What an open returns when the session was stopped meanwhile. It is a generic retry failure, so neither the decoder
/// retry nor the format fallback takes it for their own; nobody reads it (the session is gone).
fn stopped_open() -> Failure {
    Failure::Retry(RetryKind::Error, "the camera open was stopped".into())
}

/// [`no_picture_as_format_problem`] for an open that may have been stopped: a stopped session ends the wait for the
/// first frame with a "no picture", which is the stop and not the camera's format, so it must not turn into the
/// fallback to the device default (and its log line "delivers no picture ...; using the device default").
fn format_problem_unless_stopped(
    f: Failure,
    cap: &CaptureCfg,
    first_open: bool,
    stop: &AtomicBool,
) -> Failure {
    if stop.load(Ordering::Acquire) {
        eprintln!("[engine] the camera open was stopped ({})", describe_cap(cap));
        return stopped_open();
    }
    no_picture_as_format_problem(f, cap, first_open)
}

/// One attempt to open a camera, unless the session was stopped before it began.
fn open_unless_stopped<T>(
    stop: &AtomicBool,
    open: impl FnOnce() -> Result<T, Failure>,
) -> Result<T, Failure> {
    if stop.load(Ordering::Acquire) {
        return Err(stopped_open());
    }
    open()
}

/// A camera that cannot deliver a picture in the format it lists is as good as one that lacks it, so the saved
/// format gives way to the device default ([`open_with_fallback`]). But only the first time this source is opened
/// (`first_open`: the Start, or the source just changed): a camera that did open and then sent no picture in time
/// after a reconnect is slow, not lacking the format, and is retried in the same format.
fn no_picture_as_format_problem(f: Failure, cap: &CaptureCfg, first_open: bool) -> Failure {
    match f {
        Failure::Retry(RetryKind::NoFrames, _) if first_open && !is_device_default(cap) => {
            Failure::Format(format!("the camera delivers no picture in {}", describe_cap(cap)))
        }
        other => other,
    }
}

/// Fill in what the fallback opened: the toast's "used", and the size in the settings that are in effect from now
/// on (the fields the window shows then match what runs, so setting them again is no change).
fn finish_fallback(fallback: Option<FormatFallback>, opened: &SourceInfo) -> Option<FormatFallback> {
    fallback.map(|mut f| {
        f.used = describe_source(opened);
        f.width = opened.width;
        f.height = opened.height;
        f.format = opened.format.clone();
        f.cfg.resolution = format!("{}x{}", opened.width, opened.height);
        // The rate that runs, as a frame interval (60 fps is 166667): the window shows it with the other fields.
        let fps = if (opened.fps - opened.fps.round()).abs() < 0.01 { opened.fps.round() } else { opened.fps };
        if fps > 0.0 {
            f.cfg.frame_interval = (10_000_000.0 / fps).round() as i64;
        }
        f
    })
}

/// The settings of the device default: the camera's first format at its highest rate.
fn device_default(cap: &CaptureCfg) -> CaptureCfg {
    CaptureCfg {
        res_type: ResType::Preferred,
        video_format: VideoFormat::Any,
        ..cap.clone()
    }
}

fn is_device_default(cap: &CaptureCfg) -> bool {
    cap.res_type == ResType::Preferred && cap.video_format == VideoFormat::Any
}

/// What the settings ask for, in words for a toast: `1920×1080 YUY2 60 fps`, `YUY2` or `device default`.
fn describe_cap(cap: &CaptureCfg) -> String {
    let format = match cap.video_format {
        VideoFormat::Any => None,
        VideoFormat::Format(f) => Some(f.name().to_uppercase()),
    };
    let size = (cap.res_type == ResType::Custom)
        .then(|| crate::config::parse_size(&cap.resolution).ok())
        .flatten()
        .map(|(w, h)| format!("{w}×{h}"));
    let fps = (cap.res_type == ResType::Custom && cap.frame_interval > 0).then(|| {
        let r = crate::config::ratio_from_interval(cap.frame_interval.min(u32::MAX as i64) as u32);
        format!("{} fps", rate_text(r.as_f64()))
    });
    let parts: Vec<String> = [size, format, fps].into_iter().flatten().collect();
    if parts.is_empty() {
        "device default".into()
    } else {
        parts.join(" ")
    }
}

/// `60`, `29.97`: whole rates without decimals.
fn rate_text(fps: f64) -> String {
    if (fps - fps.round()).abs() < 0.01 {
        format!("{}", fps.round() as u32)
    } else {
        format!("{fps:.2}")
    }
}

/// What a source delivers, for a toast: `1920×1080 MJPEG 60 fps`.
fn describe_source(i: &SourceInfo) -> String {
    format!(
        "{}×{} {} {} fps",
        i.width,
        i.height,
        i.format.to_uppercase(),
        rate_text(i.fps)
    )
}

/// Open a camera; when the video decoder fails at the open or at the first frame, open it once more with hardware
/// decoding off (plan 4.9), before anything else is given up: the next step, if the decoder fails again, is the format
/// fallback ([`open_with_fallback`]), so a decoder that cannot run the saved format counts as lacking it. But only
/// on the `first_open` of a Start: a camera that worked and then fails in the decoder after a reconnect is not
/// lacking the format; the failure is returned as a decoder retry (like any other generic failure, for a while).
fn open_retrying_decoder<T>(
    spec: &CameraSpec,
    first_open: bool,
    mut open: impl FnMut(&CameraSpec) -> Result<T, Failure>,
) -> Result<T, Failure> {
    match open(spec) {
        Err(Failure::Retry(RetryKind::Decoder, why)) if spec.hw_decode => {
            eprintln!("[engine] the video decoder failed ({why}); trying again with hardware decoding off");
            let software = CameraSpec {
                hw_decode: false,
                ..spec.clone()
            };
            match open(&software) {
                Err(Failure::Retry(RetryKind::Decoder, again)) if first_open => Err(Failure::Format(format!(
                    "the video decoder cannot decode this format, also with hardware decoding off ({again})"
                ))),
                other => other,
            }
        }
        // Hardware decoding was off already: nothing left to try for this format.
        Err(Failure::Retry(RetryKind::Decoder, why)) if first_open => Err(Failure::Format(format!(
            "the video decoder cannot decode this format ({why})"
        ))),
        other => other,
    }
}

/// Open with the settings; when the camera lacks the format, open once more with the device default (plan 4.9).
/// Returns the note for the toast and the settings now in effect when the fallback was used. A second failure to
/// find a format, or any other failure, is returned as it is.
fn open_with_fallback<T>(
    cap: &CaptureCfg,
    mut open: impl FnMut(&CaptureCfg) -> Result<T, Failure>,
) -> Result<(T, Option<FormatFallback>), Failure> {
    match open(cap) {
        Ok(t) => Ok((t, None)),
        Err(Failure::Format(why)) => {
            if is_device_default(cap) {
                return Err(Failure::Format(why));
            }
            let fallback = device_default(cap);
            eprintln!("[engine] {why}; using the device default");
            match open(&fallback) {
                Ok(t) => Ok((
                    t,
                    Some(FormatFallback {
                        asked: describe_cap(cap),
                        // what was opened is only known to the caller, which fills these in ([`finish_fallback`])
                        used: String::new(),
                        width: 0,
                        height: 0,
                        format: String::new(),
                        cfg: fallback,
                    }),
                )),
                Err(Failure::Format(second)) => Err(Failure::Format(format!(
                    "{why}. The device default does not work either: {second}"
                ))),
                Err(other) => Err(other),
            }
        }
        Err(other) => Err(other),
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
        // Latest-frame mailbox: the newest frame, whatever the queue's length; the older ones are counted as dropped.
        match self.reader.recv_newest(timeout) {
            Ok(f) => SourceEvent::Frame(InFrame::Camera(f, (self.info.width, self.info.height))),
            Err(RecvError::Timeout) => SourceEvent::Timeout,
            Err(RecvError::Stopped(r)) => SourceEvent::Stopped(Failure::from_stop(r)),
        }
    }

    fn counters(&self) -> (u64, u64) {
        let s = self.reader.stats();
        (s.produced, s.dropped)
    }

    fn controls(&self) -> Option<Arc<dyn CameraControls>> {
        self.controls
            .as_ref()
            .map(|c| c.clone() as Arc<dyn CameraControls>)
    }
}

fn open_camera(
    spec: &CameraSpec,
    gpu: &Arc<GpuDevice>,
    first_open: bool,
    stop: &Arc<AtomicBool>,
) -> Result<CameraInput, Failure> {
    let mut opts = OpenOptions::new(&spec.link, spec.request);
    opts.hw_decode = spec.hw_decode;
    opts.gpu = Some(gpu.clone());
    // The wait for the first frame (5 s) ends when the session is stopped.
    opts.cancel = Some(stop.clone());
    // A queue of one is the latest-frame mailbox: a newer frame replaces the one the render thread has not taken.
    opts.queue_len = 1;
    let reader = CaptureReader::open(opts).map_err(|e| Failure::from_capture_at(&e, first_open))?;
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
            fallback: None,
        },
        controls,
        reader,
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

/// When the synthetic source's frames are due, and the capture time each one carries.
///
/// A frame is stamped with the time it was *planned* for, not the time the thread woke up to make it: a late wake-up
/// (the machine was busy for 8 ms) otherwise leaves one stamp late and the next one on time, 8 ms closer together,
/// and the virtual camera drops a frame that is captured less than 0.75 of its period after the one before and
/// repeats a frame a period later. A real camera stamps frames at exposure time, which is the planned time too.
struct Schedule {
    /// The plan and the clock reading (100 ns, the capture layer's clock) it is counted from.
    origin: Instant,
    origin_100ns: i64,
    next_at: Instant,
    period: Duration,
}

impl Schedule {
    fn new(fps: u32, now: Instant, now_100ns: i64) -> Schedule {
        Schedule {
            origin: now,
            origin_100ns: now_100ns,
            next_at: now,
            period: Duration::from_secs_f64(1.0 / fps as f64),
        }
    }

    /// Hand out the next planned frame, made at `now` (not before `next_at`, unless the caller is late): its capture
    /// stamp in 100 ns units. The frame after it is due one period after this one was *planned*; a caller that is
    /// more than a period late has missed slots, which are not made up: the plan restarts from `now`.
    fn take(&mut self, now: Instant) -> i64 {
        let planned = self.next_at;
        self.next_at = (planned + self.period).max(now);
        self.origin_100ns + (planned.saturating_duration_since(self.origin).as_nanos() / 100) as i64
    }
}

struct SyntheticInput {
    spec: SyntheticSpec,
    info: SourceInfo,
    data: Arc<Vec<u8>>,
    stride: usize,
    schedule: Schedule,
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
                fallback: None,
            },
            spec,
            data: Arc::new(data),
            stride,
            schedule: Schedule::new(spec.fps, Instant::now(), qpc_now_100ns()),
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
        let next_at = self.schedule.next_at;
        if next_at > now + timeout {
            std::thread::sleep(timeout);
            return SourceEvent::Timeout;
        }
        if next_at > now {
            std::thread::sleep(next_at - now);
        }
        let timestamp_100ns = self.schedule.take(Instant::now());
        self.made += 1;
        SourceEvent::Frame(InFrame::Synthetic(SynthFrame {
            data: self.data.clone(),
            stride: self.stride as i32,
            width: self.spec.width,
            height: self.spec.height,
            format: self.spec.format,
            color: self.spec.color,
            timestamp_100ns,
            arrival_100ns: qpc_now_100ns(),
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

/// What the `controls auto` thread reads from the engine and where it reports (docs/AUTO-CONTROLS.md 5.5).
impl runner::Host for Shared {
    fn hub(&self) -> &Arc<ControlsHub> {
        &self.hub
    }

    fn facts(&self) -> crate::meter::SessionFacts {
        let cell = lock(&self.facts).clone();
        let facts = *lock(&cell);
        facts
    }

    fn sample(&self, timeout: Duration) -> Option<crate::meter::MeterSample> {
        self.meter.wait(timeout)
    }

    fn set_metering(&self, on: bool) {
        self.metering.store(on, Ordering::Release);
    }

    fn emit(&self, event: AutoStatusEvent) {
        emit(self, EngineEvent::AutoStatus(event));
    }
}

/// The engine's background thread for the controls: while the window is visible, every second the controls that are on
/// Auto are read again and, when one moved, sent as `controls-changed` (so sliders follow the device), and the
/// controls Better Cam auto drove (or whose mode changed) are sent at most four times a second; a hidden window
/// costs no device reads and no events, and gets the list once when it is visible again. A lease that has been idle
/// for a while is closed.
fn spawn_poller(shared: Arc<Shared>) -> std::io::Result<(Arc<AtomicBool>, JoinHandle<()>)> {
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let join = std::thread::Builder::new()
        .name("controls poll".into())
        .spawn(move || {
            ensure_com();
            let mut last_poll = Instant::now();
            let mut last_driven = Instant::now();
            let mut was_visible = false;
            while !flag.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(100));
                #[cfg(test)]
                while shared.poller_stall.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                shared.hub.expire_lease();
                let visible = shared.window_visible.load(Ordering::Acquire);
                let mut due = last_poll.elapsed() >= Duration::from_secs(1);
                if visible && !was_visible {
                    // The window is back and has not heard of the controls for a while: it gets them at once, changed
                    // or not.
                    shared.hub.request_resend();
                    due = true;
                }
                was_visible = visible;
                if visible && due {
                    last_poll = Instant::now();
                    last_driven = last_poll;
                    if let Some(infos) = shared.hub.poll() {
                        emit(&shared, EngineEvent::Controls(infos));
                    }
                }
                // What Better Cam auto wrote: four times a second at most.
                if visible && last_driven.elapsed() >= Duration::from_millis(250) {
                    if let Some(infos) = shared.hub.take_driven() {
                        last_driven = Instant::now();
                        emit(&shared, EngineEvent::Controls(infos));
                    }
                }
            }
        })?;
    Ok((stop, join))
}

// ---------------------------------------------------------------------------------------------------------------
// Statistics

/// Latency samples: a sliding window of the last [`WINDOW`] and a histogram of the whole run.
///
/// The histogram has [`TOTAL_BINS`] bins of [`BIN_MS`] up to [`TOTAL_RANGE_MS`] and one overflow bin, and the exact
/// maximum. A sample costs one increment and the numbers are read by scanning the bins (about 20 000, two or three
/// times a second), so neither depends on how long the run is, nothing is allocated after [`LatencyTrack::new`], and
/// the figures never freeze: the old store (every sample, sorted on each read, capped at 200 000) took 0.9 ms on the
/// render thread after ten minutes at 60 fps and 5.6 ms at the cap, reached after 55 minutes. A percentile is
/// the middle of its bin, so it is within half a bin (0.025 ms) of the sorted value; the maximum is exact.
struct LatencyTrack {
    window: VecDeque<f32>,
    /// Samples per bin; the last one counts everything at or above [`TOTAL_RANGE_MS`].
    bins: Box<[u32]>,
    count: u64,
    max: f64,
}

const WINDOW: usize = 240;
const BIN_MS: f64 = 0.05;
const TOTAL_RANGE_MS: f64 = 1000.0;
const TOTAL_BINS: usize = (TOTAL_RANGE_MS / BIN_MS) as usize;

impl Default for LatencyTrack {
    fn default() -> Self {
        LatencyTrack::new()
    }
}

impl LatencyTrack {
    fn new() -> LatencyTrack {
        LatencyTrack {
            window: VecDeque::with_capacity(WINDOW),
            bins: vec![0; TOTAL_BINS + 1].into_boxed_slice(),
            count: 0,
            max: 0.0,
        }
    }

    fn push(&mut self, ms: f64) {
        if self.window.len() == WINDOW {
            self.window.pop_front();
        }
        self.window.push_back(ms as f32);
        // A negative latency (a camera clock ahead of ours) counts as 0.
        let ms = ms.max(0.0);
        let bin = ((ms / BIN_MS) as usize).min(TOTAL_BINS);
        self.bins[bin] = self.bins[bin].saturating_add(1);
        self.count += 1;
        self.max = self.max.max(ms);
    }

    /// `(p50, p95, max)` of the window.
    fn window_stats(&self) -> (f64, f64, f64) {
        let n = self.window.len();
        let mut v = [0f32; WINDOW];
        for (slot, s) in v.iter_mut().zip(&self.window) {
            *slot = *s;
        }
        let v = &mut v[..n];
        v.sort_unstable_by(|a, b| a.total_cmp(b));
        (
            percentile(v, 0.5),
            percentile(v, 0.95),
            v.last().copied().unwrap_or(0.0) as f64,
        )
    }

    /// The `q` quantile of every sample so far (same rank rule as [`percentile`]), in milliseconds.
    fn total_quantile(&self, q: f64) -> f64 {
        if self.count == 0 {
            return 0.0;
        }
        // 1-based rank of the sample `percentile` would pick from the sorted run.
        let rank = ((self.count - 1) as f64 * q).ceil() as u64 + 1;
        let mut seen = 0u64;
        for (i, n) in self.bins.iter().enumerate() {
            seen += *n as u64;
            if seen >= rank {
                if i == TOTAL_BINS {
                    return self.max;
                }
                // The middle of the bin, but never past the largest sample.
                return ((i as f64 + 0.5) * BIN_MS).min(self.max);
            }
        }
        self.max
    }

    fn total_p50(&self) -> f64 {
        self.total_quantile(0.5)
    }

    fn total_p95(&self) -> f64 {
        self.total_quantile(0.95)
    }

    fn total_max(&self) -> f64 {
        self.max
    }
}

fn percentile(sorted: &[f32], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let i = ((sorted.len() - 1) as f64 * q).ceil() as usize;
    sorted[i.min(sorted.len() - 1)] as f64
}

/// Why a rendered frame is not encoded, read back and published.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Skip {
    /// No app reads the virtual camera.
    NoConsumer,
    /// The app reads at a lower rate and the virtual camera would drop the frame ([`Pacer`]).
    Rate,
}

/// A frame period in 100 ns units for a rate of `fps_num / fps_den`; `None` when the rate is 0 or unknown.
fn period_100ns(fps_num: u32, fps_den: u32) -> Option<i64> {
    (fps_num > 0 && fps_den > 0).then(|| 10_000_000 * fps_den as i64 / fps_num as i64)
}

/// The frame period of a consumer that negotiated `f`, as the virtual camera computes it (`crates/vcam-source/src/
/// frames.rs`: `freq / fps` in QPC ticks, `fps` the rate rounded to a whole number, at least 1, integer division).
/// `None` for a format without a usable rate.
fn dll_period(f: &Format, freq: u64) -> Option<i64> {
    let fps = f.fps();
    (fps > 0).then(|| (freq / fps.max(1) as u64) as i64)
}

/// The period of the consumer in QPC ticks, when it reads at a lower rate than the camera delivers (`camera_fps`, more
/// than 5 % lower): the frames for it are decimated ([`Pacer`]). `None` when it reads at the camera's rate or above.
fn lower_consumer_period(f: &Format, camera_fps: f64, freq: u64) -> Option<i64> {
    let theirs = dll_period(f, freq)?;
    (camera_fps > 0.0 && theirs as f64 / freq as f64 > 1.05 / camera_fps).then_some(theirs)
}

/// Decimation for a consumer that reads at a lower rate than the camera delivers (a 30 fps app on a 60 fps camera).
///
/// The virtual camera takes a frame only when it was captured at least 3/4 of its period after the last frame it took
/// ([`dll_min_gap`]; `crates/vcam-source/src/frames.rs`, `min_capture_gap`) and drops the others unseen, so
/// encoding, reading back and publishing them is work for nothing. The engine follows the same rule on the capture
/// times, but looser, so that nothing the virtual camera would have taken is ever held back. It runs on QPC ticks with
/// the virtual camera's own integer period (`freq / fps`, [`dll_period`]), so that no rounding through 100 ns units
/// makes it disagree with the virtual camera about a frame that sits right on the limit:
///
/// * [`Pacer::base`] is the capture time of the last frame the virtual camera would have taken from what was published:
///   the engine runs the virtual camera's rule itself on the frames it publishes.
/// * A frame is published unless it was captured less than 7/10 of the consumer's period after `base`
///   ([`engine_min_gap`]). Between 7/10 and 3/4 it is published although the virtual camera will drop it (it does not
///   move `base`); from 3/4 on, the virtual camera takes it and `base` moves to it.
///
/// By induction `base` is the virtual camera's own last taken frame, so the frames it takes from the published
/// sequence are exactly the frames it takes from the whole sequence (see the test
/// `the_pacer_never_holds_back_a_frame_the_virtual_camera_takes`). A time that goes backwards is taken (the virtual
/// camera takes it too). The first frame, and the first after the consumer changed or went away ([`Pacer::reset`]),
/// is always published.
///
/// **The virtual camera's bypass.** Past its repeat deadline (the last new frame it saw plus 1.5 periods) the virtual
/// camera takes the newest frame live even if the decimation would drop it, and stamps it a gap after the last one
/// (`crates/vcam-source/src/frames.rs`, `decimate`, `live_stamp`): a camera that stalled must not leave a stream
/// without a picture. The Pacer does not model that, and need not: a frame it holds back is one captured less than 7/10
/// of a period after `base`, which the virtual camera would have dropped unless its own stream had stalled for 1.5
/// periods, and then the newest frame it finds is the last one published, at most 0.7 of a period older than the one
/// held back. At worst a stall shows that older picture as a repeat; nothing is wasted the other way round, and `base`
/// is still the last frame the virtual camera took by the decimation rule, which is all the bypass leaves alone. So
/// "never holds back a frame the virtual camera takes" holds for the decimating path, and for the bypass it costs a
/// repeat of at most 0.7 of a period in a stall.
#[derive(Default)]
struct Pacer {
    base: Option<i64>,
}

/// The smallest capture-time distance between two frames the virtual camera takes, for a consumer period `period`: 3/4
/// of it, in the same integer arithmetic as the virtual camera's `min_capture_gap`.
fn dll_min_gap(period: i64) -> i64 {
    period * 3 / 4
}

/// What the engine holds back: a frame captured less than 7/10 of the consumer's period after [`Pacer::base`].
fn engine_min_gap(period: i64) -> i64 {
    period * 7 / 10
}

impl Pacer {
    fn reset(&mut self) {
        self.base = None;
    }

    /// Whether the frame captured at `capture` (100 ns) is worth publishing for a consumer whose frame period is
    /// `period` (100 ns).
    fn admit(&mut self, capture: i64, period: i64) -> bool {
        let Some(base) = self.base else {
            self.base = Some(capture);
            return true;
        };
        let gap = capture - base;
        if gap < 0 {
            self.base = Some(capture);
            return true;
        }
        if gap < engine_min_gap(period) {
            return false;
        }
        if gap >= dll_min_gap(period) {
            self.base = Some(capture);
        }
        true
    }
}

/// Counters for the one-second window.
struct Window {
    started: Instant,
    captured_at_start: u64,
    /// The longest a pass of the render loop took in this window, from taking the frame to the end of the pass
    /// (the pass that closes a window and does its statistics counts in the next one), milliseconds.
    worst_ms: f64,
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
            worst_ms: 0.0,
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

/// Replace the published statistics but keep the state the control thread owns. Nothing is stored by a session that
/// was abandoned (looked at under the lock that [`Control::abandon`] passes through after it sets the flag).
fn store_stats(shared: &Shared, abandoned: &AtomicBool, stats: &EngineStats) {
    let mut s = lock(&shared.stats);
    if abandoned.load(Ordering::Acquire) {
        return;
    }
    let state = s.state;
    *s = stats.clone();
    s.state = state;
}

/// Show `frame` to the tap (the preview), if there is one and the session is not being stopped.
///
/// Looked at under the tap's lock, which a rebuild of the device takes to hand the tap its new renderer
/// ([`Control::rebuild_gpu`]): a thread that was left behind and wakes in the middle of a frame must not call a tap
/// that has the next device's renderer.
fn tap_frame(shared: &Shared, stop: &AtomicBool, renderer: &Renderer, frame: &RgbaTexture, capture_qpc: u64) {
    if let Some(tap) = lock(&shared.tap).as_mut() {
        if !stop.load(Ordering::Acquire) {
            tap.on_frame(renderer, frame, capture_qpc);
        }
    }
}

/// Tell the tap (the preview) the frame's readback is done ([`FrameTap::frame_done`]), under the same lock and stop rule
/// as [`tap_frame`].
fn tap_done(shared: &Shared, stop: &AtomicBool, wait: bool) {
    if let Some(tap) = lock(&shared.tap).as_mut() {
        if !stop.load(Ordering::Acquire) {
            tap.frame_done(wait);
        }
    }
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

/// A GPU error as a failure of the session: a lost device is [`Failure::GpuLost`], anything else is fatal.
fn gpu_failure(renderer: &Renderer, e: GpuError) -> Failure {
    if e.is_device_lost() || renderer.gpu.removed_reason().is_some() {
        Failure::GpuLost(e.to_string())
    } else {
        Failure::Fatal(e.to_string())
    }
}

/// A failure of the camera side, or a lost GPU device when the device reports one (a stream that stops, an open that
/// fails: the decoder shares the device, so its failures are the device's).
fn device_check(renderer: &Renderer, f: Failure) -> Failure {
    match (&f, renderer.gpu.removed_reason()) {
        (Failure::GpuLost(_), _) => f,
        (_, Some(hr)) => Failure::GpuLost(format!("DXGI device removed ({})", hr_text(hr))),
        (_, None) => f,
    }
}

fn render_session(ctx: &SessionCtx) -> Result<(), Failure> {
    let SessionCtx {
        shared,
        renderer,
        writer,
        stop,
        removed,
        abandoned,
        ..
    } = ctx;
    let output_fps = lock(&shared.output).fps;
    // A failed open is looked at through the device first: with the device gone, the open fails with whatever code
    // the decoder or the media type happens to give, and none of them is about the camera or its format.
    let mut source: Box<dyn Source> = match ctx.factory.open(
        &ctx.source,
        output_fps,
        &renderer.gpu,
        ctx.first_open,
        stop,
    ) {
        Ok(s) => s,
        // A stopped open fails with whatever it was doing: the session is over, and the control thread has forgotten
        // it already.
        Err(_) if stop.load(Ordering::Acquire) => return Ok(()),
        Err(f) => return Err(device_check(renderer, f)),
    };
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
                i.fps,
            ),
        )
    };
    {
        // Under the lock, so that a session that was abandoned meanwhile ([`Control::abandon`] clears the link after
        // it sets the flag) does not announce its camera after that.
        let mut link = lock(&shared.camera_link);
        if !abandoned.load(Ordering::Acquire) {
            *link = capture_info.4.clone();
        }
    }
    // The colour space and range the user chose win over what the camera states; read again every frame, so a
    // change applies live.
    let effective_color = |user: ColorSettings| ColorSettings {
        space: user.space.or(camera_color.space),
        range: user.range.or(camera_color.range),
    };
    let mut color = effective_color(*lock(&shared.color));

    let mut converter =
        Converter::new(renderer.clone()).map_err(|e| gpu_failure(renderer, e))?;
    let stage = OutputStage::new(renderer.clone()).map_err(|e| gpu_failure(renderer, e))?;
    let mut chain = Chain::new();
    chain.set_assets(ctx.assets.clone());
    // The chain version this session has applied; the session starts with none, so the first frame applies it.
    let mut chain_gen = u64::MAX;
    // The preview point this session works with, and the version of it that was read (L1).
    let mut point = PreviewPoint::Output;
    let mut point_gen = u64::MAX;
    let mut output = OutputWriter::new(renderer.clone(), writer.clone())
        .map_err(|e| gpu_failure(renderer, e))?;
    let mut timer = GpuTimer::new(&renderer.gpu.device).ok();
    // The meter's GPU resources live and die with this session (a lost device ends it); what a session before it left
    // in the mailbox is gone, and the facts are this session's until it ends.
    let mut meter = crate::meter::Meter::new();
    let mut meter_failed = false;
    if !abandoned.load(Ordering::Acquire) {
        shared.meter.clear();
    }
    // The session's own cell, made current below: when this thread ends, late, its feed resets its own cell and not the
    // facts of the session that took its place.
    let facts_cell = Arc::new(Mutex::new(crate::meter::SessionFacts::CLOSED));
    let mut facts = crate::meter::FactsFeed::new(
        &facts_cell,
        _controls.as_ref().map(|c| c.token),
        source.info().fps,
    );
    {
        // Under the lock, so that a session that was abandoned meanwhile ([`Control::abandon`] replaces the cell after
        // it sets the flag) does not take the place again.
        let mut current = lock(&shared.facts);
        if !abandoned.load(Ordering::Acquire) {
            *current = facts_cell.clone();
        }
    }

    let mut published_any = false;
    // When the camera last delivered a frame, and whether the file shows the "starting" slate for a camera that has
    // been silent since ([`silent_slate_after`]), and the gaps the camera keeps between frames.
    let mut last_frame = Instant::now();
    let mut silent_slated = false;
    let mut frame_gaps = FrameGaps::default();
    // The consumer rate the pacer last saw; a change starts it over.
    let mut pacer = Pacer::default();
    let mut paced_for: Option<Format> = None;
    let mut gaps = Gaps::default();
    // The file's newest frame was forgotten ([`Writer::clear_latest`]) since the last frame that was published. The
    // file may hold the last frame of an earlier session (or of the app that left a moment ago) when this one starts.
    let mut latest_cleared = false;
    let mut was_paused = false;
    let mut window = Window::new(0);
    let mut latency = LatencyTrack::new();
    let mut pipeline = LatencyTrack::new();
    let mut stats = EngineStats {
        source: format!("{description}; color {}", describe_color(&color, src_h)),
        capture_width: capture_info.0,
        capture_height: capture_info.1,
        capture_format: capture_info.2,
        capture_rate: capture_info.5,
        decode: capture_info.3,
        ..EngineStats::default()
    };
    let (mut rendered, mut published) = (0u64, 0u64);
    let mut consecutive_errors = 0u32;
    let mut last_heartbeat = Instant::now();
    let mut first_error: Option<String> = None;
    // A stream that stops because the GPU device went (the decoder shares it) is a device loss, not a camera error.
    let gpu_check = |f: Failure| device_check(renderer, f);
    // The first published frame (or, with nobody reading the camera, the first rendered one) is the only thing that
    // sets the writer `Live` (see `Shared::live_gen`).
    let go_live = || {
        #[cfg(test)]
        {
            let delay = *lock(&shared.go_live_delay);
            if !delay.is_zero() {
                std::thread::sleep(delay);
            }
        }
        let mut live = lock(&shared.live_gen);
        // A session that is being stopped does not go live: the control thread sets the stop flag first and writes the
        // slate under this lock, so a first frame that comes late cannot put `Live` back over the slate.
        // (Nor once the app is quitting: [`Engine::close`] writes `Closed` under this lock after it set the flag.)
        if stop.load(Ordering::Acquire) || shared.exiting.load(Ordering::Acquire) {
            return;
        }
        *live = ctx.gen;
        writer.set_state(WriterState::Live);
    };
    // The `Paused` slate, written under the same lock and with the same look at the stop flag: a stop that comes first
    // wrote its own state (a quit: `Closed`) under that lock, and a thread that is still within its wait for the end
    // must not write over it.
    let write_paused = || {
        #[cfg(test)]
        {
            let delay = *lock(&shared.pause_write_delay);
            if !delay.is_zero() {
                std::thread::sleep(delay);
            }
        }
        let _live = lock(&shared.live_gen);
        if !stop.load(Ordering::Acquire) && !shared.exiting.load(Ordering::Acquire) {
            writer.set_state(WriterState::Paused);
        }
    };
    // The "starting" slate for a camera that went silent while live, under the same lock and with the same looks: a
    // stop, a quit or a pause that came first wrote its own state, which this must not replace.
    let write_starting = || {
        let _live = lock(&shared.live_gen);
        if !stop.load(Ordering::Acquire)
            && !shared.exiting.load(Ordering::Acquire)
            && !shared.paused.load(Ordering::Acquire)
        {
            writer.set_state(WriterState::Starting);
        }
    };

    store_stats(shared, abandoned, &stats);
    // The camera is open and (for a real camera) the first frame is in: the control thread may call us `Live`.
    let _ = ctx.events.send(SessionEvent {
        gen: ctx.gen,
        msg: SessionMsg::Opened {
            fallback: source.info().fallback.clone(),
        },
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
        advertise_session(shared, writer, (ow, oh, ofps, out_settings.extra_sizes));
        color = effective_color(*lock(&shared.color));

        // Pause: keep the camera open, read and drop its frames, show the paused slate.
        let paused = shared.paused.load(Ordering::Acquire);
        if paused != was_paused {
            was_paused = paused;
            stats.paused = paused;
            // The time between two publishes says nothing about lateness across a pause.
            gaps.reset();
            if paused {
                // Nothing is measured while paused, and what was in flight must not reach the controller after.
                if !abandoned.load(Ordering::Acquire) {
                    meter.reset(&shared.meter);
                }
            } else {
                // Resumed: the writer leaves `Paused` with the first frame published from here on.
                published_any = false;
                last_frame = Instant::now();
                silent_slated = false;
            }
        }
        facts.update(
            Instant::now(),
            || source.counters().0,
            paused,
            vcam_protocol::qpc_now(),
        );
        if paused {
            write_paused();
            let event = source.next(Duration::from_millis(40));
            // A thread that was stuck in the read and wakes after the stop (left behind, or about to be) does nothing
            // more: it shares the GPU context with the session that took its place.
            if stop.load(Ordering::Acquire) {
                break Ok(());
            }
            match event {
                SourceEvent::Frame(_) => last_frame = Instant::now(),
                SourceEvent::Timeout => {}
                SourceEvent::Stopped(f) => break Err(gpu_check(f)),
            }
            maybe_publish_stats(
                shared,
                abandoned,
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
                .map_err(|e| gpu_failure(renderer, e))?
            {
                note_published(
                    &p,
                    &mut published,
                    &mut published_any,
                    &go_live,
                    &mut latency,
                    &mut pipeline,
                    &mut window,
                    &mut gaps,
                );
            }
        }

        let event = source.next(Duration::from_millis(40));
        // Before any GPU work, statistics or tap call of this pass: a thread that was stuck in the read and wakes after
        // the stop (left behind, or about to be) shares the GPU context with the session that took its place, and
        // after a device rebuild its renderer is not the tap's any more.
        if stop.load(Ordering::Acquire) {
            break Ok(());
        }
        let frame = match event {
            SourceEvent::Frame(f) => {
                let now = Instant::now();
                frame_gaps.record(now.saturating_duration_since(last_frame));
                last_frame = now;
                silent_slated = false;
                f
            }
            SourceEvent::Timeout => {
                if last_heartbeat.elapsed() >= Duration::from_millis(100) {
                    writer.heartbeat();
                    last_heartbeat = Instant::now();
                }
                // A camera that went silent (the driver stopped sending, the stream is stuck): the file shows
                // "starting" instead of the last picture (or the paused slate, when it went silent during a pause)
                // for good, the next frame goes live again, and after a while the session ends so that the engine
                // reconnects. `last_frame` is set when the session opens and when a pause ends.
                let silent = last_frame.elapsed();
                let slate_after = silent_slate_after(source.info().fps, frame_gaps.largest());
                if !silent_slated && silent >= slate_after {
                    write_starting();
                    published_any = false;
                    silent_slated = true;
                }
                if silent_slated && silent >= silent_reconnect_after(slate_after) {
                    break Err(Failure::Retry(
                        RetryKind::NoFrames,
                        "the camera stopped sending pictures".into(),
                    ));
                }
                maybe_publish_stats(
                    shared,
                    abandoned,
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
            SourceEvent::Stopped(failure) => break Err(gpu_check(failure)),
        };
        let (capture_100ns, arrival_100ns) = frame.times();
        let capture_qpc = vcam_protocol::hns_to_qpc(capture_100ns);
        let cpu_started = Instant::now();

        // Who reads the virtual camera, read once for the whole frame: the format to encode, and whether the frame
        // goes to the file at all. A frame nobody takes is still rendered (the preview shows it) but not encoded,
        // read back or published: with no app reading the camera there is nothing to publish to, and an app that
        // reads at a lower rate than the camera would have the virtual camera drop it unseen ([`Pacer`]).
        let negotiated = writer.negotiated();
        let watched = negotiated.is_some() || writer.consumer_active();
        let target = target_from(negotiated, (ow, oh));
        let mut skip = None;
        if !watched {
            pacer.reset();
            paced_for = None;
            // Nothing is published, so no gap between publishes is looked at when an app comes back.
            gaps.reset();
            skip = Some(Skip::NoConsumer);
            if !latest_cleared {
                // With nobody reading, the file's newest frame is the last one of the app that left (or of an earlier
                // session): the first frame the next app takes must be a new one, so the old one is forgotten, and so
                // are the frames still in flight, which would put another old one there.
                output.clear();
                writer.clear_latest();
                latest_cleared = true;
            }
        } else {
            latest_cleared = false;
            if paced_for != negotiated {
                // A new consumer or format: its first frame is published at once.
                pacer.reset();
                gaps.reset();
                paced_for = negotiated;
            }
            // The consumer's period in the virtual camera's own units and arithmetic (QPC ticks, `freq / fps`), when
            // it reads at a lower rate than the camera delivers.
            let lower = negotiated.and_then(|f| {
                lower_consumer_period(&f, source.info().fps, vcam_protocol::qpc_frequency())
            });
            // What a publish may be behind its frame's capture time, compared with the one before, before it counts
            // as late: half the frame period of the app (of the camera while the app's rate is not known).
            gaps.period = negotiated
                .and_then(|f| period_100ns(f.fps_num, f.fps_den))
                .unwrap_or_else(|| (10_000_000.0 / source.info().fps.max(1.0)) as i64);
            match lower {
                Some(period) if !pacer.admit(capture_qpc as i64, period) => skip = Some(Skip::Rate),
                Some(_) => {}
                None => pacer.reset(),
            }
        }
        let mut submitted = false;

        // Everything from here to the readback submit is one frame of GPU work.
        let timing = timer
            .as_mut()
            .is_some_and(|t| t.begin(&renderer.gpu.context));
        let rendered_frame = (|| -> Result<(), GpuError> {
            #[cfg(test)]
            if shared
                .gpu_faults
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok()
            {
                // A real loss leaves the device reporting it from then on.
                renderer
                    .gpu
                    .simulate_removed
                    .store(true, Ordering::SeqCst);
                return Err(GpuError::Api(windows::core::Error::from(
                    windows::Win32::Graphics::Dxgi::DXGI_ERROR_DEVICE_REMOVED,
                )));
            }
            let rgba = {
                let src = frame.source_frame(color)?;
                converter.convert(&src, shared.flip.load(Ordering::Relaxed))?
            };
            // The decoder surface was copied by the pass above; give it back before the rest of the pipeline.
            drop(frame);
            // The meter sees what the sensor delivers (converted, before any filter). A failure of it is not the
            // frame's, unless the device is gone.
            // (Not at all once the session is abandoned: its samples are not for whoever reads the mailbox now. The
            // controller drops a sample of another session by its token as well.)
            if let Err(e) = if abandoned.load(Ordering::Acquire) {
                Ok(())
            } else {
                meter.frame(
                    renderer,
                    &rgba,
                    shared.metering.load(Ordering::Acquire),
                    facts.token().unwrap_or(0),
                    capture_qpc,
                    cpu_started,
                    &shared.meter,
                )
            } {
                if e.is_device_lost() || renderer.gpu.removed_reason().is_some() {
                    return Err(e);
                }
                if !abandoned.load(Ordering::Acquire) {
                    meter.reset(&shared.meter);
                }
                if !std::mem::replace(&mut meter_failed, true) {
                    eprintln!("[engine] meter: {e}");
                }
            }
            // The point's version is read before the chain's: a point sent after a chain sees that chain.
            let wanted_point = shared.preview_point_gen.load(Ordering::Acquire);
            // The newest chain applies before this frame.
            let wanted = shared.chain_gen.load(Ordering::Acquire);
            if wanted != chain_gen {
                chain_gen = wanted;
                let cfg = lock(&shared.chain).clone();
                chain.apply(renderer, &cfg);
            }
            if wanted_point != point_gen {
                point_gen = wanted_point;
                point = lock(&shared.preview_point).clone();
            }
            if let PreviewPoint::Filter(id) = &point {
                if !chain.has_filter(id) {
                    // The filter was removed (or never was a filter): the preview is the output again.
                    let mut shared_point = lock(&shared.preview_point);
                    if *shared_point == point {
                        *shared_point = PreviewPoint::Output;
                        shared.preview_point_gen.fetch_add(1, Ordering::Release);
                    }
                    point = PreviewPoint::Output;
                }
            }
            // The filters that scale to the "base (canvas) resolution" use the output size.
            chain.set_canvas((target.width, target.height));
            // The preview shows the picture at its point while a gesture runs; the frame goes on unchanged.
            let mut tapped = false;
            let rgba = chain.run_tapped(renderer, rgba, Instant::now(), &point, &mut |tex| {
                tapped = true;
                tap_frame(shared, stop, renderer, tex, capture_qpc);
            })?;
            let fitted = stage.run(
                &rgba,
                &OutputParams {
                    width: target.width,
                    height: target.height,
                    fit: out_settings.fit,
                    background: out_settings.background.map(|c| c as f32 / 255.0),
                },
            )?;
            // The output frame is the preview's picture unless a point showed it another one this frame.
            if !tapped {
                tap_frame(shared, stop, renderer, &fitted, capture_qpc);
            }
            if skip.is_none() {
                output.submit(
                    &fitted,
                    target,
                    FrameTag {
                        capture_qpc,
                        capture_100ns,
                        arrival_100ns,
                    },
                )?;
                submitted = true;
            }
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
        // (A session that is being stopped says nothing more: the next one reports for itself.)
        let stopping = stop.load(Ordering::Acquire);
        for e in chain.take_errors() {
            if !stopping {
                emit(shared, EngineEvent::FilterError(e));
            }
        }
        for s in chain.take_status() {
            if !stopping {
                emit(shared, EngineEvent::FilterStatus(s));
            }
        }
        match rendered_frame {
            Ok(()) => {
                consecutive_errors = 0;
                rendered += 1;
                window.rendered += 1;
                window.cpu_sum += cpu_started.elapsed().as_secs_f64() * 1000.0;
                window.cpu_n += 1;
                match skip {
                    Some(Skip::NoConsumer) => {
                        stats.skipped_no_consumer += 1;
                        // Nobody reads the file, so nothing is published, and the writer still goes `Live` with the
                        // first frame the engine renders: the camera is open and frames flow, and the first one
                        // after an app attaches is published at once.
                        if !published_any {
                            published_any = true;
                            go_live();
                        }
                    }
                    Some(Skip::Rate) => stats.skipped_rate += 1,
                    None => {}
                }
                if skip.is_some() {
                    output.skip();
                }
            }
            Err(e) => {
                if e.is_device_lost() || renderer.gpu.removed_reason().is_some() {
                    break Err(Failure::GpuLost(e.to_string()));
                }
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
                        &go_live,
                        &mut latency,
                        &mut pipeline,
                        &mut window,
                        &mut gaps,
                    );
                }
                Ok(None) => {}
                Err(e) => {
                    if e.is_device_lost() || renderer.gpu.removed_reason().is_some() {
                        break Err(Failure::GpuLost(e.to_string()));
                    }
                    stats.errors += 1;
                    first_error.get_or_insert(e.to_string());
                }
            },
            ReadbackPolicy::NextFrame => {
                // Anything the GPU already finished goes out now; the rest on the next pass of the loop.
                while let Some(p) = output.poll(false).map_err(|e| gpu_failure(renderer, e))? {
                    window.readback_sum += p.copy_ms;
                    note_published(
                        &p,
                        &mut published,
                        &mut published_any,
                        &go_live,
                        &mut latency,
                        &mut pipeline,
                        &mut window,
                        &mut gaps,
                    );
                }
            }
        }
        // The preview's picture is mapped now, after the readback above: under the default policy that waited for the
        // GPU, which had the preview's copy queued ahead of the output's, so the preview shows this frame. Where the
        // engine did not wait (next-frame policy with an app reading) the preview does not either.
        tap_done(
            shared,
            stop,
            matches!(ctx.readback, ReadbackPolicy::Immediate) || skip.is_some(),
        );
        // Draw calls report nothing when the device is gone; this does (one cheap call a frame).
        if let Some(hr) = renderer.gpu.removed_reason() {
            break Err(Failure::GpuLost(format!(
                "DXGI device removed ({})",
                hr_text(hr)
            )));
        }
        // A publish beats the file's heartbeat itself; a frame that was not published (nobody reads the file, or the
        // frame would be dropped) leaves it to this, so the virtual camera never sees a stale heartbeat.
        if submitted {
            last_heartbeat = Instant::now();
        } else if last_heartbeat.elapsed() >= Duration::from_millis(100) {
            writer.heartbeat();
            last_heartbeat = Instant::now();
        }
        // A pause that arrived while this frame was in flight must not be undone by its first publish.
        if shared.paused.load(Ordering::Acquire) {
            write_paused();
        }
        stats.dropped_output = output.dropped + output.rejected;
        stats.late_publishes = gaps.late;
        maybe_publish_stats(
            shared,
            abandoned,
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
        window.worst_ms = window.worst_ms.max(cpu_started.elapsed().as_secs_f64() * 1000.0);
        // Measurement hook ([`Engine::prefill_latency`]), after the pass was timed.
        let n = shared.prefill_latency.swap(0, Ordering::AcqRel);
        for i in 0..n {
            // Scattered over 1 to 6 ms (a repeating pattern would sort faster than real latencies do).
            let ms = 1.0 + (i.wrapping_mul(2_654_435_761) >> 8) as f64 % 5000.0 * 0.001;
            latency.push(ms);
            pipeline.push(ms);
        }
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

/// Publishes that came late: the virtual camera repeats the last picture when no new one comes within 1.5 frame periods
/// of the app, so a publish that comes later than its frame's capture time says it should is a repeat.
///
/// A publish is late when the time since the publish before it exceeds the time between the capture of the two
/// frames by more than half a period of the app ([`Gaps::period`]): the engine kept the frame for that much longer than
/// the camera made it wait. The gap between the publishes alone says nothing (a camera below its nominal rate, or one
/// that stalls, has big gaps and a pipeline that keeps up), and a gap that spans a time nothing was published (no app
/// reading, a pause, another format) is not a lateness at all: [`Gaps::reset`] forgets the publish before.
#[derive(Default)]
struct Gaps {
    /// `(publish time, capture time)` of the last publish, 100 ns.
    last: Option<(i64, i64)>,
    /// The frame period of the app (the camera's while it has not said), 100 ns; 0 until known.
    period: i64,
    /// Publishes that came more than half a period later, against the frames' capture times, than the one before.
    late: u64,
}

impl Gaps {
    fn note(&mut self, published: i64, captured: i64) {
        if let Some((last_pub, last_cap)) = self.last {
            let behind = (published - last_pub) - (captured - last_cap);
            if self.period > 0 && behind > self.period / 2 {
                self.late += 1;
            }
        }
        self.last = Some((published, captured));
    }

    /// Nothing was published for a while (or what was is not comparable): the next publish is the first of a run.
    fn reset(&mut self) {
        self.last = None;
    }
}

#[allow(clippy::too_many_arguments)]
fn note_published(
    p: &Published,
    published: &mut u64,
    published_any: &mut bool,
    go_live: &dyn Fn(),
    latency: &mut LatencyTrack,
    pipeline: &mut LatencyTrack,
    window: &mut Window,
    gaps: &mut Gaps,
) {
    if !*published_any {
        *published_any = true;
        go_live();
    }
    *published += 1;
    window.published += 1;
    let done = qpc_to_hns(p.publish_qpc);
    gaps.note(done, p.tag.capture_100ns);
    latency.push((done - p.tag.capture_100ns) as f64 / 10_000.0);
    pipeline.push((done - p.tag.arrival_100ns) as f64 / 10_000.0);
}

#[allow(clippy::too_many_arguments)]
fn maybe_publish_stats(
    shared: &Shared,
    abandoned: &AtomicBool,
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
    let closing = Instant::now();
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
    stats.worst_frame_ms = window.worst_ms;
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
    stats.total_latency_p50_ms = latency.total_p50();
    stats.total_latency_p95_ms = latency.total_p95();
    stats.total_latency_max_ms = latency.total_max();
    stats.total_pipeline_p95_ms = pipeline.total_p95();
    stats.consumer = output.writer().consumer_active();
    // What closing this window cost the render thread (the percentiles above); the next window reports it.
    stats.stats_ms = closing.elapsed().as_secs_f64() * 1000.0;
    store_stats(shared, abandoned, stats);
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
        // An app reads the virtual camera (nothing is published for nobody), at the configured size as NV12.
        let consumer = Consumer::start_with(&path, Format::new(640, 360, 30, SUBTYPE_NV12));

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

        // The configured size as NV12, equal to the source bars.
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

        // The consumer asks for 320x180 RGB32: the engine follows the next frame.
        consumer
            .file
            .shared()
            .set_negotiated(&Format::new(320, 180, 30, SUBTYPE_RGB32));
        let deadline = Instant::now() + Duration::from_secs(10);
        let frame = loop {
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

        drop(consumer);
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

    /// Records what the preview would get: how many pictures, the size of the last and its widest channel spread (0 is
    /// grey, the colour bars are far above 100).
    struct ProbeTap(Probe);

    /// `(pictures, size of the last, spread of the last)`.
    type Probe = Arc<Mutex<(u64, (u32, u32), u8)>>;

    impl FrameTap for ProbeTap {
        fn on_frame(&mut self, r: &Renderer, f: &RgbaTexture, _: u64) {
            let px = crate::gpu::pipeline_tests::read_rgba(r, f);
            let spread = px
                .chunks(4)
                .map(|p| p[..3].iter().max().unwrap() - p[..3].iter().min().unwrap())
                .max()
                .unwrap_or(0);
            let mut g = self.0.lock().unwrap();
            *g = (g.0 + 1, (f.width, f.height), spread);
        }
    }

    /// UX-PASS L1: while a point is set the preview gets the picture at that point and apps still get the output.
    #[test]
    fn the_preview_point_shows_the_picture_before_the_filter_and_apps_still_get_the_output() {
        let rig = Rig::new("sm-point.bin", fast());
        let _consumer = rig.consumer("sm-point.bin");
        let probe = Arc::new(Mutex::new((0u64, (0u32, 0u32), 0u8)));
        rig.engine.send(Msg::SetTap(Some(Box::new(ProbeTap(probe.clone())))));
        let chain = |second: bool| {
            let mut v = vec![serde_json::json!({ "id": "g", "kind": "color_filter", "enabled": true, "params": { "saturation": -1 } })];
            if second {
                // switched off: its point is still the picture that would enter it
                v.push(serde_json::json!({ "id": "h", "kind": "color_filter", "enabled": false, "params": { "gamma": 0.5 } }));
            }
            crate::fx::parse_chain(&serde_json::Value::Array(v)).unwrap()
        };
        let file_spread = |rig: &Rig| -> Option<u8> {
            let (w, h, _, buf) = latest(&rig.file)?;
            let rgb = nv12_to_rgb(&buf, w, h, w as usize, ColorSpace::Bt709, ColorRange::Limited);
            rgb.chunks(3).map(|p| p.iter().max().unwrap() - p.iter().min().unwrap()).max()
        };
        let tap_spread = || probe.lock().unwrap().2;
        let point_is = |rig: &Rig, p: PreviewPoint| rig.wait("the point", Duration::from_secs(10), |r| r.engine.preview_point() == p);

        rig.engine.set_chain(chain(true));
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        rig.wait("grey output in the tap and the file", Duration::from_secs(10), |r| {
            tap_spread() <= 4 && file_spread(r).is_some_and(|s| s <= 4)
        });

        // The camera's picture, before the chain: colour in the preview, grey for the app.
        rig.engine.set_preview_point(PreviewPoint::Input);
        rig.wait("colour in the tap", Duration::from_secs(10), |_| tap_spread() > 100);
        assert!(file_spread(&rig).is_some_and(|s| s <= 4), "apps still get the grey output");
        assert_eq!(rig.engine.preview_point(), PreviewPoint::Input);

        // Entering the first filter: the camera's picture; entering the second, which is off: what the first made.
        rig.engine.set_preview_point(PreviewPoint::Filter("h".into()));
        rig.wait("grey before h", Duration::from_secs(10), |_| tap_spread() <= 4);
        rig.engine.set_preview_point(PreviewPoint::Filter("g".into()));
        rig.wait("colour before g", Duration::from_secs(10), |_| tap_spread() > 100);
        assert!(file_spread(&rig).is_some_and(|s| s <= 4));

        // One picture per frame, and none from the output stage meanwhile (it is the same count as the frames rendered).
        let (n0, rendered0) = (probe.lock().unwrap().0, rig.engine.stats().rendered);
        rig.wait("frames", Duration::from_secs(10), |r| r.engine.stats().rendered >= rendered0 + 15);
        let (n1, rendered1) = (probe.lock().unwrap().0, rig.engine.stats().rendered);
        assert!(n1 - n0 <= rendered1 - rendered0 + 1, "{} pictures for {} frames", n1 - n0, rendered1 - rendered0);

        // A filter that is not in the chain: the engine takes the point back, and the output is what the tap gets.
        rig.engine.set_preview_point(PreviewPoint::Filter("nope".into()));
        point_is(&rig, PreviewPoint::Output);
        rig.wait("grey output", Duration::from_secs(10), |_| tap_spread() <= 4);

        // Removing the filter the point names takes it back too.
        rig.engine.set_preview_point(PreviewPoint::Filter("h".into()));
        rig.wait("grey before h", Duration::from_secs(10), |_| tap_spread() <= 4);
        point_is(&rig, PreviewPoint::Filter("h".into()));
        rig.engine.set_chain(chain(false));
        point_is(&rig, PreviewPoint::Output);

        // A point set in the same breath as the chain that has the filter is valid (the messages keep their order).
        rig.engine.set_chain(chain(true));
        rig.engine.set_preview_point(PreviewPoint::Filter("h".into()));
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(rig.engine.preview_point(), PreviewPoint::Filter("h".into()));

        // The window is hidden: the gesture is over.
        rig.engine.set_window_visible(true);
        rig.engine.set_preview_point(PreviewPoint::Input);
        point_is(&rig, PreviewPoint::Input);
        rig.engine.set_window_visible(false);
        point_is(&rig, PreviewPoint::Output);
        assert!(file_spread(&rig).is_some_and(|s| s <= 4));
        assert_eq!(rig.opens(), 1, "no reopen");
    }

    #[test]
    fn set_chain_applies_live_and_survives_a_stop() {
        let rig = Rig::new("sm-chain.bin", fast());
        // The frames are looked at in the file, which only gets frames while an app reads the camera.
        let _consumer = rig.consumer("sm-chain.bin");
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
            error_cap: Duration::from_millis(1500),
            stable_after: Duration::from_millis(600),
            gpu_failure_window: Duration::from_secs(5),
            gpu_slate_delay: Duration::from_secs(1),
            gpu_rebuild_timeout: Duration::from_secs(10),
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
        /// Windows' camera privacy switch is off: opens fail with "access denied".
        privacy: AtomicBool,
        /// Open sources fail with this stream error while it is set (a hiccup of the driver).
        stream_error: Arc<AtomicBool>,
        /// The next source to read fails with the stream error once.
        stream_error_once: Arc<AtomicBool>,
        /// The stream error that `stream_error_once` makes comes with a lost GPU device (the same read: no frame
        /// is drawn in between to find the loss first).
        lose_device_with_error: Arc<AtomicBool>,
        /// How long an open takes.
        open_delay: Mutex<Duration>,
        /// How long closing a source takes (a camera that is slow to close, a control write that hangs).
        close_delay: Arc<Mutex<Duration>>,
        /// The flag the sources opened from now on stall on: while it is set, their `next` does not return (a read
        /// that is stuck in the driver, and does not look at the stop flag).
        stall: Mutex<Arc<Stall>>,
        /// While set, open sources deliver no frame and answer every read with a timeout (a camera whose driver stopped
        /// sending while the stream stays open).
        silent: Arc<AtomicBool>,
        /// While not zero, open sources deliver one frame every this many milliseconds (a long exposure), whatever
        /// their nominal rate says.
        frame_every_ms: Arc<std::sync::atomic::AtomicU64>,
        /// An open that has this many steps of this length, each only begun when the session is not stopped.
        open_steps: Mutex<(u32, Duration)>,
        /// The steps of `open_steps` that were begun.
        steps_taken: std::sync::atomic::AtomicU32,
        /// The rate the next opens report as the camera's (the media type's), whatever it really makes.
        nominal_fps: Mutex<Option<f64>>,
        /// Formats the camera does not list: opening a `SourceSpec::Configured` that names one fails as a format.
        /// `VideoFormat::Any` here is a camera that offers nothing the engine can read.
        lacks: Mutex<Vec<VideoFormat>>,
        /// The `first_open` flag of every open, in order.
        first_opens: Mutex<Vec<bool>>,
        /// A `SourceSpec::Configured` open of a camera that "sends no picture" in a saved format fails like the
        /// real factory does (a no-frames failure, a format failure on the first open of a source only).
        silent_formats: Mutex<Vec<VideoFormat>>,
        /// While set, an open waits for a message (or for the sender to go away) before it goes on.
        open_gate: Mutex<Option<Mutex<std::sync::mpsc::Receiver<()>>>>,
        /// Whether the device the open was handed was removed, for every open.
        opened_on_removed: Mutex<Vec<bool>>,
        /// The next open loses the GPU device while it runs (once).
        lose_device_in_open: AtomicBool,
        /// The registry says the privacy switch is off, but opens work (Windows does not enforce it for every app).
        registry_deny: AtomicBool,
        /// The programs the registry says use a camera (`SourceFactory::camera_users`).
        users: Mutex<Vec<String>>,
        /// `camera_users` goes to the real lookup (registry and camera log, both moved to test sources by the caller).
        real_users: AtomicBool,
        /// The target link of every `camera_users` call.
        targets: Mutex<Vec<Option<String>>>,
        /// The next opens fail like the real factory's with these `(call, HRESULT)` of Media Foundation, in order
        /// (`Failure::from_capture_at` and `open_retrying_decoder` with the open's `first_open`).
        capture_errors: Mutex<VecDeque<(&'static str, u32)>>,
        /// The picture of the next opens is one flat grey (an 8-bit coded level, NV12) instead of the bars.
        flat: Mutex<Option<u8>>,
        /// The picture of the next opens is a flat grey whose level this function gives for every frame (a camera
        /// whose picture follows its exposure and gain).
        live: Mutex<Option<LiveLevel>>,
    }

    type LiveLevel = Arc<dyn Fn() -> u8 + Send + Sync>;

    /// The symbolic link the fake camera has.
    const FAKE_LINK: &str = "\\\\?\\usb#vid_0c45&pid_636d&mi_00#9&3171a958&0&0000#{e5323777-f976-4f5b-9b55-b94699c46e44}\\global";

    struct FakeSource {
        inner: SyntheticInput,
        info: SourceInfo,
        controls: Arc<FakeControls>,
        gone: Arc<AtomicBool>,
        stream_error: Arc<AtomicBool>,
        stream_error_once: Arc<AtomicBool>,
        lose_device_with_error: Arc<AtomicBool>,
        gpu: Arc<GpuDevice>,
        alive: Arc<std::sync::atomic::AtomicU32>,
        live: Option<LiveLevel>,
        close_delay: Arc<Mutex<Duration>>,
        stall: Arc<Stall>,
        silent: Arc<AtomicBool>,
        frame_every_ms: Arc<std::sync::atomic::AtomicU64>,
        last_sent: Option<Instant>,
    }

    /// A read that is stuck in the driver: while `on` is set the source's `next` does not return, and it does not
    /// look at the stop flag.
    #[derive(Default)]
    struct Stall {
        on: AtomicBool,
        /// A `next` is stuck right now.
        inside: AtomicBool,
    }

    impl Stall {
        /// Stick the next read of the sources that were opened with this and wait until one is stuck.
        fn hold(&self) {
            self.on.store(true, Ordering::SeqCst);
            let t = Instant::now();
            while !self.inside.load(Ordering::SeqCst) {
                assert!(t.elapsed() < Duration::from_secs(5), "no read got stuck");
                std::thread::sleep(Duration::from_millis(2));
            }
        }

        fn release(&self) {
            self.on.store(false, Ordering::SeqCst);
        }
    }

    impl Drop for FakeSource {
        fn drop(&mut self) {
            let delay = *self.close_delay.lock().unwrap();
            if !delay.is_zero() {
                std::thread::sleep(delay);
            }
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
            if self.stall.on.load(Ordering::SeqCst) {
                self.stall.inside.store(true, Ordering::SeqCst);
                while self.stall.on.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                self.stall.inside.store(false, Ordering::SeqCst);
            }
            if self.gone.load(Ordering::SeqCst) {
                return SourceEvent::Stopped(Failure::disconnected(None));
            }
            if self.silent.load(Ordering::SeqCst) {
                std::thread::sleep(timeout);
                return SourceEvent::Timeout;
            }
            // A camera at a long exposure: a frame only when this long has passed since the last one.
            let every = Duration::from_millis(self.frame_every_ms.load(Ordering::SeqCst));
            if !every.is_zero() {
                if let Some(due) = self.last_sent.map(|t| t + every).filter(|due| *due > Instant::now()) {
                    std::thread::sleep(timeout.min(due.saturating_duration_since(Instant::now())));
                    return SourceEvent::Timeout;
                }
                self.last_sent = Some(Instant::now());
            }
            if self.stream_error.load(Ordering::SeqCst) || self.stream_error_once.swap(false, Ordering::SeqCst) {
                if self.lose_device_with_error.swap(false, Ordering::SeqCst) {
                    self.gpu.simulate_removed.store(true, Ordering::SeqCst);
                }
                return SourceEvent::Stopped(Failure::from_stop(StopReason::Error(HRESULT(
                    0xC00D_3E8C_u32 as i32,
                ))));
            }
            if let Some(level) = self.live.as_ref().map(|f| f()) {
                let (w, h) = (self.inner.spec.width, self.inner.spec.height);
                let (space, range) = self.inner.spec.color.resolve(h);
                let rgb = vec![level; w as usize * h as usize * 3];
                self.inner.data = Arc::new(pattern::pack_nv12(&rgb, w, h, w as usize, h, space, range));
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
            gpu: &Arc<GpuDevice>,
            first_open: bool,
            stop: &Arc<AtomicBool>,
        ) -> Result<Box<dyn Source>, Failure> {
            self.opens.fetch_add(1, Ordering::SeqCst);
            self.specs.lock().unwrap().push(spec.clone());
            self.first_opens.lock().unwrap().push(first_open);
            // What a camera does when it is handed a device that is gone (the real factory fails to set the media
            // type, which `from_capture` used to call a format problem), and one that goes while it opens.
            let removed = gpu.removed_reason().is_some();
            self.opened_on_removed.lock().unwrap().push(removed);
            if removed || self.lose_device_in_open.swap(false, Ordering::SeqCst) {
                gpu.simulate_removed.store(true, Ordering::SeqCst);
                return Err(Failure::Format("SetCurrentMediaType failed: the device was removed".into()));
            }
            let delay = *self.open_delay.lock().unwrap();
            if !delay.is_zero() {
                std::thread::sleep(delay);
            }
            // An open of several steps that looks at the stop flag between them, as the real factory does (the
            // decoder retry, the format fallback): each step takes `step`.
            let (steps, step) = *self.open_steps.lock().unwrap();
            for _ in 0..steps {
                open_unless_stopped(stop, || {
                    self.steps_taken.fetch_add(1, Ordering::SeqCst);
                    std::thread::sleep(step);
                    Ok(())
                })?;
            }
            if let Some(gate) = self.open_gate.lock().unwrap().as_ref() {
                let _ = gate.lock().unwrap().recv();
            }
            if let Some(f) = self.failures.lock().unwrap().pop_front() {
                return Err(f);
            }
            if let Some((what, code)) = self.capture_errors.lock().unwrap().pop_front() {
                let spec = CameraSpec {
                    link: FAKE_LINK.into(),
                    request: FormatRequest { format: PixelFormat::Mjpeg, size: None, fps: None },
                    hw_decode: true,
                };
                let failed = open_retrying_decoder(&spec, first_open, |_| -> Result<(), Failure> {
                    let error = windows::core::Error::from(HRESULT(code as i32));
                    Err(Failure::from_capture_at(&CaptureError::Api { what, error }, first_open))
                });
                return Err(failed.unwrap_err());
            }
            if self.gone.load(Ordering::SeqCst) {
                return Err(Failure::Retry(
                    RetryKind::NotFound,
                    "camera not found".into(),
                ));
            }
            if self.privacy.load(Ordering::SeqCst) {
                return Err(Failure::in_use(Some(windows::Win32::Foundation::E_ACCESSDENIED)));
            }
            let (synthetic, fallback) = match spec {
                SourceSpec::Synthetic(s) => (*s, None),
                // The fake camera has one picture (320x180 NV12 at 30) and lacks the formats in `lacks`.
                SourceSpec::Configured(cap) => {
                    let (s, fb) = open_with_fallback(cap, |c| {
                        if self.lacks.lock().unwrap().contains(&c.video_format) {
                            return Err(Failure::Format(format!(
                                "camera does not list {}",
                                describe_cap(c)
                            )));
                        }
                        if self.silent_formats.lock().unwrap().contains(&c.video_format) {
                            // What `DefaultFactory` makes of a camera that opens and sends nothing.
                            return Err(no_picture_as_format_problem(
                                Failure::from_capture(&CaptureError::Timeout("the first frame")),
                                c,
                                first_open,
                            ));
                        }
                        match bars(30) {
                            SourceSpec::Synthetic(s) => Ok(s),
                            _ => unreachable!(),
                        }
                    })?;
                    (s, fb)
                }
                SourceSpec::Camera(_) => {
                    return Err(Failure::Fatal("the fake opens synthetic sources".into()))
                }
            };
            self.alive.fetch_add(1, Ordering::SeqCst);
            let mut inner = SyntheticInput::new(synthetic)?;
            if let (Some(level), PixelFormat::Nv12) = (*self.flat.lock().unwrap(), synthetic.format) {
                let (w, h) = (synthetic.width, synthetic.height);
                let (space, range) = synthetic.color.resolve(h);
                let rgb = vec![level; w as usize * h as usize * 3];
                inner.data = Arc::new(pattern::pack_nv12(&rgb, w, h, w as usize, h, space, range));
            }
            let mut info = SourceInfo {
                link: Some(FAKE_LINK.into()),
                ..inner.info().clone()
            };
            // A camera whose media type says more than it makes (the W4GS: 30 nominal, 14.4 real).
            if let Some(nominal) = *self.nominal_fps.lock().unwrap() {
                info.fps = nominal;
            }
            info.fallback = finish_fallback(fallback, &info);
            // A camera that was closed and opened again has forgotten its controls (plan 4.1).
            self.controls.forget();
            Ok(Box::new(FakeSource {
                inner,
                info,
                controls: self.controls.clone(),
                gone: self.gone.clone(),
                stream_error: self.stream_error.clone(),
                stream_error_once: self.stream_error_once.clone(),
                lose_device_with_error: self.lose_device_with_error.clone(),
                gpu: gpu.clone(),
                alive: self.alive.clone(),
                live: self.live.lock().unwrap().clone(),
                close_delay: self.close_delay.clone(),
                stall: self.stall.lock().unwrap().clone(),
                silent: self.silent.clone(),
                frame_every_ms: self.frame_every_ms.clone(),
                last_sent: None,
            }))
        }

        fn privacy_blocked(&self) -> Option<crate::capture::privacy::Blocked> {
            (self.privacy.load(Ordering::SeqCst) || self.registry_deny.load(Ordering::SeqCst))
                .then_some(crate::capture::privacy::Blocked::DesktopApps)
        }

        fn camera_users(&self, target: Option<&str>, vcam_consumer: bool) -> crate::capture::in_use::CameraUsers {
            self.targets.lock().unwrap().push(target.map(str::to_string));
            if self.real_users.load(Ordering::SeqCst) {
                return crate::capture::in_use::camera_users(target, vcam_consumer);
            }
            let names = self.users.lock().unwrap().clone();
            let maybe = !names.is_empty() && vcam_consumer;
            crate::capture::in_use::CameraUsers { names, maybe }
        }
    }

    struct Rig {
        engine: Engine,
        /// The device the engine started with (the next ones are its own).
        gpu: Arc<GpuDevice>,
        fake: Arc<Fake>,
        file: MappedFile,
        events: Arc<Mutex<Vec<(Instant, EngineEvent)>>>,
        /// Every state and error event with the file's writer state at the moment the sink got it.
        writer_at: Arc<Mutex<Vec<(EngineEvent, u32)>>>,
        started: Instant,
    }

    impl Rig {
        fn new(name: &str, timings: Timings) -> Rig {
            Rig::with_opener(name, timings, |fake| {
                Arc::new(crate::camera_controls::fake::FakeOpener {
                    camera: fake.controls.clone(),
                    link: FAKE_LINK.into(),
                    opened: Default::default(),
                    closed: Default::default(),
                })
            })
        }

        /// [`Rig::new`] with the opener of the camera controls' leases made by `make_opener`.
        fn with_opener(
            name: &str,
            timings: Timings,
            make_opener: impl FnOnce(&Arc<Fake>) -> Arc<dyn ControlsOpener>,
        ) -> Rig {
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
                privacy: AtomicBool::new(false),
                stream_error: Arc::new(AtomicBool::new(false)),
                stream_error_once: Arc::new(AtomicBool::new(false)),
                lose_device_with_error: Arc::new(AtomicBool::new(false)),
                open_delay: Mutex::new(Duration::ZERO),
                close_delay: Arc::new(Mutex::new(Duration::ZERO)),
                stall: Mutex::new(Arc::default()),
                silent: Arc::new(AtomicBool::new(false)),
                frame_every_ms: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                open_steps: Mutex::new((0, Duration::ZERO)),
                steps_taken: Default::default(),
                nominal_fps: Mutex::new(None),
                lacks: Mutex::new(Vec::new()),
                first_opens: Mutex::new(Vec::new()),
                silent_formats: Mutex::new(Vec::new()),
                open_gate: Mutex::new(None),
                opened_on_removed: Mutex::new(Vec::new()),
                lose_device_in_open: AtomicBool::new(false),
                registry_deny: AtomicBool::new(false),
                users: Mutex::new(Vec::new()),
                real_users: AtomicBool::new(false),
                targets: Mutex::new(Vec::new()),
                capture_errors: Mutex::new(VecDeque::new()),
                flat: Mutex::new(None),
                live: Mutex::new(None),
            });
            let opener = make_opener(&fake);
            let probe = writer.clone();
            let engine = Engine::build(gpu.clone(), writer, timings, fake.clone(), opener).unwrap();
            let events: Arc<Mutex<Vec<(Instant, EngineEvent)>>> = Arc::default();
            let writer_at: Arc<Mutex<Vec<(EngineEvent, u32)>>> = Arc::default();
            let (sink, seen) = (events.clone(), writer_at.clone());
            engine.set_event_sink(Arc::new(move |e: EngineEvent| {
                if matches!(e, EngineEvent::State(_) | EngineEvent::Error(_)) {
                    let w = probe.header().writer_state;
                    seen.lock().unwrap().push((e.clone(), w));
                }
                sink.lock().unwrap().push((Instant::now(), e));
            }));
            Rig {
                engine,
                gpu,
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

        /// The same with the format the app negotiated.
        fn consumer_with(&self, path_name: &str, format: Format) -> Consumer {
            Consumer::start_with(&temp_file(path_name), format)
        }
    }

    struct Consumer {
        stop: Arc<AtomicBool>,
        join: Option<std::thread::JoinHandle<()>>,
        file: Arc<MappedFile>,
    }

    impl Consumer {
        /// An app that asked for 320x180 NV12 at 30 fps (the size of the fake camera's picture).
        fn start(path: &std::path::Path) -> Consumer {
            Consumer::start_with(path, Format::new(320, 180, 30, SUBTYPE_NV12))
        }

        /// An app that negotiated `format`.
        fn start_with(path: &std::path::Path, format: Format) -> Consumer {
            let file = Arc::new(MappedFile::open(path, Access::ReadWrite).unwrap());
            let stop = Arc::new(AtomicBool::new(false));
            file.shared().set_negotiated(&format);
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

        // plugged in again: Live on the next retry (counted in opens, not in seconds)
        let opens_when_back = rig.opens();
        rig.fake.gone.store(false, Ordering::SeqCst);
        let back = rig.wait_state(EngineState::Live, Duration::from_secs(10));
        eprintln!("camera back -> live in {back:?}");
        assert!(rig.opens() <= opens_when_back + 2, "{} opens before, {} after", opens_when_back, rig.opens());
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
        assert_eq!(reason, "camera is in use by another app", "no code in the words the window shows");
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

    /// The newest `reconnecting` event: `(reason, apps, apps_maybe)`.
    fn last_in_use(rig: &Rig) -> Option<(Option<String>, Option<Vec<String>>, bool)> {
        rig.events.lock().unwrap().iter().rev().find_map(|(_, e)| match e {
            EngineEvent::State(s) if s.state == EngineState::Reconnecting => {
                Some((s.reason.clone(), s.apps.clone(), s.apps_maybe))
            }
            _ => None,
        })
    }

    #[test]
    fn a_camera_in_use_names_the_programs_and_the_list_follows_each_retry() {
        let rig = Rig::new("sm-inuse-names.bin", fast());
        let in_use = || Failure::in_use(Some(HRESULT(0xC00D_3704_u32 as i32)));
        *rig.fake.users.lock().unwrap() = vec!["Discord.exe".into()];
        for _ in 0..200 {
            rig.fake.failures.lock().unwrap().push_back(in_use());
        }
        rig.engine.start(settings(bars(30), false));
        rig.wait_state(EngineState::Reconnecting, Duration::from_secs(5));
        let (reason, apps, maybe) = last_in_use(&rig).unwrap();
        assert_eq!(reason.as_deref(), Some("camera is in use by Discord.exe"));
        assert_eq!(apps, Some(vec!["Discord.exe".to_string()]));
        assert!(!maybe);
        assert_eq!(last_kind(&rig), Some("in_use"));

        // the registry is read again with every retry: another program, then two, then none
        let next = |users: &[&str], what: &str| {
            *rig.fake.users.lock().unwrap() = users.iter().map(|s| s.to_string()).collect();
            let want: Option<Vec<String>> = (!users.is_empty()).then(|| users.iter().map(|s| s.to_string()).collect());
            rig.wait(what, Duration::from_secs(5), |r| last_in_use(r).is_some_and(|(_, a, _)| a == want));
            last_in_use(&rig).unwrap()
        };
        let (reason, ..) = next(&["chrome.exe"], "the new name");
        assert_eq!(reason.as_deref(), Some("camera is in use by chrome.exe"));
        let (reason, ..) = next(&["a.exe", "b.exe", "c.exe", "d.exe"], "four names");
        assert_eq!(reason.as_deref(), Some("camera is in use by a.exe, b.exe and 2 more"));
        let (reason, apps, _) = next(&[], "nobody");
        assert_eq!(reason.as_deref(), Some("camera is in use by another app"));
        assert_eq!(apps, None, "no list, not an empty one");

        // every state event of the wait is a real change: the same list is not announced twice
        let events = rig.states();
        assert!(events.windows(2).all(|w| w[0] != w[1]), "{events:?}");
        rig.fake.failures.lock().unwrap().clear();
        rig.engine.wait_live(Duration::from_secs(10)).unwrap();
    }

    #[test]
    fn the_names_are_a_maybe_while_an_app_reads_the_virtual_camera() {
        let rig = Rig::new("sm-inuse-maybe.bin", fast());
        *rig.fake.users.lock().unwrap() = vec!["Discord.exe".into()];
        for _ in 0..200 {
            rig.fake.failures.lock().unwrap().push_back(Failure::in_use(None));
        }
        let mut consumer = rig.consumer("sm-inuse-maybe.bin");
        rig.engine.start(settings(bars(30), true));
        rig.wait_state(EngineState::Reconnecting, Duration::from_secs(5));
        let (reason, apps, maybe) = last_in_use(&rig).unwrap();
        assert_eq!(reason.as_deref(), Some("camera may be in use by Discord.exe"));
        assert_eq!(apps, Some(vec!["Discord.exe".to_string()]));
        assert!(maybe, "a consumer is attached: the list may name it");
        consumer.detach();
        rig.fake.failures.lock().unwrap().clear();
    }

    /// Moves the lookup to test sources for one test and puts everything back (the variables are process-wide, so only
    /// the test that sets them reads the real lookup).
    struct TestSources {
        log: std::path::PathBuf,
        child: Option<std::process::Child>,
    }

    impl TestSources {
        fn new(name: &str) -> TestSources {
            let log = std::env::temp_dir().join(format!("better-cam-{name}-{}.txt", std::process::id()));
            std::fs::write(&log, "").unwrap();
            std::env::set_var(crate::capture::in_use_log::TEST_LOG_ENV, &log);
            // a registry root that does not exist: no record from the real consent store
            std::env::set_var(
                crate::capture::in_use::TEST_ROOT_ENV,
                format!(r"Software\BetterCamTest\none-{}", std::process::id()),
            );
            TestSources { log, child: None }
        }

        /// Starts a program that stays for a minute; returns its process id.
        fn client(&mut self) -> u32 {
            let child = std::process::Command::new("ping")
                .args(["-n", "60", "127.0.0.1"])
                .stdout(std::process::Stdio::null())
                .spawn()
                .expect("ping");
            let pid = child.id();
            self.child = Some(child);
            pid
        }

        fn kill_client(&mut self) {
            if let Some(mut c) = self.child.take() {
                let _ = c.kill();
                let _ = c.wait();
            }
        }

        fn write(&self, text: &str) {
            std::fs::write(&self.log, text).unwrap();
        }
    }

    impl Drop for TestSources {
        fn drop(&mut self) {
            self.kill_client();
            std::env::remove_var(crate::capture::in_use_log::TEST_LOG_ENV);
            std::env::remove_var(crate::capture::in_use::TEST_ROOT_ENV);
            let _ = std::fs::remove_file(&self.log);
        }
    }

    #[test]
    fn the_camera_log_names_the_program_that_streams_the_camera_and_the_name_follows_each_retry() {
        let mut src = TestSources::new("camlog");
        let client = src.client();
        // events are written relative to "now": the program must be older than the events
        std::thread::sleep(Duration::from_millis(1200));
        let me = std::process::id(); // stands for the camera service host: a running process older than the events
        let link = FAKE_LINK;
        let virtual_link = r"\\?\swd#vcamdevapi#d8f8cb27#{65e8773d-8f56-11d0-a3b9-00a0c9223196}\{fcebba03}";
        src.write(&format!(
            "open 800 {client} {link}\nstart 700 {me} 7 {link}\n"
        ));

        let rig = Rig::new("sm-inuse-camlog.bin", fast());
        rig.fake.real_users.store(true, Ordering::SeqCst);
        for _ in 0..400 {
            rig.fake.failures.lock().unwrap().push_back(Failure::in_use(Some(HRESULT(0xC00D_3704_u32 as i32))));
        }
        let mut cap = saved_format(VideoFormat::Any);
        cap.device = Some(FAKE_LINK.to_string());
        rig.engine.start(settings(SourceSpec::Configured(cap), false));
        rig.wait_state(EngineState::Reconnecting, Duration::from_secs(5));
        let named = |r: &Rig| last_in_use(r).is_some_and(|(_, a, _)| a.is_some());
        rig.wait("the program from the log", Duration::from_secs(5), named);
        let (reason, apps, maybe) = last_in_use(&rig).unwrap();
        let app = apps.as_ref().and_then(|a| a.first()).cloned().unwrap();
        assert!(app.eq_ignore_ascii_case("ping.exe"), "{app}");
        assert_eq!(apps.as_ref().map(Vec::len), Some(1));
        assert_eq!(reason.as_deref(), Some(format!("camera is in use by {app}").as_str()));
        assert!(!maybe, "one client opened just before the stream: no doubt");
        assert_eq!(rig.fake.targets.lock().unwrap().last(), Some(&Some(FAKE_LINK.to_string())), "the lookup is told the camera");

        // a client of the virtual camera is not a holder of the physical camera
        src.write(&format!("open 800 {client} {virtual_link}\nstart 700 {me} 7 {virtual_link}\n"));
        rig.wait("nobody", Duration::from_secs(5), |r| last_in_use(r).is_some_and(|(_, a, _)| a.is_none()));
        assert_eq!(last_in_use(&rig).unwrap().0.as_deref(), Some("camera is in use by another app"));

        // the stream stops: nobody; starts again: named; the program dies without a stop event: nobody again
        src.write(&format!("open 800 {client} {link}\nstart 700 {me} 7 {link}\nstop 100 {me} 7 {link}\n"));
        std::thread::sleep(Duration::from_millis(700));
        assert!(last_in_use(&rig).unwrap().1.is_none(), "a stopped stream names nobody");
        src.write(&format!("open 800 {client} {link}\nstart 700 {me} 7 {link}\n"));
        rig.wait("named again", Duration::from_secs(5), named);
        src.kill_client();
        rig.wait("the dead program is dropped", Duration::from_secs(5), |r| {
            last_in_use(r).is_some_and(|(_, a, _)| a.is_none())
        });

        rig.fake.failures.lock().unwrap().clear();
        rig.engine.wait_live(Duration::from_secs(10)).unwrap();
        let events = rig.states();
        assert!(events.windows(2).all(|w| w[0] != w[1]), "every state event is a change: {events:?}");
    }

    #[test]
    fn only_a_camera_in_use_is_named() {
        let rig = Rig::new("sm-inuse-only.bin", fast());
        *rig.fake.users.lock().unwrap() = vec!["Discord.exe".into()];
        rig.fake.failures.lock().unwrap().push_back(Failure::disconnected(Some(HRESULT(0xC00D_36B2_u32 as i32))));
        rig.engine.start(settings(bars(30), false));
        rig.wait_state(EngineState::Reconnecting, Duration::from_secs(5));
        let (reason, apps, _) = last_in_use(&rig).unwrap();
        assert!(reason.unwrap().starts_with("camera disconnected"), "the code stays in this reason (not in scope)");
        assert_eq!(apps, None);
        assert_eq!(last_kind(&rig), Some("disconnected"));
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
    fn stats_events_come_when_the_numbers_change_and_only_while_the_window_is_visible() {
        let rig = Rig::new("sm-stats.bin", fast());
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        // let a one-second statistics window complete (no app reads the camera yet: frames are rendered for the
        // preview and not published)
        rig.wait("a full window", Duration::from_secs(10), |r| {
            r.engine.stats().render_fps > 0.0
        });
        let all_stats = |rig: &Rig| -> Vec<StatsEvent> {
            rig.events
                .lock()
                .unwrap()
                .iter()
                .filter_map(|(_, e)| match e {
                    EngineEvent::Stats(s) => Some(s.clone()),
                    _ => None,
                })
                .collect()
        };
        // A hidden window hears nothing, however the numbers move.
        std::thread::sleep(Duration::from_millis(1600));
        assert!(all_stats(&rig).is_empty(), "stats events while the window is hidden");

        // It is shown: one event at once, with everything in it.
        rig.engine.set_window_visible(true);
        rig.wait("the first event", Duration::from_secs(2), |r| !all_stats(r).is_empty());
        let first = all_stats(&rig)[0].clone();
        assert_eq!((first.capture.width, first.output.width), (320, 320));
        // With no app attached the output rate is the rate of the picture the preview gets.
        assert!((3.0..=36.0).contains(&first.output.fps), "{first:?}");
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
                if s.rendered > after {
                    return (s, Instant::now());
                }
                assert!(t.elapsed() < Duration::from_secs(10), "no stats window");
                std::thread::sleep(Duration::from_millis(2));
            }
        };
        let (a, ta) = next_window(&rig, rig.engine.stats().rendered);
        let (b, tb) = next_window(&rig, a.rendered);
        // the count is taken over exactly three seconds of the events' own timestamps: a test thread that
        // oversleeps on a busy machine does not add events
        if let Some(rest) = (mark + Duration::from_secs(3)).checked_duration_since(Instant::now()) {
            std::thread::sleep(rest);
        }
        let end = mark + Duration::from_secs(3);
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
        // The figures change once a second: about three events in three seconds (it was 12 at 4 Hz), and no event
        // repeats the one before it.
        assert!(
            (2..=5).contains(&none.len()),
            "{} stats events in 3 s",
            none.len()
        );
        assert!(
            none.windows(2).all(|w| w[0] != w[1]),
            "an event that carries nothing new: {none:?}"
        );
        // The frame rates are per window over its real elapsed time, so they follow what the machine managed (13 fps
        // for a 30 fps source was seen with every core busy): compare them with the counters and the test's own
        // clock, not with a fixed rate.
        let secs = tb.duration_since(ta).as_secs_f64();
        for (name, reported, frames) in [
            ("capture", b.capture_fps, b.captured - a.captured),
            ("render", b.render_fps, b.rendered - a.rendered),
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
        // frames are published now, so the latency of the file is a number
        rig.wait("a latency", Duration::from_secs(5), |r| {
            stats_since(t, r).last().is_some_and(|s| s.latency_p95_ms > 0.0 && s.output.fps > 0.0)
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

    // ---- performance faults of the audit: latency statistics, frames nobody takes, stamps, events ----------------

    mod count_allocs {
        //! Counts the allocations of the calling thread, so a test can say that a call allocates nothing.
        use std::alloc::{GlobalAlloc, Layout, System};
        use std::cell::Cell;

        thread_local! {
            static ALLOCS: Cell<u64> = const { Cell::new(0) };
        }

        pub struct Counting;

        fn count() {
            let _ = ALLOCS.try_with(|c| c.set(c.get() + 1));
        }

        // SAFETY: every call goes to the system allocator unchanged.
        unsafe impl GlobalAlloc for Counting {
            unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
                count();
                unsafe { System.alloc(layout) }
            }
            unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
                count();
                unsafe { System.alloc_zeroed(layout) }
            }
            unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
                unsafe { System.dealloc(ptr, layout) }
            }
            unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
                count();
                unsafe { System.realloc(ptr, layout, new_size) }
            }
        }

        /// Allocations this thread has made so far.
        pub fn allocations() -> u64 {
            ALLOCS.with(|c| c.get())
        }
    }

    #[global_allocator]
    static COUNTING: count_allocs::Counting = count_allocs::Counting;

    /// A deterministic stream of numbers in `[0, 1)`.
    fn unit_stream(seed: u64) -> impl FnMut() -> f64 {
        let mut x = seed;
        move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 11) as f64 / (1u64 << 53) as f64
        }
    }

    #[test]
    fn the_latency_histogram_matches_a_sorted_run_and_a_push_allocates_nothing() {
        const N: usize = 3_000_000;
        let mut track = LatencyTrack::new();
        let mut reference: Vec<f32> = Vec::with_capacity(N);
        let mut next = unit_stream(0x9E37_79B9_7F4A_7C15);
        let mut largest = 0.0f64;
        let before = count_allocs::allocations();
        for i in 0..N {
            // About 2 ms, a tail up to 40 ms, a spike past the histogram's range once in a while.
            let u = next();
            let ms = if i % 100_000 == 99_999 {
                1500.0 + 500.0 * u
            } else {
                1.0 + 2.0 * u + 40.0 * u.powi(8)
            };
            track.push(ms);
            reference.push(ms as f32);
            largest = largest.max(ms);
        }
        assert_eq!(
            count_allocs::allocations() - before,
            0,
            "pushing {N} samples allocated"
        );
        reference.sort_by(|a, b| a.total_cmp(b));
        // The old store stopped taking samples at 200 000; these are all 3 million.
        for q in [0.5, 0.9, 0.95, 0.99, 0.999] {
            let want = percentile(&reference, q);
            let got = track.total_quantile(q);
            assert!(
                (got - want).abs() <= BIN_MS,
                "q {q}: histogram {got:.4} ms, sorted {want:.4} ms (a bin is {BIN_MS} ms)"
            );
        }
        assert_eq!(track.total_max(), largest, "the maximum is exact");
        assert!(track.total_max() >= 1500.0, "the overflow bin keeps the exact maximum");
        // a read is a scan of the bins: far from the sort of millions of samples
        let t = Instant::now();
        for _ in 0..200 {
            std::hint::black_box(track.total_p95());
        }
        assert!(
            t.elapsed() < Duration::from_millis(250),
            "200 reads took {:?}",
            t.elapsed()
        );
        let mut empty = LatencyTrack::new();
        assert_eq!((empty.total_p95(), empty.total_max()), (0.0, 0.0));
        empty.push(-3.0);
        assert_eq!(empty.total_p95(), 0.0, "a negative latency counts as 0");
    }

    #[test]
    fn reading_the_whole_run_percentile_after_200000_samples_is_not_a_sort() {
        let mut track = LatencyTrack::new();
        let mut kept: Vec<f32> = Vec::new();
        let mut next = unit_stream(7);
        for _ in 0..200_000 {
            let ms = 1.0 + 5.0 * next();
            track.push(ms);
            kept.push(ms as f32);
        }
        // What the render thread did every second until now, twice: clone and sort the capped history.
        let old = || {
            let mut v = kept.clone();
            v.sort_by(|a, b| a.total_cmp(b));
            percentile(&v, 0.95)
        };
        let best = |f: &dyn Fn() -> f64| {
            (0..20)
                .map(|_| {
                    let t = Instant::now();
                    std::hint::black_box(f());
                    t.elapsed()
                })
                .min()
                .unwrap()
        };
        let (was, is) = (best(&old), best(&|| track.total_p95()));
        eprintln!("whole-run p95 over 200 000 samples: sorted {was:?}, histogram {is:?}");
        assert!(is * 20 < was, "the read takes {is:?}, the sort took {was:?}");
        assert!((track.total_p95() - old()).abs() <= BIN_MS);
        // The scan ends where the percentile is; the worst case is a long tail (one sample at 900 ms).
        track.push(900.0);
        let worst = best(&|| track.total_quantile(1.0));
        eprintln!("whole-run maximum rank with a sample at 900 ms: histogram {worst:?}");
        assert!(worst * 20 < was, "the longest scan takes {worst:?}, the sort took {was:?}");
    }

    #[test]
    fn the_latency_window_is_the_last_240_samples() {
        let mut t = LatencyTrack::new();
        for i in 0..1000 {
            t.push(i as f64);
        }
        let (p50, p95, max) = t.window_stats();
        // samples 760..=999
        assert_eq!(max, 999.0);
        assert_eq!(p50, percentile(&(760..1000).map(|i| i as f32).collect::<Vec<_>>(), 0.5));
        assert!(p95 > 980.0 && p95 <= 999.0, "{p95}");
    }

    /// The virtual camera's choice of frames, as `crates/vcam-source/src/frames.rs` makes it (`try_read` and
    /// `min_capture_gap`) on the capture times of what it finds in the file: a frame is taken unless it was captured
    /// less than 3/4 of a period after the last frame it took.
    fn dll_takes(captures: &[i64], period: i64) -> Vec<i64> {
        let mut taken: Vec<i64> = Vec::new();
        for &c in captures {
            let skip = taken.last().is_some_and(|&last| c >= last && c - last < period * 3 / 4);
            if !skip {
                taken.push(c);
            }
        }
        taken
    }

    #[test]
    fn the_pacer_never_holds_back_a_frame_the_virtual_camera_takes() {
        let mut next = unit_stream(0xDEAD_BEEF_1234_5678);
        let mut saved = 0usize;
        let mut total = 0usize;
        for camera_fps in [120u32, 60, 50, 48, 40, 30, 29, 25, 15] {
            for consumer_fps in [60u32, 30, 25, 24, 20, 15, 10] {
                for jitter in [0.0, 0.1, 0.3, 0.6] {
                    for _ in 0..8 {
                        // a camera with jitter (never going back, apart from a rare glitch), 900 frames
                        let cam_period = 10_000_000.0 / camera_fps as f64;
                        let mut t = 1_000_000_000.0f64;
                        let mut captures = Vec::new();
                        for k in 0..900 {
                            t += cam_period;
                            let mut c = t + (next() - 0.5) * 2.0 * jitter * cam_period;
                            if k % 250 == 249 {
                                c -= 3.0 * cam_period; // a clock that steps back
                            }
                            captures.push(c as i64);
                        }
                        let period = 10_000_000 / consumer_fps as i64;
                        let mut pacer = Pacer::default();
                        let published: Vec<i64> = captures
                            .iter()
                            .copied()
                            .filter(|&c| pacer.admit(c, period))
                            .collect();
                        assert_eq!(
                            dll_takes(&published, period),
                            dll_takes(&captures, period),
                            "camera {camera_fps}, consumer {consumer_fps}, jitter {jitter}: the virtual camera takes \
                             other frames from what was published than from what the camera made"
                        );
                        total += captures.len();
                        saved += captures.len() - published.len();
                    }
                }
            }
        }
        assert!(saved * 5 > total, "the pacer saves nothing: {saved} of {total}");

        // 60 fps against a 30 fps consumer with a steady clock: exactly every other frame
        let period = 10_000_000 / 30;
        let mut pacer = Pacer::default();
        let kept: Vec<usize> = (0..12)
            .filter(|k| pacer.admit(5_000 + *k as i64 * 10_000_000 / 60, period))
            .collect();
        assert_eq!(kept, [0, 2, 4, 6, 8, 10]);
        // The first frame goes out at once, and so does the first after a reset.
        let mut pacer = Pacer::default();
        assert!(pacer.admit(100, period));
        assert!(!pacer.admit(100 + period / 2, period));
        pacer.reset();
        assert!(pacer.admit(100 + period / 2, period));
        // The engine's rule is looser than the virtual camera's: nothing it takes is held back, and a frame in
        // between goes out although it is dropped there (it does not count as the last one taken).
        assert!(engine_min_gap(period) < dll_min_gap(period));
        let mut pacer = Pacer::default();
        assert!(pacer.admit(0, period));
        let between = (engine_min_gap(period) + dll_min_gap(period)) / 2;
        assert!(pacer.admit(between, period), "between the two rules: published");
        assert_eq!(pacer.base, Some(0), "but the virtual camera drops it");
        assert!(pacer.admit(dll_min_gap(period), period), "the next one at 3/4 of the period is taken");
    }

    #[test]
    fn a_publish_that_trails_its_capture_time_by_more_than_half_a_period_more_than_the_last_counts_as_late() {
        // 60 fps app: period 166 666, so half a period is 83 333
        let period = 166_666;
        let mut gaps = Gaps { period, ..Gaps::default() };
        let mut captured = 1_000_000_000;
        let mut note = |gaps: &mut Gaps, delay: i64| {
            captured += period;
            gaps.note(captured + delay, captured);
        };
        for _ in 0..5 {
            note(&mut gaps, 20_000);
        }
        assert_eq!(gaps.late, 0, "a steady pipeline is never late");
        note(&mut gaps, 20_000 + 50_000);
        note(&mut gaps, 20_000);
        assert_eq!(gaps.late, 0, "50 000 later than usual is under half a period");
        note(&mut gaps, 20_000 + 100_000);
        assert_eq!(gaps.late, 1, "100 000 later than usual is late");
        // the frame after it, back on time, is early against the late one, not late
        note(&mut gaps, 20_000);
        assert_eq!(gaps.late, 1);
        // until the period is known nothing is late
        let mut unknown = Gaps::default();
        unknown.note(0, 0);
        unknown.note(10_000_000, 0);
        assert_eq!(unknown.late, 0);
    }

    /// M3: a camera that makes fewer frames than its media type says (the W4GS: 14.4 fps of 30) has publish gaps far
    /// over 1.5 nominal periods and a pipeline that keeps up: nothing is late.
    #[test]
    fn a_camera_below_its_nominal_rate_is_never_late() {
        let nominal_period = 10_000_000 / 30;
        let mut gaps = Gaps { period: nominal_period, ..Gaps::default() };
        let actual = 10_000_000 * 10 / 144; // 14.4 fps
        for k in 0..100i64 {
            let captured = 5_000_000_000 + k * actual;
            gaps.note(captured + 30_000, captured);
        }
        assert_eq!(gaps.late, 0);
        // a stall of the render thread on such a camera is still found
        gaps.note(5_000_000_000 + 100 * actual + 400_000, 5_000_000_000 + 100 * actual);
        assert_eq!(gaps.late, 1);
    }

    #[test]
    fn a_gap_that_spans_a_time_nothing_was_published_is_forgotten() {
        let mut gaps = Gaps { period: 166_666, ..Gaps::default() };
        gaps.note(1_000, 900);
        gaps.note(168_000, 167_566);
        gaps.reset();
        // an app attached after seconds without one, or a pause was left: not late
        gaps.note(50_000_000, 49_990_000);
        assert_eq!(gaps.late, 0);
        gaps.note(50_170_000, 50_156_666);
        assert_eq!(gaps.late, 0);
    }

    /// L2: a capture time ahead of the arrival is a camera clock that runs fast, not a time to believe.
    #[test]
    fn a_camera_timestamp_ahead_of_its_arrival_is_not_believed() {
        let arrival = 50_000_000_000i64;
        // a little behind: the camera's own
        assert_eq!(plausible_capture(arrival - 300_000, arrival), arrival - 300_000);
        assert_eq!(plausible_capture(arrival - 9_000_000, arrival), arrival - 9_000_000);
        // up to 50 ms ahead (the virtual camera's own clamp): accepted
        assert_eq!(plausible_capture(arrival + 400_000, arrival), arrival + 400_000);
        assert_eq!(plausible_capture(arrival + 500_000, arrival), arrival + 500_000);
        // further ahead: the arrival (it was believed up to a second ahead)
        assert_eq!(plausible_capture(arrival + 600_000, arrival), arrival);
        assert_eq!(plausible_capture(arrival + 9_000_000, arrival), arrival);
        // missing, or a second or more behind: the arrival
        assert_eq!(plausible_capture(0, arrival), arrival);
        assert_eq!(plausible_capture(-5, arrival), arrival);
        assert_eq!(plausible_capture(arrival - 10_000_000, arrival), arrival);
        assert_eq!(plausible_capture(arrival - 50_000_000, arrival), arrival);
    }

    /// L3: the pacer works in QPC ticks with the virtual camera's integer period, so it agrees with the virtual camera
    /// about every frame whatever the frequency of the counter is (not only at 10 MHz).
    #[test]
    fn the_pacer_follows_the_virtual_cameras_arithmetic_at_any_counter_frequency() {
        let mut next = unit_stream(0x1234_5678_9ABC_DEF0);
        let mut saved = 0usize;
        let mut total = 0usize;
        for freq in [3_000_000u64, 10_000_000, 19_200_000, 24_000_000] {
            for camera_fps in [60u32, 30, 25] {
                for consumer_fps in [30u32, 24, 20, 15, 7] {
                    for jitter in [0.0, 0.1, 0.3] {
                        let format = Format::new(320, 180, consumer_fps, SUBTYPE_NV12);
                        let Some(period) = lower_consumer_period(&format, camera_fps as f64, freq) else {
                            continue;
                        };
                        assert_eq!(period, (freq / consumer_fps as u64) as i64, "the virtual camera's own period");
                        let cam_period = freq as f64 / camera_fps as f64;
                        let mut t = 1_000_000_000.0f64;
                        let captures: Vec<i64> = (0..600)
                            .map(|_| {
                                t += cam_period;
                                (t + (next() - 0.5) * 2.0 * jitter * cam_period) as i64
                            })
                            .collect();
                        let mut pacer = Pacer::default();
                        let published: Vec<i64> =
                            captures.iter().copied().filter(|&c| pacer.admit(c, period)).collect();
                        assert_eq!(
                            dll_takes(&published, period),
                            dll_takes(&captures, period),
                            "counter {freq} Hz, camera {camera_fps}, consumer {consumer_fps}, jitter {jitter}"
                        );
                        total += captures.len();
                        saved += captures.len() - published.len();
                    }
                }
            }
        }
        assert!(saved * 5 > total, "the pacer saves nothing: {saved} of {total}");
        // a consumer at or above the camera's rate is not decimated at all, and no rate is no period
        let f = |fps| Format::new(320, 180, fps, SUBTYPE_NV12);
        assert_eq!(lower_consumer_period(&f(30), 30.0, 10_000_000), None);
        assert_eq!(lower_consumer_period(&f(60), 30.0, 10_000_000), None);
        assert_eq!(lower_consumer_period(&f(30), 29.97, 10_000_000), None);
        assert_eq!(lower_consumer_period(&Format { fps_num: 0, ..f(30) }, 60.0, 10_000_000), None);
        assert_eq!(dll_period(&Format { fps_num: 30000, fps_den: 1001, ..f(30) }, 10_000_000), Some(333_333));
    }

    #[test]
    fn a_late_wake_up_does_not_move_the_stamps_of_the_synthetic_source() {
        let t0 = Instant::now();
        let origin = 5_000_000_000i64;
        let period = Duration::from_secs_f64(1.0 / 60.0);
        let period_100ns = 10_000_000.0 / 60.0;
        let mut plan = Schedule::new(60, t0, origin);
        let (mut stamps, mut wakes) = (Vec::new(), Vec::new());
        for k in 0..12 {
            // The thread wakes on time, except for frame 4, which it makes 12 ms late (a quarter period is 4 ms).
            let due = plan.next_at;
            let wake = if k == 4 { due + Duration::from_millis(12) } else { due };
            stamps.push(plan.take(wake));
            wakes.push(origin + (wake - t0).as_nanos() as i64 / 100);
        }
        for w in stamps.windows(2) {
            let gap = (w[1] - w[0]) as f64;
            assert!(
                (gap - period_100ns).abs() <= 1.0,
                "stamped {gap} apart, a period is {period_100ns:.1}: {stamps:?}"
            );
        }
        // What stamping with the wake-up made of it: one frame looks too early to the virtual camera (it takes
        // frames at least 3/4 of a period after the last).
        let worst = wakes.windows(2).map(|w| w[1] - w[0]).min().unwrap() as f64;
        assert!(worst < period_100ns * 0.75, "the wake-up stamps would have been fine too: {worst}");
        assert_eq!(plan.next_at, t0 + period * 12, "the plan did not slip");

        // More than a period late: the missed slots are not made up, and no stamp is ever early or from the future.
        let mut plan = Schedule::new(60, t0, origin);
        let mut last = None;
        for k in 0..10u32 {
            let due = plan.next_at;
            let wake = if k == 3 { due + Duration::from_millis(40) } else { due };
            let stamp = plan.take(wake);
            if let Some(l) = last {
                assert!(
                    (stamp - l) as f64 >= period_100ns - 1.0,
                    "frame {k} is stamped {} after the one before",
                    stamp - l
                );
            }
            assert!(stamp <= origin + (wake - t0).as_nanos() as i64 / 100, "stamped from the future");
            last = Some(stamp);
        }
    }

    #[test]
    fn the_synthetic_source_stamps_the_planned_time_not_the_wake_up() {
        let mut s = SyntheticInput::new(SyntheticSpec {
            width: 64,
            height: 36,
            fps: 100,
            format: PixelFormat::Nv12,
            color: ColorSettings::default(),
        })
        .unwrap();
        let mut stamps = Vec::new();
        for k in 0..30 {
            if k == 10 {
                // the render thread is held up for 4 ms: a late wake-up of the source's own thread
                std::thread::sleep(Duration::from_millis(4));
            }
            let SourceEvent::Frame(f) = s.next(Duration::from_millis(100)) else {
                panic!("a frame is due");
            };
            let (capture, arrival) = f.times();
            assert!(arrival >= capture, "arrived before it was planned: {arrival} < {capture}");
            stamps.push(capture);
        }
        for w in stamps.windows(2) {
            let gap = w[1] - w[0];
            assert!((99_999..=100_001).contains(&gap), "stamps {gap} apart at 100 fps: {stamps:?}");
        }
    }

    /// The newest statistics snapshot that satisfies `cond`.
    fn wait_stats(rig: &Rig, what: &str, cond: impl Fn(&EngineStats) -> bool) -> EngineStats {
        let t = Instant::now();
        loop {
            let s = rig.engine.stats();
            if cond(&s) {
                return s;
            }
            assert!(
                t.elapsed() < Duration::from_secs(30),
                "timed out waiting for {what}: {s:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Counts the frames the preview would get.
    struct CountingTap(Arc<std::sync::atomic::AtomicU64>);

    impl FrameTap for CountingTap {
        fn on_frame(&mut self, _: &Renderer, _: &RgbaTexture, _: u64) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn frames_in_file(rig: &Rig) -> u64 {
        rig.file.shared().header().writer_frames
    }

    #[test]
    fn a_consumer_at_half_the_camera_rate_gets_every_other_frame() {
        let rig = Rig::new("pub-60-30.bin", fast());
        let _consumer = rig.consumer_with("pub-60-30.bin", Format::new(320, 180, 30, SUBTYPE_NV12));
        rig.engine.start(settings(bars(60), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        let s = wait_stats(&rig, "150 frames", |s| s.rendered >= 150);
        // the counters of one snapshot add up: every rendered frame was published or skipped for its rate
        assert_eq!(s.rendered, s.published + s.skipped_rate + s.skipped_no_consumer, "{s:?}");
        assert_eq!(s.skipped_no_consumer, 0);
        assert!(
            s.published * 100 >= s.rendered * 40 && s.published * 100 <= s.rendered * 70,
            "{} of {} frames published for a consumer at half the rate",
            s.published,
            s.rendered
        );
        assert!(frames_in_file(&rig) >= s.published);
    }

    #[test]
    fn a_consumer_at_the_camera_rate_or_above_it_gets_every_frame() {
        for (camera, consumer, name) in [(60, 60, "pub-60-60.bin"), (30, 60, "pub-30-60.bin"), (30, 30, "pub-30-30.bin")] {
            let rig = Rig::new(name, fast());
            let _consumer = rig.consumer_with(name, Format::new(320, 180, consumer, SUBTYPE_NV12));
            rig.engine.start(settings(bars(camera), false));
            rig.engine.wait_live(Duration::from_secs(20)).unwrap();
            let s = wait_stats(&rig, "90 frames", |s| s.rendered >= 90);
            assert_eq!(s.skipped_rate, 0, "camera {camera}, consumer {consumer}: {s:?}");
            assert_eq!(s.skipped_no_consumer, 0);
            assert_eq!(s.published, s.rendered, "camera {camera}, consumer {consumer}: {s:?}");
        }
    }

    #[test]
    fn without_an_app_reading_the_camera_frames_are_rendered_but_not_published() {
        let rig = Rig::new("pub-none.bin", fast());
        let seen = Arc::new(std::sync::atomic::AtomicU64::new(0));
        rig.engine.send(Msg::SetTap(Some(Box::new(CountingTap(seen.clone())))));
        rig.engine.start(settings(bars(30), false));
        // Live is reached without a published frame: the state, and the writer too (it was `Starting`).
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        rig.wait("the writer is live", Duration::from_secs(10), |r| r.writer_state() == LIVE);
        let s = wait_stats(&rig, "60 frames", |s| s.rendered >= 60);
        assert_eq!(s.published, 0, "{s:?}");
        assert_eq!(s.skipped_no_consumer, s.rendered, "{s:?}");
        assert_eq!(s.readback_ms, 0.0);
        assert_eq!(frames_in_file(&rig), 0, "nothing was written to the file");
        assert_eq!(rig.engine.state(), EngineState::Live);
        // the preview still gets every frame
        assert!(seen.load(Ordering::SeqCst) >= s.rendered, "{} frames reached the tap", seen.load(Ordering::SeqCst));
        // and the heartbeat goes on: the virtual camera would not show "not running"
        std::thread::sleep(Duration::from_millis(1200));
        let h = rig.file.shared().header();
        let age = vcam_protocol::qpc_now().saturating_sub(h.writer_heartbeat);
        assert!(age < vcam_protocol::qpc_frequency() / 4, "the heartbeat is {age} ticks old");
        assert_eq!(h.writer_state, LIVE);
    }

    #[test]
    fn the_first_frame_after_an_app_attaches_goes_out_at_once_and_leaving_stops_it() {
        let rig = Rig::new("pub-attach.bin", fast());
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        wait_stats(&rig, "30 frames", |s| s.rendered >= 30);
        assert_eq!(frames_in_file(&rig), 0);

        let t = Instant::now();
        let mut consumer = rig.consumer("pub-attach.bin");
        rig.wait("the first frame", Duration::from_secs(5), |r| frames_in_file(r) > 0);
        let took = t.elapsed();
        // one camera frame is 33 ms; the margin is for a busy machine, not for a frame that waits for a window
        assert!(took < Duration::from_millis(250), "the first frame came {took:?} after the app attached");
        assert_eq!(rig.writer_state(), LIVE);
        rig.wait("frames keep coming", Duration::from_secs(5), |r| frames_in_file(r) >= 10);

        // the app goes: no more frames
        consumer.detach();
        std::thread::sleep(Duration::from_millis(200));
        let at = frames_in_file(&rig);
        std::thread::sleep(Duration::from_millis(600));
        assert_eq!(frames_in_file(&rig), at, "frames were written for nobody");
        assert_eq!(rig.writer_state(), LIVE);

        // and one more attaches: again at once
        let t = Instant::now();
        let _again = rig.consumer("pub-attach.bin");
        rig.wait("a frame for the second app", Duration::from_secs(5), |r| frames_in_file(r) > at);
        assert!(t.elapsed() < Duration::from_millis(250), "{:?}", t.elapsed());
    }

    #[test]
    fn a_consumer_that_changes_its_rate_gets_a_frame_at_once() {
        let rig = Rig::new("pub-rate-change.bin", fast());
        let consumer = rig.consumer_with("pub-rate-change.bin", Format::new(320, 180, 30, SUBTYPE_NV12));
        rig.engine.start(settings(bars(60), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        wait_stats(&rig, "frames", |s| s.published >= 10);
        // the app negotiates 15 fps now: the pacer starts over and the next frame is published
        let before = frames_in_file(&rig);
        consumer.file.shared().set_negotiated(&Format::new(320, 180, 15, SUBTYPE_NV12));
        rig.wait("a frame", Duration::from_secs(2), |r| frames_in_file(r) > before);
        let s = wait_stats(&rig, "more frames", |s| s.rendered >= 240);
        assert_eq!(s.rendered, s.published + s.skipped_rate, "{s:?}");
        assert!(s.skipped_rate > s.published, "a quarter of the rate keeps about one frame in four: {s:?}");
    }

    #[test]
    fn the_controls_are_read_and_sent_only_while_the_window_is_visible_and_only_when_they_changed() {
        let rig = Rig::new("ctl-poll.bin", fast());
        let lists = |r: &Rig| -> Vec<Vec<crate::camera_controls::ControlInfoDto>> {
            r.events
                .lock()
                .unwrap()
                .iter()
                .filter_map(|(_, e)| match e {
                    EngineEvent::Controls(c) => Some(c.clone()),
                    _ => None,
                })
                .collect()
        };
        let focus_of = |l: &Vec<crate::camera_controls::ControlInfoDto>| {
            l.iter().find(|i| i.key == crate::capture::controls::ControlKey::Focus).unwrap().value
        };
        let lens = |value: i32| {
            lock(&rig.fake.controls.state)
                .iter_mut()
                .find(|i| i.key == crate::capture::controls::ControlKey::Focus)
                .unwrap()
                .value = value;
        };
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        // the list that the re-apply after the open sends
        rig.wait("the list after the open", Duration::from_secs(5), |r| !lists(r).is_empty());
        let n = lists(&rig).len();

        // hidden: the lens moves, nothing is read, nothing is sent
        lens(321);
        std::thread::sleep(Duration::from_millis(2500));
        assert_eq!(lists(&rig).len(), n, "controls were sent to a hidden window");

        // shown: the list at once, with where the lens is now
        rig.engine.set_window_visible(true);
        rig.wait("the list for the window that is back", Duration::from_secs(3), |r| lists(r).len() == n + 1);
        assert_eq!(focus_of(lists(&rig).last().unwrap()), 321);

        // nothing moves: nothing is sent, however many polls there are
        std::thread::sleep(Duration::from_millis(2500));
        assert_eq!(lists(&rig).len(), n + 1, "an unchanged list was sent again");

        // the lens moves while the window is visible: one list
        lens(400);
        rig.wait("the list with the new lens", Duration::from_secs(3), |r| lists(r).len() == n + 2);
        assert_eq!(focus_of(lists(&rig).last().unwrap()), 400);
        std::thread::sleep(Duration::from_millis(1500));
        assert_eq!(lists(&rig).len(), n + 2);

        // hidden again: it moves, nothing is sent; shown: the list once
        rig.engine.set_window_visible(false);
        std::thread::sleep(Duration::from_millis(300));
        lens(500);
        std::thread::sleep(Duration::from_millis(2200));
        assert_eq!(lists(&rig).len(), n + 2);
        rig.engine.set_window_visible(true);
        rig.wait("the list again", Duration::from_secs(3), |r| lists(r).len() == n + 3);
        assert_eq!(focus_of(lists(&rig).last().unwrap()), 500);
    }

    #[test]
    fn a_stats_event_follows_a_state_change_once_and_a_returning_window_gets_one() {
        let rig = Rig::new("stats-state.bin", fast());
        let stats = |r: &Rig| -> Vec<StatsEvent> {
            r.events
                .lock()
                .unwrap()
                .iter()
                .filter_map(|(_, e)| match e {
                    EngineEvent::Stats(s) => Some(s.clone()),
                    _ => None,
                })
                .collect()
        };
        rig.engine.set_window_visible(true);
        // stopped, visible: the window gets the (zero) numbers once when it is shown, and nothing after
        rig.wait("the first event", Duration::from_secs(2), |r| stats(r).len() == 1);
        std::thread::sleep(Duration::from_millis(800));
        assert_eq!(stats(&rig).len(), 1, "events while stopped");
        assert_eq!(stats(&rig)[0].capture.width, 0);
        assert_eq!(stats(&rig)[0].capture.rate, 0.0);

        // live: the numbers come, each different from the one before
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        rig.wait("live numbers", Duration::from_secs(5), |r| stats(r).iter().any(|s| s.capture.width == 320));
        let live = stats(&rig);
        assert!(live.windows(2).all(|w| w[0] != w[1]), "{live:?}");

        // hidden: silence; shown again: the numbers as they are, once at once
        rig.engine.set_window_visible(false);
        std::thread::sleep(Duration::from_millis(100));
        let n = stats(&rig).len();
        std::thread::sleep(Duration::from_millis(1500));
        assert_eq!(stats(&rig).len(), n, "events to a hidden window");
        rig.engine.set_window_visible(true);
        rig.wait("an event for the returning window", Duration::from_millis(800), |r| stats(r).len() == n + 1);
        assert_eq!(stats(&rig).last().unwrap().capture.width, 320);
        // the rate of the open media type (what the camera is asked for) is in every live event, apart from the rate
        // it delivers
        assert!(live.iter().filter(|s| s.capture.width == 320).all(|s| s.capture.rate == 30.0), "{live:?}");
        assert_eq!(stats(&rig).last().unwrap().capture.rate, 30.0);
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
        // A camera that sends nothing is retried (and a saved format that never delivers falls back, see the factory).
        assert!(matches!(
            Failure::from_capture(&CaptureError::Timeout("the first frame")),
            Failure::Retry(RetryKind::NoFrames, m) if m.contains("picture")
        ));
        assert!(matches!(
            Failure::from_stop(StopReason::DeviceLost(None)),
            Failure::Retry(RetryKind::Disconnected, _)
        ));
        assert!(matches!(
            Failure::from_stop(StopReason::InUse(hr)),
            Failure::Retry(RetryKind::InUse, _)
        ));
        // A stream error and the end of the stream are retried like a lost camera (for a while: `error_cap`).
        assert!(matches!(
            Failure::from_stop(StopReason::EndOfStream),
            Failure::Retry(RetryKind::Error, _)
        ));
        assert!(matches!(
            Failure::from_stop(StopReason::Error(hr)),
            Failure::Retry(RetryKind::Error, r) if r.contains("0xC00D3704")
        ));
        // A format the camera does not list names the format.
        let e = CaptureError::FormatUnavailable {
            wanted: "yuy2 1920x1080".into(),
            available: vec!["mjpeg".into()],
        };
        assert!(matches!(Failure::from_capture(&e), Failure::Format(m) if m.contains("yuy2 1920x1080")));
        assert!(matches!(
            Failure::from_capture(&CaptureError::OwnVirtualCamera),
            Failure::Fatal(_)
        ));
    }

    // ---- P15: robustness -----------------------------------------------------------------------------------------

    /// The `kind` of the newest state event.
    fn last_kind(rig: &Rig) -> Option<&'static str> {
        rig.events
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find_map(|(_, e)| match e {
                EngineEvent::State(s) => Some(s.kind),
                _ => None,
            })
            .flatten()
    }

    fn saved_format(format: VideoFormat) -> CaptureCfg {
        CaptureCfg {
            device: None,
            res_type: ResType::Custom,
            resolution: "1920x1080".into(),
            frame_interval: 166_666,
            video_format: format,
            hw_decode: true,
            buffering: crate::config::Buffering::Auto,
        }
    }

    fn configured(format: VideoFormat) -> SourceSpec {
        SourceSpec::Configured(saved_format(format))
    }

    #[test]
    fn a_stream_error_is_retried_like_a_lost_camera_and_capped() {
        let rig = Rig::new("p15-stream-error.bin", fast());
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        rig.wait("video", Duration::from_secs(10), |r| r.writer_state() == LIVE);

        // A hiccup: the stream fails, the engine reconnects (not fatal), and the camera is back before the cap.
        rig.fake.stream_error.store(true, Ordering::SeqCst);
        rig.wait_state(EngineState::Reconnecting, Duration::from_secs(5));
        assert!(rig.engine.reason().unwrap().contains("stream failed"), "{:?}", rig.engine.reason());
        assert_eq!(last_kind(&rig), Some("error"));
        assert_eq!(rig.writer_state(), STARTING, "the slate says the camera is starting");
        std::thread::sleep(Duration::from_millis(500));
        rig.fake.stream_error.store(false, Ordering::SeqCst);
        rig.wait("live again", Duration::from_secs(5), |r| {
            r.engine.state() == EngineState::Live && r.writer_state() == LIVE
        });
        // (That a camera that stayed up starts a new count instead of adding to the old one is `the_failure_count_*`,
        // on a clock the test controls.)

        // A camera that keeps failing: Reconnecting for `error_cap`, then Stopped with a plain error.
        std::thread::sleep(Duration::from_millis(700)); // up for longer than `stable_after`: a fresh count
        let t = Instant::now();
        rig.fake.stream_error.store(true, Ordering::SeqCst);
        assert!(rig.engine.wait_stopped(Duration::from_secs(10)));
        // not before the cap (the engine cannot know earlier); how much later is no business of a wall clock
        let took = t.elapsed();
        assert!(took >= Duration::from_millis(1400), "stopped after {took:?}");
        let err = rig.engine.last_error().expect("an error");
        assert!(err.contains("kept failing") && err.contains("stream failed"), "{err}");
        assert_eq!(rig.writer_state(), PAUSED);
        assert!(rig.opens() >= 4, "it kept trying: {} opens", rig.opens());
    }

    #[test]
    fn a_camera_that_opens_but_sends_nothing_is_capped_too() {
        let rig = Rig::new("p15-no-frames.bin", fast());
        for _ in 0..50 {
            rig.fake.failures.lock().unwrap().push_back(Failure::from_capture(
                &CaptureError::Timeout("the first frame"),
            ));
        }
        rig.engine.start(settings(bars(30), false));
        rig.wait_state(EngineState::Reconnecting, Duration::from_secs(5));
        assert!(rig.engine.wait_stopped(Duration::from_secs(10)));
        let err = rig.engine.last_error().unwrap();
        assert!(err.contains("kept failing") && err.contains("picture"), "{err}");
        assert!(!err.contains("timed out"), "{err}");
    }

    #[test]
    fn waiting_for_a_busy_camera_has_no_cap() {
        let rig = Rig::new("p15-in-use-nocap.bin", fast());
        for _ in 0..30 {
            rig.fake.failures.lock().unwrap().push_back(Failure::in_use(None));
        }
        rig.engine.start(settings(bars(30), false));
        rig.wait_state(EngineState::Reconnecting, Duration::from_secs(5));
        assert_eq!(last_kind(&rig), Some("in_use"));
        // well past the 1.5 s cap of generic failures
        std::thread::sleep(Duration::from_millis(2500));
        assert_eq!(rig.engine.state(), EngineState::Reconnecting);
        assert!(rig.engine.last_error().is_none());
    }

    #[test]
    fn a_saved_format_the_camera_lacks_falls_back_once_and_says_so() {
        let rig = Rig::new("p15-format.bin", fast());
        rig.fake.lacks.lock().unwrap().push(VideoFormat::Format(PixelFormat::Yuy2));
        rig.engine.start(settings(configured(VideoFormat::Format(PixelFormat::Yuy2)), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        rig.wait("video", Duration::from_secs(10), |r| r.writer_state() == LIVE);

        // the toast names what was asked and what is used
        let events = rig.events.lock().unwrap().clone();
        let fallbacks: Vec<_> = events
            .iter()
            .filter_map(|(_, e)| match e {
                EngineEvent::FormatFallback(f) => Some(f.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            fallbacks,
            vec![FormatFallbackEvent {
                asked: "1920×1080 YUY2 60 fps".into(),
                used: "320×180 NV12 30 fps".into(),
                width: 320,
                height: 180,
                interval: 333_333,
                format: "nv12".into(),
            }]
        );
        assert!(rig.engine.last_error().is_none());
        // one open of the source, with the saved format tried first and the device default second
        assert_eq!(rig.opens(), 1);
        // the engine now runs with the device default, so the same settings are no change for it
        let device_default = match rig.engine.resume_state().unwrap().settings.source {
            SourceSpec::Configured(c) => c,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            (device_default.res_type, device_default.video_format),
            (ResType::Preferred, VideoFormat::Any)
        );
        assert_eq!(device_default.resolution, "320x180", "the size that is used, for the fields");
        assert_eq!(device_default.frame_interval, 333_333, "and the rate that is used");
        rig.engine.set_source(SourceUpdate {
            source: SourceSpec::Configured(device_default),
            flip_vertical: false,
            color: ColorSettings::default(),
            deactivate_when_not_showing: false,
        });
        std::thread::sleep(Duration::from_millis(400));
        assert_eq!(rig.opens(), 1, "no reopen: the fields the UI now shows are what runs");
        assert_eq!(rig.engine.state(), EngineState::Live);
    }

    #[test]
    fn a_missing_format_that_the_default_cannot_replace_is_a_named_error() {
        let rig = Rig::new("p15-format-fatal.bin", fast());
        rig.fake.lacks.lock().unwrap().extend([
            VideoFormat::Format(PixelFormat::Yuy2),
            VideoFormat::Any,
        ]);
        rig.engine.start(settings(configured(VideoFormat::Format(PixelFormat::Yuy2)), false));
        let err = rig.engine.wait_live(Duration::from_secs(5)).unwrap_err();
        assert!(err.contains("1920×1080 YUY2 60 fps"), "names the format: {err}");
        assert!(err.contains("device default"), "{err}");
        assert!(!err.contains("timed out"), "{err}");
        assert!(rig.engine.wait_stopped(Duration::from_secs(5)));
        let stopped = rig
            .events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|(_, e)| match e {
                EngineEvent::State(s) if s.state == EngineState::Stopped => Some(s.clone()),
                _ => None,
            })
            .next_back()
            .unwrap();
        assert_eq!((stopped.reason.as_deref(), stopped.kind), (Some("error"), Some("format")));
        // exactly one fallback: first the saved format, then the device default
        assert_eq!(rig.opens(), 1, "one engine-level open, two attempts inside it");
        // the device default itself is never replaced
        let rig = Rig::new("p15-format-default.bin", fast());
        rig.fake.lacks.lock().unwrap().push(VideoFormat::Any);
        let mut s = saved_format(VideoFormat::Any);
        s.res_type = ResType::Preferred;
        rig.engine.start(settings(SourceSpec::Configured(s), false));
        assert!(rig.engine.wait_live(Duration::from_secs(5)).is_err());
        assert!(!rig
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|(_, e)| matches!(e, EngineEvent::FormatFallback(_))));
    }

    #[test]
    fn a_camera_that_lists_a_format_but_sends_no_picture_in_it_falls_back_too() {
        // the factory's own mapping: no frames in a saved format is a format problem, in the default it is not
        let custom = saved_format(VideoFormat::Format(PixelFormat::Yuy2));
        let r: Result<((), Option<FormatFallback>), Failure> = open_with_fallback(&custom, |c| {
            if is_device_default(c) {
                Ok(())
            } else {
                Err(Failure::Format(format!("the camera delivers no picture in {}", describe_cap(c))))
            }
        });
        let (_, fb) = r.unwrap();
        let fb = fb.unwrap();
        assert_eq!(fb.asked, "1920×1080 YUY2 60 fps");
        assert!(is_device_default(&fb.cfg) && fb.cfg.device == custom.device);
        // other failures pass through untouched, with no second attempt
        let mut calls = 0;
        let r: Result<((), _), Failure> = open_with_fallback(&custom, |_| {
            calls += 1;
            Err(Failure::in_use(None))
        });
        assert!(matches!(r, Err(Failure::Retry(RetryKind::InUse, _))) && calls == 1);
    }

    #[test]
    fn describing_what_was_asked() {
        let mut c = saved_format(VideoFormat::Format(PixelFormat::Mjpeg));
        assert_eq!(describe_cap(&c), "1920×1080 MJPEG 60 fps");
        c.frame_interval = 0;
        assert_eq!(describe_cap(&c), "1920×1080 MJPEG");
        c.video_format = VideoFormat::Any;
        assert_eq!(describe_cap(&c), "1920×1080");
        c.res_type = ResType::Preferred;
        assert_eq!(describe_cap(&c), "device default");
        c.video_format = VideoFormat::Format(PixelFormat::Nv12);
        assert_eq!(describe_cap(&c), "NV12");
        c.res_type = ResType::Custom;
        c.frame_interval = 333_333;
        assert_eq!(describe_cap(&c), "1920×1080 NV12 30 fps");
        c.frame_interval = 333_667;
        assert_eq!(describe_cap(&c), "1920×1080 NV12 29.97 fps");
    }

    #[test]
    fn access_denied_waits_with_the_privacy_reason_and_goes_live_when_access_returns() {
        let rig = Rig::new("p15-privacy.bin", fast());
        rig.fake.privacy.store(true, Ordering::SeqCst);
        rig.engine.start(settings(bars(30), false));
        rig.wait_state(EngineState::Reconnecting, Duration::from_secs(5));
        let reason = rig.engine.reason().unwrap();
        assert!(reason.contains("privacy settings"), "{reason}");
        assert!(!reason.contains("in use"), "{reason}");
        assert_eq!(last_kind(&rig), Some("privacy"));
        assert_eq!(rig.writer_state(), STARTING);
        // not a loop that gives up: retried every interval, well past the cap of generic failures
        let opens = rig.opens();
        std::thread::sleep(Duration::from_millis(2000));
        assert_eq!(rig.engine.state(), EngineState::Reconnecting);
        assert!(rig.opens() >= opens + 3, "retries go on: {} -> {}", opens, rig.opens());
        assert!(rig.engine.last_error().is_none());

        // access is back: live by itself on the next retry, no restart (counted in opens, not in seconds)
        let opens_when_back = rig.opens();
        rig.fake.privacy.store(false, Ordering::SeqCst);
        let back = rig.wait_state(EngineState::Live, Duration::from_secs(10));
        eprintln!("access back -> live in {back:?}");
        assert!(rig.opens() <= opens_when_back + 2, "{} opens before, {} after", opens_when_back, rig.opens());
        rig.wait("video", Duration::from_secs(10), |r| r.writer_state() == LIVE);
        assert!(rig.states().last() == Some(&(EngineState::Live, None)));
    }

    #[test]
    fn the_window_getting_the_focus_back_retries_at_once() {
        let mut t = fast();
        t.retry_interval = Duration::from_secs(30);
        let rig = Rig::new("p15-focus.bin", t);
        rig.fake.privacy.store(true, Ordering::SeqCst);
        rig.engine.start(settings(bars(30), false));
        rig.wait_state(EngineState::Reconnecting, Duration::from_secs(5));
        rig.fake.privacy.store(false, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(rig.engine.state(), EngineState::Reconnecting, "the next retry is 30 s away");
        rig.engine.retry_now();
        rig.wait_state(EngineState::Live, Duration::from_secs(3));
        // a retry nudge while live or stopped does nothing
        rig.engine.retry_now();
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(rig.engine.state(), EngineState::Live);
        assert_eq!(rig.opens(), 2);
    }

    #[test]
    fn access_taken_away_while_live_is_a_privacy_wait_not_an_in_use_one() {
        let rig = Rig::new("p15-privacy-live.bin", fast());
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        // Windows cuts the stream and denies the next open
        rig.fake.privacy.store(true, Ordering::SeqCst);
        rig.fake.stream_error.store(true, Ordering::SeqCst);
        rig.wait("the privacy wait", Duration::from_secs(5), |r| {
            r.engine.state() == EngineState::Reconnecting
                && r.engine.reason().is_some_and(|x| x.contains("privacy"))
        });
        rig.fake.privacy.store(false, Ordering::SeqCst);
        rig.fake.stream_error.store(false, Ordering::SeqCst);
        rig.wait_state(EngineState::Live, Duration::from_secs(3));
    }

    /// The GPU device is lost on the render thread: the engine builds a new device, renderer and filter chain,
    /// opens the camera again and is live without a restart.
    #[test]
    fn a_lost_gpu_device_is_rebuilt_and_the_engine_is_live_again() {
        let rig = Rig::new("p15-gpu.bin", fast());
        // The frames are looked at in the file, which only gets frames while an app reads the camera.
        let _consumer = rig.consumer("p15-gpu.bin");
        // The chain must be rebuilt on the new device too: grey frames before and after.
        rig.engine.set_chain(
            crate::fx::parse_chain(&serde_json::json!([
                { "id": "c", "kind": "color_filter", "enabled": true, "params": { "saturation": -1.0 } }
            ]))
            .unwrap(),
        );
        let device_changes = Arc::new(std::sync::atomic::AtomicU32::new(0));
        struct Tap(Arc<std::sync::atomic::AtomicU32>);
        impl FrameTap for Tap {
            fn on_frame(&mut self, _: &Renderer, _: &RgbaTexture, _: u64) {}
            fn device_changed(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        rig.engine.send(Msg::SetTap(Some(Box::new(Tap(device_changes.clone())))));
        let spread = |rig: &Rig| -> Option<u8> {
            let (w, h, _, buf) = latest(&rig.file)?;
            nv12_to_rgb(&buf, w, h, w as usize, ColorSpace::Bt709, ColorRange::Limited)
                .chunks(3)
                .map(|p| p.iter().max().unwrap() - p.iter().min().unwrap())
                .max()
        };
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        rig.wait("grey video", Duration::from_secs(10), |r| {
            r.writer_state() == LIVE && spread(r).is_some_and(|s| s <= 4)
        });
        assert_eq!(rig.engine.gpu_generation(), 1);
        assert_eq!(rig.opens(), 1);
        let published = rig.engine.stats().published;

        // the device goes
        let lost = Instant::now();
        rig.engine.inject_device_loss(1);
        rig.wait("a new device", Duration::from_secs(10), |r| r.engine.gpu_generation() == 2);
        rig.wait("live again", Duration::from_secs(10), |r| {
            r.engine.state() == EngineState::Live && r.writer_state() == LIVE && r.opens() == 2
        });
        // How long it took is for the log; what has to hold is the order, not a time.
        eprintln!("device lost -> live again in {:?}", lost.elapsed());

        // capture reopened, tap told, chain on the new device: still grey, frames keep coming
        assert_eq!(device_changes.load(Ordering::SeqCst), 1);
        rig.wait("grey video on the new device", Duration::from_secs(10), |r| {
            r.engine.stats().published > published + 3 && spread(r).is_some_and(|s| s <= 4)
        });
        // the file showed the last frame meanwhile: the states are reopening (last frame kept), then live
        let states = rig.states();
        assert_eq!(
            states[2..],
            [
                (EngineState::Standby, Some("reopening".into())),
                (EngineState::Live, None)
            ]
        );
        assert!(rig.engine.last_error().is_none());
        assert_eq!(rig.alive(), 1, "the old camera was released");
    }

    /// Plan 4.9: after a device loss the last frame stays on the file for `gpu_slate_delay` (1 s); a recovery that
    /// takes longer shows the "starting" slate until the first new frame. Driven by the delay (made short here) and
    /// by event order, not by a window of wall-clock time.
    #[test]
    fn a_slow_recovery_from_a_device_loss_shows_the_slate_after_the_delay() {
        let mut t = fast();
        t.gpu_slate_delay = Duration::from_millis(200);
        let rig = Rig::new("p15-gpu-slow.bin", t);
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        rig.wait("video", Duration::from_secs(10), |r| r.writer_state() == LIVE);
        // the camera is held up until the test lets it go: the recovery lasts as long as the test wants
        let (release, hold) = std::sync::mpsc::channel::<()>();
        *rig.fake.open_gate.lock().unwrap() = Some(Mutex::new(hold));
        rig.engine.inject_device_loss(1);
        rig.wait("the device is rebuilt", Duration::from_secs(10), |r| r.engine.gpu_generation() == 2);
        // the loss was announced with the last picture still on the file (the slate comes after the delay)...
        assert_eq!(
            rig.writer_at_state(EngineState::Standby)[1],
            (Some("reopening".into()), LIVE),
            "the last frame is repeated at first"
        );
        // ...and while the camera is still opening, the slate takes over
        rig.wait("the slate", Duration::from_secs(4), |r| r.writer_state() == STARTING);
        assert_eq!(rig.engine.state(), EngineState::Standby, "still opening: the slate came first");
        // the first frame of the new session ends it
        release.send(()).unwrap();
        rig.wait_state(EngineState::Live, Duration::from_secs(10));
        rig.wait("video again", Duration::from_secs(10), |r| r.writer_state() == LIVE);
    }

    /// A recovery that is quicker than the delay never shows the slate.
    #[test]
    fn a_quick_recovery_from_a_device_loss_never_shows_the_slate() {
        let mut t = fast();
        t.gpu_slate_delay = Duration::from_secs(30);
        let rig = Rig::new("p15-gpu-quick.bin", t);
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        rig.wait("video", Duration::from_secs(10), |r| r.writer_state() == LIVE);
        rig.engine.inject_device_loss(1);
        rig.wait("a new device", Duration::from_secs(10), |r| r.engine.gpu_generation() == 2);
        rig.wait("live again", Duration::from_secs(10), |r| {
            r.engine.state() == EngineState::Live && r.writer_state() == LIVE && r.opens() == 2
        });
        // the loss was announced over the last picture, which the file kept (no slate: the delay is far away)
        assert_eq!(rig.writer_at_state(EngineState::Standby)[1], (Some("reopening".into()), LIVE));
    }

    /// L1: the slate that follows a slow recovery is cancelled by a Stop and a Start (and by a failure): it must
    /// not cover a session that is live by then. The slate comes `gpu_slate_delay` after the loss, which is what
    /// the test waits out (there is nothing to observe when it does not come).
    #[test]
    fn a_device_loss_slate_never_covers_a_session_that_started_after_it() {
        let mut t = fast();
        t.gpu_slate_delay = Duration::from_millis(300);
        let rig = Rig::new("p15-gpu-slate-cancel.bin", t);
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        rig.wait("video", Duration::from_secs(10), |r| r.writer_state() == LIVE);
        // the loss, with the camera held up so the recovery is still going on
        let (release, hold) = std::sync::mpsc::channel::<()>();
        *rig.fake.open_gate.lock().unwrap() = Some(Mutex::new(hold));
        let lost = Instant::now();
        rig.engine.inject_device_loss(1);
        rig.wait("a new device", Duration::from_secs(10), |r| r.engine.gpu_generation() == 2);
        // Stop and Start before the slate is due. The Stop waits for the session thread that is held up in its open,
        // so the camera is let go first (a gate whose sender is gone does not hold anything back).
        rig.engine.stop();
        drop(release);
        assert!(rig.engine.wait_stopped(Duration::from_secs(10)));
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        rig.wait("video", Duration::from_secs(10), |r| r.writer_state() == LIVE);
        // wait out the delay of the loss that is long gone
        let due = Duration::from_millis(300) * 3;
        if let Some(rest) = due.checked_sub(lost.elapsed()) {
            std::thread::sleep(rest);
        }
        assert_eq!(rig.engine.state(), EngineState::Live);
        assert_eq!(rig.writer_state(), LIVE, "the old slate covered the live picture");
    }

    #[test]
    fn three_device_losses_in_a_row_stop_the_engine_with_an_error() {
        let rig = Rig::new("p15-gpu-three.bin", fast());
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        // every frame of every session fails with a lost device
        rig.engine.shared.gpu_faults.store(1_000, Ordering::SeqCst);
        assert!(rig.engine.wait_stopped(Duration::from_secs(20)));
        let err = rig.engine.last_error().unwrap();
        assert!(err.contains("graphics device was lost 3 times"), "{err}");
        assert_eq!(rig.engine.gpu_generation(), 3, "two rebuilds, then it gave up");
        assert_eq!(rig.writer_state(), PAUSED);
        assert_eq!(rig.alive(), 0);
        // Start works again afterwards (the injected fault is gone)
        rig.engine.shared.gpu_faults.store(0, Ordering::SeqCst);
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        assert_eq!(rig.engine.last_error(), None);
    }

    #[test]
    fn device_losses_far_apart_are_not_in_a_row() {
        let mut t = fast();
        t.gpu_failure_window = Duration::from_millis(500);
        let rig = Rig::new("p15-gpu-apart.bin", t);
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        for generation in 2..=5 {
            std::thread::sleep(Duration::from_millis(700));
            rig.engine.shared.gpu_faults.store(1, Ordering::SeqCst);
            rig.wait("a new device", Duration::from_secs(10), |r| {
                r.engine.gpu_generation() == generation
            });
            rig.wait("live again", Duration::from_secs(10), |r| {
                r.engine.state() == EngineState::Live && r.writer_state() == LIVE
            });
        }
        assert!(rig.engine.last_error().is_none());
    }

    #[test]
    fn a_stream_that_dies_with_the_device_is_a_device_loss_not_a_camera_error() {
        // the failure type: the same stop reason is a camera error without a lost device, so it is the check on the
        // device that decides (the render loop asks `removed_reason` when a stream stops); here the type is pinned
        assert!(crate::gpu::is_device_lost_hr(windows::Win32::Graphics::Dxgi::DXGI_ERROR_DEVICE_REMOVED));
        assert!(crate::gpu::is_device_lost_hr(windows::Win32::Graphics::Dxgi::DXGI_ERROR_DEVICE_RESET));
        assert!(!crate::gpu::is_device_lost_hr(HRESULT(0xC00D_3704_u32 as i32)));
        let e = GpuError::Api(windows::core::Error::from(
            windows::Win32::Graphics::Dxgi::DXGI_ERROR_DEVICE_REMOVED,
        ));
        assert!(e.is_device_lost());
        assert!(!GpuError::Invalid("x".into()).is_device_lost());
        let gpu = GpuDevice::new(AdapterKind::Warp).unwrap();
        assert_eq!(gpu.removed_reason(), None, "a working device reports nothing");
    }

    /// Plan 4.9 "Device unplugged": the removal notification and the arrival of the same device, with and without
    /// the `\global` reference string the symbolic links of Media Foundation carry.
    #[test]
    fn hotplug_notices_move_the_engine_with_or_without_the_reference_string() {
        let rig = Rig::new("p15-hotplug.bin", {
            let mut t = fast();
            t.retry_interval = Duration::from_secs(30);
            t
        });
        // the settings name the camera, as the app's do (an arrival counts only when it is that camera)
        let mut cap = saved_format(VideoFormat::Any);
        cap.res_type = ResType::Preferred;
        cap.device = Some(FAKE_LINK.into());
        rig.engine.start(settings(SourceSpec::Configured(cap), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        let bare = FAKE_LINK.trim_end_matches("\\global").to_uppercase();
        assert!(!bare.contains("GLOBAL"));
        // the notice names the camera that is open: the source has not failed by itself
        rig.engine.send(Msg::Hotplug(HotplugEvent::Removal { link: bare.clone() }));
        rig.wait_state(EngineState::Reconnecting, Duration::from_secs(3));
        assert!(rig.engine.reason().unwrap().contains("disconnected"));
        assert_eq!(last_kind(&rig), Some("disconnected"));
        assert!(rig
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|(_, e)| *e == EngineEvent::DevicesChanged));
        // another camera arriving changes nothing; the same one (with the string, or the same vid/pid on another
        // port) is retried at once although the next retry is 30 s away
        let other = "\\\\?\\usb#vid_046d&pid_085e&mi_00#6&aaaa&0&0003#{e5323777-f976-4f5b-9b55-b94699c46e44}";
        rig.engine.send(Msg::Hotplug(HotplugEvent::Arrival { link: other.into() }));
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(rig.engine.state(), EngineState::Reconnecting);
        rig.engine.send(Msg::Hotplug(HotplugEvent::Arrival { link: bare }));
        let back = rig.wait_state(EngineState::Live, Duration::from_secs(3));
        eprintln!("arrival -> live in {back:?}");
        assert_eq!(rig.opens(), 2);
    }

    #[test]
    fn a_removal_of_another_camera_changes_nothing() {
        let rig = Rig::new("p15-hotplug-other.bin", fast());
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        let other = "\\\\?\\usb#vid_046d&pid_085e&mi_00#6&aaaa&0&0003#{e5323777-f976-4f5b-9b55-b94699c46e44}\\global";
        rig.engine.send(Msg::Hotplug(HotplugEvent::Removal { link: other.into() }));
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(rig.engine.state(), EngineState::Live);
        // but the list of cameras changed, and the window is told
        assert!(rig
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|(_, e)| *e == EngineEvent::DevicesChanged));
    }

    /// Rough edge 3: an output change while the camera opens must reach the file at once, not when the first frame
    /// arrives, so a consumer that attaches meanwhile sees the new list.
    #[test]
    fn an_output_change_while_the_camera_opens_is_advertised_at_once() {
        let rig = Rig::new("p15-advertise.bin", fast());
        // the sizes in the file, each once (every size is listed in more than one pixel format)
        let advertised = |r: &Rig| {
            let h = r.file.shared().header();
            let mut sizes = h.advertised[..h.advertised_count as usize]
                .iter()
                .map(|f| (f.width, f.height))
                .collect::<Vec<_>>();
            sizes.dedup();
            sizes
        };
        // a first run leaves 1280x720 in the file
        let out = |w, h| OutputSettings {
            width: Some(w),
            height: Some(h),
            fps: Some(30),
            extra_sizes: false,
            ..OutputSettings::default()
        };
        rig.engine.start(Settings {
            output: out(1280, 720),
            ..settings(bars(30), false)
        });
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        rig.wait("1280x720", Duration::from_secs(5), |r| advertised(r) == vec![(1280, 720)]);
        rig.engine.stop();
        assert!(rig.engine.wait_stopped(Duration::from_secs(5)));

        // the next start is slow to open the camera (two seconds); the output is 640x360 and then changed again
        *rig.fake.open_delay.lock().unwrap() = Duration::from_millis(1500);
        rig.engine.start(Settings {
            output: out(640, 360),
            ..settings(bars(30), false)
        });
        rig.wait("the new list while opening", Duration::from_millis(600), |r| {
            r.engine.state() == EngineState::Standby && advertised(r) == vec![(640, 360)]
        });
        assert_eq!(rig.engine.reason().as_deref(), Some("opening"));
        rig.engine.set_output(out(960, 540));
        rig.wait("the changed list while opening", Duration::from_millis(600), |r| {
            r.engine.state() == EngineState::Standby && advertised(r) == vec![(960, 540)]
        });
        // and it stays that way once the camera is open
        rig.wait_state(EngineState::Live, Duration::from_secs(10));
        assert_eq!(advertised(&rig), vec![(960, 540)]);
    }

    /// An output change while the camera is lost (Reconnecting) is advertised too.
    #[test]
    fn an_output_change_while_reconnecting_is_advertised_at_once() {
        let rig = Rig::new("p15-advertise-lost.bin", fast());
        let out = |w, h| OutputSettings {
            width: Some(w),
            height: Some(h),
            fps: Some(30),
            extra_sizes: false,
            ..OutputSettings::default()
        };
        rig.engine.start(Settings {
            output: out(1280, 720),
            ..settings(bars(30), false)
        });
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        rig.fake.gone.store(true, Ordering::SeqCst);
        rig.wait_state(EngineState::Reconnecting, Duration::from_secs(5));
        rig.engine.set_output(out(640, 360));
        rig.wait("the list while reconnecting", Duration::from_millis(500), |r| {
            let h = r.file.shared().header();
            h.advertised_count > 0 && h.advertised[0].width == 640
        });
    }

    #[test]
    fn the_engine_can_be_brought_back_to_what_another_one_was_doing() {
        let rig = Rig::new("p15-resume.bin", fast());
        assert!(rig.engine.resume_state().is_none(), "a stopped engine has nothing to resume");
        rig.engine.set_chain(
            crate::fx::parse_chain(&serde_json::json!([
                { "id": "c", "kind": "color_filter", "enabled": true, "params": {} }
            ]))
            .unwrap(),
        );
        rig.engine.controls().set_record(Some(controls_record(33)));
        let out = OutputSettings {
            width: Some(640),
            height: Some(360),
            ..OutputSettings::default()
        };
        rig.engine.start(Settings {
            output: OutputSettings::default(),
            ..settings(bars(30), true)
        });
        rig.engine.set_output(out);
        rig.engine.set_pause(true);
        rig.wait("the idle standby", Duration::from_secs(5), |r| {
            r.engine.state() == EngineState::Standby
        });
        std::thread::sleep(Duration::from_millis(100));
        let s = rig.engine.resume_state().expect("started");
        assert_eq!(s.settings.output.width, Some(640), "the live output, not the one at Start");
        assert!(s.settings.deactivate_when_not_showing && s.paused);
        assert_eq!(s.chain.len(), 1);
        assert!(s.record.is_some());

        // a second engine takes it over
        let other = Rig::new("p15-resume-2.bin", fast());
        other.engine.set_window_visible(true);
        other.engine.resume(s);
        other.wait_state(EngineState::Live, Duration::from_secs(10));
        assert!(other.engine.paused());
        assert_eq!(other.engine.resume_state().unwrap().chain.len(), 1);

        // after a stop or a fatal error there is nothing to resume
        other.engine.stop();
        assert!(other.engine.wait_stopped(Duration::from_secs(5)));
        assert!(other.engine.resume_state().is_none());
        assert!(!other.engine.is_started());
    }

    // ---- review fixes: GPU device, pause, queued commands, formats, failure counts ------------------------------------

    fn fallbacks(rig: &Rig) -> Vec<FormatFallbackEvent> {
        rig.events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|(_, e)| match e {
                EngineEvent::FormatFallback(f) => Some(f.clone()),
                _ => None,
            })
            .collect()
    }

    fn pauses(rig: &Rig) -> Vec<bool> {
        rig.events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|(_, e)| match e {
                EngineEvent::Paused(p) => Some(p.paused),
                _ => None,
            })
            .collect()
    }

    /// H1: a device that was removed while nothing used it (a driver update while the app sat in the tray, the camera
    /// off in standby) is replaced before the camera opens, not handed to the camera.
    #[test]
    fn a_device_lost_while_idle_is_rebuilt_when_the_camera_wakes() {
        let rig = Rig::new("rf-h1-idle.bin", fast());
        rig.engine.start(settings(bars(30), true));
        rig.wait("idle standby", Duration::from_secs(5), |r| {
            r.engine.state() == EngineState::Standby && r.engine.reason().as_deref() == Some("idle")
        });
        assert_eq!(rig.opens(), 0);
        rig.gpu.simulate_removed.store(true, Ordering::SeqCst);
        rig.engine.set_window_visible(true);
        rig.wait_state(EngineState::Live, Duration::from_secs(10));
        assert_eq!(rig.engine.gpu_generation(), 2, "a new device");
        assert_eq!(*rig.fake.opened_on_removed.lock().unwrap(), vec![false], "the camera got a working device");
        assert!(rig.engine.last_error().is_none());
        assert!(fallbacks(&rig).is_empty(), "a lost device is not a format problem");
        rig.wait("video", Duration::from_secs(10), |r| r.writer_state() == LIVE);
    }

    /// H1: a device lost while the engine is stopped is replaced by the next Start.
    #[test]
    fn a_device_lost_while_stopped_is_rebuilt_by_the_next_start() {
        let rig = Rig::new("rf-h1-stopped.bin", fast());
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        rig.engine.stop();
        assert!(rig.engine.wait_stopped(Duration::from_secs(10)));
        rig.gpu.simulate_removed.store(true, Ordering::SeqCst);
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        assert_eq!(rig.engine.gpu_generation(), 2);
        assert_eq!(*rig.fake.opened_on_removed.lock().unwrap(), vec![false, false]);
        assert!(rig.engine.last_error().is_none());
    }

    /// H1: the open itself reports the loss (the decoder or the media type fails because the device went): it is
    /// a device loss, not a format problem (which would replace the saved format) or a camera error (30 s of retries).
    #[test]
    fn a_device_loss_that_shows_in_the_open_is_rebuilt_and_not_called_a_format_error() {
        let rig = Rig::new("rf-h1-open.bin", fast());
        rig.fake.lose_device_in_open.store(true, Ordering::SeqCst);
        rig.engine.start(settings(configured(VideoFormat::Format(PixelFormat::Yuy2)), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        assert_eq!(rig.engine.gpu_generation(), 2);
        assert_eq!(rig.opens(), 2, "opened again on the new device");
        assert!(fallbacks(&rig).is_empty());
        assert!(rig.engine.last_error().is_none());
        assert!(rig.states().iter().all(|(s, _)| *s != EngineState::Stopped));
        // and the saved format was kept: both opens asked for it
        let specs = rig.fake.specs.lock().unwrap().clone();
        assert!(specs.iter().all(|s| *s == configured(VideoFormat::Format(PixelFormat::Yuy2))));
    }

    /// The same, in the classification: a device-lost code is a GPU loss before it is anything else.
    #[test]
    fn device_lost_codes_are_a_gpu_loss_first() {
        use windows::core::Error;
        use windows::Win32::Graphics::Dxgi::{DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET};
        for (what, code) in [
            ("SetCurrentMediaType", DXGI_ERROR_DEVICE_REMOVED),
            ("first ReadSample", DXGI_ERROR_DEVICE_RESET),
            ("MFCreateSourceReaderFromMediaSource", DXGI_ERROR_DEVICE_REMOVED),
        ] {
            let e = CaptureError::Api { what, error: Error::from(code) };
            assert!(matches!(Failure::from_capture(&e), Failure::GpuLost(m) if m.contains("0x887A0005") || m.contains("0x887A0007")), "{what}");
        }
        assert!(matches!(
            Failure::from_stop(StopReason::Error(DXGI_ERROR_DEVICE_REMOVED)),
            Failure::GpuLost(_)
        ));
        // without a lost device the same call is what it was: a format problem
        let e = CaptureError::Api { what: "SetCurrentMediaType", error: Error::from(HRESULT(0xC00D_36B4_u32 as i32)) };
        assert!(matches!(Failure::from_capture(&e), Failure::Format(_)));
    }

    /// H1: after the engine stopped itself for repeated losses it holds the lost device; the next Start rebuilds it.
    #[test]
    fn start_after_the_stop_by_repeated_losses_builds_a_new_device() {
        let rig = Rig::new("rf-h1-three.bin", fast());
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        rig.engine.inject_device_loss(1_000);
        assert!(rig.engine.wait_stopped(Duration::from_secs(20)));
        assert!(rig.engine.last_error().unwrap().contains("3 times"));
        assert_eq!(rig.engine.gpu_generation(), 3);
        rig.engine.inject_device_loss(0);
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        assert_eq!(rig.engine.gpu_generation(), 4, "the device of the stop was lost: a new one");
        assert!(rig.engine.last_error().is_none());
        assert!(rig.fake.opened_on_removed.lock().unwrap().iter().all(|r| !r));
    }

    /// L9: the render loop looks at the device after a frame (a draw call reports nothing when the device is gone).
    #[test]
    fn a_device_that_goes_between_frames_is_found_after_the_frame() {
        let rig = Rig::new("rf-l9-frame.bin", fast());
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        rig.wait("video", Duration::from_secs(10), |r| r.writer_state() == LIVE);
        // no frame fails: only the device reports itself removed
        rig.gpu.simulate_removed.store(true, Ordering::SeqCst);
        rig.wait("a new device", Duration::from_secs(10), |r| r.engine.gpu_generation() == 2);
        rig.wait_state(EngineState::Live, Duration::from_secs(10));
        assert_eq!(rig.opens(), 2);
        assert!(rig.engine.last_error().is_none());
    }

    /// L9: a stream that stops because the device went is a device loss, not a camera error (no 30 s of retries).
    #[test]
    fn a_stream_that_stops_with_the_device_is_a_device_loss() {
        let rig = Rig::new("rf-l9-stream.bin", fast());
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        // the stream stops with a camera error code, and the device is gone in the same read: no frame is drawn
        // between them, so only the stop can show the loss
        rig.fake.lose_device_with_error.store(true, Ordering::SeqCst);
        rig.fake.stream_error_once.store(true, Ordering::SeqCst);
        rig.wait("a new device", Duration::from_secs(10), |r| r.engine.gpu_generation() == 2);
        rig.wait_state(EngineState::Live, Duration::from_secs(10));
        assert!(
            rig.states().iter().all(|(s, _)| *s != EngineState::Reconnecting),
            "never a camera problem: {:?}",
            rig.states()
        );
    }

    /// M6: the heartbeat of the file goes on while the GPU device is rebuilt (the virtual camera says "not running"
    /// after a second without one).
    #[test]
    fn the_heartbeat_goes_on_while_the_gpu_device_is_rebuilt() {
        let rig = Rig::new("rf-m6.bin", fast());
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        rig.wait("video", Duration::from_secs(10), |r| r.writer_state() == LIVE);
        // a rebuild that takes two seconds on top of the real work
        *rig.engine.shared.rebuild_delay.lock().unwrap() = Duration::from_secs(2);
        let age_ms = |r: &Rig| {
            let h = r.file.shared().header();
            vcam_protocol::qpc_to_hns(vcam_protocol::qpc_now().saturating_sub(h.writer_heartbeat)) / 10_000
        };
        rig.engine.inject_device_loss(1);
        let mut worst = 0;
        let end = Instant::now() + Duration::from_secs(20);
        while rig.engine.gpu_generation() < 2 || rig.engine.state() != EngineState::Live {
            worst = worst.max(age_ms(&rig));
            assert!(Instant::now() < end, "no new device");
            std::thread::sleep(Duration::from_millis(5));
        }
        eprintln!("oldest heartbeat while the device was rebuilt: {worst} ms");
        assert!(worst < 500, "the heartbeat was {worst} ms old");
    }

    /// A device rebuild that never returns is a failed rebuild after `gpu_rebuild_timeout`; three stop the engine.
    #[test]
    fn a_gpu_rebuild_that_never_returns_stops_the_engine_with_an_error() {
        let mut t = fast();
        t.gpu_rebuild_timeout = Duration::from_millis(300);
        let rig = Rig::new("rf-rebuild-hang.bin", t);
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        *rig.engine.shared.rebuild_delay.lock().unwrap() = Duration::from_secs(3600);
        let t0 = Instant::now();
        rig.engine.inject_device_loss(1);
        assert!(rig.engine.wait_stopped(Duration::from_secs(10)), "{:?}", rig.states());
        let err = rig.engine.last_error().unwrap();
        assert!(err.contains("could not be rebuilt") && err.contains("took longer"), "{err}");
        assert!(t0.elapsed() < Duration::from_secs(8), "{:?}", t0.elapsed());
        assert_eq!(rig.engine.gpu_generation(), 1, "no device was ever built");
        assert_eq!(rig.alive(), 0);
    }

    /// A Stop or a Shutdown sent while the device is being rebuilt is read within a tick, not after the rebuild.
    #[test]
    fn a_stop_or_shutdown_during_a_gpu_rebuild_is_honoured_at_once() {
        let rebuilding = |name: &str| {
            let rig = Rig::new(name, fast());
            rig.engine.start(settings(bars(30), false));
            rig.engine.wait_live(Duration::from_secs(20)).unwrap();
            *rig.engine.shared.rebuild_delay.lock().unwrap() = Duration::from_secs(3600);
            rig.engine.inject_device_loss(1);
            rig.wait("the rebuild", Duration::from_secs(10), |r| {
                r.engine.state() == EngineState::Standby && r.engine.reason().as_deref() == Some("reopening")
            });
            std::thread::sleep(Duration::from_millis(150));
            rig
        };
        // Stop: the engine stops without opening a camera
        let rig = rebuilding("rf-rebuild-stop.bin");
        let opens = rig.opens();
        let t0 = Instant::now();
        rig.engine.stop();
        assert!(rig.engine.wait_stopped(Duration::from_secs(3)), "{:?}", rig.states());
        assert!(t0.elapsed() < Duration::from_secs(2), "{:?}", t0.elapsed());
        assert_eq!(rig.opens(), opens, "no camera was opened for the engine that was told to stop");
        assert!(rig.engine.last_error().is_none());
        drop(rig);
        // Shutdown: the control thread ends
        let rig = rebuilding("rf-rebuild-shutdown.bin");
        let t0 = Instant::now();
        rig.engine.close();
        assert!(t0.elapsed() < Duration::from_secs(2), "{:?}", t0.elapsed());
    }

    /// The real lookup (registry and the camera service's log) on the control thread, at every retry: the file's
    /// heartbeat does not stall.
    #[test]
    fn the_heartbeat_goes_on_while_the_camera_log_is_read_at_every_retry() {
        let rig = Rig::new("rf-camlog-heartbeat.bin", fast());
        rig.fake.real_users.store(true, Ordering::SeqCst);
        for _ in 0..400 {
            rig.fake.failures.lock().unwrap().push_back(Failure::in_use(Some(HRESULT(0xC00D_3704_u32 as i32))));
        }
        rig.engine.start(settings(bars(30), false));
        rig.wait_state(EngineState::Reconnecting, Duration::from_secs(5));
        let age_ms = |r: &Rig| {
            let h = r.file.shared().header();
            vcam_protocol::qpc_to_hns(vcam_protocol::qpc_now().saturating_sub(h.writer_heartbeat)) / 10_000
        };
        let end = Instant::now() + Duration::from_secs(5);
        let mut worst = 0;
        while Instant::now() < end {
            worst = worst.max(age_ms(&rig));
            std::thread::sleep(Duration::from_millis(5));
        }
        let lookups = rig.fake.targets.lock().unwrap().len();
        eprintln!("oldest heartbeat over {lookups} real lookups: {worst} ms");
        assert!(lookups >= 10, "{lookups} lookups in 5 s at 300 ms retries");
        assert!(worst < 500, "the heartbeat was {worst} ms old");
        rig.fake.failures.lock().unwrap().clear();
    }

    /// M5: the failure count of generic failures starts over after idle standby.
    #[test]
    fn the_failure_count_does_not_run_through_idle_standby() {
        let mut t = fast();
        t.error_cap = Duration::from_millis(1000);
        let rig = Rig::new("rf-m5.bin", t);
        let no_picture = || Failure::from_capture(&CaptureError::Timeout("the first frame"));
        // the open and its first retry (300 ms later) fail: the run is 300 ms old when the camera is given up
        for _ in 0..2 {
            rig.fake.failures.lock().unwrap().push_back(no_picture());
        }
        rig.engine.set_window_visible(true);
        rig.engine.start(settings(bars(30), true));
        rig.wait_state(EngineState::Reconnecting, Duration::from_secs(5));
        // nobody watches: the camera is given up, and stays so for longer than the cap
        rig.engine.set_window_visible(false);
        rig.wait("idle standby", Duration::from_secs(5), |r| {
            r.engine.state() == EngineState::Standby && r.engine.reason().as_deref() == Some("idle")
        });
        std::thread::sleep(Duration::from_millis(1100));
        // the first failure after the wake-up is the first of a new run, not the one that ends the old
        rig.fake.failures.lock().unwrap().push_back(no_picture());
        rig.engine.set_window_visible(true);
        rig.wait_state(EngineState::Reconnecting, Duration::from_secs(5));
        assert!(rig.engine.last_error().is_none());
        rig.wait_state(EngineState::Live, Duration::from_secs(5));
    }

    /// M5, on a clock the test controls: the run starts at the first generic failure and a camera that was up
    /// starts a new one.
    #[test]
    fn the_failure_count_has_the_run_the_timings_say() {
        let t = fast(); // cap 1500 ms, stable after 600 ms
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut first = None;
        let note = |first: &mut Option<Instant>, kind, lived: Option<u64>, now: u64| {
            streak_exhausted(first, kind, lived.map(Duration::from_millis), at(now), &t)
        };
        assert!(!note(&mut first, RetryKind::Error, None, 0));
        assert!(!note(&mut first, RetryKind::NoFrames, None, 1_000));
        assert!(note(&mut first, RetryKind::Decoder, None, 1_500), "the cap, counted from the first");
        // a camera that was up for 600 ms or more starts a new run
        assert!(!note(&mut first, RetryKind::Error, Some(600), 1_600));
        assert_eq!(first, Some(at(1_600)));
        assert!(!note(&mut first, RetryKind::Error, Some(599), 3_000), "up for less: the same run");
        assert!(note(&mut first, RetryKind::Error, Some(599), 3_100));
        // a failure that may last as long as it likes ends the run and counts for nothing
        for kind in [RetryKind::InUse, RetryKind::AccessDenied, RetryKind::Disconnected, RetryKind::NotFound, RetryKind::Privacy] {
            first = Some(at(0));
            assert!(!note(&mut first, kind, None, 100_000), "{kind:?}");
            assert_eq!(first, None, "{kind:?}");
        }
    }

    /// M4: a camera that sends no picture in the saved format is taken for lacking it on the first open of a Start
    /// only. The device default runs for that Start; the next Start tries the saved format again.
    #[test]
    fn a_saved_format_without_a_picture_falls_back_for_one_start_only() {
        let rig = Rig::new("rf-m4-first.bin", fast());
        let saved = || configured(VideoFormat::Format(PixelFormat::Yuy2));
        rig.fake.silent_formats.lock().unwrap().push(VideoFormat::Format(PixelFormat::Yuy2));
        rig.engine.start(settings(saved(), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        assert_eq!(fallbacks(&rig).len(), 1);
        assert_eq!(fallbacks(&rig)[0].format, "nv12");
        assert_eq!(*rig.fake.first_opens.lock().unwrap(), vec![true]);
        // it runs the device default, and the owner's `set_source` with those same settings is no change
        let running = match rig.engine.resume_state().unwrap().settings.source {
            SourceSpec::Configured(c) => c,
            other => panic!("{other:?}"),
        };
        assert_eq!((running.res_type, running.video_format), (ResType::Preferred, VideoFormat::Any));

        // Stop, and the camera is fine again: the next Start asks for the saved format, and gets it
        rig.engine.stop();
        assert!(rig.engine.wait_stopped(Duration::from_secs(10)));
        rig.fake.silent_formats.lock().unwrap().clear();
        rig.engine.start(settings(saved(), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        assert_eq!(*rig.fake.specs.lock().unwrap().last().unwrap(), saved());
        assert_eq!(fallbacks(&rig).len(), 1, "no second fallback");
        let running = match rig.engine.resume_state().unwrap().settings.source {
            SourceSpec::Configured(c) => c,
            other => panic!("{other:?}"),
        };
        assert_eq!(running.video_format, VideoFormat::Format(PixelFormat::Yuy2), "the saved format runs");
        assert_eq!(*rig.fake.first_opens.lock().unwrap(), vec![true, true], "each Start opens as a first open");
    }

    /// M4: once a source has opened, a camera that comes back after an unplug and is slow with its first picture is
    /// retried in the same format; the saved format is not replaced.
    #[test]
    fn a_slow_first_picture_after_a_reconnect_is_retried_in_the_same_format() {
        let mut t = fast();
        t.error_cap = Duration::from_secs(60);
        let rig = Rig::new("rf-m4-retry.bin", t);
        let saved = || configured(VideoFormat::Format(PixelFormat::Yuy2));
        rig.engine.start(settings(saved(), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        // unplugged, and back with a camera that sends nothing for a while
        rig.fake.gone.store(true, Ordering::SeqCst);
        rig.wait_state(EngineState::Reconnecting, Duration::from_secs(5));
        rig.fake.silent_formats.lock().unwrap().push(VideoFormat::Format(PixelFormat::Yuy2));
        rig.fake.gone.store(false, Ordering::SeqCst);
        let opens = rig.opens();
        rig.wait("two retries of the camera that is back", Duration::from_secs(10), |r| r.opens() >= opens + 3);
        assert_eq!(rig.engine.state(), EngineState::Reconnecting);
        assert!(rig.engine.reason().unwrap().contains("picture"), "{:?}", rig.engine.reason());
        assert!(fallbacks(&rig).is_empty(), "the saved format is not replaced by the device default");
        // it answers in the end: live in the saved format
        rig.fake.silent_formats.lock().unwrap().clear();
        rig.wait_state(EngineState::Live, Duration::from_secs(10));
        assert!(fallbacks(&rig).is_empty());
        let specs = rig.fake.specs.lock().unwrap().clone();
        assert!(specs.iter().all(|s| *s == saved()), "every open asked for the saved format");
        let firsts = rig.fake.first_opens.lock().unwrap().clone();
        assert!(firsts[0]);
        assert!(firsts[1..].iter().all(|f| !f), "{firsts:?}");
    }

    /// M4: choosing the saved format again (a `set_source` with it) is a new first open: the fallback applies again.
    #[test]
    fn trying_the_saved_format_again_can_fall_back_again() {
        let rig = Rig::new("rf-m4-again.bin", fast());
        let saved = || configured(VideoFormat::Format(PixelFormat::Yuy2));
        rig.fake.silent_formats.lock().unwrap().push(VideoFormat::Format(PixelFormat::Yuy2));
        rig.engine.start(settings(saved(), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        assert_eq!(fallbacks(&rig).len(), 1);
        rig.engine.set_source(SourceUpdate {
            source: saved(),
            flip_vertical: false,
            color: ColorSettings::default(),
            deactivate_when_not_showing: false,
        });
        rig.wait("the second fallback", Duration::from_secs(10), |r| fallbacks(r).len() == 2);
        rig.wait_state(EngineState::Live, Duration::from_secs(10));
        assert_eq!(*rig.fake.first_opens.lock().unwrap(), vec![true, true]);
    }

    /// M1: pause is reported as it changes, and a Stop or a fatal error that clears it says so.
    #[test]
    fn pause_is_reported_when_it_changes() {
        let rig = Rig::new("rf-m1.bin", fast());
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        assert!(pauses(&rig).is_empty(), "nothing to report: it was never paused");
        rig.engine.set_pause(true);
        rig.wait("paused", Duration::from_secs(5), |r| pauses(r) == vec![true]);
        rig.engine.set_pause(true);
        rig.engine.set_pause(false);
        rig.wait("resumed", Duration::from_secs(5), |r| pauses(r) == vec![true, false]);
        rig.engine.set_pause(true);
        rig.wait("paused again", Duration::from_secs(5), |r| pauses(r) == vec![true, false, true]);
        // a Stop clears the pause: the window is told
        rig.engine.stop();
        assert!(rig.engine.wait_stopped(Duration::from_secs(5)));
        rig.wait("cleared by the stop", Duration::from_secs(5), |r| pauses(r) == vec![true, false, true, false]);
        // a pause that arrives while stopped has nothing to pause
        rig.engine.set_pause(true);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(pauses(&rig), vec![true, false, true, false]);
        assert!(!rig.engine.paused());
    }

    /// M3: what a snapshot of the engine says, asked while the control thread has not read the messages that were
    /// queued before the question.
    fn frozen_snapshot(rig: &Rig, queue: impl FnOnce(&Engine)) -> Option<ResumeState> {
        let engine = &rig.engine;
        let gate = lock(&engine.shared.control_gate);
        queue(engine);
        let asked = engine.shared.snapshots.load(Ordering::SeqCst);
        std::thread::scope(|s| {
            let answer = s.spawn(move || engine.resume_state());
            let end = Instant::now() + Duration::from_secs(10);
            // The question is on its way once it is counted; the answer cannot come before the gate is let go.
            while engine.shared.snapshots.load(Ordering::SeqCst) == asked && !answer.is_finished() {
                assert!(Instant::now() < end, "the question was never asked");
                std::thread::yield_now();
            }
            drop(gate);
            answer.join().unwrap()
        })
    }

    fn running_rig(name: &str) -> Rig {
        let rig = Rig::new(name, fast());
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        rig
    }

    fn output_of(width: u32) -> OutputSettings {
        OutputSettings {
            width: Some(width),
            height: Some(width * 9 / 16),
            ..OutputSettings::default()
        }
    }

    #[test]
    fn the_snapshot_sees_a_stop_that_was_queued_before_it() {
        let rig = running_rig("rf-m3-stop.bin");
        assert!(frozen_snapshot(&rig, |e| e.stop()).is_none(), "the engine is stopped: nothing to resume");
        assert!(!rig.engine.is_started());
    }

    #[test]
    fn the_snapshot_sees_a_start_that_was_queued_before_it() {
        let rig = running_rig("rf-m3-start.bin");
        rig.engine.stop();
        assert!(rig.engine.wait_stopped(Duration::from_secs(10)));
        let s = frozen_snapshot(&rig, |e| {
            e.start(Settings {
                output: output_of(800),
                ..settings(bars(25), true)
            })
        })
        .expect("started");
        assert_eq!(s.settings.source, bars(25));
        assert_eq!(s.settings.output.width, Some(800));
        assert!(s.settings.deactivate_when_not_showing);
        // a Start that waits in the queue counts as started
        rig.engine.stop();
        assert!(rig.engine.wait_stopped(Duration::from_secs(10)));
        let engine = &rig.engine;
        let gate = lock(&engine.shared.control_gate);
        engine.start(settings(bars(30), false));
        let started = std::thread::scope(|sc| {
            let asked = engine.shared.snapshots.load(Ordering::SeqCst);
            let h = sc.spawn(move || engine.is_started());
            while engine.shared.snapshots.load(Ordering::SeqCst) == asked && !h.is_finished() {
                std::thread::yield_now();
            }
            drop(gate);
            h.join().unwrap()
        });
        assert!(started);
    }

    #[test]
    fn the_snapshot_sees_settings_changes_that_were_queued_before_it() {
        let rig = running_rig("rf-m3-settings.bin");
        let chain = crate::fx::parse_chain(&serde_json::json!([
            { "id": "c", "kind": "color_filter", "enabled": true, "params": {} }
        ]))
        .unwrap();
        let s = frozen_snapshot(&rig, |e| {
            e.set_output(output_of(640));
            e.set_chain(chain);
            e.set_source(SourceUpdate {
                source: bars(15),
                flip_vertical: true,
                color: ColorSettings::default(),
                deactivate_when_not_showing: true,
            });
            e.set_pause(true);
        })
        .expect("running");
        assert_eq!(s.settings.output.width, Some(640));
        assert_eq!(s.chain.len(), 1);
        assert_eq!(s.settings.source, bars(15));
        assert!(s.settings.flip_vertical && s.settings.deactivate_when_not_showing);
        assert!(s.paused);
    }

    /// L7: a deterministic error stops at once with its name; one that may pass is retried.
    #[test]
    fn errors_that_cannot_change_stop_at_once_and_the_others_are_retried() {
        use windows::core::Error;
        let api = |what: &'static str, code: u32| CaptureError::Api { what, error: Error::from(HRESULT(code as i32)) };
        for code in [0x8007_0057_u32, 0x8000_4001, 0x8000_4002, 0x8007_0032] {
            let f = Failure::from_capture(&api("MFCreateSourceReaderFromMediaSource", code));
            assert!(matches!(&f, Failure::Fatal(m) if m.contains("MFCreateSourceReaderFromMediaSource") && m.contains(&format!("0x{code:08X}"))), "{f:?}");
        }
        for code in [0x8000_4005_u32, 0xC00D_3E8C, 0x8007_001F] {
            assert!(matches!(
                Failure::from_capture(&api("MFCreateSourceReaderFromMediaSource", code)),
                Failure::Retry(RetryKind::Error, _)
            ));
        }
        // and through the engine: a fatal open stops it with that error, no reconnecting
        let rig = Rig::new("rf-l7.bin", fast());
        rig.fake.failures.lock().unwrap().push_back(Failure::from_capture(&api("MFCreateSourceReaderFromMediaSource", 0x8007_0057)));
        rig.engine.start(settings(bars(30), false));
        let err = rig.engine.wait_live(Duration::from_secs(5)).unwrap_err();
        assert!(err.contains("0x80070057"), "{err}");
        assert!(rig.engine.wait_stopped(Duration::from_secs(5)));
        assert!(rig.states().iter().all(|(s, _)| *s != EngineState::Reconnecting));
    }

    /// L6: a decoder failure at the open is retried once with hardware decoding off, before the format fallback.
    #[test]
    fn a_decoder_failure_is_retried_once_with_hardware_decoding_off() {
        use windows::core::Error;
        let decoder = || {
            Failure::from_capture(&CaptureError::Api {
                what: "first ReadSample",
                error: Error::from(HRESULT(0xC00D_6D60_u32 as i32)),
            })
        };
        assert!(matches!(decoder(), Failure::Retry(RetryKind::Decoder, _)));
        let spec = |hw| CameraSpec {
            link: FAKE_LINK.into(),
            request: FormatRequest { format: PixelFormat::Mjpeg, size: None, fps: None },
            hw_decode: hw,
        };
        // hardware first, software works
        let mut seen = Vec::new();
        let r = open_retrying_decoder(&spec(true), true, |s| {
            seen.push(s.hw_decode);
            if s.hw_decode { Err(decoder()) } else { Ok(7) }
        });
        assert_eq!(r.unwrap(), 7);
        assert_eq!(seen, vec![true, false]);
        // both fail: a format problem, which the fallback to the device default takes up
        let mut calls = 0;
        let r: Result<u8, Failure> = open_retrying_decoder(&spec(true), true, |_| {
            calls += 1;
            Err(decoder())
        });
        assert!(matches!(r, Err(Failure::Format(m)) if m.contains("hardware decoding off")));
        assert_eq!(calls, 2, "once more, not more");
        // software from the start: nothing to retry with, and nothing else to blame
        let mut calls = 0;
        let r: Result<u8, Failure> = open_retrying_decoder(&spec(false), true, |_| {
            calls += 1;
            Err(decoder())
        });
        assert!(matches!(r, Err(Failure::Format(_))));
        assert_eq!(calls, 1);
        // other failures are not the decoder's
        let mut calls = 0;
        let r: Result<u8, Failure> = open_retrying_decoder(&spec(true), true, |_| {
            calls += 1;
            Err(Failure::in_use(None))
        });
        assert!(matches!(r, Err(Failure::Retry(RetryKind::InUse, _))) && calls == 1);
    }

    /// A decoder that fails twice is a format problem on the first open of a Start only: after a reconnect it is a
    /// decoder retry (no device-default fallback, no format stop).
    #[test]
    fn a_decoder_failure_on_a_reconnect_is_retried_not_a_format_problem() {
        use windows::core::Error;
        let decoder = || {
            Failure::from_capture(&CaptureError::Api {
                what: "first ReadSample",
                error: Error::from(HRESULT(0xC00D_6D60_u32 as i32)),
            })
        };
        let spec = |hw| CameraSpec {
            link: FAKE_LINK.into(),
            request: FormatRequest { format: PixelFormat::Mjpeg, size: None, fps: None },
            hw_decode: hw,
        };
        for hw in [true, false] {
            let mut calls = 0;
            let r: Result<u8, Failure> = open_retrying_decoder(&spec(hw), false, |_| {
                calls += 1;
                Err(decoder())
            });
            assert!(matches!(r, Err(Failure::Retry(RetryKind::Decoder, _))), "hw {hw}: {r:?}");
            assert_eq!(calls, if hw { 2 } else { 1 }, "the software retry is still made");
        }
        // through the engine: the first open stops with a format error ...
        let rig = Rig::new("rf-decoder-first.bin", fast());
        rig.fake.capture_errors.lock().unwrap().push_back(("first ReadSample", 0xC00D_6D60));
        rig.engine.start(settings(bars(30), false));
        let err = rig.engine.wait_live(Duration::from_secs(5)).unwrap_err();
        assert!(err.contains("decoder cannot decode"), "{err}");
        assert!(rig.engine.wait_stopped(Duration::from_secs(5)));
        drop(rig);
        // ... a reconnect waits and comes back
        let rig = Rig::new("rf-decoder-again.bin", fast());
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        rig.fake.capture_errors.lock().unwrap().push_back(("first ReadSample", 0xC00D_6D60));
        rig.fake.capture_errors.lock().unwrap().push_back(("first ReadSample", 0xC00D_6D60));
        rig.fake.stream_error_once.store(true, Ordering::SeqCst);
        rig.wait("a failed retry", Duration::from_secs(10), |r| r.fake.capture_errors.lock().unwrap().is_empty());
        rig.wait_state(EngineState::Live, Duration::from_secs(10));
        assert!(rig.engine.last_error().is_none());
        assert!(rig.states().iter().all(|(s, _)| *s != EngineState::Stopped), "{:?}", rig.states());
        let firsts = rig.fake.first_opens.lock().unwrap().clone();
        assert!(firsts.len() >= 4 && firsts[0] && firsts[1..].iter().all(|f| !f), "{firsts:?}");
    }

    /// A deterministic error code stops the engine at once on the first open of a Start; while a camera that worked is
    /// waited for it is an error like others (retried for `error_cap`).
    #[test]
    fn a_deterministic_error_code_during_a_reconnect_does_not_stop_the_engine() {
        use windows::core::Error;
        let api = |code: u32| CaptureError::Api { what: "MFCreateSourceReaderFromMediaSource", error: Error::from(HRESULT(code as i32)) };
        for code in [0x8007_0057_u32, 0x8000_4001, 0x8000_4002, 0x8007_0032, 0xC00D_3E98, 0xC00D_36B4] {
            assert!(matches!(Failure::from_capture_at(&api(code), true), Failure::Fatal(_)), "{code:#x}");
            assert!(matches!(Failure::from_capture_at(&api(code), false), Failure::Retry(RetryKind::Error, _)), "{code:#x}");
        }
        let rig = Rig::new("rf-deterministic-again.bin", fast());
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        rig.fake.capture_errors.lock().unwrap().push_back(("MFCreateSourceReaderFromMediaSource", 0x8007_0057));
        rig.fake.capture_errors.lock().unwrap().push_back(("MFCreateSourceReaderFromMediaSource", 0x8007_0057));
        rig.fake.stream_error_once.store(true, Ordering::SeqCst);
        rig.wait("a failed retry", Duration::from_secs(10), |r| r.fake.capture_errors.lock().unwrap().is_empty());
        rig.wait_state(EngineState::Live, Duration::from_secs(10));
        assert!(rig.engine.last_error().is_none());
        assert!(rig.states().iter().all(|(s, _)| *s != EngineState::Stopped), "{:?}", rig.states());
    }

    /// L4: only a refusal ("access denied") of the open is Windows' privacy setting, and only while a switch is off.
    /// A value that says `Deny` that Windows does not enforce never relabels anything else.
    #[test]
    fn a_privacy_switch_only_explains_an_access_denied() {
        let rig = Rig::new("rf-l4.bin", fast());
        rig.fake.registry_deny.store(true, Ordering::SeqCst);
        let privacy = |f: Failure| outcome_with_privacy(Err(f), &*rig.fake);
        for other in [
            Failure::Retry(RetryKind::Error, "the camera stream failed (0xC00D3E8C)".into()),
            Failure::Retry(RetryKind::NoFrames, "no picture".into()),
            Failure::Retry(RetryKind::InUse, "camera in use by another app (0xC00D3704)".into()),
            Failure::Retry(RetryKind::Disconnected, "camera disconnected".into()),
            Failure::Format("no such format".into()),
            Failure::Fatal("boom".into()),
        ] {
            assert_eq!(privacy(other.clone()), Err(other));
        }
        let denied = Failure::in_use(Some(windows::Win32::Foundation::E_ACCESSDENIED));
        assert!(matches!(&denied, Failure::Retry(RetryKind::AccessDenied, _)));
        assert!(matches!(privacy(denied.clone()), Err(Failure::Retry(RetryKind::Privacy, r)) if r.contains("privacy")));
        // no switch is off: it stays what it looks like, a camera that is in use
        rig.fake.registry_deny.store(false, Ordering::SeqCst);
        assert_eq!(privacy(denied.clone()), Err(denied));
    }

    /// L4 through the engine: the registry says `Deny`, Windows lets the app in anyway: a stream error stays a
    /// stream error, and an open that is refused is the privacy wait.
    #[test]
    fn a_deny_value_that_windows_does_not_enforce_does_not_make_a_privacy_wait() {
        let rig = Rig::new("rf-l4-engine.bin", fast());
        rig.fake.registry_deny.store(true, Ordering::SeqCst);
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        rig.fake.stream_error.store(true, Ordering::SeqCst);
        rig.wait_state(EngineState::Reconnecting, Duration::from_secs(5));
        assert_eq!(last_kind(&rig), Some("error"), "{:?}", rig.engine.reason());
        assert!(!rig.engine.reason().unwrap().contains("privacy"));
        rig.fake.stream_error.store(false, Ordering::SeqCst);
        rig.wait_state(EngineState::Live, Duration::from_secs(5));
        // an open that is refused is what the switch explains
        rig.fake.privacy.store(true, Ordering::SeqCst);
        rig.fake.stream_error.store(true, Ordering::SeqCst);
        rig.wait("the privacy wait", Duration::from_secs(5), |r| last_kind(r) == Some("privacy"));
    }

    /// L10: the file holds what the last writer of the advertised list wrote, whoever that is.
    #[test]
    fn the_advertised_list_is_never_left_stale_by_the_other_writer() {
        let rig = Rig::new("rf-l10.bin", fast());
        let (shared, writer) = (&rig.engine.shared, rig.engine.writer());
        let first = |r: &Rig| {
            let h = r.file.shared().header();
            (h.advertised[0].width, h.advertised[0].height)
        };
        advertise_session(shared, writer, (1280, 720, 30, false));
        assert_eq!(first(&rig), (1280, 720));
        // the control thread writes the configured output while no camera delivers
        *lock(&shared.output) = output_of(640);
        advertise_configured(shared, writer);
        assert_eq!(first(&rig), (640, 360));
        // the render thread asks for what it advertised last (1280x720): the file holds something else, so it is
        // written again (comparing with its own last value would leave the control thread's list)
        advertise_session(shared, writer, (1280, 720, 30, false));
        assert_eq!(first(&rig), (1280, 720));
        // and the control thread works from the latest list, not from an older one
        *lock(&shared.output) = OutputSettings { fps: Some(25), ..OutputSettings::default() };
        advertise_configured(shared, writer);
        assert_eq!(first(&rig), (1280, 720), "no explicit size: what the session advertised");
        assert_eq!(*lock(&shared.advertised), Some((1280, 720, 25, true)));

        // two threads at it: the file and the record always agree in the end
        std::thread::scope(|s| {
            s.spawn(|| {
                for i in 0..300u32 {
                    advertise_session(shared, writer, (1920 - (i % 2) * 640, 1080, 30, true));
                }
            });
            s.spawn(|| {
                *lock(&shared.output) = output_of(640);
                for _ in 0..300 {
                    advertise_configured(shared, writer);
                }
            });
        });
        let record = lock(&shared.advertised).unwrap();
        assert_eq!(first(&rig).0, record.0, "the file holds what was recorded");
    }

    #[test]
    fn state_events_serialize_as_plan_5_6() {
        let live = serde_json::to_value(StateEvent::new(EngineState::Live, None, None)).unwrap();
        assert_eq!(live, serde_json::json!({"state": "live"}));
        let r = serde_json::to_value(StateEvent::new(
            EngineState::Reconnecting,
            Some("camera is in use by another app".into()),
            Some(RetryKind::InUse.name()),
        ))
        .unwrap();
        assert_eq!(
            r,
            serde_json::json!({"state": "reconnecting", "reason": "camera is in use by another app", "kind": "in_use"})
        );
        // with the programs: the list, and the flag only when it is set
        let named = StateEvent {
            apps: Some(vec!["Discord.exe".into(), "chrome.exe".into()]),
            ..StateEvent::new(
                EngineState::Reconnecting,
                Some("camera is in use by Discord.exe and chrome.exe".into()),
                Some(RetryKind::InUse.name()),
            )
        };
        assert_eq!(
            serde_json::to_value(&named).unwrap(),
            serde_json::json!({
                "state": "reconnecting",
                "reason": "camera is in use by Discord.exe and chrome.exe",
                "kind": "in_use",
                "apps": ["Discord.exe", "chrome.exe"]
            })
        );
        let maybe = StateEvent { apps_maybe: true, ..named };
        assert_eq!(serde_json::to_value(&maybe).unwrap()["apps_maybe"], serde_json::json!(true));
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

    // ---- the meter (docs/AUTO-CONTROLS.md 2.4, 5.5) -------------------------------------------------------------

    use crate::meter::MeterSample;

    /// What the tap of a meter test saw, on the render thread: every frame's capture time in order, the frame (by
    /// index) at which the meter first looked switched on, and every sample it took from the mailbox with the index of
    /// the frame whose pass found it and the time.
    #[derive(Default)]
    struct MeterLog {
        frames: Vec<u64>,
        /// When the tap saw each of `frames`.
        times: Vec<Instant>,
        first_on: Option<usize>,
        samples: Vec<(usize, Instant, MeterSample)>,
    }

    /// The tap is the mailbox's reader (it runs on the render thread right after the meter's pass of a frame), so a
    /// sample is seen at the frame that delivered it, with nothing racing.
    struct MeterTap {
        shared: Arc<Shared>,
        log: Arc<Mutex<MeterLog>>,
    }

    impl FrameTap for MeterTap {
        fn on_frame(&mut self, _: &Renderer, _: &RgbaTexture, capture_qpc: u64) {
            let mut log = self.log.lock().unwrap();
            let idx = log.frames.len();
            log.frames.push(capture_qpc);
            log.times.push(Instant::now());
            if log.first_on.is_none() && self.shared.metering.load(Ordering::SeqCst) {
                log.first_on = Some(idx);
            }
            if let Some(s) = self.shared.meter.take() {
                log.samples.push((idx, Instant::now(), s));
            }
        }
    }

    /// A rig whose camera shows one flat grey (8-bit coded `level`, 320x180 at `fps`), running, with a [`MeterTap`].
    fn grey_rig(name: &str, level: u8, fps: u32) -> (Rig, Arc<Mutex<MeterLog>>) {
        let rig = Rig::new(name, fast());
        *rig.fake.flat.lock().unwrap() = Some(level);
        let log = Arc::new(Mutex::new(MeterLog::default()));
        rig.engine.send(Msg::SetTap(Some(Box::new(MeterTap {
            shared: rig.engine.shared.clone(),
            log: log.clone(),
        }))));
        rig.engine.start(settings(bars(fps), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        (rig, log)
    }

    fn wait_log(rig: &Rig, log: &Arc<Mutex<MeterLog>>, what: &str, cond: impl Fn(&MeterLog) -> bool) {
        rig.wait(what, Duration::from_secs(10), |_| cond(&log.lock().unwrap()));
    }

    fn frames_seen(log: &Arc<Mutex<MeterLog>>) -> usize {
        log.lock().unwrap().frames.len()
    }

    /// The linear light of an 8-bit sRGB-coded grey.
    fn linear_of(level: u8) -> f32 {
        let c = f32::from(level) / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }

    /// The mean linear colour of a sample, weighted by coverage.
    fn mean_of(s: &MeterSample) -> [f32; 3] {
        let mut sum = [0.0f32; 3];
        let mut weight = 0.0f32;
        for c in &s.stats.cells {
            for k in 0..3 {
                sum[k] += c[k] * c[3];
            }
            weight += c[3];
        }
        assert!(weight > 0.0, "no coverage");
        sum.map(|v| v / weight)
    }

    fn assert_grey(s: &MeterSample, level: u8) {
        assert_eq!(s.stats.cells.len(), 256);
        let want = linear_of(level);
        for (k, v) in mean_of(s).into_iter().enumerate() {
            assert!(
                (v - want).abs() <= 0.01,
                "channel {k}: weighted linear mean {v}, the picture is {want} ({level}/255)"
            );
        }
    }

    #[test]
    fn a_sample_comes_within_three_frames_of_metering_on_and_measures_the_known_grey() {
        let (rig, log) = grey_rig("meter-known.bin", 128, 60);
        wait_log(&rig, &log, "30 frames", |l| l.frames.len() >= 30);
        assert!(!rig.engine.metering());
        // no sample without metering
        std::thread::sleep(Duration::from_millis(400));
        assert!(log.lock().unwrap().samples.is_empty(), "samples while metering is off");
        assert!(rig.engine.meter().take().is_none());

        rig.engine.set_metering(true);
        wait_log(&rig, &log, "a sample", |l| !l.samples.is_empty());
        let l = log.lock().unwrap();
        let (arrived, _, s) = &l.samples[0];
        let on = l.first_on.expect("the tap saw metering on");
        assert!(
            *arrived - on <= 3,
            "the sample was found by frame {arrived}, metering was on at frame {on}"
        );
        assert_grey(s, 128);
        // `capture_qpc` is the capture time of a frame made after the switch and before the one that delivered it
        let at = l.frames.iter().position(|q| *q == s.capture_qpc).expect("a frame's capture time");
        assert!(at >= on.saturating_sub(1) && at < *arrived, "frame {at}, on at {on}, delivered at {arrived}");
        // the token is the camera session's (the hub's), the same one the facts name
        let facts = rig.engine.session_facts();
        assert!(facts.token.is_some());
        assert_eq!(Some(s.token), facts.token);
    }

    #[test]
    fn at_60_fps_the_meter_makes_at_most_16_samples_a_second() {
        let (rig, log) = grey_rig("meter-rate.bin", 90, 60);
        rig.engine.set_metering(true);
        wait_log(&rig, &log, "samples", |l| l.samples.len() >= 3);
        let from = Instant::now();
        let (before, frames_before) = {
            let l = log.lock().unwrap();
            (l.samples.len(), l.frames.len())
        };
        std::thread::sleep(Duration::from_secs(4));
        let l = log.lock().unwrap();
        let n = l.samples.iter().filter(|(_, t, _)| *t >= from).count();
        let frames = l.frames.len() - frames_before;
        assert!(l.samples.len() >= before);
        // 15 a second when frames come at 60: no more than 16 a second ...
        assert!(n <= 16 * 4, "{n} samples in 4 s");
        // ... and, on a machine that is busy with other work (a whole test run in parallel), as many as the frames the
        // render thread got to allow: a sample is taken from the first frame that comes 66 ms after the last sample's
        // (every frame when they come slower than that), so starving the thread lowers the frames and with them the
        // samples, not the ratio. The rule is played over the wall times of the frames of the window, which is what
        // a meter that samples at the rate it should makes; three quarters of that allows for the frames whose
        // spacing is borderline (the tap sees a frame a little after the meter took its time), and for the samples that
        // are still on their way when the window ends. A meter that samples at half the rate gets half.
        let mut last: Option<Instant> = None;
        let mut allowed = 0usize;
        for t in l.times.iter().filter(|t| **t >= from) {
            if last.is_none_or(|p| t.duration_since(p) >= crate::meter::MIN_GAP) {
                allowed += 1;
                last = Some(*t);
            }
        }
        assert!(
            n * 4 >= allowed * 3,
            "{n} samples in 4 s from {frames} frames, the 66 ms rule allows {allowed}"
        );
        assert!(n >= 3, "{n} samples in 4 s");
        // each sample is of a different frame, in order
        let q: Vec<u64> = l.samples.iter().map(|(_, _, s)| s.capture_qpc).collect();
        assert!(q.windows(2).all(|w| w[1] > w[0]), "{q:?}");
        // and they are 66 ms of wall time apart at the most often: the first and the last sample's frames are at least
        // (n - 1) of those apart. The wall clock is the meter's (the frames' capture stamps are planned times, which a
        // thread that wakes late leaves behind the clock by a different amount for each frame), and the total, not
        // each pair, because the time from the meter's pass to the tap varies with the load by as much as tens of ms
        // for one frame and cancels over many.
        let wall: Vec<Instant> = q
            .iter()
            .map(|c| l.times[l.frames.iter().position(|f| f == c).expect("the sample's frame")])
            .collect();
        let span = *wall.last().unwrap() - wall[0];
        let least = crate::meter::MIN_GAP * (wall.len() as u32 - 1);
        assert!(
            span + Duration::from_millis(250) >= least,
            "{} samples came over {span:?}, 66 ms apart at the least is {least:?}",
            wall.len()
        );
    }

    #[test]
    fn switching_metering_off_stops_the_samples_and_on_again_resumes_them() {
        let (rig, log) = grey_rig("meter-onoff.bin", 160, 60);
        rig.engine.set_metering(true);
        wait_log(&rig, &log, "samples", |l| l.samples.len() >= 3);
        rig.engine.set_metering(false);
        // the render thread notices on its next frame and drops what was in flight
        let seen = frames_seen(&log);
        wait_log(&rig, &log, "5 frames", |l| l.frames.len() >= seen + 5);
        let n = log.lock().unwrap().samples.len();
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(log.lock().unwrap().samples.len(), n, "samples with metering off");
        assert!(rig.engine.meter().take().is_none());
        rig.engine.set_metering(true);
        wait_log(&rig, &log, "samples again", |l| l.samples.len() >= n + 2);
        assert_grey(&log.lock().unwrap().samples.last().unwrap().2, 160);
    }

    #[test]
    fn the_meter_sees_the_picture_before_the_filter_chain() {
        let (rig, log) = grey_rig("meter-prechain.bin", 100, 60);
        let _consumer = rig.consumer_with("meter-prechain.bin", Format::new(320, 180, 60, SUBTYPE_NV12));
        // a chain that makes the picture far brighter than the camera's
        rig.engine.set_chain(
            crate::fx::parse_chain(&serde_json::json!([
                { "id": "c", "kind": "color_filter", "enabled": true, "params": { "brightness": 0.6 } }
            ]))
            .unwrap(),
        );
        rig.engine.set_metering(true);
        wait_log(&rig, &log, "samples", |l| l.samples.len() >= 3);
        let out = |rig: &Rig| -> Option<u8> {
            let (w, h, _, buf) = latest(&rig.file)?;
            let rgb = nv12_to_rgb(&buf, w, h, w as usize, ColorSpace::Bt709, ColorRange::Limited);
            Some(rgb[(w as usize * (h as usize / 2) + w as usize / 2) * 3])
        };
        rig.wait("the chain's picture", Duration::from_secs(10), |r| {
            out(r).is_some_and(|v| v >= 140)
        });
        for (_, _, s) in &log.lock().unwrap().samples {
            assert_grey(s, 100);
        }
    }

    #[test]
    fn the_facts_follow_the_session_and_a_resume_moves_the_resume_time() {
        let (rig, log) = grey_rig("meter-facts.bin", 128, 60);
        assert_eq!(rig.engine.session_facts().target_fps, 60.0);
        rig.wait("the capture rate", Duration::from_secs(10), |r| {
            let f = r.engine.session_facts();
            f.capture_fps > 40.0 && f.capture_fps < 75.0
        });
        let f = rig.engine.session_facts();
        assert!(f.token.is_some() && !f.paused && f.resumed_qpc == 0, "{f:?}");

        // paused: the facts say so, nothing is measured, and what was in flight does not come after the resume
        rig.engine.set_metering(true);
        wait_log(&rig, &log, "samples", |l| l.samples.len() >= 2);
        rig.engine.set_pause(true);
        rig.wait("paused", Duration::from_secs(5), |r| r.engine.session_facts().paused);
        let n = log.lock().unwrap().samples.len();
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(log.lock().unwrap().samples.len(), n, "samples while paused");
        let resumed = vcam_protocol::qpc_now();
        rig.engine.set_pause(false);
        rig.wait("resumed", Duration::from_secs(5), |r| !r.engine.session_facts().paused);
        let f = rig.engine.session_facts();
        assert!(f.resumed_qpc >= resumed, "{} < {resumed}", f.resumed_qpc);
        wait_log(&rig, &log, "samples after the resume", |l| l.samples.len() > n);
        // every sample after the resume was captured after it
        for (_, _, s) in log.lock().unwrap().samples.iter().skip(n) {
            assert!(s.capture_qpc >= f.resumed_qpc, "{} < {}", s.capture_qpc, f.resumed_qpc);
        }

        // a stopped engine has no session
        rig.engine.stop();
        assert!(rig.engine.wait_stopped(Duration::from_secs(10)));
        assert_eq!(rig.engine.session_facts(), crate::meter::SessionFacts::CLOSED);
    }

    /// The PERF.md method on the hardware adapter: 1080p60 synthetic NV12, an app reading the camera, no filters, the
    /// engine's own mean GPU time per frame (`gpu_ms`, one mean per second) with the meter off and on in alternating
    /// 20 s stretches (the window that straddles a switch is dropped). Run by hand under the `app-test.lock`:
    /// `cargo test -p better-cam --lib meter_gpu_cost -- --ignored --nocapture`.
    #[test]
    #[ignore = "a timing run on the hardware adapter"]
    fn meter_gpu_cost_at_1080p60() {
        let gpu = Arc::new(GpuDevice::new(AdapterKind::Hardware).expect("hardware adapter"));
        let path = temp_file("meter-perf.bin");
        let _ = std::fs::remove_file(&path);
        let writer = Arc::new(Writer::create(&path).unwrap());
        let engine = Engine::with_parts(gpu, writer).unwrap();
        let _consumer = Consumer::start_with(&path, Format::new(1920, 1080, 60, SUBTYPE_NV12));
        let spec = SourceSpec::Synthetic(SyntheticSpec {
            width: 1920,
            height: 1080,
            fps: 60,
            format: PixelFormat::Nv12,
            color: ColorSettings::default(),
        });
        engine.start(settings(spec, false));
        engine.wait_live(Duration::from_secs(20)).unwrap();
        std::thread::sleep(Duration::from_secs(8));
        let mut runs: [Vec<f64>; 2] = [Vec::new(), Vec::new()];
        for round in 0..3 {
            for on in [false, true] {
                engine.set_metering(on);
                let started = Instant::now();
                let mut last = engine.stats().rendered;
                let mut skipped = false;
                while started.elapsed() < Duration::from_secs(21) {
                    std::thread::sleep(Duration::from_millis(50));
                    let s = engine.stats();
                    if s.rendered != last {
                        last = s.rendered;
                        if std::mem::replace(&mut skipped, true) {
                            runs[usize::from(on)].push(s.gpu_ms);
                        }
                    }
                }
                let v = &runs[usize::from(on)];
                eprintln!(
                    "round {round}, metering {on}: {} windows, last gpu {:.4} ms, fps {:.1}",
                    v.len(),
                    v.last().copied().unwrap_or(0.0),
                    engine.stats().render_fps
                );
            }
        }
        let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
        let median = |v: &[f64]| {
            let mut s = v.to_vec();
            s.sort_by(|a, b| a.total_cmp(b));
            s[s.len() / 2]
        };
        let (off, on) = (mean(&runs[0]), mean(&runs[1]));
        eprintln!(
            "meter off: mean {off:.4} ms, median {:.4} ms over {} windows; meter on: mean {on:.4} ms, median {:.4} ms over {} windows; difference of the means {:+.4} ms, of the medians {:+.4} ms; a sample waits in the mailbox: {}",
            median(&runs[0]),
            runs[0].len(),
            median(&runs[1]),
            runs[1].len(),
            on - off,
            median(&runs[1]) - median(&runs[0]),
            engine.meter().take().is_some()
        );
    }

    #[test]
    fn samples_resume_after_a_device_loss_with_the_new_session() {
        let (rig, log) = grey_rig("meter-loss.bin", 128, 60);
        rig.engine.set_metering(true);
        wait_log(&rig, &log, "samples", |l| l.samples.len() >= 3);
        let old = rig.engine.session_facts().token.expect("a session");
        let n = log.lock().unwrap().samples.len();
        rig.engine.inject_device_loss(1);
        rig.wait("a new device", Duration::from_secs(10), |r| r.engine.gpu_generation() == 2);
        rig.wait("live again", Duration::from_secs(10), |r| {
            r.engine.state() == EngineState::Live && r.engine.session_facts().token.is_some_and(|t| t != old)
        });
        // metering is still on (nobody touched it) and the new session measures with the new device
        assert!(rig.engine.metering());
        let new = rig.engine.session_facts().token.unwrap();
        wait_log(&rig, &log, "samples from the new session", |l| {
            l.samples.iter().skip(n).any(|(_, _, s)| s.token == new)
        });
        let l = log.lock().unwrap();
        let s = &l.samples.iter().skip(n).find(|(_, _, s)| s.token == new).unwrap().2;
        assert_grey(s, 128);
        assert_ne!(s.token, old);
    }

    // ---- Better Cam auto end to end (docs/AUTO-CONTROLS.md 2.8, phase A4) --------------------------------------------

    use crate::auto_controls::runner::AutoStatusEvent;
    use crate::auto_controls::{ExposureState, Mode as ControlMode};
    use crate::capture::controls::ControlKey;
    use std::sync::atomic::AtomicI32;

    /// The camera's picture follows its controls: a flat grey that is the 18 % target when exposure plus the gain's
    /// stops (3 stops over the whole range) reach `need` (thousandths of a stop).
    fn live_level(cam: Arc<FakeControls>, need: Arc<AtomicI32>) -> LiveLevel {
        Arc::new(move || {
            let e = f64::from(cam.get(ControlKey::Exposure).value);
            let g = f64::from(cam.get(ControlKey::Gain).value);
            let x = e + 3.0 * g / 100.0 - f64::from(need.load(Ordering::SeqCst)) / 1000.0;
            let lin = (0.1706 * 2f64.powf(x)).clamp(0.0, 1.0);
            let coded = if lin <= 0.003_130_8 {
                12.92 * lin
            } else {
                1.055 * lin.powf(1.0 / 2.4) - 0.055
            };
            (coded * 255.0).round() as u8
        })
    }

    fn auto_record(exposure: i32, gain: i32) -> crate::capture::controls::ControlSnapshot {
        crate::camera_controls::parse_record(Some(&serde_json::json!({
            "id": FAKE_LINK,
            "name": "W4GS",
            "controls": {
                "exposure": { "value": exposure, "auto": false, "mode": "app" },
                "gain": { "value": gain, "mode": "app" }
            }
        })))
        .unwrap()
        .unwrap()
    }

    fn auto_statuses(rig: &Rig) -> Vec<AutoStatusEvent> {
        rig.events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|(_, e)| match e {
                EngineEvent::AutoStatus(s) => Some(s.clone()),
                _ => None,
            })
            .collect()
    }

    fn exposure_state(s: &AutoStatusEvent) -> Option<ExposureState> {
        s.status.exposure.as_ref().map(|e| e.state)
    }

    /// Wait for an `auto-status` after the first `after` ones whose exposure state is `state`.
    fn wait_exposure_state(rig: &Rig, after: usize, state: ExposureState, secs: u64) -> Duration {
        rig.wait(&format!("auto-status {state:?}"), Duration::from_secs(secs), |r| {
            auto_statuses(r).iter().skip(after).any(|s| exposure_state(s) == Some(state))
        })
    }

    #[test]
    fn better_cam_auto_holds_a_live_camera_and_obeys_pause_reopen_and_the_user() {
        let rig = Rig::new("auto-e2e.bin", fast());
        let cam = rig.fake.controls.clone();
        let hub = rig.engine.controls().clone();
        let need = Arc::new(AtomicI32::new(-3500));
        *rig.fake.live.lock().unwrap() = Some(live_level(cam.clone(), need.clone()));
        // exposure and gain in Better Cam auto, resumed at a dark point: the loop must find -5 and about 1.5 stops
        hub.set_record(Some(auto_record(-6, 0)));
        rig.engine.set_window_visible(true);
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();

        // the stored resume point is written before the loop's first write
        rig.wait("the stored values", Duration::from_secs(5), |_| cam.log().len() >= 3);
        assert_eq!(
            cam.log()[..3],
            ["auto:exposure:false", "value:exposure:-6", "value:gain:0"],
            "{:?}",
            cam.log()
        );
        // 1. engaged: holding within 5 s, the meter is on, the picture is where the target wants it
        let took = wait_exposure_state(&rig, 0, ExposureState::Holding, 5);
        eprintln!("[test] holding after {took:?}");
        assert!(rig.engine.metering(), "the loop measures while it runs");
        let s = auto_statuses(&rig).into_iter().rev().find_map(|s| s.status.exposure).unwrap();
        assert_eq!(s.cap, -5, "30 fps: the longest exposure that fits the frame");
        let (e, g) = (cam.get(ControlKey::Exposure).value, cam.get(ControlKey::Gain).value);
        assert!(e <= -5, "exposure {e} is longer than the frame");
        let x = f64::from(e) + 3.0 * f64::from(g) / 100.0;
        assert!((x + 3.5).abs() <= 0.5, "exposure {e} gain {g}: {x} stops, wanted -3.5");
        let ev = auto_statuses(&rig).pop().unwrap();
        assert_eq!(ev.id, FAKE_LINK);
        // the window hears of the controls and who drives them
        rig.wait("controls-changed with the mode", Duration::from_secs(5), |r| {
            r.events.lock().unwrap().iter().any(|(_, e)| {
                matches!(e, EngineEvent::Controls(c)
                    if c.iter().any(|i| i.key == ControlKey::Exposure && i.mode == ControlMode::App))
            })
        });

        // 2. pause: no writes while the camera is paused, whatever the light does
        rig.engine.set_pause(true);
        rig.wait("the pause in the facts", Duration::from_secs(5), |r| r.engine.session_facts().paused);
        std::thread::sleep(Duration::from_millis(300));
        cam.clear_log();
        need.store(-1000, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(1500));
        assert!(cam.log().is_empty(), "writes while paused: {:?}", cam.log());
        rig.engine.set_pause(false);
        rig.wait("writes after the resume", Duration::from_secs(10), |_| !cam.log().is_empty());
        need.store(-3500, Ordering::SeqCst);
        let seen = auto_statuses(&rig).len();
        rig.wait("the picture back at the target", Duration::from_secs(10), |_| {
            let x = f64::from(cam.get(ControlKey::Exposure).value)
                + 3.0 * f64::from(cam.get(ControlKey::Gain).value) / 100.0;
            (x + 3.5).abs() <= 0.5
        });
        let _ = seen;

        // 3. Stop and Start: the camera forgets its controls; the stored resume point is written first
        rig.engine.stop();
        assert!(rig.engine.wait_stopped(Duration::from_secs(10)));
        let rec = hub.record().unwrap();
        let (re, rg) = (
            rec.get(ControlKey::Exposure).unwrap().value.unwrap(),
            rec.get(ControlKey::Gain).unwrap().value.unwrap(),
        );
        let before = auto_statuses(&rig).len();
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        rig.wait("the stored values after the reopen", Duration::from_secs(5), |_| cam.log().len() >= 3);
        assert_eq!(
            cam.log()[..3],
            [
                "auto:exposure:false".to_string(),
                format!("value:exposure:{re}"),
                format!("value:gain:{rg}")
            ],
            "{:?}",
            cam.log()
        );
        wait_exposure_state(&rig, before, ExposureState::Holding, 8);

        // 4. the user picks Manual in the middle of it: the loop's next write finds the mode changed
        need.store(-2000, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(300));
        let rows = hub.set_mode(FAKE_LINK, ControlKey::Exposure, ControlMode::Manual).unwrap();
        assert!(rows.iter().all(|r| r.mode == ControlMode::Manual));
        let (e, g) = (cam.get(ControlKey::Exposure).value, cam.get(ControlKey::Gain).value);
        let token = rig.engine.session_facts().token.expect("a camera session");
        assert!(matches!(
            hub.drive(token, ControlKey::Exposure, -3),
            Err(crate::camera_controls::DriveError::NotApp)
        ));
        let after = auto_statuses(&rig).len();
        cam.clear_log();
        need.store(-3500, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(1500));
        assert!(cam.log().is_empty(), "writes after the user's Manual: {:?}", cam.log());
        assert_eq!((cam.get(ControlKey::Exposure).value, cam.get(ControlKey::Gain).value), (e, g));
        let rec = hub.record().unwrap();
        assert!(!rec.is_app(ControlKey::Exposure) && !rec.is_app(ControlKey::Gain));
        assert_eq!(rec.get(ControlKey::Exposure).unwrap().value, Some(e), "the last driven value is the manual one");
        // the loop ended: the status has no exposure part any more, and the meter is off
        rig.wait("the end of the loop", Duration::from_secs(5), |r| {
            auto_statuses(r).iter().skip(after).any(|s| s.status.exposure.is_none())
        });
        rig.wait("the meter off", Duration::from_secs(5), |r| !r.engine.metering());
    }

    // ---- robustness review: M1 (privacy), M2 (stop), M3 (late publishes), M4 (slate), L4 (frame file) -----------

    /// Capture time of the frame in the file's newest slot; `None` when the file holds no newest frame.
    fn newest_capture(map: &MappedFile) -> Option<u64> {
        let h = map.shared().header();
        (h.latest_slot != vcam_protocol::NO_SLOT)
            .then(|| map.shared().slot_meta(h.latest_slot as usize).capture_qpc)
    }

    fn heartbeat_age_ms(r: &Rig) -> i64 {
        let h = r.file.shared().header();
        vcam_protocol::qpc_to_hns(vcam_protocol::qpc_now().saturating_sub(h.writer_heartbeat)) / 10_000
    }

    /// M1: with no app reading, the file's newest frame is the last one of the app that left. The first frame a new app
    /// can take must be a new one, so it is forgotten when the app leaves.
    #[test]
    fn the_first_frame_an_attaching_app_can_take_is_a_new_one() {
        let rig = Rig::new("m1-new-frame.bin", fast());
        let mut consumer = rig.consumer("m1-new-frame.bin");
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        rig.wait("frames", Duration::from_secs(10), |r| frames_in_file(r) >= 5);
        let old = newest_capture(&rig.file).expect("a frame in the file");

        consumer.detach();
        rig.wait("the file forgets its newest frame", Duration::from_secs(5), |r| {
            newest_capture(&r.file).is_none()
        });
        assert!(latest(&rig.file).is_none(), "an attaching reader would be offered a frame");
        // the camera is open and live all the while, and nothing comes back into the file
        assert_eq!(rig.engine.state(), EngineState::Live);
        assert_eq!(rig.writer_state(), LIVE);
        std::thread::sleep(Duration::from_millis(500));
        assert!(newest_capture(&rig.file).is_none());

        let _again = rig.consumer("m1-new-frame.bin");
        rig.wait("a frame for the new app", Duration::from_secs(5), |r| newest_capture(&r.file).is_some());
        let first = newest_capture(&rig.file).unwrap();
        assert!(first > old, "the first frame is the one of the earlier app ({first} <= {old})");
    }

    /// M1: a session that starts with a frame of an earlier one in the file forgets it before it goes live.
    #[test]
    fn a_frame_left_by_an_earlier_session_is_forgotten_before_the_writer_goes_live() {
        let rig = Rig::new("m1-stale.bin", fast());
        let format = vcam_protocol::SlotFormat { width: 320, height: 180, stride: 320, subtype: SUBTYPE_NV12 };
        rig.engine
            .writer()
            .publish(format, vcam_protocol::qpc_now(), |d| d.fill(7))
            .unwrap();
        assert!(newest_capture(&rig.file).is_some());
        rig.engine.start(settings(bars(30), false));
        rig.wait("the writer is live", Duration::from_secs(20), |r| r.writer_state() == LIVE);
        assert!(
            newest_capture(&rig.file).is_none(),
            "the writer went live with the earlier session's frame in the file"
        );
        assert!(latest(&rig.file).is_none());
    }

    /// M2: ending a session waits for a render thread that is stuck in a slow open, and the heartbeat of the file does
    /// not stop meanwhile (the virtual camera shows "not running" after a second without one).
    #[test]
    fn a_stop_during_a_slow_open_keeps_the_heartbeat_fresh() {
        let rig = Rig::new("m2-heartbeat.bin", fast());
        // an open that cannot be interrupted, longer than the second after which the virtual camera gives up
        *rig.fake.open_delay.lock().unwrap() = Duration::from_millis(2500);
        rig.engine.start(settings(bars(30), false));
        rig.wait_state(EngineState::Standby, Duration::from_secs(5));
        rig.wait("the open has begun", Duration::from_secs(5), |r| r.opens() == 1);
        let t0 = Instant::now();
        rig.engine.stop();
        let mut worst = 0;
        while rig.engine.state() != EngineState::Stopped {
            worst = worst.max(heartbeat_age_ms(&rig));
            assert!(t0.elapsed() < Duration::from_secs(10), "the stop did not return");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(t0.elapsed() > Duration::from_millis(1500), "the stop did not wait for the open: {:?}", t0.elapsed());
        eprintln!("oldest heartbeat during a stop of {:?}: {worst} ms", t0.elapsed());
        assert!(worst < 500, "the heartbeat was {worst} ms old during the stop");
        assert_eq!(rig.alive(), 0);
    }

    /// M2: the stop flag reaches the factory and is looked at between the steps of an open (the decoder retry, the
    /// format fallback): a stop does not wait for the steps that have not begun.
    #[test]
    fn a_stop_during_an_open_of_several_steps_ends_it_before_the_next_step() {
        let rig = Rig::new("m2-steps.bin", fast());
        *rig.fake.open_steps.lock().unwrap() = (8, Duration::from_millis(500));
        rig.engine.start(settings(bars(30), false));
        rig.wait_state(EngineState::Standby, Duration::from_secs(5));
        rig.wait("a step has begun", Duration::from_secs(5), |r| r.fake.steps_taken.load(Ordering::SeqCst) >= 1);
        let t0 = Instant::now();
        rig.engine.stop();
        assert!(rig.engine.wait_stopped(Duration::from_secs(10)), "{:?}", rig.states());
        // one step was under way (up to 500 ms); eight steps are 4 s
        assert!(t0.elapsed() < Duration::from_millis(1600), "the stop took {:?}", t0.elapsed());
        let taken = rig.fake.steps_taken.load(Ordering::SeqCst);
        assert!(taken <= 2, "{taken} steps were begun after the stop");
        assert_eq!(rig.alive(), 0);
    }

    /// M2: a render thread that does not end for [`SESSION_END_CAP`] is left behind; the engine is stopped, the heartbeat
    /// went on, and the thread's late end changes nothing.
    #[test]
    fn a_render_thread_that_does_not_end_in_time_is_left_behind() {
        let rig = Rig::new("m2-cap.bin", fast());
        *rig.engine.shared.session_end_cap.lock().unwrap() = Some(Duration::from_millis(400));
        *rig.fake.open_delay.lock().unwrap() = Duration::from_millis(2500);
        rig.engine.start(settings(bars(30), false));
        rig.wait_state(EngineState::Standby, Duration::from_secs(5));
        rig.wait("the open has begun", Duration::from_secs(5), |r| r.opens() == 1);
        let t0 = Instant::now();
        rig.engine.stop();
        assert!(rig.engine.wait_stopped(Duration::from_secs(10)));
        assert!(t0.elapsed() < Duration::from_millis(1500), "the stop took {:?}", t0.elapsed());
        // the abandoned open finishes, sees the stop flag and closes its camera; the engine stays stopped
        std::thread::sleep(Duration::from_millis(3000).saturating_sub(t0.elapsed()));
        assert_eq!(rig.alive(), 0, "the abandoned open kept its camera");
        assert_eq!(rig.engine.state(), EngineState::Stopped);
        assert!(rig.engine.last_error().is_none());
        // and a new start works
        *rig.fake.open_delay.lock().unwrap() = Duration::ZERO;
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
    }

    /// M3: a publish after the app was away, or after a pause, is the first of a run, not a late one.
    #[test]
    fn attaching_again_and_resuming_from_a_pause_are_not_late_publishes() {
        let rig = Rig::new("m3-attach.bin", fast());
        let format = Format::new(320, 180, 30, SUBTYPE_NV12);
        let mut consumer = rig.consumer_with("m3-attach.bin", format);
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        wait_stats(&rig, "30 frames", |s| s.published >= 30);

        // The count of late publishes also moves when this thread or the render thread is not scheduled for a while
        // (a busy machine), whatever the first publish does. The bug is a late publish at every return, so it is
        // enough that one return in a few has none; a scheduling delay would have to hit all of them.
        const TRIES: usize = 3;

        // the app leaves for a while and comes back
        let mut seen = Vec::new();
        let mut returns = Vec::new();
        let mut detached = Vec::new();
        for _ in 0..TRIES {
            consumer.detach();
            std::thread::sleep(Duration::from_millis(1500));
            let base = wait_stats(&rig, "a fresh snapshot", |s| s.rendered >= 60);
            let at = frames_in_file(&rig);
            // (the detached one is kept: dropping it would take the new one's place in the file down with it)
            let next = rig.consumer_with("m3-attach.bin", format);
            detached.push(std::mem::replace(&mut consumer, next));
            let after = wait_stats(&rig, "frames after the app came back", |s| s.published >= at + 15);
            seen.push(after.late_publishes - base.late_publishes);
            if seen.last() == Some(&0) {
                break;
            }
            returns.push(after);
        }
        assert!(
            seen.last() == Some(&0),
            "the first publish after the app came back counted as late, every time: {seen:?} {returns:?}"
        );

        // a pause of a while
        let mut seen = Vec::new();
        for _ in 0..TRIES {
            rig.engine.set_pause(true);
            std::thread::sleep(Duration::from_millis(1500));
            let base = wait_stats(&rig, "a snapshot of the pause", |s| s.paused);
            let at = frames_in_file(&rig);
            rig.engine.set_pause(false);
            let after = wait_stats(&rig, "frames after the pause", |s| !s.paused && s.published >= at + 15);
            seen.push(after.late_publishes - base.late_publishes);
            if seen.last() == Some(&0) {
                break;
            }
        }
        assert!(
            seen.last() == Some(&0),
            "the first publish after the pause counted as late, every time: {seen:?}"
        );
    }

    /// M3: a camera that makes 15 fps of the 30 its media type says is not late at every publish.
    #[test]
    fn a_camera_below_its_nominal_rate_is_not_late_at_every_publish() {
        let rig = Rig::new("m3-slow-camera.bin", fast());
        *rig.fake.nominal_fps.lock().unwrap() = Some(30.0);
        let _consumer = rig.consumer_with("m3-slow-camera.bin", Format::new(320, 180, 30, SUBTYPE_NV12));
        rig.engine.start(settings(bars(15), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        // The bug is a late publish at every frame (all of them); a few that a busy machine delays do not count.
        let s = wait_stats(&rig, "90 frames", |s| s.published >= 90);
        assert!(
            s.late_publishes * 2 < s.published,
            "{} of {} publishes were late on a steady 15 fps camera: {s:?}",
            s.late_publishes,
            s.published
        );
    }

    /// M4: a slow rebuild of the GPU device shows the slate after `gpu_slate_delay` (the control thread's tick does not
    /// run meanwhile, so the wait loop does it).
    #[test]
    fn a_slow_gpu_rebuild_shows_the_slate_after_the_delay_while_it_runs() {
        let mut t = fast();
        t.gpu_slate_delay = Duration::from_millis(300);
        let rig = Rig::new("m4-slate.bin", t);
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        rig.wait("video", Duration::from_secs(10), |r| r.writer_state() == LIVE);
        *rig.engine.shared.rebuild_delay.lock().unwrap() = Duration::from_secs(3);
        let t0 = Instant::now();
        rig.engine.inject_device_loss(1);
        rig.wait("the slate while the device is rebuilt", Duration::from_millis(2000), |r| {
            r.writer_state() == STARTING
        });
        assert_eq!(rig.engine.gpu_generation(), 1, "the device was rebuilt already: the slate came late");
        assert!(t0.elapsed() < Duration::from_millis(2000), "{:?}", t0.elapsed());
        // the new session's first frame ends it
        rig.wait_state(EngineState::Live, Duration::from_secs(20));
        rig.wait("video again", Duration::from_secs(10), |r| r.writer_state() == LIVE);
    }

    /// L4: a frame file that is deleted under the running engine stops it with the message that names the Repair, and
    /// a Start does not publish into the file it still has mapped.
    #[test]
    fn a_frame_file_deleted_under_the_running_engine_stops_it_with_the_repair_message() {
        let rig = Rig::new("l4-deleted.bin", fast());
        *rig.engine.shared.file_check_interval.lock().unwrap() = Some(Duration::from_millis(100));
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        // an untouched file is never a problem
        std::thread::sleep(Duration::from_millis(600));
        assert_eq!(rig.engine.state(), EngineState::Live);
        assert!(rig.engine.last_error().is_none());

        std::fs::remove_file(temp_file("l4-deleted.bin")).unwrap();
        assert!(rig.engine.wait_stopped(Duration::from_secs(5)), "{:?}", rig.states());
        let err = rig.engine.last_error().expect("an error");
        assert!(
            err.contains("frame file") && err.contains("Repair") && err.contains("virtual camera is not installed"),
            "{err}"
        );
        assert!(rig
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|(_, e)| matches!(e, EngineEvent::Error(m) if m.contains("frame file"))));
        assert_eq!(rig.alive(), 0);

        // a Start on the engine that still maps the old file: the same answer, no camera opened
        let opens = rig.opens();
        rig.engine.start(settings(bars(30), false));
        rig.wait("the error again", Duration::from_secs(5), |r| {
            r.engine.last_error().is_some_and(|e| e.contains("frame file"))
        });
        assert_eq!(rig.engine.state(), EngineState::Stopped);
        assert_eq!(rig.opens(), opens, "a camera was opened to publish into a deleted file");
    }

    /// L4: one look that says "replaced" is not enough (a Repair that swaps the file for a moment): the engine stops
    /// when two looks in a row say so.
    #[test]
    fn a_frame_file_that_is_away_for_a_single_look_does_not_stop_the_engine() {
        let rig = Rig::new("l4-swap.bin", fast());
        *rig.engine.shared.file_check_interval.lock().unwrap() = Some(Duration::from_millis(100));
        let (path, away) = (temp_file("l4-swap.bin"), temp_file("l4-swap-away.bin"));
        let _ = std::fs::remove_file(&away);
        // No timing: the control thread itself moves the file away just before its first look and back just before its
        // second, so the first look says "replaced" and the second does not, whatever the machine is doing.
        {
            let (p, a) = (path.clone(), away.clone());
            *rig.engine.shared.before_file_look.lock().unwrap() = Some(Box::new(move |n| match n {
                1 => std::fs::rename(&p, &a).unwrap(),
                2 => std::fs::rename(&a, &p).unwrap(),
                _ => {}
            }));
        }
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        rig.wait("four looks", Duration::from_secs(10), |r| {
            r.engine.shared.file_looks.load(Ordering::SeqCst) >= 4
        });
        assert_eq!(
            rig.engine.shared.file_looks_replaced.load(Ordering::SeqCst),
            1,
            "the one look that found the file away did not happen: the test proves nothing"
        );
        assert_eq!(rig.engine.state(), EngineState::Live, "{:?}", rig.states());
        assert!(rig.engine.last_error().is_none(), "{:?}", rig.engine.last_error());
        // gone for good: two looks stop it
        std::fs::remove_file(&path).unwrap();
        assert!(rig.engine.wait_stopped(Duration::from_secs(5)), "{:?}", rig.states());
        assert!(rig.engine.last_error().is_some_and(|e| e.contains("frame file")));
    }

    /// M-1: stopping a live session whose camera is slow to close: the file shows the slate at once and keeps a fresh
    /// heartbeat, not the last picture, frozen, for as long as the camera takes.
    #[test]
    fn a_stop_with_a_slow_close_shows_the_slate_while_the_camera_closes() {
        let rig = Rig::new("m1-slow-close.bin", fast());
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        rig.wait("video", Duration::from_secs(10), |r| r.writer_state() == LIVE);
        *rig.fake.close_delay.lock().unwrap() = Duration::from_millis(2000);
        let t0 = Instant::now();
        rig.engine.stop();
        rig.wait("the slate while the camera is still open", Duration::from_millis(800), |r| {
            r.writer_state() != LIVE
        });
        assert_eq!(rig.alive(), 1, "the camera was closed already: the check proves nothing");
        assert_eq!(rig.writer_state(), PAUSED);
        // it stays the slate, with a fresh heartbeat, until the camera is closed
        while rig.alive() > 0 {
            assert_eq!(rig.writer_state(), PAUSED, "the last picture came back");
            assert!(heartbeat_age_ms(&rig) < 500, "the heartbeat stopped");
            assert!(t0.elapsed() < Duration::from_secs(8));
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(rig.engine.wait_stopped(Duration::from_secs(5)));
        assert_eq!(rig.writer_state(), PAUSED);
    }

    /// M-1: a reopen keeps the last picture while the camera closes, for `gpu_slate_delay`; then the slate.
    #[test]
    fn a_reopen_with_a_slow_close_keeps_the_picture_for_a_second_and_then_shows_the_slate() {
        let mut t = fast();
        t.gpu_slate_delay = Duration::from_millis(400);
        let rig = Rig::new("m1-reopen.bin", t);
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        rig.wait("video", Duration::from_secs(10), |r| r.writer_state() == LIVE);
        *rig.fake.close_delay.lock().unwrap() = Duration::from_millis(1800);
        rig.engine.set_source(SourceUpdate {
            source: bars(15),
            flip_vertical: false,
            color: ColorSettings::default(),
            deactivate_when_not_showing: false,
        });
        let t0 = Instant::now();
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(rig.alive(), 1, "setup: the camera is closed already");
        assert_eq!(rig.writer_state(), LIVE, "the picture did not stay for the first moments of a reopen");
        rig.wait("the slate", Duration::from_millis(1200), |r| r.writer_state() == STARTING);
        assert!(t0.elapsed() >= Duration::from_millis(350), "the slate came too early: {:?}", t0.elapsed());
        assert_eq!(rig.alive(), 1, "the camera was closed before the slate");
        // the new session goes live again
        *rig.fake.close_delay.lock().unwrap() = Duration::ZERO;
        rig.wait("video again", Duration::from_secs(20), |r| r.writer_state() == LIVE);
    }

    /// M-1: a first frame that is published after the stop (the thread was past its stop check) does not put `Live`
    /// back over the slate.
    #[test]
    fn a_late_first_frame_does_not_flip_the_slate_back_to_live() {
        let rig = Rig::new("m1-late-first.bin", fast());
        *rig.engine.shared.go_live_delay.lock().unwrap() = Duration::from_millis(700);
        rig.engine.start(settings(bars(30), false));
        // the camera is open (the state is live) and the first frame is on its way, held at the lock
        rig.wait_state(EngineState::Live, Duration::from_secs(20));
        // the camera takes a while to close, so the file's state can be looked at after the late frame went live
        *rig.fake.close_delay.lock().unwrap() = Duration::from_millis(1500);
        rig.engine.stop();
        rig.wait("the slate", Duration::from_secs(2), |r| r.writer_state() == PAUSED);
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_millis(1800) {
            assert_eq!(
                rig.writer_state(),
                PAUSED,
                "the late first frame made the engine live again {:?} after the stop",
                t0.elapsed()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(rig.engine.wait_stopped(Duration::from_secs(10)));
        assert_eq!(rig.writer_state(), PAUSED);
    }

    /// The app quits while the camera does not close: the file says closed at once, and the exit waits 3 s at the most
    /// (not the 30 s of other stops).
    #[test]
    fn quitting_waits_three_seconds_at_most_for_a_render_thread_and_the_file_says_closed_meanwhile() {
        let rig = Rig::new("m1-quit.bin", fast());
        *rig.engine.shared.session_end_cap.lock().unwrap() = Some(Duration::from_secs(30));
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        rig.wait("video", Duration::from_secs(10), |r| r.writer_state() == LIVE);
        let stall = rig.fake.stall.lock().unwrap().clone();
        stall.hold();
        let t0 = Instant::now();
        std::thread::scope(|s| {
            let quit = s.spawn(|| rig.engine.close_for_exit());
            rig.wait("closed while the thread is stuck", Duration::from_secs(1), |r| r.writer_state() == CLOSED);
            assert!(!quit.is_finished(), "the quit did not wait at all");
            quit.join().unwrap();
        });
        let took = t0.elapsed();
        assert!(took >= Duration::from_millis(2500) && took < Duration::from_secs(4), "the quit took {took:?}");
        assert_eq!(rig.writer_state(), CLOSED);
        stall.release();
    }

    /// Opens leases like the fake opener, but an open waits while `hold` is set (a command that opens the camera for
    /// the controls and is slow to get its answer, with the lease lock in its hand).
    struct HoldingOpener {
        inner: crate::camera_controls::fake::FakeOpener,
        hold: Arc<AtomicBool>,
        inside: Arc<AtomicBool>,
    }

    impl ControlsOpener for HoldingOpener {
        fn open(&self, id: &str) -> Result<crate::camera_controls::Leased, String> {
            if self.hold.load(Ordering::SeqCst) {
                self.inside.store(true, Ordering::SeqCst);
                while self.hold.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                self.inside.store(false, Ordering::SeqCst);
            }
            self.inner.open(id)
        }

        fn key_of(&self, link: &str) -> Option<crate::capture::DeviceKey> {
            self.inner.key_of(link)
        }
    }

    /// A rig whose controls' opener can be held ([`HoldingOpener`]): `(rig, hold, inside)`.
    fn rig_with_holding_opener(name: &str) -> (Rig, Arc<AtomicBool>, Arc<AtomicBool>) {
        let (hold, inside) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        let (h, i) = (hold.clone(), inside.clone());
        let rig = Rig::with_opener(name, fast(), move |fake| {
            Arc::new(HoldingOpener {
                inner: crate::camera_controls::fake::FakeOpener {
                    camera: fake.controls.clone(),
                    link: FAKE_LINK.into(),
                    opened: Default::default(),
                    closed: Default::default(),
                },
                hold: h,
                inside: i,
            })
        });
        (rig, hold, inside)
    }

    /// M1: the control thread is in `open_session`, waiting in `release_lease` for the lease lock that a command holds
    /// while it opens a camera for the controls. The quit does not wait for it for ever (the old `join` did): it is over
    /// 3 s after it began, the file says closed, and the thread, left behind, writes nothing when it wakes.
    #[test]
    fn quitting_does_not_wait_for_ever_for_a_control_thread_stuck_on_the_lease() {
        let (rig, hold, inside) = rig_with_holding_opener("m1-quit-lease.bin");
        *rig.engine.shared.session_end_cap.lock().unwrap() = Some(Duration::from_secs(30));
        // no camera is open: a call for the controls opens a lease, and the open does not return
        hold.store(true, Ordering::SeqCst);
        let hub = rig.engine.controls().clone();
        let (took, finished) = std::thread::scope(|s| {
            let call = s.spawn(|| {
                let _ = hub.get(FAKE_LINK);
            });
            rig.wait("the command is inside the open", Duration::from_secs(5), |_| {
                inside.load(Ordering::SeqCst)
            });
            // a Start: the control thread comes to release the lease before it opens the camera, and waits for the lock
            rig.engine.start(settings(bars(30), false));
            std::thread::sleep(Duration::from_millis(300));
            assert_eq!(rig.opens(), 0, "setup: the camera was opened without the lease");
            let t0 = Instant::now();
            let quit = s.spawn(|| rig.engine.close_for_exit());
            while !quit.is_finished() && t0.elapsed() < Duration::from_secs(10) {
                std::thread::sleep(Duration::from_millis(5));
            }
            let (took, finished) = (t0.elapsed(), quit.is_finished());
            // whatever happened, let the command go so that the scope can end
            hold.store(false, Ordering::SeqCst);
            let _ = call.join();
            (took, finished)
        });
        assert!(finished, "the quit never ended");
        assert!(took >= Duration::from_millis(2500) && took < Duration::from_millis(3500), "the quit took {took:?}");
        assert_eq!(rig.writer_state(), CLOSED);
        // the control thread wakes now: no session, no slate over the closed file
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(rig.opens(), 0, "the thread that was left behind opened a camera");
        assert_eq!(rig.writer_state(), CLOSED);
    }

    /// M2: a second `close` (the `Drop` after the quit) does nothing and returns at once, whatever is still stuck.
    #[test]
    fn a_second_close_returns_at_once_and_does_nothing() {
        let (rig, hold, inside) = rig_with_holding_opener("m2-second-close.bin");
        *rig.engine.shared.session_end_cap.lock().unwrap() = Some(Duration::from_secs(30));
        hold.store(true, Ordering::SeqCst);
        let hub = rig.engine.controls().clone();
        std::thread::scope(|s| {
            let call = s.spawn(|| {
                let _ = hub.get(FAKE_LINK);
            });
            rig.wait("the command is inside the open", Duration::from_secs(5), |_| {
                inside.load(Ordering::SeqCst)
            });
            // the release of the lease (the end of the quit) is stuck on the lock of the command: left behind
            rig.engine.close_for_exit();
            assert_eq!(rig.writer_state(), CLOSED);
            // what a second close would write
            rig.engine.writer().set_state(WriterState::Paused);
            let t0 = Instant::now();
            rig.engine.close();
            let took = t0.elapsed();
            hold.store(false, Ordering::SeqCst);
            let _ = call.join();
            assert!(took < Duration::from_millis(200), "the second close took {took:?}");
        });
        assert_eq!(rig.writer_state(), PAUSED, "the second close ran again");
    }

    /// M1: a Start that was queued before the quit does not open a camera (and wait for its end) for an app that
    /// is leaving.
    #[test]
    fn a_command_queued_before_the_quit_is_not_run() {
        let rig = Rig::new("m1-queued.bin", fast());
        let gate = lock(&rig.engine.shared.control_gate);
        rig.engine.start(settings(bars(30), false));
        std::thread::scope(|s| {
            let quit = s.spawn(|| rig.engine.close_for_exit());
            // the quit is under way (the flag is set first thing), the control thread is held at the top of its pass
            std::thread::sleep(Duration::from_millis(300));
            drop(gate);
            quit.join().unwrap();
        });
        assert_eq!(rig.opens(), 0, "the queued Start opened a camera");
        assert_eq!(rig.writer_state(), CLOSED);
    }

    /// M3: a camera that stops sending while the stream stays open: the file shows the "starting" slate after a second
    /// (not the last picture for good), the next frame goes live again, and a camera that stays silent is reconnected.
    #[test]
    fn a_camera_that_stops_sending_frames_shows_the_slate_goes_live_again_and_is_reconnected() {
        let rig = Rig::new("m3-silent.bin", fast());
        let _consumer = rig.consumer("m3-silent.bin");
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        rig.wait("video", Duration::from_secs(10), |r| r.writer_state() == LIVE);

        rig.fake.silent.store(true, Ordering::SeqCst);
        let t0 = Instant::now();
        rig.wait("the slate", Duration::from_secs(3), |r| r.writer_state() == STARTING);
        let after = t0.elapsed();
        assert!(after >= Duration::from_millis(800), "the slate came after {after:?}");
        assert_eq!(rig.engine.state(), EngineState::Live, "the session ended too early");

        // the camera sends again: live again, and the slate does not come back
        rig.fake.silent.store(false, Ordering::SeqCst);
        rig.wait("video again", Duration::from_secs(5), |r| r.writer_state() == LIVE);
        std::thread::sleep(Duration::from_millis(1500));
        assert_eq!(rig.writer_state(), LIVE);
        assert_eq!(rig.engine.state(), EngineState::Live);

        // silent for good: the slate, then the session ends and the engine reconnects
        rig.fake.silent.store(true, Ordering::SeqCst);
        let t1 = Instant::now();
        rig.wait_state(EngineState::Reconnecting, Duration::from_secs(9));
        assert!(t1.elapsed() >= Duration::from_millis(4500), "reconnecting after {:?}", t1.elapsed());
        assert_eq!(rig.writer_state(), STARTING);
        let opens = rig.opens();
        rig.fake.silent.store(false, Ordering::SeqCst);
        rig.wait("live after the reconnect", Duration::from_secs(10), |r| r.writer_state() == LIVE);
        assert!(rig.opens() > opens, "the camera was not opened again");
        rig.wait_state(EngineState::Live, Duration::from_secs(5));
    }

    /// A camera that went silent during a pause is slated when the pause ends (not `Paused` for good), and reconnected.
    #[test]
    fn a_camera_that_goes_silent_during_a_pause_is_slated_and_reconnected_after_the_resume() {
        let rig = Rig::new("m3-silent-pause.bin", fast());
        let _consumer = rig.consumer("m3-silent-pause.bin");
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        rig.wait("video", Duration::from_secs(10), |r| r.writer_state() == LIVE);
        rig.engine.set_pause(true);
        rig.wait("the paused slate", Duration::from_secs(5), |r| r.writer_state() == PAUSED);
        rig.fake.silent.store(true, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(300));

        rig.engine.set_pause(false);
        let t0 = Instant::now();
        rig.wait("the starting slate after the resume", Duration::from_millis(2500), |r| {
            r.writer_state() == STARTING
        });
        rig.wait_state(EngineState::Reconnecting, Duration::from_secs(9));
        assert!(t0.elapsed() >= Duration::from_millis(4500), "reconnecting after {:?}", t0.elapsed());
        let opens = rig.opens();
        rig.fake.silent.store(false, Ordering::SeqCst);
        rig.wait("live after the reconnect", Duration::from_secs(10), |r| r.writer_state() == LIVE);
        assert!(rig.opens() > opens, "the camera was not opened again");
    }

    /// A camera at a long exposure keeps a frame every 1.2 s whatever its media type says (30 fps): that is not
    /// silence. Once the engine has seen the gap the file never shows the slate, and a camera that then really stops is
    /// still slated and reconnected, later (2.5 and 5 gaps).
    #[test]
    fn a_camera_at_a_long_exposure_is_not_slated_between_frames_but_a_silent_one_still_is() {
        let rig = Rig::new("m3-long-exposure.bin", fast());
        *rig.fake.nominal_fps.lock().unwrap() = Some(30.0);
        rig.fake.frame_every_ms.store(1200, Ordering::SeqCst);
        let _consumer = rig.consumer("m3-long-exposure.bin");
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        rig.wait("video", Duration::from_secs(10), |r| r.writer_state() == LIVE);
        // The first gap is the first the engine can look at: it may slate once, as it knows nothing of the camera yet.
        std::thread::sleep(Duration::from_millis(3000));
        rig.wait("video", Duration::from_secs(5), |r| r.writer_state() == LIVE);
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(8) {
            assert_eq!(rig.writer_state(), LIVE, "the slate came {:?} into a camera that sends a frame per 1.2 s", t.elapsed());
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(rig.engine.state(), EngineState::Live);

        rig.fake.silent.store(true, Ordering::SeqCst);
        let t1 = Instant::now();
        rig.wait("the slate", Duration::from_secs(6), |r| r.writer_state() == STARTING);
        assert!(t1.elapsed() >= Duration::from_millis(2000), "the slate came after {:?}", t1.elapsed());
        rig.wait_state(EngineState::Reconnecting, Duration::from_secs(9));
        assert!(t1.elapsed() >= Duration::from_millis(5000), "reconnecting after {:?}", t1.elapsed());
    }

    #[test]
    fn the_silence_thresholds_follow_the_gaps_the_camera_keeps() {
        let s = Duration::from_secs_f64;
        assert_eq!(silent_slate_after(30.0, Duration::ZERO), SILENT_SLATE_MIN);
        assert_eq!(silent_slate_after(f64::NAN, Duration::ZERO), SILENT_SLATE_MIN);
        // a camera that is slower than 3 frames per second: three periods
        assert_eq!(silent_slate_after(1.0, Duration::ZERO), s(3.0));
        // a gap that was seen: two and a half of it, with no ceiling
        assert_eq!(silent_slate_after(30.0, s(1.2)), s(3.0));
        assert_eq!(silent_slate_after(30.0, s(8.0)), s(20.0));
        assert_eq!(silent_reconnect_after(SILENT_SLATE_MIN), SILENT_RECONNECT_MIN);
        assert_eq!(silent_reconnect_after(s(3.0)), s(6.0));
        assert_eq!(silent_reconnect_after(s(20.0)), s(40.0));
        let mut gaps = FrameGaps::default();
        assert_eq!(gaps.largest(), Duration::ZERO);
        gaps.record(s(2.0));
        for _ in 0..FRAME_GAPS_KEPT - 1 {
            gaps.record(s(0.03));
        }
        assert_eq!(gaps.largest(), s(2.0));
        gaps.record(s(0.03));
        assert_eq!(gaps.largest(), s(0.03), "an old gap is forgotten after {FRAME_GAPS_KEPT} frames");
    }

    /// A control thread that was left behind and wakes between the retire of the writer and the `Closed` of the quit
    /// writes nothing over it.
    #[test]
    fn a_control_thread_that_wakes_during_the_quit_cannot_write_over_closed() {
        let (rig, hold, inside) = rig_with_holding_opener("m1-quit-order.bin");
        *rig.engine.shared.session_end_cap.lock().unwrap() = Some(Duration::from_secs(30));
        hold.store(true, Ordering::SeqCst);
        let hub = rig.engine.controls().clone();
        let writer = rig.engine.writer().clone();
        *rig.engine.shared.after_retire.lock().unwrap() = Some(Box::new(move || writer.set_state(WriterState::Paused)));
        std::thread::scope(|s| {
            let call = s.spawn(|| {
                let _ = hub.get(FAKE_LINK);
            });
            rig.wait("the command is inside the open", Duration::from_secs(5), |_| {
                inside.load(Ordering::SeqCst)
            });
            rig.engine.start(settings(bars(30), false));
            std::thread::sleep(Duration::from_millis(300));
            rig.engine.close_for_exit();
            hold.store(false, Ordering::SeqCst);
            let _ = call.join();
        });
        assert!(rig.engine.shared.after_retire.lock().unwrap().is_none(), "setup: the thread was not left behind");
        assert_eq!(rig.writer_state(), CLOSED);
    }

    #[test]
    fn a_value_dropped_off_the_thread_does_not_hold_the_caller() {
        struct Slow(Arc<AtomicBool>);
        impl Drop for Slow {
            fn drop(&mut self) {
                std::thread::sleep(Duration::from_millis(400));
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let dropped = Arc::new(AtomicBool::new(false));
        let t0 = Instant::now();
        drop_off_thread(Slow(dropped.clone()));
        assert!(t0.elapsed() < Duration::from_millis(200), "the caller waited {:?}", t0.elapsed());
        let t = Instant::now();
        while !dropped.load(Ordering::SeqCst) {
            assert!(t.elapsed() < Duration::from_secs(5), "the value was never dropped");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// A writer that was retired wrote nothing, so the record of what the file advertises says nothing is known (the
    /// next writer writes its list again).
    #[test]
    fn the_advertised_record_stays_unknown_for_a_retired_writer() {
        let path = temp_file("m-advertised-retired.bin");
        let _ = std::fs::remove_file(&path);
        let writer = Writer::create(&path).unwrap();
        let mut record = None;
        write_advertised(&writer, &mut record, (320, 180, 30, false));
        assert_eq!(record, Some((320, 180, 30, false)));
        let lease = writer.lease();
        lease.retire();
        let mut record = None;
        write_advertised(&lease, &mut record, (640, 360, 30, false));
        assert_eq!(record, None, "a list that was not written was recorded");
        // a writer that is not retired records what it wrote
        write_advertised(&writer, &mut record, (640, 360, 30, false));
        assert_eq!(record, Some((640, 360, 30, false)));
    }

    /// M-2: a render thread that is left behind writes nothing to the file, however long it lives: not when it comes
    /// back to its frame, not after the engine was shut down and a new writer took the file over, and its end resets
    /// nothing of the session that followed it.
    #[test]
    fn a_render_thread_that_was_left_behind_cannot_write_to_the_file_or_the_facts_of_a_later_one() {
        let rig = Rig::new("m2-left-behind.bin", fast());
        *rig.engine.shared.session_end_cap.lock().unwrap() = Some(Duration::from_millis(300));
        let _consumer = rig.consumer("m2-left-behind.bin");
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        rig.wait("frames", Duration::from_secs(10), |r| frames_in_file(r) >= 5);
        // the read of the camera is stuck in the driver: the thread does not see the stop
        let first = rig.fake.stall.lock().unwrap().clone();
        first.hold();
        rig.engine.stop();
        assert!(rig.engine.wait_stopped(Duration::from_secs(10)));
        assert_eq!(rig.alive(), 1, "setup: the thread is not stuck");
        assert_eq!(rig.writer_state(), PAUSED);
        let frames = rig.file.shared().header().writer_frames;

        // a new session while the old thread is still stuck
        *rig.fake.stall.lock().unwrap() = Arc::default();
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        rig.wait("video of the new session", Duration::from_secs(10), |r| r.writer_state() == LIVE);
        let token = rig.engine.session_facts().token.expect("the new session's facts");
        let frames_now = rig.file.shared().header().writer_frames;
        assert!(frames_now > frames, "setup: the new session publishes nothing");

        // the old thread comes back to its frame: it must not publish, set a state, or reset the facts
        first.release();
        rig.wait("the old camera closes", Duration::from_secs(10), |r| r.alive() == 1);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(rig.writer_state(), LIVE, "the old thread changed the state of the file");
        assert_eq!(rig.engine.session_facts().token, Some(token), "the old thread reset the new session's facts");
        // the numbers of the file only grow with the new session's frames (one writer): the sequence of every slot is even
        let h = rig.file.shared().header();
        assert!(h.writer_frames >= frames_now);
        assert_eq!(rig.engine.state(), EngineState::Live);
    }

    /// M-2, the way a change of the virtual camera goes: the engine is closed while a thread is stuck, a new writer
    /// takes the file over, then the thread wakes. It publishes nothing and its last drop does not close the file.
    #[test]
    fn a_thread_left_behind_by_an_engine_that_was_replaced_does_not_touch_the_new_writer() {
        let rig = Rig::new("m2-replaced.bin", fast());
        *rig.engine.shared.session_end_cap.lock().unwrap() = Some(Duration::from_millis(300));
        let consumer = rig.consumer("m2-replaced.bin");
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        rig.wait("frames", Duration::from_secs(10), |r| frames_in_file(r) >= 5);
        let stall = rig.fake.stall.lock().unwrap().clone();
        stall.hold();
        // the old engine goes (the thread is left behind), and its writer is released
        rig.engine.close();
        assert_eq!(rig.alive(), 1, "setup: the thread is not stuck");
        // a new engine's writer on the same path
        let path = temp_file("m2-replaced.bin");
        let new = Writer::create(&path).unwrap();
        new.set_state(WriterState::Live);
        // the app is still there: it announces itself to the new header (the old thread would publish for it)
        consumer.file.shared().set_negotiated(&Format::new(320, 180, 30, SUBTYPE_NV12));
        consumer.file.shared().set_consumer_active(true);
        let t = Instant::now();
        while !new.consumer_active() {
            assert!(t.elapsed() < Duration::from_secs(2), "setup: nobody reads the new file");
            std::thread::sleep(Duration::from_millis(10));
        }
        let format = vcam_protocol::SlotFormat { width: 320, height: 180, stride: 320, subtype: SUBTYPE_NV12 };
        new.publish(format, vcam_protocol::qpc_now(), |d| d.fill(9)).unwrap();
        let frames = new.header().writer_frames;
        // the old thread wakes: it renders its frame, tries to publish, ends, and the last of its handles goes
        stall.release();
        rig.wait("the old camera is closed", Duration::from_secs(10), |r| r.alive() == 0);
        std::thread::sleep(Duration::from_millis(200));
        let h = new.header();
        assert_eq!(h.writer_state, WriterState::Live as u32, "the old engine closed or changed the new writer's file");
        assert_eq!(h.writer_frames, frames, "the old thread published into the new writer's file");
    }

    /// The app quits while the wait of a Stop for a stuck render thread is already running: the quit cap (3 s from the
    /// start of that wait) applies to it, not the cap it began with.
    #[test]
    fn a_quit_shortens_a_wait_for_the_render_thread_that_is_already_running() {
        let rig = Rig::new("m1-quit-running.bin", fast());
        *rig.engine.shared.session_end_cap.lock().unwrap() = Some(Duration::from_secs(12));
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        rig.wait("video", Duration::from_secs(10), |r| r.writer_state() == LIVE);
        let stall = rig.fake.stall.lock().unwrap().clone();
        stall.hold();
        rig.engine.stop();
        // the control thread is in its wait for the thread (the state changes after it)
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(rig.engine.state(), EngineState::Live, "setup: the stop is not waiting");
        let t0 = Instant::now();
        rig.engine.close_for_exit();
        let took = t0.elapsed();
        stall.release();
        assert!(took < Duration::from_secs(5), "the quit waited for the cap the Stop began with: {took:?}");
        assert_eq!(rig.writer_state(), CLOSED);
    }

    /// The app quits while the controls poller hangs in the driver: the quit leaves it behind after the cap.
    #[test]
    fn a_quit_does_not_wait_for_ever_for_a_controls_poller_that_hangs() {
        let rig = Rig::new("m1-quit-poller.bin", fast());
        *rig.engine.shared.session_end_cap.lock().unwrap() = Some(Duration::from_millis(800));
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        rig.engine.shared.poller_stall.store(true, Ordering::SeqCst);
        // the poller beats every 100 ms: by now it is inside the hang
        std::thread::sleep(Duration::from_millis(400));
        let t0 = Instant::now();
        let (finished, took) = std::thread::scope(|s| {
            let quit = s.spawn(|| rig.engine.close_for_exit());
            while !quit.is_finished() && t0.elapsed() < Duration::from_secs(6) {
                std::thread::sleep(Duration::from_millis(10));
            }
            let finished = quit.is_finished();
            let took = t0.elapsed();
            // whatever happened, let the poller go so that the scope can end
            rig.engine.shared.poller_stall.store(false, Ordering::SeqCst);
            (finished, took)
        });
        assert!(finished, "the quit hung with the poller");
        assert!(took >= Duration::from_millis(600), "the quit did not wait for the poller at all: {took:?}");
        assert_eq!(rig.writer_state(), CLOSED);
    }

    /// A render thread that is stuck in the read of its camera and wakes after the stop (it was left behind) shares the
    /// GPU context with the next session: it does no frame of work, so no tap call either (the tap has the new
    /// device's renderer after a rebuild).
    #[test]
    fn a_render_thread_that_wakes_after_the_stop_does_no_gpu_work_and_no_tap_call() {
        let rig = Rig::new("m2-wakes.bin", fast());
        *rig.engine.shared.session_end_cap.lock().unwrap() = Some(Duration::from_millis(300));
        let seen = Arc::new(std::sync::atomic::AtomicU64::new(0));
        rig.engine.send(Msg::SetTap(Some(Box::new(CountingTap(seen.clone())))));
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        rig.wait("frames through the tap", Duration::from_secs(10), |_| seen.load(Ordering::SeqCst) >= 5);
        let stall = rig.fake.stall.lock().unwrap().clone();
        stall.hold();
        rig.engine.stop();
        assert!(rig.engine.wait_stopped(Duration::from_secs(10)));
        assert_eq!(rig.alive(), 1, "setup: the thread is not stuck");
        // the thread is inside its read: nothing of its last pass is in flight
        let before = seen.load(Ordering::SeqCst);
        stall.release();
        rig.wait("the old camera closes", Duration::from_secs(10), |r| r.alive() == 0);
        assert_eq!(seen.load(Ordering::SeqCst), before, "the thread that woke after the stop ran a frame");
    }

    /// Quitting while a paused render thread is slow to reach its `Paused` write: the file says `Closed` from the stop on
    /// and stays so.
    #[test]
    fn a_paused_render_thread_does_not_write_its_slate_over_the_quit() {
        let rig = Rig::new("m1-quit-paused.bin", fast());
        *rig.engine.shared.session_end_cap.lock().unwrap() = Some(Duration::from_secs(10));
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        rig.engine.set_pause(true);
        rig.wait("the paused slate", Duration::from_secs(5), |r| r.writer_state() == PAUSED);
        *rig.engine.shared.pause_write_delay.lock().unwrap() = Duration::from_millis(700);
        // the thread is inside the delay of its next write
        std::thread::sleep(Duration::from_millis(150));
        let seen = std::thread::scope(|s| {
            let quit = s.spawn(|| rig.engine.close_for_exit());
            let mut seen = Vec::new();
            while !quit.is_finished() {
                seen.push(rig.writer_state());
                std::thread::sleep(Duration::from_millis(1));
            }
            seen
        });
        let first = seen.iter().position(|s| *s == CLOSED).expect("the file never said closed");
        assert!(
            seen[first..].iter().all(|s| *s == CLOSED),
            "the paused thread wrote over the quit: {:?}",
            &seen[first..]
        );
        assert_eq!(rig.writer_state(), CLOSED);
    }

    /// A camera switch while live: the old picture stays for `gpu_slate_delay` and then gives way to the slate for as
    /// long as the new open takes; the new session's first frame ends the slate.
    #[test]
    fn a_reopen_with_a_slow_open_shows_the_slate_after_the_delay_and_goes_live_again() {
        let mut t = fast();
        t.gpu_slate_delay = Duration::from_millis(400);
        let rig = Rig::new("m1-reopen-slow-open.bin", t);
        rig.engine.start(settings(bars(30), false));
        rig.engine.wait_live(Duration::from_secs(20)).unwrap();
        rig.wait("video", Duration::from_secs(10), |r| r.writer_state() == LIVE);
        *rig.fake.open_delay.lock().unwrap() = Duration::from_millis(3500);
        rig.engine.set_source(SourceUpdate {
            source: bars(15),
            flip_vertical: false,
            color: ColorSettings::default(),
            deactivate_when_not_showing: false,
        });
        let t0 = Instant::now();
        // the old camera closes at once; the picture stays for the first moments
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(rig.writer_state(), LIVE, "the picture did not stay for the first moments of a reopen");
        rig.wait("the slate while the new camera opens", Duration::from_millis(1500), |r| {
            r.writer_state() == STARTING
        });
        assert!(t0.elapsed() >= Duration::from_millis(350), "the slate came too early: {:?}", t0.elapsed());
        assert_eq!(rig.engine.state(), EngineState::Standby, "setup: the new camera is open already");
        assert_eq!(rig.engine.reason().as_deref(), Some("reopening"));
        // the new session publishes: live again, and the slate does not come back
        rig.wait("video again", Duration::from_secs(20), |r| r.writer_state() == LIVE);
        rig.wait_state(EngineState::Live, Duration::from_secs(5));
        // past the delay since the reopen began, and past the moment the control thread cleared it
        std::thread::sleep(Duration::from_millis(600));
        assert_eq!(rig.writer_state(), LIVE, "the slate covered the live session");
    }

    /// L2: a first open that was stopped is logged as stopped and never turned into a format problem.
    #[test]
    fn a_stopped_first_open_is_not_taken_for_a_missing_format() {
        let cap = saved_format(VideoFormat::Any);
        let cap = CaptureCfg {
            // a saved format (not the device default), so that a failure to deliver in it is a format problem
            video_format: VideoFormat::Format(crate::capture::PixelFormat::Nv12),
            ..cap
        };
        let no_picture = || Failure::Retry(RetryKind::NoFrames, "no frame".into());
        let running = AtomicBool::new(false);
        assert!(matches!(
            format_problem_unless_stopped(no_picture(), &cap, true, &running),
            Failure::Format(_)
        ));
        let stopped = AtomicBool::new(true);
        let f = format_problem_unless_stopped(no_picture(), &cap, true, &stopped);
        assert!(matches!(f, Failure::Retry(RetryKind::Error, _)), "{f:?}");
        // so the fallback to the device default is not tried
        let mut tries = 0;
        let r: Result<((), Option<FormatFallback>), Failure> = open_with_fallback(&cap, |c| {
            tries += 1;
            Err(format_problem_unless_stopped(no_picture(), c, true, &stopped))
        });
        assert!(r.is_err());
        assert_eq!(tries, 1, "the device default was tried after a stop");
    }
}
