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
    open_namespace_with(
        &mut || {
            let ns =
                unsafe { CreatePrivateNamespaceW(Some(attributes), boundary.0, pcwstr(alias)) };
            if ns.is_invalid() {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(ns)
            }
        },
        &mut || {
            let ns = unsafe { OpenPrivateNamespaceW(boundary.0, pcwstr(alias)) };
            if ns.is_invalid() {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(ns)
            }
        },
    )
}

/// How often the create-or-open sequence is tried before it gives up.
const NAMESPACE_ATTEMPTS: u32 = 20;

/// [`open_namespace`] with the two system calls given (tests make them race).
///
/// A namespace lives only as long as some process has a handle to it. So "create fails because it exists, then open
/// it" can lose a race: the process that made it closes its last handle in between, and the open finds nothing,
/// although nobody holds the lock any more. That is not an error and not a busy lock: the sequence starts again
/// (the next create makes it), a bounded number of times.
fn open_namespace_with(
    create: &mut dyn FnMut() -> std::io::Result<HANDLE>,
    open: &mut dyn FnMut() -> std::io::Result<HANDLE>,
) -> Res<HANDLE> {
    let mut lost_race = None;
    for _ in 0..NAMESPACE_ATTEMPTS {
        let created = match create() {
            Ok(ns) => return Ok(ns),
            Err(e) => e,
        };
        match created.raw_os_error() {
            // another process made the namespace: open it
            Some(e) if e == ERROR_ALREADY_EXISTS.0 as i32 => match open() {
                Ok(ns) => return Ok(ns),
                // the alias is bound in this process already (see below)
                Err(opened) if opened.raw_os_error() == Some(ERROR_DUP_NAME.0 as i32) => {
                    return Ok(HANDLE::default())
                }
                // gone again: the holder ended between the two calls
                Err(opened) => {
                    lost_race = Some(opened);
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            },
            // this process has the alias bound already (a lock held by another thread of it): the name resolves,
            // there is nothing to open; the mutex below is found and the answer is "busy"
            Some(e) if e == ERROR_DUP_NAME.0 as i32 => return Ok(HANDLE::default()),
            _ => {
                return Err(werr(
                    "install lock: private namespace (is this process elevated?)",
                    created,
                ))
            }
        }
    }
    Err(werr(
        "install lock: open namespace",
        lost_race.unwrap_or_else(|| std::io::Error::other("no attempt was made")),
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

    fn code(c: windows::Win32::Foundation::WIN32_ERROR) -> std::io::Error {
        std::io::Error::from_raw_os_error(c.0 as i32)
    }

    /// The holder of the namespace closes its last handle between our failed create ("already exists") and our open:
    /// nobody holds the lock, and the helper must take it, not exit with an error.
    #[test]
    fn a_namespace_that_goes_away_between_the_create_and_the_open_is_created_again() {
        use std::cell::Cell;
        let creates = Cell::new(0);
        let opens = Cell::new(0);
        let made = HANDLE(0x77 as *mut core::ffi::c_void);
        let got = open_namespace_with(
            &mut || {
                creates.set(creates.get() + 1);
                // the first two creates find it there, the third finds it gone and makes it
                if creates.get() < 3 {
                    Err(code(ERROR_ALREADY_EXISTS))
                } else {
                    Ok(made)
                }
            },
            &mut || {
                opens.set(opens.get() + 1);
                Err(code(windows::Win32::Foundation::ERROR_FILE_NOT_FOUND))
            },
        )
        .expect("a lock nobody holds must be taken");
        assert_eq!(got, made);
        assert_eq!((creates.get(), opens.get()), (3, 2));
    }

    /// The retry is bounded: an open that keeps failing is reported, with its error.
    #[test]
    fn a_namespace_that_cannot_be_opened_is_reported_after_a_bounded_number_of_tries() {
        let creates = std::cell::Cell::new(0);
        let e = open_namespace_with(
            &mut || {
                creates.set(creates.get() + 1);
                Err(code(ERROR_ALREADY_EXISTS))
            },
            &mut || Err(code(windows::Win32::Foundation::ERROR_ACCESS_DENIED)),
        )
        .unwrap_err();
        assert_eq!(creates.get(), NAMESPACE_ATTEMPTS);
        assert!(e.contains("open namespace"), "{e}");
        // a create that fails for any other reason is not retried
        let creates = std::cell::Cell::new(0);
        let e = open_namespace_with(
            &mut || {
                creates.set(creates.get() + 1);
                Err(code(windows::Win32::Foundation::ERROR_ACCESS_DENIED))
            },
            &mut || panic!("no open after a refused create"),
        )
        .unwrap_err();
        assert_eq!(creates.get(), 1);
        assert!(e.contains("private namespace"), "{e}");
        // the alias bound in this process already means "held here", from the create or from the open
        let held =
            open_namespace_with(&mut || Err(code(ERROR_DUP_NAME)), &mut || panic!()).unwrap();
        assert!(held.is_invalid());
        let held = open_namespace_with(&mut || Err(code(ERROR_ALREADY_EXISTS)), &mut || {
            Err(code(ERROR_DUP_NAME))
        })
        .unwrap();
        assert!(held.is_invalid());
    }

    /// With the real calls, in several processes (a namespace alias is bound once per process, so threads of one
    /// process would hit that rule instead): helpers take the lock and let go of it in a tight loop. The answer is
    /// "held" or "busy" every time; the old sequence ("create fails because it exists", then "open it") met a
    /// namespace that had just been closed by the last holder and ended with an error although nobody held the lock.
    #[test]
    fn racing_helpers_never_see_an_error_when_nobody_holds_the_lock() {
        let name = unique("R");
        let exe = std::env::current_exe().unwrap();
        let children: Vec<_> = (0..3)
            .map(|_| {
                std::process::Command::new(&exe)
                    .args([
                        "--exact",
                        "lock::tests::race_worker",
                        "--nocapture",
                        "--ignored",
                    ])
                    .env("BETTERCAM_LOCK_TEST_NAME", &name)
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .unwrap()
            })
            .collect();
        let mut held = 0u32;
        for c in children {
            let out = c.wait_with_output().unwrap();
            let text = String::from_utf8_lossy(&out.stdout).into_owned()
                + &String::from_utf8_lossy(&out.stderr);
            assert!(
                out.status.success() && text.contains("RACE DONE"),
                "a helper saw an error although nobody held the lock: {text}"
            );
            held += text
                .split("held=")
                .nth(1)
                .and_then(|r| r.split_whitespace().next())
                .and_then(|n| n.parse::<u32>().ok())
                .unwrap_or(0);
        }
        assert!(held > 0, "nobody ever got the lock");
    }

    /// The child side of the test above (run only by it).
    #[test]
    #[ignore]
    fn race_worker() {
        let Ok(name) = std::env::var("BETTERCAM_LOCK_TEST_NAME") else {
            return;
        };
        let me = me();
        let until = std::time::Instant::now() + std::time::Duration::from_millis(2500);
        let (mut held, mut busy) = (0u32, 0u32);
        while std::time::Instant::now() < until {
            match acquire_in(&name, &me) {
                Ok(Acquired::Held(l)) => {
                    held += 1;
                    drop(l);
                }
                Ok(Acquired::Busy) => busy += 1,
                Err(e) => panic!("an unheld lock gave an error: {e}"),
            }
        }
        println!("RACE DONE held={held} busy={busy}");
    }
    #[test]
    fn the_message_names_the_situation() {
        assert!(BUSY_MESSAGE.contains("already running"));
    }
}
