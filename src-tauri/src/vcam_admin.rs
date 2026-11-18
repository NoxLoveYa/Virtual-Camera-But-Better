//! Install, update, repair and remove the virtual camera from the app (plan 4.3, P14).
//!
//! Two halves in one exe:
//!
//! * The elevated helper: `better-cam.exe --vcam-install --user-sid <SID>` and `better-cam.exe --vcam-uninstall`.
//!   `main()` runs [`parse_helper_args`] before the Tauri builder (so the single-instance plugin cannot swallow the
//!   call) and, when it is a helper call, [`run_helper`] and exits: no window. The work itself is `vcam_install`, the
//!   same code `vcam-tool` runs. Details go to `%ProgramData%\BetterCam\logs\install.log`.
//! * The app's side: [`status`] (no elevation), and [`run_action`], which starts the helper with the `runas` verb,
//!   waits, and turns the outcome into a sentence ([`outcome_to_result`]).
//!
//! Helper exit codes: [`EXIT_OK`] 0, [`EXIT_FAILED`] 1 (see `install.log`), [`EXIT_USAGE`] 2 (bad arguments),
//! [`EXIT_NOT_ELEVATED`] 5 (started without administrator rights), [`EXIT_PENDING`] 10 (done, but it takes effect
//! only after the camera service restarts or Windows restarts), [`EXIT_BUSY`] 11 (another install, repair or removal
//! is running on this machine: the helper took no action).
//!
//! The app waits for the helper to *exit*, however long the Windows prompt stays open: there is no timeout, so the
//! guard that refuses a second action is held for exactly as long as a helper may still be running. That guard lives
//! in one app process; the helper itself takes a machine-wide lock (`vcam_install::lock`) before it touches
//! anything, so an app restarted while a helper still runs, a second copy of the app and `vcam-tool` are refused too.
//!
//! **Whose camera it is.** The frame file belongs to *one* Windows account at a time: the helper is told which
//! (`--user-sid`, always: the account that answers the administrator prompt may be another one, and guessing would
//! hand the camera to the wrong account). Installing or repairing from another Windows account replaces the
//! permissions, so the virtual camera works for that account from then on and no longer for the earlier one, until
//! that account repairs it again. The windows of the app say so.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::Serialize;
use vcam_install::log::Log;
use vcam_install::{InstallOutcome, LaunchOutcome, Launcher, UninstallOutcome};

use crate::api::Service;

pub const ARG_INSTALL: &str = "--vcam-install";
pub const ARG_UNINSTALL: &str = "--vcam-uninstall";
pub const ARG_USER_SID: &str = "--user-sid";

pub const EXIT_OK: i32 = 0;
pub const EXIT_FAILED: i32 = 1;
pub const EXIT_USAGE: i32 = 2;
pub const EXIT_NOT_ELEVATED: i32 = 5;
/// Done, but the camera service still has the old build loaded (install) or a file waits for a restart (remove).
pub const EXIT_PENDING: i32 = 10;
/// Another install, repair or removal is running on this machine (see `vcam_install::lock`); nothing was done.
pub const EXIT_BUSY: i32 = 11;

/// What the helper was asked to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HelperCommand {
    /// Install, update or repair; `user_sid` is the user who clicked (the elevated account may be another one). It
    /// is not optional: without it the helper would have to guess, and the guess is the administrator's account.
    Install { user_sid: String },
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
        // The app always gives it. A helper without it would use the account that is elevated, which with an
        // over-the-shoulder prompt is the administrator's and not the user's.
        let user_sid = user_sid.ok_or_else(|| format!("{ARG_INSTALL} needs {ARG_USER_SID} <SID>"))?;
        Ok(HelperCommand::Install { user_sid })
    }
}

/// The elevated side. Returns the process exit code.
pub fn run_helper(cmd: HelperCommand) -> i32 {
    if !vcam_install::is_elevated() {
        eprintln!("the virtual camera helper needs administrator rights");
        return EXIT_NOT_ELEVATED;
    }
    let user_sid = match &cmd {
        HelperCommand::Install { user_sid } => Some(user_sid.as_str()),
        HelperCommand::Uninstall => None,
    };
    // One helper at a time on the machine, before anything is touched (or written to install.log, which belongs to
    // the run that holds the lock).
    let _lock = match vcam_install::lock::acquire() {
        Ok(vcam_install::lock::Acquired::Held(l)) => l,
        Ok(vcam_install::lock::Acquired::Busy) => {
            eprintln!("{}", vcam_install::lock::BUSY_MESSAGE);
            return EXIT_BUSY;
        }
        Err(e) => {
            let mut log = vcam_install::FileLog::open_for(user_sid);
            log.line(&format!("ERROR: {e}"));
            return EXIT_FAILED;
        }
    };
    // Opens install.log only when the folder is already there and as it must be; the install makes it and the log
    // follows (see `FileLog`). Nothing here creates or follows anything in the data folder. A log that is dropped
    // writes what still waits for a file, so the `ERROR:` line below is in the file when the process ends.
    let mut log = vcam_install::FileLog::open_for(user_sid);
    let result: Result<i32, String> = match cmd {
        HelperCommand::Install { user_sid } => {
            log.line(&format!("{} elevated, user {user_sid}", vcam_install::log::INSTALL_MARKER));
            locate_dll().and_then(|dll| {
                log.line(&format!("dll source: {}", dll.display()));
                vcam_install::install(
                    &vcam_install::InstallOptions {
                        user_sid: Some(user_sid),
                        dshow_class: false,
                    },
                    &dll,
                    &mut log,
                )
                .map(|o| match o {
                    InstallOutcome::Done => EXIT_OK,
                    InstallOutcome::PendingServiceRestart => EXIT_PENDING,
                })
            })
        }
        HelperCommand::Uninstall => {
            log.line(&format!("{} elevated", vcam_install::log::UNINSTALL_MARKER));
            vcam_install::uninstall(&mut log).map(|o| match o {
                UninstallOutcome::Done => EXIT_OK,
                UninstallOutcome::PendingReboot => EXIT_PENDING,
            })
        }
    };
    let code = match result {
        Ok(code) => code,
        Err(e) => {
            log.line(&format!("ERROR: {e}"));
            EXIT_FAILED
        }
    };
    log.flush();
    code
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

/// What the app says when it cannot tell which Windows account runs it: the helper must be told, so nothing starts.
pub const NO_ACCOUNT: &str = "Could not tell which Windows account is running Better Cam, so the virtual camera was not installed. Sign out of Windows and in again, then try once more.";

/// The arguments the app gives the helper. An install always carries the user's SID: without one there is no
/// helper to start (the helper would guess the account, see the module docs).
pub fn helper_args(action: Action, user_sid: &str) -> Result<Vec<String>, String> {
    match action {
        Action::Install if user_sid.is_empty() => Err(NO_ACCOUNT.into()),
        Action::Install => Ok(vec![ARG_INSTALL.into(), ARG_USER_SID.into(), user_sid.into()]),
        Action::Uninstall => Ok(vec![ARG_UNINSTALL.into()]),
    }
}

/// How an action that did not fail ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionDone {
    Done,
    /// Installed (or removed), but it takes effect only after the camera service restarts or Windows restarts.
    PendingRestart,
}

impl ActionDone {
    /// The word the frontend gets from the command.
    pub fn as_str(self) -> &'static str {
        match self {
            ActionDone::Done => "done",
            ActionDone::PendingRestart => "pending_restart",
        }
    }
}

/// A sentence for what happened, or how it went when the helper succeeded. `log_tail` is the end of `install.log`.
pub fn outcome_to_result(action: Action, outcome: &LaunchOutcome, log_tail: &[String]) -> Result<ActionDone, String> {
    outcome_to_result_or(action, outcome, log_tail, &|| None)
}

/// [`outcome_to_result`] that asks `no_log_reason` for a reason when an install failed and its log has no `ERROR:`
/// line (the helper can fail before the log folder exists: on a fresh install into an encrypted `%ProgramData%` it
/// refuses before `logs` is made, so nothing was written). Asked only then, and only for an install.
pub fn outcome_to_result_or(
    action: Action,
    outcome: &LaunchOutcome,
    log_tail: &[String],
    no_log_reason: &dyn Fn() -> Option<String>,
) -> Result<ActionDone, String> {
    let verb = action.verb();
    match outcome {
        LaunchOutcome::Exited(c) if *c as i32 == EXIT_OK => Ok(ActionDone::Done),
        LaunchOutcome::Exited(c) if *c as i32 == EXIT_PENDING => Ok(ActionDone::PendingRestart),
        LaunchOutcome::Cancelled => Err(format!(
            "You declined the administrator prompt, so the virtual camera was not {}.",
            match action {
                Action::Install => "installed",
                Action::Uninstall => "removed",
            }
        )),
        LaunchOutcome::Exited(c) => {
            let code = *c as i32;
            if code == EXIT_NOT_ELEVATED {
                return Err(format!("Windows did not give the {verb} step administrator rights."));
            }
            if code == EXIT_BUSY {
                return Err(vcam_install::lock::BUSY_MESSAGE.to_string());
            }
            // Only the helper's own `ERROR:` line is a reason. The last line of the log is not: it is whatever the
            // helper happened to write before it stopped, and the real cause may never have reached the file.
            let why = log_tail
                .iter()
                .rev()
                .find(|l| l.contains("ERROR:"))
                .map(|l| strip_stamp(l).to_string())
                .or_else(|| (action == Action::Install).then(no_log_reason).flatten())
                .map(|w| format!(": {w}"))
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
/// helper has exited, however long that takes (see the module docs).
pub fn run_action(launcher: &dyn Launcher, exe: &str, action: Action, user_sid: &str) -> Result<ActionDone, String> {
    // before any prompt: an install that cannot say whose camera it is does not start
    let args = helper_args(action, user_sid)?;
    let started = SystemTime::now();
    let outcome = launcher.run_elevated(exe, &args);
    let failed = !matches!(&outcome, LaunchOutcome::Exited(c) if *c as i32 == EXIT_OK || *c as i32 == EXIT_PENDING || *c as i32 == EXIT_BUSY);
    let tail = if failed {
        // only what this run wrote (from its own marker on): an older run's lines say nothing about it
        vcam_install::log::log_tail_since(started, 8)
    } else {
        Vec::new()
    };
    // unelevated and read-only (attributes only): the app can say what the helper could not write down
    let result = outcome_to_result_or(action, &outcome, &tail, &|| {
        vcam_install::datadir::encryption_problem(&vcam_install::util::Paths::system())
    });
    // a helper that failed leaves its reason in install.log; when a program of the user holds a lock on that file
    // neither the helper could write it nor can this read it: say that instead of showing no reason
    match result {
        Err(msg) if failed && matches!(outcome, LaunchOutcome::Exited(_)) && vcam_install::log::install_log_locked() => {
            Err(format!("{msg} {LOG_LOCKED}"))
        }
        other => other,
    }
}

/// Added to a failure when `install.log` is locked by another program (see `run_action`).
const LOG_LOCKED: &str =
    "The details cannot be shown: another program has the install log (ProgramData\\BetterCam\\logs\\install.log) open and locked, so the helper could not write them either. Close that program and run the action again; what was done stays done.";

/// [`run_action`] under the claim on the virtual camera: one change at a time, the engine (and its handle on
/// `frames.bin`, which the helper may have to replace) released first. The claim is dropped when the helper has
/// exited, not before, so a second action is refused for as long as a helper (or its prompt) is alive. Dropping it
/// brings the engine back to what it was doing (not after a removal).
pub fn run_claimed(
    svc: &Service,
    launcher: &dyn Launcher,
    exe: &str,
    action: Action,
    user_sid: &str,
) -> Result<ActionDone, String> {
    let mut change = svc.begin_vcam_change(true)?;
    let done = run_action(launcher, exe, action, user_sid);
    if action == Action::Uninstall && done.is_ok() {
        // The virtual camera is gone: the engine has nothing to write to and stays released. A removal that was
        // declined or failed leaves the camera where it was, and the engine comes back to what it was doing.
        change.keep_engine_released();
    }
    done
}

/// Install, update or repair: asks for administrator rights once. The DLL is looked for first, so a copy of the
/// app without it never shows a prompt.
pub fn install(svc: &Service, launcher: &dyn Launcher) -> Result<ActionDone, String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    // no SID, no helper: the message says so (see `run_action`); nothing is released or started before that
    let sid = vcam_install::current_user_sid().unwrap_or_default();
    helper_args(Action::Install, &sid)?;
    // Dropped at the end of this function: the engine comes back by itself, after a success and after a refusal.
    let _change = svc.begin_vcam_change(true)?;
    locate_dll()?;
    run_action(launcher, &exe.to_string_lossy(), Action::Install, &sid)
}

/// Remove: the engine is shut down first (frame file closed), then one prompt.
pub fn uninstall(svc: &Service, launcher: &dyn Launcher) -> Result<ActionDone, String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    run_claimed(svc, launcher, &exe.to_string_lossy(), Action::Uninstall, "")
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
            Some(Ok(HelperCommand::Install { user_sid: SID.into() }))
        );
        assert_eq!(
            parse_helper_args(&args(&["--user-sid", SID, "--vcam-install"])),
            Some(Ok(HelperCommand::Install { user_sid: SID.into() }))
        );
    }

    /// L8: an install without the user's SID would use the elevated account's, which with an over-the-shoulder
    /// prompt is the administrator's. The helper refuses to start instead.
    #[test]
    fn an_install_without_the_users_sid_is_a_usage_error() {
        let e = parse_helper_args(&args(&["--vcam-install"])).unwrap().unwrap_err();
        assert!(e.contains("--user-sid"), "{e}");
        assert!(parse_helper_args(&args(&["--vcam-install", "--user-sid"])).unwrap().is_err());
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
        assert_eq!(helper_args(Action::Install, SID).unwrap(), args(&["--vcam-install", "--user-sid", SID]));
        assert_eq!(helper_args(Action::Uninstall, SID).unwrap(), args(&["--vcam-uninstall"]));
        // what the app sends, the helper accepts
        assert_eq!(
            parse_helper_args(&helper_args(Action::Install, SID).unwrap()),
            Some(Ok(HelperCommand::Install { user_sid: SID.into() }))
        );
    }

    /// L8: when the app cannot read its own user's SID it must not start a helper without one (the old code left
    /// the argument out and the helper then used the elevated account). The user gets a clear sentence and no
    /// prompt.
    #[test]
    fn without_the_users_sid_no_helper_is_started_and_the_message_says_why() {
        assert_eq!(helper_args(Action::Install, "").unwrap_err(), NO_ACCOUNT);
        let f = fake(LaunchOutcome::Exited(0));
        let e = run_action(&f, r"C:\app\better-cam.exe", Action::Install, "").unwrap_err();
        assert!(e.contains("which Windows account"), "{e}");
        assert!(f.seen.borrow().is_empty(), "a helper was started without the user's SID");
        // a removal needs no account
        assert_eq!(
            run_action(&f, r"C:\app\better-cam.exe", Action::Uninstall, ""),
            Ok(ActionDone::Done)
        );
        // and the claim is not taken for nothing: `install` checks before it releases the engine
        let svc = Service::new();
        assert!(helper_args(Action::Install, "").is_err());
        assert!(svc.begin_vcam_change(false).is_ok());
    }

    /// L6: a second helper exits with its own code and the app shows what that means, not "exit code 11".
    #[test]
    fn a_helper_that_found_another_one_running_says_so() {
        assert_eq!(EXIT_BUSY, 11);
        for action in [Action::Install, Action::Uninstall] {
            let e = outcome_to_result(action, &LaunchOutcome::Exited(EXIT_BUSY as u32), &[]).unwrap_err();
            assert!(e.contains("already running"), "{e}");
            assert!(!e.contains("exit code"), "{e}");
        }
        // run through the launcher: the log is not even looked at (it belongs to the run that is still going)
        let f = fake(LaunchOutcome::Exited(EXIT_BUSY as u32));
        let e = run_action(&f, "x.exe", Action::Install, SID).unwrap_err();
        assert!(e.contains("already running"), "{e}");
        assert_ne!(EXIT_BUSY, EXIT_OK);
        assert_ne!(EXIT_BUSY, EXIT_PENDING);
        assert_ne!(EXIT_BUSY, EXIT_FAILED);
    }

    struct Fake {
        outcome: LaunchOutcome,
        seen: RefCell<Vec<(String, Vec<String>)>>,
    }

    impl Launcher for Fake {
        fn run_elevated(&self, exe: &str, a: &[String]) -> LaunchOutcome {
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
        assert_eq!(outcome_to_result(Action::Install, &LaunchOutcome::Exited(0), &[]), Ok(ActionDone::Done));
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

    /// M3: when the `ERROR:` line did not reach the log, the last line that did is not the reason (the old code
    /// showed it as one: "Could not install ...: data folder: kept").
    #[test]
    fn a_log_without_an_error_line_gives_no_reason() {
        let tail = vec![
            "2026-11-17 01:02:03 C:\\ProgramData\\BetterCam: kept".to_string(),
            "2026-11-17 01:02:04 C:\\ProgramData\\BetterCam\\logs: kept".to_string(),
        ];
        let e = outcome_to_result(Action::Install, &LaunchOutcome::Exited(1), &tail).unwrap_err();
        assert_eq!(e, "Could not install the virtual camera (exit code 1)");
    }

    /// A fresh install into an encrypted `%ProgramData%` is refused before `logs` exists, so there is no `ERROR:`
    /// line: the sentence of the status check is the reason. An `ERROR:` line, when there is one, wins; the check is
    /// not asked for it, nor for a removal, a success, a refused prompt or a busy lock.
    #[test]
    fn a_failed_install_without_an_error_line_gives_the_encryption_sentence_when_there_is_one() {
        use std::cell::Cell;
        let asked = Cell::new(0);
        let sentence = || {
            asked.set(asked.get() + 1);
            Some("The folder X is encrypted (Windows EFS)".to_string())
        };
        let exited = LaunchOutcome::Exited(1);
        let e = outcome_to_result_or(Action::Install, &exited, &[], &sentence).unwrap_err();
        assert_eq!(e, "Could not install the virtual camera (exit code 1): The folder X is encrypted (Windows EFS)");
        assert_eq!(asked.get(), 1);
        // nothing to say: as before
        let e = outcome_to_result_or(Action::Install, &exited, &[], &|| None).unwrap_err();
        assert_eq!(e, "Could not install the virtual camera (exit code 1)");
        // the helper's own line is the reason, and the check is not asked
        let tail = vec!["2026-11-17 01:02:04 ERROR: MFCreateVirtualCamera: boom".to_string()];
        let e = outcome_to_result_or(Action::Install, &exited, &tail, &sentence).unwrap_err();
        assert!(e.ends_with(": MFCreateVirtualCamera: boom") && !e.contains("encrypted"), "{e}");
        assert_eq!(asked.get(), 1);
        // only an install: a failed removal is not about the data folder being encrypted
        let e = outcome_to_result_or(Action::Uninstall, &exited, &[], &sentence).unwrap_err();
        assert_eq!(e, "Could not remove the virtual camera (exit code 1)");
        assert_eq!(asked.get(), 1);
        // none of the other outcomes asks
        for o in [
            LaunchOutcome::Cancelled,
            LaunchOutcome::Exited(EXIT_BUSY as u32),
            LaunchOutcome::Exited(EXIT_NOT_ELEVATED as u32),
            LaunchOutcome::Failed("boom".into()),
        ] {
            let e = outcome_to_result_or(Action::Install, &o, &[], &sentence).unwrap_err();
            assert!(!e.contains("encrypted"), "{e}");
        }
        assert_eq!(outcome_to_result_or(Action::Install, &LaunchOutcome::Exited(0), &[], &sentence), Ok(ActionDone::Done));
        assert_eq!(asked.get(), 1);
    }

    #[test]
    fn a_pending_exit_code_is_a_success_with_a_different_word() {
        let done = outcome_to_result(Action::Install, &LaunchOutcome::Exited(EXIT_PENDING as u32), &[]);
        assert_eq!(done, Ok(ActionDone::PendingRestart));
        assert_eq!(ActionDone::PendingRestart.as_str(), "pending_restart");
        assert_eq!(ActionDone::Done.as_str(), "done");
        assert_ne!(EXIT_PENDING, EXIT_OK);
        assert_ne!(EXIT_PENDING, EXIT_FAILED);
        // for a remove too
        assert_eq!(
            outcome_to_result(Action::Uninstall, &LaunchOutcome::Exited(EXIT_PENDING as u32), &[]),
            Ok(ActionDone::PendingRestart)
        );
        // an ordinary failure stays a failure
        assert!(outcome_to_result(Action::Install, &LaunchOutcome::Exited(1), &[]).is_err());
    }

    // ---- one helper at a time

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};

    /// A launcher whose prompt stays open until the test lets it go: "the user has not answered yet", for as long as
    /// the test wants (the old code gave up after five minutes and let a second helper start).
    struct Prompt {
        calls: AtomicUsize,
        entered: Mutex<mpsc::Sender<()>>,
        answer: Mutex<mpsc::Receiver<LaunchOutcome>>,
    }

    impl Launcher for Prompt {
        fn run_elevated(&self, _: &str, _: &[String]) -> LaunchOutcome {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.entered.lock().unwrap().send(()).unwrap();
            self.answer.lock().unwrap().recv().unwrap()
        }
    }

    #[test]
    fn a_second_action_is_refused_for_as_long_as_the_first_helper_has_not_exited() {
        let svc = Arc::new(Service::new());
        let (entered_tx, entered_rx) = mpsc::channel();
        let (answer_tx, answer_rx) = mpsc::channel();
        let prompt = Arc::new(Prompt {
            calls: AtomicUsize::new(0),
            entered: Mutex::new(entered_tx),
            answer: Mutex::new(answer_rx),
        });
        let first = {
            let (svc, prompt) = (svc.clone(), prompt.clone());
            std::thread::spawn(move || run_claimed(&svc, &*prompt, r"C:\app\better-cam.exe", Action::Install, SID))
        };
        entered_rx.recv().unwrap(); // the prompt is open

        // the prompt sits there for a while: nothing times out, nothing else may start
        for _ in 0..5 {
            std::thread::sleep(std::time::Duration::from_millis(40));
            let e = run_claimed(&svc, &*prompt, r"C:\app\better-cam.exe", Action::Uninstall, "").unwrap_err();
            assert!(e.contains("already running"), "{e}");
            let e = run_claimed(&svc, &*prompt, r"C:\app\better-cam.exe", Action::Install, SID).unwrap_err();
            assert!(e.contains("already running"), "{e}");
            assert!(svc.begin_vcam_change(false).is_err());
            assert!(svc.engine().is_err(), "no engine may map frames.bin while a helper may replace it");
        }
        assert_eq!(prompt.calls.load(Ordering::SeqCst), 1, "a second helper was started");

        // the prompt is answered, the helper exits: the claim is free again and the first call reports its result
        answer_tx.send(LaunchOutcome::Exited(0)).unwrap();
        assert_eq!(first.join().unwrap(), Ok(ActionDone::Done));
        let again = svc.begin_vcam_change(false);
        assert!(again.is_ok());
    }

    #[test]
    fn a_declined_prompt_frees_the_claim_too() {
        let svc = Service::new();
        let f = fake(LaunchOutcome::Cancelled);
        assert!(run_claimed(&svc, &f, "x.exe", Action::Install, SID).is_err());
        assert!(svc.begin_vcam_change(true).is_ok());
    }

    /// A service whose engines run on WARP and write to temporary files (never the real `frames.bin`).
    fn warp_service() -> Service {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!("bettercam-vadmin-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Service::with_maker(Arc::new(move || {
            let path = dir.join(format!("engine-{}.bin", N.fetch_add(1, Ordering::SeqCst)));
            let _ = std::fs::remove_file(&path);
            let gpu = Arc::new(crate::gpu::GpuDevice::new(crate::gpu::AdapterKind::Warp).map_err(|e| e.to_string())?);
            let writer = Arc::new(vcam_protocol::Writer::create(&path).map_err(|e| e.to_string())?);
            crate::engine::Engine::with_parts(gpu, writer)
        }))
    }

    fn running(svc: &Service) {
        use crate::engine::{OutputSettings, ReadbackPolicy, Settings, SourceSpec, SyntheticSpec};
        use crate::gpu::convert::ColorSettings;
        svc.start_with(
            Settings {
                source: SourceSpec::Synthetic(SyntheticSpec {
                    width: 320,
                    height: 180,
                    fps: 30,
                    format: crate::capture::PixelFormat::Nv12,
                    color: ColorSettings::default(),
                }),
                output: OutputSettings::default(),
                flip_vertical: false,
                color: ColorSettings::default(),
                readback: ReadbackPolicy::Immediate,
                deactivate_when_not_showing: false,
            },
            None,
            None,
        )
        .unwrap();
        svc.engine().unwrap().wait_live(std::time::Duration::from_secs(20)).unwrap();
    }

    /// L3: a removal that was declined or failed leaves the virtual camera where it was, and the engine comes back
    /// to what it was doing; only one that worked leaves it released.
    #[test]
    fn a_removal_that_did_not_happen_gives_the_engine_back() {
        for outcome in [
            LaunchOutcome::Cancelled,
            LaunchOutcome::Failed("no prompt".into()),
            LaunchOutcome::Exited(1),
            LaunchOutcome::Exited(EXIT_BUSY as u32),
        ] {
            let svc = warp_service();
            running(&svc);
            let result = run_claimed(&svc, &fake(outcome.clone()), "x.exe", Action::Uninstall, "");
            assert!(result.is_err(), "{outcome:?}");
            let e = svc.engine().unwrap();
            e.wait_live(std::time::Duration::from_secs(20)).unwrap_or_else(|why| panic!("{outcome:?}: {why}"));
        }
        // the removal worked: nothing is brought back (the next use makes an engine, which runs nothing)
        let svc = warp_service();
        running(&svc);
        assert_eq!(run_claimed(&svc, &fake(LaunchOutcome::Exited(0)), "x.exe", Action::Uninstall, ""), Ok(ActionDone::Done));
        assert!(!svc.engine().unwrap().is_started());
        // an install or a repair that did not happen brings it back too
        let svc = warp_service();
        running(&svc);
        assert!(run_claimed(&svc, &fake(LaunchOutcome::Cancelled), "x.exe", Action::Install, SID).is_err());
        svc.engine().unwrap().wait_live(std::time::Duration::from_secs(20)).unwrap();
    }

    #[test]
    fn the_status_names_match_the_command_contract() {
        let json = serde_json::to_value(VcamStatus { state: "outdated", listed: true, consumers: 1 }).unwrap();
        assert_eq!(json, serde_json::json!({ "state": "outdated", "listed": true, "consumers": 1 }));
    }
}
