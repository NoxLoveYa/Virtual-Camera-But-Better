//! Extension manifests (`extension.json`, `docs/EXTENSIONS.md` 4.2): parsing and validation.
//!
//! [`Manifest::parse`] takes the JSON text and the name of the folder it was found in and either gives a checked
//! [`Manifest`] or an [`ExtError`] whose message starts with the field it is about (`params[2].min: must be a number`).
//! It touches no file; [`check_files`] looks at the files the manifest names.
//!
//! Unknown fields are ignored on purpose: additive changes keep `apiVersion` (4.1), so a manifest written for a newer
//! app that only adds a field still loads. A new `type` of param or a new `kind` is not additive and is refused.

use super::wgsl::ParamDecl;
use super::ExtError;
use serde::Serialize;
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

/// The extension API versions this build runs.
pub const SUPPORTED_API: &[u32] = &[1];
/// `extension.json`, shaders and scripts may each be this big.
pub const MAX_FILE_BYTES: u64 = 256 * 1024;
pub const MAX_PARAMS: usize = 32;
pub const MAX_PATH_PARAMS: usize = 4;
pub const MAX_UNIFORMS: usize = 16;
pub const MAX_ACTIONS: usize = 6;
pub const MAX_OPTIONS: usize = 64;
pub const MAX_TEXT_LENGTH: u32 = 200;
pub const DEFAULT_TEXT_LENGTH: u32 = 80;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ExtKind {
    Filter,
    Overlay,
    Widget,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RenderMode {
    Scene,
    Shader,
}

/// Types of the values a script computes for a shader (`uniforms`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum UniformType {
    #[serde(rename = "f32")]
    F32,
    #[serde(rename = "i32")]
    I32,
    #[serde(rename = "u32")]
    U32,
    #[serde(rename = "vec2f")]
    Vec2f,
    #[serde(rename = "vec4f")]
    Vec4f,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct UniformDef {
    pub key: String,
    #[serde(rename = "type")]
    pub ty: UniformType,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ActionDef {
    pub id: String,
    pub label: String,
}

/// `showIf`: the param is hidden unless the param `key` has one of `values` (same meaning as in `src/filters/schema.ts`).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ShowIf {
    pub key: String,
    #[serde(rename = "in")]
    pub values: Vec<Value>,
}

/// What every param has.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ParamBase {
    pub key: String,
    pub label: String,
    #[serde(rename = "showIf", skip_serializing_if = "Option::is_none")]
    pub show_if: Option<ShowIf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SelectOption {
    pub value: Value,
    pub label: String,
}

/// One entry of `params`. Serializes to the JSON `src/filters/schema.ts` reads (`ParamDef`), `text` included.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type")]
pub enum ParamDef {
    #[serde(rename = "slider")]
    Slider {
        #[serde(flatten)]
        base: ParamBase,
        min: f64,
        max: f64,
        step: f64,
        def: f64,
        #[serde(skip_serializing_if = "Option::is_none")]
        unit: Option<String>,
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        int: bool,
    },
    #[serde(rename = "select")]
    Select {
        #[serde(flatten)]
        base: ParamBase,
        def: Value,
        options: Vec<SelectOption>,
    },
    #[serde(rename = "bool")]
    Bool {
        #[serde(flatten)]
        base: ParamBase,
        def: bool,
    },
    #[serde(rename = "int")]
    Int {
        #[serde(flatten)]
        base: ParamBase,
        min: i64,
        max: i64,
        def: i64,
        #[serde(skip_serializing_if = "Option::is_none")]
        unit: Option<String>,
    },
    #[serde(rename = "color")]
    Color {
        #[serde(flatten)]
        base: ParamBase,
        def: u32,
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        alpha: bool,
    },
    #[serde(rename = "path")]
    Path {
        #[serde(flatten)]
        base: ParamBase,
        def: String,
        filter: String,
    },
    #[serde(rename = "combo")]
    Combo {
        #[serde(flatten)]
        base: ParamBase,
        def: String,
        options: Vec<String>,
        /// A regular expression (JavaScript syntax) typed values must match. The page compiles it; a pattern it cannot
        /// compile is ignored there.
        #[serde(skip_serializing_if = "Option::is_none")]
        pattern: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        placeholder: Option<String>,
    },
    #[serde(rename = "text")]
    Text {
        #[serde(flatten)]
        base: ParamBase,
        def: String,
        #[serde(rename = "maxLength")]
        max_length: u32,
    },
}

impl ParamDef {
    pub fn base(&self) -> &ParamBase {
        match self {
            ParamDef::Slider { base, .. }
            | ParamDef::Select { base, .. }
            | ParamDef::Bool { base, .. }
            | ParamDef::Int { base, .. }
            | ParamDef::Color { base, .. }
            | ParamDef::Path { base, .. }
            | ParamDef::Combo { base, .. }
            | ParamDef::Text { base, .. } => base,
        }
    }

    pub fn key(&self) -> &str {
        &self.base().key
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Manifest {
    pub api_version: u32,
    pub id: String,
    pub name: String,
    pub version: String,
    pub author: Option<String>,
    pub license: Option<String>,
    pub kind: ExtKind,
    pub hint: Option<String>,
    pub render: Option<RenderMode>,
    pub shader: Option<PathBuf>,
    pub script: Option<PathBuf>,
    /// Script tick rate; 0 = only on change (scene overlays) or the output rate (filter scripts).
    pub fps: f64,
    pub history: bool,
    pub params: Vec<ParamDef>,
    pub uniforms: Vec<UniformDef>,
    pub actions: Vec<ActionDef>,
}

impl Manifest {
    /// Parse and check `extension.json`. `folder` is the name of the folder the file was found in: the `id` must
    /// equal it.
    pub fn parse(text: &str, folder: &str) -> Result<Manifest, ExtError> {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let value: Value = serde_json::from_str(text)
            .map_err(|e| ExtError::manifest(format!("extension.json is not valid JSON (line {}, column {}): {e}", e.line(), e.column())))?;
        let Value::Object(root) = value else {
            return Err(ExtError::manifest("extension.json: must be a JSON object"));
        };
        build(&root, folder)
    }

    /// Every file the manifest names, relative to the extension folder.
    pub fn files(&self) -> Vec<&Path> {
        self.shader.iter().chain(self.script.iter()).map(|p| p.as_path()).collect()
    }

    /// The members of the shader's `Params` struct, in manifest order: one per param that reaches the shader, then
    /// one per manifest `uniforms` entry (`docs/EXTENSIONS.md` 4.3). Sliders are `f32`, ints `i32`, bools `u32`
    /// (0 or 1), selects `f32` when every option value is a number and `u32` (the index of the option) otherwise,
    /// colours `vec4<f32>` (straight RGBA, 0..1). `path`, `text` and `combo` params are not in `Params`: a `path` is a
    /// texture ([`Manifest::image_keys`]), the others only a script reads.
    pub fn param_decls(&self) -> Vec<ParamDecl> {
        let mut out = Vec::new();
        for p in &self.params {
            let ty = match p {
                ParamDef::Slider { .. } => "f32",
                ParamDef::Int { .. } => "i32",
                ParamDef::Bool { .. } => "u32",
                ParamDef::Select { options, .. } => {
                    if options.iter().all(|o| o.value.is_number()) {
                        "f32"
                    } else {
                        "u32"
                    }
                }
                ParamDef::Color { .. } => "vec4<f32>",
                ParamDef::Path { .. } | ParamDef::Text { .. } | ParamDef::Combo { .. } => continue,
            };
            out.push(ParamDecl::new(p.key(), ty));
        }
        for u in &self.uniforms {
            let ty = match u.ty {
                UniformType::F32 => "f32",
                UniformType::I32 => "i32",
                UniformType::U32 => "u32",
                UniformType::Vec2f => "vec2<f32>",
                UniformType::Vec4f => "vec4<f32>",
            };
            out.push(ParamDecl::new(&u.key, ty));
        }
        out
    }

    /// The keys of the `path` params in manifest order: the images of the shader, at `@binding(4..)`.
    pub fn image_keys(&self) -> Vec<&str> {
        self.params.iter().filter(|p| matches!(p, ParamDef::Path { .. })).map(|p| p.key()).collect()
    }
}

fn err(path: &str, message: impl std::fmt::Display) -> ExtError {
    ExtError::manifest(format!("{path}: {message}"))
}

fn at(base: &str, key: &str) -> String {
    if base.is_empty() {
        key.to_string()
    } else {
        format!("{base}.{key}")
    }
}

fn describe(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// A string field: `None` when absent, an error for another type, an empty string, or too many characters.
fn opt_str(obj: &Map<String, Value>, base: &str, key: &str, max_chars: usize) -> Result<Option<String>, ExtError> {
    let path = at(base, key);
    match obj.get(key) {
        None => Ok(None),
        Some(Value::String(s)) => {
            let s = s.trim();
            if s.is_empty() {
                return Err(err(&path, "must not be empty"));
            }
            if s.chars().count() > max_chars {
                return Err(err(&path, format!("must be at most {max_chars} characters")));
            }
            if s.chars().any(|c| c.is_control()) {
                return Err(err(&path, "must not contain control characters"));
            }
            Ok(Some(s.to_string()))
        }
        Some(other) => Err(err(&path, format!("must be a string, not {}", describe(other)))),
    }
}

fn req_str(obj: &Map<String, Value>, base: &str, key: &str, max_chars: usize) -> Result<String, ExtError> {
    opt_str(obj, base, key, max_chars)?.ok_or_else(|| err(&at(base, key), "is required"))
}

fn finite(obj: &Map<String, Value>, base: &str, key: &str) -> Result<f64, ExtError> {
    let path = at(base, key);
    match obj.get(key) {
        None => Err(err(&path, "is required")),
        Some(Value::Number(n)) => n.as_f64().filter(|f| f.is_finite()).ok_or_else(|| err(&path, "must be a finite number")),
        Some(other) => Err(err(&path, format!("must be a number, not {}", describe(other)))),
    }
}

/// A whole number, written with or without a `.0`.
fn as_integer(v: &Value) -> Option<i64> {
    let n = v.as_number()?;
    if let Some(i) = n.as_i64() {
        return Some(i);
    }
    let f = n.as_f64()?;
    (f.fract() == 0.0 && f.abs() <= 9_007_199_254_740_992.0).then_some(f as i64)
}

fn integer(obj: &Map<String, Value>, base: &str, key: &str) -> Result<i64, ExtError> {
    let path = at(base, key);
    match obj.get(key) {
        None => Err(err(&path, "is required")),
        Some(v) => as_integer(v).ok_or_else(|| err(&path, "must be a whole number")),
    }
}

fn opt_bool(obj: &Map<String, Value>, base: &str, key: &str) -> Result<Option<bool>, ExtError> {
    match obj.get(key) {
        None => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(other) => Err(err(&at(base, key), format!("must be true or false, not {}", describe(other)))),
    }
}

fn array<'a>(obj: &'a Map<String, Value>, base: &str, key: &str, max: usize) -> Result<Option<&'a Vec<Value>>, ExtError> {
    let path = at(base, key);
    match obj.get(key) {
        None => Ok(None),
        Some(Value::Array(a)) => {
            if a.len() > max {
                return Err(err(&path, format!("has {} entries; at most {max} are allowed", a.len())));
            }
            Ok(Some(a))
        }
        Some(other) => Err(err(&path, format!("must be an array, not {}", describe(other)))),
    }
}

fn object<'a>(v: &'a Value, path: &str) -> Result<&'a Map<String, Value>, ExtError> {
    v.as_object().ok_or_else(|| err(path, format!("must be an object, not {}", describe(v))))
}

/// `^[a-z][a-z0-9_]{0,max-1}$`: keys become WGSL member names, so they stay plain.
fn is_key(s: &str, max: usize) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some('a'..='z')) && s.len() <= max && chars.all(|c| matches!(c, 'a'..='z' | '0'..='9' | '_'))
}

/// `^[a-z0-9][a-z0-9-]{1,47}$`
fn is_id(s: &str) -> bool {
    let b = s.as_bytes();
    (2..=48).contains(&b.len())
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b[1..].iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

/// `MAJOR.MINOR.PATCH` with optional `-pre` and `+build` (semver 2, no leading zeros in the numbers).
fn is_semver(s: &str) -> bool {
    let (core, rest) = match s.find(['-', '+']) {
        Some(i) => (&s[..i], &s[i..]),
        None => (s, ""),
    };
    let nums: Vec<&str> = core.split('.').collect();
    let number = |n: &&str| !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit()) && (n.len() == 1 || !n.starts_with('0'));
    if nums.len() != 3 || !nums.iter().all(number) {
        return false;
    }
    let tail_ok = |t: &str| !t.is_empty() && t.split('.').all(|p| !p.is_empty() && p.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-'));
    match rest.strip_prefix('-') {
        Some(r) => {
            let (pre, build) = r.split_once('+').map_or((r, None), |(a, b)| (a, Some(b)));
            tail_ok(pre) && build.is_none_or(tail_ok)
        }
        None => match rest.strip_prefix('+') {
            Some(b) => tail_ok(b),
            None => rest.is_empty(),
        },
    }
}

/// A path inside the extension folder with one of `exts`. The checks are lexical; [`check_files`] resolves the file.
fn rel_path(obj: &Map<String, Value>, base: &str, key: &str, exts: &[&str]) -> Result<Option<PathBuf>, ExtError> {
    let path = at(base, key);
    let Some(text) = opt_str(obj, base, key, 120)? else {
        return Ok(None);
    };
    if text.contains(':') || text.starts_with('/') || text.starts_with('\\') {
        return Err(err(&path, "must be a path inside the extension folder (no drive letter, no leading slash)"));
    }
    let mut out = PathBuf::new();
    for part in text.split(['/', '\\']) {
        if part.is_empty() || part == "." || part == ".." {
            return Err(err(&path, "must be a path inside the extension folder (no empty, \".\" or \"..\" parts)"));
        }
        out.push(part);
    }
    let ext = out.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).unwrap_or_default();
    if !exts.contains(&ext.as_str()) {
        let list = exts.iter().map(|e| format!(".{e}")).collect::<Vec<_>>().join(", ");
        return Err(err(&path, format!("must be a {list} file")));
    }
    Ok(Some(out))
}

fn build(root: &Map<String, Value>, folder: &str) -> Result<Manifest, ExtError> {
    // apiVersion first: a manifest for a newer app may differ in everything else.
    let api = match root.get("apiVersion") {
        None => return Err(err("apiVersion", "is required")),
        Some(v) => as_integer(v).filter(|n| *n >= 1).ok_or_else(|| err("apiVersion", "must be a whole number, 1 or more"))?,
    };
    if !SUPPORTED_API.iter().any(|v| i64::from(*v) == api) {
        return Err(err("apiVersion", format!("Needs a newer Better Cam (extension API {api})")));
    }

    let id = req_str(root, "", "id", 48)?;
    if !is_id(&id) {
        return Err(err("id", "must be 2 to 48 characters: lowercase letters, digits and \"-\", not starting with \"-\""));
    }
    if id != folder {
        return Err(err("id", format!("must equal the folder name (\"{id}\" is in the folder \"{folder}\")")));
    }
    let name = req_str(root, "", "name", 48)?;
    let version = req_str(root, "", "version", 48)?;
    if !is_semver(&version) {
        return Err(err("version", "must be a semantic version such as 1.0.0"));
    }
    let author = opt_str(root, "", "author", 80)?;
    let license = opt_str(root, "", "license", 64)?;
    opt_str(root, "", "$schema", 200)?;
    let hint = opt_str(root, "", "hint", 80)?;
    let kind = match root.get("kind") {
        None => return Err(err("kind", "is required")),
        Some(Value::String(s)) => match s.as_str() {
            "filter" => ExtKind::Filter,
            "overlay" => ExtKind::Overlay,
            "widget" => ExtKind::Widget,
            other => return Err(err("kind", format!("must be \"filter\", \"overlay\" or \"widget\", not \"{other}\""))),
        },
        Some(other) => return Err(err("kind", format!("must be a string, not {}", describe(other)))),
    };

    match root.get("permissions") {
        None => {}
        Some(Value::Array(list)) => {
            if let Some((i, p)) = list.iter().enumerate().next() {
                let what = p.as_str().map_or_else(|| p.to_string(), str::to_string);
                return Err(err(&format!("permissions[{i}]"), format!("Needs permission \"{what}\", not supported")));
            }
        }
        Some(other) => return Err(err("permissions", format!("must be an array, not {}", describe(other)))),
    }

    let render = match root.get("render") {
        None => None,
        Some(Value::String(s)) if s == "scene" => Some(RenderMode::Scene),
        Some(Value::String(s)) if s == "shader" => Some(RenderMode::Shader),
        Some(_) => return Err(err("render", "must be \"scene\" or \"shader\"")),
    };
    let shader = rel_path(root, "", "shader", &["wgsl", "hlsl"])?;
    let script = rel_path(root, "", "script", &["ts", "js", "mjs"])?;
    let history = opt_bool(root, "", "history")?;

    // What each kind needs and may not have.
    match kind {
        ExtKind::Filter => {
            if render.is_some() {
                return Err(err("render", "is only for overlays and widgets"));
            }
            if shader.is_none() {
                return Err(err("shader", "is required for a filter"));
            }
        }
        ExtKind::Overlay | ExtKind::Widget => {
            if history.is_some() {
                return Err(err("history", "is only for filters"));
            }
            match (kind, render) {
                (_, None) => return Err(err("render", "is required for an overlay or widget: \"scene\" or \"shader\"")),
                (ExtKind::Widget, Some(RenderMode::Shader)) => {
                    return Err(err("render", "widgets support \"scene\" only in extension API 1"));
                }
                (_, Some(RenderMode::Scene)) => {
                    if script.is_none() {
                        return Err(err("script", "is required when render is \"scene\""));
                    }
                    if shader.is_some() {
                        return Err(err("shader", "is not used when render is \"scene\""));
                    }
                }
                (_, Some(RenderMode::Shader)) => {
                    if shader.is_none() {
                        return Err(err("shader", "is required when render is \"shader\""));
                    }
                }
            }
        }
    }

    let fps = match root.get("fps") {
        None => 0.0,
        Some(_) => {
            let f = finite(root, "", "fps")?;
            let max = if kind == ExtKind::Filter { 240.0 } else { 30.0 };
            let min = if kind == ExtKind::Filter { 1.0 } else { 0.0 };
            if f < min || f > max {
                return Err(err("fps", format!("must be between {min} and {max}")));
            }
            f
        }
    };

    let params = match array(root, "", "params", MAX_PARAMS)? {
        None => Vec::new(),
        Some(list) => list.iter().enumerate().map(|(i, p)| param(p, &format!("params[{i}]"))).collect::<Result<_, _>>()?,
    };
    check_param_set(&params)?;

    let uniforms = match array(root, "", "uniforms", MAX_UNIFORMS)? {
        None => Vec::new(),
        Some(list) => list.iter().enumerate().map(|(i, u)| uniform(u, &format!("uniforms[{i}]"))).collect::<Result<Vec<_>, _>>()?,
    };
    if !uniforms.is_empty() {
        if kind == ExtKind::Widget || render == Some(RenderMode::Scene) {
            return Err(err("uniforms", "are for shaders; a scene overlay or widget has none"));
        }
        if script.is_none() {
            return Err(err("uniforms", "need a \"script\" to compute them"));
        }
    }
    let mut keys: Vec<&str> = params.iter().map(|p| p.key()).collect();
    for (i, u) in uniforms.iter().enumerate() {
        if keys.contains(&u.key.as_str()) {
            return Err(err(&format!("uniforms[{i}].key"), format!("\"{}\" is already used by a param or an earlier uniform", u.key)));
        }
        keys.push(&u.key);
    }

    let actions = match array(root, "", "actions", MAX_ACTIONS)? {
        None => Vec::new(),
        Some(list) => list.iter().enumerate().map(|(i, a)| action(a, &format!("actions[{i}]"))).collect::<Result<Vec<_>, _>>()?,
    };
    if !actions.is_empty() && kind != ExtKind::Widget {
        return Err(err("actions", "are only for widgets"));
    }
    for (i, a) in actions.iter().enumerate() {
        if actions[..i].iter().any(|b| b.id == a.id) {
            return Err(err(&format!("actions[{i}].id"), format!("\"{}\" is used twice", a.id)));
        }
    }

    Ok(Manifest {
        api_version: api as u32,
        id,
        name,
        version,
        author,
        license,
        kind,
        hint,
        render,
        shader,
        script,
        fps,
        history: history.unwrap_or(false),
        params,
        uniforms,
        actions,
    })
}

fn uniform(v: &Value, path: &str) -> Result<UniformDef, ExtError> {
    let o = object(v, path)?;
    let key = req_str(o, path, "key", 32)?;
    if !is_key(&key, 32) {
        return Err(err(&at(path, "key"), "must be lowercase letters, digits and \"_\", starting with a letter (32 characters at most)"));
    }
    let ty = match o.get("type") {
        None => return Err(err(&at(path, "type"), "is required")),
        Some(Value::String(s)) => match s.as_str() {
            "f32" => UniformType::F32,
            "i32" => UniformType::I32,
            "u32" => UniformType::U32,
            "vec2f" => UniformType::Vec2f,
            "vec4f" => UniformType::Vec4f,
            other => return Err(err(&at(path, "type"), format!("must be f32, i32, u32, vec2f or vec4f, not \"{other}\""))),
        },
        Some(_) => return Err(err(&at(path, "type"), "must be a string")),
    };
    Ok(UniformDef { key, ty })
}

fn action(v: &Value, path: &str) -> Result<ActionDef, ExtError> {
    let o = object(v, path)?;
    let id = req_str(o, path, "id", 24)?;
    if !is_key(&id, 24) {
        return Err(err(&at(path, "id"), "must be lowercase letters, digits and \"_\", starting with a letter (24 characters at most)"));
    }
    let label = req_str(o, path, "label", 24)?;
    Ok(ActionDef { id, label })
}

/// A scalar `showIf` or option value: string, number or boolean.
fn is_scalar(v: &Value) -> bool {
    matches!(v, Value::String(_) | Value::Number(_) | Value::Bool(_))
}

fn param(v: &Value, path: &str) -> Result<ParamDef, ExtError> {
    let o = object(v, path)?;
    let key = req_str(o, path, "key", 32)?;
    if !is_key(&key, 32) {
        return Err(err(&at(path, "key"), "must be lowercase letters, digits and \"_\", starting with a letter (32 characters at most)"));
    }
    let label = req_str(o, path, "label", 60)?;
    let hint = opt_str(o, path, "hint", 200)?;
    let show_if = match o.get("showIf") {
        None => None,
        Some(s) => {
            let p = at(path, "showIf");
            let so = object(s, &p)?;
            let key = req_str(so, &p, "key", 32)?;
            let values = array(so, &p, "in", 16)?.ok_or_else(|| err(&at(&p, "in"), "is required"))?;
            if values.is_empty() {
                return Err(err(&at(&p, "in"), "must list at least one value"));
            }
            if let Some(i) = values.iter().position(|x| !is_scalar(x)) {
                return Err(err(&format!("{p}.in[{i}]"), "must be a string, number or boolean"));
            }
            Some(ShowIf { key, values: values.clone() })
        }
    };
    let base = ParamBase { key, label, show_if, hint };
    let kind = match o.get("type") {
        None => return Err(err(&at(path, "type"), "is required")),
        Some(Value::String(s)) => s.as_str(),
        Some(_) => return Err(err(&at(path, "type"), "must be a string")),
    };
    match kind {
        "slider" => {
            let (min, max, step, def) = (finite(o, path, "min")?, finite(o, path, "max")?, finite(o, path, "step")?, finite(o, path, "def")?);
            if min >= max {
                return Err(err(&at(path, "min"), format!("must be below max ({max})")));
            }
            if step <= 0.0 {
                return Err(err(&at(path, "step"), "must be above 0"));
            }
            if def < min || def > max {
                return Err(err(&at(path, "def"), format!("must be between min ({min}) and max ({max})")));
            }
            let int = opt_bool(o, path, "int")?.unwrap_or(false);
            Ok(ParamDef::Slider { base, min, max, step, def, unit: opt_str(o, path, "unit", 12)?, int })
        }
        "int" => {
            let (min, max, def) = (integer(o, path, "min")?, integer(o, path, "max")?, integer(o, path, "def")?);
            if min >= max {
                return Err(err(&at(path, "min"), format!("must be below max ({max})")));
            }
            if def < min || def > max {
                return Err(err(&at(path, "def"), format!("must be between min ({min}) and max ({max})")));
            }
            Ok(ParamDef::Int { base, min, max, def, unit: opt_str(o, path, "unit", 12)? })
        }
        "bool" => match o.get("def") {
            Some(Value::Bool(b)) => Ok(ParamDef::Bool { base, def: *b }),
            _ => Err(err(&at(path, "def"), "must be true or false")),
        },
        "color" => {
            let def = o
                .get("def")
                .and_then(|d| d.as_u64())
                .filter(|n| *n <= u64::from(u32::MAX))
                .ok_or_else(|| err(&at(path, "def"), "must be a whole number from 0 to 4294967295 (OBS colour 0xAABBGGRR)"))?;
            Ok(ParamDef::Color { base, def: def as u32, alpha: opt_bool(o, path, "alpha")?.unwrap_or(false) })
        }
        "path" => {
            match o.get("def") {
                Some(Value::String(s)) if s.is_empty() => {}
                _ => return Err(err(&at(path, "def"), "must be \"\" (the user picks the file)")),
            }
            match o.get("filter") {
                Some(Value::String(s)) if s == "image" => {}
                _ => return Err(err(&at(path, "filter"), "must be \"image\"")),
            }
            Ok(ParamDef::Path { base, def: String::new(), filter: "image".into() })
        }
        "select" => {
            let list = array(o, path, "options", MAX_OPTIONS)?.ok_or_else(|| err(&at(path, "options"), "is required"))?;
            if list.is_empty() {
                return Err(err(&at(path, "options"), "must list at least one option"));
            }
            let mut options: Vec<SelectOption> = Vec::new();
            for (i, item) in list.iter().enumerate() {
                let p = format!("{path}.options[{i}]");
                let io = object(item, &p)?;
                let value = match io.get("value") {
                    Some(v @ (Value::String(_) | Value::Number(_))) => v.clone(),
                    _ => return Err(err(&at(&p, "value"), "must be a string or a number")),
                };
                if options.iter().any(|o| o.value == value) {
                    return Err(err(&at(&p, "value"), "is used twice"));
                }
                if let Some(first) = options.first() {
                    if first.value.is_number() != value.is_number() {
                        return Err(err(&at(&p, "value"), "must be a number like the other options (or a string like them)"));
                    }
                }
                options.push(SelectOption { value, label: req_str(io, &p, "label", 60)? });
            }
            let def = o.get("def").cloned().ok_or_else(|| err(&at(path, "def"), "is required"))?;
            if !options.iter().any(|o| o.value == def) {
                return Err(err(&at(path, "def"), "must be the value of one of the options"));
            }
            Ok(ParamDef::Select { base, def, options })
        }
        "combo" => {
            let def = match o.get("def") {
                Some(Value::String(s)) => s.clone(),
                _ => return Err(err(&at(path, "def"), "must be a string")),
            };
            let list = array(o, path, "options", MAX_OPTIONS)?.ok_or_else(|| err(&at(path, "options"), "is required"))?;
            let mut options = Vec::new();
            for (i, item) in list.iter().enumerate() {
                match item {
                    Value::String(s) if !s.is_empty() && s.chars().count() <= 60 => options.push(s.clone()),
                    _ => return Err(err(&format!("{path}.options[{i}]"), "must be a string of 1 to 60 characters")),
                }
            }
            let pattern = opt_str(o, path, "pattern", 200)?;
            if pattern.is_none() && !options.contains(&def) {
                return Err(err(&at(path, "def"), "must be one of the options"));
            }
            Ok(ParamDef::Combo { base, def, options, pattern, placeholder: opt_str(o, path, "placeholder", 60)? })
        }
        "text" => {
            let max_length = match o.get("maxLength") {
                None => DEFAULT_TEXT_LENGTH,
                Some(v) => as_integer(v)
                    .filter(|n| (1..=i64::from(MAX_TEXT_LENGTH)).contains(n))
                    .ok_or_else(|| err(&at(path, "maxLength"), format!("must be a whole number from 1 to {MAX_TEXT_LENGTH}")))? as u32,
            };
            let def = match o.get("def") {
                Some(Value::String(s)) => s.clone(),
                _ => return Err(err(&at(path, "def"), "must be a string")),
            };
            if def.chars().count() > max_length as usize {
                return Err(err(&at(path, "def"), format!("is longer than maxLength ({max_length})")));
            }
            Ok(ParamDef::Text { base, def, max_length })
        }
        other => Err(err(
            &at(path, "type"),
            format!("must be slider, select, bool, int, color, path, combo or text, not \"{other}\""),
        )),
    }
}

/// Rules that look at several params: unique keys, `showIf` targets, the number of image params.
fn check_param_set(params: &[ParamDef]) -> Result<(), ExtError> {
    for (i, p) in params.iter().enumerate() {
        if params[..i].iter().any(|q| q.key() == p.key()) {
            return Err(err(&format!("params[{i}].key"), format!("\"{}\" is used twice", p.key())));
        }
        if let Some(s) = &p.base().show_if {
            if s.key == p.key() {
                return Err(err(&format!("params[{i}].showIf.key"), "a param cannot depend on itself"));
            }
            if !params.iter().any(|q| q.key() == s.key) {
                return Err(err(&format!("params[{i}].showIf.key"), format!("\"{}\" is not a param of this extension", s.key)));
            }
        }
    }
    let images = params.iter().filter(|p| matches!(p, ParamDef::Path { .. })).count();
    if images > MAX_PATH_PARAMS {
        let first_extra = params.iter().enumerate().filter(|(_, p)| matches!(p, ParamDef::Path { .. })).nth(MAX_PATH_PARAMS).map_or(0, |(i, _)| i);
        return Err(err(&format!("params[{first_extra}]"), format!("{images} image (path) params; at most {MAX_PATH_PARAMS} are allowed")));
    }
    Ok(())
}

/// Look at the files the manifest names (in `dir`, the extension folder): each must exist, be a regular file inside
/// the folder once links are resolved, and be at most [`MAX_FILE_BYTES`].
pub fn check_files(dir: &Path, m: &Manifest) -> Result<(), ExtError> {
    let canon_dir = dir.canonicalize().map_err(|e| ExtError::manifest(format!("the extension folder cannot be read: {e}")))?;
    for (field, file) in [("shader", &m.shader), ("script", &m.script)] {
        let Some(file) = file else { continue };
        let full = dir.join(file);
        let shown = file.to_string_lossy().replace('\\', "/");
        let meta = std::fs::metadata(&full).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                err(field, format!("{shown} does not exist"))
            } else {
                err(field, format!("{shown} cannot be read: {e}"))
            }
        })?;
        if !meta.is_file() {
            return Err(err(field, format!("{shown} is not a file")));
        }
        let canon = full.canonicalize().map_err(|e| err(field, format!("{shown} cannot be read: {e}")))?;
        if !canon.starts_with(&canon_dir) {
            return Err(err(field, format!("{shown} leaves the extension folder")));
        }
        if meta.len() > MAX_FILE_BYTES {
            return Err(err(field, format!("{shown} is {} KB; the limit is {} KB", meta.len().div_ceil(1024), MAX_FILE_BYTES / 1024)));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn repo(rel: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join(rel)
    }

    /// The invalid manifests of `src-tauri/tests/ext_manifests/`, each with the field its message must name.
    const INVALID: &[(&str, &str)] = &[
        ("01-not-json.json", "extension.json"),
        ("02-not-object.json", "extension.json"),
        ("03-no-api-version.json", "apiVersion"),
        ("04-api-too-new.json", "apiVersion"),
        ("05-id-uppercase.json", "id"),
        ("06-id-folder-mismatch.json", "id"),
        ("07-no-name.json", "name"),
        ("08-version-not-semver.json", "version"),
        ("09-kind-unknown.json", "kind"),
        ("10-filter-no-shader.json", "shader"),
        ("11-shader-escapes-folder.json", "shader"),
        ("12-permissions.json", "permissions[0]"),
        ("13-param-type-unknown.json", "params[1].type"),
        ("14-slider-min-above-max.json", "params[0].min"),
        ("15-slider-def-out-of-range.json", "params[0].def"),
        ("16-select-def-not-an-option.json", "params[0].def"),
        ("17-param-key-twice.json", "params[1].key"),
        ("18-show-if-unknown-key.json", "params[1].showIf.key"),
        ("19-path-filter-lut.json", "params[0].filter"),
        ("20-uniform-without-script.json", "uniforms"),
        ("21-widget-action-id.json", "actions[1].id"),
        ("22-text-def-too-long.json", "params[0].def"),
    ];

    #[test]
    fn invalid_manifests_are_each_rejected_naming_the_field() {
        let dir = repo("src-tauri/tests/ext_manifests");
        let mut on_disk: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".json"))
            .collect();
        on_disk.sort();
        let mut listed: Vec<String> = INVALID.iter().map(|(n, _)| n.to_string()).collect();
        listed.sort();
        assert_eq!(on_disk, listed, "the table and the folder must list the same files");
        assert!(INVALID.len() >= 20);
        for (file, field) in INVALID {
            let text = std::fs::read_to_string(dir.join(file)).unwrap();
            let e = Manifest::parse(&text, "sample").expect_err(file);
            assert!(e.message.contains(field), "{file}: the message {:?} does not name {field}", e.message);
            assert_eq!(e.kind, super::super::ErrorKind::Manifest);
        }
    }

    #[test]
    fn every_example_manifest_parses_and_its_files_exist() {
        let dir = repo("sdk/examples");
        let mut seen = 0;
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let id = entry.file_name().to_string_lossy().into_owned();
            let text = std::fs::read_to_string(path.join("extension.json")).unwrap_or_else(|e| panic!("{id}: {e}"));
            let m = Manifest::parse(&text, &id).unwrap_or_else(|e| panic!("{id}: {}", e.message));
            check_files(&path, &m).unwrap_or_else(|e| panic!("{id}: {}", e.message));
            seen += 1;
        }
        assert!(seen >= 1, "no example found in sdk/examples");
    }

    fn base() -> Value {
        json!({ "apiVersion": 1, "id": "x1", "name": "X", "version": "1.2.3", "kind": "filter", "shader": "x.wgsl" })
    }

    fn parse(v: &Value) -> Result<Manifest, ExtError> {
        Manifest::parse(&v.to_string(), "x1")
    }

    #[test]
    fn a_minimal_filter_parses_with_defaults() {
        let m = parse(&base()).unwrap();
        assert_eq!((m.id.as_str(), m.kind, m.fps, m.history), ("x1", ExtKind::Filter, 0.0, false));
        assert_eq!(m.shader.as_deref(), Some(Path::new("x.wgsl")));
        assert!(m.params.is_empty() && m.uniforms.is_empty() && m.actions.is_empty());
    }

    #[test]
    fn unknown_fields_are_ignored_but_not_unknown_param_types() {
        let mut v = base();
        v["futureThing"] = json!({ "a": 1 });
        v["params"] = json!([{ "type": "bool", "key": "on", "label": "On", "def": true, "tooltipStyle": "x" }]);
        assert!(parse(&v).is_ok());
        v["params"] = json!([{ "type": "gradient", "key": "g", "label": "G", "def": 0 }]);
        assert!(parse(&v).unwrap_err().message.contains("params[0].type"));
    }

    #[test]
    fn a_bom_and_a_byte_order_are_fine() {
        let text = format!("\u{feff}{}", base());
        assert!(Manifest::parse(&text, "x1").is_ok());
    }

    #[test]
    fn the_api_version_check_comes_first() {
        let v = json!({ "apiVersion": 2, "id": "BAD ID", "kind": "banana" });
        assert_eq!(parse(&v).unwrap_err().message, "apiVersion: Needs a newer Better Cam (extension API 2)");
    }

    #[test]
    fn params_serialize_to_the_json_the_page_reads() {
        let mut v = base();
        v["params"] = json!([
            { "type": "slider", "key": "amount", "label": "Amount", "min": 0, "max": 1, "step": 0.01, "def": 0.5, "unit": "px", "showIf": { "key": "on", "in": [true] } },
            { "type": "bool", "key": "on", "label": "On", "def": true },
            { "type": "text", "key": "label", "label": "Label", "def": "Hi" },
            { "type": "color", "key": "tint", "label": "Tint", "def": 4294967295u32, "alpha": true },
            { "type": "select", "key": "mode", "label": "Mode", "def": "a", "options": [{ "value": "a", "label": "A" }, { "value": "b", "label": "B" }] },
            { "type": "path", "key": "logo", "label": "Logo", "def": "", "filter": "image" },
            { "type": "int", "key": "n", "label": "N", "min": 1, "max": 9, "def": 3 },
            { "type": "combo", "key": "size", "label": "Size", "def": "16:9", "options": ["16:9"], "pattern": "^\\d+x\\d+$" }
        ]);
        let m = parse(&v).unwrap();
        let out = serde_json::to_value(&m.params).unwrap();
        assert_eq!(out[0]["showIf"], json!({ "key": "on", "in": [true] }));
        assert_eq!(out[0]["unit"], json!("px"));
        assert!(out[0].get("int").is_none());
        assert_eq!(out[2], json!({ "type": "text", "key": "label", "label": "Label", "def": "Hi", "maxLength": 80 }));
        assert_eq!(out[3]["def"], json!(4294967295u32));
        assert_eq!(out[3]["alpha"], json!(true));
        assert_eq!(out[5]["filter"], json!("image"));
        assert_eq!(out[7]["pattern"], json!("^\\d+x\\d+$"));
    }

    #[test]
    fn kinds_have_their_own_requirements() {
        let overlay = |extra: Value| {
            let mut v = json!({ "apiVersion": 1, "id": "x1", "name": "X", "version": "1.0.0", "kind": "overlay" });
            for (k, val) in extra.as_object().unwrap() {
                v[k] = val.clone();
            }
            parse(&v)
        };
        assert!(overlay(json!({})).unwrap_err().message.starts_with("render"));
        assert!(overlay(json!({ "render": "scene" })).unwrap_err().message.starts_with("script"));
        assert!(overlay(json!({ "render": "scene", "script": "x.ts", "fps": 2 })).is_ok());
        assert!(overlay(json!({ "render": "scene", "script": "x.ts", "fps": 31 })).unwrap_err().message.starts_with("fps"));
        assert!(overlay(json!({ "render": "shader" })).unwrap_err().message.starts_with("shader"));
        assert!(overlay(json!({ "render": "shader", "shader": "x.wgsl", "history": true })).unwrap_err().message.starts_with("history"));
        let widget = |extra: Value| {
            let mut v = json!({ "apiVersion": 1, "id": "x1", "name": "X", "version": "1.0.0", "kind": "widget", "script": "x.ts" });
            for (k, val) in extra.as_object().unwrap() {
                v[k] = val.clone();
            }
            parse(&v)
        };
        assert!(widget(json!({ "render": "shader", "shader": "x.wgsl" })).unwrap_err().message.contains("scene"));
        let ok = widget(json!({ "render": "scene", "actions": [{ "id": "start", "label": "Start" }] })).unwrap();
        assert_eq!(ok.actions.len(), 1);
        assert!(parse(&{
            let mut v = base();
            v["actions"] = json!([{ "id": "go", "label": "Go" }]);
            v
        })
        .unwrap_err()
        .message
        .starts_with("actions"));
    }

    #[test]
    fn limits_are_enforced() {
        let mut v = base();
        v["params"] = Value::Array((0..33).map(|i| json!({ "type": "bool", "key": format!("p{i}"), "label": "P", "def": false })).collect());
        assert!(parse(&v).unwrap_err().message.starts_with("params"));
        v["params"] = Value::Array((0..5).map(|i| json!({ "type": "path", "key": format!("img{i}"), "label": "I", "def": "", "filter": "image" })).collect());
        assert!(parse(&v).unwrap_err().message.contains("params[4]"));
    }

    #[test]
    fn semver_and_ids() {
        for ok in ["0.0.1", "1.2.3", "10.20.30", "1.0.0-beta.1", "1.0.0+build.5", "1.0.0-rc.1+x"] {
            assert!(is_semver(ok), "{ok}");
        }
        for bad in ["1", "1.2", "01.2.3", "1.2.3.4", "1.2.x", "1.0.0-", "v1.0.0", ""] {
            assert!(!is_semver(bad), "{bad}");
        }
        assert!(is_id("vhs") && is_id("a1") && is_id("my-clock-2"));
        assert!(!is_id("a") && !is_id("-a") && !is_id("A1") && !is_id("a_b") && !is_id(&"a".repeat(49)));
    }

    #[test]
    fn check_files_refuses_missing_and_oversize_files() {
        let dir = vcam_protocol::testdir::TestDir::new("better-cam-manifest");
        let m = parse(&base()).unwrap();
        assert!(check_files(&dir, &m).unwrap_err().message.starts_with("shader: x.wgsl does not exist"));
        std::fs::write(dir.join("x.wgsl"), "// ok").unwrap();
        assert!(check_files(&dir, &m).is_ok());
        std::fs::write(dir.join("x.wgsl"), vec![b' '; MAX_FILE_BYTES as usize + 1]).unwrap();
        assert!(check_files(&dir, &m).unwrap_err().message.contains("the limit is 256 KB"));
    }
}
