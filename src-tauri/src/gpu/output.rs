//! Output stage and encode (plan 4.2 stages 3 and 4, plan 6.3).
//!
//! * [`OutputStage`]: places the filter chain result in the output frame (fit / fill / stretch) over a background
//!   colour. Transparent pixels from keys and masks show the background; the result is opaque.
//! * [`Encoder`]: RGBA to NV12 (BT.709 limited range), or to `B8G8R8A8` for RGB32 consumers. NV12 is made in one compute
//!   dispatch that writes both planes (a frame whose width is a multiple of 4 and whose height is even; the planes are
//!   `R32_UINT` textures whose bytes are the rows of the Y and the UV plane), or, for any other even size, by two draws
//!   into an `R8_UNORM` Y target (W x H) and an `R8G8_UNORM` UV target (W/2 x H/2).

use std::sync::Arc;

use windows::Win32::Graphics::Direct3D11::{
    ID3D11RenderTargetView, ID3D11Texture2D, ID3D11UnorderedAccessView, D3D11_BIND_RENDER_TARGET,
    D3D11_BIND_UNORDERED_ACCESS, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R32_UINT, DXGI_FORMAT_R8G8_UNORM,
    DXGI_FORMAT_R8_UNORM, DXGI_SAMPLE_DESC,
};

use super::convert::{rgb_to_yuv_rows, ColorRange, ColorSpace};
use super::readback::PlaneDesc;
use super::{
    ConstantBuffer, GpuDevice, GpuError, GpuResult, Pass, Pooled, Renderer, RgbaTexture, ShaderId,
};

// ---------------------------------------------------------------------------------------------------------------
// Fit modes

/// How the image fills the output frame (plan 6.3 `fit`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FitMode {
    /// Whole image visible, bars where the aspect ratios differ.
    #[default]
    Fit,
    /// The frame is covered, the overflow is cropped.
    Fill,
    /// Distort to the output size.
    Stretch,
}

/// A rectangle in output pixels; `x` and `y` are negative when the image overflows (Fill).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// Where an image of `src` size lands in an `out` sized frame. The image is centred; both edges are rounded to
/// whole pixels, so a rectangle can be one pixel off the ideal aspect ratio.
pub fn fit_rect(src: (u32, u32), out: (u32, u32), mode: FitMode) -> Rect {
    let (sw, sh) = (src.0.max(1) as f64, src.1.max(1) as f64);
    let (ow, oh) = (out.0 as f64, out.1 as f64);
    let scale = match mode {
        FitMode::Stretch => {
            return Rect {
                x: 0,
                y: 0,
                w: out.0 as i32,
                h: out.1 as i32,
            }
        }
        FitMode::Fit => (ow / sw).min(oh / sh),
        FitMode::Fill => (ow / sw).max(oh / sh),
    };
    let (w, h) = (sw * scale, sh * scale);
    let (x, y) = ((ow - w) / 2.0, (oh - h) / 2.0);
    let (x0, x1) = (x.round(), (x + w).round());
    let (y0, y1) = (y.round(), (y + h).round());
    Rect {
        x: x0 as i32,
        y: y0 as i32,
        w: (x1 - x0) as i32,
        h: (y1 - y0) as i32,
    }
}

/// Settings of the output stage.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OutputParams {
    pub width: u32,
    pub height: u32,
    pub fit: FitMode,
    /// Background colour, each channel 0..1.
    pub background: [f32; 3],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct FitParams {
    dst_rect: [f32; 4],
    background: [f32; 4],
    /// `background` in linear light, for compositing translucent pixels (made here once, not by `pow` per pixel).
    background_linear: [f32; 4],
    point_mode: u32,
    pad: [u32; 3],
}

/// The sRGB code `c` (0..1) in linear light.
fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// Stage 3: fit the filter chain result into the output frame.
pub struct OutputStage {
    renderer: Arc<Renderer>,
    constants: ConstantBuffer,
}

impl OutputStage {
    pub fn new(renderer: Arc<Renderer>) -> GpuResult<OutputStage> {
        let constants = renderer.constants(std::mem::size_of::<FitParams>())?;
        Ok(OutputStage {
            renderer,
            constants,
        })
    }

    /// Render `input` into a pooled `params.width` x `params.height` texture.
    pub fn run(&self, input: &RgbaTexture, params: &OutputParams) -> GpuResult<Pooled> {
        if params.width == 0 || params.height == 0 {
            return Err(GpuError::Invalid("output size is zero".into()));
        }
        let r = &self.renderer;
        let rect = fit_rect(
            (input.width, input.height),
            (params.width, params.height),
            params.fit,
        );
        let cb = FitParams {
            dst_rect: [rect.x as f32, rect.y as f32, rect.w as f32, rect.h as f32],
            background: [
                params.background[0],
                params.background[1],
                params.background[2],
                1.0,
            ],
            background_linear: [
                srgb_to_linear(params.background[0]),
                srgb_to_linear(params.background[1]),
                srgb_to_linear(params.background[2]),
                1.0,
            ],
            // 1:1 placement: copy texels exactly instead of going through the bilinear sampler.
            point_mode: (rect.w as u32 == input.width && rect.h as u32 == input.height) as u32,
            pad: [0; 3],
        };
        self.constants.write(&r.gpu.context, &cb)?;
        let ps = r.shaders.pixel(ShaderId::OutputFit, "main")?;
        let out = r.texture(params.width, params.height)?;
        r.draw(&Pass {
            ps: &ps,
            srvs: &[Some(input.srv.clone())],
            constants: Some(&self.constants),
            target: &out.rtv,
            width: params.width,
            height: params.height,
        });
        Ok(out)
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Encode

/// What a consumer asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncodeFormat {
    /// Y plane then interleaved UV, BT.709 limited range.
    Nv12,
    /// BGRX, top-down.
    Bgra,
}

/// One plane of the encoded frame: the texture, how the encoder writes it (a render target view for the draws, an
/// unordered access view for the compute dispatch) and how it is read back.
pub struct EncodePlane {
    pub texture: ID3D11Texture2D,
    pub rtv: Option<ID3D11RenderTargetView>,
    pub uav: Option<ID3D11UnorderedAccessView>,
    pub desc: PlaneDesc,
}

/// The targets of one encoded frame: Y and UV for NV12, one plane for BGRA.
pub struct EncodeTargets {
    pub format: EncodeFormat,
    pub width: u32,
    pub height: u32,
    /// The NV12 planes are `R32_UINT` textures a compute shader writes (four bytes of the plane to a texel).
    pub packed: bool,
    pub planes: Vec<EncodePlane>,
}

impl EncodeTargets {
    pub fn textures(&self) -> Vec<&ID3D11Texture2D> {
        self.planes.iter().map(|p| &p.texture).collect()
    }

    pub fn plane_descs(&self) -> Vec<PlaneDesc> {
        self.planes.iter().map(|p| p.desc).collect()
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct EncodeParams {
    row_y: [f32; 4],
    row_u: [f32; 4],
    row_v: [f32; 4],
}

/// `cbuffer EncodeParams` of `encode_nv12_cs.hlsl`: the rows, then the blocks of 4 x 2 pixels per row and column.
#[repr(C)]
#[derive(Clone, Copy)]
struct EncodeCsParams {
    rows: EncodeParams,
    blocks: [u32; 2],
    pad: [u32; 2],
}

/// Whether one compute dispatch can write both NV12 planes of a `w` x `h` frame: a thread owns 4 x 2 pixels, and the
/// planes are `R32_UINT` textures of whole texels.
pub fn packs(w: u32, h: u32) -> bool {
    w.is_multiple_of(4) && h.is_multiple_of(2)
}

/// A plane written by draws (`packed` false) or by the compute shader (`packed` true: `R32_UINT`).
fn make_plane(
    gpu: &GpuDevice,
    f: DXGI_FORMAT,
    w: u32,
    h: u32,
    bpt: u32,
    packed: bool,
) -> GpuResult<EncodePlane> {
    let texture = gpu.create_texture(&D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: f,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: if packed {
            D3D11_BIND_UNORDERED_ACCESS.0
        } else {
            D3D11_BIND_RENDER_TARGET.0
        } as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    })?;
    let (rtv, uav) = if packed {
        (None, Some(gpu.create_uav(&texture, f)?))
    } else {
        (Some(gpu.create_rtv(&texture)?), None)
    };
    Ok(EncodePlane {
        texture,
        rtv,
        uav,
        desc: PlaneDesc {
            format: f,
            width: w,
            height: h,
            bytes_per_texel: bpt,
        },
    })
}

/// The planes of the one-dispatch NV12 encode for a `w` x `h` frame ([`packs`] must hold): the same bytes as the R8 and
/// R8G8 planes (a row is `w` bytes in both), four to a texel.
pub fn packed_nv12_targets(gpu: &GpuDevice, w: u32, h: u32) -> GpuResult<EncodeTargets> {
    if !packs(w, h) {
        return Err(GpuError::Invalid(format!(
            "NV12 in one dispatch needs a width of 4n and an even height, got {w}x{h}"
        )));
    }
    Ok(EncodeTargets {
        format: EncodeFormat::Nv12,
        width: w,
        height: h,
        packed: true,
        planes: vec![
            make_plane(gpu, DXGI_FORMAT_R32_UINT, w / 4, h, 4, true)?,
            make_plane(gpu, DXGI_FORMAT_R32_UINT, w / 4, h / 2, 4, true)?,
        ],
    })
}

/// The constant buffer [`dispatch_packed_nv12`] writes.
pub fn nv12_cs_constants(r: &Renderer) -> GpuResult<ConstantBuffer> {
    r.constants(std::mem::size_of::<EncodeCsParams>())
}

/// Write both NV12 planes of `input` (BT.709 limited range, the media type the virtual camera advertises, and what the
/// preview's shader decodes) into `targets` with one dispatch: every thread reads a block of 4 x 2 pixels once.
pub fn dispatch_packed_nv12(
    r: &Renderer,
    constants: &ConstantBuffer,
    input: &RgbaTexture,
    targets: &EncodeTargets,
) -> GpuResult<()> {
    let (w, h) = (input.width, input.height);
    let rows =
        rgb_to_yuv_rows(ColorSpace::Bt709, ColorRange::Limited).map(|row| row.map(|v| v as f32));
    constants.write(
        &r.gpu.context,
        &EncodeCsParams {
            rows: EncodeParams {
                row_y: rows[0],
                row_u: rows[1],
                row_v: rows[2],
            },
            blocks: [w / 4, h / 2],
            pad: [0; 2],
        },
    )?;
    let cs = r.shaders.compute(ShaderId::EncodeNv12Cs)?;
    let uavs = [targets.planes[0].uav.clone(), targets.planes[1].uav.clone()];
    r.dispatch(
        &cs,
        &[Some(input.srv.clone())],
        Some(constants),
        &uavs,
        ((w / 4).div_ceil(8), (h / 2).div_ceil(8)),
    );
    Ok(())
}

/// Stage 4: RGBA to the consumer's format.
pub struct Encoder {
    renderer: Arc<Renderer>,
    constants: ConstantBuffer,
    /// The constants of the compute dispatch, made when the first NV12 frame needs them.
    cs_constants: Option<ConstantBuffer>,
    targets: Option<EncodeTargets>,
    /// Test hook: use the two draws even where one dispatch could write both planes.
    #[cfg(test)]
    pub(crate) force_two_passes: bool,
}

impl Encoder {
    pub fn new(renderer: Arc<Renderer>) -> GpuResult<Encoder> {
        let constants = renderer.constants(std::mem::size_of::<EncodeParams>())?;
        Ok(Encoder {
            renderer,
            constants,
            cs_constants: None,
            targets: None,
            #[cfg(test)]
            force_two_passes: false,
        })
    }

    fn make_targets(&self, format: EncodeFormat, w: u32, h: u32) -> GpuResult<EncodeTargets> {
        let gpu = &*self.renderer.gpu;
        let plane = |f: DXGI_FORMAT, w: u32, h: u32, bpt: u32, packed: bool| {
            make_plane(gpu, f, w, h, bpt, packed)
        };
        let mut packed = false;
        let planes = match format {
            EncodeFormat::Nv12 => {
                #[allow(clippy::manual_is_multiple_of)] // MSRV 1.87: unsigned `is_multiple_of` is stable only from 1.90
                if w % 2 != 0 || h % 2 != 0 {
                    return Err(GpuError::Invalid(format!(
                        "NV12 needs an even size, got {w}x{h}"
                    )));
                }
                packed = self.wants_packed(w, h);
                if packed {
                    // The same bytes as the R8 and R8G8 planes (a row is `w` bytes in both), four to a texel.
                    vec![
                        plane(DXGI_FORMAT_R32_UINT, w / 4, h, 4, true)?,
                        plane(DXGI_FORMAT_R32_UINT, w / 4, h / 2, 4, true)?,
                    ]
                } else {
                    vec![
                        plane(DXGI_FORMAT_R8_UNORM, w, h, 1, false)?,
                        plane(DXGI_FORMAT_R8G8_UNORM, w / 2, h / 2, 2, false)?,
                    ]
                }
            }
            EncodeFormat::Bgra => vec![plane(DXGI_FORMAT_B8G8R8A8_UNORM, w, h, 4, false)?],
        };
        Ok(EncodeTargets {
            format,
            width: w,
            height: h,
            packed,
            planes,
        })
    }

    /// Whether NV12 of this size is written by the compute dispatch (see [`packs`]).
    fn wants_packed(&self, w: u32, h: u32) -> bool {
        #[cfg(test)]
        if self.force_two_passes {
            return false;
        }
        packs(w, h)
    }

    /// Render `input` into the encode targets (kept between calls while format and size stay the same).
    pub fn encode(
        &mut self,
        input: &RgbaTexture,
        format: EncodeFormat,
    ) -> GpuResult<&EncodeTargets> {
        let (w, h) = (input.width, input.height);
        let packed = format == EncodeFormat::Nv12 && self.wants_packed(w, h);
        let same = self
            .targets
            .as_ref()
            .is_some_and(|t| t.format == format && t.width == w && t.height == h && t.packed == packed);
        if !same {
            self.targets = None;
            self.targets = Some(self.make_targets(format, w, h)?);
        }
        if packed && self.cs_constants.is_none() {
            self.cs_constants = Some(self.renderer.constants(std::mem::size_of::<EncodeCsParams>())?);
        }
        let r = &self.renderer;
        let targets = self.targets.as_ref().expect("targets were just made");
        let srvs = [Some(input.srv.clone())];
        match format {
            EncodeFormat::Nv12 => {
                // Encoded frames are BT.709 limited range, the media type the virtual camera advertises.
                let rows = rgb_to_yuv_rows(ColorSpace::Bt709, ColorRange::Limited)
                    .map(|row| row.map(|v| v as f32));
                if targets.packed {
                    dispatch_packed_nv12(
                        r,
                        self.cs_constants.as_ref().expect("made above"),
                        input,
                        targets,
                    )?;
                } else {
                    self.constants.write(
                        &r.gpu.context,
                        &EncodeParams {
                            row_y: rows[0],
                            row_u: rows[1],
                            row_v: rows[2],
                        },
                    )?;
                    let luma = r.shaders.pixel(ShaderId::EncodeNv12, "PSLuma")?;
                    let chroma = r.shaders.pixel(ShaderId::EncodeNv12, "PSChroma")?;
                    for (ps, plane) in [(&luma, &targets.planes[0]), (&chroma, &targets.planes[1])] {
                        let Some(target) = &plane.rtv else {
                            return Err(GpuError::Invalid("an NV12 plane of the two-pass encoder has no render target".into()));
                        };
                        r.draw(&Pass {
                            ps,
                            srvs: &srvs,
                            constants: Some(&self.constants),
                            target,
                            width: plane.desc.width,
                            height: plane.desc.height,
                        });
                    }
                }
            }
            EncodeFormat::Bgra => {
                let ps = r.shaders.pixel(ShaderId::EncodeBgra, "main")?;
                r.draw(&Pass {
                    ps: &ps,
                    srvs: &srvs,
                    constants: None,
                    target: targets.planes[0].rtv.as_ref().expect("the BGRA plane is a render target"),
                    width: w,
                    height: h,
                });
            }
        }
        Ok(targets)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect { x, y, w, h }
    }

    #[test]
    fn four_by_three_into_sixteen_by_nine() {
        let (src, out) = ((640, 480), (1920, 1080));
        // 640x480 scaled by 2.25: pillarbox of 240 pixels each side.
        assert_eq!(fit_rect(src, out, FitMode::Fit), r(240, 0, 1440, 1080));
        // scaled by 3: 1920x1440, 180 rows cropped top and bottom.
        assert_eq!(fit_rect(src, out, FitMode::Fill), r(0, -180, 1920, 1440));
        assert_eq!(fit_rect(src, out, FitMode::Stretch), r(0, 0, 1920, 1080));
    }

    #[test]
    fn sixteen_by_nine_into_four_by_three() {
        let (src, out) = ((1920, 1080), (640, 480));
        // scaled by 1/3: 640x360, letterbox of 60 rows.
        assert_eq!(fit_rect(src, out, FitMode::Fit), r(0, 60, 640, 360));
        // scaled by 4/9: 853.33 x 480, centred at -106.67 .. 746.67.
        assert_eq!(fit_rect(src, out, FitMode::Fill), r(-107, 0, 854, 480));
        assert_eq!(fit_rect(src, out, FitMode::Stretch), r(0, 0, 640, 480));
    }

    #[test]
    fn same_aspect_is_the_whole_frame() {
        for mode in [FitMode::Fit, FitMode::Fill, FitMode::Stretch] {
            assert_eq!(
                fit_rect((1280, 720), (1920, 1080), mode),
                r(0, 0, 1920, 1080)
            );
            assert_eq!(
                fit_rect((1920, 1080), (1920, 1080), mode),
                r(0, 0, 1920, 1080)
            );
        }
    }
}
