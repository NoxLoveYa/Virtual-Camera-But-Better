//! The exposure meter (docs/AUTO-CONTROLS.md 2.4): from the 16x16 statistics image of the camera picture (before any
//! filter) to "how many stops of light does the picture need".
//!
//! Pure maths, no state. `Stats` is `fx::auto_adjust::Stats` (it is `pub` there); the small helpers below are f64
//! copies of the private ones in `fx/auto_adjust.rs` (`srgb_encode`, `luma`, `smoothstep`, `centre_weight`), copied
//! because that file is changed in parallel. Phase A3 unifies them.
//!
//! The white balance meter (gray world on the trusted cells, 2.4 "White balance error") belongs in this file in
//! phase A2, next to [`measure_exposure`].

use crate::fx::auto_adjust::Stats;

/// Cells per side of the statistics image.
pub const GRID: usize = 16;

/// Cells with less coverage than this are left out (as Auto Adjust does).
const MIN_COVERAGE: f32 = 0.02;
/// The error is searched in `log2 k` within this many stops either way.
const SEARCH_STOPS: f64 = 4.0;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(lin: f32) -> Stats {
        Stats {
            cells: vec![[lin, lin, lin, 1.0]; GRID * GRID],
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
