//! Device controls backend (plan 4.1 "Device controls", 6.2): the "Configure Video" properties of a camera.
//!
//! * [`DeviceControls::new`] / [`DeviceControls::from_reader`] attach to the media source of an open camera.
//! * [`DeviceControls::read_all`] lists the controls the device supports with their own range, default, value and
//!   Auto state ([`ControlInfo`]). Controls the device lacks are not listed.
//! * [`DeviceControls::set_value`] and [`DeviceControls::set_auto`] write them.
//! * [`ControlSnapshot`] holds the values and Auto flags a user changed for one [`DeviceKey`]; it is `serde`
//!   serialisable. [`DeviceControls::apply_snapshot`] re-applies it after the device is reopened: Auto flags first,
//!   then manual values, clamped to the device's current range. Neither Media Foundation nor DirectShow keeps
//!   control values across a reconnect, so the caller stores the snapshot and applies it after every open.
//!
//! # Access path
//! `IKsControl::KsProperty` first (`PROPSETID_VIDCAP_VIDEOPROCAMP` / `PROPSETID_VIDCAP_CAMERACONTROL`):
//! `BASICSUPPORT` gives min / max / step and the access flags, `DEFAULTVALUES` the default, `GET` / `SET` the value
//! and flags (`AUTO`, `MANUAL`). When a device rejects the KS property, `IAMVideoProcAmp` / `IAMCameraControl`
//! serve the ten ProcAmp and seven CameraControl properties instead ([`AccessPath::Iam`]). Powerline frequency
//! and low light compensation exist only as KS properties.
//!
//! # Threading
//! [`DeviceControls`] is `Send + Sync`; every call takes an internal lock, so calls from several threads are
//! serialised (a write blocks for the duration of one USB control transfer, a few milliseconds). Each public
//! method makes sure the calling thread has COM initialised. The object holds references on the camera's media
//! source: drop it before or together with the [`CaptureReader`] it came from. After `CaptureReader::close` every
//! call fails with a device error.
//!
//! This module only reads and writes what it is told: its "Auto" is the device's own Auto flag. Who decides the values
//! is the layer above (`camera_controls`): the user, the camera's Auto, or **Better Cam auto**, the one case where
//! software computes values (owner decision 2026-11-18, plan 9.3 item 1, `docs/AUTO-CONTROLS.md`). Better Cam may write
//! exposure, gain and white balance, and only for a control the user switched to Better Cam auto; every other control
//! is written only on the user's own change.

use std::fmt;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use windows::core::{Error, Interface, GUID, HRESULT};
use windows::Win32::Foundation::{E_NOINTERFACE, E_NOTIMPL};
use windows::Win32::Media::DirectShow::{IAMCameraControl, IAMVideoProcAmp};
use windows::Win32::Media::KernelStreaming::{
    IKsControl, KSIDENTIFIER, KSPROPERTY_CAMERACONTROL_FLAGS_AUTO,
    KSPROPERTY_CAMERACONTROL_FLAGS_MANUAL, KSPROPERTY_MEMBER_FLAG_DEFAULT,
    KSPROPERTY_MEMBER_RANGES, KSPROPERTY_MEMBER_STEPPEDRANGES, KSPROPERTY_MEMBER_VALUES,
    KSPROPERTY_TYPE_BASICSUPPORT, KSPROPERTY_TYPE_DEFAULTVALUES, KSPROPERTY_TYPE_GET,
    KSPROPERTY_TYPE_SET, PROPSETID_VIDCAP_CAMERACONTROL, PROPSETID_VIDCAP_VIDEOPROCAMP,
};
use windows::Win32::Media::MediaFoundation::{IMFMediaSource, MF_E_SHUTDOWN};

use super::reader::CaptureReader;
use super::{
    classify_hresult, ensure_com, hr_text, Agile, CaptureError, DeviceKey, ErrorClass, MfSession,
};

// ---------------------------------------------------------------------------------------------------------------
// Keys

/// One control of plan 6.2, named with the plan's key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlKey {
    Brightness,
    Contrast,
    Hue,
    Saturation,
    Sharpness,
    Gamma,
    ColorEnable,
    WhiteBalance,
    BacklightCompensation,
    Gain,
    PowerlineFrequency,
    Pan,
    Tilt,
    Roll,
    Zoom,
    Exposure,
    Iris,
    Focus,
    LowLightCompensation,
}

/// The two KS property sets controls live in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KsSet {
    /// `PROPSETID_VIDCAP_VIDEOPROCAMP`
    VideoProcAmp,
    /// `PROPSETID_VIDCAP_CAMERACONTROL`
    CameraControl,
}

impl KsSet {
    pub fn guid(self) -> GUID {
        match self {
            KsSet::VideoProcAmp => PROPSETID_VIDCAP_VIDEOPROCAMP,
            KsSet::CameraControl => PROPSETID_VIDCAP_CAMERACONTROL,
        }
    }
}

/// How the UI should show a control (plan 6.2 "Notes").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlKind {
    Slider,
    /// 0 / 1.
    Switch,
    /// Powerline frequency: Disabled 0, 50 Hz 1, 60 Hz 2, Auto 3 (UVC 1.5; the device's range says which exist).
    Select,
}

impl ControlKey {
    /// Every control, ProcAmp first, in the order of plan 6.2.
    pub const ALL: [ControlKey; 19] = [
        ControlKey::Brightness,
        ControlKey::Contrast,
        ControlKey::Hue,
        ControlKey::Saturation,
        ControlKey::Sharpness,
        ControlKey::Gamma,
        ControlKey::ColorEnable,
        ControlKey::WhiteBalance,
        ControlKey::BacklightCompensation,
        ControlKey::Gain,
        ControlKey::PowerlineFrequency,
        ControlKey::Pan,
        ControlKey::Tilt,
        ControlKey::Roll,
        ControlKey::Zoom,
        ControlKey::Exposure,
        ControlKey::Iris,
        ControlKey::Focus,
        ControlKey::LowLightCompensation,
    ];

    /// Plan 6.2 key (also the serde name).
    pub fn key(self) -> &'static str {
        match self {
            ControlKey::Brightness => "brightness",
            ControlKey::Contrast => "contrast",
            ControlKey::Hue => "hue",
            ControlKey::Saturation => "saturation",
            ControlKey::Sharpness => "sharpness",
            ControlKey::Gamma => "gamma",
            ControlKey::ColorEnable => "color_enable",
            ControlKey::WhiteBalance => "white_balance",
            ControlKey::BacklightCompensation => "backlight_compensation",
            ControlKey::Gain => "gain",
            ControlKey::PowerlineFrequency => "powerline_frequency",
            ControlKey::Pan => "pan",
            ControlKey::Tilt => "tilt",
            ControlKey::Roll => "roll",
            ControlKey::Zoom => "zoom",
            ControlKey::Exposure => "exposure",
            ControlKey::Iris => "iris",
            ControlKey::Focus => "focus",
            ControlKey::LowLightCompensation => "low_light_compensation",
        }
    }

    pub fn from_key(s: &str) -> Option<ControlKey> {
        let s = s.to_ascii_lowercase().replace('-', "_");
        ControlKey::ALL.into_iter().find(|k| k.key() == s)
    }

    /// Plan 6.2 label.
    pub fn label(self) -> &'static str {
        match self {
            ControlKey::Brightness => "Brightness",
            ControlKey::Contrast => "Contrast",
            ControlKey::Hue => "Hue",
            ControlKey::Saturation => "Saturation",
            ControlKey::Sharpness => "Sharpness",
            ControlKey::Gamma => "Gamma",
            ControlKey::ColorEnable => "Color",
            ControlKey::WhiteBalance => "White balance",
            ControlKey::BacklightCompensation => "Backlight compensation",
            ControlKey::Gain => "Gain",
            ControlKey::PowerlineFrequency => "Powerline frequency",
            ControlKey::Pan => "Pan",
            ControlKey::Tilt => "Tilt",
            ControlKey::Roll => "Roll",
            ControlKey::Zoom => "Zoom",
            ControlKey::Exposure => "Exposure",
            ControlKey::Iris => "Iris",
            ControlKey::Focus => "Focus",
            ControlKey::LowLightCompensation => "Low light compensation",
        }
    }

    pub fn kind(self) -> ControlKind {
        match self {
            ControlKey::ColorEnable | ControlKey::LowLightCompensation => ControlKind::Switch,
            ControlKey::PowerlineFrequency => ControlKind::Select,
            _ => ControlKind::Slider,
        }
    }

    /// KS property set and property id, from `ksmedia.h` (Windows SDK 10.0.26100):
    /// `KSPROPERTY_VIDCAP_VIDEOPROCAMP` 0..9 and 13, `KSPROPERTY_VIDCAP_CAMERACONTROL` 0..6 and 19.
    pub const fn ks_property(self) -> (KsSet, u32) {
        match self {
            ControlKey::Brightness => (KsSet::VideoProcAmp, 0),
            ControlKey::Contrast => (KsSet::VideoProcAmp, 1),
            ControlKey::Hue => (KsSet::VideoProcAmp, 2),
            ControlKey::Saturation => (KsSet::VideoProcAmp, 3),
            ControlKey::Sharpness => (KsSet::VideoProcAmp, 4),
            ControlKey::Gamma => (KsSet::VideoProcAmp, 5),
            ControlKey::ColorEnable => (KsSet::VideoProcAmp, 6),
            ControlKey::WhiteBalance => (KsSet::VideoProcAmp, 7),
            ControlKey::BacklightCompensation => (KsSet::VideoProcAmp, 8),
            ControlKey::Gain => (KsSet::VideoProcAmp, 9),
            ControlKey::PowerlineFrequency => (KsSet::VideoProcAmp, 13),
            ControlKey::Pan => (KsSet::CameraControl, 0),
            ControlKey::Tilt => (KsSet::CameraControl, 1),
            ControlKey::Roll => (KsSet::CameraControl, 2),
            ControlKey::Zoom => (KsSet::CameraControl, 3),
            ControlKey::Exposure => (KsSet::CameraControl, 4),
            ControlKey::Iris => (KsSet::CameraControl, 5),
            ControlKey::Focus => (KsSet::CameraControl, 6),
            ControlKey::LowLightCompensation => (KsSet::CameraControl, 19),
        }
    }

    /// The `IAMVideoProcAmp` / `IAMCameraControl` property id (the same numbers as KS ids 0..9 and 0..6);
    /// `None` for the two controls that exist only as KS properties.
    pub const fn iam_property(self) -> Option<(KsSet, i32)> {
        match self {
            ControlKey::PowerlineFrequency | ControlKey::LowLightCompensation => None,
            other => {
                let (set, id) = other.ks_property();
                Some((set, id as i32))
            }
        }
    }
}

impl fmt::Display for ControlKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.key())
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Values

/// Which interface served a control.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AccessPath {
    /// `IKsControl::KsProperty`
    #[serde(rename = "KS")]
    Ks,
    /// `IAMVideoProcAmp` / `IAMCameraControl`
    #[serde(rename = "IAM")]
    Iam,
}

impl fmt::Display for AccessPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            AccessPath::Ks => "KS",
            AccessPath::Iam => "IAM",
        })
    }
}

/// What the device reports about one supported control.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlInfo {
    pub key: ControlKey,
    pub min: i32,
    pub max: i32,
    pub step: i32,
    pub default: i32,
    /// Current value. While Auto is on this is what the device currently uses.
    pub value: i32,
    /// The device can run this control automatically.
    pub can_auto: bool,
    /// The device accepts manual values.
    pub can_manual: bool,
    /// The device's Auto flag is on now.
    pub auto: bool,
    /// Interface that served the read (and serves writes).
    pub path: AccessPath,
}

impl ControlInfo {
    /// `value` limited to `[min, max]`.
    pub fn clamp(&self, value: i32) -> i32 {
        value.clamp(self.min.min(self.max), self.max.max(self.min))
    }
}

/// Why a control call failed.
#[derive(Clone, Debug)]
pub enum ControlError {
    /// The device does not have this control.
    NotSupported(ControlKey),
    /// The control has no Auto mode.
    AutoNotSupported(ControlKey),
    /// The control cannot be set manually (Auto only or read only).
    ManualNotSupported(ControlKey),
    /// The device rejected the call, was lost, or is shut down.
    Capture(CaptureError),
}

impl fmt::Display for ControlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ControlError::NotSupported(k) => write!(f, "{k}: not supported by this camera"),
            ControlError::AutoNotSupported(k) => write!(f, "{k}: Auto mode not supported"),
            ControlError::ManualNotSupported(k) => write!(f, "{k}: manual values not supported"),
            ControlError::Capture(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ControlError {}

impl From<CaptureError> for ControlError {
    fn from(e: CaptureError) -> Self {
        ControlError::Capture(e)
    }
}

// ---------------------------------------------------------------------------------------------------------------
// KS wire format

/// `KSPROPERTY_VIDEOPROCAMP_S` and `KSPROPERTY_CAMERACONTROL_S` are the same layout.
#[repr(C)]
#[derive(Clone, Copy)]
struct KsValue {
    property: KSIDENTIFIER,
    value: i32,
    flags: u32,
    capabilities: u32,
}

const KS_AUTO: u32 = KSPROPERTY_CAMERACONTROL_FLAGS_AUTO as u32;
const KS_MANUAL: u32 = KSPROPERTY_CAMERACONTROL_FLAGS_MANUAL as u32;

/// Byte offsets inside a `BASICSUPPORT` / `DEFAULTVALUES` answer: `KSPROPERTY_DESCRIPTION` (40 bytes), then
/// `KSPROPERTY_MEMBERSHEADER` (16 bytes) and its members.
const DESC_LEN: usize = 40;
const OFF_MEMBERS_LIST_COUNT: usize = 32;
const MEMBERSHEADER_LEN: usize = 16;

/// What `BASICSUPPORT` and `DEFAULTVALUES` told us.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct KsSupport {
    access: u32,
    range: Option<(i32, i32, i32)>,
    default: Option<i32>,
}

fn le_u32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn le_i32(b: &[u8], at: usize) -> Option<i32> {
    le_u32(b, at).map(|v| v as i32)
}

/// Parse the answer of `KSPROPERTY_TYPE_BASICSUPPORT` or `KSPROPERTY_TYPE_DEFAULTVALUES`: a description, then
/// `MembersListCount` member lists. A range member (`RANGES`, `STEPPEDRANGES`) is a `KSPROPERTY_STEPPING_LONG`
/// (delta, reserved, min, max); a `VALUES` member flagged `DEFAULT` carries the default as a LONG.
fn parse_support(buf: &[u8]) -> Option<KsSupport> {
    let mut out = KsSupport {
        access: le_u32(buf, 0)?,
        ..KsSupport::default()
    };
    let lists = le_u32(buf, OFF_MEMBERS_LIST_COUNT)?;
    let mut at = DESC_LEN;
    for _ in 0..lists {
        let kind = le_u32(buf, at)?;
        let size = le_u32(buf, at + 4)? as usize;
        let count = le_u32(buf, at + 8)? as usize;
        let flags = le_u32(buf, at + 12)?;
        let body = at + MEMBERSHEADER_LEN;
        if kind == KSPROPERTY_MEMBER_RANGES || kind == KSPROPERTY_MEMBER_STEPPEDRANGES {
            if size >= 16 && count >= 1 && out.range.is_none() {
                let step = le_u32(buf, body)? as i32;
                out.range = Some((le_i32(buf, body + 8)?, le_i32(buf, body + 12)?, step.max(1)));
            }
        } else if kind == KSPROPERTY_MEMBER_VALUES
            && flags & KSPROPERTY_MEMBER_FLAG_DEFAULT != 0
            && size >= 4
            && count >= 1
            && out.default.is_none()
        {
            out.default = Some(le_i32(buf, body)?);
        }
        at = body.checked_add(size.checked_mul(count)?)?;
    }
    Some(out)
}

// ---------------------------------------------------------------------------------------------------------------
// Raw device state

/// One read of a control from one interface.
#[derive(Clone, Copy, Debug)]
struct Raw {
    min: i32,
    max: i32,
    step: i32,
    default: i32,
    value: i32,
    flags: u32,
    caps: u32,
    writable: bool,
    path: AccessPath,
}

impl Raw {
    fn can_auto(&self) -> bool {
        self.caps & KS_AUTO != 0
    }

    /// A control that reports no capability bits at all (the KS powerline frequency does) takes plain manual
    /// writes when its access flags allow `SET`.
    fn can_manual(&self) -> bool {
        self.writable && (self.caps & KS_MANUAL != 0 || self.caps == 0)
    }

    /// The flags word a manual write carries.
    fn manual_flags(&self) -> u32 {
        if self.caps == 0 {
            0
        } else {
            KS_MANUAL
        }
    }

    fn info(&self, key: ControlKey) -> ControlInfo {
        ControlInfo {
            key,
            min: self.min,
            max: self.max,
            step: self.step.max(1),
            default: self.default,
            value: self.value,
            can_auto: self.can_auto(),
            can_manual: self.can_manual(),
            auto: self.flags & KS_AUTO != 0,
            path: self.path,
        }
    }
}

/// The errors that mean "this call is for a device that is gone", as opposed to "this control is absent".
fn is_fatal(hr: HRESULT) -> bool {
    classify_hresult(hr) != ErrorClass::Other || hr == MF_E_SHUTDOWN
}

fn api(what: &'static str, e: Error) -> ControlError {
    ControlError::Capture(CaptureError::from_call(what, e))
}

// ---------------------------------------------------------------------------------------------------------------
// The device

/// Controls of one open camera.
pub struct DeviceControls {
    ks: Option<Agile<IKsControl>>,
    procamp: Option<Agile<IAMVideoProcAmp>>,
    camera: Option<Agile<IAMCameraControl>>,
    lock: Mutex<()>,
    /// 0: KS first with IAM as fallback (normal); 1: KS only; 2: IAM only. Diagnostics, see `force_path`.
    forced: AtomicU8,
    // Last: Media Foundation stays started while the interfaces above exist.
    _mf: MfSession,
}

impl DeviceControls {
    /// Attach to a camera's media source (`CaptureReader::media_source`). Fails when the source offers none of
    /// `IKsControl`, `IAMVideoProcAmp` and `IAMCameraControl`.
    pub fn new(source: &IMFMediaSource) -> Result<DeviceControls, CaptureError> {
        let mf = MfSession::start()?;
        let ks = source.cast::<IKsControl>().ok().map(Agile);
        let procamp = source.cast::<IAMVideoProcAmp>().ok().map(Agile);
        let camera = source.cast::<IAMCameraControl>().ok().map(Agile);
        if ks.is_none() && procamp.is_none() && camera.is_none() {
            return Err(CaptureError::Api {
                what: "QueryInterface(IKsControl, IAMVideoProcAmp, IAMCameraControl)",
                error: Error::from(E_NOINTERFACE),
            });
        }
        Ok(DeviceControls {
            ks,
            procamp,
            camera,
            lock: Mutex::new(()),
            forced: AtomicU8::new(0),
            _mf: mf,
        })
    }

    /// Diagnostics: serve every read and write through one interface only (`None`: KS first, IAM as the
    /// fallback, which is what the app uses). Lets a probe prove that the IAM fallback works on a device whose
    /// KS path is fine.
    pub fn force_path(&self, path: Option<AccessPath>) {
        self.forced.store(
            match path {
                None => 0,
                Some(AccessPath::Ks) => 1,
                Some(AccessPath::Iam) => 2,
            },
            Ordering::Relaxed,
        );
    }

    fn forced(&self) -> Option<AccessPath> {
        match self.forced.load(Ordering::Relaxed) {
            1 => Some(AccessPath::Ks),
            2 => Some(AccessPath::Iam),
            _ => None,
        }
    }

    pub fn from_reader(reader: &CaptureReader) -> Result<DeviceControls, CaptureError> {
        DeviceControls::new(reader.media_source())
    }

    fn guard(&self) -> std::sync::MutexGuard<'_, ()> {
        ensure_com();
        self.lock.lock().unwrap_or_else(|e| e.into_inner())
    }

    // --- KS ---------------------------------------------------------------------------------------------------

    fn ks_query(&self, key: ControlKey, kind: u32) -> windows::core::Result<Vec<u8>> {
        let ks = self.ks.as_ref().ok_or_else(|| Error::from(E_NOINTERFACE))?;
        let (set, id) = key.ks_property();
        let mut prop = KSIDENTIFIER::default();
        prop.Anonymous.Anonymous.Set = set.guid();
        prop.Anonymous.Anonymous.Id = id;
        prop.Anonymous.Anonymous.Flags = kind;
        let mut buf = vec![0u8; 256];
        let mut returned = 0u32;
        // SAFETY: `prop` and `buf` outlive the call; lengths are those of the buffers.
        unsafe {
            ks.0.KsProperty(
                &prop,
                std::mem::size_of::<KSIDENTIFIER>() as u32,
                buf.as_mut_ptr().cast(),
                buf.len() as u32,
                &mut returned,
            )?;
        }
        buf.truncate((returned as usize).min(256));
        Ok(buf)
    }

    fn ks_value(
        &self,
        key: ControlKey,
        kind: u32,
        value: i32,
        flags: u32,
    ) -> windows::core::Result<KsValue> {
        let ks = self.ks.as_ref().ok_or_else(|| Error::from(E_NOINTERFACE))?;
        let (set, id) = key.ks_property();
        let mut s = KsValue {
            property: KSIDENTIFIER::default(),
            value,
            flags,
            capabilities: 0,
        };
        s.property.Anonymous.Anonymous.Set = set.guid();
        s.property.Anonymous.Anonymous.Id = id;
        s.property.Anonymous.Anonymous.Flags = kind;
        let size = std::mem::size_of::<KsValue>() as u32;
        let p = &mut s as *mut KsValue;
        let mut returned = 0u32;
        // SAFETY: the property header is the first field, so `p` serves as the KSPROPERTY and the data buffer at
        // once, as `KSPROPERTY_*_S` calls require.
        unsafe {
            ks.0.KsProperty(
                p.cast::<KSIDENTIFIER>(),
                size,
                p.cast(),
                size,
                &mut returned,
            )?;
        }
        Ok(s)
    }

    fn read_ks(&self, key: ControlKey) -> windows::core::Result<Raw> {
        let basic = parse_support(&self.ks_query(key, KSPROPERTY_TYPE_BASICSUPPORT)?)
            .ok_or_else(|| Error::from(E_NOTIMPL))?;
        let (min, max, step) = basic.range.ok_or_else(|| Error::from(E_NOTIMPL))?;
        let default = self
            .ks_query(key, KSPROPERTY_TYPE_DEFAULTVALUES)
            .ok()
            .and_then(|b| parse_support(&b))
            .and_then(|s| s.default);
        let cur = self.ks_value(key, KSPROPERTY_TYPE_GET, 0, 0)?;
        Ok(Raw {
            min,
            max,
            step,
            default: default.unwrap_or(cur.value),
            value: cur.value,
            flags: cur.flags,
            caps: cur.capabilities,
            writable: basic.access & KSPROPERTY_TYPE_SET != 0,
            path: AccessPath::Ks,
        })
    }

    // --- IAM --------------------------------------------------------------------------------------------------

    fn read_iam(&self, key: ControlKey) -> windows::core::Result<Raw> {
        let (set, id) = key.iam_property().ok_or_else(|| Error::from(E_NOTIMPL))?;
        let (mut min, mut max, mut step, mut default, mut caps) = (0, 0, 0, 0, 0);
        let (mut value, mut flags) = (0, 0);
        // SAFETY: out pointers are locals.
        unsafe {
            match set {
                KsSet::VideoProcAmp => {
                    let p = self
                        .procamp
                        .as_ref()
                        .ok_or_else(|| Error::from(E_NOINTERFACE))?;
                    p.0.GetRange(id, &mut min, &mut max, &mut step, &mut default, &mut caps)?;
                    p.0.Get(id, &mut value, &mut flags)?;
                }
                KsSet::CameraControl => {
                    let c = self
                        .camera
                        .as_ref()
                        .ok_or_else(|| Error::from(E_NOINTERFACE))?;
                    c.0.GetRange(id, &mut min, &mut max, &mut step, &mut default, &mut caps)?;
                    c.0.Get(id, &mut value, &mut flags)?;
                }
            }
        }
        Ok(Raw {
            min,
            max,
            step,
            default,
            value,
            flags: flags as u32,
            caps: caps as u32,
            writable: true,
            path: AccessPath::Iam,
        })
    }

    // --- Shared -----------------------------------------------------------------------------------------------

    /// Read through `only` when given, else KS first and IAM as the fallback. `Ok(None)`: the device has no such
    /// control. `Err` only when the device is gone.
    fn read_raw(
        &self,
        key: ControlKey,
        only: Option<AccessPath>,
    ) -> Result<Option<Raw>, ControlError> {
        let only = only.or_else(|| self.forced());
        let mut tries = Vec::new();
        if only != Some(AccessPath::Iam) {
            tries.push(AccessPath::Ks);
        }
        if only != Some(AccessPath::Ks) {
            tries.push(AccessPath::Iam);
        }
        for path in tries {
            let r = match path {
                AccessPath::Ks => self.read_ks(key),
                AccessPath::Iam => self.read_iam(key),
            };
            match r {
                Ok(raw) if raw.max >= raw.min => return Ok(Some(raw)),
                Ok(_) => {}
                Err(e) if is_fatal(e.code()) => {
                    return Err(api("read control", e));
                }
                Err(_) => {}
            }
        }
        Ok(None)
    }

    fn write_raw(
        &self,
        key: ControlKey,
        raw: &Raw,
        value: i32,
        flags: u32,
    ) -> Result<(), ControlError> {
        let via_iam = |this: &Self| -> windows::core::Result<()> {
            let (set, id) = key.iam_property().ok_or_else(|| Error::from(E_NOTIMPL))?;
            // SAFETY: plain COM calls.
            unsafe {
                match set {
                    KsSet::VideoProcAmp => this
                        .procamp
                        .as_ref()
                        .ok_or_else(|| Error::from(E_NOINTERFACE))?
                        .0
                        .Set(id, value, flags as i32),
                    KsSet::CameraControl => this
                        .camera
                        .as_ref()
                        .ok_or_else(|| Error::from(E_NOINTERFACE))?
                        .0
                        .Set(id, value, flags as i32),
                }
            }
        };
        match raw.path {
            AccessPath::Ks => match self.ks_value(key, KSPROPERTY_TYPE_SET, value, flags) {
                Ok(_) => Ok(()),
                Err(e) if is_fatal(e.code()) => Err(api("KsProperty(SET)", e)),
                Err(e) => match via_iam(self) {
                    Ok(()) => Ok(()),
                    Err(_) => Err(api("KsProperty(SET)", e)),
                },
            },
            AccessPath::Iam => via_iam(self).map_err(|e| api("IAM Set", e)),
        }
    }

    // --- Public API -------------------------------------------------------------------------------------------

    /// Read one control. `Ok(None)`: the device does not have it.
    pub fn read(&self, key: ControlKey) -> Result<Option<ControlInfo>, ControlError> {
        let _g = self.guard();
        Ok(self.read_raw(key, None)?.map(|r| r.info(key)))
    }

    /// Read one control through one interface only (diagnostics: compare KS with the IAM fallback).
    pub fn read_via(
        &self,
        key: ControlKey,
        path: AccessPath,
    ) -> Result<Option<ControlInfo>, ControlError> {
        let _g = self.guard();
        Ok(self.read_raw(key, Some(path))?.map(|r| r.info(key)))
    }

    /// Every control the device supports, in [`ControlKey::ALL`] order.
    pub fn read_all(&self) -> Result<Vec<ControlInfo>, ControlError> {
        self.read_all_via(None)
    }

    /// [`read_all`](Self::read_all) through one interface only.
    pub fn read_all_via(&self, only: Option<AccessPath>) -> Result<Vec<ControlInfo>, ControlError> {
        let _g = self.guard();
        let mut out = Vec::new();
        for key in ControlKey::ALL {
            if let Some(raw) = self.read_raw(key, only)? {
                out.push(raw.info(key));
            }
        }
        Ok(out)
    }

    /// Set a manual value (clamped to the device range) and switch the control to manual. Returns the control as
    /// the device reports it afterwards.
    pub fn set_value(&self, key: ControlKey, value: i32) -> Result<ControlInfo, ControlError> {
        let _g = self.guard();
        let raw = self
            .read_raw(key, None)?
            .ok_or(ControlError::NotSupported(key))?;
        let info = raw.info(key);
        if !raw.can_manual() {
            return Err(ControlError::ManualNotSupported(key));
        }
        self.write_raw(key, &raw, info.clamp(value), raw.manual_flags())?;
        Ok(self
            .read_raw(key, Some(raw.path))?
            .map_or(info, |r| r.info(key)))
    }

    /// Turn the device's Auto flag on or off. Turning it off keeps the value the device uses now. A control
    /// without Auto mode accepts `false` (it is always manual) and refuses `true`.
    pub fn set_auto(&self, key: ControlKey, auto: bool) -> Result<ControlInfo, ControlError> {
        let _g = self.guard();
        let raw = self
            .read_raw(key, None)?
            .ok_or(ControlError::NotSupported(key))?;
        let info = raw.info(key);
        if info.auto == auto {
            return Ok(info);
        }
        if auto {
            if !raw.can_auto() {
                return Err(ControlError::AutoNotSupported(key));
            }
            self.write_raw(key, &raw, raw.value, KS_AUTO)?;
        } else {
            if !raw.can_manual() {
                return Err(ControlError::ManualNotSupported(key));
            }
            self.write_raw(key, &raw, raw.value, raw.manual_flags())?;
        }
        Ok(self
            .read_raw(key, Some(raw.path))?
            .map_or(info, |r| r.info(key)))
    }

    /// Re-apply a stored snapshot (see [`apply_snapshot`]).
    pub fn apply_snapshot(&self, snapshot: &ControlSnapshot) -> Result<ApplyReport, ControlError> {
        apply_snapshot(self, snapshot)
    }
}

/// The operations [`apply_snapshot`] needs; [`DeviceControls`] implements them. A seam for tests.
pub trait ControlDevice {
    fn read(&self, key: ControlKey) -> Result<Option<ControlInfo>, ControlError>;
    fn set_auto(&self, key: ControlKey, auto: bool) -> Result<ControlInfo, ControlError>;
    fn set_value(&self, key: ControlKey, value: i32) -> Result<ControlInfo, ControlError>;
}

impl ControlDevice for DeviceControls {
    fn read(&self, key: ControlKey) -> Result<Option<ControlInfo>, ControlError> {
        DeviceControls::read(self, key)
    }

    fn set_auto(&self, key: ControlKey, auto: bool) -> Result<ControlInfo, ControlError> {
        DeviceControls::set_auto(self, key, auto)
    }

    fn set_value(&self, key: ControlKey, value: i32) -> Result<ControlInfo, ControlError> {
        DeviceControls::set_value(self, key, value)
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Persistence

/// What the user changed on one control.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredControl {
    pub key: ControlKey,
    /// The Auto flag to restore (`None`: the user never touched it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto: Option<bool>,
    /// The manual value to restore (`None`: the user never set one).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<i32>,
    /// Better Cam auto drives this control (docs/AUTO-CONTROLS.md 5.2): `value` is the resume point, written as a
    /// manual value on open, and `auto` is `Some(false)`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub app: bool,
}

/// The values and Auto flags a user changed on one device, to be re-applied after every open. Storage on disk
/// belongs to a later phase; this type only has to serialise.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ControlSnapshot {
    pub device: DeviceKey,
    pub controls: Vec<StoredControl>,
    /// Better Cam auto settings of this camera (not written to the device).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto: Option<crate::auto_controls::AutoCfg>,
    /// What Better Cam auto learned about this camera (not written to the device).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub learned: Option<crate::auto_controls::Learned>,
}

impl ControlSnapshot {
    pub fn new(device: DeviceKey) -> ControlSnapshot {
        ControlSnapshot {
            device,
            controls: Vec::new(),
            auto: None,
            learned: None,
        }
    }

    /// A snapshot of the full device state: Auto flag for every control that has Auto, and the value of every
    /// control that is manual now.
    pub fn from_infos(device: DeviceKey, infos: &[ControlInfo]) -> ControlSnapshot {
        let mut s = ControlSnapshot::new(device);
        for i in infos {
            if i.can_auto {
                s.record_auto(i.key, i.auto);
            }
            if !i.auto {
                s.record_value(i.key, i.value);
            }
        }
        s
    }

    pub fn get(&self, key: ControlKey) -> Option<&StoredControl> {
        self.controls.iter().find(|c| c.key == key)
    }

    fn entry(&mut self, key: ControlKey) -> &mut StoredControl {
        let pos = match self.controls.iter().position(|c| c.key == key) {
            Some(p) => p,
            None => {
                self.controls.push(StoredControl {
                    key,
                    auto: None,
                    value: None,
                    app: false,
                });
                self.controls.len() - 1
            }
        };
        &mut self.controls[pos]
    }

    /// The user flipped Auto. A control that was in Better Cam auto leaves it.
    pub fn record_auto(&mut self, key: ControlKey, auto: bool) {
        let e = self.entry(key);
        e.auto = Some(auto);
        e.app = false;
    }

    /// The user set a manual value. Setting a value takes a control out of Auto (and out of Better Cam auto).
    pub fn record_value(&mut self, key: ControlKey, value: i32) {
        let e = self.entry(key);
        e.value = Some(value);
        e.app = false;
        if e.auto == Some(true) {
            e.auto = Some(false);
        }
    }

    /// Better Cam auto drives this control; `value` is its resume point (written as a manual value on open).
    pub fn record_app(&mut self, key: ControlKey, value: i32) {
        let e = self.entry(key);
        e.auto = Some(false);
        e.value = Some(value);
        e.app = true;
    }

    /// Whether Better Cam auto drives this control.
    pub fn is_app(&self, key: ControlKey) -> bool {
        self.get(key).is_some_and(|c| c.app)
    }

    /// Forget a control (reset to the device's own behaviour).
    pub fn forget(&mut self, key: ControlKey) {
        self.controls.retain(|c| c.key != key);
    }

    pub fn is_empty(&self) -> bool {
        self.controls.is_empty()
    }
}

/// A control [`apply_snapshot`] wrote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppliedControl {
    pub key: ControlKey,
    /// The Auto flag written.
    pub auto: Option<bool>,
    /// The manual value written (after clamping).
    pub value: Option<i32>,
    /// The stored value lay outside the device's current range; this is what it was.
    pub clamped_from: Option<i32>,
}

/// Why a stored control was not written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SkipReason {
    /// The device has no such control now.
    Unsupported,
    AutoNotSupported,
    ManualNotSupported,
    /// The device rejected the write; the message is the error text.
    Failed(String),
}

impl fmt::Display for SkipReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SkipReason::Unsupported => f.write_str("not supported"),
            SkipReason::AutoNotSupported => f.write_str("Auto not supported"),
            SkipReason::ManualNotSupported => f.write_str("manual not supported"),
            SkipReason::Failed(m) => write!(f, "failed: {m}"),
        }
    }
}

/// What [`apply_snapshot`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ApplyReport {
    pub applied: Vec<AppliedControl>,
    pub skipped: Vec<(ControlKey, SkipReason)>,
}

/// Whether a control error says the camera itself is gone or shut down (the engine's reconnect path takes over),
/// not just that this control refused.
pub fn device_gone(e: &ControlError) -> bool {
    matches!(e, ControlError::Capture(c) if c.hresult().is_some_and(is_fatal))
}

fn skip_reason(e: ControlError) -> Result<SkipReason, ControlError> {
    match e {
        ControlError::NotSupported(_) => Ok(SkipReason::Unsupported),
        ControlError::AutoNotSupported(_) => Ok(SkipReason::AutoNotSupported),
        ControlError::ManualNotSupported(_) => Ok(SkipReason::ManualNotSupported),
        ControlError::Capture(c) => match c.hresult() {
            Some(hr) if is_fatal(hr) => Err(ControlError::Capture(c)),
            _ => Ok(SkipReason::Failed(c.to_string())),
        },
    }
}

/// Re-apply `snapshot` to `device`, in the fixed order of plan 4.1: first every stored Auto flag, then every
/// stored manual value (a control stored as Auto keeps its Auto mode; its manual value is not written). Values
/// are clamped to the device's current range. Controls the device lacks, or rejects, are skipped and reported.
/// `Err` only when the device itself is gone.
pub fn apply_snapshot(
    device: &dyn ControlDevice,
    snapshot: &ControlSnapshot,
) -> Result<ApplyReport, ControlError> {
    let mut report = ApplyReport::default();
    fn skipped(report: &mut ApplyReport, key: ControlKey, reason: SkipReason) {
        if !report.skipped.iter().any(|(k, _)| *k == key) {
            report.skipped.push((key, reason));
        }
    }
    let ordered: Vec<&StoredControl> = ControlKey::ALL
        .iter()
        .filter_map(|k| snapshot.get(*k))
        .collect();

    // 1. Auto flags.
    for c in &ordered {
        let Some(auto) = c.auto else { continue };
        match device.set_auto(c.key, auto) {
            Ok(_) => report.applied.push(AppliedControl {
                key: c.key,
                auto: Some(auto),
                value: None,
                clamped_from: None,
            }),
            Err(e) => skipped(&mut report, c.key, skip_reason(e)?),
        }
    }

    // 2. Manual values.
    for c in &ordered {
        let Some(value) = c.value else { continue };
        if c.auto == Some(true) || report.skipped.iter().any(|(k, _)| *k == c.key) {
            continue;
        }
        let info = match device.read(c.key) {
            Ok(Some(i)) => i,
            Ok(None) => {
                skipped(&mut report, c.key, SkipReason::Unsupported);
                continue;
            }
            Err(e) => {
                skipped(&mut report, c.key, skip_reason(e)?);
                continue;
            }
        };
        let clamped = info.clamp(value);
        match device.set_value(c.key, clamped) {
            Ok(_) => {
                let entry = AppliedControl {
                    key: c.key,
                    auto: None,
                    value: Some(clamped),
                    clamped_from: (clamped != value).then_some(value),
                };
                match report.applied.iter_mut().find(|a| a.key == c.key) {
                    Some(a) => {
                        a.value = entry.value;
                        a.clamped_from = entry.clamped_from;
                    }
                    None => report.applied.push(entry),
                }
            }
            Err(e) => skipped(&mut report, c.key, skip_reason(e)?),
        }
    }
    Ok(report)
}

/// `0xC00D3704`-style text of a control error's HRESULT, when it has one (diagnostics).
pub fn error_code_text(e: &ControlError) -> Option<String> {
    match e {
        ControlError::Capture(c) => c.hresult().map(hr_text),
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use windows::Win32::Media::DirectShow as ds;
    use windows::Win32::Media::KernelStreaming as ks;

    fn words(w: &[u32]) -> Vec<u8> {
        w.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    // Answers captured from W4GS (vid_0c45&pid_636d) through IKsControl.
    fn brightness_basic() -> Vec<u8> {
        words(&[
            3, 0x48, 0x97e99ba0, 0x11cfbdea, 0xdb28d6a5, 0x0000c104, 0, 0, 1, 0, 1, 0x10, 1, 0, 1,
            0, 0xffffffc0, 0x40,
        ])
    }

    fn brightness_default() -> Vec<u8> {
        words(&[
            3, 0x3c, 0x97e99ba0, 0x11cfbdea, 0xdb28d6a5, 0x0000c104, 3, 0, 1, 0, 3, 4, 1, 1, 0,
        ])
    }

    fn exposure_basic() -> Vec<u8> {
        words(&[
            3, 0x48, 0x97e99ba0, 0x11cfbdea, 0xdb28d6a5, 0x0000c104, 4, 0, 1, 0, 1, 0x10, 1, 0, 1,
            0, 0xfffffff3, 0xffffffff,
        ])
    }

    fn exposure_default() -> Vec<u8> {
        words(&[
            3, 0x3c, 0x97e99ba0, 0x11cfbdea, 0xdb28d6a5, 0x0000c104, 4, 0, 1, 0, 3, 4, 1, 1,
            0xfffffffa,
        ])
    }

    #[test]
    fn ks_ids_match_ksmedia_h() {
        // Literal numbers from the enum order in ksmedia.h (Windows SDK 10.0.26100).
        let procamp: [(ControlKey, u32); 11] = [
            (ControlKey::Brightness, 0),
            (ControlKey::Contrast, 1),
            (ControlKey::Hue, 2),
            (ControlKey::Saturation, 3),
            (ControlKey::Sharpness, 4),
            (ControlKey::Gamma, 5),
            (ControlKey::ColorEnable, 6),
            (ControlKey::WhiteBalance, 7),
            (ControlKey::BacklightCompensation, 8),
            (ControlKey::Gain, 9),
            (ControlKey::PowerlineFrequency, 13),
        ];
        for (k, id) in procamp {
            assert_eq!(k.ks_property(), (KsSet::VideoProcAmp, id), "{k}");
        }
        let camera: [(ControlKey, u32); 8] = [
            (ControlKey::Pan, 0),
            (ControlKey::Tilt, 1),
            (ControlKey::Roll, 2),
            (ControlKey::Zoom, 3),
            (ControlKey::Exposure, 4),
            (ControlKey::Iris, 5),
            (ControlKey::Focus, 6),
            (ControlKey::LowLightCompensation, 19),
        ];
        for (k, id) in camera {
            assert_eq!(k.ks_property(), (KsSet::CameraControl, id), "{k}");
        }
        // And against the constants the windows crate generates from the SDK metadata.
        assert_eq!(ks::KSPROPERTY_VIDEOPROCAMP_BRIGHTNESS.0 as u32, 0);
        assert_eq!(
            ks::KSPROPERTY_VIDEOPROCAMP_GAIN.0 as u32,
            ControlKey::Gain.ks_property().1
        );
        assert_eq!(
            ks::KSPROPERTY_VIDEOPROCAMP_POWERLINE_FREQUENCY.0 as u32,
            ControlKey::PowerlineFrequency.ks_property().1
        );
        assert_eq!(
            ks::KSPROPERTY_VIDEOPROCAMP_WHITEBALANCE.0 as u32,
            ControlKey::WhiteBalance.ks_property().1
        );
        assert_eq!(
            ks::KSPROPERTY_CAMERACONTROL_AUTO_EXPOSURE_PRIORITY.0 as u32,
            ControlKey::LowLightCompensation.ks_property().1
        );
        assert_eq!(ks::KSPROPERTY_CAMERACONTROL_AUTO_EXPOSURE_PRIORITY.0, 19);
        assert_eq!(
            ks::KSPROPERTY_CAMERACONTROL_FOCUS.0 as u32,
            ControlKey::Focus.ks_property().1
        );
        // Property set GUIDs.
        assert_eq!(
            KsSet::VideoProcAmp.guid(),
            GUID::from_u128(0xc6e13360_30ac_11d0_a18c_00a0c9118956)
        );
        assert_eq!(
            KsSet::CameraControl.guid(),
            GUID::from_u128(0xc6e13370_30ac_11d0_a18c_00a0c9118956)
        );
        // Flags and request types.
        assert_eq!((KS_AUTO, KS_MANUAL), (1, 2));
        assert_eq!(ks::KSPROPERTY_VIDEOPROCAMP_FLAGS_AUTO, 1);
        assert_eq!(ks::KSPROPERTY_VIDEOPROCAMP_FLAGS_MANUAL, 2);
        assert_eq!(KSPROPERTY_TYPE_GET, 1);
        assert_eq!(KSPROPERTY_TYPE_SET, 2);
        assert_eq!(KSPROPERTY_TYPE_BASICSUPPORT, 0x200);
        assert_eq!(KSPROPERTY_TYPE_DEFAULTVALUES, 0x1_0000);
    }

    #[test]
    fn iam_ids_equal_the_ks_ids_and_exclude_the_ks_only_controls() {
        for k in ControlKey::ALL {
            match k.iam_property() {
                None => assert!(
                    matches!(
                        k,
                        ControlKey::PowerlineFrequency | ControlKey::LowLightCompensation
                    ),
                    "{k}"
                ),
                Some((set, id)) => assert_eq!((set, id as u32), k.ks_property(), "{k}"),
            }
        }
        assert_eq!(ds::VideoProcAmp_Brightness.0, 0);
        assert_eq!(ds::VideoProcAmp_Gain.0, 9);
        assert_eq!(ds::VideoProcAmp_WhiteBalance.0, 7);
        assert_eq!(ds::CameraControl_Exposure.0, 4);
        assert_eq!(ds::CameraControl_Focus.0, 6);
        assert_eq!(ds::CameraControl_Pan.0, 0);
        assert_eq!(ds::CameraControl_Zoom.0, 3);
    }

    #[test]
    fn wire_struct_matches_the_sdk_structs() {
        assert_eq!(
            std::mem::size_of::<KsValue>(),
            std::mem::size_of::<ks::KSPROPERTY_VIDEOPROCAMP_S>()
        );
        assert_eq!(
            std::mem::size_of::<KsValue>(),
            std::mem::size_of::<ks::KSPROPERTY_CAMERACONTROL_S>()
        );
        assert_eq!(std::mem::size_of::<KsValue>(), 40);
    }

    #[test]
    fn keys_names_labels_and_serde_agree() {
        for k in ControlKey::ALL {
            assert_eq!(ControlKey::from_key(k.key()), Some(k));
            assert_eq!(
                serde_json::to_string(&k).unwrap(),
                format!("\"{}\"", k.key())
            );
            assert!(!k.label().is_empty());
        }
        assert_eq!(
            ControlKey::from_key("White-Balance"),
            Some(ControlKey::WhiteBalance)
        );
        assert_eq!(ControlKey::from_key("nope"), None);
        assert_eq!(ControlKey::ColorEnable.kind(), ControlKind::Switch);
        assert_eq!(ControlKey::PowerlineFrequency.kind(), ControlKind::Select);
        assert_eq!(ControlKey::Gain.kind(), ControlKind::Slider);
        let mut names: Vec<_> = ControlKey::ALL.iter().map(|k| k.key()).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), 19);
    }

    #[test]
    fn parses_the_range_and_default_answers_of_a_real_device() {
        let b = parse_support(&brightness_basic()).unwrap();
        assert_eq!(b.access, 3);
        assert_eq!(b.range, Some((-64, 64, 1)));
        assert_eq!(b.default, None);
        let d = parse_support(&brightness_default()).unwrap();
        assert_eq!(d.default, Some(0));
        assert_eq!(d.range, None);

        let e = parse_support(&exposure_basic()).unwrap();
        assert_eq!(e.range, Some((-13, -1, 1)));
        assert_eq!(
            parse_support(&exposure_default()).unwrap().default,
            Some(-6)
        );
    }

    #[test]
    fn short_or_garbled_answers_do_not_panic() {
        assert_eq!(parse_support(&[]), None);
        assert_eq!(parse_support(&[1, 0, 0, 0]), None);
        let full = brightness_basic();
        for n in 0..full.len() {
            let _ = parse_support(&full[..n]);
        }
        // A member count that points past the buffer.
        let mut huge = brightness_basic();
        huge[48..52].copy_from_slice(&u32::MAX.to_le_bytes());
        let _ = parse_support(&huge);
        // A list count that points past the buffer.
        let mut many = brightness_basic();
        many[32..36].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(parse_support(&many), None);
        // A zero step becomes 1.
        let mut zero = brightness_basic();
        zero[56..60].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(parse_support(&zero).unwrap().range, Some((-64, 64, 1)));
    }

    fn info(key: ControlKey, min: i32, max: i32, value: i32) -> ControlInfo {
        ControlInfo {
            key,
            min,
            max,
            step: 1,
            default: min,
            value,
            can_auto: false,
            can_manual: true,
            auto: false,
            path: AccessPath::Ks,
        }
    }

    #[test]
    fn clamp_limits_to_the_device_range() {
        let i = info(ControlKey::Brightness, -64, 64, 0);
        assert_eq!(i.clamp(100), 64);
        assert_eq!(i.clamp(-100), -64);
        assert_eq!(i.clamp(7), 7);
        let backwards = info(ControlKey::Brightness, 5, -5, 0);
        assert_eq!(backwards.clamp(9), 5);
    }

    #[test]
    fn raw_flags_map_to_the_info() {
        let raw = Raw {
            min: 0,
            max: 2,
            step: 1,
            default: 1,
            value: 1,
            flags: 0,
            caps: 0,
            writable: true,
            path: AccessPath::Ks,
        };
        let i = raw.info(ControlKey::PowerlineFrequency);
        assert!(!i.can_auto && i.can_manual && !i.auto);
        assert_eq!(raw.manual_flags(), 0, "powerline writes carry no flag");
        let raw = Raw {
            caps: 3,
            flags: 1,
            ..raw
        };
        let i = raw.info(ControlKey::Exposure);
        assert!(i.can_auto && i.can_manual && i.auto);
        assert_eq!(raw.manual_flags(), KS_MANUAL);
        let read_only = Raw {
            caps: 0,
            writable: false,
            ..raw
        };
        assert!(!read_only.can_manual());
        // Auto-only controls refuse manual values.
        let auto_only = Raw { caps: 1, ..raw };
        assert!(auto_only.can_auto() && !auto_only.can_manual());
    }

    #[test]
    fn snapshot_round_trips_through_json() {
        let key = DeviceKey::new(
            "\\\\?\\usb#vid_0c45&pid_636d&mi_00#9&3171a958&0&0000#{e5323777-f976-4f5b-9b55-b94699c46e44}\\global",
            "W4GS",
        );
        let mut s = ControlSnapshot::new(key.clone());
        s.record_auto(ControlKey::Exposure, true);
        s.record_auto(ControlKey::WhiteBalance, false);
        s.record_value(ControlKey::WhiteBalance, 5000);
        s.record_value(ControlKey::Brightness, -12);
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"white_balance\""));
        assert!(!json.contains("null"), "unset fields are omitted: {json}");
        let back: ControlSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
        assert_eq!(back.device, key);
        assert_eq!(back.get(ControlKey::Brightness).unwrap().value, Some(-12));
        assert_eq!(back.get(ControlKey::Exposure).unwrap().auto, Some(true));
        assert_eq!(back.get(ControlKey::Exposure).unwrap().value, None);
    }

    #[test]
    fn recording_a_value_takes_a_control_out_of_auto_and_forget_removes_it() {
        let mut s = ControlSnapshot::new(DeviceKey::new("x", "y"));
        s.record_auto(ControlKey::Focus, true);
        s.record_value(ControlKey::Focus, 300);
        assert_eq!(
            s.get(ControlKey::Focus),
            Some(&StoredControl {
                key: ControlKey::Focus,
                auto: Some(false),
                value: Some(300),
                app: false,
            })
        );
        s.record_value(ControlKey::Gain, 3);
        assert_eq!(s.get(ControlKey::Gain).unwrap().auto, None);
        s.forget(ControlKey::Focus);
        assert!(s.get(ControlKey::Focus).is_none());
        assert_eq!(s.controls.len(), 1);
    }

    #[test]
    fn a_control_in_better_cam_auto_is_a_manual_value_that_serialises_its_mode() {
        let mut s = ControlSnapshot::new(DeviceKey::new("x", "y"));
        s.record_app(ControlKey::Exposure, -5);
        s.record_value(ControlKey::Brightness, 3);
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"app\":true"), "{json}");
        assert_eq!(json.matches("\"app\"").count(), 1, "only the driven control says so: {json}");
        let back: ControlSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
        // the device is told a plain manual value: Auto flag off, then the resume point
        let c = s.get(ControlKey::Exposure).unwrap();
        assert_eq!((c.auto, c.value, c.app), (Some(false), Some(-5), true));
        assert!(s.is_app(ControlKey::Exposure) && !s.is_app(ControlKey::Brightness));
        // an older save has no such field
        let old: StoredControl = serde_json::from_str(r#"{"key":"gain","value":4}"#).unwrap();
        assert!(!old.app);
        // the user's own write or Auto flag ends it
        s.record_value(ControlKey::Exposure, -3);
        assert!(!s.is_app(ControlKey::Exposure));
        s.record_app(ControlKey::Exposure, -5);
        s.record_auto(ControlKey::Exposure, true);
        assert!(!s.is_app(ControlKey::Exposure));
    }

    #[test]
    fn from_infos_keeps_auto_flags_and_manual_values_only() {
        let mut exposure = info(ControlKey::Exposure, -13, -1, -7);
        exposure.can_auto = true;
        exposure.auto = true;
        let mut wb = info(ControlKey::WhiteBalance, 2800, 6500, 4600);
        wb.can_auto = true;
        let contrast = info(ControlKey::Contrast, 0, 64, 32);
        let s = ControlSnapshot::from_infos(DeviceKey::new("x", "y"), &[exposure, wb, contrast]);
        assert_eq!(
            s.get(ControlKey::Exposure).unwrap().value,
            None,
            "auto: value is the device's"
        );
        assert_eq!(s.get(ControlKey::Exposure).unwrap().auto, Some(true));
        assert_eq!(s.get(ControlKey::WhiteBalance).unwrap().auto, Some(false));
        assert_eq!(s.get(ControlKey::WhiteBalance).unwrap().value, Some(4600));
        assert_eq!(s.get(ControlKey::Contrast).unwrap().auto, None);
        assert_eq!(s.get(ControlKey::Contrast).unwrap().value, Some(32));
    }

    /// A fake camera that logs every write.
    struct Mock {
        controls: RefCell<HashMap<ControlKey, ControlInfo>>,
        log: RefCell<Vec<String>>,
        reject: Option<ControlKey>,
    }

    impl Mock {
        fn new(infos: Vec<ControlInfo>) -> Mock {
            Mock {
                controls: RefCell::new(infos.into_iter().map(|i| (i.key, i)).collect()),
                log: RefCell::new(Vec::new()),
                reject: None,
            }
        }
    }

    impl ControlDevice for Mock {
        fn read(&self, key: ControlKey) -> Result<Option<ControlInfo>, ControlError> {
            Ok(self.controls.borrow().get(&key).cloned())
        }

        fn set_auto(&self, key: ControlKey, auto: bool) -> Result<ControlInfo, ControlError> {
            let mut m = self.controls.borrow_mut();
            let c = m.get_mut(&key).ok_or(ControlError::NotSupported(key))?;
            if auto && !c.can_auto {
                return Err(ControlError::AutoNotSupported(key));
            }
            if Some(key) == self.reject {
                return Err(ControlError::Capture(CaptureError::Api {
                    what: "mock",
                    error: Error::from(windows::Win32::Foundation::E_FAIL),
                }));
            }
            self.log.borrow_mut().push(format!("auto {key} {auto}"));
            c.auto = auto;
            Ok(c.clone())
        }

        fn set_value(&self, key: ControlKey, value: i32) -> Result<ControlInfo, ControlError> {
            let mut m = self.controls.borrow_mut();
            let c = m.get_mut(&key).ok_or(ControlError::NotSupported(key))?;
            self.log.borrow_mut().push(format!("value {key} {value}"));
            c.value = c.clamp(value);
            c.auto = false;
            Ok(c.clone())
        }
    }

    fn camera() -> Mock {
        let mut exposure = info(ControlKey::Exposure, -13, -1, -7);
        exposure.can_auto = true;
        exposure.auto = true;
        let mut wb = info(ControlKey::WhiteBalance, 2800, 6500, 4600);
        wb.can_auto = true;
        wb.auto = true;
        Mock::new(vec![
            info(ControlKey::Brightness, -64, 64, 0),
            info(ControlKey::Contrast, 0, 64, 32),
            wb,
            exposure,
        ])
    }

    #[test]
    fn apply_writes_every_auto_flag_before_any_value() {
        let cam = camera();
        let mut s = ControlSnapshot::new(DeviceKey::new("x", "y"));
        // Recorded in an order that is neither the canonical nor the phase order.
        s.record_value(ControlKey::Contrast, 40);
        s.record_value(ControlKey::Exposure, -4);
        s.record_auto(ControlKey::Exposure, false);
        s.record_auto(ControlKey::WhiteBalance, true);
        s.record_value(ControlKey::Brightness, 10);
        let report = apply_snapshot(&cam, &s).unwrap();
        assert_eq!(
            *cam.log.borrow(),
            vec![
                // phase 1: flags, canonical order (white balance before exposure)
                "auto white_balance true",
                "auto exposure false",
                // phase 2: values, canonical order
                "value brightness 10",
                "value contrast 40",
                "value exposure -4",
            ]
        );
        assert!(report.skipped.is_empty());
        let c = cam.controls.borrow();
        assert!(
            c[&ControlKey::WhiteBalance].auto,
            "a stored Auto stays Auto"
        );
        assert!(!c[&ControlKey::Exposure].auto);
        assert_eq!(c[&ControlKey::Exposure].value, -4);
    }

    #[test]
    fn apply_clamps_to_the_current_range_and_reports_it() {
        let cam = camera();
        let mut s = ControlSnapshot::new(DeviceKey::new("x", "y"));
        s.record_value(ControlKey::Brightness, 500);
        s.record_value(ControlKey::Contrast, -9);
        let report = apply_snapshot(&cam, &s).unwrap();
        assert_eq!(
            *cam.log.borrow(),
            vec!["value brightness 64", "value contrast 0"]
        );
        assert_eq!(report.applied.len(), 2);
        assert_eq!(report.applied[0].value, Some(64));
        assert_eq!(report.applied[0].clamped_from, Some(500));
        assert_eq!(report.applied[1].clamped_from, Some(-9));
    }

    #[test]
    fn apply_skips_and_reports_what_the_device_lacks_or_refuses() {
        let cam = camera();
        let mut s = ControlSnapshot::new(DeviceKey::new("x", "y"));
        s.record_value(ControlKey::Pan, 5); // absent
        s.record_auto(ControlKey::Contrast, true); // no Auto
        s.record_value(ControlKey::Brightness, 3);
        let report = apply_snapshot(&cam, &s).unwrap();
        assert_eq!(
            report.skipped,
            vec![
                (ControlKey::Contrast, SkipReason::AutoNotSupported),
                (ControlKey::Pan, SkipReason::Unsupported),
            ]
        );
        assert_eq!(*cam.log.borrow(), vec!["value brightness 3"]);
        assert_eq!(report.applied.len(), 1);
    }

    #[test]
    fn apply_continues_after_a_rejected_write_and_stops_when_the_device_is_gone() {
        let mut cam = camera();
        cam.reject = Some(ControlKey::Exposure);
        let mut s = ControlSnapshot::new(DeviceKey::new("x", "y"));
        s.record_auto(ControlKey::Exposure, false);
        s.record_value(ControlKey::Exposure, -3);
        s.record_value(ControlKey::Brightness, 1);
        let report = apply_snapshot(&cam, &s).unwrap();
        assert_eq!(report.skipped.len(), 1);
        assert!(matches!(report.skipped[0].1, SkipReason::Failed(_)));
        assert_eq!(
            *cam.log.borrow(),
            vec!["value brightness 1"],
            "no value after a failed flag"
        );

        struct Gone;
        impl ControlDevice for Gone {
            fn read(&self, _: ControlKey) -> Result<Option<ControlInfo>, ControlError> {
                unreachable!()
            }
            fn set_auto(&self, _: ControlKey, _: bool) -> Result<ControlInfo, ControlError> {
                Err(ControlError::Capture(CaptureError::DeviceLost(Error::from(
                    windows::Win32::Media::MediaFoundation::MF_E_VIDEO_RECORDING_DEVICE_INVALIDATED,
                ))))
            }
            fn set_value(&self, _: ControlKey, _: i32) -> Result<ControlInfo, ControlError> {
                unreachable!()
            }
        }
        assert!(apply_snapshot(&Gone, &s).is_err());
    }

    #[test]
    fn an_empty_snapshot_writes_nothing() {
        let cam = camera();
        let r = apply_snapshot(&cam, &ControlSnapshot::new(DeviceKey::new("x", "y"))).unwrap();
        assert_eq!(r, ApplyReport::default());
        assert!(cam.log.borrow().is_empty());
    }

    #[test]
    fn shutdown_and_device_loss_are_fatal_but_a_missing_property_is_not() {
        assert!(is_fatal(MF_E_SHUTDOWN));
        assert!(is_fatal(
            windows::Win32::Media::MediaFoundation::MF_E_VIDEO_RECORDING_DEVICE_INVALIDATED
        ));
        assert!(!is_fatal(HRESULT(0x8007_0490_u32 as i32)));
        assert!(!is_fatal(E_NOTIMPL));
    }
}
