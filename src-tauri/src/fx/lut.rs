/*****************************************************************************
Ported to Rust for Better Cam from OBS Studio, plugins/obs-filters/color-grade-filter.c (the filter id "clut_filter":
`color_grade_filter_update` for the settings, the lookup constants and the choice of technique,
`color_grade_filter_defaults`, `color_grade_filter_properties`, `make_clut_texture_png` in fx/assets.rs). The pixel
work is in src/gpu/shaders/lut3d.hlsl and lut1d.hlsl, ports of plugins/obs-filters/data/color_grade_filter.effect.

This program is free software: you can redistribute it and/or modify
it under the terms of the GNU General Public License as published by
the Free Software Foundation, either version 2 of the License, or
(at your option) any later version.

This program is distributed in the hope that it will be useful,
but WITHOUT ANY WARRANTY; without even the implied warranty of
MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
GNU General Public License for more details.

You should have received a copy of the GNU General Public License
along with this program.  If not, see <http://www.gnu.org/licenses/>.
*****************************************************************************/

//! Apply LUT (`clut_filter`, plan 6.4): `image_path` (a `.cube` or `.png` table), `clut_amount` (0 to 1, default 1)
//! and `passthrough_alpha`.
//!
//! The asset service loads and parses the file ([`AssetKind::ColorLut`]): a `.cube` keeps its size and domain, a
//! picture becomes a 64 cubed table with domain 0 to 1 ([`super::assets::png_lut_to_cube`]). This filter turns the
//! table into the lookup constants of `color_grade_filter_update` ([`constants`]), picks the technique OBS would
//! ([`technique`]) and runs it. While the file is loading, missing or invalid, the picture passes through unchanged;
//! the card shows the loader's message. An empty path is not an error.
//!
//! Where OBS differs from the pipeline here, and what was done about it, is in the comments of the two shaders.

#[cfg(test)]
mod tests;

use windows::Win32::Graphics::Direct3D11::ID3D11PixelShader;

use super::assets::{Asset, AssetHandle, AssetKind, CubeLut, LutDim};
use super::shader_filter::PATH_KEY;
use super::{pb, pf, ps, GpuFilter, Params};
use crate::gpu::{ConstantBuffer, GpuResult, Pass, Renderer, RgbaTexture};

pub const KIND: &str = "clut_filter";

/// The settings the filter reads (OBS setting names).
pub const KEYS: &[&str] = &["image_path", "clut_amount", "passthrough_alpha"];

const HLSL_3D: &str = include_str!("../gpu/shaders/lut3d.hlsl");
const HLSL_1D: &str = include_str!("../gpu/shaders/lut1d.hlsl");
const SHADER_3D: &str = "clut_filter_3d";
const SHADER_1D: &str = "clut_filter_1d";

/// The size of `cbuffer Params` in both shaders.
const CONSTANTS_SIZE: usize = 64;

/// OBS's techniques of `color_grade_filter.effect`, as `color_grade_filter_update` picks them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Technique {
    /// A 1D table.
    Draw1D,
    /// A 3D table, "Passthrough Alpha" on, full amount, no domain.
    Draw3D,
    /// A 3D table, full amount, no domain.
    DrawAlpha3D,
    /// A 3D table, amount below 1, no domain.
    DrawAmount3D,
    /// A 3D table with a domain narrower than 0 to 1 (and the amount, whatever it is).
    DrawDomain3D,
}

impl Technique {
    /// The `mode` constant of `lut3d.hlsl`.
    fn mode(self) -> u32 {
        match self {
            Technique::Draw1D | Technique::Draw3D => 0,
            Technique::DrawAlpha3D => 1,
            Technique::DrawAmount3D => 2,
            Technique::DrawDomain3D => 3,
        }
    }
}

/// The technique `color_grade_filter_update` selects for this table and these settings.
pub fn technique(lut: &CubeLut, amount: f32, passthrough_alpha: bool) -> Technique {
    if lut.dim == LutDim::OneD {
        Technique::Draw1D
    } else if lut.domain_min.iter().any(|v| *v > 0.0) || lut.domain_max.iter().any(|v| *v < 1.0) {
        Technique::DrawDomain3D
    } else if amount < 1.0 {
        Technique::DrawAmount3D
    } else if !passthrough_alpha {
        Technique::DrawAlpha3D
    } else {
        Technique::Draw3D
    }
}

/// `clut_scale` and `clut_offset` of `color_grade_filter_update` for a `.cube` table (a picture table is the
/// 64 cubed table with domain 0 to 1, for which this gives OBS's `1 - 1/64` and `0.5/64`). The lookup coordinate of
/// a gamma-encoded colour `c` is `c * scale + offset`, in texture units.
pub fn scale_offset(lut: &CubeLut) -> ([f32; 3], [f32; 3]) {
    let width = lut.size as f32;
    let width_minus_one = width - 1.0;
    let mut scale = [0.0f32; 3];
    let mut offset = [0.0f32; 3];
    for i in 0..3 {
        let domain_scale = lut.domain_max[i] - lut.domain_min[i];
        let s = width_minus_one / domain_scale;
        let o = -lut.domain_min[i] * s;
        // want normalized UVW
        scale[i] = s / width;
        offset[i] = (o + 0.5) / width;
    }
    (scale, offset)
}

/// `cbuffer Params` of the two shaders: scale, amount, offset, mode, domain minimum, domain maximum.
pub fn constants(lut: &CubeLut, amount: f32, passthrough_alpha: bool) -> [u8; CONSTANTS_SIZE] {
    let (scale, offset) = scale_offset(lut);
    let mode = technique(lut, amount, passthrough_alpha).mode();
    let mut out = [0u8; CONSTANTS_SIZE];
    let mut put = |i: usize, v: [u8; 4]| out[i * 4..i * 4 + 4].copy_from_slice(&v);
    for i in 0..3 {
        put(i, scale[i].to_le_bytes());
        put(4 + i, offset[i].to_le_bytes());
        put(8 + i, lut.domain_min[i].to_le_bytes());
        put(12 + i, lut.domain_max[i].to_le_bytes());
    }
    put(3, amount.to_le_bytes());
    put(7, mode.to_le_bytes());
    out
}

/// The Apply LUT filter.
#[derive(Default)]
pub struct LutFilter {
    assets: Option<AssetHandle>,
    amount: f32,
    passthrough_alpha: bool,
    constants: Option<ConstantBuffer>,
    /// What the constant buffer holds.
    uploaded: Option<[u8; CONSTANTS_SIZE]>,
    ps_3d: Option<ID3D11PixelShader>,
    ps_1d: Option<ID3D11PixelShader>,
}

pub fn make() -> Box<dyn GpuFilter> {
    Box::new(LutFilter {
        amount: 1.0,
        ..LutFilter::default()
    })
}

impl LutFilter {
    fn shader(&mut self, r: &Renderer, dim: LutDim) -> GpuResult<ID3D11PixelShader> {
        let (slot, name, src) = match dim {
            LutDim::ThreeD => (&mut self.ps_3d, SHADER_3D, HLSL_3D),
            LutDim::OneD => (&mut self.ps_1d, SHADER_1D, HLSL_1D),
        };
        if slot.is_none() {
            *slot = Some(r.shaders.pixel_source(name, src)?);
        }
        Ok(slot.clone().expect("set above"))
    }
}

impl GpuFilter for LutFilter {
    fn attach_assets(&mut self, assets: AssetHandle) {
        self.assets = Some(assets);
    }

    fn update(&mut self, _r: &Renderer, p: &Params) {
        if let Some(h) = self.assets.as_mut() {
            h.request(AssetKind::ColorLut, ps(p, PATH_KEY, ""));
        }
        self.amount = pf(p, "clut_amount", 1.0, 0.0, 1.0);
        self.passthrough_alpha = pb(p, "passthrough_alpha", false);
    }

    fn warm_up(&mut self, r: &Renderer) -> GpuResult<()> {
        self.shader(r, LutDim::ThreeD)?;
        self.shader(r, LutDim::OneD)?;
        Ok(())
    }

    fn render(&mut self, r: &Renderer, input: &RgbaTexture, output: &RgbaTexture) -> GpuResult<()> {
        let Some(asset) = self.assets.as_ref().and_then(AssetHandle::get) else {
            // No path, or the table is loading or failed: show the picture unchanged.
            return r.copy(input, output);
        };
        let Asset::Cube(table) = &*asset else {
            return r.copy(input, output);
        };
        let ps = self.shader(r, table.lut.dim)?;
        let packed = constants(&table.lut, self.amount, self.passthrough_alpha);
        if self.constants.is_none() {
            self.constants = Some(r.constants(CONSTANTS_SIZE)?);
            self.uploaded = None;
        }
        let buffer = self.constants.as_ref().expect("made above");
        if self.uploaded != Some(packed) {
            buffer.write_bytes(&r.gpu.context, &packed)?;
            self.uploaded = Some(packed);
        }
        r.draw(&Pass {
            ps: &ps,
            srvs: &[Some(input.srv.clone()), Some(table.srv.clone())],
            constants: Some(buffer),
            target: &output.rtv,
            width: output.width,
            height: output.height,
        });
        Ok(())
    }
}
