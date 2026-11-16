//! Small helpers shared by the subcommands.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use windows::Win32::Media::MediaFoundation::{MFStartup, MFSTARTUP_FULL, MF_VERSION};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows_core::PCWSTR;

/// Null-terminated UTF-16 copy of `s`.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// `PCWSTR` that borrows `buf`; keep `buf` alive while it is used.
pub fn pcwstr(buf: &[u16]) -> PCWSTR {
    PCWSTR(buf.as_ptr())
}

/// COM (MTA) and Media Foundation for the current thread/process.
pub fn init_com_and_mf() -> windows_core::Result<()> {
    unsafe {
        let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
        if hr.is_err() {
            // RPC_E_CHANGED_MODE is fine: some apartment already exists
            if hr != windows::Win32::Foundation::RPC_E_CHANGED_MODE {
                return Err(hr.into());
            }
        }
        MFStartup(MF_VERSION, MFSTARTUP_FULL)
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

pub fn log_dir() -> PathBuf {
    data_dir().join(vcam_protocol::LOG_DIR_NAME)
}

/// Output sink of the elevated child: stdout (hidden console) plus a result file the parent prints.
pub struct Out {
    file: Option<PathBuf>,
}

impl Out {
    pub fn new(file: Option<&Path>) -> Out {
        if let Some(f) = file {
            let _ = std::fs::write(f, b"");
        }
        Out {
            file: file.map(|p| p.to_path_buf()),
        }
    }

    pub fn line(&mut self, s: &str) {
        println!("{s}");
        if let Some(f) = &self.file {
            if let Ok(mut h) = OpenOptions::new().create(true).append(true).open(f) {
                let _ = writeln!(h, "{s}");
            }
        }
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
