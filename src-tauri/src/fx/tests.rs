//! Tests of the filter framework and Color Correction on WARP.

use std::sync::Arc;

use serde_json::{json, Value};
use windows::Win32::Graphics::Direct3D11::ID3D11ShaderResourceView;

use super::color;
use super::shader_filter::{ExtraInput, ShaderFilter, ShaderFilterDef};
use super::*;
use crate::gpu::pipeline_tests::{fill_rgba, read_rgba, warp};
use crate::gpu::{GpuError, GpuResult, Renderer, RgbaTexture};

// ---------------------------------------------------------------------------------------------------------------
// Helpers

fn cfg(id: &str, kind: &str, enabled: bool, params: Value) -> FilterCfg {
    FilterCfg {
        id: id.into(),
        kind: kind.into(),
        enabled,
        params: params.as_object().cloned().unwrap_or_default(),
    }
}

fn color_cfg(params: Value) -> Vec<FilterCfg> {
    vec![cfg("c", "color_filter", true, params)]
}

type Pixel = [u8; 4];

/// Run `pixels` (a `w` x `h` image) through `chain` and read the result back.
fn run(r: &Arc<Renderer>, chain: &mut Chain, w: u32, h: u32, pixels: &[Pixel]) -> Vec<Pixel> {
    let tex = r.texture(w, h).unwrap();
    fill_rgba(r, &tex, |x, y| pixels[(y * w + x) as usize]);
    let out = chain.run(r, tex).unwrap();
    assert_eq!((out.width, out.height), (w, h));
    read_rgba(r, &out)
        .chunks(4)
        .map(|p| [p[0], p[1], p[2], p[3]])
        .collect()
}

/// One filter of `kind` with `params` applied to `pixels`.
fn run_color(r: &Arc<Renderer>, params: Value, w: u32, h: u32, pixels: &[Pixel]) -> Vec<Pixel> {
    let mut chain = Chain::new();
    chain.apply(r, &color_cfg(params));
    run(r, &mut chain, w, h, pixels)
}

fn one(r: &Arc<Renderer>, params: Value, px: Pixel) -> Pixel {
    run_color(r, params, 1, 1, &[px])[0]
}

fn near(got: Pixel, want: [f64; 4], tol: f64) -> bool {
    got.iter()
        .zip(want)
        .all(|(g, w)| (*g as f64 - w).abs() <= tol)
}

/// The test picture: grays, primaries, secondaries, a skin tone, and a gradient with every alpha from opaque to
/// nearly clear.
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

// ---------------------------------------------------------------------------------------------------------------
// The CPU reference: Color Correction written from the OBS maths as a sequence of plain steps on one pixel, not as
// matrices. Light is linear (OBS runs the effect on an sRGB texture and framebuffer), alpha is straight.

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

#[derive(Clone, Copy)]
struct Ref {
    gamma: f64,
    contrast: f64,
    brightness: f64,
    saturation: f64,
    hue: f64,
    opacity: f64,
    multiply: [u8; 3],
    add: [u8; 3],
}

impl Default for Ref {
    fn default() -> Ref {
        Ref {
            gamma: 0.0,
            contrast: 0.0,
            brightness: 0.0,
            saturation: 0.0,
            hue: 0.0,
            opacity: 1.0,
            multiply: [255; 3],
            add: [0; 3],
        }
    }
}

/// OBS stores a colour as 0xAABBGGRR.
fn obs_color(c: [u8; 3]) -> u32 {
    c[0] as u32 | (c[1] as u32) << 8 | (c[2] as u32) << 16
}

impl Ref {
    fn json(&self) -> Value {
        json!({
            "gamma": self.gamma, "contrast": self.contrast, "brightness": self.brightness,
            "saturation": self.saturation, "hue_shift": self.hue, "opacity": self.opacity,
            "color_multiply": obs_color(self.multiply), "color_add": obs_color(self.add),
        })
    }

    /// The expected pixel in code values (not rounded).
    fn apply(&self, px: Pixel) -> [f64; 4] {
        let mut c = [dec(px[0] as f64), dec(px[1] as f64), dec(px[2] as f64)];
        let mut a = px[3] as f64 / 255.0;

        // gamma: a negative setting darkens the mid-tones (exponent above 1), a positive one lifts them
        let exponent = if self.gamma < 0.0 {
            1.0 - self.gamma
        } else {
            1.0 / (1.0 + self.gamma)
        };
        c = c.map(|v| v.powf(exponent));

        // contrast: a plain gain, 1 / (1 - x) below zero and 1 + x above
        let gain = if self.contrast < 0.0 {
            1.0 / (1.0 - self.contrast)
        } else {
            1.0 + self.contrast
        };
        c = c.map(|v| v * gain);

        // brightness: added in proportion to alpha (the translation row of the matrix meets alpha)
        c = c.map(|v| v + a * self.brightness);

        // saturation: blend between the luma and the colour
        let s = 1.0 + self.saturation;
        let luma = 0.299 * c[0] + 0.587 * c[1] + 0.114 * c[2];
        c = c.map(|v| luma + s * (v - luma));

        // hue: rotate about the grey axis (1, 1, 1) / sqrt(3). A positive setting turns red towards blue, which is
        // a rotation by minus the angle in the right-handed sense (Rodrigues' formula).
        let theta = -self.hue.to_radians();
        let k = 1.0 / 3.0f64.sqrt();
        let cross = [k * (c[2] - c[1]), k * (c[0] - c[2]), k * (c[1] - c[0])];
        let dot = k * (c[0] + c[1] + c[2]);
        c = [0, 1, 2].map(|i| {
            c[i] * theta.cos() + cross[i] * theta.sin() + k * dot * (1.0 - theta.cos())
        });

        // opacity scales alpha
        a *= self.opacity;

        // colour multiply and add (picked as sRGB, applied as linear); the add is scaled by the new alpha
        let m = self.multiply.map(|v| dec(v as f64));
        let d = self.add.map(|v| dec(v as f64));
        c = [0, 1, 2].map(|i| c[i] * m[i] + a * d[i]);

        [enc(c[0]), enc(c[1]), enc(c[2]), a.clamp(0.0, 1.0) * 255.0]
    }
}

#[test]
fn defaults_are_the_identity_on_a_gradient_with_translucent_pixels() {
    let r = warp();
    // 256 x 4: every code value of every channel, with alpha 255, 192, 128 and 32 in the four rows
    let (w, h) = (256u32, 4u32);
    let alphas = [255u8, 192, 128, 32];
    let mut px = Vec::new();
    for y in 0..h {
        for x in 0..w {
            px.push([x as u8, 255 - x as u8, (x * 3 % 256) as u8, alphas[y as usize]]);
        }
    }
    for params in [json!({}), Ref::default().json()] {
        let out = run_color(&r, params, w, h, &px);
        let worst = px
            .iter()
            .zip(&out)
            .flat_map(|(a, b)| a.iter().zip(b).map(|(a, b)| (*a as i32 - *b as i32).abs()))
            .max()
            .unwrap();
        assert!(worst <= 1, "identity differs by {worst}");
    }
}

#[test]
fn reference_values_for_each_parameter_and_a_combination() {
    let r = warp();
    let (w, h, px) = picture();
    let d = Ref::default();
    let sets: Vec<(&str, Ref)> = vec![
        ("gamma +1", Ref { gamma: 1.0, ..d }),
        ("gamma -1.5", Ref { gamma: -1.5, ..d }),
        ("contrast +1", Ref { contrast: 1.0, ..d }),
        ("contrast -1", Ref { contrast: -1.0, ..d }),
        ("brightness +0.2", Ref { brightness: 0.2, ..d }),
        ("brightness -0.2", Ref { brightness: -0.2, ..d }),
        ("saturation -1", Ref { saturation: -1.0, ..d }),
        ("saturation +2", Ref { saturation: 2.0, ..d }),
        ("hue +90", Ref { hue: 90.0, ..d }),
        ("hue -45", Ref { hue: -45.0, ..d }),
        ("hue 180", Ref { hue: 180.0, ..d }),
        ("opacity 0.5", Ref { opacity: 0.5, ..d }),
        ("multiply", Ref { multiply: [255, 128, 40], ..d }),
        ("add", Ref { add: [64, 128, 255], ..d }),
        (
            "combined",
            Ref {
                gamma: 0.4,
                contrast: 0.5,
                brightness: 0.05,
                saturation: 0.7,
                hue: 30.0,
                opacity: 0.8,
                multiply: [240, 220, 255],
                add: [10, 20, 30],
            },
        ),
    ];
    for (name, set) in sets {
        let out = run_color(&r, set.json(), w, h, &px);
        let mut worst = 0.0f64;
        for (i, (got, src)) in out.iter().zip(&px).enumerate() {
            let want = set.apply(*src);
            for c in 0..4 {
                worst = worst.max((got[c] as f64 - want[c]).abs());
            }
            assert!(
                near(*got, want, 2.0),
                "{name}: pixel {i} {src:?} -> {got:?}, expected about {want:?}"
            );
        }
        eprintln!("{name}: worst difference {worst:.2} code values");
    }
}

#[test]
fn hand_checked_values() {
    let r = warp();
    let red = [255, 0, 0, 255];
    let mid = [128, 128, 128, 255];

    // saturation -1: every pixel becomes grey at the OBS luma weights (0.299, 0.587, 0.114) in linear light
    let (w, h, px) = picture();
    let out = run_color(&r, json!({ "saturation": -1 }), w, h, &px);
    for (src, got) in px.iter().zip(&out) {
        assert!(
            got[0].abs_diff(got[1]) <= 1 && got[1].abs_diff(got[2]) <= 1,
            "{src:?} -> {got:?} is not grey"
        );
        let luma = 0.299 * dec(src[0] as f64) + 0.587 * dec(src[1] as f64) + 0.114 * dec(src[2] as f64);
        assert!((got[0] as f64 - enc(luma)).abs() <= 1.5, "{src:?} -> {got:?}");
    }
    // red alone: luma 0.299 in linear light is code 149
    let got = one(&r, json!({ "saturation": -1 }), red);
    assert!(near(got, [149.0, 149.0, 149.0, 255.0], 1.0), "{got:?}");

    // hue 180 on pure red: the grey-axis rotation sends (1, 0, 0) to (-1/3, 2/3, 2/3), clamped to (0, 2/3, 2/3);
    // 2/3 in linear light is code 213
    let got = one(&r, json!({ "hue_shift": 180 }), red);
    assert!(near(got, [0.0, 213.0, 213.0, 255.0], 1.0), "{got:?}");
    // hue 90: OBS's matrix turns red towards blue, (0.333, -0.244, 0.911) in linear light
    let got = one(&r, json!({ "hue_shift": 90 }), red);
    assert!(near(got, [156.0, 0.0, 245.0, 255.0], 1.0), "{got:?}");

    // mid grey 128 is 0.2159 in linear light
    // contrast +1 doubles it: 0.4317 is code 176
    let got = one(&r, json!({ "contrast": 1 }), mid);
    assert!(near(got, [176.0, 176.0, 176.0, 255.0], 1.0), "{got:?}");
    // contrast -1 halves it: 0.1079 is code 92
    let got = one(&r, json!({ "contrast": -1 }), mid);
    assert!(near(got, [92.0, 92.0, 92.0, 255.0], 1.0), "{got:?}");
    // brightness +0.1 adds to it: 0.3159 is code 152
    let got = one(&r, json!({ "brightness": 0.1 }), mid);
    assert!(near(got, [152.0, 152.0, 152.0, 255.0], 1.0), "{got:?}");
    // gamma +1 is the exponent 1/2: sqrt(0.2159) = 0.4646 is code 181; gamma -1 is the exponent 2: 0.0466 is code 61
    let got = one(&r, json!({ "gamma": 1 }), mid);
    assert!(near(got, [181.0, 181.0, 181.0, 255.0], 1.0), "{got:?}");
    let got = one(&r, json!({ "gamma": -1 }), mid);
    assert!(near(got, [61.0, 61.0, 61.0, 255.0], 1.0), "{got:?}");

    // opacity 0.5 halves alpha and leaves the colour alone
    let got = one(&r, json!({ "opacity": 0.5 }), [10, 120, 240, 200]);
    assert!(near(got, [10.0, 120.0, 240.0, 100.0], 1.0), "{got:?}");

    // colour multiply: white takes the colour (0xAABBGGRR: R 255, G 128, B 0), black stays black
    let multiply = json!({ "color_multiply": 0x0000_80FFu32 });
    let got = one(&r, multiply.clone(), [255, 255, 255, 255]);
    assert!(near(got, [255.0, 128.0, 0.0, 255.0], 1.0), "{got:?}");
    let got = one(&r, multiply, [0, 0, 0, 255]);
    assert!(near(got, [0.0, 0.0, 0.0, 255.0], 1.0), "{got:?}");
    // colour add: black takes the colour (R 64, G 128, B 255), white stays white
    let add = json!({ "color_add": 0x00FF_8040u32 });
    let got = one(&r, add.clone(), [0, 0, 0, 255]);
    assert!(near(got, [64.0, 128.0, 255.0, 255.0], 1.0), "{got:?}");
    let got = one(&r, add, [255, 255, 255, 255]);
    assert!(near(got, [255.0, 255.0, 255.0, 255.0], 1.0), "{got:?}");
}

#[test]
fn the_obs_matrix_has_the_documented_shape() {
    // defaults: identity matrix, exponent 1
    let c = color::matrix(&Params::new());
    assert_eq!(c.gamma, 1.0);
    for (i, row) in c.matrix.iter().enumerate() {
        for (j, v) in row.iter().enumerate() {
            assert!((v - if i == j { 1.0 } else { 0.0 }).abs() < 1e-6, "[{i}][{j}] = {v}");
        }
    }
    // saturation -1: every column is a luma weight
    let p = json!({ "saturation": -1 });
    let c = color::matrix(p.as_object().unwrap());
    for (i, w) in [0.299f32, 0.587, 0.114].iter().enumerate() {
        for j in 0..3 {
            assert!((c.matrix[i][j] - w).abs() < 1e-6);
        }
    }
    // opacity sits in the last diagonal entry; the colour add is the translation row of the last factor, so the
    // opacity row before it scales it too (the add is applied to the new alpha)
    let p = json!({ "opacity": 0.25, "color_add": 0x00FF_FFFFu32 });
    let c = color::matrix(p.as_object().unwrap());
    assert_eq!(c.matrix[3][3], 0.25);
    assert_eq!(&c.matrix[3][..3], &[0.25, 0.25, 0.25]);
    let p = json!({ "color_add": 0x00FF_FFFFu32 });
    let c = color::matrix(p.as_object().unwrap());
    assert_eq!(&c.matrix[3][..3], &[1.0, 1.0, 1.0]);
    let p = json!({ "opacity": 0.25, "color_add": 0x00FF_FFFFu32 });
    let c = color::matrix(p.as_object().unwrap());
    // the packed constants are the matrix rows, the exponent and padding
    let bytes = color::pack(p.as_object().unwrap());
    assert_eq!(bytes.len(), 80);
    assert_eq!(f32::from_le_bytes(bytes[60..64].try_into().unwrap()), c.matrix[3][3]);
    assert_eq!(f32::from_le_bytes(bytes[64..68].try_into().unwrap()), c.gamma);
}

// ---------------------------------------------------------------------------------------------------------------
// The readers

#[test]
fn readers_clamp_and_fall_back_to_the_default() {
    let p = json!({
        "num": 7.5, "huge": 1e30, "low": -1e30, "text": "3", "nothing": null, "flag": true, "list": [1],
        "nan": f64::NAN, "inf": f64::INFINITY, "int": 3.6,
        "red": 0x0000_00FFu32, "neg": -1, "big": 1e12
    });
    let p = p.as_object().unwrap();
    assert_eq!(pf(p, "num", 0.0, -10.0, 10.0), 7.5);
    assert_eq!(pf(p, "num", 0.0, -1.0, 1.0), 1.0);
    assert_eq!(pf(p, "huge", 0.0, -2.0, 3.0), 3.0);
    assert_eq!(pf(p, "low", 0.0, -2.0, 3.0), -2.0);
    for key in ["text", "nothing", "flag", "list", "nan", "inf", "missing"] {
        assert_eq!(pf(p, key, 0.25, -1.0, 1.0), 0.25, "{key}");
        assert_eq!(pi(p, key, 4, 0, 10), 4, "{key}");
    }
    assert_eq!(pi(p, "int", 0, 0, 10), 4);
    assert_eq!(pi(p, "huge", 0, -5, 5), 5);
    assert!(pb(p, "flag", false));
    assert!(pb(p, "num", true), "a number is not a boolean");
    assert_eq!(ps(p, "text", "d"), "3");
    assert_eq!(ps(p, "num", "d"), "d");
    // 0xAABBGGRR: red is the low byte
    assert_eq!(pc(p, "red", 0), [1.0, 0.0, 0.0, 0.0]);
    assert_eq!(pc(p, "neg", 0), [1.0; 4], "-1 is 0xFFFFFFFF");
    assert_eq!(pc(p, "big", 0x00FF_0000), [0.0, 0.0, 1.0, 0.0]);
    assert_eq!(pc(p, "text", 0xFF00_0000), [0.0, 0.0, 0.0, 1.0]);
}

#[test]
fn the_filter_error_event_payload_is_id_and_message() {
    let e = FilterError {
        id: "c1".into(),
        message: "boom".into(),
    };
    assert_eq!(serde_json::to_value(&e).unwrap(), json!({ "id": "c1", "message": "boom" }));
}

#[test]
fn a_chain_parses_from_json() {
    let ok = parse_chain(&json!([
        { "id": "a", "kind": "color_filter", "enabled": false, "params": { "gamma": 1 } },
        { "id": "b", "kind": "whatever" }
    ]))
    .unwrap();
    assert_eq!(ok.len(), 2);
    assert!(!ok[0].enabled && ok[1].enabled, "enabled defaults to true");
    assert!(ok[1].params.is_empty());
    assert!(parse_chain(&Value::Null).unwrap().is_empty());
    let err = parse_chain(&json!([{ "id": "a", "kind": "x" }, { "kind": "y" }])).unwrap_err();
    assert!(err.contains("entry 1"), "{err}");
    assert!(parse_chain(&json!({ "id": "a" })).is_err());
}

// ---------------------------------------------------------------------------------------------------------------
// Chain behaviour

/// A test filter that copies its input, or fails when its `fail` param is true.
struct Flaky {
    fail: bool,
}

impl GpuFilter for Flaky {
    fn update(&mut self, _r: &Renderer, p: &Params) {
        self.fail = pb(p, "fail", false);
    }

    fn render(&mut self, r: &Renderer, input: &RgbaTexture, output: &RgbaTexture) -> GpuResult<()> {
        if self.fail {
            return Err(GpuError::Invalid("flaky filter failed".into()));
        }
        r.copy(input, output)
    }
}

/// A test filter that halves the size.
struct Half;

impl GpuFilter for Half {
    fn update(&mut self, _r: &Renderer, _p: &Params) {}

    fn output_size(&self, input: (u32, u32)) -> (u32, u32) {
        (input.0 / 2, input.1 / 2)
    }

    fn render(&mut self, r: &Renderer, input: &RgbaTexture, output: &RgbaTexture) -> GpuResult<()> {
        let ps = r.shaders.pixel_source(
            "test_half",
            "Texture2D<float4> t : register(t0);\nfloat4 main(float4 p : SV_Position) : SV_Target { return t.Load(int3(int2(p.xy) * 2, 0)); }",
        )?;
        r.draw(&crate::gpu::Pass {
            ps: &ps,
            srvs: &[Some(input.srv.clone())],
            constants: None,
            target: &output.rtv,
            width: output.width,
            height: output.height,
        });
        Ok(())
    }
}

fn test_kinds(kind: &str) -> Option<Box<dyn GpuFilter>> {
    match kind {
        "test_flaky" => Some(Box::new(Flaky { fail: false })),
        "test_half" => Some(Box::new(Half)),
        other => make(other),
    }
}

fn test_chain() -> Chain {
    Chain::with_factory(test_kinds)
}

#[test]
fn apply_keeps_recreates_drops_skips_and_orders() {
    let r = warp();
    let mut chain = test_chain();
    chain.apply(
        &r,
        &[
            cfg("a", "color_filter", true, json!({})),
            cfg("b", "test_flaky", true, json!({})),
            cfg("x", "no_such_filter", true, json!({})),
            cfg("d", "color_filter", false, json!({})),
        ],
    );
    let first = chain.instances();
    let ids: Vec<&str> = first.iter().map(|i| i.0.as_str()).collect();
    assert_eq!(ids, ["a", "b", "d"], "an unknown kind is skipped, the order is kept");
    assert!(!first[2].2, "a disabled filter stays in the chain, switched off");
    assert_eq!(chain.len(), 3);

    // same ids and kinds, new params and a new order: the instances stay
    chain.apply(
        &r,
        &[
            cfg("d", "color_filter", true, json!({ "gamma": 1 })),
            cfg("a", "color_filter", true, json!({ "gamma": 2 })),
            cfg("b", "test_flaky", false, json!({})),
        ],
    );
    let second = chain.instances();
    let serial_of = |v: &[(String, String, bool, u64)], id: &str| v.iter().find(|i| i.0 == id).unwrap().3;
    assert_eq!(
        second.iter().map(|i| i.0.as_str()).collect::<Vec<_>>(),
        ["d", "a", "b"]
    );
    for id in ["a", "b", "d"] {
        assert_eq!(serial_of(&first, id), serial_of(&second, id), "{id} was kept");
    }
    assert!(second[0].2 && second[1].2 && !second[2].2);

    // the kind changes under the same id: a new instance; a removed id is dropped; a new id appears
    chain.apply(
        &r,
        &[
            cfg("a", "test_half", true, json!({})),
            cfg("d", "color_filter", true, json!({})),
            cfg("n", "color_filter", true, json!({})),
        ],
    );
    let third = chain.instances();
    assert_eq!(
        third.iter().map(|i| (i.0.as_str(), i.1.as_str())).collect::<Vec<_>>(),
        [("a", "test_half"), ("d", "color_filter"), ("n", "color_filter")]
    );
    assert_ne!(serial_of(&third, "a"), serial_of(&first, "a"), "a changed kind is recreated");
    assert_eq!(serial_of(&third, "d"), serial_of(&first, "d"));
    assert!(third.iter().all(|i| i.0 != "b"), "b was removed");
    assert!(serial_of(&third, "n") > serial_of(&first, "d"), "n is new");

    // an empty configuration empties the chain
    chain.apply(&r, &[]);
    assert!(chain.is_empty());
}

#[test]
fn a_disabled_filter_is_skipped_and_a_changed_size_flows_on() {
    let r = warp();
    let (w, h, px) = picture();
    let mut chain = test_chain();
    chain.apply(
        &r,
        &[
            cfg("s", "color_filter", false, json!({ "saturation": -1 })),
            cfg("half", "test_half", true, json!({})),
        ],
    );
    let tex = r.texture(w, h).unwrap();
    fill_rgba(&r, &tex, |x, y| px[(y * w + x) as usize]);
    let out = chain.run(&r, tex).unwrap();
    assert_eq!((out.width, out.height), (w / 2, h / 2));
    let got = read_rgba(&r, &out);
    // the disabled saturation filter did nothing: the half-size picture is every second pixel of the input
    for y in 0..h / 2 {
        for x in 0..w / 2 {
            let o = ((y * (w / 2) + x) * 4) as usize;
            assert_eq!(&got[o..o + 4], &px[(y * 2 * w + x * 2) as usize]);
        }
    }
}

#[test]
fn a_filter_that_fails_is_bypassed_and_reported_once() {
    let r = warp();
    let (w, h, px) = picture();
    let mut chain = test_chain();
    let failing = |fail: bool| {
        vec![
            cfg("flaky", "test_flaky", true, json!({ "fail": fail })),
            cfg("gray", "color_filter", true, json!({ "saturation": -1 })),
        ]
    };
    chain.apply(&r, &failing(true));
    let grayed = |out: &[Pixel]| out.iter().all(|p| p[0].abs_diff(p[1]) <= 1 && p[1].abs_diff(p[2]) <= 1);
    for frame in 0..5 {
        let out = run(&r, &mut chain, w, h, &px);
        assert!(grayed(&out), "frame {frame} still comes out, through the filters after the failing one");
        if frame == 0 {
            let errors = chain.take_errors();
            assert_eq!(errors.len(), 1, "{errors:?}");
            assert_eq!(errors[0].id, "flaky");
            assert!(errors[0].message.contains("flaky filter failed"));
        }
    }
    assert!(chain.take_errors().is_empty(), "frames 1 to 4 add no report");

    // recovered: no report, and the filter renders again
    chain.apply(&r, &failing(false));
    run(&r, &mut chain, w, h, &px);
    assert!(chain.take_errors().is_empty());
    // failing anew: reported again, once
    chain.apply(&r, &failing(true));
    for _ in 0..3 {
        run(&r, &mut chain, w, h, &px);
    }
    let errors = chain.take_errors();
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0].id, "flaky");
}

#[test]
fn an_unknown_kind_and_garbage_params_do_not_panic() {
    let r = warp();
    let (w, h, px) = picture();

    // unknown kind: skipped, the picture passes through untouched
    let mut chain = Chain::new();
    chain.apply(&r, &[cfg("x", "does_not_exist", true, json!({ "gamma": 3 }))]);
    assert!(chain.is_empty());
    let out = run(&r, &mut chain, w, h, &px);
    assert_eq!(out, px);

    // garbage: wrong types are ignored, numbers are clamped. contrast 1e30 -> 4, opacity -5 -> 0, the rest default.
    let garbage = json!({
        "gamma": "x", "contrast": 1e30, "brightness": null, "saturation": [1], "hue_shift": true,
        "opacity": -5, "color_multiply": "red", "color_add": 1e12, "unknown": 5, "nan": f64::NAN
    });
    let clamped = json!({ "contrast": 4, "opacity": 0 });
    let a = run_color(&r, garbage, w, h, &px);
    let b = run_color(&r, clamped, w, h, &px);
    assert_eq!(a, b);
    assert!(a.iter().all(|p| p[3] == 0));
    // out-of-range sliders match the range's edge
    let a = run_color(&r, json!({ "saturation": 99, "hue_shift": -999, "gamma": 50 }), w, h, &px);
    let b = run_color(&r, json!({ "saturation": 5, "hue_shift": -180, "gamma": 3 }), w, h, &px);
    assert_eq!(a, b);
    // params that are not an object at all
    let mut chain = Chain::new();
    chain.apply(&r, &[cfg("c", "color_filter", true, Value::Null)]);
    assert_eq!(chain.len(), 1);
}

#[test]
fn warm_up_compiles_every_registered_filter() {
    let r = warp();
    warm_up(&r).expect("every registered shader compiles");
    // a second pass hits the cache
    warm_up(&r).unwrap();
}

#[test]
fn defaults_json_covers_every_registered_kind_and_is_accepted() {
    let r = warp();
    let defaults: Value = serde_json::from_str(DEFAULTS_JSON).expect("defaults.json is JSON");
    let defaults = defaults.as_object().expect("an object of kinds");
    let (w, h, px) = picture();
    let mut registered = 0;
    for kind in kinds() {
        registered += 1;
        let params = defaults
            .get(kind)
            .unwrap_or_else(|| panic!("defaults.json has no entry for {kind}"))
            .as_object()
            .unwrap_or_else(|| panic!("the entry for {kind} is not an object"));
        assert!(make(kind).is_some(), "{kind}");
        // the filter accepts the params, and they are exactly what it uses when given none
        let with = |p: Params| {
            let mut chain = Chain::new();
            chain.apply(&r, &[cfg("k", kind, true, Value::Object(p))]);
            assert_eq!(chain.len(), 1, "{kind} was not created");
            run(&r, &mut chain, w, h, &px)
        };
        assert_eq!(with(params.clone()), with(Params::new()), "{kind}: defaults.json differs from the code's defaults");
    }
    assert!(registered >= 1);
    // Color Correction: the keys are the ones the port reads
    let mut keys: Vec<&str> = defaults["color_filter"].as_object().unwrap().keys().map(String::as_str).collect();
    let mut want = color::KEYS.to_vec();
    keys.sort_unstable();
    want.sort_unstable();
    assert_eq!(keys, want);
    assert!(make("not_a_kind").is_none());
}

#[test]
fn opacity_half_over_a_red_background_blends_at_the_output_stage() {
    use crate::gpu::output::{FitMode, OutputParams, OutputStage};

    let r = warp();
    let mut chain = Chain::new();
    chain.apply(&r, &color_cfg(json!({ "opacity": 0.5 })));
    let src = [[255, 255, 255, 255], [128, 128, 128, 255], [0, 0, 255, 255], [255, 0, 0, 255]];
    let tex = r.texture(4, 1).unwrap();
    fill_rgba(&r, &tex, |x, _| src[x as usize]);
    let filtered = chain.run(&r, tex).unwrap();
    let stage = OutputStage::new(r.clone()).unwrap();
    let out = stage
        .run(
            &filtered,
            &OutputParams {
                width: 4,
                height: 1,
                fit: FitMode::Fit,
                background: [1.0, 0.0, 0.0],
            },
        )
        .unwrap();
    let got = read_rgba(&r, &out);
    // The stored alpha is 0.5 rounded to a code (128); the blend is done in linear light, as OBS does.
    let a = 128.0 / 255.0;
    for (i, c) in src.iter().enumerate() {
        let bg = [1.0, 0.0, 0.0];
        let want: Vec<f64> = (0..3)
            .map(|k| enc(a * dec(c[k] as f64) + (1.0 - a) * dec(bg[k] * 255.0)))
            .collect();
        for k in 0..3 {
            assert!(
                (got[i * 4 + k] as f64 - want[k]).abs() <= 1.5,
                "pixel {i} channel {k}: {} vs {}",
                got[i * 4 + k],
                want[k]
            );
        }
        assert_eq!(got[i * 4 + 3], 255, "the output is opaque");
    }
    // white at half opacity over red: red stays, green and blue are half the light, code 188
    assert!((got[1] as i32 - 188).abs() <= 1 && (got[2] as i32 - 188).abs() <= 1 && got[0] == 255);
    // red over red is red
    assert_eq!(&got[12..15], &[255, 0, 0]);
}

// ---------------------------------------------------------------------------------------------------------------
// Shader filters with extra inputs

static MULTIPLY: ShaderFilterDef = ShaderFilterDef {
    kind: "test_multiply",
    hlsl: "Texture2D<float4> image : register(t0);\n\
           Texture2D<float4> extra : register(t1);\n\
           cbuffer Params : register(b0) { float4 gain; };\n\
           float4 main(float4 p : SV_Position) : SV_Target {\n\
             return float4(image.Load(int3(int2(p.xy), 0)).rgb * extra.Load(int3(0, 0, 0)).rgb * gain.x, 1.0); }",
    pack: |p| {
        [pf(p, "gain", 1.0, 0.0, 1.0), 0.0, 0.0, 0.0]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect()
    },
    extra: &[ExtraInput::Image],
};

#[test]
fn a_shader_filter_passes_through_until_its_extra_input_is_set() {
    let r = warp();
    let mut f = ShaderFilter::new(&MULTIPLY);
    let input = r.texture(2, 1).unwrap();
    fill_rgba(&r, &input, |x, _| if x == 0 { [200, 100, 50, 255] } else { [10, 20, 30, 255] });
    let out = r.texture(2, 1).unwrap();

    f.render(&r, &input, &out).unwrap();
    assert_eq!(read_rgba(&r, &out), read_rgba(&r, &input), "no extra yet: the input comes through");

    let half = r.texture(1, 1).unwrap();
    fill_rgba(&r, &half, |_, _| [128, 128, 128, 255]);
    let srv: ID3D11ShaderResourceView = half.srv.clone();
    f.set_extra(0, Some(srv));
    f.update(&r, json!({ "gain": 0.5 }).as_object().unwrap());
    f.render(&r, &input, &out).unwrap();
    let got = read_rgba(&r, &out);
    // 200 * (128/255) * 0.5 = 50.2; 100 * 0.502 * 0.5 = 25.1
    assert!((got[0] as i32 - 50).abs() <= 1 && (got[1] as i32 - 25).abs() <= 1, "{got:?}");
    f.set_extra(0, None);
    f.render(&r, &input, &out).unwrap();
    assert_eq!(read_rgba(&r, &out), read_rgba(&r, &input));
}

#[test]
fn a_shader_that_does_not_compile_fails_the_render_not_the_process() {
    static BROKEN: ShaderFilterDef = ShaderFilterDef {
        kind: "test_broken",
        hlsl: "float4 main() : SV_Target { return nope; }",
        pack: |_| vec![0; 16],
        extra: &[],
    };
    let r = warp();
    let mut f = ShaderFilter::new(&BROKEN);
    let (input, out) = (r.texture(4, 4).unwrap(), r.texture(4, 4).unwrap());
    let err = f.render(&r, &input, &out).unwrap_err();
    assert!(err.to_string().contains("nope"), "{err}");
}
