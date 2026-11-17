/*****************************************************************************
Copyright (C) OBS Studio contributors (https://github.com/obsproject/obs-studio)

Ported to Rust for Better Cam from OBS Studio, plugins/obs-filters/chroma-key-filter.c
(`chroma_key_filter_v2`: `color_settings_update_v2`, `chroma_settings_update_v2`, `chroma_key_defaults_v2`,
`chroma_key_properties_v2`), plugins/obs-filters/color-key-filter.c (`color_key_filter_v2`: `color_settings_update_v2`,
`key_settings_update_v2`, `color_key_defaults_v2`) and plugins/obs-filters/luma-key-filter.c (`luma_key_filter_v2`:
`luma_key_update`, `luma_key_defaults`). The pixel work is in src/gpu/shaders/{chroma_key,color_key,luma_key}.hlsl,
ports of plugins/obs-filters/data/{chroma,color,luma}_key_filter_v2.effect.

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

//! Chroma Key, Color Key and Luma Key (`chroma_key_filter`, `color_key_filter`, `luma_key_filter`, plan 6.4): the
//! three filters that make part of the picture transparent. The output stage shows the output background colour
//! through the transparent pixels.
//!
//! Each is one [`ShaderFilterDef`]. The functions here turn the OBS settings into the constants of the shader the way
//! the `*_settings_update_v2` functions of OBS do: contrast and gamma become the exponents the effect uses, the key
//! colour type becomes a fixed colour (or the picked one for `custom`), the similarity, smoothness and spill sliders
//! (1 to 1000) become fractions.

use super::shader_filter::{ShaderFilter, ShaderFilterDef};
use super::{pc, pf, pi, ps, GpuFilter, Params};

#[cfg(test)]
mod tests;

pub const CHROMA_KIND: &str = "chroma_key_filter";
pub const COLOR_KIND: &str = "color_key_filter";
pub const LUMA_KIND: &str = "luma_key_filter";

/// The settings each filter reads (OBS setting names).
pub const CHROMA_KEYS: &[&str] = &[
    "key_color_type",
    "key_color",
    "similarity",
    "smoothness",
    "spill",
    "opacity",
    "contrast",
    "brightness",
    "gamma",
];
pub const COLOR_KEYS: &[&str] = &[
    "key_color_type",
    "key_color",
    "similarity",
    "smoothness",
    "opacity",
    "contrast",
    "brightness",
    "gamma",
];
pub const LUMA_KEYS: &[&str] = &["luma_max", "luma_max_smooth", "luma_min", "luma_min_smooth"];

pub static CHROMA_DEF: ShaderFilterDef = ShaderFilterDef {
    kind: CHROMA_KIND,
    hlsl: include_str!("../gpu/shaders/chroma_key.hlsl"),
    pack: chroma_pack,
    extra: &[],
};

pub static COLOR_DEF: ShaderFilterDef = ShaderFilterDef {
    kind: COLOR_KIND,
    hlsl: include_str!("../gpu/shaders/color_key.hlsl"),
    pack: color_pack,
    extra: &[],
};

pub static LUMA_DEF: ShaderFilterDef = ShaderFilterDef {
    kind: LUMA_KIND,
    hlsl: include_str!("../gpu/shaders/luma_key.hlsl"),
    pack: luma_pack,
    extra: &[],
};

pub fn make_chroma() -> Box<dyn GpuFilter> {
    Box::new(ShaderFilter::new(&CHROMA_DEF))
}

pub fn make_color() -> Box<dyn GpuFilter> {
    Box::new(ShaderFilter::new(&COLOR_DEF))
}

pub fn make_luma() -> Box<dyn GpuFilter> {
    Box::new(ShaderFilter::new(&LUMA_DEF))
}

// ---------------------------------------------------------------------------------------------------------------
// Shared settings

/// The colour terms of `color_settings_update_v2`: opacity, the contrast gain, brightness and the gamma exponent.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ColorTerms {
    pub opacity: f32,
    pub contrast: f32,
    pub brightness: f32,
    pub gamma: f32,
}

/// `color_settings_update_v2`, with the ranges of the `*_properties_v2` functions (opacity 0 to 1, contrast -4 to 4,
/// brightness -1 to 1, gamma -1 to 1).
pub fn color_terms(p: &Params) -> ColorTerms {
    let opacity = pf(p, "opacity", 1.0, 0.0, 1.0);

    // contrast = (contrast < 0.0) ? (1.0 / (-contrast + 1.0)) : (contrast + 1.0), in double precision
    let contrast = pf(p, "contrast", 0.0, -4.0, 4.0) as f64;
    let contrast = if contrast < 0.0 {
        1.0 / (-contrast + 1.0)
    } else {
        contrast + 1.0
    } as f32;

    let brightness = pf(p, "brightness", 0.0, -1.0, 1.0);

    // gamma = (gamma < 0.0) ? (-gamma + 1.0) : (1.0 / (gamma + 1.0))
    let gamma = pf(p, "gamma", 0.0, -1.0, 1.0) as f64;
    let gamma = if gamma < 0.0 {
        -gamma + 1.0
    } else {
        1.0 / (gamma + 1.0)
    } as f32;

    ColorTerms {
        opacity,
        contrast,
        brightness,
        gamma,
    }
}

/// `vec4_from_rgba` without alpha: the OBS colour int `0xAABBGGRR` as RGB floats (red is the lowest byte).
fn rgb_of(c: u32) -> [f32; 3] {
    let b = c.to_le_bytes();
    [b[0] as f32 / 255.0, b[1] as f32 / 255.0, b[2] as f32 / 255.0]
}

/// The key colour int for a type: the fixed colours of the filter, or the picked `key_color` for `custom` (and for
/// any other string, as OBS falls through to the setting when no name matches).
fn key_color(p: &Params, fixed: &[(&str, u32)]) -> [f32; 3] {
    let ty = ps(p, "key_color_type", "green");
    match fixed.iter().find(|(name, _)| *name == ty) {
        Some((_, c)) => rgb_of(*c),
        None => {
            let c = pc(p, "key_color", 0x0000_FF00);
            [c[0], c[1], c[2]]
        }
    }
}

fn floats(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

// ---------------------------------------------------------------------------------------------------------------
// Chroma Key

/// The constant vectors of the effect (`cb_vec`, `cr_vec` of the C file): the Cb and Cr rows of the RGB to YCbCr
/// matrix, with the 0.5 offset in the last entry.
pub const CB_VEC: [f32; 4] = [-0.100644, -0.338572, 0.439216, 0.501961];
pub const CR_VEC: [f32; 4] = [0.439216, -0.398942, -0.040274, 0.501961];

/// What `chroma_settings_update_v2` computes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Chroma {
    /// The key colour's (Cb, Cr) (`filter->chroma`).
    pub chroma: [f32; 2],
    pub similarity: f32,
    pub smoothness: f32,
    pub spill: f32,
}

/// `chroma_settings_update_v2`: the green, blue and magenta types are fixed colours (blue is 0xFF9900, a screen blue,
/// not pure blue), `custom` takes `key_color`.
pub fn chroma(p: &Params) -> Chroma {
    let similarity = pi(p, "similarity", 400, 1, 1000);
    let smoothness = pi(p, "smoothness", 80, 1, 1000);
    let spill = pi(p, "spill", 100, 1, 1000);
    let rgb = key_color(
        p,
        &[("green", 0x0000_FF00), ("blue", 0x00FF_9900), ("magenta", 0x00FF_00FF)],
    );
    // key_rgb has alpha 1 (`key_color | 0xFF000000`), so vec4_dot adds the offset once.
    let dot = |v: &[f32; 4]| rgb[0] * v[0] + rgb[1] * v[1] + rgb[2] * v[2] + v[3];
    Chroma {
        chroma: [dot(&CB_VEC), dot(&CR_VEC)],
        similarity: similarity as f32 / 1000.0,
        smoothness: smoothness as f32 / 1000.0,
        spill: spill as f32 / 1000.0,
    }
}

/// `cbuffer Params`: `float2 chroma_key; float opacity, contrast, brightness, gamma, similarity, smoothness, spill;`
/// and padding to 48 bytes.
pub fn chroma_pack(p: &Params) -> Vec<u8> {
    let c = chroma(p);
    let t = color_terms(p);
    floats(&[
        c.chroma[0],
        c.chroma[1],
        t.opacity,
        t.contrast,
        t.brightness,
        t.gamma,
        c.similarity,
        c.smoothness,
        c.spill,
        0.0,
        0.0,
        0.0,
    ])
}

// ---------------------------------------------------------------------------------------------------------------
// Color Key

/// What `key_settings_update_v2` of the colour key computes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ColorKey {
    pub key_color: [f32; 3],
    pub similarity: f32,
    pub smoothness: f32,
}

/// `key_settings_update_v2`: green 0x00FF00, blue 0xFF0000, red 0x0000FF, magenta 0xFF00FF (the colour ints are
/// `0xBBGGRR`, so blue is pure blue here), `custom` takes `key_color`.
pub fn color_key(p: &Params) -> ColorKey {
    let similarity = pi(p, "similarity", 80, 1, 1000);
    let smoothness = pi(p, "smoothness", 50, 1, 1000);
    ColorKey {
        key_color: key_color(
            p,
            &[
                ("green", 0x0000_FF00),
                ("blue", 0x00FF_0000),
                ("red", 0x0000_00FF),
                ("magenta", 0x00FF_00FF),
            ],
        ),
        similarity: similarity as f32 / 1000.0,
        smoothness: smoothness as f32 / 1000.0,
    }
}

/// `cbuffer Params`: `float4 key_color; float opacity, contrast, brightness, gamma, similarity, smoothness;` and
/// padding to 48 bytes.
pub fn color_pack(p: &Params) -> Vec<u8> {
    let k = color_key(p);
    let t = color_terms(p);
    floats(&[
        k.key_color[0],
        k.key_color[1],
        k.key_color[2],
        1.0,
        t.opacity,
        t.contrast,
        t.brightness,
        t.gamma,
        k.similarity,
        k.smoothness,
        0.0,
        0.0,
    ])
}

// ---------------------------------------------------------------------------------------------------------------
// Luma Key

/// `cbuffer Params`: `float luma_max, luma_min, luma_max_smooth, luma_min_smooth;` (`luma_key_update`; the slider
/// ranges are 0 to 1).
pub fn luma_pack(p: &Params) -> Vec<u8> {
    floats(&[
        pf(p, "luma_max", 1.0, 0.0, 1.0),
        pf(p, "luma_min", 0.0, 0.0, 1.0),
        pf(p, "luma_max_smooth", 0.0, 0.0, 1.0),
        pf(p, "luma_min_smooth", 0.0, 0.0, 1.0),
    ])
}
