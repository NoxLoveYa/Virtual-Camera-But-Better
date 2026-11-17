//! Helpers shared by the tests of this crate: a sandbox below the temp folder, junctions, security snapshots.

use std::path::{Path, PathBuf};

use vcam_protocol::secure;

use crate::acl::{self, Principals, Snapshot};
use crate::datadir::remove_no_follow;
use crate::util::Paths;

pub const FAKE_USER: &str = "S-1-5-21-1-2-3-1001";

pub struct Sandbox {
    pub root: PathBuf,
    pub paths: Paths,
    pub p: Principals,
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        // junctions are removed as junctions, never through
        let _ = remove_no_follow(&self.root);
    }
}

/// A sandbox folder with `Paths::below`; the current user plays the administrators (so the test keeps access to
/// what it locks), the owner and the only trusted owner. The "user" of the camera is a made-up SID.
pub fn sandbox(name: &str) -> Sandbox {
    let root = std::env::temp_dir().join(format!("vcam-test-{}-{name}", std::process::id()));
    let _ = remove_no_follow(&root);
    let paths = Paths::below(&root);
    std::fs::create_dir_all(&paths.program_data).unwrap();
    let me = crate::current_user_sid().unwrap();
    let p = Principals {
        system: acl::SID_SYSTEM.into(),
        admins: me.clone(),
        local_service: acl::SID_LOCAL_SERVICE.into(),
        user: Some(FAKE_USER.into()),
        owner: me.clone(),
        // an elevated test run makes folders owned by Administrators
        trusted_owners: vec![me, acl::SID_ADMINS.into()],
    };
    Sandbox { root, paths, p }
}

pub fn junction(link: &Path, target: &Path) {
    let out = std::process::Command::new("cmd")
        .args(["/c", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()
        .unwrap();
    assert!(out.status.success(), "mklink /J failed");
}

/// Owner and DACL of `path`, read without following a link at the end.
pub fn security_of(path: &Path) -> Snapshot {
    let h = secure::open_with(
        path,
        0x0002_0000, // READ_CONTROL
        secure::SHARE_ALL,
        secure::DISP_OPEN_EXISTING,
        true,
        0,
    )
    .unwrap();
    acl::read(&h).unwrap()
}

/// How long an operation that must not wait for another program may take before the test calls it a hang.
pub const BOUND: std::time::Duration = std::time::Duration::from_secs(10);

/// Run `job` on a thread of its own and fail the test, instead of hanging it, when it has not returned in `bound`:
/// an operation that waits for another program's lock is a failure, not a slow test.
pub fn within<T: Send + 'static>(
    what: &str,
    bound: std::time::Duration,
    job: impl FnOnce() -> T + Send + 'static,
) -> T {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(job());
    });
    rx.recv_timeout(bound)
        .unwrap_or_else(|_| panic!("{what}: still waiting after {bound:?} (it waits for a lock)"))
}

/// An oplock on `p` the way a program of the user takes one on a file it may read (while it is held, a plain open,
/// delete or rename of the file by anybody else waits, and so does a read of the file here: look at it with
/// attribute-only calls such as `std::fs::metadata`). `None` (skip the test) when this file system grants none.
pub fn lock_on(p: &Path) -> Option<vcam_protocol::secure::oplock::Oplock> {
    let l = vcam_protocol::secure::oplock::Oplock::take(p).unwrap();
    if l.is_none() {
        eprintln!("this file system grants no oplock: skipped");
    }
    l
}

/// The identity (volume and file id) of the file or folder at `p`, read without following a link at its end.
pub fn identity_of(p: &Path) -> secure::FileIdentity {
    let h = secure::open_with(
        p,
        0x80, // FILE_READ_ATTRIBUTES
        secure::SHARE_ALL,
        secure::DISP_OPEN_EXISTING,
        true,
        0,
    )
    .unwrap();
    secure::file_identity(secure::raw(&h)).unwrap()
}

/// The record entry for the existing folder `program_data\name`, as the helper writes it.
pub fn entry_of(program_data: &Path, name: &str) -> crate::aside::AsideEntry {
    crate::aside::AsideEntry {
        name: name.to_string(),
        identity: identity_of(&program_data.join(name)),
    }
}

/// A record store in memory (the helper's HKLM value) that can be told to fail, and counts its writes.
#[derive(Default)]
pub struct MemStore {
    /// `None`: there is no value.
    pub stored: Option<crate::machine::Stored>,
    /// Every write, in order (an empty list is the removal of the value).
    pub writes: Vec<Vec<String>>,
    pub fail_writes: bool,
}

impl crate::machine::AsideStore for MemStore {
    fn aside_record(&mut self) -> crate::machine::Stored {
        self.stored
            .clone()
            .unwrap_or(crate::machine::Stored::Missing)
    }
    fn write_aside_record(&mut self, strings: &[String]) -> crate::util::Res<()> {
        if self.fail_writes {
            return Err("write failed (injected)".into());
        }
        self.writes.push(strings.to_vec());
        self.stored =
            (!strings.is_empty()).then(|| crate::machine::Stored::Strings(strings.to_vec()));
        Ok(())
    }
}

impl MemStore {
    /// The entries on the record.
    pub fn entries(&mut self) -> Vec<crate::aside::AsideEntry> {
        match crate::aside::load(self) {
            crate::aside::Record::Entries { entries, .. } => entries,
            crate::aside::Record::Unreadable(why) => panic!("the record is unreadable: {why}"),
        }
    }

    /// The names on the record, in order.
    pub fn names(&mut self) -> Vec<String> {
        self.entries().into_iter().map(|e| e.name).collect()
    }
}

/// A fake DLL that passes `looks_like_dll`.
pub fn fake_dll(dir: &Path, fill: u8) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let mut bytes = vec![fill; 70_000];
    bytes[..2].copy_from_slice(b"MZ");
    let p = dir.join("better_cam_vcam.dll");
    std::fs::write(&p, bytes).unwrap();
    p
}

/// A file `name` in `dir` that nobody but SYSTEM has any right on (its owner can only change the permissions back, by
/// [`make_deletable`]). Every open of the helper asks for `SYNCHRONIZE` and the owner has no right to it, so the
/// helper cannot delete the file (the plain `DeleteFileW` could, through the delete-child right of the folder). A
/// folder with such a file in it can be renamed and not deleted: an old copy that stays.
pub fn undeletable_file(dir: &Path, name: &str, owner: &str) -> PathBuf {
    let f = dir.join(name);
    std::fs::write(&f, b"x").unwrap();
    let h = secure::open_with(
        &f,
        0x8000_0000 | acl::SECURITY_ACCESS,
        secure::SHARE_ALL,
        secure::DISP_OPEN_EXISTING,
        true,
        0,
    )
    .unwrap();
    let only_system = [acl::Ace {
        sid: acl::SID_SYSTEM.into(),
        mask: acl::FULL,
        flags: 0,
    }];
    acl::apply_aces(&h, &only_system, owner).unwrap();
    f
}

/// Give `owner` full control of a file made by [`undeletable_file`] again, so that the folder it is in can be deleted.
/// By path (`SetNamedSecurityInfoW` opens the file for `WRITE_DAC` alone, which an owner always has: nothing here
/// can open it, as every open asks for `SYNCHRONIZE` too). The file is in a folder of the sandbox, not behind a link.
pub fn make_deletable(f: &Path, owner: &str) {
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SetNamedSecurityInfoW,
        SDDL_REVISION_1, SE_FILE_OBJECT,
    };
    use windows::Win32::Security::{
        GetSecurityDescriptorDacl, ACL, DACL_SECURITY_INFORMATION,
        PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
    };
    let sddl = crate::util::wide(&format!("D:P(A;;FA;;;{owner})"));
    let path = crate::util::wide(&f.to_string_lossy());
    let mut psd = PSECURITY_DESCRIPTOR::default();
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            crate::util::pcwstr(&sddl),
            SDDL_REVISION_1,
            &mut psd,
            None,
        )
        .unwrap();
        let (mut present, mut defaulted) = (windows_core::BOOL(0), windows_core::BOOL(0));
        let mut dacl: *mut ACL = std::ptr::null_mut();
        GetSecurityDescriptorDacl(psd, &mut present, &mut dacl, &mut defaulted).unwrap();
        let r = SetNamedSecurityInfoW(
            crate::util::pcwstr(&path),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(dacl),
            None,
        );
        let _ = LocalFree(Some(HLOCAL(psd.0)));
        assert_eq!(
            r.0,
            0,
            "could not give {} its permissions back",
            f.display()
        );
    }
}
/// The `BetterCam.old-*` entries of the sandbox's `%ProgramData%`.
pub fn asides_in(paths: &Paths) -> Vec<PathBuf> {
    std::fs::read_dir(&paths.program_data)
        .unwrap()
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("BetterCam.old-")
        })
        .map(|e| e.path())
        .collect()
}
