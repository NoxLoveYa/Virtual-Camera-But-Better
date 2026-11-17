//! `capture-test`: open the Better Cam device with a Media Foundation Source Reader (the same path the Camera app
//! and browsers use through Frame Server) and check the stream against what it should show.
//!
//! `--expect feed` (default): the checkerboard + moving stripe of `vcam-tool feed`, stripe advancing, no torn,
//! black or foreign frames. `--expect slate[-paused|-starting]`: every frame is that slate. `--expect feed-to-slate`:
//! feed frames, then the "not running" slate for the rest of the capture (stop the feeder while it runs).

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use vcam_protocol::pattern::{self, Analysis};
use vcam_protocol::slate::{self, Slate, SlateKind};
use vcam_protocol::{qpc_frequency, qpc_now, Access, MappedFile, SUBTYPE_NV12, SUBTYPE_RGB32};
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

pub const USAGE: &str =
    "usage: vcam-tool capture-test [--frames N] [--format nv12|rgb32] [--size WxH] [--fps N]
                             [--expect feed|slate|slate-paused|slate-starting|feed-to-slate]";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fmt {
    Nv12,
    Rgb32,
}

impl Fmt {
    fn subtype(self) -> u32 {
        match self {
            Fmt::Nv12 => SUBTYPE_NV12,
            Fmt::Rgb32 => SUBTYPE_RGB32,
        }
    }
    fn row_bytes(self, width: u32) -> usize {
        match self {
            Fmt::Nv12 => width as usize,
            Fmt::Rgb32 => width as usize * 4,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expect {
    Feed,
    Slate(SlateKind),
    FeedToSlate,
}

pub struct Args {
    frames: usize,
    fmt: Fmt,
    size: (u32, u32),
    fps: Option<u32>,
    expect: Expect,
}

pub fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut a = Args {
        frames: 120,
        fmt: Fmt::Nv12,
        size: (1920, 1080),
        fps: None,
        expect: Expect::Feed,
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
            "--fps" => a.fps = Some(val()?.parse().map_err(|_| "bad --fps")?),
            "--expect" => {
                a.expect = match val()?.to_lowercase().as_str() {
                    "feed" => Expect::Feed,
                    "slate" => Expect::Slate(SlateKind::NotRunning),
                    "slate-paused" => Expect::Slate(SlateKind::Paused),
                    "slate-starting" => Expect::Slate(SlateKind::Starting),
                    "feed-to-slate" => Expect::FeedToSlate,
                    o => {
                        return Err(format!(
                            "unknown expectation '{o}' (feed|slate|slate-paused|slate-starting|feed-to-slate)"
                        ))
                    }
                }
            }
            other => return Err(format!("unknown argument '{other}'")),
        }
    }
    if a.frames < 2 {
        return Err("--frames must be at least 2".into());
    }
    Ok(a)
}

// ---- embedded slates ---------------------------------------------------------------------------------------------

static EMBEDDED: [&[u8]; 3] = [
    include_bytes!("../../vcam-source/assets/slates/not_running.rle"),
    include_bytes!("../../vcam-source/assets/slates/paused.rle"),
    include_bytes!("../../vcam-source/assets/slates/starting.rle"),
];

fn embedded_slate(kind: SlateKind) -> &'static Slate {
    static DECODED: [OnceLock<Slate>; 3] = [const { OnceLock::new() }; 3];
    let i = SlateKind::ALL.iter().position(|k| *k == kind).unwrap();
    DECODED[i].get_or_init(|| slate::decode(EMBEDDED[i]).expect("embedded slate decodes"))
}

/// The slate scaled and converted exactly like the DLL does, as tight rows.
struct ExpectedSlate {
    pixels: Vec<u8>,
    row_bytes: usize,
}

impl ExpectedSlate {
    fn new(kind: SlateKind, fmt: Fmt, width: u32, height: u32) -> ExpectedSlate {
        let s = embedded_slate(kind);
        let pixels = match fmt {
            Fmt::Nv12 => s.scale_nv12(width, height),
            Fmt::Rgb32 => s.scale_bgrx(width, height),
        };
        ExpectedSlate {
            pixels,
            row_bytes: fmt.row_bytes(width),
        }
    }
    fn row(&self, y: u32) -> Vec<u8> {
        self.pixels[y as usize * self.row_bytes..(y as usize + 1) * self.row_bytes].to_vec()
    }
}

/// Mean luma difference under which a frame counts as "this slate".
const SLATE_TOLERANCE: f64 = 2.0;

// ---- per-frame analysis ------------------------------------------------------------------------------------------

struct FrameInfo {
    analysis: Analysis,
    /// Mean luma difference to each slate that was asked about (`NaN` when not computed).
    slate_diff: f64,
}

impl FrameInfo {
    fn is_feed(&self) -> bool {
        self.analysis.slot().is_some() && self.analysis.board_ok
    }
    fn is_slate(&self) -> bool {
        self.slate_diff <= SLATE_TOLERANCE
    }
}

/// Analyse a locked frame. `scan0`/`pitch` as returned by `IMF2DBuffer::Lock2D`.
///
/// # Safety
/// `scan0` must point at the first scanline of a locked buffer holding `height` rows at `pitch` bytes.
unsafe fn analyse_locked(
    scan0: *const u8,
    pitch: i32,
    fmt: Fmt,
    width: u32,
    height: u32,
    slate: Option<&ExpectedSlate>,
) -> FrameInfo {
    let rb = fmt.row_bytes(width);
    let row = |r: u32| std::slice::from_raw_parts(scan0.offset(r as isize * pitch as isize), rb);
    let analysis = pattern::analyse(row, fmt.subtype(), width, height);
    let slate_diff = match slate {
        Some(s) => slate::mean_luma_diff(
            |y| s.row(y),
            |y| row(y).to_vec(),
            width,
            height,
            fmt == Fmt::Rgb32,
            4,
        ),
        None => f64::NAN,
    };
    FrameInfo {
        analysis,
        slate_diff,
    }
}

fn decode(
    sample: &IMFSample,
    fmt: Fmt,
    width: u32,
    height: u32,
    slate: Option<&ExpectedSlate>,
) -> windows_core::Result<FrameInfo> {
    let buffer = unsafe { sample.ConvertToContiguousBuffer()? };
    if let Ok(b2) = buffer.cast::<IMF2DBuffer>() {
        let mut scan0 = std::ptr::null_mut();
        let mut pitch = 0i32;
        unsafe { b2.Lock2D(&mut scan0, &mut pitch)? };
        let info = unsafe { analyse_locked(scan0, pitch, fmt, width, height, slate) };
        unsafe { b2.Unlock2D()? };
        Ok(info)
    } else {
        // contiguous 1-D buffer: assume a tight pitch
        let (mut data, mut max, mut cur) = (std::ptr::null_mut(), 0u32, 0u32);
        unsafe { buffer.Lock(&mut data, Some(&mut max), Some(&mut cur))? };
        let pitch = fmt.row_bytes(width) as i32;
        let info = unsafe { analyse_locked(data, pitch, fmt, width, height, slate) };
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
        let fps = (rate >> 32) as u32 / ((rate & 0xFFFF_FFFF) as u32).max(1);
        seen.push(format!(
            "{} {w}x{h} @{fps}",
            if sub == MFVideoFormat_NV12 {
                "nv12"
            } else if sub == MFVideoFormat_RGB32 {
                "rgb32"
            } else {
                "other"
            },
        ));
        if sub == want && (w, h) == args.size && args.fps.is_none_or(|f| f == fps) {
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
    label: String,
}

/// One frame as it arrived.
struct Rec {
    at: Instant,
    qpc: u64,
    stamp: i64,
    info: FrameInfo,
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
    let rate = unsafe { negotiated.GetUINT64(&MF_MT_FRAME_RATE) }.unwrap_or(0);
    let (w, h) = ((size >> 32) as u32, (size & 0xFFFF_FFFF) as u32);
    let negotiated_fps = (rate >> 32) as f64 / ((rate & 0xFFFF_FFFF) as f64).max(1.0);
    let mut failures = Vec::new();
    let sub_ok = match args.fmt {
        Fmt::Nv12 => sub == MFVideoFormat_NV12,
        Fmt::Rgb32 => sub == MFVideoFormat_RGB32,
    };
    lines.push(format!(
        "negotiated: {} {w}x{h} @{negotiated_fps} (requested {:?} {}x{})",
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

    // what to compare against
    let slate_kinds: Vec<SlateKind> = match args.expect {
        Expect::Feed => vec![],
        Expect::Slate(k) => vec![k],
        Expect::FeedToSlate => vec![SlateKind::NotRunning],
    };
    let expected: Option<ExpectedSlate> = slate_kinds
        .first()
        .map(|k| ExpectedSlate::new(*k, args.fmt, w, h));

    let started = Instant::now();
    let mut recs: Vec<Rec> = Vec::with_capacity(args.frames);
    let mut ticks = 0usize;
    // `--expect feed` ignores frames until the first feed frame (the writer needs a moment to follow the
    // negotiated size), at most 2 s worth
    let mut warmup_frames = 0usize;
    let mut warming = args.expect == Expect::Feed;
    let warmup_limit = Duration::from_secs(2);
    while recs.len() < args.frames {
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
        let qpc = qpc_now();
        let at = Instant::now();
        let info = decode(&sample, args.fmt, w, h, expected.as_ref())
            .map_err(|e| format!("decode: {e}"))?;
        if warming {
            if info.is_feed() {
                warming = false;
            } else {
                warmup_frames += 1;
                if started.elapsed() > warmup_limit {
                    failures.push(format!(
                        "no feed frame within {warmup_limit:?} ({warmup_frames} other frames)"
                    ));
                    break;
                }
                continue;
            }
        }
        recs.push(Rec {
            at,
            qpc,
            stamp: ts,
            info,
        });
    }
    let total = started.elapsed();
    unsafe {
        let _ = source.Shutdown();
        let _ = activate.ShutdownObject();
    }

    // ---- checks common to all expectations
    let n = recs.len();
    lines.push(format!("frames read: {n} in {:.2}s", total.as_secs_f64()));
    if warmup_frames > 0 {
        lines.push(format!(
            "warm-up frames before the first feed frame: {warmup_frames}"
        ));
    }
    if n < args.frames {
        failures.push(format!("only {n} of {} frames read", args.frames));
    }
    if n >= 2 {
        let span = recs[n - 1].at.duration_since(recs[0].at).as_secs_f64();
        let wall = (n - 1) as f64 / span;
        let ts_span = (recs[n - 1].stamp - recs[0].stamp) as f64 / 1e7;
        let ts_fps = if ts_span > 0.0 {
            (n - 1) as f64 / ts_span
        } else {
            0.0
        };
        let max_gap = recs
            .windows(2)
            .map(|p| p[1].at.duration_since(p[0].at))
            .max()
            .unwrap_or(Duration::ZERO);
        lines.push(format!(
            "wall fps: {wall:.2}   timestamp fps: {ts_fps:.2}   max gap: {} ms",
            max_gap.as_millis()
        ));
        // a feed-to-slate run is not steady, the rate there is informational
        if args.expect != Expect::FeedToSlate && negotiated_fps > 0.0 {
            let (lo, hi) = (negotiated_fps * 0.9, negotiated_fps * 1.1);
            if !(lo..=hi).contains(&wall) {
                failures.push(format!(
                    "measured {wall:.2} fps, expected {negotiated_fps} +/- 10%"
                ));
            }
        }
        if max_gap > Duration::from_millis(250) {
            failures.push(format!(
                "stall: {} ms between two frames",
                max_gap.as_millis()
            ));
        }
        if recs.windows(2).any(|p| p[1].stamp <= p[0].stamp) {
            failures.push("sample timestamps did not grow".into());
        }
    }

    match args.expect {
        Expect::Feed => check_feed(&recs, w, &mut lines, &mut failures),
        Expect::Slate(kind) => check_slate(&recs, kind, &mut lines, &mut failures),
        Expect::FeedToSlate => check_feed_to_slate(&recs, &mut lines, &mut failures),
    }
    lines.push(format!("stream ticks: {ticks}"));
    let label = format!("{:?} {w}x{h} {:?}", args.fmt, args.expect);
    Ok(Report {
        lines,
        failures,
        label,
    })
}

fn median(v: &mut [i64]) -> i64 {
    v.sort_unstable();
    v.get(v.len() / 2).copied().unwrap_or(0)
}

fn check_feed(recs: &[Rec], width: u32, lines: &mut Vec<String>, failures: &mut Vec<String>) {
    let n_slots = pattern::slots(width).max(1) as i64;
    let (mut torn, mut black, mut no_stripe, mut board, mut slate) = (0, 0, 0, 0, 0);
    let mut slots: Vec<i64> = Vec::new();
    for r in recs {
        let a = &r.info.analysis;
        if a.black {
            black += 1;
        } else if a.torn() {
            torn += 1;
        } else if let (Some(s), true) = (a.slot(), a.board_ok) {
            slots.push(s as i64);
        } else if a.slot().is_some() {
            board += 1;
        } else if r.info.slate_diff.is_nan() && a.stripes.iter().all(|s| s.is_none()) {
            no_stripe += 1;
        } else {
            slate += 1;
        }
    }
    // signed step between frames, folded into -n/2..n/2
    let mut steps: Vec<i64> = slots
        .windows(2)
        .map(|p| ((p[1] - p[0] + n_slots + n_slots / 2).rem_euclid(n_slots)) - n_slots / 2)
        .collect();
    let backwards = steps.iter().filter(|&&d| d < 0).count();
    let repeats = steps.iter().filter(|&&d| d == 0).count();
    let typical = median(&mut steps.clone());
    let irregular = steps.iter().filter(|&&d| d != typical).count();
    lines.push(format!(
        "stripe: {} frames decoded, typical step {typical} slot(s)/frame, {repeats} repeated, {backwards} backwards, {irregular} irregular",
        slots.len()
    ));
    steps.sort_unstable();
    lines.push(format!(
        "torn frames: {torn}   black frames: {black}   frames without stripe: {no_stripe}   bad checkerboard: {board}   other: {slate}"
    ));
    if torn > 0 {
        failures.push(format!(
            "{torn} torn frame(s): the stripe is at different places on different rows"
        ));
    }
    if black > 0 {
        failures.push(format!("{black} black frame(s)"));
    }
    if no_stripe > 0 || slate > 0 {
        failures.push(format!(
            "{} frame(s) without the feed pattern",
            no_stripe + slate
        ));
    }
    if board > 0 {
        failures.push(format!("{board} frame(s) with a damaged checkerboard"));
    }
    if backwards > 0 {
        failures.push(format!("{backwards} frame(s) moved the stripe backwards"));
    }
    if typical < 1 {
        failures.push("the stripe does not advance".into());
    }
    if irregular * 100 > slots.len().max(1) * 2 {
        failures.push(format!(
            "{irregular} of {} frames broke the typical step of {typical} (more than 2 %)",
            slots.len()
        ));
    }
}

fn check_slate(recs: &[Rec], kind: SlateKind, lines: &mut Vec<String>, failures: &mut Vec<String>) {
    let worst = recs
        .iter()
        .map(|r| r.info.slate_diff)
        .fold(0.0f64, f64::max);
    let mismatched = recs.iter().filter(|r| !r.info.is_slate()).count();
    let feed = recs.iter().filter(|r| r.info.is_feed()).count();
    lines.push(format!(
        "slate '{}': {} of {} frames match (worst mean luma difference {worst:.2}, tolerance {SLATE_TOLERANCE}); {feed} feed frames",
        kind.text(),
        recs.len() - mismatched,
        recs.len()
    ));
    if mismatched > 0 {
        failures.push(format!(
            "{mismatched} frame(s) are not the '{}' slate",
            kind.text()
        ));
    }
}

fn check_feed_to_slate(recs: &[Rec], lines: &mut Vec<String>, failures: &mut Vec<String>) {
    let first_slate = recs
        .iter()
        .position(|r| r.info.is_slate() && !r.info.is_feed());
    let feed_before = recs
        .iter()
        .take(first_slate.unwrap_or(recs.len()))
        .filter(|r| r.info.is_feed())
        .count();
    lines.push(format!(
        "feed frames before the slate: {feed_before}; first slate frame: {}",
        first_slate
            .map(|i| format!("#{i}"))
            .unwrap_or_else(|| "none".into())
    ));
    let Some(i) = first_slate else {
        failures
            .push("the 'not running' slate never appeared (is the feeder still running?)".into());
        return;
    };
    if feed_before == 0 {
        failures
            .push("no feed frame before the slate (start the feeder before the capture)".into());
    }
    let after_bad = recs[i..]
        .iter()
        .filter(|r| !(r.info.is_slate() && !r.info.is_feed()))
        .count();
    if after_bad > 0 {
        failures.push(format!(
            "{after_bad} frame(s) after the first slate frame are not the slate"
        ));
    }
    let other = recs[..i].iter().filter(|r| !r.info.is_feed()).count();
    if other > 0 {
        failures.push(format!(
            "{other} frame(s) before the slate are neither feed nor slate"
        ));
    }
    // how long after the writer's last sign of life did the slate appear
    match MappedFile::open(&vcam_protocol::frames_path(), Access::ReadOnly) {
        Ok(m) => {
            let h = m.shared().header();
            let state = vcam_protocol::WriterState::from_u32(h.writer_state)
                .map(|s| s.name())
                .unwrap_or("?");
            let delta_ms =
                (recs[i].qpc as i128 - h.writer_heartbeat as i128) * 1000 / qpc_frequency() as i128;
            lines.push(format!(
                "writer state at the end: {state}; slate appeared {delta_ms} ms after the writer's last heartbeat"
            ));
            if delta_ms > 1000 {
                failures.push(format!(
                    "the slate took {delta_ms} ms after the last heartbeat (limit 1000 ms)"
                ));
            }
        }
        Err(e) => lines.push(format!(
            "frames.bin not readable ({e}): timing against the heartbeat skipped"
        )),
    }
}

pub fn run(args: &[String]) -> i32 {
    let args = match parse_args(args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}\n{USAGE}");
            return 2;
        }
    };
    // the reader can block forever when no frames arrive: run it on a worker and watch the clock
    let budget = Duration::from_secs(60) + Duration::from_secs_f64(args.frames as f64 / 20.0);
    let (tx, rx) = std::sync::mpsc::channel();
    let fmt_name = format!(
        "{:?} {}x{} {:?}",
        args.fmt, args.size.0, args.size.1, args.expect
    );
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
                println!("PASS ({})", report.label);
                0
            } else {
                for f in &report.failures {
                    println!("  - {f}");
                }
                println!("FAIL ({})", report.label);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn parses_expectations() {
        assert_eq!(parse_args(&[]).unwrap().expect, Expect::Feed);
        assert_eq!(
            parse_args(&s(&["--expect", "slate-paused", "--frames", "10"]))
                .unwrap()
                .expect,
            Expect::Slate(SlateKind::Paused)
        );
        assert_eq!(
            parse_args(&s(&["--expect", "feed-to-slate"]))
                .unwrap()
                .expect,
            Expect::FeedToSlate
        );
        assert_eq!(parse_args(&s(&["--fps", "30"])).unwrap().fps, Some(30));
        assert!(parse_args(&s(&["--expect", "nope"])).is_err());
        assert!(parse_args(&s(&["--frames", "1"])).is_err());
        assert!(parse_args(&s(&["--size", "big"])).is_err());
    }

    fn rec(info: FrameInfo, i: usize) -> Rec {
        Rec {
            at: Instant::now(),
            qpc: i as u64,
            stamp: i as i64 * 333_333,
            info,
        }
    }
    fn feed_info(slot: u32) -> FrameInfo {
        FrameInfo {
            analysis: Analysis {
                stripes: [Some(slot); 3],
                board_ok: true,
                black: false,
            },
            slate_diff: f64::NAN,
        }
    }

    #[test]
    fn feed_check_accepts_a_steady_stripe_and_rejects_trouble() {
        let steady: Vec<Rec> = (0..100)
            .map(|i| rec(feed_info((i * 2 % 118) as u32), i))
            .collect();
        let (mut lines, mut fails) = (vec![], vec![]);
        check_feed(&steady, 1920, &mut lines, &mut fails);
        assert!(fails.is_empty(), "{fails:?} {lines:?}");

        // wrap-around at the slot count is a normal step
        let wrap: Vec<Rec> = (0..300)
            .map(|i| rec(feed_info((i % 118) as u32), i))
            .collect();
        let (mut lines, mut fails) = (vec![], vec![]);
        check_feed(&wrap, 1920, &mut lines, &mut fails);
        assert!(fails.is_empty(), "{fails:?}");

        // a torn frame, a black frame and a backwards step
        let mut bad: Vec<Rec> = (0..100)
            .map(|i| rec(feed_info((i % 118) as u32), i))
            .collect();
        bad[10] = rec(
            FrameInfo {
                analysis: Analysis {
                    stripes: [Some(3), Some(3), Some(9)],
                    board_ok: true,
                    black: false,
                },
                slate_diff: f64::NAN,
            },
            10,
        );
        bad[20] = rec(
            FrameInfo {
                analysis: Analysis {
                    stripes: [None; 3],
                    board_ok: false,
                    black: true,
                },
                slate_diff: f64::NAN,
            },
            20,
        );
        bad[30] = rec(feed_info(5), 30);
        let (mut lines, mut fails) = (vec![], vec![]);
        check_feed(&bad, 1920, &mut lines, &mut fails);
        let text = fails.join("\n");
        assert!(text.contains("torn"), "{text}");
        assert!(text.contains("black"), "{text}");
        assert!(text.contains("backwards"), "{text}");
        let _ = lines;
    }

    #[test]
    fn feed_check_tolerates_a_decimated_stream() {
        // consumer at half the writer rate: every step is 2
        let v: Vec<Rec> = (0..100)
            .map(|i| rec(feed_info(((i * 2) % 118) as u32), i))
            .collect();
        let (mut lines, mut fails) = (vec![], vec![]);
        check_feed(&v, 1920, &mut lines, &mut fails);
        assert!(fails.is_empty(), "{fails:?}");
        assert!(lines[0].contains("typical step 2"), "{lines:?}");
    }

    #[test]
    fn slate_check_needs_every_frame_to_match() {
        let slate_info = |d: f64| FrameInfo {
            analysis: Analysis {
                stripes: [None; 3],
                board_ok: false,
                black: false,
            },
            slate_diff: d,
        };
        let good: Vec<Rec> = (0..10).map(|i| rec(slate_info(0.0), i)).collect();
        let (mut lines, mut fails) = (vec![], vec![]);
        check_slate(&good, SlateKind::NotRunning, &mut lines, &mut fails);
        assert!(fails.is_empty());
        let mut mixed = good;
        mixed[4] = rec(slate_info(40.0), 4);
        let (mut lines, mut fails) = (vec![], vec![]);
        check_slate(&mixed, SlateKind::NotRunning, &mut lines, &mut fails);
        assert_eq!(fails.len(), 1, "{lines:?}");
    }

    #[test]
    fn embedded_slates_decode() {
        for k in SlateKind::ALL {
            let s = embedded_slate(k);
            assert_eq!((s.width, s.height), (1920, 1080));
        }
    }
}
