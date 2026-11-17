//! Output stage and encode (plan 4.2 stages 3 and 4, plan 6.3).
//!
//! * [`OutputStage`]: places the filter chain result in the output frame (fit / fill / stretch) over a background
//!   colour. Transparent pixels from keys and masks show the background; the result is opaque.
//! * [`Encoder`]: RGBA to NV12 (BT.709 limited range) as an `R8_UNORM` Y target (W x H) and an `R8G8_UNORM` UV target
//!   (W/2 x H/2), or to `B8G8R8A8` for RGB32 consumers.

use std::sync::Arc;

use windows::Win32::Graphics::Direct3D11::{
    ID3D11RenderTargetView, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R8G8_UNORM, DXGI_FORMAT_R8_UNORM,
    DXGI_SAMPLE_DESC,
};

use super::convert::{rgb_to_yuv_rows, ColorRange, ColorSpace};
use super::readback::PlaneDesc;
use super::{ConstantBuffer, GpuError, GpuResult, Pass, Pooled, Renderer, RgbaTexture, ShaderId};

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
    point_mode: u32,
    pad: [u32; 3],
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

/// One render target of the encoded frame.
pub struct EncodePlane {
    pub texture: ID3D11Texture2D,
    pub rtv: ID3D11RenderTargetView,
    pub desc: PlaneDesc,
}

/// The targets of one encoded frame: Y and UV for NV12, one plane for BGRA.
pub struct EncodeTargets {
    pub format: EncodeFormat,
    pub width: u32,
    pub height: u32,
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

/// Stage 4: RGBA to the consumer's format.
pub struct Encoder {
    renderer: Arc<Renderer>,
    constants: ConstantBuffer,
    targets: Option<EncodeTargets>,
}

impl Encoder {
    pub fn new(renderer: Arc<Renderer>) -> GpuResult<Encoder> {
        let constants = renderer.constants(std::mem::size_of::<EncodeParams>())?;
        Ok(Encoder {
            renderer,
            constants,
            targets: None,
        })
    }

    fn make_targets(&self, format: EncodeFormat, w: u32, h: u32) -> GpuResult<EncodeTargets> {
        let gpu = &self.renderer.gpu;
        let plane = |f: DXGI_FORMAT, w: u32, h: u32, bpt: u32| -> GpuResult<EncodePlane> {
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
                BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            })?;
            let rtv = gpu.create_rtv(&texture)?;
            Ok(EncodePlane {
                texture,
                rtv,
                desc: PlaneDesc {
                    format: f,
                    width: w,
                    height: h,
                    bytes_per_texel: bpt,
                },
            })
        };
        let planes = match format {
            EncodeFormat::Nv12 => {
                if w % 2 != 0 || h % 2 != 0 {
                    return Err(GpuError::Invalid(format!(
                        "NV12 needs an even size, got {w}x{h}"
                    )));
                }
                vec![
                    plane(DXGI_FORMAT_R8_UNORM, w, h, 1)?,
                    plane(DXGI_FORMAT_R8G8_UNORM, w / 2, h / 2, 2)?,
                ]
            }
            EncodeFormat::Bgra => vec![plane(DXGI_FORMAT_B8G8R8A8_UNORM, w, h, 4)?],
        };
        Ok(EncodeTargets {
            format,
            width: w,
            height: h,
            planes,
        })
    }

    /// Render `input` into the encode targets (kept between calls while format and size stay the same).
    pub fn encode(
        &mut self,
        input: &RgbaTexture,
        format: EncodeFormat,
    ) -> GpuResult<&EncodeTargets> {
        let (w, h) = (input.width, input.height);
        let same = self
            .targets
            .as_ref()
            .is_some_and(|t| t.format == format && t.width == w && t.height == h);
        if !same {
            self.targets = None;
            self.targets = Some(self.make_targets(format, w, h)?);
        }
        let r = &self.renderer;
        let targets = self.targets.as_ref().expect("targets were just made");
        let srvs = [Some(input.srv.clone())];
        match format {
            EncodeFormat::Nv12 => {
                // Encoded frames are BT.709 limited range, the media type the virtual camera advertises.
                let rows = rgb_to_yuv_rows(ColorSpace::Bt709, ColorRange::Limited)
                    .map(|row| row.map(|v| v as f32));
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
                    r.draw(&Pass {
                        ps,
                        srvs: &srvs,
                        constants: Some(&self.constants),
                        target: &plane.rtv,
                        width: plane.desc.width,
                        height: plane.desc.height,
                    });
                }
            }
            EncodeFormat::Bgra => {
                let ps = r.shaders.pixel(ShaderId::EncodeBgra, "main")?;
                r.draw(&Pass {
                    ps: &ps,
                    srvs: &srvs,
                    constants: None,
                    target: &targets.planes[0].rtv,
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
