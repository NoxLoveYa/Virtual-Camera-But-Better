//! Camera arrival and removal (`CM_Register_Notification` on `KSCATEGORY_VIDEO_CAMERA`).
//!
//! The callback runs on a configuration manager thread. It only parses the event and sends it into a channel, so
//! it never blocks. Events for Better Cam's own virtual camera are dropped (the capture list hides it too).
//!
//! An arrival can precede the moment Media Foundation can enumerate the device: retry `list_cameras` for a
//! second or two after an arrival before concluding the camera is unusable.

use std::ffi::c_void;
use std::mem::{offset_of, size_of};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::mpsc::{channel, Receiver, Sender};

use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Register_Notification, CM_Unregister_Notification, CM_NOTIFY_ACTION,
    CM_NOTIFY_ACTION_DEVICEINTERFACEARRIVAL, CM_NOTIFY_ACTION_DEVICEINTERFACEREMOVAL,
    CM_NOTIFY_EVENT_DATA, CM_NOTIFY_EVENT_DATA_0_0, CM_NOTIFY_FILTER,
    CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE, CR_SUCCESS, HCMNOTIFICATION,
};
use windows::Win32::Media::KernelStreaming::KSCATEGORY_VIDEO_CAMERA;

use super::{is_own_virtual_camera, CaptureError};

/// A camera appeared or disappeared.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HotplugEvent {
    /// A camera interface arrived. `link` is its device interface path (compare with [`super::links_match`]).
    Arrival { link: String },
    /// A camera interface was removed.
    Removal { link: String },
}

impl HotplugEvent {
    pub fn link(&self) -> &str {
        match self {
            HotplugEvent::Arrival { link } | HotplugEvent::Removal { link } => link,
        }
    }
}

/// Map a configuration manager action and interface path to an event. Only interface arrival and removal count;
/// everything else (query remove, instance events, custom events) is ignored, as is Better Cam's own camera.
pub fn map_notification(action: CM_NOTIFY_ACTION, link: &str) -> Option<HotplugEvent> {
    if is_own_virtual_camera("", link) {
        return None;
    }
    if action == CM_NOTIFY_ACTION_DEVICEINTERFACEARRIVAL {
        Some(HotplugEvent::Arrival {
            link: link.to_string(),
        })
    } else if action == CM_NOTIFY_ACTION_DEVICEINTERFACEREMOVAL {
        Some(HotplugEvent::Removal {
            link: link.to_string(),
        })
    } else {
        None
    }
}

/// The symbolic link out of a device interface notification.
///
/// # Safety
/// `data` must point at `size` readable bytes, as the callback receives them (or be null).
pub unsafe fn symbolic_link_from_event(
    data: *const CM_NOTIFY_EVENT_DATA,
    size: u32,
) -> Option<String> {
    if data.is_null() {
        return None;
    }
    let link_offset =
        offset_of!(CM_NOTIFY_EVENT_DATA, u) + offset_of!(CM_NOTIFY_EVENT_DATA_0_0, SymbolicLink);
    let size = size as usize;
    if size < link_offset + size_of::<u16>() {
        return None;
    }
    if (*data).FilterType != CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE {
        return None;
    }
    let chars = (size - link_offset) / size_of::<u16>();
    let start = (data as *const u8).add(link_offset) as *const u16;
    // The array is a flexible one; bound the scan by the size the system reported.
    let mut len = 0;
    while len < chars && start.add(len).read_unaligned() != 0 {
        len += 1;
    }
    let units: Vec<u16> = (0..len).map(|i| start.add(i).read_unaligned()).collect();
    Some(String::from_utf16_lossy(&units))
}

struct CallbackContext {
    tx: Sender<HotplugEvent>,
}

unsafe extern "system" fn on_notification(
    _handle: HCMNOTIFICATION,
    context: *const c_void,
    action: CM_NOTIFY_ACTION,
    data: *const CM_NOTIFY_EVENT_DATA,
    data_size: u32,
) -> u32 {
    // A panic must not unwind into the configuration manager.
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `context` is the boxed CallbackContext kept alive until after CM_Unregister_Notification.
        let ctx = unsafe { &*(context as *const CallbackContext) };
        // SAFETY: `data` and `data_size` come straight from the configuration manager.
        let Some(link) = (unsafe { symbolic_link_from_event(data, data_size) }) else {
            return;
        };
        if let Some(event) = map_notification(action, &link) {
            let _ = ctx.tx.send(event);
        }
    }));
    0 // ERROR_SUCCESS
}

/// Registration for camera arrival and removal. Dropping it unregisters (and waits for a running callback).
pub struct HotplugWatcher {
    handle: HCMNOTIFICATION,
    context: *mut CallbackContext,
}

// SAFETY: the handle and context are only used in Drop.
unsafe impl Send for HotplugWatcher {}

impl HotplugWatcher {
    /// Start watching. Events arrive on the returned channel in order.
    pub fn start() -> Result<(HotplugWatcher, Receiver<HotplugEvent>), CaptureError> {
        let (tx, rx) = channel();
        let context = Box::into_raw(Box::new(CallbackContext { tx }));

        let mut filter = CM_NOTIFY_FILTER {
            cbSize: size_of::<CM_NOTIFY_FILTER>() as u32,
            ..Default::default()
        };
        filter.FilterType = CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE;
        filter.u.DeviceInterface.ClassGuid = KSCATEGORY_VIDEO_CAMERA;

        let mut handle = HCMNOTIFICATION::default();
        // SAFETY: `filter` is read during the call; `context` stays valid until Drop; `handle` is a local.
        let cr = unsafe {
            CM_Register_Notification(
                &filter,
                Some(context as *const c_void),
                Some(on_notification),
                &mut handle,
            )
        };
        if cr != CR_SUCCESS {
            // SAFETY: registration failed, so nothing else holds the context.
            drop(unsafe { Box::from_raw(context) });
            return Err(CaptureError::Api {
                what: "CM_Register_Notification",
                error: windows::core::Error::new(
                    windows::Win32::Foundation::E_FAIL,
                    format!("CONFIGRET {}", cr.0),
                ),
            });
        }
        Ok((HotplugWatcher { handle, context }, rx))
    }
}

impl Drop for HotplugWatcher {
    fn drop(&mut self) {
        // SAFETY: CM_Unregister_Notification waits for callbacks in flight, so freeing the context afterwards is
        // sound. Never drop the watcher from inside the callback.
        unsafe {
            let _ = CM_Unregister_Notification(self.handle);
            drop(Box::from_raw(self.context));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Devices::DeviceAndDriverInstallation::{
        CM_NOTIFY_ACTION_DEVICEQUERYREMOVE, CM_NOTIFY_ACTION_DEVICEREMOVECOMPLETE,
    };

    const USB_LINK: &str = "\\\\?\\usb#vid_0c45&pid_636d&mi_00#9&3171a958&0&0000#{e5323777-f976-4f5b-9b55-b94699c46e44}\\global";
    const VCAM_LINK: &str = "\\\\?\\swd#vcamdevapi#d8f8cb27#{e5323777-f976-4f5b-9b55-b94699c46e44}\\{fcebba03-9d13-4c13-9940-cc84fcd132d1}";

    /// Build the bytes the configuration manager would pass for an interface event.
    fn event_bytes(filter_type: i32, link: &str, trailing_garbage: bool) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&filter_type.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&[0u8; 16]); // ClassGuid
        for u in link.encode_utf16().chain(std::iter::once(0)) {
            b.extend_from_slice(&u.to_le_bytes());
        }
        if trailing_garbage {
            b.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]);
        }
        b
    }

    fn parse(bytes: &[u8]) -> Option<String> {
        // Copy into an 8-aligned buffer, like the real callback data.
        let mut aligned = vec![0u64; bytes.len().div_ceil(8)];
        // SAFETY: `aligned` has at least `bytes.len()` bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                aligned.as_mut_ptr() as *mut u8,
                bytes.len(),
            )
        };
        unsafe {
            symbolic_link_from_event(
                aligned.as_ptr() as *const CM_NOTIFY_EVENT_DATA,
                bytes.len() as u32,
            )
        }
    }

    #[test]
    fn layout_matches_the_documented_struct() {
        // FilterType, Reserved, then the union at 8; ClassGuid (16 bytes) precedes the link.
        assert_eq!(offset_of!(CM_NOTIFY_EVENT_DATA, u), 8);
        assert_eq!(offset_of!(CM_NOTIFY_EVENT_DATA_0_0, SymbolicLink), 16);
    }

    #[test]
    fn interface_actions_map_to_events() {
        assert_eq!(
            map_notification(CM_NOTIFY_ACTION_DEVICEINTERFACEARRIVAL, USB_LINK),
            Some(HotplugEvent::Arrival {
                link: USB_LINK.into()
            })
        );
        assert_eq!(
            map_notification(CM_NOTIFY_ACTION_DEVICEINTERFACEREMOVAL, USB_LINK),
            Some(HotplugEvent::Removal {
                link: USB_LINK.into()
            })
        );
        assert_eq!(
            map_notification(CM_NOTIFY_ACTION_DEVICEQUERYREMOVE, USB_LINK),
            None
        );
        assert_eq!(
            map_notification(CM_NOTIFY_ACTION_DEVICEREMOVECOMPLETE, USB_LINK),
            None
        );
    }

    #[test]
    fn own_virtual_camera_events_are_dropped() {
        assert_eq!(
            map_notification(CM_NOTIFY_ACTION_DEVICEINTERFACEARRIVAL, VCAM_LINK),
            None
        );
        assert_eq!(
            map_notification(CM_NOTIFY_ACTION_DEVICEINTERFACEREMOVAL, VCAM_LINK),
            None
        );
    }

    #[test]
    fn link_is_read_from_raw_event_data() {
        assert_eq!(
            parse(&event_bytes(0, USB_LINK, false)).as_deref(),
            Some(USB_LINK)
        );
        // bytes after the terminator do not leak into the link
        assert_eq!(
            parse(&event_bytes(0, USB_LINK, true)).as_deref(),
            Some(USB_LINK)
        );
    }

    #[test]
    fn malformed_event_data_is_rejected() {
        assert_eq!(
            parse(&event_bytes(1, USB_LINK, false)),
            None,
            "device handle events carry no link"
        );
        assert_eq!(parse(&[0u8; 8]), None, "too short");
        assert_eq!(
            unsafe { symbolic_link_from_event(std::ptr::null(), 100) },
            None
        );
        // unterminated: the scan stops at the reported size
        let mut b = event_bytes(0, "abc", false);
        b.truncate(b.len() - 2);
        assert_eq!(parse(&b).as_deref(), Some("abc"));
    }

    #[test]
    fn end_to_end_event_flow() {
        let bytes = event_bytes(0, USB_LINK, false);
        let link = parse(&bytes).unwrap();
        let ev = map_notification(CM_NOTIFY_ACTION_DEVICEINTERFACEREMOVAL, &link).unwrap();
        assert!(super::super::links_match(ev.link(), USB_LINK));
    }

    #[test]
    fn watcher_registers_and_unregisters() {
        let (watcher, rx) = HotplugWatcher::start().expect("register");
        assert!(rx.try_recv().is_err(), "no event without a plug action");
        drop(watcher);
    }
}
