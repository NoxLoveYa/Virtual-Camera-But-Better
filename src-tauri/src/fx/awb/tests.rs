//! Cases A1-A12 of docs/AUTO-WHITE-BALANCE.md 5.2 on the estimator, with the scene kit of `scenes.rs`.

use super::real;
use super::scenes::*;
use super::{centre_weight, estimate_light, estimate_with_gate, gains, Reason, LOW_CONFIDENCE};
use crate::fx::auto_adjust::Stats;

const NEUTRAL: [f64; 3] = [1.0; 3];
/// The gain limits of the proposal (D3).
const LIMITS: (f64, f64) = (0.25, 4.0);
const CAM: f64 = 4600.0;

/// The gains the proposal settles on for a picture, aiming at grey.
fn gains_of(stats: &Stats) -> [f64; 3] {
    estimate_light(stats).gains([1.0; 3], LIMITS)
}

/// `(angle in degrees, residual in mired)` of a neutral surface after the proposal, for a scene under `t` kelvin on a
/// camera at `k`.
fn proposed(name: &str, t: f64, k: f64) -> (f64, f64) {
    let out = neutral_out(gains_of(&shoot(name, t, k)), t, k);
    (angle(out, NEUTRAL), residual(out, NEUTRAL))
}

/// The same for no correction at all.
fn none(t: f64, k: f64) -> (f64, f64) {
    let out = neutral_out([1.0; 3], t, k);
    (angle(out, NEUTRAL), residual(out, NEUTRAL))
}

/// Today's Auto Adjust in its steady state (`auto_adjust::white_balance_target` before this change): gray world on the
/// cells that are not strongly saturated, judged before the correction, gains limited to `0.5 ..= 2`.
fn today_gains(stats: &Stats) -> [f64; 3] {
    let smooth = |e0: f64, e1: f64, x: f64| {
        let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
        t * t * (3.0 - 2.0 * t)
    };
    let (mut total, mut ws) = (0.0, 0.0);
    let mut sum = [0.0f64; 3];
    for (i, c) in stats.cells.iter().enumerate() {
        let cw = centre_weight(i) * f64::from(c[3]);
        total += cw;
        let rgb = [f64::from(c[0]), f64::from(c[1]), f64::from(c[2])];
        let y = srgb_encode(luma(rgb));
        let (mx, mn) = (rgb[0].max(rgb[1]).max(rgb[2]), rgb[0].min(rgb[1]).min(rgb[2]));
        let sat = if mx > 1e-4 { (mx - mn) / mx } else { 1.0 };
        let w = cw * smooth(0.04, 0.15, y) * (1.0 - smooth(0.85, 0.97, y)) * (1.0 - smooth(0.55, 0.9, sat));
        ws += w;
        for k in 0..3 {
            sum[k] += w * rgb[k];
        }
    }
    let conf = smooth(0.02, 0.12, ws / total);
    let mean = sum.map(|s| s / ws);
    if ws <= 0.0 || conf <= 0.0 || mean.iter().any(|m| *m < 1e-5) {
        return [1.0; 3];
    }
    let y = luma(mean);
    let raw = mean.map(|m| (y / m).clamp(0.5, 2.0));
    let keep = y / luma([raw[0] * mean[0], raw[1] * mean[1], raw[2] * mean[2]]);
    raw.map(|g| (g * keep).clamp(0.5, 2.0))
}

fn today(name: &str, t: f64, k: f64) -> (f64, f64) {
    let out = neutral_out(today_gains(&shoot(name, t, k)), t, k);
    (angle(out, NEUTRAL), residual(out, NEUTRAL))
}

const NORMAL: [&str; 4] = ["office", "office_dark_skin", "grey_room", "checker"];

#[test]
fn a1_normal_scenes_are_neutral_from_2700_to_6500() {
    for name in NORMAL {
        for t in [2700.0, 3000.0, 3500.0, 4000.0, 5000.0, 5500.0, 6500.0] {
            let (a, r) = proposed(name, t, CAM);
            assert!(a <= 3.0 && r.abs() <= 12.0, "{name} at {t} K: {a:.2} deg, {r:+.1} mired");
        }
    }
}

#[test]
fn a2_normal_scenes_at_2500_k() {
    for name in NORMAL {
        let (a, r) = proposed(name, 2500.0, CAM);
        assert!(a <= 5.0, "{name} at 2500 K: {a:.2} deg, {r:+.1} mired");
    }
}

#[test]
fn a3_normal_scenes_at_2200_k_are_gain_limited_but_better_than_today() {
    for name in NORMAL {
        let (_, r) = proposed(name, 2200.0, CAM);
        let (_, today) = today(name, 2200.0, CAM);
        assert!(r <= 95.0, "{name} at 2200 K: {r:+.1} mired");
        // The doc says "less than half of today's". With the gain limit of 4 the floor is +86, and the checker's
        // today (+166) halves to +83, which no estimate can reach: 0.55 keeps the intent (see the doc, W1 notes).
        assert!(r < 0.55 * today, "{name} at 2200 K: {r:+.1} mired, today {today:+.1}");
    }
}

#[test]
fn a4_a_camera_at_2800_k_corrects_2200_to_3000_k() {
    for name in NORMAL {
        for t in [2200.0, 2500.0, 2700.0, 3000.0] {
            let (a, r) = proposed(name, t, 2800.0);
            assert!(a <= 3.0, "{name} at {t} K on 2800 K: {a:.2} deg, {r:+.1} mired");
        }
    }
}

#[test]
fn a5_closeup_and_warm_room_do_not_regress() {
    for t in [2500.0, 2700.0, 3000.0, 3500.0, 4000.0, 5000.0, 5500.0, 6500.0] {
        let (a, r) = proposed("closeup", t, CAM);
        assert!(a <= 6.5, "closeup at {t} K: {a:.2} deg, {r:+.1} mired");
    }
    for t in [3000.0, 3500.0, 4000.0, 5000.0, 5500.0, 6500.0] {
        let (a, r) = proposed("warm_room", t, CAM);
        assert!(a <= 14.0, "warm_room at {t} K: {a:.2} deg, {r:+.1} mired");
    }
}

#[test]
fn a6_a_one_colour_picture_is_never_corrected_past_no_correction() {
    for name in ONE_COLOUR {
        for t in LIGHTS {
            for k in [4600.0, 6500.0] {
                let (a, r) = proposed(name, t, k);
                let (a0, r0) = none(t, k);
                // The one known exception (doc 3.6, "a blue wall under tungsten looks grey"): the model leaves
                // blue_wall at 2700 K on a 4600 K camera 1.4 deg (15 mired) past none. It stays visible here.
                let slack = if name == "blue_wall" && t == 2700.0 && k == 4600.0 { 2.0 } else { 1.0 };
                assert!(
                    a <= a0 + slack,
                    "{name} at {t} K on {k} K: {a:.2} deg ({r:+.1} mired), none {a0:.2} deg ({r0:+.1})"
                );
            }
        }
    }
}

#[test]
fn a7_a_face_or_a_red_wall_in_daylight_is_not_taken_for_a_cast() {
    for name in ["face_fill", "red_wall"] {
        for t in [5000.0, 5500.0, 6500.0] {
            let est = estimate_light(&shoot(name, t, CAM));
            assert!(est.conf <= 0.1, "{name} at {t} K: conf {:.3}", est.conf);
            assert_eq!(est.reason, Reason::OneColour, "{name} at {t} K");
        }
    }
}

#[test]
fn a8_the_gate_is_judged_after_the_correction() {
    let cells = scene("office");
    let (est, gate) = estimate_with_gate(&camera(&cells, blackbody_rgb(2700.0), CAM));
    assert!(est.conf > 0.9, "conf {}", est.conf);
    let mean_of = |name: &str| {
        let g: Vec<f64> = cells
            .iter()
            .enumerate()
            .filter(|(i, c)| c.name == name && !HIGHLIGHTS.contains(i))
            .map(|(i, _)| gate[i])
            .collect();
        assert!(!g.is_empty(), "no cell of {name}");
        g.iter().sum::<f64>() / g.len() as f64
    };
    let wall = mean_of("wall_grey");
    let shirt = mean_of("shirt_blue");
    assert!(wall >= 0.8, "grey wall gate {wall:.3}");
    assert!(shirt <= 0.2, "blue shirt gate {shirt:.3}");
    // The cells of the grey wall that are in the picture all count.
    for (i, c) in cells.iter().enumerate() {
        if c.name == "wall_grey" && !HIGHLIGHTS.contains(&i) {
            assert!(gate[i] >= 0.8, "wall cell {i} gate {:.3}", gate[i]);
        }
    }
}

#[test]
fn a9_noise_moves_the_temperature_less_than_a_hundredth() {
    for name in ["office", "checker"] {
        for t in [2700.0, 5500.0] {
            let base = shoot(name, t, CAM);
            let mut rng = Rng::new(3);
            let ts: Vec<f64> = (0..200).map(|_| estimate_light(&noisy(&base, &mut rng)).t).collect();
            let mean = ts.iter().sum::<f64>() / ts.len() as f64;
            let sd = (ts.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / ts.len() as f64).sqrt();
            assert!(sd <= 0.01, "{name} at {t} K: sd of t {sd:.4}");
        }
    }
}

#[test]
fn a10_a_neutral_grey_picture_gets_gains_of_exactly_one() {
    for level in [0.05f32, 0.2, 0.45] {
        let stats = Stats { cells: vec![[level, level, level, 1.0]; 256] };
        let est = estimate_light(&stats);
        assert!(est.conf > 0.9, "conf {}", est.conf);
        assert_eq!(est.gains([1.0; 3], LIMITS), [1.0; 3], "level {level}");
    }
}

#[test]
fn a11_gains_stay_within_the_limits_and_keep_the_luma() {
    for name in SCENES {
        for t in LIGHTS {
            for k in [2800.0, 4600.0, 6500.0] {
                let stats = shoot(name, t, k);
                let est = estimate_light(&stats);
                let g = est.gains([1.0; 3], LIMITS);
                for c in g {
                    assert!((0.25..=4.0).contains(&c), "{name} at {t} K on {k} K: gains {g:?}");
                }
                let kept = luma([g[0] * est.light[0], g[1] * est.light[1], g[2] * est.light[2]]) / luma(est.light);
                assert!((kept - 1.0).abs() <= 0.02, "{name} at {t} K on {k} K: luma kept {kept:.4}");
            }
        }
    }
}

#[test]
fn a12_mixed_light_follows_the_light_on_most_neutral_surfaces() {
    // D2 (taken 2026-11-19): 5500 | 2700 K with the split at column 8, the daylight side has more neutral surface.
    let stats = mixed("office", 5500.0, 2700.0, CAM, 8);
    let g = gains_of(&stats);
    let day = neutral_out(g, 5500.0, CAM);
    assert!(residual(day, NEUTRAL).abs() <= 15.0, "daylight side {:+.1} mired", residual(day, NEUTRAL));
    assert!(estimate_light(&stats).conf >= LOW_CONFIDENCE);
}

// ---------------------------------------------------------------------------------------------------------------
// Frames from a real camera (`real.rs`): a dim room, a bright window, a face in the lower middle, then a lamp.

#[test]
fn a_real_room_with_a_bright_face_says_when_it_has_two_answers() {
    // 112.4 - 113.6 s: the picture hardly changes, and the shades-of-gray start lands on `t` -0.55, -0.27 or -0.18
    // (the face pulls it) where the median start stays at 0 (docs/AUTO-WHITE-BALANCE.md 9).
    let calm: Vec<_> = real::between(108.0, 112.4).into_iter().chain(real::between(40.0, 44.0)).collect();
    assert!(calm.len() > 40);
    for f in &calm {
        let est = estimate_light(&f.stats);
        assert!(est.apart < 0.02 && est.conf > 0.9, "{:.3} s: apart {:.3}, conf {:.2}", f.t, est.apart, est.conf);
    }
    let doubt = real::between(112.4, 113.6);
    assert!(doubt.len() > 8);
    for f in &doubt {
        let est = estimate_light(&f.stats);
        assert!(est.apart > 0.15, "{:.3} s: apart {:.3}", f.t, est.apart);
        // The alternative answer is the room's: neutral.
        assert!(est.t_alt.abs() < 0.05, "{:.3} s: the median start says {:+.3}", f.t, est.t_alt);
    }
}

#[test]
fn a_real_lamp_coming_on_is_one_answer() {
    // 164.0 - 165.4 s: the whole picture turns orange (`t` about -1); nothing to doubt, it is the light.
    for f in real::between(164.0, 165.4) {
        let est = estimate_light(&f.stats);
        let ok = est.t < -0.8 && est.conf > 0.9 && est.apart < 0.02;
        assert!(ok, "{:.3} s: t {:+.3}, conf {:.2}, apart {:.3}", f.t, est.t, est.conf, est.apart);
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Behaviour the cases above do not name.

#[test]
fn the_estimate_costs_microseconds() {
    // The budget of the doc (5.2, GPU and perf): median of 100 runs at most 50 us.
    let stats = shoot("office", 2700.0, CAM);
    let mut times: Vec<u128> = (0..100)
        .map(|_| {
            let t0 = std::time::Instant::now();
            std::hint::black_box(estimate_light(std::hint::black_box(&stats)));
            t0.elapsed().as_nanos()
        })
        .collect();
    times.sort_unstable();
    println!("estimate_light median {} ns", times[50]);
    assert!(times[50] <= 50_000, "median {} ns", times[50]);
}

#[test]
fn a_dark_picture_says_dark() {
    let stats = Stats { cells: vec![[0.001, 0.001, 0.001, 1.0]; 256] };
    let est = estimate_light(&stats);
    assert_eq!((est.conf, est.reason), (0.0, Reason::Dark));
    assert_eq!(est.gains([1.0; 3], LIMITS), [1.0; 3]);
}

#[test]
fn nothing_there_or_garbage_is_held_not_a_panic() {
    let empty = Stats { cells: Vec::new() };
    assert_eq!(estimate_light(&empty).conf, 0.0);
    let clear = Stats { cells: vec![[0.5, 0.4, 0.3, 0.0]; 256] };
    assert_eq!(estimate_light(&clear).conf, 0.0);
    let mut nan = shoot("office", 5500.0, CAM);
    for c in nan.cells.iter_mut().step_by(7) {
        c[1] = f32::NAN;
    }
    let est = estimate_light(&nan);
    assert!(est.t.is_finite() && est.g.is_finite() && est.conf.is_finite());
    let clipped = Stats { cells: vec![[1.0, 1.0, 1.0, 1.0]; 256] };
    assert_eq!(estimate_light(&clipped).conf, 0.0);
}

#[test]
fn a_face_filling_the_frame_is_held_in_daylight_and_only_partly_corrected_under_tungsten() {
    // D4 (taken 2026-11-19): hold. The model's numbers: conf 0 at 5500 K, 0.38 at 2700 K (a third of the way), and
    // a full correction at 2200 K, where a face is orange enough to look like a cast (doc 3.5: -62 mired).
    let day = estimate_light(&shoot("face_fill", 5500.0, CAM));
    assert_eq!((day.conf, day.reason), (0.0, Reason::OneColour));
    let lamp = estimate_light(&shoot("face_fill", 2700.0, CAM));
    assert!(lamp.conf < 0.5, "face_fill at 2700 K: conf {:.2}", lamp.conf);
    let (a, _) = proposed("face_fill", 2700.0, CAM);
    assert!(a < none(2700.0, CAM).0, "{a:.1} deg is no better than no correction");
}

#[test]
fn a_cell_with_a_clipped_channel_does_not_count() {
    // The same picture with one very bright orange cell added: its clipped red must not move the estimate.
    let base = shoot("office", 2700.0, CAM);
    let mut with = base.clone();
    with.cells[7 * 16 + 1] = [1.0, 0.55, 0.2, 1.0];
    let (a, b) = (estimate_light(&base), estimate_light(&with));
    assert!((a.t - b.t).abs() < 0.01, "{} vs {}", a.t, b.t);
}

#[test]
fn gains_scale_with_the_confidence_in_the_log_domain() {
    let light = estimate_light(&shoot("office", 2700.0, CAM)).light;
    let full = gains(light, [1.0; 3], 1.0, LIMITS);
    let half = gains(light, [1.0; 3], 0.5, LIMITS);
    let none = gains(light, [1.0; 3], 0.0, LIMITS);
    assert_eq!(none, [1.0; 3]);
    for k in 0..3 {
        // The half correction is roughly the square root (the luma renormalisation moves it slightly).
        assert!((half[k].ln() - 0.5 * full[k].ln()).abs() < 0.05, "{k}: {half:?} {full:?}");
    }
}

#[test]
fn the_aim_moves_the_result() {
    // A warm aim keeps a neutral surface orange: R above B.
    let est = estimate_light(&shoot("office", 5500.0, CAM));
    let aim = crate::fx::auto_adjust::white_aim(0.3, 0.0);
    let g = est.gains(aim, LIMITS);
    let out = neutral_out(g, 5500.0, CAM);
    assert!(out[0] > out[2], "{out:?}");
}

/// Prints the table of the report (`cargo test -p better-cam --lib fx::awb::tests::report -- --ignored --nocapture`).
#[test]
#[ignore]
fn report() {
    for name in ["office", "grey_room", "closeup", "warm_room"] {
        for (label, f) in [("today", 0), ("proposed", 1)] {
            let mut row = format!("{name:10} {label:9}");
            for t in [2200.0, 2500.0, 2700.0, 3000.0, 4000.0, 5500.0, 6500.0] {
                let (a, r) = if f == 0 { today(name, t, CAM) } else { proposed(name, t, CAM) };
                row += &format!(" {t:5.0}K {r:+5.0}/{a:4.1}");
            }
            println!("{row}");
        }
    }
    for name in ONE_COLOUR {
        for t in LIGHTS {
            for k in [4600.0, 6500.0] {
                let (a, r) = proposed(name, t, k);
                let (a0, r0) = none(t, k);
                let est = estimate_light(&shoot(name, t, k));
                if a > a0 + 0.5 || r > r0 + 5.0 {
                    println!(
                        "ONE {name:14} {t:5.0}K cam {k:4.0}: {a:5.2} deg {r:+6.1} mired | none {a0:5.2} {r0:+6.1} | conf {:.2} t {:+.2}",
                        est.conf, est.t
                    );
                }
            }
        }
    }
    for name in ["office", "checker"] {
        for t in [2700.0, 5500.0] {
            let base = shoot(name, t, CAM);
            let mut rng = Rng::new(3);
            let ts: Vec<f64> = (0..200).map(|_| estimate_light(&noisy(&base, &mut rng)).t).collect();
            let mean = ts.iter().sum::<f64>() / 200.0;
            let sd = (ts.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / 200.0).sqrt();
            println!("NOISE {name} {t} K sd of t {sd:.4} ({:.2} mired)", sd / 0.0105);
        }
    }
    for t in [2200.0, 2700.0, 5500.0] {
        let est = estimate_light(&shoot("face_fill", t, CAM));
        println!("FACE {t} K conf {:.3} t {:+.2} reason {:?}", est.conf, est.t, est.reason);
    }
    let cells = scene("office");
    let (_, gate) = estimate_with_gate(&camera(&cells, blackbody_rgb(2700.0), CAM));
    let avg = |name: &str| {
        let g: Vec<f64> =
            cells.iter().enumerate().filter(|(i, c)| c.name == name && !HIGHLIGHTS.contains(i)).map(|(i, _)| gate[i]).collect();
        (g.iter().cloned().fold(f64::MAX, f64::min), g.iter().sum::<f64>() / g.len() as f64)
    };
    println!("A8 wall min/mean {:?} shirt {:?}", avg("wall_grey"), avg("shirt_blue"));
    let m = mixed("office", 5500.0, 2700.0, CAM, 8);
    let g = gains_of(&m);
    println!(
        "A12 daylight {:+.1} lamp {:+.1}",
        residual(neutral_out(g, 5500.0, CAM), NEUTRAL),
        residual(neutral_out(g, 2700.0, CAM), NEUTRAL)
    );
    for name in ["office_dark_skin", "checker"] {
        let mut row = format!("{name:16}");
        for t in [2200.0, 2500.0, 2700.0, 3000.0, 4000.0, 5500.0, 6500.0] {
            let (a, r) = proposed(name, t, CAM);
            row += &format!(" {t:5.0}K {r:+5.0}/{a:4.1}");
        }
        println!("{row}");
    }
}
