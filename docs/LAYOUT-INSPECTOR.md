# Better Cam layout rework (Inspector) and full presets

Status: design, not built. Written against `feat/layout-inspector` at `d202708`. No application code was changed for
this document.

## Recommendation

Rebuild the window as three docked regions under the title bar: a compact **navigator** on the left, the **preview**
in the centre, and an **inspector** on the right that shows the settings of one selected item. Move the preset control
into the title bar and drop the toolbar row. Make a preset hold the whole look of the camera: the filter chain, the
camera and its capture settings, the camera controls with their modes, Better Cam auto's settings, and the output.

Build it in five coding sessions, in three waves:

| Wave | Brief | What the user gets | Runs in parallel with |
|---|---|---|---|
| 1 | **L1** Shell and navigator | Three regions, docked preview, two splitters, chain rows in the navigator, selection, drag and drop rewired | P1 |
| 1 | **P1** Preset format and Rust | Versioned preset file, pure apply plan, Rust keeps the new sections, rename | L1 |
| 2 | **L2** Inspector content | Compact property rows, inspector header for filters and sections, no nested cards | P2 |
| 2 | **P2** Preset store and menu | Save and load everything, undo, dirty mark, rename, reset to defaults | L2 |
| 3 | **L3** Narrow windows, keyboard, status | Icon rail below 1100 px, flyout, keyboard navigation, row summaries and status dots | — |

---

## 0. Requirements and constraints

### From the owner (verbatim intent and decisions)

| # | Requirement |
|---|---|
| O1 | Less scrolling to reach the settings of a thing. |
| O2 | The preview is docked and always visible. |
| O3 | A more modern, usable layout everywhere, filters included: cleaner hierarchy, compact rows, less chrome. Same theme. |
| O4 | Presets save and reapply everything the user can set, except app-level settings. |
| O5 | Layout "Inspector": navigator left, preview centre, inspector right (decided, not reopened). |
| O6 | Old filter-only presets and preset drop-import keep working. |
| O7 | Every existing feature keeps working (list in 0.3). |
| O8 | Works from 900×600 to 1920×1080 and up; the preview stays visible at every size. |

### Constraints found in the code

| # | Constraint | Evidence |
|---|---|---|
| C1 | Window minimum is 900×600, default 1200×800, frameless, acrylic. | `src-tauri/tauri.conf.json:17-24` |
| C2 | Theme tokens live in `:root`; the page is one translucent layer with hairlines; floating things are frosted. | `src/styles.css:1-23` |
| C3 | Today: a toolbar row (Preset, Save, Collapse all, Add filter), then two columns. The left column pins the preview above its own scroll of Camera, Camera controls and Output; the right column scrolls the chain of cards. Below 1100 px everything stacks under a sticky preview band. | `src/App.tsx:67-121`, `src/styles.css:228-240`, `src/styles.css:565-581` |
| C4 | One splitter, saved as a ratio `ui.split`, plus fold flags for Camera, Camera controls, Output and Advanced. | `src/ui/layout.ts:16-37`, `src/components/Splitter.tsx:10-93` |
| C5 | Filter cards fold; the folds are saved per filter id (`collapsed`), and "Collapse all" folds every card. | `src/store.ts:76-77`, `src/store.ts:1262-1269`, `src/App.tsx:30`, `src/App.tsx:70-72` |
| C6 | The Camera controls card reads the camera only while it is open and the engine is settled (a read with no open camera lights the camera LED). | `src/components/ControlsCard.tsx:41-55` |
| C7 | A preset today is the chain only. The frontend writes `{app, version: 1, chain}`; Rust strips everything but the chain on save and on load. | `src/filters/chain.ts:131-157`, `src-tauri/src/commands.rs:339-370` |
| C8 | Rust tests pin the "chain only" behaviour (a `camera` section is dropped). | `src-tauri/src/commands.rs:461-488` |
| C9 | Old presets are a bare array or `{chain}`; both load. A dropped `.json` is read by `read_preset_file` (1 MiB cap, BOM stripped) and imported under its file name. | `src-tauri/src/commands.rs:389-414`, `src/store.ts:1327-1340`, `src/ui/fileDrop.ts:135-149` |
| C10 | Device controls are stored per camera, only where they differ from the device's default state; Better Cam auto is `mode: "app"` with the value as the resume point. Better Cam auto settings sit in the same record. | `src/camera/schema.ts:632-693`, `src/camera/schema.ts:727-737`, `src/camera/schema.ts:621-622` |
| C11 | `set_source(source, controls)` replaces the backend's record but does **not** write it to an open camera; the record is written only after a camera open (`apply_on_open`), and only the controls it lists. | `src-tauri/src/commands.rs:67-77`, `src-tauri/src/api.rs:414-430`, `src-tauri/src/camera_controls.rs:515-528`, `src-tauri/src/camera_controls.rs:630-684` |
| C12 | `set_control_mode(app)` on a control the record already marks `app` does nothing on the device. | `src-tauri/src/camera_controls.rs:1013` |
| C13 | Undo is a single snapshot of the chain plus the active preset; Ctrl+Z presses the toast's Undo. | `src/store.ts:94-99`, `src/store.ts:1219-1224`, `src/components/Toast.tsx:29-39` |
| C14 | Reorder, insert-drag and file-drop find the chain by DOM hooks: `[data-drop="chain"]`, `.chain > li.step`, `.card`, `li.empty`. Each has its own copy of the gap maths. | `src/ui/useChainDrag.ts:124-140`, `src/ui/insertDragSession.ts:50-64`, `src/ui/useFileDrop.ts:41-58` |
| C15 | The preview measures its own box and asks the backend for that size, capped at 960×540. | `src/components/Preview.tsx:151-172` |
| C16 | Checks run through the Chrome DevTools Protocol against a debug build placed with `BETTER_CAM_TEST_WINDOW`; `window.__betterCam` (dev only) drops files, toasts, patches the store and feeds `auto-status`. | `docs/UX-PASS.md:155-165`, `src/ui/useFileDrop.ts:202-216` |
| C17 | Test scripts: `npm run test:schema`, `test:ui`, `test:ext` (esbuild + node); `npm run build` is `tsc && vite build`. | `package.json:9-12` |

### 0.3 Features that must keep working

Pointer reorder, keyboard lift-and-move and Alt+arrows, Undo of a move (`useChainDrag.ts`, `chainDrag.ts`); insert-drag
out of the Add menu and the "+" between steps (`insertDragSession.ts`, `AddFilter.tsx:118-134`); OS file drops on path
fields, the chain and the window, the `.png` chooser (`useFileDrop.ts`, `fileDrop.ts`, `DropChooser.tsx`); preview
point, hold to Compare, eyedropper and crop-on-picture (`Preview.tsx`, `PickOverlay.tsx`, `CropOverlay.tsx`,
`previewTool.ts`); toasts with Undo in the footer (`Toast.tsx`); banners (`VcamBanner.tsx`, `EngineBanner.tsx`); the
empty-chain state (`EmptyChain.tsx`); the NVIDIA prompt (`NvidiaPrompt.tsx`); Settings (`TitleBar.tsx:124-166`); the
extension notes, NVIDIA "needs" lines, filter errors and "Loading model…" lines (`FilterCard.tsx:135-163`).

---

## 1. Layout

### 1.1 Regions

```
┌ title bar 44 px: brand · status · PRESET ▾ · Save ············ Pause · Start/Stop · Settings · window buttons ┐
├ banners (only when there is one; full width) ──────────────────────────────────────────────────────────────────┤
├ navigator ─┬┬ centre: preview ────────────────────────────────────────────┬┬ inspector ──────────────────────┤
│            ││                                                              ││                                 │
├────────────┴┴──────────────────────────────────────────────────────────────┴┴─────────────────────────────────┤
└ footer 49 px: toast · drop hint · stats ───────────────────────────────────────────────────────────────────────┘
```

The toolbar row is removed. Its four items go elsewhere:

| Today (`App.tsx:67-74`) | New place |
|---|---|
| Preset menu and Save | Title bar, after the status text (section 3.8) |
| Collapse all / Expand all | Removed: nothing folds any more |
| Add filter | Navigator: a "+" on the FILTERS heading and an "Add filter" row at the end of the list |

Sizes (CSS px; "auto" means the size until the user drags a splitter):

| Region | Auto width | Drag range | Narrow rule |
|---|---|---|---|
| Navigator, full | `clamp(200px, 17vw, 260px)` | 184 to 320 | Below 1100 px window width, or when the user collapsed it: an icon rail, 52 px (section 1.6) |
| Centre | the rest | min 360 | Never hidden. The other two give way first |
| Inspector | `clamp(340px, 25vw, 460px)` | 300 to 560 | Shrinks towards 300 so the centre keeps 360 |

Effective widths are worked out from the saved values every render, in this order: the navigator is the rail below
1100 px, else its saved or auto width clamped to [184, W − 300 − 360]; the inspector is its saved or auto width clamped
to [300, min(560, W − nav − 360)], where W is the workspace width minus the two 1 px hairlines. The saved values are
not changed by a narrow window, so widening the window restores them.

The preview box fills the centre with 16 px padding, keeps 16:9 and is centred in both directions:
`width = round(down, min(100cqw − 32px, (100cqh − 32px) × 16/9), 1px)`. The centre is a size container, as `.col-left`
is today (`styles.css:236-240`). The backend still caps the picture at 960×540 (`Preview.tsx:151`); a larger box scales
the canvas up, which is fine for a preview.

Heights: title bar 44, footer 49 (`styles.css:52`, `styles.css:22`). The body is the window height minus 93 px, minus
the banners when shown.

### 1.2 Wireframe at 1200×800

Navigator 204 (auto), inspector 340 (auto), centre 654. Body 707 px. Preview 622×350.

```
┌──────────────────────────────────────────────────────────────────────────────────────────────────────────────┐
│● Better Cam  LIVE   PRESET Studio light* ▾   Save                     ‖ Pause   ■ Stop   Settings   –  □  × │
├───────────────────┬┬──────────────────────────────────────────────────────┬┬──────────────────────────────────┤
│ ▣ Camera          ││                                                      ││ ⠿ Chroma Key            On ━●  ⋯ │
│   BRIO · 1080p 30 ││                                                      ││   Step 3 of 4                    │
│ ▤ Camera controls ││                                                      ││   [◐ Before] [⊕ Pick from preview]│
│   3 changed     ● ││   ┌──────────────────────────────────────────────┐   ││                        [⟲ Reset] │
│ ▭ Output          ││   │ 30 fps                Compare  PREVIEW ━●    │   ││ ─────────────────────────────────│
│   Same as camera  ││   │                                              │   ││ Key Color Type        [Green  ▾] │
│ ───────────────── ││   │                                              │   ││ Similarity                   400 │
│ FILTERS         + ││   │              preview 622×350                 │   ││ ━━━━━━━━━━━━━●━━━━━━━━━━━━━━━━━━ │
│ ⠿01 Color Corr ━● ││   │                                              │   ││ Smoothness                80   ⟲ │
│ ⠿02 Crop/Pad   ━● ││   │                                              │   ││ ━━━━━━━━●━━━━━━━━━━━━━━━━━━━━━━━ │
│▌⠿03 Chroma Key ━● ││   │                                              │   ││ Key Color Spill Reduct…      100 │
│ ⠿04 Sharpen  ● ●━ ││   └──────────────────────────────────────────────┘   ││ ━━━━━━━━━━●━━━━━━━━━━━━━━━━━━━━━ │
│ + Add filter      ││                                                      ││ Opacity                     1.00 │
│                   ││                                                      ││ ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━● │
│                   ││                                                      ││ … (scrolls only if taller)       │
├───────────────────┴┴──────────────────────────────────────────────────────┴┴──────────────────────────────────┤
│ [Added Chroma Key as step 3 · Undo ×]      Not in use · choose “Better Cam” … · 1280×720 → 1280×720 · 30 fps │
└──────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
 ▌ = selected row (2 px accent bar + accent-soft fill)   ━● = bypass switch   ● = status dot   ⠿ = grip on hover
```

The inspector content is 296 px wide here (340 − 2 × 16 padding − 12 scrollbar gutter), under the 400 px line of
section 2, so sliders take two lines.

### 1.3 Wireframe at 1920×1080

Navigator 260 (auto), inspector 460 (auto), centre 1198. Body 987 px. Preview 1166×656. Inspector content 416 px:
sliders take one line.

```
┌──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┐
│● Better Cam  LIVE   PRESET Studio light ▾   Save                                    ‖ Pause   ■ Stop   Settings   –  □  × │
├──────────────────────┬┬───────────────────────────────────────────────────────────────────────┬┬────────────────────────────────┤
│ ▣ Camera             ││                                                                       ││ Camera controls      [⟲ Reset all]│
│   Logitech BRIO      ││                                                                       ││ 3 changed · Better Cam auto      │
│   1920×1080 · 30 fps ││  ┌─────────────────────────────────────────────────────────────────┐  ││ ─────────────────────────────────│
│▌▤ Camera controls    ││  │                                                                 │  ││ EXPOSURE AND COLOUR              │
│   3 changed       ●  ││  │                                                                 │  ││ Exposure      [Better Cam auto ▾]│
│ ▭ Output             ││  │                                                                 │  ││   ━━━━━━━━●━━━━━━┊░░░  1/64 s    │
│   Same as camera     ││  │                                                                 │  ││   │ Target bright… ━━━●━━━  0.45 │
│ ──────────────────── ││  │                     preview 1166×656                            │  ││   │ Longest exposure [Fit the f…▾]│
│ FILTERS            + ││  │                                                                 │  ││   │ Most gain     ━━━━━●━━    75 │
│ ⠿01 Color Correc ━●  ││  │                                                                 │  ││   │ Response          [Normal ▾] │
│ ⠿02 Crop/Pad     ━●  ││  │                                                                 │  ││   │ ● Holding 0.45 at 1/64 s     │
│ ⠿03 Chroma Key   ━●  ││  │                                                                 │  ││ Gain          ━━━━━━●━━━━   75   │
│ + Add filter         ││  │                                                                 │  ││   Set by Better Cam auto exposure│
│                      ││  └─────────────────────────────────────────────────────────────────┘  ││ White balance [Camera auto     ▾]│
│                      ││                                                                       ││ Backlight comp ━━━●━━━━━━━  1  ⟲ │
│                      ││                                                                       ││ PICTURE                          │
│                      ││                                                                       ││ Brightness     ━━━━━●━━━━━ 128   │
│                      ││                                                                       ││ …                                │
├──────────────────────┴┴───────────────────────────────────────────────────────────────────────┴┴────────────────────────────────┤
│                                     In use by 1 app · 1920×1080 MJPEG (HW) → 1920×1080 · 30 fps · GPU 0.4 ms · 38 ms latency │
└──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
```

### 1.4 Wireframe at 900×600

Rail 52 (automatic below 1100 px), inspector 340, centre 506. Body 507 px. Preview 474×267.

```
┌──────────────────────────────────────────────────────────────────────────────────────────┐
│● Better Cam LIVE  PRESET Studio* ▾  Save           ‖ Pause  ■ Stop  Settings   –  □  ×   │
├─────┬┬──────────────────────────────────────────────────┬┬────────────────────────────────┤
│ »   ││                                                  ││ ⠿ Color Correction   On ━●  ⋯  │
│ ▣   ││                                                  ││   Step 1 of 3                  │
│ ▤ ● ││   ┌──────────────────────────────────────────┐   ││   [◐ Before]          [⟲ Reset]│
│ ▭   ││   │ 30 fps          Compare  PREVIEW ━●      │   ││ ───────────────────────────────│
│ ─── ││   │                                          │   ││ Gamma                     0.00 │
│▌01  ││   │            preview 474×267               │   ││ ━━━━━━━━━━━━━━●━━━━━━━━━━━━━━━ │
│ 02  ││   │                                          │   ││ Contrast                0.20 ⟲ │
│ 03● ││   │                                          │   ││ ━━━━━━━━━━━━━━━━●━━━━━━━━━━━━━ │
│ +   ││   └──────────────────────────────────────────┘   ││ Brightness                0.00 │
│     ││                                                  ││ ━━━━━━━━━━━━━━●━━━━━━━━━━━━━━━ │
│     ││                                                  ││ … (scrolls inside the inspector)│
├─────┴┴──────────────────────────────────────────────────┴┴────────────────────────────────┤
│ Not in use · choose “Better Cam” as the camera in your video app · 1280×720 → 1280×720 …  │
└──────────────────────────────────────────────────────────────────────────────────────────┘
 » opens the full navigator as a flyout over the left of the centre (section 1.6)
```

### 1.5 Navigator

Order, top to bottom: the three section rows, a hairline, the FILTERS heading, the filter rows, the Add filter row.
The list scrolls inside the navigator when it is taller than the body (`.nav-scroll`), which only a long chain causes.

**Sections.** Camera, Camera controls, Output. Better Cam auto is **not** its own entry: it is a mode of the
Exposure and White balance rows, and its settings appear only under those rows while that mode is chosen
(`ControlsCard.tsx:205-209`). A separate page would split one decision over two pages.

A section row is 44 px: an icon (16 px), the name (13 px, 600), and a one-line summary under it (11 px, `--dim`,
ellipsis). An 8 px status dot sits at the right end when there is something to see.

| Row | Summary | Dot |
|---|---|---|
| Camera | Device name; when Custom, `1920×1080 · 30 fps · MJPEG`; else `Device default` | warn: saved camera disconnected (`SourcePanel.tsx:41`), or a format fallback runs (`inUse`, `store.ts:177`) |
| Camera controls | `Defaults`, or `N changed` (the entries of the selected camera's record, `findStored`, `schema.ts:699-706`), plus ` · Better Cam auto` when a loop is in `app` mode | the worst `autoTone` of the two loops while in `app` mode (`schema.ts:927-944`); warn while the slow-camera hint holds (`AutoControls.tsx:209-228`) |
| Output | `Same as camera · 30 fps` or `1280×720 · Fill` | none |

The summaries read the store only. They never read the camera, so they do not light its LED (C6).

**FILTERS heading.** 11 px uppercase `.label`, tooltip "Applied top to bottom, camera first". At its right a 24 px "+"
button opens the Add menu (adds at the end). The "Camera in" and "Better Cam out" terminals (`App.tsx:103`, `:108`) go
away; 8 px of padding above the first row and below the last row hold the drop line for gap 0 and gap n.

**Filter row.** 32 px, one line. Left to right:

| Part | Size | Notes |
|---|---|---|
| Grip | 16×32 | Shown on row hover and while dragging; the whole row's empty area also starts a drag (as `.card-head .fill` does today, `FilterCard.tsx:116`) |
| Step number | 22 px, tabular, 11 px | `01`, `02`… |
| Label | flex, ellipsis | `labelOf(kind)`; full name in the tooltip |
| Status dot or spinner | 8 px / 10 px | see states below |
| Bypass switch | 30×16, hit 30×24 | the existing `.switch` (`styles.css:410-416`) |
| More (⋯) | 24×24 | shown on hover, on the selected row, and while its menu is open. Menu: Move up, Move down, Remove |

Row states:

| State | Look |
|---|---|
| Normal | text `--text`, number `--dim` |
| Hover | background `rgba(255,255,255,0.04)`, grip and ⋯ appear |
| Selected | 2 px `--accent` bar on the left edge, background `--accent-soft`, number `--accent`; `aria-current="true"` |
| Bypassed (`enabled` false, or an extension that does not run) | label and number `--faint`, switch off; no strikethrough |
| Error (`filterErrors[id]`, an NVIDIA "needs" line, an extension that is missing or failed) | 8 px `--danger` dot before the switch; the message is the dot's tooltip |
| Extension switched off | 8 px `--warn` dot |
| Loading model (`filterStatus[id]`) | the existing 10 px `.spinner` in the dot's place |
| Tool running (eyedropper or crop on this filter) | 1 px `--accent` outline round the row |
| Dragged (pointer) | the row stays as a faded slot (opacity 0.35); the lifted copy (`.reorder-lift`, `App.tsx:123-129`) follows the pointer, at most 300 px wide; a 2 px accent drop line between rows |
| Lifted (keyboard) | accent outline, as `.step.lifted` today (`styles.css:712-713`) |
| Drop target for a file | the row's own highlight when a fitting file is over a filter that has a path param (1.5.3) |

The "+" between rows (`AddHere`, `AddFilter.tsx:118-134`) stays: an 8 px strip over the gap at the top of every row
but the first, the 18 px round "+" at the left over the number column, shown on hover and focus, always on touch.

**Add filter row.** 32 px, "+ Add filter", `--dim`, opens the Add menu (it carries `data-add-filter`, which
`EmptyChain.tsx:9-13` looks for). Dragging an item out of that menu into the list inserts it (`insertDragSession.ts`).

**Empty chain.** `EmptyChain` moves into the navigator, between the FILTERS heading and the Add filter row, as
`li.empty`: one line "No filters yet." in `--dim`, then the quick adds as compact 28 px ghost rows (Color Correction,
Auto Adjust, Crop/Pad, and NVIDIA Background Blur with an RTX GPU), then "Or drop a .cube LUT or a picture here." in
11 px `--faint`. "More…" goes: the Add filter row is right under it. The block is the drop target of gap 0 as today
(`useFileDrop.ts:46-50`).

#### 1.5.1 DOM contract shared by reorder, insert-drag and file drop

One new module, `src/ui/chainGeometry.ts`, replaces the three copies of the gap maths (C14):

```ts
/** The filter rows of the navigator list, top to bottom. */
export function rowRects(block: Element): DOMRect[];               // ol.chain > li[data-filter-id]
/** The gap under client y and the drop line's top relative to the block. Empty chain: gap 0 at the top of li.empty. */
export function gapAt(block: Element, y: number): { gap: number; lineTop: number } | null;
/** Pure: the line's top for gap k, from the row rects and the list's padded top and bottom. */
export function gapLine(rows: readonly { top: number; bottom: number }[], k: number, listTop: number, listBottom: number, blockTop: number): number;
```

Hooks: the navigator's filter block carries `data-drop="chain"`; the list is `ol.chain`; each row is
`li.nav-step[data-filter-id]`; the empty state is `li.empty`. `useChainDrag.measure` (`useChainDrag.ts:124-140`),
`insertDragSession.gapAt` (`insertDragSession.ts:50-64`) and `useFileDrop.chainGap` (`useFileDrop.ts:41-58`) call
`chainGeometry`. The drag's lifted copy takes its left and width from the row (`useChainDrag.ts:232` reads `.card` today).
`findScroller` (`useChainDrag.ts:96-102`) then finds `.nav-scroll`, so auto-scroll scrolls the navigator.

#### 1.5.2 Focus after a move

The inspector root carries `data-inspects="<filter id>"`, not `data-filter-id`, so `cardOf`
(`useChainDrag.ts:61`) only finds the navigator row. A move started from the inspector does not move the inspector's
DOM, so its focus survives; `restoreFocus` (`useChainDrag.ts:74-86`) only acts when the focus was in a navigator row.

#### 1.5.3 File drops

Unchanged routing (`fileDrop.ts:97-114`). Targets after the change:

1. A path field in the inspector (`PathField.tsx:41-52`), as today.
2. **New:** a navigator row whose filter has exactly one `path` param of a fitting kind (Apply LUT, Image Mask/Blend):
   the row registers itself with `registerField` (`fileDrop.ts:165-170`) and carries `data-drop="path"`. A drop sets
   that filter's path. This keeps today's "drop on the second Image Mask card" behaviour (UX-PASS W2 check 3) now that
   only one filter's fields are on screen.
3. The navigator's filter block: insert at the gap.
4. Anywhere else: append, or import a preset.

### 1.6 Narrow windows: the icon rail

Below 1100 px window width the navigator becomes a 52 px rail. The user can also collapse it at any width with a
chevron button in the navigator's top-right corner; that choice is saved (`ui.navRail`).

Rail contents, top to bottom:

| Item | Look |
|---|---|
| Expand (») | 36×32 button, opens the flyout |
| Camera, Camera controls, Output | 36×36 icon buttons; tooltip is the name and summary; the status dot sits on the icon's top-right |
| Hairline | |
| One 36×28 chip per filter | the step number (`01`), `--accent` when enabled, `--faint` when bypassed; dot or spinner at the top-right; tooltip is the label |
| + | Add menu |

Selected rail items have the accent bar and fill of a selected row. A pointer drag on a chip reorders it (the same
hook and `chainGeometry`). The "+" gaps and the bypass switch are not shown in the rail; bypass is in the inspector
header and on Space (1.10).

**Flyout.** » opens the full navigator (width 232) as a frosted overlay (`--pop`, `--blur`, `styles.css:8-9`) over the
left edge of the centre, under the title bar, full body height, `z-index` above the preview. It closes on Escape, on
a click outside, and after a selection. Focus moves into it on open and back to » on close. The preview behind stays
live; the flyout covers its left part only while open.

The inspector never overlays. At 900 px the rail leaves 506 px for the centre, so the preview is 474×267: larger than
today's stacked band at that size (about 320 px wide, UX-PASS R3/W3).

### 1.7 Inspector

The inspector shows the selected item only: one section or one filter. It is a column with a sticky header and a
scrolling body (`.insp-scroll`). The body scrolls only when that one item is taller than the inspector.

**Header for a filter** (frosted when content scrolls under it):

```
⠿  Chroma Key   EXTENSION 1.2  BYPASSED                     On ━●   ⋯
   Step 3 of 4
   [◐ Before]  [⊕ Pick from preview]                             [⟲ Reset]
   ┃ Extension “x” is switched off in Settings → Extensions. …         ← status lines, full width
```

| Element | Behaviour | Today |
|---|---|---|
| Grip (`button.grip`) | Pointer drag reorders against the navigator list; Space lifts, arrows move the drop line in the navigator, Space drops, Escape cancels | `FilterCard.tsx:90-103` |
| Title | `h2`, 15 px, 600, ellipsis; tooltip with the full name | `FilterCard.tsx:104-109` (the fold button goes) |
| Badges | `Extension 1.2`, `Bypassed` | `FilterCard.tsx:110-115` |
| Step | "Step 3 of 4", 11 px `--dim` | `FilterCard.tsx:85-87` |
| On switch | Bypass toggle, label "On" | `FilterCard.tsx:123-130` |
| ⋯ | Move up (Alt+↑), Move down (Alt+↓), Remove (Delete) | up/down/remove buttons, `FilterCard.tsx:117-133` |
| Before (hold) | **New.** Press and hold: the preview shows the picture entering this filter (`setPreviewPoint({ filter: id })`); release: back to the output. Same hold rules as Compare (`Preview.tsx:356-390`, `previewFrame.ts` `holdStep`). Disabled with the `toolBlock` reason as tooltip (`previewTool.ts:25-30`) or while a tool runs | the store API exists (`store.ts:277-283`), no button |
| Pick from preview / Edit on picture | Only for `toolOf(kind)` filters; same labels, hint line and Esc behaviour | `FilterCard.tsx:164-190` |
| Reset | **New.** Every param back to its default in one change, toast "Reset Chroma Key" with Undo. Uses `checkpoint`, `setParams`, `sayUndo` (`store.ts:293-298`, `store.ts:1209-1211`). Disabled when all params are at their default | per-slider reset only (`ParamField.tsx:123-137`) |
| Status lines | NVIDIA needs + "Install it now", extension note, filter error (codes stripped, raw in tooltip), "Loading model…" | `FilterCard.tsx:135-163` |

**Header for a section:**

| Section | Title line | Summary line | Actions |
|---|---|---|---|
| Camera | Camera | the navigator summary | Refresh (icon + "Refresh"; `SourcePanel.tsx:95-98`) |
| Camera controls | Camera controls | the navigator summary | Reset all (`ControlsCard.tsx:90-98`) |
| Output | Output | the navigator summary | none |

**Better Cam auto hints and warnings** stay in the Camera controls inspector under the row they belong to:
the status line, the stripes note, "whole stops", "Auto Adjust finishes…", the low-light and after-sensor hints, the
slow-camera hint with "Use Better Cam auto" (`AutoControls.tsx:135-289`). In addition the Camera controls navigator
row shows the loop's tone as its dot (1.5), so a limit or a hand-back is visible while another item is selected. The
footer's slow-camera text stays (`Stats.tsx:33`, `Stats.tsx:48`).

### 1.8 Selection, add, remove

Selection is one string, saved in `ui.selected`: `"camera" | "controls" | "output" | "filter:<id>"`. Filter ids are
saved with the chain (`store.ts:75`), so a selected filter survives a restart. A saved selection that names no
filter falls back to `"camera"`.

| Event | Selection | Focus |
|---|---|---|
| Click or arrow onto a row | that item | stays on the row |
| Add filter (Add menu, a "+" gap, insert-drag, quick add, file drop that adds) | the new filter | the new navigator row (replaces `focusCardAt`, `AddFilter.tsx:10-20`) |
| "Add Auto Adjust" links inside Camera controls (`AutoControls.tsx:193`, `:281`) | unchanged (`addFilter(…, { select: false })`); the toast says where it went | unchanged |
| Remove the selected filter | `nextChain[min(i, len − 1)]`, i.e. the next filter, else the previous one, else `"output"` (the row above FILTERS) | that row |
| Remove another filter | unchanged | unchanged |
| Reorder | unchanged (follows the id) | as 1.5.2 |
| Undo, preset load, extension refresh: any `setChain` that drops the selected id | the same rule, with i = the old index | unchanged |
| Engine state, camera change | unchanged | unchanged |

The rule is one pure function, `selectionAfter(prevChain, nextChain, selected)`, in `src/ui/layout.ts`, called from
`setChain` (`store.ts:414-450`).

### 1.9 Splitters

Two splitters, both the existing component generalised (`Splitter.tsx`): `data-side="nav"` on the navigator's right
edge and `data-side="inspector"` on the inspector's left edge. 8 px hit zone on the 1 px hairline, `col-resize`, accent
on hover, drag and focus (`styles.css:243-247`).

| | Navigator splitter | Inspector splitter |
|---|---|---|
| Drag | sets `ui.navWidth` in px, clamped 184–320 and to the room left | sets `ui.inspectorWidth`, clamped 300–560 and to the room left; dragging left widens |
| Saved | on `pointerup`, as today (`Splitter.tsx:53-61`) | same |
| Double-click, Enter | back to auto (`null`) | same |
| Keys | ArrowLeft/Right ±16 px, Shift ±64 px, Home/End to the limits | same, mirrored |
| ARIA | `role="separator"`, `aria-orientation="vertical"`, `aria-label="Resize navigator"`, `aria-valuenow` in px | `aria-label="Resize inspector"` |
| Rail mode | hidden | shown |

A drag writes CSS variables on `.workspace` (`--nav`, `--insp`) and renders nothing in React, as today
(`Splitter.tsx:5-9`). The preview's ResizeObserver debounce stays (`Preview.tsx:163-166`).

### 1.10 Keyboard

The navigator is a `nav` landmark (`aria-label="Sections and filters"`) with two lists. Each row's main element is a
button; the selected one has `aria-current="true"`. Roving tab index: one row is in the tab order (the selected one);
the row's grip, switch and ⋯ have `tabindex="-1"` (pointer only), because every one of them has a key or an inspector
equivalent. They are siblings of the row button, not inside it.

| Key | Where | Does |
|---|---|---|
| ↑ / ↓ | navigator row | focus and select the previous / next row, across the hairline between sections and filters |
| Home / End | navigator row | first / last row |
| Enter, F6 | navigator row | focus the inspector's first control |
| Escape, Shift+F6 | inside the inspector (no popover open, no tool running) | focus the selected navigator row |
| Space | filter row | toggle bypass (selection already follows focus, so Space is free) |
| Delete | filter row | remove, toast with Undo, focus to the new selection |
| Alt+↑ / Alt+↓ | filter row or anywhere in its inspector | move one step (`onCardKeyDown`, `useChainDrag.ts:373-383`) |
| Space on the inspector grip | inspector header | keyboard lift; ↑/↓/Home/End move the line in the navigator; Space drops; Escape or Tab cancels (`useChainDrag.ts:328-365`) |
| Ctrl+S | anywhere | save the active preset, or Save as (`PresetMenu.tsx:51-60`) |
| Ctrl+Z | not in a text field | the toast's Undo (`Toast.tsx:29-39`) |

The reorder live region (`App.tsx:141-143`) stays and also announces "Color Correction selected, step 1 of 3" on a
selection change from the keyboard.

### 1.11 Persistence

`UiState` (`layout.ts:16-24`) becomes:

```ts
export interface UiState {
  /** Navigator width in px; null = auto (clamp(200, 17vw, 260)). */
  navWidth: number | null;
  /** Inspector width in px; null = auto (clamp(340, 25vw, 460)). */
  inspectorWidth: number | null;
  /** The user collapsed the navigator to the rail (below 1100 px it is the rail anyway). */
  navRail: boolean;
  /** "camera" | "controls" | "output" | "filter:<id>". */
  selected: string;
  /** The Advanced group of the Camera inspector is open. */
  advanced: boolean;
}
export const DEFAULT_UI: UiState = { navWidth: null, inspectorWidth: null, navRail: false, selected: "camera", advanced: false };
```

`normalizeUi` drops the old `split`, `camera`, `controls` and `output` keys and keeps `advanced`. A width outside its
drag range is `null`. `Saved.collapsed` and the actions `toggleCollapsed` and `setAllCollapsed` go
(`store.ts:76-77`, `store.ts:315-316`, `store.ts:1262-1269`); `loadSaved` ignores an old `collapsed` key.

The Camera controls inspector reads the camera when it is shown and the engine is settled: the existing effect with
`open` replaced by "mounted" (`ControlsCard.tsx:46-55`).

### 1.12 Title bar

```
● Better Cam  LIVE   PRESET Studio light* ▾  [Save]                ‖ Pause  ■ Stop  Settings   –  □  ×
```

The preset trigger is `width: clamp(150px, 22vw, 260px)` (today a fixed 260 px, `styles.css:344`). At 900 px the bar
holds brand ~150, preset 150, Save 56, Pause 80, Stop 74, Settings 78, window buttons 138 and gaps: about 790 px. Both
are buttons, so they are not part of the drag region. The dirty mark and the menu are section 3.8.

---

## 2. Inspector content density

### 2.1 Rules

1. No card inside the inspector. `.card`, `.panel`, `.params` and `.ctl-rows` boxes, their borders and padding are not
   used there (`styles.css:215`, `styles.css:395-397`, `styles.css:418-423`, `styles.css:655`). Hierarchy comes from
   group headings and indentation.
2. One column of **property rows**. Every row uses the same four columns, so labels, controls, values and buttons
   line up from row to row (CSS subgrid; WebView2 is evergreen Chromium):

   ```css
   .props { display: grid; grid-template-columns: [lbl] var(--lbl) [ctl] minmax(0, 1fr) [val] auto [act] auto;
            column-gap: 8px; row-gap: 4px; container-type: inline-size; }
   .prop  { grid-column: 1 / -1; display: grid; grid-template-columns: subgrid; align-items: center; min-height: 28px; }
   ```

3. Two densities, by the inspector body's width (a container query on `.insp-scroll`, not the window):

   | Body width | `--lbl` | Slider row | Other rows |
   |---|---|---|---|
   | ≥ 400 px (inspector ≥ 444) | `clamp(96px, 34%, 160px)` | one line: label · slider · value · actions, 28 px | one line |
   | < 400 px | `40%` | two lines: label · value · actions, then the slider across the full width; 44 px | one line |

4. Controls inside the inspector are 28 px high (32 px elsewhere, `styles.css:162-163`). Switches stay 30×16 with the
   24 px hit area (`styles.css:410-416`).
5. The value column is right-aligned tabular figures, `min-width: 44px`. The actions column holds up to two 24 px
   icon buttons; it is empty (0 px) on rows without actions.
6. Group headings: the `.label` style (`styles.css:216`), 14 px above, 4 px below, no line.
7. Hints: 11 px `--faint`, in `ctl / -1` under the control. Status and error lines span the full width.
8. Padding of the body: 12 px top, 16 px sides, 20 px bottom, plus the 12 px scrollbar gutter that `.scroll` keeps
   (`styles.css:47`). No other padding inside.
9. The only nested line is the 1 px indent rule of a Better Cam auto block (`.ctl-sub`, `styles.css:677`), kept as an
   indent guide with 12 px left padding.

### 2.2 Slider row

```
≥ 400 px:  Contrast        ━━━━━━━━━━━●━━━━━━━━━━━━━━━   0.20  ⟲
< 400 px:  Contrast                                     0.20  ⟲
           ━━━━━━━━━━━━━━━━━━━━━━━●━━━━━━━━━━━━━━━━━━━━━━━━━━━━
```

The value text and the reset button keep their behaviour: double-click and Delete reset, the reset button appears only
when the value differs and gives focus back to the slider (`ParamField.tsx:105-157`, `ControlsCard.tsx:149-163`).
The `<label htmlFor>` rule stays (`ParamField.tsx:117-118`).

### 2.3 How each component renders

| Component | Rows in the inspector |
|---|---|
| `ParamField` slider | slider row (2.2) |
| `ParamField` select, combo, text, int, color | label · control spans `ctl`, `val` and `act` |
| `ParamField` bool | label · switch at the end of `ctl` (one line; the label wraps beside it, `styles.css:424-427`) |
| `ParamField` path | label on its own line, then the path field across all columns |
| `FilterDef.layout = "crop"` | the four-edge frame grid (`styles.css:450-467`) only at ≥ 400 px; below that, plain rows |
| `ControlsCard` slider control | label · slider · value · actions (Auto toggle, reset). The Auto switch becomes a 24 px toggle button "A" (`aria-pressed`, label "Brightness auto", tooltip "Let the camera set this") in the actions column, beside reset |
| `ControlsCard` control with a mode list (Exposure, White balance) | line 1: label · mode `Select` across `ctl`…`act`; line 2: indented slider · value · reset |
| `ControlsCard` switch control, Powerline frequency | bool row, select row |
| Gain driven by exposure | slider row, disabled, hint "Set by Better Cam auto exposure" |
| `ExposureAuto`, `WhiteBalanceAuto` | an indented block (2.1 rule 9) of rows: Target brightness (slider), Longest exposure (select), Most gain (slider), Response (select), Warmth (slider with the "cooler / warmer" ends as the slider's tooltip ends); then the status line and hints full width. `.auto-field` (`styles.css:678-686`) is replaced by `.prop` |
| `SourcePanel` | Device (select; Refresh is in the header), Resolution/FPS type, Resolution, FPS, Video format; the "Using the device default" notice full width; an "Advanced" group heading with a chevron (`ui.advanced`), then Color space, Color range, Buffering, and the four switches |
| `OutputPanel` | Resolution, FPS, Aspect, Background (swatch), "Also offer 720p and 360p to apps" (bool) |

Camera controls are grouped for display only (the store order stays `CONTROL_KEYS`, `schema.ts:513-516`):

| Group | Keys |
|---|---|
| Exposure and colour | exposure, gain, iris, white_balance, backlight_compensation, low_light_compensation, powerline_frequency |
| Picture | brightness, contrast, gamma, saturation, hue, sharpness, color_enable |
| Lens | focus, zoom, pan, tilt, roll |

A group with no control the camera reports is not shown.

Expected heights at 900×600 (body 507 px, inspector header about 100 px for a filter): Color Correction (6 sliders,
2 colours) is 6 × 44 + 2 × 28 + gaps ≈ 350 px, so it fits; Camera with Advanced closed is about 230 px; Camera
controls on a typical webcam (12–15 controls) scrolls, which is the one place where scrolling is expected.

---

## 3. Presets

### 3.1 What a preset holds

Everything the user can set, except app-level settings.

| Group | Fields | Store source | Engine call on load |
|---|---|---|---|
| Filter chain | every entry's `kind`, `enabled`, `params` (no ids) | `chain` | `set_chain` (`commands.rs:157-160`) |
| Camera device | `{ id, name }` of the selected camera, or `null` | `source.video_device_id` + `cameras` | `set_source` (`commands.rs:69-77`), part of the capture call |
| Capture | `res_type`, `resolution`, `frame_interval`, `video_format`, `color_space`, `color_range`, `buffering`, `flip_vertically`, `autorotation`, `hw_decode`, `deactivate_when_not_showing` | `source` (`schema.ts:19-33`) | `set_source`; a change to a capture key reopens the camera (`schema.ts:292-295`, `config.rs:152-175`) |
| Camera controls | the selected camera's changed controls: `{ value, auto, mode? }` per key | `controls[cam].values` (`schema.ts:687-693`) | `set_camera_control`, `set_control_mode` (`commands.rs:90-125`); the record also goes with `set_source` |
| Better Cam auto | `exposure { target, longest, max_gain, speed }`, `white_balance { warmth, speed }` | `controls[cam].auto` (`schema.ts:649-664`) | `set_auto_config` (`commands.rs:127-136`) |
| Output | `resolution`, `fps`, `fit`, `background_color`, `extra_sizes` | `output` (`schema.ts:37-46`) | `set_output` (`commands.rs:148-151`) |

Not in a preset: autostart, close to tray, the NVIDIA prompt flag, extensions (enabled state and folder), the
preview switch, pause, run state, window and UI state (widths, selection, Advanced), and what Better Cam auto learned
about a camera (`learned`, `schema.ts:675-684`): that is a measurement of the camera, not a choice.

`deactivate_when_not_showing` ("Turn off camera when no app is using Better Cam") is included. It is shown in the
Camera section with the other capture settings, and the rule "everything in Camera, Camera controls, Output and the
chain" is easier to predict than a list of exceptions.

### 3.2 File format, version 2

```jsonc
{
  "app": "better-cam",
  "version": 2,
  "chain": [ { "kind": "color_filter", "enabled": true, "params": { "saturation": 0.2 } } ],
  "camera": {
    "device": { "id": "\\\\?\\usb#vid_046d&pid_085e&mi_00#…#{e5323777-…}\\global", "name": "Logitech BRIO" },
    "capture": {
      "res_type": "custom", "resolution": "1920x1080", "frame_interval": 333333, "video_format": "mjpeg",
      "color_space": "default", "color_range": "default", "buffering": "auto",
      "flip_vertically": false, "autorotation": true, "hw_decode": true, "deactivate_when_not_showing": true
    },
    "controls": {
      "brightness": { "value": 140, "auto": false },
      "focus": { "value": 30, "auto": false },
      "exposure": { "value": -6, "auto": false, "mode": "app" },
      "gain": { "value": 40, "auto": false, "mode": "app" }
    },
    "auto": {
      "exposure": { "target": 0.5, "longest": "fit", "max_gain": null, "speed": "normal" },
      "white_balance": { "warmth": 0.1, "speed": "slow" }
    }
  },
  "output": { "resolution": "source", "fps": 30, "fit": "fit", "background_color": 4278190080, "extra_sizes": true }
}
```

- `chain` stays at the top level with the same entry shape, so an older Better Cam that reads only `chain`
  (`chain.ts:153-157`, `commands.rs:361-370`) still loads the filters of a version 2 file.
- `camera.controls` lists only controls that differ from the device's default state, as the store does
  (`schema.ts:727-737`). A control that is not listed means "the device's default state": Auto on where the camera
  has Auto, else its default value.
- `camera.auto` is always written (the defaults too), so a load sets Better Cam auto's settings even when the file's
  camera had none stored.
- `camera.device` is `null` when no camera was selected.
- On read, a `camera` that is not an object with a `capture` object is ignored (`camera: null`), and an `output` that is
  not an object is ignored (`output: null`). Inside a valid section, each bad field falls back to its default.
- `overlays` (`chain.ts:136-147`) is unchanged and still not kept by Rust; out of scope.

TypeScript, in new `src/presets/preset.ts`:

```ts
export interface PresetCamera {
  device: { id: string; name: string } | null;
  capture: Omit<SourceCfg, "video_device_id">;
  controls: Partial<Record<ControlKey, StoredControl>>;
  auto: AutoCfg;
}
/** What a preset sets, as the store holds it (ids kept: this is also the Undo snapshot). */
export interface PresetState { chain: FilterCfg[]; camera: PresetCamera; output: OutputCfg }
export interface PresetFileV2 { app: "better-cam"; version: 2; chain: PresetEntry[]; camera: PresetCamera; output: OutputCfg }
/** A file read from disk: v1 files have no camera and no output. */
export interface ParsedPreset { version: number; chain: FilterCfg[]; camera: PresetCamera | null; output: OutputCfg | null }

export function presetFileOf(state: PresetState): PresetFileV2;
/** Bare array, {chain}, v1 object or v2 object. Throws "This is not a Better Cam preset". Every section normalised. */
export function parsePreset(raw: unknown, extensions: readonly ExtensionInfo[]): ParsedPreset;
/** Fingerprint for "unsaved changes" (3.6). */
export function presetSignature(state: PresetState): string;
/**
 * The defaults for a camera. (Changed in 0.1.2: the text this line had, "empty chain, DEFAULT_SOURCE, no controls,
 * DEFAULT_AUTO, DEFAULT_OUTPUT", is the old behaviour. Now: the default chain (one Auto Adjust, white balance only), the
 * factory capture at the camera's largest 16:9 size, Better Cam auto white balance with no fixed value, the factory
 * Better Cam auto settings, the factory output: see `FACTORY_*` and `defaultState`.)
 */
export function defaultState(cam: CameraInfo | null): PresetState;
/** The camera a preset names, among the connected ones (findCamera, then the same vid/pid and name). */
export function presetCamera(cams: CameraInfo[], device: PresetCamera["device"]): CameraInfo | null;
/** The writes that bring the camera from `infos` to `target` (3.4). */
export function planControls(infos: ControlInfo[], target: PresetCamera["controls"]): ControlPlan;
```

Normalising reuses what exists: `normalizeChain` (`chain.ts:95-108`), `normalizeSource` (`schema.ts:474-491`),
`normalizeOutput` (`schema.ts:493-507`), `normalizeAutoCfg` (`schema.ts:802-822`), and the per-control checks of
`normalizeControls` (`schema.ts:851-874`), which drop `mode: "app"` outside exposure, white balance and gain, and gain
`app` without exposure `app`. `vidPid` and `linkKey` (`schema.ts:237-238`) are exported for `presetCamera` and the
signature.

### 3.3 Apply order

Loading a preset, "Reset to defaults" and Undo all run one store action, `applyState(target, how)`, where `target`
is a `ParsedPreset` (a section that is `null` is left as it is). The steps run in this order:

| # | Step | Why here | Calls |
|---|---|---|---|
| 1 | Take the Undo snapshot: the current `PresetState` (the selected camera's record) and `activePreset`, `presetBaseline` | before anything changes | none |
| 2 | Chain: `setChain(target.chain)` | instant, no device I/O; the picture changes at once | `set_chain` |
| 3 | Output: `output = target.output`, one push | instant | `set_output` while running (`store.ts:480-482`) |
| 4 | Camera record in the store: `controls[targetCam]` = the preset's controls **with `app` entries turned into manual at the same value**, and `auto` = the preset's | so step 5 sends a record whose values are the seeds of step 9; an `app` flag in the record would make step 9 a no-op (C12) | none |
| 5 | Device and capture in **one** source change: `video_device_id` = the preset's camera when connected; the capture fields; then `repairSource` against that camera (`schema.ts:262-286`); then `api.setSource(runningSource(), record())` **always** (also when the source did not change and while stopped: it sets the backend record, C11) | one `set_source`, so at most one reopen | `set_source` |
| 6 | Wait until the engine is settled (`live` or `stopped`, as `ControlsCard.tsx:44`), at most 10 s, then read the controls of the target camera (`refreshControls`, `store.ts:967-981`) | the writes need the camera's ranges and current state | `get_camera_controls` |
| 7 | Better Cam auto settings: `api.setAutoConfig(cam.id, target.auto)` and await it | the loops start with the right target in step 9 | `set_auto_config` |
| 8 | Controls, phases A and B of `planControls`: leave Better Cam auto where the target is not `app`; then every plain control in `CONTROL_KEYS` order | Auto flags before values, as `apply_on_open` does (`camera_controls.rs:658-662`) | `set_control_mode`, `set_camera_control` |
| 9 | Controls, phase C: engage Better Cam auto, exposure first, then white balance | last, as `apply_on_open` does (`camera_controls.rs:663-672`) | `set_control_mode(app)` |
| 10 | Set `activePreset` and `presetBaseline = presetSignature(state now)`; persist; toast with Undo and notes (3.4) | after the repair, so a repaired value is not "unsaved" | none |

While steps 2–9 run, the preset trigger shows a 10 px spinner and the name it is loading. A second load or reset while
one runs waits for it to end, and only the latest waiting one runs. A user edit of a control during step 8 or 9 is not
overwritten: the pumps already let the newest value win (`store.ts:587-591`), and the plan skips a key whose pump is
busy.

If step 6 times out (the camera is in use by another app, reconnecting, or opening slowly), steps 7–9 are kept as
`pendingControls = { camId, target }` (not saved) and run the next time the engine is `live` with that camera. Any
control edit by the user, another load or a camera change clears it. The toast then says "Camera controls apply when
the camera opens."

### 3.4 `planControls` and the missing cases

For each control the camera reports, the wanted state is `target[key]`, else the default state
(`{ value: def, auto: can_auto }`, `schema.ts:617-622`).

```ts
export type ControlStep =
  | { key: ControlKey; op: "mode"; mode: ControlMode }
  | { key: ControlKey; op: "set"; value: number; auto: boolean };
export interface ControlPlan { a: ControlStep[]; b: ControlStep[]; c: ControlStep[]; skipped: { key: ControlKey; why: string }[] }
```

| Phase | Rule |
|---|---|
| A | exposure and white balance whose current mode is `app` and whose wanted mode is not: `mode` step to `camera` (wanted `auto`) or `manual` |
| B | every other control in `CONTROL_KEYS` order, skipping keys whose wanted mode is `app` and the gain when the exposure's is: wanted Auto and the camera has Auto and it is off → `set { auto: true }`; wanted manual and (Auto is on or the value differs) → `set { value: clamp(value, min, max), auto: false }` |
| C | exposure, then white balance, whose wanted mode is `app` and current is not: `mode` step to `app` |

Missing and unsupported cases:

| Case | What happens | Toast note |
|---|---|---|
| The preset's camera is connected | it is selected; everything applies | — |
| The preset's camera is not connected | the current camera stays; capture fields apply and are repaired to it; controls and Most gain do **not** apply (device units of another model); the other Better Cam auto settings apply | "“Logitech BRIO” is not connected: camera controls were not applied." |
| The preset has no camera (`device: null`) | as above, without the note | — |
| No camera connected at all | chain, output and capture fields are stored; nothing is written to a device | "No camera connected: camera settings apply when one is." |
| A control key the camera does not report | skipped | "N camera controls are not on this camera." |
| A value out of the camera's range | clamped (`store.ts:988`) | — |
| A manual value on a control without a manual mode (`can_manual` false) | skipped | counted in the line above |
| `app` wanted but Better Cam auto is not offered (`info.app == null`, `schema.ts:570-573`; blocked, `schema.ts:682-683`) | Camera auto if the camera has Auto, else Manual at the stored value | "Better Cam auto is not available for Exposure on this camera." |
| `set_control_mode` or `set_camera_control` fails | that key is skipped, the controls are read again (as today, `store.ts:576-580`) | the error, once |
| A capture setting the camera does not offer | `repairSource`; a format the camera cannot open falls back as today (`format-fallback`, `store.ts:829-835`) | today's toast |
| An unknown filter kind, an extension that is not installed | `normalizeChain` drops unknown kinds and keeps `ext:` entries (`chain.ts:89-108`); the row shows the extension note | — |
| A v1 file (no camera, no output) | only the chain changes | "“Old preset” has filters only (saved by an older Better Cam)." |
| A file with `version` above 2 | the known sections are read | "This preset was saved by a newer Better Cam; some settings may be missing." |

The device id in a file is only compared with the cameras `list_cameras` returned (`presetCamera`). A preset never
makes the app open a device that is not in that list.

### 3.5 Undo

`Snapshot` (`store.ts:95-99`) gains `full?: PresetState`. A preset load, a reset to defaults and an import store
`{ chain, activePreset, presetBaseline, full }`; every other chain change keeps today's chain-only snapshot.
`undoChain` (`store.ts:1219-1224`) runs `applyState(full)` when `full` is there and puts `activePreset` and
`presetBaseline` back after it; else it does what it does today. The toast's Undo and Ctrl+Z (`Toast.tsx:29-39`)
need no change. Undo of a load therefore switches the camera back, restores its controls and Better Cam auto
settings, the output and the chain. A load whose result has the same signature as before offers no Undo, as today
(`store.ts:1322`).

### 3.6 Dirty mark and the "Default" label

`presetSignature(state)` is JSON of, in a fixed key order:

- the chain's existing fingerprint (`chain.ts:13-22`, ids ignored);
- the device as `vidPid(id) ?? linkKey(id)` plus the lower-case name (so a camera on another USB port is the same);
- the capture fields, without `resolution` and `frame_interval` when `res_type` is `preferred` (they are unused then,
  `config.rs:118-121`, and `repairSource` may change them);
- the controls sorted by key, **without the `value` of `app` controls** (the resume point moves while Better Cam auto
  runs, `withResume`, `schema.ts:768-782`, and must not make the preset look changed);
- the Better Cam auto settings after `normalizeAutoCfg`;
- the output after `normalizeOutput`.

The selector caches by the identity of `chain`, `source`, `output`, `controls` and `cameras`, like `signatureOf` does
today for the chain (`PresetMenu.tsx:8-17`).

| State | Trigger label | Mark |
|---|---|---|
| Active preset, signature = baseline | `Studio light` | none |
| Active preset, signature ≠ baseline | `Studio light*` | the `*` in `--warn`; `aria-label="Studio light, modified"`; tooltip "Modified. Ctrl+S saves." The "Modified" tag (`PresetMenu.tsx:87`) goes |
| No active preset, state = `defaultState(cam)` ignoring the device | `Default` | none |
| No active preset, anything else | `Unsaved` | none |

**Migration.** `presetBaseline` values saved by today's build are chain fingerprints. New baselines start with
`"v2:"`. At `init` (`store.ts:905-917`): a baseline without the prefix that equals `signature(chain)` becomes
`presetSignature(state)` (clean); any other old baseline becomes `null` (shown as modified, which it is). The matching
of a restored state against saved presets compares full signatures for v2 files and chain fingerprints for v1 files.

### 3.7 Save, Save as, overwrite, rename, delete

| Action | Behaviour |
|---|---|
| Save (button, Ctrl+S) | writes `presetFileOf(current state)` under the active name; baseline = signature now. With no active preset it opens Save as. Disabled when clean |
| Save as… | the name field of today (`PresetMenu.tsx:131-150`): `PRESET_NAME` rule, 40 characters, a clash says "“X” already exists. Saving replaces it." and the button says Replace |
| Load | click a row; `applyState`; toast "Loaded “X”" with Undo |
| Rename | **new**: a pencil icon beside the trash on row hover and focus; the row turns into the name field; Enter renames, Escape cancels; a clash with another preset is refused with the same hint. The active preset's name follows. Calls new `rename_preset` |
| Delete | as today, with the inline confirm (`PresetMenu.tsx:95-110`) |
| Reset to defaults | replaces "Reset to default chain" (`PresetMenu.tsx:151-159`, `store.ts:1354-1358`): `applyState(defaultState(cam))` without changing the camera; toast "Reset to defaults" with Undo |
| Import by drop | unchanged flow (`store.ts:1327-1340`, `fileDrop.ts:135-149`), through `parsePreset`; the file is saved as it was (a v1 file stays v1, filters only) and applied with `applyState` |

A line under the list says what a preset holds: "Presets save the filters, the camera and its controls, Better Cam
auto and the output."

### 3.8 Rust changes

Presets stay opaque JSON in Rust: the frontend normalises every section. Changes in `src-tauri/src/commands.rs`:

1. `preset_object(chain, camera, output)` (`commands.rs:339-342`): `version` is 2 when `camera` or `output` is
   present, else 1.
2. `preset_to_write` (`commands.rs:344-357`): keeps `camera` and `output` when they are JSON objects; still drops
   `id` from chain entries and every other top-level key.
3. `preset_from_file` (`commands.rs:359-370`): passes `camera` and `output` through when they are objects; a bare
   array is still wrapped as version 1.
4. Atomic save: write `<name>.json.tmp`, then `std::fs::rename` over `<name>.json` (it replaces on Windows).
   `list_presets` (`commands.rs:327-337`) keeps only files whose extension is `json`, so a leftover `.tmp` is never
   listed.
5. New `rename_preset(from, to)`: both names through `preset_path` (`commands.rs:317-325`); refuse when `from` is
   missing ("There is no preset named …") or when `to` exists and is not `from` in another letter case ("A preset named
   … already exists"); then `std::fs::rename`. Registered in `main.rs` next to `save_preset` (`main.rs:266-267`).
6. Tests: the two tests that pin "a camera section is dropped" (`commands.rs:461-488`) change to "camera and output
   are kept, ids and other keys are not"; new tests for the version rule, `rename_in(dir, from, to)` (refusals,
   case-only rename) and the `.tmp` filter. The pure parts take a directory, like `read_preset_at`
   (`commands.rs:395-408`), so they test without an `AppHandle`.

The 1 MiB cap and the BOM handling (`commands.rs:389-408`) stay. A v2 preset is a few KiB.

### 3.9 API changes (`src/api.ts`)

| Function | Change |
|---|---|
| `savePreset(name, file: PresetFileV2)` | sends the file as given (today it builds a v1 file from the chain, `api.ts:183-184`) |
| `loadPreset(name)` | returns the raw value; the store runs `parsePreset` (today `presetChain`, `api.ts:185-190`) |
| `readPresetFile(path)` | unchanged |
| `renamePreset(from, to)` | new, `invoke("rename_preset", { from, to })` |

P1 adds the new shapes as `savePresetFile` and `loadPresetRaw` next to the old ones, so the store keeps compiling;
P2 switches the store over and deletes the old two.

---

## 4. Build plan

### 4.1 Order and overlap

```
wave 1:  L1 ───────────────┐        P1 ─────────┐
wave 2:  L2 (after L1) ────┤        P2 (after L1 and P1)
wave 3:  L3 (after L1, L2, P2)
```

| Pair | Shared files | Parallel? |
|---|---|---|
| L1 ∥ P1 | `package.json` only: L1 edits the `test:ui` line, P1 adds a `test:preset` line after `test:ext`. One unchanged line lies between, so git merges them | yes |
| L2 ∥ P2 | `styles.css` only: P2 edits only the preset block (`.preset-trigger`, `.pt-*`, `.pm-*`, `styles.css:123-141` and `:344-348`); L2 does not touch it. L2 needs no store change (it uses `checkpoint`, `setParams`, `sayUndo`, `setPreviewPoint`) | yes |
| P2 after L1 | `store.ts` (L1: `ui`, selection, `collapsed`; P2: presets, snapshot) and `PresetMenu.tsx` (L1 moves it into the title bar) | no, sequential |
| L3 after L2, P2 | `Navigator.tsx`, `styles.css`, `PresetMenu.tsx` (narrow title bar) | no |

Every brief ends with `npm run build`, `npm run test:schema`, `npm run test:ui` and `npm run test:ext` passing, and
`cargo test` for P1. CDP checks follow `docs/UX-PASS.md` section 3 ("How checks run"): debug build,
`BETTER_CAM_TEST_WINDOW=x,y,w,h`, `Runtime.evaluate`, `Input.dispatchMouseEvent`/`dispatchKeyEvent`,
`Page.captureScreenshot`, and `Emulation.setFocusEmulationEnabled({enabled: true})` before any focus check.

Selectors every brief may rely on after L1:

| Selector | Meaning |
|---|---|
| `.workspace` with `data-nav-mode="full"\|"rail"` | the three regions |
| `[data-region="nav"]`, `[data-region="center"]`, `[data-region="inspector"]` | the regions |
| `[data-nav="camera"\|"controls"\|"output"]` | section rows (their main button) |
| `ol.chain > li.nav-step[data-filter-id]`, `.nav-label` | filter rows and their labels |
| `[aria-current="true"]` | the selected row |
| `.inspector[data-shows="camera"\|"controls"\|"output"\|"filter:<id>"]` | what the inspector shows |
| `.splitter[data-side="nav"\|"inspector"]` | splitters |
| `[data-add-filter]` | the Add filter row button |

Seed a chain: click `[data-add-filter]`, then the `.pm-item` whose `b` text is the label (`AddFilter.tsx:65-71`).
Read the order:

```js
[...document.querySelectorAll('ol.chain > li.nav-step .nav-label')].map(e => e.textContent.trim())
```

---

### L1. Shell and navigator

**Goal.** Three docked regions with the preview always in view, the chain as compact rows in the navigator, one
selected item in the inspector, and every drag and drop working against the new rows. The inspector still renders
today's components (the filter's card, the three panels) without their fold headers; L2 restyles them.

**Files.** `src/App.tsx`, `src/styles.css`, `src/store.ts` (`ui`, selection, `addFilter` option, `setChain` selection
rule, remove `collapsed`), `src/ui/layout.ts` + `layout.test.ts` (widths, `normalizeUi`, `selectionAfter`), new
`src/ui/chainGeometry.ts` + `chainGeometry.test.ts`, `src/ui/useChainDrag.ts`, `src/ui/insertDragSession.ts`,
`src/ui/useFileDrop.ts`, `src/components/Splitter.tsx`, new `src/components/Navigator.tsx` (section rows, filter
rows, empty state, Add filter row), new `src/components/Inspector.tsx` (host: picks the component for the selection),
`src/components/FilterCard.tsx` (no fold, `data-inspects`, no `li` wrapper), `src/components/AddFilter.tsx` (anchor
in the navigator, focus the new row), `src/components/EmptyChain.tsx` (compact, in the navigator),
`src/components/TitleBar.tsx` (hosts `PresetMenu`), `src/components/SourcePanel.tsx`, `ControlsCard.tsx`,
`OutputPanel.tsx` (drop the fold header; read controls while mounted), `src/components/Icon.tsx` (camera, sliders,
output, more icons), `package.json` (`test:ui` gains `chainGeometry.test.ts`).

**Behaviour.**

- Sections 1.1, 1.5 (without the status dots and summaries' dots, which are L3), 1.5.1, 1.5.2, 1.5.3, 1.7 (host only),
  1.8, 1.9, 1.11, 1.12. The navigator is always full in L1 (the rail is L3); at 900 px the widths clamp as in 1.1.
- Remove the toolbar, Collapse all, `collapsed`, `.preview-pin`, the `--band` observer (`App.tsx:51-60`), the stacked
  layout (`styles.css:565-581`) and the stale 1100 px query.
- Navigator rows: select on click, bypass switch, ⋯ menu (Move up, Move down, Remove). Arrow-key navigation is L3;
  rows are plain buttons in the tab order for now.
- Section summaries as text (1.5 table) without dots.
- Move the preset trigger and Save into the title bar unchanged in behaviour.

**Check (CDP).**

1. 1200×800: `.toolbar` does not exist; `.titlebar .preset-trigger` exists. The three `[data-region]` rects are side
   by side; `[data-region=nav]` width 204 ± 1, `[data-region=inspector]` width 340 ± 1.
2. `.panel.preview` rect: aspect 16:9 ± 1 px, fully inside `[data-region=center]`. Select Camera controls, set the
   inspector's scroll element `scrollTop = 1e6`: the preview rect is unchanged.
3. Seed Color Correction, Crop/Pad, Sharpen, Transform. The order reads `[Color Correction, Crop/Pad, Sharpen,
   Transform]`; the Transform row has `aria-current="true"` and `.inspector[data-shows]` is `filter:<its id>`.
4. Click the Crop/Pad row, open its ⋯, Remove: the Sharpen row is selected (the next one). Click the Transform row
   and remove it via ⋯: Sharpen is selected (the previous one). Remove every filter: `[data-nav=output]` is selected; `li.empty` exists
   in the navigator with 3 or 4 quick-add buttons.
5. Reorder (UX-PASS W1 checks 2–4 on rows): press on row 1's grip area, move in 10 px steps to 6 px above row 4's
   top, `.drop-line` sits between rows 3 and 4, release: `[Crop, Sharpen, CC, Transform]`, toast "Moved Color
   Correction to step 3", Undo restores. Escape mid-drag changes nothing.
6. Keyboard lift from the inspector: select row 2, focus `.inspector .grip`, send Space, ArrowDown, ArrowDown, Space:
   the filter is step 4; `document.activeElement` is still the inspector's grip.
7. Alt+ArrowUp with focus on a range input in the inspector: the filter moves up one step; focus stays on that input.
8. Auto-scroll: seed 24 filters at 1200×600 so the navigator scrolls; `.nav-scroll.scrollTop = 1e6`; press on the
   last row, hold the pointer 10 px below the top of `.nav-scroll` for 2 s: `scrollTop` reaches 0; release: that
   filter is first.
9. Insert drag (L5): open `[data-add-filter]`, press on the Sharpen item, move to the gap between rows 1 and 2,
   release: Sharpen is step 2 and selected.
10. File drops (`__betterCam.drop`): a `.cube` at the gap between rows 1 and 2 inserts Apply LUT there; a `.jpg` on
    the centre of the second of two Image Mask rows sets only that filter's `image_path`; a `.jpg` on the inspector's
    `.pf-text` sets the selected filter's path; a preset `.json` on the preview imports it.
11. Splitters: drag `.splitter[data-side=inspector]` 80 px left: the inspector is 420 ± 1. `Page.reload`: 420 ± 1, and
    the selection is the same row. Double-click: back to 340 ± 1. Focus the nav splitter, ArrowRight ×2: nav +32.
12. 900×600 (full navigator until L3): navigator 200 ± 1, inspector 338 ± 1, centre ≥ 360 px, preview ≥ 320 px wide.
13. Eyedropper and crop: select a Chroma Key, click "Pick from preview" while live: `.pick-overlay` exists. Select a
    Crop/Pad, "Edit on picture": `.crop-overlay` exists; Escape ends it.
14. Old saved state: put `{"collapsed": {"x": true}, "ui": {"split": 0.6, "camera": false}}` into
    `localStorage["better-cam"]`, reload: no error, `[data-nav=camera]` selected, auto widths.
15. `npm run test:ui` (new cases: `gapLine` for gap 0, n and between; `selectionAfter` for remove first, middle, last,
    only, and a replaced chain; `normalizeUi` for old and junk values; width clamps at 900, 1100, 1200, 1920) and
    `npm run build` pass.
16. Screenshots at 900×600, 1200×800, 1920×1080 attached to the report.

---

### P1. Preset format, apply plan and Rust

**Goal.** The version 2 file, its parser and signature, the control plan, and a Rust side that keeps the new sections,
renames and saves atomically. No visible change yet.

**Files.** New `src/presets/preset.ts`, new `src/presets/preset.test.ts`, `src/camera/schema.ts` (export `vidPid`,
`linkKey`), `src/api.ts` (add `savePresetFile`, `loadPresetRaw`, `renamePreset`), `package.json` (`test:preset`
after `test:ext`), `src-tauri/src/commands.rs`, `src-tauri/src/main.rs` (register `rename_preset`).

**Behaviour.** Sections 3.1, 3.2, 3.4 (the pure `planControls`), 3.6 (`presetSignature`, `defaultState`), 3.8, 3.9.

**Check.**

1. `npm run test:preset` covers:
   - `parsePreset` on a bare array, a v1 object, a v1 object with an unknown `camera` key from an old test
     (`{gain: 3}`, as in `commands.rs:484`; it is not a valid camera section and parses as `camera: null`), a full v2
     file, a v2 file with junk in every field (each falls back to its default), `"text"` (throws), a version 3 file.
   - `presetFileOf` → `parsePreset` round trip equals the input state apart from fresh ids.
   - `presetSignature`: ids ignored; an `app` control's value ignored; a `manual` value change counted; the same
     camera on another port (another link, same vid/pid and name) equal; `resolution` ignored under `preferred`.
   - `planControls` on fixture `ControlInfo` lists: default target resets a changed Brightness and turns Focus Auto
     back on; exposure `app` → `manual` goes in phase A; exposure wanted `app` puts no exposure or gain step in phase
     B and one `mode app` step in phase C; white balance after exposure in C; a missing key and a `can_manual: false`
     key land in `skipped`; values are clamped.
   - `presetCamera`: exact link, another port, a missing camera.
2. `cargo test -p better-cam commands` (or the crate's test filter for these tests) passes: camera and output kept on
   save and load, ids dropped, the version rule, rename refusals and a case-only rename, `.tmp` files not listed.
3. `npm run build` passes; the app still saves and loads presets as before (the store is unchanged).

---

### L2. Inspector content

**Goal.** The inspector reads like a property panel: the header of 1.7, the rows of section 2, no nested cards.

**Files.** `src/components/Inspector.tsx`, new `src/components/InspectorHeader.tsx`, `src/components/FilterCard.tsx`
(becomes `FilterInspector.tsx`: header + rows), `src/components/ParamField.tsx`, `src/components/ControlsCard.tsx`,
`src/components/AutoControls.tsx`, `src/components/SourcePanel.tsx`, `src/components/OutputPanel.tsx`,
`src/components/Field.tsx`, `src/components/ToggleRow.tsx`, `src/styles.css` (inspector, props, ctl and auto blocks;
remove the dead `.chain .card`, `.params`, `.ctl-rows` rules).

**Behaviour.** Sections 1.7 (headers, Before, Reset, sticky header) and 2 (all rules and the component table). The
Auto switch of a camera control becomes the "A" toggle. Camera controls get the three display groups.

**Check (CDP).**

1. Inspector at 340 px (default at 1200×800), Color Correction selected: every `.prop` that holds a range input is
   44 ± 4 px high, and its `input[type=range]` top is below its label's bottom. Drag the inspector splitter to 480:
   the same rows are 28 ± 2 px, and label, range and value share a top ± 4 px.
2. No nested boxes: inside `.inspector`, no element other than inputs, buttons, `.select`, `.swatch`, `.pf-text` and
   the status lines has a computed `border-top-width` > 0 (the `.ctl-sub` rule is a `border-left`).
3. Value columns line up: the `left` of every `.prop .pf-val, .prop b` in Color Correction is equal ± 1.
4. Before: select the second filter while live; press and hold `.insp-before`: `.preview-caption` text names that
   filter; release: the caption is gone.
5. Reset: change Contrast, click `.insp-reset`: Contrast is back to 0.00, the toast says "Reset Color Correction",
   Undo restores the value.
6. Crop/Pad: at inspector 340 its params are plain rows (no `.layout-crop` grid active); at 480 the four-edge grid
   shows.
7. Camera controls with the dev hook (`__betterCam.set({ controlInfos: [...fixture], controlsFor: "<cam id>" })` and
   `__betterCam.autoStatus(...)`): the group headings "Exposure and colour", "Picture", "Lens" show only for groups
   with controls; the exposure row in `app` mode shows the indented block with Target brightness, Longest exposure,
   Most gain, Response and the status line; the "A" toggle has `aria-pressed` matching `auto`.
8. Camera inspector: Advanced closed, the body is not scrollable at 1200×800 (`scrollHeight <= clientHeight`).
   Output: not scrollable at 900×600.
9. Re-run L1 checks 6, 7 and 13 (keyboard lift, Alt+arrows, tools).
10. `npm run build` and the test scripts pass. Screenshots of Color Correction, Camera, Camera controls (Better Cam
    auto on) and Output at inspector 340 and 480.

---

### P2. Preset store and menu

**Goal.** Presets save and reapply everything, with Undo, a correct dirty mark, rename and reset to defaults.

**Files.** `src/store.ts` (`applyState`, snapshot `full`, `undoChain`, `savePreset`, `loadPreset`, `importPreset`,
`renamePreset`, `resetAll` replacing `resetChain`, `pendingControls`, init migration of the baseline, the cached
signature selector), `src/components/PresetMenu.tsx`, `src/api.ts` (switch to the v2 calls, remove the old two),
`src/filters/chain.ts` (remove `presetFile` and `presetChain` once unused; keep `PRESET_NAME`, `signature`),
`src/filters/ext.test.ts` (move its preset cases to `parsePreset`), `src/styles.css` (preset block only),
`README.md` (Presets line, `README.md:26`).

**Behaviour.** Sections 3.3, 3.4 (wiring), 3.5, 3.6, 3.7.

**Check (CDP, on a machine with a webcam; the dev box has one).**

1. Baseline: Start. Set Output resolution `1280x720`; Camera → Advanced → Flip vertically on; Camera controls →
   Brightness to a value other than the default; Exposure mode Better Cam auto and Target brightness 0.60; add Sharpen.
   Save as "T1": the trigger reads `T1`, no `*`.
2. Change all five things back. The trigger reads `T1*` with `aria-label` "T1, modified".
3. Load "T1": within 5 s the Output Resolution select shows `1280x720`, Flip vertically is on, the Brightness row's
   value text equals the saved value, the Exposure mode select shows "Better Cam auto", Target brightness reads 0.60,
   the chain is `[Sharpen]`, and the trigger reads `T1` without `*`.
4. Undo (Ctrl+Z): all five are back to the state of step 2.
5. Reload the page: the trigger still reads `T1` (clean or modified as before the reload).
6. Old preset: `__betterCam.drop([fixtureV1Path], x, y)` with a bare-array file: the chain is the file's, the Output
   and Camera values are unchanged, the toast contains "filters only".
7. Missing camera: drop a v2 fixture whose `camera.device.id` names `vid_ffff&pid_ffff`: the toast contains "is not
   connected"; the Output values from the file apply; the Brightness value does not change.
8. Rename "T1" to "T2" from the menu: the list shows T2, the trigger reads `T2`; renaming another preset to "t2" is
   refused with the hint.
9. Reset to defaults: the chain is empty, Output is "Same as camera", the controls show Defaults, the camera is the
   same; the trigger reads `Default`; Undo restores.
10. Better Cam auto resume points: with T1 loaded and live for 30 s (the loop moves the exposure), the trigger has no
    `*`.
11. `npm run test:preset`, `test:ext`, `test:ui`, `test:schema`, `npm run build` pass.

---

### L3. Narrow windows, keyboard and status

**Goal.** The preview stays large in small windows, the navigator works from the keyboard, and its rows show state
at a glance.

**Files.** `src/components/Navigator.tsx`, new `src/components/NavRail.tsx` (rail and flyout), `src/App.tsx`
(rail mode), `src/styles.css`, `src/ui/layout.ts` (+ tests: the 1100 px rule), `src/components/PresetMenu.tsx`
(trigger width), `src/components/TitleBar.tsx` (bar at 900 px), `README.md` (`README.md:27-28`, the layout lines).

**Behaviour.** Sections 1.5 (status dots, summaries), 1.6, 1.10, and the narrow title bar of 1.12.

**Check (CDP).**

1. 1099×700: `.workspace[data-nav-mode=rail]`; `[data-region=nav]` width 52 ± 1; the nav splitter's computed `display`
   is `none`. 1100×700: `full`.
2. 900×600: the centre is ≥ 500 px wide, the preview ≥ 470 px wide; the title bar's buttons do not overlap
   (`getBoundingClientRect` of `.preset-trigger`, Save, Pause, Stop, Settings and `.wc` are disjoint).
3. Flyout: click the rail's »: a `.nav-flyout` exists and has focus inside; click a filter in it: the flyout is gone,
   that filter is selected; » again, then Escape: gone, focus on ».
4. Rail reorder: press on chip 1, move to below chip 3, release: the order changes, toast with Undo.
5. Collapse toggle at 1400 px: click it: rail; reload: still rail; click » then the expand toggle: full again.
6. Keyboard at 1200×800: focus the Camera row; ArrowDown ×3: the first filter row is selected and focused. Space: that
   filter's switch is off and its row is bypassed. Delete: the filter is gone, the toast offers Undo, focus is on the
   newly selected row. Enter: focus is inside `.inspector`. Escape: focus back on the row.
7. Status dots: `__betterCam.set({ filterErrors: { [id]: "could not load x.cube" } })`: that row has a `.nav-dot` with
   `data-tone="error"` and the message as `title`. `__betterCam.set({ filterStatus: { [id]: "Loading model…" } })`:
   a `.spinner` in the row. `__betterCam.autoStatus({ id: camId, exposure: { state: "limit_gain", … } })` with exposure in
   `app` mode: the Camera controls row has a warn dot.
8. Summaries: with a Custom 1920×1080 30 fps MJPEG source, the Camera row's summary contains "1920×1080 · 30 fps".
9. Re-run L1 checks 5, 9, 10 and P2 check 3 at 1000×700 (rail mode).
10. Screenshots at 900×600, 1099×700, 1200×800, 1920×1080, plus the flyout open at 900×600.

---

## 5. Risks

| Risk | Mitigation |
|---|---|
| A preset load writes many controls; the first write to exposure, white balance or focus takes up to 700 ms (`camera_controls.rs:23-24`) | `planControls` writes only what differs; the trigger shows a spinner; the user can keep working |
| Better Cam auto engaged from a preset seeds at the stored resume point, not where the camera is | the same seed `apply_on_open` uses at every start; the loop moves from there within seconds |
| A preset loaded while another app holds the camera cannot write controls | `pendingControls` applies them at the next `live`; the toast says so |
| Subgrid or container queries behave differently in an old WebView2 | WebView2 is evergreen; L2 check 1 measures the result, not the CSS |
| Removing the cards loses "see two filters at once" | the navigator shows the whole chain with state; Before and Compare show the effect; the owner chose the Inspector layout |
| Selection-follows-focus in the navigator re-renders the inspector on each arrow press | the inspector mounts one item; `FilterCard` already reads only its own filter (`FilterCard.tsx:12-15`) |

## 6. Open questions

None. The decisions taken here that the owner may want to revisit after use: Better Cam auto stays inside Camera
controls (1.5); `deactivate_when_not_showing` is part of a preset (3.1); controls of another camera model are not
applied (3.4); the inspector never overlays the preview and the navigator becomes a rail below 1100 px (1.6).
