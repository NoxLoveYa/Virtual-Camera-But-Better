//! What install and uninstall do to the machine outside the data folder: the registry, the virtual camera object, the
//! camera service, files that wait for a reboot. A trait, so the order of the steps and the rollback are tested with
//! a fake that fails on demand; [`RealMachine`] is the Windows implementation.

use std::path::Path;
use std::time::Duration;

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

/// The parts of the machine that install and uninstall change besides the data folder and the DLL copies.
pub trait Machine {
    /// Prepare the process: privileges, COM, Media Foundation. Called once, before anything is changed.
    fn start(&mut self) -> Res<()>;
    /// `sid` names a user account on this computer.
    fn confirm_user(&mut self, sid: &str) -> Res<()>;
    /// The registration as it is, `None` when there is none (or it is incomplete).
    fn read_registration(&mut self) -> Option<Registration>;
    /// Write the COM class and the state key.
    fn write_registration(&mut self, r: &Registration) -> Res<()>;
    /// Delete the COM class and the state key; the texts of what could not be deleted (empty: all gone).
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
        for key in [
            vcam_protocol::clsid_key(),
            vcam_protocol::VCAM_STATE_KEY.to_string(),
        ] {
            let r = registry::delete_tree(&key);
            if r != windows::Win32::Foundation::ERROR_SUCCESS && r != ERROR_FILE_NOT_FOUND {
                failed.push(format!("HKLM\\{key} (error {})", r.0));
            }
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
