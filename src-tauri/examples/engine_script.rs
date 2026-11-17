//! Scripted engine run: drives the engine through `better_cam::api::Service`, the functions behind the Tauri
//! commands, from a JSON script whose arguments are written the way the frontend sends them.
//!
//! ```text
//! cargo run -p better-cam --release --example engine_script -- --script examples/scripts/live_swap.json
//! ```
//!
//! A script is `{"steps": [ ... ]}`; the steps run one after the other. Every step has a `do`:
//!
//! * `start` `{source, output, controls?, chain?}`, `stop`, `set_source` `{source}`, `set_output` `{output}`,
//!   `pause` `{on}`, `window` `{visible}`: the commands (`window` is what `main.rs` sends from window events).
//! * `list_cameras`: prints the cameras and their formats as the frontend gets them.
//! * `sleep` `{seconds}`, `print` `{text}`, `header`: print the writer state and counters of `frames.bin`.
//! * `wait_state` `{state, reason_contains?, timeout, max?, label}`: wait for an `engine-state` event after the
//!   last command and print how long it took (`FAIL` past `max`, or when it never comes).
//! * `stats_report` `{label}`: count the `stats` events since the last report and print the rate and the last one.
//! * `consume` `{seconds, size, format?, label, dump?, background?}`: open the Better Cam camera as an app does,
//!   read frames, and say how many were video, which slate showed when, and how many pictures were distinct.
//!   `dump` saves one video frame as PNG.
//! * `run` `{cmd, args, label, wait?}`: start another process (`capture_probe`, `vcam-tool`), print its output.
//! * `join`: wait for background consumers and processes.
//!
//! Engine events are printed as they arrive with the time since the start of the script.

use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use better_cam::api::Service;
use better_cam::capture::{self, MfSession};
use better_cam::config::{OutputCfg, SourceCfg};
use better_cam::engine::{EngineEvent, EventSink};
use better_cam::gpu::convert::{ColorRange, ColorSpace};
use better_cam::gpu::pattern;
use serde_json::Value;
use vcam_protocol::slate::{self, Slate, SlateKind};
use vcam_protocol::{frames_path, Access, MappedFile, WriterState};
use windows::core::Interface;
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer, IMFActivate, IMFMediaSource, IMFSample, MFCreateAttributes,
    MFCreateSourceReaderFromMediaSource, MFEnumDeviceSources, MFMediaType_Video,
    MFVideoFormat_NV12, MFVideoFormat_RGB32, MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
    MF_E_NO_MORE_TYPES, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE,
    MF_READWRITE_DISABLE_CONVERTERS, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
};
use windows::Win32::System::Com::CoTaskMemFree;

// ---------------------------------------------------------------------------------------------------------------
// Event log

struct Logged {
    at: Instant,
    event: EngineEvent,
}

struct Log {
    t0: Instant,
    events: Mutex<Vec<Logged>>,
}

impl Log {
    fn t(&self) -> f64 {
        self.t0.elapsed().as_secs_f64()
    }
}

struct LogSink(Arc<Log>);

impl EventSink for LogSink {
    fn emit(&self, event: EngineEvent) {
        let t = self.0.t();
        match &event {
            EngineEvent::State(s) => println!(
                "[t={t:7.3}] engine-state {}",
                serde_json::to_string(s).unwrap_or_default()
            ),
            EngineEvent::Error(m) => println!("[t={t:7.3}] engine-error {m:?}"),
            EngineEvent::DevicesChanged => println!("[t={t:7.3}] devices-changed"),
            EngineEvent::Stats(_) | EngineEvent::Controls(_) => {}
        }
        self.0.events.lock().unwrap().push(Logged {
            at: Instant::now(),
            event,
        });
    }
}

/// Seconds from `since` until an `engine-state` event with this state (and a reason containing the text).
fn wait_state(
    log: &Log,
    since: Instant,
    state: &str,
    reason: Option<&str>,
    timeout: Duration,
) -> Option<Duration> {
    let deadline = Instant::now() + timeout;
    loop {
        let found = log
            .events
            .lock()
            .unwrap()
            .iter()
            .find_map(|l| match &l.event {
                EngineEvent::State(s)
                    if l.at >= since
                        && s.state.name() == state
                        && reason
                            .is_none_or(|r| s.reason.as_deref().is_some_and(|x| x.contains(r))) =>
                {
                    Some(l.at.duration_since(since))
                }
                _ => None,
            });
        if found.is_some() {
            return found;
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

// ---------------------------------------------------------------------------------------------------------------
// PNG (stored deflate blocks: big, but needs no compressor)

fn crc32(data: &[u8], table: &[u32; 256], mut crc: u32) -> u32 {
    crc = !crc;
    for &b in data {
        crc = table[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    !crc
}

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

// ---------------------------------------------------------------------------------------------------------------
// The consumer: Better Cam opened through Media Foundation, like any app does

static SLATES: [&[u8]; 3] = [
    include_bytes!("../../crates/vcam-source/assets/slates/not_running.rle"),
    include_bytes!("../../crates/vcam-source/assets/slates/paused.rle"),
    include_bytes!("../../crates/vcam-source/assets/slates/starting.rle"),
];

fn slate_of(kind: SlateKind) -> &'static Slate {
    static DECODED: [OnceLock<Slate>; 3] = [const { OnceLock::new() }; 3];
    let i = SlateKind::ALL.iter().position(|k| *k == kind).unwrap();
    DECODED[i].get_or_init(|| slate::decode(SLATES[i]).expect("embedded slate"))
}

/// What a frame is: video or one of the slates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Class {
    Video,
    NotRunning,
    Paused,
    Starting,
}

impl Class {
    fn name(self) -> &'static str {
        match self {
            Class::Video => "video",
            Class::NotRunning => "slate:not-running",
            Class::Paused => "slate:paused",
            Class::Starting => "slate:starting",
        }
    }
}

fn better_cam_device() -> Result<IMFActivate, String> {
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
            found = Some(a);
        }
    }
    if !list.is_null() {
        unsafe { CoTaskMemFree(Some(list as *const _)) };
    }
    found.ok_or_else(|| "the Better Cam device is not listed (vcam-tool status)".to_string())
}

fn parse_size(v: &str) -> Result<(u32, u32), String> {
    better_cam::config::parse_size(v)
}

struct Seen {
    t: f64,
    class: Class,
    hash: u64,
}

/// Read the virtual camera for `seconds` and describe what came.
fn consume(
    label: &str,
    size: (u32, u32),
    nv12: bool,
    seconds: f64,
    dump: Option<String>,
    gain: u32,
    t0: Instant,
) -> Result<String, String> {
    capture::ensure_com();
    let _mf = MfSession::start().map_err(|e| e.to_string())?;
    let activate = better_cam_device()?;
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
    let mut offered = Vec::new();
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
        let this = ((sz >> 32) as u32, sz as u32);
        offered.push(format!(
            "{}x{}{}",
            this.0,
            this.1,
            if sub == MFVideoFormat_NV12 { "n" } else { "r" }
        ));
        if major == MFMediaType_Video && sub == want && this == size {
            unsafe { reader.SetCurrentMediaType(stream, None, &t) }.map_err(|e| e.to_string())?;
            chosen = true;
            break;
        }
    }
    if !chosen {
        unsafe {
            let _ = source.Shutdown();
        }
        return Err(format!(
            "Better Cam does not offer {}x{} that way (offers: {})",
            size.0,
            size.1,
            offered.join(" ")
        ));
    }
    let (w, h) = size;
    // The slates scaled the way the DLL scales them.
    let slates: Vec<(Class, Vec<u8>)> = if nv12 {
        vec![
            (
                Class::NotRunning,
                slate_of(SlateKind::NotRunning).scale_nv12(w, h),
            ),
            (Class::Paused, slate_of(SlateKind::Paused).scale_nv12(w, h)),
            (
                Class::Starting,
                slate_of(SlateKind::Starting).scale_nv12(w, h),
            ),
        ]
    } else {
        Vec::new()
    };

    let started = Instant::now();
    let deadline = started + Duration::from_secs_f64(seconds);
    let mut seen: Vec<Seen> = Vec::new();
    let mut picture: Option<Vec<u8>> = None;
    let mut first_at: Option<f64> = None;
    while Instant::now() < deadline {
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
        let t = t0.elapsed().as_secs_f64();
        first_at.get_or_insert(started.elapsed().as_secs_f64());
        let buffer = unsafe { sample.ConvertToContiguousBuffer() }.map_err(|e| e.to_string())?;
        let (mut scan0, mut pitch) = (std::ptr::null_mut(), 0i32);
        let b2 = buffer.cast::<IMF2DBuffer>().ok();
        let locked1d;
        if let Some(b) = &b2 {
            unsafe { b.Lock2D(&mut scan0, &mut pitch) }.map_err(|e| e.to_string())?;
            locked1d = false;
        } else {
            let mut cur = 0u32;
            unsafe { buffer.Lock(&mut scan0, None, Some(&mut cur)) }.map_err(|e| e.to_string())?;
            pitch = if nv12 { w as i32 } else { w as i32 * 4 };
            locked1d = true;
        }
        let row_bytes = if nv12 { w as usize } else { w as usize * 4 };
        let row = |y: u32| -> &[u8] {
            // SAFETY: rows below the frame height of a locked buffer.
            unsafe {
                std::slice::from_raw_parts(scan0.offset(y as isize * pitch as isize), row_bytes)
            }
        };
        // FNV-1a of a sparse sample: equal for repeated frames, different for new ones.
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        for y in (0..h).step_by(7) {
            let r = row(y);
            for x in (0..row_bytes).step_by(if nv12 { 5 } else { 20 }) {
                hash = (hash ^ r[x] as u64).wrapping_mul(0x100_0000_01b3);
            }
        }
        // The slates differ only in their text line: the closest one decides, and it must be close.
        let mut class = Class::Video;
        let mut best = 2.0f64;
        for (c, expected) in &slates {
            let diff = slate::mean_luma_diff(
                |y| expected[y as usize * w as usize..(y as usize + 1) * w as usize].to_vec(),
                |y| row(y).to_vec(),
                w,
                h,
                false,
                1,
            );
            if diff <= best {
                best = diff;
                class = *c;
            }
        }
        // Keep the last video frame: after the first second, so a freshly opened camera has settled.
        if class == Class::Video && started.elapsed().as_secs_f64() > 1.0 {
            let rows = if nv12 { h * 3 / 2 } else { h };
            let mut copy = Vec::with_capacity(rows as usize * row_bytes);
            for y in 0..rows {
                copy.extend_from_slice(row(y));
            }
            picture = Some(copy);
        }
        unsafe {
            if locked1d {
                let _ = buffer.Unlock();
            } else if let Some(b) = &b2 {
                let _ = b.Unlock2D();
            }
        }
        seen.push(Seen { t, class, hash });
    }
    unsafe {
        let _ = source.Shutdown();
        let _ = activate.ShutdownObject();
    }

    // ---- report
    let n = seen.len();
    let count = |c: Class| seen.iter().filter(|s| s.class == c).count();
    let mut distinct = 0;
    let mut prev = None;
    for s in seen.iter().filter(|s| s.class == Class::Video) {
        if prev != Some(s.hash) {
            distinct += 1;
        }
        prev = Some(s.hash);
    }
    let max_gap = seen
        .windows(2)
        .map(|p| p[1].t - p[0].t)
        .fold(0.0f64, f64::max);
    let mut report = format!(
        "consumer '{label}' ({w}x{h} {}): {n} frames in {seconds:.1} s, first after {:.2} s, max gap {:.0} ms | video {} (distinct pictures {distinct}), paused slate {}, starting slate {}, not-running slate {}",
        if nv12 { "nv12" } else { "rgb32" },
        first_at.unwrap_or(f64::NAN),
        max_gap * 1000.0,
        count(Class::Video),
        count(Class::Paused),
        count(Class::Starting),
        count(Class::NotRunning),
    );
    // class timeline
    let mut timeline = Vec::new();
    let mut last: Option<Class> = None;
    for s in &seen {
        if last != Some(s.class) {
            timeline.push(format!("t={:.2} {}", s.t, s.class.name()));
            last = Some(s.class);
        }
    }
    report.push_str(&format!("\n  timeline: {}", timeline.join(" -> ")));
    if let (Some(data), Some(path)) = (picture, dump) {
        if nv12 {
            let mut rgb = pattern::nv12_to_rgb(
                &data,
                w,
                h,
                w as usize,
                ColorSpace::Bt709,
                ColorRange::Limited,
            );
            if gain > 1 {
                // A dark room: brighten the picture so it can be looked at.
                for v in rgb.iter_mut() {
                    *v = (*v as u32 * gain).min(255) as u8;
                }
            }
            std::fs::write(&path, encode_png(&rgb, w, h)).map_err(|e| format!("{path}: {e}"))?;
            report.push_str(&format!("\n  wrote {path} ({w}x{h}, gain {gain})"));
        }
    }
    Ok(report)
}

// ---------------------------------------------------------------------------------------------------------------
// Script

fn field<'a>(step: &'a Value, key: &str) -> Result<&'a Value, String> {
    step.get(key)
        .ok_or_else(|| format!("step {} needs '{key}'", step["do"]))
}

fn secs(step: &Value, key: &str, default: f64) -> f64 {
    step.get(key).and_then(Value::as_f64).unwrap_or(default)
}

struct Background {
    label: String,
    kind: &'static str,
    consumer: Option<std::thread::JoinHandle<Result<String, String>>>,
    child: Option<Child>,
}

fn finish(b: Background) {
    if let Some(h) = b.consumer {
        match h.join() {
            Ok(Ok(r)) => println!("{r}"),
            Ok(Err(e)) => println!("{} '{}' failed: {e}", b.kind, b.label),
            Err(_) => println!("{} '{}' panicked", b.kind, b.label),
        }
    }
    if let Some(c) = b.child {
        match c.wait_with_output() {
            Ok(o) => println!(
                "process '{}' exited {}:\n{}{}",
                b.label,
                o.status.code().map_or("?".into(), |c| c.to_string()),
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            ),
            Err(e) => println!("process '{}' failed: {e}", b.label),
        }
    }
}

fn run() -> Result<(), String> {
    let mut path = None;
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--script" => path = it.next().cloned(),
            other => return Err(format!("unknown argument '{other}' (use --script FILE)")),
        }
    }
    let path = path.ok_or("usage: engine_script --script FILE")?;
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{path}: {e}"))?;
    let script: Value = serde_json::from_str(&text).map_err(|e| format!("{path}: {e}"))?;
    let steps = script["steps"]
        .as_array()
        .ok_or("the script needs a 'steps' array")?;

    capture::ensure_com();
    let t0 = Instant::now();
    let log = Arc::new(Log {
        t0,
        events: Mutex::new(Vec::new()),
    });
    let svc = Service::new();
    svc.set_sink(Arc::new(LogSink(log.clone())));
    svc.init()?;
    println!("[t={:7.3}] engine ready", log.t());

    let mut last_command = Instant::now();
    let mut last_report = Instant::now();
    let mut background: Vec<Background> = Vec::new();
    let mut failed = 0;

    for (i, step) in steps.iter().enumerate() {
        let what = step["do"].as_str().ok_or("every step needs a 'do'")?;
        println!("[t={:7.3}] step {i}: {what}", log.t());
        match what {
            "start" => {
                let source: SourceCfg = serde_json::from_value(field(step, "source")?.clone())
                    .map_err(|e| {
                        format!("invalid args `source` for command `start_engine`: {e}")
                    })?;
                let output: OutputCfg = serde_json::from_value(field(step, "output")?.clone())
                    .map_err(|e| {
                        format!("invalid args `output` for command `start_engine`: {e}")
                    })?;
                last_command = Instant::now();
                svc.start_engine(
                    source,
                    output,
                    step.get("controls").cloned(),
                    step.get("chain").cloned(),
                )?;
            }
            "stop" => {
                last_command = Instant::now();
                svc.stop_engine()?;
            }
            "set_source" => {
                let source: SourceCfg = serde_json::from_value(field(step, "source")?.clone())
                    .map_err(|e| format!("invalid args `source` for command `set_source`: {e}"))?;
                last_command = Instant::now();
                svc.set_source(source)?;
            }
            "set_output" => {
                let output: OutputCfg = serde_json::from_value(field(step, "output")?.clone())
                    .map_err(|e| format!("invalid args `output` for command `set_output`: {e}"))?;
                last_command = Instant::now();
                svc.set_output(output)?;
            }
            "pause" => {
                last_command = Instant::now();
                svc.set_pause(
                    field(step, "on")?
                        .as_bool()
                        .ok_or("'on' must be a boolean")?,
                )?;
            }
            "window" => {
                last_command = Instant::now();
                svc.set_window_visible(
                    field(step, "visible")?
                        .as_bool()
                        .ok_or("'visible' must be a boolean")?,
                );
            }
            "list_cameras" => {
                let t = Instant::now();
                let cams = svc.list_cameras()?;
                println!(
                    "list_cameras took {:.0} ms: {} camera(s)",
                    t.elapsed().as_secs_f64() * 1000.0,
                    cams.len()
                );
                for c in &cams {
                    println!("  {} ({})", c.name, c.id);
                    for f in &c.formats {
                        let fps: Vec<String> = f
                            .intervals
                            .iter()
                            .map(|i| format!("{:.2}", 1e7 / *i as f64))
                            .collect();
                        println!(
                            "    {} {}x{} @ {} fps (intervals {:?})",
                            f.format,
                            f.width,
                            f.height,
                            fps.join("/"),
                            f.intervals
                        );
                    }
                }
            }
            "sleep" => std::thread::sleep(Duration::from_secs_f64(secs(step, "seconds", 1.0))),
            "print" => println!("{}", step["text"].as_str().unwrap_or("")),
            "header" => {
                let m = MappedFile::open(&frames_path(), Access::ReadOnly)
                    .map_err(|e| e.to_string())?;
                let h = m.shared().header();
                println!(
                    "frames.bin: writer_state {} consumer_active {} delivered {} repeated {} negotiated {}x{} advertised {:?}",
                    WriterState::from_u32(h.writer_state).map_or("?", |s| s.name()),
                    h.consumer_active,
                    h.delivered,
                    h.repeated,
                    h.negotiated.width,
                    h.negotiated.height,
                    h.advertised[..h.advertised_count as usize]
                        .iter()
                        .map(|f| format!("{}x{}@{}", f.width, f.height, f.fps()))
                        .collect::<Vec<_>>()
                );
            }
            "wait_state" => {
                let state = field(step, "state")?
                    .as_str()
                    .ok_or("'state' must be a string")?;
                let label = step["label"].as_str().unwrap_or(state);
                let timeout = secs(step, "timeout", 10.0);
                let got = wait_state(
                    &log,
                    last_command,
                    state,
                    step["reason_contains"].as_str(),
                    Duration::from_secs_f64(timeout),
                );
                match got {
                    Some(d) => {
                        let max = step.get("max").and_then(Value::as_f64);
                        let ok = max.is_none_or(|m| d.as_secs_f64() <= m);
                        if !ok {
                            failed += 1;
                        }
                        println!(
                            "RESULT {label}: '{state}' {:.3} s after the command{} -> {}",
                            d.as_secs_f64(),
                            max.map_or(String::new(), |m| format!(" (limit {m} s)")),
                            if ok { "PASS" } else { "FAIL" }
                        );
                    }
                    None => {
                        failed += 1;
                        println!("RESULT {label}: '{state}' never came within {timeout} s -> FAIL");
                    }
                }
            }
            "stats_report" => {
                let label = step["label"].as_str().unwrap_or("stats");
                // The events of the last `seconds` (default 2), so a long quiet stretch before does not dilute the rate.
                let from = Instant::now()
                    .checked_sub(Duration::from_secs_f64(secs(step, "seconds", 2.0)))
                    .unwrap_or(last_report);
                let events = log.events.lock().unwrap();
                let stats: Vec<_> = events
                    .iter()
                    .filter(|l| l.at >= from)
                    .filter_map(|l| match &l.event {
                        EngineEvent::Stats(s) => Some((l.at, s.clone())),
                        _ => None,
                    })
                    .collect();
                // The rate between the first and the last event of the window (the window itself may start before
                // the first one, e.g. while the camera opens).
                let span = match (stats.first(), stats.last()) {
                    (Some((a, _)), Some((b, _))) => b.duration_since(*a).as_secs_f64(),
                    _ => 0.0,
                };
                println!(
                    "RESULT {label}: {} stats events in the last {:.1} s, {:.2} Hz between the first and the last",
                    stats.len(),
                    from.elapsed().as_secs_f64(),
                    if span > 0.0 { (stats.len() - 1) as f64 / span } else { 0.0 }
                );
                if let Some((_, s)) = stats.last() {
                    println!("  last: {}", serde_json::to_string(s).unwrap_or_default());
                }
                last_report = Instant::now();
            }
            "consume" => {
                let seconds = secs(step, "seconds", 3.0);
                let size = parse_size(field(step, "size")?.as_str().ok_or("'size' must be WxH")?)?;
                let nv12 = step["format"].as_str().unwrap_or("nv12") != "rgb32";
                let label = step["label"].as_str().unwrap_or("consumer").to_string();
                let dump = step["dump"].as_str().map(str::to_string);
                let gain = step["gain"].as_u64().unwrap_or(1) as u32;
                let l = label.clone();
                // `wait_state` after this step times from here: how long until the engine reacts to the consumer.
                last_command = Instant::now();
                let handle =
                    std::thread::spawn(move || consume(&l, size, nv12, seconds, dump, gain, t0));
                let job = Background {
                    label,
                    kind: "consumer",
                    consumer: Some(handle),
                    child: None,
                };
                if step["background"].as_bool().unwrap_or(false) {
                    background.push(job);
                } else {
                    finish(job);
                }
            }
            "run" => {
                let cmd = field(step, "cmd")?
                    .as_str()
                    .ok_or("'cmd' must be a string")?;
                let args: Vec<&str> = step["args"]
                    .as_array()
                    .map_or(vec![], |a| a.iter().filter_map(Value::as_str).collect());
                let label = step["label"].as_str().unwrap_or(cmd).to_string();
                let child = Command::new(cmd)
                    .args(&args)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .map_err(|e| format!("{cmd}: {e}"))?;
                let job = Background {
                    label,
                    kind: "process",
                    consumer: None,
                    child: Some(child),
                };
                if step["wait"].as_bool().unwrap_or(true) {
                    finish(job);
                } else {
                    background.push(job);
                }
            }
            "join" => {
                for b in background.drain(..) {
                    finish(b);
                }
            }
            "wait_process" => {
                // Wait for a background process and time the next `wait_state` from the moment it exited.
                let label = field(step, "label")?
                    .as_str()
                    .ok_or("'label' must be a string")?;
                let at = background
                    .iter()
                    .position(|b| b.label == label && b.child.is_some());
                let b = background
                    .remove(at.ok_or_else(|| format!("no background process '{label}'"))?);
                finish(b);
                last_command = Instant::now();
            }
            other => return Err(format!("unknown step '{other}'")),
        }
    }
    for b in background.drain(..) {
        finish(b);
    }

    // Quit: the engine shuts down, the camera is released and the file is closed.
    svc.shutdown();
    let m = MappedFile::open(&frames_path(), Access::ReadOnly).map_err(|e| e.to_string())?;
    println!(
        "[t={:7.3}] shut down; writer_state {}",
        log.t(),
        WriterState::from_u32(m.shared().header().writer_state).map_or("?", |s| s.name())
    );
    if failed > 0 {
        return Err(format!("{failed} check(s) failed"));
    }
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
