//! `status`: is the virtual camera listed by Media Foundation, and by DirectShow (what OBS uses)?

use windows::Win32::Media::DirectShow::ICreateDevEnum;
use windows::Win32::Media::MediaFoundation::{
    CLSID_SystemDeviceEnum, CLSID_VideoInputDeviceCategory, IMFActivate, MFCreateAttributes,
    MFEnumDeviceSources, MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
};
use windows::Win32::System::Com::StructuredStorage::IPropertyBag;
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_INPROC_SERVER};
use windows::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ,
};
use windows::Win32::System::Variant::VariantClear;
use windows_core::{w, PWSTR};

use crate::util::{init_com_and_mf, pcwstr, wide};

/// One MF video capture device.
#[derive(Debug, Clone)]
pub struct MfDevice {
    pub name: String,
    pub link: String,
}

fn activate_string(a: &IMFActivate, key: &windows_core::GUID) -> String {
    let mut p = PWSTR::null();
    let mut len = 0u32;
    if unsafe { a.GetAllocatedString(key, &mut p, &mut len) }.is_err() || p.is_null() {
        return String::new();
    }
    let s = unsafe { p.to_string() }.unwrap_or_default();
    unsafe { CoTaskMemFree(Some(p.0 as *const _)) };
    s
}

/// All video capture devices as `(activate, name, link)`.
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

fn registry_has_clsid() -> bool {
    let key = wide(&vcam_protocol::clsid_key());
    let mut h = HKEY::default();
    let r = unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, pcwstr(&key), None, KEY_READ, &mut h) };
    if r.is_ok() {
        unsafe {
            let _ = RegCloseKey(h);
        }
        true
    } else {
        false
    }
}

pub fn run() -> i32 {
    if let Err(e) = init_com_and_mf() {
        eprintln!("MFStartup failed: {e}");
        return 1;
    }
    let want = vcam_protocol::CAMERA_NAME.to_lowercase();
    println!("registry clsid key present: {}", registry_has_clsid());

    let devices = match enum_mf_devices() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("MFEnumDeviceSources failed: {e}");
            return 1;
        }
    };
    for (_, d) in &devices {
        println!("mf device: '{}' link '{}'", d.name, d.link);
    }
    match devices
        .iter()
        .find(|(_, d)| d.name.to_lowercase().contains(&want))
    {
        Some((_, d)) => {
            println!("listed: true");
            println!("friendly name: {}", d.name);
            println!("symbolic link: {}", d.link);
        }
        None => println!("listed: false"),
    }

    match enum_directshow_names() {
        Ok(names) => {
            for n in &names {
                println!("directshow device: '{n}'");
            }
            println!(
                "directshow listed: {}",
                names.iter().any(|n| n.to_lowercase().contains(&want))
            );
        }
        Err(e) => println!("directshow enumeration failed: {e}"),
    }
    0
}
