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

/// A fake DLL that passes `looks_like_dll`.
pub fn fake_dll(dir: &Path, fill: u8) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let mut bytes = vec![fill; 70_000];
    bytes[..2].copy_from_slice(b"MZ");
    let p = dir.join("better_cam_vcam.dll");
    std::fs::write(&p, bytes).unwrap();
    p
}
