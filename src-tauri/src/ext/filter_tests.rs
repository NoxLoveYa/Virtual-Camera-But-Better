//! Tests of the extension filter: the whole path from an extension folder to pixels, on WARP.
//!
//! Each test makes its own folder under the temporary directory and its own registry, so tests run in parallel without
//! sharing anything but the loader thread, the loader's cache (keyed by source) and the process-wide count of stuck
//! workers.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;

use super::*;
use crate::ext::loader::{self, BuildSpec};
use crate::ext::wgsl::ParamDecl;
use crate::ext::{hlsl, layout, manifest, params, stuck_workers, wgsl, ErrorKind, ExtRegistry, ExtState, KIND_PREFIX};
use crate::fx::assets::testgen::{picture as block_picture, write_png};
use crate::fx::assets::{Assets, AssetState};
use crate::fx::{Chain, FilterCfg, FilterError};
use crate::gpu::pipeline_tests::{fill_rgba, read_rgba, warp};
use crate::gpu::Pooled;

type Pixel = [u8; 4];

static SEQ: AtomicUsize = AtomicUsize::new(0);

// ---------------------------------------------------------------------------------------------------------------
// Helpers

/// A folder with an `extensions` directory and a registry on it. Deleted on drop.
struct Fixture {
    _base: vcam_protocol::testdir::TestDir,
    root: PathBuf,
    reg: ExtRegistry,
}

impl Fixture {
    fn new() -> Fixture {
        let base = vcam_protocol::testdir::TestDir::new("better-cam-ext-filter");
        let root = base.join("extensions");
        std::fs::create_dir_all(&root).unwrap();
        let reg = ExtRegistry::new();
        reg.configure(root.clone(), base.join("state.json"), None);
        Fixture { _base: base, root, reg }
    }

    fn folder(&self, id: &str) -> PathBuf {
        self.root.join(id)
    }

    /// Write one file of an extension (the folder is made), without scanning.
    fn write(&self, id: &str, file: &str, text: &str) {
        std::fs::create_dir_all(self.folder(id)).unwrap();
        std::fs::write(self.folder(id).join(file), text).unwrap();
    }

    /// A filter extension: `extension.json` and its shader file, then a scan.
    fn add(&self, id: &str, shader_file: &str, shader: &str, params: &str, extra: &str) {
        self.write(id, "extension.json", &manifest(id, shader_file, params, extra));
        self.write(id, shader_file, shader);
        self.reg.reload();
    }

    /// Copy `sdk/examples/<id>` into the folder, then scan.
    fn install_example(&self, id: &str) {
        let from = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("sdk").join("examples").join(id);
        std::fs::create_dir_all(self.folder(id)).unwrap();
        for entry in std::fs::read_dir(&from).unwrap().flatten() {
            if entry.path().is_file() {
                std::fs::copy(entry.path(), self.folder(id).join(entry.file_name())).unwrap();
            }
        }
        self.reg.reload();
    }

    fn state(&self, id: &str) -> Option<ExtState> {
        self.reg.snapshot().info(id).map(|i| i.state)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // the registry stops first; the folder goes with the field
        self.reg.stop();
    }
}

fn manifest(id: &str, shader_file: &str, params: &str, extra: &str) -> String {
    format!(
        r#"{{ "apiVersion": 1, "id": "{id}", "name": "{id}", "version": "1.0.0", "kind": "filter", "shader": "{shader_file}", "params": [{params}]{extra} }}"#
    )
}

fn cfg(v: serde_json::Value) -> Params {
    v.as_object().cloned().unwrap_or_default()
}

/// Colour int of the OBS format `0xAABBGGRR`, opaque.
fn obs(r: u8, g: u8, b: u8) -> u32 {
    0xFF00_0000 | (b as u32) << 16 | (g as u32) << 8 | r as u32
}

/// A picture that is not symmetric in x or y and has varying alpha.
fn pattern(w: u32, h: u32) -> Vec<Pixel> {
    let mut px = Vec::new();
    for y in 0..h {
        for x in 0..w {
            px.push([
                (x * 255 / (w - 1).max(1)) as u8,
                (y * 255 / (h - 1).max(1)) as u8,
                ((x * 37 + y * 91 + 13) % 256) as u8,
                255 - ((x + 2 * y) % 4 * 30) as u8,
            ]);
        }
    }
    px
}

fn flat(w: u32, h: u32, p: Pixel) -> Vec<Pixel> {
    vec![p; (w * h) as usize]
}

fn worst(got: &[Pixel], want: &[Pixel]) -> u8 {
    assert_eq!(got.len(), want.len());
    got.iter().zip(want).flat_map(|(g, w)| (0..4).map(move |c| g[c].abs_diff(w[c]))).max().unwrap_or(0)
}

fn worst_f(got: &[Pixel], want: &[[f32; 4]]) -> f32 {
    got.iter()
        .zip(want)
        .flat_map(|(g, w)| (0..4).map(move |c| (g[c] as f32 - w[c].clamp(0.0, 1.0) * 255.0).abs()))
        .fold(0.0, f32::max)
}

/// One filter on WARP with an input picture.
struct Harness {
    r: Arc<crate::gpu::Renderer>,
    f: ExtShaderFilter,
    input: Pooled,
    pixels: Vec<Pixel>,
    w: u32,
    h: u32,
    notes: Vec<Note>,
    n: u32,
    start: Instant,
}

impl Harness {
    fn new(reg: &ExtRegistry, id: &str, w: u32, h: u32, pixels: Vec<Pixel>) -> Harness {
        Harness::on(warp(), reg, id, w, h, pixels)
    }

    /// On a renderer the caller made (what shares a device with an asset service).
    fn on(r: Arc<crate::gpu::Renderer>, reg: &ExtRegistry, id: &str, w: u32, h: u32, pixels: Vec<Pixel>) -> Harness {
        let input = r.texture(w, h).unwrap();
        fill_rgba(&r, &input, |x, y| pixels[(y * w + x) as usize]);
        Harness { f: ExtShaderFilter::new(id, reg.clone()), r, input, pixels, w, h, notes: Vec::new(), n: 0, start: Instant::now() }
    }

    /// One frame as the chain runs it: settings, clock, render. Returns the picture.
    fn frame(&mut self, p: &Params) -> Vec<Pixel> {
        self.f.update(&self.r, p);
        self.f.set_frame(&FrameInfo { time: self.start.elapsed().as_secs_f64(), delta: 0.016, frame: self.n });
        self.n += 1;
        let out = self.r.texture(self.w, self.h).unwrap();
        self.f.render(&self.r, &self.input, &out).expect("an extension filter never fails a frame");
        self.f.take_notes(&mut self.notes);
        read_rgba(&self.r, &out).chunks(4).map(|c| [c[0], c[1], c[2], c[3]]).collect()
    }

    /// Frames (5 ms apart) until `done` says so; at most 30 s. Returns the last picture.
    fn until(&mut self, p: &Params, what: &str, done: impl Fn(&Harness, &[Pixel]) -> bool) -> Vec<Pixel> {
        let end = Instant::now() + Duration::from_secs(30);
        loop {
            let out = self.frame(p);
            if done(self, &out) {
                return out;
            }
            assert!(Instant::now() < end, "gave up waiting for {what}; error shown: {:?}", self.error());
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn running(&mut self, p: &Params) -> Vec<Pixel> {
        self.until(p, "the shader", |h, _| h.f.state() == FilterState::Running)
    }

    /// The error the card shows now, empty when none.
    fn error(&self) -> String {
        self.notes
            .iter()
            .rev()
            .find_map(|n| if let Note::Error(m) = n { Some(m.clone()) } else { None })
            .unwrap_or_default()
    }

    fn passes_through(&self, out: &[Pixel]) -> bool {
        out == self.pixels.as_slice()
    }
}

const VIGNETTE_BROKEN: &str = "// line 1\n// line 2\nlet x = ;\n@fragment\nfn main(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> { return vec4<f32>(1.0); }\n";

const SOLID_RED: &str = "@fragment\nfn main(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> { return vec4<f32>(1.0, 0.0, 0.0, 1.0); }\n";

// ---------------------------------------------------------------------------------------------------------------
// 1. The shipped examples

#[test]
fn vignette_at_strength_zero_is_the_identity() {
    let fx = Fixture::new();
    fx.install_example("vignette");
    assert_eq!(fx.state("vignette"), Some(ExtState::Ready));
    let (w, h) = (32, 16);
    let mut hn = Harness::new(&fx.reg, "vignette", w, h, pattern(w, h));
    let out = hn.until(&cfg(json!({ "amount": 0 })), "vignette", |h, _| h.f.state() == FilterState::Running);
    let d = worst(&out, &hn.pixels);
    eprintln!("vignette at 0: worst difference {d}/255");
    assert!(d <= 1, "difference {d}");
    // the defaults darken
    let dark = hn.frame(&cfg(json!({})));
    assert!(dark != hn.pixels, "the default vignette changes the picture");
    assert!(hn.error().is_empty(), "{}", hn.error());
}

#[test]
fn vignette_matches_its_cpu_reference() {
    let fx = Fixture::new();
    fx.install_example("vignette");
    let (w, h) = (48u32, 27u32);
    let mut hn = Harness::new(&fx.reg, "vignette", w, h, pattern(w, h));
    let (amount, inner, outer) = (0.9f32, 0.2f32, 0.7f32);
    let out = hn.until(&cfg(json!({ "amount": amount, "inner": inner, "outer": outer })), "vignette", |h, _| {
        h.f.state() == FilterState::Running
    });
    let smooth = |e0: f32, e1: f32, x: f32| {
        let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
        t * t * (3.0 - 2.0 * t)
    };
    let want: Vec<[f32; 4]> = hn
        .pixels
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let (x, y) = ((i as u32 % w) as f32, (i as u32 / w) as f32);
            let (u, v) = ((x + 0.5) / w as f32, (y + 0.5) / h as f32);
            let d = ((u - 0.5).powi(2) + (v - 0.5).powi(2)).sqrt();
            let k = 1.0 - smooth(inner, outer, d) * amount;
            [p[0] as f32 / 255.0 * k, p[1] as f32 / 255.0 * k, p[2] as f32 / 255.0 * k, p[3] as f32 / 255.0]
        })
        .collect();
    let d = worst_f(&out, &want);
    eprintln!("vignette vs CPU: worst difference {d:.2}/255");
    assert!(d <= 1.5, "difference {d}");
}

#[test]
fn duotone_maps_black_and_white_to_its_two_colours() {
    let fx = Fixture::new();
    fx.install_example("duotone");
    let (shadow, highlight) = ((0x20, 0x30, 0x70), (0xF0, 0xD0, 0x40));
    let params = cfg(json!({ "shadow": obs(shadow.0, shadow.1, shadow.2), "highlight": obs(highlight.0, highlight.1, highlight.2), "amount": 1 }));
    // black, white, mid grey, pure red (luma 0.2126), and a half transparent white
    let px = vec![[0, 0, 0, 255], [255, 255, 255, 255], [128, 128, 128, 255], [255, 0, 0, 255], [255, 255, 255, 100]];
    let mut hn = Harness::new(&fx.reg, "duotone", 5, 1, px);
    let out = hn.until(&params, "duotone", |h, _| h.f.state() == FilterState::Running);
    let mix = |t: f32, a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as i32;
    let near = |got: u8, want: i32| (got as i32 - want).abs() <= 1;
    assert!(near(out[0][0], shadow.0 as i32) && near(out[0][1], shadow.1 as i32) && near(out[0][2], shadow.2 as i32), "black -> {:?}", out[0]);
    assert!(
        near(out[1][0], highlight.0 as i32) && near(out[1][1], highlight.1 as i32) && near(out[1][2], highlight.2 as i32),
        "white -> {:?}",
        out[1]
    );
    let t = 128.0 / 255.0;
    assert!(
        near(out[2][0], mix(t, shadow.0, highlight.0)) && near(out[2][1], mix(t, shadow.1, highlight.1)) && near(out[2][2], mix(t, shadow.2, highlight.2)),
        "grey -> {:?}",
        out[2]
    );
    let t = 0.2126;
    assert!(near(out[3][0], mix(t, shadow.0, highlight.0)) && near(out[3][2], mix(t, shadow.2, highlight.2)), "red -> {:?}", out[3]);
    assert_eq!(out[0][3], 255);
    assert_eq!(out[4][3], 100, "alpha is kept");
    // amount 0 hands the picture back
    let same = hn.frame(&cfg(json!({ "shadow": obs(1, 2, 3), "amount": 0 })));
    assert!(worst(&same, &hn.pixels) <= 1, "{same:?}");
}

// ---------------------------------------------------------------------------------------------------------------
// 2. Errors, hot reload, no-ops

#[test]
fn a_compile_error_reports_file_line_and_column_and_the_picture_passes_through() {
    let fx = Fixture::new();
    fx.add("vignette", "vignette.wgsl", VIGNETTE_BROKEN, "", "");
    let mut hn = Harness::new(&fx.reg, "vignette", 8, 4, pattern(8, 4));
    let out = hn.until(&cfg(json!({})), "the error", |h, _| !h.error().is_empty());
    let message = hn.error();
    eprintln!("card shows: {message}");
    assert!(message.starts_with("vignette.wgsl:3:"), "{message}");
    assert_eq!(hn.f.state(), FilterState::Waiting);
    assert!(hn.passes_through(&out), "no shader: the picture passes through");
    // the same error is not sent again
    let before = hn.notes.len();
    for _ in 0..5 {
        hn.frame(&cfg(json!({})));
    }
    assert_eq!(hn.notes.len(), before, "{:?}", hn.notes);
}

#[test]
fn a_broken_edit_keeps_the_last_good_shader_and_a_fix_replaces_it() {
    let fx = Fixture::new();
    fx.install_example("vignette");
    let (w, h) = (16, 8);
    let mut hn = Harness::new(&fx.reg, "vignette", w, h, pattern(w, h));
    let p = cfg(json!({ "amount": 1 }));
    let good = hn.running(&p);
    assert!(good != hn.pixels);
    let first_hash = hn.f.running_hash().unwrap();

    // break the shader: the poll sees a changed file
    std::thread::sleep(Duration::from_millis(20));
    fx.write("vignette", "vignette.wgsl", VIGNETTE_BROKEN);
    fx.reg.scan(false);
    let still = hn.until(&p, "the error", |h, _| !h.error().is_empty());
    assert!(hn.error().starts_with("vignette.wgsl:3:"), "{}", hn.error());
    assert_eq!(hn.f.state(), FilterState::Running, "the old shader keeps running");
    assert_eq!(hn.f.running_hash(), Some(first_hash));
    assert!(worst(&still, &good) <= 1, "the picture is the last good one");

    // fix it with another shader: the new one takes over and the error is cleared
    std::thread::sleep(Duration::from_millis(20));
    fx.write("vignette", "vignette.wgsl", SOLID_RED);
    fx.reg.scan(false);
    // the params of the manifest do not matter to a shader that reads none
    let red = hn.until(&p, "the new shader", |h, _| h.f.running_hash() != Some(first_hash) && h.error().is_empty());
    assert!(red.iter().all(|px| *px == [255, 0, 0, 255]), "{:?}", &red[..2]);
}

#[test]
fn an_extension_that_is_switched_off_or_gone_is_a_no_op_card() {
    let fx = Fixture::new();
    fx.install_example("vignette");
    let (w, h) = (16, 8);
    let mut hn = Harness::new(&fx.reg, "vignette", w, h, pattern(w, h));
    let p = cfg(json!({ "amount": 1 }));
    let dark = hn.running(&p);
    assert!(dark != hn.pixels);

    fx.reg.set_enabled("vignette", false).unwrap();
    let off = hn.until(&p, "the card to switch off", |h, _| h.f.state() == FilterState::Waiting);
    assert!(hn.passes_through(&off));
    assert!(hn.error().is_empty(), "switched off is not an error: {}", hn.error());

    fx.reg.set_enabled("vignette", true).unwrap();
    let on = hn.running(&p);
    assert!(worst(&on, &dark) <= 1, "back on, the same picture");

    std::fs::remove_dir_all(fx.folder("vignette")).unwrap();
    fx.reg.scan(false);
    let gone = hn.until(&p, "the card to switch off", |h, _| h.f.state() == FilterState::Waiting);
    assert!(hn.passes_through(&gone));
    assert!(hn.error().is_empty(), "{}", hn.error());

    // a card of an extension that was never installed
    let mut ghost = Harness::new(&fx.reg, "never-installed", w, h, pattern(w, h));
    for _ in 0..3 {
        let out = ghost.frame(&p);
        assert!(ghost.passes_through(&out));
    }
    assert_eq!(ghost.f.state(), FilterState::Waiting);
    assert!(ghost.error().is_empty());
}

#[test]
fn a_manifest_that_breaks_keeps_what_runs_and_the_page_says_why() {
    let fx = Fixture::new();
    fx.install_example("vignette");
    let mut hn = Harness::new(&fx.reg, "vignette", 16, 8, pattern(16, 8));
    let p = cfg(json!({ "amount": 1 }));
    let good = hn.running(&p);
    std::thread::sleep(Duration::from_millis(20));
    fx.write("vignette", "extension.json", "{ not json");
    fx.reg.scan(false);
    assert_eq!(fx.state("vignette"), Some(ExtState::Error));
    for _ in 0..3 {
        let out = hn.frame(&p);
        assert!(worst(&out, &good) <= 1);
    }
    assert_eq!(hn.f.state(), FilterState::Running);
}

#[test]
fn a_card_of_a_script_only_kind_is_refused_with_a_message() {
    let fx = Fixture::new();
    fx.write("clock", "clock.ts", "export default {};");
    fx.write(
        "clock",
        "extension.json",
        r#"{ "apiVersion": 1, "id": "clock", "name": "Clock", "version": "1.0.0", "kind": "overlay", "render": "scene", "script": "clock.ts" }"#,
    );
    fx.reg.reload();
    assert_eq!(fx.state("clock"), Some(ExtState::Ready));
    let mut hn = Harness::new(&fx.reg, "clock", 8, 4, pattern(8, 4));
    let out = hn.frame(&cfg(json!({})));
    assert!(hn.passes_through(&out));
    assert!(hn.error().contains("not a filter"), "{}", hn.error());
}

// ---------------------------------------------------------------------------------------------------------------
// 3. What the shader is given

#[test]
fn frame_values_reach_the_shader_and_the_seed_changes_every_frame() {
    let fx = Fixture::new();
    let shader = "@fragment\nfn main(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {\n  return vec4<f32>(frame.time / 10.0, f32(frame.frame) / 255.0, frame.size.x / 255.0, frame.canvas.x / 8.0 / 255.0);\n}\n";
    fx.add("probe", "probe.wgsl", shader, "", "");
    let mut hn = Harness::new(&fx.reg, "probe", 32, 4, flat(32, 4, [9, 9, 9, 255]));
    hn.f.set_canvas((1920, 1080));
    hn.f.update(&hn.r, &cfg(json!({})));
    // drive the clock by hand so the numbers are exact
    let end = Instant::now() + Duration::from_secs(30);
    let out = loop {
        hn.f.update(&hn.r, &cfg(json!({})));
        hn.f.set_frame(&FrameInfo { time: 2.5, delta: 0.1, frame: 7 });
        let out = hn.r.texture(32, 4).unwrap();
        hn.f.render(&hn.r, &hn.input, &out).unwrap();
        if hn.f.state() == FilterState::Running {
            break read_rgba(&hn.r, &out);
        }
        assert!(Instant::now() < end);
        std::thread::sleep(Duration::from_millis(5));
    };
    let near = |got: u8, want: f32| (got as f32 - want).abs() <= 1.0;
    assert!(near(out[0], 63.75), "time: {}", out[0]);
    assert!(near(out[1], 7.0), "frame counter: {}", out[1]);
    assert!(near(out[2], 32.0), "size.x: {}", out[2]);
    assert!(near(out[3], 240.0), "canvas.x: {}", out[3]);

    // seed: a new value each frame
    let seed_fx = Fixture::new();
    let shader = "@fragment\nfn main(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> { return vec4<f32>(frame.seed, frame.seed, frame.seed, 1.0); }\n";
    seed_fx.add("seed", "seed.wgsl", shader, "", "");
    let mut hn = Harness::new(&seed_fx.reg, "seed", 4, 4, flat(4, 4, [0, 0, 0, 255]));
    hn.running(&cfg(json!({})));
    let seeds: Vec<u8> = (0..12).map(|_| hn.frame(&cfg(json!({})))[0][0]).collect();
    let distinct = seeds.iter().collect::<std::collections::BTreeSet<_>>().len();
    assert!(distinct >= 8, "seeds {seeds:?}");
}

#[test]
fn the_shader_sees_gamma_encoded_codes_and_writes_codes() {
    // Colour space decision of 4.3: the pool's UNORM views, no sRGB decode on read or encode on write.
    let fx = Fixture::new();
    fx.add("identity", "identity.wgsl", "@fragment\nfn main(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> { return tex(image, pos.xy * frame.inv_size); }\n", "", "");
    let px: Vec<Pixel> = (0..64).map(|i| [i * 4, 255 - i * 4, i * 2, 255]).collect();
    let mut hn = Harness::new(&fx.reg, "identity", 64, 1, px.clone());
    let out = hn.running(&cfg(json!({})));
    assert_eq!(worst(&out, &px), 0, "codes in, the same codes out");

    // a shader that needs light converts itself: to_linear then to_srgb is the identity within a code
    fx.add("roundtrip", "roundtrip.wgsl", "@fragment\nfn main(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> { let c = tex(image, pos.xy * frame.inv_size); return vec4<f32>(to_srgb(to_linear(c.rgb)), c.a); }\n", "", "");
    let mut hn = Harness::new(&fx.reg, "roundtrip", 64, 1, px.clone());
    let out = hn.running(&cfg(json!({})));
    assert!(worst(&out, &px) <= 1, "worst {}", worst(&out, &px));

    // and half way between two codes the sampler interpolates the codes themselves (a 2 x 1 picture, sampled at the
    // seam): (0 + 255) / 2 = 127.5, not the 188 that interpolating light would give
    fx.add("seam", "seam.wgsl", "@fragment\nfn main(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> { return tex(image, vec2<f32>(0.5, 0.5)); }\n", "", "");
    let mut hn = Harness::new(&fx.reg, "seam", 2, 1, vec![[0, 0, 0, 255], [255, 255, 255, 255]]);
    let out = hn.running(&cfg(json!({})));
    assert!((out[0][0] as i32 - 128).abs() <= 1, "{:?}", out[0]);
}

#[test]
fn history_is_the_previous_output_of_this_card() {
    let fx = Fixture::new();
    let shader = "@fragment\nfn main(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {\n  let uv = pos.xy * frame.inv_size;\n  return mix(tex(image, uv), tex(history, uv), 0.5);\n}\n";
    fx.add("trail", "trail.wgsl", shader, "", r#", "history": true"#);
    let mut hn = Harness::new(&fx.reg, "trail", 8, 4, flat(8, 4, [200, 100, 40, 200]));
    let p = cfg(json!({}));
    // the first frame the shader runs sees a transparent black history
    let first = hn.running(&p);
    assert!(worst(&first, &flat(8, 4, [100, 50, 20, 100])) <= 1, "{:?}", first[0]);
    let second = hn.frame(&p);
    assert!(worst(&second, &flat(8, 4, [150, 75, 30, 150])) <= 2, "{:?}", second[0]);
    let third = hn.frame(&p);
    assert!(worst(&third, &flat(8, 4, [175, 88, 35, 175])) <= 2, "{:?}", third[0]);

    // a card without "history" gets a transparent texture there, never the previous output
    let fx2 = Fixture::new();
    fx2.add("no-trail", "t.wgsl", shader, "", "");
    let mut hn = Harness::new(&fx2.reg, "no-trail", 8, 4, flat(8, 4, [200, 100, 40, 200]));
    hn.running(&p);
    let again = hn.frame(&p);
    assert!(worst(&again, &flat(8, 4, [100, 50, 20, 100])) <= 1, "{:?}", again[0]);
}

#[test]
fn path_params_are_textures_blank_until_loaded() {
    let fx = Fixture::new();
    let shader = "@fragment\nfn main(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {\n  let uv = pos.xy * frame.inv_size;\n  let base = tex(image, uv);\n  let l = tex(logo, uv);\n  return vec4<f32>(mix(base.rgb, l.rgb, l.a), base.a);\n}\n";
    let params = r#"{ "type": "path", "key": "logo", "label": "Logo", "def": "", "filter": "image" }"#;
    fx.add("frame-logo", "logo.wgsl", shader, params, "");
    let dir = crate::fx::assets::testgen::TempDir::new("ext-filter-logo");
    let (pw, ph, rgba) = block_picture(255);
    let png = dir.join("logo.png");
    write_png(&png, pw, ph, &rgba);

    let r = warp();
    let errors: Arc<std::sync::Mutex<Vec<FilterError>>> = Arc::default();
    let sink = errors.clone();
    let assets = Assets::with_scan_interval(r.gpu.clone(), Arc::new(move |e| sink.lock().unwrap().push(e)), Duration::from_millis(100));
    let client = assets.client();
    let grey = flat(pw, ph, [128, 128, 128, 255]);
    let mut hn = Harness::on(r.clone(), &fx.reg, "frame-logo", pw, ph, grey.clone());
    hn.f.attach_asset_client(&client, "card-1");

    // no path yet: the 1x1 transparent texture, so the picture is unchanged
    let out = hn.running(&cfg(json!({})));
    assert!(worst(&out, &grey) <= 1);

    // a path: the picture shows once the file is loaded
    let with = cfg(json!({ "logo": png.to_string_lossy() }));
    let out = hn.until(&with, "the image", |_, out| out.first() != Some(&[128, 128, 128, 255]));
    let want: Vec<Pixel> = rgba.chunks(4).map(|c| [c[0], c[1], c[2], 255]).collect();
    assert!(worst(&out, &want) <= 1, "{:?} vs {:?}", out[0], want[0]);

    // the path cleared: back to the plain picture
    let out = hn.until(&cfg(json!({ "logo": "" })), "the image to go", |_, out| out.first() == Some(&[128, 128, 128, 255]));
    assert!(worst(&out, &grey) <= 1);

    // a file that is not there: the error comes from the asset service, under the card's id, and the picture is unchanged
    let missing = dir.join("nope.png");
    // (the service also sends empty messages that clear an earlier error)
    let failed = |errors: &std::sync::Mutex<Vec<FilterError>>| errors.lock().unwrap().iter().find(|e| !e.message.is_empty()).cloned();
    let out = hn.until(&cfg(json!({ "logo": missing.to_string_lossy() })), "the asset error", |_, _| failed(&errors).is_some());
    assert!(worst(&out, &grey) <= 1);
    let e = failed(&errors).unwrap();
    assert_eq!(e.id, "card-1");
    assert!(e.message.contains("nope.png"), "{e:?}");
    assert!(matches!(hn.f.images[0].1.state(), AssetState::Failed(_)));
}

thread_local! {
    /// The registry the test chain's factory hands to the cards it makes (the real one is process-wide).
    static REG: std::cell::RefCell<Option<ExtRegistry>> = const { std::cell::RefCell::new(None) };
}

fn factory(kind: &str) -> Option<Box<dyn GpuFilter>> {
    let id = kind.strip_prefix(KIND_PREFIX)?;
    let reg = REG.with(|r| r.borrow().clone())?;
    Some(Box::new(ExtShaderFilter::new(id, reg)))
}

#[test]
fn the_chains_clock_reaches_the_card_frame_by_frame() {
    let fx = Fixture::new();
    // red: seconds / 10, green: frame counter, blue: delta * 100
    let shader = "@fragment\nfn main(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {\n  return vec4<f32>(frame.time / 10.0, f32(frame.frame) / 255.0, frame.delta * 100.0 / 255.0, 1.0);\n}\n";
    fx.add("clock-probe", "c.wgsl", shader, "", "");
    REG.with(|r| *r.borrow_mut() = Some(fx.reg.clone()));
    let r = warp();
    let mut chain = Chain::with_factory(factory);
    chain.apply(&r, &[FilterCfg { id: "p".into(), kind: "ext:clock-probe".into(), enabled: true, params: Params::new() }]);
    let t0 = Instant::now();
    let run = |chain: &mut Chain, at: Instant| -> [u8; 4] {
        let tex = r.texture(4, 4).unwrap();
        fill_rgba(&r, &tex, |_, _| [0, 0, 0, 255]);
        let out = chain.run_at(&r, tex, at).unwrap();
        let px = read_rgba(&r, &out);
        [px[0], px[1], px[2], px[3]]
    };
    // frames until the shader is built (the chain counts them too)
    let end = Instant::now() + Duration::from_secs(30);
    let mut at = t0;
    let mut n = 0u32;
    loop {
        let px = run(&mut chain, at);
        if px[3] == 255 && px != [0, 0, 0, 255] {
            // frame n at n * 50 ms: time 0.05 n s, delta 0.05 s (0 for the first frame)
            let (time, delta) = (0.05 * n as f32, if n == 0 { 0.0 } else { 5.0 });
            assert!((px[0] as f32 - time / 10.0 * 255.0).abs() <= 1.5, "time at frame {n}: {px:?}");
            assert_eq!(px[1] as u32, n, "frame counter");
            assert!((px[2] as f32 - delta).abs() <= 1.5, "delta at frame {n}: {px:?}");
            break;
        }
        n += 1;
        at += Duration::from_millis(50);
        assert!(Instant::now() < end, "the shader never ran");
        std::thread::sleep(Duration::from_millis(5));
    }
    // two more frames: the counter and the time move on
    at += Duration::from_millis(50);
    let a = run(&mut chain, at);
    at += Duration::from_millis(50);
    let b = run(&mut chain, at);
    assert_eq!(a[1] as u32, n + 1);
    assert_eq!(b[1] as u32, n + 2);
    assert!(b[0] >= a[0]);
}

#[test]
fn a_chain_makes_the_card_runs_it_and_follows_its_settings() {
    let fx = Fixture::new();
    fx.install_example("vignette");
    REG.with(|r| *r.borrow_mut() = Some(fx.reg.clone()));
    let r = warp();
    let mut chain = Chain::with_factory(factory);
    chain.set_canvas((16, 8));
    let entry = |amount: f64, enabled: bool| FilterCfg { id: "v1".into(), kind: "ext:vignette".into(), enabled, params: cfg(json!({ "amount": amount })) };
    let pixels = pattern(16, 8);
    let run = |chain: &mut Chain| -> Vec<Pixel> {
        let tex = r.texture(16, 8).unwrap();
        fill_rgba(&r, &tex, |x, y| pixels[(y * 16 + x) as usize]);
        let out = chain.run(&r, tex).unwrap();
        read_rgba(&r, &out).chunks(4).map(|c| [c[0], c[1], c[2], c[3]]).collect()
    };
    chain.apply(&r, &[entry(0.0, true)]);
    assert_eq!(chain.len(), 1, "the chain made an ext: card");
    let end = Instant::now() + Duration::from_secs(30);
    let mut out = run(&mut chain);
    // identity at strength 0 is also what the pass-through before the build gives: switch to 1 and wait for the change
    chain.apply(&r, &[entry(1.0, true)]);
    while out == pixels && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(5));
        out = run(&mut chain);
    }
    assert!(out != pixels, "the vignette shows");
    // the instance is kept when only the settings change
    let serial = chain.instances()[0].3;
    chain.apply(&r, &[entry(0.5, true)]);
    assert_eq!(chain.instances()[0].3, serial);
    let half = run(&mut chain);
    assert!(half != out && half != pixels);
    // switched off: the picture passes through, no error
    chain.apply(&r, &[entry(1.0, false)]);
    assert_eq!(run(&mut chain), pixels);
    assert!(chain.take_errors().is_empty(), "{:?}", chain.take_errors());
    // a card of an extension nobody installed is made too, and does nothing
    chain.apply(&r, &[FilterCfg { id: "g".into(), kind: "ext:ghost".into(), enabled: true, params: Params::new() }]);
    assert_eq!(run(&mut chain), pixels);
    assert!(chain.take_errors().is_empty());
    // a kind that is neither built in nor an extension is still skipped
    chain.apply(&r, &[FilterCfg { id: "x".into(), kind: "nonsense".into(), enabled: true, params: Params::new() }]);
    assert!(chain.is_empty());
}

#[test]
fn a_new_device_starts_the_card_again() {
    let fx = Fixture::new();
    fx.add("red", "red.wgsl", SOLID_RED, "", "");
    let mut hn = Harness::new(&fx.reg, "red", 4, 4, flat(4, 4, [0, 0, 0, 255]));
    hn.running(&cfg(json!({})));
    // another renderer is another device (the engine rebuilds one after a GPU reset)
    let other = Arc::new(crate::gpu::Renderer::new(Arc::new(
        crate::gpu::GpuDevice::new(crate::gpu::AdapterKind::Warp).unwrap(),
    ))
    .unwrap());
    let input = other.texture(4, 4).unwrap();
    fill_rgba(&other, &input, |_, _| [0, 0, 0, 255]);
    let end = Instant::now() + Duration::from_secs(30);
    loop {
        hn.f.update(&other, &cfg(json!({})));
        let out = other.texture(4, 4).unwrap();
        hn.f.render(&other, &input, &out).expect("renders on the new device");
        if hn.f.state() == FilterState::Running {
            assert_eq!(&read_rgba(&other, &out)[..4], &[255, 0, 0, 255]);
            break;
        }
        assert!(Instant::now() < end);
        std::thread::sleep(Duration::from_millis(5));
    }
}

// ---------------------------------------------------------------------------------------------------------------
// 4. The render thread never waits

#[test]
fn the_render_thread_does_not_wait_for_a_slow_build() {
    // 5000 straight-line statements: FXC needs a second or more (E1).
    let straight: String = (0..5000).map(|i| format!("  acc = acc + sin(acc * {i}.37 + {i}.1) * 0.0001;\n")).collect();
    let shader = format!("@fragment\nfn main(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {{\n  var acc = vec3<f32>(0.5);\n{straight}  return vec4<f32>(acc, 1.0);\n}}\n");
    let fx = Fixture::new();
    fx.add("slow-build", "slow.wgsl", &shader, "", "");
    let mut hn = Harness::new(&fx.reg, "slow-build", 16, 8, pattern(16, 8));
    let p = cfg(json!({}));
    let begin = Instant::now();
    let mut waiting_frames = 0;
    let mut slowest = Duration::ZERO;
    while hn.f.state() != FilterState::Running {
        let t = Instant::now();
        let out = hn.frame(&p);
        slowest = slowest.max(t.elapsed());
        if hn.f.state() == FilterState::Waiting {
            waiting_frames += 1;
            assert!(hn.passes_through(&out));
        }
        assert!(begin.elapsed() < Duration::from_secs(60), "never built; {:?}", hn.error());
        std::thread::sleep(Duration::from_millis(2));
    }
    eprintln!("built after {:?}; {waiting_frames} frames passed through meanwhile, slowest frame {slowest:?}", begin.elapsed());
    assert!(waiting_frames >= 3, "the build was not asynchronous ({waiting_frames} waiting frames)");
    assert!(slowest < Duration::from_millis(250), "a frame waited {slowest:?}");
}

// ---------------------------------------------------------------------------------------------------------------
// 5. The GPU time guard

#[test]
fn the_guard_switches_a_card_off_when_its_mean_is_over_the_limit() {
    let mut g = Guard::new(3.0, Duration::from_millis(100));
    let t0 = Instant::now();
    // under the limit for a whole window: nothing
    for i in 0..12 {
        assert_eq!(g.add(2.0, t0 + Duration::from_millis(i * 10)), None);
    }
    // over it for a whole window: the mean is reported once the window ends
    g.reset();
    let t1 = t0 + Duration::from_secs(1);
    let mut got = None;
    for i in 0..15 {
        got = got.or(g.add(5.0, t1 + Duration::from_millis(i * 10)));
    }
    let mean = got.expect("tripped");
    assert!((mean - 5.0).abs() < 1e-9);
    // one slow frame in a window of fast ones does not trip it
    g.reset();
    let t2 = t1 + Duration::from_secs(1);
    let mut got = None;
    for i in 0..15 {
        got = got.or(g.add(if i == 3 { 20.0 } else { 1.0 }, t2 + Duration::from_millis(i * 10)));
    }
    assert_eq!(got, None);
}

#[test]
fn a_slow_shader_is_bypassed_with_the_message_of_the_design() {
    // n loop iterations of sin per pixel: tuned so that WARP needs several ms for a 64 x 64 target.
    let shader = "@fragment\nfn main(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {\n  var acc = 0.0;\n  for (var i = 0; i < i32(params.n); i++) { acc = acc + sin(f32(i) + pos.x); }\n  return vec4<f32>(fract(acc), 0.5, 0.5, 1.0);\n}\n";
    let params = r#"{ "type": "slider", "key": "n", "label": "n", "min": 0, "max": 1000000, "step": 1, "def": 0, "int": true }"#;
    let fx = Fixture::new();
    fx.add("busy", "busy.wgsl", shader, params, "");
    let (w, h) = (64, 64);
    let mut hn = Harness::new(&fx.reg, "busy", w, h, pattern(w, h));
    // the real limit (3 ms over 1 s), a shader that is cheap at first
    hn.running(&cfg(json!({ "n": 1 })));
    let cheap = hn.frame(&cfg(json!({ "n": 1 })));
    assert!(!hn.passes_through(&cheap));
    assert_eq!(hn.f.state(), FilterState::Running);

    // now a busy loop
    let busy = cfg(json!({ "n": 20000 }));
    let begin = Instant::now();
    let mut slowest = Duration::ZERO;
    while hn.f.state() != FilterState::TooSlow {
        let t = Instant::now();
        hn.frame(&busy);
        slowest = slowest.max(t.elapsed());
        assert!(begin.elapsed() < Duration::from_secs(90), "never tripped; error {:?}", hn.error());
    }
    eprintln!("bypassed after {:?} (slowest frame {slowest:?}): {}", begin.elapsed(), hn.error());
    assert!(hn.error().starts_with("Too slow for live video ("), "{}", hn.error());
    assert!(hn.error().ends_with(" ms per frame)"), "{}", hn.error());
    let out = hn.frame(&busy);
    assert!(hn.passes_through(&out), "bypassed: the picture passes through");

    // the extension reloaded: a new chance
    fx.reg.reload();
    let out = hn.until(&cfg(json!({ "n": 1 })), "the reload", |h, _| h.f.state() == FilterState::Running);
    assert!(!hn.passes_through(&out));
    assert!(hn.error().is_empty(), "{}", hn.error());
}

// ---------------------------------------------------------------------------------------------------------------
// 6. The loader: cache and hostile files

fn spec(dir: &Path, file: &str, hlsl: bool, params: &[(&str, &str)]) -> BuildSpec {
    BuildSpec {
        file_name: file.to_string(),
        path: dir.join(file),
        hlsl,
        params: params.iter().map(|(n, t)| ParamDecl::new(n, t)).collect(),
        images: Vec::new(),
    }
}

const ENTRY: &str = "@fragment\nfn main(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {\n";

#[test]
fn the_loader_builds_once_per_source_and_remembers_failures_too() {
    let fx = Fixture::new();
    // The cache is shared by every test of the binary and holds 32 results, so a result can be pushed out between two
    // lines of this test; a source of its own and a few attempts make that irrelevant.
    let mut hits = false;
    for _ in 0..5 {
        let unique = format!("{SOLID_RED}// {}\n", SEQ.fetch_add(1, Ordering::SeqCst) + 5_555_000);
        fx.write("a", "a.wgsl", &unique);
        fx.write("b", "b.wgsl", &unique);
        let s = spec(&fx.folder("a"), "a.wgsl", false, &[]);
        let t0 = Instant::now();
        let first = loader::build(&s).expect("builds");
        let cold = t0.elapsed();
        let t1 = Instant::now();
        let second = loader::build(&s).expect("builds");
        let warm = t1.elapsed();
        // another file with the same text is the same build
        let third = loader::build(&spec(&fx.folder("b"), "b.wgsl", false, &[])).unwrap();
        eprintln!("cold {cold:?}, warm {warm:?}");
        if Arc::ptr_eq(&first, &second) && Arc::ptr_eq(&first, &third) {
            assert!(warm < cold / 2 + Duration::from_millis(5), "cold {cold:?} warm {warm:?}");
            assert!(loader::is_cached(&s));
            // other params are another build
            assert!(!loader::is_cached(&spec(&fx.folder("b"), "b.wgsl", false, &[("k", "f32")])), "other params, another build");
            hits = true;
            break;
        }
    }
    assert!(hits, "the second build of a source was never served from the cache");

    // the same text read under another name keeps that name in its message
    fx.write("named", "other.wgsl", &format!("{VIGNETTE_BROKEN}// shared text\n"));
    fx.write("named", "same.wgsl", &format!("{VIGNETTE_BROKEN}// shared text\n"));
    let a = loader::build(&spec(&fx.folder("named"), "other.wgsl", false, &[])).unwrap_err();
    let b = loader::build(&spec(&fx.folder("named"), "same.wgsl", false, &[])).unwrap_err();
    assert!(a.starts_with("other.wgsl:3:") && b.starts_with("same.wgsl:3:"), "{a} / {b}");

    // a failure is kept, so a poll that changed nothing does not compile it again (and a stuck FXC is not started
    // again on every poll)
    // (a source no other test builds: the cache is shared by the whole test binary)
    let unique = format!("{VIGNETTE_BROKEN}// {}\n", SEQ.fetch_add(1, Ordering::SeqCst) + 6_666_000);
    fx.write("bad", "bad.wgsl", &unique);
    let bad = spec(&fx.folder("bad"), "bad.wgsl", false, &[]);
    assert!(!loader::is_cached(&bad));
    let e1 = loader::build(&bad).unwrap_err();
    assert!(loader::is_cached(&bad));
    let e2 = loader::build(&bad).unwrap_err();
    assert_eq!(e1, e2);
    assert!(e1.starts_with("bad.wgsl:3:"), "{e1}");
}

#[test]
fn a_ticket_that_is_dropped_before_its_job_runs_is_never_built() {
    let fx = Fixture::new();
    let r = warp();
    // a shader no other test builds (the cache is shared by the whole test binary)
    let unique = format!("{SOLID_RED}// {}\n", SEQ.fetch_add(1, Ordering::SeqCst) + 7_777_000);
    fx.write("a", "a.wgsl", &unique);
    // a slow job in front keeps the loader busy; the second ticket is dropped while it waits
    let straight: String = (0..3000).map(|i| format!("  acc = acc + sin(acc * {i}.37 + {i}.1) * 0.0001;\n")).collect();
    fx.write("slow", "slow.wgsl", &format!("{ENTRY}  var acc = vec3<f32>(0.5);\n{straight}  return vec4<f32>(acc, 1.0);\n}}\n"));
    let slow = loader::submit(spec(&fx.folder("slow"), "slow.wgsl", false, &[]), r.gpu.device.clone());
    let dropped = loader::submit(spec(&fx.folder("a"), "a.wgsl", false, &[]), r.gpu.device.clone());
    drop(dropped);
    let end = Instant::now() + Duration::from_secs(60);
    let result = loop {
        if let Some(res) = slow.take() {
            break res;
        }
        assert!(Instant::now() < end, "the slow job never finished");
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(result.is_ok());
    // the queue is in order: once a job queued after the dropped one is done, the loader has passed over it
    let marker_text = format!("{SOLID_RED}// {}\n", SEQ.fetch_add(1, Ordering::SeqCst) + 8_888_000);
    fx.write("marker", "m.wgsl", &marker_text);
    let marker = loader::submit(spec(&fx.folder("marker"), "m.wgsl", false, &[]), r.gpu.device.clone());
    let end = Instant::now() + Duration::from_secs(60);
    while marker.take().is_none() {
        assert!(Instant::now() < end, "the marker job never finished");
        std::thread::sleep(Duration::from_millis(10));
    }
    let slow_spec = spec(&fx.folder("slow"), "slow.wgsl", false, &[]);
    assert!(loader::is_cached(&slow_spec));
    assert!(!loader::is_cached(&spec(&fx.folder("a"), "a.wgsl", false, &[])), "the dropped job was skipped");
}

/// What a card shows for a hostile file, and how long the loader took to say it.
fn hostile(fx: &Fixture, name: &str, file: &str, text: &[u8], hlsl: bool) -> (Result<(), String>, Duration) {
    std::fs::create_dir_all(fx.folder(name)).unwrap();
    std::fs::write(fx.folder(name).join(file), text).unwrap();
    let t = Instant::now();
    let r = loader::build(&spec(&fx.folder(name), file, hlsl, &[])).map(|_| ());
    (r, t.elapsed())
}

#[test]
fn hostile_wgsl_files_end_in_a_card_error_or_a_build_never_a_hang() {
    let fx = Fixture::new();
    let deep_parens = format!("{ENTRY}  return vec4<f32>({}1.0{});\n}}\n", "(".repeat(30_000), ")".repeat(30_000));
    let deep_blocks = format!("{ENTRY}{}  {}\n  return vec4<f32>(1.0);\n}}\n", "  {\n".repeat(20_000), "}\n".repeat(20_000));
    let long_sum = format!("{ENTRY}  let a = pos.x;\n  let b = {};\n  return vec4<f32>(b);\n}}\n", vec!["a"; 125_000].join("+"));
    let noise: String = (0..60_000u32).map(|i| char::from_u32(0x20 + i % 95).unwrap()).collect();
    let mut bomb = String::from("fn blur0(uv: vec2<f32>) -> vec4<f32> { return tex(image, uv); }\n");
    for i in 1..30 {
        bomb.push_str(&format!(
            "fn blur{i}(uv: vec2<f32>) -> vec4<f32> {{ return (blur{}(uv + vec2<f32>(0.001, 0.0)) + blur{}(uv - vec2<f32>(0.001, 0.0))) * 0.5; }}\n",
            i - 1,
            i - 1
        ));
    }
    let bomb = format!("{bomb}{ENTRY}  return blur29(pos.xy * frame.inv_size);\n}}\n");
    let too_big = format!("{SOLID_RED}// {}\n", "x".repeat(300 * 1024));
    let cases: Vec<(&str, Vec<u8>, &str)> = vec![
        ("nul byte", format!("{ENTRY}  return vec4<f32>(1.0);\u{0}\n}}\n").into_bytes(), ""),
        ("30000 nested parentheses", deep_parens.into_bytes(), ""),
        ("20000 nested blocks", deep_blocks.into_bytes(), ""),
        ("125000-term sum", long_sum.into_bytes(), ""),
        ("printable noise", noise.into_bytes(), ""),
        ("inlining bomb", bomb.into_bytes(), "inlined"),
        ("not UTF-8", vec![0xff, 0xfe, 0xfd, b'x'], "not UTF-8"),
        ("over the size limit", too_big.into_bytes(), "limit is 256 KB"),
        ("empty file", Vec::new(), ""),
    ];
    for (name, text, expect) in cases {
        let (result, took) = hostile(&fx, "hostile", "h.wgsl", &text, false);
        eprintln!("[{name}] {took:?}: {}", match &result { Ok(()) => "built".to_string(), Err(e) => e.chars().take(110).collect() });
        assert!(took < Duration::from_secs(45), "{name} took {took:?}");
        if let Err(e) = &result {
            assert!(e.starts_with("h.wgsl"), "{name}: the message names the file: {e}");
        }
        if !expect.is_empty() {
            let e = result.expect_err(name);
            assert!(e.contains(expect), "{name}: {e}");
        }
    }
    // a byte order mark is stripped
    let bom = format!("\u{feff}{SOLID_RED}");
    let (result, _) = hostile(&fx, "bom", "b.wgsl", bom.as_bytes(), false);
    assert!(result.is_ok(), "{result:?}");
    // a shader that is a directory, a missing file
    std::fs::create_dir_all(fx.folder("dir").join("d.wgsl")).unwrap();
    let e = loader::build(&spec(&fx.folder("dir"), "d.wgsl", false, &[])).unwrap_err();
    assert!(e.starts_with("d.wgsl"), "{e}");
    let e = loader::build(&spec(&fx.folder("dir"), "missing.wgsl", false, &[])).unwrap_err();
    assert!(e.starts_with("missing.wgsl") && e.contains("cannot be read"), "{e}");
    assert!(stuck_workers() <= 2);
}

#[test]
fn a_hostile_shader_on_a_card_leaves_the_picture_flowing() {
    let fx = Fixture::new();
    let mut bomb = String::from("fn blur0(uv: vec2<f32>) -> vec4<f32> { return tex(image, uv); }\n");
    for i in 1..30 {
        bomb.push_str(&format!(
            "fn blur{i}(uv: vec2<f32>) -> vec4<f32> {{ return (blur{}(uv + vec2<f32>(0.001, 0.0)) + blur{}(uv - vec2<f32>(0.001, 0.0))) * 0.5; }}\n",
            i - 1,
            i - 1
        ));
    }
    fx.add("bomb", "bomb.wgsl", &format!("{bomb}{ENTRY}  return blur29(pos.xy * frame.inv_size);\n}}\n"), "", "");
    let mut hn = Harness::new(&fx.reg, "bomb", 16, 8, pattern(16, 8));
    let begin = Instant::now();
    let out = hn.until(&cfg(json!({})), "the refusal", |h, _| !h.error().is_empty());
    assert!(begin.elapsed() < Duration::from_secs(10));
    assert!(hn.error().starts_with("bomb.wgsl"), "{}", hn.error());
    assert!(hn.error().contains("inlined"), "{}", hn.error());
    assert!(hn.passes_through(&out));
    assert_eq!(hn.f.state(), FilterState::Waiting);
}

// ---------------------------------------------------------------------------------------------------------------
// 7. Raw HLSL

const HLSL_TINT: &str = "float4 main(float4 pos : SV_Position) : SV_Target {\n  float2 uv = pos.xy * frame.inv_size;\n  float4 c = tex(image, uv);\n  return float4(lerp(c.rgb, params.tint.rgb, params.amount), c.a);\n}\n";

fn hlsl_params() -> &'static str {
    r#"{ "type": "slider", "key": "amount", "label": "Amount", "min": 0, "max": 1, "step": 0.01, "def": 0.5 },
       { "type": "color", "key": "tint", "label": "Tint", "def": 4278190335 }"#
}

#[test]
fn an_hlsl_shader_runs_with_the_same_names_and_registers() {
    let fx = Fixture::new();
    fx.add("hlsl-tint", "tint.hlsl", HLSL_TINT, hlsl_params(), "");
    let mut hn = Harness::new(&fx.reg, "hlsl-tint", 16, 8, flat(16, 8, [40, 80, 120, 255]));
    // tint 0xFF0000FF is opaque red in OBS' AABBGGRR
    let out = hn.until(&cfg(json!({ "amount": 0.5, "tint": obs(255, 0, 0) })), "the shader", |h, _| h.f.state() == FilterState::Running);
    let want = [((40.0 + 255.0) / 2.0f32).round() as u8, 40, 60, 255];
    assert!(worst(&out, &flat(16, 8, want)) <= 1, "{:?} vs {want:?}", out[0]);
    // history, images and the helpers are there too
    let shader = "float4 main(float4 pos : SV_Position) : SV_Target {\n  float2 uv = pos.xy * frame.inv_size;\n  float4 c = tex(image, uv);\n  float3 back = to_srgb(to_linear(c.rgb));\n  return float4(back, c.a) * 0.5 + tex(history, uv) * 0.5 + texel(image, int2(0, 0)) * 0.0 + luma(c.rgb) * 0.0 + hash(uv) * 0.0 + frame.time * 0.0;\n}\n";
    fx.add("hlsl-misc", "misc.hlsl", shader, "", r#", "history": true"#);
    let mut hn = Harness::new(&fx.reg, "hlsl-misc", 8, 4, flat(8, 4, [100, 150, 200, 255]));
    let first = hn.running(&cfg(json!({})));
    assert!(worst(&first, &flat(8, 4, [50, 75, 100, 128])) <= 2, "{:?}", first[0]);
    assert!(hn.error().is_empty(), "{}", hn.error());
}

#[test]
fn an_hlsl_shader_reads_its_offsets_from_the_compiled_bytecode() {
    // WGSL puts a vec2 after an f32 at offset 8, HLSL packs it at 4; the layout must say 4 and the values must arrive.
    let decls = [ParamDecl::new("a", "f32"), ParamDecl::new("b", "vec2<f32>"), ParamDecl::new("c", "f32")];
    let src = "float4 main(float4 pos : SV_Position) : SV_Target { return float4(params.a, params.b.x, params.b.y, params.c); }\n";
    let (t, code) = hlsl::translate_hlsl(src, &decls, &[]).expect("compiles");
    let layout = t.params.as_ref().expect("the shader reads params");
    let at = |n: &str| layout.members.iter().find(|m| m.name == n).unwrap().offset;
    assert_eq!((at("a"), at("b"), at("c")), (0, 4, 12), "HLSL packing, read from FXC");
    let frame_layout = t.frame.as_ref();
    assert!(frame_layout.is_none() || frame_layout.unwrap().members.iter().any(|m| m.name == "size"));
    // and the same shader in WGSL puts them elsewhere
    let wgsl = "@fragment\nfn main(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> { return vec4<f32>(params.a, params.b.x, params.b.y, params.c); }\n";
    let w = wgsl::translate(wgsl, &decls, &[]).unwrap();
    let wl = w.params.as_ref().unwrap();
    assert_eq!(wl.members.iter().find(|m| m.name == "b").unwrap().offset, 8);

    // run it: four values in, four out
    let r = warp();
    let ps = {
        let mut ps = None;
        // SAFETY: bytecode FXC just made, on the device that owns it.
        unsafe { r.gpu.device.CreatePixelShader(&code, None, Some(&mut ps)).unwrap() };
        ps.unwrap()
    };
    let bytes = layout
        .pack(&[
            ("a", layout::Value::F32(0.25)),
            ("b", layout::Value::vec([0.5, 0.75])),
            ("c", layout::Value::F32(1.0)),
        ])
        .unwrap();
    let b1 = r.constants(bytes.len().max(16)).unwrap();
    b1.write_bytes(&r.gpu.context, &bytes).unwrap();
    let input = r.texture(2, 2).unwrap();
    fill_rgba(&r, &input, |_, _| [0, 0, 0, 0]);
    let out = r.texture(2, 2).unwrap();
    r.draw_with(
        &crate::gpu::Pass { ps: &ps, srvs: &[Some(input.srv.clone())], constants: None, target: &out.rtv, width: 2, height: 2 },
        &crate::gpu::DrawExtras { constants_b1: Some(&b1) },
    );
    let px = read_rgba(&r, &out);
    let near = |g: u8, w: f32| (g as f32 - w * 255.0).abs() <= 1.0;
    assert!(near(px[0], 0.25) && near(px[1], 0.5) && near(px[2], 0.75) && near(px[3], 1.0), "{:?}", &px[..4]);
}

#[test]
fn an_hlsl_error_is_reported_at_the_line_of_the_authors_file() {
    let decls = [ParamDecl::new("amount", "f32")];
    let src = "// one\nfloat4 main(float4 pos : SV_Position) : SV_Target {\n  return float4(undefined_name, 0, 0, 1);\n}\n";
    let e = hlsl::translate_hlsl(src, &decls, &[]).unwrap_err();
    assert_eq!(e.kind, ErrorKind::Fxc, "{e:?}");
    assert_eq!(e.line, Some(3), "{e:?}");
    let shown = e.render("tint.hlsl");
    eprintln!("{shown}");
    assert!(shown.starts_with("tint.hlsl:3:"), "{shown}");
    assert!(shown.contains("undefined_name"), "{shown}");
    // a missing entry point
    let e = hlsl::translate_hlsl("float4 other(float4 p : SV_Position) : SV_Target { return p; }\n", &[], &[]).unwrap_err();
    assert!(!e.message.is_empty());
    // a problem in a manifest name
    let e = hlsl::translate_hlsl("float4 main(float4 p : SV_Position) : SV_Target { return p; }\n", &[ParamDecl::new("1bad", "f32")], &[]).unwrap_err();
    assert_eq!(e.kind, ErrorKind::Manifest);
}

#[test]
fn hostile_hlsl_files_end_in_a_card_error_or_a_build_never_a_hang() {
    let fx = Fixture::new();
    let main = "float4 main(float4 pos : SV_Position) : SV_Target {\n";
    let deep = format!("{main}  return {}1.0{};\n}}\n", "(".repeat(5_000), ")".repeat(5_000));
    let long_sum = format!("{main}  float a = pos.x;\n  float b = {};\n  return float4(b, b, b, 1);\n}}\n", vec!["a"; 40_000].join("+"));
    let recursion = "float f(float x) { return f(x) + 1; }\nfloat4 main(float4 pos : SV_Position) : SV_Target { return f(pos.x); }\n";
    let include = "#include \"C:/Windows/win.ini\"\nfloat4 main(float4 pos : SV_Position) : SV_Target { return 1; }\n";
    let include_rel = "#include \"../secret.hlsl\"\nfloat4 main(float4 pos : SV_Position) : SV_Target { return 1; }\n";
    let pragma = "#pragma pack_matrix(row_major)\n#define X(a) a\nfloat4 main(float4 pos : SV_Position) : SV_Target { return X(1); }\n";
    let many: String = (0..3000).map(|i| format!("float h{i}(float x) {{ return x + {i}; }}\n")).collect();
    let cases: Vec<(&str, Vec<u8>, &str)> = vec![
        ("5000 nested parentheses", deep.into_bytes(), "nested"),
        ("40000-term sum", long_sum.into_bytes(), ""),
        ("recursion", recursion.as_bytes().to_vec(), ""),
        ("#include of an absolute path", include.as_bytes().to_vec(), ""),
        ("#include of a relative path", include_rel.as_bytes().to_vec(), ""),
        ("#pragma and #define", pragma.as_bytes().to_vec(), ""),
        ("3000 functions", format!("{many}float4 main(float4 pos : SV_Position) : SV_Target {{ return h2999(pos.x); }}\n").into_bytes(), ""),
        ("NUL byte", format!("{main}  return 1;\u{0}\n}}\n").into_bytes(), ""),
        ("not UTF-8", vec![0xff, 0xfe, 0x00], "not UTF-8"),
        ("empty file", Vec::new(), ""),
    ];
    for (name, text, expect) in cases {
        let (result, took) = hostile(&fx, "hlsl-hostile", "h.hlsl", &text, true);
        eprintln!("[{name}] {took:?}: {}", match &result { Ok(()) => "built".to_string(), Err(e) => e.chars().take(110).collect() });
        assert!(took < Duration::from_secs(45), "{name} took {took:?}");
        if let Err(e) = &result {
            assert!(e.starts_with("h.hlsl"), "{name}: {e}");
        }
        if !expect.is_empty() {
            let e = result.expect_err(name);
            assert!(e.contains(expect), "{name}: {e}");
        }
    }
    // reading outside the folder is impossible: neither include form built a shader that could contain the file
    let (r, _) = hostile(&fx, "hlsl-hostile", "h.hlsl", include.as_bytes(), true);
    assert!(r.is_err(), "#include must not be honoured");
    let (r, _) = hostile(&fx, "hlsl-hostile", "h.hlsl", include_rel.as_bytes(), true);
    assert!(r.is_err(), "#include must not be honoured");
    assert!(stuck_workers() <= 2);
}

#[test]
fn param_declarations_follow_the_manifest() {
    let json = r#"{ "apiVersion": 1, "id": "all-types", "name": "All", "version": "1.0.0", "kind": "filter", "shader": "a.wgsl",
      "params": [
        { "type": "slider", "key": "s", "label": "s", "min": 0, "max": 1, "step": 0.1, "def": 0 },
        { "type": "int", "key": "i", "label": "i", "min": 0, "max": 9, "def": 1 },
        { "type": "bool", "key": "b", "label": "b", "def": false },
        { "type": "select", "key": "sel_num", "label": "n", "def": 2, "options": [ { "value": 1, "label": "one" }, { "value": 2, "label": "two" } ] },
        { "type": "select", "key": "sel_str", "label": "t", "def": "x", "options": [ { "value": "x", "label": "X" }, { "value": "y", "label": "Y" } ] },
        { "type": "color", "key": "c", "label": "c", "def": 4278190080 },
        { "type": "path", "key": "img", "label": "img", "def": "", "filter": "image" },
        { "type": "text", "key": "t", "label": "t", "def": "" },
        { "type": "combo", "key": "co", "label": "co", "def": "a", "options": ["a", "b"] }
      ],
      "uniforms": [] }"#;
    let dir = Fixture::new();
    dir.write("all-types", "a.wgsl", "x");
    let m = manifest::Manifest::parse(json, "all-types").unwrap();
    let got: Vec<(String, String)> = m.param_decls().into_iter().map(|d| (d.name, d.ty)).collect();
    let want: Vec<(&str, &str)> = vec![
        ("s", "f32"),
        ("i", "i32"),
        ("b", "u32"),
        ("sel_num", "f32"),
        ("sel_str", "u32"),
        ("c", "vec4<f32>"),
    ];
    assert_eq!(got, want.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect::<Vec<_>>());
    assert_eq!(m.image_keys(), vec!["img"]);
}

#[test]
fn settings_become_params_values_with_the_manifests_ranges() {
    use layout::Value;
    let json = r#"{ "apiVersion": 1, "id": "vals", "name": "Vals", "version": "1.0.0", "kind": "filter", "shader": "a.wgsl",
      "params": [
        { "type": "slider", "key": "s", "label": "s", "min": 0, "max": 2, "step": 0.1, "def": 1 },
        { "type": "slider", "key": "si", "label": "si", "min": 0, "max": 10, "step": 1, "def": 3, "int": true },
        { "type": "int", "key": "i", "label": "i", "min": -5, "max": 9, "def": 1 },
        { "type": "bool", "key": "b", "label": "b", "def": true },
        { "type": "select", "key": "sn", "label": "n", "def": 2, "options": [ { "value": 1, "label": "one" }, { "value": 2, "label": "two" } ] },
        { "type": "select", "key": "ss", "label": "t", "def": "x", "options": [ { "value": "x", "label": "X" }, { "value": "y", "label": "Y" } ] },
        { "type": "color", "key": "c", "label": "c", "def": 4278190335 },
        { "type": "color", "key": "ca", "label": "ca", "def": 2147483648, "alpha": true }
      ] }"#;
    let m = manifest::Manifest::parse(json, "vals").unwrap();
    let get = |c: Params, k: &str| params::values(&m, &c).into_iter().find(|(n, _)| n == k).unwrap().1;
    // defaults for an empty card
    let e = Params::new();
    assert_eq!(get(e.clone(), "s"), Value::F32(1.0));
    assert_eq!(get(e.clone(), "si"), Value::F32(3.0));
    assert_eq!(get(e.clone(), "i"), Value::I32(1));
    assert_eq!(get(e.clone(), "b"), Value::U32(1));
    assert_eq!(get(e.clone(), "sn"), Value::F32(2.0));
    assert_eq!(get(e.clone(), "ss"), Value::U32(0));
    assert_eq!(get(e.clone(), "c"), Value::vec([1.0, 0.0, 0.0, 1.0]));
    assert_eq!(get(e, "ca"), Value::vec([0.0, 0.0, 0.0, 128.0 / 255.0]));
    // clamped, rounded, coerced
    assert_eq!(get(cfg(json!({ "s": 9 })), "s"), Value::F32(2.0));
    assert_eq!(get(cfg(json!({ "s": -3 })), "s"), Value::F32(0.0));
    assert_eq!(get(cfg(json!({ "s": "loud" })), "s"), Value::F32(1.0));
    assert_eq!(get(cfg(json!({ "si": 4.6 })), "si"), Value::F32(5.0));
    assert_eq!(get(cfg(json!({ "i": -50 })), "i"), Value::I32(-5));
    assert_eq!(get(cfg(json!({ "i": 4.4 })), "i"), Value::I32(4));
    assert_eq!(get(cfg(json!({ "b": false })), "b"), Value::U32(0));
    assert_eq!(get(cfg(json!({ "b": 0 })), "b"), Value::U32(1), "only a JSON boolean counts");
    assert_eq!(get(cfg(json!({ "sn": 1 })), "sn"), Value::F32(1.0));
    assert_eq!(get(cfg(json!({ "sn": 7 })), "sn"), Value::F32(2.0), "not an option: the default");
    assert_eq!(get(cfg(json!({ "ss": "y" })), "ss"), Value::U32(1));
    assert_eq!(get(cfg(json!({ "ss": "z" })), "ss"), Value::U32(0));
    // a colour without "alpha" is opaque whatever the card holds; with it the alpha byte counts
    assert_eq!(get(cfg(json!({ "c": 0x0000FF00u32 })), "c"), Value::vec([0.0, 1.0, 0.0, 1.0]));
    assert_eq!(get(cfg(json!({ "ca": 0x4000FF00u32 })), "ca"), Value::vec([0.0, 1.0, 0.0, 64.0 / 255.0]));
}
