//! Install, update, repair, remove and inspect the Better Cam virtual camera.
//!
//! One implementation for both programs that do it: `better-cam.exe` (`--vcam-install` / `--vcam-uninstall`, started
//! by the app through [`elevate`]) and `vcam-tool register / unregister / status`.
//!
//! * [`install`] / [`uninstall`]: the elevated work (registry, Program Files copy, data folder permissions, the
//!   camera). The steps run against a [`machine::Machine`] and a set of [`util::Paths`], which tests replace.
//! * [`acl`] and [`datadir`]: the permissions of `%ProgramData%\BetterCam` and how the helper prepares it without
//!   following a link a user planted.
//! * [`status`]: what any process can read without elevation.
//! * [`elevate`]: `runas` launcher and the outcome of a launch.

pub mod acl;
pub mod aside;
pub mod camera;
pub mod datadir;
pub mod elevate;
pub mod install;
pub mod lock;
pub mod log;
pub mod machine;
pub mod registry;
pub mod status;
#[cfg(test)]
mod testkit;
pub mod util;

pub use camera::{enum_directshow_names, enum_mf_devices, is_listed, MfDevice};
pub use elevate::{current_user_sid, is_elevated, LaunchOutcome, Launcher, ShellLauncher};
pub use install::{
    file_looks_like_dll, install, looks_like_dll, uninstall, InstallOptions, InstallOutcome,
    UninstallOutcome,
};
pub use log::{FileLog, Log};
pub use status::{consumers, gather_facts, state_from, Facts, VcamState};
pub use util::{init_com_and_mf, MfSession};
