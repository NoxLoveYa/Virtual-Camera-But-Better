//! Auto Adjust (`auto_adjust`, plan 6.4, P16): Better Cam's own software "auto mode". It has no OBS counterpart.
//!
//! It measures the picture on the GPU and corrects brightness (auto exposure) and colour cast (auto white balance)
//! in a shader, smoothly over time, for cameras whose built-in auto is poor.
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
//! Gray world on the cells that tell something about the light: not near black, not near white and not strongly
//! saturated (a smooth weight in each, so no cell flips the result by crossing a threshold). The gains make the mean of
//! those cells grey, `g_c = Y / m_c`, normalised to keep the luma of the mean, and each is limited to `0.5 ..= 2`: a
//! scene that really is one colour is corrected only part of the way. A small dead band holds the gains too.
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

use super::geometry::{pack_f32, Program};
use super::{pb, pf, GpuFilter, Params};
use crate::gpu::readback::{MappedPlane, PlaneDesc, ReadbackRing};
use crate::gpu::{ConstantBuffer, GpuResult, Pass, Renderer, RgbaTexture};

#[cfg(test)]
mod tests;

pub const KIND: &str = "auto_adjust";

/// The settings the filter reads.
pub const KEYS: &[&str] = &["exposure", "white_balance", "target", "strength", "speed"];

const MAIN_HLSL: &str = include_str!("../gpu/shaders/auto_adjust.hlsl");
const STATS_HLSL: &str = include_str!("../gpu/shaders/stats_downsample.hlsl");

/// Cells per side of the statistics image.
const GRID: usize = 16;
const CELLS: usize = GRID * GRID;

/// The exposure gain limits (linear): two stops either way.
const MIN_GAIN: f64 = 0.25;
const MAX_GAIN: f64 = 4.0;
/// The white balance gain limits per channel (linear).
const MIN_WB: f64 = 0.5;
const MAX_WB: f64 = 2.0;
/// Dead band of the exposure loop, in brightness units (the sRGB-coded scale of `target`).
const EXPOSURE_BAND: f32 = 0.012;
/// Dead band of the white balance loop, in natural-log units of the gain (0.8 %).
const WB_BAND: f64 = 0.008;
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
    strength: f32,
    speed: f32,
}

fn settings(p: &Params) -> Settings {
    Settings {
        exposure: pb(p, "exposure", true),
        white_balance: pb(p, "white_balance", true),
        target: pf(p, "target", 0.45, 0.2, 0.8),
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
    fn from_plane(plane: &MappedPlane<'_>) -> Stats {
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

/// The wanted white balance gains (natural logarithms, red green blue) for the measured `stats`, given the gains
/// `ln_cur` held now. A picture with nothing usable to measure gives `ln_cur`.
pub fn white_balance_target(stats: &Stats, ln_cur: [f64; 3]) -> [f64; 3] {
    let mut total = 0.0f32;
    let mut weight_sum = 0.0f32;
    let mut sum = [0.0f32; 3];
    for (i, c) in stats.cells.iter().enumerate() {
        if c[3] < 0.02 {
            continue;
        }
        let cw = centre_weight(i) * c[3];
        total += cw;
        let rgb = [c[0], c[1], c[2]];
        let y = srgb_encode(luma(rgb));
        let (mx, mn) = (
            rgb[0].max(rgb[1]).max(rgb[2]),
            rgb[0].min(rgb[1]).min(rgb[2]),
        );
        let saturation = if mx > 1e-4 { (mx - mn) / mx } else { 1.0 };
        let w = cw
            * smoothstep(0.04, 0.15, y)
            * (1.0 - smoothstep(0.85, 0.97, y))
            * (1.0 - smoothstep(0.55, 0.9, saturation));
        weight_sum += w;
        for k in 0..3 {
            sum[k] += w * rgb[k];
        }
    }
    if total <= 0.0 || weight_sum <= 0.0 {
        return ln_cur;
    }
    let confidence = f64::from(smoothstep(0.02, 0.12, weight_sum / total));
    let mean = sum.map(|s| s / weight_sum);
    if mean.iter().any(|m| *m < 1e-5) {
        return ln_cur;
    }
    // Gray world: every channel of the mean to the luma of the mean, limited, then normalised to keep that luma.
    let y = luma(mean);
    let limit = |g: f32| g.clamp(MIN_WB as f32, MAX_WB as f32);
    let raw = [0, 1, 2].map(|k| limit(y / mean[k]));
    let keep = y / luma([raw[0] * mean[0], raw[1] * mean[1], raw[2] * mean[2]]);
    let mut out = ln_cur;
    for k in 0..3 {
        let wanted = f64::from(limit(raw[k] * keep)).ln();
        let delta = wanted - ln_cur[k];
        let past_band = delta.signum() * (delta.abs() - WB_BAND).max(0.0);
        out[k] = ln_cur[k] + confidence * past_band;
    }
    out
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
}

impl Gains {
    pub const NEUTRAL: Gains = Gains {
        exposure: 1.0,
        wb: [1.0; 3],
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

struct Stage {
    texture: ID3D11Texture2D,
    rtv: ID3D11RenderTargetView,
    srv: ID3D11ShaderResourceView,
    size: (u32, u32),
    constants: ConstantBuffer,
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
struct StatsChain {
    input: (u32, u32),
    stages: Vec<Stage>,
}

impl StatsChain {
    fn new(r: &Renderer, input: (u32, u32)) -> GpuResult<StatsChain> {
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
    /// The newest measurement that has come back from the GPU.
    stats: Option<Stats>,
    main: Program,
    gpu: Option<Gpu>,
    /// Tests: map each measurement as soon as it is copied (one frame of latency, the same every run).
    sync_readback: bool,
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
            stats: None,
            main: Program::new("auto_adjust", MAIN_HLSL),
            gpu: None,
            sync_readback: false,
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
        }
    }

    /// The gains the shader applies: the held gains to the power of `strength`.
    pub fn applied(&self) -> Gains {
        let s = f64::from(self.settings.strength);
        Gains {
            exposure: (s * self.ln_exposure).exp() as f32,
            wb: self.ln_wb.map(|l| (s * l).exp() as f32),
        }
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
            stats.map_or(self.ln_wb, |st| white_balance_target(st, self.ln_wb))
        } else {
            [0.0; 3]
        };
        self.ln_exposure = follow(self.ln_exposure, want_exposure, alpha);
        for (cur, want) in self.ln_wb.iter_mut().zip(want_wb) {
            *cur = follow(*cur, want, alpha);
        }
    }

    /// Whether the gains held are exactly 1.
    fn neutral(&self) -> bool {
        self.ln_exposure == 0.0 && self.ln_wb == [0.0; 3]
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
        let measure = self.settings.exposure || self.settings.white_balance || !self.neutral();
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
        self.main.draw(
            r,
            &pack_f32(&[g.wb[0], g.wb[1], g.wb[2], g.exposure, k, 0.0, 0.0, 0.0]),
            input,
            output,
        )
    }
}
