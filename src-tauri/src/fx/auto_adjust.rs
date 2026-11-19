//! Auto Adjust (`auto_adjust`, plan 6.4, P16): Better Cam's own software "auto mode". It has no OBS counterpart.
//!
//! It measures the picture on the GPU and corrects brightness (auto exposure), colour cast (auto white balance),
//! contrast and saturation toward targets the user sets, in a shader, smoothly over time, for cameras whose built-in
//! auto is poor.
//!
//! # Measuring
//! Each frame the filter's *input* shrinks on the GPU through a chain of passes ([`stats_downsample.hlsl`], at most 8
//! texels per output pixel per axis and pass, so 1080p goes 240x135, 30x17, 16x16) to a 16x16 image of linear-light
//! mean colours, alpha weighted (transparent areas count for nothing). That image is copied into a staging ring
//! ([`ReadbackRing`]) and mapped on a later frame without waiting: the CPU never stalls on the GPU, the measurement
//! simply arrives one or two frames late. The statistics come from the filter's input, before its own gains, so the
//! control loop has no feedback: it cannot oscillate, and a change by the camera's own auto exposure is just
//! followed.
//!
//! # Brightness
//! The brightness of a cell is the Rec. 709 luma of its linear colour (`0.2126 R + 0.7152 G + 0.0722 B`) encoded with
//! the sRGB transfer function, the scale the pipeline's pixel codes use: `0.45` (the default `target`) is about
//! 18 % grey, a well exposed mid-tone. The brightness of the picture is the weighted mean of the cell brightnesses.
//! The weights favour the centre (`1 / (1 + 1.5 r^2)`, the edge of the picture counts 0.4 and a corner 0.25), cells
//! that are near black count less and less (they say nothing about the scene, and gaining them up only shows
//! noise) and near white cells count a little (they are clipped, so the measurement is a lower bound).
//!
//! # Exposure
//! The wanted gain `g` is the one that brings the *predicted output brightness* to the target: the same shoulder as
//! the shader applies is run on the 256 cells and `g` is found by bisection in the log domain. `g` stays between
//! `0.25` and `4`, so a covered lens (every cell black, which carries no information) holds the gain instead of
//! turning noise into picture. A dead band of `0.012` around the target holds the gain too, so it does not hunt on
//! sensor noise. Highlights roll off with a soft shoulder (see `auto_adjust.hlsl`) instead of clipping.
//!
//! # White balance
//! The light the picture is lit by is estimated by [`awb::estimate_light`], the estimator the device loop uses too
//! (docs/AUTO-WHITE-BALANCE.md 3.1): a gray world that judges the cells *after* the correction (so a grey wall under
//! an orange lamp counts and a blue shirt that tungsten turned grey does not), with a blackbody prior that keeps the
//! tint to what lamps show and lowers the confidence for what looks like skin or a coloured wall instead of a cast.
//! The gains take that light to the aim colour, keep the luma, and are scaled by the confidence in the log domain, so
//! a picture of one colour is corrected only as far as the evidence goes. When the estimator has two answers (a bright
//! face beside dim walls: its two starts end `apart`) the gains aim at the light between them and the confidence is
//! multiplied by `1 - smoothstep(0.02, 0.06, apart)`, so the picture does not shift when the face moves
//! ([`awb::LightEstimate::steady_gains`], docs/AUTO-WHITE-BALANCE.md 9.5). Each is limited to `0.25 ..= 4` (two stops:
//! a 2700 K lamp on a camera at 4600 K needs blue x 3.3). The wanted gains come from the input alone, every frame,
//! without state; a dead band of 0.8 % against the held gains stops hunting. A picture with nothing to measure (too
//! dark, clear) holds the gains.
//!
//! # Warmth and tint
//! `temperature` ("Warmth") and `tint` move where the white balance aims: the estimated light is taken to the aim
//! colour `t` (linear RGB with the luma 1, see [`white_aim`]) instead of to gray. `0 / 0` is the aim `[1, 1, 1]`.
//! Warmth is a colour temperature of the light the neutral surface
//! seems to be lit by, linear in mireds from 5500 K (0) to 4000 K (+1, warmer: a neutral ends orange, about
//! R : G : B = 1.25 : 0.96 : 0.63) or 7000 K (-1, cooler: it ends blue), blackbody colours in linear sRGB relative to
//! 5500 K. Tint multiplies red and blue by `e^d` and green by `e^-2d` with `d = 0.045 tint`, so +1 leaves green 13 %
//! below red and blue (magenta) and -1 13 % above (green). Both aims are multiplied and renormalised to luma 1. With
//! `white_balance` off they do nothing.
//!
//! # Contrast
//! Order of the stages (the shader and every prediction here use it): exposure, white balance, contrast, saturation.
//! The *contrast* of the picture is the spread of the brightness of its cells between the 10th and 90th percentile,
//! the weighted percentiles of the 256 cell brightnesses after the exposure and white balance gains (weights as for
//! the exposure: centre weight times coverage). The scale is the brightness scale of `target`: 0 is a flat gray
//! picture, 0.5 a normal well lit scene, 0.8 is what cells spread evenly over black to white give. (The cells are
//! 16 x 16 means, so they fall short of the extremes of single pixels.) The slider is `target_contrast`.
//!
//! The correction is a soft S-curve around a pivot (`auto_adjust.hlsl`) with the slope `k` at the pivot, `k` between
//! `0.5` and `2`; 0 and 1 stay put and nothing clips. The shader applies it to each encoded channel, so the prediction
//! does the same to each cell and reads the brightness of the result (for gray cells that is the curve of the
//! brightness). The pivot is the one for which the mean brightness of the cells does not change, found by regula
//! falsi (for a picture that is symmetrical around its mean it is the mean). The spread after the curve is measured
//! on the predicted cells and `k` comes by bisection, with a dead band of `0.008` on the spread. A flat picture
//! (spread below 0.02) or a black or white one has nothing to measure and holds the gain.
//!
//! The prediction works on cell means: detail inside a cell (hard edges, texture) makes the real effect of the curve
//! weaker than predicted (about two thirds of it on a picture of flat areas with hard edges, measured live), so
//! such a picture settles somewhat short of the target. The loop does not look at its own output, by design.
//!//! # Saturation
//! The colourfulness of a cell is `max(R, G, B) - min(R, G, B)` of its encoded colour after the stages before
//! (so the contrast curve is included); the colourfulness of the picture is the weighted mean over the cells that are
//! neither near black nor near white. 0 is gray, 0.2 a normal scene, 0.5 very vivid. The shader scales the
//! distance of the encoded colour from its luma by the gain `s`, which scales the colourfulness by exactly `s` (up to
//! clipping), so the wanted gain is `target / measured`, limited to `0.5 ..= 2`, with a dead band of 4 % around the
//! target. Below a colourfulness of 0.02 nothing is held to be measured: the gain stays where it is (it cannot
//! make colour out of gray anyway: a gray pixel does not move).
//!
//! # Time
//! The filter keeps the gains as logarithms and moves them toward the wanted values with a first-order low-pass
//! whose time constant is `0.15 s / speed` (`speed` 0.3, the default: 0.5 s; 1: 0.15 s; 0.05: 3 s). The step of
//! each frame is `1 - exp(-dt / tau)` of the remaining distance with `dt` the real time since the previous frame
//! (at most 0.25 s), so 15, 30 and 60 fps behave the same. The state lives in the filter instance: a new instance
//! starts neutral and a size change keeps the gains. Turning a correction off moves its gains back to 1 the same
//! way. `strength` scales the gains in the log domain (`g^strength`): 0 is the exact identity (the filter copies its
//! input), 0.5 is half way. Alpha is left unchanged.
//!
//! The clock is the filter's own `Instant`; tests inject one.

use std::time::Instant;

use windows::Win32::Graphics::Direct3D11::{
    ID3D11PixelShader, ID3D11RenderTargetView, ID3D11ShaderResourceView, ID3D11Texture2D,
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_R16G16B16A16_UNORM, DXGI_SAMPLE_DESC};

use super::awb;
use super::geometry::{pack_f32, Program};
use super::{pb, pf, GpuFilter, Params};
use crate::gpu::readback::{MappedPlane, PlaneDesc, ReadbackRing};
use crate::gpu::{ConstantBuffer, GpuResult, Pass, Renderer, RgbaTexture, Space, Views};

#[cfg(test)]
mod tests;

pub const KIND: &str = "auto_adjust";

/// The settings the filter reads.
pub const KEYS: &[&str] = &[
    "exposure",
    "white_balance",
    "target",
    "temperature",
    "tint",
    "contrast",
    "target_contrast",
    "saturation",
    "target_saturation",
    "strength",
    "speed",
];

const MAIN_HLSL: &str = include_str!("../gpu/shaders/auto_adjust.hlsl");
pub(crate) const STATS_HLSL: &str = include_str!("../gpu/shaders/stats_downsample.hlsl");

/// Cells per side of the statistics image.
pub(crate) const GRID: usize = 16;
const CELLS: usize = GRID * GRID;

/// The exposure gain limits (linear): two stops either way.
const MIN_GAIN: f64 = 0.25;
const MAX_GAIN: f64 = 4.0;
/// The white balance gain limits per channel (linear): two stops either way (decision D3, so that a 2700 K light on a
/// camera at 4600 K, or a 2200 K bulb with the device at its 2800 K floor, can be corrected fully).
const MIN_WB: f64 = 0.25;
const MAX_WB: f64 = 4.0;
/// Dead band of the exposure loop, in brightness units (the sRGB-coded scale of `target`).
const EXPOSURE_BAND: f32 = 0.012;
/// Dead band of the white balance loop, in natural-log units of the gain (0.8 %).
const WB_BAND: f64 = 0.008;
/// The contrast curve's slope at its pivot: from half to double.
const MIN_CONTRAST: f64 = 0.5;
const MAX_CONTRAST: f64 = 2.0;
/// Dead band of the contrast loop, in units of the spread (the brightness scale).
const CONTRAST_BAND: f32 = 0.008;
/// The saturation gain limits.
const MIN_SAT: f64 = 0.5;
const MAX_SAT: f64 = 2.0;
/// Dead band of the saturation loop, in natural-log units of the gain (4 %).
const SAT_BAND: f64 = 0.04;
/// The colour temperature of the neutral aim (warmth 0) and of the two ends of the warmth slider, in kelvin.
const REF_KELVIN: f64 = 5500.0;
const WARM_KELVIN: f64 = 4000.0;
const COOL_KELVIN: f64 = 7000.0;
/// Tint 1 multiplies red and blue by `e^TINT_SCALE` and green by `e^-2 TINT_SCALE`.
const TINT_SCALE: f32 = 0.045;
/// The default targets (the values of `schema.ts`).
const DEF_TARGET_CONTRAST: f32 = 0.4;
const DEF_TARGET_SATURATION: f32 = 0.2;
/// `tau = TAU_SCALE / speed` seconds.
const TAU_SCALE: f64 = 0.15;
/// The longest step of the smoothing: a stalled frame does not make a jump.
const MAX_DT: f64 = 0.25;
/// A gap this long (the filter was off or the engine paused) drops the old statistics.
const STALE_GAP: f64 = 1.0;
/// Below this, a gain that is on its way back to 1 lands on 1 exactly (0.1 %).
const SNAP: f64 = 1e-3;

type Clock = Box<dyn FnMut() -> f64 + Send>;

/// Seconds since the filter was made.
fn real_clock() -> Clock {
    let t0 = Instant::now();
    Box::new(move || t0.elapsed().as_secs_f64())
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Settings {
    exposure: bool,
    white_balance: bool,
    target: f32,
    temperature: f32,
    tint: f32,
    contrast: bool,
    target_contrast: f32,
    saturation: bool,
    target_saturation: f32,
    strength: f32,
    speed: f32,
}

fn settings(p: &Params) -> Settings {
    Settings {
        exposure: pb(p, "exposure", true),
        white_balance: pb(p, "white_balance", true),
        target: pf(p, "target", 0.45, 0.2, 0.8),
        temperature: pf(p, "temperature", 0.0, -1.0, 1.0),
        tint: pf(p, "tint", 0.0, -1.0, 1.0),
        contrast: pb(p, "contrast", false),
        target_contrast: pf(p, "target_contrast", DEF_TARGET_CONTRAST, 0.1, 0.9),
        saturation: pb(p, "saturation", false),
        target_saturation: pf(p, "target_saturation", DEF_TARGET_SATURATION, 0.05, 0.5),
        strength: pf(p, "strength", 1.0, 0.0, 1.0),
        speed: pf(p, "speed", 0.3, 0.05, 1.0),
    }
}

/// The time constant of the smoothing in seconds.
pub fn time_constant(speed: f32) -> f64 {
    TAU_SCALE / f64::from(speed.clamp(0.05, 1.0))
}

// ---------------------------------------------------------------------------------------------------------------
// The maths (CPU side, pure)

/// The sRGB transfer function (linear to coded), clamped to `0..=1`.
fn srgb_encode(l: f32) -> f32 {
    let c = l.clamp(0.0, 1.0);
    if c <= 0.003_130_8 {
        12.92 * c
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

/// Rec. 709 luma of a linear colour.
fn luma(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// The weight of cell `i` for its place in the picture: 1 in the middle, 0.4 at the edge of the picture.
fn centre_weight(i: usize) -> f32 {
    let u = ((i % GRID) as f32 + 0.5) / GRID as f32 * 2.0 - 1.0;
    let v = ((i / GRID) as f32 + 0.5) / GRID as f32 * 2.0 - 1.0;
    1.0 / (1.0 + 1.5 * (u * u + v * v))
}

/// Where the highlight shoulder starts (a linear level) for an exposure gain. 1 for a gain of 1 or less (no
/// shoulder, the exact identity at 1), 0.7 from a gain of 1.5 up, and linear in between. `auto_adjust.hlsl`
/// receives this value.
pub fn knee(gain: f32) -> f32 {
    if gain <= 1.0 {
        1.0
    } else {
        1.0 - 0.3 * ((gain - 1.0) / 0.5).min(1.0)
    }
}

/// One linear channel `c` after the exposure gain with the highlight shoulder: what `auto_adjust.hlsl` does.
pub fn shape(c: f32, gain: f32) -> f32 {
    let v = c * gain;
    let k = knee(gain);
    if k >= 1.0 || v <= k {
        v
    } else {
        let room = 1.0 - k;
        k + room * (1.0 - (-(v - k) / room).exp())
    }
}

/// The 16x16 image of linear mean colours (`[r, g, b, coverage]` per cell, row by row).
#[derive(Clone, Debug, PartialEq)]
pub struct Stats {
    pub cells: Vec<[f32; 4]>,
}

impl Stats {
    /// Read the staging copy of the last stage: `R16G16B16A16_UNORM`, 16 x 16.
    pub(crate) fn from_plane(plane: &MappedPlane<'_>) -> Stats {
        let mut cells = Vec::with_capacity(CELLS);
        for y in 0..GRID {
            let row = plane.row(y);
            for x in 0..GRID {
                let px = &row[x * 8..x * 8 + 8];
                let ch =
                    |k: usize| f32::from(u16::from_le_bytes([px[2 * k], px[2 * k + 1]])) / 65535.0;
                cells.push([ch(0), ch(1), ch(2), ch(3)]);
            }
        }
        Stats { cells }
    }
}

/// The wanted exposure gain as a natural logarithm, for the measured `stats`, a `target` brightness and the gain
/// `ln_cur` the filter holds now. A picture with nothing to measure (every cell near black or clear) gives `ln_cur`.
pub fn exposure_target(stats: &Stats, target: f32, ln_cur: f64) -> f64 {
    let mut total = 0.0f32;
    let mut lit = 0.0f32;
    let mut weight_sum = 0.0f32;
    let mut cells: Vec<([f32; 3], f32)> = Vec::with_capacity(CELLS);
    for (i, c) in stats.cells.iter().enumerate() {
        if c[3] < 0.02 {
            continue;
        }
        let cw = centre_weight(i) * c[3];
        let rgb = [c[0], c[1], c[2]];
        let y = srgb_encode(luma(rgb));
        let not_black = smoothstep(0.02, 0.08, y);
        let not_white = 0.15 + 0.85 * (1.0 - smoothstep(0.93, 0.99, y));
        total += cw;
        lit += cw * not_black;
        let w = cw * not_black * not_white;
        if w > 0.0 {
            cells.push((rgb, w));
            weight_sum += w;
        }
    }
    if total <= 0.0 || weight_sum <= 0.0 {
        return ln_cur;
    }
    let confidence = f64::from(smoothstep(0.03, 0.12, lit / total));
    if confidence <= 0.0 {
        return ln_cur;
    }
    // The brightness the picture would have with the exposure gain e^ln_g.
    let brightness = |ln_g: f64| -> f32 {
        let g = ln_g.exp() as f32;
        let sum: f32 = cells
            .iter()
            .map(|(rgb, w)| w * srgb_encode(luma(rgb.map(|c| shape(c, g)))))
            .sum();
        sum / weight_sum
    };
    let error = brightness(ln_cur) - target;
    if error.abs() <= EXPOSURE_BAND {
        return ln_cur;
    }
    // Aim at the edge of the dead band, so the gain stops as soon as it is inside.
    let aim = target + error.clamp(-EXPOSURE_BAND, EXPOSURE_BAND);
    let (mut lo, mut hi) = (MIN_GAIN.ln(), MAX_GAIN.ln());
    let want = if brightness(hi) <= aim {
        hi
    } else if brightness(lo) >= aim {
        lo
    } else {
        for _ in 0..28 {
            let mid = 0.5 * (lo + hi);
            if brightness(mid) < aim {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        0.5 * (lo + hi)
    };
    ln_cur + confidence * (want - ln_cur)
}

/// The linear sRGB colour of a blackbody of `kelvin` with the luminance 1 (Kim et al.'s cubic spline of the Planckian
/// locus, 1667 K to 25000 K).
pub(crate) fn blackbody_rgb(kelvin: f64) -> [f64; 3] {
    let t = kelvin.clamp(1667.0, 25000.0);
    let (t2, t3) = (t * t, t * t * t);
    let x = if t <= 4000.0 {
        -0.266_123_9e9 / t3 - 0.234_358_9e6 / t2 + 0.877_695_6e3 / t + 0.179_910
    } else {
        -3.025_846_9e9 / t3 + 2.107_037_9e6 / t2 + 0.222_634_7e3 / t + 0.240_390
    };
    let y = if t <= 2222.0 {
        -1.106_381_4 * x * x * x - 1.348_110_20 * x * x + 2.185_558_32 * x - 0.202_196_83
    } else if t <= 4000.0 {
        -0.954_947_6 * x * x * x - 1.374_185_93 * x * x + 2.091_370_15 * x - 0.167_488_67
    } else {
        3.081_758_0 * x * x * x - 5.873_386_70 * x * x + 3.751_129_97 * x - 0.370_014_83
    };
    let (cx, cz) = (x / y, (1.0 - x - y) / y);
    [
        3.2406 * cx - 1.5372 - 0.4986 * cz,
        -0.9689 * cx + 1.8758 + 0.0415 * cz,
        0.0557 * cx - 0.2040 + 1.0570 * cz,
    ]
}

/// The colour (linear RGB with the luma 1) the white balance makes a neutral surface, for the settings `temperature`
/// ("Warmth") and `tint`, both `-1 ..= 1`. `0, 0` is exactly `[1, 1, 1]`: gray.
///
/// Warmth is a blackbody colour temperature, linear in mireds, from 5500 K at 0 to 4000 K at +1 and 7000 K at -1; the
/// aim is the colour of that blackbody relative to 5500 K, so +1 gives about `[1.25, 0.96, 0.63]` (orange) and -1 about
/// `[0.87, 1.01, 1.28]` (blue). Tint scales red and blue by `e^d` and green by `e^-2d`, `d = 0.045 tint` (+1: magenta,
/// -1: green). The two are multiplied and the result is normalised to luma 1.
pub fn white_aim(temperature: f32, tint: f32) -> [f32; 3] {
    if temperature == 0.0 && tint == 0.0 {
        return [1.0; 3];
    }
    let mut aim = [1.0f64; 3];
    if temperature != 0.0 {
        let t = f64::from(temperature.clamp(-1.0, 1.0));
        let end = if t > 0.0 { WARM_KELVIN } else { COOL_KELVIN };
        let (m0, m1) = (1e6 / REF_KELVIN, 1e6 / end);
        let kelvin = 1e6 / (m0 + t.abs() * (m1 - m0));
        let (c, r) = (blackbody_rgb(kelvin), blackbody_rgb(REF_KELVIN));
        aim = [c[0] / r[0], c[1] / r[1], c[2] / r[2]];
    }
    let d = f64::from(TINT_SCALE * tint.clamp(-1.0, 1.0));
    aim = [aim[0] * d.exp(), aim[1] * (-2.0 * d).exp(), aim[2] * d.exp()];
    let y = 0.2126 * aim[0] + 0.7152 * aim[1] + 0.0722 * aim[2];
    aim.map(|c| (c / y) as f32)
}

/// The wanted white balance gains (natural logarithms, red green blue) for the measured `stats`, given the gains
/// `ln_cur` held now and the colour `aim` a neutral surface should get (see [`white_aim`]; `[1; 3]` is gray). The
/// light is estimated by the shared [`awb::estimate_light`]; the gains take the light between its two answers to
/// `aim`, scaled by the estimate's confidence (less when the answers differ) in the log domain and limited to `MIN_WB ..= MAX_WB` per channel. They are computed from the input
/// alone every frame (no state: the same picture gives the same target); `ln_cur` only sets the dead band. A picture
/// with nothing to measure (too dark, clear, or every cell clipped) gives `ln_cur`.
pub fn white_balance_target(stats: &Stats, ln_cur: [f64; 3], aim: [f32; 3]) -> [f64; 3] {
    let est = awb::estimate_light(stats);
    if est.reason == awb::Reason::Dark || est.share <= 0.0 {
        return ln_cur;
    }
    let wanted = est.steady_gains(aim, (MIN_WB, MAX_WB)).map(f64::ln);
    let mut out = ln_cur;
    for k in 0..3 {
        let delta = wanted[k] - ln_cur[k];
        out[k] = ln_cur[k] + delta.signum() * (delta.abs() - WB_BAND).max(0.0);
    }
    out
}

/// The contrast curve of `auto_adjust.hlsl` on one encoded value `x`: 0, 1 and `m` stay, the slope at `m` is `k`.
/// Each side of `m` is stretched to `u` in `[-1, 1]` and bent by `k u + (1 - k) u |u|`.
pub fn contrast_curve(x: f32, k: f32, m: f32) -> f32 {
    let (u, room) = if x < m {
        ((x - m) / m, m)
    } else {
        ((x - m) / (1.0 - m), 1.0 - m)
    };
    let a = u.abs();
    m + u * (a + k * (1.0 - a)) * room
}

/// A cell as the shader leaves it after the exposure and white balance gains, before contrast and saturation.
struct Tone {
    /// The encoded colour.
    coded: [f32; 3],
    /// Its brightness (encoded luma of the linear colour).
    b: f32,
    /// Centre weight times coverage.
    w: f32,
}

/// The cells of `stats` as the exposure and white balance gains (natural logarithms) leave them. Clear cells are
/// left out.
fn tone_cells(stats: &Stats, ln_exposure: f64, ln_wb: [f64; 3]) -> Vec<Tone> {
    let g = ln_exposure.exp() as f32;
    let wb = ln_wb.map(|l| l.exp() as f32);
    stats
        .cells
        .iter()
        .enumerate()
        .filter(|(_, c)| c[3] >= 0.02)
        .map(|(i, c)| {
            let lin = [0, 1, 2].map(|k| shape(c[k] * wb[k], g));
            Tone {
                coded: lin.map(srgb_encode),
                b: srgb_encode(luma(lin)),
                w: centre_weight(i) * c[3],
            }
        })
        .collect()
}

/// The weighted `q` quantile (`0..=1`) of the values in `sorted` (value, weight; ascending): each value sits at the
/// middle of its share of the weight and the quantile is interpolated between neighbours.
fn weighted_quantile(sorted: &[(f32, f32)], total: f32, q: f32) -> f32 {
    let target = q * total;
    let mut before = 0.0f32;
    let mut prev: Option<(f32, f32)> = None;
    for &(v, w) in sorted {
        let at = before + 0.5 * w;
        if at >= target {
            return match prev {
                Some((pv, pat)) if at > pat => pv + (v - pv) * (target - pat) / (at - pat),
                _ => v,
            };
        }
        prev = Some((v, at));
        before += w;
    }
    prev.map_or(0.0, |(v, _)| v)
}

/// The sRGB transfer function (coded to linear).
fn srgb_decode(c: f32) -> f32 {
    let c = c.clamp(0.0, 1.0);
    if c <= 0.040_45 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// Table size of the transfer functions of the prediction (the loops evaluate them thousands of times a frame).
const LUT: usize = 1024;

/// Linear interpolation in `table` at `x` in `0..=1`.
fn lerp_table(table: &[f32], x: f32) -> f32 {
    let t = x.clamp(0.0, 1.0) * (LUT - 1) as f32;
    let i = (t as usize).min(LUT - 2);
    let f = t - i as f32;
    table[i] + (table[i + 1] - table[i]) * f
}

/// `srgb_decode` from a table, within `1e-5`.
fn fast_decode(c: f32) -> f32 {
    static T: std::sync::OnceLock<Vec<f32>> = std::sync::OnceLock::new();
    let t = T.get_or_init(|| {
        (0..LUT)
            .map(|i| srgb_decode(i as f32 / (LUT - 1) as f32))
            .collect()
    });
    lerp_table(t, c)
}

/// `srgb_encode` from a table over the square root of the linear value (the curve is steep near 0).
fn fast_encode(l: f32) -> f32 {
    static T: std::sync::OnceLock<Vec<f32>> = std::sync::OnceLock::new();
    let t = T.get_or_init(|| {
        (0..LUT)
            .map(|i| {
                let u = i as f32 / (LUT - 1) as f32;
                srgb_encode(u * u)
            })
            .collect()
    });
    lerp_table(t, l.clamp(0.0, 1.0).sqrt())
}

/// The brightness of a cell after the contrast curve `k` around the pivot `m`, as the shader makes it: the curve runs
/// on each encoded channel, and the brightness is read from the result like every brightness here (the encoded luma of
/// the linear colour). For a gray cell this is the curve of its brightness; for a coloured one it differs a little.
fn tone_after(c: &Tone, k: f32, m: f32) -> f32 {
    if (k - 1.0).abs() < 1e-6 {
        return c.b;
    }
    fast_encode(luma(c.coded.map(|x| fast_decode(contrast_curve(x, k, m)))))
}

/// The weighted mean brightness of `cells` through the curve `k` around `m`.
fn mean_after(cells: &[Tone], k: f32, m: f32, total: f32) -> f32 {
    cells.iter().map(|c| c.w * tone_after(c, k, m)).sum::<f32>() / total
}

/// The percentiles (10th, 90th) of the brightness of `cells` through the curve `k` around `m`.
fn percentiles_after(cells: &[Tone], k: f32, m: f32, total: f32) -> (f32, f32) {
    let mut sorted: Vec<(f32, f32)> = cells.iter().map(|c| (tone_after(c, k, m), c.w)).collect();
    sorted.sort_by(|a, b| a.0.total_cmp(&b.0));
    (
        weighted_quantile(&sorted, total, 0.1),
        weighted_quantile(&sorted, total, 0.9),
    )
}

/// The contrast of the picture through the curve `k` around `m`: the spread between the 10th and 90th percentile of
/// the cell brightnesses.
fn spread_after(cells: &[Tone], k: f32, m: f32, total: f32) -> f32 {
    let (p10, p90) = percentiles_after(cells, k, m, total);
    p90 - p10
}

/// The total weight and the mean brightness of `cells`; `None` when there is no weight.
fn weight_and_mean(cells: &[Tone]) -> Option<(f32, f32)> {
    let total: f32 = cells.iter().map(|c| c.w).sum();
    if total <= 0.0 {
        return None;
    }
    Some((total, cells.iter().map(|c| c.w * c.b).sum::<f32>() / total))
}

/// The pivot of the contrast curve with the slope `k` for which the weighted mean brightness of `cells` is what it
/// was (`mean`), within `0.05 ..= 0.95`: regula falsi (Illinois) from a first try `guess`, `mean` when no pivot does it.
fn contrast_pivot(cells: &[Tone], k: f32, mean: f32, total: f32, guess: f32) -> f32 {
    let (lo_end, hi_end) = (0.05f32, 0.95f32);
    let mean = mean.clamp(lo_end, hi_end);
    if (k - 1.0).abs() < 1e-6 || total <= 0.0 {
        return mean;
    }
    let diff = |m: f32| mean_after(cells, k, m, total) - mean;
    const TOL: f32 = 2e-4;
    let guess = guess.clamp(lo_end, hi_end);
    let d_guess = diff(guess);
    if d_guess.abs() < TOL {
        return guess;
    }
    let (mut a, mut fa) = (lo_end, diff(lo_end));
    let (mut b, mut fb) = (hi_end, diff(hi_end));
    if fa.signum() == fb.signum() {
        return mean;
    }
    // the guess narrows the bracket
    if d_guess.signum() == fa.signum() {
        (a, fa) = (guess, d_guess);
    } else {
        (b, fb) = (guess, d_guess);
    }
    let mut side = 0;
    let mut m = 0.5 * (a + b);
    for _ in 0..12 {
        m = (a * fb - b * fa) / (fb - fa);
        let fm = diff(m);
        if fm.abs() < TOL {
            break;
        }
        if fm.signum() == fb.signum() {
            b = m;
            fb = fm;
            if side == -1 {
                fa *= 0.5;
            }
            side = -1;
        } else {
            a = m;
            fa = fm;
            if side == 1 {
                fb *= 0.5;
            }
            side = 1;
        }
    }
    m
}
/// The wanted contrast gain (the slope of the curve at its pivot, natural logarithm) for the measured `stats`, the
/// `target` spread and the gains held now: `ln_exposure`, `ln_wb` and the contrast `ln_cur`. A picture with nothing
/// to measure (black, white or flat) gives `ln_cur`.
pub fn contrast_target(
    stats: &Stats,
    ln_exposure: f64,
    ln_wb: [f64; 3],
    target: f32,
    ln_cur: f64,
) -> f64 {
    let cells = tone_cells(stats, ln_exposure, ln_wb);
    let Some((total, mean)) = weight_and_mean(&cells) else {
        return ln_cur;
    };
    let spread = spread_after(&cells, 1.0, mean, total);
    let confidence = f64::from(
        smoothstep(0.03, 0.10, mean)
            * (1.0 - smoothstep(0.93, 0.99, mean))
            * smoothstep(0.02, 0.06, spread),
    );
    if confidence <= 0.0 {
        return ln_cur;
    }
    let k_cur = ln_cur.exp() as f32;
    let m_cur = contrast_pivot(&cells, k_cur, mean, total, mean);
    let error = spread_after(&cells, k_cur, m_cur, total) - target;
    if error.abs() <= CONTRAST_BAND {
        return ln_cur;
    }
    let aim = target + error.clamp(-CONTRAST_BAND, CONTRAST_BAND);
    // The slope that brings the spread to `aim` for a pivot `m` (the spread rises with the slope).
    let solve = |m: f32| -> f64 {
        let after = |ln_k: f64| spread_after(&cells, ln_k.exp() as f32, m, total);
        let (mut lo, mut hi) = (MIN_CONTRAST.ln(), MAX_CONTRAST.ln());
        if after(hi) <= aim {
            return hi;
        }
        if after(lo) >= aim {
            return lo;
        }
        for _ in 0..14 {
            let mid = 0.5 * (lo + hi);
            if after(mid) < aim {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        0.5 * (lo + hi)
    };
    // The pivot depends on the slope and the slope on the pivot: two rounds of each settle it.
    let mut want = solve(mean.clamp(0.05, 0.95));
    for _ in 0..2 {
        want = solve(contrast_pivot(&cells, want.exp() as f32, mean, total, mean));
    }
    ln_cur + confidence * (want - ln_cur)
}

/// The pivot the shader should use for the contrast gain `k` on the picture `stats` shows after the gains `ln_exposure`
/// and `ln_wb`: mean brightness stays. `None` when nothing was measured.
fn shader_pivot(stats: &Stats, ln_exposure: f64, ln_wb: [f64; 3], k: f32, guess: f32) -> Option<f32> {
    let cells = tone_cells(stats, ln_exposure, ln_wb);
    let (total, mean) = weight_and_mean(&cells)?;
    Some(contrast_pivot(&cells, k, mean, total, guess))
}
/// The wanted saturation gain (natural logarithm) for the measured `stats`, the `target` colourfulness and the gains
/// held now: `ln_exposure`, `ln_wb`, the contrast `ln_contrast` and the saturation `ln_cur`. Nothing to measure
/// (gray, black, white) gives `ln_cur`.
pub fn saturation_target(
    stats: &Stats,
    ln_exposure: f64,
    ln_wb: [f64; 3],
    ln_contrast: f64,
    target: f32,
    ln_cur: f64,
) -> f64 {
    let cells = tone_cells(stats, ln_exposure, ln_wb);
    let Some((total, mean)) = weight_and_mean(&cells) else {
        return ln_cur;
    };
    let k = ln_contrast.exp() as f32;
    let pivot = contrast_pivot(&cells, k, mean, total, mean);
    let (mut weight, mut sum) = (0.0f32, 0.0f32);
    for c in &cells {
        let (coded, b) = if (k - 1.0).abs() < 1e-6 {
            (c.coded, c.b)
        } else {
            (c.coded.map(|x| contrast_curve(x, k, pivot)), tone_after(c, k, pivot))
        };
        let w = c.w * smoothstep(0.04, 0.12, b) * (1.0 - smoothstep(0.88, 0.97, b));
        let chroma = coded[0].max(coded[1]).max(coded[2]) - coded[0].min(coded[1]).min(coded[2]);
        weight += w;
        sum += w * chroma;
    }
    if weight <= 0.0 {
        return ln_cur;
    }
    let measured = sum / weight;
    let confidence = f64::from(
        smoothstep(0.02, 0.10, weight / total) * smoothstep(0.02, 0.05, measured),
    );
    if confidence <= 0.0 {
        return ln_cur;
    }
    let ideal = (f64::from(target) / f64::from(measured)).ln();
    let error = ln_cur - ideal;
    if error.abs() <= SAT_BAND {
        return ln_cur;
    }
    let want = (ideal + error.clamp(-SAT_BAND, SAT_BAND)).clamp(MIN_SAT.ln(), MAX_SAT.ln());
    ln_cur + confidence * (want - ln_cur)
}

/// Move `cur` toward `target` by the fraction `alpha`; a gain that goes back to 1 lands on 1.
fn follow(cur: f64, target: f64, alpha: f64) -> f64 {
    let next = cur + (target - cur) * alpha;
    if target == 0.0 && next.abs() < SNAP {
        0.0
    } else {
        next
    }
}

/// The gains the filter applies (linear).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Gains {
    pub exposure: f32,
    pub wb: [f32; 3],
    /// The slope of the contrast curve at its pivot.
    pub contrast: f32,
    pub saturation: f32,
}

impl Gains {
    pub const NEUTRAL: Gains = Gains {
        exposure: 1.0,
        wb: [1.0; 3],
        contrast: 1.0,
        saturation: 1.0,
    };
}

// ---------------------------------------------------------------------------------------------------------------
// GPU side

/// `cbuffer Params` of `stats_downsample.hlsl`.
#[repr(C)]
#[derive(Clone, Copy)]
struct StageParams {
    src_size: [f32; 2],
    dst_size: [f32; 2],
    taps: [u32; 2],
    linearize: u32,
    pad: u32,
}

/// Cells per axis of the box filter: one bilinear tap covers about 2 texels.
fn taps_for(scale: f32) -> u32 {
    ((scale / 2.0).ceil() as u32).clamp(1, 8)
}

/// The sizes of the stages for an input of `(w, h)`: each is at most 8 times smaller per axis than the one before
/// and not below 16; the last is 16 x 16.
pub fn stage_sizes(input: (u32, u32)) -> Vec<(u32, u32)> {
    let next = |n: u32| {
        if n <= GRID as u32 {
            GRID as u32
        } else {
            n.div_ceil(8).max(GRID as u32)
        }
    };
    let mut sizes = Vec::new();
    let mut cur = input;
    loop {
        cur = (next(cur.0), next(cur.1));
        sizes.push(cur);
        if cur == (GRID as u32, GRID as u32) {
            return sizes;
        }
    }
}

pub(crate) struct Stage {
    pub(crate) texture: ID3D11Texture2D,
    pub(crate) rtv: ID3D11RenderTargetView,
    pub(crate) srv: ID3D11ShaderResourceView,
    pub(crate) size: (u32, u32),
    pub(crate) constants: ConstantBuffer,
}

impl Stage {
    fn new(r: &Renderer, src: (u32, u32), dst: (u32, u32), linearize: bool) -> GpuResult<Stage> {
        let texture = r.gpu.create_texture(&D3D11_TEXTURE2D_DESC {
            Width: dst.0,
            Height: dst.1,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R16G16B16A16_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        })?;
        let rtv = r.gpu.create_rtv(&texture)?;
        let srv = r.gpu.create_srv(&texture, DXGI_FORMAT_R16G16B16A16_UNORM)?;
        let constants = r.constants(std::mem::size_of::<StageParams>())?;
        let (sx, sy) = (src.0 as f32 / dst.0 as f32, src.1 as f32 / dst.1 as f32);
        constants.write(
            &r.gpu.context,
            &StageParams {
                src_size: [src.0 as f32, src.1 as f32],
                dst_size: [dst.0 as f32, dst.1 as f32],
                taps: [taps_for(sx), taps_for(sy)],
                linearize: u32::from(linearize),
                pad: 0,
            },
        )?;
        Ok(Stage {
            texture,
            rtv,
            srv,
            size: dst,
            constants,
        })
    }
}

/// The downsample chain for one input size.
pub(crate) struct StatsChain {
    pub(crate) input: (u32, u32),
    pub(crate) stages: Vec<Stage>,
}

impl StatsChain {
    pub(crate) fn new(r: &Renderer, input: (u32, u32)) -> GpuResult<StatsChain> {
        let mut stages = Vec::new();
        let mut src = input;
        for (n, dst) in stage_sizes(input).into_iter().enumerate() {
            stages.push(Stage::new(r, src, dst, n == 0)?);
            src = dst;
        }
        Ok(StatsChain { input, stages })
    }
}

/// What the filter makes on its first frame.
struct Gpu {
    stats_ps: ID3D11PixelShader,
    ring: ReadbackRing<()>,
    chain: Option<StatsChain>,
}

pub struct AutoAdjust {
    settings: Settings,
    clock: Clock,
    last_time: Option<f64>,
    /// Natural logarithm of the exposure gain the loop holds (strength not applied).
    ln_exposure: f64,
    /// Natural logarithms of the white balance gains.
    ln_wb: [f64; 3],
    /// Natural logarithm of the contrast gain.
    ln_contrast: f64,
    /// Natural logarithm of the saturation gain.
    ln_saturation: f64,
    /// The pivot of the contrast curve the shader got last.
    pivot: f32,
    /// The newest measurement that has come back from the GPU.
    stats: Option<Stats>,
    main: Program,
    gpu: Option<Gpu>,
    /// Tests: map each measurement as soon as it is copied (one frame of latency, the same every run).
    sync_readback: bool,
    /// Tests: leave the gains as they are (to time the GPU work without the CPU maths).
    #[cfg(test)]
    hold_gains: bool,
}

pub fn make() -> Box<dyn GpuFilter> {
    Box::new(AutoAdjust::new(real_clock()))
}

impl AutoAdjust {
    fn new(clock: Clock) -> AutoAdjust {
        AutoAdjust {
            settings: settings(&Params::new()),
            clock,
            last_time: None,
            ln_exposure: 0.0,
            ln_wb: [0.0; 3],
            ln_contrast: 0.0,
            ln_saturation: 0.0,
            pivot: 0.45,
            stats: None,
            main: Program::new("auto_adjust", MAIN_HLSL),
            gpu: None,
            sync_readback: false,
            #[cfg(test)]
            hold_gains: false,
        }
    }

    /// A filter whose time comes from `clock` (seconds) and whose measurements are mapped in step with the frames.
    #[cfg(test)]
    fn for_test(clock: Clock) -> AutoAdjust {
        AutoAdjust {
            sync_readback: true,
            ..AutoAdjust::new(clock)
        }
    }

    /// The gains the control loop holds, before `strength`.
    #[cfg(test)]
    fn held(&self) -> Gains {
        Gains {
            exposure: self.ln_exposure.exp() as f32,
            wb: self.ln_wb.map(|l| l.exp() as f32),
            contrast: self.ln_contrast.exp() as f32,
            saturation: self.ln_saturation.exp() as f32,
        }
    }

    /// The gains the shader applies: the held gains to the power of `strength`.
    pub fn applied(&self) -> Gains {
        let s = f64::from(self.settings.strength);
        Gains {
            exposure: (s * self.ln_exposure).exp() as f32,
            wb: self.ln_wb.map(|l| (s * l).exp() as f32),
            contrast: (s * self.ln_contrast).exp() as f32,
            saturation: (s * self.ln_saturation).exp() as f32,
        }
    }

    /// The pivot of the contrast curve for the gains applied now: the one that keeps the mean brightness of the
    /// latest measurement. Without a measurement, the last one used.
    fn contrast_pivot_now(&mut self) -> f32 {
        let s = f64::from(self.settings.strength);
        if let Some(stats) = &self.stats {
            let k = (s * self.ln_contrast).exp() as f32;
            if let Some(p) = shader_pivot(
                stats,
                s * self.ln_exposure,
                self.ln_wb.map(|l| s * l),
                k,
                self.pivot,
            ) {
                self.pivot = p;
            }
        }
        self.pivot
    }

    fn ensure_gpu(&mut self, r: &Renderer) -> GpuResult<()> {
        if self.gpu.is_none() {
            self.gpu = Some(Gpu {
                stats_ps: r.shaders.pixel_source("stats_downsample", STATS_HLSL)?,
                ring: ReadbackRing::new(
                    r.gpu.clone(),
                    &[PlaneDesc {
                        format: DXGI_FORMAT_R16G16B16A16_UNORM,
                        width: GRID as u32,
                        height: GRID as u32,
                        bytes_per_texel: 8,
                    }],
                )?,
                chain: None,
            });
        }
        Ok(())
    }

    /// Move the gains toward what the newest measurement asks for.
    fn steer(&mut self, dt: f64) {
        #[cfg(test)]
        if self.hold_gains {
            return;
        }
        let s = self.settings;
        let alpha = 1.0 - (-dt / time_constant(s.speed)).exp();
        let stats = self.stats.as_ref();
        let want_exposure = if s.exposure {
            stats.map_or(self.ln_exposure, |st| {
                exposure_target(st, s.target, self.ln_exposure)
            })
        } else {
            0.0
        };
        let want_wb = if s.white_balance {
            let aim = white_aim(s.temperature, s.tint);
            stats.map_or(self.ln_wb, |st| {
                white_balance_target(st, self.ln_wb, aim)
            })
        } else {
            [0.0; 3]
        };
        // Contrast and saturation measure the picture after the stages before them, as held now.
        let want_contrast = if s.contrast {
            stats.map_or(self.ln_contrast, |st| {
                contrast_target(
                    st,
                    self.ln_exposure,
                    self.ln_wb,
                    s.target_contrast,
                    self.ln_contrast,
                )
            })
        } else {
            0.0
        };
        let want_saturation = if s.saturation {
            stats.map_or(self.ln_saturation, |st| {
                saturation_target(
                    st,
                    self.ln_exposure,
                    self.ln_wb,
                    self.ln_contrast,
                    s.target_saturation,
                    self.ln_saturation,
                )
            })
        } else {
            0.0
        };
        self.ln_exposure = follow(self.ln_exposure, want_exposure, alpha);
        for (cur, want) in self.ln_wb.iter_mut().zip(want_wb) {
            *cur = follow(*cur, want, alpha);
        }
        self.ln_contrast = follow(self.ln_contrast, want_contrast, alpha);
        self.ln_saturation = follow(self.ln_saturation, want_saturation, alpha);
    }

    /// Whether the gains held are exactly 1.
    fn neutral(&self) -> bool {
        self.ln_exposure == 0.0
            && self.ln_wb == [0.0; 3]
            && self.ln_contrast == 0.0
            && self.ln_saturation == 0.0
    }
}

impl GpuFilter for AutoAdjust {
    fn update(&mut self, _r: &Renderer, p: &Params) {
        self.settings = settings(p);
    }

    fn warm_up(&mut self, r: &Renderer) -> GpuResult<()> {
        r.shaders.pixel_source("stats_downsample", STATS_HLSL)?;
        self.main.warm_up(r)
    }

    fn render(&mut self, r: &Renderer, input: &RgbaTexture, output: &RgbaTexture) -> GpuResult<()> {
        let now = (self.clock)();
        let gap = self.last_time.replace(now).map(|t| (now - t).max(0.0));
        self.ensure_gpu(r)?;
        {
            let gpu = self.gpu.as_mut().expect("made above");
            if gap.is_some_and(|g| g > STALE_GAP) {
                // The filter was bypassed for a while: what it measured then says nothing about now.
                gpu.ring.clear();
                self.stats = None;
            }
            // The measurements that finished since the last frame; the newest one counts.
            while let Ok(Some(stats)) = gpu.ring.poll(self.sync_readback, |planes, ()| {
                Stats::from_plane(&planes[0])
            }) {
                self.stats = Some(stats);
            }
        }

        self.steer(gap.map_or(0.0, |g| g.min(MAX_DT)));

        // Measure this frame's input, whatever the gains are (the loop never sees its own output). With both
        // corrections off and the gains home there is nothing to measure.
        let s = self.settings;
        let measure =
            s.exposure || s.white_balance || s.contrast || s.saturation || !self.neutral();
        let gpu = self.gpu.as_mut().expect("made above");
        if measure {
            let size = (input.width, input.height);
            if gpu.chain.as_ref().map(|c| c.input) != Some(size) {
                gpu.chain = Some(StatsChain::new(r, size)?);
            }
            let chain = gpu.chain.as_ref().expect("made above");
            let mut source = input.srv.clone();
            for stage in &chain.stages {
                r.draw(&Pass {
                    ps: &gpu.stats_ps,
                    srvs: &[Some(source)],
                    constants: Some(&stage.constants),
                    target: &stage.rtv,
                    width: stage.size.0,
                    height: stage.size.1,
                });
                source = stage.srv.clone();
            }
            let last = chain.stages.last().expect("at least one stage");
            gpu.ring.submit(&[&last.texture], ())?;
        }

        let g = self.applied();
        if g == Gains::NEUTRAL {
            return r.copy(input, output);
        }
        let k = knee(g.exposure);
        let pivot = if g.contrast == 1.0 {
            self.pivot
        } else {
            self.contrast_pivot_now()
        };
        // The picture is read in linear light (the `_SRGB` view decodes). Contrast and saturation work on the encoded
        // colour, so the shader encodes by hand and writes the codes when either is on; without them it returns linear
        // light and the `_SRGB` render target encodes (see `auto_adjust.hlsl`; the shader tests the same two values).
        let plain = g.contrast == 1.0 && g.saturation == 1.0;
        let views = Views {
            read: Space::Linear,
            write: if plain { Space::Linear } else { Space::Encoded },
            with_codes: false,
        };
        self.main.draw_with(
            r,
            &pack_f32(&[
                g.wb[0],
                g.wb[1],
                g.wb[2],
                g.exposure,
                k,
                g.contrast,
                pivot,
                g.saturation,
            ]),
            input,
            output,
            views,
        )
    }
}
