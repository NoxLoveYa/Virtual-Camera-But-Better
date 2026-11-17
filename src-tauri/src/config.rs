//! Source and output configuration as the frontend sends it (plan 5.2) and its mapping to the capture layer and the
//! engine (plan 6.1, 6.3).
//!
//! The JSON keys and string values are exactly the TypeScript interfaces of plan 5.2: snake_case keys named like the
//! OBS `dshow_input` settings. Every field is accepted; [`NOT_IMPLEMENTED`] lists the ones this phase stores but
//! does not act on.
//!
//! Nothing here depends on Tauri.

use serde::de::{self, Deserializer};
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};
use vcam_protocol::Format;

use crate::capture::{
    self, camera_formats, group_formats, links_match, list_camera_keys, parse_vid_pid, CameraInfo,
    DeviceKey, FormatRequest, NativeFormat, PixelFormat, Ratio,
};
use crate::engine::{CameraSpec, Failure, OutputSettings, RetryKind};
use crate::gpu::convert::{is_supported, ColorRange, ColorSettings, ColorSpace};
use crate::gpu::output::FitMode;

/// Fields that are accepted and stored but have no effect yet, with the reason.
pub const NOT_IMPLEMENTED: &[(&str, &str)] = &[
    (
        "color_space = 2100PQ | 2100HLG",
        "HDR matrices and the tone map belong to the later HDR Tone Mapping filter; treated as \"default\"",
    ),
    (
        "buffering",
        "the engine always renders the newest frame (the plan's Auto for live cameras); \"on\" (a 6-frame queue) needs a timed presenter; only a change reopens the camera",
    ),
    (
        "autorotation",
        "MF_MT_VIDEO_ROTATION needs a rotate pass in the convert stage; no change reopens the camera",
    ),
];

// ---------------------------------------------------------------------------------------------------------------
// Source

/// `res_type`: OBS int 0 | 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResType {
    Preferred,
    Custom,
}

/// `video_format`: `"any"` or an OBS format name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoFormat {
    Any,
    Format(PixelFormat),
}

impl Serialize for VideoFormat {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(match self {
            VideoFormat::Any => "any",
            VideoFormat::Format(f) => f.name(),
        })
    }
}

impl<'de> Deserialize<'de> for VideoFormat {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<VideoFormat, D::Error> {
        let s = String::deserialize(d)?;
        if s.eq_ignore_ascii_case("any") {
            return Ok(VideoFormat::Any);
        }
        PixelFormat::from_name(&s)
            .map(VideoFormat::Format)
            .ok_or_else(|| de::Error::custom(format!("unknown video_format '{s}'")))
    }
}

/// `color_space`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ColorSpaceCfg {
    #[serde(rename = "default")]
    Default,
    #[serde(rename = "709")]
    Rec709,
    #[serde(rename = "601")]
    Rec601,
    #[serde(rename = "2100PQ")]
    Rec2100Pq,
    #[serde(rename = "2100HLG")]
    Rec2100Hlg,
}

/// `color_range`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ColorRangeCfg {
    Default,
    Partial,
    Full,
}

/// `buffering`: OBS int 0 | 1 | 2.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Buffering {
    Auto,
    On,
    Off,
}

/// Plan 5.2 `SourceCfg`. Missing keys take the defaults of plan 6.1 (Better Cam column).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SourceCfg {
    /// Media Foundation symbolic link of the camera; `null` = the first camera the system lists.
    pub video_device_id: Option<String>,
    pub res_type: ResType,
    /// `"1920x1080"`; only used when `res_type` is `custom`.
    pub resolution: String,
    /// 100 ns units; -1 = match the output FPS, 0 = highest. Only used when `res_type` is `custom`.
    pub frame_interval: i64,
    pub video_format: VideoFormat,
    pub color_space: ColorSpaceCfg,
    pub color_range: ColorRangeCfg,
    pub buffering: Buffering,
    pub flip_vertically: bool,
    pub autorotation: bool,
    pub hw_decode: bool,
    pub deactivate_when_not_showing: bool,
}

impl Default for SourceCfg {
    fn default() -> Self {
        SourceCfg {
            video_device_id: None,
            res_type: ResType::Preferred,
            // The frontend replaces this with the device's largest 16:9 size once it has listed the cameras.
            resolution: "1920x1080".into(),
            frame_interval: -1,
            video_format: VideoFormat::Any,
            color_space: ColorSpaceCfg::Default,
            color_range: ColorRangeCfg::Default,
            buffering: Buffering::Auto,
            flip_vertically: false,
            autorotation: true,
            hw_decode: true,
            deactivate_when_not_showing: true,
        }
    }
}

/// The fields of [`SourceCfg`] that decide how the camera is opened: a change reopens it.
#[derive(Clone, Debug, PartialEq)]
pub struct CaptureCfg {
    pub device: Option<String>,
    pub res_type: ResType,
    pub resolution: String,
    pub frame_interval: i64,
    pub video_format: VideoFormat,
    pub hw_decode: bool,
    pub buffering: Buffering,
}

impl SourceCfg {
    pub fn capture(&self) -> CaptureCfg {
        CaptureCfg {
            device: self.video_device_id.clone(),
            res_type: self.res_type,
            resolution: self.resolution.clone(),
            frame_interval: self.frame_interval,
            video_format: self.video_format,
            hw_decode: self.hw_decode,
            buffering: self.buffering,
        }
    }

    /// The overrides for the colour space and range the camera states (`None` = what the camera says).
    pub fn color(&self) -> ColorSettings {
        ColorSettings {
            space: match self.color_space {
                ColorSpaceCfg::Rec709 => Some(ColorSpace::Bt709),
                ColorSpaceCfg::Rec601 => Some(ColorSpace::Bt601),
                // HDR is not implemented: behave like "default".
                ColorSpaceCfg::Default | ColorSpaceCfg::Rec2100Pq | ColorSpaceCfg::Rec2100Hlg => {
                    None
                }
            },
            range: match self.color_range {
                ColorRangeCfg::Default => None,
                ColorRangeCfg::Partial => Some(ColorRange::Limited),
                ColorRangeCfg::Full => Some(ColorRange::Full),
            },
        }
    }

    /// Reject what could never open (a custom size that is not `WxH`) before the engine hears of it.
    pub fn validate(&self) -> Result<(), String> {
        if self.res_type == ResType::Custom {
            parse_size(&self.resolution).map_err(|e| format!("resolution: {e}"))?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Output

/// `fps`: `"source"` or a number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputFps {
    Source,
    Rate(u32),
}

impl Serialize for OutputFps {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            OutputFps::Source => s.serialize_str("source"),
            OutputFps::Rate(n) => s.serialize_u32(*n),
        }
    }
}

impl<'de> Deserialize<'de> for OutputFps {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<OutputFps, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Rate(u32),
            Word(String),
        }
        match Raw::deserialize(d)? {
            Raw::Rate(n) => Ok(OutputFps::Rate(n)),
            Raw::Word(w) if w.eq_ignore_ascii_case("source") => Ok(OutputFps::Source),
            Raw::Word(w) => Err(de::Error::custom(format!(
                "fps must be \"source\" or a number, got '{w}'"
            ))),
        }
    }
}

/// `fit`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FitCfg {
    Fit,
    Fill,
    Stretch,
}

/// `background_color` is an OBS int `0xAABBGGRR`; the frontend may send it signed (OBS stores 32-bit ints).
fn de_abgr<'de, D: Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
    let v = i64::deserialize(d)?;
    if !(i32::MIN as i64..=u32::MAX as i64).contains(&v) {
        return Err(de::Error::custom(format!(
            "background_color {v} is not a 32-bit colour"
        )));
    }
    Ok(v as u32)
}

/// Plan 5.2 `OutputCfg`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OutputCfg {
    /// `"source"` or `"WxH"`.
    pub resolution: String,
    pub fps: OutputFps,
    pub fit: FitCfg,
    #[serde(deserialize_with = "de_abgr")]
    pub background_color: u32,
    pub extra_sizes: bool,
}

impl Default for OutputCfg {
    fn default() -> Self {
        OutputCfg {
            resolution: "source".into(),
            fps: OutputFps::Source,
            fit: FitCfg::Fit,
            background_color: 0xFF00_0000,
            extra_sizes: true,
        }
    }
}

/// Largest output frame (plan 6.3, capped by the protocol's 3840x2160 slot).
pub const OUTPUT_MAX: (u32, u32) = (3840, 2160);
/// Smallest output frame (plan 6.3).
pub const OUTPUT_MIN: (u32, u32) = (160, 120);

/// `WxH` (also `W×H`, any case).
pub fn parse_size(s: &str) -> Result<(u32, u32), String> {
    let t = s.trim().to_lowercase().replace('×', "x");
    let (w, h) = t
        .split_once('x')
        .ok_or_else(|| format!("'{s}' is not WxH"))?;
    let num = |v: &str| {
        v.trim()
            .parse::<u32>()
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(|| format!("'{s}' is not WxH"))
    };
    Ok((num(w)?, num(h)?))
}

/// OBS `0xAABBGGRR` to `[r, g, b]` (alpha is ignored: the background is always opaque).
pub fn abgr_to_rgb(c: u32) -> [u8; 3] {
    [c as u8, (c >> 8) as u8, (c >> 16) as u8]
}

/// Map the output config to the engine's settings. An explicit size is capped to 3840x2160, raised to 160x120 and
/// made even, each axis on its own.
pub fn output_settings(o: &OutputCfg) -> Result<OutputSettings, String> {
    let (width, height) = if o.resolution.eq_ignore_ascii_case("source") {
        (None, None)
    } else {
        let (w, h) = parse_size(&o.resolution).map_err(|e| format!("output resolution: {e}"))?;
        (
            Some(w.clamp(OUTPUT_MIN.0, OUTPUT_MAX.0) & !1),
            Some(h.clamp(OUTPUT_MIN.1, OUTPUT_MAX.1) & !1),
        )
    };
    let fps = match o.fps {
        OutputFps::Source => None,
        OutputFps::Rate(0) => return Err("output fps must be \"source\" or at least 1".into()),
        OutputFps::Rate(n) => Some(n.min(240)),
    };
    Ok(OutputSettings {
        width,
        height,
        fps,
        fit: match o.fit {
            FitCfg::Fit => FitMode::Fit,
            FitCfg::Fill => FitMode::Fill,
            FitCfg::Stretch => FitMode::Stretch,
        },
        background: abgr_to_rgb(o.background_color),
        extra_sizes: o.extra_sizes,
    })
}

/// The media types to advertise for an output size and rate. Without `extra_sizes` only the output size stays.
pub fn advertised_list(width: u32, height: u32, fps: u32, extra_sizes: bool) -> Vec<Format> {
    let mut v = vcam_protocol::advertised_for_output(width, height, fps);
    if !extra_sizes {
        v.retain(|f| f.width == width && f.height == height);
    }
    v
}

// ---------------------------------------------------------------------------------------------------------------
// Camera choice

/// `frame_interval` (100 ns units) as a rate. A rate within 0.01 of a whole number is that number (333333 is 30).
pub fn ratio_from_interval(interval: u32) -> Ratio {
    let fps = 10_000_000.0 / interval.max(1) as f64;
    if (fps - fps.round()).abs() < 0.01 {
        Ratio::new(fps.round() as u32, 1)
    } else {
        Ratio::new(10_000_000, interval)
    }
}

/// A rate as `frame_interval` units: the inverse of [`ratio_from_interval`], rounded (30 -> 333333).
pub fn interval_of(r: Ratio) -> u64 {
    if r.num == 0 {
        return 0;
    }
    (10_000_000.0 * r.den as f64 / r.num as f64).round() as u64
}

/// The camera `device` names among the present ones: the exact link, else the only camera with the same
/// `vid_xxxx&pid_xxxx` (the device moved to another USB port); `None` as the id means the first camera.
pub fn pick_device<'a>(keys: &'a [DeviceKey], wanted: Option<&str>) -> Option<&'a DeviceKey> {
    let Some(want) = wanted.filter(|w| !w.is_empty()) else {
        return keys.first();
    };
    if let Some(k) = keys.iter().find(|k| links_match(&k.symbolic_link, want)) {
        return Some(k);
    }
    let vid_pid = parse_vid_pid(want)?;
    let mut same = keys
        .iter()
        .filter(|k| k.vid_pid.as_deref() == Some(vid_pid.as_str()));
    match (same.next(), same.next()) {
        (Some(k), None) => Some(k),
        _ => None,
    }
}

/// Whether a camera arrival is the device the engine is waiting for: the same link or the same vid/pid. Without a
/// target any arrival counts.
pub fn arrival_matches(target: Option<&str>, arrived: &str) -> bool {
    match target.filter(|t| !t.is_empty()) {
        None => true,
        Some(t) => {
            links_match(t, arrived)
                || parse_vid_pid(t).is_some_and(|v| parse_vid_pid(arrived).as_deref() == Some(&v))
        }
    }
}

/// Formats the engine can take in: the ones the converter knows and the compressed ones the reader decodes.
fn ingestible(f: PixelFormat) -> bool {
    is_supported(f) || f.is_compressed()
}

/// Format preference when several reach the wanted rate: raw before compressed (no decoder, least latency).
fn format_rank(f: PixelFormat) -> u32 {
    match f {
        PixelFormat::Nv12 => 0,
        PixelFormat::Yuy2 => 1,
        PixelFormat::I420 | PixelFormat::Yv12 => 2,
        PixelFormat::P010 => 3,
        PixelFormat::Xrgb | PixelFormat::Argb => 4,
        PixelFormat::Mjpeg => 5,
        PixelFormat::H264 => 6,
        PixelFormat::Hevc => 7,
        _ => 9,
    }
}

/// The highest rate a native type offers.
fn top_rate(f: &NativeFormat) -> f64 {
    f.fps.as_f64().max(f.fps_max.as_f64())
}

/// Whether `f` can run at `want` (its own rate or inside its range).
fn offers_rate(f: &NativeFormat, want: Ratio) -> bool {
    let w = want.as_f64();
    (f.fps.as_f64() - w).abs() < 0.005
        || (f.fps_min.as_f64() - 0.005 <= w && w <= f.fps_max.as_f64() + 0.005)
}

/// Pick a format for `video_format: "any"` at `size`: the one that reaches the wanted rate (or the highest rate when
/// none is wanted), raw before compressed.
fn pick_any(
    formats: &[NativeFormat],
    size: (u32, u32),
    want: Option<Ratio>,
) -> Option<PixelFormat> {
    let mut best: Option<(PixelFormat, (u8, i64, u32))> = None;
    for f in formats
        .iter()
        .filter(|f| (f.width, f.height) == size)
        .filter(|f| f.format.is_some_and(ingestible))
    {
        let pf = f.format.expect("filtered above");
        // Smallest key wins: a type that reaches the wanted rate, else the highest rate (milli-fps, so ties
        // compare), then the raw-before-compressed rank.
        let reaches = want.is_none_or(|w| offers_rate(f, w));
        let key = (
            !reaches as u8,
            if reaches && want.is_some() {
                0
            } else {
                -((top_rate(f) * 1000.0) as i64)
            },
            format_rank(pf),
        );
        if best.is_none_or(|(_, k)| key < k) {
            best = Some((pf, key));
        }
    }
    best.map(|(f, _)| f)
}

/// Turn the capture settings into what `choose_format` takes, from the camera's native types (plan 6.1).
///
/// * Device Default (`preferred`): the camera's first native type (of the chosen format) at its highest rate;
///   `frame_interval` is not used.
/// * Custom: the stated size; `frame_interval` -1 follows `output_fps` (the highest rate when the output is "same as
///   the camera"), 0 is the highest rate, anything else is that interval.
/// * `video_format` "any" picks per [`pick_any`].
///
/// `formats` may be empty when the settings name a format and a size (nothing needs the list then).
pub fn resolve_request(
    cap: &CaptureCfg,
    output_fps: Option<u32>,
    formats: &[NativeFormat],
) -> Result<FormatRequest, String> {
    let (size, want_fps): (Option<(u32, u32)>, Option<Ratio>) = match cap.res_type {
        ResType::Preferred => {
            let first = formats.iter().find(|f| match (cap.video_format, f.format) {
                (VideoFormat::Any, Some(p)) => ingestible(p),
                (VideoFormat::Format(want), Some(p)) => want == p,
                _ => false,
            });
            (first.map(|f| (f.width, f.height)), None)
        }
        ResType::Custom => {
            let size = parse_size(&cap.resolution).map_err(|e| format!("resolution: {e}"))?;
            let fps = match cap.frame_interval {
                i if i < 0 => output_fps.map(|f| Ratio::new(f.max(1), 1)),
                0 => None,
                i => Some(ratio_from_interval(i.min(u32::MAX as i64) as u32)),
            };
            (Some(size), fps)
        }
    };
    let format = match cap.video_format {
        VideoFormat::Format(f) => f,
        VideoFormat::Any => {
            let size = size.ok_or_else(|| {
                "the camera lists no format Better Cam can read (no size to start from)".to_string()
            })?;
            pick_any(formats, size, want_fps).ok_or_else(|| {
                let sizes: Vec<String> = group_formats(formats)
                    .iter()
                    .filter(|g| g.format.is_some_and(ingestible))
                    .map(|g| {
                        format!(
                            "{}x{} {}",
                            g.width,
                            g.height,
                            g.format.map_or("?", |f| f.name())
                        )
                    })
                    .collect();
                format!(
                    "camera does not list {}x{} in a format Better Cam can read; it lists: {}",
                    size.0,
                    size.1,
                    sizes.join("; ")
                )
            })?
        }
    };
    Ok(FormatRequest {
        format,
        size,
        fps: want_fps,
    })
}

/// Resolve the settings against the cameras that are present now (opens the camera briefly to read its formats when
/// the settings leave the format or size open). Errors are classified: a missing or busy camera is worth retrying,
/// anything else is not.
pub fn resolve_camera(cap: &CaptureCfg, output_fps: Option<u32>) -> Result<CameraSpec, Failure> {
    let keys = list_camera_keys().map_err(|e| Failure::from_capture(&e))?;
    let key = pick_device(&keys, cap.device.as_deref()).ok_or_else(|| {
        Failure::Retry(
            RetryKind::NotFound,
            if keys.is_empty() {
                "camera not found (no camera is connected)".to_string()
            } else {
                "camera not found".to_string()
            },
        )
    })?;
    let open_ended = cap.res_type == ResType::Preferred || cap.video_format == VideoFormat::Any;
    let formats = if open_ended {
        camera_formats(&key.symbolic_link).map_err(|e| Failure::from_capture(&e))?
    } else {
        Vec::new()
    };
    let request = resolve_request(cap, output_fps, &formats).map_err(Failure::Fatal)?;
    Ok(CameraSpec {
        link: key.symbolic_link.clone(),
        request,
        hw_decode: cap.hw_decode,
    })
}

// ---------------------------------------------------------------------------------------------------------------
// list_cameras payload

/// One entry of the `list_cameras` result.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CameraDto {
    /// The Media Foundation symbolic link: the value of `video_device_id`.
    pub id: String,
    pub name: String,
    pub formats: Vec<FormatDto>,
}

/// A (format, size) pair with every frame interval the camera offers it at.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct FormatDto {
    /// OBS name: `mjpeg`, `nv12`, `yuy2`, ...
    pub format: String,
    pub width: u32,
    pub height: u32,
    /// 100 ns units, ascending (the highest rate first).
    pub intervals: Vec<u64>,
}

/// Rates a camera that states a range (not discrete types) is offered at, inside that range.
const RANGE_RATES: [u32; 9] = [5, 10, 15, 20, 24, 25, 30, 50, 60];

/// The frontend's view of the cameras. Formats Better Cam does not know are left out (they cannot be chosen).
pub fn camera_dtos(cameras: &[CameraInfo]) -> Vec<CameraDto> {
    cameras
        .iter()
        .map(|c| CameraDto {
            id: c.key.symbolic_link.clone(),
            name: c.key.name.clone(),
            formats: group_formats(&c.formats)
                .into_iter()
                .filter_map(|g| {
                    let format = g.format?;
                    let mut rates: Vec<Ratio> = g.fps.clone();
                    if let Some((lo, hi)) = g.range {
                        for n in RANGE_RATES {
                            let r = Ratio::new(n, 1);
                            let in_range = lo.as_f64() - 0.005 <= r.as_f64()
                                && r.as_f64() <= hi.as_f64() + 0.005;
                            if in_range
                                && !rates
                                    .iter()
                                    .any(|x| (x.as_f64() - r.as_f64()).abs() < 0.005)
                            {
                                rates.push(r);
                            }
                        }
                    }
                    let mut intervals: Vec<u64> = rates
                        .into_iter()
                        .map(interval_of)
                        .filter(|i| *i > 0)
                        .collect();
                    intervals.sort_unstable();
                    intervals.dedup();
                    Some(FormatDto {
                        format: format.name().to_string(),
                        width: g.width,
                        height: g.height,
                        intervals,
                    })
                })
                .collect(),
        })
        .collect()
}

/// Enumerate the cameras (opens each briefly to read its formats: call it off the UI thread).
pub fn list_camera_dtos() -> Result<Vec<CameraDto>, String> {
    capture::list_cameras()
        .map(|c| camera_dtos(&c))
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const LINK_A: &str = "\\\\?\\usb#vid_0c45&pid_636d&mi_00#9&3171a958&0&0000#{e5323777-f976-4f5b-9b55-b94699c46e44}\\global";
    const LINK_B: &str = "\\\\?\\usb#vid_046d&pid_085e&mi_00#6&aaaa&0&0003#{e5323777-f976-4f5b-9b55-b94699c46e44}\\global";
    const LINK_A_MOVED: &str = "\\\\?\\usb#vid_0c45&pid_636d&mi_00#6&bbbb&0&0001#{e5323777-f976-4f5b-9b55-b94699c46e44}\\global";

    fn nf(f: PixelFormat, w: u32, h: u32, fps: u32) -> NativeFormat {
        NativeFormat {
            index: 0,
            subtype: f.guid(),
            format: Some(f),
            width: w,
            height: h,
            fps: Ratio::new(fps, 1),
            fps_min: Ratio::new(fps, 1),
            fps_max: Ratio::new(fps, 1),
        }
    }

    /// W4GS-like: NV12 and MJPEG at 1080p/720p up to 60, YUY2 640x480 at 30.
    fn w4gs() -> Vec<NativeFormat> {
        vec![
            nf(PixelFormat::Nv12, 1920, 1080, 60),
            nf(PixelFormat::Nv12, 1920, 1080, 30),
            nf(PixelFormat::Mjpeg, 1920, 1080, 60),
            nf(PixelFormat::Mjpeg, 1920, 1080, 30),
            nf(PixelFormat::Nv12, 1280, 720, 60),
            nf(PixelFormat::Mjpeg, 1280, 720, 60),
            nf(PixelFormat::Mjpeg, 1280, 720, 30),
            nf(PixelFormat::Nv12, 2688, 1520, 15),
            nf(PixelFormat::Mjpeg, 2688, 1520, 30),
            nf(PixelFormat::Yuy2, 640, 480, 30),
        ]
    }

    fn cap(f: impl FnOnce(&mut SourceCfg)) -> CaptureCfg {
        let mut s = SourceCfg::default();
        f(&mut s);
        s.capture()
    }

    #[test]
    fn source_cfg_round_trips_from_frontend_json() {
        let sent = json!({
            "video_device_id": LINK_A,
            "res_type": "custom",
            "resolution": "1280x720",
            "frame_interval": 333333,
            "video_format": "mjpeg",
            "color_space": "709",
            "color_range": "partial",
            "buffering": "off",
            "flip_vertically": true,
            "autorotation": false,
            "hw_decode": true,
            "deactivate_when_not_showing": false
        });
        let cfg: SourceCfg = serde_json::from_value(sent.clone()).unwrap();
        assert_eq!(cfg.video_device_id.as_deref(), Some(LINK_A));
        assert_eq!(cfg.res_type, ResType::Custom);
        assert_eq!(cfg.frame_interval, 333333);
        assert_eq!(cfg.video_format, VideoFormat::Format(PixelFormat::Mjpeg));
        assert_eq!(cfg.color_space, ColorSpaceCfg::Rec709);
        assert_eq!(cfg.color_range, ColorRangeCfg::Partial);
        assert_eq!(cfg.buffering, Buffering::Off);
        assert!(cfg.flip_vertically && !cfg.autorotation && cfg.hw_decode);
        assert!(!cfg.deactivate_when_not_showing);
        assert_eq!(serde_json::to_value(&cfg).unwrap(), sent);

        // every string value of the plan's interface parses and prints back unchanged
        for (key, values) in [
            ("res_type", vec!["preferred", "custom"]),
            (
                "color_space",
                vec!["default", "709", "601", "2100PQ", "2100HLG"],
            ),
            ("color_range", vec!["default", "partial", "full"]),
            ("buffering", vec!["auto", "on", "off"]),
            (
                "video_format",
                vec![
                    "any", "mjpeg", "h264", "hevc", "nv12", "yuy2", "i420", "yv12", "y800", "p010",
                    "yvyu", "uyvy", "hdyc", "xrgb", "argb", "rgb24",
                ],
            ),
        ] {
            for v in values {
                let cfg: SourceCfg = serde_json::from_value(json!({ key: v })).unwrap();
                assert_eq!(
                    serde_json::to_value(&cfg).unwrap()[key],
                    json!(v),
                    "{key} {v}"
                );
            }
        }
        let bad = serde_json::from_value::<SourceCfg>(json!({"video_format": "nope"}));
        assert!(bad
            .unwrap_err()
            .to_string()
            .contains("unknown video_format"));

        // null device and a partial object take the defaults
        let cfg: SourceCfg = serde_json::from_value(json!({"video_device_id": null})).unwrap();
        assert_eq!(cfg, SourceCfg::default());
    }

    #[test]
    fn output_cfg_round_trips_from_frontend_json() {
        let sent = json!({
            "resolution": "640x360",
            "fps": 30,
            "fit": "fill",
            "background_color": 4278190335u32,
            "extra_sizes": false
        });
        let cfg: OutputCfg = serde_json::from_value(sent.clone()).unwrap();
        assert_eq!(cfg.fps, OutputFps::Rate(30));
        assert_eq!(cfg.fit, FitCfg::Fill);
        assert_eq!(cfg.background_color, 0xFF00_00FF);
        assert!(!cfg.extra_sizes);
        assert_eq!(serde_json::to_value(&cfg).unwrap(), sent);

        let src: OutputCfg =
            serde_json::from_value(json!({"resolution": "source", "fps": "source",
            "fit": "fit", "background_color": 4278190080u32, "extra_sizes": true}))
            .unwrap();
        assert_eq!(src, OutputCfg::default());
        assert_eq!(serde_json::to_value(&src).unwrap()["fps"], json!("source"));

        // OBS may store the colour as a signed 32-bit int
        let signed: OutputCfg =
            serde_json::from_value(json!({"background_color": -16777216})).unwrap();
        assert_eq!(signed.background_color, 0xFF00_0000);
        assert!(
            serde_json::from_value::<OutputCfg>(json!({"background_color": 4294967296u64}))
                .is_err()
        );
        assert!(serde_json::from_value::<OutputCfg>(json!({"fps": "fast"})).is_err());
        for n in [15, 24, 25, 30, 50, 60] {
            let c: OutputCfg = serde_json::from_value(json!({ "fps": n })).unwrap();
            assert_eq!(c.fps, OutputFps::Rate(n));
        }
    }

    #[test]
    fn defaults_follow_plan_6() {
        let s = SourceCfg::default();
        assert_eq!(s.video_device_id, None);
        assert_eq!(s.res_type, ResType::Preferred);
        assert_eq!(s.frame_interval, -1);
        assert_eq!(s.video_format, VideoFormat::Any);
        assert_eq!(
            (s.color_space, s.color_range),
            (ColorSpaceCfg::Default, ColorRangeCfg::Default)
        );
        assert_eq!(s.buffering, Buffering::Auto);
        assert!(
            !s.flip_vertically && s.autorotation && s.hw_decode && s.deactivate_when_not_showing
        );
        let o = OutputCfg::default();
        assert_eq!(o.resolution, "source");
        assert_eq!(
            (o.fps, o.fit, o.background_color, o.extra_sizes),
            (OutputFps::Source, FitCfg::Fit, 0xFF00_0000, true)
        );
    }

    #[test]
    fn color_mapping() {
        let mut s = SourceCfg::default();
        assert_eq!(s.color(), ColorSettings::default());
        s.color_space = ColorSpaceCfg::Rec601;
        s.color_range = ColorRangeCfg::Full;
        assert_eq!(
            s.color(),
            ColorSettings {
                space: Some(ColorSpace::Bt601),
                range: Some(ColorRange::Full)
            }
        );
        s.color_space = ColorSpaceCfg::Rec709;
        s.color_range = ColorRangeCfg::Partial;
        assert_eq!(
            s.color(),
            ColorSettings {
                space: Some(ColorSpace::Bt709),
                range: Some(ColorRange::Limited)
            }
        );
        // HDR is accepted and behaves like "default"
        s.color_space = ColorSpaceCfg::Rec2100Pq;
        assert_eq!(s.color().space, None);
    }

    #[test]
    fn preferred_uses_the_first_native_type_at_its_highest_rate() {
        let c = cap(|_| {});
        let r = resolve_request(&c, Some(30), &w4gs()).unwrap();
        // the first type of the list, whatever frame_interval or the output rate say
        assert_eq!(
            r,
            FormatRequest {
                format: PixelFormat::Nv12,
                size: Some((1920, 1080)),
                fps: None
            }
        );
        // an explicit format keeps the first size of that format
        let c = cap(|s| s.video_format = VideoFormat::Format(PixelFormat::Yuy2));
        let r = resolve_request(&c, None, &w4gs()).unwrap();
        assert_eq!(
            (r.format, r.size, r.fps),
            (PixelFormat::Yuy2, Some((640, 480)), None)
        );
        // the first type is skipped when the engine cannot read it
        let list = vec![
            nf(PixelFormat::Y800, 640, 480, 30),
            nf(PixelFormat::Mjpeg, 1280, 720, 30),
        ];
        let r = resolve_request(&cap(|_| {}), None, &list).unwrap();
        assert_eq!((r.format, r.size), (PixelFormat::Mjpeg, Some((1280, 720))));
    }

    #[test]
    fn custom_size_format_and_interval() {
        let c = cap(|s| {
            s.res_type = ResType::Custom;
            s.resolution = "1280x720".into();
            s.frame_interval = 333333;
            s.video_format = VideoFormat::Format(PixelFormat::Mjpeg);
        });
        // nothing needs the camera's list: it can be empty
        let r = resolve_request(&c, None, &[]).unwrap();
        assert_eq!(
            r,
            FormatRequest {
                format: PixelFormat::Mjpeg,
                size: Some((1280, 720)),
                fps: Some(Ratio::new(30, 1))
            }
        );
        assert!(resolve_request(
            &cap(|s| {
                s.res_type = ResType::Custom;
                s.resolution = "wide".into();
            }),
            None,
            &[]
        )
        .is_err());
    }

    #[test]
    fn frame_interval_minus_one_zero_and_exact() {
        let at = |interval: i64, out: Option<u32>| {
            let c = cap(|s| {
                s.res_type = ResType::Custom;
                s.resolution = "1920x1080".into();
                s.video_format = VideoFormat::Format(PixelFormat::Mjpeg);
                s.frame_interval = interval;
            });
            resolve_request(&c, out, &[]).unwrap().fps
        };
        assert_eq!(
            at(-1, Some(25)),
            Some(Ratio::new(25, 1)),
            "-1 follows the output rate"
        );
        assert_eq!(
            at(-1, None),
            None,
            "-1 with an output 'same as the camera' is the highest rate"
        );
        assert_eq!(at(0, Some(25)), None, "0 is the highest rate");
        assert_eq!(at(166667, None), Some(Ratio::new(60, 1)));
        assert_eq!(at(400000, None), Some(Ratio::new(25, 1)));
        assert_eq!(
            at(333667, None),
            Some(Ratio::new(10_000_000, 333667)),
            "29.97 stays exact"
        );
        assert_eq!(at(666667, None), Some(Ratio::new(15, 1)));
        assert_eq!(interval_of(Ratio::new(30, 1)), 333333);
        assert_eq!(interval_of(Ratio::new(60, 1)), 166667);
        assert_eq!(interval_of(Ratio::new(30000, 1001)), 333667);
    }

    #[test]
    fn video_format_any_prefers_raw_when_it_reaches_the_rate() {
        let custom = |size: &str, interval: i64| {
            cap(|s| {
                s.res_type = ResType::Custom;
                s.resolution = size.into();
                s.frame_interval = interval;
            })
        };
        // both reach 60 at 1080p: NV12 (no decoder)
        let r = resolve_request(&custom("1920x1080", 166667), None, &w4gs()).unwrap();
        assert_eq!(r.format, PixelFormat::Nv12);
        // only MJPEG reaches 30 at 2688x1520 (NV12 stops at 15)
        let r = resolve_request(&custom("2688x1520", 333333), None, &w4gs()).unwrap();
        assert_eq!(r.format, PixelFormat::Mjpeg);
        // no rate wanted: the format with the highest rate, then raw
        let r = resolve_request(&custom("2688x1520", 0), None, &w4gs()).unwrap();
        assert_eq!(r.format, PixelFormat::Mjpeg);
        let r = resolve_request(&custom("640x480", -1), Some(30), &w4gs()).unwrap();
        assert_eq!((r.format, r.size), (PixelFormat::Yuy2, Some((640, 480))));
        // a size the camera lacks says what it has
        let e = resolve_request(&custom("800x600", -1), None, &w4gs()).unwrap_err();
        assert!(e.contains("800x600") && e.contains("1920x1080"), "{e}");
    }

    #[test]
    fn output_resolution_source_explicit_and_cap() {
        let s = output_settings(&OutputCfg::default()).unwrap();
        assert_eq!((s.width, s.height, s.fps), (None, None, None));
        assert_eq!(s.fit, FitMode::Fit);
        assert_eq!(s.background, [0, 0, 0]);
        assert!(s.extra_sizes);

        let o = |res: &str| OutputCfg {
            resolution: res.into(),
            ..OutputCfg::default()
        };
        let s = output_settings(&o("640x360")).unwrap();
        assert_eq!((s.width, s.height), (Some(640), Some(360)));
        // capped at 3840x2160, each axis
        let s = output_settings(&o("7680x4320")).unwrap();
        assert_eq!((s.width, s.height), (Some(3840), Some(2160)));
        let s = output_settings(&o("4096x2160")).unwrap();
        assert_eq!((s.width, s.height), (Some(3840), Some(2160)));
        // even, and not below 160x120
        let s = output_settings(&o("1281x721")).unwrap();
        assert_eq!((s.width, s.height), (Some(1280), Some(720)));
        let s = output_settings(&o("64x48")).unwrap();
        assert_eq!((s.width, s.height), (Some(160), Some(120)));
        assert!(output_settings(&o("big"))
            .unwrap_err()
            .contains("output resolution"));
        assert!(output_settings(&o("0x0")).is_err());

        let f = |fps| OutputCfg {
            fps,
            ..OutputCfg::default()
        };
        assert_eq!(
            output_settings(&f(OutputFps::Rate(60))).unwrap().fps,
            Some(60)
        );
        assert!(output_settings(&f(OutputFps::Rate(0))).is_err());
        let fit = |fit| OutputCfg {
            fit,
            ..OutputCfg::default()
        };
        assert_eq!(
            output_settings(&fit(FitCfg::Fill)).unwrap().fit,
            FitMode::Fill
        );
        assert_eq!(
            output_settings(&fit(FitCfg::Stretch)).unwrap().fit,
            FitMode::Stretch
        );
    }

    #[test]
    fn background_is_abgr() {
        assert_eq!(abgr_to_rgb(0xFF00_0000), [0, 0, 0]);
        assert_eq!(
            abgr_to_rgb(0xFF00_00FF),
            [255, 0, 0],
            "OBS red is 0xFF0000FF"
        );
        assert_eq!(abgr_to_rgb(0xFF00_FF00), [0, 255, 0]);
        assert_eq!(abgr_to_rgb(0xFFFF_0000), [0, 0, 255]);
        assert_eq!(
            abgr_to_rgb(0x00A1_B2C3),
            [0xC3, 0xB2, 0xA1],
            "alpha is ignored"
        );
        let s = output_settings(&OutputCfg {
            background_color: 0xFF30_2010,
            ..OutputCfg::default()
        })
        .unwrap();
        assert_eq!(s.background, [0x10, 0x20, 0x30]);
    }

    #[test]
    fn advertised_types_follow_extra_sizes() {
        let with = advertised_list(1920, 1080, 30, true);
        assert!(with.iter().any(|f| f.width == 1280 && f.height == 720));
        assert!(with.iter().any(|f| f.width == 640 && f.height == 360));
        let without = advertised_list(1920, 1080, 30, false);
        assert!(!without.is_empty());
        assert!(without.iter().all(|f| f.width == 1920 && f.height == 1080));
        // the 30 fps entry of a faster output stays
        assert!(advertised_list(1920, 1080, 60, false)
            .iter()
            .any(|f| f.fps() == 30));
    }

    #[test]
    fn device_choice() {
        let keys = vec![
            DeviceKey::new(LINK_A, "W4GS"),
            DeviceKey::new(LINK_B, "Other"),
        ];
        assert_eq!(
            pick_device(&keys, None).unwrap().name,
            "W4GS",
            "null = the first camera"
        );
        assert_eq!(pick_device(&keys, Some("")).unwrap().name, "W4GS");
        assert_eq!(pick_device(&keys, Some(LINK_B)).unwrap().name, "Other");
        // same link without the reference string, other case
        let cm = LINK_B.to_uppercase();
        assert_eq!(
            pick_device(&keys, Some(&cm[..cm.rfind('\\').unwrap()]))
                .unwrap()
                .name,
            "Other"
        );
        // moved to another port: the only camera with that vid/pid
        assert_eq!(pick_device(&keys, Some(LINK_A_MOVED)).unwrap().name, "W4GS");
        // two cameras of one model: no guess
        let twins = vec![
            DeviceKey::new(LINK_A, "A"),
            DeviceKey::new(LINK_A_MOVED, "A"),
        ];
        assert!(pick_device(&twins, Some("\\\\?\\usb#vid_0c45&pid_636d&mi_00#zzzz#{e5323777-f976-4f5b-9b55-b94699c46e44}\\global")).is_none());
        assert!(pick_device(&keys, Some("\\\\?\\usb#vid_dead&pid_beef#x")).is_none());
        assert!(pick_device(&[], None).is_none());
    }

    #[test]
    fn arrivals_are_matched_to_the_wanted_device() {
        assert!(arrival_matches(Some(LINK_A), LINK_A));
        assert!(arrival_matches(Some(LINK_A), LINK_A_MOVED), "same vid/pid");
        assert!(!arrival_matches(Some(LINK_A), LINK_B));
        assert!(arrival_matches(None, LINK_B), "no target: any camera");
        assert!(arrival_matches(Some(""), LINK_B));
    }

    #[test]
    fn camera_list_payload() {
        let mut ranged = nf(PixelFormat::Yuy2, 320, 240, 30);
        ranged.fps_min = Ratio::new(10, 1);
        let mut unknown = nf(PixelFormat::Nv12, 640, 480, 30);
        unknown.format = None;
        let mut formats = w4gs();
        formats.push(ranged);
        formats.push(unknown);
        let cams = vec![CameraInfo {
            key: DeviceKey::new(LINK_A, "W4GS"),
            formats,
            formats_error: None,
        }];
        let dto = camera_dtos(&cams);
        assert_eq!(dto.len(), 1);
        assert_eq!((dto[0].id.as_str(), dto[0].name.as_str()), (LINK_A, "W4GS"));
        let find = |f: &str, w: u32| {
            dto[0]
                .formats
                .iter()
                .find(|x| x.format == f && x.width == w)
                .unwrap()
        };
        assert_eq!(find("nv12", 1920).intervals, vec![166667, 333333]);
        assert_eq!(find("mjpeg", 1280).intervals, vec![166667, 333333]);
        assert_eq!(find("yuy2", 640).intervals, vec![333333]);
        // a range lists the usual rates inside it
        assert_eq!(
            find("yuy2", 320).intervals,
            vec![333333, 400000, 416667, 500000, 666667, 1000000]
        );
        // the unknown subtype is not offered
        assert!(dto[0]
            .formats
            .iter()
            .all(|f| !(f.width == 640 && f.height == 480 && f.format == "nv12")));
        let json = serde_json::to_value(&dto).unwrap();
        assert_eq!(
            json[0]["formats"][0],
            json!({"format": "nv12", "width": 1920, "height": 1080, "intervals": [166667, 333333]})
        );
    }

    #[test]
    fn capture_fields_decide_a_reopen() {
        let base = SourceCfg::default();
        let same_capture = |f: &dyn Fn(&mut SourceCfg)| {
            let mut s = base.clone();
            f(&mut s);
            s.capture() == base.capture()
        };
        // live fields
        assert!(same_capture(&|s| s.color_space = ColorSpaceCfg::Rec709));
        assert!(same_capture(&|s| s.color_range = ColorRangeCfg::Full));
        assert!(same_capture(&|s| s.flip_vertically = true));
        assert!(same_capture(&|s| s.autorotation = false));
        assert!(same_capture(&|s| s.deactivate_when_not_showing = false));
        // capture fields
        assert!(!same_capture(&|s| s.video_device_id = Some("x".into())));
        assert!(!same_capture(&|s| s.res_type = ResType::Custom));
        assert!(!same_capture(&|s| s.resolution = "640x480".into()));
        assert!(!same_capture(&|s| s.frame_interval = 333333));
        assert!(!same_capture(
            &|s| s.video_format = VideoFormat::Format(PixelFormat::Yuy2)
        ));
        assert!(!same_capture(&|s| s.hw_decode = false));
        assert!(!same_capture(&|s| s.buffering = Buffering::On));
    }

    #[test]
    fn validate_rejects_a_bad_custom_size_only() {
        let mut s = SourceCfg {
            resolution: "x".into(),
            ..SourceCfg::default()
        };
        assert!(s.validate().is_ok(), "ignored while the type is preferred");
        s.res_type = ResType::Custom;
        assert!(s.validate().unwrap_err().contains("resolution"));
        s.resolution = "1280x720".into();
        assert!(s.validate().is_ok());
    }
}
