//! The filter framework (plan 4.5): a GPU filter chain configured from JSON and updated live.
//!
//! * [`FilterCfg`]: one entry of the chain as the frontend sends it (`{id, kind, enabled, params}`). `kind` is an OBS
//!   filter id (`color_filter`), the keys of `params` are OBS setting names.
//! * [`Params`] and the readers [`pf`], [`pi`], [`pb`], [`ps`], [`pc`]: read a value with a default and clamp it to its
//!   documented range. A missing key, a value of the wrong type or a non-finite number gives the default.
//! * [`GpuFilter`]: `update` (cheap, repack the constants), `output_size`, `render` (input texture to output
//!   texture). [`shader_filter::ShaderFilter`] implements it for any [`shader_filter::ShaderFilterDef`], the one
//!   pixel shader case; filters that need more (several passes, a changed size) implement it directly.
//! * [`Chain`]: the filters in order. [`Chain::apply`] takes the newest configuration on the render thread, keeping
//!   the instances whose `id` and `kind` are unchanged, and [`Chain::run`] renders a frame through the enabled ones.
//!   A filter whose `render` fails is bypassed for that frame; the failure is reported once ([`FilterError`]) until
//!   the filter has rendered again.
//! * [`assets`]: files that filters read (pictures, `.cube` tables), loaded off the render thread. A filter that
//!   needs one keeps the [`assets::AssetHandle`] the chain gives it in [`GpuFilter::attach_assets`].
//! * [`make`] and [`kinds`]: the registry of filter kinds. Adding a filter is one file and one line in `REGISTRY`.
//! * `defaults.json`: the default params of every registered kind, a copy of the TypeScript schema's defaults.

pub mod assets;
pub mod color;
pub mod geometry;
pub mod keys;
pub mod lut;
pub mod mask;
pub mod shader_filter;
pub mod sharpen;
#[cfg(test)]
mod tests;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::gpu::{GpuResult, Pooled, Renderer, RgbaTexture};
use assets::{AssetClient, AssetHandle};

/// The `params` of a filter: setting name to number, string or boolean.
pub type Params = serde_json::Map<String, Value>;

fn default_enabled() -> bool {
    true
}

/// One filter of the chain as the frontend sends it (plan 5.3).
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct FilterCfg {
    /// Identifies the instance across updates (the frontend's card id).
    pub id: String,
    /// The OBS filter id. Kinds that are not registered are skipped.
    pub kind: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub params: Params,
}

/// A filter that failed to render (`filter-error` event, plan 5.6).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct FilterError {
    pub id: String,
    pub message: String,
}

/// Parse the `chain` argument of `start_engine` and `set_chain`: a JSON array of [`FilterCfg`]. `null` is an empty
/// chain. A malformed entry rejects the whole chain with a message that names it.
pub fn parse_chain(value: &Value) -> Result<Vec<FilterCfg>, String> {
    match value {
        Value::Null => Ok(Vec::new()),
        Value::Array(items) => items
            .iter()
            .enumerate()
            .map(|(i, v)| {
                FilterCfg::deserialize(v).map_err(|e| format!("invalid chain entry {i}: {e}"))
            })
            .collect(),
        _ => Err("invalid chain: expected an array of filters".into()),
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Param readers

/// A float setting, clamped to `lo..=hi`. `d` when the key is missing, not a number or not finite.
pub fn pf(p: &Params, k: &str, d: f32, lo: f32, hi: f32) -> f32 {
    match p.get(k).and_then(Value::as_f64) {
        Some(v) if v.is_finite() => (v as f32).clamp(lo, hi),
        _ => d,
    }
}

/// An integer setting (a fractional value is rounded), clamped to `lo..=hi`.
pub fn pi(p: &Params, k: &str, d: i32, lo: i32, hi: i32) -> i32 {
    match p.get(k).and_then(Value::as_f64) {
        Some(v) if v.is_finite() => (v.round().clamp(lo as f64, hi as f64)) as i32,
        _ => d,
    }
}

/// A boolean setting. Only a JSON boolean counts.
pub fn pb(p: &Params, k: &str, d: bool) -> bool {
    p.get(k).and_then(Value::as_bool).unwrap_or(d)
}

/// A string setting.
pub fn ps<'a>(p: &'a Params, k: &str, d: &'a str) -> &'a str {
    p.get(k).and_then(Value::as_str).unwrap_or(d)
}

/// An OBS colour setting: the int `0xAABBGGRR` that `obs_properties_add_color` stores (red is the lowest byte), as
/// RGBA floats in `0..=1`. `d` (same int format) when the value is missing, not a number, or not a 32 bit colour.
pub fn pc(p: &Params, k: &str, d: u32) -> [f32; 4] {
    let v = p
        .get(k)
        .and_then(Value::as_f64)
        .filter(|v| v.is_finite())
        .map(f64::round)
        .filter(|v| (-2_147_483_648.0..=4_294_967_295.0).contains(v))
        // OBS keeps the colour in a signed int: a negative value is the same 32 bits.
        .map(|v| if v < 0.0 { (v as i32) as u32 } else { v as u32 })
        .unwrap_or(d);
    v.to_le_bytes().map(|b| b as f32 / 255.0)
}

// ---------------------------------------------------------------------------------------------------------------
// Filters

/// One filter on the GPU. All methods run on the render thread.
pub trait GpuFilter: Send {
    /// The chain calls this once, right after it made the filter, when the engine has an asset service (a chain
    /// built for a test may have none: then the filter never gets a handle and must work without assets). Keep the
    /// handle; ask for files with [`AssetHandle::request`] in `update` and read them with [`AssetHandle::get`] in
    /// `render`. The handle is the filter's alone: dropping the filter lets go of its file.
    fn attach_assets(&mut self, _assets: AssetHandle) {}

    /// Take new settings. Cheap: repack constants, remember what changed. Called every time the chain is applied,
    /// on every instance (also when nothing changed), so compare before doing real work.
    fn update(&mut self, r: &Renderer, p: &Params);

    /// Size of the result for an input of this size (crop, scale and rotate change it).
    fn output_size(&self, input: (u32, u32)) -> (u32, u32) {
        input
    }

    /// The engine's output resolution (OBS's "base (canvas) resolution"), for a filter that needs it (Scaling/Aspect
    /// Ratio). The chain calls it before the first frame that follows each change of that size, and on every
    /// instance made after it was set; a filter that does not care leaves the default. Called on the render thread.
    fn set_canvas(&mut self, _size: (u32, u32)) {}

    /// Prepare what the first `render` would otherwise build while a frame waits (compile shaders into the
    /// renderer's cache). Called for a fresh instance of every registered kind at engine start; see [`warm_up`].
    fn warm_up(&mut self, _r: &Renderer) -> GpuResult<()> {
        Ok(())
    }

    /// Render `input` into `output` (a pooled texture of the size [`GpuFilter::output_size`] returned). An error
    /// bypasses the filter for this frame.
    fn render(&mut self, r: &Renderer, input: &RgbaTexture, output: &RgbaTexture) -> GpuResult<()>;
}

type Maker = fn() -> Box<dyn GpuFilter>;

/// Every filter kind the engine knows: one line per filter.
const REGISTRY: &[(&str, Maker)] = &[
    (color::KIND, color::make),
    // Geometry (P10)
    (geometry::crop::KIND, geometry::crop::make),
    (geometry::scale::KIND, geometry::scale::make),
    (geometry::transform::KIND, geometry::transform::make),
    // Sharpen and Apply LUT (P11)
    (sharpen::KIND, sharpen::make),
    (lut::KIND, lut::make),
    // Keys and Image Mask/Blend (P12)
    (keys::CHROMA_KIND, keys::make_chroma),
    (keys::COLOR_KIND, keys::make_color),
    (keys::LUMA_KIND, keys::make_luma),
    (mask::KIND, mask::make),
];

/// A new filter of this kind, with default settings. `None` for a kind that is not registered.
pub fn make(kind: &str) -> Option<Box<dyn GpuFilter>> {
    REGISTRY.iter().find(|(k, _)| *k == kind).map(|(_, m)| m())
}

/// The registered kinds.
pub fn kinds() -> impl Iterator<Item = &'static str> {
    REGISTRY.iter().map(|(k, _)| *k)
}

/// Compile the shaders of every registered kind now (engine start), so the first frame that uses a filter does not
/// pay for the compiler. Returns the first error.
pub fn warm_up(r: &Renderer) -> GpuResult<()> {
    for (_, maker) in REGISTRY {
        maker().warm_up(r)?;
    }
    Ok(())
}

/// The default params of every registered kind: `{ "<kind>": { "<param>": value, ... }, ... }`.
pub const DEFAULTS_JSON: &str = include_str!("defaults.json");

// ---------------------------------------------------------------------------------------------------------------
// Chain

struct Slot {
    id: String,
    kind: String,
    enabled: bool,
    /// Counts instances made by this chain, so a test can tell a kept instance from a new one.
    serial: u64,
    /// The last render failed (and was reported).
    failing: bool,
    filter: Box<dyn GpuFilter>,
}

/// The filters of a session, in order.
pub struct Chain {
    slots: Vec<Slot>,
    factory: fn(&str) -> Option<Box<dyn GpuFilter>>,
    made: u64,
    errors: Vec<FilterError>,
    assets: Option<AssetClient>,
    /// The output resolution, once the engine has told ([`Chain::set_canvas`]).
    canvas: Option<(u32, u32)>,
}

impl Default for Chain {
    fn default() -> Chain {
        Chain::new()
    }
}

impl Chain {
    pub fn new() -> Chain {
        Chain::with_factory(make)
    }

    /// A chain that makes its filters with `factory` (tests add filters that fail on purpose).
    pub fn with_factory(factory: fn(&str) -> Option<Box<dyn GpuFilter>>) -> Chain {
        Chain {
            slots: Vec::new(),
            factory,
            made: 0,
            errors: Vec::new(),
            assets: None,
            canvas: None,
        }
    }

    /// Tell the filters the engine's output resolution ([`GpuFilter::set_canvas`]). Call it before [`Chain::run`]
    /// with the size the output stage will produce; it only reaches the filters when the size changed.
    pub fn set_canvas(&mut self, size: (u32, u32)) {
        if self.canvas != Some(size) {
            self.canvas = Some(size);
            for slot in &mut self.slots {
                slot.filter.set_canvas(size);
            }
        }
    }

    /// Give the filters this chain makes from now on a handle to the asset service ([`GpuFilter::attach_assets`]).
    /// Call it before the first [`Chain::apply`].
    pub fn set_assets(&mut self, assets: AssetClient) {
        self.assets = Some(assets);
    }

    /// Rebuild the chain from `cfg`: an entry whose `id` and `kind` match an existing instance keeps it, any other
    /// entry gets a new filter (kinds that are not registered are skipped), instances that are no longer listed are
    /// dropped. Every kept or new filter gets `update`. The order of `cfg` is the order of the chain.
    pub fn apply(&mut self, r: &Renderer, cfg: &[FilterCfg]) {
        let mut old = std::mem::take(&mut self.slots);
        for c in cfg {
            let mut slot = match old
                .iter()
                .position(|s| s.id == c.id && s.kind == c.kind)
            {
                Some(i) => old.swap_remove(i),
                None => match (self.factory)(&c.kind) {
                    Some(mut filter) => {
                        if let Some(a) = &self.assets {
                            filter.attach_assets(a.handle(&c.id));
                        }
                        if let Some(size) = self.canvas {
                            filter.set_canvas(size);
                        }
                        self.made += 1;
                        Slot {
                            id: c.id.clone(),
                            kind: c.kind.clone(),
                            enabled: c.enabled,
                            serial: self.made,
                            failing: false,
                            filter,
                        }
                    }
                    None => continue,
                },
            };
            slot.enabled = c.enabled;
            slot.filter.update(r, &c.params);
            self.slots.push(slot);
        }
    }

    /// Render `image` through the enabled filters. A filter that fails is bypassed: its input goes on to the next
    /// one, and the failure is queued for [`Chain::take_errors`] unless the filter was already failing. An empty
    /// chain hands the input back untouched.
    pub fn run(&mut self, r: &Renderer, mut image: Pooled) -> GpuResult<Pooled> {
        for slot in self.slots.iter_mut().filter(|s| s.enabled) {
            let (w, h) = slot.filter.output_size((image.width, image.height));
            let out = r.texture(w, h)?;
            match slot.filter.render(r, &image, &out) {
                Ok(()) => {
                    slot.failing = false;
                    // `image` returns to the pool here and may become the next filter's output.
                    image = out;
                }
                Err(e) => {
                    if !slot.failing {
                        slot.failing = true;
                        self.errors.push(FilterError {
                            id: slot.id.clone(),
                            message: e.to_string(),
                        });
                    }
                }
            }
        }
        Ok(image)
    }

    /// The failures to report since the last call.
    pub fn take_errors(&mut self) -> Vec<FilterError> {
        std::mem::take(&mut self.errors)
    }

    /// Filters in the chain (enabled or not).
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// `(id, kind, enabled, serial)` of every slot, in order. The serial numbers the instances this chain has made,
    /// so a kept instance keeps its serial and a recreated one gets a new one.
    pub fn instances(&self) -> Vec<(String, String, bool, u64)> {
        self.slots
            .iter()
            .map(|s| (s.id.clone(), s.kind.clone(), s.enabled, s.serial))
            .collect()
    }
}
