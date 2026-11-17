//! The HKLM keys of the virtual camera: the COM registration of the media source and the state key.

use windows::Win32::Foundation::{ERROR_SUCCESS, WIN32_ERROR};
use windows::Win32::System::Registry::{
    RegCloseKey, RegDeleteKeyValueW, RegDeleteKeyW, RegDeleteTreeW, RegGetValueW, RegOpenKeyExW,
    RegSetKeyValueW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ, REG_DWORD, REG_MULTI_SZ, REG_SZ,
    RRF_RT_REG_DWORD, RRF_RT_REG_MULTI_SZ, RRF_RT_REG_SZ,
};
use windows_core::PCWSTR;

use crate::util::{pcwstr, wide, Res};

pub fn set_string(subkey: &str, name: Option<&str>, value: &str) -> Res<()> {
    let sk = wide(subkey);
    let nm = name.map(wide);
    let data = wide(value);
    let r = unsafe {
        RegSetKeyValueW(
            HKEY_LOCAL_MACHINE,
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
        Err(format!("RegSetKeyValueW HKLM\\{subkey}: {r:?}"))
    }
}

pub fn set_dword(subkey: &str, name: &str, value: u32) -> Res<()> {
    let sk = wide(subkey);
    let nm = wide(name);
    let r = unsafe {
        RegSetKeyValueW(
            HKEY_LOCAL_MACHINE,
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
        Err(format!("RegSetKeyValueW HKLM\\{subkey}\\{name}: {r:?}"))
    }
}

fn get_string(subkey: &str, name: &str) -> Option<String> {
    let sk = wide(subkey);
    let nm = wide(name);
    let mut buf = vec![0u16; 1024];
    let mut size = (buf.len() * 2) as u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
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

fn get_dword(subkey: &str, name: &str) -> Option<u32> {
    let sk = wide(subkey);
    let nm = wide(name);
    let mut v = 0u32;
    let mut size = 4u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
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

fn key_exists(subkey: &str) -> bool {
    let key = wide(subkey);
    let mut h = HKEY::default();
    let r = unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, pcwstr(&key), None, KEY_READ, &mut h) };
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
    key_exists(&vcam_protocol::clsid_key())
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
    if !key_exists(vcam_protocol::VCAM_STATE_KEY) {
        return None;
    }
    let key = vcam_protocol::VCAM_STATE_KEY;
    registered_from(
        get_string(key, "DllPath"),
        get_dword(key, "Protocol"),
        get_string(key, "Build"),
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

/// The value of the state key that lists the names of the data folders this helper moved aside in `%ProgramData%`
/// (a `REG_MULTI_SZ`). Only administrators can write the key, so a name in it was put there by the helper: it is what
/// the sweep of old data folders trusts, instead of every `BetterCam.old-*` entry a user could have made.
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

/// The strings of the `REG_MULTI_SZ` `name` of `subkey` (empty when there is none).
pub fn get_multi_string(subkey: &str, name: &str) -> Vec<String> {
    let sk = wide(subkey);
    let nm = wide(name);
    let mut size = 0u32;
    // SAFETY: a size query (no buffer), then a read into a buffer of that size.
    let r = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            pcwstr(&sk),
            pcwstr(&nm),
            RRF_RT_REG_MULTI_SZ,
            None,
            None,
            Some(&mut size),
        )
    };
    if r != ERROR_SUCCESS || size == 0 {
        return Vec::new();
    }
    let mut buf = vec![0u16; (size as usize).div_ceil(2) + 1];
    let mut size = (buf.len() * 2) as u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            pcwstr(&sk),
            pcwstr(&nm),
            RRF_RT_REG_MULTI_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut _),
            Some(&mut size),
        )
    };
    if r != ERROR_SUCCESS {
        return Vec::new();
    }
    decode_multi_sz(&buf[..(size as usize / 2).min(buf.len())])
}

/// Write the `REG_MULTI_SZ` `name` of `subkey` (the key is made when it is missing).
pub fn set_multi_string(subkey: &str, name: &str, items: &[String]) -> Res<()> {
    let sk = wide(subkey);
    let nm = wide(name);
    let data = encode_multi_sz(items);
    let r = unsafe {
        RegSetKeyValueW(
            HKEY_LOCAL_MACHINE,
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
        Err(format!("RegSetKeyValueW HKLM\\{subkey}\\{name}: {r:?}"))
    }
}

/// Delete the value `name` of `subkey` (errors ignored: a value that is not there is as good as gone).
pub fn delete_value(subkey: &str, name: &str) {
    let sk = wide(subkey);
    let nm = wide(name);
    let _ = unsafe { RegDeleteKeyValueW(HKEY_LOCAL_MACHINE, pcwstr(&sk), pcwstr(&nm)) };
}

/// Delete `subkey` and everything below it.
pub fn delete_tree(subkey: &str) -> WIN32_ERROR {
    let key = wide(subkey);
    unsafe { RegDeleteTreeW(HKEY_LOCAL_MACHINE, pcwstr(&key)) }
}

/// Delete an empty `subkey` (errors ignored).
pub fn delete_key_if_empty(subkey: &str) {
    let key = wide(subkey);
    let _ = unsafe { RegDeleteKeyW(HKEY_LOCAL_MACHINE, pcwstr(&key)) };
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
