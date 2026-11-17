//! Tests of Image Mask/Blend on WARP, with real picture files through the asset service.
//!
//! The CPU reference is the OBS effects written as plain steps on one pixel: the placement of the image is worked out
//! as "scale to fit, centre, repeat the edge pixels" (not with the shader's mul/add), the picture is sampled bilinearly
//! in linear light.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::*;
use crate::fx::assets::testgen::{write_png, TempDir};
use crate::fx::assets::Assets;
use crate::fx::{Chain, FilterCfg, FilterError};
use crate::gpu::pipeline_tests::{fill_rgba, read_rgba, warp};

type Pixel = [u8; 4];
type Log = Arc<Mutex<Vec<FilterError>>>;

fn near(got: Pixel, want: [f64; 4], tol: f64) -> bool {
    got.iter().zip(want).all(|(g, w)| (*g as f64 - w).abs() <= tol)
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

fn obs_color(c: [u8; 3]) -> u32 {
    c[0] as u32 | (c[1] as u32) << 8 | (c[2] as u32) << 16
}

/// A seeded pseudo random RGBA picture; alpha from `alpha_floor` up.
fn noise(w: u32, h: u32, seed: u32, alpha_floor: u8) -> Vec<Pixel> {
    let mut s = seed;
    let mut next = move || {
        s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (s >> 24) as u8
    };
    (0..w * h)
        .map(|_| {
            let (r, g, b) = (next(), next(), next());
            let alpha = alpha_floor as u32 + next() as u32 * (255 - alpha_floor as u32) / 255;
            [r, g, b, alpha as u8]
        })
        .collect()
}

fn flatten(px: &[Pixel]) -> Vec<u8> {
    px.iter().flatten().copied().collect()
}

/// A chain with the asset service, one Image Mask/Blend filter and a folder for its files.
struct Rig {
    r: Arc<crate::gpu::Renderer>,
    chain: Chain,
    log: Log,
    dir: TempDir,
    // dropped last: the chain's handles point into it
    _assets: Assets,
}

impl Rig {
    fn new(name: &str) -> Rig {
        let r = warp();
        let log: Log = Arc::default();
        let sink = log.clone();
        let assets = Assets::with_scan_interval(
            r.gpu.clone(),
            Arc::new(move |e: FilterError| sink.lock().unwrap().push(e)),
            Duration::from_millis(100),
        );
        let mut chain = Chain::new();
        chain.set_assets(assets.client());
        Rig {
            r,
            chain,
            log,
            dir: TempDir::new(name),
            _assets: assets,
        }
    }

    fn png(&self, name: &str, w: u32, h: u32, px: &[Pixel]) -> String {
        let f = self.dir.join(name);
        write_png(&f, w, h, &flatten(px));
        f.to_string_lossy().into_owned()
    }

    fn apply(&mut self, params: Value) {
        self.chain.apply(
            &self.r,
            &[FilterCfg {
                id: "mask".into(),
                kind: KIND.into(),
                enabled: true,
                params: params.as_object().cloned().unwrap_or_default(),
            }],
        );
    }

    fn frame(&mut self, w: u32, h: u32, px: &[Pixel]) -> Vec<Pixel> {
        let tex = self.r.texture(w, h).unwrap();
        fill_rgba(&self.r, &tex, |x, y| px[(y * w + x) as usize]);
        let out = self.chain.run(&self.r, tex).unwrap();
        assert_eq!((out.width, out.height), (w, h));
        assert!(self.chain.take_errors().is_empty(), "the filter itself never fails");
        read_rgba(&self.r, &out)
            .chunks(4)
            .map(|p| [p[0], p[1], p[2], p[3]])
            .collect()
    }

    /// Frames until the picture differs from `px` (the image has arrived), at most five seconds.
    fn until_changed(&mut self, w: u32, h: u32, px: &[Pixel]) -> Vec<Pixel> {
        let end = Instant::now() + Duration::from_secs(5);
        loop {
            let out = self.frame(w, h, px);
            if out != px {
                return out;
            }
            assert!(Instant::now() < end, "the image never changed the picture");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn wait_for_error(&self, text: &str) {
        let end = Instant::now() + Duration::from_secs(5);
        while !self.log.lock().unwrap().iter().any(|e| e.message.contains(text)) {
            assert!(Instant::now() < end, "no error containing {text:?}: {:?}", self.log.lock().unwrap());
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn errors(&self) -> Vec<FilterError> {
        self.log.lock().unwrap().iter().filter(|e| !e.message.is_empty()).cloned().collect()
    }
}

// ---------------------------------------------------------------------------------------------------------------
// The reference

#[derive(Clone)]
struct Ref<'a> {
    ty: MaskType,
    mask: &'a [Pixel],
    mw: u32,
    mh: u32,
    color: [u8; 3],
    opacity: f64,
    stretch: bool,
}

impl Ref<'_> {
    fn json(&self, path: &str) -> Value {
        json!({
            "type": self.ty.effect(), "image_path": path, "color": obs_color(self.color),
            "opacity": self.opacity, "stretch": self.stretch,
        })
    }

    /// The mask texel as linear colour and straight alpha, the position clamped to the image.
    fn texel(&self, x: i64, y: i64) -> [f64; 4] {
        let x = x.clamp(0, self.mw as i64 - 1);
        let y = y.clamp(0, self.mh as i64 - 1);
        let p = self.mask[(y * self.mw as i64 + x) as usize];
        [dec(p[0] as f64), dec(p[1] as f64), dec(p[2] as f64), p[3] as f64 / 255.0]
    }

    /// Bilinear sample at `(u, v)` in image pixels, texel `i` centred at `i + 0.5`.
    fn sample(&self, u: f64, v: f64) -> [f64; 4] {
        let (tx, ty) = (u - 0.5, v - 0.5);
        let (x0, y0) = (tx.floor(), ty.floor());
        let (fx, fy) = (tx - x0, ty - y0);
        let mut out = [0.0; 4];
        for (dy, wy) in [(0, 1.0 - fy), (1, fy)] {
            for (dx, wx) in [(0, 1.0 - fx), (1, fx)] {
                let t = self.texel(x0 as i64 + dx, y0 as i64 + dy);
                for (o, c) in out.iter_mut().zip(t) {
                    *o += c * wx * wy;
                }
            }
        }
        out
    }

    /// The expected pixel (x, y) of a `w` x `h` picture, in code values.
    fn apply(&self, input: &[Pixel], w: u32, h: u32, x: u32, y: u32) -> [f64; 4] {
        let p = input[(y * w + x) as usize];
        let a = p[3] as f64 / 255.0;
        let lin: [f64; 3] = if a > 0.0 {
            [dec(p[0] as f64), dec(p[1] as f64), dec(p[2] as f64)]
        } else {
            [0.0; 3]
        };
        let tint = self.color.map(|v| dec(v as f64));
        let mut rgb = [lin[0] * tint[0], lin[1] * tint[1], lin[2] * tint[2]];
        let mut alpha = a * self.opacity;

        // where the picture's pixel lands on the image, in image pixels
        let (px, py) = (x as f64 + 0.5, y as f64 + 0.5);
        let (u, v) = if self.stretch {
            (px / w as f64 * self.mw as f64, py / h as f64 * self.mh as f64)
        } else {
            // scale to fit inside the picture, centred
            let s = (w as f64 / self.mw as f64).min(h as f64 / self.mh as f64);
            let (ox, oy) = ((w as f64 - self.mw as f64 * s) / 2.0, (h as f64 - self.mh as f64 * s) / 2.0);
            ((px - ox) / s, (py - oy) / s)
        };
        let t = self.sample(u, v);
        match self.ty {
            MaskType::Color => alpha *= (t[0] + t[1] + t[2]) / 3.0,
            MaskType::Alpha => alpha *= t[3],
            MaskType::Multiply => rgb = [0, 1, 2].map(|i| (rgb[i] * t[i]).clamp(0.0, 1.0)),
            MaskType::Add => rgb = [0, 1, 2].map(|i| (rgb[i] + t[i]).clamp(0.0, 1.0)),
            MaskType::Subtract => rgb = [0, 1, 2].map(|i| (rgb[i] - t[i]).clamp(0.0, 1.0)),
        }
        [enc(rgb[0]), enc(rgb[1]), enc(rgb[2]), alpha.clamp(0.0, 1.0) * 255.0]
    }
}

fn check(name: &str, set: &Ref, input: &[Pixel], out: &[Pixel], w: u32, h: u32) {
    let mut worst = 0.0f64;
    for y in 0..h {
        for x in 0..w {
            let got = out[(y * w + x) as usize];
            let want = set.apply(input, w, h, x, y);
            worst = got.iter().zip(want).fold(worst, |m, (g, e)| m.max((*g as f64 - e).abs()));
            assert!(
                near(got, want, 2.0),
                "{name}: pixel ({x}, {y}) {:?} -> {got:?}, expected about {want:?}",
                input[(y * w + x) as usize]
            );
        }
    }
    eprintln!("mask {name}: worst difference {worst:.2} code values");
}

const WHITE: Pixel = [255, 255, 255, 255];
const BLACK: Pixel = [0, 0, 0, 255];

/// `cols` columns of an image `w` x `h`: the first `white` of them white, the rest black.
fn bars(w: u32, h: u32, white: u32) -> Vec<Pixel> {
    (0..w * h).map(|i| if i % w < white { WHITE } else { BLACK }).collect()
}

// ---------------------------------------------------------------------------------------------------------------
// The mask types

#[test]
fn color_mask_left_white_right_black() {
    let mut rig = Rig::new("mask-color");
    let path = rig.png("bars.png", 8, 4, &bars(8, 4, 4));
    rig.apply(json!({ "type": "mask_color_filter.effect", "image_path": path }));
    let input = vec![[100, 150, 200, 255]; 32];
    let out = rig.until_changed(8, 4, &input);
    for y in 0..4 {
        for x in 0..8 {
            let p = out[y * 8 + x];
            if x < 4 {
                assert_eq!(p, [100, 150, 200, 255], "left ({x}, {y})");
            } else {
                assert_eq!(p[3], 0, "right ({x}, {y}): {p:?}");
            }
        }
    }
    assert!(rig.errors().is_empty());
}

#[test]
fn color_mask_reads_the_mask_in_linear_light() {
    // a mid gray mask (128 = 0.2159 in linear light) gives an alpha of 0.2159, not 0.5
    let mut rig = Rig::new("mask-gray");
    let path = rig.png("gray.png", 4, 4, &[[128, 128, 128, 255]; 16]);
    rig.apply(json!({ "image_path": path }));
    let input = vec![[10, 200, 30, 255]; 16];
    let out = rig.until_changed(4, 4, &input);
    assert!(out.iter().all(|p| near(*p, [10.0, 200.0, 30.0, 55.06], 1.0)), "{:?}", out[0]);
}

#[test]
fn alpha_mask_uses_the_alpha_of_the_picture() {
    let mut rig = Rig::new("mask-alpha");
    // columns 0-1 opaque, 2-5 half, 6-7 clear; the colours are irrelevant
    let img: Vec<Pixel> = (0..32)
        .map(|i| match i % 8 {
            0 | 1 => [10, 20, 30, 255],
            2..=5 => [40, 50, 60, 128],
            _ => [70, 80, 90, 0],
        })
        .collect();
    let path = rig.png("alpha.png", 8, 4, &img);
    rig.apply(json!({ "type": "mask_alpha_filter.effect", "image_path": path }));
    let input = vec![[100, 150, 200, 255]; 32];
    let out = rig.until_changed(8, 4, &input);
    for y in 0..4 {
        for x in 0..8 {
            let p = out[y * 8 + x];
            let alpha = match x {
                0 | 1 => 255.0,
                2..=5 => 128.0,
                _ => 0.0,
            };
            assert!(near(p, [100.0, 150.0, 200.0, alpha], 1.0), "({x}, {y}): {p:?}");
        }
    }
}

#[test]
fn blend_modes_hand_checked() {
    // image 128 gray (0.2159 linear), picture 200 gray (0.5776 linear)
    let input = vec![[200, 200, 200, 255]; 16];
    for (ty, want) in [
        // 0.5776 * 0.2159 = 0.1247 -> code 99
        (MaskType::Multiply, 99.0),
        // 0.5776 + 0.2159 = 0.7935 -> code 230
        (MaskType::Add, 230.0),
        // 0.5776 - 0.2159 = 0.3617 -> code 162
        (MaskType::Subtract, 162.0),
    ] {
        let mut rig = Rig::new("mask-blend-hand");
        let path = rig.png("gray.png", 4, 4, &[[128, 128, 128, 255]; 16]);
        rig.apply(json!({ "type": ty.effect(), "image_path": path }));
        let out = rig.until_changed(4, 4, &input);
        assert!(out.iter().all(|p| near(*p, [want, want, want, 255.0], 1.5)), "{ty:?}: {:?}", out[0]);
    }
    // add saturates and subtract floors at zero
    let mut rig = Rig::new("mask-blend-clamp");
    let path = rig.png("white.png", 2, 2, &[WHITE; 4]);
    rig.apply(json!({ "type": "blend_add_filter.effect", "image_path": path }));
    let out = rig.until_changed(2, 2, &[[10, 20, 30, 255]; 4]);
    assert!(out.iter().all(|p| near(*p, [255.0, 255.0, 255.0, 255.0], 0.0)), "{:?}", out[0]);
    rig.apply(json!({ "type": "blend_sub_filter.effect", "image_path": path }));
    let out = rig.until_changed(2, 2, &[[10, 20, 30, 255]; 4]);
    assert!(out.iter().all(|p| near(*p, [0.0, 0.0, 0.0, 255.0], 0.0)), "{:?}", out[0]);
}

#[test]
fn every_type_matches_the_reference_with_stretch_on_and_off() {
    let mut rig = Rig::new("mask-ref");
    // an image whose aspect (5 x 3) is not the picture's (16 x 8), with translucent texels, over a noisy picture
    let (mw, mh) = (5u32, 3u32);
    let mask = noise(mw, mh, 77, 40);
    let path = rig.png("noise.png", mw, mh, &mask);
    let (w, h) = (16u32, 8u32);
    let input = noise(w, h, 5, 90);
    for ty in MaskType::ALL {
        for stretch in [false, true] {
            let set = Ref { ty, mask: &mask, mw, mh, color: [255, 255, 255], opacity: 1.0, stretch };
            rig.apply(set.json(&path));
            let out = rig.until_changed(w, h, &input);
            check(&format!("{ty:?} stretch={stretch}"), &set, &input, &out, w, h);
        }
    }
    // a picture taller than the image
    let (w, h) = (6u32, 14u32);
    let input = noise(w, h, 9, 200);
    for ty in MaskType::ALL {
        let set = Ref { ty, mask: &mask, mw, mh, color: [255, 255, 255], opacity: 1.0, stretch: false };
        rig.apply(set.json(&path));
        let out = rig.until_changed(w, h, &input);
        check(&format!("{ty:?} tall"), &set, &input, &out, w, h);
    }
    assert!(rig.errors().is_empty());
}

#[test]
fn mask_color_and_opacity() {
    let mut rig = Rig::new("mask-color-opacity");
    let path = rig.png("white.png", 2, 2, &[WHITE; 4]);
    // 0x0000FF is red in the OBS colour int: the picture's colour is multiplied by it, alpha by the opacity
    rig.apply(json!({ "image_path": path, "color": 0x0000_00FFu32, "opacity": 0.5 }));
    let out = rig.until_changed(2, 2, &[[200, 200, 200, 255]; 4]);
    assert!(out.iter().all(|p| near(*p, [200.0, 0.0, 0.0, 127.5], 1.0)), "{:?}", out[0]);
    // the picked colour is sRGB, applied in linear light: 128 gray is 0.2159, so a 200 picture (0.5776) becomes
    // 0.1247, code 99
    rig.apply(json!({ "image_path": path, "color": obs_color([128, 128, 128]), "opacity": 1.0 }));
    let out = rig.until_changed(2, 2, &[[200, 200, 200, 255]; 4]);
    assert!(out.iter().all(|p| near(*p, [99.0, 99.0, 99.0, 255.0], 1.0)), "{:?}", out[0]);
    // opacity 0 makes everything transparent, whatever the type
    for ty in MaskType::ALL {
        rig.apply(json!({ "type": ty.effect(), "image_path": path, "opacity": 0 }));
        let out = rig.until_changed(2, 2, &[[200, 200, 200, 255]; 4]);
        assert!(out.iter().all(|p| p[3] == 0), "{ty:?}: {:?}", out[0]);
    }
    // colour and opacity against the reference, on every type, with a picture with alpha
    let mask = noise(4, 4, 3, 0);
    let mask_path = rig.png("mask.png", 4, 4, &mask);
    let input = noise(8, 8, 11, 60);
    for ty in MaskType::ALL {
        let set = Ref { ty, mask: &mask, mw: 4, mh: 4, color: [250, 120, 40], opacity: 0.7, stretch: true };
        rig.apply(set.json(&mask_path));
        let out = rig.until_changed(8, 8, &input);
        check(&format!("{ty:?} color+opacity"), &set, &input, &out, 8, 8);
    }
}

#[test]
fn stretch_off_keeps_the_aspect_of_the_image_and_on_stretches_it() {
    // An 8 x 8 image with white in its two left columns, over a 16 x 8 picture (twice as wide).
    let mask = bars(8, 8, 2);
    let input = vec![[90, 160, 230, 255]; 16 * 8];
    let alpha_of = |out: &[Pixel], x: usize| out[x][3];

    let mut rig = Rig::new("mask-stretch");
    let path = rig.png("bars.png", 8, 8, &mask);

    // stretch off: the image stays 8 x 8 and sits in the middle (x 4 to 11); its edge pixels repeat on both sides.
    // Picture pixel x reads image column x - 4: columns 0 and 1 are white, so x <= 5 is opaque (the left of the
    // image repeats the white edge), x >= 6 is transparent.
    rig.apply(json!({ "image_path": path, "stretch": false }));
    let out = rig.until_changed(16, 8, &input);
    for x in 0..16 {
        let want = if x <= 5 { 255 } else { 0 };
        assert_eq!(alpha_of(&out, x), want, "stretch off, x = {x}: {:?}", out[..16].iter().map(|p| p[3]).collect::<Vec<_>>());
    }
    // the colour is not touched by a colour mask
    assert_eq!(&out[0][..3], &[90, 160, 230]);

    // stretch on: the 8 columns spread over 16 pixels, each image column covers two picture pixels; the white
    // columns end at x = 4 and the bilinear filter blends the edge: x = 3 is three quarters white, x = 4 a quarter
    rig.apply(json!({ "image_path": path, "stretch": true }));
    let end = Instant::now() + Duration::from_secs(5);
    let out = loop {
        let out = rig.frame(16, 8, &input);
        if out[5][3] == 0 && out[0][3] > 0 {
            break out;
        }
        assert!(Instant::now() < end, "the setting never applied: {:?}", out[..16].iter().map(|p| p[3]).collect::<Vec<_>>());
        std::thread::sleep(Duration::from_millis(5));
    };
    let alphas: Vec<u8> = out[..16].iter().map(|p| p[3]).collect();
    assert_eq!(&alphas[..3], &[255, 255, 255], "{alphas:?}");
    let linear = |frac: f64| (frac * 255.0).round() as i32;
    assert!((alphas[3] as i32 - linear(0.75)).abs() <= 2, "{alphas:?}");
    assert!((alphas[4] as i32 - linear(0.25)).abs() <= 2, "{alphas:?}");
    assert!(alphas[5..].iter().all(|a| *a == 0), "{alphas:?}");

    // the same in the other direction: a tall picture (8 x 16) and a square image with white only in its top two rows
    let rows: Vec<Pixel> = (0..64).map(|i| if i / 8 < 2 { WHITE } else { BLACK }).collect();
    let path = rig.png("rows.png", 8, 8, &rows);
    let tall = vec![[90, 160, 230, 255]; 8 * 16];
    rig.apply(json!({ "image_path": path, "stretch": false }));
    let end = Instant::now() + Duration::from_secs(5);
    let out = loop {
        let out = rig.frame(8, 16, &tall);
        if out[6 * 8][3] == 0 && out[0][3] > 0 {
            break out;
        }
        assert!(Instant::now() < end, "the tall image never arrived");
        std::thread::sleep(Duration::from_millis(5));
    };
    for y in 0..16usize {
        let want = if y <= 5 { 255 } else { 0 };
        assert_eq!(out[y * 8][3], want, "tall, stretch off, y = {y}");
    }
}

#[test]
fn the_placement_arithmetic_of_obs() {
    // picture 16 x 8, image 8 x 8: fit to the height (the picture is wider than the image), centred horizontally
    let (mul, add) = aspect_fit((16.0, 8.0), (8.0, 8.0));
    assert_eq!((mul, add), ([2.0, 1.0], [-0.5, 0.0]));
    // picture 8 x 16, image 8 x 8: fit to the width, centred vertically
    let (mul, add) = aspect_fit((8.0, 16.0), (8.0, 8.0));
    assert_eq!((mul, add), ([1.0, 2.0], [0.0, -0.5]));
    // the same aspect: the identity
    let (mul, add) = aspect_fit((32.0, 16.0), (8.0, 4.0));
    assert_eq!((mul, add), ([1.0, 1.0], [0.0, 0.0]));
    // a wide image (4 x 1) on a 16 x 8 picture: fit to the width (the picture is narrower in aspect)
    let (mul, add) = aspect_fit((16.0, 8.0), (4.0, 1.0));
    // the image is 16 x 4 on the picture: uv.y * 2 - 0.5
    assert_eq!((mul, add), ([1.0, 2.0], [0.0, -0.5]));
}

#[test]
fn settings_follow_the_obs_update_function() {
    let s = settings(&Params::new());
    assert_eq!(s.kind, MaskType::Color);
    assert_eq!(s.color, [1.0, 1.0, 1.0, 1.0]);
    assert!(!s.stretch);
    // the colour is converted from sRGB to linear (vec4_from_rgba_srgb), the opacity is not
    let p = json!({ "color": obs_color([128, 255, 0]), "opacity": 0.25, "stretch": true, "type": "blend_sub_filter.effect" });
    let s = settings(p.as_object().unwrap());
    assert_eq!(s.kind, MaskType::Subtract);
    assert!((s.color[0] as f64 - dec(128.0)).abs() < 1e-5 && s.color[1] == 1.0 && s.color[2] == 0.0);
    assert_eq!(s.color[3], 0.25);
    assert!(s.stretch);
    // unknown type falls back to the default, opacity clamps
    let p = json!({ "type": "nope.effect", "opacity": 9 });
    let s = settings(p.as_object().unwrap());
    assert_eq!((s.kind, s.color[3]), (MaskType::Color, 1.0));
    for t in MaskType::ALL {
        assert_eq!(MaskType::from_effect(t.effect()), Some(t));
    }
    assert_eq!(pack([1.0; 4], [1.0; 2], [0.0; 2], (3, 4)).len(), 48);
}

// ---------------------------------------------------------------------------------------------------------------
// Failure

#[test]
fn no_image_leaves_the_picture_alone() {
    let input = noise(8, 4, 21, 90);

    // no service at all (a chain built without one), and an empty path with one
    let r = warp();
    let mut chain = Chain::new();
    chain.apply(
        &r,
        &[FilterCfg { id: "m".into(), kind: KIND.into(), enabled: true, params: json!({ "image_path": "C:\\no\\such.png" }).as_object().cloned().unwrap() }],
    );
    let tex = r.texture(8, 4).unwrap();
    fill_rgba(&r, &tex, |x, y| input[(y * 8 + x) as usize]);
    let out = chain.run(&r, tex).unwrap();
    assert_eq!(read_rgba(&r, &out), flatten(&input), "without a service the picture passes through");
    assert!(chain.take_errors().is_empty());

    let mut rig = Rig::new("mask-none");
    rig.apply(json!({ "image_path": "" }));
    for _ in 0..3 {
        assert_eq!(rig.frame(8, 4, &input), input, "an empty path is no image");
    }
    std::thread::sleep(Duration::from_millis(200));
    assert!(rig.errors().is_empty(), "an empty path is not an error: {:?}", rig.errors());
}

#[test]
fn a_missing_or_broken_file_passes_through_and_reports_once() {
    let input = noise(8, 4, 33, 90);
    let mut rig = Rig::new("mask-bad");

    // a file that is not there
    let missing = rig.dir.join("missing.png").to_string_lossy().into_owned();
    rig.apply(json!({ "image_path": missing }));
    rig.wait_for_error("could not load missing.png");
    for _ in 0..5 {
        assert_eq!(rig.frame(8, 4, &input), input);
    }
    let errors = rig.errors();
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0].id, "mask");
    assert!(errors[0].message.contains("file not found"), "{}", errors[0].message);

    // a file that is not a picture
    let bad = rig.dir.join("bad.png");
    std::fs::write(&bad, b"this is not a picture at all, just some text in a file").unwrap();
    rig.apply(json!({ "image_path": bad.to_string_lossy() }));
    rig.wait_for_error("could not load bad.png");
    for _ in 0..5 {
        assert_eq!(rig.frame(8, 4, &input), input);
    }
    assert_eq!(rig.errors().len(), 2, "{:?}", rig.errors());

    // a picture that was cut short
    let good = rig.png("good.png", 4, 4, &noise(4, 4, 1, 255));
    let bytes = std::fs::read(&good).unwrap();
    let cut = rig.dir.join("cut.png");
    std::fs::write(&cut, &bytes[..bytes.len() / 2]).unwrap();
    rig.apply(json!({ "image_path": cut.to_string_lossy() }));
    rig.wait_for_error("could not load cut.png");
    assert_eq!(rig.frame(8, 4, &input), input);

    // a good file clears the message and the filter works
    let white = rig.png("white.png", 2, 2, &[WHITE; 4]);
    rig.apply(json!({ "image_path": white, "type": "blend_sub_filter.effect" }));
    let out = rig.until_changed(8, 4, &input);
    assert!(out.iter().all(|p| p[..3] == [0, 0, 0]), "{:?}", out[0]);
    assert!(rig.log.lock().unwrap().last().unwrap().message.is_empty(), "the card's error clears");

    // clearing the path restores the picture
    rig.apply(json!({ "image_path": "" }));
    assert_eq!(rig.frame(8, 4, &input), input);
}

#[test]
fn the_filter_is_registered_and_warms_up_all_five_shaders() {
    let r = warp();
    assert!(crate::fx::kinds().any(|k| k == KIND));
    crate::fx::make(KIND).unwrap().warm_up(&r).unwrap();
    for t in MaskType::ALL {
        // each shader is cached under its own name after the warm-up
        r.shaders.pixel_source(t.effect(), t.hlsl()).unwrap();
    }
    let defaults: Value = serde_json::from_str(crate::fx::DEFAULTS_JSON).unwrap();
    let mut have: Vec<&str> = defaults[KIND].as_object().unwrap().keys().map(String::as_str).collect();
    let mut want = KEYS.to_vec();
    have.sort_unstable();
    want.sort_unstable();
    assert_eq!(have, want);
}
