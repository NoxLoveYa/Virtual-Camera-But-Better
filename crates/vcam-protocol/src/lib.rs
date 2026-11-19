//! Everything the virtual camera DLL (`vcam-source`), `vcam-tool` and the app share.
//!
//! * Names, paths and sizes (this file).
//! * The frame transport of plan 2.5: [`header`] (layout), [`seqlock`] (atomic access and the seqlock),
//!   [`validate`] (every field is checked before use), and on Windows [`map`] (file mapping, QPC, [`Writer`]).
//! * [`signal`] (Windows): the frame event that wakes the camera service's pump when the writer has something new.
//! * [`pattern`] (checkerboard + stripe test pattern) and [`slate`] (RLE slates), used by `vcam-tool` and the tests.

pub mod header;
#[cfg(windows)]
pub mod map;
pub mod pattern;
#[cfg(windows)]
pub mod secure;
pub mod seqlock;
#[cfg(windows)]
pub mod signal;
pub mod slate;
pub mod validate;

pub use header::{
    advertised_for_output, default_advertised, off, round_fps, slot_off, wake_mode, Format,
    RawHeader, SlotMeta, WriterState, ADVERTISED_MAX, FEATURE_FRAME_EVENT, FILE_SIZE, HEADER_SIZE,
    MAGIC, MAX_HEIGHT, MAX_WIDTH, NO_SLOT, SLOT_CAPACITY, SLOT_COUNT, SUBTYPE_NV12, SUBTYPE_RGB32,
};
#[cfg(windows)]
pub use map::{
    data_dir as frames_dir, frames_path, hns_to_qpc, qpc_frequency, qpc_now, qpc_to_hns,
    read_advertised, read_advertised_pinned, Access, BackingFile, FrameEventStatus, MappedFile,
    Writer, WriterOptions,
};
pub use seqlock::{Disposition, PublishError, Read, ReadError, Shared, SlotData, SlotFormat};
pub use validate::{
    is_stale, validate_format, validate_header, validate_slot, validate_wake, HeaderError,
    SlotError, ValidHeader, WakeAdvert,
};

/// CLSID of the Better Cam media source, in registry form.
pub const CLSID_STRING: &str = "{3C0EE391-6A3A-4D0E-A4E5-8243EA287A41}";
/// The same CLSID as a number: data1 in the top 32 bits, as `windows::core::GUID::from_u128` expects.
pub const CLSID_U128: u128 = 0x3C0EE391_6A3A_4D0E_A4E5_8243EA287A41;

/// Friendly name handed to `MFCreateVirtualCamera`. The pipeline may append " Windows Virtual Camera".
pub const CAMERA_NAME: &str = "Better Cam";
/// Default value of `HKLM\SOFTWARE\Classes\CLSID\{CLSID}`.
pub const CLSID_DESCRIPTION: &str = "Better Cam Media Source";

/// Bumped when the DLL and the app can no longer talk to each other.
pub const PROTOCOL_VERSION: u32 = 1;

/// File name stem of the DLL as built (`better_cam_vcam.dll`).
pub const DLL_BASE_NAME: &str = "better_cam_vcam";

/// Folder under `%ProgramFiles%` (`Better Cam\vcam`) that holds the versioned DLL copies.
pub const INSTALL_DIR_COMPANY: &str = "Better Cam";
pub const INSTALL_DIR_VCAM: &str = "vcam";

/// Folder under `%ProgramData%`.
pub const DATA_DIR_NAME: &str = "BetterCam";
/// Sub-folder of [`DATA_DIR_NAME`] for logs.
pub const LOG_DIR_NAME: &str = "logs";

/// `HKLM` key (without the leading hive) that holds the media source COM registration.
pub fn clsid_key() -> String {
    format!(r"SOFTWARE\Classes\CLSID\{CLSID_STRING}")
}
/// `HKLM` key that records the installed DLL (`DllPath`, `Protocol`, `Build`).
pub const VCAM_STATE_KEY: &str = r"SOFTWARE\BetterCam\Vcam";

/// Name of the versioned DLL copy: `better_cam_vcam_<protocol>.<build>.dll`.
pub fn versioned_dll_name(build: &str) -> String {
    format!("{DLL_BASE_NAME}_{PROTOCOL_VERSION}.{build}.dll")
}

/// Frame rate of every advertised media type.
pub const FPS: u32 = 30;

/// Frame sizes advertised by P1, preferred first.
pub const SIZES: [(u32, u32); 2] = [(1920, 1080), (1280, 720)];

/// File name of the shared frame file inside the data folder.
pub const FRAMES_FILE_NAME: &str = "frames.bin";

/// The scratch folder of one kind of test, `%TEMP%\bettercam-<kind>-<pid>`. It is made on first use and removed, with
/// everything in it, when the test process exits: a frame file is 37 MB and a run of the tests used to leave about a
/// gigabyte behind. The tests of a kind share the folder and use names of their own.
#[cfg(all(test, windows))]
pub(crate) fn test_dir(kind: &str) -> std::path::PathBuf {
    use std::sync::Mutex;
    static MADE: Mutex<Vec<std::path::PathBuf>> = Mutex::new(Vec::new());
    extern "C" fn remove_all() {
        let made = MADE.lock().unwrap_or_else(|e| e.into_inner());
        for dir in made.iter() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
    extern "C" {
        fn atexit(callback: extern "C" fn()) -> i32;
    }
    let dir = std::env::temp_dir().join(format!("bettercam-{kind}-{}", std::process::id()));
    let mut made = MADE.lock().unwrap_or_else(|e| e.into_inner());
    if !made.contains(&dir) {
        if made.is_empty() {
            // SAFETY: `remove_all` is a plain `extern "C"` function that does not unwind
            unsafe { atexit(remove_all) };
        }
        made.push(dir.clone());
    }
    dir
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clsid_string_matches_number() {
        let n = CLSID_U128;
        let s = format!(
            "{{{:08X}-{:04X}-{:04X}-{:04X}-{:012X}}}",
            (n >> 96) as u32,
            (n >> 80) as u16,
            (n >> 64) as u16,
            (n >> 48) as u16,
            n & 0xFFFF_FFFF_FFFF
        );
        assert_eq!(s, CLSID_STRING);
    }

    #[test]
    fn versioned_name() {
        assert_eq!(
            versioned_dll_name("abcd1234"),
            "better_cam_vcam_1.abcd1234.dll"
        );
    }

    #[test]
    fn default_sizes_are_valid_formats() {
        for &(w, h) in &SIZES {
            assert!(validate_format(w, h, SUBTYPE_NV12).is_ok());
            assert!(validate_format(w, h, SUBTYPE_RGB32).is_ok());
        }
    }
}
