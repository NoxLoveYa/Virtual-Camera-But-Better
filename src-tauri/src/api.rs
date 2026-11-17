//! The functions behind the Tauri commands of plan 5.5 (engine, camera and output commands), without Tauri types, so
//! the scripted example and the tests drive the engine through exactly the code the commands run.
//!
//! [`Service`] owns the engine: it is created on the first use (a few hundred milliseconds: D3D11 device, shaders,
//! `frames.bin`), so call [`Service::init`] from a background thread at startup. Events go to the sink the owner
//! sets with [`Service::set_sink`] (the app forwards them to the webview).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use serde_json::Value;

use crate::config::{self, CameraDto, OutputCfg, SourceCfg};
use crate::engine::{Engine, EventSink, ReadbackPolicy, Settings, SourceSpec, SourceUpdate};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The engine and everything the commands need around it.
#[derive(Default)]
pub struct Service {
    engine: Mutex<Option<Arc<Engine>>>,
    sink: Mutex<Option<Arc<dyn EventSink>>>,
    window_visible: AtomicBool,
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

    /// The engine, created on first use. Blocks while another thread creates it.
    pub fn engine(&self) -> Result<Arc<Engine>, String> {
        let created = {
            let mut g = lock(&self.engine);
            if let Some(e) = g.as_ref() {
                return Ok(e.clone());
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

    /// `start_engine(source, output, controls, chain)`: `controls` and `chain` are accepted and ignored until the
    /// phases that use them.
    pub fn start_engine(
        &self,
        source: SourceCfg,
        output: OutputCfg,
        controls: Option<Value>,
        chain: Option<Value>,
    ) -> Result<(), String> {
        let _ = (controls, chain);
        let settings = settings_from(&source, &output)?;
        self.engine()?.start(settings);
        Ok(())
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

    /// `set_pause(on)`.
    pub fn set_pause(&self, on: bool) -> Result<(), String> {
        if let Some(e) = self.existing() {
            e.set_pause(on);
        }
        Ok(())
    }

    /// The app window is shown (and not minimized) or not. Never blocks: when the engine is being created the flag is
    /// applied right after.
    pub fn set_window_visible(&self, visible: bool) {
        self.window_visible.store(visible, Ordering::Release);
        if let Ok(g) = self.engine.try_lock() {
            if let Some(e) = g.as_ref() {
                e.set_window_visible(visible);
            }
        }
    }

    /// Stop the engine and release the camera and `frames.bin` (the app quits).
    pub fn shutdown(&self) {
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
        svc.set_window_visible(true);
        svc.shutdown();
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
