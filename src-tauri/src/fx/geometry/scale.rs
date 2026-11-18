/*****************************************************************************
Ported to Rust for Better Cam from OBS Studio, plugins/obs-filters/scale-filter.c (`scale_filter_update`,
`scale_filter_tick`, `scale_filter_defaults`, `scale_filter_properties`). The pixel work is in
src/gpu/shaders/scale_{basic,bicubic,lanczos,area}.hlsl, ports of libobs/data/{default,bilinear_lowres_scale,
bicubic_scale,lanczos_scale,area}.effect.

This program is free software: you can redistribute it and/or modify
it under the terms of the GNU General Public License as published by
the Free Software Foundation, either version 2 of the License, or
(at your option) any later version.

This program is distributed in the hope that it will be useful,
but WITHOUT ANY WARRANTY; without even the implied warranty of
MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
GNU General Public License for more details.

You should have received a copy of the GNU General Public License
along with this program.  If not, see <http://www.gnu.org/licenses/>.
*****************************************************************************/

//! Scaling/Aspect Ratio (`scale_filter`, plan 6.4): resize the picture or force an aspect ratio.
//!
//! `resolution` is one of
//! * `None` (or anything that is not a size): no change. OBS skips the filter then too.
//! * `Base (Canvas) Resolution`: the output resolution of the engine ([`GpuFilter::set_canvas`]).
//! * an aspect ratio `A:B` (`16:9`, `16:10`, `4:3`, `1:1`, or typed): as in OBS, a ratio wider than the picture
//!   keeps the height and sets the width to `height * A / B`; any other ratio keeps the width and sets the height to
//!   `width / (A / B)` (4:3 on 1920x1080 gives 1920x1440). Both are truncated to whole pixels. A picture that has
//!   the aspect already (within 0.0001) passes through.
//! * `WxH`: that size.
//!
//! [`plan`] is `scale_filter_tick`: it picks the effect from the input size, the output size and `sampling`:
//! the size does not change: a plain copy; the output is below half the input in either direction (and `sampling` is
//! not `point`): the low-res bilinear effect; otherwise point and bilinear use the default effect, bicubic and
//! lanczos their kernels (with `undistort`), area the box average (the upscale variant when the output is at least as
//! large as the input in both directions).
//!
//! Where Better Cam differs from OBS:
//! * A size that does not change is copied, for every sampling mode. OBS runs the effect, which for point, bilinear,
//!   bicubic and lanczos returns the same texels, so the result is the same without the work.
//! * A size of zero or below, and a size above [`super::MAX_SIDE`], pass through or clamp ([`super::clamp_size`]).
//! * The filtering is done in linear light on premultiplied alpha in the shader (as OBS's sRGB textures do), without
//!   the hardware-sampler tap merging OBS uses to save fetches. The kernels and weights are OBS's.

use super::{clamp_size, pack_f32, Program};
use crate::fx::{pb, ps, GpuFilter, Params};
use crate::gpu::{GpuResult, Renderer, RgbaTexture, Views};

pub const KIND: &str = "scale_filter";

/// The settings the filter reads (OBS setting names).
pub const KEYS: &[&str] = &["sampling", "resolution", "undistort"];

/// The text of the "Base (Canvas) Resolution" entry (OBS stores the displayed text as the value).
pub const BASE_CANVAS: &str = "Base (Canvas) Resolution";

const BASIC: &str = concat!(
    include_str!("../../gpu/shaders/geometry_common.hlsl"),
    "\n",
    include_str!("../../gpu/shaders/scale_basic.hlsl")
);
const BICUBIC: &str = concat!(
    include_str!("../../gpu/shaders/geometry_common.hlsl"),
    "\n",
    include_str!("../../gpu/shaders/scale_bicubic.hlsl")
);
const LANCZOS: &str = concat!(
    include_str!("../../gpu/shaders/geometry_common.hlsl"),
    "\n",
    include_str!("../../gpu/shaders/scale_lanczos.hlsl")
);
const AREA: &str = concat!(
    include_str!("../../gpu/shaders/geometry_common.hlsl"),
    "\n",
    include_str!("../../gpu/shaders/scale_area.hlsl")
);

/// `obs_scale_type`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sampling {
    Point,
    Bilinear,
    Bicubic,
    Lanczos,
    Area,
}

impl Sampling {
    /// `scale_filter_update`: the names match without regard to case, anything else is bicubic.
    pub fn parse(s: &str) -> Sampling {
        match s.to_ascii_lowercase().as_str() {
            "point" => Sampling::Point,
            "bilinear" => Sampling::Bilinear,
            "lanczos" => Sampling::Lanczos,
            "area" => Sampling::Area,
            _ => Sampling::Bicubic,
        }
    }
}

/// What `resolution` asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// No usable resolution: the filter does nothing.
    None,
    /// The base (canvas) resolution.
    Canvas,
    /// `A:B`
    Aspect(i32, i32),
    /// `WxH`
    Size(i32, i32),
}

/// What C's `sscanf(s, "%d<sep>%d", ...)` accepts: leading space, an optional sign and digits, the separator, then
/// the same again; whatever follows the second number is ignored.
fn scan_pair(s: &str, sep: char) -> Option<(i32, i32)> {
    fn int(s: &str) -> Option<(i32, &str)> {
        let s = s.trim_start();
        let (sign, digits) = match s.as_bytes().first() {
            Some(b'-') => (-1i64, &s[1..]),
            Some(b'+') => (1i64, &s[1..]),
            _ => (1i64, s),
        };
        let end = digits
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(digits.len());
        if end == 0 {
            return None;
        }
        let value = digits[..end].parse::<i64>().unwrap_or(i64::MAX);
        let value = (sign * value).clamp(i32::MIN as i64, i32::MAX as i64) as i32;
        Some((value, &digits[end..]))
    }
    let (a, rest) = int(s)?;
    let rest = rest.strip_prefix(sep)?;
    let (b, _) = int(rest)?;
    Some((a, b))
}

impl Target {
    /// `scale_filter_update`: `Base (Canvas) Resolution`, then `%dx%d`, then `%d:%d`; no match is [`Target::None`].
    pub fn parse(s: &str) -> Target {
        if s == BASE_CANVAS {
            return Target::Canvas;
        }
        if let Some((w, h)) = scan_pair(s, 'x') {
            return Target::Size(w, h);
        }
        if let Some((a, b)) = scan_pair(s, ':') {
            return Target::Aspect(a, b);
        }
        Target::None
    }
}

/// How the picture is resampled.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Method {
    /// Same size, or nothing to do: copy.
    Copy,
    Point,
    Bilinear,
    /// OBS_EFFECT_BILINEAR_LOWRES
    LowRes,
    Bicubic {
        undistort: bool,
    },
    Lanczos {
        undistort: bool,
    },
    Area {
        upscale: bool,
    },
}

/// What [`plan`] decided.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Plan {
    pub size: (u32, u32),
    pub method: Method,
    /// The `undistort_factor` the bicubic and lanczos shaders get: the new aspect over the old one (1 without
    /// undistort).
    pub undistort_factor: f32,
}

/// The size an aspect ratio `new_aspect` (width over height) gives a picture of `input`, as in OBS's
/// `scale_filter_tick`: a wider aspect keeps the height and the width is `height * new_aspect`; otherwise the width
/// stays and the height is `width / new_aspect`. The `as` casts truncate like C's `(int)`.
pub fn aspect_size(input: (u32, u32), new_aspect: f64) -> (i64, i64) {
    let (cx, cy) = (input.0 as f64, input.1 as f64);
    if new_aspect > cx / cy {
        ((cy * new_aspect) as i64, input.1 as i64)
    } else {
        (input.0 as i64, (cx / new_aspect) as i64)
    }
}

/// `scale_filter_tick`. `canvas` is the base resolution, when it is known.
pub fn plan(
    target: Target,
    sampling: Sampling,
    can_undistort: bool,
    canvas: Option<(u32, u32)>,
    input: (u32, u32),
) -> Plan {
    let copy = Plan {
        size: input,
        method: Method::Copy,
        undistort_factor: 1.0,
    };
    let (cx, cy) = (input.0 as f64, input.1 as f64);
    let old_aspect = cx / cy;

    // The wanted size, and the aspect the undistort factor compares with (the one that was typed, not the rounded
    // result).
    let (size, new_aspect) = match target {
        Target::None => return copy,
        Target::Canvas => match canvas {
            Some((w, h)) if w > 0 && h > 0 => (clamp_size(w as i64, h as i64), w as f64 / h as f64),
            _ => return copy,
        },
        Target::Size(w, h) => {
            if w <= 0 || h <= 0 {
                return copy;
            }
            (clamp_size(w as i64, h as i64), w as f64 / h as f64)
        }
        Target::Aspect(a, b) => {
            if a <= 0 || b <= 0 {
                return copy;
            }
            let new_aspect = a as f64 / b as f64;
            // EPSILON of libobs/graphics/math-defs.h
            if (old_aspect - new_aspect).abs() <= 0.0001 {
                return copy;
            }
            let (w, h) = aspect_size(input, new_aspect);
            (clamp_size(w, h), new_aspect)
        }
    };
    if size == input {
        return Plan { size, ..copy };
    }

    let lower_than_2x = size.0 < input.0 / 2 || size.1 < input.1 / 2;
    let mut undistort = false;
    let method = if lower_than_2x && sampling != Sampling::Point {
        Method::LowRes
    } else {
        match sampling {
            Sampling::Point => Method::Point,
            Sampling::Bilinear => Method::Bilinear,
            Sampling::Bicubic => {
                undistort = can_undistort;
                Method::Bicubic { undistort }
            }
            Sampling::Lanczos => {
                undistort = can_undistort;
                Method::Lanczos { undistort }
            }
            Sampling::Area => Method::Area {
                upscale: size.0 >= input.0 && size.1 >= input.1,
            },
        }
    };
    Plan {
        size,
        method,
        undistort_factor: if undistort {
            (new_aspect / old_aspect) as f32
        } else {
            1.0
        },
    }
}

pub struct Scale {
    target: Target,
    sampling: Sampling,
    undistort: bool,
    canvas: Option<(u32, u32)>,
    basic: Program,
    bicubic: Program,
    lanczos: Program,
    area: Program,
}

pub fn make() -> Box<dyn GpuFilter> {
    Box::new(Scale {
        target: Target::None,
        sampling: Sampling::Bicubic,
        undistort: false,
        canvas: None,
        basic: Program::new("scale_basic", BASIC),
        bicubic: Program::new("scale_bicubic", BICUBIC),
        lanczos: Program::new("scale_lanczos", LANCZOS),
        area: Program::new("scale_area", AREA),
    })
}

impl Scale {
    fn plan(&self, input: (u32, u32)) -> Plan {
        plan(
            self.target,
            self.sampling,
            self.undistort,
            self.canvas,
            input,
        )
    }
}

impl GpuFilter for Scale {
    fn update(&mut self, _r: &Renderer, p: &Params) {
        self.target = Target::parse(ps(p, "resolution", "None"));
        self.sampling = Sampling::parse(ps(p, "sampling", "bicubic"));
        self.undistort = pb(p, "undistort", false);
    }

    fn set_canvas(&mut self, size: (u32, u32)) {
        self.canvas = Some(size);
    }

    fn output_size(&self, input: (u32, u32)) -> (u32, u32) {
        self.plan(input).size
    }

    fn warm_up(&mut self, r: &Renderer) -> GpuResult<()> {
        self.basic.warm_up(r)?;
        self.bicubic.warm_up(r)?;
        self.lanczos.warm_up(r)?;
        self.area.warm_up(r)
    }

    fn render(&mut self, r: &Renderer, input: &RgbaTexture, output: &RgbaTexture) -> GpuResult<()> {
        let plan = self.plan((input.width, input.height));
        let (w, h) = (output.width as f32, output.height as f32);
        let k = plan.undistort_factor;
        match plan.method {
            Method::Copy => r.copy(input, output),
            // A point copy takes the texel as it is: the codes in, the codes out.
            Method::Point => self
                .basic
                .draw(r, &pack_f32(&[w, h, 0.0, 0.0]), input, output),
            // Every mode that filters works in linear light on the `_SRGB` views (the hardware converts).
            Method::Bilinear => self
                .basic
                .draw_with(r, &pack_f32(&[w, h, 0.0, 1.0]), input, output, Views::LINEAR),
            Method::LowRes => self
                .basic
                .draw_with(r, &pack_f32(&[w, h, 0.0, 2.0]), input, output, Views::LINEAR),
            Method::Bicubic { undistort } => {
                let u = if undistort { 1.0 } else { 0.0 };
                self.bicubic
                    .draw_with(r, &pack_f32(&[w, h, k, u]), input, output, Views::LINEAR)
            }
            Method::Lanczos { undistort } => {
                let u = if undistort { 1.0 } else { 0.0 };
                self.lanczos
                    .draw_with(r, &pack_f32(&[w, h, k, u]), input, output, Views::LINEAR)
            }
            Method::Area { upscale } => {
                let u = if upscale { 1.0 } else { 0.0 };
                self.area
                    .draw_with(r, &pack_f32(&[w, h, u, 0.0]), input, output, Views::LINEAR)
            }
        }
    }
}
