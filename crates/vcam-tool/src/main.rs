//! `vcam-tool`: register / unregister the Better Cam virtual camera, show its status, capture-test it, and (P2)
//! act as the frame writer (`feed`), print the transport counters (`stats`) and render the slates (`gen-slates`).
//!
//! `register` and `unregister` re-launch this exe elevated (UAC), wait, print what the elevated child logged and
//! return its exit code. The child is the same binary started with `--elevated`.

mod capture;
mod elevate;
mod feed;
mod gen_slates;
mod install;
mod stats;
mod status;
mod util;

use std::path::PathBuf;
use std::time::Duration;

use util::Out;

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

/// How long the parent waits for the UAC prompt and the elevated work.
const ELEVATION_WAIT: Duration = Duration::from_secs(5 * 60);

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
    let sid = elevate::current_user_sid().unwrap_or_default();
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
    let code = elevate::run_elevated(&exe, &child_args, ELEVATION_WAIT);
    if let Ok(text) = std::fs::read_to_string(&result) {
        print!("{text}");
    }
    let _ = std::fs::remove_file(&result);
    match code {
        Ok(c) => c,
        Err(c) => c,
    }
}

/// Elevated side of `register` / `unregister`.
fn do_elevated(verb: &str, args: &ElevatedArgs) -> i32 {
    let mut out = Out::new(args.result.as_deref());
    out.line(&format!(
        "{verb}: running elevated: {}",
        elevate::is_elevated()
    ));
    let r = if verb == "register" {
        install::register(
            &install::RegisterOptions {
                user_sid: args
                    .user_sid
                    .clone()
                    .or_else(|| elevate::current_user_sid().ok()),
                dshow_class: args.dshow_class,
            },
            &mut out,
        )
    } else {
        install::unregister(&mut out)
    };
    match r {
        Ok(()) => 0,
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
            Ok(a) if a.elevated || elevate::is_elevated() => do_elevated(&cmd, &a),
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
