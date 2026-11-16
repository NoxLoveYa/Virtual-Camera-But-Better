# SPIKE-VCAM: P1 results

Spike P1 of `docs/PLAN.md`: a Rust Media Foundation media source DLL, registered with `MFCreateVirtualCamera`, loaded
by Windows Frame Server, no IPC (the DLL draws its own test pattern).
Machine: Windows 11 Pro build 26200, x64, split-token admin user, one physical USB webcam ("W4GS").

**Verdict: the approach lives.** The primary design (Rust `windows` 0.62 `#[implement]`, HKLM COM registration, DLL in
`%ProgramFiles%`, `MFCreateVirtualCamera(System, AllUsers, null categories)`) worked first time. No fallback was
needed. Consumers verified here: a Media Foundation Source Reader (`vcam-tool capture-test`) and a DirectShow
enumeration (`vcam-tool status`). Still to do by hand: Camera app, Edge, OBS, reboot (list at the end).

## What was built

| Path | Content |
|---|---|
| `Cargo.toml`, `.cargo/config.toml`, `.gitignore` | workspace (`crates/vcam-protocol`, `vcam-source`, `vcam-tool`); `panic = "unwind"` in release (catch_unwind needs it); static CRT so the DLL has no vcruntime dependency |
| `crates/vcam-protocol/src/lib.rs` | CLSID `{3C0EE391-6A3A-4D0E-A4E5-8243EA287A41}`, name "Better Cam", paths, sizes, and the test-pattern geometry (`pattern` module: shared by the DLL renderer and the tool's detector) |
| `crates/vcam-source` (cdylib `better_cam_vcam.dll`, also rlib for tests) | `lib.rs` exports; `factory.rs`; `activator.rs` (IMFActivate, attributes delegated to an inner store); `source.rs` (IMFMediaSource2 = Ex + base, IMFGetService, IKsControl, IMFSampleAllocatorControl); `stream.rs` (IMFMediaStream2, IKsControl, IMFAttributes, pacing thread); `mediatype.rs`; `pattern.rs`; `guard.rs` (catch_unwind); `log.rs` (file log); `attrs.rs` (one macro for the 30 IMFAttributes methods) |
| `crates/vcam-tool` | `register`, `unregister` (self-elevating), `status`, `capture-test` |

Windows crate: `windows` 0.62.2 (no API forced a different version). crates.io now also shows `windows-core` 0.100.0;
not used.

## Check results

### 1. Build and tests

```
cargo build -p vcam-source -p vcam-tool --release   -> Finished `release` profile (exit 0)
cargo test --workspace
  vcam-protocol  3 passed     vcam-source  14 passed     vcam-tool  2 passed     (0 failed)
cargo clippy --workspace --all-targets, cargo fmt --check -> clean
```

The 14 `vcam-source` tests include in-process runs through `DllGetClassObject` -> Activator -> MediaSource ->
MediaStream -> samples (all four media types, 30 fps pacing, box step per frame, Stop/Start -> `MEUpdatedStream`,
`IKsControl` -> `ERROR_SET_NOT_FOUND`, panic containment). Release DLL: 552 448 bytes; exports `DllGetClassObject` and
`DllCanUnloadNow`; imports only system DLLs (no vcruntime). The linker prints `LNK4104 ... should be PRIVATE` for
the two exports because the crate is `cdylib` + `rlib`; harmless, `dumpbin /exports` shows both.

### 2. `register`

```
requesting elevation for 'register' (UAC prompt)...
register: running elevated: true
dll copied: C:\Program Files\Better Cam\vcam\better_cam_vcam_1.d30ad8c1.dll
clsid key written: HKLM\SOFTWARE\Classes\CLSID\{3C0EE391-6A3A-4D0E-A4E5-8243EA287A41}
data dir ready: C:\ProgramData\BetterCam\logs
registered
started
exit=0
```

One UAC prompt, answered within seconds.

### 3. `status`

```
registry clsid key present: true
mf device: 'W4GS' link '\\?\usb#vid_0c45&pid_636d&mi_00#9&3171a958&0&0000#{e5323777-f976-4f5b-9b55-b94699c46e44}\global'
mf device: 'Better Cam (Windows Virtual Camera)' link '\\?\swd#vcamdevapi#d8f8cb274e00e6fdbf536ad683ae8e406a97548c1c136231882660f69513bee9#{e5323777-f976-4f5b-9b55-b94699c46e44}\{fcebba03-9d13-4c13-9940-cc84fcd132d1}'
listed: true
friendly name: Better Cam (Windows Virtual Camera)
symbolic link: \\?\swd#vcamdevapi#d8f8cb27...bee9#{e5323777-f976-4f5b-9b55-b94699c46e44}\{fcebba03-9d13-4c13-9940-cc84fcd132d1}
directshow device: 'W4GS'
directshow device: 'Better Cam (Windows Virtual Camera)'
directshow listed: true
```

- Displayed name: **"Better Cam (Windows Virtual Camera)"**, suffix in parentheses, in both Media Foundation and
  DirectShow.
- Symbolic link prefix: `\\?\swd#vcamdevapi#<64 hex>#{e5323777-f976-4f5b-9b55-b94699c46e44}\{fcebba03-9d13-4c13-9940-cc84fcd132d1}`
  (`swd#vcamdevapi`, KSCATEGORY_VIDEO_CAMERA, then a per-device GUID). The Frame Server log also shows an activation
  through the KSCATEGORY_CAPTURE interface of the same device (`...#{65e8773d-8f56-11d0-a3b9-00a0c9223196}\{fcebba03-...}`)
  for another client process; I did not identify that client.
- The hash and trailing GUID stay the same after unregister + register (same parameters -> same device).

### 4. `capture-test`

```
=== nv12 1920x1080                                =========== nv12 1280x720
negotiated: nv12 1920x1080                        negotiated: nv12 1280x720
frames read: 120 in 3.97s                         frames read: 120 in 3.97s
wall fps: 30.00   timestamp fps: 30.00            wall fps: 29.99   timestamp fps: 30.00
max gap: 37 ms                                    max gap: 34 ms
box step 16px/frame: 120 decoded, 0 out of seq    box step 16px/frame: 120 decoded, 0 out of seq
black frames: 0                                   black frames: 0
PASS (Nv12 1920x1080)                             PASS (Nv12 1280x720)

=== rgb32 1280x720
negotiated: rgb32 1280x720 / frames read: 120 in 3.97s / wall fps: 29.99 / max gap: 34 ms
box step 16px/frame: 120 decoded, 0 out of sequence, 0 without a box / black frames: 0
PASS (Rgb32 1280x720)
```

The reader runs with `MF_READWRITE_DISABLE_CONVERTERS`, so these are the device's native types. The test fails on:
negotiated type different from the request, fps outside 27-33, a gap over 250 ms, any frame whose box is not exactly one
slot after the previous, any frame without a box, any black frame, a reader error or end of stream, and a 60 s + N/20 s
watchdog. No check was loosened.

### 5. Two-minute run

```
vcam-tool capture-test --frames 3600
frames read: 3600 in 119.97s
wall fps: 30.00   timestamp fps: 30.00   max gap: 36 ms
box step 16px/frame: 3600 frames decoded, 0 out of sequence, 0 without a box
black frames: 0   stream ticks: 1
PASS (Nv12 1920x1080)
```

Frame Server host from the DLL log (`%ProgramData%\BetterCam\logs\vcam-30576.log`, first line):

```
---- vcam log start: process 'C:\WINDOWS\System32\svchost.exe' pid 30576 session 0 account 'LOCAL SERVICE' dll 'C:\Program Files\Better Cam\vcam\better_cam_vcam_1.d30ad8c1.dll' ----
```

`Get-CimInstance Win32_Service FrameServer` agrees: `StartName NT AUTHORITY\LocalService`, same pid. No `PANIC` line in
any Frame Server log. A log line about "client pid N" in `Activator::ActivateObject` names the app that asked for the
camera (attribute `{5F8D322E-0FE4-43E4-9E50-D83ECD9FC2B8}`).

### 6. `unregister`, `status`, `register`

```
removed
delete HKLM\SOFTWARE\Classes\CLSID\{3C0EE391-...}: WIN32_ERROR(0)
delete HKLM\SOFTWARE\BetterCam\Vcam: WIN32_ERROR(0)
some DLLs are loaded by Frame Server; stopping the service
FrameServer stop requested, stopped: false
deleted C:\Program Files\Better Cam\vcam\better_cam_vcam_1.d30ad8c1.dll
unregistered
---- status afterwards
registry clsid key present: false      listed: false      directshow listed: false
reg query HKLM\SOFTWARE\Classes\CLSID\{3C0EE391-...}  -> "unable to find the specified registry key"
C:\Program Files\Better Cam  -> gone
```

Then `register` again: `registered`, `started`, `listed: true`, `capture-test --frames 90` PASS (29.98 fps). The camera
is left installed.

## Findings

**Lifetime.** `Lifetime_System` + `Access_AllUsers`, created by the elevated tool, which then called `Start()` and
`Shutdown()` and exited: the camera stays listed and activatable from a non-elevated process, and survives the tool's
exit. Session-lifetime fallback was not needed and is not implemented. Reboot persistence is not yet verified (manual
item 4).

**DirectShow fallback not needed.** `ICreateDevEnum` on `CLSID_VideoInputDeviceCategory` lists the camera without
`AddProperty(DEVPKEY_Device_ClassGuid, GUID_DEVCLASS_CAMERA)`. `vcam-tool register --dshow-class` implements that
fallback (untested because unneeded). OBS itself is not installed here; the DirectShow enumeration is the stand-in.

**The DLL also loads inside the consumer.** Besides Frame Server (svchost, session 0, LOCAL SERVICE), the DLL was
loaded in the calling process (`vcam-tool.exe`, session 1, the user's account; log `vcam-31736.log`): a short probe
`DllGetClassObject` -> `Activator` (2 attributes) -> `CreatePresentationDescriptor` -> `KsProperty` -> `Shutdown`, no
streaming. Every consumer app that opens the camera does this. The plan's process-model section assumes Frame Server
only; the DLL must work as LOCAL SERVICE *and* as the user, must stay cheap at creation (no file or shared-memory work in
`DllGetClassObject`, `Activator` or `CreatePresentationDescriptor`), and its log directory must be writable by both.

**What Frame Server asks the source** (all from the log):
- `IMFGetService::GetService` for `{2032C7EF-...}` (IMFDeviceController2), `{A1F58958-...}` (IMFDeviceController),
  `{E52F643C-408F-4161-BD60-25520B0C4CD7}`, `{B91EBFEE-CA03-4AF4-8A82-A31752F4A0FC}`: answered `MF_E_UNSUPPORTED_SERVICE`.
- `IMFSampleAllocatorControl::GetAllocatorUsage`, then `IMFMediaSource2::SetMediaType`.
- `IKsControl::KsProperty` on the source with sets `{00000000-...}` id 0 flags 0x100, `{1CB79112-C0D2-4213-9CA6-CD4FDB927972}` id 4,
  `{8C134960-51AD-11CF-878A-94F801C10000}` id 12, `{720D4AC0-7533-11D0-A5D6-28DB04C10000}` id 2, and `KsEvent` on
  `{C6E13370-30AC-11D0-A18C-00A0C9118956}` id 8: answered `ERROR_SET_NOT_FOUND`; nothing broke.
- `SetD3DManager` was never called in these runs (Source Reader without a D3D manager), so the GPU path is untested.
- Frame Server creates a new source per activation (several per capture-test run); state must not live in the instance.

**Attributes that were enough.** Activator: `MFT_TRANSFORM_CLSID_Attribute` only. Not set: `MF_VIRTUALCAMERA_PROVIDE_ASSOCIATED_CAMERA_SOURCES`
(VCamSample sets it; not needed here). Source attributes: the activator's store (shared), plus
`MF_DEVICEMFT_SENSORPROFILE_COLLECTION` (Legacy and HighFrameRate profiles as in Microsoft's sample). Stream:
`MF_DEVICESTREAM_STREAM_CATEGORY=PINNAME_VIDEO_CAPTURE`, `STREAM_ID=0`, `FRAMESERVER_SHARED=1`,
`ATTRIBUTE_FRAMESOURCE_TYPES=Color`, set on both the stream descriptor and the stream's own attribute store.
`IMFSampleAllocatorControl::GetAllocatorUsage` returns `MFSampleAllocatorUsage_UsesCustomAllocator` (the source makes
its own samples); whether `DoesNotAllocate` would also work was not tried.

**Pacing is the source's job.** Frame Server does not pace a live source. `RequestSample` queues the token; one thread
per stream waits for the next 33.33 ms deadline (`timeBeginPeriod(1)`, condvar timeout), draws the pattern into
`MFCreateSample` + `MFCreate2DMediaBuffer`, stamps `MFGetSystemTime` based times and posts `MEMediaSample`. Measured
30.00 fps over 3600 frames.

## Plan section 9.2: unverified facts

| # | Fact | Result |
|---|---|---|
| 1 | File-backed shared memory is needed (isolated namespace) | not tested (no IPC in P1) |
| 2 | `FILE_ATTRIBUTE_TEMPORARY` keeps pages off disk | not tested |
| 3 | System lifetime + AllUsers from the elevated helper persists and works for non-admin users' apps | **partly confirmed**: persists after the helper exits; works from a non-elevated process of an admin account (split token). Reboot and a real standard user: not tested |
| 4 | Apps show "Better Cam" with " Windows Virtual Camera" appended | **confirmed, form differs**: "Better Cam (Windows Virtual Camera)" |
| 5 | DirectShow consumers need `DEVPKEY_Device_ClassGuid` | **refuted on this machine** for DirectShow enumeration (listed without it); OBS open/stream not tested |
| 6 | Frame Server requires `IKsControl`; interface list from VCamSample | **list works** (all interfaces present, every call seen); removing one was not tried, so "required" is not proven |
| 7 | `CreateSyncEvent` duplicates the handle into Frame Server | not tested |
| 8 | KS ids (powerline 13, auto exposure priority 19) | not tested |
| 9 | Exact MF error codes for "camera in use" | not tested |
| 10-11 | NVIDIA URLs and estimates | not in scope |
| 12 | Nested cargo build from `build.rs` deadlocks | not tested |
| 13 | Teams remote side shows nothing | not tested |
| 14 | Handle duplication via `SendCameraProperty` from a non-admin process | not tested |

## Traps hit

1. **Frame Server does not pace.** See above; without the pacing thread frames would be generated as fast as requested.
2. **windows-rs interface pointers are not `Send`.** The pacing thread needs the event queue and the sample tokens; both are
   free-threaded by MF contract, wrapped in a small `Agile<T>` with a `SAFETY` comment. Edition 2021 closures capture the
   field, not the wrapper (`let queue = queue;` forces the whole wrapper).
3. **A thread in a DLL cannot be unloaded safely.** `DllCanUnloadNow` returns `S_FALSE` once any pacing thread ever ran, so
   the module stays loaded until process exit (it is pinned by Frame Server anyway).
4. **Locked DLL on unregister.** The first delete failed (loaded by Frame Server). `unregister` then requests a
   `FrameServer` stop and retries; the log said `stopped: false` yet the retry succeeded, and Frame Server came back with a
   new pid. Keep this order in P14: `Remove()` first, registry second, files last.
5. **Versioned DLL name** = FNV-1a of the content, so re-registering the same build re-uses the file (an identical locked
   copy is not rewritten), and a changed build gets a new name.
6. **windows 0.62 module layout:** `CLSID_SystemDeviceEnum` and `CLSID_VideoInputDeviceCategory` live under
   `Win32::Media::MediaFoundation`; `IPropertyBag::Read` needs the `Win32_System_Ole` feature; `IMFMediaEventQueue`/`IMFMediaEventGenerator`
   take event types as `u32` (`MENewStream.0 as u32`); `#[implement]` needs the exact `*_Impl` trait set
   (`IMFMediaSource2_Impl` also needs `IMFMediaSourceEx_Impl` and `IMFMediaSource_Impl`), and `ComObject::to_interface` only works for
   interfaces listed in `#[implement(...)]` (use `.into()` / `cast()` for the parents).
7. **Static CRT.** Without `+crt-static` the DLL would import `vcruntime140.dll`; Frame Server runs as LOCAL SERVICE and
   must not depend on a redistributable. Set in `.cargo/config.toml` (so `cargo build` from the repo root applies it).
8. **`panic = "abort"` would defeat `catch_unwind`.** Pinned to `unwind` in the workspace release profile.
9. **Clippy `not_unsafe_ptr_arg_deref`** fires on every COM trait method that takes a raw pointer; allowed once in `lib.rs`
   (signatures come from windows-rs).
10. **Test-harness bug found by the first run:** after `Stop` the source queue holds one event, not two. No product impact.

## Deviations from the plan

- `vcam-protocol` holds the test-pattern geometry (`pattern` module), not only constants: the DLL draws the box, the tool
  finds it.
- `register` also writes `HKLM\SOFTWARE\BetterCam\Vcam` (plan step 3); `unregister` leaves `%ProgramData%\BetterCam` (logs)
  in place; the plan deletes it. Kept for diagnosis during the spike.
- `Activator::ShutdownObject` shuts the cached source down and `DetachObject` drops it; Microsoft's doc says both return
  `E_NOTIMPL` for a source that implements `IMFActivate` itself, VCamSample returns `S_OK`. Ours is a separate activator
  object; no problem seen.
- Process model (see "The DLL also loads inside the consumer").

## Machine state at the end

Camera registered and running. `C:\Program Files\Better Cam\vcam\better_cam_vcam_1.d30ad8c1.dll`;
`HKLM\SOFTWARE\Classes\CLSID\{3C0EE391-6A3A-4D0E-A4E5-8243EA287A41}\InprocServer32` (+ `ThreadingModel=Both`);
`HKLM\SOFTWARE\BetterCam\Vcam` (`DllPath`, `Protocol=1`, `Build=d30ad8c1`); `%ProgramData%\BetterCam\logs` with a protected
DACL (SYSTEM, Administrators full; LOCAL SERVICE and the user modify). Three UAC prompts were used (register,
unregister, register).

## Manual tests for you

Camera device is "Better Cam (Windows Virtual Camera)". Expect scrolling colour bars and a white square that steps right
16 px per frame and wraps.

1. **Camera app**: open it, switch camera (the icon with two arrows) to Better Cam. Watch 2 minutes, no stalls. Try
   Video mode too.
2. **Edge**: `https://googlechrome.github.io/samples/image-capture/grab-frame-take-photo.html`, allow the camera, pick
   Better Cam, "Grab frame" / "Take photo".
3. **OBS** (not installed here): Sources > Video Capture Device > Device = Better Cam. If it is missing from the list,
   run `target\release\vcam-tool.exe register --dshow-class` (one more UAC prompt) and re-test; report the result.
4. **Reboot**, then open the Camera app: Better Cam must still be listed. (`vcam-tool status` should print `listed: true`.)
5. Optional: open the Camera app and OBS at the same time (shared stream), and look in `%ProgramData%\BetterCam\logs`
   for `PANIC` lines: `findstr /s PANIC C:\ProgramData\BetterCam\logs\vcam-*.log`.

To remove: `target\release\vcam-tool.exe unregister`.
