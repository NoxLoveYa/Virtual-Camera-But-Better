//! The HKLM keys of the virtual camera: the COM registration of the media source and the state key.
//!
//! Every function takes the root key it works under ([`Root`]) and the subkey path, so the real code can be run
//! against a throwaway key under `HKCU` in tests ([`Keys`]); production passes `HKLM` and the real paths
//! ([`Keys::machine`]) and nothing else.

use windows::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, ERROR_SUCCESS, WIN32_ERROR,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegDeleteKeyValueW, RegDeleteKeyW, RegDeleteTreeW, RegGetValueW, RegOpenKeyExW,
    RegSetKeyValueW, HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, REG_DWORD,
    REG_MULTI_SZ, REG_SZ, RRF_RT_ANY, RRF_RT_REG_DWORD, RRF_RT_REG_MULTI_SZ, RRF_RT_REG_SZ,
};
use windows_core::PCWSTR;

use crate::util::{pcwstr, wide, Res};

/// The root key the registry calls work under. Production uses [`Root::LocalMachine`] only; the tests of this
/// crate use a throwaway key under [`Root::CurrentUser`], and never `HKLM`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Root {
    LocalMachine,
    CurrentUser,
}

impl Root {
    fn hkey(self) -> HKEY {
        match self {
            Root::LocalMachine => HKEY_LOCAL_MACHINE,
            Root::CurrentUser => HKEY_CURRENT_USER,
        }
    }

    /// The name used in messages.
    pub fn name(self) -> &'static str {
        match self {
            Root::LocalMachine => "HKLM",
            Root::CurrentUser => "HKCU",
        }
    }
}

/// Where the helper keeps its keys: the root and the three paths below it. [`Keys::machine`] is the real place.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Keys {
    pub root: Root,
    /// The COM class key of the media source (`SOFTWARE\Classes\CLSID\{...}`).
    pub clsid: String,
    /// The state key (`SOFTWARE\BetterCam\Vcam`): the registration values and the record of moved-aside folders.
    pub state: String,
    /// The key above the state key, deleted when it is empty (`SOFTWARE\BetterCam`).
    pub vendor: String,
}

impl Keys {
    /// The keys of this machine: `HKLM`, the class key and the state key of the virtual camera.
    pub fn machine() -> Keys {
        Keys {
            root: Root::LocalMachine,
            clsid: vcam_protocol::clsid_key(),
            state: vcam_protocol::VCAM_STATE_KEY.to_string(),
            vendor: r"SOFTWARE\BetterCam".to_string(),
        }
    }

    /// A set of keys of the same shape in `HKCU` (a throwaway key of a test, see `testkit::ScratchKeys`): the real code
    /// runs against it unchanged. `prefix` names a key that the test owns and deletes ([`Keys::vendor`] is `prefix`
    /// and holds the state key, like `SOFTWARE\BetterCam` does; the class key is next to it, as `Classes` is next to
    /// `BetterCam` in `SOFTWARE`).
    #[cfg(test)]
    pub(crate) fn scratch(prefix: &str) -> Keys {
        Keys {
            root: Root::CurrentUser,
            clsid: format!(r"{prefix}-classes\{{test}}"),
            state: format!(r"{prefix}\Vcam"),
            vendor: prefix.to_string(),
        }
    }
}

pub fn set_string(root: Root, subkey: &str, name: Option<&str>, value: &str) -> Res<()> {
    let sk = wide(subkey);
    let nm = name.map(wide);
    let data = wide(value);
    let r = unsafe {
        RegSetKeyValueW(
            root.hkey(),
            pcwstr(&sk),
            nm.as_ref().map(|n| pcwstr(n)).unwrap_or(PCWSTR::null()),
            REG_SZ.0,
            Some(data.as_ptr() as *const _),
            (data.len() * 2) as u32,
        )
    };
    if r == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(format!("RegSetKeyValueW {}\\{subkey}: {r:?}", root.name()))
    }
}

pub fn set_dword(root: Root, subkey: &str, name: &str, value: u32) -> Res<()> {
    let sk = wide(subkey);
    let nm = wide(name);
    let r = unsafe {
        RegSetKeyValueW(
            root.hkey(),
            pcwstr(&sk),
            pcwstr(&nm),
            REG_DWORD.0,
            Some(&value as *const u32 as *const _),
            4,
        )
    };
    if r == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(format!(
            "RegSetKeyValueW {}\\{subkey}\\{name}: {r:?}",
            root.name()
        ))
    }
}

fn get_string(root: Root, subkey: &str, name: &str) -> Option<String> {
    let sk = wide(subkey);
    let nm = wide(name);
    let mut buf = vec![0u16; 1024];
    let mut size = (buf.len() * 2) as u32;
    let r = unsafe {
        RegGetValueW(
            root.hkey(),
            pcwstr(&sk),
            pcwstr(&nm),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut _),
            Some(&mut size),
        )
    };
    if r != ERROR_SUCCESS {
        return None;
    }
    let len = (size as usize / 2).min(buf.len());
    let end = buf[..len].iter().position(|&c| c == 0).unwrap_or(len);
    Some(String::from_utf16_lossy(&buf[..end]))
}

fn get_dword(root: Root, subkey: &str, name: &str) -> Option<u32> {
    let sk = wide(subkey);
    let nm = wide(name);
    let mut v = 0u32;
    let mut size = 4u32;
    let r = unsafe {
        RegGetValueW(
            root.hkey(),
            pcwstr(&sk),
            pcwstr(&nm),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut v as *mut u32 as *mut _),
            Some(&mut size),
        )
    };
    (r == ERROR_SUCCESS).then_some(v)
}

/// `subkey` exists under `root`.
pub fn key_exists(root: Root, subkey: &str) -> bool {
    let key = wide(subkey);
    let mut h = HKEY::default();
    let r = unsafe { RegOpenKeyExW(root.hkey(), pcwstr(&key), None, KEY_READ, &mut h) };
    if r.is_ok() {
        unsafe {
            let _ = RegCloseKey(h);
        }
        true
    } else {
        false
    }
}

/// `HKLM\SOFTWARE\Classes\CLSID\{CLSID}` exists.
pub fn clsid_key_present() -> bool {
    clsid_key_present_in(&Keys::machine())
}

/// [`clsid_key_present`] for any set of keys.
pub fn clsid_key_present_in(keys: &Keys) -> bool {
    key_exists(keys.root, &keys.clsid)
}

/// What `HKLM\SOFTWARE\BetterCam\Vcam` records about the installed DLL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Registered {
    pub dll_path: Option<String>,
    pub protocol: Option<u32>,
    pub build: Option<String>,
}

/// The state key, or `None` when it does not exist or holds no registration (readable without elevation). The key
/// can exist with nothing but the record of moved-aside data folders in it ([`ASIDE_VALUE`]): that is no
/// registration.
pub fn read_registered() -> Option<Registered> {
    read_registered_in(&Keys::machine())
}

/// [`read_registered`] for any set of keys.
pub fn read_registered_in(keys: &Keys) -> Option<Registered> {
    if !key_exists(keys.root, &keys.state) {
        return None;
    }
    registered_from(
        get_string(keys.root, &keys.state, "DllPath"),
        get_dword(keys.root, &keys.state, "Protocol"),
        get_string(keys.root, &keys.state, "Build"),
    )
}

/// The registration made of what the state key holds: `None` when it holds none of the three values.
pub fn registered_from(
    dll_path: Option<String>,
    protocol: Option<u32>,
    build: Option<String>,
) -> Option<Registered> {
    if dll_path.is_none() && protocol.is_none() && build.is_none() {
        return None;
    }
    Some(Registered {
        dll_path,
        protocol,
        build,
    })
}

/// The value of the state key that lists the data folders this helper moved aside in `%ProgramData%` (a
/// `REG_MULTI_SZ`, one string per folder: its name and its identity, see [`crate::aside`]). Only administrators can
/// write the key, so an entry in it was put there by the helper: it is what the sweep of old data folders trusts,
/// instead of every `BetterCam.old-*` entry a user could have made.
pub const ASIDE_VALUE: &str = "MovedAside";

/// The strings of a `REG_MULTI_SZ` (up to the first empty one).
pub fn decode_multi_sz(w: &[u16]) -> Vec<String> {
    w.split(|&c| c == 0)
        .take_while(|s| !s.is_empty())
        .map(String::from_utf16_lossy)
        .collect()
}

/// `items` as the data of a `REG_MULTI_SZ`; an empty string would end the list and is left out.
pub fn encode_multi_sz(items: &[String]) -> Vec<u16> {
    let mut out = Vec::new();
    for s in items.iter().filter(|s| !s.is_empty()) {
        out.extend(s.encode_utf16());
        out.push(0);
    }
    out.push(0);
    if out.len() == 1 {
        out.push(0);
    }
    out
}

/// What a `REG_MULTI_SZ` value holds, told apart from what it does not: a value that is **not there** is an empty
/// record; one that is there and **cannot be read** (another type, too long, access denied, changing while it is read)
/// is something nobody may act on or overwrite.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stored {
    /// The value (or its key) does not exist.
    Missing,
    /// The strings of the value.
    Strings(Vec<String>),
    /// It exists and was not read; the text says why.
    Unreadable(String),
}

/// The longest value read, in bytes. The record is a few lines; anything longer is not something this helper wrote.
pub const MAX_VALUE_BYTES: u32 = 64 * 1024;

/// True for the result codes that say "there is no such value or key" (the only case that is an empty record).
pub fn is_missing(code: WIN32_ERROR) -> bool {
    code == ERROR_FILE_NOT_FOUND || code == ERROR_PATH_NOT_FOUND
}

/// The strings of the `REG_MULTI_SZ` `name` of `subkey`: [`Stored::Missing`] only when it is not there,
/// [`Stored::Unreadable`] for every other way of not getting it (a value of another type is error 1630, a value that
/// grew while it was read is `ERROR_MORE_DATA`, anything above [`MAX_VALUE_BYTES`] is refused).
pub fn read_multi_string(root: Root, subkey: &str, name: &str) -> Stored {
    let keys_path = format!("{}\\{subkey}\\{name}", root.name());
    let sk = wide(subkey);
    let nm = wide(name);
    let mut size = 0u32;
    // SAFETY: a size query (no buffer), then a read into a buffer of that size.
    let r = unsafe {
        RegGetValueW(
            root.hkey(),
            pcwstr(&sk),
            pcwstr(&nm),
            RRF_RT_REG_MULTI_SZ,
            None,
            None,
            Some(&mut size),
        )
    };
    if is_missing(r) {
        return Stored::Missing;
    }
    if r != ERROR_SUCCESS {
        return Stored::Unreadable(format!("{keys_path} cannot be read (error {})", r.0));
    }
    if size > MAX_VALUE_BYTES {
        return Stored::Unreadable(format!(
            "{keys_path} is {size} bytes long, more than the {MAX_VALUE_BYTES} that are read"
        ));
    }
    let mut buf = vec![0u16; (size as usize).div_ceil(2) + 1];
    let mut size = (buf.len() * 2) as u32;
    let r = unsafe {
        RegGetValueW(
            root.hkey(),
            pcwstr(&sk),
            pcwstr(&nm),
            RRF_RT_REG_MULTI_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut _),
            Some(&mut size),
        )
    };
    if is_missing(r) {
        return Stored::Missing;
    }
    if r != ERROR_SUCCESS {
        return Stored::Unreadable(format!("{keys_path} cannot be read (error {})", r.0));
    }
    Stored::Strings(decode_multi_sz(&buf[..(size as usize / 2).min(buf.len())]))
}

/// The value `name` of `subkey` exists, or *may* exist: anything but "no such value or key" counts, so a value that
/// cannot even be asked about is never taken for absent.
pub fn value_exists(root: Root, subkey: &str, name: &str) -> bool {
    let sk = wide(subkey);
    let nm = wide(name);
    let mut size = 0u32;
    // SAFETY: a size query only; any type (a record of another type still exists).
    let r = unsafe {
        RegGetValueW(
            root.hkey(),
            pcwstr(&sk),
            pcwstr(&nm),
            RRF_RT_ANY,
            None,
            None,
            Some(&mut size),
        )
    };
    !is_missing(r)
}

/// Write the `REG_MULTI_SZ` `name` of `subkey` (the key is made when it is missing).
pub fn set_multi_string(root: Root, subkey: &str, name: &str, items: &[String]) -> Res<()> {
    let sk = wide(subkey);
    let nm = wide(name);
    let data = encode_multi_sz(items);
    let r = unsafe {
        RegSetKeyValueW(
            root.hkey(),
            pcwstr(&sk),
            pcwstr(&nm),
            REG_MULTI_SZ.0,
            Some(data.as_ptr() as *const _),
            (data.len() * 2) as u32,
        )
    };
    if r == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(format!(
            "RegSetKeyValueW {}\\{subkey}\\{name}: {r:?}",
            root.name()
        ))
    }
}

/// Delete the value `name` of `subkey`. The result is the system's: a value or key that is not there is
/// [`is_missing`] (as good as gone), anything else is a failure.
pub fn delete_value(root: Root, subkey: &str, name: &str) -> WIN32_ERROR {
    let sk = wide(subkey);
    let nm = wide(name);
    unsafe { RegDeleteKeyValueW(root.hkey(), pcwstr(&sk), pcwstr(&nm)) }
}

/// Delete `subkey` and everything below it.
pub fn delete_tree(root: Root, subkey: &str) -> WIN32_ERROR {
    let key = wide(subkey);
    unsafe { RegDeleteTreeW(root.hkey(), pcwstr(&key)) }
}

/// Delete an empty `subkey` (errors ignored).
pub fn delete_key_if_empty(root: Root, subkey: &str) {
    let key = wide(subkey);
    let _ = unsafe { RegDeleteKeyW(root.hkey(), pcwstr(&key)) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_multi_string_round_trips_and_an_empty_one_is_two_nuls() {
        let names = vec![
            "BetterCam.old-1-0".to_string(),
            "BetterCam.old-2-0".to_string(),
        ];
        let w = encode_multi_sz(&names);
        assert_eq!(w.last(), Some(&0));
        assert_eq!(w[w.len() - 2], 0, "each string and the list end with a nul");
        assert_eq!(decode_multi_sz(&w), names);
        assert_eq!(encode_multi_sz(&[]), vec![0, 0]);
        assert!(decode_multi_sz(&[0, 0]).is_empty());
        assert!(decode_multi_sz(&[]).is_empty());
        // an empty string would end the list: it is not written
        assert_eq!(
            decode_multi_sz(&encode_multi_sz(&["a".into(), String::new(), "b".into()])),
            ["a", "b"]
        );
        // data that is cut off in the middle of a string still gives its text
        assert_eq!(decode_multi_sz(&[97, 98]), ["ab"]);
    }

    /// "Missing" and "unreadable" are told apart by the result code: only a value or key that is not there is an empty
    /// record. A value of another type (1630), one that grew while it was read (`ERROR_MORE_DATA`), access denied and
    /// every other code are values that exist and were not read: nobody acts on them or overwrites them.
    #[test]
    fn only_no_such_value_is_an_empty_record() {
        assert!(is_missing(ERROR_FILE_NOT_FOUND));
        assert!(is_missing(ERROR_PATH_NOT_FOUND));
        for code in [0u32, 5, 234, 1630, 1450, 87] {
            assert!(!is_missing(WIN32_ERROR(code)), "error {code}");
        }
    }

    // ---- the real registry code, against a throwaway key under HKCU (never HKLM)

    use crate::testkit::ScratchKeys;

    fn names(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("BetterCam.old-{i}-0")).collect()
    }

    /// A value that is not there is an empty record, whether the key is missing or only the value is; a value of
    /// another type exists (and is unreadable as a record), and asking about it never creates anything.
    #[test]
    fn a_missing_value_is_missing_and_asking_creates_nothing() {
        let k = ScratchKeys::new();
        let (root, state) = (k.keys.root, k.keys.state.as_str());
        assert_eq!(read_multi_string(root, state, ASIDE_VALUE), Stored::Missing);
        assert!(!value_exists(root, state, ASIDE_VALUE));
        assert!(!key_exists(root, state));
        // the key is there, the value is not
        set_string(root, state, Some("DllPath"), r"C:\x.dll").unwrap();
        assert!(key_exists(root, state));
        assert_eq!(read_multi_string(root, state, ASIDE_VALUE), Stored::Missing);
        assert!(!value_exists(root, state, ASIDE_VALUE));
        assert!(is_missing(delete_value(root, state, ASIDE_VALUE)));
        assert!(is_missing(delete_value(
            root,
            r"Software\no-such-key-of-the-test",
            "x"
        )));
    }

    #[test]
    fn a_multi_string_is_written_and_read_back() {
        let k = ScratchKeys::new();
        let (root, state) = (k.keys.root, k.keys.state.as_str());
        let items = names(3);
        set_multi_string(root, state, ASIDE_VALUE, &items).unwrap();
        assert!(value_exists(root, state, ASIDE_VALUE));
        assert_eq!(
            read_multi_string(root, state, ASIDE_VALUE),
            Stored::Strings(items.clone())
        );
        // written again: replaced, not appended
        set_multi_string(root, state, ASIDE_VALUE, &items[..1]).unwrap();
        assert_eq!(
            read_multi_string(root, state, ASIDE_VALUE),
            Stored::Strings(items[..1].to_vec())
        );
        // an empty list is a value that exists and holds nothing
        set_multi_string(root, state, ASIDE_VALUE, &[]).unwrap();
        assert_eq!(
            read_multi_string(root, state, ASIDE_VALUE),
            Stored::Strings(Vec::new())
        );
        assert!(value_exists(root, state, ASIDE_VALUE));
        // deleted: missing again
        assert_eq!(delete_value(root, state, ASIDE_VALUE), ERROR_SUCCESS);
        assert_eq!(read_multi_string(root, state, ASIDE_VALUE), Stored::Missing);
    }

    /// A value of another type is something that exists and cannot be read as a record: `Unreadable` (error 1630),
    /// never `Missing`, so nobody overwrites it.
    #[test]
    fn a_value_of_another_type_is_unreadable_and_still_exists() {
        let k = ScratchKeys::new();
        let (root, state) = (k.keys.root, k.keys.state.as_str());
        set_string(root, state, Some(ASIDE_VALUE), "not a list").unwrap();
        assert!(value_exists(root, state, ASIDE_VALUE));
        let Stored::Unreadable(why) = read_multi_string(root, state, ASIDE_VALUE) else {
            panic!("a REG_SZ was read as a record")
        };
        assert!(why.contains("1630") && why.contains(ASIDE_VALUE), "{why}");
        // a DWORD too
        set_dword(root, state, ASIDE_VALUE, 7).unwrap();
        assert!(matches!(
            read_multi_string(root, state, ASIDE_VALUE),
            Stored::Unreadable(_)
        ));
    }

    /// More than `MAX_VALUE_BYTES` is not something this helper wrote: refused without reading it.
    #[test]
    fn an_over_long_value_is_unreadable() {
        let k = ScratchKeys::new();
        let (root, state) = (k.keys.root, k.keys.state.as_str());
        // 70,000 characters in one string: 140,000 bytes
        set_multi_string(root, state, ASIDE_VALUE, &["x".repeat(70_000)]).unwrap();
        let Stored::Unreadable(why) = read_multi_string(root, state, ASIDE_VALUE) else {
            panic!("an over-long value was read")
        };
        assert!(why.contains("more than the 65536"), "{why}");
        // a value under the bound is read (32,000 characters are about 64,000 bytes)
        set_multi_string(root, state, ASIDE_VALUE, &["x".repeat(32_000)]).unwrap();
        assert!(matches!(
            read_multi_string(root, state, ASIDE_VALUE),
            Stored::Strings(s) if s.len() == 1 && s[0].len() == 32_000
        ));
    }

    /// A state key that holds only the record of moved-aside folders is no registration.
    #[test]
    fn a_state_key_without_the_three_values_is_not_a_registration() {
        assert_eq!(registered_from(None, None, None), None);
        let r = registered_from(Some("C:\\x.dll".into()), None, None).unwrap();
        assert_eq!(r.dll_path.as_deref(), Some("C:\\x.dll"));
        assert!(registered_from(None, Some(1), None).is_some());
        assert!(registered_from(None, None, Some("aa".into())).is_some());
    }
}
