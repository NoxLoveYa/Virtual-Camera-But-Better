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


---

# P2: frame transport

Spike P2 of `docs/PLAN.md`: frames written by another process reach camera consumers through
`%ProgramData%\BetterCam\frames.bin`; the DLL no longer draws anything of its own, it copies frames from that file or
shows a slate. Machine as in P1. DLL under test: `better_cam_vcam_1.ae99667d.dll` (the second P2 build, see trap 1).

**Verdict: the transport works end to end through the real Frame Server.** 1080p60 NV12, 1080p60 RGB32, 720p30,
360p30 and a 60-to-30 fps decimation all pass `capture-test` with no torn, black or irregular frames; the three slates
match their embedded images exactly; hostile files give the slate. One of the brief's measurements does not come out
the way the brief wants (disk writes, see check 6) and one is a hair over its bound by design (killed writer, check 4).

## What was built

| Path | Content |
|---|---|
| `crates/vcam-protocol/src/header.rs` | header layout (offsets + a `#[repr(C)]` mirror the compiler checks with `offset_of!`), `Format`, `WriterState`, `RawHeader`, advertised-list rules (`advertised_for_output`, `default_advertised`) |
| `.../seqlock.rs` | `Shared`: atomic header access, `publish` (writer seqlock), `read_latest` (reader seqlock + callback), DLL-side setters; a view made read-only never stores |
| `.../validate.rs` | `validate_header`, `validate_slot`, `validate_format`, `is_stale`: every field checked before use |
| `.../map.rs` | Windows: `MappedFile` (open/map, refuses files under 4096 B or over 256 MB before mapping), QPC helpers, `read_advertised` (one plain 4096-byte read), `Writer` |
| `.../pattern.rs`, `.../slate.rs` | test pattern (checkerboard + stripe, fill + analyse), slate RLE codec, nearest-neighbour scale, NV12 to BGRX |
| `crates/vcam-source/src/frames.rs` | `FrameSource`: the poll state machine, slates, repeat rule, consumer counters; `advertised()`; test-only path override |
| `crates/vcam-source/src/stream.rs` | pump now driven by `FrameSource`; 1 ms high-resolution waitable timer for the fine polls; `make_sample` copies a tight frame into a pitched MF buffer |
| `crates/vcam-source/assets/slates/*.rle` | `not_running`, `paused`, `starting` (32-35 KB each) |
| `crates/vcam-tool` | new `feed`, `stats`, `gen-slates`; `capture-test --expect feed\|slate\|slate-paused\|slate-starting\|feed-to-slate [--fps N]`; `register` verifies the data-folder DACL and re-applies it to an existing `frames.bin` |

The P1 pattern (bars + box) is gone from the DLL, from `vcam-protocol` and from the tool.

Header layout actually used (bytes): magic 0, protocol 4, header_size 8, slot_count 12, file_size 16, slot_capacity 24,
writer_pid 32, writer_state 36, writer_heartbeat 40, qpc_frequency 48, writer_frames 56, advertised_count 64,
advertised[8] 72 (20 B each), consumer_active 232, reader_slot 236, consumer_heartbeat 240, negotiated 248,
latency_p50_us 268, latency_p95_us 272, delivered 280, repeated 288, latest_slot 296, slots 304 (48 B each:
seq, width, height, stride, subtype, capture_qpc, data_offset, data_size). File = 4096 + 3 x 12 451 840 = 37 359 616 B.

## Check results

### 1. Tests

```
cargo test -p vcam-protocol -p vcam-source -p vcam-tool
  vcam-protocol  39 passed     vcam-source  27 passed     vcam-tool  14 passed     (0 failed)
```

Including: writer thread at 1 kHz + reader thread for 2 s with no torn frame (`seqlock::tests`, plus an unthrottled
writer); validation rejects `file_size` past the mapping, slot offsets past the file end (also `u64::MAX - 4`, and inside
the header), odd sizes, zero sizes, sizes over 4096x2304, RGB32 over 1080p, bad strides; in-process DLL tests through
`DllGetClassObject` with a real writer thread: live frames at every advertised type (NV12 and RGB32), 30 fps pacing, the
three slates, a writer that closes, a writer that dies (slate after 0.8-1.6 s, last frame repeated meanwhile), a corrupted
header (magic, protocol, header size, slot count, file size, slot capacity, qpc frequency, writer state, heartbeat in the
future: each gives the slate and recovers when restored), damaged slot descriptors, activation stays cheap (nothing
mapped, `consumer_active` stays 0), 60 fps writer into a 30 fps stream is halved exactly. The DLL suite was run 7 more times
in a row (4 before and 3 after the last code change): no flake.
`cargo clippy -p vcam-protocol -p vcam-source -p vcam-tool --all-targets` and `cargo fmt -p ... --check`: clean
(fmt scoped to these three packages; `src-tauri` belongs to another agent).

### 2. Build and register

```
cargo build -p vcam-source -p vcam-tool --release  -> Finished (exit 0); DLL 698 880 bytes
vcam-tool register  (second P2 build)
  dll copied: C:\Program Files\Better Cam\vcam\better_cam_vcam_1.ae99667d.dll
  frames.bin: DACL applied (C:\ProgramData\BetterCam\frames.bin)
  data dir DACL: D:PAI(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;0x1301bf;;;LS)(A;OICI;0x1301bf;;;S-1-5-21-...-1001)
  registered / started
  locked, delete scheduled at reboot: ...\better_cam_vcam_1.04629341.dll
```

Frame Server log (first line): `process 'C:\WINDOWS\System32\svchost.exe' pid 33460 session 0 account 'LOCAL SERVICE' dll '...better_cam_vcam_1.ae99667d.dll'`.
LOCAL SERVICE opens `frames.bin` read-write (the DLL writes `negotiated` and the feeder reads it back). No `PANIC` line
in any Frame Server log. The log of the consumer-side probe (`vcam-tool.exe`, session 1, the user) has no `pump` and no
`frames:` line: activation and `CreatePresentationDescriptor` map nothing and start nothing.

### 3. `capture-test --expect feed`

```
feed --size 1920x1080 --fps 60 --seconds 300   (background)
capture-test --expect feed --frames 600
negotiated: nv12 1920x1080 @60
frames read: 600 in 10.00s
wall fps: 60.00   timestamp fps: 60.00   max gap: 18 ms
stripe: 600 frames decoded, typical step 1 slot(s)/frame, 0 repeated, 0 backwards, 0 irregular
torn frames: 0   black frames: 0   frames without stripe: 0   bad checkerboard: 0   other: 0
PASS (Nv12 1920x1080 Feed)

feed --size 1280x720 --fps 30      capture-test --expect feed --size 1280x720 --frames 600
negotiated: nv12 1280x720 @30 / frames read: 600 in 19.98s / wall fps: 30.00 / max gap: 34 ms
stripe: 600 frames decoded, typical step 1, 0 repeated, 0 backwards, 0 irregular / torn 0, black 0
PASS (Nv12 1280x720 Feed)
```

More, all with the final DLL (feeder at 1080p60 unless noted; the feeder follows the format the DLL negotiated):

| consumer type | frames | result |
|---|---|---|
| NV12 1920x1080 @60, 2400 frames (run alongside the disk measurements; every result that was looked at passed) | 2400 | PASS, 60.03 fps, 1 repeated, 1 irregular, max gap 33 ms (the run whose output was read in full) |
| RGB32 1920x1080 @60 | 300 | PASS, 60.00 fps, 0 irregular |
| NV12 640x360 @30 (writer at 60) | 150 | PASS, 30.01 fps, constant step 2 |
| NV12 1920x1080 @30 (writer at 60) | 300 | PASS, 30.00 fps, constant step 2, 0 irregular |

`--expect feed` skips frames until the first feed frame (the writer needs a frame or two to follow a new negotiated size;
at most 2 s) and then requires every frame to be a feed frame; irregular steps are tolerated up to 2 %.

### 4. Slates

```
feeder stopped cleanly while a capture runs (feed --seconds 6; capture-test --expect feed-to-slate --frames 360)
feed frames before the slate: 120; first slate frame: #120
writer state at the end: closed; slate appeared 35 ms after the writer's last heartbeat
PASS (Nv12 1280x720 FeedToSlate)                                    (earlier DLL build: 31 ms)

feeder killed (Stop-Process -Force, no clean close)
feed frames before the slate: 151; first slate frame: #151
writer state at the end: live; slate appeared 1006 ms after the writer's last heartbeat
  - the slate took 1006 ms after the last heartbeat (limit 1000 ms)
FAIL (Nv12 1280x720 FeedToSlate)                                    (earlier DLL build: 1034 ms)
```

A writer that closes the file (`Drop`, Ctrl+C, end of `--seconds`) gives the slate within one frame. A writer that
vanishes without closing is only noticed when its heartbeat is older than 1 s, so the slate comes 1.0-1.03 s after the
last heartbeat: the decided 1 s timeout and a "within 1 s" bound cannot both hold for a killed process. The check keeps
its 1000 ms limit and fails by 6-34 ms; lowering `HEARTBEAT_TIMEOUT_MS` (one constant in `vcam-protocol/src/header.rs`,
say 750 ms) would pass it. Not changed: the 1 s is a decision already made.

```
capture-test --expect slate --size 1280x720 --frames 90      (no writer)
slate 'Better Cam is not running': 90 of 90 frames match (worst mean luma difference 0.00, tolerance 2); 0 feed frames
PASS (Nv12 1280x720 Slate(NotRunning))
feed --state paused    -> slate 'Camera paused': 60 of 60 frames match ... PASS (Nv12 1280x720 Slate(Paused))
feed --state starting  -> slate 'Starting camera...': 60 of 60 frames match ... PASS (Nv12 1280x720 Slate(Starting))
capture-test --expect slate --format rgb32 --size 1280x720   -> 60 of 60 frames match ... PASS
```

Mean luma difference 0.00: the DLL and the tool scale the same decoded 1080p slate with the same function.

### 5. `stats` during feed (consumer attached, final DLL)

```
feed 1920x1080 @60 + capture-test (2400 frames) running, vcam-tool stats:
writer_state: live            consumer_active: 1            negotiated: nv12 1920x1080 @60/1
latency_p50_us: 1110          latency_p95_us: 1984          delivered: 2064      repeated: 1      repeated_pct: 0.048
feed 1280x720 @30:            latency_p50_us: 985           latency_p95_us: 1815  delivered: 601       repeated: 0
60 fps writer, 30 fps type:   latency_p50_us: 1190          latency_p95_us: 1964  delivered: 301       repeated: 0
```

`latency_p95_us` <= 20000 (about 2 ms) and `repeated` < 1 % of `delivered` in every run. Latency is measured from the
frame's `capture_qpc` to the moment the sample is queued, so with the feeder (which stamps the frame's ideal due time)
it includes the feeder's own wake-up jitter.

### 6. Disk writes during feed

Counter: `Get-Counter '\PhysicalDisk(_Total)\Disk Write Bytes/sec' -SampleInterval 2 -MaxSamples 15`, feed 1920x1080 @60
with `capture-test --frames 2400` attached for the whole window (consumer attached, `consumer_active: 1` checked first),
the idle baseline measured the same way in the window before (nothing of Better Cam running). Window averages:

| pair | idle baseline | feed + consumer | difference | notes |
|---|---|---|---|---|
| 1 (first DLL build, quiet machine) | 0.32 | 0.33 | +0.01 | |
| 2 | 6.84 | 8.43 | +1.59 | baseline itself had a 61.7 MB/s second |
| 3 | 1.17 | 15.34 | **+14.2** | every sample 5.9-30.8 MB/s; page counters not recorded in this pair |
| 3 | 2.45 | 0.24 | -2.2 | identical setup to the line above |
| 4 | 0.45 | 15.56 | **+15.1** | one 211.5 MB/s second: modified page list 43 MB to 3 511 MB, Page Writes/sec 211; the other 14 samples 0.1-11.6 |
| 4 | 1.48 | 8.34 | **+6.9** | Page Writes/sec 0 in all 15 samples; modified list was already 179 MB at the start and drained to 52 MB |
| 5 | 16.63 | 0.25 | -16.4 | **baseline with no feeder and no consumer**: 241 MB/s second, modified list 31 MB to 3 684 MB, Page Writes/sec 232 |
| 5 | 0.51 | 0.65 | +0.14 | |

**Result as the brief states it: not met.** 5 of 8 pairs are under +5 MB/s, 3 exceed it (+14.2, +15.1, +6.9). Per the
brief nothing was built as a fallback. What the numbers say about the cause, honestly:

* The idle baseline on this machine is not idle: windows with nothing of Better Cam running range from 0.32 to
  16.63 MB/s. Pair 5's baseline contains the same multi-GB burst (modified list +3.6 GB in one second, 241 MB/s of page
  writes) as pair 4's feed window, with no feeder and no consumer running. Those bursts come from other software (Firefox,
  WebView2 hosts and the OpenChamber host are the top ordinary writers, about 1 MB/s each).
* The transport cannot dirty much: the writer only ever touches the first 3.1 MB of each slot it uses (two slots at
  1080p NV12, about 6.2 MB plus the header) out of the 37 MB file; write-back of that region at the frame rate (the
  worst case) would be 190 MB/s and shows up as `Page Writes/sec` > 0 for the whole window. Page Writes/sec was 0 in
  every feed window in which it was recorded (pairs 4 and 5) except the single burst second of pair 4; pairs 1-3 only
  have the disk counter.
* Memory does not leak: during 44 s of 1080p60 the Frame Server host stayed at 6-7 MB private, `capture-test` at 3 MB, the
  feeder at 2 MB, system commit flat (26.6 to 25.6 GB). A multi-GB burst that equals 18 s of 1080p60 frames is a
  coincidence of arithmetic, not a leak.
* **Not explained:** pair 3's feed window (+14.2 MB/s, sustained over all 15 samples, no page counters recorded). It
  is not repeated in four later feed windows with the same setup. If the write-back question matters for the decision,
  the definitive measurement is an ETW file trace (`Microsoft-Windows-Kernel-File`, filtered to `frames.bin`) over a long
  feed on a quiet machine: it needs an elevated shell, which was not used for measuring here.

The file is created with `FILE_ATTRIBUTE_TEMPORARY` (`Writer::create` also sets it on an existing file; a unit test asserts
the attribute, `Get-Item` shows `Archive, Temporary`).

### 7. clippy, fmt

Clean (see check 1).

### 8. Machine state

Camera registered with the new DLL, see "Machine state at the end (P2)".

## Findings

**Pacing and decimation.** The pump gives frames out as they appear: a new frame is taken at once if it was captured at
least 3/4 of a frame period after the last emitted one (`FrameSource::min_capture_gap`). A writer at twice the consumer's
rate is halved exactly (constant stripe step of 2), a matching writer is passed through (jitter of a quarter period
never drops a frame), a slower writer gets the previous frame repeated after 1.5 periods without a new one. Sample time
is the frame's `capture_qpc` converted to 100 ns (checked: `MFGetSystemTime` and QPC agree within 20 ms), made strictly
increasing; slates and repeats are stamped with the time of the decision. A frame of another size or subtype than the
negotiated one is never shown or scaled: the last frame repeats, or "Starting camera..." if there is none.

**Slates.** NV12 1080p RLE (planar Y, U, V runs): 32-35 KB each, decoded once per process (about 3 MB each), scaled by
nearest neighbour, RGB32 converted once per stream. They are drawn with GDI by `vcam-tool gen-slates` (Segoe UI, 72 px);
`--preview DIR` writes PNGs. Rerun the command and rebuild only when a slate's look changes.

**Advertised types persist.** The header's advertised list is read at every activation and ignored for staleness, so the
camera keeps the types of the last session after the app exits (stable device list). Defaults (1080p and 720p at 30, NV12
and RGB32) apply only when the file is missing or invalid. Consequence for tests: after a 720p feeder, a plain
`capture-test` (default `--size 1920x1080`) fails with "device does not offer that format/size"; pass `--size`.

**Hostile or broken file.** Mapping is refused for files under 4096 B or over 256 MB; `file_size` must not exceed the
mapped length; `slot_capacity` is bounded; QPC frequency must equal the local one; a heartbeat more than 1 s in the
future is invalid; slot data must lie inside the file. A slot whose descriptor is stable but invalid gives the slate. A
panic inside the poll is contained: the file is dropped and the slate shown. Windows refuses to truncate a file that is
mapped, so a view can never end up past the end of the file.

**Writer must not truncate.** `Writer::create` opens `OPEN_ALWAYS` and only grows the file. A DLL mapping makes shrinking
fail, and a deleted-and-recreated file is picked up by the DLL by re-opening every 0.5 s while the status is "not
running".

**`consumer_active`.** Set to 1 on every emission, cleared when the last stream of the process that uses the file stops
(a per-path counter in the DLL). The writer treats it as "attached" only while `consumer_heartbeat` is under 1 s old.

## Traps hit

1. **Decimating on the previous emission time drifts.** First version: take a frame when `now >= last_emit + 0.9 period`.
   With a 60 fps writer and a 30 fps consumer a new frame is always waiting, so emissions ran every 30 ms: 32.6 fps, steps
   of 1-3 slots (`48 of 300 frames irregular`) and p95 latency 16.7 ms. Found by the real-Frame-Server run, not by the
   in-process tests (they only ran matching rates). Fixed by gating on capture timestamps (above); regression test
   `a_faster_writer_is_decimated_on_capture_time_and_a_matching_one_is_not`; the DLL was registered twice because of it.
2. **A killed writer is noticed after 1.0-1.03 s**, see check 4.
3. **The old DLL is locked on the second `register`.** First P2 register deleted the P1 DLL at once (Frame Server did not
   hold it at that moment); the second found the previous build mapped and scheduled it for deletion at reboot. Harmless: the registry
   points at the new file; both files are 698 880 bytes. `unregister` stops Frame Server and deletes both.
4. **Disk-counter noise** (check 6): idle windows between 0.32 and 16.63 MB/s on this machine.
5. **A test that pokes the heartbeat races the live feeder**, which rewrites it with every frame; the corrupted-header test
   now stops the feeder before the future-heartbeat case.
6. **`FILE_ATTRIBUTE_TEMPORARY` is only applied at creation.** `CreateFileW(OPEN_ALWAYS)` on an existing file keeps its
   attributes; `Writer::create` sets the flag afterwards with `SetFileAttributesW`.
7. **windows 0.62:** `BOOL` moved to `windows_core`; GDI drawing calls return `BOOL` and are `#[must_use]`;
   `AtomicU32::from_ptr` (stable) is what makes atomic access over a mapping work; `CreateWaitableTimerExW` needs
   `Win32_Security`; `CREATE_WAITABLE_TIMER_HIGH_RESOLUTION` and `TIMER_ALL_ACCESS` live in `Win32_System_Threading`.
8. **PowerShell turns a native command's stderr into an error record** (`cargo ... 2>&1` ends with "Exited with code 1"
   although cargo succeeded); run cargo through `cmd /c` when capturing its output.
9. LNK4104 on the two exports (as in P1), harmless.

## Deviations from the plan

* `vcam-protocol` also holds `pattern.rs` (test pattern fill and analysis, used by the tool and by the DLL tests) and
  `slate.rs` (RLE codec and scaling, used by the tool to compare and by the DLL to show), plus `map.rs` (Windows file
  mapping and `Writer`): the brief's file list named four files.
* Header additions to plan 2.5: `slot_count`, `slot_capacity`, `reader_slot` (the plan's "slot the reader marked in use"),
  `writer_frames`; `subtype` values are the FourCC-like `'NV12'` and `'BGRX'`.
* The DLL reads `negotiated` back as the format the writer must deliver, and decimates a faster writer on capture time
  (3/4 period) instead of the plan's "repeat when late" alone.
* NV12 frames of 4096x2304 pass `validate_format` (limit from the brief) but do not fit a slot (12.45 MB slot, 14.2 MB
  frame): `publish` returns `TooLarge` and the DLL would reject the descriptor. The largest NV12 frame that fits is
  3840x2160. `advertised_for_output` does not yet drop sizes that do not fit.
* The heartbeat timeout is the plan's 1 s; a killed writer therefore misses a "within 1 s" bound by up to about 35 ms.

## Machine state at the end (P2)

Camera registered and running with `C:\Program Files\Better Cam\vcam\better_cam_vcam_1.ae99667d.dll`
(`HKLM\SOFTWARE\Classes\CLSID\{3C0EE391-...}\InprocServer32` and `HKLM\SOFTWARE\BetterCam\Vcam` `DllPath`, `Protocol=1`,
`Build=ae99667d`); `better_cam_vcam_1.04629341.dll` is still in the folder (loaded by an old Frame Server instance,
delete scheduled at reboot). `C:\ProgramData\BetterCam\frames.bin` (37 359 616 bytes, `Temporary`, DACL: SYSTEM and
Administrators full, LOCAL SERVICE and the user modify) with the last writer's header (state left by the last killed
test feeder; its heartbeat is stale, so the camera shows "Better Cam is not running"). Two UAC prompts were used in
P2 (both `register`, both answered within seconds). No test process is left running.

## Commands to re-run

```
cargo build -p vcam-source -p vcam-tool --release
target\release\vcam-tool.exe register                       (UAC prompt; needed after every DLL change)
target\release\vcam-tool.exe feed --size 1920x1080 --fps 60 --seconds 300     (background)
target\release\vcam-tool.exe capture-test --expect feed --frames 600
target\release\vcam-tool.exe stats
target\release\vcam-tool.exe capture-test --expect feed-to-slate --frames 360   (then stop the feeder)
target\release\vcam-tool.exe capture-test --expect slate --size 1280x720        (no writer running)
target\release\vcam-tool.exe gen-slates --preview %TEMP%\slates                 (only when a slate's look changes)
```
# P-hardening: robustness of the DLL and the frame-file protocol (review fixes)

A code review of the P2 DLL and `vcam-protocol` found faults that could crash the camera service or alter files it
should not touch. Every finding below was fixed with a regression test (one finding was wrong, see 5). No layout
change: `PROTOCOL_VERSION` is still 1 and the file is byte-compatible with the app. Build registered at the end:
`C:\Program Files\Better Cam\vcam\better_cam_vcam_1.4690e664.dll` (`Build=4690e664`).

## What changed

| # | Finding | Fix | Regression test |
|---|---|---|---|
| 1 | The mapping could be shorter than `len`; a reader then ran past the view (reproduced: `0xC0000005` in the DLL path) | `MappedFile::map` asks `MapViewOfFile` for exactly the checked size (fails when the section is smaller) and confirms it with `VirtualQuery`; the guarantee that protects a reader after mapping is written on `MappedFile` | `map::tests::a_file_that_shrinks_between_the_size_check_and_the_mapping_is_refused` (deterministic, hook between size query and mapping), `a_reader_survives_a_file_whose_size_keeps_switching` (the reviewer's race, 3 s), `a_mapped_file_cannot_be_truncated_under_a_reader` |
| 2 | The DLL wrote consumer fields into files whose header was not valid | `FrameSource.header_ok` is the result of the most recent `validate_header`; `announce`, `finish_emit`, `stop` and the statistics write only while it is true | `frames::tests::foreign_files_and_other_protocol_versions_are_never_written_to` (file byte-identical afterwards), `a_valid_file_still_gets_consumer_state_after_it_became_valid` |
| 3 | The shared file and the log were opened without checking what they are | new `vcam_protocol::secure`: `FILE_FLAG_OPEN_REPARSE_POINT`, refuse reparse points, directories and files with more than one hard link, and `GetFinalPathNameByHandleW` must equal `<ProgramData>\BetterCam\frames.bin` on a local fixed disk (`MappedFile::open_pinned`, `read_advertised_pinned`); `%ProgramData%` comes from `SHGetKnownFolderPath`; the DLL creates no folders; the log verifies the `logs` folder and file the same way and rotates by truncating its verified handle | `secure::tests::*` (junction, hard link, directory, symbolic link, `..`), `frames::tests::a_frame_file_behind_a_junction_or_hard_link_is_not_used_and_not_touched`, `log::tests::missing_folders_are_never_created`, `a_logs_folder_that_is_a_junction_gets_nothing`, `a_data_folder_that_is_a_junction_gets_nothing`, `a_hard_linked_log_file_is_refused_and_left_alone`, `a_symbolic_link_as_log_file_is_refused_when_one_can_be_made` |
| 4 | One log file per process, never deleted | when a log starts it keeps the newest 8 `vcam-*.log` plus those of running processes (only plain files are deleted); size cap kept (1 MB, truncated in place) | `log::tests::the_number_of_logs_stays_bounded_over_many_process_starts`, `pruning_keeps_the_newest_and_the_running_and_touches_only_plain_logs`, `a_full_log_is_truncated_through_its_handle_and_never_renamed` |
| 5 | `PROPVARIANT` leak in `dump_attributes` | **finding is wrong, code unchanged** (see below) | `activator::tests::dumping_the_attributes_leaks_no_reference` |
| 6 | `capture_qpc` from the file used unchecked | clamped to `[now - 5 s, now + 50 ms]` (`frames::clamp_capture`); `saturating_add` for the pacing arithmetic; `next_timestamp` saturates; `qpc_to_hns` / `hns_to_qpc` saturate | `frames::tests::hostile_capture_times_are_clamped_and_never_break_the_stream`, `capture_times_are_clamped_for_every_input`, `stream::tests::sample_times_never_overflow_and_always_grow`, `map::tests::time_conversions_saturate_instead_of_wrapping` |
| 7 | A torn copy stayed in the "last frame" buffer | the read callback copies into `scratch`, swapped into `frame` only on a confirmed read | `frames::tests::a_torn_copy_never_reaches_the_last_frame_buffer` |
| 8a | A panic in the pump left the stream "running" | the thread body runs in `catch_unwind`; a panic queues `MEError`; `timeBeginPeriod` is a guard (`TimerPeriod`) that ends on every path | `stream::tests::a_panic_in_the_pump_becomes_an_error_event_for_the_consumer`, `the_timer_resolution_is_given_back_when_the_pump_panics` |
| 8b | `Pump::stop` joined under the stream and source locks | stream `start`/`stop`/`shutdown` take the old pump out and stop it outside the `state` lock (a separate `control` lock serialises them); `Source::Start`/`Stop` no longer hold the source lock across it; the wait is bounded (2 s), after that the thread is left to end by itself | `stream::tests::the_stream_state_lock_is_free_while_an_old_pump_is_being_stopped`, `stopping_a_pump_stuck_in_a_call_does_not_wait_for_ever` |
| 8c | `num + den / 2` overflowed `u32` | `round_fps` in 64 bits (`Format::fps`, `FrameFormat::from_media_type` through `fps_of_rate`) | `header::tests::rounded_frame_rates_do_not_overflow`, `validate::tests::extreme_frame_rates_neither_overflow_nor_pass`, `mediatype::tests::frame_rates_of_every_edge_do_not_overflow` |
| 8d | 4096x2304 NV12 was valid but did not fit a slot | `validate_format` also requires the tight NV12 frame to fit `SLOT_CAPACITY`; `advertised_for_output` drops entries that are not valid formats | `validate::tests::accepted_formats_always_fit_a_slot_and_nothing_that_fits_is_refused`, `seqlock::tests::publish_rejects_bad_formats` |

The tests of findings 1, 2, 3, 4, 6, 7, 8a (the error event), 8b (the state lock), 8c (header) and 8d were run with the
old behaviour re-introduced by hand, one at a time: each fails then (finding 1: the stress test dies with
`STATUS_ACCESS_VIOLATION` on the unfixed code, the hook test fails). The tests of the new helper functions (`clamp_capture`,
`next_timestamp`, `fps_of_rate`, `TimerPeriod`) have no old version to run. The truncation test passes on the old code too: it
documents the guarantee the old code already relied on.

### The guarantee behind finding 1

Windows refuses to cut a file below the size of a mapped section (`ERROR_USER_MAPPED_FILE`, 1224), in read-only and
read-write mappings, so after `CreateFileMappingW` a view cannot shrink under a reader (test above). The old code's
fault was the window *before* the section exists: the size was read first, the section took the size at a later moment.
`MapViewOfFile(.., len)` now fails when the section is smaller. What no check can catch is a read error of the medium
under a mapped view (an in-page exception that `catch_unwind` cannot stop); pinning the file to a local fixed disk is
what keeps a removable or network medium out.

### Finding 5 is wrong

`PROPVARIANT` has a `Drop` in `windows` 0.62.2 when the `Win32_System_Variant` feature is on, which this crate enables
(`windows-0.62.2/src/extensions/Win32/System/StructuredStorage.rs`: `impl Drop for PROPVARIANT` calls
`PropVariantClear`). The test holds a COM reference in an attribute and dumps 50 times: the count does not move. As a
control, the same test fails (52 references instead of 2) when the value is wrapped in `ManuallyDrop`, so it would catch
a real leak. No change to `activator.rs`.

## Check results

```
cargo test -p vcam-protocol -p vcam-source -p vcam-tool    54 + 51 + 12 passed, 0 failed
cargo clippy -p vcam-protocol -p vcam-source -p vcam-tool --all-targets   0 warnings (the LNK4104 linker note was there before)
cargo fmt -p vcam-protocol -p vcam-source -p vcam-tool --check            clean
cargo check -p better-cam                                  passes
```

Live, with the fixed DLL registered (one UAC prompt, answered in seconds; Frame Server was stopped once by `register`):

```
vcam-tool status                                      listed: true, directshow listed: true
vcam-tool feed --size 1920x1080 --fps 60 --seconds 40 (background)
vcam-tool capture-test --expect feed --frames 600     600 frames in 9.98 s, 60.03 fps, max gap 18 ms, 0 torn / black / repeated   PASS
stats, consumer attached                              consumer_active 1, latency_p50 1086 us, latency_p95 1970 us, delivered 232, repeated 0 (0.000 %)
stats right after the 600 frames                      latency_p95 2092 us, repeated 0.000 %
feeder stopped: capture-test --expect slate           120 of 120 frames match the slate, PASS
```

A foreign `frames.bin` (8 KB of 0x5A in place of the file) gave the slate (`capture-test --expect slate --frames 90`
PASS) and was byte-identical afterwards (same SHA-256). `frames.bin` as a second hard link of a live file, with a
feeder writing into it, gave the slate too (PASS); the unit test of the same case fails with the old open call. Both were done by
renaming the real file away and back: afterwards the folder DACL, the file DACL, the creation time and the size were
the same and the file has one link. Not done live: a junction in place of `BetterCam` or `logs` (the service holds
open files in `logs`, and replacing `BetterCam` would touch the folder and its permissions); covered by the unit
tests above.

Logs: 31 `vcam-*.log` files before the fixed DLL was registered; the first start of the fixed DLL pruned them to 9
(newest 8 plus the current). Ten `capture-test --frames 30` runs (all go through one Frame Server process) and fourteen
`status` runs: still 9 (`before 9, after 9`). The ten runs print `FAIL (... Feed)` only because `capture-test` expects a
feed by default and no feeder was running; they were run for their consumer side effect, not for the pattern.

## Open items for the installer

* The DLL no longer creates `%ProgramData%\BetterCam` or `logs`; the installer must (it already does, register prints
  `data dir ready`), with a DACL that lets LOCAL SERVICE and the user write.
* `vcam-install` still resolves the data folder from the `ProgramData` environment variable (`util.rs` `data_dir`);
  use `vcam_protocol::secure::program_data_dir()`.
* The elevated `register` applies a DACL to `frames.bin` with `SetNamedSecurityInfoW`, which follows links: it must
  refuse a `frames.bin` (and `logs`) that is a link, a junction or has more than one hard link before it touches it
  (`vcam_protocol::secure::open_existing` and `verify_dir` do the checks). The same for `remove_data_dir`.
* The folder is writable by the installing user. To make link planting impossible rather than refused, the installer
  could give the user only modify rights on `frames.bin` (not create or delete rights in the folder) and make `logs`
  writable by LOCAL SERVICE only; that needs the installer to create `frames.bin` first, and the app's `Writer::create`
  (which still creates the folder and opens with `OPEN_ALWAYS`, following links) would then only open it.

# P-hardening 2: the installer and the folder permissions (review fixes)

A second review found that the elevated helper trusted a folder the installing user could rearrange, plus several
smaller faults of install and uninstall. All fixed, each with a test. No change to the frame-file layout,
`PROTOCOL_VERSION`, the helper's command line or its exit codes 0, 1, 2, 5; no change to `crates/vcam-source`.
New exit code **10**: done, but the old build is still loaded by the camera service (install) or a file waits for a
restart (remove); the app shows a message that says so (`"pending_restart"` from the commands).

## The permission design

Applied by the elevated helper, always with a protected DACL (nothing inherited from `%ProgramData%`) and the owner
set to Administrators, through a handle that was opened without following links and verified (not a reparse point,
the right kind, one hard link, final path exactly `<ProgramData>\BetterCam[\...]`). Read back with `GetAce` and
`EqualSid`. The masks are in `crates/vcam-install/src/acl.rs`.

| object | ACE | rights | why |
|---|---|---|---|
| `BetterCam` | SYSTEM, Administrators | full control, this folder only | the installer works here |
| | LOCAL SERVICE, installing user | read and traverse (`0x1200a9`), this folder only | reach `frames.bin` and `logs`, list them; **no** create, delete, rename, change of permissions |
| `logs` | SYSTEM, Administrators | full control, inherited (OI CI) | the helper's `install.log`, clean-up |
| | LOCAL SERVICE | read, traverse and create files (`0x1200ab`), this folder only | the service writes `vcam-<pid>.log` |
| | LOCAL SERVICE | modify (`0x1301bf`), files only (OI IO) | read, write and prune (delete) the logs it made |
| | installing user | read and traverse, this folder only; read (`0x120089`) on files (OI IO) | read the logs and `install.log`; **cannot** create or delete in `logs` (a consumer process that runs as the user logs nothing, which the DLL tolerates) |
| `frames.bin` | SYSTEM, Administrators | full control | |
| | LOCAL SERVICE, installing user | read and write data and attributes (`0x12019f`) | the DLL maps it read-write (consumer fields), the app maps it read-write; **no** delete, rename or `WRITE_DAC` |

Owner of all three (and of `install.log`): Administrators. The data folder has no inheritable ACE for the user, so
nothing the user could leave in it inherits anything. The installer pre-creates `frames.bin` (full size,
`FILE_ATTRIBUTE_TEMPORARY`); `Writer::create_default` (the app) only opens it and returns a clear `NotFound` when
it is missing; `Writer::create(path)` still creates a file for any other path (tests, scratch folders).

## What changed

| # | Finding | Fix | Tests |
|---|---|---|---|
| 1 | Elevated code followed links in a folder the user could rearrange | the permissions above; `datadir.rs` opens every object with `FILE_FLAG_OPEN_REPARSE_POINT`, verifies the handle, sets and reads permissions with `SetSecurityInfo` / `GetSecurityInfo` on it; the folder is held without delete sharing meanwhile; `install.log` is written only through a verified handle (`FileLog`); deleting never recurses through a link (`remove_no_follow`); `Writer` opens the pre-created file | `datadir::tests::a_data_folder_that_is_a_junction_is_moved_aside_and_its_target_is_left_alone`, `a_logs_folder_that_is_a_junction_is_replaced_and_its_target_is_left_alone`, `a_hard_linked_frame_file_is_replaced_and_the_other_name_is_left_alone`, `a_frame_file_that_is_a_symbolic_link_is_replaced_when_one_can_be_made`, `strangers_in_the_data_folder_are_removed_without_following_links`, `deleting_never_recurses_through_a_link`, `a_path_based_acl_call_follows_a_junction_which_is_why_nothing_here_uses_one` (the reviewer's reproduction), `acl::tests::the_*_acl_*`, `log::tests::*` (junction, hard link, symlink, tail), `map::tests::a_writer_opens_the_precreated_file_and_never_creates_one`, `a_writer_refuses_links_and_hard_links`, `only_the_production_path_takes_the_strict_route` |
| 2 | Existing folder kept its owner and explicit ACEs | `datadir::decide`: a link, wrong kind, extra hard links, a path that is not where it says, or an owner that is not SYSTEM/Administrators means "move aside and make again"; `logs` with other permissions too; the data folder and `frames.bin` have no children whose ACEs could survive, so their DACL is replaced as a whole and the owner always set; strangers in the data folder are deleted; this is also the upgrade path | `datadir::tests::a_missing_object_is_created_and_a_good_one_is_kept`, `links_wrong_kinds_hard_links_and_foreign_owners_are_replaced`, `other_permissions_replace_logs_but_only_rewrite_the_folder_and_the_file`, `an_owner_who_is_not_trusted_loses_the_object`, `a_logs_folder_with_other_permissions_is_replaced`, `preparing_again_keeps_what_is_there_and_its_contents` |
| 3 | Read-back searched SDDL text for the raw SID | `acl::check` reads the ACEs with `GetAce` and compares SIDs with `EqualSid` | `acl::tests::what_was_applied_reads_back_and_checks_even_for_accounts_sddl_prints_as_aliases` (asserts that the stored SDDL says `LA`/`LG`, i.e. the old search could not find them), `a_dacl_with_anything_else_does_not_check` |
| 4 | Uninstall reported success after a failed removal | a camera that is still listed after a failed `Remove` stops the uninstall before anything else is removed (keys and files stay, Repair or Remove can finish); everything else that cannot be removed is logged, done last and returned as an error (exit 1); a file that waits for a restart is exit 10 | `install::tests::a_failed_remove_stops_the_uninstall_and_keeps_the_keys_and_files`, `registry_keys_that_stay_are_an_error_after_everything_else_was_done`, `a_data_folder_that_cannot_be_deleted_is_an_error`, `a_dll_that_waits_for_a_restart_is_a_pending_result`, `a_half_installed_state_is_cleaned_up_although_remove_says_there_is_no_camera` |
| 5 | A timeout let two helpers run at once | no timeout: `Launcher::run_elevated` returns when the helper has exited (`LaunchOutcome::TimedOut` is gone); the claim on the camera is held across that call and the engine stays released | `vcam_admin::tests::a_second_action_is_refused_for_as_long_as_the_first_helper_has_not_exited`, `a_declined_prompt_frees_the_claim_too` |
| 6a | `valid_sid` | only `S-1-5-21-<3>-<rid>` and Entra ID `S-1-12-1-<4>`, each part canonical `u32`; `confirm_user_account` (`LookupAccountSidW`, `SidTypeUser`); both before any change | `acl::tests::only_user_account_sids_pass`, `a_group_of_this_machine_is_not_a_user_account`, `install::tests::a_bad_sid_stops_the_install_before_anything_is_touched`, `a_sid_that_is_not_a_user_account_is_refused_before_anything_is_touched` |
| 6b | Paths from environment variables | `SHGetKnownFolderPath` for Program Files and ProgramData | `secure::tests::program_data_comes_from_the_shell_and_exists` (existing); the environment is no longer read |
| 6c | Service not stopped: install returned plain Ok | `InstallOutcome::PendingServiceRestart`, exit 10, a message in the app | `install::tests::a_locked_old_dll_and_a_service_that_will_not_stop_is_a_pending_result_not_a_plain_ok`, `a_locked_old_dll_that_goes_once_the_service_stopped_is_done`, `vcam_admin::tests::a_pending_exit_code_is_a_success_with_a_different_word` |
| 6d | `vcam-tool` result file in `%TEMP%` | opened without following links, `CREATE_NEW` when missing, a plain one-link file when present, emptied through the handle | `vcam-tool util::tests::*` |
| 6e | Partial install | order: validate, data folder, DLL (temp name + rename), registration, camera, only then old DLLs; a failure puts the earlier registration back (or removes ours) and deletes the copy it made | `install::tests::a_failed_registration_on_a_clean_machine_leaves_nothing_registered`, `a_failed_camera_on_a_clean_machine_*`, `a_camera_that_will_not_start_*`, `a_failed_update_puts_the_earlier_registration_back_at_every_step`, `a_data_folder_that_cannot_be_prepared_*`, `a_dll_that_cannot_be_copied_*`, `a_rollback_that_fails_says_so` |

Tests that need a symbolic link are skipped without the privilege (this machine: unelevated run); they were run
elevated (`vcam_install`, `vcam_protocol`, `vcam_tool` test binaries as administrator) and pass.

## Things found only by running it

* **A folder cannot be renamed while any file in it is open, even one opened with delete sharing** (PowerShell
  `Rename-Item` with a child opened `ReadWrite, Delete`: access denied). Every process that loaded the DLL keeps its
  `vcam-<pid>.log` open, and so did the helper's own `install.log`: the first live upgrade failed at "move `logs`
  aside". The helper now lets go of its log before it replaces anything (the lines wait and are replayed into the new
  file), stops the camera service once, and as a last resort empties `logs` and resets it in place through the held
  handle (links removed as links). Test `a_logs_folder_that_cannot_be_moved_is_emptied_and_reset_in_place`, `the_install_log_of_the_helper_does_not_keep_logs_from_being_replaced`.
* **`IMFVirtualCamera::Remove` on a camera that is not registered fails** (`0xC00D36B2`). Treating every failed
  `Remove` as "stop" would make Remove useless after a failed install, so only a camera that Windows still lists
  stops the uninstall.
* On this machine `ConsentPromptBehaviorAdmin = 0` (elevate without prompting): `runas` shows no prompt at all, so a
  prompt that stays open could not be produced live; it is covered by the test with a launcher whose prompt stays
  open.

## Notes for packaging

* The helper must be started with `--vcam-install --user-sid <SID of the user who will run the app>`; without a SID
  only LOCAL SERVICE and the administrators can use the frame file. The SID must be a user account (`S-1-5-21-...`
  or `S-1-12-1-...`); an installer that runs as another account must pass the right one.
* Exit code 10 is success with a pending restart (not an error).
* When the helper fails the app shows the last error line of `%ProgramData%\BetterCam\logs\install.log`; it reads the
  file through the same verified open and ignores a file that is older than the run (another run's text).
* `better-cam.exe` (the app) can no longer create `frames.bin`: an app started on a machine without the installed
  camera does not create the folder; "Start" says the virtual camera is not installed.


# P-hardening 3: locks, link roots and the last permissions (second review fixes)

A second review of the DLL, the frame-file helpers and the installer found three medium and nine small faults, plus
a test that could not fail. All fixed, each with a test (the ones that could be run against the old behaviour were,
see "Check results"). No change to the
frame-file layout or `PROTOCOL_VERSION`, no change to the helper's command line except that `--user-sid` is now
required for an install; exit codes 0, 1, 2, 5, 10 as before and one new one, **11** ("another install, repair or
removal is running"). `crates/vcam-source` changed (M1, L3, L4): the machine needs one new registration, done by
Update.

## What changed

| # | Finding | Fix | Tests |
|---|---|---|---|
| M1 | An open waited for as long as a program held an opportunistic lock on the file (a standard user can take one on `frames.bin`, which they may read) | every open goes through `NtCreateFile` with `FILE_COMPLETE_IF_OPLOCKED` (`secure::open_with`, one choke point; also the installer's opens. **Opens only:** the installer's deletes and renames (`remove_dir_all`, `DeleteFileW`, `std::fs::rename`) still opened the file the plain way and waited; that is fixed in "P-hardening 4"); a break in progress is a `refused` open ("show the slate" / "skip"). The log prune deletes through such a handle too (`secure::remove_plain_file`). The `LOG` mutex no longer covers an open or a prune: one thread claims the open (`begin_open`), the others drop their line meanwhile. The user has no access to `vcam-*.log` at all (see the table below), so only the service account could ever lock one | `secure::tests::an_open_does_not_wait_for_a_program_that_holds_an_oplock` (a real `FSCTL_REQUEST_OPLOCK`, no elevation; a plain open on a control thread is shown to block; the checked opens, read and write, pinned or not, and the delete return within 2 s, the old code hangs), `frames::tests::a_frame_file_with_somebody_elses_oplock_gives_the_slate_at_once` (activation read, mapping, pump polls), `log::tests::a_log_file_that_somebody_holds_an_oplock_on_is_not_waited_for`, `pruning_does_not_wait_for_a_locked_log_file`, `only_one_thread_opens_the_log_and_the_others_do_not_wait` |
| M2 | A `%ProgramData%` that is a junction or symbolic link made everything refuse (root opened as a link, files below it resolved to the real place, the comparison failed) | `canonical_dir` opens the root *following* links (an administrator's folder); everything below it is still never followed | `secure::tests::a_root_that_is_a_junction_is_followed_and_links_below_it_are_not`, `datadir::tests::a_program_data_that_is_a_junction_prepares_checks_and_logs` (prepare, kept on the second run, the app's writer, the helper's log and its tail; with the old open it fails with exactly the reviewer's "opened ...moved..., expected ...ProgramData..." message) |
| M3 | A failed preparation of the data folder lost its `ERROR:` line (the log was let go of, the line waited for a file, the process ended); the app then showed an earlier line | `Log::flush`, called on every way out and on drop of `FileLog`; `install_with` makes the log ready again before it returns the error; the app shows a reason only from an `ERROR:` line | `install::tests::the_error_of_a_failed_preparation_reaches_install_log`, `lines_that_wait_for_a_file_are_written_when_the_log_is_dropped`, `vcam_admin::tests::a_log_without_an_error_line_gives_no_reason` |
| L1 | Objects existed with inherited permissions for a moment; an old folder kept the user's inheritable modify right | a new object is created with its final protected DACL in the create call (`create_dir_with_sd`, `open_with_sd`, `acl::SecurityDescriptor`); the data folder is locked first, then `logs`, `frames.bin`, `install.log`; `logs` is swept of entries that are not owned by SYSTEM, Administrators or LOCAL SERVICE | `datadir::tests::a_new_object_has_its_final_permissions_from_the_create_call` (the DACL read back right after the create call, no `lock` call), `the_data_folder_is_locked_before_anything_below_it_is_made` (starts from an older folder with an inheritable user right), `entries_of_logs_that_are_not_owned_by_a_trusted_account_are_removed` |
| L2 | `BetterCam.old-*` next to the data folder was never deleted | `datadir::sweep_data_asides` runs in install and in uninstall, deletes without following links, touches no other name; an old copy that is still in use is reported by uninstall | `datadir::tests::moved_aside_data_folders_next_to_betercam_are_swept_without_following_links`, `install::tests::an_install_and_an_uninstall_delete_the_data_folders_that_were_moved_aside`, `an_old_data_folder_that_cannot_be_deleted_is_reported_by_uninstall` |
| L3 | A detached pump could post a stale sample | `post_sample`: `stop` is checked again under the pump's lock, the one `stop` takes, immediately before the sample is queued | `stream::tests::a_pump_that_is_stopped_while_a_sample_is_ready_posts_nothing` (with a control that does post) |
| L4 | A failed log truncation caused a reopen on every line | `next_try` is set on that path | `log::tests::a_failed_truncation_waits_before_the_next_open` (the file is mapped by "another process", error 1224; counts opens) |
| L5 | LOCAL SERVICE could modify `install.log` | its own protected DACL (SYSTEM, Administrators full; the user read); created with it, or rewritten when an older file is kept | `acl::tests::the_install_log_is_not_writable_by_the_service_and_readable_by_the_user`, `datadir::tests::the_install_log_loses_the_modify_right_the_service_would_inherit_from_logs` |
| L6 | The busy claim worked only inside one app process | the helper takes a machine-wide lock before anything else (`vcam_install::lock`): a mutex in a *private namespace* whose boundary is the Administrators group, so a standard user can neither hold nor squat it (a plain `Global\` name could be created beforehand by anybody); the existence of the object is the lock, so a helper that dies frees it; busy = exit 11, shown by the app as a sentence; `vcam-tool` takes the same lock | `lock::tests::a_second_helper_finds_the_lock_taken_and_the_next_one_gets_it_after_the_first_ends`, `the_end_of_the_holding_process_frees_the_lock` (a second process), `a_boundary_that_is_not_enabled_in_the_token_cannot_make_the_lock`, `vcam_admin::tests::a_helper_that_found_another_one_running_says_so`; live below |
| L7 | The `vcam-tool` result file refused a link only in its last component | no result file at all: the elevated child reports through `install.log` (its own verified, admin-owned file, readable by the user) and the exit code; the parent prints the lines after the child's marker line. `--result` is refused | `vcam-tool util::tests::the_parent_prints_only_what_the_last_elevated_run_wrote`, `a_log_that_this_run_did_not_write_says_nothing`, `a_link_in_place_of_the_log_is_not_read`, `main::tests::the_elevated_copy_takes_no_result_path` |
| L8 | Without `--user-sid` the helper used the elevated account | the helper (and the app's helper call) require it; the app refuses to start a helper when it cannot read its own SID, with a sentence and no prompt; `vcam-tool` passes it always and falls back to the current account only when run directly in an elevated shell or with `--current-user`. **Repair by a second Windows user:** the frame file belongs to one account at a time; the install replaces the user ACE, so the virtual camera works for the account that installed or repaired last and the earlier one repairs it again to get it back. The app says so (banner, button hint, the message after the install) | `vcam_admin::tests::an_install_without_the_users_sid_is_a_usage_error`, `without_the_users_sid_no_helper_is_started_and_the_message_says_why`, `the_app_gives_the_helper_the_users_sid`, `main::tests::the_elevated_step_must_be_told_whose_camera_it_is` |
| L9 | `frames.bin` gave `FILE_WRITE_ATTRIBUTES` and `FILE_WRITE_EA` (hard link from a user's folder, read-only, sparse) | mask `0x12008f`; every open of the file asks for `FILE_READ_DATA | FILE_WRITE_DATA | FILE_READ_ATTRIBUTES` and nothing more (`secure::ACCESS_READ_WRITE`, `Writer`, `MappedFile`); a frame file that is read-only or encrypted, or hard-linked, is replaced by the next Repair (**the mask does not prevent sparse, compressed or an extra data stream**: those need only rights the mask has; see "P-hardening 4", where Repair clears the first two in place and replaces a file that has a stream) | `acl::tests::the_frame_file_acl_denies_hard_links_and_attributes_but_not_a_read_write_mapping` (real files; the control with the old mask `0x12019f` shows both attacks work), `datadir::tests::a_read_only_or_sparse_frame_file_is_replaced`, `a_frame_file_with_attributes_the_users_of_an_older_version_could_set_is_replaced`, `the_frame_file_acl_is_read_and_write_without_delete` |
| 8 | The test of "no second helper until the first exited" could not fail | `elevate::wait_for_exit` is the only wait and has no timeout: it asks for `WAIT_FOREVER`, and anything but "the process ended" is a `Failed` | `elevate::tests::the_wait_returns_only_when_the_process_has_exited` (a real child that runs 3 s; the wait ends with its exit code, not before), `the_wait_has_no_timeout` (the wait function is given a recording stand-in for `WaitForSingleObject`) |
| - | Launch hygiene | `lpDirectory` is the System32 folder (`util::system_dir`, the shell's known folder) | `elevate::tests::the_elevated_process_starts_in_the_system_folder` |
| - | Installer mode | `bundle.targets` is `["nsis"]`, `bundle.windows.nsis.installMode` is `perMachine` (both keys checked against `node_modules/@tauri-apps/cli/config.schema.json`); no installer was built | `npm run tauri build -- --no-bundle` passes |

## The permission design now

Same rules as in P-hardening 2 (protected DACLs, owner Administrators, applied through verified handles). Changed rows
are marked. Everything below is the machine after the live update (`icacls`, owners read back).

| object | owner | ACEs |
|---|---|---|
| `BetterCam` | Administrators | SYSTEM and Administrators `(F)`; LOCAL SERVICE `(RX)`; user `(RX)`; this folder only |
| `logs` | Administrators | SYSTEM and Administrators `(OI)(CI)(F)`; LOCAL SERVICE `(RX,WD)` here and `(OI)(IO)(M)` for the files it makes; user `(RX)` here **(changed: no right on the files in it)** |
| `frames.bin` | Administrators | SYSTEM and Administrators `(F)`; LOCAL SERVICE and user `(R,WD,AD)` = `0x12008f` **(changed: no write of attributes or extended attributes)** |
| `logs\install.log` | Administrators | **(new)** SYSTEM and Administrators `(F)`; user `(R)`; protected, so nothing from `logs` reaches it |

As the standard user (medium integrity), live: creating a file or a folder in `BetterCam` or in `logs`, deleting or
renaming `logs` or `frames.bin`, deleting or writing `install.log`, `mklink /H <own folder>\x.bin frames.bin`
(`Access is denied`), `attrib +R frames.bin` (`Access denied`), `fsutil sparse setflag` (`Error 5`; but that tool asks for more than data access: a handle with only `FILE_WRITE_DATA` can still set the flag, see "P-hardening 4"), opening
`frames.bin` with `WriteAttributes`, `Delete` or `ChangePermissions`, and reading any `vcam-*.log` are all denied. Reading
`install.log`, listing both folders, and opening `frames.bin` with exactly `ReadData | WriteData | ReadAttributes` work.
Note that an open with `GENERIC_WRITE` (a plain .NET `FileAccess.ReadWrite`) is now denied too: it includes the right that
the file no longer gives. The app, the DLL and `vcam-tool` ask for the specific rights.

## Things found only by running it

* **`SetSecurityInfo` on a folder walks its children.** With the data folder locked first (the L1 order), the elevated
  helper changed the permissions of a file a user had hard-linked into the folder under the name `frames.bin` (the
  file is somewhere else too): the old test `a_hard_linked_frame_file_is_replaced_and_the_other_name_is_left_alone` failed.
  `acl::apply` now sets owner and DACL with `SetKernelObjectSecurity`, one `NtSetSecurityObject` on the handle that was
  opened and verified, so the helper changes that object and nothing else.
* **File tunneling.** A file made under a name within seconds of the deletion of the file that had it gets the old
  file's creation time back, so "the file was replaced" cannot be tested through `created()`; the tests put a marker in
  the old file and look for it.
* **A private namespace alias is bound once per process.** A second `CreatePrivateNamespace` or `OpenPrivateNamespace`
  of the same alias in the same process fails with `ERROR_DUP_NAME` (52); it only happens in one process with two
  callers (tests), and means "the lock is held here": `lock::open_namespace` answers busy.
* `attrib` prints `Access denied` and still exits 0; the denial list above reads the attribute back instead.
* **The linker stamps a build time into the PE header.** A relink of an unchanged DLL has another content tag, so the
  app calls the registered one outdated. The machine was updated once more at the end with the final build; do not
  rebuild the release DLL between the last test and the registration.
* The frame file cannot be locked by a test while the app runs: the app's engine keeps its handle for as long as it
  exists. The oplock check of the live file therefore ran with the app closed (see below).

## Check results

```
cargo test --workspace      better-cam 311 passed (+2 in main.rs), vcam-install 89 passed, vcam-protocol 61, vcam-source 57, vcam-tool 17; 0 failed.
                            1 ignored in better-cam (earlier) and 1 in vcam-install (`lock::tests::holder`, the child process of
                            another test). Tests that need the privilege to make a symbolic link, or an elevated token, print
                            "skipped" and pass without it (this run was not elevated)
cargo clippy --workspace --all-targets     0 warnings
cargo fmt --check -p vcam-protocol -p vcam-source -p vcam-install -p vcam-tool     clean (src-tauri is not rustfmt-managed: the
                            files there, vcam_admin.rs included, differ from rustfmt's default at HEAD already, so that file keeps its style)
npm run build               exit 0
npm run tauri build -- --no-bundle     exit 0 (twice: the second after the last code change; the DLL was not relinked)
```

Each of the new tests that can have the old behaviour put back by hand was run that way, one at a time, and failed:
M1 (the open waits; the test fails after its 2 s bound), M2 (the reviewer's message), M3 (no `ERROR:` line in the file),
L2 (old copies stay), L3 ("a sample was posted after the stop"), L4 (the next open is not delayed), and the hard-link
fix above (a regression that the new order caused). The tests of L1, L5, L6, L7, L8, L9 and item 8 test new code
(new functions, new ACEs, new exit code, new argument rules); the L9 test has its own control that shows the attacks
work with the old mask.

Live, on this machine (Windows 11 build 26200, elevation without a prompt), driven from the app through the DevTools
protocol:

```
Update from the banner      2.5 s; install.log: data folder kept and locked, logs replaced ("other permissions"; Frame Server stopped once),
                            install.log created with its own DACL, new DLL registered, old one retired
app LIVE, consumer          234 frames in 8.0 s, max gap 52 ms, video 234, not-running 0;
                            vcam-tool stats: consumer_active 1 (the service writes its header fields through the new mask);
                            a fresh vcam-<pid>.log appears and grows (LOCAL SERVICE creates and writes in the new logs folder)
oplock on frames.bin        app closed, no other handle; a read-write-handle oplock taken through a read-only handle, held 14 s:
                              vcam-tool status 0.11 s; vcam-tool stats refuses in 0.02 s ("another program holds a lock on the file");
                              a consumer opens the camera and gets slate frames from t=0.00 (146 frames in 5 s, not-running slate)
                            after the release: Start -> LIVE, consumer reads video
Remove, then Install        status missing (not listed, no key, no folders), then ok; Start went LIVE without restarting the app; consumer video
second helper               a holder (elevated) kept the machine-wide lock: the helper by hand `--vcam-install --user-sid ...` exited 11,
                            `--vcam-uninstall` exited 11, `vcam-tool register` and `unregister` exited 11 with the message; install.log and the
                            registry unchanged
```

One slip during the run, said here because it changed the machine for a moment: the first attempt at the second-helper
check started its holder with a broken command line, so the "second" helpers really ran (an install, a removal and
`vcam-tool register`). The last one reinstalled the camera at once and nothing was lost; the check was then redone with the
holder shown to be alive before anything else was started. A real install started 0.35 s after the click of the UI
button was too late to meet the first helper (the helper finishes in under a second): the two ran one after the other
and the log shows them not interleaved.

## Notes for packaging

* `installMode: perMachine` installs under `Program Files`; the installer asks for administrator rights, and the app
  that it installs asks for them again for the virtual camera (one prompt). Nothing was built or run for it here.
* The helper's `--user-sid` is always given by the app; an installer that runs the helper must give the SID of the
  user who will run the app (it is checked to be a user account).
* Not done on purpose (decided by the owner later): refusing a helper or DLL source in a folder a user can write to, and
  signature checks.

# P-hardening 4: deletes, renames and the sweep (third review fixes)

A third review judged the DLL and the protocol reader ready and found the faults in the installer's delete and rename
paths. All fixed, each with a test; the ones that can have the old behaviour put back by hand were run that way (see
"Check results"). No change to the frame-file layout, `PROTOCOL_VERSION`, the permission design of "P-hardening 3", the
helper's command line or its exit codes (0, 1, 2, 5, 10, 11). **No source line of `crates/vcam-source` changed.** The
DLL links `vcam-protocol`, whose `secure` module was refactored (a relative `nt_open_at`, one place that closes a
handle on every status other than plain success), so the DLL was rebuilt and registered once (the content tag changes
with any relink); the behaviour of every call the DLL makes is the one it had.

## What changed

| # | Finding | Fix | Tests |
|---|---|---|---|
| 1 | The sweep of `BetterCam.old-*` in `%ProgramData%` acted on entries any user can create, and waited for ever on a locked file in one | `datadir::sweep_data_asides(paths, trusted, log)`: each entry is looked at without following a link and without breaking any lock (`READ_CONTROL`, attributes only) and is **skipped unless its owner is SYSTEM or Administrators** (the helper's moved-aside folders have that owner; a standard user cannot make an entry with it); the owner is checked again on the handle that is deleted through (`secure::Top`); the delete is `secure::remove_tree_checked` (handles, `FILE_COMPLETE_IF_OPLOCKED`). A skipped entry is logged and never counts as a failure: install ignores the result, uninstall reports only entries of the helper's that stayed (as before), and says which file is locked | `datadir::tests::a_sweep_entry_of_somebody_else_is_left_alone_and_never_waited_for`, `an_old_data_folder_of_the_helpers_with_a_locked_file_is_left_without_waiting`, `moved_aside_data_folders_next_to_betercam_are_swept_without_following_links` (junction named like an entry), `install::tests::a_repair_does_not_wait_for_or_fail_on_an_old_data_folder_with_a_locked_file`, `an_uninstall_does_not_wait_for_a_locked_file_in_an_old_data_folder` (foreign entry: no failure, untouched; the helper's own: reported), `an_old_data_folder_that_cannot_be_deleted_is_reported_by_uninstall` |
| 2 | The installer's other deletes and renames waited on such a lock too (`remove_dir_all`, `DeleteFileW`, `std::fs::rename`, `MoveFileExW`) | `secure::remove_tree`, `remove_tree_checked`, `remove_children` and `rename_no_wait` are the only delete and rename the installer uses: `NtCreateFile` with `DELETE` and `FILE_COMPLETE_IF_OPLOCKED`, then `FileDispositionInfoEx` (POSIX semantics, read-only ignored) or `FileRenameInfo` **on that handle**; folders are listed through the held handle and every entry is opened relative to it (`OBJECT_ATTRIBUTES.RootDirectory`), a link is deleted as a link and never entered (checked on the opened handle), depth is bounded. A file that is locked is left, the rest goes, and the error names the first file that stayed and how many more (`RemoveError`). `datadir::remove_no_follow`, `move_aside`, `empty_in_place`, `remove_all_reporting` (uninstall says what stayed behind), the sweeps and the deletes of DLLs in Program Files (`install::delete_file`) use them. A repair that has to replace a locked `frames.bin` fails at once with a sentence that names the file and does not stop the camera service (that does not help against a lock of the user). The log prune in the DLL already deleted through `secure::remove_plain_file` | `secure::tests::a_tree_is_deleted_without_following_links_and_a_locked_file_is_left_without_waiting`, `the_error_says_how_many_entries_stayed`, `a_check_on_the_top_entry_can_stop_the_delete_before_anything_is_changed`, `a_rename_does_not_wait_for_an_oplock_and_never_goes_through_a_link` (a junction is renamed or deleted itself, its target is left), `datadir::tests::removing_the_data_folder_does_not_wait_for_a_locked_file_and_says_which_one_stayed`, `a_logs_folder_that_is_emptied_in_place_does_not_wait_for_a_locked_log`, `moving_a_locked_file_aside_does_not_wait`, `replacing_a_locked_frame_file_is_an_error_that_names_it_and_does_not_wait`, `a_lock_that_is_given_up_within_a_moment_does_not_fail_the_repair_and_one_that_is_not_does`, `deleting_never_recurses_through_a_link`, `moving_a_junction_aside_renames_the_junction_and_not_the_target`, `install::tests::an_uninstall_does_not_wait_for_a_locked_install_log_and_says_what_stayed`, `an_old_dll_that_a_program_holds_a_lock_on_does_not_hold_up_an_update_or_a_removal`, `datadir::tests::an_object_that_gives_the_caller_no_delete_right_is_left_and_named` |
| 3 | The frame file can still be made sparse or compressed or get extra data streams (the mask `0x12008f` has the data rights those need) | Docs and the test comment corrected (see below). Repair **clears the sparse and compressed state in place** through the held handle (`secure::clear_sparse_and_compression`: `FSCTL_SET_SPARSE` false, `FSCTL_SET_COMPRESSION` none; same file, same contents), and **replaces the file when it has any stream besides `::$DATA`** (`secure::extra_streams`, `FileStreamInfo`); read-only and encrypted files are replaced as before, and so is one whose flags cannot be cleared | `acl::tests::a_write_data_handle_can_still_make_the_frame_file_sparse_or_compressed_or_give_it_a_stream` (a `FILE_WRITE_DATA`-only handle on a file with the real mask), `datadir::tests::a_sparse_or_compressed_frame_file_is_cleared_in_place_and_stays_the_same_file` (file index unchanged, contents kept), `a_frame_file_with_an_extra_stream_is_replaced`, `a_read_only_frame_file_is_replaced`, `sparse_and_compressed_files_are_kept_and_a_file_with_a_stream_is_replaced`, `secure::tests::a_sparse_or_compressed_file_is_made_plain_in_place` (flag only, with a hole, compressed), `extra_data_streams_are_listed` |
| 4 | The app could show an earlier run's `ERROR:` line (helper died after fewer than 8 lines) | The tail is cut at the last `---- install:` / `---- uninstall:` marker (`log::from_last_marker`, shared with `vcam-tool`'s `util::run_output`); the markers are constants used by the helper that writes them. When `install.log` itself is locked by another program neither the helper could write its reason nor the app can read it: the app says so instead of showing nothing (`log::install_log_locked`) | `log::tests::the_app_reads_only_the_last_run_and_never_the_error_of_an_earlier_one` (with the old reading as a control that finds the old error), `a_log_that_another_program_holds_a_lock_on_is_reported_as_locked`, `vcam-tool util::tests::*` |
| 5 | The machine-wide lock could report an error when nobody holds it | `lock::open_namespace_with`: "create fails because it exists, then open" starts over (bounded: 20 tries, 10 ms apart) when the open finds nothing because the holder closed its last handle in between | `lock::tests::a_namespace_that_goes_away_between_the_create_and_the_open_is_created_again`, `a_namespace_that_cannot_be_opened_is_reported_after_a_bounded_number_of_tries`, `racing_helpers_never_see_an_error_when_nobody_holds_the_lock` (three processes, real calls) |
| 6 | A handle could leak on an unexpected success status | `secure::finish_open_with` closes a non-null handle on every status but plain success | `secure::tests::a_handle_that_comes_with_an_unexpected_status_is_closed` (a recording close; success codes, informational, pending, warning and error statuses) |

## The permission design: one correction

The mask `0x12008f` of `frames.bin` is unchanged. What it does **not** prevent, because the right is one it has
(reproduced on temp files with the same DACL by `acl::tests::a_write_data_handle_can_still_...`): `FSCTL_SET_SPARSE`
through a handle with only `FILE_WRITE_DATA`; `FSCTL_SET_COMPRESSION` through read and write data; a named stream
(`frames.bin:name`) through `FILE_WRITE_DATA`. The DLL and the app do not mind any of the three, but "a Repair replaces
such a file" was not what the code did for streams (their data survived) and replaced the file for the other two for
no need. `fsutil sparse setflag` fails for the user only because it asks for more than data access. The text of
"P-hardening 3" (L9) and `acl.rs` now say this.

## Things found only by running it

* **Renaming a folder waits for a lock on a file below it, and no create flag can refuse that.** A read-write-handle
  oplock (what any reader can take) on a file makes `SetFileInformationByHandle(FileRenameInfo)` of a folder above it
  block until the holder lets go (a read-handle lock does not: the rename is refused at once). `FILE_COMPLETE_IF_OPLOCKED`
  is a create option. `secure::rename_no_wait` therefore runs the call on a worker with a bound (1.5 s) and
  `CancelSynchronousIo`s it; the rename is then not done (the folder is where it was) and the result is the same
  refusal as an open. A moved-aside data folder or `logs` that holds such a file is therefore reported (`logs` is emptied
  in place instead, as it already was for open files).
* **A delete or rename of a *file* met the lock at the open** (it asks for `DELETE`), so those return at once
  with the refusal; a lock that has been asked to break and never answers keeps every later open of the file refused
  until it answers or the system gives up on it, so retrying at once does not help and `move_aside` does not retry.
* Opens that meet a lock are asked again for about two seconds (`datadir::LOCK_PATIENCE`) before a Repair gives up:
  a program that is asked to give a lock up does so within moments, and a failed Repair for an anti-virus scan would be
  worse than a short wait. A lock that is not given up fails the Repair with a sentence that names the file.
* Tests that look at a locked file must not read it (a plain read waits): they use attribute-only calls.
* **A delete through a handle needs `DELETE` on the object itself.** `DeleteFileW` also deletes an object whose DACL gives the
  caller nothing, through the delete-child right of the folder it is in; no open for `DELETE` that was tried (`CreateFileW`
  with `DELETE`, with `DELETE_ON_CLOSE`, with and without backup intent, relative or absolute) is allowed in that case. Such an
  object is left and named (it can only be one that somebody made with permissions that exclude the administrators: what the
  helper makes grants Administrators full control), and the rest of its folder goes (`datadir::tests::an_object_that_gives_the_caller_no_delete_right_is_left_and_named`).
  A test that made such a file in a sandbox as an unelevated user (a log made with the real accounts) used to leave its
  folder in `%TEMP%`; those tests now use the sandbox's own accounts.
* **`FSCTL_SET_SPARSE` false works on a file with holes** (they become zeros with storage); no filling pass is needed.
* POSIX delete semantics (`FILE_DISPOSITION_FLAG_POSIX_SEMANTICS`) remove the name at once even while a program still has
  the file open with delete sharing, which is what `remove_dir_all` did and what lets `logs` and the data folder go
  while the camera service has a log open; a volume that does not know them gets the classic delete.
* `tauri dev` watches `crates/vcam-install` and `crates/vcam-protocol` and restarts the app on every edit; with
  `BETTER_CAM_TEST_WINDOW` set the restarted app stays in the background.
* Seen once: a Repair started within a second of the app's launch left the engine "Opening the camera..." (STANDBY)
  until Stop and Start; a Repair of a running app does not (checked). That is the engine's claim around a launch, not
  the installer (not touched, not investigated further).

## Not done

* Reads and the rename of the DLL in `Program Files` (`install::copy_dll`: `std::fs::read(dest)`, `rename(tmp, dest)`;
  `std::fs::read(dll_src)`) still use the plain open: a standard user can read that folder, so a lock on the installed
  DLL could make a Repair wait. The deletes there go through handles now. The same bounded handle read would close it.
* A Repair fails (quickly, with the file named) while a program of the user holds an unanswered lock on `install.log`;
  Remove leaves `install.log`, `logs` and the data folder in that case, and says so. Neither waits.

## Check results

```
cargo test -p vcam-protocol -p vcam-source -p vcam-install -p vcam-tool
                            vcam-protocol 68 passed, vcam-source 57, vcam-install 110 (+2 ignored: the child processes of two
                            lock tests), vcam-tool 17; 0 failed (before: 61, 57, 89, 17)
cargo test -p better-cam vcam_admin          20 passed
cargo clippy -p vcam-protocol -p vcam-source -p vcam-install -p vcam-tool --all-targets     0 warnings
cargo fmt --check -p vcam-protocol -p vcam-source -p vcam-install -p vcam-tool              clean
cargo check -p better-cam                    ok
```

With the old behaviour put back by hand (`remove_no_follow` as `remove_dir_all`, `move_aside` as `std::fs::rename`,
the old sweep, in one go), the nine tests that name a lock in `datadir::tests` and `install::tests` fail after their 10 s
bound with "still waiting ... it waits for a lock": the two sweep tests, `removing_the_data_folder_...`,
`a_logs_folder_that_is_emptied_in_place_...`, `moving_a_locked_file_aside_...`, `replacing_a_locked_frame_file_...`,
and the three install tests. `an_old_dll_...` fails the same way with `std::fs::remove_file`. The lock race test
(`racing_helpers_...`) fails with the retry count at 1; the sparse test fails with the flag cleared by
replacing (file index changes); the handle test fails with the close restricted to the oplock status ("status
0x40000000: the handle leaked"); the log test carries its own control (the old reading finds the old error).

Live, on this machine (Windows 11 build 26200, elevation without a prompt, debug build, app started with
`BETTER_CAM_TEST_WINDOW`, driven through the DevTools protocol; the helper is started with `SW_HIDE`, see
`elevate.rs`):

```
update (Repair, new DLL)     1.1 s, status ok, the registered DLL is byte-identical to target\release\better_cam_vcam.dll
standard user's BetterCam.old-test with f.txt, an oplock held on f.txt by a hidden non-elevated process
                             Repair 0.8 s, "done", status ok; install.log: "BetterCam.old-test left alone: its owner is S-1-5-21-...-1001, not SYSTEM
                             or Administrators"; the entry and the locked file untouched; no helper left; a second Repair 0.2 s
oplock on install.log (read-only open, nothing else had it open)
                             Remove 3.7 s: "Could not remove the virtual camera (exit code 1) The details cannot be shown: another program has the
                             install log ... open and locked ..."; camera, keys and DLL gone, logs\install.log left; Install while the lock is
                             still held: 0.2 s, the same sentence; after the release: Install 0.3 s, status ok (the machine lock was free)
frames.bin made sparse by a FILE_WRITE_DATA-only handle (app closed, standard user)
                             attributes Archive, Temporary, SparseFile -> Archive, Temporary; file id 0x036d00000000e2ec before and after; Repair 1.2 s,
                             install.log: "frames.bin: it is sparse or compressed; clearing that in place"; app LIVE, a consumer read 234 video frames in 8 s
Remove, Install              5.8 s, 0.3 s; status missing, then ok; the user's old entry was skipped again, not an error
```
