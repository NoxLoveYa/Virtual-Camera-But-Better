//! The functions behind the Tauri commands of plan 5.5 (engine, camera and output commands), without Tauri types, so
//! the scripted example and the tests drive the engine through exactly the code the commands run.
//!
//! [`Service`] owns the engine: it is created on the first use (a few hundred milliseconds: D3D11 device, shaders,
//! `frames.bin`), so call [`Service::init`] from a background thread at startup. Events go to the sink the owner
//! sets with [`Service::set_sink`] (the app forwards them to the webview).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use serde_json::Value;

use crate::camera_controls::{parse_record, ControlInfoDto};
use crate::config::{self, CameraDto, OutputCfg, SourceCfg};
use crate::engine::{
    Engine, EngineEvent, EngineState, EventSink, ReadbackPolicy, ResumeState, Settings, SourceSpec,
    SourceUpdate, StateEvent,
};
use crate::fx;
use crate::preview::{Preview, Sink};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// What `start_engine` says while the virtual camera is not installed: the engine would write frames nobody reads.
pub const VCAM_NOT_INSTALLED: &str =
    "Better Cam's virtual camera is not installed. Use the “Install virtual camera” button above.";
const VCAM_BEING_CHANGED: &str = "The virtual camera is being changed. Try again in a moment.";

/// The engine and everything the commands need around it.
#[derive(Default)]
pub struct Service {
    engine: Mutex<Option<Arc<Engine>>>,
    sink: Mutex<Option<Arc<dyn EventSink>>>,
    window_visible: AtomicBool,
    preview: Preview,
    /// An install, repair or removal of the virtual camera is running (one at a time).
    vcam_busy: AtomicBool,
    /// A removal is running: no engine may be created, or it would map `frames.bin` again.
    engine_blocked: AtomicBool,
    /// What the engine was doing when a virtual camera change released it, or what the owner asked for meanwhile
    /// (a Start at launch that raced with a Repair, a Stop, a changed source). The change brings it back at its end.
    parked: Mutex<Option<ResumeState>>,
    /// Makes the engine. `None` is the real thing (the virtual camera must be installed); tests give their own.
    maker: Option<EngineMaker>,
}

/// How a [`Service`] makes its engine.
pub type EngineMaker = Arc<dyn Fn() -> Result<Engine, String> + Send + Sync>;

/// Held while the virtual camera is installed, repaired or removed; releases the claim on drop. When the change
/// released the engine, dropping the guard builds a new one and starts it with what the old one was doing, so the
/// engine comes back by itself after a change, whether it worked or was declined.
pub struct VcamChange<'a> {
    svc: &'a Service,
    /// The engine was released for this change.
    released: bool,
    /// Bring the engine back at the end. Not after a removal: there is no virtual camera to write to.
    restore: bool,
}

impl VcamChange<'_> {
    /// The change takes the virtual camera away: the engine stays released.
    pub fn keep_engine_released(&mut self) {
        self.restore = false;
    }
}

impl Drop for VcamChange<'_> {
    fn drop(&mut self) {
        if self.released {
            self.svc.end_release(self.restore);
        }
        self.svc.vcam_busy.store(false, Ordering::Release);
    }
}

/// The engine's settings for a pair of configs. Rejects what could never open.
pub fn settings_from(source: &SourceCfg, output: &OutputCfg) -> Result<Settings, String> {
    source.validate()?;
    Ok(Settings {
        source: SourceSpec::Configured(source.capture()),
        output: config::output_settings(output)?,
        flip_vertical: source.flip_vertically,
        color: source.color(),
        readback: ReadbackPolicy::Immediate,
        deactivate_when_not_showing: source.deactivate_when_not_showing,
    })
}

/// The live part of a source config for `set_source`.
pub fn update_from(source: &SourceCfg) -> Result<SourceUpdate, String> {
    source.validate()?;
    Ok(SourceUpdate {
        source: SourceSpec::Configured(source.capture()),
        flip_vertical: source.flip_vertically,
        color: source.color(),
        deactivate_when_not_showing: source.deactivate_when_not_showing,
    })
}

impl Service {
    pub fn new() -> Service {
        Service::default()
    }

    /// A service around an engine that already exists (tests, the example).
    pub fn with_engine(engine: Engine) -> Service {
        let s = Service::default();
        *lock(&s.engine) = Some(Arc::new(engine));
        s
    }

    /// A service that makes its engines with `maker` instead of on the real device and the installed frame file
    /// (tests). Everything else, the claim on the virtual camera included, works as in the app.
    pub fn with_maker(maker: EngineMaker) -> Service {
        Service {
            maker: Some(maker),
            ..Service::default()
        }
    }

    /// Where engine events go. Applies to the engine at once when it exists, else when it is created.
    pub fn set_sink(&self, sink: Arc<dyn EventSink>) {
        *lock(&self.sink) = Some(sink.clone());
        if let Some(e) = lock(&self.engine).as_ref() {
            e.set_event_sink(sink);
        }
    }

    /// Claim the right to change the virtual camera (install, repair, remove); one change at a time. With
    /// `release_engine` the engine is shut down first (camera and `frames.bin` released: the helper may have to
    /// replace the file) and cannot be created until the guard drops. The claim is held until the helper has really
    /// exited (see `vcam_admin`), so nothing else starts meanwhile. It covers this process only: a restarted app, a
    /// second copy of it and `vcam-tool` are kept apart by the machine-wide lock the helper itself takes
    /// (`vcam_install::lock`), which answers them with `EXIT_BUSY`.
    pub fn begin_vcam_change(&self, release_engine: bool) -> Result<VcamChange<'_>, String> {
        if self
            .vcam_busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err("Another change of the virtual camera is already running.".into());
        }
        let mut guard = VcamChange {
            svc: self,
            released: false,
            restore: true,
        };
        if release_engine {
            self.engine_blocked.store(true, Ordering::Release);
            guard.released = true;
            // What the engine is doing, to bring it back: read before it goes. An engine that is being created
            // right now (the app was just launched) finishes first: this waits for the lock it holds.
            let resume = lock(&self.engine).as_ref().and_then(|e| e.resume_state());
            *lock(&self.parked) = resume;
            self.shutdown();
            // The engine is gone: say so, so the window shows the truth until it is back.
            self.emit_state(EngineState::Stopped, "vcam-change");
        }
        Ok(guard)
    }

    fn emit_state(&self, state: EngineState, reason: &str) {
        if let Some(s) = lock(&self.sink).clone() {
            s.emit(EngineEvent::State(StateEvent {
                state,
                reason: Some(reason.into()),
                kind: None,
            }));
        }
    }

    /// The change is over: a new engine takes the place of the one that was released and starts with what it was
    /// doing (`restore`). Nothing to bring back when the engine was stopped (the next use makes one, as before), or
    /// when the camera was removed.
    fn end_release(&self, restore: bool) {
        let parked = lock(&self.parked).take();
        self.engine_blocked.store(false, Ordering::Release);
        let (true, Some(parked)) = (restore, parked) else {
            return;
        };
        let engine = match self.engine() {
            Ok(e) => e,
            // Without the virtual camera there is nothing to bring back; the banner says what to do.
            Err(e) if e == VCAM_NOT_INSTALLED => return,
            Err(e) => {
                if let Some(s) = lock(&self.sink).clone() {
                    s.emit(EngineEvent::Error(e));
                }
                return;
            }
        };
        // A Start that came in after the block was lifted wins over the old state.
        if !engine.is_started() {
            engine.resume(parked);
        }
    }

    /// Change what is parked while the engine is released. Does nothing when nothing is parked.
    fn with_parked(&self, f: impl FnOnce(&mut ResumeState)) {
        if let Some(p) = lock(&self.parked).as_mut() {
            f(p);
        }
    }

    /// The engine, created on first use. Blocks while another thread creates it. Without the virtual camera there is
    /// no engine: its frame file lives in the folder the installer makes, and nobody could read the frames.
    pub fn engine(&self) -> Result<Arc<Engine>, String> {
        let created = {
            let mut g = lock(&self.engine);
            if let Some(e) = g.as_ref() {
                return Ok(e.clone());
            }
            if self.engine_blocked.load(Ordering::Acquire) {
                return Err(VCAM_BEING_CHANGED.into());
            }
            let engine = match &self.maker {
                Some(make) => make()?,
                None => {
                    // the installer makes the frame file; the app only opens it
                    if !vcam_install::registry::clsid_key_present()
                        || !vcam_protocol::frames_path().is_file()
                    {
                        return Err(VCAM_NOT_INSTALLED.into());
                    }
                    Engine::new()?
                }
            };
            let e = Arc::new(engine);
            if let Some(s) = lock(&self.sink).clone() {
                e.set_event_sink(s);
            }
            *g = Some(e.clone());
            e
        };
        // After the lock is released: a window event that found the lock taken only stored its flag.
        created.set_window_visible(self.window_visible.load(Ordering::Acquire));
        Ok(created)
    }

    /// Create the engine now (startup, off the UI thread).
    pub fn init(&self) -> Result<(), String> {
        self.engine().map(|_| ())
    }

    fn existing(&self) -> Option<Arc<Engine>> {
        lock(&self.engine).clone()
    }

    /// `list_cameras`: opens every camera briefly, so call it off the UI thread.
    pub fn list_cameras(&self) -> Result<Vec<CameraDto>, String> {
        config::list_camera_dtos()
    }

    /// `start_engine(source, output, controls, chain)`: `controls` is the record of what the user changed on the
    /// selected camera (see [`crate::camera_controls::parse_record`]), re-applied after every camera open; `chain` is
    /// the initial filter chain (an array of filters, see [`fx::parse_chain`]). Without it the engine keeps the chain
    /// it has (an earlier `set_chain`).
    pub fn start_engine(
        &self,
        source: SourceCfg,
        output: OutputCfg,
        controls: Option<Value>,
        chain: Option<Value>,
    ) -> Result<(), String> {
        let settings = settings_from(&source, &output)?;
        self.start_with(settings, controls, chain)
    }

    /// [`Service::start_engine`] with settings that are already built (the scripted example starts synthetic bars
    /// this way).
    pub fn start_with(
        &self,
        settings: Settings,
        controls: Option<Value>,
        chain: Option<Value>,
    ) -> Result<(), String> {
        let chain = chain.as_ref().map(fx::parse_chain).transpose()?;
        let record = parse_record(controls.as_ref())?;
        let engine = match self.engine() {
            Ok(e) => e,
            // The virtual camera is being changed (a Repair clicked right after launch, before the first Start went
            // through): the engine starts when the change ends, with these settings.
            Err(_) if self.engine_blocked.load(Ordering::Acquire) => {
                let mut parked = lock(&self.parked);
                let chain = chain
                    .or_else(|| parked.as_ref().map(|p| p.chain.clone()))
                    .unwrap_or_default();
                *parked = Some(ResumeState {
                    settings,
                    paused: false,
                    chain,
                    record,
                });
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        engine.controls().set_record(record);
        if let Some(chain) = chain {
            // Before `start`: both go over the same channel, so the first frame already has the chain.
            engine.set_chain(chain);
        }
        engine.start(settings);
        Ok(())
    }

    /// The record of a camera's changed controls, sent along with `set_source` when the camera changed (`None`
    /// leaves the record as it is).
    pub fn set_control_record(&self, controls: Option<Value>) -> Result<(), String> {
        let Some(record) = parse_record(controls.as_ref())? else {
            return Ok(());
        };
        match self.existing() {
            Some(e) => e.controls().set_record(Some(record)),
            None => self.with_parked(|p| p.record = Some(record)),
        }
        Ok(())
    }

    /// `get_camera_controls(id)`: what the camera supports, with the device's ranges and values. Uses the open camera,
    /// or opens the device for a few seconds.
    pub fn get_camera_controls(&self, id: &str) -> Result<Vec<ControlInfoDto>, String> {
        self.engine()?.controls().get(id)
    }

    /// `set_camera_control(id, key, value, auto)`: applied at once; returns the control as the device reports it.
    pub fn set_camera_control(
        &self,
        id: &str,
        key: &str,
        value: i32,
        auto: bool,
    ) -> Result<ControlInfoDto, String> {
        self.engine()?.controls().set(id, key, value, auto)
    }

    /// `reset_camera_controls(id)`: the device's defaults, Auto on where the device has Auto; forgets the record.
    pub fn reset_camera_controls(&self, id: &str) -> Result<Vec<ControlInfoDto>, String> {
        self.engine()?.controls().reset(id)
    }

    /// `stop_engine`.
    pub fn stop_engine(&self) -> Result<(), String> {
        match self.existing() {
            Some(e) => e.stop(),
            // Released for a virtual camera change: it stays stopped when the change ends.
            None => *lock(&self.parked) = None,
        }
        Ok(())
    }

    /// `retry_now`: the window got the focus back; a camera that is being waited for is tried at once.
    pub fn retry_now(&self) {
        if let Some(e) = self.existing() {
            e.retry_now();
        }
    }

    /// `set_source(source)`: reopens the camera only when a capture setting changed. Ignored while stopped.
    pub fn set_source(&self, source: SourceCfg) -> Result<(), String> {
        let update = update_from(&source)?;
        match self.existing() {
            Some(e) => e.set_source(update),
            None => self.with_parked(|p| {
                p.settings.source = update.source;
                p.settings.flip_vertical = update.flip_vertical;
                p.settings.color = update.color;
                p.settings.deactivate_when_not_showing = update.deactivate_when_not_showing;
            }),
        }
        Ok(())
    }

    /// `set_output(output)`: applies live; the advertised types follow for the next consumer.
    pub fn set_output(&self, output: OutputCfg) -> Result<(), String> {
        let settings = config::output_settings(&output)?;
        match self.existing() {
            Some(e) => e.set_output(settings),
            None => self.with_parked(|p| p.settings.output = settings),
        }
        Ok(())
    }

    /// `set_chain(chain)`: the filter chain as the frontend holds it, an array of `{id, kind, enabled, params}`. It
    /// applies to the next frame with no reopen and no restart; while the engine is stopped it is kept for the next
    /// start. A malformed chain is an error and changes nothing; unknown kinds are skipped by the engine.
    ///
    /// Before the engine exists (the first moments of the app) the call does nothing, like `set_output`; the chain
    /// that `start_engine` carries is the one that counts then.
    pub fn set_chain(&self, chain: &Value) -> Result<(), String> {
        let chain = fx::parse_chain(chain)?;
        match self.existing() {
            Some(e) => e.set_chain(chain),
            None => self.with_parked(|p| p.chain = chain),
        }
        Ok(())
    }

    /// `set_pause(on)`.
    pub fn set_pause(&self, on: bool) -> Result<(), String> {
        match self.existing() {
            Some(e) => e.set_pause(on),
            None => self.with_parked(|p| p.paused = on),
        }
        Ok(())
    }

    /// `set_preview(on, width, height, channel)`: frames of at most `width` x `height` pixels (and 960x540) go to
    /// `sink` while `on`; `on: false` stops them and drops the tap. Starting again replaces the stream.
    pub fn set_preview(&self, on: bool, width: u32, height: u32, sink: Sink) -> Result<(), String> {
        if on {
            self.preview.start(&*self.engine()?, (width, height), sink)
        } else {
            self.preview.stop(self.existing().as_deref());
            Ok(())
        }
    }

    /// `preview_ack(frame)`: the webview drew `frame`; the next one may go out.
    pub fn preview_ack(&self, frame: u32) {
        self.preview.ack(frame);
    }

    /// The preview's counters and the window flag (frames sent so far).
    pub fn preview(&self) -> &Preview {
        &self.preview
    }

    /// The app window is shown (and not minimized) or not. Never blocks: when the engine is being created the flag is
    /// applied right after.
    ///
    /// Called on window events and twice a second by the poll that watches the window, so a report that found the
    /// engine locked (it is being created, or released for a virtual camera change) is repeated soon. The engine is
    /// told every time (a message the engine ignores when nothing changed). Returns `true` when this call changes
    /// what the previous one said.
    pub fn set_window_visible(&self, visible: bool) -> bool {
        let changed = self.window_visible.swap(visible, Ordering::AcqRel) != visible;
        self.preview.set_hidden(!visible);
        if let Ok(g) = self.engine.try_lock() {
            if let Some(e) = g.as_ref() {
                e.set_window_visible(visible);
            }
        }
        changed
    }

    /// Stop the engine and release the camera and `frames.bin` (the app quits, or the virtual camera is removed). A
    /// later call that needs the engine creates a new one.
    pub fn shutdown(&self) {
        self.preview.stop(None);
        let e = lock(&self.engine).take();
        if let Some(e) = e {
            e.close();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::PixelFormat;
    use crate::config::{ResType, VideoFormat};
    use serde_json::json;

    /// The arguments as the frontend's `invoke("start_engine", {source, output, controls, chain})` sends them.
    #[test]
    fn start_arguments_map_to_engine_settings() {
        let source: SourceCfg = serde_json::from_value(json!({
            "video_device_id": null,
            "res_type": "custom",
            "resolution": "1280x720",
            "frame_interval": 333333,
            "video_format": "mjpeg",
            "color_space": "601",
            "color_range": "full",
            "buffering": "auto",
            "flip_vertically": true,
            "autorotation": true,
            "hw_decode": true,
            "deactivate_when_not_showing": true
        }))
        .unwrap();
        let output: OutputCfg = serde_json::from_value(json!({
            "resolution": "640x360", "fps": 30, "fit": "fill",
            "background_color": 4278190335u32, "extra_sizes": true
        }))
        .unwrap();
        let s = settings_from(&source, &output).unwrap();
        assert!(s.flip_vertical && s.deactivate_when_not_showing);
        assert_eq!(
            (s.output.width, s.output.height, s.output.fps),
            (Some(640), Some(360), Some(30))
        );
        assert_eq!(s.output.background, [255, 0, 0]);
        let SourceSpec::Configured(cap) = &s.source else {
            panic!("expected a configured camera");
        };
        assert_eq!(cap.res_type, ResType::Custom);
        assert_eq!(cap.video_format, VideoFormat::Format(PixelFormat::Mjpeg));
        assert_eq!(cap.frame_interval, 333333);
        assert!(s.color.space.is_some() && s.color.range.is_some());

        // a custom size that is not WxH never reaches the engine
        let bad = SourceCfg {
            res_type: ResType::Custom,
            resolution: "wide".into(),
            ..source.clone()
        };
        assert!(settings_from(&bad, &output)
            .unwrap_err()
            .contains("resolution"));
        assert!(update_from(&bad).is_err());
        let bad_out = OutputCfg {
            resolution: "huge".into(),
            ..output
        };
        assert!(settings_from(&source, &bad_out).is_err());
    }

    #[test]
    fn commands_before_the_engine_exists_are_harmless() {
        let svc = Service::new();
        assert!(svc.stop_engine().is_ok());
        assert!(svc.set_pause(true).is_ok());
        assert!(svc.set_source(SourceCfg::default()).is_ok());
        assert!(svc.set_output(OutputCfg::default()).is_ok());
        assert!(svc.set_chain(&json!([])).is_ok());
        svc.set_window_visible(true);
        let nowhere: Sink = Arc::new(|_| true);
        assert!(svc.set_preview(false, 0, 0, nowhere).is_ok());
        svc.preview_ack(1);
        svc.shutdown();
    }

    #[test]
    fn set_chain_takes_the_frontends_shape_and_rejects_a_malformed_chain() {
        let svc = Service::new();
        let chain = json!([
            { "id": "a1", "kind": "color_filter", "enabled": true, "params": { "saturation": -1 } },
            { "id": "b2", "kind": "crop_filter", "enabled": false, "params": { "relative": true, "left": 10 } }
        ]);
        assert!(svc.set_chain(&chain).is_ok());
        assert!(svc.set_chain(&json!([{ "kind": "color_filter" }])).is_err());
        assert!(svc.set_chain(&json!("nope")).is_err());
    }

    // ---- the engine across a change of the virtual camera (P15, rough edges 1 and 2) ---------------------------------

    use crate::engine::{OutputSettings, SyntheticSpec, Timings};
    use crate::gpu::convert::ColorSettings;
    use crate::gpu::{AdapterKind, GpuDevice};
    use std::time::{Duration, Instant};

    fn fast() -> Timings {
        Timings {
            standby_delay: Duration::from_millis(400),
            retry_interval: Duration::from_millis(300),
            tick: Duration::from_millis(20),
            stats_interval: Duration::from_millis(250),
            ..Timings::default()
        }
    }

    /// Makes engines on WARP that write to their own temporary file (never the real `frames.bin`).
    fn warp_service_maker() -> EngineMaker {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!("bettercam-api-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Arc::new(move || {
            let path = dir.join(format!("engine-{}.bin", N.fetch_add(1, Ordering::SeqCst)));
            let _ = std::fs::remove_file(&path);
            let gpu = Arc::new(GpuDevice::new(AdapterKind::Warp).map_err(|e| e.to_string())?);
            let writer = Arc::new(vcam_protocol::Writer::create(&path).map_err(|e| e.to_string())?);
            Engine::with_timings(gpu, writer, fast())
        })
    }

    /// A service whose engines run on WARP with synthetic bars.
    fn warp_service() -> (Arc<Service>, Arc<Mutex<Vec<EngineEvent>>>) {
        let svc = Arc::new(Service::with_maker(warp_service_maker()));
        let events: Arc<Mutex<Vec<EngineEvent>>> = Arc::default();
        let sink = events.clone();
        svc.set_sink(Arc::new(move |e: EngineEvent| lock(&sink).push(e)));
        (svc, events)
    }

    fn bars_settings(deactivate: bool) -> Settings {
        Settings {
            source: SourceSpec::Synthetic(SyntheticSpec {
                width: 320,
                height: 180,
                fps: 30,
                format: crate::capture::PixelFormat::Nv12,
                color: ColorSettings::default(),
            }),
            output: OutputSettings::default(),
            flip_vertical: false,
            color: ColorSettings::default(),
            readback: ReadbackPolicy::Immediate,
            deactivate_when_not_showing: deactivate,
        }
    }

    fn states(events: &Mutex<Vec<EngineEvent>>) -> Vec<(EngineState, Option<String>)> {
        lock(events)
            .iter()
            .filter_map(|e| match e {
                EngineEvent::State(s) => Some((s.state, s.reason.clone())),
                _ => None,
            })
            .collect()
    }

    fn wait_until(what: &str, f: impl Fn() -> bool) {
        let t = Instant::now();
        while !f() {
            assert!(t.elapsed() < Duration::from_secs(20), "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn the_engine_comes_back_by_itself_after_a_change_and_the_window_hears_of_the_gap() {
        let (svc, events) = warp_service();
        svc.start_with(
            bars_settings(false),
            None,
            Some(json!([{ "id": "c", "kind": "color_filter", "enabled": true, "params": {} }])),
        )
        .unwrap();
        let first = svc.engine().unwrap();
        first.wait_live(Duration::from_secs(20)).unwrap();
        svc.set_pause(true).unwrap();
        assert!(first.paused());
        drop(first);

        let change = svc.begin_vcam_change(true).unwrap();
        // released, and said so: the window shows a stopped engine for the length of the change
        assert!(svc.existing().is_none());
        assert_eq!(
            states(&events).last(),
            Some(&(EngineState::Stopped, Some("vcam-change".into())))
        );
        // what the owner does meanwhile is kept for the new engine
        svc.set_pause(false).unwrap();
        svc.set_output(OutputCfg {
            resolution: "640x360".into(),
            ..OutputCfg::default()
        })
        .unwrap();
        drop(change);

        let engine = svc.engine().unwrap();
        engine.wait_live(Duration::from_secs(20)).unwrap();
        assert!(!engine.paused());
        let back = engine.resume_state().expect("running again");
        assert_eq!(back.settings.output.width, Some(640));
        assert_eq!(back.chain.len(), 1, "the filter chain came along");
        // Stopped (the change), then the new engine's own start: opening, live
        let s = states(&events);
        let at = s.iter().position(|x| x.1.as_deref() == Some("vcam-change")).unwrap();
        assert_eq!(
            s[at + 1..],
            [(EngineState::Standby, Some("opening".into())), (EngineState::Live, None)]
        );
    }

    /// Rough edge 1: a Repair clicked within a second of launch, while the first Start is still on its way.
    #[test]
    fn a_start_that_meets_a_change_runs_when_the_change_ends() {
        let (svc, events) = warp_service();
        let change = svc.begin_vcam_change(true).unwrap();
        // the app's own Start at launch arrives now: accepted, not an error, and nothing runs yet
        svc.start_with(bars_settings(false), None, None).unwrap();
        assert!(svc.existing().is_none());
        assert_eq!(states(&events).last().map(|s| s.0), Some(EngineState::Stopped));
        drop(change);
        let engine = svc.engine().unwrap();
        engine.wait_live(Duration::from_secs(20)).unwrap();
        assert!(engine.is_started());

        // and a Start that was already running when the change began comes back as well
        let change = svc.begin_vcam_change(true).unwrap();
        drop(change);
        svc.engine().unwrap().wait_live(Duration::from_secs(20)).unwrap();
    }

    #[test]
    fn a_stopped_engine_stays_stopped_and_a_stop_during_a_change_is_kept() {
        let (svc, _events) = warp_service();
        svc.init().unwrap();
        let change = svc.begin_vcam_change(true).unwrap();
        drop(change);
        let e = svc.engine().unwrap();
        assert!(!e.is_started(), "nothing was running, nothing is started");
        drop(e);

        svc.start_with(bars_settings(false), None, None).unwrap();
        svc.engine().unwrap().wait_live(Duration::from_secs(20)).unwrap();
        let change = svc.begin_vcam_change(true).unwrap();
        svc.stop_engine().unwrap();
        drop(change);
        let e = svc.engine().unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert!(!e.is_started() && e.state() == EngineState::Stopped);
    }

    #[test]
    fn a_removal_leaves_the_engine_released() {
        let (svc, _events) = warp_service();
        svc.start_with(bars_settings(false), None, None).unwrap();
        svc.engine().unwrap().wait_live(Duration::from_secs(20)).unwrap();
        let mut change = svc.begin_vcam_change(true).unwrap();
        change.keep_engine_released();
        drop(change);
        assert!(svc.existing().is_none(), "no engine is made after a removal");
        assert!(lock(&svc.parked).is_none());
        // the next use makes one, as before
        assert!(svc.engine().is_ok());
    }

    #[test]
    fn a_failed_restore_says_so() {
        // the first engine works, the one that replaces it cannot be made
        let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let (maker_calls, inner) = (calls.clone(), warp_service_maker());
        let svc = Service::with_maker(Arc::new(move || {
            if maker_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                inner()
            } else {
                Err("no graphics".to_string())
            }
        }));
        let events: Arc<Mutex<Vec<EngineEvent>>> = Arc::default();
        let sink = events.clone();
        svc.set_sink(Arc::new(move |e: EngineEvent| lock(&sink).push(e)));
        svc.start_with(bars_settings(false), None, None).unwrap();
        svc.engine().unwrap().wait_live(Duration::from_secs(20)).unwrap();
        drop(svc.begin_vcam_change(true).unwrap());
        assert!(lock(&events).iter().any(|e| *e == EngineEvent::Error("no graphics".into())));
        // one change at a time, and the claim is free again
        assert!(svc.begin_vcam_change(false).is_ok());
    }

    /// Rough edge 4: what the window reports and what the engine does with it.
    #[test]
    fn window_visibility_reports_drive_the_camera() {
        let (svc, _events) = warp_service();
        // reported before the engine exists (the window shows during start-up): the engine starts knowing
        assert!(svc.set_window_visible(true));
        assert!(!svc.set_window_visible(true), "the same report again changes nothing");
        svc.start_with(bars_settings(true), None, None).unwrap();
        let e = svc.engine().unwrap();
        e.wait_live(Duration::from_secs(20)).unwrap();

        // minimized or hidden: Standby after the delay (400 ms here), not before
        assert!(svc.set_window_visible(false));
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(e.state(), EngineState::Live, "the camera stays on for the delay");
        wait_until("standby", || e.state() == EngineState::Standby);
        assert_eq!(e.reason().as_deref(), Some("idle"));

        // restored: live again, and it stays live however long the window is visible and reported
        assert!(svc.set_window_visible(true));
        wait_until("live", || e.state() == EngineState::Live);
        for _ in 0..12 {
            // the poll that watches the window repeats its report twice a second
            assert!(!svc.set_window_visible(true));
            std::thread::sleep(Duration::from_millis(100));
        }
        assert_eq!(e.state(), EngineState::Live);

        // hide and restore faster than the delay: the camera never closed
        assert!(svc.set_window_visible(false));
        std::thread::sleep(Duration::from_millis(150));
        assert!(svc.set_window_visible(true));
        std::thread::sleep(Duration::from_millis(700));
        assert_eq!(e.state(), EngineState::Live);

        // hide to the tray, show from it (a second hide report in between changes nothing)
        assert!(svc.set_window_visible(false));
        assert!(!svc.set_window_visible(false));
        wait_until("standby", || e.state() == EngineState::Standby);
        assert!(svc.set_window_visible(true));
        wait_until("live", || e.state() == EngineState::Live);
    }

    #[test]
    fn a_window_reported_while_the_engine_is_replaced_is_not_lost() {
        let (svc, _events) = warp_service();
        svc.set_window_visible(true);
        svc.start_with(bars_settings(true), None, None).unwrap();
        svc.engine().unwrap().wait_live(Duration::from_secs(20)).unwrap();

        // the window is hidden while the change runs: the new engine starts knowing it, and goes idle
        let change = svc.begin_vcam_change(true).unwrap();
        svc.set_window_visible(false);
        drop(change);
        let e = svc.engine().unwrap();
        wait_until("idle standby", || {
            e.state() == EngineState::Standby && e.reason().as_deref() == Some("idle")
        });

        // and shown again while the engine is being replaced: the new one is live
        let change = svc.begin_vcam_change(true).unwrap();
        svc.set_window_visible(true);
        drop(change);
        svc.engine().unwrap().wait_live(Duration::from_secs(20)).unwrap();
    }

    #[test]
    fn a_live_change_is_a_source_update_without_a_reopen() {
        let a = SourceCfg::default();
        let b = SourceCfg {
            flip_vertically: true,
            ..a.clone()
        };
        let (ua, ub) = (update_from(&a).unwrap(), update_from(&b).unwrap());
        assert_eq!(ua.source, ub.source, "flip is not a capture setting");
        assert_ne!(ua.flip_vertical, ub.flip_vertical);
    }
}
