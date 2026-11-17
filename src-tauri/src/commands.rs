use better_cam::api::Service;
use better_cam::camera_controls::ControlInfoDto;
use better_cam::config::{CameraDto, OutputCfg, SourceCfg};
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

/// Every control back to the device's default, Auto on where the device has Auto.
#[tauri::command]
pub async fn reset_camera_controls(
    state: State<'_, AppState>,
    id: String,
) -> Result<Vec<ControlInfoDto>, String> {
    let svc = state.service.clone();
    blocking(move || svc.reset_camera_controls(&id)).await
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
/// raw bytes: a 16-byte header and RGBA pixels (see `better_cam::preview`). A new call replaces the old stream; the
/// webview acknowledges each frame with `preview_ack`.
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

#[tauri::command]
pub fn preview_ack(state: State<AppState>, frame: u32) {
    state.service.preview_ack(frame);
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

/// Downloads NVIDIA's Video Effects redistributable for `arch`, checks that it is signed by NVIDIA and runs it
/// elevated. Progress goes out as `nvidia-install` events `{stage, done, total}`; resolves when the installer has
/// closed, rejects with a sentence otherwise.
#[tauri::command]
pub async fn install_nvidia_sdk(app: AppHandle, arch: String) -> Result<(), String> {
    use tauri::Emitter;

    let url = nvidia::installer_url(&arch).ok_or("Unknown GPU generation.")?;
    #[cfg(debug_assertions)]
    let url = nvidia::test_url_override().unwrap_or(url);
    if NVIDIA_INSTALLING.swap(true, Ordering::SeqCst) {
        return Err("The NVIDIA installer is already running.".into());
    }
    let result = blocking(move || {
        nvidia::run_install(&url, None, |line| {
            if let Some(p) = line.progress() {
                let _ = app.emit("nvidia-install", p);
            } else if let nvidia::ScriptLine::Signature { status, signer } = line {
                eprintln!("[nvidia] installer signature: {status}, signer {signer}");
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

fn preset_path(app: &AppHandle, name: &str) -> Result<PathBuf, String> {
    let ok = !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_'));
    if !ok {
        return Err("preset names may only contain letters, digits, spaces, '-' and '_'".into());
    }
    let dir = app.path().app_data_dir().map_err(|e| e.to_string())?.join("presets");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir.join(format!("{name}.json")))
}

#[tauri::command]
pub fn list_presets(app: AppHandle) -> Result<Vec<String>, String> {
    let dir = preset_path(&app, "x")?.parent().unwrap().to_path_buf();
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .map_err(|e| e.to_string())?
        .flatten()
        .filter_map(|e| e.path().file_stem().map(|s| s.to_string_lossy().into_owned()))
        .collect();
    v.sort();
    Ok(v)
}

/// The preset object of plan 5.4 around a chain list: `{ "app", "version", "chain" }`.
fn preset_object(chain: Vec<serde_json::Value>) -> serde_json::Value {
    serde_json::json!({ "app": "better-cam", "version": 1, "chain": chain })
}

/// What `save_preset` writes: the chain of the given preset object without ids (presets hold the filters only, never
/// device controls and never a camera section).
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
    Ok(preset_object(chain))
}

/// What `load_preset` returns for the file's content: the object format, or the older bare array wrapped in it. Other
/// sections (a future `camera`) are ignored.
fn preset_from_file(content: serde_json::Value) -> Result<serde_json::Value, String> {
    match content {
        serde_json::Value::Array(chain) => Ok(preset_object(chain)),
        serde_json::Value::Object(mut o) => match o.remove("chain") {
            Some(serde_json::Value::Array(chain)) => Ok(preset_object(chain)),
            _ => Err("this preset file has no \"chain\" list".into()),
        },
        _ => Err("this file is not a Better Cam preset".into()),
    }
}

#[tauri::command]
pub fn save_preset(app: AppHandle, name: String, preset: serde_json::Value) -> Result<(), String> {
    let json = serde_json::to_string_pretty(&preset_to_write(&preset)?).map_err(|e| e.to_string())?;
    std::fs::write(preset_path(&app, &name)?, json).map_err(|e| e.to_string())
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_saved_preset_is_the_object_without_ids_or_other_sections() {
        let given = json!({
            "app": "other", "version": 9, "camera": { "gain": 3 },
            "chain": [{ "id": "a1", "kind": "color_filter", "enabled": true, "params": { "saturation": -1 } }]
        });
        let want = json!({
            "app": "better-cam", "version": 1,
            "chain": [{ "kind": "color_filter", "enabled": true, "params": { "saturation": -1 } }]
        });
        assert_eq!(preset_to_write(&given).unwrap(), want);
        assert!(preset_to_write(&json!({ "chain": "no" })).is_err());
        assert!(preset_to_write(&json!([1, 2])).is_err());
        assert!(preset_to_write(&json!({ "chain": [5] })).is_err());
    }

    #[test]
    fn loading_accepts_the_object_and_the_old_bare_array() {
        let entry = json!({ "kind": "color_filter", "enabled": true, "params": {} });
        let want = json!({ "app": "better-cam", "version": 1, "chain": [entry.clone()] });
        assert_eq!(preset_from_file(json!([entry.clone()])).unwrap(), want);
        assert_eq!(preset_from_file(want.clone()).unwrap(), want);
        // A camera section is ignored.
        let with_camera = json!({ "app": "better-cam", "version": 1, "chain": [entry], "camera": { "gain": 3 } });
        assert_eq!(preset_from_file(with_camera).unwrap(), want);
        assert!(preset_from_file(json!({ "app": "better-cam" })).is_err());
        assert!(preset_from_file(json!("text")).is_err());
    }
}
