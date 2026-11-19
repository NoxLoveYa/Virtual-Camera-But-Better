//! Tests of Apply LUT on WARP. The CPU reference is OBS's lookup written as plain steps: the picture's code values
//! are the lookup position, the table is read in linear light (OBS converts the table from sRGB when it uploads it),
//! interpolated trilinearly, blended with the picture in linear light, and encoded again.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::*;
use crate::fx::assets::testgen::{identity_cube_1d, identity_cube_3d, write_png, TempDir};
use crate::fx::assets::{
    parse_cube, png_lut_to_cube, AssetKind, AssetState, Assets, DecodedImage, PNG_LUT_SIZE,
};
use crate::fx::{Chain, FilterCfg, FilterError};
use crate::gpu::pipeline_tests::{fill_rgba, read_rgba, warp};

type Pixel = [u8; 4];

// ---------------------------------------------------------------------------------------------------------------
// Helpers

struct Rig {
    r: Arc<Renderer>,
    _assets: Assets,
    log: Arc<Mutex<Vec<FilterError>>>,
    chain: Chain,
}

impl Rig {
    fn new() -> Rig {
        let r = warp();
        let log: Arc<Mutex<Vec<FilterError>>> = Arc::default();
        let sink = log.clone();
        let assets = Assets::with_scan_interval(
            r.gpu.clone(),
            Arc::new(move |e| sink.lock().unwrap().push(e)),
            Duration::from_millis(50),
        );
        let mut chain = Chain::new();
        chain.set_assets(assets.client());
        Rig {
            r,
            _assets: assets,
            log,
            chain,
        }
    }

    fn set(&mut self, params: Value) {
        self.chain.apply(
            &self.r,
            &[FilterCfg {
                id: "l".into(),
                kind: KIND.into(),
                enabled: true,
                params: params.as_object().cloned().unwrap_or_default(),
            }],
        );
    }

    /// What the card of the filter has been told (the probe's own events are left out).
    fn events(&self) -> Vec<FilterError> {
        self.log.lock().unwrap().iter().filter(|e| e.id == "l").cloned().collect()
    }

    fn wait_events(&self, n: usize) {
        let end = Instant::now() + Duration::from_secs(20);
        while self.events().len() < n {
            assert!(Instant::now() < end, "timed out waiting for event {n}: {:?}", self.events());
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Ask for `file` (plus the other settings) and wait until the loader has answered. Returns the error the card
    /// was told, or an empty message when the file loaded. (A load that follows a good one sends no event, so the
    /// answer is read from a second handle on the same cache entry.)
    fn load(&mut self, file: &Path, extra: Value) -> FilterError {
        let before = self.events().len();
        let mut params = extra.as_object().cloned().unwrap_or_default();
        let path = file.to_string_lossy().into_owned();
        params.insert("image_path".into(), json!(path));
        self.set(Value::Object(params));
        let mut probe = self._assets.client().handle("probe");
        probe.request(AssetKind::ColorLut, &path);
        let end = Instant::now() + Duration::from_secs(20);
        loop {
            match probe.state() {
                AssetState::Ready => {
                    return FilterError {
                        id: "l".into(),
                        message: String::new(),
                    }
                }
                AssetState::Failed(_) => {
                    self.wait_events(before + 1);
                    return self.events()[before].clone();
                }
                _ => {
                    assert!(Instant::now() < end, "timed out loading {path}");
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
    }

    fn run(&mut self, w: u32, h: u32, px: &[Pixel]) -> Vec<Pixel> {
        let tex = self.r.texture(w, h).unwrap();
        fill_rgba(&self.r, &tex, |x, y| px[(y * w + x) as usize]);
        let out = self.chain.run(&self.r, tex).unwrap();
        assert_eq!((out.width, out.height), (w, h));
        read_rgba(&self.r, &out).chunks(4).map(|p| [p[0], p[1], p[2], p[3]]).collect()
    }
}

fn dec(code: f64) -> f64 {
    let u = code / 255.0;
    if u <= 0.04045 {
        u / 12.92
    } else {
        ((u + 0.055) / 1.055).powf(2.4)
    }
}

/// Linear light to a code value in 0..=1.
fn enc01(lin: f64) -> f64 {
    let c = lin.clamp(0.0, 1.0);
    if c <= 0.0031308 {
        12.92 * c
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t
}

/// A table entry in linear light.
fn entry(lut: &CubeLut, i: usize, c: usize) -> f64 {
    dec(lut.data[i][c] as f64 * 255.0)
}

/// Position and fraction of a code value on one axis of the table, clamped to its ends.
fn axis(lut: &CubeLut, c: usize, code: f64) -> (usize, f64) {
    let n = lut.size as usize;
    let range = lut.domain_max[c] as f64 - lut.domain_min[c] as f64;
    let u = (code - lut.domain_min[c] as f64) / range;
    let p = (u * (n as f64 - 1.0)).clamp(0.0, n as f64 - 1.0);
    let i0 = (p.floor() as usize).min(n - 1);
    (i0, p - i0 as f64)
}

/// Trilinear lookup of a 3D table: the colour in linear light.
fn lookup3d(lut: &CubeLut, code: [f64; 3]) -> [f64; 3] {
    let n = lut.size as usize;
    let at = [axis(lut, 0, code[0]), axis(lut, 1, code[1]), axis(lut, 2, code[2])];
    let mut out = [0.0; 3];
    for dz in 0..2 {
        for dy in 0..2 {
            for dx in 0..2 {
                let w = |d: usize, a: (usize, f64)| if d == 1 { a.1 } else { 1.0 - a.1 };
                let weight = w(dx, at[0]) * w(dy, at[1]) * w(dz, at[2]);
                let x = (at[0].0 + dx).min(n - 1);
                let y = (at[1].0 + dy).min(n - 1);
                let z = (at[2].0 + dz).min(n - 1);
                for (c, o) in out.iter_mut().enumerate() {
                    *o += weight * entry(lut, x + y * n + z * n * n, c);
                }
            }
        }
    }
    out
}

/// Linear lookup of one channel of a 1D table, in linear light.
fn lookup1d(lut: &CubeLut, c: usize, code: f64) -> f64 {
    let n = lut.size as usize;
    let (i0, f) = axis(lut, c, code);
    lerp(entry(lut, i0, c), entry(lut, (i0 + 1).min(n - 1), c), f)
}

/// OBS's result for one pixel, in code values (not rounded).
fn reference(lut: &CubeLut, amount: f64, passthrough: bool, px: Pixel) -> [f64; 4] {
    let code = [px[0], px[1], px[2]].map(|v| v as f64 / 255.0);
    let a = px[3] as f64 / 255.0;
    let lit = [px[0], px[1], px[2]].map(|v| dec(v as f64));
    // the domain is compared in single precision, as the GPU does: code 51 of 255 is exactly the f32 0.2
    let code32 = [px[0], px[1], px[2]].map(|v| v as f32 / 255.0);
    let out: [f64; 3] = if lut.dim == LutDim::OneD {
        let mut o = lit;
        for c in 0..3 {
            if code32[c] >= lut.domain_min[c] && code32[c] <= lut.domain_max[c] {
                o[c] = lerp(lit[c], lookup1d(lut, c, code[c]), amount);
            }
        }
        o
    } else {
        let narrower = (0..3).any(|c| lut.domain_min[c] > 0.0 || lut.domain_max[c] < 1.0);
        let blend = |l: [f64; 3]| [0, 1, 2].map(|c| lerp(lit[c], l[c], amount));
        if narrower {
            let inside = (0..3).all(|c| code32[c] >= lut.domain_min[c] && code32[c] <= lut.domain_max[c]);
            if inside {
                blend(lookup3d(lut, code))
            } else {
                lit
            }
        } else if amount < 1.0 {
            blend(lookup3d(lut, code))
        } else if !passthrough {
            lookup3d(lut, code)
        } else {
            // looks up the premultiplied colour and hands the result back as premultiplied
            let premultiplied = lit.map(|v| enc01(v * a));
            let l = lookup3d(lut, premultiplied);
            if a > 0.0 {
                l.map(|v| v / a)
            } else {
                [0.0; 3]
            }
        }
    };
    [
        enc01(out[0]) * 255.0,
        enc01(out[1]) * 255.0,
        enc01(out[2]) * 255.0,
        px[3] as f64,
    ]
}

fn worst(got: &[Pixel], want: &[[f64; 4]]) -> f64 {
    assert_eq!(got.len(), want.len());
    got.iter()
        .zip(want)
        .flat_map(|(g, w)| g.iter().zip(w).map(|(g, w)| (*g as f64 - w).abs()))
        .fold(0.0, f64::max)
}

fn worst_vs_input(got: &[Pixel], input: &[Pixel]) -> i32 {
    got.iter()
        .zip(input)
        .flat_map(|(a, b)| a.iter().zip(b).map(|(a, b)| (*a as i32 - *b as i32).abs()))
        .max()
        .unwrap()
}

/// `n` random opaque colours (alpha from `alpha`), the same every run.
fn colors(n: usize, alpha: impl Fn(usize) -> u8) -> Vec<Pixel> {
    let mut seed = 0x00ab_cdefu32;
    let mut next = move || {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (seed >> 24) as u8
    };
    (0..n).map(|i| [next(), next(), next(), alpha(i)]).collect()
}

/// A 256 x 4 picture holding every code value of every channel.
fn gradient() -> (u32, u32, Vec<Pixel>) {
    let mut px = Vec::new();
    for y in 0..4u32 {
        for x in 0..256u32 {
            px.push(match y {
                0 => [x as u8, x as u8, x as u8, 255],
                1 => [x as u8, 255 - x as u8, (x * 3 % 256) as u8, 255],
                2 => [(x * 5 % 256) as u8, x as u8, 128, 255],
                _ => [255 - x as u8, (x * 7 % 256) as u8, x as u8, 255],
            });
        }
    }
    (256, 4, px)
}

/// The text of a 3D `.cube` of `size` whose entry for the input `(r, g, b)` in 0..1 is `f(r, g, b)`.
fn cube3d(size: u32, header: &str, f: impl Fn([f32; 3]) -> [f32; 3]) -> String {
    let d = (size - 1) as f32;
    let mut s = format!("{header}LUT_3D_SIZE {size}\n");
    for b in 0..size {
        for g in 0..size {
            for r in 0..size {
                let o = f([r as f32 / d, g as f32 / d, b as f32 / d]);
                s.push_str(&format!("{:.6} {:.6} {:.6}\n", o[0], o[1], o[2]));
            }
        }
    }
    s
}

/// A warm tint with a gamma curve and cross terms (green into red, red into blue), so a mix-up of axes shows.
fn tint([r, g, b]: [f32; 3]) -> [f32; 3] {
    [r * 0.9 + g * 0.2, g.powf(1.2), b * 0.6 + 0.3 * r]
}

fn invert([r, g, b]: [f32; 3]) -> [f32; 3] {
    [1.0 - r, 1.0 - g, 1.0 - b]
}

fn parse(text: &str) -> CubeLut {
    parse_cube(text).expect("a valid table")
}

fn assets_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("assets")
}

fn write(dir: &TempDir, name: &str, text: &str) -> std::path::PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, text).unwrap();
    p
}

// ---------------------------------------------------------------------------------------------------------------
// The lookup constants

#[test]
fn technique_follows_color_grade_filter_update() {
    let id3 = parse(&identity_cube_3d(2));
    let id1 = parse(&identity_cube_1d(4));
    let domain = |min: f32, max: f32| {
        let mut l = id3.clone();
        l.domain_min = [min; 3];
        l.domain_max = [max; 3];
        l
    };
    assert_eq!(technique(&id1, 1.0, false), Technique::Draw1D);
    assert_eq!(technique(&id1, 0.3, true), Technique::Draw1D);
    assert_eq!(technique(&id3, 1.0, false), Technique::DrawAlpha3D);
    assert_eq!(technique(&id3, 1.0, true), Technique::Draw3D);
    // an amount below 1 wins over passthrough alpha
    assert_eq!(technique(&id3, 0.99, true), Technique::DrawAmount3D);
    assert_eq!(technique(&id3, 0.0, false), Technique::DrawAmount3D);
    // a domain wins over everything but 1D
    assert_eq!(technique(&domain(0.1, 1.0), 1.0, true), Technique::DrawDomain3D);
    assert_eq!(technique(&domain(0.0, 0.9), 0.5, false), Technique::DrawDomain3D);
    // a domain that contains 0..1 is no domain
    assert_eq!(technique(&domain(-1.0, 2.0), 1.0, false), Technique::DrawAlpha3D);
    assert_eq!(technique(&domain(-1.0, 1.0), 0.5, false), Technique::DrawAmount3D);
}

#[test]
fn constants_are_the_ones_obs_computes() {
    // a picture table: scale 1 - 1/64, offset 0.5/64
    let png = CubeLut {
        dim: LutDim::ThreeD,
        size: PNG_LUT_SIZE,
        domain_min: [0.0; 3],
        domain_max: [1.0; 3],
        data: Vec::new(),
    };
    let (s, o) = scale_offset(&png);
    for i in 0..3 {
        assert!((s[i] - (1.0 - 1.0 / 64.0)).abs() < 1e-7, "{s:?}");
        assert!((o[i] - 0.5 / 64.0).abs() < 1e-7, "{o:?}");
    }
    // a domain of -0.5 .. 1.5 on a table of 5: coordinate (c - min) / range * 4 + 0.5, over 5
    let lut = CubeLut {
        domain_min: [-0.5; 3],
        domain_max: [1.5, 1.0, 3.5],
        size: 5,
        ..png
    };
    let (s, o) = scale_offset(&lut);
    for (i, range) in [2.0f32, 1.5, 4.0].into_iter().enumerate() {
        assert!((s[i] - 4.0 / range / 5.0).abs() < 1e-6, "{s:?}");
        assert!((o[i] - (0.5 * 4.0 / range + 0.5) / 5.0).abs() < 1e-6, "{o:?}");
    }
    // the packed block: scale, amount / offset, mode / domain min / domain max
    let bytes = constants(&lut, 0.25, false);
    let f = |i: usize| f32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap());
    assert_eq!([f(0), f(1), f(2)], s);
    assert_eq!(f(3), 0.25);
    assert_eq!([f(4), f(5), f(6)], o);
    assert_eq!(u32::from_le_bytes(bytes[28..32].try_into().unwrap()), 2);
    assert_eq!([f(8), f(9), f(10)], [-0.5; 3]);
    assert_eq!([f(12), f(13), f(14)], [1.5, 1.0, 3.5]);
}

// ---------------------------------------------------------------------------------------------------------------
// Picture tables

fn picture_of(w: u32, h: u32, f: impl Fn(u32, u32) -> [u8; 4]) -> DecodedImage {
    let mut rgba = Vec::new();
    for y in 0..h {
        for x in 0..w {
            rgba.extend_from_slice(&f(x, y));
        }
    }
    DecodedImage {
        width: w,
        height: h,
        rgba,
    }
}

/// The identity table as a 512 x 512 picture: tile `z` at column `z % 8`, row `z / 8`.
fn identity_png_pixels(x: u32, y: u32) -> [u8; 4] {
    let (tile_x, tile_y) = (x / 64, y / 64);
    let z = tile_y * 8 + tile_x;
    let code = |v: u32| (v as f64 * 255.0 / 63.0).round() as u8;
    [code(x % 64), code(y % 64), code(z), 255]
}

#[test]
fn a_512_picture_is_cut_into_tiles_like_obs() {
    // mark one entry: red 3, green 5, blue 9 sits in tile (1, 1) at (3, 5): pixel (64 + 3, 64 + 5)
    let img = picture_of(512, 512, |x, y| if (x, y) == (67, 69) { [10, 20, 30, 40] } else { [0, 0, 0, 255] });
    let lut = png_lut_to_cube(&img).unwrap();
    assert_eq!((lut.dim, lut.size), (LutDim::ThreeD, 64));
    assert_eq!((lut.domain_min, lut.domain_max), ([0.0; 3], [1.0; 3]));
    assert_eq!(lut.data.len(), 64 * 64 * 64);
    let i = 3 + 5 * 64 + 9 * 64 * 64;
    assert_eq!(lut.data[i], [10.0 / 255.0, 20.0 / 255.0, 30.0 / 255.0], "alpha is ignored");
    assert_eq!(lut.data.iter().filter(|v| **v != [0.0; 3]).count(), 1);
    // the identity picture is the identity table
    let id = png_lut_to_cube(&picture_of(512, 512, identity_png_pixels)).unwrap();
    let d = 63.0f32;
    for (z, y, x) in [(0u32, 0u32, 0u32), (63, 63, 63), (9, 5, 3), (40, 1, 62)] {
        let e = id.data[(x + y * 64 + z * 4096) as usize];
        for (got, want) in e.iter().zip([x, y, z]) {
            assert!((got - want as f32 / d).abs() < 0.003, "{x},{y},{z}: {e:?}");
        }
    }
}

#[test]
fn pictures_that_are_not_tables_are_refused() {
    let flat = |w, h| picture_of(w, h, |_, _| [1, 2, 3, 255]);
    for (w, h) in [(100u32, 100u32), (64, 64), (1024, 32), (512, 513), (4096, 4096), (63, 4161), (1, 1)] {
        let e = png_lut_to_cube(&flat(w, h)).unwrap_err();
        assert!(e.contains(&format!("{w}x{h}")), "{e}");
    }
    // OBS's size test passes for these, but its tile placement leaves the picture: refused, not read outside
    for (w, h) in [(1024u32, 256u32), (2048, 128), (4096, 64)] {
        let e = png_lut_to_cube(&flat(w, h)).unwrap_err();
        assert!(e.contains(&format!("{w}x{h}")), "{e}");
    }
    // these stay inside the picture (OBS repeats tiles); the same formula gives the same table
    for (w, h) in [(128u32, 2048u32), (256, 1024), (64, 4096)] {
        let lut = png_lut_to_cube(&flat(w, h)).unwrap();
        assert_eq!(lut.data.len(), 64 * 64 * 64, "{w}x{h}");
    }
    let mut short = flat(512, 512);
    short.rgba.truncate(100);
    assert!(png_lut_to_cube(&short).is_err());
}

// ---------------------------------------------------------------------------------------------------------------
// Identity tables

#[test]
fn identity_tables_of_17_and_33_leave_a_gradient_alone() {
    let (w, h, px) = gradient();
    let dir = TempDir::new("lut-id");
    let mut rig = Rig::new();
    for size in [17u32, 33] {
        let f = write(&dir, &format!("id{size}.cube"), &identity_cube_3d(size));
        assert_eq!(rig.load(&f, json!({})).message, "");
        let out = rig.run(w, h, &px);
        let diff = worst_vs_input(&out, &px);
        assert!(diff <= 2, "identity {size}: differs by {diff}");
        // and it is what the reference says, not just close to the input
        let lut = parse(&identity_cube_3d(size));
        let want: Vec<[f64; 4]> = px.iter().map(|p| reference(&lut, 1.0, false, *p)).collect();
        assert!(worst(&out, &want) <= 1.0, "identity {size} against the reference");
    }
    // the committed 9 point table too (coarser: the interpolation error of a convex curve is larger)
    let f = assets_dir().join("identity_3d_9.cube");
    assert_eq!(rig.load(&f, json!({})).message, "");
    let out = rig.run(w, h, &px);
    let lut = parse(&std::fs::read_to_string(&f).unwrap());
    let want: Vec<[f64; 4]> = px.iter().map(|p| reference(&lut, 1.0, false, *p)).collect();
    assert!(worst(&out, &want) <= 1.0, "identity 9 against the reference");
}

#[test]
fn an_identity_picture_table_of_512_leaves_a_gradient_alone() {
    let (w, h, px) = gradient();
    let dir = TempDir::new("lut-png");
    let f = dir.join("identity.png");
    let img = picture_of(512, 512, identity_png_pixels);
    write_png(&f, 512, 512, &img.rgba);
    let mut rig = Rig::new();
    assert_eq!(rig.load(&f, json!({})).message, "");
    let out = rig.run(w, h, &px);
    let diff = worst_vs_input(&out, &px);
    assert!(diff <= 2, "identity picture differs by {diff}");
    // the same picture against the reference of the table it holds
    let lut = png_lut_to_cube(&img).unwrap();
    let want: Vec<[f64; 4]> = px.iter().map(|p| reference(&lut, 1.0, false, *p)).collect();
    assert!(worst(&out, &want) <= 1.0);
}

#[test]
fn a_picture_table_that_changes_colours_matches_the_reference() {
    let dir = TempDir::new("lut-png2");
    let f = dir.join("warm.png");
    // red kept, green and blue pulled down by an amount that depends on red (a cross term), blue lifted by green
    let img = picture_of(512, 512, |x, y| {
        let [r, g, b, a] = identity_png_pixels(x, y);
        let (rf, gf, bf) = (r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0);
        let t = tint([rf, gf, bf]);
        [(t[0].min(1.0) * 255.0) as u8, (t[1] * 255.0) as u8, (t[2].min(1.0) * 255.0) as u8, a]
    });
    write_png(&f, 512, 512, &img.rgba);
    let lut = png_lut_to_cube(&img).unwrap();
    let px = colors(400, |_| 255);
    let mut rig = Rig::new();
    assert_eq!(rig.load(&f, json!({})).message, "");
    let out = rig.run(20, 20, &px);
    let want: Vec<[f64; 4]> = px.iter().map(|p| reference(&lut, 1.0, false, *p)).collect();
    let diff = worst(&out, &want);
    assert!(diff <= 2.0, "picture table: worst difference {diff}");
    assert!(worst_vs_input(&out, &px) > 20, "the table changes the picture");
}

// ---------------------------------------------------------------------------------------------------------------
// Non-identity tables

#[test]
fn swap_red_and_blue_swaps_red_and_blue() {
    let f = assets_dir().join("swap_rb_2.cube");
    let mut rig = Rig::new();
    assert_eq!(rig.load(&f, json!({})).message, "");
    // on the corners of the table the result is exact
    let corners: Vec<Pixel> = vec![
        [255, 0, 0, 255],
        [0, 0, 255, 255],
        [255, 255, 0, 255],
        [0, 255, 255, 255],
        [255, 0, 255, 255],
        [0, 255, 0, 255],
        [0, 0, 0, 255],
        [255, 255, 255, 255],
    ];
    let out = rig.run(8, 1, &corners);
    let swapped: Vec<Pixel> = corners.iter().map(|p| [p[2], p[1], p[0], p[3]]).collect();
    assert_eq!(out, swapped);
    // in between, OBS interpolates the two-point table in linear light: the reference says by how much
    let lut = parse(&std::fs::read_to_string(&f).unwrap());
    let px = colors(64, |_| 255);
    let out = rig.run(8, 8, &px);
    let want: Vec<[f64; 4]> = px.iter().map(|p| reference(&lut, 1.0, false, *p)).collect();
    assert!(worst(&out, &want) <= 2.0, "worst {}", worst(&out, &want));
}

#[test]
fn a_tint_table_matches_the_reference_between_grid_points() {
    let dir = TempDir::new("lut-tint");
    let text = cube3d(5, "TITLE \"warm\"\n", tint);
    let f = write(&dir, "tint.cube", &text);
    let lut = parse(&text);
    let mut rig = Rig::new();
    assert_eq!(rig.load(&f, json!({})).message, "");
    // random colours, plus colours that sit on grid points (code 0, 64, 128, 191, 255 are on or near them)
    let mut px = colors(300, |_| 255);
    for r in [0u8, 64, 128, 191, 255] {
        for g in [0u8, 128, 255] {
            px.push([r, g, 255 - r, 255]);
        }
    }
    px.truncate(315);
    let out = rig.run(21, 15, &px);
    let want: Vec<[f64; 4]> = px.iter().map(|p| reference(&lut, 1.0, false, *p)).collect();
    let diff = worst(&out, &want);
    assert!(diff <= 2.0, "tint table: worst difference {diff}");
    assert!(worst_vs_input(&out, &px) > 20, "the table changes the picture");
}

#[test]
fn a_custom_domain_is_honoured() {
    let dir = TempDir::new("lut-domain");
    let mut rig = Rig::new();
    let px = colors(400, |_| 255);

    // a narrow domain: colours outside it are untouched, inside they are looked up
    let narrow = cube3d(5, "DOMAIN_MIN 0.2 0.2 0.2\nDOMAIN_MAX 0.8 0.8 0.8\n", invert);
    let f = write(&dir, "narrow.cube", &narrow);
    let lut = parse(&narrow);
    assert_eq!(rig.load(&f, json!({})).message, "");
    let out = rig.run(20, 20, &px);
    let want: Vec<[f64; 4]> = px.iter().map(|p| reference(&lut, 1.0, false, *p)).collect();
    assert!(worst(&out, &want) <= 2.0, "narrow domain: worst {}", worst(&out, &want));
    let inside = px
        .iter()
        .zip(&out)
        .filter(|(p, _)| p[..3].iter().all(|v| (52..=204).contains(v)))
        .count();
    let unchanged = px.iter().zip(&out).filter(|(p, o)| p == o).count();
    assert!(inside > 5 && unchanged > 100, "inside {inside}, unchanged {unchanged}");
    // a colour with one channel outside is not looked up at all
    let one = rig.run(2, 1, &[[128, 128, 250, 255], [128, 128, 128, 255]]);
    assert_eq!(one[0], [128, 128, 250, 255]);
    assert_ne!(one[1], [128, 128, 128, 255]);

    // a domain wider than 0..1: the lookup position is rescaled, nothing is skipped
    let wide = cube3d(6, "DOMAIN_MIN -0.5 -1 0\nDOMAIN_MAX 1.5 2 1.25\n", tint);
    let f = write(&dir, "wide.cube", &wide);
    let lut = parse(&wide);
    assert_eq!(rig.load(&f, json!({})).message, "");
    let out = rig.run(20, 20, &px);
    let want: Vec<[f64; 4]> = px.iter().map(|p| reference(&lut, 1.0, false, *p)).collect();
    assert!(worst(&out, &want) <= 2.0, "wide domain: worst {}", worst(&out, &want));

    // a domain that only starts above 0 with the full amount blend
    let high = cube3d(5, "DOMAIN_MIN 0.5 0 0\nDOMAIN_MAX 1 1 1\n", invert);
    let f = write(&dir, "high.cube", &high);
    let lut = parse(&high);
    assert_eq!(rig.load(&f, json!({ "clut_amount": 0.4 })).message, "");
    let out = rig.run(20, 20, &px);
    let want: Vec<[f64; 4]> = px.iter().map(|p| reference(&lut, 0.4, false, *p)).collect();
    assert!(worst(&out, &want) <= 2.0, "domain with amount: worst {}", worst(&out, &want));
}

// ---------------------------------------------------------------------------------------------------------------
// 1D tables

#[test]
fn a_1d_table_works() {
    let (w, h, px) = gradient();
    let mut rig = Rig::new();
    // the committed identity: a gradient comes back
    let f = assets_dir().join("identity_1d_16.cube");
    assert_eq!(rig.load(&f, json!({})).message, "");
    let out = rig.run(w, h, &px);
    assert!(worst_vs_input(&out, &px) <= 2, "identity 1D differs by {}", worst_vs_input(&out, &px));

    // a curve per channel, red inverted but only up to the domain's end at 0.5, green squared, blue constant
    let dir = TempDir::new("lut-1d");
    let mut text = String::from("TITLE \"curves\"\nDOMAIN_MIN 0 0 0\nDOMAIN_MAX 0.5 1 1\nLUT_1D_SIZE 7\n");
    for i in 0..7 {
        let t = i as f32 / 6.0;
        text.push_str(&format!("{:.6} {:.6} {:.6}\n", 1.0 - t, t * t, 0.25));
    }
    let f = write(&dir, "curves.cube", &text);
    let lut = parse(&text);
    assert_eq!(lut.dim, LutDim::OneD);
    assert_eq!(rig.load(&f, json!({})).message, "");
    let px = colors(400, |_| 255);
    let out = rig.run(20, 20, &px);
    let want: Vec<[f64; 4]> = px.iter().map(|p| reference(&lut, 1.0, false, *p)).collect();
    let diff = worst(&out, &want);
    assert!(diff <= 2.0, "1D curves: worst difference {diff}");
    // red above the domain's end is left alone
    let one = rig.run(2, 1, &[[200, 100, 100, 255], [20, 100, 100, 255]]);
    assert_eq!(one[0][0], 200);
    assert_ne!(one[1][0], 20);

    // the amount blends the curves in linear light; passthrough_alpha has no effect on a 1D table
    rig.set(json!({ "image_path": f.to_string_lossy(), "clut_amount": 0.5 }));
    let half = rig.run(20, 20, &px);
    let want: Vec<[f64; 4]> = px.iter().map(|p| reference(&lut, 0.5, false, *p)).collect();
    assert!(worst(&half, &want) <= 2.0);
    rig.set(json!({ "image_path": f.to_string_lossy(), "clut_amount": 0.5, "passthrough_alpha": true }));
    assert_eq!(rig.run(20, 20, &px), half);
}

// ---------------------------------------------------------------------------------------------------------------
// Amount and alpha

#[test]
fn clut_amount_blends_in_linear_light() {
    let dir = TempDir::new("lut-amount");
    let text = cube3d(5, "", tint);
    let f = write(&dir, "tint.cube", &text);
    let lut = parse(&text);
    let mut rig = Rig::new();
    assert_eq!(rig.load(&f, json!({ "clut_amount": 0 })).message, "");
    let px = colors(400, |_| 255);

    let zero = rig.run(20, 20, &px);
    assert!(worst_vs_input(&zero, &px) <= 1, "amount 0 differs by {}", worst_vs_input(&zero, &px));

    let at = |rig: &mut Rig, a: f64| {
        rig.set(json!({ "image_path": f.to_string_lossy(), "clut_amount": a }));
        rig.run(20, 20, &px)
    };
    let half = at(&mut rig, 0.5);
    let want: Vec<[f64; 4]> = px.iter().map(|p| reference(&lut, 0.5, false, *p)).collect();
    assert!(worst(&half, &want) <= 2.0, "amount 0.5: worst {}", worst(&half, &want));

    let full = at(&mut rig, 1.0);
    let want: Vec<[f64; 4]> = px.iter().map(|p| reference(&lut, 1.0, false, *p)).collect();
    assert!(worst(&full, &want) <= 2.0);
    // the half result sits between the picture and the table's result, in linear light (where the table's result
    // is not clipped: OBS blends the unclipped value)
    for ((p, h), f) in px.iter().zip(&half).zip(&full) {
        if f[..3].iter().any(|v| *v >= 250) {
            continue;
        }
        for c in 0..3 {
            let mid = lerp(dec(p[c] as f64), dec(f[c] as f64), 0.5);
            assert!((enc01(mid) * 255.0 - h[c] as f64).abs() <= 3.0, "{p:?} {h:?} {f:?}");
        }
    }

    // an amount above 1 or below 0 is clamped, a bad type is the default (1)
    assert_eq!(at(&mut rig, 7.0), full);
    assert_eq!(at(&mut rig, -2.0), zero);
    rig.set(json!({ "image_path": f.to_string_lossy(), "clut_amount": "much" }));
    assert_eq!(rig.run(20, 20, &px), full);
}

#[test]
fn alpha_is_untouched_and_passthrough_alpha_follows_obs() {
    let dir = TempDir::new("lut-alpha");
    let text = cube3d(5, "", tint);
    let f = write(&dir, "tint.cube", &text);
    let lut = parse(&text);
    let px = colors(400, |i| [255, 200, 128, 64, 17, 0][i % 6]);
    let mut rig = Rig::new();

    // the default: the colour is looked up as it is, alpha stays
    assert_eq!(rig.load(&f, json!({})).message, "");
    let plain = rig.run(20, 20, &px);
    assert!(plain.iter().zip(&px).all(|(o, p)| o[3] == p[3]), "alpha changed");
    let want: Vec<[f64; 4]> = px.iter().map(|p| reference(&lut, 1.0, false, *p)).collect();
    assert!(worst(&plain, &want) <= 2.0, "worst {}", worst(&plain, &want));
    // the same colour at any alpha gives the same colour
    let opaque: Vec<Pixel> = px.iter().map(|p| [p[0], p[1], p[2], 255]).collect();
    let opaque_out = rig.run(20, 20, &opaque);
    assert!(plain.iter().zip(&opaque_out).all(|(a, b)| a[..3] == b[..3]));

    // passthrough alpha: the premultiplied colour is looked up, so a translucent pixel comes out differently
    rig.set(json!({ "image_path": f.to_string_lossy(), "passthrough_alpha": true }));
    let pass = rig.run(20, 20, &px);
    assert!(pass.iter().zip(&px).all(|(o, p)| o[3] == p[3]), "alpha changed");
    let want: Vec<[f64; 4]> = px.iter().map(|p| reference(&lut, 1.0, true, *p)).collect();
    assert!(worst(&pass, &want) <= 3.0, "passthrough: worst {}", worst(&pass, &want));
    assert_ne!(pass, plain);
    // opaque pixels do not care
    let opaque_pass = rig.run(20, 20, &opaque);
    assert!(worst_vs_input(&opaque_pass, &opaque_out) <= 1);

    // with an amount below 1 OBS ignores passthrough_alpha
    rig.set(json!({ "image_path": f.to_string_lossy(), "passthrough_alpha": true, "clut_amount": 0.6 }));
    let a = rig.run(20, 20, &px);
    rig.set(json!({ "image_path": f.to_string_lossy(), "passthrough_alpha": false, "clut_amount": 0.6 }));
    assert_eq!(rig.run(20, 20, &px), a);
}

// ---------------------------------------------------------------------------------------------------------------
// Files that are not there, not valid, or not set

#[test]
fn an_empty_path_and_a_chain_without_an_asset_service_pass_through() {
    let (w, h, px) = gradient();
    let mut rig = Rig::new();
    rig.set(json!({}));
    assert_eq!(rig.run(w, h, &px), px);
    rig.set(json!({ "image_path": "   ", "clut_amount": 0.5 }));
    assert_eq!(rig.run(w, h, &px), px);
    assert!(rig.events().is_empty(), "an empty path is not an error: {:?}", rig.events());
    assert!(rig.chain.take_errors().is_empty());

    // no service at all (a chain built for a test): the filter gets no handle and still works
    let r = warp();
    let mut chain = Chain::new();
    chain.apply(
        &r,
        &[FilterCfg {
            id: "l".into(),
            kind: KIND.into(),
            enabled: true,
            params: json!({ "image_path": "C:\\nowhere\\x.cube" }).as_object().cloned().unwrap(),
        }],
    );
    let tex = r.texture(w, h).unwrap();
    fill_rgba(&r, &tex, |x, y| px[(y * w + x) as usize]);
    let out = chain.run(&r, tex).unwrap();
    assert_eq!(read_rgba(&r, &out), px.concat());
    assert!(chain.take_errors().is_empty());
}

#[test]
fn bad_files_leave_the_picture_alone_and_report_once() {
    let dir = TempDir::new("lut-bad");
    let (w, h, px) = gradient();
    let mut rig = Rig::new();

    let cases: Vec<(&str, std::path::PathBuf)> = vec![
        ("malformed cube", write(&dir, "broken.cube", "LUT_3D_SIZE 4\n0 0 0\n1 1 1\nnot a number\n")),
        ("cube without entries", write(&dir, "empty.cube", "TITLE \"x\"\nLUT_3D_SIZE 3\n")),
        ("cube with an inverted domain", write(&dir, "domain.cube", &format!("DOMAIN_MIN 1 1 1\nDOMAIN_MAX 0 0 0\n{}", identity_cube_3d(2)))),
        ("missing file", dir.join("gone.cube")),
        ("missing picture", dir.join("gone.png")),
        ("text in a png", write(&dir, "text.png", "this is not a picture")),
        ("a folder", dir.0.to_path_buf()),
    ];
    for (what, path) in &cases {
        let e = rig.load(path, json!({}));
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(e.message.starts_with(&format!("could not load {name}: ")), "{what}: {}", e.message);
        assert_eq!(e.id, "l");
        for _ in 0..3 {
            assert_eq!(rig.run(w, h, &px), px, "{what}: the picture must pass through");
        }
    }
    // a picture of an unsupported size
    for (name, pw, ph) in [("small.png", 100u32, 100u32), ("strip.png", 1024, 32), ("wide.png", 1024, 256)] {
        let f = dir.join(name);
        write_png(&f, pw, ph, &vec![128u8; (pw * ph * 4) as usize]);
        let e = rig.load(&f, json!({}));
        assert!(e.message.starts_with(&format!("could not load {name}: ")), "{}", e.message);
        assert!(e.message.contains(&format!("{pw}x{ph}")), "{}", e.message);
        assert_eq!(rig.run(w, h, &px), px);
    }
    // exactly one report per bad file, none from the chain, none while frames render
    let events = rig.events();
    assert_eq!(events.len(), cases.len() + 3, "{events:?}");
    assert!(events.iter().all(|e| !e.message.is_empty()));
    assert!(rig.chain.take_errors().is_empty());
}

#[test]
fn fixing_the_file_clears_the_error_and_applies_the_table() {
    let dir = TempDir::new("lut-fix");
    let (w, h, px) = gradient();
    let mut rig = Rig::new();
    let f = write(&dir, "look.cube", "LUT_3D_SIZE 2\n0 0 0\n1 0 0\n");
    let e = rig.load(&f, json!({}));
    assert!(e.message.starts_with("could not load look.cube: "), "{}", e.message);
    assert_eq!(rig.run(w, h, &px), px);
    assert_eq!(rig.events().len(), 1);

    // a valid table in the same file: the loader notices the change, the card is told the error is gone
    let text = cube3d(5, "", tint);
    std::fs::write(&f, &text).unwrap();
    rig.wait_events(2);
    let events = rig.events();
    assert_eq!(events.len(), 2, "{events:?}");
    assert_eq!(events[1].message, "");
    let lut = parse(&text);
    let out = rig.run(w, h, &px);
    let want: Vec<[f64; 4]> = px.iter().map(|p| reference(&lut, 1.0, false, *p)).collect();
    assert!(worst(&out, &want) <= 2.0, "worst {}", worst(&out, &want));
    assert_ne!(out, px);

    // the table is damaged again: the card hears it, the picture is passed through, then the file heals again
    std::fs::write(&f, "LUT_3D_SIZE 99\n").unwrap();
    rig.wait_events(3);
    assert!(!rig.events()[2].message.is_empty());
    assert_eq!(rig.run(w, h, &px), px);
    std::fs::write(&f, identity_cube_3d(17)).unwrap();
    rig.wait_events(4);
    assert_eq!(rig.events()[3].message, "");
    assert!(worst_vs_input(&rig.run(w, h, &px), &px) <= 2);

    // clearing the path restores the picture exactly and is not an error
    rig.set(json!({ "image_path": "" }));
    assert_eq!(rig.run(w, h, &px), px);
    assert_eq!(rig.events().len(), 4);
}

#[test]
fn a_table_picked_by_path_in_another_spelling_loads_once() {
    // forward slashes and a different case are the same file (the service's cache key)
    let dir = TempDir::new("lut-spell");
    let f = write(&dir, "Look.cube", &cube3d(3, "", tint));
    let mut rig = Rig::new();
    let spelled = f.to_string_lossy().replace('\\', "/").to_uppercase();
    assert_eq!(rig.load(Path::new(&spelled), json!({})).message, "");
    let (w, h, px) = gradient();
    assert_ne!(rig.run(w, h, &px), px);
}

#[test]
fn a_big_table_loads_and_works() {
    // 65 cubed is the largest the plan names; a quick identity check on the size that stresses the volume upload
    let dir = TempDir::new("lut-65");
    let f = write(&dir, "id65.cube", &identity_cube_3d(65));
    let mut rig = Rig::new();
    assert_eq!(rig.load(&f, json!({})).message, "");
    let (w, h, px) = gradient();
    assert!(worst_vs_input(&rig.run(w, h, &px), &px) <= 2);
}

#[test]
fn warm_up_compiles_both_shaders() {
    let r = warp();
    let mut f = make();
    f.warm_up(&r).expect("both shaders compile");
}
