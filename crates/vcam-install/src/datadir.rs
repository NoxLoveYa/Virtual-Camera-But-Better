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
//!   moved aside and made again; what is moved aside is deleted without ever recursing through a link;
//! * nothing waits for another program: a standard user can read `install.log` and `frames.bin` and can create
//!   entries in `%ProgramData%`, and a program of theirs that holds an opportunistic lock on a file makes the plain
//!   delete, rename and open of it wait. Deletes and renames go through handles ([`secure::remove_tree`],
//!   [`secure::rename_no_wait`]); a file that is locked is left, named, and tried again at the next install. The
//!   helper acts only on entries it made: a `BetterCam.old-*` entry whose owner is not SYSTEM or Administrators is
//!   left exactly as it is.
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
use windows::Win32::Foundation::HANDLE;
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
    /// A file that is read-only or encrypted: the DLL and the app refuse such a frame file, and nothing but a new
    /// file makes it right. Folders have no such flags that matter.
    pub bad_attributes: bool,
    /// A file that is sparse or compressed (the DLL and the app refuse it too). A handle with only `FILE_WRITE_DATA`
    /// can set both, so the mask of the frame file does not prevent it; the flags are cleared in place.
    pub sparse_or_compressed: bool,
    /// A file with a data stream besides its own contents (`frames.bin:name`). A handle with only `FILE_WRITE_DATA`
    /// can create one, and what is in it would survive a Repair that kept the file.
    pub extra_streams: bool,
}

/// `FILE_ATTRIBUTE_READONLY | FILE_ATTRIBUTE_ENCRYPTED`.
const BAD_FILE_ATTRIBUTES: u32 = 0x1 | 0x4000;

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
        return Plan::Replace("it is read-only or encrypted".into());
    }
    if !wants_dir && f.extra_streams {
        return Plan::Replace("it has an extra data stream".into());
    }
    // `f.sparse_or_compressed` is not a reason to replace: the flags are cleared in place (see `ensure_exists`)
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
                sparse_or_compressed: false,
                extra_streams: false,
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
    // a stream that cannot be listed is as bad as one that is there: the file is replaced
    let extra_streams =
        obj.is_file() && secure::extra_streams(secure::raw(&h)).map_or(true, |s| !s.is_empty());
    Ok(Some(Facts {
        reparse_point: kind.reparse_point,
        is_directory: kind.directory,
        links: kind.links,
        in_place,
        owner_trusted,
        acl_as_expected,
        bad_attributes: kind.attributes & BAD_FILE_ATTRIBUTES != 0,
        sparse_or_compressed: kind.attributes
            & (secure::ATTRIBUTE_SPARSE | secure::ATTRIBUTE_COMPRESSED)
            != 0,
        extra_streams,
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

/// Delete `p`, through handles ([`secure::remove_tree`]): a link (symbolic link, junction, any reparse point) is
/// removed itself and what it points to is left alone; a folder is emptied without following links in it; a
/// read-only file goes too. A missing path is not an error.
///
/// Nothing here waits for another program: a file that a program holds an opportunistic lock on (a user may take one
/// on any file they may read) is not deleted, the rest is, and the error names the file (see
/// [`secure::is_oplock_busy`]).
pub fn remove_no_follow(p: &Path) -> std::io::Result<()> {
    secure::remove_tree(p).map_err(std::io::Error::from)
}

/// Rename `p` to `<name>.old-<n>` next to it (a link is renamed itself, not what it points to). Retries a little:
/// a handle that someone holds without delete sharing, such as the DLL listing the log folder, lasts a moment. The
/// rename goes through a handle opened like every other open here, so it never waits for a program that holds an
/// opportunistic lock on the file ([`secure::rename_no_wait`]).
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
        match secure::rename_no_wait(p, &target) {
            Ok(()) => return Ok(target),
            // gone: nothing to move, and waiting does not bring it back. Locked by another program: it was waited
            // for already (up to the bound of the rename), and asking again does not make the holder answer
            Err(e) if e.kind() == std::io::ErrorKind::NotFound || secure::is_oplock_busy(&e) => {
                return Err(e)
            }
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
        // a program of the user holds a lock on it: stopping the camera service does not help against that
        Err(first) if secure::is_oplock_busy(&first) => return Err(locked_message(path)),
        Err(first) => {
            log.line(&format!(
                "{} is in use ({first}); stopping the camera service",
                path.display()
            ));
            stop_service();
            move_aside(path).map_err(|e| {
                if secure::is_oplock_busy(&e) {
                    locked_message(path)
                } else {
                    format!(
                        "cannot move {} out of the way: {e}. Close the apps that use the camera, or restart Windows, and try again",
                        path.display()
                    )
                }
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

/// The sentence for a file that another program holds an opportunistic lock on: said as it is, and not waited for.
fn locked_message(path: &Path) -> String {
    format!(
        "{} (or a file in it) is locked by another program (it holds an opportunistic lock); not waiting for it. Close the program that has it open and run Repair again",
        path.display()
    )
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

/// How long an open that met another program's lock is tried again (a program that holds such a lock and is asked to
/// give it up, an anti-virus scan for one, answers within moments; one that never answers is reported after this).
const LOCK_PATIENCE: [u64; 10] = [50, 100, 150, 150, 200, 200, 250, 250, 250, 250];

/// Open the existing `obj` not following a link, and verify it. An open that meets another program's opportunistic
/// lock does not wait for it: it is asked again a few times (about two seconds in all), then it fails with a
/// sentence that says so.
fn open_checked(paths: &Paths, obj: Object) -> Res<OwnedHandle> {
    let path = path_of(paths, obj);
    let mut patience = LOCK_PATIENCE.iter();
    let h = loop {
        match secure::open_with(
            &path,
            access_for(obj),
            share_for(obj),
            DISP_OPEN_EXISTING,
            true,
            attributes_for(obj),
        ) {
            Ok(h) => break h,
            Err(e) if secure::is_oplock_busy(&e) => match patience.next() {
                Some(ms) => std::thread::sleep(Duration::from_millis(*ms)),
                None => return Err(locked_message(&path)),
            },
            Err(e) => return Err(werr(&format!("open {}", path.display()), e)),
        }
    };
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
            let h = open_checked(paths, obj)?;
            // the one thing the frame file's mask lets a user do to it that is put right in place, not by a new file
            if obj == Object::FramesFile {
                if let Err(why) = plain_again(&h, &path, log) {
                    drop(h);
                    return replace(paths, obj, p, &why, stop_service, log);
                }
            }
            log.line(&format!("{}: kept", path.display()));
            Ok(h)
        }
        Plan::Create => {
            log.line(&format!("{}: created", path.display()));
            create_new(paths, obj, p)
        }
        Plan::Replace(why) => replace(paths, obj, p, &why, stop_service, log),
    }
}

/// Move `obj` aside and make it again.
fn replace(
    paths: &Paths,
    obj: Object,
    p: &Principals,
    why: &str,
    stop_service: &mut dyn FnMut() -> bool,
    log: &mut dyn Log,
) -> Res<OwnedHandle> {
    let path = path_of(paths, obj);
    match replace_aside(&path, why, stop_service, log) {
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
            empty_in_place(&h, &path, log);
            Ok(h)
        }
        Err(e) => Err(e),
    }
}

/// Clear the sparse and compressed state of the frame file through its handle (a handle with only `FILE_WRITE_DATA`
/// can set both; the contents are not touched and the file stays the same file). `Err` says why that did not work
/// and the file is to be replaced.
fn plain_again(h: &OwnedHandle, path: &Path, log: &mut dyn Log) -> Result<(), String> {
    let attrs = secure::kind(secure::raw(h))
        .map_err(|e| format!("its attributes cannot be read ({e})"))?
        .attributes;
    if attrs & (secure::ATTRIBUTE_SPARSE | secure::ATTRIBUTE_COMPRESSED) == 0 {
        return Ok(());
    }
    log.line(&format!(
        "{}: it is sparse or compressed; clearing that in place",
        path.display()
    ));
    secure::clear_sparse_and_compression(secure::raw(h))
        .map(|_| ())
        .map_err(|e| format!("it is sparse or compressed and that could not be cleared ({e})"))
}

/// A folder that is not a link of any kind.
fn is_real_dir(p: &Path) -> bool {
    attributes(p).is_ok_and(|a| {
        a & FILE_ATTRIBUTE_DIRECTORY.0 != 0 && a & FILE_ATTRIBUTE_REPARSE_POINT.0 == 0
    })
}

/// Delete what is inside the folder held as `dir` (never following a link, never waiting for a program that holds
/// a lock on a file); what a process still has open stays until it closes (delete pending: nobody can open it any
/// more), and what cannot go is logged.
fn empty_in_place(dir: &OwnedHandle, shown: &Path, log: &mut dyn Log) {
    if let Err(e) = secure::remove_children(dir, shown) {
        log.line(&format!(
            "could not delete everything in {}: {e}",
            shown.display()
        ));
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
/// moved-aside data folders next to `BetterCam` itself ([`sweep_data_asides`]; only those owned by one of `trusted`).
pub fn sweep_asides(paths: &Paths, trusted: &[String], log: &mut dyn Log) {
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
    sweep_data_asides(paths, trusted, log);
}

/// The owner of `path` (as SID text), read through a handle opened as the entry itself: no link is followed, and
/// nothing is asked of it that would break another program's opportunistic lock (attributes and permissions only).
/// `None` when the entry cannot be opened or its owner cannot be read (for example a folder that denies even that).
fn owner_of(path: &Path) -> Option<String> {
    secure::open_with(
        path,
        READ_CONTROL | 0x80, /* FILE_READ_ATTRIBUTES */
        secure::SHARE_ALL,
        DISP_OPEN_EXISTING,
        true,
        0,
    )
    .ok()
    .and_then(|h| acl::read(&h).ok())
    .map(|s| s.owner)
}

/// Delete the `BetterCam.old-*` folders in `%ProgramData%`: a data folder that [`move_aside`] moved out of the way
/// (a link, a folder with the wrong owner, ...) lives *next to* `BetterCam`, not inside it, and its deletion can
/// fail when the camera service still has a file of it open. Called by install and by uninstall.
///
/// Standard users may create entries in `%ProgramData%`, under any name, so a name is not enough to act on: an entry
/// is deleted only when its owner is one of `trusted` (SYSTEM, Administrators: what the helper moved aside is owned
/// by them, and a standard user cannot make an entry with such an owner). The owner is read through a handle that
/// does not follow a link and does not touch any lock, then checked again on the handle the delete goes through.
/// Anything else is left exactly as it is, and never counts as a failure: it is not the helper's.
///
/// The delete never waits for another program (see [`remove_no_follow`]): an entry of the helper's that a program
/// still has open or locked stays until the next install or removal. Other names are never touched. Returns how many
/// of the helper's entries were left.
pub fn sweep_data_asides(paths: &Paths, trusted: &[String], log: &mut dyn Log) -> usize {
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
        let path = e.path();
        match owner_of(&path) {
            Some(o) if acl::owner_trusted(&o, trusted) => {}
            other => {
                log.line(&format!(
                    "{name} left alone: {}",
                    match other {
                        Some(o) => format!("its owner is {o}, not SYSTEM or Administrators"),
                        None => "its owner cannot be read".to_string(),
                    }
                ));
                continue;
            }
        }
        // the check again, on the handle that is deleted through (the entry may have been swapped meanwhile)
        let swapped = std::cell::Cell::new(false);
        let check = |h: HANDLE| -> std::io::Result<()> {
            let snap = acl::read_handle(h).map_err(secure::refused)?;
            if acl::owner_trusted(&snap.owner, trusted) {
                Ok(())
            } else {
                swapped.set(true);
                Err(secure::refused(format!(
                    "its owner is {}, not SYSTEM or Administrators",
                    snap.owner
                )))
            }
        };
        let top = secure::Top {
            access: secure::ACCESS_OWNER_CHECK,
            check: &check,
        };
        match secure::remove_tree_checked(&path, &top) {
            Ok(()) => log.line(&format!("deleted old data folder {}", path.display())),
            Err(_) if swapped.get() => {
                log.line(&format!(
                    "{name} left alone: it was replaced by somebody else's"
                ));
            }
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
        let owner = owner_of(&path);
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

/// Delete the data folder: the folder is emptied and removed without following links and without waiting for a
/// program that holds a lock on a file in it; retried a few times (about 1.5 s in all) because a stream that just
/// ended may still hold `frames.bin`. `Err` says what stayed behind: the first entry that could not go (told apart
/// when another program holds a lock on it) and how many others.
pub fn remove_all_reporting(paths: &Paths) -> Result<(), String> {
    let mut last = String::new();
    for attempt in 0..6 {
        match secure::remove_tree(&paths.data) {
            Ok(()) => return Ok(()),
            Err(e) => last = e.to_string(),
        }
        if attempt < 5 {
            std::thread::sleep(Duration::from_millis(300));
        }
    }
    Err(last)
}

/// [`remove_all_reporting`] as a yes or no: whether the data folder is gone.
pub fn remove_all(paths: &Paths) -> bool {
    remove_all_reporting(paths).is_ok()
}

#[cfg(test)]
mod tests;
