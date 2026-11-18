//! Headless engine: camera (or synthetic bars) -> GPU pipeline -> `frames.bin`, with a stats line every second.
//!
//! ```text
//! cargo run -p better-cam --release --example engine_headless -- \
//!     --device 0 --format mjpeg --size 1920x1080 --fps 60 --output 1920x1080 --seconds 30
//! ```
//!
//! Options (all optional):
//! * source: `--device N`, `--format mjpeg|nv12|yuy2|...`, `--size WxH`, `--fps N`, `--hw-decode on|off`
//! * or `--synthetic bars` with `--synth-format nv12|yuy2|i420|yv12|p010|xrgb`, `--synth-size WxH` (default: the
//!   output size) and `--fps N` (default 30): generated SMPTE bars through the same pipeline
//! * output: `--output WxH`, `--out-fps N`, `--fit fit|fill|stretch`, `--flip`, `--readback immediate|next`,
//!   `--color-space 601|709`, `--color-range limited|full`
//! * run: `--seconds N` (default 10; Ctrl+C also stops it)
//! * picture check: `--dump file.png|file.bmp` opens the Better Cam camera as a consumer (the way Teams or the Camera
//!   app does), saves one frame, prints its statistics, and with `--synthetic bars` compares it with the reference
//!   bars. `--consumer-format nv12|rgb32` and `--consumer-size WxH` choose what the consumer asks for;
//!   `--dump-gain N` also saves a brightened copy (`<name>_xN`) for dark scenes.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use better_cam::capture::{self, FormatRequest, MfSession, PixelFormat, Ratio};
use better_cam::engine::{
    Engine, OutputSettings, ReadbackPolicy, Settings, SourceSpec, SyntheticSpec,
};
use better_cam::gpu::convert::{ColorRange, ColorSettings, ColorSpace};
use better_cam::gpu::output::FitMode;
use better_cam::gpu::pattern;
use vcam_protocol::{WriterState, SUBTYPE_NV12, SUBTYPE_RGB32};
use windows::core::{Interface, BOOL};
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer, IMFActivate, IMFMediaSource, IMFSample, MFCreateAttributes,
    MFCreateSourceReaderFromMediaSource, MFEnumDeviceSources, MFMediaType_Video,
    MFVideoFormat_NV12, MFVideoFormat_RGB32, MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
    MF_E_NO_MORE_TYPES, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE,
    MF_READWRITE_DISABLE_CONVERTERS, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Console::{SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_C_EVENT};

static STOP: AtomicBool = AtomicBool::new(false);

unsafe extern "system" fn on_ctrl(kind: u32) -> BOOL {
    if kind == CTRL_C_EVENT || kind == CTRL_BREAK_EVENT {
        STOP.store(true, Ordering::Release);
        return BOOL(1);
    }
    BOOL(0)
}

// ---------------------------------------------------------------------------------------------------------------
// Arguments

struct Args {
    device: usize,
    format: PixelFormat,
    size: Option<(u32, u32)>,
    fps: Option<u32>,
    hw_decode: bool,
    synthetic: bool,
    synth_format: PixelFormat,
    synth_size: Option<(u32, u32)>,
    output: Option<(u32, u32)>,
    out_fps: Option<u32>,
    fit: FitMode,
    flip: bool,
    readback: ReadbackPolicy,
    color: ColorSettings,
    seconds: u64,
    dump: Option<String>,
    dump_gain: u32,
    consume: Option<u32>,
    probe_cpu: bool,
    consumer_nv12: bool,
    consumer_size: Option<(u32, u32)>,
}

fn parse_size(v: &str) -> Result<(u32, u32), String> {
    let (w, h) = v
        .to_lowercase()
        .split_once('x')
        .map(|(w, h)| (w.to_string(), h.to_string()))
        .ok_or_else(|| format!("'{v}' is not WxH"))?;
    Ok((
        w.parse().map_err(|_| format!("bad width in '{v}'"))?,
        h.parse().map_err(|_| format!("bad height in '{v}'"))?,
    ))
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        device: 0,
        format: PixelFormat::Mjpeg,
        size: None,
        fps: None,
        hw_decode: true,
        synthetic: false,
        synth_format: PixelFormat::Nv12,
        synth_size: None,
        output: None,
        out_fps: None,
        fit: FitMode::Fit,
        flip: false,
        readback: ReadbackPolicy::Immediate,
        color: ColorSettings::default(),
        seconds: 10,
        dump: None,
        dump_gain: 1,
        consume: None,
        probe_cpu: false,
        consumer_nv12: true,
        consumer_size: None,
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut it = args.iter();
    while let Some(k) = it.next() {
        let mut val = || {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{k} needs a value"))
        };
        match k.as_str() {
            "--device" => a.device = val()?.parse().map_err(|_| "bad --device")?,
            "--format" => {
                let v = val()?;
                a.format =
                    PixelFormat::from_name(&v).ok_or_else(|| format!("unknown format '{v}'"))?;
            }
            "--size" => a.size = Some(parse_size(&val()?)?),
            "--fps" => a.fps = Some(val()?.parse().map_err(|_| "bad --fps")?),
            "--hw-decode" => {
                a.hw_decode = match val()?.as_str() {
                    "on" => true,
                    "off" => false,
                    o => return Err(format!("--hw-decode on|off, got '{o}'")),
                }
            }
            "--synthetic" => match val()?.as_str() {
                "bars" => a.synthetic = true,
                o => return Err(format!("--synthetic bars, got '{o}'")),
            },
            "--synth-format" => {
                let v = val()?;
                a.synth_format =
                    PixelFormat::from_name(&v).ok_or_else(|| format!("unknown format '{v}'"))?;
            }
            "--synth-size" => a.synth_size = Some(parse_size(&val()?)?),
            "--output" => a.output = Some(parse_size(&val()?)?),
            "--out-fps" => a.out_fps = Some(val()?.parse().map_err(|_| "bad --out-fps")?),
            "--fit" => {
                a.fit = match val()?.as_str() {
                    "fit" => FitMode::Fit,
                    "fill" => FitMode::Fill,
                    "stretch" => FitMode::Stretch,
                    o => return Err(format!("--fit fit|fill|stretch, got '{o}'")),
                }
            }
            "--flip" => a.flip = true,
            "--readback" => {
                a.readback = match val()?.as_str() {
                    "immediate" => ReadbackPolicy::Immediate,
                    "next" => ReadbackPolicy::NextFrame,
                    o => return Err(format!("--readback immediate|next, got '{o}'")),
                }
            }
            "--color-space" => {
                a.color.space = Some(match val()?.as_str() {
                    "601" => ColorSpace::Bt601,
                    "709" => ColorSpace::Bt709,
                    o => return Err(format!("--color-space 601|709, got '{o}'")),
                })
            }
            "--color-range" => {
                a.color.range = Some(match val()?.as_str() {
                    "limited" => ColorRange::Limited,
                    "full" => ColorRange::Full,
                    o => return Err(format!("--color-range limited|full, got '{o}'")),
                })
            }
            "--seconds" => a.seconds = val()?.parse().map_err(|_| "bad --seconds")?,
            "--dump" => a.dump = Some(val()?),
            "--dump-gain" => a.dump_gain = val()?.parse().map_err(|_| "bad --dump-gain")?,
            "--consume" => a.consume = Some(val()?.parse().map_err(|_| "bad --consume")?),
            "--probe-cpu" => a.probe_cpu = true,
            "--consumer-format" => {
                a.consumer_nv12 = match val()?.as_str() {
                    "nv12" => true,
                    "rgb32" => false,
                    o => return Err(format!("--consumer-format nv12|rgb32, got '{o}'")),
                }
            }
            "--consumer-size" => a.consumer_size = Some(parse_size(&val()?)?),
            "--help" | "-h" => return Err("see the header of examples/engine_headless.rs".into()),
            other => return Err(format!("unknown argument '{other}'")),
        }
    }
    Ok(a)
}

// ---------------------------------------------------------------------------------------------------------------
// Picture files

fn crc32(data: &[u8], table: &[u32; 256], mut crc: u32) -> u32 {
    crc = !crc;
    for &b in data {
        crc = table[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    !crc
}

/// A PNG made of stored (uncompressed) deflate blocks: big, but needs no compressor.
fn encode_png(rgb: &[u8], w: u32, h: u32) -> Vec<u8> {
    let mut table = [0u32; 256];
    for (i, t) in table.iter_mut().enumerate() {
        let mut c = i as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
        *t = c;
    }
    let mut raw = Vec::with_capacity((w as usize * 3 + 1) * h as usize);
    for y in 0..h as usize {
        raw.push(0);
        raw.extend_from_slice(&rgb[y * w as usize * 3..(y + 1) * w as usize * 3]);
    }
    let mut z = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = raw.chunks(65535).collect();
    for (i, b) in blocks.iter().enumerate() {
        z.push((i + 1 == blocks.len()) as u8);
        z.extend_from_slice(&(b.len() as u16).to_le_bytes());
        z.extend_from_slice(&(!(b.len() as u16)).to_le_bytes());
        z.extend_from_slice(b);
    }
    let (mut s1, mut s2) = (1u32, 0u32);
    for &b in &raw {
        s1 = (s1 + b as u32) % 65521;
        s2 = (s2 + s1) % 65521;
    }
    z.extend_from_slice(&((s2 << 16) | s1).to_be_bytes());

    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut chunk = |kind: &[u8; 4], data: &[u8]| {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        let crc = crc32(data, &table, crc32(kind, &table, 0));
        out.extend_from_slice(&crc.to_be_bytes());
    };
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(b"IHDR", &ihdr);
    chunk(b"IDAT", &z);
    chunk(b"IEND", &[]);
    out
}

fn encode_bmp(rgb: &[u8], w: u32, h: u32) -> Vec<u8> {
    let row = (w as usize * 3).div_ceil(4) * 4;
    let size = 54 + row * h as usize;
    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&(size as u32).to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&54u32.to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(w as i32).to_le_bytes());
    out.extend_from_slice(&(h as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&24u16.to_le_bytes());
    out.extend_from_slice(&[0; 24]);
    for y in (0..h as usize).rev() {
        for x in 0..w as usize {
            let p = &rgb[(y * w as usize + x) * 3..][..3];
            out.extend_from_slice(&[p[2], p[1], p[0]]);
        }
        out.resize(out.len() + row - w as usize * 3, 0);
    }
    out
}

fn save_picture(path: &str, rgb: &[u8], w: u32, h: u32) -> std::io::Result<()> {
    let bytes = if path.to_lowercase().ends_with(".bmp") {
        encode_bmp(rgb, w, h)
    } else {
        encode_png(rgb, w, h)
    };
    std::fs::write(path, bytes)
}

fn brightened(rgb: &[u8], gain: u32) -> Vec<u8> {
    rgb.iter()
        .map(|&v| (v as u32 * gain).min(255) as u8)
        .collect()
}

// ---------------------------------------------------------------------------------------------------------------
// The consumer: Better Cam opened through Media Foundation, like any app does

struct ConsumerFrame {
    width: u32,
    height: u32,
    rgb: Vec<u8>,
    pitch: i32,
    contiguous: u32,
    frames_seen: u32,
}

/// What a run of `--consume N` saw.
#[derive(Default)]
struct ConsumerStats {
    frames: u32,
    wall_secs: f64,
    /// Frames per second by the samples' own timestamps.
    stamp_fps: f64,
    max_gap_ms: f64,
    /// Frames whose pixels differ from the previous frame (the camera's real rate shows here; the DLL repeats the last
    /// frame to keep the stream's cadence).
    distinct: u32,
    /// Sampled mean luma (Y code) of each frame: min, median, max.
    luma: (f64, f64, f64),
    /// Frames whose mean luma is far from the median (a slate or a black frame between camera frames).
    odd: u32,
    /// When the first frame far from the luma of the first 20 frames arrived (the slate after the engine stopped).
    first_odd_at: Option<Instant>,
}

/// Set by the main thread when the engine stopped, so the consumer's report can say how fast the slate followed.
static ENGINE_STOPPED_AT: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

/// FNV-1a hash and mean of a sparse sample of the luma plane.
fn sample_luma(scan0: *const u8, pitch: i32, w: u32, h: u32) -> (u64, f64) {
    let (mut hash, mut sum, mut n) = (0xcbf2_9ce4_8422_2325u64, 0u64, 0u64);
    for y in (0..h).step_by(7) {
        for x in (0..w).step_by(5) {
            // SAFETY: rows y < h of a locked buffer, x < w.
            let v = unsafe { *scan0.offset(y as isize * pitch as isize + x as isize) };
            hash = (hash ^ v as u64).wrapping_mul(0x100_0000_01b3);
            sum += v as u64;
            n += 1;
        }
    }
    (hash, sum as f64 / n.max(1) as f64)
}

fn better_cam_device() -> Result<(IMFActivate, String), String> {
    let mut attrs = None;
    unsafe { MFCreateAttributes(&mut attrs, 1) }.map_err(|e| e.to_string())?;
    let attrs = attrs.ok_or("no attributes")?;
    unsafe {
        attrs
            .SetGUID(
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
            )
            .map_err(|e| e.to_string())?;
    }
    let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    unsafe { MFEnumDeviceSources(&attrs, &mut list, &mut count) }.map_err(|e| e.to_string())?;
    let mut found = None;
    for i in 0..count as usize {
        let item = unsafe { std::ptr::read(list.add(i)) };
        let Some(a) = item else { continue };
        let mut p = windows::core::PWSTR::null();
        let mut len = 0u32;
        let name = unsafe {
            if a.GetAllocatedString(&MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, &mut p, &mut len)
                .is_ok()
            {
                let s = p.to_string().unwrap_or_default();
                CoTaskMemFree(Some(p.0 as *const _));
                s
            } else {
                String::new()
            }
        };
        if found.is_none() && name.to_lowercase().contains("better cam") {
            found = Some((a, name));
        }
    }
    if !list.is_null() {
        unsafe { CoTaskMemFree(Some(list as *const _)) };
    }
    found.ok_or_else(|| "the Better Cam device is not listed (vcam-tool status)".to_string())
}

/// Read frames from the virtual camera until `skip` frames of the wanted size went by, then convert the next one to
/// RGB. NV12 is decoded as BT.709 limited range (what the camera advertises).
fn consumer_capture(
    nv12: bool,
    size: (u32, u32),
    skip: u32,
    take: u32,
) -> Result<(ConsumerFrame, ConsumerStats), String> {
    let _mf = MfSession::start().map_err(|e| e.to_string())?;
    let (activate, _name) = better_cam_device()?;
    let source: IMFMediaSource = unsafe { activate.ActivateObject() }.map_err(|e| e.to_string())?;
    let mut attrs = None;
    unsafe { MFCreateAttributes(&mut attrs, 1) }.map_err(|e| e.to_string())?;
    let attrs = attrs.ok_or("no attributes")?;
    unsafe { attrs.SetUINT32(&MF_READWRITE_DISABLE_CONVERTERS, 1) }.map_err(|e| e.to_string())?;
    let reader = unsafe { MFCreateSourceReaderFromMediaSource(&source, &attrs) }
        .map_err(|e| e.to_string())?;
    let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
    let want = if nv12 {
        MFVideoFormat_NV12
    } else {
        MFVideoFormat_RGB32
    };
    let mut chosen = false;
    for i in 0.. {
        let t = match unsafe { reader.GetNativeMediaType(stream, i) } {
            Ok(t) => t,
            Err(e) if e.code() == MF_E_NO_MORE_TYPES => break,
            Err(e) => return Err(format!("GetNativeMediaType: {e}")),
        };
        let sub = unsafe { t.GetGUID(&MF_MT_SUBTYPE) }.unwrap_or_default();
        let sz = unsafe { t.GetUINT64(&MF_MT_FRAME_SIZE) }.unwrap_or(0);
        let major = unsafe { t.GetGUID(&MF_MT_MAJOR_TYPE) }.unwrap_or_default();
        if major == MFMediaType_Video && sub == want && ((sz >> 32) as u32, sz as u32) == size {
            unsafe { reader.SetCurrentMediaType(stream, None, &t) }.map_err(|e| e.to_string())?;
            chosen = true;
            break;
        }
    }
    if !chosen {
        return Err(format!(
            "Better Cam does not offer {}x{} in that format",
            size.0, size.1
        ));
    }
    let (w, h) = size;
    let mut seen = 0u32;
    let deadline = Instant::now() + Duration::from_secs(40);
    let mut picture: Option<ConsumerFrame> = None;
    let mut stats = ConsumerStats::default();
    let (mut first_at, mut last_at) = (None::<Instant>, Instant::now());
    let (mut first_ts, mut last_ts) = (0i64, 0i64);
    let mut lumas: Vec<f64> = Vec::new();
    let mut arrivals: Vec<Instant> = Vec::new();
    let mut last_hash = None;
    let result = loop {
        if Instant::now() > deadline {
            break Err("not enough frames arrived within 40 s".to_string());
        }
        let mut sample: Option<IMFSample> = None;
        let (mut flags, mut ts, mut actual) = (0u32, 0i64, 0u32);
        unsafe {
            reader.ReadSample(
                stream,
                0,
                Some(&mut actual),
                Some(&mut flags),
                Some(&mut ts),
                Some(&mut sample),
            )
        }
        .map_err(|e| format!("ReadSample: {e}"))?;
        let Some(sample) = sample else { continue };
        seen += 1;
        if seen <= skip {
            continue;
        }
        let now = Instant::now();
        let buffer = unsafe { sample.ConvertToContiguousBuffer() }.map_err(|e| e.to_string())?;
        let (mut scan0, mut pitch) = (std::ptr::null_mut(), 0i32);
        let b2 = buffer.cast::<IMF2DBuffer>().ok();
        let contiguous = b2
            .as_ref()
            .and_then(|b| unsafe { b.GetContiguousLength() }.ok())
            .unwrap_or(0);
        // Lock the frame, sample it for the rate statistics, and copy it once for the picture.
        let (hash, mean, copy) = if let Some(b2) = &b2 {
            unsafe { b2.Lock2D(&mut scan0, &mut pitch) }.map_err(|e| e.to_string())?;
            let (hash, mean) = if nv12 {
                sample_luma(scan0, pitch, w, h)
            } else {
                (0, 0.0)
            };
            let copy = (picture.is_none()).then(|| {
                let rows = if nv12 { h * 3 / 2 } else { h } as usize;
                unsafe {
                    std::slice::from_raw_parts(
                        scan0 as *const u8,
                        pitch.unsigned_abs() as usize * rows,
                    )
                }
                .to_vec()
            });
            unsafe { b2.Unlock2D() }.ok();
            (hash, mean, copy)
        } else {
            let (mut p, mut cur) = (std::ptr::null_mut(), 0u32);
            unsafe { buffer.Lock(&mut p, None, Some(&mut cur)) }.map_err(|e| e.to_string())?;
            pitch = if nv12 { w as i32 } else { w as i32 * 4 };
            let (hash, mean) = if nv12 {
                sample_luma(p, pitch, w, h)
            } else {
                (0, 0.0)
            };
            let copy = (picture.is_none()).then(|| {
                unsafe { std::slice::from_raw_parts(p as *const u8, cur as usize) }.to_vec()
            });
            unsafe { buffer.Unlock() }.ok();
            (hash, mean, copy)
        };
        if let Some(data) = copy {
            let rgb = if nv12 {
                pattern::nv12_to_rgb(
                    &data,
                    w,
                    h,
                    pitch.unsigned_abs() as usize,
                    ColorSpace::Bt709,
                    ColorRange::Limited,
                )
            } else {
                pattern::bgrx_to_rgb(&data, w, h, pitch.unsigned_abs() as usize)
            };
            picture = Some(ConsumerFrame {
                width: w,
                height: h,
                rgb,
                pitch,
                contiguous,
                frames_seen: seen,
            });
        }
        if stats.frames == 0 {
            first_at = Some(now);
            first_ts = ts;
        } else {
            stats.max_gap_ms = stats
                .max_gap_ms
                .max(now.duration_since(last_at).as_secs_f64() * 1000.0);
        }
        if last_hash != Some(hash) {
            stats.distinct += 1;
        }
        last_hash = Some(hash);
        last_at = now;
        last_ts = ts;
        lumas.push(mean);
        arrivals.push(now);
        stats.frames += 1;
        if stats.frames >= take.max(1) {
            break Ok(());
        }
    };
    unsafe {
        let _ = source.Shutdown();
        let _ = activate.ShutdownObject();
    }
    result?;
    stats.wall_secs = first_at.map_or(0.0, |t| last_at.duration_since(t).as_secs_f64());
    let span = (last_ts - first_ts) as f64 / 1e7;
    stats.stamp_fps = if span > 0.0 {
        (stats.frames - 1) as f64 / span
    } else {
        0.0
    };
    let mut sorted = lumas.clone();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let median = sorted[sorted.len() / 2];
    stats.luma = (sorted[0], median, sorted[sorted.len() - 1]);
    stats.odd = lumas
        .iter()
        .filter(|m| (**m - median).abs() > median.max(8.0) * 0.5)
        .count() as u32;
    let mut head: Vec<f64> = lumas.iter().take(20).copied().collect();
    head.sort_by(|a, b| a.total_cmp(b));
    let base = head.get(head.len() / 2).copied().unwrap_or(median);
    stats.first_odd_at = lumas
        .iter()
        .zip(&arrivals)
        .skip(20)
        .find(|(m, _)| (**m - base).abs() > base.max(8.0) * 0.5)
        .map(|(_, t)| *t);
    Ok((picture.ok_or("no picture captured")?, stats))
}

fn rgb_stats(rgb: &[u8]) -> String {
    let n = (rgb.len() / 3).max(1) as f64;
    let mut sum = [0f64; 3];
    let mut max = [0u8; 3];
    let mut luma = 0f64;
    for p in rgb.chunks(3) {
        for i in 0..3 {
            sum[i] += p[i] as f64;
            max[i] = max[i].max(p[i]);
        }
        luma += 0.2126 * p[0] as f64 + 0.7152 * p[1] as f64 + 0.0722 * p[2] as f64;
    }
    format!(
        "mean rgb {:.1}/{:.1}/{:.1}, max {}/{}/{}, mean luma {:.1}",
        sum[0] / n,
        sum[1] / n,
        sum[2] / n,
        max[0],
        max[1],
        max[2],
        luma / n
    )
}

/// Compare a consumer frame with the reference bars (same size: every pixel; scaled: away from the edges).
fn compare_bars(frame: &ConsumerFrame, source: (u32, u32)) -> String {
    let (w, h) = (frame.width, frame.height);
    let reference_src = pattern::smpte_bars(source.0, source.1);
    // The engine fits the source into the output: same aspect -> a plain scale; compare by nearest source pixel.
    let (xs, ys) = pattern::bar_edges(source.0, source.1);
    let scale = (source.0 as f64 / w as f64, source.1 as f64 / h as f64);
    let mut want = vec![0u8; (w * h * 3) as usize];
    for y in 0..h {
        for x in 0..w {
            let (sx, sy) = (
                (((x as f64 + 0.5) * scale.0) as u32).min(source.0 - 1),
                (((y as f64 + 0.5) * scale.1) as u32).min(source.1 - 1),
            );
            let o = ((sy * source.0 + sx) * 3) as usize;
            want[((y * w + x) * 3) as usize..][..3].copy_from_slice(&reference_src[o..o + 3]);
        }
    }
    let margin = if (w, h) == source { 0.0 } else { 3.0 };
    let near = |i: usize| {
        if margin == 0.0 {
            return false;
        }
        let (x, y) = (
            (i as u32 % w) as f64 * scale.0,
            (i as u32 / w) as f64 * scale.1,
        );
        xs.iter()
            .any(|e| (*e as f64 - x).abs() <= margin * scale.0.max(1.0) + 1.0)
            || ys
                .iter()
                .any(|e| (*e as f64 - y).abs() <= margin * scale.1.max(1.0) + 1.0)
    };
    let worst = pattern::max_diff(&frame.rgb, &want, 3, near);
    let skipped = if margin == 0.0 {
        0
    } else {
        (0..(w * h) as usize).filter(|&i| near(i)).count()
    };
    format!(
        "bars check: max error {worst} code values over {} pixels ({} edge pixels ignored) -> {}",
        w * h,
        skipped,
        if worst <= 3 {
            "PASS (within +/-3)"
        } else {
            "FAIL (more than +/-3)"
        }
    )
}

/// `--probe-cpu`: open the camera without the engine, take CPU frames and find where the chroma plane of a planar
/// frame starts. A dark scene makes this easy: luma sits near 16 and chroma near 128, so the first row whose bytes
/// jump up is the first chroma row. Prints the buffer's stride and length next to it.
fn probe_cpu(a: &Args) -> Result<(), String> {
    use better_cam::capture::reader::{CaptureReader, FrameData, OpenOptions};
    let cameras = capture::list_cameras().map_err(|e| e.to_string())?;
    let cam = cameras.get(a.device).ok_or("no such camera")?;
    let mut opts = OpenOptions::new(
        &cam.key.symbolic_link,
        FormatRequest {
            format: a.format,
            size: a.size,
            fps: a.fps.map(|f| Ratio::new(f, 1)),
        },
    );
    opts.hw_decode = false;
    opts.gpu = None;
    let reader = CaptureReader::open(opts).map_err(|e| e.to_string())?;
    let i = reader.info();
    println!(
        "probe: native {}x{} {}, delivered {}x{} {} ({} decode)",
        i.native.width,
        i.native.height,
        a.format,
        i.width,
        i.height,
        i.output_format.map(|f| f.name()).unwrap_or("?"),
        i.decode
    );
    for n in 0..5 {
        let f = reader
            .recv_timeout(Duration::from_secs(5))
            .map_err(|e| format!("{e:?}"))?;
        let FrameData::Cpu(cpu) = &f.data else {
            return Err("got a texture frame".into());
        };
        let (data, stride) = (cpu.data(), cpu.stride().unsigned_abs() as usize);
        let rows = data.len() / stride.max(1);
        let row_mean = |r: usize| {
            let row = &data[r * stride..(r * stride + f.width as usize).min(data.len())];
            row.iter().map(|&v| v as f64).sum::<f64>() / row.len().max(1) as f64
        };
        // first row where the mean jumps by more than 40 over the row above (luma -> chroma)
        let jump = (1..rows).find(|&r| row_mean(r) - row_mean(r - 1) > 40.0);
        println!(
            "probe frame {n}: {}x{} stride {} (negative: {}) data.len {} = {} rows of stride (1.5 x height = {}), chroma starts at row {:?}",
            f.width, f.height, stride, cpu.stride() < 0, data.len(), rows, f.height as usize * 3 / 2, jump
        );
    }
    Ok(())
}

/// `--consume N` with N above the 21 frames a plain `--dump` reads.
fn a_consume_requested(take: u32) -> bool {
    take != 21
}

// ---------------------------------------------------------------------------------------------------------------
// Main

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let a = parse_args()?;
    capture::ensure_com();
    if a.probe_cpu {
        return probe_cpu(&a);
    }
    // SAFETY: registers a handler that only stores an atomic.
    unsafe {
        let _ = SetConsoleCtrlHandler(Some(on_ctrl), true);
    }

    // ---- source
    let (source, out_default) = if a.synthetic {
        let size = a.synth_size.or(a.output).unwrap_or((1920, 1080));
        (
            SourceSpec::Synthetic(SyntheticSpec {
                width: size.0,
                height: size.1,
                fps: a.fps.unwrap_or(30),
                format: a.synth_format,
                color: ColorSettings {
                    space: Some(a.color.space.unwrap_or(ColorSpace::Bt709)),
                    range: Some(a.color.range.unwrap_or(ColorRange::Limited)),
                },
            }),
            size,
        )
    } else {
        let cameras = capture::list_cameras().map_err(|e| e.to_string())?;
        let cam = cameras.get(a.device).ok_or_else(|| {
            format!(
                "no camera at index {}; found: {}",
                a.device,
                cameras
                    .iter()
                    .map(|c| c.key.name.clone())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
        println!(
            "camera {}: {} ({})",
            a.device, cam.key.name, cam.key.symbolic_link
        );
        (
            SourceSpec::Camera(better_cam::engine::CameraSpec {
                link: cam.key.symbolic_link.clone(),
                request: FormatRequest {
                    format: a.format,
                    size: a.size,
                    fps: a.fps.map(|f| Ratio::new(f, 1)),
                },
                hw_decode: a.hw_decode,
            }),
            a.size.unwrap_or((1920, 1080)),
        )
    };

    let settings = Settings {
        source,
        output: OutputSettings {
            width: a.output.map(|s| s.0),
            height: a.output.map(|s| s.1),
            fps: a.out_fps,
            fit: a.fit,
            background: [0, 0, 0],
            extra_sizes: true,
        },
        flip_vertical: a.flip,
        color: if a.synthetic {
            ColorSettings::default()
        } else {
            a.color
        },
        readback: a.readback,
        deactivate_when_not_showing: false,
    };

    // ---- start (the camera may be held by another process for a moment)
    let engine = Engine::new()?;
    let mut attempt = 0;
    loop {
        engine.start(settings.clone());
        match engine.wait_live(Duration::from_secs(20)) {
            Ok(()) => break,
            Err(e) => {
                attempt += 1;
                let busy = e.contains("C00D3704") || e.to_lowercase().contains("in use");
                if busy && attempt < 10 && !STOP.load(Ordering::Acquire) {
                    eprintln!("camera busy ({e}); retrying in 2 s ({attempt}/10)");
                    engine.wait_stopped(Duration::from_secs(5));
                    std::thread::sleep(Duration::from_secs(2));
                    continue;
                }
                return Err(e);
            }
        }
    }
    println!("source: {}", engine.stats().source);
    let out = a.output.unwrap_or(out_default);
    println!(
        "output: {}x{} (consumers may ask for other sizes), advertised first: {}",
        out.0,
        out.1,
        {
            let h = engine.writer().header();
            let f = h.advertised[0];
            format!(
                "{}x{}@{} {}",
                f.width,
                f.height,
                f.fps(),
                if f.subtype == SUBTYPE_NV12 {
                    "nv12"
                } else {
                    "rgb32"
                }
            )
        }
    );

    // ---- the consumer for --dump runs next to the stats loop
    let dump_job = (a.dump.is_some() || a.consume.is_some()).then(|| {
        let path = a.dump.clone();
        let nv12 = a.consumer_nv12;
        let size = a.consumer_size.unwrap_or(out);
        let synth = a.synthetic.then(|| a.synth_size.or(a.output).unwrap_or((1920, 1080)));
        let gain = a.dump_gain;
        let take = a.consume.unwrap_or(21);
        std::thread::spawn(move || -> Result<String, String> {
            // let the engine settle, then ask for frames
            std::thread::sleep(Duration::from_millis(1500));
            let (f, s) = consumer_capture(nv12, size, 20, take)?;
            let mut report = format!(
                "consumer: {} {}x{}: picture taken at frame {} (pitch {}, contiguous length {}): {}",
                if nv12 { "nv12" } else { "rgb32" },
                f.width, f.height, f.frames_seen, f.pitch, f.contiguous, rgb_stats(&f.rgb)
            );
            if a_consume_requested(take) && nv12 {
                report.push_str(&format!(
                    "\nconsumer: {} frames in {:.2} s = {:.1} fps wall, {:.1} fps by sample time, max gap {:.0} ms, {} distinct frames ({:.1}/s), sampled luma min/median/max {:.1}/{:.1}/{:.1}, {} frames far from the median",
                    s.frames, s.wall_secs, (s.frames - 1) as f64 / s.wall_secs.max(1e-9), s.stamp_fps, s.max_gap_ms,
                    s.distinct, s.distinct as f64 / s.wall_secs.max(1e-9), s.luma.0, s.luma.1, s.luma.2, s.odd
                ));
                if let (Some(odd), Some(stop)) = (s.first_odd_at, ENGINE_STOPPED_AT.get()) {
                    let ms = if odd >= *stop {
                        odd.duration_since(*stop).as_secs_f64() * 1000.0
                    } else {
                        -(stop.duration_since(odd).as_secs_f64() * 1000.0)
                    };
                    report.push_str(&format!(
                        "\nconsumer: the first frame unlike the live picture (the slate) arrived {ms:.0} ms after the engine stopped"
                    ));
                }
            }
            let path = path.unwrap_or_default();
            if !path.is_empty() {
                save_picture(&path, &f.rgb, f.width, f.height).map_err(|e| format!("{path}: {e}"))?;
                report.push_str(&format!("\ndump: wrote {path}"));
            }
            if gain > 1 && !path.is_empty() {
                let p = std::path::Path::new(&path);
                let boosted = p.with_file_name(format!(
                    "{}_x{gain}.{}",
                    p.file_stem().and_then(|s| s.to_str()).unwrap_or("dump"),
                    p.extension().and_then(|s| s.to_str()).unwrap_or("png")
                ));
                save_picture(&boosted.to_string_lossy(), &brightened(&f.rgb, gain), f.width, f.height)
                    .map_err(|e| e.to_string())?;
                report.push_str(&format!("\ndump: wrote {}", boosted.display()));
            }
            if let Some(src) = synth {
                report.push('\n');
                report.push_str(&compare_bars(&f, src));
            }
            Ok(report)
        })
    });

    // ---- stats loop
    let started = Instant::now();
    let mut next_print = started + Duration::from_secs(1);
    let mut samples: Vec<[f64; 5]> = Vec::new(); // capture fps, render fps, gpu_ms, readback_ms, cpu_ms
    while started.elapsed() < Duration::from_secs(a.seconds) && !STOP.load(Ordering::Acquire) {
        std::thread::sleep(Duration::from_millis(50));
        if engine.state() != better_cam::engine::EngineState::Live {
            eprintln!(
                "engine stopped: {}",
                engine.last_error().unwrap_or_default()
            );
            break;
        }
        if Instant::now() < next_print {
            continue;
        }
        next_print += Duration::from_secs(1);
        let s = engine.stats();
        let h = engine.writer().header();
        println!(
            "t={:>3}s cap {:>5.1} render {:>5.1} pub {:>5.1} fps | gpu {:.2} ms readback {:.2} ms cpu {:.2} ms | latency p50/p95/max {:.1}/{:.1}/{:.1} ms, pipeline p95 {:.1} ms | dropped mailbox {} output {} errors {} | out {}x{} {} | dll: {} delivered {} repeated {} p50 {} us",
            started.elapsed().as_secs(),
            s.capture_fps, s.render_fps, s.publish_fps,
            s.gpu_ms, s.readback_ms, s.cpu_ms,
            s.latency_p50_ms, s.latency_p95_ms, s.latency_max_ms, s.pipeline_p95_ms,
            s.dropped_mailbox, s.dropped_output, s.errors,
            s.output_width, s.output_height, s.output_format,
            if engine.writer().consumer_active() {
                format!("{}x{} {}", h.negotiated.width, h.negotiated.height,
                    if h.negotiated.subtype == SUBTYPE_RGB32 { "rgb32" } else { "nv12" })
            } else { "no consumer".into() },
            h.delivered, h.repeated, h.latency_p50_us,
        );
        if started.elapsed() > Duration::from_secs(2) {
            samples.push([
                s.capture_fps,
                s.render_fps,
                s.gpu_ms,
                s.readback_ms,
                s.cpu_ms,
            ]);
        }
    }

    // ---- results
    let last = engine.stats();
    let h = engine.writer().header();
    let mean = |i: usize| samples.iter().map(|s| s[i]).sum::<f64>() / samples.len().max(1) as f64;
    let max = |i: usize| samples.iter().map(|s| s[i]).fold(0.0, f64::max);
    println!(
        "summary: capture {:.1} fps, render {:.1} fps | gpu_ms mean {:.2} max {:.2} | readback mean {:.2} ms | cpu mean {:.2} ms | latency p95 over the run {:.1} ms (pipeline only {:.1} ms) | published {} rendered {} captured {} | dropped mailbox {} output {} | dll delivered {} repeated {} ({:.1}%)",
        mean(0), mean(1), mean(2), max(2), mean(3), mean(4),
        last.total_latency_p95_ms, last.total_pipeline_p95_ms,
        last.published, last.rendered, last.captured,
        last.dropped_mailbox, last.dropped_output,
        h.delivered, h.repeated,
        if h.delivered > 0 { h.repeated as f64 * 100.0 / h.delivered as f64 } else { 0.0 },
    );
    // ---- stop: the camera is released and the file is left closed
    let writer = Arc::clone(engine.writer());
    engine.stop();
    let stopped = engine.wait_stopped(Duration::from_secs(10));
    let _ = ENGINE_STOPPED_AT.set(Instant::now());
    let state = WriterState::from_u32(writer.header().writer_state)
        .map(|s| s.name())
        .unwrap_or("?");
    println!("stopped: {stopped}, writer_state: {state}");
    // A consumer that is still reading (--consume longer than --seconds) now sees the slate.
    if let Some(job) = dump_job {
        match job.join() {
            Ok(Ok(report)) => println!("{report}"),
            Ok(Err(e)) => println!("dump failed: {e}"),
            Err(_) => println!("dump thread panicked"),
        }
    }
    engine.shutdown();
    Ok(())
}
