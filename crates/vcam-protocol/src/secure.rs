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
    FileDispositionInfo, GetDriveTypeW, GetFileInformationByHandle, GetFinalPathNameByHandleW,
    GetFullPathNameW, SetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, CREATE_NEW,
    FILE_ACCESS_RIGHTS, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_CREATION_DISPOSITION, FILE_FLAGS_AND_ATTRIBUTES,
    FILE_NAME_NORMALIZED, FILE_SHARE_DELETE, FILE_SHARE_MODE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    GETFINALPATHNAMEBYHANDLE_FLAGS, OPEN_ALWAYS, OPEN_EXISTING, SYNCHRONIZE, VOLUME_NAME_DOS,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::IO::IO_STATUS_BLOCK;
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
    let mut name = nt_path(path)?;
    let bytes = (name.len() * 2) as u16;
    let us = UNICODE_STRING {
        Length: bytes,
        MaximumLength: bytes,
        Buffer: PWSTR(name.as_mut_ptr()),
    };
    let oa = OBJECT_ATTRIBUTES {
        Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: HANDLE::default(),
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
    if status == NTSTATUS(0) {
        // SAFETY: NtCreateFile returned a valid handle that nothing else owns.
        return Ok(unsafe { OwnedHandle::from_raw_handle(h.0) });
    }
    if status == STATUS_OPLOCK_BREAK_IN_PROGRESS {
        // success code with a handle: the open went through, but only after asking a holder to give up its lock
        unsafe {
            let _ = CloseHandle(h);
        }
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
