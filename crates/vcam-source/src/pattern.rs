//! P1 test pattern: scrolling colour bars (no white) plus a white box whose left edge encodes the frame number.
//! Geometry lives in `vcam_protocol::pattern`. NV12 is BT.709 limited range; RGB32 is BGRX.

use vcam_protocol::pattern::{box_x, box_y, BAR_COUNT, BAR_SCROLL, BOX_SIZE, WHITE_Y};

/// 75 % bars minus white: gray, yellow, cyan, green, magenta, red, blue.
const BARS_RGB: [[u8; 3]; BAR_COUNT as usize] = [
    [191, 191, 191],
    [191, 191, 0],
    [0, 191, 191],
    [0, 191, 0],
    [191, 0, 191],
    [191, 0, 0],
    [0, 0, 191],
];

/// BT.709 limited-range Y'CbCr for 8-bit full-range R'G'B'.
pub fn rgb_to_yuv709(rgb: [u8; 3]) -> (u8, u8, u8) {
    let (r, g, b) = (rgb[0] as f32, rgb[1] as f32, rgb[2] as f32);
    let y = 16.0 + 219.0 * (0.2126 * r + 0.7152 * g + 0.0722 * b) / 255.0;
    let u =
        128.0 + 224.0 * (-0.2126 / 1.8556 * r - 0.7152 / 1.8556 * g + 0.9278 / 1.8556 * b) / 255.0;
    let v =
        128.0 + 224.0 * (0.7874 / 1.5748 * r - 0.7152 / 1.5748 * g - 0.0722 / 1.5748 * b) / 255.0;
    (y.round() as u8, u.round() as u8, v.round() as u8)
}

fn bar_width(width: u32) -> u32 {
    width.div_ceil(BAR_COUNT)
}

fn bar_at(x: u32, scroll: u32, width: u32) -> usize {
    (((x + scroll) / bar_width(width)) % BAR_COUNT) as usize
}

/// Number of bytes an NV12 frame needs for the given pitch (Y plane then interleaved UV, same pitch).
pub fn nv12_len(height: u32, pitch: usize) -> usize {
    pitch * height as usize * 3 / 2
}

/// Fill an NV12 frame. `buf` starts at the first scanline; the UV plane follows the Y plane at the same pitch.
/// `buf` must hold at least [`nv12_len`] bytes.
pub fn fill_nv12(frame: u64, width: u32, height: u32, buf: &mut [u8], pitch: usize) {
    let (w, h) = (width as usize, height as usize);
    assert!(pitch >= w && buf.len() >= nv12_len(height, pitch));
    let scroll = ((frame % 1_000_000) as u32) * BAR_SCROLL;

    let mut y_row = vec![0u8; w];
    let mut uv_row = vec![0u8; w];
    let yuv: Vec<(u8, u8, u8)> = BARS_RGB.iter().map(|c| rgb_to_yuv709(*c)).collect();
    for x in 0..w {
        y_row[x] = yuv[bar_at(x as u32, scroll, width)].0;
    }
    for x in (0..w).step_by(2) {
        let (_, u, v) = yuv[bar_at(x as u32, scroll, width)];
        uv_row[x] = u;
        if x + 1 < w {
            uv_row[x + 1] = v;
        }
    }

    let bx = box_x(width, frame) as usize;
    let by = box_y(height) as usize;
    let bs = BOX_SIZE as usize;

    let (y_plane, uv_plane) = buf.split_at_mut(pitch * h);
    for row in 0..h {
        let dst = &mut y_plane[row * pitch..row * pitch + w];
        dst.copy_from_slice(&y_row);
        if row >= by && row < by + bs {
            dst[bx..bx + bs].fill(WHITE_Y);
        }
    }
    for row in 0..h / 2 {
        let dst = &mut uv_plane[row * pitch..row * pitch + w];
        dst.copy_from_slice(&uv_row);
        let luma_row = row * 2;
        if luma_row >= by && luma_row < by + bs {
            dst[bx..bx + bs].fill(128);
        }
    }
}

/// Fill a BGRX (`MFVideoFormat_RGB32`) frame, top-down. `buf` holds at least `pitch * height` bytes.
pub fn fill_bgrx(frame: u64, width: u32, height: u32, buf: &mut [u8], pitch: usize) {
    let (w, h) = (width as usize, height as usize);
    assert!(pitch >= w * 4 && buf.len() >= pitch * h);
    let scroll = ((frame % 1_000_000) as u32) * BAR_SCROLL;

    let mut row_tpl = vec![0u8; w * 4];
    for x in 0..w {
        let c = BARS_RGB[bar_at(x as u32, scroll, width)];
        row_tpl[x * 4..x * 4 + 4].copy_from_slice(&[c[2], c[1], c[0], 0xFF]);
    }
    let bx = box_x(width, frame) as usize;
    let by = box_y(height) as usize;
    let bs = BOX_SIZE as usize;
    for row in 0..h {
        let dst = &mut buf[row * pitch..row * pitch + w * 4];
        dst.copy_from_slice(&row_tpl);
        if row >= by && row < by + bs {
            dst[bx * 4..(bx + bs) * 4].fill(0xFF);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vcam_protocol::pattern::{slot_from_box_x, slots, DETECT_LUMA};

    /// Left edge of the first run of >= BOX_SIZE/2 bright luma pixels on the middle row of the box band.
    fn detect_nv12(buf: &[u8], width: u32, height: u32, pitch: usize) -> Option<u32> {
        let row = box_y(height) as usize + BOX_SIZE as usize / 2;
        let line = &buf[row * pitch..row * pitch + width as usize];
        let min_run = BOX_SIZE as usize / 2;
        let mut run = 0;
        for (x, &y) in line.iter().enumerate() {
            if y >= DETECT_LUMA {
                run += 1;
                if run == min_run {
                    return Some((x + 1 - run) as u32);
                }
            } else {
                run = 0;
            }
        }
        None
    }

    #[test]
    fn nv12_box_encodes_frame_number() {
        for &(w, h) in &[(1920u32, 1080u32), (1280, 720)] {
            let pitch = w as usize + 64; // exercise a padded pitch
            let mut buf = vec![0u8; nv12_len(h, pitch)];
            for frame in [0u64, 1, 2, 57, slots(w) as u64 - 1, slots(w) as u64, 1000] {
                fill_nv12(frame, w, h, &mut buf, pitch);
                let x = detect_nv12(&buf, w, h, pitch).expect("box not found");
                assert_eq!(x, box_x(w, frame));
                assert_eq!(slot_from_box_x(x) as u64, frame % slots(w) as u64);
            }
        }
    }

    #[test]
    fn nv12_bars_are_never_bright_and_never_black() {
        let (w, h) = (1280u32, 720u32);
        let mut buf = vec![0u8; nv12_len(h, w as usize)];
        fill_nv12(3, w, h, &mut buf, w as usize);
        let by = box_y(h) as usize;
        // a row far from the box band: every pixel is a bar, none bright enough to be mistaken for the box
        let row = &buf[(by + 200) * w as usize..(by + 201) * w as usize];
        assert!(row.iter().all(|&y| (16..DETECT_LUMA).contains(&y)));
        assert!(buf[..w as usize * h as usize].iter().any(|&y| y > 100));
    }

    #[test]
    fn bars_scroll_between_frames() {
        let (w, h) = (1280u32, 720u32);
        let mut a = vec![0u8; nv12_len(h, w as usize)];
        let mut b = a.clone();
        fill_nv12(0, w, h, &mut a, w as usize);
        fill_nv12(1, w, h, &mut b, w as usize);
        assert_ne!(a[..w as usize], b[..w as usize]);
    }

    #[test]
    fn bgrx_box_is_white_and_bars_are_not() {
        let (w, h) = (1280u32, 720u32);
        let pitch = w as usize * 4;
        let mut buf = vec![0u8; pitch * h as usize];
        fill_bgrx(5, w, h, &mut buf, pitch);
        let row = box_y(h) as usize + 32;
        let bx = box_x(w, 5) as usize;
        let px = |x: usize| &buf[row * pitch + x * 4..row * pitch + x * 4 + 4];
        assert_eq!(px(bx), [255, 255, 255, 255]);
        assert_eq!(px(bx + 63), [255, 255, 255, 255]);
        assert_ne!(px(bx + 64)[..3], [255, 255, 255]);
    }

    #[test]
    fn white_luma_matches_constant() {
        assert_eq!(rgb_to_yuv709([255, 255, 255]).0, WHITE_Y);
        assert_eq!(rgb_to_yuv709([0, 0, 0]).0, 16);
        let (y, u, v) = rgb_to_yuv709([128, 128, 128]);
        assert_eq!((u, v), (128, 128));
        assert!((y as i32 - 126).abs() <= 1);
    }
}
