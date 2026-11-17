//! Command-line probe for the capture backend (plan phase P3).
//!
//! ```text
//! capture_probe list
//! capture_probe open  [--device N|NAME] [--format mjpeg|nv12|yuy2|...] [--size WxH] [--fps N]
//!                     [--seconds N] [--hw-decode on|off] [--gpu hw|warp]
//! capture_probe watch [--seconds N]
//! capture_probe controls [--device N|NAME] [--path auto|ks|iam|compare] [--save FILE]
//! capture_probe set      [--device N|NAME] --control KEY [--value N|min|max|mid|default] [--auto true|false]
//!                        [--measure] [--settle-ms N] [--reapply-test] [--path auto|ks|iam]
//! capture_probe restore  [--device N|NAME] --file FILE
//! ```
//!
//! `controls`, `set` and `restore` (plan phase P4) also take `--format`, `--size` and `--fps` like `open`.
//! `controls --save FILE` writes the full device state as a snapshot; `restore --file FILE` applies it again.
//!
//! Exit codes: 0 success (also when the device is lost during `open`), 1 stream error or a control the camera
//! lacks, 2 usage or setup error, 3 camera in use.

use std::process::ExitCode;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use better_cam::capture::controls::{
    AccessPath, ControlError, ControlInfo, ControlKey, ControlSnapshot, DeviceControls,
};
use better_cam::capture::hotplug::{HotplugEvent, HotplugWatcher};
use better_cam::capture::reader::{
    CaptureReader, DecodeKind, FrameData, OpenOptions, RecvError, StopReason,
};
use better_cam::capture::{
    group_formats, hr_text, links_match, list_camera_keys, list_cameras, subtype_label,
    CaptureError, FormatRequest, PixelFormat, Ratio,
};
use better_cam::gpu::{AdapterKind, GpuDevice};
use windows::core::{Interface, GUID};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Texture2D, D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};

const USAGE: &str = "usage:
  capture_probe list
  capture_probe open  [--device N|NAME] [--format F] [--size WxH] [--fps N] [--seconds N] [--hw-decode on|off] [--gpu hw|warp]
  capture_probe watch [--seconds N]
  capture_probe controls [--device N|NAME] [--path auto|ks|iam|compare] [--save FILE]
  capture_probe set      [--device N|NAME] --control KEY [--value N|min|max|mid|default] [--auto true|false] [--measure] [--settle-ms N] [--reapply-test] [--path auto|ks|iam]
  capture_probe restore  [--device N|NAME] --file FILE";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        Some("list") => list(),
        Some("open") => open(&args[1..]),
        Some("watch") => watch(&args[1..]),
        Some("controls") => finish(controls_cmd(&args[1..])),
        Some("set") => finish(set_cmd(&args[1..])),
        Some("restore") => finish(restore_cmd(&args[1..])),
        _ => {
            eprintln!("{USAGE}");
            2
        }
    };
    ExitCode::from(code)
}

/// `--key value` pairs; flags that are not in `known` are an error.
fn parse_opts(args: &[String], known: &[&str]) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    let mut it = args.iter();
    while let Some(k) = it.next() {
        if !known.contains(&k.as_str()) {
            return Err(format!("unknown argument '{k}'"));
        }
        let v = it.next().ok_or_else(|| format!("{k} needs a value"))?;
        out.push((k.clone(), v.clone()));
    }
    Ok(out)
}

fn get<'a>(opts: &'a [(String, String)], key: &str) -> Option<&'a str> {
    opts.iter()
        .rev()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

/// The first four bytes of an MF subtype as text (`MJPG`), or `D3DFMT22`.
fn fourcc_text(g: &GUID) -> String {
    let b = g.data1.to_le_bytes();
    if b.iter().all(|c| c.is_ascii_graphic()) {
        String::from_utf8_lossy(&b).into_owned()
    } else {
        format!("D3DFMT{}", g.data1)
    }
}

// ---------------------------------------------------------------------------------------------------------------

fn list() -> u8 {
    let cameras = match list_cameras() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            return 2;
        }
    };
    if cameras.is_empty() {
        println!("no cameras");
    }
    for (i, cam) in cameras.iter().enumerate() {
        println!("camera {i}: {}", cam.key.name);
        println!("  symbolic link: {}", cam.key.symbolic_link);
        println!(
            "  device key:    link={} | vid_pid={} | name={}",
            cam.key.symbolic_link,
            cam.key.vid_pid.as_deref().unwrap_or("-"),
            cam.key.name
        );
        if let Some(e) = &cam.formats_error {
            println!("  formats: unavailable ({e})");
            continue;
        }
        println!("  formats: {} native types", cam.formats.len());
        for g in group_formats(&cam.formats) {
            let fps: Vec<String> = g.fps.iter().map(|r| r.to_string()).collect();
            let range = g
                .range
                .map(|(lo, hi)| format!("  range {lo}-{hi}"))
                .unwrap_or_default();
            println!(
                "    {:<6} {:<7} {{{}}}  {:>4}x{:<4}  fps {}{}",
                g.format.map_or("-".to_string(), |f| f.name().to_string()),
                fourcc_text(&g.subtype),
                format!("{:?}", g.subtype).trim_matches(|c| c == '{' || c == '}'),
                g.width,
                g.height,
                fps.join(", "),
                range
            );
        }
    }
    0
}

// ---------------------------------------------------------------------------------------------------------------

fn parse_ratio(s: &str) -> Result<Ratio, String> {
    let v: f64 = s.parse().map_err(|_| format!("bad --fps '{s}'"))?;
    if v <= 0.0 {
        return Err(format!("bad --fps '{s}'"));
    }
    for (num, den) in [(24000u32, 1001u32), (30000, 1001), (60000, 1001)] {
        if (v - num as f64 / den as f64).abs() < 0.01 {
            return Ok(Ratio::new(num, den));
        }
    }
    Ok(Ratio::new(v.round() as u32, 1))
}

fn parse_size(s: &str) -> Result<(u32, u32), String> {
    let (w, h) = s
        .to_ascii_lowercase()
        .split_once('x')
        .map(|(w, h)| (w.to_string(), h.to_string()))
        .ok_or("--size expects WxH")?;
    Ok((
        w.parse().map_err(|_| "bad width")?,
        h.parse().map_err(|_| "bad height")?,
    ))
}

fn open(args: &[String]) -> u8 {
    finish(open_inner(args))
}

/// Print a failure and turn the result into an exit code.
fn finish(r: Result<u8, Failure>) -> u8 {
    match r {
        Ok(code) => code,
        Err(Failure::Usage(m)) => {
            eprintln!("error: {m}");
            2
        }
        Err(Failure::Capture(e)) => {
            eprintln!("error: {e}");
            match e {
                CaptureError::InUse(_) => 3,
                _ => 2,
            }
        }
        Err(Failure::Control(e)) => {
            eprintln!("error: {e}");
            match e {
                ControlError::NotSupported(_)
                | ControlError::AutoNotSupported(_)
                | ControlError::ManualNotSupported(_) => 1,
                ControlError::Capture(CaptureError::InUse(_)) => 3,
                ControlError::Capture(_) => 2,
            }
        }
    }
}

enum Failure {
    Usage(String),
    Capture(CaptureError),
    Control(ControlError),
}

impl From<&str> for Failure {
    fn from(s: &str) -> Self {
        Failure::Usage(s.to_string())
    }
}

impl From<ControlError> for Failure {
    fn from(e: ControlError) -> Self {
        Failure::Control(e)
    }
}

impl From<String> for Failure {
    fn from(s: String) -> Self {
        Failure::Usage(s)
    }
}

impl From<CaptureError> for Failure {
    fn from(e: CaptureError) -> Self {
        Failure::Capture(e)
    }
}

fn open_inner(args: &[String]) -> Result<u8, Failure> {
    let opts = parse_opts(
        args,
        &[
            "--device",
            "--format",
            "--size",
            "--fps",
            "--seconds",
            "--hw-decode",
            "--gpu",
            "--simulate-removal",
        ],
    )?;
    // Test aid: report a removal after N seconds, as a hotplug notice for this camera would.
    let simulate_removal: Option<f64> = get(&opts, "--simulate-removal")
        .map(|s| {
            s.parse()
                .map_err(|_| format!("bad --simulate-removal '{s}'"))
        })
        .transpose()?;
    let seconds: f64 = get(&opts, "--seconds").map_or(Ok(10.0), |s| {
        s.parse().map_err(|_| format!("bad --seconds '{s}'"))
    })?;
    let hw_decode = match get(&opts, "--hw-decode").unwrap_or("on") {
        "on" => true,
        "off" => false,
        o => return Err(format!("--hw-decode expects on|off, got '{o}'").into()),
    };
    let adapter = match get(&opts, "--gpu").unwrap_or("hw") {
        "hw" => AdapterKind::Hardware,
        "warp" => AdapterKind::Warp,
        o => return Err(format!("--gpu expects hw|warp, got '{o}'").into()),
    };

    // Device.
    let cameras = list_cameras()?;
    let dev_arg = get(&opts, "--device").unwrap_or("0");
    let cam = match dev_arg.parse::<usize>() {
        Ok(i) => cameras.get(i),
        Err(_) => {
            let needle = dev_arg.to_ascii_lowercase();
            cameras.iter().find(|c| {
                c.key.name.to_ascii_lowercase().contains(&needle)
                    || c.key.symbolic_link.to_ascii_lowercase().contains(&needle)
            })
        }
    }
    .ok_or_else(|| CaptureError::NoSuchDevice(dev_arg.to_string()))?;

    // Format: default to the camera's first native type of a known format.
    let format = match get(&opts, "--format") {
        Some(f) => PixelFormat::from_name(f).ok_or_else(|| format!("unknown --format '{f}'"))?,
        None => cam
            .formats
            .iter()
            .find_map(|f| f.format)
            .ok_or_else(|| "camera lists no usable format".to_string())?,
    };
    let request = FormatRequest {
        format,
        size: get(&opts, "--size").map(parse_size).transpose()?,
        fps: get(&opts, "--fps").map(parse_ratio).transpose()?,
    };

    let gpu = if hw_decode {
        match GpuDevice::new(adapter) {
            Ok(g) => {
                println!(
                    "gpu: {} (vendor 0x{:04X}, feature level 0x{:X}, video support {})",
                    g.adapter_name,
                    g.vendor_id,
                    g.feature_level.0,
                    if g.video_support { "yes" } else { "no" }
                );
                Some(std::sync::Arc::new(g))
            }
            Err(e) => {
                eprintln!("warning: no D3D11 device ({e}); continuing with CPU frames");
                None
            }
        }
    } else {
        None
    };

    let hotplug = HotplugWatcher::start().ok();
    let mut open_opts = OpenOptions::new(&cam.key.symbolic_link, request);
    open_opts.hw_decode = hw_decode;
    open_opts.gpu = gpu.clone();

    println!("device: {}", cam.key.name);
    println!("link: {}", cam.key.symbolic_link);
    println!(
        "requested: {} {} @ {} hw_decode={}",
        request.format,
        request
            .size
            .map_or("largest".to_string(), |(w, h)| format!("{w}x{h}")),
        request.fps.map_or("highest".to_string(), |r| r.to_string()),
        if hw_decode { "on" } else { "off" }
    );

    let reader = CaptureReader::open(open_opts)?;
    let info = reader.info().clone();
    println!(
        "native: {} (index {})",
        info.native.label(),
        info.native.index
    );
    if info.fps_approximate {
        println!("note: requested rate not offered; using {}", info.fps);
    }
    println!(
        "output: {} {}x{}",
        info.output_format.map_or_else(
            || subtype_label(&info.output_subtype),
            |f| f.name().to_string()
        ),
        info.width,
        info.height
    );
    println!("decoder: {}", info.decoder_name.as_deref().unwrap_or("-"));
    println!("decode: {}", info.decode);
    println!(
        "d3d manager: {}",
        if info.d3d_manager { "yes" } else { "no" }
    );

    // Stream.
    let link = cam.key.symbolic_link.clone();
    let started = Instant::now();
    let end = started + Duration::from_secs_f64(seconds);
    let mut frames = 0u64;
    let mut texture_frames = 0u64;
    let mut first: Option<(Instant, i64)> = None;
    let mut last: Option<(Instant, i64)> = None;
    let mut max_gap = Duration::ZERO;
    let mut lat_sum = 0f64;
    let mut lat_max = 0f64;
    let mut last_luma: Option<f64> = None;
    let mut texture_desc: Option<String> = None;
    let mut stop: Option<StopReason> = None;

    let mut simulated = false;
    loop {
        if let Some(after) = simulate_removal {
            if !simulated && started.elapsed().as_secs_f64() >= after {
                simulated = true;
                reader.report_device_removed();
            }
        }
        if let Some(hp) = &hotplug {
            while let Ok(ev) = hp.1.try_recv() {
                if let HotplugEvent::Removal { link: l } = &ev {
                    if links_match(l, &link) {
                        reader.report_device_removed();
                    }
                }
            }
        }
        match reader.recv_timeout(Duration::from_millis(100)) {
            Ok(frame) => {
                let now = Instant::now();
                if let Some((t, _)) = last {
                    max_gap = max_gap.max(now - t);
                }
                first.get_or_insert((now, frame.timestamp_100ns));
                last = Some((now, frame.timestamp_100ns));
                frames += 1;
                let lat = (frame.arrival_100ns - frame.timestamp_100ns) as f64 / 10_000.0;
                lat_sum += lat;
                lat_max = lat_max.max(lat);
                match &frame.data {
                    FrameData::Texture {
                        texture,
                        subresource,
                    } => {
                        texture_frames += 1;
                        if texture_desc.is_none() {
                            let mut d = D3D11_TEXTURE2D_DESC::default();
                            unsafe { texture.GetDesc(&mut d) };
                            texture_desc = Some(format!(
                                "{}x{} format={} array={} slice={} bind=0x{:X} usage={} misc=0x{:X}",
                                d.Width, d.Height, d.Format.0, d.ArraySize, subresource, d.BindFlags, d.Usage.0, d.MiscFlags
                            ));
                        }
                    }
                    FrameData::Cpu(_) => {}
                }
                if frames % 30 == 0 || now >= end {
                    last_luma = luma_of(&frame, gpu.as_deref()).or(last_luma);
                }
            }
            Err(RecvError::Timeout) => {}
            Err(RecvError::Stopped(reason)) => {
                stop = Some(reason);
                break;
            }
        }
        if Instant::now() >= end {
            break;
        }
    }
    let stats = reader.stats();
    let closing = Instant::now();
    drop(reader); // clean shutdown releases the camera before we print the verdict
    drop(hotplug);
    println!("shutdown: {} ms", closing.elapsed().as_millis());

    // Report.
    println!(
        "frames: {frames} in {:.2} s",
        started.elapsed().as_secs_f64()
    );
    if let (Some((t0, ts0)), Some((t1, ts1))) = (first, last) {
        if frames > 1 {
            let wall = (frames - 1) as f64 / (t1 - t0).as_secs_f64().max(1e-9);
            let ts = (frames - 1) as f64 / ((ts1 - ts0) as f64 / 1e7).max(1e-9);
            println!("fps: {wall:.2}");
            println!("fps (sample timestamps): {ts:.2}");
            let target = info.fps.as_f64();
            let dev = (wall - target).abs() / target * 100.0;
            println!(
                "fps target: {} deviation {dev:.1} % -> {}",
                info.fps,
                if dev <= 10.0 {
                    "within 10 %"
                } else {
                    "OUTSIDE 10 %"
                }
            );
        }
        println!(
            "max gap between frames: {:.0} ms",
            max_gap.as_secs_f64() * 1000.0
        );
        println!(
            "latency (arrival - timestamp): mean {:.1} ms, max {:.1} ms",
            lat_sum / frames as f64,
            lat_max
        );
    }
    println!("dropped: {}", stats.dropped);
    println!("texture: {}", if texture_frames > 0 { "yes" } else { "no" });
    if let Some(d) = &texture_desc {
        println!("texture desc: {d}");
    }
    if texture_frames > 0 && texture_frames < frames {
        println!("note: {texture_frames} of {frames} frames were textures");
    }
    match last_luma {
        Some(l) => println!("luma mean (last sampled frame): {l:.1}"),
        None => println!("luma mean: n/a"),
    }

    match stop {
        Some(StopReason::DeviceLost(hr)) => {
            println!(
                "device lost{}",
                hr.map_or(String::new(), |h| format!(" ({})", hr_text(h)))
            );
            Ok(0)
        }
        Some(StopReason::InUse(hr)) => {
            println!("camera taken by another app ({})", hr_text(hr));
            Ok(1)
        }
        Some(other) => {
            println!("stopped: {other}");
            Ok(1)
        }
        None => {
            if info.decode == DecodeKind::None && texture_frames > 0 {
                println!("note: raw format delivered textures");
            }
            Ok(0)
        }
    }
}

/// Mean of the first-plane luma bytes of a frame (textures are copied to a staging texture first).
fn luma_of(frame: &better_cam::capture::reader::Frame, gpu: Option<&GpuDevice>) -> Option<f64> {
    let fmt = frame.format?;
    let (w, h) = (frame.width as usize, frame.height as usize);
    match &frame.data {
        FrameData::Cpu(cpu) => {
            let stride = cpu.stride();
            if stride <= 0 {
                return None;
            }
            mean_luma(cpu.data(), stride as usize, w, h, fmt)
        }
        FrameData::Texture {
            texture,
            subresource,
        } => {
            let gpu = gpu?;
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            unsafe { texture.GetDesc(&mut desc) };
            desc.ArraySize = 1;
            desc.MipLevels = 1;
            desc.Usage = D3D11_USAGE_STAGING;
            desc.BindFlags = 0;
            desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            desc.MiscFlags = 0;
            unsafe {
                let mut staging: Option<ID3D11Texture2D> = None;
                gpu.device
                    .CreateTexture2D(&desc, None, Some(&mut staging))
                    .ok()?;
                let staging = staging?;
                let dst: windows::Win32::Graphics::Direct3D11::ID3D11Resource =
                    staging.cast().ok()?;
                let src: windows::Win32::Graphics::Direct3D11::ID3D11Resource =
                    texture.cast().ok()?;
                gpu.context
                    .CopySubresourceRegion(&dst, 0, 0, 0, 0, &src, *subresource, None);
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                gpu.context
                    .Map(&dst, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                    .ok()?;
                let stride = mapped.RowPitch as usize;
                let bytes = std::slice::from_raw_parts(mapped.pData as *const u8, stride * h);
                let mean = mean_luma(bytes, stride, w, h, fmt);
                gpu.context.Unmap(&dst, 0);
                mean
            }
        }
    }
}

fn mean_luma(data: &[u8], stride: usize, w: usize, h: usize, fmt: PixelFormat) -> Option<f64> {
    let (step, offset) = match fmt {
        PixelFormat::Nv12 | PixelFormat::I420 | PixelFormat::Yv12 | PixelFormat::Y800 => (1, 0),
        PixelFormat::Yuy2 | PixelFormat::Yvyu => (2, 0),
        PixelFormat::Uyvy | PixelFormat::Hdyc => (2, 1),
        _ => return None,
    };
    let (mut sum, mut n) = (0u64, 0u64);
    for y in (0..h).step_by(4) {
        let row = data.get(y * stride..)?;
        for x in (0..w).step_by(4) {
            sum += *row.get(x * step + offset)? as u64;
            n += 1;
        }
    }
    (n > 0).then(|| sum as f64 / n as f64)
}

// ---------------------------------------------------------------------------------------------------------------

fn watch(args: &[String]) -> u8 {
    let opts = match parse_opts(args, &["--seconds"]) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("error: {e}");
            return 2;
        }
    };
    let seconds: f64 = match get(&opts, "--seconds").map_or(Ok(20.0), str::parse) {
        Ok(s) => s,
        Err(_) => {
            eprintln!("error: bad --seconds");
            return 2;
        }
    };
    let (watcher, rx) = match HotplugWatcher::start() {
        Ok(w) => w,
        Err(e) => {
            eprintln!("error: {e}");
            return 2;
        }
    };
    let started = Instant::now();
    let end = started + Duration::from_secs_f64(seconds);
    let show = |label: &str| match list_camera_keys() {
        Ok(keys) => {
            println!("{label}: {} camera(s)", keys.len());
            for k in keys {
                println!("  {} | {}", k.name, k.symbolic_link);
            }
        }
        Err(e) => println!("{label}: list failed: {e}"),
    };
    show("cameras now");
    println!("watching for {seconds:.0} s: plug or unplug a camera");
    while let Some(left) = end.checked_duration_since(Instant::now()) {
        match rx.recv_timeout(left) {
            Ok(ev) => {
                let t = started.elapsed().as_secs_f64();
                match &ev {
                    HotplugEvent::Arrival { link } => {
                        println!("[{t:6.2} s] arrival: {link}");
                        // The interface can appear before Media Foundation lists the device: retry briefly.
                        let mut listed = false;
                        for _ in 0..10 {
                            if list_camera_keys().is_ok_and(|k| {
                                k.iter().any(|k| links_match(&k.symbolic_link, link))
                            }) {
                                listed = true;
                                break;
                            }
                            std::thread::sleep(Duration::from_millis(300));
                        }
                        println!(
                            "           listed by Media Foundation: {}",
                            if listed { "yes" } else { "no" }
                        );
                    }
                    HotplugEvent::Removal { link } => println!("[{t:6.2} s] removal: {link}"),
                }
            }
            Err(RecvTimeoutError::Timeout) => break,
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    drop(watcher);
    show("cameras at end");
    0
}

// ---------------------------------------------------------------------------------------------------------------
// Device controls (plan phase P4)

/// An open camera with its controls. `controls` is declared first so it drops before the reader.
struct Session {
    controls: DeviceControls,
    reader: CaptureReader,
}

/// The camera `--device` names, and the format the probe opens it with.
struct Target {
    key: better_cam::capture::DeviceKey,
    request: FormatRequest,
}

fn target(opts: &[(String, String)]) -> Result<Target, Failure> {
    let cameras = list_cameras()?;
    let dev_arg = get(opts, "--device").unwrap_or("0");
    let cam = match dev_arg.parse::<usize>() {
        Ok(i) => cameras.get(i),
        Err(_) => {
            let needle = dev_arg.to_ascii_lowercase();
            cameras.iter().find(|c| {
                c.key.name.to_ascii_lowercase().contains(&needle)
                    || c.key.symbolic_link.to_ascii_lowercase().contains(&needle)
            })
        }
    }
    .ok_or_else(|| CaptureError::NoSuchDevice(dev_arg.to_string()))?;
    // The default must stream fast: some cameras deliver their largest raw sizes at 2 fps, which can miss the
    // first-frame timeout. Prefer MJPEG, and 1280x720 when the camera lists it.
    let format = match get(opts, "--format") {
        Some(f) => PixelFormat::from_name(f).ok_or_else(|| format!("unknown --format '{f}'"))?,
        None => cam
            .formats
            .iter()
            .find(|f| f.format == Some(PixelFormat::Mjpeg))
            .or_else(|| cam.formats.iter().find(|f| f.format.is_some()))
            .and_then(|f| f.format)
            .ok_or_else(|| "camera lists no usable format".to_string())?,
    };
    let size = match get(opts, "--size") {
        Some(s) => Some(parse_size(s)?),
        None => cam
            .formats
            .iter()
            .any(|f| f.format == Some(format) && (f.width, f.height) == (1280, 720))
            .then_some((1280, 720)),
    };
    Ok(Target {
        key: cam.key.clone(),
        request: FormatRequest {
            format,
            size,
            fps: get(opts, "--fps").map(parse_ratio).transpose()?,
        },
    })
}

/// Open the camera (software decode, CPU frames) and attach its controls. Another process holding the camera is
/// retried for about 40 s.
fn open_session(t: &Target) -> Result<Session, Failure> {
    let mut attempt = 0;
    let reader = loop {
        let mut o = OpenOptions::new(&t.key.symbolic_link, t.request);
        o.hw_decode = false;
        match CaptureReader::open(o) {
            Err(CaptureError::InUse(e)) if attempt < 20 => {
                attempt += 1;
                eprintln!(
                    "camera in use ({}); retry {attempt}/20 in 2 s",
                    hr_text(e.code())
                );
                std::thread::sleep(Duration::from_secs(2));
            }
            other => break other?,
        }
    };
    let controls = DeviceControls::from_reader(&reader)?;
    Ok(Session { controls, reader })
}

const COLUMNS: &str =
    "key                      min     max  step  default   value  can_auto  manual  auto   path";

fn row(i: &ControlInfo) -> String {
    format!(
        "{:<22} {:>6}  {:>6}  {:>4}  {:>7}  {:>6}  {:<8}  {:<6}  {:<5}  {}",
        i.key.key(),
        i.min,
        i.max,
        i.step,
        i.default,
        i.value,
        i.can_auto,
        i.can_manual,
        i.auto,
        i.path
    )
}

fn print_table(infos: &[ControlInfo]) {
    println!("{COLUMNS}");
    for i in infos {
        println!("{}", row(i));
    }
    let missing: Vec<&str> = ControlKey::ALL
        .iter()
        .filter(|k| !infos.iter().any(|i| i.key == **k))
        .map(|k| k.key())
        .collect();
    println!("not supported: {}", missing.join(", "));
}

fn parse_bool(s: &str, flag: &str) -> Result<bool, String> {
    match s.to_ascii_lowercase().as_str() {
        "true" | "on" | "1" | "yes" => Ok(true),
        "false" | "off" | "0" | "no" => Ok(false),
        _ => Err(format!("{flag} expects true|false, got '{s}'")),
    }
}

fn controls_cmd(args: &[String]) -> Result<u8, Failure> {
    let opts = parse_opts(
        args,
        &[
            "--device", "--format", "--size", "--fps", "--path", "--save",
        ],
    )?;
    let t = target(&opts)?;
    let session = open_session(&t)?;
    println!("device: {}", t.key.name);
    println!("link: {}", t.key.symbolic_link);
    let path = get(&opts, "--path").unwrap_or("auto");
    let infos = match path {
        "auto" => session.controls.read_all()?,
        "ks" => session.controls.read_all_via(Some(AccessPath::Ks))?,
        "iam" => session.controls.read_all_via(Some(AccessPath::Iam))?,
        "compare" => {
            let ks = session.controls.read_all_via(Some(AccessPath::Ks))?;
            let iam = session.controls.read_all_via(Some(AccessPath::Iam))?;
            println!("-- KS --");
            print_table(&ks);
            println!("-- IAM --");
            print_table(&iam);
            let mut same = true;
            for k in ControlKey::ALL {
                if k.iam_property().is_none() {
                    continue; // KS only: IAM cannot serve it
                }
                let a = ks.iter().find(|i| i.key == k);
                let b = iam.iter().find(|i| i.key == k);
                let eq = match (a, b) {
                    (Some(a), Some(b)) => {
                        ControlInfo {
                            path: AccessPath::Ks,
                            ..b.clone()
                        } == *a
                    }
                    (None, None) => true,
                    _ => false,
                };
                if !eq {
                    same = false;
                    println!(
                        "differs: {k}: KS {:?} | IAM {:?}",
                        a.map(|i| (
                            i.min,
                            i.max,
                            i.step,
                            i.default,
                            i.value,
                            i.can_auto,
                            i.can_manual,
                            i.auto
                        )),
                        b.map(|i| (
                            i.min,
                            i.max,
                            i.step,
                            i.default,
                            i.value,
                            i.can_auto,
                            i.can_manual,
                            i.auto
                        ))
                    );
                }
            }
            println!(
                "KS and IAM agree on every control IAM can serve: {}",
                if same { "yes" } else { "NO" }
            );
            ks
        }
        o => return Err(format!("--path expects auto|ks|iam|compare, got '{o}'").into()),
    };
    if path != "compare" {
        print_table(&infos);
    }
    if let Some(file) = get(&opts, "--save") {
        // Full device state: the library's snapshot plus the value an Auto control reports, so `restore` can put
        // that back too (devices keep the last manual value while Auto is on).
        let mut snap = ControlSnapshot::from_infos(t.key.clone(), &infos);
        for c in &mut snap.controls {
            if c.value.is_none() {
                c.value = infos.iter().find(|i| i.key == c.key).map(|i| i.value);
            }
        }
        std::fs::write(
            file,
            serde_json::to_string_pretty(&snap).map_err(|e| e.to_string())?,
        )
        .map_err(|e| format!("cannot write {file}: {e}"))?;
        println!(
            "saved snapshot ({} controls) to {file}",
            snap.controls.len()
        );
    }
    Ok(0)
}

fn restore_cmd(args: &[String]) -> Result<u8, Failure> {
    let opts = parse_opts(args, &["--device", "--format", "--size", "--fps", "--file"])?;
    let file = get(&opts, "--file").ok_or("restore needs --file")?;
    let snap: ControlSnapshot = serde_json::from_str(
        &std::fs::read_to_string(file).map_err(|e| format!("cannot read {file}: {e}"))?,
    )
    .map_err(|e| format!("bad snapshot {file}: {e}"))?;
    let t = target(&opts)?;
    let session = open_session(&t)?;
    println!("device: {}", t.key.name);
    let report = session.controls.apply_snapshot(&snap)?;
    print_report(&report);
    // A control stored as Auto keeps its Auto mode under `apply_snapshot`; the value the device reports while
    // Auto is on is the last manual one. Put it back the way it was: write it, then turn Auto on again.
    for c in &snap.controls {
        if let (Some(true), Some(v)) = (c.auto, c.value) {
            session.controls.set_value(c.key, v)?;
            session.controls.set_auto(c.key, true)?;
            println!("restored: {:<22} value={v} then auto=true", c.key.key());
        }
    }
    println!("restored table:");
    print_table(&session.controls.read_all()?);
    Ok(0)
}

fn print_report(r: &better_cam::capture::controls::ApplyReport) {
    for a in &r.applied {
        println!(
            "applied: {:<22} auto={} value={}{}",
            a.key.key(),
            a.auto.map_or("-".to_string(), |v| v.to_string()),
            a.value.map_or("-".to_string(), |v| v.to_string()),
            a.clamped_from
                .map(|v| format!(" (clamped from {v})"))
                .unwrap_or_default()
        );
    }
    for (k, why) in &r.skipped {
        println!("skipped: {:<22} {why}", k.key());
    }
}

/// `min|max|mid|default|<int>` against what the device reports.
fn resolve_value(spec: &str, i: &ControlInfo) -> Result<i32, String> {
    Ok(match spec.to_ascii_lowercase().as_str() {
        "min" => i.min,
        "max" => i.max,
        "default" => i.default,
        "mid" => i.min + (i.max - i.min) / 2 / i.step.max(1) * i.step.max(1),
        n => n
            .parse()
            .map_err(|_| format!("--value expects min|max|mid|default|<number>, got '{spec}'"))?,
    })
}

/// Mean luma of the next `n` frames (stale queued frames are dropped first).
fn collect_luma(reader: &CaptureReader, n: usize) -> Result<f64, Failure> {
    while reader.try_recv().is_ok() {}
    let mut sum = 0.0;
    let mut got = 0;
    while got < n {
        match reader.recv_timeout(Duration::from_secs(2)) {
            Ok(f) => {
                let l = luma_of(&f, None).ok_or_else(|| {
                    "luma needs a CPU frame in nv12, yuy2, i420 or similar".to_string()
                })?;
                sum += l;
                got += 1;
            }
            Err(RecvError::Timeout) => return Err("no frame for 2 s".to_string().into()),
            Err(RecvError::Stopped(r)) => return Err(format!("stream stopped: {r}").into()),
        }
    }
    Ok(sum / n as f64)
}

/// `(ms since t0, luma)` of every frame that arrives until `t0 + window`.
fn watch_luma(reader: &CaptureReader, t0: Instant, window: Duration) -> Vec<(f64, f64)> {
    let mut out = Vec::new();
    while let Some(left) = (t0 + window).checked_duration_since(Instant::now()) {
        if let Ok(f) = reader.recv_timeout(left.min(Duration::from_millis(100))) {
            if let Some(l) = luma_of(&f, None) {
                out.push((t0.elapsed().as_secs_f64() * 1000.0, l));
            }
        }
    }
    out
}

/// First time the picture moved at least halfway from `before` to `after`.
fn first_visible(series: &[(f64, f64)], before: f64, after: f64) -> Option<f64> {
    let half = before + (after - before) / 2.0;
    series
        .iter()
        .find(|(_, l)| {
            if after > before {
                *l >= half
            } else {
                *l <= half
            }
        })
        .map(|(t, _)| *t)
}

fn set_cmd(args: &[String]) -> Result<u8, Failure> {
    // `--measure` and `--reapply-test` are bare flags; the rest are `--key value` pairs.
    let measure = args.iter().any(|a| a == "--measure");
    let reapply = args.iter().any(|a| a == "--reapply-test");
    let pairs: Vec<String> = args
        .iter()
        .filter(|a| *a != "--measure" && *a != "--reapply-test")
        .cloned()
        .collect();
    let opts = parse_opts(
        &pairs,
        &[
            "--device",
            "--format",
            "--size",
            "--fps",
            "--control",
            "--value",
            "--auto",
            "--settle-ms",
            "--path",
        ],
    )?;
    let forced = match get(&opts, "--path").unwrap_or("auto") {
        "auto" => None,
        "ks" => Some(AccessPath::Ks),
        "iam" => Some(AccessPath::Iam),
        o => return Err(format!("--path expects auto|ks|iam, got '{o}'").into()),
    };

    let name = get(&opts, "--control").ok_or("set needs --control KEY")?;
    let key = ControlKey::from_key(name).ok_or_else(|| {
        format!(
            "unknown control '{name}'; keys: {}",
            ControlKey::ALL.map(|k| k.key()).join(", ")
        )
    })?;
    let value_spec = get(&opts, "--value");
    let auto = get(&opts, "--auto")
        .map(|s| parse_bool(s, "--auto"))
        .transpose()?;
    if value_spec.is_none() && auto.is_none() {
        return Err("set needs --value and/or --auto".to_string().into());
    }
    if auto == Some(true) && value_spec.is_some() {
        return Err(
            "--auto true cannot be combined with --value (a value makes a control manual)"
                .to_string()
                .into(),
        );
    }
    let settle = Duration::from_millis(
        get(&opts, "--settle-ms")
            .map_or(Ok(2500), str::parse)
            .map_err(|_| "bad --settle-ms".to_string())?,
    );

    let t = target(&opts)?;
    let session = open_session(&t)?;
    session.controls.force_path(forced);
    println!("device: {}", t.key.name);
    let Some(before) = session.controls.read(key)? else {
        return Err(ControlError::NotSupported(key).into());
    };
    println!("{COLUMNS}");
    println!("{}   <- before", row(&before));
    let wanted = value_spec.map(|s| resolve_value(s, &before)).transpose()?;

    let luma_before = if measure {
        let l = collect_luma(&session.reader, 30)?;
        println!("luma before: {l:.2} (mean of 30 frames)");
        Some(l)
    } else {
        None
    };

    let mut snapshot = ControlSnapshot::new(t.key.clone());
    let write_start = Instant::now();
    if let Some(a) = auto {
        let s = Instant::now();
        session.controls.set_auto(key, a)?;
        println!(
            "write: auto={a} took {:.1} ms",
            s.elapsed().as_secs_f64() * 1000.0
        );
        snapshot.record_auto(key, a);
    }
    if let Some(v) = wanted {
        let s = Instant::now();
        session.controls.set_value(key, v)?;
        println!(
            "write: value={v}{} took {:.1} ms",
            if v != before.clamp(v) {
                format!(" (clamped to {})", before.clamp(v))
            } else {
                String::new()
            },
            s.elapsed().as_secs_f64() * 1000.0
        );
        snapshot.record_value(key, before.clamp(v));
    }
    let after = session
        .controls
        .read(key)?
        .ok_or(ControlError::NotSupported(key))?;
    println!("{}   <- after", row(&after));

    if let Some(lb) = luma_before {
        let series = watch_luma(&session.reader, write_start, settle);
        let la = collect_luma(&session.reader, 30)?;
        println!(
            "luma after:  {la:.2} (mean of 30 frames, {} ms after the write)",
            settle.as_millis()
        );
        println!(
            "luma change: {:+.2}  after > before: {}",
            la - lb,
            if la > lb { "yes" } else { "no" }
        );
        let shown: Vec<String> = series
            .iter()
            .take(14)
            .map(|(t, l)| format!("{t:.0}ms:{l:.1}"))
            .collect();
        println!("luma series after write: {}", shown.join(" "));
        if (la - lb).abs() >= 0.5 {
            match first_visible(&series, lb, la) {
                Some(t) => println!("picture moved halfway after {t:.0} ms"),
                None => println!(
                    "picture moved halfway: not inside the {} ms window",
                    settle.as_millis()
                ),
            }
        } else {
            println!(
                "no visible change in luma (before {lb:.2}, after {la:.2}): the scene may be too dark or too flat to show this control"
            );
        }
    }

    if reapply {
        reapply_test(&t, session, key, &snapshot)?;
    }
    Ok(0)
}

/// Close and reopen the camera, show what the device reports right after reopen, apply the snapshot, and check
/// the re-applied state equals the stored one. A second round first disturbs the control, so the check also
/// passes meaningfully on a device that keeps its values.
fn reapply_test(
    t: &Target,
    session: Session,
    key: ControlKey,
    snapshot: &ControlSnapshot,
) -> Result<(), Failure> {
    let stored = session
        .controls
        .read(key)?
        .ok_or(ControlError::NotSupported(key))?;
    println!(
        "-- reapply test: stored state of {key}: value={} auto={}",
        stored.value, stored.auto
    );
    let json = serde_json::to_string(snapshot).map_err(|e| e.to_string())?;
    println!("snapshot: {json}");
    let snapshot: ControlSnapshot = serde_json::from_str(&json).map_err(|e| e.to_string())?;

    // Close: the controls drop first, then the reader releases the camera.
    let closing = Instant::now();
    drop(session);
    println!("closed in {} ms; reopening", closing.elapsed().as_millis());
    std::thread::sleep(Duration::from_millis(800));
    let session = open_session(t)?;

    let fresh = session
        .controls
        .read(key)?
        .ok_or(ControlError::NotSupported(key))?;
    let same =
        |a: &ControlInfo, b: &ControlInfo| a.auto == b.auto && (a.auto || a.value == b.value);
    println!("{COLUMNS}");
    println!("{}   <- straight after reopen", row(&fresh));
    println!(
        "device after reopen: {}",
        if same(&fresh, &stored) {
            "KEPT the stored state"
        } else {
            "LOST the stored state"
        }
    );
    let report = session.controls.apply_snapshot(&snapshot)?;
    print_report(&report);
    let applied = session
        .controls
        .read(key)?
        .ok_or(ControlError::NotSupported(key))?;
    println!("{}   <- after re-apply", row(&applied));
    println!(
        "re-applied == stored: {}",
        if same(&applied, &stored) { "yes" } else { "NO" }
    );

    // Disturb the control, apply again.
    let disturbed = if stored.auto {
        session.controls.set_auto(key, false)
    } else {
        let other = if stored.value == stored.min {
            stored.max
        } else {
            stored.min
        };
        session.controls.set_value(key, other)
    }?;
    println!("{}   <- disturbed", row(&disturbed));
    let report = session.controls.apply_snapshot(&snapshot)?;
    print_report(&report);
    let again = session
        .controls
        .read(key)?
        .ok_or(ControlError::NotSupported(key))?;
    println!("{}   <- after second re-apply", row(&again));
    println!(
        "re-applied after disturbance == stored: {}",
        if same(&again, &stored) { "yes" } else { "NO" }
    );
    Ok(())
}
