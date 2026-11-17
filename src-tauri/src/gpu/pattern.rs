//! Test pictures and CPU reference conversions.
//!
//! Used by the unit tests and by `engine_headless --synthetic bars`: SMPTE colour bars as RGB, packers that turn RGB
//! into the camera formats the converter accepts (NV12, YUY2, I420, P010, BGRA), and a CPU NV12 decoder to compare
//! what the GPU and the virtual camera produce. The CPU conversion is deliberately plain f64 math, independent of
//! the shader code path.

use super::convert::{rgb_to_yuv_rows, yuv_to_rgb_rows, ColorRange, ColorSpace};

/// Colour bars in the SMPTE ECR 1-1978 layout (75 % bars, the reverse-blue strip, and the -I / white / +Q / PLUGE
/// row), as tightly packed RGB. Every edge sits on an even pixel so 4:2:0 subsampling keeps the bars exact.
pub fn smpte_bars(width: u32, height: u32) -> Vec<u8> {
    let even = |v: f64| ((v / 2.0).round() as u32) * 2;
    let top = even(height as f64 * 0.67);
    let mid = even(height as f64 * 0.08);
    let (w, h) = (width, height);
    let seven = |i: u32| even(w as f64 * i as f64 / 7.0);

    const W: [u8; 3] = [191, 191, 191];
    const K: [u8; 3] = [19, 19, 19];
    let top_colors: [[u8; 3]; 7] = [
        W,
        [191, 191, 0],
        [0, 191, 191],
        [0, 191, 0],
        [191, 0, 191],
        [191, 0, 0],
        [0, 0, 191],
    ];
    let mid_colors: [[u8; 3]; 7] = [[0, 0, 191], K, [191, 0, 191], K, [0, 191, 191], K, W];
    // Bottom row: -I, 100 % white, +Q, black, then the PLUGE (-4 %, 0, +4 %) and black.
    let u = w as f64 / 7.0;
    let bottom_edges: [u32; 9] = [
        0,
        even(u * 1.25),
        even(u * 2.5),
        even(u * 3.75),
        even(u * 5.0),
        even(u * (5.0 + 1.0 / 3.0)),
        even(u * (5.0 + 2.0 / 3.0)),
        even(u * 6.0),
        w,
    ];
    let bottom_colors: [[u8; 3]; 8] = [
        [0, 33, 76],
        [255, 255, 255],
        [50, 0, 106],
        K,
        [9, 9, 9],
        K,
        [29, 29, 29],
        K,
    ];

    let mut out = vec![0u8; (w * h * 3) as usize];
    for y in 0..h {
        for x in 0..w {
            let c = if y < top {
                top_colors[(0..7).rev().find(|&i| x >= seven(i)).unwrap_or(0) as usize]
            } else if y < top + mid {
                mid_colors[(0..7).rev().find(|&i| x >= seven(i)).unwrap_or(0) as usize]
            } else {
                let seg = (0..8).rev().find(|&i| x >= bottom_edges[i]).unwrap_or(0);
                bottom_colors[seg]
            };
            let o = ((y * w + x) * 3) as usize;
            out[o..o + 3].copy_from_slice(&c);
        }
    }
    out
}

/// Rows and columns where the bars change colour (the first pixel of each new bar), for tests that ignore the
/// blended pixels around an edge after scaling.
pub fn bar_edges(width: u32, height: u32) -> (Vec<u32>, Vec<u32>) {
    let rgb = smpte_bars(width, height);
    let px = |x: u32, y: u32| {
        let o = ((y * width + x) * 3) as usize;
        [rgb[o], rgb[o + 1], rgb[o + 2]]
    };
    let mut xs = Vec::new();
    for x in 1..width {
        if (0..height).any(|y| px(x, y) != px(x - 1, y)) {
            xs.push(x);
        }
    }
    let mut ys = Vec::new();
    for y in 1..height {
        if (0..width).any(|x| px(x, y) != px(x, y - 1)) {
            ys.push(y);
        }
    }
    (xs, ys)
}

/// One RGB pixel to 8 bit Y, Cb, Cr.
pub fn rgb_to_yuv8(space: ColorSpace, range: ColorRange, rgb: [f64; 3]) -> [u8; 3] {
    let rows = rgb_to_yuv_rows(space, range);
    let c = |row: [f64; 4]| {
        let v =
            row[0] * rgb[0] / 255.0 + row[1] * rgb[1] / 255.0 + row[2] * rgb[2] / 255.0 + row[3];
        (v * 255.0).round().clamp(0.0, 255.0) as u8
    };
    [c(rows[0]), c(rows[1]), c(rows[2])]
}

/// One 8 bit Y, Cb, Cr sample to RGB.
pub fn yuv8_to_rgb(space: ColorSpace, range: ColorRange, yuv: [u8; 3]) -> [u8; 3] {
    let rows = yuv_to_rgb_rows(space, range, 8);
    let s = [
        yuv[0] as f64 / 255.0,
        yuv[1] as f64 / 255.0,
        yuv[2] as f64 / 255.0,
    ];
    let c = |row: [f64; 4]| {
        ((row[0] * s[0] + row[1] * s[1] + row[2] * s[2] + row[3]) * 255.0)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    [c(rows[0]), c(rows[1]), c(rows[2])]
}

fn px(rgb: &[u8], w: u32, x: u32, y: u32) -> [f64; 3] {
    let o = ((y * w + x) * 3) as usize;
    [rgb[o] as f64, rgb[o + 1] as f64, rgb[o + 2] as f64]
}

/// Average of the 2x2 block at chroma position (cx, cy).
fn block(rgb: &[u8], w: u32, cx: u32, cy: u32) -> [f64; 3] {
    let mut s = [0.0; 3];
    for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
        let p = px(rgb, w, cx * 2 + dx, cy * 2 + dy);
        for i in 0..3 {
            s[i] += p[i] / 4.0;
        }
    }
    s
}

/// NV12: Y plane, then interleaved CbCr after `stride * luma_rows` bytes (`luma_rows >= height` leaves padding rows).
/// Chroma is the average of its 2x2 block. Unused bytes are 0xCD so a reader that strays shows up.
pub fn pack_nv12(
    rgb: &[u8],
    w: u32,
    h: u32,
    stride: usize,
    luma_rows: u32,
    space: ColorSpace,
    range: ColorRange,
) -> Vec<u8> {
    let lr = luma_rows.max(h) as usize;
    let mut out = vec![0xCDu8; stride * lr + stride * (lr / 2)];
    for y in 0..h {
        for x in 0..w {
            out[y as usize * stride + x as usize] = rgb_to_yuv8(space, range, px(rgb, w, x, y))[0];
        }
    }
    for cy in 0..h / 2 {
        for cx in 0..w / 2 {
            let yuv = rgb_to_yuv8(space, range, block(rgb, w, cx, cy));
            let o = stride * lr + cy as usize * stride + cx as usize * 2;
            out[o] = yuv[1];
            out[o + 1] = yuv[2];
        }
    }
    out
}

/// I420 (`v_first` false) or YV12 (`v_first` true): Y, then two quarter-size planes with half the stride.
pub fn pack_i420(
    rgb: &[u8],
    w: u32,
    h: u32,
    stride: usize,
    space: ColorSpace,
    range: ColorRange,
    v_first: bool,
) -> Vec<u8> {
    let cs = stride / 2;
    let (h, hh) = (h as usize, h as usize / 2);
    let mut out = vec![0xCDu8; stride * h + 2 * cs * hh];
    for y in 0..h as u32 {
        for x in 0..w {
            out[y as usize * stride + x as usize] = rgb_to_yuv8(space, range, px(rgb, w, x, y))[0];
        }
    }
    let (first, second) = (stride * h, stride * h + cs * hh);
    for cy in 0..hh as u32 {
        for cx in 0..w / 2 {
            let yuv = rgb_to_yuv8(space, range, block(rgb, w, cx, cy));
            let (uo, vo) = if v_first {
                (second, first)
            } else {
                (first, second)
            };
            out[uo + cy as usize * cs + cx as usize] = yuv[1];
            out[vo + cy as usize * cs + cx as usize] = yuv[2];
        }
    }
    out
}

/// YUY2: Y0 Cb Y1 Cr per pixel pair (chroma averaged over the pair horizontally).
pub fn pack_yuy2(
    rgb: &[u8],
    w: u32,
    h: u32,
    stride: usize,
    space: ColorSpace,
    range: ColorRange,
) -> Vec<u8> {
    let mut out = vec![0xCDu8; stride * h as usize];
    for y in 0..h {
        for cx in 0..w / 2 {
            let (a, b) = (px(rgb, w, cx * 2, y), px(rgb, w, cx * 2 + 1, y));
            let avg = [
                (a[0] + b[0]) / 2.0,
                (a[1] + b[1]) / 2.0,
                (a[2] + b[2]) / 2.0,
            ];
            let c = rgb_to_yuv8(space, range, avg);
            let o = y as usize * stride + cx as usize * 4;
            out[o] = rgb_to_yuv8(space, range, a)[0];
            out[o + 1] = c[1];
            out[o + 2] = rgb_to_yuv8(space, range, b)[0];
            out[o + 3] = c[2];
        }
    }
    out
}

/// P010: like NV12 with 16 bit little-endian samples holding a 10 bit code in the top bits. Limited range uses the
/// 10 bit limited definition (64..940 / 64..960).
pub fn pack_p010(
    rgb: &[u8],
    w: u32,
    h: u32,
    stride: usize,
    space: ColorSpace,
    range: ColorRange,
) -> Vec<u8> {
    let rows = rgb_to_yuv_rows(space, range);
    let scale = match range {
        ColorRange::Limited => 1020.0, // 255 * 4
        ColorRange::Full => 1023.0,
    };
    let code = |row: [f64; 4], c: [f64; 3]| -> u16 {
        let n = row[0] * c[0] / 255.0 + row[1] * c[1] / 255.0 + row[2] * c[2] / 255.0 + row[3];
        ((n * scale).round().clamp(0.0, 1023.0) as u16) << 6
    };
    let mut out = vec![0u8; stride * h as usize + stride * (h as usize / 2)];
    let put = |out: &mut Vec<u8>, o: usize, v: u16| out[o..o + 2].copy_from_slice(&v.to_le_bytes());
    for y in 0..h {
        for x in 0..w {
            put(
                &mut out,
                y as usize * stride + x as usize * 2,
                code(rows[0], px(rgb, w, x, y)),
            );
        }
    }
    for cy in 0..h / 2 {
        for cx in 0..w / 2 {
            let c = block(rgb, w, cx, cy);
            let o = stride * h as usize + cy as usize * stride + cx as usize * 4;
            put(&mut out, o, code(rows[1], c));
            put(&mut out, o + 2, code(rows[2], c));
        }
    }
    out
}

/// BGRA / BGRX bytes. `alpha` is written to the fourth byte.
pub fn pack_bgra(rgb: &[u8], w: u32, h: u32, stride: usize, alpha: u8) -> Vec<u8> {
    let mut out = vec![0xCDu8; stride * h as usize];
    for y in 0..h {
        for x in 0..w {
            let p = px(rgb, w, x, y);
            let o = y as usize * stride + x as usize * 4;
            out[o] = p[2] as u8;
            out[o + 1] = p[1] as u8;
            out[o + 2] = p[0] as u8;
            out[o + 3] = alpha;
        }
    }
    out
}

/// Decode NV12 (chroma after `stride * height` bytes) to tight RGB on the CPU, point sampling the chroma.
pub fn nv12_to_rgb(
    nv12: &[u8],
    w: u32,
    h: u32,
    stride: usize,
    space: ColorSpace,
    range: ColorRange,
) -> Vec<u8> {
    let uv = stride * h as usize;
    let mut out = vec![0u8; (w * h * 3) as usize];
    for y in 0..h as usize {
        for x in 0..w as usize {
            let yy = nv12[y * stride + x];
            let o = uv + (y / 2) * stride + (x / 2) * 2;
            let rgb = yuv8_to_rgb(space, range, [yy, nv12[o], nv12[o + 1]]);
            out[(y * w as usize + x) * 3..][..3].copy_from_slice(&rgb);
        }
    }
    out
}

/// BGRX with `stride` to tight RGB.
pub fn bgrx_to_rgb(bgrx: &[u8], w: u32, h: u32, stride: usize) -> Vec<u8> {
    let mut out = vec![0u8; (w * h * 3) as usize];
    for y in 0..h as usize {
        for x in 0..w as usize {
            let o = y * stride + x * 4;
            out[(y * w as usize + x) * 3..][..3].copy_from_slice(&[
                bgrx[o + 2],
                bgrx[o + 1],
                bgrx[o],
            ]);
        }
    }
    out
}

/// Largest per-channel difference between two equally sized byte images (`skip` is asked per pixel index and
/// excludes the pixel).
pub fn max_diff(a: &[u8], b: &[u8], channels: usize, skip: impl Fn(usize) -> bool) -> u8 {
    assert_eq!(a.len(), b.len());
    let mut worst = 0u8;
    for (i, (pa, pb)) in a.chunks(channels).zip(b.chunks(channels)).enumerate() {
        if skip(i) {
            continue;
        }
        for (x, y) in pa.iter().zip(pb) {
            worst = worst.max(x.abs_diff(*y));
        }
    }
    worst
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bars_have_the_expected_layout() {
        let (w, h) = (1920, 1080);
        let rgb = smpte_bars(w, h);
        let at = |x: u32, y: u32| {
            let o = ((y * w + x) * 3) as usize;
            [rgb[o], rgb[o + 1], rgb[o + 2]]
        };
        assert_eq!(at(10, 10), [191, 191, 191]);
        assert_eq!(at(w - 10, 10), [0, 0, 191]);
        assert_eq!(at(10, h - 10), [0, 33, 76]);
        assert_eq!(at(w * 3 / 14, h - 10), [255, 255, 255]);
        assert_eq!(at(10, h * 70 / 100), [0, 0, 191]);
        let (xs, ys) = bar_edges(w, h);
        assert!(xs.iter().all(|x| x % 2 == 0) && ys.iter().all(|y| y % 2 == 0));
        assert!(xs.len() >= 8 && ys.len() == 2);
    }

    #[test]
    fn reference_conversion_round_trips_primary_values() {
        for space in [ColorSpace::Bt601, ColorSpace::Bt709] {
            for range in [ColorRange::Limited, ColorRange::Full] {
                for rgb in [
                    [255.0, 0.0, 0.0],
                    [0.0, 255.0, 0.0],
                    [0.0, 0.0, 255.0],
                    [128.0, 128.0, 128.0],
                ] {
                    let yuv = rgb_to_yuv8(space, range, rgb);
                    let back = yuv8_to_rgb(space, range, yuv);
                    for i in 0..3 {
                        assert!(
                            (back[i] as f64 - rgb[i]).abs() <= 2.0,
                            "{space:?} {range:?} {rgb:?} -> {yuv:?} -> {back:?}"
                        );
                    }
                }
            }
        }
    }
}
