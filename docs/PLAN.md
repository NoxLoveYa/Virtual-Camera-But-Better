# Better Cam — Technical Plan

Status: plan for version 1. No code exists yet.
Reference app: Better Mic (`E:\project\Better-Mic`, v0.2.5). Target repo: `D:\Project\Virtual-Camera-But-Better`.
Facts were checked on 2026-11-16 against Microsoft Learn, the `smourier/VCamSample` README, OBS Studio `master`
(`plugins/win-dshow`, `plugins/obs-filters`, `plugins/nv-filters`), `obsproject/libdshowcapture`, NVIDIA's
Broadcast SDK resources page and the wgpu changelog. Anything not checked against a primary source is marked
**(unverified)**. Section 9 lists them all.

---

## 1. Summary and v1 scope

Better Cam is the video sibling of Better Mic. It opens one physical webcam, applies camera settings and a GPU
filter chain, and publishes the result as a Windows 11 virtual camera named "Better Cam". Other apps (Teams, Zoom,
browsers, OBS, the Windows Camera app) pick "Better Cam" like any webcam.

The app looks and behaves like Better Mic: frameless acrylic dark window, one filter chain of cards with drag
reorder, enable switch, collapse, presets, Add filter menu, toast with Undo, NVIDIA install prompt, tray icon,
autostart, close to tray, state restored on launch.

### The picks in one place

| Decision | Pick |
|---|---|
| Capture API | Media Foundation Source Reader, async mode, with a DXGI device manager |
| GPU pipeline | Direct3D 11 through the `windows` crate, HLSL shaders compiled at runtime with `D3DCompile` |
| Virtual camera | Rust COM DLL (`windows` crate `#[implement]`) registered in HKLM, camera created with `MFCreateVirtualCamera` (System lifetime, AllUsers access) by an elevated helper |
| Frame transport | File-backed shared memory in `%ProgramData%\BetterCam\frames.bin`, three slots, seqlock, polled by the DLL |
| Preview | Raw BGRA frames over a Tauri `ipc::Channel` into a `<canvas>` (WebGL), capped at 30 fps and the panel's size |
| Filter architecture | One schema in TypeScript mirrored by a Rust `GpuFilter` trait and a `make(kind)` table, as in Better Mic |
| NVIDIA effects | NVIDIA Video Effects SDK 0.7.6+ loaded at runtime with `libloading`, D3D11 interop through `NvCVImage_InitFromD3D11Texture` |
| Licensing | GPL-2.0-or-later; OBS shaders and filter logic ported directly, with OBS parameter names, ranges and defaults |
| Auto mode | Device Auto toggles in v1; software "Auto adjust" filter (P16) built right after v1 core (9.3 item 1) |

### In v1

- Source settings from OBS's Video Capture Device: device, resolution/FPS type, resolution, FPS, video format,
  color space, color range, buffering, flip vertically, autorotation, hardware decode, deactivate when not showing.
- Output settings: resolution, FPS, aspect handling, background color. Independent of the capture format.
- Device controls from OBS's "Configure Video" page (VideoProcAmp and CameraControl): only those the device
  supports, with the device's own min, max, step and default, plus an Auto switch where the device allows it.
- Filters: Color Correction; Crop/Pad; Scaling/Aspect Ratio; Transform (flip and rotate); Sharpen; Apply LUT;
  Chroma Key; Color Key; Luma Key; Image Mask/Blend; NVIDIA Background Removal; NVIDIA Blur; NVIDIA Background
  Blur. OBS filter ids, parameter names, ranges and defaults are kept so values transfer one to one.
- Live preview in the window.
- Presets as JSON in the app data dir, autostart, close to tray, state restored on launch.
- One UAC prompt to install the virtual camera. Windows 11 (build 22000+) only.

### Not in v1 (design leaves room)

- Overlays: images, text and other sources composited on the camera. The engine's output stage is a compositor
  with one layer in v1; overlays add layers (section 2.6).
- Render delay filter. The engine has a frame queue (Buffering) that a delay filter can reuse.
- Remaining OBS video filters: Scroll, HDR Tone Mapping, Render Delay, Image Slide Show, Shader, Color Grade
  variants.
- Importing an OBS scene collection's filter list. Ids and parameter names already match, so this is a parser.
- Audio. The OBS source has audio settings; Better Cam ignores them. Better Mic covers audio.
- Zero-copy GPU hand-off to the virtual camera (section 4.3, later).

---

## 2. Architecture

### 2.1 Processes

| Process | Account / session | What runs there |
|---|---|---|
| `better-cam.exe` (Tauri) | Signed-in user, medium integrity, session 1+ | WebView2 UI, capture, GPU pipeline, preview, frame writer |
| `better-cam.exe --vcam-install` / `--vcam-uninstall` | Same user, elevated (UAC), short-lived | Copies the DLL, writes HKLM, sets ACLs, creates or removes the virtual camera |
| Windows Camera Frame Server (`svchost`, service `FrameServer`) | `LOCAL SERVICE`, session 0 | Loads `better_cam_vcam.dll`, runs our media source, reads frames, serves consumers |
| Frame Server Monitor (`svchost`) | `LOCAL SYSTEM` | Also loads the DLL (enumeration, monitoring) |
| Consumer app (Teams, Edge, Camera) | Its user | Receives frames from Frame Server; may also load the DLL in-process |

The VCamSample README states the DLL is loaded by at least four processes (driver app, Frame Server, Frame Server
Monitor, consumer) and that Frame Server and Frame Server Monitor run as Local Service and Local System. The DLL
must therefore sit in a folder those accounts can read, and its COM class must be in HKLM.

### 2.2 Data flow

```
 physical webcam (UVC)
        │ USB
        ▼
 Frame Server (OS) ── shared/exclusive arbitration of the physical camera
        │
        ▼
 better-cam.exe ──────────────────────────────────────────────────────────────────────────────┐
 │ [capture] MF Source Reader (async, MF work-queue threads)                                    │
 │    native type: MJPEG / NV12 / YUY2 / H264 / HEVC / P010 / RGB …                            │
 │    decoders inserted by the reader (HW MFT if available) → ID3D11Texture2D or CPU buffer    │
 │        │ latest-frame mailbox (1 slot; queue of up to 6 when Buffering = On)                 │
 │        ▼                                                                                    │
 │ [render thread] owns the D3D11 immediate context                                             │
 │    1 input convert   YUV/RGB → RGBA8 (color space, range, flip, autorotation)               │
 │    2 filter chain    ping-pong RGBA8 textures, filters in user order (incl. NVIDIA)          │
 │    3 output stage    fit/fill/stretch to output size, composite over background color       │
 │    4 encode          RGBA8 → Y (R8) + UV (R8G8), or BGRA for RGB32 consumers                 │
 │    5 readback        staging ring of 3, mapped one frame later                               │
 │        │                                      │                                             │
 │        ▼                                      ▼                                             │
 │ [writer] memcpy into frames.bin slot     [preview] downscale → BGRA → staging → Channel ───────► WebView2 <canvas>
 └────────┼─────────────────────────────────────────────────────────────────────────────────────┘
          │ %ProgramData%\BetterCam\frames.bin  (header + 3 slots, seqlock)
          ▼
 Frame Server: better_cam_vcam.dll
    MediaSource / MediaStream; on RequestSample: newest slot → IMFSample (MFCreate2DMediaBuffer)
    no app / stale heartbeat → built-in slate frame
          │
          ▼
 consumer apps (Teams, Zoom, Edge/Chrome, Windows Camera, OBS)
```

### 2.3 Threads in `better-cam.exe`

| Thread | Owns | Talks to |
|---|---|---|
| Tauri main | Window, tray, commands | Engine via `mpsc` messages (same pattern as Better Mic `engine::Msg`) |
| Engine control | Device open/close, device controls, hot-plug, standby logic | Capture callbacks, render thread |
| MF work queue (OS) | `IMFSourceReaderCallback::OnReadSample` | Pushes into the latest-frame mailbox, wakes the render thread |
| Render | D3D11 immediate context, filter chain, readback | Writer (inline), preview sender |
| Preview sender | `tauri::ipc::Channel` sends | WebView; drops frames while the previous one is unacknowledged |
| Asset loader | WIC image decode, `.cube` parse | Hands finished textures to the render thread; never blocks it |
| Stats | 4 Hz `stats` event | UI footer |

D3D11 runs with multithread protection on (`ID3D11Multithread::SetMultithreadProtected(TRUE)`), because the
Source Reader's decoder uses the same device through the DXGI device manager from MF threads.

### 2.4 Engine states

```
Stopped ── Start ──► Standby ── consumer attaches or window visible ──► Live
   ▲                    ▲                                                │
   └──── Stop ──────────┴──── no consumer and window hidden for 5 s ────┘   (only when deactivate_when_not_showing)
Live ── camera error ──► Reconnecting (retry every 3 s, banner) ──► Live
```

- Stopped: camera closed. DLL shows the "paused" slate.
- Standby: camera closed, engine ready. DLL shows a "starting" slate while the camera opens (0.5–2 s).
- Live: camera open, frames flow.
- Pause (title bar, Better Mic's Mute): camera stays open, writer publishes the "paused" slate state.

### 2.5 Frame transport protocol (`crates/vcam-protocol`)

File `%ProgramData%\BetterCam\frames.bin`, created by the app with `FILE_ATTRIBUTE_TEMPORARY`, mapped with
`CreateFileMappingW` (no name) and `MapViewOfFile` by both sides. All integers little-endian, `#[repr(C)]`.

```
Header (4096 bytes)
  magic            u32  'BCAM'
  protocol         u32  1
  header_size      u32
  file_size        u64
  writer_pid       u32
  writer_state     u32  0 closed · 1 paused · 2 starting · 3 live
  writer_heartbeat u64  QPC ticks, updated every frame and every 200 ms when idle
  qpc_frequency    u64
  advertised[8]    {width u32, height u32, fps_num u32, fps_den u32, subtype u32}   written by app, read by DLL at activation
  advertised_count u32
  -- written by the DLL --
  consumer_active  u32  1 while a stream is running in Frame Server
  consumer_heartbeat u64
  negotiated       {width, height, fps_num, fps_den, subtype}
  latency_p50_us   u32, latency_p95_us u32, delivered u64, repeated u64
  -- slots --
  latest_slot      u32
  slot[3]          {seq u64 (odd = being written), width u32, height u32, stride u32, subtype u32,
                    capture_qpc u64, data_offset u64, data_size u64}
Data: 3 × slot_capacity bytes (slot_capacity = 3840×2160×3/2 rounded up to 64 KB = 12.5 MB; file ≈ 38 MB)
```

Rules:

- Writer: pick the slot that is neither `latest_slot` nor the one the reader marked in use; set `seq` odd; copy;
  set `seq` even; store `latest_slot` (release ordering).
- Reader (DLL): read `latest_slot`, read `seq`, copy, re-read `seq`; retry once if changed or odd.
- The DLL validates every field before use: `data_offset + data_size <= file_size`, sizes match subtype and
  dimensions, dimensions ≤ 4096×2304. A bad file yields the slate, never a crash.
- Heartbeat older than 1 s, or file missing → slate "Better Cam is not running".
- RGB32 is advertised only up to 1920×1080, so the slot capacity covers every case.

Sync: the DLL does not wait on a kernel event. A worker thread in the DLL polls `latest_slot` with a 1 ms
high-resolution waitable timer (`CREATE_WAITABLE_TIMER_HIGH_RESOLUTION`) only while a `RequestSample` is pending.
Cost is negligible and it sidesteps the namespace problem in section 4.3.

### 2.6 Room for later phases

- Overlays: the output stage takes a list of layers `{texture, transform, opacity, blend}`. v1 has one layer (the
  filtered camera) over the background color. Overlay sources (image, text, second camera, window capture) add
  layers. Filters stay per-layer.
- Render delay: the Buffering queue already holds timestamped frames; a delay filter holds N ms of RGBA textures
  in a pool.
- More OBS filters: each is one HLSL file plus one schema entry (section 5.4).
- Zero-copy output: replace the CPU slot copy with a shared NT-handle texture duplicated into Frame Server
  (section 4.3).

---

## 3. Repo layout

```
Virtual-Camera-But-Better/
├─ Cargo.toml                      workspace: members = ["src-tauri", "crates/*"]; shared [profile.*] from Better Mic
├─ package.json                    copy of Better Mic, name "better-cam"; scripts: dev, build, tauri, vcam:build
├─ package-lock.json
├─ tsconfig.json                   copy
├─ vite.config.ts                  copy
├─ index.html                      copy, title "Better Cam"
├─ docs/
│  ├─ PLAN.md                      this file
│  └─ SPIKE-VCAM.md                written by phase 1: observed names, symbolic links, consumer results
├─ src/                            React 19 + zustand 5 frontend
│  ├─ main.tsx                     copy
│  ├─ App.tsx                      new layout (section 7)
│  ├─ store.ts                     Better Mic store, extended (section 5.1)
│  ├─ api.ts                       commands and events (section 5.5)
│  ├─ styles.css                   Better Mic styles + additions
│  ├─ filters/schema.ts            filter catalogue, new param types
│  ├─ filters/chain.ts             signature, normalizeChain, PRESET_NAME (small edits)
│  ├─ camera/schema.ts             source, output and device-control definitions
│  └─ components/
│     ├─ TitleBar.tsx  Select.tsx  Popover.tsx  Toast.tsx  Icon.tsx  AddFilter.tsx  PresetMenu.tsx
│     ├─ FilterCard.tsx  NvidiaPrompt.tsx
│     ├─ ParamField.tsx            renders one param of any type (slider, select, bool, int, color, path, combo, crop)
│     ├─ ColorField.tsx  PathField.tsx  NumberField.tsx  ComboField.tsx
│     ├─ Preview.tsx               canvas + WebGL upload of Channel frames
│     ├─ SourcePanel.tsx           device + OBS source properties
│     ├─ OutputPanel.tsx           output resolution, fps, aspect, background
│     ├─ ControlsCard.tsx          device controls with Auto switches
│     ├─ Stats.tsx                 footer, replaces Meters.tsx
│     └─ VcamBanner.tsx            install / repair virtual camera banner
├─ src-tauri/
│  ├─ Cargo.toml                   package "better-cam"
│  ├─ build.rs                     copy
│  ├─ tauri.conf.json              productName "Better Cam", identifier "dev.bettercam.app", window 1200×800 min 900×600, resources: vcam DLL
│  ├─ capabilities/default.json    copy
│  ├─ icons/
│  ├─ examples/
│  │  ├─ capture_probe.rs          list/open/controls test tool (phases 3–4)
│  │  └─ engine_headless.rs        capture → GPU → frames.bin without UI (phase 5)
│  └─ src/
│     ├─ main.rs                   Tauri setup, tray; handles --vcam-install/--vcam-uninstall before the builder
│     ├─ commands.rs               Tauri commands
│     ├─ engine.rs                 engine control thread, states, messages, stats
│     ├─ capture/
│     │  ├─ mod.rs                 enumerate devices and native formats, device keys
│     │  ├─ reader.rs              Source Reader setup, callback, format selection, decode path
│     │  ├─ controls.rs            VideoProcAmp / CameraControl via IKsControl and IAM* interfaces
│     │  └─ hotplug.rs             CM_Register_Notification for KSCATEGORY_VIDEO_CAMERA
│     ├─ gpu/
│     │  ├─ mod.rs                 device creation, adapter choice, texture pool, shader cache
│     │  ├─ convert.rs             input convert (NV12, YUY2, P010, I420, BGRA…) and output encode
│     │  ├─ readback.rs            staging ring
│     │  ├─ output.rs              fit/fill/stretch, background composite (future layer compositor)
│     │  └─ shaders/*.hlsl         fullscreen_vs, convert_*, encode_nv12, one file per filter
│     ├─ fx/
│     │  ├─ mod.rs                 FilterCfg, Params helpers, GpuFilter trait, Chain, make()
│     │  ├─ shader_filter.rs       generic single-pass filter driven by a ShaderFilterDef
│     │  ├─ color.rs  geometry.rs  sharpen.rs  lut.rs  keys.rs  mask.rs
│     │  ├─ assets.rs              WIC image load, .cube parser, loader thread
│     │  ├─ nvidia.rs              VFX SDK probe, loader, three effects
│     │  └─ auto_adjust.rs         phase 16
│     ├─ output_writer.rs          frames.bin owner/writer (uses vcam-protocol)
│     ├─ preview.rs                preview downscale + Channel sender
│     ├─ vcam_admin.rs             elevated install/uninstall, status, ShellExecuteExW "runas"
│     ├─ file_dialog.rs            IFileOpenDialog wrapper for the pick_file command
│     └─ install_nvidia.ps1        copy of Better Mic's, temp dir renamed
└─ crates/
   ├─ vcam-protocol/               header layout, constants (CLSID, paths, names), seqlock read/write, validation; no deps but windows
   ├─ vcam-source/                 cdylib "better_cam_vcam": the media source DLL
   │  └─ src/ lib.rs (Dll* exports, class factory) · activate.rs · source.rs · stream.rs · frames.rs (reader + slates) · log.rs
   └─ vcam-tool/                   bin: register | unregister | status | feed | stats  (spike + support tool)
```

Building the DLL: `npm run vcam:build` runs `cargo build -p vcam-source --release` before `tauri dev/build`.
Do not build it from `build.rs`; a nested cargo call deadlocks on the cargo lock (reported by the
`webbertakken/cameras` project, **(unverified)** for this repo).

### 3.1 Better Mic files: copy, edit or new

| File | Action | Notes |
|---|---|---|
| `src/components/Popover.tsx` | copy unchanged | |
| `src/components/Select.tsx` | copy unchanged | |
| `src/components/Toast.tsx` | copy unchanged | |
| `src/components/AddFilter.tsx` | copy unchanged | Iterates `FILTERS` and `FILTER_HINTS`; works with 13 kinds |
| `src/components/PresetMenu.tsx` | copy unchanged | Uses `signature`/`DEFAULT_SIGNATURE`; text "chain" still fits |
| `src/main.tsx`, `vite.config.ts`, `tsconfig.json` | copy unchanged | |
| `src-tauri/build.rs`, `capabilities/default.json` | copy unchanged | |
| `src/components/Icon.tsx` | edit | Add `camera`, `folder`, `eye`, `pause` paths |
| `src/components/FilterCard.tsx` | edit | Render params through `ParamField`; `showIf`; NVIDIA availability note for `nv_*` kinds |
| `src/components/TitleBar.tsx` | edit | Name "Better Cam"; states Live/Standby/Idle/Reconnecting; Pause replaces Mute; Preview button removed; Settings gains virtual camera row |
| `src/components/NvidiaPrompt.tsx` | edit | Title "Install NVIDIA video effects", SDK name and download size text |
| `src/styles.css` | edit | Keep all; add two-column layout, preview, color/path/number/combo fields, stats footer |
| `src/filters/schema.ts` | rewrite content | Same exports, new kinds and param types |
| `src/filters/chain.ts` | edit | Accept boolean params; preset object format |
| `src/store.ts` | edit heavily | Same structure and helpers (`persist`, `setChain`, `say`, snapshot/undo, presets) |
| `src/api.ts` | edit heavily | New commands and events |
| `src/App.tsx` | rewrite | New layout |
| `src/components/Meters.tsx` | drop | Replaced by `Stats.tsx` |
| `index.html`, `package.json` | edit | Title, name, version 0.1.0, `vcam:build` script |
| `src-tauri/src/main.rs` | edit | Names, commands, elevated-mode arg handling before the builder |
| `src-tauri/src/commands.rs` | edit | Keep autostart, tray, presets, NVIDIA install; drop VB-Cable; add camera, vcam, file picker |
| `src-tauri/src/dsp/nvidia.rs` | becomes `fx/nvidia.rs` | Keep `detect_gpu`, `arch_for`, `Status`; new SDK paths, URLs, VFX calls |
| `src-tauri/src/install_nvidia.ps1` | copy, edit | Temp dir `better-cam-nvidia` |
| `src-tauri/src/dsp/mod.rs` | pattern copied into `fx/mod.rs` | Same `FilterCfg`, `Params`, `pf`/`ps`, `Chain::apply` keeping state by id+kind |
| `src-tauri/src/audio/*`, `dsp/{basic,denoise,dynamics}.rs` | not copied | |
| `src-tauri/Cargo.toml` | edit | Drop `cpal`, `rtrb`, `nnnoiseless`; keep `libloading`; `windows` features below |
| `src-tauri/tauri.conf.json` | edit | Names, identifier, size, resources |

`windows` crate features (app): `Win32_Foundation`, `Win32_System_Com`, `Win32_System_Com_StructuredStorage`,
`Win32_System_Threading`, `Win32_System_Memory`, `Win32_System_Performance`, `Win32_Storage_FileSystem`,
`Win32_Security`, `Win32_Security_Authorization`, `Win32_System_Registry`, `Win32_Media_MediaFoundation`,
`Win32_Media_DirectShow`, `Win32_Media_KernelStreaming`, `Win32_Graphics_Direct3D`, `Win32_Graphics_Direct3D11`,
`Win32_Graphics_Direct3D_Fxc`, `Win32_Graphics_Dxgi`, `Win32_Graphics_Dxgi_Common`, `Win32_Graphics_Imaging`,
`Win32_UI_Shell`, `Win32_UI_Shell_Common`, `Win32_Devices_DeviceAndDriverInstallation`. The builder trims unused ones.

---

## 4. Decisions

### 4.1 Capture API

| Option | Formats and decode | Device controls | Fit with the rest | Cost / risk |
|---|---|---|---|---|
| **Media Foundation Source Reader** | `GetNativeMediaType` lists every subtype, size and frame-rate range. The reader inserts MJPEG/H264/HEVC decoders, hardware MFTs when `MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS` is set, and outputs D3D11 textures when given an `IMFDXGIDeviceManager` | The media source answers `QueryInterface` for `IAMVideoProcAmp`, `IAMCameraControl` and `IKsControl` (documented pattern, widely used) | Same API family as the virtual camera. Microsoft marks DirectShow legacy | Async callbacks need care; some cameras report odd frame-rate ranges |
| DirectShow (libdshowcapture style) | Pin enumeration gives formats; decoding needs our own MJPEG/H264 path (OBS uses FFmpeg) | Native `IAMVideoProcAmp`/`IAMCameraControl`; driver property pages | No Rust bindings for the sample-grabber path (`qedit.h` is gone from current SDKs); libdshowcapture is C++ under LGPL-2.1 | More code, a C++ dependency or a decoder dependency |

**Pick: Media Foundation Source Reader, async mode.**

Reasons: it decodes MJPEG/H264/HEVC with hardware where available and hands back D3D11 textures with no extra
dependency. Device controls are reachable from the same media source. It is the supported API on Windows 11.

How it is used:

- Enumerate with `MFEnumDeviceSources` (`MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID`). Device id = symbolic link.
- Exclude our own virtual camera from the list: friendly name starting with "Better Cam", and the software-device
  symbolic link recorded in `docs/SPIKE-VCAM.md` by phase 1. This prevents a feedback loop.
- Formats: walk `IMFSourceReader::GetNativeMediaType(VIDEO_STREAM, i)` and collect subtype, `MF_MT_FRAME_SIZE`,
  `MF_MT_FRAME_RATE`, `MF_MT_FRAME_RATE_RANGE_MIN/MAX`. Map subtypes to the OBS `video_format` names.
- Open: `MFCreateDeviceSource` → `MFCreateSourceReaderFromMediaSource` with attributes
  `MF_SOURCE_READER_ASYNC_CALLBACK`, `MF_SOURCE_READER_D3D_MANAGER`, `MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS`
  (when `hw_decode` is on), `MF_SOURCE_READER_DISCONNECT_MEDIASOURCE_ON_SHUTDOWN`.
- Select the native type on the source with `SetCurrentMediaType` on the media type handler, then set the reader
  output type: raw formats keep their subtype (no conversion in MF); MJPEG/H264/HEVC request NV12 (or P010 for
  10-bit HEVC).
- Decoder textures may lack `D3D11_BIND_SHADER_RESOURCE` and arrive as texture-array slices. The render thread
  copies the slice into its own NV12/P010 texture with `CopySubresourceRegion` (GPU-only, cheap), then samples it.
- CPU buffers (no D3D path, or software decode) are uploaded with `UpdateSubresource`.

Device controls: read and write through `IKsControl::KsProperty` with `PROPSETID_VIDCAP_VIDEOPROCAMP` and
`PROPSETID_VIDCAP_CAMERACONTROL`. `KSPROPERTY_TYPE_BASICSUPPORT` gives min/max/step, `KSPROPERTY_TYPE_DEFAULTVALUES`
the default, `GET`/`SET` the value and flags (`AUTO`, `MANUAL`). Use `IAMVideoProcAmp`/`IAMCameraControl` as the
fallback for the ten ProcAmp and seven CameraControl properties if a device rejects `IKsControl`. Powerline
frequency and low light compensation exist only as KS properties. **(unverified: KS ids
`KSPROPERTY_VIDEOPROCAMP_POWERLINE_FREQUENCY` = 13 and `KSPROPERTY_CAMERACONTROL_AUTO_EXPOSURE_PRIORITY` = 19 from
`ksmedia.h`; "Low Light Compensation" in the UVC property page maps to auto-exposure priority.)**

Persistence across reconnects: neither MF nor DirectShow keeps values. Better Cam stores the user's changed values
per device key and reapplies them after every open, in a fixed order: auto flags first, then manual values.
Device key = symbolic link; on reconnect at another USB port, fall back to `vid_XXXX&pid_YYYY` from the link plus
the friendly name.

### 4.2 GPU pipeline

| Option | D3D11 decode interop | NVIDIA VFX interop | Readback / NV12 | Effort |
|---|---|---|---|---|
| **Direct D3D11 (`windows` crate)** | Native: MF decoders output D3D11 textures on our device | Native: OBS uses `NvCVImage_InitFromD3D11Texture` and `NvCVImage_MapResource` | Staging textures; R8 + R8G8 render targets | Raw API, more boilerplate, HLSL |
| wgpu | No D3D11 backend since v0.19 (verified, changelog). Needs D3D11→D3D12 shared handles and `wgpu-hal` unsafe interop for every frame | Needs the same D3D11 shared-texture bridge | Buffer mapping is fine | Portable, but every interop edge is custom |
| D3D12 direct | MF decoders target D3D11; D3D12 decode needs another path | VFX D3D interop is D3D11 | Fine | Most effort |

**Pick: direct D3D11.**

Reasons: MF decode, NVIDIA VFX and readback all speak D3D11 natively, so the pipeline has no API bridges.
Portability is not a goal (Windows 11 only).

Details:

- Device: `D3D11CreateDevice` with `BGRA_SUPPORT | VIDEO_SUPPORT`, feature level 11_0+. Adapter: the default
  adapter, or the NVIDIA adapter (vendor 0x10DE) when an NVIDIA filter is in the chain. Changing adapter restarts
  the engine.
- Shaders: HLSL source embedded with `include_str!`, compiled once at startup with `D3DCompile`
  (`d3dcompiler_47.dll` ships with Windows), `ps_5_0`. Bytecode cached in memory. No build-time SDK tools needed.
- Working format: `R8G8B8A8_UNORM`, gamma-encoded SDR, same as OBS's default 8-bit path. Filters ping-pong
  between pooled textures keyed by size.
- Output encode: render Y into an `R8_UNORM` texture (W×H) and interleaved UV into `R8G8_UNORM` (W/2×H/2). Avoids
  NV12 render-target support differences between GPUs. BT.709 limited range, matching the media type the DLL
  advertises (`MF_MT_YUV_MATRIX = MFVideoTransferMatrix_BT709`, `MF_MT_VIDEO_NOMINAL_RANGE = MFNominalRange_16_235`).
- Readback: ring of 3 staging textures. Copy at frame N, `Map` at frame N+1. Adds at most one frame of latency
  and never stalls the GPU.

Zero-copy: decode → convert → filters are all GPU-resident in v1. The only CPU copies are readback (3.1 MB per
1080p frame) and the DLL's copy into the MF sample. A later phase can share an NT-handle texture into Frame Server
and wrap it with `MFCreateDXGISurfaceBuffer`; it only pays off when consumer and app use the same adapter.

### 4.3 Virtual camera component

#### Language

| Option | For | Against |
|---|---|---|
| **Rust, `windows` crate `#[implement]`** | One toolchain and workspace; shares `vcam-protocol` with the app; two public Rust media sources exist (a `windows-rs` source in `microsoft/Windows-Camera` issue #41 and the `vcam-source` crate in `webbertakken/cameras`) | No WIL; COM threading and `IMFAttributes` delegation are verbose |
| C++ (port VCamSample, MIT) | Closest to the reference sample; WIL helpers | Second toolchain; protocol code duplicated in two languages |

**Pick: Rust.** Port VCamSample's structure, not its code text. Classes and interfaces (per VCamSample and the
`webbertakken` notes, **(unverified)** as a complete required set):

- `Activator`: `IMFActivate` (with `IMFAttributes` delegated to an inner `MFCreateAttributes` store).
- `MediaSource`: `IMFMediaSourceEx`, `IMFMediaSource2`, `IMFGetService`, `IKsControl`, `IMFSampleAllocatorControl`.
  `IKsControl` returns `ERROR_SET_NOT_FOUND` for unknown sets (Frame Server requires the interface).
- `MediaStream`: `IMFMediaStream2`, `IKsControl`. Stream attributes per Microsoft's Frame Server custom media
  source guide (verified): `MF_DEVICESTREAM_STREAM_CATEGORY = PINNAME_VIDEO_CAPTURE`, `MF_DEVICESTREAM_STREAM_ID = 0`,
  `MF_DEVICESTREAM_FRAMESERVER_SHARED = 1`, `MF_DEVICESTREAM_ATTRIBUTE_FRAMESOURCE_TYPES = Color`.
- Samples: `MFCreateSample` + `MFCreate2DMediaBuffer` (Microsoft requires MF-allocated buffers), sample time and
  duration in 100 ns QPC units. Sample time = the app's `capture_qpc` converted to 100 ns, so A/V sync holds.
- Ignore the consumer's D3D manager in v1 and always deliver system-memory samples. VCamSample documents that its
  CPU path works for D3D-less consumers (browsers, Teams).
- Every exported and COM method body runs inside `catch_unwind`; a panic returns `E_UNEXPECTED`. A crash would take
  down Frame Server for every camera app on the machine.
- Logging: rate-limited file log in `%ProgramData%\BetterCam\logs\vcam-<pid>.log`, 1 MB rotation.

#### Process model

The DLL runs inside Frame Server as `LOCAL SERVICE` in session 0. The app runs as the user in session 1+. So:

- No window handles, no user-session named objects, no user profile paths in the DLL.
- The DLL never touches the camera, the GPU filter chain or NVIDIA. It only copies finished frames.

#### Frame transport

| Option | For | Against |
|---|---|---|
| **File-backed shared memory in `%ProgramData%`** | Works across session and namespace boundaries; a third-party Rust/Tauri app (`webbertakken/cameras` PR #61) shipped it after named objects failed | Possible disk writes; mitigated with `FILE_ATTRIBUTE_TEMPORARY` and measured in phase 2 |
| Named section `Global\…` created by the app | Simple | The app cannot create it: creating a global file mapping from a non-zero session needs `SeCreateGlobalPrivilege`, which this user's token lacks (checked with `whoami /priv` on the target machine) |
| Named section `Global\…` created by the DLL with a DACL | Service has the privilege | `webbertakken/cameras` reports Frame Server's namespace is isolated and the app could not open it **(unverified root cause)** |
| Section handle duplicated into Frame Server (DLL calls `DuplicateHandle` from the app process; app grants `LOCAL SERVICE` `PROCESS_DUP_HANDLE` on itself; pid/handle sent via `IMFVirtualCamera::SendCameraProperty`) | No file, no namespace | Needs the app to hold an `IMFVirtualCamera` (AllUsers needs admin to open), more moving parts **(unverified)** |
| Shared D3D11 texture (NT handle) | Zero-copy | Same handle problem; adapter mismatch with consumers; later phase |

**Pick: file-backed shared memory, `%ProgramData%\BetterCam\frames.bin`, layout in section 2.5.**
Fallback if phase 2 measures sustained disk writes above 5 MB/s: the handle-duplication option.

Sync primitive: seqlock in the file plus DLL-side polling (section 2.5). `IMFVirtualCamera::CreateSyncEvent`
exists as a documented app↔source event channel; its cross-process handle behaviour is **(unverified)**, so v1 does
not depend on it.

Format: NV12 (BT.709, limited range) at every advertised size; RGB32 (BGRA) at sizes up to 1080p for consumers
that ask for it.

Advertised media types (read from the header at source creation; defaults when the file is missing):

1. The output resolution at the output FPS (first = preferred).
2. The same at 30 fps if the output FPS is higher.
3. 1280×720 and 640×360 when the output is 16:9 and larger (lower sizes for apps that ask).
Each in NV12, and in RGB32 when ≤ 1920×1080. Default when no file: 1920×1080 and 1280×720 at 30 fps.

The app reads `negotiated` from the header and renders the encode step at that size, so the DLL never scales.

What the DLL outputs when the app is not delivering:

| Condition | Output |
|---|---|
| File missing, bad magic or heartbeat older than 1 s | Slate "Better Cam is not running" |
| `writer_state = paused` (Stop or Pause) | Slate "Camera paused" |
| `writer_state = starting` | Slate "Starting camera…" |
| Live, no new frame within 1.5 frame durations | Repeat the last frame (counted in `repeated`) |

Slates are pre-rendered NV12 images (1920×1080, dark gray, Better Cam mark, one line of text) embedded in the DLL,
compressed with RLE, scaled with nearest neighbour to the negotiated size. RGB32 slates are converted on the fly.

#### Registration and unregistration

| Option | For | Against |
|---|---|---|
| Installer only (NSIS per-machine hook) | Standard | Portable zip and dev builds have no path; upgrades of the DLL need the installer |
| **App-driven elevated helper (`better-cam.exe --vcam-install`), also called by the installer** | One code path for installer, portable and dev; "Repair" button | The app must handle UAC refusal |

**Pick: the elevated helper inside `better-cam.exe`.** The NSIS installer runs it post-install; the app offers it
in a banner when the camera is missing or outdated. One UAC prompt.

`--vcam-install --user-sid <SID>` does, in order:

1. Copy `better_cam_vcam.dll` to `%ProgramFiles%\Better Cam\vcam\better_cam_vcam_<protocol>.<build>.dll`.
   Versioned name, because Frame Server keeps the old DLL loaded. Never into `%LOCALAPPDATA%`: Local Service
   cannot read user profiles (VCamSample README).
2. Write `HKLM\SOFTWARE\Classes\CLSID\{BETTER_CAM_CLSID}` = "Better Cam Media Source" and `InprocServer32`
   (default = DLL path, `ThreadingModel = Both`). HKCU does not work (VCamSample README).
3. Write `HKLM\SOFTWARE\BetterCam\Vcam`: `DllPath`, `Protocol`, `Build`.
4. Create `%ProgramData%\BetterCam\` and `logs\` with ACL: SYSTEM and Administrators full, `LOCAL SERVICE`
   modify, the given user SID modify. Inheritance off. No other users.
5. `MFCreateVirtualCamera(MFVirtualCameraType_SoftwareCameraSource, MFVirtualCameraLifetime_System,
   MFVirtualCameraAccess_AllUsers, L"Better Cam", L"{BETTER_CAM_CLSID}", nullptr, 0, &cam)`; null categories give
   `KSCATEGORY_VIDEO_CAMERA`, `KSCATEGORY_VIDEO`, `KSCATEGORY_CAPTURE` (verified). If phase 1 shows DirectShow
   apps (OBS) cannot see it, also `AddProperty(DEVPKEY_Device_ClassGuid, GUID_DEVCLASS_CAMERA)` before `Start`;
   this needs admin, which the helper has **(unverified, reported by `webbertakken/cameras`)**.
6. `cam.Start()`, then `Shutdown()`. With System lifetime the camera stays registered across reboots (verified in
   the API docs; phase 1 confirms on this machine).
7. Delete DLLs with older versions in the folder; schedule locked ones with `MoveFileExW(DELAY_UNTIL_REBOOT)`.
8. Exit code 0 on success; details in `%ProgramData%\BetterCam\logs\install.log`.

The app starts it with `ShellExecuteExW` (verb `runas`, `SEE_MASK_NOCLOSEPROCESS`) and reads the exit code. The
argument check runs in `main()` before the Tauri builder, so the single-instance plugin does not swallow it.

`--vcam-uninstall`: `MFCreateVirtualCamera` with the same parameters → `Remove()`; delete HKLM keys; try to delete
the DLLs; `Restart-Service FrameServer` equivalent through the Service Control Manager if a DLL is locked, else
delay-delete; delete `%ProgramData%\BetterCam`.

Status (no elevation): read `HKLM\SOFTWARE\BetterCam\Vcam` and check that `MFEnumDeviceSources` lists a device
named "Better Cam…". States: `missing`, `outdated` (registered protocol < bundled), `ok`.

#### Lifetime

| Option | For | Against |
|---|---|---|
| **System lifetime, AllUsers** | Always listed, like OBS's virtual camera; Teams/Zoom keep the selection even when Better Cam is closed; created once by the elevated helper | Needs admin (we have one prompt anyway); the camera is listed when the app is not running, so the DLL needs slates |
| Session lifetime, CurrentUser, created by the app on each launch | No admin for this step | Disappears when the app exits; apps lose the selection; still needs admin for HKLM |

**Pick: System lifetime, AllUsers.** Fallback if phase 1 shows it does not persist: Session lifetime created by the
app at each launch.

Friendly name: Microsoft's docs say the pipeline appends "Windows Virtual Camera" to the name. Phase 1 records what
apps actually show **(unverified)**.

#### When the physical camera is also listed

Consumers see both the physical webcam and "Better Cam". The app holds the physical camera while Live. If a
consumer opens the physical camera too, Frame Server may share it (same media type) or report it in use. Better
Cam does not call `AddDeviceSourceInfo`: that API marks the physical camera as exclusively held by the virtual
camera and is meant for sources that open the camera inside Frame Server (verified, docs). With
`deactivate_when_not_showing` on, the physical camera is released whenever nobody watches Better Cam, so other apps
can use it.

### 4.4 Preview path into the webview

| Option | Latency (est.) | CPU (est., 960×540 @ 30) | Complexity | Problems |
|---|---|---|---|---|
| Native child window with its own DXGI swapchain over the webview | 1 frame | ~0 | High | Airspace: menus, popovers, dialog and toast cannot draw over it; must track scroll/resize; acrylic transparency |
| **Raw BGRA over `tauri::ipc::Channel` into a WebGL canvas** | 30–60 ms | 1–3 % (62 MB/s copy) | Low | Copies through IPC; capped size/fps |
| WebView2 shared buffer (`CreateSharedBuffer` + `PostSharedBufferToScript`) into WebGL | 20–40 ms | <1 % | Medium | COM plumbing through `with_webview` and `webview2-com`; tied to WebView2 version |
| MJPEG over a local HTTP server into `<img>` | 50–100 ms | 3–6 % (encode) | Medium | Port, auth, JPEG encoder dependency |
| WebRTC | 50–150 ms | Encode cost | High | Overkill |

**Pick: raw BGRA frames over `tauri::ipc::Channel`, drawn with WebGL `texImage2D` on a `<canvas>`.**

- Preview size = canvas CSS size × devicePixelRatio, capped at 960×540; 30 fps cap; frames sent only while the
  window is visible (`document.visibilityState`) and the preview is on.
- Backpressure: the sender keeps one frame in flight; JS acknowledges by calling `preview_ack`. New frames
  overwrite the pending one.
- Payload: 16-byte header (width, height, stride, frame number) + pixels.
- Upgrade path if profiling shows the copy matters: the WebView2 shared buffer, same JS drawing code.

### 4.5 Filter architecture

Shared schema, mirrored like Better Mic (`src/filters/schema.ts` ↔ `src-tauri/src/fx/mod.rs`):

- The TS schema is the source of truth for UI, labels, ranges and defaults. Rust reads params with defaults
  (`pf`, `ps`, `pb`, `pi`, `pc` helpers) and clamps every value to the same range. A unit test in Rust parses a
  JSON copy of the defaults exported by a tiny script (`npm run schema:export` → `src-tauri/src/fx/defaults.json`)
  and checks each `make(kind)` accepts it. This keeps the two sides from drifting.
- `kind` strings are OBS filter ids (`color_filter`, `crop_filter`, …). Param keys are OBS setting names.

New param types (beyond Better Mic's slider and select):

```ts
type ParamValue = number | string | boolean;

type ParamDef =
  | { type: "slider"; key; label; min; max; step; def: number; unit?; int?: boolean }
  | { type: "select"; key; label; def: string | number; options: { value: string | number; label: string }[] }
  | { type: "bool";   key; label; def: boolean }
  | { type: "int";    key; label; min; max; def: number; unit? }            // number box with arrows, for crop
  | { type: "color";  key; label; def: number; alpha?: boolean }            // OBS int 0xAABBGGRR
  | { type: "path";   key; label; def: ""; filter: "image" | "lut" }        // Browse… opens IFileOpenDialog
  | { type: "combo";  key; label; def: string; options: string[]; pattern?: RegExp }  // editable list (scale resolution)
// every ParamDef may carry: showIf?: { key: string; in: ParamValue[] }; hint?: string
```

Crop uses six `int` fields plus the `relative` bool, laid out by `ParamField` as a 3×3 grid around a small frame
diagram ("CropField" layout, no new param type).

Rust side:

```rust
pub trait GpuFilter: Send {
    /// Cheap. Repack constants; start async asset loads when a path changed.
    fn update(&mut self, gpu: &Gpu, p: &Params);
    /// Size of the output for a given input size (crop, scale, rotate change it).
    fn output_size(&self, input: Size) -> Size { input }
    /// Render input → output. Output texture is sized by output_size.
    fn render(&mut self, gpu: &Gpu, input: &Tex, output: &Tex, frame: &FrameInfo) -> Result<(), FxError>;
}

pub struct ShaderFilterDef {
    pub kind: &'static str,
    pub hlsl: &'static str,                       // include_str!("../gpu/shaders/color.hlsl")
    pub pack: fn(&Params, &FrameInfo) -> Vec<u8>, // 16-byte aligned cbuffer
    pub extra: &'static [ExtraInput],             // Lut3D, Image(key) …
}
```

- A shader filter is one HLSL pixel shader (`float4 main(VSOut i) : SV_Target`) with a `cbuffer Params : b0`, the
  input at `t0`, extra inputs at `t1+`, linear clamp sampler at `s0`. `ShaderFilter` implements `GpuFilter` for any
  `ShaderFilterDef`. Multi-pass or special filters (scale with Lanczos, NVIDIA) implement `GpuFilter` directly.
- `Chain::apply(cfg)` is Better Mic's algorithm: keep instances whose `id` and `kind` match, create new ones,
  call `update`. Disabled filters are skipped. A filter whose `render` fails is bypassed for that frame and
  reported once through `engine-error`-style `filter-error` events.
- Live updates: the store calls `set_chain` on every change (Better Mic behaviour). The engine applies the newest
  chain before the next frame. Slider drags at 60 Hz send ~1 KB of JSON per change; no engine restart.
- Assets (`image_path`): loaded on the asset loader thread with WIC (PNG, JPEG, BMP, GIF) or the `.cube` parser,
  then swapped in. Until loaded, the filter passes the input through. TGA is not supported in v1.

### 4.6 NVIDIA Video Effects

- Detection (no load): GPU via Better Mic's `detect_gpu()` (`nvidia-smi`, compute capability → turing / ampere /
  ada / blackwell). SDK dir: `%NV_VIDEO_EFFECTS_PATH%`, else `%ProgramFiles%\NVIDIA Corporation\NVIDIA Video
  Effects` (both verified in OBS `nvvfx-load.h`). Required: `NVVideoEffects.dll`, `NVCVImage.dll`, `models\`.
- Version: load the DLL on the engine control thread, call `NvVFX_GetVersion`, require ≥ 0.7.6 (OBS's
  `MIN_VFX_SDK_VERSION`, verified). NVIDIA's resources page lists Video Effects 0.7.6 for RTX 20–50 (verified).
- Install flow: same as Better Mic (`install_nvidia.ps1`: download, Authenticode check for an NVIDIA signer,
  `Start-Process -Verb RunAs -Wait`, progress events). URLs:
  `https://international.download.nvidia.com/Windows/broadcast/sdk/VFX/nvidia_video_effects_sdk_installer_v0.7.6_{turing|ampere|ada|blackwell}.exe`
  **(unverified: from the StreamFX wiki, not NVIDIA's page; phase 13 checks each returns HTTP 200 and a valid
  NVIDIA signature)**. Prompt opens once when an RTX GPU is present and the SDK is missing (Better Mic rule).
- Loading: `libloading` with `LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS` (OBS's flags),
  model directory `<sdk>\models` via `NvVFX_SetString(NVVFX_MODEL_DIRECTORY)`, one CUDA stream per effect
  (`NvVFX_CudaStreamCreate`). Effects: `NVVFX_FX_GREEN_SCREEN` (background removal) and `NVVFX_FX_BGBLUR`
  (blur, background blur). Model load takes seconds: it runs on the engine control thread; the filter passes
  frames through until ready, card shows "Loading model…".
- Place in the pipeline: an ordinary chain entry at the user's position. Input RGBA8 texture → wrap with
  `NvCVImage_InitFromD3D11Texture` → `NvCVImage_MapResource` on the stream → transfer to the effect's input
  format → `NvVFX_Run` → mask (A8) or blurred image into a D3D11 texture → unmap → our shader applies the mask
  (threshold) or blend. `processing_interval` N reuses the last mask for N−1 frames.
- Transparent pixels from background removal end up over the output background color (section 6.3).
- Fallback: when the SDK or GPU is missing, the card stays in the chain, is bypassed, and shows the reason with
  "Install it now" (RTX present) or "Needs an NVIDIA RTX GPU" (absent). The Add filter menu marks these "RTX only".
- Reference for call order: NVIDIA's own `MAXINE-VFX-SDK` samples and the SDK guide; OBS `plugins/nv-filters` may also be used as a reference (licence, 4.7).

### 4.7 Licensing

OBS Studio is GPL-2.0-or-later. libdshowcapture is LGPL-2.1 (verified, `COPYING`).

- Translating OBS `.effect` shaders or C filter code into HLSL/Rust makes Better Cam a derivative work. Distributing
  it then requires GPL-2.0-or-later (or GPL-3.0) for the whole app, source on request, and a GPL-compatible story
  for loading NVIDIA's proprietary SDK.
- Filter ids, setting names, numeric ranges and defaults are interface facts, not expression. Reusing them so
  values transfer one to one is low risk.

| Option | Cost | What it rules out |
|---|---|---|
| Clean-room: write shaders from public math (Rec.709/601 matrices, YCbCr distance keying, unsharp mask, trilinear 3D LUT, standard blend formulas); keep OBS names/ranges/defaults; verify behaviour by comparing output against OBS screenshots | Writing shaders ourselves (planned anyway); output may differ slightly from OBS for the same values | Nothing; any licence stays open |
| **License Better Cam GPL-2.0-or-later and port OBS shaders directly** | Saves maybe one session of shader work; exact OBS look | Closed-source or permissive licensing; adds GPL obligations around the NVIDIA SDK |

**Pick: GPL-2.0-or-later, port OBS shaders directly (owner decision, 2026-11-17).** Rules for builders: port each filter from its OBS `.effect` file and filter `.c` file so output matches OBS for the same values. At the top of each ported HLSL or Rust file, keep the OBS copyright notice and name the OBS source file it came from. The repo root carries a `LICENSE` file with the GPL-2.0 text, and `package.json` and every `Cargo.toml` declare `GPL-2.0-or-later`. MIT code (VCamSample) may be used; it is GPL-compatible. NVIDIA SDK: never bundled; it is loaded at runtime and users install NVIDIA's redistributable, as Better Mic does and as OBS's own NVIDIA filters do.

### 4.8 Performance targets

| Metric | 1080p60 | 4K30 | How measured |
|---|---|---|---|
| Added latency: frame arrives in app → sample delivered by DLL, p95 | ≤ 25 ms | ≤ 50 ms | `capture_qpc` in slot vs DLL delivery time, reported in header, shown in Stats |
| GPU time per frame, 4 shader filters | ≤ 2 ms (GTX 1650 class), ≤ 4 ms (Intel Iris Xe) | ≤ 4 ms / ≤ 8 ms | D3D11 timestamp queries |
| GPU time, NVIDIA background removal (quality) | ≤ 6 ms on RTX 3060 **(unverified estimate)** | ≤ 12 ms | same |
| App CPU, excluding WebView2 and software MJPEG decode | ≤ 4 % of a 6-core desktop | ≤ 6 % | Task Manager / `Get-Counter '\Process(better-cam)\% Processor Time'` |
| Software MJPEG decode (if no HW decoder) | ~1 core **(unverified)** | ~2 cores | same; Stats shows "software decode" |
| Frame Server CPU for our DLL | ≤ 1 % | ≤ 2 % | `\Process(svchost*)` while Better Cam streams minus idle |
| Preview | 30 fps, ≤ 60 ms, ≤ 3 % CPU incl. WebView2 | same | Stats + Task Manager |
| Memory | ≤ 250 MB private bytes (app, no NVIDIA); ≤ 50 MB in Frame Server | ≤ 400 MB | Task Manager |
| Dropped frames | 0 in 10 min steady state | 0 | `delivered`/`repeated` counters vs capture count |

Budget per 1080p60 frame (16.7 ms): decode ≤ 3 ms (HW), convert + 4 filters + encode ≤ 2 ms GPU, readback copy
≤ 0.5 ms CPU, slot write ≤ 0.5 ms, DLL copy ≤ 0.5 ms. Latency: ≤ 1 frame readback + ≤ 1 ms polling + copy.

### 4.9 Failure cases

| Case | Detection | Behaviour |
|---|---|---|
| Camera in use by another app | Open or first read fails (`MF_E_VIDEO_RECORDING_DEVICE_LOCKED`/`E_ACCESSDENIED`-class errors, exact codes **(unverified)**) | State Reconnecting, banner "Camera is in use by another app. Close it or pick another camera.", retry every 3 s; DLL shows "Starting camera…" slate |
| Camera access blocked by Windows privacy settings | `E_ACCESSDENIED` on open with privacy off | Banner with button opening `ms-settings:privacy-webcam`; no retry loop |
| Device unplugged | `OnReadSample` error / `MF_SOURCE_READERF_ERROR`, hot-plug removal notification | Close reader, Reconnecting, wait for arrival of the same device key (or VID/PID + name), reopen, reapply controls |
| Camera stays attached but goes silent | No frame for 1 s (more for a slow camera: 2.5 × its longest recent gap) | "Starting camera…" slate (also over a pause), session ends after 5 s (twice the slate time if longer) and reconnects; failed reopens count toward the 30 s `error_cap` like any generic error, then a fatal stop with a plain message (kept on purpose: an endless silent Reconnecting is worse than the message) |
| Format not supported | Chosen type missing after reconnect, or `SetCurrentMediaType` fails | Fall back to device default (`res_type = preferred`), toast "1920×1080 MJPEG isn't available; using device default", keep the user's setting stored |
| Decoder failure (H264/HEVC) | Reader error after type set | Retry once with `hw_decode` off, then fall back to the next raw format; toast |
| Frame Server restarts | Consumers lose the stream; app's own reader errors (it also goes through Frame Server) | App reopens the camera through the Reconnecting path; DLL re-maps `frames.bin` on the next activation; nothing to re-register |
| DLL not registered / outdated | Status `missing` or `outdated` on launch and on window focus | Warn banner (Better Mic VB-Cable banner style) "Install virtual camera" / "Update virtual camera" → one UAC prompt |
| UAC refused | `ShellExecuteExW` returns `ERROR_CANCELLED` | Toast "Virtual camera not installed. Administrator approval is needed once." Banner stays |
| `frames.bin` folder missing or ACL broken | Create/open fails | Banner "Repair virtual camera" (same helper) |
| RTX absent / SDK absent / model load fails | Probe, `NvVFX_Load` error | NVIDIA cards bypassed with reason; prompt as in Better Mic; other filters unaffected |
| GPU device removed (driver update, TDR) | `DXGI_ERROR_DEVICE_REMOVED` | Recreate device, filters, textures; DLL repeats last frame for ≤ 1 s, then slate |
| Asset file missing | Load error | Filter passes through, card shows "File not found" |
| Two Better Cam users at once (fast user switching) | Writer pid/heartbeat from another session | Not supported in v1; second instance shows "Better Cam is in use by another Windows user" and stays Stopped |

---

## 5. Data model

### 5.1 Persisted app state (`localStorage["better-cam"]`, like Better Mic)

```ts
interface Saved {
  source: SourceCfg;
  output: OutputCfg;
  /** Only values the user changed, per device key. Reapplied on every open. */
  controls: Record<string, Record<ControlKey, { value: number; auto: boolean }>>;
  chain: FilterCfg[];
  closeToTray: boolean;
  autostartDefaulted: boolean;
  nvidiaPromptDismissed: boolean;
  activePreset: string | null;
  presetBaseline: string | null;
  previewOn: boolean;
  /** Last running state, so a login launch knows whether to Start. */
  wasRunning: boolean;
}
```

The backend gets everything through commands; it does not read localStorage. A login launch (`--autostarted`)
runs `init()` in the hidden webview and calls `start_engine` with the saved state, as Better Mic does. With
`deactivate_when_not_showing`, that start lands in Standby, so the camera LED stays off until an app watches.

### 5.2 Source and output config

```ts
interface SourceCfg {               // keys = OBS dshow_input setting names
  video_device_id: string | null;   // MF symbolic link (OBS stores "name:path"; import maps by name)
  res_type: "preferred" | "custom"; // OBS int 0 | 1
  resolution: string;               // "1920x1080"
  frame_interval: number;           // 100 ns units; -1 = match output FPS, 0 = highest
  video_format: VideoFormat;        // "any" | "mjpeg" | "h264" | "hevc" | "nv12" | "yuy2" | "i420" | "yv12"
                                    // | "y800" | "p010" | "yvyu" | "uyvy" | "hdyc" | "xrgb" | "argb" | "rgb24"
  color_space: "default" | "709" | "601" | "2100PQ" | "2100HLG";
  color_range: "default" | "partial" | "full";
  buffering: "auto" | "on" | "off"; // OBS int 0 | 1 | 2
  flip_vertically: boolean;
  autorotation: boolean;
  hw_decode: boolean;
  deactivate_when_not_showing: boolean;
}

interface OutputCfg {
  resolution: "source" | `${number}x${number}`;  // default "source"
  fps: "source" | 15 | 24 | 25 | 30 | 50 | 60;    // default "source"
  fit: "fit" | "fill" | "stretch";               // default "fit"
  background_color: number;                      // OBS int ABGR; default 0xFF000000 (opaque black)
  extra_sizes: boolean;                          // advertise 720p/360p too; default true
}
```

OBS `video_format` ints for import: Any 0, Unknown 1, ARGB 100, XRGB 101, RGB24 102, I420 200, NV12 201,
YV12 202, Y800 203, P010 204, YVYU 300, YUY2 301, UYVY 302, HDYC 303, MJPEG 400, H264 401, HEVC 402
(verified, `libdshowcapture/dshowcapture.hpp`).

### 5.3 Filter chain

Same shape as Better Mic, plus boolean params:

```ts
interface FilterCfg { id: string; kind: FilterKind; enabled: boolean; params: Record<string, number | string | boolean>; }
```

Default chain for a fresh install: empty. (Better Mic ships its "Fifine" chain; a camera has no universal fix.
`DEFAULT_SIGNATURE` becomes the signature of `[]`, so PresetMenu shows "Default" for an empty chain.)

### 5.4 Preset file (`<app_data_dir>/presets/<name>.json`)

```json
{
  "app": "better-cam",
  "version": 1,
  "chain": [
    { "kind": "color_filter", "enabled": true, "params": { "saturation": 0.2, "contrast": 0.1 } },
    { "kind": "nv_background_blur_filter", "enabled": true, "params": { "intensity": 0.5 } }
  ]
}
```

- `load_preset` also accepts a bare array (Better Mic format) and a future `"camera"` section (ignored in v1).
- Name rule unchanged: `^[\p{L}\p{N} _-]+$`, enforced in Rust and TS.
- `normalizeChain` drops unknown kinds, fills missing params with defaults, clamps to ranges, assigns fresh ids.

### 5.5 Tauri commands

| Command | Args | Returns | Notes |
|---|---|---|---|
| `list_cameras` | — | `CameraInfo[]` `{id, name, formats: {format, width, height, intervals: number[]}[]}` | Excludes Better Cam |
| `get_camera_controls` | `id` | `ControlInfo[]` `{key, min, max, step, def, value, auto, can_auto, can_manual, mode: "manual"\|"camera"\|"app", app: "full"\|"stops"\|null, app_why?, driven_by?: "exposure"}` | Opens the device briefly if not Live. `app` says whether and how Better Cam auto is offered, `app_why` why not, `driven_by` marks the gain the exposure loop sets |
| `set_camera_control` | `id, key, value, auto` | `ControlInfo` (read back) | Applied immediately (manual value or the camera's Auto flag) |
| `set_control_mode` | `id, key, mode` | `ControlInfo[]` (the rows that changed: exposure with gain) | Who drives a control: `manual`, `camera` (Auto flag) or `app` (Better Cam auto, exposure and white balance only). Works on a closed camera too: the mode and its resume value are stored, the loop starts at the next open |
| `set_auto_config` | `id, cfg` | — | Better Cam auto settings per camera: `{exposure: {target, longest, max_gain, speed}, white_balance: {warmth, speed}}`. Live: the next decision uses them |
| `resume_auto` | `id, key` | — | After a `conflict`; also clears `blocked` ("Try again") |
| `reset_camera_controls` | `id` | `ControlInfo[]` | Device defaults, auto where supported by default; every Better Cam auto mode ends |
| `start_engine` | `source, output, controls, chain` | — | Enters Standby or Live. The `controls` record carries each control's mode (`app` with its resume value), the Better Cam auto settings and what the controller learned |
| `stop_engine` | — | — | Stopped; DLL shows paused slate |
| `set_source` | `source` | — | Reopens the device only when a capture field changed |
| `set_output` | `output` | — | Live; advertised types update for the next consumer activation |
| `set_chain` | `chain` | — | Live |
| `set_pause` | `on` | — | Better Mic's `set_mute` |
| `set_preview` | `on, width, height, channel: Channel` | — | Starts/stops preview frames |
| `preview_ack` | `frame` | — | Backpressure |
| `vcam_status` | — | `{state: "missing"\|"outdated"\|"ok", listed, consumers}` | |
| `vcam_install` | — | — | Runs the elevated helper; errors on UAC refusal |
| `vcam_uninstall` | — | — | Elevated |
| `pick_file` | `kind: "image"\|"lut"` | `string \| null` | `IFileOpenDialog` with filters |
| `nvidia_status` | — | `{available, detail, version}` | Copied, VFX paths |
| `nvidia_gpu` | — | `{name, arch} \| null` | Copied unchanged |
| `install_nvidia_sdk` | `arch` | — | Copied; VFX URL |
| `get_autostart`, `set_autostart`, `launched_at_startup`, `set_close_to_tray` | | | Copied unchanged |
| `list_presets`, `save_preset`, `load_preset`, `delete_preset` | | | Copied; `save_preset` takes the preset object |

### 5.6 Events

| Event | Payload |
|---|---|
| `engine-state` | `{state: "stopped"\|"standby"\|"live"\|"reconnecting", reason?: string}` |
| `engine-error` | `string` (fatal; store stops, like Better Mic) |
| `filter-error` | `{id, message}` (card shows it; not fatal) |
| `stats` (4 Hz) | `{capture: {width, height, format, fps, decode: "hw"\|"sw"\|"none"}, output: {width, height, fps}, gpu_ms, dropped, repeated, latency_p95_ms, consumers}` |
| `devices-changed` | — (store calls `list_cameras`) |
| `controls-changed` | `ControlInfo[]` (1 Hz while any Auto is on, so sliders follow the device; at most 4 Hz while Better Cam auto writes; also once after the stored controls were applied at open) |
| `auto-status` | `{id, exposure?: {state, exposure, gain, cap, cap_reason, max_gain, fps, target_fps, message, handed_back?}, white_balance?: {state, kelvin, message, handed_back?}, learned?, resume?}` (on every state change, at most 2 Hz while adjusting; `learned` and `resume` only when they changed, the store persists them) |
| `nvidia-install` | Copied unchanged |

---

## 6. Catalogue of v1 settings and filters

Ranges and defaults below come from OBS `master` source unless marked "Better Cam". Colors are OBS ints
`0xAABBGGRR`. "v2" = OBS's current version of the filter.

### 6.1 Source (OBS `dshow_input`)

| Key | Label | Type | Values | Default (OBS) | Better Cam default |
|---|---|---|---|---|---|
| `video_device_id` | Device | select | Physical cameras | — | System default camera |
| `res_type` | Resolution/FPS Type | select | Device Default (0), Custom (1) | Device Default | same |
| `resolution` | Resolution | combo | Sizes the device lists | — | Device's largest 16:9 |
| `frame_interval` | FPS | select | Match Output FPS (−1), Highest FPS (0), device intervals in 100 ns | Match (−1) | same |
| `video_format` | Video Format | select | Any + formats the device lists (5.2) | Any | same |
| `color_space` | Color Space | select | Default, Rec. 709, Rec. 601, Rec. 2100 (PQ), Rec. 2100 (HLG) | default | same |
| `color_range` | Color Range | select | Default, Limited (partial), Full | default | same |
| `buffering` | Buffering | select | Auto-Detect, Enable, Disable | Auto | same |
| `flip_vertically` | Flip Vertically | bool | | false | same |
| `autorotation` | Apply rotation data from camera | bool | | true | same |
| `hw_decode` | Use hardware decoding when available | bool | | false | **true** (MF hardware MFTs, not FFmpeg) |
| `deactivate_when_not_showing` | Turn off camera when no app is using Better Cam | bool | | false | **true** |

Semantics:

- Device Default: the device's first native type (MF lists preferred types first) at its highest frame rate.
- Color space "default": use `MF_MT_YUV_MATRIX` if present, else Rec. 709 for height ≥ 720, Rec. 601 below.
  Range "default": `MF_MT_VIDEO_NOMINAL_RANGE` if present, else limited for YUV, full for RGB.
- Rec. 2100 PQ/HLG (P010 or HEVC Main10 input): BT.2020 matrix, PQ or HLG EOTF, simple luminance tone map to
  SDR Rec. 709. Full HDR handling belongs to the later HDR Tone Mapping filter.
- Buffering: Disable = always render the newest frame. Enable = queue up to 6 frames and present them on their
  timestamps. Auto = Disable for live cameras (all UVC devices).
- Autorotation: honour `MF_MT_VIDEO_ROTATION` (0/90/180/270).

### 6.2 Device controls ("Configure Video")

Shown only when supported. Range, step and default come from the device. "Auto" switch shown when the device
reports the auto flag as supported.

| Key | Label | Property | Notes |
|---|---|---|---|
| `brightness` | Brightness | ProcAmp 0 | |
| `contrast` | Contrast | ProcAmp 1 | |
| `hue` | Hue | ProcAmp 2 | |
| `saturation` | Saturation | ProcAmp 3 | |
| `sharpness` | Sharpness | ProcAmp 4 | |
| `gamma` | Gamma | ProcAmp 5 | |
| `color_enable` | Color | ProcAmp 6 | Shown as a switch (0/1) |
| `white_balance` | White balance | ProcAmp 7 | Kelvin; Auto common |
| `backlight_compensation` | Backlight compensation | ProcAmp 8 | |
| `gain` | Gain | ProcAmp 9 | |
| `powerline_frequency` | Powerline frequency | KS ProcAmp 13 **(unverified id)** | Select: Disabled 0, 50 Hz 1, 60 Hz 2, Auto 3 (UVC 1.5) |
| `pan` | Pan | CameraControl 0 | Degrees |
| `tilt` | Tilt | CameraControl 1 | Degrees |
| `roll` | Roll | CameraControl 2 | Degrees |
| `zoom` | Zoom | CameraControl 3 | |
| `exposure` | Exposure | CameraControl 4 | log2 seconds (−6 = 1/64 s); shown as "1/64 s"; Auto common |
| `iris` | Iris | CameraControl 5 | |
| `focus` | Focus | CameraControl 6 | Auto common |
| `low_light_compensation` | Low light compensation | KS CameraControl 19 (auto exposure priority) **(unverified id/mapping)** | Switch |

Each row: label, value, slider (device min/max/step), double-click resets to the device default. "Reset all" button in
the card head (every control back to the camera's defaults: every Better Cam auto mode ends; the Better Cam auto
settings and what was learned stay). When the camera, or Better Cam, sets the value, the slider is disabled and
follows `controls-changed`.

**Modes (owner decision 2026-11-18, 9.3 item 1; design in `docs/AUTO-CONTROLS.md`).** A control has up to three modes:
**Manual** (the user's value, the camera's Auto flag off), **Camera auto** (the camera's own Auto flag) and **Better
Cam auto** (Better Cam's controller writes the value; only `exposure` and `white_balance`, and `gain` follows
exposure). A control whose only choice is Manual or Camera auto (focus) keeps the Auto switch; exposure and white
balance show the three-way `Select` instead. Better Cam auto is listed disabled, with the reason as tooltip, when the
camera cannot do it safely (no manual exposure or white balance, units it does not know, writes it ignored). Under the
row it shows its settings (exposure: target brightness, longest exposure, most gain, response; white balance: warmth,
response) and one status line; exposure never gets longer than one frame time unless the user allows it, gain makes
up the rest up to Most gain. Brightness, contrast, hue, saturation, sharpness, gamma and the other controls are never
driven by software.

### 6.3 Output (Better Cam)

| Key | Label | Type | Values | Default |
|---|---|---|---|---|
| `resolution` | Output resolution | combo | Same as camera, 3840x2160, 2560x1440, 1920x1080, 1280x720, 960x540, 640x360, custom `WxH` (even, 160–4096 × 120–2304) | Same as camera |
| `fps` | Output FPS | select | Same as camera, 60, 50, 30, 25, 24, 15 | Same as camera |
| `fit` | Aspect | select | Fit (bars), Fill (crop), Stretch | Fit |
| `background_color` | Background | color | Any opaque color | Black |
| `extra_sizes` | Also offer 720p and 360p to apps | bool | | true |

The output stage composites the chain result over `background_color`, so transparent pixels from keys, masks
or NVIDIA background removal show the background. Background images are a later overlay feature.

### 6.4 Filters

`step` is OBS's slider step. Every filter gets the card behaviour from Better Mic.

**Color Correction** — `color_filter` (v2). Hint: "Brightness, contrast, saturation, hue and gamma".

| Key | Label | Type | Range | Step | Default |
|---|---|---|---|---|---|
| `gamma` | Gamma | slider | −3 … 3 | 0.01 | 0 |
| `contrast` | Contrast | slider | −4 … 4 | 0.01 | 0 |
| `brightness` | Brightness | slider | −1 … 1 | 0.0001 | 0 |
| `saturation` | Saturation | slider | −1 … 5 | 0.01 | 0 |
| `hue_shift` | Hue Shift | slider | −180 … 180 | 0.01 | 0 |
| `opacity` | Opacity | slider | 0 … 1 | 0.0001 | 1 |
| `color_multiply` | Color Multiply | color | | | 0x00FFFFFF |
| `color_add` | Color Add | color | | | 0x00000000 |

**Crop/Pad** — `crop_filter`. Hint: "Trim edges; negative values add padding".

| Key | Label | Type | Range | Default |
|---|---|---|---|---|
| `relative` | Relative | bool | | true |
| `left` | Left | int | −8192 … 8192 | 0 |
| `top` | Top | int | −8192 … 8192 | 0 |
| `right` | Right | int | −8192 … 8192 | 0 (`showIf relative = true`) |
| `bottom` | Bottom | int | −8192 … 8192 | 0 (`showIf relative = true`) |
| `cx` | Width | int | 0 … 8192 | 0 (`showIf relative = false`) |
| `cy` | Height | int | 0 … 8192 | 0 (`showIf relative = false`) |

Negative values pad with transparent pixels. Width/height 0 in absolute mode means "to the edge".

**Scaling/Aspect Ratio** — `scale_filter`. Hint: "Resize the image or force an aspect ratio".

| Key | Label | Type | Values | Default |
|---|---|---|---|---|
| `sampling` | Scale Filtering | select | point, bilinear, bicubic, lanczos, area | bicubic |
| `resolution` | Resolution | combo | None, Base (Canvas) Resolution, 16:9, 16:10, 4:3, 1:1, `WxH` | None |
| `undistort` | Undistort center of image when scaling from ultrawide | bool | | false |

"Base (Canvas) Resolution" means the output resolution. Aspect values follow OBS: an aspect wider than the picture keeps the height and sets the width; otherwise the width stays and the height is set.

**Transform** — `transform` (Better Cam; mirrors OBS Edit Transform). Hint: "Mirror or rotate the image".

| Key | Label | Type | Range | Step | Default |
|---|---|---|---|---|---|
| `flip_h` | Flip horizontal (mirror) | bool | | | false |
| `flip_v` | Flip vertical | bool | | | false |
| `rotation` | Rotation | slider | −180 … 180 | 0.1 | 0 |

Multiples of 90° are exact pixel moves; 90/270 swap width and height. Other angles use bilinear sampling and an
output sized to the rotated bounding box, with transparent corners.

**Sharpen** — `sharpness_filter` (v2). Hint: "Crisper edges (unsharp mask)".

| Key | Label | Type | Range | Step | Default |
|---|---|---|---|---|---|
| `sharpness` | Sharpness | slider | 0 … 1 | 0.01 | 0.08 |

**Apply LUT** — `clut_filter`. Hint: "Color grade with a .cube or .png LUT".

| Key | Label | Type | Range | Step | Default |
|---|---|---|---|---|---|
| `image_path` | Path | path (`*.cube *.png`) | | | "" |
| `clut_amount` | Amount | slider | 0 … 1 | 0.0001 | 1 |
| `passthrough_alpha` | Passthrough Alpha | bool | | | false |

`.cube`: 1D or 3D, any size up to 65³, `DOMAIN_MIN/MAX`. `.png`: 512×512 (64³) square layout and the 1024×32
strip layout. Empty path = pass-through.

**Chroma Key** — `chroma_key_filter` (v2). Hint: "Remove a green or blue screen".

| Key | Label | Type | Range | Step | Default |
|---|---|---|---|---|---|
| `key_color_type` | Key Color Type | select | green, blue, magenta, custom | | green |
| `key_color` | Key Color | color | | | 0x00FF00 (`showIf type = custom`) |
| `similarity` | Similarity (1-1000) | slider int | 1 … 1000 | 1 | 400 |
| `smoothness` | Smoothness (1-1000) | slider int | 1 … 1000 | 1 | 80 |
| `spill` | Key Color Spill Reduction (1-1000) | slider int | 1 … 1000 | 1 | 100 |
| `opacity` | Opacity | slider | 0 … 1 | 0.0001 | 1 |
| `contrast` | Contrast | slider | −4 … 4 | 0.01 | 0 |
| `brightness` | Brightness | slider | −1 … 1 | 0.0001 | 0 |
| `gamma` | Gamma | slider | −1 … 1 | 0.01 | 0 |

**Color Key** — `color_key_filter` (v2). Hint: "Make one color transparent".

| Key | Label | Type | Range | Step | Default |
|---|---|---|---|---|---|
| `key_color_type` | Key Color Type | select | green, blue, red, magenta, custom | | green |
| `key_color` | Key Color | color | | | 0x00FF00 (`showIf type = custom`) |
| `similarity` | Similarity (1-1000) | slider int | 1 … 1000 | 1 | 80 |
| `smoothness` | Smoothness (1-1000) | slider int | 1 … 1000 | 1 | 50 |
| `opacity` | Opacity | slider | 0 … 1 | 0.0001 | 1 |
| `contrast` | Contrast | slider | −4 … 4 | 0.01 | 0 |
| `brightness` | Brightness | slider | −1 … 1 | 0.0001 | 0 |
| `gamma` | Gamma | slider | −1 … 1 | 0.01 | 0 |

**Luma Key** — `luma_key_filter` (v2). Hint: "Make dark or bright areas transparent".

| Key | Label | Type | Range | Step | Default |
|---|---|---|---|---|---|
| `luma_max` | Luma Max | slider | 0 … 1 | 0.0001 | 1 |
| `luma_max_smooth` | Luma Max Smooth | slider | 0 … 1 | 0.0001 | 0 |
| `luma_min` | Luma Min | slider | 0 … 1 | 0.0001 | 0 |
| `luma_min_smooth` | Luma Min Smooth | slider | 0 … 1 | 0.0001 | 0 |

**Image Mask/Blend** — `mask_filter` (v2). Hint: "Shape or blend the image with a picture".

| Key | Label | Type | Values / Range | Step | Default |
|---|---|---|---|---|---|
| `type` | Type | select | `mask_color_filter.effect` Alpha Mask (Color Channel); `mask_alpha_filter.effect` Alpha Mask (Alpha Channel); `blend_mul_filter.effect` Blend (Multiply); `blend_add_filter.effect` Blend (Addition); `blend_sub_filter.effect` Blend (Subtraction) | | `mask_color_filter.effect` |
| `image_path` | Path | path (`*.bmp *.jpg *.jpeg *.gif *.png`; OBS also allows `*.tga`, not in v1) | | | "" |
| `color` | Color | color | | | 0xFFFFFF |
| `opacity` | Opacity | slider | 0 … 1 | 0.0001 | 1 |
| `stretch` | Stretch Image (discard image aspect ratio) | bool | | | false |

**NVIDIA Background Removal** — `nv_greenscreen_filter`. Hint: "AI background removal (RTX only)".

| Key | Label | Type | Values / Range | Step | Default |
|---|---|---|---|---|---|
| `mode` | Mode | select | 0 Quality, 1 Performance, 2 Quality + chair removal, 3 Performance + chair removal | | 0 |
| `threshold` | Threshold | slider | 0 … 1 | 0.05 | 1.0 |
| `processing_interval` | Mask refresh frequency in frames | slider int | 1 … 4 | 1 | 1 |

**NVIDIA Blur** — `nv_blur_filter`. Hint: "Blur the whole image (RTX only)".
**NVIDIA Background Blur** — `nv_background_blur_filter`. Hint: "Blur only the background (RTX only)".

| Key | Label | Type | Range | Step | Default |
|---|---|---|---|---|---|
| `intensity` | Blur Intensity | slider | 0 … 1 | 0.05 | 0.5 |

**Auto Adjust** — `auto_adjust` (Better Cam, phase 16). Hint: "Automatic exposure and white balance in software".

| Key | Label | Type | Range | Step | Default |
|---|---|---|---|---|---|
| `exposure` | Auto exposure | bool | | | true |
| `white_balance` | Auto white balance | bool | | | true |
| `target` | Target brightness | slider | 0.2 … 0.8 | 0.01 | 0.45 |
| `strength` | Strength | slider | 0 … 1 | 0.01 | 1 |
| `speed` | Adaptation speed | slider | 0.05 … 1 | 0.01 | 0.3 |

---

## 7. UI layout

Window: 1200×800 default, 900×600 minimum, frameless, acrylic, dark (Better Mic `tauri.conf.json` with new size).

```
┌ TitleBar ─────────────────────────────────────────────────────────────────────────────────────┐
│ ● Better Cam  LIVE                                          [Pause] [Stop] [Settings]  ─ □ ✕  │
├ Toolbar ──────────────────────────────────────────────────────────────────────────────────────┤
│ [Preset  Studio light  Modified ▾] [Save]                         [Collapse all] [+ Add filter]│
├───────────────────────────────────────────────┬───────────────────────────────────────────────┤
│ left column (scrolls)                          │ right column (scrolls)                        │
│ ┌ Preview (16:9 canvas, [Preview on/off]) ┐   │ banners (error / vcam missing)                │
│ │                                          │   │ SIGNAL CHAIN · APPLIED TOP TO BOTTOM          │
│ └──────────────────────────────────────────┘   │ □ CAMERA IN                                   │
│ ┌ Camera panel ─────────────────────────────┐  │ 01 ┌ Color Correction ▾  ↑ ↓ (on) ✕ ┐          │
│ │ Camera [Logitech BRIO      ▾] [Refresh]   │  │    │ sliders…                      │          │
│ │ Resolution/FPS type [Custom ▾]            │  │    └───────────────────────────────┘          │
│ │ Resolution [1920x1080▾] FPS [30▾] Format [MJPEG▾] │ 02 ┌ NVIDIA Background Blur … ┐          │
│ │ ▸ Advanced: color space, range, buffering,│  │ □ BETTER CAM OUT                              │
│ │   flip, autorotation, hw decode, standby  │  │                                               │
│ └───────────────────────────────────────────┘  │                                               │
│ ┌ Camera controls ▾        [Reset all] ─────┐  │                                               │
│ │ Exposure  1/64 s ───────●───  [Auto ■]    │  │                                               │
│ │ White balance 4600 K ──●────  [Auto ■]    │  │                                               │
│ └───────────────────────────────────────────┘  │                                               │
│ ┌ Output ───────────────────────────────────┐  │                                               │
│ │ Resolution [Same as camera▾] FPS [Same▾] Aspect [Fit▾] Background [■]                        │
│ └───────────────────────────────────────────┘  │                                               │
├ Footer (Stats) ───────────────────────────────┴───────────────────────────────────────────────┤
│ 1920×1080 MJPEG 30 fps (HW) → 1920×1080 30 fps · GPU 1.4 ms · 12 ms latency · In use by 1 app │
└───────────────────────────────────────────────────────────────────────────────────────────────┘
```

Below 1100 px wide the columns stack: preview, camera, controls, output, then the chain. The toast sits above the
footer as in Better Mic. Dialogs (NVIDIA prompt) use Better Mic's backdrop and modal.

### 7.1 Mapping to Better Mic widgets

| Better Cam element | Better Mic widget / class | Change |
|---|---|---|
| Title bar, window buttons, Settings popover | `TitleBar`, `.titlebar`, `.wc`, `Popover`, `.row` settings | Pause replaces Mute; Preview button removed; Settings adds "Virtual camera: Installed · Repair · Remove" row |
| Toolbar, presets, Add filter, Collapse all | `.toolbar`, `PresetMenu`, `AddFilter` | none |
| Banners | `.banner.error`, `.banner.warn` | VB-Cable banner → `VcamBanner` |
| Camera panel | `.panel.devices`, `Select`, Refresh button | More fields; "Advanced" fold uses `.card-title` chevron |
| Camera controls | `.card` + `.params` grid + range input + `.switch` | Each slider row gains an Auto switch in its label line |
| Output panel | `.panel`, `Select` | `ColorField` for background |
| Filter chain | `.chain`, `FilterCard`, terminals | Terminals "Camera in" / "Better Cam out"; params through `ParamField` |
| NVIDIA prompt | `NvidiaPrompt`, `.modal`, `.progress` | Text only |
| Toast with Undo | `Toast` | none |
| Footer | `.footer` + `Meters` | `Stats` (one line, tabular numbers) |

### 7.2 New widgets

| Widget | Purpose | Look |
|---|---|---|
| `Preview` | Canvas, WebGL texture upload, letterboxed to 16:9 box, "Preview off" placeholder, fps badge | `.panel` with no padding, 4 px radius, hairline border |
| `ParamField` | One switch on `ParamDef.type`; handles `showIf` | Reuses `.params label` layout |
| `NumberField` | Integer box with −/+ buttons and wheel; for crop and custom sizes | Input style from `.pm-save input` |
| `ColorField` | Swatch button + popover with hex input, alpha for `alpha: true`, 12 presets; native `<input type=color>` inside the popover | `Popover` |
| `PathField` | Read-only path text (ellipsis, title = full path) + "Browse…" (calls `pick_file`) + clear | `.select` look |
| `ComboField` | `Select` with a final "Custom…" option revealing a text box with validation | `Select` + input |
| `ControlsCard` | Device controls list, Auto switches, Reset all | `.card` |
| `Stats` | Footer line | `.meters-head small` style |
| `VcamBanner` | Missing/outdated/broken virtual camera, Install/Update/Repair button, progress text | `.banner.warn` |

Accessibility: every new control has a label; switches keep `role="switch"` and `aria-checked`; ColorField and
PathField are reachable by keyboard; the preview canvas has `aria-label="Camera preview"`; sliders keep
double-click reset and also reset on `Delete` key.

---

## 8. Build phases

Each phase is one developer brief for one coding session. "Depends on" lists hard prerequisites. Phases on the
same row of the parallel map can run at the same time in separate worktrees.

```
Wave A:  P0 scaffold      P1 SPIKE vcam DLL      P3 capture
Wave B:  P2 transport (P1)                       P4 device controls (P3)
Wave C:  P5 GPU core + engine (P2, P3)
Wave D:  P6 app shell (P0, P5)
Wave E:  P7 preview (P6)   P8 controls UI (P4, P6)   P9 filter framework (P6)   P14 vcam install flow (P1, P6)
Wave F:  P10 geometry  P11 sharpen+LUT  P12 keys+mask  P13 NVIDIA   (all after P9)   P15 robustness (P6, P14)
Wave G:  P16 auto adjust (right after v1 core, P9)   P17 packaging + performance sign-off (all)
```

Shared-file rule for parallel work: `filters/schema.ts`, `fx/mod.rs` (`make()` table) and `styles.css` are touched
by P10–P13. Each of those phases appends its own block and does not reorder others; the second to merge resolves
the trivial conflict.

### P0 — Repo scaffold from Better Mic

- Goal: an empty Better Cam app that builds and looks like Better Mic.
- Files: root `Cargo.toml` (workspace), `package.json`, `LICENSE` (GPL-2.0 text), `tsconfig.json`, `vite.config.ts`, `index.html`,
  `src/main.tsx`, `src/styles.css`, all components listed as "copy unchanged" plus `TitleBar`, `Icon` and
  `FilterCard` copied for now; `src/App.tsx` with toolbar and an empty chain ("Camera in" / "Better Cam out");
  `src/store.ts` reduced to chain, presets, autostart, tray; `src-tauri/*` from Better Mic with audio, DSP and
  VB-Cable code removed; empty crates `crates/vcam-protocol`, `crates/vcam-source`, `crates/vcam-tool`; `package.json` and every `Cargo.toml` carry the licence field `GPL-2.0-or-later`;
  `.gitignore` (node_modules, dist, target).
- Decisions: 3.1 copy matrix; identifier `dev.bettercam.app`; window 1200×800, min 900×600.
- Check: `npm install; npm run build` exits 0. `cargo check --workspace` exits 0. `npm run tauri dev` opens a
  frameless acrylic window titled "Better Cam" with Preset, Save, Collapse all, Add filter (empty menu), terminals
  "Camera in" / "Better Cam out"; tray icon menu "Show Better Cam" / "Quit" works; Settings popover toggles
  autostart.

### P1 — SPIKE: virtual camera DLL visible in real apps (highest risk)

- Goal: prove a Rust media source registered with `MFCreateVirtualCamera` streams in the Windows Camera app, Edge
  and OBS on this machine. No IPC yet: the DLL draws its own test pattern.
- Files: `crates/vcam-protocol/src/lib.rs` (CLSID, names, paths only), `crates/vcam-source/**` (Dll exports,
  class factory, Activator, MediaSource, MediaStream, `IKsControl` stubs, test pattern: moving color bars + a
  white box whose position encodes the frame number; NV12 and RGB32; 1920×1080 and 1280×720 at 30 fps; file log),
  `crates/vcam-tool/src/main.rs` (`register`, `unregister`, `status`; `register` self-elevates via `runas`),
  `docs/SPIKE-VCAM.md`.
- Decisions: 4.3 (Rust; HKLM; Program Files copy; System lifetime + AllUsers; null categories; catch_unwind on
  every COM method; system-memory samples only).
- Check:
  1. `cargo build -p vcam-source -p vcam-tool --release` exits 0.
  2. `target\release\vcam-tool.exe register` shows one UAC prompt and prints `registered` and `started`.
  3. `vcam-tool status` prints `listed: true` and the device's friendly name and symbolic link.
  4. Windows Camera app → switch camera → "Better Cam" shows moving bars for 2 minutes without stalls.
  5. Edge at `https://googlechrome.github.io/samples/image-capture/grab-frame-take-photo.html` shows the bars.
  6. OBS (if installed) → Video Capture Device lists Better Cam and shows the bars. If not listed, add
     `AddProperty(DEVPKEY_Device_ClassGuid, GUID_DEVCLASS_CAMERA)` in `register`, re-run, record the result.
  7. Reboot → Camera app still lists Better Cam.
  8. `vcam-tool unregister` → Camera app no longer lists it; `HKLM\SOFTWARE\Classes\CLSID\{…}` is gone.
  9. `docs/SPIKE-VCAM.md` records: displayed name (with or without "Windows Virtual Camera"), symbolic link
     prefix, results per app, whether System lifetime persisted, Frame Server process account seen in the log.
  If step 4 or 7 fails, the spike tries the documented fallback (Session lifetime from a non-elevated process)
  and records which works.

### P2 — Frame transport

- Goal: frames from another process reach consumers through `frames.bin`.
- Files: `crates/vcam-protocol/src/{lib.rs,header.rs,seqlock.rs,validate.rs}` (layout 2.5, writer and reader,
  validation), `crates/vcam-source/src/frames.rs` (map file, poll, copy into samples, slates, advertised types),
  `crates/vcam-source/assets/slates/*.rle` (+ a small generator in `vcam-tool`), `crates/vcam-tool` commands
  `feed --size WxH --fps N --seconds S` (writes a pattern distinct from P1's: checkerboard + moving stripe) and
  `stats` (prints header counters), `register` gains folder creation with the ACL from 4.3.
- Decisions: 4.3 transport, sync, slates, advertised types; 2.5 layout.
- Check:
  1. `cargo test -p vcam-protocol` passes, including: a writer thread at 1 kHz and a reader thread for 2 s see no
     torn frames; validation rejects offsets past the file end and odd sizes.
  2. `vcam-tool register`, then `vcam-tool feed --size 1920x1080 --fps 60 --seconds 300` with the Camera app on
     Better Cam: checkerboard with moving stripe, smooth.
  3. Stop the feeder: within 1 s the Camera app shows the "Better Cam is not running" slate.
  4. `vcam-tool stats` during feed: `latency_p95_us` ≤ 20000, `repeated` < 1 % of `delivered`.
  5. During feed, `Get-Counter '\PhysicalDisk(_Total)\Disk Write Bytes/sec' -SampleInterval 2 -MaxSamples 15`
     averages < 5 MB/s above an idle baseline. If not, stop and report; the fallback transport becomes a new brief.

### P3 — Capture backend

- Goal: enumerate cameras and formats, open one with a chosen format, receive frames as D3D11 textures or CPU
  buffers.
- Files: `src-tauri/src/capture/{mod.rs,reader.rs,hotplug.rs}`, `src-tauri/src/gpu/mod.rs` (device creation only),
  `src-tauri/examples/capture_probe.rs`.
- Decisions: 4.1 (Source Reader async, D3D manager, hardware transforms, exclude Better Cam, device key).
- Check:
  1. `cargo run -p better-cam --release --example capture_probe -- list` prints each physical camera with its
     formats (subtype, size, fps list) and does not list Better Cam.
  2. `… -- open --device 0 --format mjpeg --size 1280x720 --fps 30 --seconds 10` prints fps within ±10 % of 30
     and `decode: hw` or `decode: sw`, and `texture: yes` when hardware decode is used.
  3. Same with `--format yuy2` (or any raw format the camera has) prints `decode: none`.
  4. Unplug the camera during `open`: prints `device lost` and exits 0. Plug it back: `list` shows it again.

### P4 — Device controls backend

- Goal: read ranges, read and write values and auto flags, reapply stored values after reopen.
- Files: `src-tauri/src/capture/controls.rs`, `capture_probe` subcommands `controls` and `set`.
- Decisions: 4.1 controls (IKsControl first, IAM* fallback, order: auto flags then values), 6.2 keys.
- Check:
  1. `capture_probe controls --device 0` prints a table of supported keys with min/max/step/default/value/can_auto.
  2. `capture_probe set --device 0 --control brightness --value max --measure` opens the camera, prints mean luma
     of 30 frames before and after; after > before.
  3. `capture_probe set --device 0 --control exposure --auto true` then `controls` shows `auto: true`.
  4. `capture_probe set … --reapply-test` closes and reopens the device and prints the reapplied values equal to
     the stored ones.
  5. A device without a property (e.g. pan) does not list it and `set` returns "not supported" without panic.

### P5 — GPU core and headless engine

- Goal: camera → convert → output stage → NV12 → `frames.bin`, running headless.
- Files: `src-tauri/src/gpu/{mod.rs,convert.rs,readback.rs,output.rs}`, `gpu/shaders/{fullscreen_vs,convert_nv12,
  convert_yuy2,convert_p010,convert_i420,convert_rgb,encode_nv12,encode_bgra,output_fit}.hlsl`,
  `src-tauri/src/output_writer.rs`, `src-tauri/src/engine.rs` (render thread, states, stats struct),
  `src-tauri/examples/engine_headless.rs`.
- Decisions: 4.2 (D3D11, runtime `D3DCompile`, RGBA8 working format, R8 + R8G8 encode, staging ring of 3),
  6.1 color space/range rules, 6.3 fit modes, 2.4 states (Stopped/Live only in this phase), negotiated size.
- Check:
  1. `cargo test -p better-cam gpu` passes on WARP (`D3D_DRIVER_TYPE_WARP`): synthetic SMPTE bars in NV12, YUY2,
     I420 and BGRA convert to RGBA and back to NV12 with max error ≤ 2 code values; fit/fill/stretch produce the
     expected rectangles for 4:3 → 16:9.
  2. `cargo run -p better-cam --release --example engine_headless -- --device 0 --output 1920x1080 --fps 30`
     with the Camera app on Better Cam shows the live webcam: white paper is white, skin tones normal, no
     red/blue swap, smooth motion.
  3. The example prints stats every second: capture fps ≈ camera fps, `gpu_ms` < 2, `latency_p95` < 25 ms.

### P6 — App shell wired to the engine

- Goal: the Tauri app drives the engine: camera panel, output panel, Start/Stop, Pause, engine states, stats
  footer, persisted state, autostart launch, tray, close to tray.
- Files: `src-tauri/src/commands.rs`, `src-tauri/src/main.rs`, `src/api.ts`, `src/store.ts`, `src/App.tsx`,
  `src/camera/schema.ts`, `src/components/{SourcePanel,OutputPanel,Stats,TitleBar}.tsx`, `src/styles.css`
  (two-column layout).
- Decisions: 5.1, 5.2, 5.5 (camera, engine, output commands), 5.6 (engine-state, engine-error, stats,
  devices-changed), 2.4 states including Standby and `deactivate_when_not_showing`, 6.1, 6.3, 7, 7.1.
- Check (`npm run tauri dev`):
  1. Pick a camera, Custom 1280x720 30 fps MJPEG, Start → title shows LIVE; Camera app on Better Cam shows it.
  2. Change format to YUY2 → camera reopens within 2 s, Camera app keeps streaming.
  3. Output 640x360 Fill → Camera app (after reopening) shows the cropped 360p image.
  4. Pause → Camera app shows "Camera paused" slate; Pause again → video back.
  5. Stop → slate; Start → video.
  6. With "Turn off camera when no app is using Better Cam" on and the window hidden to tray: camera LED off
     when the Camera app is closed; opening the Camera app turns it on within 3 s.
  7. Quit, relaunch: same camera, format and output settings restored.
  8. Footer shows capture and output size, fps, GPU ms, latency and "In use by 1 app" while the Camera app watches.

### P7 — Live preview

- Goal: in-window preview over a Tauri Channel.
- Files: `src-tauri/src/preview.rs`, `src-tauri/src/commands.rs` (`set_preview`, `preview_ack`),
  `src/components/Preview.tsx`, `src/store.ts`, `src/App.tsx`, `src/styles.css`.
- Decisions: 4.4 (Channel, WebGL, ≤ 960×540, 30 fps, one frame in flight, visible-only).
- Check:
  1. Preview shows the processed image; waving a hand looks smooth; the fps badge shows 29–30.
  2. Minimizing or hiding to tray stops preview frames (`stats` shows preview fps 0); restoring resumes.
  3. Task Manager: Better Cam plus its WebView2 processes use ≤ 3 % more CPU with preview on than off at 1080p30.
  4. Preview off → placeholder shown, no frames sent.

### P8 — Camera controls UI and persistence

- Goal: the Camera controls card with device ranges, Auto switches, Reset all, stored per device.
- Files: `src/components/ControlsCard.tsx`, `src/camera/schema.ts` (labels, units, exposure formatting),
  `src/store.ts` (`controls` state), `src-tauri/src/commands.rs` (controls commands, `controls-changed` 1 Hz),
  `src-tauri/src/engine.rs` (reapply on open).
- Decisions: 6.2, 4.1 persistence and key fallback, 5.1 `controls`.
- Check:
  1. The card lists only supported controls with the device's ranges (compare with `capture_probe controls`).
  2. Dragging Brightness changes the preview and the Camera app within 200 ms.
  3. Turning Auto off for Exposure enables the slider; turning it on disables it and the value follows the device.
  4. Unplug and replug the camera (same port, then another port): values come back.
  5. Restart the app: values come back. Reset all restores device defaults.

### P9 — Filter framework and Color Correction

- Goal: GPU filter chain with live updates, new param types and widgets, file picker, preset format v1, first
  filter.
- Files: `src-tauri/src/fx/{mod.rs,shader_filter.rs,color.rs,assets.rs}`, `gpu/shaders/color.hlsl`,
  `src-tauri/src/file_dialog.rs`, `src-tauri/src/commands.rs` (`set_chain`, `pick_file`, preset object),
  `src/filters/{schema.ts,chain.ts}`, `src/components/{ParamField,NumberField,ColorField,PathField,ComboField,
  FilterCard}.tsx`, `src/styles.css`, `package.json` script `schema:export`, `src-tauri/src/fx/defaults.json`.
- Decisions: 4.5, 4.7 (GPL; port OBS `color_correction_filter.effect`), 5.3, 5.4, 6.4 Color Correction.
- Check:
  1. `cargo test -p better-cam fx` passes: `Chain::apply` keeps instances by id+kind; every registered kind
     accepts `defaults.json`; Color Correction at defaults is identity within 1/255 on a gradient image (WARP).
  2. `npm run build` exits 0.
  3. Add Color Correction, drag Saturation to −1: preview and Camera app turn gray within 100 ms; no stutter while
     dragging.
  4. Toggle, collapse, reorder (drag and arrows), remove with Undo all work as in Better Mic.
  5. Save preset "Test", change a value (shows Modified), load "Test" (value restored); the file in
     `%APPDATA%\dev.bettercam.app\presets\Test.json` matches 5.4.
  6. ColorField and PathField work by keyboard only.

### P10 — Geometry filters (parallel with P11–P13)

- Goal: Crop/Pad, Scaling/Aspect Ratio, Transform.
- Files: `src-tauri/src/fx/geometry.rs`, `gpu/shaders/{crop,scale_bicubic,scale_lanczos,scale_area,transform}.hlsl`,
  `src/filters/schema.ts` (three entries), crop layout in `ParamField`.
- Decisions: 6.4 entries, `output_size` changes sizes downstream.
- Check: `cargo test -p better-cam geometry` passes: crop 10 px each side on 100×100 → 80×80; −10 → 120×120 with
  transparent border; absolute 50×40 at (5,5) → 50×40; rotation 90 on 160×90 → 90×160; flip_h mirrors a known
  pixel; scale "4:3" on 1920×1080 → 1920×1440; each sampling mode runs. Manual: each filter visibly correct in
  preview and the Camera app.

### P11 — Sharpen and Apply LUT (parallel)

- Goal: Sharpen and Apply LUT with `.cube` and `.png`.
- Files: `src-tauri/src/fx/{sharpen.rs,lut.rs}`, `fx/assets.rs` (`.cube` parser, PNG LUT layouts),
  `gpu/shaders/{sharpen,lut3d}.hlsl`, `src/filters/schema.ts`, test assets in `src-tauri/tests/assets/`.
- Decisions: 6.4 entries; LUT formats in 6.4.
- Check: `cargo test -p better-cam lut sharpen` passes: generated identity `.cube` (17³ and 33³) and identity PNG
  (512×512) leave a gradient image within 2/255; `clut_amount = 0` is identity; a 1D `.cube` works; malformed
  `.cube` returns an error without panic; sharpness 0 is identity. Manual: a warm LUT visibly warms the preview.

### P12 — Keys and Image Mask/Blend (parallel)

- Goal: Chroma Key, Color Key, Luma Key, Image Mask/Blend.
- Files: `src-tauri/src/fx/{keys.rs,mask.rs}`, `gpu/shaders/{chroma_key,color_key,luma_key,mask_color,mask_alpha,
  blend_mul,blend_add,blend_sub}.hlsl`, `src/filters/schema.ts`.
- Decisions: 6.4 entries; transparent pixels show the output background (6.3).
- Check: `cargo test -p better-cam keys mask` passes: pure 0x00FF00 frame with chroma defaults → alpha < 0.05;
  mid-gray frame → alpha > 0.95; color key on pure red with type red → alpha < 0.05; luma key `luma_min = 0.5` on
  black → alpha 0, on white → 1; color mask with a left-white/right-black PNG → left alpha 1, right alpha 0;
  `stretch` false keeps the image aspect. Manual: green cloth behind the user disappears to the background color.

### P13 — NVIDIA Video Effects (parallel)

- Goal: probe, install prompt, and the three NVIDIA filters.
- Files: `src-tauri/src/fx/nvidia.rs` (from Better Mic `dsp/nvidia.rs`: GPU detection kept; VFX paths, version,
  loader, effects), `src-tauri/src/install_nvidia.ps1`, `src-tauri/src/commands.rs`, `gpu/shaders/{nv_mask,
  nv_blend}.hlsl`, `src/components/{NvidiaPrompt,FilterCard,AddFilter}.tsx`, `src/filters/schema.ts`,
  `src/store.ts`.
- Decisions: 4.6, 6.4 NVIDIA entries, adapter choice in 4.2.
- Check:
  1. `Invoke-WebRequest -Method Head` on each of the four installer URLs returns 200 (else update URLs from
     NVIDIA's resources page and note it in the brief's report).
  2. `cargo test -p better-cam nvidia -- --nocapture` loads the SDK when present (skips with a message when not),
     runs 100 frames of greenscreen on a 1280×720 test image, all outputs finite, prints model load time.
  3. On an RTX machine without the SDK: the prompt appears once, downloads, checks the NVIDIA signature, installs;
     NVIDIA filters become available without restart (window focus re-probe).
  4. Background Removal (Quality): background turns to the output background color; Background Blur blurs only
     the background; `processing_interval` 4 lowers GPU ms in Stats.
  5. On a non-RTX machine: Add filter marks NVIDIA items "RTX only"; an NVIDIA card in a loaded preset is bypassed
     with "Needs an NVIDIA RTX GPU".

### P14 — Virtual camera install, update and repair from the app

- Goal: `--vcam-install` / `--vcam-uninstall` in `better-cam.exe`, status, banner, Settings row.
- Files: `src-tauri/src/vcam_admin.rs`, `src-tauri/src/main.rs` (arg handling before the builder),
  `src-tauri/src/commands.rs` (`vcam_status`, `vcam_install`, `vcam_uninstall`), `src/components/VcamBanner.tsx`,
  `src/components/TitleBar.tsx`, `src/store.ts`; `tauri.conf.json` resources (DLL), `package.json` `vcam:build`.
- Decisions: 4.3 registration steps 1–8, lifetime (as confirmed by P1), status states, versioned DLL name.
- Check:
  1. From a clean state (`vcam-tool unregister`): launch the app → warn banner "Install virtual camera"; click →
     one UAC prompt → banner disappears; Camera app lists Better Cam.
  2. Refuse UAC → toast, banner stays, app keeps working.
  3. Bump the DLL build number, rebuild, relaunch → banner "Update virtual camera" → after update
     `HKLM\SOFTWARE\BetterCam\Vcam\Build` shows the new build and the Camera app still works (old DLL removed or
     scheduled for deletion).
  4. Settings → Remove → UAC → Better Cam disappears from the Camera app; `%ProgramData%\BetterCam` is gone.
  5. `icacls "$env:ProgramData\BetterCam"` lists only SYSTEM, Administrators, LOCAL SERVICE and the installing user.

### P15 — Robustness and failure handling

- Goal: every row of 4.9 behaves as specified.
- Files: `src-tauri/src/engine.rs` (Reconnecting, retries, device-removed GPU recovery), `capture/hotplug.rs`,
  `capture/reader.rs` (error mapping), `src/store.ts` and banners, `src/App.tsx`.
- Decisions: 4.9, 2.4.
- Check (manual matrix, `npm run tauri dev`, Camera app on Better Cam):
  1. Open the physical camera in another app that holds it, then Start → banner "Camera is in use…", "Starting
     camera…" slate; close the other app → video within 5 s.
  2. Unplug during Live → slate, Reconnecting; replug → video within 5 s, controls reapplied.
  3. Pick a format, then swap in a camera without it (same name not needed): toast and device default used.
  4. Turn off "Let desktop apps access your camera" → banner with Settings link; turn on → Start works.
  5. `Restart-Service FrameServer -Force` (elevated shell) → Camera app reopened shows video; app recovers alone.
  6. Simulate device removal with `dxcap -forcetdr` (Graphics Tools) or a driver restart (Win+Ctrl+Shift+B):
     video returns within 2 s.
  7. Kill `better-cam.exe` in Task Manager → Camera app shows "Better Cam is not running" within 1 s.

### P16 — Software Auto adjust filter (built right after v1 core)

- Goal: `auto_adjust` filter: frame statistics on the GPU (mip chain to 16×16, read back two frames later),
  temporal smoothing, exposure gain and gray-world white balance in a shader.
- Files: `src-tauri/src/fx/auto_adjust.rs`, `gpu/shaders/{stats_downsample,auto_adjust}.hlsl`,
  `src/filters/schema.ts`.
- Decisions: 6.4 Auto Adjust; owner decision 9.3 item 1.
- Check: `cargo test -p better-cam auto_adjust` passes: a dark gray input converges to mean luma within ±0.05 of
  `target` in ≤ 60 frames at `speed` 0.3; a blue-tinted gray converges to neutral (|R−B| < 0.03); no oscillation
  over 300 frames. Manual: covering half the lens with a hand changes brightness smoothly, without flicker.

### P17 — Packaging and performance sign-off

- Goal: installer, portable zip, uninstall cleanup, and the 4.8 targets measured.
- Files: `src-tauri/tauri.conf.json` (NSIS, `installerHooks`), `src-tauri/windows/hooks.nsh` (post-install runs
  `better-cam.exe --vcam-install --user-sid <SID>`, pre-uninstall runs `--vcam-uninstall`), `scripts/portable.ps1`,
  `docs/PERF.md` (results).
- Decisions: 4.3 registration, 4.8 targets.
- Check:
  1. `npm run vcam:build; npm run tauri build` produces `src-tauri\target\release\bundle\nsis\Better Cam_*_x64-setup.exe`.
  2. On a fresh Windows 11 user: installer → one UAC prompt total → app opens; Teams or Zoom lists Better Cam and
     the remote side sees the processed video (two-machine test; VCamSample reports a Teams remote-side issue).
  3. Uninstall → Better Cam gone from the Camera app, `%ProgramData%\BetterCam` and the HKLM keys gone.
  4. Portable zip: unzip, run, banner install works.
  5. `docs/PERF.md` lists measured values for every 4.8 row at 1080p60 and 4K30 (or the camera's max) and each
     meets its target, or the gap is listed as a follow-up.

---

## 9. Risks and open questions

### 9.1 Risks

| # | Risk | Likelihood / impact | Mitigation |
|---|---|---|---|
| 1 | The Rust media source fails in Frame Server (missing interface, threading, attribute delegation) | Medium / blocks the product | P1 spike first; port VCamSample's class structure; trace every COM call in the log; compare with the two public Rust sources |
| 2 | File-backed transport causes disk writes | Medium / SSD wear, stutter | `FILE_ATTRIBUTE_TEMPORARY`; P2 measures; fallback handle-duplication transport |
| 3 | DirectShow apps (OBS, some Zoom builds) do not list an MF virtual camera | Medium / lost users | P1 tests OBS; elevated helper adds `DEVPKEY_Device_ClassGuid` if needed |
| 4 | Displayed name gets " Windows Virtual Camera" appended | High (documented) / cosmetic | Accept; record in P1 |
| 5 | A DLL bug crashes Frame Server and kills every camera app | Low / severe | `catch_unwind` everywhere, header validation, no allocations proportional to untrusted sizes, fuzz the validator in `vcam-protocol` tests |
| 6 | DLL locked by Frame Server during updates | High / failed update | Versioned file names, delay-delete, Frame Server restart only on uninstall |
| 7 | Teams preview works but the remote side sees nothing (reported by VCamSample) | Unknown / serious for calls | P17 two-machine test; if hit, try GPU samples when a D3D manager is offered, then `MF_DEVICESTREAM_FRAMESERVER_SHARED` and timestamp variants |
| 8 | Software MJPEG decode too slow at 4K30 on weak CPUs | Medium / dropped frames | Prefer HW MFT; Stats shows "software decode"; suggest 1080p |
| 9 | NVIDIA URLs or SDK API change | Medium / NVIDIA filters break | URLs in one table, P13 checks them; version gate ≥ 0.7.6; filters degrade to bypass |
| 10 | Multi-GPU laptops: NVIDIA filter needs the dGPU, decoder on iGPU | Medium / slow or failing NVIDIA | Choose the NVIDIA adapter when an NVIDIA filter is present; decoder follows the device manager |
| 11 | Preview IPC cost higher than estimated | Low / CPU | Size and fps caps; WebView2 shared-buffer upgrade path |
| 12 | GPL obligations not met in a release (missing source offer, licence file or OBS notices) | Low / licence | `LICENSE` in repo root; OBS notice and source file named at the top of each ported file; public source repo; NVIDIA SDK never bundled |
| 13 | Another local user writes `frames.bin` | Low / spoofed video | ACL limited to the installing user and LOCAL SERVICE |
| 14 | Device control ids or ranges odd on some cameras (exposure units) | Medium / confusing UI | Show raw values with units only where the API defines them; log unsupported ids |

### 9.2 Unverified facts

1. File-backed shared memory is required because Frame Server's namespace is isolated (third-party report).
2. `FILE_ATTRIBUTE_TEMPORARY` keeps mapped-view pages off the disk under normal memory pressure.
3. System lifetime + AllUsers, created by the elevated helper, persists across reboots and works for non-admin
   users' apps (docs say System persists; the combination is untested here).
4. Apps show "Better Cam" with " Windows Virtual Camera" appended (docs say so; real display untested).
5. DirectShow consumers need `DEVPKEY_Device_ClassGuid` set to the Camera class, which needs elevation (third-party).
6. Frame Server requires `IKsControl` on the media source; the full interface list taken from VCamSample.
7. `IMFVirtualCamera::CreateSyncEvent` duplicates the handle into Frame Server.
8. KS ids: powerline frequency 13, auto exposure priority 19; "Low light compensation" = auto exposure priority.
9. Exact MF error codes for "camera in use".
10. NVIDIA Video Effects 0.7.6 installer URLs (from the StreamFX wiki, not NVIDIA's page).
11. NVIDIA background removal GPU time and software MJPEG decode CPU estimates in 4.8.
12. Nested cargo build from `build.rs` deadlocks (third-party report).
13. Teams remote-side issue applies to our design.
14. Handle-duplication fallback via `SendCameraProperty` works from a non-admin process.

Verified (for contrast): `MFCreateVirtualCamera` parameters, Windows build 22000 minimum, admin needed for
AllUsers, default categories; `AddDeviceSourceInfo` semantics; HKLM-only registration and Local Service / Local
System access needs (VCamSample README); custom media source rules (Microsoft Frame Server custom media source
guide); this user's token lacks `SeCreateGlobalPrivilege` (`whoami /priv`); every OBS id, key, range and default in
section 6 (OBS `master`); libdshowcapture LGPL-2.1 and its `VideoFormat` values; wgpu dropped D3D11 in v0.19;
OBS's NVIDIA filter uses `NvCVImage_InitFromD3D11Texture`, `NV_VIDEO_EFFECTS_PATH` and the default install dir;
Video Effects 0.7.6 is the current release for RTX 20–50 on NVIDIA's resources page.

### 9.3 Owner decisions (2026-11-17)

1. **Auto mode:** v1 ships the camera's own Auto switches. The software "Auto adjust" filter (P16) is built right
   after v1 core. ~~Software never drives the device controls.~~ **Reversed by the owner on 2026-11-18** (see
   `docs/AUTO-CONTROLS.md`): Better Cam may write a camera's exposure, gain and white balance, but only for a control
   the user switched to **Better Cam auto** (a third mode next to Manual and Camera auto). Nothing else is ever
   written by software, and nothing changes for a user who never picks that mode. Auto Adjust stays a filter on the
   picture; it never touches the device.
2. **Licence:** Better Cam is GPL-2.0-or-later. OBS shaders and filter logic are ported directly (4.7).
3. **Install scope:** for all users (AllUsers).
4. **Presets:** store the filter chain only, not device controls.
5. **Distribution:** installer plus portable zip.
6. **"Turn off camera when no app is using Better Cam":** on by default.
7. **Camera when Better Cam is closed:** other apps see the "Better Cam is not running" slate.
8. **Background behind removed backgrounds:** solid color only in v1.
9. **Default output:** "Same as camera", plus 720p and 360p.
10. **Portable install risk accepted.** The elevated helper installs the camera DLL found next to its own exe. From a user-writable folder (a portable zip, a dev build) another program running as that user could swap that DLL before the install, and it would then run inside the camera service. The owner accepts this for the portable build; the installer is per-machine. No code-signature check and no refusal of user-writable sources in v1.
