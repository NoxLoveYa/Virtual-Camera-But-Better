//! Transform (`transform`, plan 6.4): mirror and rotate the picture. Better Cam's own filter: OBS does this on the
//! scene item (Edit Transform: a negative scale mirrors, the rotation turns the item about its centre), so there is
//! no OBS filter to port. The pixel work is in `src/gpu/shaders/transform.hlsl`, which uses the bilinear sampling of
//! libobs/data/default.effect.
//!
//! The picture is mirrored first, then rotated clockwise by `rotation` degrees about its centre. A multiple of 90
//! degrees moves whole pixels, exactly: 90 and 270 swap the width and the height, and no pixel is resampled. Any
//! other angle is resampled bilinearly, in linear light, into a picture the size of the rotated bounding box; the
//! corners the picture does not reach are transparent (the output stage shows the background there).

use super::{clamp_size, pack_f32, Program};
use crate::fx::{pb, pf, GpuFilter, Params};
use crate::gpu::{GpuResult, Renderer, RgbaTexture, Views};

pub const KIND: &str = "transform";

/// The settings the filter reads.
pub const KEYS: &[&str] = &["flip_h", "flip_v", "rotation"];

const HLSL: &str = concat!(
    include_str!("../../gpu/shaders/geometry_common.hlsl"),
    "\n",
    include_str!("../../gpu/shaders/transform.hlsl")
);

/// How far from a multiple of 90 degrees a rotation still counts as one (the slider's step is 0.1).
const SNAP_DEGREES: f32 = 0.01;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Settings {
    pub flip_h: bool,
    pub flip_v: bool,
    /// Degrees, clockwise, -180 to 180.
    pub rotation: f32,
}

impl Settings {
    pub fn read(p: &Params) -> Settings {
        Settings {
            flip_h: pb(p, "flip_h", false),
            flip_v: pb(p, "flip_v", false),
            rotation: pf(p, "rotation", 0.0, -180.0, 180.0),
        }
    }

    /// Quarter turns clockwise (0 to 3) when the rotation is a multiple of 90 degrees.
    pub fn quarter_turns(&self) -> Option<u32> {
        let q = (self.rotation / 90.0).round();
        ((self.rotation - q * 90.0).abs() < SNAP_DEGREES).then(|| (q as i32).rem_euclid(4) as u32)
    }

    /// The size of the result for an input of `(width, height)`.
    pub fn size(&self, input: (u32, u32)) -> (u32, u32) {
        match self.quarter_turns() {
            Some(0) | Some(2) => input,
            Some(_) => (input.1, input.0),
            None => {
                let (w, h) = (input.0 as f64, input.1 as f64);
                let (s, c) = (self.rotation as f64).to_radians().sin_cos();
                let (s, c) = (s.abs(), c.abs());
                // The small allowance keeps rounding noise from adding a pixel.
                let bw = (w * c + h * s - 1e-3).ceil() as i64;
                let bh = (w * s + h * c - 1e-3).ceil() as i64;
                clamp_size(bw, bh)
            }
        }
    }

    /// Nothing to do: no mirror and no turn (or a full one).
    fn is_identity(&self) -> bool {
        !self.flip_h && !self.flip_v && self.quarter_turns() == Some(0)
    }

    /// `(cos, sin)` of the rotation, exact for the quarter turns.
    fn cos_sin(&self) -> (f32, f32) {
        match self.quarter_turns() {
            Some(0) => (1.0, 0.0),
            Some(1) => (0.0, 1.0),
            Some(2) => (-1.0, 0.0),
            Some(_) => (0.0, -1.0),
            None => {
                let (s, c) = self.rotation.to_radians().sin_cos();
                (c, s)
            }
        }
    }
}

pub struct Transform {
    settings: Settings,
    program: Program,
}

pub fn make() -> Box<dyn GpuFilter> {
    Box::new(Transform {
        settings: Settings::read(&Params::new()),
        program: Program::new(KIND, HLSL),
    })
}

impl GpuFilter for Transform {
    fn update(&mut self, _r: &Renderer, p: &Params) {
        self.settings = Settings::read(p);
    }

    fn output_size(&self, input: (u32, u32)) -> (u32, u32) {
        self.settings.size(input)
    }

    fn warm_up(&mut self, r: &Renderer) -> GpuResult<()> {
        self.program.warm_up(r)
    }

    fn render(&mut self, r: &Renderer, input: &RgbaTexture, output: &RgbaTexture) -> GpuResult<()> {
        let s = self.settings;
        if s.is_identity() && (input.width, input.height) == (output.width, output.height) {
            return r.copy(input, output);
        }
        let (cos, sin) = s.cos_sin();
        let flag = |b: bool| if b { 1.0 } else { 0.0 };
        let exact = flag(s.quarter_turns().is_some());
        let packed = pack_f32(&[
            output.width as f32,
            output.height as f32,
            flag(s.flip_h),
            flag(s.flip_v),
            cos,
            sin,
            exact,
            0.0,
        ]);
        // A flip or a quarter turn moves texels as they are (the codes); any other angle resamples in linear light.
        let views = if s.quarter_turns().is_some() { Views::ENCODED } else { Views::LINEAR };
        self.program.draw_with(r, &packed, input, output, views)
    }
}
