//! Elevation: ask for administrator rights (`runas`), wait for the elevated process, and tell the caller what
//! happened. The launcher is a trait so the callers can be tested with a fake that refuses.

use std::time::Duration;

use windows::Win32::Foundation::{
    CloseHandle, ERROR_CANCELLED, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows::Win32::Security::{
    GetTokenInformation, TokenElevation, TokenUser, TOKEN_ELEVATION, TOKEN_QUERY, TOKEN_USER,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetExitCodeProcess, OpenProcessToken, WaitForSingleObject,
};
use windows::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
use windows_core::PCWSTR;

use crate::util::{pcwstr, wide};

/// How an attempt to run a program elevated ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LaunchOutcome {
    /// The elevated program ran and returned this exit code.
    Exited(u32),
    /// The user declined the UAC prompt (`ERROR_CANCELLED`, 1223).
    Cancelled,
    /// The elevated program did not finish in time.
    TimedOut,
    /// The program could not be started, or its result could not be read.
    Failed(String),
}

/// Starts a program with administrator rights and waits for it.
pub trait Launcher {
    fn run_elevated(&self, exe: &str, args: &[String], timeout: Duration) -> LaunchOutcome;
}

/// The real thing: `ShellExecuteExW` with the verb `runas`.
pub struct ShellLauncher;

impl Launcher for ShellLauncher {
    fn run_elevated(&self, exe: &str, args: &[String], timeout: Duration) -> LaunchOutcome {
        run_elevated(exe, args, timeout)
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

/// Run `exe args...` elevated and wait up to `timeout`.
pub fn run_elevated(exe: &str, args: &[String], timeout: Duration) -> LaunchOutcome {
    let params = args.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ");
    let verb = wide("runas");
    let file = wide(exe);
    let params_w = wide(&params);
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: pcwstr(&verb),
        lpFile: pcwstr(&file),
        lpParameters: pcwstr(&params_w),
        lpDirectory: PCWSTR::null(),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    if let Err(e) = unsafe { ShellExecuteExW(&mut info) } {
        return outcome_for_shell_error(&e);
    }
    let process = info.hProcess;
    if process.is_invalid() {
        return LaunchOutcome::Failed("ShellExecuteExW returned no process handle".into());
    }
    let wait = unsafe {
        WaitForSingleObject(
            process,
            timeout.as_millis().min(u32::MAX as u128 - 1) as u32,
        )
    };
    let result = if wait == WAIT_OBJECT_0 {
        let mut code = 0u32;
        match unsafe { GetExitCodeProcess(process, &mut code) } {
            Ok(()) => LaunchOutcome::Exited(code),
            Err(e) => LaunchOutcome::Failed(format!("GetExitCodeProcess failed: {e}")),
        }
    } else if wait == WAIT_TIMEOUT {
        LaunchOutcome::TimedOut
    } else {
        LaunchOutcome::Failed("WaitForSingleObject failed".into())
    };
    unsafe {
        let _ = CloseHandle(process);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_CANCELLED};

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
}
