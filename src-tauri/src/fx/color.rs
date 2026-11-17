/*****************************************************************************
Copyright (C) 2016 by c3r1c3 <c3r1c3@nevermindonline.com>

Ported to Rust for Better Cam from OBS Studio, plugins/obs-filters/color-correction-filter.c
(the v2 filter, id "color_filter" version 2: `color_correction_filter_update_v2`,
`color_correction_filter_defaults_v2`, `color_correction_filter_properties_v2`). The pixel work is in
src/gpu/shaders/color.hlsl, a port of plugins/obs-filters/data/color_correction_filter.effect.

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

//! Color Correction (`color_filter`, plan 6.4): gamma, contrast, brightness, saturation, hue shift, opacity, colour
//! multiply and colour add.
//!
//! OBS builds one 4x4 matrix from the settings (`final = contrast * brightness * saturation * hue+opacity * color`,
//! in OBS's row-vector convention, so a pixel `(r, g, b, a)` is multiplied from the left) and a gamma exponent. The
//! shader applies `pow(rgb, gamma)` and then the matrix. [`matrix`] reproduces the construction step by step;
//! [`pack`] hands it to the shader.
//!
//! Differences to OBS that the pipeline forces (see `color.hlsl`): the texture holds straight alpha, so there is no
//! un-premultiply before and no premultiply after; the output stage composites the translucent result.

use std::f64::consts::PI;

use super::shader_filter::{ShaderFilter, ShaderFilterDef};
use super::{pc, pf, GpuFilter, Params};

pub const KIND: &str = "color_filter";

/// The settings the filter reads (OBS setting names).
pub const KEYS: &[&str] = &[
    "gamma",
    "contrast",
    "brightness",
    "saturation",
    "hue_shift",
    "opacity",
    "color_multiply",
    "color_add",
];

pub static DEF: ShaderFilterDef = ShaderFilterDef {
    kind: KIND,
    hlsl: include_str!("../gpu/shaders/color.hlsl"),
    pack,
    extra: &[],
};

pub fn make() -> Box<dyn GpuFilter> {
    Box::new(ShaderFilter::new(&DEF))
}

/// OBS `matrix4`: four rows `x, y, z, t`.
type Mat = [[f32; 4]; 4];

const IDENTITY: Mat = [
    [1.0, 0.0, 0.0, 0.0],
    [0.0, 1.0, 0.0, 0.0],
    [0.0, 0.0, 1.0, 0.0],
    [0.0, 0.0, 0.0, 1.0],
];

/// `matrix4_mul(dst, m1, m2)`: the ordinary product `m1 * m2`.
fn matrix4_mul(m1: &Mat, m2: &Mat) -> Mat {
    let mut out = [[0.0f32; 4]; 4];
    for (i, row) in out.iter_mut().enumerate() {
        for (j, v) in row.iter_mut().enumerate() {
            *v = (0..4).map(|k| m1[i][k] * m2[k][j]).sum();
        }
    }
    out
}

/// `gs_srgb_nonlinear_to_linear`.
pub fn srgb_to_linear(u: f32) -> f32 {
    if u <= 0.04045 {
        u / 12.92
    } else {
        ((u + 0.055) / 1.055).powf(2.4)
    }
}

const ROOT3: f32 = 0.57735;
const RED_WEIGHT: f32 = 0.299;
const GREEN_WEIGHT: f32 = 0.587;
const BLUE_WEIGHT: f32 = 0.114;

/// What `color_correction_filter_update_v2` computes: the gamma exponent and the final matrix.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Computed {
    pub gamma: f32,
    pub matrix: [[f32; 4]; 4],
}

/// `color_correction_filter_update_v2`, with the ranges of `color_correction_filter_properties_v2`.
pub fn matrix(p: &Params) -> Computed {
    // Gamma.
    let gamma = pf(p, "gamma", 0.0, -3.0, 3.0) as f64;
    let gamma = (if gamma < 0.0 { -gamma + 1.0 } else { 1.0 / (gamma + 1.0) }) as f32;

    // Contrast.
    let contrast = pf(p, "contrast", 0.0, -4.0, 4.0);
    let contrast = if contrast < 0.0 {
        1.0 / (-contrast + 1.0)
    } else {
        contrast + 1.0
    };
    let con_matrix: Mat = [
        [contrast, 0.0, 0.0, 0.0],
        [0.0, contrast, 0.0, 0.0],
        [0.0, 0.0, contrast, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ];

    // Brightness: the identity with the translation row set.
    let brightness = pf(p, "brightness", 0.0, -1.0, 1.0);
    let mut bright_matrix = IDENTITY;
    bright_matrix[3][0] = brightness;
    bright_matrix[3][1] = brightness;
    bright_matrix[3][2] = brightness;

    // Saturation.
    let saturation = pf(p, "saturation", 0.0, -1.0, 5.0) + 1.0;
    let one_minus_sat_red = (1.0 - saturation) * RED_WEIGHT;
    let one_minus_sat_green = (1.0 - saturation) * GREEN_WEIGHT;
    let one_minus_sat_blue = (1.0 - saturation) * BLUE_WEIGHT;
    let sat_val_red = one_minus_sat_red + saturation;
    let sat_val_green = one_minus_sat_green + saturation;
    let sat_val_blue = one_minus_sat_blue + saturation;
    let sat_matrix: Mat = [
        [sat_val_red, one_minus_sat_red, one_minus_sat_red, 0.0],
        [one_minus_sat_green, sat_val_green, one_minus_sat_green, 0.0],
        [one_minus_sat_blue, one_minus_sat_blue, sat_val_blue, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ];

    // Hue and opacity.
    let hue_shift = pf(p, "hue_shift", 0.0, -180.0, 180.0);
    let opacity = pf(p, "opacity", 1.0, 0.0, 1.0);
    // `0.5f * (float)(hue_shift / (180.0f / M_PI))`: the division is in double precision in C.
    let half_angle = 0.5f32 * (hue_shift as f64 / (180.0f64 / PI)) as f32;

    // Pseudo-quaternion to matrix. All three components of the axis are equal, so one value stands for each.
    let rot_quad1 = ROOT3 * (half_angle as f64).sin() as f32;
    let rot_quaternion = [rot_quad1; 3];
    let rot_quaternion_w = (half_angle as f64).cos() as f32;
    let half_unit = [0.5f32; 3];

    let cross: [f32; 3] = rot_quaternion.map(|v| v * v);
    let square: [f32; 3] = rot_quaternion.map(|v| v * v).map(|v| v * 2.0);
    let wimag: [f32; 3] = rot_quaternion.map(|v| v * rot_quaternion_w);
    let diag: [f32; 3] = [0, 1, 2].map(|i| half_unit[i] - square[i]);
    let a_line: [f32; 3] = [0, 1, 2].map(|i| cross[i] + wimag[i]);
    let b_line: [f32; 3] = [0, 1, 2].map(|i| cross[i] - wimag[i]);

    let hue_op_matrix: Mat = [
        [diag[0] * 2.0, b_line[2] * 2.0, a_line[1] * 2.0, 0.0],
        [a_line[2] * 2.0, diag[1] * 2.0, b_line[0] * 2.0, 0.0],
        [b_line[1] * 2.0, a_line[0] * 2.0, diag[2] * 2.0, 0.0],
        [0.0, 0.0, 0.0, opacity],
    ];

    // Colour multiply and add (`vec4_from_rgba_srgb`: the colour picker's values are sRGB, the maths are linear).
    let multiply = pc(p, "color_multiply", 0x00FF_FFFF).map(srgb_to_linear);
    let add = pc(p, "color_add", 0x0000_0000).map(srgb_to_linear);
    let mut color_matrix = IDENTITY;
    color_matrix[0][0] = multiply[0];
    color_matrix[1][1] = multiply[1];
    color_matrix[2][2] = multiply[2];
    color_matrix[3][0] = add[0];
    color_matrix[3][1] = add[1];
    color_matrix[3][2] = add[2];

    // Contrast & brightness, then saturation, then hue & opacity, lastly the colour wash.
    let mut final_matrix = matrix4_mul(&con_matrix, &bright_matrix);
    final_matrix = matrix4_mul(&final_matrix, &sat_matrix);
    final_matrix = matrix4_mul(&final_matrix, &hue_op_matrix);
    final_matrix = matrix4_mul(&final_matrix, &color_matrix);

    Computed {
        gamma,
        matrix: final_matrix,
    }
}

/// `cbuffer Params`: `row_major float4x4 color_matrix; float gamma; float3 pad;`.
pub fn pack(p: &Params) -> Vec<u8> {
    let c = matrix(p);
    c.matrix
        .iter()
        .flatten()
        .copied()
        .chain([c.gamma, 0.0, 0.0, 0.0])
        .flat_map(f32::to_le_bytes)
        .collect()
}
