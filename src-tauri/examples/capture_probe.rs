//! Command-line probe for the capture backend (plan phase P3).
//!
//! ```text
//! capture_probe list
//! capture_probe open  [--device N|NAME] [--format mjpeg|nv12|yuy2|...] [--size WxH] [--fps N]
//!                     [--seconds N] [--hw-decode on|off] [--gpu hw|warp]
//! capture_probe watch [--seconds N]
//! ```
//!
//! Exit codes: 0 success (also when the device is lost during `open`), 1 stream error, 2 usage or setup error,
//! 3 camera in use.

use std::process::ExitCode;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

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
  capture_probe watch [--seconds N]";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        Some("list") => list(),
        Some("open") => open(&args[1..]),
        Some("watch") => watch(&args[1..]),
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
    match open_inner(args) {
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
    }
}

enum Failure {
    Usage(String),
    Capture(CaptureError),
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
