//! Permissions of `%ProgramData%\BetterCam`, `logs` and `frames.bin`.
//!
//! The folder is shared by three accounts of different trust: the installer (an administrator), LOCAL SERVICE (Frame
//! Server, which runs the DLL), and the user who runs the app. The user is the one who must not be able to rearrange
//! anything: the elevated helper works in these folders and a junction planted by the user would send it somewhere
//! else. So the user gets exactly what the app needs and nothing that creates, deletes or renames:
//!
//! | object | owner | ACE (all protected, no inheritance from `%ProgramData%`) |
//! |---|---|---|
//! | `BetterCam` | Administrators | SYSTEM, Administrators: full, this folder only. LOCAL SERVICE, user: read and traverse, this folder only |
//! | `logs` | Administrators | SYSTEM, Administrators: full, inherited. LOCAL SERVICE: read, traverse and create files here; modify on files created here (`vcam-<pid>.log`). User: read and traverse here, **no** access to the files in it |
//! | `logs\install.log` | Administrators | SYSTEM, Administrators: full. User: read. Protected: nothing is inherited from `logs`, so LOCAL SERVICE cannot touch it |
//! | `frames.bin` | Administrators | SYSTEM, Administrators: full. LOCAL SERVICE, user: read and write *data* (no delete, no change of permissions, no write of attributes or extended attributes; sparse, compressed and extra streams are *not* prevented, see [`FRAMES_READ_WRITE`]) |
//!
//! The user has no access to `vcam-*.log` because a read right is all it takes to hold an opportunistic lock on a
//! file that blocks every other open of it; and the frame file gives no `FILE_WRITE_ATTRIBUTES` because that right
//! lets a user hard-link the file from a folder of their own or set it read-only, after which the DLL and the app
//! refuse it until a Repair. (Making it sparse or compressed, or giving it a data stream, only needs the data rights
//! the user has; the installer puts those right again.)
//!
//! Everything here is applied and read back through a handle that [`vcam_protocol::secure`] verified, never by path.
//! A new object is created with its final DACL in the create call ([`SecurityDescriptor`]), so it never exists with
//! the permissions it would inherit.

use std::os::windows::io::{AsRawHandle, OwnedHandle};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{LocalFree, ERROR_SUCCESS, HANDLE, HLOCAL};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
    ConvertStringSidToSidW, GetSecurityInfo, SDDL_REVISION_1, SE_FILE_OBJECT,
};
use windows::Win32::Security::{
    EqualSid, GetAce, GetSecurityDescriptorControl, LookupAccountSidW, SetKernelObjectSecurity,
    ACL, DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION,
    PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SID_NAME_USE,
};

use crate::util::{pcwstr, werr, wide, Res};

pub const SID_SYSTEM: &str = "S-1-5-18";
pub const SID_LOCAL_SERVICE: &str = "S-1-5-19";
pub const SID_ADMINS: &str = "S-1-5-32-544";

// access masks (documented file and folder rights; see `winnt.h`)
/// `FILE_ALL_ACCESS`.
pub const FULL: u32 = 0x001f_01ff;
/// List the folder / read the file's data, read attributes and permissions, traverse. `FILE_GENERIC_READ` and
/// `FILE_GENERIC_EXECUTE` together.
pub const READ_TRAVERSE: u32 = 0x0012_00a9;
/// [`READ_TRAVERSE`] plus `FILE_ADD_FILE`: create files in the folder.
pub const READ_TRAVERSE_ADD_FILE: u32 = 0x0012_00ab;
/// `FILE_GENERIC_READ`.
pub const FILE_READ: u32 = 0x0012_0089;
/// What the frame file gives its two users: `SYNCHRONIZE | READ_CONTROL | FILE_READ_ATTRIBUTES | FILE_READ_EA |
/// FILE_APPEND_DATA | FILE_WRITE_DATA | FILE_READ_DATA`. A read-write mapping works with it. Missing on purpose:
/// `FILE_WRITE_ATTRIBUTES` (hard-link the file from another folder, set it read-only), `FILE_WRITE_EA`, `DELETE`,
/// `WRITE_DAC`, `WRITE_OWNER`.
///
/// What the mask does **not** prevent, because the right it is done with is one the mask has: `FSCTL_SET_SPARSE` (a
/// handle with `FILE_WRITE_DATA` is enough), `FSCTL_SET_COMPRESSION` (read and write data), and a data stream
/// (`frames.bin:name`, created through `FILE_WRITE_DATA`). None of the three bothers the DLL or the app; the
/// installer undoes them: a Repair clears the sparse and compressed state in place and replaces a file that has a
/// stream (see `datadir`).
pub const FRAMES_READ_WRITE: u32 = 0x0012_008f;
/// Read, write, execute and `DELETE` (a file the service made may be deleted by the service).
pub const FILE_MODIFY: u32 = 0x0013_01bf;

// ACE flags
pub const OBJECT_INHERIT: u8 = 0x01;
pub const CONTAINER_INHERIT: u8 = 0x02;
pub const INHERIT_ONLY: u8 = 0x08;
const INHERITED_ACE: u8 = 0x10;

/// One access-allowed ACE.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ace {
    /// The trustee as `S-1-...` text (a raw SID, never an SDDL alias).
    pub sid: String,
    pub mask: u32,
    /// `OBJECT_INHERIT | CONTAINER_INHERIT | INHERIT_ONLY` bits.
    pub flags: u8,
}

/// Who gets what. [`Principals::machine`] is the real set; tests name other accounts.
#[derive(Clone, Debug)]
pub struct Principals {
    pub system: String,
    pub admins: String,
    pub local_service: String,
    /// The user who runs the app; `None` leaves them out (then only LOCAL SERVICE and the administrators can use
    /// the folder).
    pub user: Option<String>,
    /// Owner every object is given.
    pub owner: String,
    /// Owners that an existing object may have and still be kept.
    pub trusted_owners: Vec<String>,
}

impl Principals {
    /// SYSTEM, Administrators, LOCAL SERVICE and `user`; the owner is Administrators, SYSTEM may own too.
    pub fn machine(user: Option<&str>) -> Principals {
        Principals {
            system: SID_SYSTEM.into(),
            admins: SID_ADMINS.into(),
            local_service: SID_LOCAL_SERVICE.into(),
            user: user.map(str::to_string),
            owner: SID_ADMINS.into(),
            trusted_owners: vec![SID_SYSTEM.into(), SID_ADMINS.into()],
        }
    }
}

/// The objects of the data folder that get permissions of their own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Object {
    DataDir,
    LogsDir,
    FramesFile,
    /// `logs\install.log`: the helper's own log, which the service must not be able to change.
    InstallLog,
}

impl Object {
    pub fn describe(self) -> &'static str {
        match self {
            Object::DataDir => "the data folder",
            Object::LogsDir => "the logs folder",
            Object::FramesFile => "frames.bin",
            Object::InstallLog => "install.log",
        }
    }

    /// A file, not a folder.
    pub fn is_file(self) -> bool {
        matches!(self, Object::FramesFile | Object::InstallLog)
    }
}

fn ace(sid: &str, mask: u32, flags: u8) -> Ace {
    Ace {
        sid: sid.to_string(),
        mask,
        flags,
    }
}

/// The DACL `obj` must have (see the table in the module docs).
pub fn expected(obj: Object, p: &Principals) -> Vec<Ace> {
    let both = OBJECT_INHERIT | CONTAINER_INHERIT;
    let mut v = Vec::new();
    match obj {
        Object::DataDir => {
            // Nothing is inherited: whatever is made in here later gets no access from this folder, and the
            // children that exist have DACLs of their own.
            v.push(ace(&p.system, FULL, 0));
            v.push(ace(&p.admins, FULL, 0));
            v.push(ace(&p.local_service, READ_TRAVERSE, 0));
            if let Some(u) = &p.user {
                v.push(ace(u, READ_TRAVERSE, 0));
            }
        }
        Object::LogsDir => {
            v.push(ace(&p.system, FULL, both));
            v.push(ace(&p.admins, FULL, both));
            // the service creates its log here ...
            v.push(ace(&p.local_service, READ_TRAVERSE_ADD_FILE, 0));
            // ... and owns what it created
            v.push(ace(
                &p.local_service,
                FILE_MODIFY,
                OBJECT_INHERIT | INHERIT_ONLY,
            ));
            if let Some(u) = &p.user {
                // list and traverse the folder only: no right on `vcam-*.log` at all (an open for reading is
                // enough to hold a lock on the file that makes every other open wait)
                v.push(ace(u, READ_TRAVERSE, 0));
            }
        }
        Object::FramesFile => {
            v.push(ace(&p.system, FULL, 0));
            v.push(ace(&p.admins, FULL, 0));
            v.push(ace(&p.local_service, FRAMES_READ_WRITE, 0));
            if let Some(u) = &p.user {
                v.push(ace(u, FRAMES_READ_WRITE, 0));
            }
        }
        Object::InstallLog => {
            // protected: LOCAL SERVICE's inheritable modify right on `logs` does not reach this file
            v.push(ace(&p.system, FULL, 0));
            v.push(ace(&p.admins, FULL, 0));
            if let Some(u) = &p.user {
                v.push(ace(u, FILE_READ, 0));
            }
        }
    }
    v
}

fn flag_text(flags: u8) -> String {
    let mut s = String::new();
    for (bit, t) in [
        (OBJECT_INHERIT, "OI"),
        (CONTAINER_INHERIT, "CI"),
        (INHERIT_ONLY, "IO"),
    ] {
        if flags & bit != 0 {
            s.push_str(t);
        }
    }
    s
}

/// The protected DACL as SDDL (hex masks, raw SIDs).
pub fn sddl(aces: &[Ace]) -> String {
    let mut s = String::from("D:P");
    for a in aces {
        s.push_str(&format!(
            "(A;{};0x{:x};;;{})",
            flag_text(a.flags),
            a.mask,
            a.sid
        ));
    }
    s
}

// ---------------------------------------------------------------------------------------------------------------
// SIDs

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

/// The SID of a user account in the text form `S-1-5-21-<domain 3 parts>-<rid>` (local, domain and Microsoft
/// accounts) or `S-1-12-1-<4 parts>` (Entra ID accounts), every part a plain 32-bit decimal.
///
/// Nothing else passes: not Everyone (`S-1-1-0`), not a group (`S-1-5-32-545`), not a part above `u32::MAX`
/// (which would only fail later, after the DLL was copied), nothing with characters that could change the meaning of
/// an SDDL string. Whether the account really is a *user* is checked with [`confirm_user_account`].
pub fn valid_sid(sid: &str) -> bool {
    let (prefix, count) = if let Some(rest) = sid.strip_prefix("S-1-5-21-") {
        (rest, 4)
    } else if let Some(rest) = sid.strip_prefix("S-1-12-1-") {
        (rest, 4)
    } else {
        return false;
    };
    let parts: Vec<&str> = prefix.split('-').collect();
    parts.len() == count && parts.iter().all(|p| sub_authority(p).is_some())
}

/// Whether Windows knows `sid` as a user account (`SidTypeUser`), not a group, alias or well-known SID. Call after
/// [`valid_sid`].
pub fn confirm_user_account(sid: &str) -> Res<()> {
    if !valid_sid(sid) {
        return Err(format!("'{sid}' is not a user SID"));
    }
    let s = SidBuf::parse(sid)?;
    let (mut nl, mut dl) = (0u32, 0u32);
    let mut kind = SID_NAME_USE::default();
    // first call: sizes
    let _ = unsafe {
        LookupAccountSidW(
            PCWSTR::null(),
            s.psid(),
            None,
            &mut nl,
            None,
            &mut dl,
            &mut kind,
        )
    };
    let mut name = vec![0u16; (nl as usize).max(1)];
    let mut dom = vec![0u16; (dl as usize).max(1)];
    unsafe {
        LookupAccountSidW(
            PCWSTR::null(),
            s.psid(),
            Some(windows::core::PWSTR(name.as_mut_ptr())),
            &mut nl,
            Some(windows::core::PWSTR(dom.as_mut_ptr())),
            &mut dl,
            &mut kind,
        )
    }
    .map_err(|e| format!("'{sid}' is not an account on this computer: {e}"))?;
    // SidTypeUser == 1
    if kind.0 == 1 {
        Ok(())
    } else {
        Err(format!(
            "'{sid}' is not a user account (Windows reports type {})",
            kind.0
        ))
    }
}

/// A SID converted from its text, freed on drop.
pub(crate) struct SidBuf(PSID);

impl SidBuf {
    pub(crate) fn parse(text: &str) -> Res<SidBuf> {
        let w = wide(text);
        let mut p = PSID::default();
        unsafe { ConvertStringSidToSidW(pcwstr(&w), &mut p) }
            .map_err(|e| werr(&format!("ConvertStringSidToSidW '{text}'"), e))?;
        Ok(SidBuf(p))
    }

    pub(crate) fn psid(&self) -> PSID {
        self.0
    }
}

impl Drop for SidBuf {
    fn drop(&mut self) {
        unsafe {
            let _ = LocalFree(Some(HLOCAL(self.0 .0)));
        }
    }
}

/// `S-1-...` text of a SID, whatever alias SDDL would print for it.
fn sid_text(p: PSID) -> Res<String> {
    let mut s = windows::core::PWSTR::null();
    unsafe { ConvertSidToStringSidW(p, &mut s) }.map_err(|e| werr("ConvertSidToStringSidW", e))?;
    let t = unsafe { s.to_string() }.map_err(|e| werr("sid utf16", e));
    unsafe {
        let _ = LocalFree(Some(HLOCAL(s.0 as *mut _)));
    }
    t
}

/// Two SID texts name the same SID (`EqualSid` on the converted values).
pub fn sid_eq(a: &str, b: &str) -> bool {
    match (SidBuf::parse(a), SidBuf::parse(b)) {
        (Ok(a), Ok(b)) => unsafe { EqualSid(a.psid(), b.psid()) }.is_ok(),
        _ => false,
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Applying and reading

/// Access an opened handle needs for [`apply`] and [`read`]: permissions to read, `WRITE_DAC` and `WRITE_OWNER`.
pub const SECURITY_ACCESS: u32 = 0x0002_0000 /* READ_CONTROL */ | 0x0004_0000 /* WRITE_DAC */ | 0x0008_0000 /* WRITE_OWNER */;

/// A security descriptor with the protected DACL of an object, to give to the create call of that object: it then
/// exists with its final permissions from its first moment (and not with whatever the folder around it would
/// hand down). Freed on drop.
pub struct SecurityDescriptor {
    psd: PSECURITY_DESCRIPTOR,
}

impl SecurityDescriptor {
    /// The descriptor for `obj`.
    pub fn for_object(obj: Object, p: &Principals) -> Res<SecurityDescriptor> {
        Self::from_aces(&expected(obj, p))
    }

    /// A protected DACL of exactly `aces`.
    pub fn from_aces(aces: &[Ace]) -> Res<SecurityDescriptor> {
        let text = sddl(aces);
        let w = wide(&text);
        let mut psd = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                pcwstr(&w),
                SDDL_REVISION_1,
                &mut psd,
                None,
            )
        }
        .map_err(|e| werr(&format!("security descriptor '{text}'"), e))?;
        Ok(SecurityDescriptor { psd })
    }

    /// For the `sd` argument of [`vcam_protocol::secure::open_with_sd`] and `create_dir_with_sd`; valid as long as
    /// `self` lives.
    pub fn as_ptr(&self) -> *const core::ffi::c_void {
        self.psd.0 as *const core::ffi::c_void
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        unsafe {
            let _ = LocalFree(Some(HLOCAL(self.psd.0)));
        }
    }
}

/// Set owner and the protected DACL of `obj` on `h`.
pub fn apply(h: &OwnedHandle, obj: Object, p: &Principals) -> Res<()> {
    apply_aces_to(h, &expected(obj, p), &p.owner)
        .map_err(|e| format!("setting the permissions of {}: {e}", obj.describe()))
}

/// Set the owner and a protected DACL of exactly `aces` on `h`, and nothing else.
///
/// This uses `SetKernelObjectSecurity` (one `NtSetSecurityObject` on the handle), *not* `SetSecurityInfo`: that one
/// walks the children of a folder and rewrites their inheritance, which would change the permissions of whatever a
/// user hard-linked into the folder under a name the helper looks at (the file is somewhere else too). The helper
/// changes the one object it opened and verified.
pub(crate) fn apply_aces_to(h: &OwnedHandle, aces: &[Ace], owner: &str) -> Res<()> {
    let text = format!("O:{owner}{}", sddl(aces));
    let w = wide(&text);
    let mut psd = PSECURITY_DESCRIPTOR::default();
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            pcwstr(&w),
            SDDL_REVISION_1,
            &mut psd,
            None,
        )
    }
    .map_err(|e| werr(&format!("security descriptor '{text}'"), e))?;
    let r = unsafe {
        SetKernelObjectSecurity(
            HANDLE(h.as_raw_handle()),
            OWNER_SECURITY_INFORMATION
                | DACL_SECURITY_INFORMATION
                | PROTECTED_DACL_SECURITY_INFORMATION,
            psd,
        )
    };
    unsafe {
        let _ = LocalFree(Some(HLOCAL(psd.0)));
    }
    r.map_err(|e| werr("SetKernelObjectSecurity", e))
}

/// [`apply_aces_to`] under the name the tests use (they set up the permissions an older version left).
#[cfg(test)]
pub(crate) fn apply_aces(h: &OwnedHandle, aces: &[Ace], owner: &str) -> Res<()> {
    apply_aces_to(h, aces, owner)
}

/// An ACE as stored on an object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredAce {
    /// `ACCESS_ALLOWED_ACE_TYPE` is 0, denied is 1.
    pub kind: u8,
    pub sid: String,
    pub mask: u32,
    pub flags: u8,
}

/// Owner and DACL of an object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub owner: String,
    /// `SE_DACL_PROTECTED`: the DACL does not inherit from the parent.
    pub protected: bool,
    pub aces: Vec<StoredAce>,
}

/// `ACCESS_ALLOWED_ACE`: header (type, flags, size), mask, then the SID.
#[repr(C)]
struct AllowedAce {
    kind: u8,
    flags: u8,
    size: u16,
    mask: u32,
    sid_start: u32,
}

/// Read owner and DACL through `h` (opened with [`SECURITY_ACCESS`] or at least `READ_CONTROL`).
pub fn read(h: &OwnedHandle) -> Res<Snapshot> {
    read_handle(HANDLE(h.as_raw_handle()))
}

/// [`read`] for a raw handle that somebody else owns.
pub fn read_handle(h: HANDLE) -> Res<Snapshot> {
    let mut owner = PSID::default();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut psd = PSECURITY_DESCRIPTOR::default();
    let r = unsafe {
        GetSecurityInfo(
            h,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            Some(&mut owner),
            None,
            Some(&mut dacl),
            None,
            Some(&mut psd),
        )
    };
    if r != ERROR_SUCCESS {
        return Err(format!("GetSecurityInfo: error {}", r.0));
    }
    let out = (|| {
        let mut control = 0u16;
        let mut revision = 0u32;
        unsafe { GetSecurityDescriptorControl(psd, &mut control, &mut revision) }
            .map_err(|e| werr("GetSecurityDescriptorControl", e))?;
        let mut aces = Vec::new();
        if !dacl.is_null() {
            let count = unsafe { (*dacl).AceCount };
            for i in 0..count as u32 {
                let mut p: *mut core::ffi::c_void = std::ptr::null_mut();
                unsafe { GetAce(dacl, i, &mut p) }.map_err(|e| werr("GetAce", e))?;
                // SAFETY: GetAce returned a pointer to the i-th ACE inside the ACL; only the common header and
                // mask of an allowed or denied ACE are read, and the SID starts at `sid_start`.
                let a = unsafe { &*(p as *const AllowedAce) };
                let sid = PSID(&a.sid_start as *const u32 as *mut core::ffi::c_void);
                aces.push(StoredAce {
                    kind: a.kind,
                    sid: sid_text(sid)?,
                    mask: a.mask,
                    flags: a.flags,
                });
            }
        }
        Ok(Snapshot {
            owner: sid_text(owner)?,
            // SE_DACL_PROTECTED
            protected: control & 0x1000 != 0,
            aces,
        })
    })();
    unsafe {
        let _ = LocalFree(Some(HLOCAL(psd.0)));
    }
    out
}

/// Why `snap` is not the DACL `want` (and the owner `owner`), or `Ok`. SIDs are compared with `EqualSid`, so an
/// account that SDDL prints as an alias (`LA`, `SY`, ...) is found like any other. Order does not matter; the
/// inherited flag is ignored; nothing besides the expected ACEs may be there.
pub fn check(snap: &Snapshot, want: &[Ace], owner: &str) -> Result<(), String> {
    if !sid_eq(&snap.owner, owner) {
        return Err(format!("owner is {}, not {owner}", snap.owner));
    }
    check_dacl(snap, want)
}

/// [`check`] without the owner: the DACL must be protected and consist of exactly `want`.
pub fn check_dacl(snap: &Snapshot, want: &[Ace]) -> Result<(), String> {
    if !snap.protected {
        return Err("the DACL inherits from the parent folder".into());
    }
    let mut left: Vec<&StoredAce> = snap.aces.iter().collect();
    for w in want {
        let at = left.iter().position(|a| {
            a.kind == 0
                && a.mask == w.mask
                && a.flags & !INHERITED_ACE == w.flags
                && sid_eq(&a.sid, &w.sid)
        });
        match at {
            Some(i) => {
                left.remove(i);
            }
            None => {
                return Err(format!(
                    "no ACE for {} with rights 0x{:x} and flags {}",
                    w.sid,
                    w.mask,
                    flag_text(w.flags)
                ))
            }
        }
    }
    if let Some(extra) = left.first() {
        return Err(format!(
            "unexpected ACE for {} (type {}, rights 0x{:x}, flags {})",
            extra.sid,
            extra.kind,
            extra.mask,
            flag_text(extra.flags)
        ));
    }
    Ok(())
}

/// True when `owner` is one of `trusted`.
pub fn owner_trusted(owner: &str, trusted: &[String]) -> bool {
    trusted.iter().any(|t| sid_eq(owner, t))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::fs::MetadataExt;

    const USER: &str = "S-1-5-21-1004336348-1177238915-682003330-1001";

    #[test]
    fn only_user_account_sids_pass() {
        assert!(valid_sid(USER));
        assert!(valid_sid("S-1-5-21-1-2-3-500"));
        assert!(valid_sid("S-1-5-21-4294967295-0-3-4294967295"));
        assert!(valid_sid("S-1-12-1-1-2-3-4"), "an Entra ID account");
        for bad in [
            // groups and well-known SIDs
            "S-1-1-0",
            "S-1-5-18",
            "S-1-5-19",
            "S-1-5-32-544",
            "S-1-5-32-545",
            "S-1-5-11",
            "S-1-5-21",
            "S-1-5-21-1-2-3",
            "S-1-12-1-1-2-3",
            // out of range or not canonical
            "S-1-5-21-1-2-3-4294967296",
            "S-1-5-21-99999999999-2-3-4",
            "S-1-5-21-1-2-3-04",
            "S-1-5-21-1-2-3--4",
            "S-1-5-21-1-2-3-+4",
            "S-1-5-21-1-2-3-4-5",
            // text
            "",
            "S-1-",
            "S-1-5-21-1-2-3-12a",
            "S-1-5-21-1-2-3-1)(A;;FA;;;WD",
            "s-1-5-21-1-2-3-4",
            "S-2-5-21-1-2-3-4",
            " S-1-5-21-1-2-3-4",
            "S-1-5-21-1-2-3-4 ",
            "S-1-5-21-1-2-3-4\n",
        ] {
            assert!(!valid_sid(bad), "'{bad}' must be refused");
        }
    }

    /// The SID of this machine's account `rid`, built from the current user's.
    fn machine_sid(rid: u32) -> String {
        let me = crate::current_user_sid().unwrap();
        let (domain, _) = me.rsplit_once('-').unwrap();
        format!("{domain}-{rid}")
    }

    #[test]
    fn a_group_of_this_machine_is_not_a_user_account() {
        let me = crate::current_user_sid().unwrap();
        assert!(valid_sid(&me));
        confirm_user_account(&me).unwrap();
        // the built-in Administrator and Guest accounts are users
        confirm_user_account(&machine_sid(500)).unwrap();
        // 513 is the group "None" on a computer that is not in a domain: a valid-looking SID, but a group
        let e = confirm_user_account(&machine_sid(513)).unwrap_err();
        assert!(e.contains("not a user account"), "{e}");
        // unknown to this machine
        assert!(confirm_user_account("S-1-5-21-1-2-3-4").is_err());
        // pure syntax is refused before any lookup
        assert!(confirm_user_account("S-1-5-32-545")
            .unwrap_err()
            .contains("not a user SID"));
    }

    fn principals() -> Principals {
        Principals::machine(Some(USER))
    }

    #[test]
    fn the_data_folder_acl_gives_the_user_no_way_to_rearrange_it() {
        let p = principals();
        let aces = expected(Object::DataDir, &p);
        assert_eq!(aces.len(), 4);
        assert_eq!(aces[0], ace(SID_SYSTEM, FULL, 0));
        assert_eq!(aces[1], ace(SID_ADMINS, FULL, 0));
        assert_eq!(aces[2], ace(SID_LOCAL_SERVICE, READ_TRAVERSE, 0));
        assert_eq!(aces[3], ace(USER, READ_TRAVERSE, 0));
        // nothing is inherited, and the user and the service only read and traverse: none of
        // FILE_ADD_FILE (2), FILE_ADD_SUBDIRECTORY (4), FILE_DELETE_CHILD (0x40), DELETE, WRITE_DAC, WRITE_OWNER
        for a in &aces[2..] {
            assert_eq!(a.flags, 0);
            assert_eq!(a.mask & (0x2 | 0x4 | 0x40 | 0x10000 | 0x40000 | 0x80000), 0);
        }
        assert_eq!(
            sddl(&aces),
            format!(
                "D:P(A;;0x1f01ff;;;S-1-5-18)(A;;0x1f01ff;;;S-1-5-32-544)(A;;0x1200a9;;;S-1-5-19)(A;;0x1200a9;;;{USER})"
            )
        );
        // without a user SID only three ACEs
        assert_eq!(
            expected(Object::DataDir, &Principals::machine(None)).len(),
            3
        );
    }

    #[test]
    fn the_logs_acl_lets_only_the_service_write_and_the_user_read_nothing_in_it() {
        let p = principals();
        let aces = expected(Object::LogsDir, &p);
        assert_eq!(aces.len(), 5);
        let both = OBJECT_INHERIT | CONTAINER_INHERIT;
        assert!(aces.contains(&ace(SID_SYSTEM, FULL, both)));
        assert!(aces.contains(&ace(SID_ADMINS, FULL, both)));
        assert!(aces.contains(&ace(SID_LOCAL_SERVICE, READ_TRAVERSE_ADD_FILE, 0)));
        assert!(aces.contains(&ace(
            SID_LOCAL_SERVICE,
            FILE_MODIFY,
            OBJECT_INHERIT | INHERIT_ONLY
        )));
        // the user: read and traverse the folder itself, and NOTHING on the files in it (an open for reading is
        // enough to hold an oplock that blocks every other open of the file): no inheritable ACE for the user,
        // none that gives ADD_FILE, DELETE or WRITE_*
        let mine: Vec<&Ace> = aces.iter().filter(|a| a.sid == USER).collect();
        assert_eq!(mine.len(), 1);
        assert_eq!(*mine[0], ace(USER, READ_TRAVERSE, 0));
        assert_eq!(mine[0].flags & (OBJECT_INHERIT | CONTAINER_INHERIT), 0);
        assert_eq!(
            mine[0].mask & (0x2 | 0x4 | 0x10 | 0x40 | 0x100 | 0x10000 | 0x40000 | 0x80000),
            0
        );
        // nothing inheritable reaches a file besides the administrators' and the service's own
        for a in aces
            .iter()
            .filter(|a| a.flags & OBJECT_INHERIT != 0 && a.sid != SID_SYSTEM && a.sid != SID_ADMINS)
        {
            assert_eq!(a.sid, SID_LOCAL_SERVICE);
        }
        assert_eq!(
            sddl(&aces),
            format!(
                "D:P(A;OICI;0x1f01ff;;;S-1-5-18)(A;OICI;0x1f01ff;;;S-1-5-32-544)(A;;0x1200ab;;;S-1-5-19)\
                 (A;OIIO;0x1301bf;;;S-1-5-19)(A;;0x1200a9;;;{USER})"
            )
        );
    }

    /// `install.log` has a DACL of its own so that LOCAL SERVICE's inheritable modify right on `logs` does not
    /// reach it, and the user can read it (the app shows its last error line).
    #[test]
    fn the_install_log_is_not_writable_by_the_service_and_readable_by_the_user() {
        let p = principals();
        let aces = expected(Object::InstallLog, &p);
        assert_eq!(
            aces,
            vec![
                ace(SID_SYSTEM, FULL, 0),
                ace(SID_ADMINS, FULL, 0),
                ace(USER, FILE_READ, 0),
            ]
        );
        assert!(aces.iter().all(|a| a.sid != SID_LOCAL_SERVICE));
        // read only for the user: none of FILE_WRITE_DATA, FILE_APPEND_DATA, attributes, DELETE, WRITE_DAC/OWNER
        assert_eq!(
            aces[2].mask & (0x2 | 0x4 | 0x10 | 0x100 | 0x10000 | 0x40000 | 0x80000),
            0
        );
        assert_eq!(
            sddl(&aces),
            format!(
                "D:P(A;;0x1f01ff;;;S-1-5-18)(A;;0x1f01ff;;;S-1-5-32-544)(A;;0x120089;;;{USER})"
            )
        );
        // the part of `logs` that would hand it LOCAL SERVICE's modify right is inheritable: that is why the file's
        // DACL must be protected
        assert!(sddl(&aces).starts_with("D:P"));
        assert!(Object::InstallLog.is_file() && Object::FramesFile.is_file());
        assert!(!Object::LogsDir.is_file() && !Object::DataDir.is_file());
    }

    #[test]
    fn the_frame_file_acl_is_read_and_write_without_delete() {
        let p = principals();
        let aces = expected(Object::FramesFile, &p);
        assert_eq!(aces.len(), 4);
        for who in [SID_LOCAL_SERVICE, USER] {
            let a = aces.iter().find(|a| a.sid == who).unwrap();
            assert_eq!(a.mask, FRAMES_READ_WRITE);
            assert_eq!(a.flags, 0);
            // FILE_READ_DATA and FILE_WRITE_DATA, so a mapping can be read-write
            assert_eq!(a.mask & 0x3, 0x3);
            // no DELETE (0x10000), WRITE_DAC (0x40000), WRITE_OWNER (0x80000), FILE_DELETE_CHILD (0x40), and
            // neither FILE_WRITE_ATTRIBUTES (0x100) nor FILE_WRITE_EA (0x10): those let a user hard-link the file
            // from their own folder or set it read-only or sparse
            assert_eq!(
                a.mask & (0x10000 | 0x40000 | 0x80000 | 0x40 | 0x100 | 0x10),
                0
            );
        }
        assert_eq!(
            sddl(&aces),
            format!(
                "D:P(A;;0x1f01ff;;;S-1-5-18)(A;;0x1f01ff;;;S-1-5-32-544)(A;;0x12008f;;;S-1-5-19)(A;;0x12008f;;;{USER})"
            )
        );
    }

    /// The reviewer's reproduction on a real file: with the old mask (`0x12019f`) the user could hard-link the file
    /// from a folder of their own and set it read-only; with the new one both are denied while a read-write
    /// mapping, the way the app and the DLL use the file, still works.
    #[test]
    fn the_frame_file_acl_denies_hard_links_and_attributes_but_not_a_read_write_mapping() {
        use vcam_protocol::secure::{self, DISP_CREATE_NEW};
        let me = crate::current_user_sid().unwrap();
        let dir = std::env::temp_dir().join(format!("vcam-acl-frames-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // a file that gives `me` exactly one ACE, with the given mask, from the create call on
        let make = |name: &str, mask: u32| {
            let sd = SecurityDescriptor::from_aces(&[ace(&me, mask, 0)]).unwrap();
            let path = dir.join(name);
            let h = unsafe {
                secure::open_with_sd(
                    &path,
                    secure::ACCESS_READ_WRITE,
                    secure::SHARE_ALL,
                    DISP_CREATE_NEW,
                    false,
                    0,
                    Some(sd.as_ptr()),
                )
            }
            .unwrap();
            let f = std::fs::File::from(h);
            f.set_len(vcam_protocol::FILE_SIZE).unwrap();
            path
        };
        let denied = |r: std::io::Result<()>| {
            r.expect_err("must be denied").kind() == std::io::ErrorKind::PermissionDenied
        };

        // control: the old mask lets the user link and mark the file
        let old = make("old.bin", 0x0012_019f);
        std::fs::hard_link(&old, dir.join("old-link.bin"))
            .expect("the old mask allows a hard link");
        let mut perm = std::fs::metadata(&old).unwrap().permissions();
        perm.set_readonly(true);
        std::fs::set_permissions(&old, perm).expect("the old mask allows attrib +R");
        assert!(
            secure::open_existing(&old, true, None).is_err(),
            "a read-only or hard-linked file is refused: the fault the reviewer found"
        );

        // the new mask
        let new = make("new.bin", FRAMES_READ_WRITE);
        assert!(denied(std::fs::hard_link(&new, dir.join("new-link.bin"))));
        let mut perm = std::fs::metadata(&new).unwrap().permissions();
        perm.set_readonly(true);
        assert!(denied(std::fs::set_permissions(&new, perm)));
        assert!(!dir.join("new-link.bin").exists());
        // read-write mapping and writes through it still work, and it is still one plain file (the mask does not
        // stop sparse, compressed or extra streams: see the next test)
        let m = vcam_protocol::MappedFile::open(&new, vcam_protocol::Access::ReadWrite)
            .expect("a read-write mapping needs no more than the new mask gives");
        m.shared().set_consumer_active(true);
        m.shared().set_consumer_heartbeat(7);
        assert!(m.shared().is_writable());
        assert_ne!(m.shared().header().consumer_active, 0);
        assert_eq!(m.shared().header().consumer_heartbeat, 7);
        drop(m);
        secure::open_existing(&new, true, None).expect("still a plain file with one link");
        let w = vcam_protocol::Writer::open_precreated(&new, None)
            .expect("the app's writer opens it with the new access too");
        w.set_state(vcam_protocol::WriterState::Live);
        drop(w);
        let mut perm = std::fs::metadata(&old).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perm.set_readonly(false);
        let _ = std::fs::set_permissions(&old, perm);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// What the mask `0x12008f` does *not* prevent (the reviewer's reproduction, on temp files with the same DACL):
    /// a handle with only `FILE_WRITE_DATA` makes the file sparse and can create a named stream; compression needs
    /// read and write data, which the mask has. The installer therefore clears the first two states in place and
    /// replaces a file that has a stream (`datadir` tests), and this test is the reason it has to.
    #[test]
    fn a_write_data_handle_can_still_make_the_frame_file_sparse_or_compressed_or_give_it_a_stream()
    {
        use std::os::windows::fs::OpenOptionsExt;
        use vcam_protocol::secure::{self, DISP_CREATE_NEW};
        let me = crate::current_user_sid().unwrap();
        let dir = std::env::temp_dir().join(format!("vcam-acl-writedata-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("frames.bin");
        let sd = SecurityDescriptor::from_aces(&[ace(&me, FRAMES_READ_WRITE, 0)]).unwrap();
        let h = unsafe {
            secure::open_with_sd(
                &path,
                secure::ACCESS_READ_WRITE,
                secure::SHARE_ALL,
                DISP_CREATE_NEW,
                false,
                0,
                Some(sd.as_ptr()),
            )
        }
        .unwrap();
        std::fs::File::from(h).set_len(1 << 20).unwrap();

        // only FILE_WRITE_DATA (0x2): sparse works
        let write_only = secure::open_with(
            &path,
            0x2,
            secure::SHARE_ALL,
            secure::DISP_OPEN_EXISTING,
            false,
            0,
        )
        .expect("the mask gives FILE_WRITE_DATA");
        secure::set_sparse(secure::raw(&write_only), true)
            .expect("FSCTL_SET_SPARSE needs only FILE_WRITE_DATA: the mask does not prevent it");
        assert_ne!(
            std::fs::metadata(&path).unwrap().file_attributes() & secure::ATTRIBUTE_SPARSE,
            0
        );
        // read and write data: compression works
        let read_write = secure::open_existing(&path, true, None).unwrap();
        if secure::set_compressed(secure::raw(&read_write), true).is_ok() {
            assert_ne!(
                std::fs::metadata(&path).unwrap().file_attributes() & secure::ATTRIBUTE_COMPRESSED,
                0
            );
        } else {
            eprintln!("this volume cannot compress a file: that part is skipped");
        }
        // a named stream through FILE_WRITE_DATA
        let stream = format!("{}:extra", path.display());
        let made = std::fs::OpenOptions::new()
            .write(true)
            .access_mode(0x2)
            .create(true)
            .truncate(true)
            .open(&stream);
        if made.is_ok() {
            let extra = secure::extra_streams(secure::raw(&read_write)).unwrap();
            assert_eq!(extra, [":extra:$DATA"], "the stream is there");
        } else {
            eprintln!("no named streams here: that part is skipped ({made:?})");
        }
        // and what the mask does prevent is still prevented
        let mut perm = std::fs::metadata(&path).unwrap().permissions();
        perm.set_readonly(true);
        assert!(std::fs::set_permissions(&path, perm).is_err());

        // the way back, through the installer's own handle (read and write data, not the user's mask)
        drop((write_only, read_write));
        // (the file has only `FRAMES_READ_WRITE`, so even the test asks for no more than that)
        let mine = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .access_mode(0x3)
            .open(&path)
            .unwrap();
        let changed = secure::clear_sparse_and_compression(HANDLE(mine.as_raw_handle())).unwrap();
        assert!(changed);
        let attrs = std::fs::metadata(&path).unwrap().file_attributes();
        assert_eq!(
            attrs & (secure::ATTRIBUTE_SPARSE | secure::ATTRIBUTE_COMPRESSED),
            0
        );
        drop(mine);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A temp folder owned by the current user whose DACL is `aces` (the user is the owner and the "administrator"
    /// of the test).
    fn object_with(aces: &[Ace], name: &str) -> (std::path::PathBuf, OwnedHandle) {
        let dir = std::env::temp_dir().join(format!("vcam-acl-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let h = vcam_protocol::secure::open_with(
            &dir,
            0x8000_0000 | SECURITY_ACCESS,
            vcam_protocol::secure::SHARE_ALL,
            vcam_protocol::secure::DISP_OPEN_EXISTING,
            true,
            0,
        )
        .unwrap();
        let me = crate::current_user_sid().unwrap();
        let mut p = Principals::machine(None);
        p.owner = me.clone();
        // build the DACL by hand: `aces` plus full control for the test itself
        let mut all = aces.to_vec();
        all.push(ace(&me, FULL, OBJECT_INHERIT | CONTAINER_INHERIT));
        apply_aces(&h, &all, &me).unwrap();
        (dir, h)
    }

    /// The DACL of `h` as SDDL text, as Windows prints it.
    fn stored_sddl(h: &OwnedHandle) -> String {
        use windows::Win32::Security::Authorization::ConvertSecurityDescriptorToStringSecurityDescriptorW;
        let mut psd = PSECURITY_DESCRIPTOR::default();
        let r = unsafe {
            GetSecurityInfo(
                HANDLE(h.as_raw_handle()),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                None,
                None,
                None,
                None,
                Some(&mut psd),
            )
        };
        assert_eq!(r, ERROR_SUCCESS);
        let mut s = windows::core::PWSTR::null();
        unsafe {
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                psd,
                SDDL_REVISION_1,
                DACL_SECURITY_INFORMATION,
                &mut s,
                None,
            )
            .unwrap();
        }
        let text = unsafe { s.to_string() }.unwrap();
        unsafe {
            let _ = LocalFree(Some(HLOCAL(s.0 as *mut _)));
            let _ = LocalFree(Some(HLOCAL(psd.0)));
        }
        text
    }

    #[test]
    fn what_was_applied_reads_back_and_checks_even_for_accounts_sddl_prints_as_aliases() {
        // The old check looked for the SID text in the SDDL: it never found the built-in Administrator account
        // (-500, printed "LA"), Guest (-501, "LG") or SYSTEM ("SY"), so install always failed for those users.
        let me = crate::current_user_sid().unwrap();
        for user in [machine_sid(500), machine_sid(501), SID_SYSTEM.to_string()] {
            let mut p = Principals::machine(Some(&user));
            p.owner = me.clone();
            // the test process must keep access to the temp folder it applies to: system and admins are "me"
            p.admins = me.clone();
            let want = expected(Object::FramesFile, &p);
            let dir = std::env::temp_dir().join(format!("vcam-acl-alias-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let f = dir.join("frames.bin");
            std::fs::write(&f, b"x").unwrap();
            let h = vcam_protocol::secure::open_with(
                &f,
                0x8000_0000 | 0x4000_0000 | SECURITY_ACCESS,
                vcam_protocol::secure::SHARE_ALL,
                vcam_protocol::secure::DISP_OPEN_EXISTING,
                true,
                0,
            )
            .unwrap();
            apply(&h, Object::FramesFile, &p).unwrap();
            let snap = read(&h).unwrap();
            check(&snap, &want, &me).unwrap();
            // What broke: the old read-back searched the SDDL *text* of the stored DACL for the raw SID, and
            // Windows prints these accounts as aliases, so the search found nothing.
            let stored_text = stored_sddl(&h);
            if user != SID_SYSTEM {
                assert!(
                    !stored_text.contains(&user),
                    "the stored SDDL names the account by its alias: {stored_text}"
                );
                assert!(
                    stored_text.contains(";;;LA)") || stored_text.contains(";;;LG)"),
                    "{stored_text}"
                );
            }
            let as_text = sddl(&want);
            assert!(as_text.contains(&user), "our own SDDL names the raw SID");
            if user != SID_SYSTEM {
                // the stored ACE reads back as the raw SID, EqualSid finds it
                assert!(snap.aces.iter().any(|a| sid_eq(&a.sid, &user)));
            }
            drop(h);
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn a_dacl_with_anything_else_does_not_check() {
        let me = crate::current_user_sid().unwrap();
        let mut p = Principals::machine(Some(USER));
        p.owner = me.clone();
        p.admins = me.clone();
        let want = expected(Object::FramesFile, &p);
        let (dir, h) = object_with(&[ace(SID_LOCAL_SERVICE, FRAMES_READ_WRITE, 0)], "extra");
        let snap = read(&h).unwrap();
        // the user's own ACE of `object_with` is inheritable (OICI): it is not what `want` says
        assert!(check(&snap, &want, &me).is_err());
        // an extra ACE next to the right ones
        let mut with_extra = want.clone();
        with_extra.push(ace("S-1-1-0", FILE_READ, 0));
        apply_aces(&h, &with_extra, &me).unwrap();
        let snap = read(&h).unwrap();
        check(&snap, &want, &me).expect_err("Everyone must not be tolerated");
        check(&snap, &with_extra, &me).unwrap();
        // the wrong owner, and a DACL that inherits
        assert!(check(&snap, &with_extra, SID_ADMINS)
            .unwrap_err()
            .contains("owner"));
        let mut inherit = snap.clone();
        inherit.protected = false;
        assert!(check(&inherit, &with_extra, &me)
            .unwrap_err()
            .contains("inherits"));
        // a denied ACE does not count as the allowed one
        let mut denied = snap.clone();
        denied.aces[0].kind = 1;
        assert!(check(&denied, &with_extra, &me).is_err());
        drop(h);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn owners_are_compared_as_sids() {
        let trusted = vec![SID_SYSTEM.to_string(), SID_ADMINS.to_string()];
        assert!(owner_trusted("S-1-5-18", &trusted));
        assert!(owner_trusted("S-1-5-32-544", &trusted));
        assert!(!owner_trusted(USER, &trusted));
        assert!(!owner_trusted("S-1-1-0", &trusted));
    }
}
