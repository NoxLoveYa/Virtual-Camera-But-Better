//! The chain entry of an extension filter (`ext:<id>`): one pixel shader the loader built from the extension's WGSL (or
//! HLSL), run like a built-in pass (`docs/EXTENSIONS.md` 2.1, 4.3, 4.5, 5.1, 6.3, 6.4).
//!
//! # What the render thread does
//! Nothing that can wait. [`ExtShaderFilter::render`] looks at one atomic (the registry's generation), takes a finished
//! build from its [`Ticket`] with `try_lock`, and draws. Reading files, translating, FXC and `CreatePixelShader` happen
//! on the loader thread ([`super::loader`]); until the first build is ready, and whenever the extension is switched
//! off or gone, the card passes the picture through (a copy), so a card of a missing extension is a no-op as before.
//!
//! # The pass
//! * `t0` is the picture so far, `t1` the card's previous output (`history`, a texture of its own, only when the
//!   manifest asks for it; transparent black until there is one), `t2..t5` the `path` images in manifest order (1x1
//!   transparent until loaded); `s0` is the pass's linear clamp sampler. `b0` holds `frame`, written every frame; `b1`
//!   holds `params`, written when a value changed.
//! * **Colour space: the shader sees the gamma-encoded codes** (`Views::ENCODED`, the pool's `UNORM` views, which is
//!   what the built-in `ShaderFilter::new` does and what 4.3 documents). The sampler therefore interpolates codes, as
//!   OBS's filters and every built-in gamma-space filter do; a shader that needs light uses `to_linear` and `to_srgb`.
//!   The path images are bound by their `UNORM` view as well. The pool also offers `UNORM_SRGB` views (hardware
//!   decode and encode); a manifest switch for them is not part of API 1.
//!
//! # Hot reload and errors
//! The registry bumps a generation when an extension changed. The filter then asks the snapshot: a new manifest means a
//! new build; the old shader keeps running until the new one is ready, and a failed build keeps the old one and tells
//! the card (`Note::Error`, cleared by the next good build). An extension that is switched off or gone is a no-op; one
//! whose manifest broke keeps what it had (the page shows why).
//!
//! # Guard
//! The pass is timed with GPU timestamps. A mean over one second above [`SLOW_MS`] switches the card off ("Too slow
//! for live video") until the extension is reloaded. A shader that hangs the GPU outright is the device-loss path.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::layout::PackError;
use super::loader::{self, BuildSpec, Built, Loaded, Ticket};
use super::manifest::{ExtKind, Manifest};
use super::wgsl::FrameValues;
use super::{params, ExtRegistry, ExtState};
use crate::fx::assets::{Asset, AssetClient, AssetHandle, AssetKind};
use crate::fx::{ps, FrameInfo, GpuFilter, Note, Params};
use crate::gpu::{ConstantBuffer, DrawExtras, GpuResult, GpuTimer, Pass, Pooled, Renderer, RgbaTexture, Views};

use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11PixelShader, ID3D11ShaderResourceView};

/// A shader whose pass takes longer than this on average over [`SLOW_WINDOW`] is switched off (6.3).
pub const SLOW_MS: f64 = 3.0;
pub const SLOW_WINDOW: Duration = Duration::from_secs(1);

/// What a card is doing, for tests and diagnostics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilterState {
    /// No shader yet (the build is queued or running, or there is none): the picture passes through.
    Waiting,
    /// A shader runs.
    Running,
    /// The shader was switched off for taking too long.
    TooSlow,
}

struct Pending {
    ticket: Ticket,
    manifest: Arc<Manifest>,
}

/// The shader that runs, with what only it needs.
struct Active {
    manifest: Arc<Manifest>,
    ps: ID3D11PixelShader,
    built: Arc<Built>,
    b0: Option<ConstantBuffer>,
    b1: Option<ConstantBuffer>,
    /// The packed `params`; written to `b1` when `dirty`.
    packed: Vec<u8>,
    dirty: bool,
    history: Option<Pooled>,
}

/// The mean GPU time of the pass over a window, against the limit.
struct Guard {
    limit_ms: f64,
    window: Duration,
    since: Option<Instant>,
    sum: f64,
    n: u32,
    size: (u32, u32),
}

impl Guard {
    fn new(limit_ms: f64, window: Duration) -> Guard {
        Guard { limit_ms, window, since: None, sum: 0.0, n: 0, size: (0, 0) }
    }

    fn reset(&mut self) {
        self.since = None;
        self.sum = 0.0;
        self.n = 0;
    }

    /// A measured pass of `ms` at `now`. Gives the mean when a window ended above the limit.
    fn add(&mut self, ms: f64, now: Instant) -> Option<f64> {
        let start = *self.since.get_or_insert(now);
        self.sum += ms;
        self.n += 1;
        if now.saturating_duration_since(start) < self.window {
            return None;
        }
        let mean = self.sum / self.n as f64;
        self.reset();
        (mean > self.limit_ms).then_some(mean)
    }
}

pub struct ExtShaderFilter {
    ext_id: String,
    registry: ExtRegistry,
    device: Option<ID3D11Device>,
    /// The registry generation last looked at.
    seen: Option<u64>,
    cfg: Params,
    canvas: Option<(u32, u32)>,
    clock: FrameInfo,
    rng: u64,
    pending: Option<Pending>,
    /// The manifest of the last build that failed, so a poll that changed nothing about it does not try again.
    failed_for: Option<Arc<Manifest>>,
    active: Option<Active>,
    client: Option<(AssetClient, String)>,
    /// One handle per `path` param of the active manifest, by key.
    images: Vec<(String, AssetHandle)>,
    blank: Option<Pooled>,
    timer: Option<GpuTimer>,
    guard: Guard,
    tripped: Option<f64>,
    notes: Vec<Note>,
    /// The error the card shows now (empty: none); a note is only sent when it changes.
    shown_error: String,
}

impl ExtShaderFilter {
    /// A card for the extension `ext_id` (the kind without `ext:`), reading the registry's snapshots.
    pub fn new(ext_id: &str, registry: ExtRegistry) -> ExtShaderFilter {
        // Any seed will do: the noise only has to differ from frame to frame (and between two cards).
        let mut seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0x9E37_79B9_7F4A_7C15, |d| d.as_nanos() as u64);
        for b in ext_id.bytes() {
            seed = seed.rotate_left(5) ^ b as u64;
            seed = seed.wrapping_mul(0x0000_0100_0000_01B3);
        }
        ExtShaderFilter {
            ext_id: ext_id.to_string(),
            registry,
            device: None,
            seen: None,
            cfg: Params::new(),
            canvas: None,
            clock: FrameInfo::default(),
            rng: seed | 1,
            pending: None,
            failed_for: None,
            active: None,
            client: None,
            images: Vec::new(),
            blank: None,
            timer: None,
            guard: Guard::new(SLOW_MS, SLOW_WINDOW),
            tripped: None,
            notes: Vec::new(),
            shown_error: String::new(),
        }
    }

    pub fn state(&self) -> FilterState {
        if self.tripped.is_some() {
            FilterState::TooSlow
        } else if self.active.is_some() {
            FilterState::Running
        } else {
            FilterState::Waiting
        }
    }

    /// The error the card shows now, empty when none.
    pub fn error(&self) -> &str {
        &self.shown_error
    }

    /// Another limit and window for the GPU time guard (tests).
    pub fn set_guard(&mut self, limit_ms: f64, window: Duration) {
        self.guard = Guard::new(limit_ms, window);
    }

    /// The hash of the source of the shader that runs (tests).
    pub fn running_hash(&self) -> Option<u64> {
        self.active.as_ref().map(|a| a.built.source_hash)
    }

    fn set_error(&mut self, message: String) {
        if message != self.shown_error {
            self.notes.push(Note::Error(message.clone()));
            self.shown_error = message;
        }
    }

    fn clear_error(&mut self) {
        self.set_error(String::new());
    }

    // ---- the registry and the loader ----

    fn service(&mut self) {
        let Some(device) = self.device.clone() else { return };
        let generation = self.registry.generation();
        if self.seen != Some(generation) {
            self.seen = Some(generation);
            self.sync_with_registry(&device);
        }
        self.poll_pending();
    }

    fn sync_with_registry(&mut self, device: &ID3D11Device) {
        let snapshot = self.registry.snapshot();
        let Some(m) = snapshot.ready(&self.ext_id) else {
            if snapshot.info(&self.ext_id).is_some_and(|i| i.state == ExtState::Error) {
                // The manifest or a file broke: what runs keeps running, the page says what is wrong.
                return;
            }
            // Switched off or gone: a no-op card.
            self.active = None;
            self.pending = None;
            self.failed_for = None;
            self.images.clear();
            self.clear_error();
            return;
        };
        if m.kind != ExtKind::Filter {
            self.active = None;
            self.pending = None;
            self.images.clear();
            self.set_error(format!("“{}” is not a filter extension, so it cannot be in the filter chain", self.ext_id));
            return;
        }
        // Already asked for, running, or failed for this very manifest (a failed build is not asked for again until
        // the extension changes; the old shader may still be running meanwhile).
        let known = [self.pending.as_ref().map(|p| &p.manifest), self.active.as_ref().map(|a| &a.manifest), self.failed_for.as_ref()];
        if known.into_iter().flatten().any(|k| Arc::ptr_eq(k, m)) {
            return;
        }
        let (Some(shader), Some(info)) = (&m.shader, snapshot.info(&self.ext_id)) else {
            self.set_error(format!("“{}” has no shader file", self.ext_id));
            return;
        };
        let spec = BuildSpec {
            file_name: shader.file_name().map_or_else(|| shader.to_string_lossy().into_owned(), |n| n.to_string_lossy().into_owned()),
            path: PathBuf::from(&info.folder).join(shader),
            hlsl: shader.extension().is_some_and(|e| e.eq_ignore_ascii_case("hlsl")),
            params: m.param_decls(),
            images: m.image_keys().into_iter().map(str::to_string).collect(),
        };
        self.failed_for = None;
        self.pending = Some(Pending { ticket: loader::submit(spec, device.clone()), manifest: m.clone() });
    }

    fn poll_pending(&mut self) {
        let Some(result) = self.pending.as_ref().and_then(|p| p.ticket.take()) else { return };
        let Some(p) = self.pending.take() else { return };
        match result {
            Ok(loaded) => self.activate(p.manifest, loaded),
            Err(message) => {
                self.failed_for = Some(p.manifest);
                self.set_error(message);
            }
        }
    }

    fn activate(&mut self, manifest: Arc<Manifest>, loaded: Loaded) {
        let keys: Vec<String> = manifest.image_keys().into_iter().map(str::to_string).collect();
        if self.images.iter().map(|(k, _)| k).ne(keys.iter()) {
            self.images = match &self.client {
                Some((client, card)) => keys.iter().map(|k| (k.clone(), client.handle(card))).collect(),
                None => Vec::new(),
            };
        }
        self.active = Some(Active {
            manifest,
            ps: loaded.ps,
            built: loaded.built,
            b0: None,
            b1: None,
            packed: Vec::new(),
            dirty: true,
            history: None,
        });
        self.tripped = None;
        self.guard.reset();
        self.clear_error();
        self.request_images();
        self.repack();
    }

    fn request_images(&mut self) {
        for (key, handle) in &mut self.images {
            handle.request(AssetKind::Image, ps(&self.cfg, key, ""));
        }
    }

    /// Pack the card's settings for the running shader's `params`.
    fn repack(&mut self) {
        let Some(a) = &mut self.active else { return };
        let Some(layout) = &a.built.translated.params else { return };
        match params::pack(&a.manifest, layout, &self.cfg) {
            Ok(bytes) => {
                if bytes != a.packed {
                    a.packed = bytes;
                    a.dirty = true;
                }
            }
            Err(PackError(e)) => {
                let message = format!("{}: the settings do not fit the shader ({e})", self.ext_id);
                self.set_error(message);
            }
        }
    }

    fn next_seed(&mut self) -> f32 {
        // xorshift64*: a fresh value in 0..1 every frame; the shader only needs noise.
        let mut x = self.rng;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng = x;
        (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 40) as f32 / 16_777_216.0
    }

    /// A 1x1 transparent texture for an input that has nothing yet.
    fn blank_srv(&mut self, r: &Renderer) -> GpuResult<ID3D11ShaderResourceView> {
        if self.blank.is_none() {
            let t = r.texture(1, 1)?;
            r.clear(&t.rtv, [0.0; 4]);
            self.blank = Some(t);
        }
        Ok(self.blank.as_ref().expect("made above").srv.clone())
    }
}

impl GpuFilter for ExtShaderFilter {
    fn attach_asset_client(&mut self, client: &AssetClient, card_id: &str) {
        self.client = Some((client.clone(), card_id.to_string()));
    }

    fn set_canvas(&mut self, size: (u32, u32)) {
        self.canvas = Some(size);
    }

    fn set_frame(&mut self, frame: &FrameInfo) {
        self.clock = *frame;
    }

    fn update(&mut self, r: &Renderer, p: &Params) {
        if self.device.as_ref() != Some(&r.gpu.device) {
            // A new device (the engine rebuilt after a GPU reset): nothing made on the old one can be used.
            self.device = Some(r.gpu.device.clone());
            self.active = None;
            self.pending = None;
            self.failed_for = None;
            self.blank = None;
            self.timer = None;
            self.seen = None;
        }
        self.cfg = p.clone();
        self.service();
        self.request_images();
        self.repack();
    }

    fn render(&mut self, r: &Renderer, input: &RgbaTexture, output: &RgbaTexture) -> GpuResult<()> {
        self.service();
        if self.active.is_none() || self.tripped.is_some() {
            return r.copy(input, output);
        }
        let (w, h) = (output.width, output.height);
        let blank = self.blank_srv(r)?;
        let frame = FrameValues {
            size: [w as f32, h as f32],
            canvas: self.canvas.map_or([w as f32, h as f32], |(cw, ch)| [cw as f32, ch as f32]),
            time: self.clock.time as f32,
            delta: self.clock.delta as f32,
            frame: self.clock.frame,
            seed: self.next_seed(),
        };
        // The images first: asking the handles needs `self`, the rest of the pass borrows the active shader.
        let image_srvs: Vec<ID3D11ShaderResourceView> = self
            .images
            .iter()
            .map(|(_, h)| match h.get().as_deref() {
                Some(Asset::Image(i)) => i.srv.clone(),
                _ => blank.clone(),
            })
            .collect();
        let want_images = self.active.as_ref().map_or(0, |a| a.manifest.image_keys().len());
        let a = self.active.as_mut().expect("checked above");

        if let Some(bytes) = a.built.translated.pack_frame(&frame) {
            let bytes = bytes.map_err(|e| crate::gpu::GpuError::Invalid(format!("frame values: {e}")))?;
            if a.b0.is_none() {
                a.b0 = Some(r.constants(bytes.len())?);
            }
            a.b0.as_ref().expect("made above").write_bytes(&r.gpu.context, &bytes)?;
        }
        if a.built.translated.params.is_some() {
            if a.b1.is_none() {
                a.b1 = Some(r.constants(a.packed.len().max(16))?);
                a.dirty = true;
            }
            if a.dirty {
                if a.packed.is_empty() {
                    a.packed = vec![0; 16];
                }
                a.b1.as_ref().expect("made above").write_bytes(&r.gpu.context, &a.packed)?;
                a.dirty = false;
            }
        }
        if a.manifest.history && a.history.as_ref().is_none_or(|t| (t.width, t.height) != (w, h)) {
            let t = r.texture(w, h)?;
            r.clear(&t.rtv, [0.0; 4]);
            a.history = Some(t);
        }

        let mut extras: Vec<Option<ID3D11ShaderResourceView>> = Vec::with_capacity(1 + want_images);
        extras.push(Some(a.history.as_ref().map_or_else(|| blank.clone(), |t| t.srv.clone())));
        for i in 0..want_images {
            extras.push(Some(image_srvs.get(i).cloned().unwrap_or_else(|| blank.clone())));
        }
        let srvs = Views::ENCODED.bind(input, &extras);

        if self.timer.is_none() {
            self.timer = GpuTimer::new(&r.gpu.device).ok();
        }
        if self.guard.size != (w, h) {
            self.guard.size = (w, h);
            self.guard.reset();
        }
        let timed = self.timer.as_mut().is_some_and(|t| t.begin(&r.gpu.context));
        r.draw_with(
            &Pass {
                ps: &a.ps,
                srvs: &srvs,
                constants: a.b0.as_ref(),
                target: Views::ENCODED.target(output),
                width: w,
                height: h,
            },
            &DrawExtras { constants_b1: a.b1.as_ref() },
        );
        if let Some(t) = self.timer.as_mut() {
            if timed {
                t.end(&r.gpu.context);
            }
        }
        if let Some(history) = &a.history {
            r.copy(output, history)?;
        }
        if let Some(ms) = self.timer.as_mut().and_then(|t| t.poll(&r.gpu.context)) {
            if let Some(mean) = self.guard.add(ms, Instant::now()) {
                self.tripped = Some(mean);
                self.set_error(format!("Too slow for live video ({mean:.1} ms per frame)"));
            }
        }
        Ok(())
    }

    fn take_notes(&mut self, out: &mut Vec<Note>) {
        out.append(&mut self.notes);
    }

    fn bypassed(&mut self, _now: Instant) {
        // A switched-off card keeps no history; it is rebuilt when the card is switched on. Builds still finish, so an
        // error shows even on a card that is off.
        if let Some(a) = &mut self.active {
            a.history = None;
        }
        self.service();
    }
}

#[cfg(test)]
#[path = "filter_tests.rs"]
mod tests;
