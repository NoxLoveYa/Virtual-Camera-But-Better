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
mod util;

use std::path::PathBuf;

use util::Out;
use vcam_install::{InstallOutcome, LaunchOutcome, Launcher, ShellLauncher, UninstallOutcome};

/// Exit code when the UAC prompt was declined.
const EXIT_DECLINED: i32 = 3;
/// Exit code when it worked but takes effect only after the camera service or Windows restarts.
const EXIT_PENDING: i32 = 10;

const USAGE: &str = "usage: vcam-tool <command>
  register [--dshow-class]   install DLL + registry + virtual camera (asks for elevation)
  unregister                 remove the virtual camera, registry keys and DLL (asks for elevation)
  status                     is Better Cam listed by Media Foundation and DirectShow?
  capture-test [--frames N] [--format nv12|rgb32] [--size WxH] [--fps N]
               [--expect feed|slate|slate-paused|slate-starting|feed-to-slate]
                             open the camera with a Source Reader and check what it shows
  feed [--size WxH] [--fps N] [--seconds S] [--state live|paused|starting]
                             write the test pattern into frames.bin like the app would
  stats                      print the counters in frames.bin
  gen-slates [--preview DIR] render the slates into crates/vcam-source/assets/slates";

struct ElevatedArgs {
    elevated: bool,
    result: Option<PathBuf>,
    user_sid: Option<String>,
    dshow_class: bool,
}

fn parse_elevated(args: &[String]) -> Result<ElevatedArgs, String> {
    let mut a = ElevatedArgs {
        elevated: false,
        result: None,
        user_sid: None,
        dshow_class: false,
    };
    let mut it = args.iter();
    while let Some(k) = it.next() {
        match k.as_str() {
            "--elevated" => a.elevated = true,
            "--dshow-class" => a.dshow_class = true,
            "--result" => a.result = Some(it.next().ok_or("--result needs a path")?.into()),
            "--user-sid" => a.user_sid = Some(it.next().ok_or("--user-sid needs a value")?.clone()),
            other => return Err(format!("unknown argument '{other}'")),
        }
    }
    Ok(a)
}

/// Parent side: relaunch elevated, wait, print the child's result file.
fn elevate_self(verb: &str, args: &ElevatedArgs) -> i32 {
    let exe = match std::env::current_exe() {
        Ok(p) => p.to_string_lossy().into_owned(),
        Err(e) => {
            eprintln!("current_exe: {e}");
            return 1;
        }
    };
    let result = std::env::temp_dir().join(format!("better-cam-{verb}-{}.log", std::process::id()));
    let _ = std::fs::remove_file(&result);
    let sid = vcam_install::current_user_sid().unwrap_or_default();
    let mut child_args = vec![
        verb.to_string(),
        "--elevated".into(),
        "--result".into(),
        result.to_string_lossy().into_owned(),
    ];
    if !sid.is_empty() {
        child_args.push("--user-sid".into());
        child_args.push(sid);
    }
    if args.dshow_class {
        child_args.push("--dshow-class".into());
    }
    println!("requesting elevation for '{verb}' (UAC prompt)...");
    // waits until the elevated child has exited, however long the prompt stays open
    let outcome = ShellLauncher.run_elevated(&exe, &child_args);
    if let Ok(text) = std::fs::read_to_string(&result) {
        print!("{text}");
    }
    let _ = std::fs::remove_file(&result);
    match outcome {
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
    let mut out = match Out::new(args.result.as_deref()) {
        Ok(o) => o,
        Err(e) => {
            // the console of the elevated child is hidden; the parent prints what is in the result file
            eprintln!("{e}");
            return 1;
        }
    };
    out.line(&format!(
        "{verb}: running elevated: {}",
        vcam_install::is_elevated()
    ));
    let r: Result<i32, String> = if verb == "register" {
        dll_next_to_exe().and_then(|dll| {
            vcam_install::install(
                &vcam_install::InstallOptions {
                    user_sid: args
                        .user_sid
                        .clone()
                        .or_else(|| vcam_install::current_user_sid().ok()),
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
    match r {
        Ok(code) => code,
        Err(e) => {
            out.line(&format!("ERROR: {e}"));
            1
        }
    }
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
        "stats" => stats::run(),
        "gen-slates" => gen_slates::run(rest),
        _ => {
            eprintln!("{USAGE}");
            2
        }
    };
    std::process::exit(code);
}
