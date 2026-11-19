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
    EqualSid, GetAce, IsValidSid, ACL, DACL_SECURITY_INFORMATION, OBJECT_SECURITY_INFORMATION,
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

/// `ACCESS_ALLOWED_ACE` and `SYSTEM_MANDATORY_LABEL_ACE` start alike: a header (type, flags, size: 4 bytes), a mask
/// (4 bytes), then the SID. Other types (denied, audit, the object ACEs) are laid out differently or are not read at all.
const ACE_SID_OFFSET: usize = 8;

/// The smallest SID: revision, sub-authority count and the 6-byte authority.
const SID_FIXED: usize = 8;

/// The `i`-th ACE of `acl` as (pointer, type, size), after checking that its 4-byte header lies inside the list and that
/// its size field is at least the header and reaches no further than the end of the list. **Every** ACE is checked,
/// whatever its type, before anything of it is read: a list that holds one that does not fit is an error.
fn ace_header(acl: *mut ACL, acl_size: usize, i: u32) -> io::Result<(*const u8, u8, usize)> {
    let mut p: *mut c_void = std::ptr::null_mut();
    unsafe { GetAce(acl, i, &mut p) }.map_err(win_err)?;
    let ace = p as *const u8;
    let at = (ace as usize).wrapping_sub(acl as usize);
    if !at.checked_add(4).is_some_and(|end| end <= acl_size) {
        return Err(io::Error::other("an ACE header lies outside its list"));
    }
    // SAFETY: the 4-byte header (the part every ACE has) is inside the list (checked above).
    let (ty, size) = unsafe {
        (
            *ace,
            u16::from_le_bytes([*ace.add(2), *ace.add(3)]) as usize,
        )
    };
    if size < 4 || size > acl_size - at {
        return Err(io::Error::other("an ACE does not fit inside its list"));
    }
    Ok((ace, ty, size))
}

/// `ACCESS_ALLOWED_COMPOUND`, `_OBJECT`, `_CALLBACK` and `_CALLBACK_OBJECT`: access-allowed ACEs of other layouts, that
/// [`aces`] does not read. A DACL that holds one grants access to somebody this module did not see.
const OTHER_ALLOWED_ACES: [u8; 4] = [4, 5, 9, 0xB];

/// `(mask, sid text)` of each ACE of `acl` that is of type `kind` (a null list has none). **Only** an ACE of that type
/// is looked at beyond its header, and its SID must lie inside the ACE and inside the list: an ACE of another type, with
/// whatever it holds in the place of a SID, is skipped without a read; one that does not fit (of any type) is an error.
fn aces(acl: *mut ACL, kind: u8) -> io::Result<Vec<(u32, String)>> {
    let mut out = Vec::new();
    if acl.is_null() {
        return Ok(out);
    }
    // SAFETY: a list that `GetSecurityInfo` (or a caller that built one) gave: its header is readable.
    let (count, acl_size) = unsafe { ((*acl).AceCount, (*acl).AclSize as usize) };
    for i in 0..count as u32 {
        let (ace, ty, size) = ace_header(acl, acl_size, i)?;
        if ty != kind {
            continue;
        }
        if size < ACE_SID_OFFSET + SID_FIXED {
            return Err(io::Error::other("an ACE does not fit inside its list"));
        }
        // SAFETY: the ACE has at least `ACE_SID_OFFSET + SID_FIXED` bytes (checked above).
        let sub_authorities = unsafe { *ace.add(ACE_SID_OFFSET + 1) } as usize;
        if size < ACE_SID_OFFSET + SID_FIXED + 4 * sub_authorities {
            return Err(io::Error::other("the SID of an ACE does not fit inside it"));
        }
        // SAFETY: the mask and the whole SID are inside the ACE (checked above).
        let mask = unsafe { (ace.add(4) as *const u32).read_unaligned() };
        let sid = PSID(unsafe { ace.add(ACE_SID_OFFSET) } as *mut c_void);
        if !unsafe { IsValidSid(sid) }.as_bool() {
            return Err(io::Error::other("an ACE holds no valid SID"));
        }
        out.push((mask, sid_text(sid)?));
    }
    Ok(out)
}

/// The SID texts of the access-allowed ACEs of the DACL `acl`; `None` when it also holds an access-allowed ACE of another
/// layout ([`OTHER_ALLOWED_ACES`]), whose trustee is not read here: the list then names more than it shows, so no user
/// can be named from it. An ACE that does not fit is an error.
fn dacl_trustees(acl: *mut ACL) -> io::Result<Option<Vec<String>>> {
    let trustees: Vec<String> = aces(acl, ALLOWED_ACE)?.into_iter().map(|a| a.1).collect();
    // SAFETY: `aces` has just walked this list (a null one has no ACEs): its header is readable.
    let (count, acl_size) = if acl.is_null() {
        (0, 0)
    } else {
        unsafe { ((*acl).AceCount, (*acl).AclSize as usize) }
    };
    for i in 0..count as u32 {
        let (_, ty, _) = ace_header(acl, acl_size, i)?;
        if OTHER_ALLOWED_ACES.contains(&ty) {
            return Ok(None);
        }
    }
    Ok(Some(trustees))
}

/// The SID texts of the access-allowed ACEs in the DACL of the file behind `h` (opened with `READ_CONTROL`); `None` when
/// the DACL also holds an access-allowed ACE of a layout that is not read ([`dacl_trustees`]). A file with no DACL at
/// all (everyone may do everything) is an error: no user can be named from it.
pub fn allowed_trustees(h: HANDLE) -> io::Result<Option<Vec<String>>> {
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
    dacl_trustees(dacl)
}

/// The user the events must belong to, from the DACL of the frame file at `path` (the file the caller has mapped:
/// `same_as` is its identity, and the file read here must be that very file). `Ok(None)` when the DACL names no single
/// user ([`expected_owner`]) or holds an access-allowed ACE of a layout that is not read ([`allowed_trustees`]). The file is opened with `READ_CONTROL | FILE_READ_ATTRIBUTES` and nothing else, never
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
    Ok(allowed_trustees(raw)?.and_then(|t| expected_owner(&t)))
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
    let labels: Vec<u32> = aces(sacl, LABEL_ACE)
        .map_err(|e| format!("reading the label: {e}"))?
        .into_iter()
        .filter_map(|a| {
            a.1.strip_prefix("S-1-16-")
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

    /// A list of the given complete ACEs, in memory that is 4-byte aligned like the system's.
    fn acl_of(aces: &[Vec<u8>]) -> Vec<u32> {
        let body: usize = aces.iter().map(Vec::len).sum();
        let size = 8 + body;
        let mut bytes = vec![2u8, 0];
        bytes.extend((size as u16).to_le_bytes());
        bytes.extend((aces.len() as u16).to_le_bytes());
        bytes.extend([0, 0]);
        for a in aces {
            bytes.extend(a);
        }
        bytes.resize(size.div_ceil(4) * 4, 0);
        bytes
            .chunks(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect()
    }

    /// An ACE of `kind` with `mask` and the SID `sid` (bytes), its size field `size` (the real size when `None`).
    fn ace(kind: u8, mask: u32, sid: &[u8], size: Option<u16>) -> Vec<u8> {
        let real = 8 + sid.len();
        let mut a = vec![kind, 0];
        a.extend(size.unwrap_or(real as u16).to_le_bytes());
        a.extend(mask.to_le_bytes());
        a.extend(sid);
        a
    }

    /// `S-1-5-18` as bytes.
    const SYSTEM_SID: [u8; 12] = [1, 1, 0, 0, 0, 0, 0, 5, 18, 0, 0, 0];

    fn read(acl: &mut [u32], kind: u8) -> io::Result<Vec<(u32, String)>> {
        aces(acl.as_mut_ptr() as *mut ACL, kind)
    }

    #[test]
    fn only_aces_of_the_wanted_type_are_read_and_their_sid_must_fit() {
        // an ACE of another type is skipped without a look at what it holds where a SID would be: a denied ACE (type 1)
        // with 4 bytes of rubbish after its mask, and an audit ACE (type 2) that is only a header and a mask
        let mut acl = acl_of(&[
            ace(1, 2, &[0xff, 0xff, 0xff, 0xff], None),
            ace(2, 2, &[], None),
            ace(0, 0x12008f, &SYSTEM_SID, None),
        ]);
        assert_eq!(
            read(&mut acl, ALLOWED_ACE).unwrap(),
            vec![(0x12008f, "S-1-5-18".to_string())]
        );
        // the same list read for labels has none
        assert!(read(&mut acl, LABEL_ACE).unwrap().is_empty());
        // a wanted ACE whose size is too small for its SID is an error, not a read past it
        let mut short = acl_of(&[ace(0, 1, &SYSTEM_SID, Some(12))]);
        assert!(read(&mut short, ALLOWED_ACE).is_err());
        // a count of sub-authorities that is larger than the ACE (a SID of 15 of them in an ACE that holds one)
        let mut lying = SYSTEM_SID;
        lying[1] = 15;
        let mut acl = acl_of(&[ace(0, 1, &lying, None)]);
        assert!(read(&mut acl, ALLOWED_ACE).is_err());
        // an ACE that claims to reach past the end of its list
        let mut acl = acl_of(&[ace(0, 1, &SYSTEM_SID, Some(60))]);
        assert!(read(&mut acl, ALLOWED_ACE).is_err());
        // no SID at all (revision 0) in an ACE of the wanted type
        let mut bad = SYSTEM_SID;
        bad[0] = 0;
        let mut acl = acl_of(&[ace(0, 1, &bad, None)]);
        assert!(read(&mut acl, ALLOWED_ACE).is_err());
        // a list with nothing in it, and no list
        assert!(read(&mut acl_of(&[]), ALLOWED_ACE).unwrap().is_empty());
        assert!(aces(std::ptr::null_mut(), ALLOWED_ACE).unwrap().is_empty());
    }

    #[test]
    fn every_ace_must_fit_its_list_whatever_its_type() {
        // an ACE of a type that is not wanted is still checked: it is not skipped on the word of its own header
        // (a size of 0 would otherwise be walked past, and a header that is cut off by the end of the list read)
        let mut zero = acl_of(&[
            ace(0, 0x12008f, &SYSTEM_SID, None),
            ace(1, 2, &SYSTEM_SID, Some(0)),
        ]);
        assert!(read(&mut zero, LABEL_ACE).is_err(), "size 0, not wanted");
        assert!(read(&mut zero, ALLOWED_ACE).is_err(), "size 0, other");
        // a size that is smaller than the header
        let mut tiny = acl_of(&[ace(1, 2, &SYSTEM_SID, Some(3))]);
        assert!(read(&mut tiny, LABEL_ACE).is_err(), "size 3");
        // a size that reaches past the end of the list
        let mut past = acl_of(&[ace(1, 2, &SYSTEM_SID, Some(500))]);
        assert!(read(&mut past, LABEL_ACE).is_err(), "size past the end");
        // the last ACE starts within 3 bytes of the end of the list: its header is not all inside it
        for rest in [1usize, 2, 3] {
            let mut cut = acl_of(&[
                ace(0, 0x12008f, &SYSTEM_SID, None),
                vec![0u8; rest], // a cut-off ACE of type 0
            ]);
            assert!(read(&mut cut, LABEL_ACE).is_err(), "{rest} bytes left");
            assert!(read(&mut cut, ALLOWED_ACE).is_err(), "{rest} bytes left");
        }
        // a last ACE that is just a header (4 bytes, of another type) does fit
        let mut header_only = acl_of(&[ace(0, 0x12008f, &SYSTEM_SID, None), vec![1, 0, 4, 0]]);
        assert_eq!(read(&mut header_only, ALLOWED_ACE).unwrap().len(), 1);
    }

    #[test]
    fn a_dacl_with_an_allowed_ace_of_another_layout_names_nobody() {
        let user = ace(0, 0x12008f, &SYSTEM_SID, None);
        let mut plain = acl_of(std::slice::from_ref(&user));
        let names = dacl_trustees(plain.as_mut_ptr() as *mut ACL).unwrap();
        assert_eq!(names, Some(vec!["S-1-5-18".to_string()]));
        // compound (4), object (5), callback (9) and callback object (0xB): each makes the list unreadable for the check
        for ty in OTHER_ALLOWED_ACES {
            let mut acl = acl_of(&[user.clone(), ace(ty, 0x12008f, &SYSTEM_SID, None)]);
            assert_eq!(
                dacl_trustees(acl.as_mut_ptr() as *mut ACL).unwrap(),
                None,
                "ACE type {ty:#x}"
            );
        }
        // a denied ACE (1), an audit ACE (2) and a denied callback ACE (6) are not access that is given
        for ty in [1u8, 2, 6] {
            let mut acl = acl_of(&[user.clone(), ace(ty, 2, &SYSTEM_SID, None)]);
            assert_eq!(
                dacl_trustees(acl.as_mut_ptr() as *mut ACL).unwrap(),
                Some(vec!["S-1-5-18".to_string()]),
                "ACE type {ty:#x}"
            );
        }
        // a null list has none
        assert_eq!(dacl_trustees(std::ptr::null_mut()).unwrap(), Some(vec![]));
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
