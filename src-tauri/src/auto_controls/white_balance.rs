//! The white balance loop (docs/AUTO-CONTROLS.md 2.6): Kelvin worked in mired, step-and-settle like the exposure
//! loop, with exposure first.
//!
//! The picture's blue-red signal `c = ln(B/R)` follows `c ≈ α·(M − M_light)` (`M = 10^6 / K`): a setting warmer than
//! the light (higher K, lower M) leaves the picture orange. The loop measures `e_wb = c - c*` (`c*` from the warmth,
//! Auto Adjust's scale, see `meter::warmth_target`), moves `ΔM = -k·e_wb/α` at most `ΔM_max` per decision and waits
//! until a frame captured 0.3 s after the write has been measured.
//!
//! Pure like the exposure loop; the watchdog (`watchdog.rs`) is part of it.

use std::collections::VecDeque;

use super::exposure::ExposureView;
use super::meter::measure_white_balance;
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
const ALPHA_MIN: f64 = 0.002;
const ALPHA_MAX: f64 = 0.02;
/// A sample is valid when captured this long after the write returned.
const D_APPLY: f64 = 0.3;
/// Hold (no move) while the confidence is below this: a scene of one colour, a dark scene.
const HOLD_BELOW: f64 = 0.3;
/// Band: `B_out = max(OUT_MIN, OUT_SIGMA·σ)`, `B_in = max(IN_MIN, IN_SIGMA·σ)`.
const OUT_MIN: f64 = 0.04;
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
}

#[derive(Clone, Copy, Debug)]
struct Pending {
    prev: i32,
    issued_at: f64,
    seed: bool,
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
    no_picture: Option<f64>,
    retake: Option<i32>,
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
            no_picture: None,
            retake: None,
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
        self.batch = None;
        self.resumed_at = f64::NEG_INFINITY;
        self.last_arrival = f64::NEG_INFINITY;
        self.no_picture = None;
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
        } else {
            self.k = p.prev;
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
        if self.pending.is_some() || now < self.watchdog.retry_at() || self.no_picture.is_some() {
            return Vec::new();
        }
        // Exposure first (2.6): one write in flight, none within 0.3 s of its batch, none while it is far off.
        if exposure.engaged
            && (exposure.busy || now < exposure.last_write + EXPOSURE_QUIET || exposure.unsettled)
        {
            return Vec::new();
        }
        if let Some(k) = self.retake.take() {
            self.batch = None;
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
    fn start(&mut self, now: f64) -> Vec<Action> {
        let k0 = self
            .resume
            .white_balance
            .unwrap_or(self.caps.k_default)
            .clamp(self.caps.k_min, self.caps.k_max);
        self.started = true;
        self.state = WhiteBalanceState::Starting;
        self.band = Band::Adjusting;
        self.batch = None;
        if self.hub_seeded {
            self.k = k0;
            self.t_done = now;
            return Vec::new();
        }
        self.issue(k0, true)
    }

    fn issue(&mut self, k: i32, seed: bool) -> Vec<Action> {
        let prev = std::mem::replace(&mut self.k, k);
        self.pending = Some(Pending {
            prev,
            issued_at: self.now,
            seed,
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
        if r.conf < HOLD_BELOW {
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
        self.issue(k2, false)
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
        if self.engaged() && self.state == WhiteBalanceState::Holding {
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
