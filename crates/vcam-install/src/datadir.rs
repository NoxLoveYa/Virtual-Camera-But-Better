//! The data folder `%ProgramData%\BetterCam` as the elevated helper prepares and removes it.
//!
//! The helper must never act on something a standard user planted. Three rules:
//!
//! * every object is opened without following links (`secure::open_with`), then checked on the *handle*: not a
//!   reparse point, the right kind, one hard link, and its final path is exactly `<ProgramData>\BetterCam[\...]`
//!   (so a junction anywhere in the chain fails the comparison). Permissions are set and read back on that handle,
//!   never by path;
//! * the folder handle is held without delete sharing while the helper works in it, so it cannot be swapped;
//! * an object that is not what it must be (a link, the wrong kind, extra hard links, a frame file that is read-only,
//!   sparse or compressed, an owner who is not SYSTEM or Administrators, for `logs` also any other permissions) is
//!   moved aside and made again; what is moved aside is deleted without ever recursing through a link.
//!
//! Nothing is ever left with the permissions it would inherit: a new object is created with its final security
//! descriptor in the create call, and the order is the data folder first (its permissions are replaced before
//! anything below it is touched, which takes away the inheritable rights an older version gave the user), then
//! `logs`, `frames.bin` and `install.log`.
//!
//! The permissions themselves are in [`crate::acl`].

use std::os::windows::fs::MetadataExt;
use std::os::windows::io::OwnedHandle;
use std::path::{Path, PathBuf};
use std::time::Duration;

use vcam_protocol::secure::{self, DISP_CREATE_NEW, DISP_OPEN_EXISTING};
use windows::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TEMPORARY,
    FILE_SHARE_MODE,
};

use crate::acl::{self, Object, Principals};
use crate::log::Log;
use crate::util::{werr, Paths, Res};

const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
/// `READ_CONTROL`: enough to read owner and DACL.
const READ_CONTROL: u32 = 0x0002_0000;

/// What is known about an object that already exists (facts of an opened handle).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Facts {
    pub reparse_point: bool,
    pub is_directory: bool,
    pub links: u32,
    /// Its final path is exactly where it should be (no link in the chain, local fixed disk).
    pub in_place: bool,
    pub owner_trusted: bool,
    /// Owner, protection and ACEs are exactly the expected ones.
    pub acl_as_expected: bool,
    /// A file that is read-only, sparse, compressed or encrypted: the DLL and the app refuse such a frame file (an
    /// older version gave the user the right to set these). Folders have no such flags that matter.
    pub bad_attributes: bool,
}

/// `FILE_ATTRIBUTE_READONLY | FILE_ATTRIBUTE_SPARSE_FILE | FILE_ATTRIBUTE_COMPRESSED | FILE_ATTRIBUTE_ENCRYPTED`.
const BAD_FILE_ATTRIBUTES: u32 = 0x1 | 0x200 | 0x800 | 0x4000;

/// What to do with an object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Plan {
    /// It does not exist: make it.
    Create,
    /// Use it; its permissions are set again in place.
    Keep,
    /// Move it aside and make a new one; the text says why.
    Replace(String),
}

/// Decide from facts. `None` means the object does not exist.
pub fn decide(obj: Object, facts: Option<&Facts>) -> Plan {
    let Some(f) = facts else {
        return Plan::Create;
    };
    let wants_dir = !obj.is_file();
    if f.reparse_point {
        return Plan::Replace("it is a link or junction".into());
    }
    if f.is_directory != wants_dir {
        return Plan::Replace(if wants_dir {
            "it is not a folder".into()
        } else {
            "it is not a file".into()
        });
    }
    if !f.in_place {
        return Plan::Replace("it is not where its path says (a link in the way)".into());
    }
    if !wants_dir && f.links != 1 {
        return Plan::Replace(format!("it has {} hard links", f.links));
    }
    if !f.owner_trusted {
        return Plan::Replace("its owner is not SYSTEM or Administrators".into());
    }
    if !wants_dir && f.bad_attributes {
        return Plan::Replace("it is read-only, sparse, compressed or encrypted".into());
    }
    // `logs` can hold files with permissions of their own, so other permissions on it mean the folder was
    // tampered with or made by an older version: start again. The data folder and frames.bin have no such
    // children: their permissions are replaced as a whole, nothing of the old ones survives.
    if obj == Object::LogsDir && !f.acl_as_expected {
        return Plan::Replace("it has other permissions than expected".into());
    }
    Plan::Keep
}

fn path_of(paths: &Paths, obj: Object) -> PathBuf {
    match obj {
        Object::DataDir => paths.data.clone(),
        Object::LogsDir => paths.logs(),
        Object::FramesFile => paths.frames(),
        Object::InstallLog => paths.install_log(),
    }
}

/// Open `path` for reading its kind and permissions, not following a link. `None` when it does not exist.
fn inspect(paths: &Paths, obj: Object, p: &Principals) -> Res<Option<Facts>> {
    let path = path_of(paths, obj);
    let h = match secure::open_with(
        &path,
        READ_CONTROL | 0x80, /* FILE_READ_ATTRIBUTES */
        secure::SHARE_ALL,
        DISP_OPEN_EXISTING,
        true,
        0,
    ) {
        Ok(h) => h,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        // exists but cannot be looked at: it is not ours to keep
        Err(_) => {
            return Ok(Some(Facts {
                reparse_point: false,
                is_directory: !obj.is_file(),
                links: 1,
                in_place: true,
                owner_trusted: false,
                acl_as_expected: false,
                bad_attributes: false,
            }))
        }
    };
    let kind = secure::kind(secure::raw(&h)).map_err(|e| werr("inspect", e))?;
    let in_place = match secure::expected_path(&paths.program_data, &path) {
        Ok(expect) => secure::verify_place(secure::raw(&h), &expect).is_ok(),
        Err(_) => false,
    };
    let snap = acl::read(&h);
    let (owner_trusted, acl_as_expected) = match &snap {
        Ok(s) => (
            acl::owner_trusted(&s.owner, &p.trusted_owners),
            acl::check(s, &acl::expected(obj, p), &p.owner).is_ok(),
        ),
        Err(_) => (false, false),
    };
    Ok(Some(Facts {
        reparse_point: kind.reparse_point,
        is_directory: kind.directory,
        links: kind.links,
        in_place,
        owner_trusted,
        acl_as_expected,
        bad_attributes: kind.attributes & BAD_FILE_ATTRIBUTES != 0,
    }))
}

// ---------------------------------------------------------------------------------------------------------------
// Removing and moving, never through a link

fn attributes(p: &Path) -> std::io::Result<u32> {
    Ok(std::fs::symlink_metadata(p)?.file_attributes())
}

/// True when `p` is a link of any kind (symbolic link, junction, other reparse point). False when it is missing.
pub fn is_link(p: &Path) -> bool {
    attributes(p).is_ok_and(|a| a & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0)
}

/// Delete `p`. A link (symbolic link, junction, any reparse point) is removed itself and what it points to is left
/// alone; a folder is emptied without following links inside it (`remove_dir_all` opens every entry as a link
/// first); a read-only file loses the flag first. A missing path is not an error.
pub fn remove_no_follow(p: &Path) -> std::io::Result<()> {
    let attrs = match attributes(p) {
        Ok(a) => a,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let dir = attrs & FILE_ATTRIBUTE_DIRECTORY.0 != 0;
    if attrs & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
        // a junction or directory symlink is removed with RemoveDirectory, a file symlink with DeleteFile; neither
        // touches the target
        return if dir {
            std::fs::remove_dir(p)
        } else {
            std::fs::remove_file(p)
        };
    }
    if dir {
        std::fs::remove_dir_all(p)
    } else {
        remove_plain_file(p)
    }
}

fn remove_plain_file(p: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(p) {
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            if let Ok(md) = std::fs::symlink_metadata(p) {
                let mut perm = md.permissions();
                if perm.readonly() {
                    // Windows only: this clears FILE_ATTRIBUTE_READONLY (the lint is about Unix mode bits)
                    #[allow(clippy::permissions_set_readonly_false)]
                    perm.set_readonly(false);
                    let _ = std::fs::set_permissions(p, perm);
                    return std::fs::remove_file(p);
                }
            }
            Err(e)
        }
        other => other,
    }
}

/// Rename `p` to `<name>.old-<n>` next to it (a link is renamed itself, not what it points to). Retries a little:
/// a handle that someone holds without delete sharing, such as the DLL listing the log folder, lasts a moment.
pub fn move_aside(p: &Path) -> std::io::Result<PathBuf> {
    let name = p
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let parent = p.parent().unwrap_or(Path::new("."));
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let mut last = None;
    for i in 0..8u32 {
        let target = parent.join(format!("{name}.old-{stamp}-{i}"));
        if std::fs::symlink_metadata(&target).is_ok() {
            continue;
        }
        match std::fs::rename(p, &target) {
            Ok(()) => return Ok(target),
            Err(e) => {
                last = Some(e);
                std::thread::sleep(Duration::from_millis(150));
            }
        }
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("no free name for the old copy")))
}

/// True for a name that [`move_aside`] made.
pub fn is_aside_name(name: &str) -> bool {
    name.contains(".old-")
}

/// Move `path` aside (stopping the camera service once when something holds it), then delete the old copy.
fn replace_aside(
    path: &Path,
    why: &str,
    stop_service: &mut dyn FnMut() -> bool,
    log: &mut dyn Log,
) -> Res<()> {
    log.line(&format!("{}: {why}; moving it aside", path.display()));
    let aside = match move_aside(path) {
        Ok(a) => a,
        Err(first) => {
            log.line(&format!(
                "{} is in use ({first}); stopping the camera service",
                path.display()
            ));
            stop_service();
            move_aside(path).map_err(|e| {
                format!(
                    "cannot move {} out of the way: {e}. Close the apps that use the camera, or restart Windows, and try again",
                    path.display()
                )
            })?
        }
    };
    match remove_no_follow(&aside) {
        Ok(()) => log.line(&format!("old copy deleted ({})", aside.display())),
        Err(e) => log.line(&format!(
            "old copy kept as {} ({e}); it is deleted by the next install",
            aside.display()
        )),
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------------
// Preparing

/// The access the helper needs to an object: to set and read permissions, and to size a file.
fn access_for(obj: Object) -> u32 {
    GENERIC_READ | acl::SECURITY_ACCESS | if obj.is_file() { GENERIC_WRITE } else { 0 }
}

/// Folders are held without delete sharing: nobody can rename, delete or replace one while the helper works in it.
fn share_for(obj: Object) -> FILE_SHARE_MODE {
    if obj.is_file() {
        secure::SHARE_ALL
    } else {
        secure::SHARE_READ_WRITE
    }
}

/// The attribute the frame file is made with.
fn attributes_for(obj: Object) -> u32 {
    if obj == Object::FramesFile {
        FILE_ATTRIBUTE_TEMPORARY.0
    } else {
        0
    }
}

/// Verify that what was opened is the right kind at the right place.
fn verify_opened(paths: &Paths, obj: Object, h: &OwnedHandle) -> Res<()> {
    let path = path_of(paths, obj);
    let expect = secure::expected_path(&paths.program_data, &path)
        .map_err(|e| werr(&format!("{}", path.display()), e))?;
    let verdict = if obj.is_file() {
        secure::verify_file(secure::raw(h), Some(&expect))
    } else {
        secure::verify_dir(secure::raw(h), Some(&expect))
    };
    verdict.map_err(|e| werr(&format!("{} is not what it must be", path.display()), e))
}

/// Open the existing `obj` not following a link, and verify it.
fn open_checked(paths: &Paths, obj: Object) -> Res<OwnedHandle> {
    let path = path_of(paths, obj);
    let h = secure::open_with(
        &path,
        access_for(obj),
        share_for(obj),
        DISP_OPEN_EXISTING,
        true,
        attributes_for(obj),
    )
    .map_err(|e| werr(&format!("open {}", path.display()), e))?;
    verify_opened(paths, obj, &h)?;
    Ok(h)
}

/// Create the *new* `obj` with its final security descriptor in the create call (so it never exists with the
/// permissions it would inherit), open it, and verify it. Fails when anything is there already.
pub(crate) fn create_new(paths: &Paths, obj: Object, p: &Principals) -> Res<OwnedHandle> {
    let path = path_of(paths, obj);
    let sd = acl::SecurityDescriptor::for_object(obj, p)?;
    // SAFETY: `sd` lives until the end of this function and holds a valid descriptor.
    let h = unsafe {
        if obj.is_file() {
            secure::open_with_sd(
                &path,
                access_for(obj),
                share_for(obj),
                DISP_CREATE_NEW,
                true,
                attributes_for(obj),
                Some(sd.as_ptr()),
            )
        } else {
            secure::create_dir_with_sd(&path, access_for(obj), share_for(obj), sd.as_ptr())
        }
    }
    .map_err(|e| werr(&format!("create {}", path.display()), e))?;
    verify_opened(paths, obj, &h)?;
    Ok(h)
}

/// Make `obj` exist as the right kind of object and return its verified handle. A new object has its final
/// permissions already; one that was kept gets them from [`lock`].
fn ensure_exists(
    paths: &Paths,
    obj: Object,
    p: &Principals,
    stop_service: &mut dyn FnMut() -> bool,
    log: &mut dyn Log,
) -> Res<OwnedHandle> {
    let path = path_of(paths, obj);
    let facts = inspect(paths, obj, p)?;
    let plan = decide(obj, facts.as_ref());
    match plan {
        Plan::Keep => {
            log.line(&format!("{}: kept", path.display()));
            open_checked(paths, obj)
        }
        Plan::Create => {
            log.line(&format!("{}: created", path.display()));
            create_new(paths, obj, p)
        }
        Plan::Replace(why) => match replace_aside(&path, &why, stop_service, log) {
            Ok(()) => {
                log.line(&format!("{}: created", path.display()));
                create_new(paths, obj, p)
            }
            // A folder cannot be renamed while any file in it is open, even one opened with delete sharing, and
            // every camera app that loaded the DLL keeps its log open. `logs` only holds logs, so when it cannot
            // be moved it is emptied and reset in place (through the held handle, links removed as links). A
            // link or a file is never in that position: renaming those does not depend on what they contain.
            Err(e) if obj == Object::LogsDir && is_real_dir(&path) => {
                log.line(&format!("{e}; emptying and resetting it in place instead"));
                let h = open_checked(paths, obj)?;
                empty_in_place(&path, log);
                Ok(h)
            }
            Err(e) => Err(e),
        },
    }
}

/// A folder that is not a link of any kind.
fn is_real_dir(p: &Path) -> bool {
    attributes(p).is_ok_and(|a| {
        a & FILE_ATTRIBUTE_DIRECTORY.0 != 0 && a & FILE_ATTRIBUTE_REPARSE_POINT.0 == 0
    })
}

/// Delete what is inside `dir` (never following a link); what a process still has open stays until it closes
/// (delete pending: nobody can open it any more).
fn empty_in_place(dir: &Path, log: &mut dyn Log) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        match remove_no_follow(&e.path()) {
            Ok(()) => {}
            Err(err) => log.line(&format!("could not delete {}: {err}", e.path().display())),
        }
    }
}

/// Set owner and permissions on `h` and read them back.
fn lock(h: &OwnedHandle, obj: Object, p: &Principals, log: &mut dyn Log) -> Res<()> {
    acl::apply(h, obj, p)?;
    let snap = acl::read(h)?;
    acl::check(&snap, &acl::expected(obj, p), &p.owner).map_err(|e| {
        format!(
            "{} permissions did not read back as set: {e}",
            obj.describe()
        )
    })?;
    log.line(&format!(
        "{}: owner {}, {} ACEs, verified",
        obj.describe(),
        snap.owner,
        snap.aces.len()
    ));
    Ok(())
}

/// Delete everything in the data folder except `logs` and `frames.bin` (leftovers of old versions, copies moved
/// aside earlier, anything a user left). Failures are logged; the number left is returned.
fn sweep(paths: &Paths, log: &mut dyn Log) -> usize {
    let Ok(rd) = std::fs::read_dir(&paths.data) else {
        return 0;
    };
    let mut left = 0;
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.eq_ignore_ascii_case(vcam_protocol::LOG_DIR_NAME)
            || name.eq_ignore_ascii_case(vcam_protocol::FRAMES_FILE_NAME)
        {
            continue;
        }
        match remove_no_follow(&e.path()) {
            Ok(()) => log.line(&format!("removed unexpected {}", e.path().display())),
            Err(err) => {
                left += 1;
                log.line(&format!("could not remove {}: {err}", e.path().display()));
            }
        }
    }
    left
}

/// Delete the copies [`move_aside`] left next to `logs` and `frames.bin` that were still in use then, and the
/// moved-aside data folders next to `BetterCam` itself ([`sweep_data_asides`]).
pub fn sweep_asides(paths: &Paths, log: &mut dyn Log) {
    if let Ok(rd) = std::fs::read_dir(&paths.data) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if is_aside_name(&name) {
                match remove_no_follow(&e.path()) {
                    Ok(()) => log.line(&format!("deleted old copy {}", e.path().display())),
                    Err(err) => log.line(&format!("old copy {name} still in use ({err})")),
                }
            }
        }
    }
    sweep_data_asides(paths, log);
}

/// Delete the `BetterCam.old-*` folders in `%ProgramData%`: a data folder that [`move_aside`] moved out of the way
/// (a link, a folder with the wrong owner, ...) lives *next to* `BetterCam`, not inside it, and its deletion can
/// fail when the camera service still has a file of it open. Called by install and by uninstall. Each entry is
/// removed without following links (a junction with such a name is removed as a junction); other names are never
/// touched. Returns how many were left.
pub fn sweep_data_asides(paths: &Paths, log: &mut dyn Log) -> usize {
    let Ok(rd) = std::fs::read_dir(&paths.program_data) else {
        return 0;
    };
    let prefix = format!("{}.old-", vcam_protocol::DATA_DIR_NAME).to_lowercase();
    let mut left = 0;
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !name.to_lowercase().starts_with(&prefix) {
            continue;
        }
        match remove_no_follow(&e.path()) {
            Ok(()) => log.line(&format!("deleted old data folder {}", e.path().display())),
            Err(err) => {
                left += 1;
                log.line(&format!(
                    "old data folder {name} still in use ({err}); it is deleted by the next install or removal"
                ));
            }
        }
    }
    left
}

/// Delete the entries of `logs` that are not owned by one of `trusted`. Everything the helper and the service make
/// there is owned by SYSTEM, Administrators or LOCAL SERVICE; anything else was put there by somebody else in the
/// moment before `logs` was locked (or by an older version's permissions). Each entry is looked at without
/// following a link and deleted the same way. Returns how many were removed.
pub fn sweep_logs(paths: &Paths, trusted: &[String], log: &mut dyn Log) -> usize {
    let Ok(rd) = std::fs::read_dir(paths.logs()) else {
        return 0;
    };
    let mut removed = 0;
    for e in rd.flatten() {
        let path = e.path();
        let owner = secure::open_with(
            &path,
            READ_CONTROL | 0x80,
            secure::SHARE_ALL,
            DISP_OPEN_EXISTING,
            true,
            0,
        )
        .ok()
        .and_then(|h| acl::read(&h).ok())
        .map(|s| s.owner);
        let ok = owner
            .as_deref()
            .is_some_and(|o| acl::owner_trusted(o, trusted));
        if ok {
            continue;
        }
        match remove_no_follow(&path) {
            Ok(()) => {
                removed += 1;
                log.line(&format!(
                    "removed {} from the logs folder (owner {})",
                    path.display(),
                    owner.as_deref().unwrap_or("unknown")
                ));
            }
            Err(err) => log.line(&format!(
                "could not remove {} from the logs folder: {err}",
                path.display()
            )),
        }
    }
    removed
}

/// Create or repair `BetterCam`, `logs`, `frames.bin` and `install.log` with their permissions, or fail without
/// having used anything that is not what it must be. `stop_service` is called (at most once per object) when
/// something is held open by the camera service and has to be moved.
///
/// Order: the data folder first and locked at once (so what an older version let the user inherit is gone before
/// anything below is made), then each child, a new one created with its final permissions in the create call.
pub fn prepare(
    paths: &Paths,
    p: &Principals,
    stop_service: &mut dyn FnMut() -> bool,
    log: &mut dyn Log,
) -> Res<()> {
    // a log file of ours inside `logs` would keep the folder from being moved: close it, lines wait meanwhile
    log.release_folder();
    std::fs::create_dir_all(&paths.program_data)
        .map_err(|e| werr(&format!("create {}", paths.program_data.display()), e))?;
    // 1. the folder: held (no delete sharing) until the end, and locked right away: from here the user can neither
    // create nor rename anything in it, and what it hands down to new children is only what the helper wants
    let data = ensure_exists(paths, Object::DataDir, p, stop_service, log)?;
    lock(&data, Object::DataDir, p, log)?;
    // 2. the folder's children, each with a protected DACL of its own
    let logs = ensure_exists(paths, Object::LogsDir, p, stop_service, log)?;
    lock(&logs, Object::LogsDir, p, log)?;
    let mut trusted = p.trusted_owners.clone();
    trusted.extend([p.system.clone(), p.admins.clone(), p.local_service.clone()]);
    sweep_logs(paths, &trusted, log);
    let frames = ensure_exists(paths, Object::FramesFile, p, stop_service, log)?;
    {
        let file = std::fs::File::from(
            frames
                .try_clone()
                .map_err(|e| werr("duplicate handle", e))?,
        );
        if file.metadata().map(|m| m.len()).unwrap_or(0) < vcam_protocol::FILE_SIZE {
            file.set_len(vcam_protocol::FILE_SIZE)
                .map_err(|e| werr("size frames.bin", e))?;
        }
    }
    lock(&frames, Object::FramesFile, p, log)?;
    // the helper's own log: a DACL of its own, so the service's inheritable modify right on `logs` stops short of it
    let install_log = ensure_exists(paths, Object::InstallLog, p, stop_service, log)?;
    lock(&install_log, Object::InstallLog, p, log)?;
    let left = sweep(paths, log);
    if left > 0 {
        log.line(&format!("{left} unexpected item(s) could not be removed; they are inert (no access for the user)"));
    }
    drop((install_log, frames, logs, data));
    Ok(())
}

/// Delete the data folder: the folder is emptied and removed without following links; retried a few times because a
/// stream that just ended may still hold `frames.bin`. Returns whether it is gone.
pub fn remove_all(paths: &Paths) -> bool {
    for attempt in 0..6 {
        if std::fs::symlink_metadata(&paths.data).is_err() {
            return true;
        }
        if remove_no_follow(&paths.data).is_ok() {
            return true;
        }
        if attempt < 5 {
            std::thread::sleep(Duration::from_millis(300));
        }
    }
    std::fs::symlink_metadata(&paths.data).is_err()
}

#[cfg(test)]
mod tests;
