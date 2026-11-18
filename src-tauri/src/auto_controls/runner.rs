//! The runner (docs/AUTO-CONTROLS.md 2.8, phase A4): the `controls auto` thread that executes the controller's
//! [`Action`]s against the camera through the hub.
//!
//! # Thread model
//! One thread, started by the engine next to the controls poller and living as long as the engine. It never touches
//! the render thread: the picture arrives as [`MeterSample`]s in a one-slot mailbox ([`Host::sample`]), the session as
//! a small copy of the engine's facts ([`Host::facts`]); the thread waits for a sample for up to 100 ms, feeds it to
//! the [`AutoController`] and calls [`AutoController::tick`] every 100 ms. With no camera open, or nothing in Better
//! Cam auto, it only wakes ten times a second.
//!
//! # Ordering with the user
//! Every write goes through [`ControlsHub::drive`] (or [`ControlsHub::hand_back`]), which takes the same gate as the
//! user's `set_camera_control` / `set_control_mode`, and which refuses unless the record says the control is in Better
//! Cam auto. A user's change that comes first therefore wins: the next `drive` answers [`DriveError::NotApp`], the
//! runner drops the write and looks at the hub again ([`ControlsHub::auto_snapshot`]) at once. The user's value stays
//! the last driven one.
//!
//! # Engagement
//! The controller starts after [`ControlsHub::applied`] says the stored values were written for the session's token,
//! so its first write never races the re-apply. Whenever a session starts, a mode changes to Better Cam auto or the
//! user resumes a loop, the runner asks the controller where it would start ([`AutoController::seeds`]) and reads the
//! device: if the device already holds those values (the hub or `apply_on_open` wrote the resume point) the loop starts
//! without a seed write ([`Modes::seeded`]), else its first action is the seed write (exposure first, then gain).

use std::collections::HashSet;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde::Serialize;

use super::{
    Action, AutoController, AutoStatus, HandBack, Learned, MeterSample, Mode, Modes, NoOffer, Offer,
    SessionFacts, Support, WriteDone,
};
use crate::camera_controls::{same_camera, ControlsHub, DriveError};
use crate::capture::controls::{device_gone, ControlInfo, ControlKey};

/// Between two ticks of the controller.
pub const TICK: Duration = Duration::from_millis(100);

/// The `auto-status` event (docs/AUTO-CONTROLS.md 5.4): the controller's status for the camera `id`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AutoStatusEvent {
    /// The camera: the link of the record (what the frontend calls the camera's id).
    pub id: String,
    #[serde(flatten)]
    pub status: AutoStatus,
}

/// What the runner needs from the engine. The engine implements it for its shared state; tests plug in a fake.
pub trait Host: Send + Sync + 'static {
    fn hub(&self) -> &Arc<ControlsHub>;
    /// The session as the render loop keeps it (no session: `token` is `None`).
    fn facts(&self) -> crate::meter::SessionFacts;
    /// The newest measurement, waiting up to `timeout` for one.
    fn sample(&self, timeout: Duration) -> Option<MeterSample>;
    /// Switch the meter (`Engine::set_metering`).
    fn set_metering(&self, on: bool);
    /// Send `auto-status`.
    fn emit(&self, event: AutoStatusEvent);
}

/// Start the thread. The flag stops it; join the handle afterwards.
pub fn spawn(host: Arc<dyn Host>) -> io::Result<(Arc<AtomicBool>, JoinHandle<()>)> {
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let join = std::thread::Builder::new()
        .name("controls auto".into())
        .spawn(move || {
            crate::capture::ensure_com();
            let mut r = Runner::new(host);
            r.run(&flag);
            r.finish();
        })?;
    Ok((stop, join))
}

/// The controller of one camera, and what the runner knows about it.
struct Session {
    ctrl: AutoController,
    /// The camera's link as the engine opened it.
    link: String,
    /// The token of the camera session the controller serves now (`None`: closed or still being applied).
    token: Option<u64>,
    /// The token the engagement logic ran for.
    engaged_for: Option<u64>,
    /// The modes the controller was last set to (exposure, white balance).
    modes: (Mode, Mode),
    /// Frame rate of the last session, for the closed state.
    target_fps: f64,
}

struct Runner {
    host: Arc<dyn Host>,
    base: Instant,
    freq: f64,
    session: Option<Session>,
    /// The hub's generation the controller was last set from.
    generation: u64,
    /// The camera id of `auto-status` events.
    id: String,
    /// The controls whose "units unknown" line was logged.
    logged: HashSet<(String, ControlKey)>,
    metering: bool,
}

impl Runner {
    fn new(host: Arc<dyn Host>) -> Runner {
        Runner {
            host,
            base: Instant::now(),
            freq: vcam_protocol::qpc_frequency().max(1) as f64,
            session: None,
            generation: 0,
            id: String::new(),
            logged: HashSet::new(),
            metering: false,
        }
    }

    fn now(&self) -> f64 {
        self.base.elapsed().as_secs_f64()
    }

    fn run(&mut self, stop: &AtomicBool) {
        let mut last_tick = Instant::now() - TICK;
        while !stop.load(Ordering::Acquire) {
            let wait = TICK.saturating_sub(last_tick.elapsed());
            // The mailbox is read only while this thread has the meter on: whoever else switched it on owns it.
            if self.metering {
                if let Some(s) = self.host.sample(wait) {
                    self.feed(&s);
                }
            } else {
                std::thread::sleep(wait);
            }
            if last_tick.elapsed() >= TICK {
                last_tick = Instant::now();
                self.tick();
            }
        }
    }

    /// The thread ends (the engine closes): the camera keeps its last values, the meter goes off.
    fn finish(&mut self) {
        self.set_metering(false);
        self.keep_learned();
    }

    /// Give the controller one measurement. A measurement of another session, or captured before the last resume,
    /// is dropped here (the controller drops the first kind too).
    fn feed(&mut self, s: &MeterSample) {
        let Some(sess) = self.session.as_mut() else {
            return;
        };
        if sess.token != Some(s.token) {
            return;
        }
        let facts = self.host.facts();
        if facts.token != Some(s.token) || s.capture_qpc < facts.resumed_qpc {
            return;
        }
        let qpc_now = vcam_protocol::qpc_now();
        let now = self.base.elapsed().as_secs_f64();
        let t = now - qpc_now.saturating_sub(s.capture_qpc) as f64 / self.freq;
        if let Some(sess) = self.session.as_mut() {
            sess.ctrl.on_sample(s, t);
        }
    }

    /// Hand the controller's learned values to the hub's record (the frontend's store has them from `auto-status`).
    fn keep_learned(&mut self) {
        if let Some(sess) = &self.session {
            self.host
                .hub()
                .store_learned(&self.id_or(&sess.link), sess.ctrl.learned().clone());
        }
    }

    fn id_or(&self, link: &str) -> String {
        if self.id.is_empty() {
            link.to_string()
        } else {
            self.id.clone()
        }
    }

    fn tick(&mut self) {
        let hub = self.host.hub().clone();
        let facts = self.host.facts();
        let usable = facts.token.filter(|t| hub.applied(*t));
        if let Some(t) = usable {
            self.ensure_session(t);
        }
        let Some(sess) = self.session.as_mut() else {
            self.set_metering(false);
            return;
        };
        sess.token = usable;
        if usable.is_some() {
            sess.target_fps = facts.target_fps;
        }
        // The hub moved (a mode, the settings, a restart) or the session is new: set the controller from it.
        if hub.auto_generation() != self.generation || sess.engaged_for != usable {
            self.resync(usable, facts.target_fps);
        }
        let Some(sess) = self.session.as_mut() else {
            return;
        };
        let qpc_now = vcam_protocol::qpc_now();
        let now = self.base.elapsed().as_secs_f64();
        let resumed_at = if facts.resumed_qpc == 0 {
            f64::NEG_INFINITY
        } else {
            now - qpc_now.saturating_sub(facts.resumed_qpc) as f64 / self.freq
        };
        let cf = SessionFacts {
            token: usable,
            target_fps: if usable.is_some() { facts.target_fps } else { sess.target_fps },
            capture_fps: facts.capture_fps,
            paused: facts.paused,
            resumed_at,
        };
        let actions = sess.ctrl.tick(now, &cf);
        let on = usable.is_some() && sess.ctrl.wants_meter();
        if !actions.is_empty() {
            self.execute(usable, actions);
        }
        self.set_metering(on);
    }

    fn set_metering(&mut self, on: bool) {
        if on != self.metering {
            self.metering = on;
            self.host.set_metering(on);
        }
    }

    /// A controller for the camera of `token`: made when there is none, or when the camera is another one.
    fn ensure_session(&mut self, token: u64) {
        let hub = self.host.hub().clone();
        let Some((link, caps)) = hub.attached_caps(token) else {
            return;
        };
        if caps.is_empty() {
            return;
        }
        if self.session.as_ref().is_some_and(|s| same_camera(&s.link, &link)) {
            return;
        }
        // Another camera: the old controller's learned values go to its record first.
        self.keep_learned();
        let snap = hub.auto_snapshot();
        self.log_units(&link, &caps, &snap.learned);
        self.id = if snap.id.is_empty() { link.clone() } else { snap.id.clone() };
        self.generation = snap.generation;
        self.session = Some(Session {
            ctrl: AutoController::new(&caps, snap.cfg, snap.learned, snap.modes),
            link,
            token: Some(token),
            engaged_for: None,
            modes: (Mode::Manual, Mode::Manual),
            target_fps: 0.0,
        });
    }

    /// The line of 2.9, once per camera and control.
    fn log_units(&mut self, link: &str, caps: &[ControlInfo], learned: &Learned) {
        let Support { exposure, white_balance } = AutoController::support(caps, learned);
        for (key, offer) in [(ControlKey::Exposure, exposure), (ControlKey::WhiteBalance, white_balance)] {
            if offer == Offer::No(NoOffer::UnknownUnits) && self.logged.insert((link.to_string(), key)) {
                if let Some(c) = caps.iter().find(|c| c.key == key) {
                    eprintln!("[auto] {key} range {}..{} step {}: units unknown", c.min, c.max, c.step);
                }
            }
        }
    }

    /// Set the controller from the hub: settings, modes, restart requests, and where the loops start.
    fn resync(&mut self, usable: Option<u64>, target_fps: f64) {
        let hub = self.host.hub().clone();
        let snap = hub.auto_snapshot();
        let resumes = hub.take_resumes();
        self.generation = snap.generation;
        if !snap.id.is_empty() {
            self.id = snap.id.clone();
        }
        let Some(sess) = self.session.as_mut() else {
            return;
        };
        sess.ctrl.set_cfg(snap.cfg);
        let mut modes: Modes = snap.modes;
        modes.seeded = false;
        sess.ctrl.set_modes(modes);
        // A loop that (re)starts needs the seed check: a new session, a mode that became Better Cam auto, a restart.
        let now_modes = (modes.exposure, modes.white_balance);
        let starts = |old: Mode, new: Mode| new == Mode::App && old != Mode::App;
        let engage = sess.engaged_for != usable
            || !resumes.is_empty()
            || starts(sess.modes.0, now_modes.0)
            || starts(sess.modes.1, now_modes.1);
        sess.modes = now_modes;
        for key in resumes {
            sess.ctrl.resume(key);
        }
        sess.engaged_for = usable;
        let Some(token) = usable.filter(|_| engage) else {
            return;
        };
        // Where the engaged loops would start, and whether the device already holds it.
        let seeds = sess.ctrl.seeds(target_fps);
        let read = |key: ControlKey| hub.read_attached(token, key).ok().flatten();
        let holds = |info: Option<ControlInfo>, want: i32| info.is_some_and(|i| !i.auto && i.value == want);
        let mut any = false;
        let mut all = true;
        if let Some((e, g)) = seeds.exposure {
            any = true;
            all &= holds(read(ControlKey::Exposure), e);
            if let Some(g) = g {
                all &= holds(read(ControlKey::Gain), g);
            }
        }
        if let Some(k) = seeds.white_balance {
            any = true;
            all &= holds(read(ControlKey::WhiteBalance), k);
        }
        modes.seeded = any && all;
        if modes.seeded {
            sess.ctrl.set_modes(modes);
        }
    }

    /// Run the controller's actions in order.
    fn execute(&mut self, token: Option<u64>, actions: Vec<Action>) {
        let hub = self.host.hub().clone();
        let mut dropped = false;
        for action in actions {
            match action {
                Action::Write { key, value } => {
                    let (Some(token), false) = (token, dropped) else {
                        continue;
                    };
                    let started = self.now();
                    let result = hub.drive(token, key, value);
                    let done = self.now();
                    let Some(sess) = self.session.as_mut() else {
                        return;
                    };
                    match result {
                        Ok(info) => {
                            eprintln!(
                                "[auto] t={started:.1} {key} {value} -> {} ({:.0} ms)",
                                info.value,
                                (done - started) * 1000.0
                            );
                            sess.ctrl.on_write_done(WriteDone {
                                key,
                                requested: value,
                                readback: Some(info.value),
                                ok: true,
                                started,
                                done,
                            });
                        }
                        // The user's mode change came first, or the camera is gone: the loop is not ours to run.
                        Err(DriveError::NotApp | DriveError::NotAttached) => dropped = true,
                        // A lost camera: the engine's reconnect path takes over and the session restarts the loop.
                        Err(DriveError::Control(e)) if device_gone(&e) => dropped = true,
                        Err(DriveError::Control(e)) => {
                            eprintln!("[auto] t={started:.1} {key} {value} failed: {e}");
                            sess.ctrl.on_write_done(WriteDone {
                                key,
                                requested: value,
                                readback: None,
                                ok: false,
                                started,
                                done,
                            });
                        }
                    }
                }
                Action::ReadBack { key } => {
                    let (Some(token), false) = (token, dropped) else {
                        continue;
                    };
                    let now = self.now();
                    let mut keys = vec![key];
                    if key == ControlKey::Exposure {
                        // The stripes note reads the mains frequency.
                        keys.push(ControlKey::PowerlineFrequency);
                    }
                    for k in keys {
                        let Some(sess) = self.session.as_mut() else {
                            return;
                        };
                        match hub.read_attached(token, k) {
                            Ok(Some(info)) => sess.ctrl.on_readback(&info, now),
                            Ok(None) => {}
                            Err(e) if device_gone(&e) => {}
                            Err(_) if k == key => sess.ctrl.on_read_failed(k, now),
                            Err(_) => {}
                        }
                    }
                }
                Action::HandBack { key, to } => {
                    let Some(token) = token else {
                        continue;
                    };
                    self.hand_back(&hub, token, key, to);
                }
                Action::Emit => self.emit(),
            }
        }
        if dropped {
            // Look at the hub now: the mode the user chose reaches the controller before its next tick.
            self.generation = 0;
        }
    }

    fn hand_back(&mut self, hub: &ControlsHub, token: u64, key: ControlKey, to: HandBack) {
        match hub.hand_back(token, key, to) {
            Ok(info) => eprintln!("[auto] {key} handed back to {to:?} (now {}, auto {})", info.value, info.auto),
            // The control is not ours any more (the user changed its mode), or the camera is gone.
            Err(DriveError::NotApp | DriveError::NotAttached) => {}
            Err(DriveError::Control(e)) => eprintln!("[auto] {key} could not be handed back: {e}"),
        }
        // The record has the new mode; the controller learns it at the next look at the hub.
        self.generation = 0;
    }

    fn emit(&mut self) {
        let Some(sess) = self.session.as_ref() else {
            return;
        };
        let Some(status) = sess.ctrl.emitted().cloned() else {
            return;
        };
        if let Some(l) = &status.learned {
            self.host.hub().store_learned(&self.id_or(&sess.link), l.clone());
        }
        self.host.emit(AutoStatusEvent {
            id: self.id_or(&sess.link),
            status,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auto_controls::Mode as ControlMode;
    use crate::camera_controls::fake::{camera, FakeControls, FakeOpener};
    use crate::camera_controls::{parse_record, CameraControls};
    use crate::capture::controls::ControlDevice;
    use serde_json::json;
    use std::sync::atomic::AtomicBool;
    use std::sync::Mutex;

    const LINK: &str = "\\\\?\\usb#vid_0c45&pid_636d&mi_00#9&3171a958&0&0000#{e5323777-f976-4f5b-9b55-b94699c46e44}\\global";

    struct FakeHost {
        hub: Arc<ControlsHub>,
        facts: Mutex<crate::meter::SessionFacts>,
        events: Mutex<Vec<AutoStatusEvent>>,
        metering: AtomicBool,
    }

    impl Host for FakeHost {
        fn hub(&self) -> &Arc<ControlsHub> {
            &self.hub
        }
        fn facts(&self) -> crate::meter::SessionFacts {
            *self.facts.lock().unwrap()
        }
        fn sample(&self, _: Duration) -> Option<MeterSample> {
            None
        }
        fn set_metering(&self, on: bool) {
            self.metering.store(on, Ordering::SeqCst);
        }
        fn emit(&self, event: AutoStatusEvent) {
            self.events.lock().unwrap().push(event);
        }
    }

    /// A runner on a hub whose attached camera (token returned) has had its stored values applied.
    fn rig(record: serde_json::Value) -> (Runner, Arc<FakeHost>, Arc<FakeControls>, u64) {
        let cam = camera();
        let hub = Arc::new(ControlsHub::new(Arc::new(FakeOpener {
            camera: cam.clone(),
            link: LINK.into(),
            opened: Default::default(),
            closed: Default::default(),
        })));
        hub.set_record(
            parse_record(Some(&json!({ "id": LINK, "name": "W4GS", "controls": record })))
                .unwrap(),
        );
        let dev: Arc<dyn CameraControls> = cam.clone();
        let token = hub.attach(LINK, dev.clone());
        hub.apply_on_open(token, LINK, &dev, &AtomicBool::new(false)).unwrap();
        let host = Arc::new(FakeHost {
            hub,
            facts: Mutex::new(crate::meter::SessionFacts {
                token: Some(token),
                target_fps: 30.0,
                capture_fps: 30.0,
                paused: false,
                resumed_qpc: 0,
            }),
            events: Mutex::new(Vec::new()),
            metering: AtomicBool::new(false),
        });
        (Runner::new(host.clone()), host, cam, token)
    }

    #[test]
    fn a_loop_starts_without_a_seed_write_when_the_device_already_holds_it() {
        let (mut r, host, cam, _) = rig(json!({
            "exposure": { "value": -6, "auto": false, "mode": "app" },
            "gain": { "value": 0, "mode": "app" }
        }));
        cam.clear_log();
        for _ in 0..3 {
            r.tick();
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(cam.log().is_empty(), "{:?}", cam.log());
        assert!(host.metering.load(Ordering::SeqCst), "the loop wants the picture measured");
        assert!(!host.events.lock().unwrap().is_empty(), "and says what it is doing");
    }

    #[test]
    fn a_loop_whose_device_differs_from_its_seed_writes_exposure_then_gain() {
        let (mut r, host, cam, _) = rig(json!({
            "exposure": { "value": -5, "auto": false, "mode": "app" },
            "gain": { "value": 48, "mode": "app" }
        }));
        // another program (or a reopen) left other values behind
        cam.set_value(ControlKey::Exposure, -9).unwrap();
        cam.set_value(ControlKey::Gain, 3).unwrap();
        cam.clear_log();
        r.tick();
        assert_eq!(cam.log(), ["value:exposure:-5", "value:gain:48"]);
        assert_eq!(cam.get(ControlKey::Exposure).value, -5);
        // the record is the resume point of what was written
        let rec = host.hub.record().unwrap();
        assert_eq!(rec.get(ControlKey::Gain).unwrap().value, Some(48));
    }

    #[test]
    fn the_users_manual_mode_ends_the_loop_and_the_meter_goes_off() {
        let (mut r, host, cam, _) = rig(json!({
            "exposure": { "value": -6, "auto": false, "mode": "app" },
            "gain": { "value": 0, "mode": "app" }
        }));
        r.tick();
        assert!(host.metering.load(Ordering::SeqCst));
        host.hub.set_mode(LINK, ControlKey::Exposure, ControlMode::Manual).unwrap();
        cam.clear_log();
        for _ in 0..3 {
            r.tick();
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(cam.log().is_empty());
        assert!(!host.metering.load(Ordering::SeqCst));
        let last = host.events.lock().unwrap().last().cloned().unwrap();
        assert!(last.status.exposure.is_none(), "the status no longer talks about the exposure");
    }

    #[test]
    fn a_camera_that_is_not_applied_yet_is_left_alone() {
        let (mut r, host, cam, token) = rig(json!({
            "exposure": { "value": -6, "auto": false, "mode": "app" }
        }));
        // a session whose stored values are not written yet is another token
        host.facts.lock().unwrap().token = Some(token + 1);
        cam.clear_log();
        r.tick();
        assert!(cam.log().is_empty());
        assert!(!host.metering.load(Ordering::SeqCst), "nothing to measure for a closed camera");
    }

    #[test]
    fn the_event_is_the_status_with_the_camera_id() {
        let (mut r, host, ..) = rig(json!({
            "exposure": { "value": -6, "auto": false, "mode": "app" },
            "gain": { "value": 0, "mode": "app" }
        }));
        r.tick();
        let ev = host.events.lock().unwrap().first().cloned().unwrap();
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["id"], LINK);
        let e = &v["exposure"];
        assert!(e["state"] == "starting" || e["state"] == "adjusting", "{e}");
        for k in ["exposure", "gain", "cap", "cap_reason", "max_gain", "fps", "target_fps", "message"] {
            assert!(e.get(k).is_some(), "{k} missing in {e}");
        }
        assert_eq!(e["cap"], -5);
        assert_eq!(e["cap_reason"], "fps");
        assert!(v.get("white_balance").is_none(), "no white balance loop, no part: {v}");
        assert!(v.get("learned").is_none() && v.get("resume").is_none(), "{v}");
    }
}
