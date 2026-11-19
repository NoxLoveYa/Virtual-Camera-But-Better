# Better Cam Auto — software auto for the camera's own controls

Status: built (phases A1-A5) and live-tested on the owner's W4GS (A6, 2026-11-19: section 9). Date: 2026-11-18. Covers
two owner requests: "auto also drives the camera itself" and "per-control targets in Camera controls". The third
request (more targets in the Auto Adjust filter) is built separately and is only referenced here. A6 found the room
dark (see 9): checks that need light are listed there as owed to the owner.

**Owner decision recorded (2026-11-18), reverses part of PLAN 9.3 item 1.** PLAN 9.3 item 1 said "Software never
drives the device controls". The owner now wants software to drive them. New rule: Better Cam may write a camera's
exposure, gain and white balance, but only for a control the user switched to **Better Cam auto**. Nothing changes
for users who never pick it. PLAN.md 9.3, 5.5, 5.6, 6.2 and the header of `capture/controls.rs` ("Software never
computes control values") were updated in phase A6 (2026-11-19).

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
3. Exposure does not get longer than one frame time, so the camera keeps the frame rate you asked for. In the owner's
   dim room this means 30 fps (or 60) instead of the 14-17 fps the camera's own auto gives. **One exception (2.5.12):**
   when the picture is still more than a stop too dark with gain at its limit, exposure may go one stop (twice the
   frame time) longer, and the frame rate drops with it (30 → about 16 fps).
4. When exposure is at that limit, gain makes up the rest, up to a **Most gain** limit you set.
5. The cost: gain adds noise. Each halving of exposure time needs one more stop of gain. At 30 fps that is about one
   stop more than the camera's 15 fps auto: visibly grainier shadows (about 1.4-2× the noise). The exception of 3 is
   what takes that stop back in a room too dark for the gain limit: a brighter, cleaner picture at the cost of frame
   rate (the owner's room needed it: at 1/32 s and full gain the picture was still 1.7-2.2 stops dark, green and grainy).
6. You choose per camera: target brightness, longest exposure (default "fit the frame rate"; a stop at the frame-rate
   limit never lowers the frame rate), most gain, response speed, and white balance warmth.
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
coarse UVC steps. Exposure prefers the **longest exposure within the frame time**, then gain; only a picture that is
still more than a stop too dark at full gain may go one stop past the frame time (2.5.12). The core is a pure
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

> **Since 10-13 (2026-11-19) the reading is not only this `e`.** The `error` the loop gets is `e_all` (the one above)
> plus two one-way terms that read the subject: a backlight term (10.4: a window or lamp in the upper part makes the
> loop expose for the zone around the person, +0 to +3 stops; the zone's reading `e_face` has the knee of 12.4 and is
> the one subject reading of 13.4) and a front light term (11.4: a lit face in a much darker room takes up to 3 stops
> off; faded out by light sources and by a wide bright patch, 13.4). Both are 0 in an evenly lit room, which reads as
> `e_all`, bit for bit. What counts as a light source follows the target (13.12). `ExposureReading` also carries
> `backlight`, `sources`, `front`, `room_gap` and `spread` for the log (9.6) and the status flags (4.2).

**White balance error.** Gray world on the cells Auto Adjust's white balance trusts (not near black, not near white,
not strongly saturated; same smooth weights). `m` = weighted mean linear RGB of those cells.
`conf_wb = smoothstep(0.02, 0.12, Σw / Σc)`. Signal `c = ln(m_B / m_R)` (the blue-red axis; colour temperature moves
along it). Target `c*(warmth)`, warmth in −1..+1 (+1 = warmer). Error `e_wb = c − c*`.

> **Note W3 (2026-11-19, rule changed: old → new).** Old: the gray world above, with the cells judged by their
> saturation *before* any correction. New: the shared estimator `fx::awb::estimate_light` (docs/AUTO-WHITE-BALANCE.md
> 3.1) judges them *after* a correction, so under an orange light the neutral surfaces count and the loop has an
> unbiased reading. `c` is its `t`, `conf_wb` its `conf`, and `e_wb = t − c*(warmth)` as before. The estimator's
> reason (`dark`, `one_colour`) picks the hold text (2.6). **Mean-luma gate (F4's leftover):** a picture whose centre
> weighted mean coded luma is below `MIN_MEAN_LUMA = 0.10` (about 25 of 255) has `conf_wb = 0` and the reason `dark`,
> whatever its cells look like. Why: a black picture lifted by gain (gain 75, display luma 17) is a flat grey, flat
> grey is a neutral light, and the loop said `holding` on it. The exposure loop aims at 0.45, so only a picture that is
> dark for good ends here.

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
  time: the user accepts a lower rate; the option text says which rate, e.g. "1/16 s (camera drops to ≈ 16 fps)").
  **Follow-up F3:** the shown rate is `min(F, 1/τ)` rounded to a whole number (it was `0.95 / τ`, which said 15 where
  W4GS measured 16.2 fps at 1/16 s); `longestOptions` in `src/camera/schema.ts` is the one place that makes the text.
- **Learned cap** (2.5.8): `E_learned[F]`, only applies with `fit`.
- `E_rate = clamp(min(longest, E_learned[F] if any), E_min, E_max)`: the longest exposure that keeps the frame rate
  (named `E_cap` in the rest of this document, and still the cap everywhere except where 2.5.12 says otherwise; the
  card's tick and the status field `cap` are `E_rate`).
- **Ceiling** `E_ceil` (2.5.12): `E_rate + 1` (clamped to `E_max`) with `longest = fit`, so one stop, twice the frame
  time (at 30 fps -5 → -4 = 1/16 s ≈ 16 fps; at 60 fps -6 → -5 = 1/32 s ≈ 30 fps; a learned cap makes it
`E_learned + 1`); with a stop `n` it is `E_rate = n`: the user's stop is a
  limit the loop never passes, and a stop at the frame-rate limit means the frame rate is never lowered.
- `E_cap` (the cap the allocation uses) is `E_rate`, or `E_ceil` while the loop lowers the frame rate (2.5.12).
- **Most gain** (user): `G_cap`, default `G_min + round(0.75 · (G_max − G_min))` (75 on W4GS). `Gs_cap = gs(G_cap)`.
  Most gain = `G_min` turns gain off (exposure-only behaviour). **Follow-up F1:** "off" also drives the device gain to
  `G_min`: the loop treats it like a constraint (the allocation with `Gs_cap = 0`, one gain write, no samples needed,
  exposure takes the stops the gain gave as far as whole stops allow), also when Most gain is changed while the loop
  runs. The status then says "gain is off" (4.2), not "no gain": that wording is for cameras without a gain control.

#### 2.5.3 Order of preference

Longest exposure up to `E_cap` first, then gain. Not the shortest exposure with more gain: gain multiplies noise, and
short exposures make mains-light banding worse. Motion blur at 1/32 s is acceptable for a webcam; a user who wants
sharper motion picks a shorter Longest exposure. Going darker, gain goes down first, then exposure. The one
exception is 2.5.12: past the frame-rate limit exposure is the last resort, used only after gain is at its limit, and
given up first (before gain is cut) when the light comes back.

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

With Longest exposure = `fit`, the loop **not** lowering the frame rate on purpose (2.5.12: the low rate is then what
it asked for), `E == E_cap`, no write in the last 1 s, and the 2-second capture rate `< 0.95 · F` for
3 s (**A6: constant changed, old → new: `0.9 · F` → `0.95 · F`**; live on W4GS at 60 fps, 1/64 s gives 55.1 fps = 92 %
of the request, so `0.9` never fired and the rate stayed short of 59; see 9.2): set `E_learned[F] = E_cap - 1` (not below `E_min`); the next decision applies it as a constraint. If the rate
recovers to `≥ 0.95 · F` within 3 s, keep it (stored per camera and frame rate). If not, the cause is not exposure:
remove it, and do not learn again in this session. Status text: "The camera dropped to 24 fps at 1/32 s, so Better
Cam keeps exposure at 1/64 s or shorter."

#### 2.5.9 Saturation (limits reached)

- `E' == E_cap`, `G' == G_cap` and `ê > B_out` for 3 s: state `limit_gain` (or `limit_exposure` on whole-stops
  cameras). With `fit` and `ê` more than 1 stop, this is not a limit yet: the loop first lowers the frame rate
  (2.5.12), so `limit_gain` shows at `E_ceil`, or at `E_rate` for a picture at most a stop too dark.
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
2. Seed: `E0 = min(E_ceil, resume.exposure)`, or `min(E_rate, device default)` without a resume point;
   `G0 = resume.gain ?? G_min`. An `E0` above `E_rate` starts the loop with the frame rate lowered (2.5.12), but only
   **provisionally**: the resume point is the last value written and says nothing about the frame rate (a value from an
   earlier room, from before a learned cap, or from another frame rate). The first decision with `conf ≥ 0.2` judges
   it: the loop stays lowered only when the entry condition holds already (`X + ê > E_rate + gs(G_cap) + OVER_ENTER`,
   no timer), else it ends the lowered state and takes the exposure to `E_rate` (a normal room reopens at the full frame
   rate; a dark room stays where it was, with no write). The device's reported
   exposure is not used while Camera auto is on: W4GS then reports a stale manual value.
3. Write `exposure = E0` (manual; this is the slow 200-700 ms first write), then `gain = G0`. On open these are part of
   `apply_on_open` (the stored resume values), so the loop's first move never waits on a slow first write.
4. Start in `adjusting` with no valid samples; the normal validity rule makes it wait for proof.

The resume point (`E`, `G`, Kelvin) is stored when the loop enters `holding`, at most once per 10 s. The camera also
keeps values across a reopen (measured on W4GS), so a reopen shows no flash.

**Follow-up F2:** it is also stored when the loop has sat in a limit state (`limit_gain`, `limit_exposure`,
`too_bright`; white balance `at_limit`) for 3 s, under the same once-per-10-s rule. A dark room never reaches
`holding`; without this each reopen climbed from the seed (-6, 0) for about 2 s. The stored point rides the `resume`
of the status while the state lasts (not before the 3 s are over).

#### 2.5.12 Lowering the frame rate for light (reverses the decision of 1.5 and 8.2)

Live finding (the owner's room, W4GS at 30 fps and 2560×1440, Most gain 100, Fast): the room needed 1.7-2.2 stops more
than `E_rate = -5` and full gain give. The loop sat in `limit_gain` at `E = -5, G = 100`: maximum sensor gain, a
green, dark, grainy picture. The camera's own auto drops to about 16 fps with 1/16 s and is clean. Holding 30 fps was
the wrong trade there.

The rule: while gain is at `G_cap` and exposure at `E_rate`, and `ê > OVER_ENTER = 1.0` stop with `conf ≥ 0.2` for
`LIMIT_AFTER = 3 s` (the median of the valid samples, so not one frame; the 3 s are the saturation timer of 2.5.9,
which does not show `limit_gain` meanwhile), the loop **lowers the frame rate**: `E_cap = E_ceil` (2.5.2), the one stop
past `E_rate`. The 3 s must be unbroken: any decision that does not reach the saturation check (a dip to a bright or
covered picture, a move, the highlight guard, a constraint move, a pause, a gap of 2 s without a frame: W5) starts the
timer again. (The 3 s limit timer of 2.5.9 is not touched by a frame gap; it only shows a text.) The entry itself
writes nothing and leaves the band `adjusting`: the move up to `E_ceil` is the next decision. If that decision finds
the room brighter and holds, or moves gain only, with the exposure still at `E_rate` (gain alone covers it), the
lowered state ends there: it never stays "lowered" at the full frame rate (the guard back on, the next darkening
waiting for its own 3 s).
Nothing else changes: the allocation of 2.5.6 then moves exposure up to `E_ceil` in a crossover and gain drops by the
stop it was giving where exposure alone covers the need (gain is only at its limit when the scene needs the whole
stop). It never passes `Longest exposure` (a stop `n` is the ceiling itself, so a stop at the frame-rate limit means
the old behaviour: gain only) nor the camera's `E_max`.

Back to the frame rate: when the scene needs `OVER_EXIT = 0.5` stop less light than `E_rate` and `Most gain` give
together (`X + ê < E_rate + gs(G_cap) − 0.5`), the loop stops lowering it (`E_cap = E_rate`). The exposure then goes
down to `E_rate` before gain is cut (2.5.6: the exposure goes first, gain takes the rest): in the exit decision itself
when the reading is outside its band; when the reading is in the band that decision writes nothing, and the next
decision's constraint move (`E > E_cap`, brightness-neutral, no samples needed) does it. The frame rate returns first
and gain lands at least half a stop under its limit. Hysteresis: 1 stop of enter and 0.5 stop
of exit make a dead band of 1.5 stops of scene change; between them the exposure stays where it is (and gain follows
the light, so a scene that brightens inside the band lowers gain at the longer exposure). Exit needs `conf ≥ 0.2`.
The state also ends once the exposure, after it was above `E_rate`, is back at or below `E_rate` (a no-gain camera, a
small `Most gain` or a small learned `S` whose `Gs_cap` is under 1.5 stops would otherwise stay "lowered" at the full
frame rate for good, and `E_ceil` would stay allowed): nothing is written for it.

Choices of the numbers: **one stop** is the camera's own auto (W4GS: 1/16 s at about 16 fps) and halves the gain
that was needed; more stops would take the frame rate below what a video call tolerates (8 fps at 1/8 s) and the
user who wants that has `Longest exposure`. **1 stop to enter** (not 0.33) because up to a stop of darkness at 30 fps
and full gain is a picture the owner can use, and a smaller error does not pay for half the frame rate. **0.5 to
exit** because the step back to `E_rate` costs one stop of gain (a visible noise step); landing half a stop under
`G_cap` keeps the loop from being at its limit again straight away.

Other rules: the frame-rate guard (2.5.8) is off while the frame rate is lowered, and the loop does not start lowering
it while the guard is trying a learned cap; a change of `Longest exposure` or `Most gain` ends it (the loop starts again from the new
limits); a reopen from a resume point above `E_rate` starts lowered but is judged by the first confident decision
(2.5.11); a camera that does not take the longer exposure (the write above `E_rate` reads back `E_rate` or less) has
refused it: that is no mismatch for W2 (no hand-back, nothing `blocked`), the lowered state ends and `E_ceil = E_rate`
for the rest of the session (a reopen or a new engagement asks again), so the card shows the normal limit text; a
picture without confidence (`conf < 0.2`: a covered lens, a nearly black one) never lowers it and starts the
entry timer again. Status: "Lowering frame rate for light…" only while the exposure goes past `E_rate` (the entry and
the move up to `E_ceil`; a gain move at the lowered rate, or a start from a lowered resume point, says "Adjusting…"; the "Lowered…" texts show only while the exposure is past `E_rate`),
"Lowered frame rate for light: holding 16 fps · gain 80 of 100" when it holds, and at `E_ceil` with gain at its limit "Gain at its maximum
(100) and exposure at its longest (1/16 s, 16 fps). Add light or allow a longer exposure." (4.2). The card's exposure
tick and `cap` stay at the frame-rate limit (`E_rate`), so the value is shown beyond the tick.

Tests (`auto_controls/tests.rs`, owner's settings): `a_room_two_stops_too_dark_lowers_the_frame_rate` (a),
`a_lit_room_keeps_the_frame_rate` (b), `light_back_restores_the_frame_rate_before_it_cuts_gain` (c),
`a_light_at_the_boundary_does_not_flip_the_frame_rate`, `a_brighter_room_needs_less_gain_at_the_longer_exposure`,
`the_longest_exposure_setting_is_never_passed` (d), `a_reopen_keeps_the_lowered_frame_rate`,
`owner_settings_settle_inside_the_limits_and_sit_at_them_outside`; `case_03` and the F1/F2 tests run with
`Longest exposure` at the frame-rate limit or with the new limit. The audit's cases:
`a_reopen_in_a_normal_room_leaves_the_lowered_frame_rate` and
`a_reopen_above_a_learned_rate_cap_leaves_the_lowered_frame_rate` (a provisional lowered start),
`a_dip_in_the_light_starts_the_entry_timer_again`, `a_camera_without_gain_ends_the_lowered_frame_rate`,
`an_unsure_picture_never_lowers_the_frame_rate`, `a_most_gain_change_ends_the_lowered_frame_rate`,
`the_frame_rate_is_not_lowered_while_the_guard_tries_a_cap`, `the_log_line_is_silent_while_closed_or_paused`. The
second audit's: `a_camera_that_refuses_a_longer_exposure_keeps_the_frame_rate`,
`a_camera_that_ignores_every_exposure_write_is_still_ignored`, `a_gap_in_the_frames_starts_the_entry_timer_again`,
`a_room_that_brightens_after_the_entry_does_not_stay_lowered`,
`a_darkening_after_a_cancelled_entry_waits_for_its_own_timer` and
`a_light_at_the_exit_boundary_does_not_flip_the_frame_rate` (the loop enters for real, then the light wobbles across
the exit point). A change of `Longest exposure` or `Most gain` ends the lowered state and does not restore it (a reopen judges it again).

### 2.6 The white balance loop

- Device setting `K` (Kelvin), worked as mired `M = 10⁶ / K`. Model: `c ≈ α · (M − M_light)`, `α > 0`: a setting
  warmer than the light (higher K, lower M) leaves the picture orange (`c < 0`). Initial `α = 0.006` per mired
  **(unverified)**, learned like `S` (ρ = observed Δc / (α·ΔM), same bounds and exponent), `α ∈ [0.002, 0.02]`, stored
  as `learned.wb_alpha`.
- Samples valid when `capture_time ≥ t_done + 0.3 s`. `ê_wb` = median of last 3 valid `e_wb`; `σ_wb` as above.
- Hold when `conf_wb < 0.3` (a scene of one colour, a dark scene). **Follow-up F4:** the state is then `no_signal`
  ("Nothing to measure (too dark). Keeping 4600 K."), not `holding`: nothing was checked. It leaves the state when the
  confidence is back and measures from the wide band (`adjusting`) like after a write.
  **Note W3 (text by reason, rule changed):** the state stays `no_signal`, the text follows the estimator's reason:
  `dark` (also the mean-luma gate of 2.4) "Nothing to measure (too dark). Keeping 4650 K."; `one_colour` "Mostly one
  colour (a face or a wall): nothing neutral to measure. Staying at 4650 K; Warmth still applies." (the text of W3 was
  "The picture is mostly one colour (a face or a wall): keeping 4650 K."; changed by V1, AUTO-WHITE-BALANCE.md 10.4.4).
  Before W3 the second case also said "too dark", which was wrong. The hold itself is unchanged, but under a strong
  orange cast the confidence is no longer low: `a_cast_with_nothing_neutral_to_trust_is_held` (6.2, the 0.012 camera at
  3000 K) is reversed into `a_strong_cast_is_followed`.
  **Note V2 (rule changed):** the hold is for a dark picture and a true one-colour picture only (the estimator's own
  confidence under 0.3: the skin guard, little neutral surface), because a closed loop on a biased reading goes all the
  way to the wrong answer. A picture with two answers no longer holds: see the slow lane below.
- Band: `B_out = max(0.06, 3σ_wb)`, `B_in = max(0.015, 1.5σ_wb)`, same holding/adjusting states. **Note (live finding,
  constant changed): `B_out` floor 0.04 → 0.06.** The estimate of a person at a desk wanders by ±0.03-0.05 on its own;
  with 0.04 the loop followed it, reversed, and W4 handed back. See AUTO-WHITE-BALANCE.md 9.
- **Note (live finding, rule changed): the loop's `ê_wb` is the mean of the estimator's two answers and a picture with
  two answers is not followed at once.** `e_wb = (t + t_alt)/2 − c*`, and
  `conf_wb = conf · (1 − smoothstep(0.02, 0.06, apart))`, where `apart` is how far the median-start answer lies from
  the shades-of-gray one (`fx::awb`, 3.1 step 3). A bright face beside dim walls has two stable answers; before, the
  loop took whichever the frame landed on (`t` 0, −0.27, −0.55 within a second on a still picture).
  **Note V2 (rule changed, AUTO-WHITE-BALANCE.md 10.4.2): it no longer holds, it slows.** The meter's reason for
  `conf_wb < 0.3` with a sure estimator (`light_conf ≥ 0.3`) is the new `TwoAnswers` (not `OneColour`). The slow lane
  keeps a confidence-weighted average of `e_wb` (time constant 8 s at full weight, 32 s with no confidence left; a gap
  counts as at most 0.5 s; the lane survives 2 s without evidence), writes only when the average passes `B_out` (0.06),
  then shifts it by the move's expected effect (`α·ΔM`). The state is `slow`, text "Unsure of the light (a bright face
  or two lights): adjusting slowly · 4650 K". A doubt of a second or two moves nothing; a doubt that lasts follows the
  middle of the two answers in small steps. A dark picture, or a sample whose estimator has little light of its own
  (`light_conf < 0.3`), is no evidence and still holds (`no_signal`).
- Move: `ΔM = clamp(−k_wb · ê_wb / α, −ΔM_max, +ΔM_max)`; `K' = round_to_step(10⁶ / (M + ΔM))`, clamped to
  `K_min..K_max`; skip if `|K' − K| < step`.
- **Warmth feed-forward (V1, AUTO-WHITE-BALANCE.md 10.4.3).** A change of the Warmth setting always moves the
  camera, in every state: `ΔM = Δc*/α_ff` with `α_ff = max(α, 0.0105)`, written on the next tick whatever the reading
  says and without waiting for the exposure quiet time, the gap or the reading (only for an exposure write in flight).
  The loop then measures from the narrow band; it is not counted as a swing by W4 and not judged by W3.
- White balance never writes in the same 0.3 s as an exposure batch, and waits while the exposure loop is
  `adjusting` with `|ê| > 1` stop: confounded measurements would teach `α` wrong and gray world is unreliable in a
  badly exposed picture. Exposure has priority.
- Saturation: `K` at `K_min` or `K_max` and `|ê_wb| > B_out` for 3 s → `at_limit`. **Note W3:** what is left goes to
  Auto Adjust, which measures this loop's output and (W2) corrects up to a gain of 4 per channel; the text names the
  end of the range (4.2) and the card says whether Auto Adjust is there (W4).
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
    Longest exposure  [Fit the frame rate (1/32 s at 30 fps, one stop longer in a dark room at most gain) ▾]
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
- Longest exposure options: "Fit the frame rate (1/32 s at 30 fps, one stop longer in a dark room at most gain)" (the
  hint only when the camera has a longer stop; without a known frame rate just "Fit the frame rate"), then every whole stop of the camera's range from
  shortest to longest, each with what it means: "1/64 s (sharper motion, more gain)", "1/16 s (camera drops to
  ≈ 16 fps)" (2.5.2: `min(F, 1/τ)`). The default "Fit the frame rate" keeps the rate except in a room that is still
  more than a stop too dark at full gain: then one stop longer, and the status line says so (2.5.12). The stop "1/32 s
  (the frame-rate limit)" never lowers the rate.
- **Slow-camera hint (F6).** Better Cam auto cannot change the camera's own auto. While exposure is in **Camera auto**
  and the measured capture rate (`stats.capture.fps`, about once a second) stays under 0.9 × the rate asked for for
  3 s, the exposure row shows "Camera auto is slowing the camera to N fps in this light. Better Cam auto keeps F fps."
  with a button "Use Better Cam auto" (only when Better Cam auto is offered for this camera); it goes away when the
  rate recovers (or the mode changes). The rate asked for is known to the card for **Custom** format with a rate, and
  for "Highest FPS" / "Match output FPS" read from the rates of the open format (`requestedFps` in `schema.ts`); with
  Device default the card does not know it and shows no hint.
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
| `holding` (exposure, gain off, F1) | "Holding 30 fps · 1/32 s · gain off" |
| `holding` (WB) | "Holding · 4650 K" |
| `no_signal` (WB, F4, reason `dark`) | "Nothing to measure (too dark). Keeping 4650 K." |
| `no_signal` (WB, W3, reason `one_colour`; text changed in V1) | "Mostly one colour (a face or a wall): nothing neutral to measure. Staying at 4650 K; Warmth still applies." |
| `slow` (WB, V2, reason `two_answers`; neutral dot, no resume point) | "Unsure of the light (a bright face or two lights): adjusting slowly · 4650 K" |
| `limit_exposure`, gain off (F1) | "Exposure at the 30 fps limit and gain is off. Add light, allow more gain, or allow a longer exposure." |
| `limit_gain` | "Gain at its maximum (75). Add light, allow more gain, or allow a longer exposure." |
| `starting` / `adjusting`, while the exposure goes past the frame-rate limit (2.5.12; any other adjusting step at the lowered rate says "Adjusting…") | "Lowering frame rate for light…" |
| `holding`, frame rate lowered (2.5.12) | "Lowered frame rate for light: holding 16 fps · gain 80 of 100" (without gain: "… holding 16 fps · 1/16 s") |
| `holding`, backlight term on (10.7) | The `holding` text (also the lowered one) ends with " · backlight: exposing for the middle", e.g. "Holding 30 fps · gain 48 of 75 · backlight: exposing for the middle". On at a median `backlight` of 0.5 stop, off at 0.25. |
| `holding`, front light term on (11.8, 13.11) | The same texts end with " · dark room: exposing for the lit middle", e.g. "Holding 30 fps · gain 31 of 75 · dark room: exposing for the lit middle". On at a held front term of −0.5 stop, off at −0.25. At most one suffix: this one only while the backlight one is off (`status.rs`, `hold_note`). |
| `limit_gain`, frame rate lowered (2.5.12) | "Gain at its maximum (100) and exposure at its longest (1/16 s, 16 fps). Add light or allow a longer exposure." |
| `limit_exposure`, frame rate lowered (2.5.12) | "Exposure at its longest (1/16 s, 16 fps) and gain is off. Add light or allow a longer exposure." (or "… and this camera has no gain. …") |
| `limit_exposure` | "Exposure at the 30 fps limit and this camera has no gain. Add light or allow a longer exposure." |
| `too_bright` | "Too bright: exposure at its shortest (1/8192 s) and gain at 0." |
| learned cap | "The camera dropped to 24 fps at 1/32 s, so Better Cam keeps exposure at 1/64 s or shorter." |
| `no_signal` | "Nothing to measure (lens covered?). Brightening slowly." |
| `no_picture` (> 10 s) | "No picture to measure." |
| `at_limit` (WB, W3: was "…at its limit (2800 K)… Auto Adjust can finish it.") | "White balance at its warmest (2800 K): the light is warmer than this camera can correct." / "White balance at its coolest (6500 K): the light is cooler than this camera can correct." The line about Auto Adjust moves to the card (W4). |
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
  // state: the exposure states without the three limits, plus "at_limit" and (V2) "slow": the picture has two
  // answers and the Kelvin follows their middle slowly. The card draws "slow" with the neutral dot.
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
- **Coloured scenes (W3, `SimCfg::scene`):** a scene of the kit `fx::awb::scenes` (office, warm room, grey room, a face
  or a wall filling the frame, ...) replaces the grey wall. Every cell keeps its reflectance colour and its own luma
  (the exposure meter does not see colour); the cast is `(bb(T_light) / bb(K_set))^(α_true / 0.0105)` per channel, so
  `α_true = 0.0105` is the physical camera. The true `e_wb` is the blue-red signal of that cast minus the aim. The
  grey wall's own cast (`tint`) now lies on the blackbody locus (green `e^(−0.14 c)`), as a real light does: without
  the green shift a strong cast sits off the locus and the skin guard of the estimator rightly distrusts it.
- **Clock:** the test loop advances simulated time frame by frame and calls `tick` every 100 ms.

### 6.2 Cases the controller must pass (`cargo test -p better-cam auto_controls`)

| # | Case | Pass criteria |
|---|---|---|
| 1 | Dim room, F = 30, needs `E = −5` and ≈ 1.5 stops of gain; cold start `E = −6, G = 0` | Within ±0.33 stop of target in ≤ 3 s; `E` never above −5; sim fps ≥ 29.5 for every 1 s window after the first write; ≤ 15 writes to converge; 0 writes in the next 60 s. |
| 2 | Step +2 stops (light on), then −2 stops | Back within the band in ≤ 2 s each; overshoot past target ≤ 0.5 stop. |
| 3 | Darker than the limits allow | Ends at `E_cap`, `G_cap`; state `limit_gain` within 5 s; no writes afterwards; fps held. (Run with `Longest exposure` at the frame-rate limit; with `fit` and more than a stop dark it ends at `E_ceil`, 2.5.12 and case 25.) |
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
| 25 | Lowering the frame rate (2.5.12), owner's settings (F = 30, Most gain 100, Fast): (a) 2 stops too dark at `E = −5, G = 100`; (b) a lit room; (c) dark, then bright; (d) `Longest exposure` at −5 | (a) `E = −4` after 3-5 s, never above, error ≤ 1.0 stop (≤ 0.5 when ≤ 1.4 stops short), fps < 20, no learned cap; (b) `E ≤ −5`, `holding`, fps ≥ 29.5; (c) `E` back at −5 with gain not yet below its final value, never −4 again, 0 writes in the last 20 s; boundary wobble: ≤ 1 exposure write in 150 s; (d) `E = −5`, `G = 100`, no write, fps ≥ 29.5. |

Notes W3 (2026-11-19): cases 15-17 run twice, on the grey wall and in the coloured office (B6). Case 15's text of the
`at_limit` state (case 16) is the new one of 4.2. In the office the estimator has a constant error of 4 mired
(`ESTIMATOR_BIAS = 0.042` in `ln(B/R)`, AUTO-WHITE-BALANCE.md 3.5): case 15 there allows a settle of up to 12 mired
before the light changes and judges `|e_wb| < 0.03 + 0.042`. New cases B1-B8 and the lifted black: AUTO-WHITE-BALANCE.md
5.2 and the W3 note in 6.

Notes V2 (2026-11-19, AUTO-WHITE-BALANCE.md 10.6): the cases above are unchanged. New tests of the slow lane, on the
recorded desk: `a_face_that_stays_in_doubt_is_followed_slowly` (the doubtful 1.4 s of the recording played forward and
back on a camera at 6500 K: `slow` with its text by 3 s, no write before 5 s, every write toward a higher mired, the
middle of the two answers within 0.07 at 150 s, no hand-back, at Slow, Normal and Fast),
`a_face_that_stays_in_doubt_at_its_kelvin_stays_close` (the same from 6261 K: at most 3 writes and 15 mired in 180 s,
one way, no hand-back), `a_picture_with_two_answers_says_so` (meter, `TwoAnswers` / `OneColour` / `Ok` / `Dark`), and
the warmth tests of V1 (`a_small_warmth_change_moves_the_camera_at_once`, `warmth_applies_while_the_picture_is_one_colour`,
`warmth_applies_while_the_picture_reads_two_ways`, `a_warmth_drag_is_not_a_swing`). Case
`a_face_in_a_dim_room_does_not_make_the_loop_hunt_or_hand_back` is unchanged (at most 1 write after 20 s).

Notes 10-13 (2026-11-19): the cases above are unchanged. The device exposure meter's subject terms have their own
tests in `meter.rs` (M1-M25: even rooms at every target, the recorded desk, windows, lamps, skin tones, the knee, the
source levels, NaN coverage) and in `tests.rs` (L1-L30: backlit and lamp-lit rooms, bright walls, someone leaning or
walking in front, the lit hold, the high-target rests). Their cases and thresholds are in 10.8 / 10.10 (backlight),
11.9 / 11.11 (front light), 12.8 / 12.9 (knee), 13.8 / 13.10 / 13.11 (one subject reading, spread fade, loop audit) and
13.12 (source levels that follow the target).

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
- **Done 2026-11-19 (section 9).** One constant changed (9.4). The room was dark, so checks 2-5 and 10 and the gain
  curve are owed to the owner with exact steps (9.6); the follow-ups F1-F6 (9.5) wait for the owner's agreement.

---

## 8. Open questions for the owner

| # | Question | Recommendation |
|---|---|---|
| 1 | Default **Most gain**: the full range, or a cap for noise? | 75 % of the range (75 on W4GS). You can raise it; A6 shows you the noise at both. |
| 2 | Default **Longest exposure**: fit the frame rate, or allow the camera to drop to half rate in very dim light? | ~~Fit the frame rate. Full frame rate is the reason for the feature.~~ **Reversed (live finding, owner's room, 2.5.12):** fit the frame rate, **except** that a picture still more than a stop too dark at full gain may go one stop past it (30 → about 16 fps), as the camera's own auto does; the green, grainy picture at full gain cost more than the frame rate. Picking a stop at the frame-rate limit in the list never lowers the rate. |
| 3 | Should the exposure target be **shared with Auto Adjust's target**? | No. Separate, same scale, same default; Auto Adjust is per chain and in presets, this is per camera. The card shows Auto Adjust's target when both are on. |
| 4 | **Metering area:** the whole camera picture (centre-weighted), or only the part the output shows after Crop / Scaling? | Whole picture in v1. Cropped metering needs the chain's geometry in the meter; revisit if you crop a lot. |
| 5 | **Hand-back target** when the watchdog gives up: Camera auto, or your last manual value? | Camera auto where the camera has it: a correct picture at a lower frame rate beats a frozen wrong one. |
| 6 | Should the default for a **new camera** become Better Cam auto exposure? | No (brief: nothing changes until you pick it). Reconsider after A6 shows it beats Camera auto on W4GS. |
| 7 | Mode labels: "Manual / Camera auto / Better Cam auto"? | Yes. "Auto" alone becomes ambiguous once there are two. |
| 8 | Feed-forward from the device loop into Auto Adjust (so Auto Adjust does not follow each device step for 0.5 s)? | Not in v1. Build it only if live check 10 shows visible double movement. **A6: undecided, check 10 is owed (it needs a lit scene); nothing seen so far argues for it.** |
| 9 | Finer exposure: try the Windows extended exposure control (100 µs units) to escape whole stops and mains banding? | Later, as a spike. Whether UVC cameras on the inbox driver expose it is **unverified**. |
| 10 | Should Better Cam auto also be offered for **focus** (software contrast autofocus)? | No. Camera auto focus exists on W4GS; a software focus sweep is a separate feature with visible hunting by design. |

---

## 9. Live results (A6, 2026-11-19)

Debug build of `main` at 8349a5c (the first runs) and with the one A6 change of 9.4 (the last run), W4GS (`vid_0c45&pid_636d`), 1280×720 MJPEG at 30 fps as the
owner had it set (6.3 says 1080p; the owner's saved format was kept), `better-cam.exe` in test mode, driven through
the webview only. Everything the run changed on the camera and in the saved settings was put back (9.7).

### 9.1 The light, and what it means for the checks

The scene was **dark**. The picture at any exposure from 1/8192 s to 1/32 s and gain 0 is one flat value: display luma
**8.00** (the camera's black level, 0.00243 linear) in every frame, no signal (frame-to-frame and pixel-to-pixel
difference ≈ 0.003-0.04). With the camera's own Auto (as found) the picture is luma 9.6-10.5 with a faint red-tinged
speckle. Either the lens was covered or the room was dark, as in A4's run. That decides what can be measured:

- Better Cam auto exposure can never reach its target here (a black picture needs far more light than E = -5 with
  gain 75 gives): it always ends in `limit_gain`. Checks that need the loop to *hold* a picture, or to react to light, are **owed**.
- The exposure meter never has confidence (`conf < 0.2`), so nothing was ever learned: `learned` stayed empty.

### 9.2 Checks 1-10 of 6.3

| # | Check | Measured | Verdict |
|---|---|---|---|
| 1 | Frame rate, 30 fps | Footer/stats capture fps over 300 s with Better Cam auto: **min 31.73, mean 31.97** (the camera runs at its exposure-limited 32 fps at 1/32 s). First 60 s: min 31.73, mean 31.97. Camera auto in the same room: 15.8-16.7 (mean 16.7). | **Pass** (≥ 29.5) |
| 1 | Frame rate, 60 fps | Before A6: E = -6 (1/64 s, the 60 fps cap) gave **55.1 fps (min 54.55)** for 60 s: **fail** (< 59). The learned cap never fired: 55.1 is 92 % of 60, the guard waited for < 90 %. After the constant change (9.4): the cap `60 → -7` was learned 6.7 s after the camera opened (the last gain write ended at ≈ 2.6 s; the guard needs 1 s without writes and 3 s of low rate, plus the 0.7 s exposure write), `auto-status` carried `learned: {rate_caps: {"60": -7}}`, and the rate was **61.46 fps (min 60.52)** over the next 20 s. | **Pass after the change** (the cap engages about 4 s after the guard may act, which is its own 1 + 3 s, plus the write itself) |
| 2 | Convergence from Manual -9 | In this room the band cannot be reached. Time to the limits: E: -9 → -5 and gain 0 → 75 in **3.9 s** (7 stops at one stop per decision, 0.55 s each: 250 ms gap + 0.15 s + 1 frame to apply); `limit_gain` shown at **5.4 s** (3 s of saturation, `LIMIT_AFTER`); picture luma 8.76 (Manual -9) → 17.6 (limits). First decision was a gain write (G 33), then E -8, -7, -6, -5, then G 66, 75. | **Not measurable here**; owed with light |
| 3 | No hunting, 5 min | **0 writes in 300 s** (log) and luma constant 17.0-17.6, fps 31.7-32.0 the whole time. Trivial: the loop sat at its limit. | **Pass, but not a holding test**; owed with light |
| 4 | Light switch | Not produced (needs the room light). | **Owed**: see 9.6 |
| 5 | White balance, warm lamp | Better Cam auto white balance engaged on the card: `starting` → `holding · 4600 K` in 0.9 s, **no write ever**, K stayed 4600. Honest but empty: gray world has nothing to measure in a black picture (it holds when `conf_wb < 0.3`). | **Owed**: see 9.6 |
| 6 | Reopen (Stop → Start) | Through the card, exposure + white balance in Better Cam auto: LIVE again in **0.76 s**; the log shows `[engine] stored controls applied after open: exposure=-6, gain=0, white_balance=4600` **before** the first `[auto]` write; the mode survived (`get_camera_controls`: `mode: app` for exposure, gain and white balance). The stored resume point was the seed (-6, 0), because the loop was never in `holding` (see 9.5, F2): the loop climbed again to -5 / 75 in 1.7 s and showed `limit_gain` 4.6 s after the click. Without a stored record (camera keeps its values) no frame was more than 25 % off the pre-stop luma (272 frames, first frame 702 ms after the click). A flash cannot be judged on a black picture. | **Pass for the write order and the mode; flash owed with light** |
| 7 | Mode changes | Better Cam auto → Manual: returned in 59 ms, device E -5 and gain 75 kept as Manual values, luma 17.03 → 17.03 (linear 0.00562 → 0.00563), fps 32.0: **the picture is kept**. → Camera auto: Auto flag on, fps fell to **16.6** as expected, gain stays at its last driven value (75) as Manual, luma 18.5 with speckle. | **Pass** |
| 8 | Noise cost | See 9.3. | Described |
| 9 | First-write latency | The runner logs decision writes only: 2-27 ms at 30 fps. The writes of `apply_on_open` are not timed in the log. Indirectly, after a Start click at 30 fps: first `controls-changed` at 0.75 s (open), the second, after the stored controls were applied, at **1.72 s** (≈ 1.0 s after LIVE for the exposure, gain and white balance writes together). No decision write came before the `stored controls applied` line (log order on the Stop → Start run). At 60 fps the gain writes right after a reopen took **259-669 ms** (two runs; 2-27 ms at 30 fps), the exposure write that set the learned cap 680 ms; later manual gain writes 3-4 ms. A manual write batch (gain, then exposure) at 60 fps took 0.83-1.09 s for E = -7, -6, -5 but 30-33 ms for -9 and -4. | **Partly measured**; a per-write time for the first write after open needs a log line (follow-up F5) |
| 10 | With Auto Adjust, step the light | Not run: needs light, and a dark picture gives Auto Adjust nothing to measure either. | **Owed**; open question 8 is therefore **undecided** (nothing seen argues for feed-forward; do not build it before check 10) |

### 9.3 W4GS gain curve and noise cost

**Gain curve (`S`).** Not measurable in this room. At E = -5 the manual gain steps give luma 8.00, 11.02, 14.02, 17.02,
20.03 at G = 0, 25, 50, 75, 100: a straight **+0.12 display-luma per gain unit** lift of the black level, with no
signal to amplify; the exposure steps E = -13 … -5 at G = 0 gave an identical 8.00. That is a pedestal, not a gain
curve (it would read as 1.5 stops over the range in linear terms, which means nothing). The controller's `S` stays at
its **initial 3.0, unverified**; the gain model never learned (`conf` never reached 0.8). To estimate it, run in a lit
scene: Manual exposure at a fixed value, gain 0 / 25 / 50 / 75 / 100, mean linear luma of the preview for each, `S` =
log2(signal at 100 / signal at 0) (steps in 9.6).

**Noise cost (screenshots, same scene).** Better Cam auto at 30 fps (E -5, gain 75, 31.7 fps) against Camera auto
(≈ 16.6 fps, gain 0) in the dark scene:

| | Better Cam auto, 30 fps | Camera auto, ≈ 16.6 fps |
|---|---|---|
| picture | flat dark gray, luma 17.0-17.6, no visible grain | black with a faint red-tinged speckle, luma 9.6-10.5 |
| pixel-to-pixel difference (mean abs. of neighbours, 0-255) | 0.09-0.11 (preview frames), 0.39 (screenshot) | 0.74-0.82 |

So in the dark scene Better Cam auto was **not noisier**: its picture is a lifted flat black (gain 75 lifts the black
level from 8 to 17 of 255) while the camera's own auto shows grain from its own processing at about 1/16 s. The
"1.4-2× the noise" of section 1 point 5 is **unmeasured**, because there is no signal to put noise on; it needs the lit
scene of 9.6. Screenshots are kept by the A6 session (`a6\s1-a-camera-auto.png`, `a6\s1-c-bettercam-30fps.png` under
the session temp folder), not committed.

### 9.4 Constant changed

| Constant | Old → new | Why |
|---|---|---|
| `GUARD_LOW` (`exposure.rs`, 2.5.8): capture rate below which the frame-rate guard counts | `0.9 · F` → `0.95 · F` | W4GS at 60 fps: the camera's frame time is about **1.16 × the exposure** (1/64 s gives 55.2 fps, 1/32 s 27.6, 1/16 s 13.8; 1/128 s gives 61.5 and 1/512 s 61.3). `E_rate = floor(log2(0.95 / F))` allows 1/64 s, which holds only 92 % of 60. At 30 fps the frame time is the exposure itself (1/32 s gives 31.7-32.2 fps), so `E_rate` is right there. `0.9` never fired at 55 fps and the rate stayed short of 59; `0.95` learns the cap `-7` in about 5 s and gives 61.5 fps (9.2, check 1). Recovery stays at `≥ 0.95 · F`. A camera sitting at 0.94 · F for another reason costs one try: one stop less exposure for at most 3 s, then the cap is removed and not tried again (case 9). Test: `case_08b_rate_a_little_low_still_learns_a_cap` (sim now has `rate_drop_fill`; it fails with 0.9). |

No other constant is contradicted by the live data. `E_rate`'s `0.95` stays (changing it would cost a stop of light at
30 fps for nothing).

### 9.5 Follow-ups found in A6 (all six done afterwards: see the table below the list)

- **F1, Most gain "Off".** With Most gain at its minimum the loop switches to whole stops, but the device gain stays at
  its last driven value (75 here) instead of going to its minimum, and the status says "this camera has no gain"
  (`limit_exposure`), which is wrong for a camera that has one. 2.5.2 says Off means gain at its minimum; the text of
  4.2 is for cameras without a gain control. Needs a gain write to `G_min` on the change and its own text.
- **F2, resume point in limit states.** The resume point is stored only on entering `holding` (2.5.11), so a camera
  that is at a limit (dark room, `limit_gain`) reopens from the seed (-6, 0) and climbs again for about 2 s. Store it
  in the limit states too.
- **F3, option text.** "1/16 s (camera drops to ≈ 15 fps)" measured **16.2 fps** (min 15.9); `1/exposure` is 16.
- **F4, white balance in a black picture** says `Holding · 4600 K` (it holds when `conf_wb < 0.3`). A4 saw `starting`.
  Harmless; a "nothing to measure" text would be more honest.
- **F5, log line for the apply.** `apply_on_open` writes are not timed. One log line with each write's milliseconds
  would let check 9 be read directly.
- **F6, camera facts for the owner.** Camera auto at **60 fps requested** gives **14.3 fps** in this room; at 30 fps
  16.5. W4GS answers an exposure write at 60 fps in 0.8-1.1 s for some values (the loop copes: `gap` grows with the
  write time, 2.9).

**Follow-ups F1-F6 done (2026-11-19, after the A6 run).** Each is in the section named; tests are in
`auto_controls/tests.rs` (F1, F2, F4), `status.rs`, `camera_controls.rs` and `src/camera/schema.test.ts`.

| # | Change | Where | Live proof (W4GS, same dark room, debug build, test mode) |
|---|---|---|---|
| F1 | Most gain "Off" drives the device gain to `G_min`; status "gain is off" | 2.5.2, 4.2 | Most gain → 0 with the loop at gain 75: device gain read **0** in 3.5 s, one gain write; after 9 s `limit_exposure` "Exposure at the 30 fps limit and gain is off. Add light, allow more gain, or allow a longer exposure."; Most gain back to 75 → gain 75, `limit_gain` |
| F2 | The resume point is also stored 3 s into `limit_gain` / `limit_exposure` / `too_bright` / `at_limit`, once per 10 s | 2.5.11 | The stored record read `exposure -5, gain 75` (A6: the seed -6 / 0). Stop → Start: LIVE in 0.77 s, `stored controls applied after open: exposure=-5, gain=75`, **0 `[auto]` writes** after the reopen (A6: a 1.7 s climb), `limit_gain` shown 4.4 s after the click (3 s of saturation, as before) |
| F3 | Longest exposure text is `min(F, 1/τ)`: "1/16 s (camera drops to ≈ 16 fps)" | 2.5.2, 4.1 | Unit test (the measured 16.2 fps of 9.5) |
| F4 | White balance with `conf_wb < 0.3` is `no_signal`: "Nothing to measure (too dark). Keeping 4600 K." (warning dot) | 2.6, 4.2 | Exposure Manual -13, gain 0 (the camera's black level): `no_signal`, the card shows the text with the warning dot. **Not changed:** the A6 picture (E -5, gain 75: a flat lifted black, display luma 17) still reads `Holding · 4600 K`, because `conf_wb` is high there (computed, not logged: the trusted-cell weight at coded luma 0.067 is 15 % of the total, above the 12 % where `conf_wb` is 1) and the gray-world error of a flat grey is 0. Whether a flat lifted black should count as "too dark" (a mean-luma gate in `measure_white_balance`) was left open: it changes the 2.4 meter. **Done in W3** (2026-11-19, `MIN_MEAN_LUMA = 0.10`, 2.4): the lifted black is `no_signal` "too dark" (tests `a_lifted_black_is_dark_not_a_neutral_grey` and `a_lifted_black_says_too_dark_not_holding`, which fail without the gate) |
| F5 | One `[apply] t=… <key> <value> -> <read-back> (… ms)` line per write of `apply_on_open` (`t` = seconds since the re-apply began; also `auto on/off` and `failed`) | `camera_controls.rs` | After the reopen: `[apply] t=0.0 exposure auto off -> -5 (17 ms)`, `exposure -5 -> -5 (5 ms)`, `gain auto off -> 75 (22 ms)`, `gain 75 -> 75 (2 ms)` (the camera was warm; the 200-700 ms first write did not recur in this run) |
| F6 | Slow-camera hint under the exposure row in Camera auto, with a button to Better Cam auto | 4.1 | Camera auto at the saved 30 fps (1280×720 MJPEG Custom 30): hint "Camera auto is slowing the camera to 17 fps in this light. Better Cam auto keeps 30 fps." with "Use Better Cam auto"; the button switched exposure to Better Cam auto and the hint went. The 60 fps case was not run (the saved format was kept); the same code path with `requestedFps` = 60 |

**A5's card, checked live (all as designed unless noted).**

| Item | Result |
|---|---|
| Three modes on exposure and white balance | Manual / Camera auto / Better Cam auto, all enabled; focus and gain have no mode select (focus keeps its Auto switch) |
| Optimistic state, then the backend's answer | Choosing Better Cam auto showed `mode: app`, the disabled slider and "Adjusting…" at **110 ms**; the backend's seed (1/64 s) followed at 160 ms and `auto-status` `starting` at 170 ms. After a Start: "Starts when the camera opens." (grey) → "Adjusting…" at 762 ms. The first write after open was not slow in this run (30 fps), so the optimistic gap was short |
| Gain follows exposure | Gain row disabled, value 0 → 33 → 66 → 75 in step with the log, hint "Set by Better Cam auto exposure" |
| Persistence | Record in `localStorage`: `exposure {value -6, auto false, mode "app"}`, `gain {…, mode "app"}`, `white_balance {value 4600, mode "app"}` and `auto {exposure {target .45, longest "fit", max_gain 75, speed "normal"}, white_balance {warmth 0, speed "normal"}}`; after Stop → Start the log wrote `exposure=-6, gain=0, white_balance=4600` and the modes were still `app` |
| Status line and dot | Neutral (teal) while adjusting, grey when closed, **warning (amber, `rgb(255,184,77)`)** at `limit_gain` with the text "Gain at its maximum (75). Add light, allow more gain, or allow a longer exposure." The dot colour follows the text with a fade of about 0.3 s. Error colour not produced (no hand-back without a faulty camera) |
| Limit markers | Exposure tick "30 fps limit" at 0.667 of the range (E -5 of -13…-1), range beyond shaded; gain tick "Most gain: 75" at 0.75. With Longest exposure 1/16 s the exposure tick reads "Your limit" at 0.75 and the camera ran at 16.2 fps |
| Longest exposure options | "Fit the frame rate (1/32 s at 30 fps, one stop longer in a dark room at most gain)", 1/8192 s … 1/64 s "(sharper motion, more gain)", "1/32 s (the frame-rate limit)", "1/16 s (camera drops to ≈ 15 fps)", 1/8 s ≈ 8 fps, 1/4 s ≈ 4 fps, 1/2 s ≈ 2 fps (see F3) |
| Most gain "Off" | Shows "Off" at the minimum; behaviour wrong, see F1. Double-click resets it to 75 |
| Reset all | Exposure, gain and white balance went back to Camera auto / manual 0 / Camera auto, every Better Cam auto mode ended, `controls` values cleared, **`auto` kept** with the changed `max_gain`; `learned` did not exist to keep (nothing was learned) |
| Not produced | `conflict`, `ignored`, `no_response`, `not_answering`, `hunting` (need a faulty camera or another program meddling); the sim covers them (6.2 cases 18-22) |

### 9.6 Owed to the owner (needs light or a real-world action)

Run with the camera in the lit room, preview visible, Better Cam auto exposure on, Auto Adjust off unless stated. The
log lines are `[auto] t=… exposure -5 -> -5 (… ms)` in the app's stderr.

1. **Check 2/3 (convergence, no hunting):** with the room lit (a normal evening lamp), set exposure to Manual -9 in the
   card, then pick Better Cam auto. Expect `holding` within 3 s. Leave it 5 minutes; count the `[auto]` lines
   (≤ 30) and watch the preview for pumping.
2. **Check 4 (light switch):** while it holds, switch the room light off and on again, twice. Expect the status to go
   to `adjusting` and back to `holding` in ≤ 2 s each way, and no frame fully black or white for more than 0.5 s
   (record the screen).
3. **Check 5 (white balance):** exposure and white balance both on Better Cam auto, a warm desk lamp moved into the
   scene. Expect `holding` at a lower Kelvin in ≤ 6 s with no colour swinging.
4. **Check 10:** add Auto Adjust with exposure and white balance on, repeat steps 2 and 3, and note any visible double
   movement (the picture moving twice for one light change). That decides open question 8.
5. **Gain curve `S` and the noise cost:** in the lit scene, Manual exposure -5, gain 0 / 25 / 50 / 75 / 100, a few
   seconds each, a screenshot of each; `S` = log2 of the brightness ratio between gain 100 and 0 in linear light (or let
   Better Cam auto run: after about 20 gain moves `learned.gain_stops` is in `localStorage["better-cam"]`). Compare a
   screenshot at 30 fps Better Cam auto with one in Camera auto (≈ 15 fps) for grain.
6. **Check 6 flash:** with the lit scene, Stop → Start and look for a flash longer than 1 s.

**State line in the log (added after "the picture turns green, dark and noisy").** While the exposure loop runs, the
app's stderr gets one line every 5 s (only while the camera is open and the loop is not paused) next to the `[auto] t=… exposure -5 -> -5` write lines:
`[auto] t=84.2 exposure LimitGain E=-4 G=100 cap=-5 (Fps) ceil=-4 over=true maxG=100 S=2.88 err=+0.90 conf=1.00 clip=0.00 fps=15.9/30`.
`err` is the meter's reading in stops (positive: the picture wants more light), `cap` the longest exposure the frame
rate allows (`Fps`, `User` or `Learned`), `ceil` the longest the loop may use with the frame rate lowered and `over`
whether it does (2.5.12), `maxG` the gain limit, `fps` the measured/requested rate. A dark, grainy picture
with `LimitGain`, `E` at `ceil` (or at `cap` when `ceil` is the same) and `G` at `maxG` is the loop at its limits (the
room needs more light than that exposure and gain give: see 2.5.3, 2.5.9 and 2.5.12); `err` near 0 means the loop is on
target and the cause is elsewhere. Test: `the_log_line_says_where_the_loop_is_and_what_it_saw`;
`owner_settings_settle_inside_the_limits_and_sit_at_them_outside` pins the loop's behaviour with the owner's settings
(Most gain 100, Fast, 30 fps) over room brightnesses 10 stops apart: it settles inside `E_rate` and full gain, stays at
`E = -5, G = 100` up to a stop short of it, and goes to `E = -4` beyond that (2.5.12).

**To check on the real camera after 2.5.12:** in the dim room the `[auto]` line should show `over=true`, `E=-4`,
`fps≈16/30` and `err` under about 0.5 at `G` below 100 or at most 1 stop at `G=100`; with the light on again, `over=false`,
`E=-5`, `fps≈30`; no `rate_caps` is learned from the low rate (`learned` unchanged); and the same room with `Longest
exposure` set to 1/32 s must give the old `E=-5, G=100` at 30 fps.

### 9.7 What the run changed, and how it was put back

Exposure and white balance were left in Camera auto (as found: exposure Auto on, white balance Auto on at 4600 K, focus
Auto on), gain back to Manual 0, every other control untouched (read back after each segment: all 12 controls
matched the "before" list by value, Auto flag and mode; focus is the camera's own autofocus and moves by itself).
`localStorage["better-cam"]` was written back to the saved 787-byte string and read back identical in a fresh launch.
The presets folder was empty and stayed empty. FPS was set to 60 for two runs and back to 30.

### 9.8 Auto Adjust's contrast on hard-edged pictures: known limit, not fixed

Auto Adjust's contrast lands short of its target on hard-edged pictures (0.478 for a target of 0.50, measured live
earlier). The cause is in the design, not a constant: the filter predicts the spread from the **16 × 16 cell means**,
and detail inside a cell (hard edges, texture) makes the curve act weaker on the real pixels (about two thirds of the
predicted effect on flat areas with hard edges, as `fx/auto_adjust.rs` documents) while the loop deliberately does not
look at its own output. A fix is not small and clear: scaling the aim by a constant would overshoot soft pictures,
and measuring the filter's output is a second statistics pass and a closed loop (the design says no). The 0.022
residual is inside what the slider's step shows; left as a known limit. No change to `fx/auto_adjust.rs`.

---

## 10. Backlight: subject-priority metering (design, 2026-11-19)

Status: design, not built. Scope: the device exposure meter (`auto_controls/meter.rs`) and what the exposure loop
shows of it. The Auto Adjust filter's metering (`fx/auto_adjust.rs::exposure_target`) is out of scope and shares no
code with this change (10.8).

**Owner's case.** One W4GS at a desk; light goes from a dark room with an orange lamp, to a bright room with yellow
light, to daylight from a window **behind** the owner. Preset: Better Cam auto exposure (target 0.45, Longest
exposure "fit", Most gain 75 %, Normal), Better Cam auto white balance (warmth +0.2, Normal), Auto Adjust off. With
the window behind, the face goes dark. Wanted: the natural look of "an actual automatic compensation" in all three.

### 10.1 Recommendation

Keep today's reading and add a bounded **backlight term** on top of it. Per frame, from the same 16 × 16 statistics:

1. Meter a **subject zone** (a soft oval a little below the middle of the picture) on its own.
2. Count the **light sources**: cells outside the zone, in the upper part of the picture, that would be white with
   the zone on target (or are white now).
3. When light sources cover enough of the picture (gate), raise the reading toward the zone's reading, never above
   the exposure at which more than 45 % of the picture would be white, and never more than 3 stops above today's
   reading.

In every scene without a light source the gate is shut and the reading is today's, bit for bit. The change removes
the loop's second, dark resting point in a backlit picture (10.2); it does not move the good one. No new statistics,
no GPU work, no dependency; about twice today's meter arithmetic on the controller thread.

### 10.2 Root cause, with numbers

Numbers in this section come from a scratch model (not in the repo) that copies `measure_exposure`, the simulated
camera's in-cell texture and clipping (6.1) and the reflectances of `fx::awb::scenes`, and drives an idealised loop:
Normal (`k` 0.7, `step_max` 1), bands 0.33 / 0.12, no noise unless stated. The scene is the kit's **office** (light
skin, blue shirt, grey wall) with a **window** that replaces the wall where the person does not hide it: *top and
sides* (rows 0-5, and columns 0-2 / 13-15 down to row 11: 33 % of the centre-weighted picture), or *big* (rows 0-9:
38 %). "Window +5" means the window is 5 stops brighter than the face would be under the same light. Brightness is the
mean coded luma of the face cells on the 0-255 scale; the same room without the window rests with the face at
**114/255**.

**The cause is not the mean as such but the near-white weight.** Today a cell weighs in full up to coded 0.93 and
0.15 from 0.99 (2.4), judged on the picture **as it is now**. A window just under white counts in full; the same
window once white counts 0.15. So the reading of a backlit picture depends on which side of white the window is, and
the loop has two resting points. Office, window +5, *top and sides*, today's reading at each exposure (stops are the
face's offset from the windowless rest):

| Face offset | −4 | −3 | −2.5 | −2 | −1 | 0 | +0.5 |
|---|---|---|---|---|---|---|---|
| Face (/255) | 26 | 40 | 48 | 58 | 82 | 114 | 134 |
| Window (/255) | 157 | 215 | 245 | 255 | 255 | 255 | 255 |
| Today's error (stops) | +1.15 | **+0.52** | +1.72 | +2.14 | +1.20 | +0.20 | −0.26 |
| New error (stops) | +3.55 | +2.55 | +2.05 | +2.14 | +1.20 | +0.20 | −0.26 |

With the *big* window the dip is **+0.25** at −3 stops, inside the hold band (0.33). A loop that comes from the dark
side (camera open from the seed −6 / 0, a resume point from a darker hour, daylight rising in the morning) creeps up
in 0.2-0.35 stop steps and stops in that dip: the window just under white, the face 2-4 stops dark. A loop that
comes from the bright side lands on the good point (window white, face +0.1 to +0.4). Idealised loop, rest point
(face offset in stops, face /255, centre-weighted clipped share):

| Scene (window +4 / +5 / +6) | Today, from dark | Today, from bright |
|---|---|---|
| Office, top and sides | −1.8 (63) / +0.1 (120) / +0.1 (119) | +0.3 (127), clip 0.33 |
| Office, big | −2.0 (57) / −2.8 (44) / −4.2 (25) | +0.4 (131), clip 0.38 |
| Warm room, top and sides | −2.1 (54) / −2.9 (40) / −3.9 (27) | +0.1 (116), clip 0.41 |
| Warm room, big | −2.3 (51) / −3.1 (37) / −4.4 (21) | +0.2 (120), clip 0.45 |
| Office, dark skin, top and sides | +0.4 (82) / +0.3 (81) / **−5.6 (4)** | +0.5 (87), clip 0.33 |

**Daylight rising** (noisy loop, warm room, big window going from +1.7 to +6.7 stops over the face in 120 s): today
follows the window down: the face is −1.4 stops at 36 s, −2.9 at 84 s and **−5.8 (black) at 120 s**. That is the
owner's report. With the window instead white from the start (the bright side) the face is fine, which is why the
fault looks random.

Two more facts shape the fix. (1) On the deep resting point the room is under coded 0.08, where today's not-black
weight drops it: the picture has **nothing lit but the window**, so any rule that compares the window with "the rest"
sees no rest. (2) From target (0.45) to white (0.97) is 2.45 stops in linear light: a fix capped below that cannot
take a window that the loop holds at target past white.

### 10.3 Options considered

| Option | Result in the model | Verdict |
|---|---|---|
| A. Meter a centre / lower-centre subject zone always | Changes every scene whose middle differs from the whole: dark-skin office +0.43 stop, the sim's grey wall −0.17, a face filling the frame −0.26 | Rejected as the rule (breaks "even scenes as today"); kept as the floor under a gate (F) |
| B. Weighted median or percentile instead of the mean | Not built: it moves every scene too, and the window's weight still flips at white, so the two resting points stay | Rejected |
| C. Weight light sources like white cells by their ratio to the subject zone (exposure-invariant) | Fixed office *top and sides*; left *big* (window +5 / +6 over the wall) at −3.7 / −6.5 stops (on the deep point the zone is black, so the "subject level" is the window itself); a lamp-lit face beside the middle is 3-4.7 stops over the walls and became "glare": the recorded desk read **+0.35 stop** more | Rejected |
| D. Two-zone blend gated by "zone needs more than frame", capped at 2 stops over today's reading | Deep points lifted by 2 stops and no more (big, window +6 over the wall: −6.5 → −4.3): the cap is measured from today's reading, which sits 4-6 stops low there | Rejected in that form; F keeps its zone floor, replaces its gate and widens its cap past 2.45 stops |
| E. Drop the near-white down-weighting (plain mean: one resting point) | One resting point, the bad one: face −1.8 to −3.0 stops in every backlit scene | Rejected |
| F. Subject-zone floor, gated by light sources, capped in stops and by a clip budget | 10.5 | **Recommended** |

### 10.4 The design

Notation as in 2.4: cells with coverage `cov_i ≥ 0.02`; `lin_i` linear luma; `y_i = srgb_encode(lin_i)`;
`c_i = centre_i · cov_i`; `(u_i, v_i)` the cell centre in −1..1 (`v` grows downward, as in `centre_weight`); `T` the
target; `ss` = `smoothstep`. `e_all`, `conf` and `clip` are today's 2.4 values, unchanged.

1. **Subject zone.** `z_i = exp(−(u_i² + (v_i − 0.1)²) / (2 · 0.4²))`: 1 a little below the middle, 0.5 about 3.8
   cells away, under 0.01 in the corners.
2. **Subject reading.** `e_sub = log2 k*` where `Σ z_i cov_i · srgb_encode(min(1, k* · lin_i)) / Σ z_i cov_i = T`,
   by today's bisection (±4 stops, 24 steps). No near-black or near-white factor: the weights do not depend on the
   exposure, so `X + e_sub` does not move when the loop moves (one answer), and a black subject under a bright
   window still counts (fact 1 of 10.2).
3. **Light sources.** For each cell
   `src_i = max(ss(0.90, 0.97, srgb_encode(min(1, 2^e_sub · lin_i))), ss(0.93, 0.99, y_i))` (white with the zone on
   target, or white now), placed by `p_i = (1 − z_i) · (1 − ss(0.0, 0.4, v_i))` (outside the zone; full above the
   middle row, none below row 11). Share `G = Σ c_i p_i src_i / Σ c_i`. Gate `a = ss(0.10, 0.20, G)`.
   "White now" keeps `G` from falling when the window turns white (a white cell's `lin` is only a lower bound), which
   would otherwise shut the gate on the good resting point and open it again below.
4. **Clip budget.** `e_clip` = the largest `e` at which the centre-weighted share of cells with
   `srgb_encode(min(1, 2^e · lin_i)) ≥ 0.95` stays ≤ 0.45. Closed form: sort cells by `lin` descending, add up `c_i`;
   at the first cell `j` whose sum passes `0.45 · Σ c_i`, `e_clip = log2(srgb_decode(0.95) / lin_j)` (+4 when none).
5. **Reading.** `b = a · clamp(min(e_sub, e_clip) − e_all, 0, 3)`; `error = min(e_all + b, 4)`.
   `ExposureReading` gains `backlight = b` (stops) and `sources = G` (for the log and the status flag).

| Constant (`meter.rs`) | Value | Why |
|---|---|---|
| `SUBJECT_V` | 0.1 | Zone centre a little below the middle: head and shoulders of a seated user. |
| `SUBJECT_SIGMA` | 0.4 | Wide enough for a face that moves a little; the window beside the head stays mostly outside. |
| `SOURCE_ROWS` | (0.0, 0.4) | Windows and ceiling lights are in the upper part. Below the middle a bright patch is more likely a lit face or desk (the recorded desk: the face in the lower right, 10.5). |
| `SOURCE_AT_SUBJECT` | (0.90, 0.97) at target 0.45 | "White with the subject on target": 2.2-2.45 stops over the zone's level. Lit surfaces seldom are (model: `G` ≤ 0.007 in every even kit scene, dark skin in front of a white wall included). **Since 13.12 these are stops over the target, not fixed coded levels:** `SOURCE_AT_SUBJECT_STOPS` (2.206, 2.451), the same 0.90 / 0.97 at 0.45. |
| `SOURCE_WHITE_NOW` | (0.93, 0.99) | Today's near-white edges (2.4). |
| `BACKLIGHT_GATE` | (0.10, 0.20) | A corner window (the recorded desk: 0.11) or a lamp (0.05) opens it barely or not; a window behind the user (model: 0.26-0.36) opens it fully. |
| `BACKLIGHT_MAX_STOPS` | 3.0 | Most stops added over today's reading. Must exceed 2.45 (fact 2 of 10.2), else a window held at target never turns white. |
| `BACKLIGHT_CLIP_SHARE` | 0.45 | Most of the picture (centre weighted) the change may predict white: today's good resting point already holds up to 0.45 (warm room, big); 0.05 under the highlight guard (0.5). |
| `BACKLIGHT_CLIP_LEVEL` | 0.95 | Counted as white for the budget. Lower than the guard's 0.97, so the prediction errs to the safe side. |

**Bounds.** On every frame `0 ≤ error − e_all ≤ 3`: never darker than today, at most 3 stops brighter, and never past
the exposure at which 45 % of the picture would be white. At rest that 3 is mostly unused: the new resting point in
10.5 is today's own good one (the white window weighs 0.15 there, `e_sub` ≤ `e_all`, `b` = 0); the term only carries
the loop past white. Where it does stay on at rest (a window only 2-3 stops over the face, never white), it adds what
the zone needs, up to the budget and the 3 stops.

**Normal scenes.** Without a light source `G` ≤ 0.05 and `a` = 0: the reading is today's, exactly. Tolerance for the
build: rest exposure within 0.02 stop of today in the even scenes, and `|backlight|` ≤ 0.05 stop on every recorded
desk frame (model: 0.000 and 0.031).

**No pumping.** (1) `e_sub` and `X + e_sub` do not depend on the exposure; neither does `G`, except that cells turning
white only add to it. (2) A white window that gets brighter or dimmer changes nothing: it weighs 0.15 in `e_all` as
today, counts as a source, and sits at 1.0 in the zone mean. (3) The gate is a smoothstep, not a switch. (4) The clip
budget predicts with a lower threshold (0.95) than the guard measures (0.97) and 0.05 under its share, so the meter
never asks for a picture the guard steps back from; after a guard step (a sudden brightening) the next reading goes
back to the budget, not past it. (5) The loop's band, median of 3, `k`, `step_max` and W4 are unchanged. What can
remain is a pair of resting points around the gate's edge (a window only 1.5-2 stops over the face, `G` near 0.1-0.2):
a scene change then gives one move, not a cycle. Someone walking in front of the window changes `G` and the zone:
one move each way (test L5).

**Interplay.** The term is part of `ê`, so everything downstream follows it: a dim backlit room at full gain may
lower the frame rate (2.5.12, as for any picture over a stop too dark), the white balance loop waits while
`|ê| > 1` (2.6). W3 does not judge batches while `clip ≥ 0.05` (as today on the good resting point), and the gain curve
is not learned then (2.5.7). Auto Adjust, when in the chain with exposure on, meters its own input with today's
weights: on the new resting point (window white) it agrees within 0.2-0.3 stop; with a window that stays under white
it pulls back part of the boost. The owner runs it off.

**Cost.** Per sample: one more 24-step bisection over the zone, two passes for `G`, one sort of 256 values. About
twice today's meter, on the controller thread at ≤ 15 Hz; no GPU or `Stats` change.

### 10.5 Effect on the scenes (model)

Face offset from the windowless rest in stops (face /255), centre-weighted clipped share at rest.

| Scene | Today | New |
|---|---|---|
| **Backlit window**, office, top and sides, +4 / +5 / +6, from the dark side | −1.8 (63) / +0.1 (120) / +0.1 (119) | +0.1 (118) all three, clip 0.33 |
| Backlit, office, big, +4 / +5 / +6, from the dark side | −2.0 (57) / −2.8 (44) / −4.2 (25) | +0.2 (121-123), clip 0.38 |
| Backlit, warm room, top and sides, from the dark side | −2.1 / −2.9 / −3.9 | −0.1 to 0.0 (109-110), clip 0.41 |
| Backlit, warm room, big (window share 0.449, at the budget) | −2.3 / −3.1 / −4.4 | **−1.4 (69)** / +0.1 / +0.1 (114) |
| Backlit, dark-skin office, top and sides +6 | −5.6 (4) | +0.3 (82) |
| Backlit, any of the above, from the bright side | +0.1 to +0.7 | unchanged |
| Daylight rising +1.7 → +6.7 over 120 s, warm room, big (noisy) | −1.4 at 36 s, −5.8 at 120 s | −1.4 until 54 s (window +4 over the face), +0.1 from 60 s on |
| **Even room**: office, dark-skin office, warm room, the sim's grey wall, close-up, `G` = 0 | rest X | identical (0.000 stop) |
| **Dark room with a lamp in the picture**: warm room, a 2 × 2 lamp at +7 stops and a shade ring at +2.5 in the upper right (`G` 0.05) | rest X | identical (0.000 stop) |
| The **recorded desk** (`fx/awb/fixtures/real_room.txt`, 93 frames: dim room, window top left, lamp-lit face lower middle and lower right) | today's reading | at most **+0.031 stop** (`G` ≤ 0.114, gate ≤ 5 %) |
| **Face filling the frame** (kit `face_fill`) | rest X | identical (0.000 stop) |

The one shortfall is the budget's own price: a window that covers more than 45 % of the centre-weighted picture (about
the upper two thirds of the frame) cannot be made white by this change, so a loop on the dark side stays there (the
warm room *big* +4 row). The guard margin decides that number (10.9, question 2).

### 10.6 White balance in mixed light (warm lamp on the face, daylight window behind)

**No change in this step.** Reasons: (1) the estimator (`fx::awb`) already ignores near-white cells and cells with a
clipped channel; once the window is white (the new resting point) it stops voting for daylight, which today it did on
the dark resting point (window at 215-245/255). The exposure fix moves white balance toward the room and the lit face
by itself. (2) The mixed-light policy is the owner's decision D2 in docs/AUTO-WHITE-BALANCE.md 8 ("the light on most
neutral surfaces wins"); weighting the subject zone would change that policy, and the zone is mostly skin, which the
skin guard deliberately does not neutralise (D4). (3) Step 5 of that doc's live checklist (mixed light) has not been
run. **Later step, only if** that check shows an orange face with the window behind after this change: give the
estimator's cells a weight `1 + κ · z_i` (the same zone, `κ` about 1) in the *slow lane's* middle only, keeping the
skin guard; a separate design with its own tests.

### 10.7 Status and log text

- **Status (4.2), one new suffix**: while holding, when the backlight term is on, the text ends with
  " · backlight: exposing for the middle", e.g. "Holding 30 fps · gain 48 of 75 · backlight: exposing for the middle"
  (also on the "Lowered frame rate for light: holding …" text). Flag on when the median `backlight` of the last
  reading is ≥ 0.5 stop, off at ≤ 0.25 (hysteresis, so the polite live region does not chatter). It tells the owner
  why the window is white. Made in Rust (`status.rs`); the card shows `message` as it is, so **no frontend change**.
- **Log line (9.6)**: append `bl=+1.20 src=0.31` (the median `backlight` in stops and `G`).

### 10.8 Build plan (one session)

Files: `src-tauri/src/auto_controls/meter.rs`, `exposure.rs`, `status.rs`, `sim.rs`, `tests.rs`; this document (2.4
pointer, 4.2 row, 6.2 rows, a "Built" note under 10). Not touched: `fx/auto_adjust.rs`, `fx/awb*`, the shader, the
frontend.

1. **`meter.rs`.** Extract today's bisection into `fn solve(cells: &[(f64, f64)], target: f64) -> f64` (behaviour
   unchanged; `measure_exposure` uses it). Add the constants of 10.4, `fn subject_weight(i) -> f64`,
   `fn subject_error(stats, target) -> f64`, `fn light_sources(stats, e_sub) -> f64`, `fn clip_limit(stats) -> f64`,
   and the two fields `backlight`, `sources` on `ExposureReading`. `measure_exposure` computes today's value, then the
   term (skipped, 0, when `confidence` is 0 or there is no weight). Check: the existing meter tests and
   `cargo test -p better-cam --lib auto_controls` pass unchanged (136 today).
2. **`exposure.rs` / `status.rs`.** `Sample` and `Reading` carry `backlight` (median like `e`) and `sources`;
   `ExposureLoop` keeps a `backlit: bool` with the 0.5 / 0.25 hysteresis; `ExposureFacts.backlit`; the suffix of
   10.7; the log fields. Check: `the_log_line_says_where_the_loop_is_and_what_it_saw` extended with `bl=+0.00` in a
   normal room.
3. **`sim.rs`.** `SimCfg::window: Option<Window>` with `shape: WindowShape` (`Top` rows 0-5; `TopAndSides` rows 0-5
   plus columns 0-2 and 13-15 down to row 11; `Big` rows 0-9; `Wall` rows 0-12) and `level: WindowLevel`
   (`Const(stops)`, `Ramp { from, to, secs }`, `Wave { mid, amp, period }`, stops over the light-skin face). Kit
   scenes only (`office`, `warm_room`, `office_dark_skin`); window cells are the cells that are not skin, hair or
   shirt, neutral `rho`, `refl = luma(skin_light) · 2^stops(t)`. The scene is **calibrated before the window is
   added**, so `need` keeps its meaning (the windowless room on target). Oracle `Sim::stops_over_need() =
   eff_e + gs_true(eff_g) + log2 L(t)`: the face's offset from the windowless rest. Check: a test that a `Const`
   window leaves `stops_over_need` and the calibration unchanged at a fixed exposure.
4. **Tests** (below). First run L1, L2 and M4 against the old meter (before step 1, or with the term forced to 0) and
   record their failing values in the Built note: they must fail there.
5. **Docs**: constants, the Built note with the measured before/after values, 6.2 rows.

Tests (`auto_controls::meter` unit tests M, `auto_controls::tests` loop tests L; owner's preset: Normal, Most gain
75, Longest "fit", F = 30, `dim(-3.5)` unless stated):

| Name | Scene | Pass |
|---|---|---|
| M1 `even_rooms_have_no_backlight` | `scenes::camera` of office, office_dark_skin, warm_room, grey_room, closeup, face_fill, checker (model: `G` ≤ 0.007); the same scenes and the sim's grey wall from 1 stop under to 0.5 stop over their rest | `backlight == 0.0`, `error` identical to the old formula (`< 1e-9`) |
| M2 `the_recorded_desk_reads_as_before` | every frame of `real::frames()` | `backlight ≤ 0.05` |
| M3 `a_lamp_in_the_picture_is_not_a_backlight` | warm room, 2 × 2 lamp at +7 stops, 12-cell shade ring at +2.5, upper right, at rest | `backlight ≤ 0.02` |
| M4 `a_backlit_frame_asks_for_the_subject` | office, `TopAndSides` +5, face at −3 stops (window ≈ 215/255); then at +0.4 (window white) | first: old `error` ≤ +0.6, new ≥ +2.0; second: new = old within 0.01, `|error|` ≤ 0.35 |
| M5 `the_backlight_term_stays_in_its_bounds` | 200 seeded random frames (random window shape, level −2..+8 stops, exposure −4..+2) | `0 ≤ backlight ≤ 3`; when `backlight > 0`, predicted share at ≥ 0.95 at `error` ≤ 0.45 + 1e-6 |
| L1 `a_backlit_face_is_on_target_after_open` | office `TopAndSides` +5; office `Big` +5; warm room `TopAndSides` +5; seed −6 / 0 | after 8 s `stops_over_need` in [−0.5, +0.8]; `Snap.clip` < 0.5 after 5 s; 0 writes in 20-30 s |
| L2 `rising_daylight_keeps_the_face` | warm room `Big`, `Ramp` +1.7 → +6.7 over 120 s, `jitter` 0.02 | `stops_over_need ≥ −0.6` for t in 80-120 s (model today: −2.9 to −5.8) |
| L3 `a_backlit_picture_holds_still` | office `TopAndSides` +5, `jitter` 0.05, 120 s, at Slow, Normal, Fast | ≤ 2 batches in 20-120 s; no guard batch (`dx ≤ −0.95 · step_max` with `clip ≥ 0.5`); ≤ 1 reversal; no hand-back, not damped |
| L4 `a_white_window_that_brightens_or_dims_moves_nothing` | office `TopAndSides`, `Wave` mid +5.5, amp 1, period 20 s | 0 writes in 20-120 s |
| L5 `walking_in_front_of_the_window_moves_once_each_way` | office, shape toggles `TopAndSides` ↔ `Top` every 15 s for 90 s | per 15 s: ≤ 3 batches, ≤ 1 reversal; no W4 damping |
| L6 `a_window_beyond_the_clip_budget_is_not_forced_white` | warm room `Wall` +5 (share > 0.5) | `Snap.clip` < 0.5 after 5 s; every reading `backlight ≥ 0`; 0 writes in 20-30 s; no hand-back |
| L7 `the_clip_budget_holds_with_a_wrong_gain_curve` | warm room `Big` +5 (share 0.45), `s_true` 1.5 and 5 | ≤ 1 guard batch; ≤ 2 reversals after 10 s; no hand-back |
| L8 `a_backlit_hold_says_so` | L1's office scene; case 1's room | message ends " · backlight: exposing for the middle"; case 1 has no suffix; log has `bl=+` |
| — all existing | — | unchanged; the sim's `Snap.err` now includes the term, which is 0 in every existing scene (grey wall, kit scenes with two highlight cells) |

**Done when**, from the repository root:

```
cargo test -p better-cam --lib auto_controls
cargo test -p better-cam --lib fx::awb
cargo clippy -p better-cam --tests
```

all pass (the first ran 136 tests in 5.7 s on 2026-11-19 before this change; run it without piping stderr in
PowerShell, which turns cargo's progress lines into errors and a non-zero exit code).

### 10.9 Risks and open questions

Known limits, each visible in a test:

- **A window over 45 % of the picture** (L6, the warm room *big* row of 10.5) is not forced white: a loop on the dark
  side stays as dark as today there.
- **No face detection.** The zone is a guess. A face far out of the zone with a window inside it, or a person
  standing at the side of the frame, is metered by the zone anyway. The recorded desk (face lower right, window top
  left) opens the gate to 5 %: with the gate fully open on those frames (a larger window) the term reaches +0.6 stop.
- **The opposite case is not fixed**: a face lit by a lamp in a dark room. Today's meter brightens the dark room and
  burns the face (the recorded desk reads +1.7 to +2.2 stops before the lamp comes on: the face, now at 100-190/255,
  would go white). This change
  leaves it as it is (`b ≥ 0`, gate shut). It is the owner's first scene ("dark room with an orange lamp").
- **Windows 1.5-2.5 stops over the face** are only partly helped (the gate's edge); in the model they keep up to
  0.5 stop of today's darkening.
- The model is the scratch copy; the builder's Rust tests are the measurement. The kit's bookshelf and highlight cells
  differ from the model in detail, which is why the thresholds above have margin.

Questions for the owner:

1. **Should the lamp-lit face in a dark room be fixed too** (darken toward the subject when the middle would turn
   white)? It is the reverse of this change, with the same zone, and the risk that a white shirt or a bright wall
   behind the face reads as "subject". Recommendation: yes, as a separate design after this one is live-checked.
2. **Clip budget 0.45 or 0.40?** 0.45 matches what today's good resting point already shows and lifts windows up to
   about two thirds of the frame; 0.40 keeps a wider margin under the highlight guard but leaves windows of 40-45 %
   (the warm room *top and sides* and *big*) dark from the dark side. Recommendation: 0.45; L7 is the check.
3. **Status suffix wording**: "backlight: exposing for the middle" — or no suffix at all?
4. **Live check** (needs the owner, daylight behind the desk): Better Cam auto exposure on, Stop → Start (the dark
   side), then wait 30 s: the face should look as in the lit room and the log line should show `bl=` above 0 and
   `src=` above 0.2; then cover the window with a hand-held card (the person walking in front): one move each way, no
   pumping for 2 minutes.

### 10.10 Built (2026-11-19)

Built as 10.4 and 10.8 say, constants as in the tables (clip budget 0.45), the 10.7 suffix and log fields, Auto Adjust
and white balance untouched. Not done here: the pointers in 2.4, 4.2 and 6.2 (this note is the only doc change).

**Files.** `meter.rs`: `solve` (today's bisection, unchanged), the constants, `subject_weight`, `subject_error`,
`light_sources`, `clip_limit`, and `backlight` / `sources` on `ExposureReading`; tests M1-M5. `exposure.rs`: both
fields on `Sample` / `Reading` (median), the `backlit` flag (`backlit_after`: on at 0.5, off at 0.25), `bl=` / `src=`
in the log line. `status.rs`: `ExposureFacts.backlit`, `BACKLIGHT_NOTE` on every holding text except the learned-cap
one ("The camera dropped to …"), whose sentence is about something else. `sim.rs`: `SimCfg::window` (`Window`,
`WindowShape`, `WindowLevel`; `Window::alternate` for L5), `Sim::stops_over_need`, `Sim::calibration`,
`Snap::over_need`. `tests.rs`: L1-L8, the window/calibration test, the flag test, `bl=+0.00 src=0.00` in the log test.

**Fail first (old meter, before `meter.rs` changed).**

- M4: `backlit, window 224/255: error -0.04 (today -0.04)` (needs ≥ +2.0).
- L2: `face -2.81 stops from its rest at 80.0 s` (needs ≥ -0.6).
- L1: **passes on the old meter at the spec's seed -6 / 0** (rest -0.18 / -0.06 / -0.12): with `dim(-3.5)` the face
  starts 2.5 stops under, where a +5 window is already white and today's loop lands on the good point. L1 now also runs
  from seed -8 / 0 (4.5 stops under, the dark side); there it fails on the old meter (`office TopAndSides from -8:
  face -3.12 stops from its rest at 8.0 s`; at rest Big -3.36, warm room *top and sides* -2.96).

**After.** `cargo test -p better-cam --lib auto_controls`: 152 passed (136 before; +5 M, +1 sim, +8 L, +1 status, +1
flag). `cargo test -p better-cam --lib fx::awb`: 23 passed, 1 ignored. `cargo clippy -p better-cam --tests`: clean.
All spec tests M1-M5, L1-L8 pass. Face offset at rest after 30 s from seed -8 / 0 (old meter / new), window +4 / +5 / +6:

| Scene | Old | New |
|---|---|---|
| Office, top and sides | -2.27 / -3.12 / -4.35 | -0.21 / -0.18 / -0.18 |
| Office, big | -2.39 / -3.36 / -4.50 | **-1.63** / -0.06 / -0.06 |
| Warm room, top and sides | -2.15 / -2.96 / -3.93 | -0.12 / -0.09 / -0.15 |
| Warm room, big | -2.27 / -3.15 / -4.50 | -1.48 / 0.00 / +0.03 |

**Deviations from the spec (numbers).**

1. **M4, second half.** The kit's office reads -0.44 at face +0.4 under today's meter, so "new = old and |error| ≤
   0.35" cannot hold there (new = old, as asked). The test uses ≤ 0.5 at +0.4 and ≤ 0.35 at the rest point (+0.0:
   -0.11). The window at face -3 is 224/255 (spec: ≈ 215); new reads +2.11 (≥ +2.0, little margin), old -0.04.
2. **L8** uses a +3 window: at L1's +5 the loop rests on the white-window point where the term is 0, so there is no
   suffix there (the test says so). +3: `bl=+0.75 src=0.20`, suffix shown.
3. **Recorded desk (M2).** Largest term 0.031 stop, as modelled. The gate itself is not "≤ 5 %" on all frames: the
   lamp-coming-on frames (164-165 s) have `G` 0.195-0.225 (gate fully open) but the term is 0.000 there (the zone
   wants less light than the picture). The other frames' `G` is ≤ 0.114 as modelled.
4. **10.5 rows not reproduced.** Office *big* +4 rests 1.63 stops under (spec +0.2): the window cells beside the head
   (rows 5-9, columns 3-4 and 11-12) have zone weight 0.3-0.8, so `e_sub` is only +0.08 where the window is 247/255
   and the term stops adding. Windows +3 (office top and sides: -0.84; warm room: -1.45) are partly helped, as 10.9
   says. M5 has the term on in 16 of its 200 frames (most random frames are white windows or even pictures), so it
   asks for at least 10.
5. **L5** judges the periods from 15 s on (the first is the loop finding the room). "Not damped" is the proxy of at
   most one reversal per period (damping needs four in 10 s): the watchdog's damping is not observable from outside.

**Left open.** The live check of 10.9 question 4. The 10.5 *big* +4 shortfall (4) is the zone catching window cells;
a narrower `SUBJECT_SIGMA` or a zone that skips cells already read as sources would address it, which is a design
change and was not made.

---

## 11. Front-lit subject in a dark room (design, 2026-11-19)

Status: design, not built. Builds on section 10 **as built** (10.10). Scope: the device exposure meter
(`auto_controls/meter.rs`), a 2 s hold of the new term in the exposure loop (`exposure.rs`), one status suffix and two
log fields. Auto Adjust (`fx/auto_adjust.rs`), the white balance meter (`fx/awb*`) and the frontend are not touched
(11.7, 11.9).

**Owner's case.** The first of the three lights of 10: a dark room and an orange lamp near the desk that lights the
face. Preset as in 10 (target 0.45, Longest "fit", Most gain 75, Normal; white balance warmth +0.2, Normal; Auto
Adjust off). The meter brightens the dark room until the picture's mean is on target, and the face goes white. Wanted:
"like an actual automatic compensation": the face as in a lit room, the room left darker than the face (its natural
fall-off, not lifted to grey), and no pumping when the user leans in or out of the lamp's light or switches the lamp.

### 11.1 Recommendation

Add a second bounded, one-way term to the reading, the mirror of 10's: a **ceiling** set by the brightest part of the
middle of the picture, active only when the room around it is much darker.

1. **Bright tenth of the middle.** Weight the cells by 10's subject oval, leaving out the part of it that 10 treats as
   the place of light sources (`z·(1 − p)`), and take the brightest 10 % of that weight. The ceiling `e_top` is the
   exposure that puts their mean at **0.60** coded (0.9 stop over the target grey; the brightest tenth of an evenly lit
   room's middle sits at 0.53-0.65).
2. **Dark room.** Compare that tenth with the room: the 75th percentile of the cells outside the oval, light sources
   left out. The gap `D` in stops opens the term over **1.0-1.8 stops**; it fades out again as 10's light-source share
   `G` goes from 0.15 to 0.25 (a window behind the user: 10's case).
3. **Reading.** `error = e_all + b + f` with `f = −gate · clamp(e_all + b − e_top, 0, 3)`: never brighter than with 10,
   at most 3 stops darker, and only when the reading would push the bright tenth over 0.60.
4. **Loop.** The exposure loop uses the strongest (most negative) `f` of the last **2 s**: it darkens at once when a lit
   face appears and waits 2 s before it brightens when the face turns away or crosses the oval's edge.

Even rooms, rooms with a lamp in the picture and every backlit scene of 10 read exactly as with 10 (`f = 0`). On the
recorded desk the face goes from 202-247/255 (white) to 120-162 and the room from 79-89 to 28-51, nothing clipped; the
loop needs 1.2-3.0 stops less light: less gain and, in the owner's room, 30 fps more often. No face detection, no new
statistics, no dependency; one more pass over the 256 cells on the controller thread.

### 11.2 Root cause, with numbers

Numbers come from a scratch model (not in the repo): a copy of `measure_exposure` with 10's term as built, the sim's
kit scenes as `Scene::render` makes them (in-cell texture ±0.2, the two highlight cells at 2.0), the recorded desk
decoded as `real.rs` does, an idealised loop (Normal: `k` 0.7, `step_max` 1, bands 0.33 / 0.12, highlight guard) and,
for the replays of 11.5, a timed loop (a decision every 0.45 s on the median of 3 samples, as 2.5.4). "Lamp-lit, room
−3" is a kit scene whose skin and hair keep their light while the room gets 3 stops less (the shirt half that).
"Face" is the mean coded luma of the skin cells (kit) or of the recorded frame's cells from row 6 down that are 90/255
or brighter (desk); "room" the median of the other cells. The evenly lit office rests with the face at **114/255**.

**The recorded desk** (`fx/awb/fixtures/real_room.txt`, 87 of its 93 frames: a dark room, a window top left, a face lit
by a lamp in the lower middle or lower right, 4-15 % of the centre-weighted picture). At the recorded exposure today's
reading is **+1.67 to +2.21** stops, and 10 does not change it: the oval's own reading `e_sub` is within −0.24..+0.60
of `e_all` (the oval is mostly dark wall), `G` is 0.07-0.11 (gate ≤ 5 %, `b` ≤ 0.031). Where the loop rests:

| | Today (10 as built) |
|---|---|
| Rest, over the recorded exposure | +1.95 .. +2.58 stops |
| Face | 202-247/255: +1.8 .. +2.5 stops over the lit-room face |
| Room | 79-89/255 |
| Clipped share at rest | 7-18 % |

Two causes:

1. **The mean of a picture that is mostly dark room.** With 85-95 % of the weight on walls 3-3.5 stops under the face,
   a mean on target needs the walls near 80/255, and the face, 3 stops higher, is white.
2. **The near-white weight (2.4) once more.** A face turning white weighs 0.15, so the reading moves further toward the
   room: a second, brighter resting point. Kit office, lamp-lit, room −3: the loop rests with the face at 206/255 from
   the dark side and 253 from the bright side; room −4: 240 / 255; room −5: 255 from both.

What 10 offers does not reach it: `e_sub` is a mean over the oval, and the face is a small bright part of it.

### 11.3 Options considered

| Option | Result in the model | Verdict |
|---|---|---|
| A. A white budget for the whole picture (at most a few % white) | Also clamps lamps and windows: changes an even room with a lamp in it, and fights 10, which wants a window white | Rejected |
| B. 10's `e_sub` as a ceiling | Desk: `e_sub` within −0.24..+0.60 of `e_all`; no effect | Rejected |
| C. Brightest share of the plain oval (`z`, no `(1 − p)`) | A window that reaches the oval (top-left corner, rows 0-5) became the "subject" from the dark side: an evenly lit office rested 1.5 stops dark | Rejected; F weights by `z·(1 − p)` |
| D. Gap against the median of the upper part, or of the whole picture | Upper part: the desk's window lifts it (gap 1.3-2.2, gate barely open). Whole picture: opens on even scenes (checker 1.8, dark-skin office 1.8) | Rejected; F uses the 75th percentile outside the oval without light sources |
| E. Drop from the tail the cells that would still be white at the ceiling (lamps) | On a partly clipped face it drops the face's own white cells: the reading jumped 0.23 stop within 0.1 stop of exposure and the loop cycled (54 reversals in 60 decisions on one desk frame) | Rejected: the tail's weights must not depend on the exposure |
| F. Bright-tenth ceiling, gated by the gap to the room, fading out over `G` 0.15-0.25 | 11.5 | **Recommended** |
| G. F, coupled to 10 by `(1 − a)` (10's own gate) | The burned face raises `G` itself (corner window + lamp-lit face: `G` 0.11 → 0.16 as the face whitens) and shuts the ceiling when it is needed: face 253/255 from the bright side | Rejected; F's own fade at 0.15-0.25 |
| H. F without the 2 s hold | Desk replay 111-116 s: 25 writes in 90 s; 108-116 s (the face moving): 7 hunting triggers (W4: damping, then a hand-back) | Rejected |

### 11.4 The design

Notation of 10.4: covered cells (`cov_i ≥ 0.02`), `lin_i`, `y_i = srgb_encode(lin_i)`, `c_i = centre_i · cov_i`, `z_i`
(`subject_weight`), `p_i = (1 − z_i) · (1 − ss(SOURCE_ROWS, v_i))` (10's source placement), `src_i` = 10's per-cell
source value before placement, `max(ss(SOURCE_AT_SUBJECT, srgb_encode(min(1, 2^e_sub · lin_i))),
ss(SOURCE_WHITE_NOW, y_i))`, and `G`, `b`, `e_all` as built. `T` is the target; `ss` = `smoothstep`.

1. **Middle weights.** `s_i = c_i · z_i · (1 − p_i)`: the oval without the part 10 treats as where light sources are.
   Fixed per cell (times coverage): they do not depend on the exposure.
2. **Bright tenth.** Walk the cells by `lin_i`, brightest first (the order `clip_limit` sorts), taking
   `t_i = min(s_i, Q · Σs − taken)` until `Q = SUBJECT_TOP_SHARE` of `Σ s_i` is taken (the last cell in part).
   `L_top = Σ t_i lin_i / Σ t_i`.
3. **Ceiling.** `Y_top = min(SUBJECT_TOP_MAX, srgb_encode(2^SUBJECT_TOP_STOPS · srgb_decode(T)))`: 0.60 at T 0.45
   (0.28 at 0.2, 0.79 at 0.6, 0.90 from 0.68 up). `e_top = solve(tail, Y_top)`, the tail being the cells with weights
   `t_i` (the shared `solve`, ±4 stops, 24 steps).
4. **Room.** Weights `r_i = c_i · (1 − z_i) · (1 − src_i)`: outside the oval, light sources left out. `R` = the
   `ROOM_PERCENTILE` weighted percentile of `lin_i`, interpolated: each cell sits at the middle of its weight interval
   and `R` is read linearly between the two cells around the percentile, so it moves continuously when weights change.
   No room weight (`Σ r_i = 0`) or `L_top = 0`: no term.
5. **Gap and gate.** `D = log2(L_top / max(R, ROOM_FLOOR))`. `g = ss(FRONT_GATE, D) · (1 − ss(FRONT_SOURCES_OFF, G))`.
6. **Reading.** `f = −g · clamp(e_all + b − e_top, 0, FRONT_MAX_STOPS)`; `error = clamp(e_all + b + f, −4, 4)`.
   `ExposureReading` gains `front = f` (stops, `−3..=0`) and `room_gap = D` (stops; 0 when not computed). Computed only
   when `confidence > 0`, like 10's term; skip `e_top` when `g = 0`.

| Constant (`meter.rs`) | Value | Why |
|---|---|---|
| `SUBJECT_TOP_SHARE` | 0.10 | The face is 4-15 % of the desk picture; a tenth of the middle weight is its lit side. 0.20 left the gate shut on the desk where the face moved (109.9 s: gap 0.95 at today's rest, 1.56 with 0.10); even scenes stay ≤ 0.85 with 0.10. |
| `SUBJECT_TOP_STOPS` | 0.9 | `Y_top` 0.60 at T 0.45: the brightest tenth of the kit's lit rooms at rest is 0.53-0.65 (134-165/255), so the lit middle ends as bright as a lit room's brightest part. Follows the user's target. |
| `SUBJECT_TOP_MAX` | 0.90 | From T 0.68 up the formula passes white. |
| `ROOM_PERCENTILE` | 0.75 | The room's bright end. In a scene of reflectances (the checker, a dark-skin face, a white shirt) the room has bright things too, which the median misses (median: checker and dark-skin office 1.8 stops); in a lamp-lit room even the room's bright end is dark. |
| `ROOM_FLOOR` | 1e-4 | Linear; a black room gives a finite gap. |
| `FRONT_GATE` | (1.0, 1.8) stops | Even scenes ≤ 0.83 (checker), lamps in the picture ≤ 0.69; the desk 1.73-3.20 at the recorded exposure and still ≥ 1.56 with the face clipped at today's rest, so the bright side opens too. A face lit 2 stops over the room (gap 1.28) gets 0.3 of it. |
| `FRONT_SOURCES_OFF` | (0.15, 0.25) on `G` | Above the desk's 0.07-0.115 and the 0.16 a burned face adds; under 10's backlit windows (0.26-0.36). |
| `FRONT_MAX_STOPS` | 3.0 | Mirror of 10. At rest `|f|` ≤ 2.15 on the desk and 1.74 on the kit's room −5. |

| Constant (`exposure.rs`) | Value | Why |
|---|---|---|
| `FRONT_HOLD` | 2.0 s | Longer than a head turn or a face crossing the oval's edge (0.8 s on the desk); short enough that switching the lamp off brightens within about 3 s. 3 s gave about the same (108-116 s: 40-44 writes against 44-49). **Superseded by 13.11 and 13.13:** the hold is 2 s only after the last lit sample (the lamp-off release is still 2 s); while lit samples arrive it is the lowest reading of the last 6 s. |
| `FRONTLIT_ON` / `FRONTLIT_OFF` | −0.5 / −0.25 stop | The status flag's hysteresis (11.8), as 10's 0.5 / 0.25. |

**Target relation.** The brightest tenth of the middle at 0.60: the lit face as bright as the brightest part of a lit
room, nothing in the middle white. The room keeps its own ratio below it: a room 3.3 stops under the face lands at
28-35/255, darker than the face, not lifted toward grey. In a lit room the term is 0 and the whole picture stays on
target as today (the face at 114).

**Bounds.** On every frame `−3 ≤ f ≤ 0`: never brighter than 10's reading, at most 3 stops darker, and only by what it
takes to bring the bright tenth down to `Y_top`. A reading that already puts the tenth at or under `Y_top` (an
overexposed even room, a backlit face) is untouched. At rest on the desk `f` is −1.1 .. −2.15: 0.85 stop of margin for a
room darker than the recording.

**How the two terms combine.** Order: 10's floor first (`b` on `e_all`), then this ceiling on the result. Both can be
non-zero only while 0.10 < `G` < 0.25 (10's gate partly open, this one not yet closed); under 0.10 `b = 0`, from 0.25
on `f = 0`. The ceiling may take back part of the floor's raise inside that band, never more than `f`'s bound. A window
that reaches the oval is kept out of the bright tenth by `(1 − p_i)` and out of the room by `src_i` once it is white.

**No oscillation and no second resting point.**

1. *Continuous.* For a fixed scene the reading is a continuous function of the exposure: every gate is a smoothstep,
   the tenth takes its last cell in part, `R` is interpolated, `solve` is continuous. There is no switch for the loop to
   flip across (option E shows what one does).
2. *Invariant while not clipped.* `s_i` is fixed and scaling the exposure keeps the order of `lin_i`, so the tenth is
   the same cells at every exposure: `X + e_top` and `D` do not move with the loop while no tenth cell is clipped.
3. *The rest only weakens it as the picture brightens.* What depends on the exposure: the tenth clipping (`L_top` a
   lower bound: `D` and the darkening smaller), `G` rising with white cells (10's `b` up, this gate down), and the
   room's brightest cells turning white (`R` down, `D` up). The first two make the reading fall more slowly than a stop
   per stop as the exposure rises; that cannot make the step-and-settle loop overshoot (it would need a slope steeper
   than −2/k), it could only create a resting point on the bright side, and the model finds none: from-dark and
   from-bright rests agree within 0.23 stop on all 80 fixed desk frames and 0.20 on the kit (today: up to 1.6 stops on
   the kit). The third needs room cells at 0.93 and up, far above the room's 75th percentile in every lamp-lit scene.
4. *Measured.* Idealised loop, 57 scenes from both sides, Slow / Normal / Fast, reading noise σ 0.05: 0 / 1 / 3
   reversals in total, no move after settling.

**The 2 s hold (`exposure.rs`).** *Superseded by 13.11 (the hold holds the lit reading, not the term) and 13.13 (its
length); what follows is the first design.* In motion the bright tenth is a handful of cells: a head turning changes `f` by
0.2-0.5 stop from one frame to the next, and a face crossing the oval's edge (desk 110.0-110.8 s) drops `D` under the
gate for 0.8 s (option H). So the loop uses `f_held` = the most negative `front` of the samples with `conf ≥ 0.2` of the
last `FRONT_HOLD` seconds, **across writes** (`f` is the difference of two readings that both shift with the exposure,
so a write does not change it), and decides on `ê = median₃(e − front) + f_held`. Attack is immediate (a lamp switched
on darkens at once), release waits 2 s. The history is cleared on pause, close and engagement. W3 (judging a batch) and
the gain-curve learning (2.5.7) keep the raw median of `e`: they measure the camera's answer to a write, which the hold
would blur. With no term (every even or backlit picture) `f_held = 0` and `ê` is today's, bit for bit.

**Cost.** One walk down the sort `clip_limit` already makes, one bisection over the tenth (≤ 30 cells), one weighted
percentile (the same sort, read upward), and the 256 `src_i` that `light_sources` computes (share them). About a third
more than 10's meter; controller thread, ≤ 15 Hz. The hold: a ring of (time, `front`) of at most 2 s of samples.

### 11.5 Effect on the scenes (model)

Face and room coded /255 at rest, from the dark and the bright side ("today" is 10 as built).

| Scene | Today | New |
|---|---|---|
| **Recorded desk, lamp-lit face, dark room** (80 of 87 dark frames) | face 202-247 (+1.8..+2.5 stops over 114), room 79-89, 7-18 % clipped | face **120-162** (+0.15..+1.1), room **28-51**, 0-7 % clipped; rest 1.2-3.0 stops lower; both sides within 0.23 stop |
| Recorded desk, 110.0-110.8 s (7 frames, the face moving to the right edge of the oval) | 235-250 | 167-253: partly (gap 1.24-1.83, the gate's edge) |
| Recorded desk, lamp lighting the room (163.9-165.4 s) | rest X | identical (`f = 0`) |
| **Kit lamp-lit face**, office and warm room, room 3-5 stops darker | 199-255 from the dark side, 208-255 from the bright side (two resting points) | **149-158** from both; room 33-75 |
| Lamp from one side (1.5 stops across the face), room −3 / −4 | 224-247 | 120-132 |
| Mildly lit (room −2) | 165-183 | 160-176 (gate 0.3) |
| Dark-skin office, room −4 / −5 | 175-206 | 148-164; room −2 / −3 identical (gap < 1: the face is not brighter than the room) |
| **Lamp in the picture**, dark room, lit face (lamp upper right, side, lower left) | 190-206 | 146-158 |
| **Even rooms** (office, dark-skin office, warm room, grey room, closeup, face fill, dark face fill, checker), also with a lamp in the picture (3 places) | rest X | identical, `f = 0.000` (gap ≤ 0.83) |
| **Backlit windows** (Top, TopAndSides, Big, Wall; office, warm room, dark skin; +2..+6) | 10.5 / 10.10 | identical: `f = 0` within ±1.5 stops of every rest; 2-3 stops underexposed a corner window can give `f` up to −0.44 where the reading is still about +2 or more (the loop climbs anyway) |
| Corner window (rows 0-5, columns 0-6, the desk's kind), even room | rest X | identical |
| Corner window, lamp-lit face, room −3 / −4 | 199-227 from the dark side, 253-255 from the bright side | 138-150 from both |
| Window behind (TopAndSides +3..+5) and a lamp-lit face, room −3 / −4 | 157-183 | identical (`G` 0.32 shuts the term; question 2) |
| **Face filling the frame** (face fill, closeup), even | rest X | identical |
| Lamp-lit closeup (the face is most of the picture), room −3..−5 | 155-184 from the dark side, 167-255 from the bright side | dark side identical (gap ≈ 0: the face is its own surround), bright side 164-193 |
| White shirt, dark walls, even light (a risk, 11.10) | face 141-146, shirt ≈ 214 | face 119-127, shirt ≈ 153 |

Scene changes, timed loop with the hold, Normal (writes and reversals per change, last write after the change):

| Change | New |
|---|---|
| Lamp on / off every 15 s (lamp-lit office, room −3 ↔ the face 3 stops darker) | 4-5 writes, ≤ 1 reversal, last ≤ 2.5 s; no W4 |
| Lean toward / away from the lamp (face ±1 stop) every 15 s | 5-8 writes, ≤ 1 reversal, last ≤ 3.3 s |
| Lean into / out of the frame (office ↔ closeup) every 15 s | 2-5 writes, ≤ 1 reversal |
| Lean ±1 stop every **4 s**, 2 minutes | ≤ 3 reversals in any 10 s (W4 needs 4): never damped |
| Desk replay 111-116 s and 40-44.5 s, forward and back, 120 s | 0 writes after 30 s; face median 117-134 (today 219-248) |
| Desk replay 108-116 s (two face positions 0.8 stop apart and the edge crossing, every 8 s) | 44-49 writes in 90 s, no W4; face median 126-128, 90th percentile 132-135 (without the hold: 70-74 writes, 7 W4 triggers) |
| Walking past a white window, steady lamp-lit room with noise | 0 writes |

Without the hold a change costs 3-4 writes and no reversal; the hold adds one or two small writes and at most one
reversal on a release (the lamp off, leaning away), and is what keeps a moving face from W4.

### 11.6 Gain, noise and the frame rate

`f ≤ 0` always: the term only ever takes stops off what the loop asks for.

- **Gain and noise.** At rest the loop needs 1.2-3.0 stops less light on the desk. With the exposure at `E_rate`, gain
  drops by that (down to `G_min`, then the exposure shortens: 2.5.3). Less gain is less noise on the face, and the room
  is no longer lifted by gain (79-89 → 28-51/255), so what grain is left sits in a dark area. The darker room is not a
  "too dark" signal: the mean coded luma at the new rest is 0.23-0.33 and `conf` stays 1.
- **Frame rate (2.5.12, commit 1e7dcd9).** Its enter and exit tests (`need = X + ê`) use the held reading, so a
  lamp-lit picture is `|f|` (1.1-2.2 stops on the desk) further from entering the lowered rate and as much nearer to
  leaving it. Nothing in 2.5.12 changes (`OVER_ENTER` 1.0, `OVER_EXIT` 0.5, the 3 s timer, `E_ceil`). The fixture does
  not store its exposure; if it was taken at 2.5.12's limits (E −5, gain 100: its +1.7..+2.2 matches that finding), the
  reading there goes from +1.67..+2.21 to −0.55..+0.94: no frame passes the entry at Most gain 100. At the owner's Most
  gain 75 (0.75 stop less with the unverified `S` 3.0) the still face of 111-116 s needs +0.2..+1.0 over the limit
  (30 fps holds), 40-44 s about +1.0 (at the entry's edge), 163 s +1.3..+1.7 (lowered: 1/16 s, 16 fps, gain under 75).
  Today every one of them lowers the rate at full gain.
- **Lamp switched while lowered.** On: the need drops 1-2 stops, the exit fires once (as for any brightening). Off: a
  dark room, entry after 3 s as today. The 1.5-stop dead band and the timer allow one rate change per switch.
- **W3 and gain learning.** At today's rest 7-18 % of the lamp-lit picture is clipped, over `JUDGE_CLIP` and
  `LEARN_MAX_CLIP` (0.05): W3 never judges and `S` never learns in that scene. At the new rest 0-7 % is clipped (0 on the
  still frames), so both work again.

### 11.7 White balance (warm lamp on the face, dark room)

**No change.**

1. **One light.** A lamp that lights the face in a dark room also lights the room, dimly: one colour. The shared
   estimator reads it from the neutral surfaces, and warmth +0.2 keeps a fifth of the way to 4000 K (aim about 5100 K
   against 5500), the slightly warm look of a lamp-lit room. The face being brighter changes none of that.
2. **The fix gives the estimator a better picture.** Today the face is white at rest and the estimator drops it
   (`BRIGHT` 0.85-0.97, `CLIP` 0.90-0.98), so the walls alone decide. At the new rest the face is in range and the
   walls (28-51/255) partly inside the `DARK` gate (0.04-0.15): the estimator sees what it saw on the recorded frames at
   their recorded exposure (the new rest is −0.58..+0.87 stop from it), which `a_real_room_is_read_calmly_or_not_at_all`
   and `a_face_in_a_dim_room_does_not_make_the_loop_hunt_or_hand_back` already cover (the face's second answer is
   doubted: `TWO_ANSWERS`). The mean coded luma at rest (0.23-0.33) stays over `MIN_MEAN_LUMA` (0.10).
3. The mixed-light policy (docs/AUTO-WHITE-BALANCE.md D2) is unchanged; the white balance loop still waits while
   `|ê| > 1`.

**Later step, only if** the live check (11.10, question 4) shows the face orange against a neutral room, or the room
blue against a natural face: a separate design (for example the estimator's middle cells weighted by the same
`z·(1 − p)` in its slow lane, as 10.6 sketches) with its own tests.

### 11.8 Status and log text

- **Status (4.2), one new suffix:** " · dark room: exposing for the lit middle", e.g. "Holding 30 fps · gain 31 of 75 ·
  dark room: exposing for the lit middle", on every text that gets 10's `BACKLIGHT_NOTE`. Flag on when
  `f_held ≤ −0.5`, off at `≥ −0.25`. At most one suffix: this one only while 10's `backlit` flag is off. Made in
  `status.rs`; the card shows `message` as it is, so **no frontend change**.
- **Log line (9.6):** append `fl=-1.60 gap=+2.05` (`f_held` and the median `room_gap`, stops).

### 11.9 Build plan (one session)

Start after 10.10 (it is in). Files: `src-tauri/src/auto_controls/meter.rs`, `exposure.rs`, `status.rs`, `sim.rs`,
`tests.rs`; this document (2.4 pointer, 4.2 row, 6.2 rows, an "11.11 Built" note). Not touched: `fx/auto_adjust.rs`,
`fx/awb*`, the shaders, the frontend.

1. **Fail first.** Add the sim pieces of step 3 and the tests of the table, run M8, M9, L9, L10 and L18 against the
   meter as it is (before step 2), and record their failing values in the Built note: they must fail there.
2. **`meter.rs`.** The constants of 11.4; `fn middle_weight(i) -> f64` (`z·(1 − p)`); make 10's per-cell `src_i` and
   the descending sort shareable (`light_sources` and `clip_limit` behaviour unchanged); `fn bright_tenth(stats) ->
   Option<(Vec<(f64, f64)>, f64)>` (tail cells with weights, `L_top`); `fn room_level(stats, src) -> Option<f64>`
   (interpolated percentile); `fn front_light(...) -> (f64, f64)` (`f`, `D`); the fields `front`, `room_gap` on
   `ExposureReading`; `error` as 11.4 step 6. Check: every existing meter test, including 10's M1-M5, passes unchanged.
3. **`sim.rs`.** `SimCfg::lamp_lit: Option<LampLit>` with `room: f64` (stops, < 0: every cell that is not skin, hair or
   shirt, the two highlight cells included; the shirt gets half) and `face: WindowLevel` plus a `Toggle { a, b, every }`
   variant (stops on skin and hair over time). Calibrated **before** the room is darkened, so `need` keeps its meaning
   (the evenly lit room on target); `Snap::face_over_need` = `over_need` plus the face's level now. `WindowShape::Corner`
   (rows 0-5, columns 0-6). A lamp-in-the-picture helper for the meter tests (10's M3 cells upper right; side: bulb
   columns 14-15 rows 7-8, ring columns 13-15 rows 6-9; lower left: bulb columns 1-2 rows 12-13, ring columns 0-3 rows
   11-14). `Replay::with_exposure(frames, x_rec)`: the exposure meter is fed the recorded frame times
   `2^(eff_e + gs_true(eff_g) − x_rec)` per channel, at most 1; `Sim::replay_face_room()` (face: the recorded frame's
   cells from row 6 down at 90/255 or more; room: the median of the other cells from row 5 down; coded, as exposed now).
   Check: a test that `lamp_lit` with `room: 0` and a `Const(0)` face leaves the picture and the calibration unchanged.
4. **`exposure.rs` / `status.rs`.** `Sample` carries `front` and `room_gap`; a ring of `(t, front)` for `FRONT_HOLD`
   (confident samples, not cleared by writes, cleared on pause, close, engagement); `Reading` gets `e` = the held value
   of 11.4 and `e_raw` = the median of raw `e` (used by `assess` and `learn` only), `front_held`, `room_gap` (median).
   `ExposureLoop.frontlit` with the −0.5 / −0.25 hysteresis; `ExposureFacts.frontlit`; the suffix and log fields of
   11.8. Check: `the_log_line_says_where_the_loop_is_and_what_it_saw` extended with `fl=+0.00 gap=` in a normal room.
5. **Docs:** 2.4 (a pointer to 11 for the third term), 4.2 (the suffix), 6.2 (the new rows), the Built note with the
   measured values next to the model's, and any threshold changed with its reason (as 10.10 did).

Tests (`auto_controls::meter` M, `auto_controls::tests` L; owner's preset: Normal, Most gain 75, Longest "fit", F = 30;
"lamp-lit, room −n" = `lamp_lit { room: −n, face: Const(0) }`; offsets are the face's stops over need):

| Name | Scene | Pass (model) |
|---|---|---|
| M6 `even_rooms_have_no_front_light` | 10's M1 picture set at −1..+0.5 stop; the sim's warm room with a lamp in the picture (3 places) at −1..+0.5 | `front == 0.0`, `error` equal to the reading without the term (< 1e-12) (gap ≤ 0.83) |
| M7 `windows_are_not_lit_faces` | Top, TopAndSides, Big, Wall, Corner at +1.5..+8; office, warm room, dark skin; offsets −1.5..+1, and −3..−1.5 | `front == 0.0` at −1.5..+1; below that, `front < 0` only where `error ≥ +1.5` (≥ +1.99) |
| M8 `the_recorded_desk_asks_for_less_light` | `real::frames()` at the recorded exposure | frames whose 10-reading is > +1, outside 110.0-110.8 s: `front ≤ −0.9` (≤ −1.04), `room_gap ≥ 1.6` (≥ 1.73), `error ≤ +1.1` (≤ +0.94; 10: +1.67..+2.14); 110.0-110.8 s: `front ≤ 0`; 163.9 s on: `front == 0.0` |
| M9 `a_lamp_lit_face_asks_for_less_light` | lamp-lit office, room −4, at +2.3 and +0.8; lamp-lit warm room −3 with a lamp in the picture (bulb +4, ring −0.5 over the face; 3 places) at +1.7 | +2.3: `error ≤ −1.2` (−1.48; 10: +0.24); +0.8: `|error| ≤ 0.25` (−0.02; 10: +1.47); lamps: `error ≤ 10's − 0.6` (−0.79..−0.84) |
| M10 `the_front_light_term_stays_in_its_bounds` | 300 seeded random frames: scene, room 0..−6, window shape and level −2..+8 or none, offset −4..+4 | `−3 ≤ front ≤ 0`; `error == clamp(e_all + backlight + front, ±4)`; `front == 0` when `sources ≥ 0.25` or `room_gap ≤ 1.0`; at least 30 frames with `front < −0.5` |
| M11 `the_ceiling_follows_the_target` | lamp-lit office, room −4, targets 0.3, 0.45, 0.6 | at `offset + error` the tenth's coded mean is `Y_top(T)` within 0.01 |
| L9 `a_lamp_lit_face_is_not_white_after_open` | lamp-lit office and warm room, room −3 and −4, `dim(-5.0)`; seeds −8 / 0 (dark side) and −5 / 75 (bright side) | after 8 s `face_over_need` in [+0.4, +1.3] (+0.71..+1.02; 10: +1.74..+3.44); the two seeds within 0.35; `clip < 0.02`; 0 writes in 20-30 s |
| L10 `the_recorded_desk_settles_with_the_face_lit_not_white` | `Replay::with_exposure`, `real::between(111.0, 116.0)` and `(40.0, 44.5)`, forward and back, 120 s, `x_rec = −5 + gs(25)`, seeds −5 / 0 and −5 / 75 | after 30 s face median ≤ 150 (117-134; 10: 219-248), room median ≤ 60; ≤ 2 writes in 30-120 s; no hand-back, never damped |
| L11 `a_moving_lit_face_is_followed_without_hunting` | the same with `between(108.0, 116.0)`, Normal and Fast | never damped (≤ 3 reversals of moves ≥ 0.15 stop in any 10 s), no hand-back; face 90th percentile after 30 s ≤ 175 (132-135) |
| L12 `lamp_on_and_off_moves_once_each_way` | lamp-lit office, room −3, face `Toggle { a: 0, b: −3, every: 15 }`, `dim(-6.0)`, 120 s | per 15 s: ≤ 6 batches, ≤ 1 reversal, last batch ≤ 4 s after the change (4-5, ≤ 1, ≤ 2.5 s); never damped; at each "on" period's end `face_over_need` in [+0.4, +1.3] |
| L13 `leaning_toward_and_away_does_not_pump` | lamp-lit office, room −3, face `Toggle { a: +1, b: −1 }` every 15 s at Slow, Normal, Fast, and every 4 s at Normal | never damped, no hand-back; 15 s: ≤ 1 reversal per change (≤ 1) |
| L14 `a_window_and_a_lamp_lit_face_keep_section_10` | TopAndSides +4 and lamp-lit room −3 | `front == 0.0` on `Sim::picture()` from 5 s on; `face_over_need` in [+0.6, +1.3] (+0.87..+1.03) |
| L15 `a_corner_window_with_a_lamp_lit_face` | Corner +4 with lamp-lit room −4, both seeds; Corner +4 in the even office | lamp-lit: `face_over_need` in [+0.2, +1.0] (+0.50..+0.64), seeds within 0.35; even: `front == 0.0` at rest, `face_over_need` in [−0.3, +0.3] |
| L16 `a_lamp_lit_room_holds_still` | lamp-lit office, room −4, `jitter` 0.05, 120 s, Slow, Normal, Fast | ≤ 2 batches in 20-120 s, ≤ 1 reversal, never damped |
| L17 `a_lamp_lit_hold_says_so` | L9's office room −4; case 1's room; a `status.rs` unit test with `backlit` and `frontlit` both set | message ends " · dark room: exposing for the lit middle"; case 1 has no suffix; both flags: only 10's suffix; log has `fl=-` |
| L18 `a_lamp_lit_face_keeps_the_frame_rate` | lamp-lit office, room −5, `dim(-5.5)`, owner's preset | after 10 s: never `over`, capture ≥ 29.5 fps, `G < 75` (10 lowers the rate: its rest needs 1.5 stops over the limit) |
| — all existing | — | unchanged (the term is 0 in every existing scene: no lamp-lit room) |

**Done when**, from the repository root:

```
cargo test -p better-cam --lib auto_controls
cargo test -p better-cam --lib fx::awb
cargo clippy -p better-cam --tests
```

all pass (152 and 23 + 1 ignored after 10.10). Run them without piping stderr in PowerShell (`2>&1` turns cargo's
progress lines into errors and a non-zero exit code).

The model's thresholds carry margin because the Rust kit differs in detail (10.10 found rows of 10.5 off by up to
1.8 stops). A threshold may move with a measured reason in the Built note; a scene that fails by more than its margin
is a finding to report, not a constant to tune quietly.

### 11.10 Risks and open questions

Known limits, each visible in a test or the table of 11.5:

- **No face detection.** The middle is a guess: a face at the frame's edge (desk 110.0-110.8 s) or standing beside the
  oval is not the bright tenth; the loop then holds the old ceiling 2 s and lets go (L11).
- **A reflectance scene can open the gate.** A white shirt in a dark-walled room under even light: gap 1.34, face
  141-146 → 119-127 (half a stop darker, the shirt no longer near white). The checker is 0.17 stop under the gate.
- **A lamp-lit face that fills most of the frame** is its own surround (gap ≈ 0): the reading is 10's, face 155-184
  from the dark side.
- **A window behind a lamp-lit face** keeps 10's result (face 157-183): the term is off from `G` 0.25 on.
- **The release costs one or two more writes** and up to one reversal per change (the hold); a face moving between
  lit positions every few seconds makes the loop follow it (L11: about one write per 2 s, never damped).
- **Auto Adjust in the chain** meters its own input with today's weights and would brighten the picture again, up to its
  limits; the owner runs it off. Sharing the term with Auto Adjust would be a separate step (`fx/auto_adjust.rs`).
- The model is a scratch copy; the builder's Rust tests are the measurement.
- Side note for 10.10's open point (4): the same `z·(1 − p)` weight is "a zone that skips cells read as sources"; it
  is not applied to `e_sub` here.

Questions for the owner:

1. **How bright the lit face, how dark the room?** Proposed: the lit side of the face as bright as the brightest part of
   a lit room (0.60 coded at target 0.45: the desk's face 120-162, the room 28-51/255). `SUBJECT_TOP_STOPS` 1.2 instead
   of 0.9 would put face and room about 0.3 stop brighter. Recommendation: 0.9, then judge it live.
2. **A window behind a lamp-lit face:** keep 10's result (face 157-183/255), or let the ceiling act there too (model:
   face 126-135, but pure backlit scenes then move by up to 0.1 stop)? Recommendation: keep 10's.
3. **Status suffix wording:** "dark room: exposing for the lit middle", or no suffix?
4. **Live check** (needs the owner, the dark room and the orange lamp): Better Cam auto exposure on, Stop → Start, wait
   20 s: the face should not be white, the room darker than the face, the log `fl=` below −0.5 and `gap=` above 1.8.
   Then switch the lamp off and on twice (one settle each way, ≤ 3 s), lean toward and away from the lamp for a minute
   (no "kept swinging"), and check that the frame rate shows 30 fps where it showed 16 before. Note the face colour for
   11.7.
5. **The white shirt case** (a dark-walled room in even light: face half a stop darker): accept, or widen the gate to
   1.2-2.0 stops (the mild lamp-lit face, room −2, would then get nothing)? Recommendation: accept.

### 11.11 Built (2026-11-19)

Built as 11.4 and 11.9 say, with the owner's choices: `SUBJECT_TOP_STOPS` 0.9, `FRONT_GATE` (1.0, 1.8), the gate off by
`FRONT_SOURCES_OFF` (0.15, 0.25) so a window behind a lamp-lit face keeps 10's result, the suffix " · dark room:
exposing for the lit middle", the log fields `fl=` and `gap=`; every other constant as in the tables. Auto Adjust, white
balance, the shaders and the frontend are untouched. Not done here: the pointers in 2.4, 4.2 and 6.2 (this note is the
only doc change).

**Files.** `meter.rs`: the constants, `middle_weight`, `bright_tenth`, `room_level`, `front_light`, `source_level` /
`source_place` (10's per-cell source value, shared with `light_sources`), `front` / `room_gap` on `ExposureReading`,
`error = clamp(e_all + b + f, ±4)`; tests M6-M11. `exposure.rs`: `front` / `room_gap` on `Sample`, the 2 s ring of
`(t, front)` (`FRONT_HOLD`, confident samples, not cleared by writes, cleared on pause, close, engagement and a new
target; **superseded by 13.11 and 13.13: the ring holds lit readings, shifted by the writes**), `Reading::e` = held
(`median₃(e − front) + f_held`), `Reading::e_raw` for W3 and gain learning, the `frontlit`
flag (`frontlit_after`: on at −0.5, off at −0.25), `fl=` / `gap=` in the log line. `status.rs`: `ExposureFacts.frontlit`,
`FRONTLIT_NOTE`, one suffix at most (10's wins). `sim.rs`: `SimCfg::lamp_lit` (`LampLit`), `WindowLevel::Toggle`,
`WindowShape::Corner`, `Replay::with_exposure`, `Sim::face_over_need`, `Sim::replay_face_room`, `Snap::face_over_need` /
`Snap::face_room`. `tests.rs`: L9-L18, the lamp-lit calibration test, `fl=+0.00 gap=` in the log test.

**Fail first (the meter as it was: `front` and `room_gap` stubbed to 0, the new functions not called; the loop's hold
was in and reads 0).** 11 of the new tests fail, among them the five of 11.9 step 1:

- M8: `40.11 s: error +1.76 (10: +1.76)`, `front` 0 (needs `front ≤ −0.9`, `error ≤ +1.1`).
- M9: `+2.3: error +0.24`, `front` 0 (needs ≤ −1.2).
- L9: `office, room -3, from -8/0: face +1.77 at 8.0 s` (needs [+0.4, +1.3]).
- L10: `frames 111-116 s from (-5, 0, false): face 215, room 63` (needs face ≤ 150, room ≤ 60).
- L18: `10.1 s: ... E=-4 G=75 over=true fps=15.5/30`, clip 0.25 (needs over=false, 30 fps, G < 75).

The rest of that run: M10 (the term on in 0 of 300 frames), M11 (target 0.3: error −1.01, no ceiling), L11 (face 90th
percentile 219), L12 (`lamp on, 14.9 s: face +1.75`), L15 (`from -8/0: face +2.07 at 8.0 s`), L17 (`Holding 30 fps ·
gain 75 of 75`, no suffix). M6, M7, L13, L14 and L16 pass on the old reading: they say "as before".

**After.** `cargo test -p better-cam --lib auto_controls`: 170 passed (152 before; +6 M, +1 sim, +10 L, +1 status).
`cargo test -p better-cam --lib fx::awb`: 23 passed, 1 ignored. `cargo clippy -p better-cam --tests`: clean.

**Measured against the model (11.5, 11.9).** The meter agrees with the model to the second decimal; the loop is inside
every threshold.

| Test | Model / spec | Rust |
|---|---|---|
| M8 recorded desk | 80 of 87 dark frames: `front ≤ −1.04`, gap ≥ 1.73, error ≤ +0.94 | 80 of 93 frames ask: least negative `front` −1.04, gap ≥ 1.73, error ≤ +0.94; 110.0-110.8 s: `front` −1.02..−0.08, gap 1.25-1.83; 163.9 s on: 0.000 |
| M9 | +2.3: −1.48; +0.8: −0.02; lamps −0.79..−0.84 | −1.48 (10: +0.24); −0.02 (10: +1.47); −0.78, −0.79, −0.82 |
| M11 | tenth at `Y_top` ±0.01 | 0.405 / 0.600 / 0.794, equal to `Y_top` (fronts −1.69 / −1.72 / −2.27) |
| M7 | `front` 0 at −1.5..+1; under that only where error ≥ +1.99 | 0.000 at −1.5..+1; worst −0.44 where error is +2.63 (dark skin, Corner +8, −3) |
| M10 | ≥ 30 of 300 under −0.5 | 58 under −0.5, 88 under 0, worst −3.00 (the bound) |
| L9 | face +0.71..+1.02, sides within 0.35 | +0.69..+1.02, sides within 0.21 (warm room −3), clip 0.000, 0 writes in 20-30 s |
| L10 | face 117-134, room ≤ 60 | face 120-132, room 29-45, 0 writes in 30-120 s |
| L11 | face p90 132-135 | 132-133; 31-37 batches in 30-120 s; never damped |
| L12 | 4-5 batches, ≤ 1 reversal, last ≤ 2.5 s | 4-6 batches, ≤ 1 reversal, last ≤ 2.8 s; "on" periods end at +0.69..+0.72 |
| L13 | ≤ 1 reversal per change | ≤ 1 per change; 2 in any 10 s at 4 s toggling (W4 needs 4) |
| L14 / L15 | +0.87..+1.03; +0.50..+0.64; even −0.3..+0.3 | +0.87; +0.48 / +0.63; even −0.04, `front` 0 |
| L16 | ≤ 2 batches | 0 batches in 20-120 s at every speed |
| L17 / L18 | suffix, `fl=-`; 30 fps, G < 75 | "Holding 30 fps · gain 23 of 75 · dark room: exposing for the lit middle", `fl=-1.51 gap=+3.27`; E −6, G 38, 30.00 fps, over=false |

**Deviations from the spec (numbers).**

1. **M6, the checker at +0.5 stop.** The kit's checker has gap 0.83 from −1 to +0.25 stop, as modelled. At +0.5 its
   white patches clip, drop out of the room (`src_i`) and the gap is **1.08** (spec: ≤ 0.83), 0.08 over the gate:
   `front` −0.016. M6 keeps `front == 0.0` and gap ≤ 1.0 everywhere else (14 pictures × 5 exposures, three lamps
   included) and allows `|front| ≤ 0.02`, gap ≤ 1.1 for that one frame. No constant was changed.
2. **10's M1 and M5 were edited, the claim is the same.** Both compare `error` with today's reading plus 10's term;
   they now compare `error − front` (M1: the checker at +0.5, above; M5: see 3). Their other asserts are unchanged.
3. **M5 finding (open).** 11 of M5's 200 random frames have `front ≠ 0`, up to **−1.15**; 4 of them are under −0.5. All
   have a "window" of −2..0 stops over the light-skin face, which is a bright wall region and no window, mostly in front
   of dark skin (dark-skin office, Top −1.41 at face −0.46: `front` −0.94, gap 1.78, error +0.13; TopAndSides −1.96 at
   −3.18: −1.15). Windows from +1.5 up give 0 (M7). It is the reflectance case of 11.10 (a bright region in the middle's
   upper half, a darker room), larger than the white shirt's half stop; it was not in the model's table.
4. **`σ` of the bands** is taken on `e − front` (what the loop decides on once the hold is added), not on raw `e`. The
   spec is silent; with raw `e` a moving face's 0.2-0.5 stop frame-to-frame term would widen the bands.
5. **`peek_error`** (what the white balance loop waits on) uses the held reading too; the spec is silent.
6. **Sorts are not shared.** `bright_tenth`, `room_level` and `clip_limit` each sort their 256 cells (three sorts per
   frame at ≤ 15 Hz, instead of one shared). `source_level` / `source_place` are shared. Behaviour is the same.
7. **`Replay` without white balance.** A replay now also feeds a sim that has no `wb` (alpha 0, the frame's own colour):
   `Replay::with_exposure` needs it for L10 / L11. Replays with `wb` behave as before.
8. **"Never damped"** (L10, L11, L12, L13, L16) is `would_damp`, W4's count (four reversals of moves ≥ 0.15 stop within
   10 s) run over the batches, as 10.10 used a proxy; the damping itself is not visible from outside.
9. **Choices the spec leaves open.** L13 runs at `dim(-6.0)` as L12; L14-L16 at `dim(-5.0)` as L9; M11 uses face +2.3 for
   all three targets; L12 / L13 judge the 15 s periods from the second on and L12's "on" ends at 15, 45, 75, 105 s; M7
   asserts `error ≥ 1.5` where `front < 0` (spec: ≥ +1.99; the four frames with the largest `front` read +2.38..+2.81).

**Left open.** The live check of 11.10 question 4 (the owner's dark room and lamp). The M5 finding (3): a dark-skin face
in front of a brighter wall region can lose up to 1.15 stops; narrowing it is a design change (for example a smaller
weight for the middle's cells above the middle row in the bright tenth) and was not made.

---

## 12. Big window behind the subject (design, 2026-11-19)

Status: design, not built. Builds on 10 and 11 as built (10.10, 11.11). Scope: one input of 10's backlight term in
`auto_controls/meter.rs`, and tests. `exposure.rs`, `status.rs`, `sim.rs`, Auto Adjust, white balance, the shaders and
the frontend are not touched.

**The gap (10.10, deviation 4).** The owner's third light: daylight from a window behind the desk. With a big window
(rows 0-9 of the picture) about 4 stops brighter than the face, a loop that comes from the dark side rests with the
face 1.63 stops under its rest in the office and 1.48 in the warm room. 10.5 had promised +0.2.

### 12.1 Recommendation

Give 10's term a subject reading with a **knee**: in the subject zone, no cell counts brighter than a coded level
1.8 stops over the target grey (0.794 at target 0.45). Use it only where 10.4 step 5 compares the zone with the
picture (`b`). Everything else stays as built. That includes `e_sub` as the reference of the light-source map (`G`,
the gate, 11's room level), the clip budget, the 3-stop cap, the gate and 11's ceiling.

- The bright window cells beside the head (18 % of the zone's weight in the big-window office) count at most 0.794,
  not 0.95-1.0. On the whole dark side the zone then asks for the face at −0.76 stop (office) and −0.70 (warm room)
  instead of −1.55 and −1.45. That is past the exposure at which the window turns white (about −1.5). The loop
  crosses it and lands on 10's good resting point, where the term is 0 as before.
- Measured with the real Rust meter, sim and loop (12.2), from the dark side: office big window +3.5 / +4 goes from
  −1.33..−1.63 to −0.06..−0.12 stop. Warm room big +4 goes from −1.48 / −1.54 to 0.00 / −0.03. Every other backlit
  row of 10 is within 0.06 stop of 10 as built (dark skin within 0.03) or better (office TopAndSides +3: −0.84 →
  −0.57; warm room Top +3: −0.45 → +0.03).
- Pictures whose gate is shut (`G` < 0.10: even rooms, lamps, lamp-lit rooms) read bit for bit as today. All 170 tests
  of 10 + 11 as built pass with the change. 11's scenes L9, L10 and L12-L18 run bit-identical. L11's moving face
  takes another path to the same rest.
- No new statistics, no dependency, no status or log change: one more bisection per frame while the gate is open.

Not fixed here: windows only 2.5-3.5 stops over the face (12.7). There the gate itself stays shut.

### 12.2 The gap, reproduced (Rust sim)

**Method.** The workspace was copied outside the repository and a harness test was added there. It drives the real
`measure_exposure`, `Sim` and `AutoController` (no Python model). Two copies: 10 as built, and 10 + 11 as built (the
files of 11.11, `meter.rs` of 18:30). The owner's preset (target 0.45, Longest "fit", Most gain 75, Normal, F 30),
`dim(-3.5)`, no noise unless stated, kit scenes with 10's `Window` (`Big` = rows 0-9, `TopAndSides`, `Top`).
"Offset" is `Sim::stops_over_need`, the face's stops from the windowless rest. "Rest" is its mean over 25-30 s. Seeds
−8 / 0 and −6 / 0 are the dark side, −2 / 0 the bright side. Scratch code only: nothing in the repository changed.
The knee runs used 0.794 (12.4) for the rest tables and 12.6, and 0.796 for the reading table, the dark-skin rows and
the noise and ramp runs; where both were run they agree within 0.03 stop (one gain step).

**Rests of 10 as built from the dark side** (seed −8 / 0; −6 / 0 within 0.1 except where two values are given):

| Window over the face | +2.5 | +3 | +3.5 | +4 | +4.5 | +5 | +6 |
|---|---|---|---|---|---|---|---|
| Office, Big | −1.36 | −1.69 | −1.33 / −1.39 | **−1.63** | −0.09 | −0.06 | −0.06 |
| Warm room, Big | −1.24 | −1.57 | −1.93 / −1.87 | **−1.48 / −1.54** | 0.00 | 0.00 | +0.03 |
| Office, TopAndSides | −1.24 | −0.84 | −0.21 | −0.21 | −0.18 | −0.18 | −0.18 |
| Warm room, TopAndSides | −1.20 | −1.36 / −1.45 | −0.15 | −0.12 | −0.09 | −0.09 | −0.15 |
| Office / warm room, Top | −0.87 / −0.81 | 0.00 / −0.45 | 0.00 / +0.03 | +0.03 | +0.03 / +0.06 | +0.03 / +0.06 | 0.00 / +0.06 |

From the bright side (−2 / 0) every Big and TopAndSides row from +3.5 up rests at −0.24..+0.03: two resting points.

**The reading along the exposure**, office, Big, +4 (`X*` = offset + error: where the reading sends the face):

| Offset | −3.0 | −2.5 | −2.0 | −1.5 | −1.0 | 0.0 | +0.5 |
|---|---|---|---|---|---|---|---|
| Face / window (/255) | 42 / 164 | 51 / 192 | 61 / 224 | 72 / 250 | 86 / 255 | 119 / 255 | 140 / 255 |
| `e_all` | +0.57 | +0.12 | −0.36 | +1.22 | +0.99 | 0.00 | −0.40 |
| `e_sub` | +1.45 | +0.95 | +0.45 | +0.02 | −0.25 | −0.75 | −1.03 |
| 10 as built: error, `X*` | +1.45, −1.55 | +0.95, −1.55 | +0.45, −1.55 | +1.22, −0.28 | +0.99, −0.01 | 0.00, 0.00 | −0.40, +0.10 |
| Knee (12.4): `e_face` | +2.24 | +1.74 | +1.24 | +0.74 | +0.24 | −0.75 | −1.03 |
| Knee: error, `X*` | +2.24, **−0.76** | +1.74, −0.76 | +1.24, −0.76 | +1.22, −0.28 | +0.99, −0.01 | 0.00, 0.00 | −0.40, +0.10 |

`G` is 0.36 (gate fully open) at every offset. The warm room reads the same way: `X*` −1.45 with 10 as built and
−0.70 with the knee on the dark side, both 0.08 once the window is white.

**Cause.**

1. **The reference, not the term.** `e_sub` does not depend on the exposure, by design. On the whole dark side it
   says "on target with the face at −1.55". The window cells that are not the person in rows 0-9 carry 27 % of the
   zone's weight, 18 % of it beside the head (rows 5-9). At the zone's target they are near white (0.95-1.0) and
   lift the zone's mean. The face, about half of the weight, is then 1.55 stops under.
2. **The loop stops before the window turns white.** The window reaches white at about −1.5 (250/255). There
   `e_all` jumps up (the near-white weight drops to 0.15) and the good resting point takes over. At −1.63 the window is
   245/255, `e_all` reads 0.00 and `e_sub` +0.11, so the error is +0.10, inside the hold band (0.33). 10.10 says "the
   term stops adding". In effect that is right, but the term never pointed further: 10's `b = a · (e_sub − e_all)`
   can take the face only to `e_sub`'s −1.55.
3. **Where the gate is shut it is another limit.** Office Big +3 (window 181/255 at its −1.69 rest) has `G` 0.00.
   Warm room Big +3.5 has 0.11 and warm room TopAndSides +3 has 0.12. Their window is not "white with the zone on
   target", because that test (10.4 step 3) also uses the polluted `e_sub` and the window is only 2.5-3.5 stops over
   the face. 12.7 covers it.

### 12.3 Options considered

All measured with the Rust sim. Each variant changes only the reading used for `b`, the source map stays on `e_sub`
unless stated. Rests from the dark side, 10 as built → variant.

| Option | Measured | Verdict |
|---|---|---|
| A. Narrower oval for the term (σ 0.3 / 0.25 instead of 0.4) | Big +4 office / warm room: −1.63 / −1.48 → −0.06 / 0.00; office TopAndSides +3 −0.84 → −0.21. Dark-skin backlit rests (10 as built +0.15..+0.39) → +0.36..+0.51 (σ 0.3) and +0.45..+0.75 (σ 0.25). Narrowing `SUBJECT_SIGMA` itself would also move 11's middle and source placement | Rejected: moves 10's dark-skin rows, and a narrower zone depends more on where the face is (the recorded desk has it lower right) |
| B. Zone leaves out cells read as sources (`z · (1 − src)`, one pass) | With `src` as in 10.4 ("white now" included): Big +4 +0.21 / +0.27; the good points of Top, TopAndSides and Big +5 / +6 rest 0.06-0.30 stop brighter; dark-skin rests +0.45..+0.60 (with 10's row placement on `src`: Big +4 +0.12 / +0.18). With only "white with the zone on target": Big +3.5 / +4 −0.03..−0.09 (office), +0.03 (warm room), good points within 0.06, but dark-skin rests +0.27..+0.39 (Top and TopAndSides 0.12-0.15 stop brighter than 10 as built) | Rejected: the first moves every good resting point; the second works but lifts dark-skin backlit faces, and its weights switch cell by cell on a threshold of the polluted `e_sub`. The knee does the same job with no dark-skin change |
| C. Weighted median of the zone instead of its mean | Big +4: −0.12 / +0.06; dark-skin rests +0.45..+0.75 (the median is the shirt and face mix) | Rejected (dark skin) |
| D. Zone without its brightest 20 % of weight (trimmed mean) | Big +4: −0.12 / +0.06; Top and TopAndSides good points 0.15-0.30 stop brighter; dark-skin rests +0.45..+0.75 | Rejected: also trims the face's own bright side when no window is in the zone |
| E. Source map with hysteresis | Not built: `G` is 0.36 and the gate fully open at the dark rest; the map is right, the reference is wrong | Rejected |
| F. **Knee on the zone's reading for the term** (12.4), 1.8 stops (0.794) | 12.5 | **Recommended** |
| F′. Knee at other levels (stops over the target, coded at T 0.45) | 0.9 stop (0.60, 11's `Y_top`): Big +4 −0.12 / +0.06, office TopAndSides +3 −0.18, dark-skin rests +0.45..+0.75. 1.4 stops (0.70): TopAndSides +3 −0.21, dark-skin rests +0.27..+0.42. 1.7 stops (0.772): as 1.8, dark-skin Top rows +0.03..+0.06. 2.0 stops (0.85): office Big +3.5 stays at −1.20 | 1.8: dark-skin rows within one gain step of 10 as built; 0.2 stop under the level that fails |
| G. F, and `e_face` also as the source map's reference | Also warm room Big +3.5 −1.81 → 0.00, office Big +3 −1.69 → −0.84, warm room TopAndSides +3 −1.33 → −0.51. M2 fails (desk 110.16 s: `backlight` 0.075 > 0.05, `G` 0.12), and 11's shared `source_level` input changes (its room level and its fade) | Rejected here; 12.7 |

### 12.4 The design

Notation of 10.4: covered cells, `lin_i`, `z_i` (`subject_weight`), `cov_i`, `T`, `e_all`, `e_sub`, `G`, `a`,
`e_clip`.

1. **Knee.** `Y_knee = min(SUBJECT_KNEE_MAX, srgb_encode(2^SUBJECT_KNEE_STOPS · srgb_decode(T)))`: 0.794 at T 0.45.
   Over the target range (0.2-0.8, `AutoCfg::sanitized`): 0.374 at 0.2, 0.542 at 0.3, 0.90 from T 0.52 up. Always
   above `T`.
2. **Subject reading with the knee.** `e_face = log2 k*` where
   `Σ z_i cov_i · min(Y_knee, srgb_encode(min(1, k* · lin_i))) / Σ z_i cov_i = T`. Same weights as `e_sub`, same
   bisection (±4 stops, 24 steps). Implemented as `solve` with a cap: `solve(cells, T)` is the cap 1.0, and
   `min(1.0)` after `srgb_encode` is exact, so every existing caller is bit for bit unchanged.
3. **Term.** *(Superseded by 13.4: `e_face` is now computed for every picture with a confidence, not only when
   `a > 0`, because it is also the reference of the light-source map.)* 10.4 step 5 becomes
   `b = a · clamp(min(e_face, e_clip) − e_all, 0, BACKLIGHT_MAX_STOPS)`. Compute `e_face` only when `a > 0`, as `e_clip`
   is today.
4. **Unchanged.** *(Superseded by 13.4: `e_sub` is no longer the reference of `source_level`; `e_face` is, so `G`, `a`,
   11's `room_level` and 11's fade follow it.)* `e_sub` stays the reference of `source_level` (so `G`, `a`, 11's
   `room_level` and 11's fade are as built), `e_clip`, the 3-stop cap, `error = clamp(e_all + b + f, ±4)`, and 11's `f`
   on `e_all + b`.

| Constant (`meter.rs`) | Value | Why |
|---|---|---|
| `SUBJECT_KNEE_STOPS` | 1.8 | 0.794 at T 0.45. A zone cell counts at most as bright as a lit face's highlight, not as white. At 1.8 the dark-skin backlit rows of 10 stay within one gain step (0.03); lower knees lift them (1.7: +0.03..+0.06, 1.4: up to +0.18). 2.0 (0.85) leaves office Big +3.5 at −1.20; 1.82 (0.80) still works. In stops over the target, like 11's `SUBJECT_TOP_STOPS`, so it follows the user's target. |
| `SUBJECT_KNEE_MAX` | 0.90 | From T 0.52 up the formula reaches 0.90 and soon white; 0.90 keeps a knee there (11's `SUBJECT_TOP_MAX`). |

No constant of 10 or 11 changes.

**Why it closes the gap.** At the zone's target the window cells beside the head count 0.794, not 0.95-1.0. The
zone's mean then needs the face 0.79 stop brighter: `X*` −0.76 instead of −1.55 (office), −0.70 instead of −1.45
(warm room), on the whole dark side. The window turns white at about −1.5, so the loop no longer rests before it. Once
it is white, `e_all` takes over as before. At the good point `e_face` (−0.75) is under `e_all` (0.00), so `b = 0`.
The rest is 10's good resting point, unchanged. A zone without a cell above 0.794 at its own target (every scene
without a light source) has `e_face = e_sub` exactly.

**Bounds.** Capping can only lower the zone's mean at every `k`, so `e_face ≥ e_sub` and the new `b` is never smaller
than 10's. The term stays one-way, at most 3 stops, and never past the clip budget (`min(·, e_clip)` is kept). 10's
bounds and M5 hold as written.

**No pumping.**

1. **The gate is untouched.** A picture with `a = 0` reads exactly as built: even rooms, lamps, lamp-lit rooms
   without a window, and desk frames with `G` < 0.10.
2. **Exposure-invariant.** `X + e_face` does not depend on the exposure while no zone cell is clipped (scaling). A
   clipped zone cell counts `Y_knee` as long as `e_face ≥ log2(srgb_decode(Y_knee))` = −0.75 stop, that is, unless
   the zone is already more than 0.75 stop too bright. Where the term can act, the reading therefore no longer
   depends on how far past white the window is, which `e_sub` (a clipped cell counted at its lower bound) did.
   Measured: `X + e_face` is −0.76 at every offset from −4.5 to −2.0 (office Big +4).
3. **Continuous.** The knee is a `min`, not a switch. 10's points (1)-(5) of 10.4 stand.
4. **Measured.** Big +4 office and warm room, and office Big +3.5, jitter 0.05, Slow / Normal / Fast, seeds −6 and
   −8, 120 s: 0 batches in 20-120 s at +4. Office +3.5 at Slow from −6 is still climbing at 20 s (0.5-stop steps),
   then quiet. Walking in front of the window (Big +4 ↔ Top every 15 s, 90 s): 0 batches per period after the first
   (10 as built: 3 in the first, then 0).

**Interplay with 11.** 11's `f = −g · clamp(e_all + b − e_top, 0, 3)` sees a larger `b` only where `a > 0`
(`G` ≥ 0.10), and 11's gate fades out over `G` 0.15-0.25. The one overlap on recorded frames is the desk at
110.03-110.64 s (`G` 0.111-0.113): `b` +0.006 at most, `f` −0.006 at most, error +0.004 at most (12.6).

**Status, log, cost.** No change to the texts or the log line. The `backlit` flag follows `backlight` as built: on
during the climb, off at rest, where `b = 0`. Cost: one 24-step bisection over the 256 cells per frame while the gate
is open, nothing while it is shut.

### 12.5 Effect on the scenes (Rust sim, measured)

Rests from the dark side, seeds −8 / 0 and −6 / 0, 10 as built → with the knee. From the bright side (−2 / 0) every row
is identical except warm room Top +3 (−0.03 → +0.06).

| Scene | 10 as built | New |
|---|---|---|
| **Office, Big +4** | **−1.63** (69/255) | **−0.06** (117), clip 0.45 |
| **Warm room, Big +4** | **−1.48 / −1.54** (68-69) | **0.00 / −0.03** (113-114) |
| Office, Big +3.5 | −1.33 / −1.39 | −0.06 / −0.12 |
| Office / warm room, Big +4.5, +5, +6 | −0.09..+0.03 | −0.06..+0.06 |
| Office, TopAndSides +3 | −0.84 | −0.57 / −0.63 (partly) |
| Warm room, Top +3 | −0.45 / −0.48 | +0.03 / +0.06 |
| Office and warm room, Top and TopAndSides +3.5..+6 | −0.24..+0.06 | the same within 0.06 |
| Dark-skin office, Top / TopAndSides / Big +3..+6 | +0.15..+0.39 (79-85) | the same within 0.03 |
| Office Big +3, warm room Big +3 / +3.5, warm room TopAndSides +3, every +2.5 | −0.81..−1.93 | the same within 0.12 (gate shut: 12.7) |
| Daylight rising (L2's ramp, Big +1.7 → +6.7 over 120 s, jitter 0.02) | face back within 0.5 stop at 52.1 s (warm room) / 50.8 s (office): window +3.9 / +3.8 | 45.8 / 44.7 s: window +3.6; from 60 s on 0.00 / −0.12 |
| Even rooms (M1's pictures, the sim's grey wall), lamps (M3), lamp-lit rooms without a window | rest X | identical, bit for bit (gate shut) |
| Recorded desk, 93 frames | `backlight` ≤ 0.031 | 6 frames (110.03-110.64 s) differ: error +0.004 at most, `backlight` ≤ 0.041 (M2's bound 0.05 holds) |
| 10's tests M1-M5, L1-L8 | pass | pass. L1, L3-L6 bit-identical; L2, L7 and L8 take other paths (L8: TopAndSides +3 rests at −0.63 instead of −0.84, suffix still shown) |

The 10.10 deviation-4 line now reads: office Big +4 −0.06, warm room Big +4 0.00. Every +4, +5 and +6 Big or
TopAndSides case lies between −0.24 and +0.06, from either side.

### 12.6 Effect on section 11

Run on 10 + 11 as built (the files of 11.11) with the knee: `cargo test -p better-cam --lib auto_controls` 170 passed,
as without it. Scene by scene (same scenes and seeds as the tests, 10 + 11 as built → with the knee):

| Scene | Result |
|---|---|
| L9 (4 rooms, both seeds), L10 (desk 111-116 s and 40-44.5 s), L12, L13 (15 s and 4 s), L14, L15 (both seeds and the even office), L16, L17, L18 | Bit-identical: same writes, same `face_over_need` at every tick |
| L11 (desk 108-116 s, Normal and Fast, both seeds) | Another path to the same rest: −0.64 (Fast −0.67 instead of −0.64), 45-53 writes in 120 s instead of 45-52, face / room 132-133 / 31. Passes (face p90 ≤ 175, never damped) |
| M6, M9, M11 | Identical (their pictures have the gate shut) |
| M7, M10 | Pass (M10's identity `error = clamp(e_all + b + f)` holds by construction) |
| M8 (desk) | The 6 frames above lie in its 110.0-110.8 s band (`front ≤ 0` only there): `front` −0.947 → −0.953 at most. The rest identical |
| 11.5 "window behind a lamp-lit face" rows | TopAndSides +4 / +5, Top and Corner +3..+5: identical. From the dark side: TopAndSides +3 +0.90 → +0.87 (room −3) and +1.17 → +1.11 (room −4); Big +4, room −3: +0.84 → +0.78; Big +3: −1.59 → −1.16 (room −3) and −0.86 → +0.99 (room −4, where the bright side rests at +1.20: the two sides now agree within 0.21). Bright sides identical |

Nothing in 11 moves by a threshold.

### 12.7 Risks and open questions

Known limits:

- **Windows 2.5-3.5 stops over the face stay dark from the dark side**: office Big +3 −1.69, warm room Big +3 / +3.5
  −1.57 / −1.81, warm room TopAndSides +3 −1.33, every +2.5 −0.8..−1.4. At those rests the gate is shut (`G` 0.00-0.12)
  because `source_level` uses the polluted `e_sub` too. Option G fixes part of it but opens the desk's gate (M2 0.075)
  and changes 11's shared source value. It needs its own design and a re-run of 11. From the bright side office
  Big +3 and warm room Big +3.5 rest at −0.09 / +0.03 (two resting points remain there); warm room Big +3,
  TopAndSides +3 and every +2.5 are dark from both sides (−0.66..−1.41): 10's term does not act there at all.
- **Margin of the knee.** Office Big +3.5 is the closest row: it holds at 1.82 stops (0.80) and fails at 2.0 (0.85).
  1.8 leaves about 0.2 stop of margin.
- **No face detection.** A lamp-lit face in the zone brighter than 0.794 is capped too, which raises `e_face`. With a
  window behind (gate open) this moves the 11.5 rows listed in 12.6: by 0.03-0.06 stop where they were already lit
  (the loop takes another path), and it lifts two dark-side rests of a 3-stop window (Big +3: −1.59 → −1.16 and
  −0.86 → +0.99).
- The Rust kit is the measurement here, not a model; the live picture can differ (10.9).

Questions for the owner:

1. **Windows only 2.5-3.5 stops brighter than the face** (overcast daylight, a big window): leave them as they are for
   now, or plan option G? Recommendation: first the live check (3). If the log shows `src=` under 0.15 with a dark
   face and a bright window, design G.
2. **Knee level**: 1.8 stops (dark-skinned faces in backlight as with 10), or 0.9 stop (also fixes office TopAndSides
   +3, but dark-skinned faces in backlight come out 0.5 stop brighter than with 10)? Recommendation: 1.8.
3. **Live check** (needs the owner and daylight behind the desk): Better Cam auto exposure on, Stop → Start (the dark
   side), wait 30 s. During the climb the log line shows `bl=` above 0; at rest `bl=+0.00` and `src=` 0.3 or more,
   the window white and the face as in the lit room. Repeat at dusk, when the window is only 3-4 stops over the face.

### 12.8 Build plan (one session)

Files: `src-tauri/src/auto_controls/meter.rs` (code and M tests), `src-tauri/src/auto_controls/tests.rs` (L tests),
this document (a "12.9 Built" note). Not touched: `exposure.rs`, `status.rs`, `sim.rs` (10's `Window` already has
`Big`, `Ramp` and `alternate`), `fx/*`, the shaders, the frontend.

1. **Fail first.** Add the tests below. Run M12, L19 and L21 against the meter as built and record their failing
   values in the Built note (expected: M12's 10 reading `X*` −1.55 / −1.45; L19 office Big +4 −1.63, office Big +3.5
   −1.33 / −1.39, warm room Big +4 −1.48 / −1.54; L21 50.8 / 52.1 s).
2. **`meter.rs`.** `solve(cells, target)` → `solve_capped(cells, target, cap)` (`.min(cap)` after `srgb_encode`), and
   `solve` calls it with 1.0. Add the constants of 12.4, `fn subject_knee(target) -> f64` (`Y_knee`) and
   `fn subject_error_knee(stats, target) -> f64` (`subject_error`'s weights, `solve_capped` with `Y_knee`). In
   `measure_exposure`, inside `if gate > 0.0`, use `e_face` instead of `e_sub` in `room`. `e_sub` keeps feeding
   `light_sources` and `front_light`. No new field on `ExposureReading`. Check: all 170 existing tests pass
   unchanged.
3. **Tests**, then the Built note: the measured values beside the ones here, and any threshold that moves, with its
   reason (as 10.10 and 11.11 did).

Tests (`auto_controls::meter` M, `auto_controls::tests` L; owner's preset, Normal, F 30, `dim(-3.5)` unless stated;
the measured values from 12.2 in parentheses). "10's reading" means the same frame read with `e_sub` in place of
`e_face`: a test helper built from `subject_error`, `light_sources`, `clip_limit` and `front_light`.

| Name | Scene | Pass |
|---|---|---|
| M12 `a_big_window_beside_the_head_does_not_hold_the_reading` | office Big +4 and warm room Big +4 (meter tests' `frame`), offsets −4.5..−2.0 in 0.25 steps (window ≤ 225/255); then −0.5..+1.0 (window white) | dark side: `offset + error ≥ −1.0` (−0.76 / −0.70) where 10's reading gives ≤ −1.3 (−1.55 / −1.45); white window: `error` equal to 10's reading (< 1e-9) |
| M13 `the_knee_only_adds_where_the_term_is_open` | (a) M1's pictures and exposures, M3's lamp, M9's lamp-lit frames, every `real::frames()` frame with `sources < 0.10`; (b) every `real::frames()` frame; (c) M5's 200 and M10's 300 random frames | (a) `error` equal to 10's reading (< 1e-12); (b) `|error − 10's| ≤ 0.01` (0.004), `backlight ≤ 0.05` (0.041); (c) `subject_error_knee ≥ subject_error − 1e-9`, `backlight ≥ 10's backlight − 1e-9`, M5's bounds |
| M14 `the_knee_reading_does_not_move_with_the_exposure` | office and warm room Big +4, offsets −4.5..−2.0; `subject_knee` at T 0.2, 0.45, 0.6, 0.8 | `offset + subject_error_knee` within 0.01 over the offsets (−0.76 / −0.70 throughout); `subject_knee` 0.374 / 0.794 / 0.90 / 0.90 (± 0.002) |
| L19 `a_big_window_is_on_target_from_the_dark_side` | office Big +3.5, +4, +6; warm room Big +4, +6; seeds −6 / 0 and −8 / 0; 30 s | after 8 s `stops_over_need` in [−0.5, +0.5] (−0.12..+0.06); `Snap.clip < 0.5` after 5 s; 0 writes in 20-30 s |
| L20 `a_big_window_holds_still_at_every_speed` | office and warm room Big +4, `jitter` 0.05, Slow / Normal / Fast, seeds −6 / 0 and −8 / 0, 120 s | ≤ 2 batches in 20-120 s (0); no guard batch; ≤ 1 reversal; no hand-back, not hunting; `stops_over_need` in [−0.5, +0.5] from 20 s (−0.09..+0.12) |
| L21 `rising_daylight_in_a_big_window_recovers_sooner` | L2's ramp (Big, +1.7 → +6.7 over 120 s, `jitter` 0.02), warm room and office, seed −6 / 0 | first tick after 10 s with `stops_over_need ≥ −0.5` at or before 48 s (45.8 / 44.7; 10 as built 52.1 / 50.8); `≥ −0.6` from 60 s on (0.00 / −0.12) |
| L22 `walking_in_front_of_a_big_window_moves_once_each_way` | office, `Big` +4 ↔ `Top` every 15 s (`Window::alternate`), 90 s | per 15 s period from 15 s on: ≤ 3 batches, ≤ 1 reversal (0 batches); no hand-back |
| — all existing | — | unchanged: 170 pass with the knee in the scratch copy |

**Done when**, from the repository root:

```
cargo test -p better-cam --lib auto_controls
cargo test -p better-cam --lib fx::awb
cargo clippy -p better-cam --tests
```

all pass (170, and 23 + 1 ignored, after 11.11; this adds 3 M and 4 L). Run them without piping stderr in PowerShell:
`2>&1` turns cargo's progress lines into errors and a non-zero exit code.

The thresholds come from the Rust sim itself, so their margins are small where the measured value is close. A
threshold may move with a measured reason in the Built note; a scene that misses by more than its margin is a finding
to report, not a constant to tune quietly.

### 12.9 Built (2026-11-19)

Built as 12.4 and 12.8 say: `SUBJECT_KNEE_STOPS` 1.8, `SUBJECT_KNEE_MAX` 0.90, `e_face` used only in 10's term
`b = a · clamp(min(e_face, e_clip) − e_all, 0, 3)`. `e_sub` still feeds the source map, the gate, 11's room level and
its fade. No constant of 10 or 11 changed. Not done here: the pointers in 2.4, 4.2 and 6.2 and the 10.10 deviation-4
line (this note is the only doc change).

**Files.** `meter.rs`: `solve` now calls `solve_capped(cells, target, cap)` (cap 1.0, so every existing caller is bit
for bit as before), the two constants, `subject_knee(target)`, `subject_error_knee(stats, target)`, and
`measure_exposure` uses `e_face` in `room` inside `if gate > 0.0`; tests M12-M14 (helper `without_knee`: the reading
with `e_sub` in place of `e_face`). `tests.rs`: L19-L22. `exposure.rs`, `status.rs`, `sim.rs`, `fx/*`: not touched.

**Fail first (knee functions present, `measure_exposure` still on `e_sub`).**

- M12: `office at -4.5 (window 101/255): error +2.95, X* -1.55 (10: -1.55)` (needs `X* ≥ -1.0`).
- L19: `office Big +3.5 from -6: face -1.39 stops from its rest at 8.0 s` (needs [-0.5, +0.5]).
- L21: `warm_room: face back within 0.5 stop at Some(52.1) s` (needs ≤ 48 s).
- L20 also failed (`warm_room Slow from -6: face -1.60 stops from its rest at 20.0 s`). M13 and M14 pass on the old
  reading (the knee is only added where the term is open; M14 tests the new function). L22 passes on the old meter
  (0 batches per period, as 12.4 says for 10 as built after the first period).

**After.** `cargo test -p better-cam --lib auto_controls`: 177 passed (170 before; +3 M, +4 L).
`cargo test -p better-cam --lib fx::awb`: 23 passed, 1 ignored. `cargo clippy -p better-cam --tests`: clean.

**Measured against 12.2 and 12.5.**

| Test | Spec | Rust |
|---|---|---|
| M12 | `X*` −0.76 / −0.70 (10: −1.55 / −1.45); white window equal to 10's | −0.75 / −0.69 at every offset −4.5..−2.0 (10: −1.55 / −1.45); window 101..224 (office), 97..214 (warm room); white-window frames equal within 1e-9 |
| M13 | desk: `|error − 10's|` ≤ 0.01 (0.004), `backlight` ≤ 0.05 (0.041) | 6 of 93 frames differ, largest 0.0039, `backlight` ≤ 0.0409; the other frames and every gate-shut picture equal within 1e-12 |
| M14 | `X + e_face` within 0.01; knee 0.374 / 0.794 / 0.90 / 0.90 | −0.752 / −0.691 constant to 3 decimals; knee as asked within 0.002 |
| L19 | [−0.5, +0.5] after 8 s (−0.12..+0.06), clip < 0.5, 0 writes | office +3.5 / +4 / +6: −0.12 / −0.06 / −0.06 (from either seed); warm room +4: −0.03 (−6), 0.00 (−8), +6: +0.03 / +0.06; clip 0.45; 0 writes in 20-30 s |
| L20 | ≤ 2 batches in 20-120 s (0), ≤ 1 reversal, inside [−0.5, +0.5] (−0.09..+0.12) | 0 batches, 0 reversals; −0.09..+0.12 (warm room Fast from −6); no hand-back, never hunting |
| L21 | ≤ 48 s (45.8 / 44.7), ≥ −0.6 from 60 s (0.00 / −0.12) | 45.8 s (warm room) / 44.7 s (office); 0.00 / −0.12 from 60 s on |
| L22 | ≤ 3 batches, ≤ 1 reversal per period (0) | 0 batches in every period, no hand-back |

**Deviations from the spec (numbers).** None in the thresholds: every one is as written. Small choices the spec leaves
open: M12's "window ≤ 225/255" is asserted as ≤ 227/255 (the office reads 224 at −2.0); M13 (c) uses the generators
and seeds of M5 and M10 and checks the knee against `subject_error`, 10's `backlight` and the term's 0..3 bound (M5's
and M10's own tests keep their clip-share and identity checks); L19-L22 start the controller from the seed as L1 does
(`resume(seed, 0)`, the sim's camera at its default start); M12 and L20 also cover the warm room as the spec lists.

**Left open.** Windows only 2.5-3.5 stops over the face (12.7, known limit) and the live check of 12.7 question 3.

---

## 13. Mid-bright windows and dark skin before a bright wall (design, 2026-11-19)

Status: design, not built. Builds on 10, 11 and 12 as built (10.10, 11.11, 12.9). Scope: the device exposure meter
(`auto_controls/meter.rs`), one log field (`exposure.rs`), two sim pieces (`sim.rs`) and tests. `status.rs`, Auto
Adjust, white balance, the shaders and the frontend are not touched.

**Two open problems, one shared cause: the subject zone does not know which bright cells are the subject.**

- **(A) Windows only 2.5-3.5 stops brighter than the face** stay 1.3-1.8 stops dark from the dark side (12.7). 10's
  light-source test ("white with the zone on target") uses `e_sub`, the zone's plain mean, and the window cells
  beside the head lift that mean. The window then never looks white enough to count, `G` stays under 10's gate and the
  backlight term never starts.
- **(B) The dark-room cap of 11 misfires on a face in front of a bright wall** (11.11, deviation 3). 11's "bright tenth
  of the middle" is the brightest 10 % of the zone's weight. When the wall beside the head is brighter than the face,
  the tenth is that wall, not the face; when the rest of the room is darker than the wall, the gap opens and the cap
  pulls the wall down to 0.60 coded, with the face 0.7-2.5 stops under it. Darker skin loses most. The owner wants
  this fixed: a darker-skinned face must never be pushed darker by the dark-room cap because of a bright wall.

### 13.1 Recommendation

Two changes in `meter.rs`, no new statistics, no dependency:

1. **(A) One subject reading.** 12's zone reading with the knee, `e_face`, replaces `e_sub` everywhere: as the
   reference of the light-source map (`G`, 10's gate, 11's room level and 11's fade), not only in 10's term. `e_sub`
   leaves the production path. A window 3 stops over the face now counts as a light source on the dark side (office
   *big* +3: `G` 0.000 → 0.260), so 10's term starts and lifts the face. White walls behind faces of every skin tone
   read exactly as before (360 frames, difference 0), because a knee at 1.8 stops does not touch a reflecting wall
   at the zone's target.
2. **(B) Spread fade on the dark-room cap.** 11's gate gets a third factor, `1 − ss(0.31, 0.41, S)`, where `S` is the
   horizontal spread (weighted standard deviation of `u`) of the bright tenth, leaving out cells that count as light
   sources in place. A lit face is one compact patch: `S` ≤ 0.191 in every lamp-lit kit scene (all three skin tones),
   0.07-0.28 on the recorded desk outside the face-at-the-edge frames (most frames 0.07-0.10). A bright wall behind a
   seated person shows on both sides of the head: `S` ≥ 0.412 in every bright-wall scene where the gap is open. The
   cap is then 0 there for every skin tone.

Measured with the real Rust meter, sim and loop:

- **(B)** In 900 bright-wall frames (office and warm room, light / medium / dark skin, a neutral region over the top,
  top and sides, or upper two thirds, from 3 stops darker than a light-skinned face to a white wall, five exposures)
  the cap is **0.0000** everywhere; as built it reaches −1.05 (light), −1.26 (medium), −1.59 (dark). 10's M5 set:
  11 frames with `front ≠ 0`, worst −1.151 → 2 frames, worst −0.053 (a face in front of a *darker* wall, where the
  tenth is the face itself). At rest a dark-skinned face before a bright wall goes from 65/255 back to 91-100/255,
  where it rests without the cap.
- **(B, dark room)** Every lamp-lit scene of the kit, all three skin tones, rooms −3..−5, both seeds: rests, writes and
  readings **bit-identical**. The recorded desk (L10's frames) bit-identical; the moving face (L11) rests within 0.03
  stop with 2-6 more writes in 120 s, never damped.
- **(A)** From the dark side: office *big* +3 −1.69 → −0.81, office *top* +2.5 −0.87 → −0.33, warm room *top and
  sides* +3 −1.33 → −0.51, warm room *big* +3.5 −1.81 → 0.00; medium skin office *big* +3 −1.48 → +0.09 and *top and
  sides* +2.5 −1.36 → −0.30. Dark-skin backlit rows are unchanged (*corner* +5 / +6 within 0.12). Windows of +2.5 behind light skin (*big*, *top and
  sides*) and warm room *big* +3 stay dark: relative to the zone they are as bright as a white wall behind a
  darker-skinned face, and opening the gate for them would brighten darker faces in front of white walls (13.3,
  option A2; 13.9 question 1).
- Even rooms, lamps in the picture and every test of 10-12 pass, except three that move by a stated amount (13.7):
  M2's bound 0.05 → 0.12, M13's helper and two bounds, L8's scene.

Constants: one added (`SUBJECT_TOP_SPREAD` (0.31, 0.41)), none of 10-12 changed. One field (`ExposureReading.spread`)
and one log field (`sd=`).

### 13.2 Both problems, reproduced (Rust sim)

**Method.** As 12.2: the workspace copied outside the repository (`D:\scratch\bc12\ws11f`, the repository's files of
18:57 with 12 as built: 177 tests pass there), a harness test module, and a per-thread switch in the copy's
`meter.rs` that selects each variant. It drives the real `measure_exposure`, `Sim` and `AutoController`. The copy's
sim got `SimCfg::skin` (13.8 step 1) so every kit scene runs with light (`skin_light`, luma 0.349), medium (`skin_mid`,
0.228, −0.61 stop) or dark (`skin_dark`, 0.101, −1.79 stop) skin, calibrated with that skin: "offset 0" is that
skin's evenly lit rest. A "window" or "neutral region" at level `L` is 10's `Window` with `L` stops over the
light-skin face whatever the scene's skin: up to +1.35 it is a reflecting surface (the kit's white paper; the grey
wall is +0.73), above that it is a light source. Owner's preset (target 0.45, Longest "fit", Most gain 75, Normal,
F 30). Rests are `stops_over_need` (or `face_over_need`) averaged over 25-30 s. Seeds −8 / 0 and −6 / 0 are the dark
side, −2 / 0 the bright side; lamp-lit rooms use 11's seeds −8 / 0 and −5 / 75. Nothing in the repository changed.

**(A) Windows 2.5-3.5 stops over the face, as built** (dark side, `dim(-3.5)`; light skin unless stated):

| Window over the face | Office | Warm room |
|---|---|---|
| *Top* +2.5 | −0.87 (bright side −0.72) | −0.81 (−0.66) |
| *Corner* +2.5 | −0.42 | −0.45 |
| *Top and sides* +2.5 / +3 | −1.24 / −0.57 | −1.20 / −1.33 |
| *Big* +2.5 / +3 / +3.5 | −1.36 / −1.69 / −0.06 | −1.24 / −1.57 / −1.81 |
| Medium skin, *top and sides* +2.5 / *big* +3 | −1.36 / −1.48 | −1.24 / −1.63 |

Along the exposure (office *big* +3; `X*` = offset + error, where the reading sends the face; `G` is 10's source
share):

| Offset | −2.0 | −1.5 | −1.0 | −0.5 | 0.0 |
|---|---|---|---|---|---|
| Face / window (/255) | 61 / 164 | 72 / 192 | 86 / 224 | 101 / 250 | 119 / 255 |
| `e_sub`, `e_face` | +0.98, +1.25 | +0.48, +0.75 | −0.02, +0.25 | −0.46, −0.25 | −0.75, −0.75 |
| As built: `G`, `b`, `X*` | 0.000, 0, −1.63 | 0.000, 0, −1.61 | 0.000, 0, −1.59 | 0.330, 0, −0.25 | 0.359, 0, 0.00 |
| `e_face` as map reference: `G`, `b`, `X*` | 0.260, 0.88, −0.75 | 0.260, 0.86, −0.75 | 0.260, 0.84, −0.75 | 0.330, 0, −0.25 | 0.359, 0, 0.00 |

With `e_sub` as the reference the window (164-224/255) is not "white with the zone on target", so `G` is 0 and the
term never starts; the loop rests where `e_all` alone sends it (−1.69). With `e_face` the same cells count, the term
lifts the face to `e_face`'s −0.75, and the loop rests at −0.81. The same in the other rows: warm room *top and
sides* +3 `G` 0.117 → 0.320, office *top* +2.5 0.053 → 0.232, warm room *big* +3.5 0.112 → 0.359.

**(B) The cap on bright walls, as built.** 10's M5 set (200 random frames): 11 with `front ≠ 0`, among them
`#134 office_dark_skin Top −1.41 at −0.46: front −0.938, gap 1.78`, `#140 office_dark_skin TopAndSides −1.96 at −3.18:
front −1.151, gap 2.52`, `#129 Top −1.28 at −1.60: −0.884`. Over the 900-frame grid of 13.1, `front` < −0.02 on
110 light, 165 medium and 165 dark-skin frames (worst −1.05 / −1.26 / −1.59).

What the tenth is (office, neutral region −2, the frame at offset 0): `onface` = share of the tenth's weight on skin or
hair cells; `S` = the tenth's horizontal spread (13.4 step 2).

| Scene | gap | `onface` | `S` | `front` |
|---|---|---|---|---|
| Lamp-lit face, room −3..−5, every skin tone (offset +0.5) | 0.49-4.28 | 0.99-1.00 | 0.17-0.19 | 0 (dark skin, room −3)..−1.70 |
| *Top and sides* −2 (wall left only beside the head), every skin tone | 1.77-2.52 | 0.00 | 0.48 | −0.78 / −0.97 / −1.27 |
| *Top* −2, medium / dark | 1.35 / 1.78 | 0.00 | 0.50 | −0.33 / −1.08 |
| *Big* −2 (wall left below row 9), every skin tone | 1.73-2.12 | 0.26-0.45 | 0.44-0.53 | −0.64..−0.69 |
| *Wall* −2 (a wall darker than the face), light | 2.03 | 0.99 | 0.18 | −0.47 |
| Recorded desk, frames the cap acts on | 1.7-3.2 | — | 0.07-0.28 | −1.04..−2.33 |

At rest (`dim(-6.0)`, so no row is at the gain limit; dark side; face /255):

| Office, region −2 | Light | Medium | Dark |
|---|---|---|---|
| *Top and sides*: as built / without the cap | −0.07 (117) / +0.75 (152) | −0.19 (96) / +0.78 (132) | −0.40 (65) / +0.87 (100) |
| *Top* | +0.60 / +0.60 | +0.17 (108) / +0.63 (126) | −0.40 (65) / +0.69 (94) |
| *Big* | +0.23 (128) / +0.93 (160) | +0.39 (116) / +0.99 (141) | +0.42 (86) / +1.08 (107) |

**Cause.** 11's tenth takes the brightest cells of the middle, wherever they are. Its only test that the bright part is
lit and not just bright is the gap to the room's 75th percentile, and a room with a darker part (a dark upper wall,
dark shelves, a curtain) opens that gap for a light wall as readily as a lamp does for a face. Lighter skin is caught
too (a grey wall is 0.73 stop over light skin), but darker skin is 1.3-2.5 stops under the same wall and goes darkest.

### 13.3 Options considered

All measured with the Rust sim as in 13.2.

| Option | Measured | Verdict |
|---|---|---|
| A1. `e_face` (knee 1.8) as the map reference, for 10 and 11 alike | 13.5. White walls (360 frames, every skin tone) identical to as built; desk: `backlight` up to 0.103 on 4 frames of 110.0-110.8 s (`G` 0.12), taken back by 11's ceiling (error within 0.011 of the same reading with `e_sub` in the term) | **Recommended** |
| A1′. A1 for 10 only, 11 keeps `e_sub` (two maps) | 11's scenes identical to A1 except the desk 108-116 s replay (writes in 120 s differ by at most 1) and one desk frame (163.26 s: 0.002 stop) | Rejected: two light-source shares for the same picture, no measurable gain |
| A2. Lower knee for the map (1.4, 1.2) | Adds warm room *big* +3 −1.57 → −0.75, warm room *top* +2.5 −0.75 → −0.27 (1.4), office *top and sides* +2.5 −1.24 → −0.57 (1.2). But white walls behind darker faces open the gate: office dark skin *top* +1.4 −0.45 → +0.24, *big* +1.4 −0.72 → −0.48 (1.4); at 1.2 also *top and sides* +1.4 −0.72 → +0.06, warm room *top* and *big* +1.4 | Rejected under "a scene without a light source reads as today"; 13.9 question 1 |
| A3. Lower knee for the term too (12's F′) | 12.3: lifts dark-skin backlit rests by up to 0.6 stop | Rejected (12) |
| B1. Smaller weight for the tenth's cells above the middle row (11.11's suggestion) | M5: 15 frames (weight 0.3) or 16 (weight 0) with `front ≠ 0`, worst −1.15: the wall beside the head runs from row 6 to row 12, below the middle row as much as above, and the face loses its upper half | Rejected |
| B2. Room level at the 90th percentile instead of the 75th | M5: 3 frames, worst −0.054. But the lamp-lit face's own edge cells sit in the room weights (`1 − z` at the oval's edge): the cap goes off for light skin at room −3 (the reading sends the face to +1.81 instead of +0.78), medium at −3 / −4 (+1.91 / +2.43 instead of +1.30 / +1.25), dark at −4 / −5 (+2.68 / +3.19 instead of +2.35 / +2.19) | Rejected: unfair to darker skin in the dark room |
| B3. Tenth against the oval's core (a darker core inside a bright ring = a backdrop) | The recorded desk has the lit face off-centre, the core is dark wall (`D_core` 1.4-3.4 on frames the cap must act on; 111.36 s has the face half in the core) | Rejected |
| B4. Skin colour of the tenth | Face detection by another name, depends on the white balance, and skin detectors are known to treat tones unequally | Rejected (owner's decision: no face detection) |
| B5. Horizontal spread of the tenth, every cell | Desk frames 110.0-110.8 s read 0.36-0.40 (window cells of the top-left corner enter the tenth with tiny weights, far from the face) | Rejected in this form |
| B5′. Spread without every source-like cell (`1 − src`) | On the desk the lit face itself is "white with the zone on target", so the spread is computed from almost nothing (0.00 at 112.44 s): right by accident | Rejected |
| B6. Spread without the cells that are light sources in place (`1 − p·src`), fade 0.31-0.41 | 13.5 | **Recommended** |

### 13.4 The design

Notation of 10.4, 11.4 and 12.4: covered cells, `lin_i`, `y_i`, `c_i = centre_i · cov_i`, `(u_i, v_i)`, `z_i`
(`subject_weight`), `p_i` (`source_place`), `T`, `ss` = `smoothstep`, `e_all`, `e_clip`, `e_face`, `Y_knee`.

1. **One subject reading.** When `confidence > 0`, compute `e_face` (12.4 step 2) and no longer `e_sub`. It is the
   reference of every per-cell source value:
   `src_i = max(ss(SOURCE_AT_SUBJECT, srgb_encode(min(1, 2^e_face · lin_i))), ss(SOURCE_WHITE_NOW, y_i))`.
   Then, as built but on this `src_i`: `G = Σ c_i p_i src_i / Σ c_i`, `a = ss(BACKLIGHT_GATE, G)`,
   `b = a · clamp(min(e_face, e_clip) − e_all, 0, BACKLIGHT_MAX_STOPS)` (12.4 step 3), 11's room weights
   `r_i = c_i (1 − z_i)(1 − src_i)` and 11's fade `1 − ss(FRONT_SOURCES_OFF, G)`.
2. **Spread of the bright tenth.** Take the tenth's cells as 11.4 step 2 takes them, with their indices:
   `(i, lin_i, t_i)`. Weight each by `w_i = t_i · (1 − p_i · src_i)`: a window or lamp cell that 10 counts as a light
   source in the upper part is left out; a lit face (in the zone, `p_i` ≈ 0) is not, even where it is "white with the
   zone on target". `ū = Σ w_i u_i / Σ w_i`, `S = sqrt(Σ w_i (u_i − ū)² / Σ w_i)`; `S = 0` when `Σ w_i = 0`.
3. **Gate.** `g = ss(FRONT_GATE, D) · (1 − ss(FRONT_SOURCES_OFF, G)) · (1 − ss(SUBJECT_TOP_SPREAD, S))`;
   `f = −g · clamp(e_all + b − e_top, 0, FRONT_MAX_STOPS)` and `error = clamp(e_all + b + f, ±4)` as built.
4. **Reading.** `ExposureReading` gains `spread = S` (0 when there is no tenth or no room, as `room_gap`). `sources`
   is the new `G`.

| Constant (`meter.rs`) | Value | Why |
|---|---|---|
| `SUBJECT_TOP_SPREAD` | (0.31, 0.41) | `u` runs −1..1 across the frame; a cell is 0.125, so 0.31 is about 2.5 cells. Lamp-lit faces ≤ 0.191 clean and ≤ 0.28 with 3 % per-cell noise (the tenth may then take the face's two outer columns: 0.31 is that worst case for the kit's six-cell face); the recorded desk 0.07-0.28 (most frames 0.07-0.10; 0.279 at 163.26 s, two lit patches), 0.32-0.38 only on the face-at-the-edge frames 110.0-110.8 s; bright walls ≥ 0.412 clean and ≥ 0.40 with noise (factor ≤ 0.03 there) |

No constant of 10, 11 or 12 changes; `SUBJECT_KNEE_STOPS` 1.8 now also sets the map's reference.

**Why the spread.** A lamp lights the person: the brightest tenth of the middle is the lit side of a face, one patch a
few cells wide, wherever the face is (the desk has it lower middle and lower right). A wall that is brighter than the
face is behind the person, and the person hides its middle: what is left of it in the zone is on both sides of the
head. The spread sees exactly that difference, and it does not depend on the face's reflectance: in lamp-lit scenes
the tenth is the face for every skin tone (`onface` 1.00, `S` 0.17), and in bright-wall scenes it is the wall for every
skin tone (`onface` 0.00-0.45, `S` 0.44-0.53).

**Bounds.** 10: `e_face ≥ e_sub` makes every `src_i` at least as large, so `G` and `a` are at least as large as built;
`b` stays one-way, ≤ 3 stops and within the clip budget (12.4). 11: the new factor is in 0..1, so `−3 ≤ f ≤ 0` holds
and `|f|` is never larger than built on the same `G` and room. The cap now acts on fewer pictures, never on more.

**Scenes without a light source.** Even rooms (M1's set): `G` ≤ 0.012, `backlight` 0, `front` 0, error as built.
White walls behind light, medium and dark skin (*top*, *top and sides*, *big*, *wall* at +0.5 / +1.0 / +1.4, five
exposures, 360 frames): error and `backlight` identical to as built (difference 0). Bright-wall scenes with a darker
room part (the 900 frames): `front` 0, everything else as built.

**No pumping and no second resting point.**

1. `X + e_face` does not move with the exposure while no zone cell is clipped (12.4), so neither does the map's
   "white with the zone on target" part.
2. `S` uses the tenth's fixed weights and the order of `lin_i`, both unchanged by scaling; `src_i` changes with the
   exposure only through "white now", that is when tenth cells clip (the middle 1.4 stops or more over `Y_top`, where
   the reading is strongly negative anyway). So `S` is constant along the loop's path in every measured scene.
3. Every new piece is continuous: `w_i` (smoothsteps, the tenth's partial last cell), `S`, the fade.
4. Measured: office, three skin tones, *top and sides* −2, *top* −1.4, *one side* −2, *big* +3, *top and sides* +3,
   `jitter` 0.05, Slow / Normal / Fast, 120 s: 0 moves after 20 s in all 45 runs (as built: 0 too). A lamp-lit face
   leaning ±1 stop in front of a bright strip (*top and sides* −0.5 / 0 / +0.5, room −3) every 15 s and every 4 s,
   three skin tones: moves and reversals identical to as built, at most 3 reversals in any 10 s (W4 needs 4), no
   hand-back. The lamp-lit desk replay 108-116 s: 2-6 more writes in 120 s (13.5), never damped.

**Status, log, cost.** No status text changes; the backlit and front-lit flags follow `backlight` and `front` as
built (a +3 window now keeps `backlight` ≥ 0.5 at rest, so its hold says " · backlight: exposing for the middle").
Log line: append `sd=0.17` (median `spread`). Cost: `e_face`'s bisection replaces `e_sub`'s (same cost), and the term
no longer needs its own; one more pass over the tenth (≤ 30 cells).

### 13.5 Effect on the scenes (Rust sim, measured)

**Windows behind the person** (`dim(-3.5)`; dark side −8 / 0 | bright side −2 / 0; −6 / 0 within 0.09 of −8 / 0 in
every row; level over the light-skin face):

| Scene | As built | New |
|---|---|---|
| Office *big* +3, light | −1.69 \| −0.09 | **−0.81** \| −0.09 |
| Office *top* +2.5, light | −0.87 \| −0.72 | **−0.33** \| −0.30 |
| Warm room *top and sides* +3, light | −1.33 \| −1.29 | **−0.51** \| −0.51 |
| Warm room *big* +3.5, light | −1.81 \| +0.03 | **0.00** \| +0.03 |
| Office *top and sides* +2.5, medium | −1.36 \| −1.32 | **−0.30** \| −0.33 |
| Office *big* +3, medium | −1.48 \| +0.09 | **+0.09** \| +0.09 |
| Warm room *top and sides* +2.5, medium | −1.24 \| −1.17 | **−0.24** \| −0.30 |
| Warm room *big* +3, medium | −1.63 \| +0.21 | **+0.21** \| +0.21 |
| Office *big* +2.5, medium | −1.42 \| −1.38 | −1.21 \| −1.23 |
| Warm room *top* +2.5, light | −0.81 \| −0.66 | −0.75 \| −0.60 |
| *Corner* +5 / +6, light and medium, both rooms | −0.18..−0.06 \| −0.03..0.00 | −0.09..+0.06 \| −0.03..+0.03 |
| Still dark: light *top and sides* +2.5 (office / warm room), light *big* +2.5, warm room light *big* +3, office light *top and sides* +3, every *corner* +2.5, warm room medium *big* +2.5 | −1.24 / −1.20, −1.36 / −1.24, −1.57 \| −1.41, −0.57, −0.39..−0.45, −1.30 | the same |
| Every other light and medium row, *top*, *corner*, *top and sides*, *big*, +3..+6 | −0.24..+0.21 | the same within 0.06 |
| Dark skin, every shape, +2.5..+6, both rooms | +0.03..+0.51 | identical, except *corner* +5 / +6 (office −0.03 / +0.06 → +0.09, warm room −0.09 / +0.09 → +0.06) |
| L2 / L21 ramp (*big*, +1.7 → +6.7 in 120 s): face back within 0.5 stop | 45.8 s (warm room) / 44.7 s (office), 17 writes | **39.6 / 38.5 s** (window +3.3), 13 writes; from 60 s +0.03 / −0.03 |
| Noisy (`jitter` 0.05), office *big* +3, Slow / Normal / Fast | −1.75 / −1.60 / −1.63 | −0.87 / −0.78 / −0.75, 0 moves after 20 s |

**Faces before a bright wall** (`dim(-6.0)`, dark side; as built → new; "no cap" = the same meter with 11's term
forced to 0; face /255):

| Scene | Light | Medium | Dark |
|---|---|---|---|
| Office *top and sides* −2 | −0.07 (117) → +0.75 (152) | −0.19 (96) → +0.78 (132) | −0.40 (65) → +0.87 (100) |
| Office *top* −2 | +0.60 → +0.60 | +0.17 (108) → +0.63 (126) | −0.40 (65) → +0.69 (94) |
| Office *big* −2 | +0.23 (128) → +0.93 (160) | +0.39 (116) → +0.99 (141) | +0.42 (86) → +1.08 (107) |
| Warm room *top and sides* −2 | +0.11 (118) → +0.87 (151) | −0.04 (97) → +0.90 (131) | −0.25 (65) → +0.96 (98) |
| Warm room *top* −2 / *big* −2 | +0.66 → +0.66 / +0.63 → +0.90 | +0.42 → +0.66 / +0.93 → +1.05 | −0.25 → +0.72 / +0.96 → +1.17 |
| All of the above, region −1 | the same pattern, 0.2-0.4 stop smaller | | |
| New = no cap in every row above | yes (warm room light *big* −2: +0.90 against +1.11, another path to a rest inside the band) | yes | yes |

New is where 10 and 12 put the face before 11 existed: brighter than the face's evenly lit rest by 0.4-1.2 stops,
as the plain mean puts any face in front of a bright wall with darker surroundings. The cap no longer takes it below.

**Dark room, lamp-lit face** (`dim(-5.0)`, 11's seeds −8 / 0 | −5 / 75): identical rests, writes and `front` for every
skin tone, both rooms, rooms −3 / −4 / −5:

| Room | Light | Medium | Dark |
|---|---|---|---|
| −3 | +0.69 \| +0.78 (office), +0.81 \| +1.02 (warm) | +1.17 \| +1.35, +1.29 \| +1.47 | +1.98 \| +2.13 (both) |
| −4 | +0.69 \| +0.75, +0.81 \| +0.90 | +1.14 \| +1.32, +1.26 \| +1.44 | +2.25 (gain limit) |
| −5 | +0.69 \| +0.69, +0.81 \| +0.60 | +1.14 \| +1.29, +1.26 \| +1.32 | +2.04 \| +2.25, +2.19 \| +2.25 |
| `front` at rest | −0.90..−1.82 | −0.51..−1.57 | 0 (−3), −0.24 / −0.33 (−4), −0.86 / −1.01 (−5) |

**Window behind a lamp-lit face, dark room** (office, *top*, *corner*, *top and sides*, *big* at +3 / +4 / +5, rooms
−3 / −4, light, medium and dark skin, both seeds; 72 scenes): identical rests and `front`, except light skin *big* +3,
room −3, from the dark side: −1.16 → +0.81 (now as from the bright side, +1.02). Medium skin rests +0.90..+2.07, dark
+1.56..+2.25, as built.

Also identical: a lamp-lit face with a lit strip of wall beside it (*top and sides* −1 / 0, rooms −3 / −4, every skin
tone), every lamp-in-the-picture frame (M9's three places: `S` 0.17-0.21) and L12-L18.

**The recorded desk.** Static (`real::frames()`): 85 of 93 frames identical; 163.26 s +0.002 stop; the face-at-the-edge
frames 110.03-110.76 s move by 0.034-0.321 stop (`front` −0.08..−1.03 → −0.04..−0.94; `S` 0.32-0.38: the face is at
the right edge with the window's lower cells in the tenth), `backlight` up to 0.103 (110.76 s). Replays: L10's frames
(111-116 s, 40-44.5 s, both seeds) bit-identical, face 127-134, room 30-45. L11's frames (108-116 s): rests −0.64 /
−0.70 (as built −0.64 / −0.67), writes in 120 s 57 / 50 / 56 / 48 (as built 51 / 48 / 53 / 46), face / room
131-133 / 31, never damped.

**Sections 10-12.** Even rooms and lamps (M1, M3, M6): identical. L1, L3-L7, L9, L10, L12-L18: bit-identical. L2 and L8
take other paths (L2 +0.00 → +0.03, 17 → 13 writes; L8's *top and sides* +3 rests at −0.57 instead of −0.63 with
`backlight` 0.17, so its suffix goes: 13.7). L11 as above. L19-L22 pass (office *big* +3.5 from −8: −0.06 → −0.12, one
gain step; L21 in the table above).

### 13.6 Skin tones

The fairness requirement of the brief, case by case (as built → new):

| Case | Light | Medium | Dark |
|---|---|---|---|
| Bright wall on both sides of the head, darker room part (900 frames): worst `front` | −1.05 → **0** | −1.26 → **0** | −1.59 → **0** |
| The same at rest (*top and sides* −2, office) | −0.07 → +0.75 | −0.19 → +0.78 | −0.40 → +0.87 |
| White wall behind the person (360 frames): error, `backlight` | identical | identical | identical |
| Lamp-lit face, dark room (rooms −3..−5, both rooms, both seeds) | identical | identical | identical |
| Window behind the person, +2.5..+6 | 4 rows lifted (13.5) | 4 rows lifted | identical |
| Bright wall on **one** side only (*one side* −2, the residual; at rest, no cap in brackets) | `front` −0.70, +0.30 (+0.93) | `front` −0.71, +0.23 (+0.99) | `front` −0.04, +0.39 (+1.08) |
| Wall darker than the face (*wall* −2: the tenth is the face) | `front` −0.47, +0.66 (+1.20) | `front` −0.04, +1.14 (+1.26) | 0, +1.36 |

The residual: a bright patch on one side of the head only is, cell for cell, what the recorded desk shows when the lit
face sits off-centre (110.88 s: `S` 0.29; 163.26 s: 0.28; the *one side* strip: 0.28-0.29). No rule found keeps the
desk and catches it (B3). Its bound, measured: the cap takes at most 0.71 stop there and every face still rests at
least 0.2 stop *over* its evenly lit rest (+0.20..+0.84). With this design the cap does not put a face under its
evenly lit level in any measured scene.

A pre-existing asymmetry, not changed here: 11 aims every lamp-lit face at the same coded level (the tenth at 0.60),
so a darker-skinned face in a lamp-lit dark room ends about 2 stops over its evenly lit look (+1.98..+2.25) where a
light-skinned one ends 0.7-1.0 over; at room −3 the cap does not act on dark skin at all (gap 0.49). 13.9 question 3.

### 13.7 What moves in existing tests

| Test | Change | Reason (measured) |
|---|---|---|
| M2 `the_recorded_desk_reads_as_before` | `backlight ≤ 0.05` → `≤ 0.12` | 0.103 at 110.76 s (`G` 0.123; 12's option G measured 0.075 at 110.16 s). 11's ceiling takes it back: the error moves by ≤ 0.011 against the same reading with `e_sub` in the term (M13 b) |
| M13 `the_knee_only_adds_where_the_term_is_open` | Helper `without_knee` = the same reading with `subject_error` in place of `subject_error_knee` **in the term only** (map, room, fade and spread as new); (b) `|error − helper|` ≤ 0.01 → ≤ 0.02 and `backlight` ≤ 0.05 → ≤ 0.12 | (b) 7 desk frames differ, largest 0.011 (110.16 s); (a) and (c) as written |
| L8 `a_backlit_hold_says_so` | Scene office *top and sides* +3 → office *big* +3 | *top and sides* +3 now rests at −0.57 with `backlight` 0.17 (no suffix, correctly); *big* +3 rests at −0.81 with `backlight` 0.83: suffix and `bl=+0.8` |
| The log test (`the_log_line_says_where_the_loop_is_and_what_it_saw`) | Extended with `sd=0.00` in a normal room | New field |
| 11.11 deviation 3 (M5 finding) | Closed: 2 frames, worst −0.053, both a face in front of a darker wall | B |
| Every other test of 10-12 | Unchanged, passes (174 of 177 in the scratch copy before the three edits above) | — |

### 13.8 Build plan (one session)

Files: `src-tauri/src/auto_controls/meter.rs` (code, M tests), `exposure.rs` (one field), `sim.rs` (two pieces),
`tests.rs` (L tests, three edits), this document (a "13.10 Built" note). Not touched: `status.rs`, `fx/*`, the shaders,
the frontend.

1. **`sim.rs`.** `SimCfg::skin: Option<&'static str>`: in a kit scene every cell whose kit name starts with `skin` gets
   `scenes::surface(skin)` (`skin_light`, `skin_mid`, `skin_dark`) for its colour and luma, before calibration, so
   `need` is that skin's evenly lit rest. `WindowShape::OneSide`: rows 0-5, columns 0-4 down to row 12 and columns
   13-15 down to row 11 (the wall is left only right of the head). Check, sim test
   `a_skin_override_keeps_the_scene_calibrated`: office with `Some("skin_light")` has the same picture and
   calibration as `None`; office with `Some("skin_dark")` the same as `office_dark_skin`.
2. **Fail first.** Add the tests below (with `tenth_spread` as a stub returning 0 that `front_light` does not use yet)
   and run M15, M17, M18, L23, L25 and the edited L8 against the meter as built (12.9); record the failing values in
   the Built note. Expected: M15 `front` −1.589 (office dark skin *top and sides* −3 at +1); M17 walls `S` 0 (needs ≥
   0.41); M18 office *big* +3 `G` 0.000 (needs ≥ 0.2); L23 office dark *top and sides* −2 rest −0.40 (needs ≥ +0.4);
   L25 office *big* +3 −1.69 (needs ≥ −1.0); L8 no suffix (office *big* +3 rests with `backlight` 0).
3. **`meter.rs`.** The constant `SUBJECT_TOP_SPREAD`. `measure_exposure`: `e_face = subject_error_knee(..)` once,
   `sources = light_sources(stats, e_face)`, the term on `e_face` (no second call), and
   `front_light(stats, target, e_face, sources, e_all + backlight)`. `subject_error` becomes `#[cfg(test)]` (M13's
   helper). `bright_tenth` also returns each taken cell's index (`(usize, f64, f64)`); `fn tenth_spread(tail, k) ->
   f64` (13.4 step 2; `k = 2^e_face`, `source_level` and `source_place` as built); `front_light` multiplies its gate
   by `1 − ss(SUBJECT_TOP_SPREAD, S)` and returns `S`; it takes a `fade: bool` (production `true`) so the tests can
   build the as-built reading. `ExposureReading.spread`. Check: every existing meter test except M2 and M13 passes
   unchanged.
4. **`exposure.rs`.** `Sample` carries `spread`; `Reading.spread` is its median; the log line gets `sd=` after `gap=`.
5. **Test edits** of 13.7 (M2, M13, L8, the log test), then the Built note: measured values next to the ones here, and
   any threshold that moves, with its reason.

Tests (`auto_controls::meter` M, `auto_controls::tests` L; owner's preset, Normal, F 30; skins by `SimCfg::skin`;
"as built" = a test helper giving 10 + 11 + 12's reading: `e_sub` as the map's reference, `e_face` in the term,
`front_light(.., fade: false)`; measured values in brackets):

| Name | Scene | Pass |
|---|---|---|
| M15 `bright_walls_never_trigger_the_dark_room_cap` | office and warm room × light / medium / dark × *top*, *top and sides*, *big* at −3, −2.5, …, +1.0, +1.4 × offsets −1..+1 in 0.5 (900 frames) | `front == 0.0` (all 0; as built down to −1.589); `backlight` within 1e-9 of as built (measured 0) |
| M16 `a_lamp_lit_face_keeps_its_ceiling_for_every_skin_tone` | lamp-lit office and warm room × three skins × rooms −3, −4, −5 × offsets −1, 0, +0.8, +1.5, +2.3 (90 frames) | `front` and `error` equal to as built (< 1e-12; measured 0); `spread ≤ 0.25` (0.191) |
| M17 `the_tenth_spread_tells_a_lit_face_from_a_wall_behind_it` | (a) M16's frames; (b) every `real::frames()` frame with `room_gap > 1` outside 109.9-110.8 s; (c) M15's frames with `room_gap > 1`; (d) office *top and sides* −2, *top* −1.4, *big* −2 and lamp-lit room −4, three skins, 300 copies each with every cell times `2^(0.03·gauss)` (fixed seed) | (a) `S ≤ 0.25` (0.191); (b) `S ≤ 0.30` (0.279 at 163.26 s); (c) `S ≥ 0.41` (0.412); (d) walls `|front| ≤ 0.05` (≤ 0.01), lamp-lit `front` within 0.15 of the noiseless frame (0.10, dark skin) |
| M18 `a_window_three_stops_over_the_face_counts_as_a_light_source` | offsets −2..−1 in 0.25: office *big* +3, warm room *top and sides* +3, office *top* +2.5; warm room *big* +3.5 at −2..−1.25 | `sources` ≥ 0.2 / 0.25 / 0.18 / 0.30 (0.260 / 0.320 / 0.232 / 0.359; as built 0.000 / 0.117 / 0.053 / 0.112); offset + error ≥ −0.9 / −0.6 / −0.4 / −0.9 (−0.75 / −0.41 / −0.21 / −0.69; as built −1.59..−1.82) |
| M19 `white_walls_behind_any_skin_tone_read_as_before` | office and warm room × three skins × *top*, *top and sides*, *big*, *wall* at +0.5, +1.0, +1.4 × offsets −1..+1 (360 frames) | `error` and `backlight` equal to as built (< 1e-9; measured 0) |
| M20 `the_recorded_desk_moves_only_at_the_edge_of_the_middle` | every `real::frames()` frame | outside 110.0-110.8 s `|error − as built| ≤ 0.005` (0.002 at 163.26 s); inside `front ≤ 0` and `|error − as built| ≤ 0.4` (0.321 at 110.76 s) |
| L23 `a_face_before_a_bright_wall_is_not_darkened_for_any_skin_tone` | office and warm room × three skins × *top and sides* −2, *top* −2, *big* −2; `dim(-6.0)`; seeds −8 / 0 and −2 / 0; 30 s | `|front|` of `measure_exposure(&sim.picture())` ≤ 0.01 every second from 10 s on (0.00); after 10 s `stops_over_need ≥ +0.4` (min +0.60, light *top* −2; as built down to −0.40) |
| L24 `a_lamp_lit_face_of_every_skin_tone_rests_as_before` | lamp-lit office and warm room × three skins × rooms −3, −4, −5; `dim(-5.0)`; 11's seeds | after 10 s `face_over_need` in [+0.4, +1.3] light, [+0.9, +1.7] medium, [+1.7, +2.3] dark (13.5: +0.60..+1.02, +1.14..+1.47, +1.98..+2.25); at 30 s `front` ≤ −0.8 light, ≤ −0.4 medium room −3 and ≤ −0.9 rooms −4 / −5, ≤ −0.7 dark room −5; 0 writes in 20-30 s |
| L25 `a_window_three_stops_over_the_face_is_lifted_from_the_dark_side` | `dim(-3.5)`, seeds −8 / 0 and −6 / 0, 30 s: office *big* +3; warm room *big* +3.5; warm room *top and sides* +3; office *top* +2.5; office *big* +3 medium skin; warm room *big* +3 medium skin | after 10 s `stops_over_need` ≥ −1.0 / −0.3 / −0.7 / −0.5 / −0.3 / 0.0 (−0.81 / 0.00 / −0.51 / −0.33 / +0.09 / +0.12..+0.21; as built −1.69 / −1.84 / −1.42 / −0.87 / −1.48 / −1.63); `clip < 0.5` after 5 s; 0 writes in 20-30 s |
| L26 `bright_walls_and_three_stop_windows_hold_still` | office × three skins × *top and sides* −2, *top* −1.4, *one side* −2, *big* +3, *top and sides* +3; `jitter` 0.05; Slow / Normal / Fast; 120 s | ≤ 2 batches in 20-120 s (0), no guard batch, ≤ 1 reversal, no hand-back, not hunting |
| L27 `leaning_in_front_of_a_bright_strip_does_not_pump` | lamp-lit office room −3, *top and sides* at −0.5 / 0 / +0.5, face `Toggle { a: +1, b: −1 }` every 15 s and 4 s, three skins, `dim(-6.0)`, 120 s | never damped (`would_damp` false; at most 3 reversals in 10 s), no hand-back, not hunting |
| L28 `a_one_sided_bright_wall_stays_bounded` | office and warm room × three skins × *one side* −2 and −1, and *wall* −2; `dim(-6.0)`; seeds −8 / 0 and −2 / 0 | after 10 s `stops_over_need ≥ +0.15` (min +0.20); `front ≥ −0.8` (−0.71) |
| — the sim check of step 1, the edits of 13.7, all other existing tests | — | as 13.7 |

**Done when**, from the repository root:

```
cargo test -p better-cam --lib auto_controls
cargo test -p better-cam --lib fx::awb
cargo clippy -p better-cam --tests
```

all pass (177, and 23 + 1 ignored, after 12.9; this adds 6 M, 6 L and 1 sim test: 190). Run them without piping
stderr in PowerShell: `2>&1` turns cargo's progress lines into errors and a non-zero exit code.

The thresholds come from the Rust sim itself; where a margin is small (M17 (b) 0.021, (c) 0.002; L23's minimum) a
miss by more than the margin is a finding to report, not a constant to tune quietly.

### 13.9 Risks and open questions

Known limits:

- **No face detection.** A bright patch on one side of the head only is read as a lit face (13.6): the cap takes up to
  0.71 stop, faces still rest at least 0.2 stop over their evenly lit level. A lit face wider than about a third of the
  frame and lit across its whole width (very close to the camera) can reach `S` 0.31 and lose part of the cap; the
  kit's lamp-lit close-up reads 0.22 and has gap 0 anyway.
- **The desk's face at the frame's edge** (110.0-110.8 s, 0.8 s) loses part of the cap (`front` −1.03 → −0.77 at
  110.76 s). The 2 s hold covers it in the loop (L11: 2-6 more writes in 120 s, never damped). A lit face that *stays*
  at the right edge with the window's lower cells in its tenth would get less of the cap than as built.
- **Windows of +2.5 behind light skin** (*big*, *top and sides*) and warm room *big* +3 stay 1.2-1.6 stops dark, and
  office *big* +3 / *top and sides* +3 rest at −0.81 / −0.57. The first need a map that would also open on white walls
  behind darker faces (question 1); the second are 12's knee limit on the term (12.3 F′: a lower knee lifts dark-skin
  backlit faces).
- **Margins.** Lamp-lit faces under noise reach `S` 0.28 (edge 0.31); bright walls under noise 0.40 (edge 0.41, factor
  0.03). Both come from the kit's geometry (a six-cell face, walls two to three cells beside it); the live log's `sd=`
  is the check.
- The sim kit is the measurement here, the live picture can differ (10.9, 12.7).

Questions for the owner:

1. **Map knee 1.8 or 1.4?** 1.8 (recommended) keeps every white-wall scene as before for every skin tone. 1.4 also
   lifts warm room *big* +3 (−1.57 → −0.75) and warm room *top* +2.5 (−0.75 → −0.27), but then a white wall behind a
   dark-skinned face opens the backlight gate and brightens the face: office *top* +1.4 −0.45 → +0.24, *big* +1.4
   −0.72 → −0.48. That is closer to the face's evenly lit look (0), and it breaks "a scene without a light source reads
   as today" for those scenes only.
2. **The one-sided bright wall** (13.6): accept the bound (cap ≤ 0.71 stop, faces stay over their evenly lit level), or
   ask for face detection later? Recommendation: accept; check it live (question 4).
3. **Lamp-lit darker skin** (pre-existing, 13.6): every lamp-lit face is aimed at about 150/255, which is 2 stops over a
   dark-skinned face's evenly lit look and 0.7-1.0 over a light one's. A separate design could aim the tenth lower when
   the tenth is darker than the room's brightest reflecting surfaces, or let the owner set the lit-face level.
   Recommendation: decide after the live check of 11.10 question 4.
4. **Live check** (needs the owner): the log's `sd=` with the lamp-lit face in the dark room should read under 0.3
   and `fl=` stay as before; sitting in front of a light wall or door with the room around it darker, `sd=` should
   read over 0.4 and `fl=+0.00`. At dusk, with the window behind about 3 stops over the face, Stop → Start: `src=`
   should read 0.2 or more on the dark side and the face should come up within 10 s.

### 13.10 Built (2026-11-19)

Built as 13.4 and 13.8 say: `e_face` (`subject_error_knee`) is the one subject reading (light-source map, 10's gate, 11's
room level and fade, 10's term), and 11's gate has the third factor `1 − ss(0.31, 0.41, S)`. New constant
`SUBJECT_TOP_SPREAD` (0.31, 0.41), new field `ExposureReading.spread`, new log field `sd=`. No other constant of 10-12
changed. Not done here: the pointers in 2.4, 4.2 and 6.2, the 10.10 and 11.11 notes (11.11 deviation 3 is closed by
this build, see 13.7) and 12.9 (this note is the only doc change).

**Files.** `meter.rs`: `SUBJECT_TOP_SPREAD`; `measure_exposure` computes `e_face` once and uses it everywhere (`e_sub`
is gone from the production path; `subject_error` is `#[cfg(test)]`); `bright_tenth` returns `TenthCell`s `(index, lin,
weight)`; `tenth_spread` (13.4 step 2); `front_light(.., fade)` returns `(front, gap, spread)` and multiplies its gate
by the spread fade; `ExposureReading.spread`. Tests M15-M20, helpers `skin_frame`, `skin_lit_frame`, `as_built`,
`term_without_knee`, `noisy`, `wall_frames`, `lamp_frames`; edited M2 and M13. `exposure.rs`: `Sample.spread`,
`Reading.spread` (median), `sd=` after `gap=` in the log line. `sim.rs`: `SimCfg::skin`, `WindowShape::OneSide`.
`tests.rs`: L23-L28, `a_skin_override_keeps_the_scene_calibrated`, edited L8 and the log test. `status.rs`, `fx/*`: not
touched.

**Fail first (stub `tenth_spread` returning 0, production still on `e_sub`).** The tests were added and run before the
meter changed:

- M15: `office skin_light TopAndSides -3 at -1: front -1.027` (first frame; over the 900 frames the worst is `front`
  −1.589 at office dark skin *top and sides* −3 at +1, 440 frames under −0.02: 110 light, 165 medium, 165 dark).
- M17: `office skin_light TopAndSides -3 at -1: spread 0.000 (gap 1.78)` (needs ≥ 0.41).
- M18: `office Big +3 at -2: sources 0.000, want 0.2` (error +0.37).
- L23: `office skin_light TopAndSides -2 from -8/0: front -0.777 at 10 s` (needs |front| ≤ 0.01).
- L25: `office skin_light Big +3 from -8: face -1.69 stops from its rest at 10.0 s (needs >= -1)`.
- L8 (edited): `Holding 30 fps · gain 27 of 75`, no suffix (office *big* +3 rests with `backlight` 0).
- The log test (with `sd=`) failed too, before the spread was real.

**After.** `cargo test -p better-cam --lib auto_controls`: 190 passed (177 before; +6 M, +6 L, +1 sim).
`cargo test -p better-cam --lib fx::awb`: 23 passed, 1 ignored. `cargo clippy -p better-cam --tests`: clean.

**Measured against 13.1-13.6.** Every number of the spec came out as written, at the spec's thresholds.

| Test | Spec | Rust |
|---|---|---|
| M15 | `front` 0, `backlight` within 1e-9 of as built | 900 frames: max \|front\| 0, max \|Δbacklight\| 0 |
| M16 | `front`, `error` equal (< 1e-12), S ≤ 0.25 (0.191) | 90 frames: Δfront 0, Δerror 0, S ≤ 0.191 |
| M17 | (a) ≤ 0.25 (b) ≤ 0.30 (c) ≥ 0.41 (d) walls ≤ 0.05 (≤ 0.01), lamp-lit within 0.15 (0.10) | (a) 0.191 (b) 0.279 at 163.26 s (c) 0.412 (d) walls 0.0070, lamp-lit 0.0973 |
| M18 | `sources` ≥ 0.2 / 0.25 / 0.18 / 0.30; X* ≥ −0.9 / −0.6 / −0.4 / −0.9 | min `sources` 0.260 / 0.320 / 0.232 / 0.359 (as built 0.000 / 0.117 / 0.053 / 0.112); min X* −0.75 / −0.41 / −0.21 / −0.69 |
| M19 | `error`, `backlight` equal (< 1e-9) | 360 frames, equal |
| M20 | outside 110.0-110.8 s ≤ 0.005 (0.002); inside ≤ 0.4 (0.321) | outside 0.0017 (163.26 s); inside 0.321; 8 of 93 frames differ from as built |
| M2 (edited) | `backlight` ≤ 0.12 (0.103) | 0.1035 at 110.76 s |
| M13 (edited) | b: ≤ 0.02 (0.011), `backlight` ≤ 0.12 | 7 desk frames differ from the helper, largest 0.0108 |
| L23 | \|front\| ≤ 0.01 (0.00), face ≥ +0.4 (min +0.60) | max \|front\| 0.000, min +0.60 (36 runs) |
| L24 | light +0.60..+1.02, medium +1.14..+1.47, dark +1.98..+2.25; front at 30 s | light +0.60..+1.02 (front −1.82..−0.90), medium +1.14..+1.47 (−1.59..−0.51), dark +1.98..+2.25 (−1.01..0.00); 0 writes in 20-30 s |
| L25 | ≥ −1.0 / −0.3 / −0.7 / −0.5 / −0.3 / 0.0 | −0.81 / 0.00 / −0.51 / −0.33 / +0.09 / +0.12..+0.21 |
| L26 | ≤ 2 batches (0), ≤ 1 reversal | 0 batches in all 45 runs (at `dim(-3.5)`, see 2 below) |
| L27 | ≤ 3 reversals in 10 s | 3 (W4 needs 4), never damped |
| L28 | face ≥ +0.15 (min +0.20), `front` ≥ −0.8 (−0.71) | min +0.20, min `front` −0.709 |

**Deviations from the spec (numbers).**

1. **Log test: `sd=0.00` does not hold.** The sim's grey wall has a face patch six cells wide, so its bright tenth is
   one row of six cells and `S` reads 0.17-0.18 in every normal room of the suite (`dim(-1.0)` at the gain limit 0.18,
   held at the frame-rate limit 0.18, `dim(-3.5)` at rest 0.17, `dim(-6.5)` 0.18). `sd=0.00` only appears when `front_light`
   returns early (`sources` ≥ 0.25) or nothing is computed. The test asserts that `sd=` is in the line and under 0.25
   (the lamp-lit bound of M16) instead of `sd=0.00`; no other part of the log test changed.
2. **L26 runs at `dim(-3.5)`** (L3's room, as `backlit_sim`). At `dim(-6.0)` one run (dark skin, *one side* −2, Fast) made
   2 batches after 20 s (the threshold is ≤ 2, it passed); at `dim(-3.5)` every run makes 0, as the spec measured. The
   spec does not name the light.
3. **M12 keeps the as-built helper.** `without_knee` stays the reading with `e_sub` for map and term (M12's claim is
   about 12 alone); M13 got its own `term_without_knee` (13.7: `e_sub` in the term only, everything else as
   `measure_exposure`). Both call `front_light` with `fade` as stated (`false` for `without_knee`, `true` for the
   other).
4. **Choices the spec leaves open.** M17 (d) reads the walls at offset 0 and the lamp-lit frame at +0.8 (office, room
   −4), 300 copies each, one seed; M17 (b) and (c) also assert that at least 60 desk frames and 100 wall frames have a
   gap over 1 (so the test cannot pass on nothing); L23-L25 and L28 start the sim's camera at the seed (`start_e`), L26
   and L27 at the default −6 with resume −6 / 0. `tenth_spread` is computed whenever there is a tenth and a room,
   including when the gap leaves the gate shut; it is 0 when `sources` ≥ 0.25 (as `room_gap`).
5. **`WindowShape::OneSide`** is the expression of the designer's scratch copy: rows 0-5, columns 0-4 down to row 12,
   columns 13-15 down to row 11. 13.8's phrase "the wall is left only right of the head" reads the other way round; the
   zone weighs the left columns more, which is what makes the patch one-sided (`S` 0.28-0.29, `front` −0.709 at worst).
6. **Clippy:** `bright_tenth`'s return type tripped `type_complexity`; it uses the alias `TenthCell`.

**Left open.** The live check of 13.9 question 4 (`sd=` and `src=` on the owner's camera), and 13.9 questions 1-3 (map
knee, the one-sided bright wall as a known limit, lamp-lit darker skin) as the spec has them.

### 13.11 Audit fixes, loop (2026-11-19)

Files: `exposure.rs`, `meter.rs` (a `debug_assert` and a shared test fixture), `tests.rs`. Supersedes 11.4's "The 2 s hold"
and its W3 sentence.

**The hold (finding 1).** The 2 s hold of 11.4 held the front light *term*. A face that comes and goes every 2-3.5 s made
the loop reverse every cycle: lamp-lit office, room −3, `dim(-6.0)`, resume −6 / 0, Toggle {+1, −1} and {0, −3} every 2 /
2.5 / 3 / 3.5 s, Slow / Normal / Fast (24 runs). **Before: 24 of 24 fail**, 20 handed back to Camera auto at 18.2-42.7 s
(state Hunting), 4 would be damped (Fast and Normal {0, −3} at 3.5 s, Slow {±1, 0/−3} at 2 s). A first try that only
lengthened the held term (6 s after a come-back) did not help (12 hand-backs at 18.2-45.6 s, the other 12 runs made
79-144 batches in 90 s): with the term on, the reading follows the face (a face two stops darker reads 0.9 stop higher), so the loop follows
the face whatever the held term says. The fix holds the lit *reading*:

- A sample is lit when `front ≤ FRONTLIT_ON` (−0.5). The loop's `e` is the median of the last samples, never above the
  hold's reading: the lowest median of three consecutive lit readings of the last `len` seconds. With no lit sample in
  the window, `e` is today's. Attack is at once; release waits `len`.
- Each landed write batch shifts the older lit readings by its predicted stops (`LitHold::moved`), so the hold stays
  comparable across writes (a Slow loop writes all the time while the face moves).
- `len` is `FRONT_HOLD` 2 s. If a lit sample comes back within `FRONT_BACK_WITHIN` 6 s after the hold ran out, `len`
  becomes `FRONT_HOLD_LONG` 6 s (a face that comes and goes); a longer gap, a pause or a close put it back to 2 s.
  Known cost: after such a spell a lamp switched off brightens 6 s later, not 2. (13.13 A changes this rule: a face
  that stays lit and swings had no gap, so `len` never became 6 s.)
- `front_held` (status flag, log `fl=`) is now `e − e_plain`. The bands' `σ` is unchanged (on `e − front`).
- No rate-limited release: it cannot give L12's 3 stops in 2.8 s, and the reading, not the release, was the cause.

**After:** 24 of 24 pass, no hand-back, never damped, 4-13 batches in 90 s (60 s in the test). L9-L18, the desk replays
(L10, L11) and every older test pass unchanged: no threshold moved.

**W3 and gain learning (finding 2).** They judge on `e_plain = median(e − front)`, section 10's reading (the ±4 clamp
never binds: `measure_exposure` now has a `debug_assert` for it). W3 strikes in the 24 scenes: before 2 (Slow {±1}
every 3.5 s, then handed back at 42.2 s), 0 in the other 23 (most ended in a hand-back by 31 s); after 0 in all 24, and
0 in all 24 with the raw reading under the new hold. No measurable change in these scenes; the change is for the case
the audit named (the term follows the face, a write does not move it).

**Stale suffix (finding 4).** Pause and close now clear the `backlit` / `frontlit` flags. Test:
`a_resume_does_not_show_the_suffix_of_before_the_pause` (fails without the clear).

**Test time (finding 3).** `auto_controls` before: 190 tests, 59.2 s (51.8 s measured by the audit). After: 202 tests,
23-28 s over three runs. `wall_frames()` is built once (`OnceLock`, M15 and M17 c). L26 runs the light skin at all three
speeds and the other tones at Normal, 60 s each, one test per group; L27 per skin tone at 60 s; L20 at 60 s; L23 and L28
at 20 s (their checks start at 10 s); L24 per skin tone. New: L29 `a_face_that_comes_and_goes_quickly_does_not_hunt_at_*`
(one per speed, 24 runs of 60 s, about 4 s each).

### 13.12 Audit fixes, meter (2026-11-19)

Files: `meter.rs`, `status.rs`, `tests.rs`. The audit's remaining findings on the device exposure meter. Changes the
map of 10.4 step 3 (and so `G`, 10's gate, 11's room level and fade, 13's spread) at targets other than 0.45; every
reading at 0.45 is unchanged.

**The finding.** 10.4's "white with the zone on target" used fixed coded levels, `SOURCE_AT_SUBJECT` (0.90, 0.97).
That is a fixed number of stops over the target grey only at 0.45 (2.21 and 2.45). At target 0.8 it is 0.38-0.63 stop
over the zone, at 0.6 it is 1.31-1.55, at 0.7 0.81-1.06; at 0.2 it is 4.57-4.82 and at 0.3 3.43-3.67. So at high targets
an ordinary wall counted as a light source and opened 10's term, and at low targets a window had to be 4.6 stops over
the face to count. Audit evidence (sim rest at `dim(-6.5)`, an even room without a window, the backlight term as built
against forced to 0), face stops from its rest at 0.45:

| Room | Target | As built | Term forced to 0 | After 13.12 |
|---|---|---|---|---|
| office dark skin (kit scene) | 0.6 | +1.74 | +0.89 | +0.89 |
| office, medium skin | 0.7 | +2.04 | +1.34 | +1.34 |
| warm room, medium skin | 0.7 | +1.46 | +1.31 | +1.31 |

**The fix.** The levels are stops over the target, as the knee of 12.4 is:

```
lo = srgb_encode_open(2^S_LO · srgb_decode(T)),   hi = srgb_encode_open(2^S_HI · srgb_decode(T))
src_i = max(ss(lo, hi, srgb_encode_open(k · lin_i)), ss(0.93, 0.99, y_i))        (k = 2^e_face)
```

| Constant (`meter.rs`) | Value | Why |
|---|---|---|
| `SOURCE_AT_SUBJECT_STOPS` (replaces `SOURCE_AT_SUBJECT`) | (2.206 121 628 681 489 5, 2.451 044 680 208 762) | `log2(decode(0.90) / decode(0.45))` and `log2(decode(0.97) / decode(0.45))`: at T = 0.45 `source_levels` gives (0.90, 0.97) again (M22 asserts it within 1e-9, and the cell reading against the old formula over an 81 × 17 grid). |
| `SOURCE_WHITE_NOW` | (0.93, 0.99), unchanged | See "Fixed levels checked" below. |

`srgb_encode_open` is `srgb_encode` without the upper clamp: over 1 for a linear value over white. **Deviation from the
brief (cap).** The brief asked for each level capped below 1.0. A cap does not fix the evidence: from target 0.46 (the upper level)
and 0.50 (the lower) the formula passes 1, a capped level (the cap (0.93, 0.99) of `SOURCE_WHITE_NOW` was built and run) then means "the cell
would be white with the zone on target", and a wall that the zone's gain would clip counts however far under the formula's
level it is. With that cap `G` stayed 0.272 in office dark skin at 0.6 (the wall is 1.8 stops over the zone on target;
the formula wants 2.2) and 0.277 at 0.8. So the cell is read unclamped too (`k · lin`, over 1 when the zone's gain
pushes it over white), and no cap is needed: the levels are over 1 from target 0.46-0.50 up (1.18-1.27 at 0.6, 1.56-1.68 at 0.8),
a cell passes them only when it is that many stops over the zone on target, and a clipped cell (its `lin` a lower bound)
passes only when the zone is far enough under its target. At 0.45 a cell that was clamped to 1.0 was over the old `hi`
(0.97) anyway, so the readings are unchanged there (M22 asserts the levels and the cell reading within 1e-9; every
test of 10-13 passes as it was).

| Target | 0.2 | 0.3 | 0.45 | 0.6 | 0.7 | 0.8 |
|---|---|---|---|---|---|---|
| Fixed levels: stops over the target | 4.57-4.82 | 3.43-3.67 | 2.21-2.45 | 1.31-1.55 | 0.81-1.06 | 0.38-0.63 |
| `source_levels(T)`, coded | 0.427 / 0.463 | 0.616 / 0.666 | 0.900 / 0.970 | 1.184 / 1.274 | 1.373 / 1.477 | 1.562 / 1.680 |

**Fixed levels checked.**

- `SOURCE_WHITE_NOW` (0.93, 0.99): not changed. A cell that is white in the picture is white at every target; at 0.8 the
  picture of an even room is blown out (a white-cell share of 0.27-0.35 in 7 of the 55 pictures at 0.6 and 37 at 0.8)
  and those cells are exactly what a window looks like. Nothing in the 16 × 16 image can
  tell them apart (a clipped cell's `lin` is only a lower bound), and the share is what stops `G` falling when a window
  turns white (10.4). After the fix `sources` equals the white-cell share to within one stray cell in every picture
  (0.006 in every room at every target, 0.012 in the checker); nothing is left to fix except the blown-out pictures.
  This is the known limit of high targets.
- `BACKLIGHT_CLIP_LEVEL` 0.95 and the clip budget: a physical white (just under the guard's 0.97), not "over the
  target". Evidence: none. In every even room at every target the term is 0 (0.004 at most, in a blown-out picture at 0.8).
- `SUBJECT_TOP_MAX` 0.90 and `SUBJECT_KNEE_MAX` 0.90: the caps of the ceiling and of the knee from target 0.68 / 0.52
  up. The 90 lamp-lit frames of M16 at 0.2, 0.3, 0.45, 0.6, 0.7 and 0.8 read the same before and after 13.12 (the cap
  acts in 33 / 53 / 68 / 68 / 66 / 73 of them), so the ceiling is not moved by this change; no change.
  The knee's cap is behind one thing that stays: a dark-skinned face before a *clipped* white wall at 0.6-0.7 is a
  source and the term lifts it (office dark skin, top or sides +1.4, 0.6: backlight +0.94, +0.89, same as before 13.12):
  a clipped wall at a high target is a window to the meter. Known limit.

**Results at each target** (M21: every even room of the suite, 11 pictures, at the target's rest and 5 exposures around
it, 55 per target; the pictures that are not blown out are "clear", white-cell share ≤ 0.02):

| Target | 0.2 | 0.3 | 0.45 | 0.6 | 0.8 |
|---|---|---|---|---|---|
| Clear pictures | 55 | 55 | 55 | 48 | 18 |
| Largest `sources` (clear) | 0.006 | 0.006 | 0.012 | 0.012 | 0.014 |
| Largest `backlight` | 0 | 0 | 0 | 0 | 0.004 (blown-out) |
| Largest `front` | 0 | 0 | −0.016 | −0.011 | −0.004 |
| Largest `room_gap` | 0.83 | 0.83 | 1.08 | 1.07 | 1.03 |
| Blown-out pictures (white share up to) | 0 | 0 | 0 | 7 (0.31) | 37 (0.35) |

Before: at 0.6 office dark skin read `sources` 0.275 and `backlight` +0.60 at rest; at 0.8 `sources` were 0.27-0.35 in
most even rooms with `backlight` up to +0.20 (closeup, grey room, office) and the checker's `front` −1.06. Windows (M24:
office *big* +4, *top and sides* +3, warm room *big* +3.5, face 1-3 stops under, every target): sources ≥ 0.2 and
`offset + error` ≥ −1.0 at all of 0.2-0.8; before at 0.2 and 0.3 `sources` 0.00 and X* −3.27 .. −1.5, at 0.45-0.8 as before.
Bright walls (M25, a 108-frame subset of M15): front 0 at 0.3 and 0.6 (≥ −0.005); at 0.8 3 of 108 frames are still capped
(office light skin, +1 stop, wall 3 stops under; front down to −1.61; before 13.12 −1.82 over the full 540 frames, six
frames, both from clipping). Lamp-lit faces (M16 frames at all targets): identical readings before and after.

Wall frames (840 per target: both rooms, three skins, `Top` / `TopAndSides` / `Big` / `Wall`, seven levels −3..+1.4,
five exposures, brightened by the target's rest) that change by more than 0.01 stop of `error`, before → after: 0 at
0.2, 0.3 and 0.45; at 0.6 71 frames lower by up to 0.93 stop (the old levels over-lifted dark skin); at
0.7 130 lower (up to 0.63), 15 higher (up to 1.10); at 0.8 82 lower, 37 higher (up to 1.78). The higher ones are the
neutral-wall rows (`Wall`, level −3 or −2: a region 2-3 stops under the light-skinned face): at 0.7 and 0.8 the old
levels counted the wall as a source, the room level lost it and 11's cap came on (front −1.2 .. −2.0) where at 0.45 it
reads −0.09 .. −0.9 for the same frames; the new levels give the same front at every target (office light skin at 0.8
−2.05 in both, warm room light skin −0.20 from −1.97). Direction is the one the target-relative levels should have.

**Other findings.**

- **NaN coverage.** `has_picture` (`>= MIN_COVERAGE`) is the one test of a cell's coverage (`covered`,
  `measure_exposure`, `mean_coded_luma`): a NaN coverage has none. `measure_exposure` used `c[3] < MIN_COVERAGE`, which a
  NaN passes, and `confidence` and `clip` turned NaN. M23: three NaN cells read exactly as three cells with coverage 0
  (finite). Fail first: `confidence: NaN, clip: NaN`.
- **M16 could pass on nothing.** `as_built` shares `front_light` with the meter, so a term that was always 0 passed. It
  now also asserts that at least 60 of the 90 lamp-lit frames have `front < −0.5` (68 do).
- **Names.** `status.rs`: `backlight_note` → `hold_note` (it returns either note); the docs of `ExposureFacts.backlit` and
  `.frontlit` describe the flags (median `backlight` ≥ 0.5 stop, held reading ≥ 0.5 stop under the plain one, with
  hysteresis), not the text. Three blank lines in the `meter.rs` tests gone. Docs: pointers in 2.4, 4.2, 6.2; 12.4 steps 3-4
  marked superseded by 13.4; 10.4's table row; the doc comment of `measure_exposure`.

**Tests.** Added: M21 `even_rooms_have_no_light_sources_at_any_target`, M22 `the_source_levels_are_stops_over_the_target`,
M23 `a_nan_coverage_is_a_cell_without_a_picture`, M24 `a_window_counts_as_a_light_source_at_every_target`, M25
`bright_walls_do_not_trigger_the_dark_room_cap_at_other_targets`, and in `tests.rs` L30
`an_even_room_at_a_high_target_rests_without_the_backlight_term` (the audit's three rows; the face rests within 0.06
stop of the term-off run: measured 0.00, the gain step is 0.03; `meter::hook::with_backlight_off`, a per-thread test hook
that shuts the gate, is how the term-off run is made). M21 is M1 and M6 over the targets of the user's range. **Fail first** (the new tests on the code before the change: levels fixed at (0.90, 0.97), `c[3] < MIN_COVERAGE`):

- M21: `office_dark_skin at T 0.6 -1: … backlight: 0.6037, sources: 0.27497 …: sources 0.275, white now 0.006`.
- M22: `T 0.2: lo 0.9` (and at 0.2 the old levels are 4.57 stops over).
- M23: `confidence: NaN, clip: NaN` against the reading with the cells at coverage 0.
- M24: `office Big +4 at T 0.2, face -3: … sources: 0.0 … error -0.2676` (X* −3.27).
- L30: `office_dark_skin None at 0.6: face +1.74, without the term +0.89`; the other two rows read +2.04 / +1.34 and +1.46 / +1.31.
- M16 and M25 pass before and after (M16 asserts what was always true: 68 of 90; M25 bounds the known limit).

**After.** `cargo test -p better-cam --lib auto_controls`: 208 passed (202 before; +5 M, +1 L; 25 s, +2-4 s).
`cargo test -p better-cam --lib fx::awb`: 23 passed, 1 ignored. `cargo clippy -p better-cam --tests`: clean. Every earlier test
passes unchanged: no threshold of 10-13 moved.

**Known limits.** (1) At a target of 0.6-0.8 an even room's resting picture is blown out, and its clipped upper cells count
as light sources like a window (0.27-0.35 at 0.8); 10's term then adds 0.00-0.004 stop, 11's cap fades out there. (2) A
dark-skinned face before a clipped wall at 0.6-0.7 reads as backlit (+0.9 stop at 0.6). (3) At 0.8 a face and a wall that clip
alike can still bring on the dark-room cap in a few frames (M25). Not fixed: the meter has no way to tell a clipped wall
from a window.

### 13.13 Re-audit fixes (2026-11-19)

Files: `exposure.rs` (`LitHold`, the landing of a batch, a `tests` module), `meter.rs` (`has_picture`, the test hooks),
`tests.rs`. The re-audit's findings A-I on 13.11 and 13.12. Supersedes 11.4's "The 2 s hold", its `FRONT_HOLD` row and the
`len` rule of 13.11.

**A. A face that stays lit and swings (must).** 13.11 made `len` 6 s only after a *gap* between lit samples. A face that
changes by 0.5-1 stop and stays lit leaves no gap, so the 2 s hold released every cycle: the loop darkened for the bright
moment, brightened 2 s later, and so on. Lamp-lit office, room -3, `dim(-6.0)`, resume -6 / 0: Toggle {+1, 0} and {+0.5,
-0.5} every 2 / 2.5 / 3 / 3.5 s handed back at 20.0-35.6 s at Normal and Fast, were damped at Slow; a Wave of +-1 stop with
a 5 s period handed back at 24.2 s at Normal and Fast; periods 8 and 12 were damped. The mechanism now:

- While lit samples keep arriving (the last one at most `FRONT_HOLD` 2 s ago) the held reading is the lowest median of
  three consecutive lit readings of the last `FRONT_HOLD_LONG` 6 s, each shifted by the writes since. A face that swings
  through that window is followed at its brightest moment, not at each swing.
- Once lit samples stop (the lamp is off, the face left the light: the term is 0) the hold lasts `len` after the last one,
  as in 13.11: `FRONT_HOLD` 2 s, so L12's lamp-off timing is kept (L12 passes with its limit of 4 s after each switch).
- `len` becomes `FRONT_HOLD_LONG` when a lit sample comes back after more than `FRONT_GAP` 1 s without one: before the hold
  ran out, or after, within `FRONT_BACK_WITHIN` 6 s (13.11 had only the second). A hold that ran out is released for good:
  the readings it held are dropped, not brought back by the next lit sample (a first version without this made the loop
  brighten 1.9 stops at the first release of a 12 s wave and darken 1.4 stops 1.2 s later: Fast, damped).
  `FRONT_GAP` is 1 s because the dark half of a 8 s wave is a gap of 2.0 s, exactly `FRONT_HOLD`: with the 13.11 rule it
  released or not by a rounding error.

| Constant (`exposure.rs`) | Value | Why |
|---|---|---|
| `FRONT_HOLD` | 2.0 s | Unchanged: the release after the last lit sample (a lamp off). |
| `FRONT_HOLD_LONG` | 6.0 s | Unchanged value; now also the window while lit. Over the lit half of the slowest wave tried (12 s). |
| `FRONT_GAP` | 1.0 s | New: a gap that makes a come-back. Frame-to-frame the term moves 0.2-0.5 stop and a sample or two near its gate drop out; the faces that come and go are away 2 s or more. |
| `FRONT_BACK_WITHIN` | 6.0 s | Unchanged. |
| `FRONT_STAYED` | 12 s (2 x `FRONT_HOLD_LONG`) | New (E, below). |

**Results (A).** The 60 cases of L29 (Toggle {+1, -1}, {0, -3}, {+1, 0}, {+0.5, -0.5} x 2 / 2.5 / 3 / 3.5 s, Wave +-1 stop x
3 / 5 / 8 / 12 s; Slow / Normal / Fast; 60 s in the test, 90 s checked by hand). Before = the code of 13.11, 60 s runs:

| Cases | Slow before | Normal before | Fast before | After (all speeds) | Front term off (baseline) |
|---|---|---|---|---|---|
| Toggle {+1, 0} x 4 | damped (4 of 4) | hand-back at 20.0 / 24.5 / 30.2 / 35.2 s, Hunting | 20.1 / 24.5 / 30.2 / 35.6 s, Hunting | none, never damped, not Hunting | none, never damped |
| Toggle {+0.5, -0.5} x 4 | damped at 2 and 3.5 s | hand-back at 20.0 / 24.5 / 30.2 / 35.3 s, Hunting | 20.1 / 24.5 / 30.2 / 35.6 s, Hunting | none, never damped, not Hunting | damped at 2.5 and 3 s (Slow), 2 / 2.5 / 3 s (Normal, Fast); no hand-back |
| Wave period 3 s | quiet | quiet | quiet | none, never damped | damped at all three speeds |
| Wave period 5 s | quiet | hand-back at 24.2 s | hand-back at 24.2 s | none, never damped | damped at Slow and Fast |
| Wave period 8 s | damped | damped | damped | none, never damped | none |
| Wave period 12 s | quiet | quiet | damped | none, never damped | none |
| Toggle {+1, -1}, {0, -3} x 4 (L29 of 13.11) | quiet | quiet | quiet | none, never damped | none |

("Quiet": not a failure of the test's checks; the face peak was over 1.3 in many of these, below.) After: 0 hand-backs, 0
Hunting, 0 damped in the 60 runs at 90 s (the baseline is damped in 15 of them, the audit's 15: the damping the loop
without the term does too is accepted, and the test only asks that the loop with it is not damped where the baseline is
not). Hand-backs before: 18 of 60 (Normal 9, Fast 9); damped where the baseline was not: 10 (Slow 7, Normal 1, Fast 2).

**Residual.** A 12 s wave is a lit half of 6 s, as long as the window: the loop follows it with a 6 s lag, in small steps
the same way (Normal 54 batches in 60 s, 35 of them after 20 s; the baseline 14 moves of 0.15 stop or more) and turns round once per half period: at most 3
reversals in 10 s, one under W4's limit. Not damped at any speed, but with that margin.

**D. A frozen loop would have passed L29.** L29 now also asserts the face level: from 20 s on, the highest
`face_over_need` is within [0.4, 1.35] (L9's [0.4, 1.3] and one step of following; a loop that did not move would sit at
-6 / 0, far under, and one that released each swing was over 1.4-1.67: Normal {+1, 0} every 3.5 s +1.66, {+0.5, -0.5} +1.67,
wave 5 s +1.52; Fast {+1, 0} 1.66). Measured now: 0.65-1.31 over the 60 cases (0.78-1.00 in the toggles). The check is
also in the new cases, which are L29's.

**B. `moves` grew without limit.** It was pruned only in `push`; 38 / 206 / 617 entries after 60 / 300 / 900 s. `moved` now
drops entries older than `FRONT_HOLD_LONG` + 1 s before the batch it records. Test
`the_write_log_is_bounded_without_lit_samples`: 10 000 `moved` calls, no `push`, at most 72 entries.

**C. The shift of a landed batch.** It used the decision's predicted stops and the last write's `ok`. Now `Moving` (the
model's light and `S` when the batch was decided, the earliest write start) is kept; when the last write has returned the
shift is `x(after the read-backs) - x(before)`: a refused exposure write (read back as the old value) shifts nothing, a
failed write of a two-write batch leaves the first one's stops, a different read-back is what the camera has. Lit readings
captured from the first write's start to `valid_from` show the batch partly and are dropped (the older ones are shifted).
Tests (unit, `exposure.rs`): `a_refused_exposure_write_shifts_nothing`, `a_taken_exposure_write_shifts_by_the_stops_it_made`,
`the_first_write_of_a_batch_counts_when_the_last_one_failed`, `a_landed_batch_shifts_older_readings_and_drops_those_in_between`.
In `tests.rs` L31 `a_lamp_lit_face_on_a_camera_that_refuses_a_longer_exposure_rests_at_the_limit` (lamp-lit office on the
`ignore` fault, `dim(-1.5)`: asked once, rests at -5 / 100, one stop under the taking camera). **It cannot tell the old
shift from none**: with the old +1 forced in, its end state is the same (the shift lasts 6 s). The unit tests pin it.

**E. The long hold never ended.** Not moot: a come-back sets `len` to 6 s, and with lit samples that never stop it stayed.
`len` returns to `FRONT_HOLD` after `FRONT_STAYED` 12 s of lit samples with no gap over `FRONT_GAP` (test
`a_face_that_comes_back_lengthens_the_hold_and_a_long_stay_ends_it`). 6 s was tried: the lit half of a 12 s wave is 6 s, it
ended the hold each cycle and the 12 s wave was damped again at Normal and Fast.

**F. The hold acted on one or two lit samples.** It needs three (`held` is the lowest median of three consecutive lit
readings; none with fewer). Test `the_hold_needs_three_lit_samples`; L12, L13, L29 and the lamp tests pass.

**G. `with_backlight_off`.** It reset to `false` and not on a panic. A drop guard (`Restore`) now puts back the value it
found; `with_front_off` (the front term, with its gap and spread, forced to 0) is built the same way and makes L29's
baseline. Test M27 `a_hook_call_puts_back_what_it_found` (nested calls, a panic through `catch_unwind`).

**H. Non-finite colour.** `has_picture` also needs a finite luma: a NaN colour gave a NaN confidence, an infinite luma
`gap=+inf`. Test M26 `a_non_finite_colour_is_a_cell_without_a_picture` (NaN and infinite colours in the middle cells give the
reading of the picture without them; fail first: `confidence: NaN, clip: 0.00684 ... sources: 0.0, room_gap: 0.0`
against `confidence: 1.0, ... room_gap: 0.0165`).

**I. Docs.** 11.4's "The 2 s hold", its `FRONT_HOLD` row and 11.11's ring line are marked superseded; 13.11's `len` rule has
a pointer here.

**After.** `cargo test -p better-cam --lib auto_controls`: 220 passed (208 before; +9 exposure unit tests, M26, M27, L31;
21-26 s over three runs, about as before (the machine varies): the new L29 cases replace the 8 old runs per speed with 20, and the baseline runs only
where a case is damped). `cargo test -p better-cam --lib fx::awb`: 23 passed, 1 ignored. `cargo clippy -p better-cam
--tests`: clean. No threshold of an earlier test moved; L29's new face bound is the only addition.

**Known limits.** (1) After a face that came and went, a lamp switched off brightens `FRONT_HOLD_LONG` 6 s later, not 2 s,
until 12 s of lit samples without a gap or a pause (13.11's cost, now ended by E). (2) A face that stays lit and gets
darker for good is followed after 6 s, not 2 s. (3) A 12 s wave is followed with a lag and a small margin to damping
(above). (4) The sim cannot show C's shift in a whole-loop run.
