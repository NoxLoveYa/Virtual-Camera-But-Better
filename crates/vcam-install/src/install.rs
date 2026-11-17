//! Elevated work of install and uninstall (plan 4.3 "Registration and unregistration"). Both functions need an
//! elevated process; they are shared by `better-cam.exe --vcam-install / --vcam-uninstall` and `vcam-tool`.
//!
//! The steps run against a [`Machine`] and a set of [`Paths`], so their order and their rollback are tested with a
//! fake machine and temp folders; [`install`] and [`uninstall`] are the same code on the real machine.

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use vcam_protocol::secure;

use crate::acl::{self, valid_sid, Ace, Principals, SecurityDescriptor, FULL};
use crate::aside;
use crate::datadir;
use crate::log::Log;
use crate::machine::{Machine, RealMachine, Registration};
use crate::util::{content_tag, werr, Paths, Res};

/// Options of [`install`].
#[derive(Clone, Debug, Default)]
pub struct InstallOptions {
    /// SID of the user who asked; gets read and write access to the frame file.
    pub user_sid: Option<String>,
    /// Also `AddProperty(DEVPKEY_Device_ClassGuid, GUID_DEVCLASS_CAMERA)` before `Start` (DirectShow fallback).
    pub dshow_class: bool,
}

/// How an install ended when it did not fail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstallOutcome {
    /// Installed and in effect.
    Done,
    /// Installed, but the camera service still has the old DLL loaded and could not be stopped: the new build takes
    /// effect when that service restarts (it does when the next app opens a camera after it was stopped) or after a
    /// restart of Windows.
    PendingServiceRestart,
}

/// How an uninstall ended when it did not fail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UninstallOutcome {
    Done,
    /// Removed, but a file that the camera service holds is deleted at the next restart of Windows.
    PendingReboot,
}

/// Smallest size a real `better_cam_vcam.dll` can have; build placeholders are tiny.
const MIN_DLL_BYTES: usize = 64 * 1024;

/// A file that can be a real DLL: big enough and starts with `MZ`.
pub fn looks_like_dll(bytes: &[u8]) -> bool {
    bytes.len() >= MIN_DLL_BYTES && bytes.starts_with(b"MZ")
}

/// Whether the file at `path` can be a real DLL (reads its first bytes and its size only). A file that another
/// program holds an opportunistic lock on is not waited for (the open is the refusal of [`secure::open_with`]): it
/// does not look like a DLL for now.
pub fn file_looks_like_dll(path: &Path) -> bool {
    use std::io::Read;
    let Ok(h) = secure::open_read_following(path) else {
        return false;
    };
    let mut f = std::fs::File::from(h);
    let Ok(meta) = f.metadata() else {
        return false;
    };
    let mut head = [0u8; 2];
    meta.len() as usize >= MIN_DLL_BYTES && f.read_exact(&mut head).is_ok() && &head == b"MZ"
}

/// Delete one file of the installation without waiting for another program. `Program Files` can be read by a standard
/// user, and a program of theirs can hold an opportunistic lock on a DLL there, which makes `std::fs::remove_file`
/// wait (with the machine-wide lock taken). A file that is locked, or loaded by the camera service, is "in use" as
/// before, and the callers go on from there (stop the service, delete at the next restart).
fn delete_file(p: &Path) -> std::io::Result<()> {
    secure::remove_tree(p).map_err(std::io::Error::from)
}

/// The sentence for a read, write, delete or rename of `path` that failed with `e`. A file that another program holds
/// an opportunistic lock on is said so, with its name: nothing here waits for such a program, and the person who
/// reads it knows what to close.
fn file_error(what: &str, path: &Path, e: &std::io::Error) -> String {
    if secure::is_oplock_busy(e) {
        format!(
            "{} is locked by another program (it holds an opportunistic lock); not waiting for it. Close the program that has it open and try again",
            path.display()
        )
    } else {
        werr(&format!("{what} {}", path.display()), e)
    }
}

fn is_ours(p: &Path) -> bool {
    p.file_name()
        .and_then(|n| n.to_str())
        .map(|n| {
            n.starts_with(&format!("{}_", vcam_protocol::DLL_BASE_NAME)) && n.ends_with(".dll")
        })
        .unwrap_or(false)
}

/// Install or repair on this machine: prepare the data folder, copy the DLL under a versioned name, register it,
/// create and start the virtual camera, retire older DLLs. Running it again on a healthy install repairs it.
pub fn install(opts: &InstallOptions, dll_src: &Path, log: &mut dyn Log) -> Res<InstallOutcome> {
    install_with(
        opts,
        dll_src,
        &Paths::system(),
        None,
        &mut RealMachine::new(),
        log,
    )
}

/// [`install`] against any machine and folders. `principals` replaces the real set of accounts (tests); `None`
/// builds it from `opts.user_sid`, which must be a user account.
///
/// Order, chosen so that a failure leaves the machine as it was: validate everything; prepare the data folder; copy
/// the DLL (to a temporary name, then renamed); write the registration; create and start the camera; only then
/// delete old DLLs. A failure after the registration was written puts the earlier registration back (or removes
/// ours when there was none) and deletes the DLL copy it made, so a registered class never names a missing file.
pub fn install_with(
    opts: &InstallOptions,
    dll_src: &Path,
    paths: &Paths,
    principals: Option<Principals>,
    m: &mut dyn Machine,
    log: &mut dyn Log,
) -> Res<InstallOutcome> {
    // 0. everything that can be checked without changing anything
    let principals = match principals {
        Some(p) => p,
        None => {
            if let Some(sid) = &opts.user_sid {
                if !valid_sid(sid) {
                    return Err(format!("'{sid}' is not a user SID"));
                }
                m.confirm_user(sid)?;
            }
            Principals::machine(opts.user_sid.as_deref())
        }
    };
    // the source can be in a folder the user runs the app from: a program of theirs may hold a lock on it, which
    // makes a plain read wait (with the machine-wide lock taken)
    let bytes = secure::read_file(dll_src).map_err(|e| file_error("read", dll_src, &e))?;
    if !looks_like_dll(&bytes) {
        return Err(format!(
            "{} is not the virtual camera DLL (build it with `npm run vcam:build`)",
            dll_src.display()
        ));
    }
    let tag = content_tag(&bytes);
    let dest = paths.install.join(vcam_protocol::versioned_dll_name(&tag));
    m.start()?;
    // the registration to go back to: only one whose DLL is really there
    let before = m
        .read_registration()
        .filter(|r| Path::new(&r.dll).is_file());

    // 1. the data folder (no registration exists because of this step, nothing to undo). A data folder that is moved
    // aside is put on the record by the preparation itself, before the rename (with its identity), and taken off
    // again when the move does not happen or the old copy is deleted at once: what stays on the record is what a
    // later run deletes, also when preparing fails after that.
    let prepared = {
        let machine: RefCell<&mut dyn Machine> = RefCell::new(&mut *m);
        let mut ledger = aside::MachineLedger(&machine);
        datadir::prepare_recording(
            paths,
            &principals,
            &mut || machine.borrow_mut().stop_frame_server(),
            log,
            &mut ledger,
        )
    };
    if let Err(e) = prepared {
        // The log was let go of while the folder was worked on: whatever the caller writes about this failure (the
        // `ERROR:` line the app shows) must reach the file, so make it ready again before returning.
        log.folder_ready();
        return Err(e);
    }
    log.folder_ready();
    log.line(&format!("data folder ready: {}", paths.data.display()));

    // 2. the DLL
    let copied_new = copy_dll(&bytes, &dest, &principals, log)?;

    // 3. + 4. registration and camera, with rollback
    let reg = Registration {
        dll: dest.to_string_lossy().into_owned(),
        build: tag,
        protocol: vcam_protocol::PROTOCOL_VERSION,
    };
    let step = (|| -> Res<()> {
        m.write_registration(&reg)?;
        log.line(&format!(
            "registered: HKLM\\{} -> {}",
            vcam_protocol::clsid_key(),
            reg.dll
        ));
        m.create_camera(opts.dshow_class)?;
        log.line("camera created and started");
        Ok(())
    })();
    if let Err(cause) = step {
        let tail = rollback(&before, &dest, copied_new, m, log);
        return Err(format!("{cause}{tail}"));
    }

    // 5. only now the older DLLs
    let pending = remove_old_dlls(&paths.install, &dest, m, log);
    sweep_recorded(&mut *m, log, |recorded, log| {
        datadir::sweep_asides(paths, &principals.trusted_owners, recorded, log)
    });
    Ok(if pending {
        log.line(
            "installed; the camera service still has an older build loaded: the new one takes effect after that service restarts or Windows restarts",
        );
        InstallOutcome::PendingServiceRestart
    } else {
        InstallOutcome::Done
    })
}

/// Read the record of moved-aside data folders, run `sweep` on its entries, and apply what it found to the record (the
/// entries it is done with go, the ones that stay move to the end). A record that cannot be read is not acted on and
/// not touched afterwards (`sweep` then gets no entries); a record that cannot be written is a line of the log: it
/// never fails an install or a removal.
fn sweep_recorded(
    m: &mut dyn Machine,
    log: &mut dyn Log,
    sweep: impl FnOnce(&[aside::AsideEntry], &mut dyn Log) -> datadir::AsideSweep,
) -> datadir::AsideSweep {
    let recorded = aside::load_logged(&mut *m, log);
    let swept = sweep(recorded.as_deref().unwrap_or(&[]), log);
    if recorded.is_some() {
        if let Err(e) = aside::finish_sweep(m, &swept) {
            log.line(&format!(
                "the record of old data folders was not updated: {e}"
            ));
        }
    }
    swept
}

/// The sentence for a rename of `from` to `to` that failed with `e`. Both files are named: a lock of another program
/// on either one (the open of the one that is moved, or the replace of the one that is there) gives the same refusal,
/// and the person who reads it must know which two files it is about.
fn rename_error(from: &Path, to: &Path, e: &std::io::Error) -> String {
    if secure::is_oplock_busy(e) {
        format!(
            "cannot move {} to {}: one of the two files is locked by another program (it holds an opportunistic lock); not waiting for it. Close the program that has it open and try again",
            from.display(),
            to.display()
        )
    } else {
        werr(&format!("move {} to {}", from.display(), to.display()), e)
    }
}

/// Give the DLL copy at `dest` the permissions of the folder it is in (`Program Files`'): the camera service reads it
/// as LOCAL SERVICE through them. A rename keeps the security descriptor of the file that was moved, which is the
/// protected one the temporary file was made with, so this runs after every rename (see
/// [`acl::restore_inheritance`]). The file is looked at on the handle that is changed: a plain file, one link.
fn inherit_permissions(dest: &Path) -> Res<()> {
    // READ_CONTROL | WRITE_DAC | FILE_READ_ATTRIBUTES: nothing that breaks a lock
    let h = secure::open_with(
        dest,
        0x0002_0000 | 0x0004_0000 | 0x80,
        secure::SHARE_ALL,
        secure::DISP_OPEN_EXISTING,
        false,
        0,
    )
    .map_err(|e| file_error("open", dest, &e))?;
    secure::verify_file(secure::raw(&h), None).map_err(|e| werr(&dest.display().to_string(), e))?;
    acl::restore_inheritance(&h).map_err(|e| werr(&dest.display().to_string(), e))
}

/// Write `bytes` to `dest` unless it is already there. Returns true when a new file was made.
///
/// `Program Files` can be read by a standard user, so a program of theirs can hold an opportunistic lock on the copy
/// that is there (or on a leftover temporary file), which makes a plain read, write or rename wait with the
/// machine-wide lock taken. Nothing here waits: the copy is read through [`secure::read_file`], the temporary file is
/// made and moved into place through handles, and a file that is locked is an error that names it.
///
/// The temporary file is made with a protected DACL that grants only SYSTEM and Administrators (a standard user
/// cannot open it, so cannot lock it between the write and the rename), written, flushed to disk and renamed over
/// `dest`; the rename keeps that DACL, so the final file then gets the permissions its folder hands down
/// ([`inherit_permissions`]). A file whose permissions could not be set is deleted again: a copy that the camera
/// service cannot read must not be taken for a good one by the next Repair. A copy that is there already with the
/// right contents gets the same permissions step (a helper that died before it left one with the private
/// permissions, which every byte-for-byte check would call good); when that fails the install fails, and the copy is
/// left where it is.
fn copy_dll(bytes: &[u8], dest: &Path, p: &Principals, log: &mut dyn Log) -> Res<bool> {
    copy_dll_with(bytes, dest, p, log, &mut |_| {}, &mut inherit_permissions)
}

/// [`copy_dll`] with a hook that runs between the write of the temporary file and its rename (tests look at the
/// file as it is then), and the step that gives the copy the permissions of its folder (`inherit`: tests make it
/// fail; production passes [`inherit_permissions`]).
fn copy_dll_with(
    bytes: &[u8],
    dest: &Path,
    p: &Principals,
    log: &mut dyn Log,
    between: &mut dyn FnMut(&Path),
    inherit: &mut dyn FnMut(&Path) -> Res<()>,
) -> Res<bool> {
    match secure::read_file(dest) {
        Ok(there) if there == bytes => {
            // A copy with the right contents may still have the private permissions of the temporary file (a helper
            // that died between the rename and `inherit_permissions`, or a failure of that call and of the delete
            // after it): the camera service could never load it, and no later Repair would say so. So the
            // permissions are brought to the inherited state here too (harmless when they already are), and a
            // failure is an error, not a silent success. The copy is not deleted: it may be the registered one,
            // loaded by the service right now.
            inherit(dest).map_err(|e| {
                format!(
                    "the installed copy {} is there but could not be given the permissions of its folder, so the camera service may not be able to load it: {e}",
                    dest.display()
                )
            })?;
            log.line(&format!(
                "dll already present: {} (permissions of its folder applied)",
                dest.display()
            ));
            return Ok(false);
        }
        // other contents under this name: it is replaced below
        Ok(_) => {}
        Err(e) if secure::is_oplock_busy(&e) => return Err(file_error("read", dest, &e)),
        // not there, or not readable: the write below says what is the matter
        Err(_) => {}
    }
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir).map_err(|e| werr("create install dir", e))?;
    }
    // a name that is never registered, then a rename: a failed copy leaves no half DLL under the real name
    let tmp = dest.with_extension("dll.tmp");
    let result = (|| -> Res<()> {
        // what an earlier run left behind (the file is made new, so that nothing of it stays in the copy)
        delete_file(&tmp).map_err(|e| file_error("remove the old", &tmp, &e))?;
        let sd = SecurityDescriptor::from_aces(&[
            Ace {
                sid: p.system.clone(),
                mask: FULL,
                flags: 0,
            },
            Ace {
                sid: p.admins.clone(),
                mask: FULL,
                flags: 0,
            },
        ])?;
        // SAFETY: `sd` lives until the end of the call.
        unsafe { secure::write_new_file_with_sd(&tmp, bytes, Some(sd.as_ptr())) }
            .map_err(|e| file_error("write", &tmp, &e))?;
        between(&tmp);
        // one step: `dest` is the old file or the new one, never none
        secure::rename_replacing_no_wait(&tmp, dest).map_err(|e| rename_error(&tmp, dest, &e))
    })();
    if result.is_err() {
        let _ = delete_file(&tmp);
    }
    result?;
    if let Err(e) = inherit(dest) {
        let _ = delete_file(dest);
        return Err(format!(
            "the copy {} could not be given the permissions of its folder, and was deleted again: {e}",
            dest.display()
        ));
    }
    log.line(&format!("dll copied: {}", dest.display()));
    Ok(true)
}

/// Undo a failed registration or camera step. Returns the text to append to the error.
///
/// The record of moved-aside data folders is not part of this: `Machine::delete_registration` never touches it, so a
/// record that cannot be written (or read) is never a reason for an incomplete rollback.
fn rollback(
    before: &Option<Registration>,
    dest: &Path,
    copied_new: bool,
    m: &mut dyn Machine,
    log: &mut dyn Log,
) -> String {
    let mut complete = true;
    match before {
        Some(prev) => match m.write_registration(prev) {
            Ok(()) => log.line(&format!(
                "rolled back to the earlier registration ({})",
                prev.dll
            )),
            Err(e) => {
                complete = false;
                log.line(&format!("rollback of the registration failed: {e}"));
            }
        },
        None => {
            let left = m.delete_registration();
            if left.is_empty() {
                log.line("rolled back: registration removed");
            } else {
                complete = false;
                log.line(&format!("rollback left registry keys: {}", left.join(", ")));
            }
        }
    }
    // the DLL we copied goes, unless the registration we kept names it
    let in_use = before.as_ref().is_some_and(|b| Path::new(&b.dll) == dest);
    if copied_new && !in_use && complete {
        match delete_file(dest) {
            Ok(()) => log.line(&format!("rolled back: deleted {}", dest.display())),
            Err(e) => log.line(&format!("could not delete {}: {e}", dest.display())),
        }
    }
    if complete {
        " (nothing was changed: the earlier state is back)".into()
    } else {
        " (the rollback was incomplete: run Repair)".into()
    }
}

/// Retire the DLL versions other than `keep`. One that Frame Server still has loaded cannot be deleted, and Frame
/// Server also keeps serving new camera users from it (a loaded class is not looked up again in the registry), so
/// the update would not take effect until a reboot. Stopping the service fixes both: it starts again by itself the
/// next time an app opens a camera. Apps that have the camera open lose their stream and have to open it again.
///
/// Returns true when an older build is still loaded afterwards (the service could not be stopped, or a file waits
/// for a restart).
fn remove_old_dlls(dir: &Path, keep: &Path, m: &mut dyn Machine, log: &mut dyn Log) -> bool {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return false;
    };
    let mut locked = Vec::new();
    for e in rd.flatten() {
        let p = e.path();
        if is_ours(&p) && p != keep {
            if delete_file(&p).is_ok() {
                log.line(&format!("deleted {}", p.display()));
            } else {
                locked.push(p);
            }
        }
    }
    if locked.is_empty() {
        return false;
    }
    log.line("an older DLL is loaded by Frame Server; stopping the service so the new build loads");
    let stopped = m.stop_frame_server();
    log.line(&format!("FrameServer stopped: {stopped}"));
    let mut waiting = !stopped;
    for p in &locked {
        if delete_or_delay(p, m, log) != Removal::Deleted {
            waiting = true;
        }
    }
    waiting
}

/// What became of a file that was to be deleted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Removal {
    Deleted,
    /// Locked: it goes at the next restart of Windows.
    AtReboot,
    /// Locked and even that could not be scheduled.
    Left,
}

/// Delete `p`; a file that is locked is scheduled for deletion at the next reboot.
fn delete_or_delay(p: &Path, m: &mut dyn Machine, log: &mut dyn Log) -> Removal {
    match delete_file(p) {
        Ok(()) => {
            log.line(&format!("deleted {}", p.display()));
            Removal::Deleted
        }
        Err(_) => match m.delete_at_reboot(p) {
            Ok(()) => {
                log.line(&format!(
                    "locked, delete scheduled at reboot: {}",
                    p.display()
                ));
                Removal::AtReboot
            }
            Err(e) => {
                log.line(&format!("could not delete {}: {e}", p.display()));
                Removal::Left
            }
        },
    }
}

/// Remove the virtual camera, the HKLM keys, the DLLs and the data folder from this machine.
pub fn uninstall(log: &mut dyn Log) -> Res<UninstallOutcome> {
    uninstall_with(&Paths::system(), &mut RealMachine::new(), log)
}

/// [`uninstall`] against any machine and folders, in this order: the camera first (so nothing recreates what is
/// deleted behind it), then the registry, the DLLs and the data folder.
///
/// A camera that Windows would not remove stops everything: the keys and files stay, so the class the camera
/// names still exists and a later Repair or Remove can finish the job. Whatever else could not be removed is
/// logged and returned as an error after the other steps ran. The caller must have closed `frames.bin` in its own
/// process.
pub fn uninstall_with(
    paths: &Paths,
    m: &mut dyn Machine,
    log: &mut dyn Log,
) -> Res<UninstallOutcome> {
    uninstall_trusting(paths, &Principals::machine(None).trusted_owners, m, log)
}

/// [`uninstall_with`] for another set of owners that the helper's own leftovers in `%ProgramData%` may have (the
/// real ones are SYSTEM and Administrators; tests, which run as a standard user, name themselves).
pub fn uninstall_trusting(
    paths: &Paths,
    trusted: &[String],
    m: &mut dyn Machine,
    log: &mut dyn Log,
) -> Res<UninstallOutcome> {
    m.start()?;
    match m.remove_camera() {
        Ok(()) => log.line("camera removed"),
        Err(e) => {
            log.line(&format!("Remove failed: {e}"));
            // Remove also fails on a camera that is not registered (a half installed state, or a second remove),
            // and then everything else still has to be cleaned up: only a camera that is still listed stops it.
            if m.camera_listed() {
                return Err(format!(
                    "Windows did not remove the virtual camera ({e}). Nothing else was removed; try again"
                ));
            }
            log.line("Windows does not list the camera: it is gone, going on");
        }
    }

    let mut left: Vec<String> = Vec::new();
    let mut pending = false;

    // registry
    let failed = m.delete_registration();
    log.line(&format!(
        "registry keys deleted{}",
        if failed.is_empty() {
            String::new()
        } else {
            format!(", except {}", failed.join(", "))
        }
    ));
    left.extend(failed);

    // DLLs: delete; one that Frame Server holds: stop the service and retry, else delete at the next restart
    let dlls: Vec<PathBuf> = std::fs::read_dir(&paths.install)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| is_ours(p))
                .collect()
        })
        .unwrap_or_default();
    let mut locked = Vec::new();
    for p in &dlls {
        if delete_file(p).is_ok() {
            log.line(&format!("deleted {}", p.display()));
        } else {
            locked.push(p.clone());
        }
    }
    let mut stopped = false;
    if !locked.is_empty() {
        log.line("some DLLs are loaded by Frame Server; stopping the service");
        stopped = m.stop_frame_server();
        for p in &locked {
            match delete_or_delay(p, m, log) {
                Removal::Deleted => {}
                Removal::AtReboot => pending = true,
                Removal::Left => left.push(p.display().to_string()),
            }
        }
    }
    let _ = std::fs::remove_dir(&paths.install);
    if let Some(parent) = paths.install.parent() {
        let _ = std::fs::remove_dir(parent);
    }

    // the data folder, last: the log of this run lives in it
    log.release_folder();
    let mut gone = datadir::remove_all_reporting(paths);
    if gone.is_err() && !stopped {
        log.line("the data folder is in use; stopping Frame Server");
        m.stop_frame_server();
        gone = datadir::remove_all_reporting(paths);
    }
    if let Err(what) = gone {
        log.folder_ready();
        // what stayed behind, as it is: a file another program holds a lock on is said so
        let why = format!("{} ({what})", paths.data.display());
        log.line(&format!("could not remove {why}"));
        left.push(why);
    }
    // a data folder that an install moved out of the way lives next to `BetterCam`, not in it
    let swept = sweep_recorded(&mut *m, log, |recorded, log| {
        datadir::sweep_data_asides(paths, trusted, recorded, log)
    });
    if swept.left > 0 {
        left.push(format!(
            "an old copy of the data folder in {} (in use)",
            paths.program_data.display()
        ));
    }

    if left.is_empty() {
        log.line("unregistered");
        Ok(if pending {
            log.line("some files are deleted at the next restart of Windows");
            UninstallOutcome::PendingReboot
        } else {
            UninstallOutcome::Done
        })
    } else {
        log.line(&format!("left behind: {}", left.join("; ")));
        Err(format!(
            "The virtual camera was removed from Windows, but this was left behind: {}",
            left.join("; ")
        ))
    }
}

#[cfg(test)]
mod tests;
