# Better Cam Auto — software auto for the camera's own controls

Status: design, no code yet. Date: 2026-11-18. Covers two owner requests: "auto also drives the camera itself" and
"per-control targets in Camera controls". The third request (more targets in the Auto Adjust filter) is built
separately and is only referenced here.

**Owner decision recorded (2026-11-18), reverses part of PLAN 9.3 item 1.** PLAN 9.3 item 1 said "Software never
drives the device controls". The owner now wants software to drive them. New rule: Better Cam may write a camera's
exposure, gain and white balance, but only for a control the user switched to **Better Cam auto**. Nothing changes
for users who never pick it. PLAN.md 9.3, 5.5, 5.6, 6.2 and the header of `capture/controls.rs` ("Software never
computes control values") are updated in phase A6 (this session touches only this file).

Terms used below:

- **Manual**: the user's value, the camera's Auto flag off. Today's behaviour with the Auto switch off.
- **Camera auto**: the camera's own Auto flag on. Today's Auto switch.
- **Better Cam auto**: Better Cam's controller writes the camera's control values. New.
- **Stop**: a factor of 2 in light. Exposure on the Windows API is in whole stops (log2 seconds).
- **Loop**: one controller: the *exposure loop* (exposure + gain) or the *white balance loop*.

---

## 1. Summary

1. Exposure and white balance get a third mode next to Manual and Camera auto: **Better Cam auto**.
2. In Better Cam auto, Better Cam meters the camera picture (before any filter) and writes the camera's exposure,
   gain and white balance itself, a few times a second at most.
3. Exposure never gets longer than one frame time, so the camera keeps the frame rate you asked for. In the owner's
   dim room this means 30 fps (or 60) instead of the 14-17 fps the camera's own auto gives.
4. When exposure is at that limit, gain makes up the rest, up to a **Most gain** limit you set.
5. The cost: gain adds noise. Each halving of exposure time needs one more stop of gain. At 30 fps that is about one
   stop more than the camera's 15 fps auto: visibly grainier shadows (about 1.4-2× the noise).
6. You choose per camera: target brightness, longest exposure (default "fit the frame rate"), most gain, response
   speed, and white balance warmth.
7. When a limit is reached the card says so in words: "Gain at its maximum (70). Add light, allow more gain, or allow
   a longer exposure."
8. It never stays black or white, moves in bounded steps, and gives the control back to Camera auto if the camera
   stops answering or ignores it.
9. The Auto Adjust filter still works on top: Better Cam auto does the coarse work on the sensor, Auto Adjust the fine
   trim and the look.
10. Cameras that cannot do it safely (no manual exposure, unknown units, writes ignored) simply do not offer it.

---

## 2. The design

### 2.1 Recommendation in one paragraph

One controller thread next to the controls hub runs two independent **step-and-settle** loops: exposure (exposure +
gain as one quantity in stops) and white balance (Kelvin, worked in mired). Each loop meters a dedicated cheap GPU
statistics pass on the converted camera picture before the filter chain, waits until a frame captured after its last
write has been measured, then moves once, by a bounded fraction of the error, with hysteresis wide enough for the
coarse UVC steps. Exposure prefers the **longest exposure within the frame time**, then gain. The core is a pure
struct driven by an injected clock, tested against a simulated camera.

### 2.2 Controls and modes

Rule for which controls get Better Cam auto: only controls that set **how much light or which light the sensor
records** (exposure time, analog gain, white balance gains). Controls that reshape the picture after the sensor
(brightness, contrast, gamma, hue, saturation, sharpness) are post-processing in the camera's 8-bit ISP; the GPU does
the same work with more precision and Auto Adjust already has targets for it. Driving them would also move the meter
the exposure loop reads, so two loops would fight.

| Control | W4GS range | Manual | Camera auto | Better Cam auto | Targets and limits in Better Cam auto |
|---|---|---|---|---|---|
| `exposure` | -13..-1 (1/8192 s .. 1/2 s) | yes | yes | **yes** (drives exposure and gain together) | Target brightness; Longest exposure; Most gain; Response |
| `gain` | 0..100 | yes | no (device has none) | **driven by the exposure loop**, no mode of its own | Most gain (set in the exposure row, shown as a marker here) |
| `white_balance` | 2800..6500 K | yes | yes | **yes** | Warmth; Response |
| `brightness` | -64..64 | yes | — | no | — |
| `contrast` | 0..64 | yes | — | no | — |
| `hue` | -40..40 | yes | — | no | — |
| `saturation` | 0..128 | yes | — | no | — |
| `sharpness` | 0..6 | yes | — | no | — |
| `gamma` | 72..500 | yes | — | no | — |
| `backlight_compensation` | 0..2 | yes | — | no (only steers the camera's own metering) | — |
| `powerline_frequency` | 0..2 | yes (select) | — | no; read by the exposure loop for the flicker note | — |
| `focus` | 1..1023 | yes | yes | no (software contrast autofocus is a separate feature) | — |
| `low_light_compensation` | absent on W4GS | yes (switch) | — | no; see 2.9 | — |
| `zoom`, `pan`, `tilt`, `roll`, `iris`, `color_enable` | absent on W4GS | yes | as device | no | — |

Gain on a camera where gain has its own Camera auto flag: when the exposure loop engages it switches that flag off,
the same as exposure's.

### 2.3 When Better Cam auto is offered (per control, per camera)

The backend decides from `ControlInfo` and reports it with each control (5.3). It is offered only if every condition
holds; otherwise the mode is not listed and the reason is in the tooltip.

| Loop | Conditions | Degraded but offered | Not offered, reason text |
|---|---|---|---|
| Exposure | `exposure` present, `can_manual`, range looks like log2 seconds (`min >= -16`, `max <= 5`, `step == 1`), not marked `blocked` for this camera | No `gain`, or gain not manual, or gain range under 0.5 stop: **whole stops only** (see 2.5.6) | "This camera has no manual exposure." / "This camera reports exposure in units Better Cam does not know." / "This camera ignored Better Cam auto. [Try again]" |
| White balance | `white_balance` present, `can_manual`, range looks like Kelvin (`min >= 1500`, `max <= 12000`, `max - min >= 1000`), not `blocked` | — | "This camera has no manual white balance." / "…units Better Cam does not know." / "…ignored Better Cam auto. [Try again]" |

`blocked` is set by the watchdog when a camera ignores writes (2.7) and stored per camera; "Try again" clears it.

A2 note: "gain range under 0.5 stop" is read as a gain control with fewer than 6 steps (`(max − min) / step < 6`):
with `S = 3` stops across the range, that is a finest step coarser than half a stop, too coarse to trim with. The
units of a range say nothing about stops, so this is the one test the device facts allow.

### 2.4 The measurement

**Where.** On the render thread, right after the input convert (YUV/RGB → RGBA8, colour space and range applied) and
**before the filter chain**. The device loop must see what the sensor delivers, not what filters made of it. Crop,
background removal and Auto Adjust do not change the meter.

**How.** A dedicated statistics pass, the same one Auto Adjust uses: `stats_downsample.hlsl` reduces the picture to a
16×16 image of linear-light mean colours with coverage in alpha, copied into a `ReadbackRing` and mapped a frame or
two later without stalling. The `Stats` type and the downsample chain are reused from `fx/auto_adjust.rs` (made
`pub(crate)`, no behaviour change). Auto Adjust's own statistics cannot be reused: they measure that filter's input,
wherever it sits in the chain, and do not exist when the filter is absent.

**When.** Only while at least one control of the open camera is in Better Cam auto and the session is live (not
paused). At most one submit per 66 ms (≈15 Hz): the controller decides at 10 Hz and needs no more. Cost ≈ 0.02 ms GPU
at 1080p (Auto Adjust's stats + shader cost 0.044 ms, PERF.md).

**Sample.** Each mapped measurement becomes a `MeterSample { token, capture_qpc, stats }`. `capture_qpc` is the
frame's capture time. The controller uses it to know whether the frame was exposed **after** its last write landed.

**Exposure error (stops).** From the 256 cells, with Auto Adjust's weights (centre weight `1/(1+1.5 r²)`, coverage,
near-black cells count less, near-white cells count a little):

- `lin_i` = Rec. 709 luma of the cell's linear colour; `y_i = srgb_encode(lin_i)`.
- `w_i = centre_i · cov_i · smoothstep(0.02, 0.08, y_i) · (0.15 + 0.85 · (1 − smoothstep(0.93, 0.99, y_i)))`.
- Predicted brightness for a light factor `k`: `B(k) = Σ w_i · srgb_encode(min(1, k · lin_i)) / Σ w_i`. Hard clip,
  because the sensor clips; no shoulder.
- The error `e = log2 k*` where `B(k*) = T` (T = target brightness, the same sRGB-coded scale as Auto Adjust's
  `target`: 0.45 ≈ 18 % grey). Found by bisection over `log2 k ∈ [−4, 4]`, 24 iterations; clamped to ±4.
  Positive `e`: the picture needs more light.
- `lit = Σ c_i · smoothstep(0.02, 0.08, y_i) / Σ c_i` with `c_i = centre_i · cov_i`; confidence
  `conf = smoothstep(0.03, 0.12, lit)`.
- `clip = Σ c_i · [y_i ≥ 0.97] / Σ c_i`.

**White balance error.** Gray world on the cells Auto Adjust's white balance trusts (not near black, not near white,
not strongly saturated; same smooth weights). `m` = weighted mean linear RGB of those cells.
`conf_wb = smoothstep(0.02, 0.12, Σw / Σc)`. Signal `c = ln(m_B / m_R)` (the blue-red axis; colour temperature moves
along it). Target `c*(warmth)`, warmth in −1..+1 (+1 = warmer). Error `e_wb = c − c*`.

**Warmth scale (A2, constant changed: old → new).** Old: `c* = −0.25 · warmth` (B/R ≈ 0.78 at +1). New: Auto Adjust's
scale (`fx::auto_adjust::white_aim`, commit da34514, which merged first), so "warmth +0.3" is the same colour in the
filter and on the device. The aim is the colour of a blackbody whose temperature is linear in **mired** from 5500 K at
warmth 0 to 4000 K at +1 and 7000 K at −1, relative to 5500 K: `M(w) = 10⁶/5500 + |w| · (10⁶/K_end − 10⁶/5500)` with
`K_end = 4000` (w > 0) or `7000` (w < 0); `aim = blackbody(10⁶/M) / blackbody(5500 K)`, normalised to luma 1. Then
`c*(w) = ln(aim_B / aim_R)`. The device loop calls `white_aim(w, 0)` itself (`meter::warmth_target`), so the two cannot
drift apart.

| warmth | −1 | −0.5 | 0 | +0.3 | +0.5 | +1 |
|---|---|---|---|---|---|---|
| `c*` | +0.380 | +0.192 | 0 | −0.204 | −0.342 | −0.694 |
| B/R | 1.46 | 1.21 | 1.00 | 0.82 | 0.71 | 0.50 |

Why: one scale for the same word (owner decision). The old line was a placeholder, 2.5 times too gentle at +1.

**Why the loop is closed and what that implies.** Unlike Auto Adjust (feed-forward on its own input), every write
changes what the meter sees. Delay from write to measurement: USB write (2-7 ms; 200-700 ms for the first write to
exposure or white balance after an open) + the camera applying it (≈0.1 s measured on W4GS) + capture and readback
(1-2 frames). That is 0.15-0.3 s normally. A continuous controller at frame rate would act on stale measurements and
hunt. Hence step-and-settle: one move, wait for proof, decide again.

### 2.5 The exposure loop (exposure + gain)

#### 2.5.1 Quantities

- `E`: exposure setting, integer, log2 seconds (−5 = 1/32 s). Device limits `E_min..E_max`.
- `G`: gain setting, device units, step `G_step`. Device limits `G_min..G_max`.
- Gain model in stops: `gs(G) = S · (G − G_min) / (G_max − G_min)`; inverse `ginv(s) = G_min + s / S · (G_max − G_min)`,
  rounded to `G_step`. `S` = stops across the whole gain range. Initial `S = 3.0` **(unverified: W4GS gain curve
  unknown; 3 stops ≈ 18 dB is common for UVC sensors)**. Learned online (2.5.7) and stored per camera.
- Total light in stops: `X = E + gs(G)`.
- `F`: the capture frame rate of the session (the opened media type's rate, `SourceInfo.fps`).

#### 2.5.2 Limits

- **Frame-rate rule:** `E_rate = floor(log2(0.95 / F))`. F = 30 → −5 (31.25 ms ≤ 31.7 ms); F = 60 → −6 (15.6 ms);
  F = 25 → −5; F = 15 → −4.
- **Longest exposure** (user): `fit` (default) uses `E_rate`; a whole stop `n` uses `n` (may be longer than the frame
  time: the user accepts a lower rate; the option text says which rate, e.g. "1/16 s (camera drops to ≈ 15 fps)").
- **Learned cap** (2.5.8): `E_learned[F]`, only applies with `fit`.
- `E_cap = clamp(min(longest, E_learned[F] if any), E_min, E_max)`.
- **Most gain** (user): `G_cap`, default `G_min + round(0.75 · (G_max − G_min))` (75 on W4GS). `Gs_cap = gs(G_cap)`.
  Most gain = `G_min` turns gain off (exposure-only behaviour).

#### 2.5.3 Order of preference

Longest exposure up to `E_cap` first, then gain. Not the shortest exposure with more gain: gain multiplies noise, and
short exposures make mains-light banding worse. Motion blur at 1/32 s is acceptable for a webcam; a user who wants
sharper motion picks a shorter Longest exposure. Going darker, gain goes down first, then exposure.

#### 2.5.4 Validity of samples and timing

- A write batch (one or two writes) has `t_done` = when its last write returned.
- A sample is **valid** when `capture_time ≥ t_done + D_apply`, `D_apply = 0.15 s + 1/F`. Samples while paused are
  dropped.
- `ê` = median of the last 3 valid `e` (mean of 2 when only 2 exist). `σ` = standard deviation of valid `e` since the
  last write (at most the last 8).
- The controller ticks every 100 ms. It decides only when: no write is in flight; `now ≥ t_done + gap`; at least 2
  valid samples exist.

Speed presets ("Response"):

| Response | `k` | `step_max` per decision | `gap` between write batches | White balance `k_wb` | WB `ΔM_max` | WB `gap` |
|---|---|---|---|---|---|---|
| Slow | 0.4 | 0.5 stop | 500 ms | 0.3 | 10 mired | 600 ms |
| Normal (default) | 0.7 | 1 stop | 250 ms | 0.5 | 20 mired | 400 ms |
| Fast | 0.9 | 2 stops | 150 ms | 0.7 | 40 mired | 300 ms |

#### 2.5.5 Decision (each tick that may decide)

1. **No signal.** If `conf < 0.2` on every valid sample: start or continue the no-signal timer. After 1.5 s, move
   `ΔX = +1` stop at most once per second (allocation below applies) until confident or at `E_cap` and `G_cap`.
   This is what keeps the picture from staying black after the light goes off at a short exposure. A covered lens
   ends at the upper limits; the highlight guard brings it back within a second of uncovering.
2. **Highlight guard.** If `clip ≥ 0.5`: `ΔX = −step_max`, skip the band test. Repeats each decision until `clip`
   falls or exposure and gain are at their floors. The picture cannot stay white unless the camera has no shorter
   setting.
3. **Quantum `q`.** The smallest step that can move in the direction of `ê`: one gain step in stops
   (`S · G_step / (G_max − G_min)`) when gain has room that way, else 1 stop when exposure has room that way. When
   nothing has room: saturated (2.5.9), no write.
4. **Hysteresis band.** State `holding`: act only if `|ê| > B_out = max(0.33, 3σ, 0.6·q)`. State `adjusting`: act if
   `|ê| > B_in = max(0.12, 1.5σ, 0.5·q)`; when `|ê| ≤ B_in` go to `holding`. `B_out` adapts to measured noise and to
   mains flicker beating with the frame rate.
5. **Move.** `ΔX = clamp(k · ê, −step_max, +step_max)`; if `|ΔX| < q`, `ΔX = sign(ê) · q`.
6. **Allocate** `X' = E + gs(G) + ΔX` into `(E', G')` (2.5.6).
7. **Write.** Skip unchanged values. With two writes, write first the one that darkens the picture (an interim dark
   frame is less visible than a flash), then the other at once. The batch ends when both returned.

#### 2.5.6 Allocation with crossover hysteresis

Exposure moves in whole stops; gain fills the fraction. Thresholds: `R_hi = min(1.25, Gs_cap)`,
`R_land = min(0.25, R_hi / 5)`.

```
r = X' − E                                  # stops gain would have to give with the current exposure
if E > E_cap:                    E' = E_cap  # constraint (frame rate or user limit changed)
elif r > R_hi and E < E_cap:     E' = min(E_cap, floor(X' − R_land))
elif r < 0    and E > E_min:     E' = max(E_min, floor(X' − R_land))
else:                            E' = E
r' = X' − E'
G' = ginv(clamp(r', 0, Gs_cap))             # rounded to G_step
```

After a move up, gain lands at `R_land..R_land+1` stops; the move back down needs `r < 0`. That is 0.25 stop of
crossover hysteresis on top of the error band, so a scene sitting near a stop boundary does not toggle the exposure.
A crossover (exposure ±1, gain ∓1 stop) is brightness-neutral apart from the 1-3 frames between the two writes.

**Whole stops only** (no usable gain, or `Most gain = G_min`): when `|ê| > B_out` (with `q = 1`, so `B_out ≥ 0.6`),
`E' = clamp(E + sign(ê) · max(1, min(step_max, round(k·|ê|))), E_min, E_cap)`. The residual of up to ±0.6 stop is
left to Auto Adjust; the card recommends adding it (4.3).

#### 2.5.7 Learning the gain curve

After a batch that changed only `G` by `|Δgs| ≥ 0.2` stop, with `conf ≥ 0.8` and `clip < 0.05` before and after:
observed `o = ê_before − ê_after`; ratio `ρ = o / Δgs`. If `0.25 ≤ ρ ≤ 4`: `S ← S · clamp(ρ, 0.7, 1.4)^0.3`, then
`S ∈ [0.5, 6]`. Small, bounded updates: a scene change during one measurement cannot wreck the model. The
step-and-settle loop with `k ≤ 0.9` converges even when `S` is wrong by a factor 2 (loop gain 0.35-1.8; the band stops
it before a second overshoot). `S` is stored per camera (`learned.gain_stops`).

The camera's tone curve (its `gamma` control) also bends the stop-to-brightness relation. The meter works on sRGB-
decoded values, so the error is exact only for an sRGB-like camera curve; the same tolerance covers it.

#### 2.5.8 Frame-rate guard (learned cap)

With Longest exposure = `fit`, `E == E_cap`, no write in the last 1 s, and the 2-second capture rate `< 0.9 · F` for
3 s: set `E_learned[F] = E_cap − 1` (not below `E_min`); the next decision applies it as a constraint. If the rate
recovers to `≥ 0.95 · F` within 3 s, keep it (stored per camera and frame rate). If not, the cause is not exposure:
remove it, and do not learn again in this session. Status text: "The camera dropped to 24 fps at 1/32 s, so Better
Cam keeps exposure at 1/64 s or shorter."

#### 2.5.9 Saturation (limits reached)

- `E' == E_cap`, `G' == G_cap` and `ê > B_out` for 3 s: state `limit_gain` (or `limit_exposure` on whole-stops
  cameras).
- `E' == E_min`, `G' == G_min` and `ê < −B_out` for 3 s: state `too_bright`.
- Saturation is not a fault. The loop stays engaged at the limit (it is the best picture the limits allow) and shows
  the text (4.2). It leaves the state when `|ê| ≤ B_in` or a limit changes.

#### 2.5.10 Mains flicker

Under 50 Hz mains, lamps flicker at 100 Hz (10 ms); under 60 Hz, at 120 Hz. Whole-stop exposures (1/32 s = 31.25 ms,
1/64 s = 15.6 ms) are never whole multiples of these periods, so the controller cannot remove banding or frame-to-frame
beat; finer exposure units would need the extended exposure control (8, question 9). What it does: it prefers the
longest exposure (fewer bands, weaker beat), the noise-adaptive band (`3σ`) keeps the beat from driving writes, and
the median of 3 rejects single outliers. The card shows "Short exposures under mains light can show stripes" when
`E < log2(1 / (2 · mains))` and powerline frequency is 50 or 60 Hz.

#### 2.5.11 Engagement and start

On engagement (user picks Better Cam auto, or a camera open with the mode stored):

1. Remember `prior` = the control's mode and value before engagement (for hand-back, 2.7).
2. Seed: `E0 = min(E_cap, resume.exposure ?? device default)`, `G0 = resume.gain ?? G_min`. The device's reported
   exposure is not used while Camera auto is on: W4GS then reports a stale manual value.
3. Write `exposure = E0` (manual; this is the slow 200-700 ms first write), then `gain = G0`. On open these are part of
   `apply_on_open` (the stored resume values), so the loop's first move never waits on a slow first write.
4. Start in `adjusting` with no valid samples; the normal validity rule makes it wait for proof.

The resume point (`E`, `G`, Kelvin) is stored when the loop enters `holding`, at most once per 10 s. The camera also
keeps values across a reopen (measured on W4GS), so a reopen shows no flash.

### 2.6 The white balance loop

- Device setting `K` (Kelvin), worked as mired `M = 10⁶ / K`. Model: `c ≈ α · (M − M_light)`, `α > 0`: a setting
  warmer than the light (higher K, lower M) leaves the picture orange (`c < 0`). Initial `α = 0.006` per mired
  **(unverified)**, learned like `S` (ρ = observed Δc / (α·ΔM), same bounds and exponent), `α ∈ [0.002, 0.02]`, stored
  as `learned.wb_alpha`.
- Samples valid when `capture_time ≥ t_done + 0.3 s`. `ê_wb` = median of last 3 valid `e_wb`; `σ_wb` as above.
- Hold when `conf_wb < 0.3` (a scene of one colour, a dark scene).
- Band: `B_out = max(0.04, 3σ_wb)`, `B_in = max(0.015, 1.5σ_wb)`, same holding/adjusting states.
- Move: `ΔM = clamp(−k_wb · ê_wb / α, −ΔM_max, +ΔM_max)`; `K' = round_to_step(10⁶ / (M + ΔM))`, clamped to
  `K_min..K_max`; skip if `|K' − K| < step`.
- White balance never writes in the same 0.3 s as an exposure batch, and waits while the exposure loop is
  `adjusting` with `|ê| > 1` stop: confounded measurements would teach `α` wrong and gray world is unreliable in a
  badly exposed picture. Exposure has priority.
- Saturation: `K` at `K_min` or `K_max` and `|ê_wb| > B_out` for 3 s → `at_limit`.
- Tint (green-magenta) has no device control; it stays with Auto Adjust.
- **Learning `α` (A2 note, constant added).** The doc says "like `S`" but `S` learns from moves of at least 0.2 stop;
  in `ln(B/R)` that is 0.14, which a 20 mired step (0.12 at the initial `α`) never reaches. The loop learns from
  batches whose predicted change `|α·ΔM|` is at least **0.06** (10 mired at the initial `α`), once per batch, with
  `conf_wb ≥ 0.8` before and after, and only when no exposure write came between the batch and its measurement.

### 2.7 Watchdog

Checked on every write result and every tick. "Hand back" means: stop the loop for that control and set the control
to **Camera auto** if the camera has it (the safest picture), else to the `prior` manual value; gain goes to its
`prior` value. The record and the card follow; a toast and the row's status explain why.

| # | Trigger | Action | Status |
|---|---|---|---|
| W1 | A write or read fails with a non-fatal error: retry once after 0.5 s; 3 consecutive failures, or one write taking > 3 s | Stop the loop, try hand-back once | `not_answering`: "The camera stopped answering. Exposure went back to Camera auto." |
| W1b | Fatal device error (gone) | Nothing; the engine's reconnect path runs; the loop restarts on reopen | — |
| W2 | Read-back after a write differs from the requested value (after clamping to range and step) on 3 consecutive writes | Hand back; store `blocked` for this control and camera | `ignored`: "The camera ignored Better Cam auto for Exposure. [Try again]" |
| W3 | 3 consecutive batches with predicted change ≥ 0.5 stop (WB: ≥ 30 mired), each followed by ≥ 0.5 s of valid confident unclipped samples, show an observed change < 15 % of the prediction | Hand back | `no_response`: "The picture does not change when Better Cam changes Exposure. Back to Camera auto." |
| W4 | ≥ 4 direction reversals of applied moves within 10 s | Damp for 30 s: `k / 2`, bands × 1.5. If it happens again while damped: hand back | `hunting` (only on hand-back): "Exposure kept swinging. Back to Camera auto." |
| W5 | No valid sample for 2 s while live and not paused (GPU loss, frames stalled) | Hold; no writes | `no_picture` (no message unless > 10 s) |
| W6 | The 1 Hz read-back (when no write in the last 1 s) finds the Auto flag on or a value ≠ the last written | Re-take (write ours); count. ≥ 3 re-takes within 60 s: stop the loop, keep the mode stored, wait for the user | `conflict`: "Another program keeps changing Exposure. Better Cam auto paused. [Resume]" |

Saturation never hands back (2.5.9). The step bound (`step_max`, `ΔM_max`), the no-signal drift and the highlight
guard (2.5.5) are the guarantees against black or white.

**Notes from A2 (how the table is built; one constant changed).**

- **W3 white balance threshold, 30 → 15 mired (constant changed).** Normal's `ΔM_max` is 20 mired and Slow's 10, so
  "predicted change ≥ 30 mired" could only ever fire at Fast. 15 judges every Normal batch.
- **W3 only judges what the meter can see.** A batch is judged when the decision that made it and the picture now
  are confident (`conf ≥ 0.5`), unclipped (`clip < 0.05`) and inside the meter's range (`|ê| < 3.9`; at ±4 stops the
  meter only says "at least"). Found by case 11: a lamp 10 stops too dark reads +4 before and after a one-stop step.
  If the picture has not moved by 15 % yet, the loop waits (up to 3 s after the write) until 0.5 s of such samples
  back it up; only then is it a strike. A batch that answered ends the strikes.
- **W5** counts from the newest sample's arrival (or the loop's start / resume), not its capture time, so a long
  exposure (late frames) is not "no picture".
- **W6** replaces the decision of a tick once a second (the read-back runs alone); a re-take is a write of our value.
  Two mismatches within 0.5 s (exposure and gain of the same meddling) are one event.
- **Halts.** A fault ends the loop and keeps its status (state, text, `handed_back`) until the control's mode is
  something other than the one it was handed to; a conflict keeps the mode (Better Cam auto) and waits for `resume`.
  Hand-back targets: Camera auto where the camera has it (exposure, white balance), else the prior manual value
  (the camera default if none was recorded); gain goes to its prior value (its Camera auto flag if that was on, 2.9).

### 2.8 Where it runs

- **Controller thread** `controls auto`, spawned with the poller by the engine and living as long as the engine. It
  idles (blocks on its mailbox, 100 ms timeout) when no camera is attached or nothing is in Better Cam auto. Never the
  UI thread, never the render thread.
- **Inputs:** `MeterSample`s through a one-slot mailbox (`Mutex<Option<MeterSample>> + Condvar`; the newest wins);
  session facts read from `Shared` (requested fps `F`, capture fps over 2 s, paused, hub token).
- **Outputs:** writes through the hub (`ControlsHub::drive`, 5.3) under the existing write gate, so user writes,
  re-apply and the loop are ordered; `controls-changed` (values, ≤ 4 Hz) and `auto-status` events.
- **Rates:** decisions at 10 Hz; writes rate-limited by `gap` (≤ 4 batches/s exposure at Normal, ≤ 2.5/s WB); one
  write in flight at a time across both loops; read-back for W6 at 1 Hz.
- **Core is pure:** `AutoController` has no threads, no device, no clock of its own: `tick(now, facts) -> Vec<Action>`,
  `on_sample`, `on_write_done`, `on_readback`. A thin runner executes actions. Tests drive the core with a simulated
  camera and an injected clock (section 6).

### 2.9 Cameras that differ from W4GS

| Camera trait | Behaviour |
|---|---|
| No gain control, or gain not manual | Exposure loop in whole stops (2.5.6). Hint in the row: "No gain control: Better Cam auto works in whole stops. Add Auto Adjust for the fine trim." |
| Gain has its own Camera auto flag | Switched off on engagement; restored to `prior` on hand-back or Manual. |
| No manual exposure | Exposure Better Cam auto not offered (2.3). |
| Exposure in other units (range outside log2 plausibility) | Not offered in v1; units logged once (`[auto] exposure range a..b step s: units unknown`). |
| Narrow exposure range (e.g. −7..−4) | Works; the limits clamp; saturation texts appear sooner. |
| Rejects writes while streaming | W1 (errors) or W2 (ignored) → hand back; W2 stores `blocked`, so the mode is not offered until "Try again". |
| Has auto-exposure priority (`low_light_compensation`) | Better Cam auto still offered (it adds target, most gain and the frame-rate cap). The exposure row adds a hint: "This camera can also hold the frame rate itself: Camera auto with Low light compensation off." Better Cam never toggles that switch. |
| Slow writes (median > 100 ms after the first) | `gap = max(gap, 2 × median write time)`; D_apply grows by the write time. Median > 1 s: hand back with "This camera is too slow to adjust live." |
| Camera reports Auto flags it does not honour | W3 catches it. |
| White balance in non-Kelvin units, or no manual WB | WB Better Cam auto not offered. |

### 2.10 Options considered and rejected

| Option | Why rejected |
|---|---|
| Continuous PI controller at frame rate | Closed loop with 0.15-0.3 s delay and whole-stop actuators: limit cycles and constant USB writes. Step-and-settle is simpler and provably stops. |
| Shortest exposure first, then gain | More noise for the same brightness, stronger mains banding. Longest-first is the standard AE program. |
| Drive brightness / contrast / gamma as actuators | Post-sensor 8-bit processing: no new information, coarser than the GPU, and it moves the meter. Auto Adjust covers it. |
| Reuse Auto Adjust's statistics | Measures that filter's input (after any earlier filter), absent when the filter is off. A pre-chain pass with the same code costs ≈ 0.02 ms. |
| Meter after the chain (what viewers see) | Filters would enter the loop (Auto Adjust on top of it would cancel every move it makes). |
| One-shot model inversion (set E, G from one measurement) | Needs an accurate gain curve and an unclipped picture; both unknown. Used only as the bounded `k · ê` step. |
| Camera auto with an exposure upper limit | UVC has no standard AE-limit control (vendor extension units only). |
| Run the loop on the poller thread | The poller blocks on reads of every Auto control; decisions would wait. Separate thread, shared gate. |
| Run the loop in the frontend | UI thread rule; timing jitter; would stop when the window is hidden. |
| Feed-forward compensation into Auto Adjust in v1 | Needs the exact frame a write lands on, which is not known. Deferred; the live test decides (8, question 8). |

---

## 3. Interplay

| Situation | Behaviour |
|---|---|
| User picks Better Cam auto for exposure while Camera auto is on | Backend writes exposure manual at the seed `E0` (Camera auto goes off), then gain `G0`. The selector shows Better Cam auto. No toast: the row's status says "Adjusting…". |
| User picks Better Cam auto for white balance while Camera auto is on | Same: manual write of the resume Kelvin (or the device's reported value), then the loop. |
| User picks Camera auto on a control in Better Cam auto | Loop stops for it before its next write (the gate orders it). Exposure: gain stays at its last driven value as Manual. |
| User picks Manual on a control in Better Cam auto | Loop stops; the last driven values become the manual values. No jump in the picture. |
| User drags the gain slider while exposure is in Better Cam auto | Not possible: the slider is disabled and says "Set by Better Cam auto exposure". To fix gain, lower Most gain or leave Better Cam auto. |
| User changes brightness, contrast, gamma or backlight compensation while a loop runs | Allowed. The exposure loop sees the metered change and re-adjusts (one transient). Hint under the exposure row: "Brightness, contrast and gamma change the picture after the sensor. Set them once and leave them." |
| Auto Adjust in the chain with its exposure on, plus Better Cam auto exposure | Both run. Device loop = coarse (sensor light within ±0.33 stop, or ±0.6 on whole-stop cameras); Auto Adjust = fine trim and final look (it measures its own input, so it never fights the device loop; it follows each device step with its 0.5 s time constant). Auto Adjust's target decides the final brightness. **Recommended default for the owner:** Exposure = Better Cam auto, White balance = Better Cam auto, Auto Adjust in the chain with exposure and white balance on. |
| Auto Adjust white balance plus device Better Cam auto white balance | Both run. The device loop brings neutrals near neutral at the sensor (better colour depth); Auto Adjust removes the residual and applies tint. Warmth: Auto Adjust's warmth target wins (it measures its input). Leave the device warmth at 0 when Auto Adjust white balance is on (hint shown when both are non-zero). |
| Auto Adjust only, no Better Cam auto | As today. |
| Filter chain edits (crop, scale, background removal) | No effect on metering (pre-chain). |
| Presets load or save | Modes, targets and learned values are not in presets (owner decision 9.3 item 4 stays: presets hold the chain only). |
| Pause | Camera stays open; no samples; the loop holds (no writes, no starvation status). On resume, samples before the resume time are invalid; the loop continues normally. |
| Standby / Stop (camera closed) | Loop idle. In memory: `S`, `α`, learned caps, prior values. Stored: modes, targets, resume point, learned values. |
| Camera opens (start, standby → live, reconnect) | `apply_on_open` writes the stored values, including the resume point of Better Cam auto controls as manual values (Auto flags first, then values, as today). The controller starts only after the apply finished (hub signals it per token). |
| Reconnect at another USB port | The record follows the camera (existing vid/pid + name match); learned values kept. |
| Another camera selected | That camera's record, modes and targets. The first camera's loop stops when it closes. |
| Format change (resolution, fps, video format) | Camera reopens. New `F`, new `E_rate`. If `E > E_cap`, the first decision moves exposure to the cap with gain making up (allowed past `step_max`: it is a constraint, not a correction). Learned caps are per `F`. |
| Another program changes a driven control | W6: re-take up to 3 times per minute, then pause with "[Resume]". |
| Camera held by another app / Reconnecting | Loop idle; nothing to write. |
| Reset all | Every control back to camera defaults (Camera auto where it exists): every Better Cam auto mode ends. Targets and learned values are kept for next time. |
| GPU device lost | Meter resources rebuilt with the chain; W5 holds meanwhile. |
| Window hidden, preview off | Loop continues (the meter does not depend on the preview). |
| App closed | The camera keeps the last driven values (W4GS does). Next start re-applies the record and resumes. |
| Camera closed, card open (lease) | No loop (no frames). Rows show the mode and the stored resume value, "Starts when the camera opens." |
| Powerline frequency changed | No loop action; only the stripes note (2.5.10). |

---

## 4. UI

### 4.1 Camera controls card (text sketch)

Controls with three modes replace the Auto switch with the existing `Select` component (same row height, same look as
the powerline select). Controls with only Manual / Camera auto keep the Auto switch. Better Cam auto settings unfold
under the row as an indented sub-block, only while that mode is selected.

```
Camera controls                                                            [⟲ Reset all]
─────────────────────────────────────────────────────────────────────────────────────────
Brightness                                   0
[────────────────●────────────────]
…
White balance                           4650 K                  [Better Cam auto ▾]
[───────────●·····················]        (disabled, live value)
    Warmth            cooler [──────●──────] warmer        0
    Response          [Normal ▾]
    ● Holding · 4650 K
Gain                                        48       Set by Better Cam auto exposure
[══════════●──────────────│▒▒▒▒▒▒▒]        (disabled; │ = Most gain 75, shaded beyond)
Powerline frequency                                               [50 Hz ▾]
Exposure                                1/32 s                  [Better Cam auto ▾]
[═════════════════●│▒▒▒▒▒▒▒▒▒▒▒▒▒▒]        (disabled; │ = 30 fps limit, shaded beyond)
    Target brightness [─────────●─────────]                      0.45
    Longest exposure  [Fit the frame rate (1/32 s at 30 fps) ▾]
    Most gain         [───────────────●───]                      75
    Response          [Normal ▾]
    ● Holding 30 fps · gain 48 of 75
Focus                                         512                      Auto (○)
[──────────●──────────────────────]
```

- Mode select options: "Manual", "Camera auto", "Better Cam auto". A mode the camera lacks is not listed; a Better Cam
  auto that is not offered is listed disabled with the reason as its tooltip (2.3), so users learn it exists.
- While a control is in Better Cam auto: value text and slider show the live value (from `controls-changed`, ≤ 4 Hz);
  the slider is disabled, like Camera auto today. Limit markers: the exposure slider shows a tick at `E_cap` labelled
  by its tooltip ("30 fps limit" / "Your limit" / "Learned limit") with the range beyond shaded; the gain slider
  shows a tick at Most gain, range beyond shaded.
- Longest exposure options: "Fit the frame rate (1/32 s at 30 fps)", then every whole stop of the camera's range from
  shortest to longest, each with what it means: "1/64 s (sharper motion, more gain)", "1/16 s (camera drops to
  ≈ 15 fps)".
- Most gain: slider over the gain range; "Off" at the minimum.
- Status line: `role="status"`, `aria-live="polite"`, a dot coloured by state (neutral: adjusting/holding;
  warning: a limit; error: handed back or paused by conflict). One line, wraps on narrow windows.
- Double-click / Delete reset is disabled while the slider is disabled. Reset all ends every Better Cam auto mode.
- Keyboard: the selects and sliders are the existing focusable components; no new key handling.

### 4.2 Status texts

| State | Text |
|---|---|
| `starting` | "Adjusting…" |
| `adjusting` | "Adjusting…" |
| `holding` (exposure) | "Holding 30 fps · gain 48 of 75" (whole stops: "Holding 30 fps · 1/32 s") |
| `holding` (WB) | "Holding · 4650 K" |
| `limit_gain` | "Gain at its maximum (75). Add light, allow more gain, or allow a longer exposure." |
| `limit_exposure` | "Exposure at the 30 fps limit and this camera has no gain. Add light or allow a longer exposure." |
| `too_bright` | "Too bright: exposure at its shortest (1/8192 s) and gain at 0." |
| learned cap | "The camera dropped to 24 fps at 1/32 s, so Better Cam keeps exposure at 1/64 s or shorter." |
| `no_signal` | "Nothing to measure (lens covered?). Brightening slowly." |
| `no_picture` (> 10 s) | "No picture to measure." |
| `at_limit` (WB) | "White balance at its limit (2800 K): the light is warmer than this camera can correct. Auto Adjust can finish it." |
| `conflict` | "Another program keeps changing Exposure. Better Cam auto paused. [Resume]" |
| `ignored` | "The camera ignored Better Cam auto for Exposure. Back to Camera auto. [Try again]" |
| `no_response` | "The picture does not change when Better Cam changes Exposure. Back to Camera auto." |
| `not_answering` | "The camera stopped answering. Exposure went back to Camera auto." |
| `hunting` | "Exposure kept swinging. Back to Camera auto." |
| camera closed | "Starts when the camera opens." |
| paused | "Paused." |
| stripes note | "Short exposures under mains light can show stripes." |

Hand-back states also raise one toast with the same text.

### 4.3 Where the targets live

- All Better Cam auto settings are **per camera**, in the Camera controls card, stored with the camera's record.
  Exposure-loop settings (target brightness, longest exposure, most gain, response) sit under the exposure row; white
  balance settings (warmth, response) under the white balance row.
- They are **not shared with Auto Adjust**. Auto Adjust's settings are filter parameters (in presets, per chain);
  these are camera facts (per device). Same scale and same default (target 0.45) so a user who sets both sees the
  same number mean the same thing. When Auto Adjust with exposure on is in the chain, the exposure sub-block shows
  "Auto Adjust finishes the brightness at its own target (0.50)." When it is not and the camera is whole-stops only,
  it shows "Add Auto Adjust for the fine trim." with a button that adds it (the existing quick-add action).

---

## 5. Data model and API

### 5.1 Stored record (frontend, `localStorage["better-cam"].controls`)

Additions to `StoredControl` and `StoredDevice` in `src/camera/schema.ts`. Old saves load unchanged; an older build
reading a new save sees an `app` control as Manual at its last value (safe).

```ts
type ControlMode = "manual" | "camera" | "app";
type Speed = "slow" | "normal" | "fast";

interface StoredControl {
  value: number;        // Manual value; for "app": the resume point
  auto: boolean;        // the camera's Auto flag; false for "app"
  mode?: "app";         // absent: Manual or Camera auto as `auto` says
}

interface AutoCfg {
  exposure: {
    target: number;                  // 0.2..0.8, default 0.45 (sRGB-coded mean, Auto Adjust's scale)
    longest: "fit" | number;         // "fit" or a whole stop in log2 seconds; default "fit"
    max_gain: number | null;         // device units; null = 75 % of the range
    speed: Speed;                    // default "normal"
  };
  white_balance: {
    warmth: number;                  // -1..1, default 0
    speed: Speed;                    // default "normal"
  };
}

interface Learned {
  gain_stops?: number;               // S, 0.5..6
  wb_alpha?: number;                 // α, 0.002..0.02
  rate_caps?: Record<string, number>; // capture fps (rounded, as text) → longest exposure that held it
  blocked?: ("exposure" | "white_balance")[];
}

interface StoredDevice {
  name: string;
  values: Partial<Record<ControlKey, StoredControl>>;
  auto?: AutoCfg;       // kept when no control is in "app", so switching back restores the targets
  learned?: Learned;
}
```

Rules: `isDefaultState` is false for `mode: "app"`. `rememberControl` keeps `mode`. `normalizeControls` validates the
new fields (ranges clamped, unknown keys dropped). `forgetControls` (Reset all) drops `values`, keeps `auto` and
`learned`. A gain entry with `mode: "app"` means "driven by the exposure loop".

### 5.2 Rust record

`capture/controls.rs`:

```rust
pub struct StoredControl {
    pub key: ControlKey,
    pub auto: Option<bool>,
    pub value: Option<i32>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub app: bool,                       // Better Cam auto; value = resume point, written as manual on open
}
```

`apply_snapshot` needs no change: an `app` control has `auto: Some(false)` and a value, so it is written as manual
(the resume point). `camera_controls.rs`: `WireControl` gains `mode: Option<String>`; `WireRecord` gains
`auto: Option<AutoCfg>` and `learned: Option<Learned>` (serde structs mirroring 5.1, with defaults and clamping);
`ControlSnapshot` carries them through `ControlsHub::record`.

### 5.3 Commands (additions)

| Command | Args | Returns | Notes |
|---|---|---|---|
| `get_camera_controls` (changed) | `id` | `ControlInfoDto[]` | Each entry adds `mode: "manual"\|"camera"\|"app"`, `app: "full"\|"stops"\|null` (offered, and how), `app_why?: string` (why not), `driven_by?: "exposure"` (gain). |
| `set_control_mode` | `id, key, mode` | `ControlInfoDto[]` (the rows that changed: exposure and gain together) | `app`: engage (2.5.11); `manual`: last driven value becomes manual; `camera`: Auto flag on. Updates the record. Works on a lease too: stores the mode and writes the seed as a manual value (the camera keeps it); the loop starts at the next open. |
| `set_auto_config` | `id, cfg: AutoCfg` | — | Live; the next decision uses it. Stored in the hub record. |
| `resume_auto` | `id, key` | — | After `conflict`; also clears `blocked` ("Try again"). |
| `start_engine`, `set_source` (changed) | `controls` record gains `mode`, `auto`, `learned` | — | As 5.1. |

Hub additions (`camera_controls.rs`):

```rust
impl ControlsHub {
    /// The controller's write: only for the attached camera `token` and a control whose record mode is `app`.
    /// Takes the write gate; updates `last` and the record's resume value; does not touch `prior`.
    pub fn drive(&self, token: u64, key: ControlKey, value: i32) -> Result<ControlInfo, DriveError>;
    /// Read one control of the attached camera (W6 read-back), outside the gate.
    pub fn read_attached(&self, token: u64, key: ControlKey) -> Result<Option<ControlInfo>, ControlError>;
    /// Whether `apply_on_open` finished for `token` (the controller starts after it).
    pub fn applied(&self, token: u64) -> bool;
    pub fn set_mode(&self, id: &str, key: ControlKey, mode: ControlMode) -> Result<Vec<ControlInfoDto>, String>;
    pub fn set_auto_config(&self, id: &str, cfg: AutoCfg);
}
pub enum DriveError { NotAttached, NotApp, Control(ControlError) }
```

`DriveError::NotApp` is how a user's mode change wins: the controller drops the loop on it.

### 5.4 Events (additions)

| Event | Payload | When |
|---|---|---|
| `controls-changed` (changed) | `ControlInfoDto[]` with the new fields | As today, plus after controller writes (≤ 4 Hz). |
| `auto-status` | see below | On every state change; ≤ 2 Hz while `adjusting`. |

```ts
interface AutoStatus {
  id: string;                         // camera
  exposure?: {
    state: "starting" | "adjusting" | "holding" | "limit_gain" | "limit_exposure" | "too_bright" | "no_signal"
         | "no_picture" | "paused" | "closed" | "conflict" | "ignored" | "no_response" | "not_answering" | "hunting";
    exposure: number; gain: number | null;
    cap: number; cap_reason: "fps" | "user" | "learned";
    max_gain: number | null; fps: number; target_fps: number;
    message: string | null;           // the text of 4.2, made in Rust so tests can check it
    handed_back?: "camera" | "manual";
  };
  white_balance?: { state: string; kelvin: number; message: string | null; handed_back?: "camera" | "manual" };
  learned?: Learned;                  // present when it changed: the store persists it
  resume?: { exposure?: number; gain?: number; white_balance?: number }; // present on entering "holding"
}
```

### 5.5 What the engine provides

```rust
/// One measurement of the camera picture before the filter chain.
pub struct MeterSample {
    pub token: u64,          // the hub token of the camera session that produced it
    pub capture_qpc: u64,    // capture time of the measured frame
    pub stats: Stats,        // 16x16 linear means + coverage (fx::auto_adjust::Stats)
}

/// Read by the controller each tick.
pub struct SessionFacts {
    pub token: Option<u64>,  // attached camera, None when closed
    pub target_fps: f64,     // F: the opened media type's rate
    pub capture_fps: f64,    // measured over the last 2 s
    pub paused: bool,
    pub resumed_qpc: u64,    // samples captured before this are invalid
}
```

- `Shared.metering: AtomicBool` (set by the controller when any control is `app` for the attached camera).
- Render thread: after `converter.convert`, before `chain.run`, when `metering` and ≥ 66 ms since the last submit:
  run the stats chain on `rgba`, submit to the meter's own `ReadbackRing`; poll it without waiting each frame; push
  mapped results into `Shared.meter` (one slot, newest wins, `Condvar` notify). Meter GPU resources are dropped with
  the chain on device loss.
- `Engine::auto_status` plumbing: the controller emits through the existing `EventSink` (`EngineEvent::AutoStatus`).

### 5.6 Controller core API (`src-tauri/src/auto_controls/`)

```rust
pub struct AutoController { /* both loops, watchdog, learned values */ }

impl AutoController {
    pub fn new(caps: &[ControlInfo], cfg: AutoCfg, learned: Learned, modes: Modes) -> AutoController;
    pub fn support(caps: &[ControlInfo], learned: &Learned) -> Support;   // 2.3, for the DTOs
    pub fn set_cfg(&mut self, cfg: AutoCfg);
    pub fn set_modes(&mut self, modes: Modes);
    pub fn on_sample(&mut self, s: &MeterSample, capture_time: f64);
    pub fn on_write_done(&mut self, w: WriteDone);        // key, requested, readback, ok/err, started, done
    pub fn on_readback(&mut self, info: &ControlInfo, now: f64);
    pub fn tick(&mut self, now: f64, facts: &SessionFacts) -> Vec<Action>;
    pub fn status(&self) -> AutoStatus;
    pub fn learned(&self) -> &Learned;
}

pub enum Action {
    Write { key: ControlKey, value: i32 },
    ReadBack { key: ControlKey },
    HandBack { key: ControlKey, to: HandBack },   // Camera auto or a manual value
    Emit,                                         // status changed
}
```

Time is `f64` seconds from the runner's clock (`Instant` in the app, injected in tests); QPC capture times are
converted with the frequency from `vcam_protocol`.

---

## 6. Test plan

### 6.1 Simulated camera (`auto_controls/sim.rs`, tests only)

A discrete-event model that implements `CameraControls` (so it also plugs into the hub tests) and produces `Stats`.

- **Scene:** 16×16 reflectance map (a grey wall, a face-like mid patch, two bright cells, a dark corner), luminance
  `L(t)` and light colour temperature `T(t)` as scripted functions. Optional flicker: `L · (1 + a · sin(2π f t))`
  averaged in closed form over the exposure window (f = 100 or 120 Hz).
- **Exposure:** `τ = 2^E` s. Frame interval `max(1/F, τ + 1 ms)`, or with the "rate drop" switch `max(1/F, τ/0.8)`,
  so a long exposure lowers the frame rate as on W4GS.
- **Gain:** `gain_lin = 2^(S_true · (G − G_min)/(G_max − G_min))`; variants: `S_true` ∈ {1.5, 3, 5}, and a bent curve
  (dB-linear to the middle, twice as steep after).
- **Signal:** `lin = L · refl · τ / τ_ref · gain_lin · wb_gains(T, K_set)`, plus Gaussian noise
  `σ = 0.01 · sqrt(lin) + 0.002 · gain_lin`, clipped at 1, sRGB-encoded and averaged per cell from 4×4 sub-samples
  (so clipping inside a cell is modelled), then decoded to linear as the GPU pass delivers.
- **White balance:** raw sensor colour of a blackbody at `T` (a three-point table interpolated in mired), camera gains
  from `K_set` with the same table; `c` follows `α_true · (M_set − M_light)`, `α_true` ∈ {0.004, 0.008, 0.012}.
- **Timing:** a write takes 5 ms (first write to exposure / WB after "open": 500 ms; "slow" switch: 300 ms each); a
  value lands at the first frame starting ≥ 0.1 s after the write returns; stats arrive 2 frames after capture.
- **Faults:** `ignore_writes` (accepts, read-back shows the old value), `no_effect` (read-back shows the new value,
  picture unchanged), `fail` (writes error), `stale_auto` (while its Auto flag is on, reads report a stale value),
  `meddle(t, key, value)` (another program writes).
- **Clock:** the test loop advances simulated time frame by frame and calls `tick` every 100 ms.

### 6.2 Cases the controller must pass (`cargo test -p better-cam auto_controls`)

| # | Case | Pass criteria |
|---|---|---|
| 1 | Dim room, F = 30, needs `E = −5` and ≈ 1.5 stops of gain; cold start `E = −6, G = 0` | Within ±0.33 stop of target in ≤ 3 s; `E` never above −5; sim fps ≥ 29.5 for every 1 s window after the first write; ≤ 15 writes to converge; 0 writes in the next 60 s. |
| 2 | Step +2 stops (light on), then −2 stops | Back within the band in ≤ 2 s each; overshoot past target ≤ 0.5 stop. |
| 3 | Darker than the limits allow | Ends at `E_cap`, `G_cap`; state `limit_gain` within 5 s; no writes afterwards; fps held. |
| 4 | Bright scene needing `E = −11` | Reached with gain at `G_min`; no decision moves more than `step_max`. |
| 5 | 100 Hz flicker, a = 0.3, at F = 30 | Steady state: ≤ 1 write per 5 s, no exposure write after convergence, no reversal pattern. |
| 6 | Gain curve mismatch (`S_true` 1.5 and 5 with initial `S = 3`), and the bent curve | Converges in ≤ 5 s with ≤ 2 reversals; learned `S` within 30 % of `S_true` after 20 gain moves (straight curves). |
| 7 | Whole-stops camera (no gain) | Converges to within ±0.6 stop; state `holding`; exposure-only status text. |
| 8 | Rate drop: sim lowers fps when `τ > 0.8/F` | Learned cap set within 4 s; fps ≥ 0.95 F afterwards; cap reported in `learned.rate_caps["30"]`. |
| 9 | Rate low for another reason (fps capped at 20 regardless of `τ`) | Cap tried once, removed, not learned again; no further exposure moves from this rule. |
| 10 | Lens covered after convergence (all cells black), uncovered after 10 s | Holds 1.5 s, then ≤ 1 stop/s up to the limits; after uncovering, back within the band in ≤ 3 s. |
| 11 | Flashlight (all cells clipped) | Moves down every decision by `step_max` until `clip < 0.5` or floors; never holds while `clip ≥ 0.5`. |
| 12 | Slow scene ramp of 3 stops over 30 s with 0.1-stop noise | Exposure changes exactly 3 times; no back-and-forth at a boundary. |
| 13 | Crossover order | Brighter crossover: gain write precedes the exposure write; darker: exposure first. |
| 14 | Format change F 30 → 60 at `E = −5` | First decision writes `E = −6` and raises gain by ≈ 1 stop; ≤ 2 writes. |
| 15 | WB step 6500 K → 3000 K light | `|e_wb| < 0.03` in ≤ 6 s; no write > `ΔM_max`; ≤ 2 reversals. With `α_true` 0.004 and 0.012: converges, learned `α` within 30 %. |
| 16 | WB light at 2000 K | `at_limit` at 2800 K within 8 s; no writes afterwards. |
| 17 | WB and exposure together after a light change | No WB write within 0.3 s of an exposure batch; no WB write while `|ê| > 1`. |
| 18 | `ignore_writes` on exposure | W2 after 3 writes; `HandBack(Camera auto)`; `blocked` set; no further writes. |
| 19 | `no_effect` on exposure | W3 within 5 s; hand back; no further writes. |
| 20 | `fail` | ≤ 4 write attempts, then hand-back attempt, state `not_answering`. |
| 21 | `meddle` 4 times in 60 s | 3 re-takes, then `conflict`; no writes until `resume`. |
| 22 | `slow` writes | Never more than 1 batch per `2 ×` write time; converges in ≤ 6 s. |
| 23 | Paused for 5 s mid-adjustment | No writes while paused; samples before resume ignored; converges after. |
| 24 | Engagement from `stale_auto` | Seed from resume/default, not from the stale read; first write is the exposure manual write. |

Hub integration (`cargo test -p better-cam camera_controls`), with `FakeControls` wrapped by the sim response:

- `set_control_mode(app)` → loop writes go through the gate; `controls-changed` carries `mode: "app"`.
- A user `set_control_mode(manual)` while a write is queued → the next `drive` returns `NotApp`; the manual value is
  the last driven value.
- Reopen: `apply_on_open` writes the resume point (log: `auto:exposure:false`, `value:exposure:-5`, `value:gain:48`)
  before any `drive`.
- A lease (camera closed): `set_control_mode(app)` stores the mode and seed; no loop starts.
- `parse_record` accepts `mode`, `auto`, `learned`; rejects nothing old; clamps out-of-range values.

Frontend (`npm run test:schema`): `normalizeControls` keeps valid `mode`/`auto`/`learned` and drops invalid ones;
`rememberControl` stores `app` controls; `forgetControls` keeps `auto` and `learned`; `controlsRecord` sends all of it.

### 6.3 Live checks on W4GS

Run in the owner's usual dim room, 1080p MJPEG, Better Cam auto on exposure with defaults, Auto Adjust off unless
stated. Log line per write: `[auto] t=… exposure -5 gain 48 e=+0.21 state=holding`.

1. **Frame rate:** at 30 fps requested, footer capture fps ≥ 29.5 for 60 s (Camera auto gives 14-17). At 60 fps,
   ≥ 59 for 60 s. If either fails, the learned cap must engage within 4 s and the rate must recover; record the cap.
2. **Convergence:** from Manual at −9 (dark), switch to Better Cam auto: picture within the band in ≤ 3 s (log).
3. **No hunting:** 5 minutes steady: ≤ 1 write per 10 s on average (count log lines); no visible pumping in the
   preview or in the Windows Camera app.
4. **Light switch:** room light off/on: recovered in ≤ 2 s each way; no frame fully black or white for more than
   0.5 s (screen recording).
5. **White balance:** move a warm desk lamp into the scene: WB settles in ≤ 6 s with no visible colour swinging.
6. **Reopen:** Stop → Start: no flash longer than 1 s; the log shows the resume values written before the first
   decision.
7. **Mode changes:** Better Cam auto → Manual keeps the picture; → Camera auto hands over (frame rate falls again,
   as expected).
8. **Noise cost:** screenshots at 30 fps Better Cam auto and at Camera auto (15 fps), same scene: record gain and a
   short note on visible noise for the owner.
9. **First-write latency:** the first exposure write after open is logged with its duration (expected 200-700 ms);
   no decision happens before it returns.
10. **With Auto Adjust** in the chain (recommended default): step the light; note any visible double movement. This
    decides open question 8.

---

## 7. Build phases

Each phase is one coding session. A1 and A3 can run in parallel; A5 can start against this document's API once A4's
command names are fixed (they are, in 5.3) and merges after A4.

```
A1 controller core: exposure loop + sim ──► A2 WB loop + watchdog ──┐
A3 engine meter (parallel with A1, A2) ─────────────────────────────┼─► A4 hub, runner, commands, events ─► A6 live tuning + docs
                                                 A5 frontend (parallel with A4, merges after it) ──────────┘
```

### A1 — Controller core: exposure loop and simulated camera

- **Goal:** the pure `AutoController` for exposure + gain: meter maths (2.4), limits and frame-rate rule (2.5.2),
  decision (2.5.5), allocation (2.5.6), gain learning (2.5.7), frame-rate guard (2.5.8), saturation (2.5.9),
  engagement seed (2.5.11); the simulated camera (6.1).
- **Files:** new `src-tauri/src/auto_controls/{mod.rs, meter.rs, exposure.rs, sim.rs}`; `src-tauri/src/lib.rs` (one
  `pub mod` line). Reads `Stats`, `centre_weight`-style helpers: copy the three small pure helpers (`srgb_encode`,
  `luma`, `smoothstep`) or call them if Auto Adjust already exports them; do not edit `fx/auto_adjust.rs` (it is being
  changed in parallel).
- **Decisions:** 2.4, 2.5, 2.10. Constants exactly as written; changes only with a note in this document.
- **Check:** `cargo test -p better-cam auto_controls` passes cases 1-14, 23, 24 of 6.2.

### A2 — White balance loop and watchdog

- **Goal:** the WB loop (2.6), the watchdog W1-W6 (2.7), hand-back actions, `support()` (2.3, 2.9), status texts
  (4.2) made in Rust.
- **Files:** `src-tauri/src/auto_controls/{white_balance.rs, watchdog.rs, status.rs, mod.rs, sim.rs}`.
- **Decisions:** 2.6, 2.7, 2.9; WB warmth scale: adopt Auto Adjust's if its warmth work has merged, else 2.4's.
- **Depends on:** A1.
- **Check:** `cargo test -p better-cam auto_controls` passes all 24 cases of 6.2; a unit test lists every `support()`
  outcome for: W4GS's `ControlInfo` set, no gain, exposure range 0..10000, WB 0..100, `can_manual = false`, blocked.

### A3 — Engine meter (parallel with A1, A2)

- **Goal:** the pre-chain statistics pass and `MeterSample` mailbox (5.5), gated by `Shared.metering`, ≤ 15 Hz,
  surviving device loss; `SessionFacts` from `Shared` (target fps, 2 s capture fps, paused, resume time).
- **Files:** new `src-tauri/src/meter.rs`; `src-tauri/src/engine.rs` (render loop hook after `converter.convert`,
  `Shared` fields); `src-tauri/src/fx/auto_adjust.rs` only to make `StatsChain`/`stage_sizes`/`Stats` `pub(crate)`
  (coordinate: start after the parallel Auto Adjust work has merged, or copy the 40-line chain into `meter.rs` and
  merge later).
- **Decisions:** 2.4 "Where", "How", "When"; no change to output or preview paths.
- **Check:** engine test with the synthetic grey source at a known level: a sample arrives within 3 frames of
  `metering = true`, its weighted linear mean is within ±0.01 of the known value, and `capture_qpc` matches the frame;
  no sample with `metering = false`; ≤ 16 samples per second at 60 fps. Perf: a 1080p60 synthetic run (PERF.md
  method) shows ≤ +0.03 ms mean GPU time with metering on.

### A4 — Hub modes, runner thread, commands and events

- **Goal:** modes in the record (5.1, 5.2), `drive` / `read_attached` / `applied` / `set_mode` / `set_auto_config`
  (5.3), the `controls auto` runner thread executing `Action`s (2.8), `auto-status` and extended `controls-changed`
  (5.4), Tauri commands, interplay rules of section 3 that live in the backend.
- **Files:** `src-tauri/src/capture/controls.rs` (`StoredControl.app`), `src-tauri/src/camera_controls.rs`,
  `src-tauri/src/auto_controls/runner.rs`, `src-tauri/src/engine.rs` (spawn with the poller, attach/detach, events),
  `src-tauri/src/api.rs`, `src-tauri/src/commands.rs`, `src-tauri/src/main.rs` (register commands, emit
  `auto-status`).
- **Decisions:** 2.5.11 engagement, 2.7 hand-back, section 3, 5.3-5.5.
- **Depends on:** A1, A2, A3.
- **Check:** `cargo test -p better-cam` passes, including the hub integration tests of 6.2 and an engine test with a
  sim-backed fake camera: engage → `auto-status` reaches `holding` within 5 s; pause → no writes; stop/start → resume
  values written by `apply_on_open` before the first `drive`; user `manual` mid-loop → `NotApp`, no further writes.
  `cargo clippy -p better-cam -- -D warnings` clean.

### A5 — Frontend: Camera controls card (parallel with A4)

- **Goal:** types and storage (5.1), store actions (`setControlMode`, `setAutoConfig`, `resumeAuto`, listening to
  `auto-status`, persisting `learned` and `resume`), the card of 4.1 with the texts of 4.2 and the hints of 4.3.
- **Files:** `src/camera/schema.ts`, `src/camera/schema.test.ts`, `src/api.ts`, `src/store.ts`,
  `src/components/ControlsCard.tsx`, the card's stylesheet (markers, shaded range, sub-block indent, status dot).
- **Decisions:** 4.1-4.3; existing `Select`, `Slider` and switch components; Better Mic look.
- **Check:** `npm run test:schema` passes the new cases of 6.2; `npm run build` passes. With A4 merged: in
  `npm run tauri dev` on W4GS, exposure and white balance show three modes, focus shows the Auto switch, the
  sub-blocks unfold only in Better Cam auto, the sliders are disabled with live values and markers, gain says "Set by
  Better Cam auto exposure", and a covered lens shows the `no_signal` text.

### A6 — Live tuning on W4GS and documentation

- **Goal:** run 6.3; adjust only constants that the live data contradicts (record old → new and why in this
  document); update PLAN.md (9.3 item 1 reversal with the date, 5.5 commands, 5.6 events, 6.2 modes) and the header
  comment of `capture/controls.rs`; record the measured noise cost and the W4GS gain curve estimate (`S`).
- **Files:** `docs/AUTO-CONTROLS.md`, `docs/PLAN.md`, `src-tauri/src/capture/controls.rs` (comment only), constants
  in `src-tauri/src/auto_controls/*`.
- **Depends on:** A4, A5.
- **Check:** every item of 6.3 recorded with its measured value in a new section "Live results" of this document;
  items 1-7 meet their criteria, or the gap is listed as a follow-up with the owner's agreement.

---

## 8. Open questions for the owner

| # | Question | Recommendation |
|---|---|---|
| 1 | Default **Most gain**: the full range, or a cap for noise? | 75 % of the range (75 on W4GS). You can raise it; A6 shows you the noise at both. |
| 2 | Default **Longest exposure**: fit the frame rate, or allow the camera to drop to half rate in very dim light? | Fit the frame rate. Full frame rate is the reason for the feature. |
| 3 | Should the exposure target be **shared with Auto Adjust's target**? | No. Separate, same scale, same default; Auto Adjust is per chain and in presets, this is per camera. The card shows Auto Adjust's target when both are on. |
| 4 | **Metering area:** the whole camera picture (centre-weighted), or only the part the output shows after Crop / Scaling? | Whole picture in v1. Cropped metering needs the chain's geometry in the meter; revisit if you crop a lot. |
| 5 | **Hand-back target** when the watchdog gives up: Camera auto, or your last manual value? | Camera auto where the camera has it: a correct picture at a lower frame rate beats a frozen wrong one. |
| 6 | Should the default for a **new camera** become Better Cam auto exposure? | No (brief: nothing changes until you pick it). Reconsider after A6 shows it beats Camera auto on W4GS. |
| 7 | Mode labels: "Manual / Camera auto / Better Cam auto"? | Yes. "Auto" alone becomes ambiguous once there are two. |
| 8 | Feed-forward from the device loop into Auto Adjust (so Auto Adjust does not follow each device step for 0.5 s)? | Not in v1. Build it only if live check 10 shows visible double movement. |
| 9 | Finer exposure: try the Windows extended exposure control (100 µs units) to escape whole stops and mains banding? | Later, as a spike. Whether UVC cameras on the inbox driver expose it is **unverified**. |
| 10 | Should Better Cam auto also be offered for **focus** (software contrast autofocus)? | No. Camera auto focus exists on W4GS; a software focus sweep is a separate feature with visible hunting by design. |
