//! The pump's wait on the writer's frame event (`docs/FRAME-EVENT.md`, phase 3), through the whole DLL: a real
//! [`Writer`] on its own thread, the class factory, Media Foundation samples, the real events and the real timer.
//! What a test sees of the pump besides the samples: the diagnostics the DLL writes into the header (`wake_mode`,
//! `wake_events`, `wake_missed`, `wake_spurious`) and [`probe`], which counts the pump's waits, opens, timer periods and
//! CPU.

use super::*;

use crate::frames::test_owner;
use vcam_protocol::testdir::TestFile;
use crate::wake::probe;
use vcam_protocol::signal::{
    current_user_sid_text, descriptor_text, event_name, make_event_for_test, FrameSignal,
};
use vcam_protocol::wake_mode;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Threading::{OpenEventW, SetEvent, SYNCHRONIZATION_ACCESS_RIGHTS};

const W: u32 = 640;
const H: u32 = 360;

fn no_signal() -> WriterOptions {
    WriterOptions {
        signal: false,
        ..WriterOptions::default()
    }
}

/// What a stream saw of its writer.
#[derive(Debug)]
struct Seen {
    /// Samples that showed a writer frame, and slates.
    live: u64,
    slates: u64,
    /// The stripe's step between two samples that both showed a writer frame.
    steps: Vec<u64>,
    /// The header and the probe at the end of the window (the stream still running), and the probe after the stream
    /// stopped.
    header: vcam_protocol::RawHeader,
    probe: probe::Seen,
    after: probe::Seen,
    /// Waits and event waits of the pump, and samples delivered, during the window alone.
    waits: u64,
    event_waits: u64,
    delivered: u64,
    window: f64,
}

impl Seen {
    fn waits_per_second(&self) -> f64 {
        self.waits as f64 / self.window
    }
    fn waits_per_frame(&self) -> f64 {
        self.waits as f64 / self.delivered.max(1) as f64
    }
    /// The share (percent) of steps that are `n`.
    fn steps_of(&self, n: u64) -> f64 {
        100.0 * self.steps.iter().filter(|&&d| d == n).count() as f64
            / self.steps.len().max(1) as f64
    }
}

/// One live sample's stripe slot, if the sample shows a writer frame.
fn live_slot(run: &Running) -> (Option<u32>, IMFSample) {
    let s = run.pull();
    let a = analyse_sample(&s, &run.fmt);
    (a.slot(), s)
}

/// Pull samples for `secs` and keep what they showed.
fn watch(run: &Running, secs: f64, steps: &mut Vec<u64>, live: &mut u64, slates: &mut u64) {
    let began = Instant::now();
    let n = pattern::slots(W);
    let mut prev: Option<u32> = None;
    while began.elapsed().as_secs_f64() < secs {
        match live_slot(run).0 {
            Some(slot) => {
                *live += 1;
                if let Some(p) = prev {
                    steps.push(((slot + n - p) % n) as u64);
                }
                prev = Some(slot);
            }
            None => {
                *slates += 1;
                prev = None;
            }
        }
    }
}

/// Run a feeder and a 640x360 NV12 stream of `stream_fps`: wait for the first frame, let it settle for `warm` seconds,
/// then watch for `window` seconds. `prepare` runs on the writer before it goes Live; `during` runs on the test thread
/// when the window opens (a hostile thread, a restart).
#[allow(clippy::too_many_arguments)]
fn drive(
    name: &str,
    opts: WriterOptions,
    fps: (u32, u32, u32),
    warm: f64,
    window: f64,
    prepare: impl FnOnce(&Writer),
    during: impl FnOnce(&Feeder, &std::path::Path),
) -> Seen {
    drive_at(&test_file(name), name, opts, fps, warm, window, prepare, during)
}

/// [`drive`] on a file the test made itself (`test_file` gives a folder of its own at every call).
#[allow(clippy::too_many_arguments)]
fn drive_at(
    path: &TestFile,
    name: &str,
    opts: WriterOptions,
    (advertised_fps, publish_fps, stream_fps): (u32, u32, u32),
    warm: f64,
    window: f64,
    prepare: impl FnOnce(&Writer),
    during: impl FnOnce(&Feeder, &std::path::Path),
) -> Seen {
    init();
    probe::reset(path);
    let feeder = Feeder::start_with(&path, &opts, (W, H), (advertised_fps, publish_fps), prepare);
    let run = Running::start_fps(PixelFormat::Nv12, W, H, stream_fps);
    let began = Instant::now();
    while live_slot(&run).0.is_none() {
        assert!(began.elapsed() < Duration::from_secs(5), "{name}: no frame");
    }
    let (mut steps, mut live, mut slates) = (Vec::new(), 0, 0);
    watch(&run, warm, &mut Vec::new(), &mut 0, &mut 0);
    let p0 = probe::get(&path);
    let h0 = feeder.writer.header();
    let t0 = Instant::now();
    during(&feeder, &path);
    watch(&run, window, &mut steps, &mut live, &mut slates);
    let elapsed = t0.elapsed().as_secs_f64();
    let p1 = probe::get(&path);
    let header = feeder.writer.header();
    run.stop();
    let after = probe::get(&path);
    drop(feeder);
    set_test_path(None);
    Seen {
        live,
        slates,
        steps,
        waits: p1.waits - p0.waits,
        event_waits: p1.event_waits - p0.event_waits,
        delivered: header.delivered - h0.delivered,
        header,
        probe: p1,
        after,
        window: elapsed,
    }
}

fn nothing(_: &Writer) {}
fn nothing_during(_: &Feeder, _: &std::path::Path) {}

// ---- delivery ----------------------------------------------------------------------------------------------------

#[test]
fn the_event_gives_the_frames_polling_gives_at_60_and_30_fps_with_fewer_wake_ups() {
    for fps in [60u32, 30] {
        let ev = drive(
            &format!("ev-{fps}"),
            WriterOptions::default(),
            (fps, fps, fps),
            0.5,
            3.0,
            nothing,
            nothing_during,
        );
        let po = drive(
            &format!("po-{fps}"),
            no_signal(),
            (fps, fps, fps),
            0.5,
            3.0,
            nothing,
            nothing_during,
        );
        // the modes, as the DLL reports them in the header
        assert_eq!(
            ev.header.wake_mode,
            wake_mode::EVENT,
            "{fps}: {:?}",
            ev.header.wake_mode
        );
        assert_eq!(po.header.wake_mode, wake_mode::POLLING, "{fps} polling");
        assert_eq!(
            (ev.header.wake_missed, ev.header.wake_spurious),
            (0, 0),
            "{fps}: no miss, no spurious wake-up"
        );
        // the 1 ms timer resolution is held only while polling
        assert_eq!(
            ev.probe.periods_held, 0,
            "{fps}: event mode holds the timer period"
        );
        assert_eq!(
            po.probe.periods_held, 1,
            "{fps}: polling mode lost the timer period"
        );
        // and given back when the stream stops, in both
        assert_eq!(ev.after.periods_held, 0);
        assert_eq!(po.after.periods_held, 0);
        // the same frames: all of them, in order, none twice
        for (what, r) in [("event", &ev), ("polling", &po)] {
            let want = fps as f64 * 3.0;
            assert!(
                r.live as f64 >= want * 0.93 && r.live as f64 <= want * 1.07,
                "{fps} {what}: {} live samples, wanted about {want}",
                r.live
            );
            assert!(
                r.steps_of(1) >= 97.0,
                "{fps} {what}: steps {:?}",
                r.steps_of(1)
            );
            assert_eq!(
                r.steps.iter().filter(|&&d| d == 0).count(),
                0,
                "{fps} {what}: a frame twice"
            );
            assert!(
                r.header.repeated <= 3,
                "{fps} {what}: {} repeats",
                r.header.repeated
            );
            assert_eq!(r.slates, 0, "{fps} {what}: slates");
        }
        // wake-ups: one per frame by the event, at most 2.5 a frame in all; polling needs several times that
        let ratio = ev.header.wake_events as f64 / ev.header.delivered.max(1) as f64;
        assert!(
            (0.9..=1.1).contains(&ratio),
            "{fps}: {} event wake-ups for {} frames",
            ev.header.wake_events,
            ev.header.delivered
        );
        assert!(
            ev.waits_per_frame() <= 2.5,
            "{fps}: {:.2} waits a frame in event mode",
            ev.waits_per_frame()
        );
        assert!(
            po.waits_per_frame() >= 3.0 && po.waits_per_frame() >= 1.5 * ev.waits_per_frame(),
            "{fps}: polling {:.2} waits a frame, event {:.2}",
            po.waits_per_frame(),
            ev.waits_per_frame()
        );
        assert!(ev.event_waits > 0 && po.event_waits == 0);
        // latency is not worse than polling's (microseconds; a generous margin for a busy machine)
        assert!(
            ev.header.latency_p95_us <= po.header.latency_p95_us + 4_000,
            "{fps}: p95 {} us by event, {} us polling",
            ev.header.latency_p95_us,
            po.header.latency_p95_us
        );
    }
}

#[test]
fn a_60_fps_feeder_into_a_30_fps_stream_is_still_halved_in_event_mode() {
    let ev = drive(
        "ev-60-30",
        WriterOptions::default(),
        (60, 60, 30),
        0.5,
        3.0,
        nothing,
        nothing_during,
    );
    assert_eq!(ev.header.wake_mode, wake_mode::EVENT);
    assert_eq!(ev.header.negotiated.fps(), 30);
    let want = 90.0;
    assert!(
        (want * 0.93..=want * 1.07).contains(&(ev.live as f64)),
        "{} live samples",
        ev.live
    );
    assert!(
        ev.steps_of(2) >= 90.0,
        "halved: steps of two are {:.1}% ({:?})",
        ev.steps_of(2),
        &ev.steps[..ev.steps.len().min(20)]
    );
    assert_eq!(ev.steps.iter().filter(|&&d| d == 0).count(), 0);
    assert!(ev.waits_per_frame() <= 2.5, "{:.2}", ev.waits_per_frame());
    // the frames that decimation drops must not wake the pump: a wake-up for each of the writer's frames would be
    // more than the 90 emitted ones
    assert!(
        ev.header.wake_events <= ev.header.delivered * 12 / 10 + 5,
        "{} event wake-ups for {} samples",
        ev.header.wake_events,
        ev.header.delivered
    );
}

#[test]
fn a_slow_writer_with_a_faster_consumer_wakes_the_pump_about_as_often_as_both_run() {
    // a 14 fps camera into a 30 fps stream: polling wakes about 1000 times a second between the frames
    let ev = drive(
        "ev-14-30",
        WriterOptions::default(),
        (30, 14, 30),
        0.5,
        4.0,
        nothing,
        nothing_during,
    );
    let po = drive(
        "po-14-30",
        no_signal(),
        (30, 14, 30),
        0.5,
        4.0,
        nothing,
        nothing_during,
    );
    assert_eq!(ev.header.wake_mode, wake_mode::EVENT);
    assert_eq!((ev.header.wake_missed, ev.header.wake_spurious), (0, 0));
    for (what, r) in [("event", &ev), ("polling", &po)] {
        // the stream still runs at its own 30 fps (live frames and repeats)
        assert!(
            (110.0..=130.0).contains(&(r.delivered as f64)),
            "{what}: {} samples in 4 s",
            r.delivered
        );
    }
    assert!(
        po.waits_per_second() >= 450.0,
        "polling made only {:.0} waits a second",
        po.waits_per_second()
    );
    assert!(
        ev.waits_per_second() <= 250.0,
        "event mode made {:.0} waits a second (consumer 30 + camera 14)",
        ev.waits_per_second()
    );
    assert!(
        ev.waits_per_second() <= 0.4 * po.waits_per_second(),
        "{:.0} waits a second by event, {:.0} polling",
        ev.waits_per_second(),
        po.waits_per_second()
    );
}

// ---- guards ------------------------------------------------------------------------------------------------------

/// A thread that does what any process of the user may: open the writer's events by their names and set them as
/// fast as it can.
struct Storm {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<u64>>,
}

impl Storm {
    fn start(nonces: [u128; 2]) -> Storm {
        let stop = Arc::new(AtomicBool::new(false));
        let s2 = stop.clone();
        let thread = std::thread::spawn(move || {
            let open = |parity: usize| -> HANDLE {
                let name: Vec<u16> = event_name(nonces[parity], parity)
                    .encode_utf16()
                    .chain(std::iter::once(0))
                    .collect();
                // EVENT_MODIFY_STATE | SYNCHRONIZE: what the user's own right on the event gives
                unsafe {
                    OpenEventW(
                        SYNCHRONIZATION_ACCESS_RIGHTS(0x0010_0002),
                        false,
                        PCWSTR(name.as_ptr()),
                    )
                }
                .expect("the user may open the writer's events")
            };
            let (a, b) = (open(0), open(1));
            let mut sets = 0u64;
            while !s2.load(Ordering::Relaxed) {
                unsafe {
                    let _ = SetEvent(a);
                    let _ = SetEvent(b);
                }
                sets += 1;
                std::thread::sleep(Duration::from_micros(50));
            }
            unsafe {
                let _ = CloseHandle(a);
                let _ = CloseHandle(b);
            }
            sets
        });
        Storm {
            stop,
            thread: Some(thread),
        }
    }

    fn stop(&mut self) -> u64 {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.take().map(|t| t.join().unwrap()).unwrap_or(0)
    }
}

#[test]
fn a_hostile_signaller_demotes_the_pump_to_polling_and_costs_no_more_than_polling() {
    let storm_slot: Arc<std::sync::Mutex<Option<Storm>>> = Arc::new(std::sync::Mutex::new(None));
    let slot = storm_slot.clone();
    let hostile = drive(
        "storm",
        WriterOptions::default(),
        (30, 30, 30),
        0.5,
        4.0,
        nothing,
        move |f, _| {
            let nonces = f.writer.frame_event_nonces().expect("events");
            *slot.lock().unwrap() = Some(Storm::start(nonces));
        },
    );
    let sets = storm_slot
        .lock()
        .unwrap()
        .take()
        .map(|mut s| s.stop())
        .unwrap_or(0);
    assert!(sets > 1000, "the storm barely ran: {sets} sets");
    // the frames are right
    assert!(hostile.live >= 100, "{} live samples in 4 s", hostile.live);
    assert!(hostile.steps_of(1) >= 95.0, "{:?}", hostile.steps_of(1));
    assert_eq!(hostile.steps.iter().filter(|&&d| d == 0).count(), 0);
    // demoted for spurious wake-ups, which the diagnostics say
    assert_eq!(
        hostile.header.wake_mode,
        wake_mode::POLLING_SPURIOUS,
        "{:?}",
        hostile.header
    );
    assert!(hostile.header.wake_spurious > 8);
    assert_eq!(
        hostile.probe.periods_held, 1,
        "the pump polls again: it holds the timer period"
    );
    // the same stream with no storm and no events at all: what polling costs
    let base = drive(
        "storm-base",
        no_signal(),
        (30, 30, 30),
        0.5,
        4.0,
        nothing,
        nothing_during,
    );
    let (a, b) = (
        hostile.after.cpu_cycles as f64,
        base.after.cpu_cycles as f64,
    );
    assert!(b > 0.0 && a > 0.0, "cycles {a} {b}");
    assert!(
        a <= 1.2 * b,
        "the pump thread used {:.1} M cycles under the storm and {:.1} M polling ({:.2}x)",
        a / 1e6,
        b / 1e6,
        a / b
    );
}

/// A hostile *writer*: it keeps its own count of signals in the header, sets and resets the writer's events as a correct
/// signaller does, and does it every 50 microseconds. None of its wake-ups is spurious, so only the gap of one
/// millisecond between two event-driven polls holds it to today's polling rate.
struct CountingStorm {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<u64>>,
}

impl CountingStorm {
    fn start(writer: Arc<Writer>) -> CountingStorm {
        let stop = Arc::new(AtomicBool::new(false));
        let s2 = stop.clone();
        let nonces = writer.frame_event_nonces().expect("events");
        let thread = std::thread::spawn(move || {
            let open = |parity: usize| -> HANDLE {
                let name: Vec<u16> = event_name(nonces[parity], parity)
                    .encode_utf16()
                    .chain(std::iter::once(0))
                    .collect();
                unsafe {
                    OpenEventW(
                        SYNCHRONIZATION_ACCESS_RIGHTS(0x0010_0002),
                        false,
                        PCWSTR(name.as_ptr()),
                    )
                }
                .expect("the user may open the writer's events")
            };
            let ev = [open(0), open(1)];
            let mut n = 1_000_000u64;
            while !s2.load(Ordering::Relaxed) {
                n += 1;
                unsafe {
                    let _ =
                        windows::Win32::System::Threading::ResetEvent(ev[((n + 1) & 1) as usize]);
                }
                writer.shared().poke64(off::SIGNAL_SEQ, n);
                unsafe {
                    let _ = SetEvent(ev[(n & 1) as usize]);
                }
                // 50 microseconds: a sleep of that length lasts a millisecond or more without a timer period
                let until = Instant::now() + Duration::from_micros(50);
                while Instant::now() < until {
                    std::hint::spin_loop();
                }
            }
            for h in ev {
                unsafe {
                    let _ = CloseHandle(h);
                }
            }
            n - 1_000_000
        });
        CountingStorm {
            stop,
            thread: Some(thread),
        }
    }

    fn stop(&mut self) -> u64 {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.take().map(|t| t.join().unwrap()).unwrap_or(0)
    }
}

#[test]
fn a_signaller_that_counts_like_a_writer_is_held_to_one_event_driven_poll_a_millisecond() {
    let storm: Arc<std::sync::Mutex<Option<CountingStorm>>> = Arc::new(std::sync::Mutex::new(None));
    let slot = storm.clone();
    let r = drive(
        "counting-storm",
        WriterOptions::default(),
        (30, 30, 30),
        0.5,
        3.0,
        nothing,
        move |f, _| {
            *slot.lock().unwrap() = Some(CountingStorm::start(f.writer.clone()));
        },
    );
    let n = storm
        .lock()
        .unwrap()
        .take()
        .map(|mut s| s.stop())
        .unwrap_or(0);
    assert!(n > 30_000, "the storm barely ran: {n} signals in 3.5 s");
    // 20 000 wake-ups a second were on offer; the pump polled at most once a millisecond (a little over: the timer
    // wake-ups of its own deadlines come on top)
    assert!(
        r.waits_per_second() <= 1500.0,
        "{:.0} waits a second under a signal storm",
        r.waits_per_second()
    );
    // and the stream is still right (the frames come from the real writer; the storm only rings)
    assert!(r.live >= 60, "{} live samples in 3 s", r.live);
    assert!(r.steps.iter().filter(|&&d| d == 0).count() == 0);
}

#[test]
fn a_writer_that_advertises_events_and_never_signals_is_demoted_after_three_misses() {
    // the feeder is an app that made its events and advertises them but never sets one
    let signal = Arc::new(std::sync::Mutex::new(None::<FrameSignal>));
    let keep = signal.clone();
    let r = drive(
        "never-signals",
        no_signal(),
        (30, 30, 30),
        0.0,
        4.0,
        move |w| {
            let s = FrameSignal::create().unwrap();
            s.advertise(w.shared());
            *keep.lock().unwrap() = Some(s);
        },
        nothing_during,
    );
    drop(signal);
    assert_eq!(
        r.header.wake_mode,
        wake_mode::POLLING_MISSED,
        "{:?}",
        r.header
    );
    assert!(
        (3..=4).contains(&r.header.wake_missed),
        "{} misses",
        r.header.wake_missed
    );
    assert_eq!(r.header.wake_events, 0);
    // a missed frame is taken at the next deadline, late and not lost: a few frames at most, and no repeat for them
    assert!(r.live >= 105, "{} live samples in 4 s (30 fps)", r.live);
    assert!(
        r.header.repeated <= r.header.wake_missed + 2,
        "{} repeats for {} misses",
        r.header.repeated,
        r.header.wake_missed
    );
    assert_eq!(
        r.probe.periods_held, 1,
        "demoted: polling holds the timer period"
    );
}

#[test]
fn a_writer_that_starts_over_with_new_events_is_followed_within_a_second() {
    // a new `Writer` on the same file while the stream runs, as an app that starts over: new nonces
    init();
    let path = test_file("restart2");
    probe::reset(&path);
    let mut feeder = Feeder::start(&path, W, H, 30);
    let run = Running::start_fps(PixelFormat::Nv12, W, H, 30);
    let began = Instant::now();
    while live_slot(&run).0.is_none() {
        assert!(began.elapsed() < Duration::from_secs(5));
    }
    let (mut st, mut live, mut slates) = (Vec::new(), 0, 0);
    watch(&run, 1.0, &mut st, &mut live, &mut slates);
    assert_eq!(feeder.writer.header().wake_mode, wake_mode::EVENT);
    let old = feeder.writer.frame_event_nonces().unwrap();
    assert_eq!(probe::get(&path).opens, 1);
    feeder.stop_publishing();
    drop(feeder);
    // the new app: a new writer (init_writer wipes the header, new events) and a new feeder thread
    let feeder = Feeder::start(&path, W, H, 30);
    let new = feeder.writer.frame_event_nonces().unwrap();
    assert_ne!(old, new);
    let t0 = Instant::now();
    let mut back = None;
    while t0.elapsed() < Duration::from_secs(3) {
        let _ = run.pull();
        let h = feeder.writer.header();
        // the header was wiped by the new writer: `wake_mode` is the new pair's (the opens say the pump went through
        // the pacing and the owner check again)
        if h.wake_mode == wake_mode::EVENT && probe::get(&path).opens >= 2 {
            back = Some(t0.elapsed());
            break;
        }
    }
    let back = back.expect("the pump never went back to the event");
    assert!(
        back < Duration::from_millis(1500),
        "back in event mode after {back:?}"
    );
    let p = probe::get(&path);
    assert!(p.opens >= 2, "{} opens", p.opens);
    assert_eq!(p.events_live, 1, "the old pair is still open");
    assert_eq!(p.periods_held, 0);
    watch(&run, 1.0, &mut st, &mut live, &mut slates);
    assert!(live > 50 && slates < 20, "live {live} slates {slates}");
    run.stop();
    assert_eq!(
        probe::get(&path).events_live,
        0,
        "the pair outlived the stream"
    );
    assert_eq!(probe::get(&path).periods_held, 0);
    drop(feeder);
    set_test_path(None);
}

/// Frames reach the stream whatever the events are, and the pump polls (`wake_mode` 4).
fn polling_because_of(
    name: &str,
    opens: std::ops::RangeInclusive<u64>,
    prepare: impl FnOnce(&Writer, &std::path::Path) + 'static,
) -> Seen {
    let path = test_file(name);
    let r = drive_at(
        &path,
        name,
        no_signal(),
        (30, 30, 30),
        0.3,
        2.5,
        {
            let path = path.to_path_buf();
            move |w| prepare(w, &path)
        },
        nothing_during,
    );
    assert!(r.live >= 60, "{name}: {} live samples", r.live);
    assert!(r.steps_of(1) >= 95.0, "{name}: {:.1}", r.steps_of(1));
    assert_eq!(
        r.header.wake_mode,
        wake_mode::POLLING_OPEN_FAILED,
        "{name}: {:?}",
        r.header
    );
    assert_eq!(
        r.event_waits, 0,
        "{name}: waited on an event it did not trust"
    );
    assert_eq!(r.header.wake_events, 0);
    assert_eq!(
        r.probe.periods_held, 1,
        "{name}: polling holds the timer period"
    );
    // the pacing: one open in the whole run, not one a second (none when the file's DACL names nobody to check)
    assert!(
        opens.contains(&r.probe.opens),
        "{name}: {} open attempts in 2.8 s",
        r.probe.opens
    );
    r
}

fn user() -> String {
    current_user_sid_text().unwrap()
}

#[test]
fn events_that_are_missing_or_the_wrong_kind_or_owned_by_somebody_else_are_not_used() {
    // missing: the advert names events that were never made
    polling_because_of("bad-missing", 1..=2, |w, _| {
        w.shared().advertise_frame_event([
            0x1111_2222_3333_4444_5555_6666_7777_8888,
            0x9999_aaaa_bbbb_cccc_dddd_eeee_ffff_0001,
        ]);
    });
    // another type: a mutex under the name of the first, an event under the second
    let keep = Arc::new(std::sync::Mutex::new(Vec::<OwnedHandle>::new()));
    let k = keep.clone();
    polling_because_of("bad-type", 1..=2, move |w, _| {
        use std::os::windows::io::FromRawHandle;
        let n = [
            0x2222_0000_0000_0000_0000_0000_0000_0001u128,
            0x2222_0000_0000_0000_0000_0000_0000_0002,
        ];
        let name: Vec<u16> = event_name(n[0], 0)
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let m = unsafe {
            windows::Win32::System::Threading::CreateMutexW(None, false, PCWSTR(name.as_ptr()))
        }
        .unwrap();
        k.lock()
            .unwrap()
            .push(unsafe { OwnedHandle::from_raw_handle(m.0) });
        k.lock()
            .unwrap()
            .push(make_event_for_test(n[1], 1, None).unwrap());
        w.shared().advertise_frame_event(n);
    });
    drop(keep);
    // a low-integrity label: owned by the user and as the writer makes them in every other way (a low-integrity
    // process of the user took the names over), but labelled low
    let keep = Arc::new(std::sync::Mutex::new(Vec::<OwnedHandle>::new()));
    let k = keep.clone();
    polling_because_of("bad-label", 1..=2, move |w, _| {
        let n = [
            0x3333_0000_0000_0000_0000_0000_0000_0001u128,
            0x3333_0000_0000_0000_0000_0000_0000_0002,
        ];
        let sddl = descriptor_text(&user()).replace("S:(ML;;NW;;;ME)", "S:(ML;;NW;;;LW)");
        assert!(sddl.ends_with("(ML;;NW;;;LW)"));
        for (p, nonce) in n.into_iter().enumerate() {
            k.lock()
                .unwrap()
                .push(make_event_for_test(nonce, p, Some(&sddl)).unwrap());
        }
        w.shared().advertise_frame_event(n);
    });
    drop(keep);
    // no label at all
    let keep = Arc::new(std::sync::Mutex::new(Vec::<OwnedHandle>::new()));
    let k = keep.clone();
    polling_because_of("bad-nolabel", 1..=2, move |w, _| {
        let n = [
            0x4444_0000_0000_0000_0000_0000_0000_0001u128,
            0x4444_0000_0000_0000_0000_0000_0000_0002,
        ];
        let full = descriptor_text(&user());
        let sddl = full.split_once("S:").unwrap().0.to_string();
        for (p, nonce) in n.into_iter().enumerate() {
            k.lock()
                .unwrap()
                .push(make_event_for_test(nonce, p, Some(&sddl)).unwrap());
        }
        w.shared().advertise_frame_event(n);
    });
    drop(keep);
    // the writer's own events, but the DACL of the frame file names another user: nobody the events could belong to
    let signal = Arc::new(std::sync::Mutex::new(None::<FrameSignal>));
    let keep = signal.clone();
    polling_because_of("bad-owner", 1..=2, move |w, path| {
        let s = FrameSignal::create().unwrap();
        s.advertise(w.shared());
        *keep.lock().unwrap() = Some(s);
        test_owner::set(path, Some("S-1-5-21-1-2-3-4".into()));
    });
    drop(signal);
    // and the DACL names nobody at all
    let signal = Arc::new(std::sync::Mutex::new(None::<FrameSignal>));
    let keep = signal.clone();
    polling_because_of("bad-nobody", 0..=0, move |w, path| {
        let s = FrameSignal::create().unwrap();
        s.advertise(w.shared());
        *keep.lock().unwrap() = Some(s);
        test_owner::set(path, None);
    });
    drop(signal);
}

#[test]
fn the_real_dacl_of_the_frame_file_names_the_user_who_made_the_events() {
    // no override: the DLL reads the DACL of the temp file the writer made (the test user's own folder) and finds
    // the one user in it, which is the owner of the writer's events
    let r = drive(
        "real-owner",
        WriterOptions::default(),
        (30, 30, 30),
        0.3,
        1.0,
        nothing,
        nothing_during,
    );
    assert_eq!(r.header.wake_mode, wake_mode::EVENT, "{:?}", r.header);
}

#[test]
fn nothing_is_opened_for_a_writer_that_is_not_live_or_whose_heartbeat_is_stale_or_that_has_no_advert(
) {
    init();
    // (name, how the writer is made, whether it should be keeping the heartbeat fresh)
    for (name, state, wait_stale) in [
        ("gate-paused", WriterState::Paused, false),
        ("gate-starting", WriterState::Starting, false),
        ("gate-closed", WriterState::Closed, false),
        ("gate-stale", WriterState::Live, true),
    ] {
        let path = test_file(name);
        probe::reset(&path);
        let w = Writer::create(&path).unwrap();
        assert_eq!(
            w.header().features,
            vcam_protocol::FEATURE_FRAME_EVENT,
            "{name}: the writer advertises"
        );
        w.set_advertised(&vcam_protocol::advertised_for_output(W, H, 30));
        w.set_state(state);
        if wait_stale {
            // live, with an advert, and the heartbeat is older than a second by the time a stream starts
            std::thread::sleep(Duration::from_millis(1200));
        }
        let run = Running::start_fps(PixelFormat::Nv12, W, H, 30);
        let began = Instant::now();
        let mut n = 0;
        while began.elapsed() < Duration::from_millis(1500) {
            if !wait_stale {
                w.heartbeat();
            }
            let s = run.pull();
            // a slate every time: nothing live to show
            assert!(analyse_sample(&s, &run.fmt).slot().is_none(), "{name}");
            n += 1;
        }
        assert!(n > 20, "{name}: {n} samples");
        let p = probe::get(&path);
        assert_eq!(p.opens, 0, "{name}: the events were opened");
        assert_eq!(p.event_waits, 0, "{name}");
        assert_eq!(w.header().wake_mode, wake_mode::POLLING, "{name}");
        run.stop();
        drop(w);
    }
    set_test_path(None);
}

#[test]
fn an_old_writer_or_a_one_nonce_advert_gives_polling_and_the_same_frames() {
    // an app from before the frame event: no advert at all
    let old = drive(
        "old-writer",
        no_signal(),
        (30, 30, 30),
        0.3,
        2.0,
        nothing,
        nothing_during,
    );
    // the app of the first commit of the frame event (feed98c): one nonce, the second field zero
    let one = drive(
        "one-nonce",
        no_signal(),
        (30, 30, 30),
        0.3,
        2.0,
        |w| {
            let sh = w.shared();
            sh.poke64(off::EVENT_NONCE_LO, 0x0123_4567_89ab_cdef);
            sh.poke64(off::EVENT_NONCE_HI, 0xfedc_ba98_7654_3210);
            sh.poke32(off::FEATURES, vcam_protocol::FEATURE_FRAME_EVENT);
        },
        nothing_during,
    );
    for (what, r) in [("old writer", &old), ("one nonce", &one)] {
        assert_eq!(r.header.wake_mode, wake_mode::POLLING, "{what}");
        assert_eq!(r.probe.opens, 0, "{what}: an open was tried");
        assert_eq!(r.event_waits, 0, "{what}");
        assert_eq!(r.probe.periods_held, 1, "{what}");
        assert!(
            (55..=65).contains(&r.live),
            "{what}: {} live samples in 2 s",
            r.live
        );
        assert!(r.steps_of(1) >= 97.0, "{what}");
        assert_eq!(r.header.repeated.min(3), r.header.repeated, "{what}");
    }
}

// ---- stopping and leaks ------------------------------------------------------------------------------------------

#[test]
fn a_stop_never_waits_for_the_event_or_the_deadline_of_a_wait() {
    init();
    let path = test_file("stop-latency");
    probe::reset(&path);
    // a live writer that publishes nothing: the pump asks for a frame, finds none and waits (up to the point where
    // the "starting" slate is due) on the event, in event mode
    let w = Writer::create(&path).unwrap();
    w.set_advertised(&vcam_protocol::advertised_for_output(W, H, 5));
    w.set_state(WriterState::Live);
    let mut worst = Duration::ZERO;
    let mut in_event_wait = 0;
    for _ in 0..6 {
        let run = Running::start_fps(PixelFormat::Nv12, W, H, 5);
        // a stream of 5 fps: the "starting" slate comes after 300 ms. A token is outstanding, so the pump waits.
        unsafe { run.stream.RequestSample(None).unwrap() };
        // the first poll opens the events (the writer is Live, with an advert); it then waits on the event and the timer
        let began = Instant::now();
        while probe::get(&path).event_waits == in_event_wait {
            assert!(
                began.elapsed() < Duration::from_secs(2),
                "the pump never waited on the event"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        in_event_wait = probe::get(&path).event_waits;
        std::thread::sleep(Duration::from_millis(20));
        let t = Instant::now();
        run.stop();
        worst = worst.max(t.elapsed());
        w.heartbeat();
    }
    // the wait it was in would have lasted about 100 ms (the cap) or 300 ms (the deadline)
    assert!(worst < Duration::from_millis(50), "a stop took {worst:?}");
    drop(w);
    set_test_path(None);
}
