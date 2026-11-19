//! The frame event: the writer wakes the pump in the camera service when it has something new, instead of the pump
//! polling the header every millisecond (`docs/FRAME-EVENT.md`).
//!
//! # What is shared
//!
//! Two **manual-reset events**, `Global\BetterCam.FrameEvent.<nonce>.0` and `.1`, made by the app's [`crate::Writer`]
//! when it opens the frame file. The 128-bit random `<nonce>` is written to the header (`event_nonce_lo/hi`), which
//! only the installing user, LOCAL SERVICE and the administrators can read. `features` bit 0
//! ([`crate::header::FEATURE_FRAME_EVENT`]) says "the events exist"; `signal_seq` counts the signals.
//!
//! The events are only a hint, "look now". The seqlock, the validation and the heartbeat stay the source of truth, and
//! every failure to make, secure or signal the events leaves the writer working as it did without them: a reader that
//! does not find an advert polls.
//!
//! # Writer half ([`FrameSignal`], the app only)
//!
//! After every publish and every writer-state change ([`SignalCounter::notify`]), under one lock, with `n` the new
//! count:
//!
//! ```text
//! ResetEvent(E[(n + 1) % 2])     the event the *next* signal sets
//! signal_seq = n                 (release)
//! SetEvent(E[n % 2])
//! ```
//!
//! A reader that loaded `signal_seq == s` waits on `E[(s + 1) % 2]` ([`awaited_event`]). That event was reset by signal
//! `s` *before* `s` was stored, so it is never set by a stale signal: a correct writer causes no spurious wake-up at
//! all. Signal `s + 1` sets it, and it stays set (manual reset) until signal `s + 2` resets it, so the wait returns
//! whether it starts before or after the set. Only two signals inside the reader's load-to-wait window lose a
//! wake-up, and the reader's own deadline then takes the frame (never dropped, at most late). The model test in this
//! file runs every interleaving of the real step list ([`signal_steps`]).
//!
//! # Security descriptor
//!
//! Set by the app in the create call ([`descriptor_text`]), from the token user's SID:
//!
//! ```text
//! O:<user>D:P(A;;0x00120002;;;<user>)(A;;0x00100000;;;S-1-5-19)(A;;0x00020000;;;S-1-3-4)S:(ML;;NW;;;ME)
//! ```
//!
//! The user may modify the events and wait (`EVENT_MODIFY_STATE | SYNCHRONIZE | READ_CONTROL`). LOCAL SERVICE (the
//! camera service) may only wait (`SYNCHRONIZE`): it cannot set, reset or re-secure them. OWNER RIGHTS gets only
//! `READ_CONTROL`, which removes the owner's implicit `WRITE_DAC`. Other users get nothing. The medium mandatory
//! label with no-write-up lets a low-integrity process of the same user at most wait, which changes nothing on a
//! manual-reset event.
//!
//! # Reader half (runs in the camera service)
//!
//! [`event_name`], [`awaited_event`] and [`open_frame_events`]: format a name from a `u128`, open the two events with
//! `SYNCHRONIZE` and nothing else. The reader never takes a name from the file and never creates an object.

use std::ffi::c_void;
use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::Mutex;

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    GetLastError, LocalFree, SetLastError, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, HANDLE,
    HLOCAL, WIN32_ERROR,
};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::Cryptography::{BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG};
use windows::Win32::Security::{
    GetTokenInformation, TokenUser, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY,
    TOKEN_USER,
};
use windows::Win32::System::Threading::{
    CreateEventExW, GetCurrentProcess, OpenEventW, OpenProcessToken, ResetEvent, SetEvent,
    CREATE_EVENT_MANUAL_RESET, SYNCHRONIZATION_SYNCHRONIZE,
};

use crate::seqlock::Shared;

/// Start of both event names; the nonce and the parity follow.
pub const EVENT_PREFIX: &str = r"Global\BetterCam.FrameEvent.";

/// Nonces the writer tries before it gives up on events (a taken name means someone races the namespace).
pub const NONCE_TRIES: usize = 3;

/// `EVENT_MODIFY_STATE | SYNCHRONIZE`: what the writer asks for. `READ_CONTROL` is not asked for: the tests reopen the
/// event for that.
const WRITER_ACCESS: u32 = 0x0000_0002 | 0x0010_0000;

/// The name of event `parity` (0 or 1) of the nonce: [`EVENT_PREFIX`], 32 lowercase hex digits, `.`, `0` or `1`.
///
/// The function takes numbers and cannot be given a string, so a name read from a hostile file can only ever point at
/// an object under our own prefix.
pub fn event_name(nonce: u128, parity: usize) -> String {
    format!("{EVENT_PREFIX}{nonce:032x}.{}", parity & 1)
}

fn event_name_wide(nonce: u128, parity: usize) -> Vec<u16> {
    event_name(nonce, parity)
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect()
}

/// The event (0 or 1) a reader that loaded `signal_seq == s` waits on: the one the next signal sets.
pub fn awaited_event(signal_seq: u64) -> usize {
    (signal_seq.wrapping_add(1) & 1) as usize
}

/// One of the three steps of a signal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    Reset(usize),
    Store(u64),
    Set(usize),
}

/// The steps of signal number `n`, in the order the writer does them: reset the event the *next* signal sets, store
/// the count, set this signal's event. The one place that order is written down; the writer, the recording tests and
/// the model test all use it.
pub(crate) fn signal_steps(n: u64) -> [Step; 3] {
    [
        Step::Reset((n.wrapping_add(1) & 1) as usize),
        Step::Store(n),
        Step::Set((n & 1) as usize),
    ]
}

/// What a signal does to the world.
pub(crate) trait SignalOps {
    fn apply(&self, step: Step);
}

/// The count of signals, behind the lock that keeps two threads' signals (render and control) from interleaving
/// their three steps.
#[derive(Default)]
pub(crate) struct SignalCounter {
    n: Mutex<u64>,
}

impl SignalCounter {
    /// Do the next signal; returns its number.
    pub(crate) fn notify(&self, ops: &dyn SignalOps) -> u64 {
        let mut g = self.n.lock().unwrap_or_else(|e| e.into_inner());
        let n = g.wrapping_add(1);
        for step in signal_steps(n) {
            ops.apply(step);
        }
        *g = n;
        n
    }
}

fn win_err(e: windows::core::Error) -> io::Error {
    io::Error::from_raw_os_error(e.code().0 & 0xFFFF)
}

/// An owned pointer from `ConvertStringSecurityDescriptorToSecurityDescriptorW`.
struct LocalSd(PSECURITY_DESCRIPTOR);

impl Drop for LocalSd {
    fn drop(&mut self) {
        unsafe {
            let _ = LocalFree(Some(HLOCAL(self.0 .0)));
        }
    }
}

/// The SDDL text of the events' security descriptor for `user_sid` (see the module notes).
pub fn descriptor_text(user_sid: &str) -> String {
    format!(
        "O:{user_sid}D:P(A;;0x00120002;;;{user_sid})(A;;0x00100000;;;S-1-5-19)(A;;0x00020000;;;S-1-3-4)S:(ML;;NW;;;ME)"
    )
}

/// `S-1-...` text of the SID of this process's token user. The process token is used, not a thread's impersonation.
fn current_user_sid_text() -> io::Result<String> {
    let mut token = HANDLE::default();
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }.map_err(win_err)?;
    // SAFETY: `token` is a new handle that nothing else owns.
    let token = unsafe { OwnedHandle::from_raw_handle(token.0) };
    let t = HANDLE(token.as_raw_handle());
    let mut len = 0u32;
    // the first call only measures
    let _ = unsafe { GetTokenInformation(t, TokenUser, None, 0, &mut len) };
    if len == 0 {
        return Err(io::Error::other("TokenUser has no size"));
    }
    // a u64 buffer: TOKEN_USER holds pointers
    let mut buf = vec![0u64; (len as usize).div_ceil(8)];
    unsafe {
        GetTokenInformation(
            t,
            TokenUser,
            Some(buf.as_mut_ptr() as *mut c_void),
            len,
            &mut len,
        )
    }
    .map_err(win_err)?;
    // SAFETY: the buffer was filled by GetTokenInformation(TokenUser) and is 8-aligned.
    let user = unsafe { &*(buf.as_ptr() as *const TOKEN_USER) };
    let mut s = PWSTR::null();
    unsafe { ConvertSidToStringSidW(user.User.Sid, &mut s) }.map_err(win_err)?;
    let text = unsafe { s.to_string() }.map_err(|e| io::Error::other(e.to_string()));
    unsafe {
        let _ = LocalFree(Some(HLOCAL(s.0 as *mut c_void)));
    }
    text
}

/// 128 random bits from the system generator. Never 0 (a zero draw is an error: the header takes 0 as "none").
pub fn system_nonce() -> io::Result<u128> {
    #[cfg(test)]
    if let Some(n) = tests::draw_hooked_nonce() {
        return n;
    }
    let mut b = [0u8; 16];
    let st = unsafe { BCryptGenRandom(None, &mut b, BCRYPT_USE_SYSTEM_PREFERRED_RNG) };
    if st.0 != 0 {
        return Err(io::Error::other(format!("BCryptGenRandom: {:#x}", st.0)));
    }
    match u128::from_le_bytes(b) {
        0 => Err(io::Error::other("the random generator returned zero")),
        n => Ok(n),
    }
}

/// Why one attempt at making the pair did not work.
enum Attempt {
    /// A name was taken (by someone else, or by an object we may not open): try another nonce.
    Taken,
    Failed(io::Error),
}

/// The two events of a writer. Dropping it closes the writer's handles; the events live on while a reader holds one.
pub struct FrameSignal {
    nonce: u128,
    events: [OwnedHandle; 2],
    counter: SignalCounter,
    /// A log of what every step saw and did (tests only).
    #[cfg(test)]
    pub(crate) trace: Mutex<Vec<tests::Seen>>,
}

impl FrameSignal {
    /// Make the pair with a nonce from the system generator.
    pub fn create() -> io::Result<FrameSignal> {
        Self::create_with(&mut system_nonce)
    }

    /// Make the pair with nonces from `nonce` (the tests inject names that are taken). Up to [`NONCE_TRIES`]
    /// nonces are tried when a name already exists; then this fails with [`io::ErrorKind::AlreadyExists`] and
    /// nothing is left open.
    pub fn create_with(nonce: &mut dyn FnMut() -> io::Result<u128>) -> io::Result<FrameSignal> {
        let text = descriptor_text(&current_user_sid_text()?);
        let w: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let mut psd = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(w.as_ptr()),
                SDDL_REVISION_1,
                &mut psd,
                None,
            )
        }
        .map_err(win_err)?;
        let psd = LocalSd(psd);
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: psd.0 .0,
            bInheritHandle: false.into(),
        };
        for _ in 0..NONCE_TRIES {
            let n = nonce()?;
            if n == 0 {
                continue;
            }
            match create_pair(n, &sa) {
                Ok(events) => {
                    return Ok(FrameSignal {
                        nonce: n,
                        events,
                        counter: SignalCounter::default(),
                        #[cfg(test)]
                        trace: Mutex::new(Vec::new()),
                    })
                }
                Err(Attempt::Taken) => continue,
                Err(Attempt::Failed(e)) => return Err(e),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("the frame event names were taken {NONCE_TRIES} times"),
        ))
    }

    /// The nonce the events are named by.
    pub fn nonce(&self) -> u128 {
        self.nonce
    }

    /// Put the advert in the header: the nonce, then `features` (see [`Shared::advertise_frame_event`]).
    pub fn advertise(&self, shared: &Shared) {
        shared.advertise_frame_event(self.nonce);
    }

    /// Withdraw the advert (`features = 0`).
    pub fn withdraw(&self, shared: &Shared) {
        #[cfg(test)]
        self.record(shared, tests::What::Withdraw);
        shared.clear_features();
    }

    /// One signal: reset the event the next one sets, store the count in the header, set this one's event. Returns
    /// the number of the signal. Failing calls are ignored: a reader that is not woken takes the frame at its own
    /// deadline and counts the miss.
    pub fn notify(&self, shared: &Shared) -> u64 {
        self.counter.notify(&Ops {
            signal: self,
            shared,
        })
    }

    #[cfg(test)]
    pub(crate) fn handle(&self, parity: usize) -> HANDLE {
        HANDLE(self.events[parity & 1].as_raw_handle())
    }

    #[cfg(test)]
    fn record(&self, shared: &Shared, what: tests::What) {
        let h = shared.header();
        let state = |p: usize| {
            use windows::Win32::Foundation::WAIT_OBJECT_0;
            use windows::Win32::System::Threading::WaitForSingleObject;
            unsafe { WaitForSingleObject(self.handle(p), 0) == WAIT_OBJECT_0 }
        };
        self.trace.lock().unwrap().push(tests::Seen {
            what,
            features: h.features,
            state: h.writer_state,
            seq: h.signal_seq,
            frames: h.writer_frames,
            latest: h.latest_slot,
            set: [state(0), state(1)],
        });
    }
}

/// The real steps.
struct Ops<'a> {
    signal: &'a FrameSignal,
    shared: &'a Shared,
}

impl SignalOps for Ops<'_> {
    fn apply(&self, step: Step) {
        #[cfg(test)]
        self.signal.record(self.shared, tests::What::Step(step));
        let h = |p: usize| HANDLE(self.signal.events[p & 1].as_raw_handle());
        match step {
            Step::Reset(p) => {
                let _ = unsafe { ResetEvent(h(p)) };
            }
            Step::Store(n) => self.shared.set_signal_seq(n),
            Step::Set(p) => {
                let _ = unsafe { SetEvent(h(p)) };
            }
        }
    }
}

/// Make both events of `nonce` with the security descriptor in `sa`. Nothing is left open on any failure.
fn create_pair(nonce: u128, sa: &SECURITY_ATTRIBUTES) -> Result<[OwnedHandle; 2], Attempt> {
    let mut made: Vec<OwnedHandle> = Vec::with_capacity(2);
    for parity in 0..2 {
        let name = event_name_wide(nonce, parity);
        unsafe { SetLastError(WIN32_ERROR(0)) };
        let r = unsafe {
            CreateEventExW(
                Some(sa),
                PCWSTR(name.as_ptr()),
                CREATE_EVENT_MANUAL_RESET,
                WRITER_ACCESS,
            )
        };
        // read at once: it is how an existing object is reported
        let last = unsafe { GetLastError() };
        match r {
            Ok(h) => {
                // SAFETY: a new handle that nothing else owns; dropped (closed) on every path out.
                let h = unsafe { OwnedHandle::from_raw_handle(h.0) };
                if last == ERROR_ALREADY_EXISTS {
                    // somebody else's event: ours would not carry our security descriptor
                    return Err(Attempt::Taken);
                }
                made.push(h);
            }
            Err(e) => {
                let code = e.code();
                return Err(
                    if code == ERROR_ACCESS_DENIED.to_hresult()
                        || code == ERROR_ALREADY_EXISTS.to_hresult()
                    {
                        Attempt::Taken
                    } else {
                        Attempt::Failed(win_err(e))
                    },
                );
            }
        }
    }
    let e1 = made.pop().expect("two events");
    let e0 = made.pop().expect("two events");
    Ok([e0, e1])
}

/// Open both events of `nonce` for waiting: `SYNCHRONIZE` and nothing else. Fails when the nonce is 0, when either
/// event does not exist (the writer is gone, or this account may not open it), or when one is not an event. Nothing
/// is left open on failure.
pub fn open_frame_events(nonce: u128) -> io::Result<[OwnedHandle; 2]> {
    if nonce == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a zero nonce names no event",
        ));
    }
    let open = |parity: usize| -> io::Result<OwnedHandle> {
        let name = event_name_wide(nonce, parity);
        let h = unsafe { OpenEventW(SYNCHRONIZATION_SYNCHRONIZE, false, PCWSTR(name.as_ptr())) }
            .map_err(win_err)?;
        // SAFETY: a new handle that nothing else owns.
        Ok(unsafe { OwnedHandle::from_raw_handle(h.0) })
    };
    let e0 = open(0)?;
    let e1 = open(1)?;
    Ok([e0, e1])
}

#[cfg(test)]
pub(crate) mod tests;
