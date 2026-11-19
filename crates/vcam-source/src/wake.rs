//! How the pump waits: for the writer's frame event when there is one it can trust, else by polling every millisecond
//! (`docs/FRAME-EVENT.md`, phase 3).
//!
//! This is the whole decision logic and it runs in the camera service (Frame Server, LOCAL SERVICE, every camera app
//! depends on it), so it is small, it never creates a named object, and every failure ends in the polling the pump
//! always did. The seqlock stays the only source of truth: an event only says "look now".
//!
//! # Modes
//!
//! | Mode | Entered when | Waits |
//! |---|---|---|
//! | `Poll` | no advert, the writer is not Live, no high-resolution timer, or between two open attempts | 1 ms for `Frame` and `Soon`, the timer for `Until` (today's pump) |
//! | `Event` | advert present, writer Live, both events opened **and** checked ([`vcam_protocol::signal::owner`]) | `Frame(t)`: stop + event + timer; `Until`: stop + timer, at most [`WAIT_CAP`] |
//! | `Demoted` | more than [`SPURIOUS_LIMIT`] spurious wake-ups in 1 s, or [`MISS_LIMIT`] misses in 10 s | as `Poll` for [`DEMOTE_FOR`], then an open is tried again |
//! | `OpenFailed` | the open or the owner check failed | as `Poll`; again at a new pair of nonces (at most once per [`OPEN_RETRY`]) or after [`OPEN_RETRY_SAME`] |
//!
//! # The guards (hard requirements: the nonces are public, so they hold against other users too)
//!
//! * the events are opened only while the writer is **Live with a fresh heartbeat** ([`WakeReport::live`]): never from a
//!   slate state, never at activation, never from the thread that creates the source;
//! * **at most one open attempt per [`OPEN_RETRY`]** whatever the file says, and an attempt at the nonce pair that just
//!   failed only after [`OPEN_RETRY_SAME`]. An attempt also reads the DACL of the frame file, so it is the one thing a
//!   header that changes its nonce every millisecond could make the pump do;
//! * the **owner check** is part of the open: an event that its owner or its label does not vouch for is not used;
//! * **at most one event-driven poll per [`EVENT_MIN_GAP`]**: after an event wake-up the next wait leaves the event out
//!   until the gap has passed, so a signaller cannot drive the pump faster than today's 1 kHz polling;
//! * **demotion**: wake-ups with no signal behind them (spurious: a correct writer makes none) and frames with no
//!   wake-up (missed) switch to polling for 30 s;
//! * every wait is **bounded** by [`WAIT_CAP`] and watches the **stop event**, so a stop never waits for a signal;
//! * `timeBeginPeriod(1)` is held **only while not in `Event` mode**, and ended on every path out, a panic included;
//! * the diagnostics (`wake_mode` and three counters) are written into the file only while its header validates (the
//!   source decides: [`WakeSource::publish_diag`]).
//!
//! # The wait rule
//!
//! The poll loaded `signal_seq == s` with the header, so the next signal sets `E[awaited_event(s)]`
//! (`vcam_protocol::signal`); the pump waits on that event, the stop event and a timer for the state machine's own next
//! deadline. A correct writer never wakes it without a new signal (spurious) and never leaves a frame without a wake-up
//! (missed) except when two signals fall inside the window between the header load and the wait.

use std::collections::VecDeque;
use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

use vcam_protocol::signal::awaited_event;
use vcam_protocol::{wake_mode, WakeAdvert};
use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Media::{timeBeginPeriod, timeEndPeriod};
use windows::Win32::System::Threading::{
    CreateWaitableTimerExW, SetWaitableTimer, WaitForMultipleObjects, WaitForSingleObject,
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS,
};
use windows_core::PCWSTR;

use crate::frames::Wait;

/// After an event wake-up the next wait includes the event only this long after it (ms). A signaller can therefore not
/// drive more than 1000 event-driven polls a second, which is today's worst case (a stalled writer already keeps
/// today's pump at 1 kHz). A correct writer signals at most every ~4 ms (240 fps), so it is never delayed by more.
pub const EVENT_MIN_GAP_MS: u64 = 1;

/// Upper bound of any wait in event mode (ms): the heartbeat (1 s) and the file re-checks keep working whatever the
/// state machine asks for.
pub const WAIT_CAP_MS: u64 = 100;

/// More than this many spurious wake-ups within [`SPURIOUS_WINDOW_MS`] demote to polling. A correct writer produces
/// none, so more than 8 means a second writer or a hostile one.
pub const SPURIOUS_LIMIT: usize = 8;
pub const SPURIOUS_WINDOW_MS: u64 = 1_000;

/// This many missed wake-ups within [`MISS_WINDOW_MS`] demote to polling. The two-signals-in-the-window race is rare;
/// three in 10 s means the writer does not signal (or another writer replaced the nonce).
pub const MISS_LIMIT: usize = 3;
pub const MISS_WINDOW_MS: u64 = 10_000;

/// Polling time after a demotion (ms); then an open is tried again (logged, sampled).
pub const DEMOTE_FOR_MS: u64 = 30_000;

/// Two open attempts are at least this far apart (ms), whatever the nonces are: the pace of the file re-open.
pub const OPEN_RETRY_MS: u64 = 500;

/// An attempt at the nonce pair whose last attempt failed waits this long (ms).
pub const OPEN_RETRY_SAME_MS: u64 = 30_000;

/// A timer wake-up with a frame published and no event set is given this long (ms) before it counts as missed: a
/// writer bumps the frame count a few microseconds before it sets the event, and the deadline can fall in between.
pub const MISS_SETTLE_MS: u64 = 1;

/// Longest single wait of the OS layer (ms): a timer that was set cannot be lost, this only bounds a call that went
/// wrong.
const OS_WAIT_LIMIT_MS: u64 = 5_000;

/// Why a wait ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Woke {
    /// The pump is being stopped.
    Stop,
    /// The frame event the pump waited on was set.
    Event,
    /// The time ran out.
    Timer,
}

/// What the last poll saw of the header.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WakeReport {
    /// The writer is Live with a fresh heartbeat in a file this process can write to: the one state in which the
    /// events are used.
    pub live: bool,
    /// The advert of that poll's header (`signal_seq` as loaded with it).
    pub advert: Option<WakeAdvert>,
    /// `writer_frames` as loaded with the same header.
    pub writer_frames: u64,
}

/// The diagnostics the DLL writes into the header (`set_wake_stats`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Diag {
    pub mode: u32,
    pub events: u64,
    pub missed: u64,
    pub spurious: u64,
}

/// What [`Wake`] asks of the frame source.
pub trait WakeSource {
    /// The header facts of the last poll.
    fn report(&self) -> WakeReport;
    /// `(signal_seq, writer_frames)` now; `None` when no file is mapped.
    fn counters(&self) -> Option<(u64, u64)>;
    /// The user the events must belong to, from the DACL of the mapped frame file (an open and a read: call it once
    /// per attempt). `None`: no single user is named, so no event is trusted.
    fn expected_owner(&mut self) -> Option<String>;
    /// Write the diagnostics; the source does it only while its header validates.
    fn publish_diag(&mut self, d: &Diag);
}

/// The system calls of [`Wake`], so that the state machine runs against a fake clock and fake events in tests.
pub trait Sys {
    /// An opened and checked pair of events.
    type Events;
    /// QPC now.
    fn now(&self) -> u64;
    /// QPC ticks per second.
    fn freq(&self) -> u64;
    /// Whether a timer is there to wait on. Without one the pump polls: a kernel wait with a millisecond timeout is as
    /// coarse as the system timer.
    fn has_timer(&self) -> bool;
    /// Open both events and check who made them against `owner`.
    fn open_events(&mut self, nonces: [u128; 2], owner: &str) -> io::Result<Self::Events>;
    /// Whether event `parity` is set right now.
    fn is_set(&self, ev: &Self::Events, parity: usize) -> bool;
    /// Wait until QPC `until` for the stop event, for event `parity` of `ev` when given, and for the time.
    fn wait(&mut self, ev: Option<(&Self::Events, usize)>, until: u64) -> Woke;
    /// Ask for (`true`) or give back (`false`) the 1 ms system timer resolution. Idempotent.
    fn set_period(&mut self, held: bool);
}

/// Why the stream is demoted to polling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Why {
    Spurious,
    Missed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Poll,
    Event,
    Demoted { until: u64, why: Why },
    OpenFailed,
}

/// The last open attempt: the pacing is keyed by the pair of nonces.
#[derive(Clone, Copy, Debug)]
struct Attempt {
    nonces: [u128; 2],
    at: u64,
    ok: bool,
}

/// The pump's waiter: one per pump, on the pump thread.
pub struct Wake<S: Sys> {
    sys: S,
    freq: u64,
    mode: Mode,
    /// The opened and checked events, and the nonces they were opened for. Present exactly in `Mode::Event`.
    events: Option<(S::Events, [u128; 2])>,
    attempt: Option<Attempt>,
    /// QPC of the last event wake-up (0: none): the next wait leaves the event out until the gap has passed.
    last_event: u64,
    /// Since when the writer is not Live (or has no advert), while events are open.
    not_live_since: Option<u64>,
    /// The last poll showed a Live writer with an advert.
    eligible: bool,
    spurious_at: VecDeque<u64>,
    missed_at: VecDeque<u64>,
    stats: Diag,
    published: Diag,
    published_at: u64,
}

fn ms(freq: u64, n: u64) -> u64 {
    freq.saturating_mul(n) / 1000
}

impl<S: Sys> Wake<S> {
    /// A waiter in polling mode (the 1 ms timer resolution is held, as the pump always did).
    pub fn new(mut sys: S) -> Wake<S> {
        let freq = sys.freq().max(1);
        sys.set_period(true);
        Wake {
            sys,
            freq,
            mode: Mode::Poll,
            events: None,
            attempt: None,
            last_event: 0,
            not_live_since: None,
            eligible: false,
            spurious_at: VecDeque::new(),
            missed_at: VecDeque::new(),
            stats: Diag::default(),
            published: Diag::default(),
            published_at: 0,
        }
    }

    /// The numbers that go into the header.
    pub fn diag(&self) -> Diag {
        Diag {
            mode: self.diag_mode(),
            ..self.stats
        }
    }

    fn diag_mode(&self) -> u32 {
        match self.mode {
            Mode::Event if self.eligible => wake_mode::EVENT,
            Mode::Event | Mode::Poll => wake_mode::POLLING,
            Mode::Demoted {
                why: Why::Spurious, ..
            } => wake_mode::POLLING_SPURIOUS,
            Mode::Demoted {
                why: Why::Missed, ..
            } => wake_mode::POLLING_MISSED,
            Mode::OpenFailed => wake_mode::POLLING_OPEN_FAILED,
        }
    }

    /// Whether the events are in use (tests).
    #[cfg(test)]
    fn in_event_mode(&self) -> bool {
        self.mode == Mode::Event
    }

    fn sync_period(&mut self) {
        // the 1 ms system timer is asked for only while the pump polls
        self.sys.set_period(self.mode != Mode::Event);
    }

    /// Wait as the poll asked (`w`), by events when that is allowed and by polling when not. Returns why it woke.
    pub fn wait(&mut self, w: Wait, src: &mut dyn WakeSource) -> Woke {
        let rep = src.report();
        let now = self.sys.now();
        self.update(now, &rep, src);
        let cap = ms(self.freq, WAIT_CAP_MS);
        let woke = match (self.mode, w) {
            (Mode::Event, Wait::Frame(t)) if rep.live && rep.advert.is_some() => {
                self.event_wait(t, &rep, src)
            }
            // only a timer: decimation, slate pacing, no-buffer back-off. Frames that arrive earlier cannot qualify
            (Mode::Event, Wait::Until(t)) => self.sys.wait(None, t.min(now.saturating_add(cap))),
            (_, Wait::Until(t)) => self.sys.wait(None, t),
            // today's poll: a millisecond
            (_, Wait::Soon | Wait::Frame(_)) => {
                let n = self.sys.now();
                self.sys.wait(None, n.saturating_add(ms(self.freq, 1)))
            }
        };
        let after = self.sys.now();
        self.publish(src, after, false);
        woke
    }

    /// The pump is ending: write the last numbers.
    pub fn finish(&mut self, src: &mut dyn WakeSource) {
        let now = self.sys.now();
        self.publish(src, now, true);
    }

    fn publish(&mut self, src: &mut dyn WakeSource, now: u64, force: bool) {
        let d = self.diag();
        if d == self.published {
            return;
        }
        // a change of mode goes out at once, counters at most four times a second
        let due = force
            || d.mode != self.published.mode
            || now.saturating_sub(self.published_at) >= ms(self.freq, 250);
        if due {
            src.publish_diag(&d);
            self.published = d;
            self.published_at = now;
        }
    }

    /// Bring the mode up to date with what the last poll saw.
    fn update(&mut self, now: u64, rep: &WakeReport, src: &mut dyn WakeSource) {
        if let Mode::Demoted { until, .. } = self.mode {
            if now >= until {
                self.mode = Mode::Poll;
                self.sync_period();
                crate::log::log_sampled(crate::log::slot::WAKE_DEMOTE, 10, || {
                    "wake: the demotion is over: event mode may be tried again".into()
                });
            }
        }
        // events are used only for a Live writer with an advert (a slate state, a stale heartbeat or an old writer give
        // timer-only waits), and only with a timer to wait on
        self.eligible = rep.live && rep.advert.is_some() && self.sys.has_timer();
        let Some(advert) = rep.advert.filter(|_| self.eligible) else {
            // not now: events that are open are given up when the writer has been away for longer than a wait
            if self.mode == Mode::Event {
                let since = *self.not_live_since.get_or_insert(now);
                if now.saturating_sub(since) > ms(self.freq, WAIT_CAP_MS) {
                    self.close_events();
                }
            }
            return;
        };
        self.not_live_since = None;
        match self.mode {
            Mode::Event => {
                if self.events.as_ref().is_some_and(|e| e.1 == advert.nonces) {
                    return;
                }
                // the writer started over: old handles go, new ones are opened when the pacing allows
                self.close_events();
                self.try_open(now, advert.nonces, src);
            }
            Mode::Poll | Mode::OpenFailed => self.try_open(now, advert.nonces, src),
            Mode::Demoted { .. } => {}
        }
    }

    fn close_events(&mut self) {
        self.events = None;
        self.mode = Mode::Poll;
        self.not_live_since = None;
        self.sync_period();
    }

    /// Whether an open attempt at `nonces` may be made at QPC `now`.
    fn may_open(&self, nonces: [u128; 2], now: u64) -> bool {
        let Some(last) = self.attempt else {
            return true;
        };
        if now < last.at.saturating_add(ms(self.freq, OPEN_RETRY_MS)) {
            return false;
        }
        // the pair that just failed is left alone for longer: nothing about it changes by itself
        !(last.nonces == nonces
            && !last.ok
            && now < last.at.saturating_add(ms(self.freq, OPEN_RETRY_SAME_MS)))
    }

    fn try_open(&mut self, now: u64, nonces: [u128; 2], src: &mut dyn WakeSource) {
        if !self.may_open(nonces, now) {
            return;
        }
        let result = match src.expected_owner() {
            Some(owner) => self.sys.open_events(nonces, &owner),
            None => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "the DACL of the frame file names no single user: no event is trusted",
            )),
        };
        self.attempt = Some(Attempt {
            nonces,
            at: now,
            ok: result.is_ok(),
        });
        match result {
            Ok(ev) => {
                self.events = Some((ev, nonces));
                self.mode = Mode::Event;
                self.last_event = 0;
                self.spurious_at.clear();
                self.missed_at.clear();
                crate::log::log_sampled(crate::log::slot::WAKE_OPEN, 20, || {
                    format!("wake: frame events opened ({:08x}...)", nonces[0] >> 96)
                });
            }
            Err(e) => {
                self.events = None;
                self.mode = Mode::OpenFailed;
                crate::log::log_sampled(crate::log::slot::WAKE_OPEN, 20, || {
                    format!(
                        "wake: frame events not used ({:08x}...): {e}",
                        nonces[0] >> 96
                    )
                });
            }
        }
        self.sync_period();
    }

    fn demote(&mut self, now: u64, why: Why) {
        self.events = None;
        self.mode = Mode::Demoted {
            until: now.saturating_add(ms(self.freq, DEMOTE_FOR_MS)),
            why,
        };
        self.spurious_at.clear();
        self.missed_at.clear();
        self.sync_period();
        let (st, why) = (self.stats, why);
        crate::log::log_sampled(crate::log::slot::WAKE_DEMOTE, 10, || {
            format!(
                "wake: back to polling for {} s after {:?} wake-ups (events {} missed {} spurious {})",
                DEMOTE_FOR_MS / 1000,
                why,
                st.events,
                st.missed,
                st.spurious
            )
        });
    }

    fn note_spurious(&mut self, now: u64) {
        self.stats.spurious += 1;
        self.spurious_at.push_back(now);
        while self.spurious_at.len() > SPURIOUS_LIMIT + 1 {
            self.spurious_at.pop_front();
        }
        if self.spurious_at.len() > SPURIOUS_LIMIT
            && now.saturating_sub(self.spurious_at[0]) <= ms(self.freq, SPURIOUS_WINDOW_MS)
        {
            self.demote(now, Why::Spurious);
        }
    }

    fn note_missed(&mut self, now: u64) {
        self.stats.missed += 1;
        self.missed_at.push_back(now);
        while self.missed_at.len() > MISS_LIMIT {
            self.missed_at.pop_front();
        }
        if self.missed_at.len() >= MISS_LIMIT
            && now.saturating_sub(self.missed_at[0]) <= ms(self.freq, MISS_WINDOW_MS)
        {
            self.demote(now, Why::Missed);
        }
    }

    /// `Wait::Frame(t)` in event mode: the stop event, the event the next signal sets, and a timer for `t` (at most
    /// [`WAIT_CAP_MS`] away).
    fn event_wait(&mut self, t: u64, rep: &WakeReport, src: &mut dyn WakeSource) -> Woke {
        let Some(advert) = rep.advert else {
            let n = self.sys.now();
            return self.sys.wait(None, n.saturating_add(ms(self.freq, 1)));
        };
        let (s, f) = (advert.signal_seq, rep.writer_frames);
        let parity = awaited_event(s);
        let now = self.sys.now();
        let deadline = t.min(now.saturating_add(ms(self.freq, WAIT_CAP_MS)));
        // at most one event-driven poll per gap: the event is left out until the gap after the last wake-up has passed
        let from = match self.last_event {
            0 => 0,
            at => at.saturating_add(ms(self.freq, EVENT_MIN_GAP_MS)),
        };
        if now < from {
            let until = from.min(deadline);
            if self.sys.wait(None, until) == Woke::Stop {
                return Woke::Stop;
            }
            if until >= deadline {
                return Woke::Timer;
            }
        }
        let Some((events, _)) = self.events.as_ref() else {
            return self.sys.wait(None, deadline);
        };
        match self.sys.wait(Some((events, parity)), deadline) {
            Woke::Stop => Woke::Stop,
            Woke::Event => {
                let at = self.sys.now();
                self.last_event = at;
                match src.counters() {
                    // a wake-up with no signal behind it: the count is where the poll loaded it
                    Some((seq, _)) if seq == s => self.note_spurious(at),
                    _ => self.stats.events += 1,
                }
                Woke::Event
            }
            Woke::Timer => {
                // a frame was published since the poll and the event is not set: a wake-up may have been lost
                if src.counters().is_some_and(|(_, frames)| frames != f) {
                    self.settle(parity)
                } else {
                    Woke::Timer
                }
            }
        }
    }

    /// A timer wake-up with a newer frame in the file: give the event a moment (the writer bumps the count a few
    /// microseconds before it sets it), then count a miss when it is still not set.
    fn settle(&mut self, parity: usize) -> Woke {
        let Some((events, _)) = self.events.as_ref() else {
            return Woke::Timer;
        };
        if self.sys.is_set(events, parity) {
            return Woke::Timer; // the signal came with the timer: nothing was lost
        }
        let until = self.sys.now().saturating_add(ms(self.freq, MISS_SETTLE_MS));
        match self.sys.wait(Some((events, parity)), until) {
            Woke::Stop => Woke::Stop,
            Woke::Event => {
                // late, not lost
                self.last_event = self.sys.now();
                self.stats.events += 1;
                Woke::Event
            }
            Woke::Timer => {
                let still = self
                    .events
                    .as_ref()
                    .is_some_and(|(e, _)| self.sys.is_set(e, parity));
                if !still {
                    let at = self.sys.now();
                    self.note_missed(at);
                }
                Woke::Timer
            }
        }
    }
}

// ---- the real system calls -----------------------------------------------------------------------------------------

#[cfg(test)]
thread_local! {
    /// The `TimerPeriod`s that are open on this thread (tests: a panic must not leave one behind).
    pub(crate) static OPEN_PERIODS: std::cell::Cell<i32> = const { std::cell::Cell::new(0) };
}

/// `timeBeginPeriod(1)` for the life of the value, ended on every path out of the pump, a panic included.
pub(crate) struct TimerPeriod;

impl TimerPeriod {
    pub(crate) fn begin() -> TimerPeriod {
        unsafe { timeBeginPeriod(1) };
        #[cfg(test)]
        OPEN_PERIODS.with(|c| c.set(c.get() + 1));
        TimerPeriod
    }
}

impl Drop for TimerPeriod {
    fn drop(&mut self) {
        unsafe { timeEndPeriod(1) };
        #[cfg(test)]
        OPEN_PERIODS.with(|c| c.set(c.get() - 1));
    }
}

/// What a test can see of the pumps running on the frame file `path`: how many waits were made and whether the 1 ms
/// timer resolution is held.
#[cfg(test)]
pub(crate) mod probe {
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    #[derive(Clone, Copy, Debug, Default)]
    pub struct Seen {
        /// Waits made, and of them the ones that included a frame event.
        pub waits: u64,
        pub event_waits: u64,
        /// `timeBeginPeriod` calls that are open now, and made in all.
        pub periods_held: i32,
        pub period_begins: u64,
        /// Open attempts made, and pairs of events that are open now.
        pub opens: u64,
        pub events_live: i32,
        /// CPU cycles of the pump thread, written when the pump ends.
        pub cpu_cycles: u64,
        /// Pump waiters ([`super::Os`]: the timer, the period, the frame events) made, and alive now.
        pub waiters: u64,
        pub waiters_live: i32,
    }

    static SEEN: Mutex<Vec<(PathBuf, Seen)>> = Mutex::new(Vec::new());

    pub fn with(path: &Path, f: impl FnOnce(&mut Seen)) {
        let mut g = SEEN.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(e) = g.iter_mut().find(|e| e.0 == path) {
            f(&mut e.1);
        } else {
            let mut s = Seen::default();
            f(&mut s);
            g.push((path.to_path_buf(), s));
        }
    }

    pub fn get(path: &Path) -> Seen {
        let g = SEEN.lock().unwrap_or_else(|e| e.into_inner());
        g.iter()
            .find(|e| e.0 == path)
            .map(|e| e.1)
            .unwrap_or_default()
    }

    pub fn reset(path: &Path) {
        let mut g = SEEN.lock().unwrap_or_else(|e| e.into_inner());
        g.retain(|e| e.0 != path);
    }
}

/// The calls of the pump thread: its stop event, a high-resolution timer, the frame events and the timer period.
pub struct Os {
    /// The pump's stop event (owned by the pump's shared state, which outlives the thread).
    stop: Option<HANDLE>,
    timer: Option<OwnedHandle>,
    period: Option<TimerPeriod>,
    freq: u64,
    /// The frame file this pump reads: the key of what the tests can see of it ([`probe`]).
    #[cfg_attr(not(test), allow(dead_code))]
    path: std::path::PathBuf,
}

impl Os {
    /// `stop`: the event that ends the pump's waits. A missing one (it could not be made) limits every wait to a
    /// millisecond-sized step, so that the pump still sees the stop flag.
    pub fn new(stop: Option<HANDLE>, path: std::path::PathBuf) -> Os {
        let timer = unsafe {
            CreateWaitableTimerExW(
                None,
                PCWSTR::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS.0,
            )
        }
        .ok()
        // SAFETY: a new handle that nothing else owns.
        .map(|h| unsafe { OwnedHandle::from_raw_handle(h.0) });
        #[cfg(test)]
        probe::with(&path, |s| {
            s.waiters += 1;
            s.waiters_live += 1;
        });
        Os {
            stop,
            timer,
            period: None,
            freq: vcam_protocol::qpc_frequency(),
            path,
        }
    }

    fn raw_timer(&self) -> Option<HANDLE> {
        self.timer.as_ref().map(|t| HANDLE(t.as_raw_handle()))
    }
}

/// An opened and checked pair of events: closed when it goes.
pub struct Events {
    handles: [OwnedHandle; 2],
    #[cfg(test)]
    key: std::path::PathBuf,
}

#[cfg(test)]
impl Drop for Events {
    fn drop(&mut self) {
        probe::with(&self.key, |s| s.events_live -= 1);
    }
}

impl Sys for Os {
    type Events = Events;

    fn now(&self) -> u64 {
        vcam_protocol::qpc_now()
    }

    fn freq(&self) -> u64 {
        self.freq
    }

    fn has_timer(&self) -> bool {
        self.timer.is_some()
    }

    fn open_events(&mut self, nonces: [u128; 2], owner: &str) -> io::Result<Events> {
        #[cfg(test)]
        probe::with(&self.path, |s| s.opens += 1);
        let handles = vcam_protocol::signal::owner::open_checked_events(nonces, owner)?;
        #[cfg(test)]
        probe::with(&self.path, |s| s.events_live += 1);
        Ok(Events {
            handles,
            #[cfg(test)]
            key: self.path.clone(),
        })
    }

    fn is_set(&self, ev: &Events, parity: usize) -> bool {
        unsafe {
            WaitForSingleObject(HANDLE(ev.handles[parity & 1].as_raw_handle()), 0) == WAIT_OBJECT_0
        }
    }

    fn wait(&mut self, ev: Option<(&Events, usize)>, until: u64) -> Woke {
        #[cfg(test)]
        probe::with(&self.path, |s| {
            s.waits += 1;
            s.event_waits += ev.is_some() as u64;
        });
        let left = until.saturating_sub(self.now());
        if left == 0 {
            return Woke::Timer;
        }
        // stop first: of several signalled objects the lowest index is reported
        let mut handles: Vec<HANDLE> = Vec::with_capacity(3);
        let stop_at = self.stop.map(|s| {
            handles.push(s);
            handles.len() - 1
        });
        let ev_at = ev.map(|(e, p)| {
            handles.push(HANDLE(e.handles[p & 1].as_raw_handle()));
            handles.len() - 1
        });
        // without a stop event the pump could not be woken, so no wait is long: it looks at the stop flag every 20 ms
        let limit = if self.stop.is_some() {
            OS_WAIT_LIMIT_MS
        } else {
            20
        };
        let left_ms = ((left as u128 * 1000 / self.freq as u128) as u64).clamp(1, limit);
        let mut timeout_ms = left_ms as u32;
        let mut timer_at = None;
        if let Some(t) = self.raw_timer() {
            // a relative due time (negative, 100 ns units) from the exact distance, not the rounded milliseconds
            let due = (left as u128 * 10_000_000 / self.freq as u128)
                .clamp(1, limit as u128 * 10_000) as i64;
            if unsafe { SetWaitableTimer(t, &-due, 0, None, None, false) }.is_ok() {
                handles.push(t);
                timer_at = Some(handles.len() - 1);
                // the timer is what ends the wait; the timeout is a net under a call that went wrong
                timeout_ms = (left_ms + 50) as u32;
            }
        }
        if handles.is_empty() {
            // no stop event, no timer, no event: a short step, so that the stop flag is seen soon
            std::thread::sleep(std::time::Duration::from_millis(1));
            return Woke::Timer;
        }
        let r = unsafe { WaitForMultipleObjects(&handles, false, timeout_ms) };
        if r == WAIT_TIMEOUT {
            return Woke::Timer;
        }
        let i = r.0.wrapping_sub(WAIT_OBJECT_0.0) as usize;
        if Some(i) == stop_at {
            Woke::Stop
        } else if Some(i) == ev_at {
            Woke::Event
        } else if Some(i) == timer_at {
            Woke::Timer
        } else {
            // WAIT_FAILED or an abandoned object: never spin on it
            std::thread::sleep(std::time::Duration::from_millis(1));
            Woke::Timer
        }
    }

    fn set_period(&mut self, held: bool) {
        match (held, self.period.is_some()) {
            (true, false) => {
                self.period = Some(TimerPeriod::begin());
                #[cfg(test)]
                probe::with(&self.path, |s| {
                    s.periods_held += 1;
                    s.period_begins += 1;
                });
            }
            (false, true) => {
                self.period = None;
                #[cfg(test)]
                probe::with(&self.path, |s| s.periods_held -= 1);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
impl Drop for Os {
    fn drop(&mut self) {
        // the timer period ends with `period`; the probe must see it go
        if self.period.is_some() {
            probe::with(&self.path, |s| s.periods_held -= 1);
        }
        let mut cycles = 0u64;
        let _ = unsafe {
            windows::Win32::System::WindowsProgramming::QueryThreadCycleTime(
                windows::Win32::System::Threading::GetCurrentThread(),
                &mut cycles,
            )
        };
        probe::with(&self.path, |s| {
            s.cpu_cycles = cycles;
            s.waiters_live -= 1;
        });
    }
}

#[cfg(test)]
mod tests;
