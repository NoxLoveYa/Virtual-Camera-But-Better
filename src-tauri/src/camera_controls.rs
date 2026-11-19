//! Device controls of the app (plan 5.5, 5.6, 6.2, 4.1 "Persistence across reconnects"): everything between the
//! Tauri commands and [`crate::capture::controls`].
//!
//! * [`ControlsHub`] knows the controls of the camera the engine has open ([`ControlsHub::attach`]) and, when the
//!   engine has none open, a short-lived **lease**: the device is opened for a call and kept for [`LEASE_IDLE`]
//!   after the last one, so a slider drag does not open and close the camera for every write. The engine closes the
//!   lease before it opens the camera itself ([`ControlsHub::release_lease`]).
//! * The hub keeps the **record**: the values and Auto flags the user changed for the selected camera. The frontend
//!   sends it with `start_engine` and `set_source`; every `set_camera_control` and `reset_camera_controls` updates
//!   it, so a reopen never re-applies a stale value. The engine applies it after every camera open
//!   ([`ControlsHub::apply_on_open`]): Auto flags first, then manual values.
//! * Calls that write (user and re-apply) pass one gate, so a re-apply that runs in the background after an open
//!   can never overwrite a value the user set meanwhile.
//! * [`ControlsHub::poll`] re-reads the controls that are on Auto, for the `controls-changed` event.
//! * Better Cam auto (docs/AUTO-CONTROLS.md 5.1-5.3): the record also holds a control's **mode** (a control the
//!   controller drives has `app` set; its value is the resume point), the camera's Better Cam auto settings and what
//!   the controller learned. [`ControlsHub::set_mode`] changes a mode, [`ControlsHub::drive`] is the controller's
//!   write (only while the mode is Better Cam auto, under the same write gate as the user's writes, so a user's mode
//!   change always wins), [`ControlsHub::hand_back`] its watchdog's. The controller thread
//!   (`auto_controls::runner`) reads [`ControlsHub::auto_snapshot`] whenever [`ControlsHub::auto_generation`]
//!   moves.
//!
//! Every device call blocks for one USB transfer (2-7 ms; the first write to white balance, focus or exposure after
//! an open takes 200-700 ms), so callers are never the UI or the render thread.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::auto_controls::exposure::ExposureCaps;
use crate::auto_controls::{
    AutoCfg, AutoController, HandBack, Learned, Mode as ControlMode, Modes, Offer, Prior, Resume,
};
use crate::capture::controls::{
    apply_snapshot, ApplyReport, ControlDevice, ControlError, ControlInfo, ControlKey,
    ControlSnapshot, DeviceControls, SkipReason, StoredControl,
};
use crate::capture::{
    create_source, links_match, list_camera_keys, parse_vid_pid, Agile, CaptureError, DeviceKey,
    KeyMatch,
};
use crate::config::pick_device;

/// How long a lease stays open after its last use.
pub const LEASE_IDLE: Duration = Duration::from_secs(3);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------------------------------------------------------
// Device seam

/// What the hub needs from a camera's controls: [`ControlDevice`] plus a full read. [`DeviceControls`] implements it;
/// tests plug in a fake.
pub trait CameraControls: ControlDevice + Send + Sync {
    fn read_all(&self) -> Result<Vec<ControlInfo>, ControlError>;
}

impl CameraControls for DeviceControls {
    fn read_all(&self) -> Result<Vec<ControlInfo>, ControlError> {
        DeviceControls::read_all(self)
    }
}

/// A device opened for a lease. `close` runs when the lease ends (after the device is dropped).
pub struct Leased {
    pub link: String,
    pub device: Arc<dyn CameraControls>,
    pub close: Box<dyn FnOnce() + Send>,
}

/// Opens a camera for a lease. The app uses [`DeviceOpener`]; tests plug in a fake.
pub trait ControlsOpener: Send + Sync {
    /// Open the camera `id` names (the exact link, else the same vid/pid on another port).
    fn open(&self, id: &str) -> Result<Leased, String>;

    /// The key (link, vid/pid, friendly name) of the present camera with this link; a cheap enumeration, no camera
    /// is opened.
    fn key_of(&self, link: &str) -> Option<DeviceKey>;
}

/// Opens the real camera without streaming it: the media source alone serves the controls.
pub struct DeviceOpener;

impl ControlsOpener for DeviceOpener {
    fn key_of(&self, link: &str) -> Option<DeviceKey> {
        list_camera_keys()
            .ok()?
            .into_iter()
            .find(|k| links_match(&k.symbolic_link, link))
    }

    fn open(&self, id: &str) -> Result<Leased, String> {
        let keys = list_camera_keys().map_err(|e| e.to_string())?;
        let key = pick_device(&keys, Some(id)).ok_or_else(|| "camera not found".to_string())?;
        let source = create_source(&key.symbolic_link).map_err(|e| e.to_string())?;
        let device = match DeviceControls::new(&source) {
            Ok(d) => d,
            Err(e) => {
                // SAFETY: plain call on the source this function created.
                let _ = unsafe { source.Shutdown() };
                return Err(e.to_string());
            }
        };
        let source = Agile(source);
        Ok(Leased {
            link: key.symbolic_link.clone(),
            device: Arc::new(device),
            close: Box::new(move || {
                // The whole wrapper moves in (a closure would capture only its field, which is not `Send`).
                let source = source;
                // SAFETY: plain call; the controls were dropped before this runs. The media source is
                // free-threaded.
                let _ = unsafe { source.0.Shutdown() };
            }),
        })
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Wire types

/// How Better Cam auto is offered for a control (`app` of [`ControlInfoDto`], docs/AUTO-CONTROLS.md 5.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AppOffer {
    /// Exposure with gain, or white balance.
    Full,
    /// Exposure in whole stops only (no usable gain).
    Stops,
}

/// One entry of the `get_camera_controls` / `controls-changed` payloads (plan 5.5, docs/AUTO-CONTROLS.md 5.3):
/// `ControlInfo` with the field names the frontend uses, and who drives the control.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ControlInfoDto {
    pub key: ControlKey,
    pub min: i32,
    pub max: i32,
    pub step: i32,
    pub def: i32,
    pub value: i32,
    pub auto: bool,
    pub can_auto: bool,
    pub can_manual: bool,
    /// `manual`, `camera` (the camera's Auto flag) or `app` (Better Cam auto; the Auto flag is off).
    pub mode: ControlMode,
    /// Better Cam auto is offered for this control (`full`, or `stops`: whole stops only); `null` when it is not,
    /// and for the controls that have none.
    pub app: Option<AppOffer>,
    /// Why Better Cam auto is not offered (the tooltip of the disabled option).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_why: Option<String>,
    /// `exposure` for the gain while the exposure loop drives it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub driven_by: Option<ControlKey>,
}

impl From<&ControlInfo> for ControlInfoDto {
    fn from(i: &ControlInfo) -> Self {
        ControlInfoDto {
            key: i.key,
            min: i.min,
            max: i.max,
            step: i.step,
            def: i.default,
            value: i.value,
            auto: i.auto,
            can_auto: i.can_auto,
            can_manual: i.can_manual,
            mode: if i.auto {
                ControlMode::Camera
            } else {
                ControlMode::Manual
            },
            app: None,
            app_why: None,
            driven_by: None,
        }
    }
}

/// The plain entries: no record, so no Better Cam auto ([`ControlsHub`] adds the modes).
pub fn dtos(infos: &[ControlInfo]) -> Vec<ControlInfoDto> {
    infos.iter().map(ControlInfoDto::from).collect()
}

/// The controls Better Cam auto drives, as the controller's key.
fn is_loop_key(key: ControlKey) -> bool {
    matches!(key, ControlKey::Exposure | ControlKey::WhiteBalance)
}

/// Gain follows the exposure loop (docs/AUTO-CONTROLS.md 2.2).
fn loop_of(key: ControlKey) -> Option<ControlKey> {
    match key {
        ControlKey::Exposure | ControlKey::Gain => Some(ControlKey::Exposure),
        ControlKey::WhiteBalance => Some(ControlKey::WhiteBalance),
        _ => None,
    }
}

/// Whether the exposure loop uses the camera's gain (2.3: manual and fine enough).
fn loop_uses_gain(infos: &[ControlInfo]) -> bool {
    ExposureCaps::from_controls(infos).is_some_and(|c| c.gain.is_some())
}

/// The mode of `key` in `rec`, with `info` as the device reports it. Gain is Better Cam auto while the exposure
/// loop drives it.
fn mode_in(rec: Option<&ControlSnapshot>, info: &ControlInfo, support: crate::auto_controls::Support) -> ControlMode {
    let app = rec.is_some_and(|r| match info.key {
        ControlKey::Exposure => r.is_app(ControlKey::Exposure) && support.exposure.offered(),
        ControlKey::WhiteBalance => {
            r.is_app(ControlKey::WhiteBalance) && support.white_balance.offered()
        }
        // The exposure loop drives the gain when the camera has a usable one ([`Offer::Full`]).
        ControlKey::Gain => r.is_app(ControlKey::Exposure) && support.exposure == Offer::Full,
        _ => false,
    });
    if app {
        ControlMode::App
    } else if info.auto {
        ControlMode::Camera
    } else {
        ControlMode::Manual
    }
}

/// The `controls` argument of `start_engine` and `set_source`: what the user changed on one camera.
///
/// ```json
/// { "id": "\\\\?\\usb#vid_0c45&pid_636d...", "name": "W4GS",
///   "controls": { "brightness": { "value": 20, "auto": false }, "focus": { "value": 300, "auto": true } } }
/// ```
///
/// `auto: true` keeps the device's own Auto mode (the value is not written); `auto: false` switches it off and
/// writes `value`. Controls the app does not know are ignored.
///
/// Better Cam auto (docs/AUTO-CONTROLS.md 5.1): `"mode": "app"` on exposure, white balance and (with exposure) gain
/// means the controller drives the control; `value` is then the resume point, written as a manual value on open.
/// `auto` is the camera's Better Cam auto settings and `learned` what it learned; both are optional and a bad one
/// is dropped, not an error (an old save must always load).
///
/// ```json
/// { "id": "...", "name": "W4GS",
///   "controls": { "exposure": { "value": -5, "auto": false, "mode": "app" }, "gain": { "value": 48, "mode": "app" } },
///   "auto": { "exposure": { "target": 0.45, "longest": "fit", "max_gain": null, "speed": "normal" } },
///   "learned": { "gain_stops": 3.0 } }
/// ```
#[derive(Debug, Deserialize)]
struct WireRecord {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    controls: BTreeMap<String, WireControl>,
    #[serde(default)]
    auto: Option<Value>,
    #[serde(default)]
    learned: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct WireControl {
    #[serde(default)]
    value: Option<i32>,
    #[serde(default)]
    auto: Option<bool>,
    #[serde(default)]
    mode: Option<String>,
}

/// Learned values inside their ranges (2.5.7, 2.6, 5.1); what is not a number or not a known key is dropped.
pub fn sanitize_learned(mut l: Learned) -> Learned {
    l.gain_stops = l
        .gain_stops
        .filter(|v| v.is_finite())
        .map(|v| v.clamp(0.5, 6.0));
    l.wb_alpha = l
        .wb_alpha
        .filter(|v| v.is_finite())
        .map(|v| v.clamp(0.002, 0.02));
    l.blocked
        .retain(|k| matches!(k, ControlKey::Exposure | ControlKey::WhiteBalance));
    l.blocked.sort();
    l.blocked.dedup();
    l.rate_caps.retain(|k, _| k.parse::<u32>().is_ok());
    for v in l.rate_caps.values_mut() {
        *v = (*v).clamp(-16, 5);
    }
    l
}

/// Parse the `controls` argument. `null` or a missing argument is no record.
pub fn parse_record(v: Option<&Value>) -> Result<Option<ControlSnapshot>, String> {
    let Some(v) = v.filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let w: WireRecord = serde_json::from_value(v.clone()).map_err(|e| format!("controls: {e}"))?;
    let mut snap = ControlSnapshot::new(DeviceKey::new(&w.id, &w.name));
    let app_of = |key: ControlKey| {
        w.controls
            .get(key.key())
            .is_some_and(|c| c.mode.as_deref() == Some("app"))
    };
    for key in ControlKey::ALL {
        let Some(c) = w.controls.get(key.key()) else {
            continue;
        };
        // Gain follows exposure: it is Better Cam auto only while exposure is.
        let app = match key {
            ControlKey::Exposure | ControlKey::WhiteBalance => app_of(key),
            ControlKey::Gain => app_of(key) && app_of(ControlKey::Exposure),
            _ => false,
        };
        if c.auto.is_none() && c.value.is_none() && !app {
            continue;
        }
        snap.controls.push(StoredControl {
            key,
            auto: if app { Some(false) } else { c.auto },
            value: if !app && c.auto == Some(true) {
                None
            } else {
                c.value
            },
            app,
        });
    }
    snap.auto = w
        .auto
        .and_then(|a| serde_json::from_value::<AutoCfg>(a).ok())
        .map(AutoCfg::sanitized);
    snap.learned = w
        .learned
        .and_then(|l| serde_json::from_value::<Learned>(l).ok())
        .map(sanitize_learned);
    Ok(Some(snap))
}

// ---------------------------------------------------------------------------------------------------------------
// Matching

/// Whether the camera `id` (a symbolic link from the frontend) is the camera with this link: the same link, else
/// the same `vid_xxxx&pid_xxxx` (it moved to another USB port).
pub fn same_camera(link: &str, id: &str) -> bool {
    links_match(link, id)
        || matches!((parse_vid_pid(link), parse_vid_pid(id)), (Some(a), Some(b)) if a == b)
}

/// Whether `record` is meant for the open camera `link`: the same link, or the same vid/pid and friendly name (the
/// camera sits on another port). `present` looks the camera's key up and is only called when the link differs but
/// the vid/pid agree.
pub fn record_applies(
    record: &DeviceKey,
    link: &str,
    present: impl FnOnce() -> Option<DeviceKey>,
) -> Option<KeyMatch> {
    if !record.symbolic_link.is_empty() && links_match(&record.symbolic_link, link) {
        return Some(KeyMatch::Exact);
    }
    let vid_pid = parse_vid_pid(link)?;
    if record.vid_pid.as_deref() != Some(vid_pid.as_str()) {
        return None;
    }
    record.matches(&present()?)
}

// ---------------------------------------------------------------------------------------------------------------
// The hub

/// The camera the engine has open.
struct Attached {
    token: u64,
    link: String,
    device: Arc<dyn CameraControls>,
    /// The last values read or written: what the window was last told, for [`ControlsHub::poll`].
    last: Vec<ControlInfo>,
}

struct Lease {
    link: String,
    device: Arc<dyn CameraControls>,
    close: Option<Box<dyn FnOnce() + Send>>,
    last_used: Instant,
}

impl Lease {
    fn end(self) {
        let Lease { device, close, .. } = self;
        // The device goes first: it holds the media source the close function shuts down.
        drop(device);
        if let Some(c) = close {
            c();
        }
    }
}

/// Where a call's device came from.
enum Source {
    /// The engine's open camera (token for the cache).
    Engine(u64),
    Lease,
}

pub struct ControlsHub {
    attached: Mutex<Option<Attached>>,
    lease: Mutex<Option<Lease>>,
    record: Mutex<Option<ControlSnapshot>>,
    /// Held by every write, user or re-apply.
    gate: Mutex<()>,
    next_token: AtomicU64,
    /// Calls that are running on a device now; an idle lease is kept while there are any.
    calls: AtomicUsize,
    /// The next poll returns the list although nothing moved ([`ControlsHub::request_resend`]).
    resend: AtomicBool,
    lease_idle: Duration,
    opener: Arc<dyn ControlsOpener>,
    /// The token of the camera whose [`apply_on_open`](ControlsHub::apply_on_open) finished (0: none yet).
    applied: AtomicU64,
    /// Moves with every change the controller must look at: a mode, the settings, a restart request.
    auto_gen: AtomicU64,
    /// A control moved (the controller wrote, a mode changed): the poller sends the list ([`take_driven`]).
    ///
    /// [`take_driven`]: ControlsHub::take_driven
    driven: AtomicBool,
    /// What the controls were before Better Cam auto took them (the controller hands back to it).
    prior: Mutex<Prior>,
    /// Controls the user asked to resume (after a conflict, "Try again" after an ignored camera).
    resumes: Mutex<Vec<ControlKey>>,
}

/// What the controller thread needs from the hub (see [`ControlsHub::auto_snapshot`]).
#[derive(Clone, Debug)]
pub struct AutoSnapshot {
    /// [`ControlsHub::auto_generation`] when it was made.
    pub generation: u64,
    /// The camera the record is for (empty when there is no record and no camera).
    pub id: String,
    pub cfg: AutoCfg,
    pub learned: Learned,
    /// `seeded` is false: the runner decides it against the device.
    pub modes: Modes,
}

/// Why [`ControlsHub::drive`] and [`ControlsHub::hand_back`] did not write.
#[derive(Debug)]
pub enum DriveError {
    /// The camera of that token is not the engine's open one any more.
    NotAttached,
    /// The control is not in Better Cam auto (the user changed its mode): the controller drops the loop.
    NotApp,
    Control(ControlError),
}

impl std::fmt::Display for DriveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DriveError::NotAttached => f.write_str("the camera is not open"),
            DriveError::NotApp => f.write_str("the control is not in Better Cam auto"),
            DriveError::Control(e) => write!(f, "{e}"),
        }
    }
}

/// Counts a running call for [`ControlsHub::expire_lease`].
struct Call<'a>(&'a AtomicUsize);

impl<'a> Call<'a> {
    fn begin(n: &'a AtomicUsize) -> Call<'a> {
        n.fetch_add(1, Ordering::SeqCst);
        Call(n)
    }
}

impl Drop for Call<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl ControlsHub {
    pub fn new(opener: Arc<dyn ControlsOpener>) -> ControlsHub {
        ControlsHub {
            attached: Mutex::new(None),
            lease: Mutex::new(None),
            record: Mutex::new(None),
            gate: Mutex::new(()),
            next_token: AtomicU64::new(1),
            calls: AtomicUsize::new(0),
            resend: AtomicBool::new(false),
            lease_idle: LEASE_IDLE,
            opener,
            applied: AtomicU64::new(0),
            auto_gen: AtomicU64::new(1),
            driven: AtomicBool::new(false),
            prior: Mutex::new(Prior::default()),
            resumes: Mutex::new(Vec::new()),
        }
    }

    /// How long an unused lease stays open (tests shorten it).
    pub fn with_lease_idle(mut self, idle: Duration) -> ControlsHub {
        self.lease_idle = idle;
        self
    }

    // ---- the record

    /// Replace the record (`start_engine`, `set_source`).
    pub fn set_record(&self, record: Option<ControlSnapshot>) {
        {
            let mut g = lock(&self.record);
            let same = match (g.as_ref(), record.as_ref()) {
                (Some(a), Some(b)) => same_camera(&a.device.symbolic_link, &b.device.symbolic_link),
                _ => false,
            };
            if !same {
                *lock(&self.prior) = Prior::default();
            }
            *g = record;
        }
        self.bump(false);
    }

    /// The controller must look at the hub again (`resend`: also send the list to the window).
    fn bump(&self, send: bool) {
        self.auto_gen.fetch_add(1, Ordering::SeqCst);
        if send {
            self.driven.store(true, Ordering::Release);
        }
    }

    pub fn record(&self) -> Option<ControlSnapshot> {
        lock(&self.record).clone()
    }

    /// The record entry for `id`, made if the record belongs to another camera. `name` is looked up only then.
    fn with_record<R>(&self, id: &str, f: impl FnOnce(&mut ControlSnapshot) -> R) -> R {
        let mut g = lock(&self.record);
        let mine = g
            .as_ref()
            .is_some_and(|r| same_camera(&r.device.symbolic_link, id));
        if !mine {
            let key = self
                .opener
                .key_of(id)
                .unwrap_or_else(|| DeviceKey::new(id, ""));
            *g = Some(ControlSnapshot::new(key));
        }
        f(g.as_mut().expect("the record was just made"))
    }

    /// Note what the device now says about `info` in the record: nothing when it is the device's own default state
    /// (Auto on where there is Auto, else the default value), else the Auto flag and the manual value.
    fn note(&self, id: &str, info: &ControlInfo) {
        let default_state = info.auto == info.can_auto && (info.auto || info.value == info.default);
        self.with_record(id, |r| {
            r.forget(info.key);
            if default_state {
                return;
            }
            if info.can_auto {
                r.record_auto(info.key, info.auto);
            }
            if !info.auto {
                r.record_value(info.key, info.value);
            }
        });
    }

    // ---- the engine's camera

    /// The engine opened a camera. Returns a token for [`detach`](Self::detach).
    pub fn attach(&self, link: &str, device: Arc<dyn CameraControls>) -> u64 {
        let token = self.next_token.fetch_add(1, Ordering::SeqCst);
        *lock(&self.attached) = Some(Attached {
            token,
            link: link.to_string(),
            device,
            last: Vec::new(),
        });
        token
    }

    /// The engine is about to close the camera: drop the hub's reference first.
    pub fn detach(&self, token: u64) {
        let mut g = lock(&self.attached);
        if g.as_ref().is_some_and(|a| a.token == token) {
            *g = None;
        }
    }

    fn set_last(&self, token: u64, infos: Vec<ControlInfo>) {
        if let Some(a) = lock(&self.attached).as_mut().filter(|a| a.token == token) {
            a.last = infos;
        }
    }

    /// Note `info` as the newest reading of its control. `true` when that differs from what was noted before.
    fn update_last(&self, token: u64, info: &ControlInfo) -> bool {
        let mut g = lock(&self.attached);
        let Some(a) = g.as_mut().filter(|a| a.token == token) else {
            return false;
        };
        match a.last.iter_mut().find(|i| i.key == info.key) {
            Some(slot) => {
                let changed = *slot != *info;
                if changed {
                    *slot = info.clone();
                }
                changed
            }
            None => {
                a.last.push(info.clone());
                true
            }
        }
    }

    /// Re-apply the record to a camera the engine just opened (any thread but the render thread; it blocks). Does
    /// nothing when the record belongs to another camera. Reads everything afterwards so [`poll`](Self::poll)
    /// knows what is on Auto; returns that state for the `controls-changed` event.
    ///
    /// `cancel` is set when the camera closes: the remaining writes are skipped.
    pub fn apply_on_open(
        &self,
        token: u64,
        link: &str,
        device: &Arc<dyn CameraControls>,
        cancel: &AtomicBool,
    ) -> Result<(ApplyReport, Vec<ControlInfoDto>), ControlError> {
        let _gate = lock(&self.gate);
        let mut report = ApplyReport::default();
        let record = self.record();
        if let Some(rec) = record.filter(|r| !r.is_empty()) {
            let mut present = None;
            let hit = record_applies(&rec.device, link, || {
                present = self.opener.key_of(link);
                present.clone()
            });
            if let Some(m) = hit {
                if m == KeyMatch::Fallback {
                    // Another port: from now on the record names this link.
                    if let (Some(key), Some(r)) = (present, lock(&self.record).as_mut()) {
                        r.device = key;
                    }
                }
                let dev = Cancellable {
                    inner: device.as_ref(),
                    cancel,
                    began: Instant::now(),
                };
                // What Better Cam auto drives goes last, exposure first (2.5.11: the slow first write, then gain),
                // as a manual value at the resume point. Everything else as always: Auto flags, then values.
                let mut plain = rec.clone();
                plain.controls.retain(|c| !c.app);
                report = apply_snapshot(&dev, &plain)?;
                // A loop that is not offered (2.3: no manual control, units Better Cam does not know, blocked) never
                // runs, so its seed would only switch the camera's Auto off and freeze the control: the camera's own
                // Auto stays on.
                let offers = if rec.controls.iter().any(|c| c.app) {
                    let caps = device.read_all()?;
                    Some(AutoController::support(&caps, &rec.learned.clone().unwrap_or_default()))
                } else {
                    None
                };
                for key in [ControlKey::Exposure, ControlKey::Gain, ControlKey::WhiteBalance] {
                    let Some(c) = rec.get(key).filter(|c| c.app) else {
                        continue;
                    };
                    let offered = offers.is_none_or(|s| match key {
                        ControlKey::WhiteBalance => s.white_balance.offered(),
                        _ => s.exposure.offered(),
                    });
                    if !offered {
                        report.skipped.push((key, SkipReason::NotOffered));
                        continue;
                    }
                    let mut one = ControlSnapshot::new(rec.device.clone());
                    one.controls.push(c.clone());
                    let r = apply_snapshot(&dev, &one)?;
                    report.applied.extend(r.applied);
                    report.skipped.extend(r.skipped);
                }
            }
        }
        let infos = device.read_all()?;
        self.normalize_app(link, &infos);
        let out = self.annotate(link, &infos);
        self.set_last(token, infos);
        if !cancel.load(Ordering::Acquire) {
            self.applied.store(token, Ordering::Release);
            self.bump(false);
        }
        Ok((report, out))
    }

    /// The gain follows the exposure: it is flagged as Better Cam auto while the exposure is and the camera has a
    /// usable gain, and never otherwise (the frontend's record may not say).
    fn normalize_app(&self, link: &str, infos: &[ControlInfo]) {
        let mut g = lock(&self.record);
        let Some(r) = g
            .as_mut()
            .filter(|r| same_camera(&r.device.symbolic_link, link))
        else {
            return;
        };
        let want = r.is_app(ControlKey::Exposure) && loop_uses_gain(infos);
        if want && !r.is_app(ControlKey::Gain) {
            if let Some(i) = infos.iter().find(|i| i.key == ControlKey::Gain) {
                r.record_app(ControlKey::Gain, i.value);
            }
        } else if !want {
            if let Some(c) = r.controls.iter_mut().find(|c| c.key == ControlKey::Gain && c.app) {
                c.app = false;
            }
        }
    }

    /// Whether [`apply_on_open`](Self::apply_on_open) finished for the camera of `token`: the controller starts
    /// after it, so its first write never races the stored values.
    pub fn applied(&self, token: u64) -> bool {
        token != 0 && self.applied.load(Ordering::Acquire) == token
    }

    /// The engine's camera when `id` names it.
    fn engine_device(&self, id: &str) -> Option<(u64, Arc<dyn CameraControls>)> {
        lock(&self.attached)
            .as_ref()
            .filter(|a| same_camera(&a.link, id))
            .map(|a| (a.token, a.device.clone()))
    }

    // ---- the lease

    /// Close the lease (the engine is about to open the camera). Blocks until it is closed.
    pub fn release_lease(&self) {
        let lease = lock(&self.lease).take();
        if let Some(l) = lease {
            l.end();
        }
    }

    /// Close the lease when it has been idle for [`LEASE_IDLE`]. Called about every 100 ms.
    pub fn expire_lease(&self) {
        let expired = {
            let mut g = lock(&self.lease);
            let idle = g
                .as_ref()
                .is_some_and(|l| l.last_used.elapsed() >= self.lease_idle);
            if idle && self.calls.load(Ordering::SeqCst) == 0 {
                g.take()
            } else {
                None
            }
        };
        if let Some(l) = expired {
            l.end();
        }
    }

    /// Whether a lease is open (tests, diagnostics).
    pub fn has_lease(&self) -> bool {
        lock(&self.lease).is_some()
    }

    /// The device for `id`: the engine's open camera, else the lease (opened if needed).
    fn device(&self, id: &str) -> Result<(Source, Arc<dyn CameraControls>), String> {
        if let Some((token, d)) = self.engine_device(id) {
            return Ok((Source::Engine(token), d));
        }
        let mut g = lock(&self.lease);
        if let Some(l) = g.as_mut().filter(|l| same_camera(&l.link, id)) {
            l.last_used = Instant::now();
            return Ok((Source::Lease, l.device.clone()));
        }
        if let Some(old) = g.take() {
            old.end();
        }
        let leased = self.opener.open(id)?;
        let device = leased.device.clone();
        *g = Some(Lease {
            link: leased.link,
            device: leased.device,
            close: Some(leased.close),
            last_used: Instant::now(),
        });
        Ok((Source::Lease, device))
    }

    /// Run `f` on the device for `id`. A device that went away (the lease was released for the engine, the camera
    /// was unplugged) is looked up again once.
    fn run<R>(
        &self,
        id: &str,
        f: impl Fn(&Source, &Arc<dyn CameraControls>) -> Result<R, ControlError>,
    ) -> Result<R, String> {
        let mut last = String::new();
        for attempt in 0..2 {
            let (src, dev) = self.device(id)?;
            let call = Call::begin(&self.calls);
            let result = f(&src, &dev);
            if let Source::Lease = src {
                // The idle time counts from the end of the call.
                if let Some(l) = lock(&self.lease).as_mut() {
                    l.last_used = Instant::now();
                }
            }
            drop(call);
            match result {
                Ok(r) => return Ok(r),
                Err(e @ ControlError::Capture(_)) => {
                    last = e.to_string();
                    if let Source::Lease = src {
                        // A broken lease is not worth keeping.
                        self.release_lease();
                    }
                    if attempt == 0 {
                        std::thread::sleep(Duration::from_millis(60));
                    }
                }
                Err(e) => return Err(e.to_string()),
            }
        }
        Err(last)
    }

    // ---- the three commands

    /// `get_camera_controls`.
    pub fn get(&self, id: &str) -> Result<Vec<ControlInfoDto>, String> {
        let infos = self.run(id, |src, dev| {
            let infos = dev.read_all()?;
            if let Source::Engine(t) = src {
                self.set_last(*t, infos.clone());
            }
            Ok(infos)
        })?;
        Ok(self.annotate(id, &infos))
    }

    /// `set_camera_control`: Auto on (`auto`), or a manual `value` (clamped to the device's range). Returns the
    /// control as the device reports it. The record follows. A control in Better Cam auto leaves it (the user's
    /// write wins: the controller's next write finds the mode changed); the gain, which follows the exposure loop,
    /// is not set by hand while that runs.
    pub fn set(
        &self,
        id: &str,
        key: &str,
        value: i32,
        auto: bool,
    ) -> Result<ControlInfoDto, String> {
        let key = ControlKey::from_key(key).ok_or_else(|| format!("unknown control '{key}'"))?;
        let _gate = lock(&self.gate);
        let was_app = self.with_record(id, |r| r.is_app(key));
        if key == ControlKey::Gain && self.with_record(id, |r| r.is_app(ControlKey::Exposure) && r.is_app(key)) {
            return Err("Gain is set by Better Cam auto exposure".into());
        }
        let info = self.run(id, |src, dev| {
            let info = if auto {
                dev.set_auto(key, true)?
            } else {
                dev.set_value(key, value)?
            };
            if let Source::Engine(t) = src {
                self.update_last(*t, &info);
            }
            Ok(info)
        })?;
        self.note(id, &info);
        if was_app {
            self.left_app(id, key);
        }
        Ok(self.annotate(id, std::slice::from_ref(&info)).remove(0))
    }

    /// `key` left Better Cam auto by the user's hand: the exposure takes the gain along (it stays at its last driven
    /// value, as a manual one). The controller finds out at its next write or its next look at the hub.
    fn left_app(&self, id: &str, key: ControlKey) {
        if key == ControlKey::Exposure {
            self.with_record(id, |r| {
                if let Some(g) = r.controls.iter_mut().find(|c| c.key == ControlKey::Gain) {
                    g.app = false;
                    g.auto = Some(false);
                }
            });
        }
        self.bump(true);
    }

    /// `reset_camera_controls`: every control back to the device's default (its default value, and Auto on where
    /// the device has Auto). The record for the camera is cleared.
    pub fn reset(&self, id: &str) -> Result<Vec<ControlInfoDto>, String> {
        let _gate = lock(&self.gate);
        let infos = self.run(id, |src, dev| {
            for i in dev.read_all()? {
                if i.auto == i.can_auto && i.value == i.default {
                    continue;
                }
                if i.can_manual && (i.value != i.default || i.auto) {
                    dev.set_value(i.key, i.default)?;
                }
                if i.can_auto {
                    dev.set_auto(i.key, true)?;
                }
            }
            let infos = dev.read_all()?;
            if let Source::Engine(t) = src {
                self.set_last(*t, infos.clone());
            }
            Ok(infos)
        })?;
        // Every Better Cam auto mode ends; its settings and what it learned are kept for next time (docs 3).
        self.with_record(id, |r| r.controls.clear());
        self.bump(true);
        Ok(self.annotate(id, &infos))
    }

    // ---- Better Cam auto

    /// The entries of `infos` (the controls of the camera `link`) with their modes and whether Better Cam auto is
    /// offered, from the record when it is for that camera.
    pub fn annotate(&self, link: &str, infos: &[ControlInfo]) -> Vec<ControlInfoDto> {
        let rec = lock(&self.record)
            .as_ref()
            .filter(|r| same_camera(&r.device.symbolic_link, link))
            .cloned();
        let learned = rec
            .as_ref()
            .and_then(|r| r.learned.clone())
            .unwrap_or_default();
        let support = AutoController::support(infos, &learned);
        infos
            .iter()
            .map(|i| {
                let mut d = ControlInfoDto::from(i);
                d.mode = mode_in(rec.as_ref(), i, support);
                let offer = match i.key {
                    ControlKey::Exposure => Some(support.exposure),
                    ControlKey::WhiteBalance => Some(support.white_balance),
                    _ => None,
                };
                if let Some(o) = offer {
                    d.app = match o {
                        Offer::Full => Some(AppOffer::Full),
                        Offer::Stops => Some(AppOffer::Stops),
                        Offer::No(_) => None,
                    };
                    d.app_why = o.why(i.key);
                }
                if i.key == ControlKey::Gain && d.mode == ControlMode::App {
                    d.driven_by = Some(ControlKey::Exposure);
                }
                d
            })
            .collect()
    }

    /// `set_control_mode(id, key, mode)`: who drives `key` (docs/AUTO-CONTROLS.md 5.3, section 3).
    ///
    /// * `app`: engage Better Cam auto (exposure and white balance only). The control is written manual at the
    ///   resume point (the stored value, else what the camera holds now for white balance and its default for
    ///   exposure; for exposure the gain follows at its own resume point), which switches the camera's Auto flag
    ///   off; what the control was before is kept for a hand-back.
    ///   Works on a lease too: the camera keeps the seed and the controller starts at the next open.
    /// * `manual`: the controller stops and the last driven values stay as manual ones, so the picture does not jump.
    /// * `camera`: the camera's Auto flag on; for exposure the gain stays at its last driven value, as manual.
    ///
    /// Returns the rows that changed (exposure together with gain).
    pub fn set_mode(
        &self,
        id: &str,
        key: ControlKey,
        mode: ControlMode,
    ) -> Result<Vec<ControlInfoDto>, String> {
        if key == ControlKey::Gain && mode == ControlMode::App {
            return Err("Gain is driven by Better Cam auto exposure".into());
        }
        if mode == ControlMode::App && !is_loop_key(key) {
            return Err(format!("Better Cam auto is not available for {}", key.label()));
        }
        let _gate = lock(&self.gate);
        let (was_app, resume, learned) = self.with_record(id, |r| {
            (
                r.is_app(key),
                Resume {
                    exposure: r.get(ControlKey::Exposure).and_then(|c| c.value),
                    gain: r.get(ControlKey::Gain).and_then(|c| c.value),
                    white_balance: r.get(ControlKey::WhiteBalance).and_then(|c| c.value),
                },
                r.learned.clone().unwrap_or_default(),
            )
        });
        if key == ControlKey::Gain && self.with_record(id, |r| r.is_app(ControlKey::Exposure) && r.is_app(key)) {
            return Err("Gain is set by Better Cam auto exposure".into());
        }
        /// What the closure did, for the record.
        struct Done {
            rows: Vec<ControlInfo>,
            /// Better Cam auto engaged: what the controls were, and the values written.
            engaged: Option<(Prior, Option<i32>, Option<i32>)>,
        }
        let result = self.run(id, |src, dev| -> Result<Result<Done, String>, ControlError> {
            let caps = dev.read_all()?;
            let find = |k: ControlKey| caps.iter().find(|c| c.key == k);
            let Some(info) = find(key) else {
                // A camera without the control says so in the words of 2.3.
                let why = (mode == ControlMode::App)
                    .then(|| {
                        let s = AutoController::support(&caps, &learned);
                        if key == ControlKey::Exposure {
                            s.exposure
                        } else {
                            s.white_balance
                        }
                        .why(key)
                    })
                    .flatten();
                return Ok(Err(why.unwrap_or_else(|| ControlError::NotSupported(key).to_string())));
            };
            let mut done = Done {
                rows: Vec::new(),
                engaged: None,
            };
            match mode {
                ControlMode::App if was_app => {}
                ControlMode::App => {
                    let support = AutoController::support(&caps, &learned);
                    let offer = if key == ControlKey::Exposure {
                        support.exposure
                    } else {
                        support.white_balance
                    };
                    if let Some(why) = offer.why(key) {
                        return Ok(Err(why));
                    }
                    let mut prior = Prior::default();
                    let e_seed;
                    let mut g_seed = None;
                    if key == ControlKey::Exposure {
                        prior.exposure = (!info.auto).then_some(info.value);
                        if let Some(g) = find(ControlKey::Gain).filter(|_| loop_uses_gain(&caps)) {
                            prior.gain = Some(g.value);
                            prior.gain_auto = g.auto;
                        }
                        let e = resume.exposure.unwrap_or(info.default);
                        let ie = dev.set_value(ControlKey::Exposure, e)?;
                        e_seed = ie.value;
                        if let Some(g) = find(ControlKey::Gain).filter(|_| loop_uses_gain(&caps)) {
                            let ig = dev.set_value(ControlKey::Gain, resume.gain.unwrap_or(g.min))?;
                            g_seed = Some(ig.value);
                        }
                    } else {
                        prior.white_balance = (!info.auto).then_some(info.value);
                        let iw = dev.set_value(ControlKey::WhiteBalance, resume.white_balance.unwrap_or(info.value))?;
                        e_seed = iw.value;
                    }
                    done.engaged = Some((prior, Some(e_seed), g_seed));
                }
                ControlMode::Manual if was_app => {}
                ControlMode::Manual => {
                    if key == ControlKey::Gain && !info.can_manual {
                        return Ok(Err(ControlError::ManualNotSupported(key).to_string()));
                    }
                    dev.set_auto(key, false)?;
                }
                ControlMode::Camera => {
                    dev.set_auto(key, true)?;
                }
            }
            for k in [ControlKey::Exposure, ControlKey::Gain, ControlKey::WhiteBalance] {
                let wanted = match key {
                    ControlKey::WhiteBalance => k == ControlKey::WhiteBalance,
                    ControlKey::Exposure | ControlKey::Gain => k != ControlKey::WhiteBalance,
                    _ => k == key,
                };
                if wanted {
                    if let Some(i) = dev.read(k)? {
                        if let Source::Engine(t) = src {
                            self.update_last(*t, &i);
                        }
                        done.rows.push(i);
                    }
                }
            }
            if !matches!(key, ControlKey::Exposure | ControlKey::Gain | ControlKey::WhiteBalance) {
                done.rows = dev.read(key)?.into_iter().collect();
            }
            Ok(Ok(done))
        })?;
        let done = result?;
        let main = done
            .rows
            .iter()
            .find(|i| i.key == key)
            .cloned()
            .ok_or_else(|| "the camera did not answer".to_string())?;
        match mode {
            ControlMode::App => {
                if let Some((prior, first, gain)) = done.engaged {
                    {
                        let mut p = lock(&self.prior);
                        if key == ControlKey::Exposure {
                            p.exposure = prior.exposure;
                            p.gain = prior.gain;
                            p.gain_auto = prior.gain_auto;
                        } else {
                            p.white_balance = prior.white_balance;
                        }
                    }
                    self.with_record(id, |r| {
                        if let Some(v) = first {
                            r.record_app(key, v);
                        }
                        if let Some(v) = gain {
                            r.record_app(ControlKey::Gain, v);
                        }
                    });
                }
            }
            ControlMode::Manual | ControlMode::Camera => {
                self.note(id, &main);
                if was_app {
                    if key == ControlKey::Exposure {
                        // The gain stays where the loop left it, as a manual value.
                        if let Some(g) = done.rows.iter().find(|i| i.key == ControlKey::Gain) {
                            self.note(id, g);
                        }
                    }
                    self.left_app(id, key);
                }
            }
        }
        self.bump(true);
        Ok(self.annotate(id, &done.rows))
    }

    /// `set_auto_config(id, cfg)`: the camera's Better Cam auto settings; live, the next decision uses them.
    pub fn set_auto_config(&self, id: &str, cfg: AutoCfg) {
        self.with_record(id, |r| r.auto = Some(cfg.sanitized()));
        self.bump(false);
    }

    /// `resume_auto(id, key)`: start again after a conflict, or clear `blocked` ("Try again" after a camera ignored
    /// Better Cam auto; the frontend then sets the mode again).
    pub fn resume_auto(&self, id: &str, key: ControlKey) -> Result<(), String> {
        let key = loop_of(key).ok_or_else(|| format!("{} has no Better Cam auto", key.label()))?;
        self.with_record(id, |r| {
            if let Some(l) = r.learned.as_mut() {
                l.blocked.retain(|k| *k != key);
            }
        });
        lock(&self.resumes).push(key);
        self.bump(true);
        Ok(())
    }

    /// The controller keeps what it learned with the camera's record (`auto-status` carries it for the frontend's
    /// store too).
    pub fn store_learned(&self, id: &str, learned: Learned) {
        self.with_record(id, |r| r.learned = Some(sanitize_learned(learned)));
    }

    /// The generation the controller compares with [`AutoSnapshot::generation`]: it moves with every mode,
    /// setting or restart request.
    pub fn auto_generation(&self) -> u64 {
        self.auto_gen.load(Ordering::Acquire)
    }

    /// The controls whose restart was asked for since the last call.
    pub fn take_resumes(&self) -> Vec<ControlKey> {
        std::mem::take(&mut *lock(&self.resumes))
    }

    /// The camera the engine has open, for the controller: its token (when it is `token`), link and the controls
    /// as last read.
    pub fn attached_caps(&self, token: u64) -> Option<(String, Vec<ControlInfo>)> {
        lock(&self.attached)
            .as_ref()
            .filter(|a| a.token == token)
            .map(|a| (a.link.clone(), a.last.clone()))
    }

    /// What the controller needs: the modes, settings and learned values of the record, for the engine's open
    /// camera (or, with none open, the record's).
    pub fn auto_snapshot(&self) -> AutoSnapshot {
        let generation = self.auto_generation();
        let attached = lock(&self.attached)
            .as_ref()
            .map(|a| (a.link.clone(), a.last.clone()));
        let rec = lock(&self.record).clone();
        let rec = rec.filter(|r| {
            attached
                .as_ref()
                .is_none_or(|(link, _)| same_camera(&r.device.symbolic_link, link))
        });
        let Some(rec) = rec else {
            return AutoSnapshot {
                generation,
                id: attached.map(|(l, _)| l).unwrap_or_default(),
                cfg: AutoCfg::default(),
                learned: Learned::default(),
                modes: Modes::default(),
            };
        };
        let caps: Vec<ControlInfo> = attached.map(|(_, c)| c).unwrap_or_default();
        let mode_of = |key: ControlKey| {
            if rec.is_app(key) {
                ControlMode::App
            } else {
                match rec.get(key).and_then(|c| c.auto) {
                    Some(true) => ControlMode::Camera,
                    Some(false) => ControlMode::Manual,
                    None if caps.iter().any(|i| i.key == key && !i.auto && i.can_manual) => ControlMode::Manual,
                    None => ControlMode::Camera,
                }
            }
        };
        let value = |key: ControlKey| rec.get(key).and_then(|c| c.value);
        AutoSnapshot {
            generation,
            id: rec.device.symbolic_link.clone(),
            cfg: rec.auto.unwrap_or_default(),
            learned: rec.learned.clone().unwrap_or_default(),
            modes: Modes {
                exposure: mode_of(ControlKey::Exposure),
                white_balance: mode_of(ControlKey::WhiteBalance),
                resume: Resume {
                    exposure: value(ControlKey::Exposure),
                    gain: value(ControlKey::Gain),
                    white_balance: value(ControlKey::WhiteBalance),
                },
                prior: *lock(&self.prior),
                seeded: false,
            },
        }
    }

    fn attached_device(&self, token: u64) -> Option<(String, Arc<dyn CameraControls>)> {
        lock(&self.attached)
            .as_ref()
            .filter(|a| a.token == token)
            .map(|a| (a.link.clone(), a.device.clone()))
    }

    /// Whether the controller may write `key` of the camera `link` now: the control is in Better Cam auto in the
    /// record. The gain only with the exposure.
    fn driven(&self, link: &str, key: ControlKey) -> bool {
        lock(&self.record)
            .as_ref()
            .filter(|r| same_camera(&r.device.symbolic_link, link))
            .is_some_and(|r| match key {
                ControlKey::Exposure | ControlKey::WhiteBalance => r.is_app(key),
                // The loop only writes the gain when it has one to use, and it follows the exposure.
                ControlKey::Gain => r.is_app(ControlKey::Exposure),
                _ => false,
            })
    }

    /// The controller's write (docs/AUTO-CONTROLS.md 5.3): a manual `value` for `key` of the attached camera
    /// `token`, only while the control's mode is Better Cam auto. Takes the write gate, so the user's writes, the
    /// re-apply and the loop are ordered; a user's mode change that came first makes this return
    /// [`DriveError::NotApp`] and the manual value stays the last driven one. Updates what the window was last
    /// told and the record's resume point; does not touch `prior`.
    pub fn drive(&self, token: u64, key: ControlKey, value: i32) -> Result<ControlInfo, DriveError> {
        let _gate = lock(&self.gate);
        let (link, device) = self.attached_device(token).ok_or(DriveError::NotAttached)?;
        if !self.driven(&link, key) {
            return Err(DriveError::NotApp);
        }
        let _call = Call::begin(&self.calls);
        let info = device.set_value(key, value).map_err(DriveError::Control)?;
        self.update_last(token, &info);
        if let Some(r) = lock(&self.record)
            .as_mut()
            .filter(|r| same_camera(&r.device.symbolic_link, &link))
        {
            r.record_app(key, info.value);
        }
        self.driven.store(true, Ordering::Release);
        Ok(info)
    }

    /// The watchdog's hand-back (docs/AUTO-CONTROLS.md 2.7): the control goes to the camera's own Auto or to a
    /// manual value, and the record follows (the control leaves Better Cam auto), under the write gate. For the
    /// exposure the gain is handed back with its own call.
    pub fn hand_back(&self, token: u64, key: ControlKey, to: HandBack) -> Result<ControlInfo, DriveError> {
        let _gate = lock(&self.gate);
        let (link, device) = self.attached_device(token).ok_or(DriveError::NotAttached)?;
        let in_app = lock(&self.record)
            .as_ref()
            .filter(|r| same_camera(&r.device.symbolic_link, &link))
            .is_some_and(|r| r.is_app(key) || (key == ControlKey::Gain && r.is_app(ControlKey::Exposure)));
        if !in_app {
            return Err(DriveError::NotApp);
        }
        let _call = Call::begin(&self.calls);
        let info = match to {
            HandBack::CameraAuto => device.set_auto(key, true),
            HandBack::Manual(v) => device.set_value(key, v),
        }
        .map_err(DriveError::Control)?;
        self.update_last(token, &info);
        self.note(&link, &info);
        self.bump(true);
        Ok(info)
    }

    /// The list for `controls-changed` when the controller wrote or a mode changed since the last call (the poller
    /// calls it at most four times a second, while the window is visible).
    pub fn take_driven(&self) -> Option<Vec<ControlInfoDto>> {
        if !self.driven.swap(false, Ordering::AcqRel) {
            return None;
        }
        let (link, last) = {
            let g = lock(&self.attached);
            let a = g.as_ref().filter(|a| !a.last.is_empty())?;
            (a.link.clone(), a.last.clone())
        };
        Some(self.annotate(&link, &last))
    }

    /// Read one control of the attached camera `token` (the controller's 1 Hz read-back), outside the write gate.
    /// `Ok(None)`: no such control, or the camera is not the open one any more.
    pub fn read_attached(&self, token: u64, key: ControlKey) -> Result<Option<ControlInfo>, ControlError> {
        let Some((_, device)) = self.attached_device(token) else {
            return Ok(None);
        };
        let _call = Call::begin(&self.calls);
        device.read(key)
    }

    // ---- the poller

    /// Ask for the next [`poll`](Self::poll) to return the list even when no control moved (the window is visible
    /// again after a time in which it was sent nothing).
    pub fn request_resend(&self) {
        self.resend.store(true, Ordering::Release);
    }

    /// Re-read the engine camera's controls that are on Auto. Returns the whole list, as `controls-changed` carries
    /// it, when a control that was read differs from what the window was last told (`last`: the last full read, the
    /// last write, the last poll that came back), or when a [`resend`](Self::request_resend) is due. `None` when
    /// nothing changed, no camera is open or nothing is on Auto. One control at a time, so a write that comes in
    /// waits for one read, not for all.
    pub fn poll(&self) -> Option<Vec<ControlInfoDto>> {
        let (token, device, auto_keys) = {
            let g = lock(&self.attached);
            let a = g.as_ref()?;
            let keys: Vec<ControlKey> = a.last.iter().filter(|i| i.auto).map(|i| i.key).collect();
            (a.token, a.device.clone(), keys)
        };
        let mut send = self.resend.swap(false, Ordering::AcqRel);
        for key in auto_keys {
            if let Ok(Some(info)) = device.read(key) {
                send |= self.update_last(token, &info);
            }
        }
        if !send {
            return None;
        }
        let (link, last) = {
            let g = lock(&self.attached);
            let a = g.as_ref().filter(|a| a.token == token && !a.last.is_empty())?;
            (a.link.clone(), a.last.clone())
        };
        // This list says everything the driven one would.
        self.driven.store(false, Ordering::Release);
        Some(self.annotate(&link, &last))
    }
}

/// Lets a re-apply stop at the next control once the camera is closing, and logs every write with its time (the
/// first write after an open takes 200-700 ms, later ones 2-27 ms; docs/AUTO-CONTROLS.md 9.2 check 9).
struct Cancellable<'a> {
    inner: &'a dyn CameraControls,
    cancel: &'a AtomicBool,
    /// When the re-apply began: the `t=` of its log lines.
    began: Instant,
}

impl Cancellable<'_> {
    fn check(&self) -> Result<(), ControlError> {
        if self.cancel.load(Ordering::Acquire) {
            Err(ControlError::Capture(CaptureError::Timeout(
                "the camera is closing",
            )))
        } else {
            Ok(())
        }
    }

    /// One `[apply]` line for a write that took `from..now`.
    fn log(&self, key: ControlKey, what: &str, result: &Result<ControlInfo, ControlError>, from: Instant) {
        let t = from.saturating_duration_since(self.began).as_secs_f64();
        let ms = from.elapsed().as_secs_f64() * 1000.0;
        eprintln!("{}", apply_line(t, key, what, result.as_ref().map(|i| i.value).map_err(ToString::to_string), ms));
    }
}

/// `[apply] t=0.4 exposure -5 -> -5 (412 ms)`: the same style as the runner's `[auto]` lines, `t` in seconds since the
/// re-apply began. `->` is what the device read back right after the write.
fn apply_line(t: f64, key: ControlKey, what: &str, result: Result<i32, String>, ms: f64) -> String {
    match result {
        Ok(back) => format!("[apply] t={t:.1} {key} {what} -> {back} ({ms:.0} ms)"),
        Err(e) => format!("[apply] t={t:.1} {key} {what} failed: {e} ({ms:.0} ms)"),
    }
}

impl ControlDevice for Cancellable<'_> {
    fn read(&self, key: ControlKey) -> Result<Option<ControlInfo>, ControlError> {
        self.check()?;
        self.inner.read(key)
    }
    fn set_auto(&self, key: ControlKey, auto: bool) -> Result<ControlInfo, ControlError> {
        self.check()?;
        let from = Instant::now();
        let r = self.inner.set_auto(key, auto);
        self.log(key, if auto { "auto on" } else { "auto off" }, &r, from);
        r
    }
    fn set_value(&self, key: ControlKey, value: i32) -> Result<ControlInfo, ControlError> {
        self.check()?;
        let from = Instant::now();
        let r = self.inner.set_value(key, value);
        self.log(key, &value.to_string(), &r, from);
        r
    }
}

#[cfg(test)]
pub(crate) mod fake {
    //! A camera with controls that lives in memory.
    use super::*;

    /// Behaves like a device: `set_value` clamps and switches to manual, `set_auto` flips the flag. Every call that
    /// writes is logged as `auto:<key>:<on>` or `value:<key>:<v>`.
    pub struct FakeControls {
        pub state: Mutex<Vec<ControlInfo>>,
        pub log: Mutex<Vec<String>>,
        pub fail: AtomicBool,
    }

    pub fn info(key: ControlKey, min: i32, max: i32, def: i32, can_auto: bool) -> ControlInfo {
        ControlInfo {
            key,
            min,
            max,
            step: 1,
            default: def,
            value: def,
            can_auto,
            can_manual: true,
            auto: can_auto,
            path: crate::capture::controls::AccessPath::Ks,
        }
    }

    /// Brightness, gain, exposure (Auto) and focus (Auto): enough to see the order.
    pub fn camera() -> Arc<FakeControls> {
        Arc::new(FakeControls {
            state: Mutex::new(vec![
                info(ControlKey::Brightness, -64, 64, 0, false),
                info(ControlKey::Gain, 0, 100, 10, false),
                info(ControlKey::Exposure, -13, -1, -6, true),
                info(ControlKey::Focus, 1, 1023, 500, true),
            ]),
            log: Mutex::new(Vec::new()),
            fail: AtomicBool::new(false),
        })
    }

    impl FakeControls {
        /// The camera was closed and opened: every control is back at its default, on Auto where it has Auto.
        pub fn forget(&self) {
            for i in lock(&self.state).iter_mut() {
                i.value = i.default;
                i.auto = i.can_auto;
            }
            lock(&self.log).clear();
        }
        pub fn log(&self) -> Vec<String> {
            lock(&self.log).clone()
        }
        pub fn clear_log(&self) {
            lock(&self.log).clear();
        }
        pub fn get(&self, key: ControlKey) -> ControlInfo {
            lock(&self.state)
                .iter()
                .find(|i| i.key == key)
                .unwrap()
                .clone()
        }
        fn broken(&self) -> Result<(), ControlError> {
            if self.fail.load(Ordering::SeqCst) {
                Err(ControlError::Capture(CaptureError::Timeout("fake failure")))
            } else {
                Ok(())
            }
        }
    }

    impl ControlDevice for FakeControls {
        fn read(&self, key: ControlKey) -> Result<Option<ControlInfo>, ControlError> {
            self.broken()?;
            Ok(lock(&self.state).iter().find(|i| i.key == key).cloned())
        }
        fn set_auto(&self, key: ControlKey, auto: bool) -> Result<ControlInfo, ControlError> {
            self.broken()?;
            let mut s = lock(&self.state);
            let i = s
                .iter_mut()
                .find(|i| i.key == key)
                .ok_or(ControlError::NotSupported(key))?;
            if i.auto == auto {
                return Ok(i.clone());
            }
            if auto && !i.can_auto {
                return Err(ControlError::AutoNotSupported(key));
            }
            i.auto = auto;
            lock(&self.log).push(format!("auto:{}:{auto}", key.key()));
            Ok(i.clone())
        }
        fn set_value(&self, key: ControlKey, value: i32) -> Result<ControlInfo, ControlError> {
            self.broken()?;
            let mut s = lock(&self.state);
            let i = s
                .iter_mut()
                .find(|i| i.key == key)
                .ok_or(ControlError::NotSupported(key))?;
            i.value = i.clamp(value);
            i.auto = false;
            lock(&self.log).push(format!("value:{}:{}", key.key(), i.value));
            Ok(i.clone())
        }
    }

    impl CameraControls for FakeControls {
        fn read_all(&self) -> Result<Vec<ControlInfo>, ControlError> {
            self.broken()?;
            Ok(lock(&self.state).clone())
        }
    }

    /// Opens leases on one fake camera and counts opens and closes.
    pub struct FakeOpener {
        pub camera: Arc<FakeControls>,
        pub link: String,
        pub opened: Arc<AtomicU64>,
        pub closed: Arc<AtomicU64>,
    }

    impl ControlsOpener for FakeOpener {
        fn key_of(&self, link: &str) -> Option<DeviceKey> {
            Some(DeviceKey::new(link, "W4GS"))
        }

        fn open(&self, id: &str) -> Result<Leased, String> {
            if !same_camera(&self.link, id) {
                return Err("camera not found".into());
            }
            self.opened.fetch_add(1, Ordering::SeqCst);
            let closed = self.closed.clone();
            Ok(Leased {
                link: self.link.clone(),
                device: self.camera.clone(),
                close: Box::new(move || {
                    closed.fetch_add(1, Ordering::SeqCst);
                }),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::*;
    use super::*;
    use serde_json::json;

    const LINK: &str = "\\\\?\\usb#vid_0c45&pid_636d&mi_00#9&3171a958&0&0000#{e5323777-f976-4f5b-9b55-b94699c46e44}\\global";
    const LINK_MOVED: &str = "\\\\?\\usb#vid_0c45&pid_636d&mi_00#6&bbbb&0&0001#{e5323777-f976-4f5b-9b55-b94699c46e44}\\global";
    const OTHER: &str = "\\\\?\\usb#vid_046d&pid_085e&mi_00#6&aaaa&0&0003#{e5323777-f976-4f5b-9b55-b94699c46e44}\\global";

    fn hub() -> (
        ControlsHub,
        Arc<FakeControls>,
        Arc<AtomicU64>,
        Arc<AtomicU64>,
    ) {
        let camera = camera();
        let (opened, closed) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
        let hub = ControlsHub::new(Arc::new(FakeOpener {
            camera: camera.clone(),
            link: LINK.into(),
            opened: opened.clone(),
            closed: closed.clone(),
        }));
        (hub, camera, opened, closed)
    }

    #[test]
    fn apply_lines_have_the_style_of_the_auto_lines() {
        assert_eq!(
            apply_line(0.4, ControlKey::Exposure, "-5", Ok(-5), 412.4),
            "[apply] t=0.4 exposure -5 -> -5 (412 ms)"
        );
        assert_eq!(
            apply_line(1.0, ControlKey::Gain, "auto on", Err("nope".into()), 3.0),
            "[apply] t=1.0 gain auto on failed: nope (3 ms)"
        );
    }

    fn record(entries: Value) -> ControlSnapshot {
        parse_record(Some(
            &json!({ "id": LINK, "name": "W4GS", "controls": entries }),
        ))
        .unwrap()
        .unwrap()
    }

    #[test]
    fn the_record_arrives_as_the_frontend_stores_it() {
        let r = record(json!({
            "gain": { "value": 40, "auto": false },
            "focus": { "value": 300, "auto": true },
            "brightness": { "value": 12 },
            "nonsense": { "value": 1 },
            "contrast": {}
        }));
        assert_eq!(r.device.name, "W4GS");
        assert_eq!(r.device.vid_pid.as_deref(), Some("vid_0c45&pid_636d"));
        // in the order of plan 6.2, unknown and empty entries gone, a control on Auto keeps no value
        let keys: Vec<_> = r.controls.iter().map(|c| c.key).collect();
        assert_eq!(
            keys,
            [ControlKey::Brightness, ControlKey::Gain, ControlKey::Focus]
        );
        assert_eq!(r.get(ControlKey::Focus).unwrap().value, None);
        assert_eq!(r.get(ControlKey::Gain).unwrap().auto, Some(false));
        assert_eq!(r.get(ControlKey::Brightness).unwrap().auto, None);

        assert_eq!(parse_record(None).unwrap(), None);
        assert_eq!(parse_record(Some(&Value::Null)).unwrap(), None);
        assert!(parse_record(Some(&json!({ "name": "no id" }))).is_err());
        assert!(parse_record(Some(&json!("text"))).is_err());
    }

    #[test]
    fn a_record_follows_its_camera_to_another_port() {
        let rec = DeviceKey::new(LINK, "W4GS");
        // same link (also written differently): no lookup at all
        assert_eq!(
            record_applies(&rec, &LINK.to_uppercase(), || panic!("no lookup needed")),
            Some(KeyMatch::Exact)
        );
        // another port: the same vid/pid and name
        let moved = DeviceKey::new(LINK_MOVED, "w4gs");
        assert_eq!(
            record_applies(&rec, LINK_MOVED, || Some(moved.clone())),
            Some(KeyMatch::Fallback)
        );
        // the same vid/pid but another name is another camera
        let other_name = DeviceKey::new(LINK_MOVED, "Other cam");
        assert_eq!(record_applies(&rec, LINK_MOVED, || Some(other_name)), None);
        // another model: no lookup
        assert_eq!(
            record_applies(&rec, OTHER, || panic!("no lookup needed")),
            None
        );
        // a camera that cannot be found
        assert_eq!(record_applies(&rec, LINK_MOVED, || None), None);

        assert!(same_camera(LINK, LINK_MOVED));
        assert!(!same_camera(LINK, OTHER));
    }

    #[test]
    fn a_reopen_applies_auto_flags_then_values() {
        let (hub, cam, ..) = hub();
        hub.set_record(Some(record(json!({
            "gain": { "value": 40, "auto": false },
            "exposure": { "value": -3, "auto": false },
            "focus": { "value": 0, "auto": true },
            "brightness": { "value": 25, "auto": false }
        }))));
        // the device forgot everything: exposure is on Auto, focus on Auto
        let dev: Arc<dyn CameraControls> = cam.clone();
        let cancel = AtomicBool::new(false);
        let token = hub.attach(LINK, dev.clone());
        let (report, infos) = hub.apply_on_open(token, LINK, &dev, &cancel).unwrap();
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        // Auto flags first (in plan order), then the manual values (in plan order)
        assert_eq!(
            cam.log(),
            [
                "auto:exposure:false",
                "value:brightness:25",
                "value:gain:40",
                "value:exposure:-3"
            ]
        );
        assert_eq!(cam.get(ControlKey::Exposure).value, -3);
        assert!(cam.get(ControlKey::Focus).auto);
        // the state read afterwards is what the event carries
        assert_eq!(infos.len(), 4);
        assert_eq!(
            infos
                .iter()
                .find(|i| i.key == ControlKey::Gain)
                .unwrap()
                .value,
            40
        );

        // a record for another camera is not applied
        cam.clear_log();
        hub.set_record(Some(
            parse_record(Some(&json!({"id": OTHER, "name": "Other", "controls": {"gain": {"value": 99, "auto": false}}})))
                .unwrap()
                .unwrap(),
        ));
        hub.apply_on_open(token, LINK, &dev, &cancel).unwrap();
        assert!(cam.log().is_empty());
        // a closing camera stops the writes
        hub.set_record(Some(record(
            json!({ "gain": { "value": 7, "auto": false } }),
        )));
        cancel.store(true, Ordering::SeqCst);
        let (report, _) = hub.apply_on_open(token, LINK, &dev, &cancel).unwrap();
        assert!(cam.log().is_empty());
        assert_eq!(report.skipped.len(), 1);
    }

    #[test]
    fn a_record_applies_after_the_camera_moved_to_another_port() {
        let (hub, cam, ..) = hub();
        hub.set_record(Some(record(
            json!({ "gain": { "value": 40, "auto": false } }),
        )));
        let dev: Arc<dyn CameraControls> = cam.clone();
        let token = hub.attach(LINK_MOVED, dev.clone());
        hub.apply_on_open(token, LINK_MOVED, &dev, &AtomicBool::new(false))
            .unwrap();
        assert_eq!(cam.log(), ["value:gain:40"]);
        // the record names the new link from now on
        assert_eq!(hub.record().unwrap().device.symbolic_link, LINK_MOVED);
    }

    #[test]
    fn set_writes_and_records_what_differs_from_the_default() {
        let (hub, cam, ..) = hub();
        hub.set_record(Some(record(json!({}))));

        let r = hub.set(LINK, "brightness", 30, false).unwrap();
        assert_eq!(
            (r.key, r.value, r.auto),
            (ControlKey::Brightness, 30, false)
        );
        // values are clamped by the device
        assert_eq!(hub.set(LINK, "brightness", 1000, false).unwrap().value, 64);
        // exposure to manual with a value
        let e = hub.set(LINK, "exposure", -4, false).unwrap();
        assert!(!e.auto && e.value == -4 && e.can_auto);
        // Auto on for focus changes nothing in the record (it is the default state)
        hub.set(LINK, "focus", 0, true).unwrap();

        let rec = hub.record().unwrap();
        assert_eq!(rec.get(ControlKey::Brightness).unwrap().value, Some(64));
        assert_eq!(rec.get(ControlKey::Brightness).unwrap().auto, None);
        let ex = rec.get(ControlKey::Exposure).unwrap();
        assert_eq!((ex.auto, ex.value), (Some(false), Some(-4)));
        assert!(rec.get(ControlKey::Focus).is_none());

        // back to the device's own state: forgotten
        hub.set(LINK, "brightness", 0, false).unwrap();
        hub.set(LINK, "exposure", 0, true).unwrap();
        let rec = hub.record().unwrap();
        assert!(rec.get(ControlKey::Brightness).is_none());
        assert_eq!(
            rec.get(ControlKey::Exposure).and_then(|c| c.auto),
            None,
            "Auto on is the device default"
        );

        // errors are strings for the frontend
        assert!(hub
            .set(LINK, "pan", 0, false)
            .unwrap_err()
            .contains("not supported"));
        assert!(hub
            .set(LINK, "nonsense", 0, false)
            .unwrap_err()
            .contains("unknown control"));
        assert!(hub.set(LINK, "gain", 0, true).unwrap_err().contains("Auto"));
        assert!(hub.set(OTHER, "gain", 5, false).is_err());
        let _ = cam;
    }

    #[test]
    fn reset_returns_the_device_defaults_and_clears_the_record() {
        let (hub, cam, ..) = hub();
        hub.set_record(Some(record(json!({}))));
        hub.set(LINK, "brightness", 50, false).unwrap();
        hub.set(LINK, "gain", 70, false).unwrap();
        hub.set(LINK, "exposure", -2, false).unwrap();
        hub.set(LINK, "focus", 900, false).unwrap();
        assert_eq!(hub.record().unwrap().controls.len(), 4);

        let all = hub.reset(LINK).unwrap();
        assert_eq!(all.len(), 4);
        for i in &all {
            let c = cam.get(i.key);
            assert_eq!(i.value, c.value);
            assert_eq!(i.auto, c.can_auto, "{} is on Auto where it has Auto", i.key);
            if !i.auto {
                assert_eq!(i.value, i.def, "{}", i.key);
            }
        }
        // Auto controls had their default value written first, so the manual fallback is the default too
        assert_eq!(cam.get(ControlKey::Exposure).value, -6);
        assert_eq!(cam.get(ControlKey::Focus).value, 500);
        assert!(
            hub.record().unwrap().is_empty(),
            "the stored record is empty"
        );
        // the camera is at its defaults: a second reset writes nothing
        cam.clear_log();
        hub.reset(LINK).unwrap();
        assert!(cam.log().is_empty());
    }

    #[test]
    fn the_camera_is_opened_once_for_a_run_of_calls_and_closed_when_idle() {
        let (hub, _cam, opened, closed) = hub();
        for v in 0..10 {
            hub.set(LINK, "gain", v, false).unwrap();
        }
        assert_eq!(opened.load(Ordering::SeqCst), 1, "one open for ten writes");
        assert!(hub.has_lease());
        hub.expire_lease();
        assert!(hub.has_lease(), "not idle yet");
        // the engine wants the camera
        hub.release_lease();
        assert!(!hub.has_lease());
        assert_eq!(closed.load(Ordering::SeqCst), 1);
        // and the next call opens it again
        hub.get(LINK).unwrap();
        assert_eq!(opened.load(Ordering::SeqCst), 2);
        // an unknown camera is an error, not a panic
        assert!(hub.get(OTHER).is_err());
        assert!(
            !hub.has_lease(),
            "a camera that cannot be opened leaves no lease"
        );
    }

    #[test]
    fn an_idle_lease_closes_but_not_during_a_call() {
        let camera = camera();
        let closed = Arc::new(AtomicU64::new(0));
        let hub = ControlsHub::new(Arc::new(FakeOpener {
            camera,
            link: LINK.into(),
            opened: Default::default(),
            closed: closed.clone(),
        }))
        .with_lease_idle(Duration::from_millis(30));
        hub.get(LINK).unwrap();
        // a long call (Reset all with slow first writes) is running: the lease stays
        let call = Call::begin(&hub.calls);
        std::thread::sleep(Duration::from_millis(60));
        hub.expire_lease();
        assert!(hub.has_lease());
        assert_eq!(closed.load(Ordering::SeqCst), 0);
        drop(call);
        // the idle time counts from the end of the last call, then the lease closes
        std::thread::sleep(Duration::from_millis(60));
        hub.expire_lease();
        assert!(!hub.has_lease());
        assert_eq!(closed.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn calls_use_the_engines_camera_while_it_is_open() {
        let (hub, cam, opened, _closed) = hub();
        let dev: Arc<dyn CameraControls> = cam.clone();
        let token = hub.attach(LINK_MOVED, dev.clone());
        hub.set(LINK, "gain", 33, false).unwrap();
        assert_eq!(
            opened.load(Ordering::SeqCst),
            0,
            "no second open of the camera"
        );
        // the poller sees what is on Auto after a full read
        assert!(hub.poll().is_none(), "nothing read yet");
        hub.get(LINK).unwrap();
        // nothing moved since the full read, which the window got: nothing to send
        assert!(hub.poll().is_none(), "an unchanged poll sends nothing");
        // a window that was away asks for the list once, changed or not
        hub.request_resend();
        let polled = hub.poll().expect("exposure and focus are on Auto");
        assert_eq!(polled.len(), 4);
        assert!(hub.poll().is_none(), "the resend is one-off");
        // the device moves the lens: the poll sees it
        lock(&cam.state)
            .iter_mut()
            .find(|i| i.key == ControlKey::Focus)
            .unwrap()
            .value = 321;
        let polled = hub.poll().unwrap();
        assert_eq!(
            polled
                .iter()
                .find(|i| i.key == ControlKey::Focus)
                .unwrap()
                .value,
            321
        );
        assert!(hub.poll().is_none(), "the move was sent once");
        // nothing on Auto: nothing to poll
        hub.set(LINK, "focus", 321, false).unwrap();
        hub.set(LINK, "exposure", -5, false).unwrap();
        assert!(hub.poll().is_none());
        hub.detach(token);
        assert!(hub.poll().is_none());
        // after the camera closes the calls go to a lease again
        hub.set(LINK, "gain", 1, false).unwrap();
        assert_eq!(opened.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_device_error_reaches_the_caller_and_drops_the_lease() {
        let (hub, cam, _opened, closed) = hub();
        hub.get(LINK).unwrap();
        cam.fail.store(true, Ordering::SeqCst);
        let err = hub.set(LINK, "gain", 5, false).unwrap_err();
        assert!(!err.is_empty());
        assert!(!hub.has_lease());
        assert!(closed.load(Ordering::SeqCst) >= 1);
    }

    #[test]
    fn the_payload_has_the_shape_of_plan_5_5() {
        let i = fake::info(ControlKey::WhiteBalance, 2800, 6500, 4600, true);
        let v = serde_json::to_value(ControlInfoDto::from(&i)).unwrap();
        assert_eq!(
            v,
            json!({
                "key": "white_balance", "min": 2800, "max": 6500, "step": 1, "def": 4600,
                "value": 4600, "auto": true, "can_auto": true, "can_manual": true,
                "mode": "camera", "app": null
            })
        );
        let list = serde_json::to_value(dtos(&[i])).unwrap();
        assert!(list.as_array().is_some_and(|a| a.len() == 1));
    }

    // ---- Better Cam auto (docs/AUTO-CONTROLS.md 5.1-5.3, 6.2 "Hub integration") ---------------------------------

    use std::sync::atomic::AtomicBool as Flag;

    /// A hub whose engine camera is attached and applied, with the record `entries` for it.
    fn live(entries: Value) -> (ControlsHub, Arc<FakeControls>, u64) {
        let (hub, cam, ..) = hub();
        hub.set_record(Some(record(entries)));
        let dev: Arc<dyn CameraControls> = cam.clone();
        let token = hub.attach(LINK, dev.clone());
        hub.apply_on_open(token, LINK, &dev, &Flag::new(false)).unwrap();
        cam.clear_log();
        (hub, cam, token)
    }

    fn row(rows: &[ControlInfoDto], key: ControlKey) -> &ControlInfoDto {
        rows.iter().find(|r| r.key == key).unwrap()
    }

    #[test]
    fn better_cam_auto_engages_through_the_gate_and_the_events_carry_the_mode() {
        let (hub, cam, token) = live(json!({}));
        assert!(hub.applied(token));
        assert!(!hub.applied(token + 1));

        let rows = hub.set_mode(LINK, ControlKey::Exposure, ControlMode::App).unwrap();
        // exposure and gain together, written manual at the resume point (the camera's default, and the gain's minimum)
        assert_eq!(cam.log(), ["value:exposure:-6", "value:gain:0"]);
        assert!(!cam.get(ControlKey::Exposure).auto);
        let (e, g) = (row(&rows, ControlKey::Exposure), row(&rows, ControlKey::Gain));
        assert_eq!((e.mode, g.mode), (ControlMode::App, ControlMode::App));
        assert_eq!(e.app, Some(AppOffer::Full));
        assert_eq!(g.driven_by, Some(ControlKey::Exposure));
        assert!(!e.auto, "the camera's Auto flag is off");
        // what the controls were before, for a hand-back: exposure was on Camera auto, gain manual at 10
        let snap = hub.auto_snapshot();
        assert_eq!(snap.modes.exposure, ControlMode::App);
        assert_eq!(snap.modes.prior.exposure, None);
        assert_eq!(snap.modes.prior.gain, Some(10));
        assert_eq!(snap.modes.resume.exposure, Some(-6));

        // the loop's writes: the record's resume point follows, and the window is told
        let generation = hub.auto_generation();
        assert!(hub.take_driven().is_some(), "the mode change is sent");
        assert!(hub.take_driven().is_none());
        assert_eq!(hub.drive(token, ControlKey::Exposure, -5).unwrap().value, -5);
        assert_eq!(hub.drive(token, ControlKey::Gain, 48).unwrap().value, 48);
        assert_eq!(hub.auto_generation(), generation, "a drive is not a change for the controller");
        let rec = hub.record().unwrap();
        assert_eq!(rec.get(ControlKey::Exposure).unwrap().value, Some(-5));
        assert_eq!(rec.get(ControlKey::Gain).unwrap().value, Some(48));
        let sent = hub.take_driven().expect("the writes are sent");
        assert_eq!(row(&sent, ControlKey::Exposure).mode, ControlMode::App);
        assert_eq!(row(&sent, ControlKey::Exposure).value, -5);
        assert_eq!(row(&sent, ControlKey::Gain).value, 48);
        assert!(hub.take_driven().is_none());
        assert_eq!(hub.auto_snapshot().modes.resume.gain, Some(48));

        // only the controls in Better Cam auto, only the open camera
        assert!(matches!(hub.drive(token, ControlKey::Brightness, 3), Err(DriveError::NotApp)));
        assert!(matches!(hub.drive(token + 1, ControlKey::Exposure, -4), Err(DriveError::NotAttached)));
        assert_eq!(cam.get(ControlKey::Exposure).value, -5);
        // the serialised event has the mode
        let v = serde_json::to_value(&sent).unwrap();
        assert!(v.as_array().unwrap().iter().any(|r| r["key"] == "exposure" && r["mode"] == "app" && r["app"] == "full"));
        assert!(v.as_array().unwrap().iter().any(|r| r["key"] == "gain" && r["driven_by"] == "exposure"));
    }

    #[test]
    fn a_users_manual_mode_wins_and_keeps_the_last_driven_value() {
        let (hub, cam, token) = live(json!({}));
        hub.set_mode(LINK, ControlKey::Exposure, ControlMode::App).unwrap();
        hub.drive(token, ControlKey::Exposure, -5).unwrap();
        hub.drive(token, ControlKey::Gain, 48).unwrap();
        cam.clear_log();

        // the user's mode change comes between two writes of the loop
        let rows = hub.set_mode(LINK, ControlKey::Exposure, ControlMode::Manual).unwrap();
        assert!(cam.log().is_empty(), "no jump in the picture: {:?}", cam.log());
        assert!(matches!(hub.drive(token, ControlKey::Exposure, -3), Err(DriveError::NotApp)));
        assert!(matches!(hub.drive(token, ControlKey::Gain, 90), Err(DriveError::NotApp)));
        assert_eq!((cam.get(ControlKey::Exposure).value, cam.get(ControlKey::Gain).value), (-5, 48));
        assert!(cam.log().is_empty());
        assert_eq!(row(&rows, ControlKey::Exposure).mode, ControlMode::Manual);
        assert_eq!(row(&rows, ControlKey::Gain).mode, ControlMode::Manual);
        let rec = hub.record().unwrap();
        let (e, g) = (rec.get(ControlKey::Exposure).unwrap(), rec.get(ControlKey::Gain).unwrap());
        assert_eq!((e.app, e.auto, e.value), (false, Some(false), Some(-5)));
        assert_eq!((g.app, g.auto, g.value), (false, Some(false), Some(48)));
        assert_eq!(hub.auto_snapshot().modes.exposure, ControlMode::Manual);

        // the slider of the gain works again
        assert_eq!(hub.set(LINK, "gain", 20, false).unwrap().value, 20);
    }

    #[test]
    fn camera_auto_takes_the_exposure_back_and_leaves_the_gain_manual() {
        let (hub, cam, token) = live(json!({}));
        hub.set_mode(LINK, ControlKey::Exposure, ControlMode::App).unwrap();
        hub.drive(token, ControlKey::Exposure, -4).unwrap();
        hub.drive(token, ControlKey::Gain, 30).unwrap();
        let rows = hub.set_mode(LINK, ControlKey::Exposure, ControlMode::Camera).unwrap();
        assert!(cam.get(ControlKey::Exposure).auto);
        assert_eq!(row(&rows, ControlKey::Exposure).mode, ControlMode::Camera);
        assert_eq!(row(&rows, ControlKey::Gain).mode, ControlMode::Manual);
        assert_eq!(cam.get(ControlKey::Gain).value, 30);
        let rec = hub.record().unwrap();
        assert!(rec.get(ControlKey::Exposure).is_none(), "the camera's own Auto is the default state");
        assert_eq!(rec.get(ControlKey::Gain).unwrap().value, Some(30));
        assert!(!rec.get(ControlKey::Gain).unwrap().app);
        assert!(matches!(hub.drive(token, ControlKey::Exposure, -4), Err(DriveError::NotApp)));
    }

    #[test]
    fn a_user_write_to_a_driven_control_ends_better_cam_auto_and_the_gain_is_not_set_by_hand() {
        let (hub, cam, token) = live(json!({}));
        hub.set_mode(LINK, ControlKey::Exposure, ControlMode::App).unwrap();
        hub.drive(token, ControlKey::Gain, 44).unwrap();
        assert!(hub.set(LINK, "gain", 5, false).unwrap_err().contains("Better Cam auto"));
        assert_eq!(cam.get(ControlKey::Gain).value, 44);
        let e = hub.set(LINK, "exposure", -9, false).unwrap();
        assert_eq!((e.mode, e.value), (ControlMode::Manual, -9));
        assert!(matches!(hub.drive(token, ControlKey::Exposure, -3), Err(DriveError::NotApp)));
        assert!(!hub.record().unwrap().is_app(ControlKey::Gain));
        // Reset all ends every mode and keeps the settings and what was learned
        hub.set_auto_config(LINK, AutoCfg::default());
        hub.store_learned(LINK, Learned { gain_stops: Some(2.5), ..Learned::default() });
        hub.set_mode(LINK, ControlKey::Exposure, ControlMode::App).unwrap();
        hub.reset(LINK).unwrap();
        let rec = hub.record().unwrap();
        assert!(rec.is_empty());
        assert!(rec.auto.is_some());
        assert_eq!(rec.learned.as_ref().unwrap().gain_stops, Some(2.5));
        assert!(matches!(hub.drive(token, ControlKey::Exposure, -3), Err(DriveError::NotApp)));
        assert!(cam.get(ControlKey::Exposure).auto);
    }

    #[test]
    fn a_reopen_writes_the_resume_point_before_any_drive() {
        let (hub, cam, ..) = hub();
        hub.set_record(Some(record(json!({
            "exposure": { "value": -5, "auto": false, "mode": "app" },
            "gain": { "value": 48, "mode": "app" }
        }))));
        let rec = hub.record().unwrap();
        assert!(rec.is_app(ControlKey::Exposure) && rec.is_app(ControlKey::Gain));
        // the camera forgot everything: exposure is on Auto, gain at its default
        let dev: Arc<dyn CameraControls> = cam.clone();
        let token = hub.attach(LINK, dev.clone());
        assert!(!hub.applied(token), "not before the stored values are written");
        let (report, infos) = hub.apply_on_open(token, LINK, &dev, &Flag::new(false)).unwrap();
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        assert_eq!(cam.log(), ["auto:exposure:false", "value:exposure:-5", "value:gain:48"]);
        assert!(hub.applied(token));
        // the list the window gets says who drives
        assert_eq!(row(&infos, ControlKey::Exposure).mode, ControlMode::App);
        assert_eq!(row(&infos, ControlKey::Gain).mode, ControlMode::App);
        // and only now does a drive go through
        assert_eq!(hub.drive(token, ControlKey::Exposure, -4).unwrap().value, -4);
        assert_eq!(cam.log().last().unwrap(), "value:exposure:-4");
        let snap = hub.auto_snapshot();
        assert_eq!(snap.modes.exposure, ControlMode::App);
        assert_eq!(snap.modes.resume.exposure, Some(-4));
        assert_eq!(snap.modes.resume.gain, Some(48));
    }

    #[test]
    fn a_reopen_leaves_a_loop_the_camera_does_not_offer_on_its_own_auto() {
        let (hub, cam, ..) = hub();
        // white balance in units that are not Kelvin (a 0..100 slider): no loop runs, so writing the seed would freeze it
        lock(&cam.state).push(fake::info(ControlKey::WhiteBalance, 0, 100, 50, true));
        hub.set_record(Some(record(json!({
            "white_balance": { "auto": false, "mode": "app" },
            "exposure": { "value": -5, "auto": false, "mode": "app" }
        }))));
        let dev: Arc<dyn CameraControls> = cam.clone();
        let token = hub.attach(LINK, dev.clone());
        let (report, infos) = hub.apply_on_open(token, LINK, &dev, &Flag::new(false)).unwrap();
        // the exposure (offered) is written as always; the white balance is not touched
        assert_eq!(cam.log(), ["auto:exposure:false", "value:exposure:-5"]);
        assert!(cam.get(ControlKey::WhiteBalance).auto, "the camera's own Auto stays on");
        assert_eq!(report.skipped, [(ControlKey::WhiteBalance, SkipReason::NotOffered)]);
        assert_eq!(row(&infos, ControlKey::WhiteBalance).mode, ControlMode::Camera);
        assert_eq!(row(&infos, ControlKey::WhiteBalance).app, None);
        assert_eq!(row(&infos, ControlKey::Exposure).mode, ControlMode::App);
    }

    #[test]
    fn a_reopen_puts_white_balance_in_better_cam_auto_without_a_stored_value() {
        let (hub, cam, ..) = hub();
        lock(&cam.state).push(fake::info(ControlKey::WhiteBalance, 2800, 6500, 4600, true));
        hub.set_record(Some(record(json!({ "white_balance": { "auto": false, "mode": "app" } }))));
        assert_eq!(hub.record().unwrap().get(ControlKey::WhiteBalance).map(|c| (c.app, c.value)), Some((true, None)));
        let dev: Arc<dyn CameraControls> = cam.clone();
        let token = hub.attach(LINK, dev.clone());
        let (report, infos) = hub.apply_on_open(token, LINK, &dev, &Flag::new(false)).unwrap();
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        // Auto off, no value written: the loop starts from what the camera holds
        assert_eq!(cam.log(), ["auto:white_balance:false"]);
        assert_eq!(cam.get(ControlKey::WhiteBalance).value, 4600);
        assert_eq!(row(&infos, ControlKey::WhiteBalance).mode, ControlMode::App);
    }

    #[test]
    fn a_lease_stores_the_mode_and_the_seed_and_no_loop_can_drive() {
        let (hub, cam, ..) = hub();
        hub.set_record(Some(record(json!({
            "exposure": { "value": -7, "auto": false },
        }))));
        // the camera is closed: the lease writes the seed (the stored value, gain at its minimum)
        let rows = hub.set_mode(LINK, ControlKey::Exposure, ControlMode::App).unwrap();
        assert!(hub.has_lease());
        assert_eq!(cam.log(), ["value:exposure:-7", "value:gain:0"]);
        assert_eq!(row(&rows, ControlKey::Exposure).mode, ControlMode::App);
        let rec = hub.record().unwrap();
        assert_eq!(rec.get(ControlKey::Exposure).map(|c| (c.app, c.value)), Some((true, Some(-7))));
        assert!(rec.is_app(ControlKey::Gain));
        // nothing is attached: the controller has nothing to drive
        assert!(matches!(hub.drive(1, ControlKey::Exposure, -5), Err(DriveError::NotAttached)));
        assert!(hub.take_driven().is_none(), "no camera, no list");
        assert_eq!(hub.auto_snapshot().modes.exposure, ControlMode::App);
        // an unknown mode and controls that have none are errors, not writes
        cam.clear_log();
        assert!(hub.set_mode(LINK, ControlKey::Gain, ControlMode::App).is_err());
        assert!(hub.set_mode(LINK, ControlKey::Focus, ControlMode::App).is_err());
        assert!(
            hub.set_mode(LINK, ControlKey::WhiteBalance, ControlMode::App)
                .unwrap_err()
                .contains("no manual white balance")
        );
        assert!(cam.log().is_empty());
    }

    #[test]
    fn the_watchdogs_hand_back_goes_through_the_gate_and_ends_the_mode() {
        let (hub, cam, token) = live(json!({}));
        hub.set_mode(LINK, ControlKey::Exposure, ControlMode::App).unwrap();
        hub.drive(token, ControlKey::Exposure, -4).unwrap();
        hub.drive(token, ControlKey::Gain, 30).unwrap();
        let generation = hub.auto_generation();
        let e = hub.hand_back(token, ControlKey::Exposure, HandBack::CameraAuto).unwrap();
        assert!(e.auto);
        assert!(hub.auto_generation() > generation, "the controller looks at the hub again");
        // the gain goes to what it was before
        let g = hub.hand_back(token, ControlKey::Gain, HandBack::Manual(10)).unwrap();
        assert_eq!((g.value, g.auto), (10, false));
        let snap = hub.auto_snapshot();
        assert_eq!(snap.modes.exposure, ControlMode::Camera);
        assert!(matches!(hub.drive(token, ControlKey::Exposure, -3), Err(DriveError::NotApp)));
        assert!(matches!(
            hub.hand_back(token, ControlKey::Exposure, HandBack::CameraAuto),
            Err(DriveError::NotApp)
        ));
        let rows = hub.take_driven().unwrap();
        assert_eq!(row(&rows, ControlKey::Exposure).mode, ControlMode::Camera);
        assert_eq!(row(&rows, ControlKey::Gain).mode, ControlMode::Manual);
        let _ = cam;
    }

    #[test]
    fn a_camera_that_ignored_better_cam_auto_is_not_offered_until_try_again() {
        let (hub, _cam, _token) = live(json!({}));
        hub.store_learned(LINK, Learned { blocked: vec![ControlKey::Exposure], ..Learned::default() });
        let rows = hub.get(LINK).unwrap();
        let e = row(&rows, ControlKey::Exposure);
        assert_eq!(e.app, None);
        assert!(e.app_why.as_deref().unwrap().contains("ignored"));
        assert!(hub
            .set_mode(LINK, ControlKey::Exposure, ControlMode::App)
            .unwrap_err()
            .contains("ignored"));
        // "Try again": clears blocked and tells the controller
        let generation = hub.auto_generation();
        hub.resume_auto(LINK, ControlKey::Exposure).unwrap();
        assert!(hub.auto_generation() > generation);
        assert_eq!(hub.take_resumes(), [ControlKey::Exposure]);
        assert!(hub.take_resumes().is_empty());
        let e = hub.get(LINK).unwrap();
        assert_eq!(row(&e, ControlKey::Exposure).app, Some(AppOffer::Full));
        assert!(hub.set_mode(LINK, ControlKey::Exposure, ControlMode::App).is_ok());
        assert!(hub.resume_auto(LINK, ControlKey::Brightness).is_err());
    }

    #[test]
    fn a_camera_without_a_usable_gain_works_in_whole_stops() {
        let infos = vec![
            fake::info(ControlKey::Exposure, -13, -1, -6, true),
            fake::info(ControlKey::Gain, 0, 3, 0, false),
            fake::info(ControlKey::WhiteBalance, 0, 100, 50, true),
        ];
        let support = AutoController::support(&infos, &Learned::default());
        assert_eq!(support.exposure, Offer::Stops);
        let (hub, ..) = hub();
        let rows = hub.annotate(LINK, &infos);
        assert_eq!(row(&rows, ControlKey::Exposure).app, Some(AppOffer::Stops));
        // white balance 0..100 is not Kelvin
        let wb = row(&rows, ControlKey::WhiteBalance);
        assert_eq!(wb.app, None);
        assert!(wb.app_why.as_deref().unwrap().contains("units"));
        assert_eq!(row(&rows, ControlKey::Gain).app_why, None);
    }

    #[test]
    fn parse_record_reads_the_better_cam_auto_fields_and_clamps_them() {
        let r = parse_record(Some(&json!({
            "id": LINK, "name": "W4GS",
            "controls": {
                "exposure": { "value": -5, "auto": true, "mode": "app" },
                "gain": { "value": 48, "mode": "app" },
                "white_balance": { "value": 4650, "mode": "app" },
                "brightness": { "value": 3, "mode": "app" },
                "focus": { "value": 0, "auto": true }
            },
            "auto": { "exposure": { "target": 5, "longest": -4, "max_gain": 60, "speed": "fast" },
                      "white_balance": { "warmth": -3 } },
            "learned": { "gain_stops": 99, "wb_alpha": 0.0001, "rate_caps": { "30": -6, "x": 2 },
                         "blocked": ["white_balance", "brightness", "white_balance"] }
        })))
        .unwrap()
        .unwrap();
        let e = r.get(ControlKey::Exposure).unwrap();
        assert!(e.app);
        assert_eq!((e.auto, e.value), (Some(false), Some(-5)), "Better Cam auto is a manual value on the device");
        assert!(r.is_app(ControlKey::Gain) && r.is_app(ControlKey::WhiteBalance));
        assert!(!r.is_app(ControlKey::Brightness), "only exposure, white balance and gain have the mode");
        assert_eq!(r.get(ControlKey::Focus).unwrap().auto, Some(true));
        let cfg = r.auto.unwrap();
        assert_eq!(cfg.exposure.target, 0.8, "clamped");
        assert_eq!(cfg.exposure.longest, crate::auto_controls::Longest::Stop(-4));
        assert_eq!(cfg.exposure.max_gain, Some(60));
        assert_eq!(cfg.exposure.speed, crate::auto_controls::Speed::Fast);
        assert_eq!(cfg.white_balance.warmth, -1.0);
        let l = r.learned.unwrap();
        assert_eq!(l.gain_stops, Some(6.0));
        assert_eq!(l.wb_alpha, Some(0.002));
        assert_eq!(l.blocked, [ControlKey::WhiteBalance]);
        assert_eq!(l.rate_caps.len(), 1);

        // the gain follows the exposure: alone it is a plain manual value
        let r = record(json!({ "gain": { "value": 48, "mode": "app" }, "exposure": { "value": -5, "auto": false } }));
        assert!(!r.is_app(ControlKey::Gain));
        assert_eq!(r.get(ControlKey::Gain).unwrap().value, Some(48));
        // old records load unchanged, and nonsense in the new fields is dropped, not an error
        let old = record(json!({ "gain": { "value": 40, "auto": false } }));
        assert!(old.auto.is_none() && old.learned.is_none());
        let bad = parse_record(Some(&json!({
            "id": LINK, "controls": {},
            "auto": { "exposure": { "longest": "never" } }, "learned": "text"
        })))
        .unwrap()
        .unwrap();
        assert!(bad.auto.is_none() && bad.learned.is_none());
        // and the new fields do not change what an old camera applies
        let json = serde_json::to_string(&old).unwrap();
        assert!(!json.contains("app") && !json.contains("learned"), "{json}");
    }

    #[test]
    fn the_gain_follows_the_exposure_even_when_the_record_does_not_say() {
        // exposure alone is in the frontend's record: the camera has a gain, so the loop drives it too
        let (driven, _cam, token) =
            live(json!({ "exposure": { "value": -5, "auto": false, "mode": "app" } }));
        assert!(driven.record().unwrap().is_app(ControlKey::Gain));
        assert_eq!(driven.drive(token, ControlKey::Gain, 40).unwrap().value, 40);
        let rows = driven.get(LINK).unwrap();
        assert_eq!(row(&rows, ControlKey::Gain).mode, ControlMode::App);
        // a camera whose gain is too coarse to use: no flag, the slider stays the user's
        let infos = vec![
            fake::info(ControlKey::Exposure, -13, -1, -6, true),
            fake::info(ControlKey::Gain, 0, 3, 0, false),
        ];
        let (hub, ..) = hub();
        hub.set_record(Some(record(json!({
            "exposure": { "value": -5, "auto": false, "mode": "app" },
            "gain": { "value": 2, "mode": "app" }
        }))));
        hub.normalize_app(LINK, &infos);
        assert!(!hub.record().unwrap().is_app(ControlKey::Gain));
        assert!(hub.record().unwrap().is_app(ControlKey::Exposure));
        let rows = hub.annotate(LINK, &infos);
        assert_eq!(row(&rows, ControlKey::Exposure).app, Some(AppOffer::Stops));
        assert_eq!(row(&rows, ControlKey::Gain).mode, ControlMode::Manual);
    }

    #[test]
    fn the_settings_are_stored_live_and_move_the_generation() {
        let (hub, ..) = hub();
        hub.set_record(Some(record(json!({}))));
        let generation = hub.auto_generation();
        let mut cfg = AutoCfg::default();
        cfg.exposure.target = 0.6;
        cfg.white_balance.warmth = 7.0;
        hub.set_auto_config(LINK, cfg);
        assert!(hub.auto_generation() > generation);
        let snap = hub.auto_snapshot();
        assert_eq!(snap.cfg.exposure.target, 0.6);
        assert_eq!(snap.cfg.white_balance.warmth, 1.0, "clamped");
        assert_eq!(snap.id, LINK);
    }
}
