//! The DACL of `%ProgramData%\BetterCam` and `frames.bin`: SYSTEM and Administrators full control, LOCAL SERVICE (the
//! Frame Server account) and the installing user modify, inheritance off, nobody else.

use std::path::Path;

use windows::Win32::Foundation::{LocalFree, ERROR_SUCCESS, HLOCAL};
use windows::Win32::Security::Authorization::{
    ConvertSecurityDescriptorToStringSecurityDescriptorW,
    ConvertStringSecurityDescriptorToSecurityDescriptorW, GetNamedSecurityInfoW,
    SetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT,
};
use windows::Win32::Security::{
    GetSecurityDescriptorDacl, ACL, DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR,
};

use crate::log::Log;
use crate::util::{data_dir, pcwstr, werr, wide, Res};

/// Standard modify rights as an SDDL hex mask (read, write, execute, delete, synchronize).
const MODIFY: &str = "0x1301bf";

/// A user SID in the string form `S-1-5-21-...`: only digits and dashes after the `S-1-` prefix. The value ends up
/// inside an SDDL string that an elevated process applies, so nothing else may pass.
pub fn valid_sid(sid: &str) -> bool {
    let Some(rest) = sid.strip_prefix("S-1-") else {
        return false;
    };
    let parts: Vec<&str> = rest.split('-').collect();
    parts.len() >= 2
        && parts.len() <= 15
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.len() <= 10 && p.bytes().all(|b| b.is_ascii_digit()))
}

/// The SDDL of the data folder DACL.
pub fn data_acl_sddl(user_sid: Option<&str>) -> String {
    let mut sddl = format!("D:PAI(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;{MODIFY};;;LS)");
    if let Some(sid) = user_sid {
        sddl.push_str(&format!("(A;OICI;{MODIFY};;;{sid})"));
    }
    sddl
}

/// Apply [`data_acl_sddl`] to `path`, protected (no inheritance from `%ProgramData%`).
pub fn set_data_acl(path: &Path, user_sid: Option<&str>) -> Res<()> {
    if let Some(sid) = user_sid {
        if !valid_sid(sid) {
            return Err(format!("'{sid}' is not a user SID"));
        }
    }
    let sddl_w = wide(&data_acl_sddl(user_sid));
    let mut psd = PSECURITY_DESCRIPTOR::default();
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            pcwstr(&sddl_w),
            SDDL_REVISION_1,
            &mut psd,
            None,
        )
        .map_err(|e| werr("ConvertStringSecurityDescriptorToSecurityDescriptorW", e))?;
    }
    let result = (|| {
        let mut present = windows_core::BOOL(0);
        let mut defaulted = windows_core::BOOL(0);
        let mut dacl: *mut ACL = std::ptr::null_mut();
        unsafe {
            GetSecurityDescriptorDacl(psd, &mut present, &mut dacl, &mut defaulted)
                .map_err(|e| werr("GetSecurityDescriptorDacl", e))?;
        }
        let path_w = wide(&path.to_string_lossy());
        let r = unsafe {
            SetNamedSecurityInfoW(
                pcwstr(&path_w),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                None,
                None,
                Some(dacl),
                None,
            )
        };
        if r == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(format!("SetNamedSecurityInfoW {}: {r:?}", path.display()))
        }
    })();
    unsafe {
        let _ = LocalFree(Some(HLOCAL(psd.0)));
    }
    result
}

/// The DACL of `path` as an SDDL string.
pub fn dacl_sddl(path: &Path) -> Res<String> {
    let path_w = wide(&path.to_string_lossy());
    let mut psd = PSECURITY_DESCRIPTOR::default();
    let r = unsafe {
        GetNamedSecurityInfoW(
            pcwstr(&path_w),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            None,
            None,
            &mut psd,
        )
    };
    if r != ERROR_SUCCESS {
        return Err(format!("GetNamedSecurityInfoW {}: {r:?}", path.display()));
    }
    let mut s = windows_core::PWSTR::null();
    let conv = unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            psd,
            SDDL_REVISION_1,
            DACL_SECURITY_INFORMATION,
            &mut s,
            None,
        )
    };
    let text = match conv {
        Ok(()) => unsafe { s.to_string() }.map_err(|e| werr("sddl utf16", e)),
        Err(e) => Err(werr(
            "ConvertSecurityDescriptorToStringSecurityDescriptorW",
            e,
        )),
    };
    unsafe {
        if !s.is_null() {
            let _ = LocalFree(Some(HLOCAL(s.0 as *mut _)));
        }
        let _ = LocalFree(Some(HLOCAL(psd.0)));
    }
    text
}

/// Make sure both the user's writer and LOCAL SERVICE can open `frames.bin` read-write: the folder DACL carries
/// modify access for both with inheritance (so a file the writer creates inherits it), and an existing `frames.bin`
/// gets the same DACL. Verified by reading the DACL back.
pub fn ensure_frames_access(user_sid: Option<&str>, log: &mut dyn Log) -> Res<()> {
    let dir = data_dir();
    set_data_acl(&dir, user_sid)?;
    let frames = dir.join(vcam_protocol::FRAMES_FILE_NAME);
    if frames.exists() {
        set_data_acl(&frames, user_sid)?;
        log.line(&format!("frames.bin: DACL applied ({})", frames.display()));
    } else {
        log.line(
            "frames.bin: not created yet (the writer creates it and inherits the folder DACL)",
        );
    }
    let sddl = dacl_sddl(&dir)?;
    log.line(&format!("data dir DACL: {sddl}"));
    for (who, tag) in [("LOCAL SERVICE", ";;;LS)")]
        .into_iter()
        .chain(user_sid.map(|s| ("the user", s)))
    {
        if !sddl.contains(tag) {
            return Err(format!("data dir DACL has no entry for {who}"));
        }
    }
    if frames.exists() {
        let f = dacl_sddl(&frames)?;
        if !f.contains(";;;LS)") {
            return Err("frames.bin DACL has no entry for LOCAL SERVICE".into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_user_sids_pass() {
        assert!(valid_sid("S-1-5-21-1004336348-1177238915-682003330-1001"));
        assert!(valid_sid("S-1-5-18"));
        for bad in [
            "",
            "S-1-",
            "S-1-5",
            "S-1-5-",
            "S-1-5--1",
            "S-1-5-21-12a",
            "S-1-5-21-1)(A;;FA;;;WD",
            "s-1-5-21-1",
            "S-2-5-21-1",
            "S-1-5-21-99999999999",
            " S-1-5-21-1",
        ] {
            assert!(!valid_sid(bad), "'{bad}' must be refused");
        }
    }

    #[test]
    fn the_folder_acl_names_four_trustees_and_is_protected() {
        let sddl = data_acl_sddl(Some("S-1-5-21-1-2-3-1001"));
        assert!(sddl.starts_with("D:PAI"), "protected, auto-inherited flag");
        assert_eq!(sddl.matches("(A;").count(), 4);
        for who in [";;;SY)", ";;;BA)", ";;;LS)", ";;;S-1-5-21-1-2-3-1001)"] {
            assert!(sddl.contains(who), "{who}");
        }
        assert_eq!(data_acl_sddl(None).matches("(A;").count(), 3);
    }
}
