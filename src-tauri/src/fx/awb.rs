//! The shared white balance estimator (docs/AUTO-WHITE-BALANCE.md 3.1): what light is the picture lit by?
//!
//! One pure function, [`estimate_light`], reads the 16 x 16 statistics image ([`Stats`]) and answers with the colour
//! of the light, a confidence and, when the confidence is low, the reason. Auto Adjust and the device loop both use
//! it, so the two cannot drift apart.
//!
//! The old meter weighted each cell by how saturated it was *before* any correction. Under an orange light every
//! neutral surface is saturated, so the gate threw out the grey wall and kept the blue shirt, which tungsten turns
//! grey. This estimator judges the cells in the **corrected** space instead:
//!
//! 1. Cells are weighted by place in the picture, coverage, brightness (not near black, not near white) and not having
//!    a clipped channel.
//! 2. A shades-of-gray mean (Minkowski `p = 4`, which leans on the brighter surfaces) of those cells is the first
//!    estimate of the light.
//! 3. Three times: correct every cell by the estimate, keep the cells that are near neutral *after* that correction
//!    (saturated objects drop out in both directions) and average them again. The neutral surfaces come back into the
//!    average as soon as the estimate has moved toward the light.
//!    The same re-selection is also run from a second start, the weighted median of the cells instead of the
//!    shades-of-gray mean. Where the two ends differ the picture has two answers (a face brighter than the walls
//!    pulls the mean start, not the median); the estimate stays the first start's (so Auto Adjust is unchanged) and
//!    [`LightEstimate::apart`] and [`LightEstimate::t_alt`] carry the other answer for the device loop, which does
//!    not move on a doubtful picture.
//! 4. The result is read as a light with a blackbody-locus prior: `t = ln(B/R)` (the temperature axis) is kept as it
//!    is, the tint is limited to what real lamps show, and an estimate on the magenta side of the locus (skin, wood,
//!    a red wall are there, lights are not) lowers the confidence instead of being corrected away.
//!
//! The constants are the ones of the model the design is derived from (section 3.1 of the document). Cost: 256 cells
//! times four passes, a few microseconds.

use super::auto_adjust::{Stats, GRID};

/// Also used by `auto_controls::sim` (W3) for the device loop's coloured scenes.
#[cfg(test)]
pub(crate) mod scenes;
/// Frames recorded from a real camera, for the stability tests here and in `auto_controls`.
#[cfg(test)]
pub(crate) mod real;
#[cfg(test)]
mod tests;

/// Cells whose coverage is below this are skipped (no picture there).
const MIN_COVERAGE: f64 = 0.02;
/// The Minkowski exponent of the mean (shades of gray) is 4: `mean` writes it as two squares and two roots.
/// Re-gating passes in the corrected space.
const PASSES: usize = 3;
/// The re-gate: a cell is out of the average once its distance from neutral (in `ln` units, after the correction)
/// reaches the second edge, and fully in below the first.
const GATE: (f64, f64) = (0.35, 0.80);
/// A pass whose weights add up to no more than this keeps the previous estimate.
const MIN_WEIGHT: f64 = 1e-6;
/// The slope of the blackbody locus in `(t, g)`: `g_L = LOCUS_SLOPE * t`, a line through the origin (a locus slides
/// along itself, so the camera's own Kelvin is not needed).
const LOCUS_SLOPE: f64 = -0.14;
/// The locus bends: the tolerance is this times `|t|`.
const LOCUS_TOL: f64 = 0.04;
/// The tint a light may have, off the locus, before it is clamped.
const TINT_MAX: f64 = 0.06;
/// The skin and surface guard: confidence fades out as the estimate lies this far on the magenta side of the locus
/// (beyond the tolerance).
const GUARD: (f64, f64) = (0.08, 0.16);
/// Confidence from the share of the picture that agrees on the light.
const SHARE: (f64, f64) = (0.02, 0.12);
/// Brightness gate (coded luma) shared with the old meter: not near black, not near white.
const DARK: (f64, f64) = (0.04, 0.15);
const BRIGHT: (f64, f64) = (0.85, 0.97);
/// A cell with a channel above this is clipped (the channel lies about the colour).
const CLIP: (f64, f64) = (0.90, 0.98);
/// Below this share of lit picture the reason is [`Reason::Dark`].
const LIT_MIN: f64 = 0.12;
/// Below this confidence the reason is [`Reason::OneColour`] (the loops hold at it).
pub const LOW_CONFIDENCE: f64 = 0.3;
/// Logs are floored here.
const LOG_FLOOR: f64 = 1e-6;

/// Why the confidence is low.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// The confidence is not low.
    Ok,
    /// Too little of the picture is lit: nothing to measure.
    Dark,
    /// The picture is mostly one colour (a face, a wall) that does not look like a cast.
    OneColour,
}

/// What [`estimate_light`] concludes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LightEstimate {
    /// The temperature axis of the light, `ln(B/R)` relative to a neutral picture: negative is orange (warm light),
    /// positive is blue. `0` for a picture that needs no correction.
    pub t: f64,
    /// The tint of the light, `ln G - (ln R + ln B) / 2`: on the blackbody locus plus the tint kept (at most
    /// `0.06` plus a tolerance of `0.04 |t|`).
    pub g: f64,
    /// `0..=1`: how much the picture looks like a lit scene with a cast rather than one colour, or nothing.
    pub conf: f64,
    /// The share of the picture (by centre weight and coverage) the final average rests on.
    pub share: f64,
    /// How far (in `t`) the answer lies from the one a second start gives (the median of the cells instead of the
    /// shades-of-gray mean): `0` when the picture has one answer, large when it has two (a bright face beside the
    /// walls, two lights). The estimate itself is the first start's; the device loop reads this as doubt.
    pub apart: f64,
    /// The `t` the second start's answer has (`t` itself when there is none).
    pub t_alt: f64,
    /// `Ok`, or why `conf` is below [`LOW_CONFIDENCE`].
    pub reason: Reason,
    /// The light as linear RGB with the luma 1 (what `t` and `g` say, as colour).
    pub light: [f64; 3],
}

impl LightEstimate {
    /// Nothing to measure.
    fn none(reason: Reason) -> LightEstimate {
        LightEstimate {
            t: 0.0,
            g: 0.0,
            conf: 0.0,
            share: 0.0,
            apart: 0.0,
            t_alt: 0.0,
            reason,
            light: [1.0; 3],
        }
    }

    /// The gains that take this light to `aim` (see [`gains`]), scaled by the confidence.
    pub fn gains(&self, aim: [f32; 3], limits: (f64, f64)) -> [f64; 3] {
        gains(self.light, aim, self.conf, limits)
    }
}

fn smoothstep(e0: f64, e1: f64, x: f64) -> f64 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn srgb_encode(l: f64) -> f64 {
    let c = l.clamp(0.0, 1.0);
    if c <= 0.003_130_8 {
        12.92 * c
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

fn luma(c: [f64; 3]) -> f64 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

/// The weight of cell `i` for its place in the picture (the same as Auto Adjust's: 1 in the middle, 0.4 at the edge).
fn centre_weight(i: usize) -> f64 {
    let u = ((i % GRID) as f64 + 0.5) / GRID as f64 * 2.0 - 1.0;
    let v = ((i / GRID) as f64 + 0.5) / GRID as f64 * 2.0 - 1.0;
    1.0 / (1.0 + 1.5 * (u * u + v * v))
}

/// `(t, g)` of a linear colour: the blue-red axis and the green-magenta axis, in natural logs.
fn tg(rgb: [f64; 3]) -> (f64, f64) {
    let [r, g, b] = rgb.map(|c| c.max(LOG_FLOOR).ln());
    (b - r, g - 0.5 * (r + b))
}

/// The shades-of-gray mean of the cells with weights `w`, normalised to luma 1. `None` when it has no luma.
fn mean(cells: &[[f64; 3]], w: &[f64]) -> Option<[f64; 3]> {
    let total: f64 = w.iter().sum();
    if total <= 0.0 {
        return None;
    }
    let mut acc = [0.0f64; 3];
    for (c, w) in cells.iter().zip(w) {
        if *w > 0.0 {
            for k in 0..3 {
                let c2 = c[k] * c[k];
                acc[k] += w * c2 * c2; // c^P, P = 4
            }
        }
    }
    let m = acc.map(|a| (a / total).sqrt().sqrt());
    let y = luma(m);
    (y > 0.0 && y.is_finite()).then(|| m.map(|c| c / y))
}

/// The value below which half of the weight `w` lies.
fn weighted_median(mut v: Vec<(f64, f64)>) -> f64 {
    v.sort_by(|a, b| a.0.total_cmp(&b.0));
    let half = 0.5 * v.iter().map(|x| x.1).sum::<f64>();
    let mut acc = 0.0;
    for (x, w) in &v {
        acc += w;
        if acc >= half {
            return *x;
        }
    }
    v.last().map_or(0.0, |x| x.0)
}

/// The neutral re-selection (step 3): from the estimate `est`, correct every cell, keep the near neutral ones and
/// average them again, `PASSES` times. Returns the estimate and the weights of the last pass.
///
/// `tgs` are the cells' `(t, g)`: the logs are linear, so a cell corrected by `est` has `(t - t_est, g - g_est)`.
fn reselect(rgb: &[[f64; 3]], tgs: &[(f64, f64)], base: &[f64], start: [f64; 3]) -> ([f64; 3], Vec<f64>) {
    let (mut est, mut w) = (start, base.to_vec());
    for _ in 0..PASSES {
        let (te, ge) = tg(est);
        let next: Vec<f64> = tgs
            .iter()
            .zip(base)
            .map(|((t, g), b)| {
                if *b <= 0.0 {
                    return 0.0;
                }
                let d = ((t - te).powi(2) + (2.0 * (g - ge)).powi(2)).sqrt();
                b * (1.0 - smoothstep(GATE.0, GATE.1, d))
            })
            .collect();
        let sum: f64 = next.iter().sum();
        w = next;
        if sum <= MIN_WEIGHT {
            break;
        }
        match mean(rgb, &w) {
            Some(e) => est = e,
            None => break,
        }
    }
    (est, w)
}

/// The second start of the neutral re-selection: the weighted median of the cells' `(t, g)`, as a light of luma 1.
/// The shades-of-gray mean (the first start) leans on the brightest cells, and a face or a window that is 2-3 times
/// brighter than the walls pulls the start to its own colour; the re-selection then has two stable answers (the
/// walls, or the face with half of the walls) and a picture that did not change flipped between them frame to frame
/// (a real 0 / -0.27 / -0.55 in `t`, docs/AUTO-WHITE-BALANCE.md 9). The median follows what most of the picture
/// agrees on. `None` when no cell has weight.
fn median_start(tgs: &[(f64, f64)], w: &[f64]) -> Option<[f64; 3]> {
    let (mut ts, mut gs) = (Vec::new(), Vec::new());
    for ((t, g), w) in tgs.iter().zip(w) {
        if *w > 0.0 {
            ts.push((*t, *w));
            gs.push((*g, *w));
        }
    }
    if ts.is_empty() {
        return None;
    }
    let (t, g) = (weighted_median(ts), weighted_median(gs));
    let light = [(-t / 2.0).exp(), g.exp(), (t / 2.0).exp()];
    let y = luma(light);
    (y > 0.0 && y.is_finite()).then(|| light.map(|c| c / y))
}

/// The cells' gate factors in the last pass (`0..=1`; `0` for a cell that is skipped or whose base weight is 0), for
/// the tests that check *which* cells the estimate rests on.
#[cfg(test)]
pub(crate) fn estimate_with_gate(stats: &Stats) -> (LightEstimate, Vec<f64>) {
    estimate_core(stats)
}

/// Estimate the light the picture in `stats` is lit by. Never panics: a picture with nothing to measure gives
/// `conf = 0` and [`Reason::Dark`] or [`Reason::OneColour`].
pub fn estimate_light(stats: &Stats) -> LightEstimate {
    estimate_core(stats).0
}

fn estimate_core(stats: &Stats) -> (LightEstimate, Vec<f64>) {
    let n = stats.cells.len();
    let mut rgb = Vec::with_capacity(n);
    let mut base = Vec::with_capacity(n);
    let (mut total, mut lit) = (0.0, 0.0);
    for (i, c) in stats.cells.iter().enumerate() {
        let c64 = [f64::from(c[0]), f64::from(c[1]), f64::from(c[2])];
        rgb.push(c64);
        let usable = f64::from(c[3]) >= MIN_COVERAGE && c.iter().all(|v| v.is_finite());
        if !usable {
            base.push(0.0);
            continue;
        }
        let cw = centre_weight(i) * f64::from(c[3]);
        total += cw;
        let y = srgb_encode(luma(c64));
        let lit_w = smoothstep(DARK.0, DARK.1, y);
        lit += cw * lit_w;
        let mx = c64[0].max(c64[1]).max(c64[2]);
        base.push(
            cw * lit_w * (1.0 - smoothstep(BRIGHT.0, BRIGHT.1, y)) * (1.0 - smoothstep(CLIP.0, CLIP.1, mx)),
        );
    }
    let gates = |w: &[f64]| -> Vec<f64> {
        w.iter().zip(&base).map(|(w, b)| if *b > 0.0 { w / b } else { 0.0 }).collect()
    };
    if total <= 0.0 {
        return (LightEstimate::none(Reason::Dark), vec![0.0; n]);
    }
    let dark_or_one = if lit / total < LIT_MIN { Reason::Dark } else { Reason::OneColour };
    let Some(start) = mean(&rgb, &base) else {
        return (LightEstimate::none(dark_or_one), vec![0.0; n]);
    };
    let tgs: Vec<(f64, f64)> = rgb.iter().map(|c| tg(*c)).collect();
    let (est, w) = reselect(&rgb, &tgs, &base, start);
    // A second start, from the median (see `median_start`): how far its answer lies from this one.
    let t_alt = median_start(&tgs, &base).map_or(tg(est).0, |start2| tg(reselect(&rgb, &tgs, &base, start2).0).0);
    let apart = (tg(est).0 - t_alt).abs();
    let share = w.iter().sum::<f64>() / total;
    let conf0 = smoothstep(SHARE.0, SHARE.1, share);

    // The locus prior.
    let (t, g) = tg(est);
    let g_l = LOCUS_SLOPE * t;
    let tol = LOCUS_TOL * t.abs();
    let dg = g - g_l;
    let conf = conf0 * (1.0 - smoothstep(GUARD.0, GUARD.1, (-dg - tol).max(0.0)));
    let dg_c = dg.clamp(-(TINT_MAX + tol), TINT_MAX + tol);
    let g_out = g_l + dg_c;
    let light = [(-t / 2.0).exp(), g_out.exp(), (t / 2.0).exp()];
    let y = luma(light);
    let light = light.map(|c| c / y);
    let reason = if lit / total < LIT_MIN {
        Reason::Dark
    } else if conf < LOW_CONFIDENCE {
        Reason::OneColour
    } else {
        Reason::Ok
    };
    let gate = gates(&w);
    (LightEstimate { t, g: g_out, conf, share, apart, t_alt, reason, light }, gate)
}

/// The per-channel gains (linear light) that take an estimated `light` (linear RGB, luma 1) to `aim` (the colour a
/// neutral surface should get, see `auto_adjust::white_aim`), keeping the luma, scaled by `conf` in the log domain
/// (a picture that does not look like a cast is corrected only as far as the evidence goes), and limited to
/// `limits = (lo, hi)` per channel. The luma is kept again after the first limit, then the limit applies again.
/// A correction smaller than 1e-9 in the log is exactly 1.
pub fn gains(light: [f64; 3], aim: [f32; 3], conf: f64, limits: (f64, f64)) -> [f64; 3] {
    let (lo, hi) = limits;
    let y = luma(light);
    let g0 = [0, 1, 2].map(|k| f64::from(aim[k]) / light[k]);
    let keep = y / luma([g0[0] * light[0], g0[1] * light[1], g0[2] * light[2]]);
    let conf = conf.clamp(0.0, 1.0);
    let g = g0.map(|g| (conf * (g * keep).ln()).exp());
    let raw = g.map(|g| g.clamp(lo, hi));
    let keep = y / luma([raw[0] * light[0], raw[1] * light[1], raw[2] * light[2]]);
    let out = raw.map(|g| (g * keep).clamp(lo, hi));
    if out.iter().all(|g| g.ln().abs() < 1e-9) {
        [1.0; 3]
    } else {
        out
    }
}
