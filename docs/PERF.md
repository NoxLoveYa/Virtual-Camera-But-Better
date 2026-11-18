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
