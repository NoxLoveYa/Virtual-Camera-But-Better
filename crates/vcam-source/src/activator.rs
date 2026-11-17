//! `Activator`: the `IMFActivate` that Frame Server creates through the class factory. Its attribute store (set by
//! Frame Server: symbolic link and so on) is shared with the media source it creates.

use std::sync::Mutex;

use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFActivate_Impl, IMFAttributes, IMFMediaSource, IMFMediaSource2,
    MFCreateAttributes, MFT_TRANSFORM_CLSID_Attribute, MF_ATTRIBUTE_BLOB, MF_ATTRIBUTE_DOUBLE,
    MF_ATTRIBUTE_GUID, MF_ATTRIBUTE_IUNKNOWN, MF_ATTRIBUTE_STRING, MF_ATTRIBUTE_UINT32,
    MF_ATTRIBUTE_UINT64, MF_E_SHUTDOWN,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows_core::{implement, ComObject, Error, IUnknown, Interface, Result, GUID};

use crate::attrs::delegate_attributes;
use crate::guard::{guard, LockExt};
use crate::source::MediaSource;
use crate::{object_created, object_dropped, vlog};

/// Undocumented Frame Server attribute: pid of the process that asked for the camera.
const MF_FRAMESERVER_CLIENTCONTEXT_CLIENTPID: GUID =
    GUID::from_u128(0x5f8d322e_0fe4_43e4_9e50_d83ecd9fc2b8);

#[implement(IMFActivate)]
pub struct Activator {
    attributes: IMFAttributes,
    source: Mutex<Option<ComObject<MediaSource>>>,
}

impl Activator {
    pub fn create() -> Result<ComObject<Activator>> {
        let mut store: Option<IMFAttributes> = None;
        unsafe { MFCreateAttributes(&mut store, 8)? };
        let attributes = store.ok_or_else(|| Error::from(MF_E_SHUTDOWN))?;
        unsafe {
            attributes.SetGUID(
                &MFT_TRANSFORM_CLSID_Attribute,
                &GUID::from_u128(vcam_protocol::CLSID_U128),
            )?
        };
        object_created();
        Ok(ComObject::new(Activator {
            attributes,
            source: Mutex::new(None),
        }))
    }
}

impl Drop for Activator {
    fn drop(&mut self) {
        object_dropped();
    }
}

delegate_attributes!(Activator_Impl, attributes);

/// Log every attribute Frame Server put on the activator (keys, types and simple values).
pub fn dump_attributes(prefix: &str, a: &IMFAttributes) {
    let count = unsafe { a.GetCount() }.unwrap_or(0);
    vlog!("{prefix}: {count} attributes");
    for i in 0..count {
        let mut key = GUID::zeroed();
        let mut value = PROPVARIANT::default();
        if unsafe { a.GetItemByIndex(i, &mut key, Some(&mut value)) }.is_err() {
            continue;
        }
        let ty = unsafe { a.GetItemType(&key) }.map(|t| t.0).unwrap_or(-1);
        let text = unsafe {
            if ty == MF_ATTRIBUTE_UINT32.0 {
                a.GetUINT32(&key).map(|v| v.to_string()).unwrap_or_default()
            } else if ty == MF_ATTRIBUTE_UINT64.0 {
                a.GetUINT64(&key).map(|v| v.to_string()).unwrap_or_default()
            } else if ty == MF_ATTRIBUTE_DOUBLE.0 {
                a.GetDouble(&key).map(|v| v.to_string()).unwrap_or_default()
            } else if ty == MF_ATTRIBUTE_GUID.0 {
                a.GetGUID(&key)
                    .map(|v| format!("{v:?}"))
                    .unwrap_or_default()
            } else if ty == MF_ATTRIBUTE_STRING.0 {
                let mut buf = [0u16; 512];
                let mut len = 0u32;
                a.GetString(&key, &mut buf, Some(&mut len))
                    .map(|_| String::from_utf16_lossy(&buf[..(len as usize).min(buf.len())]))
                    .unwrap_or_default()
            } else if ty == MF_ATTRIBUTE_BLOB.0 {
                "<blob>".into()
            } else if ty == MF_ATTRIBUTE_IUNKNOWN.0 {
                "<iunknown>".into()
            } else {
                String::new()
            }
        };
        vlog!("  {{{key:?}}} type {ty} = {text}");
    }
}

impl IMFActivate_Impl for Activator_Impl {
    fn ActivateObject(&self, riid: *const GUID, ppv: *mut *mut core::ffi::c_void) -> Result<()> {
        guard("Activator::ActivateObject", || {
            if riid.is_null() || ppv.is_null() {
                return Err(Error::from(windows::Win32::Foundation::E_POINTER));
            }
            unsafe { *ppv = std::ptr::null_mut() };
            let riid = unsafe { *riid };
            vlog!("Activator::ActivateObject iid {{{riid:?}}}");
            if let Ok(pid) = unsafe {
                self.attributes
                    .GetUINT32(&MF_FRAMESERVER_CLIENTCONTEXT_CLIENTPID)
            } {
                vlog!("Activator::ActivateObject client pid {pid}");
            }
            dump_attributes("Activator attributes", &self.attributes);

            let mut slot = self.source.lock_safe();
            let source = match slot.as_ref() {
                Some(s) => s.clone(),
                None => {
                    let s = MediaSource::create(self.attributes.clone())?;
                    *slot = Some(s.clone());
                    s
                }
            };
            let unk: IUnknown = source.to_interface();
            let hr = unsafe { unk.query(&riid, ppv) };
            vlog!("Activator::ActivateObject -> {hr:?}");
            hr.ok()
        })
    }

    fn ShutdownObject(&self) -> Result<()> {
        guard("Activator::ShutdownObject", || {
            vlog!("Activator::ShutdownObject");
            if let Some(s) = self.source.lock_safe().take() {
                let src: IMFMediaSource = s.to_interface::<IMFMediaSource2>().into();
                let _ = unsafe { src.Shutdown() };
            }
            Ok(())
        })
    }

    fn DetachObject(&self) -> Result<()> {
        guard("Activator::DetachObject", || {
            vlog!("Activator::DetachObject");
            *self.source.lock_safe() = None;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Media::MediaFoundation::{MFStartup, MFSTARTUP_FULL, MF_VERSION};

    /// References on a COM object, read with an AddRef/Release pair.
    fn refs(u: &IUnknown) -> u32 {
        unsafe {
            let vt = Interface::vtable(u);
            let n = (vt.AddRef)(u.as_raw());
            (vt.Release)(u.as_raw());
            n - 1
        }
    }

    /// Review finding 5 said `dump_attributes` leaks the `PROPVARIANT` it reads (no `Drop` in windows 0.62). It does
    /// not: with the `Win32_System_Variant` feature (enabled for this crate) the `windows` crate gives `PROPVARIANT`
    /// a `Drop` that calls `PropVariantClear` (`windows-0.62.2/src/extensions/Win32/System/StructuredStorage.rs`).
    /// This test pins that down: a COM reference held by an attribute keeps its count across many dumps. If the
    /// feature were ever removed, this test would fail and `PropVariantClear` would have to be called by hand.
    #[test]
    fn dumping_the_attributes_leaks_no_reference() {
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL).unwrap() };
        let mut store: Option<IMFAttributes> = None;
        unsafe { MFCreateAttributes(&mut store, 4).unwrap() };
        let store = store.unwrap();
        let mut held: Option<IMFAttributes> = None;
        unsafe { MFCreateAttributes(&mut held, 1).unwrap() };
        let held: IUnknown = held.unwrap().into();
        unsafe {
            store
                .SetUnknown(
                    &GUID::from_u128(0x1111_2222_3333_4444_5555_6666_7777_8888),
                    &held,
                )
                .unwrap();
            store
                .SetString(
                    &GUID::from_u128(0x2111_2222_3333_4444_5555_6666_7777_8888),
                    windows_core::w!("a string that an unread PROPVARIANT would leak"),
                )
                .unwrap();
            store
                .SetUINT32(
                    &GUID::from_u128(0x3111_2222_3333_4444_5555_6666_7777_8888),
                    7,
                )
                .unwrap();
        }
        let before = refs(&held);
        for _ in 0..50 {
            dump_attributes("test", &store);
        }
        assert_eq!(
            refs(&held),
            before,
            "dump_attributes keeps references it never releases"
        );
    }
}
