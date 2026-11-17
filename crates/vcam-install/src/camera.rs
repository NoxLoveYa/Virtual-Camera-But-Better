//! The virtual camera object (`MFCreateVirtualCamera`) and what Windows lists.

use vcam_protocol::{CAMERA_NAME, CLSID_STRING};
use windows::Win32::Media::DirectShow::ICreateDevEnum;
use windows::Win32::Media::MediaFoundation::{
    CLSID_SystemDeviceEnum, CLSID_VideoInputDeviceCategory, IMFActivate, IMFVirtualCamera,
    MFCreateAttributes, MFCreateVirtualCamera, MFEnumDeviceSources, MFVirtualCameraAccess_AllUsers,
    MFVirtualCameraLifetime_System, MFVirtualCameraType_SoftwareCameraSource,
    MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
};
use windows::Win32::System::Com::StructuredStorage::IPropertyBag;
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_INPROC_SERVER};
use windows::Win32::System::Variant::VariantClear;
use windows_core::{w, GUID, PWSTR};

use crate::util::{pcwstr, werr, wide, Res};

/// System lifetime, all users, the media source of this project. The same call creates, re-opens and (with
/// `Remove`) deletes the camera: identical parameters give the identical device.
pub fn create_virtual_camera() -> Res<IMFVirtualCamera> {
    let name = wide(CAMERA_NAME);
    let clsid = wide(CLSID_STRING);
    unsafe {
        MFCreateVirtualCamera(
            MFVirtualCameraType_SoftwareCameraSource,
            MFVirtualCameraLifetime_System,
            MFVirtualCameraAccess_AllUsers,
            pcwstr(&name),
            pcwstr(&clsid),
            None,
        )
    }
    .map_err(|e| werr("MFCreateVirtualCamera", e))
}

/// `GUID_DEVCLASS_CAMERA` {ca3e7ab9-b4c3-4ae6-8251-579ef933890f}.
const GUID_DEVCLASS_CAMERA: GUID = GUID::from_u128(0xca3e7ab9_b4c3_4ae6_8251_579ef933890f);

/// `AddProperty(DEVPKEY_Device_ClassGuid, GUID_DEVCLASS_CAMERA)`: the DirectShow fallback (`--dshow-class`).
pub fn add_dshow_class(cam: &IMFVirtualCamera) -> Res<()> {
    use windows::Win32::Devices::Properties::{DEVPKEY_Device_ClassGuid, DEVPROP_TYPE_GUID};
    let bytes: [u8; 16] = unsafe { std::mem::transmute(GUID_DEVCLASS_CAMERA) };
    unsafe { cam.AddProperty(&DEVPKEY_Device_ClassGuid, DEVPROP_TYPE_GUID, &bytes) }
        .map_err(|e| werr("IMFVirtualCamera::AddProperty(DEVPKEY_Device_ClassGuid)", e))
}

/// One MF video capture device.
#[derive(Debug, Clone)]
pub struct MfDevice {
    pub name: String,
    pub link: String,
}

fn activate_string(a: &IMFActivate, key: &GUID) -> String {
    let mut p = PWSTR::null();
    let mut len = 0u32;
    if unsafe { a.GetAllocatedString(key, &mut p, &mut len) }.is_err() || p.is_null() {
        return String::new();
    }
    let s = unsafe { p.to_string() }.unwrap_or_default();
    unsafe { CoTaskMemFree(Some(p.0 as *const _)) };
    s
}

/// All video capture devices as `(activate, name, link)`. Media Foundation must be started.
pub fn enum_mf_devices() -> windows_core::Result<Vec<(IMFActivate, MfDevice)>> {
    let mut attrs = None;
    unsafe { MFCreateAttributes(&mut attrs, 1)? };
    let attrs = attrs.unwrap();
    unsafe {
        attrs.SetGUID(
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
        )?;
    }
    let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    unsafe { MFEnumDeviceSources(&attrs, &mut list, &mut count)? };
    let mut out = Vec::new();
    for i in 0..count as usize {
        let item = unsafe { std::ptr::read(list.add(i)) };
        if let Some(a) = item {
            let name = activate_string(&a, &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME);
            let link =
                activate_string(&a, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK);
            out.push((a, MfDevice { name, link }));
        }
    }
    if !list.is_null() {
        unsafe { CoTaskMemFree(Some(list as *const _)) };
    }
    Ok(out)
}

/// Whether `name` is the friendly name of Better Cam's camera (Windows appends " (Windows Virtual Camera)").
pub fn is_better_cam_name(name: &str) -> bool {
    name.to_lowercase().contains(&CAMERA_NAME.to_lowercase())
}

/// Media Foundation lists Better Cam's camera. Media Foundation must be started.
pub fn is_listed() -> windows_core::Result<bool> {
    Ok(enum_mf_devices()?
        .iter()
        .any(|(_, d)| is_better_cam_name(&d.name)))
}

/// Friendly names of the DirectShow video input devices.
pub fn enum_directshow_names() -> windows_core::Result<Vec<String>> {
    let dev_enum: ICreateDevEnum =
        unsafe { CoCreateInstance(&CLSID_SystemDeviceEnum, None, CLSCTX_INPROC_SERVER)? };
    let mut monikers = None;
    // S_FALSE with a null enumerator means "category is empty"
    unsafe { dev_enum.CreateClassEnumerator(&CLSID_VideoInputDeviceCategory, &mut monikers, 0)? };
    let mut names = Vec::new();
    let Some(monikers) = monikers else {
        return Ok(names);
    };
    loop {
        let mut item = [None];
        let mut fetched = 0u32;
        let hr = unsafe { monikers.Next(&mut item, Some(&mut fetched)) };
        if hr.is_err() || fetched == 0 {
            break;
        }
        let Some(moniker) = item[0].take() else { break };
        let bag: windows_core::Result<IPropertyBag> = unsafe { moniker.BindToStorage(None, None) };
        if let Ok(bag) = bag {
            let mut v = Default::default();
            if unsafe { bag.Read(w!("FriendlyName"), &mut v, None) }.is_ok() {
                // VT_BSTR
                let s = windows_core::BSTR::try_from(&v)
                    .map(|b| b.to_string())
                    .unwrap_or_default();
                names.push(s);
            }
            unsafe {
                let _ = VariantClear(&mut v);
            }
        }
    }
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_camera_is_recognised_by_its_friendly_name() {
        assert!(is_better_cam_name("Better Cam (Windows Virtual Camera)"));
        assert!(is_better_cam_name("better cam"));
        assert!(!is_better_cam_name("Logitech C920"));
    }
}
