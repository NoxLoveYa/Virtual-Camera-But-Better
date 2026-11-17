//! Tests of Crop/Pad, Scaling/Aspect Ratio and Transform on WARP.
//!
//! Crop and Transform are checked against pixel positions worked out by hand. The scaling modes are checked against a
//! CPU reference per kernel, written as plain steps on the pixels (decode to linear light, premultiply, weight the
//! taps, un-premultiply, encode) and compared within 2 code values.

use std::sync::Arc;

use serde_json::{json, Value};

use super::scale::{self, Method, Sampling, Target};
use super::{clamp_size, crop, transform, MAX_PIXELS, MAX_SIDE};
use crate::fx::{Chain, FilterCfg};
use crate::gpu::output::{FitMode, OutputParams, OutputStage};
use crate::gpu::pipeline_tests::{fill_rgba, read_rgba, warp};
use crate::gpu::Renderer;

type Px = [u8; 4];

// ---------------------------------------------------------------------------------------------------------------
// Helpers

fn cfg(id: &str, kind: &str, params: Value) -> FilterCfg {
    FilterCfg {
        id: id.into(),
        kind: kind.into(),
        enabled: true,
        params: params.as_object().cloned().unwrap_or_default(),
    }
}

#[derive(Clone)]
struct Img {
    w: usize,
    h: usize,
    px: Vec<Px>,
}

impl Img {
    fn new(w: usize, h: usize, f: impl Fn(usize, usize) -> Px) -> Img {
        let mut px = Vec::with_capacity(w * h);
        for y in 0..h {
            for x in 0..w {
                px.push(f(x, y));
            }
        }
        Img { w, h, px }
    }

    fn at(&self, x: usize, y: usize) -> Px {
        self.px[y * self.w + x]
    }
}

/// Run `img` through a chain of `filters` (with the canvas size when given) and read the result back.
fn run_chain(
    r: &Arc<Renderer>,
    filters: &[FilterCfg],
    canvas: Option<(u32, u32)>,
    img: &Img,
) -> Img {
    let mut chain = Chain::new();
    chain.apply(r, filters);
    assert_eq!(
        chain.len(),
        filters.len(),
        "every filter kind is registered"
    );
    if let Some(c) = canvas {
        chain.set_canvas(c);
    }
    let tex = r.texture(img.w as u32, img.h as u32).unwrap();
    fill_rgba(r, &tex, |x, y| img.at(x as usize, y as usize));
    let out = chain.run(r, tex).unwrap();
    let px = read_rgba(r, &out)
        .chunks(4)
        .map(|p| [p[0], p[1], p[2], p[3]])
        .collect();
    Img {
        w: out.width as usize,
        h: out.height as usize,
        px,
    }
}

fn run_one(r: &Arc<Renderer>, kind: &str, params: Value, img: &Img) -> Img {
    run_chain(r, &[cfg("f", kind, params)], None, img)
}

/// A picture with a smooth part (the red and green ramps) and hard edges (the blue checker), every pixel different
/// enough to notice a move by one.
fn pattern(w: usize, h: usize) -> Img {
    Img::new(w, h, |x, y| {
        [
            (x * 255 / (w - 1).max(1)) as u8,
            (y * 255 / (h - 1).max(1)) as u8,
            if (x / 4 + y / 4) % 2 == 0 { 220 } else { 30 },
            255,
        ]
    })
}

/// The same with alpha between 128 and 255.
fn pattern_translucent(w: usize, h: usize) -> Img {
    let mut img = pattern(w, h);
    for y in 0..h {
        for x in 0..w {
            img.px[y * w + x][3] = 128 + ((x * 5 + y * 3) % 128) as u8;
        }
    }
    img
}

fn same(a: &Img, b: &Img) {
    assert_eq!((a.w, a.h), (b.w, b.h));
    assert_eq!(a.px, b.px);
}

// ---------------------------------------------------------------------------------------------------------------
// Crop/Pad

#[test]
fn geometry_crop_relative_trims_every_side() {
    let r = warp();
    let img = pattern(100, 100);
    let out = run_one(
        &r,
        "crop_filter",
        json!({ "relative": true, "left": 10, "top": 10, "right": 10, "bottom": 10 }),
        &img,
    );
    assert_eq!((out.w, out.h), (80, 80));
    for y in 0..80 {
        for x in 0..80 {
            assert_eq!(out.at(x, y), img.at(x + 10, y + 10), "({x}, {y})");
        }
    }
    // each side on its own
    let out = run_one(&r, "crop_filter", json!({ "left": 30 }), &img);
    assert_eq!((out.w, out.h), (70, 100));
    assert_eq!(out.at(0, 5), img.at(30, 5));
    let out = run_one(&r, "crop_filter", json!({ "right": 30 }), &img);
    assert_eq!((out.w, out.h), (70, 100));
    assert_eq!(out.at(69, 5), img.at(69, 5));
    let out = run_one(&r, "crop_filter", json!({ "top": 25 }), &img);
    assert_eq!((out.w, out.h), (100, 75));
    assert_eq!(out.at(5, 0), img.at(5, 25));
    let out = run_one(&r, "crop_filter", json!({ "bottom": 25 }), &img);
    assert_eq!((out.w, out.h), (100, 75));
    assert_eq!(out.at(5, 74), img.at(5, 74));
}

#[test]
fn geometry_crop_negative_values_pad_with_transparent_pixels() {
    let r = warp();
    let img = pattern(100, 100);
    let out = run_one(
        &r,
        "crop_filter",
        json!({ "left": -10, "top": -10, "right": -10, "bottom": -10 }),
        &img,
    );
    assert_eq!((out.w, out.h), (120, 120));
    for y in 0..120 {
        for x in 0..120 {
            let inside = (10..110).contains(&x) && (10..110).contains(&y);
            if inside {
                assert_eq!(out.at(x, y), img.at(x - 10, y - 10), "({x}, {y})");
            } else {
                assert_eq!(out.at(x, y), [0, 0, 0, 0], "border ({x}, {y})");
            }
        }
    }
    // padding on one side only; the picture moves the other way
    let out = run_one(&r, "crop_filter", json!({ "left": -7 }), &img);
    assert_eq!((out.w, out.h), (107, 100));
    assert_eq!(out.at(6, 50), [0, 0, 0, 0]);
    assert_eq!(out.at(7, 50), img.at(0, 50));
    // a crop on one side and padding on the opposite one
    let out = run_one(&r, "crop_filter", json!({ "left": 10, "right": -5 }), &img);
    assert_eq!((out.w, out.h), (95, 100));
    assert_eq!(out.at(0, 3), img.at(10, 3));
    assert_eq!(out.at(89, 3), img.at(99, 3));
    assert_eq!(out.at(90, 3), [0, 0, 0, 0]);
}

#[test]
fn geometry_crop_absolute_window() {
    let r = warp();
    let img = pattern(100, 80);
    let out = run_one(
        &r,
        "crop_filter",
        json!({ "relative": false, "left": 5, "top": 5, "cx": 50, "cy": 40 }),
        &img,
    );
    assert_eq!((out.w, out.h), (50, 40));
    for y in 0..40 {
        for x in 0..50 {
            assert_eq!(out.at(x, y), img.at(x + 5, y + 5), "({x}, {y})");
        }
    }
    // a width or height of 0 runs to the edge
    let out = run_one(
        &r,
        "crop_filter",
        json!({ "relative": false, "left": 20, "top": 10, "cx": 0, "cy": 0 }),
        &img,
    );
    assert_eq!((out.w, out.h), (80, 70));
    assert_eq!(out.at(0, 0), img.at(20, 10));
    assert_eq!(out.at(79, 69), img.at(99, 79));
    let out = run_one(
        &r,
        "crop_filter",
        json!({ "relative": false, "cx": 30, "cy": 0 }),
        &img,
    );
    assert_eq!((out.w, out.h), (30, 80));
    let out = run_one(
        &r,
        "crop_filter",
        json!({ "relative": false, "cx": 0, "cy": 30 }),
        &img,
    );
    assert_eq!((out.w, out.h), (100, 30));
    // all zero shows the whole picture, as in relative mode
    let out = run_one(&r, "crop_filter", json!({ "relative": false }), &img);
    same(&out, &img);
    let out = run_one(&r, "crop_filter", json!({}), &img);
    same(&out, &img);
    // a window larger than the picture is padded on the right and the bottom
    let out = run_one(
        &r,
        "crop_filter",
        json!({ "relative": false, "cx": 120, "cy": 90 }),
        &img,
    );
    assert_eq!((out.w, out.h), (120, 90));
    assert_eq!(out.at(99, 79), img.at(99, 79));
    assert_eq!(out.at(100, 0), [0, 0, 0, 0]);
    assert_eq!(out.at(0, 80), [0, 0, 0, 0]);
    // the offset can be negative too
    let out = run_one(
        &r,
        "crop_filter",
        json!({ "relative": false, "left": -4, "top": -2, "cx": 10, "cy": 6 }),
        &img,
    );
    assert_eq!((out.w, out.h), (10, 6));
    assert_eq!(out.at(3, 1), [0, 0, 0, 0]);
    assert_eq!(out.at(4, 2), img.at(0, 0));
}

#[test]
fn geometry_crop_larger_than_the_picture_does_not_panic() {
    let r = warp();
    let img = pattern(100, 100);
    // takes more than there is: one pixel, as OBS does
    let out = run_one(&r, "crop_filter", json!({ "left": 60, "right": 60 }), &img);
    assert_eq!((out.w, out.h), (1, 100));
    let out = run_one(
        &r,
        "crop_filter",
        json!({ "top": 8192, "bottom": 8192 }),
        &img,
    );
    assert_eq!((out.w, out.h), (100, 1));
    let out = run_one(
        &r,
        "crop_filter",
        json!({ "left": 500, "top": 500, "right": 500, "bottom": 500 }),
        &img,
    );
    assert_eq!((out.w, out.h), (1, 1));
    // the window starts beyond the picture
    let out = run_one(
        &r,
        "crop_filter",
        json!({ "relative": false, "left": 300, "top": 300, "cx": 8, "cy": 8 }),
        &img,
    );
    assert_eq!((out.w, out.h), (8, 8));
    assert!(out.px.iter().all(|p| *p == [0, 0, 0, 0]));
    // out of range and wrong-typed settings are clamped or ignored
    let out = run_one(
        &r,
        "crop_filter",
        json!({ "left": -1e12, "top": "x", "right": null, "cx": 99999 }),
        &img,
    );
    assert_eq!(
        (out.w, out.h),
        (8192, 100),
        "100 + 8192 pixels wide is cut at the limit"
    );
}

#[test]
fn geometry_sizes_stay_inside_the_limits() {
    // the settings alone can ask for 16000 pixels on a side: the filters stop at the limit
    let p = json!({ "left": -8192, "top": -8192, "right": -8192, "bottom": -8192 });
    let s = crop::Settings::read(p.as_object().unwrap());
    for input in [(1u32, 1u32), (1920, 1080), (8192, 8192), (16384, 16384)] {
        let (w, h) = s.size(input);
        assert!(
            (1..=MAX_SIDE).contains(&w) && (1..=MAX_SIDE).contains(&h),
            "{input:?} -> {w}x{h}"
        );
        assert!(w as u64 * h as u64 <= MAX_PIXELS);
    }
    assert_eq!(clamp_size(-5, 0), (1, 1));
    assert_eq!(clamp_size(i64::MAX, i64::MIN), (MAX_SIDE, 1));
    assert_eq!(clamp_size(8192, 8192), (8192, (MAX_PIXELS / 8192) as u32));
    assert_eq!(clamp_size(1920, 1080), (1920, 1080));
    // scale: a typed size is clamped
    let big = scale::plan(
        Target::parse("99999x99999"),
        Sampling::Bicubic,
        false,
        None,
        (640, 480),
    );
    assert!(big.size.0 <= MAX_SIDE && big.size.0 as u64 * big.size.1 as u64 <= MAX_PIXELS);
    // transform: a rotation of a huge picture
    let t = transform::Settings {
        flip_h: false,
        flip_v: false,
        rotation: 45.0,
    };
    let (w, h) = t.size((8192, 4608));
    assert!(w <= MAX_SIDE && w as u64 * h as u64 <= MAX_PIXELS);
}

#[test]
fn geometry_crop_pads_show_the_background_through_the_output_stage() {
    let r = warp();
    let img = Img::new(20, 20, |_, _| [200, 100, 50, 255]);
    let mut chain = Chain::new();
    chain.apply(
        &r,
        &[cfg(
            "c",
            "crop_filter",
            json!({ "left": -10, "top": -10, "right": -10, "bottom": -10 }),
        )],
    );
    let tex = r.texture(20, 20).unwrap();
    fill_rgba(&r, &tex, |x, y| img.at(x as usize, y as usize));
    let padded = chain.run(&r, tex).unwrap();
    assert_eq!((padded.width, padded.height), (40, 40));
    let stage = OutputStage::new(r.clone()).unwrap();
    let out = stage
        .run(
            &padded,
            &OutputParams {
                width: 40,
                height: 40,
                fit: FitMode::Fit,
                background: [0.0, 1.0, 0.0],
            },
        )
        .unwrap();
    let got = read_rgba(&r, &out);
    let at = |x: usize, y: usize| {
        [
            got[(y * 40 + x) * 4],
            got[(y * 40 + x) * 4 + 1],
            got[(y * 40 + x) * 4 + 2],
            got[(y * 40 + x) * 4 + 3],
        ]
    };
    assert_eq!(
        at(0, 0),
        [0, 255, 0, 255],
        "the border is the background colour"
    );
    assert_eq!(at(39, 20), [0, 255, 0, 255]);
    assert_eq!(at(20, 20), [200, 100, 50, 255], "the picture is intact");
}

// ---------------------------------------------------------------------------------------------------------------
// Scaling/Aspect Ratio: the settings and the plan

#[test]
fn geometry_scale_resolution_text_is_read_like_obs() {
    assert_eq!(Target::parse("None"), Target::None);
    assert_eq!(Target::parse(""), Target::None);
    assert_eq!(Target::parse("garbage"), Target::None);
    assert_eq!(Target::parse("Base (Canvas) Resolution"), Target::Canvas);
    assert_eq!(Target::parse("1280x720"), Target::Size(1280, 720));
    assert_eq!(
        Target::parse(" 640x 480"),
        Target::Size(640, 480),
        "%d skips white space"
    );
    assert_eq!(
        Target::parse("1280x720 tail"),
        Target::Size(1280, 720),
        "sscanf ignores what follows"
    );
    assert_eq!(
        Target::parse("1280 x720"),
        Target::None,
        "the literal x has to follow the number"
    );
    assert_eq!(
        Target::parse("1280X720"),
        Target::None,
        "x is case sensitive"
    );
    assert_eq!(Target::parse("4:3"), Target::Aspect(4, 3));
    assert_eq!(Target::parse("16:10"), Target::Aspect(16, 10));
    assert_eq!(Target::parse("1:"), Target::None);
    assert_eq!(Target::parse("x720"), Target::None);
    assert_eq!(Target::parse("-5x10"), Target::Size(-5, 10));
    assert_eq!(Target::parse("99999999999x5"), Target::Size(i32::MAX, 5));

    assert_eq!(Sampling::parse("point"), Sampling::Point);
    assert_eq!(Sampling::parse("BILINEAR"), Sampling::Bilinear);
    assert_eq!(Sampling::parse("Lanczos"), Sampling::Lanczos);
    assert_eq!(Sampling::parse("area"), Sampling::Area);
    assert_eq!(Sampling::parse("bicubic"), Sampling::Bicubic);
    assert_eq!(
        Sampling::parse("whatever"),
        Sampling::Bicubic,
        "anything else is bicubic"
    );
}

#[test]
fn geometry_scale_sizes() {
    let size = |res: &str, input: (u32, u32), canvas: Option<(u32, u32)>| {
        scale::plan(Target::parse(res), Sampling::Bicubic, false, canvas, input).size
    };
    // an aspect ratio, as scale_filter_tick: wider than the picture keeps the height and sets the width
    // (cy * new_aspect); otherwise the width stays and the height is cx / new_aspect; both truncated
    assert_eq!(size("21:9", (1920, 1080), None), (2520, 1080));
    assert_eq!(size("21:9", (640, 480), None), (1120, 480));
    assert_eq!(size("16:9", (640, 480), None), (853, 480), "truncated");
    assert_eq!(size("16:10", (640, 480), None), (768, 480));
    assert_eq!(size("3:2", (640, 480), None), (720, 480));
    // narrower than the picture: the width stays and the height grows
    assert_eq!(size("4:3", (1920, 1080), None), (1920, 1440));
    assert_eq!(size("4:3", (1280, 720), None), (1280, 960));
    assert_eq!(size("16:10", (1920, 1080), None), (1920, 1200));
    assert_eq!(size("3:2", (1920, 1080), None), (1920, 1280));
    assert_eq!(size("9:16", (1920, 1080), None), (1920, 3413), "truncated");
    // 1:1 is narrower than both 16:9 and 4:3
    assert_eq!(size("1:1", (1920, 1080), None), (1920, 1920));
    assert_eq!(size("1:1", (640, 480), None), (640, 640));
    assert_eq!(size("1:1", (1280, 720), None), (1280, 1280));
    // the aspect the picture has already: nothing to do
    assert_eq!(size("4:3", (640, 480), None), (640, 480));
    assert_eq!(size("16:9", (1920, 1080), None), (1920, 1080));
    assert_eq!(size("16:9", (1280, 720), None), (1280, 720));
    // within OBS's EPSILON (0.0001) of the picture's aspect: nothing to do
    assert_eq!(size("17778:10000", (1920, 1080), None), (1920, 1080));
    assert_eq!(size("17777:10000", (1920, 1080), None), (1920, 1080));
    // outside it the rule applies: 1.779 is wider, 1080 * 1.779 = 1921.3
    assert_eq!(size("17790:10000", (1920, 1080), None), (1921, 1080));
    assert_eq!(size("0:5", (1920, 1080), None), (1920, 1080));
    assert_eq!(size("5:0", (1920, 1080), None), (1920, 1080));
    // a typed size
    assert_eq!(size("1280x720", (1920, 1080), None), (1280, 720));
    assert_eq!(size("640x1000", (1920, 1080), None), (640, 1000));
    assert_eq!(
        size("0x720", (1920, 1080), None),
        (1920, 1080),
        "an unusable size passes through"
    );
    assert_eq!(size("-4x720", (1920, 1080), None), (1920, 1080));
    // None and nonsense
    assert_eq!(size("None", (1920, 1080), None), (1920, 1080));
    assert_eq!(size("what", (1920, 1080), None), (1920, 1080));
    // the base (canvas) resolution is the canvas given to the filter; unknown, the filter does nothing
    assert_eq!(
        size("Base (Canvas) Resolution", (1920, 1080), Some((1280, 720))),
        (1280, 720)
    );
    assert_eq!(
        size("Base (Canvas) Resolution", (320, 240), Some((1280, 720))),
        (1280, 720)
    );
    assert_eq!(
        size("Base (Canvas) Resolution", (1920, 1080), None),
        (1920, 1080)
    );
}

#[test]
fn geometry_scale_picks_the_effect_like_obs() {
    let plan = |res: &str, s: Sampling, undistort: bool, input: (u32, u32)| {
        scale::plan(Target::parse(res), s, undistort, None, input)
    };
    let input = (1920, 1080);
    // no change in size: a copy, whatever the mode
    for s in [
        Sampling::Point,
        Sampling::Bilinear,
        Sampling::Bicubic,
        Sampling::Lanczos,
        Sampling::Area,
    ] {
        assert_eq!(plan("1920x1080", s, true, input).method, Method::Copy);
    }
    // not below half: each mode's own effect
    assert_eq!(
        plan("1440x1080", Sampling::Point, false, input).method,
        Method::Point
    );
    assert_eq!(
        plan("1440x1080", Sampling::Bilinear, false, input).method,
        Method::Bilinear
    );
    assert_eq!(
        plan("1440x1080", Sampling::Bicubic, false, input).method,
        Method::Bicubic { undistort: false }
    );
    assert_eq!(
        plan("1440x1080", Sampling::Lanczos, false, input).method,
        Method::Lanczos { undistort: false }
    );
    assert_eq!(
        plan("1440x1080", Sampling::Area, false, input).method,
        Method::Area { upscale: false }
    );
    assert_eq!(
        plan("960x540", Sampling::Bicubic, false, input).method,
        Method::Bicubic { undistort: false },
        "exactly half is not below half"
    );
    // upscale area needs both directions at least as large
    assert_eq!(
        plan("3840x2160", Sampling::Area, false, input).method,
        Method::Area { upscale: true }
    );
    assert_eq!(
        plan("3840x1080", Sampling::Area, false, input).method,
        Method::Area { upscale: true }
    );
    assert_eq!(
        plan("3840x540", Sampling::Area, false, input).method,
        Method::Area { upscale: false }
    );
    // below half in either direction: the low-res bilinear effect, except for point
    assert_eq!(
        plan("959x1080", Sampling::Bicubic, true, input).method,
        Method::LowRes
    );
    assert_eq!(
        plan("1920x539", Sampling::Lanczos, false, input).method,
        Method::LowRes
    );
    assert_eq!(
        plan("480x270", Sampling::Area, false, input).method,
        Method::LowRes
    );
    assert_eq!(
        plan("480x270", Sampling::Bilinear, false, input).method,
        Method::LowRes
    );
    assert_eq!(
        plan("480x270", Sampling::Point, false, input).method,
        Method::Point
    );
    // undistort only for the cubic kernels, and its factor is the new aspect over the old one
    let p = plan("1440x1080", Sampling::Bicubic, true, input);
    assert_eq!(p.method, Method::Bicubic { undistort: true });
    assert!((p.undistort_factor - (1440.0 / 1080.0) / (1920.0 / 1080.0)).abs() < 1e-6);
    assert_eq!(
        plan("1440x1080", Sampling::Lanczos, true, input).method,
        Method::Lanczos { undistort: true }
    );
    assert_eq!(
        plan("1440x1080", Sampling::Bilinear, true, input).undistort_factor,
        1.0
    );
    assert_eq!(
        plan("1440x1080", Sampling::Area, true, input).undistort_factor,
        1.0
    );
    assert_eq!(
        plan("1440x1080", Sampling::Bicubic, false, input).undistort_factor,
        1.0
    );
    assert_eq!(
        plan("480x270", Sampling::Bicubic, true, input).undistort_factor,
        1.0,
        "not on the low-res path"
    );
}

// ---------------------------------------------------------------------------------------------------------------
// The CPU reference for the scaling kernels

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

type Lin = [f64; 4];

impl Img {
    /// The texel as premultiplied linear light; coordinates outside are clamped to the edge, or transparent.
    fn lin(&self, x: i64, y: i64, border: bool) -> Lin {
        if border && (x < 0 || y < 0 || x >= self.w as i64 || y >= self.h as i64) {
            return [0.0; 4];
        }
        let x = x.clamp(0, self.w as i64 - 1) as usize;
        let y = y.clamp(0, self.h as i64 - 1) as usize;
        let p = self.at(x, y);
        let a = p[3] as f64 / 255.0;
        [
            dec(p[0] as f64) * a,
            dec(p[1] as f64) * a,
            dec(p[2] as f64) * a,
            a,
        ]
    }

    /// Bilinear sample at `pos` in texels (centres at +0.5).
    fn bilinear(&self, pos: [f64; 2], border: bool) -> Lin {
        let (qx, qy) = (pos[0] - 0.5, pos[1] - 0.5);
        let (ix, iy) = (qx.floor(), qy.floor());
        let (fx, fy) = (qx - ix, qy - iy);
        let (ix, iy) = (ix as i64, iy as i64);
        let mut out = [0.0; 4];
        for (dy, wy) in [(0, 1.0 - fy), (1, fy)] {
            for (dx, wx) in [(0, 1.0 - fx), (1, fx)] {
                let t = self.lin(ix + dx, iy + dy, border);
                for c in 0..4 {
                    out[c] += t[c] * wx * wy;
                }
            }
        }
        out
    }
}

/// A premultiplied linear result as code values (straight alpha, gamma encoded).
fn finish(p: Lin) -> [f64; 4] {
    let a = p[3].clamp(0.0, 1.0);
    let rgb = |v: f64| if a > 0.0 { enc(v / a) } else { 0.0 };
    [rgb(p[0]), rgb(p[1]), rgb(p[2]), a * 255.0]
}

/// The cubic convolution kernel with a = -0.75 (OBS's "sharper" bicubic, B = 0, C = 0.75).
fn cubic(t: f64) -> f64 {
    let (a, t) = (-0.75, t.abs());
    if t <= 1.0 {
        (a + 2.0) * t * t * t - (a + 3.0) * t * t + 1.0
    } else if t < 2.0 {
        a * t * t * t - 5.0 * a * t * t + 8.0 * a * t - 4.0 * a
    } else {
        0.0
    }
}

fn lanczos3(t: f64) -> f64 {
    let t = t.abs();
    if t < 1e-9 {
        1.0
    } else if t < 3.0 {
        let p = std::f64::consts::PI * t;
        3.0 * p.sin() * (p / 3.0).sin() / (p * p)
    } else {
        0.0
    }
}

fn tent(t: f64) -> f64 {
    (1.0 - t.abs()).max(0.0)
}

/// Taps and weights of one axis for a sample at `pos` (texels): the `radius` texels either side, kernel by distance,
/// normalised to sum to one.
fn axis_taps(pos: f64, radius: i64, kernel: &dyn Fn(f64) -> f64) -> Vec<(i64, f64)> {
    let first = (pos - 0.5).floor() as i64;
    let mut taps: Vec<(i64, f64)> = ((first - radius + 1)..=(first + radius))
        .map(|i| (i, kernel(i as f64 + 0.5 - pos)))
        .collect();
    let sum: f64 = taps.iter().map(|t| t.1).sum();
    for t in &mut taps {
        t.1 /= sum;
    }
    taps
}

/// Taps and weights of one axis for the box of one output pixel: each source texel by the length of its overlap.
fn box_taps(out_index: usize, out_len: usize, src_len: usize) -> Vec<(i64, f64)> {
    let scale = src_len as f64 / out_len as f64;
    let lo = out_index as f64 * scale;
    let hi = lo + scale;
    let mut taps = Vec::new();
    for i in (lo.floor() as i64)..(hi.ceil() as i64) {
        let overlap = (hi.min(i as f64 + 1.0) - lo.max(i as f64)).max(0.0);
        if overlap > 0.0 {
            taps.push((i, overlap / scale));
        }
    }
    taps
}

#[derive(Clone, Copy, Debug)]
enum Kernel {
    Bilinear,
    Bicubic,
    Lanczos,
    Area,
}

/// A separable resample of `src` to `ow` x `oh`, as premultiplied linear texels turned into code values.
fn reference(src: &Img, ow: usize, oh: usize, kernel: Kernel) -> Vec<[f64; 4]> {
    let mut out = Vec::new();
    for oy in 0..oh {
        for ox in 0..ow {
            let (px, py) = (
                (ox as f64 + 0.5) * src.w as f64 / ow as f64,
                (oy as f64 + 0.5) * src.h as f64 / oh as f64,
            );
            let (xs, ys) = match kernel {
                Kernel::Bilinear => (axis_taps(px, 1, &tent), axis_taps(py, 1, &tent)),
                Kernel::Bicubic => (axis_taps(px, 2, &cubic), axis_taps(py, 2, &cubic)),
                Kernel::Lanczos => (axis_taps(px, 3, &lanczos3), axis_taps(py, 3, &lanczos3)),
                Kernel::Area => (box_taps(ox, ow, src.w), box_taps(oy, oh, src.h)),
            };
            let mut sum = [0.0; 4];
            for (y, wy) in &ys {
                for (x, wx) in &xs {
                    let t = src.lin(*x, *y, false);
                    for c in 0..4 {
                        sum[c] += t[c] * wx * wy;
                    }
                }
            }
            out.push(finish(sum));
        }
    }
    out
}

fn point_reference(src: &Img, ow: usize, oh: usize) -> Vec<[f64; 4]> {
    let mut out = Vec::new();
    for oy in 0..oh {
        for ox in 0..ow {
            let x =
                (((ox as f64 + 0.5) * src.w as f64 / ow as f64).floor() as usize).min(src.w - 1);
            let y =
                (((oy as f64 + 0.5) * src.h as f64 / oh as f64).floor() as usize).min(src.h - 1);
            out.push(src.at(x, y).map(|v| v as f64));
        }
    }
    out
}

/// The eight bilinear taps of the low-res effect (offsets in output pixels: the pattern of OBS's shader).
fn lowres_reference(src: &Img, ow: usize, oh: usize) -> Vec<[f64; 4]> {
    let taps: [(f64, f64); 8] = [
        (1.0, -3.0),
        (-1.0, 3.0),
        (5.0, 1.0),
        (-3.0, -5.0),
        (-5.0, 5.0),
        (-7.0, -1.0),
        (3.0, 7.0),
        (7.0, -7.0),
    ];
    let mut out = Vec::new();
    for oy in 0..oh {
        for ox in 0..ow {
            let mut sum = [0.0; 4];
            for (dx, dy) in taps {
                let u = (ox as f64 + 0.5 + dx / 16.0) / ow as f64;
                let v = (oy as f64 + 0.5 + dy / 16.0) / oh as f64;
                let t = src.bilinear([u * src.w as f64, v * src.h as f64], false);
                for c in 0..4 {
                    sum[c] += t[c] / 8.0;
                }
            }
            out.push(finish(sum));
        }
    }
    out
}

/// The same with the x coordinate of every tap bent by the undistort curve (bicubic only).
fn undistort_reference(src: &Img, ow: usize, oh: usize, factor: f64) -> Vec<[f64; 4]> {
    let bend = |x: f64| {
        let n = (x / src.w as f64 - 0.5) * 2.0;
        let b = (1.0 - factor) * n.powi(5) + factor * n;
        (b * 0.5 + 0.5) * src.w as f64
    };
    let mut out = Vec::new();
    for oy in 0..oh {
        for ox in 0..ow {
            let (px, py) = (
                (ox as f64 + 0.5) * src.w as f64 / ow as f64,
                (oy as f64 + 0.5) * src.h as f64 / oh as f64,
            );
            let (xs, ys) = (axis_taps(px, 2, &cubic), axis_taps(py, 2, &cubic));
            let mut sum = [0.0; 4];
            for (y, wy) in &ys {
                for (x, wx) in &xs {
                    let t = src.bilinear([bend(*x as f64 + 0.5), *y as f64 + 0.5], false);
                    for c in 0..4 {
                        sum[c] += t[c] * wx * wy;
                    }
                }
            }
            out.push(finish(sum));
        }
    }
    out
}

/// The worst channel difference between a result and its reference; the position of the worst one with it.
fn worst(got: &Img, want: &[[f64; 4]]) -> (f64, usize) {
    assert_eq!(got.px.len(), want.len());
    let mut worst = (0.0f64, 0usize);
    for (i, (g, w)) in got.px.iter().zip(want).enumerate() {
        for c in 0..4 {
            let d = (g[c] as f64 - w[c]).abs();
            if d > worst.0 {
                worst = (d, i);
            }
        }
    }
    worst
}

fn expect_close(label: &str, got: &Img, want: &[[f64; 4]], tolerance: f64) {
    let (d, i) = worst(got, want);
    eprintln!("{label}: worst difference {d:.2} code values");
    assert!(
        d <= tolerance,
        "{label}: pixel ({}, {}) is {:?}, the reference says {:?} (off by {d:.2})",
        i % got.w,
        i / got.w,
        got.px[i],
        want[i].map(|v| (v * 100.0).round() / 100.0)
    );
}

fn scale_params(sampling: &str, res: &str, undistort: bool) -> Value {
    json!({ "sampling": sampling, "resolution": res, "undistort": undistort })
}

// ---------------------------------------------------------------------------------------------------------------
// Scaling/Aspect Ratio: the picture

#[test]
fn geometry_scale_4_3_on_a_picture() {
    let r = warp();
    let img = pattern(192, 108);
    let out = run_one(
        &r,
        "scale_filter",
        scale_params("bicubic", "4:3", false),
        &img,
    );
    // 4:3 is narrower than 16:9: the width stays and the height is 192 / (4/3) = 144
    assert_eq!((out.w, out.h), (192, 144));
    expect_close(
        "4:3",
        &out,
        &reference(&img, 192, 144, Kernel::Bicubic),
        2.0,
    );
}

#[test]
fn geometry_scale_to_a_typed_size_and_to_the_canvas() {
    let r = warp();
    let img = pattern(64, 48);
    let out = run_one(
        &r,
        "scale_filter",
        scale_params("bilinear", "40x30", false),
        &img,
    );
    assert_eq!((out.w, out.h), (40, 30));
    expect_close(
        "40x30",
        &out,
        &reference(&img, 40, 30, Kernel::Bilinear),
        2.0,
    );

    // the base (canvas) resolution is the one the engine gives the chain
    let filters = [cfg(
        "s",
        "scale_filter",
        scale_params("bilinear", "Base (Canvas) Resolution", false),
    )];
    let out = run_chain(&r, &filters, Some((96, 54)), &img);
    assert_eq!((out.w, out.h), (96, 54));
    expect_close(
        "canvas",
        &out,
        &reference(&img, 96, 54, Kernel::Bilinear),
        2.0,
    );
    // without a canvas the filter passes the picture through
    let out = run_chain(&r, &filters, None, &img);
    same(&out, &img);
    // and it follows the canvas when that changes
    let mut chain = Chain::new();
    chain.apply(&r, &filters);
    for canvas in [(32u32, 24u32), (80, 45), (32, 24)] {
        chain.set_canvas(canvas);
        let tex = r.texture(64, 48).unwrap();
        fill_rgba(&r, &tex, |x, y| img.at(x as usize, y as usize));
        let out = chain.run(&r, tex).unwrap();
        assert_eq!((out.width, out.height), canvas);
    }
    // a filter made after the canvas was set gets it at once
    chain.apply(
        &r,
        &[cfg(
            "late",
            "scale_filter",
            scale_params("bilinear", "Base (Canvas) Resolution", false),
        )],
    );
    let tex = r.texture(64, 48).unwrap();
    let out = chain.run(&r, tex).unwrap();
    assert_eq!((out.width, out.height), (32, 24));
}

#[test]
fn geometry_scale_none_and_equal_sizes_leave_the_picture_alone() {
    let r = warp();
    let img = pattern_translucent(37, 21);
    for res in ["None", "garbage", "37x21", "37:21", "0x0"] {
        for s in ["point", "bilinear", "bicubic", "lanczos", "area"] {
            let out = run_one(&r, "scale_filter", scale_params(s, res, true), &img);
            same(&out, &img);
        }
    }
    // defaults: bicubic, no resolution
    let out = run_one(&r, "scale_filter", json!({}), &img);
    same(&out, &img);
}

#[test]
fn geometry_scale_each_sampling_mode_matches_its_reference() {
    let r = warp();
    // 1.5x up, 0.75x down, up in x and down in y, and just above half: not the low-res path
    let sizes = [(36usize, 24usize), (18, 12), (36, 12), (13, 9)];
    for (name, img) in [
        ("opaque", pattern(24, 16)),
        ("translucent", pattern_translucent(24, 16)),
    ] {
        for (ow, oh) in sizes {
            let res = format!("{ow}x{oh}");
            let run = |s: &str| run_one(&r, "scale_filter", scale_params(s, &res, false), &img);
            let label = |s: &str| format!("{name} {s} -> {res}");

            let out = run("point");
            assert_eq!((out.w, out.h), (ow, oh));
            expect_close(&label("point"), &out, &point_reference(&img, ow, oh), 0.0);
            expect_close(
                &label("bilinear"),
                &run("bilinear"),
                &reference(&img, ow, oh, Kernel::Bilinear),
                2.0,
            );
            expect_close(
                &label("bicubic"),
                &run("bicubic"),
                &reference(&img, ow, oh, Kernel::Bicubic),
                2.0,
            );
            expect_close(
                &label("lanczos"),
                &run("lanczos"),
                &reference(&img, ow, oh, Kernel::Lanczos),
                2.0,
            );
            expect_close(
                &label("area"),
                &run("area"),
                &reference(&img, ow, oh, Kernel::Area),
                2.0,
            );
        }
    }
}

#[test]
fn geometry_scale_below_half_uses_the_low_res_bilinear_for_every_mode_but_point() {
    let r = warp();
    let img = pattern_translucent(48, 32);
    for (ow, oh) in [(20usize, 14usize), (11, 7), (24, 10)] {
        let res = format!("{ow}x{oh}");
        let want = lowres_reference(&img, ow, oh);
        for s in ["bilinear", "bicubic", "lanczos", "area"] {
            let out = run_one(&r, "scale_filter", scale_params(s, &res, true), &img);
            expect_close(&format!("{s} -> {res}"), &out, &want, 2.0);
        }
        let out = run_one(&r, "scale_filter", scale_params("point", &res, false), &img);
        expect_close(
            &format!("point -> {res}"),
            &out,
            &point_reference(&img, ow, oh),
            0.0,
        );
    }
}

#[test]
fn geometry_scale_flat_areas_and_edges_are_plausible() {
    let r = warp();
    // a flat picture stays flat at its colour in every mode and direction
    let flat = Img::new(20, 20, |_, _| [200, 100, 50, 255]);
    for s in ["point", "bilinear", "bicubic", "lanczos", "area"] {
        for res in ["31x31", "15x15", "7x7", "40x13"] {
            let out = run_one(&r, "scale_filter", scale_params(s, res, false), &flat);
            for p in &out.px {
                assert!(
                    p[0].abs_diff(200) <= 1
                        && p[1].abs_diff(100) <= 1
                        && p[2].abs_diff(50) <= 1
                        && p[3] == 255,
                    "{s} {res}: {p:?}"
                );
            }
        }
    }
    // a step edge: dark on the left, light on the right, upscaled by 1.6; the crossing stays at the middle, within
    // one output pixel, no mode shifts it, and the far ends keep their values
    let step = Img::new(40, 8, |x, _| {
        if x < 20 {
            [64, 64, 64, 255]
        } else {
            [192, 192, 192, 255]
        }
    });
    for s in ["point", "bilinear", "bicubic", "lanczos", "area"] {
        let out = run_one(&r, "scale_filter", scale_params(s, "64x8", false), &step);
        assert_eq!((out.w, out.h), (64, 8));
        let row: Vec<u8> = (0..64).map(|x| out.at(x, 3)[0]).collect();
        // the middle code between the two plateaus: the edge is where the row passes it
        let mid = 128u8;
        let crossing = row
            .iter()
            .position(|v| *v >= mid)
            .expect("the row reaches the light side");
        assert!(
            (31..=33).contains(&crossing),
            "{s}: the edge crosses at {crossing}: {row:?}"
        );
        for (x, v) in row.iter().enumerate() {
            if x <= 26 {
                assert!(v.abs_diff(64) <= 2, "{s}: dark side x={x} is {v}: {row:?}");
            }
            if x >= 37 {
                assert!(
                    v.abs_diff(192) <= 2,
                    "{s}: light side x={x} is {v}: {row:?}"
                );
            }
        }
        // all rows are equal: no vertical leak on a vertical edge
        for y in 0..8 {
            assert_eq!(out.at(10, y), out.at(10, 0), "{s}");
            assert_eq!(out.at(40, y), out.at(40, 0), "{s}");
        }
    }
    // the same step with the same dark and light plateaus in a downscale to 2/3
    let step = Img::new(30, 6, |x, _| {
        if x < 15 {
            [64, 64, 64, 255]
        } else {
            [192, 192, 192, 255]
        }
    });
    for s in ["bilinear", "bicubic", "lanczos", "area"] {
        let out = run_one(&r, "scale_filter", scale_params(s, "20x4", false), &step);
        let row: Vec<u8> = (0..20).map(|x| out.at(x, 1)[0]).collect();
        let crossing = row.iter().position(|v| *v >= 128).unwrap();
        assert!((9..=10).contains(&crossing), "{s}: {row:?}");
    }
    // a bicubic or lanczos result may ring beside an edge, but never by more than the kernel allows
    let step = Img::new(40, 4, |x, _| {
        if x < 20 {
            [0, 0, 0, 255]
        } else {
            [255, 255, 255, 255]
        }
    });
    for s in ["bicubic", "lanczos"] {
        let out = run_one(&r, "scale_filter", scale_params(s, "60x4", false), &step);
        let row: Vec<u8> = (0..60).map(|x| out.at(x, 0)[0]).collect();
        // a flat region is exactly the end value
        assert_eq!(row[0], 0, "{s}");
        assert_eq!(row[59], 255, "{s}");
        // the half-way pixel sits next to the edge, which is at 30
        let crossing = row.iter().position(|v| *v >= 128).unwrap();
        assert!((29..=31).contains(&crossing), "{s}: {row:?}");
    }
}

#[test]
fn geometry_scale_undistort_runs_both_ways() {
    let r = warp();
    let img = pattern(48, 16);
    // an ultrawide picture to 4:3 -> new aspect over old: the middle stays where it is, the edges bend
    let off = run_one(
        &r,
        "scale_filter",
        scale_params("bicubic", "30x16", false),
        &img,
    );
    let on = run_one(
        &r,
        "scale_filter",
        scale_params("bicubic", "30x16", true),
        &img,
    );
    assert_eq!((off.w, off.h), (30, 16));
    assert_eq!((on.w, on.h), (30, 16));
    assert_ne!(
        on.px, off.px,
        "the undistort curve changes the picture when the aspect changes"
    );
    let factor = (30.0 / 16.0) / (48.0 / 16.0);
    expect_close(
        "bicubic undistort",
        &on,
        &undistort_reference(&img, 30, 16, factor),
        2.0,
    );
    expect_close(
        "bicubic no undistort",
        &off,
        &reference(&img, 30, 16, Kernel::Bicubic),
        2.0,
    );
    // the lanczos kernel takes the same switch
    let on = run_one(
        &r,
        "scale_filter",
        scale_params("lanczos", "30x16", true),
        &img,
    );
    let off = run_one(
        &r,
        "scale_filter",
        scale_params("lanczos", "30x16", false),
        &img,
    );
    assert_ne!(on.px, off.px);
    // bilinear, point and area ignore it
    for s in ["point", "bilinear", "area"] {
        let on = run_one(&r, "scale_filter", scale_params(s, "30x16", true), &img);
        let off = run_one(&r, "scale_filter", scale_params(s, "30x16", false), &img);
        same(&on, &off);
    }
    // the same aspect has factor 1: no bend, the result is the plain bicubic one
    let same_aspect_on = run_one(
        &r,
        "scale_filter",
        scale_params("bicubic", "96x32", true),
        &img,
    );
    let same_aspect_off = run_one(
        &r,
        "scale_filter",
        scale_params("bicubic", "96x32", false),
        &img,
    );
    let off_codes: Vec<[f64; 4]> = same_aspect_off
        .px
        .iter()
        .map(|p| p.map(|v| v as f64))
        .collect();
    expect_close("factor 1", &same_aspect_on, &off_codes, 2.0);
}

// ---------------------------------------------------------------------------------------------------------------
// Transform

/// Rotate 90 degrees clockwise: output (x, y) is input (y, H-1-x).
fn rot_cw(i: &Img) -> Img {
    Img::new(i.h, i.w, |x, y| i.at(y, i.h - 1 - x))
}

fn rot_ccw(i: &Img) -> Img {
    Img::new(i.h, i.w, |x, y| i.at(i.w - 1 - y, x))
}

fn rot_180(i: &Img) -> Img {
    Img::new(i.w, i.h, |x, y| i.at(i.w - 1 - x, i.h - 1 - y))
}

fn mirror_h(i: &Img) -> Img {
    Img::new(i.w, i.h, |x, y| i.at(i.w - 1 - x, y))
}

fn mirror_v(i: &Img) -> Img {
    Img::new(i.w, i.h, |x, y| i.at(x, i.h - 1 - y))
}

fn transform(r: &Arc<Renderer>, img: &Img, flip_h: bool, flip_v: bool, rotation: f64) -> Img {
    run_one(
        r,
        "transform",
        json!({ "flip_h": flip_h, "flip_v": flip_v, "rotation": rotation }),
        img,
    )
}

#[test]
fn geometry_transform_rotation_90_swaps_the_sides() {
    let r = warp();
    let img = pattern(160, 90);
    let out = transform(&r, &img, false, false, 90.0);
    assert_eq!((out.w, out.h), (90, 160));
    // a known corner: clockwise, the top left pixel goes to the top right
    assert_eq!(out.at(89, 0), img.at(0, 0));
    assert_eq!(
        out.at(89, 159),
        img.at(159, 0),
        "the top right goes to the bottom right"
    );
    assert_eq!(
        out.at(0, 159),
        img.at(159, 89),
        "the bottom right goes to the bottom left"
    );
    assert_eq!(
        out.at(0, 0),
        img.at(0, 89),
        "the bottom left goes to the top left"
    );
    same(&out, &rot_cw(&img));
}

#[test]
fn geometry_transform_rotation_180_and_270() {
    let r = warp();
    let img = pattern(160, 90);
    let out = transform(&r, &img, false, false, 180.0);
    assert_eq!((out.w, out.h), (160, 90));
    assert_eq!(out.at(159, 89), img.at(0, 0));
    same(&out, &rot_180(&img));
    // -180 is the same turn
    same(&transform(&r, &img, false, false, -180.0), &out);

    // the slider runs from -180 to 180, so a turn of 270 clockwise is -90
    let out = transform(&r, &img, false, false, -90.0);
    assert_eq!((out.w, out.h), (90, 160));
    assert_eq!(
        out.at(0, 159),
        img.at(0, 0),
        "counter-clockwise, the top left goes to the bottom left"
    );
    same(&out, &rot_ccw(&img));
    // a value past the end of the range is clamped to 180
    same(&transform(&r, &img, false, false, 270.0), &rot_180(&img));
    // a value a hair off a quarter turn (the slider has steps of 0.1, so this is the same turn)
    same(&transform(&r, &img, false, false, 90.005), &rot_cw(&img));
    // zero is the picture itself
    same(&transform(&r, &img, false, false, 0.0), &img);
    same(&run_one(&r, "transform", json!({}), &img), &img);
}

#[test]
fn geometry_transform_flips_move_a_known_pixel() {
    let r = warp();
    let img = pattern(40, 30);
    let h = transform(&r, &img, true, false, 0.0);
    assert_eq!((h.w, h.h), (40, 30));
    assert_eq!(h.at(39, 4), img.at(0, 4));
    assert_eq!(h.at(0, 4), img.at(39, 4));
    same(&h, &mirror_h(&img));
    let v = transform(&r, &img, false, true, 0.0);
    assert_eq!(v.at(7, 29), img.at(7, 0));
    same(&v, &mirror_v(&img));
    let both = transform(&r, &img, true, true, 0.0);
    same(&both, &rot_180(&img));
}

#[test]
fn geometry_transform_flip_comes_before_the_rotation() {
    let r = warp();
    let img = pattern(40, 30);
    same(
        &transform(&r, &img, true, false, 90.0),
        &rot_cw(&mirror_h(&img)),
    );
    same(
        &transform(&r, &img, false, true, 90.0),
        &rot_cw(&mirror_v(&img)),
    );
    same(
        &transform(&r, &img, true, false, -90.0),
        &rot_ccw(&mirror_h(&img)),
    );
    same(
        &transform(&r, &img, true, true, 90.0),
        &rot_cw(&rot_180(&img)),
    );
    same(
        &transform(&r, &img, true, false, 180.0),
        &rot_180(&mirror_h(&img)),
    );
}

/// What the Transform shader should do at an angle that is not a quarter turn: each output pixel turned back about the
/// centre, the mirror undone, a bilinear sample with a transparent border.
fn rotate_reference(
    src: &Img,
    ow: usize,
    oh: usize,
    flip_h: bool,
    flip_v: bool,
    deg: f64,
) -> Vec<[f64; 4]> {
    let (s, c) = deg.to_radians().sin_cos();
    let mut out = Vec::new();
    for y in 0..oh {
        for x in 0..ow {
            let (dx, dy) = (
                x as f64 + 0.5 - ow as f64 / 2.0,
                y as f64 + 0.5 - oh as f64 / 2.0,
            );
            let mut sx = dx * c + dy * s + src.w as f64 / 2.0;
            let mut sy = -dx * s + dy * c + src.h as f64 / 2.0;
            if flip_h {
                sx = src.w as f64 - sx;
            }
            if flip_v {
                sy = src.h as f64 - sy;
            }
            out.push(finish(src.bilinear([sx, sy], true)));
        }
    }
    out
}

#[test]
fn geometry_transform_other_angles_resample_into_the_bounding_box() {
    let r = warp();
    // the size: 100 x 50 turned by 30 degrees fills 100 cos30 + 50 sin30 = 111.6 by 100 sin30 + 50 cos30 = 93.3
    let img = pattern_translucent(100, 50);
    let out = transform(&r, &img, false, false, 30.0);
    assert_eq!((out.w, out.h), (112, 94));
    expect_close(
        "30 degrees",
        &out,
        &rotate_reference(&img, 112, 94, false, false, 30.0),
        2.0,
    );
    let out = transform(&r, &img, true, false, -17.5);
    // cos 17.5 = 0.9537, sin 17.5 = 0.3007: 100 * 0.9537 + 50 * 0.3007 = 110.4, 100 * 0.3007 + 50 * 0.9537 = 77.8
    assert_eq!((out.w, out.h), (111, 78));
    expect_close(
        "-17.5 mirrored",
        &out,
        &rotate_reference(&img, 111, 78, true, false, -17.5),
        2.0,
    );

    // 45 degrees of a flat square: a diamond, the corners of the picture stay transparent, the middle is the colour
    let flat = Img::new(64, 64, |_, _| [200, 100, 50, 255]);
    let out = transform(&r, &flat, false, false, 45.0);
    assert_eq!((out.w, out.h), (91, 91));
    for corner in [(0, 0), (90, 0), (0, 90), (90, 90)] {
        assert_eq!(
            out.at(corner.0, corner.1),
            [0, 0, 0, 0],
            "corner {corner:?}"
        );
    }
    assert_eq!(out.at(45, 45), [200, 100, 50, 255]);
    let tip = out.at(45, 0);
    assert!(
        tip[3] < 255,
        "the tip of the diamond is not opaque: {tip:?}"
    );
    let side = out.at(30, 30);
    assert!(side[3] == 255, "inside the diamond: {side:?}");

    // the sense of the turn: 89.9 degrees (resampled) shows the same picture as 90 (a pixel move), within what a
    // 0.1 degree turn and a half-pixel shift of the grid change on a smooth gradient
    let ramp = Img::new(160, 90, |x, y| {
        [(x * 255 / 159) as u8, (y * 255 / 89) as u8, 128, 255]
    });
    let exact = transform(&r, &ramp, false, false, 90.0);
    let near = transform(&r, &ramp, false, false, 89.9);
    assert_eq!((near.w, near.h), (91, 161));
    let mut worst_diff = 0u8;
    for y in 4..156 {
        for x in 4..86 {
            for c in 0..3 {
                worst_diff = worst_diff.max(exact.at(x, y)[c].abs_diff(near.at(x, y)[c]));
            }
        }
    }
    assert!(
        worst_diff <= 4,
        "89.9 degrees differs from 90 by {worst_diff}"
    );
    // and the other way round it would be the opposite turn
    let wrong = transform(&r, &ramp, false, false, -89.9);
    let far = |i: &Img| i.at(10, 20);
    assert!(
        (0..2).any(|c| far(&wrong)[c].abs_diff(far(&exact)[c]) > 40),
        "{:?} vs {:?}",
        far(&wrong),
        far(&exact)
    );
}

// ---------------------------------------------------------------------------------------------------------------
// The chain

#[test]
fn geometry_a_chain_of_crop_scale_and_transform_passes_the_sizes_along() {
    let r = warp();
    let img = pattern(200, 100);
    let filters = [
        cfg(
            "c",
            "crop_filter",
            json!({ "left": 20, "top": 10, "right": 20, "bottom": 10 }),
        ),
        cfg("s", "scale_filter", scale_params("bilinear", "4:3", false)),
        cfg("t", "transform", json!({ "rotation": 90 })),
    ];
    // crop: 160 x 80; scale 4:3 (narrower than 2.0) keeps the width: 160 x 120; the turn: 120 x 160
    let out = run_chain(&r, &filters, None, &img);
    assert_eq!((out.w, out.h), (120, 160));

    // order matters: the same filters the other way round
    let reversed = [filters[2].clone(), filters[1].clone(), filters[0].clone()];
    // turn: 100 x 200; scale 4:3 (1.33 is wider than 0.5: the height stays, the width is 266); crop 20/10 -> 226 x 180
    let out = run_chain(&r, &reversed, None, &img);
    assert_eq!((out.w, out.h), (266 - 40, 200 - 20));

    // a disabled filter changes nothing; the chain reports the size of what is left
    let mut chain = Chain::new();
    let mut off = filters.to_vec();
    off[1].enabled = false;
    chain.apply(&r, &off);
    let tex = r.texture(200, 100).unwrap();
    let out = chain.run(&r, tex).unwrap();
    assert_eq!((out.width, out.height), (80, 160));

    // scale to the canvas and then pad: the pad is added to the canvas size
    let filters = [
        cfg(
            "s",
            "scale_filter",
            scale_params("bicubic", "Base (Canvas) Resolution", false),
        ),
        cfg("c", "crop_filter", json!({ "left": -4, "right": -4 })),
    ];
    let out = run_chain(&r, &filters, Some((96, 54)), &img);
    assert_eq!((out.w, out.h), (104, 54));
    assert_eq!(out.at(0, 10), [0, 0, 0, 0]);
    assert_eq!(out.at(103, 10), [0, 0, 0, 0]);
}

#[test]
fn geometry_filters_accept_garbage_settings() {
    let r = warp();
    let img = pattern(32, 24);
    let garbage = json!({
        "relative": "yes", "left": "x", "cx": [1], "sampling": 5, "resolution": 7, "undistort": "no",
        "flip_h": 1, "flip_v": null, "rotation": "fast", "right": f64::NAN
    });
    for kind in ["crop_filter", "scale_filter", "transform"] {
        let out = run_one(&r, kind, garbage.clone(), &img);
        same(&out, &img);
        let out = run_one(&r, kind, Value::Null, &img);
        same(&out, &img);
    }
    // out-of-range sliders clamp to the edge of their range
    let a = transform(&r, &img, false, false, 999.0);
    let b = transform(&r, &img, false, false, 180.0);
    same(&a, &b);
}

#[test]
fn geometry_every_shader_compiles() {
    let r = warp();
    for kind in ["crop_filter", "scale_filter", "transform"] {
        let mut f = crate::fx::make(kind).expect(kind);
        f.warm_up(&r).unwrap_or_else(|e| panic!("{kind}: {e}"));
    }
    for kind in ["crop_filter", "scale_filter", "transform"] {
        let f = crate::fx::make(kind).unwrap();
        assert_eq!(
            f.output_size((123, 45)),
            (123, 45),
            "{kind}: defaults leave the size alone"
        );
    }
}
