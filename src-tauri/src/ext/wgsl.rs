//! WGSL to HLSL translation for extension filters (spike E1).
//!
//! ```text
//! author WGSL (entry point only) --prelude--> full WGSL --naga parse + validate--> module
//!     --convention check + reflection--> layouts --naga HLSL writer (SM 5.0)--> HLSL --sampler fix--> ps_5_0 source
//! ```
//!
//! # The convention (see `docs/SPIKE-EXT.md`)
//! Everything below is plain WebGPU-valid WGSL; the host only decides which `@group(0)` bindings exist and where
//! they land in Direct3D 11:
//!
//! | WGSL | meaning | HLSL register |
//! |---|---|---|
//! | `@binding(0) var<uniform> frame: Frame` | built-in values (size, time, ...) | `cbuffer b0` |
//! | `@binding(1) var<uniform> params: Params` | one member per manifest param | `cbuffer b1` |
//! | `@binding(2) var image: texture_2d<f32>` | the picture so far | `t0` |
//! | `@binding(3) var history: texture_2d<f32>` | the card's previous output | `t1` |
//! | `@binding(4..=7) var <key>: texture_2d<f32>` | user-picked images | `t2..=t5` |
//! | `@binding(8) var smp: sampler` | linear, clamp | `s0` |
//!
//! and one entry point, `@fragment fn main(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32>`.
//!
//! Translation never panics and is bounded in time: it runs on a worker thread, panics are caught there, and a
//! source over [`MAX_SOURCE_BYTES`] is refused before `naga` sees it.

use std::fmt;
use std::time::Duration;

use naga::back::hlsl;
use naga::valid::{Capabilities, ValidationFlags, Validator};
use naga::{
    ArraySize, FastHashMap, Handle, ImageClass, ImageDimension, Module, ScalarKind as NScalarKind, ShaderStage,
    TypeInner,
};

use super::layout::{Layout, Member, ScalarKind, UType, Value};
use super::{run_bounded, ExtError};

/// The longest shader source accepted (the manifest limit of `docs/EXTENSIONS.md`).
pub const MAX_SOURCE_BYTES: usize = 256 * 1024;
/// How long one translation may take before the caller gets an error back.
pub const TRANSLATE_TIMEOUT: Duration = Duration::from_secs(10);
/// The `@binding` of the `frame` uniform, the `params` uniform, the first and last image, and the sampler.
pub const BINDING_FRAME: u32 = 0;
pub const BINDING_PARAMS: u32 = 1;
pub const BINDING_IMAGE: u32 = 2;
pub const BINDING_HISTORY: u32 = 3;
pub const BINDING_PATH_FIRST: u32 = 4;
pub const BINDING_PATH_LAST: u32 = 7;
pub const BINDING_SAMPLER: u32 = 8;
/// The most `path` images one filter can read.
pub const MAX_PATH_IMAGES: usize = (BINDING_PATH_LAST - BINDING_PATH_FIRST + 1) as usize;

/// The members of the generated `Frame` struct: name and WGSL type. The host fills them with [`FrameValues`].
pub const FRAME_MEMBERS: [(&str, &str); 8] = [
    ("size", "vec2<f32>"),
    ("inv_size", "vec2<f32>"),
    ("canvas", "vec2<f32>"),
    ("time", "f32"),
    ("delta", "f32"),
    ("frame", "u32"),
    ("seed", "f32"),
    ("_pad", "vec2<f32>"),
];

/// One manifest parameter as the shader sees it: a member of `Params`.
#[derive(Clone, Debug, PartialEq)]
pub struct ParamDecl {
    /// A WGSL identifier (the manifest `key`).
    pub name: String,
    /// The WGSL type: `f32`, `i32`, `u32`, `vec2<f32>` (or `vec2f`), `vec3<f32>`, `vec4<f32>`, `mat3x3<f32>`,
    /// `array<vec4<f32>, 4>`, ...
    pub ty: String,
}

impl ParamDecl {
    pub fn new(name: &str, ty: &str) -> ParamDecl {
        ParamDecl {
            name: name.into(),
            ty: ty.into(),
        }
    }
}

/// What the host writes into `frame` each frame.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FrameValues {
    /// Pixels of the pass's target.
    pub size: [f32; 2],
    /// The virtual camera's output size.
    pub canvas: [f32; 2],
    pub time: f32,
    pub delta: f32,
    pub frame: u32,
    pub seed: f32,
}

impl FrameValues {
    fn values(&self) -> Vec<(&'static str, Value)> {
        let inv = |v: f32| if v > 0.0 { 1.0 / v } else { 0.0 };
        vec![
            ("size", Value::vec(self.size)),
            ("inv_size", Value::vec([inv(self.size[0]), inv(self.size[1])])),
            ("canvas", Value::vec(self.canvas)),
            ("time", self.time.into()),
            ("delta", self.delta.into()),
            ("frame", self.frame.into()),
            ("seed", self.seed.into()),
        ]
    }
}

/// A texture binding the shader declared and where the host must bind it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextureSlot {
    /// The WGSL variable name (`image`, `history`, or a `path` param key).
    pub name: String,
    pub binding: u32,
    /// The HLSL `t` register.
    pub register: u32,
}

/// The result of a translation.
#[derive(Clone, Debug)]
pub struct Translated {
    /// A `ps_5_0` pixel shader, entry point `main`.
    pub hlsl: String,
    /// The `frame` uniform (`cbuffer b0`), if the shader declares it.
    pub frame: Option<Layout>,
    /// The `params` uniform (`cbuffer b1`), if the shader declares it.
    pub params: Option<Layout>,
    pub textures: Vec<TextureSlot>,
    pub uses_sampler: bool,
}

impl Translated {
    /// The bytes for `cbuffer b0`, or `None` when the shader has no `frame`.
    pub fn pack_frame(&self, v: &FrameValues) -> Option<Result<Vec<u8>, super::layout::PackError>> {
        self.frame.as_ref().map(|l| l.pack(&v.values()))
    }

    /// The bytes for `cbuffer b1` from `values` (by member name), or `None` when the shader has no `params`.
    pub fn pack_params(&self, values: &[(&str, Value)]) -> Option<Result<Vec<u8>, super::layout::PackError>> {
        self.params.as_ref().map(|l| l.pack(values))
    }
}

// ---------------------------------------------------------------------------------------------------------------
// The prelude

/// The generated lines before the author's code, and the line (1-based) at which the author's code starts.
struct Prelude {
    text: String,
    author_line: u32,
    /// The first line of each param member in `text` (1-based), in declaration order.
    param_lines: Vec<u32>,
}

const PRELUDE_HEAD: &str = "\
// ---- generated by Better Cam, extension API 1 ----
struct Frame {
  size: vec2<f32>,
  inv_size: vec2<f32>,
  canvas: vec2<f32>,
  time: f32,
  delta: f32,
  frame: u32,
  seed: f32,
  _pad: vec2<f32>,
};
";

const PRELUDE_TAIL: &str = "\
@group(0) @binding(2) var image: texture_2d<f32>;
@group(0) @binding(3) var history: texture_2d<f32>;
@group(0) @binding(8) var smp: sampler;

fn tex(t: texture_2d<f32>, uv: vec2<f32>) -> vec4<f32> { return textureSampleLevel(t, smp, uv, 0.0); }
fn texel(t: texture_2d<f32>, p: vec2<i32>) -> vec4<f32> { return textureLoad(t, p, 0); }
fn to_linear(c: vec3<f32>) -> vec3<f32> {
  return select(pow((c + vec3<f32>(0.055)) / 1.055, vec3<f32>(2.4)), c / 12.92, c <= vec3<f32>(0.04045));
}
fn to_srgb(c: vec3<f32>) -> vec3<f32> {
  let x = clamp(c, vec3<f32>(0.0), vec3<f32>(1.0));
  return select(1.055 * pow(x, vec3<f32>(1.0 / 2.4)) - vec3<f32>(0.055), x * 12.92, x <= vec3<f32>(0.0031308));
}
fn luma(c: vec3<f32>) -> f32 { return dot(c, vec3<f32>(0.2126, 0.7152, 0.0722)); }
fn hash(p: vec2<f32>) -> f32 { return fract(sin(dot(p, vec2<f32>(12.9898, 78.233))) * 43758.5453); }
// ---- end of prelude ----
";

fn is_ident(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn prelude(params: &[ParamDecl], images: &[&str]) -> Result<Prelude, ExtError> {
    if images.len() > MAX_PATH_IMAGES {
        return Err(ExtError::manifest(format!(
            "{} image params, at most {MAX_PATH_IMAGES} are supported",
            images.len()
        )));
    }
    let mut seen: Vec<&str> = vec!["frame", "params", "image", "history", "smp"];
    for name in params.iter().map(|p| p.name.as_str()).chain(images.iter().copied()) {
        if !is_ident(name) || name.starts_with('_') {
            return Err(ExtError::manifest(format!("`{name}` is not a valid parameter name")));
        }
    }
    for name in images {
        if seen.contains(name) {
            return Err(ExtError::manifest(format!("image param `{name}` clashes with a built-in name")));
        }
        seen.push(name);
    }
    let mut seen_params: Vec<&str> = Vec::new();
    for p in params {
        if seen_params.contains(&p.name.as_str()) {
            return Err(ExtError::manifest(format!("parameter `{}` is declared twice", p.name)));
        }
        seen_params.push(&p.name);
    }

    let mut text = String::from(PRELUDE_HEAD);
    let mut line = text.lines().count() as u32;
    text.push_str("@group(0) @binding(0) var<uniform> frame: Frame;\n");
    line += 1;
    let mut param_lines = Vec::new();
    if !params.is_empty() {
        text.push_str("struct Params {\n");
        line += 1;
        for p in params {
            line += 1;
            param_lines.push(line);
            text.push_str(&format!("  {}: {},\n", p.name, p.ty));
        }
        text.push_str("};\n@group(0) @binding(1) var<uniform> params: Params;\n");
    }
    for (i, name) in images.iter().enumerate() {
        text.push_str(&format!(
            "@group(0) @binding({}) var {name}: texture_2d<f32>;\n",
            BINDING_PATH_FIRST + i as u32
        ));
    }
    text.push_str(PRELUDE_TAIL);
    Ok(Prelude {
        author_line: text.lines().count() as u32 + 1,
        text,
        param_lines,
    })
}

// ---------------------------------------------------------------------------------------------------------------
// Entry points

/// Translate an author's shader: the generated prelude (from the manifest's `params` and image `path` params) is put
/// in front of `source`, which holds the entry point and any helpers. Error lines count in `source`.
pub fn translate(source: &str, params: &[ParamDecl], images: &[&str]) -> Result<Translated, ExtError> {
    // Notepad and others save UTF-8 files with a byte order mark.
    let source = source.strip_prefix('\u{feff}').unwrap_or(source);
    if source.len() > MAX_SOURCE_BYTES {
        return Err(too_big(source.len()));
    }
    let pre = prelude(params, images)?;
    let full = format!("{}{}", pre.text, source);
    let params = params.to_vec();
    run_bounded("WGSL translation", TRANSLATE_TIMEOUT, move || {
        translate_full(&full, &params, Some(&pre))
    })
}

/// Translate a complete WGSL module that declares its own bindings and structs (the form of `.sdk/prelude.wgsl` plus a
/// shader). `params` is the manifest's list: the shader's `Params` struct must match it, both ways.
pub fn translate_module(full: &str, params: &[ParamDecl]) -> Result<Translated, ExtError> {
    let full = full.strip_prefix('\u{feff}').unwrap_or(full);
    if full.len() > MAX_SOURCE_BYTES {
        return Err(too_big(full.len()));
    }
    let full = full.to_string();
    let params = params.to_vec();
    run_bounded("WGSL translation", TRANSLATE_TIMEOUT, move || translate_full(&full, &params, None))
}

fn too_big(len: usize) -> ExtError {
    ExtError::source(format!(
        "the shader is {len} bytes, the limit is {MAX_SOURCE_BYTES} bytes"
    ))
}

// ---------------------------------------------------------------------------------------------------------------
// The work (runs on the worker thread)

fn translate_full(full: &str, params: &[ParamDecl], pre: Option<&Prelude>) -> Result<Translated, ExtError> {
    let at = Locator { source: full, pre };

    let module = naga::front::wgsl::parse_str(full).map_err(|e| {
        let (span, label) = e.labels().next().map(|(s, l)| (Some(s), l.to_string())).unwrap_or((None, String::new()));
        let mut message = e.message().to_string();
        if !label.is_empty() && label != message {
            message = format!("{message} ({label})");
        }
        at.error(span, message)
    })?;

    let info = Validator::new(ValidationFlags::all(), Capabilities::empty())
        .validate(&module)
        .map_err(|e| {
            let span = e.spans().next().map(|(s, _)| *s);
            at.error(span, error_chain(e.as_inner()))
        })?;

    let plan = check_convention(&module, &at, params)?;

    let mut options = hlsl::Options {
        shader_model: hlsl::ShaderModel::V5_0,
        fake_missing_bindings: false,
        ..Default::default()
    };
    for (res, register) in &plan.registers {
        options.binding_map.insert(
            *res,
            hlsl::BindTarget {
                space: 0,
                register: *register,
                ..Default::default()
            },
        );
    }
    // Without this entry `naga` panics ("Sampler buffer of group 0 not bound to a register") as soon as a shader
    // uses a sampler. The buffer it names is removed again by `fix_samplers`.
    options.sampler_buffer_binding_map.insert(
        hlsl::SamplerIndexBufferKey { group: 0 },
        hlsl::BindTarget {
            space: 255,
            register: 0,
            ..Default::default()
        },
    );
    let pipeline = hlsl::PipelineOptions {
        entry_point: Some((ShaderStage::Fragment, "main".to_string())),
    };
    let mut text = String::new();
    let reflection = hlsl::Writer::new(&mut text, &options, &pipeline)
        .write(&module, &info, None)
        .map_err(|e| ExtError::source(format!("this shader cannot be translated to HLSL: {e}")))?;
    match reflection.entry_point_names.first() {
        Some(Ok(name)) if name == "main" => {}
        other => {
            return Err(ExtError::internal(format!(
                "the translator named the entry point {other:?}, expected `main`"
            )))
        }
    }

    Ok(Translated {
        hlsl: fix_samplers(&text)?,
        frame: plan.frame,
        params: plan.params,
        textures: plan.textures,
        uses_sampler: plan.uses_sampler,
    })
}

/// Walks error sources into one line: "Function [1] 'main' is invalid: Expression [4] is invalid: ...".
fn error_chain(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut cur = e.source();
    while let Some(s) = cur {
        out.push_str(": ");
        out.push_str(&s.to_string());
        cur = s.source();
    }
    out
}

/// Turns spans into author line numbers.
struct Locator<'a> {
    source: &'a str,
    pre: Option<&'a Prelude>,
}

impl Locator<'_> {
    fn error(&self, span: Option<naga::Span>, message: String) -> ExtError {
        let Some(span) = span.filter(|s| s.is_defined()) else {
            return ExtError::source(message);
        };
        let loc = span.location(self.source);
        self.at_line(loc.line_number, loc.line_position, message)
    }

    fn at_line(&self, line: u32, column: u32, message: String) -> ExtError {
        match self.pre {
            None => ExtError::source_at(line, column, message),
            Some(p) if line >= p.author_line => ExtError::source_at(line - p.author_line + 1, column, message),
            Some(p) => {
                // A problem inside the generated lines: a parameter's type, or the prelude itself.
                let param = p
                    .param_lines
                    .iter()
                    .position(|l| *l == line)
                    .map(|i| format!("parameter #{} of the manifest: ", i + 1))
                    .unwrap_or_else(|| "in the generated prelude: ".into());
                ExtError::manifest(format!("{param}{message}"))
            }
        }
    }

    /// An error attached to the first place `needle` appears (used where `naga` keeps no span).
    fn error_at_text(&self, needle: &str, message: String) -> ExtError {
        match self.source.find(needle) {
            Some(pos) => {
                let before = &self.source[..pos];
                let line = before.matches('\n').count() as u32 + 1;
                let column = (pos - before.rfind('\n').map(|i| i + 1).unwrap_or(0)) as u32 + 1;
                self.at_line(line, column, message)
            }
            None => ExtError::source(message),
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// The convention

struct Plan {
    frame: Option<Layout>,
    params: Option<Layout>,
    textures: Vec<TextureSlot>,
    uses_sampler: bool,
    registers: Vec<(naga::ResourceBinding, u32)>,
}

fn check_convention(module: &Module, at: &Locator<'_>, params: &[ParamDecl]) -> Result<Plan, ExtError> {
    check_entry_point(module, at)?;
    check_cost(module, at)?;

    let mut plan = Plan {
        frame: None,
        params: None,
        textures: Vec::new(),
        uses_sampler: false,
        registers: Vec::new(),
    };
    for (handle, global) in module.global_variables.iter() {
        let name = global.name.clone().unwrap_or_default();
        let span = module.global_variables.get_span(handle);
        let err = |m: String| at.error(Some(span), m);
        if let Some(binding) = global.binding {
            // `naga` leaves duplicate bindings to the host API, and HLSL would silently let one register win.
            if plan.registers.iter().any(|(b, _)| *b == binding) {
                return Err(err(format!(
                    "`{name}`: @group({}) @binding({}) is declared twice (the generated prelude owns bindings {BINDING_FRAME} to {BINDING_SAMPLER})",
                    binding.group, binding.binding
                )));
            }
        }
        let Some(binding) = global.binding else {
            match global.space {
                naga::AddressSpace::Private => continue,
                other => {
                    return Err(err(format!(
                        "`{name}`: a variable in the {other:?} address space is not available to a filter"
                    )))
                }
            }
        };
        if binding.group != 0 {
            return Err(err(format!(
                "`{name}`: @group({}) is not available, filters use @group(0) only",
                binding.group
            )));
        }
        let inner = &module.types[global.ty].inner;
        match (global.space, binding.binding) {
            (naga::AddressSpace::Uniform, BINDING_FRAME) => {
                let layout = reflect_struct(module, global.ty).map_err(|m| err(format!("`{name}`: {m}")))?;
                check_frame(&layout).map_err(|m| err(format!("`{name}`: {m}")))?;
                plan.frame = Some(layout);
                plan.registers.push((binding, 0));
            }
            (naga::AddressSpace::Uniform, BINDING_PARAMS) => {
                let layout = reflect_struct(module, global.ty).map_err(|m| err(format!("`{name}`: {m}")))?;
                check_params(&layout, params).map_err(|m| err(format!("`{name}`: {m}")))?;
                plan.params = Some(layout);
                plan.registers.push((binding, 1));
            }
            (naga::AddressSpace::Uniform, n) => {
                return Err(err(format!(
                    "`{name}`: uniform @binding({n}) is not available; use @binding({BINDING_FRAME}) for `frame` and @binding({BINDING_PARAMS}) for `params`"
                )))
            }
            (naga::AddressSpace::Storage { .. }, _) => {
                return Err(err(format!("`{name}`: storage buffers are not available to a filter")))
            }
            (naga::AddressSpace::Handle, n) => match inner {
                TypeInner::Image {
                    dim: ImageDimension::D2,
                    arrayed: false,
                    class: ImageClass::Sampled {
                        kind: NScalarKind::Float,
                        multi: false,
                    },
                } if (BINDING_IMAGE..=BINDING_PATH_LAST).contains(&n) => {
                    let register = n - BINDING_IMAGE;
                    plan.textures.push(TextureSlot {
                        name,
                        binding: n,
                        register,
                    });
                    plan.registers.push((binding, register));
                }
                TypeInner::Image { .. } => {
                    return Err(err(format!(
                        "`{name}`: only texture_2d<f32> at @binding({BINDING_IMAGE}) to @binding({BINDING_PATH_LAST}) is available (no storage, depth, multisampled, array, 1D, 3D or cube textures)"
                    )))
                }
                TypeInner::Sampler { comparison: false } if n == BINDING_SAMPLER => {
                    plan.uses_sampler = true;
                    plan.registers.push((binding, n));
                }
                TypeInner::Sampler { .. } => {
                    return Err(err(format!(
                        "`{name}`: the one sampler is `var smp: sampler` at @binding({BINDING_SAMPLER}); comparison samplers are not available"
                    )))
                }
                _ => return Err(err(format!("`{name}`: this resource type is not available to a filter"))),
            },
            (other, _) => return Err(err(format!("`{name}`: address space {other:?} is not available to a filter"))),
        }
    }
    if let Some(layout) = &plan.params {
        // already compared in check_params; the reverse case (no struct at all) is handled below
        let _ = layout;
    } else if !params.is_empty() {
        return Err(ExtError::manifest(format!(
            "the manifest declares parameters ({}) but the shader has no `@group(0) @binding({BINDING_PARAMS}) var<uniform> params`",
            params.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", ")
        )));
    }
    plan.textures.sort_by_key(|t| t.binding);
    Ok(plan)
}

/// The most statements a shader may have once every call is inlined (HLSL has no calls: FXC inlines all of them).
/// A chain of 30 helpers that each call the previous one twice is 30 lines of WGSL and 2^29 statements for FXC, which
/// does not come back (measured: still running after 30 s). 20 000 is about 4 s of FXC on the machine of the spike.
pub const MAX_INLINED_STATEMENTS: u64 = 20_000;

fn block_cost(block: &naga::Block, costs: &FastHashMap<Handle<naga::Function>, u64>) -> u64 {
    let mut total = 0u64;
    for statement in block.iter() {
        let here = match statement {
            naga::Statement::Emit(_) => 0,
            naga::Statement::Block(inner) => block_cost(inner, costs),
            naga::Statement::If { accept, reject, .. } => 1u64
                .saturating_add(block_cost(accept, costs))
                .saturating_add(block_cost(reject, costs)),
            naga::Statement::Switch { cases, .. } => cases
                .iter()
                .fold(1u64, |acc, c| acc.saturating_add(block_cost(&c.body, costs))),
            naga::Statement::Loop { body, continuing, .. } => 1u64
                .saturating_add(block_cost(body, costs))
                .saturating_add(block_cost(continuing, costs)),
            naga::Statement::Call { function, .. } => 1u64.saturating_add(costs.get(function).copied().unwrap_or(0)),
            _ => 1,
        };
        total = total.saturating_add(here);
    }
    total
}

/// Refuses shaders whose calls expand beyond [`MAX_INLINED_STATEMENTS`].
fn check_cost(module: &Module, at: &Locator<'_>) -> Result<(), ExtError> {
    let mut costs: FastHashMap<Handle<naga::Function>, u64> = FastHashMap::default();
    // `naga` guarantees a function appears after everything it calls and forbids recursion.
    for (handle, f) in module.functions.iter() {
        costs.insert(handle, block_cost(&f.body, &costs));
    }
    for ep in &module.entry_points {
        let n = block_cost(&ep.function.body, &costs);
        if n > MAX_INLINED_STATEMENTS {
            let shown = if n == u64::MAX { "an enormous number of".to_string() } else { n.to_string() };
            return Err(at.error_at_text(
                &format!("fn {}", ep.name),
                format!(
                    "the shader is too large once its function calls are inlined ({shown} statements, the limit is {MAX_INLINED_STATEMENTS}); a function that calls another one several times multiplies the work"
                ),
            ));
        }
    }
    Ok(())
}

fn check_entry_point(module: &Module, at: &Locator<'_>) -> Result<(), ExtError> {
    let mut fragments = Vec::new();
    for ep in &module.entry_points {
        if ep.stage != ShaderStage::Fragment {
            return Err(at.error_at_text(
                &format!("fn {}", ep.name),
                format!(
                    "`{}` is a {:?} entry point; a filter is one @fragment shader (no vertex, compute or other stages)",
                    ep.name, ep.stage
                ),
            ));
        }
        fragments.push(ep);
    }
    let ep = match fragments.as_slice() {
        [] => {
            return Err(ExtError::source(
                "no entry point: write `@fragment fn main(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32>`"
                    .to_string(),
            ))
        }
        [one] => *one,
        [_, second, ..] => {
            return Err(at.error_at_text(
                &format!("fn {}", second.name),
                "a filter has exactly one @fragment entry point".into(),
            ))
        }
    };
    let here = |m: String| at.error_at_text(&format!("fn {}", ep.name), m);
    if ep.name != "main" {
        return Err(here(format!("the entry point must be named `main`, not `{}`", ep.name)));
    }
    let f = &ep.function;
    let is_vec4f = |ty: Handle<naga::Type>| {
        matches!(
            module.types[ty].inner,
            TypeInner::Vector {
                size: naga::VectorSize::Quad,
                scalar: naga::Scalar {
                    kind: NScalarKind::Float,
                    width: 4
                }
            }
        )
    };
    match f.arguments.as_slice() {
        [] => {}
        [a] if is_vec4f(a.ty) && matches!(a.binding, Some(naga::Binding::BuiltIn(naga::BuiltIn::Position { .. }))) => {}
        _ => {
            return Err(here(
                "the entry point takes nothing or only `@builtin(position) pos: vec4<f32>` (the vertex stage passes no other values)"
                    .into(),
            ))
        }
    }
    match &f.result {
        Some(r)
            if is_vec4f(r.ty)
                && matches!(r.binding, Some(naga::Binding::Location { location: 0, .. })) => {}
        _ => {
            return Err(here(
                "the entry point must return `@location(0) vec4<f32>` (the colour, straight alpha)".into(),
            ))
        }
    }
    Ok(())
}

fn check_frame(layout: &Layout) -> Result<(), String> {
    for m in &layout.members {
        match FRAME_MEMBERS.iter().find(|(n, _)| *n == m.name) {
            None => return Err(format!("struct {} has an unknown member `{}`; the host fills only: {}", layout.name, m.name, frame_names())),
            Some((_, ty)) if m.ty.wgsl() != *ty => {
                return Err(format!("member `{}` must be `{ty}`, it is `{}`", m.name, m.ty.wgsl()))
            }
            Some(_) => {}
        }
    }
    Ok(())
}

fn frame_names() -> String {
    FRAME_MEMBERS
        .iter()
        .filter(|(n, _)| !n.starts_with('_'))
        .map(|(n, _)| *n)
        .collect::<Vec<_>>()
        .join(", ")
}

/// WGSL spellings that mean the same type: `vec3f` is `vec3<f32>`, whitespace does not matter.
pub fn normalize_type(ty: &str) -> String {
    let mut out = String::new();
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut String| {
        if word.is_empty() {
            return;
        }
        let b = word.as_bytes();
        let expanded = match b {
            [b'v', b'e', b'c', n @ b'2'..=b'4', s @ (b'f' | b'i' | b'u')] => {
                format!("vec{}<{}32>", *n as char, *s as char)
            }
            [b'm', b'a', b't', c @ b'2'..=b'4', b'x', r @ b'2'..=b'4', b'f'] => {
                format!("mat{}x{}<f32>", *c as char, *r as char)
            }
            _ => word.clone(),
        };
        out.push_str(&expanded);
        word.clear();
    };
    for c in ty.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            word.push(c);
        } else {
            flush(&mut word, &mut out);
            if !c.is_whitespace() {
                out.push(c);
            }
        }
    }
    flush(&mut word, &mut out);
    out
}

fn check_params(layout: &Layout, decls: &[ParamDecl]) -> Result<(), String> {
    for d in decls {
        match layout.members.iter().find(|m| m.name == d.name) {
            None => {
                return Err(format!(
                    "the manifest declares parameter `{}` but struct {} has no such member",
                    d.name, layout.name
                ))
            }
            Some(m) if m.ty.wgsl() != normalize_type(&d.ty) => {
                return Err(format!(
                    "parameter `{}` is `{}` in the manifest but `{}` in struct {}",
                    d.name,
                    normalize_type(&d.ty),
                    m.ty.wgsl(),
                    layout.name
                ))
            }
            Some(_) => {}
        }
    }
    for m in &layout.members {
        if !decls.iter().any(|d| d.name == m.name) {
            return Err(format!(
                "struct {} has member `{}` that the manifest does not declare",
                layout.name, m.name
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------------
// Reflection

fn reflect_struct(module: &Module, ty: Handle<naga::Type>) -> Result<Layout, String> {
    match reflect(module, ty, 0)? {
        UType::Struct { name, size, members } => Ok(Layout { name, size, members }),
        other => Err(format!(
            "a uniform must be a struct (found {}); wrap it in `struct Name {{ ... }}`",
            other.wgsl()
        )),
    }
}

fn reflect(module: &Module, ty: Handle<naga::Type>, depth: u32) -> Result<UType, String> {
    if depth > 16 {
        return Err("uniform types nest too deeply".into());
    }
    let t = &module.types[ty];
    let scalar = |s: naga::Scalar| match (s.kind, s.width) {
        (NScalarKind::Float, 4) => Ok(ScalarKind::F32),
        (NScalarKind::Sint, 4) => Ok(ScalarKind::I32),
        (NScalarKind::Uint, 4) => Ok(ScalarKind::U32),
        other => Err(format!("{other:?} is not available in a filter's uniforms (use f32, i32 or u32)")),
    };
    Ok(match &t.inner {
        TypeInner::Scalar(s) => UType::Scalar(scalar(*s)?),
        TypeInner::Vector { size, scalar: s } => UType::Vector {
            n: *size as u8,
            kind: scalar(*s)?,
        },
        TypeInner::Matrix { columns, rows, scalar: s } => {
            if scalar(*s)? != ScalarKind::F32 {
                return Err("matrices are f32".into());
            }
            UType::Matrix {
                columns: *columns as u8,
                rows: *rows as u8,
            }
        }
        TypeInner::Array { base, size, stride } => {
            let ArraySize::Constant(n) = size else {
                return Err("uniform arrays need a constant length".into());
            };
            UType::Array {
                len: n.get(),
                stride: *stride,
                elem: Box::new(reflect(module, *base, depth + 1)?),
            }
        }
        TypeInner::Struct { members, span } => {
            let mut out = Vec::new();
            for m in members {
                out.push(Member {
                    name: m.name.clone().unwrap_or_default(),
                    offset: m.offset,
                    ty: reflect(module, m.ty, depth + 1)?,
                });
            }
            UType::Struct {
                name: t.name.clone().unwrap_or_else(|| "(anonymous)".into()),
                size: *span,
                members: out,
            }
        }
        other => return Err(format!("{other:?} cannot be in a uniform")),
    })
}

// ---------------------------------------------------------------------------------------------------------------
// The sampler fix

/// `naga` 30 writes every sampler as an index into a D3D12-style sampler heap (`register(s0, space0)`, a 2048-entry
/// array, a `StructuredBuffer` of indices in `space255`): Shader Model 5.1 syntax that `ps_5_0` rejects and D3D11 cannot
/// bind. The pass binds exactly one sampler at `s0`, so those lines are replaced by one `SamplerState` declaration.
/// If `naga` ever writes this differently the function fails instead of passing on HLSL it does not understand.
fn fix_samplers(text: &str) -> Result<String, ExtError> {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        if line.starts_with("SamplerState nagaSamplerHeap[")
            || line.starts_with("SamplerComparisonState nagaComparisonSamplerHeap[")
            || line.starts_with("StructuredBuffer<uint> naga")
        {
            continue;
        }
        if let Some(rest) = line.strip_prefix("static const SamplerState ") {
            let Some((name, init)) = rest.split_once(" = ") else {
                return Err(ExtError::internal(format!("unexpected sampler line: {line}")));
            };
            if !init.starts_with("nagaSamplerHeap[") {
                return Err(ExtError::internal(format!("unexpected sampler line: {line}")));
            }
            out.push_str(&format!("SamplerState {name} : register(s0);\n"));
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    for marker in ["nagaSamplerHeap", "nagaComparisonSamplerHeap", "SamplerIndexArray", "space"] {
        if out.contains(marker) {
            return Err(ExtError::internal(format!(
                "the translator wrote `{marker}` (Shader Model 5.1 syntax) in a place the sampler fix does not know"
            )));
        }
    }
    Ok(out)
}

impl fmt::Display for TextureSlot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} @binding({}) -> t{}", self.name, self.binding, self.register)
    }
}
