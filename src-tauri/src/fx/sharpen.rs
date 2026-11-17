/*****************************************************************************
Based on the libretro shader sharpness.cg, converted to an OBS effect file by Nibbles.

Ported to Rust for Better Cam from OBS Studio, plugins/obs-filters/sharpness-filter.c (the v2 filter, id
"sharpness_filter" version 2: `sharpness_update`, `sharpness_defaults`, `sharpness_properties`). The pixel work is in
src/gpu/shaders/sharpen.hlsl, a port of plugins/obs-filters/data/sharpness.effect.

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

//! Sharpen (`sharpness_filter`, plan 6.4): one setting, `sharpness`, 0 to 1, default 0.08.
//!
//! The kernel is the one in `sharpness.effect`: `8 * E` minus the eight neighbours, times `sharpness`, added back to
//! the pixel where the pixel differs from both sides of a pair of neighbours. OBS passes the texel size
//! (`texture_width`, `texture_height`) to shift the sample points; the shader reads the neighbouring texels directly,
//! which are the same points. See `sharpen.hlsl` for what the straight-alpha, gamma-encoded pipeline changes.

use super::shader_filter::{ShaderFilter, ShaderFilterDef};
use super::{pf, GpuFilter, Params};

pub const KIND: &str = "sharpness_filter";

/// The settings the filter reads (OBS setting names).
pub const KEYS: &[&str] = &["sharpness"];

pub static DEF: ShaderFilterDef = ShaderFilterDef {
    kind: KIND,
    hlsl: include_str!("../gpu/shaders/sharpen.hlsl"),
    pack,
    extra: &[],
};

pub fn make() -> Box<dyn GpuFilter> {
    Box::new(ShaderFilter::new(&DEF))
}

/// `cbuffer Params`: `float sharpness; float3 pad;`. The slider's range of `sharpness_properties` is 0 to 1.
pub fn pack(p: &Params) -> Vec<u8> {
    let sharpness = pf(p, "sharpness", 0.08, 0.0, 1.0);
    [sharpness, 0.0, 0.0, 0.0]
        .into_iter()
        .flat_map(f32::to_le_bytes)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::{json, Value};

    use super::super::{Chain, FilterCfg};
    use super::*;
    use crate::gpu::pipeline_tests::{fill_rgba, read_rgba, warp};
    use crate::gpu::Renderer;

    type Pixel = [u8; 4];

    fn run(r: &Arc<Renderer>, params: Value, w: u32, h: u32, px: &[Pixel]) -> Vec<Pixel> {
        let mut chain = Chain::new();
        chain.apply(
            r,
            &[FilterCfg {
                id: "s".into(),
                kind: KIND.into(),
                enabled: true,
                params: params.as_object().cloned().unwrap_or_default(),
            }],
        );
        let tex = r.texture(w, h).unwrap();
        fill_rgba(r, &tex, |x, y| px[(y * w + x) as usize]);
        let out = chain.run(r, tex).unwrap();
        assert_eq!((out.width, out.height), (w, h));
        read_rgba(r, &out).chunks(4).map(|p| [p[0], p[1], p[2], p[3]]).collect()
    }

    fn dec(code: f64) -> f64 {
        let u = code / 255.0;
        if u <= 0.04045 {
            u / 12.92
        } else {
            ((u + 0.055) / 1.055).powf(2.4)
        }
    }

    fn enc(lin: f64) -> f64 {
        let c = lin.clamp(0.0, 1.0);
        let u = if c <= 0.0031308 {
            12.92 * c
        } else {
            1.055 * c.powf(1.0 / 2.4) - 0.055
        };
        u * 255.0
    }

    /// `sharpness.effect` as plain steps. A pixel is linear light, premultiplied, four numbers.
    fn sample(px: &[Pixel], w: i32, h: i32, x: i32, y: i32) -> [f64; 4] {
        let p = px[(y.clamp(0, h - 1) * w + x.clamp(0, w - 1)) as usize];
        let a = p[3] as f64 / 255.0;
        [dec(p[0] as f64) * a, dec(p[1] as f64) * a, dec(p[2] as f64) * a, a]
    }

    fn distance(a: [f64; 4], b: [f64; 4]) -> f64 {
        a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum::<f64>().sqrt()
    }

    fn reference(px: &[Pixel], w: i32, h: i32, sharpness: f64) -> Vec<[f64; 4]> {
        let mut out = Vec::new();
        for y in 0..h {
            for x in 0..w {
                let e = sample(px, w, h, x, y);
                let b = sample(px, w, h, x, y - 1);
                let d = sample(px, w, h, x - 1, y);
                let f = sample(px, w, h, x + 1, y);
                let hh = sample(px, w, h, x, y + 1);
                // 8 * E - the eight neighbours
                let mut color = e.map(|v| 8.0 * v);
                for (dx, dy) in [(-1, -1), (0, -1), (1, -1), (-1, 0), (1, 0), (-1, 1), (0, 1), (1, 1)] {
                    let n = sample(px, w, h, x + dx, y + dy);
                    for i in 0..4 {
                        color[i] -= n[i];
                    }
                }
                let sharpen = (distance(e, f) != 0.0 && distance(e, d) != 0.0)
                    || (distance(e, b) != 0.0 && distance(e, hh) != 0.0);
                let r: [f64; 4] = if sharpen {
                    [0, 1, 2, 3].map(|i| (e[i] + color[i] * sharpness).clamp(0.0, 1.0))
                } else {
                    e
                };
                // straight alpha, gamma-encoded
                let a = r[3];
                let rgb = [0, 1, 2].map(|i| if a > 0.0 { r[i] / a } else { 0.0 });
                out.push([enc(rgb[0]), enc(rgb[1]), enc(rgb[2]), a * 255.0]);
            }
        }
        out
    }

    fn worst(got: &[Pixel], want: &[[f64; 4]]) -> f64 {
        got.iter()
            .zip(want)
            .flat_map(|(g, w)| g.iter().zip(w).map(|(g, w)| (*g as f64 - w).abs()))
            .fold(0.0, f64::max)
    }

    /// A 24 x 12 picture: a vertical edge, a horizontal ramp, a bright dot, a coloured block and noise, all opaque.
    fn edges() -> (i32, i32, Vec<Pixel>) {
        let (w, h) = (24i32, 12i32);
        let mut seed = 0x00c0_ffeeu32;
        let mut noise = move || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 24) as u8
        };
        let mut px = Vec::new();
        for y in 0..h {
            for x in 0..w {
                let p = if x < 6 {
                    [40, 40, 40, 255]
                } else if x < 12 {
                    [200, 200, 200, 255]
                } else if x < 16 {
                    [(x * 16) as u8, (y * 20) as u8, 90, 255]
                } else if (18..21).contains(&x) && (4..8).contains(&y) {
                    [230, 60, 20, 255]
                } else {
                    [noise(), noise(), noise(), 255]
                };
                px.push(p);
            }
        }
        px[(6 * w + 4) as usize] = [255, 255, 255, 255];
        (w, h, px)
    }

    #[test]
    fn sharpness_zero_is_the_identity() {
        let r = warp();
        let (w, h, mut px) = edges();
        // translucent pixels too
        for (i, p) in px.iter_mut().enumerate() {
            if i % 5 == 0 {
                p[3] = [32, 90, 128, 200][i / 5 % 4];
            }
        }
        let out = run(&r, json!({ "sharpness": 0 }), w as u32, h as u32, &px);
        let worst = px
            .iter()
            .zip(&out)
            .flat_map(|(a, b)| a.iter().zip(b).map(|(a, b)| (*a as i32 - *b as i32).abs()))
            .max()
            .unwrap();
        assert!(worst <= 1, "sharpness 0 changes the picture by {worst}");
    }

    #[test]
    fn matches_the_obs_kernel_on_an_edge_picture() {
        let r = warp();
        let (w, h, px) = edges();
        for sharpness in [0.08, 0.5, 1.0] {
            let out = run(&r, json!({ "sharpness": sharpness }), w as u32, h as u32, &px);
            let want = reference(&px, w, h, sharpness);
            let diff = worst(&out, &want);
            assert!(diff <= 2.0, "sharpness {sharpness}: worst difference {diff}");
        }
        // the default is 0.08
        let a = run(&r, json!({}), w as u32, h as u32, &px);
        let b = run(&r, json!({ "sharpness": 0.08 }), w as u32, h as u32, &px);
        assert_eq!(a, b);
    }

    #[test]
    fn a_detail_gets_contrast_and_a_flat_or_stepped_area_stays() {
        let r = warp();
        // 5 x 5 flat grey with a lighter dot in the middle and a darker one in the corner region
        let mut px: Vec<Pixel> = vec![[100, 100, 100, 255]; 25];
        px[12] = [130, 130, 130, 255];
        px[6] = [70, 70, 70, 255];
        let out = run(&r, json!({ "sharpness": 1.0 }), 5, 5, &px);
        assert!(out[12][0] > 130, "the light dot gets lighter: {:?}", out[12]);
        assert!(out[6][0] < 70, "the dark dot gets darker: {:?}", out[6]);
        // a pixel that equals one neighbour of each pair is left alone, as in OBS: flat grey, the neighbours of the
        // dots in line, and the two sides of a step edge
        for i in [0usize, 4, 20, 24, 13, 11, 17] {
            assert_eq!(out[i], px[i], "pixel {i}");
        }
        let row: Vec<Pixel> = (0..8).map(|x| if x < 4 { [80, 80, 80, 255] } else { [160, 160, 160, 255] }).collect();
        assert_eq!(run(&r, json!({ "sharpness": 1.0 }), 8, 1, &row), row);
    }

    #[test]
    fn the_kernel_reaches_alpha_and_premultiplies_like_obs() {
        let r = warp();
        // opaque white columns alternating with faint orange ones: every pixel differs from its left and right
        // neighbours, so the kernel runs, and it works on alpha as well
        let (w, h) = (8i32, 4i32);
        let mut px = Vec::new();
        for _ in 0..h {
            for x in 0..w {
                px.push(if x % 2 == 0 { [255, 255, 255, 255] } else { [200, 120, 40, 100] });
            }
        }
        let out = run(&r, json!({ "sharpness": 0.7 }), w as u32, h as u32, &px);
        let want = reference(&px, w, h, 0.7);
        assert!(worst(&out, &want) <= 2.0, "worst {}", worst(&out, &want));
        assert!(out[1][3] < 100, "alpha of a faint column drops: {:?}", out[1]);
        assert_eq!(out[2][3], 255);
    }

    #[test]
    fn a_small_picture_does_not_read_outside() {
        let r = warp();
        for (w, h) in [(1u32, 1u32), (2, 1), (1, 3), (3, 3)] {
            let px: Vec<Pixel> = (0..w * h).map(|i| [(i * 70) as u8, 50, 200, 255]).collect();
            let out = run(&r, json!({ "sharpness": 1.0 }), w, h, &px);
            let want = reference(&px, w as i32, h as i32, 1.0);
            assert!(worst(&out, &want) <= 2.0, "{w}x{h}");
        }
    }

    #[test]
    fn out_of_range_values_are_clamped_and_bad_types_use_the_default() {
        let r = warp();
        let (w, h, px) = edges();
        let at = |v: Value| run(&r, json!({ "sharpness": v }), w as u32, h as u32, &px);
        assert_eq!(at(json!(5.0)), at(json!(1.0)));
        assert_eq!(at(json!(-3.0)), at(json!(0.0)));
        assert_eq!(at(json!("high")), at(json!(0.08)));
    }
}
