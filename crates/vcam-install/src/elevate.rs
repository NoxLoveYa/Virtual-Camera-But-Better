//! Elevation: ask for administrator rights (`runas`), wait for the elevated process, and tell the caller what
//! happened. The launcher is a trait so the callers can be tested with a fake that refuses.

use crate::util::{pcwstr, wide};
use windows::Win32::Foundation::{CloseHandle, ERROR_CANCELLED, HANDLE, WAIT_EVENT, WAIT_OBJECT_0};
use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows::Win32::Security::{
    GetTokenInformation, TokenElevation, TokenUser, TOKEN_ELEVATION, TOKEN_QUERY, TOKEN_USER,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetExitCodeProcess, OpenProcessToken, WaitForSingleObject,
};
use windows::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;

/// How an attempt to run a program elevated ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LaunchOutcome {
    /// The elevated program ran and returned this exit code.
    Exited(u32),
    /// The user declined the UAC prompt (`ERROR_CANCELLED`, 1223).
    Cancelled,
    /// The program could not be started, or its result could not be read.
    Failed(String),
}

/// Starts a program with administrator rights and waits until it has exited.
///
/// There is no timeout, on purpose: a timeout while the prompt is open (or while the helper runs) would hand control
/// back to a caller that then believes nothing is running and may start a second helper. The call returns when the
/// prompt was declined, the program could not be started, or the program has really ended.
pub trait Launcher {
    fn run_elevated(&self, exe: &str, args: &[String]) -> LaunchOutcome;
}

/// The real thing: `ShellExecuteExW` with the verb `runas`.
pub struct ShellLauncher;

impl Launcher for ShellLauncher {
    fn run_elevated(&self, exe: &str, args: &[String]) -> LaunchOutcome {
        run_elevated(exe, args)
    }
}

fn token() -> windows_core::Result<HANDLE> {
    let mut t = HANDLE::default();
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut t)? };
    Ok(t)
}

pub fn is_elevated() -> bool {
    let Ok(t) = token() else { return false };
    let mut e = TOKEN_ELEVATION::default();
    let mut len = 0u32;
    let ok = unsafe {
        GetTokenInformation(
            t,
            TokenElevation,
            Some(&mut e as *mut _ as *mut _),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        )
    };
    unsafe {
        let _ = CloseHandle(t);
    }
    ok.is_ok() && e.TokenIsElevated != 0
}

/// SID string (`S-1-5-21-...`) of the user who owns this process.
pub fn current_user_sid() -> windows_core::Result<String> {
    let t = token()?;
    let mut len = 0u32;
    let _ = unsafe { GetTokenInformation(t, TokenUser, None, 0, &mut len) };
    let mut buf = vec![0u8; len as usize];
    let r = unsafe {
        GetTokenInformation(
            t,
            TokenUser,
            Some(buf.as_mut_ptr() as *mut _),
            len,
            &mut len,
        )
    };
    unsafe {
        let _ = CloseHandle(t);
    }
    r?;
    // SAFETY: the buffer was filled by GetTokenInformation(TokenUser); it may be unaligned for TOKEN_USER.
    let user = unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const TOKEN_USER) };
    let mut s = windows_core::PWSTR::null();
    unsafe { ConvertSidToStringSidW(user.User.Sid, &mut s)? };
    let out = unsafe { s.to_string() }.unwrap_or_default();
    unsafe {
        windows::Win32::Foundation::LocalFree(Some(windows::Win32::Foundation::HLOCAL(
            s.0 as *mut _,
        )));
    }
    Ok(out)
}

/// Quote one argument for a command line (the arguments here never contain a double quote).
fn quote(arg: &str) -> String {
    if arg.contains(' ') || arg.is_empty() {
        format!("\"{arg}\"")
    } else {
        arg.to_string()
    }
}

/// What a failed `ShellExecuteExW` with the `runas` verb means: `ERROR_CANCELLED` (1223) is the user declining the
/// UAC prompt, anything else is a failure to start.
pub fn outcome_for_shell_error(e: &windows_core::Error) -> LaunchOutcome {
    if e.code() == ERROR_CANCELLED.to_hresult() {
        LaunchOutcome::Cancelled
    } else {
        LaunchOutcome::Failed(format!("ShellExecuteExW failed: {e}"))
    }
}

/// What `ShellExecuteExW` is asked for: the strings it points to live here.
struct Request {
    verb: Vec<u16>,
    file: Vec<u16>,
    params: Vec<u16>,
    /// The working directory of the elevated process: the system folder, never whatever folder the caller happens to
    /// be in (a folder the user can write to would put their files next to the helper's current directory).
    dir: Vec<u16>,
}

impl Request {
    fn new(exe: &str, args: &[String]) -> Request {
        let params = args.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ");
        Request {
            verb: wide("runas"),
            file: wide(exe),
            params: wide(&params),
            dir: wide(&crate::util::system_dir().to_string_lossy()),
        }
    }

    fn info(&self) -> SHELLEXECUTEINFOW {
        SHELLEXECUTEINFOW {
            cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_NOCLOSEPROCESS,
            lpVerb: pcwstr(&self.verb),
            lpFile: pcwstr(&self.file),
            lpParameters: pcwstr(&self.params),
            lpDirectory: pcwstr(&self.dir),
            nShow: SW_HIDE.0,
            ..Default::default()
        }
    }
}

/// `WaitForSingleObject` time-out value that means "never give up".
const WAIT_FOREVER: u32 = u32::MAX;

fn wait_object(process: HANDLE, ms: u32) -> WAIT_EVENT {
    unsafe { WaitForSingleObject(process, ms) }
}

/// Wait until `process` has exited and turn that into a [`LaunchOutcome`]. This is the only wait of
/// [`run_elevated`], and it has no time-out: it asks for [`WAIT_FOREVER`], and anything but "the process ended"
/// is a failure, never a reason to hand control back as if the helper were gone. `wait` is `WaitForSingleObject`
/// (the tests put a recording one in its place).
fn wait_for_exit(process: HANDLE, wait: fn(HANDLE, u32) -> WAIT_EVENT) -> LaunchOutcome {
    if wait(process, WAIT_FOREVER) != WAIT_OBJECT_0 {
        return LaunchOutcome::Failed("WaitForSingleObject failed".into());
    }
    let mut code = 0u32;
    match unsafe { GetExitCodeProcess(process, &mut code) } {
        Ok(()) => LaunchOutcome::Exited(code),
        Err(e) => LaunchOutcome::Failed(format!("GetExitCodeProcess failed: {e}")),
    }
}

/// Run `exe args...` elevated and wait until it has exited (see [`Launcher`]).
pub fn run_elevated(exe: &str, args: &[String]) -> LaunchOutcome {
    let request = Request::new(exe, args);
    let mut info = request.info();
    if let Err(e) = unsafe { ShellExecuteExW(&mut info) } {
        return outcome_for_shell_error(&e);
    }
    let process = info.hProcess;
    if process.is_invalid() {
        return LaunchOutcome::Failed("ShellExecuteExW returned no process handle".into());
    }
    // the process handle is kept until the helper has really exited
    let result = wait_for_exit(process, wait_object);
    unsafe {
        let _ = CloseHandle(process);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_CANCELLED};
    use windows_core::PCWSTR;

    #[test]
    fn a_declined_uac_prompt_is_a_cancel_and_nothing_else_is() {
        assert_eq!(ERROR_CANCELLED.0, 1223);
        let declined = windows_core::Error::from(ERROR_CANCELLED.to_hresult());
        assert_eq!(outcome_for_shell_error(&declined), LaunchOutcome::Cancelled);
        let other = windows_core::Error::from(ERROR_ACCESS_DENIED.to_hresult());
        assert!(matches!(
            outcome_for_shell_error(&other),
            LaunchOutcome::Failed(_)
        ));
    }

    #[test]
    fn arguments_with_spaces_are_quoted() {
        assert_eq!(quote("a"), "a");
        assert_eq!(quote("a b"), "\"a b\"");
        assert_eq!(quote(""), "\"\"");
    }

    /// The elevated process starts in the system folder, and the rest of the request is what was asked for.
    #[test]
    fn the_elevated_process_starts_in_the_system_folder() {
        let r = Request::new(
            r"C:\app\better-cam.exe",
            &["--vcam-install".into(), "a b".into()],
        );
        let info = r.info();
        let text = |p: PCWSTR| unsafe { p.to_string() }.unwrap();
        assert_eq!(text(info.lpVerb), "runas");
        assert_eq!(text(info.lpFile), r"C:\app\better-cam.exe");
        assert_eq!(text(info.lpParameters), "--vcam-install \"a b\"");
        let dir = std::path::PathBuf::from(text(info.lpDirectory));
        assert!(dir.is_dir(), "{}", dir.display());
        assert!(
            dir.ends_with("System32") || dir.ends_with("system32"),
            "{}",
            dir.display()
        );
        assert_eq!(dir, crate::util::system_dir());
        assert!(!info.lpDirectory.is_null());
    }

    fn child(script: &str) -> std::process::Child {
        std::process::Command::new("cmd")
            .args(["/c", script])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap()
    }

    fn handle_of(c: &std::process::Child) -> HANDLE {
        use std::os::windows::io::AsRawHandle;
        HANDLE(c.as_raw_handle())
    }

    /// The real wait, on a real process (the elevation itself needs a person at a prompt; the wait does not): it
    /// returns when the process has exited, with its exit code, and not before. A timeout put back into the wait
    /// would end it early.
    #[test]
    fn the_wait_returns_only_when_the_process_has_exited() {
        let c = child("ping -n 4 127.0.0.1 >nul & exit 7");
        let began = std::time::Instant::now();
        let outcome = wait_for_exit(handle_of(&c), wait_object);
        assert_eq!(outcome, LaunchOutcome::Exited(7));
        assert!(
            began.elapsed() >= std::time::Duration::from_millis(2500),
            "the wait ended after {:?}, before the process did",
            began.elapsed()
        );
        let mut c = c;
        assert!(c.try_wait().unwrap().is_some());
    }

    /// There is no timeout path: the wait asks for "forever", and an answer other than "the process ended" is a
    /// failure that is reported, not a state to carry on from.
    #[test]
    fn the_wait_has_no_timeout() {
        use std::sync::atomic::{AtomicU32, Ordering};
        static ASKED: AtomicU32 = AtomicU32::new(0);
        fn ended(_: HANDLE, ms: u32) -> WAIT_EVENT {
            ASKED.store(ms, Ordering::SeqCst);
            WAIT_OBJECT_0
        }
        fn timed_out(_: HANDLE, ms: u32) -> WAIT_EVENT {
            ASKED.store(ms, Ordering::SeqCst);
            windows::Win32::Foundation::WAIT_TIMEOUT
        }
        let mut c = child("exit 3");
        c.wait().unwrap();
        assert_eq!(
            wait_for_exit(handle_of(&c), ended),
            LaunchOutcome::Exited(3)
        );
        assert_eq!(
            ASKED.load(Ordering::SeqCst),
            u32::MAX,
            "the wait has a time limit"
        );
        // were it ever to answer "time is up", that is a failure, never an exit code
        ASKED.store(0, Ordering::SeqCst);
        assert!(matches!(
            wait_for_exit(handle_of(&c), timed_out),
            LaunchOutcome::Failed(_)
        ));
        assert_eq!(ASKED.load(Ordering::SeqCst), u32::MAX);
        assert_eq!(WAIT_FOREVER, u32::MAX);
    }
}
