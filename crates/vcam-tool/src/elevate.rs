//! Self-elevation: run this same exe with the `runas` verb, wait, return the child's exit code.

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

/// Exit code when the UAC prompt was declined.
pub const EXIT_DECLINED: i32 = 3;
/// Exit code when the UAC prompt was not answered in time.
pub const EXIT_TIMEOUT: i32 = 4;

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
    let user = unsafe { &*(buf.as_ptr() as *const TOKEN_USER) };
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

fn quote(arg: &str) -> String {
    if arg.contains(' ') || arg.is_empty() {
        format!("\"{arg}\"")
    } else {
        arg.to_string()
    }
}

/// Run `exe args...` elevated and wait up to `timeout`. Returns the child's exit code.
pub fn run_elevated(exe: &str, args: &[String], timeout: Duration) -> Result<i32, i32> {
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
        if e.code() == ERROR_CANCELLED.to_hresult() {
            eprintln!("elevation declined (UAC prompt cancelled)");
            return Err(EXIT_DECLINED);
        }
        eprintln!("ShellExecuteExW failed: {e}");
        return Err(1);
    }
    let process = info.hProcess;
    if process.is_invalid() {
        eprintln!("ShellExecuteExW returned no process handle");
        return Err(1);
    }
    let wait = unsafe { WaitForSingleObject(process, timeout.as_millis() as u32) };
    let result = if wait == WAIT_OBJECT_0 {
        let mut code = 0u32;
        match unsafe { GetExitCodeProcess(process, &mut code) } {
            Ok(()) => Ok(code as i32),
            Err(e) => {
                eprintln!("GetExitCodeProcess failed: {e}");
                Err(1)
            }
        }
    } else if wait == WAIT_TIMEOUT {
        eprintln!("elevated process did not finish within {timeout:?} (UAC prompt unanswered?)");
        Err(EXIT_TIMEOUT)
    } else {
        eprintln!("WaitForSingleObject failed");
        Err(1)
    };
    unsafe {
        let _ = CloseHandle(process);
    }
    result
}
