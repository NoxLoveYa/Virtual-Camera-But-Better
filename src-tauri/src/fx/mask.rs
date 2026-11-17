/*****************************************************************************
Copyright (C) OBS Studio contributors (https://github.com/obsproject/obs-studio)

Ported to Rust for Better Cam from OBS Studio, plugins/obs-filters/mask-filter.c (the v2 filter, id "mask_filter"
version 2: `mask_filter_update_v2` / `mask_filter_update_internal`, `mask_filter_defaults_v2`,
`mask_filter_properties_v2`, and the aspect fitting of `mask_filter_render`). The pixel work is in
src/gpu/shaders/{mask_color,mask_alpha,blend_mul,blend_add,blend_sub}.hlsl, ports of
plugins/obs-filters/data/{mask_color,mask_alpha,blend_mul,blend_add,blend_sub}_filter.effect.

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

//! Image Mask/Blend (`mask_filter`, plan 6.4): shape the picture with another picture (the colour channels or the
//! alpha channel of the image become the picture's alpha) or blend the image into it (multiply, add, subtract).
//!
//! The image comes from the asset service ([`super::assets`]) through the path in `image_path`. Until it is loaded,
//! when the path is empty, or when it failed to load (the service reports that on the card) the filter hands the
//! picture on unchanged, as OBS's mask filter skips itself without an image.
//!
//! The filter is its own [`GpuFilter`] rather than a [`super::shader_filter::ShaderFilter`] because the placement of
//! the image depends on the size of the picture it receives: with `stretch` off the image keeps its aspect ratio
//! ([`aspect_fit`], the arithmetic of `mask_filter_render`).

use windows::Win32::Graphics::Direct3D11::ID3D11PixelShader;

use super::assets::{Asset, AssetHandle, AssetKind};
use super::color::srgb_to_linear;
use super::shader_filter::PATH_KEY;
use super::{pb, pc, pf, ps, GpuFilter, Params};
use crate::gpu::{ConstantBuffer, GpuResult, Pass, Renderer, RgbaTexture};

#[cfg(test)]
mod tests;

pub const KIND: &str = "mask_filter";

/// The settings the filter reads (OBS setting names).
pub const KEYS: &[&str] = &["type", "image_path", "color", "opacity", "stretch"];

/// The `type` setting: the name of the OBS effect file that does the work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MaskType {
    /// Alpha Mask (Color Channel): the image's brightness (mean of red, green and blue) scales the alpha.
    Color,
    /// Alpha Mask (Alpha Channel): the image's alpha scales the alpha.
    Alpha,
    /// Blend (Multiply).
    Multiply,
    /// Blend (Addition).
    Add,
    /// Blend (Subtraction).
    Subtract,
}

impl MaskType {
    pub const ALL: [MaskType; 5] = [
        MaskType::Color,
        MaskType::Alpha,
        MaskType::Multiply,
        MaskType::Add,
        MaskType::Subtract,
    ];

    /// The `type` value OBS stores, also the name the shader is cached under.
    pub fn effect(self) -> &'static str {
        match self {
            MaskType::Color => "mask_color_filter.effect",
            MaskType::Alpha => "mask_alpha_filter.effect",
            MaskType::Multiply => "blend_mul_filter.effect",
            MaskType::Add => "blend_add_filter.effect",
            MaskType::Subtract => "blend_sub_filter.effect",
        }
    }

    /// The type for a stored `type` value; `None` for anything else.
    pub fn from_effect(s: &str) -> Option<MaskType> {
        MaskType::ALL.into_iter().find(|t| t.effect() == s)
    }

    fn hlsl(self) -> &'static str {
        match self {
            MaskType::Color => include_str!("../gpu/shaders/mask_color.hlsl"),
            MaskType::Alpha => include_str!("../gpu/shaders/mask_alpha.hlsl"),
            MaskType::Multiply => include_str!("../gpu/shaders/blend_mul.hlsl"),
            MaskType::Add => include_str!("../gpu/shaders/blend_add.hlsl"),
            MaskType::Subtract => include_str!("../gpu/shaders/blend_sub.hlsl"),
        }
    }
}

/// The settings of one filter (`mask_filter_update_v2`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Settings {
    pub kind: MaskType,
    /// The picked colour in linear light (`vec4_from_rgba_srgb`) and the opacity in `w` (`filter->color.w = opacity`).
    pub color: [f32; 4],
    /// `lock_aspect` is the opposite of `stretch`.
    pub stretch: bool,
}

/// Read the settings. An unknown `type` falls back to the default (the colour mask).
pub fn settings(p: &Params) -> Settings {
    let c = pc(p, "color", 0x00FF_FFFF);
    Settings {
        kind: MaskType::from_effect(ps(p, "type", MaskType::Color.effect())).unwrap_or(MaskType::Color),
        color: [
            srgb_to_linear(c[0]),
            srgb_to_linear(c[1]),
            srgb_to_linear(c[2]),
            pf(p, "opacity", 1.0, 0.0, 1.0),
        ],
        stretch: pb(p, "stretch", false),
    }
}

/// The placement of the image over the picture: `uv2 = uv * mul + add`. With `stretch` it is the identity. Without,
/// this is the arithmetic of `mask_filter_render` (`lock_aspect`): the image is scaled by the factor that makes it
/// fit its width (if the picture is narrower in aspect than the image) or its height, and centred; the part of the
/// picture the image does not cover reads the image's edge pixels.
pub fn aspect_fit(source: (f32, f32), mask: (f32, f32)) -> ([f32; 2], [f32; 2]) {
    let source_aspect = source.0 / source.1;
    let mask_aspect = mask.0 / mask.1;
    let size_to_x = source_aspect < mask_aspect;

    let fix = if size_to_x { source.0 / mask.0 } else { source.1 / mask.1 };

    // vec2_mulf(&mask_size, &mask_size, fix)
    let mask_size = [mask.0 * fix, mask.1 * fix];
    // vec2_div(&mul_val, &source_size, &mask_size)
    let mul = [source.0 / mask_size[0], source.1 / mask_size[1]];
    // vec2_mulf(&source_size, 0.5), vec2_mulf(&mask_temp, &mask_size, 0.5), vec2_sub, vec2_neg, vec2_div
    let source_half = [source.0 * 0.5, source.1 * 0.5];
    let mask_half = [mask_size[0] * 0.5, mask_size[1] * 0.5];
    let add = [
        -(source_half[0] - mask_half[0]) / mask_size[0],
        -(source_half[1] - mask_half[1]) / mask_size[1],
    ];
    (mul, add)
}

/// `cbuffer Params` of the five shaders: `float4 color; float2 mul_val; float2 add_val; float2 target_size; float2 pad;`.
pub fn pack(color: [f32; 4], mul: [f32; 2], add: [f32; 2], target: (u32, u32)) -> Vec<u8> {
    [
        color[0],
        color[1],
        color[2],
        color[3],
        mul[0],
        mul[1],
        add[0],
        add[1],
        target.0 as f32,
        target.1 as f32,
        0.0,
        0.0,
    ]
    .iter()
    .flat_map(|f| f.to_le_bytes())
    .collect()
}

pub fn make() -> Box<dyn GpuFilter> {
    Box::new(MaskFilter {
        settings: settings(&Params::new()),
        assets: None,
        constants: None,
        uploaded: Vec::new(),
    })
}

/// Image Mask/Blend.
pub struct MaskFilter {
    settings: Settings,
    /// Present when the engine has an asset service (a chain built for a test may have none: the filter then passes
    /// the picture through).
    assets: Option<AssetHandle>,
    constants: Option<ConstantBuffer>,
    /// What `constants` holds now.
    uploaded: Vec<u8>,
}

impl MaskFilter {
    fn shader(&self, r: &Renderer, kind: MaskType) -> GpuResult<ID3D11PixelShader> {
        r.shaders.pixel_source(kind.effect(), kind.hlsl())
    }
}

impl GpuFilter for MaskFilter {
    fn attach_assets(&mut self, assets: AssetHandle) {
        self.assets = Some(assets);
    }

    fn update(&mut self, _r: &Renderer, p: &Params) {
        self.settings = settings(p);
        if let Some(h) = self.assets.as_mut() {
            h.request(AssetKind::Image, ps(p, PATH_KEY, ""));
        }
    }

    fn warm_up(&mut self, r: &Renderer) -> GpuResult<()> {
        for kind in MaskType::ALL {
            self.shader(r, kind)?;
        }
        Ok(())
    }

    fn render(&mut self, r: &Renderer, input: &RgbaTexture, output: &RgbaTexture) -> GpuResult<()> {
        let asset = self.assets.as_ref().and_then(AssetHandle::get);
        let img = match asset.as_deref() {
            Some(Asset::Image(img)) => img,
            // No image yet, none asked for, or it failed: the picture comes through unchanged.
            _ => return r.copy(input, output),
        };

        let (mul, add) = if self.settings.stretch {
            ([1.0, 1.0], [0.0, 0.0])
        } else {
            aspect_fit(
                (input.width as f32, input.height as f32),
                (img.width as f32, img.height as f32),
            )
        };
        let packed = pack(self.settings.color, mul, add, (img.width, img.height));

        let ps = self.shader(r, self.settings.kind)?;
        if self.constants.is_none() {
            self.constants = Some(r.constants(packed.len())?);
            self.uploaded.clear();
        }
        let constants = self.constants.as_ref().expect("made above");
        if packed != self.uploaded {
            constants.write_bytes(&r.gpu.context, &packed)?;
            self.uploaded = packed;
        }
        r.draw(&Pass {
            ps: &ps,
            srvs: &[Some(input.srv.clone()), Some(img.srv.clone())],
            constants: Some(constants),
            target: &output.rtv,
            width: output.width,
            height: output.height,
        });
        Ok(())
    }
}
