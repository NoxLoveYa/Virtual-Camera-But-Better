//! Status without elevation: what is registered, whether Windows lists the camera, and whether it is older than the DLL
//! this app ships. The decision is a pure function of [`Facts`], so it is tested with injected facts.

use std::path::Path;

use crate::registry::{self, Registered};
use crate::util::{dll_build, MfSession};

/// The three states of plan 5.5.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VcamState {
    /// Not registered, its DLL is gone, or Windows does not list the camera.
    Missing,
    /// Registered and listed, but another protocol or build than the DLL shipped with the app.
    Outdated,
    Ok,
}

impl VcamState {
    pub fn as_str(self) -> &'static str {
        match self {
            VcamState::Missing => "missing",
            VcamState::Outdated => "outdated",
            VcamState::Ok => "ok",
        }
    }
}

/// What is known about the machine and the app's own DLL.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Facts {
    /// The `HKLM\SOFTWARE\BetterCam\Vcam` state key, if present.
    pub registered: Option<Registered>,
    /// The COM class key is present.
    pub clsid_present: bool,
    /// The DLL the state key names exists on disk.
    pub dll_present: bool,
    /// Media Foundation lists a device named "Better Cam ...".
    pub listed: bool,
    /// Build tag of the DLL that ships with this app; `None` when the app has no DLL (nothing to compare with).
    pub shipped_build: Option<String>,
    /// Protocol version this app was built with.
    pub shipped_protocol: u32,
    /// A plain sentence when the data folder or the frame file is encrypted (EFS): the camera service cannot read
    /// it, so the camera shows nothing whatever else is right. `None` when it is fine or not there yet. It does not
    /// change [`state_from`]; the app's status (`VcamStatus::problem`) carries it next to the state, and the window
    /// shows it with a Repair button, unless Repair cannot help ([`crate::datadir::DataProblem::repairable`]).
    pub data_problem: Option<crate::datadir::DataProblem>,
}

/// [`VcamState`] for `facts`.
pub fn state_from(facts: &Facts) -> VcamState {
    let Some(reg) = &facts.registered else {
        return VcamState::Missing;
    };
    if !facts.clsid_present || !facts.dll_present || !facts.listed {
        return VcamState::Missing;
    }
    if reg.protocol != Some(facts.shipped_protocol) {
        return VcamState::Outdated;
    }
    match (&facts.shipped_build, &reg.build) {
        (Some(shipped), Some(have)) if shipped == have => VcamState::Ok,
        (Some(_), _) => VcamState::Outdated,
        (None, _) => VcamState::Ok,
    }
}

/// Read the facts from the machine. `shipped_dll` is the DLL that would be installed.
pub fn gather_facts(shipped_dll: Option<&Path>) -> Facts {
    let registered = registry::read_registered();
    let dll_present = registered
        .as_ref()
        .and_then(|r| r.dll_path.as_deref())
        .map(|p| Path::new(p).is_file())
        .unwrap_or(false);
    let listed = MfSession::start()
        .ok()
        .and_then(|_session| crate::camera::is_listed().ok())
        .unwrap_or(false);
    Facts {
        registered,
        clsid_present: registry::clsid_key_present(),
        dll_present,
        listed,
        shipped_build: shipped_dll.and_then(|p| dll_build(p).ok()),
        shipped_protocol: vcam_protocol::PROTOCOL_VERSION,
        data_problem: crate::datadir::encryption_problem(&crate::util::Paths::system()),
    }
}

/// Number of consumers (0 or 1) that the frame file's header reports: an active stream with a heartbeat under a
/// second old. 0 when the file is missing or invalid. A plain read, no mapping, so it never holds the file.
pub fn consumers() -> u32 {
    let Ok((bytes, len)) = vcam_protocol::map::read_header_bytes(&vcam_protocol::frames_path())
    else {
        return 0;
    };
    let Some(raw) = vcam_protocol::RawHeader::from_bytes(&bytes) else {
        return 0;
    };
    if vcam_protocol::validate_header(&raw, len, None, None).is_err() {
        return 0;
    }
    let fresh = vcam_protocol::qpc_now().saturating_sub(raw.consumer_heartbeat)
        <= vcam_protocol::qpc_frequency();
    u32::from(raw.consumer_active != 0 && fresh)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn healthy() -> Facts {
        Facts {
            registered: Some(Registered {
                dll_path: Some(
                    r"C:\Program Files\Better Cam\vcam\better_cam_vcam_1.aaaaaaaa.dll".into(),
                ),
                protocol: Some(1),
                build: Some("aaaaaaaa".into()),
            }),
            clsid_present: true,
            dll_present: true,
            listed: true,
            shipped_build: Some("aaaaaaaa".into()),
            shipped_protocol: 1,
            data_problem: None,
        }
    }

    #[test]
    fn all_fine_is_ok() {
        assert_eq!(state_from(&healthy()), VcamState::Ok);
    }

    #[test]
    fn not_registered_is_missing() {
        let f = Facts {
            registered: None,
            clsid_present: false,
            dll_present: false,
            listed: false,
            ..healthy()
        };
        assert_eq!(state_from(&f), VcamState::Missing);
        // a camera that is listed without our keys is still missing (somebody else's, or half removed)
        assert_eq!(
            state_from(&Facts {
                registered: None,
                ..healthy()
            }),
            VcamState::Missing
        );
    }

    #[test]
    fn registered_but_not_listed_is_missing() {
        assert_eq!(
            state_from(&Facts {
                listed: false,
                ..healthy()
            }),
            VcamState::Missing
        );
    }

    #[test]
    fn a_registration_whose_files_are_gone_is_missing() {
        assert_eq!(
            state_from(&Facts {
                dll_present: false,
                ..healthy()
            }),
            VcamState::Missing
        );
        assert_eq!(
            state_from(&Facts {
                clsid_present: false,
                ..healthy()
            }),
            VcamState::Missing
        );
    }

    #[test]
    fn another_protocol_is_outdated() {
        assert_eq!(
            state_from(&Facts {
                shipped_protocol: 2,
                ..healthy()
            }),
            VcamState::Outdated
        );
        let mut f = healthy();
        f.registered.as_mut().unwrap().protocol = None;
        assert_eq!(state_from(&f), VcamState::Outdated);
    }

    #[test]
    fn another_build_is_outdated() {
        assert_eq!(
            state_from(&Facts {
                shipped_build: Some("bbbbbbbb".into()),
                ..healthy()
            }),
            VcamState::Outdated
        );
        let mut f = healthy();
        f.registered.as_mut().unwrap().build = None;
        assert_eq!(state_from(&f), VcamState::Outdated);
    }

    #[test]
    fn without_a_shipped_dll_a_working_camera_stays_ok() {
        assert_eq!(
            state_from(&Facts {
                shipped_build: None,
                ..healthy()
            }),
            VcamState::Ok
        );
    }

    #[test]
    fn states_have_the_names_of_the_command() {
        assert_eq!(VcamState::Missing.as_str(), "missing");
        assert_eq!(VcamState::Outdated.as_str(), "outdated");
        assert_eq!(VcamState::Ok.as_str(), "ok");
    }
}
