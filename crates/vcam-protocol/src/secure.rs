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
//! * the trusted root is `%ProgramData%` taken from `SHGetKnownFolderPath`, not from the environment.
//!
//! A refusal is an [`io::Error`] that wraps [`Refused`]; see [`is_refused`].

use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, GetDriveTypeW, GetFileInformationByHandle, GetFinalPathNameByHandleW,
    BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAGS_AND_ATTRIBUTES, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_NAME_NORMALIZED, FILE_SHARE_DELETE, FILE_SHARE_MODE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, GETFINALPATHNAMEBYHANDLE_FLAGS, OPEN_EXISTING,
    VOLUME_NAME_DOS,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::{FOLDERID_ProgramData, SHGetKnownFolderPath, KNOWN_FOLDER_FLAG};

/// `GetDriveTypeW` result for a local fixed disk.
const DRIVE_FIXED: u32 = 3;

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

/// `CreateFileW` with every share right and `FILE_FLAG_OPEN_REPARSE_POINT` (a link is opened, not followed).
/// `directory` adds `FILE_FLAG_BACKUP_SEMANTICS`, which opening a directory needs.
pub fn open_no_follow(
    path: &Path,
    write: bool,
    directory: bool,
    share: FILE_SHARE_MODE,
) -> io::Result<OwnedHandle> {
    let access = if write {
        GENERIC_READ.0 | GENERIC_WRITE.0
    } else {
        GENERIC_READ.0
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
pub fn open_with(
    path: &Path,
    access: u32,
    share: FILE_SHARE_MODE,
    disposition: windows::Win32::Storage::FileSystem::FILE_CREATION_DISPOSITION,
    directory: bool,
    extra_flags: u32,
) -> io::Result<OwnedHandle> {
    let w = wide(path);
    // FILE_ATTRIBUTE_NORMAL is valid only on its own
    let base = if extra_flags == 0 {
        FILE_ATTRIBUTE_NORMAL.0
    } else {
        0
    };
    let mut flags = base | FILE_FLAG_OPEN_REPARSE_POINT.0 | extra_flags;
    if directory {
        flags |= FILE_FLAG_BACKUP_SEMANTICS.0;
    }
    let h = unsafe {
        CreateFileW(
            PCWSTR(w.as_ptr()),
            access,
            share,
            None,
            disposition,
            FILE_FLAGS_AND_ATTRIBUTES(flags),
            None,
        )
    }
    .map_err(win_err)?;
    // SAFETY: CreateFileW returned a valid handle that nothing else owns.
    Ok(unsafe { OwnedHandle::from_raw_handle(h.0) })
}

/// What an opened handle is, from `GetFileInformationByHandle`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Kind {
    pub reparse_point: bool,
    pub directory: bool,
    pub links: u32,
}

/// The kind of the object behind `h`.
pub fn kind(h: HANDLE) -> io::Result<Kind> {
    let i = info(h)?;
    Ok(Kind {
        reparse_point: i.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0,
        directory: i.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0,
        links: i.nNumberOfLinks,
    })
}

/// Share mode of the frame file: everybody may read, write and delete it while it is open.
pub const SHARE_ALL: FILE_SHARE_MODE =
    FILE_SHARE_MODE(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0);

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

/// The final path of the directory `dir` (links in it are followed: use it for a root an administrator controls).
pub fn canonical_dir(dir: &Path) -> io::Result<String> {
    let h = open_no_follow(dir, false, true, SHARE_ALL)?;
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

    #[test]
    fn network_and_odd_final_paths_are_not_local_fixed() {
        assert!(!is_fixed_local(r"\\?\UNC\server\share\x"));
        assert!(!is_fixed_local(r"\\?\Volume{1234}\x"));
        assert!(!is_fixed_local(r"C:\ProgramData"));
        assert!(!is_fixed_local(""));
    }
}
