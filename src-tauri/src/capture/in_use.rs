//! Which programs use a camera right now, for the banner that says who holds it when Better Cam cannot open the
//! camera (`Failure::in_use`).
//!
//! Two sources, in this order, merged by [`merge`]:
//!
//! 1. The capability access manager's consent store, the same one [`super::privacy`] reads:
//!    `{HKCU,HKLM}\...\ConsentStore\webcam` has one subkey per packaged app (the package family name) and, under
//!    `NonPackaged`, one per desktop program (its full exe path with `\` written as `#`). Each has `LastUsedTimeStart`
//!    and `LastUsedTimeStop` (QWORD FILETIMEs); a program that is using a camera now has a start and a stop of 0.
//! 2. The camera service's event log ([`super::in_use_log`]), which names the process that streams the physical camera
//!    Better Cam wants, checked against the running processes.
//!
//! What the registry record does not say: *which* camera. A program that reads Better Cam's own virtual camera is in
//! the list like one that holds the physical camera, and with several cameras the program may hold another one. The
//! engine says "may be in use by" when a consumer is attached to the virtual camera (the one case it can see), see
//! [`reason`]. Windows also does not write the record on every PC (it is missing here for every webcam program), and a
//! program that is killed while it uses the camera can leave a start without a stop behind. The log names the camera
//! and checks the process, but cannot always tell two clients that opened the camera together; that is a doubt as
//! well. An empty list gives the words "another app".
//!
//! The parsing is separate from the registry and log reads so it can be tested without either.

use windows::core::{HSTRING, PCWSTR, PWSTR};
use windows::Win32::System::Registry::{
    RegCloseKey, RegEnumKeyExW, RegGetValueW, RegOpenKeyExW, HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE,
    KEY_READ, RRF_RT_REG_QWORD,
};

const STORE: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\webcam";

/// The name of the environment variable that moves the lookup to another `HKCU` key (debug builds only, like
/// `BETTER_CAM_TEST_WINDOW`), so a live check can write records of the shape Windows writes without touching the
/// real consent store. The key has the layout of `ConsentStore\webcam`.
pub const TEST_ROOT_ENV: &str = "BETTER_CAM_TEST_CONSENT_ROOT";

/// One subkey of the consent store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// The subkey's name: a package family name or an exe path with `#` for `\`.
    pub key: String,
    pub packaged: bool,
    pub start: u64,
    pub stop: u64,
}

/// A program uses a camera now when it has started and not stopped.
pub fn using_now(start: u64, stop: u64) -> bool {
    start != 0 && stop == 0
}

/// `C:#Users#x#AppData#Local#Discord#app-1.0.9261#Discord.exe` is `Discord.exe`.
pub fn exe_name(key: &str) -> Option<String> {
    let name = key.rsplit('#').next()?.trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// A readable name for a packaged app from its package family name: the part before the first `_`, without the
/// publisher in front (`Microsoft.WindowsCamera_8wekyb3d8bbwe` is `WindowsCamera`).
pub fn package_name(key: &str) -> String {
    let name = key.split('_').next().unwrap_or(key).trim();
    match name.split_once('.') {
        Some((_, rest)) if !rest.is_empty() => rest.to_string(),
        _ => name.to_string(),
    }
}

/// The key a program's own exe path has in the store.
pub fn key_of_path(path: &str) -> String {
    path.replace('\\', "#")
}

/// The programs that use a camera now, most recently started first, once each (by name, ignoring case; the same
/// program in the user's and in the device's store, or in two versions of its folder, counts once). `own` is this
/// process's exe path as a key ([`key_of_path`]): Better Cam is never named.
pub fn users(records: &[Record], own: Option<&str>) -> Vec<String> {
    timed_users(records, own).into_iter().map(|(_, name)| name).collect()
}

/// [`users`] with the start of each program's record (FILETIME), most recent first.
pub fn timed_users(records: &[Record], own: Option<&str>) -> Vec<(u64, String)> {
    let mut live: Vec<(u64, String)> = records
        .iter()
        .filter(|r| using_now(r.start, r.stop))
        .filter(|r| r.packaged || !own.is_some_and(|o| o.eq_ignore_ascii_case(&r.key)))
        .filter_map(|r| {
            let name = if r.packaged { Some(package_name(&r.key)) } else { exe_name(&r.key) };
            name.filter(|n| !n.is_empty()).map(|n| (r.start, n))
        })
        .collect();
    live.sort_by_key(|(start, _)| std::cmp::Reverse(*start));
    let mut out: Vec<(u64, String)> = Vec::new();
    for (start, name) in live {
        if !out.iter().any(|(_, n)| n.eq_ignore_ascii_case(&name)) {
            out.push((start, name));
        }
    }
    out
}

/// The programs found for a camera in use.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CameraUsers {
    pub names: Vec<String>,
    /// The list may include a program that does not hold the camera (the `apps_maybe` of the state event).
    pub maybe: bool,
}

/// The registry's programs and the log's, as one list: the union, once per name (ignoring case), the most recent
/// start first. `maybe` when the log doubts its attribution, or a program only the registry names may be a reader of
/// Better Cam's virtual camera (`vcam_consumer`: one is attached).
pub fn merge(registry: Vec<(u64, String)>, log: super::in_use_log::Holders, vcam_consumer: bool) -> CameraUsers {
    let in_log = |name: &str| log.names.iter().any(|(_, n)| n.eq_ignore_ascii_case(name));
    let registry_only = registry.iter().any(|(_, n)| !in_log(n));
    let maybe = (!log.names.is_empty() && log.maybe) || (registry_only && vcam_consumer);
    let mut all: Vec<(u64, String)> = registry.into_iter().chain(log.names).collect();
    all.sort_by_key(|(start, _)| std::cmp::Reverse(*start)); // stable: the registry's entry wins a tie
    let mut names: Vec<String> = Vec::new();
    for (_, name) in all {
        if !names.iter().any(|n| n.eq_ignore_ascii_case(&name)) {
            names.push(name);
        }
    }
    CameraUsers { names, maybe }
}

/// The names as they read in a sentence: `Discord.exe`, `Discord.exe and chrome.exe`,
/// `Discord.exe, chrome.exe and 2 more`; `another app` for none.
pub fn list_text(names: &[String]) -> String {
    match names {
        [] => "another app".into(),
        [a] => a.clone(),
        [a, b] => format!("{a} and {b}"),
        [a, b, rest @ ..] => format!("{a}, {b} and {} more", rest.len()),
    }
}

/// The reason sentence of the `engine-state` event for a camera in use (the frontend builds the visible text from the
/// list; this is for the log and for a reader of `reason` alone). `maybe`: the list may include a program that only
/// reads Better Cam's virtual camera.
pub fn reason(names: &[String], maybe: bool) -> String {
    let verb = if maybe && !names.is_empty() { "may be" } else { "is" };
    format!("camera {verb} in use by {}", list_text(names))
}

/// The key of this process's exe in the store, `None` when Windows cannot say.
fn own_key() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    Some(key_of_path(&exe.to_string_lossy()))
}

/// The `HKCU` key that replaces the consent store in a debug build with [`TEST_ROOT_ENV`] set.
fn test_root() -> Option<String> {
    if !cfg!(debug_assertions) {
        return None;
    }
    std::env::var(TEST_ROOT_ENV).ok().filter(|s| !s.is_empty())
}

/// A QWORD value of a subkey, `None` when it is missing.
fn read_qword(parent: HKEY, sub: &HSTRING, value: PCWSTR) -> Option<u64> {
    let mut data = 0u64;
    let mut bytes = std::mem::size_of::<u64>() as u32;
    // SAFETY: `data` and `bytes` live through the call and `bytes` is the size of `data`; `sub` and `value` are valid
    // NUL-terminated strings.
    let r = unsafe {
        RegGetValueW(
            parent,
            PCWSTR(sub.as_ptr()),
            value,
            RRF_RT_REG_QWORD,
            None,
            Some(&mut data as *mut u64 as *mut _),
            Some(&mut bytes),
        )
    };
    (r.0 == 0).then_some(data)
}

/// The records of the subkeys of `hive\path`; none when the key is missing. `skip` is a subkey that is not a program
/// (`NonPackaged`, in the packaged list).
fn read_under(hive: HKEY, path: &str, packaged: bool, skip: Option<&str>) -> Vec<Record> {
    let path = HSTRING::from(path);
    let mut key = HKEY::default();
    // SAFETY: `path` is a valid string and `key` receives the handle, closed below.
    let r = unsafe { RegOpenKeyExW(hive, PCWSTR(path.as_ptr()), None, KEY_READ, &mut key) };
    if r.0 != 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    for index in 0u32.. {
        // Key names are at most 255 characters.
        let mut name = [0u16; 512];
        let mut len = name.len() as u32;
        // SAFETY: `name` and `len` live through the call and `len` is the size of `name`.
        let r = unsafe {
            RegEnumKeyExW(key, index, Some(PWSTR(name.as_mut_ptr())), &mut len, None, None, None, None)
        };
        if r.0 != 0 {
            break; // no more subkeys (or an error: the list is what was read)
        }
        let text = String::from_utf16_lossy(&name[..(len as usize).min(name.len())]);
        if skip.is_some_and(|s| s.eq_ignore_ascii_case(&text)) {
            continue;
        }
        let sub = HSTRING::from(text.as_str());
        let start = read_qword(key, &sub, windows::core::w!("LastUsedTimeStart")).unwrap_or(0);
        let stop = read_qword(key, &sub, windows::core::w!("LastUsedTimeStop")).unwrap_or(0);
        out.push(Record { key: text, packaged, start, stop });
    }
    // SAFETY: `key` was opened above and is not used again.
    unsafe {
        let _ = RegCloseKey(key);
    }
    out
}

/// The packaged and the desktop records of one store root.
fn read_store(hive: HKEY, root: &str) -> Vec<Record> {
    let mut all = read_under(hive, root, true, Some("NonPackaged"));
    all.extend(read_under(hive, &format!(r"{root}\NonPackaged"), false, None));
    all
}

/// Every record of the store (the user's and the device's).
pub fn read_records() -> Vec<Record> {
    if let Some(root) = test_root() {
        return read_store(HKEY_CURRENT_USER, &root);
    }
    let mut all = read_store(HKEY_CURRENT_USER, STORE);
    all.extend(read_store(HKEY_LOCAL_MACHINE, STORE));
    all
}

/// The programs that use the camera `target` (the symbolic link Better Cam wants; `None` for the first camera) now,
/// Better Cam excluded: the registry records and the camera service's event log (bounded to about 50 ms, see
/// [`super::in_use_log`]). `vcam_consumer`: a program reads Better Cam's virtual camera, which the registry records
/// cannot tell from a holder of the physical camera.
pub fn camera_users(target: Option<&str>, vcam_consumer: bool) -> CameraUsers {
    let registry = timed_users(&read_records(), own_key().as_deref());
    merge(registry, super::in_use_log::lookup(target), vcam_consumer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Registry::{
        RegCreateKeyExW, RegDeleteTreeW, RegSetValueExW, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_QWORD,
    };

    fn rec(key: &str, packaged: bool, start: u64, stop: u64) -> Record {
        Record { key: key.into(), packaged, start, stop }
    }

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_key_name_is_the_exe_file_name() {
        assert_eq!(
            exe_name("C:#Users#x#AppData#Local#Discord#app-1.0.9261#Discord.exe").as_deref(),
            Some("Discord.exe")
        );
        assert_eq!(exe_name("C:#Program Files#Google#Chrome#Application#chrome.exe").as_deref(), Some("chrome.exe"));
        assert_eq!(exe_name("tool.exe").as_deref(), Some("tool.exe"));
        assert_eq!(exe_name("C:#dir#"), None);
        assert_eq!(exe_name(""), None);
        assert_eq!(key_of_path(r"C:\Users\x\better-cam.exe"), "C:#Users#x#better-cam.exe");
    }

    #[test]
    fn a_packaged_name_loses_its_publisher_and_its_family_suffix() {
        assert_eq!(package_name("Microsoft.WindowsCamera_8wekyb3d8bbwe"), "WindowsCamera");
        assert_eq!(package_name("TheBrowserCompany.Arc_ttt1ap7aakyb4"), "Arc");
        assert_eq!(package_name("MSTeams_8wekyb3d8bbwe"), "MSTeams");
        assert_eq!(package_name("Microsoft.Windows.Photos_8wekyb3d8bbwe"), "Windows.Photos");
        assert_eq!(package_name("Plain"), "Plain");
        assert_eq!(package_name("Trailing._x"), "Trailing.");
    }

    #[test]
    fn a_program_is_in_use_with_a_start_and_no_stop() {
        assert!(using_now(5, 0));
        assert!(!using_now(0, 0), "never used");
        assert!(!using_now(5, 9), "stopped");
        assert!(!using_now(9, 5), "a stop is a stop, whatever the order");
    }

    #[test]
    fn only_programs_in_use_are_listed_newest_first_once_each() {
        let own = key_of_path(r"C:\Apps\better-cam.exe");
        let records = [
            rec("C:#Users#x#Discord#app-1.0.1#Discord.exe", false, 100, 0),
            rec("C:#Program Files#Chrome#chrome.exe", false, 300, 0),
            rec("C:#Old#old.exe", false, 50, 60),
            rec("C:#Never#never.exe", false, 0, 0),
            rec(&own.to_uppercase(), false, 400, 0),
            // the same program from another folder, and from the device's store
            rec("C:#Users#x#Discord#app-1.0.2#discord.exe", false, 200, 0),
            rec("Microsoft.WindowsCamera_8wekyb3d8bbwe", true, 250, 0),
            rec("Microsoft.WindowsCamera_8wekyb3d8bbwe", true, 10, 0),
        ];
        assert_eq!(
            users(&records, Some(&own)),
            s(&["chrome.exe", "WindowsCamera", "discord.exe"]),
            "newest start first; Discord once, under its newest record"
        );
    }

    #[test]
    fn better_cam_is_never_named_but_a_package_with_its_name_is() {
        let own = key_of_path(r"C:\Apps\better-cam.exe");
        assert_eq!(users(&[rec(&own, false, 1, 0)], Some(&own)), Vec::<String>::new());
        assert_eq!(users(&[rec(&own, false, 1, 0)], None), s(&["better-cam.exe"]));
        // a packaged app is matched by its own key, never by the path
        assert_eq!(users(&[rec("Some.App_x", true, 1, 0)], Some("Some.App_x")), s(&["App"]));
    }

    #[test]
    fn nothing_in_use_is_an_empty_list() {
        assert!(users(&[], None).is_empty());
        assert!(users(&[rec("C:#a#b.exe", false, 1, 2)], None).is_empty());
    }

    #[test]
    fn the_sentence_names_none_one_two_and_more() {
        assert_eq!(list_text(&[]), "another app");
        assert_eq!(list_text(&s(&["Discord.exe"])), "Discord.exe");
        assert_eq!(list_text(&s(&["Discord.exe", "chrome.exe"])), "Discord.exe and chrome.exe");
        assert_eq!(list_text(&s(&["a.exe", "b.exe", "c.exe"])), "a.exe, b.exe and 1 more");
        assert_eq!(list_text(&s(&["a.exe", "b.exe", "c.exe", "d.exe"])), "a.exe, b.exe and 2 more");
        assert_eq!(reason(&[], false), "camera is in use by another app");
        assert_eq!(reason(&s(&["Discord.exe"]), false), "camera is in use by Discord.exe");
        assert_eq!(reason(&s(&["Discord.exe"]), true), "camera may be in use by Discord.exe");
        assert_eq!(reason(&[], true), "camera is in use by another app", "nobody is named: nothing to doubt");
    }

    /// Writes records of the shape Windows writes into a scratch key and reads them back through the real registry
    /// calls (the real store is not touched).
    #[test]
    fn the_registry_reads_back_what_windows_writes() {
        let root = format!(r"Software\BetterCamTest\in-use-{}", std::process::id());
        let put = |sub: &str, start: Option<u64>, stop: Option<u64>| {
            let path = HSTRING::from(format!(r"{root}\{sub}"));
            let mut key = HKEY::default();
            // SAFETY: valid strings and out pointer; the key is closed below.
            let r = unsafe {
                RegCreateKeyExW(
                    HKEY_CURRENT_USER,
                    PCWSTR(path.as_ptr()),
                    None,
                    PCWSTR::null(),
                    REG_OPTION_NON_VOLATILE,
                    KEY_WRITE,
                    None,
                    &mut key,
                    None,
                )
            };
            assert_eq!(r.0, 0, "cannot create {sub}");
            for (name, v) in [("LastUsedTimeStart", start), ("LastUsedTimeStop", stop)] {
                if let Some(v) = v {
                    let name = HSTRING::from(name);
                    // SAFETY: `key` is open for writing; the data is 8 bytes of a QWORD.
                    let r = unsafe { RegSetValueExW(key, PCWSTR(name.as_ptr()), None, REG_QWORD, Some(&v.to_le_bytes())) };
                    assert_eq!(r.0, 0);
                }
            }
            // SAFETY: opened above.
            unsafe {
                let _ = RegCloseKey(key);
            }
        };
        put("NonPackaged\\C:#Users#x#Discord.exe", Some(500), Some(0));
        put("NonPackaged\\C:#Users#x#finished.exe", Some(100), Some(200));
        put("NonPackaged\\C:#Users#x#no-values.exe", None, None);
        put("Microsoft.WindowsCamera_8wekyb3d8bbwe", Some(600), None);

        let mut got = read_store(HKEY_CURRENT_USER, &root);
        got.sort_by(|a, b| a.key.cmp(&b.key));
        let root_path = HSTRING::from(r"Software\BetterCamTest");
        // SAFETY: valid string; removes only the scratch tree made above.
        unsafe {
            let _ = RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(root_path.as_ptr()));
        }
        assert_eq!(
            got,
            vec![
                rec("C:#Users#x#Discord.exe", false, 500, 0),
                rec("C:#Users#x#finished.exe", false, 100, 200),
                rec("C:#Users#x#no-values.exe", false, 0, 0),
                rec("Microsoft.WindowsCamera_8wekyb3d8bbwe", true, 600, 0),
            ],
            "`NonPackaged` is the folder of the desktop programs, not a program"
        );
        assert_eq!(users(&got, None), s(&["WindowsCamera", "Discord.exe"]));
    }

    fn holders(names: &[(u64, &str)], maybe: bool) -> super::super::in_use_log::Holders {
        super::super::in_use_log::Holders { names: names.iter().map(|(t, n)| (*t, n.to_string())).collect(), maybe }
    }

    fn timed(v: &[(u64, &str)]) -> Vec<(u64, String)> {
        v.iter().map(|(t, n)| (*t, n.to_string())).collect()
    }

    #[test]
    fn the_registry_and_the_log_are_one_list_newest_first_once_each() {
        let got = merge(timed(&[(300, "chrome.exe"), (100, "Discord.exe")]), holders(&[(200, "Zoom.exe"), (250, "DISCORD.EXE")], false), false);
        assert_eq!(got.names, s(&["chrome.exe", "DISCORD.EXE", "Zoom.exe"]));
        assert!(!got.maybe);
        assert_eq!(merge(vec![], holders(&[], false), false), CameraUsers::default());
        assert_eq!(merge(timed(&[(1, "a.exe")]), holders(&[], false), false).names, s(&["a.exe"]));
        assert_eq!(merge(vec![], holders(&[(1, "b.exe")], false), false).names, s(&["b.exe"]));
    }

    #[test]
    fn the_list_is_a_maybe_for_a_doubt_of_the_log_or_a_registry_name_next_to_a_virtual_camera_reader() {
        // the log doubts its own attribution
        assert!(merge(vec![], holders(&[(1, "b.exe")], true), false).maybe);
        // a doubt without a name says nothing
        assert!(!merge(vec![], holders(&[], true), false).maybe);
        // a reader of the virtual camera could be what the registry names ...
        assert!(merge(timed(&[(1, "a.exe")]), holders(&[], false), true).maybe);
        // ... but not when the log confirms that very program streams the physical camera
        assert!(!merge(timed(&[(1, "a.exe")]), holders(&[(2, "A.exe")], false), true).maybe);
        // and a consumer with nothing from the registry is no doubt for the log's names
        assert!(!merge(vec![], holders(&[(2, "b.exe")], false), true).maybe);
    }

    #[test]
    fn this_machine_reads_without_panicking() {
        // What is in use differs per machine (and the record may be missing altogether); the read itself must work.
        let mut times = Vec::new();
        let mut found = CameraUsers::default();
        for _ in 0..20 {
            let t = std::time::Instant::now();
            found = camera_users(None, false);
            times.push(t.elapsed());
        }
        times.sort();
        eprintln!(
            "programs using a camera here: {found:?} (20 lookups: median {:?}, worst {:?})",
            times[times.len() / 2],
            times[times.len() - 1]
        );
    }
}
