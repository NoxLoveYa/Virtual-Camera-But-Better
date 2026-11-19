//! Frames recorded from a real camera (a W4GS in Better Cam auto white balance, the owner's desk: a dim room, a bright
//! window, a face in the lower middle, a lamp that comes on). The fixture is the 16 x 16 statistics image the meter
//! saw, per frame: `t` (s on the recording's clock), the Kelvin the camera had, and the cells. Tests only.
//!
//! Format of `fixtures/real_room.txt` (docs/AUTO-WHITE-BALANCE.md 9): a `#` header line, then one line per frame,
//! `t k cells`. The cells are 256 x (R, G, B), each sRGB coded to 12 bits, two values to three bytes, in base64.

use super::scenes::srgb_decode;
use crate::fx::auto_adjust::Stats;

const TEXT: &str = include_str!("fixtures/real_room.txt");
const CELLS: usize = 256;

pub struct Frame {
    pub t: f64,
    /// The camera's white balance while this frame was taken.
    pub k: i32,
    pub stats: Stats,
}

fn b64(c: u8) -> u32 {
    match c {
        b'A'..=b'Z' => u32::from(c - b'A'),
        b'a'..=b'z' => u32::from(c - b'a') + 26,
        b'0'..=b'9' => u32::from(c - b'0') + 52,
        b'+' => 62,
        _ => 63,
    }
}

fn decode(cells: &str) -> Stats {
    let mut bytes = Vec::with_capacity(cells.len() * 3 / 4);
    for q in cells.as_bytes().chunks(4) {
        let n = q.iter().fold(0u32, |n, c| (n << 6) | b64(*c));
        bytes.extend([(n >> 16) as u8, (n >> 8) as u8, n as u8]);
    }
    let mut values = bytes.chunks(3).flat_map(|b| {
        let (a, c) = (
            (u32::from(b[0]) << 4) | (u32::from(b[1]) >> 4),
            (u32::from(b[1] & 15) << 8) | u32::from(b[2]),
        );
        [a, c]
    });
    let mut next = || srgb_decode(f64::from(values.next().expect("768 values")) / 4095.0) as f32;
    let cells = (0..CELLS).map(|_| [next(), next(), next(), 1.0]).collect();
    Stats { cells }
}

/// Every recorded frame, in time order.
pub fn frames() -> Vec<Frame> {
    TEXT.lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .map(|l| {
            let mut p = l.split(' ');
            let t = p.next().and_then(|v| v.parse().ok()).expect("t");
            let k = p.next().and_then(|v| v.parse().ok()).expect("k");
            Frame {
                t,
                k,
                stats: decode(p.next().expect("cells")),
            }
        })
        .collect()
}

/// The frames of the recording between `a` and `b` seconds.
pub fn between(a: f64, b: f64) -> Vec<Frame> {
    frames()
        .into_iter()
        .filter(|f| f.t >= a && f.t < b)
        .collect()
}
