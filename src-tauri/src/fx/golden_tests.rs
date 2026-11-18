//! Golden outputs of the filters and the output stage, taken from the shaders that did their own sRGB conversion (every
//! texel decoded and encoded by `pow` in the shader, before the pool textures got sRGB views).
//!
//! `golden/srgb_views.bin` holds, for each case, the pixels the old shaders made on WARP from the picture below. The
//! new shaders let the hardware convert on sample and on write; the test renders the same cases again and compares
//! within a code value or two (the hardware conversion and `pow` round differently in the last bit). The cases cover
//! every filter that works without an asset file or the NVIDIA runtime; the asset filters (LUT, masks) are covered by
//! their own tests against a CPU reference, the NVIDIA passes by `nvidia::filter::tests`.
//!
//! To write the file again (only after the intended look of a filter changed):
//! `$env:BLESS_GOLDEN=1; cargo test -p better-cam --lib golden -- --nocapture`.

use std::sync::Arc;

use serde_json::{json, Value};

use super::*;
use crate::gpu::output::{EncodeFormat, Encoder, FitMode, OutputParams, OutputStage};
use crate::gpu::pipeline_tests::{fill_rgba, read_rgba, read_texture, warp};

const GOLDEN: &[u8] = include_bytes!("golden/srgb_views.bin");
const GOLDEN_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src/fx/golden/srgb_views.bin");

/// One case: a name, the size of the result and its bytes (RGBA, or the planes of an encode one after the other).
struct Case {
    name: String,
    w: u32,
    h: u32,
    data: Vec<u8>,
}

/// The input picture: gradients with noise, a green patch for the keys, a gray strip, and alpha that is opaque in the
/// top half and ramps from clear to opaque in the bottom half (with a few fully clear pixels).
fn picture(w: u32, h: u32) -> Vec<[u8; 4]> {
    let mut seed = 0x2468_ace1u32;
    let mut noise = move || {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (seed >> 24) as i32
    };
    let mut px = Vec::new();
    for y in 0..h {
        for x in 0..w {
            let n = noise() % 24 - 12;
            let mut p = [
                ((x * 255 / (w - 1)) as i32 + n).clamp(0, 255) as u8,
                ((y * 255 / (h - 1)) as i32 + n / 2).clamp(0, 255) as u8,
                (((x * 7 + y * 13) % 256) as i32 + n).clamp(0, 255) as u8,
                255,
            ];
            if (4..16).contains(&x) && (4..10).contains(&y) {
                p = [20, 200, 40, 255];
            }
            if (20..30).contains(&x) && (4..10).contains(&y) {
                let v = ((x - 20) * 28) as u8;
                p = [v, v, v, 255];
            }
            if y >= h / 2 {
                let a = (x * 255 / (w - 1)) as u8;
                p[3] = if (x + y) % 11 == 0 { 0 } else { a };
            }
            px.push(p);
        }
    }
    px
}

fn cfg(kind: &str, params: Value) -> Vec<FilterCfg> {
    vec![FilterCfg {
        id: "f".into(),
        kind: kind.into(),
        enabled: true,
        params: params.as_object().cloned().unwrap_or_default(),
    }]
}

fn upload(r: &Arc<Renderer>, w: u32, h: u32) -> Pooled {
    let px = picture(w, h);
    let tex = r.texture(w, h).unwrap();
    fill_rgba(r, &tex, |x, y| px[(y * w + x) as usize]);
    tex
}

fn filter_case(r: &Arc<Renderer>, name: &str, kind: &str, params: Value) -> Case {
    let (w, h) = (48, 32);
    let mut chain = Chain::new();
    chain.set_canvas((w, h));
    chain.apply(r, &cfg(kind, params));
    let out = chain.run(r, upload(r, w, h)).unwrap();
    assert!(chain.take_errors().is_empty(), "{name} failed");
    Case {
        name: name.into(),
        w: out.width,
        h: out.height,
        data: read_rgba(r, &out),
    }
}

fn cases(r: &Arc<Renderer>) -> Vec<Case> {
    let mut out = vec![
        filter_case(r, "color/default", "color_filter", json!({})),
        filter_case(r, "color/gamma", "color_filter", json!({"gamma": 0.4})),
        filter_case(
            r,
            "color/all",
            "color_filter",
            json!({
                "gamma": -0.3, "contrast": 0.25, "brightness": 0.04, "saturation": 0.4, "hue_shift": 35,
                "opacity": 0.8, "color_multiply": 0xf0e0d0u32, "color_add": 0x101820u32
            }),
        ),
        filter_case(r, "sharpen/0.08", "sharpness_filter", json!({})),
        filter_case(r, "sharpen/0.9", "sharpness_filter", json!({"sharpness": 0.9})),
        filter_case(r, "chroma/default", "chroma_key_filter", json!({})),
        filter_case(
            r,
            "chroma/tuned",
            "chroma_key_filter",
            json!({"similarity": 300, "smoothness": 150, "spill": 200, "opacity": 0.9, "contrast": 0.2,
                   "brightness": 0.05, "gamma": 0.2}),
        ),
        filter_case(r, "color_key/default", "color_key_filter", json!({})),
        filter_case(
            r,
            "color_key/tuned",
            "color_key_filter",
            json!({"key_color_type": "red", "similarity": 400, "smoothness": 200, "opacity": 0.9,
                   "contrast": 0.3, "brightness": -0.05, "gamma": 0.25}),
        ),
        filter_case(r, "luma_key/default", "luma_key_filter", json!({})),
        filter_case(
            r,
            "luma_key/smooth",
            "luma_key_filter",
            json!({"luma_max": 0.8, "luma_min": 0.15, "luma_max_smooth": 0.1, "luma_min_smooth": 0.05}),
        ),
        filter_case(r, "crop/shift", "crop_filter", json!({"left": 5, "top": 3, "right": 2, "bottom": 6})),
        filter_case(r, "crop/pad", "crop_filter", json!({"left": -4, "top": -2, "right": -3, "bottom": -5})),
        filter_case(r, "transform/flip", "transform", json!({"flip_h": true})),
        filter_case(r, "transform/90", "transform", json!({"rotation": 90})),
        filter_case(r, "transform/30", "transform", json!({"rotation": 30})),
        filter_case(r, "transform/-17flip", "transform", json!({"rotation": -17, "flip_v": true})),
    ];
    for (sampling, target) in [
        ("point", "80x56"),
        ("bilinear", "80x56"),
        ("bilinear", "30x20"),
        ("bicubic", "80x56"),
        ("bicubic", "40x28"),
        ("lanczos", "80x56"),
        ("lanczos", "40x28"),
        ("area", "80x56"),
        ("area", "40x28"),
        ("area", "30x20"),
    ] {
        out.push(filter_case(
            r,
            &format!("scale/{sampling}/{target}"),
            "scale_filter",
            json!({"sampling": sampling, "resolution": target}),
        ));
    }
    out.push(filter_case(
        r,
        "scale/bicubic/undistort",
        "scale_filter",
        json!({"sampling": "bicubic", "resolution": "16:9", "undistort": true}),
    ));
    out.push(filter_case(
        r,
        "scale/lanczos/undistort",
        "scale_filter",
        json!({"sampling": "lanczos", "resolution": "4:3", "undistort": true}),
    ));

    // The output stage: same size (texels copied), a stretch (bilinear) and a fit with bars, over a coloured background.
    let stage = OutputStage::new(r.clone()).unwrap();
    for (name, size, fit) in [
        ("output/same", (48, 32), FitMode::Stretch),
        ("output/stretch", (72, 40), FitMode::Stretch),
        ("output/fit", (64, 64), FitMode::Fit),
        ("output/fill", (40, 40), FitMode::Fill),
    ] {
        let t = stage
            .run(
                &upload(r, 48, 32),
                &OutputParams { width: size.0, height: size.1, fit, background: [0.2, 0.55, 0.8] },
            )
            .unwrap();
        out.push(Case { name: name.into(), w: t.width, h: t.height, data: read_rgba(r, &t) });
    }

    // The encoders: the planes of NV12 one after the other, and BGRX.
    let mut enc = Encoder::new(r.clone()).unwrap();
    let src = upload(r, 48, 32);
    let t = enc.encode(&src, EncodeFormat::Nv12).unwrap();
    let mut data = Vec::new();
    for p in &t.planes {
        data.extend(read_texture(r, &p.texture, p.desc.width, p.desc.height, p.desc.bytes_per_texel));
    }
    out.push(Case { name: "encode/nv12".into(), w: 48, h: 32, data });
    let t = enc.encode(&src, EncodeFormat::Bgra).unwrap();
    let p = &t.planes[0];
    out.push(Case {
        name: "encode/bgra".into(),
        w: 48,
        h: 32,
        data: read_texture(r, &p.texture, p.desc.width, p.desc.height, p.desc.bytes_per_texel),
    });
    out
}

fn serialize(cases: &[Case]) -> Vec<u8> {
    let mut v = Vec::new();
    for c in cases {
        v.push(c.name.len() as u8);
        v.extend_from_slice(c.name.as_bytes());
        v.extend_from_slice(&c.w.to_le_bytes());
        v.extend_from_slice(&c.h.to_le_bytes());
        v.extend_from_slice(&(c.data.len() as u32).to_le_bytes());
        v.extend_from_slice(&c.data);
    }
    v
}

fn parse(mut b: &[u8]) -> Vec<Case> {
    let mut out = Vec::new();
    let take = |b: &mut &[u8], n: usize| -> Vec<u8> {
        let (head, tail) = b.split_at(n);
        *b = tail;
        head.to_vec()
    };
    let u32_of = |v: Vec<u8>| u32::from_le_bytes(v.try_into().unwrap());
    while !b.is_empty() {
        let len = take(&mut b, 1)[0] as usize;
        let name = String::from_utf8(take(&mut b, len)).unwrap();
        let w = u32_of(take(&mut b, 4));
        let h = u32_of(take(&mut b, 4));
        let n = u32_of(take(&mut b, 4)) as usize;
        out.push(Case { name, w, h, data: take(&mut b, n) });
    }
    out
}

/// How far a case may be from the old output, in code values. A pixel that sits on a rounding edge, a key edge or a
/// large local gain can move by more than one code in a few pixels; the rest must be within `tolerance`, and only
/// `few` pixels (per thousand) may exceed it up to `limit`.
fn allowance(name: &str) -> (i32, usize, i32) {
    match name.split('/').next().unwrap() {
        // a sharp edge of the key, a steep curve: a handful of pixels move more
        "chroma" | "color_key" | "luma_key" => (2, 20, 6),
        "scale" | "transform" => (2, 10, 4),
        _ => (1, 5, 3),
    }
}

/// `hardware`: the hardware samples sRGB with its own precision (NVIDIA hands over about 7 bits of mantissa, which a
/// strong sharpen multiplies by 8), and the colour of a nearly clear pixel is numerically meaningless (a division by an
/// alpha near 0), so the colour of pixels whose alpha is below 64 is not compared and the limits are wide: this is a
/// check that nothing is wrong, the numbers it prints are the measurement.
fn compare(label: &str, got: &[Case], want: &[Case], hardware: bool) {
    assert_eq!(got.len(), want.len(), "{label}: the number of cases");
    let mut report = String::new();
    let mut failed = Vec::new();
    for (g, w) in got.iter().zip(want) {
        assert_eq!((&g.name, g.w, g.h, g.data.len()), (&w.name, w.w, w.h, w.data.len()), "{label}: case layout");
        let (mut tol, mut few_per_mille, mut limit) = allowance(&g.name);
        if hardware {
            (tol, few_per_mille, limit) = (tol.max(2), few_per_mille.max(100), limit.max(16));
        }
        let rgba = !g.name.starts_with("encode/");
        let diffs: Vec<i32> = g
            .data
            .iter()
            .zip(&w.data)
            .enumerate()
            .filter(|(i, _)| !(hardware && rgba && i % 4 != 3 && w.data[i / 4 * 4 + 3] < 64))
            .map(|(_, (a, b))| (*a as i32 - *b as i32).abs())
            .collect();
        let worst = diffs.iter().copied().max().unwrap_or(0);
        let over = diffs.iter().filter(|d| **d > tol).count();
        let off = diffs.iter().filter(|d| **d > 0).count();
        report.push_str(&format!(
            "{label:>9} {:<26} worst {worst}  differing {off:>5}/{}  over {tol}: {over}\n",
            g.name,
            diffs.len()
        ));
        if worst > limit || over * 1000 > few_per_mille * diffs.len() {
            failed.push(g.name.clone());
        }
    }
    eprintln!("{report}");
    assert!(failed.is_empty(), "{label}: these cases moved more than allowed: {failed:?}");
}

#[test]
fn the_filters_match_the_outputs_of_the_shaders_that_converted_srgb_themselves() {
    let r = warp();
    let got = cases(&r);
    if std::env::var_os("BLESS_GOLDEN").is_some() {
        std::fs::write(GOLDEN_PATH, serialize(&got)).unwrap();
        eprintln!("wrote {} cases to {GOLDEN_PATH}", got.len());
        return;
    }
    compare("WARP", &got, &parse(GOLDEN), false);
}

#[test]
fn the_filters_match_the_old_outputs_on_the_hardware_adapter() {
    use crate::gpu::{AdapterKind, GpuDevice};
    let Ok(gpu) = GpuDevice::new(AdapterKind::Hardware) else {
        eprintln!("no hardware adapter here");
        return;
    };
    let r = Arc::new(Renderer::new(Arc::new(gpu)).unwrap());
    // The hardware rounds its own way (and pow differs between vendors), so the cases may move a code more than on
    // the software rasterizer, whose output the file holds.
    eprintln!("hardware adapter: {}", r.gpu.adapter_name);
    compare("hardware", &cases(&r), &parse(GOLDEN), true);
}
