//! Media types advertised by the stream: the list the app wrote into the header of `frames.bin`, or NV12 and RGB32
//! at 1920x1080 and 1280x720, 30 fps, when there is no valid file.

use vcam_protocol::{FPS, SUBTYPE_NV12, SUBTYPE_RGB32};
use windows::Win32::Media::MediaFoundation::{
    IMFMediaType, MFCreateMediaType, MFMediaType_Video, MFNominalRange_16_235, MFVideoFormat_NV12,
    MFVideoFormat_RGB32, MFVideoPrimaries_BT709, MFVideoTransFunc_709, MFVideoTransferMatrix_BT709,
    MF_MT_ALL_SAMPLES_INDEPENDENT, MF_MT_AVG_BITRATE, MF_MT_DEFAULT_STRIDE,
    MF_MT_FIXED_SIZE_SAMPLES, MF_MT_FRAME_RATE, MF_MT_FRAME_RATE_RANGE_MAX,
    MF_MT_FRAME_RATE_RANGE_MIN, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE,
    MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SAMPLE_SIZE, MF_MT_SUBTYPE, MF_MT_TRANSFER_FUNCTION,
    MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_VIDEO_PRIMARIES, MF_MT_YUV_MATRIX,
};
use windows_core::{Result, GUID};

/// `MFVideoInterlace_Progressive`.
const INTERLACE_PROGRESSIVE: u32 = 2;
/// FOURCC `NV12`, the `dwFourCC` argument of `MFCreate2DMediaBuffer`.
pub const FOURCC_NV12: u32 = 0x3231_564E;
/// `D3DFMT_X8R8G8B8`, the `dwFourCC` argument of `MFCreate2DMediaBuffer` for RGB32.
pub const D3DFMT_X8R8G8B8: u32 = 22;

/// Pixel layout of a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    Nv12,
    Rgb32,
}

impl PixelFormat {
    pub fn subtype(self) -> GUID {
        match self {
            PixelFormat::Nv12 => MFVideoFormat_NV12,
            PixelFormat::Rgb32 => MFVideoFormat_RGB32,
        }
    }
    pub fn from_subtype(g: &GUID) -> Option<Self> {
        if *g == MFVideoFormat_NV12 {
            Some(PixelFormat::Nv12)
        } else if *g == MFVideoFormat_RGB32 {
            Some(PixelFormat::Rgb32)
        } else {
            None
        }
    }
    /// The `subtype` value of `frames.bin`.
    pub fn protocol_subtype(self) -> u32 {
        match self {
            PixelFormat::Nv12 => SUBTYPE_NV12,
            PixelFormat::Rgb32 => SUBTYPE_RGB32,
        }
    }
    pub fn from_protocol_subtype(s: u32) -> Option<Self> {
        match s {
            SUBTYPE_NV12 => Some(PixelFormat::Nv12),
            SUBTYPE_RGB32 => Some(PixelFormat::Rgb32),
            _ => None,
        }
    }
    /// `dwFourCC` for `MFCreate2DMediaBuffer`.
    pub fn buffer_fourcc(self) -> u32 {
        match self {
            PixelFormat::Nv12 => FOURCC_NV12,
            PixelFormat::Rgb32 => D3DFMT_X8R8G8B8,
        }
    }
    /// Bytes of one frame with a tight pitch.
    pub fn frame_bytes(self, w: u32, h: u32) -> u32 {
        match self {
            PixelFormat::Nv12 => w * h * 3 / 2,
            PixelFormat::Rgb32 => w * h * 4,
        }
    }
    pub fn stride(self, w: u32) -> u32 {
        match self {
            PixelFormat::Nv12 => w,
            PixelFormat::Rgb32 => w * 4,
        }
    }
}

/// Format, size and frame rate of a stream, read back from an `IMFMediaType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameFormat {
    pub pixel: PixelFormat,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

impl FrameFormat {
    /// Duration of one frame in 100 ns units.
    pub fn frame_duration(&self) -> i64 {
        10_000_000 / self.fps as i64
    }

    pub fn from_media_type(t: &IMFMediaType) -> Result<Self> {
        let subtype = unsafe { t.GetGUID(&MF_MT_SUBTYPE)? };
        let pixel = PixelFormat::from_subtype(&subtype).ok_or_else(|| {
            windows_core::Error::from(windows::Win32::Media::MediaFoundation::MF_E_INVALIDMEDIATYPE)
        })?;
        let size = unsafe { t.GetUINT64(&MF_MT_FRAME_SIZE)? };
        let rate = unsafe {
            t.GetUINT64(&MF_MT_FRAME_RATE)
                .unwrap_or(((FPS as u64) << 32) | 1)
        };
        let (num, den) = ((rate >> 32) as u32, (rate & 0xFFFF_FFFF) as u32);
        Ok(FrameFormat {
            pixel,
            width: (size >> 32) as u32,
            height: (size & 0xFFFF_FFFF) as u32,
            fps: if den == 0 || num == 0 {
                FPS
            } else {
                (num + den / 2) / den
            },
        })
    }
}

fn pack(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | lo as u64
}

/// Build one media type.
pub fn build_media_type(
    pixel: PixelFormat,
    width: u32,
    height: u32,
    fps: u32,
) -> Result<IMFMediaType> {
    let t = unsafe { MFCreateMediaType()? };
    unsafe {
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetGUID(&MF_MT_SUBTYPE, &pixel.subtype())?;
        t.SetUINT32(&MF_MT_INTERLACE_MODE, INTERLACE_PROGRESSIVE)?;
        t.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)?;
        t.SetUINT32(&MF_MT_FIXED_SIZE_SAMPLES, 1)?;
        t.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height))?;
        t.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
        t.SetUINT64(&MF_MT_FRAME_RATE_RANGE_MAX, pack(fps, 1))?;
        t.SetUINT64(&MF_MT_FRAME_RATE_RANGE_MIN, pack(fps, 1))?;
        t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
        t.SetUINT32(&MF_MT_DEFAULT_STRIDE, pixel.stride(width))?;
        t.SetUINT32(&MF_MT_SAMPLE_SIZE, pixel.frame_bytes(width, height))?;
        let bits = pixel.frame_bytes(width, height) as u64 * 8 * fps as u64;
        t.SetUINT32(&MF_MT_AVG_BITRATE, bits.min(u32::MAX as u64) as u32)?;
        if pixel == PixelFormat::Nv12 {
            t.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
            t.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)?;
            t.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)?;
            t.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
        }
    }
    Ok(t)
}

/// Every advertised type, preferred (default) first. Read from the header of `frames.bin` (one plain read);
/// without a valid file: NV12 1080p, NV12 720p, RGB32 1080p, RGB32 720p at 30 fps.
pub fn advertised_types() -> Result<Vec<IMFMediaType>> {
    let mut out = Vec::new();
    for f in crate::frames::advertised() {
        if let Some(pixel) = PixelFormat::from_protocol_subtype(f.subtype) {
            out.push(build_media_type(pixel, f.width, f.height, f.fps())?);
        }
    }
    if out.is_empty() {
        for f in vcam_protocol::default_advertised() {
            if let Some(pixel) = PixelFormat::from_protocol_subtype(f.subtype) {
                out.push(build_media_type(pixel, f.width, f.height, FPS)?);
            }
        }
    }
    Ok(out)
}

/// A format a stream may be started with: whatever Frame Server picked from what we advertised, but never
/// something the protocol would reject.
pub fn is_sane(f: &FrameFormat) -> bool {
    vcam_protocol::validate_format(f.width, f.height, f.pixel.protocol_subtype()).is_ok()
        && (1..=240).contains(&f.fps)
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Media::MediaFoundation::{MFStartup, MFSTARTUP_FULL, MF_VERSION};

    pub fn mf_init() {
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL).unwrap() };
    }

    #[test]
    fn four_types_in_preferred_order() {
        mf_init();
        let types = advertised_types().unwrap();
        assert_eq!(types.len(), 4);
        let fmts: Vec<_> = types
            .iter()
            .map(|t| FrameFormat::from_media_type(t).unwrap())
            .collect();
        assert_eq!(
            fmts[0],
            FrameFormat {
                pixel: PixelFormat::Nv12,
                width: 1920,
                height: 1080,
                fps: 30
            }
        );
        assert_eq!(fmts[1].width, 1280);
        assert_eq!(fmts[2].pixel, PixelFormat::Rgb32);
        assert_eq!(fmts[3].height, 720);
    }

    #[test]
    fn attributes_are_set() {
        mf_init();
        let t = build_media_type(PixelFormat::Nv12, 1280, 720, 30).unwrap();
        unsafe {
            assert_eq!(t.GetUINT32(&MF_MT_DEFAULT_STRIDE).unwrap(), 1280);
            assert_eq!(t.GetUINT32(&MF_MT_SAMPLE_SIZE).unwrap(), 1280 * 720 * 3 / 2);
            assert_eq!(t.GetUINT32(&MF_MT_INTERLACE_MODE).unwrap(), 2);
            assert_eq!(t.GetUINT64(&MF_MT_FRAME_RATE).unwrap(), (30u64 << 32) | 1);
        }
        let t = build_media_type(PixelFormat::Rgb32, 1920, 1080, 30).unwrap();
        unsafe {
            assert_eq!(t.GetUINT32(&MF_MT_DEFAULT_STRIDE).unwrap(), 1920 * 4);
            assert!(t.GetUINT32(&MF_MT_YUV_MATRIX).is_err());
        }
    }
}
