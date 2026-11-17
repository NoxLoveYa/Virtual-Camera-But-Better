//! Small helpers: wide strings, COM and Media Foundation start-up, the folders of the virtual camera.

use std::path::PathBuf;

use windows::Win32::Media::MediaFoundation::{MFShutdown, MFStartup, MFSTARTUP_FULL, MF_VERSION};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows_core::PCWSTR;

/// Error type of the install code: a sentence that can be shown to a person.
pub type Res<T> = Result<T, String>;

pub fn werr(what: &str, e: impl std::fmt::Display) -> String {
    format!("{what}: {e}")
}

/// Null-terminated UTF-16 copy of `s`.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// `PCWSTR` that borrows `buf`; keep `buf` alive while it is used.
pub fn pcwstr(buf: &[u16]) -> PCWSTR {
    PCWSTR(buf.as_ptr())
}

/// COM (MTA) and Media Foundation for the rest of the process. For short-lived programs; a long-running process
/// uses [`MfSession`].
pub fn init_com_and_mf() -> windows_core::Result<()> {
    unsafe {
        let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
        // RPC_E_CHANGED_MODE is fine: some apartment already exists
        if hr.is_err() && hr != windows::Win32::Foundation::RPC_E_CHANGED_MODE {
            return Err(hr.into());
        }
        MFStartup(MF_VERSION, MFSTARTUP_FULL)
    }
}

/// One balanced `CoInitializeEx` + `MFStartup` for a call in a long-running process (the app's status check).
pub struct MfSession {
    com: bool,
}

impl MfSession {
    pub fn start() -> windows_core::Result<MfSession> {
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let com = hr.is_ok();
        if hr.is_err() && hr != windows::Win32::Foundation::RPC_E_CHANGED_MODE {
            return Err(hr.into());
        }
        if let Err(e) = unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) } {
            if com {
                unsafe { CoUninitialize() };
            }
            return Err(e);
        }
        Ok(MfSession { com })
    }
}

impl Drop for MfSession {
    fn drop(&mut self) {
        unsafe {
            let _ = MFShutdown();
            if self.com {
                CoUninitialize();
            }
        }
    }
}

/// `%ProgramFiles%\Better Cam\vcam`.
pub fn install_dir() -> PathBuf {
    let pf = std::env::var_os("ProgramFiles").unwrap_or_else(|| "C:\\Program Files".into());
    PathBuf::from(pf)
        .join(vcam_protocol::INSTALL_DIR_COMPANY)
        .join(vcam_protocol::INSTALL_DIR_VCAM)
}

/// `%ProgramData%\BetterCam`.
pub fn data_dir() -> PathBuf {
    let pd = std::env::var_os("ProgramData").unwrap_or_else(|| "C:\\ProgramData".into());
    PathBuf::from(pd).join(vcam_protocol::DATA_DIR_NAME)
}

/// `%ProgramData%\BetterCam\logs`.
pub fn log_dir() -> PathBuf {
    data_dir().join(vcam_protocol::LOG_DIR_NAME)
}

/// FNV-1a 64 over `bytes`, folded to 8 hex digits: the "build" part of the versioned DLL name.
pub fn content_tag(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{:08x}", (h ^ (h >> 32)) as u32)
}

/// The build tag of the DLL file at `path` (see [`content_tag`]).
pub fn dll_build(path: &std::path::Path) -> Res<String> {
    let bytes = std::fs::read(path).map_err(|e| werr(&format!("read {}", path.display()), e))?;
    Ok(content_tag(&bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_is_stable_and_content_sensitive() {
        assert_eq!(content_tag(b"abc"), content_tag(b"abc"));
        assert_ne!(content_tag(b"abc"), content_tag(b"abd"));
        assert_eq!(content_tag(b"abc").len(), 8);
    }

    #[test]
    fn wide_is_nul_terminated() {
        assert_eq!(wide("ab"), vec![97, 98, 0]);
    }
}
