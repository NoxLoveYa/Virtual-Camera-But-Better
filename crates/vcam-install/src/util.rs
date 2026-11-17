//! Small helpers: wide strings, COM and Media Foundation start-up, the folders of the virtual camera.

use std::path::PathBuf;
use std::sync::OnceLock;

use windows::Win32::Media::MediaFoundation::{MFShutdown, MFStartup, MFSTARTUP_FULL, MF_VERSION};
use windows::Win32::System::Com::{
    CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_MULTITHREADED,
};
use windows::Win32::UI::Shell::{FOLDERID_ProgramFiles, SHGetKnownFolderPath, KNOWN_FOLDER_FLAG};
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

/// `%ProgramFiles%` from the shell's known folder database, not from the environment: the elevated helper runs with
/// the environment of whoever started it. Falls back to `C:\Program Files`.
pub fn program_files_dir() -> PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let known =
            unsafe { SHGetKnownFolderPath(&FOLDERID_ProgramFiles, KNOWN_FOLDER_FLAG(0), None) };
        match known {
            Ok(p) => {
                let s = unsafe { p.to_string() };
                unsafe { CoTaskMemFree(Some(p.0 as *const core::ffi::c_void)) };
                match s {
                    Ok(s) if !s.is_empty() => PathBuf::from(s),
                    _ => PathBuf::from(r"C:\Program Files"),
                }
            }
            Err(_) => PathBuf::from(r"C:\Program Files"),
        }
    })
    .clone()
}

/// `%ProgramFiles%\Better Cam\vcam`.
pub fn install_dir() -> PathBuf {
    program_files_dir()
        .join(vcam_protocol::INSTALL_DIR_COMPANY)
        .join(vcam_protocol::INSTALL_DIR_VCAM)
}

/// `%ProgramData%\BetterCam` (`%ProgramData%` from the shell, see [`vcam_protocol::secure::program_data_dir`]).
pub fn data_dir() -> PathBuf {
    vcam_protocol::frames_dir()
}

/// `%ProgramData%\BetterCam\logs`.
pub fn log_dir() -> PathBuf {
    data_dir().join(vcam_protocol::LOG_DIR_NAME)
}

/// Every folder the installer works in. [`Paths::system`] is the machine; tests build their own below a temp folder.
#[derive(Clone, Debug)]
pub struct Paths {
    /// The trusted folder the data folder must really lie in (`%ProgramData%`).
    pub program_data: PathBuf,
    /// `<program_data>\BetterCam`.
    pub data: PathBuf,
    /// Where the versioned DLL copies go (`%ProgramFiles%\Better Cam\vcam`).
    pub install: PathBuf,
}

impl Paths {
    /// This machine's folders.
    pub fn system() -> Paths {
        Paths {
            program_data: vcam_protocol::secure::program_data_dir(),
            data: data_dir(),
            install: install_dir(),
        }
    }

    /// Folders below `root` (tests): `root\ProgramData\BetterCam` and `root\ProgramFiles\Better Cam\vcam`.
    pub fn below(root: &std::path::Path) -> Paths {
        let program_data = root.join("ProgramData");
        Paths {
            data: program_data.join(vcam_protocol::DATA_DIR_NAME),
            program_data,
            install: root
                .join("ProgramFiles")
                .join(vcam_protocol::INSTALL_DIR_COMPANY)
                .join(vcam_protocol::INSTALL_DIR_VCAM),
        }
    }

    pub fn logs(&self) -> PathBuf {
        self.data.join(vcam_protocol::LOG_DIR_NAME)
    }

    pub fn frames(&self) -> PathBuf {
        self.data.join(vcam_protocol::FRAMES_FILE_NAME)
    }

    pub fn install_log(&self) -> PathBuf {
        self.logs().join("install.log")
    }
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
