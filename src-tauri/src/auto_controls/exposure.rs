//! The exposure loop (docs/AUTO-CONTROLS.md 2.5): exposure and gain as one quantity in stops, step-and-settle.
//!
//! One move, wait until a frame captured after the write has been measured (2.5.4), decide again. Exposure prefers the
//! longest time within the frame time (`E_rate`), gain fills the rest up to `Most gain`.
//!
//! The loop is pure: [`ExposureLoop::tick`] returns [`Action`]s; samples, write results and the clock come in as
//! calls. See `mod.rs` for how the runner drives it.

use std::collections::VecDeque;

use super::meter::{measure_exposure, ExposureReading};
use super::status::{exposure_message, ExposureFacts};
use super::{
    Action, CapReason, ExposureCfg, ExposureState, ExposureStatus, Learned, Resume, SessionFacts, Speed,
    WriteDone,
};
use crate::capture::controls::{ControlInfo, ControlKey};
use crate::fx::auto_adjust::Stats;

// ---------------------------------------------------------------------------------------------------------------
// Constants (2.5, exactly as written in the doc)

/// `S`: stops across the whole gain range, until learned. Unverified for the W4GS (3 stops is common for UVC).
const S_INITIAL: f64 = 3.0;
const S_MIN: f64 = 0.5;
const S_MAX: f64 = 6.0;
/// `E_rate = floor(log2(RATE_FILL / F))`.
const RATE_FILL: f64 = 0.95;
/// `D_apply = D_APPLY_BASE + 1/F`: the camera applies a write (~0.1 s measured) plus a frame.
const D_APPLY_BASE: f64 = 0.15;
const MAX_SAMPLES: usize = 16;
/// Samples that count for `sigma`.
const SIGMA_SAMPLES: usize = 8;
/// Samples that count for the median `ê`.
const MEDIAN_SAMPLES: usize = 3;
const NO_SIGNAL_CONFIDENCE: f64 = 0.2;
const NO_SIGNAL_AFTER: f64 = 1.5;
const NO_SIGNAL_EVERY: f64 = 1.0;
const CLIP_GUARD: f64 = 0.5;
/// Band, in stops: `B_out = max(OUT_MIN, OUT_SIGMA·σ, OUT_Q·q)`, `B_in = max(IN_MIN, IN_SIGMA·σ, IN_Q·q)`.
const OUT_MIN: f64 = 0.33;
const OUT_SIGMA: f64 = 3.0;
const OUT_Q: f64 = 0.6;
const IN_MIN: f64 = 0.12;
const IN_SIGMA: f64 = 1.5;
const IN_Q: f64 = 0.5;
/// Crossover thresholds (2.5.6).
const R_HI_MAX: f64 = 1.25;
const R_LAND_MAX: f64 = 0.25;
/// Default `Most gain`: this share of the range.
const DEFAULT_MAX_GAIN: f64 = 0.75;
/// How long the picture must want more than the limits allow before the limit state shows (2.5.9).
const LIMIT_AFTER: f64 = 3.0;
/// Learning the gain curve (2.5.7).
const LEARN_MIN_STEP: f64 = 0.2;
const LEARN_MIN_CONFIDENCE: f64 = 0.8;
const LEARN_MAX_CLIP: f64 = 0.05;
const LEARN_RHO: (f64, f64) = (0.25, 4.0);
const LEARN_CLAMP: (f64, f64) = (0.7, 1.4);
const LEARN_EXPONENT: f64 = 0.3;
/// The frame-rate guard (2.5.8).
const GUARD_LOW: f64 = 0.9;
const GUARD_RECOVERED: f64 = 0.95;
const GUARD_QUIET: f64 = 1.0;
const GUARD_LOW_FOR: f64 = 3.0;
const GUARD_VERIFY: f64 = 3.0;
/// The resume point is reported at most this often (2.5.11).
const RESUME_EVERY: f64 = 10.0;

/// The response presets of 2.5.4 (exposure columns).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pace {
    pub k: f64,
    pub step_max: f64,
    pub gap: f64,
}

impl Speed {
    pub fn exposure_pace(self) -> Pace {
        match self {
            Speed::Slow => Pace {
                k: 0.4,
                step_max: 0.5,
                gap: 0.5,
            },
            Speed::Normal => Pace {
                k: 0.7,
                step_max: 1.0,
                gap: 0.25,
            },
            Speed::Fast => Pace {
                k: 0.9,
                step_max: 2.0,
                gap: 0.15,
            },
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Device facts

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GainCaps {
    pub min: i32,
    pub max: i32,
    pub step: i32,
}

/// What the loop needs to know about the camera's exposure and gain controls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExposureCaps {
    pub e_min: i32,
    pub e_max: i32,
    pub e_default: i32,
    pub gain: Option<GainCaps>,
}

impl ExposureCaps {
    /// `None` when the camera has no manual exposure. Gain is used only when it is manual and has a range.
    pub fn from_controls(caps: &[ControlInfo]) -> Option<ExposureCaps> {
        let ex = caps
            .iter()
            .find(|c| c.key == ControlKey::Exposure && c.can_manual)?;
        let gain = caps
            .iter()
            .find(|c| c.key == ControlKey::Gain && c.can_manual && c.max > c.min)
            .map(|c| GainCaps {
                min: c.min,
                max: c.max,
                step: c.step.max(1),
            });
        Some(ExposureCaps {
            e_min: ex.min.min(ex.max),
            e_max: ex.max.max(ex.min),
            e_default: ex.default,
            gain,
        })
    }
}

/// The frame-rate key of `Learned::rate_caps`.
pub fn rate_key(fps: f64) -> String {
    format!("{}", fps.round() as i64)
}

/// `E_rate` (2.5.2).
pub fn rate_limit(fps: f64) -> i32 {
    (RATE_FILL / fps.max(1.0)).log2().floor() as i32
}

/// The limits for the current settings, frame rate and learned values (2.5.2).
#[derive(Clone, Copy, Debug)]
struct Limits {
    e_cap: i32,
    reason: CapReason,
    /// Gain is used (exists, manual, `Most gain` above its minimum).
    gain_usable: bool,
    g_cap: i32,
    gs_cap: f64,
    /// `S` now.
    s: f64,
}

// ---------------------------------------------------------------------------------------------------------------
// State

#[derive(Clone, Copy, Debug)]
struct Sample {
    /// Capture time.
    t: f64,
    e: f64,
    conf: f64,
    clip: f64,
}

#[derive(Clone, Copy, Debug)]
struct Pending {
    key: ControlKey,
    prev: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Band {
    Holding,
    Adjusting,
}

/// A gain-only batch whose effect teaches `S` (2.5.7).
#[derive(Clone, Copy, Debug)]
struct Probe {
    e_before: f64,
    /// The change in stops the model predicted.
    dgs: f64,
}

#[derive(Clone, Debug)]
enum Guard {
    /// Watching the capture rate while exposure sits at the frame-rate limit.
    Watching { low_since: Option<f64> },
    /// The learned cap is set; waiting until exposure is at it.
    Applying { e_new: i32 },
    /// Exposure is at the learned cap; the rate must recover before the deadline.
    Verifying { key: String, deadline: f64 },
    Done,
}

/// What the loop makes of the samples it may use now.
#[derive(Clone, Copy, Debug)]
struct Reading {
    e: f64,
    sigma: f64,
    conf: f64,
    clip: f64,
}

pub struct ExposureLoop {
    caps: ExposureCaps,
    cfg: ExposureCfg,
    engaged: bool,
    resume: Resume,
    hub_seeded: bool,
    /// The seed is written (or assumed written).
    started: bool,
    closed: bool,
    paused: bool,
    f: f64,
    capture_fps: f64,
    /// Samples captured before this are invalid (a resume).
    resumed_at: f64,
    /// The exposure and gain the camera has now, as far as the loop knows.
    e: i32,
    g: i32,
    samples: VecDeque<Sample>,
    pending: Vec<Pending>,
    /// When the last write batch returned.
    t_done: f64,
    last_write_done: f64,
    band: Band,
    state: ExposureState,
    no_signal_since: Option<f64>,
    last_no_signal_move: f64,
    limit_since: Option<f64>,
    dark_since: Option<f64>,
    probe: Option<Probe>,
    guard: Guard,
    guard_spent: bool,
    /// The rate the camera dropped to when the cap was learned (for the status text).
    dropped_to: Option<f64>,
    last_resume_at: f64,
    resume_out: Option<Resume>,
}

impl ExposureLoop {
    pub fn new(caps: ExposureCaps, cfg: ExposureCfg) -> ExposureLoop {
        ExposureLoop {
            caps,
            cfg,
            engaged: false,
            resume: Resume::default(),
            hub_seeded: false,
            started: false,
            closed: true,
            paused: false,
            f: 30.0,
            capture_fps: 0.0,
            resumed_at: f64::NEG_INFINITY,
            e: caps.e_default.clamp(caps.e_min, caps.e_max),
            g: caps.gain.map_or(0, |g| g.min),
            samples: VecDeque::new(),
            pending: Vec::new(),
            t_done: f64::NEG_INFINITY,
            last_write_done: f64::NEG_INFINITY,
            band: Band::Adjusting,
            state: ExposureState::Starting,
            no_signal_since: None,
            last_no_signal_move: f64::NEG_INFINITY,
            limit_since: None,
            dark_since: None,
            probe: None,
            guard: Guard::Watching { low_since: None },
            guard_spent: false,
            dropped_to: None,
            last_resume_at: f64::NEG_INFINITY,
            resume_out: None,
        }
    }

    pub fn engaged(&self) -> bool {
        self.engaged
    }

    pub fn set_cfg(&mut self, cfg: ExposureCfg) {
        if (cfg.target - self.cfg.target).abs() > f64::EPSILON {
            // The error of the samples was measured against the old target.
            self.samples.clear();
        }
        if cfg.longest != self.cfg.longest || cfg.max_gain != self.cfg.max_gain {
            self.limit_since = None;
            self.dark_since = None;
        }
        self.cfg = cfg;
    }

    pub fn set_engaged(&mut self, on: bool, resume: Resume, seeded: bool) {
        self.resume = resume;
        self.hub_seeded = seeded;
        if on && !self.engaged {
            self.engaged = true;
            self.reset_session();
        } else if !on {
            self.engaged = false;
        }
    }

    /// A new camera session (or the first): seed again, forget the measurements.
    pub fn reset_session(&mut self) {
        self.started = false;
        self.samples.clear();
        self.pending.clear();
        self.t_done = f64::NEG_INFINITY;
        self.last_write_done = f64::NEG_INFINITY;
        self.band = Band::Adjusting;
        self.state = ExposureState::Starting;
        self.no_signal_since = None;
        self.last_no_signal_move = f64::NEG_INFINITY;
        self.limit_since = None;
        self.dark_since = None;
        self.probe = None;
        self.guard = Guard::Watching { low_since: None };
        self.guard_spent = false;
        self.dropped_to = None;
        self.resumed_at = f64::NEG_INFINITY;
    }

    // -----------------------------------------------------------------------------------------------------------
    // The model

    fn gs_with(&self, g: i32, s: f64) -> f64 {
        match self.caps.gain {
            Some(gc) => s * f64::from(g - gc.min) / f64::from(gc.max - gc.min),
            None => 0.0,
        }
    }

    /// The gain setting for `stops` of gain, rounded to its step and kept in the device range.
    fn ginv(&self, stops: f64, s: f64) -> i32 {
        let Some(gc) = self.caps.gain else { return 0 };
        let units = stops / s * f64::from(gc.max - gc.min);
        let steps = (units / f64::from(gc.step)).round() as i32;
        (gc.min + steps * gc.step).clamp(gc.min, gc.max)
    }

    /// Total light in stops: `E + gs(G)`.
    fn x(&self, s: f64) -> f64 {
        f64::from(self.e) + self.gs_with(self.g, s)
    }

    fn limits(&self, learned: &Learned) -> Limits {
        let s = learned.gain_stops.unwrap_or(S_INITIAL).clamp(S_MIN, S_MAX);
        let (mut e_cap, mut reason) = match self.cfg.longest {
            super::Longest::Fit => (rate_limit(self.f), CapReason::Fps),
            super::Longest::Stop(n) => (n, CapReason::User),
        };
        if self.cfg.longest == super::Longest::Fit {
            if let Some(&l) = learned.rate_caps.get(&rate_key(self.f)) {
                if l < e_cap {
                    e_cap = l;
                    reason = CapReason::Learned;
                }
            }
        }
        let e_cap = e_cap.clamp(self.caps.e_min, self.caps.e_max);
        let (g_cap, gain_usable) = match self.caps.gain {
            Some(gc) => {
                let range = f64::from(gc.max - gc.min);
                let cap = match self.cfg.max_gain {
                    Some(m) => m.clamp(gc.min, gc.max),
                    None => gc.min + (DEFAULT_MAX_GAIN * range).round() as i32,
                };
                (cap, cap > gc.min)
            }
            None => (0, false),
        };
        let gs_cap = if gain_usable {
            self.gs_with(g_cap, s)
        } else {
            0.0
        };
        Limits {
            e_cap,
            reason,
            gain_usable,
            g_cap,
            gs_cap,
            s,
        }
    }

    /// 2.5.6, gain mode: `X'` to `(E', G')`.
    fn allocate(&self, x2: f64, lim: &Limits) -> (i32, i32) {
        let r_hi = R_HI_MAX.min(lim.gs_cap);
        let r_land = R_LAND_MAX.min(r_hi / 5.0);
        let r = x2 - f64::from(self.e);
        let e2 = if self.e > lim.e_cap {
            lim.e_cap
        } else if r > r_hi && self.e < lim.e_cap {
            lim.e_cap.min((x2 - r_land).floor() as i32)
        } else if r < 0.0 && self.e > self.caps.e_min {
            self.caps.e_min.max((x2 - r_land).floor() as i32)
        } else {
            self.e
        };
        let r2 = x2 - f64::from(e2);
        let g2 = match self.caps.gain {
            Some(gc) if lim.gain_usable => self.ginv(r2.clamp(0.0, lim.gs_cap), lim.s).clamp(gc.min, lim.g_cap),
            Some(gc) => gc.min,
            None => 0,
        };
        (e2, g2)
    }

    /// The smallest step that can move toward `up`/down (2.5.5 step 3); `None` when nothing has room.
    fn quantum(&self, up: bool, lim: &Limits) -> Option<f64> {
        if let (Some(gc), true) = (self.caps.gain, lim.gain_usable) {
            let room = if up { self.g < lim.g_cap } else { self.g > gc.min };
            if room {
                return Some(lim.s * f64::from(gc.step) / f64::from(gc.max - gc.min));
            }
        }
        let room = if up {
            self.e < lim.e_cap
        } else {
            self.e > self.caps.e_min
        };
        room.then_some(1.0)
    }

    /// The most light the limits allow, and the least.
    fn x_range(&self, lim: &Limits) -> (f64, f64) {
        let top = f64::from(lim.e_cap) + lim.gs_cap;
        let bottom = f64::from(self.caps.e_min);
        (bottom, top)
    }

    // -----------------------------------------------------------------------------------------------------------
    // Inputs

    pub fn on_sample(&mut self, stats: &Stats, capture_time: f64) {
        if !self.engaged || self.paused || self.closed {
            return;
        }
        let ExposureReading {
            error,
            confidence,
            clip,
        } = measure_exposure(stats, self.cfg.target);
        self.samples.push_back(Sample {
            t: capture_time,
            e: error,
            conf: confidence,
            clip,
        });
        while self.samples.len() > MAX_SAMPLES {
            self.samples.pop_front();
        }
    }

    pub fn on_write_done(&mut self, w: WriteDone) {
        let Some(at) = self.pending.iter().position(|p| p.key == w.key) else {
            return;
        };
        let p = self.pending.remove(at);
        if w.ok {
            // A watchdog (A2) compares `readback` with `requested`; the model believes the device.
            let v = w.readback.unwrap_or(w.requested);
            match w.key {
                ControlKey::Exposure => self.e = v,
                ControlKey::Gain => self.g = v,
                _ => {}
            }
            self.last_write_done = self.last_write_done.max(w.done);
        } else {
            match w.key {
                ControlKey::Exposure => self.e = p.prev,
                ControlKey::Gain => self.g = p.prev,
                _ => {}
            }
        }
        self.t_done = self.t_done.max(w.done);
    }

    // -----------------------------------------------------------------------------------------------------------
    // The tick

    pub fn tick(&mut self, now: f64, facts: &SessionFacts, learned: &mut Learned) -> Vec<Action> {
        self.f = facts.target_fps.max(1.0);
        self.capture_fps = facts.capture_fps;
        if !self.engaged {
            return Vec::new();
        }
        self.closed = facts.token.is_none();
        if self.closed {
            self.started = false;
            return Vec::new();
        }
        if facts.paused {
            self.paused = true;
            self.no_signal_since = None;
            self.limit_since = None;
            self.dark_since = None;
            return Vec::new();
        }
        if self.paused {
            // Resumed: what was measured before says nothing about now.
            self.paused = false;
            self.samples.clear();
        }
        self.resumed_at = facts.resumed_at;
        if !self.started {
            let lim = self.limits(learned);
            return self.start(now, &lim);
        }
        self.guard_step(now, learned);
        if !self.pending.is_empty() {
            return Vec::new();
        }
        let pace = self.cfg.speed.exposure_pace();
        if now < self.t_done + pace.gap {
            return Vec::new();
        }
        let lim = self.limits(learned);
        self.decide(now, &lim, &pace, learned)
    }

    /// 2.5.11: the seed. `E0 = min(E_cap, resume.exposure ?? device default)`, `G0 = resume.gain ?? G_min`. The
    /// device's reported value is never used (stale while Camera auto is on).
    fn start(&mut self, now: f64, lim: &Limits) -> Vec<Action> {
        let e0 = lim
            .e_cap
            .min(self.resume.exposure.unwrap_or(self.caps.e_default))
            .clamp(self.caps.e_min, self.caps.e_max);
        let g0 = match self.caps.gain {
            Some(gc) if lim.gain_usable => self
                .resume
                .gain
                .unwrap_or(gc.min)
                .clamp(gc.min, lim.g_cap),
            Some(gc) => gc.min,
            None => 0,
        };
        self.started = true;
        self.state = ExposureState::Starting;
        self.band = Band::Adjusting;
        if self.hub_seeded {
            self.e = e0;
            self.g = g0;
            self.t_done = now;
            return Vec::new();
        }
        let mut writes = vec![(ControlKey::Exposure, e0)];
        if self.caps.gain.is_some() {
            writes.push((ControlKey::Gain, g0));
        }
        self.issue(writes)
    }

    /// Record the writes as pending and believe them; the runner reports each result.
    fn issue(&mut self, writes: Vec<(ControlKey, i32)>) -> Vec<Action> {
        let mut out = Vec::with_capacity(writes.len());
        for (key, value) in writes {
            let prev = match key {
                ControlKey::Exposure => std::mem::replace(&mut self.e, value),
                _ => std::mem::replace(&mut self.g, value),
            };
            self.pending.push(Pending { key, prev });
            out.push(Action::Write { key, value });
        }
        out
    }

    /// The writes that take `(e, g)` to `(e2, g2)`, the one that darkens the picture first (2.5.5 step 7).
    fn plan(&self, e2: i32, g2: i32, s: f64) -> Vec<(ControlKey, i32, f64)> {
        let mut v = Vec::with_capacity(2);
        if e2 != self.e {
            v.push((ControlKey::Exposure, e2, f64::from(e2 - self.e)));
        }
        if self.caps.gain.is_some() && g2 != self.g {
            v.push((ControlKey::Gain, g2, self.gs_with(g2, s) - self.gs_with(self.g, s)));
        }
        v.sort_by(|a, b| a.2.total_cmp(&b.2));
        v
    }

    /// Issue a plan; `None` when it changes nothing.
    fn move_to(&mut self, e2: i32, g2: i32, lim: &Limits, probe: Option<Probe>) -> Option<Vec<Action>> {
        let plan = self.plan(e2, g2, lim.s);
        if plan.is_empty() {
            return None;
        }
        let gain_only = plan.len() == 1 && plan[0].0 == ControlKey::Gain;
        self.probe = probe.filter(|p| gain_only && p.dgs.abs() >= LEARN_MIN_STEP);
        self.state = ExposureState::Adjusting;
        self.band = Band::Adjusting;
        Some(self.issue(plan.into_iter().map(|(k, v, _)| (k, v)).collect()))
    }

    /// The valid samples: captured after the last write landed (`D_apply`) and after a resume (2.5.4).
    fn valid(&self) -> Vec<Sample> {
        let threshold = (self.t_done + D_APPLY_BASE + 1.0 / self.f).max(self.resumed_at);
        self.samples.iter().filter(|s| s.t >= threshold).copied().collect()
    }

    fn reading(valid: &[Sample]) -> Option<Reading> {
        if valid.len() < 2 {
            return None;
        }
        let last: Vec<&Sample> = valid.iter().rev().take(MEDIAN_SAMPLES).collect();
        let median = |f: fn(&Sample) -> f64| {
            let mut v: Vec<f64> = last.iter().map(|s| f(s)).collect();
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
            clip: median(|s| s.clip),
        })
    }

    // -----------------------------------------------------------------------------------------------------------
    // The decision (2.5.5)

    fn decide(&mut self, now: f64, lim: &Limits, pace: &Pace, learned: &mut Learned) -> Vec<Action> {
        let s = lim.s;
        // A constraint (frame rate or the user's limit changed): move to the cap, brightness-neutral, no samples
        // needed and not bounded by `step_max`.
        if self.e > lim.e_cap {
            let (e2, g2) = self.allocate(self.x(s), lim);
            return self.move_to(e2, g2, lim, None).unwrap_or_default();
        }
        let valid = self.valid();
        let Some(r) = Self::reading(&valid) else {
            return Vec::new();
        };
        self.learn(&r, learned);
        let s = learned.gain_stops.unwrap_or(S_INITIAL).clamp(S_MIN, S_MAX);
        let lim = &Limits {
            s,
            gs_cap: if lim.gain_usable { self.gs_with(lim.g_cap, s) } else { 0.0 },
            ..*lim
        };

        // 1. No signal.
        let window = &valid[valid.len().saturating_sub(SIGMA_SAMPLES)..];
        if window.iter().all(|v| v.conf < NO_SIGNAL_CONFIDENCE) {
            let since = *self.no_signal_since.get_or_insert(now);
            if now - since >= NO_SIGNAL_AFTER {
                self.state = ExposureState::NoSignal;
                if now - self.last_no_signal_move >= NO_SIGNAL_EVERY {
                    if let Some(actions) = self.step_by(1.0, lim, None) {
                        self.last_no_signal_move = now;
                        self.state = ExposureState::NoSignal;
                        return actions;
                    }
                }
            }
            return Vec::new();
        }
        self.no_signal_since = None;

        // 2. Highlight guard.
        if r.clip >= CLIP_GUARD {
            self.limit_since = None;
            if let Some(actions) = self.step_by(-pace.step_max, lim, None) {
                self.dark_since = None;
                return actions;
            }
            self.saturated_dark(now, lim, &r);
            return Vec::new();
        }

        // 3. Quantum.
        let up = r.e > 0.0;
        let Some(q) = self.quantum(up, lim) else {
            self.saturated(now, lim, &r, up);
            return Vec::new();
        };
        // 4. Band.
        let b_out = OUT_MIN.max(OUT_SIGMA * r.sigma).max(OUT_Q * q);
        let b_in = IN_MIN.max(IN_SIGMA * r.sigma).max(IN_Q * q);
        let threshold = if self.band == Band::Holding { b_out } else { b_in };
        if r.e.abs() <= threshold {
            if r.e.abs() <= b_in {
                self.limit_since = None;
                self.dark_since = None;
                self.enter_holding(now);
            }
            return Vec::new();
        }
        // Does the whole correction still fit inside the limits? If not, the limit timer starts now.
        let (x_bottom, x_top) = self.x_range(lim);
        let x = self.x(s);
        if r.e > 0.0 && x + r.e >= x_top {
            self.limit_since.get_or_insert(now);
            self.dark_since = None;
        } else if r.e < 0.0 && x + r.e <= x_bottom {
            self.dark_since.get_or_insert(now);
            self.limit_since = None;
        } else {
            self.limit_since = None;
            self.dark_since = None;
        }

        // 5-6. Move and allocate.
        let (e2, g2) = if lim.gain_usable {
            let mut dx = (pace.k * r.e).clamp(-pace.step_max, pace.step_max);
            if dx.abs() < q {
                dx = r.e.signum() * q;
            }
            self.allocate(x + dx, lim)
        } else {
            // Whole stops only (2.5.6).
            let n = ((pace.k * r.e.abs()).round() as i32).clamp(1, (pace.step_max.floor() as i32).max(1));
            let e2 = (self.e + r.e.signum() as i32 * n).clamp(self.caps.e_min, lim.e_cap);
            (e2, self.g)
        };
        let probe = (r.conf >= LEARN_MIN_CONFIDENCE && r.clip < LEARN_MAX_CLIP).then(|| Probe {
            e_before: r.e,
            dgs: self.gs_with(g2, s) - self.gs_with(self.g, s),
        });
        match self.move_to(e2, g2, lim, probe) {
            Some(actions) => actions,
            None => {
                self.enter_holding(now);
                Vec::new()
            }
        }
    }

    /// A move of `dx` stops (no signal, highlight guard). `None` when nothing has room.
    fn step_by(&mut self, dx: f64, lim: &Limits, probe: Option<Probe>) -> Option<Vec<Action>> {
        let up = dx > 0.0;
        self.quantum(up, lim)?;
        let (e2, g2) = if lim.gain_usable {
            self.allocate(self.x(lim.s) + dx, lim)
        } else {
            let n = (dx.abs().ceil() as i32).max(1);
            (
                (self.e + dx.signum() as i32 * n).clamp(self.caps.e_min, lim.e_cap),
                self.g,
            )
        };
        self.move_to(e2, g2, lim, probe)
    }

    /// Nothing has room in the direction the picture needs: saturation (2.5.9). Not a fault; the loop stays engaged.
    fn saturated(&mut self, now: f64, lim: &Limits, r: &Reading, up: bool) {
        let b_out = OUT_MIN.max(OUT_SIGMA * r.sigma);
        let b_in = IN_MIN.max(IN_SIGMA * r.sigma);
        if r.e.abs() <= b_in {
            self.limit_since = None;
            self.dark_since = None;
            self.enter_holding(now);
        } else if up && r.e > b_out {
            self.dark_since = None;
            let since = *self.limit_since.get_or_insert(now);
            if now - since >= LIMIT_AFTER {
                self.state = if lim.gain_usable {
                    ExposureState::LimitGain
                } else {
                    ExposureState::LimitExposure
                };
            }
        } else if !up && r.e < -b_out {
            self.saturated_dark(now, lim, r);
        }
    }

    fn saturated_dark(&mut self, now: f64, _lim: &Limits, _r: &Reading) {
        self.limit_since = None;
        let since = *self.dark_since.get_or_insert(now);
        if now - since >= LIMIT_AFTER {
            self.state = ExposureState::TooBright;
        }
    }

    fn enter_holding(&mut self, now: f64) {
        self.band = Band::Holding;
        if self.state != ExposureState::Holding {
            self.state = ExposureState::Holding;
            if now - self.last_resume_at >= RESUME_EVERY {
                self.last_resume_at = now;
                self.resume_out = Some(Resume {
                    exposure: Some(self.e),
                    gain: self.caps.gain.map(|_| self.g),
                    white_balance: None,
                });
            }
        }
    }

    /// 2.5.7: a gain-only batch tells how many stops the gain really gave.
    fn learn(&mut self, r: &Reading, learned: &mut Learned) {
        let Some(p) = self.probe.take() else { return };
        if r.conf < LEARN_MIN_CONFIDENCE || r.clip >= LEARN_MAX_CLIP {
            return;
        }
        let rho = (p.e_before - r.e) / p.dgs;
        if (LEARN_RHO.0..=LEARN_RHO.1).contains(&rho) {
            let s = learned.gain_stops.unwrap_or(S_INITIAL);
            let next = s * rho.clamp(LEARN_CLAMP.0, LEARN_CLAMP.1).powf(LEARN_EXPONENT);
            learned.gain_stops = Some(next.clamp(S_MIN, S_MAX));
        }
    }

    // -----------------------------------------------------------------------------------------------------------
    // The frame-rate guard (2.5.8)

    fn guard_step(&mut self, now: f64, learned: &mut Learned) {
        let lim = self.limits(learned);
        let key = rate_key(self.f);
        match &mut self.guard {
            Guard::Watching { low_since } => {
                let eligible = !self.guard_spent
                    && self.cfg.longest == super::Longest::Fit
                    && lim.reason == CapReason::Fps
                    && self.e == lim.e_cap
                    && self.pending.is_empty()
                    && now - self.last_write_done >= GUARD_QUIET
                    && self.capture_fps < GUARD_LOW * self.f;
                if !eligible {
                    *low_since = None;
                    return;
                }
                let since = *low_since.get_or_insert(now);
                if now - since < GUARD_LOW_FOR {
                    return;
                }
                if lim.e_cap - 1 < self.caps.e_min {
                    self.guard_spent = true;
                    self.guard = Guard::Done;
                    return;
                }
                let e_new = lim.e_cap - 1;
                learned.rate_caps.insert(key, e_new);
                self.dropped_to = Some(self.capture_fps);
                self.guard = Guard::Applying { e_new };
            }
            Guard::Applying { e_new } => {
                if self.e <= *e_new && self.pending.is_empty() {
                    self.guard = Guard::Verifying {
                        key,
                        deadline: self.t_done + GUARD_VERIFY,
                    };
                }
            }
            Guard::Verifying { key, deadline } => {
                if self.capture_fps >= GUARD_RECOVERED * self.f {
                    self.guard = Guard::Done;
                } else if now > *deadline {
                    // The cause is not exposure: take the cap back and do not learn again this session.
                    learned.rate_caps.remove(key.as_str());
                    self.guard_spent = true;
                    self.dropped_to = None;
                    self.guard = Guard::Done;
                }
            }
            Guard::Done => {}
        }
    }

    // -----------------------------------------------------------------------------------------------------------
    // Output

    pub fn resume_report(&self) -> Option<Resume> {
        if self.state == ExposureState::Holding {
            self.resume_out
        } else {
            None
        }
    }

    pub fn status(&self, learned: &Learned) -> Option<ExposureStatus> {
        if !self.engaged {
            return None;
        }
        let lim = self.limits(learned);
        let state = if self.closed {
            ExposureState::Closed
        } else if self.paused {
            ExposureState::Paused
        } else {
            self.state
        };
        let gain = self.caps.gain.map(|_| self.g);
        let max_gain = lim.gain_usable.then_some(lim.g_cap);
        let message = exposure_message(&ExposureFacts {
            state,
            exposure: self.e,
            gain: if lim.gain_usable { gain } else { None },
            max_gain,
            e_min: self.caps.e_min,
            gain_min: self.caps.gain.map_or(0, |g| g.min),
            cap: lim.e_cap,
            cap_reason: lim.reason,
            fps: self.capture_fps,
            target_fps: self.f,
            dropped_to: self.dropped_to,
        });
        Some(ExposureStatus {
            state,
            exposure: self.e,
            gain,
            cap: lim.e_cap,
            cap_reason: lim.reason,
            max_gain,
            fps: self.capture_fps,
            target_fps: self.f,
            message,
        })
    }
}
