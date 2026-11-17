use super::*;
use crate::acl::Snapshot;
use crate::log::VecLog;
use crate::testkit::{junction, sandbox, security_of, Sandbox};

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
    }
}

#[test]
fn a_missing_object_is_created_and_a_good_one_is_kept() {
    assert_eq!(decide(Object::DataDir, None), Plan::Create);
    assert_eq!(decide(Object::LogsDir, None), Plan::Create);
    assert_eq!(decide(Object::FramesFile, None), Plan::Create);
    assert_eq!(decide(Object::DataDir, Some(&good(true))), Plan::Keep);
    assert_eq!(decide(Object::LogsDir, Some(&good(true))), Plan::Keep);
    assert_eq!(decide(Object::FramesFile, Some(&good(false))), Plan::Keep);
}

#[test]
fn links_wrong_kinds_hard_links_and_foreign_owners_are_replaced() {
    let replace = |obj, f: Facts| matches!(decide(obj, Some(&f)), Plan::Replace(_));
    for (obj, dir) in [
        (Object::DataDir, true),
        (Object::LogsDir, true),
        (Object::FramesFile, false),
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
        3,
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
    let mut log = crate::log::FileLog::with_paths(s.paths.clone());
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
