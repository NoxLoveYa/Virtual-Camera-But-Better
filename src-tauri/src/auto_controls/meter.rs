//! The exposure meter (docs/AUTO-CONTROLS.md 2.4): from the 16x16 statistics image of the camera picture (before any
//! filter) to "how many stops of light does the picture need".
//!
//! Pure maths, no state. `Stats` is `fx::auto_adjust::Stats` (it is `pub` there); the small helpers below are f64
//! copies of the private ones in `fx/auto_adjust.rs` (`srgb_encode`, `luma`, `smoothstep`, `centre_weight`), copied
//! because that file is changed in parallel. Phase A3 unifies them.
//!
//! The white balance meter (the shared light estimator of `fx::awb`, 2.4 "White balance error", W3) sits next to
//! [`measure_exposure`]: [`measure_white_balance`] and the warmth scale [`warmth_target`].

use crate::fx::auto_adjust::Stats;
pub use crate::fx::awb::Reason;
use crate::fx::awb::estimate_light;

/// Cells per side of the statistics image.
pub const GRID: usize = 16;

/// Cells with less coverage than this are left out (as Auto Adjust does).
const MIN_COVERAGE: f32 = 0.02;
/// The error is searched in `log2 k` within this many stops either way: a reading at this value only says "at least".
pub const SEARCH_STOPS: f64 = 4.0;
const SEARCH_ITERATIONS: usize = 24;

/// The sRGB transfer function (linear to coded), clamped to `0..=1`.
pub fn srgb_encode(l: f64) -> f64 {
    let c = l.clamp(0.0, 1.0);
    if c <= 0.003_130_8 {
        12.92 * c
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

/// The sRGB transfer function (coded to linear), clamped to `0..=1` on input.
pub fn srgb_decode(c: f64) -> f64 {
    let c = c.clamp(0.0, 1.0);
    if c <= 0.040_45 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// Rec. 709 luma of a linear colour.
pub fn luma(c: [f32; 3]) -> f64 {
    0.2126 * f64::from(c[0]) + 0.7152 * f64::from(c[1]) + 0.0722 * f64::from(c[2])
}

pub fn smoothstep(e0: f64, e1: f64, x: f64) -> f64 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// The weight of cell `i` for its place in the picture: 1 in the middle, 0.4 at the edge, 0.25 in a corner.
pub fn centre_weight(i: usize) -> f64 {
    let u = ((i % GRID) as f64 + 0.5) / GRID as f64 * 2.0 - 1.0;
    let v = ((i / GRID) as f64 + 0.5) / GRID as f64 * 2.0 - 1.0;
    1.0 / (1.0 + 1.5 * (u * u + v * v))
}

/// What one measurement says about the exposure.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ExposureReading {
    /// Stops of light the picture needs to reach the target brightness: positive = brighter wanted. `-4..=4`.
    pub error: f64,
    /// `0..=1`: how much of the picture has anything to measure (a covered lens is 0).
    pub confidence: f64,
    /// `0..=1`: the (centre weighted) share of cells that are clipped to white.
    pub clip: f64,
}

/// 2.4 "Exposure error". `target` is the brightness on the sRGB-coded scale of Auto Adjust (0.45 is about 18 % grey).
pub fn measure_exposure(stats: &Stats, target: f64) -> ExposureReading {
    // (linear luma, weight) of the cells that count for the brightness.
    let mut cells: Vec<(f64, f64)> = Vec::with_capacity(stats.cells.len());
    let (mut total, mut lit, mut clipped, mut weight_sum) = (0.0, 0.0, 0.0, 0.0);
    for (i, c) in stats.cells.iter().enumerate() {
        if c[3] < MIN_COVERAGE {
            continue;
        }
        let cw = centre_weight(i) * f64::from(c[3]);
        let lin = luma([c[0], c[1], c[2]]);
        let y = srgb_encode(lin);
        let not_black = smoothstep(0.02, 0.08, y);
        let not_white = 0.15 + 0.85 * (1.0 - smoothstep(0.93, 0.99, y));
        total += cw;
        lit += cw * not_black;
        if y >= 0.97 {
            clipped += cw;
        }
        let w = cw * not_black * not_white;
        if w > 0.0 {
            cells.push((lin, w));
            weight_sum += w;
        }
    }
    if total <= 0.0 {
        return ExposureReading {
            error: 0.0,
            confidence: 0.0,
            clip: 0.0,
        };
    }
    let confidence = smoothstep(0.03, 0.12, lit / total);
    let clip = clipped / total;
    if weight_sum <= 0.0 {
        return ExposureReading {
            error: 0.0,
            confidence,
            clip,
        };
    }
    // The brightness the picture would have with `k` times the light. Hard clip: the sensor clips.
    let brightness = |log2_k: f64| -> f64 {
        let k = log2_k.exp2();
        cells
            .iter()
            .map(|(lin, w)| w * srgb_encode((k * lin).min(1.0)))
            .sum::<f64>()
            / weight_sum
    };
    let error = if brightness(SEARCH_STOPS) <= target {
        SEARCH_STOPS
    } else if brightness(-SEARCH_STOPS) >= target {
        -SEARCH_STOPS
    } else {
        let (mut lo, mut hi) = (-SEARCH_STOPS, SEARCH_STOPS);
        for _ in 0..SEARCH_ITERATIONS {
            let mid = 0.5 * (lo + hi);
            if brightness(mid) < target {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        0.5 * (lo + hi)
    };
    ExposureReading {
        error,
        confidence,
        clip,
    }
}

/// What one measurement says about the white balance.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WhiteBalanceReading {
    /// `e_wb = c - c*`: the blue-red signal minus the signal the warmth setting aims at. Negative = too orange.
    pub error: f64,
    /// `0..=1`: how much of the picture looks like a lit scene with a cast rather than one colour or nothing.
    pub confidence: f64,
    /// Why the confidence is low (`Ok` when it is not): picks the text of the hold.
    pub reason: Reason,
    /// `0..=1`: the estimator's own confidence, before the doubt of two answers took any of it. The slow lane
    /// (docs/AUTO-WHITE-BALANCE.md 10.4.2) follows a picture whose `light_conf` is sure but whose `confidence` is not.
    pub light_conf: f64,
}

/// `c*`: the blue-red signal `ln(B/R)` a neutral surface should have for `warmth` (`-1..=1`, +1 = warmer).
///
/// The scale is Auto Adjust's (`fx::auto_adjust::white_aim`), so "warmth +0.3" means the same colour in both places:
/// the aim is the colour of a blackbody whose temperature is linear in mired from 5500 K at 0 to 4000 K at +1 and
/// 7000 K at -1, relative to 5500 K. `c*` is the log of that aim's blue over its red: 0 at warmth 0, about -0.69 at
/// +1 (blue is half the red, orange) and about +0.38 at -1.
pub fn warmth_target(warmth: f64) -> f64 {
    let aim = crate::fx::auto_adjust::white_aim(warmth.clamp(-1.0, 1.0) as f32, 0.0);
    (f64::from(aim[2]) / f64::from(aim[0])).ln()
}

/// Below this mean coded luma (centre weighted) the picture is too dark for colour, whatever its cells look like: a
/// black picture lifted by gain is flat grey noise, and flat grey reads as a neutral light (F4's leftover). About
/// 25 of 255; the exposure loop aims at 0.45, so only a picture that is dark for good ends here.
pub const MIN_MEAN_LUMA: f64 = 0.10;

/// `LightEstimate::apart` (in `t`) from which the loop doubts the reading: no doubt below the first edge, none of
/// the confidence left at the second. Doubt slows the loop (the slow lane of docs/AUTO-WHITE-BALANCE.md 10.4.2), it
/// does not hold it.
pub const TWO_ANSWERS: (f64, f64) = (0.02, 0.06);

/// The centre weighted mean coded luma of the cells that have a picture, `None` when there are none.
fn mean_coded_luma(stats: &Stats) -> Option<f64> {
    let (mut total, mut sum) = (0.0, 0.0);
    for (i, c) in stats.cells.iter().enumerate() {
        if c[3] < MIN_COVERAGE {
            continue;
        }
        let cw = centre_weight(i) * f64::from(c[3]);
        total += cw;
        sum += cw * srgb_encode(luma([c[0], c[1], c[2]]));
    }
    (total > 0.0).then(|| sum / total)
}

/// 2.4 "White balance error": the shared estimator (`fx::awb::estimate_light`, docs/AUTO-WHITE-BALANCE.md 3.1) reads
/// the light the picture is lit by; the error is its blue-red signal `t` minus the signal the warmth aims at. A
/// picture that is too dark for colour has no confidence, whatever the estimator makes of it.
pub fn measure_white_balance(stats: &Stats, warmth: f64) -> WhiteBalanceReading {
    let est = estimate_light(stats);
    if mean_coded_luma(stats).is_none_or(|y| y < MIN_MEAN_LUMA) {
        return WhiteBalanceReading {
            error: 0.0,
            confidence: 0.0,
            reason: Reason::Dark,
            light_conf: 0.0,
        };
    }
    // A picture with two answers (a bright face beside the walls: the estimator's two starts end `apart` from each
    // other) is not followed at once: the loop would take whichever the next frame lands on (docs/AUTO-WHITE-BALANCE.md
    // 9). Its middle is still a measurement: the slow lane averages it (10.4.2), so the reason says "two answers", not
    // "one colour".
    let confidence = est.conf * (1.0 - smoothstep(TWO_ANSWERS.0, TWO_ANSWERS.1, est.apart));
    let reason = if est.reason == Reason::Ok && confidence < crate::fx::awb::LOW_CONFIDENCE {
        Reason::TwoAnswers
    } else {
        est.reason
    };
    WhiteBalanceReading {
        error: 0.5 * (est.t + est.t_alt) - warmth_target(warmth),
        confidence,
        reason,
        light_conf: est.conf,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fx::awb::real;

    fn tinted(c: f64) -> Stats {
        // Luma 0.25 everywhere, blue over red `e^c`.
        let (r, b) = ((-0.5 * c).exp(), (0.5 * c).exp());
        let n = 0.25 / (0.2126 * r + 0.7152 + 0.0722 * b);
        Stats {
            cells: vec![[(r * n) as f32, n as f32, (b * n) as f32, 1.0]; GRID * GRID],
        }
    }

    #[test]
    fn warmth_scale_is_auto_adjusts() {
        assert_eq!(warmth_target(0.0), 0.0);
        // +1 is 4000 K against 5500 K: about [1.25, 0.96, 0.63], so blue is half the red.
        assert!((warmth_target(1.0) + 0.694).abs() < 0.005, "{}", warmth_target(1.0));
        assert!((warmth_target(0.3) + 0.204).abs() < 0.005, "{}", warmth_target(0.3));
        // -1 is 7000 K: about [0.87, 1.01, 1.28].
        assert!((warmth_target(-1.0) - 0.380).abs() < 0.005, "{}", warmth_target(-1.0));
        // Monotone: warmer is lower.
        let mut prev = f64::MAX;
        for i in -10..=10 {
            let c = warmth_target(f64::from(i) / 10.0);
            assert!(c < prev, "{i}: {c} !< {prev}");
            prev = c;
        }
        // The same aim Auto Adjust applies.
        let aim = crate::fx::auto_adjust::white_aim(0.3, 0.0);
        assert!((warmth_target(0.3) - (f64::from(aim[2]) / f64::from(aim[0])).ln()).abs() < 1e-12);
    }

    #[test]
    fn white_balance_error_is_the_blue_red_signal_minus_the_aim() {
        let r = measure_white_balance(&tinted(0.0), 0.0);
        assert!(r.error.abs() < 1e-4 && r.confidence > 0.99, "{r:?}");
        let r = measure_white_balance(&tinted(-0.5), 0.0);
        assert!((r.error + 0.5).abs() < 1e-3, "{r:?}");
        // A picture exactly as warm as the setting asks for has no error.
        let r = measure_white_balance(&tinted(warmth_target(0.6)), 0.6);
        assert!(r.error.abs() < 1e-3, "{r:?}");
    }

    #[test]
    fn dark_or_one_colour_pictures_have_no_white_balance_confidence() {
        let black = Stats {
            cells: vec![[0.0, 0.0, 0.0, 1.0]; GRID * GRID],
        };
        assert_eq!(measure_white_balance(&black, 0.0).confidence, 0.0);
        // Strongly saturated everywhere: nothing neutral to trust.
        let red = Stats {
            cells: vec![[0.5, 0.01, 0.01, 1.0]; GRID * GRID],
        };
        assert!(measure_white_balance(&red, 0.0).confidence < 0.3);
    }

    fn flat(lin: f32) -> Stats {
        Stats {
            cells: vec![[lin, lin, lin, 1.0]; GRID * GRID],
        }
    }

    #[test]
    fn a_lifted_black_is_dark_not_a_neutral_grey() {
        // F4's leftover: gain 75 on a black picture is a flat grey around 17 of 255, which reads as a neutral light.
        let r = measure_white_balance(&flat(srgb_decode(17.0 / 255.0) as f32), 0.0);
        assert_eq!((r.confidence, r.reason), (0.0, Reason::Dark), "{r:?}");
        // Brighter than the gate it is a neutral picture again.
        let r = measure_white_balance(&flat(srgb_decode(0.30) as f32), 0.0);
        assert!(r.confidence > 0.99 && r.reason == Reason::Ok && r.error.abs() < 1e-3, "{r:?}");
        // The gate is on the mean, not on the cast: a warm picture as dark is dark too.
        let mut warm = tinted(-0.5);
        for c in &mut warm.cells {
            for v in c.iter_mut().take(3) {
                *v *= 0.02;
            }
        }
        assert_eq!(measure_white_balance(&warm, 0.0).reason, Reason::Dark);
    }

    #[test]
    fn a_picture_of_one_colour_says_so() {
        let red = Stats {
            cells: vec![[0.4, 0.02, 0.02, 1.0]; GRID * GRID],
        };
        let r = measure_white_balance(&red, 0.0);
        assert!(r.confidence < 0.3 && r.reason == Reason::OneColour, "{r:?}");
        let black = Stats {
            cells: vec![[0.0, 0.0, 0.0, 1.0]; GRID * GRID],
        };
        assert_eq!(measure_white_balance(&black, 0.0).reason, Reason::Dark);
    }

    #[test]
    fn a_picture_with_two_answers_says_so() {
        // The owner's desk: at 112.79-113.6 s a bright face beside the walls gives two answers, each sure of itself.
        let mut two = 0;
        for f in real::between(112.79, 113.6) {
            let r = measure_white_balance(&f.stats, 0.0);
            assert!(r.reason == Reason::TwoAnswers, "{:.3} s: {r:?}", f.t);
            assert!(r.light_conf >= 0.9 && r.confidence < 0.3, "{:.3} s: {r:?}", f.t);
            two += 1;
        }
        assert!(two >= 5, "{two} frames");
        // 112.44-112.68 s: the estimator itself has little to go on (skin guard).
        for f in real::between(112.44, 112.68) {
            let r = measure_white_balance(&f.stats, 0.0);
            assert_eq!(r.reason, Reason::OneColour, "{:.3} s: {r:?}", f.t);
        }
        // 40-44 s: a plain reading.
        for f in real::between(40.0, 44.0) {
            let r = measure_white_balance(&f.stats, 0.0);
            assert_eq!(r.reason, Reason::Ok, "{:.3} s: {r:?}", f.t);
        }
        let red = Stats {
            cells: vec![[0.4, 0.02, 0.02, 1.0]; GRID * GRID],
        };
        assert_eq!(measure_white_balance(&red, 0.0).reason, Reason::OneColour);
        let black = Stats {
            cells: vec![[0.0, 0.0, 0.0, 1.0]; GRID * GRID],
        };
        assert_eq!(measure_white_balance(&black, 0.0).reason, Reason::Dark);
    }

    #[test]
    fn a_real_room_is_read_calmly_or_not_at_all() {
        // Frames of the owner's desk at a constant 6261 K (a bright face in a dim room): the neutral walls read about
        // 0. A reading is either held (confidence under 0.3) or within 0.06 of that; it used to be -0.27 with a full
        // confidence for a second.
        let frames = real::between(108.0, 113.7);
        assert!(frames.len() > 40);
        let mut sure = 0;
        for f in &frames {
            let r = measure_white_balance(&f.stats, 0.0);
            assert!(r.confidence < 0.3 || r.error.abs() <= 0.06, "{:.3} s: {r:?}", f.t);
            sure += usize::from(r.confidence >= 0.3);
        }
        assert!(sure * 10 >= frames.len() * 7, "{sure} of {} frames read", frames.len());
        // A lamp coming on is read at once and in full.
        for f in real::between(164.0, 165.4) {
            let r = measure_white_balance(&f.stats, 0.0);
            assert!(r.error < -0.8 && r.confidence > 0.9, "{:.3} s: {r:?}", f.t);
        }
    }

    #[test]
    fn grey_at_target_reads_zero_error() {
        // A picture whose every cell is the linear value of the target brightness is exactly on target.
        let lin = srgb_decode(0.45) as f32;
        let r = measure_exposure(&flat(lin), 0.45);
        assert!(r.error.abs() < 0.01, "{r:?}");
        assert!(r.confidence > 0.99 && r.clip == 0.0, "{r:?}");
    }

    #[test]
    fn error_is_in_stops() {
        // Half the light of the target picture (in linear terms) needs one more stop.
        let lin = srgb_decode(0.45) as f32;
        let r = measure_exposure(&flat(lin * 0.5), 0.45);
        assert!((r.error - 1.0).abs() < 0.02, "{r:?}");
        let r = measure_exposure(&flat(lin * 2.0), 0.45);
        assert!((r.error + 1.0).abs() < 0.02, "{r:?}");
    }

    #[test]
    fn black_picture_has_no_confidence() {
        let r = measure_exposure(&flat(0.0), 0.45);
        assert_eq!((r.error, r.confidence, r.clip), (0.0, 0.0, 0.0));
    }

    #[test]
    fn white_picture_is_clipped_and_wants_less_light() {
        let r = measure_exposure(&flat(1.0), 0.45);
        assert!(r.clip > 0.99, "{r:?}");
        assert!(r.error < -1.5, "{r:?}");
    }

    #[test]
    fn uncovered_cells_are_ignored() {
        let mut s = flat(srgb_decode(0.45) as f32);
        for c in &mut s.cells[..128] {
            *c = [1.0, 1.0, 1.0, 0.0];
        }
        let r = measure_exposure(&s, 0.45);
        assert!(r.error.abs() < 0.01 && r.clip == 0.0, "{r:?}");
    }

    #[test]
    fn centre_counts_more_than_the_corner() {
        assert!(centre_weight(8 * GRID + 8) > 0.7);
        assert!(centre_weight(0) < 0.3);
    }
}
