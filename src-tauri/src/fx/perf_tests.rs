//! Not checks: GPU time per filter and per stage, measured with timestamp queries (the method of `docs/PERF.md`: the
//! GPU time of the commands between `GpuTimer::begin` and `end`, median of the frames).
//!
//! `cargo test --release -p better-cam --lib filter_gpu_times -- --ignored --nocapture` (one process, the hardware
//! adapter and WARP; set `PERF_SIZES=1080,2160` to choose the heights, `PERF_ADAPTER=hardware|warp`). Hold the GPU lock
//! (`app-test.lock`) while it runs on the hardware adapter.

use std::sync::Arc;

use serde_json::{json, Value};

use super::*;
use crate::capture::PixelFormat;
use crate::gpu::convert::{Converter, SourceFrame};
use crate::gpu::output::{EncodeFormat, Encoder, FitMode, OutputParams, OutputStage};
use crate::gpu::pipeline_tests::fill_rgba;
use crate::gpu::{AdapterKind, GpuDevice, GpuTimer};

/// Median GPU time of `frames` runs of `run`, in milliseconds.
fn median_gpu_ms(r: &Renderer, timer: &mut GpuTimer, frames: usize, mut run: impl FnMut()) -> f64 {
    let mut times = Vec::new();
    for n in 0..frames + 10 {
        assert!(timer.begin(&r.gpu.context));
        run();
        timer.end(&r.gpu.context);
        r.flush();
        let mut ms = None;
        for _ in 0..2_000_000 {
            ms = timer.poll(&r.gpu.context);
            if ms.is_some() {
                break;
            }
            std::thread::yield_now();
        }
        if n >= 10 {
            times.push(ms.expect("the timer answered"));
        }
    }
    times.sort_by(|a, b| a.total_cmp(b));
    times[times.len() / 2]
}

fn chain_of(r: &Arc<Renderer>, size: (u32, u32), filters: &[(&str, Value)]) -> Chain {
    let mut chain = Chain::new();
    chain.set_canvas(size);
    let cfg: Vec<FilterCfg> = filters
        .iter()
        .enumerate()
        .map(|(i, (kind, params))| FilterCfg {
            id: format!("f{i}"),
            kind: (*kind).into(),
            enabled: true,
            params: params.as_object().cloned().unwrap_or_default(),
        })
        .collect();
    chain.apply(r, &cfg);
    chain
}

#[test]
#[ignore = "measurement"]
fn filter_gpu_times() {
    let heights: Vec<u32> = std::env::var("PERF_SIZES")
        .ok()
        .map(|s| s.split(',').filter_map(|v| v.trim().parse().ok()).collect())
        .unwrap_or_else(|| vec![1080, 2160]);
    let which = std::env::var("PERF_ADAPTER").unwrap_or_default();
    for kind in [AdapterKind::Hardware, AdapterKind::Warp] {
        if (which == "hardware" && kind == AdapterKind::Warp) || (which == "warp" && kind == AdapterKind::Hardware) {
            continue;
        }
        let Ok(gpu) = GpuDevice::new(kind) else {
            eprintln!("PERF no {kind:?} adapter");
            continue;
        };
        let r = Arc::new(Renderer::new(Arc::new(gpu)).unwrap());
        crate::fx::warm_up(&r).unwrap();
        r.shaders.warm_up().unwrap();
        let adapter = r.gpu.adapter_name.clone();
        let mut timer = GpuTimer::new(&r.gpu.device).unwrap();
        let frames = if kind == AdapterKind::Warp { 5 } else { 100 };
        for height in &heights {
            let (w, h) = (height * 16 / 9, *height);
            // A picture with texture and a translucent corner, so the shaders do not run on a constant.
            let tex = r.texture(w, h).unwrap();
            fill_rgba(&r, &tex, |x, y| {
                [(x / 8) as u8 ^ (y / 4) as u8, (x / 7 + y / 9) as u8, ((x * y) >> 6) as u8, if x < w / 8 && y < h / 8 { 180 } else { 255 }]
            });
            r.flush();
            let half = format!("{}x{}", w / 2, h / 2);
            let two_thirds = format!("{}x{}", w * 2 / 3, h * 2 / 3);
            let cases: Vec<(&str, Vec<(&str, Value)>)> = vec![
                ("copy (empty chain)", vec![]),
                ("color_filter", vec![("color_filter", json!({"gamma": 0.1, "contrast": 0.15, "brightness": 0.03, "saturation": 0.25, "hue_shift": 10}))]),
                ("sharpness_filter 0.35", vec![("sharpness_filter", json!({"sharpness": 0.35}))]),
                ("chroma_key_filter", vec![("chroma_key_filter", json!({}))]),
                ("color_key_filter", vec![("color_key_filter", json!({}))]),
                ("luma_key_filter", vec![("luma_key_filter", json!({"luma_max": 0.8, "luma_min": 0.1, "luma_max_smooth": 0.1}))]),
                ("scale point (2/3)", vec![("scale_filter", json!({"sampling": "point", "resolution": two_thirds}))]),
                ("scale bilinear (2/3)", vec![("scale_filter", json!({"sampling": "bilinear", "resolution": two_thirds}))]),
                ("scale bicubic (2/3)", vec![("scale_filter", json!({"sampling": "bicubic", "resolution": two_thirds}))]),
                ("scale lanczos (2/3)", vec![("scale_filter", json!({"sampling": "lanczos", "resolution": two_thirds}))]),
                ("scale area (2/3)", vec![("scale_filter", json!({"sampling": "area", "resolution": two_thirds}))]),
                ("scale bilinear low-res (1/2 and less)", vec![("scale_filter", json!({"sampling": "bilinear", "resolution": half}))]),
                ("transform 30 deg", vec![("transform", json!({"rotation": 30}))]),
                ("transform flip_h", vec![("transform", json!({"flip_h": true}))]),
                ("crop", vec![("crop_filter", json!({"left": 8, "top": 8, "right": 8, "bottom": 8}))]),
                ("auto_adjust (exposure + wb)", vec![("auto_adjust", json!({}))]),
                (
                    "four filters (color, sharpen, scale bicubic 2/3, chroma key)",
                    vec![
                        ("color_filter", json!({"gamma": 0.1, "contrast": 0.15, "brightness": 0.03, "saturation": 0.25, "hue_shift": 10})),
                        ("sharpness_filter", json!({"sharpness": 0.35})),
                        ("scale_filter", json!({"sampling": "bicubic", "resolution": two_thirds})),
                        ("chroma_key_filter", json!({})),
                    ],
                ),
            ];
            for (label, filters) in &cases {
                let mut chain = chain_of(&r, (w, h), filters);
                if label.starts_with("auto_adjust") {
                    // the loop needs a few frames to move its gains off 1 (a neutral filter only copies)
                    for _ in 0..40 {
                        let t = r.texture(w, h).unwrap();
                        r.copy(&tex, &t).unwrap();
                        let _ = chain.run(&r, t).unwrap();
                        r.flush();
                        std::thread::sleep(std::time::Duration::from_millis(3));
                    }
                }
                let ms = median_gpu_ms(&r, &mut timer, frames, || {
                    let t = r.texture(w, h).unwrap();
                    r.copy(&tex, &t).unwrap();
                    let _ = chain.run(&r, t).unwrap();
                });
                let baseline = median_gpu_ms(&r, &mut timer, frames, || {
                    let t = r.texture(w, h).unwrap();
                    r.copy(&tex, &t).unwrap();
                });
                eprintln!("PERF | {adapter} | {w}x{h} | {label} | {:.3}", (ms - baseline).max(0.0));
            }

            // The stages around the chain.
            let stage = OutputStage::new(r.clone()).unwrap();
            let mut enc = Encoder::new(r.clone()).unwrap();
            let ms = median_gpu_ms(&r, &mut timer, frames, || {
                let out = stage
                    .run(&tex, &OutputParams { width: w, height: h, fit: FitMode::Fit, background: [0.0; 3] })
                    .unwrap();
                let _ = enc.encode(&out, EncodeFormat::Nv12).unwrap();
            });
            eprintln!("PERF | {adapter} | {w}x{h} | output stage (same size) + encode NV12 | {ms:.3}");
            let ms = median_gpu_ms(&r, &mut timer, frames, || {
                let _ = enc.encode(&tex, EncodeFormat::Nv12).unwrap();
            });
            eprintln!("PERF | {adapter} | {w}x{h} | encode NV12 alone | {ms:.3}");
            let ms = median_gpu_ms(&r, &mut timer, frames, || {
                let _ = enc.encode(&tex, EncodeFormat::Bgra).unwrap();
            });
            eprintln!("PERF | {adapter} | {w}x{h} | encode BGRX alone | {ms:.3}");

            // Convert + chain + output + encode, the whole frame (NV12 from the CPU, as a camera frame arrives).
            let mut conv = Converter::new(r.clone()).unwrap();
            let nv12 = vec![128u8; (w * h * 3 / 2) as usize];
            for (label, filters) in [
                ("convert NV12 + output + encode (no filters)", Vec::new()),
                (
                    "convert NV12 + four filters + output + encode",
                    vec![
                        ("color_filter", json!({"gamma": 0.1, "contrast": 0.15, "brightness": 0.03, "saturation": 0.25, "hue_shift": 10})),
                        ("sharpness_filter", json!({"sharpness": 0.35})),
                        ("scale_filter", json!({"sampling": "bicubic", "resolution": two_thirds})),
                        ("chroma_key_filter", json!({})),
                    ],
                ),
            ] {
                let mut chain = chain_of(&r, (w, h), &filters);
                let ms = median_gpu_ms(&r, &mut timer, frames, || {
                    let frame = SourceFrame::cpu(w, h, PixelFormat::Nv12, &nv12, w as i32);
                    let rgba = conv.convert(&frame, false).unwrap();
                    let filtered = chain.run(&r, rgba).unwrap();
                    let out = stage
                        .run(&filtered, &OutputParams { width: w, height: h, fit: FitMode::Fit, background: [0.0; 3] })
                        .unwrap();
                    let _ = enc.encode(&out, EncodeFormat::Nv12).unwrap();
                });
                eprintln!("PERF | {adapter} | {w}x{h} | {label} | {ms:.3}");
            }
        }
    }
}
