//! `capture-test`: open the Better Cam device with a Media Foundation Source Reader (the same path the Camera app
//! and browsers use through Frame Server) and check the stream against the P1 test pattern.

use std::time::{Duration, Instant};

use vcam_protocol::pattern::{slot_from_box_x, slots, BOX_SIZE, BOX_STEP, DETECT_LUMA};
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer, IMFMediaSource, IMFSample, IMFSourceReader, MFCreateAttributes,
    MFCreateSourceReaderFromMediaSource, MFMediaType_Video, MFVideoFormat_NV12,
    MFVideoFormat_RGB32, MF_E_NO_MORE_TYPES, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE,
    MF_MT_SUBTYPE, MF_READWRITE_DISABLE_CONVERTERS, MF_SOURCE_READERF_ENDOFSTREAM,
    MF_SOURCE_READERF_ERROR, MF_SOURCE_READERF_STREAMTICK, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
};
use windows_core::{Interface, GUID};

use crate::status::enum_mf_devices;
use crate::util::init_com_and_mf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fmt {
    Nv12,
    Rgb32,
}

pub struct Args {
    frames: usize,
    fmt: Fmt,
    size: (u32, u32),
}

pub fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut a = Args {
        frames: 120,
        fmt: Fmt::Nv12,
        size: (1920, 1080),
    };
    let mut it = args.iter();
    while let Some(k) = it.next() {
        let mut val = || it.next().ok_or_else(|| format!("{k} needs a value"));
        match k.as_str() {
            "--frames" => a.frames = val()?.parse().map_err(|_| "bad --frames")?,
            "--format" => {
                a.fmt = match val()?.to_lowercase().as_str() {
                    "nv12" => Fmt::Nv12,
                    "rgb32" => Fmt::Rgb32,
                    o => return Err(format!("unknown format '{o}' (nv12|rgb32)")),
                }
            }
            "--size" => {
                let v = val()?.to_lowercase();
                let (w, h) = v.split_once('x').ok_or("--size expects WxH")?;
                a.size = (
                    w.parse().map_err(|_| "bad width")?,
                    h.parse().map_err(|_| "bad height")?,
                );
            }
            other => return Err(format!("unknown argument '{other}'")),
        }
    }
    if a.frames < 2 {
        return Err("--frames must be at least 2".into());
    }
    Ok(a)
}

/// What one decoded frame looked like.
struct FrameInfo {
    slot: Option<u32>,
    black: bool,
}

/// Analyse a locked frame. `scan0`/`pitch` as returned by `IMF2DBuffer::Lock2D` (pitch may be negative).
///
/// # Safety
/// `scan0` must point at the first scanline of a locked buffer holding `height` rows at `pitch` bytes.
unsafe fn analyse(scan0: *const u8, pitch: i32, fmt: Fmt, width: u32, height: u32) -> FrameInfo {
    let row_bytes = match fmt {
        Fmt::Nv12 => width as usize,
        Fmt::Rgb32 => width as usize * 4,
    };
    let row =
        |r: u32| std::slice::from_raw_parts(scan0.offset(r as isize * pitch as isize), row_bytes);
    // brightness of pixel x on a row
    let bright = |line: &[u8], x: usize| match fmt {
        Fmt::Nv12 => line[x],
        Fmt::Rgb32 => line[x * 4].min(line[x * 4 + 1]).min(line[x * 4 + 2]),
    };
    let box_row = row(vcam_protocol::pattern::box_y(height) + BOX_SIZE / 2);
    let mut run = 0u32;
    let mut slot = None;
    for x in 0..width as usize {
        if bright(box_row, x) >= DETECT_LUMA {
            run += 1;
            if run == BOX_SIZE / 2 {
                slot = Some(slot_from_box_x(x as u32 + 1 - run));
                break;
            }
        } else {
            run = 0;
        }
    }
    // black: a row away from the box with no pixel above the black level
    let probe = row(height / 2 + BOX_SIZE);
    let any_lit = (0..width as usize).any(|x| match fmt {
        Fmt::Nv12 => probe[x] > 20,
        Fmt::Rgb32 => probe[x * 4] > 8 || probe[x * 4 + 1] > 8 || probe[x * 4 + 2] > 8,
    });
    FrameInfo {
        slot,
        black: !any_lit,
    }
}

fn decode(
    sample: &IMFSample,
    fmt: Fmt,
    width: u32,
    height: u32,
) -> windows_core::Result<FrameInfo> {
    let buffer = unsafe { sample.ConvertToContiguousBuffer()? };
    if let Ok(b2) = buffer.cast::<IMF2DBuffer>() {
        let mut scan0 = std::ptr::null_mut();
        let mut pitch = 0i32;
        unsafe { b2.Lock2D(&mut scan0, &mut pitch)? };
        let info = unsafe { analyse(scan0, pitch, fmt, width, height) };
        unsafe { b2.Unlock2D()? };
        Ok(info)
    } else {
        // contiguous 1-D buffer: assume a tight pitch
        let (mut data, mut max, mut cur) = (std::ptr::null_mut(), 0u32, 0u32);
        unsafe { buffer.Lock(&mut data, Some(&mut max), Some(&mut cur))? };
        let pitch = match fmt {
            Fmt::Nv12 => width as i32,
            Fmt::Rgb32 => width as i32 * 4,
        };
        let info = unsafe { analyse(data, pitch, fmt, width, height) };
        unsafe { buffer.Unlock()? };
        Ok(info)
    }
}

fn select_native_type(reader: &IMFSourceReader, args: &Args) -> Result<(), String> {
    let want: GUID = match args.fmt {
        Fmt::Nv12 => MFVideoFormat_NV12,
        Fmt::Rgb32 => MFVideoFormat_RGB32,
    };
    let mut seen = Vec::new();
    for i in 0.. {
        let t = match unsafe {
            reader.GetNativeMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, i)
        } {
            Ok(t) => t,
            Err(e) if e.code() == MF_E_NO_MORE_TYPES => break,
            Err(e) => return Err(format!("GetNativeMediaType({i}): {e}")),
        };
        let sub = unsafe { t.GetGUID(&MF_MT_SUBTYPE) }.unwrap_or_default();
        let size = unsafe { t.GetUINT64(&MF_MT_FRAME_SIZE) }.unwrap_or(0);
        let rate = unsafe { t.GetUINT64(&MF_MT_FRAME_RATE) }.unwrap_or(0);
        let (w, h) = ((size >> 32) as u32, (size & 0xFFFF_FFFF) as u32);
        seen.push(format!(
            "{} {w}x{h} @{}/{}",
            if sub == MFVideoFormat_NV12 {
                "nv12"
            } else if sub == MFVideoFormat_RGB32 {
                "rgb32"
            } else {
                "other"
            },
            rate >> 32,
            rate & 0xFFFF_FFFF
        ));
        if sub == want && (w, h) == args.size {
            unsafe {
                reader.SetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, None, &t)
            }
            .map_err(|e| format!("SetCurrentMediaType: {e}"))?;
            return Ok(());
        }
    }
    Err(format!(
        "device does not offer that format/size; native types: {}",
        seen.join(", ")
    ))
}

struct Report {
    lines: Vec<String>,
    failures: Vec<String>,
}

fn run_capture(args: &Args) -> Result<Report, String> {
    init_com_and_mf().map_err(|e| format!("MFStartup: {e}"))?;
    let want = vcam_protocol::CAMERA_NAME.to_lowercase();
    let devices = enum_mf_devices().map_err(|e| format!("MFEnumDeviceSources: {e}"))?;
    let (activate, dev) = devices
        .into_iter()
        .find(|(_, d)| d.name.to_lowercase().contains(&want))
        .ok_or("Better Cam device not listed (run vcam-tool register)")?;
    let mut lines = vec![format!("device: '{}' link '{}'", dev.name, dev.link)];

    let source: IMFMediaSource =
        unsafe { activate.ActivateObject() }.map_err(|e| format!("ActivateObject: {e}"))?;
    let mut attrs = None;
    unsafe { MFCreateAttributes(&mut attrs, 2) }.map_err(|e| format!("MFCreateAttributes: {e}"))?;
    let attrs = attrs.unwrap();
    // keep the device's native types: no colour converter between source and test
    unsafe { attrs.SetUINT32(&MF_READWRITE_DISABLE_CONVERTERS, 1) }.map_err(|e| e.to_string())?;
    let reader = unsafe { MFCreateSourceReaderFromMediaSource(&source, &attrs) }
        .map_err(|e| format!("MFCreateSourceReaderFromMediaSource: {e}"))?;
    select_native_type(&reader, args)?;

    let negotiated =
        unsafe { reader.GetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32) }
            .map_err(|e| format!("GetCurrentMediaType: {e}"))?;
    let sub = unsafe { negotiated.GetGUID(&MF_MT_SUBTYPE) }.unwrap_or_default();
    let major = unsafe { negotiated.GetGUID(&MF_MT_MAJOR_TYPE) }.unwrap_or_default();
    let size = unsafe { negotiated.GetUINT64(&MF_MT_FRAME_SIZE) }.unwrap_or(0);
    let (w, h) = ((size >> 32) as u32, (size & 0xFFFF_FFFF) as u32);
    let mut failures = Vec::new();
    let sub_ok = match args.fmt {
        Fmt::Nv12 => sub == MFVideoFormat_NV12,
        Fmt::Rgb32 => sub == MFVideoFormat_RGB32,
    };
    lines.push(format!(
        "negotiated: {} {w}x{h} (requested {:?} {}x{})",
        if sub == MFVideoFormat_NV12 {
            "nv12"
        } else if sub == MFVideoFormat_RGB32 {
            "rgb32"
        } else {
            "other"
        },
        args.fmt,
        args.size.0,
        args.size.1
    ));
    if !sub_ok || (w, h) != args.size || major != MFMediaType_Video {
        failures.push("negotiated type differs from the requested one".to_string());
    }

    let n_slots = slots(w.max(1));
    let started = Instant::now();
    let mut arrivals: Vec<Instant> = Vec::with_capacity(args.frames);
    let mut stamps: Vec<i64> = Vec::with_capacity(args.frames);
    let mut seq_slots: Vec<u32> = Vec::with_capacity(args.frames);
    let mut black = 0usize;
    let mut no_box = 0usize;
    let mut ticks = 0usize;
    while arrivals.len() < args.frames {
        let (mut flags, mut ts) = (0u32, 0i64);
        let mut sample: Option<IMFSample> = None;
        let mut actual = 0u32;
        unsafe {
            reader.ReadSample(
                MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
                0,
                Some(&mut actual),
                Some(&mut flags),
                Some(&mut ts),
                Some(&mut sample),
            )
        }
        .map_err(|e| format!("ReadSample: {e}"))?;
        if flags & MF_SOURCE_READERF_ERROR.0 as u32 != 0 {
            failures.push("reader reported an error".into());
            break;
        }
        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
            failures.push("end of stream".into());
            break;
        }
        if flags & MF_SOURCE_READERF_STREAMTICK.0 as u32 != 0 {
            ticks += 1;
        }
        let Some(sample) = sample else { continue };
        let now = Instant::now();
        let info = decode(&sample, args.fmt, w, h).map_err(|e| format!("decode: {e}"))?;
        arrivals.push(now);
        stamps.push(ts);
        if info.black {
            black += 1;
        }
        match info.slot {
            Some(s) => seq_slots.push(s),
            None => no_box += 1,
        }
    }
    let total = started.elapsed();
    unsafe {
        let _ = source.Shutdown();
        let _ = activate.ShutdownObject();
    }

    // checks
    let n = arrivals.len();
    lines.push(format!("frames read: {n} in {:.2}s", total.as_secs_f64()));
    if n < args.frames {
        failures.push(format!("only {n} of {} frames read", args.frames));
    }
    if n >= 2 {
        let wall = (n - 1) as f64 / arrivals[n - 1].duration_since(arrivals[0]).as_secs_f64();
        let ts_span = (stamps[n - 1] - stamps[0]) as f64 / 1e7;
        let ts_fps = if ts_span > 0.0 {
            (n - 1) as f64 / ts_span
        } else {
            0.0
        };
        let max_gap = arrivals
            .windows(2)
            .map(|p| p[1].duration_since(p[0]))
            .max()
            .unwrap_or(Duration::ZERO);
        lines.push(format!(
            "wall fps: {wall:.2}   timestamp fps: {ts_fps:.2}   max gap: {} ms",
            max_gap.as_millis()
        ));
        if !(27.0..=33.0).contains(&wall) {
            failures.push(format!("measured {wall:.2} fps, expected 30 +/- 10%"));
        }
        if max_gap > Duration::from_millis(250) {
            failures.push(format!(
                "stall: {} ms between two frames",
                max_gap.as_millis()
            ));
        }
    }
    // box must advance exactly one slot per frame
    let mut jumps = Vec::new();
    for (i, p) in seq_slots.windows(2).enumerate() {
        if n_slots > 0 && (p[0] + 1) % n_slots != p[1] {
            jumps.push((i + 1, p[0], p[1]));
        }
    }
    lines.push(format!(
        "box step {BOX_STEP}px/frame: {} frames decoded, {} out of sequence, {} without a box",
        seq_slots.len(),
        jumps.len(),
        no_box
    ));
    if let Some(&(i, a, b)) = jumps.first() {
        failures.push(format!(
            "{} frame(s) out of sequence, first at read #{i}: slot {a} -> {b} (expected {})",
            jumps.len(),
            (a + 1) % n_slots
        ));
    }
    if no_box > 0 {
        failures.push(format!("{no_box} frame(s) had no white box"));
    }
    lines.push(format!("black frames: {black}   stream ticks: {ticks}"));
    if black > 0 {
        failures.push(format!("{black} black frame(s)"));
    }
    Ok(Report { lines, failures })
}

pub fn run(args: &[String]) -> i32 {
    let args = match parse_args(args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}\nusage: vcam-tool capture-test [--frames N] [--format nv12|rgb32] [--size WxH]");
            return 2;
        }
    };
    // the reader can block forever when no frames arrive: run it on a worker and watch the clock
    let budget = Duration::from_secs(60) + Duration::from_secs_f64(args.frames as f64 / 20.0);
    let (tx, rx) = std::sync::mpsc::channel();
    let fmt_name = format!("{:?} {}x{}", args.fmt, args.size.0, args.size.1);
    std::thread::spawn(move || {
        let r = run_capture(&args);
        let _ = tx.send(r);
    });
    match rx.recv_timeout(budget) {
        Ok(Ok(report)) => {
            for l in &report.lines {
                println!("{l}");
            }
            if report.failures.is_empty() {
                println!("PASS ({fmt_name})");
                0
            } else {
                for f in &report.failures {
                    println!("  - {f}");
                }
                println!("FAIL ({fmt_name})");
                1
            }
        }
        Ok(Err(e)) => {
            println!("  - {e}");
            println!("FAIL ({fmt_name})");
            1
        }
        Err(_) => {
            println!("  - no result within {budget:?}: the stream stalled");
            println!("FAIL ({fmt_name})");
            std::process::exit(1);
        }
    }
}
