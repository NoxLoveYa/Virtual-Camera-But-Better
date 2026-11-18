//! What install and uninstall do to the machine outside the data folder: the registry, the virtual camera object, the
//! camera service, files that wait for a reboot. A trait, so the order of the steps and the rollback are tested with
//! a fake that fails on demand; [`RealMachine`] is the Windows implementation.

use std::path::Path;
use std::time::Duration;

pub use crate::registry::Stored;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_NOT_FOUND, ERROR_PATH_NOT_FOUND,
};
use windows::Win32::Security::{
    AdjustTokenPrivileges, LookupPrivilegeValueW, LUID_AND_ATTRIBUTES, SE_PRIVILEGE_ENABLED,
    TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
};
use windows::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_DELAY_UNTIL_REBOOT};
use windows::Win32::System::Services::{
    CloseServiceHandle, ControlService, OpenSCManagerW, OpenServiceW, QueryServiceStatus,
    SC_MANAGER_CONNECT, SERVICE_CONTROL_STOP, SERVICE_QUERY_STATUS, SERVICE_STATUS, SERVICE_STOP,
    SERVICE_STOPPED,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_core::PCWSTR;

use crate::camera::{add_dshow_class, create_virtual_camera};
use crate::registry::{self, Keys};
use crate::util::{init_com_and_mf, pcwstr, werr, wide, Res};

/// What `HKLM` records about the installed DLL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Registration {
    /// Full path of the DLL (`InprocServer32` default value and `DllPath`).
    pub dll: String,
    /// The build tag in the DLL's name.
    pub build: String,
    pub protocol: u32,
}

/// Where the record of moved-aside data folders is kept: two calls, so that everything that is decided about the
/// record ([`crate::aside`]) is tested against a plain fake, and the real implementation stays a thin layer over the
/// registry.
pub trait AsideStore {
    /// The record of the data folders this helper moved aside in `%ProgramData%`, as the strings stored (see
    /// [`crate::aside`] for what is in them). Kept where only administrators can write (the helper's HKLM key), so
    /// the sweep acts on exactly these and not on whatever a user made under a similar name. A record that is not
    /// there is [`Stored::Missing`]; one that is there and cannot be read is [`Stored::Unreadable`], and is never
    /// overwritten.
    fn aside_record(&mut self) -> Stored;
    /// Replace the record by `strings`. An empty list removes the value (and the state key when it holds nothing
    /// else and no registration is there). The caller has read the record first and has not got
    /// [`Stored::Unreadable`] for it.
    fn write_aside_record(&mut self, strings: &[String]) -> Res<()>;
}

/// The parts of the machine that install and uninstall change besides the data folder and the DLL copies.
pub trait Machine: AsideStore {
    /// Prepare the process: privileges, COM, Media Foundation. Called once, before anything is changed.
    fn start(&mut self) -> Res<()>;
    /// `sid` names a user account on this computer.
    fn confirm_user(&mut self, sid: &str) -> Res<()>;
    /// The registration as it is, `None` when there is none (or it is incomplete).
    fn read_registration(&mut self) -> Option<Registration>;
    /// Write the COM class and the state key.
    fn write_registration(&mut self, r: &Registration) -> Res<()>;
    /// Delete the COM class and the registration values of the state key; the texts of what could not be deleted
    /// (empty: all gone). **The record of moved-aside data folders ([`Machine::aside_record`]) is never touched and
    /// never part of the list**: it is not rewritten, so there is no moment in which it is gone, and nothing about it
    /// can make a rollback incomplete. The state key itself goes when no record is in it.
    fn delete_registration(&mut self) -> Vec<String>;
    /// Create the virtual camera and start it (`dshow`: the DirectShow class fallback first).
    fn create_camera(&mut self, dshow: bool) -> Res<()>;
    /// Remove the virtual camera from Windows. `Remove` on a camera that was never registered (or already
    /// removed) fails with `MF_E_INVALIDREQUEST`, so a failure alone does not say the camera is still there: see
    /// [`Machine::camera_listed`].
    fn remove_camera(&mut self) -> Res<()>;
    /// Windows lists the camera. When this cannot be found out, true (a camera that cannot be shown to be gone is
    /// not gone).
    fn camera_listed(&mut self) -> bool;
    /// Stop the camera service (Frame Server); true when it is stopped.
    fn stop_frame_server(&mut self) -> bool;
    /// Delete a file that is in use at the next restart.
    fn delete_at_reboot(&mut self, p: &Path) -> Res<()>;
}

/// The real machine. The registry keys it works on are a parameter ([`Keys`]): production uses `HKLM`
/// ([`RealMachine::new`]); the tests run this same code against a throwaway key under `HKCU`.
pub struct RealMachine {
    keys: Keys,
}

impl Default for RealMachine {
    fn default() -> Self {
        RealMachine::new()
    }
}

impl RealMachine {
    pub fn new() -> RealMachine {
        RealMachine {
            keys: Keys::machine(),
        }
    }

    /// The real implementation over other keys (a test's throwaway key under `HKCU`; never `HKLM`).
    #[cfg(test)]
    pub(crate) fn with_keys(keys: Keys) -> RealMachine {
        RealMachine { keys }
    }
}

/// Switch on the privileges that let the helper repair a folder somebody else owns (best effort).
fn enable_privileges() {
    unsafe {
        let mut token = windows::Win32::Foundation::HANDLE::default();
        if OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        )
        .is_err()
        {
            return;
        }
        for name in [
            "SeTakeOwnershipPrivilege",
            "SeRestorePrivilege",
            "SeBackupPrivilege",
        ] {
            let w = wide(name);
            let mut luid = Default::default();
            if LookupPrivilegeValueW(PCWSTR::null(), pcwstr(&w), &mut luid).is_err() {
                continue;
            }
            let tp = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                Privileges: [LUID_AND_ATTRIBUTES {
                    Luid: luid,
                    Attributes: SE_PRIVILEGE_ENABLED,
                }],
            };
            let _ = AdjustTokenPrivileges(token, false, Some(&tp), 0, None, None);
        }
        let _ = CloseHandle(token);
    }
}

fn not_found(e: &windows_core::Error) -> bool {
    [ERROR_NOT_FOUND, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND]
        .iter()
        .any(|c| e.code() == c.to_hresult())
}

impl Machine for RealMachine {
    fn start(&mut self) -> Res<()> {
        if crate::is_elevated() {
            enable_privileges();
        }
        init_com_and_mf().map_err(|e| werr("MFStartup", e))
    }

    fn confirm_user(&mut self, sid: &str) -> Res<()> {
        crate::acl::confirm_user_account(sid)
    }

    fn read_registration(&mut self) -> Option<Registration> {
        let r = registry::read_registered_in(&self.keys)?;
        Some(Registration {
            dll: r.dll_path?,
            build: r.build?,
            protocol: r.protocol?,
        })
    }

    fn write_registration(&mut self, r: &Registration) -> Res<()> {
        let (root, key, state) = (self.keys.root, &self.keys.clsid, &self.keys.state);
        registry::set_string(root, key, None, vcam_protocol::CLSID_DESCRIPTION)?;
        let inproc = format!(r"{key}\InprocServer32");
        registry::set_string(root, &inproc, None, &r.dll)?;
        registry::set_string(root, &inproc, Some("ThreadingModel"), "Both")?;
        registry::set_string(root, state, Some("DllPath"), &r.dll)?;
        registry::set_dword(root, state, "Protocol", r.protocol)?;
        registry::set_string(root, state, Some("Build"), &r.build)?;
        Ok(())
    }

    fn delete_registration(&mut self) -> Vec<String> {
        let mut failed = Vec::new();
        let mut note = |what: String, r: windows::Win32::Foundation::WIN32_ERROR| {
            if r != windows::Win32::Foundation::ERROR_SUCCESS && !registry::is_missing(r) {
                failed.push(format!("{what} (error {})", r.0));
            }
        };
        let root = self.keys.root;
        let shown = |p: &str| format!("{}\\{p}", self.keys.root.name());
        let clsid = &self.keys.clsid;
        note(shown(clsid), registry::delete_tree(root, clsid));
        // The state key also holds the record of moved-aside folders, which outlives the registration until the
        // folders are gone. So the registration's own values are deleted one by one and the key goes only when no
        // record is in it: the record is never deleted and written back, and is never missing for a moment.
        let state = &self.keys.state;
        for value in ["DllPath", "Protocol", "Build"] {
            note(
                format!("{}\\{value}", shown(state)),
                registry::delete_value(root, state, value),
            );
        }
        if !registry::value_exists(root, state, registry::ASIDE_VALUE) {
            note(shown(state), registry::delete_tree(root, state));
        }
        registry::delete_key_if_empty(root, &self.keys.vendor);
        failed
    }

    fn create_camera(&mut self, dshow: bool) -> Res<()> {
        let cam = create_virtual_camera()?;
        if dshow {
            add_dshow_class(&cam)?;
        }
        unsafe { cam.Start(None) }.map_err(|e| werr("IMFVirtualCamera::Start", e))?;
        let _ = unsafe { cam.Shutdown() };
        Ok(())
    }

    fn remove_camera(&mut self) -> Res<()> {
        let cam = create_virtual_camera()?;
        let r = unsafe { cam.Remove() };
        let _ = unsafe { cam.Shutdown() };
        match r {
            Ok(()) => Ok(()),
            Err(e) if not_found(&e) => Ok(()),
            Err(e) => Err(werr("IMFVirtualCamera::Remove", e)),
        }
    }

    fn camera_listed(&mut self) -> bool {
        crate::camera::is_listed().unwrap_or(true)
    }

    fn stop_frame_server(&mut self) -> bool {
        unsafe {
            let Ok(scm) = OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT) else {
                return false;
            };
            let name = wide("FrameServer");
            let mut stopped = false;
            if let Ok(svc) = OpenServiceW(scm, pcwstr(&name), SERVICE_STOP | SERVICE_QUERY_STATUS) {
                let mut st = SERVICE_STATUS::default();
                let _ = ControlService(svc, SERVICE_CONTROL_STOP, &mut st);
                for _ in 0..50 {
                    if QueryServiceStatus(svc, &mut st).is_ok()
                        && st.dwCurrentState == SERVICE_STOPPED
                    {
                        stopped = true;
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                let _ = CloseServiceHandle(svc);
            }
            let _ = CloseServiceHandle(scm);
            stopped
        }
    }

    fn delete_at_reboot(&mut self, p: &Path) -> Res<()> {
        let w = wide(&p.to_string_lossy());
        unsafe { MoveFileExW(pcwstr(&w), PCWSTR::null(), MOVEFILE_DELAY_UNTIL_REBOOT) }
            .map_err(|e| werr("MoveFileExW", e))
    }
}

impl AsideStore for RealMachine {
    fn aside_record(&mut self) -> Stored {
        registry::read_multi_string(self.keys.root, &self.keys.state, registry::ASIDE_VALUE)
    }

    fn write_aside_record(&mut self, strings: &[String]) -> Res<()> {
        if !strings.is_empty() {
            return registry::set_multi_string(
                self.keys.root,
                &self.keys.state,
                registry::ASIDE_VALUE,
                strings,
            );
        }
        let r = registry::delete_value(self.keys.root, &self.keys.state, registry::ASIDE_VALUE);
        if r != windows::Win32::Foundation::ERROR_SUCCESS && !registry::is_missing(r) {
            return Err(format!(
                "the record of old data folders could not be deleted (error {})",
                r.0
            ));
        }
        // the key holds nothing else when there is no registration: the record was all that kept it
        if registry::read_registered_in(&self.keys).is_none() {
            let _ = registry::delete_tree(self.keys.root, &self.keys.state);
            registry::delete_key_if_empty(self.keys.root, &self.keys.vendor);
        }
        Ok(())
    }
}

/// The real implementation, run against a throwaway key under `HKCU\Software\BetterCam-test-<pid>-<n>` (never
/// `HKLM`): the same code production runs on `HKLM`, only the root and the paths are other.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::aside::{self, AsideEntry, Record};
    use crate::registry::{key_exists, read_registered_in, Root};
    use crate::testkit::ScratchKeys;
    use vcam_protocol::secure::FileIdentity;

    fn machine(k: &ScratchKeys) -> RealMachine {
        assert_eq!(k.keys.root, Root::CurrentUser);
        RealMachine::with_keys(k.keys.clone())
    }

    fn entry(n: usize) -> AsideEntry {
        AsideEntry {
            name: format!("BetterCam.old-{n}-0"),
            identity: FileIdentity {
                volume: 0x1234,
                // never all zero (that is no identity)
                id: [(n % 250) as u8 + 1; 16],
            },
        }
    }

    fn registration() -> Registration {
        Registration {
            dll: r"C:\Program Files\Better Cam\vcam\better_cam_vcam_1.aaaaaaaa.dll".into(),
            build: "aaaaaaaa".into(),
            protocol: vcam_protocol::PROTOCOL_VERSION,
        }
    }

    fn names(m: &mut RealMachine) -> Vec<String> {
        match aside::load(m) {
            Record::Entries { entries, .. } => entries.into_iter().map(|e| e.name).collect(),
            Record::Unreadable(why) => panic!("the record is unreadable: {why}"),
        }
    }

    /// The registration is written, read back, and deleted by the real code.
    #[test]
    fn a_registration_is_written_read_and_deleted() {
        let k = ScratchKeys::new();
        let mut m = machine(&k);
        assert_eq!(m.read_registration(), None);
        m.write_registration(&registration()).unwrap();
        assert_eq!(m.read_registration(), Some(registration()));
        assert!(registry::clsid_key_present_in(&k.keys));
        assert!(m.delete_registration().is_empty());
        assert_eq!(m.read_registration(), None);
        assert!(!registry::clsid_key_present_in(&k.keys));
        // nothing is left: the state key and the key above it are gone
        assert!(!key_exists(k.keys.root, &k.keys.state));
        assert!(!key_exists(k.keys.root, &k.keys.vendor));
        // and deleting what is not there is not a failure
        assert!(m.delete_registration().is_empty());
    }

    /// The record round trips through `write_aside_record` and `aside_record`.
    #[test]
    fn the_record_round_trips_through_the_real_store() {
        let k = ScratchKeys::new();
        let mut m = machine(&k);
        assert_eq!(m.aside_record(), Stored::Missing);
        assert_eq!(names(&mut m), Vec::<String>::new());
        let strings: Vec<String> = (0..3).map(|i| entry(i).encode()).collect();
        m.write_aside_record(&strings).unwrap();
        assert_eq!(m.aside_record(), Stored::Strings(strings));
        assert_eq!(
            names(&mut m),
            [
                "BetterCam.old-0-0",
                "BetterCam.old-1-0",
                "BetterCam.old-2-0"
            ]
        );
        // add and remove go through the same store
        aside::add(&mut m, &entry(5)).unwrap();
        aside::remove(&mut m, "BetterCam.old-1-0").unwrap();
        assert_eq!(
            names(&mut m),
            [
                "BetterCam.old-0-0",
                "BetterCam.old-2-0",
                "BetterCam.old-5-0"
            ]
        );
    }

    /// A record that exists and cannot be read as one is `Unreadable`: another type, a value that is too long, more
    /// strings than the helper ever writes. Nothing is added to it, and it is left exactly as it is.
    #[test]
    fn an_unreadable_record_is_reported_and_never_overwritten() {
        let k = ScratchKeys::new();
        let (root, state) = (k.keys.root, k.keys.state.clone());
        let mut m = machine(&k);
        // another type
        registry::set_string(root, &state, Some(registry::ASIDE_VALUE), "text").unwrap();
        assert!(matches!(m.aside_record(), Stored::Unreadable(_)));
        let err = aside::add(&mut m, &entry(1)).unwrap_err();
        assert!(
            err.contains("cannot be read") && err.contains("not overwritten"),
            "{err}"
        );
        assert!(matches!(
            registry::read_multi_string(root, &state, registry::ASIDE_VALUE),
            Stored::Unreadable(_)
        ));
        // too long
        registry::set_multi_string(root, &state, registry::ASIDE_VALUE, &["x".repeat(70_000)])
            .unwrap();
        let Record::Unreadable(why) = aside::load(&mut m) else {
            panic!("an over-long value was read")
        };
        assert!(why.contains("more than"), "{why}");
        assert!(aside::add(&mut m, &entry(1)).is_err());
        // more strings than the helper writes (a read that succeeds, a record that is refused)
        let many: Vec<String> = (0..=aside::MAX_ENTRIES)
            .map(|i| entry(i).encode())
            .collect();
        assert_eq!(many.len(), 257);
        registry::set_multi_string(root, &state, registry::ASIDE_VALUE, &many).unwrap();
        assert_eq!(m.aside_record(), Stored::Strings(many.clone()));
        let Record::Unreadable(why) = aside::load(&mut m) else {
            panic!("a record of 257 strings was accepted")
        };
        assert!(why.contains("257"), "{why}");
        assert!(aside::add(&mut m, &entry(300)).is_err());
        assert_eq!(
            m.aside_record(),
            Stored::Strings(many),
            "the value was changed"
        );
        // exactly 256 is the most
        let most: Vec<String> = (0..aside::MAX_ENTRIES).map(|i| entry(i).encode()).collect();
        registry::set_multi_string(root, &state, registry::ASIDE_VALUE, &most).unwrap();
        assert!(matches!(
            aside::load(&mut m),
            Record::Entries { entries, .. } if entries.len() == 256
        ));
    }

    /// Deleting the registration removes its values one by one and the class key; the record stays, and so does the
    /// state key that holds it. The record is never missing for a moment (it is not deleted and written back).
    #[test]
    fn deleting_the_registration_keeps_the_record_and_its_key() {
        let k = ScratchKeys::new();
        let mut m = machine(&k);
        m.write_registration(&registration()).unwrap();
        aside::add(&mut m, &entry(1)).unwrap();
        aside::add(&mut m, &entry(2)).unwrap();
        let before = m.aside_record();
        assert!(m.delete_registration().is_empty());
        assert_eq!(m.read_registration(), None, "the registration is gone");
        assert!(
            !registry::clsid_key_present_in(&k.keys),
            "the class key is gone"
        );
        for value in ["DllPath", "Protocol", "Build"] {
            assert!(
                !registry::value_exists(k.keys.root, &k.keys.state, value),
                "{value} is still there"
            );
        }
        assert!(
            key_exists(k.keys.root, &k.keys.state),
            "the state key goes with the record in it"
        );
        assert!(key_exists(k.keys.root, &k.keys.vendor));
        assert_eq!(m.aside_record(), before, "the record changed");
        assert_eq!(names(&mut m), ["BetterCam.old-1-0", "BetterCam.old-2-0"]);
        // a registration written again next to the record leaves it alone too
        m.write_registration(&registration()).unwrap();
        assert_eq!(m.aside_record(), before);
    }

    /// The last name leaving the record removes the value; the state key goes with it only when no registration is
    /// there, and the key above it when it is empty.
    #[test]
    fn the_key_goes_when_nothing_is_left_and_stays_while_a_registration_is_there() {
        // no registration: the record was all that kept the keys
        let k = ScratchKeys::new();
        let mut m = machine(&k);
        aside::add(&mut m, &entry(1)).unwrap();
        assert!(key_exists(k.keys.root, &k.keys.state));
        aside::remove(&mut m, "BetterCam.old-1-0").unwrap();
        assert_eq!(m.aside_record(), Stored::Missing);
        assert!(
            !key_exists(k.keys.root, &k.keys.state),
            "the state key stayed"
        );
        assert!(
            !key_exists(k.keys.root, &k.keys.vendor),
            "the key above stayed"
        );
        // a registration is there: only the value goes
        let k = ScratchKeys::new();
        let mut m = machine(&k);
        m.write_registration(&registration()).unwrap();
        aside::add(&mut m, &entry(1)).unwrap();
        aside::remove(&mut m, "BetterCam.old-1-0").unwrap();
        assert_eq!(m.aside_record(), Stored::Missing);
        assert!(key_exists(k.keys.root, &k.keys.state));
        assert_eq!(
            m.read_registration(),
            Some(registration()),
            "the registration was touched"
        );
        // writing an empty list with nothing there is not an error
        let k = ScratchKeys::new();
        machine(&k).write_aside_record(&[]).unwrap();
    }

    /// A state key that holds only the record is **not installed**: the status read gives no registration, and
    /// the state derived from it is `missing`.
    #[test]
    fn a_status_read_with_only_the_record_left_says_not_installed() {
        use crate::status::{state_from, Facts, VcamState};
        let k = ScratchKeys::new();
        let mut m = machine(&k);
        m.write_registration(&registration()).unwrap();
        aside::add(&mut m, &entry(1)).unwrap();
        let registered = read_registered_in(&k.keys);
        assert!(registered.is_some());
        assert!(m.delete_registration().is_empty());
        assert!(
            key_exists(k.keys.root, &k.keys.state),
            "only the record keeps the key"
        );
        let registered = read_registered_in(&k.keys);
        assert_eq!(registered, None, "a record alone is no registration");
        assert_eq!(m.read_registration(), None);
        let facts = Facts {
            registered,
            clsid_present: registry::clsid_key_present_in(&k.keys),
            dll_present: false,
            listed: true,
            shipped_build: Some("aaaaaaaa".into()),
            shipped_protocol: vcam_protocol::PROTOCOL_VERSION,
            data_problem: None,
        };
        assert_eq!(state_from(&facts), VcamState::Missing);
    }
}
