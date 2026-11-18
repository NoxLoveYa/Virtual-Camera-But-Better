# Better Cam — Extension API design

Status: design. No code exists yet. Written 2026-11-18 against `main` at `b121ea7`.
Owner request: "Custom api for widgets / overlay / filter (in ts and webgpu if possible)".
Facts were checked on 2026-11-18 against docs.rs (`naga`, `rquickjs`, `wgpu`), Microsoft Learn (WebView2,
Windows.Graphics.Capture, Direct3D 11), the `MicrosoftEdge/WebView2Feedback`, `tauri-apps/wry` and `gfx-rs/wgpu`
issue trackers, oxc.rs and the GNU GPL FAQ. Anything not checked against a primary source is marked
**(unverified)**. Numbers carry a tag: **(M)** measured in `docs/PERF.md`, **(S)** sourced, **(E)** estimate.

---

## 1. Summary

1. Three kinds of extension, each a folder with an `extension.json` manifest in `%APPDATA%\dev.bettercam.app\extensions\<id>\`.
2. **Filter**: one WGSL fragment shader. Rust translates WGSL to HLSL with `naga` and runs it through the existing
   D3D11 path (`D3DCompile`, `ps_5_0`) as a chain entry `ext:<id>`, next to the built-in filters.
3. **Overlay**: a layer drawn on top of the finished output frame, after the output stage. Either a TypeScript script
   that returns a *scene* (text, shapes, images; drawn natively with Direct2D/DirectWrite) or a WGSL shader layer.
4. **Widget**: an overlay with buttons on its card, persistent state and a status line (timer, "be right back", counter).
5. Scripts are TypeScript. The app strips types with `oxc` and runs them in QuickJS-NG (`rquickjs`), one runtime and
   one thread per card, with a memory cap and an interrupt deadline. No DOM, no network, no file access.
6. The render thread never waits: script output (uniforms, scenes) goes through latest-value mailboxes read with
   `try_lock`; shaders compile on a loader thread; a late result is skipped and the previous one reused.
7. Parameters use the existing `ParamDef` types (plus `text`), so cards, `ParamField`, presets and ordering are reused.
8. Errors and status reuse `filter-error` / `filter-status`, keyed by card id; registry errors go to a new
   `extensions-changed` event.
9. **What "TS and WebGPU" means here**: TypeScript is the language of manifests' types, of overlay/widget logic and of
   per-frame filter logic. WebGPU means *WGSL, WebGPU's shading language*. There is **no WebGPU device and no
   WebGPU JavaScript API** in the pipeline: a real WebGPU device would need either a D3D12 bridge on every frame or the
   webview, and both cost latency, memory or independence from the window (section 3).
10. Out of v1, kept possible: an HTML/CSS/WebGPU "browser layer" through an offscreen WebView2 (section 3.2, open
    question 2), in-app custom panels, network and audio signals.

---

## 2. The three kinds

### 2.1 Filter

Changes the camera picture at its place in the chain. Runs on the GPU for every frame.

| Can | Cannot (v1) |
|---|---|
| Read any texel of its input picture (gamma-encoded sRGB, straight alpha) | Change the picture size (crop/scale stay built-in) |
| Read its own previous output (`"history": true`) for trails and smoothing | Run more than one pass |
| Read up to 4 user-picked images (`path` params) | Read pixels from a script; scripts never see the camera |
| Use params, time, frame size, a per-frame random seed | Touch files, network, other cards |
| Get extra uniforms computed by an optional script each frame | Run on the CPU |

Examples: **VHS** (scanlines, colour bleed, tape noise, script-timed glitch bursts; worked in 4.8);
**Duotone** (maps luma between two colour params).

### 2.2 Overlay

Draws on top of the finished output frame: what Teams, Zoom and browsers see. Coordinates are output pixels.

| Can | Cannot (v1) |
|---|---|
| Draw a scene: rectangles, rounded rectangles, ellipses, lines, text (system fonts), images, groups with transform, opacity and clip | Read the camera picture or the frame below it |
| Or draw a WGSL shader layer (animated graphics, particles, frames) | Receive mouse or keyboard input |
| Re-render up to 30 times a second, or only on change | HTML, CSS, DOM, Canvas or WebGPU JS APIs |
| Read params, time, the local clock | Network, files outside its folder and the user's picked files |

Examples: **Clock** (worked in 4.9); **Logo bug** (a picked image in a corner with opacity). A shader-only example,
**Snowfall**, ships with the SDK.

### 2.3 Widget

The owner's word. Two readings:

- **A. Interactive overlay** with its own state and settings: a countdown, a "be right back" card, a live counter, a
  now-playing label. Other apps see it.
- **B. A custom panel inside Better Cam's window** (HTML the extension supplies).

**Designed for: A.** Reasons: the brief says extensions must show on the virtual camera; every example the owner gave
(timer, BRB card, counter, now-playing) is something viewers see; B needs untrusted HTML in the app's privileged
webview, which is a security design of its own (open question 1). In this design a widget is an overlay plus:

| Can | Cannot (v1) |
|---|---|
| Everything an overlay can | Its own HTML panel in the app (reading B) |
| Declare actions: buttons on its card ("Start", "Reset") | Global hotkeys (open question 6) |
| Keep state that survives a restart (JSON, ≤ 64 KB) | Network (so no live chat box, no web now-playing) |
| Write a status line on its card ("4:12 left") | Read audio or system media sessions (open question 5) |

Examples: **Countdown timer** (worked in 4.10); **Be right back** (a toggle action that covers the frame with a message
and the time since it was switched on).

---

## 3. Options

Context for all costs: the measured pipeline on the owner's PC (RTX 5080) is 0.26 ms GPU for four filters at
1080p60, readback 0.33–0.55 ms, header latency p95 2.44–2.64 ms **(M)**. One built-in shader pass adds
0.007–0.039 ms at 1080p **(M)** (Color +0.007, LUT +0.014, Chroma Key +0.026, Sharpen +0.039).

### 3.1 Filters

| # | Option | How it works | Latency / cost | Risks | Effort |
|---|---|---|---|---|---|
| **F1** | **WGSL → `naga` → HLSL → `D3DCompile` → existing D3D11 chain** | The author writes a WGSL fragment shader. The app adds a generated prelude (bindings, `Params` struct from the manifest), validates with `naga`, writes HLSL with `naga::back::hlsl` (`ShaderModel::V5_0`, explicit `binding_map`), compiles with `D3DCompile` `ps_5_0` on a loader thread, and runs it as one more `GpuFilter`. A `.hlsl` file is accepted too and skips `naga` (escape hatch for OBS shader-filter ports; open question 8) | Same as a built-in pass: ~0.01–0.04 ms per pass at 1080p for simple shaders (E, from the M per-pass numbers). Compile 20–300 ms off the render thread (E). 0 added latency | FXC (`D3DCompile`) is "old, slow and unmaintained" and wgpu switched its own default to DXC in Feb 2026, not fixing FXC issues (S: wgpu `Dx12Compiler` docs, issue #8363 closed by #8882). D3D11 only accepts DXBC, so FXC is the only compiler. Future `naga` releases may emit HLSL that FXC rejects → pin `naga`, keep a corpus test. Uniform layout must match WGSL rules **(unverified, spike E1)** | M (2 sessions after the spike) |
| F2 | A real WebGPU device in the pipeline (`wgpu` on D3D12) sharing textures with the D3D11 engine | D3D11 texture with `D3D11_RESOURCE_MISC_SHARED_NTHANDLE`, opened in D3D12 with `OpenSharedHandle`, wrapped with `wgpu::Device::create_texture_from_hal` (S: wgpu docs; third-party code in Slint's Servo example and `wgpu-native-texture-interop`). Two cross-API syncs per frame (keyed mutex or shared fence) | +0.1–0.5 ms per frame for syncs and copies (E); +1 D3D12 device (~50–150 MB, E); binary +5–10 MB (E). Still no WebGPU *JS* API | Two devices to recover after TDR; adapter must match; every edge custom (plan 4.2 already rejected wgpu for this reason; wgpu has no D3D11 backend since 0.19, S) | L (4–6 sessions) |
| F2b | Replace D3D11 with `wgpu` for the whole engine | — | Unknown until rebuilt | Media Foundation decoders and NVIDIA Video Effects speak D3D11 (plan 4.2); every interop edge reopens | XL; reopens a closed decision |
| F3 | Filter in the webview (WebGPU on a canvas) | Full frames to the webview, processed with WebGPU, sent back | 1080p RGBA is 8.3 MB a frame: ~1 GB/s both ways at 60 fps through IPC (E, arithmetic); latency 2–4 frames, 35–70 ms (E) vs 2.5 ms p95 now (M) | Chromium stops `requestAnimationFrame` and throttles timers for hidden pages (S: MDN, Chrome blog); WebView2 throttles when `IsVisible` is false (S: Microsoft Learn). Breaks the rule "virtual camera independent of the window". The only zero-copy path, `ICoreWebView2ExperimentalTextureStream`, is still experimental and prerelease-only (S: Microsoft Learn, prerelease 1.0.1724 through 1.0.4015); one report of `ERROR_NOT_SUPPORTED` (S: third-party repo) | L, and fails a hard rule |

**Pick: F1.** Reasons: zero added latency and the same cost as
built-ins; no new device, no bridge, no window dependency; WGSL is the WebGPU shading language, so the "WebGPU"
part of the wish is met where it can be met. The FXC risk is contained by pinning `naga` and the corpus test in E1.

### 3.2 Overlays and widgets

| # | Option | How it works | Latency / cost | Risks | Effort |
|---|---|---|---|---|---|
| O4 | Offscreen WebView2 "browser layer" (like OBS's browser source) | `ICoreWebView2CompositionController` attached to a `Windows.UI.Composition` visual; `GraphicsCaptureItem.CreateFromVisual` (Windows 10 1809+, S) + `Direct3D11CaptureFramePool` gives D3D11 textures with alpha; copy into a shared texture; engine composites | 1–3 frames behind (E); Microsoft says its own capture-based WebView2 control has "lower framerates" (S: WPF `WebView2CompositionControl` spec); +100–300 MB RAM and a few % CPU per page (E, unverified) | No supported offscreen rendering in WebView2 (S: feature request #547/#20 still open); Tauri/wry has no offscreen API (S: wry #391 open); capture needs the visual in a live composition tree, so a host window must exist; whether a hidden or cloaked one keeps rendering at full rate is **unverified** (a third-party project notes hidden pages can pause animation callbacks); WinRT composition + capture plumbing from Rust; network must be blocked by hand | L (spike + 3 sessions) |
| **O5** | **Native layer API: scenes or WGSL shader layers** | Scene mode: a script returns a scene (JSON-like nodes); the render thread draws it with Direct2D/DirectWrite into a BGRA layer texture on the engine's own D3D11 device (it has `BGRA_SUPPORT`), only when the scene changed. Shader mode: a WGSL fragment shader (F1's compile path) draws the layer. Both are blended over the output frame | Raster 0.1–0.5 ms CPU for a typical scene, 1–3 ms for the cap (E, **spike E2 measures**); shader layer ≈ one filter pass (E); blend ~0.01–0.03 ms GPU per layer (E); script tick 0.05–1 ms on its own thread (E); 0 added latency | D2D shares the immediate context and may leave D3D state set **(unverified, E2 checks)**; text and fonts limited to DirectWrite; a new drawing vocabulary for authors | M (3–4 sessions) |
| O6 | Shader-only overlays | WGSL layers and nothing else | As a filter pass (E) | Text in shaders is impractical: no clock, no label, no timer | S |
| O7 | Skia (`skia-safe`) or `tiny-skia` raster | CPU raster of a canvas-like API, upload each change | 1080p layer upload ~8 MB per change; 1–5 ms CPU raster (E) | GPU Skia has no D3D11 backend (E); +~20 MB binary (E) | M |
| O8 | Overlays drawn in the app's own webview | DOM/Canvas in the main window, captured | Same as F3 | Same window dependency as F3 | — |

**Pick: O5** (its shader mode covers what O6 offers). Reasons: native, no extra process, no window, cost in
the tenths of a millisecond; covers every listed example except ones that need network (chat box). O4 is the only
way to get full HTML/CSS/WebGPU overlays; it is recorded as a later spike (open question 2), not v1, because offscreen
WebView2 is unsupported by Microsoft today and its cost and hidden-window behaviour are unknown.

Threading inside O5: raster on the render thread, bounded by caps (6.3) and a measured budget; unchanged scenes are
never re-rastered. Fallback if spike E2 fails: an overlay thread with its own D3D11 device and D2D, double-buffered
`SHARED_NTHANDLE | SHARED_KEYEDMUTEX` textures, render thread `AcquireSync(key, 0)` and reuse of the previous layer
when it is busy. Not the default because it doubles device-loss handling. D2D on the engine device from *another*
thread is ruled out: with multithread protection on, the render thread would block on the device lock while D2D
draws **(unverified detail; follows from `ID3D11Multithread` semantics)**.

### 3.3 Script runtime

| # | Option | How it works | Cost | Risks | Effort |
|---|---|---|---|---|---|
| **R1** | **QuickJS-NG via `rquickjs`, one runtime and thread per card** | TS stripped by `oxc_transformer` at load; ES module evaluated in its own `Runtime`; host API as a built-in `better-cam` module | ~210 KiB x86 code for the engine and runtime start-up under 300 µs (S: rquickjs README); interpreter, no JIT: hot loops 10–50× slower than V8 (E) | In-process C engine: a QuickJS memory bug could crash the app (E, low likelihood); `set_memory_limit` is a no-op with the `rust-alloc` feature (S: docs.rs), so that feature stays off; interrupt handler is "regularly called" (S) but its granularity inside native calls (big regex) is **unverified** | M |
| R2 | V8 (`deno_core` / `rusty_v8`) | Isolates with heap limits; watchdog calls `TerminateExecution` | Fast JIT; +~30 MB binary and a prebuilt static library at build (E, unverified) | Heavier build; more surface than needed for tens of lines of logic | M–L |
| R3 | Boa (pure Rust) | Same shape as R1 | No C code; slower than QuickJS (E) | Conformance and limit APIs **unverified** | M |
| R4 | The app's main webview | Scripts run beside the React UI | Free TS, DOM, WebGPU | Throttled or paused when hidden or minimized (S); a hang freezes the UI; breaks "virtual camera independent of the window" | — (rejected) |
| R5 | A hidden second webview | Same, separate window | +100–300 MB (E); IPC per scene 1–5 ms (E) | Hidden pages throttled (S); belongs to the O4 later spike | M |
| R6 | WASM (`wasmtime`) | Authors compile to WASM; fuel metering | +~10 MB (E) | Authors write AssemblyScript or Rust, not real TypeScript | M |

**Pick: R1.** Reasons: real TypeScript (types stripped, not a dialect), small, starts per card in microseconds, and
the two limits that matter (memory and time) are built in (S). Logic in extensions is small; JIT speed is not needed.

TypeScript handling: `.ts` files are transpiled in the app with the `oxc` crate (`transformer` + `codegen`
features; TypeScript stripping is supported in the Rust crate, S: oxc.rs, Rolldown uses it). Authors need no
toolchain. Plain `.js` also works. Source maps from `oxc_codegen` map error lines back to the `.ts` file.
Binary size impact of `oxc` **(unverified)**; fallback if too large: require `.js` and ship an `esbuild` recipe.

### 3.4 Verified and unverified facts

Verified against primary sources: `naga::back::hlsl` supports shader models 5.0, 5.1 and 6.0 and takes a
`binding_map` (`fake_missing_bindings` defaults to true); wgpu calls FXC "old, slow and unmaintained" and made DXC its
default (issue #8363, Feb 2026); `rquickjs` binds QuickJS-NG and offers `set_memory_limit` (no-op with
`rust-alloc`), `set_interrupt_handler`, `set_max_stack_size` and a `loader` feature; WebView2 TextureStream is
experimental and prerelease-only; WebView2 offscreen rendering is an open request (#547, #20); `CreateFromVisual`
exists since Windows 10 1809 and Microsoft's own composition-hosted WebView2 control captures with
`GraphicsCaptureSession` at "lower framerates"; wry has no offscreen rendering (#391); Chromium stops
`requestAnimationFrame` and throttles timers in hidden pages, and WebView2 throttles when `IsVisible` is false;
`wgpu::Device::create_texture_from_hal` with `OpenSharedHandle` imports D3D11 NT-handle textures (third-party code
plus Microsoft's `OpenSharedResource1` docs); `oxc_transformer` strips TypeScript in Rust; the GPL FAQ's position on
interpreters with bindings.

Unverified:

1. `naga` HLSL output for the prelude compiles with FXC `ps_5_0`, and its cbuffer layout matches WGSL uniform layout (E1).
2. Future `naga` releases keep FXC-compatible output (wgpu no longer fixes FXC issues; pin and test).
3. Direct2D on the engine device leaves D3D11 state usable and costs ≤ 0.5 ms for typical scenes (E2).
4. D2D from another thread on a multithread-protected device would block the render thread (reasoned, not tested).
5. `rquickjs` interrupt granularity inside long native calls.
6. QuickJS-NG lacks `Intl`; how its `Date` applies the Windows time zone.
7. Binary size added by `oxc`, `naga` and `rquickjs`.
8. Offscreen WebView2 capture keeps full rate with a hidden or cloaked host window; its CPU and memory cost.
9. V8 and Boa sizes, speeds and limit APIs (R2, R3).
10. Every cost tagged (E).

---

## 4. The API

### 4.1 Folder layout and packaging

```
%APPDATA%\dev.bettercam.app\extensions\
├─ .sdk\                        copied by the app at start when its version differs: better-cam.d.ts,
│                               extension.schema.json, prelude.wgsl (read-only reference), tsconfig.base.json
├─ vhs\                         one folder per extension; folder name == manifest "id"
│  ├─ extension.json
│  ├─ vhs.wgsl
│  └─ glitch.ts
└─ clock\ …
```

- Install: copy a folder, or Settings → Extensions → "Install from file…" with a `.zip` (same layout, one top folder).
  Zip entries that leave the target folder are refused; total ≤ 50 MB.
- Listing: the app scans the folder at start and polls it once a second (modification time and size of every file a
  manifest names, as `fx/assets.rs` already does for images). A change reloads that extension (hot reload).
- Enable/disable per extension in Settings → Extensions. A disabled or missing extension keeps its cards in the chain
  and in presets, bypassed, with a line on the card.
- API versioning: `apiVersion` (integer) in the manifest. The app supports a list (v1: `[1]`). An unknown version is
  refused with "Needs a newer Better Cam (extension API 2)". Additive changes keep the number; breaking ones bump it.

### 4.2 Manifest (`extension.json`)

| Field | Type | Kinds | Rules |
|---|---|---|---|
| `$schema` | string | all | Optional; `"../.sdk/extension.schema.json"` gives editor completion |
| `apiVersion` | integer | all | Required; `1` |
| `id` | string | all | `^[a-z0-9][a-z0-9-]{1,47}$`, equal to the folder name. Card kind becomes `ext:<id>` |
| `name`, `version`, `author`, `license` | string | all | `version` is semver; `license` an SPDX id (shown, not enforced) |
| `kind` | `"filter"` \| `"overlay"` \| `"widget"` | all | |
| `hint` | string | all | One line for the Add menu (≤ 80 chars) |
| `shader` | path | filter; overlay/widget with `render: "shader"` | `.wgsl`, or `.hlsl` (open question 8). ≤ 256 KB |
| `script` | path | optional for filter; required for `render: "scene"` | `.ts` or `.js`, ES module, default export from `defineFilter` / `defineOverlay` / `defineWidget` |
| `render` | `"scene"` \| `"shader"` | overlay, widget | Widgets: `"scene"` only in v1 |
| `fps` | number | overlay, widget; filter script | Script tick rate. Scene: 0 (only on change) to 30, default 0. Filter `uniforms()`: at most the output rate, default output rate |
| `history` | boolean | filter | Bind the previous output of this card at `history` |
| `params` | `ParamDef[]` | all | Same JSON as `src/filters/schema.ts` (slider, select, bool, int, color, path, combo, `showIf`, `hint`) plus `text`. `combo.pattern` is a string. `path.filter` must be `"image"`. ≤ 32 params |
| `uniforms` | `{key, type}[]` | filter, shader layer with script | Values a script computes. `type`: `f32`, `i32`, `u32`, `vec2f`, `vec4f`. ≤ 16 |
| `actions` | `{id, label}[]` | widget | Buttons on the card, in order. ≤ 6. `id` `^[a-z][a-z0-9_]{0,23}$` |
| `permissions` | string[] | all | Must be empty in v1. Any entry refuses the load ("Needs permission X, not supported") |

New param type for scripts:

```ts
interface TextDef extends ParamBase { type: "text"; def: string; maxLength?: number /* ≤ 200, default 80 */ }
```

### 4.3 Shader conventions

The app prepends a generated prelude. Authors do not declare bindings. Reference copy: `.sdk/prelude.wgsl`.

```wgsl
// ---- generated by Better Cam, extension API 1 ----
struct Frame {
  size: vec2<f32>,       // pixels of this pass's target (filter: its input size; layer: output size)
  inv_size: vec2<f32>,   // 1 / size
  canvas: vec2<f32>,     // virtual camera output size
  time: f32,             // seconds since the camera session started
  delta: f32,            // seconds since the previous frame
  frame: u32,            // frame counter of the session
  seed: f32,             // uniform random 0..1, new every frame
  _pad: vec2<f32>,
};
struct Params {          // one member per param and per manifest uniform, in manifest order
  // slider -> f32 · int -> i32 · bool -> u32 (0/1) · select -> f32 (numeric options) or u32 (index)
  // color -> vec4<f32> (straight RGBA, 0..1, as picked) · uniforms -> their declared type
  // path, text, combo -> not in Params (path images are textures below)
};
@group(0) @binding(0) var<uniform> frame: Frame;
@group(0) @binding(1) var<uniform> params: Params;
@group(0) @binding(2) var image: texture_2d<f32>;     // filter: the picture so far. Layer: 1x1 transparent
@group(0) @binding(3) var history: texture_2d<f32>;   // previous output of this card ("history": true), else 1x1
// @group(0) @binding(4..7) var <path param key>: texture_2d<f32>;   one per `path` param, 1x1 transparent until loaded
@group(0) @binding(8) var smp: sampler;               // linear filtering, clamp to edge

fn tex(t: texture_2d<f32>, uv: vec2<f32>) -> vec4<f32> { return textureSampleLevel(t, smp, uv, 0.0); }
fn texel(t: texture_2d<f32>, p: vec2<i32>) -> vec4<f32> { return textureLoad(t, p, 0); }
fn to_linear(c: vec3<f32>) -> vec3<f32> { /* sRGB EOTF */ }
fn to_srgb(c: vec3<f32>) -> vec3<f32> { /* inverse */ }
fn luma(c: vec3<f32>) -> f32 { return dot(c, vec3<f32>(0.2126, 0.7152, 0.0722)); }
fn hash(p: vec2<f32>) -> f32 { return fract(sin(dot(p, vec2<f32>(12.9898, 78.233))) * 43758.5453); }
// ---- end of prelude ----
```

The author writes one entry point:

```wgsl
@fragment
fn main(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> { ... }
```

| Topic | Rule |
|---|---|
| Coordinates | Origin top-left, +y down. `pos.xy` is the pixel centre (x + 0.5), as in every built-in shader. `uv = pos.xy * frame.inv_size` |
| Colour | Input and output are gamma-encoded sRGB in `RGBA8_UNORM` (the engine's working format). Convert with `to_linear` / `to_srgb` for maths that needs light |
| Alpha | Filters: straight alpha in and out; transparent pixels show the output background (as built-in keys do). Shader layers: return straight alpha; the compositor premultiplies |
| Sampling | Use `tex()` (explicit LOD). `textureSample` with implicit derivatives is allowed only in uniform control flow (WGSL rule) |
| Register mapping (HLSL side) | `frame` b0, `params` b1, textures t0–t5, `smp` s0. `naga` `binding_map` sets this; `fake_missing_bindings` off so a mistake fails at load |
| Forbidden | Storage buffers and storage textures, `@workgroup`, other entry points, other `@group`/`@binding` declarations: refused with a message |
| Errors | `naga` and FXC messages are reported as `vhs.wgsl:14:9: message`, lines counted in the author's file (prelude excluded) |
| Uniform layout | The host packs `Params` with the member offsets `naga` computes for the generated struct; HLSL must agree (E1 proves it) |

Raw HLSL (`.hlsl` file): `float4 main(float4 pos : SV_Position) : SV_Target` with the same registers; the app prepends an HLSL
prelude with `cbuffer Frame : register(b0)` and a generated `cbuffer Params : register(b1)` (packed with HLSL rules).

### 4.4 TypeScript API (`better-cam.d.ts` sketch)

```ts
// better-cam.d.ts: Better Cam extension API 1. Licensed MIT (see open question 3).
declare module "better-cam" {
  export const apiVersion: 1;

  export type ParamValue = number | string | boolean;
  export type Params = Record<string, ParamValue>;

  export interface FrameInfo {
    /** Seconds since the camera session started (monotonic). Same clock shaders see as `frame.time`. */
    readonly time: number;
    /** Seconds since this script's previous tick. */
    readonly delta: number;
    /** Virtual camera output size in pixels (overlay coordinates use it). */
    readonly width: number;
    readonly height: number;
    /** Output frames per second. */
    readonly fps: number;
  }

  export interface LocalTime {
    year: number; month: number; day: number; weekday: number; // month 1-12, weekday 0 = Sunday
    hour: number; minute: number; second: number; millisecond: number;
    /** Milliseconds since the Unix epoch (UTC). Use it for anything that must survive a restart. */
    epochMs: number;
    utcOffsetMinutes: number;
  }

  /** Per card, JSON values, at most 64 KB in total; saved by the app about once a second. */
  export interface Storage {
    get<T = unknown>(key: string): T | undefined;
    set(key: string, value: unknown): void;
    delete(key: string): void;
  }

  export interface Context<P extends Params = Params> {
    /** The card id (stable across restarts and presets). */
    readonly id: string;
    readonly params: Readonly<P>;
    readonly frame: FrameInfo;
    /** The local wall clock, read from Windows on each call. */
    clock(): LocalTime;
    readonly storage: Storage;
    /** One line on the card; "" clears it. Identical lines are not re-sent; at most 4 changes a second. */
    status(text: string): void;
    /** Lines in Settings → Extensions → Log (at most 20 a second per card). */
    log(...values: unknown[]): void;
    /** Overlays and widgets: call render() again at the next tick even if nothing changed. */
    requestRender(): void;
    /** Size of a text run as DirectWrite lays it out. */
    measureText(text: string, font?: Font): { width: number; height: number; baseline: number };
  }

  // Timers run on the card's own thread, between calls.
  export function setTimeout(fn: () => void, ms: number): number;
  export function setInterval(fn: () => void, ms: number): number;
  export function clearTimeout(id: number): void;
  export function clearInterval(id: number): void;

  // ---- filters ----
  export type UniformValue = number | boolean | [number, number] | [number, number, number, number];
  export interface FilterScript<P extends Params = Params> {
    init?(ctx: Context<P>): void;
    /** After a parameter change; `changed` lists the keys. */
    params?(ctx: Context<P>, changed: (keyof P & string)[]): void;
    /** Up to once per output frame. Returns values for the manifest's `uniforms`; missing keys keep their last value. */
    uniforms?(ctx: Context<P>): Partial<Record<string, UniformValue>>;
    dispose?(ctx: Context<P>): void;
  }
  export function defineFilter<P extends Params>(script: FilterScript<P>): FilterScript<P>;

  // ---- overlays ----
  export interface OverlayScript<P extends Params = Params> {
    init?(ctx: Context<P>): void;
    params?(ctx: Context<P>, changed: (keyof P & string)[]): void;
    /** Called on each tick (`fps`), after a param change and after requestRender(). A scene equal to the previous
     *  one is not drawn again. (A shader layer's optional script uses defineFilter and its uniforms() instead.) */
    render(ctx: Context<P>): Scene;
    dispose?(ctx: Context<P>): void;
  }
  export function defineOverlay<P extends Params>(script: OverlayScript<P>): OverlayScript<P>;

  // ---- widgets ----
  export interface WidgetScript<P extends Params, S extends object> {
    /** The state when the card has none saved yet. */
    init(ctx: Context<P>): S;
    params?(ctx: Context<P>, state: S, changed: (keyof P & string)[]): S | void;
    /** A button from the manifest's `actions` was pressed. Return the new state (saved), or nothing to keep it. */
    action?(ctx: Context<P>, state: S, action: string): S | void;
    render(ctx: Context<P>, state: S): Scene;
    dispose?(ctx: Context<P>, state: S): void;
  }
  export function defineWidget<P extends Params, S extends object>(script: WidgetScript<P, S>): WidgetScript<P, S>;

  // ---- scene ----
  /** "#rgb", "#rrggbb", "#rrggbbaa", or [r, g, b, a?] with 0-255 channels and alpha 0-1. sRGB. */
  export type Color = string | [number, number, number] | [number, number, number, number];
  /** A `color` param (OBS int 0xAABBGGRR) as a Color. */
  export function fromObsColor(value: number): Color;

  export interface Font { family?: string /* default "Segoe UI" */; size?: number /* px, default 32 */;
                          weight?: number /* 100-900, default 400 */; italic?: boolean }
  export interface Stroke { color: Color; width: number }
  interface NodeBase { opacity?: number /* 0-1 */ }
  export interface RectNode extends NodeBase { type: "rect"; x: number; y: number; w: number; h: number;
                                               radius?: number; fill?: Color; stroke?: Stroke }
  export interface EllipseNode extends NodeBase { type: "ellipse"; cx: number; cy: number; rx: number; ry: number;
                                                  fill?: Color; stroke?: Stroke }
  export interface LineNode extends NodeBase { type: "line"; x1: number; y1: number; x2: number; y2: number; stroke: Stroke }
  export interface TextNode extends NodeBase {
    type: "text"; text: string; x: number; y: number;          // (x, y) is the anchor point
    align?: "left" | "center" | "right"; valign?: "top" | "middle" | "bottom";
    font?: Font; color?: Color; maxWidth?: number;               // wraps at maxWidth when set
    shadow?: { color: Color; dx: number; dy: number };
    box?: { fill?: Color; stroke?: Stroke; radius?: number; padding?: number | [number, number] };
  }
  export interface ImageNode extends NodeBase {
    type: "image"; src: string;      // "./assets/x.png" inside the extension folder, or the value of a `path` param
    x: number; y: number; w?: number; h?: number; fit?: "fill" | "contain" | "cover";
    sprite?: { columns: number; rows: number; index: number };   // pick one cell of a sprite sheet
  }
  export interface GroupNode extends NodeBase {
    type: "group"; x?: number; y?: number; rotate?: number /* degrees */; scale?: number | [number, number];
    clip?: { x: number; y: number; w: number; h: number; radius?: number };
    children: Node[];
  }
  export type Node = RectNode | EllipseNode | LineNode | TextNode | ImageNode | GroupNode;
  export interface Scene { nodes: Node[] }
}
```

Globals present: ES2020 built-ins of QuickJS-NG (`Math`, `JSON`, `Date`, `Map`, `Promise`…). Absent: `fetch`,
`XMLHttpRequest`, `WebSocket`, `require`, `process`, file APIs, `Intl` **(absence of `Intl` unverified)**, DOM.
Whether QuickJS-NG's `Date` applies the Windows time zone is **unverified**; `ctx.clock()` is the supported way.
Module-level variables are per card (each card has its own runtime). Imports: relative paths inside the extension
folder and `"better-cam"`; anything else fails to load.

### 4.5 Lifecycle

```
registry (loader thread)   manifest → validate → transpile .ts (oxc) → WGSL→HLSL (naga) → D3DCompile → CreatePixelShader
                           → "ready" | "error" (Settings → Extensions)
card added / preset loaded ─► Starting: script evaluated, init() (≤ 500 ms); shader taken from the registry
                              │ until ready the card passes the picture through (filter) or draws nothing (layer)
                              ▼
                           Running ── param change ──► params(ctx, changed)
                              │──── tick (fps) ───────► uniforms() | render()      (filter shader: every frame, GPU)
                              │──── card button ──────► action(ctx, state, id)
                              │──── 3 failures in 10 s ► Stopped (card error + "Restart" button)
                              ▼
card removed / extension reloaded / app quits ─► dispose() (≤ 100 ms) ─► runtime dropped, thread ends
```

- Hot reload: a changed file recompiles on the loader thread. Cards keep the old shader and script until the new
  version is ready, then restart with the same params and storage. A failed reload keeps the old version running and
  shows the error.
- Ticks run only while the engine renders (Live). Actions and param changes are handled also while stopped.
- Scenes and uniforms are posted to a mailbox; the render thread takes the newest at its next frame.
- The render loop is driven by camera frames: overlays animate at the camera's rate (14.4 fps in the owner's room,
  M) when that is lower than the output rate (open question 7).

### 4.6 What an extension receives (v1)

| Signal | Shader | Script | v1 |
|---|---|---|---|
| Input picture (filter) | yes | no | yes |
| Own previous output | `history` | no | yes |
| Frame size, output size, fps | yes | yes | yes |
| Session time, delta, frame count, random seed | yes | time, delta | yes |
| Local wall clock | no | `ctx.clock()` | yes |
| Params | `params` | `ctx.params` | yes |
| User-picked images | textures | as `src` in scenes | yes |
| Script-computed uniforms | `params.<key>` | returns them | yes |
| Card actions, persistent storage | no | widgets | yes |
| Audio level (microphone) | — | — | no (open question 5) |
| System media "now playing" | — | — | no (open question 5) |
| Network, files, clipboard, hotkeys | — | — | no |

### 4.7 Engine-side types (sketch for builders)

```rust
// src-tauri/src/ext/manifest.rs
pub struct Manifest { pub api_version: u32, pub id: String, pub name: String, pub version: String,
                      pub kind: ExtKind, pub render: Option<RenderMode>, pub shader: Option<PathBuf>,
                      pub script: Option<PathBuf>, pub fps: f32, pub history: bool,
                      pub params: Vec<ParamDef>, pub uniforms: Vec<UniformDef>, pub actions: Vec<ActionDef> }

// src-tauri/src/ext/mod.rs
pub struct ExtRegistry { /* Arc<Snapshot> swapped on change */ }
pub struct Compiled { pub ps: ID3D11PixelShader, pub params_layout: Vec<(String, u32 /*offset*/, UniformType)>,
                      pub params_size: usize, pub image_keys: Vec<String> }
impl ExtRegistry { pub fn snapshot(&self) -> Arc<Snapshot>; pub fn make_filter(&self, kind: &str) -> Option<Box<dyn GpuFilter>>; }

// src-tauri/src/ext/script/host.rs
pub struct ScriptHandle { /* drop = stop; never joins on the caller */ }
impl ScriptHandle {
    pub fn latest_uniforms(&self) -> Option<Arc<[u8]>>;   // try_lock; None when busy or nothing new
    pub fn latest_scene(&self) -> Option<Arc<Scene>>;      // try_lock; None when busy or nothing new
    pub fn send(&self, ev: HostEvent);                     // Params / Action / Tick; bounded queue, drops oldest Tick
}
```

### 4.8 Worked example: filter `vhs`

```
extensions\vhs\
├─ extension.json
├─ vhs.wgsl
└─ glitch.ts
```

`extension.json`

```json
{
  "$schema": "../.sdk/extension.schema.json",
  "apiVersion": 1,
  "id": "vhs",
  "name": "VHS",
  "version": "1.0.0",
  "author": "Better Cam examples",
  "license": "MIT",
  "kind": "filter",
  "hint": "Scanlines, colour bleed and tape noise",
  "shader": "vhs.wgsl",
  "script": "glitch.ts",
  "params": [
    { "type": "slider", "key": "scanlines", "label": "Scanlines", "min": 0, "max": 1, "step": 0.01, "def": 0.4 },
    { "type": "slider", "key": "bleed", "label": "Colour bleed", "min": 0, "max": 8, "step": 0.1, "def": 2, "unit": "px" },
    { "type": "slider", "key": "noise", "label": "Noise", "min": 0, "max": 1, "step": 0.01, "def": 0.15 },
    { "type": "bool", "key": "glitches", "label": "Random glitches", "def": true },
    { "type": "slider", "key": "rate", "label": "Glitches per minute", "min": 1, "max": 30, "step": 1, "def": 6,
      "int": true, "showIf": { "key": "glitches", "in": [true] } }
  ],
  "uniforms": [{ "key": "glitch", "type": "f32" }]
}
```

`vhs.wgsl` (generated `Params`: `scanlines: f32, bleed: f32, noise: f32, glitches: u32, rate: f32, glitch: f32`)

```wgsl
@fragment
fn main(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
  var uv = pos.xy * frame.inv_size;

  // Horizontal tearing in thin bands while the script reports a glitch.
  let band = step(0.97, fract(uv.y * 7.0 + frame.time * 3.0));
  let jitter = hash(vec2<f32>(floor(uv.y * 90.0), frame.time)) - 0.5;
  uv.x = uv.x + params.glitch * band * 0.03 * jitter;

  // Colour bleed: red and blue sampled a few pixels apart.
  let dx = vec2<f32>(params.bleed * frame.inv_size.x, 0.0);
  let base = tex(image, uv);
  var rgb = vec3<f32>(tex(image, uv + dx).r, base.g, tex(image, uv - dx).b);

  // Scanlines on every other output row, then grain that changes every frame.
  let line = 0.5 + 0.5 * sin(pos.y * 3.14159265);
  rgb = rgb * (1.0 - params.scanlines * 0.35 * line);
  rgb = rgb + (hash(pos.xy + vec2<f32>(frame.seed * 1000.0, 0.0)) - 0.5) * params.noise * 0.25;

  return vec4<f32>(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)), base.a);
}
```

`glitch.ts`

```ts
import { defineFilter } from "better-cam";

type P = { scanlines: number; bleed: number; noise: number; glitches: boolean; rate: number };

let next = -1;  // session time (s) of the next glitch
let until = 0;  // session time (s) the current glitch ends

export default defineFilter<P>({
  uniforms(ctx) {
    const t = ctx.frame.time;
    if (!ctx.params.glitches) return { glitch: 0 };
    const meanGap = 60 / ctx.params.rate;
    if (next < 0 || next > t + 4 * meanGap) next = t + Math.random() * 2 * meanGap;
    if (t >= next) {
      until = t + 0.15 + Math.random() * 0.35;
      next = t + Math.random() * 2 * meanGap;
    }
    return { glitch: t < until ? 1 : 0 };
  },
});
```

### 4.9 Worked example: overlay `clock`

```
extensions\clock\
├─ extension.json
└─ clock.ts
```

`extension.json`

```json
{
  "$schema": "../.sdk/extension.schema.json",
  "apiVersion": 1,
  "id": "clock",
  "name": "Clock",
  "version": "1.0.0",
  "author": "Better Cam examples",
  "license": "MIT",
  "kind": "overlay",
  "render": "scene",
  "hint": "The local time in a corner",
  "script": "clock.ts",
  "fps": 2,
  "params": [
    { "type": "select", "key": "corner", "label": "Corner", "def": "top-right",
      "options": [
        { "value": "top-left", "label": "Top left" }, { "value": "top-right", "label": "Top right" },
        { "value": "bottom-left", "label": "Bottom left" }, { "value": "bottom-right", "label": "Bottom right" }
      ] },
    { "type": "select", "key": "format", "label": "Format", "def": "24h",
      "options": [{ "value": "24h", "label": "24 hour" }, { "value": "12h", "label": "12 hour" }] },
    { "type": "bool", "key": "seconds", "label": "Show seconds", "def": false },
    { "type": "slider", "key": "size", "label": "Size", "min": 16, "max": 160, "step": 1, "def": 40, "int": true, "unit": "px" },
    { "type": "int", "key": "margin", "label": "Margin", "min": 0, "max": 400, "def": 32, "unit": "px" },
    { "type": "color", "key": "color", "label": "Text", "def": 4294967295 },
    { "type": "color", "key": "background", "label": "Background", "def": 2147483648, "alpha": true }
  ]
}
```

(`4294967295` = `0xFFFFFFFF` opaque white; `2147483648` = `0x80000000` black at 50 %; OBS colour ints.)

`clock.ts`

```ts
import { defineOverlay, fromObsColor, type Scene } from "better-cam";

type P = { corner: string; format: string; seconds: boolean; size: number; margin: number; color: number; background: number };

const two = (n: number) => String(n).padStart(2, "0");

export default defineOverlay<P>({
  render(ctx): Scene {
    const p = ctx.params;
    const t = ctx.clock();
    let hour = String(t.hour).padStart(2, "0");
    let suffix = "";
    if (p.format === "12h") {
      suffix = t.hour < 12 ? " AM" : " PM";
      hour = String(t.hour % 12 || 12);
    }
    const text = `${hour}:${two(t.minute)}${p.seconds ? ":" + two(t.second) : ""}${suffix}`;
    const right = p.corner.endsWith("right");
    const bottom = p.corner.startsWith("bottom");
    return {
      nodes: [
        {
          type: "text",
          text,
          x: right ? ctx.frame.width - p.margin : p.margin,
          y: bottom ? ctx.frame.height - p.margin : p.margin,
          align: right ? "right" : "left",
          valign: bottom ? "bottom" : "top",
          font: { family: "Segoe UI", size: p.size, weight: 600 },
          color: fromObsColor(p.color),
          box: { fill: fromObsColor(p.background), padding: [p.size * 0.2, p.size * 0.4], radius: p.size * 0.25 },
        },
      ],
    };
  },
});
```

Ticks twice a second; the scene only changes once a minute (or once a second with seconds on), so it is rasterized
that often and blended every frame.

### 4.10 Worked example: widget `countdown`

```
extensions\countdown\
├─ extension.json
└─ countdown.ts
```

`extension.json`

```json
{
  "$schema": "../.sdk/extension.schema.json",
  "apiVersion": 1,
  "id": "countdown",
  "name": "Countdown",
  "version": "1.0.0",
  "author": "Better Cam examples",
  "license": "MIT",
  "kind": "widget",
  "render": "scene",
  "hint": "A timer viewers can see; start and pause it from its card",
  "script": "countdown.ts",
  "fps": 4,
  "params": [
    { "type": "int", "key": "minutes", "label": "Minutes", "min": 1, "max": 180, "def": 5 },
    { "type": "text", "key": "label", "label": "Label", "def": "Back in", "maxLength": 40 },
    { "type": "select", "key": "corner", "label": "Corner", "def": "bottom-left",
      "options": [
        { "value": "top-left", "label": "Top left" }, { "value": "top-right", "label": "Top right" },
        { "value": "bottom-left", "label": "Bottom left" }, { "value": "bottom-right", "label": "Bottom right" }
      ] },
    { "type": "slider", "key": "size", "label": "Size", "min": 16, "max": 160, "step": 1, "def": 48, "int": true, "unit": "px" },
    { "type": "color", "key": "color", "label": "Text", "def": 4294967295 },
    { "type": "color", "key": "background", "label": "Background", "def": 3003121664, "alpha": true },
    { "type": "bool", "key": "hide_when_idle", "label": "Hide while not running", "def": false }
  ],
  "actions": [
    { "id": "start", "label": "Start" },
    { "id": "pause", "label": "Pause" },
    { "id": "reset", "label": "Reset" }
  ]
}
```

(`3003121664` = `0xB3000000`, black at 70 %.)

`countdown.ts`

```ts
import { defineWidget, fromObsColor, type Scene } from "better-cam";

type P = { minutes: number; label: string; corner: string; size: number; color: number; background: number; hide_when_idle: boolean };
/** `endsAt` is wall-clock time, so a restart of the app does not lose time. */
type S = { endsAt: number | null; leftMs: number };

const full = (p: P) => p.minutes * 60_000;
const fmt = (ms: number) => {
  const s = Math.ceil(ms / 1000);
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`;
};

export default defineWidget<P, S>({
  init: (ctx) => ({ endsAt: null, leftMs: full(ctx.params) }),

  params(ctx, state, changed) {
    if (changed.includes("minutes") && state.endsAt === null) return { endsAt: null, leftMs: full(ctx.params) };
  },

  action(ctx, state, action) {
    const now = ctx.clock().epochMs;
    if (action === "start" && state.endsAt === null) return { endsAt: now + state.leftMs, leftMs: state.leftMs };
    if (action === "pause" && state.endsAt !== null) return { endsAt: null, leftMs: Math.max(0, state.endsAt - now) };
    if (action === "reset") return { endsAt: null, leftMs: full(ctx.params) };
  },

  render(ctx, state): Scene {
    const p = ctx.params;
    const running = state.endsAt !== null;
    const left = running ? Math.max(0, state.endsAt! - ctx.clock().epochMs) : state.leftMs;
    ctx.status(running ? (left > 0 ? `Running, ${fmt(left)} left` : "Finished") : `Paused at ${fmt(left)}`);
    if (!running && p.hide_when_idle) return { nodes: [] };
    const right = p.corner.endsWith("right");
    const bottom = p.corner.startsWith("bottom");
    const margin = Math.round(p.size * 0.6);
    return {
      nodes: [
        {
          type: "text",
          text: `${p.label} ${fmt(left)}`,
          x: right ? ctx.frame.width - margin : margin,
          y: bottom ? ctx.frame.height - margin : margin,
          align: right ? "right" : "left",
          valign: bottom ? "bottom" : "top",
          font: { size: p.size, weight: 700 },
          color: fromObsColor(p.color),
          box: { fill: fromObsColor(p.background), padding: [p.size * 0.25, p.size * 0.5], radius: p.size * 0.3 },
        },
      ],
    };
  },
});
```

The card shows Start, Pause and Reset under its title, the params below, and the status line ("Running, 3:12 left").
The host saves the returned state after every `action` and `params` call.

---

## 5. How it plugs into the existing code

### 5.1 Data flow and threads

```
 %APPDATA%\dev.bettercam.app\extensions\<id>\      (polled once a second)
        │
        ▼
 [ext loader thread]  manifest → validate → oxc (TS→JS) → naga (WGSL→HLSL) → D3DCompile → device.CreatePixelShader
        │  Arc<Snapshot> swap                                   │  extensions-changed ─────────────────────► UI
        ▼                                                       
 [engine control thread]  Msg::SetExtensions · Msg::SetChain · Msg::SetOverlays · Msg::ExtAction
        │  spawns / stops one script thread per extension card (never joins on the render thread)
        ▼
 [script thread × card]  QuickJS runtime: init · params · tick(fps) · action · timers
        │  latest-value mailboxes (uniforms bytes, Arc<Scene>), written with lock, read with try_lock
        ▼
 [render thread] per camera frame:
    1 input convert
    2 filter chain       built-ins + ExtShaderFilter (WGSL pass; Frame cbuffer b0, Params cbuffer b1)
    3 output stage       fit/fill/stretch over the background colour            (gpu/output.rs, unchanged)
    4 layer stack  NEW   for each overlay card, back to front:
                           scene layer: newest Scene? → D2D raster into its BGRA layer texture (only if changed)
                           shader layer: draw WGSL pass into its layer texture
                           blend layer over the output frame (premultiplied "over", in place)
    5 frame tap          preview (now shows overlays too)
    6 encode, readback → frames.bin → virtual camera
        │
 [preview sender] ─────► WebView2 canvas (unchanged)
```

Overlays sit after the output stage so their coordinates are output pixels, independent of camera crop, scale or
fit. The blend is the usual premultiplied "over" in gamma-encoded space (what browsers and Direct2D do), so text
edges look as authors expect. Filters keep the existing straight-alpha convention; the output stage still composites
translucent filter results over the background in linear light (`output_fit.hlsl`).

### 5.2 Rust modules

| File | Change |
|---|---|
| `src-tauri/src/ext/mod.rs` | New. `ExtRegistry`: scan, poll, snapshot, enable/disable, `make_filter(kind)`, `ExtensionInfo` for the UI |
| `src-tauri/src/ext/manifest.rs` | New. Serde types, validation (named field in every error), `ParamDef` mirror incl. `text`, clamping readers reused from `fx` (`pf`, `pi`, `pb`, `ps`, `pc`) |
| `src-tauri/src/ext/wgsl.rs` | New. Prelude and `Params` generation, `naga` parse + validate, layout offsets, HLSL write with `binding_map`, error line mapping. HLSL prelude for `.hlsl` files |
| `src-tauri/src/ext/compile.rs` | New. Loader thread jobs: transpile, translate, `gpu::compile_shader` (already `pub`, takes `&str`), `CreatePixelShader` (device is free-threaded) |
| `src-tauri/src/ext/filter.rs` | New. `ExtShaderFilter: GpuFilter`: two constant buffers, image params through `AssetHandle` (one handle per path param), `history` texture, per-pass GPU timer, uniforms from the script mailbox |
| `src-tauri/src/ext/script/{mod,host,api,transpile,limits}.rs` | New. One thread per card, `rquickjs` runtime (features `loader`; not `rust-alloc`), module resolver confined to the folder, `better-cam` module, timers, storage file, event queue, status/log rate limits |
| `src-tauri/src/ext/scene.rs` | New. Scene types from JS values, validation and caps, structural hash for "unchanged" |
| `src-tauri/src/gpu/layers.rs` | New. `LayerStack`: slots kept by id+kind like `Chain`; layer textures; `apply(cfg)`, `run(&fitted)` |
| `src-tauri/src/gpu/d2d.rs` | New. D2D device/context on the engine's `IDXGIDevice`, DirectWrite factory, raster of a `Scene` into a BGRA texture, text layout cache; resets D3D11 blend, rasterizer and depth state after `EndDraw` |
| `src-tauri/src/gpu/shaders/layer_over.hlsl` | New. Samples a layer (premultiplied or straight flag) for blending |
| `src-tauri/src/gpu/mod.rs` | Edit. `Renderer::draw_with(&Pass, DrawExtras { constants_b1, blend })`: leaves `Pass` and its ~20 call sites untouched. One premultiplied-over `ID3D11BlendState` |
| `src-tauri/src/fx/mod.rs` | Edit. `Chain` gets an optional registry handle: kinds starting with `ext:` are made by `ExtRegistry::make_filter`. New default trait method `GpuFilter::set_frame(&FrameInfo)`, called like `set_canvas` |
| `src-tauri/src/engine.rs` | Edit. `Msg::SetOverlays`, `Msg::SetExtensions`, `Msg::ExtAction`; session clock (`FrameInfo`); `LayerStack::run` between `stage.run` and the tap; ext instances follow chain and overlays; GPU-loss suspects (6.4) |
| `src-tauri/src/api.rs`, `commands.rs`, `main.rs` | Edit. Commands below; registry start at launch |
| `src-tauri/Cargo.toml` | Edit. `naga` (pinned, features `wgsl-in`, `hlsl-out`), `rquickjs` (`loader`), `oxc` (`transformer`, `codegen`), `zip`; `windows` features `Win32_Graphics_Direct2D`, `Win32_Graphics_Direct2D_Common`, `Win32_Graphics_DirectWrite` |

New commands: `list_extensions`, `reload_extensions`, `set_extension_enabled(id, on)`, `install_extension(path)`,
`open_extensions_folder`, `set_overlays(overlays)`, `ext_action(cardId, action)`, `ext_restart(cardId)`.
New events: `extensions-changed` (`ExtensionInfo[]`: id, name, version, kind, params, actions, `state: "ready" |
"error" | "disabled"`, `error?`), `ext-log` (`{cardId, level, text}`).

### 5.3 Frontend

| File | Change |
|---|---|
| `src/filters/schema.ts` | `FilterKind = BuiltinKind \| \`ext:${string}\``; `TextDef`; `defOf(kind, extensions)` returning a `FilterDef` for built-ins and installed extensions; `isFilterKind` accepts `ext:` |
| `src/filters/chain.ts` | `normalizeChain` keeps `ext:` entries (coerced against the manifest when installed, kept as-is when missing); `PresetFile` gains optional `overlays`; `signature` covers chain + overlays. Old presets load unchanged; older app versions ignore `overlays` (`presetChain` already ignores other sections) |
| `src/components/ParamField.tsx` | One more case: `text` (input, `maxLength`) |
| `src/components/FilterCard.tsx` | `defOf`; extension badge (name, version); actions row for widgets; "not installed" / "disabled" placeholders; "Restart" on a stopped card |
| `src/components/AddFilter.tsx` | Groups: Filters, Extension filters, Overlays, Widgets. Overlays and widgets go to the overlays list |
| `src/components/ExtensionsPanel.tsx` | New. Settings → Extensions: list, state, error, enable switch, Open folder, Install from file, Reload, Log |
| `src/App.tsx` | Chain column: "Camera in" → filters → "Output" → **Overlays · top is front** → "Better Cam out". Same `FilterCard`, drag and arrows |
| `src/store.ts` | `overlays: FilterCfg[]` persisted like `chain`, pushed with the same coalescing as `setChain`; `extensions`; snapshot/undo covers overlays |
| `src/api.ts` | New commands and events |

Parameters: an extension's `params` array is a `ParamDef[]`, so `ParamField` renders it with no new code except
`text`. Presets store `{kind: "ext:clock", enabled, params}` like any filter. The Rust side clamps with the manifest's
ranges, as it does for built-ins with `defaults.json`.

### 5.4 Errors and status

| Source | Channel | Card shows |
|---|---|---|
| Manifest invalid, API version unknown, permission requested | `extensions-changed` (`state: "error"`) | Settings list; cards of that extension: "Extension “x” failed to load: …" |
| Shader compile error | `filter-error {id, message}` | `vhs.wgsl:14:9: expected ';'` |
| Script throws / times out / runs out of memory | `filter-error` | `glitch.ts:12: TypeError: …` / "took longer than 50 ms" / "used more than 32 MB" |
| Card stopped after repeated failures | `filter-error` | message + Restart button |
| `ctx.status()` | `filter-status` | the status line |
| Asset not found | existing `filter-error` from `fx/assets.rs` | "could not load logo.png: …" |

`FilterCard` already shows both lines by card id, so overlays and widgets get them for free.

---

## 6. Safety

### 6.1 Trust model

Extensions are code the user chose to install. The sandbox protects against mistakes and keeps the camera running;
it is not a guarantee against a determined attacker who exploits a bug in QuickJS or a GPU driver (both run inside
`better-cam.exe`). The install dialog says so once: "Extensions run code on your PC. Install only extensions from
people you trust." An out-of-process script host is a later hardening step, not v1.

### 6.2 Sandbox and permissions

| Resource | Shader | Script |
|---|---|---|
| Camera pixels | Own input and history only | Never |
| Files | Images from its own folder and the user's picked files, loaded by the asset loader | No file API. Scene `src` is checked: inside the extension folder, or exactly the value of one of the card's `path` params |
| Network | No | No API exists |
| Other cards, app state | No | No; own storage only |
| Processes, registry, clipboard, devices | No | No |
| `permissions` field | Must be empty in v1 | — |

### 6.3 Limits

| Limit | Value | On breach |
|---|---|---|
| Script memory | 32 MB per card (`set_memory_limit`); stack 512 KB | Exception → counts as a failure |
| Script call time | `init` 500 ms; other calls 50 ms hard (interrupt handler); 4 ms soft | Hard: exception, failure. Soft over 1 s average: status "Slow script", tick rate halves |
| Tick rate | Scene ≤ 30/s; filter `uniforms()` ≤ output fps | Clamped |
| Event queue | 64 per card; ticks coalesce | Oldest tick dropped |
| Cards with scripts | 16 | Further cards show "Too many extension cards" |
| Overlay layers | 8 | Same |
| Scene | 512 nodes, depth 16, 16 images, 4096 text chars, 256 KB as JSON | Validation error; previous scene kept |
| Layer raster | Budget 2 ms p95 per layer | Raster rate halves; status "Overlay is slow to draw" |
| Shader source | 256 KB; 4 image params; `Params` ≤ 4 KB | Refused at load |
| Shader GPU time | 3 ms mean over 1 s at the current size | Bypassed; "Too slow for live video (x ms per frame)" |
| Images | `MAX_IMAGE_DIM` 8192 (existing asset loader) | Load error |
| Storage | 64 KB per card | `set` throws |
| Status / log | 4 changes/s / 20 lines/s per card | Dropped, "(n lines dropped)" |

### 6.4 Failure behaviour

| Failure | Behaviour | Camera |
|---|---|---|
| Script throws in `init` | Card stopped, error shown, Restart button | Filter passes through; layer draws nothing |
| Script throws in a tick | Previous uniforms/scene kept; error once; 3 failures in 10 s → stopped | Unaffected |
| Infinite loop | Interrupted at 50 ms | Unaffected (separate thread) |
| Out of memory | QuickJS exception; card stopped | Unaffected |
| Script thread stuck in native code past the deadline | Card marked stopped; its mailbox no longer read; thread left to end on its own | Unaffected |
| Panic in host Rust code for a card | `catch_unwind` around the thread body; card stopped | Unaffected |
| Shader fails to compile | Error with line; previous version kept on hot reload, else pass-through | Unaffected |
| Shader too slow | Bypassed (6.3) | One slow second at most |
| Shader hangs the GPU (TDR) | Existing device-loss recovery (P15). Extension passes and layers that ran in the lost frames are disabled on recovery: "Disabled after a GPU reset"; a second reset with the same card quarantines it until its files change | DLL repeats the last frame ≤ 1 s, then slate; video back in ≤ 2 s |
| Invalid scene | Validation error; previous scene kept | Unaffected |
| Extension folder deleted while in use | Cards become "not installed", bypassed | Unaffected |
| Render thread finds a mailbox locked | Uses the previous value | Unaffected |

---

## 7. Build phases

Each phase is one coding session. "Depends on" lists hard prerequisites. Phases on one wave can run in parallel in
separate worktrees.

```
Wave A:  E1 SPIKE wgsl→fxc        E2 SPIKE d2d layer        E3 registry + manifest + listing
Wave B:  E4 extension filters (E1, E3)                      E5 script host core (E3)
Wave C:  E6 filter scripts + VHS (E4, E5)                   E7 layer stack + shader overlays (E4)
Wave D:  E8 scene overlays (E2, E5, E7)
Wave E:  E9 widgets (E8)
Wave F:  E10 SDK, install, dev UX (E9)                      E11 hardening + perf sign-off (E9)
```

Shared-file rule: `engine.rs`, `fx/mod.rs`, `gpu/mod.rs`, `store.ts`, `api.ts`, `commands.rs`, `main.rs` and
`src-tauri/Cargo.toml` are touched by parallel phases. Each phase adds its own block and does not reorder others; the
second to merge resolves the conflict.

### E1 — SPIKE: WGSL through `naga` and FXC (riskiest question)

- Goal: prove that WGSL with the 4.3 prelude translates with `naga` (`ShaderModel::V5_0`, explicit `binding_map`,
  `fake_missing_bindings = false`) to HLSL that `D3DCompile` accepts as `ps_5_0`, renders correctly in the existing
  D3D11 renderer, and that `Params` packed with `naga`'s offsets reaches the shader intact.
- Files: `src-tauri/examples/wgsl_spike.rs`, `src-tauri/tests/wgsl_corpus/*.wgsl` (12 shaders: prelude only; VHS;
  duotone; vignette; loops with `break`; `select`; `switch`; arrays of `vec4` in `Params`; `vec2` after `f32`; `mat3x3`
  in a function; `textureLoad` + `textureSampleLevel`; a deliberate error), `docs/SPIKE-EXT.md`, `Cargo.toml` (`naga`,
  pinned exact version).
- Decisions: 3.1 F1; 4.3 conventions and register mapping.
- Check: `cargo run -p better-cam --release --example wgsl_spike -- --corpus src-tauri/tests/wgsl_corpus` and the
  same with `--warp` print one row per shader (`naga ok`, `fxc ok`, compile ms, max error vs a CPU reference) and end
  with `PASS n/12`.
  **Pass**: all 11 valid shaders compile on hardware and WARP; the layout shader's readback equals the packed values
  exactly; images within 1/255 of the CPU reference; the error shader reports the right author line; compile p95
  ≤ 500 ms. **Fail**: any layout mismatch, or more than one valid shader rejected by FXC. Then `docs/SPIKE-EXT.md`
  records each failure and v1 switches authoring to `.hlsl` files with WGSL deferred; E4 changes only in its compile step.

### E2 — SPIKE: Direct2D layer on the engine device (parallel with E1)

- Goal: measure raster cost of scenes on the render thread and prove D2D leaves the D3D11 pipeline intact.
- Files: `src-tauri/examples/layer_spike.rs` (creates `GpuDevice` + `Renderer` like the tests; D2D device from the
  same `IDXGIDevice`; rasterizes a "typical" scene (one text box) and a "cap" scene (512 nodes, 4096 chars) into a
  1920×1080 BGRA texture, 1200 times, between ordinary `Renderer::draw` passes); result section in `docs/SPIKE-EXT.md`.
- Decisions: 3.2 O5 threading; 6.3 caps.
- Check: `cargo run -p better-cam --release --example layer_spike -- --seconds 20` prints raster ms p50/p95/max per
  scene and `state check: identical` (a known shader pass before and after a D2D draw reads back byte-identical).
  **Pass**: typical p95 ≤ 0.5 ms, cap p95 ≤ 3 ms, state identical. **Fail**: E8 uses the separate-device design of
  3.2 (overlay thread, keyed-mutex textures, `AcquireSync(key, 0)`); recorded in `docs/SPIKE-EXT.md`.

### E3 — Registry, manifest and listing (parallel with E1, E2)

- Goal: the app finds extension folders, validates manifests, lists them, and keeps `ext:` cards in chains and presets.
- Files: `src-tauri/src/ext/{mod.rs,manifest.rs}`, `commands.rs` (`list_extensions`, `reload_extensions`,
  `set_extension_enabled`, `open_extensions_folder`), `main.rs`, `src/filters/{schema.ts,chain.ts}`,
  `src/components/{AddFilter,FilterCard,ParamField,ExtensionsPanel}.tsx`, `src/store.ts`, `src/api.ts`,
  `sdk/examples/vignette/` (manifest + shader, not yet runnable), `package.json` script `test:ext`.
- Decisions: 4.1, 4.2, 5.3, 5.4 (registry errors), `text` param.
- Check:
  1. `cargo test -p better-cam ext::manifest` passes: every `sdk/examples/*/extension.json` parses; 20 invalid
     manifests in `src-tauri/tests/ext_manifests/` are each rejected with a message naming the field.
  2. `npm run test:ext` passes: `normalizeChain` keeps an `ext:` entry when the extension is missing, coerces its
     params when installed, and `signature` changes when an overlay changes.
  3. `npm run build` exits 0.
  4. Live: copy `sdk/examples/vignette` into `%APPDATA%\dev.bettercam.app\extensions\`; within 2 s, without restart,
     Add filter lists "Vignette" under Extension filters and Settings → Extensions shows it "ready". Delete the folder:
     the card shows "Extension “vignette” is not installed"; save and reload a preset: the entry is still there.

### E4 — Extension filters (needs E1, E3)

- Goal: WGSL (and HLSL) filters run in the chain, with images, history, hot reload and the GPU-time guard.
- Files: `src-tauri/src/ext/{wgsl.rs,compile.rs,filter.rs}`, `src-tauri/src/fx/mod.rs` (registry in `Chain`,
  `set_frame`), `src-tauri/src/gpu/mod.rs` (`draw_with`), `src-tauri/src/engine.rs` (`Msg::SetExtensions`, session
  clock), `sdk/examples/{vignette,duotone}/`, `src-tauri/examples/scripts/ext_filter_perf.json`.
- Decisions: 3.1, 4.3, 4.5 (hot reload), 5.1, 6.3 (shader limits), 6.4 (shader rows).
- Check:
  1. `cargo test -p better-cam ext::filter` passes on WARP: vignette at strength 0 is identity within 1/255; duotone
     maps black and white to its two colours; a compile error reports `vignette.wgsl:<line>:<col>`.
  2. `cargo run -p better-cam --release --example engine_script -- src-tauri/examples/scripts/ext_filter_perf.json`
     (synthetic 1080p60, four built-ins + vignette + duotone): fps 60.0, dropped 0, header latency p95 ≤ 3 ms,
     `gpu_ms` ≤ 0.1 ms above `perf_synth1080_four.json` on the same machine.
  3. Live: add Vignette, the Camera app shows it. Break `vignette.wgsl`: within 2 s the card shows the line, the
     picture keeps the last good version. Fix it: the change shows within 2 s, Stats shows no dropped frames.
  4. A shader with a 5 ms busy loop is bypassed within 1 s with "Too slow for live video".

### E5 — Script host core (needs E3; parallel with E4)

- Goal: TypeScript cards run on their own threads with limits, timers, storage, events and the `better-cam` module.
- Files: `src-tauri/src/ext/script/{mod.rs,host.rs,api.rs,transpile.rs,limits.rs}`, `src-tauri/Cargo.toml`
  (`rquickjs`, `oxc`), `src-tauri/examples/ext_script_probe.rs` (runs one script headless and prints its outputs).
- Decisions: 3.3 R1, 4.4 (minus scene), 4.5, 6.2, 6.3 (script rows), 6.4 (script rows).
- Check: `cargo test -p better-cam ext::script` passes:
  `while (true) {}` in `uniforms()` returns an error within 60 ms and later calls still run;
  allocating 64 MB fails with the memory message and the process lives;
  `import "../../x.js"` and `import "node:fs"` fail to load; `typeof fetch === "undefined"`;
  a `.ts` file with interfaces, generics and `as` runs; an error in it reports the `.ts` line;
  `setInterval(…, 100)` fires 10 ± 1 times in 1 s; `storage` survives dropping and recreating the card;
  dropping a handle never blocks (test measures < 1 ms).
  Plus `cargo run -p better-cam --example ext_script_probe -- sdk/examples/vhs/glitch.ts --seconds 5` prints glitch
  values that are 1 for short bursts.

### E6 — Script-driven filter uniforms and VHS (needs E4, E5)

- Goal: filter cards start their script, read uniforms through the mailbox, and follow chain changes.
- Files: `src-tauri/src/ext/filter.rs`, `src-tauri/src/engine.rs` (script lifecycle follows `SetChain`),
  `src-tauri/src/ext/script/host.rs`, `sdk/examples/vhs/`.
- Decisions: 4.5, 4.6, 4.8, 6.4.
- Check:
  1. Live: VHS in the Camera app shows scanlines and noise; with "Random glitches" at 30 per minute, tearing bursts
     appear about every 2 s; turning the bool off stops them within one frame.
  2. Put `for(;;){}` in `glitch.ts`: the card shows the 50 ms error, then "stopped" after three; the Camera app keeps
     60 fps (`engine_script` run of `ext_filter_perf.json` with VHS: dropped 0).
  3. Remove the card while its script loops: Stats shows no stall (max frame CPU time < 2 ms).

### E7 — Layer stack, shader overlays, overlays list (needs E4; parallel with E6)

- Goal: overlays exist: list in the UI and presets, `LayerStack` after the output stage, shader layers.
- Files: `src-tauri/src/gpu/{layers.rs,shaders/layer_over.hlsl}`, `src-tauri/src/engine.rs` (`Msg::SetOverlays`),
  `commands.rs` (`set_overlays`), `src/App.tsx`, `src/store.ts`, `src/filters/chain.ts` (preset `overlays`),
  `src/components/AddFilter.tsx`, `sdk/examples/snow/`, `src-tauri/examples/scripts/ext_layers.json`.
- Decisions: 5.1 (placement, blend), 5.3, 2.2.
- Check:
  1. `cargo test -p better-cam layers` passes on WARP: a 50 % red straight-alpha layer over mid gray gives the
     expected value ± 1; layer order changes the result; an empty stack returns the frame untouched (byte-identical).
  2. `engine_script` `ext_layers.json` (1080p60, four built-ins, four shader layers): fps 60.0, dropped 0, `gpu_ms`
     ≤ 0.15 ms above the run without layers.
  3. Live: Snowfall shows in the Camera app and in the preview; dragging it below another overlay changes z-order;
     a saved preset file has an `overlays` array and loads back; a v1 preset without it still loads.

### E8 — Scene overlays (needs E2, E5, E7)

- Goal: scripts return scenes; the render thread rasterizes new scenes with Direct2D/DirectWrite and blends them.
- Files: `src-tauri/src/ext/scene.rs`, `src-tauri/src/gpu/d2d.rs` (or the separate-device variant if E2 failed),
  `src-tauri/src/gpu/layers.rs`, `src-tauri/src/ext/script/api.rs` (`render`, `measureText`, `fromObsColor`,
  `requestRender`), `sdk/examples/{clock,logo}/`, `src-tauri/examples/scripts/ext_scene_perf.json`.
- Decisions: 3.2 O5, 4.4 scene, 4.9, 6.3 scene and raster rows.
- Check:
  1. `cargo test -p better-cam scene` passes: each cap in 6.3 rejects; bad colours rejected with the node path
     (`nodes[3].fill`); `src` outside the folder rejected; WARP raster of a red rect gives red pixels at the right
     place; a text node has a non-empty bounding box; an unchanged scene is not rasterized again (counter).
  2. `engine_script` `ext_scene_perf.json` (1080p60, Clock with seconds + Logo): fps 60.0, dropped 0, raster p95
     ≤ 0.5 ms (new stats field), latency p95 ≤ 3 ms.
  3. Live: Clock shows local time in the chosen corner of the Camera app; hide Better Cam to the tray for 2 minutes:
     the Camera app keeps showing the clock updating.

### E9 — Widgets (needs E8)

- Goal: actions, persistent state, status line, `text` param; countdown and BRB examples.
- Files: `src-tauri/src/ext/script/{host.rs,api.rs}` (`defineWidget`, state save), `commands.rs` (`ext_action`,
  `ext_restart`), `src/components/FilterCard.tsx` (actions row), `sdk/examples/{countdown,brb}/`.
- Decisions: 2.3, 4.10, 6.3 storage row.
- Check:
  1. `cargo test -p better-cam widget` passes: actions run in order; returned state is saved and survives a card
     restart; state over 64 KB is refused with a message.
  2. Live: Countdown 5 min → Start: the Camera app counts down and the card says "Running, 4:59 left"; Pause holds it;
     quit the app at 3:00 while running, relaunch 30 s later: it shows 2:30 ± 1 s and is still running.
  3. BRB: the action covers the frame with the message within one frame and removes it on the second press.

### E10 — SDK, install and developer experience (needs E9)

- Goal: authors get types, schema, examples, install from zip, a log view.
- Files: `sdk/{better-cam.d.ts,extension.schema.json,prelude.wgsl,tsconfig.base.json,README.md}`,
  `sdk/examples/tsconfig.json`, `src-tauri/src/ext/mod.rs` (copy `.sdk` on start, `install_extension` with zip
  checks), `src/components/ExtensionsPanel.tsx` (install, log), `tauri.conf.json` (bundle `sdk/` as a resource),
  `docs/EXTENSIONS-GUIDE.md` (author guide: the API of section 4 in tutorial order).
- Decisions: 4.1, 4.4, 6.1 (install warning), open question 3 (licence files).
- Check:
  1. `npx tsc -p sdk/examples/tsconfig.json --noEmit` exits 0 (every example type-checks against `better-cam.d.ts`).
  2. A JSON Schema validator run over every example manifest passes (`npm run test:ext` gains this).
  3. Live: Install from file with `countdown.zip` → listed and usable without restart; a zip with a `..\` entry is
     refused with a message and nothing is written; Log shows `ctx.log` lines from the clock.

### E11 — Hardening and performance sign-off (needs E9; parallel with E10)

- Goal: every row of 6.4 behaves as written; costs measured and recorded.
- Files: `src-tauri/tests/ext_faults/` (scripts and shaders that throw, loop, leak, oversize scenes, a GPU-hang
  shader behind a test flag), `src-tauri/src/ext/*` fixes, `src-tauri/examples/scripts/ext_steady.json`,
  `docs/PERF.md` (new "Extensions" section).
- Decisions: 6.3, 6.4, plan 4.8 targets.
- Check:
  1. `cargo test -p better-cam ext_faults` passes: each fault gives the card message of 6.4 and the engine keeps
     rendering.
  2. GPU hang test (on this PC, accepting one screen flicker): video returns within 2 s, the shader's card says
     "Disabled after a GPU reset".
  3. `engine_script` `ext_steady.json`, 10 minutes, 1080p60, four built-ins + VHS + Clock + Countdown: engine dropped
     0, latency p95 ≤ 3.5 ms, app private bytes grow ≤ 10 MB after minute 2; numbers written to `docs/PERF.md`.

---

## 8. Open questions for the owner

| # | Question | Recommendation | What it changes |
|---|---|---|---|
| 1 | Does "widget" mean an interactive overlay viewers see (designed here), or also a custom HTML panel inside the app? | Overlay now. A panel later as a sandboxed `<iframe srcdoc sandbox="allow-scripts">` with a `postMessage` bridge and no Tauri IPC, after a security review | Panel: +2 sessions and a review of what Tauri exposes to iframes |
| 2 | Do you want HTML/CSS/WebGPU overlays (an OBS-style browser layer) later? | Not in v1. If yes, one spike after E11: offscreen WebView2 via composition + `CreateFromVisual`, pass if 1080p30 transparent page costs ≤ 5 % CPU and ≤ 250 MB and keeps 30 fps with Better Cam hidden in the tray | Yes: +1 spike, +3 sessions, +100–300 MB RAM while a browser layer is in use (E). Real "TS + WebGPU" for overlays |
| 3 | Licence of extensions. The FSF reads interpreted programs that use a GPL program's facilities through bindings as needing a GPL-compatible licence (S: GPL FAQ). | License the SDK (`better-cam.d.ts`, schema, prelude, examples) MIT, and add a written exception: extensions that use only the documented extension interface may carry any licence. You can grant it for code you own (not OBS-derived files); get legal advice | `LICENSE-EXCEPTION` file, SDK headers; authors can publish non-GPL extensions with less doubt |
| 4 | Network for extensions (chat box, web now-playing, weather)? | No in v1. Later: per-host permission in the manifest, a consent prompt per extension, `fetch` limited to those hosts | +1–2 sessions; consent UI; privacy text |
| 5 | Audio level (from the microphone) and Windows "now playing" for widgets? | Later, as opt-in permissions. Audio needs microphone capture, which Better Cam does not do today | +1 session each; a Windows privacy prompt for the microphone |
| 6 | Global hotkeys for widget actions (toggle BRB without opening the window)? | Yes, after E9: hotkeys bound per action in the card, through Tauri's global-shortcut plugin | +1 session; new dependency |
| 7 | Overlays animate at the camera's frame rate (14.4 fps measured in your room). Should the engine repeat the last camera frame to keep overlays at the output rate? | Yes, as a later engine change: when no camera frame arrives within one output interval, re-render with the previous input | Engine loop change; more GPU work when the camera is slow; smoother clocks and animations |
| 8 | Accept raw HLSL shaders besides WGSL? | Yes; cheap and lets people port OBS shader-filter code | Second compile branch in E4; docs |
| 9 | Overlays in their own section below the chain (designed here) or mixed into one list with filters? | Own section: overlays always draw after the output stage, so mixing them in would suggest an order that does not exist | UI layout in E7 |
| 10 | Distribution: folders and zip files only, no online catalogue? | Yes for v1 | A catalogue needs hosting, signing and review |
