//! The HKLM keys of the virtual camera: the COM registration of the media source and the state key.

use windows::Win32::Foundation::{ERROR_SUCCESS, WIN32_ERROR};
use windows::Win32::System::Registry::{
    RegCloseKey, RegDeleteKeyW, RegDeleteTreeW, RegGetValueW, RegOpenKeyExW, RegSetKeyValueW, HKEY,
    HKEY_LOCAL_MACHINE, KEY_READ, REG_DWORD, REG_SZ, RRF_RT_REG_DWORD, RRF_RT_REG_SZ,
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

/// The state key, or `None` when it does not exist (readable without elevation).
pub fn read_registered() -> Option<Registered> {
    if !key_exists(vcam_protocol::VCAM_STATE_KEY) {
        return None;
    }
    let key = vcam_protocol::VCAM_STATE_KEY;
    Some(Registered {
        dll_path: get_string(key, "DllPath"),
        protocol: get_dword(key, "Protocol"),
        build: get_string(key, "Build"),
    })
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
