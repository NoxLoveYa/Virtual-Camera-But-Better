//! A scratch folder for a test, that does not outlive the test.
//!
//! Tests used to make `%TEMP%\<prefix>-<pid>` and never remove it: a frame file is 37 MB and a `cargo test` run left
//! gigabytes behind. [`TestDir`] removes its folder when it is dropped (a test that panics too). The first use in a
//! process also sweeps `%TEMP%` for what a process that was killed or crashed left behind.
//!
//! Compiled for this crate's own tests and, with the `test-support` feature, for the tests of the crates that
//! depend on it (a `cfg(test)` item does not cross a crate); the ship builds never enable the feature.

use std::ffi::OsStr;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::OnceLock;

use windows::Win32::Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, STILL_ACTIVE};
use windows::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};

/// The first words of every name this module sweeps (and of every name the tests made before it existed). A name
/// outside these is never touched.
const SWEPT_PREFIXES: [&str; 4] = ["bettercam-", "better-cam-", "vcam-", "bc-"];

/// A new empty folder `%TEMP%\<prefix>-<pid>-<n>`, removed with everything in it on drop. It derefs to its [`Path`].
/// Keep the value alive as long as the test uses the folder.
#[derive(Debug)]
pub struct TestDir {
    path: PathBuf,
}

impl TestDir {
    /// Makes the folder. `prefix` starts with one of `bettercam-`, `better-cam-`, `vcam-` or `bc-`, and has no
    /// all-digit part (`bettercam-engine`, not `bettercam-engine-7`): the sweep reads the first number as a pid.
    pub fn new(prefix: &str) -> TestDir {
        assert!(
            SWEPT_PREFIXES.iter().any(|p| prefix.starts_with(p)),
            "test dir prefix {prefix:?} is not one the sweep knows"
        );
        static SWEEP: OnceLock<()> = OnceLock::new();
        SWEEP.get_or_init(|| sweep(&std::env::temp_dir()));
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("{prefix}-{}-{n}", std::process::id()));
        // a folder with this name is a leftover of a process that had this pid
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create test dir");
        TestDir { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Deref for TestDir {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for TestDir {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<OsStr> for TestDir {
    fn as_ref(&self) -> &OsStr {
        self.path.as_os_str()
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        remove_dir(&self.path);
    }
}

/// Best effort, errors ignored. A test that gave a file a DACL without DELETE leaves a folder `remove_dir_all` cannot
/// empty; the owner can still reset the DACLs to the inherited ones (`icacls /reset`), and then it can.
fn remove_dir(path: &Path) {
    if std::fs::remove_dir_all(path).is_ok() || !path.exists() {
        return;
    }
    let _ = std::process::Command::new("icacls")
        .arg(path)
        .args(["/reset", "/t", "/c", "/q"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    let _ = std::fs::remove_dir_all(path);
}

/// A path `<new TestDir>\<name>` for a test that needs one file: nothing is made at the path, and the folder is
/// removed with the value. It derefs to its [`Path`].
#[derive(Debug)]
pub struct TestFile {
    path: PathBuf,
    _dir: TestDir,
}

impl TestFile {
    pub fn new(prefix: &str, name: &str) -> TestFile {
        let dir = TestDir::new(prefix);
        TestFile {
            path: dir.join(name),
            _dir: dir,
        }
    }
}

impl Deref for TestFile {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for TestFile {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<OsStr> for TestFile {
    fn as_ref(&self) -> &OsStr {
        self.path.as_os_str()
    }
}

/// Removes the entries of `dir` that a test process made and that is no longer running. Best effort: an entry that
/// resists (a test left a folder with a DACL of its own) stays.
fn sweep(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(pid_of) else {
            continue;
        };
        if process_is_running(pid) {
            continue;
        }
        let path = entry.path();
        // not `is_dir` on the entry: a link must be removed as a link, never followed
        let is_dir = entry
            .file_type()
            .is_ok_and(|t| t.is_dir() && !t.is_symlink());
        if is_dir {
            remove_dir(&path);
        } else {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// The pid in a name of the form `<known prefix>...-<pid>[-...][.ext]`: the first part that is only digits.
fn pid_of(name: &str) -> Option<u32> {
    let rest = SWEPT_PREFIXES.iter().find_map(|p| name.strip_prefix(p))?;
    rest.split(['-', '.'])
        .find(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))?
        .parse()
        .ok()
        // 0 and 4 are never a test process; a number that is no pid is no reason to remove anything
        .filter(|&pid| pid > 4)
}

/// `true` unless it is certain that no process has this pid: a process that cannot be opened (access denied, any
/// other error) counts as running.
fn process_is_running(pid: u32) -> bool {
    // SAFETY: plain calls; the handle is closed before return
    unsafe {
        let handle = match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            Ok(h) => h,
            Err(e) => return e.code() != ERROR_INVALID_PARAMETER.to_hresult(),
        };
        let mut code = 0u32;
        let alive =
            GetExitCodeProcess(handle, &mut code).map_or(true, |()| code == STILL_ACTIVE.0 as u32);
        let _ = CloseHandle(handle);
        alive
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dir_exists_until_dropped() {
        let dir = TestDir::new("bettercam-testdir");
        let path = dir.path().to_path_buf();
        std::fs::write(dir.join("f.bin"), b"x").unwrap();
        assert!(path.is_dir());
        drop(dir);
        assert!(!path.exists());
    }

    #[test]
    fn dirs_differ() {
        let (a, b) = (
            TestDir::new("bettercam-testdir"),
            TestDir::new("bettercam-testdir"),
        );
        assert_ne!(a.path(), b.path());
    }

    #[test]
    fn dir_is_removed_when_the_test_panics() {
        let made = std::sync::Mutex::new(None);
        let r = std::panic::catch_unwind(|| {
            let dir = TestDir::new("bettercam-testdir");
            *made.lock().unwrap() = Some(dir.path().to_path_buf());
            panic!("test failed");
        });
        assert!(r.is_err());
        let path = made.lock().unwrap().take().unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn pid_is_read_from_every_name_shape() {
        assert_eq!(pid_of("bettercam-engine-4242-3"), Some(4242));
        assert_eq!(pid_of("bettercam-preview-order-4242-x"), Some(4242));
        assert_eq!(pid_of("vcam-test-4242-name"), Some(4242));
        assert_eq!(pid_of("better-cam-registry-name-4242"), Some(4242));
        assert_eq!(pid_of("bc-nv-hash-4242-1.bin"), Some(4242));
        assert_eq!(pid_of("bc-nv-live-4242.bin"), Some(4242));
        assert_eq!(pid_of("bc-build.log"), None);
        assert_eq!(pid_of("other-4242"), None);
        assert_eq!(pid_of("bettercam-4"), None);
    }

    #[test]
    fn running_and_dead_pids() {
        assert!(process_is_running(std::process::id()));
        // an exited child: its pid is no longer a process once the handle is closed
        let mut child = std::process::Command::new("cmd")
            .args(["/c", "exit"])
            .spawn()
            .unwrap();
        let pid = child.id();
        child.wait().unwrap();
        drop(child);
        assert!(!process_is_running(pid));
    }

    #[test]
    fn sweep_removes_dead_pids_only() {
        let root = TestDir::new("bettercam-testdir");
        let mut child = std::process::Command::new("cmd")
            .args(["/c", "exit"])
            .spawn()
            .unwrap();
        let dead = child.id();
        child.wait().unwrap();
        drop(child);
        let live = std::process::id();
        for name in [
            format!("bettercam-engine-{dead}"),
            format!("bc-nv-hash-{dead}-1.bin"),
            format!("bettercam-engine-{live}"),
            "unrelated-folder".to_string(),
            "bc-build.log".to_string(),
        ] {
            if name.ends_with(".bin") || name.ends_with(".log") {
                std::fs::write(root.join(&name), b"x").unwrap();
            } else {
                std::fs::create_dir(root.join(&name)).unwrap();
            }
        }
        sweep(&root);
        assert!(!root.join(format!("bettercam-engine-{dead}")).exists());
        assert!(!root.join(format!("bc-nv-hash-{dead}-1.bin")).exists());
        assert!(root.join(format!("bettercam-engine-{live}")).exists());
        assert!(root.join("unrelated-folder").exists());
        assert!(root.join("bc-build.log").exists());
    }
}
