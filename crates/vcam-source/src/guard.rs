//! Panic containment. A panic that unwinds out of a COM method or a DLL export would terminate Frame Server and
//! take every camera app on the machine with it, so each entry point runs inside [`guard`] or [`guard_hr`].

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Mutex, MutexGuard, Once};

use windows::Win32::Foundation::E_UNEXPECTED;
use windows_core::{Error, Result, HRESULT};

use crate::vlog;

/// Run `f`; a panic is logged and becomes `E_UNEXPECTED`.
pub fn guard<T>(what: &str, f: impl FnOnce() -> Result<T>) -> Result<T> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(_) => {
            vlog!("PANIC contained in {what}");
            Err(Error::from(E_UNEXPECTED))
        }
    }
}

/// Like [`guard`] for entry points that return a raw `HRESULT`.
pub fn guard_hr(what: &str, f: impl FnOnce() -> HRESULT) -> HRESULT {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(_) => {
            vlog!("PANIC contained in {what}");
            E_UNEXPECTED
        }
    }
}

/// Lock a mutex and ignore poisoning: a panic in one call must not wedge every later call.
pub trait LockExt<T> {
    fn lock_safe(&self) -> MutexGuard<'_, T>;
}

impl<T> LockExt<T> for Mutex<T> {
    fn lock_safe(&self) -> MutexGuard<'_, T> {
        match self.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        }
    }
}

/// Route panic messages into the log (chained in front of the previous hook).
pub fn install_panic_hook() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let loc = info
                .location()
                .map(|l| format!("{}:{}", l.file(), l.line()))
                .unwrap_or_default();
            let msg = if let Some(s) = info.payload().downcast_ref::<&str>() {
                (*s).to_string()
            } else if let Some(s) = info.payload().downcast_ref::<String>() {
                s.clone()
            } else {
                "<non-string payload>".into()
            };
            crate::log::log(&format!("PANIC at {loc}: {msg}"));
            previous(info);
        }));
    });
}
