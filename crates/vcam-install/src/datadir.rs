//! The data folder `%ProgramData%\BetterCam` as the elevated helper prepares and removes it.
//!
//! The helper must never act on something a standard user planted. Three rules:
//!
//! * every object is opened without following links (`secure::open_with`), then checked on the *handle*: not a
//!   reparse point, the right kind, one hard link, and its final path is exactly `<ProgramData>\BetterCam[\...]`
//!   (so a junction anywhere in the chain fails the comparison). Permissions are set and read back on that handle,
//!   never by path;
//! * the folder handle is held without delete sharing, and with a data right (`FILE_LIST_DIRECTORY`, see
//!   [`access_for`]: without one Windows does not weigh the sharing mode), while the helper works in it, so it cannot
//!   be renamed, deleted or swapped;
//! * an object that is not what it must be (a link, the wrong kind, extra hard links, a frame file that is read-only,
//!   sparse or compressed, an owner who is not SYSTEM or Administrators, for `logs` also any other permissions) is
//!   moved aside and made again; what is moved aside is deleted without ever recursing through a link;
//! * nothing waits for another program: a standard user can read `install.log` and `frames.bin` and can create
//!   entries in `%ProgramData%`, and a program of theirs that holds an opportunistic lock on a file makes the plain
//!   delete, rename and open of it wait. Deletes and renames go through handles ([`secure::remove_tree`],
//!   [`secure::rename_no_wait`]); a file that is locked is left, named, and tried again at the next install. The
//!   helper acts only on entries it made: a data folder that it moves aside is put on a record (in the registry,
//!   where only administrators write) **before** the rename, with its identity (volume and file id, read on the
//!   handle that is renamed), and only if it is neither a link nor owned by somebody but SYSTEM or Administrators.
//!   The record is dropped again when the rename does not happen or the old copy is deleted at once. The sweep deletes
//!   a recorded name only when name, identity, "not a link" and "owner trusted" all hold on the handle it deletes
//!   through. Other `BetterCam.old-*` entries are never looked at (see [`crate::aside`]).
//! * a frame file is a file of exactly [`vcam_protocol::FILE_SIZE`] bytes, not sparse, not compressed, with one stream
//!   and one link, or it is replaced by a new file (a new file is never sparse). Nothing is ever cleared in place: the
//!   camera's user may write to the file, so may extend it at any moment, and clearing the sparse flag of a file that
//!   is a hundred gigabytes long at that moment would make Windows allocate all of it while the helper holds the
//!   machine-wide lock. The reason to replace such a file is that only a new file is cheap and safe to bring to a
//!   known state, **not** that the DLL or the app refuse it (they look at the length and the header, not at these
//!   attributes);
//! * a new frame file is made uncompressed whatever the folder around it is (NTFS gives a new child of a compressed
//!   folder, `%ProgramData%` or a drive that is compressed, the compressed attribute): it is created with
//!   `FILE_NO_COMPRESSION`, and the data folder's own compressed state is cleared so that it hands none on. A file
//!   that is sparse or compressed all the same after it was made is accepted with one line of the log (the helper
//!   replaces a file once per run and never fails on this alone); only the length is a failure.
//!
//! * the data folder is held with listing, attribute and permission rights, so another program that has it open
//!   without sharing writing cannot make the open fail; the wider rights that clearing its compressed state needs are
//!   asked for in a second handle, only when the folder is compressed (see [`clear_folder_compression`]);
//! * an object that is encrypted (EFS) is never decrypted and kept only when it cannot be moved: the camera service
//!   cannot read a file that is encrypted for somebody else's key. A data folder, `logs` or a frame file that is
//!   encrypted is moved aside and made again like any other that is not what it must be (the new one is plain when the
//!   folder around it is). Two things are kept: a `logs` folder that is in use and cannot be moved is emptied in
//!   place (with a line of the log that it is still encrypted), and a data folder that is encrypted while
//!   `%ProgramData%` is too (a new one would be encrypted as well, so replacing it would only lose its logs) is kept
//!   and refused. Only a data folder that is encrypted after this run is refused, with a sentence that says what an
//!   administrator can do (see [`encryption_refusal`]).
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
use std::time::{Duration, SystemTime};

use vcam_protocol::secure::{self, DISP_CREATE_NEW, DISP_OPEN_EXISTING};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TEMPORARY,
    FILE_SHARE_MODE,
};

use crate::acl::{self, Object, Principals};
use crate::aside::{self, AsideEntry, Ledger, NoLedger};
use crate::log::Log;
use crate::util::{werr, Paths, Res};

const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
/// `READ_CONTROL`: enough to read owner and DACL.
const READ_CONTROL: u32 = 0x0002_0000;
/// `FILE_READ_ATTRIBUTES`.
const FILE_READ_ATTRIBUTES: u32 = 0x80;
/// `FILE_LIST_DIRECTORY` (`FILE_READ_DATA` of a folder).
const FILE_LIST_DIRECTORY: u32 = 0x1;
/// `FILE_ADD_FILE` (`FILE_WRITE_DATA` of a folder).
const FILE_ADD_FILE: u32 = 0x2;
/// What clearing the compressed state of the data folder needs (`FSCTL_SET_COMPRESSION`: read and write data, for a
/// folder list and add-file). Asked for in a handle of its own, only when the folder is compressed: see
/// [`clear_folder_compression`].
const COMPRESSION_ACCESS: u32 = FILE_LIST_DIRECTORY | FILE_ADD_FILE | FILE_READ_ATTRIBUTES;
/// `FILE_ATTRIBUTE_ENCRYPTED`.
pub const ATTRIBUTE_ENCRYPTED: u32 = 0x4000;

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
    /// A file that is read-only or encrypted: the DLL and the app refuse such a frame file (they cannot open it for
    /// writing, or read it), and nothing but a new file makes it right. Folders have no such flags that matter.
    pub bad_attributes: bool,
    /// A file that is sparse or compressed (the DLL and the app can use it; it is replaced because only a new file is
    /// cheap and safe to bring to a known state). A handle with only `FILE_WRITE_DATA` can set both, so the mask of
    /// the frame file does not prevent it; such a file is replaced, never cleared in place (clearing the flag of a
    /// sparse file allocates its whole length).
    pub sparse_or_compressed: bool,
    /// A file with a data stream besides its own contents (`frames.bin:name`). A handle with only `FILE_WRITE_DATA`
    /// can create one, and what is in it would survive a Repair that kept the file.
    pub extra_streams: bool,
    /// A folder or file that is encrypted (EFS). The camera service cannot read what is encrypted for somebody else's
    /// key, and a folder hands the flag to everything made in it, so a folder is replaced as well (the helper never
    /// clears the flag in place: that works by path and needs the key of whoever encrypted it).
    pub encrypted: bool,
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
    // the data folder and `logs`: a folder that is encrypted hands that on to every file made in it (`install.log`
    // would be encrypted under the key of whoever runs the helper), and the camera service could not read them. The
    // new folder is plain when the folder around it is; when that is encrypted too, `prepare` says so
    if wants_dir && f.encrypted {
        return Plan::Replace("it is encrypted (Windows EFS)".into());
    }
    if !wants_dir && f.extra_streams {
        return Plan::Replace("it has an extra data stream".into());
    }
    // only the frame file: one that is sparse or compressed still works for the camera, but only a new file is cheap
    // and safe to bring to a known state (clearing a flag in place can allocate a file's whole length); nothing
    // reads the log for that
    if obj == Object::FramesFile && f.sparse_or_compressed {
        return Plan::Replace("it is sparse or compressed".into());
    }
    // `logs` can hold files with permissions of their own, so other permissions on it mean the folder was
    // tampered with or made by an older version: start again. The data folder and frames.bin have no such
    // children: their permissions are replaced as a whole, nothing of the old ones survives.
    if obj == Object::LogsDir && !f.acl_as_expected {
        return Plan::Replace("it has other permissions than expected".into());
    }
    Plan::Keep
}

/// [`decide`] for an object whose parent folder is known to be encrypted or not. A data folder that is encrypted in a
/// `%ProgramData%` that is encrypted too would be made again encrypted, and `prepare` refuses that: replacing it
/// would only move its logs aside and delete them for nothing. So when encryption is the *only* thing wrong with it,
/// it is kept (and refused after it is locked, see [`refuse_if_encrypted`]).
pub fn decide_below(obj: Object, facts: Option<&Facts>, parent_encrypted: bool) -> Plan {
    let plan = decide(obj, facts);
    if let (Plan::Replace(_), Some(f), true, Object::DataDir) =
        (&plan, facts, parent_encrypted, obj)
    {
        let plain = Facts {
            encrypted: false,
            ..f.clone()
        };
        if f.encrypted && decide(obj, Some(&plain)) == Plan::Keep {
            return Plan::Keep;
        }
    }
    plan
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
/// `seen` is given the attributes read on the handle and returns the ones the facts are made from (the identity
/// function for real use; a test says what an encrypted folder makes of a run without encrypting anything).
fn inspect(
    paths: &Paths,
    obj: Object,
    p: &Principals,
    seen: &dyn Fn(Object, u32) -> u32,
) -> Res<Option<Facts>> {
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
                encrypted: false,
            }))
        }
    };
    let kind = secure::kind(secure::raw(&h)).map_err(|e| werr("inspect", e))?;
    let attributes = seen(obj, kind.attributes);
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
        bad_attributes: attributes & BAD_FILE_ATTRIBUTES != 0,
        sparse_or_compressed: attributes
            & (secure::ATTRIBUTE_SPARSE | secure::ATTRIBUTE_COMPRESSED)
            != 0,
        extra_streams,
        encrypted: attributes & ATTRIBUTE_ENCRYPTED != 0,
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
    move_aside_with(p, None)
}

/// What [`move_aside_with`] tells its caller about the entry it is about to rename, at the one moment that matters:
/// after the entry is open and before it is renamed, on the handle that is renamed through.
trait AsideHooks {
    /// The access the handle needs besides what the rename needs (`READ_CONTROL` to read the owner).
    fn access(&self) -> u32;
    /// The entry behind `h` is about to be renamed to `name` (a name next to it).
    fn before(&mut self, name: &str, h: HANDLE);
    /// The rename to `name` did not happen (and is not still running).
    fn failed(&mut self, name: &str);
}

/// [`move_aside`] with the hooks of [`AsideHooks`] (none: a plain rename).
fn move_aside_with(p: &Path, mut hooks: Option<&mut dyn AsideHooks>) -> std::io::Result<PathBuf> {
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
        let target_name = format!("{name}.old-{stamp}-{i}");
        let target = parent.join(&target_name);
        if std::fs::symlink_metadata(&target).is_ok() {
            continue;
        }
        let result = match hooks.as_deref_mut() {
            Some(h) => {
                let access = h.access();
                secure::rename_no_wait_checked(p, &target, access, &mut |handle| {
                    h.before(&target_name, handle);
                    Ok(())
                })
            }
            None => secure::rename_no_wait(p, &target),
        };
        match result {
            Ok(()) => return Ok(target),
            Err(e) => {
                // a rename that could not be cancelled may still happen: what was written down for it stays
                if !secure::is_abandoned(&e) {
                    if let Some(h) = hooks.as_deref_mut() {
                        h.failed(&target_name);
                    }
                }
                // gone: nothing to move, and waiting does not bring it back. Locked by another program: it was
                // waited for already (up to the bound of the rename), and asking again does not make the holder
                // answer
                if e.kind() == std::io::ErrorKind::NotFound || secure::is_oplock_busy(&e) {
                    return Err(e);
                }
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

/// Where a data folder that is moved aside is written down (see [`crate::aside`]) and whose owners are trusted.
pub struct Recording<'a> {
    pub ledger: &'a mut dyn Ledger,
    pub trusted: &'a [String],
}

/// What the entry that was renamed is, judged on the handle that was renamed through.
enum Judged {
    /// Not a link, owner trusted, identity read: it can be recorded.
    Recordable(secure::FileIdentity),
    Link,
    Untrusted(String),
    /// Something could not be read.
    Unknown(String),
}

fn judge(h: HANDLE, trusted: &[String]) -> Judged {
    let k = match secure::kind(h) {
        Ok(k) => k,
        Err(e) => return Judged::Unknown(format!("its kind cannot be read ({e})")),
    };
    if k.reparse_point {
        return Judged::Link;
    }
    match acl::read_handle(h) {
        Err(e) => return Judged::Unknown(format!("its owner cannot be read ({e})")),
        Ok(s) if !acl::owner_trusted(&s.owner, trusted) => return Judged::Untrusted(s.owner),
        Ok(_) => {}
    }
    match secure::file_identity(h) {
        Ok(id) => Judged::Recordable(id),
        Err(e) => Judged::Unknown(format!("its file id cannot be read ({e})")),
    }
}

/// The hooks that put a data folder on the record before it is renamed, and take it off when the rename fails.
struct Recorder<'a, 'b> {
    ledger: &'a mut dyn Ledger,
    trusted: &'a [String],
    log: &'b mut dyn Log,
    judged: Option<Judged>,
    recorded: Option<String>,
}

impl AsideHooks for Recorder<'_, '_> {
    fn access(&self) -> u32 {
        READ_CONTROL
    }

    fn before(&mut self, name: &str, h: HANDLE) {
        self.recorded = None;
        let j = judge(h, self.trusted);
        if let Judged::Recordable(identity) = &j {
            let entry = AsideEntry {
                name: name.to_string(),
                identity: *identity,
            };
            match self.ledger.add(&entry) {
                Ok(()) => self.recorded = Some(name.to_string()),
                Err(e) => self.log.line(&format!(
                    "could not put {name} on record ({e}); it stays where it is if it cannot be deleted"
                )),
            }
        }
        self.judged = Some(j);
    }

    fn failed(&mut self, name: &str) {
        if self.recorded.as_deref() == Some(name) {
            self.recorded = None;
            if let Err(e) = self.ledger.remove(name) {
                self.log.line(&format!(
                    "could not take {name} off the record again ({e}); the sweep drops it as gone"
                ));
            }
        }
    }
}

/// An entry that was moved aside, and what became of its record.
struct Moved {
    to: PathBuf,
    /// The name is on the record (written before the rename).
    recorded: Option<String>,
    /// Why it is not, when it is not.
    not_recorded: String,
}

/// [`move_aside`] for an entry that may have to be recorded (`rec`): the facts are read on the handle that is
/// renamed, and the record is written before the rename.
fn move_aside_recorded(
    path: &Path,
    rec: Option<&mut Recording<'_>>,
    log: &mut dyn Log,
) -> std::io::Result<Moved> {
    let Some(rec) = rec else {
        return move_aside(path).map(|to| Moved {
            to,
            recorded: None,
            not_recorded: String::new(),
        });
    };
    let mut r = Recorder {
        ledger: &mut *rec.ledger,
        trusted: rec.trusted,
        log,
        judged: None,
        recorded: None,
    };
    let to = move_aside_with(path, Some(&mut r))?;
    let not_recorded = match &r.judged {
        _ if r.recorded.is_some() => String::new(),
        Some(Judged::Link) => "it is a link".into(),
        Some(Judged::Untrusted(owner)) => {
            format!("its owner is {owner}, not SYSTEM or Administrators")
        }
        Some(Judged::Unknown(why)) => why.clone(),
        Some(Judged::Recordable(_)) => "the record could not be written".into(),
        None => String::new(),
    };
    Ok(Moved {
        to,
        recorded: r.recorded.take(),
        not_recorded,
    })
}

/// Move `path` aside (stopping the camera service once when something holds it), then delete the old copy. A data
/// folder (`rec`) is put on the record before it is renamed; the record is dropped again when the old copy is gone.
/// A copy that could not be deleted stays on the record; one that is a link or has another owner is not recorded,
/// and is left where it is with one line of the log.
fn replace_aside(
    path: &Path,
    why: &str,
    stop_service: &mut dyn FnMut() -> bool,
    log: &mut dyn Log,
    mut rec: Option<&mut Recording<'_>>,
) -> Res<()> {
    log.line(&format!("{}: {why}; moving it aside", path.display()));
    let is_data_folder = rec.is_some();
    let moved = match move_aside_recorded(path, rec.as_deref_mut(), log) {
        Ok(a) => a,
        // a program of the user holds a lock on it: stopping the camera service does not help against that
        Err(first) if secure::is_oplock_busy(&first) => return Err(locked_message(path)),
        Err(first) => {
            log.line(&format!(
                "{} is in use ({first}); stopping the camera service",
                path.display()
            ));
            stop_service();
            move_aside_recorded(path, rec.as_deref_mut(), log).map_err(|e| {
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
    match remove_no_follow(&moved.to) {
        Ok(()) => {
            log.line(&format!("old copy deleted ({})", moved.to.display()));
            if let (Some(name), Some(rec)) = (&moved.recorded, rec) {
                if let Err(e) = rec.ledger.remove(name) {
                    log.line(&format!(
                        "could not take {name} off the record ({e}); the sweep drops it as gone"
                    ));
                }
            }
        }
        Err(e) if moved.recorded.is_some() => {
            log.line(&format!(
                "old copy kept as {} ({e}); it is on record, and the next install or removal deletes it",
                moved.to.display()
            ));
        }
        Err(e) if !is_data_folder => {
            // inside the data folder: the next install deletes every old copy there by name
            log.line(&format!(
                "old copy kept as {} ({e}); it is deleted by the next install",
                moved.to.display()
            ));
        }
        Err(e) => {
            let reason = if moved.not_recorded.is_empty() {
                String::new()
            } else {
                format!(": {}, so it is not put on record", moved.not_recorded)
            };
            log.line(&format!(
                "old copy kept as {} ({e}); it is left where it is{reason}",
                moved.to.display()
            ));
        }
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
///
/// The data folder is held with the least that keeps it held: its attributes (the kind, the final path, the
/// compressed and encrypted flags), its owner and DACL (to read and set them), and **listing** it
/// (`FILE_LIST_DIRECTORY`, the one data right that asks nothing of a program that has it open besides reading).
///
/// **The share-mode trap.** Windows weighs a handle's sharing mode against other opens only when the handle has a
/// data right: read data, write data, append, execute or `DELETE`. A handle with only attribute, `READ_CONTROL`,
/// `WRITE_DAC` and `WRITE_OWNER` rights has none, so leaving out `FILE_SHARE_DELETE` ([`share_for`]) would stop
/// nothing: the folder could be renamed or deleted (and one of the user's own put in its place) between
/// [`open_checked`] and [`lock`]. `FILE_LIST_DIRECTORY` is that right. It is the least that is asked, so the open
/// still succeeds while a program that did not share writing has the folder open (an indexer, a backup or
/// anti-virus tool with read access and `FILE_SHARE_READ`): no write right is asked, and only a write right would
/// conflict with that. Clearing the folder's compressed state is the one thing that needs more (read and write
/// data); it opens a handle of its own for that, and only when the folder is compressed
/// ([`clear_folder_compression`]).
fn access_for(obj: Object) -> u32 {
    if obj == Object::DataDir {
        return FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | acl::SECURITY_ACCESS;
    }
    GENERIC_READ | acl::SECURITY_ACCESS | if obj.is_file() { GENERIC_WRITE } else { 0 }
}

/// Folders are held without delete sharing: nobody can rename, delete or replace one while the helper works in it.
/// That holds only because [`access_for`] asks for a data right on every folder: Windows weighs a handle's sharing
/// mode against other opens only when the handle has one (read, write, append, execute or `DELETE`).
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

/// The create option the frame file is made with: no compression, whatever its folder says (see [`create_new`]).
fn create_options_for(obj: Object) -> u32 {
    if obj == Object::FramesFile {
        secure::CREATE_NO_COMPRESSION
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
            Err(e) if obj == Object::DataDir && is_sharing_violation(&e) => {
                return Err(not_shared_message(&path, &e))
            }
            Err(e) => return Err(werr(&format!("open {}", path.display()), e)),
        }
    };
    verify_opened(paths, obj, &h)?;
    Ok(h)
}

/// `ERROR_SHARING_VIOLATION`: another program has the object open and did not share it.
fn is_sharing_violation(e: &std::io::Error) -> bool {
    e.raw_os_error() == Some(32)
}

/// The sentence for a data folder that another program has open without sharing it. The standard user can list the
/// folder, so a program of theirs can hold it with share mode 0, and then even a listing right is refused. The raw
/// code stays in the sentence (it is what a person searches for when this is not the whole story).
fn not_shared_message(path: &Path, e: &std::io::Error) -> String {
    format!(
        "Another program has {} open without sharing it (os error {}). Close it or restart Windows, then run Repair again.",
        path.display(),
        e.raw_os_error().unwrap_or(32)
    )
}

/// Create the *new* `obj` with its final security descriptor in the create call (so it never exists with the
/// permissions it would inherit), open it, and verify it. Fails when anything is there already. The frame file is
/// created with `FILE_NO_COMPRESSION`: a new child of a compressed folder (`%ProgramData%` or the drive may be
/// compressed) gets the compressed attribute otherwise.
pub(crate) fn create_new(paths: &Paths, obj: Object, p: &Principals) -> Res<OwnedHandle> {
    let path = path_of(paths, obj);
    let sd = acl::SecurityDescriptor::for_object(obj, p)?;
    // SAFETY: `sd` lives until the end of this function and holds a valid descriptor.
    let h = unsafe {
        if obj.is_file() {
            secure::open_with_sd_options(
                &path,
                access_for(obj),
                share_for(obj),
                DISP_CREATE_NEW,
                true,
                attributes_for(obj),
                Some(sd.as_ptr()),
                create_options_for(obj),
            )
        } else {
            secure::create_dir_with_sd(&path, access_for(obj), share_for(obj), sd.as_ptr())
        }
    }
    .map_err(|e| werr(&format!("create {}", path.display()), e))?;
    verify_opened(paths, obj, &h)?;
    Ok(h)
}

/// What [`ensure_exists`] knows of the world around an object: whether the folder around it is encrypted
/// ([`decide_below`]), and `seen`, which maps the attributes read on its handle to the ones its facts are made from
/// (see [`inspect`]).
#[derive(Clone, Copy)]
struct Below<'a> {
    parent_encrypted: bool,
    seen: &'a dyn Fn(Object, u32) -> u32,
}

/// The attributes as they were read.
fn as_read(_: Object, attributes: u32) -> u32 {
    attributes
}

/// Make `obj` exist as the right kind of object and return its verified handle. A new object has its final
/// permissions already; one that was kept gets them from [`lock`].
///
/// A data folder that is moved aside is put on the record through `ledger` before the rename, and taken off again
/// when the old copy is deleted at once or the rename does not happen (see [`replace_aside`]).
fn ensure_exists(
    paths: &Paths,
    obj: Object,
    p: &Principals,
    stop_service: &mut dyn FnMut() -> bool,
    log: &mut dyn Log,
    ledger: &mut dyn Ledger,
    below: Below,
) -> Res<OwnedHandle> {
    let path = path_of(paths, obj);
    let facts = inspect(paths, obj, p, below.seen)?;
    let plan = decide_below(obj, facts.as_ref(), below.parent_encrypted);
    match plan {
        Plan::Keep => {
            let h = open_checked(paths, obj)?;
            // looked at again on the handle that is kept (the camera's user can change the file at any moment)
            if obj == Object::FramesFile {
                if let Err(why) = frame_file_in_shape(&h) {
                    drop(h);
                    return replace(paths, obj, p, &why, stop_service, log, ledger);
                }
            }
            log.line(&format!("{}: kept", path.display()));
            Ok(h)
        }
        Plan::Create => {
            log.line(&format!("{}: created", path.display()));
            create_new(paths, obj, p)
        }
        Plan::Replace(why) => replace(paths, obj, p, &why, stop_service, log, ledger),
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
    ledger: &mut dyn Ledger,
) -> Res<OwnedHandle> {
    let path = path_of(paths, obj);
    // only a data folder lives next to `BetterCam`, in `%ProgramData%`, where a user can make entries; the others
    // are inside it, where nobody but the helper can put anything: they need no record
    let mut recording = Recording {
        ledger,
        trusted: &p.trusted_owners,
    };
    let rec = (obj == Object::DataDir).then_some(&mut recording);
    match replace_aside(&path, why, stop_service, log, rec) {
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
            // a folder that is replaced for being encrypted and is in use stays that way: what the helper makes in it
            // (`install.log`) is encrypted too, under the key of the account that approved this run, and the log line
            // says what that means (see [`still_encrypted_message`])
            if secure::kind(secure::raw(&h)).is_ok_and(|k| k.attributes & ATTRIBUTE_ENCRYPTED != 0)
            {
                log.line(&still_encrypted_message(&path));
            }
            Ok(h)
        }
        Err(e) => Err(e),
    }
}

/// The line of the log for a `logs` folder that stays encrypted because it could not be moved. What is made in it is
/// encrypted too: `install.log` is then encrypted for the account that approved this run (the elevated one), and the
/// app, when it runs as another account, cannot read it later.
fn still_encrypted_message(logs: &Path) -> String {
    format!(
        "{} is still encrypted because it is in use; install.log is encrypted for the account that approved this run, so the app may not be able to show later errors. Close the camera apps and run Repair again",
        logs.display()
    )
}

/// The length of the file behind `h`.
fn length_of(h: &OwnedHandle) -> std::io::Result<u64> {
    std::fs::File::from(h.try_clone()?)
        .metadata()
        .map(|m| m.len())
}

/// Say whether the kept frame file is what the camera accepts, or why it has to be replaced (`Err`). Nothing is
/// changed on the file: a file that is not right is replaced by a new one, never put right in place.
///
/// * Its length is exactly [`vcam_protocol::FILE_SIZE`]. The camera's user may write to the file, so may extend it
///   (to a hundred gigabytes, sparse, which costs nothing); a file of another length is never kept.
/// * It is neither sparse nor compressed (a handle with only `FILE_WRITE_DATA` can set both). Clearing the flag in
///   place is what the helper used to do, and it is the one thing that cannot be done safely: the length is
///   something the user can change at any moment, also between a look at it and the clearing, and clearing the sparse
///   flag of a file that is a hundred gigabytes long at that moment makes Windows allocate all of it, synchronously,
///   on the system drive, while the helper holds the machine-wide lock. A new file is never sparse, and is made at
///   the size it must have.
fn frame_file_in_shape(h: &OwnedHandle) -> Result<(), String> {
    let len = length_of(h).map_err(|e| format!("its length cannot be read ({e})"))?;
    if len != vcam_protocol::FILE_SIZE {
        return Err(format!(
            "its length is {len} bytes, not {}",
            vcam_protocol::FILE_SIZE
        ));
    }
    let attrs = secure::kind(secure::raw(h))
        .map_err(|e| format!("its attributes cannot be read ({e})"))?
        .attributes;
    if attrs & (secure::ATTRIBUTE_SPARSE | secure::ATTRIBUTE_COMPRESSED) != 0 {
        return Err("it is sparse or compressed".into());
    }
    Ok(())
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

/// A folder that carries the compressed attribute hands it to every file and folder made in it afterwards, and NTFS
/// gives it to a new folder made inside a compressed folder or on a compressed drive (`compact /c` on `%ProgramData%`,
/// "Compress this drive"). The data folder holds no data itself, so clearing its state through the handle that is held
/// allocates and rewrites nothing. Nothing is done for a folder that is not compressed (no call at all); a folder that
/// cannot be cleared (a volume without compression) is a line of the log and never a failure: the frame file is made
/// with `FILE_NO_COMPRESSION` whatever its folder says. `logs` is left as it is: it holds log files only, which
/// nothing sizes or maps, so a compressed one does no harm.
///
/// The folder is held with attribute and permission rights only ([`access_for`]); the clear needs read and write
/// data, so it goes through a second handle that is opened here and only here, when the folder is compressed. That
/// handle is opened like every other (no link followed, no waiting for a lock), verified the same way as the held
/// one ([`verify_opened`]), and must be the very folder the held handle is (same volume and file id): a folder that
/// was swapped in between is not touched. When it cannot be opened (a program holds the folder open without sharing
/// writing, say) that is a line of the log, like a clear that fails.
fn clear_folder_compression(paths: &Paths, dir: &OwnedHandle, log: &mut dyn Log) {
    let shown = path_of(paths, Object::DataDir);
    let shown = shown.as_path();
    let compressed = match secure::kind(secure::raw(dir)) {
        Ok(k) => k.attributes & secure::ATTRIBUTE_COMPRESSED != 0,
        Err(e) => {
            log.line(&format!(
                "could not read the attributes of {} ({e}); not looking at its compressed state",
                shown.display()
            ));
            return;
        }
    };
    if !compressed {
        return;
    }
    let wide = match reopen_for_compression(paths, dir) {
        Ok(h) => h,
        Err(e) => {
            log.line(&format!(
                "{} is compressed and that could not be cleared ({e}); the frame file is made without compression anyway",
                shown.display()
            ));
            return;
        }
    };
    match secure::set_compressed(secure::raw(&wide), false) {
        Ok(()) => log.line(&format!(
            "{} was compressed: cleared that, so that nothing made in it starts out compressed",
            shown.display()
        )),
        Err(e) => log.line(&format!(
            "{} is compressed and that could not be cleared ({e}); the frame file is made without compression anyway",
            shown.display()
        )),
    }
}

/// The data folder opened with the rights that clearing its compressed state needs ([`COMPRESSION_ACCESS`]), checked
/// to be what the held handle `held` is.
fn reopen_for_compression(paths: &Paths, held: &OwnedHandle) -> Res<OwnedHandle> {
    let path = path_of(paths, Object::DataDir);
    let h = secure::open_with(
        &path,
        COMPRESSION_ACCESS,
        share_for(Object::DataDir),
        DISP_OPEN_EXISTING,
        true,
        0,
    )
    .map_err(|e| {
        if secure::is_oplock_busy(&e) {
            "another program holds a lock on it".to_string()
        } else {
            werr("open for writing", e)
        }
    })?;
    verify_opened(paths, Object::DataDir, &h)?;
    match (
        secure::file_identity(secure::raw(held)),
        secure::file_identity(secure::raw(&h)),
    ) {
        (Ok(a), Ok(b)) if a == b => Ok(h),
        (Ok(_), Ok(_)) => Err("it is not the folder that is held any more".into()),
        (Err(e), _) | (_, Err(e)) => Err(werr("its file id cannot be read", e)),
    }
}

/// The sentence for a data folder that is encrypted after the helper has made it new or kept it, which can only mean
/// that something around it hands the flag down (an encrypted `%ProgramData%`: NTFS encrypts a new child of an
/// encrypted folder), or that it could not be replaced. The camera service (Frame Server, LocalService) cannot read what is encrypted with EFS for
/// somebody else's key, so the camera would show nothing and nothing would say why. The helper does not decrypt
/// anything (that works by path, and the helper is not whoever encrypted it), and it cannot make a plain folder inside
/// an encrypted one, so it stops and says what to do about the folder around it. `parent_attributes` are those of
/// `%ProgramData%`. `None` when the data folder is not encrypted.
///
/// `cipher /d` on a folder (without `/s`) only turns encryption off for what is made in it from then on; nothing that
/// is in it already is decrypted, so it is safe to run on `%ProgramData%`.
pub fn encryption_refusal(
    paths: &Paths,
    data_attributes: u32,
    parent_attributes: Option<u32>,
) -> Option<String> {
    if data_attributes & ATTRIBUTE_ENCRYPTED == 0 {
        return None;
    }
    let data = paths.data.display();
    let parent = paths.program_data.display();
    Some(
        if parent_attributes.is_some_and(|a| a & ATTRIBUTE_ENCRYPTED != 0) {
            format!(
            "The folder {data} is encrypted (Windows EFS) because the folder around it, {parent}, is: a new folder is encrypted too, and Repair cannot make a plain one. The camera service cannot read encrypted files, so the virtual camera would show nothing. Ask an administrator to turn off encryption for new items in {parent} (run cipher /d \"{parent}\" in an administrator command prompt; it changes that folder only, nothing already in it), then run Repair again."
        )
        } else {
            format!(
            "The folder {data} is encrypted (Windows EFS) after this run, so something on this PC encrypts new folders in {parent}. The camera service cannot read encrypted files, so the virtual camera would show nothing. Ask an administrator to turn off encryption for it (run cipher /d \"{data}\" in an administrator command prompt), then run Repair again."
        )
        },
    )
}

/// Refuse (with [`encryption_refusal`]) when the data folder, just made or kept, is still encrypted (`attributes`, read
/// on the held handle; `parent_attributes` those of `%ProgramData%`): the helper replaces an encrypted one
/// ([`decide_below`]), so what is left is a new folder that inherited the flag, or one kept because replacing it
/// would give the same.
fn refuse_if_encrypted(paths: &Paths, attributes: u32, parent_attributes: Option<u32>) -> Res<()> {
    match encryption_refusal(paths, attributes, parent_attributes) {
        Some(sentence) => Err(sentence),
        None => Ok(()),
    }
}

/// The attributes of `p` read by path, or `None` when they cannot be read.
fn attributes_of_path(p: &Path) -> Option<u32> {
    attributes(p).ok()
}

fn is_encrypted(attributes: Option<u32>) -> bool {
    attributes.is_some_and(|a| a & ATTRIBUTE_ENCRYPTED != 0)
}

/// What the app can say about a failed install whose log has no reason, when the data folder is encrypted (else
/// `None`). A folder that is still encrypted after a failed install is one that Repair did not put right, so this
/// never says "run Repair" the way [`encryption_problem`] does for a folder nobody has tried to replace. For the app,
/// which reads attributes only.
///
/// With an encrypted `%ProgramData%` that is the sentence of [`encryption_refusal`] (that is why the folder is
/// encrypted, and what to do about it). With a plain one only the fact is known: the run may have failed for another
/// reason (a held `install.log`, a helper that crashed) with the folder encrypted before and never replaced, so no
/// cause is claimed.
///
/// `started` is when the run began. A data folder that was made at or after it is one the helper made new in this run
/// (an encrypted one is replaced, never kept, unless `%ProgramData%` is encrypted), so it came out encrypted because
/// something on the PC encrypts new folders, and the sentence of [`encryption_refusal`] is the cause. (That refusal
/// fires before `logs` exists, so the log has no line for it.) A folder made earlier was not replaced.
pub fn encryption_after_failed_install(paths: &Paths, started: SystemTime) -> Option<String> {
    encryption_after_failed_install_from(
        paths,
        attributes_of_path(&paths.data),
        attributes_of_path(&paths.program_data),
        made_since(&paths.data, started),
    )
}

/// Whether `p` (not followed if it is a link) was created at or after `since`; `false` when that cannot be read.
fn made_since(p: &Path, since: SystemTime) -> bool {
    std::fs::symlink_metadata(p)
        .and_then(|m| m.created())
        .is_ok_and(|created| created >= since)
}

fn encryption_after_failed_install_from(
    paths: &Paths,
    data: Option<u32>,
    parent: Option<u32>,
    made_this_run: bool,
) -> Option<String> {
    let data = data.filter(|a| a & ATTRIBUTE_ENCRYPTED != 0)?;
    if is_encrypted(parent) || made_this_run {
        return encryption_refusal(paths, data, parent);
    }
    Some(format!(
        "The folder {} is encrypted (Windows EFS); this run did not replace it.",
        paths.data.display()
    ))
}

/// What the status report says is wrong with the data folder or the frame file, and whether Repair can put it right.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DataProblem {
    /// The sentence the window shows.
    pub text: String,
    /// Repair can put it right. `false` when `%ProgramData%` itself is encrypted: a new data folder is encrypted too,
    /// Repair refuses it, and only an administrator's `cipher /d` helps (the sentence says so). The window then shows
    /// no Repair button; after the administrator has run it the next status says "Run Repair" and the button is back.
    pub repairable: bool,
}

/// Whether the data folder or the frame file is encrypted right now, as the sentence that says so and what to do. For
/// the status report: it needs no elevation and changes nothing. `None` when neither is encrypted or not there.
///
/// An encrypted data folder, `logs` folder or frame file is something Repair puts right (it replaces it with a new one that is
/// not encrypted), so the sentence says "run Repair"; except when `%ProgramData%` itself is encrypted, where Repair
/// cannot help and the sentence is [`encryption_refusal`]'s ([`DataProblem::repairable`] is `false`). A data folder
/// that is encrypted without its files being so (`cipher /e` without `/s` encrypts what is made in it from then on)
/// says so: only new files are affected.
pub fn encryption_problem(paths: &Paths) -> Option<DataProblem> {
    // attributes only, no link followed, nothing opened for data (no lock of another program is broken or waited for)
    encryption_problem_from(
        paths,
        attributes_of_path(&paths.data),
        attributes_of_path(&paths.program_data),
        attributes_of_path(&paths.logs()),
        attributes_of_path(&paths.frames()),
    )
}

/// [`encryption_problem`] from the attributes of the data folder, `%ProgramData%`, the `logs` folder and the frame file
/// (`None`: not there or not readable).
fn encryption_problem_from(
    paths: &Paths,
    data: Option<u32>,
    parent: Option<u32>,
    logs_attributes: Option<u32>,
    frames_attributes: Option<u32>,
) -> Option<DataProblem> {
    if let Some(a) = data.filter(|a| a & ATTRIBUTE_ENCRYPTED != 0) {
        if is_encrypted(parent) {
            return encryption_refusal(paths, a, parent).map(|text| DataProblem {
                text,
                repairable: false,
            });
        }
        return Some(DataProblem {
            text: format!(
                "The folder {} is encrypted (Windows EFS): new files made in it are encrypted too, and the camera service cannot read an encrypted file, so the virtual camera may show nothing. Run Repair: it replaces the folder with a new one that is not encrypted.",
                paths.data.display()
            ),
            repairable: true,
        });
    }
    if is_encrypted(logs_attributes) {
        return Some(DataProblem {
            text: format!(
                "The folder {} is encrypted (Windows EFS), so install.log and the other logs in it are encrypted for one account and the app may not be able to show later errors. Close the apps that use the camera first, then run Repair: it replaces the folder with a new one that is not encrypted (while an app still uses the folder, Repair can only empty it and it stays encrypted).",
                paths.logs().display()
            ),
            repairable: true,
        });
    }
    let frames = paths.frames();
    frames_attributes
        .filter(|a| a & ATTRIBUTE_ENCRYPTED != 0)
        .map(|_| DataProblem {
            text: format!(
                "{} is encrypted (Windows EFS), so the camera service cannot read it and the virtual camera would show nothing. Run Repair: it replaces the file with a new one that is not encrypted.",
                frames.display()
            ),
            repairable: true,
        })
}

/// The last look at the frame file the helper holds, before its permissions are set: it must be exactly
/// [`vcam_protocol::FILE_SIZE`] bytes long. A file that was kept has that length already; a new one is empty and is
/// grown to it. The camera's user can extend the file at any moment, so the length is read again and a file that
/// changed in that moment is an error that says so.
///
/// **Sparse and compressed are not errors.** Such a file of exactly that length is one the DLL and the app can use
/// (neither looks at these attributes). The helper replaces an existing one by a new file once per run, in
/// [`ensure_exists`], and makes the new one without compression; a file that still carries the attribute here is a
/// volume that compresses regardless or a user who changed it a moment ago, and failing on that would make every
/// Repair fail again. It is one line of the log, and the file is kept as it is.
fn finish_frame_file(frames: &OwnedHandle, log: &mut dyn Log) -> Res<()> {
    let file = std::fs::File::from(
        frames
            .try_clone()
            .map_err(|e| werr("duplicate handle", e))?,
    );
    let len_now = || file.metadata().map(|m| m.len());
    if len_now().ok() != Some(vcam_protocol::FILE_SIZE) {
        file.set_len(vcam_protocol::FILE_SIZE)
            .map_err(|e| werr("size frames.bin", e))?;
    }
    let len = len_now().map_err(|e| werr("size frames.bin", e))?;
    if len != vcam_protocol::FILE_SIZE {
        let want = vcam_protocol::FILE_SIZE;
        return Err(format!(
            "frames.bin is {len} bytes long, not {want}: somebody is changing it; run Repair again"
        ));
    }
    let attrs = secure::kind(secure::raw(frames))
        .map_err(|e| werr("look at frames.bin", e))?
        .attributes;
    let what = match (
        attrs & secure::ATTRIBUTE_SPARSE != 0,
        attrs & secure::ATTRIBUTE_COMPRESSED != 0,
    ) {
        (false, false) => return Ok(()),
        (true, false) => "sparse",
        (false, true) => "compressed",
        (true, true) => "sparse and compressed",
    };
    log.line(&format!(
        "frames.bin is {what} (this volume or another program did that; a new file is made without compression and looked at before it is used). It is exactly {} bytes long, which the camera and the app can use, so it is kept as it is",
        vcam_protocol::FILE_SIZE
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
/// moved-aside data folders next to `BetterCam` itself ([`sweep_data_asides`]: the entries in `recorded`, each only
/// while it is the folder that was recorded and owned by one of `trusted`).
pub fn sweep_asides(
    paths: &Paths,
    trusted: &[String],
    recorded: &[AsideEntry],
    log: &mut dyn Log,
) -> AsideSweep {
    // inside the data folder: nobody but the helper can put anything there (it is locked first), so a name will do
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
    sweep_data_asides(paths, trusted, recorded, log)
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

/// How many recorded entries one sweep looks at. The record only grows when the helper moves a data folder aside and
/// cannot delete the old copy, so it is short; the bound keeps a run short whatever is in it. The entries that are
/// looked at and stay move to the end of the record ([`AsideSweep::kept`]), so every entry is reached in time.
pub const MAX_ASIDES_EXAMINED: usize = 32;

/// How many names of the entries that were left alone the one summary line shows.
const NAMES_SHOWN: usize = 3;

/// The outcome of a sweep of the recorded old data folders.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AsideSweep {
    /// How many of the helper's own old copies are still there (in use, or locked by another program).
    pub left: usize,
    /// The recorded names that are done with: the folder is gone, or it is no longer the helper's (a link, another
    /// owner, not the folder that was recorded, not a name the helper makes). They are taken out of the record.
    pub forget: Vec<String>,
    /// The names that were looked at and stay on the record: the copy is still in use, or the entry could not be
    /// looked at. They move to the end of the record, behind the names that were not looked at this time.
    pub kept: Vec<String>,
}

/// What a recorded entry of `%ProgramData%` is now.
enum Probe {
    Gone,
    Link,
    /// The owner, and the identity when it could be read.
    Found {
        owner: String,
        identity: Option<secure::FileIdentity>,
    },
    /// The open was refused for a reason that says nothing about the entry (access denied, ...): tried again later.
    Unreadable,
    /// The system says this is not a name (it can never be opened): the record is wrong.
    InvalidName,
}

/// The errors of an open that mean "this is not a valid name or path": `ERROR_INVALID_NAME` (123),
/// `ERROR_BAD_PATHNAME` (161), `ERROR_FILENAME_EXCED_RANGE` (206), or the refusal of a name that cannot be made
/// absolute. A record that holds such a name will never lead anywhere.
pub fn is_invalid_name_error(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::InvalidInput
        || matches!(e.raw_os_error(), Some(123) | Some(161) | Some(206))
}

/// Look at `path` without following a link and without touching any lock (attributes and permissions only).
fn probe_entry(path: &Path) -> Probe {
    let h = match secure::open_with(
        path,
        READ_CONTROL | 0x80, /* FILE_READ_ATTRIBUTES */
        secure::SHARE_ALL,
        DISP_OPEN_EXISTING,
        true,
        0,
    ) {
        Ok(h) => h,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Probe::Gone,
        Err(e) if is_invalid_name_error(&e) => return Probe::InvalidName,
        Err(_) => return Probe::Unreadable,
    };
    match secure::kind(secure::raw(&h)) {
        Ok(k) if k.reparse_point => Probe::Link,
        Ok(_) => acl::read(&h).map_or(Probe::Unreadable, |s| Probe::Found {
            owner: s.owner,
            identity: secure::file_identity(secure::raw(&h)).ok(),
        }),
        Err(_) => Probe::Unreadable,
    }
}

/// Delete the old data folders that the helper moved aside ([`move_aside`] of the data folder: it was a link, or had
/// the wrong owner, ...) and could not delete then. They live *next to* `BetterCam`, in `%ProgramData%`, and the camera
/// service can still have a file of one open. Called by install and by uninstall.
///
/// `recorded` is the helper's own record of those folders ([`crate::aside`], kept where only administrators can
/// write). **Only those entries are looked at.** A standard user can create entries in `%ProgramData%` under any
/// name, and an administrator-owned folder somewhere on the same volume that a user may rename can be moved to a name
/// like `BetterCam.old-x`, so neither a name pattern nor an owner proves that the helper made an entry; the record
/// does, and it holds each folder's **identity** (volume and file id) as well as its name: a name that was freed and
/// filled with another folder is not the recorded folder. A recorded entry is deleted only while name, identity, "not
/// a link" and "owner is one of `trusted`" (SYSTEM, Administrators) all hold, read through a handle that does not
/// follow a link and does not touch a lock, and checked again on the handle the delete goes through. One that is
/// not (or that is gone, or whose name is not one the helper makes) is left exactly as it is and forgotten.
///
/// At most [`MAX_ASIDES_EXAMINED`] entries are examined per run (the ones that stay move to the end of the record,
/// see [`AsideSweep::kept`]), and the entries that were left alone are one line of the log, with the first few names
/// and the count. The delete never waits for another program (see [`remove_no_follow`]): an entry that a program
/// still has open or locked stays, and is tried again next time.
pub fn sweep_data_asides(
    paths: &Paths,
    trusted: &[String],
    recorded: &[AsideEntry],
    log: &mut dyn Log,
) -> AsideSweep {
    sweep_data_asides_with(paths, trusted, recorded, log, &mut |_| None)
}

/// [`sweep_data_asides`] with a hook that runs between the first look at an entry and the delete (the moment in which
/// somebody could swap the entry). It may return the owners that the check on the handle then trusts (a test makes
/// an entry's owner change this way, which it cannot do for real without the privilege).
pub fn sweep_data_asides_with(
    paths: &Paths,
    trusted: &[String],
    recorded: &[AsideEntry],
    log: &mut dyn Log,
    between: &mut dyn FnMut(&Path) -> Option<Vec<String>>,
) -> AsideSweep {
    let mut out = AsideSweep::default();
    // names left alone and dropped from the record, and names that could not be looked at and stay
    let mut dropped: Vec<&str> = Vec::new();
    let mut unreadable: Vec<&str> = Vec::new();
    for entry in recorded.iter().take(MAX_ASIDES_EXAMINED) {
        let name = &entry.name;
        if !aside::is_aside_name(name) {
            // not a name the helper ever made: never joined to a path, and forgotten (it would use up a slot of
            // every run for ever)
            dropped.push(name);
            out.forget.push(name.clone());
            continue;
        }
        let path = paths.program_data.join(name);
        match probe_entry(&path) {
            Probe::Gone => {
                out.forget.push(name.clone());
                continue;
            }
            Probe::Link | Probe::InvalidName => {
                dropped.push(name);
                out.forget.push(name.clone());
                continue;
            }
            Probe::Found { owner, .. } if !acl::owner_trusted(&owner, trusted) => {
                dropped.push(name);
                out.forget.push(name.clone());
                continue;
            }
            // somebody else's folder under the recorded name: not the folder that was recorded
            Probe::Found {
                identity: Some(id), ..
            } if id != entry.identity => {
                dropped.push(name);
                out.forget.push(name.clone());
                continue;
            }
            Probe::Unreadable | Probe::Found { identity: None, .. } => {
                unreadable.push(name);
                out.kept.push(name.clone());
                continue;
            }
            Probe::Found { .. } => {}
        }
        let trusted_then = between(&path).unwrap_or_else(|| trusted.to_vec());
        // the check again, on the handle that is deleted through (the entry may have been swapped meanwhile): the
        // name was looked at, but what is deleted is whatever this handle is, so it must be the recorded folder
        let swapped = std::cell::Cell::new(false);
        let check = |h: HANDLE| -> std::io::Result<()> {
            let k = secure::kind(h)?;
            if k.reparse_point {
                swapped.set(true);
                return Err(secure::refused("it is a link now"));
            }
            let snap = acl::read_handle(h).map_err(secure::refused)?;
            if !acl::owner_trusted(&snap.owner, &trusted_then) {
                swapped.set(true);
                return Err(secure::refused(format!(
                    "its owner is {}, not SYSTEM or Administrators",
                    snap.owner
                )));
            }
            match secure::file_identity(h) {
                Ok(id) if id == entry.identity => Ok(()),
                Ok(_) => {
                    swapped.set(true);
                    Err(secure::refused("it is not the folder that was recorded"))
                }
                Err(e) => Err(secure::refused(format!(
                    "its identity cannot be read ({e})"
                ))),
            }
        };
        let top = secure::Top {
            access: secure::ACCESS_OWNER_CHECK,
            check: &check,
        };
        match secure::remove_tree_checked(&path, &top) {
            Ok(()) => {
                out.forget.push(name.clone());
                log.line(&format!("deleted old data folder {}", path.display()));
            }
            Err(_) if swapped.get() => {
                dropped.push(name);
                out.forget.push(name.clone());
            }
            Err(err) => {
                out.left += 1;
                out.kept.push(name.clone());
                log.line(&format!(
                    "old data folder {name} still in use ({err}); it is deleted by the next install or removal"
                ));
            }
        }
    }
    let list = |names: &[&str]| {
        let shown: Vec<&str> = names.iter().copied().take(NAMES_SHOWN).collect();
        let more = names.len() - shown.len();
        format!(
            "{}{}",
            shown.join(", "),
            if more > 0 {
                format!(" and {more} more")
            } else {
                String::new()
            }
        )
    };
    if !dropped.is_empty() {
        log.line(&format!(
            "{} recorded old data folder(s) left alone and dropped from the record (a link now, not the folder that was recorded, another owner than SYSTEM or Administrators, or not a name the helper makes): {}",
            dropped.len(),
            list(&dropped)
        ));
    }
    if !unreadable.is_empty() {
        log.line(&format!(
            "{} recorded old data folder(s) could not be looked at; they stay on the record and are looked at again later: {}",
            unreadable.len(),
            list(&unreadable)
        ));
    }
    if recorded.len() > MAX_ASIDES_EXAMINED {
        log.line(&format!(
            "{} more recorded old data folder(s) are looked at by the next runs (the ones that stay move to the end of the record)",
            recorded.len() - MAX_ASIDES_EXAMINED
        ));
    }
    out
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
    prepare_recording(paths, p, stop_service, log, &mut NoLedger)
}

/// [`prepare`] that puts a data folder it moves aside on the record through `ledger`: **before** the rename, with the
/// identity read on the handle that is renamed, and only for a folder that is not a link and is owned by SYSTEM or
/// Administrators; the entry is dropped again when the rename does not happen or the old copy is deleted at once. What
/// stays on the record is what a later run sweeps (see [`sweep_data_asides`]), also when preparing fails after that.
pub fn prepare_recording(
    paths: &Paths,
    p: &Principals,
    stop_service: &mut dyn FnMut() -> bool,
    log: &mut dyn Log,
    ledger: &mut dyn Ledger,
) -> Res<()> {
    prepare_reading(
        paths,
        p,
        stop_service,
        log,
        ledger,
        &Reading {
            parent: &|| attributes_of_path(&paths.program_data),
            folder: &|h| {
                secure::kind(secure::raw(h))
                    .map(|k| k.attributes)
                    .map_err(|e| werr(&format!("look at {}", paths.data.display()), e))
            },
            seen: &as_read,
        },
    )
}

/// The attributes [`prepare_reading`] reads, as functions a test can replace.
struct Reading<'a> {
    /// Those of `%ProgramData%`.
    parent: &'a dyn Fn() -> Option<u32>,
    /// Those of the held data folder, once it is locked.
    folder: &'a dyn Fn(&OwnedHandle) -> Res<u32>,
    /// Those of an object that is looked at before it is kept or replaced (see [`inspect`]).
    seen: &'a dyn Fn(Object, u32) -> u32,
}

/// [`prepare_recording`] with the attributes it reads given, so that a test can say what an encrypted `%ProgramData%`
/// or an encrypted data folder makes of a run without encrypting anything.
fn prepare_reading(
    paths: &Paths,
    p: &Principals,
    stop_service: &mut dyn FnMut() -> bool,
    log: &mut dyn Log,
    ledger: &mut dyn Ledger,
    reading: &Reading,
) -> Res<()> {
    // a log file of ours inside `logs` would keep the folder from being moved: close it, lines wait meanwhile
    log.release_folder();
    std::fs::create_dir_all(&paths.program_data)
        .map_err(|e| werr(&format!("create {}", paths.program_data.display()), e))?;
    // 1. the folder: held (no delete sharing) until the end, and locked right away: from here the user can neither
    // create nor rename anything in it, and what it hands down to new children is only what the helper wants. Read
    // before anything is replaced: a folder that is encrypted in an encrypted `%ProgramData%` is kept, not replaced
    // (a new one would be encrypted too, and its logs would be gone for nothing)
    let parent = (reading.parent)();
    let below = |parent_encrypted| Below {
        parent_encrypted,
        seen: reading.seen,
    };
    let data = ensure_exists(
        paths,
        Object::DataDir,
        p,
        stop_service,
        log,
        ledger,
        below(is_encrypted(parent)),
    )?;
    // (a folder made inside a compressed `%ProgramData%` is compressed, and would hand that on to what is made in it)
    clear_folder_compression(paths, &data, log);
    lock(&data, Object::DataDir, p, log)?;
    // an encrypted folder was replaced above ([`decide_below`]); one that is encrypted all the same is a new folder in
    // an encrypted `%ProgramData%`, or one that was kept for that reason. After the lock (so that the permissions an
    // older version left are gone whatever happens next), before anything is made in it: a child of an encrypted
    // folder is encrypted too, and the camera service could not read it
    // (`%ProgramData%` is not read a second time: the helper holds the machine-wide lock; there is no behaviour change
    // unless `%ProgramData%` changes during the run, and then only the wording of the refusal can differ)
    refuse_if_encrypted(paths, (reading.folder)(&data)?, parent)?;
    // 2. the folder's children, each with a protected DACL of its own
    let logs = ensure_exists(
        paths,
        Object::LogsDir,
        p,
        stop_service,
        log,
        ledger,
        below(false),
    )?;
    lock(&logs, Object::LogsDir, p, log)?;
    let mut trusted = p.trusted_owners.clone();
    trusted.extend([p.system.clone(), p.admins.clone(), p.local_service.clone()]);
    sweep_logs(paths, &trusted, log);
    let frames = ensure_exists(
        paths,
        Object::FramesFile,
        p,
        stop_service,
        log,
        ledger,
        below(false),
    )?;
    finish_frame_file(&frames, log)?;
    lock(&frames, Object::FramesFile, p, log)?;
    // (no look at the encrypted flag here: an encrypted frame file was replaced above, and the new one is made in a
    // data folder that was just found not encrypted, so it is plain)
    // the helper's own log: a DACL of its own, so the service's inheritable modify right on `logs` stops short of it
    let install_log = ensure_exists(
        paths,
        Object::InstallLog,
        p,
        stop_service,
        log,
        ledger,
        below(false),
    )?;
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
