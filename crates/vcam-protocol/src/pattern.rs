//! The P2 test pattern: a checkerboard with one full-height white stripe whose left edge encodes the frame number.
//!
//! `vcam-tool feed` draws it, `vcam-tool capture-test` and the in-process DLL tests analyse it. The stripe is
//! measured on three rows (top, middle, bottom): a frame that mixes two published frames (a torn frame) shows the
//! stripe at two different places. Colours are neutral gray, so NV12 chroma is 128 everywhere.

use crate::header::SUBTYPE_RGB32;

/// Side of a checkerboard cell in pixels.
pub const CELL: u32 = 64;
/// Width of the stripe in pixels.
pub const STRIPE_W: u32 = 32;
/// Horizontal move of the stripe per frame in pixels.
pub const STRIPE_STEP: u32 = 16;
/// Dark and light cell, full-range RGB (gray).
pub const DARK_RGB: u8 = 64;
pub const LIGHT_RGB: u8 = 160;
/// The same cells as BT.709 limited-range luma.
pub const DARK_Y: u8 = 71;
pub const LIGHT_Y: u8 = 153;
/// Stripe: white.
pub const STRIPE_Y: u8 = 235;
pub const STRIPE_RGB: u8 = 255;

/// Number of distinct stripe positions for a frame of this width.
pub fn slots(width: u32) -> u32 {
    width.saturating_sub(STRIPE_W) / STRIPE_STEP
}
/// Left edge of the stripe for frame number `frame`.
pub fn stripe_x(width: u32, frame: u64) -> u32 {
    (frame % slots(width).max(1) as u64) as u32 * STRIPE_STEP
}
/// Frame number modulo `slots` a measured left edge belongs to (tolerates a few pixels of error).
pub fn slot_from_x(x: u32) -> u32 {
    (x + STRIPE_STEP / 2) / STRIPE_STEP
}

fn is_dark_cell(col: u32, row: u32) -> bool {
    (col / CELL + row / CELL) % 2 == 0
}

/// Bytes an NV12 frame needs for the given pitch.
pub fn nv12_len(height: u32, pitch: usize) -> usize {
    pitch * height as usize * 3 / 2
}

/// Fill an NV12 frame. `buf` starts at the first scanline; the UV plane follows the Y plane at the same pitch.
pub fn fill_nv12(frame: u64, width: u32, height: u32, buf: &mut [u8], pitch: usize) {
    let (w, h) = (width as usize, height as usize);
    assert!(pitch >= w && buf.len() >= nv12_len(height, pitch));
    let sx = stripe_x(width, frame) as usize;
    let sw = STRIPE_W as usize;
    // two row templates: the board has two kinds of rows
    let mut rows = [vec![0u8; w], vec![0u8; w]];
    for (k, row) in rows.iter_mut().enumerate() {
        for (x, px) in row.iter_mut().enumerate() {
            *px = if is_dark_cell(x as u32, k as u32 * CELL) {
                DARK_Y
            } else {
                LIGHT_Y
            };
        }
        row[sx..sx + sw].fill(STRIPE_Y);
    }
    let (y_plane, uv_plane) = buf.split_at_mut(pitch * h);
    for row in 0..h {
        let k = (row as u32 / CELL % 2) as usize;
        y_plane[row * pitch..row * pitch + w].copy_from_slice(&rows[k]);
    }
    for row in 0..h / 2 {
        uv_plane[row * pitch..row * pitch + w].fill(128);
    }
}

/// Fill a BGRX (`MFVideoFormat_RGB32`) frame, top-down.
pub fn fill_bgrx(frame: u64, width: u32, height: u32, buf: &mut [u8], pitch: usize) {
    let (w, h) = (width as usize, height as usize);
    assert!(pitch >= w * 4 && buf.len() >= pitch * h);
    let sx = stripe_x(width, frame) as usize;
    let sw = STRIPE_W as usize;
    let mut rows = [vec![0u8; w * 4], vec![0u8; w * 4]];
    for (k, row) in rows.iter_mut().enumerate() {
        for x in 0..w {
            let g = if x >= sx && x < sx + sw {
                STRIPE_RGB
            } else if is_dark_cell(x as u32, k as u32 * CELL) {
                DARK_RGB
            } else {
                LIGHT_RGB
            };
            row[x * 4..x * 4 + 4].copy_from_slice(&[g, g, g, 0xFF]);
        }
    }
    for row in 0..h {
        let k = (row as u32 / CELL % 2) as usize;
        buf[row * pitch..row * pitch + w * 4].copy_from_slice(&rows[k]);
    }
}

/// What [`analyse`] found in one frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Analysis {
    /// Stripe slot measured on the top, middle and bottom row (`None`: no stripe found on that row).
    pub stripes: [Option<u32>; 3],
    /// Every sampled checkerboard cell has the expected gray.
    pub board_ok: bool,
    /// The middle row has no pixel above black level.
    pub black: bool,
}

impl Analysis {
    /// The slot when all three rows agree.
    pub fn slot(&self) -> Option<u32> {
        match self.stripes {
            [Some(a), Some(b), Some(c)] if a == b && b == c => Some(a),
            _ => None,
        }
    }
    /// Some rows saw a stripe and they disagree: two frames mixed in one.
    pub fn torn(&self) -> bool {
        let seen: Vec<u32> = self.stripes.iter().flatten().copied().collect();
        seen.len() >= 2 && seen.iter().any(|&s| s != seen[0])
    }
}

/// Brightness of pixel `x` on a row: NV12 luma, or the smallest channel of a BGRX pixel.
fn bright(row: &[u8], x: usize, subtype: u32) -> u8 {
    if subtype == SUBTYPE_RGB32 {
        row[x * 4].min(row[x * 4 + 1]).min(row[x * 4 + 2])
    } else {
        row[x]
    }
}

/// Analyse a frame. `row(r)` returns scanline `r` of the luma (NV12) or pixel (BGRX) data: at least `width` bytes
/// (NV12) or `width * 4` bytes (BGRX).
pub fn analyse<'a>(
    row: impl Fn(u32) -> &'a [u8],
    subtype: u32,
    width: u32,
    height: u32,
) -> Analysis {
    let detect: u8 = if subtype == SUBTYPE_RGB32 { 210 } else { 200 };
    let rows = [height / 8, height / 2, height - 1 - height / 8];
    let mut stripes = [None; 3];
    let mut stripe_edge = None;
    for (i, &r) in rows.iter().enumerate() {
        let line = row(r);
        let mut run = 0u32;
        for x in 0..width as usize {
            if bright(line, x, subtype) >= detect {
                run += 1;
                if run == STRIPE_W / 2 {
                    let left = x as u32 + 1 - run;
                    stripes[i] = Some(slot_from_x(left));
                    stripe_edge.get_or_insert(left);
                    break;
                }
            } else {
                run = 0;
            }
        }
    }

    // checkerboard: sample the centre of some cells away from the stripe
    let (dark, light) = if subtype == SUBTYPE_RGB32 {
        (DARK_RGB, LIGHT_RGB)
    } else {
        (DARK_Y, LIGHT_Y)
    };
    let mut board_ok = true;
    let mut sampled = 0;
    let cols = (width / CELL).max(1);
    let rws = (height / CELL).max(1);
    for cy in (0..rws).step_by(2) {
        for cx in (0..cols).step_by(3) {
            let (px, py) = (cx * CELL + CELL / 2, cy * CELL + CELL / 2);
            if py >= height || px >= width {
                continue;
            }
            if let Some(e) = stripe_edge {
                if px + 4 > e && px < e + STRIPE_W + 4 {
                    continue;
                }
            }
            let want = if is_dark_cell(px, py) { dark } else { light };
            let got = bright(row(py), px as usize, subtype);
            sampled += 1;
            if got.abs_diff(want) > 10 {
                board_ok = false;
            }
        }
    }
    if sampled == 0 {
        board_ok = false;
    }

    let mid = row(height / 2);
    let limit: u8 = if subtype == SUBTYPE_RGB32 { 8 } else { 24 };
    let black = (0..width as usize).all(|x| bright(mid, x, subtype) <= limit);
    Analysis {
        stripes,
        board_ok,
        black,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::header::SUBTYPE_NV12;

    fn nv12(w: u32, h: u32, pitch: usize, frame: u64) -> Vec<u8> {
        let mut b = vec![0u8; nv12_len(h, pitch)];
        fill_nv12(frame, w, h, &mut b, pitch);
        b
    }

    #[test]
    fn nv12_stripe_encodes_frame_number_and_board_is_intact() {
        for &(w, h) in &[(1920u32, 1080u32), (1280, 720), (640, 360)] {
            let pitch = w as usize + 64;
            for frame in [
                0u64,
                1,
                2,
                57,
                slots(w) as u64 - 1,
                slots(w) as u64,
                100_000,
            ] {
                let b = nv12(w, h, pitch, frame);
                let a = analyse(
                    |r| &b[r as usize * pitch..r as usize * pitch + w as usize],
                    SUBTYPE_NV12,
                    w,
                    h,
                );
                assert_eq!(
                    a.slot(),
                    Some((frame % slots(w) as u64) as u32),
                    "{w}x{h} frame {frame}"
                );
                assert!(a.board_ok && !a.black && !a.torn(), "{a:?}");
            }
        }
    }

    #[test]
    fn bgrx_stripe_and_board() {
        let (w, h) = (1280u32, 720u32);
        let pitch = w as usize * 4;
        let mut b = vec![0u8; pitch * h as usize];
        fill_bgrx(77, w, h, &mut b, pitch);
        let a = analyse(
            |r| &b[r as usize * pitch..(r as usize + 1) * pitch],
            SUBTYPE_RGB32,
            w,
            h,
        );
        assert_eq!(a.slot(), Some(77 % slots(w)));
        assert!(a.board_ok && !a.black);
    }

    #[test]
    fn a_frame_made_of_two_frames_is_torn() {
        let (w, h) = (1280u32, 720u32);
        let pitch = w as usize;
        let a = nv12(w, h, pitch, 10);
        let b = nv12(w, h, pitch, 11);
        let mut mixed = a.clone();
        // bottom third from the next frame
        let from = pitch * (h as usize * 2 / 3);
        mixed[from..pitch * h as usize].copy_from_slice(&b[from..pitch * h as usize]);
        let r = analyse(
            |r| &mixed[r as usize * pitch..(r as usize + 1) * pitch],
            SUBTYPE_NV12,
            w,
            h,
        );
        assert!(r.torn(), "{r:?}");
        assert_eq!(r.slot(), None);
    }

    #[test]
    fn black_and_flat_frames_are_detected() {
        let (w, h) = (640u32, 360u32);
        let pitch = w as usize;
        let zero = vec![0u8; nv12_len(h, pitch)];
        let r = analyse(
            |r| &zero[r as usize * pitch..(r as usize + 1) * pitch],
            SUBTYPE_NV12,
            w,
            h,
        );
        assert!(r.black && !r.board_ok && r.slot().is_none());
        let gray = vec![100u8; nv12_len(h, pitch)];
        let r = analyse(
            |r| &gray[r as usize * pitch..(r as usize + 1) * pitch],
            SUBTYPE_NV12,
            w,
            h,
        );
        assert!(!r.black && !r.board_ok && r.stripes == [None; 3]);
    }

    #[test]
    fn stripe_stays_inside_the_frame() {
        for w in [640u32, 1280, 1920] {
            for f in 0..(slots(w) as u64 * 2 + 3) {
                assert!(stripe_x(w, f) + STRIPE_W <= w);
            }
        }
    }
}
