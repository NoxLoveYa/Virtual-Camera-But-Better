//! Debug-only background test launch: with `BETTER_CAM_TEST_WINDOW=x,y[,width,height]` (physical screen pixels) the
//! main window is placed there and shown without being activated, so an automated run on a second monitor never takes
//! the keyboard focus from whatever the owner is doing. Release builds ignore the variable.

use tauri::utils::config::WindowConfig;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    IsIconic, SetWindowPos, ShowWindow, HWND_NOTOPMOST, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW,
    SW_SHOWNOACTIVATE,
};

/// The name of the environment variable that turns the mode on.
pub const ENV: &str = "BETTER_CAM_TEST_WINDOW";

/// Largest width or height accepted: a typo like `-2400,40,12000000,800` is ignored, not obeyed.
const MAX_SIDE: u32 = 16_384;

/// Where to put the window: its top left corner and, when given, its outer size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placement {
    pub x: i32,
    pub y: i32,
    pub size: Option<(u32, u32)>,
}

/// Parse `x,y` or `x,y,width,height`. Anything else (wrong count, not numbers, a zero or huge size) is `None`.
pub fn parse(value: &str) -> Option<Placement> {
    let parts: Vec<&str> = value.split(',').map(str::trim).collect();
    let side = |s: &str| s.parse::<u32>().ok().filter(|v| (1..=MAX_SIDE).contains(v));
    match parts.as_slice() {
        [x, y] => Some(Placement { x: x.parse().ok()?, y: y.parse().ok()?, size: None }),
        [x, y, w, h] => Some(Placement { x: x.parse().ok()?, y: y.parse().ok()?, size: Some((side(w)?, side(h)?)) }),
        _ => None,
    }
}

/// The placement asked for through the environment; always `None` in a release build.
pub fn requested() -> Option<Placement> {
    if !cfg!(debug_assertions) {
        return None;
    }
    parse(&std::env::var(ENV).ok()?)
}

/// Window definitions for a run in this mode. Showing the window without activation is not enough: wry moves the
/// WebView2 focus into the page while it builds the webview, and that `SetFocus` activates the still hidden top-level
/// window, which then takes the foreground as soon as it is shown. So the windows are created unfocused (tao shows
/// them with `SW_SHOWNOACTIVATE`, wry skips `MoveFocus`), which is what removes the start-up activation; and not
/// focusable (`WS_EX_NOACTIVATE`, which tao keeps across style changes), so a click on the window does not activate
/// it either. Without the mode nothing changes.
pub fn prepare(windows: &mut [WindowConfig], mode: Option<&Placement>) {
    if mode.is_some() {
        for w in windows {
            w.focus = false;
            w.focusable = false;
        }
    }
}

/// Move the window to `placement` and show it (or restore it from minimized) without activating it: no focus, no
/// foreground change, no z-order change, never topmost.
pub fn show_without_focus(hwnd: HWND, placement: &Placement) {
    let mut flags = SWP_NOACTIVATE | SWP_NOZORDER | SWP_SHOWWINDOW;
    let (w, h) = placement.size.map_or((0, 0), |(w, h)| (w as i32, h as i32));
    if placement.size.is_none() {
        flags |= SWP_NOSIZE;
    }
    // SAFETY: `hwnd` is the live main window of this process; both calls only move and show it.
    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
        // HWND_NOTOPMOST is ignored with SWP_NOZORDER; the flag keeps the window out of the always-on-top band
        // should anything else have put it there.
        let _ = SetWindowPos(hwnd, Some(HWND_NOTOPMOST), placement.x, placement.y, w, h, flags);
    }
}

/// Debug builds only, and only with `BETTER_CAM_TEST_TRACE=<file>`: write every activation and focus message of the
/// UI thread, and every system-wide foreground and focus change, with a timestamp and (for the messages that can
/// activate) a stack, so a launch that takes the focus can be traced to the call. Nothing is installed otherwise.
#[cfg(debug_assertions)]
pub mod trace {
    use std::io::Write;
    use std::sync::{Mutex, OnceLock};
    use std::time::Instant;
    use windows::Win32::Foundation::{HMODULE, HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::Threading::{GetCurrentProcessId, GetCurrentThreadId};
    use windows::Win32::UI::Accessibility::{SetWinEventHook, HWINEVENTHOOK};
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, GetAncestor, GetClassNameW, GetWindowThreadProcessId, SetWindowsHookExW, CWPSTRUCT, GA_ROOT,
        HHOOK, WH_CALLWNDPROC, WINDOWPOS, WM_ACTIVATE, WM_ACTIVATEAPP, WM_KILLFOCUS, WM_MOUSEACTIVATE, WM_NCACTIVATE,
        WM_SETFOCUS, WM_SHOWWINDOW, WM_WINDOWPOSCHANGING,
    };

    /// The name of the environment variable that turns the trace on; its value is the file to append to.
    pub const ENV: &str = "BETTER_CAM_TEST_TRACE";

    const EVENT_SYSTEM_FOREGROUND: u32 = 0x0003;
    const EVENT_OBJECT_FOCUS: u32 = 0x8005;
    const WINEVENT_OUTOFCONTEXT: u32 = 0;
    const SWP_NOACTIVATE: u32 = 0x0010;

    static OUT: OnceLock<Mutex<std::fs::File>> = OnceLock::new();
    static START: OnceLock<Instant> = OnceLock::new();

    fn log(line: &str, stack: bool) {
        let (Some(out), Some(start)) = (OUT.get(), START.get()) else { return };
        let Ok(mut f) = out.lock() else { return };
        let _ = writeln!(f, "{:>9.3} {line}", start.elapsed().as_secs_f64());
        if stack {
            let _ = writeln!(f, "{}", std::backtrace::Backtrace::force_capture());
        }
    }

    fn describe(hwnd: HWND) -> String {
        let mut class = [0u16; 128];
        // SAFETY: plain queries on a window handle that may already be gone; failure gives an empty string / zero.
        let (len, pid, root) = unsafe {
            let len = GetClassNameW(hwnd, &mut class);
            let mut pid = 0;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            (len.max(0) as usize, pid, GetAncestor(hwnd, GA_ROOT))
        };
        format!("hwnd={:#x} class={} pid={pid} root={:#x}", hwnd.0 as usize, String::from_utf16_lossy(&class[..len]), root.0 as usize)
    }

    /// The messages that can move the keyboard focus or the active window, seen by the UI thread (the Tauri window and
    /// the wry child windows).
    unsafe extern "system" fn call_wnd_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code >= 0 {
            // SAFETY: for `WH_CALLWNDPROC` with `code >= 0`, `lparam` points to a valid `CWPSTRUCT`.
            let m = unsafe { &*(lparam.0 as *const CWPSTRUCT) };
            let (name, stack) = match m.message {
                WM_ACTIVATE => ("WM_ACTIVATE", (m.wParam.0 & 0xFFFF) != 0),
                WM_ACTIVATEAPP => ("WM_ACTIVATEAPP", m.wParam.0 != 0),
                WM_NCACTIVATE => ("WM_NCACTIVATE", m.wParam.0 != 0),
                WM_SETFOCUS => ("WM_SETFOCUS", true),
                WM_KILLFOCUS => ("WM_KILLFOCUS", false),
                WM_MOUSEACTIVATE => ("WM_MOUSEACTIVATE", false),
                WM_SHOWWINDOW => ("WM_SHOWWINDOW", false),
                WM_WINDOWPOSCHANGING => ("WM_WINDOWPOSCHANGING", false),
                _ => ("", false),
            };
            if !name.is_empty() {
                let mut detail = format!("wparam={:#x} lparam={:#x}", m.wParam.0, m.lParam.0);
                if m.message == WM_WINDOWPOSCHANGING {
                    // SAFETY: for this message `lParam` points to a valid `WINDOWPOS`.
                    let p = unsafe { &*(m.lParam.0 as *const WINDOWPOS) };
                    detail = format!("flags={:#x} noactivate={} pos=({},{}) size={}x{}", p.flags.0, p.flags.0 & SWP_NOACTIVATE != 0, p.x, p.y, p.cx, p.cy);
                }
                log(&format!("{name} {} {detail}", describe(m.hwnd)), stack);
            }
        }
        // SAFETY: passes the hook chain on unchanged.
        unsafe { CallNextHookEx(None::<HHOOK>, code, wparam, lparam) }
    }

    /// Foreground changes anywhere and focus changes in this process's windows, including the WebView2 windows of
    /// the browser process that sit inside the main window.
    unsafe extern "system" fn win_event(_: HWINEVENTHOOK, event: u32, hwnd: HWND, _: i32, _: i32, _: u32, _: u32) {
        let mut root_pid = 0;
        // SAFETY: plain queries on a window handle that may already be gone.
        unsafe { GetWindowThreadProcessId(GetAncestor(hwnd, GA_ROOT), Some(&mut root_pid)) };
        let ours = root_pid == unsafe { GetCurrentProcessId() };
        match event {
            EVENT_SYSTEM_FOREGROUND => log(&format!("FOREGROUND {} ours={ours}", describe(hwnd)), false),
            EVENT_OBJECT_FOCUS if ours => log(&format!("FOCUS {}", describe(hwnd)), false),
            _ => {}
        }
    }

    /// Install the hooks on the calling thread, which must be the UI thread and must run a message loop afterwards.
    /// Does nothing without the environment variable.
    pub fn install() {
        let Some(path) = std::env::var_os(ENV) else { return };
        let Ok(file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) else { return };
        let _ = OUT.set(Mutex::new(file));
        let _ = START.set(Instant::now());
        // SAFETY: both hooks are process-lifetime, installed once from the UI thread; their callbacks only read.
        unsafe {
            let _ = SetWindowsHookExW(WH_CALLWNDPROC, Some(call_wnd_proc), None, GetCurrentThreadId());
            for event in [EVENT_SYSTEM_FOREGROUND, EVENT_OBJECT_FOCUS] {
                SetWinEventHook(event, event, None::<HMODULE>, Some(win_event), 0, 0, WINEVENT_OUTOFCONTEXT);
            }
        }
        log(&format!("TRACE START pid={} thread={}", std::process::id(), unsafe { GetCurrentThreadId() }), false);
    }

    /// A line for the caller's own events (what the app did, e.g. "show_main").
    pub fn note(text: &str) {
        log(&format!("NOTE {text}"), false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_position_alone_is_accepted() {
        assert_eq!(parse("-2400,40"), Some(Placement { x: -2400, y: 40, size: None }));
        assert_eq!(parse(" 10 , -5 "), Some(Placement { x: 10, y: -5, size: None }));
    }

    #[test]
    fn a_position_and_size_is_accepted() {
        assert_eq!(parse("-2400,40,1200,800"), Some(Placement { x: -2400, y: 40, size: Some((1200, 800)) }));
        assert_eq!(parse("0, 0, 900, 600"), Some(Placement { x: 0, y: 0, size: Some((900, 600)) }));
    }

    #[test]
    fn the_mode_creates_windows_that_cannot_take_the_focus() {
        let mut windows = vec![WindowConfig::default(), WindowConfig::default()];
        assert!(windows.iter().all(|w| w.focus && w.focusable), "the defaults are what a normal run uses");
        prepare(&mut windows, Some(&Placement { x: 0, y: 0, size: None }));
        assert!(windows.iter().all(|w| !w.focus && !w.focusable));
    }

    #[test]
    fn without_the_mode_the_windows_are_untouched() {
        let mut windows = vec![WindowConfig::default()];
        prepare(&mut windows, None);
        assert!(windows[0].focus && windows[0].focusable);
    }

    #[test]
    fn garbage_is_ignored() {
        for bad in ["", "1", "1,2,3", "1,2,3,4,5", "a,b", "1,2,x,4", "1.5,2", "1,2,0,800", "1,2,-5,800", "1,2,99999,800", "1,,2,3"] {
            assert_eq!(parse(bad), None, "{bad:?}");
        }
    }
}
