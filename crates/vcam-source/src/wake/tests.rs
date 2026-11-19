//! The state machine of [`Wake`] against a fake clock and fake events: no thread, no kernel object, no sleep. Time is in
//! microseconds (`freq` is 1 000 000), so a test of "30 seconds later" runs in no time.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use super::*;
use vcam_protocol::wake_mode;

const MS: u64 = 1_000;
const SEC: u64 = 1_000_000;

fn advert(s: u64, n: u128) -> Option<WakeAdvert> {
    Some(WakeAdvert {
        nonces: [n, n + 1],
        signal_seq: s,
    })
}

/// What the fake world records and what a test scripts.
#[derive(Default)]
struct World {
    now: u64,
    /// Outcomes of the next waits that include an event (`Timer` when the list is empty).
    script: VecDeque<Woke>,
    /// Outcomes of the next timer-only waits (`Timer` when empty).
    timer_script: VecDeque<Woke>,
    /// Results of the next opens (`Ok` when empty).
    opens_result: VecDeque<bool>,
    /// (now, nonces, owner) of every open attempt.
    opens: Vec<(u64, [u128; 2], String)>,
    /// (now, parity of the event waited on, until) of every wait.
    waits: Vec<(u64, Option<usize>, u64)>,
    /// The result of `is_set` for each parity.
    set: [bool; 2],
    /// Every `set_period` call.
    periods: Vec<bool>,
    no_timer: bool,
}

#[derive(Clone, Default)]
struct Fake(Rc<RefCell<World>>);

impl Sys for Fake {
    type Events = ();

    fn now(&self) -> u64 {
        self.0.borrow().now
    }
    fn freq(&self) -> u64 {
        SEC
    }
    fn has_timer(&self) -> bool {
        !self.0.borrow().no_timer
    }
    fn open_events(&mut self, nonces: [u128; 2], owner: &str) -> io::Result<()> {
        let mut w = self.0.borrow_mut();
        let now = w.now;
        w.opens.push((now, nonces, owner.to_string()));
        if w.opens_result.pop_front().unwrap_or(true) {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "not the writer's",
            ))
        }
    }
    fn is_set(&self, _ev: &(), parity: usize) -> bool {
        self.0.borrow().set[parity & 1]
    }
    fn wait(&mut self, ev: Option<(&(), usize)>, until: u64) -> Woke {
        let mut w = self.0.borrow_mut();
        let now = w.now;
        w.waits.push((now, ev.map(|e| e.1), until));
        let outcome = if ev.is_some() {
            w.script.pop_front().unwrap_or(Woke::Timer)
        } else {
            w.timer_script.pop_front().unwrap_or(Woke::Timer)
        };
        match outcome {
            Woke::Timer => w.now = w.now.max(until),
            // an event comes some microseconds in, at most when the wait would have ended
            Woke::Event => w.now = (w.now + 20).min(until.max(w.now + 1)),
            Woke::Stop => w.now += 1,
        }
        outcome
    }
    fn set_period(&mut self, held: bool) {
        self.0.borrow_mut().periods.push(held);
    }
}

/// A frame source with scripted facts.
struct Src {
    world: Rc<RefCell<World>>,
    rep: WakeReport,
    seq: u64,
    frames: u64,
    owner: Option<String>,
    owner_calls: u32,
    diag: Vec<Diag>,
}

impl WakeSource for Src {
    fn report(&self) -> WakeReport {
        self.rep
    }
    fn counters(&self) -> Option<(u64, u64)> {
        Some((self.seq, self.frames))
    }
    fn expected_owner(&mut self) -> Option<String> {
        self.owner_calls += 1;
        self.owner.clone()
    }
    fn publish_diag(&mut self, d: &Diag) {
        let _ = &self.world;
        self.diag.push(*d);
    }
}

fn rig() -> (Wake<Fake>, Fake, Src) {
    let fake = Fake::default();
    let src = Src {
        world: fake.0.clone(),
        rep: WakeReport {
            live: true,
            advert: advert(10, 100),
            writer_frames: 5,
        },
        seq: 10,
        frames: 5,
        owner: Some("S-1-5-21-1-2-3-1001".into()),
        owner_calls: 0,
        diag: Vec::new(),
    };
    (Wake::new(fake.clone()), fake, src)
}

fn now(f: &Fake) -> u64 {
    f.0.borrow().now
}

/// Open the events (the first `Frame` wait with a live writer and an advert does it).
fn into_event_mode(w: &mut Wake<Fake>, f: &Fake, s: &mut Src) {
    let t = now(f) + 50 * MS;
    w.wait(Wait::Frame(t), s);
    assert!(w.in_event_mode());
}

#[test]
fn a_pump_without_an_advert_polls_every_millisecond_and_never_opens_anything() {
    let (mut w, f, mut s) = rig();
    s.rep.advert = None;
    for _ in 0..50 {
        let t = now(&f) + 40 * MS;
        w.wait(Wait::Frame(t), &mut s);
    }
    let world = f.0.borrow();
    assert!(world.opens.is_empty());
    assert_eq!(s.owner_calls, 0, "the file's DACL was read for nothing");
    assert!(world.waits.iter().all(|x| x.1.is_none()), "no event wait");
    // each wait is one millisecond long: today's poll
    for (at, _, until) in &world.waits {
        assert_eq!(until - at, MS);
    }
    // the timer resolution is asked for at the start and never given back
    assert_eq!(world.periods, vec![true]);
    assert_eq!(w.diag().mode, wake_mode::POLLING);
}

#[test]
fn polling_mode_waits_what_the_pump_always_waited() {
    let (mut w, f, mut s) = rig();
    s.rep.live = false; // no events: the polling mode
    for wait in [
        Wait::Soon,
        Wait::Frame(now(&f) + 10 * MS),
        Wait::Frame(now(&f) + 90 * MS),
        Wait::Frame(now(&f) + 1),
    ] {
        let at = now(&f);
        w.wait(wait, &mut s);
        let (_, ev, until) = *f.0.borrow().waits.last().unwrap();
        assert_eq!(ev, None);
        assert_eq!(
            until - at,
            MS,
            "{wait:?}: Soon and Frame are one millisecond"
        );
    }
    // Until is the timer to that time, however far (the condvar wait of the old pump)
    let at = now(&f);
    w.wait(Wait::Until(at + 700 * MS), &mut s);
    assert_eq!(f.0.borrow().waits.last().unwrap().2, at + 700 * MS);
}

#[test]
fn a_live_writer_with_an_advert_gets_its_events_opened_once_with_the_user_of_the_file() {
    let (mut w, f, mut s) = rig();
    into_event_mode(&mut w, &f, &mut s);
    {
        let world = f.0.borrow();
        assert_eq!(world.opens.len(), 1);
        assert_eq!(world.opens[0].1, [100, 101]);
        assert_eq!(world.opens[0].2, "S-1-5-21-1-2-3-1001");
        // the wait is on the event the next signal sets: loaded 10, so E[1]
        let last = world.waits.last().unwrap();
        assert_eq!(last.1, Some(1));
        // the 1 ms timer resolution was given back
        assert_eq!(world.periods, vec![true, false]);
    }
    assert_eq!(w.diag().mode, wake_mode::EVENT);
    // the next wait uses the same events (no second open) and, loaded 11, E[0]
    s.rep.advert = advert(11, 100);
    s.seq = 11;
    let t = now(&f) + 50 * MS;
    w.wait(Wait::Frame(t), &mut s);
    let world = f.0.borrow();
    assert_eq!(world.opens.len(), 1);
    assert_eq!(world.waits.last().unwrap().1, Some(0));
    assert_eq!(s.owner_calls, 1);
}

#[test]
fn nothing_is_opened_unless_the_writer_is_live() {
    // a slate state: every one of them has `live == false` in the report
    let (mut w, f, mut s) = rig();
    s.rep.live = false;
    for _ in 0..20 {
        w.wait(Wait::Until(now(&f) + 33 * MS), &mut s);
        w.wait(Wait::Frame(now(&f) + 33 * MS), &mut s);
        f.0.borrow_mut().now += 600 * MS; // well past any pacing
    }
    assert!(f.0.borrow().opens.is_empty());
    assert_eq!(s.owner_calls, 0);
    assert!(!w.in_event_mode());
    // live again: now it opens
    s.rep.live = true;
    w.wait(Wait::Frame(now(&f) + 33 * MS), &mut s);
    assert_eq!(f.0.borrow().opens.len(), 1);
}

#[test]
fn the_events_are_not_used_while_the_writer_is_not_live_and_are_given_up_after_a_while() {
    let (mut w, f, mut s) = rig();
    into_event_mode(&mut w, &f, &mut s);
    // a slate state: Until waits are timer-only, the events stay open for a moment
    s.rep.live = false;
    w.wait(Wait::Until(now(&f) + 33 * MS), &mut s);
    assert!(w.in_event_mode());
    assert!(
        f.0.borrow().waits.last().unwrap().1.is_none(),
        "a timer-only wait"
    );
    assert_eq!(w.diag().mode, wake_mode::POLLING, "not live: polling");
    // away for longer than a wait: closed, the timer resolution is held again
    f.0.borrow_mut().now += 150 * MS;
    w.wait(Wait::Until(now(&f) + 33 * MS), &mut s);
    assert!(!w.in_event_mode());
    assert_eq!(f.0.borrow().periods.last(), Some(&true));
    // and back: the same pair is opened again, but not before the pacing allows (500 ms after the last attempt)
    s.rep.live = true;
    w.wait(Wait::Frame(now(&f) + 33 * MS), &mut s);
    assert!(!w.in_event_mode(), "reopened inside the pacing");
    f.0.borrow_mut().now += 600 * MS;
    w.wait(Wait::Frame(now(&f) + 33 * MS), &mut s);
    assert!(w.in_event_mode());
    assert_eq!(f.0.borrow().opens.len(), 2);
}

#[test]
fn a_pair_that_cannot_be_trusted_is_retried_after_30_seconds_and_a_new_pair_after_500_ms() {
    let (mut w, f, mut s) = rig();
    f.0.borrow_mut().opens_result = VecDeque::from([false, false, false, false, false]);
    w.wait(Wait::Frame(now(&f) + 50 * MS), &mut s);
    assert_eq!(f.0.borrow().opens.len(), 1);
    assert_eq!(w.diag().mode, wake_mode::POLLING_OPEN_FAILED);
    assert!(!w.in_event_mode());
    // the same pair, polled for 29 s: not tried again
    let t0 = now(&f);
    while now(&f) < t0 + 29 * SEC {
        f.0.borrow_mut().now += 10 * MS;
        w.wait(Wait::Frame(now(&f) + 50 * MS), &mut s);
    }
    assert_eq!(f.0.borrow().opens.len(), 1, "the failed pair was retried");
    assert_eq!(s.owner_calls, 1);
    // after 30 s
    f.0.borrow_mut().now += 2 * SEC;
    w.wait(Wait::Frame(now(&f) + 50 * MS), &mut s);
    assert_eq!(f.0.borrow().opens.len(), 2);

    // a pair that changes: not before 500 ms after the last attempt
    let t0 = f.0.borrow().opens[1].0;
    s.rep.advert = advert(10, 200);
    f.0.borrow_mut().now = t0 + 499 * MS;
    w.wait(Wait::Frame(now(&f) + 50 * MS), &mut s);
    assert_eq!(f.0.borrow().opens.len(), 2, "a new pair 499 ms after");
    f.0.borrow_mut().now = t0 + 501 * MS;
    w.wait(Wait::Frame(now(&f) + 50 * MS), &mut s);
    assert_eq!(f.0.borrow().opens.len(), 3);
    assert_eq!(f.0.borrow().opens[2].1, [200, 201]);
}

#[test]
fn a_nonce_that_changes_every_millisecond_costs_at_most_one_open_per_500_ms() {
    let (mut w, f, mut s) = rig();
    // each pair would open fine: the pacing alone is what limits the opens
    let mut n = 1000u128;
    let start = now(&f);
    while now(&f) < start + 10 * SEC {
        n += 2;
        s.rep.advert = advert(1, n);
        f.0.borrow_mut().now += MS;
        w.wait(Wait::Frame(now(&f) + 50 * MS), &mut s);
    }
    // 10 s of changing nonces
    let opens = f.0.borrow().opens.clone();
    assert!(opens.len() <= 21, "{} opens in 10 s", opens.len());
    assert!(opens.len() >= 15, "the pacing is too slow: {}", opens.len());
    for pair in opens.windows(2) {
        assert!(
            pair[1].0 - pair[0].0 >= 500 * MS,
            "two opens {} us apart",
            pair[1].0 - pair[0].0
        );
    }
    assert_eq!(
        s.owner_calls as usize,
        opens.len(),
        "one DACL read per attempt"
    );
}

#[test]
fn no_user_in_the_dacl_means_no_events() {
    let (mut w, f, mut s) = rig();
    s.owner = None;
    w.wait(Wait::Frame(now(&f) + 50 * MS), &mut s);
    assert!(
        f.0.borrow().opens.is_empty(),
        "opened without an owner to check"
    );
    assert!(!w.in_event_mode());
    assert_eq!(w.diag().mode, wake_mode::POLLING_OPEN_FAILED);
}

#[test]
fn without_a_timer_the_pump_polls_whatever_the_file_says() {
    let (mut w, f, mut s) = rig();
    f.0.borrow_mut().no_timer = true;
    for _ in 0..10 {
        w.wait(Wait::Frame(now(&f) + 50 * MS), &mut s);
    }
    assert!(f.0.borrow().opens.is_empty());
    assert_eq!(f.0.borrow().periods, vec![true]);
}

/// The events fire with no signal behind them: the count of signals is where the poll loaded it.
fn spurious_wake(w: &mut Wake<Fake>, f: &Fake, s: &mut Src) {
    f.0.borrow_mut().script.push_back(Woke::Event);
    let t = now(f) + 50 * MS;
    w.wait(Wait::Frame(t), s);
}

#[test]
fn more_than_eight_spurious_wake_ups_in_a_second_demote_to_polling_for_30_seconds() {
    let (mut w, f, mut s) = rig();
    into_event_mode(&mut w, &f, &mut s);
    // the gap keeps them apart by at least a millisecond; 20 ms apart is 8 in 160 ms
    for i in 1..=8 {
        f.0.borrow_mut().now += 20 * MS;
        spurious_wake(&mut w, &f, &mut s);
        assert!(w.in_event_mode(), "demoted after only {i}");
    }
    f.0.borrow_mut().now += 20 * MS;
    spurious_wake(&mut w, &f, &mut s);
    assert!(!w.in_event_mode(), "the ninth in a second did not demote");
    assert_eq!(w.diag().mode, wake_mode::POLLING_SPURIOUS);
    assert_eq!(w.diag().spurious, 9);
    assert_eq!(
        f.0.borrow().periods.last(),
        Some(&true),
        "polling holds the timer period"
    );
    // 29 s of polling: no event wait, no open
    let opens = f.0.borrow().opens.len();
    let waits_before = f.0.borrow().waits.len();
    let demoted_at = now(&f);
    while now(&f) < demoted_at + 29 * SEC {
        f.0.borrow_mut().now += 10 * MS;
        w.wait(Wait::Frame(now(&f) + 50 * MS), &mut s);
    }
    {
        let world = f.0.borrow();
        assert_eq!(world.opens.len(), opens);
        assert!(world.waits[waits_before..].iter().all(|x| x.1.is_none()));
    }
    assert_eq!(w.diag().mode, wake_mode::POLLING_SPURIOUS);
    // after 30 s: event mode again (the pair is opened afresh)
    f.0.borrow_mut().now += 2 * SEC;
    w.wait(Wait::Frame(now(&f) + 50 * MS), &mut s);
    assert!(w.in_event_mode());
    assert_eq!(f.0.borrow().opens.len(), opens + 1);
    assert_eq!(w.diag().mode, wake_mode::EVENT);
}

#[test]
fn spurious_wake_ups_spread_over_more_than_a_second_do_not_demote() {
    let (mut w, f, mut s) = rig();
    into_event_mode(&mut w, &f, &mut s);
    // 9 per 1.125 s, for a long time: never more than 8 in any second
    for _ in 0..200 {
        f.0.borrow_mut().now += 125 * MS;
        spurious_wake(&mut w, &f, &mut s);
        assert!(w.in_event_mode());
    }
    assert!(w.diag().spurious >= 200);
}

/// A frame was published since the poll and the event never fired: the timer ran out.
fn missed_wake(w: &mut Wake<Fake>, f: &Fake, s: &mut Src) {
    s.frames += 1; // a frame the poll did not see
    let t = now(f) + 50 * MS;
    s.rep.writer_frames = s.frames - 1;
    w.wait(Wait::Frame(t), s);
}

#[test]
fn three_missed_wake_ups_in_ten_seconds_demote_to_polling() {
    let (mut w, f, mut s) = rig();
    into_event_mode(&mut w, &f, &mut s);
    for i in 1..=2 {
        f.0.borrow_mut().now += 2 * SEC;
        missed_wake(&mut w, &f, &mut s);
        assert!(w.in_event_mode(), "demoted after {i}");
        assert_eq!(w.diag().missed, i);
    }
    f.0.borrow_mut().now += 2 * SEC;
    missed_wake(&mut w, &f, &mut s);
    assert!(!w.in_event_mode());
    assert_eq!(w.diag().mode, wake_mode::POLLING_MISSED);
    // and the retry after 30 s
    f.0.borrow_mut().now += 31 * SEC;
    w.wait(Wait::Frame(now(&f) + 50 * MS), &mut s);
    assert!(w.in_event_mode());
}

#[test]
fn misses_spread_over_more_than_ten_seconds_do_not_demote() {
    let (mut w, f, mut s) = rig();
    into_event_mode(&mut w, &f, &mut s);
    for _ in 0..30 {
        f.0.borrow_mut().now += 6 * SEC;
        missed_wake(&mut w, &f, &mut s);
        assert!(w.in_event_mode());
    }
    assert_eq!(w.diag().missed, 30);
}

#[test]
fn a_timer_wake_up_with_no_new_frame_or_with_the_event_set_is_not_a_miss() {
    let (mut w, f, mut s) = rig();
    into_event_mode(&mut w, &f, &mut s);
    // no new frame: the deadline ran out and that is all
    for _ in 0..10 {
        f.0.borrow_mut().now += SEC;
        w.wait(Wait::Frame(now(&f) + 50 * MS), &mut s);
    }
    assert_eq!(w.diag().missed, 0);
    // a new frame, and the event is set by the time the timer woke the pump: the signal was there
    s.frames += 1;
    f.0.borrow_mut().set = [true, true];
    for _ in 0..10 {
        f.0.borrow_mut().now += SEC;
        w.wait(Wait::Frame(now(&f) + 50 * MS), &mut s);
    }
    assert_eq!(w.diag().missed, 0);
    // a signal that arrives within the settling millisecond is late, not lost
    f.0.borrow_mut().set = [false, false];
    for _ in 0..10 {
        f.0.borrow_mut().script.push_back(Woke::Timer);
        f.0.borrow_mut().script.push_back(Woke::Event);
        s.rep.writer_frames = s.frames - 1;
        s.frames += 1;
        f.0.borrow_mut().now += SEC;
        w.wait(Wait::Frame(now(&f) + 50 * MS), &mut s);
    }
    assert_eq!(w.diag().missed, 0);
    assert!(w.in_event_mode());
}

#[test]
fn an_event_that_never_stops_ringing_costs_at_most_one_poll_a_millisecond() {
    let (mut w, f, mut s) = rig();
    into_event_mode(&mut w, &f, &mut s);
    // the writer bumps the count with every signal: these are not spurious, so no demotion, only the gap
    let start = now(&f);
    let mut polls = 0u64;
    while now(&f) < start + SEC {
        s.seq += 1;
        s.rep.advert = advert(s.seq - 1, 100);
        // the event is always set: every wait that includes it returns at once
        f.0.borrow_mut().script.push_back(Woke::Event);
        w.wait(Wait::Frame(now(&f) + 50 * MS), &mut s);
        polls += 1;
        assert!(polls < 5_000, "no progress");
    }
    assert!(
        w.in_event_mode(),
        "a legitimate-looking storm must not demote"
    );
    assert!(
        polls <= 1000 + 5,
        "{polls} polls in one second: faster than today's polling"
    );
    assert!(polls >= 900, "the gap is too long: {polls}");
    // an event wait is never started less than the gap after the last event wake-up
    let world = f.0.borrow();
    let mut last_event_end = None::<u64>;
    for (at, ev, _) in &world.waits {
        if ev.is_some() {
            if let Some(end) = last_event_end {
                assert!(
                    *at >= end + MS,
                    "an event wait started {} us after a wake-up",
                    at - end
                );
            }
            last_event_end = Some(*at + 20);
        }
    }
}

#[test]
fn every_wait_in_event_mode_is_at_most_100_ms() {
    let (mut w, f, mut s) = rig();
    into_event_mode(&mut w, &f, &mut s);
    for far in [150 * MS, 1_000 * MS, 60 * SEC, u64::MAX / 4] {
        for wait in [Wait::Frame(now(&f) + far), Wait::Until(now(&f) + far)] {
            let at = now(&f);
            w.wait(wait, &mut s);
            let (start, _, until) = *f.0.borrow().waits.last().unwrap();
            assert!(start >= at);
            assert!(until - at <= 100 * MS, "{wait:?}: {} us", until - at);
        }
    }
    // a timer-only wait never includes the event, even in event mode
    let n = f.0.borrow().waits.len();
    w.wait(Wait::Until(now(&f) + 5 * MS), &mut s);
    assert!(f.0.borrow().waits[n..].iter().all(|x| x.1.is_none()));
}

#[test]
fn a_stop_ends_the_wait_and_changes_nothing_else() {
    let (mut w, f, mut s) = rig();
    into_event_mode(&mut w, &f, &mut s);
    f.0.borrow_mut().script.push_back(Woke::Stop);
    let r = w.wait(Wait::Frame(now(&f) + 50 * MS), &mut s);
    assert_eq!(r, Woke::Stop);
    assert!(w.in_event_mode());
    assert_eq!(w.diag().spurious, 0);
    assert_eq!(w.diag().missed, 0);
    f.0.borrow_mut().timer_script.push_back(Woke::Stop);
    assert_eq!(w.wait(Wait::Until(now(&f) + 20 * MS), &mut s), Woke::Stop);
    // in the settling wait of a possible miss, too
    s.frames += 1;
    s.rep.writer_frames = s.frames - 1;
    f.0.borrow_mut().script.push_back(Woke::Timer);
    f.0.borrow_mut().script.push_back(Woke::Stop);
    assert_eq!(w.wait(Wait::Frame(now(&f) + 50 * MS), &mut s), Woke::Stop);
    assert_eq!(w.diag().missed, 0);
}

#[test]
fn the_diagnostics_are_given_to_the_source_when_they_change() {
    let (mut w, f, mut s) = rig();
    w.wait(Wait::Soon, &mut s); // opens nothing (Soon is a poll step)... but the mode is known
    into_event_mode(&mut w, &f, &mut s);
    let last = *s.diag.last().expect("the mode change was published");
    assert_eq!(last.mode, wake_mode::EVENT);
    // events are counted and published at most four times a second
    for i in 0..40 {
        s.seq += 1;
        s.rep.advert = advert(s.seq - 1, 100);
        f.0.borrow_mut().script.push_back(Woke::Event);
        f.0.borrow_mut().now += 20 * MS;
        let _ = i;
        w.wait(Wait::Frame(now(&f) + 50 * MS), &mut s);
    }
    let n = s.diag.len();
    assert!((3..=8).contains(&n), "{n} publications in 0.8 s");
    assert!(s.diag.windows(2).all(|p| p[0] != p[1]));
    w.finish(&mut s);
    let last = s.diag.last().unwrap();
    assert_eq!(last.events, 40);
    assert_eq!(last.mode, wake_mode::EVENT);
}
