//! Elevated work of install and uninstall (plan 4.3 "Registration and unregistration"). Both functions need an
//! elevated process; they are shared by `better-cam.exe --vcam-install / --vcam-uninstall` and `vcam-tool`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use windows::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_DELAY_UNTIL_REBOOT};
use windows::Win32::System::Services::{
    CloseServiceHandle, ControlService, OpenSCManagerW, OpenServiceW, QueryServiceStatus,
    SC_MANAGER_CONNECT, SERVICE_CONTROL_STOP, SERVICE_QUERY_STATUS, SERVICE_STATUS, SERVICE_STOP,
    SERVICE_STOPPED,
};
use windows_core::PCWSTR;

use crate::acl::{ensure_frames_access, valid_sid};
use crate::camera::{add_dshow_class, create_virtual_camera};
use crate::log::Log;
use crate::registry;
use crate::util::{
    content_tag, data_dir, init_com_and_mf, install_dir, log_dir, pcwstr, werr, wide, Res,
};

/// Options of [`install`].
#[derive(Clone, Debug, Default)]
pub struct InstallOptions {
    /// SID of the user who asked; gets modify access to the data folder.
    pub user_sid: Option<String>,
    /// Also `AddProperty(DEVPKEY_Device_ClassGuid, GUID_DEVCLASS_CAMERA)` before `Start` (DirectShow fallback).
    pub dshow_class: bool,
}

/// Smallest size a real `better_cam_vcam.dll` can have; build placeholders are tiny.
const MIN_DLL_BYTES: usize = 64 * 1024;

/// A file that can be a real DLL: big enough and starts with `MZ`.
pub fn looks_like_dll(bytes: &[u8]) -> bool {
    bytes.len() >= MIN_DLL_BYTES && bytes.starts_with(b"MZ")
}

/// Whether the file at `path` can be a real DLL (reads its first bytes and its size only).
pub fn file_looks_like_dll(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let Ok(meta) = f.metadata() else {
        return false;
    };
    let mut head = [0u8; 2];
    meta.len() as usize >= MIN_DLL_BYTES && f.read_exact(&mut head).is_ok() && &head == b"MZ"
}

fn is_ours(p: &Path) -> bool {
    p.file_name()
        .and_then(|n| n.to_str())
        .map(|n| {
            n.starts_with(&format!("{}_", vcam_protocol::DLL_BASE_NAME)) && n.ends_with(".dll")
        })
        .unwrap_or(false)
}

/// Install or repair: copy the DLL under a versioned name, register it, prepare the data folder, create and start
/// the virtual camera, retire older DLLs. Running it again on a healthy install repairs it.
pub fn install(opts: &InstallOptions, dll_src: &Path, log: &mut dyn Log) -> Res<()> {
    if let Some(sid) = &opts.user_sid {
        if !valid_sid(sid) {
            return Err(format!("'{sid}' is not a user SID"));
        }
    }

    // 1. copy the DLL under a versioned name
    let bytes =
        std::fs::read(dll_src).map_err(|e| werr(&format!("read {}", dll_src.display()), e))?;
    if !looks_like_dll(&bytes) {
        return Err(format!(
            "{} is not the virtual camera DLL (build it with `npm run vcam:build`)",
            dll_src.display()
        ));
    }
    let tag = content_tag(&bytes);
    let dir = install_dir();
    std::fs::create_dir_all(&dir).map_err(|e| werr("create install dir", e))?;
    let dest = dir.join(vcam_protocol::versioned_dll_name(&tag));
    let same = std::fs::read(&dest).map(|d| d == bytes).unwrap_or(false);
    if same {
        log.line(&format!("dll already present: {}", dest.display()));
    } else {
        std::fs::write(&dest, &bytes).map_err(|e| werr(&format!("write {}", dest.display()), e))?;
        log.line(&format!("dll copied: {}", dest.display()));
    }

    // 2. COM registration in HKLM
    let key = vcam_protocol::clsid_key();
    registry::set_string(&key, None, vcam_protocol::CLSID_DESCRIPTION)?;
    let inproc = format!(r"{key}\InprocServer32");
    registry::set_string(&inproc, None, &dest.to_string_lossy())?;
    registry::set_string(&inproc, Some("ThreadingModel"), "Both")?;
    log.line(&format!("clsid key written: HKLM\\{key}"));

    // 3. state key
    registry::set_string(
        vcam_protocol::VCAM_STATE_KEY,
        Some("DllPath"),
        &dest.to_string_lossy(),
    )?;
    registry::set_dword(
        vcam_protocol::VCAM_STATE_KEY,
        "Protocol",
        vcam_protocol::PROTOCOL_VERSION,
    )?;
    registry::set_string(vcam_protocol::VCAM_STATE_KEY, Some("Build"), &tag)?;

    // 4. data folders with an explicit ACL
    let logs = log_dir();
    std::fs::create_dir_all(&logs).map_err(|e| werr("create log dir", e))?;
    ensure_frames_access(opts.user_sid.as_deref(), log)?;
    log.line(&format!("data dir ready: {}", logs.display()));

    // 5. + 6. virtual camera
    init_com_and_mf().map_err(|e| werr("MFStartup", e))?;
    let cam = create_virtual_camera()?;
    log.line("registered");
    if opts.dshow_class {
        add_dshow_class(&cam)?;
        log.line("added DEVPKEY_Device_ClassGuid = GUID_DEVCLASS_CAMERA");
    }
    unsafe { cam.Start(None) }.map_err(|e| werr("IMFVirtualCamera::Start", e))?;
    log.line("started");
    let _ = unsafe { cam.Shutdown() };

    // 7. older DLLs
    remove_old_dlls(&dir, &dest, log);
    Ok(())
}

/// Retire the DLL versions other than `keep`. One that Frame Server still has loaded cannot be deleted, and Frame
/// Server also keeps serving new camera users from it (a loaded class is not looked up again in the registry), so
/// the update would not take effect until a reboot. Stopping the service fixes both: it starts again by itself the
/// next time an app opens a camera. Apps that have the camera open lose their stream and have to open it again.
fn remove_old_dlls(dir: &Path, keep: &Path, log: &mut dyn Log) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut locked = Vec::new();
    for e in rd.flatten() {
        let p = e.path();
        if is_ours(&p) && p != keep {
            if std::fs::remove_file(&p).is_ok() {
                log.line(&format!("deleted {}", p.display()));
            } else {
                locked.push(p);
            }
        }
    }
    if !locked.is_empty() {
        log.line("an older DLL is loaded by Frame Server; stopping the service so the new build loads");
        stop_frame_server(log);
        for p in &locked {
            delete_or_delay(p, log);
        }
    }
}

/// Delete `p`; a file that is locked (Frame Server has the DLL loaded) is scheduled for deletion at the next reboot.
fn delete_or_delay(p: &Path, log: &mut dyn Log) {
    match std::fs::remove_file(p) {
        Ok(()) => log.line(&format!("deleted {}", p.display())),
        Err(_) => {
            let w = wide(&p.to_string_lossy());
            let r = unsafe { MoveFileExW(pcwstr(&w), PCWSTR::null(), MOVEFILE_DELAY_UNTIL_REBOOT) };
            match r {
                Ok(()) => log.line(&format!(
                    "locked, delete scheduled at reboot: {}",
                    p.display()
                )),
                Err(e) => log.line(&format!("could not delete {}: {e}", p.display())),
            }
        }
    }
}

/// Stop the `FrameServer` service (needs elevation). Returns true when it is stopped.
fn stop_frame_server(log: &mut dyn Log) -> bool {
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
                std::thread::sleep(Duration::from_millis(100));
            }
            let _ = CloseServiceHandle(svc);
        }
        let _ = CloseServiceHandle(scm);
        log.line(&format!("FrameServer stop requested, stopped: {stopped}"));
        stopped
    }
}

/// Delete the data folder; tries a few times because a stream that just ended may still hold `frames.bin`.
fn try_remove_dir(dir: &Path) -> bool {
    for attempt in 0..6 {
        if !dir.exists() || std::fs::remove_dir_all(dir).is_ok() {
            return true;
        }
        if attempt < 5 {
            std::thread::sleep(Duration::from_millis(300));
        }
    }
    !dir.exists()
}

/// Remove `%ProgramData%\BetterCam`. When something still holds a file in it, Frame Server is the usual suspect (it
/// has `frames.bin` mapped while a consumer is attached): stop it (once) and try again.
fn remove_data_dir(frame_server_stopped: bool, log: &mut dyn Log) {
    let dir = data_dir();
    if try_remove_dir(&dir) {
        return;
    }
    if !frame_server_stopped {
        log.line("the data folder is in use; stopping Frame Server");
        stop_frame_server(log);
        if try_remove_dir(&dir) {
            return;
        }
    }
    log.line(&format!(
        "could not remove {} (a file is in use); it is harmless and the next install reuses it",
        dir.display()
    ));
}

/// Remove the virtual camera, the HKLM keys, the DLLs and the data folder, in that order (the camera first, so
/// nothing recreates what is deleted behind it). The caller must have closed `frames.bin` in its own process.
pub fn uninstall(log: &mut dyn Log) -> Res<()> {
    init_com_and_mf().map_err(|e| werr("MFStartup", e))?;
    match create_virtual_camera() {
        Ok(cam) => {
            match unsafe { cam.Remove() } {
                Ok(()) => log.line("removed"),
                Err(e) => log.line(&format!("Remove failed: {e}")),
            }
            let _ = unsafe { cam.Shutdown() };
        }
        Err(e) => log.line(&format!("could not open the virtual camera: {e}")),
    }

    // registry
    let clsid = vcam_protocol::clsid_key();
    let r = registry::delete_tree(&clsid);
    log.line(&format!("delete HKLM\\{clsid}: {r:?}"));
    let r = registry::delete_tree(vcam_protocol::VCAM_STATE_KEY);
    log.line(&format!(
        "delete HKLM\\{}: {r:?}",
        vcam_protocol::VCAM_STATE_KEY
    ));
    registry::delete_key_if_empty(r"SOFTWARE\BetterCam");

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
            log.line(&format!("deleted {}", p.display()));
        } else {
            locked.push(p.clone());
        }
    }
    let mut frame_server_stopped = false;
    if !locked.is_empty() {
        log.line("some DLLs are loaded by Frame Server; stopping the service");
        frame_server_stopped = stop_frame_server(log);
        for p in &locked {
            delete_or_delay(p, log);
        }
    }
    let _ = std::fs::remove_dir(&dir);
    if let Some(parent) = dir.parent() {
        let _ = std::fs::remove_dir(parent);
    }

    // the data folder, last: the log of this run lives in it
    remove_data_dir(frame_server_stopped, log);
    log.line("unregistered");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_big_mz_file_is_a_dll() {
        let mut real = vec![0u8; MIN_DLL_BYTES];
        real[0] = b'M';
        real[1] = b'Z';
        assert!(looks_like_dll(&real));
        assert!(!looks_like_dll(&real[..MIN_DLL_BYTES - 1]));
        assert!(!looks_like_dll(b"placeholder"));
        assert!(!looks_like_dll(&vec![0u8; MIN_DLL_BYTES]));
    }

    #[test]
    fn our_dll_names_are_recognised() {
        assert!(is_ours(Path::new(r"C:\x\better_cam_vcam_1.abcd1234.dll")));
        assert!(!is_ours(Path::new(r"C:\x\better_cam_vcam.dll")));
        assert!(!is_ours(Path::new(r"C:\x\other_1.abcd1234.dll")));
        assert!(!is_ours(Path::new(r"C:\x\better_cam_vcam_1.abcd1234.txt")));
    }

    #[test]
    fn a_bad_sid_stops_the_install_before_anything_is_touched() {
        let opts = InstallOptions {
            user_sid: Some("S-1-5-21-1)(A;;FA;;;WD".into()),
            dshow_class: false,
        };
        let mut log = crate::log::VecLog::default();
        let err = install(&opts, Path::new(r"C:\does\not\exist.dll"), &mut log).unwrap_err();
        assert!(err.contains("not a user SID"), "{err}");
        assert!(log.0.is_empty());
    }

    #[test]
    fn a_missing_or_placeholder_dll_is_refused_before_anything_is_touched() {
        let mut log = crate::log::VecLog::default();
        let opts = InstallOptions::default();
        assert!(install(&opts, Path::new(r"C:\does\not\exist.dll"), &mut log).is_err());
        let dir = std::env::temp_dir().join(format!("vcam-install-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("better_cam_vcam.dll");
        std::fs::write(&fake, b"placeholder").unwrap();
        let err = install(&opts, &fake, &mut log).unwrap_err();
        assert!(err.contains("not the virtual camera DLL"), "{err}");
        assert!(!file_looks_like_dll(&fake));
        assert!(log.0.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
