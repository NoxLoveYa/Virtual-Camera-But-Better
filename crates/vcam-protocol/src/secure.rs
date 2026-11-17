//! Opening files in a folder that a less trusted account can influence.
//!
//! `%ProgramData%\BetterCam` is writable by the installing user by design, while the DLL runs as LOCAL SERVICE
//! (and in every process that lists cameras). Anything in that folder may be replaced by a link, a junction or a
//! hard link that points somewhere else, so the DLL never follows one:
//!
//! * [`open_existing`] opens with `FILE_FLAG_OPEN_REPARSE_POINT`, so a link in the last path component is opened as
//!   the link itself and then refused;
//! * [`verify_file`] / [`verify_dir`] look at the opened handle (not at the path, so there is no check-then-use
//!   gap): no reparse point, not a directory (for files), exactly one hard link, and, when a trusted root is given,
//!   [`GetFinalPathNameByHandleW`] must name exactly `<root>\<relative path>` on a local fixed volume. A junction
//!   or symbolic link in any parent component changes the final path, so it fails that comparison;
//! * the trusted root is `%ProgramData%` taken from `SHGetKnownFolderPath`, not from the environment. The root is
//!   resolved *through* links ([`canonical_dir`]): only an administrator can change `%ProgramData%`, and a machine
//!   whose `%ProgramData%` is a junction to another drive still works. Everything *below* it is never followed;
//! * no open waits for another program: every open goes through `NtCreateFile` with `FILE_COMPLETE_IF_OPLOCKED`
//!   (see [`open_with`]). The frame file and the logs can be read by a standard user, and a user process that holds
//!   an opportunistic lock on a file makes a plain `CreateFileW` of a second process block until it lets go.
//! * deleting and renaming wait for such a program too (`DeleteFileW`, `RemoveDirectoryW`, `MoveFileExW`,
//!   `std::fs::remove_*` and `rename` open the file the plain way), so the installer's deletes and renames go through
//!   the same kind of handle ([`remove_tree`], [`remove_children`], [`rename_no_wait`]): folders are walked relative
//!   to the handles that are held, links are removed as links, and a file that is locked is left and named. A *rename
//!   of a folder* can still wait inside the rename call itself (a read-write-handle lock on a file below it must be
//!   broken first, which no create flag can refuse); that call is bounded and cancelled.
//!
//! A refusal is an [`io::Error`] that wraps [`Refused`]; see [`is_refused`].

use std::ffi::c_void;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

use windows::core::{PCWSTR, PWSTR};
use windows::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows::Wdk::Storage::FileSystem::{
    NtCreateFile, FILE_COMPLETE_IF_OPLOCKED, FILE_CREATE, FILE_DIRECTORY_FILE,
    FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_FOR_BACKUP_INTENT, FILE_OPEN_IF,
    FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT, NTCREATEFILE_CREATE_DISPOSITION,
    NTCREATEFILE_CREATE_OPTIONS,
};
use windows::Win32::Foundation::{
    CloseHandle, RtlNtStatusToDosError, HANDLE, NTSTATUS, OBJECT_ATTRIBUTE_FLAGS,
    STATUS_OPLOCK_BREAK_IN_PROGRESS, UNICODE_STRING,
};
use windows::Win32::Security::SECURITY_DESCRIPTOR;
use windows::Win32::Storage::FileSystem::{
    FileDispositionInfo, FileDispositionInfoEx, FileFullDirectoryInfo,
    FileFullDirectoryRestartInfo, FileRenameInfo, FileStreamInfo, GetDriveTypeW,
    GetFileInformationByHandle, GetFileInformationByHandleEx, GetFinalPathNameByHandleW,
    GetFullPathNameW, SetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, CREATE_NEW,
    FILE_ACCESS_RIGHTS, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_CREATION_DISPOSITION, FILE_FLAGS_AND_ATTRIBUTES,
    FILE_FULL_DIR_INFO, FILE_NAME_NORMALIZED, FILE_RENAME_INFO, FILE_SHARE_DELETE, FILE_SHARE_MODE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_STREAM_INFO, GETFINALPATHNAMEBYHANDLE_FLAGS,
    OPEN_ALWAYS, OPEN_EXISTING, SYNCHRONIZE, VOLUME_NAME_DOS,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Ioctl::{
    FILE_SET_SPARSE_BUFFER, FSCTL_SET_COMPRESSION, FSCTL_SET_SPARSE,
};
use windows::Win32::System::Threading::{GetCurrentThreadId, OpenThread, THREAD_TERMINATE};
use windows::Win32::System::IO::{CancelSynchronousIo, DeviceIoControl, IO_STATUS_BLOCK};
use windows::Win32::UI::Shell::{FOLDERID_ProgramData, SHGetKnownFolderPath, KNOWN_FOLDER_FLAG};

#[cfg(any(test, feature = "test-support"))]
pub mod oplock;

/// `GetDriveTypeW` result for a local fixed disk.
const DRIVE_FIXED: u32 = 3;

/// `GENERIC_READ`: for folders.
pub const ACCESS_GENERIC_READ: u32 = 0x8000_0000;
/// `GENERIC_WRITE`.
pub const ACCESS_GENERIC_WRITE: u32 = 0x4000_0000;
/// `FILE_READ_DATA | FILE_READ_ATTRIBUTES`: what a reader of the frame file asks for, nothing more.
pub const ACCESS_READ: u32 = 0x0000_0081;
/// `FILE_READ_DATA | FILE_WRITE_DATA | FILE_READ_ATTRIBUTES`: read and write the frame file's *contents* (a read-write
/// mapping needs exactly these). No `FILE_WRITE_ATTRIBUTES`, so the open never needs the right the file does not
/// give (see `vcam-install`'s `acl`: a user that could write attributes could set the file read-only or sparse, or
/// hard-link it from a folder of their own).
pub const ACCESS_READ_WRITE: u32 = 0x0000_0083;

/// Why a file was refused (the payload of the [`io::Error`] built by [`refused`]).
#[derive(Debug)]
pub struct Refused(pub String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "refused: {}", self.0)
    }
}

impl std::error::Error for Refused {}

/// An error that says "this file is not what it must be", as opposed to an ordinary I/O failure.
pub fn refused(why: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, Refused(why.into()))
}

/// True for an error made by [`refused`]: retrying with other access rights cannot help.
pub fn is_refused(e: &io::Error) -> bool {
    e.get_ref().is_some_and(|i| i.is::<Refused>())
}

/// `%ProgramData%` from the shell's known folder database (cached). The environment variable is not consulted: the
/// DLL runs in processes whose environment a standard user may have shaped. Falls back to `C:\ProgramData`.
pub fn program_data_dir() -> PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let known =
            unsafe { SHGetKnownFolderPath(&FOLDERID_ProgramData, KNOWN_FOLDER_FLAG(0), None) };
        match known {
            Ok(p) => {
                let s = unsafe { p.to_string() };
                unsafe { CoTaskMemFree(Some(p.0 as *const core::ffi::c_void)) };
                match s {
                    Ok(s) if !s.is_empty() => PathBuf::from(s),
                    _ => PathBuf::from(r"C:\ProgramData"),
                }
            }
            Err(_) => PathBuf::from(r"C:\ProgramData"),
        }
    })
    .clone()
}

fn wide(p: &Path) -> Vec<u16> {
    p.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn win_err(e: windows::core::Error) -> io::Error {
    io::Error::from_raw_os_error(e.code().0 & 0xFFFF)
}

/// The raw handle of anything that owns one.
pub fn raw(h: &impl AsRawHandle) -> HANDLE {
    HANDLE(h.as_raw_handle())
}

/// Open an existing file or folder with every share right, not following a link in the last component (a link is
/// opened as the link). A file is opened with exactly the rights it needs ([`ACCESS_READ`], [`ACCESS_READ_WRITE`]);
/// `directory` asks for a folder (`GENERIC_READ`, opened with backup intent).
pub fn open_no_follow(
    path: &Path,
    write: bool,
    directory: bool,
    share: FILE_SHARE_MODE,
) -> io::Result<OwnedHandle> {
    let access = match (directory, write) {
        (true, _) => ACCESS_GENERIC_READ,
        (false, false) => ACCESS_READ,
        (false, true) => ACCESS_READ_WRITE,
    };
    open_with(path, access, share, OPEN_EXISTING, directory, 0)
}

/// How [`open_with`] treats a missing file.
pub use windows::Win32::Storage::FileSystem::{
    CREATE_NEW as DISP_CREATE_NEW, OPEN_ALWAYS as DISP_OPEN_ALWAYS,
    OPEN_EXISTING as DISP_OPEN_EXISTING,
};

/// The general form of [`open_no_follow`]: any `access` mask (`WRITE_DAC` and `WRITE_OWNER` included, which the
/// installer needs to set permissions through the handle), a creation disposition and extra attribute flags
/// (for example `FILE_ATTRIBUTE_TEMPORARY`). The last path component is never followed: a link is opened as the
/// link (and `CREATE_NEW` fails on one).
///
/// The open never waits for another program: it goes through `NtCreateFile` with `FILE_COMPLETE_IF_OPLOCKED`. When
/// another process holds an opportunistic lock that this open would break, Windows would hold the caller until that
/// process answers (for as long as it likes, and a standard user can take such a lock on a file they may read). The
/// call then returns a [`refused`] error (the handle it got is closed) and the caller treats the file as not usable
/// for now. `directory` allows the object to be a folder; without it only a file opens.
pub fn open_with(
    path: &Path,
    access: u32,
    share: FILE_SHARE_MODE,
    disposition: FILE_CREATION_DISPOSITION,
    directory: bool,
    extra_flags: u32,
) -> io::Result<OwnedHandle> {
    // SAFETY: no security descriptor is passed.
    unsafe {
        open_with_sd(
            path,
            access,
            share,
            disposition,
            directory,
            extra_flags,
            None,
        )
    }
}

/// [`open_with`] that gives a *new* file its security descriptor in the create call, so it never exists with other
/// permissions (an existing file keeps its own).
///
/// # Safety
/// `sd`, when given, must point to a valid self-relative or absolute security descriptor that outlives the call.
pub unsafe fn open_with_sd(
    path: &Path,
    access: u32,
    share: FILE_SHARE_MODE,
    disposition: FILE_CREATION_DISPOSITION,
    directory: bool,
    extra_flags: u32,
    sd: Option<*const c_void>,
) -> io::Result<OwnedHandle> {
    let nt_disposition = if disposition == OPEN_EXISTING {
        FILE_OPEN
    } else if disposition == CREATE_NEW {
        FILE_CREATE
    } else if disposition == OPEN_ALWAYS {
        FILE_OPEN_IF
    } else {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    };
    let options = FILE_OPEN_REPARSE_POINT.0
        | if directory {
            // a folder or a file, as `FILE_FLAG_BACKUP_SEMANTICS` allows
            FILE_OPEN_FOR_BACKUP_INTENT.0
        } else {
            FILE_NON_DIRECTORY_FILE.0
        };
    // FILE_ATTRIBUTE_NORMAL is valid only on its own
    let attributes = if extra_flags == 0 {
        FILE_ATTRIBUTE_NORMAL.0
    } else {
        extra_flags
    };
    nt_open(path, access, share, nt_disposition, options, attributes, sd)
}

/// Create a *new* folder at `path` with the security descriptor `sd` from the first moment, and open it. Fails when
/// anything (a link included) is at `path`. The folder is not opened with delete sharing unless `share` says so.
///
/// # Safety
/// `sd` must point to a valid security descriptor that outlives the call.
pub unsafe fn create_dir_with_sd(
    path: &Path,
    access: u32,
    share: FILE_SHARE_MODE,
    sd: *const c_void,
) -> io::Result<OwnedHandle> {
    nt_open(
        path,
        access,
        share,
        FILE_CREATE,
        FILE_DIRECTORY_FILE.0 | FILE_OPEN_FOR_BACKUP_INTENT.0 | FILE_OPEN_REPARSE_POINT.0,
        FILE_ATTRIBUTE_NORMAL.0,
        Some(sd),
    )
}

/// `\??\C:\dir\file` for `p`: made absolute and normalized (`NtCreateFile` does neither), as UTF-16 without a
/// terminator.
fn nt_path(p: &Path) -> io::Result<Vec<u16>> {
    let w = wide(p);
    let mut buf = vec![0u16; 512];
    let full = loop {
        let n = unsafe { GetFullPathNameW(PCWSTR(w.as_ptr()), Some(&mut buf), None) } as usize;
        if n == 0 {
            return Err(io::Error::last_os_error());
        }
        if n < buf.len() {
            buf.truncate(n);
            break buf;
        }
        buf = vec![0u16; n + 1];
    };
    let starts = |prefix: &str| {
        let pre: Vec<u16> = prefix.encode_utf16().collect();
        full.len() >= pre.len() && full[..pre.len()] == pre[..]
    };
    let enc = |s: &str| s.encode_utf16().collect::<Vec<u16>>();
    let out = if starts(r"\\?\UNC\") {
        [enc(r"\??\UNC\"), full[8..].to_vec()].concat()
    } else if starts(r"\\?\") {
        [enc(r"\??\"), full[4..].to_vec()].concat()
    } else if starts(r"\\.\") {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    } else if starts(r"\\") {
        [enc(r"\??\UNC\"), full[2..].to_vec()].concat()
    } else {
        [enc(r"\??\"), full].concat()
    };
    if out.len() > (u16::MAX as usize) / 2 - 1 {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    }
    Ok(out)
}

/// The reason text of the refusal that an open breaks an oplock of somebody else.
const OPLOCK_BUSY: &str =
    "another program holds a lock on the file (an oplock break is in progress); not waiting for it";

/// `NtCreateFile`, synchronous, case-insensitive, never waiting for an oplock holder.
fn nt_open(
    path: &Path,
    access: u32,
    share: FILE_SHARE_MODE,
    disposition: NTCREATEFILE_CREATE_DISPOSITION,
    options: u32,
    attributes: u32,
    sd: Option<*const c_void>,
) -> io::Result<OwnedHandle> {
    nt_open_at(
        None,
        nt_path(path)?,
        access,
        share,
        disposition,
        options,
        attributes,
        sd,
    )
}

/// [`nt_open`] for an NT name that is either absolute (`\??\C:\...`, `root` is `None`) or a single name relative to
/// the folder handle `root` (the way a folder is walked: the entry is looked up in the folder that is held, never
/// by its path again).
#[allow(clippy::too_many_arguments)]
fn nt_open_at(
    root: Option<HANDLE>,
    mut name: Vec<u16>,
    access: u32,
    share: FILE_SHARE_MODE,
    disposition: NTCREATEFILE_CREATE_DISPOSITION,
    options: u32,
    attributes: u32,
    sd: Option<*const c_void>,
) -> io::Result<OwnedHandle> {
    let bytes = (name.len() * 2) as u16;
    let us = UNICODE_STRING {
        Length: bytes,
        MaximumLength: bytes,
        Buffer: PWSTR(name.as_mut_ptr()),
    };
    let oa = OBJECT_ATTRIBUTES {
        Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: root.unwrap_or_default(),
        ObjectName: &us,
        // OBJ_CASE_INSENSITIVE
        Attributes: OBJECT_ATTRIBUTE_FLAGS(0x40),
        SecurityDescriptor: sd.map_or(std::ptr::null(), |p| p as *const SECURITY_DESCRIPTOR),
        SecurityQualityOfService: std::ptr::null(),
    };
    let mut h = HANDLE::default();
    let mut iosb = IO_STATUS_BLOCK::default();
    let status = unsafe {
        NtCreateFile(
            &mut h,
            FILE_ACCESS_RIGHTS(access | SYNCHRONIZE.0),
            &oa,
            &mut iosb,
            None,
            FILE_FLAGS_AND_ATTRIBUTES(attributes),
            share,
            disposition,
            NTCREATEFILE_CREATE_OPTIONS(
                options | FILE_SYNCHRONOUS_IO_NONALERT.0 | FILE_COMPLETE_IF_OPLOCKED.0,
            ),
            None,
            0,
        )
    };
    drop(name);
    finish_open(status, h)
}

/// What the status of `NtCreateFile` means for the handle it returned. Only the plain success keeps the handle;
/// any other status closes a handle that came with it (a success code such as "oplock break in progress", or an
/// informational one, may carry one), so no way out leaks it.
fn finish_open(status: NTSTATUS, h: HANDLE) -> io::Result<OwnedHandle> {
    finish_open_with(status, h, |h| unsafe {
        let _ = CloseHandle(h);
    })
}

/// [`finish_open`] with the way a handle is closed given (a test records it).
fn finish_open_with(
    status: NTSTATUS,
    h: HANDLE,
    close: impl FnOnce(HANDLE),
) -> io::Result<OwnedHandle> {
    if status == NTSTATUS(0) {
        // SAFETY: NtCreateFile returned a valid handle that nothing else owns.
        return Ok(unsafe { OwnedHandle::from_raw_handle(h.0) });
    }
    // not the plain success: whatever handle may have been returned is not wanted, whether the status is a success
    // code, an informational one or an error
    if !h.is_invalid() && !h.0.is_null() {
        close(h);
    }
    if status == STATUS_OPLOCK_BREAK_IN_PROGRESS {
        // success code with a handle: the open went through, but only after asking a holder to give up its lock
        return Err(refused(OPLOCK_BUSY));
    }
    if status.0 >= 0 {
        // some other informational success: there is nothing usable behind it
        return Err(io::Error::other(format!(
            "NtCreateFile returned status 0x{:08x}",
            status.0 as u32
        )));
    }
    Err(io::Error::from_raw_os_error(
        unsafe { RtlNtStatusToDosError(status) } as i32,
    ))
}

/// True for the refusal of an open, delete or rename that met another program's opportunistic lock (see
/// [`open_with`]): "in use, try again later", not a fault of the file.
pub fn is_oplock_busy(e: &io::Error) -> bool {
    e.get_ref()
        .and_then(|i| i.downcast_ref::<Refused>())
        .is_some_and(|r| r.0.contains(OPLOCK_BUSY))
}

// ---------------------------------------------------------------------------------------------------------------
// Deleting and renaming through handles
//
// `DeleteFileW`, `RemoveDirectoryW`, `MoveFileExW` and `std::fs::{remove_*, rename}` open the file the plain way, so
// they wait for as long as a program holds an opportunistic lock on it (a standard user can take one on any file
// they may read). These go through [`nt_open_at`] like every open here: the file is opened with `DELETE` and
// `FILE_COMPLETE_IF_OPLOCKED`, and the deletion or rename is a call on *that handle*. A folder is walked relative to
// the handle that is held, never looked up by path again, and a link is only ever deleted as a link.

/// Deleting a tree nested deeper than this is refused (nothing here makes such a tree; a hostile one must not be
/// able to run the stack out).
const MAX_DEPTH: usize = 128;

/// Why a delete stopped: the path that could not go and why. Converts to an [`io::Error`] with the path in its text
/// (an oplock holder is still told apart by [`is_oplock_busy`]).
#[derive(Debug)]
pub struct RemoveError {
    pub path: PathBuf,
    pub error: io::Error,
    /// How many further entries could not be deleted either (not counting this one).
    pub more: usize,
}

impl RemoveError {
    fn new(path: &Path, error: io::Error) -> RemoveError {
        RemoveError {
            path: path.to_path_buf(),
            error,
            more: 0,
        }
    }

    /// Another program's opportunistic lock is in the way.
    pub fn locked(&self) -> bool {
        is_oplock_busy(&self.error)
    }

    /// Everything that stayed: this entry and `more`.
    pub fn left(&self) -> usize {
        self.more + 1
    }
}

impl std::fmt::Display for RemoveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.locked() {
            write!(
                f,
                "{} is locked by another program (it holds an opportunistic lock); not waiting for it",
                self.path.display()
            )?;
        } else {
            write!(f, "{}: {}", self.path.display(), self.error)?;
        }
        if self.more > 0 {
            write!(f, " (and {} more)", self.more)?;
        }
        Ok(())
    }
}

impl std::error::Error for RemoveError {}

impl From<RemoveError> for io::Error {
    fn from(e: RemoveError) -> io::Error {
        if e.locked() {
            refused(format!("{}: {OPLOCK_BUSY}", e.path.display()))
        } else if is_refused(&e.error) {
            refused(e.to_string())
        } else {
            io::Error::new(e.error.kind(), e.to_string())
        }
    }
}

/// One error for all that stayed behind in a folder: the first, with the count of the others.
fn first_of(mut failed: Vec<RemoveError>) -> Option<RemoveError> {
    if failed.is_empty() {
        return None;
    }
    let total: usize = failed.iter().map(RemoveError::left).sum();
    let mut first = failed.remove(0);
    first.more = total - 1;
    Some(first)
}

const ACCESS_DELETE: u32 = 0x0001_0000;
const ACCESS_READ_ATTRIBUTES: u32 = 0x80;
const ACCESS_LIST_DIRECTORY: u32 = 0x1;
const ACCESS_READ_CONTROL: u32 = 0x0002_0000;

/// What an entry of a folder is, from the listing (used only to choose how to open it; the opened handle decides).
#[derive(Clone, Copy)]
struct Entry {
    directory: bool,
}

/// Open the entry `name` of the folder `parent` (or the absolute `name` when there is none) as itself, never through
/// a link, without waiting for an oplock holder.
fn open_entry(
    parent: Option<HANDLE>,
    name: &[u16],
    access: u32,
    entry: Option<Entry>,
) -> io::Result<OwnedHandle> {
    let kind = match entry {
        Some(Entry { directory: true }) => FILE_DIRECTORY_FILE.0,
        Some(Entry { directory: false }) => FILE_NON_DIRECTORY_FILE.0,
        None => 0,
    };
    nt_open_at(
        parent,
        name.to_vec(),
        access,
        SHARE_ALL,
        FILE_OPEN,
        FILE_OPEN_REPARSE_POINT.0 | FILE_OPEN_FOR_BACKUP_INTENT.0 | kind,
        FILE_ATTRIBUTE_NORMAL.0,
        None,
    )
}

/// Mark the object behind `h` for deletion, also when it is read-only. With POSIX semantics (the name is gone at
/// once, even while another program still has the file open with delete sharing, which is what `remove_dir_all`
/// did and what lets the folder around it go); on a volume that does not know them, the classic form (the name goes
/// when the last handle is closed).
fn mark_for_deletion(h: HANDLE) -> io::Result<()> {
    #[repr(C)]
    struct DispositionInfoEx {
        flags: u32,
    }
    let set = |flags: u32| {
        let info = DispositionInfoEx { flags };
        unsafe {
            SetFileInformationByHandle(
                h,
                FileDispositionInfoEx,
                &info as *const DispositionInfoEx as *const c_void,
                std::mem::size_of::<DispositionInfoEx>() as u32,
            )
        }
        .map_err(win_err)
    };
    // FILE_DISPOSITION_FLAG_DELETE | _POSIX_SEMANTICS | _IGNORE_READONLY_ATTRIBUTE
    match set(0x1 | 0x2 | 0x10) {
        // ERROR_INVALID_PARAMETER, ERROR_INVALID_FUNCTION, ERROR_NOT_SUPPORTED: no POSIX semantics here
        Err(e) if matches!(e.raw_os_error(), Some(1) | Some(87) | Some(50)) => set(0x1 | 0x10),
        other => other,
    }
}

/// The names (with the kind) of everything directly inside the folder `dir`, which must be open with
/// `FILE_LIST_DIRECTORY`.
fn list_entries(dir: HANDLE) -> io::Result<Vec<(Vec<u16>, Entry)>> {
    let mut out = Vec::new();
    let mut buf = vec![0u64; 16 * 1024 / 8];
    let mut class = FileFullDirectoryRestartInfo;
    loop {
        let r = unsafe {
            GetFileInformationByHandleEx(
                dir,
                class,
                buf.as_mut_ptr() as *mut c_void,
                (buf.len() * 8) as u32,
            )
        };
        if let Err(e) = r {
            // ERROR_NO_MORE_FILES: the listing is complete
            if e.code().0 & 0xFFFF == 18 {
                break;
            }
            return Err(win_err(e));
        }
        class = FileFullDirectoryInfo;
        let base = buf.as_ptr() as *const u8;
        let mut at = 0usize;
        loop {
            // SAFETY: the kernel filled `buf` with FILE_FULL_DIR_INFO records chained by NextEntryOffset.
            let rec = unsafe { &*(base.add(at) as *const FILE_FULL_DIR_INFO) };
            let name_at = at + std::mem::offset_of!(FILE_FULL_DIR_INFO, FileName);
            let chars = rec.FileNameLength as usize / 2;
            let name: Vec<u16> = (0..chars)
                .map(|i| unsafe {
                    std::ptr::read_unaligned((base.add(name_at) as *const u16).add(i))
                })
                .collect();
            let dot = |c: u16| c == b'.' as u16;
            if !(name.iter().all(|&c| dot(c)) && name.len() <= 2) {
                out.push((
                    name,
                    Entry {
                        directory: rec.FileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0,
                    },
                ));
            }
            if rec.NextEntryOffset == 0 {
                break;
            }
            at += rec.NextEntryOffset as usize;
        }
    }
    Ok(out)
}

fn name_of(w: &[u16]) -> String {
    String::from_utf16_lossy(w)
}

/// Delete the entry `name` of `parent` (the absolute `name` when there is none). A folder is emptied first, through
/// its own handle; a link is deleted as a link and never entered. `Ok(false)` when it was not there.
///
/// Everything inside is attempted even when something fails; the first failure is returned (a folder that could not
/// be emptied stays, with what is in it).
fn remove_node(
    parent: Option<HANDLE>,
    name: &[u16],
    shown: &Path,
    entry: Option<Entry>,
    top: Option<&Top<'_>>,
    depth: usize,
) -> Result<bool, RemoveError> {
    let fail = |error: io::Error| RemoveError::new(shown, error);
    if depth > MAX_DEPTH {
        return Err(fail(refused("nested too deeply")));
    }
    // 1. what is it? An open for attributes only never breaks anybody's lock
    let entry = match entry {
        Some(e) => e,
        None => {
            let probe = match open_entry(parent, name, ACCESS_READ_ATTRIBUTES, None) {
                Ok(h) => h,
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
                Err(e) => return Err(fail(e)),
            };
            Entry {
                directory: kind(raw(&probe)).map_err(fail)?.directory,
            }
        }
    };
    // 2. open it for deletion (and listing, for a folder): the one handle the rest is done through. `DELETE` is
    // asked for together with the rights to read its attributes and to list it, so an object whose own permissions
    // give the caller no `DELETE` is not deleted (the plain `DeleteFileW` could, through the delete-child right of
    // the folder it is in; such an object is left and named, see `remove_all_reporting`)
    let extra = top.map_or(0, |t| t.access);
    let access = ACCESS_DELETE
        | ACCESS_READ_ATTRIBUTES
        | extra
        | if entry.directory {
            ACCESS_LIST_DIRECTORY
        } else {
            0
        };
    let h = match open_entry(parent, name, access, Some(entry)) {
        Ok(h) => h,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(fail(e)),
    };
    let k = kind(raw(&h)).map_err(fail)?;
    if k.directory != entry.directory {
        return Err(fail(refused("it changed while it was being deleted")));
    }
    if let Some(t) = top {
        (t.check)(raw(&h)).map_err(fail)?;
    }
    // 3. a real folder (not a link) is emptied first
    if k.directory && !k.reparse_point {
        if let Some(e) = first_of(remove_children_of(raw(&h), shown, depth)) {
            return Err(e);
        }
    }
    // 4. the entry itself, as itself: a junction or symbolic link is removed and what it points to is not touched
    mark_for_deletion(raw(&h)).map_err(fail)?;
    Ok(true)
}
/// Delete everything inside the open folder `dir` (shown as `shown`), through handles relative to it. Returns the
/// entries that could not be deleted (empty when all went).
fn remove_children_of(dir: HANDLE, shown: &Path, depth: usize) -> Vec<RemoveError> {
    let entries = match list_entries(dir) {
        Ok(e) => e,
        Err(error) => return vec![RemoveError::new(shown, error)],
    };
    let mut failed = Vec::new();
    for (name, entry) in entries {
        let child = shown.join(name_of(&name));
        if let Err(e) = remove_node(Some(dir), &name, &child, Some(entry), None, depth + 1) {
            failed.push(e);
        }
    }
    failed
}

/// A check on the handle of the top entry of [`remove_tree_checked`], made on the handle that is then deleted
/// through.
pub struct Top<'a> {
    /// Extra access the check needs (for example `READ_CONTROL` to read the owner).
    pub access: u32,
    /// An `Err` stops the deletion before anything is changed.
    pub check: &'a dyn Fn(HANDLE) -> io::Result<()>,
}

/// Delete `path` and everything in it without waiting for any program and without following a link anywhere: a
/// link (junction, symbolic link, any reparse point) is removed itself and what it points to is left alone; a
/// folder is walked through handles relative to the one that is held; a read-only file loses the flag with it. A
/// missing path is not an error.
///
/// When a file or folder cannot go (another program holds an opportunistic lock on it, or has it open without
/// sharing delete) the others are still deleted and the first failure is returned, with the path that is in the way
/// (see [`RemoveError::locked`]).
pub fn remove_tree(path: &Path) -> Result<(), RemoveError> {
    remove_top(path, None)
}

/// [`remove_tree`] that first runs `top.check` on the handle of `path` itself, before anything is changed.
pub fn remove_tree_checked(path: &Path, top: &Top<'_>) -> Result<(), RemoveError> {
    remove_top(path, Some(top))
}

fn remove_top(path: &Path, top: Option<&Top<'_>>) -> Result<(), RemoveError> {
    let name = nt_path(path).map_err(|error| RemoveError::new(path, error))?;
    remove_node(None, &name, path, None, top, 0).map(|_| ())
}

/// Delete everything inside the folder open as `dir` (which needs `FILE_LIST_DIRECTORY`), keeping the folder: for
/// a folder that cannot be moved or deleted itself. `Err` describes what could not be deleted (the first entry,
/// and how many more); everything else is gone.
pub fn remove_children(dir: &OwnedHandle, shown: &Path) -> Result<(), RemoveError> {
    match first_of(remove_children_of(raw(dir), shown, 0)) {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// The access an owner check needs on the handle of [`Top`].
pub const ACCESS_OWNER_CHECK: u32 = ACCESS_READ_CONTROL;

/// Rename the file or folder `from` (itself, never what a link at its end points to) to `to`, which must not exist,
/// without waiting for any program: opened with `DELETE` and `FILE_COMPLETE_IF_OPLOCKED`, then renamed through that
/// handle. A lock that another program holds gives the refusal of [`open_with`]; a file that another program has
/// open without sharing delete (or a folder with such a file in it) gives the OS error.
pub fn rename_no_wait(from: &Path, to: &Path) -> io::Result<()> {
    let h = nt_open(
        from,
        ACCESS_DELETE | ACCESS_READ_ATTRIBUTES,
        SHARE_ALL,
        FILE_OPEN,
        FILE_OPEN_REPARSE_POINT.0 | FILE_OPEN_FOR_BACKUP_INTENT.0,
        FILE_ATTRIBUTE_NORMAL.0,
        None,
    )?;
    // FILE_RENAME_INFO: a flag word (ReplaceIfExists), the root folder (none: `FileName` is a full path), the length
    // of the name in bytes and the name. A full path in the Win32 form is what SetFileInformationByHandle takes.
    let target = to_absolute(to)?;
    let units: Vec<u16> = target.encode_wide().collect();
    let header = std::mem::offset_of!(FILE_RENAME_INFO, FileName);
    let size = header + units.len() * 2 + 2;
    let mut buf = vec![0u64; size.div_ceil(8)];
    let base = buf.as_mut_ptr() as *mut u8;
    // SAFETY: `buf` is large enough for the header and the name, and 8-aligned like FILE_RENAME_INFO.
    unsafe {
        let info = base as *mut FILE_RENAME_INFO;
        // no flags: do not replace an existing name
        (*info).Anonymous.Flags = 0;
        (*info).RootDirectory = HANDLE::default();
        (*info).FileNameLength = (units.len() * 2) as u32;
        std::ptr::copy_nonoverlapping(units.as_ptr(), base.add(header) as *mut u16, units.len());
    }
    let handle = raw(&h).0 as usize;
    let buf = &buf;
    // The open above never waits, but the rename itself can: a folder cannot be renamed while a program holds a
    // read-write-handle oplock on a file somewhere below it (the rename must break that lock first, and the
    // create flag cannot ask not to wait for that). The call is given a bound and cancelled when it is exceeded.
    bounded(RENAME_WAIT, move || {
        // SAFETY: the handle outlives the scope that runs this; `buf` holds a complete FILE_RENAME_INFO.
        unsafe {
            SetFileInformationByHandle(
                HANDLE(handle as *mut c_void),
                FileRenameInfo,
                buf.as_ptr() as *const c_void,
                size as u32,
            )
        }
        .map_err(win_err)
    })
}

/// How long a rename may wait for the holder of an oplock to answer a break before it is given up.
const RENAME_WAIT: std::time::Duration = std::time::Duration::from_millis(1500);

/// Run `job` (one synchronous system call that may wait for another program) and give up after `bound`: the call is
/// cancelled (`CancelSynchronousIo`; the operation is aborted, nothing is changed) and the result is the refusal of
/// [`open_with`] ("another program holds a lock on the file"). A call that was answered in time returns its own
/// result.
fn bounded<T: Send>(
    bound: std::time::Duration,
    job: impl FnOnce() -> io::Result<T> + Send,
) -> io::Result<T> {
    std::thread::scope(|scope| {
        let (done, finished) = std::sync::mpsc::channel::<()>();
        let (hand, handed) = std::sync::mpsc::channel::<usize>();
        let worker = scope.spawn(move || {
            // a real handle to this thread, for the one that waits to cancel its call with
            // SAFETY: plain system calls; the handle is closed by the waiting thread.
            let me = unsafe { OpenThread(THREAD_TERMINATE, false, GetCurrentThreadId()) };
            let _ = hand.send(me.map_or(0, |h| h.0 as usize));
            let r = job();
            let _ = done.send(());
            r
        });
        let thread = handed.recv().unwrap_or(0);
        // an error here is the timeout; a closed channel (the job panicked) ends the wait too
        if matches!(
            finished.recv_timeout(bound),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ) && thread != 0
        {
            // SAFETY: the thread handle stays valid until it is closed below.
            unsafe {
                let _ = CancelSynchronousIo(HANDLE(thread as *mut c_void));
            }
        }
        let result = worker.join();
        if thread != 0 {
            // SAFETY: opened by the worker above, closed once.
            unsafe {
                let _ = CloseHandle(HANDLE(thread as *mut c_void));
            }
        }
        match result {
            // ERROR_OPERATION_ABORTED: our own cancel
            Ok(Err(e)) if e.raw_os_error() == Some(995) => Err(refused(OPLOCK_BUSY)),
            Ok(r) => r,
            Err(_) => Err(io::Error::other("the system call panicked")),
        }
    })
}

/// The absolute form of `p` (`GetFullPathNameW`), without a trailing nul.
fn to_absolute(p: &Path) -> io::Result<std::ffi::OsString> {
    use std::os::windows::ffi::OsStringExt;
    let w = wide(p);
    let mut buf = vec![0u16; 512];
    loop {
        let n = unsafe { GetFullPathNameW(PCWSTR(w.as_ptr()), Some(&mut buf), None) } as usize;
        if n == 0 {
            return Err(io::Error::last_os_error());
        }
        if n < buf.len() {
            return Ok(std::ffi::OsString::from_wide(&buf[..n]));
        }
        buf = vec![0u16; n + 1];
    }
}

/// Delete the plain file `path` (a link, a folder or a missing file is not deleted) without waiting for anybody: the
/// file is opened like [`open_with`] does and marked for deletion through that handle. Returns the refusal of
/// [`open_with`] when someone holds an oplock on it.
pub fn remove_plain_file(path: &Path) -> io::Result<()> {
    // DELETE and attribute access, no data access
    let h = open_with(path, 0x0001_0000 | 0x80, SHARE_ALL, OPEN_EXISTING, false, 0)?;
    let k = kind(raw(&h))?;
    if k.reparse_point || k.directory {
        return Err(refused("not a plain file"));
    }
    #[repr(C)]
    struct DispositionInfo {
        delete_file: u8,
    }
    let info = DispositionInfo { delete_file: 1 };
    unsafe {
        SetFileInformationByHandle(
            raw(&h),
            FileDispositionInfo,
            &info as *const DispositionInfo as *const c_void,
            std::mem::size_of::<DispositionInfo>() as u32,
        )
    }
    .map_err(win_err)
}

/// What an opened handle is, from `GetFileInformationByHandle`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Kind {
    pub reparse_point: bool,
    pub directory: bool,
    pub links: u32,
    /// `dwFileAttributes` as Windows reports them.
    pub attributes: u32,
}

/// The kind of the object behind `h`.
pub fn kind(h: HANDLE) -> io::Result<Kind> {
    let i = info(h)?;
    Ok(Kind {
        reparse_point: i.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0,
        directory: i.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0,
        links: i.nNumberOfLinks,
        attributes: i.dwFileAttributes,
    })
}

/// Names of the data streams of the file behind `h` (`::$DATA` is the file's own contents; anything else is an extra
/// stream, `:name:$DATA`). `h` needs `FILE_READ_ATTRIBUTES`.
pub fn stream_names(h: HANDLE) -> io::Result<Vec<String>> {
    let mut buf = vec![0u64; 4096 / 8];
    loop {
        let r = unsafe {
            GetFileInformationByHandleEx(
                h,
                FileStreamInfo,
                buf.as_mut_ptr() as *mut c_void,
                (buf.len() * 8) as u32,
            )
        };
        match r {
            Ok(()) => break,
            Err(e) => match e.code().0 & 0xFFFF {
                // ERROR_HANDLE_EOF: a folder without a named stream
                38 => return Ok(Vec::new()),
                // ERROR_MORE_DATA, ERROR_INSUFFICIENT_BUFFER: more streams than fit
                234 | 122 if buf.len() < (1 << 20) / 8 => buf = vec![0u64; buf.len() * 4],
                _ => return Err(win_err(e)),
            },
        }
    }
    let base = buf.as_ptr() as *const u8;
    let name_offset = std::mem::offset_of!(FILE_STREAM_INFO, StreamName);
    let mut out = Vec::new();
    let mut at = 0usize;
    loop {
        // SAFETY: the kernel filled `buf` with FILE_STREAM_INFO records chained by NextEntryOffset (8-aligned).
        let rec = unsafe { &*(base.add(at) as *const FILE_STREAM_INFO) };
        let chars = rec.StreamNameLength as usize / 2;
        let name: Vec<u16> = (0..chars)
            .map(|i| unsafe {
                std::ptr::read_unaligned((base.add(at + name_offset) as *const u16).add(i))
            })
            .collect();
        out.push(String::from_utf16_lossy(&name));
        if rec.NextEntryOffset == 0 {
            break;
        }
        at += rec.NextEntryOffset as usize;
    }
    Ok(out)
}

/// The data streams of the file behind `h` other than its own contents.
pub fn extra_streams(h: HANDLE) -> io::Result<Vec<String>> {
    Ok(stream_names(h)?
        .into_iter()
        .filter(|n| !n.eq_ignore_ascii_case("::$DATA"))
        .collect())
}

/// `FILE_ATTRIBUTE_SPARSE_FILE`.
pub const ATTRIBUTE_SPARSE: u32 = 0x200;
/// `FILE_ATTRIBUTE_COMPRESSED`.
pub const ATTRIBUTE_COMPRESSED: u32 = 0x800;

/// Make the file behind `h` neither compressed nor sparse again, in place (the contents stay). `h` needs
/// `FILE_READ_DATA | FILE_WRITE_DATA` (what compression needs). A sparse file with holes loses the flag too: the
/// holes become zeros that have storage. `Ok(true)` when something was changed; an error means the caller must
/// replace the file.
pub fn clear_sparse_and_compression(h: HANDLE) -> io::Result<bool> {
    let attrs = info(h)?.dwFileAttributes;
    let mut changed = false;
    if attrs & ATTRIBUTE_COMPRESSED != 0 {
        set_compressed(h, false)?;
        changed = true;
    }
    if attrs & ATTRIBUTE_SPARSE != 0 {
        set_sparse(h, false)?;
        changed = true;
    }
    let now = info(h)?.dwFileAttributes;
    if now & (ATTRIBUTE_SPARSE | ATTRIBUTE_COMPRESSED) != 0 {
        return Err(io::Error::other(
            "the file is still sparse or compressed after the flags were cleared",
        ));
    }
    Ok(changed)
}

fn ioctl(h: HANDLE, code: u32, input: *const c_void, len: u32) -> io::Result<()> {
    let mut returned = 0u32;
    unsafe {
        DeviceIoControl(
            h,
            code,
            Some(input),
            len,
            None,
            0,
            Some(&mut returned),
            None,
        )
    }
    .map_err(win_err)
}

/// `FSCTL_SET_COMPRESSION` (the default format when `on`, none when not). Needs `FILE_READ_DATA | FILE_WRITE_DATA`.
pub fn set_compressed(h: HANDLE, on: bool) -> io::Result<()> {
    // COMPRESSION_FORMAT_DEFAULT is 1, COMPRESSION_FORMAT_NONE 0
    let format: u16 = on.into();
    ioctl(
        h,
        FSCTL_SET_COMPRESSION,
        &format as *const u16 as *const c_void,
        2,
    )
}

/// `FSCTL_SET_SPARSE`. Needs `FILE_WRITE_DATA` (or `FILE_WRITE_ATTRIBUTES`).
pub fn set_sparse(h: HANDLE, on: bool) -> io::Result<()> {
    let buffer = FILE_SET_SPARSE_BUFFER { SetSparse: on };
    ioctl(
        h,
        FSCTL_SET_SPARSE,
        &buffer as *const FILE_SET_SPARSE_BUFFER as *const c_void,
        std::mem::size_of::<FILE_SET_SPARSE_BUFFER>() as u32,
    )
}

/// Share mode of the frame file: everybody may read, write and delete it while it is open.
pub const SHARE_ALL: FILE_SHARE_MODE =
    FILE_SHARE_MODE(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0);

/// Share mode that lets others read and write but not delete or rename (a folder held while it is worked in).
pub const SHARE_READ_WRITE: FILE_SHARE_MODE =
    FILE_SHARE_MODE(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0);

/// The name `h` really has (`\\?\C:\...`), links resolved, via `GetFinalPathNameByHandleW`.
pub fn final_path(h: HANDLE) -> io::Result<String> {
    // both constants are 0: normalized name, DOS volume name
    let flags = GETFINALPATHNAMEBYHANDLE_FLAGS(FILE_NAME_NORMALIZED.0 | VOLUME_NAME_DOS.0);
    let mut buf = vec![0u16; 512];
    for _ in 0..3 {
        let n = unsafe { GetFinalPathNameByHandleW(h, &mut buf, flags) } as usize;
        if n == 0 {
            return Err(io::Error::last_os_error());
        }
        if n < buf.len() {
            return Ok(String::from_utf16_lossy(&buf[..n]));
        }
        buf = vec![0u16; n + 1];
    }
    Err(io::Error::other("final path keeps growing"))
}

/// The final path of the directory `dir`, every link in it *followed* (also a link at its end): a root that an
/// administrator controls may be a junction or symbolic link to another place, and files below it then have their
/// final path below the place it leads to. Never use it for a path a standard user can change.
pub fn canonical_dir(dir: &Path) -> io::Result<String> {
    // FILE_READ_ATTRIBUTES is enough to ask for a final path; the open follows (no FILE_OPEN_REPARSE_POINT)
    let h = nt_open(
        dir,
        0x80,
        SHARE_ALL,
        FILE_OPEN,
        FILE_OPEN_FOR_BACKUP_INTENT.0,
        FILE_ATTRIBUTE_NORMAL.0,
        None,
    )?;
    final_path(raw(&h))
}

/// The final path a file at `path` must have when `root` is trusted: the canonical `root` followed by the
/// components of `path` below it. Refuses a `path` that is not below `root` or that contains `..`.
pub fn expected_path(root: &Path, path: &Path) -> io::Result<String> {
    let rel = path
        .strip_prefix(root)
        .map_err(|_| refused("path is not below the trusted root"))?;
    let mut s = canonical_dir(root)?;
    while s.ends_with('\\') {
        s.pop();
    }
    for c in rel.components() {
        match c {
            Component::Normal(n) => {
                s.push('\\');
                s.push_str(&n.to_string_lossy());
            }
            Component::CurDir => {}
            _ => return Err(refused("path has a parent or root component")),
        }
    }
    Ok(s)
}

/// True when `final_path` (`\\?\X:\...`) lies on a local fixed disk: not a network share, not removable media.
pub fn is_fixed_local(final_path: &str) -> bool {
    let b = final_path.as_bytes();
    if !(final_path.starts_with(r"\\?\") && b.len() >= 7 && b[5] == b':' && b[6] == b'\\') {
        return false; // \\?\UNC\..., \\?\Volume{...}\ and everything else are not accepted
    }
    let root: Vec<u16> = format!("{}:\\", b[4] as char)
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    unsafe { GetDriveTypeW(PCWSTR(root.as_ptr())) == DRIVE_FIXED }
}

fn info(h: HANDLE) -> io::Result<BY_HANDLE_FILE_INFORMATION> {
    let mut i = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(h, &mut i) }.map_err(win_err)?;
    Ok(i)
}

fn check_place(h: HANDLE, expect: Option<&str>) -> io::Result<()> {
    if let Some(want) = expect {
        let got = final_path(h)?;
        if got.to_lowercase() != want.to_lowercase() {
            return Err(refused(format!("opened {got}, expected {want}")));
        }
        if !is_fixed_local(&got) {
            return Err(refused(format!("{got} is not on a local fixed disk")));
        }
    }
    Ok(())
}

/// The final path of `h` must be exactly `expect` (case aside) on a local fixed disk: no link anywhere in the way.
pub fn verify_place(h: HANDLE, expect: &str) -> io::Result<()> {
    check_place(h, Some(expect))
}

/// Check an opened file: a plain file (no reparse point, not a directory) with exactly one hard link, and, with
/// `expect`, the final path `expect` on a local fixed disk.
pub fn verify_file(h: HANDLE, expect: Option<&str>) -> io::Result<()> {
    let i = info(h)?;
    if i.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
        return Err(refused("it is a link (reparse point)"));
    }
    if i.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0 {
        return Err(refused("it is a directory"));
    }
    if i.nNumberOfLinks != 1 {
        return Err(refused(format!("it has {} hard links", i.nNumberOfLinks)));
    }
    check_place(h, expect)
}

/// Check an opened directory: not a reparse point, and, with `expect`, the final path `expect` on a local fixed
/// disk.
pub fn verify_dir(h: HANDLE, expect: Option<&str>) -> io::Result<()> {
    let i = info(h)?;
    if i.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
        return Err(refused("it is a link (reparse point)"));
    }
    if i.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0 {
        return Err(refused("it is not a directory"));
    }
    check_place(h, expect)
}

/// Open an existing file without following a link and verify it. With `pin = Some(root)` the file must really be
/// `path` below the canonical `root` (see the module docs).
pub fn open_existing(path: &Path, write: bool, pin: Option<&Path>) -> io::Result<OwnedHandle> {
    let expect = match pin {
        Some(root) => Some(expected_path(root, path)?),
        None => None,
    };
    let h = open_no_follow(path, write, false, SHARE_ALL)?;
    verify_file(raw(&h), expect.as_deref())?;
    Ok(h)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh folder under the temp dir, with the temp dir itself as trusted root.
    fn sandbox(name: &str) -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("bettercam-secure-{}", std::process::id()));
        let dir = root.join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        (root, dir)
    }

    fn junction(link: &Path, target: &Path) {
        let out = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "mklink /J failed: {}",
            String::from_utf8_lossy(&out.stdout)
        );
    }

    fn remove_junction(link: &Path) {
        // removes the junction only, never what it points to
        let _ = std::fs::remove_dir(link);
    }

    #[test]
    fn program_data_comes_from_the_shell_and_exists() {
        let d = program_data_dir();
        assert!(d.is_absolute() && d.is_dir(), "{}", d.display());
        assert!(is_fixed_local(&canonical_dir(&d).unwrap()));
    }

    #[test]
    fn a_plain_file_below_the_root_is_accepted() {
        let (root, dir) = sandbox("plain");
        let f = dir.join("frames.bin");
        std::fs::write(&f, b"x").unwrap();
        open_existing(&f, false, Some(&root)).unwrap();
        open_existing(&f, true, Some(&root)).unwrap();
        open_existing(&f, false, None).unwrap();
    }

    #[test]
    fn a_junction_in_the_path_is_refused() {
        let (root, dir) = sandbox("junction");
        let real = dir.join("real");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("frames.bin"), b"x").unwrap();
        let link = dir.join("link");
        junction(&link, &real);
        let via_link = link.join("frames.bin");
        // reachable by path, but not the file the path names
        assert!(std::fs::read(&via_link).is_ok());
        let e = open_existing(&via_link, false, Some(&root)).unwrap_err();
        assert!(is_refused(&e), "{e}");
        // without a pin the handle looks like a plain file: that is why the DLL always pins
        assert!(open_existing(&via_link, false, None).is_ok());
        remove_junction(&link);
        assert!(real.join("frames.bin").exists(), "the target must survive");
    }

    #[test]
    fn a_hard_linked_file_is_refused() {
        let (root, dir) = sandbox("hardlink");
        let f = dir.join("frames.bin");
        std::fs::write(&f, b"x").unwrap();
        open_existing(&f, false, Some(&root)).unwrap();
        let other = dir.join("other.bin");
        std::fs::hard_link(&f, &other).unwrap();
        let e = open_existing(&f, false, Some(&root)).unwrap_err();
        assert!(is_refused(&e), "{e}");
        let e = open_existing(&other, false, None).unwrap_err();
        assert!(is_refused(&e), "{e}");
    }

    #[test]
    fn a_directory_is_not_a_file() {
        let (root, dir) = sandbox("dir");
        let d = dir.join("frames.bin");
        std::fs::create_dir_all(&d).unwrap();
        // CreateFileW without backup semantics cannot open a directory at all
        assert!(open_existing(&d, false, Some(&root)).is_err());
        let h = open_no_follow(&d, false, true, SHARE_ALL).unwrap();
        assert!(is_refused(&verify_file(raw(&h), None).unwrap_err()));
        verify_dir(raw(&h), None).unwrap();
    }

    #[test]
    fn a_symbolic_link_to_a_file_is_refused_when_one_can_be_made() {
        let (root, dir) = sandbox("symlink");
        let real = dir.join("real.bin");
        std::fs::write(&real, b"x").unwrap();
        let link = dir.join("frames.bin");
        if std::os::windows::fs::symlink_file(&real, &link).is_err() {
            eprintln!("no privilege to create a symbolic link: skipped");
            return;
        }
        let e = open_existing(&link, false, Some(&root)).unwrap_err();
        assert!(is_refused(&e), "{e}");
        let e = open_existing(&link, false, None).unwrap_err();
        assert!(is_refused(&e), "{e}");
    }

    #[test]
    fn the_path_must_lie_below_the_root_without_dots() {
        let (root, dir) = sandbox("dots");
        let f = dir.join("a.bin");
        std::fs::write(&f, b"x").unwrap();
        let outside = std::env::temp_dir().join("elsewhere.bin");
        assert!(is_refused(&expected_path(&root, &outside).unwrap_err()));
        let dotted = dir.join("..").join("dots").join("a.bin");
        assert!(is_refused(
            &open_existing(&dotted, false, Some(&root)).unwrap_err()
        ));
    }

    /// A `%ProgramData%` that is itself a junction (some people move it to another drive): the root resolves
    /// through the link, files below it resolve to the real place, and the two agree. A link *below* the root is
    /// still refused.
    #[test]
    fn a_root_that_is_a_junction_is_followed_and_links_below_it_are_not() {
        let (base, dir) = sandbox("junction-root");
        let real = dir.join("real-programdata");
        std::fs::create_dir_all(real.join("BetterCam")).unwrap();
        std::fs::write(real.join("BetterCam").join("frames.bin"), b"x").unwrap();
        let root = dir.join("ProgramData");
        junction(&root, &real);
        let file = root.join("BetterCam").join("frames.bin");

        // the root's final path is the real place, and a file below the root is expected exactly there
        assert_eq!(
            canonical_dir(&root).unwrap().to_lowercase(),
            canonical_dir(&real).unwrap().to_lowercase()
        );
        let want = expected_path(&root, &file).unwrap();
        assert!(want.to_lowercase().contains("real-programdata"), "{want}");
        open_existing(&file, false, Some(&root)).expect("a file below a junction root is accepted");
        open_existing(&file, true, Some(&root)).expect("also for writing");

        // a junction *below* the root still fails the comparison
        let elsewhere = dir.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("frames.bin"), b"x").unwrap();
        let inner = real.join("BetterCam").join("link");
        junction(&inner, &elsewhere);
        let via = root.join("BetterCam").join("link").join("frames.bin");
        let e = open_existing(&via, false, Some(&root)).unwrap_err();
        assert!(is_refused(&e), "{e}");
        remove_junction(&inner);
        remove_junction(&root);
        assert!(real.join("BetterCam").join("frames.bin").exists());
        let _ = base;
    }

    /// An open must not wait for a program that holds an oplock: a standard user may read the frame file and the
    /// logs, and a read-write-handle oplock taken through that read access blocks every other open (and delete)
    /// until the holder lets go. With `FILE_COMPLETE_IF_OPLOCKED` the open returns at once, as a refusal.
    #[test]
    fn an_open_does_not_wait_for_a_program_that_holds_an_oplock() {
        use std::sync::mpsc;
        use std::time::{Duration, Instant};
        let (root, dir) = sandbox("oplock");
        let f = dir.join("frames.bin");
        std::fs::write(&f, vec![0u8; 8192]).unwrap();
        let Some(lock) = oplock::Oplock::take(&f).unwrap() else {
            eprintln!("this file system grants no oplock: skipped");
            return;
        };

        // control: a plain open of the file blocks while the lock is held (this is what the old code did)
        let (tx, rx) = mpsc::channel();
        let p = f.clone();
        let control = std::thread::spawn(move || {
            let t = Instant::now();
            let r = std::fs::File::open(&p).map(|_| ());
            let _ = tx.send(t.elapsed());
            r
        });
        if rx.recv_timeout(Duration::from_millis(700)).is_ok() {
            eprintln!("a plain open was not held up by the oplock: skipped");
            return;
        }

        // the checked opens return at once, as a refusal, for reading and writing, with and without a pin. Each
        // runs on its own thread so that an open that waits fails the test instead of hanging it.
        let bound = Duration::from_secs(2);
        let within = |what: &str, job: Box<dyn FnOnce() -> io::Result<()> + Send>| {
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(job());
            });
            match rx.recv_timeout(bound) {
                Ok(r) => r,
                Err(_) => {
                    panic!("{what}: still waiting after {bound:?} (the open waits for the oplock)")
                }
            }
        };
        for (write, pinned) in [(false, false), (true, false), (false, true), (true, true)] {
            let (p, r) = (f.clone(), root.clone());
            let e = within(
                &format!("open, write {write}, pinned {pinned}"),
                Box::new(move || {
                    open_existing(&p, write, pinned.then_some(r.as_path())).map(|_| ())
                }),
            )
            .unwrap_err();
            assert!(is_refused(&e), "write {write}: {e}");
        }
        let p = f.clone();
        let e = within("delete", Box::new(move || remove_plain_file(&p))).unwrap_err();
        assert!(is_refused(&e), "{e}");
        assert!(f.exists(), "a refused delete must leave the file");

        // the holder gives up: the held-up plain open completes, and ours works again
        drop(lock);
        let waited = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the held-up open ended");
        assert!(waited >= Duration::from_millis(600), "{waited:?}");
        control.join().unwrap().unwrap();
        open_existing(&f, true, Some(&root)).unwrap();
        remove_plain_file(&f).unwrap();
        assert!(!f.exists());
    }

    #[test]
    fn a_plain_file_is_deleted_and_a_link_or_folder_is_not() {
        let (_root, dir) = sandbox("remove-plain");
        let f = dir.join("a.log");
        std::fs::write(&f, b"x").unwrap();
        remove_plain_file(&f).unwrap();
        assert!(!f.exists());
        assert!(
            remove_plain_file(&f).is_err(),
            "a missing file is an error here"
        );
        let d = dir.join("folder");
        std::fs::create_dir_all(&d).unwrap();
        assert!(remove_plain_file(&d).is_err());
        assert!(d.exists());
        let real = dir.join("real");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("keep.txt"), b"x").unwrap();
        let link = dir.join("link");
        junction(&link, &real);
        assert!(remove_plain_file(&link).is_err());
        assert!(real.join("keep.txt").exists());
        remove_junction(&link);
    }

    /// Run `job` on its own thread and fail the test (instead of hanging it) when it has not returned in `bound`.
    fn within<T: Send + 'static>(
        what: &str,
        bound: std::time::Duration,
        job: impl FnOnce() -> T + Send + 'static,
    ) -> T {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(job());
        });
        rx.recv_timeout(bound)
            .unwrap_or_else(|_| panic!("{what}: still waiting after {bound:?}"))
    }

    /// A handle that comes back with any status but the plain success is closed, and the call is an error: the
    /// oplock break (a success code with a handle), an informational status, a pending one and a warning.
    #[test]
    fn a_handle_that_comes_with_an_unexpected_status_is_closed() {
        use std::cell::Cell;
        let fake = HANDLE(0x1234 as *mut c_void);
        for status in [
            STATUS_OPLOCK_BREAK_IN_PROGRESS.0,
            0x4000_0000,           // STATUS_OBJECT_NAME_EXISTS (informational)
            0x0000_0103,           // STATUS_PENDING
            0x0000_0001,           // some other success code
            0x8000_0005u32 as i32, // STATUS_BUFFER_OVERFLOW (warning)
            0xC000_0022u32 as i32, // STATUS_ACCESS_DENIED (error)
        ] {
            let closed = Cell::new(0);
            let r = finish_open_with(NTSTATUS(status), fake, |h| {
                assert_eq!(h, fake);
                closed.set(closed.get() + 1);
            });
            assert!(r.is_err(), "status {status:#x}");
            assert_eq!(closed.get(), 1, "status {status:#x}: the handle leaked");
        }
        // no handle came back: nothing to close
        let closed = Cell::new(0);
        let r = finish_open_with(NTSTATUS(0xC000_0034u32 as i32), HANDLE::default(), |_| {
            closed.set(closed.get() + 1)
        });
        assert!(r.is_err() && closed.get() == 0);
        // the plain success keeps it
        let (_root, dir) = sandbox("finish-open");
        let f = dir.join("a.bin");
        std::fs::write(&f, b"x").unwrap();
        let h =
            std::os::windows::io::IntoRawHandle::into_raw_handle(std::fs::File::open(&f).unwrap());
        let r = finish_open_with(NTSTATUS(0), HANDLE(h), |_| panic!("closed a good handle"));
        assert!(r.is_ok());
        // the oplock break is told apart
        let e = finish_open_with(STATUS_OPLOCK_BREAK_IN_PROGRESS, fake, |_| {}).unwrap_err();
        assert!(is_oplock_busy(&e) && is_refused(&e));
    }

    /// A tree is deleted through handles: a junction in it goes as a junction, a read-only file goes, and a file
    /// that another program holds an oplock on is left (the others still go), without waiting, with a result that
    /// names it. Once the lock is given up the same call finishes the job.
    #[test]
    fn a_tree_is_deleted_without_following_links_and_a_locked_file_is_left_without_waiting() {
        use std::time::Duration;
        let (_root, dir) = sandbox("remove-tree");
        let outside = dir.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("keep.txt"), b"keep").unwrap();
        let tree = dir.join("tree");
        std::fs::create_dir_all(tree.join("a").join("deep")).unwrap();
        std::fs::write(tree.join("a").join("deep").join("x.txt"), b"x").unwrap();
        std::fs::write(tree.join("a").join("locked.txt"), vec![0u8; 4096]).unwrap();
        std::fs::write(tree.join("b.txt"), b"b").unwrap();
        let ro = tree.join("ro.txt");
        std::fs::write(&ro, b"r").unwrap();
        let mut perm = std::fs::metadata(&ro).unwrap().permissions();
        perm.set_readonly(true);
        std::fs::set_permissions(&ro, perm).unwrap();
        junction(&tree.join("a").join("link"), &outside);
        let Some(lock) = oplock::Oplock::take(&tree.join("a").join("locked.txt")).unwrap() else {
            eprintln!("this file system grants no oplock: skipped");
            return;
        };

        let t = tree.clone();
        let e = within("remove_tree", Duration::from_secs(5), move || {
            remove_tree(&t)
        })
        .expect_err("a file that is locked cannot be deleted");
        assert!(e.locked(), "{e}");
        assert!(e.path.ends_with("locked.txt"), "{e}");
        assert!(e.to_string().contains("locked by another program"), "{e}");
        assert!(io::Error::from(e).kind() == io::ErrorKind::PermissionDenied);
        // everything else went; the locked file and the folders around it are still there
        let a = tree.join("a");
        assert!(a.join("locked.txt").exists());
        assert!(!a.join("deep").exists() && !a.join("link").exists());
        assert!(!tree.join("b.txt").exists() && !ro.exists());
        // the junction was removed as a junction
        assert_eq!(std::fs::read(outside.join("keep.txt")).unwrap(), b"keep");

        drop(lock);
        remove_tree(&tree).unwrap();
        assert!(!tree.exists());
        assert_eq!(std::fs::read(outside.join("keep.txt")).unwrap(), b"keep");
        // a missing path is fine, and a link given directly goes as a link
        remove_tree(&tree).unwrap();
        junction(&dir.join("top"), &outside);
        remove_tree(&dir.join("top")).unwrap();
        assert!(!dir.join("top").exists());
        assert_eq!(std::fs::read(outside.join("keep.txt")).unwrap(), b"keep");
    }

    /// The count of what stayed is in the error.
    #[test]
    fn the_error_says_how_many_entries_stayed() {
        let (_root, dir) = sandbox("remove-count");
        let tree = dir.join("tree");
        std::fs::create_dir_all(&tree).unwrap();
        let mut held = Vec::new();
        for n in ["one", "two", "three"] {
            let p = tree.join(n);
            std::fs::write(&p, b"x").unwrap();
            // open without delete sharing: the delete fails with a sharing violation (not a lock)
            held.push(
                std::os::windows::fs::OpenOptionsExt::share_mode(
                    std::fs::OpenOptions::new().read(true),
                    3,
                )
                .open(&p)
                .unwrap(),
            );
        }
        std::fs::write(tree.join("free"), b"x").unwrap();
        let e = remove_tree(&tree).unwrap_err();
        assert!(!e.locked(), "{e}");
        assert_eq!(e.left(), 3, "{e}");
        assert!(e.to_string().contains("(and 2 more)"), "{e}");
        assert!(!tree.join("free").exists());
        drop(held);
        remove_tree(&tree).unwrap();
    }

    /// `remove_tree_checked` runs its check on the handle that is deleted through, before anything changes.
    #[test]
    fn a_check_on_the_top_entry_can_stop_the_delete_before_anything_is_changed() {
        let (_root, dir) = sandbox("remove-checked");
        let tree = dir.join("tree");
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::write(tree.join("f.txt"), b"x").unwrap();
        let no = Top {
            access: ACCESS_OWNER_CHECK,
            check: &|_| Err(refused("not yours")),
        };
        let e = remove_tree_checked(&tree, &no).unwrap_err();
        assert!(is_refused(&io::Error::from(e)));
        assert!(tree.join("f.txt").exists());
        let yes = Top {
            access: ACCESS_OWNER_CHECK,
            check: &|_| Ok(()),
        };
        remove_tree_checked(&tree, &yes).unwrap();
        assert!(!tree.exists());
    }

    /// A rename goes through a handle opened like every other open: it does not wait for a program that holds an
    /// oplock on the file, and it renames a junction itself and never what the junction points to.
    #[test]
    fn a_rename_does_not_wait_for_an_oplock_and_never_goes_through_a_link() {
        use std::time::Duration;
        let (_root, dir) = sandbox("rename");
        let f = dir.join("frames.bin");
        std::fs::write(&f, vec![0u8; 4096]).unwrap();
        let to = dir.join("frames.bin.old-1-0");
        if let Some(lock) = oplock::Oplock::take(&f).unwrap() {
            let (a, b) = (f.clone(), to.clone());
            let e = within("rename", Duration::from_secs(2), move || {
                rename_no_wait(&a, &b)
            })
            .unwrap_err();
            assert!(is_oplock_busy(&e), "{e}");
            assert!(f.exists() && !to.exists());
            drop(lock);
        } else {
            eprintln!("this file system grants no oplock: that part is skipped");
        }
        rename_no_wait(&f, &to).unwrap();
        assert!(!f.exists() && to.exists());
        // an existing name is not replaced
        std::fs::write(&f, b"new").unwrap();
        assert!(rename_no_wait(&f, &to).is_err());
        assert_eq!(std::fs::read(&f).unwrap(), b"new");

        let outside = dir.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("keep.txt"), b"keep").unwrap();
        let link = dir.join("link");
        junction(&link, &outside);
        let moved = dir.join("link.old-1-0");
        rename_no_wait(&link, &moved).unwrap();
        assert!(!link.exists());
        assert!(is_reparse(&moved), "the junction itself was renamed");
        assert!(
            outside.join("keep.txt").exists(),
            "the target stayed where it is"
        );
        remove_junction(&moved);
        // a folder
        let folder = dir.join("folder");
        std::fs::create_dir_all(&folder).unwrap();
        rename_no_wait(&folder, &dir.join("folder.old-1-0")).unwrap();
        assert!(dir.join("folder.old-1-0").is_dir());
        // missing
        assert_eq!(
            rename_no_wait(&dir.join("nothing"), &dir.join("x"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
    }

    fn is_reparse(p: &Path) -> bool {
        use std::os::windows::fs::MetadataExt;
        std::fs::symlink_metadata(p)
            .is_ok_and(|m| m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0)
    }

    fn open_attributes(p: &Path) -> OwnedHandle {
        open_with(p, ACCESS_READ_ATTRIBUTES, SHARE_ALL, OPEN_EXISTING, true, 0).unwrap()
    }

    #[test]
    fn extra_data_streams_are_listed() {
        let (_root, dir) = sandbox("streams");
        let f = dir.join("frames.bin");
        std::fs::write(&f, b"x").unwrap();
        assert!(extra_streams(raw(&open_attributes(&f))).unwrap().is_empty());
        assert_eq!(
            stream_names(raw(&open_attributes(&f))).unwrap(),
            ["::$DATA"]
        );
        let stream = format!("{}:extra", f.display());
        if std::fs::write(&stream, b"hidden").is_err() {
            eprintln!("this file system has no named streams: skipped");
            return;
        }
        let extra = extra_streams(raw(&open_attributes(&f))).unwrap();
        assert_eq!(extra, [":extra:$DATA"]);
        // a folder without a stream: none, and not an error
        assert!(extra_streams(raw(&open_attributes(&dir)))
            .unwrap()
            .is_empty());
        // many streams
        for i in 0..40 {
            std::fs::write(format!("{}:stream-number-{i}", f.display()), b"x").unwrap();
        }
        assert_eq!(extra_streams(raw(&open_attributes(&f))).unwrap().len(), 41);
    }

    /// A file with the sparse flag, the compressed flag, or both and holes in it, is made plain again through the
    /// handle, and its contents stay.
    #[test]
    fn a_sparse_or_compressed_file_is_made_plain_in_place() {
        use std::io::{Seek, SeekFrom, Write};
        let (_root, dir) = sandbox("sparse");
        let f = dir.join("frames.bin");
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&f)
            .unwrap();
        file.set_len(3 << 20).unwrap();
        file.seek(SeekFrom::Start(100)).unwrap();
        file.write_all(b"OLD FILE").unwrap();
        let h = HANDLE(std::os::windows::io::AsRawHandle::as_raw_handle(&file));
        let attrs = |h| info(h).unwrap().dwFileAttributes;
        let id = |h| {
            let i = info(h).unwrap();
            (i.dwVolumeSerialNumber, i.nFileIndexHigh, i.nFileIndexLow)
        };
        let before = id(h);
        // nothing to do
        assert!(!clear_sparse_and_compression(h).unwrap());

        // sparse, without holes (the flag only), then with holes (a zeroed range)
        if set_sparse(h, true).is_err() {
            eprintln!("cannot make a file sparse here: skipped");
            return;
        }
        assert_ne!(attrs(h) & ATTRIBUTE_SPARSE, 0);
        assert!(clear_sparse_and_compression(h).unwrap());
        assert_eq!(attrs(h) & ATTRIBUTE_SPARSE, 0);

        set_sparse(h, true).unwrap();
        let zero = windows::Win32::System::Ioctl::FILE_ZERO_DATA_INFORMATION {
            FileOffset: 1 << 20,
            BeyondFinalZero: 2 << 20,
        };
        ioctl(
            h,
            windows::Win32::System::Ioctl::FSCTL_SET_ZERO_DATA,
            &zero as *const _ as *const c_void,
            std::mem::size_of_val(&zero) as u32,
        )
        .unwrap();
        assert!(clear_sparse_and_compression(h).unwrap());
        assert_eq!(attrs(h) & ATTRIBUTE_SPARSE, 0, "still sparse");

        // compressed
        if set_compressed(h, true).is_ok() {
            assert_ne!(attrs(h) & ATTRIBUTE_COMPRESSED, 0);
            assert!(clear_sparse_and_compression(h).unwrap());
            assert_eq!(attrs(h) & ATTRIBUTE_COMPRESSED, 0);
        } else {
            eprintln!("cannot compress a file here: that part is skipped");
        }
        // the same file, the same contents and size
        assert_eq!(id(h), before);
        let bytes = std::fs::read(&f).unwrap();
        assert_eq!(bytes.len(), 3 << 20);
        assert_eq!(&bytes[100..108], b"OLD FILE");
        assert!(bytes[108..].iter().all(|&b| b == 0));
    }

    #[test]
    fn nt_paths_are_absolute_and_normalised() {
        let p = nt_path(Path::new(r"C:\a\.\b\..\c.txt")).unwrap();
        assert_eq!(String::from_utf16(&p).unwrap(), r"\??\C:\a\c.txt");
        let p = nt_path(Path::new(r"\\?\C:\a\c.txt")).unwrap();
        assert_eq!(String::from_utf16(&p).unwrap(), r"\??\C:\a\c.txt");
        let p = nt_path(Path::new(r"\\srv\share\c.txt")).unwrap();
        assert_eq!(String::from_utf16(&p).unwrap(), r"\??\UNC\srv\share\c.txt");
        assert!(nt_path(Path::new(r"\\.\pipe\x")).is_err());
        let rel = nt_path(Path::new("relative.txt")).unwrap();
        assert!(String::from_utf16(&rel).unwrap().starts_with(r"\??\"));
    }

    #[test]
    fn network_and_odd_final_paths_are_not_local_fixed() {
        assert!(!is_fixed_local(r"\\?\UNC\server\share\x"));
        assert!(!is_fixed_local(r"\\?\Volume{1234}\x"));
        assert!(!is_fixed_local(r"C:\ProgramData"));
        assert!(!is_fixed_local(""));
    }
}
