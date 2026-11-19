//! Tests of the frame event: names, nonces, the security descriptor read back from the kernel, collisions, the order of
//! a signal, every interleaving of a writer and a reader, and real threads.

use super::*;

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows::core::BOOL;
use windows::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Security::Authorization::ConvertStringSidToSidW;
use windows::Win32::Security::{
    DuplicateTokenEx, EqualSid, GetAce, GetKernelObjectSecurity, GetLengthSid,
    GetSecurityDescriptorControl, GetSecurityDescriptorDacl, GetSecurityDescriptorOwner,
    GetSecurityDescriptorSacl, SecurityImpersonation, SetTokenInformation, TokenImpersonation,
    TokenIntegrityLevel, ACCESS_ALLOWED_ACE, ACL, PSID, SID_AND_ATTRIBUTES, TOKEN_ADJUST_DEFAULT,
    TOKEN_ALL_ACCESS, TOKEN_DUPLICATE, TOKEN_MANDATORY_LABEL,
};
use windows::Win32::System::Threading::{
    CreateMutexW, SetThreadToken, WaitForSingleObject, SYNCHRONIZATION_ACCESS_RIGHTS,
};

use crate::header::{WriterState, FEATURE_FRAME_EVENT};
use crate::map::{Access, MappedFile, Writer};
use crate::seqlock::SlotFormat;
use crate::validate::validate_header;

// ---- what a step saw (the recording stand-in) ------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum What {
    Step(Step),
    Withdraw,
}

/// The world as one step found it, before it acted.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Seen {
    pub what: What,
    pub features: u32,
    pub state: u32,
    pub seq: u64,
    pub frames: u64,
    pub latest: u32,
    pub set: [bool; 2],
}

// ---- the nonce hook --------------------------------------------------------------------------------------------

type NonceHook = Box<dyn FnMut() -> io::Result<u128>>;

thread_local! {
    /// Draws of [`system_nonce`] on this thread come from here while it is set.
    static NONCES: RefCell<Option<NonceHook>> = const { RefCell::new(None) };
}

pub(crate) fn draw_hooked_nonce() -> Option<io::Result<u128>> {
    NONCES.with(|h| h.borrow_mut().as_mut().map(|f| f()))
}

/// Run `run` with `nonces` as the generator of this thread; the list ends in an error.
pub(crate) fn with_nonces<R>(nonces: Vec<io::Result<u128>>, run: impl FnOnce() -> R) -> R {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            NONCES.with(|h| *h.borrow_mut() = None);
        }
    }
    let mut it = nonces.into_iter();
    NONCES.with(|h| {
        *h.borrow_mut() = Some(Box::new(move || {
            it.next()
                .unwrap_or_else(|| Err(io::Error::other("the test ran out of nonces")))
        }))
    });
    let _reset = Reset;
    run()
}

// ---- helpers ---------------------------------------------------------------------------------------------------

fn temp_file(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("bettercam-signal-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

/// Heap memory with the alignment of a mapping, for a view of a whole header.
struct Heap(Vec<u64>);
impl Heap {
    fn new() -> Heap {
        Heap(vec![0u64; crate::header::FILE_SIZE as usize / 8 + 1])
    }
    fn shared(&mut self) -> Shared {
        unsafe {
            Shared::new(
                self.0.as_mut_ptr() as *mut u8,
                crate::header::FILE_SIZE as usize,
                true,
            )
            .unwrap()
        }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub(crate) fn is_set(h: HANDLE) -> bool {
    unsafe { WaitForSingleObject(h, 0) == WAIT_OBJECT_0 }
}

pub(crate) fn open_for(nonce: u128, parity: usize, access: u32) -> windows::core::Result<HANDLE> {
    let name = event_name_wide(nonce, parity);
    unsafe {
        OpenEventW(
            SYNCHRONIZATION_ACCESS_RIGHTS(access),
            false,
            PCWSTR(name.as_ptr()),
        )
    }
}

fn close(h: HANDLE) {
    unsafe {
        let _ = windows::Win32::Foundation::CloseHandle(h);
    }
}

fn denied(r: &windows::core::Result<HANDLE>) -> bool {
    matches!(r, Err(e) if e.code() == ERROR_ACCESS_DENIED.to_hresult())
}

pub(crate) fn not_found(r: &windows::core::Result<HANDLE>) -> bool {
    matches!(r, Err(e) if e.code() == windows::Win32::Foundation::ERROR_FILE_NOT_FOUND.to_hresult())
}

/// An object some other party made first, under the name of event `parity` of `nonce`. `sddl` gives it a descriptor
/// (`None` keeps the default one).
pub(crate) fn squat(nonce: u128, parity: usize, sddl: Option<&str>) -> OwnedHandle {
    let name = event_name_wide(nonce, parity);
    let h = match sddl {
        None => unsafe {
            CreateEventExW(
                None,
                PCWSTR(name.as_ptr()),
                CREATE_EVENT_MANUAL_RESET,
                WRITER_ACCESS,
            )
        },
        Some(text) => {
            let w = wide(text);
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
            let psd = LocalSd(psd);
            let sa = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: psd.0 .0,
                bInheritHandle: false.into(),
            };
            unsafe {
                CreateEventExW(
                    Some(&sa),
                    PCWSTR(name.as_ptr()),
                    CREATE_EVENT_MANUAL_RESET,
                    WRITER_ACCESS,
                )
            }
        }
    }
    .unwrap();
    unsafe { OwnedHandle::from_raw_handle(h.0) }
}

// ---- names and nonces ------------------------------------------------------------------------------------------

#[test]
fn names_have_the_fixed_prefix_and_32_lowercase_hex_digits() {
    let n: u128 = 0x0123_4567_89ab_cdef_fedc_ba98_7654_3210;
    assert_eq!(
        event_name(n, 0),
        r"Global\BetterCam.FrameEvent.0123456789abcdeffedcba9876543210.0"
    );
    assert_eq!(
        event_name(n, 1),
        r"Global\BetterCam.FrameEvent.0123456789abcdeffedcba9876543210.1"
    );
    // a small nonce is padded, never shortened
    assert_eq!(
        event_name(0xAB, 1),
        r"Global\BetterCam.FrameEvent.000000000000000000000000000000ab.1"
    );
    for nonce in [0u128, 1, u128::MAX, n] {
        for p in 0..2 {
            let s = event_name(nonce, p);
            let rest = s.strip_prefix(EVENT_PREFIX).expect("prefix");
            let (hex, parity) = rest.split_once('.').unwrap();
            assert_eq!(hex.len(), 32, "{s}");
            assert!(
                hex.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "{s}"
            );
            assert_eq!(parity, p.to_string());
            assert_eq!(u128::from_str_radix(hex, 16).unwrap(), nonce);
        }
    }
    // only parity 0 and 1 exist
    assert_eq!(event_name(7, 2), event_name(7, 0));
    assert_eq!(event_name(7, 3), event_name(7, 1));
    // the wide form is the same text and a terminator
    let w = event_name_wide(n, 1);
    assert_eq!(*w.last().unwrap(), 0);
    assert_eq!(
        String::from_utf16(&w[..w.len() - 1]).unwrap(),
        event_name(n, 1)
    );
}

#[test]
fn the_awaited_event_is_the_one_the_next_signal_sets() {
    for s in 0..6u64 {
        // signal s+1 sets E[(s+1) % 2]
        let next = signal_steps(s + 1);
        assert_eq!(next[2], Step::Set(awaited_event(s)));
        // and signal s reset it before it stored s
        let this = signal_steps(s.max(1));
        if s >= 1 {
            assert_eq!(this[0], Step::Reset(awaited_event(s)));
            assert_eq!(this[1], Step::Store(s));
        }
    }
    assert_eq!(awaited_event(0), 1);
    assert_eq!(awaited_event(1), 0);
    assert_eq!(
        awaited_event(u64::MAX),
        0,
        "the count wraps, it does not panic"
    );
}

#[test]
fn nonces_are_random_nonzero_and_not_repeated() {
    let mut seen = std::collections::HashSet::new();
    let mut ones = [0u32; 128];
    const N: u32 = 2000;
    for _ in 0..N {
        let n = system_nonce().unwrap();
        assert_ne!(n, 0);
        assert!(seen.insert(n), "a nonce repeated");
        for (bit, c) in ones.iter_mut().enumerate() {
            *c += ((n >> bit) & 1) as u32;
        }
    }
    // every bit is a fair coin: 2000 draws, mean 1000, sd about 22; 700..1300 is more than 13 sd away
    for (bit, c) in ones.iter().enumerate() {
        assert!(
            (700..=1300).contains(c),
            "bit {bit} was set {c} times in {N}"
        );
    }
    // the two halves are not the same value
    let n = system_nonce().unwrap();
    assert_ne!(n as u64, (n >> 64) as u64);
}

// ---- the security descriptor -----------------------------------------------------------------------------------

struct Sd(Vec<u64>);

impl Sd {
    fn psd(&self) -> PSECURITY_DESCRIPTOR {
        PSECURITY_DESCRIPTOR(self.0.as_ptr() as *mut c_void)
    }
}

/// Owner, DACL and mandatory label of an event, through a handle that was opened for `READ_CONTROL`.
fn read_sd(h: HANDLE) -> Sd {
    let info = 0x1 /* OWNER */ | 0x4 /* DACL */ | 0x10 /* LABEL */;
    let mut need = 0u32;
    let _ = unsafe { GetKernelObjectSecurity(h, info, None, 0, &mut need) };
    assert!(need > 0);
    let mut buf = vec![0u64; (need as usize).div_ceil(8)];
    unsafe {
        GetKernelObjectSecurity(
            h,
            info,
            Some(PSECURITY_DESCRIPTOR(buf.as_mut_ptr() as *mut c_void)),
            need,
            &mut need,
        )
    }
    .unwrap();
    Sd(buf)
}

/// A SID parsed from its text, freed on drop.
struct Sid(PSID);
impl Sid {
    fn new(text: &str) -> Sid {
        let w = wide(text);
        let mut p = PSID::default();
        unsafe { ConvertStringSidToSidW(PCWSTR(w.as_ptr()), &mut p) }.unwrap();
        Sid(p)
    }
}
impl Drop for Sid {
    fn drop(&mut self) {
        unsafe {
            let _ = LocalFree(Some(HLOCAL(self.0 .0)));
        }
    }
}

fn is_sid(p: PSID, text: &str) -> bool {
    unsafe { EqualSid(p, Sid::new(text).0) }.is_ok()
}

/// (type, mask, sid) of each ACE of `acl`.
fn aces(acl: *mut ACL) -> Vec<(u8, u32, PSID)> {
    assert!(!acl.is_null());
    let count = unsafe { (*acl).AceCount };
    (0..count)
        .map(|i| {
            let mut p: *mut c_void = std::ptr::null_mut();
            unsafe { GetAce(acl, i as u32, &mut p) }.unwrap();
            let ace = p as *const ACCESS_ALLOWED_ACE;
            unsafe {
                (
                    (*ace).Header.AceType,
                    (*ace).Mask,
                    PSID(&(*ace).SidStart as *const u32 as *mut c_void),
                )
            }
        })
        .collect()
}

#[test]
fn the_events_carry_exactly_the_descriptor_of_the_design() {
    let s = FrameSignal::create().unwrap();
    let user = current_user_sid_text().unwrap();
    for parity in 0..2 {
        // the events exist, are manual-reset and start unset
        assert!(!is_set(s.handle(parity)));
        unsafe { SetEvent(s.handle(parity)) }.unwrap();
        assert!(is_set(s.handle(parity)));
        assert!(is_set(s.handle(parity)), "a manual-reset event stays set");
        unsafe { ResetEvent(s.handle(parity)) }.unwrap();
        assert!(!is_set(s.handle(parity)));

        // read it back through the kernel, on a handle opened for READ_CONTROL
        let h = open_for(s.nonce(), parity, 0x0002_0000).expect("the user may read the descriptor");
        let sd = read_sd(h);
        close(h);
        let psd = sd.psd();

        // owner: the token user, not the Administrators group an elevated token would default to
        let (mut owner, mut def) = (PSID::default(), BOOL(0));
        unsafe { GetSecurityDescriptorOwner(psd, &mut owner, &mut def) }.unwrap();
        assert!(is_sid(owner, &user), "owner is not the token user");

        // DACL: protected, three ACEs, none denying
        let mut ctl = 0u16;
        let mut rev = 0u32;
        unsafe { GetSecurityDescriptorControl(psd, &mut ctl, &mut rev) }.unwrap();
        assert_ne!(
            ctl & 0x1000,
            0,
            "the DACL is not protected (SE_DACL_PROTECTED)"
        );
        let (mut present, mut dacl, mut def) = (BOOL(0), std::ptr::null_mut(), BOOL(0));
        unsafe { GetSecurityDescriptorDacl(psd, &mut present, &mut dacl, &mut def) }.unwrap();
        assert!(present.as_bool());
        let dacl = aces(dacl);
        assert_eq!(dacl.len(), 3, "the DACL has {} ACEs", dacl.len());
        assert!(dacl.iter().all(|a| a.0 == 0), "only access-allowed ACEs");
        let mask_of = |sid: &str| -> u32 {
            dacl.iter()
                .filter(|a| is_sid(a.2, sid))
                .fold(0, |m, a| m | a.1)
        };
        // the user: modify, wait, read the descriptor; nothing that changes it
        assert_eq!(mask_of(&user), 0x0012_0002);
        // LOCAL SERVICE: wait only
        assert_eq!(mask_of("S-1-5-19"), 0x0010_0000);
        // OWNER RIGHTS: READ_CONTROL only, which takes the owner's implicit WRITE_DAC away
        assert_eq!(mask_of("S-1-3-4"), 0x0002_0000);
        // everyone else gets nothing
        for other in [
            "S-1-1-0",      // Everyone
            "S-1-5-11",     // Authenticated Users
            "S-1-5-32-545", // Users
            "S-1-5-32-544", // Administrators
            "S-1-5-18",     // SYSTEM
            "S-1-5-4",      // Interactive
            "S-1-5-2",      // Network
        ] {
            assert_eq!(mask_of(other), 0, "{other} has access");
        }
        // the label: medium, no write up
        let (mut present, mut sacl, mut def) = (BOOL(0), std::ptr::null_mut(), BOOL(0));
        unsafe { GetSecurityDescriptorSacl(psd, &mut present, &mut sacl, &mut def) }.unwrap();
        assert!(present.as_bool(), "no mandatory label");
        let sacl = aces(sacl);
        assert_eq!(sacl.len(), 1);
        assert_eq!(sacl[0].0, 0x11, "not a mandatory label ACE");
        assert_eq!(sacl[0].1, 0x1, "not SYSTEM_MANDATORY_LABEL_NO_WRITE_UP");
        assert!(
            is_sid(sacl[0].2, "S-1-16-8192"),
            "not the medium integrity level"
        );
    }
}

#[test]
fn the_descriptor_text_is_the_one_in_the_design() {
    assert_eq!(
        descriptor_text("S-1-5-21-1-2-3-1001"),
        "O:S-1-5-21-1-2-3-1001D:P(A;;0x00120002;;;S-1-5-21-1-2-3-1001)(A;;0x00100000;;;S-1-5-19)(A;;0x00020000;;;S-1-3-4)S:(ML;;NW;;;ME)"
    );
}

/// Run `f` on a thread that impersonates a copy of this process's token at low integrity level.
fn as_low_integrity<R: Send>(f: impl FnOnce() -> R + Send) -> R {
    std::thread::scope(|s| {
        s.spawn(|| {
            let mut tok = HANDLE::default();
            unsafe {
                OpenProcessToken(
                    GetCurrentProcess(),
                    TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ADJUST_DEFAULT,
                    &mut tok,
                )
            }
            .unwrap();
            let tok = unsafe { OwnedHandle::from_raw_handle(tok.0) };
            let mut dup = HANDLE::default();
            unsafe {
                DuplicateTokenEx(
                    HANDLE(tok.as_raw_handle()),
                    TOKEN_ALL_ACCESS,
                    None,
                    SecurityImpersonation,
                    TokenImpersonation,
                    &mut dup,
                )
            }
            .unwrap();
            let dup = unsafe { OwnedHandle::from_raw_handle(dup.0) };
            let low = Sid::new("S-1-16-4096");
            let label = TOKEN_MANDATORY_LABEL {
                Label: SID_AND_ATTRIBUTES {
                    Sid: low.0,
                    Attributes: 0x20, /* SE_GROUP_INTEGRITY */
                },
            };
            let size = std::mem::size_of::<TOKEN_MANDATORY_LABEL>() as u32
                + unsafe { GetLengthSid(low.0) };
            unsafe {
                SetTokenInformation(
                    HANDLE(dup.as_raw_handle()),
                    TokenIntegrityLevel,
                    &label as *const _ as *const c_void,
                    size,
                )
            }
            .unwrap();
            unsafe { SetThreadToken(None, Some(HANDLE(dup.as_raw_handle()))) }.unwrap();
            let r = f();
            let _ = unsafe { SetThreadToken(None, None) };
            r
        })
        .join()
        .unwrap()
    })
}

#[test]
fn a_low_integrity_process_of_the_user_may_wait_but_not_signal() {
    let s = FrameSignal::create().unwrap();
    let n = s.nonce();
    // the same user at medium integrity may modify (the control of this test)
    let h = open_for(n, 0, 0x0000_0002).expect("the user may modify the event");
    close(h);
    // a HANDLE is not `Send`: the thread hands back its address and the error as plain numbers
    let plain = |r: windows::core::Result<HANDLE>| r.map(|h| h.0 as usize);
    let (modify, wait, read) = as_low_integrity(|| {
        (
            plain(open_for(n, 0, 0x0000_0002)),
            plain(open_for(n, 0, 0x0010_0000)),
            // the label also blocks writing the descriptor
            plain(open_for(n, 0, 0x0004_0000 /* WRITE_DAC */)),
        )
    });
    let back = |r: &windows::core::Result<usize>| r.clone().map(|a| HANDLE(a as *mut c_void));
    assert!(
        denied(&back(&modify)),
        "a low-integrity process may set the event: {modify:?}"
    );
    assert!(
        denied(&back(&read)),
        "a low-integrity process may change the descriptor: {read:?}"
    );
    let wait = wait.expect("a low-integrity process may wait on the event");
    let wait = HANDLE(wait as *mut c_void);
    // a handle that only waits cannot set or reset
    assert!(unsafe { SetEvent(wait) }.is_err());
    assert!(unsafe { ResetEvent(wait) }.is_err());
    assert!(!is_set(s.handle(0)));
    close(wait);
}

#[test]
fn a_reader_handle_waits_and_cannot_signal() {
    let s = FrameSignal::create().unwrap();
    let ev = open_frame_events(s.nonce()).unwrap();
    // opened with SYNCHRONIZE only: the kernel refuses a set, a reset and a descriptor read
    let h = HANDLE(ev[0].as_raw_handle());
    let e = unsafe { SetEvent(h) }.unwrap_err();
    assert_eq!(e.code(), ERROR_ACCESS_DENIED.to_hresult());
    assert!(unsafe { ResetEvent(h) }.is_err());
    // but it sees the writer's signals
    assert_eq!(unsafe { WaitForSingleObject(h, 0) }, WAIT_TIMEOUT);
    unsafe { SetEvent(s.handle(0)) }.unwrap();
    assert_eq!(unsafe { WaitForSingleObject(h, 0) }, WAIT_OBJECT_0);
}

#[test]
fn opening_needs_a_nonzero_nonce_and_both_events_of_the_right_type() {
    assert_eq!(
        open_frame_events(0).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    // no such events
    let e = open_frame_events(system_nonce().unwrap()).unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::NotFound, "{e}");
    // only one of the two
    let n = system_nonce().unwrap();
    let _one = squat(n, 0, None);
    assert!(open_frame_events(n).is_err());
    // the name of something that is not an event (a mutex): opening an event of that name fails
    let m = system_nonce().unwrap();
    let name = event_name_wide(m, 0);
    let mutex = unsafe { CreateMutexW(None, false, PCWSTR(name.as_ptr())) }.unwrap();
    let _mutex = unsafe { OwnedHandle::from_raw_handle(mutex.0) };
    let _e1 = squat(m, 1, None);
    let e = open_frame_events(m).unwrap_err();
    assert_eq!(
        e.raw_os_error(),
        Some(6),
        "ERROR_INVALID_HANDLE expected, got {e}"
    );
}

// ---- collisions ------------------------------------------------------------------------------------------------

#[test]
fn a_taken_name_gets_a_new_nonce_and_nothing_stays_open() {
    let n1 = system_nonce().unwrap();
    let n2 = system_nonce().unwrap();
    let n3 = system_nonce().unwrap();
    let ok = system_nonce().unwrap();
    // n1: the second name is taken, so the first one was made and must be closed again
    let _t1 = squat(n1, 1, None);
    // n2: the first name is taken by an object we may not even open
    let _t2 = squat(n2, 0, Some("D:P"));
    // n3: the second name is taken again
    let _t3 = squat(n3, 1, None);

    // the three first nonces are all refused: no events, a clear error, no leak
    let e = with_nonces(vec![Ok(n1), Ok(n2), Ok(n3)], || FrameSignal::create().err());
    let e = e.expect("three collisions");
    assert_eq!(e.kind(), io::ErrorKind::AlreadyExists, "{e}");
    for (n, p) in [(n1, 0), (n2, 1), (n3, 0)] {
        let r = open_for(n, p, 0x0010_0000);
        assert!(
            not_found(&r),
            "an event of a refused nonce stayed open ({p}): {r:?}"
        );
    }

    // the fourth nonce would have worked, but only three are tried
    let e = with_nonces(vec![Ok(n1), Ok(n2), Ok(n3), Ok(ok)], || {
        FrameSignal::create().err()
    });
    assert!(e.is_some());
    assert!(not_found(&open_for(ok, 0, 0x0010_0000)));

    // a collision and then a free name works, and the refused nonce left nothing behind
    let s = with_nonces(vec![Ok(n1), Ok(ok)], || FrameSignal::create().unwrap());
    assert_eq!(s.nonce(), ok);
    assert!(not_found(&open_for(n1, 0, 0x0010_0000)));
    assert!(open_frame_events(ok).is_ok());
    drop(s);
    assert!(
        not_found(&open_for(ok, 0, 0x0010_0000)),
        "the events outlive the writer that made them"
    );
}

#[test]
fn a_zero_draw_costs_a_try_and_a_failing_generator_fails_at_once() {
    let ok = system_nonce().unwrap();
    let s = with_nonces(vec![Ok(0), Ok(ok)], || FrameSignal::create().unwrap());
    assert_eq!(s.nonce(), ok);
    drop(s);
    let e = with_nonces(vec![Ok(0), Ok(0), Ok(0), Ok(ok)], || {
        FrameSignal::create().err()
    });
    assert_eq!(e.unwrap().kind(), io::ErrorKind::AlreadyExists);
    let e = with_nonces(vec![Err(io::Error::other("no entropy")), Ok(ok)], || {
        FrameSignal::create().err()
    });
    assert!(e.unwrap().to_string().contains("no entropy"));
    assert!(not_found(&open_for(ok, 0, 0x0010_0000)));
}

// ---- the order of a signal -------------------------------------------------------------------------------------

fn heap_writer() -> (Heap, Shared, FrameSignal) {
    let mut heap = Heap::new();
    let shared = heap.shared();
    shared.init_writer(1, 1_000_000, 1).unwrap();
    let sig = FrameSignal::create().unwrap();
    sig.advertise(&shared);
    (heap, shared, sig)
}

#[test]
fn a_signal_resets_the_next_event_stores_the_count_then_sets_its_own() {
    let (_h, shared, sig) = heap_writer();
    assert_eq!(shared.header().features, FEATURE_FRAME_EVENT);
    for expect in 1..=5u64 {
        assert_eq!(sig.notify(&shared), expect);
    }
    let t = sig.trace.lock().unwrap().clone();
    assert_eq!(t.len(), 15);
    for (i, chunk) in t.chunks(3).enumerate() {
        let n = i as u64 + 1;
        let (this, next) = ((n & 1) as usize, ((n + 1) & 1) as usize);
        let steps: Vec<What> = chunk.iter().map(|s| s.what).collect();
        assert_eq!(
            steps,
            vec![
                What::Step(Step::Reset(next)),
                What::Step(Step::Store(n)),
                What::Step(Step::Set(this)),
            ],
            "signal {n}"
        );
        // before the reset: the count is still the old one
        assert_eq!(chunk[0].seq, n - 1);
        // before the store: the event the next signal sets was reset, the count is the old one
        assert!(
            !chunk[1].set[next],
            "signal {n}: the next event was not reset before the store"
        );
        assert_eq!(chunk[1].seq, n - 1);
        // before the set: the count is the new one, this signal's event is still unset (the signal before reset it)
        assert_eq!(chunk[2].seq, n, "signal {n}: the set came before the store");
        assert!(!chunk[2].set[this]);
        assert!(!chunk[2].set[next]);
        // the previous signal's event is set until this signal resets it
        if n > 1 {
            assert!(
                chunk[0].set[((n - 1) & 1) as usize],
                "signal {}'s event was not set",
                n - 1
            );
        }
    }
    // after the last signal: its event is set, the other is not
    assert!(is_set(sig.handle(1)));
    assert!(!is_set(sig.handle(0)));
    assert_eq!(shared.header().signal_seq, 5);
}

#[test]
fn two_threads_never_interleave_the_steps_of_their_signals() {
    let (_h, shared, sig) = heap_writer();
    let (shared, sig) = (Arc::new(shared), Arc::new(sig));
    let threads: Vec<_> = (0..2)
        .map(|_| {
            let (shared, sig) = (shared.clone(), sig.clone());
            std::thread::spawn(move || {
                for _ in 0..300 {
                    sig.notify(&shared);
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    let t = sig.trace.lock().unwrap().clone();
    assert_eq!(t.len(), 600 * 3);
    for (i, chunk) in t.chunks(3).enumerate() {
        let n = i as u64 + 1;
        let steps: Vec<What> = chunk.iter().map(|s| s.what).collect();
        assert_eq!(
            steps,
            signal_steps(n)
                .iter()
                .map(|s| What::Step(*s))
                .collect::<Vec<_>>(),
            "signal {n} was interleaved with another"
        );
    }
    assert_eq!(shared.header().signal_seq, 600);
}

// ---- every interleaving of writer and reader (the model) -------------------------------------------------------

#[derive(Default, Debug)]
struct Stats {
    runs: usize,
    event_wakes: usize,
    timer_wakes: usize,
    lost: usize,
}

#[derive(Clone, Copy)]
struct St {
    w: usize,
    seq: u64,
    ev: [bool; 2],
    /// The reader has loaded and is waiting.
    waiting: bool,
    loaded: u64,
    iter: usize,
}

struct Run {
    writer: Vec<Step>,
    wait: fn(u64) -> usize,
    rounds: usize,
    stats: Stats,
}

impl Run {
    /// Every order of the writer's steps and the reader's steps (load; wait: wake on the event when it is set, or on
    /// the timer at any moment). Errors name the first violation.
    fn go(&mut self, st: St) -> Result<(), String> {
        let (w_done, r_done) = (st.w == self.writer.len(), st.iter == self.rounds);
        if w_done && r_done {
            self.stats.runs += 1;
            return Ok(());
        }
        if !w_done {
            let mut n = st;
            match self.writer[st.w] {
                Step::Reset(p) => n.ev[p] = false,
                Step::Store(v) => n.seq = v,
                Step::Set(p) => n.ev[p] = true,
            }
            n.w += 1;
            self.go(n)?;
        }
        if r_done {
            return Ok(());
        }
        if !st.waiting {
            // load
            let mut n = st;
            n.loaded = st.seq;
            n.waiting = true;
            return self.go(n);
        }
        let h = (self.wait)(st.loaded);
        let mut n = st;
        n.waiting = false;
        n.iter += 1;
        if st.ev[h] {
            // woke on the event
            if st.seq == st.loaded {
                return Err(format!(
                    "spurious wake-up: loaded {}, still {}, event {h} set (writer step {})",
                    st.loaded, st.seq, st.w
                ));
            }
            self.stats.event_wakes += 1;
            self.go(n)?;
        }
        // the timer may fire at any moment while the event is not set
        if !st.ev[h] {
            self.stats.timer_wakes += 1;
            if w_done && st.seq != st.loaded {
                // the writer is done, the event is not set and the count moved: a wake-up was lost
                if st.seq - st.loaded < 2 {
                    return Err(format!(
                        "a wake-up was lost with only {} signal(s) after the load",
                        st.seq - st.loaded
                    ));
                }
                self.stats.lost += 1;
            }
            self.go(n)?;
        }
        Ok(())
    }
}

fn explore(wait: fn(u64) -> usize, signals: u64, rounds: usize) -> Result<Stats, String> {
    let mut run = Run {
        writer: (1..=signals).flat_map(signal_steps).collect(),
        wait,
        rounds,
        stats: Stats::default(),
    };
    run.go(St {
        w: 0,
        seq: 0,
        ev: [false; 2],
        waiting: false,
        loaded: 0,
        iter: 0,
    })?;
    Ok(run.stats)
}

#[test]
fn the_model_has_no_spurious_wake_up_and_loses_one_only_after_two_signals() {
    for (signals, rounds) in [(1u64, 3usize), (2, 4), (3, 5), (4, 5)] {
        let s = explore(awaited_event, signals, rounds)
            .unwrap_or_else(|e| panic!("{signals} signals, {rounds} rounds: {e}"));
        eprintln!("model {signals} signals, {rounds} rounds: {s:?}");
        assert!(s.runs > 0 && s.event_wakes > 0);
        if signals >= 2 {
            // the model reaches the lost wake-up, so the check above is not vacuous
            assert!(
                s.lost > 0,
                "the model never loses a wake-up: it explores too little"
            );
        } else {
            assert_eq!(s.lost, 0, "one signal can never be lost");
        }
    }
}

#[test]
fn the_model_finds_the_mistake_of_waiting_on_the_wrong_event() {
    // E[s % 2] was set by signal s itself: waking on it finds nothing new
    let wrong: fn(u64) -> usize = |s| (s & 1) as usize;
    let e = explore(wrong, 3, 5).expect_err("the wrong event must fail the model");
    assert!(e.contains("spurious"), "{e}");
}

#[test]
fn the_model_finds_a_writer_that_stores_after_the_set() {
    // the same reader against a writer that sets before it stores: a wake-up that finds nothing new
    let mut run = Run {
        writer: (1..=3u64)
            .flat_map(|n| {
                let [reset, store, set] = signal_steps(n);
                [reset, set, store]
            })
            .collect(),
        wait: awaited_event,
        rounds: 5,
        stats: Stats::default(),
    };
    let r = run.go(St {
        w: 0,
        seq: 0,
        ev: [false; 2],
        waiting: false,
        loaded: 0,
        iter: 0,
    });
    assert!(
        r.is_err(),
        "a set before the store must show up as a spurious wake-up"
    );
}

// ---- real threads, real events ---------------------------------------------------------------------------------

#[derive(Debug, Default)]
struct ReaderStats {
    events: u64,
    spurious: u64,
    missed: u64,
    timeouts: u64,
    loops: u64,
}

/// A reader that follows the reader rules of the design on a read-only mapping and the real events.
fn follow(path: &std::path::Path, stop: &AtomicBool) -> ReaderStats {
    let m = MappedFile::open(path, Access::ReadOnly).unwrap();
    let advert = |m: &MappedFile| {
        let v = validate_header(&m.shared().header(), m.len() as u64, None, None).unwrap();
        (
            v.wake.expect("the writer advertises its events"),
            v.writer_frames,
        )
    };
    let ev = open_frame_events(advert(&m).0.nonce).unwrap();
    let mut st = ReaderStats::default();
    while !stop.load(Ordering::Relaxed) {
        st.loops += 1;
        let (w, f) = advert(&m);
        let h = HANDLE(ev[awaited_event(w.signal_seq)].as_raw_handle());
        if unsafe { WaitForSingleObject(h, 100) } == WAIT_OBJECT_0 {
            if m.shared().header().signal_seq == w.signal_seq {
                st.spurious += 1;
            } else {
                st.events += 1;
            }
        } else {
            st.timeouts += 1;
            let (_, f2) = advert(&m);
            if f2 != f && !is_set(h) {
                st.missed += 1;
            }
        }
    }
    st
}

fn run_pair(name: &str, period: Duration, run_for: Duration) -> (u64, ReaderStats) {
    let path = temp_file(name);
    let _ = std::fs::remove_file(&path);
    let w = Arc::new(Writer::create(&path).unwrap());
    assert_eq!(w.header().features, FEATURE_FRAME_EVENT);
    w.set_state(WriterState::Live);
    let stop = Arc::new(AtomicBool::new(false));
    let reader = {
        let (path, stop) = (path.clone(), stop.clone());
        std::thread::spawn(move || follow(&path, &stop))
    };
    // the reader needs a moment to open its events
    std::thread::sleep(Duration::from_millis(50));
    let fmt = SlotFormat {
        width: 16,
        height: 16,
        stride: 16,
        subtype: crate::header::SUBTYPE_NV12,
    };
    let published = Arc::new(AtomicU64::new(0));
    let began = Instant::now();
    let mut n = 0u32;
    while began.elapsed() < run_for {
        w.publish(fmt, n as u64, |d| d.fill(1)).unwrap();
        published.fetch_add(1, Ordering::Relaxed);
        n += 1;
        let due = began + period * n;
        // an absolute schedule: sleep the long part, spin the rest
        loop {
            let left = due.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            if left > Duration::from_millis(3) {
                std::thread::sleep(left - Duration::from_millis(2));
            } else {
                std::hint::spin_loop();
            }
        }
    }
    // let the reader see the last one, then stop it
    std::thread::sleep(Duration::from_millis(30));
    stop.store(true, Ordering::Relaxed);
    // a final signal wakes a reader that waits
    w.set_state(WriterState::Paused);
    let st = reader.join().unwrap();
    (published.load(Ordering::Relaxed), st)
}

#[test]
fn a_reader_sees_every_signal_of_a_60_hz_writer_with_no_miss_and_no_spurious_wake_up() {
    let (published, st) = run_pair(
        "pair-60.bin",
        Duration::from_micros(16_667),
        Duration::from_secs(2),
    );
    eprintln!("60 Hz: published {published}, reader {st:?}");
    assert!(published >= 100);
    assert_eq!(
        st.spurious, 0,
        "a correct writer causes no spurious wake-up"
    );
    assert_eq!(st.missed, 0, "a frame was published and no wake-up came");
    assert!(
        st.events * 10 >= published * 9,
        "the reader woke {} times for {published} frames",
        st.events
    );
}

#[test]
fn a_1_khz_writer_makes_no_spurious_wake_up_and_no_more_wake_ups_than_signals() {
    let (published, st) = run_pair(
        "pair-1k.bin",
        Duration::from_millis(1),
        Duration::from_secs(2),
    );
    eprintln!("1 kHz: published {published}, reader {st:?}");
    assert!(published >= 1000);
    assert_eq!(
        st.spurious, 0,
        "a correct writer causes no spurious wake-up"
    );
    // each wake-up needs a signal, and each signal wakes a reader at most once
    assert!(
        st.events <= published + 2,
        "{} wake-ups for {published} signals",
        st.events
    );
    assert!(st.events > 0);
    // the reader's loops are bounded by the signals (plus a timer wake every 100 ms)
    assert!(st.loops <= published + 2 + st.timeouts);
}
