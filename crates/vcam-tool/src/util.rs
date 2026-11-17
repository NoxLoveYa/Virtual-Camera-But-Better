//! Small helpers shared by the subcommands.

use std::fs::File;
use std::io::Write;
use std::path::Path;

use vcam_protocol::secure;

/// `GENERIC_READ | GENERIC_WRITE`.
const READ_WRITE: u32 = 0x8000_0000 | 0x4000_0000;

/// Output sink of the elevated child: stdout (hidden console) plus a result file the parent prints.
pub struct Out {
    file: Option<File>,
}

/// Open the result file the parent named, for the elevated child. The path is in the user's `%TEMP%`, where the
/// user (or anything running as them) can plant a link at a name that is easy to guess, and the child writes with
/// administrator rights. So nothing is followed: the file is opened as it is, created only when it does not exist
/// (`CREATE_NEW` fails on anything that is there, a link included), and an existing one must be a plain file with
/// one hard link. It is emptied through the handle that was checked.
fn open_result(path: &Path) -> Result<File, String> {
    let open =
        |disposition| secure::open_with(path, READ_WRITE, secure::SHARE_ALL, disposition, false, 0);
    let h = match open(secure::DISP_OPEN_EXISTING) {
        Ok(h) => h,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => open(secure::DISP_CREATE_NEW)
            .map_err(|e| format!("result file {}: {e}", path.display()))?,
        Err(e) => return Err(format!("result file {}: {e}", path.display())),
    };
    secure::verify_file(secure::raw(&h), None)
        .map_err(|e| format!("result file {} is not usable: {e}", path.display()))?;
    let f = File::from(h);
    f.set_len(0)
        .map_err(|e| format!("result file {}: {e}", path.display()))?;
    Ok(f)
}

impl Out {
    /// With a result `file` (see [`open_result`]); `Err` when it is a link or otherwise not acceptable.
    pub fn new(file: Option<&Path>) -> Result<Out, String> {
        Ok(Out {
            file: file.map(open_result).transpose()?,
        })
    }

    pub fn line(&mut self, s: &str) {
        println!("{s}");
        if let Some(f) = self.file.as_mut() {
            let _ = writeln!(f, "{s}");
        }
    }
}

impl vcam_install::Log for Out {
    fn line(&mut self, s: &str) {
        Out::line(self, s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh folder that is deleted again when the test ends (a junction inside is removed as a junction).
    struct Sandbox(std::path::PathBuf);

    impl std::ops::Deref for Sandbox {
        type Target = std::path::Path;
        fn deref(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn sandbox(name: &str) -> Sandbox {
        let d = std::env::temp_dir().join(format!("vcam-tool-util-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Sandbox(d)
    }

    #[test]
    fn a_missing_result_file_is_created_and_an_existing_one_is_emptied() {
        let d = sandbox("plain");
        let p = d.join("result.log");
        let mut out = Out::new(Some(&p)).unwrap();
        out.line("one");
        out.line("two");
        drop(out);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "one\ntwo\n");
        // the parent may have made it already: it is reused and emptied
        std::fs::write(&p, "stale text that is longer than the new one").unwrap();
        let mut out = Out::new(Some(&p)).unwrap();
        out.line("new");
        drop(out);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "new\n");
        // no file wanted: stdout only
        let mut none = Out::new(None).unwrap();
        none.line("only on screen");
    }

    #[test]
    fn a_hard_linked_result_file_is_refused_and_the_other_name_is_not_touched() {
        let d = sandbox("hardlink");
        let victim = d.join("victim.txt");
        std::fs::write(&victim, "precious").unwrap();
        let p = d.join("result.log");
        std::fs::hard_link(&victim, &p).unwrap();
        let e = Out::new(Some(&p)).err().expect("must be refused");
        assert!(e.contains("hard links"), "{e}");
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "precious");
    }

    #[test]
    fn a_link_in_place_of_the_result_file_is_refused_and_its_target_is_not_touched() {
        let d = sandbox("symlink");
        let victim = d.join("victim.txt");
        std::fs::write(&victim, "precious").unwrap();
        let p = d.join("result.log");
        if std::os::windows::fs::symlink_file(&victim, &p).is_err() {
            eprintln!("no privilege to create a symbolic link: skipped");
            return;
        }
        assert!(Out::new(Some(&p)).is_err());
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "precious");
        // a dangling link too: CREATE_NEW fails on it, nothing is created at its target
        std::fs::remove_file(&p).unwrap();
        let nowhere = d.join("nowhere.txt");
        std::os::windows::fs::symlink_file(&nowhere, &p).unwrap();
        assert!(Out::new(Some(&p)).is_err());
        assert!(!nowhere.exists());
    }

    #[test]
    fn a_folder_or_junction_in_place_of_the_result_file_is_refused() {
        let d = sandbox("dir");
        let p = d.join("result.log");
        std::fs::create_dir(&p).unwrap();
        assert!(Out::new(Some(&p)).is_err());
        std::fs::remove_dir(&p).unwrap();
        let target = d.join("target");
        std::fs::create_dir(&target).unwrap();
        let out = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(&p)
            .arg(&target)
            .output()
            .unwrap();
        assert!(out.status.success());
        assert!(Out::new(Some(&p)).is_err());
        assert_eq!(std::fs::read_dir(&target).unwrap().count(), 0);
        let _ = std::fs::remove_dir(&p);
    }
}
