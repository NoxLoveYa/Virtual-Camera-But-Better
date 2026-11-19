# Better Cam UX pass: drag, drop, resize and first-use rough edges

Status: plan, not built. Written against `main` at `b121ea7`. No application code was changed for this document.

## Recommendation

Build a first wave of five coding sessions, all in the frontend apart from one small Rust command:

| Wave 1 | What the user gets | Effort |
|---|---|---|
| **W1** Reorder by pointer | A card that follows the pointer, a drop line, auto-scroll, Escape to cancel, keyboard lift-and-move, Undo | M |
| **W2** Drop files from Explorer | Drop a LUT or picture on a card, on the chain or on the window; drop a preset `.json` to import it | M |
| **W3** Layout | The preview stays in view while settings scroll; a column splitter with a remembered ratio; fold states remembered | M |
| **W4** Feedback and states | The toast moves into the footer and never covers a control; plain error text without hex codes; a preview with Start and Refresh buttons; an empty-chain state; banners across the full width | M |
| **W5** Controls and accessibility | Auto sliders that can be moved; a visible reset button; consistent switch placement; focus ring, hit sizes, contrast, Settings as a dialog | M |

W2 must come after W1: OS file drops need `dragDropEnabled: true`, and that switches off in-page HTML5 drag and drop on Windows (section 1.1, fact T1). W1 replaces the HTML5 reorder with pointer events. The other items can go in any order.

Do direct manipulation on the preview next, as wave 2. Its one foundation is an engine change: a **preview point**. The preview then shows the picture as it enters a chosen filter, not only the final output. Without it a crop handle or an eyedropper reads the wrong pixels (section 1.2). Wave 2: L1 preview point (M to L, Rust), L2 eyedropper (M), L3 crop, zoom and pan on the picture (M to L), L4 hold to compare (S).

Effort scale: S = less than half a coding session, M = about one session, L = two or more.

---

## 1. Findings

Each finding points at a screenshot in `C:\Users\Obnoxious\AppData\Local\Temp\opencode\` or at a file and line.

### 1.1 Drag and drop

**Facts checked against the Tauri 2 documentation** (fetched 2026-11-18):

- **T1 (verified).** `dragDropEnabled` (window config, default `true`) "Whether the drag and drop handlers used internally to generate DragDropEvents are enabled on the webview … Disabling it is required to use HTML5 drag and drop on the frontend on Windows since we replace the drag drop handler of WebView2." Source: https://v2.tauri.app/reference/config/ (WindowConfig › dragDropEnabled). So with `true`, the page's HTML5 `dragstart/dragover/drop` reorder stops working on Windows. With `false` (today), Tauri delivers no OS file drops.
- **T2 (verified).** With the handler on, `getCurrentWebview().onDragDropEvent(cb)` delivers `enter {paths, position}`, `over {position}`, `drop {paths, position}` and `leave`. Positions are physical pixels relative to the webview's top-left corner. The docs warn that the position "may be inaccurate" while the debugger panel is open. Source: https://v2.tauri.app/reference/javascript/api/namespacewebview/ (onDragDropEvent, DragDropEvent).
- **T3 (verified by reading the config).** The window is frameless (`decorations: false`, `src-tauri/tauri.conf.json:23`). Nothing in the docs ties drop events to window decorations; the handler belongs to the webview. The title bar's `data-tauri-drag-region` (`TitleBar.tsx:156`) is a window move region and is not involved in file drops.
- **T4 (unverified).** Whether `onDragDropEvent` needs a permission beyond `core:default` (`src-tauri/capabilities/default.json:7`). It is an event listener, and `core:default` includes the event permissions, so it should work. W2 checks it first.
- **T5 (unverified, possibly a live bug).** Today, with `dragDropEnabled: false`, WebView2 handles an OS drop itself. A browser that gets a file dropped on a page with no `drop` handler usually opens the file in place of the page. If WebView2 does that here, dropping a picture on Better Cam replaces the whole UI with the picture. Test by hand: drag a `.png` from Explorer onto the preview. W2 removes the risk either way, because Tauri's handler takes the drop.

| # | Finding | Evidence |
|---|---|---|
| D1 | Reorder uses HTML5 drag and drop. Only the 20×28 px grip is draggable. The drag image is the browser's ghost of that grip, not the card. | `FilterCard.tsx:43`, `styles.css:373` |
| D2 | No drop indicator and no "dragging" style. `dragover` only calls `preventDefault`. The user cannot see where the card will land. | `FilterCard.tsx:37` |
| D3 | The drop target is a whole card: dropping on card *i* calls `moveFilter(from, i)`. Dropping on the terminals ("Camera in", "Better Cam out") or on the empty part of the column does nothing. There is no "after the last card" target except the last card itself. | `App.tsx:75-79`, `App.tsx:68`, `App.tsx:82` |
| D4 | `dragFrom` is cleared only on drop. A drag cancelled with Escape or dropped outside leaves it set. A later drop of anything else (a dragged text selection) on a card then moves the stale filter. | `App.tsx:24`, `App.tsx:76-79` |
| D5 | No app-controlled auto-scroll. Moving a card from step 1 to step 6 in a long, expanded chain depends on Chromium's built-in drag auto-scroll near the edge of `.col-right`. How well that works in WebView2 is unverified. | `App.tsx:56`, `styles.css:47` |
| D6 | The keyboard alternative exists (Move up and Move down buttons) but is one step per press. Whether focus survives the move is unverified: React moves the card's DOM node, and Chromium can drop focus from a moved node. At step 1 the "Move up" button becomes disabled under the focus, and focus falls to `<body>`. Nothing announces the new position to a screen reader. | `FilterCard.tsx:54-59` |
| D7 | A move has no Undo, unlike add, remove, preset load and reset. A slip of the mouse can only be fixed by dragging back. | `store.ts:905-909` versus `store.ts:894-904` |
| D8 | Touch and pen: HTML5 drag and drop does not start from touch in Chromium on Windows by default (unverified for WebView2). The reorder is mouse-only in practice. | `FilterCard.tsx:43` |
| D9 | A filter can only be appended at the end of the chain. The Add menu has no position, and nothing can be dragged out of it. | `AddFilter.tsx:28-34`, `store.ts:894-898` |
| D10 | Files reach the app only through the native dialog (`pick_file`). Dropping a `.cube` from Explorer does nothing (or worse, see T5). | `PathField.tsx:34-47`, `tauri.conf.json:21` |
| D11 | Presets live as `<name>.json` in the app data folder (`%APPDATA%\dev.bettercam.app\presets`). There is no import, so a preset from another machine or from the owner's site has to be copied there by hand. | `commands.rs:268-276` |

### 1.2 Direct manipulation on the preview

What the preview shows today. The engine runs the chain, then the output stage (resolution and Fit), then hands the **fitted output** to the preview tap (`engine.rs:3122-3134`). The tap shrinks it on the GPU to the box size, at most 960×540 (`preview.rs:37`; `Preview.tsx:126-136`). The canvas has the shrunk picture's own size and is letterboxed into a 16:9 box with `object-fit: contain` (`Preview.tsx:75-79`, `styles.css:539-540`). The 16-byte frame header carries only the shrunk size (`preview.rs:17-20`). The frontend knows the full output size only from the 4 Hz stats (`Stats.tsx:22`).

So a pointer position on the preview maps cleanly to the **final output**, but not to the input of a filter in the middle of the chain. Mapping back would mean inverting the output Fit (which needs the chain's output size, not sent to the UI) and every later geometry filter (crop, scale, rotation).

| # | Finding | Evidence |
|---|---|---|
| M1 | Crop is four number fields around a static diagram. The diagram does not show the values or the picture. Finding the right left/top/right/bottom values takes many −/+ clicks while watching a small preview. | `p10-crop.png`; `schema.ts:104-117`; `styles.css:407-426` |
| M2 | Keys take a colour through a hex box, twelve presets or the system picker. `p12-chroma.png` shows a custom key `#857f7a` that removes almost the whole picture: the user is guessing. An eyedropper on the **output** would sample the already-keyed picture (magenta background in that screenshot), so it would give the wrong colour as soon as the key works. | `p12-chroma.png`; `schema.ts:194`, `schema.ts:221`; `ColorField.tsx:117-124` |
| M3 | Transform has only Flip H, Flip V and Rotation. It has **no pan or zoom parameters**, so "drag to pan, wheel to zoom" has nothing to write in Transform. Zoom and pan can be expressed exactly by Crop/Pad (a smaller window = zoom; moving it = pan); the output Fit then scales the result. | `schema.ts:154-162`; `p10-transform.png` |
| M4 | There is no way to compare with and without the filters except switching filters off one by one. | `FilterCard.tsx:60-67` |
| M5 | When several filters of one kind exist (two Crop/Pad cards), nothing on the preview could say which one a gesture edits. Any gesture needs an explicit "edit this filter" entry point on the card. | `App.tsx:69-81` |

### 1.3 Resize and layout

| # | Finding | Evidence |
|---|---|---|
| R1 | The column ratio is fixed at 6:7 with a 440 px minimum on the left. There is no splitter. With a short chain the right column is mostly empty while the left column scrolls (Output is below the fold at 1200×800). | `styles.css:223`; `p7-live.png`, `p6b-live.png`, `p15-disconnected.png` |
| R2 | The preview scrolls away with the left column. Tuning a camera control means the preview is gone (`p8-card.png`: Brightness, White balance and Exposure are visible, the preview is not). | `App.tsx:49-54`; `p8-card.png` |
| R3 | Below 1100 px the columns stack in one scroll: preview, camera, controls, output, then the chain (`styles.css:504-509`). Editing a filter means the preview is a full scroll away. 1100 px is wide: a window snapped to half of a 1920 px screen (960 px) is in this layout. | `styles.css:504-509`; `p6b-narrow.png` |
| R4 | The preview is about 478×269 CSS px at 1200×800 (`p7-live.png`), next to a column of settings that is two to three screens tall. | `p7-live.png`, `p9b-card.png` |
| R5 | The window minimum is 900×600 (`tauri.conf.json:19-20`). The CSS says the minimum is 720×520 and keeps queries for 860 px and 760 px that never match at 100 % zoom. | `styles.css:511-524` |
| R6 | Fold states: filter cards remember theirs (`store.ts:47-48`). The Camera controls card and the camera "Advanced" fold do not; they reset on every launch. Camera and Output panels cannot fold at all. | `ControlsCard.tsx:17`; `SourcePanel.tsx:35`; `SourcePanel.tsx:76`, `OutputPanel.tsx:24` |
| R7 | Long text holds up: the device name and path fields shorten with an ellipsis and keep the extension (`p11-lut.png`, `p12-mask.png`; `styles.css:391-404`). Long switch captions are inconsistent (see F12). | `p11-lut.png`, `p12-mask.png` |

### 1.4 First use and the other rough edges

| # | Finding | Evidence |
|---|---|---|
| F1 | **First run, nothing selected → picture in other apps.** (1) The window opens; on an RTX machine the NVIDIA install dialog opens first, before the user has seen their camera (`store.ts:685-690`). (2) The "virtual camera isn't installed" banner sits in the right column; press Install and accept UAC. (3) A camera is picked automatically (`store.ts:362-363`). (4) Press **Start** in the title bar. Nothing points at it: the preview only says "Camera stopped" (`Preview.tsx:253`) with no button. (5) In the video app, choose "Better Cam". Better Cam never says this; the footer only says "No app is using Better Cam" (`Stats.tsx:24`). Three actions in the app, one outside it; steps 1, 4 and 5 are unclear. | `VcamBanner.tsx:15-23`; `p10-crop.png` (update banner); `Preview.tsx:251-259`; `Stats.tsx:24` |
| F2 | **Empty chain.** Only "Camera in" and "Better Cam out" with nothing between, and an empty right column. No hint what filters are or how to add one. | `p7-live.png`, `p6b-live.png`; `App.tsx:67-83` |
| F3 | **No camera.** The device box says "No camera found" (`SourcePanel.tsx:83`), the controls card "Select a camera…" (`ControlsCard.tsx:38`), the preview "Camera stopped". Three places, no action. While the list is still loading the box says "Loading…" with no spinner, which can last (`p10-scale.png`, `p10-transform.png`: "Loading…" and "Looking for cameras…"). | `p10-scale.png` |
| F4 | **Camera busy.** Handled well since the owner's change: banner and preview name the program (`inuse-name.png`). No action needed beyond F9. | `inuse-name.png`; `EngineBanner.tsx:26-27` |
| F5 | **Slow actions.** A format change reopens the camera: the preview goes black with "Opening the camera…" (`Preview.tsx:258`, the canvas is hidden by `is-off`, `Preview.tsx:265`) although the last frame is still on the canvas. Model loading has a spinner line (good, `FilterCard.tsx:88-93`). The virtual-camera install shows only "Installing…" on the button. NVIDIA install has a progress bar (good). | `Preview.tsx:258-265`; `NvidiaPrompt.tsx:44-55` |
| F6 | **The toast covers controls.** It floats 12 px above the footer, centred over both columns (`styles.css:491-492`). In `p8-card.png` it covers the Output Background field and the "Also offer 720p" switch. In `p15f-format-in-use.png` it covers **"Try saved format", the button the toast is about**. Same overlap in `p10-crop.png`, `p10-scale.png`, `p10-transform.png`, `p13c-install-it-now.png`. The timer does not pause on hover (`Toast.tsx:11-15`), so Undo can vanish under the pointer. | `p8-card.png`, `p15f-format-in-use.png` |
| F7 | **Reset is hidden.** Sliders reset on double-click or Delete; the only hint is a tooltip on the slider (`ParamField.tsx:106`, `ControlsCard.tsx:143`). Nothing shows which values differ from the default. | `p9b-card.png` |
| F8 | **Auto switches.** With Auto on, the slider is disabled (`ControlsCard.tsx:82`, `:149`): dragging White balance does nothing and the reason is a 10 px "AUTO" caption (`styles.css:560`). The store already turns Auto off when a value is set (`store.ts:777`), so the slider could simply work. | `p8-card.png` (White balance, Exposure, Focus) |
| F9 | **Error text with hex codes.** The preview shows the engine's raw reason: "Camera disconnected (0xC00D3EA2)" while the banner above says it in plain words (`p15-disconnected.png`; `Preview.tsx:256` versus `EngineBanner.tsx:28-29`). A stream error shows "Stream error (0x…)" in both (`EngineBanner.tsx:35`; `reader.rs:139`). Engine errors and filter load errors carry codes too (`gpu/mod.rs:83`, `fx/assets.rs:534`). Filter errors also show full paths (`p13c-install-it-now.png`). | `p15-disconnected.png`, `p13c-install-it-now.png` |
| F10 | **Error banner.** The whole banner is a click-to-dismiss `div`: no button, not reachable by keyboard, no `role="alert"`, easy to dismiss by accident. | `EngineBanner.tsx:61` |
| F11 | **Settings popover.** It renders with the Popover's default `role="menu"` but holds switches and buttons, not menu items. Focus stays on the Settings button when it opens. | `TitleBar.tsx:98-108`, `Popover.tsx:22`; `p14-settings.png` |
| F12 | **Switch placement in cards differs.** "Passthrough Alpha" sits at the right end of its cell (`p11-lut.png`); "Stretch Image (discard image aspect ratio)" wraps and its switch drops to the left on its own line (`p12-mask.png`); "Undistort…" has a special rule and sits mid-row (`p10-scale.png`; `styles.css:428`). | `styles.css:389-390`, `styles.css:428` |
| F13 | **Footer density.** One long line: capture size, format, fps, decode, output size, fps, GPU ms, latency, consumers. The fact a first-time user needs (is an app using Better Cam?) is last. | `Stats.tsx:25-30`; every live screenshot |
| F14 | **Panel headers differ.** Camera and Output use the small uppercase label; Camera controls uses a bold title with a fold chevron; Advanced is a fold inside Camera. Field captions are uppercase in Camera/Output and sentence case in cards. The caption style is Better Mic's and stays (decision); the header and fold behaviour can be made one. | `SourcePanel.tsx:76`, `ControlsCard.tsx:54-59`, `OutputPanel.tsx:24` |
| F15 | **Focus visibility.** The focus ring is 1 px (`styles.css:41`), thin against a busy acrylic background. The grip is a `span` and cannot take focus (`FilterCard.tsx:43`). | `styles.css:41` |
| F16 | **Hit sizes.** Switches are 30×16 px (`styles.css:376`), below the 24 px WCAG 2.2 target height; the grip is 20×28. Card icon buttons are 28×28 (fine). | `styles.css:373`, `styles.css:376` |
| F17 | **Contrast of muted labels.** `--faint` is `#636e7e` (`styles.css:14`). On the window background measured in the screenshots (about `#202226`) that is about 3.1:1, below the 4.5:1 that 11 px text needs. It is used for every uppercase caption, "Camera in", hints and the "AUTO" caption. `--dim` (`#8a94a3`) is about 5.3:1 and passes. (Estimate from screenshot pixels; the acrylic backdrop changes with the desktop behind it.) | `styles.css:14`, `styles.css:213`, `styles.css:338` |
| F18 | **Tab order.** Banner buttons (Install virtual camera, Use device default) come after the whole left column, about 30 tab stops in. | `App.tsx:49-62` |

---

## 2. Proposals

| ID | Change: what the user sees | Files | Effort | Risk | Value |
|---|---|---|---|---|---|
| P1 | **Pointer reorder.** Drag a card by its grip or its empty header area. A compact copy follows the pointer, the card's place stays as a faded slot, and a 2 px accent line shows where it lands. The column scrolls when the pointer nears its edge; Escape cancels; the drop shows "Moved X to step N · Undo". | `App.tsx`, `FilterCard.tsx`, new `src/ui/chainDrag.ts`, `store.ts`, `styles.css` | M | Low | High |
| P2 | **Keyboard reorder.** The grip becomes a button: Space lifts, arrows move the line, Space drops, Escape cancels. Alt+↑/↓ inside a card moves it one step. Focus stays on the control you used; a live region says the new step. | same as P1 | in P1 | Low | Med |
| P3 | **Drop a file on a card.** Dragging a `.cube`/`.png` over an Apply LUT path field, or a picture over an Image Mask path field, highlights it; dropping sets the path. | `tauri.conf.json`, new `src/ui/fileDrop.ts`, `PathField.tsx`, `App.tsx`, `styles.css` | M | Med (T1) | High |
| P4 | **Drop a file on the chain.** A `.cube` adds Apply LUT at the drop line. A picture adds Image Mask/Blend; a `.png` asks "LUT or mask". Toast with Undo. | as P3, `store.ts` | in P3 | Low | High |
| P5 | **Import a preset by dropping its `.json`.** It is saved under its file name (a clash becomes "Name 2") and loaded, with Undo. | as P3, `api.ts`, `src-tauri/src/commands.rs` (`read_preset_file`) | S | Low | Med |
| P6 | **Preview stays in view.** Two columns: the preview is pinned above the left column's scroll. Stacked: the preview is a frosted sticky band at the top of the single scroll. Its height is capped so settings keep room. | `App.tsx`, `styles.css` | S | Low | High |
| P7 | **Column splitter.** Drag the line between the columns; double-click resets; arrow keys work; the ratio is remembered. Hidden when stacked. | new `Splitter.tsx`, `App.tsx`, `store.ts`, `styles.css` | S | Low | High |
| P8 | **Fold states remembered; Camera and Output fold like Camera controls.** | `SourcePanel.tsx`, `ControlsCard.tsx`, `OutputPanel.tsx`, `store.ts`, `styles.css` | S | Low | Med |
| P9 | **Toast in the footer.** It sits on the footer line next to the stats and never covers a control. Hover or focus pauses it; Ctrl+Z triggers its Undo. | `Toast.tsx`, `App.tsx`, `styles.css` | S | Low | High |
| P10 | **One plain problem text.** Preview and banner say the same thing (short and long form); hex codes leave the visible text and stay in the tooltip. | `camera/schema.ts` (+ test), `Preview.tsx`, `EngineBanner.tsx`, `FilterCard.tsx` | S | Low | High |
| P11 | **Preview states with actions.** Stopped: "Camera stopped" + Start. No camera: + Refresh. Virtual camera missing: + Install. Reopening or paused: the last frame stays, dimmed, with a spinner line. | `Preview.tsx`, `styles.css` | S | Low | High |
| P12 | **Empty chain.** A short explanation and four quick-add buttons between the terminals. | new `EmptyChain.tsx`, `App.tsx`, `styles.css` | S | Low | High |
| P13 | **Banners across the full width** under the toolbar, first in tab order. The error banner gets a Dismiss button and `role="alert"`. | `App.tsx`, `EngineBanner.tsx`, `styles.css` | S | Low | Med |
| P14 | **Stats lead with use.** "Not in use · choose “Better Cam” in your video app" or "In use by 1 app", then sizes and fps; GPU and latency stay. | `Stats.tsx` | S | Low | Med |
| P15 | **Auto sliders can be moved.** Moving one turns its Auto off (the store already does this). | `ControlsCard.tsx`, `styles.css` | S | Low | Med |
| P16 | **Visible reset.** A small reset button after a value that differs from its default. | `ParamField.tsx`, `ControlsCard.tsx`, `styles.css` | S | Low | Med |
| P17 | **Switches at the right end of their cell**, captions wrap beside them. | `styles.css` | S | Low | Low |
| P18 | **Accessibility basics.** 2 px focus ring, 24 px switch hit area, `--faint` raised to pass 4.5:1, Settings as a dialog with focus moved in. | `styles.css`, `TitleBar.tsx`, `Popover.tsx` | S | Low | Med |
| P19 | **Stale size CSS fixed** (comment and dead queries). | `styles.css` | S | Low | Low |
| L1 | **Done (c3c5685).** **Preview point (engine).** The preview can show the picture entering a chosen filter, or the camera input, while a gesture runs. The output to apps does not change. | `fx/mod.rs`, `engine.rs`, `preview.rs`, `commands.rs`, `api.ts`, `Preview.tsx` | M–L | Med | High (enables L2–L4) |
| L2 | **Done (commit id: add when committed).** **Eyedropper** for Chroma Key and Color Key: "Pick from preview", click the backdrop, the key colour is set. | `FilterCard.tsx`, `Preview.tsx`, new `src/components/PickOverlay.tsx`, `src/ui/eyedropper.ts`, `src/ui/previewTool.ts` | M | Low | High |
| L3 | **Done (commit id: add when committed).** **Crop, zoom and pan on the picture.** "Edit on picture" on a Crop/Pad card shows the uncropped input with a frame and 8 handles; drag inside to pan, wheel to zoom. Writes the same params. | `FilterCard.tsx`, `Preview.tsx`, new `src/components/CropOverlay.tsx`, `src/ui/cropRect.ts` | M–L | Med | High |
| L4 | **Done (c3c5685).** **Hold to compare.** Press and hold "Compare" on the preview to see the camera input. | `Preview.tsx` | S | Low | Med |
| L5 | **Done (ce49ac6).** **Insert at a position.** Drag a filter out of the Add menu into the chain, or use a "+" in the gap between cards. | `AddFilter.tsx`, `store.ts`, `chainDrag.ts` | M | Low | Med |
| L6 | **Not built (owner's choice).** **Pop-out preview** in its own resizable window. Needs a second webview; the engine has one preview stream, so the main preview pauses while popped out. | Rust + new window | L | Med | Med |
| L7 | **Done (ce49ac6).** NVIDIA install dialog on first need, not at launch (open question Q3). | `store.ts` | S | Low | Med |
| — | Not recommended now: a split before/after view (needs the engine to composite two pictures), a rotation handle (the slider is enough), a global undo stack for every parameter change. | | | | |

### Recommended order

```
W1  P1 + P2                  pointer reorder                   M   (must precede W2)
W2  P3 + P4 + P5             file drops, preset import         M
W3  P6 + P7 + P8 + P19       pinned preview, splitter, folds   M
W4  P9 + P10 + P11 + P12 + P13 + P14   feedback and states     M
W5  P15 + P16 + P17 + P18    controls and accessibility        M
--- first wave ends (5 sessions) ---
wave 2: L1 → L2 → L4 → L3   (L1 first; L2 and L4 are small once L1 exists)
later:  L5, L7 (if Q3 says yes; it is small enough to join W4), L6
```

If only three sessions are available, do W1, W3 and W4: they fix the most visible faults (F6, R2, D2) and keep the engine untouched.

---

## 3. Build briefs for the first wave

### How checks run

The project tests the running app through the Chrome DevTools Protocol: a debug build with `BETTER_CAM_TEST_WINDOW=x,y,w,h` (placed and shown without taking the focus, `src-tauri/src/test_window.rs:1-3`) and the WebView2 remote debugging port. Every check below uses `Runtime.evaluate` (DOM reads), `Input.dispatchMouseEvent` and `Input.dispatchKeyEvent` (these produce pointer and keyboard events in the page), `Page.captureScreenshot`, and canvas reads. Because the window is not focused, call `Emulation.setFocusEmulationEnabled({enabled: true})` before any check that reads focus or `:focus-visible`. Run each check at 1200×800 and at 1000×700 (stacked), unless it says otherwise.

Seed a chain through the UI: click the button whose text is "Add filter", then the `.pm-item` whose `b` text is the filter's label. Read the order with:

```js
[...document.querySelectorAll('.chain > li.step .card-title button')].map(b => b.textContent.trim())
```

Pure logic (insertion index, drop routing, problem text, crop mapping later) goes into small modules with a test file run like `npm run test:schema` (esbuild bundle + node). Add a `test:ui` script of the same shape for `src/ui/*.test.ts`.

---

### W1. Reorder by pointer (P1 + P2), effort M

**Goal.** Reorder that shows where the card lands, scrolls by itself, cancels cleanly, works by keyboard, touch and pen, and can be undone. No HTML5 drag and drop left in the page, so W2 can turn on Tauri's file drops.

**Layout during a drag**

```
 □ CAMERA IN
 01 ┌ Color Correction ───────┐   ← faded slot (opacity .35), keeps its height
    └─────────────────────────┘
 02 ┌ Crop/Pad ───────────────┐
    └─────────────────────────┘
 ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━   ← drop line: 2 px accent, a dot on the rail
 03 ┌ Sharpen ────────────────┐
        ┌ ⠿ Color Correction  ┐    ← lifted copy: header row only, follows pointer Y,
        └─────────────────────┘      x fixed to the card column, shadow
 □ BETTER CAM OUT
```

**Behaviour**

- Start: primary-button `pointerdown` on the grip, or on the header's empty space (`.card-head .fill`), not on its buttons or title. The drag begins after 4 px of movement, so a click is still a click. Call `setPointerCapture` on the element that got the `pointerdown`. Set `touch-action: none` on the grip only, so touch scrolling elsewhere still works.
- While dragging, the source keeps its place as a faded slot. The lifted copy shows only the header row (title, "Bypassed" tag), so a 340 px card does not hide its targets.
- Target: insertion index *k* in 0…n from the pointer Y against the vertical midpoints of the other cards. The line goes in the gap above card *k*, or under the last card for *k* = n. If *k* equals the source index or source index + 1, the line stays at the source slot and dropping does nothing.
- Auto-scroll: the scroll container is the nearest scrolling ancestor at drag start (`.col-right` in two columns, `.cols` when stacked). Within 56 px of its top or bottom edge, scroll by up to 18 px per animation frame, growing linearly towards the edge. Keep scrolling while the pointer is still (a rAF loop), and recompute *k* after each scroll step.
- Drop on `pointerup`: `moveFilter(from, k > from ? k - 1 : k)`. That is one `set_chain` call. Toast: "Moved Color Correction to step 3", with Undo restoring the snapshot taken at drag start.
- Cancel without a change or toast on Escape, `pointercancel`, `lostpointercapture` or window `blur`.
- Keyboard: the grip becomes `<button class="grip">` with `aria-label="Move Color Correction, step 1 of 3"` and a 28×32 hit area. Space or Enter lifts it (`aria-pressed="true"`). Each ArrowUp/ArrowDown press moves the line so the card would land one step earlier or later; Home/End go to the ends. Space or Enter drops, as the mouse does. Escape or Tab cancels. A visually hidden `aria-live="polite"` region says "Lifted Color Correction, step 1 of 3. Up and down arrows move it, Space drops it, Escape cancels.", then "Color Correction moved to step 3."
- Shortcut: Alt+ArrowUp/Alt+ArrowDown with focus anywhere inside a card moves it one step at once, with the same toast. None of the card's controls use Alt+arrows.
- Focus after any move (drag, keyboard, the ↑/↓ buttons, Alt+arrows) stays on the same control of the same card. Find it by `data-filter-id` and a role after the next frame. When "Move up" becomes disabled at step 1, focus goes to that card's "Move down".
- One card only: the grip shows but is `aria-disabled`, and no drag starts.
- Several filters of one kind: identity is the filter id; the toast names the label and step.
- Folded and bypassed cards drag the same way.
- Engine stopped: works the same, because `set_chain` applies while stopped (`api.ts:146-147`).
- `prefers-reduced-motion`: no slide animation for the cards that make room.
- Remove `draggable`, `onDragStart`, `onDragOver`, `onDrop` and `dragFrom` (`FilterCard.tsx:37`, `:43`; `App.tsx:24`, `:75-79`).
- Persistence: as today (`persist()` in `setChain`).

**Store change.** `moveFilter(from, to)` takes the snapshot and shows the toast with Undo. Drag cancel never calls it.

**Files.** `src/App.tsx`, `src/components/FilterCard.tsx`, new `src/ui/chainDrag.ts` (hook plus pure `insertionIndex(midpoints, y)`), new `src/ui/chainDrag.test.ts`, `src/store.ts`, `src/styles.css`, `package.json` (`test:ui`).

**Check (CDP)**

1. Seed four filters: Color Correction, Crop/Pad, Sharpen, Transform. Read the order: `[CC, Crop, Sharpen, Transform]`.
2. Mouse drag: `mousePressed` at the centre of card 1's `.grip`. Send `mouseMoved` in 10 px steps to 8 px above card 4's top edge. Mid-drag, `document.querySelector('.drop-line')` exists. Its rect top is between card 3's bottom and card 4's top. `mouseReleased`. The order is `[Crop, Sharpen, CC, Transform]`. `.footer .toast` text is "Moved Color Correction to step 3". Click its Undo: the order is back to step 1's.
3. No-op: press on card 2's grip, move 30 px, release. The order is unchanged and there is no toast.
4. Escape: press, move 120 px, `keyDown Escape`, release. The order is unchanged, there is no toast, and `.drop-line` is gone.
5. Auto-scroll at 1200×800: seed eight filters, then click "Expand all". Set `.col-right.scrollTop = 1e6`. Press on the last card's grip. Move to 20 px below the top of `.col-right` and send that same `mouseMoved` every 50 ms for 2 s. `.col-right.scrollTop` reaches 0. Release: that filter is now first.
6. Stacked at 1000×700: repeat 2 and 5 with `.cols` as the scroll container.
7. Keyboard: focus card 2's grip with `.focus()`. Send Space, ArrowDown, ArrowDown, Space. Card 2 is now step 4. `document.activeElement.closest('[data-filter-id]')` is the moved card, and `activeElement` is its grip. The live region's text contains "moved to step 4".
8. Alt+ArrowUp with focus on a range input in card 3: the card is step 2, and focus is on that same input.
9. Move up to the top with the ↑ button: `activeElement` is that card's "Move down" button, not `body`.
10. `document.querySelectorAll('[draggable="true"]').length === 0`.
11. `npm run test:ui` passes (`insertionIndex` edge cases: above the first card, below the last, exactly on a midpoint).

**Judge by eye.** The lifted copy reads as "this card"; the drop line is clear on the acrylic background; auto-scroll speed feels controllable; with a touch screen or pen, if one is available, a drag works and page scrolling elsewhere still works.

---

### W2. Drop files from Explorer (P3 + P4 + P5), effort M

**Goal.** Files go where the user drops them: on a path field, on the chain, or anywhere on the window. A preset file can be imported the same way.

**Prerequisite.** W1 is merged: there is no HTML5 drag and drop left. Then remove `"dragDropEnabled": false` from `src-tauri/tauri.conf.json:21` (the default is `true`, fact T1). Side effect, accepted: dragging selected text from one text box to another stops working.

**Behaviour**

- One listener for the app's lifetime: `getCurrentWebview().onDragDropEvent` in new `src/ui/fileDrop.ts`, started from `App`.
- Position: `x / devicePixelRatio`, `y / devicePixelRatio` gives CSS pixels (fact T2), then `document.elementFromPoint`, then `.closest('[data-drop]')`.
- File kind by extension, case-insensitive. LUT: `.cube`, `.png`. Image: `.png .jpg .jpeg .bmp .gif` (the same lists as `file_dialog.rs:45-52`). Preset: `.json`. Anything else, folders included, is "unsupported".
- Several files: use the first supported one, and add to the toast: "Better Cam takes one file at a time."
- Targets, checked in this order:
  1. **Path field** (`data-drop="path"`, `data-kind="image|lut"` on `PathField`). If the file fits the kind, the field shows an accent border while hovered and the caption "Drop to use this file". Dropping calls the field's `onChange(path)`, which is `setParam`. A file of the wrong kind shows no highlight and falls through to the chain rule. With several Image Mask cards, the one under the pointer wins.
  2. **Chain** (`data-drop="chain"` on `.chain-block`). The drop line from W1's `insertionIndex` shows while hovering. `.cube`: insert Apply LUT at the line with `image_path` set. `.jpg/.jpeg/.bmp/.gif`: insert Image Mask/Blend with `image_path` set. `.png`: a small chooser at the drop point, "Use as: Apply LUT · Image mask · Cancel" (keyboard: arrows, Enter, Escape). `.json`: preset import (below). Toast: "Added Apply LUT as step 2", with Undo.
  3. **Anywhere else** (preview, left column, toolbar). `.json`: preset import. `.cube`: append Apply LUT. Images: the same rule as on the chain, appending at the end. Unsupported: toast "Better Cam can't use .txt files. Drop a .cube or .png LUT, a picture, or a preset (.json)."
- **While dragging** (`enter` with a supported file): a 1 px accent outline around the window content and a hint pill above the footer, such as "Drop to add Apply LUT" or "Drop to import preset". `leave` and `drop` clear both.
- **Preset import.** New Rust command `read_preset_file(path) -> Value`. It refuses files over 1 MiB, parses JSON and returns `preset_from_file(...)` (`commands.rs:312-321`). It only reads, and rejects with a sentence on failure. The frontend then:
  - runs `presetChain(value)` (it throws "This is not a Better Cam preset", shown as a toast);
  - names the preset from the file stem, keeping only `PRESET_NAME` characters (`chain.ts:82`), or "Imported" if nothing is left;
  - on a case-insensitive clash, uses "Name 2", "Name 3", and so on;
  - calls `savePreset(name)` with that chain, then `loadPreset(name)`.
  Toast: "Imported “Studio light”", with Undo restoring the chain before the import. The preset file stays.
- While the NVIDIA modal is open: ignore drops and show no highlight.
- Engine stopped: works; the chain applies on the next Start.
- A file that fails to load (a bad `.cube`) shows the card's existing "could not load …" line (`store.ts:288-305`).
- New store API: `addFilter(kind, at?: number, params?: Partial<Params>)`. The default `at` is the end, so today's callers are unchanged.

**Security.** A dropped path is a user choice, like a path from the picker, and goes only into filter params the engine already reads. `read_preset_file` only reads, caps the size, and returns parsed JSON that the frontend normalises (`normalizeChain`, `chain.ts:65-79`). Nothing is executed.

**Files.** `src-tauri/tauri.conf.json`, `src-tauri/src/commands.rs` (+ the command's registration and a unit test next to the preset tests), `src/api.ts`, new `src/ui/fileDrop.ts` (with a pure `routeDrop(paths, target)`) + `src/ui/fileDrop.test.ts`, `src/components/PathField.tsx`, `src/App.tsx`, `src/store.ts`, `src/styles.css`.

**Check.** CDP cannot produce an OS drop, so the routing is a pure function and the checks call it. In debug builds only, expose `window.__betterCam = { drop(paths, cssX, cssY) }`. It runs the same code as the Tauri listener after the coordinate conversion. Fixtures: a `.cube` and a `.png` from the repo's test assets, and a preset `.json` saved by the app.

1. Empty chain: `__betterCam.drop(["…\\warm.cube"], x, y)` at the centre of `.chain-block`. There is one card, "Apply LUT", and its `.pf-text` `title` equals the path.
2. Seed CC and Sharpen. Drop the `.cube` at the gap between them. The order is `[CC, Apply LUT, Sharpen]`. The toast reads "Added Apply LUT as step 2"; Undo removes it.
3. Seed two Image Mask cards. Drop a `.jpg` at the centre of the second card's `.pf-text`. Only the second card's path changes.
4. Drop a `.png` on the chain: the chooser exists. Send `keyDown ArrowDown` then `Enter`: an Image Mask card is added.
5. Drop the preset `.json` on the preview. The toolbar `.pt-name` shows the imported name, the preset menu lists it, and the card titles match the file's chain. Drop it again: the name gets " 2".
6. Drop a `.txt`: the toast text starts with "Better Cam can't use .txt files".
7. Run W1's checks 2 to 9 again: the reorder still works with `dragDropEnabled` on.
8. Rust: `cargo test` covers `read_preset_file` (size cap, bad JSON, bare array, object).

**By hand (required, because only this path uses Tauri's real event).** With DevTools detached (fact T2), drag from Explorer at 100 % and at 150 % display scaling:

- a `.cube` onto the LUT field;
- a `.jpg` between two cards;
- a preset `.json` onto the preview;
- a `.png` onto the window, to see the chooser.

Before W2, also try a plain drop on today's build to settle T5.

**Judge by eye.** Highlight and hint pill are visible but quiet; the chooser sits at the drop point and inside the window.

---

### W3. Layout: preview in view, splitter, remembered folds (P6 + P7 + P8 + P19), effort M

**Goal.** The user can watch the picture while changing any setting, and can choose how wide the preview is.

**Layout**

```
Two columns (≥ 1100 px)
┌ toolbar ─────────────────────────────────────────────────────────┐
├──────────────────────────┬┬──────────────────────────────────────┤
│ ┌ preview (pinned) ────┐ ││ FILTER CHAIN · APPLIED TOP TO BOTTOM │
│ └──────────────────────┘ ││ □ CAMERA IN                          │
│ ┌ scroll ──────────────┐ ││ 01 ┌ Color Correction …             │
│ │ Camera ▾             │ ◀▶ (splitter: 8 px hit zone on the line) │
│ │ Camera controls ▾    │ ││                                      │
│ │ Output ▾             │ ││                                      │
│ └──────────────────────┘ ││                                      │
├ footer ──────────────────┴┴──────────────────────────────────────┤

Stacked (900–1099 px)
┌ toolbar ─────────────────────────────┐
│ ┌ preview band (sticky, frosted) ──┐ │
│ └──────────────────────────────────┘ │
│   Camera / controls / Output / chain │  ← scroll under the band
├ footer ──────────────────────────────┤
```

**Behaviour**

- **Two columns.** `.col-left` becomes a flex column: the preview (`flex: none`) on top, then a new inner `.scroll` holding Camera, Camera controls and Output. The left column itself no longer scrolls. Preview height = min(16:9 of the column width, 50 % of the column height). When the cap applies, the box narrows and centres, keeping 16:9.
- **Stacked.** The preview is wrapped in a band with `position: sticky; top: 0` inside `.cols`. The band uses the frosted style of floating elements (`--pop` + `--blur`, `styles.css:8-9`), so content scrolling under it reads as underneath and no gap shows through. Height cap: 40 % of the body height (at 900×600: about 180 px, so about 320 px wide).
- **Preview resolution.** Any size change reaches the backend through the existing ResizeObserver with its 150 ms debounce (`Preview.tsx:138-141`). During a splitter drag the picture stretches; it is sharp again 150 ms after the pointer stops.
- **Splitter**
  - New `Splitter` between the columns, placed on the existing 1 px `.col-right` border with an 8 px hit zone and `cursor: col-resize`. The line turns accent on hover and while dragging.
  - Drag sets the left width in px, clamped to left ≥ 440 px (today's minimum) and right ≥ 420 px (a card's two parameter columns plus the rail).
  - Saved as a ratio `split` = left / `.cols` width, written on `pointerup` and not on every move. The default is 6/13 ≈ 0.4615, today's look. On window resize, the left width is the ratio times the new width, clamped.
  - Double-click resets to the default.
  - Keyboard: `role="separator"`, `aria-orientation="vertical"`, `aria-label="Resize columns"`, `tabIndex=0`, and `aria-valuemin`/`max`/`now` as percentages. ArrowLeft/Right change it by 2 %, Shift+arrows by 10 %, Home/End go to the limits, Enter resets.
  - `display: none` in the stacked layout. The saved ratio is kept for when the window widens again.
- **Folds remembered.** Camera, Camera controls and Output each get the same fold header as Camera controls: `.card-title` with a chevron, keeping the uppercase caption style inside the panels. "Advanced" keeps its own fold. All four open/closed states go into `Saved` as `ui: { split, camera, controls, output, advanced }`, normalised on load with defaults: everything open except Advanced, which starts closed. "Collapse all" in the toolbar stays for the filter cards only.
- A folded Camera controls card does not read the camera (`ControlsCard.tsx:26` already skips while closed). The remembered state keeps that saving across launches.
- **P19.** Fix the comment at `styles.css:511` to 900×600. Keep the 860/760 px queries only if they help at webview zoom; otherwise delete them.

**Files.** `src/App.tsx`, new `src/components/Splitter.tsx`, `src/components/SourcePanel.tsx`, `src/components/ControlsCard.tsx`, `src/components/OutputPanel.tsx`, `src/store.ts` (`Saved.ui`, `loadSaved`, `persist`, setters), `src/styles.css`.

**Check (CDP)**

1. 1200×800: `t0 = .preview.getBoundingClientRect().top`. Scroll the left inner `.scroll` to 400. The preview's top equals `t0`, and the Camera panel's top has changed.
2. Preview height ≤ 50 % of `.col-left` height, at 1200×800 and at 1200×600.
3. Splitter drag: press at the splitter's centre and move +150 px in 10 px steps. `.col-left` width grew by 150 ± 1. `Page.reload`: the width ratio is the same ± 0.005. Double-click: the ratio is 0.4615 ± 0.01. Drag to the far right: `.col-right` width is 420 ± 1.
4. Keyboard: focus the splitter and send ArrowRight ×3. `aria-valuenow` rose by 6.
5. 300 ms after step 3, `canvas.width === Math.min(960, Math.round(boxWidth × devicePixelRatio))`, read from `.preview-canvas` and `.preview-box` (only while live).
6. Stacked 1000×700: the splitter's computed `display` is `none`. Set `.cols.scrollTop` to its maximum: the preview band's top equals `.cols`' top, and the last chain card is visible below it.
7. Folds: click the Camera controls chevron, then the Output chevron. Reload: both are still folded. Same for Advanced.
8. Screenshots at 900×600, 1099×700, 1100×700, 1200×800, 1920×1080, attached to the session report.

**Judge by eye.** No content shows through or beside the sticky band; the splitter is findable but quiet; nothing jumps when the window crosses 1100 px.

---

### W4. Feedback and states (P9 + P10 + P11 + P12 + P13 + P14), effort M

**Goal.** Messages never hide controls, errors read like sentences, every dead end in the preview offers the next action, and a new user sees what to do.

**Behaviour**

- **P9, toast in the footer**
  - The footer becomes a row: stats (`flex: 1`, one line, ellipsis) and the toast (up to 60 % of the width, text wraps to at most 3 lines, Undo and ×). The footer grows to fit and shrinks back when the toast goes. The columns lose that height; nothing overlaps.
  - Remove the absolute positioning at `styles.css:491-492`. Keep `role="status"`.
  - The timer (today 4 s + 40 ms per character, `Toast.tsx:13`) pauses while the pointer is over the toast or focus is inside it, and restarts in full when it leaves.
  - Ctrl+Z triggers the toast's Undo while one is offered and focus is not in a text input. It works like the existing Ctrl+S listener (`PresetMenu.tsx:40-48`).
- **P10, one problem text**
  - New pure `problemText(kind, reason, apps, appsMaybe): { short, long }` in `src/camera/schema.ts`. `long` is today's banner text (`EngineBanner.tsx:24-37`). `short` is for the preview, for example "Camera disconnected", "Camera blocked by Windows privacy settings", the in-use sentence, or "The camera stopped sending pictures".
  - `stripCodes(s)` removes `\s*\(0x[0-9A-Fa-f]{8}\)`. Visible text never shows a code. The raw `reason` goes into the `title` of the preview placeholder and of the banner.
  - Apply `stripCodes` to the error banner's text and to filter errors (`FilterCard.tsx:84-86`), with the raw text in `title`. Leave paths alone.
  - Unit tests go in `src/camera/schema.test.ts`. The engine keeps its codes; its own tests depend on them (`engine.rs:4612`).
- **P11, preview states**

  | State | Placeholder | Action |
  |---|---|---|
  | stopped, virtual camera ok | Camera stopped | **Start** (primary, calls `toggleRun`) |
  | stopped, virtual camera missing | Install the virtual camera to start | **Install** (calls `runVcam("install")`, disabled while busy) |
  | cameras listed, none found | No camera found | **Refresh** |
  | standby `opening`/`reopening` after a frame was shown | Applying camera settings… (spinner) | — the last frame stays on the canvas at 35 % opacity |
  | paused | Paused: apps see the paused slate | — the last frame stays, dimmed |
  | reconnecting | `problemText().short` | — |

  The placeholder becomes a centred stack: text plus an optional button. The canvas keeps its last frame for the dimmed states; today `is-off` hides it (`Preview.tsx:265`). On a camera or size change the old frame is dropped (`shown` = false) so a stale picture never stays up.
- **P12, empty chain.** Between the terminals, `li.empty`: "No filters yet. Filters change the picture before other apps see it." Then quick-add buttons for Color Correction, Auto Adjust and Crop/Pad, plus NVIDIA Background Blur only when an RTX GPU is present (`gpu` in the store), and "More…" which opens the Add filter menu. After W2, add the line "Or drop a .cube LUT or a picture here."
- **P13, banners.** Move `.banners` out of the right column to a full-width row between the toolbar and `.cols`, in both layouts (`App.tsx:57-62`). The error banner gets a ghost × button (`aria-label="Dismiss"`) and `role="alert"`; clicking its text no longer dismisses it.
- **P14, stats line.** Lead with use, then the rest:
  - live, no app: "Not in use · choose “Better Cam” as the camera in your video app · 1280×720 → 1280×720 · 16.7 fps · GPU 0.1 ms · 34 ms latency";
  - live, in use: "In use by 1 app · 1280×720 MJPEG (HW) → 1280×720 · 16.7 fps · GPU 0.1 ms · 34 ms latency".
  The full detail stays in the tooltip.

**Files.** `src/components/Toast.tsx`, `src/App.tsx`, `src/components/Stats.tsx`, `src/components/Preview.tsx`, `src/components/EngineBanner.tsx`, `src/components/FilterCard.tsx`, `src/camera/schema.ts`, `src/camera/schema.test.ts`, new `src/components/EmptyChain.tsx`, `src/styles.css`.

**Check (CDP)**

1. Toast placement: scroll the left column to the bottom and add a filter from the menu. `.toast.closest('.footer')` is not null. Its rect does not intersect the rect of any `button, input, [role=switch], [role=combobox]` inside `.cols`. Repeat with a long message: press Repair in Settings and decline the UAC prompt (its error toast is long), or use a dev-only `__betterCam.toast(text)`. The toast wraps to at most 3 lines and still intersects nothing.
2. Pause on hover: move the mouse over the toast and wait 10 s; it is still there. Move away; it is gone within 6 s.
3. Ctrl+Z: remove a card, send Ctrl+Z (`modifiers: 2`, `key: "z"`): the card is back.
4. Problem text: `npm run test:schema` covers `problemText` for every kind and `stripCodes`. Live: hold the camera with `capture_probe.exe` (as in `inuse-name.png`). `.preview-ph` text has no `/0x[0-9A-F]{8}/`, and its `title` equals the engine's raw reason. Unplug the camera (by hand): the placeholder reads "Camera disconnected".
5. Stopped: click Stop. `.preview-ph button` text is "Start"; click it. The title bar `.status` text becomes "Live" within 5 s.
6. Reopen: while live, pick another resolution in the Camera panel. Within 300 ms: `.preview-canvas` computed `visibility` is `visible`, `opacity` < 0.5, and `.preview-ph` text starts with "Applying camera settings".
7. Empty chain: remove all cards. `li.empty` exists with 3 or 4 buttons; click "Color Correction". There is one card and `li.empty` is gone.
8. Banners: with the vcam outdated or missing, `.banners`' rect left is about 28 and its right is about the window width minus 40, at both 1200 and 1000 widths. Tab from the toolbar's "Add filter": the next focus is the banner's button.
9. Stats: live with no consumer, `.stats` text starts with "Not in use".

**Judge by eye.** A two-line toast in the footer looks intended; dimmed frames are clearly "not live"; the empty-chain text is short.

---

### W5. Controls and accessibility basics (P15 + P16 + P17 + P18), effort M

**Goal.** Controls behave the way they look, and the basics of keyboard use, focus, target size and contrast hold.

**Behaviour**

- **P15, Auto sliders**
  - A slider is disabled only when the control has no manual mode (`!info.can_manual`); with Auto on it stays enabled. Moving it calls `setControl(key, { value })`, which already turns Auto off (`store.ts:776-777`).
  - While Auto is on, the value text stays dim (`styles.css:559`) and the slider's title says "Moving this turns Auto off".
  - Keyboard arrows on the slider do the same. Delete and double-click still reset (Auto back on where the camera has it, `store.ts:783-790`).
- **P16, visible reset**
  - When a filter slider's value differs from `def`, or a camera control's value differs from `info.def` (or Auto differs from `can_auto`), a ghost reset button appears after the value: 16 px icon, 24×24 hit area, `aria-label="Reset Gamma"`, `title="Reset to 0.00"`.
  - Clicking it resets, the same as double-click. It is hidden at the default, so cards at their defaults stay clean.
  - Number fields (crop), selects, colours and paths get no reset button in this wave.
- **P17.** For every `.params label.pf-row`, the caption takes `flex: 1 1 0; min-width: 0` and the switch sits at the right end of its grid cell. This is the existing rule at `styles.css:428`, made general; delete the special case.
- **P18, accessibility**
  - Focus ring: `:focus-visible { outline: 2px solid var(--accent); outline-offset: 2px }`. The new grip and splitter get it too.
  - Switch hit area: a `::before` with `inset: -4px 0` gives 30×24 without changing the look.
  - Contrast: raise `--faint` from `#636e7e` to about `#7f8a9a` (estimated 4.6:1 on `#202226`). Verify on the composited background in a screenshot over both a dark and a light desktop. If a light desktop drops below 4.5:1, raise it further or raise `--bg` alpha. `--dim` is unchanged.
  - Settings: pass `role="dialog"` (`TitleBar.tsx:98`) and `aria-label="Settings"`. On open, focus the first switch. Escape closes and returns focus to the Settings button. Add an `autoFocusFirst` option to `Popover`; other popovers are unchanged.

**Files.** `src/components/ControlsCard.tsx`, `src/components/ParamField.tsx`, `src/components/TitleBar.tsx`, `src/components/Popover.tsx`, `src/styles.css`.

**Check (CDP)** (with focus emulation on)

1. Auto: find `.ctl[data-key="white_balance"]` with its Auto switch `aria-checked="true"` (the W4GS camera in `p8-card.png` has it). The `input[type=range]` is not `disabled`. Press on the slider at 25 % of its width and release. Within 2 s the Auto switch is `aria-checked="false"`, and the value text changed.
2. Reset button: focus the Gamma slider of a Color Correction card and send ArrowRight ×5. A `button[aria-label="Reset Gamma"]` exists in that field. Click it: the value text is "0.00" and the button is gone.
3. Focus ring: Tab through the first 25 stops. For each, `getComputedStyle(activeElement).outlineWidth === "2px"`, or it is a text input (border focus style).
4. Hit size: for a filter card switch at centre (x, y), `document.elementFromPoint(x, y - 10)` is the switch.
5. Contrast: compute the contrast of `--faint` against the background pixel next to a `.label` in a `Page.captureScreenshot` at 1200×800. It must be ≥ 4.5.
6. Settings: click Settings. The `.pop-menu` has `role="dialog"`, and `activeElement` is the first `[role=switch]` in it. Send Escape: `activeElement` is the Settings button.
7. Switch alignment: in Image Mask, Apply LUT and Scaling cards, each `.pf-row .switch` right edge is within 2 px of its `.pf-row` right edge.

**Judge by eye.** The reset icons don't clutter a busy card; the brighter `--faint` keeps the quiet look; the 2 px ring is clear but not loud.

---

## 4. Design notes for wave 2 (not first wave; recorded so the next brief starts from here)

**L1, preview point.**
- New command `set_preview_point(point: null | "input" | { filter: id })`. `null` is today's fitted output; `"input"` is the camera picture before the chain; `{ filter }` is the picture entering that filter, whether the filter is enabled or not.
- In `Chain::run` (`fx/mod.rs:338-369`), call the tap with the intermediate texture just before the chosen slot, and do not call it after the output stage.
- The frame header grows from 16 to 24 bytes, adding `source_width` and `source_height`: the full size of the tapped picture before the downscale. The `stride` field already allows layout change (`preview.rs:17-20`); bump the reader in `Preview.tsx:17-23` in the same change.
- The output to apps is unchanged. The point resets to `null` when the gesture ends, when the window hides, and when the filter is removed.
- Tests: the tap sees the pre-filter picture, the virtual camera output is unchanged, and the header sizes are right.

**Coordinate mapping (L2, L3).**
- With the box rect B, the canvas size (cw, ch) and the header's source size (sw, sh): s = min(B.w / cw, B.h / ch), ox = (B.w − cw·s) / 2, oy = (B.h − ch·s) / 2.
- Source pixel x = round((clientX − B.left − ox) / s · sw / cw), and y the same way.
- Points in the letterbox are outside the picture and are ignored, or clamped while a drag is under way.
- A source size change mid-gesture (camera reopened) cancels the gesture.

**Which filter a gesture edits.** Only the card whose "Edit on picture" (crop) or "Pick from preview" (key) button started it. One session at a time; starting another ends the first. The card gets the accent outline. The preview shows a label: "Crop/Pad · step 2 · showing the picture before this filter". The buttons are disabled with a tooltip while the engine is stopped or the preview is off.

**L2, eyedropper.**
- Sample from the last frame's RGBA buffer, already in `onFrame` (`Preview.tsx:157-175`); keep a reference, no GL readback. Average 5×5 preview pixels.
- Convert to OBS `0xAABBGGRR`, keeping the alpha byte. Set `key_color` and switch `key_color_type` to `custom`.
- A swatch follows the pointer. Click commits; Escape cancels. One Undo entry per pick.

**L3, crop, zoom and pan.**
- The rectangle has 8 handles, 12 px hit areas; dragging inside moves it.
- Relative on: write left, top, right, bottom as distances from the edges. Relative off: write left, top, cx, cy as explicit sizes; the "0 runs to the edge" rule stays a number-field feature.
- A gesture never writes a negative (padding) value. An edge already negative shows its handle at the picture edge, marked "padded", and keeps its value unless dragged.
- Minimum crop window: 16×16 source pixels. Shift locks the aspect ratio.
- Wheel over the picture zooms the window about the pointer, keeping the output aspect; arrow keys nudge by 1 px (Shift: 10).
- Writes go through `setParam`; `pushChain` already folds them to one call per frame (`store.ts:253-283`). One Undo entry per gesture.

**As built (L2, L3).**
- One tool at a time lives in the store (`previewTool`, set with `startPreviewTool`); it owns `previewPoint` and ends when the point does. While it runs, the preview bar (fps, Compare, Preview switch) is hidden and Compare is disabled, so no chip sits over a handle or a pick. Esc or the card's Done / Cancel pick ends it.
- A source-size change ends the tool, and a drag under way is put back; frames in the first 600 ms after the start only set the baseline (`sourceChange`, `src/ui/previewTool.ts`).
- Crop: a pan, a wheel zoom or an arrow key moves every edge, so it writes them all inside the picture (a padded edge becomes the picture edge); a handle drag writes only the edges it moved. Handles on the picture's edge sit 6 px inside it so their whole hit area is on the preview. A window within one pixel of the whole picture snaps to it when zooming out. Wheel and arrow-key bursts are one Undo each (450 ms of stillness ends a burst).
- The eyedropper averages 5×5 preview pixels of the last frame and sets `key_color` (alpha byte kept) and `key_color_type: "custom"` in one change, one Undo.

**L4, hold to compare.** A "Compare" button in the preview bar: while it is held (pointer, or Space with it focused), the preview point is `"input"` and the label reads "Before". Release goes back.

---

## 5. Open questions for the owner

| # | Question | Recommendation |
|---|---|---|
| Q1 | A `.png` dropped on the chain could be a LUT (HaldCLUT) or a mask picture. Ask each time, or guess? | Ask with the small chooser (W2). A guess that turns the picture magenta is worse than one click. |
| Q2 | Should a dropped preset file be saved into the presets folder, or only loaded as an unsaved chain? | Save and load it (W2), with Undo. Losing an imported preset on the next load is the surprise to avoid. |
| Q3 | The NVIDIA install dialog opens at the first launch on RTX machines, before the user sees their camera (`store.ts:685-690`). Move it to the first time an NVIDIA filter is added? | Yes (L7, small: can join W4). The card already shows "Install it now" (`FilterCard.tsx:72-80`). |
| Q4 | Better Mic has the same HTML5 reorder (`E:\project\Better-Mic\src\components\FilterCard.tsx:22-28`) and the same toast position. Port W1, P9 and P18 there afterwards? | Yes, after W1 and W4 have been used for a while here. This plan does not touch Better Mic. |
| Q5 | Zoom and pan on the picture write Crop/Pad params (L3), because Transform has no pan or zoom and filter semantics stay OBS-compatible. Is a crop used as zoom acceptable, or do you want a separate "Zoom" filter later? | Use Crop/Pad. A new filter would be a semantics change, which this pass rules out. |
| Q6 | Should the preview point (L1, an engine change) move into the first wave, to bring the eyedropper forward? | No. Keep the first wave engine-free (one small read-only command aside) and start wave 2 with L1. The eyedropper is the first thing built on it. |
| Q7 | Raising `--faint` (W5) makes captions slightly brighter than Better Mic's. Accept a small visual drift for contrast? | Yes, and port the same value to Better Mic with Q4. |
