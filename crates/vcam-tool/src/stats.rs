//! `stats`: print the counters of `frames.bin` (read-only mapping, nothing is modified).

use vcam_protocol::{
    frames_path, qpc_frequency, qpc_now, validate_header, Access, Format, MappedFile, WriterState,
    SUBTYPE_NV12, SUBTYPE_RGB32,
};

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

pub fn run() -> i32 {
    let path = frames_path();
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
    use vcam_protocol::{SlotFormat, Writer};

    #[test]
    fn report_has_the_counters_scripts_grep_for() {
        let dir = std::env::temp_dir().join(format!("bettercam-stats-{}", std::process::id()));
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
}
