use super::*;
use crate::log::VecLog;
use crate::machine::{AsideStore, Stored};
use crate::testkit::{fake_dll, lock_on, sandbox, within, Sandbox, BOUND};

/// A machine that keeps its registry and camera in memory and fails at a named step.
#[derive(Default)]
struct Fake {
    reg: Option<Registration>,
    camera: bool,
    fail: Option<&'static str>,
    /// `stop_frame_server` result.
    service_stops: bool,
    /// Scheduling a delete at reboot fails.
    reboot_delete_fails: bool,
    delayed: Vec<PathBuf>,
    /// What `delete_registration` reports as not deleted.
    registry_leftovers: Vec<String>,
    stop_calls: u32,
    calls: Vec<String>,
    /// The record of moved-aside data folders as stored (the helper's HKLM value); `None`: there is none.
    stored: Option<Stored>,
    /// Every write of the record, in order (an empty list is the removal of the value).
    record_writes: Vec<Vec<String>>,
    /// Writing the record fails.
    record_fails: bool,
    /// A path whose existence is noted at every write of the record (the data folder: "before the rename").
    watch: Option<PathBuf>,
    /// For every write of the record: whether `watch` existed at that moment.
    exists_at_write: Vec<bool>,
}

impl AsideStore for Fake {
    fn aside_record(&mut self) -> Stored {
        self.stored.clone().unwrap_or(Stored::Missing)
    }
    fn write_aside_record(&mut self, strings: &[String]) -> Res<()> {
        if self.record_fails {
            return Err("RegSetKeyValueW failed (injected)".into());
        }
        if let Some(w) = &self.watch {
            self.exists_at_write.push(w.exists());
        }
        self.record_writes.push(strings.to_vec());
        self.stored = (!strings.is_empty()).then(|| Stored::Strings(strings.to_vec()));
        Ok(())
    }
}

impl Fake {
    /// The entries on the record.
    fn entries(&mut self) -> Vec<aside::AsideEntry> {
        match aside::load(self) {
            aside::Record::Entries { entries, .. } => entries,
            aside::Record::Unreadable(why) => panic!("the record is unreadable: {why}"),
        }
    }

    /// The names on the record.
    fn names(&mut self) -> Vec<String> {
        self.entries().into_iter().map(|e| e.name).collect()
    }

    /// Put the folder `pd\name` (which exists) on the record, as the helper does when it moves it aside.
    fn put_on_record(&mut self, pd: &Path, name: &str) {
        let e = aside::AsideEntry {
            name: name.to_string(),
            identity: crate::testkit::identity_of(&pd.join(name)),
        };
        aside::add(self, &e).unwrap();
        self.record_writes.clear();
    }
}

impl Fake {
    fn step(&mut self, name: &'static str) -> Res<()> {
        self.calls.push(name.to_string());
        if self.fail == Some(name) {
            // one shot: the rollback calls the same step again and it works then
            if name != "remove_camera" {
                self.fail = None;
            }
            Err(format!("{name} failed (injected)"))
        } else {
            Ok(())
        }
    }
}

impl Machine for Fake {
    fn start(&mut self) -> Res<()> {
        self.step("start")
    }
    fn confirm_user(&mut self, _: &str) -> Res<()> {
        self.step("confirm_user")
    }
    fn read_registration(&mut self) -> Option<Registration> {
        self.reg.clone()
    }
    fn write_registration(&mut self, r: &Registration) -> Res<()> {
        self.step("write_registration")?;
        self.reg = Some(r.clone());
        Ok(())
    }
    fn delete_registration(&mut self) -> Vec<String> {
        self.calls.push("delete_registration".into());
        if self.registry_leftovers.is_empty() {
            self.reg = None;
        }
        self.registry_leftovers.clone()
    }
    fn create_camera(&mut self, _: bool) -> Res<()> {
        self.step("create_camera")?;
        self.step("start_camera")?;
        self.camera = true;
        Ok(())
    }
    fn remove_camera(&mut self) -> Res<()> {
        self.step("remove_camera")?;
        // like Windows: removing a camera that is not there is an error
        if !self.camera {
            return Err("The request is invalid in the current state (0xC00D36B2)".into());
        }
        self.camera = false;
        Ok(())
    }
    fn camera_listed(&mut self) -> bool {
        self.camera
    }
    fn stop_frame_server(&mut self) -> bool {
        self.stop_calls += 1;
        self.service_stops
    }
    fn delete_at_reboot(&mut self, p: &Path) -> Res<()> {
        if self.reboot_delete_fails {
            Err("MoveFileEx failed (injected)".into())
        } else {
            self.delayed.push(p.to_path_buf());
            Ok(())
        }
    }
}

fn opts() -> InstallOptions {
    InstallOptions::default()
}

fn do_install(s: &Sandbox, dll: &Path, m: &mut Fake, log: &mut VecLog) -> Res<InstallOutcome> {
    install_with(&opts(), dll, &s.paths, Some(s.p.clone()), m, log)
}

fn dll_files(s: &Sandbox) -> Vec<String> {
    std::fs::read_dir(&s.paths.install)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// The invariant of the install: a registered class never names a missing file.
fn registration_is_sound(m: &Fake) {
    if let Some(r) = &m.reg {
        assert!(
            Path::new(&r.dll).is_file(),
            "registered DLL is missing: {}",
            r.dll
        );
    }
}

// ---- unchanged contracts

#[test]
fn only_a_big_mz_file_is_a_dll() {
    let mut real = vec![0u8; MIN_DLL_BYTES];
    real[0] = b'M';
    real[1] = b'Z';
    assert!(looks_like_dll(&real));
    assert!(!looks_like_dll(&real[..MIN_DLL_BYTES - 1]));
    assert!(!looks_like_dll(b"placeholder"));
    assert!(!looks_like_dll(&vec![0u8; MIN_DLL_BYTES]));
}

#[test]
fn our_dll_names_are_recognised() {
    assert!(is_ours(Path::new(r"C:\x\better_cam_vcam_1.abcd1234.dll")));
    assert!(!is_ours(Path::new(r"C:\x\better_cam_vcam.dll")));
    assert!(!is_ours(Path::new(r"C:\x\other_1.abcd1234.dll")));
    assert!(!is_ours(Path::new(r"C:\x\better_cam_vcam_1.abcd1234.txt")));
}

#[test]
fn a_bad_sid_stops_the_install_before_anything_is_touched() {
    let s = sandbox("install-badsid");
    let dll = fake_dll(&s.root.join("src"), 1);
    for bad in [
        "S-1-5-21-1)(A;;FA;;;WD",
        "S-1-1-0",
        "S-1-5-32-545",
        "S-1-5-21-1-2-3-4294967296",
    ] {
        let o = InstallOptions {
            user_sid: Some(bad.into()),
            dshow_class: false,
        };
        let mut m = Fake::default();
        let mut log = VecLog::default();
        let err = install_with(&o, &dll, &s.paths, None, &mut m, &mut log).unwrap_err();
        assert!(err.contains("not a user SID"), "{bad}: {err}");
        assert!(log.0.is_empty());
        assert!(
            m.calls.is_empty(),
            "{bad}: the machine was touched: {:?}",
            m.calls
        );
    }
    assert!(!s.paths.data.exists() && !s.paths.install.exists());
}

#[test]
fn a_sid_that_is_not_a_user_account_is_refused_before_anything_is_touched() {
    // looks like a user SID, but Windows does not say "user"
    struct Group;
    impl Machine for Group {
        fn start(&mut self) -> Res<()> {
            panic!("must not start")
        }
        fn confirm_user(&mut self, sid: &str) -> Res<()> {
            Err(format!("'{sid}' is not a user account"))
        }
        fn read_registration(&mut self) -> Option<Registration> {
            None
        }
        fn write_registration(&mut self, _: &Registration) -> Res<()> {
            panic!()
        }
        fn delete_registration(&mut self) -> Vec<String> {
            panic!()
        }
        fn create_camera(&mut self, _: bool) -> Res<()> {
            panic!()
        }
        fn remove_camera(&mut self) -> Res<()> {
            panic!()
        }
        fn camera_listed(&mut self) -> bool {
            panic!()
        }
        fn stop_frame_server(&mut self) -> bool {
            panic!()
        }
        fn delete_at_reboot(&mut self, _: &Path) -> Res<()> {
            panic!()
        }
    }
    impl AsideStore for Group {
        fn aside_record(&mut self) -> Stored {
            panic!()
        }
        fn write_aside_record(&mut self, _: &[String]) -> Res<()> {
            panic!()
        }
    }
    let s = sandbox("install-group");
    let dll = fake_dll(&s.root.join("src"), 1);
    let o = InstallOptions {
        user_sid: Some("S-1-5-21-1-2-3-513".into()),
        dshow_class: false,
    };
    let err =
        install_with(&o, &dll, &s.paths, None, &mut Group, &mut VecLog::default()).unwrap_err();
    assert!(err.contains("not a user account"), "{err}");
    assert!(!s.paths.data.exists() && !s.paths.install.exists());
}

#[test]
fn a_missing_or_placeholder_dll_is_refused_before_anything_is_touched() {
    let s = sandbox("install-placeholder");
    let mut log = VecLog::default();
    let mut m = Fake::default();
    assert!(do_install(&s, Path::new(r"C:\does\not\exist.dll"), &mut m, &mut log).is_err());
    let fake = s.root.join("better_cam_vcam.dll");
    std::fs::write(&fake, b"placeholder").unwrap();
    let err = do_install(&s, &fake, &mut m, &mut log).unwrap_err();
    assert!(err.contains("not the virtual camera DLL"), "{err}");
    assert!(!file_looks_like_dll(&fake));
    assert!(log.0.is_empty());
    assert!(m.calls.is_empty());
    assert!(!s.paths.data.exists());
}

// ---- a good install, and Repair

#[test]
fn a_clean_install_prepares_everything_and_a_second_one_repairs() {
    let s = sandbox("install-ok");
    let dll = fake_dll(&s.root.join("src"), 1);
    let mut m = Fake::default();
    let mut log = VecLog::default();
    assert_eq!(
        do_install(&s, &dll, &mut m, &mut log).unwrap(),
        InstallOutcome::Done
    );
    assert!(m.camera);
    registration_is_sound(&m);
    assert!(s.paths.frames().is_file() && s.paths.logs().is_dir());
    assert_eq!(dll_files(&s).len(), 1);
    // Repair on a healthy install: the same, with the copy kept
    let mut log = VecLog::default();
    assert_eq!(
        do_install(&s, &dll, &mut m, &mut log).unwrap(),
        InstallOutcome::Done
    );
    assert!(log.0.iter().any(|l| l.contains("dll already present")));
    assert_eq!(dll_files(&s).len(), 1);
}

#[test]
fn an_update_retires_the_older_dll() {
    let s = sandbox("install-update");
    let mut m = Fake::default();
    let old = fake_dll(&s.root.join("src-old"), 1);
    do_install(&s, &old, &mut m, &mut VecLog::default()).unwrap();
    let new = fake_dll(&s.root.join("src-new"), 2);
    assert_eq!(
        do_install(&s, &new, &mut m, &mut VecLog::default()).unwrap(),
        InstallOutcome::Done
    );
    assert_eq!(dll_files(&s).len(), 1, "{:?}", dll_files(&s));
    registration_is_sound(&m);
    assert_eq!(m.stop_calls, 0, "no locked file: the service stays up");
}

// ---- the service cannot be stopped

#[test]
fn a_locked_old_dll_and_a_service_that_will_not_stop_is_a_pending_result_not_a_plain_ok() {
    let s = sandbox("install-pending");
    let mut m = Fake::default();
    let old = fake_dll(&s.root.join("src-old"), 1);
    do_install(&s, &old, &mut m, &mut VecLog::default()).unwrap();
    let old_dll = Path::new(&m.reg.as_ref().unwrap().dll).to_path_buf();
    // Frame Server has the old DLL loaded: a file opened without delete sharing cannot be deleted
    use std::os::windows::fs::OpenOptionsExt;
    let _held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(&old_dll)
        .unwrap();
    m.service_stops = false;
    let new = fake_dll(&s.root.join("src-new"), 2);
    let mut log = VecLog::default();
    let outcome = do_install(&s, &new, &mut m, &mut log).unwrap();
    assert_eq!(outcome, InstallOutcome::PendingServiceRestart);
    assert!(m.stop_calls >= 1);
    assert_eq!(
        m.delayed,
        vec![old_dll.clone()],
        "the old DLL waits for a restart"
    );
    assert!(
        log.0.iter().any(|l| l.contains("takes effect after")),
        "{:?}",
        log.0
    );
    // the new build is registered
    registration_is_sound(&m);
    assert_ne!(Path::new(&m.reg.as_ref().unwrap().dll), old_dll);
}

/// `Program Files` can be read by a standard user, who can hold an opportunistic lock on an old DLL there: deleting
/// it must not wait (the update used to hang in `remove_file` with the machine-wide lock taken). The file counts as
/// in use, like one Frame Server has loaded, and is deleted at the next restart.
#[test]
fn an_old_dll_that_a_program_holds_a_lock_on_does_not_hold_up_an_update_or_a_removal() {
    let s = sandbox("install-dll-locked");
    let mut m = Fake::default();
    let old = fake_dll(&s.root.join("src-old"), 1);
    do_install(&s, &old, &mut m, &mut VecLog::default()).unwrap();
    let old_dll = Path::new(&m.reg.as_ref().unwrap().dll).to_path_buf();
    let Some(lock) = lock_on(&old_dll) else {
        return;
    };
    let new = fake_dll(&s.root.join("src-new"), 2);
    let (paths, p) = (s.paths.clone(), s.p.clone());
    let (r, m) = within("update", BOUND, move || {
        let mut m = m;
        let r = install_with(
            &opts(),
            &new,
            &paths,
            Some(p),
            &mut m,
            &mut VecLog::default(),
        );
        (r, m)
    });
    assert_eq!(r.unwrap(), InstallOutcome::PendingServiceRestart);
    assert_eq!(m.delayed, vec![old_dll.clone()]);
    assert!(old_dll.exists());
    // and a removal
    let paths = s.paths.clone();
    let (r, m) = within("uninstall", BOUND, move || {
        let mut m = m;
        let r = uninstall_with(&paths, &mut m, &mut VecLog::default());
        (r, m)
    });
    assert_eq!(r.unwrap(), UninstallOutcome::PendingReboot);
    assert!(m.delayed.contains(&old_dll));
    drop(lock);
}

#[test]
fn a_locked_old_dll_that_goes_once_the_service_stopped_is_done() {
    let s = sandbox("install-stopped");
    let mut m = Fake::default();
    let old = fake_dll(&s.root.join("src-old"), 1);
    do_install(&s, &old, &mut m, &mut VecLog::default()).unwrap();
    let old_dll = Path::new(&m.reg.as_ref().unwrap().dll).to_path_buf();
    use std::os::windows::fs::OpenOptionsExt;
    let held = std::cell::RefCell::new(Some(
        std::fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(&old_dll)
            .unwrap(),
    ));
    // stopping the service closes its handle
    struct Stopper<'a> {
        inner: Fake,
        held: &'a std::cell::RefCell<Option<std::fs::File>>,
    }
    impl Machine for Stopper<'_> {
        fn start(&mut self) -> Res<()> {
            self.inner.start()
        }
        fn confirm_user(&mut self, s: &str) -> Res<()> {
            self.inner.confirm_user(s)
        }
        fn read_registration(&mut self) -> Option<Registration> {
            self.inner.read_registration()
        }
        fn write_registration(&mut self, r: &Registration) -> Res<()> {
            self.inner.write_registration(r)
        }
        fn delete_registration(&mut self) -> Vec<String> {
            self.inner.delete_registration()
        }
        fn create_camera(&mut self, d: bool) -> Res<()> {
            self.inner.create_camera(d)
        }
        fn remove_camera(&mut self) -> Res<()> {
            self.inner.remove_camera()
        }
        fn camera_listed(&mut self) -> bool {
            self.inner.camera_listed()
        }
        fn stop_frame_server(&mut self) -> bool {
            self.held.borrow_mut().take();
            self.inner.stop_calls += 1;
            true
        }
        fn delete_at_reboot(&mut self, p: &Path) -> Res<()> {
            self.inner.delete_at_reboot(p)
        }
    }
    impl AsideStore for Stopper<'_> {
        fn aside_record(&mut self) -> Stored {
            self.inner.aside_record()
        }
        fn write_aside_record(&mut self, s: &[String]) -> Res<()> {
            self.inner.write_aside_record(s)
        }
    }
    let mut st = Stopper {
        inner: std::mem::take(&mut m),
        held: &held,
    };
    let new = fake_dll(&s.root.join("src-new"), 2);
    let outcome = install_with(
        &opts(),
        &new,
        &s.paths,
        Some(s.p.clone()),
        &mut st,
        &mut VecLog::default(),
    )
    .unwrap();
    assert_eq!(outcome, InstallOutcome::Done);
    assert!(!old_dll.exists());
    assert!(st.inner.delayed.is_empty());
}

// ---- rollback at each failing step

/// Fail `step` on a clean machine: nothing may be left registered, no copy of the DLL may stay.
fn clean_machine_fails_at(step: &'static str) {
    let s = sandbox(&format!("rollback-clean-{step}"));
    let dll = fake_dll(&s.root.join("src"), 1);
    let mut m = Fake {
        fail: Some(step),
        ..Fake::default()
    };
    let mut log = VecLog::default();
    let err = do_install(&s, &dll, &mut m, &mut log).unwrap_err();
    assert!(err.contains(step), "{step}: {err}");
    assert!(m.reg.is_none(), "{step}: a class is still registered");
    assert!(!m.camera, "{step}: a camera exists");
    assert!(
        dll_files(&s).is_empty(),
        "{step}: DLL copies left: {:?}",
        dll_files(&s)
    );
    // a later Repair recovers
    m.fail = None;
    assert_eq!(
        do_install(&s, &dll, &mut m, &mut VecLog::default()).unwrap(),
        InstallOutcome::Done
    );
    assert!(m.camera);
    registration_is_sound(&m);
}

#[test]
fn a_failed_registration_on_a_clean_machine_leaves_nothing_registered() {
    clean_machine_fails_at("write_registration");
}

#[test]
fn a_failed_camera_on_a_clean_machine_removes_the_registration_and_the_copy() {
    clean_machine_fails_at("create_camera");
}

#[test]
fn a_camera_that_will_not_start_on_a_clean_machine_removes_the_registration_and_the_copy() {
    clean_machine_fails_at("start_camera");
}

/// Fail `step` on a machine that has an older build: the older registration is back, its DLL is there.
fn update_fails_at(step: &'static str) {
    let s = sandbox(&format!("rollback-update-{step}"));
    let mut m = Fake::default();
    let old = fake_dll(&s.root.join("src-old"), 1);
    do_install(&s, &old, &mut m, &mut VecLog::default()).unwrap();
    let before = m.reg.clone().unwrap();
    m.fail = Some(step);
    let new = fake_dll(&s.root.join("src-new"), 2);
    let err = do_install(&s, &new, &mut m, &mut VecLog::default()).unwrap_err();
    assert!(err.contains(step), "{err}");
    assert_eq!(
        m.reg.as_ref(),
        Some(&before),
        "{step}: the earlier registration is back"
    );
    assert!(m.camera, "the earlier camera still exists");
    registration_is_sound(&m);
    assert_eq!(
        dll_files(&s).len(),
        1,
        "{step}: the new copy was deleted: {:?}",
        dll_files(&s)
    );
    // Repair recovers
    m.fail = None;
    do_install(&s, &new, &mut m, &mut VecLog::default()).unwrap();
    assert_ne!(m.reg.as_ref().unwrap(), &before);
    registration_is_sound(&m);
    assert_eq!(dll_files(&s).len(), 1);
}

#[test]
fn a_failed_update_puts_the_earlier_registration_back_at_every_step() {
    update_fails_at("write_registration");
    update_fails_at("create_camera");
    update_fails_at("start_camera");
}

#[test]
fn a_failure_before_any_change_leaves_the_machine_untouched() {
    // the machine cannot start
    let s = sandbox("rollback-start");
    let dll = fake_dll(&s.root.join("src"), 1);
    let mut m = Fake {
        fail: Some("start"),
        ..Fake::default()
    };
    assert!(do_install(&s, &dll, &mut m, &mut VecLog::default()).is_err());
    assert!(m.reg.is_none() && !s.paths.data.exists() && !s.paths.install.exists());
}

#[test]
fn a_data_folder_that_cannot_be_prepared_stops_before_the_registry_is_written() {
    // %ProgramData% is a file: nothing can be made below it
    let s = sandbox("rollback-datadir");
    std::fs::remove_dir(&s.paths.program_data).unwrap();
    std::fs::write(&s.paths.program_data, b"not a folder").unwrap();
    let dll = fake_dll(&s.root.join("src"), 1);
    let mut m = Fake::default();
    let err = do_install(&s, &dll, &mut m, &mut VecLog::default()).unwrap_err();
    assert!(!err.is_empty());
    assert!(m.reg.is_none(), "the registry must not be written");
    assert!(!m.calls.contains(&"write_registration".to_string()));
    assert!(dll_files(&s).is_empty());
}

#[test]
fn a_dll_that_cannot_be_copied_stops_before_the_registry_is_written() {
    // the install folder's parent is a file
    let s = sandbox("rollback-copy");
    std::fs::create_dir_all(s.paths.install.parent().unwrap().parent().unwrap()).unwrap();
    std::fs::write(s.paths.install.parent().unwrap(), b"not a folder").unwrap();
    let dll = fake_dll(&s.root.join("src"), 1);
    let mut m = Fake::default();
    let err = do_install(&s, &dll, &mut m, &mut VecLog::default()).unwrap_err();
    assert!(err.contains("install dir"), "{err}");
    assert!(m.reg.is_none());
    assert!(!m.calls.contains(&"write_registration".to_string()));
}

#[test]
fn a_rollback_that_fails_says_so() {
    struct Stubborn(Fake);
    impl Machine for Stubborn {
        fn start(&mut self) -> Res<()> {
            self.0.start()
        }
        fn confirm_user(&mut self, s: &str) -> Res<()> {
            self.0.confirm_user(s)
        }
        fn read_registration(&mut self) -> Option<Registration> {
            self.0.read_registration()
        }
        fn write_registration(&mut self, r: &Registration) -> Res<()> {
            self.0.write_registration(r)
        }
        fn delete_registration(&mut self) -> Vec<String> {
            vec!["HKLM\\x (error 5)".into()]
        }
        fn create_camera(&mut self, d: bool) -> Res<()> {
            self.0.create_camera(d)
        }
        fn remove_camera(&mut self) -> Res<()> {
            self.0.remove_camera()
        }
        fn camera_listed(&mut self) -> bool {
            self.0.camera_listed()
        }
        fn stop_frame_server(&mut self) -> bool {
            self.0.stop_frame_server()
        }
        fn delete_at_reboot(&mut self, p: &Path) -> Res<()> {
            self.0.delete_at_reboot(p)
        }
    }
    impl AsideStore for Stubborn {
        fn aside_record(&mut self) -> Stored {
            self.0.aside_record()
        }
        fn write_aside_record(&mut self, s: &[String]) -> Res<()> {
            self.0.write_aside_record(s)
        }
    }
    let s = sandbox("rollback-incomplete");
    let dll = fake_dll(&s.root.join("src"), 1);
    let mut m = Stubborn(Fake {
        fail: Some("create_camera"),
        ..Fake::default()
    });
    let err = install_with(
        &opts(),
        &dll,
        &s.paths,
        Some(s.p.clone()),
        &mut m,
        &mut VecLog::default(),
    )
    .unwrap_err();
    assert!(err.contains("create_camera"), "{err}");
    assert!(err.contains("run Repair"), "{err}");
    // the DLL is kept: the registry still names it
    assert!(Path::new(&m.0.reg.as_ref().unwrap().dll).is_file());
}

// ---- uninstall

fn installed(name: &str) -> (Sandbox, Fake) {
    let s = sandbox(name);
    let mut m = Fake::default();
    let dll = fake_dll(&s.root.join("src"), 1);
    do_install(&s, &dll, &mut m, &mut VecLog::default()).unwrap();
    (s, m)
}

#[test]
fn removing_a_healthy_install_removes_everything() {
    let (s, mut m) = installed("uninstall-ok");
    let mut log = VecLog::default();
    assert_eq!(
        uninstall_with(&s.paths, &mut m, &mut log).unwrap(),
        UninstallOutcome::Done
    );
    assert!(!m.camera && m.reg.is_none());
    assert!(!s.paths.data.exists());
    assert!(!s.paths.install.exists());
    assert!(log.0.iter().any(|l| l == "unregistered"));
}

#[test]
fn a_failed_remove_stops_the_uninstall_and_keeps_the_keys_and_files() {
    let (s, mut m) = installed("uninstall-remove-fails");
    m.fail = Some("remove_camera");
    let before = m.reg.clone();
    let mut log = VecLog::default();
    let err = uninstall_with(&s.paths, &mut m, &mut log).unwrap_err();
    assert!(err.contains("did not remove the virtual camera"), "{err}");
    assert!(m.camera, "the camera is still there");
    assert_eq!(m.reg, before, "the class the camera names must stay");
    assert!(!m.calls.contains(&"delete_registration".to_string()));
    assert!(s.paths.data.is_dir() && s.paths.frames().is_file());
    assert_eq!(dll_files(&s).len(), 1);
    assert!(log.0.iter().any(|l| l.contains("Remove failed")));
    // a later attempt finishes the job
    m.fail = None;
    uninstall_with(&s.paths, &mut m, &mut VecLog::default()).unwrap();
    assert!(!m.camera && m.reg.is_none() && !s.paths.data.exists());
}

#[test]
fn registry_keys_that_stay_are_an_error_after_everything_else_was_done() {
    let (s, mut m) = installed("uninstall-keys");
    m.registry_leftovers = vec!["HKLM\\SOFTWARE\\BetterCam\\Vcam (error 5)".into()];
    let mut log = VecLog::default();
    let err = uninstall_with(&s.paths, &mut m, &mut log).unwrap_err();
    assert!(
        err.contains("left behind") && err.contains("BetterCam\\Vcam"),
        "{err}"
    );
    assert!(!m.camera);
    assert!(!s.paths.data.exists(), "the rest was still removed");
    assert!(log.0.iter().any(|l| l.contains("left behind")));
}

#[test]
fn a_data_folder_that_cannot_be_deleted_is_an_error() {
    let (s, mut m) = installed("uninstall-folder");
    // a stray file held open without delete sharing, in the folder
    let held_path = s.paths.logs().join("vcam-1.log");
    std::fs::write(&held_path, b"x").unwrap();
    use std::os::windows::fs::OpenOptionsExt;
    let _held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(&held_path)
        .unwrap();
    let mut log = VecLog::default();
    let err = uninstall_with(&s.paths, &mut m, &mut log).unwrap_err();
    assert!(
        err.contains("left behind") && err.contains("BetterCam"),
        "{err}"
    );
    assert!(
        log.0.iter().any(|l| l.contains("could not remove")),
        "{:?}",
        log.0
    );
    assert!(
        m.stop_calls >= 1,
        "the camera service was stopped to release it"
    );
    assert!(
        !m.camera && m.reg.is_none(),
        "the camera and the keys are gone all the same"
    );
}

#[test]
fn a_dll_that_waits_for_a_restart_is_a_pending_result() {
    let (s, mut m) = installed("uninstall-pending");
    let dll = Path::new(&m.reg.as_ref().unwrap().dll).to_path_buf();
    use std::os::windows::fs::OpenOptionsExt;
    let _held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(&dll)
        .unwrap();
    let out = uninstall_with(&s.paths, &mut m, &mut VecLog::default()).unwrap();
    assert_eq!(out, UninstallOutcome::PendingReboot);
    assert_eq!(m.delayed, vec![dll.clone()]);
    // and one that cannot even be scheduled is left behind: an error
    let (s2, mut m2) = installed("uninstall-left");
    m2.reboot_delete_fails = true;
    let dll2 = Path::new(&m2.reg.as_ref().unwrap().dll).to_path_buf();
    let _held2 = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(&dll2)
        .unwrap();
    let err = uninstall_with(&s2.paths, &mut m2, &mut VecLog::default()).unwrap_err();
    assert!(err.contains("left behind"), "{err}");
}

#[test]
fn removing_when_nothing_is_installed_is_a_success() {
    let s = sandbox("uninstall-nothing");
    let mut m = Fake::default();
    assert_eq!(
        uninstall_with(&s.paths, &mut m, &mut VecLog::default()).unwrap(),
        UninstallOutcome::Done
    );
}

#[test]
fn a_half_installed_state_is_cleaned_up_although_remove_says_there_is_no_camera() {
    // Windows answers Remove on a camera that is not registered with an error (seen live: 0xC00D36B2); the keys and
    // files that are left must still go, so that Remove cleans up after a failed install
    let (s, mut m) = installed("uninstall-half");
    m.camera = false;
    let mut log = VecLog::default();
    assert_eq!(
        uninstall_with(&s.paths, &mut m, &mut log).unwrap(),
        UninstallOutcome::Done
    );
    assert!(m.reg.is_none() && !s.paths.data.exists() && !s.paths.install.exists());
    assert!(
        log.0.iter().any(|l| l.contains("it is gone")),
        "{:?}",
        log.0
    );
}

// ---- M3: the error of a failed run reaches install.log

/// The helper writes `ERROR: <why>` as its last act and exits; the app shows that line. When preparing the data
/// folder failed, the log had been let go of (so that the folder can be moved), the line waited for a file, and the
/// process ended with it: the app then showed an earlier line of the same run as the reason.
#[test]
fn the_error_of_a_failed_preparation_reaches_install_log() {
    use std::os::windows::fs::OpenOptionsExt;
    let (s, mut m) = installed("error-line");
    let mut flog = crate::log::FileLog::with_paths_and_principals(s.paths.clone(), s.p.clone());
    assert!(flog.attached());
    // frames.bin is a folder that somebody holds open: it cannot be moved out of the way, so preparing fails
    std::fs::remove_file(s.paths.frames()).unwrap();
    std::fs::create_dir(s.paths.frames()).unwrap();
    let held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(1)
        .custom_flags(0x0200_0000) // FILE_FLAG_BACKUP_SEMANTICS
        .open(s.paths.frames())
        .unwrap();
    let dll = fake_dll(&s.root.join("src2"), 2);
    let started = std::time::SystemTime::now() - std::time::Duration::from_secs(1);
    let err = install_with(
        &opts(),
        &dll,
        &s.paths,
        Some(s.p.clone()),
        &mut m,
        &mut flog,
    )
    .unwrap_err();
    assert!(err.contains("cannot move"), "{err}");
    // what the helper does next, and then ends (the log is dropped with the process)
    flog.line(&format!("ERROR: {err}"));
    drop(flog);
    drop(held);
    let tail = crate::log::tail_of(&s.paths, started, 5);
    let last = tail.last().expect("install.log has no line of this run");
    assert!(
        last.contains("ERROR: ") && last.contains("cannot move"),
        "the last line is not the error: {tail:?}"
    );
    // and the lines before it are there too, once
    let text = std::fs::read_to_string(s.paths.install_log()).unwrap();
    assert_eq!(text.matches("ERROR: ").count(), 1, "{text}");
    assert!(text.contains("it is not a file; moving it aside"), "{text}");
}

/// A log that is dropped without anybody asking for a flush still writes what waited (a helper that returns early).
#[test]
fn lines_that_wait_for_a_file_are_written_when_the_log_is_dropped() {
    let (s, _m) = installed("drop-flush");
    let mut flog = crate::log::FileLog::with_paths_and_principals(s.paths.clone(), s.p.clone());
    flog.release_folder();
    flog.line("written while the folder was let go of");
    assert!(!flog.attached());
    drop(flog);
    let text = std::fs::read_to_string(s.paths.install_log()).unwrap();
    assert!(
        text.contains("written while the folder was let go of"),
        "{text}"
    );
}

// ---- L2: data folders that were moved aside are deleted by the next install and by uninstall

#[test]
fn an_install_and_an_uninstall_delete_the_data_folders_that_were_moved_aside() {
    let (s, mut m) = installed("old-data");
    let dll = fake_dll(&s.root.join("src2"), 2);
    let pd = &s.paths.program_data;
    let mk = |n: &str| {
        let d = pd.join(n);
        std::fs::create_dir_all(d.join("logs")).unwrap();
        std::fs::write(d.join("frames.bin"), b"old").unwrap();
        d
    };
    let a = mk("BetterCam.old-1-0");
    m.put_on_record(pd, "BetterCam.old-1-0");
    let mut log = VecLog::default();
    do_install(&s, &dll, &mut m, &mut log).unwrap();
    assert!(
        !a.exists(),
        "the next install leaves the old copy: {:?}",
        log.0
    );
    assert!(
        s.paths.frames().is_file(),
        "the live data folder is untouched"
    );
    assert!(
        m.names().is_empty(),
        "a name that is gone is taken off the record"
    );
    assert_eq!(
        m.stored, None,
        "the last entry takes the value away, not an empty list"
    );

    let b = mk("BetterCam.old-2-0");
    m.put_on_record(pd, "BetterCam.old-2-0");
    let mut log = VecLog::default();
    uninstall_trusting(&s.paths, &s.p.trusted_owners, &mut m, &mut log).unwrap();
    assert!(!b.exists(), "uninstall leaves the old copy: {:?}", log.0);
    assert!(!s.paths.data.exists());
    assert!(m.names().is_empty());
}

/// An old copy that is still in use at uninstall is reported, not claimed as gone.
#[test]
fn an_old_data_folder_that_cannot_be_deleted_is_reported_by_uninstall() {
    let (s, mut m) = installed("old-data-busy");
    let d = s.paths.program_data.join("BetterCam.old-5-0");
    std::fs::create_dir_all(&d).unwrap();
    m.put_on_record(&s.paths.program_data, "BetterCam.old-5-0");
    let f = d.join("vcam-1.log");
    std::fs::write(&f, b"x").unwrap();
    use std::os::windows::fs::OpenOptionsExt;
    let _held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(&f)
        .unwrap();
    let err = uninstall_trusting(
        &s.paths,
        &s.p.trusted_owners,
        &mut m,
        &mut VecLog::default(),
    )
    .unwrap_err();
    assert!(err.contains("old copy of the data folder"), "{err}");
    assert!(d.exists());
    assert_eq!(
        m.names(),
        ["BetterCam.old-5-0"],
        "a copy that is still in use stays on record, for the next run"
    );
}

// ---- findings 1 and 2: what a user leaves in %ProgramData% never makes install or uninstall wait or fail

/// An entry in `%ProgramData%` named like the helper's old data folders, with a file in it that a program holds an
/// oplock on (the sandbox user made it).
fn entry_with_a_locked_file(s: &Sandbox) -> (PathBuf, vcam_protocol::secure::oplock::Oplock) {
    let d = s.paths.program_data.join("BetterCam.old-test");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("f.txt"), vec![1u8; 4096]).unwrap();
    let lock = lock_on(&d.join("f.txt")).expect("an oplock");
    (d, lock)
}

/// Repair with such an entry present: it returns within the bound and succeeds; the entry's file is still locked
/// and still there. The old code waited in `remove_dir_all` for as long as the lock was held, with the machine-wide
/// lock taken: every later helper exited 11.
#[test]
fn a_repair_does_not_wait_for_or_fail_on_an_old_data_folder_with_a_locked_file() {
    let (s, mut m) = installed("repair-locked-aside");
    let (d, lock) = entry_with_a_locked_file(&s);
    m.put_on_record(&s.paths.program_data, "BetterCam.old-test");
    let dll = fake_dll(&s.root.join("src2"), 2);
    let (paths, p) = (s.paths.clone(), s.p.clone());
    let (r, lines, m) = within("repair", BOUND, move || {
        let mut log = VecLog::default();
        let r = install_with(&opts(), &dll, &paths, Some(p), &mut m, &mut log);
        (r, log.0, m)
    });
    assert_eq!(r.unwrap(), InstallOutcome::Done, "{lines:?}");
    assert!(m.camera);
    assert!(
        lines
            .iter()
            .any(|l| l.contains("BetterCam.old-test") && l.contains("locked by another program")),
        "{lines:?}"
    );
    assert_eq!(std::fs::metadata(d.join("f.txt")).unwrap().len(), 4096);
    drop(lock);
}

/// Remove: an entry that is not the helper's (its owner is not trusted) is left as it is and is no failure, and no
/// wait; one that is the helper's with a lock in it is reported as left behind, truthfully, without a wait.
#[test]
fn an_uninstall_does_not_wait_for_a_locked_file_in_an_old_data_folder() {
    let (s, mut m) = installed("uninstall-locked-aside");
    let (d, lock) = entry_with_a_locked_file(&s);
    m.put_on_record(&s.paths.program_data, "BetterCam.old-test");
    // 1. not the helper's: only SYSTEM is trusted, the sandbox user made the entry
    let (paths, only_system) = (s.paths.clone(), vec![crate::acl::SID_SYSTEM.to_string()]);
    let (r, mut m) = within("uninstall (somebody else's entry)", BOUND, move || {
        let mut m = m;
        let r = uninstall_trusting(&paths, &only_system, &mut m, &mut VecLog::default());
        (r, m)
    });
    assert_eq!(r.unwrap(), UninstallOutcome::Done);
    assert!(
        !s.paths.data.exists() && !m.camera,
        "the removal itself succeeded"
    );
    assert_eq!(std::fs::metadata(d.join("f.txt")).unwrap().len(), 4096);
    assert!(
        m.names().is_empty(),
        "an entry that is not the helper's is taken off the record"
    );

    // 2. the helper's own (trusted owner), with a lock in it: reported, not waited for
    m.put_on_record(&s.paths.program_data, "BetterCam.old-test");
    let (paths, trusted) = (s.paths.clone(), s.p.trusted_owners.clone());
    let (r, lines) = within("uninstall (the helper's entry)", BOUND, move || {
        let mut log = VecLog::default();
        (
            uninstall_trusting(&paths, &trusted, &mut m, &mut log),
            log.0,
        )
    });
    let err = r.unwrap_err();
    assert!(err.contains("old copy of the data folder"), "{err}");
    assert!(
        lines
            .iter()
            .any(|l| l.contains("f.txt") && l.contains("locked by another program")),
        "{lines:?}"
    );
    drop(lock);
    let mut last = Fake::default();
    last.put_on_record(&s.paths.program_data, "BetterCam.old-test");
    uninstall_trusting(
        &s.paths,
        &s.p.trusted_owners,
        &mut last,
        &mut VecLog::default(),
    )
    .unwrap();
    assert!(!d.exists());
}

/// Remove with a program holding a lock on a file of the live data folder (the user may read `install.log`): the
/// removal returns within the bound and says what stayed; the camera and the keys are gone all the same.
#[test]
fn an_uninstall_does_not_wait_for_a_locked_install_log_and_says_what_stayed() {
    let (s, m) = installed("uninstall-locked-log");
    let Some(lock) = lock_on(&s.paths.install_log()) else {
        return;
    };
    let (paths, trusted) = (s.paths.clone(), s.p.trusted_owners.clone());
    let (r, lines, m) = within("uninstall", BOUND, move || {
        let mut m = m;
        let mut log = VecLog::default();
        let r = uninstall_trusting(&paths, &trusted, &mut m, &mut log);
        (r, log.0, m)
    });
    let err = r.unwrap_err();
    assert!(
        err.contains("left behind")
            && err.contains("install.log")
            && err.contains("locked by another program"),
        "{err}"
    );
    assert!(!m.camera && m.reg.is_none());
    assert!(
        lines.iter().any(|l| l.contains("could not remove")),
        "{lines:?}"
    );
    assert!(s.paths.install_log().exists() && !s.paths.frames().exists());
    drop(lock);
    // the next removal finishes the job
    uninstall_with(&s.paths, &mut Fake::default(), &mut VecLog::default()).unwrap();
    assert!(!s.paths.data.exists());
}

// ---- M2: the copy of the DLL does not wait for a program that holds a lock

use vcam_protocol::secure::oplock::{self, Oplock};

/// Where an install of `dll` puts its copy.
fn dest_of(s: &Sandbox, dll: &Path) -> PathBuf {
    let tag = content_tag(&std::fs::read(dll).unwrap());
    s.paths
        .install
        .join(vcam_protocol::versioned_dll_name(&tag))
}

fn dll_bytes(dll: &Path) -> Vec<u8> {
    std::fs::read(dll).unwrap()
}

/// The DLL to install is in a folder the user runs the app from; a program of theirs that holds a read-write-handle
/// lock on it made `std::fs::read` wait, with the machine-wide lock held. Now it is an error that names the file,
/// before anything is touched, and a check whether the file looks like a DLL does not wait either.
#[test]
fn a_dll_source_that_a_program_holds_a_lock_on_is_a_named_error_and_not_a_wait() {
    let s = sandbox("dll-source-locked");
    let dll = fake_dll(&s.root.join("src"), 1);
    let Some(lock) = lock_on(&dll) else {
        return;
    };
    let (paths, p, d) = (s.paths.clone(), s.p.clone(), dll.clone());
    let (r, calls) = within("install", BOUND, move || {
        let mut m = Fake::default();
        let r = install_with(&opts(), &d, &paths, Some(p), &mut m, &mut VecLog::default());
        (r, m.calls)
    });
    let err = r.unwrap_err();
    assert!(
        err.contains("locked by another program") && err.contains("better_cam_vcam.dll"),
        "{err}"
    );
    assert!(calls.is_empty(), "nothing was touched: {calls:?}");
    assert!(!s.paths.data.exists() && !s.paths.install.exists());
    let d = dll.clone();
    assert!(
        !within("looks like a DLL", BOUND, move || file_looks_like_dll(&d)),
        "a file that cannot be looked at for now does not look like a DLL"
    );
    let d = dll.clone();
    let e = within("build tag", BOUND, move || crate::util::dll_build(&d)).unwrap_err();
    assert!(e.contains("locked") || e.contains("lock"), "{e}");
    drop(lock);
    assert!(file_looks_like_dll(&dll));
    let mut m = Fake::default();
    assert_eq!(
        do_install(&s, &dll, &mut m, &mut VecLog::default()).unwrap(),
        InstallOutcome::Done
    );
}

/// The installed copy is locked (a user may read `Program Files`): a Repair says so, within the bound, names the file,
/// keeps the registration, and the next one works.
#[test]
fn an_installed_copy_that_a_program_holds_a_lock_on_is_a_named_error_and_not_a_wait() {
    let (s, mut m) = installed("dll-dest-locked");
    let dll = s.root.join("src").join("better_cam_vcam.dll");
    let dest = PathBuf::from(m.reg.as_ref().unwrap().dll.clone());
    let before = m.reg.clone();
    let Some(lock) = lock_on(&dest) else {
        return;
    };
    let (paths, p) = (s.paths.clone(), s.p.clone());
    let (r, m) = within("repair", BOUND, move || {
        let r = install_with(
            &opts(),
            &dll,
            &paths,
            Some(p),
            &mut m,
            &mut VecLog::default(),
        );
        (r, m)
    });
    let err = r.unwrap_err();
    assert!(
        err.contains("locked by another program") && err.contains("better_cam_vcam_"),
        "{err}"
    );
    assert_eq!(m.reg, before, "the registration is as it was");
    assert!(m.camera);
    drop(lock);
    let mut m = m;
    let dll = s.root.join("src").join("better_cam_vcam.dll");
    assert_eq!(
        do_install(&s, &dll, &mut m, &mut VecLog::default()).unwrap(),
        InstallOutcome::Done
    );
}

/// A copy with other contents under the name (a damaged one) is replaced, in one step. A program that holds a lock of
/// either kind on it (the read-handle lock is the one a rename breaks, a read does not) is not waited for: the
/// error names the file, the old copy is as it was, and no temporary file is left.
#[test]
fn a_different_copy_that_a_program_holds_a_lock_on_is_not_replaced_and_not_waited_for() {
    let s = sandbox("dll-replace-locked");
    let dll = fake_dll(&s.root.join("src"), 7);
    let bytes = dll_bytes(&dll);
    let dest = dest_of(&s, &dll);
    std::fs::create_dir_all(&s.paths.install).unwrap();
    let old = vec![9u8; 70_000];
    for level in [oplock::READ_HANDLE, oplock::READ_WRITE_HANDLE] {
        std::fs::write(&dest, &old).unwrap();
        let Some(lock) = Oplock::take_level(&dest, level).unwrap() else {
            eprintln!("this file system grants no oplock: skipped");
            return;
        };
        let (b, d, p) = (bytes.clone(), dest.clone(), s.p.clone());
        let r = within("copy", BOUND, move || {
            copy_dll(&b, &d, &p, &mut VecLog::default())
        });
        // (both kinds are met at the read of the copy, which says so)
        let err = r.unwrap_err();
        assert!(
            err.contains("locked by another program") && err.contains("better_cam_vcam_"),
            "level {level}: {err}"
        );
        // (attribute-only calls: a read of the file would wait)
        assert_eq!(std::fs::metadata(&dest).unwrap().len(), old.len() as u64);
        assert!(
            !dest.with_extension("dll.tmp").exists(),
            "the temporary file is not left"
        );
        drop(lock);
        assert!(copy_dll(&bytes, &dest, &s.p, &mut VecLog::default()).unwrap());
        assert_eq!(std::fs::read(&dest).unwrap(), bytes);
    }

    // A lock that is taken after the copy was read and before the rename (so it is the rename that meets it): the
    // temporary file is not moved, the copy is as it was, and the message is about *moving* the one to the other and
    // names both (a lock on either file gives this refusal; it used to name the copy only)
    std::fs::write(&dest, &old).unwrap();
    let mut lock = None;
    let result = copy_dll_with(&bytes, &dest, &s.p, &mut VecLog::default(), &mut |_| {
        lock = Oplock::take(&dest).unwrap();
    });
    if lock.is_none() {
        eprintln!("this file system grants no oplock: that part is skipped");
        return;
    }
    let err = result.unwrap_err();
    let (tmp, to) = (
        dest.with_extension("dll.tmp").display().to_string(),
        dest.display().to_string(),
    );
    assert!(
        err.contains("cannot move")
            && err.contains("locked by another program")
            && err.contains(&tmp)
            && err.contains(&to),
        "{err}"
    );
    assert_eq!(std::fs::metadata(&dest).unwrap().len(), old.len() as u64);
    assert!(
        !dest.with_extension("dll.tmp").exists(),
        "the temporary file is not left"
    );
    drop(lock);
    assert!(copy_dll(&bytes, &dest, &s.p, &mut VecLog::default()).unwrap());
}

/// A temporary file that an earlier run left and that a program holds a lock on: the same, and it names that file.
#[test]
fn a_leftover_temporary_file_that_a_program_holds_a_lock_on_is_a_named_error_and_not_a_wait() {
    let s = sandbox("dll-tmp-locked");
    let dll = fake_dll(&s.root.join("src"), 3);
    let bytes = dll_bytes(&dll);
    let dest = dest_of(&s, &dll);
    std::fs::create_dir_all(&s.paths.install).unwrap();
    let tmp = dest.with_extension("dll.tmp");
    std::fs::write(&tmp, b"half a DLL").unwrap();
    let Some(lock) = lock_on(&tmp) else {
        return;
    };
    let (b, d, p) = (bytes.clone(), dest.clone(), s.p.clone());
    let err = within("copy", BOUND, move || {
        copy_dll(&b, &d, &p, &mut VecLog::default())
    })
    .unwrap_err();
    assert!(
        err.contains("locked by another program") && err.contains(".dll.tmp"),
        "{err}"
    );
    assert!(!dest.exists(), "no copy under the real name");
    assert_eq!(std::fs::metadata(&tmp).unwrap().len(), 10);
    drop(lock);
    assert!(copy_dll(&bytes, &dest, &s.p, &mut VecLog::default()).unwrap());
    assert_eq!(std::fs::read(&dest).unwrap(), bytes);
    assert!(!tmp.exists());
}

/// Nothing locked: the copy is made, a copy that is there is recognised, a different one is replaced, a leftover
/// temporary file does not matter, and a failure after the copy puts everything back (the copy goes).
#[test]
fn the_copy_works_when_nothing_is_locked_and_is_rolled_back_when_the_install_fails() {
    let s = sandbox("dll-copy-plain");
    let dll = fake_dll(&s.root.join("src"), 4);
    let bytes = dll_bytes(&dll);
    let dest = dest_of(&s, &dll);
    let mut log = VecLog::default();
    assert!(copy_dll(&bytes, &dest, &s.p, &mut log).unwrap());
    assert_eq!(std::fs::read(&dest).unwrap(), bytes);
    assert!(
        !copy_dll(&bytes, &dest, &s.p, &mut log).unwrap(),
        "already there"
    );
    assert!(log.0.iter().any(|l| l.contains("already present")));
    // other contents under the name, and a leftover temporary file
    std::fs::write(&dest, b"damaged").unwrap();
    std::fs::write(dest.with_extension("dll.tmp"), b"leftover").unwrap();
    assert!(copy_dll(&bytes, &dest, &s.p, &mut log).unwrap());
    assert_eq!(std::fs::read(&dest).unwrap(), bytes);
    assert!(!dest.with_extension("dll.tmp").exists());
    // the copy is a real file (not a handle's partial write): the same size and a plain one
    assert_eq!(std::fs::metadata(&dest).unwrap().len(), bytes.len() as u64);

    // an install that finds a damaged copy under its name replaces it, and a failing camera then removes it again
    std::fs::write(&dest, b"damaged").unwrap();
    let mut m = Fake {
        fail: Some("create_camera"),
        ..Fake::default()
    };
    let err = do_install(&s, &dll, &mut m, &mut VecLog::default()).unwrap_err();
    assert!(err.contains("create_camera"), "{err}");
    assert!(
        !dest.exists(),
        "the copy that this run made is deleted by the rollback"
    );
    assert!(m.reg.is_none());
    // and the next one works
    m.fail = None;
    do_install(&s, &dll, &mut m, &mut VecLog::default()).unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), bytes);
    registration_is_sound(&m);
}

// ---- the record of what the helper moved aside (findings 1, 4, 5, 7)

fn remove_all_asides(s: &Sandbox) {
    for a in crate::testkit::asides_in(&s.paths) {
        crate::datadir::remove_no_follow(&a).unwrap();
    }
}

/// A data folder that a user made (its owner is not SYSTEM or Administrators) is moved aside and, with a file in it
/// that cannot be deleted, stays. It is **not put on the record**: the old code recorded it, so that a user who kept a
/// file open and then swapped the name for an administrator-owned folder got that folder deleted by the next run.
/// The entry is left where it is, with one line that says why, and no record is written.
#[test]
fn a_data_folder_that_is_not_owned_by_a_trusted_account_is_never_put_on_record() {
    let mut s = sandbox("record-untrusted");
    let mut m = Fake::default();
    let dll = fake_dll(&s.root.join("src"), 1);
    do_install(&s, &dll, &mut m, &mut VecLog::default()).unwrap();
    // only SYSTEM is trusted from now on (the sandbox user owns the data folder), with an object in it that cannot be
    // deleted: the folder goes aside and stays
    s.p.trusted_owners = vec![crate::acl::SID_SYSTEM.into()];
    crate::testkit::undeletable_file(&s.paths.data, "junk.txt", &s.p.owner);
    let mut log = VecLog::default();
    do_install(&s, &dll, &mut m, &mut log).unwrap();
    let asides = crate::testkit::asides_in(&s.paths);
    assert_eq!(asides.len(), 1, "{:?}", log.0);
    assert!(m.record_writes.is_empty(), "{:?}", m.record_writes);
    assert_eq!(m.stored, None);
    assert!(
        log.0.iter().any(|l| l.contains("old copy kept as")
            && l.contains("left where it is")
            && l.contains("not SYSTEM or Administrators")),
        "{:?}",
        log.0
    );
    assert!(asides[0].exists());
    crate::testkit::make_deletable(&asides[0].join("junk.txt"), &s.p.owner);
    remove_all_asides(&s);
}

/// A data folder that is the helper's (a trusted owner; here a file named like the folder) is put on the record
/// **before** it is renamed, with its identity, and taken off again when the old copy is deleted at once. The fake
/// notes whether the data folder still existed under its name at each write.
#[test]
fn a_moved_aside_folder_is_put_on_record_before_the_rename_and_off_it_when_it_is_deleted() {
    let s = sandbox("record-before");
    std::fs::write(&s.paths.data, b"a file where the data folder goes").unwrap();
    let before = crate::testkit::identity_of(&s.paths.data);
    let mut m = Fake {
        watch: Some(s.paths.data.clone()),
        ..Fake::default()
    };
    let dll = fake_dll(&s.root.join("src"), 1);
    let mut log = VecLog::default();
    do_install(&s, &dll, &mut m, &mut log).unwrap();
    assert_eq!(m.record_writes.len(), 2, "{:?}", log.0);
    let first = &m.record_writes[0];
    assert_eq!(first.len(), 1);
    let entry = aside::AsideEntry::parse(&first[0]).expect("an entry with an identity");
    assert!(entry.name.starts_with("BetterCam.old-"), "{entry:?}");
    assert_eq!(
        entry.identity, before,
        "the identity read on the handle that was renamed"
    );
    assert_eq!(
        m.exists_at_write,
        [true, false],
        "written while the data folder still had its name, taken off once the copy was deleted"
    );
    assert!(m.record_writes[1].is_empty());
    assert_eq!(m.stored, None, "nothing is left on the record");
    assert!(s.paths.data.is_dir() && crate::testkit::asides_in(&s.paths).is_empty());
}

/// A record that cannot be written is said and does not fail the install. And, the other half of a fault in the
/// rollback: a failing step after that rolls back completely: nothing about the record makes it "incomplete".
#[test]
fn a_record_that_cannot_be_written_is_a_line_of_the_log_and_never_an_incomplete_rollback() {
    let s = sandbox("record-fails");
    std::fs::write(&s.paths.data, b"a file where the data folder goes").unwrap();
    let dll = fake_dll(&s.root.join("src"), 1);
    // the install goes through
    let mut m = Fake {
        record_fails: true,
        ..Fake::default()
    };
    let mut log = VecLog::default();
    do_install(&s, &dll, &mut m, &mut log).unwrap();
    assert!(
        log.0
            .iter()
            .any(|l| l.contains("could not put") && l.contains("on record")),
        "{:?}",
        log.0
    );
    // and a later failing camera is rolled back completely, with the record failing all the while
    let s = sandbox("record-fails-rollback");
    std::fs::write(&s.paths.data, b"a file where the data folder goes").unwrap();
    let mut m = Fake {
        record_fails: true,
        fail: Some("create_camera"),
        ..Fake::default()
    };
    let err = do_install(&s, &dll, &mut m, &mut VecLog::default()).unwrap_err();
    assert!(err.contains("create_camera"), "{err}");
    assert!(
        err.contains("nothing was changed") && !err.contains("incomplete"),
        "{err}"
    );
    assert!(m.reg.is_none());
    assert!(
        dll_files(&s).is_empty(),
        "the copy was deleted by the rollback"
    );
}

/// A record that exists and cannot be read (another type, too long) is not acted on and not overwritten: nothing is
/// deleted because of it, nothing is written, and the install goes on. Strings in it that are not entries are
/// dropped, and the good entries next to them are still swept.
#[test]
fn a_damaged_record_is_not_overwritten_and_not_acted_on() {
    let (s, mut m) = installed("record-damaged");
    let pd = s.paths.program_data.clone();
    let old = pd.join("BetterCam.old-1-0");
    std::fs::create_dir_all(old.join("logs")).unwrap();
    let dll = fake_dll(&s.root.join("src2"), 2);
    let good = crate::testkit::entry_of(&pd, "BetterCam.old-1-0");
    for damaged in [
        Stored::Unreadable("error 1630 (another type)".into()),
        Stored::Unreadable("error 234 (more data)".into()),
        Stored::Strings(
            (0..=aside::MAX_ENTRIES)
                .map(|i| format!("BetterCam.old-{i}-0|0000000000000001|{:032x}", i + 1))
                .collect(),
        ),
    ] {
        m.stored = Some(damaged.clone());
        m.record_writes.clear();
        let mut log = VecLog::default();
        do_install(&s, &dll, &mut m, &mut log).unwrap();
        assert!(old.is_dir(), "an old copy was deleted on a damaged record");
        assert_eq!(
            m.stored,
            Some(damaged),
            "the damaged record was overwritten"
        );
        assert!(m.record_writes.is_empty());
        assert!(
            log.0.iter().any(|l| l.contains("cannot be read")),
            "{:?}",
            log.0
        );
    }
    // strings that are not entries are dropped; the entry next to them is the helper's and is swept
    m.stored = Some(Stored::Strings(vec![
        "junk".into(),
        good.encode(),
        "BetterCam.old-9-0|zz|zz".into(),
    ]));
    let mut log = VecLog::default();
    do_install(&s, &dll, &mut m, &mut log).unwrap();
    assert!(!old.exists(), "{:?}", log.0);
    assert_eq!(
        m.stored, None,
        "the junk went with the write that removed the entry"
    );
    assert!(
        log.0
            .iter()
            .any(|l| l.contains("2 string(s)") && l.contains("not entries")),
        "{:?}",
        log.0
    );
    // a record that holds nothing but strings that are not entries is cleaned up by the run that found it
    m.stored = Some(Stored::Strings(vec!["junk".into(), "also|junk".into()]));
    do_install(&s, &dll, &mut m, &mut VecLog::default()).unwrap();
    assert_eq!(m.stored, None, "the junk stayed on the record");
}

/// The reviewer's chain, end to end: a name is on the record, the folder behind it is deleted, and another folder
/// with a *trusted owner* (here: the same account, which is trusted in the sandbox) is renamed into the name. The old
/// code saw "a recorded name, not a link, trusted owner" and deleted it with administrator rights. Now the folder is
/// not the one that was recorded: it is left exactly as it is, and the name is dropped from the record.
#[test]
fn a_recorded_name_that_was_freed_and_filled_with_another_folder_is_left_alone() {
    let (s, mut m) = installed("record-swapped");
    let pd = s.paths.program_data.clone();
    let name = "BetterCam.old-1-0";
    let mk = || {
        std::fs::create_dir_all(pd.join(name).join("logs")).unwrap();
        std::fs::write(pd.join(name).join("frames.bin"), b"theirs").unwrap();
    };
    mk();
    m.put_on_record(&pd, name);
    // the user keeps it from being deleted at once and later frees the name
    crate::datadir::remove_no_follow(&pd.join(name)).unwrap();
    // another folder of the same account takes the name (an administrator-owned folder that the user may rename)
    let other = pd.join("Somebody-elses-folder");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join("precious.txt"), b"precious").unwrap();
    std::fs::rename(&other, pd.join(name)).unwrap();
    let dll = fake_dll(&s.root.join("src2"), 2);
    let mut log = VecLog::default();
    do_install(&s, &dll, &mut m, &mut log).unwrap();
    assert_eq!(
        std::fs::read(pd.join(name).join("precious.txt")).unwrap(),
        b"precious",
        "the folder that took the name was deleted: {:?}",
        log.0
    );
    assert!(m.names().is_empty(), "the name is dropped from the record");
    assert!(
        log.0
            .iter()
            .any(|l| l.contains("left alone") && l.contains(name)),
        "{:?}",
        log.0
    );
    // and the same for an uninstall
    m.put_on_record(&pd, name);
    crate::datadir::remove_no_follow(&pd.join(name)).unwrap();
    std::fs::create_dir_all(other.join("sub")).unwrap();
    std::fs::write(other.join("precious.txt"), b"precious").unwrap();
    std::fs::rename(&other, pd.join(name)).unwrap();
    uninstall_trusting(
        &s.paths,
        &s.p.trusted_owners,
        &mut m,
        &mut VecLog::default(),
    )
    .unwrap();
    assert!(pd.join(name).join("precious.txt").is_file());
    assert!(m.names().is_empty());
}

// ---- the temporary DLL (findings 3 and 6)

/// The temporary file is made with a protected DACL that grants only SYSTEM and Administrators (here: the account
/// that plays the administrators), so no standard user can open or lock it before the rename. A rename keeps that
/// DACL (`acl::tests::a_rename_keeps_a_protected_dacl...`), so the final file is given what its folder hands down: not
/// protected, with the entry that `Program Files` gives its readers (the camera service reads the DLL as LOCAL
/// SERVICE through such an entry).
#[test]
fn the_temporary_dll_is_private_and_the_final_one_has_the_permissions_of_its_folder() {
    use crate::acl::{self, Ace};
    let s = sandbox("dll-permissions");
    let dll = fake_dll(&s.root.join("src"), 5);
    let bytes = dll_bytes(&dll);
    let dest = dest_of(&s, &dll);
    // the install folder hands down read access to somebody else (as `Program Files` does to Users)
    std::fs::create_dir_all(&s.paths.install).unwrap();
    {
        let h = secure::open_with(
            &s.paths.install,
            0x8000_0000 | acl::SECURITY_ACCESS,
            secure::SHARE_ALL,
            secure::DISP_OPEN_EXISTING,
            true,
            0,
        )
        .unwrap();
        let inherit = acl::OBJECT_INHERIT | acl::CONTAINER_INHERIT;
        let aces = [
            Ace {
                sid: s.p.owner.clone(),
                mask: acl::FULL,
                flags: inherit,
            },
            Ace {
                sid: crate::testkit::FAKE_USER.into(),
                mask: acl::FILE_READ,
                flags: inherit,
            },
        ];
        acl::apply_aces(&h, &aces, &s.p.owner).unwrap();
    }
    let mut seen = None;
    let copied = copy_dll_with(&bytes, &dest, &s.p, &mut VecLog::default(), &mut |tmp| {
        seen = Some(crate::testkit::security_of(tmp));
    })
    .unwrap();
    assert!(copied);
    // the temporary file, as it was between the write and the rename: protected, exactly SYSTEM and the
    // administrators, and nothing for the user the folder gives read access to
    let tmp_acl = seen.expect("the hook ran between the write and the rename");
    acl::check_dacl(
        &tmp_acl,
        &[
            Ace {
                sid: s.p.system.clone(),
                mask: acl::FULL,
                flags: 0,
            },
            Ace {
                sid: s.p.admins.clone(),
                mask: acl::FULL,
                flags: 0,
            },
        ],
    )
    .unwrap_or_else(|e| panic!("the temporary file's DACL is not the private one: {e}"));
    assert!(!tmp_acl
        .aces
        .iter()
        .any(|a| acl::sid_eq(&a.sid, crate::testkit::FAKE_USER)));
    // the final file: the folder's permissions
    let fin = crate::testkit::security_of(&dest);
    assert!(!fin.protected, "the DACL of the copy does not inherit");
    let user = fin
        .aces
        .iter()
        .find(|a| acl::sid_eq(&a.sid, crate::testkit::FAKE_USER))
        .unwrap_or_else(|| panic!("the entry the folder hands down is missing: {:?}", fin.aces));
    assert_eq!(user.mask & acl::FILE_READ, acl::FILE_READ);
    assert_eq!(std::fs::read(&dest).unwrap(), bytes);
    assert!(!dest.with_extension("dll.tmp").exists());
    // a copy that is there and right is left as it is
    assert!(!copy_dll(&bytes, &dest, &s.p, &mut VecLog::default()).unwrap());
    assert_eq!(crate::testkit::security_of(&dest), fin);
}

/// The sentence for a failed move names both files, whichever of them another program holds a lock on, and whatever
/// else went wrong.
#[test]
fn the_error_of_a_failed_move_names_both_files() {
    let from = Path::new(r"C:\x\better_cam_vcam_1.abcd1234.dll.tmp");
    let to = Path::new(r"C:\x\better_cam_vcam_1.abcd1234.dll");
    let busy = secure::refused(format!(
        "{}: another program holds a lock on the file (an oplock break is in progress); not waiting for it",
        to.display()
    ));
    assert!(secure::is_oplock_busy(&busy));
    for e in [busy, std::io::Error::from_raw_os_error(5)] {
        let text = rename_error(from, to, &e);
        assert!(
            text.contains(&from.display().to_string()) && text.contains(&to.display().to_string()),
            "{text}"
        );
        assert!(text.contains("move"), "{text}");
    }
}
