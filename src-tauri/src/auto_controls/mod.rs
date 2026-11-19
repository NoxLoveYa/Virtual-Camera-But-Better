//! Better Cam auto (docs/AUTO-CONTROLS.md): software auto for the camera's own controls.
//!
//! This is the **pure core**: no threads, no device, no clock of its own. A runner (phase A4) feeds it measurements
//! ([`AutoController::on_sample`]), write results ([`AutoController::on_write_done`]), read-backs
//! ([`AutoController::on_readback`]) and the time ([`AutoController::tick`], every 100 ms) and executes the
//! [`Action`]s it returns. Time is `f64` seconds on one clock (`Instant` in the app, injected in tests); the runner
//! converts QPC capture times.
//!
//! # Layout
//! * `meter.rs`: the exposure error and the white balance error from the 16x16 statistics image (2.4), and the warmth
//!   scale (Auto Adjust's).
//! * `exposure.rs`: the exposure + gain loop (2.5): limits, decision, allocation, gain learning, frame-rate guard,
//!   saturation, engagement.
//! * `white_balance.rs`: the white balance loop (2.6). It sits beside the exposure loop in [`AutoController`], which
//!   ticks it second: exposure has priority (see [`exposure::ExposureView`]).
//! * `watchdog.rs`: W1-W6 (2.7), one [`watchdog::Watchdog`] per loop. A loop that gets a [`watchdog::Fault`] stops,
//!   keeps a [`watchdog::Halt`] and answers with [`Action::HandBack`] (or nothing, for a conflict).
//! * `status.rs`: every status text of 4.2, the reasons of 2.3 and the hints of 2.9, made in Rust.
//! * `sim.rs` (tests only): the simulated camera (6.1). `tests.rs`: the cases of 6.2.
//!
//! The runner (A4) lives in `runner.rs`; the hub and commands are not part of this module.
//!
//! # Contract with the runner (A4)
//! * [`AutoController::tick`] every 100 ms; execute its actions in order. Every [`Action::Write`] gets exactly one
//!   [`AutoController::on_write_done`] (also when it failed or timed out: a write that never reports back is a W1
//!   fault after 3 s). `readback` is what a read right after the write showed.
//! * [`Action::ReadBack`]: read the control and call [`AutoController::on_readback`] (or
//!   [`AutoController::on_read_failed`]). Also feed the powerline frequency control the same way when it changes:
//!   the stripes note reads it.
//! * [`Action::HandBack`]: write the control's Camera auto flag or the manual value, then record the new mode and call
//!   [`AutoController::set_modes`] with it. A hand-back status stays in [`AutoController::status`] until the mode
//!   becomes something other than the one the control was handed to.
//! * On [`Action::Emit`] send [`AutoController::emitted`] as `auto-status`. Its `learned` and `resume` are present when
//!   they changed since the emit before: store them in the camera's record. ([`AutoController::status`] is the same
//!   for polling; its `learned` is present until the next emit takes it.)
//! * `Fault::Conflict` hands nothing back: the mode stays Better Cam auto, the loop waits for
//!   [`AutoController::resume`]. A control whose camera ignores writes is `blocked` in [`Learned`]: store it.

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::capture::controls::{ControlInfo, ControlKey};
#[cfg(test)]
use crate::fx::auto_adjust::Stats;

pub mod exposure;
pub mod meter;
pub mod runner;
pub mod status;
pub mod watchdog;
pub mod white_balance;

#[cfg(test)]
mod sim;
#[cfg(test)]
mod tests;

use exposure::{ExposureCaps, ExposureLoop};
use white_balance::{WhiteBalanceCaps, WhiteBalanceLoop};

// ---------------------------------------------------------------------------------------------------------------
// Settings (5.1: the stored per-camera record)

/// Response speed ("Response" in the card).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Speed {
    Slow,
    #[default]
    Normal,
    Fast,
}

/// "Longest exposure": the frame time, or one whole stop (log2 seconds).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Longest {
    /// Fit the frame rate: `floor(log2(0.95 / F))` (default).
    #[default]
    Fit,
    /// A whole stop in log2 seconds; may be longer than the frame time (the camera drops its rate).
    Stop(i32),
}

impl Serialize for Longest {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Longest::Fit => s.serialize_str("fit"),
            Longest::Stop(n) => s.serialize_i32(*n),
        }
    }
}

impl<'de> Deserialize<'de> for Longest {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Text(String),
            Number(i32),
        }
        match Raw::deserialize(d)? {
            Raw::Number(n) => Ok(Longest::Stop(n)),
            Raw::Text(t) if t == "fit" => Ok(Longest::Fit),
            Raw::Text(t) => Err(serde::de::Error::custom(format!(
                "longest exposure: expected \"fit\" or a number, got {t:?}"
            ))),
        }
    }
}

/// Exposure loop settings.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExposureCfg {
    /// Target brightness, `0.2..=0.8`, the scale of Auto Adjust's `target`.
    pub target: f64,
    pub longest: Longest,
    /// Most gain in device units; `None` = 75 % of the range.
    pub max_gain: Option<i32>,
    pub speed: Speed,
}

impl Default for ExposureCfg {
    fn default() -> Self {
        ExposureCfg {
            target: 0.45,
            longest: Longest::Fit,
            max_gain: None,
            speed: Speed::Normal,
        }
    }
}

/// White balance loop settings.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WhiteBalanceCfg {
    /// `-1..=1`, +1 = warmer.
    pub warmth: f64,
    pub speed: Speed,
}

impl Default for WhiteBalanceCfg {
    fn default() -> Self {
        WhiteBalanceCfg {
            warmth: 0.0,
            speed: Speed::Normal,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AutoCfg {
    pub exposure: ExposureCfg,
    pub white_balance: WhiteBalanceCfg,
}

impl AutoCfg {
    /// The same settings with every value inside its range.
    pub fn sanitized(mut self) -> AutoCfg {
        self.exposure.target = if self.exposure.target.is_finite() {
            self.exposure.target.clamp(0.2, 0.8)
        } else {
            0.45
        };
        self.white_balance.warmth = if self.white_balance.warmth.is_finite() {
            self.white_balance.warmth.clamp(-1.0, 1.0)
        } else {
            0.0
        };
        self
    }
}

/// What the loops learned about one camera (kept with its record, 5.1).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Learned {
    /// `S`: stops across the whole gain range, `0.5..=6`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gain_stops: Option<f64>,
    /// `alpha`: colour signal per mired, `0.002..=0.02`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wb_alpha: Option<f64>,
    /// Capture fps (rounded, as text) to the longest exposure that held it.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub rate_caps: BTreeMap<String, i32>,
    /// Controls whose Better Cam auto is blocked because the camera ignores writes (W2); "Try again" clears it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub blocked: Vec<ControlKey>,
}

/// Who drives a control.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Manual,
    Camera,
    /// Better Cam auto.
    App,
}

/// The values stored when a loop entered `holding` (2.5.11): where a loop starts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Resume {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exposure: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gain: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub white_balance: Option<i32>,
}

/// What the controls were before Better Cam auto took them (2.5.11 step 1): where a hand-back puts them when the
/// camera has no Camera auto (2.7). A missing value is the camera's default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Prior {
    pub exposure: Option<i32>,
    pub gain: Option<i32>,
    /// Gain had its own Camera auto flag on (2.9): a hand-back switches it on again.
    pub gain_auto: bool,
    pub white_balance: Option<i32>,
}

/// The mode of each loop, and where it starts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modes {
    pub exposure: Mode,
    pub white_balance: Mode,
    pub resume: Resume,
    pub prior: Prior,
    /// The hub already wrote the seed values (`apply_on_open`): the loop starts from them without writing. When
    /// false the loop's first action is the seed write (exposure first, then gain; 2.5.11).
    pub seeded: bool,
}

// ---------------------------------------------------------------------------------------------------------------
// Inputs and outputs (5.5, 5.6)

/// One measurement of the camera picture before the filter chain (5.5): the engine's meter makes it. The core takes
/// the capture time converted to its own clock in [`AutoController::on_sample`]; the runner converts the QPC time.
pub use crate::meter::MeterSample;

/// Read by the controller each tick (5.5). Differs from the doc in one point: `resumed_at` is on the controller's
/// clock (seconds), not QPC, because the core's time is `f64` seconds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SessionFacts {
    /// The attached camera, `None` when closed.
    pub token: Option<u64>,
    /// `F`: the opened media type's rate.
    pub target_fps: f64,
    /// Measured over the last 2 s.
    pub capture_fps: f64,
    pub paused: bool,
    /// Samples captured before this time are invalid.
    pub resumed_at: f64,
}

/// The result of one write the runner executed for an [`Action::Write`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WriteDone {
    pub key: ControlKey,
    pub requested: i32,
    /// What a read-back right after the write showed (W2 compares it), if the runner read it.
    pub readback: Option<i32>,
    pub ok: bool,
    /// When the call started and when it returned (the controller's clock).
    pub started: f64,
    pub done: f64,
}

/// What a hand-back puts the control into (2.7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandBack {
    CameraAuto,
    Manual(i32),
}

/// [`HandBack`] as the card shows it (5.4 `handed_back`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandBackKind {
    Camera,
    Manual,
}

impl From<HandBack> for HandBackKind {
    fn from(h: HandBack) -> HandBackKind {
        match h {
            HandBack::CameraAuto => HandBackKind::Camera,
            HandBack::Manual(_) => HandBackKind::Manual,
        }
    }
}

/// What the runner must do.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Action {
    /// Write the manual value. Write actions of one tick run in order; the runner reports each with
    /// [`AutoController::on_write_done`].
    Write { key: ControlKey, value: i32 },
    /// Read the control and report with [`AutoController::on_readback`] (W6).
    ReadBack { key: ControlKey },
    /// Stop the loop for `key` and give the control back (watchdog). Gain is handed back with exposure.
    HandBack { key: ControlKey, to: HandBack },
    /// The status changed: emit `auto-status`.
    Emit,
}

// ---------------------------------------------------------------------------------------------------------------
// Status (5.4)

/// The states of the exposure loop as the card shows them (5.4). The last five are the watchdog's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExposureState {
    Starting,
    Adjusting,
    Holding,
    LimitGain,
    LimitExposure,
    TooBright,
    NoSignal,
    NoPicture,
    Paused,
    Closed,
    Conflict,
    Ignored,
    NoResponse,
    NotAnswering,
    Hunting,
}

/// Why the exposure cap is where it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapReason {
    Fps,
    User,
    Learned,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ExposureStatus {
    pub state: ExposureState,
    pub exposure: i32,
    pub gain: Option<i32>,
    pub cap: i32,
    pub cap_reason: CapReason,
    pub max_gain: Option<i32>,
    pub fps: f64,
    pub target_fps: f64,
    /// The text of 4.2, made in Rust so tests can check it.
    pub message: Option<String>,
    /// Where the controls went when the watchdog stopped the loop.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handed_back: Option<HandBackKind>,
    /// The stripes note (2.5.10), when it applies.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// The states of the white balance loop (5.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WhiteBalanceState {
    Starting,
    Adjusting,
    Holding,
    AtLimit,
    /// Too little colour to measure (a dark picture, one colour): the Kelvin is kept.
    NoSignal,
    NoPicture,
    Paused,
    Closed,
    Conflict,
    Ignored,
    NoResponse,
    NotAnswering,
    Hunting,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct WhiteBalanceStatus {
    pub state: WhiteBalanceState,
    pub kelvin: i32,
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handed_back: Option<HandBackKind>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct AutoStatus {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exposure: Option<ExposureStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub white_balance: Option<WhiteBalanceStatus>,
    /// Present on entering `holding`, or after 3 s in a limit state (at most once per 10 s): the store persists it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume: Option<Resume>,
    /// Present when the learned values changed since the last emitted status: the store persists them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub learned: Option<Learned>,
}

/// Where the engaged loops start (2.5.11): what the device must hold for [`Modes::seeded`] to be true.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Seeds {
    /// Exposure and, when the loop uses gain, gain.
    pub exposure: Option<(i32, Option<i32>)>,
    pub white_balance: Option<i32>,
}

/// Whether Better Cam auto is offered for a loop, and how (2.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Offer {
    Full,
    /// Exposure in whole stops only (no usable gain, 2.9).
    Stops,
    No(NoOffer),
}

impl Offer {
    pub fn offered(self) -> bool {
        !matches!(self, Offer::No(_))
    }

    /// The reason text of a mode that is not offered (the tooltip of the disabled option).
    pub fn why(self, key: ControlKey) -> Option<String> {
        match self {
            Offer::No(why) => Some(status::not_offered(key, why)),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoOffer {
    /// The control is absent or does not take manual values.
    NoManual,
    /// The range does not look like log2 seconds (exposure) or Kelvin (white balance).
    UnknownUnits,
    /// The watchdog found that the camera ignores the writes; "Try again" clears it.
    Blocked,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Support {
    pub exposure: Offer,
    pub white_balance: Offer,
}

// ---------------------------------------------------------------------------------------------------------------
// The controller

pub struct AutoController {
    cfg: AutoCfg,
    modes: Modes,
    learned: Learned,
    /// The learned values the last emitted status carried.
    learned_sent: Learned,
    session: Option<u64>,
    exposure: Option<ExposureLoop>,
    white_balance: Option<WhiteBalanceLoop>,
    /// Mains frequency from the powerline control, for the stripes note.
    mains: Option<u32>,
    last_status: Option<AutoStatus>,
    last_emit: f64,
}

/// How much a status differs from the last emitted one.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Change {
    Same,
    /// Only numbers moved: emit at most twice a second.
    Moved,
    /// The state, a text or an end of the loop changed: emit now.
    Visible,
}

/// The mains frequency a powerline control value means (Disabled 0, 50 Hz 1, 60 Hz 2, Auto 3).
fn mains_of(value: i32) -> Option<u32> {
    match value {
        1 => Some(50),
        2 => Some(60),
        _ => None,
    }
}

impl AutoController {
    /// `caps` is what the device reports now (`ControlInfo` of every control it has).
    pub fn new(caps: &[ControlInfo], cfg: AutoCfg, learned: Learned, modes: Modes) -> AutoController {
        let cfg = cfg.sanitized();
        // A loop exists for a control that can run it (2.3 without `blocked`: "Try again" clears that).
        let ex = Self::offer(caps, ControlKey::Exposure, &Learned::default());
        let wb = Self::offer(caps, ControlKey::WhiteBalance, &Learned::default());
        let mut c = AutoController {
            cfg,
            modes,
            learned_sent: learned.clone(),
            learned,
            session: None,
            exposure: ex
                .offered()
                .then(|| ExposureCaps::from_controls(caps))
                .flatten()
                .map(|caps| ExposureLoop::new(caps, cfg.exposure)),
            white_balance: wb
                .offered()
                .then(|| WhiteBalanceCaps::from_controls(caps))
                .flatten()
                .map(|caps| WhiteBalanceLoop::new(caps, cfg.white_balance)),
            mains: caps
                .iter()
                .find(|c| c.key == ControlKey::PowerlineFrequency)
                .and_then(|c| mains_of(c.value)),
            last_status: None,
            last_emit: f64::NEG_INFINITY,
        };
        c.apply_modes();
        c
    }

    /// 2.3 and 2.9, for the DTOs: whether Better Cam auto is offered for each loop, and how.
    pub fn support(caps: &[ControlInfo], learned: &Learned) -> Support {
        Support {
            exposure: Self::offer(caps, ControlKey::Exposure, learned),
            white_balance: Self::offer(caps, ControlKey::WhiteBalance, learned),
        }
    }

    fn offer(caps: &[ControlInfo], key: ControlKey, learned: &Learned) -> Offer {
        if learned.blocked.contains(&key) {
            return Offer::No(NoOffer::Blocked);
        }
        let Some(c) = caps.iter().find(|c| c.key == key) else {
            return Offer::No(NoOffer::NoManual);
        };
        if !c.can_manual {
            return Offer::No(NoOffer::NoManual);
        }
        match key {
            ControlKey::WhiteBalance => {
                // Kelvin: `min >= 1500`, `max <= 12000`, `max - min >= 1000`.
                if c.min < 1500 || c.max > 12_000 || c.max - c.min < 1000 {
                    Offer::No(NoOffer::UnknownUnits)
                } else {
                    Offer::Full
                }
            }
            _ => {
                // log2 seconds: `min >= -16`, `max <= 5`, `step == 1`.
                if c.min < -16 || c.max > 5 || c.step != 1 {
                    Offer::No(NoOffer::UnknownUnits)
                } else if ExposureCaps::from_controls(caps).is_some_and(|c| c.gain.is_some()) {
                    Offer::Full
                } else {
                    Offer::Stops
                }
            }
        }
    }

    pub fn set_cfg(&mut self, cfg: AutoCfg) {
        self.cfg = cfg.sanitized();
        if let Some(e) = &mut self.exposure {
            e.set_cfg(self.cfg.exposure);
        }
        if let Some(w) = &mut self.white_balance {
            w.set_cfg(self.cfg.white_balance);
        }
    }

    pub fn set_modes(&mut self, modes: Modes) {
        self.modes = modes;
        self.apply_modes();
    }

    fn apply_modes(&mut self) {
        let m = self.modes;
        if let Some(e) = &mut self.exposure {
            e.set_mode(m.exposure, m.resume, m.seeded, m.prior);
        }
        if let Some(w) = &mut self.white_balance {
            w.set_mode(m.white_balance, m.resume, m.seeded, m.prior);
        }
    }

    /// The user pressed "Resume" after a conflict, or "Try again" after the camera ignored Better Cam auto: the
    /// loop forgets its halt and `blocked` is cleared. A loop that was handed back starts again when the mode is
    /// set to Better Cam auto ([`AutoController::set_modes`]); a paused one (conflict) starts again now.
    pub fn resume(&mut self, key: ControlKey) {
        match key {
            ControlKey::Exposure | ControlKey::Gain => {
                self.learned.blocked.retain(|k| *k != ControlKey::Exposure);
                if let Some(e) = &mut self.exposure {
                    e.resume();
                }
            }
            ControlKey::WhiteBalance => {
                self.learned.blocked.retain(|k| *k != ControlKey::WhiteBalance);
                if let Some(w) = &mut self.white_balance {
                    w.resume();
                }
            }
            _ => {}
        }
    }

    /// The values the engaged loops write as their first action when the hub did not seed them (2.5.11), for a
    /// session opened at `target_fps`: the runner reads the device and sets [`Modes::seeded`] when it already holds
    /// them.
    pub fn seeds(&self, target_fps: f64) -> Seeds {
        Seeds {
            exposure: self
                .exposure
                .as_ref()
                .filter(|e| e.engaged())
                .map(|e| e.seed(target_fps, &self.learned)),
            white_balance: self
                .white_balance
                .as_ref()
                .filter(|w| w.engaged())
                .map(WhiteBalanceLoop::seed),
        }
    }

    /// Whether any loop needs measurements (the runner sets `Shared.metering` from it).
    pub fn wants_meter(&self) -> bool {
        self.exposure.as_ref().is_some_and(ExposureLoop::engaged)
            || self.white_balance.as_ref().is_some_and(WhiteBalanceLoop::engaged)
    }

    /// A measurement arrived. `capture_time` is the frame's capture time on the controller's clock.
    pub fn on_sample(&mut self, s: &MeterSample, capture_time: f64) {
        if self.session.is_some_and(|t| t != s.token) {
            return;
        }
        if let Some(e) = &mut self.exposure {
            e.on_sample(&s.stats, capture_time);
        }
        if let Some(w) = &mut self.white_balance {
            w.on_sample(&s.stats, capture_time);
        }
    }

    pub fn on_write_done(&mut self, w: WriteDone) {
        match w.key {
            ControlKey::Exposure | ControlKey::Gain => {
                if let Some(e) = &mut self.exposure {
                    e.on_write_done(w);
                }
            }
            ControlKey::WhiteBalance => {
                if let Some(l) = &mut self.white_balance {
                    l.on_write_done(w);
                }
            }
            _ => {}
        }
    }

    /// The answer to an [`Action::ReadBack`] (W6): a driven control that is not as we left it (Auto flag on, another
    /// value) is taken back, three times a minute at most. The powerline control, when the runner reads it, feeds the
    /// stripes note. The device's reported exposure is never used to seed a loop (2.5.11).
    pub fn on_readback(&mut self, info: &ControlInfo, now: f64) {
        match info.key {
            ControlKey::Exposure | ControlKey::Gain => {
                if let Some(e) = &mut self.exposure {
                    e.on_readback(info, now);
                }
            }
            ControlKey::WhiteBalance => {
                if let Some(w) = &mut self.white_balance {
                    w.on_readback(info, now);
                }
            }
            ControlKey::PowerlineFrequency => self.mains = mains_of(info.value),
            _ => {}
        }
    }

    /// A read-back failed (W1: three failures in a row stop the loop).
    pub fn on_read_failed(&mut self, key: ControlKey, now: f64) {
        match key {
            ControlKey::Exposure | ControlKey::Gain => {
                if let Some(e) = &mut self.exposure {
                    e.on_read_failed(now);
                }
            }
            ControlKey::WhiteBalance => {
                if let Some(w) = &mut self.white_balance {
                    w.on_read_failed(now);
                }
            }
            _ => {}
        }
    }

    pub fn tick(&mut self, now: f64, facts: &SessionFacts) -> Vec<Action> {
        if facts.token != self.session {
            self.session = facts.token;
            if let Some(e) = &mut self.exposure {
                e.reset_session();
            }
            if let Some(w) = &mut self.white_balance {
                w.reset_session();
            }
        }
        let mut actions = Vec::new();
        let wb_busy = self.white_balance.as_ref().is_some_and(WhiteBalanceLoop::busy);
        if let Some(e) = &mut self.exposure {
            actions.extend(e.tick(now, facts, &mut self.learned, wb_busy));
        }
        let view = self.exposure.as_ref().map_or_else(exposure::ExposureView::idle, |e| e.view());
        if let Some(w) = &mut self.white_balance {
            actions.extend(w.tick(now, facts, &mut self.learned, &view));
        }
        let status = self.status();
        if self.status_changed(now, &status) {
            self.last_status = Some(status);
            self.last_emit = now;
            self.learned_sent = self.learned.clone();
            actions.push(Action::Emit);
        }
        actions
    }

    /// How the visible state differs from the last emitted one.
    fn status_changed(&self, now: f64, status: &AutoStatus) -> bool {
        let Some(old) = &self.last_status else {
            return status.exposure.is_some() || status.white_balance.is_some();
        };
        let mut change = Change::Same;
        let mut up = |c: Change| change = change.max(c);
        match (&old.exposure, &status.exposure) {
            (None, None) => {}
            (Some(o), Some(n)) => {
                if o.state != n.state || o.message != n.message || o.handed_back != n.handed_back || o.note != n.note
                {
                    up(Change::Visible);
                } else if o.exposure != n.exposure
                    || o.gain != n.gain
                    || o.cap != n.cap
                    || o.max_gain != n.max_gain
                {
                    up(Change::Moved);
                }
            }
            _ => up(Change::Visible),
        }
        match (&old.white_balance, &status.white_balance) {
            (None, None) => {}
            (Some(o), Some(n)) => {
                if o.state != n.state || o.message != n.message || o.handed_back != n.handed_back {
                    up(Change::Visible);
                } else if o.kelvin != n.kelvin {
                    up(Change::Moved);
                }
            }
            _ => up(Change::Visible),
        }
        if status.resume != old.resume {
            up(Change::Visible);
        }
        if status.learned.is_some() {
            up(Change::Moved);
        }
        match change {
            Change::Same => false,
            Change::Moved => now - self.last_emit >= 0.5,
            Change::Visible => true,
        }
    }

    pub fn status(&self) -> AutoStatus {
        let exposure = self.exposure.as_ref().and_then(|e| e.status(&self.learned, self.mains));
        let white_balance = self.white_balance.as_ref().and_then(|w| w.status(&self.learned));
        let from_exposure = self.exposure.as_ref().and_then(ExposureLoop::resume_report);
        let from_white_balance = self.white_balance.as_ref().and_then(WhiteBalanceLoop::resume_report);
        let resume = (from_exposure.is_some() || from_white_balance.is_some()).then(|| Resume {
            exposure: from_exposure.and_then(|r| r.exposure),
            gain: from_exposure.and_then(|r| r.gain),
            white_balance: from_white_balance.and_then(|r| r.white_balance),
        });
        AutoStatus {
            exposure,
            white_balance,
            resume,
            learned: (self.learned != self.learned_sent).then(|| self.learned.clone()),
        }
    }

    /// The status that went with the last [`Action::Emit`]: what the runner sends as `auto-status` and what the
    /// store persists (`learned` and `resume` are in it when they changed since the emit before).
    pub fn emitted(&self) -> Option<&AutoStatus> {
        self.last_status.as_ref()
    }

    pub fn learned(&self) -> &Learned {
        &self.learned
    }
}
