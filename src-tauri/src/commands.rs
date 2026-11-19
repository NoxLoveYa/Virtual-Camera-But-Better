use better_cam::api::Service;
use better_cam::auto_controls::Mode as ControlMode;
use better_cam::camera_controls::ControlInfoDto;
use better_cam::config::{CameraDto, OutputCfg, SourceCfg};
use better_cam::ext::{ExtRegistry, ExtensionInfo};
use better_cam::file_dialog;
use better_cam::fx::nvidia;
use better_cam::preview::Sink;
use better_cam::vcam_admin::{self, VcamStatus};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::{AppHandle, Manager, State};
use tauri_plugin_autostart::ManagerExt;

/// Passed to the app by the Windows startup entry, so a login launch can be told apart from a manual one.
pub const AUTOSTART_ARG: &str = "--autostarted";

#[derive(Default)]
pub struct AppState {
    pub close_to_tray: AtomicBool,
    /// The engine and the logic behind the engine commands (`better_cam::api`).
    pub service: Arc<Service>,
    /// The extensions found in `%APPDATA%\dev.bettercam.app\extensions` (started in `main`'s setup).
    pub extensions: ExtRegistry,
}

/// Run a blocking job off the UI thread. Media Foundation calls (enumerating formats, opening a camera, creating
/// the D3D11 device) take hundreds of milliseconds.
async fn blocking<T: Send + 'static>(
    job: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(job)
        .await
        .map_err(|e| e.to_string())?
}

/// Cameras with their formats and frame intervals; Better Cam's own camera is not listed.
#[tauri::command]
pub async fn list_cameras(state: State<'_, AppState>) -> Result<Vec<CameraDto>, String> {
    let svc = state.service.clone();
    blocking(move || svc.list_cameras()).await
}

/// Start the engine: Standby, then Live (at once, or when an app or the window watches with
/// `deactivate_when_not_showing`). `controls` is the record of the changed camera controls, re-applied after every
/// camera open; `chain` is the initial filter chain (an array of `{id, kind, enabled, params}`); omitted, the engine
/// keeps the chain of an earlier `set_chain`.
#[tauri::command]
pub async fn start_engine(
    state: State<'_, AppState>,
    source: SourceCfg,
    output: OutputCfg,
    controls: Option<serde_json::Value>,
    chain: Option<serde_json::Value>,
) -> Result<(), String> {
    let svc = state.service.clone();
    blocking(move || svc.start_engine(source, output, controls, chain)).await
}

#[tauri::command]
pub async fn stop_engine(state: State<'_, AppState>) -> Result<(), String> {
    state.service.stop_engine()
}

/// Reopens the camera only when a capture setting changed (device, size, rate, format, hw decode, buffering).
/// `controls` (optional) replaces the record of changed camera controls, for a camera that was just selected.
#[tauri::command]
pub async fn set_source(
    state: State<'_, AppState>,
    source: SourceCfg,
    controls: Option<serde_json::Value>,
) -> Result<(), String> {
    state.service.set_control_record(controls)?;
    state.service.set_source(source)
}

/// What the camera `id` supports (plan 5.5): ranges, values, Auto flags. Opens the device briefly when the engine has
/// no camera open.
#[tauri::command]
pub async fn get_camera_controls(
    state: State<'_, AppState>,
    id: String,
) -> Result<Vec<ControlInfoDto>, String> {
    let svc = state.service.clone();
    blocking(move || svc.get_camera_controls(&id)).await
}

/// Set one control: a manual `value`, or Auto on (`auto`). Returns the control as the device reports it.
#[tauri::command]
pub async fn set_camera_control(
    state: State<'_, AppState>,
    id: String,
    key: String,
    value: i32,
    auto: bool,
) -> Result<ControlInfoDto, String> {
    let svc = state.service.clone();
    blocking(move || svc.set_camera_control(&id, &key, value, auto)).await
}

/// Every control back to the device's default, Auto on where the device has Auto. Every Better Cam auto mode ends.
#[tauri::command]
pub async fn reset_camera_controls(
    state: State<'_, AppState>,
    id: String,
) -> Result<Vec<ControlInfoDto>, String> {
    let svc = state.service.clone();
    blocking(move || svc.reset_camera_controls(&id)).await
}

/// Who drives a control: `mode` is `manual`, `camera` (the camera's Auto flag) or `app` (Better Cam auto; exposure
/// and white balance). Returns the rows that changed (exposure together with gain). Works while the camera is closed
/// too: the mode and its resume point are stored and the loop starts at the next open.
#[tauri::command]
pub async fn set_control_mode(
    state: State<'_, AppState>,
    id: String,
    key: String,
    mode: ControlMode,
) -> Result<Vec<ControlInfoDto>, String> {
    let svc = state.service.clone();
    blocking(move || svc.set_control_mode(&id, &key, mode)).await
}

/// The camera's Better Cam auto settings: `{exposure: {target, longest, max_gain, speed}, white_balance: {warmth,
/// speed}}`. Live: the next decision uses them.
#[tauri::command]
pub async fn set_auto_config(
    state: State<'_, AppState>,
    id: String,
    cfg: serde_json::Value,
) -> Result<(), String> {
    state.service.set_auto_config(&id, cfg)
}

/// Start Better Cam auto again after a `conflict`; also clears `blocked` ("Try again": set the mode again after).
#[tauri::command]
pub async fn resume_auto(
    state: State<'_, AppState>,
    id: String,
    key: String,
) -> Result<(), String> {
    state.service.resume_auto(&id, &key)
}

#[tauri::command]
pub async fn set_output(state: State<'_, AppState>, output: OutputCfg) -> Result<(), String> {
    state.service.set_output(output)
}

/// Replace the filter chain: an array of `{id, kind, enabled, params}` (plan 5.3). It applies to the next frame with no
/// reopen and no restart, also while stopped (the engine keeps it for the next start). Unknown kinds are skipped and
/// values are clamped to their ranges. Not async on purpose: commands that are not async run in order on one thread,
/// so the newest chain is always the one applied, and the call only parses and queues a message.
#[tauri::command]
pub fn set_chain(state: State<AppState>, chain: serde_json::Value) -> Result<(), String> {
    state.service.set_chain(&chain)
}

#[tauri::command]
pub async fn set_pause(state: State<'_, AppState>, on: bool) -> Result<(), String> {
    state.service.set_pause(on)
}

/// Start or stop the live preview. Frames of at most `width` x `height` pixels (and 960x540) arrive on `channel` as
/// raw bytes: a 28-byte header (seven little-endian u32) and NV12 pixels (see `better_cam::preview`). A new call
/// replaces the old stream; the webview acknowledges each frame with `preview_ack`.
#[tauri::command]
pub async fn set_preview(
    state: State<'_, AppState>,
    on: bool,
    width: u32,
    height: u32,
    channel: Channel<InvokeResponseBody>,
) -> Result<(), String> {
    let svc = state.service.clone();
    let sink: Sink = Arc::new(move |bytes| channel.send(InvokeResponseBody::Raw(bytes)).is_ok());
    // Creating the engine can still be in progress on first use.
    blocking(move || svc.set_preview(on, width, height, sink)).await
}

/// Which picture the preview shows (UX-PASS L1): `null` the output, `"input"` the camera picture before the chain,
/// `{ "filter": id }` the picture entering that filter (on or off). The output to apps is the same whatever this says.
/// The page sets it for the length of a gesture and puts it back to `null`; the engine puts it back by itself when the
/// filter leaves the chain or the window is hidden. Not async on purpose, like `set_chain`: the calls run in order, and
/// a point that names a filter of the chain sent just before is valid.
#[tauri::command]
pub fn set_preview_point(state: State<AppState>, point: serde_json::Value) -> Result<(), String> {
    state.service.set_preview_point(&point)
}

#[tauri::command]
pub fn preview_ack(state: State<AppState>, frame: u32) {
    state.service.preview_ack(frame);
}

/// Open Windows Settings on the camera privacy page (`ms-settings:privacy-webcam`), for the banner that says Windows
/// keeps desktop apps from the camera. Returns at once.
#[tauri::command]
pub fn open_camera_settings() -> Result<(), String> {
    better_cam::capture::privacy::open_settings()
}

/// What the virtual camera needs (plan 5.5): `{state: "missing" | "outdated" | "ok", listed, consumers}`. No elevation.
#[tauri::command]
pub async fn vcam_status() -> Result<VcamStatus, String> {
    blocking(|| Ok(vcam_admin::status())).await
}

/// Install, update or repair the virtual camera: the engine stops and `frames.bin` closes first (the helper may have to
/// replace it), then one UAC prompt. Resolves when the helper has exited, however long the prompt stays open; a
/// second call meanwhile is refused. Resolves to `"done"`, or `"pending_restart"` when it takes effect only after
/// the camera service restarts; rejects with a readable sentence when the prompt is declined or the helper fails.
#[tauri::command]
pub async fn vcam_install(state: State<'_, AppState>) -> Result<String, String> {
    let svc = state.service.clone();
    blocking(move || vcam_admin::install(&svc, &vcam_install::ShellLauncher).map(|d| d.as_str().to_string())).await
}

/// Remove the virtual camera: the engine stops and `frames.bin` closes first, then one UAC prompt. Resolves like
/// [`vcam_install`].
#[tauri::command]
pub async fn vcam_uninstall(state: State<'_, AppState>) -> Result<String, String> {
    let svc = state.service.clone();
    blocking(move || vcam_admin::uninstall(&svc, &vcam_install::ShellLauncher).map(|d| d.as_str().to_string())).await
}

/// The native "open file" dialog (plan 5.5), owned by the app window: `kind` is `"image"` (PNG, JPEG, BMP, GIF) or
/// `"lut"` (`.cube`, `.png`), each with an "All files" entry. Resolves to the chosen file's full path, or `null` when
/// the user cancelled. The dialog runs off the UI thread, so the page keeps running while it is open.
#[tauri::command]
pub async fn pick_file(window: tauri::WebviewWindow, kind: String) -> Result<Option<String>, String> {
    let kind = file_dialog::PickKind::parse(&kind)?;
    // The window handle as an integer: Tauri's `windows` crate need not be the one this crate builds against.
    let owner = window.hwnd().ok().map(|h| h.0 as isize);
    blocking(move || file_dialog::pick_file(kind, owner)).await
}

/// Whether NVIDIA's Video Effects runtime is usable (plan 4.6): `{available, detail, version}`. Reads the GPU and the
/// runtime folder and loads the runtime DLL for a moment to ask its version, so it runs off the UI thread.
#[tauri::command]
pub async fn nvidia_status() -> Result<nvidia::Status, String> {
    blocking(|| Ok(nvidia::probe())).await
}

/// The installed RTX GPU with the redistributable build that fits it, or `null` (`nvidia-smi` takes a second or so).
#[tauri::command]
pub async fn nvidia_gpu() -> Result<Option<nvidia::Gpu>, String> {
    blocking(|| Ok(nvidia::detect_gpu())).await
}

/// One install at a time.
static NVIDIA_INSTALLING: AtomicBool = AtomicBool::new(false);

/// Downloads NVIDIA's Video Effects redistributable for `arch`, accepts it only by a valid NVIDIA signature or its
/// pinned fingerprint, and runs it elevated. Progress goes out as `nvidia-install` events `{stage, done, total}`;
/// resolves when the installer has closed, rejects with a sentence otherwise.
#[tauri::command]
pub async fn install_nvidia_sdk(app: AppHandle, arch: String) -> Result<(), String> {
    use tauri::Emitter;

    let url = nvidia::installer_url(&arch).ok_or("Unknown GPU generation.")?;
    #[cfg(debug_assertions)]
    let url = nvidia::test_url_override().unwrap_or(url);
    let pins = nvidia::pins();
    #[cfg(debug_assertions)]
    let pins = nvidia::test_pins(&url).unwrap_or(pins);
    if NVIDIA_INSTALLING.swap(true, Ordering::SeqCst) {
        return Err("The NVIDIA installer is already running.".into());
    }
    let result = blocking(move || {
        nvidia::run_install(&url, &pins, None, |line| {
            if let Some(p) = line.progress() {
                let _ = app.emit("nvidia-install", p);
            } else if let nvidia::ScriptLine::Signature { status, signer } = line {
                eprintln!("[nvidia] installer signature: {status}, signer {signer}");
            } else if let nvidia::ScriptLine::Accepted { rule, detail } = line {
                eprintln!("[nvidia] installer accepted by {rule}: {detail}");
            }
        })
    })
    .await;
    NVIDIA_INSTALLING.store(false, Ordering::SeqCst);
    result
}

#[tauri::command]
pub fn get_autostart(app: AppHandle) -> Result<bool, String> {
    app.autolaunch().is_enabled().map_err(|e| e.to_string())
}

#[tauri::command]
pub fn set_autostart(app: AppHandle, on: bool) -> Result<(), String> {
    let launcher = app.autolaunch();
    let res = if on {
        launcher.enable()
    } else if launcher.is_enabled().unwrap_or(false) {
        launcher.disable()
    } else {
        Ok(())
    };
    res.map_err(|e| e.to_string())
}

#[tauri::command]
pub fn launched_at_startup() -> bool {
    std::env::args().any(|a| a == AUTOSTART_ARG)
}

#[tauri::command]
pub fn set_close_to_tray(state: State<AppState>, on: bool) {
    state.close_to_tray.store(on, Ordering::Relaxed);
}

/// The file of preset `name` inside the presets folder `dir`; the name is checked (it becomes a file name).
fn preset_file_in(dir: &std::path::Path, name: &str) -> Result<PathBuf, String> {
    let ok = !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_'));
    if !ok {
        return Err("preset names may only contain letters, digits, spaces, '-' and '_'".into());
    }
    Ok(dir.join(format!("{name}.json")))
}

/// The presets folder (created when missing).
fn presets_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app.path().app_data_dir().map_err(|e| e.to_string())?.join("presets");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir)
}

fn preset_path(app: &AppHandle, name: &str) -> Result<PathBuf, String> {
    preset_file_in(&presets_dir(app)?, name)
}

/// The names of the presets in `dir`, sorted: the `.json` files only, so the `.json.tmp` an interrupted save may leave
/// behind is never listed.
fn list_in(dir: &std::path::Path) -> Result<Vec<String>, String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .map_err(|e| e.to_string())?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("json")))
        .filter_map(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .collect();
    v.sort();
    Ok(v)
}

#[tauri::command]
pub fn list_presets(app: AppHandle) -> Result<Vec<String>, String> {
    list_in(&presets_dir(&app)?)
}

/// The preset object around a chain list: `{ "app", "version", "chain" }` and, when given, the `camera` and `output`
/// sections. `version` is 2 when a section is there (a version 2 file), else 1 (filters only).
fn preset_object(chain: Vec<serde_json::Value>, camera: Option<serde_json::Value>, output: Option<serde_json::Value>) -> serde_json::Value {
    let version = if camera.is_some() || output.is_some() { 2 } else { 1 };
    let mut o = serde_json::json!({ "app": "better-cam", "version": version, "chain": chain });
    for (key, section) in [("camera", camera), ("output", output)] {
        if let Some(s) = section {
            o[key] = s;
        }
    }
    o
}

/// A section of a preset object when it is a JSON object; the frontend normalises what is inside, Rust keeps it opaque.
fn section(preset: &serde_json::Value, key: &str) -> Option<serde_json::Value> {
    preset.get(key).filter(|s| s.is_object()).cloned()
}

/// What `save_preset` writes: the chain of the given preset object without ids, and its `camera` and `output` sections
/// when they are objects. Any other key is dropped (`overlays` and unknown ones).
fn preset_to_write(preset: &serde_json::Value) -> Result<serde_json::Value, String> {
    let list = preset.get("chain").and_then(|c| c.as_array()).ok_or("a preset needs a \"chain\" list")?;
    let chain = list
        .iter()
        .map(|f| {
            let mut f = f.as_object().cloned().ok_or("every filter in a preset must be an object")?;
            f.remove("id");
            Ok(serde_json::Value::Object(f))
        })
        .collect::<Result<Vec<_>, &str>>()?;
    Ok(preset_object(chain, section(preset, "camera"), section(preset, "output")))
}

/// What `load_preset` returns for the file's content: the object format with its `camera` and `output` (when they are
/// objects), or the older bare array wrapped in it as version 1. A file that says it is a newer version than this one
/// writes keeps that number, so the page can say so; everything else is read as version 1 or 2.
fn preset_from_file(content: serde_json::Value) -> Result<serde_json::Value, String> {
    match content {
        serde_json::Value::Array(chain) => Ok(preset_object(chain, None, None)),
        serde_json::Value::Object(ref o) => match o.get("chain") {
            Some(serde_json::Value::Array(chain)) => {
                let mut preset = preset_object(chain.clone(), section(&content, "camera"), section(&content, "output"));
                if let Some(v) = o.get("version").and_then(|v| v.as_u64()).filter(|v| *v > 2) {
                    preset["version"] = v.into();
                }
                Ok(preset)
            }
            _ => Err("this preset file has no \"chain\" list".into()),
        },
        _ => Err("this file is not a Better Cam preset".into()),
    }
}

/// Writes `preset` to `path` without a moment in which the file is half written: the text goes to `<file>.tmp`, which
/// then replaces the file (a rename over an existing file replaces it on Windows).
fn write_atomic(path: &std::path::Path, preset: &serde_json::Value) -> Result<(), String> {
    let json = serde_json::to_string_pretty(preset).map_err(|e| e.to_string())?;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, json).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e.to_string()
    })
}

#[tauri::command]
pub fn save_preset(app: AppHandle, name: String, preset: serde_json::Value) -> Result<(), String> {
    write_atomic(&preset_path(&app, &name)?, &preset_to_write(&preset)?)
}

#[tauri::command]
pub fn load_preset(app: AppHandle, name: String) -> Result<serde_json::Value, String> {
    let s = std::fs::read_to_string(preset_path(&app, &name)?).map_err(|e| e.to_string())?;
    preset_from_file(serde_json::from_str(&s).map_err(|e| e.to_string())?)
}

#[tauri::command]
pub fn delete_preset(app: AppHandle, name: String) -> Result<(), String> {
    std::fs::remove_file(preset_path(&app, &name)?).map_err(|e| e.to_string())
}

/// Renames preset `from` to `to` inside `dir`. Refused when `from` does not exist or `to` is another preset; a change
/// of letter case only ("studio" to "Studio") is the same preset and goes through.
fn rename_in(dir: &std::path::Path, from: &str, to: &str) -> Result<(), String> {
    let src = preset_file_in(dir, from)?;
    let dst = preset_file_in(dir, to)?;
    if !src.is_file() {
        return Err(format!("There is no preset named \"{from}\""));
    }
    if from == to {
        return Ok(());
    }
    if dst.exists() && from.to_lowercase() != to.to_lowercase() {
        return Err(format!("A preset named \"{to}\" already exists"));
    }
    std::fs::rename(&src, &dst).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn rename_preset(app: AppHandle, from: String, to: String) -> Result<(), String> {
    rename_in(&presets_dir(&app)?, &from, &to)
}

/// A preset file may be at most this big. Real ones are a few KiB; the cap keeps a wrong file (a video dropped on the
/// window, say) from being read into memory.
const PRESET_FILE_MAX_BYTES: u64 = 1024 * 1024;

/// What `read_preset_file` returns for the file at `path`: the file's chain in the object format (see
/// `preset_from_file`). Only reads; every failure is a sentence for the user.
fn read_preset_at(path: &std::path::Path) -> Result<serde_json::Value, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("could not read this file: {e}"))?;
    if !meta.is_file() {
        return Err("this is not a file".into());
    }
    if meta.len() > PRESET_FILE_MAX_BYTES {
        return Err("this file is too big to be a Better Cam preset (the limit is 1 MiB)".into());
    }
    let bytes = std::fs::read(path).map_err(|e| format!("could not read this file: {e}"))?;
    // Notepad and PowerShell save UTF-8 with a byte order mark, which JSON parsers refuse.
    let json = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes);
    let content = serde_json::from_slice(json).map_err(|_| "this file is not a Better Cam preset (it is not valid JSON)")?;
    preset_from_file(content)
}

/// Reads a preset file from anywhere (a file dropped on the window) so the page can import it; nothing is written.
#[tauri::command]
pub fn read_preset_file(path: String) -> Result<serde_json::Value, String> {
    read_preset_at(std::path::Path::new(&path))
}

/// The extensions found in the extensions folder, valid or not (`state`: `ready`, `error` or `disabled`). The list also
/// arrives as the `extensions-changed` event whenever it changes.
#[tauri::command]
pub fn list_extensions(state: State<AppState>) -> Vec<ExtensionInfo> {
    state.extensions.list()
}

/// Read every extension folder again now, whether or not a file changed; returns the list.
#[tauri::command]
pub async fn reload_extensions(state: State<'_, AppState>) -> Result<Vec<ExtensionInfo>, String> {
    let registry = state.extensions.clone();
    blocking(move || Ok(registry.reload())).await
}

/// Switch an installed extension on or off (remembered). Its cards stay in the chain and are bypassed while it is off.
#[tauri::command]
pub async fn set_extension_enabled(state: State<'_, AppState>, id: String, on: bool) -> Result<Vec<ExtensionInfo>, String> {
    let registry = state.extensions.clone();
    blocking(move || registry.set_enabled(&id, on)).await
}

/// Open the extensions folder in Explorer (created when it does not exist yet). Returns at once.
#[tauri::command]
pub fn open_extensions_folder(state: State<AppState>) -> Result<(), String> {
    use windows::core::{w, HSTRING};
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let dir = state.extensions.root().ok_or("The extensions folder is not known yet.")?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("could not create the extensions folder: {e}"))?;
    // SAFETY: plain call with owned strings; the shell opens the folder in Explorer.
    let r = unsafe { ShellExecuteW(None, w!("open"), &HSTRING::from(dir.as_os_str()), None, None, SW_SHOWNORMAL) };
    // ShellExecute reports success with a value above 32.
    if r.0 as isize > 32 {
        Ok(())
    } else {
        Err(format!("Explorer did not open the folder (code {})", r.0 as isize))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_saved_preset_keeps_camera_and_output_but_not_ids_or_other_keys() {
        let camera = json!({ "device": { "id": "cam", "name": "Cam" }, "capture": { "res_type": "custom" }, "controls": {}, "auto": {} });
        let output = json!({ "resolution": "1280x720", "fps": 30 });
        let given = json!({
            "app": "other", "version": 9, "camera": camera, "output": output, "overlays": [{ "kind": "x" }], "extra": 1,
            "chain": [{ "id": "a1", "kind": "color_filter", "enabled": true, "params": { "saturation": -1 } }]
        });
        let want = json!({
            "app": "better-cam", "version": 2, "camera": camera, "output": output,
            "chain": [{ "kind": "color_filter", "enabled": true, "params": { "saturation": -1 } }]
        });
        assert_eq!(preset_to_write(&given).unwrap(), want);
        // A section that is not an object is dropped, and with both gone the file is a version 1 file.
        let bad = json!({ "chain": [], "camera": "no", "output": [1] });
        assert_eq!(preset_to_write(&bad).unwrap(), json!({ "app": "better-cam", "version": 1, "chain": [] }));
        assert!(preset_to_write(&json!({ "chain": "no" })).is_err());
        assert!(preset_to_write(&json!([1, 2])).is_err());
        assert!(preset_to_write(&json!({ "chain": [5] })).is_err());
    }

    #[test]
    fn the_version_is_2_when_there_is_a_camera_or_an_output() {
        let section = Some(json!({ "x": 1 }));
        assert_eq!(preset_object(vec![], None, None)["version"], 1);
        assert_eq!(preset_object(vec![], section.clone(), None)["version"], 2);
        assert_eq!(preset_object(vec![], None, section.clone())["version"], 2);
        assert_eq!(preset_object(vec![], section.clone(), section)["version"], 2);
        // No section, no key.
        assert!(preset_object(vec![], None, None).get("camera").is_none());
    }

    #[test]
    fn loading_accepts_the_object_and_the_old_bare_array() {
        let entry = json!({ "kind": "color_filter", "enabled": true, "params": {} });
        let want = json!({ "app": "better-cam", "version": 1, "chain": [entry.clone()] });
        assert_eq!(preset_from_file(json!([entry.clone()])).unwrap(), want);
        assert_eq!(preset_from_file(want.clone()).unwrap(), want);
        // A camera section that is not an object (an old test's `{ "gain": 3 }` is one, and the page ignores it) or
        // that is unknown is passed on only when it is an object.
        let junk_camera = json!({ "app": "better-cam", "version": 1, "chain": [entry.clone()], "camera": "no", "output": 5, "other": 1 });
        assert_eq!(preset_from_file(junk_camera).unwrap(), want);
        assert!(preset_from_file(json!({ "app": "better-cam" })).is_err());
        assert!(preset_from_file(json!({ "chain": "no" })).is_err());
        assert!(preset_from_file(json!("text")).is_err());
    }

    #[test]
    fn loading_keeps_camera_and_output() {
        let entry = json!({ "kind": "color_filter", "enabled": true, "params": {} });
        let camera = json!({ "device": null, "capture": {}, "controls": { "brightness": { "value": 140, "auto": false } }, "auto": {} });
        let output = json!({ "fps": 30 });
        let file = json!({ "app": "better-cam", "version": 2, "chain": [entry], "camera": camera, "output": output });
        assert_eq!(preset_from_file(file.clone()).unwrap(), file);
        // A file without a version still gets the one that matches what it holds.
        let mut no_version = file.clone();
        no_version.as_object_mut().unwrap().remove("version");
        assert_eq!(preset_from_file(no_version).unwrap(), file);
        // An older file that holds a section is a version 2 file, whatever it says.
        let mut says_1 = file.clone();
        says_1["version"] = json!(1);
        assert_eq!(preset_from_file(says_1).unwrap(), file);
        // A newer version keeps its number so the page can tell the user.
        let mut newer = file.clone();
        newer["version"] = json!(3);
        newer["future"] = json!({ "x": 1 });
        let got = preset_from_file(newer).unwrap();
        assert_eq!(got["version"], 3);
        assert_eq!(got["camera"], file["camera"]);
        assert!(got.get("future").is_none());
    }

    /// A scratch folder that goes away with the test.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> TempDir {
            let dir = std::env::temp_dir().join(format!("better-cam-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn read_preset_file_reads_the_object_and_the_bare_array() {
        let dir = TempDir::new("read-preset");
        let entry = json!({ "kind": "color_filter", "enabled": true, "params": {} });
        let want = json!({ "app": "better-cam", "version": 1, "chain": [entry.clone()] });

        let object = dir.0.join("object.json");
        std::fs::write(&object, serde_json::to_string(&want).unwrap()).unwrap();
        assert_eq!(read_preset_at(&object).unwrap(), want);

        let bare = dir.0.join("bare.json");
        std::fs::write(&bare, serde_json::to_string(&json!([entry])).unwrap()).unwrap();
        assert_eq!(read_preset_at(&bare).unwrap(), want);

        // A file saved by Notepad or PowerShell starts with a UTF-8 byte order mark.
        let with_bom = dir.0.join("bom.json");
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(serde_json::to_string(&want).unwrap().as_bytes());
        std::fs::write(&with_bom, bytes).unwrap();
        assert_eq!(read_preset_at(&with_bom).unwrap(), want);
    }

    #[test]
    fn read_preset_file_refuses_what_is_not_a_preset() {
        let dir = TempDir::new("read-preset-bad");

        let bad = dir.0.join("bad.json");
        std::fs::write(&bad, "{ not json").unwrap();
        assert!(read_preset_at(&bad).unwrap_err().contains("not valid JSON"));

        let no_chain = dir.0.join("no-chain.json");
        std::fs::write(&no_chain, r#"{ "app": "better-cam" }"#).unwrap();
        assert!(read_preset_at(&no_chain).unwrap_err().contains("no \"chain\""));

        let text = dir.0.join("text.json");
        std::fs::write(&text, r#""just text""#).unwrap();
        assert!(read_preset_at(&text).is_err());

        assert!(read_preset_at(&dir.0.join("missing.json")).unwrap_err().contains("could not read"));
        // A folder is not a file.
        assert_eq!(read_preset_at(&dir.0).unwrap_err(), "this is not a file");
    }

    #[test]
    fn read_preset_file_caps_the_size() {
        let dir = TempDir::new("read-preset-big");
        let path = dir.0.join("big.json");
        // Valid JSON, one byte over the limit: the size is checked before the content is read.
        let padding = " ".repeat(PRESET_FILE_MAX_BYTES as usize - 1);
        std::fs::write(&path, format!("[]{padding}")).unwrap();
        assert!(read_preset_at(&path).unwrap_err().contains("too big"));
        // Exactly at the limit is still read.
        let padding = " ".repeat(PRESET_FILE_MAX_BYTES as usize - 2);
        std::fs::write(&path, format!("[]{padding}")).unwrap();
        assert_eq!(read_preset_at(&path).unwrap(), preset_object(vec![], None, None));
    }

    #[test]
    fn read_preset_file_keeps_camera_and_output() {
        let dir = TempDir::new("read-preset-v2");
        let file = json!({
            "app": "better-cam", "version": 2, "chain": [],
            "camera": { "device": null, "capture": {}, "controls": {}, "auto": {} }, "output": { "fps": 30 }
        });
        let path = dir.0.join("v2.json");
        std::fs::write(&path, serde_json::to_string(&file).unwrap()).unwrap();
        assert_eq!(read_preset_at(&path).unwrap(), file);
    }

    fn write_preset_named(dir: &TempDir, name: &str, preset: &serde_json::Value) {
        write_atomic(&preset_file_in(&dir.0, name).unwrap(), &preset_to_write(preset).unwrap()).unwrap();
    }

    #[test]
    fn a_saved_preset_is_written_whole_and_replaces_the_old_file() {
        let dir = TempDir::new("save-preset");
        let first = json!({ "chain": [{ "id": "a", "kind": "color_filter", "enabled": true, "params": {} }], "camera": { "capture": {} } });
        write_preset_named(&dir, "Studio light", &first);
        let path = dir.0.join("Studio light.json");
        let saved: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved, preset_to_write(&first).unwrap());
        assert!(saved["chain"][0].get("id").is_none());
        assert_eq!(saved["version"], 2);
        // No temp file is left, and a second save replaces the first.
        assert!(!dir.0.join("Studio light.json.tmp").exists());
        write_preset_named(&dir, "Studio light", &json!({ "chain": [] }));
        assert_eq!(read_preset_at(&path).unwrap(), json!({ "app": "better-cam", "version": 1, "chain": [] }));
        assert_eq!(list_in(&dir.0).unwrap(), vec!["Studio light".to_string()]);
    }

    #[test]
    fn the_list_has_the_json_files_only() {
        let dir = TempDir::new("list-presets");
        write_preset_named(&dir, "beta", &json!({ "chain": [] }));
        write_preset_named(&dir, "alpha", &json!({ "chain": [] }));
        // What an interrupted save leaves, and other files.
        std::fs::write(dir.0.join("alpha.json.tmp"), "{").unwrap();
        std::fs::write(dir.0.join("gamma.json.tmp"), "{").unwrap();
        std::fs::write(dir.0.join("notes.txt"), "x").unwrap();
        assert_eq!(list_in(&dir.0).unwrap(), vec!["alpha".to_string(), "beta".to_string()]);
    }

    #[test]
    fn a_preset_is_renamed_unless_the_new_name_is_another_preset() {
        let dir = TempDir::new("rename-preset");
        write_preset_named(&dir, "one", &json!({ "chain": [], "output": { "fps": 30 } }));
        write_preset_named(&dir, "two", &json!({ "chain": [] }));
        let names = || list_in(&dir.0).unwrap();

        // Refusals change nothing.
        assert!(rename_in(&dir.0, "missing", "three").unwrap_err().contains("There is no preset named \"missing\""));
        assert!(rename_in(&dir.0, "one", "two").unwrap_err().contains("A preset named \"two\" already exists"));
        assert!(rename_in(&dir.0, "one", "TWO").unwrap_err().contains("already exists"));
        assert!(rename_in(&dir.0, "one", "a/b").is_err());
        assert!(rename_in(&dir.0, "one", "").is_err());
        assert!(rename_in(&dir.0, "../one", "three").is_err());
        assert_eq!(names(), vec!["one".to_string(), "two".to_string()]);

        // A plain rename keeps the content.
        rename_in(&dir.0, "one", "three").unwrap();
        assert_eq!(names(), vec!["three".to_string(), "two".to_string()]);
        assert_eq!(read_preset_at(&dir.0.join("three.json")).unwrap()["output"], json!({ "fps": 30 }));

        // The same name is not a clash with itself.
        rename_in(&dir.0, "three", "three").unwrap();
        assert_eq!(names(), vec!["three".to_string(), "two".to_string()]);

        // A change of letter case only is allowed.
        rename_in(&dir.0, "three", "Three").unwrap();
        assert_eq!(names(), vec!["Three".to_string(), "two".to_string()]);
        assert_eq!(read_preset_at(&dir.0.join("Three.json")).unwrap()["output"], json!({ "fps": 30 }));
    }
}
