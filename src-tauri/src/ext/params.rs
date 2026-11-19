//! A card's settings as the values of the shader's `Params` struct.
//!
//! The page sends `{key: value}` for every param of the extension. Each value is read with the manifest's own range
//! and default, the way the built-in filters read theirs (`fx::pf` and friends): a missing key, a value of the wrong
//! type or a number outside the range gives the default or the nearest allowed value, never an error. The members come
//! out in the form [`Layout::pack`](super::layout::Layout::pack) takes; which member has which type is
//! [`Manifest::param_decls`](super::manifest::Manifest::param_decls).

use serde_json::Value as Json;

use super::layout::{Layout, PackError, Value};
use super::manifest::{Manifest, ParamDef, UniformType};
use crate::fx::{pb, pc, pf, pi, Params};

/// `Params` member values for the manifest's params (from `cfg`) and uniforms (zero until a script computes them).
pub fn values(m: &Manifest, cfg: &Params) -> Vec<(String, Value)> {
    let mut out: Vec<(String, Value)> = Vec::new();
    for p in &m.params {
        let key = p.key();
        let v = match p {
            ParamDef::Slider { min, max, def, int, .. } => {
                let v = pf(cfg, key, *def as f32, *min as f32, *max as f32);
                Value::F32(if *int { v.round() } else { v })
            }
            ParamDef::Int { min, max, def, .. } => {
                let (lo, hi) = (clamp_i32(*min), clamp_i32(*max));
                Value::I32(pi(cfg, key, clamp_i32(*def), lo, hi))
            }
            ParamDef::Bool { def, .. } => Value::U32(pb(cfg, key, *def) as u32),
            ParamDef::Select { def, options, .. } => select(cfg.get(key), def, options.iter().map(|o| &o.value)),
            ParamDef::Color { def, alpha, .. } => {
                let mut c = pc(cfg, key, *def);
                if !*alpha {
                    c[3] = 1.0;
                }
                Value::vec(c)
            }
            ParamDef::Path { .. } | ParamDef::Text { .. } | ParamDef::Combo { .. } => continue,
        };
        out.push((key.to_string(), v));
    }
    for u in &m.uniforms {
        let zero = match u.ty {
            UniformType::F32 => Value::F32(0.0),
            UniformType::I32 => Value::I32(0),
            UniformType::U32 => Value::U32(0),
            UniformType::Vec2f => Value::vec([0.0; 2]),
            UniformType::Vec4f => Value::vec([0.0; 4]),
        };
        out.push((u.key.clone(), zero));
    }
    out
}

fn clamp_i32(v: i64) -> i32 {
    v.clamp(i32::MIN as i64, i32::MAX as i64) as i32
}

/// A select: the option the card holds, else the default. Numeric options give their number (`f32`), string options
/// the index of the option (`u32`).
fn select<'a>(have: Option<&Json>, def: &Json, options: impl Iterator<Item = &'a Json> + Clone) -> Value {
    let same = |a: &Json, b: &Json| match (a, b) {
        (Json::Number(x), Json::Number(y)) => x.as_f64() == y.as_f64(),
        _ => a == b,
    };
    let chosen = have
        .filter(|h| options.clone().any(|o| same(o, h)))
        .unwrap_or(def);
    if options.clone().all(Json::is_number) {
        Value::F32(chosen.as_f64().unwrap_or(0.0) as f32)
    } else {
        Value::U32(options.enumerate().find(|(_, o)| same(o, chosen)).map_or(0, |(i, _)| i as u32))
    }
}

/// The bytes of `cbuffer b1` for `layout` (what the shader declared) from the card's settings.
pub fn pack(m: &Manifest, layout: &Layout, cfg: &Params) -> Result<Vec<u8>, PackError> {
    let owned = values(m, cfg);
    let borrowed: Vec<(&str, Value)> = owned.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
    layout.pack(&borrowed)
}
