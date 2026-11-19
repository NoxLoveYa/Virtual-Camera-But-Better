//! The white balance loop (docs/AUTO-CONTROLS.md 2.6): Kelvin worked in mired, step-and-settle like the exposure
//! loop, with exposure first.
//!
//! The picture's blue-red signal `c = ln(B/R)` follows `c ≈ α·(M − M_light)` (`M = 10^6 / K`): a setting warmer than
//! the light (higher K, lower M) leaves the picture orange. The loop measures `e_wb = c - c*` (`c*` from the warmth,
//! Auto Adjust's scale, see `meter::warmth_target`), moves `ΔM = -k·e_wb/α` at most `ΔM_max` per decision and waits
//! until a frame captured 0.3 s after the write has been measured.
//!
//! `e_wb` comes from the shared light estimator (`fx::awb`, docs/AUTO-WHITE-BALANCE.md 3.3): under a strong cast the
//! loop has a confident, unbiased reading and walks toward the light. It holds when the picture is dark or mostly one
//! colour (a closed loop on a biased reading goes all the way to the wrong answer), and the estimator's reason picks
//! the text. A picture with two answers (a bright face beside the walls, two lights) does not hold: the slow lane
//! follows the middle of the two answers with a confidence-weighted average (time constant 8 s, 32 s with no
//! confidence left) and writes only when that average passes the band (10.4.2). What is left at the end of the range
//! goes to Auto Adjust, which measures this loop's output.
//!
//! A change of the Warmth setting is not a measurement and is never held back (docs/AUTO-WHITE-BALANCE.md 10.4.3): it
//! is turned into one move (`ΔM = Δc*/α_ff`) that the next tick writes whatever the reading says, then the loop
//! measures from the narrow band and finishes the rest when the picture can be measured.
//!
//! Pure like the exposure loop; the watchdog (`watchdog.rs`) is part of it.

use std::collections::VecDeque;

use super::exposure::ExposureView;
use super::meter::{measure_white_balance, warmth_target, Reason};
use super::status::{white_balance_message, WhiteBalanceFacts};
use super::watchdog::{
    Drift, Fault, Halt, Moved, Unit, Watchdog, NO_EFFECT_RATIO, NO_EFFECT_SPAN, NO_PICTURE_AFTER,
};
use super::{
    Action, HandBack, Learned, Mode, Prior, Resume, SessionFacts, Speed, WhiteBalanceCfg, WhiteBalanceState,
    WhiteBalanceStatus, WriteDone,
};
use crate::capture::controls::{ControlInfo, ControlKey};
use crate::fx::auto_adjust::Stats;

// ---------------------------------------------------------------------------------------------------------------
// Constants (2.6, exactly as written in the doc)

/// `α`: colour signal per mired, until learned. Unverified.
const ALPHA_INITIAL: f64 = 0.006;
/// The blackbody slope near 5000 K (docs/AUTO-WHITE-BALANCE.md 2.1): the `α` a warmth change is turned into a move
/// with, whenever the loop's own `α` is smaller (a move that is too small is finished by the loop, one that is too
/// large would have to be taken back).
const ALPHA_FF: f64 = 0.0105;
/// The slow lane (docs/AUTO-WHITE-BALANCE.md 10.4.2): the time constant of its average at full weight.
const SLOW_TAU: f64 = 8.0;
/// The weight of a sample whose doubt took all its confidence: a time constant of `SLOW_TAU / SLOW_FLOOR` = 32 s.
const SLOW_FLOOR: f64 = 0.25;
/// A gap between samples (a write, `D_APPLY`, a dropped frame) counts as at most this much evidence.
const SLOW_DT_MAX: f64 = 0.5;
/// The lane survives this long without evidence, so the card does not flicker between texts.
const SLOW_KEEP: f64 = 2.0;
/// The feed-forward writes at most this often (measured from the end of the last write): the slider sends a value
/// every 80 ms and a camera takes a write of this kind badly at 10 a second. `warmth_shift` keeps adding up in
/// between, so nothing of the drag is lost; the next write carries it.
const FF_GAP: f64 = 0.25;
/// A feed-forward is stored as the resume point once no feed-forward has written for this long (the drag is over).
const FF_STORE_QUIET: f64 = 1.0;
const ALPHA_MIN: f64 = 0.002;
const ALPHA_MAX: f64 = 0.02;
/// A sample is valid when captured this long after the write returned.
const D_APPLY: f64 = 0.3;
/// Hold (no move) while the confidence is below this: a scene of one colour, a dark scene.
const HOLD_BELOW: f64 = 0.3;
/// Band: `B_out = max(OUT_MIN, OUT_SIGMA·σ)`, `B_in = max(IN_MIN, IN_SIGMA·σ)`.
const OUT_MIN: f64 = 0.06;
const OUT_SIGMA: f64 = 3.0;
const IN_MIN: f64 = 0.015;
const IN_SIGMA: f64 = 1.5;
/// No write within this long of an exposure batch (2.6).
const EXPOSURE_QUIET: f64 = 0.3;
/// `K` at a limit and the error past `B_out` for this long: `at_limit`.
const LIMIT_AFTER: f64 = 3.0;
const MAX_SAMPLES: usize = 16;
const SIGMA_SAMPLES: usize = 8;
const MEDIAN_SAMPLES: usize = 3;
/// Learning `α`, the exposure loop's way (2.5.7): "same bounds and exponent". The doc leaves the smallest move that
/// teaches open; the exposure loop's 0.2 stop is 0.14 in `ln(B/R)`, which a 20 mired step (0.12) never reaches, so
/// the white balance loop learns from predicted changes of 0.06 and more (10 mired at the initial `α`).
const LEARN_MIN_PREDICTED: f64 = 0.06;
const LEARN_MIN_CONFIDENCE: f64 = 0.8;
const LEARN_RHO: (f64, f64) = (0.25, 4.0);
const LEARN_CLAMP: (f64, f64) = (0.7, 1.4);
const LEARN_EXPONENT: f64 = 0.3;
/// W3: a sample counts as confident for judging a batch.
const JUDGE_CONFIDENCE: f64 = 0.5;
const JUDGE_PATIENCE: f64 = 3.0;
const RESUME_EVERY: f64 = 10.0;
/// The resume point is also reported once the loop has sat in `at_limit` this long (2.5.11).
const LIMIT_STORE_AFTER: f64 = 3.0;

/// The response presets of 2.5.4 (white balance columns).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WbPace {
    pub k: f64,
    /// `ΔM_max`, mired per decision.
    pub dm_max: f64,
    pub gap: f64,
}

impl Speed {
    pub fn white_balance_pace(self) -> WbPace {
        match self {
            Speed::Slow => WbPace {
                k: 0.3,
                dm_max: 10.0,
                gap: 0.6,
            },
            Speed::Normal => WbPace {
                k: 0.5,
                dm_max: 20.0,
                gap: 0.4,
            },
            Speed::Fast => WbPace {
                k: 0.7,
                dm_max: 40.0,
                gap: 0.3,
            },
        }
    }
}

/// What the loop needs to know about the camera's white balance control.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WhiteBalanceCaps {
    pub k_min: i32,
    pub k_max: i32,
    pub k_step: i32,
    pub k_default: i32,
    /// The camera has Camera auto for white balance: where a hand-back goes.
    pub can_auto: bool,
}

impl WhiteBalanceCaps {
    /// `None` when the camera has no manual white balance.
    pub fn from_controls(caps: &[ControlInfo]) -> Option<WhiteBalanceCaps> {
        let c = caps
            .iter()
            .find(|c| c.key == ControlKey::WhiteBalance && c.can_manual)?;
        let (k_min, k_max) = (c.min.min(c.max), c.max.max(c.min));
        Some(WhiteBalanceCaps {
            k_min,
            k_max,
            k_step: c.step.max(1),
            k_default: c.default.clamp(k_min, k_max),
            can_auto: c.can_auto,
        })
    }

    /// A Kelvin value on the camera's step, inside its range.
    fn snap(&self, k: f64) -> i32 {
        let steps = ((k - f64::from(self.k_min)) / f64::from(self.k_step)).round() as i32;
        (self.k_min + steps * self.k_step).clamp(self.k_min, self.k_max)
    }
}

fn mired(k: i32) -> f64 {
    1e6 / f64::from(k.max(1))
}

#[derive(Clone, Copy, Debug)]
struct Sample {
    t: f64,
    e: f64,
    conf: f64,
    reason: Reason,
    /// The estimator's confidence before the doubt of two answers (`WhiteBalanceReading::light_conf`).
    light_conf: f64,
}

/// The slow lane's state: a confidence-weighted average of the middle of the two answers (10.4.2).
#[derive(Clone, Copy, Debug)]
struct Slow {
    /// The average error `e_wb`, kept as it will be once the moves already written have taken effect.
    e: f64,
    /// Capture time of the last sample it took in.
    t: f64,
    /// Loop time of the last sample that was evidence.
    evidence_at: f64,
}

#[derive(Clone, Copy, Debug)]
struct Pending {
    prev: i32,
    issued_at: f64,
    seed: bool,
    /// The `Δc*` of the user's warmth change this write carries (`feed_forward`): given back to `warmth_shift` when
    /// the write fails, so the change is not lost.
    ff: f64,
    /// What the slow lane's average is to be shifted by once the write has succeeded (`α·ΔM` of its move).
    lane_shift: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Band {
    Holding,
    Adjusting,
}

/// A batch whose effect teaches `α` and that W3 judges.
#[derive(Clone, Copy, Debug)]
struct Batch {
    issued_at: f64,
    e_before: f64,
    /// Confident when the decision was made.
    confident: bool,
    /// `α·ΔM` as predicted: the change of `e_wb`.
    predicted: f64,
    /// `|ΔM|`, mired.
    dm: f64,
    /// `α` has learned from it (once, at the first reading after it).
    learned: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Assess {
    Go,
    Wait,
    Fault(Fault),
}

#[derive(Clone, Copy, Debug)]
struct Reading {
    e: f64,
    sigma: f64,
    conf: f64,
    /// The latest reason a recent sample gave for a low confidence (`Ok` when none did).
    reason: Reason,
    /// The median of the last samples' `light_conf`.
    light_conf: f64,
}

pub struct WhiteBalanceLoop {
    caps: WhiteBalanceCaps,
    cfg: WhiteBalanceCfg,
    mode_app: bool,
    halt: Option<Halt>,
    fault: Option<Fault>,
    prior: Prior,
    resume: Resume,
    hub_seeded: bool,
    started: bool,
    closed: bool,
    paused: bool,
    resumed_at: f64,
    now: f64,
    last_arrival: f64,
    /// The Kelvin the camera has now, as far as the loop knows.
    k: i32,
    samples: VecDeque<Sample>,
    pending: Option<Pending>,
    t_done: f64,
    band: Band,
    state: WhiteBalanceState,
    limit_since: Option<f64>,
    batch: Option<Batch>,
    last_resume_at: f64,
    resume_out: Option<Resume>,
    /// Since when the state is `at_limit`, and whether the resume point of this stay is stored.
    limit_state_since: Option<f64>,
    limit_stored: bool,
    no_picture: Option<f64>,
    retake: Option<i32>,
    /// Why the last hold (`NoSignal`) was made: picks its text.
    hold_reason: Reason,
    /// The change of the aim `c*` the user made with the Warmth setting that the Kelvin has not followed yet
    /// (docs/AUTO-WHITE-BALANCE.md 10.4.3). Written by `feed_forward` whatever the picture says.
    warmth_shift: f64,
    /// A feed-forward has written and its Kelvin is not stored as the resume point yet (`FF_STORE_QUIET`,
    /// `RESUME_EVERY`).
    ff_store: bool,
    /// The stored resume point is a feed-forward's: reported in `no_signal` and `slow` too, where the loop stores
    /// nothing of its own. Ends with the next write of the loop.
    ff_resume: bool,
    /// The slow lane, while the picture has two answers (`WhiteBalanceState::Slow`).
    slow: Option<Slow>,
    watchdog: Watchdog,
}

impl WhiteBalanceLoop {
    pub fn new(caps: WhiteBalanceCaps, cfg: WhiteBalanceCfg) -> WhiteBalanceLoop {
        WhiteBalanceLoop {
            caps,
            cfg,
            mode_app: false,
            halt: None,
            fault: None,
            prior: Prior::default(),
            resume: Resume::default(),
            hub_seeded: false,
            started: false,
            closed: true,
            paused: false,
            resumed_at: f64::NEG_INFINITY,
            now: f64::NEG_INFINITY,
            last_arrival: f64::NEG_INFINITY,
            k: caps.k_default,
            samples: VecDeque::new(),
            pending: None,
            t_done: f64::NEG_INFINITY,
            band: Band::Adjusting,
            state: WhiteBalanceState::Starting,
            limit_since: None,
            batch: None,
            last_resume_at: f64::NEG_INFINITY,
            resume_out: None,
            limit_state_since: None,
            limit_stored: false,
            no_picture: None,
            retake: None,
            hold_reason: Reason::Dark,
            warmth_shift: 0.0,
            ff_store: false,
            ff_resume: false,
            slow: None,
            watchdog: Watchdog::new(Unit::Mired),
        }
    }

    pub fn engaged(&self) -> bool {
        self.mode_app && self.halt.is_none()
    }

    /// A write is in flight.
    pub fn busy(&self) -> bool {
        self.pending.is_some()
    }

    pub fn set_cfg(&mut self, cfg: WhiteBalanceCfg) {
        if (cfg.warmth - self.cfg.warmth).abs() > f64::EPSILON {
            // The error of the samples was measured against the old warmth.
            self.samples.clear();
            self.batch = None;
            self.slow = None;
            if self.engaged() && self.started {
                // A warmth change always moves the camera (10.4.3): the move is made by the next tick, whatever the
                // reading says, and the loop then measures from the narrow band.
                self.warmth_shift += warmth_target(cfg.warmth) - warmth_target(self.cfg.warmth);
                self.band = Band::Adjusting;
                self.limit_since = None;
                if matches!(self.state, WhiteBalanceState::Holding | WhiteBalanceState::Slow) {
                    self.state = WhiteBalanceState::Adjusting;
                }
            }
        }
        self.cfg = cfg;
    }

    /// See `ExposureLoop::set_mode`.
    pub fn set_mode(&mut self, mode: Mode, resume: Resume, seeded: bool, prior: Prior) {
        self.resume = resume;
        self.hub_seeded = seeded;
        self.prior = prior;
        if self.halt.is_some_and(|h| h.left_in != mode) {
            self.halt = None;
        }
        if mode == Mode::App {
            if !self.mode_app {
                self.mode_app = true;
                self.reset_session();
            }
        } else {
            self.mode_app = false;
        }
    }

    /// See `ExposureLoop::resume`.
    pub fn resume(&mut self) {
        self.halt = None;
        if self.mode_app {
            self.reset_session();
        }
    }

    pub fn reset_session(&mut self) {
        self.started = false;
        self.samples.clear();
        self.pending = None;
        self.retake = None;
        self.fault = None;
        self.t_done = f64::NEG_INFINITY;
        self.band = Band::Adjusting;
        self.state = WhiteBalanceState::Starting;
        self.limit_since = None;
        self.limit_state_since = None;
        self.limit_stored = false;
        self.batch = None;
        self.resumed_at = f64::NEG_INFINITY;
        self.last_arrival = f64::NEG_INFINITY;
        self.no_picture = None;
        // A new session measures against the new warmth anyway.
        self.warmth_shift = 0.0;
        self.ff_store = false;
        self.ff_resume = false;
        self.slow = None;
        self.watchdog.reset();
    }

    // -----------------------------------------------------------------------------------------------------------
    // Inputs

    pub fn on_sample(&mut self, stats: &Stats, capture_time: f64) {
        if !self.engaged() || self.paused || self.closed {
            return;
        }
        self.last_arrival = self.now;
        let r = measure_white_balance(stats, self.cfg.warmth);
        self.samples.push_back(Sample {
            t: capture_time,
            e: r.error,
            conf: r.confidence,
            reason: r.reason,
            light_conf: r.light_conf,
        });
        while self.samples.len() > MAX_SAMPLES {
            self.samples.pop_front();
        }
    }

    pub fn on_write_done(&mut self, w: WriteDone) {
        if w.key != ControlKey::WhiteBalance {
            return;
        }
        let Some(p) = self.pending.take() else { return };
        let expected = self.caps.snap(f64::from(w.requested));
        let matched = w.readback.is_none_or(|r| r == expected);
        if let Some(f) = self.watchdog.write_done(&w, matched) {
            self.raise(f);
        }
        if w.ok {
            self.k = w.readback.unwrap_or(w.requested);
            // The lane's average is kept as it will be once the move has taken effect: only a move that was made.
            if let Some(l) = &mut self.slow {
                l.e += p.lane_shift;
            }
            if p.ff != 0.0 {
                self.ff_store = true;
            }
        } else {
            self.k = p.prev;
            // The user's warmth change was not made: it is still to be written.
            self.warmth_shift += p.ff;
            if p.seed {
                self.started = false;
            }
        }
        self.t_done = self.t_done.max(w.done);
    }

    /// W6: the 1 Hz read-back of the white balance.
    pub fn on_readback(&mut self, info: &ControlInfo, now: f64) {
        if info.key != ControlKey::WhiteBalance
            || !self.engaged()
            || !self.started
            || self.closed
            || self.paused
            || self.pending.is_some()
        {
            return;
        }
        self.watchdog.read_ok();
        if !info.auto && info.value == self.k {
            return;
        }
        match self.watchdog.drift(now) {
            Drift::Retake => self.retake = Some(self.k),
            Drift::Conflict => self.raise(Fault::Conflict),
        }
    }

    pub fn on_read_failed(&mut self, now: f64) {
        if let Some(f) = self.watchdog.read_failed(now) {
            self.raise(f);
        }
    }

    fn raise(&mut self, f: Fault) {
        if self.halt.is_none() && self.fault.is_none() {
            self.fault = Some(f);
        }
    }

    fn flush_fault(&mut self, learned: &mut Learned) -> Option<Vec<Action>> {
        let fault = self.fault.take()?;
        self.pending = None;
        self.retake = None;
        self.batch = None;
        self.resume = Resume {
            white_balance: Some(self.k),
            ..self.resume
        };
        let mut actions = Vec::new();
        let mut to = None;
        if fault.hands_back() {
            let back = if self.caps.can_auto {
                HandBack::CameraAuto
            } else {
                HandBack::Manual(
                    self.prior
                        .white_balance
                        .unwrap_or(self.caps.k_default)
                        .clamp(self.caps.k_min, self.caps.k_max),
                )
            };
            actions.push(Action::HandBack {
                key: ControlKey::WhiteBalance,
                to: back,
            });
            to = Some(back);
            self.mode_app = false;
        }
        if fault == Fault::Ignored && !learned.blocked.contains(&ControlKey::WhiteBalance) {
            learned.blocked.push(ControlKey::WhiteBalance);
        }
        self.halt = Some(Halt::new(fault, to));
        Some(actions)
    }

    // -----------------------------------------------------------------------------------------------------------
    // The tick

    pub fn tick(
        &mut self,
        now: f64,
        facts: &SessionFacts,
        learned: &mut Learned,
        exposure: &ExposureView,
    ) -> Vec<Action> {
        self.now = now;
        if let Some(actions) = self.flush_fault(learned) {
            return actions;
        }
        if !self.engaged() {
            return Vec::new();
        }
        self.closed = facts.token.is_none();
        if self.closed {
            self.started = false;
            self.no_picture = None;
            return Vec::new();
        }
        if facts.paused {
            self.paused = true;
            self.limit_since = None;
            self.no_picture = None;
            return Vec::new();
        }
        if self.paused {
            self.paused = false;
            self.samples.clear();
            self.batch = None;
            self.slow = None;
            self.watchdog.touch(now);
        }
        self.resumed_at = facts.resumed_at;
        if !self.started {
            if now < self.watchdog.retry_at() || (exposure.engaged && exposure.busy) {
                return Vec::new();
            }
            return self.start(now);
        }
        if self.pending.is_some_and(|p| Watchdog::too_long(now, p.issued_at).is_some()) {
            self.raise(Fault::NotAnswering);
            return self.flush_fault(learned).unwrap_or_default();
        }
        let quiet = self.watchdog.quiet(now, self.last_arrival);
        self.no_picture = (quiet >= NO_PICTURE_AFTER).then_some(quiet);
        self.track_limit(now);
        self.store_feed_forward(now);
        if self.pending.is_some() || now < self.watchdog.retry_at() || self.no_picture.is_some() {
            return Vec::new();
        }
        // A warmth change is the user's move, not a measurement: it waits only for a write of the exposure loop that
        // is in flight, not for its quiet time, the gap or the reading (10.4.3).
        if self.warmth_shift != 0.0 && !(exposure.engaged && exposure.busy) {
            let actions = self.feed_forward(learned);
            if !actions.is_empty() {
                return actions;
            }
        }
        // Exposure first (2.6): one write in flight, none within 0.3 s of its batch, none while it is far off.
        if exposure.engaged
            && (exposure.busy || now < exposure.last_write + EXPOSURE_QUIET || exposure.unsettled)
        {
            return Vec::new();
        }
        if let Some(k) = self.retake.take() {
            self.batch = None;
            self.slow = None;
            return self.issue(k, false);
        }
        if self.watchdog.readback_due(now, self.t_done) {
            return vec![Action::ReadBack {
                key: ControlKey::WhiteBalance,
            }];
        }
        let pace = self.cfg.speed.white_balance_pace();
        if now < self.t_done + self.watchdog.gap(pace.gap) {
            return Vec::new();
        }
        let actions = self.decide(now, &pace, learned, exposure);
        if self.fault.is_some() {
            return self.flush_fault(learned).unwrap_or_default();
        }
        actions
    }

    /// The seed: `K0 = resume.white_balance ?? device default`, written as a manual value (3).
    pub fn seed(&self) -> i32 {
        self.resume
            .white_balance
            .unwrap_or(self.caps.k_default)
            .clamp(self.caps.k_min, self.caps.k_max)
    }

    fn start(&mut self, now: f64) -> Vec<Action> {
        let k0 = self.seed();
        self.started = true;
        self.state = WhiteBalanceState::Starting;
        self.band = Band::Adjusting;
        self.batch = None;
        self.warmth_shift = 0.0;
        self.slow = None;
        if self.hub_seeded {
            self.k = k0;
            self.t_done = now;
            return Vec::new();
        }
        self.issue(k0, true)
    }

    fn issue(&mut self, k: i32, seed: bool) -> Vec<Action> {
        self.issue_with(k, seed, 0.0, 0.0)
    }

    /// `ff` and `lane_shift` ride the write: see `Pending`.
    fn issue_with(&mut self, k: i32, seed: bool, ff: f64, lane_shift: f64) -> Vec<Action> {
        let prev = std::mem::replace(&mut self.k, k);
        self.pending = Some(Pending {
            prev,
            issued_at: self.now,
            seed,
            ff,
            lane_shift,
        });
        vec![Action::Write {
            key: ControlKey::WhiteBalance,
            value: k,
        }]
    }

    /// The valid samples: captured 0.3 s after the last write returned (longer for a slow camera) and after a
    /// resume.
    fn valid(&self) -> Vec<Sample> {
        let threshold = (self.t_done + D_APPLY + self.watchdog.slow()).max(self.resumed_at);
        self.samples.iter().filter(|s| s.t >= threshold).copied().collect()
    }

    fn reading(valid: &[Sample]) -> Option<Reading> {
        if valid.len() < 2 {
            return None;
        }
        let median = |f: fn(&Sample) -> f64| {
            let mut v: Vec<f64> = valid.iter().rev().take(MEDIAN_SAMPLES).map(f).collect();
            v.sort_by(f64::total_cmp);
            if v.len() == 2 {
                0.5 * (v[0] + v[1])
            } else {
                v[v.len() / 2]
            }
        };
        let tail: Vec<f64> = valid.iter().rev().take(SIGMA_SAMPLES).map(|s| s.e).collect();
        let mean = tail.iter().sum::<f64>() / tail.len() as f64;
        let sigma = (tail.iter().map(|e| (e - mean).powi(2)).sum::<f64>() / tail.len() as f64).sqrt();
        Some(Reading {
            e: median(|s| s.e),
            sigma,
            conf: median(|s| s.conf),
            light_conf: median(|s| s.light_conf),
            reason: valid
                .iter()
                .rev()
                .take(MEDIAN_SAMPLES)
                .map(|s| s.reason)
                .find(|r| *r != Reason::Ok)
                .unwrap_or(Reason::Ok),
        })
    }

    // -----------------------------------------------------------------------------------------------------------
    // The decision (2.6)

    fn decide(&mut self, now: f64, pace: &WbPace, learned: &mut Learned, exposure: &ExposureView) -> Vec<Action> {
        let valid = self.valid();
        let Some(r) = Self::reading(&valid) else {
            return Vec::new();
        };
        // An exposure write since our batch changed the picture too: it teaches nothing and proves nothing.
        if self.batch.is_some_and(|b| exposure.last_write > b.issued_at) {
            self.batch = None;
        }
        self.learn(&r, learned);
        match self.assess(now, &valid, &r) {
            Assess::Go => {}
            Assess::Wait => return Vec::new(),
            Assess::Fault(f) => {
                self.raise(f);
                return Vec::new();
            }
        }
        if r.conf >= HOLD_BELOW {
            if matches!(self.state, WhiteBalanceState::NoSignal | WhiteBalanceState::Slow) {
                // The signal is back: measure from the wide band again, as after a write.
                self.state = WhiteBalanceState::Adjusting;
                self.band = Band::Adjusting;
            }
            self.slow = None;
        } else if (r.reason != Reason::Dark && r.light_conf >= HOLD_BELOW)
            || self.slow.is_some_and(|l| now - l.evidence_at < SLOW_KEEP)
        {
            // Two answers, each sure: follow their middle slowly (10.4.2).
            return self.decide_slow(now, pace, learned, &valid);
        } else {
            // Dark, or one colour: keep the Kelvin and say so (it is not `holding`: nothing was checked). A closed
            // loop on a biased reading goes all the way to the wrong answer (D4). `r.reason` may be `TwoAnswers`
            // here (the median `light_conf` is low while a recent sample was doubtful): the text treats every reason
            // but `Dark` as one colour (`white_balance_message`).
            self.slow = None;
            self.state = WhiteBalanceState::NoSignal;
            self.hold_reason = r.reason;
            self.limit_since = None;
            return Vec::new();
        }
        let alpha = learned.wb_alpha.unwrap_or(ALPHA_INITIAL).clamp(ALPHA_MIN, ALPHA_MAX);
        let bands = self.watchdog.band_scale(now);
        let b_out = OUT_MIN.max(OUT_SIGMA * r.sigma) * bands;
        let b_in = IN_MIN.max(IN_SIGMA * r.sigma) * bands;
        let threshold = if self.band == Band::Holding { b_out } else { b_in };
        if r.e.abs() <= threshold {
            if r.e.abs() <= b_in {
                self.limit_since = None;
                self.enter_holding(now);
            }
            return Vec::new();
        }
        // `ΔM = clamp(-k·e/α, ±ΔM_max)`; `K' = round_to_step(10^6 / (M + ΔM))`.
        let k = pace.k * self.watchdog.k_scale(now);
        let dm = (-k * r.e / alpha).clamp(-pace.dm_max, pace.dm_max);
        let k2 = self.caps.snap(1e6 / (mired(self.k) + dm));
        if (k2 - self.k).abs() < self.caps.k_step {
            self.cannot_move(now, dm, &r, b_out);
            return Vec::new();
        }
        self.limit_since = None;
        let applied = mired(k2) - mired(self.k);
        if self.watchdog.moved(now, applied) == Moved::Hunting {
            self.raise(Fault::Hunting);
            return Vec::new();
        }
        self.batch = Some(Batch {
            issued_at: now,
            e_before: r.e,
            confident: r.conf >= LEARN_MIN_CONFIDENCE,
            predicted: alpha * applied,
            dm: applied.abs(),
            learned: false,
        });
        self.state = WhiteBalanceState::Adjusting;
        self.band = Band::Adjusting;
        // This move was measured against the new aim already: what is left of the user's move is in `e`.
        self.warmth_shift = 0.0;
        self.ff_store = false;
        self.ff_resume = false;
        self.issue(k2, false)
    }

    /// The slow lane (10.4.2): a picture with two answers does not hold the loop, it slows it. The lane is a
    /// confidence-weighted average of the middle of the two answers; the loop writes when the average passes the
    /// narrow band, then shifts it by the move's expected effect, so the next samples correct it. A sample that is
    /// dark or has no light of its own is no evidence (D4).
    fn decide_slow(&mut self, now: f64, pace: &WbPace, learned: &Learned, valid: &[Sample]) -> Vec<Action> {
        let mut lane = self.slow.unwrap_or(Slow {
            e: 0.0,
            t: valid[0].t,
            evidence_at: now,
        });
        // Whether this call took in evidence: the lane writes only on a fresh sample, not on what it kept for
        // `SLOW_KEEP`.
        let mut fresh = false;
        for s in valid {
            if s.t <= lane.t {
                continue;
            }
            let dt = (s.t - lane.t).min(SLOW_DT_MAX);
            lane.t = s.t;
            if s.reason == Reason::Dark || s.light_conf < HOLD_BELOW {
                continue;
            }
            fresh = true;
            lane.evidence_at = now;
            let w = SLOW_FLOOR + (1.0 - SLOW_FLOOR) * s.conf.clamp(0.0, 1.0);
            lane.e += (1.0 - (-w * dt / SLOW_TAU).exp()) * (s.e - lane.e);
        }
        self.slow = Some(lane);
        self.state = WhiteBalanceState::Slow;
        self.hold_reason = Reason::TwoAnswers;
        let bands = self.watchdog.band_scale(now);
        if lane.e.abs() <= OUT_MIN * bands {
            self.limit_since = None;
            return Vec::new();
        }
        // On a doubtful picture the lane under-moves rather than overshoots: the larger of the loop's `α` and the
        // blackbody slope, as the feed-forward does.
        let alpha = learned.wb_alpha.unwrap_or(ALPHA_INITIAL).max(ALPHA_FF);
        let k = pace.k * self.watchdog.k_scale(now);
        let dm = (-k * lane.e / alpha).clamp(-pace.dm_max, pace.dm_max);
        let k2 = self.caps.snap(1e6 / (mired(self.k) + dm).max(1.0));
        if (k2 - self.k).abs() < self.caps.k_step {
            // At an end of the range: `at_limit` after `LIMIT_AFTER`, else stay slow. Never `holding` here: that
            // means "checked and fine".
            let at_limit = if dm < 0.0 {
                self.k >= self.caps.k_max
            } else {
                self.k <= self.caps.k_min
            };
            if at_limit {
                let since = *self.limit_since.get_or_insert(now);
                if now - since >= LIMIT_AFTER {
                    self.state = WhiteBalanceState::AtLimit;
                }
            } else {
                self.limit_since = None;
            }
            return Vec::new();
        }
        if !fresh {
            return Vec::new();
        }
        self.limit_since = None;
        let applied = mired(k2) - mired(self.k);
        if self.watchdog.moved(now, applied) == Moved::Hunting {
            self.raise(Fault::Hunting);
            return Vec::new();
        }
        // A doubtful move teaches nothing and is not judged by W3.
        self.batch = None;
        self.ff_store = false;
        self.ff_resume = false;
        // `warmth_shift` stays: the lane starts from 0 at a warmth change and has not seen the user's move, so the
        // part of it not written yet (less than a camera step, or held back by `FF_GAP`) is still to be written.
        // The lane's average is shifted by this move's expected effect when the write has succeeded
        // (`Pending::lane_shift`).
        self.issue_with(k2, false, 0.0, alpha * applied)
    }

    /// The Kelvin the user's warmth change asks for, written at once: `ΔM = Δc*/α_ff` (a warmer aim lowers `c*`, so
    /// the move is toward a lower mired, a higher Kelvin). `α_ff` is the larger of the loop's `α` and the blackbody
    /// slope, so the move under-shoots rather than overshoots; a picture that can be measured finishes the rest from
    /// the narrow band. The move is not judged by W3 and is not a swing for W4: it is the user's.
    fn feed_forward(&mut self, learned: &Learned) -> Vec<Action> {
        if self.now < self.t_done + FF_GAP {
            // A drag of the slider: `warmth_shift` keeps adding up and the next write carries it.
            return Vec::new();
        }
        let alpha = learned.wb_alpha.unwrap_or(ALPHA_INITIAL).max(ALPHA_FF);
        let dm = self.warmth_shift / alpha;
        let k2 = self.caps.snap(1e6 / (mired(self.k) + dm).max(1.0));
        if k2 == self.k {
            // A drag in steps of 0.01 adds up until it is a step of the camera; at its end there is nothing more
            // the camera can do (the state `at_limit` says so when the picture can be measured).
            let at_end = if dm < 0.0 {
                self.k >= self.caps.k_max
            } else {
                self.k <= self.caps.k_min
            };
            if at_end {
                self.warmth_shift = 0.0;
            }
            return Vec::new();
        }
        // What the write carries leaves `warmth_shift`; the part the camera's step rounds away stays in it, so a drag
        // in many small writes adds up to the whole change. A failed write gives it back (`on_write_done`).
        let carried = alpha * (mired(k2) - mired(self.k));
        self.warmth_shift -= carried;
        self.batch = None;
        self.retake = None;
        self.slow = None;
        self.band = Band::Adjusting;
        self.limit_since = None;
        if matches!(
            self.state,
            WhiteBalanceState::Holding | WhiteBalanceState::AtLimit | WhiteBalanceState::Slow
        ) {
            self.state = WhiteBalanceState::Adjusting;
        }
        self.watchdog.forget_direction();
        self.issue_with(k2, false, carried, 0.0)
    }

    /// A warmth change made while the picture is held (`no_signal`, `slow`) is not stored by the loop's own rules, and
    /// would be lost when the camera reopens. Once the feed-forward has been quiet for `FF_STORE_QUIET` (the drag is
    /// over) and `RESUME_EVERY` allows, its Kelvin becomes the resume point.
    fn store_feed_forward(&mut self, now: f64) {
        if !self.ff_store
            || self.pending.is_some()
            || now < self.t_done + FF_STORE_QUIET
            || now - self.last_resume_at < RESUME_EVERY
        {
            return;
        }
        self.ff_store = false;
        self.ff_resume = true;
        self.last_resume_at = now;
        self.resume_out = Some(Resume {
            white_balance: Some(self.k),
            ..Resume::default()
        });
    }

    /// The move rounds to nothing: at the end of the range (`at_limit` after 3 s) or fine enough already.
    fn cannot_move(&mut self, now: f64, dm: f64, r: &Reading, b_out: f64) {
        let want_up = dm < 0.0;
        let at_limit = if want_up {
            self.k >= self.caps.k_max
        } else {
            self.k <= self.caps.k_min
        };
        if at_limit && r.e.abs() > b_out {
            let since = *self.limit_since.get_or_insert(now);
            if now - since >= LIMIT_AFTER {
                self.state = WhiteBalanceState::AtLimit;
            }
        } else {
            self.limit_since = None;
            self.enter_holding(now);
        }
    }

    fn enter_holding(&mut self, now: f64) {
        self.band = Band::Holding;
        if self.state != WhiteBalanceState::Holding {
            self.state = WhiteBalanceState::Holding;
            if now - self.last_resume_at >= RESUME_EVERY {
                self.last_resume_at = now;
                self.resume_out = Some(Resume {
                    white_balance: Some(self.k),
                    ..Resume::default()
                });
            }
        }
    }

    /// 2.5.11: also store the resume point after `LIMIT_STORE_AFTER` in `at_limit`, at most once per `RESUME_EVERY`.
    fn track_limit(&mut self, now: f64) {
        if self.state != WhiteBalanceState::AtLimit || self.no_picture.is_some() {
            self.limit_state_since = None;
            self.limit_stored = false;
            return;
        }
        let since = *self.limit_state_since.get_or_insert(now);
        if !self.limit_stored
            && self.pending.is_none()
            && now - since >= LIMIT_STORE_AFTER
            && now - self.last_resume_at >= RESUME_EVERY
        {
            self.last_resume_at = now;
            self.limit_stored = true;
            self.resume_out = Some(Resume {
                white_balance: Some(self.k),
                ..Resume::default()
            });
        }
    }

    /// Learn `α` from the batch before: `ρ = observed / predicted`, bounded and slow like `S`.
    fn learn(&mut self, r: &Reading, learned: &mut Learned) {
        let Some(b) = &mut self.batch else { return };
        if std::mem::replace(&mut b.learned, true) {
            return;
        }
        let b = *b;
        if !b.confident || r.conf < LEARN_MIN_CONFIDENCE || b.predicted.abs() < LEARN_MIN_PREDICTED {
            return;
        }
        let rho = (r.e - b.e_before) / b.predicted;
        if (LEARN_RHO.0..=LEARN_RHO.1).contains(&rho) {
            let a = learned.wb_alpha.unwrap_or(ALPHA_INITIAL);
            let next = a * rho.clamp(LEARN_CLAMP.0, LEARN_CLAMP.1).powf(LEARN_EXPONENT);
            learned.wb_alpha = Some(next.clamp(ALPHA_MIN, ALPHA_MAX));
        }
    }

    /// W3, as the exposure loop does it: 3 batches in a row of at least 15 mired that left `e_wb` where it was.
    fn assess(&mut self, now: f64, valid: &[Sample], r: &Reading) -> Assess {
        let Some(b) = self.batch else {
            return Assess::Go;
        };
        if !b.confident || r.conf < JUDGE_CONFIDENCE || b.dm < Unit::Mired.min_predicted() {
            // Not judged (`learn` has had its look).
            self.batch = None;
            return Assess::Go;
        }
        let ratio = |e: f64| (e - b.e_before) / b.predicted;
        if ratio(r.e) >= NO_EFFECT_RATIO {
            self.watchdog.responded();
            self.batch = None;
            return Assess::Go;
        }
        let good: Vec<&Sample> = valid.iter().filter(|s| s.conf >= JUDGE_CONFIDENCE).collect();
        let span = match (good.first(), good.last()) {
            (Some(a), Some(z)) => z.t - a.t,
            _ => 0.0,
        };
        if good.len() < 2 || span < NO_EFFECT_SPAN {
            if now - self.t_done > JUDGE_PATIENCE {
                self.batch = None;
                return Assess::Go;
            }
            return Assess::Wait;
        }
        let mut tail: Vec<f64> = good.iter().rev().take(MEDIAN_SAMPLES).map(|s| s.e).collect();
        tail.sort_by(f64::total_cmp);
        let observed = tail[tail.len() / 2];
        self.batch = None;
        if ratio(observed) >= NO_EFFECT_RATIO {
            self.watchdog.responded();
            return Assess::Go;
        }
        match self.watchdog.no_effect() {
            Some(f) => Assess::Fault(f),
            None => Assess::Go,
        }
    }

    // -----------------------------------------------------------------------------------------------------------
    // Output

    pub fn resume_report(&self) -> Option<Resume> {
        let reportable = match self.state {
            WhiteBalanceState::Holding => true,
            WhiteBalanceState::AtLimit => self.limit_stored,
            // The Kelvin of a warmth change on a held picture (`store_feed_forward`).
            WhiteBalanceState::NoSignal | WhiteBalanceState::Slow => self.ff_resume,
            _ => false,
        };
        if self.engaged() && reportable {
            self.resume_out
        } else {
            None
        }
    }

    pub fn status(&self, _learned: &Learned) -> Option<WhiteBalanceStatus> {
        if !self.mode_app && self.halt.is_none() {
            return None;
        }
        let halted = self.halt;
        let state = if let Some(h) = halted {
            match h.fault {
                Fault::NotAnswering | Fault::TooSlow => WhiteBalanceState::NotAnswering,
                Fault::Ignored => WhiteBalanceState::Ignored,
                Fault::NoResponse => WhiteBalanceState::NoResponse,
                Fault::Hunting => WhiteBalanceState::Hunting,
                Fault::Conflict => WhiteBalanceState::Conflict,
            }
        } else if self.closed {
            WhiteBalanceState::Closed
        } else if self.paused {
            WhiteBalanceState::Paused
        } else if self.no_picture.is_some() {
            WhiteBalanceState::NoPicture
        } else {
            self.state
        };
        let handed_back = halted.and_then(|h| h.to).map(Into::into);
        let message = white_balance_message(&WhiteBalanceFacts {
            state,
            kelvin: self.k,
            warm_limit: self.k <= (self.caps.k_min + self.caps.k_max) / 2,
            no_picture_for: self.no_picture.unwrap_or(0.0),
            reason: self.hold_reason,
            fault: halted.map(|h| h.fault),
            handed_back,
        });
        Some(WhiteBalanceStatus {
            state,
            kelvin: self.k,
            message,
            handed_back,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps() -> WhiteBalanceCaps {
        WhiteBalanceCaps {
            k_min: 2800,
            k_max: 6500,
            k_step: 1,
            k_default: 4600,
            can_auto: true,
        }
    }

    fn done(ok: bool, value: i32) -> WriteDone {
        WriteDone {
            key: ControlKey::WhiteBalance,
            requested: value,
            readback: ok.then_some(value),
            ok,
            started: 1.0,
            done: 1.01,
        }
    }

    fn pending(ff: f64, lane_shift: f64) -> Pending {
        Pending {
            prev: 4600,
            issued_at: 1.0,
            seed: false,
            ff,
            lane_shift,
        }
    }

    fn lane() -> Slow {
        Slow {
            e: 0.1,
            t: 1.0,
            evidence_at: 1.0,
        }
    }

    #[test]
    fn a_failed_write_gives_back_the_warmth_change_and_does_not_shift_the_lane() {
        let mut w = WhiteBalanceLoop::new(caps(), WhiteBalanceCfg::default());
        w.slow = Some(lane());
        w.warmth_shift = 0.01;
        w.pending = Some(pending(-0.03, -0.05));
        w.on_write_done(done(false, 4700));
        assert_eq!(w.k, 4600);
        assert!((w.warmth_shift - (0.01 - 0.03)).abs() < 1e-12, "{}", w.warmth_shift);
        assert!((w.slow.unwrap().e - 0.1).abs() < 1e-12);
        assert!(!w.ff_store);
    }

    #[test]
    fn a_successful_write_shifts_the_lane_and_keeps_the_warmth_change_written() {
        let mut w = WhiteBalanceLoop::new(caps(), WhiteBalanceCfg::default());
        w.slow = Some(lane());
        w.warmth_shift = 0.01;
        w.pending = Some(pending(-0.03, -0.05));
        w.on_write_done(done(true, 4700));
        assert_eq!(w.k, 4700);
        assert!((w.warmth_shift - 0.01).abs() < 1e-12);
        assert!((w.slow.unwrap().e - 0.05).abs() < 1e-12);
        assert!(w.ff_store);
    }
}
