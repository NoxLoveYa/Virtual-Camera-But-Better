/*****************************************************************************
Ported to Rust for Better Cam from OBS Studio, plugins/obs-filters/crop-filter.c (`crop_filter_update`,
`crop_filter_defaults`, `crop_filter_properties`, `calc_crop_dimensions`). The pixel work is in
src/gpu/shaders/crop.hlsl, a port of plugins/obs-filters/data/crop_filter.effect.

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

//! Crop/Pad (`crop_filter`, plan 6.4): trim the edges of the picture, or with a negative value add transparent
//! padding.
//!
//! Relative mode (the default) takes `left`, `top`, `right` and `bottom` off the picture: the result is
//! `width - left - right` by `height - top - bottom`. Absolute mode shows a `cx` by `cy` window whose top left corner
//! is the input pixel `(left, top)`; OBS's labels turn `left` and `top` into X and Y there. A size below one pixel
//! becomes one, as in OBS. The output is a whole-pixel copy of the window, and what lies outside the picture is
//! transparent.
//!
//! Where Better Cam differs from OBS: in absolute mode a `cx` or `cy` of 0 means "to the edge" (the window runs to
//! the right or bottom edge of the picture) where OBS would make a one pixel window. The default (all zero) then
//! shows the whole picture in both modes. Sizes are clamped to [`super::MAX_SIDE`] and [`super::MAX_PIXELS`].

use super::{clamp_size, pack_i32, Program};
use crate::fx::{pb, pi, GpuFilter, Params};
use crate::gpu::{GpuResult, Renderer, RgbaTexture};

pub const KIND: &str = "crop_filter";

/// The settings the filter reads (OBS setting names).
pub const KEYS: &[&str] = &["relative", "left", "top", "right", "bottom", "cx", "cy"];

const HLSL: &str = include_str!("../../gpu/shaders/crop.hlsl");

/// The settings of one crop (`crop_filter_update`, ranges of `crop_filter_properties`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settings {
    pub absolute: bool,
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
    pub cx: i32,
    pub cy: i32,
}

impl Settings {
    pub fn read(p: &Params) -> Settings {
        Settings {
            absolute: !pb(p, "relative", true),
            left: pi(p, "left", 0, -8192, 8192),
            top: pi(p, "top", 0, -8192, 8192),
            right: pi(p, "right", 0, -8192, 8192),
            bottom: pi(p, "bottom", 0, -8192, 8192),
            cx: pi(p, "cx", 0, 0, 8192),
            cy: pi(p, "cy", 0, 0, 8192),
        }
    }

    /// The size of the result for an input of `(width, height)`: `calc_crop_dimensions`.
    pub fn size(&self, input: (u32, u32)) -> (u32, u32) {
        let (w, h) = (input.0 as i64, input.1 as i64);
        let (width, height) = if self.absolute {
            (
                if self.cx == 0 {
                    w - self.left as i64
                } else {
                    self.cx as i64
                },
                if self.cy == 0 {
                    h - self.top as i64
                } else {
                    self.cy as i64
                },
            )
        } else {
            (
                w - self.left as i64 - self.right as i64,
                h - self.top as i64 - self.bottom as i64,
            )
        };
        clamp_size(width, height)
    }
}

pub struct Crop {
    settings: Settings,
    program: Program,
}

pub fn make() -> Box<dyn GpuFilter> {
    Box::new(Crop {
        settings: Settings::read(&Params::new()),
        program: Program::new(KIND, HLSL),
    })
}

impl GpuFilter for Crop {
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
        if s.left == 0 && s.top == 0 && (input.width, input.height) == (output.width, output.height)
        {
            return r.copy(input, output);
        }
        self.program
            .draw(r, &pack_i32(&[s.left, s.top, 0, 0]), input, output)
    }
}
