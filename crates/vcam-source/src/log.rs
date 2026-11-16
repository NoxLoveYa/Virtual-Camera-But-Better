//! File log at `%ProgramData%\BetterCam\logs\vcam-<pid>.log`.
//!
//! The DLL runs inside Frame Server (LOCAL SERVICE, session 0), so a file is the only way to see what it does.
//! Logging must never fail the caller: every error is swallowed. The file rotates at 1 MB (`.1.log` is kept).

use std::fmt::Write as _;
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use windows::core::PWSTR;
use windows::Win32::System::LibraryLoader::{
    GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
    GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
};
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::Threading::{GetCurrentProcessId, GetCurrentThreadId};
use windows::Win32::System::WindowsProgramming::GetUserNameW;

const MAX_BYTES: u64 = 1024 * 1024;

struct LogFile {
    path: PathBuf,
    file: Option<File>,
    written: u64,
}

static LOG: OnceLock<Mutex<LogFile>> = OnceLock::new();
static COUNTERS: [AtomicU64; 16] = [const { AtomicU64::new(0) }; 16];

/// Directory of the log files: `%ProgramData%\BetterCam\logs`.
pub fn log_dir() -> PathBuf {
    let base = std::env::var_os("ProgramData").unwrap_or_else(|| "C:\\ProgramData".into());
    PathBuf::from(base)
        .join(vcam_protocol::DATA_DIR_NAME)
        .join(vcam_protocol::LOG_DIR_NAME)
}

fn open(path: &PathBuf) -> Option<File> {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    OpenOptions::new().create(true).append(true).open(path).ok()
}

fn wide_to_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

fn process_exe() -> String {
    let mut buf = [0u16; 520];
    let n = unsafe { GetModuleFileNameW(None, &mut buf) } as usize;
    String::from_utf16_lossy(&buf[..n.min(buf.len())])
}

fn account_name() -> String {
    let mut buf = [0u16; 257];
    let mut len = buf.len() as u32;
    match unsafe { GetUserNameW(Some(PWSTR(buf.as_mut_ptr())), &mut len) } {
        Ok(()) => wide_to_string(&buf),
        Err(_) => "?".into(),
    }
}

fn dll_path() -> String {
    let mut module = windows::Win32::Foundation::HMODULE::default();
    let addr = dll_path as *const () as *const u16;
    let ok = unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            windows::core::PCWSTR(addr),
            &mut module,
        )
    };
    if ok.is_err() {
        return "?".into();
    }
    let mut buf = [0u16; 520];
    let n = unsafe { GetModuleFileNameW(Some(module), &mut buf) } as usize;
    String::from_utf16_lossy(&buf[..n.min(buf.len())])
}

fn timestamp() -> String {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = d.as_secs();
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        (secs / 3600) % 24,
        (secs / 60) % 60,
        secs % 60,
        d.subsec_millis()
    )
}

fn write_line(inner: &mut LogFile, line: &str) {
    if inner.written > MAX_BYTES {
        inner.file = None;
        let mut rotated = inner.path.clone();
        rotated.set_extension("1.log");
        let _ = std::fs::remove_file(&rotated);
        let _ = std::fs::rename(&inner.path, &rotated);
        inner.written = 0;
    }
    if inner.file.is_none() {
        inner.file = open(&inner.path);
    }
    if let Some(f) = inner.file.as_mut() {
        if f.write_all(line.as_bytes()).is_ok() {
            inner.written += line.len() as u64;
        }
    }
}

fn header() -> String {
    let pid = unsafe { GetCurrentProcessId() };
    let mut session = u32::MAX;
    let _ = unsafe { ProcessIdToSessionId(pid, &mut session) };
    let mut s = String::new();
    let _ = writeln!(
        s,
        "---- vcam log start: process '{}' pid {} session {} account '{}' dll '{}' ----",
        process_exe(),
        pid,
        session,
        account_name(),
        dll_path()
    );
    s
}

/// Append one line. Never panics, never fails the caller.
pub fn log(msg: &str) {
    let _ = std::panic::catch_unwind(|| {
        let cell = LOG.get_or_init(|| {
            let pid = unsafe { GetCurrentProcessId() };
            let path = log_dir().join(format!("vcam-{pid}.log"));
            let mut inner = LogFile {
                path,
                file: None,
                written: 0,
            };
            write_line(&mut inner, &header());
            Mutex::new(inner)
        });
        let mut inner = match cell.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let tid = unsafe { GetCurrentThreadId() };
        let line = format!("{} [{}] {}\n", timestamp(), tid, msg);
        write_line(&mut inner, &line);
    });
}

/// Rate-limited log for hot paths: logs the first 5 calls and then every `every`-th call of counter `slot`.
pub fn log_sampled(slot: usize, every: u64, msg: impl FnOnce() -> String) {
    let n = COUNTERS[slot % COUNTERS.len()].fetch_add(1, Ordering::Relaxed);
    if n < 5 || n % every == 0 {
        log(&format!("#{n} {}", msg()));
    }
}

/// Convenience macro: `vlog!("Source::Start state={}", x)`.
#[macro_export]
macro_rules! vlog {
    ($($arg:tt)*) => { $crate::log::log(&format!($($arg)*)) };
}

/// Counter slots for [`log_sampled`].
pub mod slot {
    pub const REQUEST_SAMPLE: usize = 0;
    pub const SAMPLE_OUT: usize = 1;
}
