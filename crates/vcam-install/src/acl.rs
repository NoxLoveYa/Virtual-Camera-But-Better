//! Permissions of `%ProgramData%\BetterCam`, `logs` and `frames.bin`.
//!
//! The folder is shared by three accounts of different trust: the installer (an administrator), LOCAL SERVICE (Frame
//! Server, which runs the DLL), and the user who runs the app. The user is the one who must not be able to rearrange
//! anything: the elevated helper works in these folders and a junction planted by the user would send it somewhere
//! else. So the user gets exactly what the app needs and nothing that creates, deletes or renames:
//!
//! | object | owner | ACE (all protected, no inheritance from `%ProgramData%`) |
//! |---|---|---|
//! | `BetterCam` | Administrators | SYSTEM, Administrators: full, inherited. LOCAL SERVICE, user: read and traverse, this folder only |
//! | `logs` | Administrators | SYSTEM, Administrators: full, inherited. LOCAL SERVICE: read, traverse and create files here; modify on files created here. User: read and traverse here, read on the files in it |
//! | `frames.bin` | Administrators | SYSTEM, Administrators: full. LOCAL SERVICE, user: read and write data (no delete, no change of permissions) |
//!
//! Everything here is applied and read back through a handle that [`vcam_protocol::secure`] verified, never by path.

use std::os::windows::io::{AsRawHandle, OwnedHandle};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{LocalFree, ERROR_SUCCESS, HANDLE, HLOCAL};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
    ConvertStringSidToSidW, GetSecurityInfo, SetSecurityInfo, SDDL_REVISION_1, SE_FILE_OBJECT,
};
use windows::Win32::Security::{
    EqualSid, GetAce, GetSecurityDescriptorControl, GetSecurityDescriptorDacl, LookupAccountSidW,
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
/// `FILE_GENERIC_READ | FILE_GENERIC_WRITE`: read and write data and attributes, no `DELETE`, no `WRITE_DAC`.
pub const FILE_READ_WRITE: u32 = 0x0012_019f;
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

/// The three objects of the data folder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Object {
    DataDir,
    LogsDir,
    FramesFile,
}

impl Object {
    pub fn describe(self) -> &'static str {
        match self {
            Object::DataDir => "the data folder",
            Object::LogsDir => "the logs folder",
            Object::FramesFile => "frames.bin",
        }
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
                v.push(ace(u, READ_TRAVERSE, 0));
                v.push(ace(u, FILE_READ, OBJECT_INHERIT | INHERIT_ONLY));
            }
        }
        Object::FramesFile => {
            v.push(ace(&p.system, FULL, 0));
            v.push(ace(&p.admins, FULL, 0));
            v.push(ace(&p.local_service, FILE_READ_WRITE, 0));
            if let Some(u) = &p.user {
                v.push(ace(u, FILE_READ_WRITE, 0));
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
struct SidBuf(PSID);

impl SidBuf {
    fn parse(text: &str) -> Res<SidBuf> {
        let w = wide(text);
        let mut p = PSID::default();
        unsafe { ConvertStringSidToSidW(pcwstr(&w), &mut p) }
            .map_err(|e| werr(&format!("ConvertStringSidToSidW '{text}'"), e))?;
        Ok(SidBuf(p))
    }

    fn psid(&self) -> PSID {
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

/// Set owner and the protected DACL of `obj` on `h`.
pub fn apply(h: &OwnedHandle, obj: Object, p: &Principals) -> Res<()> {
    let text = sddl(&expected(obj, p));
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
    let result = (|| {
        let mut present = windows_core::BOOL(0);
        let mut defaulted = windows_core::BOOL(0);
        let mut dacl: *mut ACL = std::ptr::null_mut();
        unsafe { GetSecurityDescriptorDacl(psd, &mut present, &mut dacl, &mut defaulted) }
            .map_err(|e| werr("GetSecurityDescriptorDacl", e))?;
        let owner = SidBuf::parse(&p.owner)?;
        let r = unsafe {
            SetSecurityInfo(
                HANDLE(h.as_raw_handle()),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION
                    | DACL_SECURITY_INFORMATION
                    | PROTECTED_DACL_SECURITY_INFORMATION,
                Some(owner.psid()),
                None,
                Some(dacl),
                None,
            )
        };
        if r == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(format!(
                "SetSecurityInfo on {}: error {}",
                obj.describe(),
                r.0
            ))
        }
    })();
    unsafe {
        let _ = LocalFree(Some(HLOCAL(psd.0)));
    }
    result
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
    let mut owner = PSID::default();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut psd = PSECURITY_DESCRIPTOR::default();
    let r = unsafe {
        GetSecurityInfo(
            HANDLE(h.as_raw_handle()),
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
    fn the_logs_acl_lets_only_the_service_write() {
        let p = principals();
        let aces = expected(Object::LogsDir, &p);
        assert_eq!(aces.len(), 6);
        let both = OBJECT_INHERIT | CONTAINER_INHERIT;
        assert!(aces.contains(&ace(SID_SYSTEM, FULL, both)));
        assert!(aces.contains(&ace(SID_ADMINS, FULL, both)));
        assert!(aces.contains(&ace(SID_LOCAL_SERVICE, READ_TRAVERSE_ADD_FILE, 0)));
        assert!(aces.contains(&ace(
            SID_LOCAL_SERVICE,
            FILE_MODIFY,
            OBJECT_INHERIT | INHERIT_ONLY
        )));
        // the user: read and traverse here, read on files; no ACE gives the user ADD_FILE, DELETE or WRITE_*
        let mine: Vec<&Ace> = aces.iter().filter(|a| a.sid == USER).collect();
        assert_eq!(mine.len(), 2);
        for a in mine {
            assert_eq!(
                a.mask & (0x2 | 0x4 | 0x10 | 0x40 | 0x100 | 0x10000 | 0x40000 | 0x80000),
                0
            );
        }
        assert_eq!(
            sddl(&aces),
            format!(
                "D:P(A;OICI;0x1f01ff;;;S-1-5-18)(A;OICI;0x1f01ff;;;S-1-5-32-544)(A;;0x1200ab;;;S-1-5-19)\
                 (A;OIIO;0x1301bf;;;S-1-5-19)(A;;0x1200a9;;;{USER})(A;OIIO;0x120089;;;{USER})"
            )
        );
    }

    #[test]
    fn the_frame_file_acl_is_read_and_write_without_delete() {
        let p = principals();
        let aces = expected(Object::FramesFile, &p);
        assert_eq!(aces.len(), 4);
        for who in [SID_LOCAL_SERVICE, USER] {
            let a = aces.iter().find(|a| a.sid == who).unwrap();
            assert_eq!(a.mask, FILE_READ_WRITE);
            assert_eq!(a.flags, 0);
            // FILE_READ_DATA and FILE_WRITE_DATA, so a mapping can be read-write
            assert_eq!(a.mask & 0x3, 0x3);
            // no DELETE (0x10000), WRITE_DAC (0x40000), WRITE_OWNER (0x80000), FILE_DELETE_CHILD (0x40)
            assert_eq!(a.mask & (0x10000 | 0x40000 | 0x80000 | 0x40), 0);
        }
        assert_eq!(
            sddl(&aces),
            format!(
                "D:P(A;;0x1f01ff;;;S-1-5-18)(A;;0x1f01ff;;;S-1-5-32-544)(A;;0x12019f;;;S-1-5-19)(A;;0x12019f;;;{USER})"
            )
        );
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

    /// [`apply`] for an arbitrary list (tests).
    fn apply_aces(h: &OwnedHandle, aces: &[Ace], owner: &str) -> Res<()> {
        let mut p = Principals::machine(None);
        p.owner = owner.to_string();
        // reuse `apply` through a one-off object kind: write the SDDL directly
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
        .map_err(|e| werr("sd", e))?;
        let mut present = windows_core::BOOL(0);
        let mut defaulted = windows_core::BOOL(0);
        let mut dacl: *mut ACL = std::ptr::null_mut();
        unsafe { GetSecurityDescriptorDacl(psd, &mut present, &mut dacl, &mut defaulted) }
            .map_err(|e| werr("dacl", e))?;
        let owner = SidBuf::parse(owner)?;
        let r = unsafe {
            SetSecurityInfo(
                HANDLE(h.as_raw_handle()),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION
                    | DACL_SECURITY_INFORMATION
                    | PROTECTED_DACL_SECURITY_INFORMATION,
                Some(owner.psid()),
                None,
                Some(dacl),
                None,
            )
        };
        unsafe {
            let _ = LocalFree(Some(HLOCAL(psd.0)));
        }
        let _ = p;
        if r == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(format!("SetSecurityInfo {}", r.0))
        }
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
        let (dir, h) = object_with(&[ace(SID_LOCAL_SERVICE, FILE_READ_WRITE, 0)], "extra");
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
