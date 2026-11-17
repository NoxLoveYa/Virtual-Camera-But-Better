//! Install, update, repair and remove the virtual camera from the app (plan 4.3, P14).
//!
//! Two halves in one exe:
//!
//! * The elevated helper: `better-cam.exe --vcam-install [--user-sid <SID>]` and `better-cam.exe --vcam-uninstall`.
//!   `main()` runs [`parse_helper_args`] before the Tauri builder (so the single-instance plugin cannot swallow the
//!   call) and, when it is a helper call, [`run_helper`] and exits: no window. The work itself is `vcam_install`, the
//!   same code `vcam-tool` runs. Details go to `%ProgramData%\BetterCam\logs\install.log`.
//! * The app's side: [`status`] (no elevation), and [`run_action`], which starts the helper with the `runas` verb,
//!   waits, and turns the outcome into a sentence ([`outcome_to_result`]).
//!
//! Helper exit codes: [`EXIT_OK`] 0, [`EXIT_FAILED`] 1 (see `install.log`), [`EXIT_USAGE`] 2 (bad arguments),
//! [`EXIT_NOT_ELEVATED`] 5 (started without administrator rights).

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use vcam_install::log::Log;
use vcam_install::{LaunchOutcome, Launcher};

pub const ARG_INSTALL: &str = "--vcam-install";
pub const ARG_UNINSTALL: &str = "--vcam-uninstall";
pub const ARG_USER_SID: &str = "--user-sid";

pub const EXIT_OK: i32 = 0;
pub const EXIT_FAILED: i32 = 1;
pub const EXIT_USAGE: i32 = 2;
pub const EXIT_NOT_ELEVATED: i32 = 5;

/// How long the app waits for the UAC prompt and the helper (the helper itself needs seconds).
pub const HELPER_WAIT: Duration = Duration::from_secs(5 * 60);

/// What the helper was asked to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HelperCommand {
    /// Install, update or repair; `user_sid` is the user who clicked (the elevated account may be another one).
    Install { user_sid: Option<String> },
    Uninstall,
}

/// `None` for a normal start of the app; `Some(Ok(..))` for a helper call; `Some(Err(..))` for a helper call that
/// cannot be understood. `args` are the command line arguments without the program name.
pub fn parse_helper_args(args: &[String]) -> Option<Result<HelperCommand, String>> {
    let install = args.iter().any(|a| a == ARG_INSTALL);
    let uninstall = args.iter().any(|a| a == ARG_UNINSTALL);
    if !install && !uninstall {
        return None;
    }
    Some(parse_helper(args, install, uninstall))
}

fn parse_helper(args: &[String], install: bool, uninstall: bool) -> Result<HelperCommand, String> {
    if install && uninstall {
        return Err(format!("{ARG_INSTALL} and {ARG_UNINSTALL} cannot be combined"));
    }
    let mut user_sid = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            ARG_INSTALL | ARG_UNINSTALL => {}
            ARG_USER_SID => {
                let sid = it.next().ok_or_else(|| format!("{ARG_USER_SID} needs a value"))?;
                if !vcam_install::acl::valid_sid(sid) {
                    return Err(format!("'{sid}' is not a user SID"));
                }
                user_sid = Some(sid.clone());
            }
            other => return Err(format!("unknown argument '{other}'")),
        }
    }
    if uninstall {
        if user_sid.is_some() {
            return Err(format!("{ARG_USER_SID} only goes with {ARG_INSTALL}"));
        }
        Ok(HelperCommand::Uninstall)
    } else {
        Ok(HelperCommand::Install { user_sid })
    }
}

/// The elevated side. Returns the process exit code.
pub fn run_helper(cmd: HelperCommand) -> i32 {
    if !vcam_install::is_elevated() {
        eprintln!("the virtual camera helper needs administrator rights");
        return EXIT_NOT_ELEVATED;
    }
    let install = matches!(cmd, HelperCommand::Install { .. });
    let mut log = match vcam_install::FileLog::open(install) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("{e}");
            return EXIT_FAILED;
        }
    };
    let result = match cmd {
        HelperCommand::Install { user_sid } => {
            let user_sid = user_sid.or_else(|| vcam_install::current_user_sid().ok());
            log.line(&format!(
                "---- install: elevated, user {}",
                user_sid.as_deref().unwrap_or("unknown")
            ));
            locate_dll().and_then(|dll| {
                log.line(&format!("dll source: {}", dll.display()));
                vcam_install::install(
                    &vcam_install::InstallOptions {
                        user_sid,
                        dshow_class: false,
                    },
                    &dll,
                    &mut log,
                )
            })
        }
        HelperCommand::Uninstall => {
            log.line("---- uninstall: elevated");
            vcam_install::uninstall(&mut log)
        }
    };
    match result {
        Ok(()) => EXIT_OK,
        Err(e) => {
            log.line(&format!("ERROR: {e}"));
            EXIT_FAILED
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Where the DLL is

/// Where the virtual camera DLL may be, best first, for an app whose exe is `exe`.
///
/// * Bundled app (`resources` in `tauri.conf.json`): `<exe dir>\vcam\better_cam_vcam.dll`.
/// * Development (the exe sits under a `target` folder): the workspace's `target\release\better_cam_vcam.dll` that
///   `npm run vcam:build` made. The copy Tauri places next to a dev exe is only the last resort, because it can
///   be older than the build.
pub fn dll_candidates(exe: &Path) -> Vec<PathBuf> {
    let name = format!("{}.dll", vcam_protocol::DLL_BASE_NAME);
    let Some(dir) = exe.parent() else {
        return Vec::new();
    };
    let bundled = dir.join("vcam").join(&name);
    let beside = dir.join(&name);
    match dir.ancestors().find(|a| a.file_name().is_some_and(|n| n == "target")) {
        Some(target) => vec![target.join("release").join(&name), beside, bundled],
        None => vec![bundled, beside],
    }
}

/// The first candidate that `usable` accepts.
pub fn pick_dll(candidates: &[PathBuf], usable: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    candidates.iter().find(|p| usable(p)).cloned()
}

const DLL_NOT_FOUND: &str = "The virtual camera files (better_cam_vcam.dll) are missing from this copy of Better Cam. Reinstall the app, or in a development checkout run `npm run vcam:build`.";

/// The DLL this app would install: a real DLL (a build placeholder does not count), found from the running exe.
pub fn locate_dll() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    pick_dll(&dll_candidates(&exe), vcam_install::file_looks_like_dll).ok_or_else(|| DLL_NOT_FOUND.to_string())
}

// ---------------------------------------------------------------------------------------------------------------
// Status

/// `vcam_status` (plan 5.5).
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct VcamStatus {
    /// `missing`, `outdated` or `ok`.
    pub state: &'static str,
    /// Windows lists the camera.
    pub listed: bool,
    /// 0 or 1: an app has the camera open right now.
    pub consumers: u32,
}

/// Read the state without elevation. Takes a moment (Media Foundation enumeration): call it off the UI thread.
pub fn status() -> VcamStatus {
    let dll = locate_dll().ok();
    let facts = vcam_install::gather_facts(dll.as_deref());
    VcamStatus {
        state: vcam_install::state_from(&facts).as_str(),
        listed: facts.listed,
        consumers: vcam_install::consumers(),
    }
}

// ---------------------------------------------------------------------------------------------------------------
// The app starting the helper

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Install, update or repair.
    Install,
    Uninstall,
}

impl Action {
    fn verb(self) -> &'static str {
        match self {
            Action::Install => "install",
            Action::Uninstall => "remove",
        }
    }
}

/// The arguments the app gives the helper.
pub fn helper_args(action: Action, user_sid: &str) -> Vec<String> {
    match action {
        Action::Install if !user_sid.is_empty() => {
            vec![ARG_INSTALL.into(), ARG_USER_SID.into(), user_sid.into()]
        }
        Action::Install => vec![ARG_INSTALL.into()],
        Action::Uninstall => vec![ARG_UNINSTALL.into()],
    }
}

/// A sentence for what happened, or `Ok` when the helper succeeded. `log_tail` is the end of `install.log`.
pub fn outcome_to_result(action: Action, outcome: &LaunchOutcome, log_tail: &[String]) -> Result<(), String> {
    let verb = action.verb();
    match outcome {
        LaunchOutcome::Exited(c) if *c as i32 == EXIT_OK => Ok(()),
        LaunchOutcome::Cancelled => Err(format!(
            "You declined the administrator prompt, so the virtual camera was not {}.",
            match action {
                Action::Install => "installed",
                Action::Uninstall => "removed",
            }
        )),
        LaunchOutcome::TimedOut => Err(format!(
            "The administrator step to {verb} the virtual camera did not finish in time."
        )),
        LaunchOutcome::Exited(c) => {
            let code = *c as i32;
            if code == EXIT_NOT_ELEVATED {
                return Err(format!("Windows did not give the {verb} step administrator rights."));
            }
            let why = log_tail
                .iter()
                .rev()
                .find(|l| l.contains("ERROR:"))
                .or(log_tail.last())
                .map(|l| format!(": {}", strip_stamp(l)))
                .unwrap_or_default();
            Err(format!("Could not {verb} the virtual camera (exit code {code}){why}"))
        }
        LaunchOutcome::Failed(why) => Err(format!("Could not start the administrator step to {verb} the virtual camera: {why}")),
    }
}

/// `2026-11-17 01:02:03 ERROR: text` -> `text`.
fn strip_stamp(line: &str) -> &str {
    let rest = line.get(20..).filter(|_| line.as_bytes().get(4) == Some(&b'-')).unwrap_or(line);
    rest.strip_prefix("ERROR: ").unwrap_or(rest)
}

/// Start the helper with `launcher`, wait, and read what happened. Blocks until the UAC prompt is answered and the
/// helper is done.
pub fn run_action(launcher: &dyn Launcher, exe: &str, action: Action, user_sid: &str) -> Result<(), String> {
    let outcome = launcher.run_elevated(exe, &helper_args(action, user_sid), HELPER_WAIT);
    let tail = match &outcome {
        LaunchOutcome::Exited(0) => Vec::new(),
        _ => vcam_install::log::log_tail(8),
    };
    outcome_to_result(action, &outcome, &tail)
}

/// Install, update or repair: asks for administrator rights once. The DLL is looked for first, so a copy of the
/// app without it never shows a prompt.
pub fn install(launcher: &dyn Launcher) -> Result<(), String> {
    locate_dll()?;
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let sid = vcam_install::current_user_sid().unwrap_or_default();
    run_action(launcher, &exe.to_string_lossy(), Action::Install, &sid)
}

/// Remove: the engine is shut down first (frame file closed), then one prompt.
pub fn uninstall(svc: &crate::api::Service, launcher: &dyn Launcher) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let _change = svc.begin_vcam_change(true)?;
    run_action(launcher, &exe.to_string_lossy(), Action::Uninstall, "")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    const SID: &str = "S-1-5-21-1004336348-1177238915-682003330-1001";

    // ---- argument parsing

    #[test]
    fn a_normal_start_is_not_a_helper_call() {
        assert_eq!(parse_helper_args(&args(&[])), None);
        assert_eq!(parse_helper_args(&args(&["--autostarted"])), None);
        assert_eq!(parse_helper_args(&args(&["--user-sid", SID])), None);
    }

    #[test]
    fn install_takes_a_user_sid() {
        assert_eq!(
            parse_helper_args(&args(&["--vcam-install", "--user-sid", SID])),
            Some(Ok(HelperCommand::Install { user_sid: Some(SID.into()) }))
        );
        assert_eq!(
            parse_helper_args(&args(&["--user-sid", SID, "--vcam-install"])),
            Some(Ok(HelperCommand::Install { user_sid: Some(SID.into()) }))
        );
        assert_eq!(
            parse_helper_args(&args(&["--vcam-install"])),
            Some(Ok(HelperCommand::Install { user_sid: None }))
        );
    }

    #[test]
    fn uninstall_takes_nothing() {
        assert_eq!(parse_helper_args(&args(&["--vcam-uninstall"])), Some(Ok(HelperCommand::Uninstall)));
        assert!(parse_helper_args(&args(&["--vcam-uninstall", "--user-sid", SID])).unwrap().is_err());
    }

    #[test]
    fn a_broken_helper_call_is_an_error_not_a_normal_start() {
        for bad in [
            &["--vcam-install", "--vcam-uninstall"][..],
            &["--vcam-install", "--user-sid"][..],
            &["--vcam-install", "--user-sid", "S-1-5-21-1)(A;;FA;;;WD"][..],
            &["--vcam-install", "--user-sid", "not a sid"][..],
            &["--vcam-install", "--whatever"][..],
        ] {
            let r = parse_helper_args(&args(bad));
            assert!(matches!(r, Some(Err(_))), "{bad:?} gave {r:?}");
        }
    }

    // ---- the DLL path

    #[test]
    fn a_bundled_app_uses_the_resource_next_to_its_exe() {
        let c = dll_candidates(Path::new(r"C:\Program Files\Better Cam\better-cam.exe"));
        assert_eq!(
            c,
            vec![
                PathBuf::from(r"C:\Program Files\Better Cam\vcam\better_cam_vcam.dll"),
                PathBuf::from(r"C:\Program Files\Better Cam\better_cam_vcam.dll"),
            ]
        );
    }

    #[test]
    fn a_dev_run_prefers_the_workspace_release_build() {
        let c = dll_candidates(Path::new(r"D:\work\repo\target\debug\better-cam.exe"));
        assert_eq!(c[0], PathBuf::from(r"D:\work\repo\target\release\better_cam_vcam.dll"));
        assert_eq!(c.len(), 3);
        // a release exe in target\release has the DLL right beside it
        let r = dll_candidates(Path::new(r"D:\work\repo\target\release\better-cam.exe"));
        assert_eq!(r[0], PathBuf::from(r"D:\work\repo\target\release\better_cam_vcam.dll"));
    }

    #[test]
    fn the_first_usable_candidate_wins_and_a_placeholder_is_skipped() {
        let c = dll_candidates(Path::new(r"D:\work\repo\target\debug\better-cam.exe"));
        // only the Tauri copy beside the exe is "usable": the build in target\release is missing
        let beside = c[1].clone();
        assert_eq!(pick_dll(&c, |p| p == beside), Some(beside.clone()));
        // both usable: the release build, not the copy
        assert_eq!(pick_dll(&c, |_| true), Some(c[0].clone()));
        assert_eq!(pick_dll(&c, |_| false), None);
        assert_eq!(pick_dll(&[], |_| true), None);
    }

    #[test]
    fn a_real_file_check_rejects_the_build_placeholder() {
        let dir = std::env::temp_dir().join(format!("better-cam-dll-pick-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("better_cam_vcam.dll");
        std::fs::write(&fake, b"placeholder").unwrap();
        assert_eq!(pick_dll(std::slice::from_ref(&fake), vcam_install::file_looks_like_dll), None);
        let mut real = vec![0u8; 70_000];
        real[..2].copy_from_slice(b"MZ");
        std::fs::write(&fake, &real).unwrap();
        assert_eq!(
            pick_dll(std::slice::from_ref(&fake), vcam_install::file_looks_like_dll),
            Some(fake)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- the helper's arguments and the outcome of a launch

    #[test]
    fn the_app_gives_the_helper_the_users_sid() {
        assert_eq!(helper_args(Action::Install, SID), args(&["--vcam-install", "--user-sid", SID]));
        assert_eq!(helper_args(Action::Install, ""), args(&["--vcam-install"]));
        assert_eq!(helper_args(Action::Uninstall, SID), args(&["--vcam-uninstall"]));
        // what the app sends, the helper accepts
        assert_eq!(
            parse_helper_args(&helper_args(Action::Install, SID)),
            Some(Ok(HelperCommand::Install { user_sid: Some(SID.into()) }))
        );
    }

    struct Fake {
        outcome: LaunchOutcome,
        seen: RefCell<Vec<(String, Vec<String>)>>,
    }

    impl Launcher for Fake {
        fn run_elevated(&self, exe: &str, a: &[String], _: Duration) -> LaunchOutcome {
            self.seen.borrow_mut().push((exe.into(), a.to_vec()));
            self.outcome.clone()
        }
    }

    fn fake(outcome: LaunchOutcome) -> Fake {
        Fake { outcome, seen: RefCell::new(Vec::new()) }
    }

    #[test]
    fn a_declined_prompt_says_so() {
        let f = fake(LaunchOutcome::Cancelled);
        let err = run_action(&f, r"C:\app\better-cam.exe", Action::Install, SID).unwrap_err();
        assert!(err.contains("declined the administrator prompt"), "{err}");
        assert!(err.contains("not installed"), "{err}");
        let err = run_action(&f, r"C:\app\better-cam.exe", Action::Uninstall, "").unwrap_err();
        assert!(err.contains("declined the administrator prompt") && err.contains("not removed"), "{err}");
        let seen = f.seen.borrow();
        assert_eq!(seen[0].0, r"C:\app\better-cam.exe");
        assert_eq!(seen[0].1, args(&["--vcam-install", "--user-sid", SID]));
        assert_eq!(seen[1].1, args(&["--vcam-uninstall"]));
    }

    #[test]
    fn other_outcomes_do_not_look_like_a_refusal() {
        assert!(outcome_to_result(Action::Install, &LaunchOutcome::Exited(0), &[]).is_ok());
        let e = outcome_to_result(Action::Install, &LaunchOutcome::TimedOut, &[]).unwrap_err();
        assert!(e.contains("did not finish") && !e.contains("declined"), "{e}");
        let e = outcome_to_result(Action::Install, &LaunchOutcome::Failed("boom".into()), &[]).unwrap_err();
        assert!(e.contains("boom") && !e.contains("declined"), "{e}");
        let e = outcome_to_result(Action::Uninstall, &LaunchOutcome::Exited(EXIT_NOT_ELEVATED as u32), &[]).unwrap_err();
        assert!(e.contains("administrator rights") && !e.contains("declined"), "{e}");
    }

    #[test]
    fn a_failed_helper_reports_the_error_line_of_the_log() {
        let tail = vec![
            "2026-11-17 01:02:03 dll copied: C:\\x".to_string(),
            "2026-11-17 01:02:04 ERROR: MFCreateVirtualCamera: boom".to_string(),
            "2026-11-17 01:02:05 something after".to_string(),
        ];
        let e = outcome_to_result(Action::Install, &LaunchOutcome::Exited(1), &tail).unwrap_err();
        assert!(e.contains("exit code 1") && e.ends_with("MFCreateVirtualCamera: boom"), "{e}");
        let e = outcome_to_result(Action::Install, &LaunchOutcome::Exited(1), &[]).unwrap_err();
        assert!(e.contains("exit code 1"), "{e}");
    }

    #[test]
    fn the_status_names_match_the_command_contract() {
        let json = serde_json::to_value(VcamStatus { state: "outdated", listed: true, consumers: 1 }).unwrap();
        assert_eq!(json, serde_json::json!({ "state": "outdated", "listed": true, "consumers": 1 }));
    }
}
