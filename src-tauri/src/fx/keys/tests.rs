//! Tests of Chroma Key, Color Key and Luma Key on WARP.
//!
//! The CPU references below are the OBS effects written as plain steps on one pixel, with the samples the effect takes
//! worked out as continuous positions (bilinear, in linear light, premultiplied), not as the shader's shortcuts.

use std::sync::Arc;

use serde_json::{json, Value};

use super::*;
use crate::fx::{Chain, FilterCfg};
use crate::gpu::pipeline_tests::{fill_rgba, read_rgba, warp};
use crate::gpu::Renderer;

type Pixel = [u8; 4];

fn run_filter(r: &Arc<Renderer>, kind: &str, params: Value, w: u32, h: u32, px: &[Pixel]) -> Vec<Pixel> {
    let mut chain = Chain::new();
    chain.apply(
        r,
        &[FilterCfg {
            id: "k".into(),
            kind: kind.into(),
            enabled: true,
            params: params.as_object().cloned().unwrap_or_default(),
        }],
    );
    assert_eq!(chain.len(), 1, "{kind} is registered");
    let tex = r.texture(w, h).unwrap();
    fill_rgba(r, &tex, |x, y| px[(y * w + x) as usize]);
    let out = chain.run(r, tex).unwrap();
    assert_eq!((out.width, out.height), (w, h));
    assert!(chain.take_errors().is_empty());
    read_rgba(r, &out)
        .chunks(4)
        .map(|p| [p[0], p[1], p[2], p[3]])
        .collect()
}

fn flat(w: u32, h: u32, c: Pixel) -> Vec<Pixel> {
    vec![c; (w * h) as usize]
}

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

/// Linear light to a code value (not rounded), clamped.
fn enc(lin: f64) -> f64 {
    let c = lin.clamp(0.0, 1.0);
    let u = if c <= 0.0031308 {
        12.92 * c
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    };
    u * 255.0
}

/// The OBS colour int 0xAABBGGRR of an RGB triple.
fn obs_color(c: [u8; 3]) -> u32 {
    c[0] as u32 | (c[1] as u32) << 8 | (c[2] as u32) << 16
}

/// The test picture: grays, primaries, secondaries, skin tone, then noise with every alpha from 32 up.
fn picture() -> (u32, u32, Vec<Pixel>) {
    let mut px: Vec<Pixel> = vec![
        [0, 0, 0, 255],
        [64, 64, 64, 255],
        [128, 128, 128, 255],
        [192, 192, 192, 255],
        [255, 255, 255, 255],
        [255, 0, 0, 255],
        [0, 255, 0, 255],
        [0, 0, 255, 255],
        [255, 255, 0, 255],
        [0, 255, 255, 255],
        [255, 0, 255, 255],
        [224, 172, 140, 255],
        [40, 90, 200, 255],
        [200, 30, 60, 255],
        [10, 20, 30, 255],
        [250, 240, 230, 255],
    ];
    let mut seed = 0x1234_5678u32;
    let mut next = move || {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (seed >> 24) as u8
    };
    while px.len() < 16 * 8 {
        px.push([next(), next(), next(), next() | 0x20]);
    }
    (16, 8, px)
}

/// An opaque picture of mixed greens, grays and skin tones, for the box filter.
fn opaque_picture() -> (u32, u32, Vec<Pixel>) {
    let (w, h, mut px) = picture();
    for p in &mut px {
        p[3] = 255;
    }
    (w, h, px)
}

struct Img<'a> {
    w: i64,
    h: i64,
    px: &'a [Pixel],
}

impl Img<'_> {
    /// A texel as a premultiplied linear colour (OBS's texture holds that), the position clamped to the picture.
    fn premultiplied(&self, x: i64, y: i64) -> [f64; 3] {
        let (x, y) = (x.clamp(0, self.w - 1), y.clamp(0, self.h - 1));
        let p = self.px[(y * self.w + x) as usize];
        let a = p[3] as f64 / 255.0;
        [dec(p[0] as f64) * a, dec(p[1] as f64) * a, dec(p[2] as f64) * a]
    }

    /// `image.Sample` with a linear clamped sampler at the position `(x, y)` in pixels, texel `i` centred at `i + 0.5`.
    fn sample(&self, x: f64, y: f64) -> [f64; 3] {
        let (tx, ty) = (x - 0.5, y - 0.5);
        let (x0, y0) = (tx.floor(), ty.floor());
        let (fx, fy) = (tx - x0, ty - y0);
        let mut out = [0.0; 3];
        for (dy, wy) in [(0, 1.0 - fy), (1, fy)] {
            for (dx, wx) in [(0, 1.0 - fx), (1, fx)] {
                let t = self.premultiplied(x0 as i64 + dx, y0 as i64 + dy);
                for (o, v) in out.iter_mut().zip(t) {
                    *o += v * wx * wy;
                }
            }
        }
        out
    }

    fn at(&self, x: i64, y: i64) -> Pixel {
        self.px[(y * self.w + x) as usize]
    }
}

fn to_encoded(lin: [f64; 3]) -> [f64; 3] {
    lin.map(|v| enc(v) / 255.0)
}

/// OBS's `CalcColor` and the clamp and encode of the output: `pow(rgb, gamma) * contrast + brightness`.
fn calc_color(lin: [f64; 3], contrast: f64, brightness: f64, gamma: f64) -> [f64; 3] {
    lin.map(|v| enc(v.powf(gamma) * contrast + brightness))
}

fn contrast_gain(setting: f64) -> f64 {
    if setting < 0.0 {
        1.0 / (1.0 - setting)
    } else {
        1.0 + setting
    }
}

fn gamma_exponent(setting: f64) -> f64 {
    if setting < 0.0 {
        1.0 - setting
    } else {
        1.0 / (1.0 + setting)
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Chroma Key

#[derive(Clone, Copy)]
struct ChromaRef {
    /// The key colour as nominal RGB codes (what `key_color_type` makes of the setting).
    key: [u8; 3],
    kind: &'static str,
    custom: [u8; 3],
    similarity: f64,
    smoothness: f64,
    spill: f64,
    opacity: f64,
    contrast: f64,
    brightness: f64,
    gamma: f64,
}

impl Default for ChromaRef {
    fn default() -> ChromaRef {
        ChromaRef {
            key: [0, 255, 0],
            kind: "green",
            custom: [0, 255, 0],
            similarity: 400.0,
            smoothness: 80.0,
            spill: 100.0,
            opacity: 1.0,
            contrast: 0.0,
            brightness: 0.0,
            gamma: 0.0,
        }
    }
}

impl ChromaRef {
    fn json(&self) -> Value {
        json!({
            "key_color_type": self.kind, "key_color": obs_color(self.custom),
            "similarity": self.similarity, "smoothness": self.smoothness, "spill": self.spill,
            "opacity": self.opacity, "contrast": self.contrast, "brightness": self.brightness, "gamma": self.gamma,
        })
    }

    /// The expected pixel (x, y) in code values.
    fn apply(&self, img: &Img, x: i64, y: i64) -> [f64; 4] {
        const CB: [f64; 3] = [-0.100644, -0.338572, 0.439216];
        const CR: [f64; 3] = [0.439216, -0.398942, -0.040274];
        let coords = |rgb: [f64; 3]| {
            (
                rgb[0] * CB[0] + rgb[1] * CB[1] + rgb[2] * CB[2] + 0.501961,
                rgb[0] * CR[0] + rgb[1] * CR[1] + rgb[2] * CR[2] + 0.501961,
            )
        };
        let key = self.key.map(|v| v as f64 / 255.0);
        let (key_cb, key_cr) = coords(key);
        let dist = |encoded: [f64; 3]| {
            let (cb, cr) = coords(encoded);
            ((cb - key_cb).powi(2) + (cr - key_cr).powi(2)).sqrt()
        };

        let p = img.at(x, y);
        let a = p[3] as f64 / 255.0;
        let lin: [f64; 3] = if a > 0.0 {
            [dec(p[0] as f64), dec(p[1] as f64), dec(p[2] as f64)]
        } else {
            [0.0; 3]
        };

        // Box filter: the four neighbours at (-1, -0.5), (+1, +0.5), (-0.5, +1) and (+0.5, -1) pixels from the centre,
        // counted twice, plus the centre.
        let (cx, cy) = (x as f64 + 0.5, y as f64 + 0.5);
        let mut total = 0.0;
        for (dx, dy) in [(-1.0, -0.5), (1.0, 0.5), (-0.5, 1.0), (0.5, -1.0)] {
            total += dist(to_encoded(img.sample(cx + dx, cy + dy)));
        }
        total = total * 2.0 + dist(to_encoded(lin));
        let chroma_dist = total / 9.0;

        let base = chroma_dist - self.similarity / 1000.0;
        let full = (base / (self.smoothness / 1000.0)).clamp(0.0, 1.0).powf(1.5);
        let spill = (base / (self.spill / 1000.0)).clamp(0.0, 1.0).powf(1.5);
        let alpha = a * self.opacity * full;

        let luma = 0.2126 * lin[0] + 0.7152 * lin[1] + 0.0722 * lin[2];
        let desat = lin.map(|v| luma + spill * (v - luma));
        let rgb = calc_color(
            desat,
            contrast_gain(self.contrast),
            self.brightness,
            gamma_exponent(self.gamma),
        );
        [rgb[0], rgb[1], rgb[2], alpha.clamp(0.0, 1.0) * 255.0]
    }
}

fn check_chroma(r: &Arc<Renderer>, name: &str, set: ChromaRef, w: u32, h: u32, px: &[Pixel]) {
    let out = run_filter(r, CHROMA_KIND, set.json(), w, h, px);
    let img = Img { w: w as i64, h: h as i64, px };
    let mut worst = 0.0f64;
    for y in 0..h as i64 {
        for x in 0..w as i64 {
            let got = out[(y * w as i64 + x) as usize];
            let want = set.apply(&img, x, y);
            worst = got.iter().zip(want).fold(worst, |w, (g, e)| w.max((*g as f64 - e).abs()));
            assert!(
                near(got, want, 2.0),
                "{name}: pixel ({x}, {y}) {:?} -> {got:?}, expected about {want:?}",
                img.at(x, y)
            );
        }
    }
    eprintln!("chroma {name}: worst difference {worst:.2} code values");
}

#[test]
fn chroma_pure_green_is_keyed_out_and_gray_stays() {
    let r = warp();
    let green = run_filter(&r, CHROMA_KIND, json!({}), 8, 6, &flat(8, 6, [0, 255, 0, 255]));
    assert!(green.iter().all(|p| p[3] < 13), "alpha below 0.05: {:?}", green[0]);
    let gray = run_filter(&r, CHROMA_KIND, json!({}), 8, 6, &flat(8, 6, [128, 128, 128, 255]));
    assert!(gray.iter().all(|p| p[3] > 242), "alpha above 0.95: {:?}", gray[0]);
    // the colour of a kept pixel is not touched at the defaults: gray 128, no spill reduction (far from the key)
    assert!(gray.iter().all(|p| near(*p, [128.0, 128.0, 128.0, 255.0], 1.0)), "{:?}", gray[0]);
}

#[test]
fn chroma_hand_checked_values() {
    // gray 128: Cb = Cr = 0.501961 (the rows of the matrix sum to zero), the green key is at (0.163389, 0.103019), so
    // the distance is 0.5234 and (0.5234 - 0.4) / 0.08 > 1: opaque.
    let key_cb = -0.338572 + 0.501961;
    let key_cr = -0.398942 + 0.501961;
    let dist = ((0.501961f64 - key_cb).powi(2) + (0.501961f64 - key_cr).powi(2)).sqrt();
    assert!((dist - 0.5234).abs() < 1e-3, "{dist}");

    // Defaults: similarity 400, smoothness 80, so a flat picture at distance d has alpha ((d - 0.4) / 0.08)^1.5.
    // Pick a pixel on the way from green to gray: at t = 0.9 the distance is 0.9 * 0.5234 = 0.4710 (the path is a
    // straight line in Cb/Cr), alpha = (0.0710 / 0.08)^1.5 = 0.836.
    let r = warp();
    let t = 0.9f64;
    let c = [(128.0 * t).round() as u8, (255.0 + (128.0 - 255.0) * t).round() as u8, (128.0 * t).round() as u8, 255];
    let img = Img { w: 1, h: 1, px: &[c] };
    let want = ChromaRef::default().apply(&img, 0, 0)[3];
    let got = run_filter(&r, CHROMA_KIND, json!({}), 1, 1, &[c])[0];
    assert!((got[3] as f64 - want).abs() <= 2.0, "{got:?} vs {want}");
    assert!((got[3] as f64 / 255.0 - 0.836).abs() < 0.05, "{got:?}");

    // opacity 0.5 on a kept pixel halves alpha and leaves the colour
    let got = run_filter(&r, CHROMA_KIND, json!({ "opacity": 0.5 }), 1, 1, &[[128, 128, 128, 255]])[0];
    assert!(near(got, [128.0, 128.0, 128.0, 127.5], 1.0), "{got:?}");
    // mid gray 128 is 0.2159 in linear light: contrast +1 doubles it (code 176), contrast -1 halves it (92),
    // brightness +0.1 adds to it (152), gamma +1 is the exponent 1/2 (181), gamma -1 the exponent 2 (61)
    for (params, want) in [
        (json!({ "contrast": 1 }), 176.0),
        (json!({ "contrast": -1 }), 92.0),
        (json!({ "brightness": 0.1 }), 152.0),
        (json!({ "gamma": 1 }), 181.0),
        (json!({ "gamma": -1 }), 61.0),
    ] {
        let got = run_filter(&r, CHROMA_KIND, params.clone(), 1, 1, &[[128, 128, 128, 255]])[0];
        assert!(near(got, [want, want, want, 255.0], 1.0), "{params}: {got:?}");
    }
}

#[test]
fn chroma_matches_the_reference_on_a_green_to_gray_ramp_and_is_monotonic() {
    let r = warp();
    // 64 x 3: every row runs from pure green to mid gray in code values
    let (w, h) = (64u32, 3u32);
    let px: Vec<Pixel> = (0..w * h)
        .map(|i| {
            let t = (i % w) as f64 / (w - 1) as f64;
            [(128.0 * t).round() as u8, (255.0 - 127.0 * t).round() as u8, (128.0 * t).round() as u8, 255]
        })
        .collect();
    let out = run_filter(&r, CHROMA_KIND, json!({}), w, h, &px);
    let row = &out[..w as usize];
    for pair in row.windows(2) {
        assert!(pair[1][3] >= pair[0][3], "alpha falls along the ramp: {pair:?}");
    }
    assert!(row[0][3] < 13 && row[w as usize - 1][3] > 242, "{:?} .. {:?}", row[0], row[w as usize - 1]);
    // the keyed pixels come out the same on every row (the ramp does not change vertically)
    assert_eq!(&out[..w as usize], &out[w as usize..2 * w as usize]);
    check_chroma(&r, "ramp", ChromaRef::default(), w, h, &px);
    // the reference at wider and tighter keys, on the same ramp
    check_chroma(&r, "ramp narrow", ChromaRef { similarity: 150.0, smoothness: 300.0, ..Default::default() }, w, h, &px);
    check_chroma(&r, "ramp wide", ChromaRef { similarity: 600.0, smoothness: 20.0, ..Default::default() }, w, h, &px);
}

#[test]
fn chroma_matches_the_reference_on_the_test_picture() {
    let r = warp();
    let d = ChromaRef::default();
    for (name, (w, h, px)) in [("mixed alpha", picture()), ("opaque", opaque_picture())] {
        check_chroma(&r, &format!("{name} defaults"), d, w, h, &px);
        check_chroma(&r, &format!("{name} loose"), ChromaRef { similarity: 700.0, smoothness: 250.0, spill: 300.0, ..d }, w, h, &px);
    }
}

#[test]
fn chroma_spill_reduction_follows_the_reference() {
    let r = warp();
    // A greenish gray: the distance from the key is 0.441, a little above the similarity, so alpha is partial and the
    // spill ramp (base / spill) is not at its end: colours move towards gray.
    let px = flat(4, 4, [110, 150, 110, 255]);
    let (small, large) = (
        ChromaRef { spill: 1.0, ..Default::default() },
        ChromaRef { spill: 1000.0, ..Default::default() },
    );
    check_chroma(&r, "spill 1", small, 4, 4, &px);
    check_chroma(&r, "spill 100", ChromaRef::default(), 4, 4, &px);
    check_chroma(&r, "spill 1000", large, 4, 4, &px);

    let img = Img { w: 4, h: 4, px: &px };
    let with_small = small.apply(&img, 1, 1);
    let with_large = large.apply(&img, 1, 1);
    assert!(with_small[3] > 13.0 && with_small[3] < 242.0, "partial alpha {with_small:?}");
    // spill 1/1000 leaves the colour: the pixel's green stays well above its red; spill 1000/1000 pulls it to gray
    assert!((with_small[1] - 150.0).abs() <= 1.5 && (with_small[0] - 110.0).abs() <= 1.5, "{with_small:?}");
    assert!(with_large[1] < with_small[1] - 5.0, "{with_large:?} vs {with_small:?}");
    assert!(with_large[0] > with_small[0] + 15.0, "{with_large:?} vs {with_small:?}");
    let got_small = run_filter(&r, CHROMA_KIND, small.json(), 4, 4, &px)[5];
    let got_large = run_filter(&r, CHROMA_KIND, large.json(), 4, 4, &px)[5];
    assert!(near(got_small, with_small, 2.0) && near(got_large, with_large, 2.0), "{got_small:?} {got_large:?}");
    assert_ne!(got_small[..3], got_large[..3], "the spill setting changes the colour");
}

#[test]
fn chroma_color_terms_follow_the_reference() {
    let r = warp();
    let d = ChromaRef::default();
    let (w, h, px) = opaque_picture();
    for (name, set) in [
        ("opacity 0.3", ChromaRef { opacity: 0.3, ..d }),
        ("contrast 1.5", ChromaRef { contrast: 1.5, ..d }),
        ("contrast -2", ChromaRef { contrast: -2.0, ..d }),
        ("brightness 0.15", ChromaRef { brightness: 0.15, ..d }),
        ("brightness -0.2", ChromaRef { brightness: -0.2, ..d }),
        ("gamma 0.6", ChromaRef { gamma: 0.6, ..d }),
        ("gamma -0.7", ChromaRef { gamma: -0.7, ..d }),
        (
            "combined",
            ChromaRef { opacity: 0.8, contrast: 0.5, brightness: 0.05, gamma: 0.3, spill: 250.0, ..d },
        ),
    ] {
        check_chroma(&r, name, set, w, h, &px);
    }
}

#[test]
fn chroma_key_color_types() {
    let r = warp();
    // The fixed colours of the filter: green 0x00FF00, blue 0xFF9900 (a screen blue: R 0, G 0x99, B 0xFF), magenta.
    let cases: [(&str, [u8; 3], [u8; 3]); 4] = [
        ("green", [0, 255, 0], [0, 255, 0]),
        ("blue", [0, 0x99, 0xFF], [0, 0x99, 0xFF]),
        ("magenta", [255, 0, 255], [255, 0, 255]),
        ("custom", [200, 120, 30], [200, 120, 30]),
    ];
    for (kind, key, pixel) in cases {
        let set = ChromaRef { kind, key, custom: if kind == "custom" { key } else { [1, 2, 3] }, ..Default::default() };
        let keyed = run_filter(&r, CHROMA_KIND, set.json(), 4, 4, &flat(4, 4, [pixel[0], pixel[1], pixel[2], 255]));
        assert!(keyed.iter().all(|p| p[3] < 13), "{kind}: a pixel of the key colour is keyed out: {:?}", keyed[0]);
        // the same pixel against another key stays
        let other = if kind == "green" { "magenta" } else { "green" };
        let kept = run_filter(
            &r,
            CHROMA_KIND,
            ChromaRef { kind: other, key: [0, 0, 0], ..Default::default() }.json(),
            4,
            4,
            &flat(4, 4, [pixel[0], pixel[1], pixel[2], 255]),
        );
        assert!(kept.iter().all(|p| p[3] > 242), "{kind} vs {other}: {:?}", kept[0]);
    }
    // key_color is read only for the custom type: type green with a blue key_color still keys green
    let set = ChromaRef { kind: "green", custom: [0, 0, 255], ..Default::default() };
    let got = run_filter(&r, CHROMA_KIND, set.json(), 2, 2, &flat(2, 2, [0, 255, 0, 255]));
    assert!(got.iter().all(|p| p[3] < 13), "{:?}", got[0]);
    // the matrix's constants: the key's (Cb, Cr) for green and the offset for a gray key
    let c = chroma(json!({}).as_object().unwrap());
    assert!((c.chroma[0] - 0.163389).abs() < 1e-5 && (c.chroma[1] - 0.103019).abs() < 1e-5, "{c:?}");
    let p = json!({ "key_color_type": "custom", "key_color": 0 });
    let c = chroma(p.as_object().unwrap());
    assert!((c.chroma[0] - 0.501961).abs() < 1e-6 && (c.chroma[1] - 0.501961).abs() < 1e-6, "black key: {c:?}");
    assert_eq!((c.similarity, c.smoothness, c.spill), (0.4, 0.08, 0.1));
}

#[test]
fn chroma_the_defaults_and_garbage_params_are_safe() {
    let r = warp();
    let (w, h, px) = picture();
    let a = run_filter(&r, CHROMA_KIND, json!({}), w, h, &px);
    let b = run_filter(&r, CHROMA_KIND, ChromaRef::default().json(), w, h, &px);
    assert_eq!(a, b, "the explicit defaults are the defaults");
    // wrong types fall back, numbers clamp: similarity 1e9 is 1000, smoothness -5 is 1
    let garbage = json!({ "similarity": 1e9, "smoothness": -5, "spill": "x", "opacity": null, "contrast": [1], "key_color_type": 7 });
    let clamped = json!({ "similarity": 1000, "smoothness": 1 });
    assert_eq!(
        run_filter(&r, CHROMA_KIND, garbage, w, h, &px),
        run_filter(&r, CHROMA_KIND, clamped, w, h, &px)
    );
    assert_eq!(chroma_pack(&Params::new()).len(), 48);
}

// ---------------------------------------------------------------------------------------------------------------
// Color Key

#[derive(Clone, Copy)]
struct ColorRef {
    key: [u8; 3],
    kind: &'static str,
    custom: [u8; 3],
    similarity: f64,
    smoothness: f64,
    opacity: f64,
    contrast: f64,
    brightness: f64,
    gamma: f64,
}

impl Default for ColorRef {
    fn default() -> ColorRef {
        ColorRef {
            key: [0, 255, 0],
            kind: "green",
            custom: [0, 255, 0],
            similarity: 80.0,
            smoothness: 50.0,
            opacity: 1.0,
            contrast: 0.0,
            brightness: 0.0,
            gamma: 0.0,
        }
    }
}

impl ColorRef {
    fn json(&self) -> Value {
        json!({
            "key_color_type": self.kind, "key_color": obs_color(self.custom),
            "similarity": self.similarity, "smoothness": self.smoothness,
            "opacity": self.opacity, "contrast": self.contrast, "brightness": self.brightness, "gamma": self.gamma,
        })
    }

    fn apply(&self, p: Pixel) -> [f64; 4] {
        let a = p[3] as f64 / 255.0;
        let lin: [f64; 3] = if a > 0.0 {
            [dec(p[0] as f64), dec(p[1] as f64), dec(p[2] as f64)]
        } else {
            [0.0; 3]
        };
        // the distance is measured on the encoded colour
        let encoded = to_encoded(lin);
        let key = self.key.map(|v| v as f64 / 255.0);
        let dist = (0..3).map(|i| (key[i] - encoded[i]).powi(2)).sum::<f64>().sqrt();
        let mask = ((dist - self.similarity / 1000.0).max(0.0) / (self.smoothness / 1000.0)).clamp(0.0, 1.0);
        let alpha = a * self.opacity * mask;
        let rgb = calc_color(lin, contrast_gain(self.contrast), self.brightness, gamma_exponent(self.gamma));
        [rgb[0], rgb[1], rgb[2], alpha.clamp(0.0, 1.0) * 255.0]
    }
}

fn check_color(r: &Arc<Renderer>, name: &str, set: ColorRef, w: u32, h: u32, px: &[Pixel]) {
    let out = run_filter(r, COLOR_KIND, set.json(), w, h, px);
    for (i, (got, src)) in out.iter().zip(px).enumerate() {
        let want = set.apply(*src);
        assert!(near(*got, want, 2.0), "{name}: pixel {i} {src:?} -> {got:?}, expected about {want:?}");
    }
}

#[test]
fn color_key_red_on_red_is_keyed_and_blue_stays() {
    let r = warp();
    let red = ColorRef { kind: "red", key: [255, 0, 0], ..Default::default() };
    let keyed = run_filter(&r, COLOR_KIND, red.json(), 4, 4, &flat(4, 4, [255, 0, 0, 255]));
    assert!(keyed.iter().all(|p| p[3] < 13), "{:?}", keyed[0]);
    let kept = run_filter(&r, COLOR_KIND, red.json(), 4, 4, &flat(4, 4, [0, 0, 255, 255]));
    assert!(kept.iter().all(|p| p[3] > 242), "{:?}", kept[0]);
    // the colour of a kept pixel is untouched at the defaults
    assert!(kept.iter().all(|p| near(*p, [0.0, 0.0, 255.0, 255.0], 1.0)), "{:?}", kept[0]);
}

#[test]
fn color_key_similarity_and_smoothness_hand_checked() {
    let r = warp();
    // Key red, similarity 80, smoothness 50: a pixel (255, g, 0) is g / 255 away from the key (encoded distance), so
    // alpha = ((g / 255) - 0.08) / 0.05. g = 26: 0.1020 - 0.08 = 0.0220, / 0.05 = 0.439, alpha code 112.
    let set = ColorRef { kind: "red", key: [255, 0, 0], ..Default::default() };
    let got = run_filter(&r, COLOR_KIND, set.json(), 1, 1, &[[255, 26, 0, 255]])[0];
    assert!(near(got, [255.0, 26.0, 0.0, 112.0], 1.5), "{got:?}");
    // inside the similarity: g = 10 is 0.039 away: transparent; outside similarity + smoothness: g = 40, 0.157: opaque
    let got = run_filter(&r, COLOR_KIND, set.json(), 1, 1, &[[255, 10, 0, 255]])[0];
    assert_eq!(got[3], 0, "{got:?}");
    let got = run_filter(&r, COLOR_KIND, set.json(), 1, 1, &[[255, 40, 0, 255]])[0];
    assert_eq!(got[3], 255, "{got:?}");
    // a wider similarity moves the whole ramp: similarity 200 makes g = 40 transparent
    let wide = ColorRef { similarity: 200.0, ..set };
    let got = run_filter(&r, COLOR_KIND, wide.json(), 1, 1, &[[255, 40, 0, 255]])[0];
    assert_eq!(got[3], 0, "{got:?}");
}

#[test]
fn color_key_matches_the_reference() {
    let r = warp();
    let d = ColorRef::default();
    for (name, (w, h, px)) in [("mixed alpha", picture()), ("opaque", opaque_picture())] {
        for (what, set) in [
            ("green", d),
            ("red", ColorRef { kind: "red", key: [255, 0, 0], ..d }),
            ("blue", ColorRef { kind: "blue", key: [0, 0, 255], ..d }),
            ("magenta", ColorRef { kind: "magenta", key: [255, 0, 255], ..d }),
            ("custom", ColorRef { kind: "custom", key: [224, 172, 140], custom: [224, 172, 140], similarity: 150.0, smoothness: 300.0, ..d }),
            ("loose", ColorRef { kind: "red", key: [255, 0, 0], similarity: 600.0, smoothness: 400.0, ..d }),
            ("opacity", ColorRef { opacity: 0.4, ..d }),
            ("contrast", ColorRef { contrast: 1.2, ..d }),
            ("brightness", ColorRef { brightness: -0.1, ..d }),
            ("gamma", ColorRef { gamma: -0.5, ..d }),
            (
                "combined",
                ColorRef { kind: "magenta", key: [255, 0, 255], opacity: 0.7, contrast: -1.0, brightness: 0.1, gamma: 0.4, ..d },
            ),
        ] {
            check_color(&r, &format!("{name} {what}"), set, w, h, &px);
        }
    }
    // the colour terms alone, hand-checked on mid gray (0.2159 in linear light), a key that does not match it
    let far = ColorRef { kind: "red", key: [255, 0, 0], ..d };
    for (set, want) in [
        (ColorRef { contrast: 1.0, ..far }, 176.0),
        (ColorRef { brightness: 0.1, ..far }, 152.0),
        (ColorRef { gamma: 1.0, ..far }, 181.0),
    ] {
        let got = run_filter(&r, COLOR_KIND, set.json(), 1, 1, &[[128, 128, 128, 255]])[0];
        assert!(near(got, [want, want, want, 255.0], 1.0), "{got:?}");
    }
    let got = run_filter(&r, COLOR_KIND, ColorRef { opacity: 0.5, ..far }.json(), 1, 1, &[[128, 128, 128, 255]])[0];
    assert!(near(got, [128.0, 128.0, 128.0, 127.5], 1.0), "{got:?}");
}

#[test]
fn color_key_constants() {
    let c = color_key(json!({}).as_object().unwrap());
    assert_eq!((c.key_color, c.similarity, c.smoothness), ([0.0, 1.0, 0.0], 0.08, 0.05));
    // blue is pure blue here (0xFF0000 in 0xBBGGRR), unlike the chroma key's screen blue
    let p = json!({ "key_color_type": "blue" });
    assert_eq!(color_key(p.as_object().unwrap()).key_color, [0.0, 0.0, 1.0]);
    let p = json!({ "key_color_type": "red" });
    assert_eq!(color_key(p.as_object().unwrap()).key_color, [1.0, 0.0, 0.0]);
    let p = json!({ "key_color_type": "custom", "key_color": 0x0080_4020u32 });
    let k = color_key(p.as_object().unwrap()).key_color;
    assert!((k[0] - 32.0 / 255.0).abs() < 1e-6 && (k[1] - 64.0 / 255.0).abs() < 1e-6 && (k[2] - 128.0 / 255.0).abs() < 1e-6);
    assert_eq!(color_pack(&Params::new()).len(), 48);
}

// ---------------------------------------------------------------------------------------------------------------
// Luma Key

#[derive(Clone, Copy)]
struct LumaRef {
    max: f64,
    max_smooth: f64,
    min: f64,
    min_smooth: f64,
}

impl Default for LumaRef {
    fn default() -> LumaRef {
        LumaRef { max: 1.0, max_smooth: 0.0, min: 0.0, min_smooth: 0.0 }
    }
}

/// HLSL `smoothstep` with the values it takes when the edges are equal: the quotient is infinite (or 0 / 0, which the
/// saturate of Direct3D makes 0), so the step is 0 up to and at the edge and 1 above it.
fn smoothstep(lo: f64, hi: f64, x: f64) -> f64 {
    let t = if hi > lo {
        ((x - lo) / (hi - lo)).clamp(0.0, 1.0)
    } else if x > lo {
        1.0
    } else {
        0.0
    };
    t * t * (3.0 - 2.0 * t)
}

impl LumaRef {
    fn json(&self) -> Value {
        json!({ "luma_max": self.max, "luma_max_smooth": self.max_smooth, "luma_min": self.min, "luma_min_smooth": self.min_smooth })
    }

    fn apply(&self, p: Pixel) -> [f64; 4] {
        let a = p[3] as f64 / 255.0;
        let lin: [f64; 3] = if a > 0.0 {
            [dec(p[0] as f64), dec(p[1] as f64), dec(p[2] as f64)]
        } else {
            [0.0; 3]
        };
        let luminance = 0.2126 * lin[0] + 0.7152 * lin[1] + 0.0722 * lin[2];
        let clo = smoothstep(self.min, self.min + self.min_smooth, luminance);
        let chi = 1.0 - smoothstep(self.max - self.max_smooth, self.max, luminance);
        [p[0] as f64, p[1] as f64, p[2] as f64, a * clo * chi * 255.0]
    }
}

fn check_luma(r: &Arc<Renderer>, name: &str, set: LumaRef, w: u32, h: u32, px: &[Pixel]) {
    let out = run_filter(r, LUMA_KIND, set.json(), w, h, px);
    for (i, (got, src)) in out.iter().zip(px).enumerate() {
        let want = set.apply(*src);
        assert!(near(*got, want, 2.0), "{name}: pixel {i} {src:?} -> {got:?}, expected about {want:?}");
    }
}

#[test]
fn luma_min_and_max_sides() {
    let r = warp();
    let black = [0, 0, 0, 255];
    let white = [255, 255, 255, 255];
    let gray = [128, 128, 128, 255]; // luminance 0.2159 in linear light

    // luma_min 0.5, no smoothing: black is below it, white above
    let min = json!({ "luma_min": 0.5, "luma_min_smooth": 0 });
    let got = run_filter(&r, LUMA_KIND, min.clone(), 1, 1, &[black])[0];
    assert_eq!(got, [0, 0, 0, 0], "black below luma_min");
    let got = run_filter(&r, LUMA_KIND, min.clone(), 1, 1, &[white])[0];
    assert_eq!(got, white, "white above luma_min");
    // mid gray 128 is 0.2159 < 0.5; gray 200 is 0.578 > 0.5
    assert_eq!(run_filter(&r, LUMA_KIND, min.clone(), 1, 1, &[gray])[0][3], 0);
    assert_eq!(run_filter(&r, LUMA_KIND, min, 1, 1, &[[200, 200, 200, 255]])[0][3], 255);

    // luma_max 0.5: white is above it (transparent), a dark gray below (opaque)
    let max = json!({ "luma_max": 0.5, "luma_max_smooth": 0 });
    let got = run_filter(&r, LUMA_KIND, max.clone(), 1, 1, &[white])[0];
    assert_eq!(got[3], 0, "white above luma_max");
    let got = run_filter(&r, LUMA_KIND, max.clone(), 1, 1, &[[64, 64, 64, 255]])[0];
    assert_eq!(got, [64, 64, 64, 255], "dark gray below luma_max");
    assert_eq!(run_filter(&r, LUMA_KIND, max.clone(), 1, 1, &[[200, 200, 200, 255]])[0][3], 0);
    // exactly black with the default luma_min 0 sits on the lower edge of an unsmoothed key: OBS's smoothstep is 0/0
    // there, which Direct3D turns into 0, so a pure black pixel is transparent even at the defaults
    assert_eq!(run_filter(&r, LUMA_KIND, max, 1, 1, &[black])[0][3], 0);
    assert_eq!(run_filter(&r, LUMA_KIND, json!({}), 1, 1, &[black])[0][3], 0);
    assert_eq!(run_filter(&r, LUMA_KIND, json!({}), 1, 1, &[[1, 1, 1, 255]])[0], [1, 1, 1, 255]);
    assert_eq!(run_filter(&r, LUMA_KIND, json!({}), 1, 1, &[white])[0], white);
}

#[test]
fn luma_key_matches_the_reference_with_smoothing() {
    let r = warp();
    // a gray ramp, every second code value
    let (w, h) = (128u32, 2u32);
    let ramp: Vec<Pixel> = (0..w * h).map(|i| { let v = ((i % w) * 2) as u8; [v, v, v, 255] }).collect();
    let d = LumaRef::default();
    for (name, set) in [
        ("defaults", d),
        ("min 0.3 smooth 0.2", LumaRef { min: 0.3, min_smooth: 0.2, ..d }),
        ("max 0.7 smooth 0.3", LumaRef { max: 0.7, max_smooth: 0.3, ..d }),
        ("both", LumaRef { min: 0.1, min_smooth: 0.15, max: 0.8, max_smooth: 0.25 }),
        ("narrow band", LumaRef { min: 0.4, min_smooth: 0.0, max: 0.6, max_smooth: 0.0 }),
    ] {
        check_luma(&r, name, set, w, h, &ramp);
    }
    // colour pictures: the luminance is Rec. 709 weighted, in linear light; translucent pixels keep alpha * mask
    for (w, h, px) in [picture(), opaque_picture()] {
        check_luma(&r, "picture", LumaRef { min: 0.2, min_smooth: 0.3, max: 0.9, max_smooth: 0.2 }, w, h, &px);
        check_luma(&r, "picture min", LumaRef { min: 0.5, ..d }, w, h, &px);
    }
    // the smoothstep is the cubic: halfway through a smoothed edge the mask is exactly one half
    let half = run_filter(&r, LUMA_KIND, json!({ "luma_min": 0.2, "luma_min_smooth": 0.6 }), 1, 1, &[[255, 255, 255, 255]])[0];
    assert_eq!(half[3], 255, "luminance 1 is past the edge");
    // luminance 0.5 (a code of about 188) is the middle of a 0.2 .. 0.8 edge
    let code = enc(0.5).round() as u8;
    let mid = run_filter(&r, LUMA_KIND, json!({ "luma_min": 0.2, "luma_min_smooth": 0.6 }), 1, 1, &[[code, code, code, 255]])[0];
    assert!((mid[3] as i32 - 128).abs() <= 3, "{mid:?}");
    assert_eq!(luma_pack(&Params::new()).len(), 16);
}

#[test]
fn luma_key_clamps_garbage() {
    let r = warp();
    let px = [[40, 40, 40, 255], [220, 220, 220, 255]];
    let a = run_filter(&r, LUMA_KIND, json!({ "luma_min": -3, "luma_max": 9, "luma_max_smooth": "x" }), 2, 1, &px);
    let b = run_filter(&r, LUMA_KIND, json!({ "luma_min": 0, "luma_max": 1 }), 2, 1, &px);
    assert_eq!(a, b);
}

#[test]
fn the_three_kinds_are_registered_and_warm_up() {
    let r = warp();
    for kind in [CHROMA_KIND, COLOR_KIND, LUMA_KIND] {
        assert!(crate::fx::kinds().any(|k| k == kind), "{kind}");
        crate::fx::make(kind).unwrap().warm_up(&r).unwrap_or_else(|e| panic!("{kind}: {e}"));
    }
    // every setting the port reads is in the schema's defaults
    let defaults: Value = serde_json::from_str(crate::fx::DEFAULTS_JSON).unwrap();
    for (kind, keys) in [(CHROMA_KIND, CHROMA_KEYS), (COLOR_KIND, COLOR_KEYS), (LUMA_KIND, LUMA_KEYS)] {
        let mut have: Vec<&str> = defaults[kind].as_object().unwrap().keys().map(String::as_str).collect();
        let mut want = keys.to_vec();
        have.sort_unstable();
        want.sort_unstable();
        assert_eq!(have, want, "{kind}");
    }
}
