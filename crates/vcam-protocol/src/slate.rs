//! Slates: pre-rendered 1920x1080 NV12 images (dark gray, Better Cam mark, one line of text) that the DLL shows when
//! the app is not delivering frames. They are stored RLE-compressed in the DLL, decoded once, and scaled with
//! nearest neighbour to the negotiated size. `vcam-tool gen-slates` renders and encodes them; `capture-test`
//! compares captured frames against the same decode + scale.
//!
//! File format (little-endian): `"BCS1"`, `width u32`, `height u32`, then three planes (Y, U, V; U and V are
//! `width/2 x height/2`) each as `byte_len u32` followed by `(count u8 in 1..=255, value u8)` pairs.

pub const MAGIC: &[u8; 4] = b"BCS1";
/// Size every slate is rendered at.
pub const WIDTH: u32 = 1920;
pub const HEIGHT: u32 = 1080;

/// The three slates of plan 4.3.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SlateKind {
    /// "Better Cam is not running": file missing, bad magic, stale heartbeat, closed writer, hostile file.
    NotRunning,
    /// "Camera paused".
    Paused,
    /// "Starting camera…".
    Starting,
}

impl SlateKind {
    pub const ALL: [SlateKind; 3] = [
        SlateKind::NotRunning,
        SlateKind::Paused,
        SlateKind::Starting,
    ];
    pub fn file_stem(self) -> &'static str {
        match self {
            SlateKind::NotRunning => "not_running",
            SlateKind::Paused => "paused",
            SlateKind::Starting => "starting",
        }
    }
    pub fn text(self) -> &'static str {
        match self {
            SlateKind::NotRunning => "Better Cam is not running",
            SlateKind::Paused => "Camera paused",
            SlateKind::Starting => "Starting camera\u{2026}",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlateError {
    BadMagic,
    BadSize,
    Truncated,
    /// A plane decoded to a different length than its size says.
    BadPlane,
}

/// A decoded slate: tight NV12 (Y plane then interleaved UV, pitch = width).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Slate {
    pub width: u32,
    pub height: u32,
    pub nv12: Vec<u8>,
}

fn rle_encode(data: &[u8], out: &mut Vec<u8>) {
    let mut body = Vec::new();
    let mut i = 0;
    while i < data.len() {
        let v = data[i];
        let mut n = 1;
        while i + n < data.len() && data[i + n] == v && n < 255 {
            n += 1;
        }
        body.push(n as u8);
        body.push(v);
        i += n;
    }
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
}

#[allow(clippy::manual_is_multiple_of)] // MSRV 1.87: unsigned `is_multiple_of` is stable only from 1.90
fn rle_decode(bytes: &[u8], pos: &mut usize, expect: usize) -> Result<Vec<u8>, SlateError> {
    let len_bytes = bytes.get(*pos..*pos + 4).ok_or(SlateError::Truncated)?;
    let len = u32::from_le_bytes(len_bytes.try_into().unwrap()) as usize;
    *pos += 4;
    let body = bytes.get(*pos..*pos + len).ok_or(SlateError::Truncated)?;
    *pos += len;
    if len % 2 != 0 {
        return Err(SlateError::BadPlane);
    }
    let mut out = Vec::with_capacity(expect);
    for pair in body.chunks_exact(2) {
        let (n, v) = (pair[0] as usize, pair[1]);
        if n == 0 || out.len() + n > expect {
            return Err(SlateError::BadPlane);
        }
        out.resize(out.len() + n, v);
    }
    if out.len() != expect {
        return Err(SlateError::BadPlane);
    }
    Ok(out)
}

/// Encode a tight NV12 image.
pub fn encode(nv12: &[u8], width: u32, height: u32) -> Vec<u8> {
    let (w, h) = (width as usize, height as usize);
    assert!(w % 2 == 0 && h % 2 == 0 && nv12.len() == w * h * 3 / 2);
    let (y, uv) = nv12.split_at(w * h);
    let u: Vec<u8> = uv.iter().step_by(2).copied().collect();
    let v: Vec<u8> = uv.iter().skip(1).step_by(2).copied().collect();
    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&height.to_le_bytes());
    rle_encode(y, &mut out);
    rle_encode(&u, &mut out);
    rle_encode(&v, &mut out);
    out
}

/// Decode a slate file. Never panics on bad input.
pub fn decode(bytes: &[u8]) -> Result<Slate, SlateError> {
    if bytes.len() < 12 || &bytes[..4] != MAGIC {
        return Err(SlateError::BadMagic);
    }
    let width = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    let height = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    if width == 0
        || height == 0
        || width % 2 != 0
        || height % 2 != 0
        || width > 4096
        || height > 2304
    {
        return Err(SlateError::BadSize);
    }
    let (w, h) = (width as usize, height as usize);
    let mut pos = 12;
    let y = rle_decode(bytes, &mut pos, w * h)?;
    let u = rle_decode(bytes, &mut pos, w * h / 4)?;
    let v = rle_decode(bytes, &mut pos, w * h / 4)?;
    let mut nv12 = y;
    nv12.reserve(w * h / 2);
    for (a, b) in u.iter().zip(v.iter()) {
        nv12.push(*a);
        nv12.push(*b);
    }
    Ok(Slate {
        width,
        height,
        nv12,
    })
}

impl Slate {
    /// Nearest-neighbour scale to `width` x `height` (both even), tight NV12.
    #[allow(clippy::manual_is_multiple_of)] // MSRV 1.87: unsigned `is_multiple_of` is stable only from 1.90
    pub fn scale_nv12(&self, width: u32, height: u32) -> Vec<u8> {
        assert!(width % 2 == 0 && height % 2 == 0 && width > 0 && height > 0);
        let (sw, sh) = (self.width as usize, self.height as usize);
        let (w, h) = (width as usize, height as usize);
        let mut out = vec![0u8; w * h * 3 / 2];
        let xmap: Vec<usize> = (0..w).map(|x| x * sw / w).collect();
        let (src_y, src_uv) = self.nv12.split_at(sw * sh);
        let (dst_y, dst_uv) = out.split_at_mut(w * h);
        for y in 0..h {
            let sy = y * sh / h;
            let srow = &src_y[sy * sw..(sy + 1) * sw];
            for (d, &sx) in dst_y[y * w..(y + 1) * w].iter_mut().zip(&xmap) {
                *d = srow[sx];
            }
        }
        let (cw, ch, scw, sch) = (w / 2, h / 2, sw / 2, sh / 2);
        let cxmap: Vec<usize> = (0..cw).map(|x| x * scw / cw).collect();
        for y in 0..ch {
            let sy = y * sch / ch;
            let srow = &src_uv[sy * sw..(sy + 1) * sw];
            let drow = &mut dst_uv[y * w..(y + 1) * w];
            for (x, &sx) in cxmap.iter().enumerate() {
                drow[x * 2] = srow[sx * 2];
                drow[x * 2 + 1] = srow[sx * 2 + 1];
            }
        }
        out
    }

    /// Scale, then convert to BGRX (`pitch = width * 4`).
    pub fn scale_bgrx(&self, width: u32, height: u32) -> Vec<u8> {
        nv12_to_bgrx(&self.scale_nv12(width, height), width, height)
    }

    /// A flat dark-gray slate (used when an embedded slate cannot be decoded).
    pub fn flat(width: u32, height: u32) -> Slate {
        let (w, h) = (width as usize, height as usize);
        let mut nv12 = vec![42u8; w * h];
        nv12.extend(std::iter::repeat_n(128u8, w * h / 2));
        Slate {
            width,
            height,
            nv12,
        }
    }
}

/// BT.709 limited-range NV12 to BGRX, tight pitches.
pub fn nv12_to_bgrx(nv12: &[u8], width: u32, height: u32) -> Vec<u8> {
    let (w, h) = (width as usize, height as usize);
    let (y_plane, uv) = nv12.split_at(w * h);
    let mut out = vec![0u8; w * h * 4];
    for row in 0..h {
        let uv_row = &uv[(row / 2) * w..(row / 2) * w + w];
        for x in 0..w {
            let y = (y_plane[row * w + x] as f32 - 16.0) * 1.164_383;
            let u = uv_row[x / 2 * 2] as f32 - 128.0;
            let v = uv_row[x / 2 * 2 + 1] as f32 - 128.0;
            let r = y + 1.792_741 * v;
            let g = y - 0.213_249 * u - 0.532_909 * v;
            let b = y + 2.112_402 * u;
            let o = (row * w + x) * 4;
            out[o] = b.clamp(0.0, 255.0).round() as u8;
            out[o + 1] = g.clamp(0.0, 255.0).round() as u8;
            out[o + 2] = r.clamp(0.0, 255.0).round() as u8;
            out[o + 3] = 0xFF;
        }
    }
    out
}

/// Mean absolute difference of the luma of two frames of the same size and subtype (NV12: the Y plane; BGRX: the
/// smallest channel per pixel), compared at every `step`-th pixel in both directions. Rows may have different pitches.
pub fn mean_luma_diff(
    a: impl Fn(u32) -> Vec<u8>,
    b: impl Fn(u32) -> Vec<u8>,
    width: u32,
    height: u32,
    bgrx: bool,
    step: u32,
) -> f64 {
    let (mut sum, mut n) = (0u64, 0u64);
    for y in (0..height).step_by(step as usize) {
        let (ra, rb) = (a(y), b(y));
        for x in (0..width as usize).step_by(step as usize) {
            let (pa, pb) = if bgrx {
                (
                    ra[x * 4].min(ra[x * 4 + 1]).min(ra[x * 4 + 2]),
                    rb[x * 4].min(rb[x * 4 + 1]).min(rb[x * 4 + 2]),
                )
            } else {
                (ra[x], rb[x])
            };
            sum += pa.abs_diff(pb) as u64;
            n += 1;
        }
    }
    sum as f64 / n.max(1) as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(w: u32, h: u32) -> Slate {
        let (wu, hu) = (w as usize, h as usize);
        let mut nv12 = vec![40u8; wu * hu];
        // a bright block and a gradient so runs of every length occur
        for y in 10..30 {
            for x in 5..300.min(wu) {
                nv12[y * wu + x] = 200;
            }
        }
        for (i, p) in nv12.iter_mut().enumerate().skip(wu * 50).take(1000) {
            *p = (i % 251) as u8;
        }
        let mut uv = Vec::new();
        for i in 0..wu * hu / 4 {
            uv.push(100 + (i / 500 % 50) as u8);
            uv.push(128);
        }
        nv12.extend(uv);
        Slate {
            width: w,
            height: h,
            nv12,
        }
    }

    #[test]
    fn rle_round_trips() {
        let s = sample(320, 180);
        let enc = encode(&s.nv12, 320, 180);
        assert!(
            enc.len() < s.nv12.len() / 4,
            "flat areas must compress: {} bytes",
            enc.len()
        );
        assert_eq!(decode(&enc).unwrap(), s);
    }

    #[test]
    fn long_runs_split_at_255() {
        let s = Slate::flat(1000, 100);
        let enc = encode(&s.nv12, 1000, 100);
        assert_eq!(decode(&enc).unwrap(), s);
    }

    #[test]
    fn decode_rejects_garbage_without_panicking() {
        assert_eq!(decode(b"").unwrap_err(), SlateError::BadMagic);
        assert_eq!(decode(b"XXXX00000000").unwrap_err(), SlateError::BadMagic);
        let mut bad = Vec::new();
        bad.extend_from_slice(MAGIC);
        bad.extend_from_slice(&[0, 0, 0, 0, 10, 0, 0, 0]);
        assert_eq!(decode(&bad).unwrap_err(), SlateError::BadSize);
        let good = encode(&sample(64, 32).nv12, 64, 32);
        for cut in [12, 13, 20, good.len() - 1] {
            assert!(decode(&good[..cut]).is_err(), "cut at {cut}");
        }
        // zero-length run and run past the plane
        let mut zero_run = good.clone();
        zero_run[16] = 0;
        assert_eq!(decode(&zero_run).unwrap_err(), SlateError::BadPlane);
        let mut huge = Vec::new();
        huge.extend_from_slice(MAGIC);
        huge.extend_from_slice(&64u32.to_le_bytes());
        huge.extend_from_slice(&32u32.to_le_bytes());
        huge.extend_from_slice(&0xFFFF_FFF0u32.to_le_bytes());
        assert_eq!(decode(&huge).unwrap_err(), SlateError::Truncated);
    }

    #[test]
    fn scaling_keeps_size_and_corners() {
        let s = sample(320, 180);
        let same = s.scale_nv12(320, 180);
        assert_eq!(same, s.nv12);
        let small = s.scale_nv12(160, 90);
        assert_eq!(small.len(), 160 * 90 * 3 / 2);
        // the bright block at (5..300, 10..30) maps to (2..150, 5..15)
        assert_eq!(small[7 * 160 + 50], 200);
        assert_eq!(small[60 * 160 + 80], 40);
        let big = s.scale_nv12(640, 360);
        assert_eq!(big.len(), 640 * 360 * 3 / 2);
        assert_eq!(big[30 * 640 + 100], 200);
    }

    #[test]
    fn bgrx_conversion_of_known_colours() {
        // 4x2 frame: first row video black, second row video white, neutral chroma
        let nv12 = [16, 16, 16, 16, 235, 235, 235, 235, 128, 128, 128, 128];
        let px = nv12_to_bgrx(&nv12, 4, 2);
        assert_eq!(px.len(), 4 * 2 * 4);
        assert_eq!(&px[..4], &[0, 0, 0, 255]);
        assert_eq!(&px[16..20], &[255, 255, 255, 255]);
    }

    #[test]
    fn mean_luma_diff_is_zero_for_equal_frames() {
        let s = sample(64, 32);
        let row = |y: u32| s.nv12[y as usize * 64..(y as usize + 1) * 64].to_vec();
        assert_eq!(mean_luma_diff(row, row, 64, 32, false, 1), 0.0);
        let other = |y: u32| {
            s.nv12[y as usize * 64..(y as usize + 1) * 64]
                .iter()
                .map(|v| v.wrapping_add(4))
                .collect()
        };
        let d = mean_luma_diff(row, other, 64, 32, false, 1);
        assert!((d - 4.0).abs() < 1.0, "{d}");
    }
}
