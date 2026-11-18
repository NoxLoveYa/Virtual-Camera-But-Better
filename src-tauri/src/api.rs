//! The functions behind the Tauri commands of plan 5.5 (engine, camera and output commands), without Tauri types, so
//! the scripted example and the tests drive the engine through exactly the code the commands run.
//!
//! [`Service`] owns the engine: it is created on the first use (a few hundred milliseconds: D3D11 device, shaders,
//! `frames.bin`), so call [`Service::init`] from a background thread at startup. Events go to the sink the owner
//! sets with [`Service::set_sink`] (the app forwards them to the webview).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::auto_controls::{AutoCfg, Mode as ControlMode};
use crate::camera_controls::{parse_record, ControlInfoDto};
use crate::capture::controls::ControlKey;
use crate::config::{self, CameraDto, OutputCfg, SourceCfg};
use crate::engine::{
    Engine, EngineEvent, EngineState, EventSink, ReadbackPolicy, ResumeState, Settings, SourceSpec,
    SourceUpdate, StateEvent, SHUTDOWN_END_CAP,
};
use crate::fx;
use crate::preview::{Preview, Sink};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// How often the quit looks whether the engine lock is free and the preview has stopped.
const QUIT_LOCK_POLL: Duration = Duration::from_millis(5);

/// What `start_engine` says while the virtual camera is not installed: the engine would write frames nobody reads.
pub const VCAM_NOT_INSTALLED: &str =
    "Better Cam's virtual camera is not installed. Use the “Install virtual camera” button above.";
const VCAM_BEING_CHANGED: &str = "The virtual camera is being changed. Try again in a moment.";

/// A change to the parked state, made by a command that arrived while the engine was released.
type ParkEdit = Box<dyn FnOnce(&mut Option<ResumeState>) + Send>;

/// What the commands do while the virtual camera is changed: the engine is gone and they write to what is parked.
#[derive(Default)]
struct Park {
    /// The engine is released for a virtual camera change (or the change is ending): commands go to `parked`.
    blocked: bool,
    /// What the engine was doing when a virtual camera change released it, or what the owner asked for meanwhile
    /// (a Start at launch that raced with a Repair, a Stop, a changed source). The change brings it back at its end.
    parked: Option<ResumeState>,
    /// While the release waits for the engine to say what it was doing (the wait is not made under the lock: the
    /// control thread may be busy for seconds, and a command must not make the window wait for it): the edits that
    /// commands made meanwhile, to be applied to the answer, in order.
    awaiting: Option<Vec<ParkEdit>>,
}

impl Park {
    /// Apply an edit to the parked state now, or when the engine's answer is in.
    fn edit(&mut self, edit: ParkEdit) {
        match self.awaiting.as_mut() {
            Some(queue) => queue.push(edit),
            None => edit(&mut self.parked),
        }
    }
}

/// The engine and everything the commands need around it.
///
/// Every command that reaches the engine, and the release and the restore of it, take the `park` lock first (then the
/// `engine` lock, never the other way round). So a command lands either in the engine, before the release has read
/// its state (the snapshot is ordered after every message that is queued), or in the parked state, which the restore
/// reads under the same lock: none arrives where nobody looks.
#[derive(Default)]
pub struct Service {
    engine: Mutex<Option<Arc<Engine>>>,
    sink: Mutex<Option<Arc<dyn EventSink>>>,
    window_visible: AtomicBool,
    /// Shared so that the quit can stop it on a helper thread ([`Service::shutdown`]).
    preview: Arc<Preview>,
    /// An install, repair or removal of the virtual camera is running (one at a time).
    vcam_busy: AtomicBool,
    /// Mirrors `Park::blocked` for the code that creates the engine (it holds the `engine` lock, not the `park` one):
    /// no engine may be made while the virtual camera is being replaced, or it would map `frames.bin` again.
    engine_blocked: AtomicBool,
    park: Mutex<Park>,
    /// The app is quitting: no engine is made any more (a change that ends after the exit began must not make one).
    closed: AtomicBool,
    /// Makes the engine. `None` is the real thing (the virtual camera must be installed); tests give their own.
    maker: Option<EngineMaker>,
    #[cfg(test)]
    hooks: Mutex<Hooks>,
}

/// Test hooks at the points of a virtual camera change where a command can arrive. Each runs once.
#[cfg(test)]
#[derive(Default)]
struct Hooks {
    /// After the engine was blocked and its state read, before it is shut down (no lock is held).
    after_block: Option<Box<dyn FnOnce() + Send>>,
    /// In the restore, right after the parked state was taken and before the block is lifted: the moment a command
    /// used to be written where nobody reads it. The restore holds the lock then, so the hook starts the command
    /// on its own thread and waits a moment for it; the command is ordered after the restore, whatever the timing.
    in_restore: Option<Box<dyn FnOnce() + Send>>,
}

/// What `engine()` says once the app began to quit.
const APP_CLOSING: &str = "Better Cam is closing.";

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
            guard.released = true;
            let (engine, answer) = {
                let mut park = lock(&self.park);
                // From here on the commands write to the parked state; the ones before this point are in the
                // engine's queue, ahead of the question that is asked next.
                park.blocked = true;
                park.parked = None;
                park.awaiting = Some(Vec::new());
                self.engine_blocked.store(true, Ordering::Release);
                // What the engine is doing, to bring it back: read before it goes, by the engine itself, after
                // everything queued. An engine that is being created right now (the app was just launched)
                // finishes first: this waits for the lock it holds.
                let engine = lock(&self.engine).take();
                let answer = engine.as_ref().map(|e| e.ask_state());
                (engine, answer)
            };
            #[cfg(test)]
            self.run_hook(|h| h.after_block.take());
            // The answer is waited for outside the lock; the commands that came meanwhile are applied to it.
            let state = match (&engine, answer) {
                (Some(e), Some(answer)) => answer.wait().unwrap_or_else(|| e.resume_state()),
                _ => None,
            };
            {
                let mut park = lock(&self.park);
                park.parked = state;
                for edit in park.awaiting.take().unwrap_or_default() {
                    edit(&mut park.parked);
                }
            }
            self.preview.stop(None);
            if let Some(e) = engine {
                e.close();
            }
            // The engine is gone: say so, so the window shows the truth until it is back.
            self.emit_state(EngineState::Stopped, "vcam-change");
        }
        Ok(guard)
    }

    #[cfg(test)]
    fn run_hook(&self, take: impl FnOnce(&mut Hooks) -> Option<Box<dyn FnOnce() + Send>>) {
        let hook = take(&mut lock(&self.hooks));
        if let Some(h) = hook {
            h();
        }
    }

    fn emit_state(&self, state: EngineState, reason: &str) {
        if let Some(s) = lock(&self.sink).clone() {
            s.emit(EngineEvent::State(StateEvent::new(state, Some(reason.into()), None)));
        }
    }

    /// The change is over: a new engine takes the place of the one that was released and starts with what it was
    /// doing (`restore`). Nothing to bring back when the engine was stopped (the next use makes one, as before), or
    /// when the camera was removed.
    fn end_release(&self, restore: bool) {
        // The whole restore is under the lock the commands take: one that comes first is in the parked state read
        // here, one that comes later finds the new engine.
        let mut park = lock(&self.park);
        let parked = park.parked.take();
        #[cfg(test)]
        self.run_hook(|h| h.in_restore.take());
        park.blocked = false;
        self.engine_blocked.store(false, Ordering::Release);
        let (true, Some(parked)) = (restore, parked) else {
            return;
        };
        let engine = match self.engine() {
            Ok(e) => e,
            // Without the virtual camera there is nothing to bring back; the banner says what to do. After the app
            // began to quit there is nobody to bring it back for.
            Err(e) if e == VCAM_NOT_INSTALLED || e == APP_CLOSING => return,
            Err(e) => {
                if let Some(s) = lock(&self.sink).clone() {
                    s.emit(EngineEvent::Error(e));
                }
                return;
            }
        };
        if !engine.is_started() {
            engine.resume(parked);
        }
    }

    /// Run a command on the engine, or on the parked state while the engine is released (and nothing when there is
    /// neither: the engine does not exist yet). Under the lock that orders it with the release and the restore.
    fn route(
        &self,
        on_engine: impl FnOnce(&Arc<Engine>),
        on_parked: impl FnOnce(&mut Option<ResumeState>) + Send + 'static,
    ) {
        let mut park = lock(&self.park);
        if park.blocked {
            park.edit(Box::new(on_parked));
        } else if let Some(e) = self.existing() {
            on_engine(&e);
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
            if self.closed.load(Ordering::Acquire) {
                return Err(APP_CLOSING.into());
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
        let mut park = lock(&self.park);
        // The virtual camera is being changed (a Repair clicked right after launch, before the first Start went
        // through): the engine starts when the change ends, with these settings.
        if park.blocked {
            park.edit(Box::new(move |parked| {
                let chain = chain
                    .or_else(|| parked.as_ref().map(|p| p.chain.clone()))
                    .unwrap_or_default();
                *parked = Some(ResumeState {
                    settings,
                    paused: false,
                    chain,
                    record,
                });
            }));
            return Ok(());
        }
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
        let Some(record) = parse_record(controls.as_ref())? else {
            return Ok(());
        };
        let parked_record = record.clone();
        self.route(
            |e| e.controls().set_record(Some(record)),
            |p| {
                if let Some(p) = p {
                    p.record = Some(parked_record);
                }
            },
        );
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
    /// Every Better Cam auto mode ends; its settings and what it learned are kept.
    pub fn reset_camera_controls(&self, id: &str) -> Result<Vec<ControlInfoDto>, String> {
        self.engine()?.controls().reset(id)
    }

    /// `set_control_mode(id, key, mode)`: who drives a control (`manual`, `camera`, `app`: Better Cam auto, see
    /// docs/AUTO-CONTROLS.md 5.3). Returns the rows that changed (exposure together with gain).
    pub fn set_control_mode(
        &self,
        id: &str,
        key: &str,
        mode: ControlMode,
    ) -> Result<Vec<ControlInfoDto>, String> {
        let key = ControlKey::from_key(key).ok_or_else(|| format!("unknown control '{key}'"))?;
        self.engine()?.controls().set_mode(id, key, mode)
    }

    /// `set_auto_config(id, cfg)`: the camera's Better Cam auto settings (`{exposure: {target, longest, max_gain,
    /// speed}, white_balance: {warmth, speed}}`); live. Values are clamped to their ranges; missing fields keep
    /// their defaults.
    pub fn set_auto_config(&self, id: &str, cfg: Value) -> Result<(), String> {
        let cfg: AutoCfg = serde_json::from_value(cfg).map_err(|e| format!("auto config: {e}"))?;
        self.engine()?.controls().set_auto_config(id, cfg);
        Ok(())
    }

    /// `resume_auto(id, key)`: start Better Cam auto again after another program kept changing a control
    /// (`conflict`), and clear `blocked` ("Try again" after the camera ignored it; set the mode again after).
    pub fn resume_auto(&self, id: &str, key: &str) -> Result<(), String> {
        let key = ControlKey::from_key(key).ok_or_else(|| format!("unknown control '{key}'"))?;
        self.engine()?.controls().resume_auto(id, key)
    }

    /// `stop_engine`.
    pub fn stop_engine(&self) -> Result<(), String> {
        self.route(
            |e| e.stop(),
            // Released for a virtual camera change: it stays stopped when the change ends.
            |p| *p = None,
        );
        Ok(())
    }

    /// `retry_now`: the window got the focus back; a camera that is being waited for is tried at once. Called on the
    /// UI thread: it never waits for a lock (the engine is being created, or replaced); the focus event comes again.
    pub fn retry_now(&self) {
        if let Ok(g) = self.engine.try_lock() {
            if let Some(e) = g.as_ref() {
                e.retry_now();
            }
        }
    }

    /// `set_source(source)`: reopens the camera only when a capture setting changed. Ignored while stopped.
    pub fn set_source(&self, source: SourceCfg) -> Result<(), String> {
        let update = update_from(&source)?;
        let parked_update = update.clone();
        self.route(
            |e| e.set_source(update),
            move |p| {
                if let Some(p) = p {
                    p.settings.source = parked_update.source;
                    p.settings.flip_vertical = parked_update.flip_vertical;
                    p.settings.color = parked_update.color;
                    p.settings.deactivate_when_not_showing = parked_update.deactivate_when_not_showing;
                }
            },
        );
        Ok(())
    }

    /// `set_output(output)`: applies live; the advertised types follow for the next consumer.
    pub fn set_output(&self, output: OutputCfg) -> Result<(), String> {
        let settings = config::output_settings(&output)?;
        self.route(
            |e| e.set_output(settings),
            move |p| {
                if let Some(p) = p {
                    p.settings.output = settings;
                }
            },
        );
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
        let parked_chain = chain.clone();
        self.route(
            |e| e.set_chain(chain),
            |p| {
                if let Some(p) = p {
                    p.chain = parked_chain;
                }
            },
        );
        Ok(())
    }

    /// `set_pause(on)`.
    pub fn set_pause(&self, on: bool) -> Result<(), String> {
        self.route(
            |e| e.set_pause(on),
            move |p| {
                if let Some(p) = p {
                    p.paused = on;
                }
            },
        );
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

    /// The app quits: stop the engine and release the camera and `frames.bin`. No engine is made after this, not
    /// even by a virtual camera change that is still running and restores its engine when it ends.
    ///
    /// The whole quit takes [`SHUTDOWN_END_CAP`] at most, the waits of this function included: the engine lock is held
    /// while an engine is created (a driver that hangs there), and the preview's sender is joined while it is inside
    /// the webview's call. What does not end by then is left behind (the process exits next and frees everything).
    pub fn shutdown(&self) {
        self.closed.store(true, Ordering::Release);
        let deadline = Instant::now() + SHUTDOWN_END_CAP;
        let preview = self.preview.clone();
        let preview_stop = std::thread::Builder::new()
            .name("preview stop".into())
            .spawn(move || preview.stop(None));
        let e = loop {
            match self.engine.try_lock() {
                Ok(mut g) => break g.take(),
                Err(std::sync::TryLockError::Poisoned(p)) => break p.into_inner().take(),
                Err(std::sync::TryLockError::WouldBlock) => {
                    if Instant::now() >= deadline {
                        eprintln!("[api] the engine is busy and did not free its lock within the quit: leaving it behind");
                        break None;
                    }
                    std::thread::sleep(QUIT_LOCK_POLL);
                }
            }
        };
        if let Some(e) = e {
            // The process exits next and frees the camera: a render thread that does not end in time is left behind.
            e.close_for_exit_by(deadline);
        }
        match preview_stop {
            Ok(helper) => {
                while !helper.is_finished() {
                    if Instant::now() >= deadline {
                        eprintln!("[api] the preview did not stop within the quit: leaving it behind");
                        return;
                    }
                    std::thread::sleep(QUIT_LOCK_POLL);
                }
                let _ = helper.join();
            }
            Err(e) => eprintln!("[api] cannot start the thread that stops the preview ({e}): it is left running"),
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
        warp_service_maker_with(fast())
    }

    fn warp_service_maker_with(timings: Timings) -> EngineMaker {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!("bettercam-api-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Arc::new(move || {
            let path = dir.join(format!("engine-{}.bin", N.fetch_add(1, Ordering::SeqCst)));
            let _ = std::fs::remove_file(&path);
            let gpu = Arc::new(GpuDevice::new(AdapterKind::Warp).map_err(|e| e.to_string())?);
            let writer = Arc::new(vcam_protocol::Writer::create(&path).map_err(|e| e.to_string())?);
            Engine::with_timings(gpu, writer, timings)
        })
    }

    /// A service whose engines run on WARP with synthetic bars.
    fn warp_service() -> (Arc<Service>, Arc<Mutex<Vec<EngineEvent>>>) {
        warp_service_with(fast())
    }

    fn warp_service_with(timings: Timings) -> (Arc<Service>, Arc<Mutex<Vec<EngineEvent>>>) {
        let svc = Arc::new(Service::with_maker(warp_service_maker_with(timings)));
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
        assert!(lock(&svc.park).parked.is_none());
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
        // The delay is a second: the things that must hold within it are checked right after the report, with all of
        // it to spare; the standby itself is waited for by its event, whatever time it takes.
        let (svc, _events) = warp_service_with(Timings {
            standby_delay: Duration::from_secs(1),
            ..fast()
        });
        // reported before the engine exists (the window shows during start-up): the engine starts knowing
        assert!(svc.set_window_visible(true));
        assert!(!svc.set_window_visible(true), "the same report again changes nothing");
        svc.start_with(bars_settings(true), None, None).unwrap();
        let e = svc.engine().unwrap();
        e.wait_live(Duration::from_secs(20)).unwrap();

        // minimized or hidden: Standby after the delay, not at once
        assert!(svc.set_window_visible(false));
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

        // hide and restore at once, long before the delay: the camera never closed
        assert!(svc.set_window_visible(false));
        assert!(svc.set_window_visible(true));
        std::thread::sleep(Duration::from_millis(100));
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

    // ---- review fixes: pause, commands that race with the release and the restore, quitting ---------------------

    fn pauses(events: &Mutex<Vec<EngineEvent>>) -> Vec<bool> {
        lock(events)
            .iter()
            .filter_map(|e| match e {
                EngineEvent::Paused(p) => Some(p.paused),
                _ => None,
            })
            .collect()
    }

    /// M1: a paused engine is brought back paused, and the window is told so by an `engine-paused` event after the
    /// restore; at no point does the owner hear that the pause went away.
    #[test]
    fn a_paused_engine_comes_back_paused_and_the_window_is_told() {
        let (svc, events) = warp_service();
        svc.start_with(bars_settings(false), None, None).unwrap();
        svc.engine().unwrap().wait_live(Duration::from_secs(20)).unwrap();
        svc.set_pause(true).unwrap();
        wait_until("the pause is reported", || pauses(&events) == vec![true]);

        let change = svc.begin_vcam_change(true).unwrap();
        drop(change);
        let e = svc.engine().unwrap();
        e.wait_live(Duration::from_secs(20)).unwrap();
        assert!(e.paused(), "the new engine is paused");
        // the new engine reports it: the window follows what is true
        wait_until("the pause is reported again", || pauses(&events).len() == 2);
        assert_eq!(pauses(&events), vec![true, true], "never a report that the pause is gone");
        // one Pause click resumes video
        svc.set_pause(false).unwrap();
        wait_until("the resume is reported", || pauses(&events) == vec![true, true, false]);
    }

    /// What the owner does around a virtual camera change: each case is one command at one of the three points.
    #[derive(Clone, Copy)]
    enum When {
        /// Just before the release: the command is queued in the engine and the control thread has not read it.
        BeforeRelease,
        /// After the engine was blocked, before it is shut down.
        DuringChange,
        /// At the start of the restore.
        AtRestore,
    }

    #[derive(Clone, Copy)]
    enum Command {
        Start,
        Stop,
        Settings,
    }

    fn run_command(svc: &Service, c: Command) {
        match c {
            Command::Start => svc
                .start_with(
                    Settings {
                        source: SourceSpec::Synthetic(SyntheticSpec {
                            width: 320,
                            height: 180,
                            fps: 25,
                            format: crate::capture::PixelFormat::Nv12,
                            color: ColorSettings::default(),
                        }),
                        output: crate::config::output_settings(&OutputCfg {
                            resolution: "640x360".into(),
                            ..OutputCfg::default()
                        })
                        .unwrap(),
                        ..bars_settings(false)
                    },
                    None,
                    None,
                )
                .unwrap(),
            Command::Stop => svc.stop_engine().unwrap(),
            Command::Settings => {
                svc.set_output(OutputCfg {
                    resolution: "800x450".into(),
                    ..OutputCfg::default()
                })
                .unwrap();
                svc.set_pause(true).unwrap();
            }
        }
    }

    fn command_survives(when: When, command: Command) {
        let (svc, _events) = warp_service();
        // what the restore's hook starts on a thread of its own, to be waited for before the checks
        let racer: Arc<Mutex<Option<std::thread::JoinHandle<()>>>> = Arc::default();
        svc.start_with(bars_settings(false), None, None).unwrap();
        let engine = svc.engine().unwrap();
        engine.wait_live(Duration::from_secs(20)).unwrap();

        let change = match when {
            When::BeforeRelease => {
                // the control thread is held up: the command waits in its queue, unread, when the release asks
                // for the engine's state
                let gate = lock(engine.control_gate());
                run_command(&svc, command);
                let asked = engine.snapshot_requests();
                std::thread::scope(|s| {
                    let begin = s.spawn(|| svc.begin_vcam_change(true).unwrap());
                    while engine.snapshot_requests() == asked && !begin.is_finished() {
                        std::thread::yield_now();
                    }
                    drop(gate);
                    begin.join().unwrap()
                })
            }
            When::DuringChange => {
                let again = svc.clone();
                lock(&svc.hooks).after_block = Some(Box::new(move || run_command(&again, command)));
                svc.begin_vcam_change(true).unwrap()
            }
            When::AtRestore => {
                let change = svc.begin_vcam_change(true).unwrap();
                let (again, slot) = (svc.clone(), racer.clone());
                lock(&svc.hooks).in_restore = Some(Box::new(move || {
                    let (done, finished) = std::sync::mpsc::channel();
                    let handle = std::thread::spawn(move || {
                        run_command(&again, command);
                        let _ = done.send(());
                    });
                    *lock(&slot) = Some(handle);
                    // Where nothing is locked the command is through at once; under the lock it waits for the restore.
                    let _ = finished.recv_timeout(Duration::from_millis(300));
                }));
                change
            }
        };
        drop(engine);
        drop(change);
        if let Some(h) = lock(&racer).take() {
            h.join().unwrap();
        }

        let restored = svc.engine().unwrap();
        match command {
            Command::Stop => {
                // after everything the restore queued (a Start of the old state would be in it)
                assert!(restored.settled().is_none(), "the Stop was lost: the engine runs again");
                assert_eq!(restored.state(), EngineState::Stopped);
            }
            Command::Start => {
                restored.wait_live(Duration::from_secs(20)).unwrap();
                let s = restored.settled().expect("started");
                assert_eq!(s.settings.output.width, Some(640), "the Start was lost: the old settings run");
                match s.settings.source {
                    SourceSpec::Synthetic(b) => assert_eq!(b.fps, 25),
                    other => panic!("{other:?}"),
                }
            }
            Command::Settings => {
                restored.wait_live(Duration::from_secs(20)).unwrap();
                let s = restored.settled().expect("started");
                assert_eq!(s.settings.output.width, Some(800), "the settings change was lost");
                assert!(s.paused && restored.paused(), "the pause was lost");
            }
        }
    }

    macro_rules! command_case {
        ($name:ident, $when:expr, $command:expr) => {
            #[test]
            fn $name() {
                command_survives($when, $command);
            }
        };
    }

    command_case!(a_start_queued_before_the_release_survives, When::BeforeRelease, Command::Start);
    command_case!(a_stop_queued_before_the_release_survives, When::BeforeRelease, Command::Stop);
    command_case!(settings_queued_before_the_release_survive, When::BeforeRelease, Command::Settings);
    command_case!(a_start_between_the_block_and_the_shutdown_survives, When::DuringChange, Command::Start);
    command_case!(a_stop_between_the_block_and_the_shutdown_survives, When::DuringChange, Command::Stop);
    command_case!(settings_between_the_block_and_the_shutdown_survive, When::DuringChange, Command::Settings);
    command_case!(a_start_at_the_restore_survives, When::AtRestore, Command::Start);
    command_case!(a_stop_at_the_restore_survives, When::AtRestore, Command::Stop);
    command_case!(settings_at_the_restore_survive, When::AtRestore, Command::Settings);

    /// L2: the focus event runs on the UI thread; it must never wait for the engine to be created or replaced.
    #[test]
    fn retry_now_never_waits_for_the_engine_lock() {
        let (svc, _events) = warp_service();
        svc.init().unwrap();
        std::thread::scope(|s| {
            // the engine lock is taken the way creating or replacing the engine takes it
            let held = lock(&svc.engine);
            let (done, finished) = std::sync::mpsc::channel();
            let svc = &svc;
            s.spawn(move || {
                svc.retry_now();
                let _ = done.send(());
            });
            assert!(
                finished.recv_timeout(Duration::from_secs(10)).is_ok(),
                "retry_now waits for the lock the engine is created under"
            );
            drop(held);
        });
    }

    /// L11: a change that ends after the app began to quit does not make an engine again.
    #[test]
    fn no_engine_is_made_after_the_app_began_to_quit() {
        let (svc, _events) = warp_service();
        svc.start_with(bars_settings(false), None, None).unwrap();
        svc.engine().unwrap().wait_live(Duration::from_secs(20)).unwrap();
        let change = svc.begin_vcam_change(true).unwrap();
        svc.shutdown(); // the app exits while the helper still runs
        drop(change);
        assert!(svc.existing().is_none(), "the restore made an engine after the exit began");
        assert_eq!(svc.engine().err().as_deref(), Some(APP_CLOSING));
        assert!(svc.start_with(bars_settings(false), None, None).is_err());

        // and without a change running: no engine after shutdown either
        let (svc, _events) = warp_service();
        svc.init().unwrap();
        svc.shutdown();
        assert!(svc.engine().is_err());
    }

    /// Runs `quit` (the quit of `svc`) on its own thread and says how long it took, giving up after 10 s so that a
    /// quit that never ends fails the test instead of hanging it; `unblock` runs after that, whatever happened.
    fn time_the_quit(svc: &Service, unblock: impl FnOnce()) -> Option<Duration> {
        std::thread::scope(|s| {
            let (done, finished) = std::sync::mpsc::channel();
            s.spawn(move || {
                let t0 = Instant::now();
                svc.shutdown();
                let _ = done.send(t0.elapsed());
            });
            let took = finished.recv_timeout(Duration::from_secs(10)).ok();
            unblock();
            took
        })
    }

    /// The quit is over [`SHUTDOWN_END_CAP`] after it began even when the engine lock is held (an engine being made
    /// by a driver that hangs).
    #[test]
    fn the_quit_does_not_wait_for_ever_for_the_engine_lock() {
        let (svc, _events) = warp_service();
        svc.init().unwrap();
        let held = lock(&svc.engine);
        let mut held = Some(held);
        let took = time_the_quit(&svc, || drop(held.take()));
        let took = took.expect("the quit waited for the engine lock for ever");
        assert!(took >= Duration::from_millis(2500) && took < Duration::from_secs(5), "the quit took {took:?}");
    }

    /// The same for a preview whose sender is inside the webview's call: the stop of the preview is part of the 3 s.
    #[test]
    fn the_quit_does_not_wait_for_ever_for_a_preview_that_does_not_stop() {
        let (svc, _events) = warp_service();
        svc.start_with(bars_settings(false), None, None).unwrap();
        svc.engine().unwrap().wait_live(Duration::from_secs(20)).unwrap();
        let (inside, release) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        let (i, r) = (inside.clone(), release.clone());
        let sink: Sink = Arc::new(move |_| {
            i.store(true, Ordering::SeqCst);
            while !r.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
            true
        });
        svc.set_preview(true, 320, 180, sink).unwrap();
        wait_until("the sender is inside the sink", || inside.load(Ordering::SeqCst));
        let took = time_the_quit(&svc, || release.store(true, Ordering::SeqCst));
        let took = took.expect("the quit waited for the preview for ever");
        assert!(took >= Duration::from_millis(2500) && took < Duration::from_secs(5), "the quit took {took:?}");
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
