//! The watchdog (docs/AUTO-CONTROLS.md 2.7): W1-W6, shared by both loops.
//!
//! One [`Watchdog`] per loop. The loop feeds it what it sees (write results, the moves it makes, read-backs, the
//! clock) and it answers with a [`Fault`] when the loop must stop. What a loop does with a fault (the hand-back
//! actions, the status, `blocked`) is the loop's business; this file only counts and decides.
//!
//! | # | What | Here |
//! |---|---|---|
//! | W1 | a write or read fails 3 times in a row, or one write takes > 3 s | [`Watchdog::write_done`], [`Watchdog::read_failed`], [`Watchdog::too_long`] |
//! | W2 | the read-back differs from the request on 3 writes in a row | [`Watchdog::write_done`] |
//! | W3 | 3 batches in a row that the picture did not answer | [`Watchdog::no_effect`], [`Watchdog::responded`] |
//! | W4 | 4 direction reversals within 10 s: damp for 30 s, then hand back | [`Watchdog::moved`] |
//! | W5 | no sample for 2 s | [`Watchdog::quiet`] |
//! | W6 | the 1 Hz read-back finds another program's value | [`Watchdog::readback_due`], [`Watchdog::drift`] |
//!
//! Also here: slow writes (2.9): the median write time stretches the loop's `gap` and `D_apply`, and over 1 s the
//! camera is too slow to adjust live.

use std::collections::VecDeque;

use super::{HandBack, Mode, WriteDone};
use crate::capture::controls::ControlKey;

/// W1: a failed write is tried again after this long.
const RETRY_AFTER: f64 = 0.5;
/// W1: consecutive failures that stop the loop.
const MAX_FAILURES: u32 = 3;
/// W1: one write taking longer than this stops the loop.
pub const WRITE_TOO_LONG: f64 = 3.0;
/// W2: consecutive read-backs that differ from the request.
const MAX_MISMATCHES: u32 = 3;
/// W3: consecutive unanswered batches.
const MAX_STRIKES: u32 = 3;
/// W3: a batch moved the picture by less than this share of the prediction.
pub const NO_EFFECT_RATIO: f64 = 0.15;
/// W3: how long confident, unclipped samples must have followed the batch before it is judged.
pub const NO_EFFECT_SPAN: f64 = 0.5;
/// W4: reversals of applied moves within the window.
const HUNT_REVERSALS: usize = 4;
const HUNT_WINDOW: f64 = 10.0;
/// W4: how long the damping lasts, and what it does: `k / 2`, the bands times 1.5.
const DAMP_FOR: f64 = 30.0;
const DAMP_K: f64 = 0.5;
const DAMP_BANDS: f64 = 1.5;
/// W5: no sample for this long.
pub const NO_PICTURE_AFTER: f64 = 2.0;
/// W5: the status text shows after this long.
pub const NO_PICTURE_TEXT_AFTER: f64 = 10.0;
/// W6: read-back once a second, when no write happened in the last second.
const READBACK_EVERY: f64 = 1.0;
const READBACK_QUIET: f64 = 1.0;
/// W6: re-takes within the window; one more is a conflict.
const MAX_RETAKES: usize = 3;
const RETAKE_WINDOW: f64 = 60.0;
/// Two mismatches this close are one event (the exposure and the gain read-back of the same meddling).
const SAME_EVENT: f64 = 0.5;
/// 2.9: writes slower than this (median, after the first) stretch the loop.
const SLOW_WRITE: f64 = 0.1;
/// 2.9: a median above this and the camera is too slow to adjust live.
const TOO_SLOW: f64 = 1.0;
/// Write times kept for the median, and how many it needs.
const WRITE_TIMES: usize = 5;
const WRITE_TIMES_NEEDED: usize = 3;

/// Why a loop stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// W1: the camera stopped answering.
    NotAnswering,
    /// 2.9: the writes are too slow for a live loop.
    TooSlow,
    /// W2: the camera ignores the writes. Stored as `blocked`.
    Ignored,
    /// W3: the picture does not change.
    NoResponse,
    /// W4: the loop kept swinging.
    Hunting,
    /// W6: another program keeps changing the control. Nothing is handed back; the loop waits for the user.
    Conflict,
}

impl Fault {
    /// Whether the control is given back (all but a conflict).
    pub fn hands_back(self) -> bool {
        self != Fault::Conflict
    }
}

/// A loop that stopped because of a [`Fault`]; kept until the user changes the mode or resumes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Halt {
    pub fault: Fault,
    /// Where the (main) control went; `None` for a conflict.
    pub to: Option<HandBack>,
    /// The mode the control is in now: the halt stays while the runner confirms it and ends when the mode is
    /// something else.
    pub left_in: Mode,
}

impl Halt {
    pub fn new(fault: Fault, to: Option<HandBack>) -> Halt {
        Halt {
            fault,
            to,
            left_in: match to {
                None => Mode::App,
                Some(HandBack::CameraAuto) => Mode::Camera,
                Some(HandBack::Manual(_)) => Mode::Manual,
            },
        }
    }
}

/// The unit of the loop's moves, for the W3 threshold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unit {
    Stops,
    Mired,
}

impl Unit {
    /// W3: a batch must predict at least this much to be judged (2.7: 0.5 stop; white balance 15 mired, see the
    /// note under 2.7 in the document).
    pub fn min_predicted(self) -> f64 {
        match self {
            Unit::Stops => 0.5,
            Unit::Mired => 15.0,
        }
    }

    /// W4: a move smaller than this is not a direction.
    pub fn min_move(self) -> f64 {
        match self {
            Unit::Stops => 0.15,
            Unit::Mired => 2.0,
        }
    }
}

/// What W4 says about a move.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Moved {
    Ok,
    /// Damping started.
    Damped,
    /// Damped already: hand back.
    Hunting,
}

/// What W6 says about a value that is not ours.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Drift {
    Retake,
    Conflict,
}

#[derive(Clone, Debug)]
pub struct Watchdog {
    unit: Unit,
    // W1 and the write times.
    failures: u32,
    retry_at: f64,
    writes: u32,
    times: VecDeque<f64>,
    // W2
    mismatches: Vec<(ControlKey, u32)>,
    // W3
    strikes: u32,
    // W4
    last_sign: Option<i8>,
    reversals: VecDeque<f64>,
    damped_until: f64,
    // W5
    baseline: Option<f64>,
    // W6
    last_readback: f64,
    retakes: VecDeque<f64>,
    last_drift: f64,
}

impl Watchdog {
    pub fn new(unit: Unit) -> Watchdog {
        Watchdog {
            unit,
            failures: 0,
            retry_at: f64::NEG_INFINITY,
            writes: 0,
            times: VecDeque::new(),
            mismatches: Vec::new(),
            strikes: 0,
            last_sign: None,
            reversals: VecDeque::new(),
            damped_until: f64::NEG_INFINITY,
            baseline: None,
            last_readback: f64::NEG_INFINITY,
            retakes: VecDeque::new(),
            last_drift: f64::NEG_INFINITY,
        }
    }

    pub fn unit(&self) -> Unit {
        self.unit
    }

    /// A new engagement or camera session: every count starts again. The damping and the re-take history stay: they
    /// describe the user's room and the other program, not the session.
    pub fn reset(&mut self) {
        let (damped_until, retakes, last_drift) = (self.damped_until, self.retakes.clone(), self.last_drift);
        *self = Watchdog::new(self.unit);
        self.damped_until = damped_until;
        self.retakes = retakes;
        self.last_drift = last_drift;
    }

    // -----------------------------------------------------------------------------------------------------------
    // W1, W2 and slow writes

    /// A write returned. `matched`: the read-back (if there was one) equals what was requested, after the camera's
    /// range and step.
    pub fn write_done(&mut self, w: &WriteDone, matched: bool) -> Option<Fault> {
        let first = self.writes == 0;
        self.writes += 1;
        if !w.ok {
            self.failures += 1;
            self.retry_at = w.done + RETRY_AFTER;
            return (self.failures >= MAX_FAILURES).then_some(Fault::NotAnswering);
        }
        self.failures = 0;
        let took = (w.done - w.started).max(0.0);
        if took > WRITE_TOO_LONG {
            return Some(Fault::NotAnswering);
        }
        // The first write after an open takes 200-700 ms on the W4GS: not a speed.
        if !first {
            self.times.push_back(took);
            while self.times.len() > WRITE_TIMES {
                self.times.pop_front();
            }
            if self.median().is_some_and(|m| m > TOO_SLOW) {
                return Some(Fault::TooSlow);
            }
        }
        let at = match self.mismatches.iter().position(|(k, _)| *k == w.key) {
            Some(i) => i,
            None => {
                self.mismatches.push((w.key, 0));
                self.mismatches.len() - 1
            }
        };
        let count = &mut self.mismatches[at].1;
        if matched {
            *count = 0;
            None
        } else {
            *count += 1;
            (*count >= MAX_MISMATCHES).then_some(Fault::Ignored)
        }
    }

    /// A read-back failed (the camera did not answer).
    pub fn read_failed(&mut self, now: f64) -> Option<Fault> {
        self.failures += 1;
        self.retry_at = now + RETRY_AFTER;
        (self.failures >= MAX_FAILURES).then_some(Fault::NotAnswering)
    }

    pub fn read_ok(&mut self) {
        self.failures = 0;
    }

    /// W1: no write before this (the retry delay).
    pub fn retry_at(&self) -> f64 {
        self.retry_at
    }

    /// W1: a write issued at `issued_at` that never reported back.
    pub fn too_long(now: f64, issued_at: f64) -> Option<Fault> {
        (now - issued_at > WRITE_TOO_LONG).then_some(Fault::NotAnswering)
    }

    fn median(&self) -> Option<f64> {
        if self.times.len() < WRITE_TIMES_NEEDED {
            return None;
        }
        let mut v: Vec<f64> = self.times.iter().copied().collect();
        v.sort_by(f64::total_cmp);
        Some(v[v.len() / 2])
    }

    /// 2.9: the median write time when it is slow (> 100 ms), else 0. `D_apply` grows by it.
    pub fn slow(&self) -> f64 {
        self.median().filter(|m| *m > SLOW_WRITE).unwrap_or(0.0)
    }

    /// 2.9: `gap = max(gap, 2 x median write time)`.
    pub fn gap(&self, base: f64) -> f64 {
        base.max(2.0 * self.slow())
    }

    // -----------------------------------------------------------------------------------------------------------
    // W3

    /// A judged batch did not move the picture. `Some` on the third in a row.
    pub fn no_effect(&mut self) -> Option<Fault> {
        self.strikes += 1;
        (self.strikes >= MAX_STRIKES).then_some(Fault::NoResponse)
    }

    /// The picture answered a batch.
    pub fn responded(&mut self) {
        self.strikes = 0;
    }

    // -----------------------------------------------------------------------------------------------------------
    // W4

    /// A move of `dx` (stops or mired, signed) is about to be applied.
    pub fn moved(&mut self, now: f64, dx: f64) -> Moved {
        if dx.abs() < self.unit.min_move() {
            return Moved::Ok;
        }
        let sign = if dx > 0.0 { 1 } else { -1 };
        if self.last_sign.is_some_and(|s| s != sign) {
            self.reversals.push_back(now);
        }
        self.last_sign = Some(sign);
        while self.reversals.front().is_some_and(|t| now - t > HUNT_WINDOW) {
            self.reversals.pop_front();
        }
        if self.reversals.len() < HUNT_REVERSALS {
            return Moved::Ok;
        }
        self.reversals.clear();
        if self.damped(now) {
            Moved::Hunting
        } else {
            self.damped_until = now + DAMP_FOR;
            Moved::Damped
        }
    }

    pub fn damped(&self, now: f64) -> bool {
        now < self.damped_until
    }

    /// `k` is multiplied by this.
    pub fn k_scale(&self, now: f64) -> f64 {
        if self.damped(now) {
            DAMP_K
        } else {
            1.0
        }
    }

    /// The hysteresis bands are multiplied by this.
    pub fn band_scale(&self, now: f64) -> f64 {
        if self.damped(now) {
            DAMP_BANDS
        } else {
            1.0
        }
    }

    // -----------------------------------------------------------------------------------------------------------
    // W5

    /// Seconds without a sample: since the newest arrival or since the loop (re)started, whichever is later.
    pub fn quiet(&mut self, now: f64, newest_arrival: f64) -> f64 {
        let base = *self.baseline.get_or_insert(now);
        now - newest_arrival.max(base)
    }

    /// The loop starts again from `now` (a resume): samples are expected from here.
    pub fn touch(&mut self, now: f64) {
        self.baseline = Some(now);
    }

    // -----------------------------------------------------------------------------------------------------------
    // W6

    /// Whether the 1 Hz read-back of the driven controls is due: no write returned in the last second.
    pub fn readback_due(&mut self, now: f64, last_write_done: f64) -> bool {
        if now - last_write_done >= READBACK_QUIET && now - self.last_readback >= READBACK_EVERY {
            self.last_readback = now;
            true
        } else {
            false
        }
    }

    /// A read-back found a value that is not ours (or the Auto flag on).
    pub fn drift(&mut self, now: f64) -> Drift {
        if now - self.last_drift < SAME_EVENT {
            return Drift::Retake;
        }
        self.last_drift = now;
        while self.retakes.front().is_some_and(|t| now - t > RETAKE_WINDOW) {
            self.retakes.pop_front();
        }
        if self.retakes.len() >= MAX_RETAKES {
            return Drift::Conflict;
        }
        self.retakes.push_back(now);
        Drift::Retake
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn done(key: ControlKey, ok: bool, started: f64, took: f64) -> WriteDone {
        WriteDone {
            key,
            requested: 1,
            readback: Some(1),
            ok,
            started,
            done: started + took,
        }
    }

    #[test]
    fn three_failures_stop_the_loop_and_each_delays_the_retry() {
        let mut w = Watchdog::new(Unit::Stops);
        let f = |t| done(ControlKey::Exposure, false, t, 0.005);
        assert_eq!(w.write_done(&f(1.0), true), None);
        assert!((w.retry_at() - 1.505).abs() < 1e-9);
        assert_eq!(w.write_done(&f(2.0), true), None);
        assert_eq!(w.write_done(&f(3.0), true), Some(Fault::NotAnswering));
        // A success in between starts the count again.
        let mut w = Watchdog::new(Unit::Stops);
        w.write_done(&f(1.0), true);
        w.write_done(&f(2.0), true);
        assert_eq!(w.write_done(&done(ControlKey::Exposure, true, 3.0, 0.005), true), None);
        assert_eq!(w.write_done(&f(4.0), true), None);
        assert_eq!(w.write_done(&f(5.0), true), None);
    }

    #[test]
    fn one_slow_write_stops_the_loop() {
        let mut w = Watchdog::new(Unit::Stops);
        assert_eq!(
            w.write_done(&done(ControlKey::Exposure, true, 0.0, 3.2), true),
            Some(Fault::NotAnswering)
        );
        assert_eq!(Watchdog::too_long(10.0, 6.0), Some(Fault::NotAnswering));
        assert_eq!(Watchdog::too_long(8.0, 6.0), None);
    }

    #[test]
    fn three_mismatches_in_a_row_are_an_ignored_control() {
        let mut w = Watchdog::new(Unit::Stops);
        let d = |t| done(ControlKey::Exposure, true, t, 0.005);
        assert_eq!(w.write_done(&d(0.0), false), None);
        assert_eq!(w.write_done(&d(1.0), false), None);
        // The gain's matches do not clear the exposure's count.
        assert_eq!(w.write_done(&done(ControlKey::Gain, true, 1.5, 0.005), true), None);
        assert_eq!(w.write_done(&d(2.0), false), Some(Fault::Ignored));
        let mut w = Watchdog::new(Unit::Stops);
        w.write_done(&d(0.0), false);
        w.write_done(&d(1.0), false);
        assert_eq!(w.write_done(&d(2.0), true), None);
        assert_eq!(w.write_done(&d(3.0), false), None);
    }

    #[test]
    fn slow_writes_stretch_the_gap_and_over_a_second_stop() {
        let mut w = Watchdog::new(Unit::Stops);
        // The first write is the slow first one: not counted.
        w.write_done(&done(ControlKey::Exposure, true, 0.0, 0.6), true);
        for i in 0..2 {
            w.write_done(&done(ControlKey::Exposure, true, 1.0 + f64::from(i), 0.3), true);
        }
        assert_eq!(w.slow(), 0.0, "two samples are not a median");
        w.write_done(&done(ControlKey::Exposure, true, 4.0, 0.3), true);
        assert!((w.slow() - 0.3).abs() < 1e-9);
        assert!((w.gap(0.25) - 0.6).abs() < 1e-9);
        assert!((w.gap(1.0) - 1.0).abs() < 1e-9);
        let mut w = Watchdog::new(Unit::Stops);
        w.write_done(&done(ControlKey::Exposure, true, 0.0, 0.6), true);
        let mut last = None;
        for i in 0..3 {
            last = w.write_done(&done(ControlKey::Exposure, true, 1.0 + f64::from(i) * 2.0, 1.2), true);
        }
        assert_eq!(last, Some(Fault::TooSlow));
    }

    #[test]
    fn three_unanswered_batches_in_a_row_are_no_response() {
        let mut w = Watchdog::new(Unit::Stops);
        assert_eq!(w.no_effect(), None);
        assert_eq!(w.no_effect(), None);
        w.responded();
        assert_eq!(w.no_effect(), None);
        assert_eq!(w.no_effect(), None);
        assert_eq!(w.no_effect(), Some(Fault::NoResponse));
    }

    #[test]
    fn hunting_damps_once_then_hands_back() {
        let mut w = Watchdog::new(Unit::Stops);
        let mut t = 0.0;
        let mut dx = 1.0;
        let mut out = Vec::new();
        // One move every second, alternating direction: the 5th move is the 4th reversal.
        for _ in 0..5 {
            out.push(w.moved(t, dx));
            t += 1.0;
            dx = -dx;
        }
        assert_eq!(out[..4], [Moved::Ok; 4]);
        assert_eq!(out[4], Moved::Damped);
        assert!(w.damped(t));
        assert_eq!((w.k_scale(t), w.band_scale(t)), (0.5, 1.5));
        // Four more reversals while damped: hand back.
        let mut out = Vec::new();
        for _ in 0..5 {
            out.push(w.moved(t, dx));
            t += 1.0;
            dx = -dx;
        }
        assert_eq!(out[..3], [Moved::Ok; 3]);
        assert_eq!(out[3], Moved::Hunting);
        // The damping ends after 30 s.
        assert!(!w.damped(100.0));
        assert_eq!((w.k_scale(100.0), w.band_scale(100.0)), (1.0, 1.0));
    }

    #[test]
    fn slow_swings_and_small_moves_are_not_hunting() {
        let mut w = Watchdog::new(Unit::Stops);
        // A reversal every 4 s: never 4 within 10 s.
        let mut dx = 1.0;
        for i in 0..20 {
            assert_eq!(w.moved(f64::from(i) * 4.0, dx), Moved::Ok);
            dx = -dx;
        }
        let mut w = Watchdog::new(Unit::Stops);
        let mut dx = 0.1;
        for i in 0..20 {
            assert_eq!(w.moved(f64::from(i) * 0.1, dx), Moved::Ok);
            dx = -dx;
        }
    }

    #[test]
    fn no_sample_for_two_seconds_is_quiet() {
        let mut w = Watchdog::new(Unit::Stops);
        // Nothing ever arrived: counted from the first look.
        assert_eq!(w.quiet(10.0, f64::NEG_INFINITY), 0.0);
        assert!((w.quiet(12.5, f64::NEG_INFINITY) - 2.5).abs() < 1e-9);
        assert!((w.quiet(12.5, 12.0) - 0.5).abs() < 1e-9);
        w.touch(20.0);
        assert!((w.quiet(21.0, 12.0) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn readback_waits_for_a_quiet_second_and_runs_once_a_second() {
        let mut w = Watchdog::new(Unit::Stops);
        assert!(!w.readback_due(5.0, 4.5));
        assert!(w.readback_due(5.6, 4.5));
        assert!(!w.readback_due(6.0, 4.5), "less than a second after the last read-back");
        assert!(w.readback_due(6.7, 4.5));
    }

    #[test]
    fn a_fourth_meddling_in_a_minute_is_a_conflict() {
        let mut w = Watchdog::new(Unit::Stops);
        assert_eq!(w.drift(10.0), Drift::Retake);
        // The gain read-back of the same event is not another one.
        assert_eq!(w.drift(10.1), Drift::Retake);
        assert_eq!(w.drift(20.0), Drift::Retake);
        assert_eq!(w.drift(30.0), Drift::Retake);
        assert_eq!(w.drift(40.0), Drift::Conflict);
        // A minute later the old ones are forgotten.
        assert_eq!(w.drift(95.0), Drift::Retake);
    }

    #[test]
    fn a_halt_remembers_where_the_control_went() {
        assert_eq!(Halt::new(Fault::Ignored, Some(HandBack::CameraAuto)).left_in, Mode::Camera);
        assert_eq!(Halt::new(Fault::Ignored, Some(HandBack::Manual(-5))).left_in, Mode::Manual);
        assert_eq!(Halt::new(Fault::Conflict, None).left_in, Mode::App);
        assert!(!Fault::Conflict.hands_back());
        assert!(Fault::Hunting.hands_back());
    }
}
