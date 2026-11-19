# Auto white balance under warm light — analysis and plan

Status: design, nothing built. Date: 2026-11-19. Owner report: "if I have an orange light, auto colouring does not
compensate well." Covers both places Better Cam balances colour:

1. **Auto Adjust** (the filter): `src-tauri/src/fx/auto_adjust.rs`, `gpu/shaders/auto_adjust.hlsl`,
   `gpu/shaders/stats_downsample.hlsl`, tests in `fx/auto_adjust/tests.rs`.
2. **Better Cam auto white balance** (the device loop): `src-tauri/src/auto_controls/{meter.rs, white_balance.rs}`,
   docs/AUTO-CONTROLS.md 2.4 and 2.6.

Every number below comes from a CPU model of the stats path run on synthetic scenes (2.1). Nothing was measured on the
W4GS: the room has no warm light (AUTO-CONTROLS.md 9.1), so the live part is a checklist for the owner (section 7).
Facts about the W4GS that only a lit room can show are marked **(unverified)**.

---

## 1. Summary

**Why a strong orange cast ends under-corrected (five lines):**

1. Both places run gray world only on cells that are "not strongly saturated", judged **before** the correction. Under
   orange light every neutral surface is itself saturated (linear saturation 0.81 at 2700 K with the camera at
   4600 K), so the gate throws out the grey wall and the face and keeps the blue shirt, which tungsten turns grey. At
   2700 K the shirt carries 64 % of the weight. The estimate sees 63 % of the cast, and only 30 % at 2500 K.
2. Auto Adjust limits each channel gain to `0.5 ..= 2` (`auto_adjust.rs:132-133`). Once the estimate is right, this
   limit binds: 2700 K light on a camera at 4600 K needs blue × 3.3, and the limit leaves +51 mired (12.8°) of
   orange. A grey room, which has no coloured object to fool the gate, ends at exactly that today.
3. When the gate leaves too little, confidence falls and the loops hold. The device loop then never leaves its seed
   (4600 K): a warm room under 2700 K stays +158 mired orange. A test codifies this as correct
   (`a_cast_with_nothing_neutral_to_trust_is_held`, `auto_controls/tests.rs:1015`).
4. The device cannot go below 2800 K (W4GS range). Without Auto Adjust, a 2200 K bulb stays +135 mired orange, and
   nothing in the UI says that Auto Adjust would finish it.
5. The same gate also has a daylight bias: a centre face counts fully while the grey wall barely does, so daylight
   scenes with a face end 14-49 mired too blue (3-12°). Pictures of one colour are over-corrected by up to 84 mired
   (a face filling the frame turns blue).

**The fix (five lines):**

1. One shared estimator for both places (`fx/awb.rs`). It starts with plain gray world and then gates the cells **in
   the corrected space**, three times (a robust, mode-seeking gray world). The mean is shades-of-gray with p = 4, and
   cells with a clipped channel are left out.
2. A **blackbody-locus prior** splits the estimate into temperature and tint. Temperature is followed freely. Tint is
   kept within ±0.06. An estimate on the magenta side of the locus looks like skin or wood, not like a light, so it
   lowers the confidence (the skin guard).
3. **Auto Adjust**: the correction is the confidence times the full correction, and the gain range becomes
   `0.25 ..= 4`.
4. **Device loop**: same estimator. It no longer holds under a strong cast, and its "nothing to measure" text says why.
   At 2800 K the rest goes to Auto Adjust automatically, and the card says so or offers "Add Auto Adjust".
5. **Residual warmth** is the warmth target's job. Recommended new default: warmth **+0.15** in both places (≈ 5200 K
   look). This is the owner's decision (section 8).

**Effect in the model.** Office scene (face, grey wall, blue shirt, desk), camera at 4600 K, with Auto Adjust:

| Light | 2200 K | 2500 K | 2700 K | 3000 K | 5500 K |
|---|---|---|---|---|---|
| Today | +185 | +134 | +58 | +20 | −27 |
| Proposed | +86 | +12 | −4 | −4 | −3 |

Residuals in mired (+ = too orange). With the device loop plus Auto Adjust, the proposal gives −4 mired (≤ 1.2°) from
2200 K to 6500 K. Pictures of one colour are no longer pushed past "no correction": 47 such cases today, 1 proposed.

---

## 2. Findings

### 2.1 Method

Throwaway Python model, not committed (scripts are in the analysis session's temp folder `awb-analysis\`; phase W1
ports the scene kit to Rust):

- **Light and camera:** diagonal (von Kries) model in linear sRGB. A surface of colour `ρ` lit by a blackbody of `T`,
  on a camera whose white balance is set to `K`, records `ρ · bb(T) / bb(K)`. `bb` is exactly
  `auto_adjust.rs::blackbody_rgb` (Kim's spline → XYZ → linear sRGB). Exposure puts the mean coded luma at 0.45.
- **Stats path:** each of the 16 × 16 cells has 16 sub-samples with ±30 % texture. Each sub-sample is clipped at 1,
  8-bit sRGB coded and decoded, then averaged: what `stats_downsample.hlsl` delivers to both meters. Two cells hold a
  clipped highlight (a lamp or a specular spot).
- **Scenes (12):** `office` (grey wall, centre face with dark hair, blue shirt, wood desk, plant, white paper, red
  mug, a shelf of ColorChecker colours), `office_dark_skin`, `grey_room`, `checker` (the 24 ColorChecker patches),
  `closeup` (a face over 40 % of the frame, wall partly beige), `warm_room` (beige walls, wood, sofa), `face_fill`,
  `face_fill_dark`, and one-colour walls `red_wall`, `blue_wall`, `green_screen` and `orange_wall`, each with a small
  face.
- **Metric:** a virtual neutral surface is put through the light, the camera and the correction. The error is its
  angle to the aim in linear RGB, in degrees (about 1° is just visible, 3° is clearly visible), and its residual
  `ln(B/R)` in mired-equivalents (÷ 0.0105, the blackbody slope near 4000 K). `+` means too orange.
- **Steady state:** Auto Adjust is feed-forward, so it settles on its wanted gains (`white_balance_target`). The
  device loop is iterated as `white_balance.rs::decide` does (from the 4600 K seed, `ΔM` ≤ 20 mired, holds at
  `conf < 0.3`), without timing.

Physical blackbody facts used below (linear sRGB, relative to 5500 K):

| T (K) | 2000 | 2200 | 2500 | 2700 | 2800 | 3000 | 3500 | 4000 | 4600 | 5500 | 6500 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| `ln(B/R)` | −4.55 | −3.28 | −2.40 | −2.02 | −1.86 | −1.59 | −1.07 | −0.69 | −0.36 | 0 | +0.27 |
| `d ln(B/R) / d mired` | 0.045 | 0.020 | 0.014 | 0.012 | 0.012 | 0.011 | 0.011 | 0.0105 | 0.010 | 0.0099 | 0.0097 |

### 2.2 Root cause 1: the "not strongly saturated" gate is judged before the correction

`auto_adjust.rs:417-425` and `meter.rs:176-184` weight each cell by `1 − smoothstep(0.55, 0.9, s)` with
`s = (max − min) / max` of its **uncorrected** linear colour. Linear saturation is harsh. Under a warm light it is high
for exactly the surfaces that tell the truth:

| Linear saturation | 2000 K | 2200 K | 2700 K | 3000 K | 4000 K | 5500 K |
|---|---|---|---|---|---|---|
| neutral, camera at 4600 K | 0.98 | 0.95 | 0.81 | 0.71 | 0.29 | 0.30 |
| light skin, camera at 4600 K | 0.99 | 0.98 | 0.92 | 0.88 | 0.70 | 0.41 |
| beige wall, camera at 4600 K | 0.99 | 0.97 | 0.89 | 0.83 | 0.58 | 0.15 |
| blue shirt, camera at 4600 K | 0.91 | 0.69 | **0.09** | 0.41 | 0.76 | 0.88 |
| neutral, camera at 6500 K | 0.99 | 0.97 | 0.90 | 0.84 | 0.62 | 0.24 |

So under tungsten the blue shirt becomes the "grey card". Office scene, share of today's gray-world weight:

| Light / camera | conf | grey wall | skin | blue shirt | shelf | Cast the estimate sees |
|---|---|---|---|---|---|---|
| 5500 K / 4600 K | 1.00 | 49 % | 34 % | 0 % | 7 % | 22 % (the face pulls it) |
| 2700 K / 4600 K | 1.00 | 24 % (cell weight 0.13) | 0 % | **64 %** | 12 % | **63 %** |
| 2500 K / 4600 K | 1.00 | 3 % | 0 % | 87 % | 10 % | 30 % |
| 2200 K / 4600 K | 0.82 | 0 % | 0 % | 95 % | 5 % | 39 % |
| 2700 K / 6500 K | 1.00 | 0 % | 0 % | 92 % | 8 % | 25 % |

Auto Adjust measures its own input and never sees its own correction ("the loop does not look at its own output",
`auto_adjust.rs:12-14`). The bias is therefore permanent: no number of frames removes it. The device loop is closed,
so its picture moves toward neutral and the gate improves as `K` falls. But it starts from the biased reading, holds
when that reading has low confidence (2.4), and stops at its range end (2.5).

Why no test caught it: the simulated camera's scene is all neutral reflectances with one uniform tint
(`sim.rs:171` `tint`, `sim.rs:186-231`), and Auto Adjust's tests use flat one-colour pictures. No test has a coloured
object under a warm light.

### 2.3 Root cause 2: Auto Adjust's gain limit

`white_balance_target` limits each gain to `MIN_WB ..= MAX_WB` = `0.5 ..= 2` (`auto_adjust.rs:132-133, 442-447`), then
renormalises to keep luma. Residual with a **perfect** estimate:

| Camera at | Limit | 2200 K | 2500 K | 2700 K | 3000 K | 3500 K |
|---|---|---|---|---|---|---|
| 4600 K | 0.5-2 (today) | +154 (B × 9.5 needed) | +81 (× 4.5) | **+51** (× 3.3) | +18 (× 2.4) | 0 |
| 4600 K | 0.25-4 | +86 | +12 | 0 | 0 | 0 |
| 6500 K | 0.5-2 (today) | +210 | +128 | +98 (× 5.3) | +64 (× 3.8) | +24 |
| 6500 K | 0.25-4 | +135 | +60 | +29 | 0 | 0 |
| 2800 K (device floor) | 0.5-2 (today) | +41 (× 3.0) | 0 | 0 | 0 | 0 |
| 2800 K | 0.25-4 | 0 | 0 | 0 | 0 | 0 |

The two causes stack. Widening the limit to `0.25 ..= 4` **alone**, with today's gate, takes the grey room at 2700 K
from +51 to −3 mired. The office stays at +57 and the closeup goes only from +56 to +42, because their estimate is
the problem (2.2): it asks for less than × 2 anyway. Fixing the estimate alone hits this limit instead. Blue × 4 is
two stops of gain on the blue channel. Blue is 7 % of luma, so its noise shows little in the picture, and the limit
still bounds a single-colour scene.

### 2.4 Root cause 3: low confidence means "hold", in both places

- **Device loop:** `decide` returns without a move when `conf < 0.3` (`white_balance.rs:36`, `:587-591`; since F4 it
  shows `NoSignal`: "Nothing to measure (too dark). Keeping 4600 K."). Today's `conf_wb` with the camera at 4600 K:

  | Scene | 2000 K | 2200 K | 2500 K | 2700 K | 3000 K | 3500 K | ≥ 4000 K |
  |---|---|---|---|---|---|---|---|
  | office | 0.00 | 0.82 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
  | warm_room | 0.00 | 0.00 | 0.00 | **0.01** | 1.00 | 1.00 | 1.00 |
  | grey_room | 0.00 | 0.00 | **0.00** | 1.00 | 1.00 | 1.00 | 1.00 |
  | closeup | 0.00 | 0.10 | 0.41 | 0.83 | 1.00 | 1.00 | 1.00 |
  | face_fill_dark, red, green, orange walls | 0.00 | 0.00 | 0.00 | 0.00 | 0.00 | 0.29-0.95 | 1.00 |

  So a warm room under a 2700 K bulb leaves the device at 4600 K, **+158 mired**, with a status text that blames the
  dark. Any scene at 2000 K does the same (+400). `a_cast_with_nothing_neutral_to_trust_is_held` (`tests.rs:1015`)
  asserts this with `α_true = 0.012`, which is the physical value (table in 2.1), not an extreme one.
- **Auto Adjust:** its confidence only slows the move (`auto_adjust.rs:450`, `out = ln_cur + confidence · past_band`).
  A biased estimate with low confidence is still reached in the end, only later.

### 2.5 Root cause 4: the device's warm end, and no hand-off

`WhiteBalanceCaps::snap` clamps to the reported range (`white_balance.rs:121-124`). The W4GS reports 2800-6500 K
(AUTO-CONTROLS.md 2.2, `sim.rs:160-161`). Whether its 2800 K setting is a 2800 K blackbody is **(unverified)**. The
loop does not need that, because it is closed and learns `α`, but it must not write past the reported range. The
device loop alone ends at: +15 mired at 2700 K, +52 at 2500 K, +135 at 2200 K. That is `at_limit` with the text "…
Auto Adjust can finish it" (`status.rs:92-96`). The hand-off is in fact automatic: Auto Adjust measures its input,
which is the device's output. Today it cannot finish the job (2.2, 2.3), and the card neither tells the user whether
Auto Adjust is there nor offers to add it, as the exposure row does (`AutoControls.tsx:186-197`).

### 2.6 Contributing causes

- **Red clipping under orange light.** With the luma at the target, red is about 1.6 × the luma (camera 4600 K,
  2700 K light). A cell whose coded luma is about 0.80 or more already has its red clipped, and single pixels clip
  earlier (a lamp-lit white wall, the paper). Both gates test luma only: the white gate fades over 0.85-0.97 coded. A
  clipped cell reads less orange than it is, which pulls toward under-correction. In the model's scenes it made no
  measurable difference (the same residual with and without sensor clipping): their bright areas are small. With a
  large lamp-lit white wall it can matter **(unverified)**. The gate on the brightest channel (3.1, step 1) is cheap
  insurance.
- **Gray-world bias in daylight.** Skin has a linear saturation of 0.59, so a centre face counts fully (centre weight
  1.0). Office at 5500 K: −27 mired (6.6°) today. Closeup: −49 (11.6°). Warm room (beige walls): −57 (13.5°).
- **One-colour pictures are over-corrected today** (the gain limit is the only brake). At 5500-6500 K: face fill −79
  to −84 mired (18°, a bluish face), dark-skin face fill −103, a red or green wall −84. Today there are 47 cases where
  Auto Adjust's result is worse than no correction at all. The face's effect shows here too.
- **Camera auto white balance half-correcting.** The W4GS's own auto under tungsten is **(unverified)**: where it
  stops and how much warmth it leaves. With a camera auto modelled as "corrects 70 % of the mired distance, floor
  2800 K", today's Auto Adjust gives office −2 mired at 2700 K but +44 at 2200 K. With "100 %, floor 2800 K" it gives
  −21 at 2700 K (over-corrected by the skin bias). With either model, the proposal gives −4 mired (office) and −6
  (grey room) at every light from 2200 K to 5500 K, and −23 for the closeup.

### 2.7 Checked and not a cause

- **sRGB vs linear.** Gains are applied to linear light (`auto_adjust.hlsl:61`, the `_SRGB` view). Statistics are
  linearised per tap (`stats_downsample.hlsl:42-43`). The only nonlinearity is the bilinear tap averaging about 2 × 2
  coded texels before it decodes, which is negligible for colour.
- **Time constants.** Auto Adjust: `τ = 0.5 s` (`auto_adjust.rs:158, 206`). It reaches its steady state within about
  2 s, and the steady state itself is wrong. The device loop moves `ΔM_max` = 20 mired per decision at Normal
  (`white_balance.rs:79-83`), about 0.5-0.8 s per decision (gap 0.4 s + `D_apply` 0.3 s + 2 samples). 4600 → 2800 K is
  140 mired, about 7 decisions or 4-6 s. That is slow, but it converges.
- **`α`.** The initial `α = 0.006` (`white_balance.rs:30`) is about half the physical 0.010-0.012. The first steps are
  limited to `ΔM_max` anyway. Near the end a step covers `0.5 × 0.011/0.006 ≈ 0.92` of the error, which is still under
  1, so it does not oscillate, and learning corrects it. Whether the W4GS's processing compresses `α` is
  **(unverified)**. Read `learned.wb_alpha` after the live check (7, step 8) before changing the constant.
- **Warmth default and "leave the device warmth at 0".** With a correct estimator, Auto Adjust's output is its own aim
  whatever its input. The device warmth only changes Auto Adjust's input, so the two cannot add up. Today the device
  warmth leaks through slightly, but only because the estimate is biased. The hint is not needed after the fix (4).

---

## 3. Recommended algorithm

### 3.1 The shared estimator (`fx/awb.rs`)

One pure function, `estimate_light(stats) -> LightEstimate`, used by Auto Adjust and by the device meter, so the two
cannot drift apart (as with the warmth scale today). It starts from plain gray world and then repeats three times:
correct every cell by the current estimate, keep the cells that are near neutral **after** that correction, and
re-average. Under a strong cast, the neutral surfaces (wall, paper, grey clothes) therefore come back into the average
as soon as the first estimate has moved toward the light, and saturated objects drop out in both directions. The
average is shades-of-gray (Minkowski p = 4), which leans on brighter surfaces, and cells with a clipped channel do not
count. The result is then read as a light: its temperature (the blue-red axis) is kept, and its tint is limited to
what real lamps show. An estimate on the magenta side of the blackbody line is what skin, wood or a red wall give,
and lights are never there, so it lowers the confidence instead of being corrected away.

Steps and constants (cells with coverage < 0.02 are skipped, as today):

1. `base_i = centre_i · cov_i · smoothstep(0.04, 0.15, y_i) · (1 − smoothstep(0.85, 0.97, y_i)) · (1 − smoothstep(0.90,
   0.98, max(r, g, b)_i))`, with `y_i = srgb_encode(luma_i)`. The last factor is new: the clip gate (2.6).
2. Shades-of-gray mean with **p = 4**: `m_k = (Σ w_i c_ik⁴ / Σ w_i)^(1/4)`, `E = m / luma(m)`. Start: `w = base`.
3. Three times: `q_i = c_i / E`; `t_i = ln(q_B/q_R)`, `g_i = ln q_G − (ln q_R + ln q_B)/2`;
   `d_i = √(t_i² + (2 g_i)²)`; `w_i = base_i · (1 − smoothstep(0.35, 0.80, d_i))`; `E` = the mean of step 2 with `w`.
   Stop early if `Σ w ≤ 1e-6` and keep the previous `E`. Logs are floored at `1e-6`.
4. `share = Σ w / Σ (centre · cov)`; `conf₀ = smoothstep(0.02, 0.12, share)` (today's curve).
5. **Locus prior.** `(t, g)` of `E`. The blackbody line through the origin is `g_L = −0.14 · t`. A line slides along
   itself, so the camera's own Kelvin is not needed; its bend is covered by a tolerance `tol = 0.04 · |t|`.
   `dg = g − g_L`.
   - Skin and surface guard: `conf = conf₀ · (1 − smoothstep(0.08, 0.16, max(0, −dg − tol)))`. It acts on the
     magenta side only: light skin is about 0.25 magenta of the line, a blackbody cast is on it.
   - Tint kept: `dg_c = clamp(dg, ±(0.06 + tol))`. The light estimate is
     `E' = (e^(−t/2), e^(g_L + dg_c), e^(t/2))`, normalised to luma 1.
6. Output: `LightEstimate { t, g: g_L + dg_c, conf, share, reason }`, with `reason` = `Dark` when
   `lit = Σ centre·cov·smoothstep(0.04, 0.15, y) / Σ centre·cov < 0.12`, else `OneColour` when `conf < 0.3`, else
   `Ok`.

Cost: 256 cells × 4 passes, a few µs on the CPU per frame. Noise: with 1 % per-cell noise, the standard deviation of
`t` is ≤ 0.005 over 200 frames (≤ 0.5 mired; office and ColorChecker at 2700 and 5500 K). That is within Auto Adjust's dead band (0.008) and the device's `B_in`
(0.015).

Variants tried in the model (normal scenes at 4000-6500 K, mean / max angle; worst one-colour case):

| Variant | Normal, daylight | One colour, daylight | Worse than none |
|---|---|---|---|
| Today | 5.5° / 13.7° | 18.1° / 26.7° | 47 |
| Plain gray world, no gate | 7.2° / 13.9° | 24.0° / 35.3° | 62 |
| Re-gated × 3, p = 1, no prior | 3.5° / 13.8° | 18.5° / 42.5° | 38 |
| + locus prior (curve), guard on any off-locus | 3.4° / 12.5° | 10.5° / 37.7° | 4 |
| + p = 4, magenta-only guard, gains 0.25-4 | 3.6° / 12.4° | 9.0° / 20.1° (= no correction) | 1 |
| **Proposed** (+ locus as a line with tolerance) | 3.5° / 13.0° | 9.0° / 20.1° | 1 |

p = 6 gave the same as p = 4. A gate of (0.25, 0.60) gave the same as (0.35, 0.80). The worst normal daylight case
(13°) is the warm room, see 3.6.

### 3.2 Auto Adjust

Auto Adjust keeps its shape: it measures its input, it is feed-forward, gains are in linear light, luma is kept, and
it is smoothed with `τ`. Only the target changes. `white_balance_target` calls `estimate_light`, builds the gains that
take the estimated light to the aim (`white_aim(warmth, tint)`, unchanged), and scales them by the confidence in the
log domain. A picture that does not look like a cast (one colour, a face filling the frame) is therefore corrected
only as far as the evidence goes, instead of slowly all the way. The per-channel limit grows to `0.25 ..= 4` so that
a 2700 K light on a camera at 4600 K, or a 2200 K bulb on a camera at its 2800 K floor, can be corrected fully.

- `G = aim / E'`, renormalised so `luma(G · E') = 1`; `ln g_k = conf · ln G_k`; then today's limit-keep-limit with
  `MIN_WB = 0.25`, `MAX_WB = 4.0`.
- Dead band `WB_BAND = 0.008` as today, measured against the held gains. Confidence no longer multiplies the step: it
  is already in the target. `follow()` and `τ` are unchanged.
- The wanted gains are computed from the input every frame, without state: the same input gives the same target, so
  there is no hysteresis.
- The module doc (`auto_adjust.rs:31-46`) is rewritten to match.

### 3.3 Better Cam auto white balance (device loop)

The device loop keeps its step-and-settle design (AUTO-CONTROLS.md 2.6) and changes only its meter.
`measure_white_balance` returns `e_wb = t − c*(warmth)` from the shared estimate and `conf` from it, so under a strong
cast it has a confident, unbiased reading and walks toward the light instead of holding at its seed. It still holds at
`conf < 0.3`, and the reason (`Dark` or `OneColour`) picks the status text. When it reaches 2800 K, whatever is left
goes to Auto Adjust, which now corrects it (it measures the device's output). The card says that, or offers to add
Auto Adjust.

- `WhiteBalanceReading` gains `reason`; `HOLD_BELOW = 0.3`, the bands, `ΔM_max`, `k` and the watchdog are unchanged.
- Tint stays with Auto Adjust (no device control), as today.
- `α` initial stays 0.006 until the live check reads `learned.wb_alpha` (2.7).
- Known limit, not fixed: a camera left at 2800 K that then gets daylight on a scene with many magenta patches (the
  ColorChecker in the model) can read `OneColour` and hold. Live use leaves the range end only through this loop, and
  the guard is magenta-only, so this needs an unusual scene. Watch for it in the live check (7, step 7).

### 3.4 Residual warmth: the warmth target's job

With an unbiased estimator, the look under warm light is decided by one number: the warmth aim. Both places already
share that scale (`white_aim`, `meter::warmth_target`). Recommendation: keep one mechanism, change the default.

| Default warmth | Look of a neutral (any light the range can correct) | `ln(B/R)` aim | Coded R − B on white paper (R ≈ 220) |
|---|---|---|---|
| 0 (today) | neutral, 5500 K | 0 | ≈ 0 |
| **+0.15 (recommended)** | barely warm, ≈ 5200 K | −0.10 | ≈ 10 |
| +0.3 | clearly warm, ≈ 4900 K | −0.20 | ≈ 20 |

A fixed warmth also warms daylight scenes slightly. The alternative, a warmth that follows the light (keep about 15 %
of the light's mired distance below 5500 K, so a 2700 K lamp would look like 4760 K and daylight neutral), needs the
absolute light temperature in Auto Adjust. Auto Adjust only sees its input, after Camera auto or the device loop has
corrected part of the cast, so it would need the camera's Kelvin passed in. It is listed as a later option, not
planned (8, D1).

### 3.5 Results of the proposal (model)

Residual mired (+ = too orange) / angle. **Camera manual at 4600 K (the W4GS default) + Auto Adjust:**

| Scene | | 2200 K | 2500 K | 2700 K | 3000 K | 4000 K | 5500 K | 6500 K |
|---|---|---|---|---|---|---|---|---|
| office | today | +185 / 31° | +134 / 26° | +58 / 13.5° | +20 / 5.0° | −14 / 3.5° | −27 / 6.6° | −29 / 7.1° |
| | proposed | +86 / 19° | +12 / 3.4° | −4 / 1.1° | −4 / 1.3° | −4 / 1.2° | −3 / 1.0° | −4 / 1.2° |
| grey_room | today | +278 (held) | +195 (held) | +51 / 12.8° | +18 / 4.8° | −15 / 3.6° | −20 / 4.9° | −21 / 5.0° |
| | proposed | +86 / 19° | +13 / 3.4° | −5 / 1.5° | −6 / 1.7° | −6 / 1.6° | −5 / 1.6° | −6 / 1.6° |
| closeup | today | +187 / 32° | +129 / 25° | +56 / 13.1° | +17 / 4.7° | −39 / 9.4° | −49 / 11.6° | −49 / 11.6° |
| | proposed | +80 / 20° | +6 / 4.3° | −22 / 5.4° | −23 / 5.6° | −22 / 5.5° | −23 / 5.5° | −23 / 5.5° |
| warm_room | today | +278 (held) | +195 (held) | +49 / 12.7° | +8 / 5.9° | −51 / 12.4° | −57 / 13.5° | −57 / 13.7° |
| | proposed | +74 / 20° | 0 / 6.7° | −31 / 7.6° | −53 / 13° | −52 / 12.8° | −53 / 12.9° | −53 / 12.9° |

**Better Cam auto white balance from 4600 K, then Auto Adjust** (device alone in brackets):

| Scene | | 2200 K | 2500 K | 2700 K | 3000 K | 4000 K | 5500 K |
|---|---|---|---|---|---|---|---|
| office | today | +44 (+135) | −6 (+52) | −21 (+15) | −27 (−26) | −27 (−26) | −27 (−28) |
| | proposed | **−4** (+135, at limit) | **−4** (+52, at limit) | **−4** (+15) | −4 (−3) | −3 (−4) | −4 (−5) |
| grey_room | today | +278 (held) | +195 (held) | −18 (+15) | −20 | −20 | −20 |
| | proposed | −6 | −6 | −6 | −6 | −6 | −6 |
| warm_room | today | +278 (held) | +195 (held) | +49 (+158, held) | −57 | −57 | −58 |
| | proposed | −43 | −52 | −53 | −54 | −53 | −53 |
| face_fill | today | +167 | +168 | +168 | −84 | −84 | −84 |
| | proposed | −62 | +50 | +83 | +117 (= none) | +32 (= none) | −34 (= none) |

At 2000 K (a candle or a very orange LED) the proposal leaves +60 to +70 mired with the device at 2800 K: the gain
limit of 4 binds (blue × 9.4 needed). That is intended: blue × 9 would be mostly noise.

### 3.6 Known limits (each stays visible in the test suite as a "no regression" case)

- **Warm room (beige walls, wood).** A beige wall in daylight has the colour of a grey wall under 4400 K. Colour alone
  cannot tell them apart. Proposed −53 mired (12.9°), today −57. The default warmth of +0.15 hides part of it.
- **Face filling the frame.** The skin guard stops the face from being neutralised (today it turns blue: −84). The
  price is that a face filling the frame under tungsten is corrected only as far as the device loop's previous state
  and the confidence allow. "Skin as a reference" (skin's chroma is consistent within about ±20 mired across the skin
  tones in the model) could correct it. It is a later option, not planned (8, D4).
- **Mixed light** (daylight through a window plus a desk lamp). One global white balance cannot make both neutral. The
  proposal follows the light on most of the neutral surfaces (mode-seeking). Office with a window at the left and a
  2700 K lamp at the right: daylight side −8 mired, lamp side +185. Today: −45 and +147, which sits 23 % of the way
  toward the lamp. A compromise rule (two clusters, weighted by area) was tried and detected mixed light only 4 times
  in 16, so it is not proposed. The owner decides the policy (8, D2).
- **A blue wall under tungsten** looks grey. Proposed and today both leave it uncorrected (+195 mired at 2500 K, no
  worse than no correction).

### 3.7 Options considered and rejected

| Option | Why not |
|---|---|
| Only widen the gain limits | Removes cause 2 only. Grey room at 2700 K: +51 → −3. Office at 2700 K: still +57, and at 2500 K still +134, with today's gate. |
| Plain gray world (drop the gate) | Normal scenes in daylight get worse (7.2° mean) and one-colour scenes much worse (62 cases worse than none). |
| Gray edge on the 16 × 16 cells | Cell means hide the edges that gray edge needs. A real gray edge needs a gradient pass on the GPU (a new shader and cost), for a method that is not better than the re-gated mean on these scenes. |
| White patch (brightest cells) | Under a warm light the brightest cells are clipped in red (2.6) or are the lamp itself. p = 4 and the clip gate keep the useful part. |
| Seed the re-gating from the held gains | Adds state and possible hysteresis in mixed light. Starting from plain gray world each frame converges in 3 passes on every scene tried. |
| Pass the camera's Kelvin into Auto Adjust (exact locus) | Couples a filter to device state. The straight-line locus with a tolerance did as well as the exact curve on the normal scenes and better with the camera at 2800 K (normal daylight mean 12.8° vs 23.1°). |
| Two-light compromise | Unreliable (3.6). |
| Face detection for the skin guard | No face detector in the pipeline. The colour-based guard handles the dominant case (a face filling the frame). |

---

## 4. UI and status texts

| Where | Today | New |
|---|---|---|
| WB row, `NoSignal`, reason `Dark` (`status.rs:97`) | "Nothing to measure (too dark). Keeping 4600 K." | unchanged |
| WB row, `NoSignal`, reason `OneColour` | (same "too dark" text, which is wrong) | "The picture is mostly one colour (a face or a wall): keeping 4600 K." |
| WB row, `at_limit` warm (`status.rs:92-96`) | "White balance at its limit (2800 K): the light is warmer than this camera can correct. Auto Adjust can finish it." | Backend: "White balance at its warmest (2800 K): the light is warmer than this camera can correct." Frontend line under it: with Auto Adjust white balance on in the chain, "Auto Adjust finishes the colour."; without, "Add Auto Adjust to finish the colour." with the **Add Auto Adjust** button (the exposure row's pattern, `AutoControls.tsx:186-197`; the added filter has white balance on by default). |
| WB row, `at_limit` cool | "…cooler than this camera can correct. Auto Adjust can finish it." | "White balance at its coolest (6500 K): the light is cooler than this camera can correct." + the same frontend line |
| WB sub-block hint (`AutoControls.tsx:224-226`) | "Auto Adjust has a warmth of its own. Leave this one at 0 while Auto Adjust white balance is on." | With Auto Adjust white balance on: "Auto Adjust's warmth decides the final colour ({w})." No "leave at 0" (2.7). |
| Auto Adjust Warmth slider (`filters/schema.ts:284`) | def 0, hint "Aim for an orange (up) or blue (down) cast" | def per D1 (+0.15 recommended), hint "0 is neutral. Up keeps warm light a little warm, down cools." |
| Camera card Warmth (`camera/schema.ts:668`) | def 0 | same default as Auto Adjust (D1) |

A default change applies to new Auto Adjust filters and to camera records without a stored warmth. Stored values are
kept. No new controls.

---

## 5. Synthetic test suite

### 5.1 The scene kit (`fx/awb/scenes.rs`, `#[cfg(test)]`, shared by both test modules)

- **Reflectances** (8-bit sRGB codes as they look under neutral light, decoded to linear): the 24 ColorChecker
  patches; skin light (194, 150, 130), mid (170, 120, 95), dark (115, 82, 68); dark hair (55, 42, 35); walls grey
  (200, 200, 196), white (232, 232, 226), beige (215, 196, 170); paper (242, 242, 238); shirts blue (60, 80, 140) and
  grey (110, 110, 112); wood (150, 105, 65); plant (70, 110, 60); red mug (175, 54, 60); sofa (95, 62, 45);
  one-colour walls red (170, 60, 55), blue (60, 80, 160), green screen (60, 160, 70), orange (220, 140, 70).
- **Scenes:** the 12 of 2.1, as 16 × 16 layouts with a face oval (centre (7.5, 7.5), radii 3.2 × 4.2 cells; closeup
  5 × 7). The layouts are fixed in code, copied from the analysis model.
- **Lights:** blackbody 2000, 2200, 2500, 2700, 3000, 3500, 4000, 5000, 5500, 6500 K (`blackbody_rgb`, made
  `pub(crate)`). Mixed: two lights blended over 4 columns at split 5, 8 and 11, pairs 5500 | 2700, 6500 | 3000 and
  5000 | 2200.
- **Camera:** `ρ · bb(T) / bb(K)`, exposure bisected to a mean coded luma of 0.45, 16 sub-samples per cell with the
  texture `1 + 0.3·((7s mod 5) − 2)/2`, clip at 1, 8-bit sRGB round trip, mean → `Stats`; two clipped highlight cells.
  An optional noise mode adds `1 % · value + 0.002` per cell (seeded xorshift, as `sim.rs`).
- **Metric:** `angle(gains · bb(T)/bb(K), aim)` in degrees and `residual = −(ln(B/R)_out − ln(B/R)_aim) / 0.0105`
  mired.

### 5.2 Cases and pass thresholds

Thresholds are the model's results with a margin of about 1° or 5 mired. "None" means the uncorrected camera picture.

**Estimator and Auto Adjust, pure** (`fx/awb/tests.rs`, `fx/auto_adjust/tests.rs`). Camera at 4600 K unless stated:

| # | Case | Pass |
|---|---|---|
| A1 | office, office_dark_skin, grey_room, checker × 2700-6500 K | angle ≤ 3.0°, \|residual\| ≤ 12 mired |
| A2 | the same at 2500 K | angle ≤ 5.0° |
| A3 | the same at 2200 K (gain-limited) | residual ≤ +95 mired and less than half of today's |
| A4 | camera at 2800 K, those scenes × 2200-3000 K | angle ≤ 3.0° |
| A5 | closeup × 2500-6500 K; warm_room × 3000-6500 K | closeup ≤ 6.5°; warm_room ≤ 14° (no regression vs today's 13.7°) |
| A6 | every one-colour scene (face fills, walls) × every light × cameras 4600 and 6500 K | angle ≤ none + 1° (never worse than no correction) |
| A7 | face_fill, red_wall × 5000-6500 K | `conf ≤ 0.1` (the skin guard holds) |
| A8 | office at 2700 K: the gate is judged after correction | grey-wall cells weigh ≥ 0.8 in the last pass; blue shirt ≤ 0.2 |
| A9 | noise mode, office and checker at 2700 and 5500 K, 200 frames | sd of `t` ≤ 0.01 |
| A10 | a neutral grey picture, warmth 0 | gains exactly 1 (existing test kept) |
| A11 | limits | every gain within `0.25 ..= 4`, luma kept within 2 % (existing test, new numbers) |
| A12 | mixed 5500 \| 2700, split 8 | the side with more neutral surface (daylight) within ±15 mired: the policy of D2, the case is rewritten if D2 changes |
| A13 | the existing one-colour test (`tests.rs:679`, wall (200, 150, 110)) | reworded: "not turned grey": output R > B, and `conf < 1` |

**GPU** (WARP, `fx/auto_adjust/tests.rs`): render `office` at 2700 K / 4600 K as a 256 × 144 texture (each cell 16 × 9
pixels with the same texture). Run 90 frames at 30 fps. Mean output colour of the grey-wall pixels: angle ≤ 3° from
neutral. The same at 5500 K: ≤ 2°. The `Stats` from the real chain match the CPU kit's within 0.003 per channel (this
checks the CPU model of `stats_downsample`). Perf: the existing 1080p cost test stays under its limit, and the CPU
time of `estimate_light` is ≤ 50 µs (median of 100).

**Device loop** (`auto_controls/tests.rs` with `sim.rs`, which gets coloured scenes). Today `Scene` is neutral with a
uniform tint. New: per-cell reflectance colours from the kit, and a cast of `(bb(T)/bb(K))^(α_true/0.0105)` per
channel, so the `α_true` variants stay.

| # | Case | Pass |
|---|---|---|
| B1 | office, light 6500 → 2700 K at t = 5 s, seed 6500 K | device ≤ 2850 K and `holding` or `at_limit` within 10 s; with Auto Adjust after it: ≤ 2° |
| B2 | office, 2200 K light | `at_limit` at 2800 K within 10 s; device + Auto Adjust ≤ 2° |
| B3 | warm_room and grey_room, 2700 K light, seed 4600 K | no hold at the seed: device ≤ 2900 K within 10 s (today: holds, +158) |
| B4 | `a_cast_with_nothing_neutral_to_trust_is_held` (`tests.rs:1015`) | **reversed**: renamed `a_strong_cast_is_followed`, converges (`\|e_wb\|` < 0.03 or `at_limit`) |
| B5 | face_fill and red_wall under 5500 K, seed 4600 K | state `NoSignal`, reason `OneColour`, ≤ 1 write, text of 4 |
| B6 | cases 15-17 (`tests.rs:998-1030`) with the office scene | their existing criteria |
| B7 | office in daylight, seed 4600 K | holds within 6 s with \|residual\| ≤ 10 mired (today −28) |
| B8 | noise (sim jitter) at 2700 K, 60 s holding | ≤ 1 write per 10 s, no reversal pattern |

**Frontend** (`npm run test:schema`): new warmth defaults load, and stored warmths are kept.

---

## 6. Build phases

Each phase is one coding session. W1 → W2 → W3 → W4; W2 and W3 can run in parallel after W1.

### W1 — Shared estimator and scene kit

- **Goal:** `estimate_light` (3.1) and the test scene kit (5.1); cases A1-A10 on the estimator plus a gains helper
  (`awb::gains(est, aim, conf, limits)`).
- **Files:** new `src-tauri/src/fx/awb.rs`, `src-tauri/src/fx/awb/tests.rs`, `src-tauri/src/fx/awb/scenes.rs`;
  `src-tauri/src/fx/mod.rs` (one `pub(crate) mod awb;` line; this file is being edited by the extensions work, see
  git status, so add the line last and rebase); `src-tauri/src/fx/auto_adjust.rs` only to make `blackbody_rgb`
  `pub(crate)`.
- **Check:** `cargo test -p better-cam awb` passes A1-A12 (A12 per D2); `cargo clippy -p better-cam -- -D warnings`
  clean.

### W2 — Auto Adjust uses it

- **Goal:** 3.2: the new `white_balance_target`, the limits `0.25 ..= 4`, confidence in the target, the module doc
  rewritten, `filters/schema.ts` warmth default and hint (per D1).
- **Files:** `src-tauri/src/fx/auto_adjust.rs`, `src-tauri/src/fx/auto_adjust/tests.rs`, `src/filters/schema.ts`.
- **Depends on:** W1.
- **Check:** `cargo test -p better-cam auto_adjust` passes, including A11, A13, the GPU and perf cases of 5.2, and every
  existing test (`auto_adjust_a_blue_cast_goes_neutral…`, `…both_corrections_together`, `…does_not_oscillate…`,
  `…static_scene_with_sensor_noise…`, `…a_picture_at_target_and_neutral_is_left_alone`) unchanged or changed only as
  listed in 5.2. `npm run build` passes.

### W3 — Device meter and loop

- **Goal:** 3.3: `measure_white_balance` on the shared estimator with `reason`; `NoSignal` texts by reason; `at_limit`
  texts; the coloured sim; cases B1-B8; AUTO-CONTROLS.md 2.4, 2.6, 4.2 and 6.2 updated (one note per changed rule).
- **Files:** `src-tauri/src/auto_controls/{meter.rs, white_balance.rs, status.rs, sim.rs, tests.rs}`,
  `docs/AUTO-CONTROLS.md`.
- **Depends on:** W1, and **the F1-F6 follow-ups merged** (same files are being edited now).
- **Check:** `cargo test -p better-cam auto_controls` passes all 24 cases of AUTO-CONTROLS.md 6.2 (case 16 with the new
  text) and B1-B8; clippy clean.

### W4 — Card and defaults

- **Goal:** the frontend lines of 4: the `at_limit` line with Add Auto Adjust, the new warmth hint, the camera warmth
  default (D1).
- **Files:** `src/components/AutoControls.tsx`, `src/camera/schema.ts`, `src/camera/schema.test.ts`, the card's
  stylesheet if needed.
- **Depends on:** W3 (texts).
- **Check:** `npm run test:schema` and `npm run build` pass. A real camera cannot reach `at_limit` without warm light,
  so the three states (with Auto Adjust, without it, cool limit) are checked by feeding `auto-status` payloads to the
  card in `npm run tauri dev` (emitting a hand-made event from the devtools console; whether the frontend accepts one
  from there is unverified, otherwise use a small render test), with a screenshot of each. The live look comes in W5.

### W5 — Owner live check and constants (needs the owner and a warm light)

- **Goal:** section 7; change only constants the live data contradicts (`α` initial, the gate's 0.35/0.80, the guard's
  0.08/0.16), each recorded old → new with the reason in this document.
- **Files:** `docs/AUTO-WHITE-BALANCE.md` (a "Live results" section), constants in `fx/awb.rs` and
  `auto_controls/white_balance.rs`.
- **Check:** every step of 7 recorded with its measured value; steps 2-6 meet their criteria, or the gap is listed with
  the owner's agreement.

---

## 7. Owner's live checklist

What you need: the W4GS on its usual stand; a lamp with a **2700 K** bulb (most "warm white" LED bulbs; the box says
2700K); if you have one, a **2200 K** "vintage" or candle-style bulb; a sheet of white printer paper; for step 5, a
window with daylight. Better Cam at 1280 × 720, 30 fps, as you normally use it. Write down each value asked for. A
screenshot uses the app's preview (Win + Shift + S).

**How to read the paper:** open the screenshot in Paint, pick the colour in the middle of the paper with the colour
picker, then Edit colours shows Red, Green, Blue (0-255). "R − B" is Red minus Blue.

1. **Find your setup.** Note the setup in which you saw the problem: the white balance mode in Camera controls (Manual,
   Camera auto or Better Cam auto) and its Kelvin, and whether Auto Adjust is in the chain with Auto white balance on
   and its Warmth.
2. **Baseline (before the update).** Room lit by the 2700 K lamp only (curtains closed or evening). Your face in the
   middle, the paper at the side of the picture, about a quarter of the frame height. For each of these four setups,
   wait 10 s, take a screenshot, and note the paper's R, G, B, the Kelvin in the card and the white balance status
   line:
   a. White balance Camera auto, Auto Adjust off.
   b. White balance Camera auto, Auto Adjust on (exposure and white balance on, Warmth 0).
   c. White balance Better Cam auto, Auto Adjust off.
   d. White balance Better Cam auto, Auto Adjust on.
   Expected today: the paper is visibly orange in all four, with R − B well above 20 in b and d. The model says so,
   but the exact values are unverified: they depend on the W4GS's own auto and on the paper's brightness.
3. **After the update, same four setups.** Pass: in b and d, R − B is between −5 and +20 with the default warmth
   (+0.15 if D1 is accepted; −5 to +10 with Warmth 0). In c, the card shows a Kelvin of 2900 or less. The paper in b
   and d looks white or barely cream, not orange.
4. **2200 K bulb** (if you have one), setups c and d. Pass: c shows "White balance at its warmest (2800 K) …" within
   10 s; d shows "Auto Adjust finishes the colour." and the paper's R − B is ≤ 30.
5. **Mixed light.** Daylight from the window on one side, the 2700 K lamp lit on your face from the other side, setup
   d. Take a screenshot. Note which looks right: the wall in daylight, your face, or neither. This decides D2.
6. **Face filling the frame.** Setup d under the lamp. Lean in until your face fills the picture, hold 10 s, lean back.
   Pass: your face does not turn grey or blue while you are close. The status may say "The picture is mostly one
   colour…". After you lean back, the colour returns within 5 s.
7. **Steady and switching.** Setup d, sit still for 2 minutes: no visible colour drift or pulsing. Then switch the lamp
   off and the ceiling or daylight on, and back. Each time, the colour settles within 8 s without swinging past and
   back. Also: after a tungsten evening (the card at 2800 K), the next morning in daylight, the Kelvin must rise again
   within 10 s (3.3, known limit).
8. **Learned values.** In the app's developer tools (or ask the agent to read it): `localStorage["better-cam"]`, the W4GS
   record, `learned.wb_alpha`. Note the value (the model expects about 0.010-0.012; the code starts at 0.006).
9. **Camera facts (unverified so far).** In Camera auto under the 2700 K lamp: the Kelvin the card shows (does the W4GS
   report its own auto value, and which?). In Manual: the lowest Kelvin the slider allows (expected 2800).

---

## 8. Decisions for the owner

| # | Question | Options | Recommendation |
|---|---|---|---|
| D1 | How much warmth to keep by default? | 0 (neutral, today); **+0.15** (barely warm, ≈ 5200 K look, everywhere); +0.3 (clearly warm). Later option: warmth that follows the light (warm only under warm light; needs the camera's Kelvin in Auto Adjust) | **+0.15** for new Auto Adjust filters and camera records. It is subtle in daylight, flatters skin, and is one number in one place. Revisit "follows the light" only if tungsten scenes look too white after the update. |
| D2 | In mixed light, which light wins? | **The light on most neutral surfaces** (proposed: background right, a lamp-lit face stays warm); a compromise between both (both sides wrong by half) | The proposed rule for now. Step 5 of the live check decides whether to build a compromise. |
| D3 | Auto Adjust's largest gain per channel | 2 (today: leaves +51 mired at 2700 K); **4** (two stops; corrects 2700 K on a camera at 4600 K and 2200 K with the device at 2800 K); 8 (candles; noisy blue) | **4**. |
| D4 | A face filling the frame under warm light | **Hold** (the skin guard: the face is not neutralised, warm light is only partly corrected); skin as a reference (corrects it, ±20 mired by skin tone; another phase) | **Hold** now; skin reference only if step 6 shows a face that stays orange in your real use. |
