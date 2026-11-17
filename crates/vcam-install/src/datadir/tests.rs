use super::*;
use crate::acl::Snapshot;
use crate::log::VecLog;
use crate::testkit::{junction, lock_on, sandbox, security_of, within, Sandbox, BOUND};

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

/// The two states a `FILE_WRITE_DATA` handle can set are put right in place (the file is kept); an extra data
/// stream is a reason to replace the file.
#[test]
fn sparse_and_compressed_files_are_kept_and_a_file_with_a_stream_is_replaced() {
    let sparse = Facts {
        sparse_or_compressed: true,
        ..good(false)
    };
    assert_eq!(decide(Object::FramesFile, Some(&sparse)), Plan::Keep);
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
/// it (`acl::tests::a_write_data_handle_can_still_...`). Repair clears both in place, through the held handle: the
/// same file, with its contents, not a copy.
#[test]
fn a_sparse_or_compressed_frame_file_is_cleared_in_place_and_stays_the_same_file() {
    let s = sandbox("sparse-in-place");
    run(&s, &mut VecLog::default()).unwrap();
    mark(&s);
    let id = file_id(&s.paths.frames());
    let frames = s.paths.frames();
    // sparse, through a handle with only FILE_WRITE_DATA
    {
        let h = secure::open_with(
            &frames,
            0x2,
            secure::SHARE_ALL,
            DISP_OPEN_EXISTING,
            false,
            0,
        )
        .unwrap();
        secure::set_sparse(secure::raw(&h), true).unwrap();
    }
    assert_ne!(attributes_of(&frames) & secure::ATTRIBUTE_SPARSE, 0);
    let mut log = VecLog::default();
    run(&s, &mut log).unwrap();
    assert!(mentions(&log, "clearing that in place"), "{:?}", log.0);
    assert_eq!(attributes_of(&frames) & secure::ATTRIBUTE_SPARSE, 0);
    assert_eq!(file_id(&frames), id, "the file was replaced, not cleared");
    assert!(marked(&s), "the contents are gone");
    assert_eq!(
        std::fs::metadata(&frames).unwrap().len(),
        vcam_protocol::FILE_SIZE
    );
    assert!(
        log.0.iter().any(|l| l.ends_with("frames.bin: kept")),
        "{:?}",
        log.0
    );

    // compressed (and sparse too), through a read-write handle
    {
        let h = secure::open_with(
            &frames,
            0x3,
            secure::SHARE_ALL,
            DISP_OPEN_EXISTING,
            false,
            0,
        )
        .unwrap();
        secure::set_sparse(secure::raw(&h), true).unwrap();
        if secure::set_compressed(secure::raw(&h), true).is_err() {
            eprintln!("this volume cannot compress a file: that part is skipped");
        }
    }
    let mut log = VecLog::default();
    run(&s, &mut log).unwrap();
    assert_eq!(
        attributes_of(&frames) & (secure::ATTRIBUTE_SPARSE | secure::ATTRIBUTE_COMPRESSED),
        0
    );
    assert_eq!(file_id(&frames), id);
    assert!(marked(&s));
    // and a file that is fine is not touched or mentioned
    let mut log = VecLog::default();
    run(&s, &mut log).unwrap();
    assert!(!mentions(&log, "clearing that in place"), "{:?}", log.0);
    assert_eq!(file_id(&frames), id);
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
    assert_eq!(
        sweep_data_asides(&s.paths, &s.p.trusted_owners, &mut log),
        0
    );
    assert!(!pd.join("BetterCam.old-1-0").exists());
    assert!(!pd.join("BetterCam.old-2-0").exists(), "the junction goes");
    untouched(&dir, &frames, &logs);
    // not ours: other names stay, and so does the live data folder
    assert!(pd.join("Other.old-1-0").exists() && pd.join("BetterCamera").exists());
    assert!(pd.join("Better.old-3-0").exists());
    assert!(s.paths.frames().is_file());
    assert!(mentions(&log, "BetterCam.old-1-0"), "{:?}", log.0);
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
    let (left, lines) = within("sweep", BOUND, move || {
        let mut log = VecLog::default();
        (sweep_data_asides(&paths, &trusted, &mut log), log.0)
    });
    assert_eq!(
        left, 0,
        "somebody else's entry is not the helper's to report: {lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("BetterCam.old-test left alone")),
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
    let mut log = VecLog::default();
    assert_eq!(
        sweep_data_asides(&s.paths, &[acl::SID_SYSTEM.to_string()], &mut log),
        0
    );
    assert!(is_link(&pd.join("BetterCam.old-link")));
    untouched(&dir, &frames, &logs);
    // trusted: now it goes (as a junction), and so does the entry
    let mut log = VecLog::default();
    assert_eq!(
        sweep_data_asides(&s.paths, &s.p.trusted_owners, &mut log),
        0
    );
    assert!(!entry.exists() && !pd.join("BetterCam.old-link").exists());
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
    let (left, lines) = within("sweep", BOUND, move || {
        let mut log = VecLog::default();
        (sweep_data_asides(&paths, &trusted, &mut log), log.0)
    });
    assert_eq!(left, 1, "{lines:?}");
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
    assert_eq!(
        sweep_data_asides(&s.paths, &s.p.trusted_owners, &mut log),
        0
    );
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
