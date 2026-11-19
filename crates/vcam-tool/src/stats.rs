//! `stats`: print the counters of `frames.bin` (read-only mapping, nothing is modified).

use std::path::PathBuf;

use vcam_protocol::{
    frames_path, qpc_frequency, qpc_now, validate_header, validate_wake, wake_mode, Access, Format,
    MappedFile, WriterState, FEATURE_FRAME_EVENT, SUBTYPE_NV12, SUBTYPE_RGB32,
};

/// `wake_mode` (written by the DLL's pump) in words.
fn wake_mode_name(m: u32) -> String {
    let s = match m {
        wake_mode::POLLING => "polling",
        wake_mode::EVENT => "event",
        wake_mode::POLLING_SPURIOUS => "polling after spurious wake-ups",
        wake_mode::POLLING_MISSED => "polling after missed wake-ups",
        wake_mode::POLLING_OPEN_FAILED => "polling, the events could not be opened",
        _ => "unknown",
    };
    format!("{m} ({s})")
}

fn fmt_name(f: &Format) -> String {
    let s = match f.subtype {
        SUBTYPE_NV12 => "nv12",
        SUBTYPE_RGB32 => "rgb32",
        _ => "?",
    };
    format!("{s} {}x{} @{}/{}", f.width, f.height, f.fps_num, f.fps_den)
}

fn age_ms(then: u64, now: u64) -> String {
    if then == 0 {
        return "never".into();
    }
    let ms = now.saturating_sub(then) as f64 * 1000.0 / qpc_frequency() as f64;
    format!("{ms:.0}")
}

/// The report as `key: value` lines (so scripts can grep them).
pub fn report(map: &MappedFile, path: &std::path::Path, now: u64) -> Vec<String> {
    let h = map.shared().header();
    let mut out = vec![format!("file: {} ({} bytes)", path.display(), map.len())];
    match validate_header(&h, map.len() as u64, Some(qpc_frequency()), Some(now)) {
        Ok(_) => out.push("header: valid".into()),
        Err(e) => out.push(format!("header: INVALID ({e:?})")),
    }
    let state = WriterState::from_u32(h.writer_state)
        .map(|s| s.name())
        .unwrap_or("?");
    out.push(format!("protocol: {}", h.protocol));
    out.push(format!("writer_pid: {}", h.writer_pid));
    out.push(format!("writer_state: {state}"));
    out.push(format!(
        "writer_heartbeat_age_ms: {}",
        age_ms(h.writer_heartbeat, now)
    ));
    out.push(format!("writer_frames: {}", h.writer_frames));
    let n = (h.advertised_count as usize).min(h.advertised.len());
    out.push(format!(
        "advertised: {}",
        h.advertised[..n]
            .iter()
            .map(fmt_name)
            .collect::<Vec<_>>()
            .join(", ")
    ));
    out.push(format!("consumer_active: {}", h.consumer_active));
    out.push(format!(
        "consumer_heartbeat_age_ms: {}",
        age_ms(h.consumer_heartbeat, now)
    ));
    out.push(format!("negotiated: {}", fmt_name(&h.negotiated)));
    out.push(format!("latency_p50_us: {}", h.latency_p50_us));
    out.push(format!("latency_p95_us: {}", h.latency_p95_us));
    out.push(format!("delivered: {}", h.delivered));
    out.push(format!("repeated: {}", h.repeated));
    let pct = if h.delivered == 0 {
        0.0
    } else {
        h.repeated as f64 * 100.0 / h.delivered as f64
    };
    out.push(format!("repeated_pct: {pct:.3}"));
    out.push(format!("latest_slot: {}", h.latest_slot as i32));
    // the writer's frame event advert (the nonces themselves are not printed)
    out.push(format!(
        "features: {}{}",
        h.features,
        if h.features & FEATURE_FRAME_EVENT != 0 {
            " (frame_event)"
        } else {
            ""
        }
    ));
    let nonce = |lo: u64, hi: u64| lo != 0 || hi != 0;
    out.push(format!(
        "nonces_present: {}",
        if nonce(h.event_nonce_lo, h.event_nonce_hi) && nonce(h.event_nonce1_lo, h.event_nonce1_hi)
        {
            "yes"
        } else {
            "no"
        }
    ));
    out.push(format!(
        "frame_event_advert: {}",
        if validate_wake(&h).is_some() {
            "yes"
        } else {
            "no"
        }
    ));
    out.push(format!("signal_seq: {}", h.signal_seq));
    // what the DLL's pump wrote about how it waits (all 0 while no pump has run)
    out.push(format!("wake_mode: {}", wake_mode_name(h.wake_mode)));
    out.push(format!("wake_events: {}", h.wake_events));
    out.push(format!("wake_missed: {}", h.wake_missed));
    out.push(format!("wake_spurious: {}", h.wake_spurious));
    for i in 0..3 {
        let m = map.shared().slot_meta(i);
        out.push(format!(
            "slot{i}: seq {} {}x{} stride {} {} offset {} size {}",
            m.seq,
            m.width,
            m.height,
            m.stride,
            if m.subtype == SUBTYPE_NV12 {
                "nv12"
            } else if m.subtype == SUBTYPE_RGB32 {
                "rgb32"
            } else {
                "?"
            },
            m.data_offset,
            m.data_size
        ));
    }
    out
}

pub const USAGE: &str = "usage: vcam-tool stats [--file PATH]";

/// `stats [--file PATH]`: `--file` reads another frame file (a scratch file of `feed --file`) instead of the real one.
fn parse_args(args: &[String]) -> Result<Option<PathBuf>, String> {
    let mut file = None;
    let mut it = args.iter();
    while let Some(k) = it.next() {
        match k.as_str() {
            "--file" => {
                file = Some(PathBuf::from(it.next().ok_or("--file needs a value")?));
            }
            other => return Err(format!("unknown argument '{other}'")),
        }
    }
    Ok(file)
}

pub fn run(args: &[String]) -> i32 {
    let path = match parse_args(args) {
        Ok(p) => p.unwrap_or_else(frames_path),
        Err(e) => {
            eprintln!("{e}\n{USAGE}");
            return 2;
        }
    };
    match MappedFile::open(&path, Access::ReadOnly) {
        Ok(map) => {
            for l in report(&map, &path, qpc_now()) {
                println!("{l}");
            }
            0
        }
        Err(e) => {
            println!("file: {} not readable: {e}", path.display());
            println!("header: missing");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::ScratchDir;
    use vcam_protocol::{SlotFormat, Writer, WriterOptions};

    #[test]
    fn report_has_the_counters_scripts_grep_for() {
        let dir = ScratchDir::new("stats-report");
        let path = dir.join("frames.bin");
        let w = Writer::create(&path).unwrap();
        w.set_state(WriterState::Live);
        w.publish(
            SlotFormat {
                width: 64,
                height: 64,
                stride: 64,
                subtype: SUBTYPE_NV12,
            },
            qpc_now(),
            |_| {},
        )
        .unwrap();
        w.shared().set_stats(4_000, 9_000, 1000, 5);
        let map = MappedFile::open(&path, Access::ReadOnly).unwrap();
        let lines = report(&map, &path, qpc_now());
        let text = lines.join("\n");
        for key in [
            "header: valid",
            "writer_state: live",
            "latency_p95_us: 9000",
            "delivered: 1000",
            "repeated: 5",
            "repeated_pct: 0.500",
            "writer_frames: 1",
        ] {
            assert!(text.contains(key), "missing '{key}' in:\n{text}");
        }
    }

    fn lines_of(name: &str, opts: &WriterOptions, wake: Option<(u32, u64, u64, u64)>) -> String {
        let dir = ScratchDir::new(&format!("stats-{name}"));
        let path = dir.join("frames.bin");
        let w = Writer::create_with(&path, opts).unwrap();
        w.set_state(WriterState::Live);
        w.publish(
            SlotFormat {
                width: 64,
                height: 64,
                stride: 64,
                subtype: SUBTYPE_NV12,
            },
            qpc_now(),
            |_| {},
        )
        .unwrap();
        if let Some((m, e, mi, s)) = wake {
            w.shared().set_wake_stats(m, e, mi, s);
        }
        let map = MappedFile::open(&path, Access::ReadOnly).unwrap();
        report(&map, &path, qpc_now()).join("\n")
    }

    fn assert_lines(text: &str, keys: &[&str]) {
        for key in keys {
            assert!(
                text.lines().any(|l| l == *key),
                "missing line '{key}' in:\n{text}"
            );
        }
    }

    #[test]
    fn the_frame_event_advert_and_the_dll_wake_diagnostics_are_printed() {
        // a writer with events, after a stream's pump wrote its diagnostics
        let text = lines_of(
            "ev-on",
            &WriterOptions::default(),
            Some((wake_mode::EVENT, 120, 3, 4)),
        );
        assert_lines(
            &text,
            &[
                "features: 1 (frame_event)",
                "nonces_present: yes",
                "frame_event_advert: yes",
                // one signal for the state change and one for the publish
                "signal_seq: 2",
                "wake_mode: 1 (event)",
                "wake_events: 120",
                "wake_missed: 3",
                "wake_spurious: 4",
            ],
        );
        // the nonces are not printed
        assert!(!text.to_lowercase().contains("nonce:"), "{text}");
        // each mode has its words
        for (m, word) in [
            (wake_mode::POLLING, "polling"),
            (wake_mode::POLLING_SPURIOUS, "spurious"),
            (wake_mode::POLLING_MISSED, "missed"),
            (wake_mode::POLLING_OPEN_FAILED, "could not be opened"),
            (77, "unknown"),
        ] {
            let l = wake_mode_name(m);
            assert!(l.starts_with(&format!("{m} (")) && l.contains(word), "{l}");
        }
    }

    #[test]
    fn a_writer_without_events_shows_no_advert_and_zero_counters() {
        let text = lines_of(
            "ev-off",
            &WriterOptions {
                signal: false,
                ..WriterOptions::default()
            },
            None,
        );
        assert_lines(
            &text,
            &[
                "features: 0",
                "nonces_present: no",
                "frame_event_advert: no",
                "signal_seq: 0",
                "wake_mode: 0 (polling)",
                "wake_events: 0",
                "wake_missed: 0",
                "wake_spurious: 0",
            ],
        );
    }

    #[test]
    fn the_file_option_is_the_only_argument() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(parse_args(&[]), Ok(None));
        assert_eq!(
            parse_args(&s(&["--file", r"C:\x\f.bin"])),
            Ok(Some(PathBuf::from(r"C:\x\f.bin")))
        );
        assert!(parse_args(&s(&["--file"])).is_err());
        assert!(parse_args(&s(&["--bogus"])).is_err());
    }
}
