# Better Cam

Better Cam takes a webcam, applies capture settings, device controls and filters, and offers the result to other
apps as its own virtual camera, "Better Cam". It is a sibling of Better Mic. It is built with Tauri 2 (Rust) and
React, and runs on Windows 11 only.

## Features

- Capture settings: camera, resolution and frame rate, video format, color space and range, buffering, flip,
  rotation data, hardware decoding, and an option to turn the camera off when no app is using Better Cam.
- Device controls: the controls the camera itself offers (brightness, exposure, focus, gain and so on), each with an
  Auto switch where the camera has one.
- Filters, in a chain you can reorder: Color Correction, Crop/Pad, Scaling/Aspect Ratio, Transform, Sharpen,
  Apply LUT, Chroma Key, Color Key, Luma Key, Image Mask/Blend, Auto Adjust (automatic exposure and white
  balance in software), and the NVIDIA filters Background Removal, Blur and Background Blur. The NVIDIA filters need
  a GeForce RTX card and NVIDIA's Video Effects runtime (see Licence).
- Presets: save and load a filter chain by name. A preset holds the filters only, not the device controls.
- Live preview and a line of stats (capture and output size and frame rate, latency, dropped and repeated frames).
- Output: resolution, frame rate, aspect, background color, and an option to also offer 720p and 360p.
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
running" picture.

Registering it needs administrator rights, once. `better-cam.exe --vcam-install --user-sid <SID>` does it and
`better-cam.exe --vcam-uninstall` undoes it (no window; details in `%ProgramData%\BetterCam\logs\install.log`).

- Installer: it installs for all users under `C:\Program Files\Better Cam`. After copying the files it runs the
  install step for the user who is signed in at the PC, and the uninstaller removes the camera before it removes the
  app. If the install step did not run or failed, the app shows an "Install virtual camera" banner.
- Portable zip: the first start shows the same banner. The camera is installed from the `vcam` folder next to
  `better-cam.exe`. Remove it from Settings > Virtual camera > Remove before you delete the folder.
- Development: the camera is installed from `target\release\better_cam_vcam.dll`.

The camera belongs to one Windows account at a time: the account that installed or repaired it.

## Licence

GPL-2.0-or-later, see `LICENSE`. The filters are ported from OBS Studio, which is GPL-2.0-or-later. NVIDIA's Video
Effects runtime is not part of Better Cam: the app downloads it from NVIDIA when you ask for it, and never bundles it.

## Status

Pre-release. The icons are still those of Better Mic, and the installer is unsigned.
