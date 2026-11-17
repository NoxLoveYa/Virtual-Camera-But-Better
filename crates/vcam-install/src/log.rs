//! Where the install code reports to: a line sink, and the install log file.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

use windows::Win32::System::SystemInformation::GetLocalTime;

use crate::util::{log_dir, werr, Res};

/// Receives one line per step of an install or uninstall.
pub trait Log {
    fn line(&mut self, s: &str);
}

/// A sink that drops everything.
pub struct NullLog;

impl Log for NullLog {
    fn line(&mut self, _: &str) {}
}

/// A sink that keeps the lines (tests).
#[derive(Default)]
pub struct VecLog(pub Vec<String>);

impl Log for VecLog {
    fn line(&mut self, s: &str) {
        self.0.push(s.to_string());
    }
}

/// `%ProgramData%\BetterCam\logs\install.log`.
pub fn install_log_path() -> PathBuf {
    log_dir().join("install.log")
}

/// Appends timestamped lines to `install.log` and echoes them to stdout. The file is opened per line, so deleting
/// the folder at the end of an uninstall never fights a handle of ours; when the folder is gone, lines are only
/// echoed.
pub struct FileLog {
    path: PathBuf,
}

impl FileLog {
    /// With `create` the log folder is made first (install); without it a missing folder stays missing (uninstall).
    pub fn open(create: bool) -> Res<FileLog> {
        if create {
            std::fs::create_dir_all(log_dir()).map_err(|e| werr("create the log folder", e))?;
        }
        Ok(FileLog {
            path: install_log_path(),
        })
    }

    fn stamp() -> String {
        let t = unsafe { GetLocalTime() };
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
        )
    }
}

impl Log for FileLog {
    fn line(&mut self, s: &str) {
        println!("{s}");
        if let Ok(mut f) = OpenOptions::new()
            .append(true)
            .create(true)
            .open(&self.path)
        {
            let _ = writeln!(f, "{} {s}", Self::stamp());
        }
    }
}

/// The last `max` non-empty lines of `install.log` (what the app shows when the helper failed).
pub fn log_tail(max: usize) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(install_log_path()) else {
        return Vec::new();
    };
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(max)..]
        .iter()
        .map(|l| l.to_string())
        .collect()
}
