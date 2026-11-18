# Performance sign-off (plan 4.8)

Measured 2026-11-18 on commit `1c950ef` (release build; `engine_script` also carries one new step, `engine_stats`, see Method).
Nothing was tuned. Every number below comes from a run of that day; the raw logs, per-second process samples (`*.samples.csv`)
and summaries (`*.summary.txt`) are in `C:\Users\Obnoxious\AppData\Local\Temp\opencode\perf\` (not in the repo). The harness
(`perf_batch.ps1`) and the script generator (`gen_scripts.ps1`) are in the same folder.

## Hardware and conditions

| | |
|---|---|
| PC | Windows 11 Pro build 26200, Ryzen 7 9800X3D (8 cores, 16 logical), 31.7 GB RAM |
| GPU | NVIDIA GeForce RTX 5080, driver 621.01 (the plan's GPU figures are for a GTX 1650 / Iris Xe / RTX 3060: this GPU is far faster) |
| Camera | `W4GS`. Live rate in this room: **14.4 fps** at 1080p (auto exposure), so every rate-dependent number uses the synthetic source. No camera control was touched. No 4K mode, so 4K is synthetic only. |
| Background load | Other agents were working on the PC. "Machine busy" in the summaries was 11 to 22 % in the windows (38 % in one live run). Compilation was never running during a timed run (the harness waits for `rustc` to end). |
| CPU readings | CPU of the app is the engine process `engine_script.exe` (engine + capture + GPU + writer; no WebView2, no UI, no preview). The consumer is a separate process (`p6b-consumer.exe`, a plain Media Foundation reader of the virtual camera), so its cost is not in the app's. |

## Method

* Scripts: `src-tauri/examples/scripts/perf_*.json`, run by `engine_script` through the same `Service` functions as the app. Each script: `start` (synthetic NV12 bars, or the camera), wait for `live`, start the consumer process (`run`, no wait), settle 6 s (8 to 10 s for live and NVIDIA), then `engine_stats` for 20 s (25 s live), then `stats_report`, `vcam-tool stats`, stop. `perf_steady.json` = 10 steps of `engine_stats` x 60 s.
* Chain of "four shader filters" = Color Correction (gamma 0.1, contrast 0.15, brightness 0.03, saturation 0.25, hue 10) + Sharpen 0.35 + Apply LUT (33^3 `.cube`, amount 1) + Scaling (bicubic to 1280x720; 2560x1440 at 4K) with the output left at the source size (so the picture is scaled back by the output stage). "Heavy" = those four + Chroma Key + Auto Adjust. NVIDIA filters ran alone in the chain.
* New step `engine_stats {label, seconds}` in `engine_script.rs` (the only code change; about 55 lines, example only): samples `engine.stats()` twice a second and prints fps (capture, render, publish), `gpu_ms`, `readback_ms`, `cpu_ms` (mean and max of the engine's own one-second means, **not per-frame maxima**), counter deltas, the engine's latency percentiles and the header counters of `frames.bin` (`latency_p50_us`, `latency_p95_us`, `delivered`, `repeated`). The `stats` event itself leaves these out.
* Latency: `latency_p95_us` in the `frames.bin` header = capture timestamp to delivery by the DLL (the plan's definition); the engine's own "pipeline p95" (arrival in the capture callback to published) is listed where the camera is live.
* CPU and memory: `Win32_PerfRawData_PerfProc_Process` (cumulative CPU time and private bytes) once a second for the engine process, the consumer and the `svchost` that hosts `FrameServer` (found through the service's process id, which is readable without admin). CPU = delta CPU time / delta wall time over the `engine_stats` window, interpolated at the window edges (the counter ticks in 15.6 ms steps, so single windows are quantised: expect about +-1.5 % of a core). "% of machine" = / 16 logical CPUs. Camera-service CPU and memory minus an idle baseline = the same svchost sampled for 10 s before each run with no app and no consumer (CPU 0.0 %, 5.1 to 5.4 MB in synthetic runs).
* Commands (one run = one name, lock `app-test.lock` held per batch): `powershell -File perf_batch.ps1 -Names synth1080_none,synth1080_four,synth1080_heavy` (and the other names below; `-Steady` adds a once-a-minute `nvidia-smi` / GPU Process Memory / header sample); build: `cargo build -p better-cam --release --examples` (`CARGO_INCREMENTAL=0`).
* "Dropped": engine `dropped_mailbox + dropped_output + errors` and the DLL's `repeated`, as deltas over the window. The engine's counters refresh once a second, so a 20 s delta is 1200 +-60; only totals and the dropped counters are reliable.
* Repeat runs: configurations 1 and 2 were run 4 times each (the first pass raw file was overwritten by a later run; its numbers are kept from the console in the ranges below). The others once.

## Results

All with a consumer attached. fps = capture / render / publish (equal in every run unless noted). `gpu` = mean (max of 0.5 s samples) ms. `rb` = readback map + copy ms, `cpu` = CPU-side encode/submit ms per frame (means). Latency = header p50 / p95 ms. `app CPU` = % of one core (= x/16 % of this machine). `FS` = camera service CPU % of one core and private bytes added over idle.

### 1080p60 synthetic

| Config | fps | gpu | rb | cpu | latency p50/p95 | dropped / repeated | app CPU | app private | FS CPU / mem |
|---|---|---|---|---|---|---|---|---|---|
| 1 no filters (4 runs) | 60.0 | 0.156-0.169 (max 0.191) | 0.33-0.42 | 0.20-0.23 | 1.4-1.9 / 2.44-2.62 | 0 / 0 | 1.3-4.9 % (0.1-0.3 % of machine) | 105-110 MB | 2.5-6.8 % / +7.1-7.7 MB |
| 2 four filters (4 runs) | 60.0 | 0.263-0.267 (max 0.299) | 0.49-0.53 | 0.22-0.26 | 2.0-2.4 / 2.47-2.64 | 0 / 0 | 3.3-7.1 % (0.2-0.45 %) | 113-115 MB | 1.5-6.5 % / +7.2-7.4 MB |
| 3 heavy (four + Chroma Key + Auto Adjust) | 60.0 | 0.320 (0.350) | 0.52 | 0.28 | 1.95 / 2.49 | 0 / 0 | 4.0 % | 119 MB | 6.4 % / +7.5 MB |
| 5a NVIDIA Background Removal, mode 0 | 60.0 | 2.72 (3.19) | 0.34 | 2.89 | 4.0 / 5.48 | 0 / 0 | 20.9 % | 772 MB | 2.9 % / +7.9 MB |
| 5b same, `processing_interval` 4 | 60.0 | 1.33 (1.72) | 0.80 | 0.97 | 2.9 / 5.19 | 0 / 0 | 9.0 % | 771 MB | 5.1 % / +7.2 MB |
| 5c NVIDIA Background Blur | 60.0 | 3.00 (3.17) | 0.32 | 3.10 | 4.2 / 5.52 | 0 / 0 | 14.9 % | 785 MB | 3.8 % / +7.3 MB |
| 5d NVIDIA Blur | 60.0 | 1.12 (1.28) | 0.34 | 1.29 | 2.4 / 3.92 | 0 / 1 | 9.5 % | 564 MB | 3.7 % / +6.7 MB |

Cost per filter (one run each, chain grows one filter at a time, mean gpu ms): empty 0.169, + Color 0.176, + Sharpen 0.215, + LUT 0.229, + Scaling 0.266, + Chroma Key 0.292, + Auto Adjust 0.310 (`perf_breakdown1080.json`). The LUT's `filter-error` event with an empty message at start is the "cleared" notice; the visible effect of the LUT was not checked in this session, only its cost step (+0.014 ms).

### 3840x2160 at 30, synthetic

| Config | fps | gpu | rb | cpu | latency p50/p95 | dropped / repeated | app CPU | app private | FS CPU / mem |
|---|---|---|---|---|---|---|---|---|---|
| 6a no filters | 30.0 | 0.558 (0.574) | 1.19 | 0.67 | 4.5 / 5.08 | 0 / 0 | 5.7 % (0.36 % of machine) | 221 MB | 6.9 % / +32.0 MB |
| 6b four filters (scale to 2560x1440) | 30.0 | 0.910 (0.930) | 1.46 | 0.64 | 4.5 / 5.19 | 0 / 0 | 7.2 % (0.45 %) | 238 MB | 8.3 % / +28.2 MB |
| 6c NVIDIA Background Removal | 30.0 | 3.06 (3.32) | 1.00 | 3.28 | 6.6 / 8.05 | 0 / 0 | 13.8 % (0.87 %) | 967 MB | 6.1 % / +31.2 MB |

4K breakdown (mean gpu ms): empty 0.565, + Color 0.611, + Sharpen 0.751, + LUT 0.790, + Scaling 0.918, + Chroma Key 1.024, + Auto Adjust 1.085; app private grows to 257 MB with Chroma Key + Auto Adjust, app CPU 5.4-10.3 % of a core (`perf_breakdown4k.json`, 5 s windows).

### 1080p live camera (MJPEG), camera at 14.4 fps

| Config | capture fps | gpu | rb | cpu | header latency p50/p95 (engine pipeline p95) | dropped / repeated | app CPU | app private | FS CPU / mem |
|---|---|---|---|---|---|---|---|---|---|
| 4a no filters, HW decode | 14.37 | 0.103 (0.117) | 1.21 | 0.13 | 19.0 / 19.9 (1.6) | 0 / 1168 of 1528 delivered | 3.4 % | 193 MB | 2.5 % / +98.6 MB |
| 4b four filters, HW decode | 14.38 | 0.205 (0.259) | 1.41 | 0.18 | 19.6 / **47.7** (2.4) | 0 / 1134 of 1496 | 3.3 % | 202 MB | 2.1 % / +94.6 MB |
| 4c no filters, **software decode** (`hw_decode` false) | 14.39 | 0.164 (0.195) | 0.36 | 0.25 | 19.9 / 20.6 (0.8) | 0 / 1176 of 1536 | 6.3 % | 113 MB | 2.5 % / +49.5 MB |

The "repeated" count is by design here: the camera gives 14 fps and the consumer reads 60 fps. The live latency includes the camera's own time before the frame reaches the app (about 17.5 ms; the engine's pipeline p95 is 0.8 to 2.4 ms). Run 4b had a 47.7 ms p95 with the machine 38 % busy; the app's own part stayed at 2.4 ms. The Frame Server memory in live runs includes the physical camera stream (it also goes through Frame Server), not only our DLL: the DLL alone is the +7 MB of the synthetic runs. Software decode costs +2.9 % of a core at 14.4 fps (6.3 vs 3.4 %), about 2 ms of CPU per frame; at 60 fps that would be roughly +12 % of a core (extrapolated, not measured: the camera does not reach 60 fps here).

### 7. Steady state: 10 minutes, 1080p60 synthetic, four filters, consumer attached

| Minute | gpu mean (max) | dropped (mailbox + output) | DLL repeated (cumulative) | header latency p95 (ms) | app CPU | app private (avg) | FS CPU over idle | FS private over idle |
|---|---|---|---|---|---|---|---|---|
| 1 | 0.267 (0.298) | 0 | 0 | 2.57 | 7.5 % | 113.8 MB | 4.0 % | +7.4 MB |
| 2 | 0.269 (0.415) | 0 | 4 | 2.54 | 7.2 % | 114.1 MB | 4.2 % | +7.2 MB |
| 3 | 0.265 (0.304) | 0 | 4 | 2.47 | 7.5 % | 114.2 MB | 5.3 % | +7.2 MB |
| 4 | 0.266 (0.300) | 0 | 5 | 2.50 | 7.8 % | 117.1 MB | 4.4 % | +7.0 MB |
| 5 | 0.266 (0.297) | 0 | 5 | 2.52 | 7.7 % | 117.5 MB | 4.8 % | +6.8 MB |
| 6 | 0.268 (0.299) | 0 | 5 | 2.50 | 8.0 % | 117.5 MB | 5.8 % | +7.0 MB |
| 7 | 0.266 (0.291) | 0 | 5 | 2.59 | 7.2 % | 117.6 MB | 5.1 % | +6.9 MB |
| 8 | 0.259 (0.282) | 0 | 5 | 2.45 | 7.2 % | 117.7 MB | 6.1 % | +7.0 MB |
| 9 | 0.264 (0.295) | 0 | 5 | 2.48 | 7.1 % | 117.8 MB | 4.0 % | +7.2 MB |
| 10 | 0.259 (0.275) | 0 | 5 | 2.53 | 7.3 % | 118.3 MB | 5.4 % | +7.2 MB |

* Totals at the end: captured = rendered = published = 36 353 (60.0 fps), `dropped_mailbox` 0, `dropped_output` 0, errors 0; DLL `delivered` 36 416, `repeated` 5 (0.014 %). The 5 repeats came in minutes 2 and 4 (machine busy 19.7 % in minute 2).
* Leak check: app private bytes 112.3 MB (first sample) -> 118.4 MB (last): +6.1 MB, +5 MB of it in the first four minutes, then about +0.15 MB a minute. Frame Server private bytes flat (+7 MB over idle all the time). GPU: the engine's dedicated GPU memory 42.89 MB at minute 1 and 42.90 MB at minute 10 (flat); `nvidia-smi` total memory.used 3222 MiB at 61 s and 3222 MiB at 548 s (it was 3194 MiB before streaming; 3163 MiB at the very end, other processes). A creep of 0.15 MB a minute is too small to call a leak from 10 minutes; a one-hour run would settle it.

## Targets (plan 4.8)

| Target | Measured | Verdict |
|---|---|---|
| Added latency p95, 1080p60: <= 25 ms | 2.44 to 2.64 ms (header, synthetic, all non-NVIDIA configs); 5.2 to 5.5 ms with NVIDIA; 2.45 to 2.59 ms through the 10 minutes | **Met** |
| Added latency p95, 4K30: <= 50 ms | 5.08 to 5.19 ms; 8.05 ms with NVIDIA removal | **Met** |
| Added latency, live camera (not a plan row): | 19.9 to 20.6 ms p95 at 14 fps (camera's own ~17.5 ms included); one run 47.7 ms under 38 % machine load, the app's part 2.4 ms | informational |
| GPU time, 4 shader filters, 1080p60: <= 2 ms | 0.259 to 0.269 ms mean (max 0.299 in the short runs, 0.415 once in the 10-minute run; 0.32 ms for the 6-filter chain). RTX 5080, far above the plan's GTX 1650 | **Met** (this GPU); not measurable on the plan's reference GPUs |
| GPU time, 4 shader filters, 4K30: <= 4 ms | 0.91 ms mean (max 0.93) | **Met** (this GPU) |
| NVIDIA background removal, 1080p: <= 6 ms | 2.72 ms mean (max 3.19); 1.33 ms with `processing_interval` 4. RTX 5080, the plan's figure was an unverified RTX 3060 estimate | **Met** (this GPU) |
| NVIDIA background removal, 4K: <= 12 ms | 3.06 ms mean (max 3.32) | **Met** (this GPU) |
| App CPU, 1080p60: <= 4 % of a 6-core desktop (= 24 % of one core) | 1.3 to 8.0 % of one core (about 0.1 to 1.3 % of 6 cores); 7.1 to 8.0 % in the steady four-filter run; 20.9 % with NVIDIA removal (3.5 % of 6 cores) | **Met** (engine process only: excludes WebView2, UI and preview, which this session could not run) |
| App CPU, 4K30: <= 6 % of a 6-core desktop (= 36 % of one core) | 5.7 to 7.2 % of one core; 13.8 % with NVIDIA removal | **Met** (same scope) |
| Software MJPEG decode (no row target: ~1 core, unverified) | +2.9 % of a core at 14.4 fps (about 2 ms per frame); about +12 % of a core extrapolated to 60 fps | **Not measurable at 60 fps here**; the estimate ("~1 core") looks far too high on this CPU |
| Frame Server CPU for the DLL: <= 1 % (<= 2 % at 4K) | `% of one core` (the plan's `\Process(svchost*)` counter): 1080p60 1.5 to 6.8 % (steady run 4.0 to 6.1 %, mean 4.8 %); 4K30 4.5 to 8.6 %. As % of this 16-logical machine: 0.1 to 0.4 % (4K 0.3 to 0.5 %). As % of a 6-core desktop: 0.8 % (steady 1080p60), 1.4 % (4K, worst window) | **Missed** as the plan's counter reads it (by about 4 to 6 times at 1080p60, 2 to 4 times at 4K, worst windows 6.8 % and 8.6 %); inside the target if read as % of a 6-core machine. The figure includes everything Frame Server does for the consumer; it did not rise with resolution (1080p60 and 4K30 are similar), and the run-to-run spread is wide |
| Memory, app without NVIDIA: <= 250 MB | 105 to 119 MB synthetic (118 MB at minute 10); 193 to 202 MB live with HW decode; 113 MB live software decode | **Met** |
| Memory, app at 4K: <= 400 MB | 221 to 238 MB (257 MB with Chroma Key + Auto Adjust) | **Met** |
| Memory, added in the camera service: <= 50 MB | +7 MB at 1080p60; +28 to +33 MB at 4K30 (+95 to +99 MB in live runs, but that includes the physical camera stream) | **Met** (DLL part) |
| Zero dropped frames in 10 min | Engine: 0 dropped in 36 353 frames (capture = render = publish). DLL: 5 repeated frames in 36 416 delivered (0.014 %) | **Missed by 5 repeated frames** on the strict reading of `repeated`; **met** for frames lost inside the engine. 4K30: 0 / 0 in 20 s runs only, not measured for 10 minutes |
| Preview: 30 fps, <= 60 ms, <= 3 % CPU incl. WebView2 | Not measured (needs the app window). Measured earlier at 16 fps: +1.1 to +1.4 % of total CPU; not measured at 30 fps | **Not measurable here** |

Also not covered: 4K from a real camera (none here), live 60 fps (camera gives 14.4 fps in this light), an install on a 6-core machine, the plan's reference GPUs, and any run with the app window and WebView2.

## Surprising or worth a look

* **Frame Server CPU** is the only real miss, and it does not scale with resolution or with the filters; consecutive identical runs gave 1.5 % and 6.8 % of a core. Where it goes (the DLL's copy per frame versus Frame Server's own work for the consumer) is not separable from these counters.
* **DLL repeated 5 frames in 10 minutes** (synthetic 60.0 fps against a 60 Hz consumer, two free-running clocks), all while the machine was busier; the engine never dropped. A longer run with a quiet machine would show whether it is load or clock phase.
* **Live camera at 14.4 fps** (auto exposure, dim room): `repeated` is then 76 % of what the DLL delivers, by design; the drop counters are only meaningful with the synthetic source.
* **Hardware decode costs memory**: app private bytes 193 MB with HW decode vs 113 MB with software decode, and Frame Server +99 MB vs +50 MB. CPU is the other way (3.4 % vs 6.3 % of a core).
* **NVIDIA effects**: removal mode 0 gives 2.72 ms at 1080p and 3.06 ms at 4K (so cost barely follows pixels); `processing_interval` 4 halves it; app private bytes 564 to 967 MB, CPU 9 to 21 % of a core (the CPU thread waits on the GPU: `cpu_ms` ~ `gpu_ms`). Blur had one repeated frame in 20 s.
* **Readback is the largest per-frame cost without NVIDIA**: 0.33 to 0.55 ms at 1080p, 1.2 to 1.8 ms at 4K, against 0.16 to 0.27 ms (1080p) and 0.56 to 0.91 ms (4K) of GPU time.
* **Against earlier phases**: no regression in the GPU figures. Convert-only is 0.103 ms live (earlier: 0.11 ms) and 0.156 to 0.169 ms with the synthetic source; Color Correction gives 0.176 ms (earlier: 0.18 ms); NVIDIA removal 2.72 ms at 1080p (earlier: 3.1 to 3.8 ms at 720p, so no worse). Transport latency p95 is 2.4 to 2.6 ms against 1.96 ms earlier: about 0.5 ms higher, with other work running on the PC (p50 1.4 to 2.4 ms); far below the 25 ms target but not a match for the earlier figure.
* Counter caveats: `gpu_ms` "max" is the largest of the engine's one-second means, not a worst frame; engine counters refresh once a second.
* The repo also carries `src-tauri/windows/hooks.nsh` as modified by the installer agent, not by this work.


## After engine fixes (2026-11-18, working tree on top of `ae89677`)

Six faults found by a code audit of the engine, fixed and measured. "Before" is the HEAD engine plus a measurement-only patch
(the `prefill_latency` hook, `worst_frame_ms`, `stats_ms`, the skip counters: same code in both builds); "after" is the working
tree. Same PC, camera, method and conditions as above (synthetic 1080p60 NV12 bars, four filters, pure consumer process, 20 s
windows). Raw logs, summaries and the harness are in `C:\Users\Obnoxious\AppData\Local\Temp\opencode\perf-eng\` (not in the
repo): `perf_eng_batch.ps1` is `perf_batch.ps1` with an `-Build old|new` switch, `pe-consumer.exe` is `p6b-consumer.exe` plus
`--fps N` (it picks the native type of that rate, so a 30 fps app can be played), `engine_script_old.exe` / `engine_script_new.exe`
are the two builds. The scripts are `src-tauri/examples/scripts/perf_eng_*.json` (generated by `gen_eng_scripts.ps1`). Commands:

```
cargo build -p better-cam --release --examples                  # CARGO_INCREMENTAL=0
powershell -File perf_eng_batch.ps1 -Names eng_four_c60,eng_four_none,eng_four_c30,eng_four_stall,eng_four_attach -Build new   # and -Build old
powershell -File perf_eng_batch.ps1 -Names eng_four_c60_5min -Build new -MaxHoldSeconds 760
powershell -File perf_eng_batch.ps1 -Names 4k30_none -Build new       # the 4K30 script of the sign-off, memory at 4K
```

The machine was busy during these runs (25 to 93 % in the windows, other agents), so single CPU readings are good to about +-2 %
of a core: the CPU rows give every run. The engine's frame counters refresh once a second, so a 20 s delta is +-30 frames at 30 fps.

### What changed

| # | Fault | Change | Test |
|---|---|---|---|
| 1 | Whole-run latency percentile sorted up to 200 000 samples twice a second on the render thread, frozen at the cap | `LatencyTrack`: a fixed histogram (0.05 ms bins to 1 s, overflow bin, exact maximum), no allocation after start, never capped | `the_latency_histogram_matches_a_sorted_run_and_a_push_allocates_nothing` (3 million samples against a sorted run, within a bin; 0 allocations in the pushes), `reading_the_whole_run_percentile_after_200000_samples_is_not_a_sort` |
| 2 | Frames nobody takes were encoded, read back and published | No app attached: rendered for the preview only (no encode, readback, publish; heartbeat, writer `Live` and the tap go on; the first frame after an app attaches is published at once). App at a lower rate than the camera: `Pacer` skips what the virtual camera would drop | `the_pacer_never_holds_back_a_frame_the_virtual_camera_takes` (property test against the virtual camera's rule), `a_consumer_at_half_the_camera_rate_gets_every_other_frame`, `a_consumer_at_the_camera_rate_or_above_it_gets_every_frame`, `without_an_app_reading_the_camera_frames_are_rendered_but_not_published`, `the_first_frame_after_an_app_attaches_goes_out_at_once_and_leaving_stops_it`, `a_consumer_that_changes_its_rate_gets_a_frame_at_once` |
| 3 | Synthetic frames stamped with the wake-up time, scheduled from the plan | `Schedule`: stamp = planned time | `a_late_wake_up_does_not_move_the_stamps_of_the_synthetic_source`, `the_synthetic_source_stamps_the_planned_time_not_the_wake_up` |
| 4 | Staging ring held 3 slots, the default policy uses 1 | `ReadbackRing` makes a slot when a frame first needs one (lowest free slot; a full ring reuses the evicted frame's slot) | `gpu::readback::tests::a_ring_that_maps_each_frame_at_once_owns_one_slot_and_grows_only_with_pending_frames` (WARP) |
| 5 | `stats` 4 times a second, `controls-changed` every second, hidden window included | `stats`: only while the window is visible, only when the payload differs from the last one sent, once when the window is shown. Controls: read and sent only while visible, only when a control moved, one list when the window is shown | `stats_events_come_when_the_numbers_change_and_only_while_the_window_is_visible`, `a_stats_event_follows_a_state_change_once_and_a_returning_window_gets_one`, `the_controls_are_read_and_sent_only_while_the_window_is_visible_and_only_when_they_changed`, `camera_controls::tests::calls_use_the_engines_camera_while_it_is_open` |
| 6 | "Queue of 3 hands over the oldest frame" | **Wrong for the engine** (see below). `CaptureReader::recv_newest` added; the engine's camera source uses it | `a_slow_consumer_gets_the_newest_frame_not_one_a_queue_old`, `the_newest_frame_goes_before_a_stop_is_reported` |

The rule of finding 2A: the virtual camera takes a frame captured at least 3/4 of its period after the last frame it took
(`frames.rs`, `min_capture_gap`). The engine tracks the capture time of the last frame the virtual camera *would have taken* from
what was published (it runs the same rule on its own output) and holds a frame back only when it was captured less than 7/10 of the
consumer's period after that. By induction the frames the virtual camera takes from the published sequence are exactly the frames it
takes from the whole sequence; the property test runs 9 camera rates x 7 consumer rates x 4 jitter levels (and a clock that steps
back) against a copy of the virtual camera's rule, and also checks that at least 20 % of the frames are saved. It only runs when the
consumer's rate is more than 5 % below the camera's; a consumer at the camera's rate or above it is never paced.

### 1080p60 synthetic, four filters: consumer at 60 fps (unchanged), none, and 30

All numbers over a 20 s window, one row per run. `pub` = frames the engine published (counter delta), `DLL` = frames the DLL
delivered (header counter delta), `gpu` / `rb` = mean GPU time and readback ms per frame.

| Config | Build | engine publish fps | frames published / DLL delivered | gpu ms | rb ms | app CPU, % of one core (runs) | app private MB |
|---|---|---|---|---|---|---|---|
| Consumer at 60 | before | 60.0 | 1153 / 1208, 1272 / 1216, 1154 / 1200 | 0.266-0.297 | 0.75-0.85 | 8.5, 6.3, 8.2 | 111-116 |
| Consumer at 60 | after | 60.0 | 1214 / 1200, 1211 / 1200, 1149 / 1200, 1153 / 1200 | 0.264-0.280 | 0.58-0.86 | 7.4, 6.6, 9.2, 8.1 | 110-113 |
| No consumer | before | 60.0 | 1151-1213 / 0 | 0.260-0.285 | 0.49-0.76 | 8.6, 6.4, 7.9 | 113-116 |
| No consumer | **after** | **0.0** | **0 / 0** (1211-1212 rendered, all skipped) | **0.190-0.195** | **0** | **2.9, 3.5, 2.7, 3.3** | 102-105 |
| Consumer negotiated 30 | before | 60.0 | 1151-1212 / 600 | 0.262-0.272 | 0.61-0.72 | 7.5, 7.5, 5.2 | 113-115 |
| Consumer negotiated 30 | **after** | **30.0** | **575-607 / 600** (skipped for rate: 575-606) | 0.257-0.270 | 0.51-0.69 | **2.8, 5.7, 6.6, 5.5** | 111-113 |

* **Consumer at 60: unchanged.** Frame counts equal the DLL's, no frame is skipped (`skipped_rate` 0), GPU and readback times are the same. The
  DLL's header latency (capture time to delivered) p50 / p95: before 1.8-2.3 / 2.4-3.7 ms, after 2.3-2.4 / 2.65-3.3 ms. The small rise of the
  median is the stamp of finding 3 (the engine's own p50 went from 0.8 to 1.1 ms for the same reason: a synthetic frame is now stamped
  with the time it was planned for, which is up to a wake-up earlier than it was made); GPU, readback and CPU times did not move.
* **No consumer:** the app's CPU falls to 2.7-3.5 % of a core from 6.4-8.6 %; GPU time per frame falls by about 30 % (0.19 against 0.27 ms: the
  encode pass and the copy to the staging texture are gone) and the readback is zero. `vcam-tool stats` during the run: `writer_state: live`,
  `writer_heartbeat_age_ms: 2`, `writer_frames: 0`, `consumer_active: 0`. The engine is `Live` (state events as before).
* **Consumer at 30 fps** (a 30 fps app on the 60 fps camera, `pe-consumer --fps 30`): the engine now publishes 30 fps, 575-607 frames per 20 s
  against 600 delivered by the DLL (before: 60 fps and 1151-1212 frames, half of them dropped unseen by the DLL); `repeated` 0 in all of them;
  the app's CPU is 2.8-6.6 % against 5.2-7.5 % (the readback copy and publish of half the frames are gone; the measurement noise is as large as
  the saving here, the frame counts are the evidence).
* **First frame after an app attaches** (`eng_four_attach`: 6 s without a consumer, then `pe-consumer`): before and after, the consumer's first
  frame is the live picture (`timeline: t=0.00 video` before, `t=0.03 video` after, no "starting" slate); the engine publishes the first frame it
  renders after it sees the consumer.

### Finding 1: the stall

The hook `prefill_latency 200000` (an `engine_script` step, `Engine::prefill_latency`) adds 200 000 scattered latencies (1-6 ms) to the
render thread's whole-run statistics, what an hour at 60 fps leaves, and `engine_stats` reports the time the render thread spent closing
each one-second statistics window (`closing a statistics window`) and its longest pass (`worst render pass`). 30 s windows, consumer at 60:

| Build | closing a statistics window, mean / max | longest render pass of the windows | gpu ms | 
|---|---|---|---|
| before, runs 1-2 | **3.8 / 7.2 ms**, **3.5 / 4.8 ms** | 8.4, 5.5 ms | 0.264, 0.262 |
| after, runs 1-2 | **0.010 / 0.011 ms**, **0.012 / 0.054 ms** | 12.7, 1.9 ms | 0.258, 0.262 |
| before, no prefill | 0.04 ms at 30 s (about 2 000 samples), 0.26 / 0.77 ms after 5 minutes (18 000 samples) | 2.9-3.6 ms | |
| after, 5 minutes, no prefill | 0.011 / 0.036 ms | 9.2-18.3 ms (noise) | |

Closing a window cost the render thread 3.5-3.8 ms every second in the old code once the history was full, and 0.01 ms in the new. The
"longest render pass" column is dominated by the machine's scheduling noise (the other agents' work), not by statistics: one run of the new
code shows 12.7 ms with the window closing in 0.011 ms; the audit's "5.6 ms at the cap" sort is 5.95 ms in the unit test on this PC (the
sort of 200 000 samples, `reading_the_whole_run_percentile_after_200000_samples_is_not_a_sort`) against under 0.1 microseconds for the
histogram read (5 microseconds at worst, with a sample at 900 ms).

### Finding 3: repeated frames over 5 minutes

| Build | repeated (DLL, over the 5 min window) | engine publishes with a gap over 1.5 periods | app CPU | machine busy |
|---|---|---|---|---|
| before (one run) | 1 | not counted | n/a | |
| after, run 1 | 5 (6 in total with the start) | not counted yet | 5.8 % | 32.9 % |
| after, run 2 | **0** | 1 | 7.3 % | 30.6 % |

The audit expected 0. The stamps are fixed (the unit tests: with one wake-up 12 ms late all stamps stay one period apart; stamping with
the wake-up gave 0.28 of a period, which the DLL drops), but the 5-minute runs do not show the effect cleanly: 1, 5 and 0 repeated frames in
three runs on a machine that was 30 % busy, so a repeat here comes from a publish that really came late (the render thread was held up
for more than 25 ms: `late_publishes`, a counter added to the engine stats, counts them) or from the two free-running clocks, which the
stamps cannot change. The counter was added after the first of these runs.

### Finding 4: committed memory at 4K

`perf_4k30_none.json` (3840x2160 at 30, no filters, consumer attached; Immediate policy), app private bytes, two runs each:

| Build | app private bytes avg (MB) |
|---|---|
| before | 220.6, 220.9 |
| after | **197.0, 197.0** |

-23.7 MB: the two 12.4 MB NV12 staging textures the ring no longer makes. At 1080p they are 3.1 MB each (app private bytes 111-116 MB before, 110-113 MB after, in the tables above).

### Finding 5: events

`stats_report` over the same windows with the window shown (`window visible`): before 4.00 Hz (80 events in 20 s); after 0.99-1.00 Hz (20 events
in 20 s, 300 in 300 s), and none while hidden (0 events in the first batch of runs, in which the window was not shown). Controls (from the code and the unit tests, not measured live): before one `controls-changed` a second with the
whole list and a device read of every Auto control, also while hidden; after, nothing while hidden and a list only when a control moved
(unit test with the fake camera: 0 events in 2.5 s hidden, one on show, none for 2.5 s unchanged, one when the lens moved). The frontend needed no
change: `src/store.ts` replaces the stats on every `stats` event and `Stats.tsx` has no timer or "stale" check; the only 4 Hz is a comment
("The 4 Hz stats live in their own store"). Note for the window: with no app attached the output fps shown is the render rate (the picture the preview
gets), and the latency is the figure of the last app that was attached (it is 0 before the first one).

### Finding 6: the claim does not hold for the engine

`capture/reader.rs:73` sets a default queue of 3, but the engine never uses it: `open_camera` sets `queue_len = 1` (a latest-frame mailbox, a newer
frame replaces the one the render thread has not taken) and `CameraInput::next` drained the queue to its newest frame. So a render thread that is slower than the camera
already got the newest frame, which is also what `docs`' phase-1 notes say. The "buffering on" setting (`Buffering`, `SourceCfg.buffering`) is not
wired to any queue length: it only takes part in the "capture settings changed, reopen" comparison. What changed: the drain loop moved into
`CaptureReader::recv_newest` (the older frames are counted as dropped there, in `ReaderStats::dropped`, instead of in an engine-side counter),
so any longer queue also hands over the newest frame; the test shows that with queues of 1, 3 and 5 the newest frame comes and the others are
counted, and that the oldest-first path is the one that is two frames old.

### Not measured

A live camera (14 fps in this light; the pacing needs a rate above the consumer's), 4K with an app at a lower rate, the preview window (the
engine runs without a tap in these scripts), the gain of the fence that keeps the CPU two frames ahead of the GPU when nothing is read back (it
costs two calls a frame; it matters only where the GPU is slower than the camera, which no run here had), and a one-hour run.

## Shaders compiled at build time (2026-11-18)

Before, every built-in HLSL shader was compiled with FXC (`D3DCompile`) when the engine was created (`Engine::build`) and again when the GPU device was rebuilt after a device loss (`Engine::rebuild_gpu`): `ShaderCache::warm_up`, `fx::warm_up` and `meter::warm_up` each compile what they warm. Now `build.rs` compiles every `src/gpu/shaders/*.hlsl` (same entry points, `vs_5_0` / `ps_5_0`, `ENABLE_STRICTNESS | OPTIMIZATION_LEVEL3`) and embeds the bytecode (`gpu/precompiled.rs`); the warm-ups only create the shader objects. Extension shaders (WGSL from the user) still compile at run time.

Method: the ignored test `gpu::pipeline_tests::shader_setup_time` (release build, hardware adapter RTX 5080, one process, 3 rounds; the "run time" rows switch the embedded table off for the thread, which is the old path). Time of `Renderer::new` plus the three warm-ups, which is the whole shader share of both engine creation and the GPU rebuild (the rebuild also pays for `GpuDevice::new`, about 94 ms, unchanged, and `Assets::new`).

| | before (run-time FXC) | after (embedded) |
|---|---|---|
| shader warm-up (all three warm-ups), 3 rounds | 613 / 601 / 609 ms | 1.5 / 1.0 / 1.0 ms |
| `Renderer::new` | 1.4-1.8 ms | 0.34-0.44 ms |
| engine creation, shader share | about 0.61 s | about 1.4 ms |
| GPU rebuild, shader share (device 94 ms on top, unchanged) | about 0.61 s | about 1.4 ms |

Tests: `built_in_shaders_are_not_compiled_at_run_time` (no run-time compile during all three warm-ups; every table entry is used) and `precompiled_bytecode_equals_a_run_time_compile` (each embedded blob is byte-identical to a run-time compile of the same source, so the pixels are the same). Cost at build time: the build script run is 1.7 s (cargo `--timings`; it includes the FXC compile of all 33 programs (32 files) and `tauri-build`), only when an `.hlsl` or `build.rs` changes.


## sRGB views and NV12 in one pass (2026-11-18)

Two changes for weak GPUs (GTX 1650, Iris Xe: the plan's 4K figures are estimated to be missed there). This PC has an RTX 5080, so the numbers below show the saving as a ratio, not the time a weak GPU would take.

### What changed

1. **The pool textures are `R8G8B8A8_TYPELESS` with four views** (`gpu::RgbaTexture`): `srv` / `rtv` as `UNORM` (the codes, as before: everything outside the filters keeps using them) and `srv_srgb` / `rtv_srgb` as `UNORM_SRGB`. A pass that works in linear light binds the `_SRGB` views (`gpu::Views`, `gpu::Space`): the texture unit decodes on read (`Load` too, checked), the output merger encodes on write, and the shader has no `pow` for either. This is what OBS does (libobs makes its `GS_RGBA` textures typeless, with a linear view for `gs_effect_set_texture_srgb` and for `gs_enable_framebuffer_srgb`; from memory, not re-read). The mask picture asset is typeless too and carries a linear view (`ImageAsset::srv_linear`).
2. **NV12 is written by one compute dispatch** (`encode_nv12_cs.hlsl`, `cs_5_0`, compiled by `build.rs`) when the width is a multiple of 4 and the height is even; any other even size keeps the two draws.

### Choice per filter and pass

"codes" = the plain `UNORM` views, "linear" = the `_SRGB` views. "Saved" counts the per-pixel `pow` conversions that went away.

| Filter / pass | Reads | Writes | Why | Saved per pixel |
|---|---|---|---|---|
| Color Correction | linear | linear | OBS runs gamma, matrix, multiply and add in linear light | 1 decode + 1 encode |
| Sharpen | linear | linear | kernel in linear light, premultiplied (OBS) | 9 decodes + 1 encode |
| Chroma Key | linear + codes (centre texel) | linear | the distance is measured on the encoded colour, the rest is linear; the four box-filter means are still encoded by hand | 9 decodes + 2 encodes (4 encodes left) |
| Color Key | linear + codes | linear | distance on the code, gamma/contrast in linear light | 1 decode + 2 encodes |
| Luma Key | linear + codes | codes | luminance of the linear colour; the colour goes out untouched | 1 decode |
| Apply LUT 3D and 1D | linear + codes | linear | table addressed by the code, blend in linear light | 1 decode + 1 encode (mode 0 with alpha below 1 keeps one encode for the coordinate) |
| Image Mask/Blend (5 types) | linear picture, linear mask asset | linear | OBS binds both as sRGB | 5 decodes + 1 encode |
| Scaling: point | codes | codes | a raw texel copy, every code must survive | 0 (none was spent) |
| Scaling: bilinear, low-res bilinear, bicubic, lanczos, area | linear | linear | OBS filters in linear light on premultiplied alpha | 1 decode per tap (4 to 36) + 1 encode |
| Transform: flips and quarter turns | codes | codes | raw copy | 0 |
| Transform: other angles | linear | linear | bilinear in linear light | 4 decodes + 1 encode |
| Crop/Pad | codes | codes | raw copy | 0 |
| Auto Adjust, render pass | linear | linear when contrast and saturation are both 1 (the default), else codes | contrast and saturation are defined on the encoded colour, so with either on the shader still encodes by hand | 1 decode (+ 1 encode by default) |
| Auto Adjust, statistics chain (`StatsChain`) | codes | | left as it was: its first stage filters bilinearly in gamma space and decodes the result; the `_SRGB` view would filter in linear light, which changes the measurements | 0 |
| NVIDIA Background Removal, final pass (`nv_mask`) | codes | codes | only alpha is scaled | 0 |
| NVIDIA Blur / Background Blur, final pass (`nv_blend`) | picture linear, blurred picture codes | linear | the blurred picture is NVIDIA's own typed `R8G8B8A8_UNORM` texture (the SDK wants a typed format; a typeless one was not tried), so that decode stays in the shader | 1 decode + 1 encode (1 decode left) |
| Output stage (`output_fit`) | codes | codes | opaque pixels are copied or filtered bilinearly in gamma space (unchanged); a translucent pixel is composited in linear light | the background's decode is made once on the CPU instead of per translucent pixel |
| Convert (all formats), encode BGRX, preview downscale, meter | codes | codes | not linear-light work | 0 |

### NV12 in one pass: why a compute shader

Two render targets of one draw must have the same width and height; luma is W x H and chroma W/2 x H/2, so MRT cannot write both planes. The options on D3D11 feature level 11.0:

* *Pixel shader with a UAV for chroma* (luma as the render target): needs a typed UAV store to `R8G8_UNORM`, which feature level 11.0 does not guarantee (only the 32 bit single channel formats are mandatory), so it would need a capability query and a fallback.
* *Compute shader writing `R32_UINT` planes* (chosen): compute shader 5.0, UAVs and `R32_UINT` typed stores are all mandatory at 11.0, so it needs no query and runs on Intel, NVIDIA and AMD alike (not tried on the latter two). A thread owns 4 x 2 pixels, reads each once, and writes two luma texels and one chroma texel (four bytes of the plane each; the bytes in memory are the rows of the R8 and the R8G8 plane). The staging copy and `MappedPlane` rows are therefore the same bytes as before: `output_writer.rs` needed no change (`plane_descs()` / `textures()` give the new descriptors, the row is `W` bytes in both planes).
* Not done: luma at full size and chroma from quad derivatives in the same draw (still two render targets of different sizes).

The compute version needs width % 4 == 0 and an even height (a thread's block); every other even size falls back to the two draws. The rounding to a byte is done by hand (`saturate(v) * 255 + 0.5`), where the render target rounds in the output merger: on the RTX the two differ by one code in about 1.4 % of the luma bytes (64x32 test), never more; on WARP the bytes are identical.

### GPU time, before and after

Method: the ignored test `fx::perf_tests::filter_gpu_times` (timestamp queries around the commands; median of 100 frames on the hardware adapter, then the median of 5 runs; "after" is this working tree, "before" is a build of commit `ce45acc` with only the test files added; the two were run in turns). One filter alone: the cost of the chain minus the copy that feeds it. Input: a 16:9 picture with texture and a translucent corner. The last rows are whole stages. RTX 5080, driver 621.01, other agents working on the PC. Milliseconds of GPU time.

| Case | 1920x1080 before | 1920x1080 after | change | 3840x2160 before | 3840x2160 after | change |
|---|---|---|---|---|---|---|
| color_filter | 0.014 | 0.008 | -43% | 0.051 | 0.042 | -18% |
| sharpness_filter 0.35 | 0.039 | 0.021 | -46% | 0.153 | 0.080 | -48% |
| chroma_key_filter | 0.062 | 0.029 | -53% | 0.243 | 0.115 | -53% |
| color_key_filter | 0.016 | 0.009 | -44% | 0.061 | 0.031 | -49% |
| luma_key_filter | 0.009 | 0.008 | -11% | 0.032 | 0.028 | -12% |
| scale point (2/3) | 0.005 | 0.005 | +0% | 0.014 | 0.014 | +0% |
| scale bilinear (2/3) | 0.012 | 0.006 | -50% | 0.039 | 0.018 | -54% |
| scale bicubic (2/3) | 0.031 | 0.018 | -42% | 0.117 | 0.064 | -45% |
| scale lanczos (2/3) | 0.074 | 0.038 | -49% | 0.285 | 0.144 | -49% |
| scale area (2/3) | 0.021 | 0.013 | -38% | 0.079 | 0.048 | -39% |
| scale bilinear low-res (1/2 and less) | 0.008 | 0.005 | -38% | 0.023 | 0.011 | -52% |
| transform 30 deg | 0.035 | 0.023 | -34% | 0.131 | 0.088 | -33% |
| transform flip_h | 0.008 | 0.009 | +12% | 0.028 | 0.028 | +0% |
| crop | 0.009 | 0.009 | +0% | 0.028 | 0.028 | +0% |
| auto_adjust (exposure + wb) | 0.033 | 0.029 | -12% | 0.085 | 0.060 | -29% |
| four filters (color, sharpen, scale bicubic 2/3, chroma key) | 0.112 | 0.060 | -46% | 0.431 | 0.226 | -48% |
| output stage (same size) + encode NV12 | 0.018 | 0.014 | -22% | 0.061 | 0.042 | -31% |
| encode NV12 alone | 0.010 | 0.006 | -40% | 0.034 | 0.014 | -59% |
| encode BGRX alone | 0.008 | 0.008 | +0% | 0.027 | 0.027 | +0% |
| convert NV12 + output + encode (no filters) | 0.102 | 0.100 | -2% | 0.327 | 0.309 | -6% |
| convert NV12 + four filters + output + encode | 0.213 | 0.159 | -25% | 0.754 | 0.527 | -30% |

The 5080 is far from ALU-bound at these sizes, so absolute times are tiny; the ratios are what carry over (the saving is ALU: texture reads and render target writes cost what they did). Rows at 0 % are paths that did not change (raw copies, crop, BGRX). The "convert" rows include the upload of the NV12 frame. `transform flip_h` at 1080p (+12 %) is one microsecond of noise.

**WARP** (the software rasterizer: CPU time of the same commands; median of 5 runs at 1080p and 3 at 4K) as a proxy for a GPU that is bound by ALU. Treat it as an upper bound, not an estimate, and the machine was busy: the point copy and the flip are the same raw texel copy before and after, yet are 40 to 80 % faster here, which is what a rasterizer that also pays for the untaken branches of the shader they share (the other scaling modes with their `pow`) would show; on the RTX they did not move. This explanation was not checked.

| Case | 1920x1080 before | 1920x1080 after | change | 3840x2160 before | 3840x2160 after | change |
|---|---|---|---|---|---|---|
| color_filter | 6.497 | 3.291 | -49% | 25.405 | 14.144 | -44% |
| sharpness_filter 0.35 | 22.484 | 4.450 | -80% | 89.338 | 21.218 | -76% |
| chroma_key_filter | 39.397 | 15.900 | -60% | 150.103 | 84.660 | -44% |
| color_key_filter | 7.499 | 3.228 | -57% | 34.610 | 20.367 | -41% |
| luma_key_filter | 3.281 | 1.226 | -63% | 13.559 | 4.634 | -66% |
| scale point (2/3) | 33.664 | 6.513 | -81% | 143.303 | 29.445 | -79% |
| scale bilinear (2/3) | 39.420 | 8.174 | -79% | 180.969 | 33.453 | -82% |
| scale bicubic (2/3) | 16.622 | 4.058 | -76% | 86.922 | 17.784 | -80% |
| scale lanczos (2/3) | 44.147 | 13.546 | -69% | 184.026 | 66.266 | -64% |
| scale area (2/3) | 12.672 | 5.475 | -57% | 59.253 | 23.693 | -60% |
| scale bilinear low-res (1/2 and less) | 23.799 | 4.671 | -80% | 93.310 | 19.485 | -79% |
| transform 30 deg | 16.644 | 6.706 | -60% | 68.968 | 27.434 | -60% |
| transform flip_h | 4.051 | 2.411 | -40% | 16.805 | 9.341 | -44% |
| crop | 0.952 | 0.976 | +3% | 3.861 | 3.446 | -11% |
| auto_adjust (exposure + wb) | 8.831 | 7.242 | -18% | 34.774 | 29.564 | -15% |
| four filters (color, sharpen, scale bicubic 2/3, chroma key) | 66.656 | 20.330 | -70% | 279.998 | 78.213 | -72% |
| output stage (same size) + encode NV12 | 7.660 | 6.479 | -15% | 31.649 | 23.992 | -24% |
| encode NV12 alone | 1.041 | 0.811 | -22% | 3.832 | 2.786 | -27% |
| encode BGRX alone | 0.714 | 0.723 | +1% | 2.352 | 2.086 | -11% |
| convert NV12 + output + encode (no filters) | 8.939 | 7.544 | -16% | 39.228 | 29.331 | -25% |
| convert NV12 + four filters + output + encode | 83.619 | 29.203 | -65% | 327.172 | 129.487 | -60% |

### Accuracy

* **WARP** (exact conversions): every case of the golden file differs from the old output by at most **1 code**, most by 0 (`fx::golden_tests`: 35 cases, every filter that needs no asset file or NVIDIA runtime, plus the output stage and both encoders; the golden file was made with the old shaders). The LUT and mask filters and the NVIDIA final passes are covered by their own tests against a CPU reference (all pass).
* **RTX 5080:** NVIDIA's texture unit returns a decoded texel with about 7 bits of mantissa (the codes 248 to 252 decode to 0.9375, 0.9453, 0.9531, 0.9609 and 0.9766, against the exact 0.9387, 0.9473, 0.9560, 0.9647 and 0.9734: up to 0.0037 off in linear light, a third of a code once it is encoded again; checked in `pool_textures_read_linear_and_write_encoded_through_their_srgb_views_hardware`). The encode on write is exact (within 0.6 of a code) and decode followed by encode gives back every code. OBS on NVIDIA behaves the same way, since it uses the same hardware path. A filter that amplifies the decoded values shows it. Worst difference from the golden file on this GPU, old shaders / new shaders: Color Correction default 0 / 0, with every setting 1 / **4**; Sharpen 0.08 (the default) 1 / 1, Sharpen 0.9 1 / **13** (6 % of the bytes more than 1 code off, on a picture of random noise of +-12 codes); Chroma Key 1 / 1; Color Key 1 / 1; Luma Key with smoothing 1 / **6** (alpha, on a smoothstep edge); Bilinear 1 / 1; Bicubic 1 / **4**; Lanczos 1 / **5** (2 on the smaller size); Area 1 / 1; Transform 1 / 1; output stage 1 / 1. An Intel or AMD part may round differently (not measured: none here). If a strong Sharpen shows banding in highlights on NVIDIA, Sharpen is the filter to put back on the codes with its own `pow` (`Views::ENCODED` and the two conversion functions): its default setting is not affected.
* The `scale area` upscale case of the golden file differs by up to 252 on this GPU in pixels whose alpha is near 0, before and after the change (a division by an alpha near 0: the colour of a nearly clear pixel is meaningless and WARP and the GPU round it differently); the hardware comparison leaves out the colour of pixels with alpha below 64.

### Tests

`gpu::pipeline_tests`: the sRGB views decode on `Load`, encode on write, leave alpha alone and round-trip every code exactly (WARP and hardware); a typeless pool texture copies to and from typed textures; NV12 in one dispatch equals the two draws, and a CPU luma, at nine sizes including the fall-back ones (WARP and hardware); the packed planes read back through `ReadbackRing` with the rows of the NV12 planes. `fx::golden_tests`: old against new output, WARP strict and hardware loose. `fx::nvidia::filter::tests`: the two final NVIDIA passes against their formulas (WARP, no runtime needed). The test `auto_adjust_defaults_give_the_output_of_the_previous_filter` compared bytes with the previous shader; it now allows one code on the colour (alpha stays exact), because the conversion is the hardware's.

### What remains for weak GPUs

* The output stage still draws the picture when it is not scaled (`output_fit` with the rectangle equal to the frame and an opaque picture). Skipping it needs `engine.rs` (hand the chain's last texture to the encoder in that case); its GPU cost is the "output stage + encode" row minus the "encode NV12 alone" row.
* Decoder surfaces (the camera's frame reaching the convert pass without the CPU upload) are a capture / engine task.
* Convert is unchanged (the convert row includes the upload of the NV12 frame: 0.10 ms at 1080p and 0.3 ms at 4K on the 5080).
* Chroma Key still encodes the four means of its box filter by hand (4 `pow` per colour channel); Lanczos still loads 36 texels per pixel. Whether either is now bound by texture reads rather than ALU was not measured.
* Not measured: an Intel or AMD GPU, a GTX 1650, the NVIDIA final passes (one draw each), the engine end to end (`engine_script`) with these changes.
