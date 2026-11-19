# Better Cam

Better Cam takes a webcam, applies capture settings, device controls and filters, and offers the result to other
apps as its own virtual camera, "Better Cam". It is a sibling of Better Mic. It is built with Tauri 2 (Rust) and
React, and runs on Windows 11 only.

## Features

- Capture settings: camera, resolution and frame rate, video format, color space and range, buffering, flip,
  rotation data, hardware decoding, and an option to turn the camera off when no app is using Better Cam.
- Camera controls: the controls the camera itself offers (brightness, exposure, focus, gain and so on). Exposure and
  white balance each have three modes: Manual, Camera auto and Better Cam auto. Focus keeps its Auto switch.
- Better Cam auto: the app sets the camera's exposure, gain and white balance itself, from the picture before the
  filters. In dim light it keeps the frame rate you asked for, up to the most gain you allow, but gain adds noise. When
  the picture is still more than a stop too dark at the most gain, it lets the camera drop to about half the frame rate
  (one stop longer exposure) for a brighter, cleaner picture; set the longest exposure to the frame-rate limit to never
  do that. You set the target brightness, the longest exposure, the most gain, the warmth and the response speed. It is offered
  only on cameras that support manual exposure and white balance.
- Filters, in a chain you can reorder by dragging a card and insert where you want: Color Correction, Crop/Pad,
  Scaling/Aspect Ratio, Transform, Sharpen, Apply LUT, Chroma Key, Color Key, Luma Key, Image Mask/Blend, Auto Adjust,
  and the NVIDIA filters Background Removal, Blur and Background Blur. The NVIDIA filters need a GeForce RTX card and
  NVIDIA's Video Effects runtime (see Licence).
- Auto Adjust: moves brightness, contrast, saturation, warmth and tint toward targets you set. Its white balance also
  corrects warm light, such as an orange lamp.
- Extension filters: WGSL or HLSL shaders, each in its own folder with an `extension.json`, in
  `%APPDATA%\dev.bettercam.app\extensions`. Settings → Extensions lists them. Scripts, overlays and widgets are not
  built yet.
- Presets: save and load the whole look of the camera by name: the filter chain, the camera and its capture settings, the camera controls with their Manual / Camera auto / Better Cam auto modes, Better Cam auto's settings, and the output. Loading is one step you can undo; the title bar marks unsaved changes (`Name*`). Save, Save as, rename, delete and Reset to defaults are in the Preset menu. A fresh install and Reset to defaults start from the built-in defaults: one Auto Adjust filter (white balance only), the camera's largest 16:9 size at 30 fps in MJPEG, Better Cam auto white balance, and the output filling the frame (Default in the title bar). Existing saved state and presets are not changed by the new defaults. If you used the old defaults before upgrading to 0.1.2, the title bar shows "Unsaved" until you press Reset to defaults, because the defaults are now different. Presets from older versions (filters only) still load and change only the filters. App settings (autostart, tray, extensions, window layout) are not part of a preset.
- Live preview and a line of stats (capture and output size and frame rate, latency, dropped and repeated frames).
  The window has three docked regions: a navigator on the left (Camera, Camera controls, Output, then the filter chain as
  rows with a status dot or spinner), the preview in the middle (always in view, 16:9) and the inspector on the right (the
  settings of the one selected item). Splitters set the widths of the navigator and the inspector. Below 1100 px the navigator
  is a 52 px icon rail (its » button opens the full list over the preview; the button at the top of the navigator also
  collapses it at any width). The navigator works from the keyboard: arrows select, Space bypasses a filter, Delete removes
  it, Enter or F6 goes to the inspector, Escape comes back.
- Preview tools: show the picture as it enters a chosen filter. Compare is a toggle: a bar appears over the preview
  with the camera picture (before the filters) on one side and the result (what apps get) on the other; drag it, or
  drag anywhere on the picture, to wipe between them. Arrow keys move the bar (Shift for big steps), `O` turns it
  from left-right to up-down. Esc turns Compare off when nothing else uses it: an open menu, the navigator flyout
  or a dialog closes first, and in the inspector Esc takes the focus back to the navigator and leaves Compare on. An eyedropper picks key colours from the preview (Chroma Key,
  Color Key). Crop/Pad can be edited on the picture: drag to pan, mouse wheel to zoom.
- Drop files from Explorer: a LUT, a picture or a preset (`.json`) onto a field, the chain or the window.
- Output: resolution, frame rate, aspect, background color, and an option to also offer 720p and 360p.
- Errors in plain words (codes stay in tooltips), and accessibility basics: a clear focus ring, larger switch targets
  and readable small labels.
- Launch on Windows startup and minimize to the tray.

## Build

You need Node.js, and Rust with the MSVC toolchain.

```
npm install
npm run tauri dev       # development run
npm run tauri build     # release build and the installer
npm run portable        # portable zip, after the release build
```

`npm run tauri build` also builds the virtual camera DLL first (`npm run vcam:build`). It leaves:

- the installer: `target\release\bundle\nsis\Better Cam_<version>_x64-setup.exe`
- the release exe: `target\release\better-cam.exe`

`npm run portable` packs that exe, the DLL, `LICENSE` and a short `README.txt` into
`target\release\bundle\portable\Better-Cam-<version>-portable.zip`.

The installer is not code-signed, so Windows SmartScreen warns before it runs.

## The virtual camera

The camera is a DLL (`crates/vcam-source`) that Windows' Frame Server loads. The app writes frames to a file in
`%ProgramData%\BetterCam`, and the DLL reads them. When Better Cam is not running, other apps see a "Better Cam is not
running" picture. The virtual camera waits on a signal from the app instead of polling, falling back to polling.

Registering it needs administrator rights, once. `better-cam.exe --vcam-install --user-sid <SID>` does it and
`better-cam.exe --vcam-uninstall` undoes it (no window; details in `%ProgramData%\BetterCam\logs\install.log`).

- Installer: it installs for all users under `C:\Program Files\Better Cam`. After copying the files it runs the
  install step for the user who is signed in at the PC, and the uninstaller removes the camera before it removes the
  app. If the install step did not run or failed, the app shows an "Install virtual camera" banner.
- Portable zip: the first start shows the same banner. The camera is installed from the `vcam` folder next to
  `better-cam.exe`. Remove it from Settings > Virtual camera > Remove before you delete the folder.
- Development: the camera is installed from `target\release\better_cam_vcam.dll`.

The camera belongs to one Windows account at a time: the account that installed or repaired it.

## Known limits

- Disconnect and stream errors read in plain words, but some codes stay in tooltips.
- A program that holds the camera through DirectShow may show as "another app" instead of its name.
- Only ffmpeg and test consumers were tested against the virtual camera. Discord, Chrome, Teams and Windows Camera are untested.
- `autostartDefaulted` survives an uninstall, so a reinstall does not re-enable autostart.
- Some settings are accepted but do nothing: Buffering, Apply rotation data, and the 2100 PQ and HLG color spaces.
- NVIDIA filters need the engine on the NVIDIA adapter. There is no adapter switching.
- Untested for real: GPU reset, Windows refusing camera access, a declined elevation prompt, uninstall while running,
  first run on a new profile, the portable zip, a compressed drive, and the installer on another PC.
- After a replug, video returns about 6.7 s after the camera is re-enabled (the camera itself needs 5.9 s).
- Auto Adjust contrast lands slightly short on hard-edged pictures.
- The camera's own white balance cannot go below 2800 K. Auto Adjust finishes the rest.
- The installer is unsigned, so Windows SmartScreen warns before it runs.

## Licence

GPL-2.0-or-later, see `LICENSE`. The filters are ported from OBS Studio, which is GPL-2.0-or-later. NVIDIA's Video
Effects runtime is not part of Better Cam: the app downloads it from NVIDIA when you ask for it, and never bundles it.

## Status

Pre-release. The installer is unsigned.
