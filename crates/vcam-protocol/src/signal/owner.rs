//! The reader's check of **who made** the frame events it opened (`docs/FRAME-EVENT.md`, "Owner check").
//!
//! The nonces are public, and the events vanish with their last handle, so after an app that died without `close` the
//! names are free for anyone who listed them. A reader that waits on an object somebody else made is waiting on a
//! signal that is not the writer's. It therefore checks each *opened handle* (never a path):
//!
//! * the **owner** is the user the installer named: the one access-allowed trustee of the DACL of `frames.bin` that is
//!   not SYSTEM, Administrators or LOCAL SERVICE ([`expected_owner`]). Only an administrator can change that DACL, so
//!   nobody who can create an object in `\BaseNamedObjects` can make the expected value be theirs. A user that is not
//!   the owner of an event cannot make themselves its owner without a privilege;
//! * the event carries an **explicit mandatory label of medium integrity or higher**. A process cannot give an object
//!   a label above its own integrity level, so this keeps out a low-integrity process of the same user that made the
//!   object itself (it would be owned by the user, but could only label it low). It is read from the descriptor; an
//!   object with no label is refused even though Windows treats it as medium.
//!
//! The alternative, comparing the whole descriptor with [`super::descriptor_text`], was not chosen: it adds nothing
//! against the threat (a same-user, medium-integrity creator can make exactly that descriptor, and is the "malware
//! running as the user" case that the demotion rule bounds), and it would turn any future change of the descriptor
//! (an extra ACE for a new reader) into "every older DLL polls" without anyone noticing.
//!
//! Everything here runs in the camera service. It opens nothing but the two events and the frame file's descriptor, and
//! asks for no right beyond `SYNCHRONIZE | READ_CONTROL` (events) and `READ_CONTROL | FILE_READ_ATTRIBUTES` (file).

use std::ffi::c_void;
use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{LocalFree, ERROR_SUCCESS, HANDLE, HLOCAL};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSidToSidW, GetSecurityInfo, SE_FILE_OBJECT,
    SE_KERNEL_OBJECT,
};
use windows::Win32::Security::{
    EqualSid, GetAce, ACL, DACL_SECURITY_INFORMATION, OBJECT_SECURITY_INFORMATION,
    OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
};
use windows::Win32::System::Threading::{OpenEventW, SYNCHRONIZATION_ACCESS_RIGHTS};

use super::{event_name_wide, win_err};
use crate::secure;

/// SYSTEM, Administrators and LOCAL SERVICE: the trustees of `frames.bin` that are never "the user".
const NOT_A_USER: [&str; 3] = ["S-1-5-18", "S-1-5-32-544", "S-1-5-19"];

/// `SYNCHRONIZE | READ_CONTROL`: what the reader opens each event with (wait, and read the owner and the label).
const READER_ACCESS: u32 = 0x0010_0000 | 0x0002_0000;

/// `LABEL_SECURITY_INFORMATION`.
const LABEL_INFO: u32 = 0x10;

/// `SYSTEM_MANDATORY_LABEL_ACE_TYPE`.
const LABEL_ACE: u8 = 0x11;

/// `ACCESS_ALLOWED_ACE_TYPE`.
const ALLOWED_ACE: u8 = 0;

/// `SECURITY_MANDATORY_MEDIUM_RID`: the lowest integrity level an event of the writer may carry.
const MEDIUM_RID: u32 = 0x2000;

/// One part of a SID as a canonical 32-bit decimal: digits only, no leading zero, at most `u32::MAX`.
fn sub_authority(p: &str) -> Option<u32> {
    if p.is_empty() || p.len() > 10 || !p.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if p.len() > 1 && p.starts_with('0') {
        return None;
    }
    p.parse::<u32>().ok()
}

/// The shape of the SID of a user account that the installer accepts (`vcam-install` `acl::valid_sid`, which a test
/// there compares with this): `S-1-5-21-<4 parts>` (local, domain and Microsoft accounts) or `S-1-12-1-<4 parts>`
/// (Entra ID), every part a plain 32-bit decimal. No well-known SID, no group, nothing else.
pub fn is_user_sid_shape(sid: &str) -> bool {
    let rest = if let Some(rest) = sid.strip_prefix("S-1-5-21-") {
        rest
    } else if let Some(rest) = sid.strip_prefix("S-1-12-1-") {
        rest
    } else {
        return false;
    };
    let parts: Vec<&str> = rest.split('-').collect();
    parts.len() == 4 && parts.iter().all(|p| sub_authority(p).is_some())
}

/// The user a frame event must belong to, from the SIDs of the access-allowed ACEs of the DACL of `frames.bin`: the
/// single trustee that is none of SYSTEM, Administrators and LOCAL SERVICE (the same SID in several ACEs counts once).
/// `None` when there is none or more than one, or when it does not have the shape of a user account
/// ([`is_user_sid_shape`]): no event is trusted then, and the pump polls.
pub fn expected_owner<S: AsRef<str>>(allowed: &[S]) -> Option<String> {
    let mut found: Option<&str> = None;
    for s in allowed {
        let s = s.as_ref();
        if NOT_A_USER.iter().any(|n| n.eq_ignore_ascii_case(s)) {
            continue;
        }
        match found {
            None => found = Some(s),
            Some(f) if f.eq_ignore_ascii_case(s) => {}
            Some(_) => return None,
        }
    }
    found.filter(|s| is_user_sid_shape(s)).map(str::to_string)
}

/// `S-1-...` text of a SID.
fn sid_text(p: PSID) -> io::Result<String> {
    let mut s = PWSTR::null();
    unsafe { ConvertSidToStringSidW(p, &mut s) }.map_err(win_err)?;
    let t = unsafe { s.to_string() }.map_err(|e| io::Error::other(e.to_string()));
    unsafe {
        let _ = LocalFree(Some(HLOCAL(s.0 as *mut c_void)));
    }
    t
}

/// A descriptor allocated by `GetSecurityInfo`, freed on drop.
struct Descriptor(PSECURITY_DESCRIPTOR);

impl Drop for Descriptor {
    fn drop(&mut self) {
        unsafe {
            let _ = LocalFree(Some(HLOCAL(self.0 .0)));
        }
    }
}

/// `ACCESS_ALLOWED_ACE` and `SYSTEM_MANDATORY_LABEL_ACE` start alike: type, flags, size, a mask, then the SID.
#[repr(C)]
struct AceHead {
    kind: u8,
    flags: u8,
    size: u16,
    mask: u32,
    sid_start: u32,
}

/// `(type, mask, sid text)` of each ACE of `acl` (a null list has none).
fn aces(acl: *mut ACL) -> io::Result<Vec<(u8, u32, String)>> {
    let mut out = Vec::new();
    if acl.is_null() {
        return Ok(out);
    }
    let count = unsafe { (*acl).AceCount };
    for i in 0..count as u32 {
        let mut p: *mut c_void = std::ptr::null_mut();
        unsafe { GetAce(acl, i, &mut p) }.map_err(win_err)?;
        // SAFETY: GetAce returned a pointer to the i-th ACE inside the list; only the common head is read, and the SID
        // starts at `sid_start`.
        let a = unsafe { &*(p as *const AceHead) };
        let sid = PSID(&a.sid_start as *const u32 as *mut c_void);
        out.push((a.kind, a.mask, sid_text(sid)?));
    }
    Ok(out)
}

/// The SID texts of the access-allowed ACEs in the DACL of the file behind `h` (opened with `READ_CONTROL`). A file with
/// no DACL at all (everyone may do everything) is an error: no user can be named from it.
pub fn allowed_trustees(h: HANDLE) -> io::Result<Vec<String>> {
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut psd = PSECURITY_DESCRIPTOR::default();
    let r = unsafe {
        GetSecurityInfo(
            h,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(&mut dacl),
            None,
            Some(&mut psd),
        )
    };
    if r != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(r.0 as i32));
    }
    let _free = Descriptor(psd);
    if dacl.is_null() {
        return Err(io::Error::other("the file has no DACL"));
    }
    Ok(aces(dacl)?
        .into_iter()
        .filter(|a| a.0 == ALLOWED_ACE)
        .map(|a| a.2)
        .collect())
}

/// The user the events must belong to, from the DACL of the frame file at `path` (the file the caller has mapped:
/// `same_as` is its identity, and the file read here must be that very file). `Ok(None)` when the DACL names no single
/// user ([`expected_owner`]). The file is opened with `READ_CONTROL | FILE_READ_ATTRIBUTES` and nothing else, never
/// through a link, and, with `pin`, only when it really is `path` below the trusted folder `pin`.
pub fn frames_expected_owner(
    path: &Path,
    pin: &Path,
    same_as: secure::FileIdentity,
) -> io::Result<Option<String>> {
    let h = secure::open_existing_for(
        path,
        secure::ACCESS_OWNER_CHECK | secure::ACCESS_ATTRIBUTES_ONLY,
        Some(pin),
    )?;
    let raw = secure::raw(&h);
    if secure::file_identity(raw)? != same_as {
        return Err(secure::refused(
            "the frame file at the path is not the file that is mapped",
        ));
    }
    Ok(expected_owner(&allowed_trustees(raw)?))
}

/// A SID parsed from its text, freed on drop.
struct Sid(PSID);

impl Sid {
    fn parse(text: &str) -> io::Result<Sid> {
        let w: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let mut p = PSID::default();
        unsafe { ConvertStringSidToSidW(PCWSTR(w.as_ptr()), &mut p) }.map_err(win_err)?;
        Ok(Sid(p))
    }
}

impl Drop for Sid {
    fn drop(&mut self) {
        unsafe {
            let _ = LocalFree(Some(HLOCAL(self.0 .0)));
        }
    }
}

/// Whether the event behind `h` (opened with `READ_CONTROL`) is the writer's: owned by `expected` and carrying an explicit
/// mandatory label of at least medium integrity. `Err` says what is wrong (or that it could not be read), and any
/// error means "do not trust it".
pub fn check_event(h: HANDLE, expected: &str) -> Result<(), String> {
    if !is_user_sid_shape(expected) {
        return Err(format!("{expected} is not the SID of a user account"));
    }
    let want = Sid::parse(expected).map_err(|e| format!("the expected owner: {e}"))?;
    let mut owner = PSID::default();
    let mut sacl: *mut ACL = std::ptr::null_mut();
    let mut psd = PSECURITY_DESCRIPTOR::default();
    let r = unsafe {
        GetSecurityInfo(
            h,
            SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION | OBJECT_SECURITY_INFORMATION(LABEL_INFO),
            Some(&mut owner),
            None,
            None,
            Some(&mut sacl),
            Some(&mut psd),
        )
    };
    if r != ERROR_SUCCESS {
        return Err(format!("GetSecurityInfo: error {}", r.0));
    }
    let _free = Descriptor(psd);
    if owner.0.is_null() {
        return Err("the event has no owner".into());
    }
    if unsafe { EqualSid(owner, want.0) }.is_err() {
        let got = sid_text(owner).unwrap_or_else(|_| "?".into());
        return Err(format!("the event is owned by {got}, not by {expected}"));
    }
    let labels: Vec<u32> = aces(sacl)
        .map_err(|e| format!("reading the label: {e}"))?
        .into_iter()
        .filter(|a| a.0 == LABEL_ACE)
        .filter_map(|a| {
            a.2.strip_prefix("S-1-16-")
                .and_then(|rid| rid.parse::<u32>().ok())
        })
        .collect();
    match labels.as_slice() {
        [] => Err("the event has no explicit mandatory label".into()),
        [rid] if *rid >= MEDIUM_RID => Ok(()),
        [rid] => Err(format!(
            "the event is labelled below medium integrity (S-1-16-{rid})"
        )),
        _ => Err("the event has more than one mandatory label".into()),
    }
}

/// Open both events (`nonces[i]` names event `.i`) with `SYNCHRONIZE | READ_CONTROL` and check each handle with
/// [`check_event`] against `expected`. Fails when a nonce is 0, when either event does not exist or may not be opened,
/// when one is not an event, or when one is not the writer's. Nothing is left open on failure. Untrusted objects are
/// reported as [`io::ErrorKind::PermissionDenied`], a failure to open as what the system said.
pub fn open_checked_events(nonces: [u128; 2], expected: &str) -> io::Result<[OwnedHandle; 2]> {
    if nonces.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a zero nonce names no event",
        ));
    }
    let open = |parity: usize| -> io::Result<OwnedHandle> {
        let name = event_name_wide(nonces[parity], parity);
        let h = unsafe {
            OpenEventW(
                SYNCHRONIZATION_ACCESS_RIGHTS(READER_ACCESS),
                false,
                PCWSTR(name.as_ptr()),
            )
        }
        .map_err(win_err)?;
        // SAFETY: a new handle that nothing else owns; closed on every path out.
        let h = unsafe { OwnedHandle::from_raw_handle(h.0) };
        check_event(HANDLE(h.as_raw_handle()), expected).map_err(|why| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("event .{parity}: {why}"),
            )
        })?;
        Ok(h)
    };
    let e0 = open(0)?;
    let e1 = open(1)?;
    Ok([e0, e1])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signal::{
        current_user_sid_text, descriptor_text, make_event_for_test, system_nonce, FrameSignal,
    };
    use std::path::PathBuf;
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows::Win32::Storage::FileSystem::{CREATE_NEW, FILE_ATTRIBUTE_NORMAL};
    use windows::Win32::System::Threading::{CreateMutexW, SetEvent};

    const USER: &str = "S-1-5-21-1111111111-2222222222-3333333333-1001";

    fn two() -> [u128; 2] {
        [system_nonce().unwrap(), system_nonce().unwrap()]
    }

    fn me() -> String {
        current_user_sid_text().unwrap()
    }

    /// The descriptor of the design with `label` as its SACL text.
    fn descriptor_with_label(label: &str) -> String {
        let full = descriptor_text(&me());
        let (head, _) = full.split_once("S:").unwrap();
        format!("{head}S:{label}")
    }

    fn denied_because(r: io::Result<[OwnedHandle; 2]>, why: &str) {
        let e = r.expect_err("the events were accepted");
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied, "{e}");
        assert!(e.to_string().contains(why), "'{e}' does not say '{why}'");
    }

    #[test]
    fn events_made_by_the_writer_pass_for_their_owner_and_for_nobody_else() {
        let s = FrameSignal::create().unwrap();
        let ev = open_checked_events(s.nonces(), &me()).expect("the writer's own events");
        // the handles are the events (the set is seen through them)
        unsafe { SetEvent(s.handle(0)) }.unwrap();
        assert!(crate::signal::tests::is_set(HANDLE(ev[0].as_raw_handle())));
        assert!(!crate::signal::tests::is_set(HANDLE(ev[1].as_raw_handle())));
        // another user's SID as the expected owner: refused, and nothing stays open
        denied_because(
            open_checked_events(s.nonces(), "S-1-5-21-1-2-3-4"),
            "is owned by",
        );
        denied_because(open_checked_events(s.nonces(), "S-1-5-21-1-2-3-4"), &me());
        // an expected owner that is no user account is refused before anything is opened
        for bad in ["S-1-5-18", "S-1-5-19", "S-1-1-0", "S-1-5-32-544", "", "x"] {
            denied_because(
                open_checked_events(s.nonces(), bad),
                "not the SID of a user",
            );
        }
        // a zero nonce names no event
        assert_eq!(
            open_checked_events([0, s.nonces()[1]], &me())
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        // the handle the check reads from may read the descriptor and wait, but not signal
        assert!(unsafe { SetEvent(HANDLE(ev[0].as_raw_handle())) }.is_err());
    }

    #[test]
    fn an_event_that_somebody_else_made_under_the_name_is_not_trusted() {
        let user = me();
        // the same owner and the same descriptor, except for the label
        for (what, label, why) in [
            (
                "low integrity",
                "(ML;;NW;;;LW)",
                "labelled below medium integrity",
            ),
            (
                "untrusted integrity",
                "(ML;;NW;;;S-1-16-0)",
                "labelled below medium integrity",
            ),
            ("medium plus", "(ML;;NW;;;S-1-16-8448)", ""),
            ("no label", "", "no explicit mandatory label"),
        ] {
            let n = two();
            let sddl = if label.is_empty() {
                // no S: part at all
                let full = descriptor_text(&user);
                full.split_once("S:").unwrap().0.to_string()
            } else {
                descriptor_with_label(label)
            };
            let _e0 = make_event_for_test(n[0], 0, Some(&sddl));
            let _e1 = make_event_for_test(n[1], 1, Some(&sddl));
            let (Ok(_e0), Ok(_e1)) = (_e0, _e1) else {
                // a label above the caller's own level cannot be set without a privilege: nothing to test then
                assert_eq!(what, "medium plus", "{what}: could not make the event");
                continue;
            };
            let r = open_checked_events(n, &user);
            if why.is_empty() {
                assert!(r.is_ok(), "{what}: {:?}", r.err());
            } else {
                denied_because(r, why);
            }
        }
        // medium, as the writer makes it, with no write-up bit: the level is what counts
        let n = two();
        let sddl = descriptor_with_label("(ML;;;;;ME)");
        let _a = make_event_for_test(n[0], 0, Some(&sddl)).unwrap();
        let _b = make_event_for_test(n[1], 1, Some(&sddl)).unwrap();
        assert!(open_checked_events(n, &user).is_ok());
    }

    #[test]
    fn one_good_event_and_one_that_is_not_make_the_pair_untrusted() {
        let user = me();
        let n = two();
        let good = descriptor_text(&user);
        let low = descriptor_with_label("(ML;;NW;;;LW)");
        let _e0 = make_event_for_test(n[0], 0, Some(&good)).unwrap();
        let _e1 = make_event_for_test(n[1], 1, Some(&low)).unwrap();
        denied_because(open_checked_events(n, &user), "event .1");
        let m = two();
        let _e0 = make_event_for_test(m[0], 0, Some(&low)).unwrap();
        let _e1 = make_event_for_test(m[1], 1, Some(&good)).unwrap();
        denied_because(open_checked_events(m, &user), "event .0");
    }

    #[test]
    fn an_event_that_does_not_let_the_reader_read_its_owner_is_not_opened() {
        // no READ_CONTROL for anyone (OWNER RIGHTS with no right takes the owner's implicit one away): the reader's
        // open, which asks for it, is refused, so the owner can never be read from an object that hides it
        let user = me();
        let n = two();
        let hidden = format!(
            "O:{user}D:P(A;;0x00100002;;;{user})(A;;0x00100000;;;S-1-5-19)(A;;0x00000000;;;S-1-3-4)S:(ML;;NW;;;ME)"
        );
        let _a = make_event_for_test(n[0], 0, Some(&hidden)).unwrap();
        let _b = make_event_for_test(n[1], 1, Some(&hidden)).unwrap();
        let e = open_checked_events(n, &user).expect_err("opened without READ_CONTROL");
        assert_eq!(
            e.raw_os_error(),
            Some(5),
            "ERROR_ACCESS_DENIED expected: {e}"
        );
    }

    #[test]
    fn an_object_of_another_type_or_no_object_is_not_an_event() {
        let user = me();
        // nothing there
        let e = open_checked_events(two(), &user).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::NotFound, "{e}");
        // a mutex under the name of event .0
        let n = two();
        let name = crate::signal::event_name_wide(n[0], 0);
        let mutex = unsafe { CreateMutexW(None, false, PCWSTR(name.as_ptr())) }.unwrap();
        let _mutex = unsafe { OwnedHandle::from_raw_handle(mutex.0) };
        let _e1 = make_event_for_test(n[1], 1, None).unwrap();
        let e = open_checked_events(n, &user).unwrap_err();
        assert_eq!(e.raw_os_error(), Some(6), "ERROR_INVALID_HANDLE: {e}");
    }

    // ---- the DACL of the frame file ---------------------------------------------------------------------------------

    fn frame_file(name: &str, sddl: &str) -> (PathBuf, PathBuf, OwnedHandle) {
        let root = crate::test_dir("owner");
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join(name);
        let _ = std::fs::remove_file(&path);
        let w: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
        let mut psd = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(w.as_ptr()),
                SDDL_REVISION_1,
                &mut psd,
                None,
            )
        }
        .unwrap();
        let sd = Descriptor(psd);
        let h = unsafe {
            secure::open_with_sd(
                &path,
                secure::ACCESS_READ_WRITE,
                secure::SHARE_ALL,
                CREATE_NEW,
                false,
                FILE_ATTRIBUTE_NORMAL.0,
                Some(sd.0 .0 as *const c_void),
            )
        }
        .unwrap();
        (root, path, h)
    }

    fn identity_of(h: &OwnedHandle) -> secure::FileIdentity {
        secure::file_identity(secure::raw(h)).unwrap()
    }

    #[test]
    fn the_expected_owner_is_read_from_the_dacl_of_the_frame_file() {
        let user = me();
        // the installer's ACL
        let installer =
            format!("D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;0x12008f;;;LS)(A;;0x12008f;;;{user})");
        let (root, path, h) = frame_file("owner-installer.bin", &installer);
        let r = frames_expected_owner(&path, &root, identity_of(&h)).unwrap();
        assert_eq!(r.as_deref(), Some(user.as_str()));
        // an installer without a user
        let (root, path, h) = frame_file(
            "owner-nouser.bin",
            "D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;0x12008f;;;LS)(A;;0x12008f;;;S-1-5-32-545)",
        );
        // (the creator reads the descriptor through the Users group it belongs to; that group is no user account)
        assert_eq!(
            frames_expected_owner(&path, &root, identity_of(&h)).unwrap(),
            None
        );
        // two users: nobody
        let two_users = format!(
            "D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;0x12008f;;;LS)(A;;0x12008f;;;{user})(A;;0x12008f;;;S-1-5-21-1-2-3-1002)"
        );
        let (root, path, h) = frame_file("owner-two.bin", &two_users);
        assert_eq!(
            frames_expected_owner(&path, &root, identity_of(&h)).unwrap(),
            None
        );
        // a denied ACE does not add a trustee
        let with_deny = format!(
            "D:P(D;;0x2;;;S-1-5-21-1-2-3-1002)(A;;FA;;;SY)(A;;FA;;;BA)(A;;0x12008f;;;LS)(A;;0x12008f;;;{user})"
        );
        let (root, path, h) = frame_file("owner-deny.bin", &with_deny);
        assert_eq!(
            frames_expected_owner(&path, &root, identity_of(&h))
                .unwrap()
                .as_deref(),
            Some(user.as_str())
        );
        // the file at the path is not the file that was mapped: refused
        let (root, path, _h) = frame_file("owner-other.bin", &installer);
        let (_r, _p, mapped) = frame_file("owner-mapped.bin", &installer);
        let e = frames_expected_owner(&path, &root, identity_of(&mapped)).unwrap_err();
        assert!(secure::is_refused(&e), "{e}");
        // a path that is not below the pin is refused
        let (_root2, path, h) = frame_file("owner-pin.bin", &installer);
        let elsewhere = std::env::temp_dir().join("bettercam-owner-no-such-root");
        let e = frames_expected_owner(&path, &elsewhere, identity_of(&h)).unwrap_err();
        assert!(secure::is_refused(&e), "{e}");
    }

    #[test]
    fn a_frame_file_without_a_dacl_names_nobody() {
        // a NULL DACL grants everything to everyone: there is no user to name, and reading it is an error
        let (root, path, h) = frame_file("owner-null.bin", "D:NO_ACCESS_CONTROL");
        let e = frames_expected_owner(&path, &root, identity_of(&h)).unwrap_err();
        assert!(e.to_string().contains("no DACL"), "{e}");
    }

    #[test]
    fn the_shape_is_the_one_the_installer_accepts() {
        for ok in [
            USER,
            "S-1-5-21-0-0-0-500",
            "S-1-12-1-1-2-3-4",
            "S-1-5-21-4294967295-1-2-3",
        ] {
            assert!(is_user_sid_shape(ok), "{ok}");
        }
        for bad in [
            "",
            "S-1-1-0",
            "S-1-5-18",
            "S-1-5-19",
            "S-1-5-32-544",
            "S-1-5-32-545",
            "S-1-5-21-1-2-3",
            "S-1-5-21-1-2-3-4-5",
            "S-1-5-21-1-2-3-04",
            "S-1-5-21-1-2-3-4294967296",
            "S-1-5-21-1-2-3-+4",
            "S-1-5-21-1-2-3-",
            "s-1-5-21-1-2-3-4",
            "S-1-5-21-1-2-3-4)(A;;GA;;;WD",
            "S-1-12-1-1-2-3",
        ] {
            assert!(!is_user_sid_shape(bad), "{bad:?}");
        }
    }

    #[test]
    fn the_expected_owner_is_the_single_user_in_the_dacl() {
        let sys = ["S-1-5-18", "S-1-5-32-544", "S-1-5-19"];
        // the installer's DACL: three well-known trustees and the user
        let mut dacl: Vec<&str> = sys.to_vec();
        dacl.push(USER);
        assert_eq!(expected_owner(&dacl).as_deref(), Some(USER));
        // order and a repeated ACE of the same user do not matter
        assert_eq!(
            expected_owner(&[USER, "S-1-5-19", USER, "S-1-5-18"]).as_deref(),
            Some(USER)
        );
        // an install made without a user: nobody to trust
        assert_eq!(expected_owner(&sys), None);
        assert_eq!(expected_owner::<&str>(&[]), None);
        // two users (or any second trustee): ambiguous, so nobody
        assert_eq!(
            expected_owner(&[USER, "S-1-5-21-1-2-3-1002", "S-1-5-18"]),
            None
        );
        assert_eq!(expected_owner(&[USER, "S-1-1-0"]), None, "Everyone as well");
        assert_eq!(expected_owner(&[USER, "S-1-5-11"]), None);
        // a lone trustee that is not a user account is not accepted either
        for other in ["S-1-1-0", "S-1-5-32-545", "S-1-5-21-1-2-3", "garbage"] {
            assert_eq!(expected_owner(&[other]), None, "{other}");
        }
    }
}
