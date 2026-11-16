//! `IClassFactory`: hands out the `Activator` (Frame Server asks for `IMFActivate`) or, for in-process users that
//! ask for a media source interface directly, a bare `MediaSource`.

use std::sync::atomic::{AtomicI32, Ordering};

use windows::Win32::Foundation::{CLASS_E_NOAGGREGATION, E_POINTER};
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFAttributes, MFCreateAttributes, MF_E_SHUTDOWN,
};
use windows::Win32::System::Com::{IClassFactory, IClassFactory_Impl};
use windows_core::{implement, Error, IUnknown, Interface, Ref, Result, BOOL, GUID};

use crate::activator::Activator;
use crate::guard::guard;
use crate::source::MediaSource;
use crate::vlog;

pub static LOCK_COUNT: AtomicI32 = AtomicI32::new(0);

#[implement(IClassFactory)]
pub struct Factory;

impl Factory {
    pub fn new() -> Factory {
        crate::object_created();
        Factory
    }
}

impl Default for Factory {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Factory {
    fn drop(&mut self) {
        crate::object_dropped();
    }
}

impl IClassFactory_Impl for Factory_Impl {
    fn CreateInstance(
        &self,
        punkouter: Ref<IUnknown>,
        riid: *const GUID,
        ppvobject: *mut *mut core::ffi::c_void,
    ) -> Result<()> {
        guard("Factory::CreateInstance", || {
            if riid.is_null() || ppvobject.is_null() {
                return Err(Error::from(E_POINTER));
            }
            unsafe { *ppvobject = std::ptr::null_mut() };
            let riid = unsafe { *riid };
            vlog!("Factory::CreateInstance iid {{{riid:?}}}");
            if punkouter.as_ref().is_some() {
                return Err(Error::from(CLASS_E_NOAGGREGATION));
            }
            let unk: IUnknown = if riid == IMFActivate::IID
                || riid == IUnknown::IID
                || riid == IMFAttributes::IID
            {
                Activator::create()?.to_interface()
            } else {
                // a media source interface asked for directly (in-process users)
                let mut store: Option<IMFAttributes> = None;
                unsafe { MFCreateAttributes(&mut store, 4)? };
                let store = store.ok_or_else(|| Error::from(MF_E_SHUTDOWN))?;
                MediaSource::create(store)?.to_interface()
            };
            let hr = unsafe { unk.query(&riid, ppvobject) };
            vlog!("Factory::CreateInstance -> {hr:?}");
            hr.ok()
        })
    }

    fn LockServer(&self, flock: BOOL) -> Result<()> {
        guard("Factory::LockServer", || {
            if flock.as_bool() {
                LOCK_COUNT.fetch_add(1, Ordering::SeqCst);
            } else {
                LOCK_COUNT.fetch_sub(1, Ordering::SeqCst);
            }
            Ok(())
        })
    }
}
