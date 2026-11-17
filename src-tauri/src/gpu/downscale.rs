//! Downscale for the preview (plan 4.4): the finished output frame shrinks to the size the window can show, on the
//! GPU, before it is read back. A 1080p frame becomes at most 960x540, so the copy to the CPU and through the IPC
//! stays small.

use super::{ConstantBuffer, GpuError, GpuResult, Pass, Pooled, Renderer, RgbaTexture, ShaderId};

/// The size of the preview picture: `source` scaled to fit inside `bound`, keeping its aspect ratio (so the picture
/// has no bars of its own), and never larger than `source`. Each side is at least 1 pixel; the rounding can leave the
/// aspect ratio one pixel off.
pub fn fit_within(source: (u32, u32), bound: (u32, u32)) -> (u32, u32) {
    let (sw, sh) = (source.0.max(1) as f64, source.1.max(1) as f64);
    let scale = (bound.0.max(1) as f64 / sw)
        .min(bound.1.max(1) as f64 / sh)
        .min(1.0);
    (
        ((sw * scale).round() as u32).max(1),
        ((sh * scale).round() as u32).max(1),
    )
}

#[repr(C)]
#[derive(Clone, Copy)]
struct DownParams {
    src_size: [f32; 2],
    dst_size: [f32; 2],
    taps: [u32; 2],
    pad: [u32; 2],
}

/// Cells per axis of the box filter: one bilinear tap covers about 2 texels, and more than 8 taps per axis would
/// cost more than the picture is worth.
fn taps_for(scale: f32) -> u32 {
    ((scale / 2.0).ceil() as u32).clamp(1, 8)
}

/// Runs the preview downscale pass. Create it once (the constant buffer is made on first use) and call
/// [`Downscaler::run`] on the render thread.
#[derive(Default)]
pub struct Downscaler {
    constants: Option<ConstantBuffer>,
}

impl Downscaler {
    pub fn new() -> Downscaler {
        Downscaler::default()
    }

    /// Render `input` into a pooled `width` x `height` texture with a box filter. The caller picks a size of the
    /// same aspect ratio ([`fit_within`]); the pass does not add bars.
    pub fn run(
        &mut self,
        r: &Renderer,
        input: &RgbaTexture,
        width: u32,
        height: u32,
    ) -> GpuResult<Pooled> {
        if width == 0 || height == 0 {
            return Err(GpuError::Invalid("preview size is zero".into()));
        }
        if self.constants.is_none() {
            self.constants = Some(r.constants(std::mem::size_of::<DownParams>())?);
        }
        let constants = self.constants.as_ref().expect("made above");
        let sx = input.width as f32 / width as f32;
        let sy = input.height as f32 / height as f32;
        constants.write(
            &r.gpu.context,
            &DownParams {
                src_size: [input.width as f32, input.height as f32],
                dst_size: [width as f32, height as f32],
                taps: [taps_for(sx), taps_for(sy)],
                pad: [0; 2],
            },
        )?;
        let ps = r.shaders.pixel(ShaderId::PreviewDown, "main")?;
        let out = r.texture(width, height)?;
        r.draw(&Pass {
            ps: &ps,
            srvs: &[Some(input.srv.clone())],
            constants: Some(constants),
            target: &out.rtv,
            width,
            height,
        });
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sixteen_by_nine_output_fills_the_cap() {
        assert_eq!(fit_within((1920, 1080), (960, 540)), (960, 540));
        assert_eq!(fit_within((3840, 2160), (960, 540)), (960, 540));
    }

    #[test]
    fn four_by_three_output_has_no_bars() {
        // The height limits: 540 high, 720 wide, the same 4:3 as the output.
        assert_eq!(fit_within((1920, 1440), (960, 540)), (720, 540));
        assert_eq!(fit_within((640, 480), (960, 540)), (640, 480));
        assert_eq!(fit_within((1280, 960), (960, 540)), (720, 540));
    }

    #[test]
    fn a_small_request_gives_a_small_picture_and_the_output_is_never_upscaled() {
        assert_eq!(fit_within((1920, 1080), (560, 315)), (560, 315));
        assert_eq!(fit_within((1920, 1080), (400, 400)), (400, 225));
        assert_eq!(fit_within((640, 360), (960, 540)), (640, 360));
        assert_eq!(fit_within((1920, 1080), (0, 0)), (1, 1));
    }

    #[test]
    fn the_aspect_ratio_stays_within_a_pixel() {
        for (src, bound) in [
            ((1920, 1080), (777, 433)),
            ((1280, 720), (601, 599)),
            ((640, 480), (333, 333)),
            ((1000, 1000), (960, 540)),
        ] {
            let (w, h) = fit_within(src, bound);
            assert!(w <= bound.0 && h <= bound.1, "{src:?} in {bound:?} gave {w}x{h}");
            let ideal_h = w as f64 * src.1 as f64 / src.0 as f64;
            assert!((ideal_h - h as f64).abs() <= 1.0, "{src:?}: {w}x{h}");
        }
    }

    #[test]
    fn taps_grow_with_the_scale_but_stay_bounded() {
        assert_eq!(taps_for(1.0), 1);
        assert_eq!(taps_for(2.0), 1);
        assert_eq!(taps_for(3.4), 2);
        assert_eq!(taps_for(100.0), 8);
    }
}
