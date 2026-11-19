//! `vcam-tool`: register / unregister the Better Cam virtual camera, show its status, capture-test it, and (P2)
//! act as the frame writer (`feed`), print the transport counters (`stats`) and render the slates (`gen-slates`).
//!
//! `register` and `unregister` re-launch this exe elevated (UAC), wait, print what the elevated child logged and
//! return its exit code. The child is the same binary started with `--elevated`.

mod capture;
mod feed;
mod gen_slates;
mod stats;
mod status;
mod storm;
mod util;

use std::path::PathBuf;
use std::time::SystemTime;

use vcam_install::lock::{self, Acquired};
use vcam_install::{
    FileLog, InstallOutcome, LaunchOutcome, Launcher, Log, ShellLauncher, UninstallOutcome,
};

/// Exit code when the UAC prompt was declined.
const EXIT_DECLINED: i32 = 3;
/// Exit code when it worked but takes effect only after the camera service or Windows restarts.
const EXIT_PENDING: i32 = 10;
/// Exit code when another install, repair or removal is running (the same as the app's helper).
const EXIT_BUSY: i32 = 11;

const USAGE: &str = "usage: vcam-tool <command>
  register [--dshow-class] [--user-sid <SID> | --current-user]
                             install DLL + registry + virtual camera (asks for elevation); the account that gets
                             access to the frame file is the one that runs this command
  unregister                 remove the virtual camera, registry keys and DLL (asks for elevation)
  status                     is Better Cam listed by Media Foundation and DirectShow?
  capture-test [--frames N] [--format nv12|rgb32] [--size WxH] [--fps N]
               [--expect feed|slate|slate-paused|slate-starting|feed-to-slate]
                             open the camera with a Source Reader and check what it shows
  feed [--size WxH] [--fps N] [--seconds S] [--state live|paused|starting] [--no-signal] [--file PATH]
                             write the test pattern into frames.bin like the app would; --no-signal makes no frame
                             events (an app from before them); --file writes a scratch file instead
  stats [--file PATH]        print the counters in frames.bin, the writer's frame event advert and the DLL's wake
                             diagnostics
  signal-storm [--seconds N] [--file PATH]
                             development only: set the writer's live frame events in a loop (hostile-signaller check)
  gen-slates [--preview DIR] render the slates into crates/vcam-source/assets/slates";

struct ElevatedArgs {
    elevated: bool,
    user_sid: Option<String>,
    /// Explicitly: the account that runs this process is the user of the camera.
    current_user: bool,
    dshow_class: bool,
}

fn parse_elevated(args: &[String]) -> Result<ElevatedArgs, String> {
    let mut a = ElevatedArgs {
        elevated: false,
        user_sid: None,
        current_user: false,
        dshow_class: false,
    };
    let mut it = args.iter();
    while let Some(k) = it.next() {
        match k.as_str() {
            "--elevated" => a.elevated = true,
            "--dshow-class" => a.dshow_class = true,
            "--current-user" => a.current_user = true,
            "--user-sid" => a.user_sid = Some(it.next().ok_or("--user-sid needs a value")?.clone()),
            // the elevated copy used to write into a result file named by the parent: a path in the user's %TEMP%
            // that the user can rearrange; its report goes to install.log now
            "--result" => {
                return Err("--result is gone: the report is read from install.log".into())
            }
            other => return Err(format!("unknown argument '{other}'")),
        }
    }
    if a.user_sid.is_some() && a.current_user {
        return Err("--user-sid and --current-user cannot be combined".into());
    }
    Ok(a)
}

/// The account that gets access to the frame file. The elevated copy started by `elevate_self` must be told
/// (`--user-sid`): the administrator who answers the prompt may be another account than the user who ran the
/// command, and falling back to "whoever I run as" would give the camera to the wrong one. Run directly in an
/// elevated shell, the current account is the one who typed the command.
fn user_sid_for(args: &ElevatedArgs) -> Result<String, String> {
    if let Some(sid) = &args.user_sid {
        return Ok(sid.clone());
    }
    if args.current_user || !args.elevated {
        return vcam_install::current_user_sid().map_err(|e| {
            format!("cannot tell which account this is ({e}); pass --user-sid <SID>")
        });
    }
    Err("the elevated step needs --user-sid <SID> (or --current-user)".into())
}

/// Parent side: relaunch elevated, wait, print what the child wrote to `install.log`.
fn elevate_self(verb: &str, args: &ElevatedArgs) -> i32 {
    let exe = match std::env::current_exe() {
        Ok(p) => p.to_string_lossy().into_owned(),
        Err(e) => {
            eprintln!("current_exe: {e}");
            return 1;
        }
    };
    // never omitted: the administrator who answers the prompt may be another account
    let sid = match &args.user_sid {
        Some(s) => s.clone(),
        None => vcam_install::current_user_sid().unwrap_or_default(),
    };
    if sid.is_empty() {
        eprintln!(
            "cannot tell which Windows account this is, so nothing was started; pass --user-sid <SID>"
        );
        return 1;
    }
    let mut child_args = vec![
        verb.to_string(),
        "--elevated".into(),
        "--user-sid".into(),
        sid,
    ];
    if args.dshow_class {
        child_args.push("--dshow-class".into());
    }
    println!("requesting elevation for '{verb}' (UAC prompt)...");
    let started = SystemTime::now();
    // waits until the elevated child has exited, however long the prompt stays open
    let outcome = ShellLauncher.run_elevated(&exe, &child_args);
    let exited = |c: &u32| *c as i32;
    if !matches!(&outcome, LaunchOutcome::Exited(c) if exited(c) == EXIT_BUSY) {
        for line in util::run_output_here(verb, started) {
            println!("{line}");
        }
    }
    match outcome {
        LaunchOutcome::Exited(c) if exited(&c) == EXIT_BUSY => {
            eprintln!("{}", lock::BUSY_MESSAGE);
            EXIT_BUSY
        }
        LaunchOutcome::Exited(c) => c as i32,
        LaunchOutcome::Cancelled => {
            eprintln!("elevation declined (UAC prompt cancelled)");
            EXIT_DECLINED
        }
        LaunchOutcome::Failed(why) => {
            eprintln!("{why}");
            1
        }
    }
}

/// The DLL the tool registers: `better_cam_vcam.dll` next to this exe.
fn dll_next_to_exe() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let p = exe
        .parent()
        .ok_or("exe has no parent")?
        .join(format!("{}.dll", vcam_protocol::DLL_BASE_NAME));
    if p.is_file() {
        Ok(p)
    } else {
        Err(format!("DLL not found next to the tool: {}", p.display()))
    }
}

/// Elevated side of `register` / `unregister`.
fn do_elevated(verb: &str, args: &ElevatedArgs) -> i32 {
    // one install, repair or removal at a time on the machine: before anything is touched
    let _lock = match lock::acquire() {
        Ok(Acquired::Held(l)) => l,
        Ok(Acquired::Busy) => {
            eprintln!("{}", lock::BUSY_MESSAGE);
            return EXIT_BUSY;
        }
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let sid = match user_sid_for(args) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    // the report goes to install.log (and the console); the parent reads it from there when it is done
    let mut out = FileLog::open_for(Some(&sid));
    out.line(&format!(
        "{} (running elevated: {}, user {sid})",
        util::marker(verb),
        vcam_install::is_elevated()
    ));
    let r: Result<i32, String> = if verb == "register" {
        dll_next_to_exe().and_then(|dll| {
            vcam_install::install(
                &vcam_install::InstallOptions {
                    user_sid: Some(sid.clone()),
                    dshow_class: args.dshow_class,
                },
                &dll,
                &mut out,
            )
            .map(|o| match o {
                InstallOutcome::Done => 0,
                InstallOutcome::PendingServiceRestart => EXIT_PENDING,
            })
        })
    } else {
        vcam_install::uninstall(&mut out).map(|o| match o {
            UninstallOutcome::Done => 0,
            UninstallOutcome::PendingReboot => EXIT_PENDING,
        })
    };
    let code = match r {
        Ok(code) => code,
        Err(e) => {
            out.line(&format!("ERROR: {e}"));
            1
        }
    };
    // lines that still wait for a file go in before the process ends
    out.flush();
    code
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first().cloned() else {
        eprintln!("{USAGE}");
        std::process::exit(2);
    };
    let rest = &args[1..];
    let code = match cmd.as_str() {
        "register" | "unregister" => match parse_elevated(rest) {
            Err(e) => {
                eprintln!("{e}\n{USAGE}");
                2
            }
            Ok(a) if a.elevated || vcam_install::is_elevated() => do_elevated(&cmd, &a),
            Ok(a) => elevate_self(&cmd, &a),
        },
        "status" => status::run(),
        "capture-test" => capture::run(rest),
        "feed" => feed::run(rest),
        "stats" => stats::run(rest),
        "signal-storm" => storm::run(rest),
        "gen-slates" => gen_slates::run(rest),
        _ => {
            eprintln!("{USAGE}");
            2
        }
    };
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    const SID: &str = "S-1-5-21-1004336348-1177238915-682003330-1001";

    /// The result file named by the unelevated parent is gone: nothing a user can rearrange is written by the
    /// elevated child.
    #[test]
    fn the_elevated_copy_takes_no_result_path() {
        assert!(parse_elevated(&args(&[
            "--elevated",
            "--result",
            r"C:\Users\x\AppData\Local\Temp\r.log"
        ]))
        .is_err());
        assert!(parse_elevated(&args(&["--result"])).is_err());
        let a = parse_elevated(&args(&["--elevated", "--user-sid", SID])).unwrap();
        assert!(a.elevated && a.user_sid.as_deref() == Some(SID));
    }

    #[test]
    fn the_elevated_step_must_be_told_whose_camera_it_is() {
        // started by the parent: the SID is required, no fallback to the administrator who answered the prompt
        let a = parse_elevated(&args(&["--elevated"])).unwrap();
        let e = user_sid_for(&a).unwrap_err();
        assert!(e.contains("--user-sid"), "{e}");
        assert_eq!(
            user_sid_for(&parse_elevated(&args(&["--elevated", "--user-sid", SID])).unwrap())
                .unwrap(),
            SID
        );
        // an explicit choice, or a command typed in an elevated shell, is the current account
        let me = vcam_install::current_user_sid().unwrap();
        assert_eq!(
            user_sid_for(&parse_elevated(&args(&["--elevated", "--current-user"])).unwrap())
                .unwrap(),
            me
        );
        assert_eq!(
            user_sid_for(&parse_elevated(&args(&["--dshow-class"])).unwrap()).unwrap(),
            me
        );
        assert!(parse_elevated(&args(&["--user-sid", SID, "--current-user"])).is_err());
    }
}
