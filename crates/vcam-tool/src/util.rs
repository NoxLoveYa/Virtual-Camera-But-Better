//! Small helpers shared by the subcommands.
//!
//! `register` and `unregister` run their work in an elevated copy of this program. What that copy reports comes
//! back through **`install.log`** (`%ProgramData%\BetterCam\logs`, written by the helper through verified handles and
//! readable by the user), never through a file the unelevated parent names: a path in the user's `%TEMP%` can be
//! rearranged by the user (a junction in any folder of the chain), and the child writes with administrator rights.
//! The parent only *reads* what the child wrote there, and the exit code says how it ended.

use std::time::SystemTime;

use vcam_install::util::Paths;

/// How many lines of `install.log` the parent looks through to find this run (a run writes a few dozen).
const LOOKBACK: usize = 400;

/// The first line the elevated copy writes: the parent prints what follows the last one of these.
pub fn marker(verb: &str) -> String {
    format!("---- vcam-tool {verb}: elevated run")
}

/// The lines the elevated copy of `verb` wrote to `install.log` in `paths` since `started`: everything after the
/// last [`marker`]. Empty when the file is older than `started` (nothing was written by this run, so whatever is
/// in it belongs to another one) or cannot be read the way the helper writes it (no link, one hard link, the right
/// place).
pub fn run_output(paths: &Paths, verb: &str, started: SystemTime) -> Vec<String> {
    let lines = vcam_install::log::tail_of(paths, started, LOOKBACK);
    let mark = marker(verb);
    match lines.iter().rposition(|l| l.contains(&mark)) {
        Some(i) => lines[i..].to_vec(),
        None => Vec::new(),
    }
}

/// [`run_output`] on this machine's folders.
pub fn run_output_here(verb: &str, started: SystemTime) -> Vec<String> {
    run_output(&Paths::system(), verb, started)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::time::Duration;

    struct Sandbox(std::path::PathBuf);

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A machine layout below the temp folder with an `install.log` that holds `lines`.
    fn machine(name: &str, lines: &[&str]) -> (Sandbox, Paths) {
        let root =
            std::env::temp_dir().join(format!("vcam-tool-util-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let paths = Paths::below(&root);
        std::fs::create_dir_all(paths.logs()).unwrap();
        let mut f = std::fs::File::create(paths.install_log()).unwrap();
        for l in lines {
            writeln!(f, "2026-11-17 01:02:03 {l}").unwrap();
        }
        (Sandbox(root), paths)
    }

    fn long_ago() -> SystemTime {
        SystemTime::now() - Duration::from_secs(3600)
    }

    #[test]
    fn the_parent_prints_only_what_the_last_elevated_run_wrote() {
        let (_s, paths) = machine(
            "last-run",
            &[
                "---- vcam-tool register: elevated run",
                "an earlier run's line",
                "ERROR: an earlier failure",
                "---- vcam-tool unregister: elevated run",
                "another verb",
                "---- vcam-tool register: elevated run",
                "data folder ready",
                "ERROR: this run failed",
            ],
        );
        let out = run_output(&paths, "register", long_ago());
        assert_eq!(out.len(), 3, "{out:?}");
        assert!(out[0].contains("elevated run") && out[2].contains("this run failed"));
        assert!(!out.iter().any(|l| l.contains("earlier")));
        // the other verb has its own run
        let out = run_output(&paths, "unregister", long_ago());
        assert!(out.is_empty() || out[0].contains("unregister"), "{out:?}");
    }

    #[test]
    fn a_log_that_this_run_did_not_write_says_nothing() {
        let (_s, paths) = machine("stale", &["---- vcam-tool register: elevated run", "old"]);
        let later = SystemTime::now() + Duration::from_secs(60);
        assert!(run_output(&paths, "register", later).is_empty());
        // no marker: nothing of this run
        let (_s2, paths2) = machine("no-marker", &["some line"]);
        assert!(run_output(&paths2, "register", long_ago()).is_empty());
        // no file at all
        let (_s3, paths3) = machine("no-file", &[]);
        std::fs::remove_file(paths3.install_log()).unwrap();
        assert!(run_output(&paths3, "register", long_ago()).is_empty());
    }

    /// The parent trusts the file as little as the helper does: a hard link or a link in place of `install.log`
    /// is not read (it could print any file of the machine, or be a way to read what the user may not).
    #[test]
    fn a_link_in_place_of_the_log_is_not_read() {
        let (s, paths) = machine("links", &[]);
        let victim = s.0.join("secret.txt");
        std::fs::write(
            &victim,
            "2026-11-17 01:02:03 ---- vcam-tool register: elevated run\nsecret\n",
        )
        .unwrap();
        std::fs::remove_file(paths.install_log()).unwrap();
        std::fs::hard_link(&victim, paths.install_log()).unwrap();
        assert!(run_output(&paths, "register", long_ago()).is_empty());
        std::fs::remove_file(paths.install_log()).unwrap();
        if std::os::windows::fs::symlink_file(&victim, paths.install_log()).is_ok() {
            assert!(run_output(&paths, "register", long_ago()).is_empty());
        } else {
            eprintln!("no privilege to create a symbolic link: skipped");
        }
        // a junction as the logs folder
        let _ = std::fs::remove_file(paths.install_log());
        std::fs::remove_dir_all(paths.logs()).unwrap();
        let elsewhere = s.0.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(
            elsewhere.join("install.log"),
            "2026-11-17 01:02:03 ---- vcam-tool register: elevated run\nvia a junction\n",
        )
        .unwrap();
        let out = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(paths.logs())
            .arg(&elsewhere)
            .output()
            .unwrap();
        assert!(out.status.success());
        assert!(run_output(&paths, "register", long_ago()).is_empty());
        let _ = std::fs::remove_dir(paths.logs());
    }
}
