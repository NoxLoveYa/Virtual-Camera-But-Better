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
use crate::engine::{Engine, EventSink, ReadbackPolicy, Settings, SourceSpec, SourceUpdate};
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
}

/// Held while the virtual camera is installed, repaired or removed; releases the claim on drop.
pub struct VcamChange<'a> {
    svc: &'a Service,
}

impl Drop for VcamChange<'_> {
    fn drop(&mut self) {
        self.svc.engine_blocked.store(false, Ordering::Release);
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

    /// Where engine events go. Applies to the engine at once when it exists, else when it is created.
    pub fn set_sink(&self, sink: Arc<dyn EventSink>) {
        *lock(&self.sink) = Some(sink.clone());
        if let Some(e) = lock(&self.engine).as_ref() {
            e.set_event_sink(sink);
        }
    }

    /// Claim the right to change the virtual camera (install, repair, remove); one change at a time. With `remove`
    /// the engine is shut down first (camera and `frames.bin` released) and cannot be created until the guard drops.
    pub fn begin_vcam_change(&self, remove: bool) -> Result<VcamChange<'_>, String> {
        if self
            .vcam_busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err("Another change of the virtual camera is already running.".into());
        }
        let guard = VcamChange { svc: self };
        if remove {
            self.engine_blocked.store(true, Ordering::Release);
            self.shutdown();
        }
        Ok(guard)
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
            if !vcam_install::registry::clsid_key_present() || !vcam_install::util::data_dir().is_dir() {
                return Err(VCAM_NOT_INSTALLED.into());
            }
            let e = Arc::new(Engine::new()?);
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
        let engine = self.engine()?;
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
        if let (Some(record), Some(e)) = (parse_record(controls.as_ref())?, self.existing()) {
            e.controls().set_record(Some(record));
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
        if let Some(e) = self.existing() {
            e.stop();
        }
        Ok(())
    }

    /// `set_source(source)`: reopens the camera only when a capture setting changed. Ignored while stopped.
    pub fn set_source(&self, source: SourceCfg) -> Result<(), String> {
        let update = update_from(&source)?;
        if let Some(e) = self.existing() {
            e.set_source(update);
        }
        Ok(())
    }

    /// `set_output(output)`: applies live; the advertised types follow for the next consumer.
    pub fn set_output(&self, output: OutputCfg) -> Result<(), String> {
        let settings = config::output_settings(&output)?;
        if let Some(e) = self.existing() {
            e.set_output(settings);
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
        if let Some(e) = self.existing() {
            e.set_chain(chain);
        }
        Ok(())
    }

    /// `set_pause(on)`.
    pub fn set_pause(&self, on: bool) -> Result<(), String> {
        if let Some(e) = self.existing() {
            e.set_pause(on);
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
    pub fn set_window_visible(&self, visible: bool) {
        self.window_visible.store(visible, Ordering::Release);
        self.preview.set_hidden(!visible);
        if let Ok(g) = self.engine.try_lock() {
            if let Some(e) = g.as_ref() {
                e.set_window_visible(visible);
            }
        }
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
