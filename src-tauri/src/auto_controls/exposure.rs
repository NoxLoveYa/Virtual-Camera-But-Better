//! The exposure loop (docs/AUTO-CONTROLS.md 2.5): exposure and gain as one quantity in stops, step-and-settle.
//!
//! One move, wait until a frame captured after the write has been measured (2.5.4), decide again. Exposure prefers the
//! longest time within the frame time (`E_rate`), gain fills the rest up to `Most gain`.
//!
//! The loop is pure: [`ExposureLoop::tick`] returns [`Action`]s; samples, write results and the clock come in as
//! calls. See `mod.rs` for how the runner drives it. The watchdog (2.7, `watchdog.rs`) is part of the loop: a
//! [`Fault`] ends it, [`Action::HandBack`] gives the controls back.

use std::collections::VecDeque;

use super::meter::{measure_exposure, ExposureReading, SEARCH_STOPS};
use super::status::{exposure_message, stripes, ExposureFacts, STRIPES_NOTE};
use super::watchdog::{
    Drift, Fault, Halt, Moved, Unit, Watchdog, NO_EFFECT_RATIO, NO_EFFECT_SPAN, NO_PICTURE_AFTER,
};
use super::{
    Action, CapReason, ExposureCfg, ExposureState, ExposureStatus, HandBack, Learned, Mode, Prior, Resume,
    SessionFacts, Speed, WriteDone,
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
const GUARD_LOW: f64 = 0.95;
const GUARD_RECOVERED: f64 = 0.95;
const GUARD_QUIET: f64 = 1.0;
const GUARD_LOW_FOR: f64 = 3.0;
const GUARD_VERIFY: f64 = 3.0;
/// Lowering the frame rate for light (2.5.12): at its gain limit and the exposure at the frame-rate limit, the picture
/// must stay more than this many stops too dark for `LIMIT_AFTER` seconds before the exposure may go one stop longer.
const OVER_ENTER: f64 = 1.0;
/// Back to the frame-rate limit once the scene needs this many stops less light than the frame-rate limit and `Most
/// gain` give together: gain then lands this far under its limit, and `OVER_ENTER + OVER_EXIT` stops of scene change
/// are needed to turn round (no flipping at the boundary).
const OVER_EXIT: f64 = 0.5;
/// The resume point is reported at most this often (2.5.11).
const RESUME_EVERY: f64 = 10.0;
/// The resume point is also reported once the loop has sat in a limit state this long (2.5.11).
const LIMIT_STORE_AFTER: f64 = 3.0;
/// W3 (2.7): a sample counts as "confident and unclipped" for judging a batch.
const JUDGE_CONFIDENCE: f64 = 0.5;
const JUDGE_CLIP: f64 = 0.05;
/// W3: a batch the picture has not answered yet is not waited for longer than this after its write.
const JUDGE_PATIENCE: f64 = 3.0;
/// W3: a reading this close to the meter's limit (`SEARCH_STOPS`) only says "at least that far off": a move cannot be
/// judged against it.
const METER_EDGE: f64 = SEARCH_STOPS - 0.1;
/// The white balance loop waits while the exposure loop is adjusting with more error than this (2.6).
const UNSETTLED_STOPS: f64 = 1.0;
/// 10.7: the card says "backlight: exposing for the middle" from this many stops of the backlight term, until it is
/// down to the second value.
const BACKLIT_ON: f64 = 0.5;
const BACKLIT_OFF: f64 = 0.25;
/// 11.4: while the front light term is on (a lit face), the loop never wants more light than the lit readings of this
/// many seconds said: it darkens at once when a lit face appears and waits this long before it brightens when the
/// face turns away.
const FRONT_HOLD: f64 = 2.0;
/// 13.11, 13.13: the hold while lit samples keep arriving, and after the last one once a lit face has come back (a face
/// that comes and goes every few seconds): a release and a new darkening, over and over, are the reversals W4 counts.
/// Longer than the face stays away (2-3.5 s in the sim) and than a face that stays lit swings (a lean, a wave of up to
/// 12 s whose lit half is 6 s); shorter than a lamp stays off.
const FRONT_HOLD_LONG: f64 = 6.0;
/// 13.13 A: a gap between two lit samples longer than this is the face away from the light (a lit face moves 0.2-0.5
/// stop from frame to frame, and a sample or two near the edge of the term's gate drop out): its coming back is the
/// come-back that lengthens the hold, also when it is back before the hold ran out (a wave of 8 s is away for 2 s).
const FRONT_GAP: f64 = 1.0;
/// 13.11: how long after the hold ran out the lit face may come back and still lengthen the hold; after a longer
/// absence the hold is `FRONT_HOLD` again.
const FRONT_BACK_WITHIN: f64 = 6.0;
/// 13.13 E: lit samples without a gap longer than `FRONT_GAP` for this long end the long hold after the last lit
/// sample (`FRONT_HOLD` again): a lamp that goes off then is a lamp, not a face that comes back. Twice
/// `FRONT_HOLD_LONG`, so that the lit half of a slow wave (12 s period: 6 s lit) is not taken for it.
const FRONT_STAYED: f64 = 2.0 * FRONT_HOLD_LONG;
/// 11.8: the card says "dark room: exposing for the lit middle" from this many stops of the held front light term
/// (negative), until it is up to the second value.
const FRONTLIT_ON: f64 = -0.5;
const FRONTLIT_OFF: f64 = -0.25;

/// The "backlit" flag after a reading whose median backlight term is `backlight` stops: on from `BACKLIT_ON`, off at
/// `BACKLIT_OFF` or less, unchanged between (hysteresis, so the polite live region of the card does not chatter).
pub fn backlit_after(was: bool, backlight: f64) -> bool {
    if backlight >= BACKLIT_ON {
        true
    } else if backlight <= BACKLIT_OFF {
        false
    } else {
        was
    }
}

/// The "front lit" flag after a held front light term of `front` stops (`<= 0`): on from `FRONTLIT_ON`, off at
/// `FRONTLIT_OFF` or more, unchanged between (11.8).
pub fn frontlit_after(was: bool, front: f64) -> bool {
    if front <= FRONTLIT_ON {
        true
    } else if front >= FRONTLIT_OFF {
        false
    } else {
        was
    }
}

/// The lit hold (11.4, 13.11, 13.13): the readings of the samples that have the front light term on (a lit face). The
/// loop never wants more light than the lowest of them says (attack at once, release later). A reading and not only its
/// front term is held: with the term on, the reading follows the face (a face two stops darker is a reading a stop
/// higher), so holding the term alone leaves the loop following a face that comes and goes.
///
/// While lit samples keep arriving (the last one at most `FRONT_HOLD` ago) the held reading is the lowest of the last
/// `FRONT_HOLD_LONG` seconds: a face that stays lit and changes by half a stop to two (a lean, a smooth wave) swings
/// through that window, and the loop follows its brightest moment and not each swing. Once they stop (the lamp is off,
/// the face left the light: the term is back to 0) the hold lasts `len` after the last one: `FRONT_HOLD` until a lit
/// sample comes back after more than `FRONT_GAP` away, within `len` or `FRONT_BACK_WITHIN` after the hold ran out (a
/// face that comes and goes), then `FRONT_HOLD_LONG` until `FRONT_STAYED` seconds of lit samples pass without such a
/// gap (it stayed: a lamp that goes off then is a lamp); a longer absence, a pause or a close puts it back. A hold that
/// ran out is released for good: the readings it held do not come back with the next lit sample.
#[derive(Clone, Debug)]
struct LitHold {
    /// `(capture time, reading)` of the lit samples of the last `FRONT_HOLD_LONG` seconds.
    ring: VecDeque<(f64, f64)>,
    /// `(time the picture shows it, stops)` of the writes of the last `FRONT_HOLD_LONG` seconds: a reading captured
    /// before one is that many stops too bright to compare with a reading captured after it.
    moves: VecDeque<(f64, f64)>,
    /// The hold after the last lit sample, when lit samples have stopped.
    len: f64,
    /// Capture time of the last lit sample.
    last: Option<f64>,
    /// Capture time of the first sample of the run of lit samples that has no gap longer than `FRONT_GAP`.
    run_from: f64,
}

impl LitHold {
    fn new() -> LitHold {
        LitHold {
            ring: VecDeque::new(),
            moves: VecDeque::new(),
            len: FRONT_HOLD,
            last: None,
            run_from: f64::NEG_INFINITY,
        }
    }

    /// A write batch that changed the light by `dx` stops (more light is positive) shows in the pictures captured from
    /// `shown`; its first write started at `from`. A lit reading captured in between shows the batch partly (the
    /// camera applies a write somewhere in there) and is dropped; the older ones are `dx` stops too bright now.
    fn moved(&mut self, shown: f64, dx: f64, from: f64) {
        self.ring.retain(|&(t, _)| !(from <= t && t < shown));
        self.moves.push_back((shown, dx));
        while self.moves.front().is_some_and(|&(s, _)| s < shown - FRONT_HOLD_LONG - 1.0) {
            self.moves.pop_front();
        }
    }

    fn clear(&mut self) {
        *self = LitHold::new();
    }

    /// A confident sample captured at `t`, read `e` stops, with the front light term on.
    fn push(&mut self, t: f64, e: f64) {
        match self.last.map(|l| t - l) {
            None => self.run_from = t,
            Some(gap) if gap > FRONT_GAP => {
                self.run_from = t;
                if gap > self.len {
                    // The hold had run out: what it held is released for good (the new lit samples must not bring it
                    // back). A lit face that is back soon after is one that comes and goes.
                    self.ring.clear();
                    self.len = if gap - self.len <= FRONT_BACK_WITHIN {
                        FRONT_HOLD_LONG
                    } else {
                        FRONT_HOLD
                    };
                } else {
                    // Away for a while and back before the hold ran out: one that comes and goes as well.
                    self.len = FRONT_HOLD_LONG;
                }
            }
            Some(_) => {
                if self.len > FRONT_HOLD && t - self.run_from >= FRONT_STAYED {
                    // It stayed lit for twice as long as the long hold lasts, with no come-back: a lamp that goes
                    // off now is a lamp.
                    self.len = FRONT_HOLD;
                }
            }
        }
        self.last = Some(t);
        self.ring.push_back((t, e));
        while self.ring.front().is_some_and(|&(s, _)| s < t - FRONT_HOLD_LONG) {
            self.ring.pop_front();
        }
        while self.moves.front().is_some_and(|&(s, _)| s < t - FRONT_HOLD_LONG) {
            self.moves.pop_front();
        }
    }

    /// The lowest median of three consecutive lit readings among those in the hold at `now`, each as the picture now
    /// would read it (less the writes since); `None` with fewer than three lit readings.
    fn held(&self, now: f64) -> Option<f64> {
        let lit_now = self.last.is_some_and(|l| now - l <= FRONT_HOLD);
        let len = if lit_now { FRONT_HOLD_LONG } else { self.len };
        let v: Vec<f64> = self
            .ring
            .iter()
            .filter(|&&(t, _)| t >= now - len)
            .map(|&(t, e)| e - self.moves.iter().filter(|&&(shown, _)| shown > t).map(|&(_, dx)| dx).sum::<f64>())
            .collect();
        v.windows(MEDIAN_SAMPLES)
            .map(|w| {
                let mut w = [w[0], w[1], w[2]];
                w.sort_by(f64::total_cmp);
                w[1]
            })
            .min_by(f64::total_cmp)
    }
}

/// 2.3 "gain range under 0.5 stop": the whole gain range (3 stops until learned) must have steps no coarser than half
/// a stop, i.e. at least this many.
const GAIN_MIN_STEPS: i32 = 6;

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
    pub default: i32,
    /// Gain has its own Camera auto flag (2.9).
    pub can_auto: bool,
}

/// What the loop needs to know about the camera's exposure and gain controls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExposureCaps {
    pub e_min: i32,
    pub e_max: i32,
    pub e_default: i32,
    /// The camera has Camera auto for exposure: where a hand-back goes (owner decision, section 8 question 5).
    pub e_auto: bool,
    pub gain: Option<GainCaps>,
}

impl ExposureCaps {
    /// `None` when the camera has no manual exposure. Gain is used only when it is manual and fine enough (2.3).
    pub fn from_controls(caps: &[ControlInfo]) -> Option<ExposureCaps> {
        let ex = caps
            .iter()
            .find(|c| c.key == ControlKey::Exposure && c.can_manual)?;
        let gain = caps
            .iter()
            .find(|c| {
                c.key == ControlKey::Gain && c.can_manual && (c.max - c.min) / c.step.max(1) >= GAIN_MIN_STEPS
            })
            .map(|c| GainCaps {
                min: c.min,
                max: c.max,
                step: c.step.max(1),
                default: c.default.clamp(c.min, c.max),
                can_auto: c.can_auto,
            });
        Some(ExposureCaps {
            e_min: ex.min.min(ex.max),
            e_max: ex.max.max(ex.min),
            e_default: ex.default,
            e_auto: ex.can_auto,
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
    /// The longest exposure the loop may use now: `e_ceil` while it lowers the frame rate for light, else `e_rate`.
    e_cap: i32,
    /// The longest exposure that keeps the frame rate (the user's stop, the learned cap, or `E_rate`).
    e_rate: i32,
    /// The longest exposure at all, reached only while the frame rate is lowered (2.5.12). One stop above `e_rate`
    /// with `Longest exposure = fit`, else `e_rate`.
    e_ceil: i32,
    reason: CapReason,
    /// Gain is used (exists, manual, `Most gain` above its minimum).
    gain_usable: bool,
    g_cap: i32,
    gs_cap: f64,
    /// `S` now.
    s: f64,
}

/// What the white balance loop needs to know about this loop (2.6: exposure has priority).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ExposureView {
    /// The loop is running.
    pub engaged: bool,
    /// A write is in flight.
    pub busy: bool,
    /// When the last write batch returned.
    pub last_write: f64,
    /// Adjusting with more than a stop of error.
    pub unsettled: bool,
}

impl ExposureView {
    /// No exposure loop: nothing to wait for.
    pub fn idle() -> ExposureView {
        ExposureView {
            engaged: false,
            busy: false,
            last_write: f64::NEG_INFINITY,
            unsettled: false,
        }
    }
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
    /// The stops of `e` that are the backlight term (10.4), and the share of light sources that opened it.
    backlight: f64,
    sources: f64,
    /// The stops of `e` that are the front light term (11.4, `<= 0`), and the gap to the room that opened it.
    front: f64,
    room_gap: f64,
    /// The horizontal spread of the bright tenth (13.4), for the log line.
    spread: f64,
}

#[derive(Clone, Copy, Debug)]
struct Pending {
    key: ControlKey,
    prev: i32,
    /// When the action was issued (W1: a write that never reports back).
    issued_at: f64,
    /// Part of the seed (a failed seed is written again).
    seed: bool,
    /// An exposure write above the frame-rate limit (2.5.12): a camera may refuse it.
    past_rate: bool,
}

/// A decision's write batch in flight: what the lit hold needs to know when it lands (13.13 C).
#[derive(Clone, Copy, Debug)]
struct Moving {
    /// The model's light `E + gs(G)` (stops) when the batch was decided, and the `S` it was computed with.
    x_before: f64,
    s: f64,
    /// When the first write of the batch started (the earliest start reported so far).
    started: f64,
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

/// The reading a decision acted on, for judging its batch (W3).
#[derive(Clone, Copy, Debug)]
struct Before {
    e: f64,
    /// Confident and unclipped then: the batch can be judged.
    judgeable: bool,
}

/// A batch waiting to be judged (W3).
#[derive(Clone, Copy, Debug)]
struct Batch {
    e_before: f64,
    /// The change in stops the model predicted.
    predicted: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Assess {
    Go,
    Wait,
    Fault(Fault),
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
    /// What the loop decides on (11.4): the median of the last samples, never above the lit hold's reading.
    e: f64,
    /// The median of the readings without their front light term (section 10's reading): what the camera's answer to a
    /// write is measured on (W3, gain learning). The front term follows the face, not the exposure (13.11).
    e_plain: f64,
    sigma: f64,
    conf: f64,
    clip: f64,
    backlight: f64,
    sources: f64,
    /// The front light term the decision has now: `e - e_plain` (11.8).
    front_held: f64,
    room_gap: f64,
    spread: f64,
}

pub struct ExposureLoop {
    caps: ExposureCaps,
    cfg: ExposureCfg,
    /// The mode is Better Cam auto.
    mode_app: bool,
    /// The watchdog stopped the loop; kept until the mode changes (see `set_mode`) or the user resumes.
    halt: Option<Halt>,
    /// A fault raised but not yet acted on (the next tick hands back).
    fault: Option<Fault>,
    prior: Prior,
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
    /// The last tick time, and when a sample last arrived (W5).
    now: f64,
    last_arrival: f64,
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
    /// Since when the picture is more than `OVER_ENTER` stops too dark at the gain limit and the frame-rate limit.
    over_since: Option<f64>,
    /// The loop lowers the frame rate for light: exposure may go up to `e_ceil` (2.5.12).
    over: bool,
    /// `over` came from the seed (a resume point above `E_rate`) and no confident decision has judged it yet.
    over_seeded: bool,
    /// The exposure has been above `E_rate` since `over` began: back at `E_rate` it ends (2.5.12).
    over_above: bool,
    /// The camera did not take an exposure above `E_rate` this session: `E_ceil` is `E_rate` until the next one.
    over_refused: bool,
    /// `E_rate` of the last tick (what `issue` compares a write with).
    e_rate: i32,
    /// The loop is taking the exposure past `E_rate` now (the entry, or a move up there): the card says so (4.2).
    lowering: bool,
    probe: Option<Probe>,
    batch: Option<Batch>,
    guard: Guard,
    guard_spent: bool,
    /// The rate the camera dropped to when the cap was learned (for the status text).
    dropped_to: Option<f64>,
    last_resume_at: f64,
    resume_out: Option<Resume>,
    /// Since when the state is a limit state, and whether the resume point of this stay is stored (2.5.11).
    limit_state_since: Option<f64>,
    limit_stored: bool,
    /// The error of the last decision's reading; infinite before the first.
    last_e: f64,
    /// Confidence and clipped share of the last decision's reading, for the log line (`diag`).
    last_conf_clip: (f64, f64),
    /// The median backlight term (stops) and light-source share of the last decision's reading (10.7, `diag`).
    last_backlight: (f64, f64),
    /// The backlight term is on: at least `BACKLIT_ON` stops, until it falls to `BACKLIT_OFF` (the card says so while
    /// holding).
    backlit: bool,
    /// The readings of the samples with the front light term on (11.4, 13.11), kept comparable across writes; cleared
    /// on pause, close, engagement and a new target.
    lit_hold: LitHold,
    /// The batch in flight, for the lit hold when it lands (13.13 C).
    moving: Option<Moving>,
    /// The held front light term (stops), the median gap to the room and the median spread of the bright tenth of the
    /// last decision's reading (11.8, 13.4, `diag`).
    last_front: (f64, f64, f64),
    /// The held front light term is on: at most `FRONTLIT_ON` stops, until it is back up to `FRONTLIT_OFF`.
    frontlit: bool,
    /// Seconds without a sample, when it is long enough to matter (W5).
    no_picture: Option<f64>,
    /// Values another program changed: written again (W6).
    retakes: Vec<(ControlKey, i32)>,
    watchdog: Watchdog,
}

impl ExposureLoop {
    pub fn new(caps: ExposureCaps, cfg: ExposureCfg) -> ExposureLoop {
        ExposureLoop {
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
            f: 30.0,
            capture_fps: 0.0,
            resumed_at: f64::NEG_INFINITY,
            now: f64::NEG_INFINITY,
            last_arrival: f64::NEG_INFINITY,
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
            over_since: None,
            over: false,
            over_seeded: false,
            over_above: false,
            over_refused: false,
            e_rate: i32::MAX,
            lowering: false,
            probe: None,
            batch: None,
            guard: Guard::Watching { low_since: None },
            guard_spent: false,
            dropped_to: None,
            last_resume_at: f64::NEG_INFINITY,
            resume_out: None,
            limit_state_since: None,
            limit_stored: false,
            last_e: f64::INFINITY,
            last_conf_clip: (0.0, 0.0),
            last_backlight: (0.0, 0.0),
            backlit: false,
            lit_hold: LitHold::new(),
            moving: None,
            last_front: (0.0, 0.0, 0.0),
            frontlit: false,
            no_picture: None,
            retakes: Vec::new(),
            watchdog: Watchdog::new(Unit::Stops),
        }
    }

    /// The loop is running: Better Cam auto and not stopped by the watchdog.
    pub fn engaged(&self) -> bool {
        self.mode_app && self.halt.is_none()
    }

    pub fn set_cfg(&mut self, cfg: ExposureCfg) {
        if (cfg.target - self.cfg.target).abs() > f64::EPSILON {
            // The error of the samples was measured against the old target.
            self.samples.clear();
            self.lit_hold.clear();
            self.batch = None;
        }
        if cfg.longest != self.cfg.longest || cfg.max_gain != self.cfg.max_gain {
            self.limit_since = None;
            self.dark_since = None;
            self.end_lowered();
        }
        self.cfg = cfg;
    }

    /// 2.5.12: the entry (`over` with the exposure still at `E_rate`, never past it) is over when the decision after it
    /// leaves the exposure at or below `E_rate` (`e2`): the room got brighter in between and gain covers it. Else the
    /// state would stay "lowered" at the full frame rate, the guard off and the next darkening without its timer.
    fn end_idle_lowered(&mut self, e2: i32, lim: &Limits) {
        if self.over && !self.over_above && self.e <= lim.e_rate && e2 <= lim.e_rate {
            self.end_lowered();
        }
    }

    /// The card's "backlit" and "front lit" notes say what the last decision saw: after a pause or a close there is no
    /// such decision, and the first "Holding" text after the resume must not show them (13.11).
    fn clear_notes(&mut self) {
        self.backlit = false;
        self.frontlit = false;
    }

    /// The frame rate is no longer lowered for light (2.5.12).
    fn end_lowered(&mut self) {
        self.over = false;
        self.over_seeded = false;
        self.over_above = false;
        self.lowering = false;
        self.over_since = None;
    }

    /// The mode the hub holds for exposure now. A halted loop keeps its status while the mode is the one the control
    /// was handed to and forgets it when the mode is anything else (the user chose, or chose Better Cam auto again).
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

    /// "Resume" after a conflict, "Try again" after an ignored control: forget the halt. A paused (conflict) loop
    /// starts again now, from the values it holds; a handed-back one starts with the next `set_mode(App)`.
    pub fn resume(&mut self) {
        self.halt = None;
        if self.mode_app {
            self.reset_session();
        }
    }

    /// A new camera session (or the first): seed again, forget the measurements.
    pub fn reset_session(&mut self) {
        self.started = false;
        self.samples.clear();
        self.lit_hold.clear();
        self.pending.clear();
        self.retakes.clear();
        self.fault = None;
        self.t_done = f64::NEG_INFINITY;
        self.last_write_done = f64::NEG_INFINITY;
        self.band = Band::Adjusting;
        self.state = ExposureState::Starting;
        self.no_signal_since = None;
        self.last_no_signal_move = f64::NEG_INFINITY;
        self.limit_since = None;
        self.dark_since = None;
        self.end_lowered();
        self.probe = None;
        self.batch = None;
        self.guard = Guard::Watching { low_since: None };
        self.guard_spent = false;
        self.dropped_to = None;
        self.limit_state_since = None;
        self.limit_stored = false;
        self.resumed_at = f64::NEG_INFINITY;
        self.last_arrival = f64::NEG_INFINITY;
        self.last_e = f64::INFINITY;
        self.last_conf_clip = (0.0, 0.0);
        self.last_backlight = (0.0, 0.0);
        self.backlit = false;
        self.last_front = (0.0, 0.0, 0.0);
        self.frontlit = false;
        self.no_picture = None;
        self.watchdog.reset();
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
        self.limits_at(self.f, learned)
    }

    /// The limits for a session at frame rate `f`.
    fn limits_at(&self, f: f64, learned: &Learned) -> Limits {
        let s = learned.gain_stops.unwrap_or(S_INITIAL).clamp(S_MIN, S_MAX);
        let (mut e_cap, mut reason) = match self.cfg.longest {
            super::Longest::Fit => (rate_limit(f), CapReason::Fps),
            super::Longest::Stop(n) => (n, CapReason::User),
        };
        if self.cfg.longest == super::Longest::Fit {
            if let Some(&l) = learned.rate_caps.get(&rate_key(f)) {
                if l < e_cap {
                    e_cap = l;
                    reason = CapReason::Learned;
                }
            }
        }
        let e_rate = e_cap.clamp(self.caps.e_min, self.caps.e_max);
        // 2.5.12: with `fit`, one stop (twice the frame time) more may be used while the frame rate is lowered for
        // light; the user's own stop is never exceeded, and a camera that refused it this session gets none.
        let e_ceil = if self.cfg.longest == super::Longest::Fit && !self.over_refused {
            (e_rate + 1).min(self.caps.e_max)
        } else {
            e_rate
        };
        let e_cap = if self.over { e_ceil } else { e_rate };
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
            e_rate,
            e_ceil,
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

    /// The camera has a gain control, `Most gain` is at its minimum (gain off), and the gain is somewhere else.
    fn gain_stuck(&self, lim: &Limits) -> bool {
        self.caps.gain.is_some_and(|gc| !lim.gain_usable && self.g != gc.min)
    }

    /// The camera has a gain control that the loop does not use: `Most gain` is at its minimum.
    fn gain_off(&self, lim: &Limits) -> bool {
        self.caps.gain.is_some() && !lim.gain_usable
    }

    /// The most light the limits allow, and the least.
    fn x_range(&self, lim: &Limits) -> (f64, f64) {
        let top = f64::from(lim.e_cap) + lim.gs_cap;
        let bottom = f64::from(self.caps.e_min);
        (bottom, top)
    }

    /// A requested value as the camera takes it: inside its range, on its step (W2 compares the read-back with it).
    fn snap(&self, key: ControlKey, v: i32) -> i32 {
        match (key, self.caps.gain) {
            (ControlKey::Gain, Some(gc)) => {
                let v = v.clamp(gc.min, gc.max);
                gc.min + ((v - gc.min) as f64 / f64::from(gc.step)).round() as i32 * gc.step
            }
            _ => v.clamp(self.caps.e_min, self.caps.e_max),
        }
    }

    // -----------------------------------------------------------------------------------------------------------
    // Inputs

    pub fn on_sample(&mut self, stats: &Stats, capture_time: f64) {
        if !self.engaged() || self.paused || self.closed {
            return;
        }
        self.last_arrival = self.now;
        let ExposureReading {
            error,
            confidence,
            clip,
            backlight,
            sources,
            front,
            room_gap,
            spread,
        } = measure_exposure(stats, self.cfg.target);
        self.samples.push_back(Sample {
            t: capture_time,
            e: error,
            conf: confidence,
            clip,
            backlight,
            sources,
            front,
            room_gap,
            spread,
        });
        while self.samples.len() > MAX_SAMPLES {
            self.samples.pop_front();
        }
        if confidence >= NO_SIGNAL_CONFIDENCE && front <= FRONTLIT_ON {
            self.lit_hold.push(capture_time, error);
        }
    }

    pub fn on_write_done(&mut self, w: WriteDone) {
        let Some(at) = self.pending.iter().position(|p| p.key == w.key) else {
            return;
        };
        let p = self.pending.remove(at);
        // 2.5.12: a camera that takes no exposure longer than its frame time reads back the old (or the longest) value
        // for the stop past `E_rate`. That is a limit of the camera, not an ignored control: no mismatch, and the
        // loop keeps the frame-rate limit for the rest of the session.
        let refused = w.ok
            && w.key == ControlKey::Exposure
            && p.past_rate
            && w.readback.is_some_and(|r| r <= self.e_rate && r < w.requested);
        let matched = refused || w.readback.is_none_or(|r| r == self.snap(w.key, w.requested));
        if let Some(f) = self.watchdog.write_done(&w, matched) {
            self.raise(f);
        }
        if refused {
            self.over_refused = true;
            self.end_lowered();
            self.batch = None;
            self.probe = None;
        }
        if w.ok {
            // The model believes the device: a read-back that differs is what the camera has now (W2 counts it).
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
            if p.seed {
                self.started = false;
            }
        }
        self.t_done = self.t_done.max(w.done);
        if let Some(m) = self.moving.as_mut() {
            m.started = m.started.min(w.started);
        }
        if self.pending.is_empty() {
            // The batch has landed: pictures captured from `valid_from` on show it. What it changed is the model's
            // light now (the read-backs, a refused or failed write left out) less the light it started from.
            if let Some(m) = self.moving.take() {
                let dx = self.x(m.s) - m.x_before;
                if dx.abs() > 1e-9 {
                    self.lit_hold.moved(self.valid_from(), dx, m.started);
                }
            }
        }
    }

    /// W6: the 1 Hz read-back of a driven control.
    pub fn on_readback(&mut self, info: &ControlInfo, now: f64) {
        if !self.engaged() || !self.started || self.closed || self.paused || !self.pending.is_empty() {
            return;
        }
        let expected = match info.key {
            ControlKey::Exposure => self.e,
            ControlKey::Gain if self.caps.gain.is_some() => self.g,
            _ => return,
        };
        self.watchdog.read_ok();
        if !info.auto && info.value == expected {
            return;
        }
        match self.watchdog.drift(now) {
            Drift::Retake => {
                if !self.retakes.iter().any(|(k, _)| *k == info.key) {
                    self.retakes.push((info.key, expected));
                }
            }
            Drift::Conflict => self.raise(Fault::Conflict),
        }
    }

    /// W1: a read-back failed.
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

    // -----------------------------------------------------------------------------------------------------------
    // The watchdog's hand-back (2.7)

    /// Where exposure goes: Camera auto where the camera has it (owner decision), else the value it had before.
    fn hand_back_exposure(&self) -> HandBack {
        if self.caps.e_auto {
            HandBack::CameraAuto
        } else {
            HandBack::Manual(
                self.prior
                    .exposure
                    .unwrap_or(self.caps.e_default)
                    .clamp(self.caps.e_min, self.caps.e_max),
            )
        }
    }

    /// Gain goes back to what it was (2.9: its Camera auto flag, if that was on).
    fn hand_back_gain(&self, gc: GainCaps) -> HandBack {
        if gc.can_auto && self.prior.gain_auto {
            HandBack::CameraAuto
        } else {
            HandBack::Manual(self.prior.gain.unwrap_or(gc.default).clamp(gc.min, gc.max))
        }
    }

    /// Act on a raised fault: stop, hand the controls back (not for a conflict) and remember why.
    fn flush_fault(&mut self, learned: &mut Learned) -> Option<Vec<Action>> {
        let fault = self.fault.take()?;
        self.pending.clear();
        self.retakes.clear();
        self.batch = None;
        self.probe = None;
        self.resume = Resume {
            exposure: Some(self.e),
            gain: self.caps.gain.map(|_| self.g),
            white_balance: self.resume.white_balance,
        };
        let mut actions = Vec::new();
        let mut to = None;
        if fault.hands_back() {
            let e_to = self.hand_back_exposure();
            actions.push(Action::HandBack {
                key: ControlKey::Exposure,
                to: e_to,
            });
            if let Some(gc) = self.caps.gain {
                actions.push(Action::HandBack {
                    key: ControlKey::Gain,
                    to: self.hand_back_gain(gc),
                });
            }
            to = Some(e_to);
            self.mode_app = false;
        }
        if fault == Fault::Ignored && !learned.blocked.contains(&ControlKey::Exposure) {
            learned.blocked.push(ControlKey::Exposure);
        }
        self.halt = Some(Halt::new(fault, to));
        Some(actions)
    }

    // -----------------------------------------------------------------------------------------------------------
    // The tick

    pub fn tick(&mut self, now: f64, facts: &SessionFacts, learned: &mut Learned, other_busy: bool) -> Vec<Action> {
        self.now = now;
        self.f = facts.target_fps.max(1.0);
        self.capture_fps = facts.capture_fps;
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
            self.lit_hold.clear();
            self.clear_notes();
            return Vec::new();
        }
        if facts.paused {
            self.paused = true;
            self.lit_hold.clear();
            self.clear_notes();
            self.no_signal_since = None;
            self.limit_since = None;
            self.dark_since = None;
            self.over_since = None;
            self.no_picture = None;
            return Vec::new();
        }
        if self.paused {
            // Resumed: what was measured before says nothing about now.
            self.paused = false;
            self.samples.clear();
            self.batch = None;
            self.watchdog.touch(now);
        }
        self.resumed_at = facts.resumed_at;
        if !self.started {
            if now < self.watchdog.retry_at() {
                return Vec::new();
            }
            // A new session (a reopen, a new engagement): the camera may take the longer exposure this time.
            self.over_refused = false;
            let lim = self.limits(learned);
            self.e_rate = lim.e_rate;
            return self.start(now, &lim);
        }
        // W1: a write that never reports back.
        let late = self.pending.iter().find_map(|p| Watchdog::too_long(now, p.issued_at));
        if let Some(f) = late {
            self.raise(f);
            return self.flush_fault(learned).unwrap_or_default();
        }
        // W5: no sample for 2 s: hold, no writes.
        let quiet = self.watchdog.quiet(now, self.last_arrival);
        self.no_picture = (quiet >= NO_PICTURE_AFTER).then_some(quiet);
        if self.no_picture.is_some() {
            // The 3 s of the entry timer are unbroken (2.5.12): a gap in the frames starts them again.
            self.over_since = None;
        }
        self.e_rate = self.limits(learned).e_rate;
        self.guard_step(now, learned);
        self.track_limit(now);
        if !self.pending.is_empty() || now < self.watchdog.retry_at() || self.no_picture.is_some() || other_busy {
            return Vec::new();
        }
        // W6: values another program changed are taken back; once a second the driven controls are read.
        if !self.retakes.is_empty() {
            self.batch = None;
            self.probe = None;
            let writes = std::mem::take(&mut self.retakes);
            return self.issue(writes, false);
        }
        if self.watchdog.readback_due(now, self.t_done) {
            let mut reads = vec![Action::ReadBack {
                key: ControlKey::Exposure,
            }];
            if self.caps.gain.is_some() {
                reads.push(Action::ReadBack { key: ControlKey::Gain });
            }
            return reads;
        }
        let base = self.cfg.speed.exposure_pace();
        let pace = Pace {
            k: base.k * self.watchdog.k_scale(now),
            step_max: base.step_max,
            gap: self.watchdog.gap(base.gap),
        };
        if now < self.t_done + pace.gap {
            return Vec::new();
        }
        let lim = self.limits(learned);
        let actions = self.decide(now, &lim, &pace, learned);
        if self.fault.is_some() {
            return self.flush_fault(learned).unwrap_or_default();
        }
        actions
    }

    /// 2.5.11: the seed. `E0 = min(E_cap, resume.exposure ?? device default)`, `G0 = resume.gain ?? G_min`. The
    /// device's reported value is never used (stale while Camera auto is on).
    fn start(&mut self, now: f64, lim: &Limits) -> Vec<Action> {
        let (e0, g0) = self.seed_in(lim);
        // A resume point above `E_rate` is only a guess (the resume point carries no frame rate): the first confident
        // decision judges whether the room still needs it (2.5.11).
        self.end_lowered();
        self.over = e0 > lim.e_rate;
        self.over_seeded = self.over;
        self.started = true;
        self.state = ExposureState::Starting;
        self.band = Band::Adjusting;
        self.batch = None;
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
        self.issue(writes, true)
    }

    /// `(E0, G0)` within `lim`.
    fn seed_in(&self, lim: &Limits) -> (i32, i32) {
        // A resume point from a lowered frame rate (2.5.12) is a valid seed (`start` takes the state with it); the
        // device's default is cut to the frame-rate limit.
        let e0 = match self.resume.exposure {
            Some(e) => e.min(lim.e_ceil),
            None => self.caps.e_default.min(lim.e_rate),
        }
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
        (e0, g0)
    }

    /// What `start` would write for a session at frame rate `f`: exposure and, when the loop has gain, gain.
    pub fn seed(&self, f: f64, learned: &Learned) -> (i32, Option<i32>) {
        let (e0, g0) = self.seed_in(&self.limits_at(f.max(1.0), learned));
        (e0, self.caps.gain.map(|_| g0))
    }

    /// Record the writes as pending and believe them; the runner reports each result.
    fn issue(&mut self, writes: Vec<(ControlKey, i32)>, seed: bool) -> Vec<Action> {
        self.moving = None;
        let mut out = Vec::with_capacity(writes.len());
        for (key, value) in writes {
            let prev = match key {
                ControlKey::Exposure => std::mem::replace(&mut self.e, value),
                _ => std::mem::replace(&mut self.g, value),
            };
            self.pending.push(Pending {
                key,
                prev,
                issued_at: self.now,
                seed,
                past_rate: key == ControlKey::Exposure && value > self.e_rate,
            });
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

    /// Issue a plan; `None` when it changes nothing. `before` is the reading a decision acted on (W3 judges the
    /// batch against it).
    fn move_to(
        &mut self,
        e2: i32,
        g2: i32,
        lim: &Limits,
        probe: Option<Probe>,
        before: Option<Before>,
    ) -> Option<Vec<Action>> {
        let plan = self.plan(e2, g2, lim.s);
        if plan.is_empty() {
            return None;
        }
        let predicted: f64 = plan.iter().map(|p| p.2).sum();
        // W4: a fourth reversal within 10 s damps; another while damped hands back.
        if self.watchdog.moved(self.now, predicted) == Moved::Hunting {
            self.raise(Fault::Hunting);
            return Some(Vec::new());
        }
        // A gain-only move (or none past `E_rate`) from the frame-rate limit: nothing is lowered after all.
        self.end_idle_lowered(e2, lim);
        self.lowering = self.over && e2 > lim.e_rate && e2 > self.e;
        let gain_only = plan.len() == 1 && plan[0].0 == ControlKey::Gain;
        self.probe = probe.filter(|p| gain_only && p.dgs.abs() >= LEARN_MIN_STEP);
        self.batch = before
            .filter(|b| b.judgeable && predicted.abs() >= Unit::Stops.min_predicted())
            .map(|b| Batch {
                e_before: b.e,
                predicted,
            });
        self.state = ExposureState::Adjusting;
        self.band = Band::Adjusting;
        let x_before = self.x(lim.s);
        let actions = self.issue(plan.into_iter().map(|(k, v, _)| (k, v)).collect(), false);
        self.moving = Some(Moving {
            x_before,
            s: lim.s,
            started: f64::INFINITY,
        });
        Some(actions)
    }

    /// The valid samples: captured after the last write landed (`D_apply`, longer for a slow camera) and after a
    /// resume (2.5.4).
    fn valid(&self) -> Vec<Sample> {
        let threshold = self.valid_from();
        self.samples.iter().filter(|s| s.t >= threshold).copied().collect()
    }

    fn valid_from(&self) -> f64 {
        (self.t_done + D_APPLY_BASE + 1.0 / self.f + self.watchdog.slow()).max(self.resumed_at)
    }

    /// The lit hold's reading (11.4, 13.11) for these valid samples: the lowest recent reading of a lit face, as the
    /// picture now would read it.
    fn lit_held(&self, valid: &[Sample]) -> Option<f64> {
        self.lit_hold.held(valid.last()?.t)
    }

    /// `hold` is the lit hold's reading (`lit_held`): the decision's `e` is the median of the last samples, never
    /// above it (11.4).
    fn reading(valid: &[Sample], hold: Option<f64>) -> Option<Reading> {
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
        // The spread of what the decision is about: the reading without the front light term, which the hold deals
        // with (a moving face changes it by 0.2-0.5 stop from frame to frame).
        let tail: Vec<f64> = valid.iter().rev().take(SIGMA_SAMPLES).map(|s| s.e - s.front).collect();
        let mean = tail.iter().sum::<f64>() / tail.len() as f64;
        let sigma = (tail.iter().map(|e| (e - mean).powi(2)).sum::<f64>() / tail.len() as f64).sqrt();
        let e_plain = median(|s| s.e - s.front);
        let e = hold.map_or(median(|s| s.e), |h| median(|s| s.e).min(h));
        Some(Reading {
            e,
            e_plain,
            sigma,
            conf: median(|s| s.conf),
            clip: median(|s| s.clip),
            backlight: median(|s| s.backlight),
            sources: median(|s| s.sources),
            front_held: e - e_plain,
            room_gap: median(|s| s.room_gap),
            spread: median(|s| s.spread),
        })
    }

    /// The error now as the white balance loop may look at it: the median of the last valid samples, never above the
    /// lit hold's reading, or the last decision's reading when there are none yet.
    fn peek_error(&self) -> f64 {
        let valid = self.valid();
        if valid.is_empty() {
            return self.last_e;
        }
        let mut v: Vec<f64> = valid.iter().rev().take(MEDIAN_SAMPLES).map(|s| s.e).collect();
        v.sort_by(f64::total_cmp);
        let e = v[v.len() / 2];
        self.lit_held(&valid).map_or(e, |h| e.min(h))
    }

    /// What the white balance loop looks at (2.6).
    pub fn view(&self) -> ExposureView {
        let engaged = self.engaged();
        ExposureView {
            engaged,
            busy: !self.pending.is_empty(),
            last_write: self.t_done,
            unsettled: engaged
                && !self.paused
                && !self.closed
                && matches!(self.state, ExposureState::Starting | ExposureState::Adjusting)
                && self.peek_error().abs() > UNSETTLED_STOPS,
        }
    }

    // -----------------------------------------------------------------------------------------------------------
    // The decision (2.5.5)

    fn decide(&mut self, now: f64, lim: &Limits, pace: &Pace, learned: &mut Learned) -> Vec<Action> {
        let s = lim.s;
        // A constraint (frame rate or the user's limit changed): move to the cap, brightness-neutral, no samples
        // needed and not bounded by `step_max`. Most gain at its minimum is one too (2.5.2: `Gs_cap = 0`): gain goes
        // to `G_min` and exposure takes the stops it gave, as far as whole stops allow.
        if self.e > lim.e_cap || self.gain_stuck(lim) {
            self.over_since = None;
            let (e2, g2) = self.allocate(self.x(s), lim);
            return self.move_to(e2, g2, lim, None, None).unwrap_or_default();
        }
        let valid = self.valid();
        let Some(r) = Self::reading(&valid, self.lit_held(&valid)) else {
            return Vec::new();
        };
        self.last_e = r.e;
        self.last_conf_clip = (r.conf, r.clip);
        self.last_backlight = (r.backlight, r.sources);
        self.backlit = backlit_after(self.backlit, r.backlight);
        self.last_front = (r.front_held, r.room_gap, r.spread);
        self.frontlit = frontlit_after(self.frontlit, r.front_held);
        self.learn(&r, learned);
        // W3: did the picture answer the last batch?
        match self.assess(now, &valid, &r) {
            Assess::Go => {}
            Assess::Wait => return Vec::new(),
            Assess::Fault(f) => {
                self.raise(f);
                return Vec::new();
            }
        }
        let s = learned.gain_stops.unwrap_or(S_INITIAL).clamp(S_MIN, S_MAX);
        let mut lim = Limits {
            s,
            gs_cap: if lim.gain_usable { self.gs_with(lim.g_cap, s) } else { 0.0 },
            ..*lim
        };
        // The entry timer runs only through `saturated` (2.5.12): any other decision starts it again.
        let over_since = self.over_since.take();
        if self.over {
            let need = self.x(s) + r.e;
            let sure = r.conf >= NO_SIGNAL_CONFIDENCE;
            // Back at the frame-rate limit after the loop went past it: the lowered state has nothing left to do.
            let back = self.over_above && self.e <= lim.e_rate;
            self.over_above |= self.e > lim.e_rate;
            // 2.5.12: the scene needs `OVER_EXIT` stops less than the frame-rate limit and `Most gain` give: back to
            // the frame-rate limit (this decision's allocation takes the exposure down first, then gain). A lowered
            // start (2.5.11) stays only where the entry condition holds.
            let exit = need < f64::from(lim.e_rate) + lim.gs_cap - OVER_EXIT;
            let enter = need > f64::from(lim.e_rate) + lim.gs_cap + OVER_ENTER;
            if back || (sure && (exit || (self.over_seeded && !enter))) {
                self.end_lowered();
                lim.e_cap = lim.e_rate;
            } else if sure {
                self.over_seeded = false;
            }
        }
        let lim = &lim;
        let before = Before {
            e: r.e_plain,
            judgeable: r.conf >= JUDGE_CONFIDENCE && r.clip < JUDGE_CLIP && r.e_plain.abs() < METER_EDGE,
        };
        // W4: while damped the bands are wider.
        let bands = self.watchdog.band_scale(now);

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
            self.over_since = over_since;
            self.saturated(now, lim, &r, up, bands);
            return Vec::new();
        };
        // 4. Band.
        let b_out = OUT_MIN.max(OUT_SIGMA * r.sigma).max(OUT_Q * q) * bands;
        let b_in = IN_MIN.max(IN_SIGMA * r.sigma).max(IN_Q * q) * bands;
        let threshold = if self.band == Band::Holding { b_out } else { b_in };
        if r.e.abs() <= threshold {
            if r.e.abs() <= b_in {
                self.limit_since = None;
                self.dark_since = None;
                self.end_idle_lowered(self.e, lim);
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
            e_before: r.e_plain,
            dgs: self.gs_with(g2, s) - self.gs_with(self.g, s),
        });
        match self.move_to(e2, g2, lim, probe, Some(before)) {
            Some(actions) => actions,
            None => {
                self.end_idle_lowered(self.e, lim);
                self.enter_holding(now);
                Vec::new()
            }
        }
    }

    /// W3: judge the last batch once the picture has had time to answer it.
    ///
    /// A batch is judged only when it predicted at least half a stop and both the decision that made it and the
    /// picture now are confident and unclipped. If the picture has not moved by 15 % of the prediction, the loop
    /// waits until 0.5 s of confident valid samples back that up (each such batch is a strike; three in a row hand
    /// the control back). A picture that did answer ends the strikes.
    fn assess(&mut self, now: f64, valid: &[Sample], r: &Reading) -> Assess {
        let Some(b) = self.batch else {
            return Assess::Go;
        };
        // W3 judges the camera's answer to a write, so it reads the picture without the front light term: the term
        // follows the face, which a write does not move (13.11).
        if r.conf < JUDGE_CONFIDENCE || r.clip >= JUDGE_CLIP || r.e_plain.abs() >= METER_EDGE {
            self.batch = None;
            return Assess::Go;
        }
        if (b.e_before - r.e_plain) / b.predicted >= NO_EFFECT_RATIO {
            self.watchdog.responded();
            self.batch = None;
            return Assess::Go;
        }
        let good: Vec<&Sample> = valid
            .iter()
            .filter(|s| s.conf >= JUDGE_CONFIDENCE && s.clip < JUDGE_CLIP && (s.e - s.front).abs() < METER_EDGE)
            .collect();
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
        let mut tail: Vec<f64> = good.iter().rev().take(MEDIAN_SAMPLES).map(|s| s.e - s.front).collect();
        tail.sort_by(f64::total_cmp);
        let observed = tail[tail.len() / 2];
        self.batch = None;
        if (b.e_before - observed) / b.predicted >= NO_EFFECT_RATIO {
            self.watchdog.responded();
            return Assess::Go;
        }
        match self.watchdog.no_effect() {
            Some(f) => Assess::Fault(f),
            None => Assess::Go,
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
        self.move_to(e2, g2, lim, probe, None)
    }

    /// Nothing has room in the direction the picture needs: saturation (2.5.9). Not a fault; the loop stays engaged.
    fn saturated(&mut self, now: f64, lim: &Limits, r: &Reading, up: bool, bands: f64) {
        let b_out = OUT_MIN.max(OUT_SIGMA * r.sigma) * bands;
        let b_in = IN_MIN.max(IN_SIGMA * r.sigma) * bands;
        // 2.5.12: more than a stop too dark with gain and exposure at their limits, and a longer exposure is allowed:
        // lower the frame rate (the frame-rate guard is not in the middle of a trial).
        let may_lower_rate = up
            && !self.over
            && r.conf >= NO_SIGNAL_CONFIDENCE
            && lim.e_ceil > lim.e_rate
            && r.e > OVER_ENTER
            && matches!(self.guard, Guard::Watching { .. } | Guard::Done);
        if !may_lower_rate {
            self.over_since = None;
        } else if now - *self.over_since.get_or_insert(now) >= LIMIT_AFTER {
            self.over = true;
            self.over_above = false;
            self.lowering = true;
            self.over_since = None;
            self.limit_since = None;
            self.state = ExposureState::Adjusting;
            // Not holding while the move up is due: the card must not say "holding" for it.
            self.band = Band::Adjusting;
            return;
        }
        if r.e.abs() <= b_in {
            self.limit_since = None;
            self.dark_since = None;
            self.enter_holding(now);
        } else if up && r.e > b_out {
            self.dark_since = None;
            let since = *self.limit_since.get_or_insert(now);
            // Not a limit while the frame rate is about to be lowered (2.5.12): that is no limit yet.
            if now - since >= LIMIT_AFTER && !may_lower_rate {
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

    /// 2.5.11: the resume point is also stored when the loop has sat in a limit state for `LIMIT_STORE_AFTER`, at
    /// most once per `RESUME_EVERY`: a dark room never reaches `holding`, and each reopen would climb from the seed.
    fn track_limit(&mut self, now: f64) {
        let limit = matches!(
            self.state,
            ExposureState::LimitGain | ExposureState::LimitExposure | ExposureState::TooBright
        ) && self.no_picture.is_none();
        if !limit {
            self.limit_state_since = None;
            self.limit_stored = false;
            return;
        }
        let since = *self.limit_state_since.get_or_insert(now);
        if !self.limit_stored
            && self.pending.is_empty()
            && now - since >= LIMIT_STORE_AFTER
            && now - self.last_resume_at >= RESUME_EVERY
        {
            self.last_resume_at = now;
            self.limit_stored = true;
            self.resume_out = Some(Resume {
                exposure: Some(self.e),
                gain: self.caps.gain.map(|_| self.g),
                white_balance: None,
            });
        }
    }

    /// 2.5.7: a gain-only batch tells how many stops the gain really gave.
    fn learn(&mut self, r: &Reading, learned: &mut Learned) {
        let Some(p) = self.probe.take() else { return };
        if r.conf < LEARN_MIN_CONFIDENCE || r.clip >= LEARN_MAX_CLIP {
            return;
        }
        let rho = (p.e_before - r.e_plain) / p.dgs;
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
                // Not while the frame rate is lowered on purpose (2.5.12): a low rate is then what the loop asked for.
                let eligible = !self.guard_spent
                    && !self.over
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
        let reportable = match self.state {
            ExposureState::Holding => true,
            ExposureState::LimitGain | ExposureState::LimitExposure | ExposureState::TooBright => self.limit_stored,
            _ => false,
        };
        if self.engaged() && reportable {
            self.resume_out
        } else {
            None
        }
    }

    /// One line for the log while the loop runs: where the camera is, what the limits are and what the picture said
    /// last (`err` in stops, positive = wants more light; `-` before the first decision). With this a dark picture can be
    /// told apart: a loop at its limits (`cap`, `maxG`) from a loop that is on target.
    pub fn diag(&self, learned: &Learned) -> Option<String> {
        if !self.engaged() || self.closed || self.paused {
            return None;
        }
        let lim = self.limits(learned);
        let err = if self.last_e.is_finite() {
            format!("{:+.2}", self.last_e)
        } else {
            "-".to_string()
        };
        let g = self.caps.gain.map_or("-".to_string(), |_| self.g.to_string());
        let max_g = lim.gain_usable.then_some(lim.g_cap).map_or("-".to_string(), |m| m.to_string());
        Some(format!(
            "exposure {:?} E={} G={g} cap={} ({:?}) ceil={} over={} maxG={max_g} S={:.2} err={err} conf={:.2} clip={:.2} bl={:+.2} src={:.2} fl={:+.2} gap={:+.2} sd={:.2} fps={:.1}/{:.0}",
            self.state,
            self.e,
            lim.e_rate,
            lim.reason,
            lim.e_ceil,
            self.over,
            lim.s,
            self.last_conf_clip.0,
            self.last_conf_clip.1,
            self.last_backlight.0,
            self.last_backlight.1,
            self.last_front.0,
            self.last_front.1,
            self.last_front.2,
            self.capture_fps,
            self.f,
        ))
    }

    pub fn status(&self, learned: &Learned, mains: Option<u32>) -> Option<ExposureStatus> {
        if !self.mode_app && self.halt.is_none() {
            return None;
        }
        let lim = self.limits(learned);
        let halted = self.halt;
        let state = if let Some(h) = halted {
            fault_state(h.fault)
        } else if self.closed {
            ExposureState::Closed
        } else if self.paused {
            ExposureState::Paused
        } else if self.no_picture.is_some() {
            ExposureState::NoPicture
        } else {
            self.state
        };
        let gain = self.caps.gain.map(|_| self.g);
        let max_gain = lim.gain_usable.then_some(lim.g_cap);
        let handed_back = halted.and_then(|h| h.to).map(Into::into);
        let message = exposure_message(&ExposureFacts {
            state,
            exposure: self.e,
            gain: if lim.gain_usable { gain } else { None },
            max_gain,
            e_min: self.caps.e_min,
            gain_min: self.caps.gain.map_or(0, |g| g.min),
            gain_off: self.gain_off(&lim),
            cap: lim.e_rate,
            cap_reason: lim.reason,
            // Lowered texts only while the exposure is past the frame-rate limit (or on its way: `lowering`).
            lowered: self.over && self.e > lim.e_rate,
            lowering: self.over && self.lowering,
            fps: self.capture_fps,
            target_fps: self.f,
            dropped_to: self.dropped_to,
            backlit: self.backlit,
            frontlit: self.frontlit,
            fault: halted.map(|h| h.fault),
            handed_back,
            no_picture_for: self.no_picture.unwrap_or(0.0),
        });
        let running = halted.is_none() && !self.closed && !self.paused;
        Some(ExposureStatus {
            state,
            exposure: self.e,
            gain,
            // The frame-rate limit, also while the loop goes past it: the card's tick marks where the frame rate ends.
            cap: lim.e_rate,
            cap_reason: lim.reason,
            max_gain,
            fps: self.capture_fps,
            target_fps: self.f,
            message,
            handed_back,
            note: (running && stripes(self.e, mains)).then(|| STRIPES_NOTE.to_string()),
        })
    }
}

/// The state the card shows for a watchdog fault.
fn fault_state(f: Fault) -> ExposureState {
    match f {
        Fault::NotAnswering | Fault::TooSlow => ExposureState::NotAnswering,
        Fault::Ignored => ExposureState::Ignored,
        Fault::NoResponse => ExposureState::NoResponse,
        Fault::Hunting => ExposureState::Hunting,
        Fault::Conflict => ExposureState::Conflict,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auto_controls::ExposureCfg;

    /// Lit samples every 0.1 s from `from` to `to`, all reading `e`.
    fn lit(h: &mut LitHold, from: f64, to: f64, e: f64) {
        for i in 0..=((to - from) * 10.0).round() as i32 {
            h.push(from + f64::from(i) * 0.1, e);
        }
    }

    /// 13.13 A: a face that stays lit and gets darker (a higher reading) is not followed for `FRONT_HOLD_LONG` s, and
    /// the hold lasts `FRONT_HOLD` after the last lit sample (a lamp that goes off).
    #[test]
    fn a_lit_face_that_stays_lit_is_held_over_the_long_window_and_a_lamp_off_releases_after_two_seconds() {
        let mut h = LitHold::new();
        lit(&mut h, 0.0, 2.0, 0.0);
        lit(&mut h, 2.1, 5.0, 1.0);
        assert_eq!(h.held(5.0), Some(0.0), "the brightest moment of the last 6 s");
        assert_eq!(h.held(6.5), Some(0.0));
        assert_eq!(h.held(5.0 + FRONT_HOLD - 0.05), Some(0.0), "still lit: the long window");
        assert_eq!(h.held(5.0 + FRONT_HOLD + 0.05), None, "no lit sample for FRONT_HOLD: released");
        // A lit face that stays 6 s at the darker level is followed.
        lit(&mut h, 5.1, 9.0, 1.0);
        assert_eq!(h.held(9.0), Some(1.0));
    }

    /// 13.13 F: the hold acts on three lit readings, not on one or two.
    #[test]
    fn the_hold_needs_three_lit_samples() {
        let mut h = LitHold::new();
        assert_eq!(h.held(0.0), None);
        h.push(0.0, -1.0);
        h.push(0.1, -1.0);
        assert_eq!(h.held(0.1), None);
        h.push(0.2, -1.0);
        assert_eq!(h.held(0.2), Some(-1.0));
    }

    /// 13.13 A, E: the hold is lengthened by a lit face that comes back after more than `FRONT_GAP`, before the hold ran
    /// out or after, within `FRONT_BACK_WITHIN`; a longer absence does not lengthen it; a run of lit samples of
    /// `FRONT_STAYED` s ends the long hold.
    #[test]
    fn a_face_that_comes_back_lengthens_the_hold_and_a_long_stay_ends_it() {
        let mut h = LitHold::new();
        lit(&mut h, 0.0, 1.0, 0.0);
        assert_eq!(h.len, FRONT_HOLD);
        // Back after 1.5 s, before the hold ran out.
        lit(&mut h, 2.5, 3.0, 0.0);
        assert_eq!(h.len, FRONT_HOLD_LONG);
        // A long stay (lit samples with no gap longer than FRONT_GAP) ends it.
        lit(&mut h, 3.1, 3.0 + FRONT_STAYED + 0.5, 0.0);
        assert_eq!(h.len, FRONT_HOLD);
        // Back after the hold ran out, within FRONT_BACK_WITHIN: long again; after longer: not.
        let end = 3.0 + FRONT_STAYED + 0.5;
        lit(&mut h, end + 3.0, end + 3.2, 0.0);
        assert_eq!(h.len, FRONT_HOLD_LONG);
        let end = end + 3.2;
        lit(&mut h, end + FRONT_HOLD_LONG + FRONT_BACK_WITHIN + 1.0, end + FRONT_HOLD_LONG + FRONT_BACK_WITHIN + 1.2, 0.0);
        assert_eq!(h.len, FRONT_HOLD);
    }

    /// A hold that ran out is released for good: the readings it held do not come back with the next lit sample.
    #[test]
    fn a_released_hold_does_not_come_back() {
        let mut h = LitHold::new();
        lit(&mut h, 0.0, 1.0, 0.0);
        lit(&mut h, 4.0, 4.5, 5.0);
        assert_eq!(h.held(4.5), Some(5.0));
    }

    /// 13.13 B: `moves` stays small without lit samples (it was pruned in `push` only).
    #[test]
    fn the_write_log_is_bounded_without_lit_samples() {
        let mut h = LitHold::new();
        for i in 0..10_000 {
            h.moved(f64::from(i) * 0.1, 0.2, f64::from(i) * 0.1 - 0.3);
        }
        assert!(h.moves.len() as f64 <= (FRONT_HOLD_LONG + 1.0) / 0.1 + 2.0, "{} entries", h.moves.len());
    }

    /// 13.13 C: older readings are shifted by the batch, those captured while it landed are dropped.
    #[test]
    fn a_landed_batch_shifts_older_readings_and_drops_those_in_between() {
        let mut h = LitHold::new();
        lit(&mut h, 0.0, 1.9, 0.0);
        lit(&mut h, 2.0, 2.9, 0.5);
        h.moved(3.0, 1.0, 2.0);
        assert_eq!(h.ring.len(), 20, "the ten readings of 2.0-2.9 s are gone");
        lit(&mut h, 3.0, 4.0, -1.0);
        // Before the batch the face read 0.0; it would read 1 stop lower now.
        assert_eq!(h.held(4.0), Some(-1.0));
        assert_eq!(h.held(1.9 + 1.0 / 20.0), Some(-1.0), "a reading from before the batch, shifted");
    }

    fn loop_with_lit_samples() -> ExposureLoop {
        let caps = ExposureCaps {
            e_min: -11,
            e_max: -2,
            e_default: -5,
            e_auto: true,
            gain: Some(GainCaps {
                min: 0,
                max: 100,
                step: 1,
                default: 0,
                can_auto: true,
            }),
        };
        let mut l = ExposureLoop::new(caps, ExposureCfg::default());
        l.f = 30.0;
        l.e = -5;
        l.g = 100;
        l.e_rate = -5;
        l.now = 10.0;
        lit(&mut l.lit_hold, 0.0, 1.9, 0.0);
        lit(&mut l.lit_hold, 10.0, 10.3, 0.0);
        l
    }

    fn done(key: ControlKey, requested: i32, readback: Option<i32>, ok: bool) -> WriteDone {
        WriteDone {
            key,
            requested,
            readback,
            ok,
            started: 10.0,
            done: 10.1,
        }
    }

    /// Writes as `move_to` records them: the model's light before, then `issue`.
    fn decided(l: &mut ExposureLoop, writes: Vec<(ControlKey, i32)>) {
        let x_before = l.x(S_INITIAL);
        l.issue(writes, false);
        l.moving = Some(Moving {
            x_before,
            s: S_INITIAL,
            started: f64::INFINITY,
        });
    }

    /// 13.13 C: an exposure write past the frame-rate limit that the camera refuses (it reads back the old value)
    /// changes no light: the lit readings are not shifted, and none is dropped.
    #[test]
    fn a_refused_exposure_write_shifts_nothing() {
        let mut l = loop_with_lit_samples();
        let before = l.lit_hold.ring.len();
        decided(&mut l, vec![(ControlKey::Exposure, -4)]);
        l.on_write_done(done(ControlKey::Exposure, -4, Some(-5), true));
        assert!(l.lit_hold.moves.is_empty(), "{:?}", l.lit_hold.moves);
        assert_eq!(l.lit_hold.ring.len(), before);
    }

    /// The same write taken: one stop, from the first picture after it landed.
    #[test]
    fn a_taken_exposure_write_shifts_by_the_stops_it_made() {
        let mut l = loop_with_lit_samples();
        decided(&mut l, vec![(ControlKey::Exposure, -4)]);
        l.on_write_done(done(ControlKey::Exposure, -4, Some(-4), true));
        let shown = l.valid_from();
        assert_eq!(l.lit_hold.moves.iter().copied().collect::<Vec<_>>(), vec![(shown, 1.0)]);
        // The samples at 10.0-10.3 s were captured while the write landed (started 10.0, shown from about 10.28).
        assert!(l.lit_hold.ring.iter().all(|&(t, _)| t < 10.0 || t >= shown), "{:?}", l.lit_hold.ring);
    }

    /// A two-write batch whose last write failed: the first one still counts (only the last write's `ok` did).
    #[test]
    fn the_first_write_of_a_batch_counts_when_the_last_one_failed() {
        let mut l = loop_with_lit_samples();
        decided(&mut l, vec![(ControlKey::Exposure, -4), (ControlKey::Gain, 50)]);
        l.on_write_done(done(ControlKey::Exposure, -4, Some(-4), true));
        assert!(l.lit_hold.moves.is_empty(), "the batch has not landed");
        l.on_write_done(done(ControlKey::Gain, 50, None, false));
        let shown = l.valid_from();
        assert_eq!(l.lit_hold.moves.iter().copied().collect::<Vec<_>>(), vec![(shown, 1.0)]);
        assert_eq!(l.g, 100, "a failed write is not believed");
    }
}
