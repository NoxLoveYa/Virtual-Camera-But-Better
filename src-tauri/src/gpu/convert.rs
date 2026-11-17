//! Input convert (plan 4.2 stage 1): camera frames in NV12, YUY2, I420/YV12, P010 or BGRA/BGRX become one
//! `R8G8B8A8_UNORM` texture, honouring colour space (BT.601 / BT.709), range (limited / full) and a vertical flip.
//!
//! The matrices are computed from the BT.601 and BT.709 luma weights and the 8 or 10 bit range definitions
//! (compare `libobs/media-io/video-matrices.c` of OBS Studio, which tabulates the same numbers); the shaders read
//! raw samples and apply one 3x4 matrix, like `libobs/data/format_conversion.effect`.
//!
//! Frames come in two shapes: a decoder/camera texture ([`SourceData::Texture`], copied with
//! `CopySubresourceRegion` into a texture of our own because decoder surfaces are not shader readable) and system
//! memory ([`SourceData::Cpu`], uploaded plane by plane with `UpdateSubresource`, so any row pitch works).

use std::sync::Arc;

use windows::Win32::Graphics::Direct3D11::{
    ID3D11ShaderResourceView, ID3D11Texture2D, D3D11_BIND_SHADER_RESOURCE, D3D11_BOX,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_FORMAT_P010,
    DXGI_FORMAT_R16G16_UNORM, DXGI_FORMAT_R16_UNORM, DXGI_FORMAT_R8G8B8A8_UNORM,
    DXGI_FORMAT_R8G8_UNORM, DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_YUY2, DXGI_SAMPLE_DESC,
};

use super::{ConstantBuffer, GpuError, GpuResult, Pass, Pooled, Renderer, ShaderId};
use crate::capture::PixelFormat;

// ---------------------------------------------------------------------------------------------------------------
// Colour matrices

/// YCbCr matrix of the source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorSpace {
    Bt601,
    Bt709,
}

impl ColorSpace {
    /// Luma weights `(Kr, Kb)`.
    pub fn weights(self) -> (f64, f64) {
        match self {
            ColorSpace::Bt601 => (0.299, 0.114),
            ColorSpace::Bt709 => (0.2126, 0.0722),
        }
    }

    /// Plan 6.1: Rec. 709 from 720 lines up, Rec. 601 below, when the camera states no matrix.
    pub fn default_for_height(height: u32) -> ColorSpace {
        if height >= 720 {
            ColorSpace::Bt709
        } else {
            ColorSpace::Bt601
        }
    }
}

/// Sample range of the source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorRange {
    /// 16-235 luma, 16-240 chroma (scaled for 10 bit).
    Limited,
    /// 0-255.
    Full,
}

/// Overrides for the plan 6.1 default rules (`None` = use the default; the engine fills in what the camera's media
/// type states: `MF_MT_YUV_MATRIX`, `MF_MT_VIDEO_NOMINAL_RANGE`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ColorSettings {
    pub space: Option<ColorSpace>,
    pub range: Option<ColorRange>,
}

impl ColorSettings {
    /// The effective matrix and range for a YUV frame of this height.
    pub fn resolve(&self, height: u32) -> (ColorSpace, ColorRange) {
        (
            self.space
                .unwrap_or_else(|| ColorSpace::default_for_height(height)),
            self.range.unwrap_or(ColorRange::Limited),
        )
    }
}

/// Offsets and spans, in fractions of the full code range, of a range definition at `bits` per sample.
struct RangeDef {
    y_off: f64,
    y_span: f64,
    c_zero: f64,
    c_span: f64,
}

fn range_def(range: ColorRange, bits: u32) -> RangeDef {
    let max = ((1u32 << bits) - 1) as f64;
    let k = (1u32 << (bits - 8)) as f64;
    match range {
        ColorRange::Limited => RangeDef {
            y_off: 16.0 * k / max,
            y_span: 219.0 * k / max,
            c_zero: 128.0 * k / max,
            c_span: 224.0 * k / max,
        },
        ColorRange::Full => RangeDef {
            y_off: 0.0,
            y_span: 1.0,
            c_zero: 128.0 * k / max,
            c_span: 1.0,
        },
    }
}

/// Rows `[a_y, a_u, a_v, c]` of the matrix that turns normalised samples into normalised RGB:
/// `R = dot(rows[0], [y, u, v, 1])`, `G = dot(rows[1], ..)`, `B = dot(rows[2], ..)`. `bits` is 8 or 10.
pub fn yuv_to_rgb_rows(space: ColorSpace, range: ColorRange, bits: u32) -> [[f64; 4]; 3] {
    let (kr, kb) = space.weights();
    let kg = 1.0 - kr - kb;
    let d = range_def(range, bits);
    // y' = (y - y_off) / y_span, pb = (u - c_zero) / c_span, pr = (v - c_zero) / c_span
    let (ay, au, av) = (1.0 / d.y_span, 1.0 / d.c_span, 1.0 / d.c_span);
    let (oy, ou, ov) = (
        -d.y_off / d.y_span,
        -d.c_zero / d.c_span,
        -d.c_zero / d.c_span,
    );
    let cr = 2.0 * (1.0 - kr);
    let cb = 2.0 * (1.0 - kb);
    let g_pb = -cb * kb / kg;
    let g_pr = -cr * kr / kg;
    [
        [ay, 0.0, cr * av, oy + cr * ov],
        [ay, g_pb * au, g_pr * av, oy + g_pb * ou + g_pr * ov],
        [ay, cb * au, 0.0, oy + cb * ou],
    ]
}

/// Rows of the inverse matrix: `Y = dot(rows[0], [r, g, b, 1])`, then `Cb`, `Cr`, as normalised codes of an 8 bit
/// signal.
pub fn rgb_to_yuv_rows(space: ColorSpace, range: ColorRange) -> [[f64; 4]; 3] {
    let (kr, kb) = space.weights();
    let kg = 1.0 - kr - kb;
    let d = range_def(range, 8);
    let ys = d.y_span;
    let (ub, vr) = (d.c_span / (2.0 * (1.0 - kb)), d.c_span / (2.0 * (1.0 - kr)));
    [
        [ys * kr, ys * kg, ys * kb, d.y_off],
        [-ub * kr, -ub * kg, ub * (1.0 - kb), d.c_zero],
        [vr * (1.0 - kr), -vr * kg, -vr * kb, d.c_zero],
    ]
}

fn rows_f32(rows: [[f64; 4]; 3]) -> [[f32; 4]; 3] {
    rows.map(|r| r.map(|v| v as f32))
}

// ---------------------------------------------------------------------------------------------------------------
// Source frames

/// Where the pixels are.
pub enum SourceData<'a> {
    /// A decoder or camera surface; `subresource` is the array slice. It need not be shader readable.
    Texture {
        texture: &'a ID3D11Texture2D,
        subresource: u32,
    },
    /// System memory. `stride` is the byte distance between rows of the first plane; negative means bottom-up
    /// (`data` then starts with the last row, like `CpuBuffer::data` of the capture layer).
    Cpu { data: &'a [u8], stride: i32 },
}

/// One frame to convert.
pub struct SourceFrame<'a> {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub data: SourceData<'a>,
    pub color: ColorSettings,
    /// Rows of the first plane in the buffer, when it holds more than `height` (padding before the chroma plane of
    /// planar formats). 0 means `height`.
    pub luma_rows: u32,
}

impl<'a> SourceFrame<'a> {
    pub fn cpu(
        width: u32,
        height: u32,
        format: PixelFormat,
        data: &'a [u8],
        stride: i32,
    ) -> SourceFrame<'a> {
        SourceFrame {
            width,
            height,
            format,
            data: SourceData::Cpu { data, stride },
            color: ColorSettings::default(),
            luma_rows: 0,
        }
    }
}

/// Formats [`Converter`] accepts.
pub fn is_supported(format: PixelFormat) -> bool {
    matches!(
        format,
        PixelFormat::Nv12
            | PixelFormat::Yuy2
            | PixelFormat::I420
            | PixelFormat::Yv12
            | PixelFormat::P010
            | PixelFormat::Xrgb
            | PixelFormat::Argb
    )
}

// ---------------------------------------------------------------------------------------------------------------
// Converter

/// Constant buffer of the convert shaders (`convert_common.hlsl`).
#[repr(C)]
#[derive(Clone, Copy)]
struct ConvertParams {
    row_r: [f32; 4],
    row_g: [f32; 4],
    row_b: [f32; 4],
    size: [u32; 2],
    flip: u32,
    opaque: u32,
    sample_scale: f32,
    pad: [f32; 3],
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct InputKey {
    format: PixelFormat,
    width: u32,
    height: u32,
    gpu: bool,
}

/// The textures shaders read, reused while format and size stay the same.
struct Inputs {
    key: InputKey,
    /// Textures the frame is uploaded or copied into (one per plane for CPU frames, one for GPU frames).
    textures: Vec<ID3D11Texture2D>,
    /// What the shader binds from slot 0.
    srvs: Vec<Option<ID3D11ShaderResourceView>>,
}

/// One plane of a CPU frame: where it starts and how a texture row maps onto it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlaneLayout {
    pub offset: usize,
    pub pitch: usize,
    pub rows: usize,
    pub row_bytes: usize,
}

/// Plane offsets of a CPU frame. `luma_rows` is the first plane's row count in the buffer (>= height).
///
/// Planar 4:2:0 buffers hold the chroma after `stride * luma_rows` bytes of luma; I420 and YV12 chroma planes use half
/// the stride.
pub fn cpu_layout(
    format: PixelFormat,
    width: u32,
    height: u32,
    stride: usize,
    luma_rows: u32,
) -> GpuResult<Vec<PlaneLayout>> {
    let (w, h) = (width as usize, height as usize);
    let lr = (luma_rows.max(height)) as usize;
    let plane = |offset, pitch, rows, row_bytes| PlaneLayout {
        offset,
        pitch,
        rows,
        row_bytes,
    };
    Ok(match format {
        PixelFormat::Nv12 => vec![plane(0, stride, h, w), plane(stride * lr, stride, h / 2, w)],
        PixelFormat::P010 => vec![
            plane(0, stride, h, w * 2),
            plane(stride * lr, stride, h / 2, w * 2),
        ],
        PixelFormat::I420 | PixelFormat::Yv12 => {
            let cs = stride / 2;
            let first = stride * lr;
            let second = first + cs * (lr / 2);
            vec![
                plane(0, stride, h, w),
                plane(first, cs, h / 2, w / 2),
                plane(second, cs, h / 2, w / 2),
            ]
        }
        PixelFormat::Yuy2 => vec![plane(0, stride, h, w * 2)],
        PixelFormat::Xrgb | PixelFormat::Argb => vec![plane(0, stride, h, w * 4)],
        other => {
            return Err(GpuError::Unsupported(format!(
                "pixel format {other} has no converter"
            )))
        }
    })
}

/// Converts [`SourceFrame`]s into pooled RGBA textures. Owns the input textures and the constant buffer; one per
/// render thread.
pub struct Converter {
    renderer: Arc<Renderer>,
    constants: ConstantBuffer,
    inputs: Option<Inputs>,
}

impl Converter {
    pub fn new(renderer: Arc<Renderer>) -> GpuResult<Converter> {
        let constants = renderer.constants(std::mem::size_of::<ConvertParams>())?;
        Ok(Converter {
            renderer,
            constants,
            inputs: None,
        })
    }

    /// Convert `src` into a `width` x `height` RGBA texture from the pool. `flip` mirrors the image vertically.
    pub fn convert(&mut self, src: &SourceFrame<'_>, flip: bool) -> GpuResult<Pooled> {
        let (w, h) = (src.width, src.height);
        if !is_supported(src.format) {
            return Err(GpuError::Unsupported(format!(
                "no converter for pixel format {}",
                src.format
            )));
        }
        let yuv420 = matches!(
            src.format,
            PixelFormat::Nv12 | PixelFormat::P010 | PixelFormat::I420 | PixelFormat::Yv12
        );
        if w == 0
            || h == 0
            || (yuv420 && (w % 2 != 0 || h % 2 != 0))
            || (src.format == PixelFormat::Yuy2 && w % 2 != 0)
        {
            return Err(GpuError::Invalid(format!(
                "{} frame of {w}x{h}: sizes must be non-zero and even for subsampled formats",
                src.format
            )));
        }
        let gpu_frame = matches!(src.data, SourceData::Texture { .. });
        let key = InputKey {
            format: src.format,
            width: w,
            height: h,
            gpu: gpu_frame,
        };
        if self.inputs.as_ref().map(|i| i.key) != Some(key) {
            self.inputs = None; // release the old textures first
            self.inputs = Some(self.make_inputs(key)?);
        }
        let mut flip = flip;
        match &src.data {
            SourceData::Texture {
                texture,
                subresource,
            } => self.copy_texture(texture, *subresource, w, h)?,
            SourceData::Cpu { data, stride } => {
                // A bottom-up image uploaded as stored is upside down; the shader reads it flipped.
                if *stride < 0 {
                    if yuv420 {
                        return Err(GpuError::Unsupported(
                            "planar frame with a negative stride".into(),
                        ));
                    }
                    flip = !flip;
                }
                self.upload(src, data, stride.unsigned_abs() as usize)?;
            }
        }

        let (shader, bits, scale) = match src.format {
            PixelFormat::Nv12 => (ShaderId::ConvertNv12, 8, 1.0),
            PixelFormat::P010 => (ShaderId::ConvertP010, 10, 65535.0 / 65472.0),
            PixelFormat::Yuy2 => (ShaderId::ConvertYuy2, 8, 1.0),
            PixelFormat::I420 | PixelFormat::Yv12 => (ShaderId::ConvertI420, 8, 1.0),
            _ => (ShaderId::ConvertRgb, 8, 1.0),
        };
        let (space, range) = src.color.resolve(h);
        let rows = rows_f32(yuv_to_rgb_rows(space, range, bits));
        let params = ConvertParams {
            row_r: rows[0],
            row_g: rows[1],
            row_b: rows[2],
            size: [w, h],
            flip: flip as u32,
            opaque: (src.format != PixelFormat::Argb) as u32,
            sample_scale: scale,
            pad: [0.0; 3],
        };
        let r = &self.renderer;
        self.constants.write(&r.gpu.context, &params)?;
        let ps = r.shaders.pixel(shader, "main")?;
        let out = r.texture(w, h)?;
        let inputs = self.inputs.as_ref().expect("inputs were just made");
        r.draw(&Pass {
            ps: &ps,
            srvs: &inputs.srvs,
            constants: Some(&self.constants),
            target: &out.rtv,
            width: w,
            height: h,
        });
        Ok(out)
    }

    fn make_inputs(&self, key: InputKey) -> GpuResult<Inputs> {
        let gpu = &self.renderer.gpu;
        let (w, h) = (key.width, key.height);
        let tex = |format: DXGI_FORMAT, w: u32, h: u32| -> GpuResult<ID3D11Texture2D> {
            gpu.create_texture(&D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: format,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            })
        };
        let srv =
            |t: &ID3D11Texture2D, f: DXGI_FORMAT| -> GpuResult<Option<ID3D11ShaderResourceView>> {
                Ok(Some(gpu.create_srv(t, f)?))
            };
        let (textures, srvs) = if key.gpu {
            // One texture in the frame's own format; planar formats get one view per plane.
            match key.format {
                PixelFormat::Nv12 => {
                    let t = tex(DXGI_FORMAT_NV12, w, h)?;
                    let s = vec![
                        srv(&t, DXGI_FORMAT_R8_UNORM)?,
                        srv(&t, DXGI_FORMAT_R8G8_UNORM)?,
                    ];
                    (vec![t], s)
                }
                PixelFormat::P010 => {
                    let t = tex(DXGI_FORMAT_P010, w, h)?;
                    let s = vec![
                        srv(&t, DXGI_FORMAT_R16_UNORM)?,
                        srv(&t, DXGI_FORMAT_R16G16_UNORM)?,
                    ];
                    (vec![t], s)
                }
                PixelFormat::Yuy2 => {
                    let t = tex(DXGI_FORMAT_YUY2, w, h)?;
                    let s = vec![srv(&t, DXGI_FORMAT_R8G8B8A8_UNORM)?];
                    (vec![t], s)
                }
                PixelFormat::Xrgb | PixelFormat::Argb => {
                    let t = tex(DXGI_FORMAT_B8G8R8A8_UNORM, w, h)?;
                    let s = vec![srv(&t, DXGI_FORMAT_B8G8R8A8_UNORM)?];
                    (vec![t], s)
                }
                other => {
                    return Err(GpuError::Unsupported(format!(
                        "texture frames in {other} are not converted"
                    )))
                }
            }
        } else {
            match key.format {
                PixelFormat::Nv12 | PixelFormat::P010 => {
                    let (fy, fc) = if key.format == PixelFormat::Nv12 {
                        (DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_R8G8_UNORM)
                    } else {
                        (DXGI_FORMAT_R16_UNORM, DXGI_FORMAT_R16G16_UNORM)
                    };
                    let (y, c) = (tex(fy, w, h)?, tex(fc, w / 2, h / 2)?);
                    let s = vec![srv(&y, fy)?, srv(&c, fc)?];
                    (vec![y, c], s)
                }
                PixelFormat::I420 | PixelFormat::Yv12 => {
                    let f = DXGI_FORMAT_R8_UNORM;
                    let (y, a, b) = (tex(f, w, h)?, tex(f, w / 2, h / 2)?, tex(f, w / 2, h / 2)?);
                    // Plane order in the buffer: I420 has U then V, YV12 has V then U. The shader wants Y, U, V.
                    let s = if key.format == PixelFormat::I420 {
                        vec![srv(&y, f)?, srv(&a, f)?, srv(&b, f)?]
                    } else {
                        vec![srv(&y, f)?, srv(&b, f)?, srv(&a, f)?]
                    };
                    (vec![y, a, b], s)
                }
                PixelFormat::Yuy2 => {
                    let t = tex(DXGI_FORMAT_R8G8B8A8_UNORM, w / 2, h)?;
                    let s = vec![srv(&t, DXGI_FORMAT_R8G8B8A8_UNORM)?];
                    (vec![t], s)
                }
                PixelFormat::Xrgb | PixelFormat::Argb => {
                    let t = tex(DXGI_FORMAT_B8G8R8A8_UNORM, w, h)?;
                    let s = vec![srv(&t, DXGI_FORMAT_B8G8R8A8_UNORM)?];
                    (vec![t], s)
                }
                other => {
                    return Err(GpuError::Unsupported(format!(
                        "no converter for pixel format {other}"
                    )))
                }
            }
        };
        Ok(Inputs {
            key,
            textures,
            srvs,
        })
    }

    fn copy_texture(
        &self,
        src: &ID3D11Texture2D,
        subresource: u32,
        w: u32,
        h: u32,
    ) -> GpuResult<()> {
        let inputs = self.inputs.as_ref().expect("inputs exist");
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: plain calls on the render thread's context; both textures are valid.
        unsafe {
            src.GetDesc(&mut desc);
            let region = D3D11_BOX {
                left: 0,
                top: 0,
                front: 0,
                right: w,
                bottom: h,
                back: 1,
            };
            let exact = desc.Width == w && desc.Height == h;
            self.renderer.gpu.context.CopySubresourceRegion(
                &inputs.textures[0],
                0,
                0,
                0,
                0,
                src,
                subresource,
                if exact { None } else { Some(&region) },
            );
        }
        Ok(())
    }

    fn upload(&self, src: &SourceFrame<'_>, data: &[u8], stride: usize) -> GpuResult<()> {
        let inputs = self.inputs.as_ref().expect("inputs exist");
        let layout = cpu_layout(src.format, src.width, src.height, stride, src.luma_rows)?;
        // Plane order of the buffer vs. texture order: YV12 stores V before U, and `make_inputs` already swapped the
        // views, so the textures stay in buffer order.
        for (plane, tex) in layout.iter().zip(&inputs.textures) {
            let last = plane.offset + plane.pitch * (plane.rows - 1) + plane.row_bytes;
            if plane.pitch < plane.row_bytes || last > data.len() {
                return Err(GpuError::Invalid(format!(
                    "{} {}x{} frame: plane at {} needs {} bytes, the buffer has {} (stride {})",
                    src.format,
                    src.width,
                    src.height,
                    plane.offset,
                    last,
                    data.len(),
                    stride
                )));
            }
            // SAFETY: the plane's rows lie inside `data` (checked above); UpdateSubresource copies synchronously.
            unsafe {
                self.renderer.gpu.context.UpdateSubresource(
                    tex,
                    0,
                    None,
                    data.as_ptr().add(plane.offset) as *const _,
                    plane.pitch as u32,
                    0,
                );
            }
        }
        Ok(())
    }
}
