# Spike E1: WGSL filters through `naga` and FXC

Date: 2026-11-18. Code: `src-tauri/src/ext/` (`wgsl.rs`, `layout.rs`, `compile.rs`, `mod.rs`, `tests.rs`, `examples/*.wgsl`).
Scope: phase E1 of `docs/EXTENSIONS.md` (section 7) and its "unverified" items 1 and 2. Nothing is wired into the
engine; there is no manifest loader and no UI.

## 1. Verdict

**PASS. WGSL is the authoring language for custom filters; `.hlsl` stays a fallback.** A WGSL fragment shader is
translated with `naga`, compiled by the engine's own `gpu::compile_shader` (`D3DCompile`, `ps_5_0`) and run by the
existing `ShaderFilter` / `Renderer::draw` path. Every picture matched its CPU reference within 0.54 code values (limit
2) on WARP and on the RTX 5080, and every uniform component of a 73-component layout probe arrived where the shader
reads it.

The pass has three conditions. None changes the choice; each is already handled in the module and each must stay in
E4:

1. **`naga` 30.0.2 writes sampler code `ps_5_0` cannot compile** (a D3D12-style sampler heap in SM 5.1 syntax). The
   module rewrites those lines (section 5.1). The rewrite fails closed if `naga` changes its output shape.
2. **Untrusted WGSL can kill the process through `naga`'s stack** (a 125 000-term `a+a+...` chain overflowed a 16 MiB
   stack, and a stack overflow cannot be caught) **and can hang FXC** (a 30-line shader whose helpers call each other
   twice per level never finished in 30 s). The module runs both on a 512 MiB-reserve worker thread with a timeout,
   refuses programs that expand past 20 000 statements after inlining, and counts abandoned workers (section 5.3).
3. **`naga` does not enforce WebGPU's "derivatives only in uniform control flow" rule** and does not bound loops
   (its loop bound is 2^64 iterations). A hostile or careless shader can still hang the GPU at run time; that is a
   device-lost / watchdog matter for the engine, not something translation can prevent (section 5.4).

## 2. Pinned dependency

```toml
naga = { version = "=30.0.2", default-features = false, features = ["wgsl-in", "hlsl-out"] }
```

- Validation (`naga::valid`) has no feature flag; it is always compiled.
- `Cargo.lock` gains `naga`, `naga-types` (both 30.0.2) and 11 small pure-Rust crates: `arrayvec`, `bit-set`,
  `bit-vec`, `codespan-reporting`, `crunchy`, `half`, `libm`, `rustc-hash` (a second version), `unicode-width`,
  `zerocopy`, `zerocopy-derive`.
- 30.0.2 was the newest release on crates.io (`cargo info` said 30.0.2; 28.0.0 and 29.0.4 exist). The pin is exact on
  purpose: wgpu no longer fixes FXC problems, so a newer `naga` may write HLSL FXC rejects. Upgrade procedure:
  change the version, run `cargo test -p better-cam --lib ext::`; `measurements_and_the_corpus` compiles every
  example through FXC, `naga_output_without_the_sampler_fix_is_rejected_by_ps_5_0` tells whether the sampler fix is
  still needed, and `fix_samplers` returns an error (not wrong HLSL) if the sampler lines changed shape.
- `naga` 30 declares `rust-version = 1.87`; the workspace declares `1.82`. It builds with the installed 1.98.1.
  Raise `workspace.package.rust-version` to 1.87 (root `Cargo.toml`, not touched by this spike).

## 3. The shader convention that works

Plain WebGPU-valid WGSL. The host generates a prelude (so authors declare no bindings); a complete module that
declares the same things itself (`translate_module`) works the same way and is what `.sdk/prelude.wgsl` can be.

| WGSL | Meaning | HLSL register |
|---|---|---|
| `@group(0) @binding(0) var<uniform> frame: Frame` | built-in values | `cbuffer b0` |
| `@group(0) @binding(1) var<uniform> params: Params` | one member per manifest param (omitted when there are none) | `cbuffer b1` |
| `@group(0) @binding(2) var image: texture_2d<f32>` | the picture so far | `t0` |
| `@group(0) @binding(3) var history: texture_2d<f32>` | the card's previous output | `t1` |
| `@group(0) @binding(4..=7) var <key>: texture_2d<f32>` | one per `path` param, by key | `t2..t5` |
| `@group(0) @binding(8) var smp: sampler` | linear, clamp (the pass's `s0`) | `s0` |

This is exactly the table of `docs/EXTENSIONS.md` 4.3. `Frame` is the doc's struct unchanged (`size`, `inv_size`,
`canvas`, `time`, `delta`, `frame: u32`, `seed`, `_pad`); the host fills it with `FrameValues`
(`inv_size` is computed). The prelude's helpers (`tex`, `texel`, `to_linear`, `to_srgb`, `luma`, `hash`) compile as
written.

Rules that the module checks (each has a test and a message):

- Exactly one entry point: `@fragment fn main(...)`, named `main`, with no arguments or only
  `@builtin(position) pos: vec4<f32>`, returning `@location(0) vec4<f32>`. A compute or vertex stage, a second
  fragment entry, other inputs (`@location`, `front_facing`) and struct outputs are refused.
- Only the bindings in the table, all in `@group(0)`. Textures must be `texture_2d<f32>` (no storage, depth,
  multisampled, array, 1D, 3D or cube); the one sampler is not a comparison sampler. A binding declared twice is an
  error (`naga` does not check that).
- `var<storage>`, `var<workgroup>`, push constants, atomics, `enable f16;` and other extensions are refused
  (validation runs with no capabilities).
- Source ≤ 256 KB (`MAX_SOURCE_BYTES`), ≤ 4 path images, calls expanding to ≤ 20 000 statements
  (`MAX_INLINED_STATEMENTS`).
- Uniform types follow WGSL's own rules, which the validator enforces: a struct member of a uniform struct needs
  `@align(16)`; an array in a uniform needs a 16-byte stride (`array<vec4<f32>, 4>`, `array<Inner, 2>` with an
  `@align(16)` member; `array<f32, 4>` is refused by WGSL). Matrices (`mat2x2` to `mat4x4`, `f32`), `vecN<f32|i32|u32>`,
  scalars and nested structs all work.

Complete minimal example. The manifest declares `strength` (a slider, so `f32`); the author's file holds only this:

```wgsl
@fragment
fn main(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
  let c = tex(image, pos.xy * frame.inv_size);
  return vec4<f32>(mix(c.rgb, vec3<f32>(1.0) - c.rgb, params.strength), c.a);
}
```

```rust
use better_cam::ext::{compile::compile_ps, layout::Value, wgsl::{translate, FrameValues, ParamDecl}};

let t = translate(src, &[ParamDecl::new("strength", "f32")], &[])?;     // off the render thread
let bytecode = compile_ps("ext:invert", &t.hlsl)?;                        // ps_5_0, entry `main`
// every frame:
let b0 = t.pack_frame(&FrameValues { size: [w, h], canvas: [1920.0, 1080.0], time, delta, frame: n, seed })
    .transpose()?;                                                        // bytes of cbuffer b0
let b1 = t.pack_params(&[("strength", Value::F32(0.5))]).transpose()?;    // bytes of cbuffer b1
```

Errors carry `line` and `column` counted in the author's file (the prelude is excluded); `ExtError::render("vhs.wgsl")`
gives `vhs.wgsl:3:11: expected expression, found ";"`. A problem in the generated lines (a bad parameter type) is
reported as `parameter #2 of the manifest: ...`. FXC errors have no author line (they refer to generated HLSL), so
they carry the offending HLSL line instead.

The pixel convention is the engine's: `pos.xy` is the pixel centre, origin top-left, +y down
(`pixel_coordinates_start_top_left_with_y_down_and_pixel_centres`); `uv = pos.xy * frame.inv_size` gives the
texel centre on a 1:1 pass. Input and output are `R8G8B8A8_UNORM`, gamma encoded, straight alpha; the tests show the
sampler hands texels through unchanged and the render target stores what the shader returns.

## 4. Parameter layout

**Rule.** The bytes of a uniform buffer are written at the offsets `naga` computes for the WGSL struct, rounded up to
a multiple of 16 bytes for the buffer. The HLSL that `naga` writes is padded explicitly (`int _pad1_0;` members, a
trailing `_end_pad_N`, `matCx2` split into one `float2` per column) so that FXC's own cbuffer packing lands every
member at the WGSL offset.

**How offsets are derived.** `wgsl::reflect` walks the validated `naga::Module`: `StructMember::offset`, array
`stride`, the struct `span` (its size) and the matrix column stride. Nothing is computed from a hand-written rule.
`Layout::pack(&[(name, Value)])` writes values by member name through that tree and rejects a missing, unknown or
mistyped value instead of writing a zero (padding members, whose names start with `_`, may be omitted).

**What differs from HLSL's own packing** (so a hand-written `cbuffer` would be wrong). Observed in
`every_uniform_component_arrives_where_the_shader_reads_it`:

| Members in order | WGSL offset (used) | HLSL alone would use |
|---|---|---|
| `a: f32`, `b: vec2<f32>` | b at 8 | 4 |
| `c: f32`, `d: vec3<f32>` | d at 32 (after c at 16) | 20 |
| `gain: f32`, `tint: vec3<f32>` | tint at 16 | 4 |
| `e: f32` after a `vec3` | fills the gap (44) | same |
| `@align(16) n: Inner` after `f32` | 16 | 16 |

**Proof.**

- 73 scalar components (`f32`, `i32`, `u32`, `vec2/3`, `mat3x3`, `mat2x2`, `mat3x2`, `mat2x3`, `mat4x4`,
  `array<vec4<f32>, 3>`, `vec4<i32>`), each given a distinct value, are written into pixels by a shader that reads
  them one by one; all 73 read back exactly (WARP).
- A hand-written module with a nested struct, an array of structs, `@align(16)` members and a trailing `vec3`
  (14 components): all arrive.
- FXC's own reflection (`D3DReflect` on the compiled bytecode) agrees with the reflected offsets for all 17 members of
  the probe and on the cbuffer size (352 bytes); same size check for the nested module.
- The colour filter (scalar, `vec3`, `vec2`, `vec4`, flag) matches its CPU reference within 0.54 code values on WARP
  and on the hardware device.

`naga` renames identifiers that end in a digit (`m3` becomes `m3_`) or collide with HLSL names (`mul` becomes `mul_`).
That does not matter for packing (offsets, not names, are used) but shows in `D3DReflect` output.

## 5. What does not work as it comes out of `naga`, and what the module does

### 5.1 Samplers: Shader Model 5.1 output, rejected by `ps_5_0`

`naga` 30 always writes samplers through a D3D12 sampler heap, whatever `ShaderModel` is set:

```hlsl
SamplerState nagaSamplerHeap[2048]: register(s0, space0);
SamplerComparisonState nagaComparisonSamplerHeap[2048]: register(s0, space1);
StructuredBuffer<uint> nagaGroup0SamplerIndexArray : register(t0, space255);
static const SamplerState smp = nagaSamplerHeap[nagaGroup0SamplerIndexArray[8]];
```

FXC for `ps_5_0` stops at `error X3721: space is only supported for shader model 5.1 or later`, and D3D11 cannot bind
a sampler array or an index buffer like that anyway. `fix_samplers` deletes the first three lines and turns the
fourth into `SamplerState smp : register(s0);`, which matches the one linear clamp sampler `Renderer::draw` binds at
`s0`. It then refuses the result if `nagaSamplerHeap`, `SamplerIndexArray` or `space` still appear. The test
`naga_output_without_the_sampler_fix_is_rejected_by_ps_5_0` keeps the failing form on record. This is a text patch of
generated code; it is the weakest part of the path and the reason for the exact pin and the corpus test.

### 5.2 Other `naga` behaviours found

- **A panic.** Writing HLSL for a shader that uses a sampler panics ("Sampler buffer of group 0 not bound to a
  register") unless `Options::sampler_buffer_binding_map` has an entry for group 0. The module sets one. The panic
  was caught by the worker thread's `catch_unwind` the first time it happened (before the entry existed), which is
  what the guard is for.
- **Duplicate bindings are not an error in `naga`** (the host API is meant to catch them); the module checks.
- **No `fake_missing_bindings`**: every global has an explicit register in `binding_map`, so a missing mapping fails
  at translation.
- **Derivative uniformity is not enforced.** `textureSample` and `dpdx` after a branch on `pos.x`, or inside a loop
  with a data-dependent `break`, pass `naga` and FXC (`implicit_derivatives_outside_uniform_control_flow_...`). The
  convention keeps `tex()` (explicit LOD) as the default for that reason. `textureSample`, `dpdx`, `dpdy` and `fwidth`
  work in uniform control flow (`builtins.wgsl`, `derivatives.wgsl`: derivatives of `uv` come out as exactly one
  pixel).
- **Loop bounding does nothing useful.** `naga` injects `uint2 loop_bound = uint2(4294967295u, 4294967295u)`: 2^64
  iterations. `loop { acc = acc + 1.0; }` translates and compiles (7 to 13 ms); running it would hang the GPU. It was
  not run.
- **`hash()` from the prelude is not reproducible across implementations** (`sin` of arguments in the thousands,
  times 43758). The `vhs` test therefore compares its deterministic parts bit for bit and its noise only
  statistically.
- **Matrices in uniforms** are written `row_major` with columns transposed; `m[i]` and `m * v` come out right (the
  probe reads `params.m3[c][r]`, `mat3x2`, `mat2x3`, `mat4x4`).
- Not exercised: `discard`, `@invariant`, `textureGather`, `textureDimensions`, `textureLoad` with LOD other than 0 (the
  prelude's `texel` uses `textureLoad` and compiles), integer division and bit operations beyond the examples,
  `u32`/`i32` overflow behaviour.

### 5.3 Guards for untrusted source

| Input | Result |
|---|---|
| source over 256 KB | refused before `naga` runs (< 1 ms) |
| `\u{FEFF}` byte order mark (Notepad) | stripped |
| NUL byte, noise, 200 KB identifier, 30 000 nested parentheses, 20 000 nested blocks, `--` chains | error or fast success, 0.2 to 1.5 ms |
| 4 000 functions (270 KB of HLSL) | translates in 50 ms |
| `a+a+...` with 40 000 terms | translates in 0.25 s |
| `a+a+...` with 125 000 terms (the most 256 KB hold) | translates in 1.9 s (quadratic parse), FXC returns an error in 12 ms |
| 30 helpers each calling the previous twice | **refused** by `check_cost` (2^29 statements) in < 1 ms; unguarded, FXC did not return in 30 s |
| chain of 11 such helpers (2 048 inlined samples) | FXC 0.54 to 0.64 s |
| 5 000 straight-line statements | FXC 1.1 to 1.6 s |
| `for` 64 x 64 x 64, `for` 100 000, `loop {}` | FXC 7 to 230 ms (FXC does not unroll them) |

- `run_bounded` runs translation and FXC on a worker thread with a **512 MiB reserved stack** (address space only).
  Measured on a release build, the parser of `naga` 30.0.2 overflows 16 MiB between 20 000 and 40 000 terms of
  `a+a+...`, passes 60 000 at 64 MiB, fails 125 000 at 64 MiB and passes it at 128 MiB. 512 MiB is 4 times the
  worst case a 256 KB source can build. This is the only reason a hostile source could end the process; keep the stack
  size and the source limit together.
- A panic becomes `ErrorKind::Internal` with the panic text. A timeout (10 s for translation, 30 s for FXC) becomes
  `ErrorKind::Internal` and the worker is **left running**: neither `naga` nor `D3DCompile` can be cancelled. Abandoned
  workers are counted (`ext::stuck_workers()`), forgotten when they end, and past 2 new jobs are refused with "restart
  Better Cam" (`a_job_that_outlives_its_timeout_is_given_up_on_and_counted`).
- `check_cost` counts statements with calls expanded (loop bodies once). It is a guard against exponential
  inlining, not a cost model; a static limit of 20 000 is about 4 s of FXC here.
- For a hard guarantee (kill a runaway FXC), compile in a helper process (`better-cam.exe --compile-shader`) with a
  kill timer. The thread approach is enough for the tested inputs and keeps the spike small; E4 should decide.

### 5.4 Not covered by translation

A shader that is valid and terminates on paper can still be slow or hang the GPU (a loop of a billion iterations, or
`loop {}`). The existing device-lost path (`removed_reason`) is the only net; E4 should disable an extension card
when the device is lost while it was the newest shader to run.

## 6. Measurements

Machine: the owner's PC, Windows 11, NVIDIA GeForce RTX 5080, WARP for the test device, `dev` profile
(`opt-level = 2`). Single-threaded runs of `measurements_and_the_corpus`. "xlate" is `translate` (a worker thread per
call included), "FXC" is `compile_ps` (`ps_5_0`, `D3DCOMPILE_OPTIMIZATION_LEVEL3`).

| Example | xlate first | xlate avg of 5 | FXC first | FXC avg of 3 | WGSL bytes | HLSL bytes |
|---|---|---|---|---|---|---|
| identity | 0.81 ms | 0.42 ms | 1.72 ms | 1.17 ms | 130 | 1 318 |
| color (6 params) | 0.56 ms | 0.53 ms | 2.14 ms | 2.36 ms | 513 | 2 234 |
| uvdump | 0.36 ms | 0.34 ms | 1.15 ms | 1.19 ms | 279 | 1 338 |
| vignette / pixelate | 0.46 ms | 0.42 ms | 2.11 ms | 2.34 ms | 573 | 1 886 |
| animated | 0.40 ms | 0.37 ms | 1.50 ms | 1.53 ms | 286 | 1 624 |
| builtins (loop, branches, derivatives) | 0.74 ms | 0.58 ms | 9.15 ms | 9.08 ms | 1 155 | 3 331 |
| derivatives | 0.49 ms | 0.36 ms | 2.09 ms | 1.77 ms | 301 | 1 629 |
| vhs (the design's example) | 0.65 ms | 0.63 ms | 3.90 ms | 4.83 ms | 961 | 2 718 |
| two inputs (history + path image) | 0.51 ms | 0.37 ms | 2.38 ms | 1.89 ms | 390 | 1 655 |
| layout probe (73 components) | 0.87 ms | 0.88 ms | 9.72 ms | 9.53 ms | 3 231 | 10 707 |

A second run gave the same to within 15 %. Compared with the 20 to 300 ms estimate in `docs/EXTENSIONS.md` 3.1: these
examples take **2 to 10 ms** in FXC and under 1 ms to translate; the guarded worst cases above reach 1 to 2 s.

Accuracy against CPU references (code values of 255; limit 2): identity 0.00; colour 0.00 to 0.50 on WARP, 0.00 to
0.54 on the RTX 5080; vignette/pixelate 0.50; animated 0.49; builtins (loops, `mix`, `pow`, `smoothstep`, `fract`,
`sin`, bilinear at 1.5-pixel offsets, implicit and explicit sampling) 0.50; `vhs` 0.50 (0.00 with scanlines and bleed
off); history + a 4x2 path image 0.50.

**Binary size.** Release profile of the workspace (`opt-level 3`, thin LTO, `codegen-units 1`), a probe binary that
parses, validates and writes HLSL with these features, against an empty binary built the same way (under
`%TEMP%\opencode\e1\{probe,base}`):

| Binary | Size |
|---|---|
| without `naga` | 142 336 bytes |
| with `naga` (`wgsl-in`, `hlsl-out`, validation) | 2 000 896 bytes |
| **growth** | **1 858 560 bytes (about 1.8 MiB)** |

`cargo build -p better-cam --release` with the dependency and `pub mod ext;` produces a 11 260 416-byte exe; the
earlier exe in `target\release` (built 13:29 from another tree state, with other agents' edits in between) was
11 326 976 bytes. No before/after pair is possible with edits from other agents landing, and the exe did not
grow today, most likely because nothing in the binary calls `ext` and the linker drops `naga`. Expect the 1.8 MiB when
E4 links it.

## 7. Tests (`cargo test -p better-cam --lib ext::`)

30 tests; about 5 s of run time once built (the FXC stress cases take most of it). They cover:

- **Pictures** (WARP, `ShaderFilter` + `Renderer::draw`, read back, compared with CPU references): identity (32x16,
  30x14, 1x1), colour with scalar/vec2/vec3/vec4/flag params, UV dump (origin, direction, pixel centre), vignette and
  pixelate, animated (three times, frame counter and seed), loops/branches/built-ins at three radii, derivatives, the
  `vhs` example, a history texture plus a path image.
- **Hardware**: the colour test on the default hardware adapter (skips with a message when there is none).
- **Layout**: probe of 73 components, nested module, FXC reflection, packing errors.
- **Errors** (each asserts an `Err`, never a panic): syntax (3:11), unknown identifier (3:20), type errors, missing
  entry point, wrong entry name, vertex input, wrong output, compute and vertex stages, a second entry point, duplicate
  binding, unavailable binding, other group, unavailable texture slot, params missing from the struct and the reverse,
  wrong param type, invalid param names, a `frame` struct with an unknown member, 14 unsupported constructs, an
  oversized source.
- **Hostile input and limits**: nine pathological sources, the inlining bomb, FXC stress shaders, the timeout and
  panic guard.

How the filter runs in the test: `ShaderFilterDef` is `&'static` with a function-pointer `pack`, so the test leaks the
translated HLSL and a definition, and `pack` returns the frame bytes from a thread-local (that is `cbuffer b0`).
`Renderer::draw` binds only constant slot 0 and leaves slot 1 alone, so the test binds the `params` buffer at `b1`
with `PSSetConstantBuffers(1, ...)` before `ShaderFilter::render`. No change to `fx/` or `gpu/` was needed.

## 8. Corrections `docs/EXTENSIONS.md` needs

1. **3.1, F1 row**, "Uniform layout must match WGSL rules (unverified, spike E1)": verified (section 4). Replace the
   compile estimate "20 to 300 ms" with "2 to 10 ms typical, up to about 2 s for 5 000-statement shaders (measured)".
   Add the risk "`naga` writes SM 5.1 sampler code that needs a text fix (spike E1)".
2. **3.4 verified/unverified**: item 1 is verified; item 2 stays open but now has a mitigation (exact pin, corpus
   test, fail-closed sampler fix); item 7 gets the `naga` number (+1.8 MiB when linked).
3. **4.3 "Register mapping"**: keep the table as is. Replace "`naga` `binding_map` sets this; `fake_missing_bindings` off"
   with "`binding_map` sets this, `fake_missing_bindings` off, and `sampler_buffer_binding_map` must have an entry for
   group 0 or `naga` panics; its sampler heap output is rewritten to one `SamplerState smp : register(s0)`".
4. **4.3 "Forbidden"**: add: textures other than `texture_2d<f32>`, comparison samplers, any extension (`enable`),
   atomics and workgroup/private-storage that WGSL forbids in fragment shaders, an entry point not named `main`,
   entry arguments other than `@builtin(position)`, outputs other than `@location(0) vec4<f32>`, a binding declared
   twice, and shaders whose calls expand past 20 000 statements.
5. **4.3 "Uniform layout"**: add the author-visible rules: nested structs need `@align(16)`; arrays in `Params` must
   be of `vec4`/struct elements (16-byte stride); the manifest has no vector or matrix types beyond `f32`, `i32`,
   `u32`, `vec2f`, `vec4f` today, and `vec3<f32>`, `mat*` and arrays would be extra manifest `uniforms` types if wanted.
   State that offsets are `naga`'s and that HLSL packing differs (the table in section 4).
6. **4.3 "Sampling"**: add that `textureSample` and derivatives are **not** checked for uniform control flow by
   `naga`; the result outside it is undefined, so authors should use `tex()`.
7. **4.3 "Errors"**: `file:line:col: message` works for WGSL parse and validation errors. FXC errors have no author line;
   they are shown with the generated HLSL line.
8. **4.3 prelude**: `hash()` is not bit-reproducible between GPUs; say so in the comment. `Params` has no member when
   the manifest has no params, so `params` must not appear in the prelude then (the module omits it).
9. **5.2 `Cargo.toml` row**: `naga = { version = "=30.0.2", default-features = false, features = ["wgsl-in", "hlsl-out"] }`.
   The listed features were right; add `default-features = false` and the exact pin. `Renderer::draw_with` with a `b1`
   buffer stays necessary for the real filter (the spike bound `b1` by hand).
10. **5.1 / 7 phase E1**: worker stack (512 MiB reserve), timeouts, `stuck_workers` and the 20 000 statement limit are part of
    the loader's contract; "compile in a helper process" is an open option for a hard kill (section 5.3).
11. **Workspace `rust-version`** 1.82 is below `naga` 30's 1.87.
12. **4.8 (`vhs`)**: no correction. The shader compiles and renders as written; the `glitches` and `rate` members of
    `Params` are declared but never read by the shader, which is fine.

## 9. What E4 can reuse

- `ext::wgsl::translate` (prelude generation, line mapping, convention checks, HLSL, sampler fix),
  `translate_module` (full-module form), `ParamDecl`, `FrameValues`, `Translated { hlsl, frame, params, textures,
  uses_sampler }`.
- `ext::layout::{Layout, Member, UType, Value}`: reflected layouts and strict packing; `Layout::buffer_size()` is the
  size for `Renderer::constants`. `Translated::pack_frame` / `pack_params` return the bytes for `b0` / `b1`.
- `ext::compile::compile_ps` (bounded FXC with HLSL context in errors) and `ext::run_bounded` / `stuck_workers` for
  any other untrusted job.
- `src-tauri/src/ext/examples/*.wgsl` as the starting corpus (identity, colour, uvdump, vignette, animated, builtins,
  derivatives, vhs, two inputs) and `tests.rs` helpers (`run`, `probe_*`, `fxc_cbuffer`) for E4's filter tests.
- Still to build: the manifest-to-`ParamDecl` mapping (`bool` to `u32`, `select` to `f32` or `u32`, `color` to
  `vec4<f32>`), `ExtShaderFilter` (two constant buffers, `history`, path image textures, per-frame `FrameValues`,
  upload only when changed), `Renderer::draw_with`, caching of `Translated` by source hash, hot reload, and the choice
  between threads and a helper process for FXC.

---

# Spike E2: Direct2D layer on the engine device

Date: 2026-11-18. Code: `src-tauri/examples/layer_spike.rs` (the only code), `src-tauri/Cargo.toml` (`windows` features
`Win32_Graphics_Direct2D`, `Win32_Graphics_Direct2D_Common`, `Win32_Graphics_DirectWrite`). Scope: phase E2 of
`docs/EXTENSIONS.md` (section 7), decisions 3.2 O5 threading, 6.3 scene and raster caps, the `gpu/d2d.rs` reset list of
5.2, and unverified item 3 of 3.4. Nothing under `src-tauri/src/` changed.

Run it: `cargo run -p better-cam --release --example layer_spike [-- --warp] [--frames N] [--st-factory]
[--high-priority] [--skip-typical] [--cap-runs N --cap-chars N --cap-images N --cap-shapes N --cap-groups N]`.
Exit code 0 = PASS, 1 = FAIL, 2 = error. The extra flags exist so the cap scene can be resized and the factory type
compared; the defaults run the spec.

## E2.1 Verdict

**FAIL by the spec's rule, in the mild way: only the cap scene misses.** The two questions have different answers:

1. **Can D2D/DirectWrite draw into a texture on the engine's own device without changing the next engine pass?
   Yes.** On the RTX 5080 and on WARP, D2D left **no** D3D11 state changed (not one of the 24 states checked; the diff
   is empty), the probe pass rendered byte-identical to its reference after typical and cap rasters, and the layer was
   byte-identical whether it was rasterized on a cleared context or right after a probe pass. **No reset list is
   needed.**
2. **What does a raster cost?** Typical scene (the clock): **0.07 to 0.18 ms p50, 0.11 to 0.23 ms p95 cold** on a quiet
   machine (pass limit 0.5 ms). Cap scene at the current 6.3 caps (512 nodes, 4 096 characters): **p50 3.0 to 3.6 ms,
   p95 3.6 to 6.3 ms cold** (limit 3 ms), so the scene caps in 6.3 must come down (E2.5). D2D works on WARP.

What follows from the spec's fail rules: "Only the cap scene misses 3 ms → keep O5 on the render thread and lower the
scene caps in 6.3 (text characters first, then nodes)". The separate-device design of 3.2 is **not** needed.

## E2.2 What was built

- Device `GpuDevice::new(Hardware | Warp)` + `Renderer::new`. D2D: `ID2D1Factory1` (multi-threaded; `--st-factory`
  for single-threaded), `CreateDevice(IDXGIDevice)`, one `ID2D1DeviceContext`, shared `IDWriteFactory`.
- Layer: one 1920×1080 `B8G8R8A8_UNORM` texture (`BIND_RENDER_TARGET | BIND_SHADER_RESOURCE`) wrapped by
  `CreateBitmapFromDxgiSurface` (premultiplied, `TARGET | CANNOT_DRAW`), `SetTextAntialiasMode(GRAYSCALE)` (ClearType
  makes no sense on a transparent layer). Each raster: `BeginDraw`, `SetTransform(identity)`, `Clear(transparent)`,
  scene, `EndDraw`.
- Typical scene: rounded rectangle at 50 % black plus "12:34:56", Segoe UI 40 px weight 600, white.
- Cap scene (default = the 6.3 caps): 512 nodes = 8 nested groups (D2D layers: rotation ±2°, scale 0.93 per level,
  rounded-rectangle geometric mask, opacity 0.95; depth of the deepest leaf 9) + 112 text runs of 36 or 37 characters
  (4 096 in all, wrapped at 320 px, Segoe UI 14 to 28 px, weights 400 and 600) + 16 bitmap draws from one 256×256
  atlas (half with a source rectangle, some with opacity) + 376 filled or stroked rectangles, rounded rectangles,
  ellipses and lines, some translucent. Leaves are spread over the 9 levels. All geometry comes from a fixed
  pseudo-random generator, so every run draws the same scene.
- **Cold** = every raster builds all 112 `IDWriteTextLayout`s and the 8 mask geometries inside the timed region.
  **Warm** = layouts and geometries cached, only drawing repeats. Brushes, text formats and the atlas are cached in
  both. Each scene and mode runs 10 untimed frames, then `--frames` (600) timed frames paced at 60 Hz (the process
  calls `timeBeginPeriod(1)`).
- Per frame: probe pass, raster (`GpuTimer` around `BeginDraw`..`EndDraw`), probe pass, `Flush`. CPU time is the wall
  time from `BeginDraw` to the return of `EndDraw`.
- State check: probe pixel shader reading `t0` through `s0` at a half-texel offset and `b0`, returning alpha 0.5, into
  a 1920×1080 target cleared to (0.2, 0.4, 0.6, 1). Reference = its readback before D2D exists on the device. After
  each scene's raster: probe again, compare byte for byte. Plus a snapshot of 24 states before `BeginDraw` and after
  `EndDraw` (IA topology, input layout, vertex buffer 0, index buffer, VS/HS/DS/GS/PS, PS cb0-1, sampler 0, SRV 0-5,
  RS state, viewports, scissors, OM blend state/factor/mask, depth-stencil state/ref, render targets 0-7 and DSV).
- Self-test of the check (printed every run): a rasterizer state with scissor enabled and a 100×100 scissor rectangle
  is injected on the same context; the snapshot diff must list `RS state, RS scissor`, the probe pixels must change,
  and restoring exactly the listed states must give the reference back. All three held, so "none" and "identical" are
  not a blind spot.
- Reverse check and layer check as in the spec. The layer check verifies: every byte outside the rounded rectangle
  (plus 2 px) is zero, the rectangle interior is (0, 0, 0, ~128) (premultiplied 50 % black), every pixel in the text
  box has alpha > 0, and at least 100 glyph pixels are opaque white.

## E2.3 Output

WARP:

```
adapter: Microsoft Basic Render Driver (warp)   d2d init 0.9 ms   first text layout 0.8 ms
cap scene: 512 nodes (text runs 112 / 4096 chars, images 16, shapes 376, groups 8)
self-test: injected scissor -> diff [RS state, RS scissor], probe pixels changed: true, restored by reset: true
first raster 1.42 ms (includes lazy D2D setup)
  typical: probe pixels identical
  cap: probe pixels identical
typical cold  frames 600  raster cpu ms p50 0.08 p95 0.13 max 0.31  gpu ms p50 0.13 p95 0.22
typical warm  frames 600  raster cpu ms p50 0.06 p95 0.10 max 0.42  gpu ms p50 0.12 p95 0.25
cap cold      frames 600  raster cpu ms p50 3.65 p95 4.81 max 12.10  gpu ms p50 3.76 p95 4.41
cap warm      frames 600  raster cpu ms p50 2.29 p95 3.41 max 9.39  gpu ms p50 4.38 p95 6.13
state diff after EndDraw: none
state check: identical
reverse check: identical
layer check: ok
verdict: PASS (WARP: times not judged)
```

RTX 5080 (hardware), last run at the default caps and normal priority:

```
adapter: NVIDIA GeForce RTX 5080 (hardware)   d2d init 1.3 ms   first text layout 0.7 ms
cap scene: 512 nodes (text runs 112 / 4096 chars, images 16, shapes 376, groups 8)
self-test: injected scissor -> diff [RS state, RS scissor], probe pixels changed: true, restored by reset: true
first raster 1.42 ms (includes lazy D2D setup)
  typical: probe pixels identical
  cap: probe pixels identical
typical cold  frames 600  raster cpu ms p50 0.11 p95 0.17 max 0.25  gpu ms p50 0.00 p95 0.00
typical warm  frames 600  raster cpu ms p50 0.08 p95 0.13 max 0.26  gpu ms p50 0.00 p95 0.00
cap cold      frames 600  raster cpu ms p50 5.85 p95 19.44 max 141.61  gpu ms p50 0.34 p95 0.39
cap warm      frames 600  raster cpu ms p50 2.28 p95 7.67 max 41.50  gpu ms p50 0.34 p95 0.40
state diff after EndDraw: none
state check: identical
reverse check: identical
layer check: ok
verdict: FAIL (cap cold p95 19.44 ms > 3 ms)
```

**Read the cap numbers with the machine in mind.** The user's game (Fortnite, 35 to 48 % CPU) and other agents' builds
ran during every hardware run, so p95 and max swing a lot between runs (the same scene gave cap cold p95 4.67, 6.85,
18.62, 19.44 ms at the default caps). The medians are stable and carry the verdict. Hardware runs at the default caps,
cap cold p50: 3.03, 3.18, 3.31, 3.33, 3.53, 3.56, 5.02, 5.45, 5.85 ms; typical cold p95: 0.42, 0.23, 0.17, 0.16, 0.11
to 0.14 ms. Even the quietest run has a cap cold median above 3 ms, so the failure is real, not noise. The first
`--high-priority` runs (flag added to cut scheduler noise) are cleaner: cap cold p95 6.28 and 3.5 to 3.9 ms.

GPU time (reported, not judged): typical 0.00 ms (below the timer's resolution; two small primitives), cap scene
0.34 ms on the RTX 5080 (the 8 nested layers alone take 0.25 ms, the 376 shapes 0.07 ms), 3.8 to 4.4 ms on WARP. The raster is CPU-bound.

## E2.4 State findings and the reset list

- **The state diff after `EndDraw` is empty on both adapters, for both scenes.** D2D put back or never touched every
  state in the list, including the render target:
  `OM render targets` after `EndDraw` is the probe target again, so the layer is **not** left bound as a render
  target and can be bound as a shader resource by the next pass without unbinding.
- Probe pixels identical after the typical and the cap raster, with no reset. The probe is sensitive: `Renderer::draw`
  sets render target, topology, input layout, VS, PS, SRV 0, sampler 0, constant buffer 0 and the viewport on every
  call, so it relies on the *defaults* for blend, rasterizer, depth-stencil, scissor, HS/DS/GS and the sample mask. Those
  are exactly the states a D2D leak would break, and the self-test shows a leaked scissor is caught.
- **Reset list for `gpu/d2d.rs` (5.2): empty.** The planned "reset blend, rasterizer and depth state after `EndDraw`"
  is not needed on this evidence. E8 should keep the check as a regression test instead of a runtime reset: take
  a snapshot before and after one raster on WARP and on hardware and assert an empty diff and an identical probe. If
  a driver ever leaks, the restore code in this example (`Snap::restore`) is the shape of the fix.
- Reverse direction (probe state does not change the layer): identical. D2D does not read any D3D11 state of the
  engine's passes.
- **The factory type did not matter.** Three alternating pairs, high priority, 300 frames: multi-threaded factory cap
  cold p50 3.31, 3.33, 3.03 ms, single-threaded 3.18, 3.05, 3.17 ms; typical cold p50 0.07 to 0.08 ms in all six; every
  check identical in all six. The engine device is multithread-protected either way. Use
  `D2D1_FACTORY_TYPE_MULTI_THREADED` (the spec's choice, since Media Foundation threads share the device); there is no
  speed reason to prefer the single-threaded one.
- D2D init is cheap: factory + device + context + one atlas bitmap 0.9 to 1.5 ms; the first text layout (font
  collection load) 0.6 to 0.9 ms; the very first raster 1.3 to 2.7 ms. None of it is a startup problem.
- D2D works on WARP (all three checks identical/ok), so E8's raster tests can run on WARP and on hardware.
- Not tested (outside the spec): D2D from a thread other than the one that owns the context (3.4 item 4); device loss
  during a raster; binding the layer texture as an SRV in a blend pass (the readback used `CopyResource`).

## E2.5 Cost model and new caps for 6.3

Typical scene, cold: 0.07 to 0.23 ms p95 on a quiet machine, 0.42 ms at worst under load. **Passes** (limit 0.5 ms).

Where the cap scene's cold time goes (hardware, p50, from the sweeps; quiet to moderately busy machine):

| Part | Cold ms | Warm ms | Note |
|---|---|---|---|
| 112 text runs, 4 096 characters, nothing else | 2.56 | 0.46 | layout creation ≈ 2.1 ms ≈ 19 µs per run (≈ 0.5 µs per character); drawing ≈ 4 µs per run |
| 8 nested layers (mask, rotation, opacity), no leaves | 0.52 | 0.51 | ≈ 65 µs per layer; building the mask geometry is negligible |
| 376 shapes | 1.2 to 1.6 | 1.6 | ≈ 3 to 4 µs per shape |
| 401 nodes without text (16 images, 376 shapes, 8 groups) | 2.14 | 1.84 | |
| Default cap scene (512 nodes) | 3.0 to 5.9 | 1.7 to 2.6 | sum of the parts |

So text layout creation is the largest single cost and the only one a cache removes: the warm scene is already
near 2 ms. Candidates, hardware, 600 frames, cap cold p95 (ms), with the default scene shape scaled down:

| Caps (nodes / text characters / images / groups) | Cap cold p50 | p95, high priority | p95, normal priority |
|---|---|---|---|
| 512 / 4 096 / 16 / 8 (current) | 3.0 to 5.9 | 3.5 to 6.9 | 4.7 to 19.4 |
| 384 / 1 536 / 16 / 8 | 3.0 to 3.4 | 5.24 | 3.85 |
| **256 / 1 024 / 16 / 8** | 1.6 to 2.1 (3.5 in one loaded run) | 2.96 | 2.10 (23.97 in one loaded run) |
| 192 / 768 / 8 / 8 | 1.4 | 2.12 | 1.92 |

**New 6.3 scene caps: 256 nodes, 1 024 text characters, 16 images, depth 16 (unchanged, the test scene only reaches
9), 256 KB as JSON (unchanged).** Text characters went down 4×, nodes 2×. 256 nodes passes in the runs on a machine that
was not saturated (cold p95 2.10 ms at normal priority, 2.96 ms high priority, warm p95 1.4 to 2.7 ms). One
normal-priority run while the game was running gave cold p95 23.97 ms (max 74.7 ms; warm p95 5.79 ms), which is
contention, not the scene: that run's median was 3.5 ms against 1.6 ms in the quiet one. The margin is thin on a
loaded machine; the 192-node row (1.9 to 2.1 ms) is the safe fallback if E8 wants room. Because the cost is per text
run more than per character, E8 should also cap text *runs* (e.g. 32) or count each run as 8 nodes in the node total.
The "Layer raster: budget 2 ms p95 per layer" row stays as the runtime guard; a full 256-node scene sits right at it
when every text changes, which is the case the guard exists for. Unchanged scenes are never re-rastered, and a clock
changes one text run per second.

## E2.6 What E8 takes from this

- `gpu/d2d.rs`: device-level D2D on `gpu.device.cast::<IDXGIDevice>()`, multi-threaded factory, one device context, a
  shared `IDWriteFactory`, layer = `B8G8R8A8_UNORM` + `BIND_RENDER_TARGET | BIND_SHADER_RESOURCE` + `CreateBitmapFromDxgiSurface`
  (premultiplied, `TARGET | CANNOT_DRAW`), grayscale text antialiasing, `PushLayer(.., None)` for groups (D2D makes the
  layer itself), cached `IDWriteTextFormat`s per (family, size, weight), cached `IDWriteTextLayout`s keyed by text and
  style, cached mask geometry per group. **No D3D11 state reset after `EndDraw`.**
- The layer is premultiplied BGRA: the blend pass must use premultiplied-over (`out = layer + (1 - layer.a) * frame`)
  and swizzle nothing (BGRA is sampled as RGBA by the shader).
- Cargo: the three `windows` features are enough, but `windows` 0.62 takes `Vector2` and `Matrix3x2` from the
  `windows-numerics` crate and does not re-export them. The example builds them from `[f32; N]` with a size-checked
  `transmute_copy`; E8 should add `windows-numerics = "0.3"` (already in `Cargo.lock` at 0.3.1) as a direct dependency
  instead of copying that trick.
- Tests: reuse the probe (`PROBE_HLSL`), `Snap` (take, diff, restore), `max_diff` and `layer_check` from the
  example as the E8 raster regression test on WARP and hardware.
- Fallback not needed: the overlay-thread design with its own device and keyed-mutex textures (3.2) stays on paper.
