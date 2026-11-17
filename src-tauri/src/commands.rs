use better_cam::api::Service;
use better_cam::camera_controls::ControlInfoDto;
use better_cam::config::{CameraDto, OutputCfg, SourceCfg};
use better_cam::preview::Sink;
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

#[tauri::command]
pub fn save_preset(app: AppHandle, name: String, chain: Vec<serde_json::Value>) -> Result<(), String> {
    let json = serde_json::to_string_pretty(&chain).map_err(|e| e.to_string())?;
    std::fs::write(preset_path(&app, &name)?, json).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn load_preset(app: AppHandle, name: String) -> Result<Vec<serde_json::Value>, String> {
    let s = std::fs::read_to_string(preset_path(&app, &name)?).map_err(|e| e.to_string())?;
    serde_json::from_str(&s).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn delete_preset(app: AppHandle, name: String) -> Result<(), String> {
    std::fs::remove_file(preset_path(&app, &name)?).map_err(|e| e.to_string())
}
