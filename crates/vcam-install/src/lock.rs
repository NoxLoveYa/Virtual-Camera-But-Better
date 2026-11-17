//! One install, repair or removal at a time on the whole machine.
//!
//! The app refuses a second action while one runs in the same process (`vcam_admin`), but that guard does not see
//! an app that was restarted while the helper was still working, a second copy of the app, or `vcam-tool`. So the
//! elevated helper takes a machine-wide lock itself, before it touches anything, and a second helper that finds it
//! taken exits with a clear result ([`Acquired::Busy`]).
//!
//! The lock is a named mutex, and it is *the existence of the object* that counts, not who owns it: the first
//! helper creates it, every other one finds it already there. It lives as long as its creator has a handle open
//! (the end of the process closes it, so a helper that crashes leaves no stale lock).
//!
//! The mutex is made in a *private namespace* whose boundary is the Administrators group. A plain `Global\` name could
//! be created beforehand by any standard user, who could then hold it and make every install say "busy"; to create or
//! even open a private namespace with that boundary a process needs the Administrators SID enabled in its token,
//! which only an elevated process has. The objects get a security descriptor that names only SYSTEM and the
//! boundary account.

use windows::Win32::Foundation::{
    CloseHandle, LocalFree, ERROR_ALREADY_EXISTS, ERROR_DUP_NAME, HANDLE, HLOCAL,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::System::Threading::{
    AddSIDToBoundaryDescriptor, ClosePrivateNamespace, CreateBoundaryDescriptorW, CreateMutexW,
    CreatePrivateNamespaceW, DeleteBoundaryDescriptor, OpenPrivateNamespaceW,
};

use crate::acl::{SidBuf, SID_ADMINS};
use crate::util::{pcwstr, werr, wide, Res};

/// Name of the boundary descriptor and of the namespace's alias.
const NAMESPACE: &str = "BetterCamVcamInstall";
/// The mutex inside the namespace.
const MUTEX: &str = "Lock";

/// The result of asking for the machine-wide lock.
#[derive(Debug)]
pub enum Acquired {
    /// This process holds the lock until the value is dropped.
    Held(InstallLock),
    /// Another install, repair or removal is running.
    Busy,
}

/// The lock; released when dropped (and by the end of the process).
#[derive(Debug)]
pub struct InstallLock {
    mutex: HANDLE,
    namespace: HANDLE,
}

impl Drop for InstallLock {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.mutex);
        }
        close_namespace(self.namespace);
    }
}

/// Close a namespace handle; a null one (this process had the alias bound already) is nothing.
fn close_namespace(ns: HANDLE) {
    if !ns.is_invalid() {
        unsafe {
            let _ = ClosePrivateNamespace(ns, 0);
        }
    }
}

/// What a caller says when it finds the lock taken.
pub const BUSY_MESSAGE: &str =
    "Another install, repair or removal of the virtual camera is already running. Wait until it has finished and try again.";

/// Take the machine-wide lock. Needs an elevated process (the Administrators group must be enabled in its token);
/// an error says why it could not be taken, [`Acquired::Busy`] that somebody else holds it.
pub fn acquire() -> Res<Acquired> {
    acquire_in(NAMESPACE, SID_ADMINS)
}

/// [`acquire`] in the namespace `name` whose boundary is the account `boundary_sid` (tests use their own name and
/// the current user, who needs no elevation for it).
pub fn acquire_in(name: &str, boundary_sid: &str) -> Res<Acquired> {
    let sid = SidBuf::parse(boundary_sid)?;
    let sd = descriptor(boundary_sid)?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.0 .0,
        bInheritHandle: false.into(),
    };
    let w_name = wide(name);
    let mut boundary = unsafe { CreateBoundaryDescriptorW(pcwstr(&w_name), 0) };
    if boundary.is_invalid() {
        return Err(werr(
            "install lock: boundary descriptor",
            std::io::Error::last_os_error(),
        ));
    }
    let result = (|| {
        unsafe { AddSIDToBoundaryDescriptor(&mut boundary, sid.psid()) }
            .map_err(|e| werr("install lock: boundary SID", e))?;
        let namespace = open_namespace(&attributes, boundary, &w_name)?;
        let w_mutex = wide(&format!("{name}\\{MUTEX}"));
        let mutex = unsafe { CreateMutexW(Some(&attributes), false, pcwstr(&w_mutex)) };
        // read at once: the answer "it was there already" is the thread's last error
        let existed =
            std::io::Error::last_os_error().raw_os_error() == Some(ERROR_ALREADY_EXISTS.0 as i32);
        match mutex {
            Ok(m) if existed => {
                unsafe {
                    let _ = CloseHandle(m);
                }
                close_namespace(namespace);
                Ok(Acquired::Busy)
            }
            Ok(m) => Ok(Acquired::Held(InstallLock {
                mutex: m,
                namespace,
            })),
            Err(e) => {
                close_namespace(namespace);
                Err(werr("install lock: mutex", e))
            }
        }
    })();
    unsafe { DeleteBoundaryDescriptor(boundary) };
    result
}

/// Create the private namespace, or open it when another process made it.
fn open_namespace(
    attributes: &SECURITY_ATTRIBUTES,
    boundary: HANDLE,
    alias: &[u16],
) -> Res<HANDLE> {
    let ns = unsafe { CreatePrivateNamespaceW(Some(attributes), boundary.0, pcwstr(alias)) };
    if !ns.is_invalid() {
        return Ok(ns);
    }
    let created = std::io::Error::last_os_error();
    match created.raw_os_error() {
        // another process made the namespace: open it
        Some(e) if e == ERROR_ALREADY_EXISTS.0 as i32 => {
            let ns = unsafe { OpenPrivateNamespaceW(boundary.0, pcwstr(alias)) };
            if !ns.is_invalid() {
                return Ok(ns);
            }
            let opened = std::io::Error::last_os_error();
            // the alias is bound in this process already (see below)
            if opened.raw_os_error() == Some(ERROR_DUP_NAME.0 as i32) {
                return Ok(HANDLE::default());
            }
            return Err(werr("install lock: open namespace", opened));
        }
        // this process has the alias bound already (a lock held by another thread of it): the name resolves, there
        // is nothing to open; the mutex below is found and the answer is "busy"
        Some(e) if e == ERROR_DUP_NAME.0 as i32 => return Ok(HANDLE::default()),
        _ => {}
    }
    Err(werr(
        "install lock: private namespace (is this process elevated?)",
        created,
    ))
}

/// Security descriptor of the namespace and the mutex: all access for SYSTEM and the boundary account, nothing for
/// anybody else.
fn descriptor(boundary_sid: &str) -> Res<DescriptorBuf> {
    let text = format!("D:P(A;;GA;;;SY)(A;;GA;;;{boundary_sid})");
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
    .map_err(|e| werr(&format!("install lock: security descriptor '{text}'"), e))?;
    Ok(DescriptorBuf(psd))
}

struct DescriptorBuf(PSECURITY_DESCRIPTOR);

impl Drop for DescriptorBuf {
    fn drop(&mut self) {
        unsafe {
            let _ = LocalFree(Some(HLOCAL(self.0 .0)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn me() -> String {
        crate::current_user_sid().unwrap()
    }

    fn unique(tag: &str) -> String {
        format!("BetterCamTest{}{tag}", std::process::id())
    }

    #[test]
    fn a_second_helper_finds_the_lock_taken_and_the_next_one_gets_it_after_the_first_ends() {
        let name = unique("A");
        let first = match acquire_in(&name, &me()).unwrap() {
            Acquired::Held(l) => l,
            Acquired::Busy => panic!("a fresh lock was busy"),
        };
        // a second one, in this process and from another thread: busy, and it does not wait
        assert!(matches!(acquire_in(&name, &me()).unwrap(), Acquired::Busy));
        let n = name.clone();
        let other = std::thread::spawn(move || {
            matches!(
                acquire_in(&n, &crate::current_user_sid().unwrap()).unwrap(),
                Acquired::Busy
            )
        });
        assert!(other.join().unwrap());
        // other names are other locks
        assert!(matches!(
            acquire_in(&unique("B"), &me()).unwrap(),
            Acquired::Held(_)
        ));
        drop(first);
        // the first ended: the lock is free again
        assert!(matches!(
            acquire_in(&name, &me()).unwrap(),
            Acquired::Held(_)
        ));
    }

    /// A helper process that dies while it holds the lock leaves no stale lock behind.
    #[test]
    fn the_end_of_the_holding_process_frees_the_lock() {
        let name = unique("C");
        // a child that takes the lock and holds it until it reads a line on stdin
        let exe = std::env::current_exe().unwrap();
        let mut child = std::process::Command::new(exe)
            .args(["--exact", "lock::tests::holder", "--nocapture", "--ignored"])
            .env("BETTERCAM_LOCK_TEST_NAME", &name)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut out = std::io::BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        loop {
            line.clear();
            use std::io::BufRead;
            assert!(
                out.read_line(&mut line).unwrap() > 0,
                "the holder ended early"
            );
            if line.contains("HOLDING") {
                break;
            }
        }
        assert!(
            matches!(acquire_in(&name, &me()).unwrap(), Acquired::Busy),
            "a second helper must be refused while the first one runs"
        );
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(
            matches!(acquire_in(&name, &me()).unwrap(), Acquired::Held(_)),
            "the lock outlived its holder"
        );
    }

    /// The child side of the test above (run only by it).
    #[test]
    #[ignore]
    fn holder() {
        let Ok(name) = std::env::var("BETTERCAM_LOCK_TEST_NAME") else {
            return;
        };
        let _l = match acquire_in(&name, &me()).unwrap() {
            Acquired::Held(l) => l,
            Acquired::Busy => panic!("busy"),
        };
        println!("HOLDING");
        let mut s = String::new();
        let _ = std::io::stdin().read_line(&mut s);
    }

    #[test]
    fn a_boundary_that_is_not_enabled_in_the_token_cannot_make_the_lock() {
        // the machine-wide boundary is Administrators: an unelevated process does not have it enabled, so it can
        // neither take the lock nor squat the name (the helper itself is always elevated)
        if crate::is_elevated() {
            eprintln!("elevated: skipped");
            return;
        }
        let e = acquire().unwrap_err();
        assert!(e.contains("install lock"), "{e}");
    }

    #[test]
    fn the_message_names_the_situation() {
        assert!(BUSY_MESSAGE.contains("already running"));
    }
}
