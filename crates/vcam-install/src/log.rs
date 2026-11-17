//! Where the install code reports to: a line sink, and the install log file.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::time::SystemTime;

use vcam_protocol::secure;
use windows::Win32::System::SystemInformation::GetLocalTime;

use crate::util::{werr, Paths, Res};

/// Receives one line per step of an install or uninstall.
pub trait Log {
    fn line(&mut self, s: &str);

    /// The data folder was made or repaired: a log that lives in it may open its file now.
    fn folder_ready(&mut self) {}

    /// The data folder is about to be deleted: close the file that lives in it.
    fn release_folder(&mut self) {}
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
pub fn install_log_path() -> std::path::PathBuf {
    Paths::system().install_log()
}

/// The log is cut to nothing when it is longer than this at the start of a run.
const MAX_LOG_BYTES: u64 = 512 * 1024;
/// Lines kept for a log file that cannot be opened yet.
const MAX_PENDING: usize = 400;

/// Appends timestamped lines to `install.log` and echoes them to stdout.
///
/// The file is opened only through [`secure`]: the `logs` folder and the file must be what they should be (no link,
/// one hard link, the exact place below `%ProgramData%`), and it is written through that one handle, never by path
/// again. Until a good file is open (a first install has no folder yet) the lines are kept and written when it
/// opens; if it never does they are only echoed.
pub struct FileLog {
    paths: Paths,
    file: Option<File>,
    /// Every line of this run (up to a limit): replayed into a new, empty file when the folder was replaced.
    history: Vec<String>,
    /// Lines written while no file was open.
    pending: Vec<String>,
}

impl FileLog {
    /// A log for this machine's folder. Opens the file when the folder is there and acceptable; never creates
    /// anything.
    pub fn open() -> FileLog {
        Self::with_paths(Paths::system())
    }

    /// [`FileLog::open`] for other folders (tests).
    pub fn with_paths(paths: Paths) -> FileLog {
        let mut l = FileLog {
            paths,
            file: None,
            history: Vec::new(),
            pending: Vec::new(),
        };
        let _ = l.attach();
        l
    }

    /// True while lines go to the file.
    pub fn attached(&self) -> bool {
        self.file.is_some()
    }

    fn stamp() -> String {
        let t = unsafe { GetLocalTime() };
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
        )
    }

    /// Open `install.log` in an existing, acceptable `logs` folder (creating the file if the folder lacks it).
    fn attach(&mut self) -> Res<()> {
        if self.file.is_some() {
            return Ok(());
        }
        let logs = self.paths.logs();
        let path = self.paths.install_log();
        let root = &self.paths.program_data;
        let expect_dir = secure::expected_path(root, &logs).map_err(|e| werr("log folder", e))?;
        // hold the folder (no delete sharing) while the file is opened
        let dir = secure::open_with(
            &logs,
            GENERIC_READ,
            windows::Win32::Storage::FileSystem::FILE_SHARE_READ
                | windows::Win32::Storage::FileSystem::FILE_SHARE_WRITE,
            secure::DISP_OPEN_EXISTING,
            true,
            0,
        )
        .map_err(|e| werr("log folder", e))?;
        secure::verify_dir(secure::raw(&dir), Some(&expect_dir))
            .map_err(|e| werr("log folder", e))?;
        let expect_file = secure::expected_path(root, &path).map_err(|e| werr("install.log", e))?;
        let h = secure::open_with(
            &path,
            GENERIC_READ | GENERIC_WRITE,
            secure::SHARE_ALL,
            secure::DISP_OPEN_ALWAYS,
            false,
            0,
        )
        .map_err(|e| werr("install.log", e))?;
        secure::verify_file(secure::raw(&h), Some(&expect_file))
            .map_err(|e| werr("install.log", e))?;
        let mut f = File::from(h);
        if f.metadata().map(|m| m.len()).unwrap_or(0) > MAX_LOG_BYTES {
            let _ = f.set_len(0);
        }
        f.seek(SeekFrom::End(0))
            .map_err(|e| werr("install.log", e))?;
        // a new file gets the whole run; a file that was there already has the lines up to the moment it was let go of
        let fresh = f.metadata().map(|m| m.len()).unwrap_or(1) == 0;
        let replay = if fresh { &self.history } else { &self.pending };
        for line in replay {
            let _ = writeln!(f, "{line}");
        }
        self.pending.clear();
        self.file = Some(f);
        drop(dir);
        Ok(())
    }
}

const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;

impl Log for FileLog {
    fn line(&mut self, s: &str) {
        println!("{s}");
        let text = format!("{} {s}", Self::stamp());
        if self.history.len() < MAX_PENDING {
            self.history.push(text.clone());
        }
        match self.file.as_mut() {
            Some(f) => {
                if writeln!(f, "{text}").is_err() {
                    self.file = None;
                }
            }
            None => {
                if self.pending.len() < MAX_PENDING {
                    self.pending.push(text);
                }
            }
        }
    }

    fn folder_ready(&mut self) {
        // the folder may have been replaced since the file was opened
        self.file = None;
        let _ = self.attach();
    }

    fn release_folder(&mut self) {
        self.file = None;
    }
}

/// The last `max` non-empty lines of `install.log` written after `since` (what the app shows when the helper
/// failed). Empty when the file is older than `since`: that is another run's text. Read through the same checks as
/// the file is written with.
pub fn log_tail_since(since: SystemTime, max: usize) -> Vec<String> {
    tail_of(&Paths::system(), since, max)
}

/// [`log_tail_since`] for other folders (tests).
pub fn tail_of(paths: &Paths, since: SystemTime, max: usize) -> Vec<String> {
    let path = paths.install_log();
    let Ok(expect) = secure::expected_path(&paths.program_data, &path) else {
        return Vec::new();
    };
    let Ok(h) = secure::open_with(
        &path,
        GENERIC_READ,
        secure::SHARE_ALL,
        secure::DISP_OPEN_EXISTING,
        false,
        0,
    ) else {
        return Vec::new();
    };
    if secure::verify_file(secure::raw(&h), Some(&expect)).is_err() {
        return Vec::new();
    }
    let mut f = File::from(h);
    if f.metadata()
        .and_then(|m| m.modified())
        .is_ok_and(|m| m < since)
    {
        return Vec::new();
    }
    let mut text = String::new();
    if f.read_to_string(&mut text).is_err() {
        return Vec::new();
    }
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(max)..]
        .iter()
        .map(|l| l.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datadir::{prepare, remove_no_follow};
    use crate::testkit::{junction, sandbox, security_of};

    fn text(p: &std::path::Path) -> String {
        std::fs::read_to_string(p).unwrap_or_default()
    }

    #[test]
    fn lines_wait_for_the_folder_and_are_written_through_the_verified_file_afterwards() {
        let s = sandbox("filelog");
        let mut log = FileLog::with_paths(s.paths.clone());
        assert!(!log.attached(), "no folder yet: nothing is created");
        log.line("before the folder");
        assert!(!s.paths.data.exists(), "the log must not create the folder");
        prepare(&s.paths, &s.p, &mut || false, &mut crate::log::NullLog).unwrap();
        log.folder_ready();
        assert!(log.attached());
        log.line("after the folder");
        let t = text(&s.paths.install_log());
        assert!(
            t.contains("before the folder") && t.contains("after the folder"),
            "{t}"
        );
        // the file inherits what `logs` gives: it is the administrators' and the user can read it
        let snap = security_of(&s.paths.install_log());
        assert!(snap
            .aces
            .iter()
            .any(|a| a.sid == crate::testkit::FAKE_USER && a.mask == crate::acl::FILE_READ));
        // released: the folder can be deleted
        log.release_folder();
        assert!(!log.attached());
        assert!(crate::datadir::remove_all(&s.paths));
        log.line("after the delete");
        assert!(!s.paths.data.exists());
    }

    #[test]
    fn a_log_folder_that_is_a_junction_gets_nothing() {
        let s = sandbox("filelog-junction");
        prepare(&s.paths, &s.p, &mut || false, &mut crate::log::NullLog).unwrap();
        let elsewhere = s.root.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        remove_no_follow(&s.paths.logs()).unwrap();
        junction(&s.paths.logs(), &elsewhere);
        let mut log = FileLog::with_paths(s.paths.clone());
        assert!(!log.attached());
        log.line("secret");
        log.folder_ready();
        assert!(!log.attached());
        log.line("secret");
        assert_eq!(
            std::fs::read_dir(&elsewhere).unwrap().count(),
            0,
            "a file was made behind the junction"
        );
    }

    #[test]
    fn a_hard_linked_or_symlinked_install_log_is_refused_and_left_alone() {
        let s = sandbox("filelog-links");
        prepare(&s.paths, &s.p, &mut || false, &mut crate::log::NullLog).unwrap();
        let victim = s.root.join("victim.txt");
        std::fs::write(&victim, "precious").unwrap();
        std::fs::hard_link(&victim, s.paths.install_log()).unwrap();
        let mut log = FileLog::with_paths(s.paths.clone());
        assert!(!log.attached());
        log.line("appended?");
        assert_eq!(text(&victim), "precious");
        std::fs::remove_file(s.paths.install_log()).unwrap();
        if std::os::windows::fs::symlink_file(&victim, s.paths.install_log()).is_ok() {
            let mut log = FileLog::with_paths(s.paths.clone());
            assert!(!log.attached());
            log.line("appended?");
            assert_eq!(text(&victim), "precious");
        } else {
            eprintln!("no privilege to create a symbolic link: skipped");
        }
    }

    #[test]
    fn the_tail_is_read_through_the_same_checks_and_only_when_it_is_new() {
        let s = sandbox("filelog-tail");
        prepare(&s.paths, &s.p, &mut || false, &mut crate::log::NullLog).unwrap();
        let before = SystemTime::now();
        let mut log = FileLog::with_paths(s.paths.clone());
        for i in 0..5 {
            log.line(&format!("line {i}"));
        }
        let tail = tail_of(&s.paths, before - std::time::Duration::from_secs(5), 3);
        assert_eq!(tail.len(), 3);
        assert!(tail[2].ends_with("line 4"));
        // a file that is older than the run says nothing about this run
        let later = SystemTime::now() + std::time::Duration::from_secs(60);
        assert!(tail_of(&s.paths, later, 3).is_empty());
        // a link in place of the file is not read
        log.release_folder();
        let victim = s.root.join("victim.txt");
        std::fs::write(&victim, "secret\n").unwrap();
        std::fs::remove_file(s.paths.install_log()).unwrap();
        std::fs::hard_link(&victim, s.paths.install_log()).unwrap();
        assert!(tail_of(&s.paths, before - std::time::Duration::from_secs(5), 3).is_empty());
    }
}
