//! Tests of Auto Adjust on WARP, with an injected clock: the pure maths, the convergence behaviour of the loop and the
//! pixels the shader writes.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde_json::{json, Value};

use super::*;
use crate::fx::{Chain, FilterCfg};
use crate::gpu::pipeline_tests::{fill_rgba, read_rgba, warp};
use crate::gpu::{AdapterKind, GpuDevice, GpuTimer};

// ---------------------------------------------------------------------------------------------------------------
// Helpers

type Pixel = [u8; 4];

fn params(v: Value) -> Params {
    v.as_object().cloned().unwrap_or_default()
}

fn dec(code: u8) -> f32 {
    let u = f32::from(code) / 255.0;
    if u <= 0.04045 {
        u / 12.92
    } else {
        ((u + 0.055) / 1.055).powf(2.4)
    }
}

/// The brightness of a picture as the filter defines it for a flat picture: the sRGB-coded Rec. 709 luma of the mean
/// linear colour, averaged over the pixels' own brightness (the pictures of these tests are flat or symmetrical).
fn brightness(rgba: &[u8]) -> f32 {
    let n = rgba.len() / 4;
    rgba.chunks(4)
        .map(|p| srgb_encode(luma([dec(p[0]), dec(p[1]), dec(p[2])])))
        .sum::<f32>()
        / n as f32
}

fn flat(code: [u8; 3]) -> impl Fn(u32, u32) -> Pixel {
    move |_, _| [code[0], code[1], code[2], 255]
}

/// A filter on WARP with a clock the test moves.
struct Rig {
    r: Arc<Renderer>,
    f: AutoAdjust,
    clock: Arc<AtomicU64>,
    now: f64,
}

impl Rig {
    fn new(p: Value) -> Rig {
        let r = warp();
        let clock = Arc::new(AtomicU64::new(0f64.to_bits()));
        let c = clock.clone();
        let mut f =
            AutoAdjust::for_test(Box::new(move || f64::from_bits(c.load(Ordering::Relaxed))));
        f.update(&r, &params(p));
        Rig {
            r,
            f,
            clock,
            now: 100.0,
        }
    }

    fn set(&mut self, p: Value) {
        self.f.update(&self.r, &params(p));
    }

    /// One frame `dt` seconds after the previous one; the result as RGBA bytes.
    fn frame(&mut self, dt: f64, (w, h): (u32, u32), px: impl Fn(u32, u32) -> Pixel) -> Vec<u8> {
        self.now += dt;
        self.clock.store(self.now.to_bits(), Ordering::Relaxed);
        let input = self.r.texture(w, h).unwrap();
        fill_rgba(&self.r, &input, px);
        let output = self.r.texture(w, h).unwrap();
        self.f.render(&self.r, &input, &output).unwrap();
        read_rgba(&self.r, &output)
    }

    /// `n` frames at `fps` of a small flat picture; the brightness of the last result.
    fn run_flat(&mut self, n: usize, fps: f64, code: [u8; 3]) -> Vec<u8> {
        let mut out = Vec::new();
        for _ in 0..n {
            out = self.frame(1.0 / fps, (32, 18), flat(code));
        }
        out
    }
}

const SMALL: (u32, u32) = (32, 18);

// ---------------------------------------------------------------------------------------------------------------
// The pure maths

#[test]
fn auto_adjust_the_shoulder_is_the_identity_at_gain_one_and_never_exceeds_one() {
    for i in 0..=100 {
        let c = i as f32 / 100.0;
        assert_eq!(shape(c, 1.0), c);
        assert!(shape(c, 0.5) <= c);
    }
    for gain in [1.0001, 1.1, 1.5, 2.0, 4.0] {
        let mut previous = -1.0;
        for i in 0..=200 {
            let y = shape(i as f32 / 100.0, gain);
            assert!(y >= previous, "gain {gain}: the curve falls at {i}");
            assert!(y < 1.0 + 1e-6, "gain {gain}: {y}");
            previous = y;
        }
    }
    // Continuous in the gain around 1: a tiny boost changes nothing visible.
    for i in 0..=100 {
        let c = i as f32 / 100.0;
        assert!((shape(c, 1.001) - c).abs() < 0.002, "{c}");
    }
    assert_eq!(knee(1.0), 1.0);
    assert_eq!(knee(0.4), 1.0);
    assert!((knee(1.25) - 0.85).abs() < 1e-6);
    assert!((knee(4.0) - 0.7).abs() < 1e-6);
}

#[test]
fn auto_adjust_the_time_constant_follows_speed_in_seconds() {
    assert!((time_constant(0.3) - 0.5).abs() < 1e-6);
    assert!((time_constant(1.0) - 0.15).abs() < 1e-6);
    assert!((time_constant(0.05) - 3.0).abs() < 1e-6);
    // out of range values are limited
    assert_eq!(time_constant(0.0), time_constant(0.05));
    assert_eq!(time_constant(9.0), time_constant(1.0));
}

#[test]
fn auto_adjust_the_stage_sizes_end_at_the_grid() {
    assert_eq!(
        stage_sizes((1920, 1080)),
        vec![(240, 135), (30, 17), (16, 16)]
    );
    assert_eq!(
        stage_sizes((3840, 2160)),
        vec![(480, 270), (60, 34), (16, 16)]
    );
    assert_eq!(stage_sizes((640, 480)), vec![(80, 60), (16, 16)]);
    assert_eq!(stage_sizes((32, 18)), vec![(16, 16)]);
    // smaller than the grid: the stage enlarges
    assert_eq!(stage_sizes((1, 1)), vec![(16, 16)]);
    assert_eq!(
        stage_sizes((4, 9000)),
        vec![(16, 1125), (16, 141), (16, 18), (16, 16)]
    );
    for (w, h) in [(1919, 1081), (17, 17), (1000, 3)] {
        let sizes = stage_sizes((w, h));
        assert_eq!(*sizes.last().unwrap(), (16, 16));
        let mut prev = (w, h);
        for s in sizes {
            assert!(prev.0 <= s.0 * 8 || s.0 == 16, "{prev:?} to {s:?}");
            assert!(prev.1 <= s.1 * 8 || s.1 == 16, "{prev:?} to {s:?}");
            prev = s;
        }
    }
}

fn uniform(rgb: [f32; 3]) -> Stats {
    Stats {
        cells: vec![[rgb[0], rgb[1], rgb[2], 1.0]; CELLS],
    }
}

#[test]
fn auto_adjust_the_exposure_target_for_a_flat_picture() {
    // Linear 0.0802 is code 80 (brightness 0.314): the gain that reaches the edge of the dead band round 0.45.
    let g = exposure_target(&uniform([0.0802; 3]), 0.45, 0.0).exp();
    assert!((1.9..2.15).contains(&g), "{g}");
    // Already at the target: the gain is held.
    assert_eq!(exposure_target(&uniform([0.1716; 3]), 0.45, 0.0), 0.0);
    // Inside the dead band the gain is held, outside it moves.
    let near = 0.1716 * 1.04;
    assert_eq!(exposure_target(&uniform([near; 3]), 0.45, 0.0), 0.0);
    assert!(exposure_target(&uniform([0.1716 * 1.5; 3]), 0.45, 0.0) < 0.0);
    // Black has nothing to measure: the gain is held, never driven to the limit.
    assert_eq!(exposure_target(&uniform([0.0; 3]), 0.45, 0.4), 0.4);
    // Near black asks for more, but never more than 4.
    let dim = exposure_target(&uniform([dec(8); 3]), 0.45, 0.0);
    assert!(dim > 0.0 && dim <= MAX_GAIN.ln() + 1e-9, "{dim}");
    // Very bright asks for the lowest gain.
    assert!((exposure_target(&uniform([1.0; 3]), 0.45, 0.0) - MIN_GAIN.ln()).abs() < 1e-6);
    // A clear picture has nothing to measure either.
    let clear = Stats {
        cells: vec![[0.5, 0.5, 0.5, 0.0]; CELLS],
    };
    assert_eq!(exposure_target(&clear, 0.45, 0.2), 0.2);
}

#[test]
fn auto_adjust_a_bright_background_does_not_decide_the_exposure() {
    // The middle of the picture is dim, a third of the cells (the left side) is a bright window.
    let cells = (0..CELLS)
        .map(|i| {
            if i % GRID < 5 {
                [0.9, 0.9, 0.9, 1.0]
            } else {
                [0.05, 0.05, 0.05, 1.0]
            }
        })
        .collect();
    let with_window = exposure_target(&Stats { cells }, 0.45, 0.0);
    let plain = exposure_target(&uniform([0.9 * 0.31 + 0.05 * 0.69; 3]), 0.45, 0.0);
    assert!(
        with_window > plain,
        "the dim part, not the mean, drives the gain: {with_window} vs {plain}"
    );
}

#[test]
fn auto_adjust_the_white_balance_target() {
    // Neutral stays neutral.
    assert_eq!(white_balance_target(&uniform([0.2; 3]), [0.0; 3]), [0.0; 3]);
    // A blue cast asks for less blue and more red, and the luma of the mean stays.
    let m = [0.156, 0.188, 0.352];
    let t = white_balance_target(&uniform(m), [0.0; 3]);
    assert!(t[0] > 0.0 && t[2] < 0.0, "{t:?}");
    let g = t.map(|l| l.exp() as f32);
    let before = luma(m);
    let after = luma([g[0] * m[0], g[1] * m[1], g[2] * m[2]]);
    assert!((after - before).abs() / before < 0.02, "{before} {after}");
    // A red wall (saturated) is left alone instead of turned grey.
    assert_eq!(
        white_balance_target(&uniform([0.58, 0.021, 0.013]), [0.0; 3]),
        [0.0; 3]
    );
    // Black and white say nothing about the light.
    assert_eq!(
        white_balance_target(&uniform([0.0; 3]), [0.1, 0.0, 0.0]),
        [0.1, 0.0, 0.0]
    );
    assert_eq!(
        white_balance_target(&uniform([1.0, 1.0, 0.9]), [0.0; 3]),
        [0.0; 3]
    );
    // Every gain stays in range even for a strong cast.
    let t = white_balance_target(&uniform([0.4, 0.25, 0.12]), [0.0; 3]);
    for l in t {
        assert!(
            (MIN_WB.ln() - 1e-6..=MAX_WB.ln() + 1e-6).contains(&l),
            "{t:?}"
        );
    }
}

// ---------------------------------------------------------------------------------------------------------------
// The loop

#[test]
fn auto_adjust_a_dark_gray_converges_to_the_target_within_60_frames() {
    let mut rig = Rig::new(json!({ "white_balance": false }));
    let mut first_ok = None;
    let mut out = Vec::new();
    for n in 1..=90 {
        out = rig.frame(1.0 / 30.0, SMALL, flat([80; 3]));
        if first_ok.is_none() && (brightness(&out) - 0.45).abs() <= 0.05 {
            first_ok = Some(n);
        }
        if n == 1 {
            // The first frame has nothing measured yet: the picture is unchanged.
            assert_eq!(out[0], 80);
        }
    }
    let first = first_ok.expect("never reached the target");
    assert!(first <= 60, "reached the target after {first} frames");
    let b = brightness(&out);
    assert!((b - 0.45).abs() <= 0.05, "settled at {b}");
    // and at a darker start, where the gain is close to the limit of 4
    let mut rig = Rig::new(json!({ "white_balance": false }));
    let out = rig.run_flat(60, 30.0, [60; 3]);
    assert!(
        (brightness(&out) - 0.45).abs() <= 0.05,
        "{}",
        brightness(&out)
    );
}

#[test]
fn auto_adjust_a_bright_gray_converges_down() {
    let mut rig = Rig::new(json!({ "white_balance": false }));
    let start = brightness(&rig.frame(1.0 / 30.0, SMALL, flat([200; 3])));
    assert!(start > 0.7);
    let out = rig.run_flat(60, 30.0, [200; 3]);
    let b = brightness(&out);
    assert!((b - 0.45).abs() <= 0.05, "settled at {b}");
    assert!(rig.f.held().exposure < 1.0);
}

#[test]
fn auto_adjust_the_target_setting_moves_where_it_settles() {
    for target in [0.3f32, 0.6] {
        let mut rig = Rig::new(json!({ "white_balance": false, "target": target }));
        let out = rig.run_flat(90, 30.0, [100; 3]);
        assert!(
            (brightness(&out) - target).abs() <= 0.05,
            "target {target}: {}",
            brightness(&out)
        );
    }
}

/// The held exposure gain (natural log) after each frame of 2 s of a dark gray at `fps`.
fn exposure_series(fps: f64, speed: f64) -> Vec<f64> {
    let mut rig = Rig::new(json!({ "white_balance": false, "speed": speed }));
    (0..(fps * 2.0) as usize)
        .map(|_| {
            rig.frame(1.0 / fps, SMALL, flat([80; 3]));
            f64::from(rig.f.held().exposure).ln()
        })
        .collect()
}

#[test]
fn auto_adjust_convergence_takes_the_same_real_time_at_15_30_and_60_fps() {
    let series: Vec<(f64, Vec<f64>)> = [15.0, 30.0, 60.0]
        .map(|fps| (fps, exposure_series(fps, 0.3)))
        .to_vec();
    // The seconds until the held gain has covered a fraction of the way to its final value. The readback lags by
    // one frame (66 ms at 15 fps, 17 ms at 60) and a frame is the resolution of the measurement, so the times agree
    // within about one frame of the slowest rate.
    let end = *series[2].1.last().unwrap();
    for fraction in [0.3, 0.5, 0.8] {
        let times: Vec<f64> = series
            .iter()
            .map(|(fps, s)| (s.iter().position(|v| *v >= fraction * end).unwrap() + 1) as f64 / fps)
            .collect();
        let spread = times.iter().cloned().fold(f64::MIN, f64::max)
            - times.iter().cloned().fold(f64::MAX, f64::min);
        assert!(
            spread <= 0.12,
            "{fraction} of the way: {times:?} s at 15, 30 and 60 fps"
        );
        // the first order response: tau * ln(1 / (1 - fraction)) with tau = 0.5 s, plus the lag
        let ideal = -0.5 * (1.0 - fraction).ln();
        for t in &times {
            assert!(
                (t - ideal).abs() < 0.2,
                "{fraction}: {t} s, ideal {ideal} s"
            );
        }
    }
    // and it moved: at 0.5 s (one time constant) about 63 % of the way
    let mid = series[1].1[14] / series[1].1[59];
    assert!((0.5..0.75).contains(&mid), "{mid}");
}

#[test]
fn auto_adjust_speed_sets_how_fast_it_recovers() {
    let reach = |speed: f64| -> usize {
        let s = exposure_series(30.0, speed);
        let end = *s.last().unwrap();
        s.iter()
            .position(|v| *v >= 0.9 * end)
            .map_or(usize::MAX, |p| p + 1)
    };
    // Compare against the series of the fastest, because the slowest has not finished in 2 s: use the common value.
    let slow = {
        let s = exposure_series(30.0, 0.05);
        s[59]
    };
    let default = exposure_series(30.0, 0.3)[59];
    let fast = exposure_series(30.0, 1.0)[59];
    assert!(slow < 0.6 * default, "{slow} {default}");
    assert!(fast > 0.9 * default, "{fast} {default}");
    assert!(reach(1.0) < reach(0.3), "{} {}", reach(1.0), reach(0.3));
}

#[test]
fn auto_adjust_a_blue_cast_goes_neutral_with_white_balance_and_stays_without() {
    let tinted = [110u8, 120, 160];
    let mut rig = Rig::new(json!({ "exposure": false }));
    let start = rig.frame(1.0 / 30.0, SMALL, flat(tinted));
    assert!(f32::from(start[2] - start[0]) / 255.0 > 0.15);
    let out = rig.run_flat(90, 30.0, tinted);
    let rb = (f32::from(out[0]) - f32::from(out[2])).abs() / 255.0;
    assert!(rb < 0.03, "|R - B| = {rb}");
    // the brightness stays (the gains keep the luma)
    let before = brightness(&start);
    assert!(
        (brightness(&out) - before).abs() < 0.03,
        "{before} {}",
        brightness(&out)
    );

    let mut rig = Rig::new(json!({ "exposure": false, "white_balance": false }));
    let out = rig.run_flat(90, 30.0, tinted);
    assert_eq!(
        &out[..4],
        &[110, 120, 160, 255],
        "white balance off leaves the picture"
    );
}

#[test]
fn auto_adjust_both_corrections_together() {
    let mut rig = Rig::new(json!({}));
    let out = rig.run_flat(120, 30.0, [60, 70, 100]);
    let rb = (f32::from(out[0]) - f32::from(out[2])).abs() / 255.0;
    assert!(rb < 0.03, "|R - B| = {rb}");
    assert!(
        (brightness(&out) - 0.45).abs() <= 0.05,
        "{}",
        brightness(&out)
    );
}

#[test]
fn auto_adjust_does_not_oscillate_over_300_frames() {
    let mut rig = Rig::new(json!({}));
    let mut series = Vec::new();
    for _ in 0..300 {
        rig.frame(1.0 / 30.0, SMALL, flat([80, 84, 96]));
        let h = rig.f.held();
        series.push((f64::from(h.exposure), h.wb.map(f64::from)));
    }
    // From neutral to the settled gains every change goes the same way, then nothing changes.
    for k in 0..4 {
        let get = |s: &(f64, [f64; 3])| if k == 0 { s.0 } else { s.1[k - 1] };
        let values: Vec<f64> = series.iter().map(get).collect();
        let up = values.last() >= values.first();
        for w in values.windows(2) {
            let d = w[1] - w[0];
            assert!(
                if up { d >= -1e-9 } else { d <= 1e-9 },
                "channel {k} turns back: {w:?}"
            );
        }
        let settled = &values[120..];
        let (lo, hi) = settled
            .iter()
            .fold((f64::MAX, f64::MIN), |(l, h), v| (l.min(*v), h.max(*v)));
        assert!(
            (hi - lo) / hi < 0.01,
            "channel {k} varies by {}",
            (hi - lo) / hi
        );
    }
}

#[test]
fn auto_adjust_a_static_scene_with_sensor_noise_does_not_flicker() {
    let mut rig = Rig::new(json!({}));
    let mut seed = 0x9e37_79b9u32;
    let mut series = Vec::new();
    for _ in 0..240 {
        let noise: Vec<i32> = (0..SMALL.0 * SMALL.1)
            .map(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                ((seed >> 24) % 13) as i32 - 6
            })
            .collect();
        rig.frame(1.0 / 30.0, SMALL, |x, y| {
            let n = noise[(y * SMALL.0 + x) as usize];
            let c = |v: i32| (v + n).clamp(0, 255) as u8;
            [c(80), c(86), c(98), 255]
        });
        series.push(rig.f.held().exposure);
    }
    let settled = &series[90..];
    let lo = settled.iter().cloned().fold(f32::MAX, f32::min);
    let hi = settled.iter().cloned().fold(f32::MIN, f32::max);
    assert!((hi - lo) / hi < 0.01, "the gain varies from {lo} to {hi}");
}

#[test]
fn auto_adjust_a_black_frame_keeps_the_gain_in_range_and_a_normal_frame_recovers() {
    // From the start: nothing to measure, nothing happens.
    let mut rig = Rig::new(json!({}));
    let out = rig.run_flat(120, 30.0, [0; 3]);
    assert_eq!(&out[..4], &[0, 0, 0, 255]);
    assert_eq!(rig.f.held(), Gains::NEUTRAL);

    // Almost black (the lens nearly covered, noise only): the gain rises, but never above the limit.
    let mut rig = Rig::new(json!({ "white_balance": false }));
    rig.run_flat(300, 30.0, [8; 3]);
    let g = rig.f.held().exposure;
    assert!(g > 1.0 && f64::from(g) <= MAX_GAIN + 1e-6, "{g}");
    let out = rig.run_flat(1, 30.0, [8; 3]);
    assert!(
        out[0] <= 60,
        "the noise is not turned into picture: {}",
        out[0]
    );

    // A scene, then a covered lens, then a bright scene.
    let mut rig = Rig::new(json!({ "white_balance": false }));
    rig.run_flat(60, 30.0, [80; 3]);
    let before = rig.f.held().exposure;
    rig.run_flat(90, 30.0, [0; 3]);
    let covered = rig.f.held().exposure;
    assert!(f64::from(covered) <= MAX_GAIN + 1e-6);
    assert!(
        (covered - before).abs() < 0.05,
        "the gain is held while there is nothing to see: {before} {covered}"
    );
    let out = rig.run_flat(90, 30.0, [200; 3]);
    assert!(
        (brightness(&out) - 0.45).abs() <= 0.05,
        "recovered to {}",
        brightness(&out)
    );
}

#[test]
fn auto_adjust_strength_zero_is_the_exact_identity() {
    let mut rig = Rig::new(json!({ "strength": 0 }));
    let picture = |x: u32, y: u32| {
        [
            (x * 8) as u8,
            (y * 13) as u8,
            ((x + y) * 5) as u8,
            255 - (x * 7) as u8,
        ]
    };
    let mut want = Vec::new();
    for y in 0..SMALL.1 {
        for x in 0..SMALL.0 {
            want.extend_from_slice(&picture(x, y));
        }
    }
    for _ in 0..40 {
        let out = rig.frame(1.0 / 30.0, SMALL, picture);
        assert_eq!(out, want);
    }
    // The loop kept measuring, so raising the strength shows the correction at once, without a restart of the loop.
    assert!(rig.f.held().exposure != 1.0 || rig.f.held().wb != [1.0; 3]);
    rig.set(json!({ "strength": 1 }));
    let out = rig.frame(1.0 / 30.0, SMALL, picture);
    assert_ne!(out, want);
}

#[test]
fn auto_adjust_strength_half_goes_half_way() {
    let at = |strength: f64| {
        let mut rig = Rig::new(json!({ "strength": strength }));
        rig.run_flat(150, 30.0, [60, 70, 100]);
        (rig.f.applied(), rig.f.held())
    };
    let (full, held_full) = at(1.0);
    let (half, held_half) = at(0.5);
    // The loop itself is the same whatever the strength.
    assert_eq!(held_full, held_half);
    // The applied gain is the held one to the power of the strength: half the logarithm.
    let ln = |g: f32| f64::from(g).ln();
    assert!(
        (ln(half.exposure) - 0.5 * ln(full.exposure)).abs() < 1e-5,
        "{half:?} {full:?}"
    );
    for k in 0..3 {
        assert!((ln(half.wb[k]) - 0.5 * ln(full.wb[k])).abs() < 1e-5);
    }
    assert!(
        ln(full.exposure) > 0.5,
        "the test picture needs a real correction"
    );
    // and the picture is brighter than the original but short of the target
    let mut rig = Rig::new(json!({ "strength": 0.5, "white_balance": false }));
    let b = brightness(&rig.run_flat(150, 30.0, [80; 3]));
    assert!(b > 0.314 + 0.03 && b < 0.45 - 0.03, "{b}");
}

#[test]
fn auto_adjust_switching_a_correction_off_returns_it_smoothly_to_one() {
    let mut rig = Rig::new(json!({}));
    rig.run_flat(120, 30.0, [60, 70, 100]);
    let settled = rig.f.held();
    assert!(settled.exposure > 1.5 && settled.wb[0] > 1.1);

    rig.set(json!({ "exposure": false, "white_balance": true }));
    let mut previous = f64::from(settled.exposure);
    let mut biggest_step = 0.0f64;
    let mut frames_to_one = None;
    for n in 1..=200 {
        rig.run_flat(1, 30.0, [60, 70, 100]);
        let g = f64::from(rig.f.held().exposure);
        assert!(g <= previous + 1e-9, "the gain rises again: {previous} {g}");
        biggest_step = biggest_step.max((previous.ln() - g.ln()).abs());
        previous = g;
        if g == 1.0 && frames_to_one.is_none() {
            frames_to_one = Some(n);
        }
    }
    assert!(frames_to_one.is_some(), "the gain never returned to 1");
    // one frame moves at most 1 - exp(-dt / tau) = 6.5 % of the way
    assert!(
        biggest_step < 0.065 * settled.exposure.ln() as f64 + 1e-3,
        "step {biggest_step}"
    );
    // white balance, still on, stayed
    assert!(rig.f.held().wb[0] > 1.1);
    // With both off and the gains home the filter is a copy.
    rig.set(json!({ "exposure": false, "white_balance": false }));
    rig.run_flat(300, 30.0, [60, 70, 100]);
    assert_eq!(rig.f.held(), Gains::NEUTRAL);
    let out = rig.run_flat(1, 30.0, [60, 70, 100]);
    assert_eq!(&out[..4], &[60, 70, 100, 255]);
}

#[test]
fn auto_adjust_a_size_change_keeps_the_gains() {
    let mut rig = Rig::new(json!({}));
    let tint = [60u8, 70, 100];
    rig.run_flat(120, 30.0, tint);
    let before = rig.f.held();
    let b_before = brightness(&rig.run_flat(1, 30.0, tint));
    for size in [(64, 36), (20, 11), (48, 27)] {
        let out = rig.frame(1.0 / 30.0, size, flat(tint));
        assert_eq!(out.len(), (size.0 * size.1 * 4) as usize);
        let held = rig.f.held();
        assert!(
            (f64::from(held.exposure).ln() - f64::from(before.exposure).ln()).abs() < 0.02,
            "{size:?}: {held:?} {before:?}"
        );
        assert!((brightness(&out) - b_before).abs() < 0.02, "{size:?}");
        for _ in 0..5 {
            rig.frame(1.0 / 30.0, size, flat(tint));
        }
        let held = rig.f.held();
        assert!(
            (f64::from(held.exposure).ln() - f64::from(before.exposure).ln()).abs() < 0.03,
            "{size:?}: {held:?} {before:?}"
        );
    }
}

#[test]
fn auto_adjust_leaves_alpha_alone() {
    let mut rig = Rig::new(json!({}));
    let picture = |x: u32, y: u32| {
        [
            70u8,
            80,
            110,
            [255u8, 200, 128, 64, 10, 0][((x + y) % 6) as usize],
        ]
    };
    let mut last = Vec::new();
    for _ in 0..60 {
        last = rig.frame(1.0 / 30.0, SMALL, picture);
    }
    assert!(rig.f.held().exposure > 1.2, "the correction is active");
    for y in 0..SMALL.1 {
        for x in 0..SMALL.0 {
            let i = ((y * SMALL.0 + x) * 4) as usize;
            assert_eq!(last[i + 3], picture(x, y)[3], "alpha at {x},{y}");
        }
    }
    assert!(last[0] > 70, "the colour changed");
}

#[test]
fn auto_adjust_a_picture_at_target_and_neutral_is_left_alone() {
    // Code 115 is a brightness of 0.451.
    let mut rig = Rig::new(json!({}));
    for _ in 0..60 {
        let out = rig.frame(1.0 / 30.0, SMALL, flat([115; 3]));
        for p in out.chunks(4) {
            for c in 0..3 {
                assert!((i32::from(p[c]) - 115).abs() <= 1, "{p:?}");
            }
            assert_eq!(p[3], 255);
        }
    }
    assert!(f64::from(rig.f.held().exposure).ln().abs() < 0.03);
}

#[test]
fn auto_adjust_a_one_colour_picture_is_corrected_only_up_to_the_limits() {
    // A strong orange wall: the white balance asks for more than 2 on blue.
    let mut rig = Rig::new(json!({ "exposure": false }));
    let out = rig.run_flat(240, 30.0, [200, 150, 110]);
    let held = rig.f.held();
    for g in held.wb {
        assert!(
            (MIN_WB as f32 - 1e-4..=MAX_WB as f32 + 1e-4).contains(&g),
            "{held:?}"
        );
    }
    assert!(held.wb[2] > 1.9, "blue is lifted to the limit: {held:?}");
    assert!(
        out[0] > out[2],
        "the picture is not turned grey: {:?}",
        &out[..3]
    );
    // A saturated red wall is left (nearly) as it is.
    let mut rig = Rig::new(json!({ "exposure": false }));
    let out = rig.run_flat(120, 30.0, [200, 40, 30]);
    for g in rig.f.held().wb {
        assert!((g - 1.0).abs() < 0.05, "{:?}", rig.f.held());
    }
    assert!(out[0] > 190 && out[1] < 50);
}

#[test]
fn auto_adjust_a_filter_that_was_off_for_a_while_starts_from_fresh_measurements() {
    let mut rig = Rig::new(json!({ "white_balance": false }));
    rig.run_flat(60, 30.0, [80; 3]);
    let g = rig.f.held().exposure;
    // A pause of ten seconds, then a different scene: no jump from the old measurement.
    rig.now += 10.0;
    let out = rig.frame(1.0 / 30.0, SMALL, flat([200; 3]));
    assert!(
        (rig.f.held().exposure - g).abs() < 1e-3,
        "the gain moved on stale data"
    );
    assert!(out[0] > 200, "still the old gain: {}", out[0]);
    let out = rig.run_flat(90, 30.0, [200; 3]);
    assert!((brightness(&out) - 0.45).abs() <= 0.05);
}

#[test]
fn auto_adjust_the_asynchronous_readback_converges_too() {
    // The engine's mode: the measurement is mapped without waiting whenever it is ready.
    let r = warp();
    let clock = Arc::new(AtomicU64::new(0));
    let c = clock.clone();
    let mut f = AutoAdjust::new(Box::new(move || f64::from_bits(c.load(Ordering::Relaxed))));
    f.update(&r, &params(json!({ "white_balance": false })));
    let mut out = Vec::new();
    for n in 1..=400 {
        clock.store((100.0 + n as f64 / 30.0).to_bits(), Ordering::Relaxed);
        let input = r.texture(32, 18).unwrap();
        fill_rgba(&r, &input, flat([80; 3]));
        let output = r.texture(32, 18).unwrap();
        f.render(&r, &input, &output).unwrap();
        out = read_rgba(&r, &output);
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert!(
        (brightness(&out) - 0.45).abs() <= 0.05,
        "{}",
        brightness(&out)
    );
}

// ---------------------------------------------------------------------------------------------------------------
// The chain and the registry

#[test]
fn auto_adjust_is_registered_and_the_chain_keeps_its_state() {
    let r = warp();
    assert!(crate::fx::kinds().any(|k| k == KIND));
    let cfg = |id: &str, enabled: bool| FilterCfg {
        id: id.into(),
        kind: KIND.into(),
        enabled,
        params: params(json!({ "white_balance": false })),
    };
    let mut chain = Chain::new();
    chain.apply(&r, &[cfg("a", true)]);
    let serial = chain.instances()[0].3;
    let input = r.texture(32, 18).unwrap();
    fill_rgba(&r, &input, flat([80; 3]));
    let out = chain.run(&r, input).unwrap();
    assert_eq!((out.width, out.height), (32, 18));
    // An update with the same id and kind keeps the instance (and so its gains); a new id makes a new one.
    chain.apply(&r, &[cfg("a", true)]);
    assert_eq!(chain.instances()[0].3, serial);
    chain.apply(&r, &[cfg("b", true)]);
    assert_ne!(chain.instances()[0].3, serial);
}

#[test]
fn auto_adjust_defaults_match_the_schema() {
    let defaults: Value = serde_json::from_str(crate::fx::DEFAULTS_JSON).unwrap();
    let d = defaults[KIND]
        .as_object()
        .expect("defaults.json has auto_adjust");
    let mut keys: Vec<&str> = d.keys().map(String::as_str).collect();
    keys.sort_unstable();
    let mut want = KEYS.to_vec();
    want.sort_unstable();
    assert_eq!(keys, want);
    assert_eq!(settings(d), settings(&Params::new()));
    assert_eq!(
        settings(&Params::new()),
        Settings {
            exposure: true,
            white_balance: true,
            target: 0.45,
            strength: 1.0,
            speed: 0.3
        }
    );
    // out of range values are clamped
    let s = settings(&params(json!({ "target": 5, "strength": -1, "speed": 0 })));
    assert_eq!((s.target, s.strength, s.speed), (0.8, 0.0, 0.05));
}

// ---------------------------------------------------------------------------------------------------------------
// Cost

/// Median GPU time of `frames` frames of `run` on the hardware adapter, in milliseconds.
fn median_gpu_ms(r: &Renderer, timer: &mut GpuTimer, frames: usize, mut run: impl FnMut()) -> f64 {
    let mut times = Vec::new();
    for n in 0..frames + 20 {
        assert!(timer.begin(&r.gpu.context));
        run();
        timer.end(&r.gpu.context);
        r.flush();
        let mut ms = None;
        for _ in 0..100_000 {
            ms = timer.poll(&r.gpu.context);
            if ms.is_some() {
                break;
            }
            std::thread::yield_now();
        }
        if n >= 20 {
            times.push(ms.expect("the timer answered"));
        }
    }
    times.sort_by(|a, b| a.total_cmp(b));
    times[times.len() / 2]
}

#[test]
fn auto_adjust_costs_well_under_a_fifth_of_a_millisecond_at_1080p() {
    let gpu = match GpuDevice::new(AdapterKind::Hardware) {
        Ok(g) => Arc::new(g),
        Err(e) => {
            eprintln!("no hardware adapter in this environment, cost not measured: {e}");
            return;
        }
    };
    let r = Renderer::new(gpu).unwrap();
    let mut timer = GpuTimer::new(&r.gpu.device).unwrap();
    // A picture with texture, so the shaders do not run on a constant.
    let tex = r.texture(1920, 1080).unwrap();
    fill_rgba(&r, &tex, |x, y| {
        [
            (x / 8) as u8 ^ (y / 4) as u8,
            (x / 7 + y / 9) as u8,
            ((x * y) >> 6) as u8,
            255,
        ]
    });
    let output = r.texture(1920, 1080).unwrap();
    r.flush();

    let mut f = AutoAdjust::new(real_clock());
    f.update(&r, &Params::new());
    // Force the shader path (a filter that is neutral would copy).
    f.ln_exposure = 0.5;
    f.ln_wb = [0.1, 0.0, -0.1];
    let mut color = crate::fx::make("color_filter").unwrap();
    color.update(
        &r,
        &params(json!({ "saturation": 0.2, "brightness": 0.05 })),
    );

    let baseline = median_gpu_ms(&r, &mut timer, 120, || {});
    let with = median_gpu_ms(&r, &mut timer, 120, || {
        f.render(&r, &tex, &output).unwrap();
    });
    let color_ms = median_gpu_ms(&r, &mut timer, 120, || {
        color.render(&r, &tex, &output).unwrap();
    });
    eprintln!(
        "1080p on {}: no filter {baseline:.3} ms, auto adjust {with:.3} ms, colour correction {color_ms:.3} ms; the filter adds {:.3} ms",
        r.gpu.adapter_name,
        with - baseline
    );
    assert!(
        with - baseline < 0.2,
        "auto adjust adds {:.3} ms",
        with - baseline
    );
}
