//! What install and uninstall do to the machine outside the data folder: the registry, the virtual camera object, the
//! camera service, files that wait for a reboot. A trait, so the order of the steps and the rollback are tested with
//! a fake that fails on demand; [`RealMachine`] is the Windows implementation.

use std::path::Path;
use std::time::Duration;

pub use crate::registry::Stored;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_NOT_FOUND, ERROR_PATH_NOT_FOUND,
};
use windows::Win32::Security::{
    AdjustTokenPrivileges, LookupPrivilegeValueW, LUID_AND_ATTRIBUTES, SE_PRIVILEGE_ENABLED,
    TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
};
use windows::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_DELAY_UNTIL_REBOOT};
use windows::Win32::System::Services::{
    CloseServiceHandle, ControlService, OpenSCManagerW, OpenServiceW, QueryServiceStatus,
    SC_MANAGER_CONNECT, SERVICE_CONTROL_STOP, SERVICE_QUERY_STATUS, SERVICE_STATUS, SERVICE_STOP,
    SERVICE_STOPPED,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_core::PCWSTR;

use crate::camera::{add_dshow_class, create_virtual_camera};
use crate::registry;
use crate::util::{init_com_and_mf, pcwstr, werr, wide, Res};

/// What `HKLM` records about the installed DLL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Registration {
    /// Full path of the DLL (`InprocServer32` default value and `DllPath`).
    pub dll: String,
    /// The build tag in the DLL's name.
    pub build: String,
    pub protocol: u32,
}

/// Where the record of moved-aside data folders is kept: two calls, so that everything that is decided about the
/// record ([`crate::aside`]) is tested against a plain fake, and the real implementation stays a thin layer over the
/// registry.
pub trait AsideStore {
    /// The record of the data folders this helper moved aside in `%ProgramData%`, as the strings stored (see
    /// [`crate::aside`] for what is in them). Kept where only administrators can write (the helper's HKLM key), so
    /// the sweep acts on exactly these and not on whatever a user made under a similar name. A record that is not
    /// there is [`Stored::Missing`]; one that is there and cannot be read is [`Stored::Unreadable`], and is never
    /// overwritten.
    fn aside_record(&mut self) -> Stored;
    /// Replace the record by `strings`. An empty list removes the value (and the state key when it holds nothing
    /// else and no registration is there). The caller has read the record first and has not got
    /// [`Stored::Unreadable`] for it.
    fn write_aside_record(&mut self, strings: &[String]) -> Res<()>;
}

/// The parts of the machine that install and uninstall change besides the data folder and the DLL copies.
pub trait Machine: AsideStore {
    /// Prepare the process: privileges, COM, Media Foundation. Called once, before anything is changed.
    fn start(&mut self) -> Res<()>;
    /// `sid` names a user account on this computer.
    fn confirm_user(&mut self, sid: &str) -> Res<()>;
    /// The registration as it is, `None` when there is none (or it is incomplete).
    fn read_registration(&mut self) -> Option<Registration>;
    /// Write the COM class and the state key.
    fn write_registration(&mut self, r: &Registration) -> Res<()>;
    /// Delete the COM class and the registration values of the state key; the texts of what could not be deleted
    /// (empty: all gone). **The record of moved-aside data folders ([`Machine::aside_record`]) is never touched and
    /// never part of the list**: it is not rewritten, so there is no moment in which it is gone, and nothing about it
    /// can make a rollback incomplete. The state key itself goes when no record is in it.
    fn delete_registration(&mut self) -> Vec<String>;
    /// Create the virtual camera and start it (`dshow`: the DirectShow class fallback first).
    fn create_camera(&mut self, dshow: bool) -> Res<()>;
    /// Remove the virtual camera from Windows. `Remove` on a camera that was never registered (or already
    /// removed) fails with `MF_E_INVALIDREQUEST`, so a failure alone does not say the camera is still there: see
    /// [`Machine::camera_listed`].
    fn remove_camera(&mut self) -> Res<()>;
    /// Windows lists the camera. When this cannot be found out, true (a camera that cannot be shown to be gone is
    /// not gone).
    fn camera_listed(&mut self) -> bool;
    /// Stop the camera service (Frame Server); true when it is stopped.
    fn stop_frame_server(&mut self) -> bool;
    /// Delete a file that is in use at the next restart.
    fn delete_at_reboot(&mut self, p: &Path) -> Res<()>;
}

/// The real machine.
#[derive(Default)]
pub struct RealMachine;

impl RealMachine {
    pub fn new() -> RealMachine {
        RealMachine
    }
}

/// Switch on the privileges that let the helper repair a folder somebody else owns (best effort).
fn enable_privileges() {
    unsafe {
        let mut token = windows::Win32::Foundation::HANDLE::default();
        if OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        )
        .is_err()
        {
            return;
        }
        for name in [
            "SeTakeOwnershipPrivilege",
            "SeRestorePrivilege",
            "SeBackupPrivilege",
        ] {
            let w = wide(name);
            let mut luid = Default::default();
            if LookupPrivilegeValueW(PCWSTR::null(), pcwstr(&w), &mut luid).is_err() {
                continue;
            }
            let tp = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                Privileges: [LUID_AND_ATTRIBUTES {
                    Luid: luid,
                    Attributes: SE_PRIVILEGE_ENABLED,
                }],
            };
            let _ = AdjustTokenPrivileges(token, false, Some(&tp), 0, None, None);
        }
        let _ = CloseHandle(token);
    }
}

fn not_found(e: &windows_core::Error) -> bool {
    [ERROR_NOT_FOUND, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND]
        .iter()
        .any(|c| e.code() == c.to_hresult())
}

impl Machine for RealMachine {
    fn start(&mut self) -> Res<()> {
        if crate::is_elevated() {
            enable_privileges();
        }
        init_com_and_mf().map_err(|e| werr("MFStartup", e))
    }

    fn confirm_user(&mut self, sid: &str) -> Res<()> {
        crate::acl::confirm_user_account(sid)
    }

    fn read_registration(&mut self) -> Option<Registration> {
        let r = registry::read_registered()?;
        Some(Registration {
            dll: r.dll_path?,
            build: r.build?,
            protocol: r.protocol?,
        })
    }

    fn write_registration(&mut self, r: &Registration) -> Res<()> {
        let key = vcam_protocol::clsid_key();
        registry::set_string(&key, None, vcam_protocol::CLSID_DESCRIPTION)?;
        let inproc = format!(r"{key}\InprocServer32");
        registry::set_string(&inproc, None, &r.dll)?;
        registry::set_string(&inproc, Some("ThreadingModel"), "Both")?;
        registry::set_string(vcam_protocol::VCAM_STATE_KEY, Some("DllPath"), &r.dll)?;
        registry::set_dword(vcam_protocol::VCAM_STATE_KEY, "Protocol", r.protocol)?;
        registry::set_string(vcam_protocol::VCAM_STATE_KEY, Some("Build"), &r.build)?;
        Ok(())
    }

    fn delete_registration(&mut self) -> Vec<String> {
        let mut failed = Vec::new();
        let mut note = |what: String, r: windows::Win32::Foundation::WIN32_ERROR| {
            if r != windows::Win32::Foundation::ERROR_SUCCESS && !registry::is_missing(r) {
                failed.push(format!("{what} (error {})", r.0));
            }
        };
        let clsid = vcam_protocol::clsid_key();
        note(format!("HKLM\\{clsid}"), registry::delete_tree(&clsid));
        // The state key also holds the record of moved-aside folders, which outlives the registration until the
        // folders are gone. So the registration's own values are deleted one by one and the key goes only when no
        // record is in it: the record is never deleted and written back, and is never missing for a moment.
        let state = vcam_protocol::VCAM_STATE_KEY;
        for value in ["DllPath", "Protocol", "Build"] {
            note(
                format!("HKLM\\{state}\\{value}"),
                registry::delete_value(state, value),
            );
        }
        if !registry::value_exists(state, registry::ASIDE_VALUE) {
            note(format!("HKLM\\{state}"), registry::delete_tree(state));
        }
        registry::delete_key_if_empty(r"SOFTWARE\BetterCam");
        failed
    }

    fn create_camera(&mut self, dshow: bool) -> Res<()> {
        let cam = create_virtual_camera()?;
        if dshow {
            add_dshow_class(&cam)?;
        }
        unsafe { cam.Start(None) }.map_err(|e| werr("IMFVirtualCamera::Start", e))?;
        let _ = unsafe { cam.Shutdown() };
        Ok(())
    }

    fn remove_camera(&mut self) -> Res<()> {
        let cam = create_virtual_camera()?;
        let r = unsafe { cam.Remove() };
        let _ = unsafe { cam.Shutdown() };
        match r {
            Ok(()) => Ok(()),
            Err(e) if not_found(&e) => Ok(()),
            Err(e) => Err(werr("IMFVirtualCamera::Remove", e)),
        }
    }

    fn camera_listed(&mut self) -> bool {
        crate::camera::is_listed().unwrap_or(true)
    }

    fn stop_frame_server(&mut self) -> bool {
        unsafe {
            let Ok(scm) = OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT) else {
                return false;
            };
            let name = wide("FrameServer");
            let mut stopped = false;
            if let Ok(svc) = OpenServiceW(scm, pcwstr(&name), SERVICE_STOP | SERVICE_QUERY_STATUS) {
                let mut st = SERVICE_STATUS::default();
                let _ = ControlService(svc, SERVICE_CONTROL_STOP, &mut st);
                for _ in 0..50 {
                    if QueryServiceStatus(svc, &mut st).is_ok()
                        && st.dwCurrentState == SERVICE_STOPPED
                    {
                        stopped = true;
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                let _ = CloseServiceHandle(svc);
            }
            let _ = CloseServiceHandle(scm);
            stopped
        }
    }

    fn delete_at_reboot(&mut self, p: &Path) -> Res<()> {
        let w = wide(&p.to_string_lossy());
        unsafe { MoveFileExW(pcwstr(&w), PCWSTR::null(), MOVEFILE_DELAY_UNTIL_REBOOT) }
            .map_err(|e| werr("MoveFileExW", e))
    }
}

impl AsideStore for RealMachine {
    fn aside_record(&mut self) -> Stored {
        registry::read_multi_string(vcam_protocol::VCAM_STATE_KEY, registry::ASIDE_VALUE)
    }

    fn write_aside_record(&mut self, strings: &[String]) -> Res<()> {
        if !strings.is_empty() {
            return registry::set_multi_string(
                vcam_protocol::VCAM_STATE_KEY,
                registry::ASIDE_VALUE,
                strings,
            );
        }
        let r = registry::delete_value(vcam_protocol::VCAM_STATE_KEY, registry::ASIDE_VALUE);
        if r != windows::Win32::Foundation::ERROR_SUCCESS && !registry::is_missing(r) {
            return Err(format!(
                "the record of old data folders could not be deleted (error {})",
                r.0
            ));
        }
        // the key holds nothing else when there is no registration: the record was all that kept it
        if registry::read_registered().is_none() {
            let _ = registry::delete_tree(vcam_protocol::VCAM_STATE_KEY);
            registry::delete_key_if_empty(r"SOFTWARE\BetterCam");
        }
        Ok(())
    }
}
