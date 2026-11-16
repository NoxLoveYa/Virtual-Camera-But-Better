//! Elevated work of `register` / `unregister` (plan 4.3 "Registration and unregistration").

use std::path::{Path, PathBuf};

use vcam_protocol::{CAMERA_NAME, CLSID_STRING};
use windows::Win32::Foundation::{LocalFree, ERROR_SUCCESS, HLOCAL};
use windows::Win32::Media::MediaFoundation::{
    IMFVirtualCamera, MFCreateVirtualCamera, MFVirtualCameraAccess_AllUsers,
    MFVirtualCameraLifetime_System, MFVirtualCameraType_SoftwareCameraSource,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SetNamedSecurityInfoW, SDDL_REVISION_1,
    SE_FILE_OBJECT,
};
use windows::Win32::Security::{
    GetSecurityDescriptorDacl, ACL, DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR,
};
use windows::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_DELAY_UNTIL_REBOOT};
use windows::Win32::System::Registry::{
    RegDeleteKeyW, RegDeleteTreeW, RegSetKeyValueW, HKEY_LOCAL_MACHINE, REG_DWORD, REG_SZ,
};
use windows::Win32::System::Services::{
    CloseServiceHandle, ControlService, OpenSCManagerW, OpenServiceW, QueryServiceStatus,
    SC_MANAGER_CONNECT, SERVICE_CONTROL_STOP, SERVICE_QUERY_STATUS, SERVICE_STATUS, SERVICE_STOP,
    SERVICE_STOPPED,
};
use windows_core::{GUID, PCWSTR};

use crate::util::{content_tag, data_dir, install_dir, log_dir, pcwstr, wide, Out};

type Res<T> = Result<T, String>;

fn werr(what: &str, e: impl std::fmt::Display) -> String {
    format!("{what}: {e}")
}

/// Options of the elevated `register`.
pub struct RegisterOptions {
    /// SID of the user who ran the tool; gets modify access to the data folders.
    pub user_sid: Option<String>,
    /// Also `AddProperty(DEVPKEY_Device_ClassGuid, GUID_DEVCLASS_CAMERA)` before `Start` (DirectShow fallback).
    pub dshow_class: bool,
}

fn reg_set_string(subkey: &str, name: Option<&str>, value: &str) -> Res<()> {
    let sk = wide(subkey);
    let nm = name.map(wide);
    let data = wide(value);
    let r = unsafe {
        RegSetKeyValueW(
            HKEY_LOCAL_MACHINE,
            pcwstr(&sk),
            nm.as_ref().map(|n| pcwstr(n)).unwrap_or(PCWSTR::null()),
            REG_SZ.0,
            Some(data.as_ptr() as *const _),
            (data.len() * 2) as u32,
        )
    };
    if r == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(format!("RegSetKeyValueW HKLM\\{subkey}: {r:?}"))
    }
}

fn reg_set_dword(subkey: &str, name: &str, value: u32) -> Res<()> {
    let sk = wide(subkey);
    let nm = wide(name);
    let r = unsafe {
        RegSetKeyValueW(
            HKEY_LOCAL_MACHINE,
            pcwstr(&sk),
            pcwstr(&nm),
            REG_DWORD.0,
            Some(&value as *const u32 as *const _),
            4,
        )
    };
    if r == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(format!("RegSetKeyValueW HKLM\\{subkey}\\{name}: {r:?}"))
    }
}

/// Apply a protected DACL: SYSTEM and Administrators full, LOCAL SERVICE and `user_sid` modify.
fn set_data_acl(path: &Path, user_sid: Option<&str>) -> Res<()> {
    const MODIFY: &str = "0x1301bf";
    let mut sddl = format!("D:PAI(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;{MODIFY};;;LS)");
    if let Some(sid) = user_sid {
        sddl.push_str(&format!("(A;OICI;{MODIFY};;;{sid})"));
    }
    let sddl_w = wide(&sddl);
    let mut psd = PSECURITY_DESCRIPTOR::default();
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            pcwstr(&sddl_w),
            SDDL_REVISION_1,
            &mut psd,
            None,
        )
        .map_err(|e| werr("ConvertStringSecurityDescriptorToSecurityDescriptorW", e))?;
    }
    let result = (|| {
        let mut present = windows_core::BOOL(0);
        let mut defaulted = windows_core::BOOL(0);
        let mut dacl: *mut ACL = std::ptr::null_mut();
        unsafe {
            GetSecurityDescriptorDacl(psd, &mut present, &mut dacl, &mut defaulted)
                .map_err(|e| werr("GetSecurityDescriptorDacl", e))?;
        }
        let path_w = wide(&path.to_string_lossy());
        let r = unsafe {
            SetNamedSecurityInfoW(
                pcwstr(&path_w),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                None,
                None,
                Some(dacl),
                None,
            )
        };
        if r == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(format!("SetNamedSecurityInfoW {}: {r:?}", path.display()))
        }
    })();
    unsafe {
        let _ = LocalFree(Some(HLOCAL(psd.0)));
    }
    result
}

fn create_virtual_camera() -> Res<IMFVirtualCamera> {
    let name = wide(CAMERA_NAME);
    let clsid = wide(CLSID_STRING);
    unsafe {
        MFCreateVirtualCamera(
            MFVirtualCameraType_SoftwareCameraSource,
            MFVirtualCameraLifetime_System,
            MFVirtualCameraAccess_AllUsers,
            pcwstr(&name),
            pcwstr(&clsid),
            None,
        )
    }
    .map_err(|e| werr("MFCreateVirtualCamera", e))
}

/// `GUID_DEVCLASS_CAMERA` {ca3e7ab9-b4c3-4ae6-8251-579ef933890f}.
const GUID_DEVCLASS_CAMERA: GUID = GUID::from_u128(0xca3e7ab9_b4c3_4ae6_8251_579ef933890f);

fn add_dshow_class(cam: &IMFVirtualCamera) -> Res<()> {
    use windows::Win32::Devices::Properties::{DEVPKEY_Device_ClassGuid, DEVPROP_TYPE_GUID};
    let bytes: [u8; 16] = unsafe { std::mem::transmute(GUID_DEVCLASS_CAMERA) };
    unsafe { cam.AddProperty(&DEVPKEY_Device_ClassGuid, DEVPROP_TYPE_GUID, &bytes) }
        .map_err(|e| werr("IMFVirtualCamera::AddProperty(DEVPKEY_Device_ClassGuid)", e))
}

fn dll_next_to_exe() -> Res<PathBuf> {
    let exe = std::env::current_exe().map_err(|e| werr("current_exe", e))?;
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

pub fn register(opts: &RegisterOptions, out: &mut Out) -> Res<()> {
    // 1. copy the DLL under a versioned name
    let src = dll_next_to_exe()?;
    let bytes = std::fs::read(&src).map_err(|e| werr("read DLL", e))?;
    let tag = content_tag(&bytes);
    let dir = install_dir();
    std::fs::create_dir_all(&dir).map_err(|e| werr("create install dir", e))?;
    let dest = dir.join(vcam_protocol::versioned_dll_name(&tag));
    let same = std::fs::read(&dest).map(|d| d == bytes).unwrap_or(false);
    if same {
        out.line(&format!("dll already present: {}", dest.display()));
    } else {
        std::fs::write(&dest, &bytes).map_err(|e| werr(&format!("write {}", dest.display()), e))?;
        out.line(&format!("dll copied: {}", dest.display()));
    }

    // 2. COM registration in HKLM
    let key = vcam_protocol::clsid_key();
    reg_set_string(&key, None, vcam_protocol::CLSID_DESCRIPTION)?;
    let inproc = format!(r"{key}\InprocServer32");
    reg_set_string(&inproc, None, &dest.to_string_lossy())?;
    reg_set_string(&inproc, Some("ThreadingModel"), "Both")?;
    out.line(&format!("clsid key written: HKLM\\{key}"));

    // 3. state key
    reg_set_string(
        vcam_protocol::VCAM_STATE_KEY,
        Some("DllPath"),
        &dest.to_string_lossy(),
    )?;
    reg_set_dword(
        vcam_protocol::VCAM_STATE_KEY,
        "Protocol",
        vcam_protocol::PROTOCOL_VERSION,
    )?;
    reg_set_string(vcam_protocol::VCAM_STATE_KEY, Some("Build"), &tag)?;

    // 4. data folders with an explicit ACL
    let logs = log_dir();
    std::fs::create_dir_all(&logs).map_err(|e| werr("create log dir", e))?;
    set_data_acl(&data_dir(), opts.user_sid.as_deref())?;
    out.line(&format!("data dir ready: {}", logs.display()));

    // 5. + 6. virtual camera
    crate::util::init_com_and_mf().map_err(|e| werr("MFStartup", e))?;
    let cam = create_virtual_camera()?;
    out.line("registered");
    if opts.dshow_class {
        add_dshow_class(&cam)?;
        out.line("added DEVPKEY_Device_ClassGuid = GUID_DEVCLASS_CAMERA");
    }
    unsafe { cam.Start(None) }.map_err(|e| werr("IMFVirtualCamera::Start", e))?;
    out.line("started");
    let _ = unsafe { cam.Shutdown() };

    // 7. older DLLs
    remove_old_dlls(&dir, &dest, out);
    Ok(())
}

fn remove_old_dlls(dir: &Path, keep: &Path, out: &mut Out) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        let is_ours = p
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| {
                n.starts_with(&format!("{}_", vcam_protocol::DLL_BASE_NAME)) && n.ends_with(".dll")
            })
            .unwrap_or(false);
        if is_ours && p != keep {
            delete_or_delay(&p, out);
        }
    }
}

fn delete_or_delay(p: &Path, out: &mut Out) {
    match std::fs::remove_file(p) {
        Ok(()) => out.line(&format!("deleted {}", p.display())),
        Err(_) => {
            let w = wide(&p.to_string_lossy());
            let r = unsafe { MoveFileExW(pcwstr(&w), PCWSTR::null(), MOVEFILE_DELAY_UNTIL_REBOOT) };
            match r {
                Ok(()) => out.line(&format!(
                    "locked, delete scheduled at reboot: {}",
                    p.display()
                )),
                Err(e) => out.line(&format!("could not delete {}: {e}", p.display())),
            }
        }
    }
}

/// Stop the `FrameServer` service (needs elevation). Returns true when it is stopped.
fn stop_frame_server(out: &mut Out) -> bool {
    unsafe {
        let Ok(scm) = OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT) else {
            return false;
        };
        let name = wide("FrameServer");
        let svc = OpenServiceW(scm, pcwstr(&name), SERVICE_STOP | SERVICE_QUERY_STATUS);
        let mut stopped = false;
        if let Ok(svc) = svc {
            let mut st = SERVICE_STATUS::default();
            let _ = ControlService(svc, SERVICE_CONTROL_STOP, &mut st);
            for _ in 0..50 {
                if QueryServiceStatus(svc, &mut st).is_ok() && st.dwCurrentState == SERVICE_STOPPED
                {
                    stopped = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            let _ = CloseServiceHandle(svc);
        }
        let _ = CloseServiceHandle(scm);
        out.line(&format!("FrameServer stop requested, stopped: {stopped}"));
        stopped
    }
}

pub fn unregister(out: &mut Out) -> Res<()> {
    crate::util::init_com_and_mf().map_err(|e| werr("MFStartup", e))?;
    match create_virtual_camera() {
        Ok(cam) => {
            match unsafe { cam.Remove() } {
                Ok(()) => out.line("removed"),
                Err(e) => out.line(&format!("Remove failed: {e}")),
            }
            let _ = unsafe { cam.Shutdown() };
        }
        Err(e) => out.line(&format!("could not open the virtual camera: {e}")),
    }

    // registry
    let key = wide(&vcam_protocol::clsid_key());
    let r = unsafe { RegDeleteTreeW(HKEY_LOCAL_MACHINE, pcwstr(&key)) };
    out.line(&format!(
        "delete HKLM\\{}: {r:?}",
        vcam_protocol::clsid_key()
    ));
    let state = wide(vcam_protocol::VCAM_STATE_KEY);
    let r = unsafe { RegDeleteTreeW(HKEY_LOCAL_MACHINE, pcwstr(&state)) };
    out.line(&format!(
        "delete HKLM\\{}: {r:?}",
        vcam_protocol::VCAM_STATE_KEY
    ));
    let parent = wide(r"SOFTWARE\BetterCam");
    let _ = unsafe { RegDeleteKeyW(HKEY_LOCAL_MACHINE, pcwstr(&parent)) };

    // DLLs: delete; if locked by Frame Server stop the service and retry; else delay-delete
    let dir = install_dir();
    let dlls: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().map(|x| x == "dll").unwrap_or(false))
                .collect()
        })
        .unwrap_or_default();
    let mut locked = Vec::new();
    for p in &dlls {
        if std::fs::remove_file(p).is_ok() {
            out.line(&format!("deleted {}", p.display()));
        } else {
            locked.push(p.clone());
        }
    }
    if !locked.is_empty() {
        out.line("some DLLs are loaded by Frame Server; stopping the service");
        stop_frame_server(out);
        for p in &locked {
            delete_or_delay(p, out);
        }
    }
    let _ = std::fs::remove_dir(&dir);
    if let Some(parent) = dir.parent() {
        let _ = std::fs::remove_dir(parent);
    }
    out.line("unregistered");
    Ok(())
}
