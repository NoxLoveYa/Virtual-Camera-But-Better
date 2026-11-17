//! Holding an opportunistic lock (oplock) on a file, for tests of code that must not wait for such a lock.
//!
//! Needs no administrator rights: a handle opened for reading may request a read-write-handle oplock when no other
//! handle to the file is open. While it is held, a second open of the file by anybody else (a plain `CreateFileW`,
//! `DeleteFileW`) blocks until the holder closes its handle: that is what a standard user could do to the
//! service's activation thread with a file they may read.

use std::ffi::c_void;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, ERROR_IO_PENDING, GENERIC_READ, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAGS_AND_ATTRIBUTES, FILE_FLAG_OVERLAPPED,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Ioctl::{
    FSCTL_REQUEST_OPLOCK, REQUEST_OPLOCK_INPUT_BUFFER, REQUEST_OPLOCK_INPUT_FLAG_REQUEST,
    REQUEST_OPLOCK_OUTPUT_BUFFER,
};
use windows::Win32::System::IO::{DeviceIoControl, OVERLAPPED};

/// `OPLOCK_LEVEL_CACHE_READ | OPLOCK_LEVEL_CACHE_WRITE | OPLOCK_LEVEL_CACHE_HANDLE`.
pub const READ_WRITE_HANDLE: u32 = 1 | 2 | 4;
/// `OPLOCK_LEVEL_CACHE_READ | OPLOCK_LEVEL_CACHE_HANDLE`: what a program that has the file open for reading can get
/// while others read it too. It is broken by a rename or a delete, not by an open that shares everything.
pub const READ_HANDLE: u32 = 1 | 4;

/// An oplock on a file, held until this value is dropped (the handle is closed, which gives the lock up).
pub struct Oplock {
    handle: HANDLE,
    // the kernel writes into these when the lock breaks or the handle closes: they live as long as the handle
    _overlapped: Box<OVERLAPPED>,
    _output: Box<REQUEST_OPLOCK_OUTPUT_BUFFER>,
}

impl Oplock {
    /// Open `path` for reading and take a read-write-handle oplock on it. `Ok(None)` when the file system does
    /// not grant one (the caller skips its test); `Err` for anything unexpected.
    pub fn take(path: &Path) -> io::Result<Option<Oplock>> {
        Self::take_level(path, READ_WRITE_HANDLE)
    }

    /// [`Oplock::take`] for another level ([`READ_HANDLE`]).
    pub fn take_level(path: &Path, level: u32) -> io::Result<Option<Oplock>> {
        let w: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let handle = unsafe {
            CreateFileW(
                PCWSTR(w.as_ptr()),
                GENERIC_READ.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(FILE_ATTRIBUTE_NORMAL.0 | FILE_FLAG_OVERLAPPED.0),
                None,
            )
        }
        .map_err(|e| io::Error::from_raw_os_error(e.code().0 & 0xFFFF))?;
        let input = REQUEST_OPLOCK_INPUT_BUFFER {
            StructureVersion: 1,
            StructureLength: std::mem::size_of::<REQUEST_OPLOCK_INPUT_BUFFER>() as u16,
            RequestedOplockLevel: level,
            Flags: REQUEST_OPLOCK_INPUT_FLAG_REQUEST,
        };
        let mut output = Box::new(REQUEST_OPLOCK_OUTPUT_BUFFER {
            StructureVersion: 1,
            StructureLength: std::mem::size_of::<REQUEST_OPLOCK_OUTPUT_BUFFER>() as u16,
            ..Default::default()
        });
        let mut overlapped = Box::new(OVERLAPPED::default());
        let r = unsafe {
            DeviceIoControl(
                handle,
                FSCTL_REQUEST_OPLOCK,
                Some(&input as *const _ as *const c_void),
                std::mem::size_of::<REQUEST_OPLOCK_INPUT_BUFFER>() as u32,
                Some(&mut *output as *mut _ as *mut c_void),
                std::mem::size_of::<REQUEST_OPLOCK_OUTPUT_BUFFER>() as u32,
                None,
                Some(&mut *overlapped),
            )
        };
        match r {
            // a granted oplock stays pending until it breaks
            Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => Ok(Some(Oplock {
                handle,
                _overlapped: overlapped,
                _output: output,
            })),
            Err(_) | Ok(()) => {
                unsafe {
                    let _ = CloseHandle(handle);
                }
                Ok(None)
            }
        }
    }
}

impl Drop for Oplock {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}
