//! Debug-only background test launch: with `BETTER_CAM_TEST_WINDOW=x,y[,width,height]` (physical screen pixels) the
//! main window is placed there and shown without being activated, so an automated run on a second monitor never takes
//! the keyboard focus from whatever the owner is doing. Release builds ignore the variable.

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
    fn garbage_is_ignored() {
        for bad in ["", "1", "1,2,3", "1,2,3,4,5", "a,b", "1,2,x,4", "1.5,2", "1,2,0,800", "1,2,-5,800", "1,2,99999,800", "1,,2,3"] {
            assert_eq!(parse(bad), None, "{bad:?}");
        }
    }
}
