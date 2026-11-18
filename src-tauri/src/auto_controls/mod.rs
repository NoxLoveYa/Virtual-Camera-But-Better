//! Better Cam auto (docs/AUTO-CONTROLS.md): software auto for the camera's own controls.
//!
//! This is the **pure core**: no threads, no device, no clock of its own. A runner (phase A4) feeds it measurements
//! ([`AutoController::on_sample`]), write results ([`AutoController::on_write_done`]), read-backs
//! ([`AutoController::on_readback`]) and the time ([`AutoController::tick`], every 100 ms) and executes the
//! [`Action`]s it returns. Time is `f64` seconds on one clock (`Instant` in the app, injected in tests); the runner
//! converts QPC capture times.
//!
//! # Layout (what each phase adds)
//! * `meter.rs` (A1): the exposure error from the 16x16 statistics image (2.4). A2 adds the white balance error.
//! * `exposure.rs` (A1): the exposure + gain loop (2.5): limits, decision, allocation, gain learning, frame-rate
//!   guard, saturation, engagement.
//! * `status.rs` (A1 for the exposure texts, A2 adds white balance and hand-back texts): status text made in Rust.
//! * `white_balance.rs` (A2, not yet): the white balance loop (2.6). It plugs in beside `exposure` in
//!   [`AutoController`]: one more `Option<WhiteBalanceLoop>` field, ticked after the exposure loop, which has priority
//!   (it must see `exposure.busy()`/`exposure.unsettled()` and the time of the last exposure batch).
//! * `watchdog.rs` (A2, not yet): W1-W6 (2.7). It reads [`WriteDone`] results, read-backs and the tick, and returns
//!   [`Action::HandBack`]. The hooks are in place: [`WriteDone::ok`] / `readback`, [`AutoController::on_readback`]
//!   (a no-op now), [`ExposureState`] has the hand-back states, [`Learned::blocked`].
//! * `sim.rs` (tests only): the simulated camera (6.1). `tests.rs`: the cases of 6.2.
//!
//! The runner (A4) lives in `runner.rs`; the hub and commands are not part of this module.

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::capture::controls::{ControlInfo, ControlKey};
use crate::fx::auto_adjust::Stats;

pub mod exposure;
pub mod meter;
pub mod status;

#[cfg(test)]
mod sim;
#[cfg(test)]
mod tests;

use exposure::{ExposureCaps, ExposureLoop};

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

/// White balance loop settings (phase A2 uses them).
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
    /// `alpha`: colour signal per mired, `0.002..=0.02` (A2).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wb_alpha: Option<f64>,
    /// Capture fps (rounded, as text) to the longest exposure that held it.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub rate_caps: BTreeMap<String, i32>,
    /// Controls whose Better Cam auto is blocked because the camera ignores writes (A2).
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
    pub exposure: Option<i32>,
    pub gain: Option<i32>,
    pub white_balance: Option<i32>,
}

/// The mode of each loop, and where it starts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modes {
    pub exposure: Mode,
    pub white_balance: Mode,
    pub resume: Resume,
    /// The hub already wrote the seed values (`apply_on_open`): the loop starts from them without writing. When
    /// false the loop's first action is the seed write (exposure first, then gain; 2.5.11).
    pub seeded: bool,
}

// ---------------------------------------------------------------------------------------------------------------
// Inputs and outputs (5.5, 5.6)

/// One measurement of the camera picture before the filter chain (5.5).
#[derive(Clone, Debug)]
pub struct MeterSample {
    /// The hub token of the camera session that produced it.
    pub token: u64,
    /// Capture time of the measured frame (QPC). The core takes the converted time in
    /// [`AutoController::on_sample`]; the runner converts with `vcam_protocol`'s frequency.
    pub capture_qpc: u64,
    /// 16x16 linear means + coverage (`fx::auto_adjust::Stats`; phase A3 makes the meter share it).
    pub stats: Stats,
}

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

/// What the runner must do.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Action {
    /// Write the manual value. Write actions of one tick run in order; the runner reports each with
    /// [`AutoController::on_write_done`].
    Write { key: ControlKey, value: i32 },
    /// Read the control and report with [`AutoController::on_readback`] (A2: W6).
    ReadBack { key: ControlKey },
    /// Stop the loop for `key` and give the control back (A2: watchdog).
    HandBack { key: ControlKey, to: HandBack },
    /// The status changed: emit `auto-status`.
    Emit,
}

// ---------------------------------------------------------------------------------------------------------------
// Status (5.4)

/// The states of a loop as the card shows them (5.4). A1 produces the first nine; the rest are the watchdog's (A2).
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
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct AutoStatus {
    pub exposure: Option<ExposureStatus>,
    /// Present on entering `holding` (at most once per 10 s): the store persists it.
    pub resume: Option<Resume>,
    // A2: `white_balance: Option<WhiteBalanceStatus>`.
}

/// Whether Better Cam auto is offered for a loop, and how (2.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Offer {
    Full,
    /// Exposure in whole stops only (no usable gain).
    Stops,
    No(NoOffer),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoOffer {
    NoManual,
    UnknownUnits,
    Blocked,
    /// Phase A2 has not built it yet.
    NotBuilt,
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
    session: Option<u64>,
    exposure: Option<ExposureLoop>,
    last_status: Option<AutoStatus>,
    last_emit: f64,
}

impl AutoController {
    /// `caps` is what the device reports now (`ControlInfo` of every control it has).
    pub fn new(caps: &[ControlInfo], cfg: AutoCfg, learned: Learned, modes: Modes) -> AutoController {
        let cfg = cfg.sanitized();
        let mut c = AutoController {
            cfg,
            modes,
            learned,
            session: None,
            exposure: ExposureCaps::from_controls(caps).map(|caps| ExposureLoop::new(caps, cfg.exposure)),
            last_status: None,
            last_emit: f64::NEG_INFINITY,
        };
        c.apply_modes();
        c
    }

    /// 2.3, for the DTOs. A1 decides the exposure loop; the white balance loop is phase A2.
    pub fn support(caps: &[ControlInfo], learned: &Learned) -> Support {
        let exposure = if learned.blocked.contains(&ControlKey::Exposure) {
            Offer::No(NoOffer::Blocked)
        } else {
            match caps.iter().find(|c| c.key == ControlKey::Exposure) {
                None => Offer::No(NoOffer::NoManual),
                Some(c) if !c.can_manual => Offer::No(NoOffer::NoManual),
                Some(c) if c.min < -16 || c.max > 5 || c.step != 1 => Offer::No(NoOffer::UnknownUnits),
                Some(_) => match ExposureCaps::from_controls(caps) {
                    Some(c) if c.gain.is_some() => Offer::Full,
                    _ => Offer::Stops,
                },
            }
        };
        Support {
            exposure,
            white_balance: Offer::No(NoOffer::NotBuilt),
        }
    }

    pub fn set_cfg(&mut self, cfg: AutoCfg) {
        self.cfg = cfg.sanitized();
        if let Some(e) = &mut self.exposure {
            e.set_cfg(self.cfg.exposure);
        }
    }

    pub fn set_modes(&mut self, modes: Modes) {
        self.modes = modes;
        self.apply_modes();
    }

    fn apply_modes(&mut self) {
        if let Some(e) = &mut self.exposure {
            e.set_engaged(self.modes.exposure == Mode::App, self.modes.resume, self.modes.seeded);
        }
    }

    /// Whether any loop needs measurements (the runner sets `Shared.metering` from it).
    pub fn wants_meter(&self) -> bool {
        self.exposure.as_ref().is_some_and(ExposureLoop::engaged)
    }

    /// A measurement arrived. `capture_time` is the frame's capture time on the controller's clock.
    pub fn on_sample(&mut self, s: &MeterSample, capture_time: f64) {
        if self.session.is_some_and(|t| t != s.token) {
            return;
        }
        if let Some(e) = &mut self.exposure {
            e.on_sample(&s.stats, capture_time);
        }
    }

    pub fn on_write_done(&mut self, w: WriteDone) {
        if let Some(e) = &mut self.exposure {
            e.on_write_done(w);
        }
    }

    /// The 1 Hz read-back of a driven control (W6, phase A2). Deliberately a no-op in A1: the device's reported
    /// exposure is stale while Camera auto is on, so the loop never seeds from it (2.5.11).
    pub fn on_readback(&mut self, _info: &ControlInfo, _now: f64) {}

    pub fn tick(&mut self, now: f64, facts: &SessionFacts) -> Vec<Action> {
        if facts.token != self.session {
            self.session = facts.token;
            if let Some(e) = &mut self.exposure {
                e.reset_session();
            }
        }
        let mut actions = Vec::new();
        if let Some(e) = &mut self.exposure {
            actions.extend(e.tick(now, facts, &mut self.learned));
        }
        // A2: white balance loop, then the watchdog.
        let status = self.status();
        if self.status_changed(now, &status) {
            self.last_status = Some(status);
            self.last_emit = now;
            actions.push(Action::Emit);
        }
        actions
    }

    /// Emit when the visible state changed; while only values move, at most twice a second.
    fn status_changed(&self, now: f64, status: &AutoStatus) -> bool {
        let Some(old) = &self.last_status else {
            return status.exposure.is_some();
        };
        let (Some(o), Some(n)) = (&old.exposure, &status.exposure) else {
            return old.exposure.is_some() != status.exposure.is_some();
        };
        if o.state != n.state || o.message != n.message || status.resume != old.resume {
            return true;
        }
        let moved = o.exposure != n.exposure || o.gain != n.gain || o.cap != n.cap || o.max_gain != n.max_gain;
        moved && now - self.last_emit >= 0.5
    }

    pub fn status(&self) -> AutoStatus {
        match &self.exposure {
            Some(e) => AutoStatus {
                exposure: e.status(&self.learned),
                resume: e.resume_report(),
            },
            None => AutoStatus::default(),
        }
    }

    pub fn learned(&self) -> &Learned {
        &self.learned
    }
}
