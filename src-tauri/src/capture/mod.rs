//! Camera capture backend (plan 4.1): device enumeration, native formats, device keys, format choice.
//!
//! * [`list_cameras`] walks `MFEnumDeviceSources` and every device's native media types.
//! * [`reader`] opens one camera with a Source Reader in async mode and delivers [`reader::Frame`]s.
//! * [`hotplug`] reports camera arrival and removal (`CM_Register_Notification`).
//! * [`controls`] reads and writes the device controls (brightness, exposure, focus, ...) of an open camera.
//!
//! Threading: call these functions from any thread. Each entry point makes sure the calling thread has COM
//! initialised ([`ensure_com`]) and holds a Media Foundation startup reference ([`MfSession`]) while it works.

pub mod controls;
pub mod hotplug;
pub mod privacy;
pub mod reader;

use std::fmt;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use windows::core::{Error, GUID, HRESULT, PCWSTR, PWSTR};
use windows::Win32::Foundation::E_ACCESSDENIED;
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFAttributes, IMFMediaSource, IMFMediaType, MFCreateAttributes,
    MFCreateDeviceSource, MFCreateSourceReaderFromMediaSource, MFEnumDeviceSources, MFShutdown,
    MFStartup, MFSTARTUP_FULL, MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK, MF_E_HW_MFT_FAILED_START_STREAMING,
    MF_E_NO_MORE_TYPES, MF_E_VIDEO_RECORDING_DEVICE_INVALIDATED,
    MF_E_VIDEO_RECORDING_DEVICE_PREEMPTED, MF_MT_FRAME_RATE, MF_MT_FRAME_RATE_RANGE_MAX,
    MF_MT_FRAME_RATE_RANGE_MIN, MF_MT_FRAME_SIZE, MF_MT_SUBTYPE,
    MF_SOURCE_READER_FIRST_VIDEO_STREAM, MF_VERSION,
};
use windows::Win32::System::Com::{
    CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_MULTITHREADED,
};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

/// Index that selects the first video stream on a Source Reader.
pub(crate) const FIRST_VIDEO_STREAM: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

/// `HRESULT_FROM_WIN32(ERROR_SHARING_VIOLATION)`.
const HR_SHARING_VIOLATION: HRESULT = HRESULT(0x8007_0020_u32 as i32);
/// `HRESULT_FROM_WIN32(ERROR_BUSY)`.
const HR_BUSY: HRESULT = HRESULT(0x8007_00AA_u32 as i32);

// ---------------------------------------------------------------------------------------------------------------
// Errors

/// Everything the capture layer can report.
#[derive(Debug, Clone)]
pub enum CaptureError {
    /// A Media Foundation or Direct3D call failed.
    Api { what: &'static str, error: Error },
    /// The caller tried to open Better Cam's own virtual camera.
    OwnVirtualCamera,
    /// No camera has that index or symbolic link.
    NoSuchDevice(String),
    /// The camera does not list the requested format or size.
    FormatUnavailable {
        wanted: String,
        available: Vec<String>,
    },
    /// Another process holds the camera (see [`classify_hresult`]).
    InUse(Error),
    /// The camera went away (see [`classify_hresult`]).
    DeviceLost(Error),
    /// No first frame or terminal error arrived in time.
    Timeout(&'static str),
}

impl CaptureError {
    /// The HRESULT behind the error, if there is one.
    pub fn hresult(&self) -> Option<HRESULT> {
        match self {
            CaptureError::Api { error, .. }
            | CaptureError::InUse(error)
            | CaptureError::DeviceLost(error) => Some(error.code()),
            _ => None,
        }
    }

    /// Wrap a failed call; in-use and device-lost HRESULTs become their own variants.
    pub(crate) fn from_call(what: &'static str, error: Error) -> CaptureError {
        match classify_hresult(error.code()) {
            ErrorClass::InUse => CaptureError::InUse(error),
            ErrorClass::DeviceLost => CaptureError::DeviceLost(error),
            ErrorClass::Other => CaptureError::Api { what, error },
        }
    }
}

/// `0xC00D3704`-style text for an HRESULT.
pub fn hr_text(hr: HRESULT) -> String {
    format!("0x{:08X}", hr.0 as u32)
}

impl fmt::Display for CaptureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CaptureError::Api { what, error } => write!(
                f,
                "{what} failed: {} ({})",
                error.message(),
                hr_text(error.code())
            ),
            CaptureError::OwnVirtualCamera => write!(
                f,
                "refusing to open Better Cam's own virtual camera (feedback loop)"
            ),
            CaptureError::NoSuchDevice(d) => write!(f, "no camera matches '{d}'"),
            CaptureError::FormatUnavailable { wanted, available } => {
                write!(
                    f,
                    "camera does not list {wanted}; it lists: {}",
                    available.join("; ")
                )
            }
            CaptureError::InUse(e) => {
                write!(
                    f,
                    "camera in use by another app or process: {} ({})",
                    e.message(),
                    hr_text(e.code())
                )
            }
            CaptureError::DeviceLost(e) => {
                write!(f, "device lost: {} ({})", e.message(), hr_text(e.code()))
            }
            CaptureError::Timeout(what) => write!(f, "timed out waiting for {what}"),
        }
    }
}

impl std::error::Error for CaptureError {}

/// Coarse meaning of a capture HRESULT.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorClass {
    /// Another process holds the camera, or took it from us.
    InUse,
    /// The device was unplugged or invalidated.
    DeviceLost,
    Other,
}

/// Map an HRESULT from Media Foundation to [`ErrorClass`].
///
/// In use: `MF_E_HW_MFT_FAILED_START_STREAMING`, `MF_E_VIDEO_RECORDING_DEVICE_PREEMPTED`, `E_ACCESSDENIED`,
/// sharing violation and busy. Lost: `MF_E_VIDEO_RECORDING_DEVICE_INVALIDATED`.
pub fn classify_hresult(hr: HRESULT) -> ErrorClass {
    if hr == MF_E_VIDEO_RECORDING_DEVICE_INVALIDATED {
        ErrorClass::DeviceLost
    } else if hr == MF_E_HW_MFT_FAILED_START_STREAMING
        || hr == MF_E_VIDEO_RECORDING_DEVICE_PREEMPTED
        || hr == E_ACCESSDENIED
        || hr == HR_SHARING_VIOLATION
        || hr == HR_BUSY
    {
        ErrorClass::InUse
    } else {
        ErrorClass::Other
    }
}

// ---------------------------------------------------------------------------------------------------------------
// COM and Media Foundation lifetime

struct ComGuard {
    initialised: bool,
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.initialised {
            // SAFETY: balances the successful CoInitializeEx of this thread.
            unsafe { CoUninitialize() };
        }
    }
}

thread_local! {
    static COM: ComGuard = {
        // SAFETY: plain call. RPC_E_CHANGED_MODE (the thread is already an STA) is fine: Media Foundation works
        // in either apartment, so only a successful init is balanced on thread exit.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        ComGuard { initialised: hr.is_ok() }
    };
}

/// Make sure the calling thread has COM initialised (MTA unless the thread already chose STA). Undone when the
/// thread exits.
pub fn ensure_com() {
    COM.with(|_| {});
}

/// One reference on the process-wide Media Foundation platform (`MFStartup`/`MFShutdown` are reference counted).
#[derive(Debug)]
pub struct MfSession(());

impl MfSession {
    pub fn start() -> Result<MfSession, CaptureError> {
        ensure_com();
        // SAFETY: plain call.
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }
            .map_err(|e| CaptureError::from_call("MFStartup", e))?;
        Ok(MfSession(()))
    }
}

impl Drop for MfSession {
    fn drop(&mut self) {
        // SAFETY: balances the MFStartup in `start`.
        let _ = unsafe { MFShutdown() };
    }
}

/// A COM interface pointer that is safe to move between threads: Media Foundation reader and source objects are
/// free-threaded.
pub(crate) struct Agile<T>(pub T);
// SAFETY: only used for MF objects documented as free-threaded (source reader, media source, media buffers).
unsafe impl<T> Send for Agile<T> {}
unsafe impl<T> Sync for Agile<T> {}

/// Current QPC time in 100 ns units (the clock UVC capture timestamps use).
pub fn qpc_now_100ns() -> i64 {
    static FREQ: OnceLock<i64> = OnceLock::new();
    let freq = *FREQ.get_or_init(|| {
        let mut f = 0i64;
        // SAFETY: out pointer is a local.
        let _ = unsafe { QueryPerformanceFrequency(&mut f) };
        f.max(1)
    });
    let mut now = 0i64;
    // SAFETY: out pointer is a local.
    let _ = unsafe { QueryPerformanceCounter(&mut now) };
    (now as i128 * 10_000_000 / freq as i128) as i64
}

// ---------------------------------------------------------------------------------------------------------------
// Pixel formats

/// `MFVideoFormat_*` subtype for a FOURCC (or a D3DFMT number passed as the first four bytes).
const fn mf_guid(d1: u32) -> GUID {
    GUID::from_u128(((d1 as u128) << 96) | 0x0000_0010_8000_00aa_0038_9b71)
}

const fn fourcc(a: u8, b: u8, c: u8, d: u8) -> GUID {
    mf_guid(a as u32 | (b as u32) << 8 | (c as u32) << 16 | (d as u32) << 24)
}

/// The formats Better Cam can ingest, named as OBS names them (`video_format` setting, plan 5.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PixelFormat {
    Mjpeg,
    H264,
    Hevc,
    Nv12,
    Yuy2,
    I420,
    Yv12,
    Y800,
    P010,
    Yvyu,
    Uyvy,
    Hdyc,
    Xrgb,
    Argb,
    Rgb24,
}

impl PixelFormat {
    pub const ALL: [PixelFormat; 15] = [
        PixelFormat::Mjpeg,
        PixelFormat::H264,
        PixelFormat::Hevc,
        PixelFormat::Nv12,
        PixelFormat::Yuy2,
        PixelFormat::I420,
        PixelFormat::Yv12,
        PixelFormat::Y800,
        PixelFormat::P010,
        PixelFormat::Yvyu,
        PixelFormat::Uyvy,
        PixelFormat::Hdyc,
        PixelFormat::Xrgb,
        PixelFormat::Argb,
        PixelFormat::Rgb24,
    ];

    /// OBS `video_format` name.
    pub fn name(self) -> &'static str {
        match self {
            PixelFormat::Mjpeg => "mjpeg",
            PixelFormat::H264 => "h264",
            PixelFormat::Hevc => "hevc",
            PixelFormat::Nv12 => "nv12",
            PixelFormat::Yuy2 => "yuy2",
            PixelFormat::I420 => "i420",
            PixelFormat::Yv12 => "yv12",
            PixelFormat::Y800 => "y800",
            PixelFormat::P010 => "p010",
            PixelFormat::Yvyu => "yvyu",
            PixelFormat::Uyvy => "uyvy",
            PixelFormat::Hdyc => "hdyc",
            PixelFormat::Xrgb => "xrgb",
            PixelFormat::Argb => "argb",
            PixelFormat::Rgb24 => "rgb24",
        }
    }

    pub fn from_name(name: &str) -> Option<PixelFormat> {
        let n = name.to_ascii_lowercase();
        PixelFormat::ALL.into_iter().find(|f| f.name() == n)
    }

    /// The Media Foundation subtype GUID.
    pub fn guid(self) -> GUID {
        match self {
            PixelFormat::Mjpeg => fourcc(b'M', b'J', b'P', b'G'),
            PixelFormat::H264 => fourcc(b'H', b'2', b'6', b'4'),
            PixelFormat::Hevc => fourcc(b'H', b'E', b'V', b'C'),
            PixelFormat::Nv12 => fourcc(b'N', b'V', b'1', b'2'),
            PixelFormat::Yuy2 => fourcc(b'Y', b'U', b'Y', b'2'),
            PixelFormat::I420 => fourcc(b'I', b'4', b'2', b'0'),
            PixelFormat::Yv12 => fourcc(b'Y', b'V', b'1', b'2'),
            PixelFormat::Y800 => fourcc(b'Y', b'8', b'0', b'0'),
            PixelFormat::P010 => fourcc(b'P', b'0', b'1', b'0'),
            PixelFormat::Yvyu => fourcc(b'Y', b'V', b'Y', b'U'),
            PixelFormat::Uyvy => fourcc(b'U', b'Y', b'V', b'Y'),
            PixelFormat::Hdyc => fourcc(b'H', b'D', b'Y', b'C'),
            PixelFormat::Xrgb => mf_guid(22), // MFVideoFormat_RGB32
            PixelFormat::Argb => mf_guid(21), // MFVideoFormat_ARGB32
            PixelFormat::Rgb24 => mf_guid(20), // MFVideoFormat_RGB24
        }
    }

    /// Map a subtype GUID, including the aliases cameras use (`IYUV` for I420, `L8` for Y800).
    pub fn from_guid(g: &GUID) -> Option<PixelFormat> {
        if let Some(f) = PixelFormat::ALL.into_iter().find(|f| f.guid() == *g) {
            return Some(f);
        }
        if *g == fourcc(b'I', b'Y', b'U', b'V') {
            Some(PixelFormat::I420)
        } else if *g == mf_guid(50) {
            Some(PixelFormat::Y800)
        } else {
            None
        }
    }

    /// MJPEG, H.264 and HEVC need a decoder before pixels exist.
    pub fn is_compressed(self) -> bool {
        matches!(
            self,
            PixelFormat::Mjpeg | PixelFormat::H264 | PixelFormat::Hevc
        )
    }

    /// Bytes per row of the first plane for a tightly packed image (0 for compressed formats).
    pub fn min_stride(self, width: u32) -> u32 {
        match self {
            PixelFormat::Nv12 | PixelFormat::I420 | PixelFormat::Yv12 | PixelFormat::Y800 => width,
            PixelFormat::P010
            | PixelFormat::Yuy2
            | PixelFormat::Yvyu
            | PixelFormat::Uyvy
            | PixelFormat::Hdyc => width * 2,
            PixelFormat::Rgb24 => width * 3,
            PixelFormat::Xrgb | PixelFormat::Argb => width * 4,
            PixelFormat::Mjpeg | PixelFormat::H264 | PixelFormat::Hevc => 0,
        }
    }

    /// Rows of first-plane pitch the whole image occupies (planar 4:2:0 formats add half a plane for chroma).
    pub fn total_rows(self, height: u32) -> u32 {
        match self {
            PixelFormat::Nv12 | PixelFormat::P010 | PixelFormat::I420 | PixelFormat::Yv12 => {
                height + height / 2
            }
            _ => height,
        }
    }
}

impl fmt::Display for PixelFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Printable name for any subtype GUID: the OBS name, else the FOURCC, else the GUID.
pub fn subtype_label(g: &GUID) -> String {
    if let Some(f) = PixelFormat::from_guid(g) {
        return f.name().to_string();
    }
    let is_mf_base = g.data2 == 0
        && g.data3 == 0x0010
        && g.data4 == [0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71];
    if is_mf_base {
        let bytes = g.data1.to_le_bytes();
        if bytes.iter().all(|b| b.is_ascii_graphic() || *b == b' ') {
            return String::from_utf8_lossy(&bytes).trim_end().to_string();
        }
        return format!("d3dfmt{}", g.data1);
    }
    format!("{g:?}")
}

// ---------------------------------------------------------------------------------------------------------------
// Frame rates and native formats

/// A frame rate as MF stores it: numerator over denominator.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Ratio {
    pub num: u32,
    pub den: u32,
}

impl Ratio {
    pub const fn new(num: u32, den: u32) -> Ratio {
        Ratio { num, den }
    }

    pub fn as_f64(self) -> f64 {
        if self.den == 0 {
            0.0
        } else {
            self.num as f64 / self.den as f64
        }
    }

    /// `MFSetAttributeRatio` packing.
    pub fn pack(self) -> u64 {
        (self.num as u64) << 32 | self.den as u64
    }

    pub fn unpack(v: u64) -> Ratio {
        Ratio {
            num: (v >> 32) as u32,
            den: v as u32,
        }
    }

    fn close_to(self, other: Ratio) -> bool {
        (self.as_f64() - other.as_f64()).abs() < 0.005
    }
}

impl fmt::Display for Ratio {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.den == 1 || (self.den != 0 && self.num % self.den == 0) {
            write!(f, "{}", self.num / self.den.max(1))
        } else {
            write!(f, "{:.2}", self.as_f64())
        }
    }
}

/// One native media type of a camera.
#[derive(Clone, Debug, PartialEq)]
pub struct NativeFormat {
    /// Index for `IMFSourceReader::GetNativeMediaType`.
    pub index: u32,
    pub subtype: GUID,
    /// `None` for subtypes Better Cam does not know.
    pub format: Option<PixelFormat>,
    pub width: u32,
    pub height: u32,
    pub fps: Ratio,
    pub fps_min: Ratio,
    pub fps_max: Ratio,
}

impl NativeFormat {
    pub fn from_media_type(index: u32, t: &IMFMediaType) -> NativeFormat {
        // SAFETY: plain attribute reads.
        let (subtype, size, rate, rmin, rmax) = unsafe {
            (
                t.GetGUID(&MF_MT_SUBTYPE).unwrap_or_default(),
                t.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0),
                t.GetUINT64(&MF_MT_FRAME_RATE).ok().map(Ratio::unpack),
                t.GetUINT64(&MF_MT_FRAME_RATE_RANGE_MIN)
                    .ok()
                    .map(Ratio::unpack),
                t.GetUINT64(&MF_MT_FRAME_RATE_RANGE_MAX)
                    .ok()
                    .map(Ratio::unpack),
            )
        };
        let fps = rate.or(rmax).unwrap_or(Ratio::new(0, 1));
        NativeFormat {
            index,
            subtype,
            format: PixelFormat::from_guid(&subtype),
            width: (size >> 32) as u32,
            height: size as u32,
            fps,
            fps_min: rmin.unwrap_or(fps),
            fps_max: rmax.unwrap_or(fps),
        }
    }

    pub fn label(&self) -> String {
        format!(
            "{} {}x{} @ {}",
            subtype_label(&self.subtype),
            self.width,
            self.height,
            self.fps
        )
    }
}

/// All frame rates of one (subtype, size) pair, for display and for the UI's pickers.
#[derive(Clone, Debug, PartialEq)]
pub struct FormatGroup {
    pub subtype: GUID,
    pub format: Option<PixelFormat>,
    pub width: u32,
    pub height: u32,
    /// Distinct rates, highest first.
    pub fps: Vec<Ratio>,
    /// Lowest and highest rate when a native type advertises a range wider than its own rate.
    pub range: Option<(Ratio, Ratio)>,
}

/// Group native types by (subtype, size), keeping first-seen order.
pub fn group_formats(formats: &[NativeFormat]) -> Vec<FormatGroup> {
    let mut groups: Vec<FormatGroup> = Vec::new();
    for f in formats {
        let pos = groups
            .iter()
            .position(|g| g.subtype == f.subtype && g.width == f.width && g.height == f.height);
        let g = match pos {
            Some(i) => &mut groups[i],
            None => {
                groups.push(FormatGroup {
                    subtype: f.subtype,
                    format: f.format,
                    width: f.width,
                    height: f.height,
                    fps: Vec::new(),
                    range: None,
                });
                groups.last_mut().expect("just pushed")
            }
        };
        if !g.fps.iter().any(|r| r.close_to(f.fps)) {
            g.fps.push(f.fps);
        }
        if f.fps_min.as_f64() + 0.005 < f.fps_max.as_f64() {
            g.range = Some(match g.range {
                Some((lo, hi)) => (
                    if f.fps_min.as_f64() < lo.as_f64() {
                        f.fps_min
                    } else {
                        lo
                    },
                    if f.fps_max.as_f64() > hi.as_f64() {
                        f.fps_max
                    } else {
                        hi
                    },
                ),
                None => (f.fps_min, f.fps_max),
            });
        }
    }
    for g in &mut groups {
        g.fps.sort_by(|a, b| b.as_f64().total_cmp(&a.as_f64()));
    }
    groups
}

/// What the caller wants to open.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FormatRequest {
    pub format: PixelFormat,
    /// `None`: the largest size the camera offers in this format.
    pub size: Option<(u32, u32)>,
    /// `None`: the highest rate.
    pub fps: Option<Ratio>,
}

/// The native type picked for a [`FormatRequest`].
#[derive(Clone, Debug, PartialEq)]
pub struct FormatChoice {
    pub native: NativeFormat,
    /// The rate the camera will be asked for.
    pub fps: Ratio,
    /// The requested rate is not what `fps` is (the camera has no such rate); `fps` is the nearest.
    pub fps_approximate: bool,
    /// `fps` lies inside the native type's advertised range but differs from its own rate, so the frame rate
    /// attribute has to be rewritten before the type is selected.
    pub override_fps: bool,
}

/// Pick the native type for `req`: exact subtype and size, then the rate (exact, inside a native range, nearest).
pub fn choose_format(
    formats: &[NativeFormat],
    req: &FormatRequest,
) -> Result<FormatChoice, CaptureError> {
    let same: Vec<&NativeFormat> = formats
        .iter()
        .filter(|f| f.format == Some(req.format))
        .collect();
    let sized: Vec<&NativeFormat> = match req.size {
        Some((w, h)) => same
            .iter()
            .copied()
            .filter(|f| f.width == w && f.height == h)
            .collect(),
        None => {
            let best = same
                .iter()
                .map(|f| f.width as u64 * f.height as u64)
                .max()
                .unwrap_or(0);
            same.iter()
                .copied()
                .filter(|f| f.width as u64 * f.height as u64 == best)
                .collect()
        }
    };
    if sized.is_empty() {
        let wanted = match req.size {
            Some((w, h)) => format!("{} {w}x{h}", req.format),
            None => req.format.to_string(),
        };
        // Keep the message short: the sizes of the requested format if the camera has it, else the formats it has.
        let available: Vec<String> = if same.is_empty() {
            let mut names: Vec<String> = Vec::new();
            for f in formats {
                let n = subtype_label(&f.subtype);
                if !names.contains(&n) {
                    names.push(n);
                }
            }
            names
        } else {
            group_formats(formats)
                .iter()
                .filter(|g| g.format == Some(req.format))
                .map(|g| {
                    let fps: Vec<String> = g.fps.iter().map(|r| r.to_string()).collect();
                    format!("{}x{} @ {}", g.width, g.height, fps.join("/"))
                })
                .collect()
        };
        return Err(CaptureError::FormatUnavailable { wanted, available });
    }

    let top = |f: &NativeFormat| {
        if f.fps_max.as_f64() > f.fps.as_f64() {
            f.fps_max
        } else {
            f.fps
        }
    };
    let pick = |native: &NativeFormat, fps: Ratio, approx: bool| FormatChoice {
        native: native.clone(),
        fps,
        fps_approximate: approx,
        override_fps: !fps.close_to(native.fps),
    };

    let Some(want) = req.fps else {
        let best = sized
            .iter()
            .copied()
            .max_by(|a, b| top(a).as_f64().total_cmp(&top(b).as_f64()))
            .expect("sized is not empty");
        return Ok(pick(best, top(best), false));
    };
    if let Some(f) = sized.iter().copied().find(|f| f.fps.close_to(want)) {
        return Ok(pick(f, f.fps, false));
    }
    if let Some(f) = sized.iter().copied().find(|f| {
        f.fps_min.as_f64() - 0.005 <= want.as_f64() && want.as_f64() <= f.fps_max.as_f64() + 0.005
    }) {
        return Ok(pick(f, want, false));
    }
    let nearest = sized
        .iter()
        .copied()
        .min_by(|a, b| {
            (a.fps.as_f64() - want.as_f64())
                .abs()
                .total_cmp(&(b.fps.as_f64() - want.as_f64()).abs())
        })
        .expect("sized is not empty");
    Ok(pick(nearest, nearest.fps, true))
}

// ---------------------------------------------------------------------------------------------------------------
// Device identity

/// Identity of a physical camera for persistence: the symbolic link, with `vid_XXXX&pid_YYYY` and the friendly
/// name as the fallback when the camera reappears on another USB port (plan 4.1).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeviceKey {
    pub symbolic_link: String,
    /// `vid_0c45&pid_636d`, lower case, when the link carries one.
    pub vid_pid: Option<String>,
    pub name: String,
}

/// How well a stored key matches a present device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyMatch {
    /// Same symbolic link.
    Exact,
    /// Different link (new port) but the same `vid_pid` and friendly name.
    Fallback,
}

impl DeviceKey {
    pub fn new(symbolic_link: &str, name: &str) -> DeviceKey {
        DeviceKey {
            symbolic_link: symbolic_link.to_string(),
            vid_pid: parse_vid_pid(symbolic_link),
            name: name.to_string(),
        }
    }

    /// Compare a stored key (`self`) with a present device's key.
    pub fn matches(&self, present: &DeviceKey) -> Option<KeyMatch> {
        if !self.symbolic_link.is_empty()
            && links_match(&self.symbolic_link, &present.symbolic_link)
        {
            Some(KeyMatch::Exact)
        } else if self.vid_pid == present.vid_pid && self.name.eq_ignore_ascii_case(&present.name) {
            Some(KeyMatch::Fallback)
        } else {
            None
        }
    }
}

/// `vid_0c45&pid_636d` from a USB symbolic link.
pub fn parse_vid_pid(link: &str) -> Option<String> {
    let l = link.to_ascii_lowercase();
    let rest = &l[l.find("vid_")? + 4..];
    let vid = rest.get(..4)?;
    let pid = rest.get(4..)?.strip_prefix("&pid_")?.get(..4)?;
    let hex = |s: &str| s.bytes().all(|b| b.is_ascii_hexdigit());
    (hex(vid) && hex(pid)).then(|| format!("vid_{vid}&pid_{pid}"))
}

/// Lower-case link without the reference string after the interface class GUID (`...#{guid}\global`), so a link
/// from Media Foundation compares equal to one from a configuration manager notification.
pub fn normalize_link(link: &str) -> String {
    let l = link.to_ascii_lowercase();
    match l
        .find("#{")
        .and_then(|i| l[i..].find('}').map(|j| i + j + 1))
    {
        Some(end) => l[..end].to_string(),
        None => l,
    }
}

pub fn links_match(a: &str, b: &str) -> bool {
    normalize_link(a) == normalize_link(b)
}

/// Better Cam's own virtual camera: opening it would feed the output back into the input.
pub fn is_own_virtual_camera(name: &str, link: &str) -> bool {
    name.trim_start()
        .to_ascii_lowercase()
        .starts_with("better cam")
        || link
            .to_ascii_lowercase()
            .starts_with("\\\\?\\swd#vcamdevapi#")
}

/// A physical camera with its native formats.
#[derive(Clone, Debug)]
pub struct CameraInfo {
    pub key: DeviceKey,
    pub formats: Vec<NativeFormat>,
    /// Set when the formats could not be read (camera busy, driver error).
    pub formats_error: Option<String>,
}

/// Find the present camera a stored key refers to; an exact link wins over a fallback match.
pub fn find_camera<'a>(
    cameras: &'a [CameraInfo],
    stored: &DeviceKey,
) -> Option<(&'a CameraInfo, KeyMatch)> {
    cameras
        .iter()
        .find(|c| stored.matches(&c.key) == Some(KeyMatch::Exact))
        .map(|c| (c, KeyMatch::Exact))
        .or_else(|| {
            cameras
                .iter()
                .find(|c| stored.matches(&c.key) == Some(KeyMatch::Fallback))
                .map(|c| (c, KeyMatch::Fallback))
        })
}

// ---------------------------------------------------------------------------------------------------------------
// Enumeration

pub(crate) fn attributes(size: u32) -> Result<IMFAttributes, CaptureError> {
    let mut a = None;
    // SAFETY: out pointer is a local.
    unsafe { MFCreateAttributes(&mut a, size) }
        .map_err(|e| CaptureError::from_call("MFCreateAttributes", e))?;
    a.ok_or(CaptureError::Timeout("MFCreateAttributes result"))
}

fn activate_string(a: &IMFActivate, key: &GUID) -> String {
    let mut p = PWSTR::null();
    let mut len = 0u32;
    // SAFETY: GetAllocatedString allocates with CoTaskMemAlloc; freed below.
    unsafe {
        if a.GetAllocatedString(key, &mut p, &mut len).is_err() || p.is_null() {
            return String::new();
        }
        let s = p.to_string().unwrap_or_default();
        CoTaskMemFree(Some(p.0 as *const _));
        s
    }
}

/// Every video capture device Media Foundation lists as `(name, symbolic link)`, Better Cam's own camera included.
fn enum_all() -> Result<Vec<(String, String)>, CaptureError> {
    let attrs = attributes(1)?;
    // SAFETY: plain calls; the returned array is freed with CoTaskMemFree after every element is taken.
    unsafe {
        attrs
            .SetGUID(
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
            )
            .map_err(|e| CaptureError::from_call("IMFAttributes::SetGUID", e))?;
        let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        MFEnumDeviceSources(&attrs, &mut list, &mut count)
            .map_err(|e| CaptureError::from_call("MFEnumDeviceSources", e))?;
        let mut out = Vec::new();
        for i in 0..count as usize {
            if let Some(a) = std::ptr::read(list.add(i)) {
                out.push((
                    activate_string(&a, &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME),
                    activate_string(&a, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK),
                ));
            }
        }
        if !list.is_null() {
            CoTaskMemFree(Some(list as *const _));
        }
        Ok(out)
    }
}

/// Physical cameras only (Better Cam's virtual camera is filtered out), without reading their formats.
pub fn list_camera_keys() -> Result<Vec<DeviceKey>, CaptureError> {
    let _mf = MfSession::start()?;
    Ok(enum_all()?
        .into_iter()
        .filter(|(name, link)| !is_own_virtual_camera(name, link))
        .map(|(name, link)| DeviceKey::new(&link, &name))
        .collect())
}

/// Physical cameras with their native formats. Reading formats activates each camera briefly.
pub fn list_cameras() -> Result<Vec<CameraInfo>, CaptureError> {
    let _mf = MfSession::start()?;
    let mut out = Vec::new();
    for (name, link) in enum_all()? {
        if is_own_virtual_camera(&name, &link) {
            continue;
        }
        let (formats, formats_error) = match read_native_formats(&link) {
            Ok(f) => (f, None),
            Err(e) => (Vec::new(), Some(e.to_string())),
        };
        out.push(CameraInfo {
            key: DeviceKey::new(&link, &name),
            formats,
            formats_error,
        });
    }
    Ok(out)
}

/// Native formats of one camera, read by opening it briefly (cheaper than [`list_cameras`], which opens them all).
pub fn camera_formats(link: &str) -> Result<Vec<NativeFormat>, CaptureError> {
    let _mf = MfSession::start()?;
    read_native_formats(link)
}

/// Create the media source of the camera with this symbolic link (`MFCreateDeviceSource`).
pub(crate) fn create_source(link: &str) -> Result<IMFMediaSource, CaptureError> {
    if is_own_virtual_camera("", link) {
        return Err(CaptureError::OwnVirtualCamera);
    }
    let attrs = attributes(2)?;
    let wide: Vec<u16> = link.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `wide` outlives the SetString call, which copies the string.
    unsafe {
        attrs
            .SetGUID(
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
            )
            .map_err(|e| CaptureError::from_call("IMFAttributes::SetGUID", e))?;
        attrs
            .SetString(
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
                PCWSTR(wide.as_ptr()),
            )
            .map_err(|e| CaptureError::from_call("IMFAttributes::SetString", e))?;
        MFCreateDeviceSource(&attrs).map_err(|e| CaptureError::from_call("MFCreateDeviceSource", e))
    }
}

/// Walk `GetNativeMediaType` of the first video stream.
fn read_native_formats(link: &str) -> Result<Vec<NativeFormat>, CaptureError> {
    let source = create_source(link)?;
    let result = (|| {
        // SAFETY: plain calls on objects created here.
        let reader =
            unsafe { MFCreateSourceReaderFromMediaSource(&source, None::<&IMFAttributes>) }
                .map_err(|e| CaptureError::from_call("MFCreateSourceReaderFromMediaSource", e))?;
        native_formats(&reader)
    })();
    // SAFETY: plain call; the reader is gone by now.
    let _ = unsafe { source.Shutdown() };
    result
}

/// Native types of the first video stream of `reader`.
pub(crate) fn native_formats(
    reader: &windows::Win32::Media::MediaFoundation::IMFSourceReader,
) -> Result<Vec<NativeFormat>, CaptureError> {
    let mut out = Vec::new();
    for i in 0u32.. {
        // SAFETY: plain call.
        match unsafe { reader.GetNativeMediaType(FIRST_VIDEO_STREAM, i) } {
            Ok(t) => out.push(NativeFormat::from_media_type(i, &t)),
            Err(e) if e.code() == MF_E_NO_MORE_TYPES => break,
            Err(e) => return Err(CaptureError::from_call("GetNativeMediaType", e)),
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const USB_LINK: &str = "\\\\?\\usb#vid_0c45&pid_636d&mi_00#9&3171a958&0&0000#{e5323777-f976-4f5b-9b55-b94699c46e44}\\global";
    const VCAM_LINK: &str = "\\\\?\\swd#vcamdevapi#d8f8cb274e00e6fdbf536ad683ae8e406a97548c1c136231882660f69513bee9#{e5323777-f976-4f5b-9b55-b94699c46e44}\\{fcebba03-9d13-4c13-9940-cc84fcd132d1}";

    fn nf(index: u32, f: PixelFormat, w: u32, h: u32, fps: u32) -> NativeFormat {
        NativeFormat {
            index,
            subtype: f.guid(),
            format: Some(f),
            width: w,
            height: h,
            fps: Ratio::new(fps, 1),
            fps_min: Ratio::new(fps, 1),
            fps_max: Ratio::new(fps, 1),
        }
    }

    #[test]
    fn vid_pid_is_parsed_from_the_link() {
        assert_eq!(
            parse_vid_pid(USB_LINK).as_deref(),
            Some("vid_0c45&pid_636d")
        );
        assert_eq!(
            parse_vid_pid("\\\\?\\USB#VID_046D&PID_085E#abc").as_deref(),
            Some("vid_046d&pid_085e")
        );
        assert_eq!(parse_vid_pid(VCAM_LINK), None);
        assert_eq!(parse_vid_pid("\\\\?\\usb#vid_zzzz&pid_0001#x"), None);
        assert_eq!(parse_vid_pid("vid_0c4"), None);
    }

    #[test]
    fn own_virtual_camera_is_recognised_by_name_or_link() {
        assert!(is_own_virtual_camera(
            "Better Cam (Windows Virtual Camera)",
            USB_LINK
        ));
        assert!(is_own_virtual_camera("something", VCAM_LINK));
        assert!(is_own_virtual_camera("", &VCAM_LINK.to_uppercase()));
        assert!(!is_own_virtual_camera("W4GS", USB_LINK));
        assert!(!is_own_virtual_camera("Logitech Better Cam", USB_LINK));
    }

    #[test]
    fn links_compare_without_reference_string_or_case() {
        let cm = "\\\\?\\USB#VID_0C45&PID_636D&MI_00#9&3171A958&0&0000#{E5323777-F976-4F5B-9B55-B94699C46E44}";
        assert!(links_match(USB_LINK, cm));
        assert!(!links_match(USB_LINK, "\\\\?\\usb#vid_0c45&pid_636d&mi_00#9&other&0&0000#{e5323777-f976-4f5b-9b55-b94699c46e44}\\global"));
        assert!(links_match("plain", "PLAIN"));
    }

    #[test]
    fn device_key_matches_exactly_or_by_vid_pid_and_name() {
        let stored = DeviceKey::new(USB_LINK, "W4GS");
        assert_eq!(stored.vid_pid.as_deref(), Some("vid_0c45&pid_636d"));
        assert_eq!(stored.matches(&stored.clone()), Some(KeyMatch::Exact));

        let other_port = DeviceKey::new(
            "\\\\?\\usb#vid_0c45&pid_636d&mi_00#6&aaaa&0&0003#{e5323777-f976-4f5b-9b55-b94699c46e44}\\global",
            "W4GS",
        );
        assert_eq!(stored.matches(&other_port), Some(KeyMatch::Fallback));

        let other_name = DeviceKey {
            name: "Other".into(),
            ..other_port.clone()
        };
        assert_eq!(stored.matches(&other_name), None);
        let other_model = DeviceKey::new(
            "\\\\?\\usb#vid_046d&pid_085e&mi_00#6&aaaa&0&0003#{e5323777-f976-4f5b-9b55-b94699c46e44}\\global",
            "W4GS",
        );
        assert_eq!(stored.matches(&other_model), None);
    }

    #[test]
    fn find_camera_prefers_exact_over_fallback() {
        let mk = |link: &str| CameraInfo {
            key: DeviceKey::new(link, "W4GS"),
            formats: vec![],
            formats_error: None,
        };
        let moved = "\\\\?\\usb#vid_0c45&pid_636d&mi_00#6&aaaa&0&0003#{e5323777-f976-4f5b-9b55-b94699c46e44}\\global";
        let cams = vec![mk(moved), mk(USB_LINK)];
        let stored = DeviceKey::new(USB_LINK, "W4GS");
        let (found, how) = find_camera(&cams, &stored).unwrap();
        assert_eq!(how, KeyMatch::Exact);
        assert_eq!(found.key.symbolic_link, USB_LINK);
        let (found, how) = find_camera(&cams[..1], &stored).unwrap();
        assert_eq!(how, KeyMatch::Fallback);
        assert_eq!(found.key.symbolic_link, moved);
        assert!(find_camera(&[], &stored).is_none());
    }

    #[test]
    fn pixel_formats_round_trip_and_alias() {
        for f in PixelFormat::ALL {
            assert_eq!(PixelFormat::from_guid(&f.guid()), Some(f), "{f}");
            assert_eq!(PixelFormat::from_name(f.name()), Some(f));
        }
        assert_eq!(PixelFormat::from_name("NV12"), Some(PixelFormat::Nv12));
        assert_eq!(PixelFormat::from_name("nope"), None);
        // Known MF GUIDs written out, so the table cannot drift together with the helper.
        assert_eq!(
            PixelFormat::Nv12.guid(),
            GUID::from_u128(0x3231564e_0000_0010_8000_00aa00389b71)
        );
        assert_eq!(
            PixelFormat::Mjpeg.guid(),
            GUID::from_u128(0x47504a4d_0000_0010_8000_00aa00389b71)
        );
        assert_eq!(
            PixelFormat::Yuy2.guid(),
            GUID::from_u128(0x32595559_0000_0010_8000_00aa00389b71)
        );
        assert_eq!(
            PixelFormat::Xrgb.guid(),
            GUID::from_u128(0x00000016_0000_0010_8000_00aa00389b71)
        );
        assert_eq!(
            PixelFormat::from_guid(&fourcc(b'I', b'Y', b'U', b'V')),
            Some(PixelFormat::I420)
        );
        assert_eq!(
            PixelFormat::from_guid(&mf_guid(50)),
            Some(PixelFormat::Y800)
        );
        assert_eq!(subtype_label(&fourcc(b'Z', b'Z', b'Z', b'Z')), "ZZZZ");
        assert_eq!(subtype_label(&PixelFormat::P010.guid()), "p010");
    }

    #[test]
    fn layout_helpers() {
        assert_eq!(PixelFormat::Nv12.min_stride(1280), 1280);
        assert_eq!(PixelFormat::Yuy2.min_stride(1280), 2560);
        assert_eq!(PixelFormat::Xrgb.min_stride(10), 40);
        assert_eq!(PixelFormat::Nv12.total_rows(720), 1080);
        assert_eq!(PixelFormat::Yuy2.total_rows(720), 720);
        assert!(PixelFormat::Mjpeg.is_compressed() && !PixelFormat::Nv12.is_compressed());
    }

    #[test]
    fn ratio_packs_and_prints() {
        let r = Ratio::new(30000, 1001);
        assert_eq!(Ratio::unpack(r.pack()), r);
        assert_eq!(Ratio::new(30, 1).to_string(), "30");
        assert_eq!(r.to_string(), "29.97");
        assert_eq!(Ratio::new(0, 0).as_f64(), 0.0);
    }

    #[test]
    fn grouping_collects_rates_per_size() {
        let formats = vec![
            nf(0, PixelFormat::Mjpeg, 1280, 720, 30),
            nf(1, PixelFormat::Mjpeg, 1280, 720, 15),
            nf(2, PixelFormat::Mjpeg, 1280, 720, 30),
            nf(3, PixelFormat::Yuy2, 640, 480, 30),
        ];
        let g = group_formats(&formats);
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].fps, vec![Ratio::new(30, 1), Ratio::new(15, 1)]);
        assert_eq!(g[1].format, Some(PixelFormat::Yuy2));
        assert_eq!(g[0].range, None);

        let mut ranged = nf(4, PixelFormat::Nv12, 640, 480, 30);
        ranged.fps_min = Ratio::new(5, 1);
        let g = group_formats(&[ranged]);
        assert_eq!(g[0].range, Some((Ratio::new(5, 1), Ratio::new(30, 1))));
    }

    #[test]
    fn choose_exact_size_and_rate() {
        let formats = vec![
            nf(0, PixelFormat::Mjpeg, 1920, 1080, 30),
            nf(1, PixelFormat::Mjpeg, 1280, 720, 30),
            nf(2, PixelFormat::Mjpeg, 1280, 720, 15),
            nf(3, PixelFormat::Yuy2, 1280, 720, 10),
        ];
        let c = choose_format(
            &formats,
            &FormatRequest {
                format: PixelFormat::Mjpeg,
                size: Some((1280, 720)),
                fps: Some(Ratio::new(15, 1)),
            },
        )
        .unwrap();
        assert_eq!(c.native.index, 2);
        assert!(!c.fps_approximate && !c.override_fps);
    }

    #[test]
    fn choose_defaults_to_largest_size_and_highest_rate() {
        let formats = vec![
            nf(0, PixelFormat::Mjpeg, 1280, 720, 60),
            nf(1, PixelFormat::Mjpeg, 1920, 1080, 15),
            nf(2, PixelFormat::Mjpeg, 1920, 1080, 30),
        ];
        let c = choose_format(
            &formats,
            &FormatRequest {
                format: PixelFormat::Mjpeg,
                size: None,
                fps: None,
            },
        )
        .unwrap();
        assert_eq!(c.native.index, 2);
        assert_eq!(c.fps, Ratio::new(30, 1));
    }

    #[test]
    fn choose_uses_a_native_range_and_overrides_the_rate() {
        let mut t = nf(0, PixelFormat::Nv12, 640, 480, 30);
        t.fps_min = Ratio::new(5, 1);
        let c = choose_format(
            &[t],
            &FormatRequest {
                format: PixelFormat::Nv12,
                size: Some((640, 480)),
                fps: Some(Ratio::new(24, 1)),
            },
        )
        .unwrap();
        assert_eq!(c.fps, Ratio::new(24, 1));
        assert!(c.override_fps && !c.fps_approximate);
    }

    #[test]
    fn choose_falls_back_to_the_nearest_rate_and_says_so() {
        let formats = vec![
            nf(0, PixelFormat::Mjpeg, 1280, 720, 30),
            nf(1, PixelFormat::Mjpeg, 1280, 720, 15),
        ];
        let c = choose_format(
            &formats,
            &FormatRequest {
                format: PixelFormat::Mjpeg,
                size: Some((1280, 720)),
                fps: Some(Ratio::new(25, 1)),
            },
        )
        .unwrap();
        assert_eq!(c.native.index, 0);
        assert!(c.fps_approximate);
    }

    #[test]
    fn choose_reports_what_the_camera_does_offer() {
        let formats = vec![
            nf(0, PixelFormat::Yuy2, 640, 480, 30),
            nf(1, PixelFormat::Yuy2, 640, 480, 15),
            nf(2, PixelFormat::Nv12, 320, 240, 30),
        ];
        // format missing: list the formats
        let err = choose_format(
            &formats,
            &FormatRequest {
                format: PixelFormat::Mjpeg,
                size: Some((1280, 720)),
                fps: None,
            },
        )
        .unwrap_err();
        match err {
            CaptureError::FormatUnavailable { wanted, available } => {
                assert_eq!(wanted, "mjpeg 1280x720");
                assert_eq!(available, vec!["yuy2".to_string(), "nv12".to_string()]);
            }
            other => panic!("unexpected {other:?}"),
        }
        // size missing: list the sizes of that format
        let err = choose_format(
            &formats,
            &FormatRequest {
                format: PixelFormat::Yuy2,
                size: Some((1280, 720)),
                fps: None,
            },
        )
        .unwrap_err();
        match err {
            CaptureError::FormatUnavailable { available, .. } => {
                assert_eq!(available, vec!["640x480 @ 30/15".to_string()])
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn hresults_are_classified() {
        assert_eq!(
            classify_hresult(MF_E_VIDEO_RECORDING_DEVICE_INVALIDATED),
            ErrorClass::DeviceLost
        );
        assert_eq!(
            classify_hresult(MF_E_HW_MFT_FAILED_START_STREAMING),
            ErrorClass::InUse
        );
        assert_eq!(
            classify_hresult(MF_E_VIDEO_RECORDING_DEVICE_PREEMPTED),
            ErrorClass::InUse
        );
        assert_eq!(classify_hresult(E_ACCESSDENIED), ErrorClass::InUse);
        assert_eq!(classify_hresult(HR_SHARING_VIOLATION), ErrorClass::InUse);
        assert_eq!(
            classify_hresult(HRESULT(0x8000_4005_u32 as i32)),
            ErrorClass::Other
        );
        assert_eq!(hr_text(MF_E_HW_MFT_FAILED_START_STREAMING), "0xC00D3704");
        assert!(matches!(
            CaptureError::from_call("x", Error::from(MF_E_VIDEO_RECORDING_DEVICE_INVALIDATED)),
            CaptureError::DeviceLost(_)
        ));
    }

    #[test]
    fn qpc_clock_advances() {
        let a = qpc_now_100ns();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let b = qpc_now_100ns();
        assert!(b - a >= 150_000, "{} ticks of 100 ns in 20 ms", b - a);
    }

    #[test]
    fn enumeration_runs_and_hides_the_virtual_camera() {
        // Works with zero, one or several cameras.
        let keys = list_camera_keys().expect("enumeration");
        for k in &keys {
            assert!(!is_own_virtual_camera(&k.name, &k.symbolic_link), "{k:?}");
        }
    }
}
