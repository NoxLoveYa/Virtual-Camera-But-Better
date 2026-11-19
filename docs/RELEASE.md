# Release readiness: Better Cam 0.1.0

Latest build: 2026-11-19 from commit `42fef49` (`fix(presets): undo and save cannot cross a preset load`), on the
development PC (Windows 11 Pro 10.0.26200). The working tree was clean. This build is published as the GitHub pre-release
`v0.1.0` (tag `v0.1.0`, on the release-notes commit that follows `42fef49`).

Earlier builds: `f68c15a` (same day; its hashes are superseded and no longer listed), and `ac02150` (its install test is
the one recorded below).

## Build

`$env:CARGO_INCREMENTAL=0; npm run tauri build`, then `npm run portable`. Version 0.1.0 (`src-tauri/tauri.conf.json`),
publisher NoxLoveYa, per-machine NSIS installer.

| File | Size (bytes) | SHA-256 |
|---|---|---|
| `target\release\bundle\nsis\Better Cam_0.1.0_x64-setup.exe` | 3,759,049 (3.58 MiB) | `C4EDD40C55AFFCE171B4A31955659F8E4DAD9386F9F7AF37EB869735E2178639` |
| `target\release\bundle\portable\Better-Cam-0.1.0-portable.zip` | 5,115,136 | `00F3A17AFCF09A9A2AA023ED0EC822A6F8E5562F9857D796FDF512241E159481` |
| `target\release\better-cam.exe` (in the zip; the installer packs the same file) | 14,492,160 | `66FE89A339C850EAB7B027C16B785EB3609AE2B7918CA1EDBBF5A4813540CFB3` |
| `target\release\better_cam_vcam.dll` (in the zip; the installer packs the same file; build id `282bb805`) | 809,472 | `516CB5AFB8DF42B19A81D40EC9195370F693A2FBF65C879E73ADF16D3DE1E074` |

The zip holds `better-cam.exe`, `vcam/better_cam_vcam.dll`, `LICENSE` and `README.txt` (checked by reading the zip
entries in memory, nothing extracted); the exe and the DLL in it match `target\release\better-cam.exe` and
`target\release\better_cam_vcam.dll`. The virtual camera DLL is byte for byte the same as in the earlier builds (same hash,
same build id). The installer's inner copy of the exe was not extracted and compared. The zip's exe has no `--version`
switch, so it was not started (it would take the camera).

### What this release adds (since `f68c15a`)

Only app code and docs changed; the virtual camera and the installer did not (see the install-test note below).

- Inspector layout: a navigator on the left (Camera, Camera controls, Output, then the filter chain with status dots),
  the preview in the middle, and a settings panel on the right for the selected item. Below 1100 px the navigator becomes
  an icon rail. Keyboard navigation: arrows select, Space bypasses a filter, Delete removes it, Enter or F6 goes to the
  settings, Escape comes back.
- Presets save and reapply everything: the filter chain, camera and capture settings, camera controls with their modes,
  Better Cam auto settings, and output. Loading is one step you can undo. The title bar marks unsaved changes. Rename and
  Reset to defaults are in the Preset menu. Presets from older versions (filters only) still load.
- Compare: a split-wipe toggle over the preview. A bar shows the camera picture on one side and the result on the other;
  drag it, or the picture, to wipe. It can be vertical or horizontal.
- Warmth fix `1a8a686`: warmth always applies; a face is followed slowly, not held.
- Preset file safety: names Windows cannot store are refused up front; undo and save cannot cross a preset load.

Build warnings (none affects the result):

- Tauri: the identifier `dev.bettercam.app` ends with `.app`, which conflicts with the macOS bundle extension. Harmless
  on Windows. Changing it would change the data folders (`%APPDATA%\dev.bettercam.app`), so it is left alone.
- Linker: `LNK4104: export of symbol 'DllCanUnloadNow' / 'DllGetClassObject' should be PRIVATE` for the virtual camera DLL.
  Cosmetic.

## Tests on this PC

These install and uninstall tests were run on the earlier build from commit `ac02150` (`Better Cam_0.1.0_x64-setup.exe`
with SHA-256 `73F978F9...3545A5`), and were **not repeated** for this build from `42fef49`. The installer code
(`src-tauri/windows`, the `bundle` part of `src-tauri/tauri.conf.json`, `crates/vcam-install`, `scripts`) is unchanged
between `3fa0ba2` and `42fef49` (`git diff f68c15a HEAD --stat` on those paths is empty, and `tauri.conf.json` did not change
in that range either), and the virtual camera DLL has the same hash, so the results below still describe the installer. The installed app code is newer, and it has not been
started from an installer.

Both the dev virtual camera and the installer use the same folder and the same DLL name
(`C:\Program Files\Better Cam\vcam\better_cam_vcam_1.282bb805.dll`), so the dev camera was registered with the very same
DLL (same hash) before the test. The installer's install and uninstall therefore replaced the dev registration and
the dev state was put back afterwards. The data of the installed app was never touched: the installed app was not started.

### Before

Dev virtual camera registered and listed in Media Foundation and DirectShow; `HKLM\SOFTWARE\BetterCam\Vcam` pointed to
the DLL above; no `HKLM\SOFTWARE\NoxLoveYa`; no Uninstall key for Better Cam; no Better Cam autostart entry (Run or
StartupApproved); no Start menu or desktop shortcut; `%APPDATA%\dev.bettercam.app\presets` empty;
`%LOCALAPPDATA%\dev.bettercam.app` (WebView2 profile, with localStorage `better-cam`) backed up first.

### Silent install (`Better Cam_0.1.0_x64-setup.exe /S`)

Exit code 0 after about 2 s, no window, no prompt, no process left running (the app does not start after a silent install).

- `C:\Program Files\Better Cam`: `better-cam.exe`, `uninstall.exe`, `vcam\better_cam_vcam.dll` (copy of the bundle's) and
  `vcam\better_cam_vcam_1.282bb805.dll` (the registered copy); both DLLs have the bundle's hash.
- Uninstall key `HKLM\...\Uninstall\Better Cam`: DisplayVersion 0.1.0, Publisher NoxLoveYa, location
  `C:\Program Files\Better Cam`. `HKLM\SOFTWARE\NoxLoveYa\Better Cam` = the install folder.
- Start menu: `C:\ProgramData\Microsoft\Windows\Start Menu\Programs\Better Cam.lnk`. Also a shortcut on the **public desktop**
  (`C:\Users\Public\Desktop\Better Cam.lnk`).
- Virtual camera: registered by the post-install hook with the release DLL (hash equal to the bundle's), listed in Media Foundation
  and DirectShow, `install.log`: "registered ... camera created and started".
- `vcam-tool capture-test --expect slate --size 1280x720 --frames 90`: PASS, "Better Cam is not running" slate, 90 of 90 frames
  (29.67 fps).

### Silent uninstall (`uninstall.exe /S`)

Exit code 0 after about 6 s. Gone: `C:\Program Files\Better Cam`, the Uninstall key, `HKLM\SOFTWARE\NoxLoveYa` (and its
publisher key), `HKLM\SOFTWARE\BetterCam`, the camera's CLSID key, `C:\ProgramData\BetterCam` (frames file and logs), the
Start menu entry and the public desktop shortcut, the startup marker (none existed). Better Cam was no longer listed in
Media Foundation or DirectShow (`registry clsid key present: false`). The camera service (FrameServer) was running again.

Left behind: one file, `%TEMP%\~nsu2.tmp\Un.exe` (82,564 bytes), the copy of the uninstaller that NSIS runs from the temp
folder; it is removed by the next temp cleanup (an empty `~nsu1.tmp` from an earlier run is there as well). Nothing else.
Presets (`%APPDATA%\dev.bettercam.app`) and the WebView2 data (`%LOCALAPPDATA%\dev.bettercam.app`) are not removed by a
silent uninstall, by design (the "delete app data" box is not ticked in silent mode).

### Restore of the dev state

The profile and presets were untouched by install and uninstall (the installed app never ran). The dev virtual camera was
registered again by starting the dev app in test mode and clicking "Install virtual camera" once.

- Registered DLL: `C:\Program Files\Better Cam\vcam\better_cam_vcam_1.282bb805.dll`, hash `516CB5AF...1E074`, build `282bb805`
  (= the release build). Registry values the same as before.
- Listed in Media Foundation and DirectShow, the same symbolic link as before.
- Consumer: `capture-test --expect feed` with `vcam-tool feed`, 1280x720, 120 frames, PASS, 30.00 fps, no torn or black frames.
  The slate checks (`slate`, `slate-paused`, `slate-starting`) also PASS. The webcam was not started.
- localStorage `better-cam` present (787 characters); the Local Storage database differs from the backup by the
  82 bytes the dev app wrote when it started; presets folder empty as before; no autostart entry for Better Cam; the other
  autostart entries and `HKLM\SOFTWARE\NoxLoveYa` (absent) as recorded.
- All started processes (dev app, vite) were stopped; the test lock was released.

## Unsigned installer

The installer and the exe are **not code signed**. What users will see:

- **SmartScreen.** Windows shows "Windows protected your PC" when a user runs an unsigned file downloaded from the
  internet. SmartScreen only stops warning when a file, or a signing certificate, has built up reputation, which takes time
  and many downloads. Users must click **More info**, then **Run anyway**.
- **UAC prompt.** The installer needs administrator rights (it installs for the whole PC and registers the camera). The
  prompt shows the publisher as **"Unknown publisher"**.
- **Antivirus.** Unsigned programs that register a system component are more likely to be flagged or sent for cloud
  analysis by antivirus software. A false positive is possible; reporting it to the vendor usually clears it.
- **The portable zip** has the same issue (SmartScreen on the unzipped exe), and Windows may add the "downloaded from the
  internet" mark to the files.

Signing needs a certificate. The options, with prices and availability **to be checked** before deciding:

- An OV (organization validated) code signing certificate: removes "Unknown publisher"; SmartScreen reputation still builds
  slowly.
- An EV (extended validation) certificate: normally gives immediate SmartScreen reputation; stricter identity checks and a
  higher price.
- Azure Trusted Signing: a cheaper option; it has identity-validation and region rules that must be checked.

Nothing else changes functionally: the same installer is signed after the build (with Tauri's `signCommand` for Windows or
`signtool`), and the install and uninstall behaviour above stays the same.

## Looks wrong for a real release

- The installer also puts a shortcut on the **public desktop** (Tauri's default); remove it if not wanted.
- The install folder holds two copies of the DLL (`vcam\better_cam_vcam.dll` and the versioned one that is registered).
  That is by design (the helper installs from the bundle copy), but it is 0.8 MB twice.
- The installed app and a dev or portable build share the identifier `dev.bettercam.app`, so they share presets and the
  WebView2 profile, and they register the same virtual camera: the last install wins.
- Not tested here: upgrade over an older version (`/UPDATE` path), a clean PC without the dev DLL present, a second Windows
  account, reboot persistence, the first start of the installed app, and a real camera session with the installed app.
