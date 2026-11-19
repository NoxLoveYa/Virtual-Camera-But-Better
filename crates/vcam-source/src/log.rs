//! File log at `%ProgramData%\BetterCam\logs\vcam-<pid>.log`.
//!
//! The DLL runs inside Frame Server (LOCAL SERVICE, session 0), so a file is the only way to see what it does.
//! Logging must never fail the caller: every error is swallowed.
//!
//! The folder is writable by a standard user, so the log never trusts what it finds there:
//! * it never creates `BetterCam` or `logs` (the installer does, with the right permissions): without the folder
//!   there is simply no log;
//! * the `logs` folder must really be `<ProgramData>\BetterCam\logs` on a local fixed disk, not a link or junction,
//!   and the log file must be a plain file with one hard link at the expected place (see
//!   [`vcam_protocol::secure`]); otherwise it logs nothing and tries again later;
//! * the folder is held open (no delete sharing, so it cannot be swapped) only while the file is opened and old
//!   logs are pruned;
//! * a log that grows past the cap is truncated through its verified handle, never renamed or deleted by path;
//! * pruning deletes only plain files named `vcam-*.log` and keeps the newest [`KEEP_LOGS`] plus every file whose
//!   process is still running, so the number of files stays bounded however often processes start;
//! * nothing slow happens while the `LOG` mutex is held: opening the file and pruning (which touch the disk and may
//!   meet a file somebody else has locked) run outside it, claimed by one thread at a time ([`LogFile::begin_open`]);
//!   the other callers' lines are dropped meanwhile instead of waiting. The opens and deletes never wait for
//!   another program (`vcam_protocol::secure`). The user has no access to `vcam-*.log` at all, so only the service
//!   account could ever hold such a lock.

use std::fmt::Write as _;
use std::fs::File;
use std::io::{Seek, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use vcam_protocol::secure;
use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED};
use windows::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
use windows::Win32::System::LibraryLoader::{
    GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
    GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
};
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::Threading::{
    GetCurrentProcessId, GetCurrentThreadId, GetExitCodeProcess, OpenProcess,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::System::WindowsProgramming::GetUserNameW;

/// Size cap of one log file.
const MAX_BYTES: u64 = 1024 * 1024;
/// Log files kept besides the current one (plus those of running processes) when a log starts.
pub const KEEP_LOGS: usize = 8;
/// How long a log that could not be opened waits before it tries again.
const RETRY_AFTER: Duration = Duration::from_secs(30);
/// `GetExitCodeProcess` result of a process that has not ended.
const STILL_ACTIVE: u32 = 259;

struct LogFile {
    /// Trusted folder the log folder must lie in.
    root: PathBuf,
    dir: PathBuf,
    name: String,
    file: Option<File>,
    written: u64,
    max_bytes: u64,
    header_written: bool,
    next_try: Option<Instant>,
    /// A thread is opening the file right now (outside the lock); nobody else starts a second open.
    opening: bool,
}

/// What the thread that claimed the open does outside the lock.
struct OpenJob {
    root: PathBuf,
    dir: PathBuf,
    name: String,
}

#[cfg(test)]
thread_local! {
    /// How many times this thread called [`open_log`] (tests: a failed truncation must not make every line open again).
    static OPEN_CALLS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

static LOG: OnceLock<Mutex<LogFile>> = OnceLock::new();
static COUNTERS: [AtomicU64; 16] = [const { AtomicU64::new(0) }; 16];

/// Directory of the log files: `%ProgramData%\BetterCam\logs` (with `%ProgramData%` from the shell).
pub fn log_dir() -> PathBuf {
    crate::frames::trusted_root()
        .join(vcam_protocol::DATA_DIR_NAME)
        .join(vcam_protocol::LOG_DIR_NAME)
}

/// Open `dir\name` for appending, the safe way (see the module docs). The folder and the file are checked against
/// `root`; the folder is created by nobody. Returns the file, positioned at its end, and its length.
fn open_log(root: &Path, dir: &Path, name: &str) -> std::io::Result<(File, u64)> {
    #[cfg(test)]
    OPEN_CALLS.with(|c| c.set(c.get() + 1));
    let expect_dir = secure::expected_path(root, dir)?;
    // No delete sharing: while this handle is open the folder cannot be renamed, deleted or replaced by a link.
    let dir_handle = secure::open_no_follow(dir, false, true, FILE_SHARE_READ | FILE_SHARE_WRITE)?;
    secure::verify_dir(secure::raw(&dir_handle), Some(&expect_dir))?;
    prune(dir, KEEP_LOGS, name);
    let path = dir.join(name);
    let expect_file = secure::expected_path(root, &path)?;
    // read and write, created when missing, a link in the last component is not followed, no waiting for a lock
    let mut file = File::from(secure::open_with(
        &path,
        secure::ACCESS_GENERIC_READ | secure::ACCESS_GENERIC_WRITE,
        secure::SHARE_ALL,
        secure::DISP_OPEN_ALWAYS,
        false,
        0,
    )?);
    secure::verify_file(secure::raw(&file), Some(&expect_file))?;
    let len = file.seek(SeekFrom::End(0))?;
    drop(dir_handle);
    Ok((file, len))
}

/// `vcam-<pid>.log` as a pid.
fn pid_of(name: &str) -> Option<u32> {
    name.strip_prefix("vcam-")?
        .strip_suffix(".log")?
        .parse()
        .ok()
}

fn process_alive(pid: u32) -> bool {
    match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
        Ok(h) => {
            let mut code = 0u32;
            let alive = unsafe { GetExitCodeProcess(h, &mut code) }.is_ok() && code == STILL_ACTIVE;
            let _ = unsafe { CloseHandle(h) };
            alive
        }
        // a process we may not open exists all the same
        Err(e) => e.code() == ERROR_ACCESS_DENIED.to_hresult(),
    }
}

/// Delete old `vcam-*.log` files in `dir`: keep the newest `keep`, `current`, and every file of a running process.
/// Only plain files are touched: a link, a junction or a directory with such a name is left alone, and a file is
/// deleted through a handle that was opened without following a link ([`secure::remove_plain_file`]).
pub fn prune(dir: &Path, keep: usize, current: &str) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut old: Vec<(SystemTime, PathBuf, String)> = Vec::new();
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !(name.starts_with("vcam-") && name.ends_with(".log")) || name == current {
            continue;
        }
        let path = e.path();
        let Ok(md) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !md.file_type().is_file() {
            continue;
        }
        old.push((md.modified().unwrap_or(UNIX_EPOCH), path, name));
    }
    old.sort_by_key(|e| std::cmp::Reverse(e.0));
    for (_, path, name) in old.into_iter().skip(keep) {
        if pid_of(&name).is_some_and(process_alive) {
            continue;
        }
        // never waits for a program that holds a lock on the file: that file stays for the next prune
        let _ = secure::remove_plain_file(&path);
    }
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

impl LogFile {
    fn new(root: PathBuf, dir: PathBuf, name: String) -> LogFile {
        LogFile {
            root,
            dir,
            name,
            file: None,
            written: 0,
            max_bytes: MAX_BYTES,
            header_written: false,
            next_try: None,
            opening: false,
        }
    }

    /// Claim the opening of the file when it is closed, nobody else is opening it and the last refusal is old
    /// enough. The caller opens with [`open_log`] *without holding the lock* and hands the result to
    /// [`LogFile::finish_open`].
    fn begin_open(&mut self) -> Option<OpenJob> {
        if self.file.is_some() || self.opening || self.next_try.is_some_and(|t| Instant::now() < t)
        {
            return None;
        }
        self.opening = true;
        Some(OpenJob {
            root: self.root.clone(),
            dir: self.dir.clone(),
            name: self.name.clone(),
        })
    }

    /// The result of the open that [`LogFile::begin_open`] claimed. A refusal makes the log wait [`RETRY_AFTER`].
    fn finish_open(&mut self, result: std::io::Result<(File, u64)>) {
        self.opening = false;
        match result {
            Ok((f, len)) => {
                self.file = Some(f);
                self.written = len;
                self.next_try = None;
            }
            Err(_) => self.next_try = Some(Instant::now() + RETRY_AFTER),
        }
    }

    /// [`begin_open`](Self::begin_open), the open and [`finish_open`](Self::finish_open) in one go, for tests that
    /// have no other thread.
    #[cfg(test)]
    fn open_now(&mut self) {
        if let Some(job) = self.begin_open() {
            let r = open_log(&job.root, &job.dir, &job.name);
            self.finish_open(r);
        }
    }

    /// [`open_now`](Self::open_now) and [`append`](Self::append).
    #[cfg(test)]
    fn write_line(&mut self, line: &str) {
        self.open_now();
        self.append(line);
    }

    /// Append `line` to the open file; nothing happens while there is none. Never opens, never creates anything.
    fn append(&mut self, line: &str) {
        if self.file.is_none() {
            return;
        }
        if self.written > self.max_bytes {
            // truncate through the verified handle: no rename or delete by path, so no link can be followed
            if let Some(f) = self.file.as_mut() {
                if f.set_len(0).is_ok() && f.seek(SeekFrom::Start(0)).is_ok() {
                    self.written = 0;
                    let note = format!("{} [-] log truncated at the size cap\n", timestamp());
                    if f.write_all(note.as_bytes()).is_ok() {
                        self.written += note.len() as u64;
                    }
                } else {
                    // e.g. another process has the file mapped (error 1224): give the file up and wait before
                    // the next open, or every line would prune, open and verify again
                    self.file = None;
                    self.next_try = Some(Instant::now() + RETRY_AFTER);
                    return;
                }
            }
        }
        if let Some(f) = self.file.as_mut() {
            if f.write_all(line.as_bytes()).is_ok() {
                self.written += line.len() as u64;
            } else {
                // the file went away or the disk is full: reopen later
                self.file = None;
                self.next_try = Some(Instant::now() + RETRY_AFTER);
            }
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

fn lock_log(cell: &Mutex<LogFile>) -> MutexGuard<'_, LogFile> {
    match cell.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

/// Append one line. Never panics, never fails the caller, and never makes a caller wait for the disk: the open (and
/// the prune that comes with it) runs outside the lock, on the one thread that found the file closed.
pub fn log(msg: &str) {
    let _ = std::panic::catch_unwind(|| {
        let cell = LOG.get_or_init(|| {
            let pid = unsafe { GetCurrentProcessId() };
            Mutex::new(LogFile::new(
                crate::frames::trusted_root(),
                log_dir(),
                format!("vcam-{pid}.log"),
            ))
        });
        let job = lock_log(cell).begin_open();
        if let Some(job) = job {
            // a panic in here must not leave `opening` set for ever
            let r = std::panic::catch_unwind(|| open_log(&job.root, &job.dir, &job.name))
                .unwrap_or_else(|_| Err(std::io::Error::other("opening the log panicked")));
            lock_log(cell).finish_open(r);
        }
        let mut inner = lock_log(cell);
        if !inner.header_written && inner.file.is_some() {
            inner.header_written = true;
            inner.append(&header());
        }
        let tid = unsafe { GetCurrentThreadId() };
        let line = format!("{} [{}] {}\n", timestamp(), tid, msg);
        inner.append(&line);
    });
}

/// Rate-limited log for hot paths: logs the first 5 calls and then every `every`-th call of counter `slot`.
#[allow(clippy::manual_is_multiple_of)] // MSRV 1.87: unsigned `is_multiple_of` is stable only from 1.90
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
    pub const HEADER_INVALID: usize = 2;
    pub const SLOT_INVALID: usize = 3;
    pub const FILE_REFUSED: usize = 4;
    /// A sample buffer that cannot hold a frame.
    pub const SINK_BUFFER: usize = 5;
    /// A sample made outside the pool (the pool was empty or could not be made).
    pub const POOL_FALLBACK: usize = 6;
    /// A sample that could not be made at all.
    pub const SAMPLE_FAILED: usize = 7;
    /// The frame events were opened, or were not used (and why).
    pub const WAKE_OPEN: usize = 8;
    /// The pump went back to polling after spurious or missed wake-ups, or may try the events again.
    pub const WAKE_DEMOTE: usize = 9;
    /// The user the frame events must belong to could not be read from the frame file.
    pub const WAKE_OWNER: usize = 10;
    /// The frame events could not be opened or were not the writer's (its own counter: a pair that keeps failing must
    /// not use up the lines of [`WAKE_OPEN`]).
    pub const WAKE_OPEN_FAILED: usize = 11;
    /// A kernel wait of the pump returned something other than an object or the timeout.
    pub const WAKE_WAIT_FAILED: usize = 12;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `<temp>\bettercam-log-<pid>` as the trusted root with an empty `BetterCam\logs` below it.
    struct Sandbox {
        root: PathBuf,
        dir: PathBuf,
    }

    fn sandbox(name: &str) -> Sandbox {
        let root = crate::test_dir("log").join(name);
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("BetterCam").join("logs");
        std::fs::create_dir_all(&dir).unwrap();
        Sandbox { root, dir }
    }

    fn junction(link: &Path, target: &Path) {
        let out = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .unwrap();
        assert!(out.status.success(), "mklink /J failed");
    }

    fn logs_in(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("vcam-") && n.ends_with(".log"))
            .collect();
        v.sort();
        v
    }

    /// A pid that no process has (pids are far below this).
    fn dead_pid(i: u32) -> u32 {
        3_000_000_000 + i
    }

    fn make_old_log(dir: &Path, name: &str, age_secs: u64) {
        let f = File::create(dir.join(name)).unwrap();
        f.set_modified(SystemTime::now() - Duration::from_secs(age_secs))
            .unwrap();
    }

    #[test]
    fn a_log_opens_in_an_existing_folder_and_appends() {
        let s = sandbox("append");
        let mut l = LogFile::new(s.root.clone(), s.dir.clone(), "vcam-1.log".into());
        l.write_line("one\n");
        l.write_line("two\n");
        drop(l);
        let mut l = LogFile::new(s.root.clone(), s.dir.clone(), "vcam-1.log".into());
        l.write_line("three\n");
        assert_eq!(
            std::fs::read_to_string(s.dir.join("vcam-1.log")).unwrap(),
            "one\ntwo\nthree\n"
        );
    }

    #[test]
    fn missing_folders_are_never_created() {
        let root = crate::test_dir("log").join("missing");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        // neither BetterCam nor logs exists
        let dir = root.join("BetterCam").join("logs");
        let mut l = LogFile::new(root.clone(), dir.clone(), "vcam-1.log".into());
        l.write_line("x\n");
        assert!(l.file.is_none());
        assert!(!root.join("BetterCam").exists(), "BetterCam was created");
        // BetterCam exists, logs does not
        std::fs::create_dir_all(root.join("BetterCam")).unwrap();
        let mut l = LogFile::new(root.clone(), dir.clone(), "vcam-1.log".into());
        l.write_line("x\n");
        assert!(!dir.exists(), "logs was created");
        // the old code created both folders
    }

    #[test]
    fn a_logs_folder_that_is_a_junction_gets_nothing() {
        let s = sandbox("junction-logs");
        let elsewhere = s.root.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::remove_dir(&s.dir).unwrap();
        junction(&s.dir, &elsewhere);
        make_old_log(&elsewhere, "vcam-5.log", 100);
        let mut l = LogFile::new(s.root.clone(), s.dir.clone(), "vcam-1.log".into());
        l.write_line("secret\n");
        assert!(l.file.is_none());
        assert_eq!(
            logs_in(&elsewhere),
            vec!["vcam-5.log"],
            "the target was touched"
        );
        let _ = std::fs::remove_dir(&s.dir);
    }

    #[test]
    fn a_data_folder_that_is_a_junction_gets_nothing() {
        let s = sandbox("junction-data");
        let elsewhere = s.root.join("elsewhere");
        std::fs::create_dir_all(elsewhere.join("logs")).unwrap();
        let data = s.root.join("BetterCam");
        std::fs::remove_dir_all(&data).unwrap();
        junction(&data, &elsewhere);
        let mut l = LogFile::new(s.root.clone(), s.dir.clone(), "vcam-1.log".into());
        l.write_line("secret\n");
        assert!(l.file.is_none());
        assert!(logs_in(&elsewhere.join("logs")).is_empty());
        let _ = std::fs::remove_dir(&data);
    }

    #[test]
    fn a_hard_linked_log_file_is_refused_and_left_alone() {
        let s = sandbox("hardlink");
        let victim = s.root.join("victim.txt");
        std::fs::write(&victim, "precious").unwrap();
        std::fs::hard_link(&victim, s.dir.join("vcam-1.log")).unwrap();
        let mut l = LogFile::new(s.root.clone(), s.dir.clone(), "vcam-1.log".into());
        l.write_line("appended\n");
        assert!(l.file.is_none());
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "precious");
    }

    #[test]
    fn a_symbolic_link_as_log_file_is_refused_when_one_can_be_made() {
        let s = sandbox("symlink");
        let victim = s.root.join("victim.txt");
        std::fs::write(&victim, "precious").unwrap();
        if std::os::windows::fs::symlink_file(&victim, s.dir.join("vcam-1.log")).is_err() {
            eprintln!("no privilege to create a symbolic link: skipped");
            return;
        }
        let mut l = LogFile::new(s.root.clone(), s.dir.clone(), "vcam-1.log".into());
        l.write_line("appended\n");
        assert!(l.file.is_none());
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "precious");
    }

    #[test]
    fn pruning_keeps_the_newest_and_the_running_and_touches_only_plain_logs() {
        let s = sandbox("prune");
        for i in 0..30u32 {
            make_old_log(
                &s.dir,
                &format!("vcam-{}.log", dead_pid(i)),
                1000 + i as u64 * 10,
            );
        }
        // oldest of all, but its process (this one) is running
        let me = format!("vcam-{}.log", std::process::id());
        make_old_log(&s.dir, &me, 100_000);
        // not ours to delete: another name, a directory called like a log, a leftover with two dots
        std::fs::write(s.dir.join("install.log"), "x").unwrap();
        std::fs::create_dir(s.dir.join("vcam-777.log")).unwrap();
        prune(&s.dir, KEEP_LOGS, "vcam-current.log");
        let left = logs_in(&s.dir);
        // 8 newest + this process + the directory that only looks like a log
        assert_eq!(left.len(), KEEP_LOGS + 2, "{left:?}");
        assert!(left.contains(&me));
        assert!(left.contains(&"vcam-777.log".to_string()));
        for i in 0..KEEP_LOGS as u32 {
            assert!(
                left.contains(&format!("vcam-{}.log", dead_pid(i))),
                "newest {i} missing"
            );
        }
        assert!(s.dir.join("install.log").exists());
    }

    #[test]
    fn the_number_of_logs_stays_bounded_over_many_process_starts() {
        let s = sandbox("bounded");
        let mut most = 0;
        for i in 0..60u32 {
            // each "process" opens its own log, as every consumer process does
            let name = format!("vcam-{}.log", dead_pid(i));
            let mut l = LogFile::new(s.root.clone(), s.dir.clone(), name);
            l.write_line("hello\n");
            assert!(l.file.is_some());
            let n = logs_in(&s.dir).len();
            most = most.max(n);
            // let the modified times differ
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(most <= KEEP_LOGS + 1, "up to {most} files");
        assert!(logs_in(&s.dir).len() <= KEEP_LOGS + 1);
    }

    #[test]
    fn a_full_log_is_truncated_through_its_handle_and_never_renamed() {
        let s = sandbox("cap");
        let mut l = LogFile::new(s.root.clone(), s.dir.clone(), "vcam-1.log".into());
        l.max_bytes = 2048;
        for i in 0..400 {
            l.write_line(&format!("line {i} {}\n", "x".repeat(40)));
        }
        let files = std::fs::read_dir(&s.dir).unwrap().count();
        assert_eq!(files, 1, "rotation must not create a second file");
        let len = std::fs::metadata(s.dir.join("vcam-1.log")).unwrap().len();
        assert!(len <= 2048 + 200, "{len} bytes");
        let text = std::fs::read_to_string(s.dir.join("vcam-1.log")).unwrap();
        assert!(text.contains("log truncated at the size cap"));
        assert!(text.contains("line 399 "));
    }

    fn open_calls() -> u32 {
        OPEN_CALLS.with(|c| c.get())
    }

    /// A log that cannot be cut (another process has it mapped: error 1224) must not make every line open, verify
    /// and prune again: the open waits like after any other refusal. The old code dropped the file without a
    /// wait, so the next line opened it again, and so did every line after it.
    #[test]
    fn a_failed_truncation_waits_before_the_next_open() {
        let s = sandbox("truncate-fails");
        let mut l = LogFile::new(s.root.clone(), s.dir.clone(), "vcam-1.log".into());
        // a log of a few KB (a mapping needs the header size at least), then a cap below it
        for i in 0..80 {
            l.write_line(&format!("line {i} {}\n", "x".repeat(60)));
        }
        let path = s.dir.join("vcam-1.log");
        assert!(std::fs::metadata(&path).unwrap().len() > 4096);
        let mapped = vcam_protocol::MappedFile::open(&path, vcam_protocol::Access::ReadOnly)
            .expect("map the log like another process could");
        l.max_bytes = 1000;
        let before = open_calls();
        l.write_line("the line that tries to truncate\n");
        assert!(l.file.is_none(), "the file is given up");
        assert!(l.next_try.is_some(), "and the next open waits");
        // more lines: no new open, no prune, nothing written
        for _ in 0..50 {
            l.write_line("dropped\n");
        }
        assert_eq!(open_calls(), before, "every line opened the file again");
        // after the wait, with the mapping gone, the log works again
        drop(mapped);
        l.next_try = Some(Instant::now());
        l.write_line("back\n");
        assert!(l.file.is_some());
        assert!(std::fs::read_to_string(&path).unwrap().contains("back"));
    }

    /// Whoever holds an oplock on a log file must not stall the logging caller: the open is refused at once.
    #[test]
    fn a_log_file_that_somebody_holds_an_oplock_on_is_not_waited_for() {
        use vcam_protocol::secure::oplock::Oplock;
        let s = sandbox("oplock");
        let path = s.dir.join("vcam-1.log");
        std::fs::write(&path, "old\n").unwrap();
        let Some(lock) = Oplock::take(&path).unwrap() else {
            eprintln!("this file system grants no oplock: skipped");
            return;
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let (root, dir) = (s.root.clone(), s.dir.clone());
        std::thread::spawn(move || {
            let mut l = LogFile::new(root, dir, "vcam-1.log".into());
            l.write_line("hello\n");
            let _ = tx.send((l.file.is_none(), l.next_try.is_some()));
        });
        let (no_file, waits) = rx
            .recv_timeout(Duration::from_secs(3))
            .expect("the log open waited for the oplock holder");
        assert!(
            no_file && waits,
            "the open is a refusal that is retried later"
        );
        drop(lock);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old\n");
    }

    /// The prune deletes through a handle and never waits for a lock either: a locked old log stays for the
    /// next prune, the others are deleted, and this returns.
    #[test]
    fn pruning_does_not_wait_for_a_locked_log_file() {
        use vcam_protocol::secure::oplock::Oplock;
        let s = sandbox("prune-oplock");
        for i in 0..4u32 {
            make_old_log(
                &s.dir,
                &format!("vcam-{}.log", dead_pid(i)),
                1000 + i as u64,
            );
        }
        let locked = s.dir.join(format!("vcam-{}.log", dead_pid(3)));
        let Some(lock) = Oplock::take(&locked).unwrap() else {
            eprintln!("this file system grants no oplock: skipped");
            return;
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let dir = s.dir.clone();
        std::thread::spawn(move || {
            prune(&dir, 0, "vcam-current.log");
            let _ = tx.send(());
        });
        rx.recv_timeout(Duration::from_secs(3))
            .expect("the prune waited for the oplock holder");
        assert_eq!(logs_in(&s.dir), vec![format!("vcam-{}.log", dead_pid(3))]);
        drop(lock);
        prune(&s.dir, 0, "vcam-current.log");
        assert!(logs_in(&s.dir).is_empty());
    }

    /// The open is claimed by one thread at a time and runs outside the lock: while it is under way every other
    /// caller finds the file closed and simply drops its line (no second open, no waiting).
    #[test]
    fn only_one_thread_opens_the_log_and_the_others_do_not_wait() {
        let s = sandbox("claim");
        let mut l = LogFile::new(s.root.clone(), s.dir.clone(), "vcam-1.log".into());
        let job = l.begin_open().expect("the first caller opens");
        assert!(l.begin_open().is_none(), "a second open was handed out");
        l.append("lost while the file is being opened\n");
        assert!(l.file.is_none());
        let r = open_log(&job.root, &job.dir, &job.name);
        l.finish_open(r);
        assert!(l.file.is_some() && l.begin_open().is_none());
        l.append("kept\n");
        assert_eq!(
            std::fs::read_to_string(s.dir.join("vcam-1.log")).unwrap(),
            "kept\n"
        );
        // a refused open waits, and nobody opens during the wait
        std::fs::remove_dir_all(s.root.join("BetterCam")).unwrap();
        let mut m = LogFile::new(s.root.clone(), s.dir.clone(), "vcam-2.log".into());
        let job = m.begin_open().unwrap();
        m.finish_open(open_log(&job.root, &job.dir, &job.name));
        assert!(m.file.is_none() && m.next_try.is_some());
        assert!(m.begin_open().is_none());
        m.next_try = Some(Instant::now());
        assert!(m.begin_open().is_some(), "after the wait it tries again");
    }

    #[test]
    fn a_log_that_was_refused_tries_again_later() {
        let s = sandbox("retry");
        std::fs::remove_dir(&s.dir).unwrap();
        let mut l = LogFile::new(s.root.clone(), s.dir.clone(), "vcam-1.log".into());
        l.write_line("lost\n");
        assert!(l.file.is_none() && l.next_try.is_some());
        // the installer creates the folder; after the wait the log starts
        std::fs::create_dir_all(&s.dir).unwrap();
        l.write_line("still waiting\n");
        assert!(l.file.is_none(), "must not retry before the wait is over");
        l.next_try = Some(Instant::now());
        l.write_line("found\n");
        assert_eq!(
            std::fs::read_to_string(s.dir.join("vcam-1.log")).unwrap(),
            "found\n"
        );
    }
}
