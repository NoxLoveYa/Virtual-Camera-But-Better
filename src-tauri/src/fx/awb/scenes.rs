//! The synthetic scene kit of docs/AUTO-WHITE-BALANCE.md 5.1 (test only; the device loop's simulator reuses it).
//!
//! Diagonal (von Kries) model in linear sRGB: a surface of colour `rho` lit by a blackbody of `T` on a camera whose
//! white balance is set to `K` records `rho * bb(T) / bb(K)`. The camera picture is the 16 x 16 cell means the
//! statistics chain delivers: 16 sub-samples per cell with a texture, clipped at 1, 8-bit sRGB coded and decoded.
//! The layouts are fixed in code, ported from the analysis model.

use crate::fx::auto_adjust::{Stats, GRID};

const CELLS: usize = GRID * GRID;

/// The lights of the suite, in kelvin.
pub const LIGHTS: [f64; 10] = [
    2000.0, 2200.0, 2500.0, 2700.0, 3000.0, 3500.0, 4000.0, 5000.0, 5500.0, 6500.0,
];

/// The scenes of the suite.
pub const SCENES: [&str; 12] = [
    "office",
    "office_dark_skin",
    "warm_room",
    "grey_room",
    "checker",
    "closeup",
    "face_fill",
    "face_fill_dark",
    "red_wall",
    "blue_wall",
    "green_screen",
    "orange_wall",
];

/// The scenes that are one colour (a face or a wall fills the frame).
pub const ONE_COLOUR: [&str; 6] = [
    "face_fill",
    "face_fill_dark",
    "red_wall",
    "blue_wall",
    "green_screen",
    "orange_wall",
];

/// The 24 ColorChecker patches as 8-bit sRGB codes.
const CHECKER: [[u8; 3]; 24] = [
    [115, 82, 68],
    [194, 150, 130],
    [98, 122, 157],
    [87, 108, 67],
    [133, 128, 177],
    [103, 189, 170],
    [214, 126, 44],
    [80, 91, 166],
    [193, 90, 99],
    [94, 60, 108],
    [157, 188, 64],
    [224, 163, 46],
    [56, 61, 150],
    [70, 148, 73],
    [175, 54, 60],
    [231, 199, 31],
    [187, 86, 149],
    [8, 133, 161],
    [243, 243, 242],
    [200, 200, 200],
    [160, 160, 160],
    [122, 122, 121],
    [85, 85, 85],
    [52, 52, 52],
];

pub fn srgb_encode(l: f64) -> f64 {
    let c = l.clamp(0.0, 1.0);
    if c <= 0.003_130_8 {
        12.92 * c
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

pub fn srgb_decode(c: f64) -> f64 {
    let c = c.clamp(0.0, 1.0);
    if c <= 0.040_45 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

pub fn luma(c: [f64; 3]) -> f64 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

/// The linear sRGB colour of a blackbody of `kelvin` with the luminance 1 (shared with Auto Adjust's warmth aim).
pub(crate) use crate::fx::auto_adjust::blackbody_rgb;

/// A reflectance: the 8-bit sRGB code of a surface under neutral light, decoded to linear.
fn lin(code: [u8; 3]) -> [f64; 3] {
    code.map(|c| srgb_decode(f64::from(c) / 255.0))
}

/// The reflectance of a named surface.
pub fn surface(name: &str) -> [f64; 3] {
    lin(match name {
        "wall_grey" => [200, 200, 196],
        "wall_white" => [232, 232, 226],
        "wall_beige" => [215, 196, 170],
        "paper" => [242, 242, 238],
        "skin_light" => [194, 150, 130],
        "skin_mid" => [170, 120, 95],
        "skin_dark" => [115, 82, 68],
        "hair_dark" => [55, 42, 35],
        "shirt_blue" => [60, 80, 140],
        "shirt_grey" => [110, 110, 112],
        "wood" => [150, 105, 65],
        "plant" => [70, 110, 60],
        "mug_red" => [175, 54, 60],
        "sofa" => [95, 62, 45],
        "wall_red" => [170, 60, 55],
        "wall_blue" => [60, 80, 160],
        "green_screen" => [60, 160, 70],
        "orange_wall" => [220, 140, 70],
        other => panic!("no surface {other}"),
    })
}

/// One cell of a scene: what is there and its reflectance.
#[derive(Clone, Copy, Debug)]
pub struct Cell {
    pub name: &'static str,
    pub rho: [f64; 3],
}

fn named(name: &'static str) -> Cell {
    Cell { name, rho: surface(name) }
}

fn checker_cell(i: usize) -> Cell {
    Cell { name: "checker", rho: lin(CHECKER[i % 24]) }
}

/// The cells of a face oval (centre `(cx, cy)`, radii `rx`, `ry` in cells).
fn face_mask(cx: f64, cy: f64, rx: f64, ry: f64) -> [bool; CELLS] {
    let mut m = [false; CELLS];
    for (i, v) in m.iter_mut().enumerate() {
        let (x, y) = ((i % GRID) as f64, (i / GRID) as f64);
        *v = ((x - cx) / rx).powi(2) + ((y - cy) / ry).powi(2) <= 1.0;
    }
    m
}

/// The 16 x 16 reflectances of a named scene (row by row).
pub fn scene(name: &str) -> Vec<Cell> {
    let face = face_mask(7.5, 7.5, 3.2, 4.2);
    let close = face_mask(7.5, 8.5, 5.0, 7.0);
    let mut cells = Vec::with_capacity(CELLS);
    for (i, &in_close) in close.iter().enumerate() {
        let (x, y) = (i % GRID, i / GRID);
        let c = match name {
            "office" | "office_dark_skin" => {
                let mut c = named("wall_grey");
                if x >= 13 {
                    c = checker_cell(y * 3 + x); // a bookshelf of coloured things
                }
                if y >= 13 {
                    c = named("wood");
                }
                if x <= 1 && (6..=12).contains(&y) {
                    c = named("plant");
                }
                if y >= 11 && (4..=11).contains(&x) {
                    c = named("shirt_blue");
                }
                if (x, y) == (2, 13) {
                    c = named("paper");
                }
                if (x, y) == (12, 13) {
                    c = named("mug_red");
                }
                c
            }
            "warm_room" => {
                let mut c = named("wall_beige");
                if y >= 12 {
                    c = named("wood");
                }
                if x <= 2 && y >= 8 {
                    c = named("sofa");
                }
                if y >= 11 && (4..=11).contains(&x) {
                    c = named("shirt_grey");
                }
                c
            }
            "grey_room" => {
                if y < 8 {
                    named("wall_white")
                } else {
                    named("shirt_grey")
                }
            }
            "closeup" => {
                let mut c = named(if x < 12 { "wall_grey" } else { "wall_beige" });
                if in_close {
                    c = named(if y <= 3 { "hair_dark" } else { "skin_light" });
                }
                if y >= 14 && (3..=12).contains(&x) {
                    c = named("shirt_blue");
                }
                c
            }
            "face_fill" => {
                let mut c = named(if y >= 2 { "skin_light" } else { "hair_dark" });
                if y >= 14 {
                    c = named("shirt_blue");
                }
                c
            }
            "face_fill_dark" => named(if y >= 2 { "skin_dark" } else { "hair_dark" }),
            "red_wall" => named("wall_red"),
            "blue_wall" => named("wall_blue"),
            "green_screen" => named("green_screen"),
            "orange_wall" => named("orange_wall"),
            "checker" => checker_cell((y / 3) * 6 + x / 3),
            other => panic!("no scene {other}"),
        };
        cells.push(c);
    }
    if matches!(
        name,
        "office" | "office_dark_skin" | "warm_room" | "grey_room" | "red_wall" | "blue_wall" | "green_screen"
            | "orange_wall"
    ) {
        let skin = if name == "office_dark_skin" { "skin_dark" } else { "skin_light" };
        for i in (0..CELLS).filter(|i| face[*i]) {
            cells[i] = named(if i / GRID <= 4 { "hair_dark" } else { skin });
        }
    }
    cells
}

/// The two cells that hold a clipped highlight (a lamp or a specular spot).
pub const HIGHLIGHTS: [usize; 2] = [2 * GRID + 3, 3 * GRID + 12];

/// In-cell texture: 16 sub-samples with `1 + 0.3 ((7 s mod 5) - 2) / 2`, normalised to mean 1.
fn texture() -> [f64; 16] {
    let mut tex = [0.0; 16];
    for (s, t) in tex.iter_mut().enumerate() {
        *t = 1.0 + 0.3 * (((s * 7) % 5) as f64 - 2.0) / 2.0;
    }
    let mean = tex.iter().sum::<f64>() / 16.0;
    tex.map(|t| t / mean)
}

/// The exposure (linear gain) that puts the mean coded luma of `raw` at `target`, by bisection of its log2.
fn exposure_for(raw: &[[f64; 3]], target: f64) -> f64 {
    let (mut lo, mut hi) = (-10.0f64, 10.0f64);
    for _ in 0..40 {
        let mid = 0.5 * (lo + hi);
        let y = raw.iter().map(|c| srgb_encode(luma(*c) * 2f64.powf(mid))).sum::<f64>() / raw.len() as f64;
        if y < target {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    2f64.powf(0.5 * (lo + hi))
}

/// The cell means of the camera picture for the linear colours `raw` (before exposure).
fn cell_means(raw: &[[f64; 3]], exposure: f64) -> Vec<[f32; 4]> {
    let tex = texture();
    raw.iter()
        .map(|c| {
            let mut sum = [0.0f64; 3];
            for t in tex {
                for k in 0..3 {
                    let v = (c[k] * exposure * t).clamp(0.0, 1.0);
                    // The 8-bit coding of the camera stream, decoded again by the GPU pass.
                    sum[k] += srgb_decode((srgb_encode(v) * 255.0).round_ties_even() / 255.0);
                }
            }
            [(sum[0] / 16.0) as f32, (sum[1] / 16.0) as f32, (sum[2] / 16.0) as f32, 1.0]
        })
        .collect()
}

/// What `stats_downsample` delivers for `cells` lit by `light` (linear RGB, e.g. a blackbody) on a camera whose white
/// balance is `k_set` kelvin: exposure to a mean coded luma of 0.45, two clipped highlight cells.
pub fn camera(cells: &[Cell], light: [f64; 3], k_set: f64) -> Stats {
    let cast = bb_ratio(light, k_set);
    let raw: Vec<[f64; 3]> =
        cells.iter().map(|c| [c.rho[0] * cast[0], c.rho[1] * cast[1], c.rho[2] * cast[2]]).collect();
    let exposure = exposure_for(&raw, 0.45);
    let mut out = cell_means(&raw, exposure);
    for i in HIGHLIGHTS {
        let v = cast.map(|c| (c * exposure * 3.0).clamp(0.0, 1.0));
        out[i] = [v[0] as f32, v[1] as f32, v[2] as f32, 1.0];
    }
    Stats { cells: out }
}

/// The same picture as [`camera`] as 8-bit sRGB RGBA pixels of `w x h` (both multiples of 16): cell `(x * 16 / w, y *
/// 16 / h)` fills the pixel, with the 16-sub-sample texture as a 4 x 4 pattern over the pixels (a cell whose sides are
/// multiples of 4 pixels has the texture's mean exactly) and the two highlight cells clipped. For the GPU tests, which
/// feed it to the real statistics chain.
pub fn picture(cells: &[Cell], light: [f64; 3], k_set: f64, (w, h): (u32, u32)) -> Vec<[u8; 4]> {
    let cast = bb_ratio(light, k_set);
    let raw: Vec<[f64; 3]> =
        cells.iter().map(|c| [c.rho[0] * cast[0], c.rho[1] * cast[1], c.rho[2] * cast[2]]).collect();
    let exposure = exposure_for(&raw, 0.45);
    let tex = texture();
    let code = |v: f64| (srgb_encode(v.clamp(0.0, 1.0)) * 255.0).round_ties_even() as u8;
    let mut out = Vec::with_capacity((w * h) as usize);
    for y in 0..h {
        for x in 0..w {
            let i = (y as usize * GRID / h as usize) * GRID + x as usize * GRID / w as usize;
            let rgb = if HIGHLIGHTS.contains(&i) {
                cast.map(|c| c * exposure * 3.0)
            } else {
                let t = tex[(x % 4 + 4 * (y % 4)) as usize];
                raw[i].map(|c| c * exposure * t)
            };
            out.push([code(rgb[0]), code(rgb[1]), code(rgb[2]), 255]);
        }
    }
    out
}

/// [`camera`] for a blackbody light of `t` kelvin.
pub fn shoot(name: &str, t: f64, k_set: f64) -> Stats {
    camera(&scene(name), blackbody_rgb(t), k_set)
}

/// Left of column `split` lit by `t1`, right by `t2`, a soft blend over 4 columns; no highlight cells.
pub fn mixed(name: &str, t1: f64, t2: f64, k_set: f64, split: usize) -> Stats {
    let cells = scene(name);
    let (l1, l2) = (blackbody_rgb(t1), blackbody_rgb(t2));
    let kk = blackbody_rgb(k_set);
    let raw: Vec<[f64; 3]> = cells
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let a = (((i % GRID) as f64 - split as f64 + 2.0) / 4.0).clamp(0.0, 1.0);
            [0, 1, 2].map(|k| c.rho[k] * ((1.0 - a) * l1[k] + a * l2[k]) / kk[k])
        })
        .collect();
    let exposure = exposure_for(&raw, 0.45);
    Stats { cells: cell_means(&raw, exposure) }
}

/// `light / bb(k_set)` per channel.
fn bb_ratio(light: [f64; 3], k_set: f64) -> [f64; 3] {
    let kk = blackbody_rgb(k_set);
    [light[0] / kk[0], light[1] / kk[1], light[2] / kk[2]]
}

/// What a neutral surface looks like through the light `t` on a camera at `k_set` after the `gains`.
pub fn neutral_out(gains: [f64; 3], t: f64, k_set: f64) -> [f64; 3] {
    let c = bb_ratio(blackbody_rgb(t), k_set);
    [gains[0] * c[0], gains[1] * c[1], gains[2] * c[2]]
}

/// Angle between two colours, in degrees.
pub fn angle(a: [f64; 3], b: [f64; 3]) -> f64 {
    let dot: f64 = (0..3).map(|k| a[k] * b[k]).sum();
    let na = a.iter().map(|v| v * v).sum::<f64>().sqrt();
    let nb = b.iter().map(|v| v * v).sum::<f64>().sqrt();
    (dot / na / nb).clamp(-1.0, 1.0).acos().to_degrees()
}

fn ln_br(c: [f64; 3]) -> f64 {
    c[2].max(1e-6).ln() - c[0].max(1e-6).ln()
}

/// The residual of `out` against `aim` in mired-equivalents (`ln(B/R)` divided by 0.0105): + is too orange.
pub fn residual(out: [f64; 3], aim: [f64; 3]) -> f64 {
    -(ln_br(out) - ln_br(aim)) / 0.0105
}

/// A seeded xorshift stream with normal variates (Box-Muller), like `auto_controls/sim.rs`'s jitter.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed.max(1))
    }

    fn uniform(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        ((self.0 >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    }

    fn normal(&mut self) -> f64 {
        let (u, v) = (self.uniform(), self.uniform());
        (-2.0 * u.ln()).sqrt() * (std::f64::consts::TAU * v).cos()
    }
}

/// `stats` with the noise of the optional mode: `1 %` of the value plus `0.002` per cell and channel.
pub fn noisy(stats: &Stats, rng: &mut Rng) -> Stats {
    let cells = stats
        .cells
        .iter()
        .map(|c| {
            let mut o = *c;
            for v in o.iter_mut().take(3) {
                let x = f64::from(*v) * (1.0 + 0.01 * rng.normal()) + 0.002 * rng.normal();
                *v = x.clamp(0.0, 1.0) as f32;
            }
            o
        })
        .collect();
    Stats { cells }
}
