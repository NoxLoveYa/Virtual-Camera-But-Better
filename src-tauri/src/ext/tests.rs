//! Spike E1: WGSL -> naga -> HLSL -> FXC -> the engine's `ShaderFilter` on WARP (and once on the hardware device),
//! compared with CPU references; error paths; measurements.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows::core::{Interface, PCSTR};
use windows::Win32::Graphics::Direct3D::Fxc::D3DReflect;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11ShaderReflection, D3D11_SHADER_BUFFER_DESC, D3D11_SHADER_TYPE_DESC, D3D11_SHADER_VARIABLE_DESC,
};

use super::compile::compile_ps;
use super::layout::Value;
use super::wgsl::{translate, translate_module, FrameValues, ParamDecl, Translated};
use super::{ErrorKind, ExtError};
use crate::fx::shader_filter::{ExtraInput, ShaderFilter, ShaderFilterDef};
use crate::fx::{GpuFilter, Params};
use crate::gpu::pipeline_tests::{fill_rgba, read_rgba, warp};
use crate::gpu::{AdapterKind, GpuDevice, Renderer};

type Pixel = [u8; 4];

const IDENTITY: &str = include_str!("examples/identity.wgsl");
const COLOR: &str = include_str!("examples/color.wgsl");
const UVDUMP: &str = include_str!("examples/uvdump.wgsl");
const VIGNETTE: &str = include_str!("examples/vignette.wgsl");
const ANIMATED: &str = include_str!("examples/animated.wgsl");
const BUILTINS: &str = include_str!("examples/builtins.wgsl");
const DERIVATIVES: &str = include_str!("examples/derivatives.wgsl");
const VHS: &str = include_str!("examples/vhs.wgsl");
const TWO_INPUTS: &str = include_str!("examples/two_inputs.wgsl");

// ---------------------------------------------------------------------------------------------------------------
// Helpers

fn hardware() -> Option<Arc<Renderer>> {
    let gpu = GpuDevice::new(AdapterKind::Hardware).ok()?;
    let r = Renderer::new(Arc::new(gpu)).ok()?;
    Some(Arc::new(r))
}

fn decls(list: &[(&str, &str)]) -> Vec<ParamDecl> {
    list.iter().map(|(n, t)| ParamDecl::new(n, t)).collect()
}

fn tr(src: &str, params: &[(&str, &str)], images: &[&str]) -> Translated {
    match translate(src, &decls(params), images) {
        Ok(t) => t,
        Err(e) => panic!("translation failed: {}\n--- source ---\n{src}", e.render("shader.wgsl")),
    }
}

fn err(src: &str, params: &[(&str, &str)]) -> ExtError {
    match translate(src, &decls(params), &[]) {
        Ok(t) => panic!("expected an error, got HLSL:\n{}", t.hlsl),
        Err(e) => e,
    }
}

thread_local! {
    /// What the next `pack` call of the leaked test definitions returns (`cbuffer b0`, the frame values).
    static FRAME_BYTES: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// A picture that is not symmetric in x or y and has varying alpha.
fn picture(w: u32, h: u32) -> Vec<Pixel> {
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

/// An extra input: its size and pixels.
type Extra = (u32, u32, Vec<Pixel>);

/// Run `t` as one pass through the engine's `ShaderFilter` over `input` (`w` x `h`).
///
/// `cbuffer b0` (the frame values) goes through `ShaderFilterDef::pack`, so the filter machinery uploads it. The
/// existing pass binds only constant buffer slot 0, so `params` (`b1`) is bound by hand before the draw, which the
/// pass leaves alone. `extras` are by texture name; textures the shader declares but the caller does not give are
/// 1x1 transparent.
#[allow(clippy::too_many_arguments)]
fn run(
    r: &Arc<Renderer>,
    t: &Translated,
    w: u32,
    h: u32,
    input: &[Pixel],
    frame: &FrameValues,
    params: &[(&str, Value)],
    extras: &[(&str, Extra)],
) -> Vec<Pixel> {
    use windows::Win32::Graphics::Direct3D11::ID3D11ShaderResourceView;

    let frame_bytes = match t.pack_frame(frame) {
        Some(b) => b.expect("frame values pack"),
        None => vec![0u8; 16],
    };
    FRAME_BYTES.with(|f| *f.borrow_mut() = frame_bytes);

    // Textures at t1.. in register order.
    let mut slots: Vec<(u32, Extra)> = Vec::new();
    for slot in t.textures.iter().filter(|s| s.register >= 1) {
        let given = extras.iter().find(|(n, _)| *n == slot.name).map(|(_, e)| e.clone());
        slots.push((slot.register, given.unwrap_or((1, 1, vec![[0, 0, 0, 0]]))));
    }
    let top = slots.iter().map(|(reg, _)| *reg).max().unwrap_or(0) as usize;
    let mut extra_textures = Vec::new();
    let mut srvs: Vec<Option<ID3D11ShaderResourceView>> = vec![None; top];
    for (reg, (ew, eh, px)) in &slots {
        let tex = r.texture(*ew, *eh).unwrap();
        fill_rgba(r, &tex, |x, y| px[(y * ew + x) as usize]);
        srvs[*reg as usize - 1] = Some(tex.srv.clone());
        extra_textures.push(tex);
    }
    // a register nobody fills (t1 when only t2 is used) gets a transparent texel
    for (i, s) in srvs.iter_mut().enumerate() {
        if s.is_none() {
            let tex = r.texture(1, 1).unwrap();
            fill_rgba(r, &tex, |_, _| [0, 0, 0, 0]);
            *s = Some(tex.srv.clone());
            extra_textures.push(tex);
            let _ = i;
        }
    }

    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let name: &'static str = Box::leak(format!("ext_test_{n}").into_boxed_str());
    let hlsl: &'static str = Box::leak(t.hlsl.clone().into_boxed_str());
    let extra: &'static [ExtraInput] = Box::leak(vec![ExtraInput::Image; top].into_boxed_slice());
    let def: &'static ShaderFilterDef = Box::leak(Box::new(ShaderFilterDef {
        kind: name,
        hlsl,
        pack: |_| FRAME_BYTES.with(|f| f.borrow().clone()),
        extra,
    }));
    let mut filter = ShaderFilter::new(def);
    for (i, s) in srvs.into_iter().enumerate() {
        filter.set_extra(i, s);
    }
    filter.update(r, &Params::new());

    let params_buffer = t.pack_params(params).map(|b| b.expect("params pack"));
    let constants = params_buffer.as_ref().map(|bytes| {
        let c = r.constants(bytes.len()).unwrap();
        c.write_bytes(&r.gpu.context, bytes).unwrap();
        c
    });
    if let Some(c) = &constants {
        // SAFETY: plain state call with a live buffer.
        unsafe { r.gpu.context.PSSetConstantBuffers(1, Some(&[Some(c.buffer.clone())])) };
    }

    let tex_in = r.texture(w, h).unwrap();
    fill_rgba(r, &tex_in, |x, y| input[(y * w + x) as usize]);
    let tex_out = r.texture(w, h).unwrap();
    filter.render(r, &tex_in, &tex_out).expect("the translated shader renders");
    read_rgba(r, &tex_out).chunks(4).map(|p| [p[0], p[1], p[2], p[3]]).collect()
}

fn frame(w: u32, h: u32) -> FrameValues {
    FrameValues {
        size: [w as f32, h as f32],
        canvas: [1920.0, 1080.0],
        ..Default::default()
    }
}

/// The worst difference in code values, and where.
fn worst(got: &[Pixel], want: &[[f32; 4]]) -> (f32, usize) {
    assert_eq!(got.len(), want.len());
    let mut w = (0.0f32, 0usize);
    for (i, (g, e)) in got.iter().zip(want).enumerate() {
        for c in 0..4 {
            let d = (g[c] as f32 - e[c].clamp(0.0, 1.0) * 255.0).abs();
            if d > w.0 {
                w = (d, i);
            }
        }
    }
    w
}

fn expect_close(name: &str, got: &[Pixel], want: &[[f32; 4]], tol: f32) {
    let (d, i) = worst(got, want);
    eprintln!("{name}: worst difference {d:.2} code values");
    assert!(
        d <= tol,
        "{name}: pixel {i}: got {:?}, expected {:?} (difference {d:.2} > {tol})",
        got[i],
        want[i].map(|v| v.clamp(0.0, 1.0) * 255.0)
    );
}

/// The picture as floats 0..=1.
fn floats(px: &[Pixel]) -> Vec<[f32; 4]> {
    px.iter().map(|p| p.map(|v| v as f32 / 255.0)).collect()
}

/// Linear filtering with clamp to edge, as the pass's sampler does it (u, v in 0..1).
fn bilinear(img: &[[f32; 4]], w: u32, h: u32, u: f32, v: f32) -> [f32; 4] {
    let x = u * w as f32 - 0.5;
    let y = v * h as f32 - 0.5;
    let (x0, y0) = (x.floor(), y.floor());
    let (fx, fy) = (x - x0, y - y0);
    let at = |xi: f32, yi: f32| {
        let xi = (xi as i32).clamp(0, w as i32 - 1) as usize;
        let yi = (yi as i32).clamp(0, h as i32 - 1) as usize;
        img[yi * w as usize + xi]
    };
    let (a, b, c, d) = (at(x0, y0), at(x0 + 1.0, y0), at(x0, y0 + 1.0), at(x0 + 1.0, y0 + 1.0));
    let mut out = [0.0; 4];
    for k in 0..4 {
        let top = a[k] + (b[k] - a[k]) * fx;
        let bottom = c[k] + (d[k] - c[k]) * fx;
        out[k] = top + (bottom - top) * fy;
    }
    out
}

fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn fract(x: f32) -> f32 {
    x - x.floor()
}

// ---------------------------------------------------------------------------------------------------------------
// 1. Identity

#[test]
fn identity_filter_returns_its_input() {
    let r = warp();
    let t = tr(IDENTITY, &[], &[]);
    assert!(t.params.is_none() && t.uses_sampler && t.textures.len() == 2);
    for (w, h) in [(32u32, 16u32), (30, 14), (1, 1)] {
        let px = picture(w, h);
        let out = run(&r, &t, w, h, &px, &frame(w, h), &[], &[]);
        expect_close(&format!("identity {w}x{h}"), &out, &floats(&px), 1.0);
    }
}

// ---------------------------------------------------------------------------------------------------------------
// 2. Parameters

const COLOR_PARAMS: [(&str, &str); 6] = [
    ("gain", "f32"),
    ("tint", "vec3<f32>"),
    ("level", "f32"),
    ("off", "vec2<f32>"),
    ("mul", "vec4<f32>"),
    ("invert", "u32"),
];

struct ColorCase {
    gain: f32,
    tint: [f32; 3],
    level: f32,
    off: [f32; 2],
    mul: [f32; 4],
    invert: u32,
}

fn color_case(r: &Arc<Renderer>, name: &str) {
    let t = tr(COLOR, &COLOR_PARAMS, &[]);
    // The layout WGSL gives (and HLSL, packing it by itself, would not: it puts `tint` at 4).
    let params = t.params.as_ref().expect("params struct");
    let offsets: Vec<(&str, u32)> = params.members.iter().map(|m| (m.name.as_str(), m.offset)).collect();
    assert_eq!(
        offsets,
        [("gain", 0), ("tint", 16), ("level", 28), ("off", 32), ("mul", 48), ("invert", 64)]
    );
    assert_eq!(params.buffer_size(), 80);

    let (w, h) = (32u32, 16u32);
    let px = picture(w, h);
    for (i, c) in [
        ColorCase { gain: 1.0, tint: [1.0, 1.0, 1.0], level: 0.0, off: [0.0, 0.0], mul: [1.0, 1.0, 1.0, 1.0], invert: 0 },
        ColorCase { gain: 0.8, tint: [1.0, 0.5, 0.25], level: 0.125, off: [0.0625, 0.25], mul: [0.9, 1.0, 0.75, 0.5], invert: 0 },
        ColorCase { gain: 1.5, tint: [0.3, 0.9, 0.6], level: 0.2, off: [0.1, 0.05], mul: [1.0, 0.8, 0.6, 0.9], invert: 1 },
    ]
    .iter()
    .enumerate()
    {
        let out = run(
            r,
            &t,
            w,
            h,
            &px,
            &frame(w, h),
            &[
                ("gain", c.gain.into()),
                ("tint", Value::vec(c.tint)),
                ("level", c.level.into()),
                ("off", Value::vec(c.off)),
                ("mul", Value::vec(c.mul)),
                ("invert", c.invert.into()),
            ],
            &[],
        );
        let want: Vec<[f32; 4]> = floats(&px)
            .iter()
            .map(|p| {
                let mut rgb = [0.0; 3];
                let add = [c.off[0], c.off[1], c.level];
                for k in 0..3 {
                    rgb[k] = (p[k] * c.gain * c.tint[k] + add[k]) * c.mul[k];
                    if c.invert != 0 {
                        rgb[k] = 1.0 - rgb[k];
                    }
                }
                [rgb[0], rgb[1], rgb[2], p[3] * c.mul[3]]
            })
            .collect();
        expect_close(&format!("{name} color case {i}"), &out, &want, 2.0);
    }
}

#[test]
fn color_filter_with_scalar_vector_and_flag_params() {
    color_case(&warp(), "warp");
}

#[test]
fn color_filter_on_the_hardware_device() {
    let Some(r) = hardware() else {
        eprintln!("no hardware D3D11 device: skipped");
        return;
    };
    eprintln!("hardware adapter: {}", r.gpu.adapter_name);
    color_case(&r, "hardware");
}

/// Every scalar of `members`, written into a one-row picture and read back: a wrong offset anywhere shows as a wrong
/// number at that component.
fn probe_source(accessors: &[String]) -> String {
    let mut s = String::from("fn pick(i: u32) -> f32 {\n  switch i {\n");
    for (i, a) in accessors.iter().enumerate() {
        s.push_str(&format!("    case {i}u: {{ return {a}; }}\n"));
    }
    s.push_str("    default: { return 0.0; }\n  }\n}\n\n");
    s.push_str(
        "@fragment\nfn main(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {\n  return vec4<f32>(pick(u32(pos.x)) / 255.0, 0.0, 0.0, 1.0);\n}\n",
    );
    s
}

fn code(i: usize) -> u32 {
    1 + ((i * 7 + 5) % 250) as u32
}

/// Declared params of the layout probe: every kind of member, in an order that makes WGSL and HLSL's own packing differ.
fn probe_decls() -> Vec<(&'static str, &'static str)> {
    vec![
        ("a", "f32"),
        ("b", "vec2<f32>"),
        ("c", "f32"),
        ("d", "vec3<f32>"),
        ("e", "f32"),
        ("f", "vec3<f32>"),
        ("g", "vec2<f32>"),
        ("h", "i32"),
        ("k", "u32"),
        ("m3", "mat3x3<f32>"),
        ("m2", "mat2x2<f32>"),
        ("m32", "mat3x2<f32>"),
        ("m23", "mat2x3<f32>"),
        ("m44", "mat4x4<f32>"),
        ("arr", "array<vec4<f32>, 3>"),
        ("v4i", "vec4<i32>"),
        ("tail", "f32"),
    ]
}

/// (accessors in WGSL, values by member name) with a distinct code per component.
fn probe_values() -> (Vec<String>, Vec<(&'static str, Value)>) {
    let mut acc: Vec<String> = Vec::new();
    let next = |acc: &mut Vec<String>, a: String| -> usize {
        acc.push(a);
        acc.len() - 1
    };
    let mut values: Vec<(&'static str, Value)> = Vec::new();
    let f = |i: usize| Value::F32(code(i) as f32);

    let i = next(&mut acc, "params.a".into());
    values.push(("a", f(i)));
    let (x, y) = (next(&mut acc, "params.b.x".into()), next(&mut acc, "params.b.y".into()));
    values.push(("b", Value::list([f(x), f(y)])));
    let i = next(&mut acc, "params.c".into());
    values.push(("c", f(i)));
    let ids: Vec<usize> = ["x", "y", "z"].iter().map(|c| next(&mut acc, format!("params.d.{c}"))).collect();
    values.push(("d", Value::list(ids.iter().map(|i| f(*i)))));
    let i = next(&mut acc, "params.e".into());
    values.push(("e", f(i)));
    let ids: Vec<usize> = ["x", "y", "z"].iter().map(|c| next(&mut acc, format!("params.f.{c}"))).collect();
    values.push(("f", Value::list(ids.iter().map(|i| f(*i)))));
    let ids: Vec<usize> = ["x", "y"].iter().map(|c| next(&mut acc, format!("params.g.{c}"))).collect();
    values.push(("g", Value::list(ids.iter().map(|i| f(*i)))));
    let i = next(&mut acc, "f32(params.h)".into());
    values.push(("h", Value::I32(code(i) as i32)));
    let i = next(&mut acc, "f32(params.k)".into());
    values.push(("k", Value::U32(code(i))));
    for (name, cols, rows) in [("m3", 3, 3), ("m2", 2, 2), ("m32", 3, 2), ("m23", 2, 3), ("m44", 4, 4)] {
        let mut columns = Vec::new();
        for c in 0..cols {
            let mut col = Vec::new();
            for r in 0..rows {
                let i = next(&mut acc, format!("params.{name}[{c}][{r}]"));
                col.push(f(i));
            }
            columns.push(Value::List(col));
        }
        values.push((name, Value::List(columns)));
    }
    let mut elems = Vec::new();
    for e in 0..3 {
        let ids: Vec<usize> = ["x", "y", "z", "w"].iter().map(|c| next(&mut acc, format!("params.arr[{e}].{c}"))).collect();
        elems.push(Value::list(ids.iter().map(|i| f(*i))));
    }
    values.push(("arr", Value::List(elems)));
    let ids: Vec<usize> = ["x", "y", "z", "w"].iter().map(|c| next(&mut acc, format!("f32(params.v4i.{c})"))).collect();
    values.push(("v4i", Value::list(ids.iter().map(|i| Value::I32(code(*i) as i32)))));
    let i = next(&mut acc, "params.tail".into());
    values.push(("tail", f(i)));
    (acc, values)
}

fn read_probe(r: &Arc<Renderer>, t: &Translated, n: usize, values: &[(&str, Value)]) -> Vec<u8> {
    let (w, h) = (n as u32, 1u32);
    let px = vec![[0u8; 4]; n];
    run(r, t, w, h, &px, &frame(w, h), values, &[]).iter().map(|p| p[0]).collect()
}

#[test]
fn every_uniform_component_arrives_where_the_shader_reads_it() {
    let r = warp();
    let (acc, values) = probe_values();
    let src = probe_source(&acc);
    let t = tr(&src, &probe_decls(), &[]);
    let got = read_probe(&r, &t, acc.len(), &values);
    for (i, g) in got.iter().enumerate() {
        assert_eq!(*g as u32, code(i), "component {i} ({}) arrived as {g}", acc[i]);
    }
    eprintln!("layout probe: {} components, all correct; offsets:", acc.len());
    for m in &t.params.as_ref().unwrap().members {
        eprintln!("  {:>5} @ {:>3}  {}", m.name, m.offset, m.ty.wgsl());
    }
    // the members that differ from what HLSL would choose by itself
    let by = |n: &str| t.params.as_ref().unwrap().members.iter().find(|m| m.name == n).unwrap().offset;
    assert_eq!(by("b"), 8, "vec2 after f32 is 8-aligned in WGSL (HLSL alone: 4)");
    assert_eq!(by("d"), 32, "vec3 after f32 is 16-aligned in WGSL (HLSL alone: 20)");
    assert_eq!(by("e"), 44, "f32 after vec3 fills the gap in both");
}

#[test]
fn offsets_match_what_fxc_compiled() {
    // Compile the probe and read the cbuffer back out of the bytecode with D3DReflect: FXC's own offsets must be
    // the ones naga reflected.
    let (acc, _) = probe_values();
    let t = tr(&probe_source(&acc), &probe_decls(), &[]);
    let code = compile_ps("probe", &t.hlsl).expect("compiles");
    let (size, members) = fxc_cbuffer(&code, "params");
    let layout = t.params.as_ref().unwrap();
    let mut all: Vec<_> = members.iter().collect();
    all.sort_by_key(|(_, o)| **o);
    eprintln!("FXC members: {all:?}");
    if let Some(start) = t.hlsl.find("struct Params") {
        eprintln!("{}", &t.hlsl[start..start + 900.min(t.hlsl.len() - start)]);
    }
    assert_eq!(size as usize, layout.buffer_size(), "cbuffer size");
    let mut compared = 0;
    for m in &layout.members {
        // naga appends `_` to names that end in a digit (`m3` -> `m3_`) and writes a `matCx2` as one float2 per column
        // (`m2__0`, `m2__1`); the first column sits at the member's offset.
        let candidates = [m.name.clone(), format!("{}_", m.name), format!("{}__0", m.name), format!("{}_0", m.name)];
        let found = candidates.iter().find_map(|c| members.get(c));
        let off = found.unwrap_or_else(|| panic!("FXC reflection has no member for `{}`", m.name));
        assert_eq!(*off, m.offset, "member {}", m.name);
        compared += 1;
    }
    eprintln!("FXC reflection agrees on all {compared} members; cbuffer {size} bytes");
    assert_eq!(compared, layout.members.len());
}

/// (size, member name -> offset) of the cbuffer `name` holding one struct variable of the same name.
fn fxc_cbuffer(code: &[u8], name: &str) -> (u32, HashMap<String, u32>) {
    // SAFETY: plain reflection calls on bytecode FXC just produced; the descriptor structs are locals.
    unsafe {
        let mut raw: *mut std::ffi::c_void = std::ptr::null_mut();
        D3DReflect(code.as_ptr() as *const _, code.len(), &ID3D11ShaderReflection::IID, &mut raw).expect("reflect");
        let refl: ID3D11ShaderReflection = ID3D11ShaderReflection::from_raw(raw);
        let cname = std::ffi::CString::new(name).unwrap();
        let cb = refl.GetConstantBufferByName(PCSTR(cname.as_ptr() as *const u8)).expect("cbuffer by name");
        let mut desc = D3D11_SHADER_BUFFER_DESC::default();
        cb.GetDesc(&mut desc).expect("cbuffer desc");
        let var = cb.GetVariableByIndex(0).expect("the struct variable");
        let mut vdesc = D3D11_SHADER_VARIABLE_DESC::default();
        var.GetDesc(&mut vdesc).expect("variable desc");
        let ty = var.GetType().expect("variable type");
        let mut tdesc = D3D11_SHADER_TYPE_DESC::default();
        ty.GetDesc(&mut tdesc).expect("type desc");
        let mut members = HashMap::new();
        for i in 0..tdesc.Members {
            let mty = ty.GetMemberTypeByIndex(i).expect("member type");
            let mname = ty.GetMemberTypeName(i);
            let mut md = D3D11_SHADER_TYPE_DESC::default();
            mty.GetDesc(&mut md).expect("member desc");
            members.insert(mname.to_string().unwrap_or_default(), md.Offset);
        }
        (desc.Size, members)
    }
}

#[test]
fn nested_structs_and_arrays_of_structs_follow_the_wgsl_layout() {
    // A complete module with its own Params (the form the prelude reference copy has), including the cases that
    // differ most from HLSL: a struct member (needs @align(16) in WGSL's uniform rules), arrays of structs.
    let full = "\
struct Frame { size: vec2<f32>, inv_size: vec2<f32>, canvas: vec2<f32>, time: f32, delta: f32, frame: u32, seed: f32, _pad: vec2<f32> };
struct Inner { q: vec2<f32>, r: f32 };
struct Params { head: f32, @align(16) n: Inner, mid: f32, @align(16) list: array<Inner, 2>, tail: vec3<f32> };
@group(0) @binding(0) var<uniform> frame: Frame;
@group(0) @binding(1) var<uniform> params: Params;
@group(0) @binding(2) var image: texture_2d<f32>;
@group(0) @binding(8) var smp: sampler;
fn pick(i: u32) -> f32 {
  switch i {
    case 0u: { return params.head; }
    case 1u: { return params.n.q.x; }
    case 2u: { return params.n.q.y; }
    case 3u: { return params.n.r; }
    case 4u: { return params.mid; }
    case 5u: { return params.list[0].q.x; }
    case 6u: { return params.list[0].q.y; }
    case 7u: { return params.list[0].r; }
    case 8u: { return params.list[1].q.x; }
    case 9u: { return params.list[1].q.y; }
    case 10u: { return params.list[1].r; }
    case 11u: { return params.tail.x; }
    case 12u: { return params.tail.y; }
    case 13u: { return params.tail.z; }
    default: { return 0.0; }
  }
}
@fragment
fn main(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
  return vec4<f32>(pick(u32(pos.x)) / 255.0, 0.0, 0.0, 1.0) + textureSampleLevel(image, smp, vec2<f32>(0.5, 0.5), 0.0) * 0.0;
}
";
    let params = decls(&[
        ("head", "f32"),
        ("n", "Inner"),
        ("mid", "f32"),
        ("list", "array<Inner,2>"),
        ("tail", "vec3<f32>"),
    ]);
    let t = translate_module(full, &params).unwrap_or_else(|e| panic!("{}", e.render("m.wgsl")));
    let layout = t.params.as_ref().unwrap();
    let off: Vec<(&str, u32)> = layout.members.iter().map(|m| (m.name.as_str(), m.offset)).collect();
    eprintln!("nested layout: {off:?}, size {}", layout.size);
    assert_eq!(off[1], ("n", 16), "a struct member sits on a 16-byte boundary");
    assert_eq!(off[2], ("mid", 32));
    assert_eq!(off[3], ("list", 48), "an array of structs starts 16-aligned");

    let inner = |q: [usize; 2], r: usize| {
        Value::structure([
            ("q", Value::list(q.map(|i| Value::F32(code(i) as f32)))),
            ("r", Value::F32(code(r) as f32)),
        ])
    };
    let values: Vec<(&str, Value)> = vec![
        ("head", Value::F32(code(0) as f32)),
        ("n", inner([1, 2], 3)),
        ("mid", Value::F32(code(4) as f32)),
        ("list", Value::list([inner([5, 6], 7), inner([8, 9], 10)])),
        ("tail", Value::list([11, 12, 13].map(|i| Value::F32(code(i) as f32)))),
    ];
    let r = warp();
    let got = read_probe(&r, &t, 14, &values);
    for (i, g) in got.iter().enumerate() {
        assert_eq!(*g as u32, code(i), "component {i} arrived as {g}");
    }
    let bc = compile_ps("nested", &t.hlsl).unwrap();
    let (size, _) = fxc_cbuffer(&bc, "params");
    assert_eq!(size as usize, layout.buffer_size(), "FXC and naga agree on the cbuffer size");
}

#[test]
fn packing_rejects_wrong_values() {
    let t = tr(COLOR, &COLOR_PARAMS, &[]);
    let l = t.params.as_ref().unwrap();
    let ok = [
        ("gain", Value::F32(1.0)),
        ("tint", Value::vec([1.0, 1.0, 1.0])),
        ("level", Value::F32(0.0)),
        ("off", Value::vec([0.0, 0.0])),
        ("mul", Value::vec([1.0; 4])),
        ("invert", Value::U32(0)),
    ];
    assert_eq!(l.pack(&ok).unwrap().len(), 80);
    let mut bad = ok.to_vec();
    bad[1].1 = Value::vec([1.0, 1.0]);
    assert!(l.pack(&bad).unwrap_err().to_string().contains("tint"));
    let mut bad = ok.to_vec();
    bad[5].1 = Value::F32(1.0);
    assert!(l.pack(&bad).unwrap_err().to_string().contains("invert"));
    assert!(l.pack(&ok[..5]).unwrap_err().to_string().contains("no value for member `invert`"));
    let mut bad = ok.to_vec();
    bad.push(("nope", Value::F32(1.0)));
    assert!(l.pack(&bad).unwrap_err().to_string().contains("no member `nope`"));
}

// ---------------------------------------------------------------------------------------------------------------
// 3. Coordinates and resolution

#[test]
fn pixel_coordinates_start_top_left_with_y_down_and_pixel_centres() {
    let r = warp();
    let t = tr(UVDUMP, &[], &[]);
    let (w, h) = (32u32, 16u32);
    let px = vec![[0u8; 4]; (w * h) as usize];
    let out = run(&r, &t, w, h, &px, &frame(w, h), &[], &[]);
    let want: Vec<[f32; 4]> = (0..h)
        .flat_map(|y| {
            (0..w).map(move |x| [(x as f32 + 0.5) / w as f32, (y as f32 + 0.5) / h as f32, 0.5, 1.0])
        })
        .collect();
    expect_close("uv dump", &out, &want, 1.0);
    // explicit corners: top-left is the smallest uv, the bottom-right the largest, x grows right, y grows down
    assert!(out[0][0] < out[1][0], "x grows to the right");
    assert!(out[0][1] < out[w as usize][1], "y grows downwards");
    assert!(out[0][0] <= 5 && out[0][1] <= 9 && out[0][0] > 0);
}

#[test]
fn vignette_and_pixelate_use_resolution_and_the_same_origin_as_the_engine() {
    let r = warp();
    let t = tr(VIGNETTE, &[("cell", "f32"), ("inner", "f32"), ("outer", "f32"), ("amount", "f32")], &[]);
    let (w, h) = (32u32, 16u32);
    let px = picture(w, h);
    let img = floats(&px);
    for (cell, inner, outer, amount) in [(1.0f32, 0.25f32, 0.7f32, 0.8f32), (4.0, 0.1, 0.6, 1.0), (8.0, 0.3, 0.9, 0.5)] {
        let out = run(
            &r,
            &t,
            w,
            h,
            &px,
            &frame(w, h),
            &[("cell", cell.into()), ("inner", inner.into()), ("outer", outer.into()), ("amount", amount.into())],
            &[],
        );
        let mut want = Vec::new();
        for y in 0..h {
            for x in 0..w {
                let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
                let (u, v) = (fx / w as f32, fy / h as f32);
                let sx = (fx / cell).floor() * cell;
                let sy = (fy / cell).floor() * cell;
                let c = img[(sy as u32 * w + sx as u32) as usize];
                let d = ((u - 0.5).powi(2) + (v - 0.5).powi(2)).sqrt();
                let vig = 1.0 - smoothstep(inner, outer, d) * amount;
                want.push([c[0] * vig, c[1] * vig, c[2] * vig, c[3]]);
            }
        }
        expect_close(&format!("vignette cell {cell}"), &out, &want, 2.0);
    }
}

// ---------------------------------------------------------------------------------------------------------------
// 4. Time

#[test]
fn animated_filter_follows_the_time_and_frame_values() {
    let r = warp();
    let t = tr(ANIMATED, &[("speed", "f32")], &[]);
    let (w, h) = (16u32, 4u32);
    let px = vec![[0u8; 4]; (w * h) as usize];
    let mut outs = Vec::new();
    for (time, n, seed) in [(0.0f32, 0u32, 0.25f32), (0.4, 7, 0.75), (2.5, 100, 0.0)] {
        let f = FrameValues { time, frame: n, seed, ..frame(w, h) };
        let out = run(&r, &t, w, h, &px, &f, &[("speed", 3.0f32.into())], &[]);
        let want: Vec<[f32; 4]> = (0..h)
            .flat_map(|_| {
                (0..w).map(move |x| {
                    let u = (x as f32 + 0.5) / w as f32;
                    [0.5 + 0.5 * (time * 3.0 + u * std::f32::consts::TAU).sin(), n as f32 / 255.0, seed, 1.0]
                })
            })
            .collect();
        expect_close(&format!("animated t={time}"), &out, &want, 2.0);
        outs.push(out);
    }
    assert_ne!(outs[0], outs[1], "two times give two pictures");
    assert_ne!(outs[1], outs[2]);
}

// ---------------------------------------------------------------------------------------------------------------
// 5. Loops, branches, built-ins

#[test]
fn loops_branches_and_builtins_match_the_cpu() {
    let r = warp();
    let t = tr(BUILTINS, &[("radius", "i32")], &[]);
    let (w, h) = (32u32, 16u32);
    let px = picture(w, h);
    let img = floats(&px);
    for radius in [0i32, 2, 5] {
        let out = run(&r, &t, w, h, &px, &frame(w, h), &[("radius", Value::I32(radius))], &[]);
        let mut want = Vec::new();
        for y in 0..h {
            for x in 0..w {
                let (u, v) = ((x as f32 + 0.5) / w as f32, (y as f32 + 0.5) / h as f32);
                let explicit = bilinear(&img, w, h, u, v);
                let (mut acc, mut total) = ([0.0f32; 3], 0.0f32);
                for i in -radius..=radius {
                    let wt = 1.0 - (i as f32).abs() * 0.2;
                    let s = bilinear(&img, w, h, u + i as f32 * 1.5 / w as f32, v);
                    for k in 0..3 {
                        acc[k] += s[k] * wt;
                    }
                    total += wt;
                }
                let mut rgb = acc.map(|a| a / total);
                if u > 0.5 {
                    rgb = rgb.map(|c| c + (1.0 - c - c) * 0.5);
                } else {
                    rgb = rgb.map(|c| c.powf(0.8));
                }
                let s = smoothstep(0.2, 0.8, rgb[1]);
                rgb[0] = (rgb[0] + 0.1 * (v * 20.0).sin()).clamp(0.0, 1.0);
                rgb[2] = fract(rgb[2] * 3.0);
                rgb[1] = s;
                want.push([rgb[0], rgb[1], rgb[2], explicit[3]]);
            }
        }
        // `fract` is a cliff: ignore pixels whose argument is within 0.01 of an integer on either side
        let mut ignored = 0;
        let mut worst_d = 0.0f32;
        for (i, (g, e)) in out.iter().zip(&want).enumerate() {
            let (x, y) = (i as u32 % w, i as u32 / w);
            let _ = (x, y);
            for k in 0..4 {
                let d = (g[k] as f32 - e[k].clamp(0.0, 1.0) * 255.0).abs();
                if d > 3.0 && k == 2 {
                    ignored += 1;
                    continue;
                }
                worst_d = worst_d.max(d);
            }
        }
        eprintln!("builtins radius {radius}: worst {worst_d:.2} code values, {ignored} fract-cliff channel(s) ignored");
        assert!(worst_d <= 3.0, "radius {radius}: worst difference {worst_d}");
        assert!(ignored <= 6, "radius {radius}: too many cliff pixels ({ignored}); the picture is not a fair test");
    }
}

#[test]
fn implicit_derivatives_outside_uniform_control_flow_are_not_stopped_by_naga_and_fxc_decides() {
    // WebGPU's rule (derivatives only in uniform control flow) is not enforced as an error by naga 30.0.2; the
    // result is whatever FXC does with it. Recorded, not hidden: the doc says so and authors are told to sample
    // before branching.
    let cases = [
        (
            "textureSample after a branch on the pixel position",
            format!("{ENTRY}  var c = vec4<f32>(0.0);\n  if (pos.x > 3.0) {{\n    c = textureSample(image, smp, pos.xy * frame.inv_size);\n  }}\n  return c;\n}}\n"),
        ),
        (
            "dpdx after a branch",
            format!("{ENTRY}  if (pos.x > 3.0) {{\n    return vec4<f32>(dpdx(pos.x));\n  }}\n  return vec4<f32>(0.0);\n}}\n"),
        ),
        (
            "textureSample inside a loop with a data-dependent exit",
            format!("{ENTRY}  var c = vec4<f32>(0.0);\n  for (var i = 0; i < 4; i++) {{\n    c = c + textureSample(image, smp, pos.xy * frame.inv_size);\n    if (c.r > 2.0) {{ break; }}\n  }}\n  return c;\n}}\n"),
        ),
    ];
    for (name, src) in cases {
        let t = translate(&src, &[], &[]);
        match t {
            Err(e) => eprintln!("[{name}] naga refuses: {}", e.render("shader.wgsl")),
            Ok(t) => match compile_ps("divergent", &t.hlsl) {
                Ok(_) => eprintln!("[{name}] naga and FXC both accept it"),
                Err(e) => {
                    eprintln!("[{name}] FXC refuses: {}", e.message.lines().next().unwrap_or(""));
                    assert_eq!(e.kind, ErrorKind::Fxc);
                }
            },
        }
    }
}

#[test]
fn derivatives_work_in_uniform_control_flow() {
    let r = warp();
    let t = tr(DERIVATIVES, &[], &[]);
    let (w, h) = (32u32, 16u32);
    let px = vec![[0u8; 4]; (w * h) as usize];
    let out = run(&r, &t, w, h, &px, &frame(w, h), &[], &[]);
    // one pixel of uv per pixel: every derivative is 1 pixel, written as 0.5 (code 128)
    let want = vec![[0.5, 0.5, 0.5, 1.0]; (w * h) as usize];
    expect_close("dpdx/dpdy/fwidth", &out, &want, 1.5);
}

// ---------------------------------------------------------------------------------------------------------------
// 6. The worked example

const VHS_PARAMS: [(&str, &str); 6] = [
    ("scanlines", "f32"),
    ("bleed", "f32"),
    ("noise", "f32"),
    ("glitches", "u32"),
    ("rate", "f32"),
    ("glitch", "f32"),
];

fn vhs_values(scanlines: f32, bleed: f32, noise: f32, glitch: f32) -> Vec<(&'static str, Value)> {
    vec![
        ("scanlines", scanlines.into()),
        ("bleed", bleed.into()),
        ("noise", noise.into()),
        ("glitches", 1u32.into()),
        ("rate", 6.0f32.into()),
        ("glitch", glitch.into()),
    ]
}

#[test]
fn the_vhs_example_from_the_design_runs_as_written() {
    let r = warp();
    let t = tr(VHS, &VHS_PARAMS, &[]);
    let (w, h) = (32u32, 16u32);
    let px = picture(w, h);
    let img = floats(&px);

    // Noise and glitch off: everything left is deterministic maths (bleed, scanlines).
    let f = FrameValues { time: 1.25, seed: 0.4, ..frame(w, h) };
    for (scanlines, bleed) in [(0.4f32, 2.0f32), (1.0, 3.5), (0.0, 0.0)] {
        let out = run(&r, &t, w, h, &px, &f, &vhs_values(scanlines, bleed, 0.0, 0.0), &[]);
        let mut want = Vec::new();
        for y in 0..h {
            for x in 0..w {
                let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
                let (u, v) = (fx / w as f32, fy / h as f32);
                let dx = bleed / w as f32;
                let base = bilinear(&img, w, h, u, v);
                let red = bilinear(&img, w, h, u + dx, v)[0];
                let blue = bilinear(&img, w, h, u - dx, v)[2];
                let line = 0.5 + 0.5 * (fy * std::f32::consts::PI).sin();
                let k = 1.0 - scanlines * 0.35 * line;
                want.push([red * k, base[1] * k, blue * k, base[3]]);
            }
        }
        expect_close(&format!("vhs scanlines {scanlines} bleed {bleed}"), &out, &want, 2.0);
    }

    // Glitch on: rows outside the bands are untouched; rows inside shift (the jitter comes from `hash`, which is
    // not bit-comparable across implementations: sin() of thousands, times 43758).
    let in_band_at = |time: f32, y: u32| fract((y as f32 + 0.5) / h as f32 * 7.0 + time * 3.0) >= 0.97;
    // a time at which at least one row of this small picture is inside a band
    let band_time = (0..200)
        .map(|i| i as f32 * 0.05)
        .find(|t| (0..h).any(|y| in_band_at(*t, y)))
        .expect("some time has a row in a band");
    let fb = FrameValues { time: band_time, ..f };
    let calm = run(&r, &t, w, h, &px, &fb, &vhs_values(0.4, 2.0, 0.0, 0.0), &[]);
    let glitched = run(&r, &t, w, h, &px, &fb, &vhs_values(0.4, 2.0, 0.0, 20.0), &[]);
    let mut in_band = 0;
    let mut changed_in_band = 0;
    for y in 0..h {
        let same = (0..w).all(|x| calm[(y * w + x) as usize] == glitched[(y * w + x) as usize]);
        if in_band_at(band_time, y) {
            in_band += 1;
            changed_in_band += usize::from(!same);
        } else {
            assert!(same, "row {y} is outside the glitch bands and must not change");
        }
    }
    eprintln!("vhs glitch at t={band_time}: {in_band} of {h} rows in a band, {changed_in_band} of them changed");
    assert!(changed_in_band >= 1, "a glitch must move the pixels of a band");

    // Noise on: bounded by its amplitude (noise * 0.25 * 0.5 of full scale = 0.0625 * 255 = 16 codes at noise 0.5),
    // averages out, and differs between two seeds.
    let n1 = run(&r, &t, w, h, &px, &FrameValues { seed: 0.1, ..f }, &vhs_values(0.4, 2.0, 0.5, 0.0), &[]);
    let n2 = run(&r, &t, w, h, &px, &FrameValues { seed: 0.9, ..f }, &vhs_values(0.4, 2.0, 0.5, 0.0), &[]);
    assert_ne!(n1, n2, "the grain changes with the seed");
    let mut sum = 0.0f64;
    let mut count = 0.0f64;
    for (a, c) in n1.iter().zip(&calm) {
        for k in 0..3 {
            let d = a[k] as f32 - c[k] as f32;
            // clamping at 0 and 255 trims the grain; only unclamped pixels count
            if c[k] > 20 && c[k] < 235 {
                assert!(d.abs() <= 16.5 + 1.0, "grain {d} exceeds its amplitude");
                sum += d as f64;
                count += 1.0;
            }
        }
    }
    let mean = sum / count;
    eprintln!("vhs grain: mean {mean:.2} codes over {count} samples");
    assert!(mean.abs() < 3.0, "grain should average out, mean {mean}");
}

// ---------------------------------------------------------------------------------------------------------------
// 7. Extra inputs

#[test]
fn history_and_a_path_image_arrive_as_extra_inputs() {
    let r = warp();
    let t = tr(TWO_INPUTS, &[("k", "f32")], &["stamp"]);
    let regs: Vec<String> = t.textures.iter().map(|s| s.to_string()).collect();
    eprintln!("textures: {regs:?}");
    assert_eq!(regs, ["image @binding(2) -> t0", "history @binding(3) -> t1", "stamp @binding(4) -> t2"]);

    let (w, h) = (32u32, 16u32);
    let px = picture(w, h);
    // history: the picture upside down; stamp: a smaller 4x2 image (sampled with the linear clamp sampler)
    let hist: Vec<Pixel> = (0..h).flat_map(|y| (0..w).map(move |x| (x, h - 1 - y))).map(|(x, y)| px[(y * w + x) as usize]).collect();
    let stamp: Vec<Pixel> = (0..8).map(|i| [(i * 30) as u8, (255 - i * 30) as u8, 100, 255]).collect();
    let out = run(
        &r,
        &t,
        w,
        h,
        &px,
        &frame(w, h),
        &[("k", 2.0f32.into())],
        &[("history", (w, h, hist.clone())), ("stamp", (4, 2, stamp.clone()))],
    );
    let (a, b, s) = (floats(&px), floats(&hist), floats(&stamp));
    let mut want = Vec::new();
    for y in 0..h {
        for x in 0..w {
            let (u, v) = ((x as f32 + 0.5) / w as f32, (y as f32 + 0.5) / h as f32);
            let st = bilinear(&s, 4, 2, u, v);
            let i = (y * w + x) as usize;
            want.push([
                a[i][0] * 0.5 + b[i][0] * 0.25 + st[0] * 0.25 * 2.0,
                a[i][1] * 0.5 + b[i][1] * 0.25 + st[1] * 0.25 * 2.0,
                a[i][2] * 0.5 + b[i][2] * 0.25 + st[2] * 0.25 * 2.0,
                a[i][3],
            ]);
        }
    }
    expect_close("history + stamp", &out, &want, 2.0);
}

// ---------------------------------------------------------------------------------------------------------------
// 8. Error paths

fn assert_source_error(e: &ExtError, what: &str) {
    assert!(!e.message.is_empty(), "{what}: empty message");
    eprintln!("[{what}] {}", e.render("shader.wgsl"));
}

const ENTRY: &str = "@fragment\nfn main(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {\n";

#[test]
fn a_syntax_error_reports_line_and_column_in_the_authors_file() {
    let src = format!("{ENTRY}  let x = ;\n  return vec4<f32>(1.0);\n}}\n");
    let e = err(&src, &[]);
    assert_source_error(&e, "syntax");
    assert_eq!(e.kind, ErrorKind::Source);
    assert_eq!(e.line, Some(3), "{e:?}");
    assert_eq!(e.column, Some(11), "{e:?}");
    // an error in the first line of the file is line 1, whatever the prelude holds
    let e = err("this is not wgsl", &[]);
    assert_eq!(e.line, Some(1), "{e:?}");
    // and unknown identifiers
    let e = err(&format!("{ENTRY}  return vec4<f32>(nothing_here);\n}}\n"), &[]);
    assert_eq!((e.line, e.column), (Some(3), Some(20)), "{e:?}");
    assert!(e.message.contains("nothing_here") || e.message.contains("no definition"), "{e:?}");
}

#[test]
fn a_type_error_is_reported_with_its_line() {
    let src = format!("{ENTRY}  var a: f32 = 1.0;\n  a = vec2<f32>(1.0, 2.0);\n  return vec4<f32>(a);\n}}\n");
    let e = err(&src, &[]);
    assert_source_error(&e, "type error");
    assert!(e.line.is_some() && e.line.unwrap() <= 5, "{e:?}");
    let e = err(&format!("{ENTRY}  return vec3<f32>(1.0);\n}}\n"), &[]);
    assert_source_error(&e, "wrong return type");
    assert!(e.line.is_some(), "{e:?}");
    // a use of a manifest param with the wrong type
    let e = err(
        &format!("{ENTRY}  let v: vec2<f32> = params.gain;\n  return vec4<f32>(v, 0.0, 1.0);\n}}\n"),
        &[("gain", "f32")],
    );
    assert_source_error(&e, "param type use");
    assert_eq!(e.line, Some(3), "{e:?}");
}

#[test]
fn a_missing_or_wrong_entry_point_is_refused() {
    let e = err("fn helper() -> f32 { return 1.0; }\n", &[]);
    assert_source_error(&e, "no entry");
    assert!(e.message.contains("no entry point"), "{e:?}");

    let e = err("@fragment\nfn shade(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> { return vec4<f32>(1.0); }\n", &[]);
    assert_source_error(&e, "wrong name");
    assert!(e.message.contains("`main`"), "{e:?}");
    assert_eq!(e.line, Some(2), "{e:?}");

    let e = err("@fragment\nfn main(@location(0) uv: vec2<f32>) -> @location(0) vec4<f32> { return vec4<f32>(uv, 0.0, 1.0); }\n", &[]);
    assert_source_error(&e, "vertex input");
    assert!(e.message.contains("builtin(position)"), "{e:?}");

    let e = err("@fragment\nfn main(@builtin(position) pos: vec4<f32>) -> @location(1) vec4<f32> { return pos; }\n", &[]);
    assert_source_error(&e, "wrong target");
    assert!(e.message.contains("location(0)"), "{e:?}");
}

#[test]
fn a_compute_or_vertex_stage_is_not_a_filter() {
    let e = err("@compute @workgroup_size(1)\nfn main() {}\n", &[]);
    assert_source_error(&e, "compute");
    assert!(e.message.contains("Compute"), "{e:?}");
    assert_eq!(e.line, Some(2), "{e:?}");

    let e = err("@vertex\nfn main(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> { return vec4<f32>(0.0); }\n", &[]);
    assert_source_error(&e, "vertex");
    assert!(e.message.contains("Vertex"), "{e:?}");

    // a fragment shader next to a second one
    let e = err(&format!("{ENTRY}  return vec4<f32>(1.0);\n}}\n@fragment\nfn other() -> @location(0) vec4<f32> {{ return vec4<f32>(1.0); }}\n"), &[]);
    assert_source_error(&e, "two entry points");
    assert!(e.message.contains("exactly one"), "{e:?}");
}

#[test]
fn wrong_bindings_are_refused() {
    let body = "  return vec4<f32>(1.0);\n}\n";
    // the prelude already owns binding 2: a second variable there is a duplicate
    let e = err(&format!("@group(0) @binding(2) var extra: texture_2d<f32>;\n{ENTRY}{body}"), &[]);
    assert_source_error(&e, "duplicate binding");
    // an unknown uniform binding
    let e = err(&format!("@group(0) @binding(9) var<uniform> more: vec4<f32>;\n{ENTRY}  return more;\n}}\n"), &[]);
    assert_source_error(&e, "uniform 9");
    assert!(e.message.contains("@binding(9)") || e.message.contains("not available"), "{e:?}");
    // another group
    let e = err(&format!("@group(1) @binding(0) var other: texture_2d<f32>;\n{ENTRY}  return textureLoad(other, vec2<i32>(0), 0);\n}}\n"), &[]);
    assert_source_error(&e, "group 1");
    assert!(e.message.contains("@group(1)"), "{e:?}");
    assert_eq!(e.line, Some(1), "{e:?}");
    // a texture on a slot past the last one
    let e = err(&format!("@group(0) @binding(10) var late: texture_2d<f32>;\n{ENTRY}  return textureLoad(late, vec2<i32>(0), 0);\n}}\n"), &[]);
    assert_source_error(&e, "texture 10");
    // a texture where the convention has a uniform
    let full = "\
struct Frame { size: vec2<f32> };
@group(0) @binding(0) var frame: texture_2d<f32>;
@group(0) @binding(8) var smp: sampler;
@fragment
fn main(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> { return textureSampleLevel(frame, smp, vec2<f32>(0.5), 0.0); }
";
    let e = translate_module(full, &[]).unwrap_err();
    assert_source_error(&e, "texture at the frame slot");
}

#[test]
fn params_in_the_manifest_and_in_the_struct_must_agree_both_ways() {
    let head = "\
struct Frame { size: vec2<f32>, inv_size: vec2<f32>, canvas: vec2<f32>, time: f32, delta: f32, frame: u32, seed: f32, _pad: vec2<f32> };
struct Params { a: f32, b: vec2<f32> };
@group(0) @binding(0) var<uniform> frame: Frame;
@group(0) @binding(1) var<uniform> params: Params;
@group(0) @binding(8) var smp: sampler;
@fragment
fn main(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> { return vec4<f32>(params.a, params.b, 1.0); }
";
    assert!(translate_module(head, &decls(&[("a", "f32"), ("b", "vec2f")])).is_ok(), "aliases and spacing are fine");
    assert!(translate_module(head, &decls(&[("b", "vec2<f32>"), ("a", "f32")])).is_ok(), "order does not matter");

    let e = translate_module(head, &decls(&[("a", "f32"), ("b", "vec2<f32>"), ("c", "f32")])).unwrap_err();
    assert_source_error(&e, "declared in the manifest, absent from the struct");
    assert!(e.message.contains("declares parameter `c`"), "{e:?}");

    let e = translate_module(head, &decls(&[("a", "f32")])).unwrap_err();
    assert_source_error(&e, "in the struct, absent from the manifest");
    assert!(e.message.contains("member `b`") && e.message.contains("does not declare"), "{e:?}");

    let e = translate_module(head, &decls(&[("a", "i32"), ("b", "vec2<f32>")])).unwrap_err();
    assert_source_error(&e, "type differs");
    assert!(e.message.contains("`a`") && e.message.contains("i32"), "{e:?}");

    // params declared but the shader has no params variable at all
    let no_params = "\
@group(0) @binding(8) var smp: sampler;
@fragment
fn main(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> { return vec4<f32>(1.0); }
";
    let e = translate_module(no_params, &decls(&[("a", "f32")])).unwrap_err();
    assert_source_error(&e, "no params variable");

    // the prelude path: a type WGSL does not know is a manifest error that names the parameter
    let e = translate(&format!("{ENTRY}  return vec4<f32>(1.0);\n}}\n"), &decls(&[("x", "f32"), ("y", "banana")]), &[]).unwrap_err();
    assert_source_error(&e, "unknown param type");
    assert_eq!(e.kind, ErrorKind::Manifest);
    assert!(e.message.contains("parameter #2"), "{e:?}");
    // a bad name, a duplicate, a name that clashes with the built-ins
    for bad in [vec![("1x", "f32")], vec![("a", "f32"), ("a", "f32")], vec![("_hidden", "f32")], vec![("a b", "f32")]] {
        let e = translate("", &decls(&bad), &[]).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Manifest, "{bad:?}");
    }
    let e = translate("", &[], &["image"]).unwrap_err();
    assert_eq!(e.kind, ErrorKind::Manifest);
    // a frame struct with a member the host does not fill
    let frame_bad = "\
struct Frame { size: vec2<f32>, wobble: f32 };
@group(0) @binding(0) var<uniform> frame: Frame;
@group(0) @binding(8) var smp: sampler;
@fragment
fn main(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> { return vec4<f32>(frame.wobble); }
";
    let e = translate_module(frame_bad, &[]).unwrap_err();
    assert!(e.message.contains("wobble"), "{e:?}");
}

#[test]
fn unsupported_constructs_are_refused_with_a_message() {
    let cases: Vec<(&str, String)> = vec![
        ("storage buffer", format!("@group(0) @binding(9) var<storage, read> sb: array<f32>;\n{ENTRY}  return vec4<f32>(sb[0]);\n}}\n")),
        (
            "storage texture",
            format!("@group(0) @binding(9) var st: texture_storage_2d<rgba8unorm, write>;\n{ENTRY}  return vec4<f32>(1.0);\n}}\n"),
        ),
        ("3d texture", format!("@group(0) @binding(4) var vol: texture_3d<f32>;\n{ENTRY}  return textureLoad(vol, vec3<i32>(0), 0);\n}}\n")),
        ("depth texture", format!("@group(0) @binding(4) var dp: texture_depth_2d;\n{ENTRY}  return vec4<f32>(1.0);\n}}\n")),
        ("comparison sampler", format!("@group(0) @binding(9) var cs: sampler_comparison;\n{ENTRY}  return vec4<f32>(1.0);\n}}\n")),
        ("workgroup memory", format!("var<workgroup> shared_value: f32;\n{ENTRY}  return vec4<f32>(1.0);\n}}\n")),
        ("f16", format!("enable f16;\n{ENTRY}  return vec4<f32>(1.0);\n}}\n")),
        ("push constants", format!("var<push_constant> pc: vec4<f32>;\n{ENTRY}  return pc;\n}}\n")),
        ("recursion", format!("fn f(x: f32) -> f32 {{ return f(x); }}\n{ENTRY}  return vec4<f32>(f(1.0));\n}}\n")),
        ("atomic", format!("var<private> counter: atomic<u32>;\n{ENTRY}  return vec4<f32>(1.0);\n}}\n")),
        ("writing to a uniform", format!("{ENTRY}  frame.time = 1.0;\n  return vec4<f32>(1.0);\n}}\n")),
        ("unknown attribute", "@fragment @bogus\nfn main() -> @location(0) vec4<f32> { return vec4<f32>(1.0); }\n".to_string()),
        ("front_facing", "@fragment\nfn main(@builtin(front_facing) f: bool) -> @location(0) vec4<f32> { return vec4<f32>(1.0); }\n".to_string()),
        ("struct output", "struct O { @location(0) a: vec4<f32>, @location(1) b: vec4<f32> };\n@fragment\nfn main() -> O { return O(vec4<f32>(1.0), vec4<f32>(1.0)); }\n".to_string()),
    ];
    for (name, src) in cases {
        let e = err(&src, &[]);
        assert_source_error(&e, name);
        assert!(!matches!(e.kind, ErrorKind::Internal), "{name} must be a user error: {e:?}");
    }
}

#[test]
fn an_oversized_source_is_refused_before_any_work() {
    let big = format!("{ENTRY}{}  return vec4<f32>(1.0);\n}}\n", "  // padding padding padding padding padding\n".repeat(7000));
    assert!(big.len() > 256 * 1024);
    let t = Instant::now();
    let e = err(&big, &[]);
    assert_source_error(&e, "oversized");
    assert!(e.message.contains("limit"), "{e:?}");
    assert!(t.elapsed() < Duration::from_millis(500));
    assert!(translate_module(&big, &[]).is_err());
}

#[test]
fn hostile_inputs_return_errors_instead_of_panics_or_hangs() {
    let nul = format!("{ENTRY}  return vec4<f32>(1.0);\u{0}\n}}\n");
    let deep_parens = format!("{ENTRY}  return vec4<f32>({}1.0{});\n}}\n", "(".repeat(30_000), ")".repeat(30_000));
    let deep_blocks = format!("{ENTRY}{}  {}\n  return vec4<f32>(1.0);\n}}\n", "  {\n".repeat(20_000), "}\n".repeat(20_000));
    let deep_unary = format!("{ENTRY}  let a = {}1.0;\n  return vec4<f32>(a);\n}}\n", "-".repeat(100_000));
    let long_ident = format!("{ENTRY}  let {} = 1.0;\n  return vec4<f32>(1.0);\n}}\n", "x".repeat(200_000));
    let nonsense: String = (0..60_000u32).map(|i| char::from_u32(0x20 + i % 95).unwrap()).collect();
    let many_fns: String = (0..4000).map(|i| format!("fn helper_{i}(x: f32) -> f32 {{ return x + {i}.0; }}\n")).collect();
    let long_sum = format!("{ENTRY}  let a = 0.5;\n  let b = {};\n  return vec4<f32>(b);\n}}\n", vec!["a"; 40_000].join(" + "));
    for (name, src) in [
        ("nul byte", nul),
        ("30000 nested parentheses", deep_parens),
        ("20000 nested blocks", deep_blocks),
        ("100000 unary minus", deep_unary),
        ("200 KB identifier", long_ident),
        ("printable noise", nonsense),
        ("4000 functions", format!("{many_fns}{ENTRY}  return vec4<f32>(helper_3999(1.0));\n}}\n")),
        ("sum of 40000 terms", long_sum),
        ("sum of 125000 terms without spaces (the most 256 KB hold)", format!("{ENTRY}  let a = pos.x;\n  let b = {};\n  return vec4<f32>(b);\n}}\n", vec!["a"; 125_000].join("+"))),
    ] {
        let start = Instant::now();
        let result = translate(&src, &[], &[]);
        let took = start.elapsed();
        match &result {
            Ok(t) => eprintln!("[{name}] translated in {took:?} ({} bytes of HLSL)", t.hlsl.len()),
            Err(e) => eprintln!("[{name}] refused in {took:?}: {}", e.render("shader.wgsl").chars().take(140).collect::<String>()),
        }
        assert!(took < Duration::from_secs(12), "{name} took {took:?}");
        if let Err(e) = &result {
            assert!(!e.message.is_empty());
        }
    }
}

#[test]
fn fxc_stress_shaders_finish_or_fail_in_bounded_time() {
    // Translation succeeds for all of these; what is measured is FXC. Nothing here is run on the GPU.
    let straight: String = (0..5000).map(|i| format!("  acc = acc + sin(acc * {i}.37 + {i}.1) * 0.0001;\n")).collect();
    let cases: Vec<(&str, String)> = vec![
        (
            "5000 straight-line statements",
            format!("{ENTRY}  var acc = vec3<f32>(0.5);\n{straight}  return vec4<f32>(acc, 1.0);\n}}\n"),
        ),
        (
            "nested loops 64 x 64 x 64",
            format!("{ENTRY}  var acc = 0.0;\n  for (var i = 0; i < 64; i++) {{ for (var j = 0; j < 64; j++) {{ for (var k = 0; k < 64; k++) {{ acc = acc + sin(f32(i * j + k)); }} }} }}\n  return vec4<f32>(acc);\n}}\n"),
        ),
        (
            "loop of 100000 iterations",
            format!("{ENTRY}  var acc = 0.0;\n  for (var i = 0; i < 100000; i++) {{ acc = acc + sin(f32(i)); }}\n  return vec4<f32>(acc);\n}}\n"),
        ),
        (
            "loop with a uniform bound (params)",
            format!("{ENTRY}  var acc = 0.0;\n  for (var i = 0; i < i32(params.n); i++) {{ acc = acc + sin(f32(i)); }}\n  return vec4<f32>(acc);\n}}\n"),
        ),
        (
            "nested loops 32 x 32 with 4 texture samples each",
            format!("{ENTRY}  var acc = vec4<f32>(0.0);\n  for (var i = 0; i < 32; i++) {{ for (var j = 0; j < 32; j++) {{ let o = vec2<f32>(f32(i), f32(j)) * 0.001; acc = acc + tex(image, o) + tex(image, o * 2.0) + tex(image, o * 3.0) + tex(history, o * 4.0); }} }}\n  return acc;\n}}\n"),
        ),
        (
            "chain of 11 helpers, each calling the previous twice (2048 samples)",
            {
                let mut s = String::from("fn blur0(uv: vec2<f32>) -> vec4<f32> { return tex(image, uv); }\n");
                for i in 1..11 {
                    s.push_str(&format!(
                        "fn blur{i}(uv: vec2<f32>) -> vec4<f32> {{ return (blur{}(uv + vec2<f32>(0.001, 0.0)) + blur{}(uv - vec2<f32>(0.001, 0.0))) * 0.5; }}\n",
                        i - 1,
                        i - 1
                    ));
                }
                format!("{s}{ENTRY}  return blur10(pos.xy * frame.inv_size);\n}}\n")
            },
        ),
        (
            "expression chain of 125000 terms (the most 256 KB hold)",
            format!("{ENTRY}  let a = pos.x;\n  let b = {};\n  return vec4<f32>(b);\n}}\n", vec!["a"; 125_000].join("+")),
        ),
        (
            "expression chain of 5000 terms of sin()",
            format!("{ENTRY}  let a = pos.x;\n  let b = {};\n  return vec4<f32>(b);\n}}\n", vec!["sin(a)"; 5_000].join("+")),
        ),
        (
            "infinite loop (compiled, never run)",
            format!("{ENTRY}  var acc = 0.0;\n  loop {{ acc = acc + 1.0; }}\n  return vec4<f32>(acc);\n}}\n"),
        ),
        (
            "while(true) with a break on a texture value",
            format!("{ENTRY}  var i = 0.0;\n  loop {{ if (tex(image, vec2<f32>(i, 0.5)).r > 2.0) {{ break; }} i = i + 0.001; }}\n  return vec4<f32>(i);\n}}\n"),
        ),
    ];
    for (name, src) in cases {
        let t0 = Instant::now();
        let translated = match translate(&src, &decls(&[("n", "f32")]), &[]) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("[{name}] translation refused in {:?}: {}", t0.elapsed(), e.message);
                continue;
            }
        };
        let t_translate = t0.elapsed();
        if name.starts_with("infinite") {
            let bound = translated.hlsl.lines().filter(|l| l.contains("loop_bound")).take(3).collect::<Vec<_>>();
            eprintln!("[{name}] naga's loop bounding in the HLSL: {bound:?}");
        }
        let t1 = Instant::now();
        let r = compile_ps("stress", &translated.hlsl);
        let t_compile = t1.elapsed();
        match &r {
            Ok(b) => eprintln!("[{name}] translate {t_translate:?}, FXC {t_compile:?}, {} bytes of bytecode", b.len()),
            Err(e) => eprintln!("[{name}] translate {t_translate:?}, FXC {t_compile:?}: {}", e.message.lines().find(|l| !l.trim().is_empty()).unwrap_or("")),
        }
        assert!(t_compile < Duration::from_secs(40), "{name}: FXC took {t_compile:?}");
        if let Err(e) = &r {
            assert!(matches!(e.kind, ErrorKind::Fxc | ErrorKind::Internal));
        }
    }
}


#[test]
fn exponential_inlining_is_refused_before_fxc_sees_it() {
    // 30 helpers, each calling the previous one twice: 2^29 copies once FXC inlines. FXC does not return from this.
    let mut s = String::from("fn blur0(uv: vec2<f32>) -> vec4<f32> { return tex(image, uv); }\n");
    for i in 1..30 {
        s.push_str(&format!(
            "fn blur{i}(uv: vec2<f32>) -> vec4<f32> {{ return (blur{}(uv + vec2<f32>(0.001, 0.0)) + blur{}(uv - vec2<f32>(0.001, 0.0))) * 0.5; }}\n",
            i - 1,
            i - 1
        ));
    }
    let src = format!("{s}{ENTRY}  return blur29(pos.xy * frame.inv_size);\n}}\n");
    let t = Instant::now();
    let e = err(&src, &[]);
    assert_source_error(&e, "inlining bomb");
    assert!(e.message.contains("inlined"), "{e:?}");
    assert!(t.elapsed() < Duration::from_secs(1));
    // a long but linear program is fine
    let ok: String = (0..1000).map(|i| format!("  acc = acc + sin(acc * {i}.5);\n")).collect();
    assert!(translate(&format!("{ENTRY}  var acc = vec3<f32>(0.5);\n{ok}  return vec4<f32>(acc, 1.0);\n}}\n"), &[], &[]).is_ok());
}

#[test]
fn a_job_that_outlives_its_timeout_is_given_up_on_and_counted() {
    use super::{run_bounded, stuck_workers};
    let before = stuck_workers();
    let e = run_bounded("sleeper", Duration::from_millis(100), || {
        std::thread::sleep(Duration::from_millis(600));
        Ok(())
    })
    .unwrap_err();
    assert_eq!(e.kind, ErrorKind::Internal);
    assert!(e.message.contains("did not finish"), "{e:?}");
    assert_eq!(stuck_workers(), before + 1, "counted while it still runs");
    std::thread::sleep(Duration::from_millis(900));
    assert_eq!(stuck_workers(), before, "and forgotten once it ends");
    // a panic is an error, not a crash
    let e = run_bounded::<()>("panicker", Duration::from_secs(5), || panic!("boom")).unwrap_err();
    assert_eq!(e.kind, ErrorKind::Internal);
    assert!(e.message.contains("boom"), "{e:?}");
}
// ---------------------------------------------------------------------------------------------------------------
// 9. Measurements and the corpus

#[test]
fn measurements_and_the_corpus() {
    // Every example: translate, compile, report. Doubles as the corpus test the design asks for ("pin naga, keep a
    // corpus"): a naga that writes HLSL FXC rejects fails here.
    let probe = {
        let (acc, _) = probe_values();
        probe_source(&acc)
    };
    type Example<'a> = (&'a str, String, Vec<(&'a str, &'a str)>, Vec<&'a str>);
    let examples: Vec<Example> = vec![
        ("identity", IDENTITY.into(), vec![], vec![]),
        ("color", COLOR.into(), COLOR_PARAMS.to_vec(), vec![]),
        ("uvdump", UVDUMP.into(), vec![], vec![]),
        ("vignette", VIGNETTE.into(), vec![("cell", "f32"), ("inner", "f32"), ("outer", "f32"), ("amount", "f32")], vec![]),
        ("animated", ANIMATED.into(), vec![("speed", "f32")], vec![]),
        ("builtins", BUILTINS.into(), vec![("radius", "i32")], vec![]),
        ("derivatives", DERIVATIVES.into(), vec![], vec![]),
        ("vhs", VHS.into(), VHS_PARAMS.to_vec(), vec![]),
        ("two_inputs", TWO_INPUTS.into(), vec![("k", "f32")], vec!["stamp"]),
        ("layout probe", probe, probe_decls(), vec![]),
    ];
    eprintln!(
        "{:<14} {:>10} {:>10} {:>10} {:>10} {:>9} {:>9}",
        "example", "xlate 1st", "xlate avg", "FXC 1st", "FXC avg", "WGSL B", "HLSL B"
    );
    for (name, src, params, images) in &examples {
        let d = decls(params);
        let t0 = Instant::now();
        let t = translate(src, &d, images).unwrap_or_else(|e| panic!("{name}: {}", e.render(name)));
        let first = t0.elapsed();
        let t0 = Instant::now();
        for _ in 0..5 {
            translate(src, &d, images).unwrap();
        }
        let avg = t0.elapsed() / 5;

        let t0 = Instant::now();
        let code = compile_ps(name, &t.hlsl).unwrap_or_else(|e| panic!("{name}: {e}"));
        let fxc_first = t0.elapsed();
        let t0 = Instant::now();
        for _ in 0..3 {
            compile_ps(name, &t.hlsl).unwrap();
        }
        let fxc_avg = t0.elapsed() / 3;
        assert!(!code.is_empty());
        eprintln!(
            "{:<14} {:>10.2?} {:>10.2?} {:>10.2?} {:>10.2?} {:>9} {:>9}",
            name,
            first,
            avg,
            fxc_first,
            fxc_avg,
            src.len(),
            t.hlsl.len()
        );
    }
}

#[test]
fn the_translated_hlsl_for_the_color_example_is_printed_for_the_record() {
    let t = tr(COLOR, &COLOR_PARAMS, &[]);
    eprintln!("{}", t.hlsl);
    assert!(t.hlsl.contains("SamplerState smp : register(s0);"));
    assert!(!t.hlsl.contains("space"));
    assert!(t.hlsl.contains("cbuffer frame : register(b0)"));
    assert!(t.hlsl.contains("cbuffer params : register(b1)"));
    assert!(t.hlsl.contains("Texture2D<float4> image : register(t0);"));
}

#[test]
fn naga_output_without_the_sampler_fix_is_rejected_by_ps_5_0() {
    // The reason `fix_samplers` exists, kept as a test: if a naga release stops needing it this fails and the fix can go.
    let t = tr(IDENTITY, &[], &[]);
    let raw = "\
Texture2D<float4> image : register(t0);
SamplerState nagaSamplerHeap[2048]: register(s0, space0);
StructuredBuffer<uint> nagaGroup0SamplerIndexArray : register(t0, space255);
static const SamplerState smp = nagaSamplerHeap[nagaGroup0SamplerIndexArray[8]];
float4 main(float4 pos : SV_Position) : SV_Target0 { return image.SampleLevel(smp, pos.xy, 0.0); }
";
    let e = compile_ps("raw", raw).unwrap_err();
    eprintln!("raw naga sampler HLSL on ps_5_0: {}", e.message.lines().next().unwrap_or(""));
    assert_eq!(e.kind, ErrorKind::Fxc);
    assert!(compile_ps("fixed", &t.hlsl).is_ok());
}
