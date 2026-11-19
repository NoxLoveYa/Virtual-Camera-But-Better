//! Windows camera privacy settings (plan 4.9): "Let desktop apps access your camera" and the master "Camera access"
//! switch. When either is off, Windows refuses to hand a desktop app the camera: the open fails (usually with
//! `E_ACCESSDENIED`, which is also what a camera held by another app can look like) or the stream never starts.
//!
//! The switches live in the capability access manager's consent store, as strings `Allow` or `Deny`:
//! `HKCU\...\ConsentStore\webcam` (the master switch of the user), `HKLM\...\ConsentStore\webcam` (the device-wide
//! one) and `HKCU\...\ConsentStore\webcam\NonPackaged` (desktop apps). A value that is missing counts as allowed.

use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::System::Registry::{
    RegGetValueW, HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/// What the "Open camera settings" button launches.
pub const SETTINGS_URI: &str = "ms-settings:privacy-webcam";

const STORE: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\webcam";

/// Which switch keeps desktop apps from the camera.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Blocked {
    /// The master switch ("Camera access"), for the user or for the device.
    AllApps,
    /// Only "Let desktop apps access your camera".
    DesktopApps,
    /// A group policy ("Let Windows apps access the camera" forced off): no switch in Settings changes it.
    Policy,
}

/// The switches as the registry holds them (`None`: no such value).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Switches {
    pub master_user: Option<String>,
    pub master_device: Option<String>,
    pub desktop_apps: Option<String>,
    /// `HKLM\SOFTWARE\Policies\Microsoft\Windows\AppPrivacy\LetAppsAccessCamera`: 0 user's choice, 1 force allow,
    /// 2 force deny.
    pub policy: Option<u32>,
}

fn denied(v: &Option<String>) -> bool {
    v.as_deref().is_some_and(|s| s.trim().eq_ignore_ascii_case("deny"))
}

/// Which switch blocks desktop apps, if any. A policy wins (nothing in Settings undoes it), then the master
/// switches, then the desktop-apps one.
pub fn blocked_by(s: &Switches) -> Option<Blocked> {
    if s.policy == Some(2) {
        Some(Blocked::Policy)
    } else if denied(&s.master_user) || denied(&s.master_device) {
        Some(Blocked::AllApps)
    } else if denied(&s.desktop_apps) {
        Some(Blocked::DesktopApps)
    } else {
        None
    }
}

/// A string value of the consent store, `None` when the key or value is missing.
fn read(hive: HKEY, subkey: &str) -> Option<String> {
    let key = HSTRING::from(subkey);
    let mut buf = [0u16; 32];
    let mut bytes = std::mem::size_of_val(&buf) as u32;
    // SAFETY: `buf` and `bytes` live through the call and `bytes` is the size of `buf`; the key is a valid string.
    let r = unsafe {
        RegGetValueW(
            hive,
            PCWSTR(key.as_ptr()),
            w!("Value"),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut _),
            Some(&mut bytes),
        )
    };
    if r.0 != 0 {
        return None;
    }
    let chars = (bytes as usize / 2).min(buf.len());
    let text = String::from_utf16_lossy(&buf[..chars]);
    Some(text.trim_end_matches('\0').to_string())
}

/// A DWORD value of the camera privacy policy, `None` when the key or value is missing.
fn read_policy() -> Option<u32> {
    let key = HSTRING::from(r"SOFTWARE\Policies\Microsoft\Windows\AppPrivacy");
    let mut value = 0u32;
    let mut bytes = std::mem::size_of::<u32>() as u32;
    // SAFETY: `value` and `bytes` live through the call and `bytes` is the size of `value`.
    let r = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(key.as_ptr()),
            w!("LetAppsAccessCamera"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut value as *mut u32 as *mut _),
            Some(&mut bytes),
        )
    };
    (r.0 == 0).then_some(value)
}

/// The switches as they are now.
pub fn read_switches() -> Switches {
    Switches {
        master_user: read(HKEY_CURRENT_USER, STORE),
        master_device: read(HKEY_LOCAL_MACHINE, STORE),
        desktop_apps: read(HKEY_CURRENT_USER, &format!(r"{STORE}\NonPackaged")),
        policy: read_policy(),
    }
}

/// Which switch blocks desktop apps right now, if any.
pub fn desktop_access_blocked() -> Option<Blocked> {
    blocked_by(&read_switches())
}

/// What the app says when [`Blocked`]: plain words, the first sentence is the engine's `reconnecting` reason.
pub fn reason(b: Blocked) -> String {
    match b {
        Blocked::AllApps => "camera access is turned off in Windows privacy settings".into(),
        Blocked::DesktopApps => {
            "camera access for desktop apps is turned off in Windows privacy settings".into()
        }
        Blocked::Policy => "camera access is turned off by a Windows policy (privacy settings)".into(),
    }
}

/// The name of the environment variable that turns the debug-only simulated refusal on (see [`test_access_denied`]).
pub const TEST_ENV: &str = "BETTER_CAM_TEST_ACCESS_DENIED";

/// `true` while the file the variable names exists.
fn flag_file_exists(var: Option<std::ffi::OsString>) -> bool {
    var.is_some_and(|p| !p.is_empty() && std::path::Path::new(&p).exists())
}

/// Debug builds only, like `BETTER_CAM_TEST_WINDOW`: while the file named by `BETTER_CAM_TEST_ACCESS_DENIED` exists,
/// opening the camera fails with `E_ACCESSDENIED`, the way Windows refuses a desktop app. Some Windows builds do not
/// enforce the consent store values for a classic desktop app at all (this check was written on one), and the banner
/// and the recovery are checked live with this. Always `false` in a release build.
pub fn test_access_denied() -> bool {
    cfg!(debug_assertions) && flag_file_exists(std::env::var_os(TEST_ENV))
}

/// Open Windows Settings on the camera privacy page. Does not wait.
pub fn open_settings() -> Result<(), String> {
    // SAFETY: plain call with static strings; the shell starts the handler of the `ms-settings` scheme.
    let r = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            &HSTRING::from(SETTINGS_URI),
            None,
            None,
            SW_SHOWNORMAL,
        )
    };
    // ShellExecute reports success with a value above 32.
    if r.0 as isize > 32 {
        Ok(())
    } else {
        Err(format!("Windows Settings did not open (code {})", r.0 as isize))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sw(user: Option<&str>, device: Option<&str>, desktop: Option<&str>) -> Switches {
        let s = |v: Option<&str>| v.map(String::from);
        Switches {
            master_user: s(user),
            master_device: s(device),
            desktop_apps: s(desktop),
            policy: None,
        }
    }

    #[test]
    fn a_policy_that_forces_the_camera_off_wins_and_other_policy_values_do_not_block() {
        let mut s = sw(Some("Allow"), Some("Allow"), Some("Allow"));
        s.policy = Some(2);
        assert_eq!(blocked_by(&s), Some(Blocked::Policy));
        for v in [0, 1, 7] {
            s.policy = Some(v);
            assert_eq!(blocked_by(&s), None, "LetAppsAccessCamera = {v}");
        }
        assert!(reason(Blocked::Policy).contains("privacy"));
    }

    #[test]
    fn nothing_blocks_when_everything_allows_or_is_missing() {
        assert_eq!(blocked_by(&sw(Some("Allow"), Some("Allow"), Some("Allow"))), None);
        assert_eq!(blocked_by(&sw(None, None, None)), None);
        assert_eq!(blocked_by(&sw(Some("Allow"), Some("Allow"), Some("Prompt"))), None);
    }

    #[test]
    fn each_switch_blocks_and_the_master_wins() {
        assert_eq!(
            blocked_by(&sw(Some("Allow"), Some("Allow"), Some("Deny"))),
            Some(Blocked::DesktopApps)
        );
        assert_eq!(
            blocked_by(&sw(Some("Deny"), Some("Allow"), Some("Allow"))),
            Some(Blocked::AllApps)
        );
        assert_eq!(
            blocked_by(&sw(Some("Allow"), Some("deny"), Some("Deny"))),
            Some(Blocked::AllApps)
        );
        assert_eq!(blocked_by(&sw(None, None, Some(" DENY "))), Some(Blocked::DesktopApps));
    }

    #[test]
    fn the_simulated_refusal_needs_its_file() {
        let dir = vcam_protocol::testdir::TestDir::new("bettercam-priv");
        let flag = dir.join("deny.flag");
        assert!(!flag_file_exists(None));
        assert!(!flag_file_exists(Some("".into())));
        assert!(!flag_file_exists(Some(flag.clone().into_os_string())));
        std::fs::write(&flag, b"x").unwrap();
        assert!(flag_file_exists(Some(flag.clone().into_os_string())));
        std::fs::remove_file(&flag).unwrap();
        assert!(!flag_file_exists(Some(flag.into_os_string())));
        assert!(!test_access_denied(), "the variable is not set in the tests");
    }

    #[test]
    fn the_settings_button_opens_the_camera_privacy_page() {
        assert_eq!(SETTINGS_URI, "ms-settings:privacy-webcam");
    }

    #[test]
    fn this_machine_reads_without_panicking() {
        // The values differ per machine; the read itself must work.
        let s = read_switches();
        eprintln!("camera consent switches here: {s:?}");
    }
}
