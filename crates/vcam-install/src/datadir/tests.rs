use super::*;
use crate::acl::Snapshot;
use crate::log::VecLog;
use crate::testkit::{
    compress_folder, compressed_sandbox, junction, lock_on, sandbox, security_of, within, Sandbox,
    BOUND,
};

fn run(s: &Sandbox, log: &mut VecLog) -> Res<()> {
    prepare(&s.paths, &s.p, &mut || false, log)
}

fn mentions(log: &VecLog, text: &str) -> bool {
    log.0.iter().any(|l| l.contains(text))
}

/// A folder `elsewhere` with a file in it that a redirected helper would change: its security descriptor is
/// recorded so that a test can show it stayed as it was.
fn elsewhere(s: &Sandbox) -> (PathBuf, Snapshot, Snapshot) {
    let dir = s.root.join("elsewhere");
    std::fs::create_dir_all(dir.join("logs")).unwrap();
    std::fs::write(dir.join("frames.bin"), b"precious").unwrap();
    std::fs::write(dir.join("logs").join("old.log"), b"old").unwrap();
    (
        dir.clone(),
        security_of(&dir.join("frames.bin")),
        security_of(&dir.join("logs")),
    )
}

fn untouched(dir: &Path, frames: &Snapshot, logs: &Snapshot) {
    assert_eq!(std::fs::read(dir.join("frames.bin")).unwrap(), b"precious");
    assert_eq!(
        std::fs::read(dir.join("logs").join("old.log")).unwrap(),
        b"old"
    );
    assert_eq!(
        &security_of(&dir.join("frames.bin")),
        frames,
        "its permissions changed"
    );
    assert_eq!(
        &security_of(&dir.join("logs")),
        logs,
        "its permissions changed"
    );
    // nothing was added
    assert_eq!(std::fs::read_dir(dir).unwrap().count(), 2);
    assert_eq!(std::fs::read_dir(dir.join("logs")).unwrap().count(), 1);
}

// ---- the decision, from injected facts

fn good(is_directory: bool) -> Facts {
    Facts {
        reparse_point: false,
        is_directory,
        links: 1,
        in_place: true,
        owner_trusted: true,
        acl_as_expected: true,
        bad_attributes: false,
        sparse_or_compressed: false,
        extra_streams: false,
        encrypted: false,
    }
}

/// An encrypted data folder or `logs` is replaced, never kept: whatever is made in it is encrypted too (the helper's
/// own `install.log` under the key of whoever elevated), and the camera service cannot read that. A frame file that
/// is encrypted is replaced as before (`bad_attributes`).
#[test]
fn an_encrypted_folder_is_replaced_and_so_is_an_encrypted_file() {
    for obj in [Object::DataDir, Object::LogsDir] {
        let Plan::Replace(why) = decide(
            obj,
            Some(&Facts {
                encrypted: true,
                ..good(true)
            }),
        ) else {
            panic!("{obj:?}: an encrypted folder must be replaced")
        };
        assert!(why.contains("encrypted"), "{why}");
    }
    for obj in [Object::FramesFile, Object::InstallLog] {
        let f = Facts {
            encrypted: true,
            bad_attributes: true,
            ..good(false)
        };
        assert!(
            matches!(decide(obj, Some(&f)), Plan::Replace(w) if w.contains("encrypted")),
            "{obj:?}"
        );
    }
}

#[test]
fn a_missing_object_is_created_and_a_good_one_is_kept() {
    assert_eq!(decide(Object::DataDir, None), Plan::Create);
    assert_eq!(decide(Object::LogsDir, None), Plan::Create);
    assert_eq!(decide(Object::FramesFile, None), Plan::Create);
    assert_eq!(decide(Object::InstallLog, None), Plan::Create);
    assert_eq!(decide(Object::DataDir, Some(&good(true))), Plan::Keep);
    assert_eq!(decide(Object::LogsDir, Some(&good(true))), Plan::Keep);
    assert_eq!(decide(Object::FramesFile, Some(&good(false))), Plan::Keep);
    assert_eq!(decide(Object::InstallLog, Some(&good(false))), Plan::Keep);
}

#[test]
fn a_frame_file_with_attributes_the_users_of_an_older_version_could_set_is_replaced() {
    let odd = |dir| Facts {
        bad_attributes: true,
        ..good(dir)
    };
    let Plan::Replace(why) = decide(Object::FramesFile, Some(&odd(false))) else {
        panic!("a read-only or encrypted frame file must be replaced")
    };
    assert!(why.contains("read-only"), "{why}");
    // the flags are meaningless for folders: they are not a reason
    assert_eq!(decide(Object::LogsDir, Some(&odd(true))), Plan::Keep);
    assert_eq!(decide(Object::DataDir, Some(&odd(true))), Plan::Keep);
}

/// The states a `FILE_WRITE_DATA` handle can set (sparse, compressed, an extra data stream) are each a reason to
/// replace the frame file: none is ever cleared in place.
#[test]
fn sparse_compressed_and_streamed_frame_files_are_replaced() {
    let sparse = Facts {
        sparse_or_compressed: true,
        ..good(false)
    };
    let Plan::Replace(why) = decide(Object::FramesFile, Some(&sparse)) else {
        panic!("a sparse or compressed frame file must be replaced, not kept")
    };
    assert!(why.contains("sparse or compressed"), "{why}");
    // the flags mean nothing for a folder
    let folder = Facts {
        sparse_or_compressed: true,
        ..good(true)
    };
    assert_eq!(decide(Object::LogsDir, Some(&folder)), Plan::Keep);
    assert_eq!(decide(Object::DataDir, Some(&folder)), Plan::Keep);
    let streams = Facts {
        extra_streams: true,
        ..good(false)
    };
    let Plan::Replace(why) = decide(Object::FramesFile, Some(&streams)) else {
        panic!("a file with a stream must be replaced")
    };
    assert!(why.contains("stream"), "{why}");
    // folders are not looked at for streams
    let dir = Facts {
        extra_streams: true,
        ..good(true)
    };
    assert_eq!(decide(Object::LogsDir, Some(&dir)), Plan::Keep);
}

#[test]
fn links_wrong_kinds_hard_links_and_foreign_owners_are_replaced() {
    let replace = |obj, f: Facts| matches!(decide(obj, Some(&f)), Plan::Replace(_));
    for (obj, dir) in [
        (Object::DataDir, true),
        (Object::LogsDir, true),
        (Object::FramesFile, false),
        (Object::InstallLog, false),
    ] {
        let g = good(dir);
        assert!(
            replace(
                obj,
                Facts {
                    reparse_point: true,
                    ..g.clone()
                }
            ),
            "{obj:?} link"
        );
        assert!(
            replace(
                obj,
                Facts {
                    is_directory: !dir,
                    ..g.clone()
                }
            ),
            "{obj:?} kind"
        );
        assert!(
            replace(
                obj,
                Facts {
                    in_place: false,
                    ..g.clone()
                }
            ),
            "{obj:?} place"
        );
        assert!(
            replace(
                obj,
                Facts {
                    owner_trusted: false,
                    ..g.clone()
                }
            ),
            "{obj:?} owner"
        );
    }
    // hard links matter for the file only
    assert!(replace(
        Object::FramesFile,
        Facts {
            links: 2,
            ..good(false)
        }
    ));
    // the reasons are said
    let Plan::Replace(why) = decide(
        Object::FramesFile,
        Some(&Facts {
            links: 3,
            ..good(false)
        }),
    ) else {
        unreachable!()
    };
    assert!(why.contains("3 hard links"), "{why}");
}

#[test]
fn other_permissions_replace_logs_but_only_rewrite_the_folder_and_the_file() {
    let odd = |dir| Facts {
        acl_as_expected: false,
        ..good(dir)
    };
    // `logs` holds files with permissions of their own: start again
    assert!(matches!(
        decide(Object::LogsDir, Some(&odd(true))),
        Plan::Replace(_)
    ));
    // the folder and the file have no children whose ACEs could survive: their DACL is replaced as a whole
    assert_eq!(decide(Object::DataDir, Some(&odd(true))), Plan::Keep);
    assert_eq!(decide(Object::FramesFile, Some(&odd(false))), Plan::Keep);
}

// ---- preparing

#[test]
fn a_fresh_machine_gets_all_three_objects_with_their_permissions() {
    let s = sandbox("fresh");
    let mut log = VecLog::default();
    run(&s, &mut log).unwrap();
    for (obj, path) in [
        (Object::DataDir, s.paths.data.clone()),
        (Object::LogsDir, s.paths.logs()),
        (Object::FramesFile, s.paths.frames()),
        (Object::InstallLog, s.paths.install_log()),
    ] {
        let snap = security_of(&path);
        acl::check(&snap, &acl::expected(obj, &s.p), &s.p.owner)
            .unwrap_or_else(|e| panic!("{obj:?}: {e}"));
    }
    let md = std::fs::metadata(s.paths.frames()).unwrap();
    assert_eq!(md.len(), vcam_protocol::FILE_SIZE);
    assert_ne!(md.file_attributes() & FILE_ATTRIBUTE_TEMPORARY.0, 0);
    assert!(mentions(&log, "verified"));
}

#[test]
fn preparing_again_keeps_what_is_there_and_its_contents() {
    let s = sandbox("again");
    let mut log = VecLog::default();
    run(&s, &mut log).unwrap();
    // the app wrote something; the helper must not recreate the file
    {
        use std::io::{Seek, Write};
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(s.paths.frames())
            .unwrap();
        f.seek(std::io::SeekFrom::Start(100)).unwrap();
        f.write_all(b"frame data").unwrap();
    }
    std::fs::write(s.paths.logs().join("vcam-1.log"), b"line").unwrap();
    let created = std::fs::metadata(s.paths.frames())
        .unwrap()
        .created()
        .unwrap();
    let mut log = VecLog::default();
    run(&s, &mut log).unwrap();
    assert_eq!(
        std::fs::metadata(s.paths.frames())
            .unwrap()
            .created()
            .unwrap(),
        created
    );
    let bytes = std::fs::read(s.paths.frames()).unwrap();
    assert_eq!(&bytes[100..110], b"frame data");
    assert_eq!(
        std::fs::read(s.paths.logs().join("vcam-1.log")).unwrap(),
        b"line"
    );
    assert_eq!(
        log.0.iter().filter(|l| l.contains(": kept")).count(),
        4,
        "{:?}",
        log.0
    );
}

#[test]
fn a_data_folder_that_is_a_junction_is_moved_aside_and_its_target_is_left_alone() {
    let s = sandbox("junction-data");
    let (dir, frames, logs) = elsewhere(&s);
    junction(&s.paths.data, &dir);
    let mut log = VecLog::default();
    run(&s, &mut log).unwrap();
    assert!(!is_link(&s.paths.data), "the junction must be gone");
    assert!(s.paths.frames().is_file() && s.paths.logs().is_dir());
    assert!(mentions(&log, "link or junction"), "{:?}", log.0);
    untouched(&dir, &frames, &logs);
    // the old behaviour (a path-based SetNamedSecurityInfoW through the junction) changed `frames.bin` there
}

#[test]
fn a_logs_folder_that_is_a_junction_is_replaced_and_its_target_is_left_alone() {
    let s = sandbox("junction-logs");
    run(&s, &mut VecLog::default()).unwrap();
    let (dir, frames, logs) = elsewhere(&s);
    remove_no_follow(&s.paths.logs()).unwrap();
    junction(&s.paths.logs(), &dir.join("logs"));
    let mut log = VecLog::default();
    run(&s, &mut log).unwrap();
    assert!(!is_link(&s.paths.logs()) && s.paths.logs().is_dir());
    assert!(mentions(&log, "link or junction"), "{:?}", log.0);
    // `dir\logs` still has old.log and its permissions
    assert_eq!(
        std::fs::read(dir.join("logs").join("old.log")).unwrap(),
        b"old"
    );
    assert_eq!(&security_of(&dir.join("logs")), &logs);
    assert_eq!(&security_of(&dir.join("frames.bin")), &frames);
    assert!(
        std::fs::read_dir(dir.join("logs")).unwrap().count() == 1,
        "something was created there"
    );
}

#[test]
fn a_hard_linked_frame_file_is_replaced_and_the_other_name_is_left_alone() {
    let s = sandbox("hardlink");
    run(&s, &mut VecLog::default()).unwrap();
    let victim = s.root.join("victim.bin");
    std::fs::write(&victim, b"precious").unwrap();
    let before = security_of(&victim);
    std::fs::remove_file(s.paths.frames()).unwrap();
    std::fs::hard_link(&victim, s.paths.frames()).unwrap();
    let mut log = VecLog::default();
    run(&s, &mut log).unwrap();
    assert!(mentions(&log, "2 hard links"), "{:?}", log.0);
    assert_eq!(std::fs::read(&victim).unwrap(), b"precious");
    assert_eq!(
        security_of(&victim),
        before,
        "the victim's permissions changed"
    );
    assert_eq!(
        std::fs::metadata(s.paths.frames()).unwrap().len(),
        vcam_protocol::FILE_SIZE
    );
}

#[test]
fn a_frame_file_that_is_a_symbolic_link_is_replaced_when_one_can_be_made() {
    let s = sandbox("symlink");
    run(&s, &mut VecLog::default()).unwrap();
    let victim = s.root.join("victim.bin");
    std::fs::write(&victim, b"precious").unwrap();
    let before = security_of(&victim);
    std::fs::remove_file(s.paths.frames()).unwrap();
    if std::os::windows::fs::symlink_file(&victim, s.paths.frames()).is_err() {
        eprintln!("no privilege to create a symbolic link: skipped");
        return;
    }
    let mut log = VecLog::default();
    run(&s, &mut log).unwrap();
    assert!(mentions(&log, "link or junction"), "{:?}", log.0);
    assert_eq!(std::fs::read(&victim).unwrap(), b"precious");
    assert_eq!(security_of(&victim), before);
    assert!(!is_link(&s.paths.frames()));
}

#[test]
fn an_owner_who_is_not_trusted_loses_the_object() {
    let mut s = sandbox("owner");
    run(&s, &mut VecLog::default()).unwrap();
    std::fs::write(s.paths.frames(), b"x").unwrap();
    std::fs::write(s.paths.logs().join("a.log"), b"x").unwrap();
    let before = std::fs::metadata(s.paths.frames())
        .unwrap()
        .created()
        .unwrap();
    // from now on only SYSTEM is trusted: the objects (owned by the test user) are somebody else's
    s.p.trusted_owners = vec![acl::SID_SYSTEM.into()];
    let mut log = VecLog::default();
    run(&s, &mut log).unwrap();
    assert!(
        mentions(&log, "owner is not SYSTEM or Administrators"),
        "{:?}",
        log.0
    );
    assert_ne!(
        std::fs::metadata(s.paths.frames())
            .unwrap()
            .created()
            .unwrap(),
        before
    );
    assert!(
        !s.paths.logs().join("a.log").exists(),
        "the old logs folder was replaced"
    );
    // the copies moved aside are gone
    let names: Vec<String> = std::fs::read_dir(&s.paths.data)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(names.iter().all(|n| !is_aside_name(n)), "{names:?}");
}

#[test]
fn a_logs_folder_with_other_permissions_is_replaced() {
    let s = sandbox("old-acl");
    run(&s, &mut VecLog::default()).unwrap();
    // what the old installer left: a plain folder that inherits from its parent, with a log in it
    remove_no_follow(&s.paths.logs()).unwrap();
    std::fs::create_dir(s.paths.logs()).unwrap();
    std::fs::write(s.paths.logs().join("vcam-9.log"), b"old").unwrap();
    let mut log = VecLog::default();
    run(&s, &mut log).unwrap();
    assert!(mentions(&log, "other permissions"), "{:?}", log.0);
    assert!(!s.paths.logs().join("vcam-9.log").exists());
    let snap = security_of(&s.paths.logs());
    acl::check(&snap, &acl::expected(Object::LogsDir, &s.p), &s.p.owner).unwrap();
}

#[test]
fn strangers_in_the_data_folder_are_removed_without_following_links() {
    let s = sandbox("sweep");
    run(&s, &mut VecLog::default()).unwrap();
    let (dir, frames, logs) = elsewhere(&s);
    std::fs::write(s.paths.data.join("evil.txt"), b"x").unwrap();
    std::fs::create_dir(s.paths.data.join("nested")).unwrap();
    junction(&s.paths.data.join("nested").join("link"), &dir);
    junction(&s.paths.data.join("evil"), &dir);
    std::fs::create_dir(s.paths.data.join("frames.bin.old-1-0")).unwrap();
    let mut log = VecLog::default();
    run(&s, &mut log).unwrap();
    let left: Vec<String> = std::fs::read_dir(&s.paths.data)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(left.len(), 2, "{left:?}");
    untouched(&dir, &frames, &logs);
}

#[test]
fn deleting_never_recurses_through_a_link() {
    let s = sandbox("remove");
    let (dir, frames, logs) = elsewhere(&s);
    let victim = s.root.join("tree");
    std::fs::create_dir_all(victim.join("a")).unwrap();
    std::fs::write(victim.join("a").join("f.txt"), b"x").unwrap();
    junction(&victim.join("a").join("link"), &dir);
    junction(&s.root.join("top-link"), &dir);
    remove_no_follow(&victim).unwrap();
    assert!(!victim.exists());
    untouched(&dir, &frames, &logs);
    // a link given directly: the link goes, the target stays
    remove_no_follow(&s.root.join("top-link")).unwrap();
    assert!(!s.root.join("top-link").exists());
    untouched(&dir, &frames, &logs);
    // missing is fine
    remove_no_follow(&s.root.join("nothing")).unwrap();
    // a read-only file
    let ro = s.root.join("ro.txt");
    std::fs::write(&ro, b"x").unwrap();
    let mut perm = std::fs::metadata(&ro).unwrap().permissions();
    perm.set_readonly(true);
    std::fs::set_permissions(&ro, perm).unwrap();
    remove_no_follow(&ro).unwrap();
    assert!(!ro.exists());
}

#[test]
fn removing_the_data_folder_through_a_junction_leaves_the_target() {
    let s = sandbox("remove-all");
    let (dir, frames, logs) = elsewhere(&s);
    junction(&s.paths.data, &dir);
    assert!(remove_all(&s.paths));
    assert!(!s.paths.data.exists());
    untouched(&dir, &frames, &logs);
    // a real folder with content goes too, and a missing one counts as gone
    run(&s, &mut VecLog::default()).unwrap();
    assert!(remove_all(&s.paths));
    assert!(remove_all(&s.paths));
}

#[test]
fn moving_a_junction_aside_renames_the_junction_and_not_the_target() {
    let s = sandbox("aside");
    let (dir, frames, logs) = elsewhere(&s);
    let link = s.root.join("link");
    junction(&link, &dir);
    let aside = move_aside(&link).unwrap();
    assert!(is_aside_name(&aside.file_name().unwrap().to_string_lossy()));
    assert!(is_link(&aside) && !link.exists());
    assert!(dir.exists());
    remove_no_follow(&aside).unwrap();
    untouched(&dir, &frames, &logs);
}

#[test]
fn a_service_that_holds_the_object_is_stopped_once_and_the_move_retried() {
    // moving a folder fails while a file inside is open without delete sharing; the helper then stops the service
    let s = sandbox("busy");
    run(&s, &mut VecLog::default()).unwrap();
    remove_no_follow(&s.paths.logs()).unwrap();
    std::fs::create_dir(s.paths.logs()).unwrap();
    let held_path = s.paths.logs().join("vcam-1.log");
    std::fs::write(&held_path, b"x").unwrap();
    // opened sharing read and write but not delete: the folder cannot be renamed meanwhile
    use std::os::windows::fs::OpenOptionsExt;
    let held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(3)
        .open(&held_path)
        .unwrap();
    let held = std::cell::RefCell::new(Some(held));
    let stops = std::cell::Cell::new(0);
    let mut log = VecLog::default();
    prepare(
        &s.paths,
        &s.p,
        &mut || {
            stops.set(stops.get() + 1);
            // "stopping the service" closes its handle
            held.borrow_mut().take();
            true
        },
        &mut log,
    )
    .unwrap();
    assert_eq!(stops.get(), 1, "{:?}", log.0);
    assert!(mentions(&log, "is in use"));
    assert!(!held_path.exists());
}

#[test]
fn a_path_based_acl_call_follows_a_junction_which_is_why_nothing_here_uses_one() {
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SetNamedSecurityInfoW,
        SDDL_REVISION_1, SE_FILE_OBJECT,
    };
    use windows::Win32::Security::{
        GetSecurityDescriptorDacl, ACL, DACL_SECURITY_INFORMATION,
        PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
    };
    // The reviewer's reproduction: the old helper called SetNamedSecurityInfoW on `<data>\frames.bin`; with the
    // folder swapped for a junction the call changed the permissions of the file the junction points to.
    let s = sandbox("path-call");
    let (dir, frames, _) = elsewhere(&s);
    let link = s.root.join("link");
    junction(&link, &dir);
    let me = &s.p.owner;
    let sddl = crate::util::wide(&format!("D:P(A;;FA;;;{me})(A;;FR;;;WD)"));
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
        let path = crate::util::wide(&link.join("frames.bin").to_string_lossy());
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
        assert_eq!(r.0, 0);
    }
    assert_ne!(
        security_of(&dir.join("frames.bin")),
        frames,
        "by path, the target's permissions changed"
    );
    // the replacement does not do that: see `a_data_folder_that_is_a_junction_is_moved_aside_...`
}

#[test]
fn a_logs_folder_that_cannot_be_moved_is_emptied_and_reset_in_place() {
    // Moving a folder fails while any file in it is open, even one opened with delete sharing (every app that
    // loaded the DLL keeps its log open): this is what stopped the first live upgrade.
    let s = sandbox("in-place");
    run(&s, &mut VecLog::default()).unwrap();
    remove_no_follow(&s.paths.logs()).unwrap();
    std::fs::create_dir(s.paths.logs()).unwrap();
    let open_log = s.paths.logs().join("vcam-1.log");
    std::fs::write(&open_log, b"x").unwrap();
    std::fs::write(s.paths.logs().join("vcam-2.log"), b"x").unwrap();
    let outside = s.root.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("keep.txt"), b"keep").unwrap();
    junction(&s.paths.logs().join("link"), &outside);
    // like the DLL: read and write sharing, delete sharing too
    let held = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&open_log)
        .unwrap();
    let mut log = VecLog::default();
    run(&s, &mut log).unwrap();
    assert!(mentions(&log, "in place"), "{:?}", log.0);
    assert!(!s.paths.logs().join("vcam-2.log").exists());
    assert!(
        std::fs::read(&open_log).is_err(),
        "delete pending: nobody can open it any more"
    );
    assert!(!s.paths.logs().join("link").exists());
    assert_eq!(
        std::fs::read(outside.join("keep.txt")).unwrap(),
        b"keep",
        "through the junction"
    );
    let snap = security_of(&s.paths.logs());
    acl::check(&snap, &acl::expected(Object::LogsDir, &s.p), &s.p.owner).unwrap();
    drop(held);
}

#[test]
fn the_install_log_of_the_helper_does_not_keep_logs_from_being_replaced() {
    let s = sandbox("own-log");
    run(&s, &mut VecLog::default()).unwrap();
    // an old `logs` folder with the install log in it, which the helper has open
    remove_no_follow(&s.paths.logs()).unwrap();
    std::fs::create_dir(s.paths.logs()).unwrap();
    let mut log = crate::log::FileLog::with_paths_and_principals(s.paths.clone(), s.p.clone());
    log.line("first line");
    assert!(log.attached());
    prepare(&s.paths, &s.p, &mut || false, &mut log).unwrap();
    log.folder_ready();
    log.line("after");
    let text = std::fs::read_to_string(s.paths.install_log()).unwrap();
    // the line written before the folder was replaced is not lost
    assert!(
        text.contains("first line") && text.contains("after"),
        "{text}"
    );
    assert!(
        text.contains("moving it aside") && !text.contains("in place"),
        "{text}"
    );
}

// ---- L1: nothing exists with permissions it should not have

/// A new object has its final DACL from the create call, before any call that sets permissions: `create_new` is
/// the only thing that touched these.
#[test]
fn a_new_object_has_its_final_permissions_from_the_create_call() {
    let s = sandbox("create-final");
    for obj in [
        Object::DataDir,
        Object::LogsDir,
        Object::FramesFile,
        Object::InstallLog,
    ] {
        let h = create_new(&s.paths, obj, &s.p).unwrap();
        let snap = acl::read(&h).unwrap();
        // the owner is whoever made it (the helper sets Administrators afterwards); the DACL is final already
        acl::check_dacl(&snap, &acl::expected(obj, &s.p))
            .unwrap_or_else(|e| panic!("{obj:?} was created with other permissions: {e}"));
        drop(h);
        let again = create_new(&s.paths, obj, &s.p);
        assert!(
            again.is_err(),
            "{obj:?}: create must not take what is there"
        );
    }
    // what the folder around it would have handed down (the sandbox's parent gives the user full control) is
    // not on the child
    let snap = security_of(&s.paths.logs());
    assert!(snap.protected);
    assert!(
        snap.aces.iter().all(|a| a.flags & 0x10 == 0),
        "inherited ACE: {snap:?}"
    );
}

/// The order of `prepare`: the data folder is locked before `logs`, `frames.bin` or `install.log` are made, so a
/// folder an older version left (with the user's inheritable modify right on it) hands nothing down.
#[test]
fn the_data_folder_is_locked_before_anything_below_it_is_made() {
    let s = sandbox("order");
    // what an older version left behind: the data folder with an inheritable modify right for the user
    let sd = acl::SecurityDescriptor::from_aces(&[
        acl::Ace {
            sid: s.p.admins.clone(),
            mask: acl::FULL,
            flags: acl::OBJECT_INHERIT | acl::CONTAINER_INHERIT,
        },
        acl::Ace {
            sid: crate::testkit::FAKE_USER.into(),
            mask: acl::FILE_MODIFY,
            flags: acl::OBJECT_INHERIT | acl::CONTAINER_INHERIT,
        },
    ])
    .unwrap();
    let old = unsafe {
        secure::create_dir_with_sd(
            &s.paths.data,
            GENERIC_READ | acl::SECURITY_ACCESS,
            secure::SHARE_ALL,
            sd.as_ptr(),
        )
    }
    .unwrap();
    // the old owner is the test user, who is trusted: the folder is kept and locked in place
    drop(old);
    let mut log = VecLog::default();
    run(&s, &mut log).unwrap();
    let at = |needle: &str| {
        log.0
            .iter()
            .position(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("no line with '{needle}' in {:?}", log.0))
    };
    let data_locked = at("the data folder: owner");
    for child in [
        "logs: created",
        "frames.bin: created",
        "install.log: created",
    ] {
        assert!(
            data_locked < at(child),
            "'{child}' came before the data folder was locked: {:?}",
            log.0
        );
    }
    // and in the end the user has no way to create anything in it or below
    for (obj, path) in [
        (Object::DataDir, s.paths.data.clone()),
        (Object::LogsDir, s.paths.logs()),
        (Object::FramesFile, s.paths.frames()),
        (Object::InstallLog, s.paths.install_log()),
    ] {
        let snap = security_of(&path);
        acl::check(&snap, &acl::expected(obj, &s.p), &s.p.owner)
            .unwrap_or_else(|e| panic!("{obj:?}: {e}"));
        assert!(
            snap.aces
                .iter()
                .filter(|a| a.sid == crate::testkit::FAKE_USER)
                .all(
                    |a| a.flags & (acl::OBJECT_INHERIT | acl::CONTAINER_INHERIT) == 0
                    // no write of EA or attributes, delete child, DELETE, WRITE_DAC, WRITE_OWNER ...
                    && a.mask & 0x000d_0150 == 0
                    // ... and in a folder no FILE_ADD_FILE (2) or FILE_ADD_SUBDIRECTORY (4)
                    && (obj.is_file() || a.mask & 0x6 == 0)
                ),
            "{obj:?}: {snap:?}"
        );
    }
}

/// Entries in `logs` that nobody trusted made are swept: a stranger's file, a stranger's link (as a link).
#[test]
fn entries_of_logs_that_are_not_owned_by_a_trusted_account_are_removed() {
    let s = sandbox("sweep-logs");
    run(&s, &mut VecLog::default()).unwrap();
    let (dir, frames, logs) = elsewhere(&s);
    std::fs::write(s.paths.logs().join("vcam-5.log"), b"x").unwrap();
    std::fs::write(s.paths.logs().join("stranger.txt"), b"x").unwrap();
    std::fs::create_dir(s.paths.logs().join("folder")).unwrap();
    junction(&s.paths.logs().join("vcam-6.log"), &dir);
    // everybody who could own them here (the test user, or Administrators when elevated) is trusted: nothing goes
    let mut trusted = s.p.trusted_owners.clone();
    trusted.push(acl::SID_ADMINS.into());
    let mut log = VecLog::default();
    assert_eq!(sweep_logs(&s.paths, &trusted, &mut log), 0);
    assert!(s.paths.logs().join("stranger.txt").exists());
    // trusted are only SYSTEM, LOCAL SERVICE and a made-up administrator: the entries are strangers'
    let only = vec![
        acl::SID_SYSTEM.to_string(),
        acl::SID_LOCAL_SERVICE.to_string(),
        "S-1-5-21-1-2-3-500".to_string(),
    ];
    let mut log = VecLog::default();
    let removed = sweep_logs(&s.paths, &only, &mut log);
    assert!(removed >= 5, "{removed}: {:?}", log.0);
    assert_eq!(std::fs::read_dir(s.paths.logs()).unwrap().count(), 0);
    // the junction was removed as a junction, its target is as it was
    untouched(&dir, &frames, &logs);
}

// ---- L5: install.log has a DACL of its own

#[test]
fn the_install_log_loses_the_modify_right_the_service_would_inherit_from_logs() {
    let s = sandbox("install-log-acl");
    run(&s, &mut VecLog::default()).unwrap();
    std::fs::write(s.paths.install_log(), b"earlier run\n").unwrap();
    // an older version's file: it inherited LOCAL SERVICE's modify right from `logs`
    let h = secure::open_with(
        &s.paths.install_log(),
        GENERIC_READ | acl::SECURITY_ACCESS,
        secure::SHARE_ALL,
        DISP_OPEN_EXISTING,
        true,
        0,
    )
    .unwrap();
    let loose = [
        acl::Ace {
            sid: s.p.admins.clone(),
            mask: acl::FULL,
            flags: 0,
        },
        acl::Ace {
            sid: acl::SID_LOCAL_SERVICE.into(),
            mask: acl::FILE_MODIFY,
            flags: 0,
        },
    ];
    acl::apply_aces(&h, &loose, &s.p.owner).unwrap();
    assert!(security_of(&s.paths.install_log())
        .aces
        .iter()
        .any(|a| a.sid == acl::SID_LOCAL_SERVICE));
    drop(h);
    let mut log = VecLog::default();
    run(&s, &mut log).unwrap();
    let snap = security_of(&s.paths.install_log());
    acl::check(&snap, &acl::expected(Object::InstallLog, &s.p), &s.p.owner).unwrap();
    assert!(
        snap.aces.iter().all(|a| a.sid != acl::SID_LOCAL_SERVICE),
        "LOCAL SERVICE can still change install.log: {snap:?}"
    );
    assert_eq!(
        std::fs::read(s.paths.install_log()).unwrap(),
        b"earlier run\n",
        "the log's content must survive"
    );
    assert!(mentions(&log, "install.log: kept"), "{:?}", log.0);
}

// ---- L9: a frame file the old permissions let the user damage is replaced

/// A replaced file is a new one: the marker in the old one is gone (creation times cannot tell, NTFS tunnels them to
/// a file made under the same name moments later).
fn mark(s: &Sandbox) {
    use std::io::{Seek, SeekFrom, Write};
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .open(s.paths.frames())
        .unwrap();
    f.seek(SeekFrom::Start(100)).unwrap();
    f.write_all(b"OLD FILE").unwrap();
}

fn marked(s: &Sandbox) -> bool {
    &std::fs::read(s.paths.frames()).unwrap()[100..108] == b"OLD FILE"
}

fn attributes_of(p: &Path) -> u32 {
    std::fs::metadata(p).unwrap().file_attributes()
}

/// The identity of the file at `p` (volume and file index): the same value means the same file, not a copy.
fn file_id(p: &Path) -> (u32, u32, u32) {
    use windows::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };
    let h = secure::open_with(p, 0x80, secure::SHARE_ALL, DISP_OPEN_EXISTING, false, 0).unwrap();
    let mut i = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(secure::raw(&h), &mut i) }.unwrap();
    (i.dwVolumeSerialNumber, i.nFileIndexHigh, i.nFileIndexLow)
}

#[test]
fn a_read_only_frame_file_is_replaced() {
    let s = sandbox("bad-attrs");
    run(&s, &mut VecLog::default()).unwrap();
    mark(&s);
    let mut perm = std::fs::metadata(s.paths.frames()).unwrap().permissions();
    perm.set_readonly(true);
    std::fs::set_permissions(s.paths.frames(), perm).unwrap();
    assert!(
        secure::open_existing(&s.paths.frames(), true, None).is_err(),
        "the writer cannot use a read-only file"
    );
    let mut log = VecLog::default();
    run(&s, &mut log).unwrap();
    assert!(mentions(&log, "read-only"), "{:?}", log.0);
    assert!(!marked(&s), "the file was not replaced");
    assert!(secure::open_existing(&s.paths.frames(), true, None).is_ok());
}

/// A handle with only `FILE_WRITE_DATA` can make the frame file sparse, one with read and write data can compress
/// it (`acl::tests::a_write_data_handle_can_still_...`). Repair **replaces** such a file by a new one (the old code
/// cleared the flags in place, which is not safe: the length can change between the look at it and the clearing, and
/// clearing the sparse flag of a file that is by then a hundred gigabytes long makes Windows allocate all of it).
/// Whatever the state, after the Repair the file has exactly the expected size, is not sparse, not compressed, has
/// one stream and one link, and is not the old file; and nothing was allocated beyond the expected size at any point:
/// the old file is held open by the test through the whole Repair, and its allocation is read at the end.
#[test]
fn a_sparse_compressed_oversized_or_streamed_frame_file_is_replaced_by_a_plain_new_one() {
    const BIG: u64 = 512 << 20;
    // (name, how the user damages the right-sized file)
    type Damage = fn(&Sandbox) -> bool;
    let cases: [(&str, Damage); 6] = [
        ("sparse", |s| {
            let h = secure::open_with(
                &s.paths.frames(),
                0x2,
                secure::SHARE_ALL,
                DISP_OPEN_EXISTING,
                false,
                0,
            )
            .unwrap();
            secure::set_sparse(secure::raw(&h), true).is_ok()
        }),
        ("compressed", |s| {
            let h = secure::open_with(
                &s.paths.frames(),
                0x3,
                secure::SHARE_ALL,
                DISP_OPEN_EXISTING,
                false,
                0,
            )
            .unwrap();
            secure::set_compressed(secure::raw(&h), true).is_ok()
        }),
        ("sparse-and-compressed", |s| {
            let h = secure::open_with(
                &s.paths.frames(),
                0x3,
                secure::SHARE_ALL,
                DISP_OPEN_EXISTING,
                false,
                0,
            )
            .unwrap();
            secure::set_sparse(secure::raw(&h), true).is_ok()
                && secure::set_compressed(secure::raw(&h), true).is_ok()
        }),
        ("oversized-sparse", |s| {
            // a plain sparse file of 512 MB made by the user (the one the helper makes is `FILE_ATTRIBUTE_TEMPORARY`,
            // which this volume gives storage to when it is extended)
            remove_no_follow(&s.paths.frames()).unwrap();
            let f = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(s.paths.frames())
                .unwrap();
            if secure::set_sparse(secure::raw(&f), true).is_err() {
                return false;
            }
            f.set_len(BIG).unwrap();
            on_disk(&s.paths.frames()) < 16 << 20
        }),
        ("extra-stream", |s| {
            std::fs::write(format!("{}:extra", s.paths.frames().display()), b"hidden").is_ok()
        }),
        ("wrong-length", |s| {
            set_length(s, vcam_protocol::FILE_SIZE + 4096, false)
        }),
    ];
    for (name, damage) in cases {
        let s = sandbox(&format!("replaced-{name}"));
        run(&s, &mut VecLog::default()).unwrap();
        mark(&s);
        if !damage(&s) {
            eprintln!("{name}: this volume cannot do that: skipped");
            continue;
        }
        let frames = s.paths.frames();
        let id = file_id(&frames);
        // the old file, held open through the whole Repair (with every sharing right): what it holds on disk is
        // what the Repair made of it, wherever its name went
        let old = secure::open_with(
            &frames,
            0x80,
            secure::SHARE_ALL,
            DISP_OPEN_EXISTING,
            false,
            0,
        )
        .unwrap();
        let before = allocation(&old);
        let mut log = VecLog::default();
        run(&s, &mut log).unwrap_or_else(|e| panic!("{name}: {e}: {:?}", log.0));
        assert!(
            !mentions(&log, "in place"),
            "{name}: something was cleared in place: {:?}",
            log.0
        );
        assert!(
            mentions(&log, "moving it aside"),
            "{name}: the file was not replaced: {:?}",
            log.0
        );
        assert!(
            allocation(&old) <= before.max(16 << 20),
            "{name}: the old file was given storage: {before} -> {} bytes",
            allocation(&old)
        );
        assert_plain_new_file(&s, id, name);
        assert!(
            !marked(&s),
            "{name}: the contents of the old file are in the new one"
        );
        // the next Repair keeps the new file
        let mut log = VecLog::default();
        run(&s, &mut log).unwrap();
        assert!(
            log.0.iter().any(|l| l.ends_with("frames.bin: kept")),
            "{name}: {:?}",
            log.0
        );
        drop(old);
    }
}

/// The allocation of the file behind `h` (`FileStandardInfo.AllocationSize`).
fn allocation(h: &OwnedHandle) -> u64 {
    use windows::Win32::Storage::FileSystem::{
        FileStandardInfo, GetFileInformationByHandleEx, FILE_STANDARD_INFO,
    };
    let mut i = FILE_STANDARD_INFO::default();
    unsafe {
        GetFileInformationByHandleEx(
            secure::raw(h),
            FileStandardInfo,
            &mut i as *mut _ as *mut core::ffi::c_void,
            std::mem::size_of::<FILE_STANDARD_INFO>() as u32,
        )
    }
    .unwrap();
    i.AllocationSize as u64
}

/// What every Repair must leave: a frame file that is not the one with `old_id`, of exactly the expected size, not
/// sparse, not compressed, with one stream and one link, and taking no more room on the disk than that.
fn assert_plain_new_file(s: &Sandbox, old_id: (u32, u32, u32), what: &str) {
    let frames = s.paths.frames();
    let md = std::fs::metadata(&frames).unwrap();
    assert_eq!(md.len(), vcam_protocol::FILE_SIZE, "{what}");
    assert_eq!(
        md.file_attributes() & (secure::ATTRIBUTE_SPARSE | secure::ATTRIBUTE_COMPRESSED),
        0,
        "{what}"
    );
    assert_ne!(file_id(&frames), old_id, "{what}: the file was kept");
    let h = secure::open_with(
        &frames,
        0x80,
        secure::SHARE_ALL,
        DISP_OPEN_EXISTING,
        false,
        0,
    )
    .unwrap();
    assert_eq!(secure::kind(secure::raw(&h)).unwrap().links, 1, "{what}");
    assert_eq!(
        secure::stream_names(secure::raw(&h)).unwrap(),
        ["::$DATA"],
        "{what}"
    );
    assert!(
        on_disk(&frames) <= vcam_protocol::FILE_SIZE + (1 << 20),
        "{what}: {} bytes on disk",
        on_disk(&frames)
    );
    // and the camera accepts it
    vcam_protocol::Writer::open_precreated(&frames, Some(&s.paths.program_data))
        .unwrap_or_else(|e| panic!("{what}: the app cannot use it: {e}"));
}

/// A data stream on the frame file (made through `FILE_WRITE_DATA`) would survive a Repair that kept the file: the
/// file is replaced, and what was in the stream goes with it.
#[test]
fn a_frame_file_with_an_extra_stream_is_replaced() {
    let s = sandbox("stream");
    run(&s, &mut VecLog::default()).unwrap();
    mark(&s);
    let stream = format!("{}:extra", s.paths.frames().display());
    if std::fs::write(&stream, b"hidden").is_err() {
        eprintln!("this volume has no named streams: skipped");
        return;
    }
    assert_eq!(std::fs::read(&stream).unwrap(), b"hidden");
    let mut log = VecLog::default();
    run(&s, &mut log).unwrap();
    assert!(mentions(&log, "extra data stream"), "{:?}", log.0);
    assert!(!marked(&s), "the file was not replaced");
    assert!(
        std::fs::read(&stream).is_err(),
        "the stream survived the repair"
    );
    // the new file is plain, and is kept the next time
    let mut log = VecLog::default();
    run(&s, &mut log).unwrap();
    assert!(!mentions(&log, "extra data stream"), "{:?}", log.0);
}
// ---- L2: a moved-aside data folder is deleted later

#[test]
fn moved_aside_data_folders_next_to_betercam_are_swept_without_following_links() {
    let s = sandbox("aside-data");
    run(&s, &mut VecLog::default()).unwrap();
    let (dir, frames, logs) = elsewhere(&s);
    let pd = &s.paths.program_data;
    std::fs::create_dir_all(pd.join("BetterCam.old-1-0").join("logs")).unwrap();
    std::fs::write(pd.join("BetterCam.old-1-0").join("frames.bin"), b"x").unwrap();
    junction(&pd.join("BetterCam.old-2-0"), &dir);
    std::fs::create_dir_all(pd.join("Better.old-3-0")).unwrap(); // another name: not ours
    std::fs::create_dir_all(pd.join("Other.old-1-0")).unwrap();
    std::fs::create_dir_all(pd.join("BetterCamera")).unwrap();
    let mut log = VecLog::default();
    let recorded = entries(pd, &["BetterCam.old-1-0", "BetterCam.old-2-0"]);
    let swept = sweep_data_asides(&s.paths, &s.p.trusted_owners, &recorded, &mut log);
    assert_eq!(swept.left, 0);
    assert!(!pd.join("BetterCam.old-1-0").exists());
    // a recorded name that is a link now is not the helper's folder any more: it is left, not followed, not deleted
    assert!(is_link(&pd.join("BetterCam.old-2-0")), "the junction stays");
    untouched(&dir, &frames, &logs);
    assert_eq!(
        swept.forget,
        names(&["BetterCam.old-1-0", "BetterCam.old-2-0"]),
        "both are done with"
    );
    // not ours: other names stay, and so does the live data folder
    assert!(pd.join("Other.old-1-0").exists() && pd.join("BetterCamera").exists());
    assert!(pd.join("Better.old-3-0").exists());
    assert!(s.paths.frames().is_file());
    assert!(mentions(&log, "BetterCam.old-1-0"), "{:?}", log.0);
    assert!(mentions(&log, "BetterCam.old-2-0"), "{:?}", log.0);
    remove_no_follow(&pd.join("BetterCam.old-2-0")).unwrap();
}

fn names(n: &[&str]) -> Vec<String> {
    n.iter().map(|s| s.to_string()).collect()
}

/// The record entries for the names `n` in `pd`: the identity of the entry that is there (a link: its own), and a
/// made-up one for a name with nothing behind it.
fn entries(pd: &Path, n: &[&str]) -> Vec<AsideEntry> {
    n.iter()
        .map(|name| {
            if std::fs::symlink_metadata(pd.join(name)).is_ok() {
                crate::testkit::entry_of(pd, name)
            } else {
                AsideEntry {
                    name: name.to_string(),
                    identity: secure::FileIdentity {
                        volume: 1,
                        id: [1; 16],
                    },
                }
            }
        })
        .collect()
}

/// The names of `e`.
fn names_of(e: &[AsideEntry]) -> Vec<String> {
    e.iter().map(|e| e.name.clone()).collect()
}

/// An object whose own permissions give the caller no `DELETE` is not deleted through a handle (the plain
/// `DeleteFileW` could, through the delete-child right of the folder it is in; the helper does not rely on that:
/// what it makes grants Administrators full control). It is left, named, with everything else of the folder gone,
/// and nothing waits or fails beyond it.
#[test]
fn an_object_that_gives_the_caller_no_delete_right_is_left_and_named() {
    let s = sandbox("no-own-access");
    let dir = s.root.join("folder");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("f.txt"), b"x").unwrap();
    std::fs::write(dir.join("other.txt"), b"y").unwrap();
    let h = secure::open_with(
        &dir.join("f.txt"),
        GENERIC_READ | acl::SECURITY_ACCESS,
        secure::SHARE_ALL,
        DISP_OPEN_EXISTING,
        true,
        0,
    )
    .unwrap();
    // nobody but SYSTEM: the test user keeps only what an owner has (read and change the permissions)
    let only_system = [acl::Ace {
        sid: acl::SID_SYSTEM.into(),
        mask: acl::FULL,
        flags: 0,
    }];
    acl::apply_aces(&h, &only_system, &s.p.owner).unwrap();
    // the owner can always give itself the right back: that is what the test does to clean up
    let e = remove_no_follow(&dir).unwrap_err();
    assert!(e.to_string().contains("f.txt"), "{e}");
    assert!(
        !dir.join("other.txt").exists(),
        "the rest of the folder went"
    );
    let back = [acl::Ace {
        sid: s.p.owner.clone(),
        mask: acl::FULL,
        flags: 0,
    }];
    acl::apply_aces(&h, &back, &s.p.owner).unwrap();
    drop(h);
    remove_no_follow(&dir).unwrap();
    assert!(!dir.exists());
}
// ---- findings 1 and 2: nothing the helper deletes or moves waits for another program, and it acts only on its own

/// What a standard user can leave in `%ProgramData%`: an entry named like the helper's, that the helper did not make,
/// with a file in it that the user holds an oplock on. The sweep must leave it as it is (its owner is not SYSTEM or
/// Administrators) and must not wait for the lock: this was a hang of every install, repair and removal, with the
/// machine-wide lock held, for as long as the program held the lock.
#[test]
fn a_sweep_entry_of_somebody_else_is_left_alone_and_never_waited_for() {
    let s = sandbox("sweep-foreign");
    let pd = s.paths.program_data.clone();
    let entry = pd.join("BetterCam.old-test");
    std::fs::create_dir_all(entry.join("sub")).unwrap();
    std::fs::write(entry.join("f.txt"), vec![1u8; 4096]).unwrap();
    std::fs::write(entry.join("sub").join("g.txt"), b"g").unwrap();
    let Some(lock) = lock_on(&entry.join("f.txt")) else {
        return;
    };
    // only SYSTEM is trusted here: the test user (who made the entry) plays the standard user
    let trusted = vec![acl::SID_SYSTEM.to_string()];
    let paths = s.paths.clone();
    let recorded = entries(&pd, &["BetterCam.old-test"]);
    let (swept, lines) = within("sweep", BOUND, move || {
        let mut log = VecLog::default();
        (
            sweep_data_asides(&paths, &trusted, &recorded, &mut log),
            log.0,
        )
    });
    assert_eq!(
        swept.left, 0,
        "somebody else's entry is not the helper's to report: {lines:?}"
    );
    assert_eq!(swept.forget, ["BetterCam.old-test"]);
    assert!(
        lines
            .iter()
            .any(|l| l.contains("left alone") && l.contains("BetterCam.old-test")),
        "{lines:?}"
    );
    // (a read of the locked file would itself wait: its size is looked at without opening it for data)
    assert_eq!(std::fs::metadata(entry.join("f.txt")).unwrap().len(), 4096);
    assert_eq!(
        std::fs::read(entry.join("sub").join("g.txt")).unwrap(),
        b"g"
    );
    drop(lock);
    // ... and a junction with such a name that somebody else made is not removed either
    let (dir, frames, logs) = elsewhere(&s);
    junction(&pd.join("BetterCam.old-link"), &dir);
    let both = entries(&pd, &["BetterCam.old-test", "BetterCam.old-link"]);
    let mut log = VecLog::default();
    assert_eq!(
        sweep_data_asides(&s.paths, &[acl::SID_SYSTEM.to_string()], &both, &mut log).left,
        0
    );
    assert!(is_link(&pd.join("BetterCam.old-link")));
    untouched(&dir, &frames, &logs);
    // trusted: now the entry goes; a junction that has that name is a link, not the helper's folder: left alone
    let mut log = VecLog::default();
    let swept = sweep_data_asides(&s.paths, &s.p.trusted_owners, &both, &mut log);
    assert_eq!(swept.left, 0);
    assert!(!entry.exists());
    assert!(is_link(&pd.join("BetterCam.old-link")));
    untouched(&dir, &frames, &logs);
    remove_no_follow(&pd.join("BetterCam.old-link")).unwrap();
    untouched(&dir, &frames, &logs);
}

/// An entry the helper moved aside itself (owner Administrators) that still has a file somebody holds a lock on is
/// left, without waiting, and said so; the rest of it is deleted, and the next sweep finishes it.
#[test]
fn an_old_data_folder_of_the_helpers_with_a_locked_file_is_left_without_waiting() {
    let s = sandbox("sweep-locked");
    let entry = s.paths.program_data.join("BetterCam.old-1-0");
    std::fs::create_dir_all(entry.join("logs")).unwrap();
    std::fs::write(entry.join("frames.bin"), vec![0u8; 8192]).unwrap();
    std::fs::write(entry.join("logs").join("install.log"), b"x").unwrap();
    let Some(lock) = lock_on(&entry.join("frames.bin")) else {
        return;
    };
    let (paths, trusted) = (s.paths.clone(), s.p.trusted_owners.clone());
    let recorded = entries(&s.paths.program_data, &["BetterCam.old-1-0"]);
    let rec = recorded.clone();
    let (swept, lines) = within("sweep", BOUND, move || {
        let mut log = VecLog::default();
        (sweep_data_asides(&paths, &trusted, &rec, &mut log), log.0)
    });
    assert_eq!(swept.left, 1, "{lines:?}");
    assert!(swept.forget.is_empty(), "still there: it stays on record");
    assert_eq!(
        swept.kept,
        names(&["BetterCam.old-1-0"]),
        "and goes to the end"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("frames.bin") && l.contains("locked by another program")),
        "{lines:?}"
    );
    assert!(
        !entry.join("logs").exists(),
        "what is not locked was deleted"
    );
    drop(lock);
    let mut log = VecLog::default();
    let swept = sweep_data_asides(&s.paths, &s.p.trusted_owners, &recorded, &mut log);
    assert_eq!(swept.left, 0);
    assert_eq!(swept.forget, names_of(&recorded));
    assert!(!entry.exists());
}

/// `remove_all` (uninstall) returns, does not wait, and says which file stayed.
#[test]
fn removing_the_data_folder_does_not_wait_for_a_locked_file_and_says_which_one_stayed() {
    let s = sandbox("remove-all-locked");
    run(&s, &mut VecLog::default()).unwrap();
    std::fs::write(s.paths.logs().join("vcam-1.log"), b"x").unwrap();
    let Some(lock) = lock_on(&s.paths.install_log()) else {
        return;
    };
    let paths = s.paths.clone();
    let r = within("remove_all", BOUND, move || remove_all_reporting(&paths));
    let why = r.expect_err("a file that is locked cannot be deleted");
    assert!(
        why.contains("install.log") && why.contains("locked by another program"),
        "{why}"
    );
    assert!(s.paths.install_log().exists());
    assert!(!s.paths.frames().exists(), "what could go, went");
    assert!(!s.paths.logs().join("vcam-1.log").exists());
    let paths = s.paths.clone();
    assert!(!within("remove_all", BOUND, move || remove_all(&paths)));
    drop(lock);
    assert!(remove_all(&s.paths));
    assert!(!s.paths.data.exists());
}

/// `logs` that cannot be moved (a file in it is open) is emptied in place; a file in it that is locked by another
/// program stays and does not hold the helper up.
#[test]
fn a_logs_folder_that_is_emptied_in_place_does_not_wait_for_a_locked_log() {
    let s = sandbox("in-place-locked");
    run(&s, &mut VecLog::default()).unwrap();
    // what an older version left: a plain folder with other permissions, with logs in it
    remove_no_follow(&s.paths.logs()).unwrap();
    std::fs::create_dir(s.paths.logs()).unwrap();
    std::fs::write(s.paths.logs().join("a.log"), vec![0u8; 4096]).unwrap();
    std::fs::write(s.paths.logs().join("b.log"), b"b").unwrap();
    // the holder has the file open, which alone keeps the folder from being moved
    let Some(lock) = lock_on(&s.paths.logs().join("a.log")) else {
        return;
    };
    let (paths, p) = (s.paths.clone(), s.p.clone());
    let (r, lines) = within("prepare", BOUND, move || {
        let mut log = VecLog::default();
        (prepare(&paths, &p, &mut || false, &mut log), log.0)
    });
    r.unwrap();
    assert!(mentions(&VecLog(lines.clone()), "in place"), "{lines:?}");
    assert!(
        lines
            .iter()
            .any(|l| l.contains("a.log") && l.contains("locked by another program")),
        "{lines:?}"
    );
    assert!(!s.paths.logs().join("b.log").exists());
    assert!(
        s.paths.logs().join("a.log").exists(),
        "the locked log is the holder's to let go of"
    );
    let snap = security_of(&s.paths.logs());
    acl::check(&snap, &acl::expected(Object::LogsDir, &s.p), &s.p.owner).unwrap();
    drop(lock);
}

/// `install.log` is a file the user may read, so a program of theirs can hold a lock on it. A Repair that meets such
/// a lock asks again for about two seconds (a program that is asked to give it up does so within moments), and then
/// fails with a sentence that names the file: it does not wait, and does not carry on without having checked the
/// file's permissions.
#[test]
fn a_lock_that_is_given_up_within_a_moment_does_not_fail_the_repair_and_one_that_is_not_does() {
    let s = sandbox("lock-patience");
    run(&s, &mut VecLog::default()).unwrap();
    let Some(lock) = lock_on(&s.paths.install_log()) else {
        return;
    };
    // given up after 400 ms
    let (paths, p) = (s.paths.clone(), s.p.clone());
    let repair = std::thread::spawn(move || {
        let mut log = VecLog::default();
        (prepare(&paths, &p, &mut || false, &mut log), log.0)
    });
    std::thread::sleep(Duration::from_millis(400));
    drop(lock);
    let (r, lines) = repair.join().unwrap();
    r.unwrap_or_else(|e| panic!("{e}: {lines:?}"));
    assert!(mentions(&VecLog(lines), "install.log: kept"));
    // never given up
    let lock = lock_on(&s.paths.install_log()).unwrap();
    let (paths, p) = (s.paths.clone(), s.p.clone());
    let t = std::time::Instant::now();
    let r = within("prepare", BOUND, move || {
        prepare(&paths, &p, &mut || false, &mut VecLog::default())
    });
    let e = r.expect_err("a lock that is never given up fails the repair");
    assert!(
        e.contains("install.log") && e.contains("locked by another program"),
        "{e}"
    );
    assert!(t.elapsed() < Duration::from_secs(6), "{:?}", t.elapsed());
    drop(lock);
    run(&s, &mut VecLog::default()).unwrap();
}

/// A rename does not wait either: `move_aside` of a file that is locked returns an error that says so, and the
/// file is where it was.
#[test]
fn moving_a_locked_file_aside_does_not_wait() {
    let s = sandbox("aside-locked");
    let f = s.root.join("frames.bin");
    std::fs::write(&f, vec![0u8; 8192]).unwrap();
    let Some(lock) = lock_on(&f) else {
        return;
    };
    let p = f.clone();
    let e = within("move_aside", BOUND, move || move_aside(&p)).unwrap_err();
    assert!(secure::is_oplock_busy(&e), "{e}");
    assert!(f.exists());
    drop(lock);
    let aside = move_aside(&f).unwrap();
    assert!(!f.exists() && aside.exists());
}

/// A Repair that must replace `frames.bin` while a program of the user holds a lock on it ends with an error line
/// that names the file and says why, within the bound (the old code waited in the rename); with the lock gone the
/// same Repair replaces it.
#[test]
fn replacing_a_locked_frame_file_is_an_error_that_names_it_and_does_not_wait() {
    let s = sandbox("replace-locked");
    run(&s, &mut VecLog::default()).unwrap();
    mark(&s);
    // a reason to replace the file (and only it)
    let stream = format!("{}:extra", s.paths.frames().display());
    if std::fs::write(&stream, b"hidden").is_err() {
        eprintln!("this volume has no named streams: skipped");
        return;
    }
    let id = file_id(&s.paths.frames());
    let Some(lock) = lock_on(&s.paths.frames()) else {
        return;
    };
    let (paths, p) = (s.paths.clone(), s.p.clone());
    let stops = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let counted = stops.clone();
    let (r, lines) = within("prepare", BOUND, move || {
        let mut log = VecLog::default();
        let r = prepare(
            &paths,
            &p,
            &mut || {
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                true
            },
            &mut log,
        );
        (r, log.0)
    });
    let err = r.expect_err("the file cannot be replaced while it is locked");
    assert!(
        err.contains("frames.bin") && err.contains("locked by another program"),
        "{err}"
    );
    assert_eq!(
        stops.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "stopping the camera service does not help against a lock of the user: {lines:?}"
    );
    // (reading the file would wait for the lock: it is looked at without opening it for data)
    assert_eq!(file_id(&s.paths.frames()), id, "the file is as it was");
    let names: Vec<String> = std::fs::read_dir(&s.paths.data)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(names.iter().all(|n| !is_aside_name(n)), "{names:?}");
    drop(lock);
    assert!(marked(&s), "the file is as it was");
    let mut log = VecLog::default();
    run(&s, &mut log).unwrap();
    assert!(!marked(&s), "the file was replaced once the lock was gone");
}
// ---- M2: a %ProgramData% that is itself a junction (moved to another drive with a link) works

/// Some people move `%ProgramData%` elsewhere and leave a junction behind. The root is an administrator's, so it is
/// followed; what is below it still must be the real thing. The old code opened the root as a link, so the root
/// resolved to the link and the files below it to the real place, the comparison failed, and everything was refused.
#[test]
fn a_program_data_that_is_a_junction_prepares_checks_and_logs() {
    let s = sandbox("junction-root");
    let real = s.root.join("moved-programdata");
    std::fs::create_dir_all(&real).unwrap();
    std::fs::remove_dir(&s.paths.program_data).unwrap();
    junction(&s.paths.program_data, &real);
    let mut log = VecLog::default();
    run(&s, &mut log).expect("prepare through a junction root");
    // everything is really below the place the junction leads to
    assert!(real.join("BetterCam").join("frames.bin").is_file());
    assert!(real
        .join("BetterCam")
        .join("logs")
        .join("install.log")
        .is_file());
    assert_eq!(
        std::fs::metadata(s.paths.frames()).unwrap().len(),
        vcam_protocol::FILE_SIZE
    );
    for (obj, path) in [
        (Object::DataDir, s.paths.data.clone()),
        (Object::LogsDir, s.paths.logs()),
        (Object::FramesFile, s.paths.frames()),
        (Object::InstallLog, s.paths.install_log()),
    ] {
        acl::check(&security_of(&path), &acl::expected(obj, &s.p), &s.p.owner)
            .unwrap_or_else(|e| panic!("{obj:?}: {e}"));
    }
    // a second run keeps them (it did not find them "not where their path says")
    let mut again = VecLog::default();
    run(&s, &mut again).unwrap();
    assert_eq!(
        again.0.iter().filter(|l| l.contains(": kept")).count(),
        4,
        "{:?}",
        again.0
    );
    // the app's writer (pinned to the root) and the helper's log reader work through it too
    let w = vcam_protocol::Writer::open_precreated(&s.paths.frames(), Some(&s.paths.program_data))
        .expect("the writer is refused behind a junction root");
    drop(w);
    let mut flog = crate::log::FileLog::with_paths_and_principals(s.paths.clone(), s.p.clone());
    assert!(flog.attached());
    flog.line("through the junction");
    let tail = crate::log::tail_of(
        &s.paths,
        std::time::SystemTime::now() - std::time::Duration::from_secs(60),
        3,
    );
    assert!(
        tail.last()
            .is_some_and(|l| l.ends_with("through the junction")),
        "{tail:?}"
    );
    // a junction *below* the root is still not followed
    drop(flog);
    remove_no_follow(&s.paths.logs()).unwrap();
    let elsewhere = s.root.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    junction(&s.paths.logs(), &elsewhere);
    let mut log = VecLog::default();
    run(&s, &mut log).unwrap();
    assert!(mentions(&log, "link or junction"), "{:?}", log.0);
    assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0);
}

// ---- M1: a frame file of another length is replaced before anything of it is allocated

/// Bytes that the file at `p` takes on the disk (`GetCompressedFileSizeW`: for a sparse file only what is stored).
fn on_disk(p: &Path) -> u64 {
    use windows::Win32::Storage::FileSystem::GetCompressedFileSizeW;
    let w = crate::util::wide(&p.to_string_lossy());
    let mut high = 0u32;
    let low = unsafe { GetCompressedFileSizeW(crate::util::pcwstr(&w), Some(&mut high)) };
    (u64::from(high) << 32) | u64::from(low)
}

/// Set the length of the frame file through a handle that has the rights the camera's user has (read and write data).
fn set_length(s: &Sandbox, len: u64, sparse: bool) -> bool {
    let h = secure::open_with(
        &s.paths.frames(),
        0x3,
        secure::SHARE_ALL,
        DISP_OPEN_EXISTING,
        false,
        0,
    )
    .unwrap();
    let file = std::fs::File::from(h);
    if sparse {
        // nothing of it is stored when it is cut to nothing first
        file.set_len(0).unwrap();
        if secure::set_sparse(secure::raw(&file), true).is_err() {
            return false;
        }
    }
    file.set_len(len).unwrap();
    true
}

/// The reviewer's case: the camera's user sets `frames.bin` to 512 MB and marks it sparse (that costs nothing on
/// disk). The Repair used to clear the sparse flag in place, which makes Windows allocate the whole length at once
/// (0 -> 536,870,912 bytes), keep the file, and report success, while the DLL refuses a file longer than
/// `MAX_FILE_SIZE`. Now the length is looked at first and the file is replaced: what is on disk afterwards is a new
/// file of the right length.
#[test]
fn an_oversized_sparse_frame_file_is_replaced_and_its_length_is_never_allocated() {
    const BIG: u64 = 512 << 20;
    let s = sandbox("oversized-sparse");
    run(&s, &mut VecLog::default()).unwrap();
    let frames = s.paths.frames();
    // a file of the user's making: plain, sparse, 512 MB (the file the helper made is `FILE_ATTRIBUTE_TEMPORARY`, which
    // this volume gives storage to when it is extended; a user's own file is not)
    remove_no_follow(&frames).unwrap();
    {
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&frames)
            .unwrap();
        if secure::set_sparse(secure::raw(&f), true).is_err() {
            eprintln!("this volume cannot make a file sparse: skipped");
            return;
        }
        f.set_len(BIG).unwrap();
    }
    let id = file_id(&frames);
    let before = on_disk(&frames);
    assert_eq!(std::fs::metadata(&frames).unwrap().len(), BIG);
    assert_ne!(attributes_of(&frames) & secure::ATTRIBUTE_SPARSE, 0);
    if before > 16 << 20 {
        eprintln!("this volume stores a sparse file in full ({before} bytes): skipped");
        return;
    }

    let mut log = VecLog::default();
    run(&s, &mut log).unwrap();

    // looked at first: nothing of the 512 MB was given storage (the old code: 0 -> 536870912)
    let after = on_disk(&frames);
    assert!(
        after < 64 << 20,
        "the length was allocated: {before} -> {after} bytes on disk ({:?})",
        log.0
    );
    assert_eq!(
        std::fs::metadata(&frames).unwrap().len(),
        vcam_protocol::FILE_SIZE,
        "the file has to end at the size the camera expects"
    );
    assert_eq!(attributes_of(&frames) & secure::ATTRIBUTE_SPARSE, 0);
    assert_ne!(
        file_id(&frames),
        id,
        "an oversized file is replaced, not kept"
    );
    // it is sparse: that alone is the reason now (the length is looked at in the replacement cases below)
    assert!(
        mentions(&log, "it is sparse or compressed; moving it aside"),
        "{:?}",
        log.0
    );
    assert!(
        !mentions(&log, "in place"),
        "the flag must not be cleared, whatever the length: {:?}",
        log.0
    );
    // and the copy that was moved aside is deleted (it cost nothing, and goes at once)
    let names: Vec<String> = std::fs::read_dir(&s.paths.data)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(names.iter().all(|n| !is_aside_name(n)), "{names:?}");
    // the new file is kept the next time
    let mut log = VecLog::default();
    run(&s, &mut log).unwrap();
    assert!(
        log.0.iter().any(|l| l.ends_with("frames.bin: kept")),
        "{:?}",
        log.0
    );
}

/// A file that is too short or too long and not sparse also ends at exactly the expected size.
#[test]
fn an_undersized_and_a_wrong_sized_frame_file_end_at_the_expected_size() {
    let size = vcam_protocol::FILE_SIZE;
    for (name, len) in [
        ("empty", 0),
        ("half", size / 2),
        ("one-short", size - 1),
        ("one-over", size + 1),
        ("double", size * 2),
    ] {
        let s = sandbox(&format!("wrong-length-{name}"));
        run(&s, &mut VecLog::default()).unwrap();
        let id = file_id(&s.paths.frames());
        assert!(set_length(&s, len, false));
        assert_eq!(std::fs::metadata(s.paths.frames()).unwrap().len(), len);
        let mut log = VecLog::default();
        run(&s, &mut log).unwrap_or_else(|e| panic!("{name}: {e}: {:?}", log.0));
        assert_eq!(
            std::fs::metadata(s.paths.frames()).unwrap().len(),
            size,
            "{name}: {:?}",
            log.0
        );
        assert_ne!(file_id(&s.paths.frames()), id, "{name}: kept");
        assert!(
            mentions(&log, &format!("its length is {len} bytes")),
            "{name}: {:?}",
            log.0
        );
        // the file is fine to the camera: the writer opens it
        vcam_protocol::Writer::open_precreated(&s.paths.frames(), Some(&s.paths.program_data))
            .unwrap_or_else(|e| panic!("{name}: the app cannot use it: {e}"));
    }
}

/// A frame file of the right length with a flag set is **replaced**, not cleared in place: it used to be kept (the
/// file index stayed the same), which is the code path that allocated a file's whole length when the length had
/// been changed after the look at it. The reason is said in the log.
#[test]
fn a_right_sized_sparse_frame_file_is_replaced_and_not_cleared_in_place() {
    let s = sandbox("sized-sparse");
    run(&s, &mut VecLog::default()).unwrap();
    mark(&s);
    let id = file_id(&s.paths.frames());
    let h = secure::open_with(
        &s.paths.frames(),
        0x3,
        secure::SHARE_ALL,
        DISP_OPEN_EXISTING,
        false,
        0,
    )
    .unwrap();
    if secure::set_sparse(secure::raw(&h), true).is_err() {
        eprintln!("this volume cannot make a file sparse: skipped");
        return;
    }
    drop(h);
    let mut log = VecLog::default();
    run(&s, &mut log).unwrap();
    assert!(
        mentions(&log, "it is sparse or compressed; moving it aside"),
        "{:?}",
        log.0
    );
    assert!(!mentions(&log, "in place"), "{:?}", log.0);
    assert_ne!(file_id(&s.paths.frames()), id, "the file was kept");
    assert!(!marked(&s));
    assert_plain_new_file(&s, id, "right-sized sparse");
}

// ---- L2, L3: the sweep acts on the names the helper recorded, and says little

/// Four hundred entries a user made under the names of the helper's old data folders cost nothing: none is opened,
/// none is deleted, and nothing is written to the log (the old sweep opened each, read its owner and logged a line
/// for each, which pushed the start of the run out of the tail the app reads).
#[test]
fn entries_that_a_user_made_under_old_names_are_not_looked_at_and_do_not_flood_the_log() {
    let s = sandbox("flood-unrecorded");
    let pd = &s.paths.program_data;
    for i in 0..400 {
        std::fs::create_dir_all(pd.join(format!("BetterCam.old-flood-{i}"))).unwrap();
    }
    let mut log = VecLog::default();
    let swept = sweep_data_asides(&s.paths, &[acl::SID_SYSTEM.to_string()], &[], &mut log);
    assert_eq!(swept, AsideSweep::default());
    assert!(
        log.0.is_empty(),
        "{} lines: {:?}",
        log.0.len(),
        &log.0[..log.0.len().min(3)]
    );
    // trusted or not, nothing that is not on record is touched
    let mut log = VecLog::default();
    sweep_asides(&s.paths, &s.p.trusted_owners, &[], &mut log);
    assert!(log.0.is_empty(), "{:?}", log.0);
    assert_eq!(
        std::fs::read_dir(pd)
            .unwrap()
            .flatten()
            .filter(|e| e
                .file_name()
                .to_string_lossy()
                .starts_with("BetterCam.old-flood-"))
            .count(),
        400
    );
}

/// A record that is long, with entries that are not the helper's, gives one summary line (the count and the first
/// few names) and a bounded number of looks; the names that were not looked at stay on record.
#[test]
fn recorded_entries_that_are_left_alone_are_one_line_and_the_number_looked_at_is_bounded() {
    let s = sandbox("flood-recorded");
    let pd = &s.paths.program_data;
    let recorded: Vec<String> = (0..100)
        .map(|i| format!("BetterCam.old-r-{i:03}"))
        .collect();
    for n in &recorded {
        std::fs::create_dir_all(pd.join(n)).unwrap();
    }
    let entries: Vec<AsideEntry> = recorded
        .iter()
        .map(|n| crate::testkit::entry_of(pd, n))
        .collect();
    // only SYSTEM is trusted: the sandbox user made all of them
    let mut log = VecLog::default();
    let swept = sweep_data_asides(&s.paths, &[acl::SID_SYSTEM.to_string()], &entries, &mut log);
    assert_eq!(swept.left, 0);
    assert_eq!(swept.forget.len(), MAX_ASIDES_EXAMINED, "{swept:?}");
    assert_eq!(swept.forget[..], recorded[..MAX_ASIDES_EXAMINED]);
    assert_eq!(
        log.0.len(),
        2,
        "one summary line and one about the names not looked at: {:?}",
        log.0
    );
    let summary = &log.0[0];
    assert!(
        summary.contains(&format!("{MAX_ASIDES_EXAMINED} recorded")),
        "{summary}"
    );
    for shown in &recorded[..NAMES_SHOWN] {
        assert!(summary.contains(shown.as_str()), "{summary}");
    }
    assert!(
        !summary.contains(recorded[NAMES_SHOWN].as_str())
            && summary.contains(&format!("and {} more", MAX_ASIDES_EXAMINED - NAMES_SHOWN)),
        "{summary}"
    );
    assert!(
        log.0[1].contains(&format!("{}", 100 - MAX_ASIDES_EXAMINED))
            && log.0[1].contains("next runs"),
        "{:?}",
        log.0
    );
    // nothing was deleted
    assert!(recorded.iter().all(|n| pd.join(n).is_dir()));
}

/// The helper deletes the names on its record: a recorded folder that is its own, nothing else. An entry that
/// somebody made under a name of that kind is left (a user can make one; so can an administrator-owned folder that
/// a user may rename into `%ProgramData%`, which an owner check alone would trust). A recorded name that became a
/// link or has another owner is left as it is and taken off the record; one that is gone too; a damaged record
/// cannot lead out of the folder.
#[test]
fn only_the_recorded_names_are_swept_and_the_record_is_cleared_of_what_is_done_with() {
    let s = sandbox("record");
    let pd = &s.paths.program_data;
    let (dir, frames, logs) = elsewhere(&s);
    let mk = |n: &str| {
        std::fs::create_dir_all(pd.join(n).join("logs")).unwrap();
        std::fs::write(pd.join(n).join("frames.bin"), b"old").unwrap();
    };
    mk("BetterCam.old-1-0"); // recorded, the helper's: goes
    mk("BetterCam.old-2-0"); // owned by a trusted account, but not recorded: stays
    junction(&pd.join("BetterCam.old-3-0"), &dir); // recorded, a link now: stays, not followed
    mk("BetterCam.old-5-0"); // recorded, but its owner is not trusted: stays
    std::fs::write(s.root.join("victim.txt"), b"victim").unwrap();
    let recorded = entries(
        pd,
        &[
            "BetterCam.old-1-0",
            "BetterCam.old-3-0",
            "BetterCam.old-4-0", // gone
            "BetterCam.old-5-0",
            r"BetterCam.old-\..\..\victim.txt", // a damaged record
            "Other.old-1-0",
        ],
    );
    let mut log = VecLog::default();
    // 1-0 and 3-0 are owned by the sandbox user, who is trusted in this first pass; 5-0 is judged below
    let first: Vec<AsideEntry> = recorded
        .iter()
        .filter(|e| !e.name.ends_with("5-0"))
        .cloned()
        .collect();
    let swept = sweep_data_asides(&s.paths, &s.p.trusted_owners, &first, &mut log);
    assert_eq!(swept.left, 0, "{:?}", log.0);
    assert_eq!(
        swept.forget,
        names_of(&first),
        "all of them are done with: {:?}",
        log.0
    );
    assert!(!pd.join("BetterCam.old-1-0").exists());
    assert!(
        pd.join("BetterCam.old-2-0").join("frames.bin").is_file(),
        "not on record"
    );
    assert!(is_link(&pd.join("BetterCam.old-3-0")));
    untouched(&dir, &frames, &logs);
    assert_eq!(std::fs::read(s.root.join("victim.txt")).unwrap(), b"victim");
    assert_eq!(log.0.len(), 2, "one deletion and one summary: {:?}", log.0);
    // the owner is not trusted any more
    let mut log = VecLog::default();
    let swept = sweep_data_asides(
        &s.paths,
        &[acl::SID_SYSTEM.to_string()],
        &entries(pd, &["BetterCam.old-5-0"]),
        &mut log,
    );
    assert_eq!(swept.forget, ["BetterCam.old-5-0"]);
    assert!(pd.join("BetterCam.old-5-0").join("frames.bin").is_file());
    assert!(mentions(&log, "left alone"), "{:?}", log.0);
    remove_no_follow(&pd.join("BetterCam.old-3-0")).unwrap();
}

/// The branch of the sweep for an entry that was swapped between the first look and the delete: the junction that
/// took its place is not entered, not deleted, and the target is untouched.
#[test]
fn an_entry_that_is_swapped_for_a_junction_after_the_first_look_is_left_alone() {
    let s = sandbox("swapped-link");
    let pd = &s.paths.program_data;
    let (dir, frames, logs) = elsewhere(&s);
    let entry = pd.join("BetterCam.old-1-0");
    std::fs::create_dir_all(entry.join("logs")).unwrap();
    std::fs::write(entry.join("frames.bin"), b"old").unwrap();
    let recorded = entries(pd, &["BetterCam.old-1-0"]);
    let mut seen = Vec::new();
    let mut log = VecLog::default();
    let swept = sweep_data_asides_with(
        &s.paths,
        &s.p.trusted_owners,
        &recorded,
        &mut log,
        &mut |p| {
            seen.push(p.to_path_buf());
            // the entry is looked at, found good, and replaced before the delete opens it
            remove_no_follow(p).unwrap();
            junction(p, &dir);
            None
        },
    );
    assert_eq!(
        seen,
        std::slice::from_ref(&entry),
        "the hook runs between the two looks"
    );
    assert_eq!(swept.left, 0, "{:?}", log.0);
    assert_eq!(swept.forget, names_of(&recorded));
    assert!(
        is_link(&entry),
        "the junction that took its place is not deleted"
    );
    assert!(
        mentions(&log, "left alone") && mentions(&log, "BetterCam.old-1-0"),
        "{:?}",
        log.0
    );
    untouched(&dir, &frames, &logs);
    remove_no_follow(&entry).unwrap();
}

/// The other half: the owner that the handle shows is not a trusted one (here the trusted set the second look uses
/// is changed, as the owner of a real entry cannot be changed without a privilege).
#[test]
fn an_entry_whose_owner_is_not_trusted_on_the_handle_is_left_alone() {
    let s = sandbox("swapped-owner");
    let entry = s.paths.program_data.join("BetterCam.old-1-0");
    std::fs::create_dir_all(entry.join("logs")).unwrap();
    std::fs::write(entry.join("frames.bin"), b"old").unwrap();
    let recorded = entries(&s.paths.program_data, &["BetterCam.old-1-0"]);
    let mut log = VecLog::default();
    let swept = sweep_data_asides_with(
        &s.paths,
        &s.p.trusted_owners,
        &recorded,
        &mut log,
        &mut |_| Some(vec![acl::SID_SYSTEM.to_string()]),
    );
    assert_eq!(swept.left, 0);
    assert_eq!(swept.forget, names_of(&recorded));
    assert_eq!(std::fs::read(entry.join("frames.bin")).unwrap(), b"old");
    assert!(mentions(&log, "left alone"), "{:?}", log.0);
    assert!(!mentions(&log, "deleted old data folder"), "{:?}", log.0);
}

// ---- the record: written before the rename, only for the helper's own, and swept by identity

/// A ledger in memory that notes what is asked of it, and whether the folder it is about is still where it was at
/// the time.
#[derive(Default)]
struct Led {
    /// `(entry, the watched path still existed)` per `add`.
    adds: Vec<(AsideEntry, bool)>,
    removes: Vec<String>,
    /// The order of everything that happened.
    events: Vec<String>,
    watch: Option<PathBuf>,
    fail_add: bool,
}

impl Ledger for Led {
    fn add(&mut self, e: &AsideEntry) -> Res<()> {
        let there = self.watch.as_ref().is_some_and(|w| w.exists());
        self.events.push(format!("add {}", e.name));
        if self.fail_add {
            return Err("write failed (injected)".into());
        }
        self.adds.push((e.clone(), there));
        Ok(())
    }
    fn remove(&mut self, name: &str) -> Res<()> {
        self.events.push(format!("remove {name}"));
        self.removes.push(name.to_string());
        Ok(())
    }
}

/// A folder `data` in the sandbox with an object in it that cannot be deleted (so the old copy stays).
fn data_folder_that_stays(s: &Sandbox) -> PathBuf {
    let data = s.root.join("ProgramData").join("BetterCam");
    std::fs::create_dir_all(&data).unwrap();
    crate::testkit::undeletable_file(&data, "junk.txt", &s.p.owner);
    data
}

/// A data folder of the helper's (a trusted owner) is put on the record **before** it is renamed, with the identity of
/// the folder (read on the handle that is renamed: the same value the folder had before and has under its new name),
/// and stays on the record when the old copy cannot be deleted.
#[test]
fn a_data_folder_is_put_on_record_before_the_rename_with_its_identity_and_stays_there_when_it_stays(
) {
    let s = sandbox("record-before-rename");
    let data = data_folder_that_stays(&s);
    let id = crate::testkit::identity_of(&data);
    let mut led = Led {
        watch: Some(data.clone()),
        ..Led::default()
    };
    let mut log = VecLog::default();
    let mut rec = Recording {
        ledger: &mut led,
        trusted: &s.p.trusted_owners,
    };
    replace_aside(&data, "test", &mut || false, &mut log, Some(&mut rec)).unwrap();
    let asides = crate::testkit::asides_in(&s.paths);
    assert_eq!(asides.len(), 1, "{:?}", log.0);
    let name = asides[0]
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert_eq!(led.adds.len(), 1, "{:?}", led.events);
    let (entry, existed) = &led.adds[0];
    assert!(
        *existed,
        "it was written after the rename: the old name was gone"
    );
    assert_eq!(entry.name, name);
    assert_eq!(
        entry.identity, id,
        "the identity of the folder that was renamed"
    );
    assert_eq!(
        crate::testkit::identity_of(&asides[0]),
        id,
        "a rename keeps the identity: this is what the sweep compares"
    );
    assert!(
        led.removes.is_empty(),
        "it stays on the record: {:?}",
        led.events
    );
    assert!(mentions(&log, "it is on record"), "{:?}", log.0);
    // what the next sweep does with it: the identity matches, the owner is trusted, the file is the one thing in the way
    crate::testkit::make_deletable(&asides[0].join("junk.txt"), &s.p.owner);
    let swept = sweep_data_asides(
        &s.paths,
        &s.p.trusted_owners,
        std::slice::from_ref(entry),
        &mut VecLog::default(),
    );
    assert_eq!(swept.forget, std::slice::from_ref(&name));
    assert!(
        !asides[0].exists(),
        "a recorded folder whose identity matches is deleted"
    );
}

/// A copy that is deleted at once is taken off the record again; a rename that does not happen takes the entry off
/// again too (it was written first), and the folder is where it was.
#[test]
fn the_entry_is_dropped_again_when_the_copy_is_deleted_or_the_rename_fails() {
    let s = sandbox("record-dropped");
    // deleted at once
    let plain = s.root.join("ProgramData").join("BetterCam");
    std::fs::create_dir_all(plain.join("logs")).unwrap();
    let mut led = Led::default();
    let mut rec = Recording {
        ledger: &mut led,
        trusted: &s.p.trusted_owners,
    };
    replace_aside(
        &plain,
        "test",
        &mut || false,
        &mut VecLog::default(),
        Some(&mut rec),
    )
    .unwrap();
    assert!(!plain.exists() && crate::testkit::asides_in(&s.paths).is_empty());
    assert_eq!(led.adds.len(), 1);
    assert_eq!(
        led.removes,
        [led.adds[0].0.name.clone()],
        "{:?}",
        led.events
    );
    assert_eq!(led.events.len(), 2, "add, then remove: {:?}", led.events);

    // a rename that fails: a program holds a read-write-handle lock on a file in the folder, which the rename of the
    // folder must break (it waits for the holder, is cancelled at the bound, and does not happen)
    std::fs::create_dir_all(plain.join("logs")).unwrap();
    std::fs::write(plain.join("logs").join("a.log"), vec![0u8; 4096]).unwrap();
    let Some(lock) = secure::oplock::Oplock::take_level(
        &plain.join("logs").join("a.log"),
        secure::oplock::READ_WRITE_HANDLE,
    )
    .unwrap() else {
        eprintln!("this file system grants no oplock: that part is skipped");
        return;
    };
    let mut led = Led::default();
    let mut log = VecLog::default();
    let mut rec = Recording {
        ledger: &mut led,
        trusted: &s.p.trusted_owners,
    };
    let e = replace_aside(&plain, "test", &mut || false, &mut log, Some(&mut rec)).unwrap_err();
    assert!(e.contains("locked by another program"), "{e}");
    assert!(plain.is_dir(), "the folder is where it was");
    assert!(crate::testkit::asides_in(&s.paths).is_empty());
    assert_eq!(led.adds.len(), 1, "it was written first: {:?}", led.events);
    assert_eq!(
        led.events,
        [
            format!("add {}", led.adds[0].0.name),
            format!("remove {}", led.adds[0].0.name)
        ],
        "and taken off again, as nothing was moved"
    );
    drop(lock);
}

/// A move whose entry could not be written down still happens (the old copy is deleted at once, or stays inert), and
/// the failure is a line of the log.
#[test]
fn a_record_that_cannot_be_written_does_not_stop_the_move() {
    let s = sandbox("record-add-fails");
    let data = data_folder_that_stays(&s);
    let mut led = Led {
        fail_add: true,
        ..Led::default()
    };
    let mut log = VecLog::default();
    let mut rec = Recording {
        ledger: &mut led,
        trusted: &s.p.trusted_owners,
    };
    replace_aside(&data, "test", &mut || false, &mut log, Some(&mut rec)).unwrap();
    assert!(
        mentions(&log, "could not put") && mentions(&log, "on record"),
        "{:?}",
        log.0
    );
    assert!(
        mentions(&log, "so it is not put on record") || mentions(&log, "left where it is"),
        "{:?}",
        log.0
    );
    let asides = crate::testkit::asides_in(&s.paths);
    assert_eq!(asides.len(), 1);
    assert!(
        led.removes.is_empty(),
        "nothing was written, nothing to take off"
    );
    crate::testkit::make_deletable(&asides[0].join("junk.txt"), &s.p.owner);
}

/// An entry that is not owned by SYSTEM or Administrators when it is moved aside is **never recorded** (a standard
/// user may make a data folder when `BetterCam` does not exist, keep a file open in it so that the delete fails, and
/// with the old code had the name recorded), and one that is a link is not either. Each is left where it is, with one
/// line of the log; a link is deleted as a link (its target is left alone).
#[test]
fn an_untrusted_owner_and_a_junction_are_never_put_on_record() {
    let s = sandbox("record-untrusted-datadir");
    // 1. owned by somebody who is not trusted (only SYSTEM is), with a file that cannot be deleted
    let data = data_folder_that_stays(&s);
    let mut led = Led::default();
    let mut log = VecLog::default();
    let only_system = vec![acl::SID_SYSTEM.to_string()];
    let mut rec = Recording {
        ledger: &mut led,
        trusted: &only_system,
    };
    replace_aside(&data, "test", &mut || false, &mut log, Some(&mut rec)).unwrap();
    assert!(
        led.events.is_empty(),
        "something was recorded: {:?}",
        led.events
    );
    let asides = crate::testkit::asides_in(&s.paths);
    assert_eq!(asides.len(), 1, "{:?}", log.0);
    let kept: Vec<&String> = log
        .0
        .iter()
        .filter(|l| l.contains("old copy kept as"))
        .collect();
    assert_eq!(kept.len(), 1, "{:?}", log.0);
    assert!(
        kept[0].contains("left where it is") && kept[0].contains("not SYSTEM or Administrators"),
        "{}",
        kept[0]
    );
    crate::testkit::make_deletable(&asides[0].join("junk.txt"), &s.p.owner);
    remove_no_follow(&asides[0]).unwrap();

    // 2. a junction (owned by the helper's own account, trusted): it is a link, so it is not recorded; it is
    // deleted as a link, and what it points to is left alone
    let (dir, frames, logs) = elsewhere(&s);
    junction(&data, &dir);
    let mut led = Led::default();
    let mut rec = Recording {
        ledger: &mut led,
        trusted: &s.p.trusted_owners,
    };
    replace_aside(
        &data,
        "test",
        &mut || false,
        &mut VecLog::default(),
        Some(&mut rec),
    )
    .unwrap();
    assert!(
        led.events.is_empty(),
        "a link was recorded: {:?}",
        led.events
    );
    assert!(!data.exists() && crate::testkit::asides_in(&s.paths).is_empty());
    untouched(&dir, &frames, &logs);
}

/// The reviewer's chain: a name is on the record; the folder behind it is deleted; **another folder with a trusted
/// owner** (in the sandbox the same account, which is trusted; for real an administrator-owned folder that a user may
/// rename) takes its name. The old sweep saw "a recorded name, not a link, owner trusted" and deleted it with
/// administrator rights. Now the folder is not the one that was recorded: it is left exactly as it is and dropped
/// from the record.
#[test]
fn a_recorded_name_that_was_freed_and_filled_with_a_trusted_folder_is_left_untouched() {
    let s = sandbox("swap-chain");
    let pd = &s.paths.program_data;
    let name = "BetterCam.old-1-0";
    std::fs::create_dir_all(pd.join(name).join("logs")).unwrap();
    std::fs::write(pd.join(name).join("frames.bin"), b"theirs").unwrap();
    let recorded = vec![crate::testkit::entry_of(pd, name)];
    // control: the recorded folder itself is deleted (same name, same identity)
    let mut log = VecLog::default();
    let swept = sweep_data_asides(&s.paths, &s.p.trusted_owners, &recorded, &mut log);
    assert_eq!(swept.forget, [name]);
    assert!(
        !pd.join(name).exists(),
        "the recorded folder goes: {:?}",
        log.0
    );
    // the chain: the folder is gone, and another one with a trusted owner is renamed into the name
    let victim = pd.join("Victim");
    std::fs::create_dir_all(victim.join("sub")).unwrap();
    std::fs::write(victim.join("precious.txt"), b"precious").unwrap();
    std::fs::rename(&victim, pd.join(name)).unwrap();
    let mut log = VecLog::default();
    let swept = sweep_data_asides(&s.paths, &s.p.trusted_owners, &recorded, &mut log);
    assert_eq!(swept.left, 0, "{:?}", log.0);
    assert_eq!(swept.forget, [name], "dropped from the record");
    assert!(swept.kept.is_empty());
    assert_eq!(
        std::fs::read(pd.join(name).join("precious.txt")).unwrap(),
        b"precious",
        "it was deleted: {:?}",
        log.0
    );
    assert!(pd.join(name).join("sub").is_dir());
    assert!(
        mentions(&log, "left alone") && mentions(&log, name),
        "{:?}",
        log.0
    );
    assert!(!mentions(&log, "deleted old data folder"), "{:?}", log.0);
}

/// The same swap in the moment between the first look at the entry and the delete (the second look is on the handle
/// that is deleted through, and compares the identity there).
#[test]
fn an_entry_that_is_swapped_for_another_folder_after_the_first_look_is_left_alone() {
    let s = sandbox("swap-after-look");
    let pd = &s.paths.program_data;
    let name = "BetterCam.old-1-0";
    std::fs::create_dir_all(pd.join(name)).unwrap();
    let recorded = vec![crate::testkit::entry_of(pd, name)];
    let mut log = VecLog::default();
    let swept = sweep_data_asides_with(
        &s.paths,
        &s.p.trusted_owners,
        &recorded,
        &mut log,
        &mut |p| {
            // looked at and found good; then replaced by another folder of a trusted owner
            remove_no_follow(p).unwrap();
            std::fs::create_dir_all(p.join("sub")).unwrap();
            std::fs::write(p.join("precious.txt"), b"precious").unwrap();
            None
        },
    );
    assert_eq!(swept.forget, [name], "{:?}", log.0);
    assert_eq!(swept.left, 0);
    assert_eq!(
        std::fs::read(pd.join(name).join("precious.txt")).unwrap(),
        b"precious"
    );
    assert!(mentions(&log, "left alone"), "{:?}", log.0);
}

/// Names that are not the helper's, that Windows would read as another name (a trailing dot), or that the system
/// refuses are forgotten (they used to stay on the record for ever and use up a slot of every run), and are never
/// joined to a path: a trailing dot does not reach the folder without it.
#[test]
fn invalid_names_are_forgotten_and_never_reach_another_entry() {
    let s = sandbox("invalid-names");
    let pd = &s.paths.program_data;
    std::fs::create_dir_all(pd.join("BetterCam.old-1-0").join("logs")).unwrap();
    let real = crate::testkit::entry_of(pd, "BetterCam.old-1-0");
    let made_up = |name: &str| AsideEntry {
        name: name.to_string(),
        identity: real.identity, // the real identity: only the name is wrong
    };
    let bad = [
        "BetterCam.old-1-0.", // Windows drops the dot and would open the folder above
        "BetterCam.old-1-0 ",
        "BetterCam.old-1*",
        "BetterCam.old-\u{0}",
        "BetterCam.old-con:x",
        r"BetterCam.old-..\..\x",
        "BetterCam",
        "Other.old-1-0",
        "",
    ];
    let recorded: Vec<AsideEntry> = bad.iter().map(|n| made_up(n)).collect();
    let mut log = VecLog::default();
    let swept = sweep_data_asides(&s.paths, &s.p.trusted_owners, &recorded, &mut log);
    assert_eq!(swept.forget, bad, "{:?}", log.0);
    assert_eq!(swept.left, 0);
    assert!(swept.kept.is_empty());
    assert!(
        pd.join("BetterCam.old-1-0").join("logs").is_dir(),
        "a name with a trailing dot reached the folder without it: {:?}",
        log.0
    );
    assert_eq!(log.0.len(), 1, "one summary line: {:?}", log.0);
    // the errors of an open that mean "not a name"
    for code in [123, 161, 206] {
        assert!(
            is_invalid_name_error(&std::io::Error::from_raw_os_error(code)),
            "{code}"
        );
    }
    assert!(is_invalid_name_error(&std::io::Error::from(
        std::io::ErrorKind::InvalidInput
    )));
    for code in [2, 3, 5, 32, 1224] {
        assert!(
            !is_invalid_name_error(&std::io::Error::from_raw_os_error(code)),
            "{code}"
        );
    }
}

/// A guard that gives every undeletable file in `dirs` its permissions back, also when the test fails.
struct Cleanup(Vec<PathBuf>, String);
impl Drop for Cleanup {
    fn drop(&mut self) {
        for d in &self.0 {
            crate::testkit::make_deletable(&d.join("junk.txt"), &self.1);
        }
    }
}

/// Entries that stay (a file in the folder cannot be deleted) are looked at 32 per run. The ones that were looked at
/// go to the end of the record, so the others are reached: with 40 of them, every entry is looked at within two runs
/// (the old order looked at the first 32 for ever: entries 33 to 40 were never reached).
#[test]
fn every_entry_of_a_long_record_is_reached_within_two_runs() {
    use crate::testkit::MemStore;
    let s = sandbox("rotation");
    let pd = &s.paths.program_data;
    let names: Vec<String> = (1..=40)
        .map(|i| format!("BetterCam.old-r-{i:02}"))
        .collect();
    let mut dirs = Vec::new();
    for n in &names {
        let d = pd.join(n);
        std::fs::create_dir_all(&d).unwrap();
        crate::testkit::undeletable_file(&d, "junk.txt", &s.p.owner);
        dirs.push(d);
    }
    let _cleanup = Cleanup(dirs, s.p.owner.clone());
    let mut store = MemStore::default();
    for n in &names {
        aside::add(&mut store, &crate::testkit::entry_of(pd, n)).unwrap();
    }
    assert_eq!(store.names(), names);
    let run = |store: &mut MemStore| -> (AsideSweep, Vec<String>) {
        let mut log = VecLog::default();
        let recorded = aside::load_logged(store, &mut log).unwrap();
        let swept = sweep_data_asides(&s.paths, &s.p.trusted_owners, &recorded, &mut log);
        aside::finish_sweep(store, &swept).unwrap();
        (swept, log.0)
    };
    // run one: the first 32 are looked at (they are in use: none is deleted) and the log is truthful about the rest
    let (first, log1) = run(&mut store);
    assert_eq!(first.left, MAX_ASIDES_EXAMINED, "{log1:?}");
    assert_eq!(first.kept, names[..MAX_ASIDES_EXAMINED]);
    assert!(first.forget.is_empty());
    let looked = |log: &[String], n: &str| {
        log.iter()
            .any(|l| l.contains(&format!("old data folder {n} still in use")))
    };
    assert!(looked(&log1, &names[0]) && looked(&log1, &names[31]));
    assert!(
        !looked(&log1, &names[39]),
        "entry 40 cannot be reached by the first run"
    );
    assert!(
        log1.iter()
            .any(|l| l.contains("8 more") && l.contains("next runs")),
        "{log1:?}"
    );
    // the ones that were looked at went to the end
    let order = store.names();
    assert_eq!(
        order[..8],
        names[32..],
        "the entries not looked at come first"
    );
    assert_eq!(order[8..], names[..32]);
    // run two: entry 40 is first in line
    let (second, log2) = run(&mut store);
    assert!(
        looked(&log2, &names[39]),
        "entry 40 was not reached in the second run: {log2:?}"
    );
    assert!(looked(&log2, &names[32]));
    assert_eq!(second.left, MAX_ASIDES_EXAMINED);
    // and every entry has been looked at in one of the two runs
    for n in &names {
        assert!(
            looked(&log1, n) || looked(&log2, n),
            "{n} was never reached"
        );
    }
    assert_eq!(store.names().len(), 40, "nothing was lost from the record");
}

/// The rotation does not hide an entry that goes: forgotten entries leave the record, and the ones that stay keep
/// their relative order at the end.
#[test]
fn entries_that_are_done_with_leave_the_record_during_rotation() {
    use crate::testkit::MemStore;
    let s = sandbox("rotation-forget");
    let pd = &s.paths.program_data;
    let mut store = MemStore::default();
    let kept_dir = pd.join("BetterCam.old-k-1");
    std::fs::create_dir_all(&kept_dir).unwrap();
    crate::testkit::undeletable_file(&kept_dir, "junk.txt", &s.p.owner);
    let _cleanup = Cleanup(vec![kept_dir.clone()], s.p.owner.clone());
    let gone = pd.join("BetterCam.old-g-1");
    std::fs::create_dir_all(&gone).unwrap();
    for n in ["BetterCam.old-k-1", "BetterCam.old-g-1"] {
        aside::add(&mut store, &crate::testkit::entry_of(pd, n)).unwrap();
    }
    let mut log = VecLog::default();
    let recorded = aside::load_logged(&mut store, &mut log).unwrap();
    let swept = sweep_data_asides(&s.paths, &s.p.trusted_owners, &recorded, &mut log);
    aside::finish_sweep(&mut store, &swept).unwrap();
    assert_eq!(swept.forget, ["BetterCam.old-g-1"]);
    assert_eq!(swept.kept, ["BetterCam.old-k-1"]);
    assert_eq!(store.names(), ["BetterCam.old-k-1"]);
}

// ---- M1 (P-hardening 7): a compressed %ProgramData% or drive hands the attribute to everything made in it

fn compressed(p: &Path) -> bool {
    attributes_of(p) & secure::ATTRIBUTE_COMPRESSED != 0
}

fn sparse_or_compressed(p: &Path) -> bool {
    attributes_of(p) & (secure::ATTRIBUTE_SPARSE | secure::ATTRIBUTE_COMPRESSED) != 0
}

/// What a Repair must leave in a data folder that is made on a compressed `%ProgramData%`: the frame file is exactly
/// the expected size and plain.
fn assert_plain_frame_file(s: &Sandbox, what: &str) {
    let frames = s.paths.frames();
    assert_eq!(
        std::fs::metadata(&frames).unwrap().len(),
        vcam_protocol::FILE_SIZE,
        "{what}"
    );
    assert!(
        !sparse_or_compressed(&frames),
        "{what}: the file is not plain"
    );
}

/// The camera's writer opens the file (it writes the header: nothing of the old contents survives that).
fn assert_writer_opens(s: &Sandbox, what: &str) {
    vcam_protocol::Writer::open_precreated(&s.paths.frames(), Some(&s.paths.program_data))
        .unwrap_or_else(|e| panic!("{what}: the app cannot use it: {e}"));
}

/// The reviewer's case. `compact /c` on `%ProgramData%` (or a compressed drive): the new `BetterCam` folder and the
/// new `frames.bin` in it are compressed, and the Install used to end with "frames.bin is sparse or compressed:
/// somebody is changing it; run Repair again". Now the first install succeeds, the frame file is exactly
/// `FILE_SIZE` and plain, and the data folder does not hand compression on.
#[test]
fn a_first_install_on_a_compressed_program_data_makes_a_plain_frame_file() {
    let Some(s) = compressed_sandbox("compressed-first") else {
        return;
    };
    let mut log = VecLog::default();
    run(&s, &mut log).unwrap_or_else(|e| panic!("{e}: {:?}", log.0));
    assert_plain_frame_file(&s, "first install");
    assert_writer_opens(&s, "first install");
    assert!(
        !compressed(&s.paths.data),
        "the data folder is still compressed: {:?}",
        log.0
    );
    assert!(
        mentions(&log, "was compressed: cleared that"),
        "the folder's state is said: {:?}",
        log.0
    );
    assert!(
        !mentions(&log, "sparse or compressed"),
        "the file was not made plain on the first try: {:?}",
        log.0
    );
    // `logs` and `install.log` were made after the folder was cleared
    assert!(!compressed(&s.paths.logs()));
    assert!(!compressed(&s.paths.install_log()));
}

/// A Repair on it succeeds and keeps the very same file: no replace loop (the old code replaced the compressed file
/// with another compressed one, and failed, at every Repair).
#[test]
fn a_repair_on_a_compressed_program_data_keeps_the_same_frame_file() {
    let Some(s) = compressed_sandbox("compressed-repair") else {
        return;
    };
    run(&s, &mut VecLog::default()).unwrap();
    mark(&s);
    let id = file_id(&s.paths.frames());
    for round in 0..3 {
        let mut log = VecLog::default();
        run(&s, &mut log).unwrap_or_else(|e| panic!("repair {round}: {e}: {:?}", log.0));
        assert_eq!(file_id(&s.paths.frames()), id, "repair {round}: replaced");
        assert!(marked(&s), "repair {round}: the contents changed");
        assert!(
            log.0.iter().any(|l| l.ends_with("frames.bin: kept")),
            "repair {round}: {:?}",
            log.0
        );
        assert!(
            !mentions(&log, "moving it aside"),
            "repair {round}: {:?}",
            log.0
        );
        assert_plain_frame_file(&s, &format!("repair {round}"));
    }
    assert_writer_opens(&s, "after the repairs");
}

/// A frame file that a user compressed or made sparse (their handle has the rights) is replaced once, by a plain
/// file, even on a compressed `%ProgramData%`; the next Repair keeps the new one.
#[test]
fn a_frame_file_a_user_compressed_or_made_sparse_is_replaced_once_and_the_result_is_plain() {
    let Some(s) = compressed_sandbox("compressed-user") else {
        return;
    };
    run(&s, &mut VecLog::default()).unwrap();
    for (name, rights, damage) in [
        (
            "compressed",
            0x3u32,
            (|h| secure::set_compressed(h, true)) as fn(_) -> std::io::Result<()>,
        ),
        ("sparse", 0x2, |h| secure::set_sparse(h, true)),
    ] {
        mark(&s);
        let h = secure::open_with(
            &s.paths.frames(),
            rights,
            secure::SHARE_ALL,
            DISP_OPEN_EXISTING,
            false,
            0,
        )
        .unwrap();
        damage(secure::raw(&h)).unwrap_or_else(|e| panic!("{name}: {e}"));
        drop(h);
        assert!(sparse_or_compressed(&s.paths.frames()), "{name}");
        let id = file_id(&s.paths.frames());
        let mut log = VecLog::default();
        run(&s, &mut log).unwrap_or_else(|e| panic!("{name}: {e}: {:?}", log.0));
        assert!(
            mentions(&log, "it is sparse or compressed; moving it aside"),
            "{name}: {:?}",
            log.0
        );
        assert_ne!(file_id(&s.paths.frames()), id, "{name}: not replaced");
        assert!(!marked(&s), "{name}: the old contents are in the new file");
        assert_plain_frame_file(&s, name);
        assert_writer_opens(&s, name);
        // the next Repair keeps it
        let id = file_id(&s.paths.frames());
        let mut log = VecLog::default();
        run(&s, &mut log).unwrap();
        assert_eq!(file_id(&s.paths.frames()), id, "{name}: replaced again");
    }
}

/// An existing data folder that is compressed (made by the version that went before this one, on a compressed drive)
/// is cleared by a Repair; a data folder that is not compressed gets no call at all and no line.
#[test]
fn a_kept_data_folder_that_is_compressed_is_cleared_and_one_that_is_not_is_left_alone() {
    let s = sandbox("kept-folder-cleared");
    run(&s, &mut VecLog::default()).unwrap();
    let mut log = VecLog::default();
    run(&s, &mut log).unwrap();
    assert!(!mentions(&log, "was compressed"), "{:?}", log.0);
    if !compress_folder(&s.paths.data) {
        eprintln!("this volume cannot compress a folder: skipped");
        return;
    }
    assert!(compressed(&s.paths.data));
    let id = file_id(&s.paths.frames());
    let mut log = VecLog::default();
    run(&s, &mut log).unwrap_or_else(|e| panic!("{e}: {:?}", log.0));
    assert!(
        !compressed(&s.paths.data),
        "kept, and still compressed: {:?}",
        log.0
    );
    assert!(
        mentions(&log, "was compressed: cleared that"),
        "{:?}",
        log.0
    );
    assert_eq!(
        file_id(&s.paths.frames()),
        id,
        "the frame file was replaced"
    );
    // and nothing made in it afterwards starts compressed
    let probe = s.paths.data.join("probe.txt");
    std::fs::write(&probe, b"x").unwrap();
    assert!(!compressed(&probe));
}

/// The two measures one by one, in a data folder that hands compression on (the answer to "which one is needed"):
///
/// * `FILE_NO_COMPRESSION` alone gives a plain new file in a compressed folder, where a file made the ordinary way is
///   compressed (so the option is what keeps the frame file plain whatever its folder is);
/// * clearing the folder's own state alone gives a plain new file made the ordinary way (so the folder is enough for
///   the frame file as long as it stays cleared, and also keeps everything else made in it plain).
///
/// Each works alone; both are harmless, and the option does not depend on the folder (a clear that fails, a folder
/// compressed again by an administrator before the file is made).
#[test]
fn each_of_the_two_compression_measures_alone_gives_a_plain_new_file() {
    let Some(s) = compressed_sandbox("compressed-measures") else {
        return;
    };
    let dir = s.paths.program_data.join("folder");
    std::fs::create_dir(&dir).unwrap();
    assert!(
        compressed(&dir),
        "a new folder in a compressed folder is compressed"
    );
    let make = |name: &str, options: u32| {
        let p = dir.join(name);
        // SAFETY: no security descriptor is passed.
        let h = unsafe {
            secure::open_with_sd_options(
                &p,
                0x8000_0000 | 0x4000_0000,
                secure::SHARE_ALL,
                DISP_CREATE_NEW,
                false,
                0,
                None,
                options,
            )
        }
        .unwrap();
        drop(h);
        p
    };
    // control: the ordinary create in a compressed folder is compressed
    assert!(compressed(&make("ordinary", 0)));
    // measure 1: the create option alone
    assert!(!compressed(&make("option", secure::CREATE_NO_COMPRESSION)));
    // measure 2: the folder cleared alone
    let h = secure::open_with(
        &dir,
        0x8000_0000 | 0x4000_0000,
        secure::SHARE_ALL,
        DISP_OPEN_EXISTING,
        true,
        0,
    )
    .unwrap();
    secure::set_compressed(secure::raw(&h), false).unwrap();
    drop(h);
    assert!(!compressed(&dir));
    assert!(!compressed(&make("folder-cleared", 0)));
    // the option in a cleared folder changes nothing
    assert!(!compressed(&make("both", secure::CREATE_NO_COMPRESSION)));
}

/// A frame file that is sparse or compressed after it was made (a volume that compresses regardless, or a user who
/// changed it that moment) is not a failure: one line of the log, the file is kept, no loop. The length stays an
/// error.
#[test]
fn a_frame_file_that_still_carries_the_attribute_is_accepted_with_one_line() {
    let s = sandbox("finish-attribute");
    run(&s, &mut VecLog::default()).unwrap();
    for (name, rights, damage) in [
        (
            "compressed",
            0x3u32,
            (|h| secure::set_compressed(h, true)) as fn(_) -> std::io::Result<()>,
        ),
        ("sparse", 0x2, |h| secure::set_sparse(h, true)),
    ] {
        let h = secure::open_with(
            &s.paths.frames(),
            rights,
            secure::SHARE_ALL,
            DISP_OPEN_EXISTING,
            false,
            0,
        )
        .unwrap();
        if damage(secure::raw(&h)).is_err() {
            eprintln!("{name}: this volume cannot do that: skipped");
            continue;
        }
        drop(h);
        let kept = open_checked(&s.paths, Object::FramesFile).unwrap();
        let mut log = VecLog::default();
        finish_frame_file(&kept, &mut log).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(log.0.len(), 1, "{name}: {:?}", log.0);
        assert!(
            log.0[0].contains(name)
                && log.0[0].contains(&vcam_protocol::FILE_SIZE.to_string())
                && log.0[0].contains("kept as it is"),
            "{name}: {:?}",
            log.0
        );
        assert!(
            sparse_or_compressed(&s.paths.frames()),
            "{name}: nothing was cleared in place"
        );
        drop(kept);
        // back to a plain file for the next case
        remove_no_follow(&s.paths.frames()).unwrap();
        run(&s, &mut VecLog::default()).unwrap();
    }
}

// ---- the data folder is held with the least access its checks need

/// The data folder opened the way an indexer, backup or anti-virus tool may: reading, sharing reading but not writing.
fn held_without_write_sharing(s: &Sandbox) -> OwnedHandle {
    secure::open_with(
        &s.paths.data,
        GENERIC_READ,
        secure::SHARE_READ,
        DISP_OPEN_EXISTING,
        true,
        0,
    )
    .unwrap()
}

/// The access the helper used to ask for on the data folder on every run (`GENERIC_READ | FILE_ADD_FILE`) is refused
/// while such a program has the folder open, which made every Install and Repair fail; the access it asks for now is
/// not. The control proves that the simulation is the real thing.
#[test]
fn a_data_folder_that_another_program_holds_without_write_sharing_does_not_fail_the_repair() {
    let s = sandbox("held-no-write-share");
    run(&s, &mut VecLog::default()).unwrap();
    let held = held_without_write_sharing(&s);
    // control: the old request is a sharing violation (ERROR_SHARING_VIOLATION, 32)
    let old = secure::open_with(
        &s.paths.data,
        GENERIC_READ | acl::SECURITY_ACCESS | FILE_ADD_FILE,
        share_for(Object::DataDir),
        DISP_OPEN_EXISTING,
        true,
        0,
    );
    assert_eq!(
        old.err().and_then(|e| e.raw_os_error()),
        Some(32),
        "the holder does not refuse a write open: the test proves nothing"
    );
    // what is asked for now: listing (the data right that makes Windows weigh the sharing mode, see the next test),
    // attributes, and permissions; no write right, which is what the holder refuses
    assert_eq!(
        access_for(Object::DataDir),
        FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | acl::SECURITY_ACCESS
    );
    assert_eq!(
        access_for(Object::DataDir) & (GENERIC_READ | GENERIC_WRITE | FILE_ADD_FILE),
        0
    );
    for _ in 0..2 {
        let mut log = VecLog::default();
        run(&s, &mut log).unwrap_or_else(|e| panic!("{e}: {:?}", log.0));
        assert!(mentions(&log, "BetterCam: kept"), "{:?}", log.0);
        assert!(mentions(&log, "the data folder: owner"), "{:?}", log.0);
    }
    // the folder is checked and locked as ever
    let snap = security_of(&s.paths.data);
    acl::check(&snap, &acl::expected(Object::DataDir, &s.p), &s.p.owner).unwrap();
    drop(held);
}

/// The held data folder cannot be renamed, deleted or swapped for one of the user's own between the open and the lock.
/// Windows weighs a handle's sharing mode only when the handle has a data right (read, write, append, execute or
/// `DELETE`); with attribute and permission rights alone the missing `FILE_SHARE_DELETE` stopped nothing, and the user
/// (who may have `DELETE` on a folder an older version made) could rename it away. The rename fails with
/// `ERROR_SHARING_VIOLATION` (32) as long as the folder is held, and works once it is let go (so the hold is what
/// stopped it).
#[test]
fn a_held_data_folder_cannot_be_renamed_while_the_helper_holds_it() {
    let s = sandbox("held-no-rename");
    run(&s, &mut VecLog::default()).unwrap();
    let away = s.paths.program_data.join("BetterCam.swapped");
    let held = open_checked(&s.paths, Object::DataDir).unwrap();
    let err = std::fs::rename(&s.paths.data, &away).expect_err("a held folder must not be renamed");
    assert_eq!(err.raw_os_error(), Some(32), "{err}");
    assert!(s.paths.data.is_dir() && !away.exists());
    // a folder is held the same way when it is made new
    drop(held);
    assert!(remove_all(&s.paths));
    let new = create_new(&s.paths, Object::DataDir, &s.p).unwrap();
    let err =
        std::fs::rename(&s.paths.data, &away).expect_err("a new held folder must not be renamed");
    assert_eq!(err.raw_os_error(), Some(32), "{err}");
    drop(new);
    std::fs::rename(&s.paths.data, &away).expect("it can be renamed once it is let go");
}

/// The same holder, and a data folder that is compressed: the clear needs the wider access, which the holder refuses.
/// That is one line of the log and not a failure (the frame file is made without compression whatever the folder
/// says); once the holder lets go, the next Repair clears the folder.
#[test]
fn a_compressed_data_folder_that_is_held_without_write_sharing_is_a_line_of_the_log_and_cleared_later(
) {
    let s = sandbox("held-compressed");
    run(&s, &mut VecLog::default()).unwrap();
    if !compress_folder(&s.paths.data) {
        eprintln!("this volume cannot compress a folder: skipped");
        return;
    }
    // no frame file: the run makes a new one in a folder that is still compressed
    secure::remove_tree(&s.paths.frames()).unwrap();
    let held = held_without_write_sharing(&s);
    let mut log = VecLog::default();
    run(&s, &mut log).unwrap_or_else(|e| panic!("{e}: {:?}", log.0));
    assert!(
        mentions(&log, "is compressed and that could not be cleared"),
        "{:?}",
        log.0
    );
    assert!(
        compressed(&s.paths.data),
        "nothing could be cleared meanwhile"
    );
    assert_plain_frame_file(&s, "held and compressed");
    drop(held);
    let mut log = VecLog::default();
    run(&s, &mut log).unwrap_or_else(|e| panic!("{e}: {:?}", log.0));
    assert!(
        mentions(&log, "was compressed: cleared that"),
        "{:?}",
        log.0
    );
    assert!(!compressed(&s.paths.data));
}

/// The second handle of the clear is the folder that is held, or nothing is cleared.
#[test]
fn the_wider_handle_for_clearing_compression_must_be_the_folder_that_is_held() {
    let s = sandbox("reopen-identity");
    run(&s, &mut VecLog::default()).unwrap();
    let held = open_checked(&s.paths, Object::DataDir).unwrap();
    assert!(reopen_for_compression(&s.paths, &held).is_ok());
    // another folder in its place: the logs folder is a folder too, with another id
    let other = open_checked(&s.paths, Object::LogsDir).unwrap();
    let err = reopen_for_compression(&s.paths, &other).unwrap_err();
    assert!(err.contains("not the folder that is held"), "{err}");
}

// ---- encrypted (EFS) data folder, `logs` or frame file

fn paths_at(root: &str) -> Paths {
    Paths::below(Path::new(root))
}

#[test]
fn a_new_data_folder_that_is_encrypted_is_refused_with_a_sentence_that_names_the_folder_around_it()
{
    let paths = paths_at(r"C:\x");
    let (data, parent) = (
        paths.data.display().to_string(),
        paths.program_data.display().to_string(),
    );
    // nothing to say for a folder that is not encrypted
    assert_eq!(encryption_refusal(&paths, 0x10, Some(0x10)), None);
    assert_eq!(encryption_refusal(&paths, 0x10 | 0x800, None), None);
    // encrypted because the folder around it is: what an administrator does about that, said as it is
    let why = encryption_refusal(
        &paths,
        0x10 | ATTRIBUTE_ENCRYPTED,
        Some(0x10 | ATTRIBUTE_ENCRYPTED),
    )
    .unwrap();
    assert!(
        why.starts_with(&format!("The folder {data} is encrypted (Windows EFS) because the folder around it, {parent}, is"))
            && why.contains("camera service cannot read")
            && why.contains(&format!(r#"cipher /d "{parent}""#))
            && why.contains("administrator")
            && why.contains("run Repair again"),
        "{why}"
    );
    // not "as the user who encrypted it": clearing the flag of a folder needs no key
    assert!(!why.contains("who encrypted"), "{why}");
    // encrypted and the folder around it is not: the folder itself is the one to turn off
    let why = encryption_refusal(&paths, ATTRIBUTE_ENCRYPTED, Some(0x10)).unwrap();
    assert!(
        why.contains(&format!(r#"cipher /d "{data}""#)) && why.contains("run Repair again"),
        "{why}"
    );
    // a sentence, not a Rust error
    assert!(!why.contains("os error") && !why.contains('{'), "{why}");
}

/// The cipher tests change the machine they run on: the first `cipher /e` of a user makes an EFS certificate and key
/// for them (in `Cert:\CurrentUser\My`), which nothing here removes. They run only when asked for.
fn efs_tests_enabled(name: &str) -> bool {
    if std::env::var("BETTER_CAM_EFS_TESTS").is_ok_and(|v| v == "1") {
        return true;
    }
    eprintln!("{name}: skipped; set BETTER_CAM_EFS_TESTS=1 to run it (`cipher /e` creates an EFS certificate for the user on first use)");
    false
}

/// Run `cipher.exe` (by full path, from the system folder) with `args`; false when EFS is not available here (not
/// NTFS, or turned off).
fn cipher(args: &[&str], path: &Path) -> bool {
    std::process::Command::new(crate::util::system_dir().join("cipher.exe"))
        .args(args)
        .arg(path)
        .output()
        .is_ok_and(|o| o.status.success())
        && std::fs::metadata(path).is_ok()
}

fn encrypted(p: &Path) -> bool {
    attributes_of(p) & ATTRIBUTE_ENCRYPTED != 0
}

/// A data folder that is encrypted (a person ran `cipher /e` on it): the status report says to run Repair, and Repair
/// moves it aside and makes a plain one, with a plain `logs`, `frames.bin` and `install.log` in it.
#[test]
fn an_encrypted_data_folder_is_replaced_by_a_plain_one() {
    if !efs_tests_enabled("an_encrypted_data_folder_is_replaced_by_a_plain_one") {
        return;
    }
    let s = sandbox("efs-data-folder");
    run(&s, &mut VecLog::default()).unwrap();
    assert_eq!(encryption_problem(&s.paths), None);
    if !cipher(&["/e"], &s.paths.data) || !encrypted(&s.paths.data) {
        eprintln!("EFS is not available here: skipped");
        return;
    }
    let report = encryption_problem(&s.paths).expect("the report says so");
    assert!(
        report.contains(&s.paths.data.display().to_string()) && report.contains("Run Repair"),
        "{report}"
    );
    let mut log = VecLog::default();
    run(&s, &mut log).unwrap_or_else(|e| panic!("{e}: {:?}", log.0));
    assert!(
        mentions(&log, "it is encrypted (Windows EFS); moving it aside"),
        "{:?}",
        log.0
    );
    for p in [
        s.paths.data.clone(),
        s.paths.logs(),
        s.paths.frames(),
        s.paths.install_log(),
    ] {
        assert!(!encrypted(&p), "{} is encrypted", p.display());
    }
    assert_plain_frame_file(&s, "efs data folder replaced");
    assert_eq!(encryption_problem(&s.paths), None);
    // permissions as ever
    acl::check(
        &security_of(&s.paths.data),
        &acl::expected(Object::DataDir, &s.p),
        &s.p.owner,
    )
    .unwrap();
}

/// `logs` that is encrypted is not kept (the helper's own `install.log` would be made in it, encrypted under the key
/// of the administrator who elevated, and the app and the service could not read or write it): it is replaced.
#[test]
fn an_encrypted_logs_folder_is_replaced_by_a_plain_one() {
    if !efs_tests_enabled("an_encrypted_logs_folder_is_replaced_by_a_plain_one") {
        return;
    }
    let s = sandbox("efs-logs");
    run(&s, &mut VecLog::default()).unwrap();
    if !cipher(&["/e"], &s.paths.logs()) || !encrypted(&s.paths.logs()) {
        eprintln!("EFS is not available here: skipped");
        return;
    }
    // the log file is made inside it, and would be encrypted with it
    let mut log = VecLog::default();
    run(&s, &mut log).unwrap_or_else(|e| panic!("{e}: {:?}", log.0));
    assert!(
        mentions(&log, "it is encrypted (Windows EFS); moving it aside"),
        "{:?}",
        log.0
    );
    assert!(!encrypted(&s.paths.logs()));
    assert!(!encrypted(&s.paths.install_log()));
    acl::check(
        &security_of(&s.paths.logs()),
        &acl::expected(Object::LogsDir, &s.p),
        &s.p.owner,
    )
    .unwrap();
}

/// `%ProgramData%` itself encrypted: the `BetterCam` folder made in it is encrypted too and cannot be made plain. That
/// is refused before anything is made below it, with a sentence that is true: after `cipher /d` on `%ProgramData%` (the
/// one thing it says to do) the next Repair goes through and everything is plain.
#[test]
fn a_data_folder_made_inside_an_encrypted_program_data_is_refused_until_that_is_turned_off() {
    if !efs_tests_enabled(
        "a_data_folder_made_inside_an_encrypted_program_data_is_refused_until_that_is_turned_off",
    ) {
        return;
    }
    let s = sandbox("efs-program-data");
    if !cipher(&["/e"], &s.paths.program_data) || !encrypted(&s.paths.program_data) {
        eprintln!("EFS is not available here: skipped");
        return;
    }
    let mut log = VecLog::default();
    let result = run(&s, &mut log);
    if !encrypted(&s.paths.data) {
        // this system made the folder in plain text (no key for the account that makes it): nothing to refuse
        eprintln!("the new folder is not encrypted here: {result:?}");
        return;
    }
    let err = result.expect_err("an encrypted data folder is refused");
    assert!(
        err.contains(&format!(
            r#"cipher /d "{}""#,
            s.paths.program_data.display()
        )) && err.contains("encrypted"),
        "{err}"
    );
    assert!(!s.paths.frames().exists(), "nothing was made below it");
    // locked all the same
    acl::check(
        &security_of(&s.paths.data),
        &acl::expected(Object::DataDir, &s.p),
        &s.p.owner,
    )
    .unwrap();
    // the report says the same
    assert_eq!(encryption_problem(&s.paths), Some(err));
    // what the sentence says to do works: the encrypted folder is replaced by a plain one
    assert!(cipher(&["/d"], &s.paths.program_data));
    // `cipher /d` without `/s` changes what is made in it from now on, not the refused folder: it is Repair that
    // replaces that one
    assert!(encrypted(&s.paths.data));
    assert!(!encrypted(&s.paths.program_data));
    let mut log = VecLog::default();
    run(&s, &mut log).unwrap_or_else(|e| panic!("{e}: {:?}", log.0));
    for p in [
        s.paths.data.clone(),
        s.paths.logs(),
        s.paths.frames(),
        s.paths.install_log(),
    ] {
        assert!(!encrypted(&p), "{} is encrypted", p.display());
    }
    assert_eq!(encryption_problem(&s.paths), None);
}

/// An encrypted frame file in a data folder that is not encrypted is replaced by a new, plain file (the file system
/// does not make a new file encrypted there), and the status report says that Repair does that.
#[test]
fn an_encrypted_frame_file_in_a_plain_folder_is_replaced_by_a_plain_one() {
    if !efs_tests_enabled("an_encrypted_frame_file_in_a_plain_folder_is_replaced_by_a_plain_one") {
        return;
    }
    let s = sandbox("efs-frames");
    run(&s, &mut VecLog::default()).unwrap();
    mark(&s);
    if !cipher(&["/e", "/a"], &s.paths.frames()) || !encrypted(&s.paths.frames()) {
        eprintln!("EFS is not available here: skipped");
        return;
    }
    let report = encryption_problem(&s.paths).expect("the report says so");
    assert!(
        report.contains("frames.bin") && report.contains("Run Repair"),
        "{report}"
    );
    let mut log = VecLog::default();
    run(&s, &mut log).unwrap_or_else(|e| panic!("{e}: {:?}", log.0));
    assert!(
        mentions(&log, "it is read-only or encrypted"),
        "{:?}",
        log.0
    );
    assert!(!marked(&s), "the file was not replaced");
    assert!(!encrypted(&s.paths.frames()));
    assert_plain_frame_file(&s, "efs replaced");
    assert_eq!(encryption_problem(&s.paths), None);
}

// ---- second review: what is said, what is kept, what another program can do

/// A data folder that is encrypted in an encrypted `%ProgramData%` is kept (a new one would be encrypted too, so
/// replacing it would move its logs aside and delete them for nothing); `prepare` refuses it after locking it. Only
/// when encryption is the one thing wrong with it, only for the data folder, and only when the folder around it is.
#[test]
fn an_encrypted_data_folder_in_an_encrypted_program_data_is_kept_not_replaced() {
    let enc = |dir| Facts {
        encrypted: true,
        ..good(dir)
    };
    assert_eq!(
        decide_below(Object::DataDir, Some(&enc(true)), true),
        Plan::Keep
    );
    // the folder around it is plain: a new one would be plain, so it is replaced as ever
    assert!(matches!(
        decide_below(Object::DataDir, Some(&enc(true)), false),
        Plan::Replace(w) if w.contains("encrypted")
    ));
    // anything else wrong with it still means replacing it, whatever the folder around it is
    for (what, f) in [
        (
            "link",
            Facts {
                reparse_point: true,
                ..enc(true)
            },
        ),
        (
            "not a folder",
            Facts {
                is_directory: false,
                ..enc(true)
            },
        ),
        (
            "place",
            Facts {
                in_place: false,
                ..enc(true)
            },
        ),
        (
            "owner",
            Facts {
                owner_trusted: false,
                ..enc(true)
            },
        ),
    ] {
        assert!(
            matches!(
                decide_below(Object::DataDir, Some(&f), true),
                Plan::Replace(_)
            ),
            "{what}"
        );
    }
    // `logs` is not the data folder, and the rest is as `decide` says
    assert!(matches!(
        decide_below(Object::LogsDir, Some(&enc(true)), true),
        Plan::Replace(_)
    ));
    assert_eq!(
        decide_below(Object::DataDir, Some(&good(true)), true),
        Plan::Keep
    );
    assert_eq!(decide_below(Object::DataDir, None, true), Plan::Create);
}

fn prepare_seeing(s: &Sandbox, folder: u32, parent: Option<u32>) -> Res<()> {
    prepare_reading(
        &s.paths,
        &s.p,
        &mut || false,
        &mut VecLog::default(),
        &mut NoLedger,
        &|| parent,
        &|_| Ok(folder),
    )
}

/// `prepare` refuses a data folder that is still encrypted once it is locked, and before anything is made in it, with
/// the sentence for the folder around it; a plain one goes through. (No cipher: the attributes are given.)
#[test]
fn prepare_refuses_a_data_folder_that_is_encrypted_after_it_is_locked_and_before_anything_is_made_in_it(
) {
    let s = sandbox("refuse-injected");
    let (data, parent) = (
        s.paths.data.display().to_string(),
        s.paths.program_data.display().to_string(),
    );
    let around = prepare_seeing(
        &s,
        0x10 | ATTRIBUTE_ENCRYPTED,
        Some(0x10 | ATTRIBUTE_ENCRYPTED),
    )
    .expect_err("refused");
    assert_eq!(
        Some(around.clone()),
        encryption_refusal(&s.paths, ATTRIBUTE_ENCRYPTED, Some(ATTRIBUTE_ENCRYPTED))
    );
    assert!(
        around.contains(&format!("because the folder around it, {parent}, is")),
        "{around}"
    );
    assert!(
        !s.paths.logs().exists() && !s.paths.frames().exists() && !s.paths.install_log().exists(),
        "nothing was made in it"
    );
    // locked all the same
    acl::check(
        &security_of(&s.paths.data),
        &acl::expected(Object::DataDir, &s.p),
        &s.p.owner,
    )
    .unwrap();
    // the folder around it is plain: the other sentence, which says "after this run"
    let alone = prepare_seeing(&s, ATTRIBUTE_ENCRYPTED, Some(0x10)).expect_err("refused");
    assert!(
        alone.contains(&format!(
            "The folder {data} is encrypted (Windows EFS) after this run"
        )) && alone.contains(&format!(r#"cipher /d "{data}""#))
            && !alone.contains("just made it new"),
        "{alone}"
    );
    assert!(!s.paths.frames().exists());
    // not encrypted: it goes through, with everything made in it
    prepare_seeing(&s, 0x10, Some(ATTRIBUTE_ENCRYPTED)).unwrap();
    assert!(s.paths.frames().exists() && s.paths.install_log().exists());
}

/// The status sentence, from the attributes of the data folder, `%ProgramData%` and the frame file.
#[test]
fn the_status_sentence_is_for_the_folder_around_it_or_for_repair() {
    let paths = paths_at(r"C:\x");
    let (data, parent) = (
        paths.data.display().to_string(),
        paths.program_data.display().to_string(),
    );
    let (plain, enc) = (Some(0x10), Some(0x10 | ATTRIBUTE_ENCRYPTED));
    // nothing encrypted, or nothing there
    assert_eq!(encryption_problem_from(&paths, plain, plain, plain), None);
    assert_eq!(encryption_problem_from(&paths, None, enc, None), None);
    assert_eq!(encryption_problem_from(&paths, plain, enc, plain), None);
    // encrypted and the folder around it is too: Repair cannot help, the administrator's step is the sentence
    let around = encryption_problem_from(&paths, enc, enc, plain).unwrap();
    assert_eq!(
        Some(around.clone()),
        encryption_refusal(&paths, ATTRIBUTE_ENCRYPTED, enc)
    );
    assert!(
        around.contains(&format!(r#"cipher /d "{parent}""#)),
        "{around}"
    );
    // encrypted alone: Repair puts it right, and the sentence says that only what is made in it is affected
    for around_it in [plain, None] {
        let alone = encryption_problem_from(&paths, enc, around_it, plain).unwrap();
        assert!(
            alone.starts_with(&format!("The folder {data} is encrypted (Windows EFS): new files made in it are encrypted too"))
                && alone.contains("Run Repair")
                && !alone.contains("would show nothing"),
            "{alone}"
        );
    }
    // a frame file that is encrypted in a folder that is not
    let frames = encryption_problem_from(&paths, plain, plain, enc).unwrap();
    assert!(
        frames.contains("frames.bin") && frames.contains("Run Repair"),
        "{frames}"
    );
    // the folder's own sentence comes first
    assert!(encryption_problem_from(&paths, enc, plain, enc)
        .unwrap()
        .contains("The folder"));
}

/// What the app says after an install that failed with nothing in its log: only a data folder that is encrypted, and
/// never "Run Repair" (a folder that is still encrypted after a failed install is one Repair could not fix).
#[test]
fn a_failed_install_says_the_refusal_when_the_data_folder_is_encrypted_and_never_run_repair() {
    let paths = paths_at(r"C:\x");
    let (plain, enc) = (Some(0x10), Some(0x10 | ATTRIBUTE_ENCRYPTED));
    // not encrypted, not there: nothing to say, whatever the folder around it is
    for (data, parent) in [(plain, plain), (plain, enc), (None, enc), (None, None)] {
        assert_eq!(
            encryption_after_failed_install_from(&paths, data, parent),
            None,
            "{data:?} {parent:?}"
        );
    }
    // encrypted: the refusal, for both ways it can be
    for parent in [plain, enc, None] {
        let said = encryption_after_failed_install_from(&paths, enc, parent).unwrap();
        assert_eq!(
            Some(said.clone()),
            encryption_refusal(&paths, ATTRIBUTE_ENCRYPTED, parent)
        );
        assert!(
            said.contains("is encrypted (Windows EFS)") && !said.contains("Run Repair"),
            "{said}"
        );
    }
    // the status report, for a folder nobody has tried to replace, does say so
    assert!(encryption_problem_from(&paths, enc, plain, plain)
        .unwrap()
        .contains("Run Repair"));
}

/// A standard user can list the data folder, so a program of theirs can hold it with share mode 0: the open of the
/// helper then fails with `ERROR_SHARING_VIOLATION`, and the app says whose doing that is.
#[test]
fn a_data_folder_another_program_holds_without_sharing_is_said_so() {
    let s = sandbox("share-none");
    run(&s, &mut VecLog::default()).unwrap();
    let held = secure::open_with(
        &s.paths.data,
        FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES,
        FILE_SHARE_MODE(0),
        DISP_OPEN_EXISTING,
        true,
        0,
    )
    .unwrap();
    let said = format!(
        "Another program has {} open without sharing it. Close it or restart Windows, then run Repair again.",
        s.paths.data.display()
    );
    assert_eq!(open_checked(&s.paths, Object::DataDir).unwrap_err(), said);
    // the whole run says the same, and a raw error number is nowhere
    assert_eq!(run(&s, &mut VecLog::default()).unwrap_err(), said);
    assert!(is_sharing_violation(&std::io::Error::from_raw_os_error(32)));
    assert!(!is_sharing_violation(&std::io::Error::from_raw_os_error(5)));
    // when the other program lets go, it goes through
    drop(held);
    run(&s, &mut VecLog::default()).unwrap();
}

#[test]
fn a_logs_folder_that_stays_encrypted_says_so_and_what_to_do() {
    let said = still_encrypted_message(Path::new(r"C:\x\BetterCam\logs"));
    assert_eq!(
        said,
        r"C:\x\BetterCam\logs is still encrypted because it is in use; close the camera apps and run Repair again"
    );
}

/// `logs` that is encrypted and in use cannot be moved: it is emptied in place, stays encrypted, and the log says so.
#[test]
fn an_encrypted_logs_folder_that_is_in_use_is_emptied_in_place_and_the_log_says_it_is_still_encrypted(
) {
    if !efs_tests_enabled(
        "an_encrypted_logs_folder_that_is_in_use_is_emptied_in_place_and_the_log_says_it_is_still_encrypted",
    ) {
        return;
    }
    let s = sandbox("efs-logs-held");
    run(&s, &mut VecLog::default()).unwrap();
    if !cipher(&["/e"], &s.paths.logs()) || !encrypted(&s.paths.logs()) {
        eprintln!("EFS is not available here: skipped");
        return;
    }
    let open_log = s.paths.logs().join("vcam-1.log");
    std::fs::write(&open_log, b"x").unwrap();
    let held = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&open_log)
        .unwrap();
    let mut log = VecLog::default();
    run(&s, &mut log).unwrap_or_else(|e| panic!("{e}: {:?}", log.0));
    assert!(mentions(&log, "in place"), "{:?}", log.0);
    assert!(
        mentions(
            &log,
            &format!(
                "{} is still encrypted because it is in use; close the camera apps and run Repair again",
                s.paths.logs().display()
            )
        ),
        "{:?}",
        log.0
    );
    drop(held);
}

/// An encrypted data folder in an encrypted `%ProgramData%`: refused, and what is in it is not moved aside or deleted.
#[test]
fn an_encrypted_data_folder_in_an_encrypted_program_data_is_refused_with_its_logs_in_place() {
    if !efs_tests_enabled(
        "an_encrypted_data_folder_in_an_encrypted_program_data_is_refused_with_its_logs_in_place",
    ) {
        return;
    }
    let s = sandbox("efs-both");
    run(&s, &mut VecLog::default()).unwrap();
    let old_log = s.paths.logs().join("vcam-1.log");
    std::fs::write(&old_log, b"keep me").unwrap();
    if !cipher(&["/e"], &s.paths.program_data)
        || !cipher(&["/e"], &s.paths.data)
        || !encrypted(&s.paths.program_data)
        || !encrypted(&s.paths.data)
    {
        eprintln!("EFS is not available here: skipped");
        return;
    }
    let mut log = VecLog::default();
    let err = run(&s, &mut log).expect_err("refused");
    assert!(
        err.contains(&format!(
            r#"cipher /d "{}""#,
            s.paths.program_data.display()
        )),
        "{err}"
    );
    assert!(
        crate::testkit::asides_in(&s.paths).is_empty(),
        "the folder was moved aside: {:?}",
        log.0
    );
    assert_eq!(std::fs::read(&old_log).unwrap(), b"keep me");
    assert!(mentions(&log, ": kept"), "{:?}", log.0);
    assert!(!mentions(&log, "moving it aside"), "{:?}", log.0);
}
