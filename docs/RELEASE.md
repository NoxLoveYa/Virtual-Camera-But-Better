# Release readiness: Better Cam 0.1.3

Latest build: 2026-11-20 from commit `8aa77af` (`chore(release): version 0.1.3`), on top of `5b22f7f` (`feat(preview): Compare shows the selected filter's before and after`), which follows `0ea3d38` (`docs(release): build 0.1.2 from 4c0b37f`), on the development PC (Windows 11 Pro 10.0.26200). The working tree was clean before the build. The tag `v0.1.3` is on the `docs(release)` commit that follows `8aa77af`, and the GitHub release `v0.1.3` was published from that tag as the latest release (not a pre-release). The earlier releases are `v0.1.2` (pre-release, from `4c0b37f`) and `v0.1.0` (pre-release).

Earlier builds: `4c0b37f` (version 0.1.2; published as the pre-release `v0.1.2`, tag on the release-notes commit `0ea3d38`), `758873d` (version 0.1.1; not published, no tag; its hashes are kept in `fae5659`), `42fef49` (published as the GitHub pre-release `v0.1.0`, tag on the release-notes commit `2ab8162` that follows `42fef49`), `f68c15a` (same day; its hashes are superseded and no longer listed), and `ac02150` (its install test is the one recorded below).

## Build

`$env:CARGO_INCREMENTAL=0; npm run tauri build`, then `npm run portable`. Both exit 0. Version 0.1.3
(`src-tauri/tauri.conf.json`), publisher NoxLoveYa, per-machine NSIS installer.

| File | Size (bytes) | SHA-256 |
|---|---|---|
| `target\release\bundle\nsis\Better Cam_0.1.3_x64-setup.exe` | 3,773,440 (3.60 MiB) | `AF7B0B4E16EA9E40AF758E73B93A62A5F3745FC47C2CBB21CAE6E20C4D535563` |
| `target\release\bundle\portable\Better-Cam-0.1.3-portable.zip` | 5,138,746 | `6572D5A5B57125C989D08C838799ED7F684C1B6808F17A1EF28A35C356CBA700` |
| `target\release\better-cam.exe` (in the zip; the installer packs the same file) | 14,551,040 | `FB84AE691E9A8A7837269C4F5D55DB15073C96395431EA382533EF146382BF0C` |
| `target\release\better_cam_vcam.dll` (in the zip; the installer packs the same file; build id `1ff5fec3`) | 809,472 | `45558C02D3A4B4A263DDA2486BE8EE16196F00113AF163876223B99A9DB4CE1B` |

The zip holds `better-cam.exe`, `vcam/better_cam_vcam.dll`, `LICENSE` and `README.txt`. Entry sizes were read from the zip in memory, nothing extracted; the exe and the DLL in it have the same sizes as `target\release\better-cam.exe` and `target\release\better_cam_vcam.dll`. Their hashes inside the zip were not compared. The build id is the FNV-1a 64 hash of the file bytes, folded to 8 hex digits (`content_tag` in `crates/vcam-install/src/util.rs`). It was computed with a separate script from that method and not cross-checked against a known file. The installer's inner copy of the exe was not extracted and compared. The zip's exe has no `--version` switch, so it was not started (it would take the camera).

### What this release adds (since `v0.1.2`, `5b22f7f`)

The version bump to 0.1.3 touched the manifests and lock files only.

- **Per-filter Compare** (`5b22f7f`): with a filter selected, Compare splits the picture entering that filter from the picture leaving it, live, both fitted like the output. With anything else selected, Compare stays camera before the chain vs output. The split follows the selection; labels read "Before <filter>" / "After". Compare on the Camera controls is unchanged.
- Preview points no longer wait behind a slider drag; only a point that names a filter the engine does not have yet waits for its chain.

Build warnings (none affects the result):

- Tauri: the identifier `dev.bettercam.app` ends with `.app`, which conflicts with the macOS bundle extension. Harmless on Windows. Changing it would change the data folders (`%APPDATA%\dev.bettercam.app`), so it is left alone.
- Linker: `LNK4104: export of symbol 'DllCanUnloadNow' / 'DllGetClassObject' should be PRIVATE` for the virtual camera DLL. Cosmetic.

## Tests on this PC

Checked for this build, on the tree at `8aa77af`:

- `npm run test:schema`, `npm run test:preset` (includes `test:store`) and `npm run test:ui` (includes `compare.test.ts` and `previewFrame.test.ts`): all exit 0.
- `cargo test -p better-cam preview_point`: 10 passed, 0 failed.
- `npm run tauri build` and `npm run portable`: both exit 0.

The new Compare was checked by automated tests only. It was **not** looked at by eye on a live camera.

Not repeated for this build: the install and uninstall tests, starting the installed app, a real camera check of the new Compare, the dim-room exposure and the face metering. The virtual camera DLL is not the file those earlier tests used (build id `1ff5fec3` now, `282bb805` in the last install test).

### Install and uninstall: last run, on the 0.1.0 build from `ac02150`

The results below are from the earlier installer (`Better Cam_0.1.0_x64-setup.exe`, SHA-256 `73F978F9...3545A5`, virtual camera DLL build id `282bb805`). The installer code (`src-tauri/windows`, the `bundle` part of `src-tauri/tauri.conf.json`, `crates/vcam-install`, `scripts`) has changed since, including `crates/vcam-install/src/acl.rs`, `testkit.rs` and `datadir/tests.rs` in `a3e348e` (the `acl.rs` hunks checked are all in `mod tests`; the rest was not checked). The vcam crates (`vcam-protocol`, `vcam-source`) changed too. So the results describe the installer's logic and the earlier DLL, and they still need a repeat for this build.

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
- Not tested here: the new dim-room exposure and face metering on a real camera (the W4GS or another camera, in a dim room
  and with a window or lamp behind the subject), the new defaults on a fresh install, upgrade over an older version
  (`/UPDATE` path), a clean PC without the dev DLL present, a second Windows account, reboot persistence, the first start of
  the installed app, and a real camera session with the installed app.
- `a3e348e` changed non-test files (see "What this release adds"); those hunks have not been reviewed for this release.
