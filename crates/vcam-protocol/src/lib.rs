//! Constants shared by the virtual camera DLL (`vcam-source`), `vcam-tool` and, in later phases, the app.
//!
//! P1 holds only names, paths and the test-pattern geometry. Frame transport (header, seqlock, validation)
//! arrives in P2.

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

/// Geometry of the P1 test pattern. The renderer lives in `vcam-source`, the detector in `vcam-tool`; both use
/// these numbers.
///
/// Background: seven scrolling colour bars, none of them white. Foreground: a white square whose left edge
/// encodes the frame number: `x = (frame % slots) * BOX_STEP`.
pub mod pattern {
    /// Side of the white box in pixels.
    pub const BOX_SIZE: u32 = 64;
    /// Horizontal move per frame in pixels.
    pub const BOX_STEP: u32 = 16;
    /// Background scroll per frame in pixels.
    pub const BAR_SCROLL: u32 = 8;
    /// Number of colour bars across the frame.
    pub const BAR_COUNT: u32 = 7;
    /// Luma (limited range) of the white box; every bar is darker than [`DETECT_LUMA`].
    pub const WHITE_Y: u8 = 235;
    /// Luma threshold the detector uses to find the box.
    pub const DETECT_LUMA: u8 = 228;

    /// Number of distinct box positions for a frame of this width.
    pub fn slots(width: u32) -> u32 {
        (width.saturating_sub(BOX_SIZE)) / BOX_STEP
    }
    /// Left edge of the box for frame number `frame`.
    pub fn box_x(width: u32, frame: u64) -> u32 {
        (frame % slots(width) as u64) as u32 * BOX_STEP
    }
    /// Top edge of the box (even, so it is aligned to the chroma grid).
    pub fn box_y(height: u32) -> u32 {
        (height / 4) & !1
    }
    /// Slot (`frame % slots`) a measured left edge belongs to; tolerates a few pixels of error.
    pub fn slot_from_box_x(x: u32) -> u32 {
        (x + BOX_STEP / 2) / BOX_STEP
    }
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
    fn box_stays_inside_frame_and_round_trips() {
        for &(w, _) in &super::SIZES {
            let slots = pattern::slots(w);
            assert!(slots > 10);
            for f in 0..(slots as u64 * 3) {
                let x = pattern::box_x(w, f);
                assert!(x + pattern::BOX_SIZE <= w);
                assert_eq!(pattern::slot_from_box_x(x) as u64, f % slots as u64);
                // a couple of pixels of error still decode to the same slot
                assert_eq!(pattern::slot_from_box_x(x + 3) as u64, f % slots as u64);
            }
        }
    }
}
