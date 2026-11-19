//! `feed`: act as the app's writer. Publishes the checkerboard + moving stripe pattern into `frames.bin` so the
//! virtual camera shows it, following the size and format the DLL negotiated (the DLL never scales).
//!
//! `feed --size WxH --fps N --seconds S [--state live|paused|starting] [--no-signal] [--file PATH]`. `paused` and
//! `starting` publish no frames; they only hold that state (with heartbeats) so the DLL shows the matching slate.
//! `--no-signal` makes no frame events, like an app from before the frame event (the DLL then polls). `--file`
//! writes a scratch frame file instead of `%ProgramData%\BetterCam\frames.bin` (for tests; the camera does not read it).
//! It takes only a file that is not there, is empty or is already a frame file, and never one in the folder of the real
//! frame file, however the path is spelled (`vcam_protocol::WriterOptions::scratch`).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use vcam_protocol::pattern;
use vcam_protocol::{
    advertised_for_output, qpc_frequency, qpc_now, Format, SlotFormat, Writer, WriterOptions,
    WriterState, SUBTYPE_NV12, SUBTYPE_RGB32,
};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
#[cfg(not(test))]
use windows::Win32::System::Console::SetConsoleCtrlHandler;
use windows::Win32::System::Threading::{
    CreateWaitableTimerExW, SetWaitableTimer, WaitForSingleObject,
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS,
};

pub const USAGE: &str = "usage: vcam-tool feed [--size WxH] [--fps N] [--seconds S] [--state live|paused|starting] [--no-signal] [--file PATH]";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Live,
    Paused,
    Starting,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Args {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub seconds: f64,
    pub state: State,
    /// Make and signal the frame events (off with `--no-signal`).
    pub signal: bool,
    /// A scratch frame file instead of the real one.
    pub file: Option<PathBuf>,
}

pub fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut a = Args {
        width: 1920,
        height: 1080,
        fps: 30,
        seconds: 10.0,
        state: State::Live,
        signal: true,
        file: None,
    };
    let mut it = args.iter();
    while let Some(k) = it.next() {
        let mut val = || it.next().ok_or_else(|| format!("{k} needs a value"));
        match k.as_str() {
            "--size" => {
                let v = val()?.to_lowercase();
                let (w, h) = v.split_once('x').ok_or("--size expects WxH")?;
                a.width = w.parse().map_err(|_| "bad width")?;
                a.height = h.parse().map_err(|_| "bad height")?;
            }
            "--fps" => a.fps = val()?.parse().map_err(|_| "bad --fps")?,
            "--seconds" => a.seconds = val()?.parse().map_err(|_| "bad --seconds")?,
            "--state" => {
                a.state = match val()?.to_lowercase().as_str() {
                    "live" => State::Live,
                    "paused" => State::Paused,
                    "starting" => State::Starting,
                    o => return Err(format!("unknown state '{o}' (live|paused|starting)")),
                }
            }
            "--no-signal" => a.signal = false,
            "--file" => a.file = Some(PathBuf::from(val()?)),
            other => return Err(format!("unknown argument '{other}'")),
        }
    }
    vcam_protocol::validate_format(a.width, a.height, SUBTYPE_NV12)
        .map_err(|e| format!("--size: {e:?}"))?;
    if !(1..=240).contains(&a.fps) {
        return Err("--fps must be 1..=240".into());
    }
    if a.seconds <= 0.0 {
        return Err("--seconds must be positive".into());
    }
    Ok(a)
}

static CTRL_C: AtomicBool = AtomicBool::new(false);

#[cfg(not(test))]
unsafe extern "system" fn ctrl_handler(_kind: u32) -> windows_core::BOOL {
    CTRL_C.store(true, Ordering::SeqCst);
    windows_core::BOOL(1)
}

/// A high-resolution timer for sleeping to a QPC deadline without burning a core.
pub struct Sleeper(Option<HANDLE>);

impl Sleeper {
    pub fn new() -> Sleeper {
        let h = unsafe {
            CreateWaitableTimerExW(
                None,
                windows_core::PCWSTR::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS.0,
            )
        };
        Sleeper(h.ok())
    }

    /// Sleep until QPC `due` (the last 0.3 ms are spun).
    pub fn sleep_until(&self, due: u64) {
        let freq = qpc_frequency();
        loop {
            let now = qpc_now();
            if now >= due {
                return;
            }
            let left_us = (due - now) as u128 * 1_000_000 / freq as u128;
            if left_us > 600 {
                let wait_us = left_us - 300;
                match self.0 {
                    Some(h) => {
                        let rel: i64 = -((wait_us * 10) as i64);
                        if unsafe { SetWaitableTimer(h, &rel, 0, None, None, false) }.is_ok() {
                            unsafe { WaitForSingleObject(h, 100) };
                        } else {
                            std::thread::sleep(Duration::from_micros(wait_us as u64));
                        }
                    }
                    None => std::thread::sleep(Duration::from_micros(wait_us as u64)),
                }
            } else {
                std::hint::spin_loop();
            }
        }
    }
}

impl Default for Sleeper {
    fn default() -> Self {
        Sleeper::new()
    }
}

impl Drop for Sleeper {
    fn drop(&mut self) {
        if let Some(h) = self.0.take() {
            let _ = unsafe { CloseHandle(h) };
        }
    }
}

fn subtype_name(s: u32) -> &'static str {
    match s {
        SUBTYPE_NV12 => "nv12",
        SUBTYPE_RGB32 => "rgb32",
        _ => "?",
    }
}

/// What to print when the frame file could not be made. The hint is about the installer's folder, so it is given only
/// for the default path: for a scratch file somebody typed (`scratch`) it would send them to the wrong place.
fn create_failure(path: &std::path::Path, e: &std::io::Error, scratch: bool) -> String {
    if scratch {
        format!("cannot create {}: {e}", path.display())
    } else {
        format!(
            "cannot create {}: {e}\n(run 'vcam-tool register' once so the folder has the right ACL)",
            path.display()
        )
    }
}

pub fn run(args: &[String]) -> i32 {
    let a = match parse_args(args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}\n{USAGE}");
            return 2;
        }
    };
    let path = a.file.clone().unwrap_or_else(vcam_protocol::frames_path);
    // a typed path is a scratch file: it must not take over an existing file that is not a frame file
    let opts = WriterOptions {
        signal: a.signal,
        scratch: a.file.is_some(),
    };
    let writer = match Writer::create_with(&path, &opts) {
        Ok(w) => w,
        Err(e) if vcam_protocol::secure::is_refused(&e) => {
            eprintln!("refusing {}: {e}", path.display());
            return 1;
        }
        Err(e) => {
            eprintln!("{}", create_failure(&path, &e, a.file.is_some()));
            return 1;
        }
    };
    // a test that runs the feed in its process must not leave a process-wide Ctrl-C handler behind
    #[cfg(not(test))]
    unsafe {
        let _ = SetConsoleCtrlHandler(Some(ctrl_handler), true);
    }
    let advertised = advertised_for_output(a.width, a.height, a.fps);
    writer.set_advertised(&advertised);
    println!(
        "feeding {}: {}x{} @{} fps for {} s, state {:?}; advertised {} types; frame event {:?}",
        path.display(),
        a.width,
        a.height,
        a.fps,
        a.seconds,
        a.state,
        advertised.len(),
        writer.frame_event_status()
    );

    let began = Instant::now();
    let limit = Duration::from_secs_f64(a.seconds);
    let sleeper = Sleeper::new();
    let freq = qpc_frequency();
    let mut frames = 0u64;
    let mut late = 0u64;
    let mut last_report = Instant::now();
    let mut last_fmt = Format::default();

    match a.state {
        State::Paused | State::Starting => {
            writer.set_state(if a.state == State::Paused {
                WriterState::Paused
            } else {
                WriterState::Starting
            });
            while began.elapsed() < limit && !CTRL_C.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(200));
                writer.heartbeat();
            }
        }
        State::Live => {
            writer.set_state(WriterState::Live);
            let start = qpc_now();
            let mut n = 0u64;
            while began.elapsed() < limit && !CTRL_C.load(Ordering::SeqCst) {
                let mut due = start + (n as u128 * freq as u128 / a.fps as u128) as u64;
                sleeper.sleep_until(due);
                let now = qpc_now();
                if now > due + freq / a.fps as u64 * 4 {
                    // fell far behind (suspend, debugger): restart the schedule instead of bursting
                    late += 1;
                    due = now;
                }
                let f = writer.negotiated().unwrap_or(Format::new(
                    a.width,
                    a.height,
                    a.fps,
                    SUBTYPE_NV12,
                ));
                if f != last_fmt {
                    println!(
                        "format: {} {}x{} (consumer {})",
                        subtype_name(f.subtype),
                        f.width,
                        f.height,
                        if writer.consumer_active() {
                            "attached"
                        } else {
                            "absent"
                        }
                    );
                    last_fmt = f;
                }
                let stride = if f.subtype == SUBTYPE_RGB32 {
                    f.width * 4
                } else {
                    f.width
                };
                let slot = SlotFormat {
                    width: f.width,
                    height: f.height,
                    stride,
                    subtype: f.subtype,
                };
                let r = writer.publish(slot, due, |d| {
                    if f.subtype == SUBTYPE_RGB32 {
                        pattern::fill_bgrx(n, f.width, f.height, d, stride as usize)
                    } else {
                        pattern::fill_nv12(n, f.width, f.height, d, stride as usize)
                    }
                });
                if let Err(e) = r {
                    eprintln!("publish failed: {e:?}");
                    // the file must not be left on its last picture (a writer's drop does not close it)
                    writer.close();
                    return 1;
                }
                n += 1;
                frames = n;
                if last_report.elapsed() >= Duration::from_secs(5) {
                    last_report = Instant::now();
                    println!(
                        "{:.0} s: {} frames ({:.1} fps), consumer {}",
                        began.elapsed().as_secs_f64(),
                        frames,
                        frames as f64 / began.elapsed().as_secs_f64(),
                        if writer.consumer_active() {
                            "attached"
                        } else {
                            "absent"
                        }
                    );
                }
            }
        }
    }
    let el = began.elapsed().as_secs_f64();
    println!(
        "done: {frames} frames in {el:.2} s ({:.2} fps), {late} schedule resets; closing the file",
        frames as f64 / el.max(0.001)
    );
    // (a writer's drop no longer closes the file)
    writer.close();
    drop(writer);
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::ScratchDir;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn parses_the_documented_options() {
        let a = parse_args(&s(&[
            "--size",
            "1280x720",
            "--fps",
            "60",
            "--seconds",
            "2.5",
            "--state",
            "paused",
        ]))
        .unwrap();
        assert_eq!(
            (a.width, a.height, a.fps, a.seconds, a.state),
            (1280, 720, 60, 2.5, State::Paused)
        );
        assert_eq!(parse_args(&[]).unwrap().width, 1920);
    }

    #[test]
    fn no_signal_and_the_scratch_file_are_options_and_signalling_is_the_default() {
        let a = parse_args(&[]).unwrap();
        assert!(a.signal && a.file.is_none());
        let a = parse_args(&s(&["--no-signal", "--file", r"C:\x\scratch.bin"])).unwrap();
        assert!(!a.signal);
        assert_eq!(a.file, Some(PathBuf::from(r"C:\x\scratch.bin")));
        assert!(parse_args(&s(&["--file"])).is_err());
        assert!(USAGE.contains("--no-signal"));
    }

    /// The header of the file a finished `run` left (the writer is closed by then, so the advert is withdrawn; the nonces
    /// and the count of signals stay).
    fn left_behind(path: &std::path::Path) -> vcam_protocol::RawHeader {
        vcam_protocol::MappedFile::open(path, vcam_protocol::Access::ReadOnly)
            .unwrap()
            .shared()
            .header()
    }

    /// `--no-signal` is the old app: through `run`, the writer it makes has no events: no nonce in the header, and no
    /// signal for the state change and the close, which a writer with events makes.
    #[test]
    fn run_with_no_signal_makes_no_events_and_without_it_makes_them() {
        let dir = ScratchDir::new("feed-signal");
        for (flags, signal) in [(&[][..], true), (&["--no-signal"][..], false)] {
            let path = dir.join(&format!("signal-{signal}.bin"));
            let mut args = s(&["--state", "paused", "--seconds", "0.2", "--file"]);
            args.push(path.display().to_string());
            args.extend(s(flags));
            assert_eq!(run(&args), 0);
            let h = left_behind(&path);
            assert_eq!(h.writer_state, WriterState::Closed as u32);
            assert_eq!(h.event_nonce_lo != 0, signal, "nonce in the header");
            assert_eq!(h.signal_seq > 0, signal, "signals made: {}", h.signal_seq);
        }
    }

    /// `--file` takes a scratch file only: a file that exists and is not a frame file is refused and left alone.
    #[test]
    fn run_refuses_a_file_that_is_not_a_frame_file_and_leaves_it_alone() {
        let dir = ScratchDir::new("feed-refuse");
        let notes = dir.join("notes.txt");
        std::fs::write(&notes, b"my notes").unwrap();
        let args = s(&["--seconds", "0.2", "--file", &notes.display().to_string()]);
        assert_eq!(run(&args), 1);
        assert_eq!(std::fs::read(&notes).unwrap(), b"my notes");
        // a frame file of an earlier run is reused
        let fresh = dir.join("fresh.bin");
        let args = s(&[
            "--state",
            "paused",
            "--seconds",
            "0.2",
            "--file",
            &fresh.display().to_string(),
        ]);
        assert_eq!(run(&args), 0);
        assert_eq!(run(&args), 0);
        assert_eq!(
            std::fs::metadata(&fresh).unwrap().len(),
            vcam_protocol::FILE_SIZE
        );
    }

    /// The "run vcam-tool register" hint belongs to the installer's folder only.
    #[test]
    fn the_register_hint_is_for_the_default_path_only() {
        // a path below a file can never be made, on any machine (a drive letter that is "free" here is a drive there)
        let dir = ScratchDir::new("feed-hint");
        let blocker = dir.join("not-a-folder");
        std::fs::write(&blocker, b"x").unwrap();
        let path = blocker.join("frames.bin");
        let shown = path.display().to_string();
        let e = std::io::Error::from_raw_os_error(3);
        let p = path.as_path();
        assert!(create_failure(p, &e, false).contains("vcam-tool register"));
        let scratch = create_failure(p, &e, true);
        assert!(!scratch.contains("register"), "{scratch}");
        assert!(scratch.contains(&shown), "{scratch}");
        // through `run`: a scratch path that cannot be made reports without the hint and fails
        let args = s(&["--seconds", "0.2", "--file", &shown]);
        assert_eq!(run(&args), 1);
        assert_eq!(std::fs::read(&blocker).unwrap(), b"x");
    }

    #[test]
    fn rejects_bad_options() {
        for bad in [
            &["--size", "1281x720"][..],
            &["--size", "5000x720"],
            &["--size", "wide"],
            &["--fps", "0"],
            &["--fps", "1000"],
            &["--seconds", "-1"],
            &["--state", "dead"],
            &["--bogus"],
            &["--size"],
        ] {
            assert!(parse_args(&s(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn sleeper_wakes_close_to_the_deadline() {
        let sl = Sleeper::new();
        let due = qpc_now() + qpc_frequency() / 50; // 20 ms
        sl.sleep_until(due);
        let over = qpc_now().saturating_sub(due) as f64 / qpc_frequency() as f64 * 1000.0;
        assert!(over < 5.0, "woke {over} ms late");
    }
}
