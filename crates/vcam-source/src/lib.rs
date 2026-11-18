//! Better Cam virtual camera media source: a COM DLL that Windows Frame Server loads as LOCAL SERVICE.
//! Pixels come from the shared file `%ProgramData%\BetterCam\frames.bin` written by the app (see `vcam-protocol`),
//! or from a slate when no writer is alive.
//!
//! Hard rule: nothing may unwind out of an export or a COM method (a crash here takes down Frame Server for every
//! camera app). Every entry point goes through `guard::guard` / `guard::guard_hr`.

// COM method signatures are dictated by the windows-rs traits and take raw pointers from the caller.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

pub mod activator;
mod attrs;
pub mod factory;
pub mod frames;
pub mod guard;
pub mod log;
pub mod mediatype;
pub mod samples;
pub mod source;
pub mod stream;

use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use windows::Win32::Foundation::{CLASS_E_CLASSNOTAVAILABLE, E_POINTER, S_FALSE, S_OK};
use windows::Win32::System::Com::IClassFactory;
use windows_core::{ComObject, Interface, GUID, HRESULT};

/// Live COM objects created by this DLL.
static OBJECT_COUNT: AtomicI32 = AtomicI32::new(0);
/// Set once any pacing thread ran: the DLL must then never be unloaded (a thread could still be returning).
static THREAD_EVER_SPAWNED: AtomicBool = AtomicBool::new(false);

pub(crate) fn object_created() {
    OBJECT_COUNT.fetch_add(1, Ordering::SeqCst);
}
pub(crate) fn object_dropped() {
    OBJECT_COUNT.fetch_sub(1, Ordering::SeqCst);
}
pub(crate) fn pump_thread_spawned() {
    THREAD_EVER_SPAWNED.store(true, Ordering::SeqCst);
}

/// The CLSID as a `GUID`.
pub fn clsid() -> GUID {
    GUID::from_u128(vcam_protocol::CLSID_U128)
}

/// COM export: class factory for [`clsid`].
///
/// # Safety
/// Called by COM. `rclsid`, `riid` and `ppv` must be valid for the duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DllGetClassObject(
    rclsid: *const GUID,
    riid: *const GUID,
    ppv: *mut *mut core::ffi::c_void,
) -> HRESULT {
    guard::guard_hr("DllGetClassObject", || {
        guard::install_panic_hook();
        if ppv.is_null() || rclsid.is_null() || riid.is_null() {
            return E_POINTER;
        }
        unsafe { *ppv = std::ptr::null_mut() };
        let (rclsid, riid) = unsafe { (*rclsid, *riid) };
        vlog!("DllGetClassObject clsid {{{rclsid:?}}} iid {{{riid:?}}}");
        if rclsid != clsid() {
            return CLASS_E_CLASSNOTAVAILABLE;
        }
        let factory: IClassFactory = ComObject::new(factory::Factory::new()).to_interface();
        let hr = unsafe { factory.query(&riid, ppv) };
        vlog!("DllGetClassObject -> {hr:?}");
        hr
    })
}

/// COM export: `S_OK` only when nothing is alive and no pacing thread was ever started.
#[unsafe(no_mangle)]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    guard::guard_hr("DllCanUnloadNow", || {
        let busy = OBJECT_COUNT.load(Ordering::SeqCst) > 0
            || factory::LOCK_COUNT.load(Ordering::SeqCst) > 0
            || THREAD_EVER_SPAWNED.load(Ordering::SeqCst);
        if busy {
            S_FALSE
        } else {
            S_OK
        }
    })
}

#[cfg(test)]
mod tests;
