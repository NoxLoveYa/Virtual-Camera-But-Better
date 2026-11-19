#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;

use better_cam::engine::EngineEvent;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Listener, Manager, RunEvent, WindowEvent};
use tauri_plugin_autostart::MacosLauncher;
use windows::Win32::Foundation::HWND;

/// Tell the engine whether somebody can see the window: shown and not minimized. Hidden to the tray and minimized
/// both count as hidden (`deactivate_when_not_showing`).
fn report_visibility(app: &AppHandle) {
    let Some(w) = app.get_webview_window("main") else {
        return;
    };
    // Either call may fail while the window is being created or destroyed: then the last report stands.
    let (Ok(shown), Ok(minimized)) = (w.is_visible(), w.is_minimized()) else {
        return;
    };
    let visible = shown && !minimized;
    if app.state::<commands::AppState>().service.set_window_visible(visible) {
        eprintln!("[window] {}", if visible { "visible" } else { "hidden or minimized" });
    }
}

/// Not every change of the window comes with an event the app sees: a window hidden or shown by something else
/// (`ShowWindow` from another program, a tray helper) sends neither a size nor a focus event, and then the engine
/// would keep the camera on for a hidden window, or close it under a visible one. So the state is also read twice a
/// second. The report only does anything when the state differs from the last one.
fn watch_visibility(app: AppHandle) {
    let _ = std::thread::Builder::new().name("window visibility".into()).spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_millis(500));
        report_visibility(&app);
    });
}

/// Every path that surfaces the main window (start-up, tray, second launch) goes through here. With the debug-only
/// `BETTER_CAM_TEST_WINDOW` the window is placed and shown without activation and nothing takes the focus.
fn show_main(app: &AppHandle) {
    #[cfg(debug_assertions)]
    better_cam::test_window::trace::note("show_main");
    if let Some(w) = app.get_webview_window("main") {
        if let Some(placement) = better_cam::test_window::requested() {
            match w.hwnd() {
                Ok(hwnd) => better_cam::test_window::show_without_focus(HWND(hwnd.0 as _), &placement),
                Err(e) => eprintln!("[test window] no window handle: {e}"),
            }
        } else {
            let _ = w.show();
            let _ = w.unminimize();
            let _ = w.set_focus();
        }
    }
    report_visibility(app);
}

/// How long an exit waits for the page to say it saved (`save-flushed`): a page that is gone or stuck must not hold the
/// quit back.
const SAVE_WAIT: std::time::Duration = std::time::Duration::from_millis(400);
/// An exit has asked the page to save and waits for the answer.
static SAVE_ASKED: AtomicBool = AtomicBool::new(false);
/// The page answered (or the wait ran out): the exit that follows goes through.
static SAVE_DONE: AtomicBool = AtomicBool::new(false);

/// Asks the page to write what it still holds (`flush-save`; the throttled save in `src/ui/saver.ts` can lag 300 ms
/// behind the last change) and waits for its answer, at most `SAVE_WAIT`. Nothing the page fires on its own covers a
/// quit from the tray or a hide to the tray.
fn ask_page_to_save(app: &AppHandle) {
    let (tx, rx) = std::sync::mpsc::channel();
    let id = app.once("save-flushed", move |_| {
        let _ = tx.send(());
    });
    if app.emit("flush-save", ()).is_ok() {
        let _ = rx.recv_timeout(SAVE_WAIT);
    }
    app.unlisten(id);
}

/// The one place every exit passes (tray Quit, the window closing without close-to-tray, any `app.exit`): the first
/// request is held back, the page saves, then the same exit is requested again and goes through. Worker thread: the
/// event loop keeps running (the page's answer needs it).
fn save_before_exit(app: &AppHandle, api: &tauri::ExitRequestApi, code: Option<i32>) {
    if SAVE_DONE.load(Ordering::SeqCst) {
        return;
    }
    api.prevent_exit();
    if SAVE_ASKED.swap(true, Ordering::SeqCst) {
        return;
    }
    let app = app.clone();
    let spawned = std::thread::Builder::new().name("save before exit".into()).spawn({
        let app = app.clone();
        move || {
            ask_page_to_save(&app);
            SAVE_DONE.store(true, Ordering::SeqCst);
            app.exit(code.unwrap_or(0));
        }
    });
    if spawned.is_err() {
        SAVE_DONE.store(true, Ordering::SeqCst);
        app.exit(code.unwrap_or(0));
    }
}

/// Engine events to the webview (plan 5.6).
fn forward_events(app: &AppHandle) -> Arc<dyn better_cam::engine::EventSink> {
    let app = app.clone();
    Arc::new(move |event: EngineEvent| {
        let _ = match event {
            EngineEvent::State(s) => app.emit("engine-state", s),
            EngineEvent::Error(message) => app.emit("engine-error", message),
            EngineEvent::Stats(s) => app.emit("stats", s),
            EngineEvent::DevicesChanged => app.emit("devices-changed", ()),
            EngineEvent::Controls(c) => app.emit("controls-changed", c),
            EngineEvent::FilterError(e) => app.emit("filter-error", e),
            EngineEvent::FilterStatus(s) => app.emit("filter-status", s),
            EngineEvent::FormatFallback(f) => app.emit("format-fallback", f),
            EngineEvent::Paused(p) => app.emit("engine-paused", p),
            EngineEvent::AutoStatus(s) => app.emit("auto-status", s),
        };
    })
}

/// `--vcam-install` / `--vcam-uninstall`: the elevated helper of the virtual camera. Runs before the Tauri builder and
/// the single-instance plugin, opens no window and exits with the helper's exit code.
fn run_helper_if_asked() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(parsed) = better_cam::vcam_admin::parse_helper_args(&args) else {
        return;
    };
    let code = match parsed {
        Ok(cmd) => better_cam::vcam_admin::run_helper(cmd),
        Err(e) => {
            eprintln!("{e}");
            better_cam::vcam_admin::EXIT_USAGE
        }
    };
    std::process::exit(code);
}

fn main() {
    run_helper_if_asked();
    // Before the window exists, so its creation is part of the trace.
    #[cfg(debug_assertions)]
    better_cam::test_window::trace::install();
    let mut context = tauri::generate_context!();
    better_cam::test_window::prepare(&mut context.config_mut().app.windows, better_cam::test_window::requested().as_ref());
    let builder = tauri::Builder::default();
    // A relaunch while the app sits in the tray should surface that window, not start a second instance.
    // Debug builds skip this so `tauri dev` still works while an installed copy is running.
    #[cfg(not(debug_assertions))]
    let builder = builder.plugin(tauri_plugin_single_instance::init(|app, _, _| show_main(app)));

    builder
        .plugin(tauri_plugin_autostart::init(MacosLauncher::LaunchAgent, Some(vec![commands::AUTOSTART_ARG])))
        .manage(commands::AppState::default())
        .setup(|app| {
            let show = MenuItem::with_id(app, "show", "Show Better Cam", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let mut tray = TrayIconBuilder::new()
                .tooltip("Better Cam")
                .menu(&Menu::with_items(app, &[&show, &quit])?)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, e| match e.id.as_ref() {
                    "show" => show_main(app),
                    // The engine is shut down in `RunEvent::Exit` below: camera released, frames.bin closed.
                    // `RunEvent::ExitRequested` holds the exit back until the page has saved.
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, e| {
                    if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e {
                        show_main(tray.app_handle());
                    }
                });
            if let Some(icon) = app.default_window_icon() {
                tray = tray.icon(icon.clone());
            }
            tray.build(app)?;

            // The engine reports to the webview; creating it (D3D11 device, shaders) happens off this thread.
            // The engine's filter chains find `ext:` filters through the registry; it is installed before the engine can
            // start a session (the folder is read further down).
            better_cam::ext::install_registry(app.state::<commands::AppState>().extensions.clone());
            let service = app.state::<commands::AppState>().service.clone();
            service.set_sink(forward_events(app.handle()));
            std::thread::spawn(move || {
                if let Err(e) = service.init() {
                    eprintln!("[engine] cannot start the engine: {e}");
                }
            });

            // The extensions folder is read now (so the page's first `list_extensions` is complete) and watched once a
            // second; a change reaches the page as `extensions-changed`.
            match app.path().app_data_dir() {
                Ok(dir) => {
                    let handle = app.handle().clone();
                    let sink: better_cam::ext::Sink = Arc::new(move |list| {
                        let _ = handle.emit("extensions-changed", list);
                    });
                    app.state::<commands::AppState>().extensions.start(dir.join("extensions"), dir.join("extensions-state.json"), Some(sink));
                }
                Err(e) => eprintln!("[ext] no app data folder, extensions are off: {e}"),
            }

            // The window starts hidden; a login launch stays in the tray, a manual one shows it.
            if commands::launched_at_startup() {
                report_visibility(app.handle());
            } else {
                show_main(app.handle());
            }
            watch_visibility(app.handle().clone());
            Ok(())
        })
        .on_window_event(|window, event| match event {
            WindowEvent::CloseRequested { api, .. } => {
                api.prevent_close();
                if window.state::<commands::AppState>().close_to_tray.load(Ordering::Relaxed) {
                    // A hidden page keeps its timers, but the save should not wait on them: asked here, no answer needed.
                    let _ = window.emit("flush-save", ());
                    let _ = window.hide();
                    report_visibility(window.app_handle());
                } else {
                    // Closing the only window quits. Exit while the page is still there to save (the window would
                    // be destroyed first otherwise): `RunEvent::ExitRequested` does the asking.
                    window.app_handle().exit(0);
                }
            }
            // Minimizing and restoring change the size and the focus; hiding and showing go through the code above.
            WindowEvent::Resized(_) | WindowEvent::Focused(_) | WindowEvent::Moved(_) => {
                report_visibility(window.app_handle());
                // Back in front: a camera that was waited for (Windows privacy settings changed, the other app closed)
                // is tried at once.
                if matches!(event, WindowEvent::Focused(true)) {
                    window.state::<commands::AppState>().service.retry_now();
                }
            }
            _ => {}
        })
        .invoke_handler(tauri::generate_handler![
            commands::list_cameras,
            commands::start_engine,
            commands::stop_engine,
            commands::set_source,
            commands::set_output,
            commands::set_chain,
            commands::set_pause,
            commands::get_camera_controls,
            commands::set_camera_control,
            commands::reset_camera_controls,
            commands::set_control_mode,
            commands::set_auto_config,
            commands::resume_auto,
            commands::set_preview,
            commands::set_preview_point,
            commands::preview_ack,
            commands::get_autostart,
            commands::set_autostart,
            commands::launched_at_startup,
            commands::set_close_to_tray,
            commands::list_presets,
            commands::save_preset,
            commands::load_preset,
            commands::delete_preset,
            commands::read_preset_file,
            commands::open_camera_settings,
            commands::vcam_status,
            commands::vcam_install,
            commands::vcam_uninstall,
            commands::pick_file,
            commands::nvidia_status,
            commands::nvidia_gpu,
            commands::install_nvidia_sdk,
            commands::list_extensions,
            commands::reload_extensions,
            commands::set_extension_enabled,
            commands::open_extensions_folder,
        ])
        .build(context)
        .expect("error while building Better Cam")
        .run(|app, event| match event {
            // Every exit (the tray's Quit, the window closing, `app.exit`) is held back once for the page to save.
            RunEvent::ExitRequested { api, code, .. } => save_before_exit(app, &api, code),
            RunEvent::Exit => app.state::<commands::AppState>().service.shutdown(),
            _ => {}
        });
}
