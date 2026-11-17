//! The geometry filters (plan 6.4, P10): Crop/Pad ([`crop`]), Scaling/Aspect Ratio ([`scale`]) and Transform
//! ([`transform`]). They change the size of the picture, so each overrides [`super::GpuFilter::output_size`]; the
//! chain gives the next filter and the output stage the new size.
//!
//! The pixel work is in `gpu/shaders/{crop,scale_basic,scale_bicubic,scale_lanczos,scale_area,transform}.hlsl`, with
//! `geometry_common.hlsl` (linear-light, premultiplied texel access) in front of all but the crop shader.
//!
//! Sizes never leave `1..=`[`MAX_SIDE`] per side and [`MAX_PIXELS`] in all ([`clamp_size`]), whatever the settings
//! and the input say, so no setting can ask for a texture the device cannot make.

pub mod crop;
pub mod scale;
#[cfg(test)]
mod tests;
pub mod transform;

use windows::Win32::Graphics::Direct3D11::ID3D11PixelShader;

use crate::gpu::{ConstantBuffer, GpuResult, Pass, Renderer, RgbaTexture};

/// The longest side a geometry filter produces (the limit of the Crop/Pad settings).
pub const MAX_SIDE: u32 = 8192;
/// The most pixels a geometry filter produces: 8192 x 4608, which holds an 8K UHD frame (7680 x 4320).
pub const MAX_PIXELS: u64 = 8192 * 4608;

/// A size from arithmetic that may have gone below one or far above anything sensible: at least 1 x 1, each side at
/// most [`MAX_SIDE`], the area at most [`MAX_PIXELS`] (the height gives way first).
pub fn clamp_size(w: i64, h: i64) -> (u32, u32) {
    let w = w.clamp(1, MAX_SIDE as i64) as u32;
    let mut h = h.clamp(1, MAX_SIDE as i64) as u32;
    if w as u64 * h as u64 > MAX_PIXELS {
        h = ((MAX_PIXELS / w as u64) as u32).max(1);
    }
    (w, h)
}

/// A pixel shader and the constant buffer it reads, made on first use. [`Program::draw`] rewrites the constants only
/// when their bytes changed.
pub(crate) struct Program {
    name: &'static str,
    hlsl: &'static str,
    ps: Option<ID3D11PixelShader>,
    constants: Option<ConstantBuffer>,
    written: Vec<u8>,
}

impl Program {
    pub(crate) fn new(name: &'static str, hlsl: &'static str) -> Program {
        Program {
            name,
            hlsl,
            ps: None,
            constants: None,
            written: Vec::new(),
        }
    }

    /// Compile the shader now (the renderer caches it by name).
    pub(crate) fn warm_up(&mut self, r: &Renderer) -> GpuResult<()> {
        self.shader(r).map(|_| ())
    }

    fn shader(&mut self, r: &Renderer) -> GpuResult<ID3D11PixelShader> {
        if let Some(ps) = &self.ps {
            return Ok(ps.clone());
        }
        let ps = r.shaders.pixel_source(self.name, self.hlsl)?;
        self.ps = Some(ps.clone());
        Ok(ps)
    }

    /// Draw `input` into `output` with the shader and the constants `packed` (a multiple of 16 bytes).
    pub(crate) fn draw(
        &mut self,
        r: &Renderer,
        packed: &[u8],
        input: &RgbaTexture,
        output: &RgbaTexture,
    ) -> GpuResult<()> {
        let ps = self.shader(r)?;
        if self.constants.is_none() || self.written.len() != packed.len() {
            self.constants = Some(r.constants(packed.len())?);
            self.written.clear();
        }
        let constants = self.constants.as_ref().expect("made above");
        if self.written != packed {
            constants.write_bytes(&r.gpu.context, packed)?;
            self.written = packed.to_vec();
        }
        r.draw(&Pass {
            ps: &ps,
            srvs: &[Some(input.srv.clone())],
            constants: Some(constants),
            target: &output.rtv,
            width: output.width,
            height: output.height,
        });
        Ok(())
    }
}

/// Constants as bytes: floats in order.
pub(crate) fn pack_f32(values: &[f32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

/// Constants as bytes: signed integers in order.
pub(crate) fn pack_i32(values: &[i32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}
