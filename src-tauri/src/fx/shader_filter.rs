//! The generic filter for the common case: one HLSL pixel shader plus a constant buffer (plan 4.5).
//!
//! A [`ShaderFilterDef`] says which shader, how to pack the params into the constant buffer, and which extra
//! textures the shader reads. [`ShaderFilter`] runs any definition: it compiles the shader once (cached by the
//! renderer), keeps the packed constants and uploads them only when they changed, and draws input to output.
//!
//! # The shader convention
//! ```hlsl
//! Texture2D<float4> image : register(t0);    // the input
//! // Texture2D / Texture3D extras at t1, t2, ... in the order of `ShaderFilterDef::extra`
//! SamplerState linear_clamp : register(s0);   // linear filtering, clamp
//! cbuffer Params : register(b0) { ... }       // what `pack` returned, a multiple of 16 bytes
//! float4 main(float4 pos : SV_Position) : SV_Target { ... }
//! ```
//! The pass draws a full-screen triangle into a texture of the input's size, so `pos.xy` is the pixel centre and
//! `image.Load(int3(pos.xy, 0))` reads the matching input texel.

use windows::Win32::Graphics::Direct3D11::{ID3D11PixelShader, ID3D11ShaderResourceView};

use super::{GpuFilter, Params};
use crate::gpu::{ConstantBuffer, GpuResult, Pass, Renderer, RgbaTexture};

/// A texture the shader reads besides the input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExtraInput {
    /// A 2D image (a picture the user picked).
    Image,
    /// A 3D lookup table.
    Lut3D,
}

/// What makes one pixel shader filter.
pub struct ShaderFilterDef {
    /// The OBS filter id; also the name the shader is cached under.
    pub kind: &'static str,
    /// The HLSL source, `include_str!("../gpu/shaders/<name>.hlsl")`.
    pub hlsl: &'static str,
    /// Params to the contents of `cbuffer Params` (a multiple of 16 bytes, the same length every time). Reads the
    /// params with the clamping readers of [`super`], so it also defines the defaults.
    pub pack: fn(&Params) -> Vec<u8>,
    /// Textures at `t1`, `t2`, ... . Until each of them is set ([`ShaderFilter::set_extra`]) the filter passes its
    /// input through.
    pub extra: &'static [ExtraInput],
}

/// A filter running a [`ShaderFilterDef`].
pub struct ShaderFilter {
    def: &'static ShaderFilterDef,
    packed: Vec<u8>,
    /// `packed` is not in the constant buffer yet.
    dirty: bool,
    constants: Option<ConstantBuffer>,
    ps: Option<ID3D11PixelShader>,
    extras: Vec<Option<ID3D11ShaderResourceView>>,
}

impl ShaderFilter {
    /// A filter with the default params (what `pack` makes of an empty map).
    pub fn new(def: &'static ShaderFilterDef) -> ShaderFilter {
        ShaderFilter {
            def,
            packed: (def.pack)(&Params::new()),
            dirty: true,
            constants: None,
            ps: None,
            extras: vec![None; def.extra.len()],
        }
    }

    /// Hand the filter the texture for extra input `index` (`None` takes it away again).
    pub fn set_extra(&mut self, index: usize, srv: Option<ID3D11ShaderResourceView>) {
        if let Some(slot) = self.extras.get_mut(index) {
            *slot = srv;
        }
    }
}

impl GpuFilter for ShaderFilter {
    fn update(&mut self, _r: &Renderer, p: &Params) {
        let packed = (self.def.pack)(p);
        if packed != self.packed {
            if packed.len() != self.packed.len() {
                // Never happens with a well-formed definition; start over rather than upload a wrong size.
                self.constants = None;
            }
            self.packed = packed;
            self.dirty = true;
        }
    }

    fn warm_up(&mut self, r: &Renderer) -> GpuResult<()> {
        self.ps = Some(r.shaders.pixel_source(self.def.kind, self.def.hlsl)?);
        Ok(())
    }

    fn render(&mut self, r: &Renderer, input: &RgbaTexture, output: &RgbaTexture) -> GpuResult<()> {
        if self.extras.iter().any(Option::is_none) {
            // An asset that is not loaded yet: show the picture unchanged.
            return r.copy(input, output);
        }
        let ps = match &self.ps {
            Some(ps) => ps.clone(),
            None => {
                let ps = r.shaders.pixel_source(self.def.kind, self.def.hlsl)?;
                self.ps = Some(ps.clone());
                ps
            }
        };
        if self.constants.is_none() {
            self.constants = Some(r.constants(self.packed.len())?);
            self.dirty = true;
        }
        let constants = self.constants.as_ref().expect("made above");
        if self.dirty {
            constants.write_bytes(&r.gpu.context, &self.packed)?;
            self.dirty = false;
        }
        let mut srvs = Vec::with_capacity(1 + self.extras.len());
        srvs.push(Some(input.srv.clone()));
        srvs.extend(self.extras.iter().cloned());
        r.draw(&Pass {
            ps: &ps,
            srvs: &srvs,
            constants: Some(constants),
            target: &output.rtv,
            width: output.width,
            height: output.height,
        });
        Ok(())
    }
}
