//! `signal-storm` (development only): act as a hostile signaller of the frame events.
//!
//! The frame events of `docs/FRAME-EVENT.md` are created by the app with a DACL that lets the installing user set them.
//! This command is that user's other process: it reads the advert from the frame file, opens the live events with
//! `EVENT_MODIFY_STATE | SYNCHRONIZE` (the names are built with `vcam_protocol::signal::event_name`, as the DLL builds
//! them) and sets both of them in a tight loop for N seconds, never resetting them. It is the check of phase 4: the
//! camera service must stay at or under its polling-mode CPU, show `wake_mode` 2 (`vcam-tool stats`) and keep delivering
//! correct frames while this runs.
//!
//! It refuses unless the writer has a live advert (events advertised, state Live, a fresh heartbeat): against anything
//! else it would do nothing. It looks again every 250 ms and stops, with a message, when the writer is no longer live or
//! has started over (other nonces): events that are not the camera's writer's any more are not its business. It touches
//! nothing but the two events and never writes the frame file.

use std::io;
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use vcam_protocol::signal::event_name;
use vcam_protocol::{
    frames_path, is_stale, qpc_frequency, qpc_now, validate_header, Access, MappedFile, WriterState,
};
use windows::core::PCWSTR;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Threading::{
    OpenEventW, SetEvent, EVENT_MODIFY_STATE, SYNCHRONIZATION_SYNCHRONIZE,
};

pub const USAGE: &str = "usage: vcam-tool signal-storm [--seconds N] [--file PATH]
  development only: sets the writer's live frame events in a loop for N seconds (default 10, 0.1 to 600), and stops
  early (saying why) when the writer is no longer live or has started over; --file reads the advert from another frame
  file";

#[derive(Debug, Clone, PartialEq)]
pub struct Args {
    pub seconds: f64,
    pub file: Option<PathBuf>,
}

pub fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut a = Args {
        seconds: 10.0,
        file: None,
    };
    let mut it = args.iter();
    while let Some(k) = it.next() {
        let mut val = || it.next().ok_or_else(|| format!("{k} needs a value"));
        match k.as_str() {
            "--seconds" => a.seconds = val()?.parse().map_err(|_| "bad --seconds")?,
            "--file" => a.file = Some(PathBuf::from(val()?)),
            other => return Err(format!("unknown argument '{other}'")),
        }
    }
    if !(0.1..=600.0).contains(&a.seconds) {
        return Err("--seconds must be 0.1..=600".into());
    }
    Ok(a)
}

/// The nonces of the writer's live advert, or why there is none: the header is valid, the writer advertises its events,
/// is Live, and its heartbeat is fresh.
pub fn live_advert(map: &MappedFile, now: u64) -> Result<[u128; 2], String> {
    let h = map.shared().header();
    let v = validate_header(&h, map.len() as u64, Some(qpc_frequency()), Some(now))
        .map_err(|e| format!("the frame file header is invalid ({e:?})"))?;
    let Some(wake) = v.wake else {
        return Err(
            "the writer advertises no frame events (an app from before them, or `feed --no-signal`, or none could be made)"
                .into(),
        );
    };
    if v.state != WriterState::Live {
        return Err(format!("the writer is {}, not live", v.state.name()));
    }
    if is_stale(&v, now) {
        return Err("the writer's heartbeat is stale".into());
    }
    Ok(wake.nonces)
}

/// Open both events with the right to set them (and to wait). Nothing is left open on failure.
pub fn open_events(nonces: [u128; 2]) -> io::Result<[OwnedHandle; 2]> {
    let open = |parity: usize| -> io::Result<OwnedHandle> {
        let name: Vec<u16> = event_name(nonces[parity], parity)
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let h = unsafe {
            OpenEventW(
                EVENT_MODIFY_STATE | SYNCHRONIZATION_SYNCHRONIZE,
                false,
                PCWSTR(name.as_ptr()),
            )
        }
        .map_err(|e| io::Error::from_raw_os_error(e.code().0 & 0xFFFF))?;
        // SAFETY: a new handle that nothing else owns.
        Ok(unsafe { OwnedHandle::from_raw_handle(h.0) })
    };
    let e0 = open(0)?;
    let e1 = open(1)?;
    Ok([e0, e1])
}

/// How often the storm looks again at the writer (see [`storm`]).
pub const RECHECK_EVERY: Duration = Duration::from_millis(250);

/// Set both events as fast as possible for `seconds`. Every `every`, `still_ok` is asked whether the target is still
/// the one the storm was started against; an `Err` ends the storm at once with that reason. Returns the number of
/// `SetEvent` calls and, when it ended early, why.
pub fn storm(
    events: &[OwnedHandle; 2],
    seconds: f64,
    every: Duration,
    still_ok: &mut dyn FnMut() -> Result<(), String>,
) -> (u64, Option<String>) {
    use std::os::windows::io::AsRawHandle;
    let (h0, h1) = (
        HANDLE(events[0].as_raw_handle()),
        HANDLE(events[1].as_raw_handle()),
    );
    let began = Instant::now();
    let end = began + Duration::from_secs_f64(seconds);
    let mut next_look = began + every;
    let mut n = 0u64;
    loop {
        let _ = unsafe { SetEvent(h0) };
        let _ = unsafe { SetEvent(h1) };
        n += 2;
        // the clock is looked at every 1024 calls
        if n & 1023 == 0 {
            let now = Instant::now();
            if now >= end {
                return (n, None);
            }
            if now >= next_look {
                next_look = now + every;
                if let Err(why) = still_ok() {
                    return (n, Some(why));
                }
            }
        }
    }
}

/// Whether the writer behind `map` still has the advert (`nonces`) the storm began with: still live, still the same
/// events. The storm must not go on against events of a writer that started over, or against one that is gone: it
/// would be setting nothing the camera reads, or somebody else's objects.
pub fn same_advert(map: &MappedFile, nonces: [u128; 2], now: u64) -> Result<(), String> {
    match live_advert(map, now) {
        Ok(n) if n == nonces => Ok(()),
        Ok(_) => Err("the writer started over: its frame events have other names".into()),
        Err(why) => Err(why),
    }
}

pub fn run(args: &[String]) -> i32 {
    let a = match parse_args(args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}\n{USAGE}");
            return 2;
        }
    };
    let path = a.file.clone().unwrap_or_else(frames_path);
    let map = match MappedFile::open(&path, Access::ReadOnly) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("{} not readable: {e}", path.display());
            return 1;
        }
    };
    let nonces = match live_advert(&map, qpc_now()) {
        Ok(n) => n,
        Err(why) => {
            eprintln!("refusing: {why}");
            return 1;
        }
    };
    let events = match open_events(nonces) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("cannot open the frame events to set them: {e}");
            return 1;
        }
    };
    println!(
        "signal-storm: setting both frame events of {} for {} s",
        path.display(),
        a.seconds
    );
    let began = Instant::now();
    let (n, stopped) = storm(&events, a.seconds, RECHECK_EVERY, &mut || {
        same_advert(&map, nonces, qpc_now())
    });
    let took = began.elapsed().as_secs_f64();
    match stopped {
        None => println!("done: {n} SetEvent calls in {took:.2} s"),
        Some(why) => println!("stopped early after {took:.2} s ({n} SetEvent calls): {why}"),
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::ScratchDir;
    use vcam_protocol::signal::open_frame_events;
    use vcam_protocol::{Writer, WriterOptions};
    use windows::Win32::Foundation::WAIT_OBJECT_0;
    use windows::Win32::System::Threading::WaitForSingleObject;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    /// The storm of the tests: never asks anything, looks as the real one does.
    fn plain_storm(events: &[OwnedHandle; 2], seconds: f64) -> u64 {
        let (n, stopped) = storm(events, seconds, RECHECK_EVERY, &mut || Ok(()));
        assert_eq!(stopped, None);
        n
    }

    fn map_of(w: &Writer, path: &std::path::Path) -> MappedFile {
        let _ = w;
        MappedFile::open(path, Access::ReadOnly).unwrap()
    }

    fn is_set(h: &OwnedHandle) -> bool {
        use std::os::windows::io::AsRawHandle;
        unsafe { WaitForSingleObject(HANDLE(h.as_raw_handle()), 0) == WAIT_OBJECT_0 }
    }

    #[test]
    fn parses_the_options_and_bounds_the_time() {
        let a = parse_args(&s(&["--seconds", "2.5", "--file", r"C:\x\f.bin"])).unwrap();
        assert_eq!(a.seconds, 2.5);
        assert_eq!(a.file, Some(PathBuf::from(r"C:\x\f.bin")));
        assert_eq!(parse_args(&[]).unwrap().seconds, 10.0);
        for bad in [
            &["--seconds", "0"][..],
            &["--seconds", "601"],
            &["--seconds", "x"],
            &["--seconds"],
            &["--file"],
            &["--bogus"],
        ] {
            assert!(parse_args(&s(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn it_refuses_unless_the_writer_has_a_live_advert() {
        // events and Live: the advert, and the nonces are the writer's
        let dir = ScratchDir::new("storm-refuse");
        let path = dir.join("live.bin");
        let w = Writer::create(&path).unwrap();
        w.set_state(WriterState::Live);
        let m = map_of(&w, &path);
        assert_eq!(
            live_advert(&m, qpc_now()),
            Ok(w.frame_event_nonces().unwrap())
        );

        // not live
        w.set_state(WriterState::Paused);
        let e = live_advert(&m, qpc_now()).unwrap_err();
        assert!(e.contains("paused") && e.contains("not live"), "{e}");
        w.set_state(WriterState::Live);

        // a heartbeat that is old
        w.shared()
            .set_writer_heartbeat(qpc_now().saturating_sub(qpc_frequency() * 5));
        let e = live_advert(&m, qpc_now()).unwrap_err();
        assert!(e.contains("stale"), "{e}");

        // an old app: no events
        let old_path = dir.join("old.bin");
        let old = Writer::create_with(
            &old_path,
            &WriterOptions {
                signal: false,
                ..WriterOptions::default()
            },
        )
        .unwrap();
        old.set_state(WriterState::Live);
        let e = live_advert(&map_of(&old, &old_path), qpc_now()).unwrap_err();
        assert!(e.contains("no frame events"), "{e}");

        // a closed writer withdrew its advert
        w.set_state(WriterState::Live);
        w.close();
        let e = live_advert(&m, qpc_now()).unwrap_err();
        assert!(e.contains("no frame events"), "{e}");

        // a file that is not a frame file
        let junk = dir.join("junk.bin");
        std::fs::write(&junk, vec![0u8; vcam_protocol::HEADER_SIZE]).unwrap();
        let e = live_advert(
            &MappedFile::open(&junk, Access::ReadOnly).unwrap(),
            qpc_now(),
        );
        assert!(e.unwrap_err().contains("invalid"));
    }

    #[test]
    fn the_storm_sets_both_events_the_writer_made_and_nothing_else() {
        let dir = ScratchDir::new("storm-sets");
        let path = dir.join("storm.bin");
        let w = Writer::create(&path).unwrap();
        w.set_state(WriterState::Live);
        let nonces = w.frame_event_nonces().unwrap();
        // a waiter, as the camera service has: SYNCHRONIZE only
        let waiter = open_frame_events(nonces).unwrap();
        let seq = w.header().signal_seq;
        let before = w.header();
        // after a signal the event the next signal sets is unset: so an event that is set afterwards was set by the storm
        w.publish(
            vcam_protocol::SlotFormat {
                width: 16,
                height: 16,
                stride: 16,
                subtype: vcam_protocol::SUBTYPE_NV12,
            },
            qpc_now(),
            |d| d.fill(1),
        )
        .unwrap();
        let after_publish = w.header();
        assert_eq!(after_publish.signal_seq, seq + 1);
        let awaited = vcam_protocol::signal::awaited_event(after_publish.signal_seq);
        assert!(!is_set(&waiter[awaited]), "the awaited event starts unset");

        let events = open_events(nonces).unwrap();
        let n = plain_storm(&events, 0.1);
        assert!(n >= 1024, "{n} SetEvent calls");
        assert!(
            is_set(&waiter[0]) && is_set(&waiter[1]),
            "the storm left an event unset"
        );
        // the storm cannot touch the file: it is as the writer left it
        let after = w.header();
        assert_eq!(
            (after.signal_seq, after.writer_frames, after.features),
            (after_publish.signal_seq, after_publish.writer_frames, 1)
        );
        assert_eq!(before.writer_frames + 1, after.writer_frames);
    }

    /// The storm looks again at the writer while it runs: when `still_ok` says no it ends at once, with the reason,
    /// long before its time is up.
    #[test]
    fn the_storm_ends_when_the_check_says_the_target_is_gone() {
        let dir = ScratchDir::new("storm-ends");
        let w = Writer::create(&dir.join("ends.bin")).unwrap();
        let events = open_events(w.frame_event_nonces().unwrap()).unwrap();
        let mut looks = 0;
        let began = Instant::now();
        let (n, stopped) = storm(&events, 30.0, Duration::from_millis(20), &mut || {
            looks += 1;
            if looks < 3 {
                Ok(())
            } else {
                Err("gone".into())
            }
        });
        assert_eq!(stopped.as_deref(), Some("gone"));
        assert_eq!(looks, 3, "asked again after it said no");
        assert!(
            began.elapsed() < Duration::from_secs(5),
            "{:?}",
            began.elapsed()
        );
        assert!(n >= 1024);
        // a check that always agrees never ends it early, and one that is not due is not asked
        let mut asked = 0;
        let (_, stopped) = storm(&events, 0.1, Duration::from_secs(60), &mut || {
            asked += 1;
            Err("must not be asked".into())
        });
        assert_eq!((stopped, asked), (None, 0));
    }

    /// Against a real writer: the advert the storm began with is compared with the live header at each look. A writer
    /// that leaves Live, or starts over with other nonces, ends the storm.
    #[test]
    fn the_storm_ends_when_the_writer_leaves_live_or_starts_over() {
        let dir = ScratchDir::new("storm-writer");
        let path = dir.join("writer.bin");
        let w = Writer::create(&path).unwrap();
        w.set_state(WriterState::Live);
        let m = map_of(&w, &path);
        let nonces = live_advert(&m, qpc_now()).unwrap();
        let events = open_events(nonces).unwrap();
        let every = Duration::from_millis(20);
        let began = Instant::now();

        // the writer pauses 100 ms in
        let (_, stopped) = std::thread::scope(|sc| {
            sc.spawn(|| {
                std::thread::sleep(Duration::from_millis(100));
                w.set_state(WriterState::Paused);
            });
            storm(&events, 30.0, every, &mut || {
                same_advert(&m, nonces, qpc_now())
            })
        });
        let why = stopped.expect("the storm went on against a paused writer");
        assert!(why.contains("paused") && why.contains("not live"), "{why}");
        assert!(began.elapsed() < Duration::from_secs(5));

        // it goes on while nothing changes
        w.set_state(WriterState::Live);
        let (_, stopped) = storm(&events, 0.2, every, &mut || {
            same_advert(&m, nonces, qpc_now())
        });
        assert_eq!(stopped, None);

        // another writer starts over the same file: other names (or no advert for a moment), never the same ones
        let (_, stopped) = std::thread::scope(|sc| {
            sc.spawn(|| {
                std::thread::sleep(Duration::from_millis(100));
                let second = Writer::create(&path).unwrap();
                second.set_state(WriterState::Live);
                std::thread::sleep(Duration::from_secs(2));
            });
            storm(&events, 30.0, every, &mut || {
                same_advert(&m, nonces, qpc_now())
            })
        });
        assert!(stopped.is_some(), "the storm went on against a new writer");
        // and the check itself names the restart when the new writer is live
        let second = Writer::create(&path).unwrap();
        second.set_state(WriterState::Live);
        let why = same_advert(&m, nonces, qpc_now()).unwrap_err();
        assert!(why.contains("started over"), "{why}");
    }

    #[test]
    fn events_nobody_made_cannot_be_opened() {
        let e = open_events([0x1234_5678_9abc_def0_1111, 0x2222_3333_4444_5555_6666]).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::NotFound, "{e}");
    }
}
