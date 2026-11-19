# FRAME-EVENT: wake the camera service's pump on a new frame instead of polling

Status 2026-11-19: phases 1 (`feed98c`), 2 (`29d1c6e`) and 3 are built; phase 3 (the DLL pump, uncommitted, **awaiting its
audit**) is described under "Phase 3 as built" below, and phase 4 (live) has not run. Phase 1's
review fixes: public names (no secret nonce), a second nonce for `.1`, `ERROR_INVALID_HANDLE` as a taken name, `features`
cleared before the header wipe. The rest of this document is the design as first written on 2026-11-18 against the working
tree on top of `ce45acc`. That tree includes the
uncommitted engine/DLL review fixes: `Writer::clear_latest`, "a frame captured before the stream attached is never shown",
and the stamping of bypass frames in `frames.rs`. Facts that could not be checked are marked **(unverified)**.

## Summary

* **Mechanism.** The app's `Writer` creates **two manual-reset events** named `Global\BetterCam.FrameEvent.<128-bit random
  hex>.0` and `Global\BetterCam.FrameEvent.<another, independent 128-bit random hex>.1`. It signals them in turn after every
  publish and every writer-state change. It writes the two random parts and a signal counter into new fields of the
  `frames.bin` header. **The nonces are not secrets** (see "What protects the events"): `\BaseNamedObjects` lets every local
  process, low integrity included, list the names. The protection is the unpredictable name plus each event's DACL.
* **DLL side.** The pump in the camera service opens both events with `SYNCHRONIZE` only. It waits on the one that the
  next signal will set, together with its own stop event and a high-resolution timer for the state machine's next
  deadline. The seqlock stays the only source of truth: an event only says "look now".
* **No protocol version bump, no installer change.** The new fields sit in unused header bytes (448..528). Old apps and
  old DLLs ignore them, and an old app's `init_writer` zeroes them. A feature bit says "events present". The app sets the
  event's security descriptor itself, in the create call; that DACL (not the `frames.bin` DACL, and not a secret name) is
  what keeps other users and low-integrity processes from signalling.
* **Bounded under attack.** A signaller that misbehaves cannot drive the pump faster than one event-driven poll per
  millisecond, which is today's worst case. Wake-ups without a new signal ("spurious") or frames published without a
  wake-up ("missed") switch the stream back to today's polling for 30 s. Failing to open the events also means polling.
* **The CPU target will probably still be missed.** By the code, a stream at the writer's rate polls about **350
  times/s**, not the 940/s that the P-perf section of `SPIKE-VCAM.md` states. The event cuts that to about **120
  wake-ups/s** at 1080p60. Estimated saving: **0.1 to 0.3 % of a core**. That gives 1080p60 about **1.5 to 1.8 %**, not 1 %.
  What remains is the per-frame copy (about 0.5 to 0.6 %) and Frame Server's own per-sample work (about 0.5 to 1 %). Both
  are estimates; Phase 0 measures them.
* **Biggest gain for slow cameras.** When the writer is slower than the consumer (the 14 fps camera of `PERF.md`), today's
  pump polls at about 1000/s between frames; with the event it wakes about as often as consumer fps plus camera fps.

## Recommendation

Build the **writer-created, randomly named pair of manual-reset events**, signalled in turn ("parity pair"), with a
`features` bit, two independent 128-bit nonces (one per event) and a `signal_seq` counter in the header. The DLL opens the events with `SYNCHRONIZE`
only and uses them only while the writer is Live. Each wait combines the event with a timer for the state machine's own
deadline (the repeat deadline, the slate pacing or the "starting" point), capped at 100 ms. There is no blind safety
poll. The DLL enforces three guards: at most one event-driven poll per millisecond; back to polling for 30 s after
spurious or missed wake-ups; and open attempts limited to one per 500 ms. Every failure ends in today's 1 ms polling, so
the worst case is today's behaviour. **Gate it on Phase 0** (one measuring session, no product code). If the
measurement confirms that polls cost under about 0.3 % of a core at 1080p60, the owner decides whether the latency and
slow-camera gains justify a change to privileged code, or re-baselines the 1 % target (open question 1).

**Phase 0 verdict (2026-11-19, unelevated part measured; WPR trace still to do, see `PERF.md` "Frame event, phase 0"):** 1 %
of the whole service at 1080p60 is **not reachable with the event**. Measured: service 1.30 to 1.55 %, pump thread 0.78 to
1.07 % (copy about 0.7 to 0.8 %), Frame Server's own threads 0.47 to 0.53 %, so even with zero polling cost the floor is about
1.2 to 1.3 %. The pump wakes about 340 times a second at 1080p60 (confirmed, 330 to 360 predicted); the event removes about 220/s
for **about 0.1 % of a core (0.05 to 0.26 %)**, so the gate's "polls under about 0.3 %" holds. The slow-camera case is bigger:
about 600 fewer wake-ups a second live, **0.12 to 0.36 %** of a pump at 0.74 to 1.06 %. These savings are estimates from measured
context switches and a fitted cost per wake-up, not measurements of an event build.

## What exists today (the facts the design builds on)

| Fact | Where |
|---|---|
| The pump thread first waits for a `RequestSample` token (condvar). Then it calls `FrameSource::poll` repeatedly. `Wait::Until(t)` sleeps on the condvar with a timeout, which relies on `timeBeginPeriod(1)` held for the whole pump life (`TimerPeriod`). `Wait::Soon` waits 1 ms on a high-resolution waitable timer. | `crates/vcam-source/src/stream.rs` `pump_loop_with`, `HiResTimer::wait_1ms`, `TimerPeriod` |
| After a live frame, `poll` answers `Until(next_ok)`, where `next_ok` = last capture + 3/4 period (no frame can qualify earlier). From then on it answers `Soon` until a frame arrives. With frames on show it also answers `Soon` past the repeat deadline (last new frame + 1.5 periods) until `last_emit + period` allows the repeat. | `frames.rs` `poll_inner` (end), `min_capture_gap`, `try_repeat` |
| So, at the writer's own rate, the pump polls from 3/4 period after a capture until the next publish (capture + period + ~1 ms): about 5 to 6 polls a frame at 60 fps and about 10 at 30 fps. That is **about 330 to 360 wake-ups/s in both cases**. A writer slower than 1/1.5 of the consumer rate keeps the pump at **~1000 polls/s** between repeats. These counts are derived from the code; WPR will confirm them. **(unverified)** | same |
| Every poll calls `validate_header`, which loads about 60 header atomics and builds a new `Vec` in `validate_advertised`: one allocation per poll. | `validate.rs`, `seqlock.rs` `header()` |
| The header ends at byte 448 (`SLOTS` 304 + 3 x 48). Bytes 448..4096 were unused; the frame event uses 448..528. `init_writer` zeroes the whole header on every writer open, in old and new apps alike. | `header.rs`, `seqlock.rs` `init_writer` |
| `validate_header` requires `protocol == PROTOCOL_VERSION` (1). A version bump makes an old DLL show the slate for a new app's file, and the reverse. | `validate.rs` |
| Writers: the engine (`Writer::create_default`, one `Arc<Writer>` for the life of the engine; `publish` on the render thread, `set_state` on the render and control threads), `vcam-tool feed`, and the tests. All go through `vcam_protocol::Writer`. | `engine.rs` 815, `output_writer.rs` `publish_planes`, `vcam-tool/src/feed.rs` |
| `frames.bin` DACL: SYSTEM and Administrators full; LOCAL SERVICE and the installing user `0x12008f` (read, write data, read attributes, `READ_CONTROL`). Other users have no access. The file has no explicit integrity label (implicit medium, no-write-up), so a low-integrity process of the same user can read it **(unverified for this file; standard MIC behaviour)**. | `SPIKE-VCAM.md` P-hardening 3 |
| Frame Server: `svchost -k Camera`, `NT AUTHORITY\LocalService`, session 0, **service SID type `UNRESTRICTED`** (not write-restricted). | `sc qc FrameServer`, `sc qsidtype FrameServer`, checked 2026-11-18 |
| From this user's medium-integrity shell without `SeCreateGlobalPrivilege`, creating a `Global\` **event** works (`created=True`, reopened by name). Creating a `Global\` **file mapping** is refused ("Access to the path is denied"). Plan 2.5 rejected kernel objects because of the mapping case; it does not apply to events. | checked 2026-11-18 (PowerShell `EventWaitHandle` and `MemoryMappedFile.CreateNew`) |
| Whether a pump in Frame Server (LocalService, session 0) can open a `Global\` event that a session-1 user created is **(unverified)**. The third-party "isolated namespace" report is unverified too. Two facts argue against isolation: the token is unrestricted, and plain LOCAL SERVICE file ACEs work for the DLL (an AppContainer would need an `ALL APPLICATION PACKAGES` ACE). | `PLAN.md` 4.3 |
| Today's measured camera-service CPU for our stream (P-perf, 3 runs): 1080p60 1.78 / 1.95 / 1.98 % of a core; 4K30 1.97 / 2.03 / 2.68 %. Per-thread at 4K30: 1.9 % in all, of which the pump is 1.36 % (1.16 user, 0.20 kernel) and the rest of Frame Server about 0.54 %. | `SPIKE-VCAM.md` P-perf |

## Requirements and constraints

From the brief, the plan and the code:

1. Correctness never depends on the event: the seqlock, the validation and the heartbeat decide what is shown, exactly as
   today.
2. The DLL stays correct and bounded when the event is missing, squatted, never signalled, signalled constantly, or
   replaced. Its worst case must not be worse than today's polling.
3. Multiple consumers: several pumps, possibly in several processes (two Frame Server instances during a service
   restart, two DLL builds after an Update) and several consumer apps all get every wake-up.
4. Mixed versions: an old app with a new DLL, and a new app with an old DLL, both keep working. No
   `PROTOCOL_VERSION` bump, because the equality check would break both directions.
5. The pump still wakes for repeats, slates, the repeat deadline and bypass takes, and stops within milliseconds.
6. Privileged code (anything that runs in Frame Server) stays small. It never creates named objects, never trusts a
   name from the file beyond a fixed format, and never asks for more than `SYNCHRONIZE`.
7. Other users and low-integrity processes get no right that could disturb the stream. Nobody can take the name before
   the app does.
8. Activation in consumer processes stays cheap: no event work outside a running pump.
9. Measured with the `PERF.md` method plus a per-thread split and a WPR trace of the camera service.

## Options considered

| Option | Verdict | Why |
|---|---|---|
| **A. Writer creates a pair of manual-reset events with a random 128-bit name in `Global\`; DLL opens them with `SYNCHRONIZE`; parity signalling** | **Chosen** | No privilege needed to create (checked). The app knows its own SID and writes an exact DACL. A random name cannot be taken in advance. Manual-reset wakes every waiter in every process, and a waiter cannot steal a wake-up from anyone. The DLL only opens by a name it formats itself and only waits. Everything fails safe to polling. |
| B. One auto-reset event | Rejected | An auto-reset event wakes exactly one waiter, so a second pump (another stream, another Frame Server process, two DLL builds) misses frames. Anyone with `SYNCHRONIZE` (any LocalService service, a low-integrity process of the user that listed the name) can **steal** wake-ups simply by waiting. Fixing the fan-out needs an extra hub thread in the DLL. |
| C. One manual-reset event, set by the writer, reset by the writer at the next publish | Rejected | The event stays set from one publish until the next, so a pump that has taken the frame and waits again returns at once and spins. |
| D. Semaphore released once per known waiter | Rejected | Wake-ups can be stolen as in B; the waiter count in the header races; it accumulates under errors. |
| E. DLL creates the event (LocalService owner); app opens it | Rejected | LocalService does not know the user. The DACL would have to grant Interactive or Authenticated Users, so any user could signal, or the installer would have to put the SID in HKLM for the DLL to read. The DLL would create `Global\` objects inside Frame Server, which is new privileged behaviour. The object would live only while a pump runs, so the app has to chase it. Several pumps or processes need several names. |
| F. Handle duplication (`DuplicateHandle` with `PROCESS_DUP_HANDLE` on the app granted to LocalService, or `IMFVirtualCamera::CreateSyncEvent` / `SendCameraProperty`) | Rejected | `PROCESS_DUP_HANDLE` on the app lets any LocalService service take over the user's process (by duplicating the process pseudo-handle). Pid reuse lets the header point the DLL at another process. Opening the AllUsers camera through `IMFVirtualCamera` needs admin, and its cross-process handle behaviour is unverified (plan 9.2 #7, #14). |
| G. Private namespace (as the install lock) | Rejected | A boundary must be satisfied by both tokens. The only SIDs that the user's token and LocalService's token share (Everyone, Authenticated Users) include every other user, so it protects nothing beyond A's random name and DACL. |
| H. `WaitOnAddress` on `signal_seq` | Rejected | Works only inside one process (documented). |
| I. File change notification on `frames.bin` / the data folder | Rejected | Writes through a mapped view do not produce timely change notifications (time stamps update lazily) **(unverified in detail)**. Log writes in `logs` would also wake it, and LocalService would need a directory handle on a folder the user can list. Unreliable and noisy. |
| J. Named pipe or ALPC from app to DLL | Rejected | The first creator owns a pipe name (squattable unless `FILE_FLAG_FIRST_PIPE_INSTANCE` plus a check). It needs a connection per pump, a non-blocking writer and buffer management. The per-frame path is heavier than `SetEvent`. More privileged code (a reader loop) for no gain. ALPC is undocumented for this use. |
| K. DLL-only: predict the writer's cadence and poll only around the expected publish (no protocol change) | Viable fallback, not chosen | No new cross-process object at all, so the smallest attack surface. But it keeps 1 ms detection jitter and `timeBeginPeriod(1)`, and it does poorly with irregular live cameras (auto exposure: 14 to 30 fps). Its gain is smaller than A's for the slow-camera case. Keep it in mind if the owner declines a protocol change (open question 2). |
| L. Shorter work per poll only (drop the per-poll `Vec`, skip `validate_header` when `writer_frames` did not move) | Complement, not a replacement | Cheap and DLL-only. It cuts the work per wake-up, not the number of wake-ups. Listed under "Related findings". |

## The design

### Objects and names

* Two **manual-reset** events per writer: `E0` and `E1`. Created non-signalled by the writer process when it opens the
  frame file (`Writer::open_file`, after `init_writer`), closed when the `Writer` is dropped.
* Name: `Global\BetterCam.FrameEvent.<nonce_i>.<i>`, where `<nonce_i>` is 32 lowercase hex digits of a 128-bit value from
  the system RNG (`BCryptGenRandom` with `BCRYPT_USE_SYSTEM_PREFERRED_RNG`) and `<i>` is `0` or `1`. **Each event has its
  own nonce** (`nonce_0` for `.0`, `nonce_1` for `.1`, drawn independently), so the name of `.0`, which anyone can list,
  says nothing about the name of `.1`, and nobody can create `.1` in the gap between the two creates. The prefix is a
  constant in `vcam-protocol`. The DLL **never takes a name string from the file**: it reads four `u64`, refuses a zero
  nonce, and formats the names itself. A hostile writer can therefore only name objects under our own prefix. Waiting on
  such an object changes nothing, and opening one of another type fails (`ERROR_INVALID_HANDLE`).
* Creation: `CreateEventExW(sa, name, CREATE_EVENT_MANUAL_RESET, EVENT_MODIFY_STATE | SYNCHRONIZE)`. If either name
  is taken (`ERROR_ALREADY_EXISTS`; `ERROR_ACCESS_DENIED` for an object we cannot open; `ERROR_INVALID_HANDLE` for a name
  held by an object of another type, such as a mutex), close both handles, draw two new nonces and retry, at most 3
  attempts. After that the writer runs without events (`features` bit off, poll mode in the DLL) and logs one line. With
  128 random bits a collision means somebody is enumerating and racing the namespace, not chance.
* Namespace: `Global\`, because the pump runs in session 0 and the app in the user's session; `Local\` names differ
  between them. Creating a `Global\` event needs no privilege (checked above). Opening it from Frame Server is
  **(unverified)** and is the first live check of Phase 4. If the open fails, the stream stays in poll mode and logs once.

### Security descriptor

Made by the app at creation (in the create call, never applied afterwards), from the token user's SID
(`GetTokenInformation(TokenUser)`). It does not depend on whether the app runs elevated:

```text
O:<user SID>
D:P(A;;0x00120002;;;<user SID>)     user: EVENT_MODIFY_STATE | SYNCHRONIZE | READ_CONTROL
   (A;;0x00120000;;;S-1-5-19)        LOCAL SERVICE: SYNCHRONIZE | READ_CONTROL (wait; read the owner)
   (A;;0x00020000;;;S-1-3-4)         OWNER RIGHTS: READ_CONTROL only (removes the owner's implicit WRITE_DAC)
S:(ML;;NW;;;ME)                      medium integrity, no write-up
```

* **The names are public.** `\BaseNamedObjects` lets Everyone, and low-integrity processes, list the names in it. So any
  local process (another user, a low-integrity process) can read both nonces as soon as the events exist, whatever the
  DACL of `frames.bin` says. **The nonce is not a secret and the `frames.bin` DACL does not protect it.** What protects the
  events is (1) the **unpredictable name** (nobody can create the name beforehand, and the `.1` name is independent of the
  `.0` name) and (2) the **event DACL** below. The reader (phase 3) therefore treats the name as public: its guards (open
  only while the writer is Live with a fresh heartbeat, the demotion rule, open pacing) are **hard requirements, also
  against other users**, not only against the installing user's own processes.
* **Other users** get nothing (no ACE): opening an event they listed is access denied.
* **Low-integrity processes of the same user** can list the names too, but the label blocks `EVENT_MODIFY_STATE`. They
  can get `SYNCHRONIZE` and wait, which on a manual-reset event changes nothing for anyone. (An object with no label is
  also treated as medium, no-write-up: the behaviour does not prove the label; `signal/tests.rs` reads the explicit label
  back.)
* **LOCAL SERVICE** (the pump, and any other LocalService service) can wait and read the descriptor (`SYNCHRONIZE |
  READ_CONTROL`): it cannot set, reset or re-secure the events. `READ_CONTROL` is there for the **owner check** of
  phase 3 (below): the DACL protects only objects the writer made, and the pump must be able to tell those from objects
  somebody else created under the same name after the writer was gone. It adds nothing a LocalService service could
  not already see: the descriptor is the one printed above, and the names are public anyway.
* **Administrators and SYSTEM** are not granted anything; they can take ownership with their privileges, as with
  anything.
* The owner is set explicitly to the user. An elevated admin token's default owner is Administrators, and `O:` makes
  both cases the same. The user can always be assigned as owner by its own token.
* Setting a mandatory label at or below the caller's own level in the create call needs no privilege **(unverified for
  `CreateEventExW`; Phase 1 reads it back)**.
* Needs the `Win32_Security_Authorization` and `Win32_Security_Cryptography` features of `windows` in `vcam-protocol`
  (writer side only; the DLL links but never calls them).

**Installer: no change.** The events are not files and are not persistent. Their security comes from the creating app
(an unpredictable name and the DACL it sets in the create call), not from the `frames.bin` DACL: the names are listable by
every local process. The elevated helper, the data-folder ACL model and the HKLM keys stay as they are. Nothing new runs
elevated.

### Protocol change

`PROTOCOL_VERSION` stays **1**. New fields in unused header bytes, added to `off`, `HeaderLayout` (with `offset_of!`
asserts) and `RawHeader`:

| Offset | Field | Type | Written by | Meaning |
|---|---|---|---|---|
| 448 | `features` | u32 | app | Bit 0 `FEATURE_FRAME_EVENT`: `E0`/`E1` exist and are signalled per the rules below. Other bits are reserved: writers write 0, readers ignore them. Stored **last** with Release, after both nonces. Cleared to 0 by `Writer::close` before the state goes `Closed`, and by `init_writer` before it wipes the header (Release fence, so a reader that sees a wiped byte sees `features == 0` after it, also on ARM64). |
| 452 | `reserved3` | u32 | - | 0 |
| 456 | `event_nonce_lo` | u64 | app | Low 64 bits of the nonce of event `.0` |
| 464 | `event_nonce_hi` | u64 | app | High 64 bits. A nonce is valid only when both halves together are non-zero. |
| 472 | `signal_seq` | u64 | app | Number of signals since `init_writer` (starts at 0). Changed only between the reset and the set (below). |
| 480 | `wake_mode` | u32 | DLL | Diagnostics: 0 polling (no advert, or the writer is not Live), 1 event, 2 polling after spurious wake-ups, 3 polling after missed wake-ups, 4 polling because the events could not be opened |
| 484 | `reserved4` | u32 | - | 0 |
| 488 | `wake_events` | u64 | DLL | Event wake-ups of the stream (diagnostics) |
| 496 | `wake_missed` | u64 | DLL | Missed wake-ups counted (diagnostics) |
| 504 | `wake_spurious` | u64 | DLL | Spurious wake-ups counted (diagnostics) |
| 512 | `event_nonce1_lo` | u64 | app | Low 64 bits of the nonce of event `.1` (its own random value, not derived from the first) |
| 520 | `event_nonce1_hi` | u64 | app | High 64 bits. The header's frame event fields end at 528. |

The DLL writes its fields only while `header_ok` (the existing rule), like the statistics. With several pumps the last
writer wins, as for `delivered` today.

**Parsing.** `validate_header` does **not** reject a header for any of these fields. A malformed advert must never turn
into a slate. A separate result `ValidHeader::wake: Option<WakeAdvert { nonces: [u128; 2], signal_seq: u64 }>` is `Some` only
when bit 0 is set and both nonces are non-zero. The nonces are read as `lo0, hi0, lo1, hi1, features, lo0, hi0, lo1, hi1` and accepted only when both
reads agree (a re-init in progress gives `None`). `writer_frames` is already in `RawHeader`; `ValidHeader` gains it for
the miss check.

**Writer rules** (`vcam_protocol::signal::FrameSignal`, held by `Writer`):

```text
notify():                                   # under a Mutex in FrameSignal: publish and set_state run on two threads
    n = signal_seq + 1
    ResetEvent(E[(n + 1) % 2])
    signal_seq.store(n, Release)
    SetEvent(E[n % 2])
```

* `Writer::publish` calls `notify()` after `Shared::publish` succeeded (the slot is even, `latest_slot` stored and
  `writer_frames` bumped first). `Writer::set_state` calls it after the state store. `Writer::close` stores
  `features = 0`, then the state `Closed`, then notifies, so a pump that waits sees the close at once.
* Heartbeats (`Writer::heartbeat`, every 100 ms while idle) do **not** notify: no wake-ups while nothing changes.
* `Writer::clear_latest` (engine M1) does not notify: it removes a frame and needs no wake-up.
* `Shared::publish` (the low-level call that tests use) does not notify.
* Cost to the app: two syscalls per frame (about 1 to 3 µs on the render thread) **(unverified)**.

**Reader rules** (pump, in event mode):

```text
s = signal_seq           # loaded with the header, before the poll (status() reads the header first)
f = writer_frames        # same load
p = poll(now)
if p == Wait::Frame(t):
    h = E[(s + 1) % 2]   # the event the next signal sets; the last signal reset it before storing s
    wait_any([stop, h, timer(min(t, now + WAIT_CAP))])     # array order = priority
    if woke on h:     if signal_seq == s   -> spurious  else -> event wake
    if woke on timer: if writer_frames != f and h is not signalled now  -> missed
```

Why no wake-up is lost with a correct writer, unless two signals fall inside the reader's load-to-wait window:

* If signal `s + 1` comes after the load, it sets `h`. `h` stays set (manual reset) until signal `s + 2` resets it, so
  the wait returns whether it starts before or after the set.
* If the load already saw `s`, then `h = E[(s + 1) % 2]` was reset by signal `s` **before** `s` was stored. So `h` is not
  stale-set: a correct writer produces **no spurious wake-up at all**, which is why spurious wake-ups can demote quickly.
* Two signals inside the window (`s + 1` sets `h`, `s + 2` resets it) lose the wake-up. The window is the time between a
  header load and the wait call (microseconds, or a preemption). Two signals need two publishes, or a publish next to a
  state change. The frame is then taken at the next deadline: at most half a period late, never dropped. It counts as
  missed only when the frame count moved.

Every pump has its own two handles. All waiters on a manual-reset event wake together, so any number of pumps in any
number of processes works, and two DLL builds loaded side by side after an Update work too.

### DLL changes

**`FrameSource` (frames.rs).** Split today's `Wait::Soon` in two:

* `Wait::Frame(t)`: "a new frame or state from the writer can change the answer; otherwise ask again at QPC `t`". It
  replaces `Soon` at the end of `poll_inner`, where `t` is the next time-based decision point:
  * frames on show, `now < deadline`: `t = deadline` (the repeat deadline = last new frame + 1.5 periods, also the
    bypass take);
  * frames on show, past the deadline, `now < last_emit + period`: `t = last_emit + period` (the repeat gate);
  * no frame yet: `t = live_since + 1.5 periods` (the "starting" slate point).
* `Wait::Soon` stays only for a torn repeat (`RepeatResult::Retry`): poll again in 1 ms.
* `Wait::Until(t)` is unchanged and **timer-only**: decimation (`next_ok`), slate pacing, no-buffer back-off. Frames
  that arrive before `next_ok` cannot qualify, so they must not wake the pump. A writer at twice the consumer's rate
  costs two wake-ups per emitted frame, not three.
* The source reports `(wake advert, signal_seq, writer_frames)` from the header it loaded, and whether the status is
  Live. Events are used **only in Live**; in every slate state the waits are timer-only, as today.

Polling mode treats `Frame(t)` as "wait `min(1 ms, t - now)`", which is exactly today's behaviour. A unit test pins that
equivalence.

**`wake.rs` (new, in vcam-source): one `Wake` per pump.** It owns:

* the pump's **stop event** (unnamed, auto-reset; `Pump::stop` sets it after setting `stop`). Today only the condvar
  wakes a stopping pump; a kernel wait needs a kernel object.
* one high-resolution waitable timer (`CREATE_WAITABLE_TIMER_HIGH_RESOLUTION`; relative due time from the QPC delta).
  It also replaces the condvar timeouts of `Until`, so `timeBeginPeriod(1)` is held **only in polling mode**. Either way
  the timer is precise; dropping the period request in event mode saves the machine its 1 kHz timer interrupt when
  nothing else asks for it **(effect unmeasured)**. If the high-resolution timer cannot be created, the pump stays in
  polling mode with today's fallback (`sleep(1 ms)`).
* the frame events (`Option<[OwnedHandle; 2]>` plus the two nonces they belong to), and the mode machine:

| Mode | Enters when | Waits |
|---|---|---|
| `Poll` | no advert, writer not Live, timer missing | today's: 1 ms for `Frame`/`Soon`, timer for `Until` |
| `Event` | advert present, events opened for that nonce, writer Live, not demoted | `Frame(t)`: stop + event + timer(min(t, now + 100 ms)); `Until`: stop + timer |
| `Demoted(until)` | more than 8 spurious wake-ups within 1 s, or 3 misses within 10 s | as `Poll` until 30 s have passed, then `Event` again |
| `OpenFailed(nonce, retry_at)` | `OpenEventW` failed for this nonce | as `Poll`; retry at a new nonce (at most once per 500 ms) or after 30 s |

Constants (in `wake.rs`, documented with these reasons):

| Constant | Value | Reason |
|---|---|---|
| `EVENT_MIN_GAP` | 1 ms | After an event wake-up the next wait includes the event only 1 ms later. A signaller can therefore not drive more than 1000 event-driven polls/s, which is today's worst case (a stalled writer already keeps today's pump at 1 kHz). A correct writer signals at most every ~4 ms (240 fps), so it is never delayed by more than the gap. |
| `WAIT_CAP` | 100 ms | Upper bound of any wait in event mode. The heartbeat (1 s) and the file re-checks keep working whatever the state machine asks for. |
| `SPURIOUS_LIMIT` | 8 per rolling 1 s | A correct writer produces none (see the protocol above), so 8 means a second writer or a hostile one. |
| `MISS_LIMIT` | 3 per rolling 10 s | The two-signals-in-the-window race is rare. Three in 10 s means the writer does not signal (or another writer replaced the nonce). |
| `DEMOTE_FOR` | 30 s | Polling time after a demotion, then event mode is tried again (logged, sampled). |
| `OPEN_RETRY` | 500 ms per changed nonce, 30 s for the same nonce | The same pacing as the file re-open. A writer that changes the nonce on every frame cannot make the pump call `OpenEventW` more than twice a second. |

`OpenEventW(SYNCHRONIZE | READ_CONTROL, FALSE, name)` for both names, done by the pump thread only (never at activation,
never in `DllGetClassObject` or in `CreatePresentationDescriptor`), followed by the **owner check** below. Both handles
are closed when the nonce changes, the writer leaves Live for longer than `WAIT_CAP`, the mode demotes, or the pump ends.
They are `OwnedHandle`s, so every path out, a panic included, closes them.

**Owner check (hard requirement).** The name comes from the file, but the *object* under a name is not necessarily the
writer's: the events vanish with their last handle, so after an app that died without `close` (the header keeps the
advert) anyone who listed the names can create them again with a DACL of their own, or put an object symbolic link
there. The pump must therefore check, on each opened handle (not on a path), that **the owner of the object is the
expected user**, and treat any other owner, or a failed read of the owner, as `OpenFailed` (poll, sampled log line,
usual pacing). The owner is read with `GetSecurityInfo(handle, SE_KERNEL_OBJECT, OWNER_SECURITY_INFORMATION)`, which
needs the `READ_CONTROL` that the descriptor gives LOCAL SERVICE.

*Where the expected owner comes from:* **the user ACE of the DACL of `frames.bin`**, which the installer wrote (an
administrator, once). That DACL is protected and holds SYSTEM, Administrators, LOCAL SERVICE and, when the installer was
given the user (`vcam-install` `acl.rs`; `0x12008f` for the last two), the installing user. The pump maps the pinned
`frames.bin` already; it reads the DACL (`GetSecurityInfo(..., DACL_SECURITY_INFORMATION)`; the file's `0x12008f` for
LOCAL SERVICE includes `READ_CONTROL`, but the DLL's open uses `ACCESS_READ` = `0x81`, so this needs a handle opened
with `READ_CONTROL` added, or a second one) and takes the single access-allowed trustee that is none of `S-1-5-18`,
`S-1-5-32-544`, `S-1-5-19`. None, or more than one, means no expected owner: **no events, polling**. Why this source
and no other:

* Only an administrator can change that DACL (the file is owned by Administrators; the user and LOCAL SERVICE have no
  `WRITE_DAC`), so nobody who can create an object in `\BaseNamedObjects` can make the expected value be theirs.
* **Not a SID recorded by the writer in the header.** The header is written by whoever may write `frames.bin` and is
  exactly what a hostile writer controls; the trust anchor must not come from the file whose advert is being checked.
  (It would also need a new header field.)
* Not the owner of `frames.bin` (Administrators, not the user), not the token of the pump (LOCAL SERVICE), and not "the
  logged-on user" (several sessions can exist; the camera service serves all of them).

*What it leaves:* an event owned by the right user can only be made by that user's own processes, which is the
"malware running as the installing user" row below, answered by the demotion rule. If the app runs as another account
than the one the installer named (a different admin account in an elevated install, say), the owners differ and the
camera polls: a safe failure, visible as `wake_mode` 4.

**`stream.rs`.** `pump_loop_with` replaces `timer.wait_1ms()` and the condvar `Until` wait with `wake.wait(poll_wait,
&src_report)`. `PumpShared` gains the stop event. `TimerPeriod` moves into `Wake` (held only in polling mode). The token
wait (condvar) and `post_sample` stay as they are.

**Logging.** New sampled log slots (`log.rs` `slot::WAKE_OPEN`, `slot::WAKE_DEMOTE`): events opened (first 8 hex digits
of the nonce), open failed (error code), demoted (reason and counts), back to event mode. All rate-limited.

### The heartbeat

**Complement, not replacement.** A signal says "something changed now"; it cannot say "the writer is still alive", and
an event object outlives a killed writer as long as the DLL holds handles to it. The heartbeat stays the liveness rule
(stale after 1 s gives "not running"). The pump still checks it at every poll. In event mode a stalled or killed writer
leaves the pump waking at the repeat deadline, at the repeat gate and at least every 100 ms, so a killed writer gives
the slate 1.0 s to 1.0 s + one period after its last heartbeat, as today. Heartbeat writes do not signal, so an idle
writer causes no wake-ups. The DLL-to-app direction (`consumer_active`, `consumer_heartbeat`) is unchanged; the engine
reads it once a frame.

### Versioning and mixed versions

| App | DLL | Result |
|---|---|---|
| new | new | Event mode while Live |
| old (no `features`) | new | `init_writer` of the old app zeroed bytes 448..528, so there is no advert: polling mode, exactly today's behaviour (`timeBeginPeriod` included) |
| new | old | The old DLL ignores bytes 448..528 and polls. The new app signals events that nobody waits on (two syscalls per frame). |
| new app restarts while a new DLL streams | | `init_writer` zeroes the advert, so the pump falls back to polling. The new nonce appears, the pump closes the old handles and opens the new ones (at most once per 500 ms). |
| two DLL builds loaded after an "Update virtual camera" | | Each pump has its own handles; manual-reset wakes both. |
| `vcam-tool feed` while the app runs (two writers; unsupported today too) | | Nobody owns the advert: each `init_writer` wipes the header and each writer then advertises its own events, so interleaved `advertise_frame_event` calls can leave `.0` of one writer next to `.1` of the other, and both writers store their own count into `signal_seq`. A pump that opens such a mixed pair (both events exist and pass the owner check, both are the user's) sees the count jump between two sequences: wake-ups with no signal behind them (spurious) or frames with no wake-up (missed). Either way the demotion rule ends in polling for 30 s: correct and bounded, not repaired (two writers stay unsupported). |

No `PROTOCOL_VERSION` change, no HKLM `Protocol` change, and no change to the app's "outdated" check: a changed DLL
build is already "outdated" by its content tag, so Update brings the new DLL.

## Behaviour under failure and hostile cases

| Case | What happens | Bound |
|---|---|---|
| Hostile process pre-creates the names | Impossible in advance: each name carries its own 128 random bits. Once the writer has created `.0`, the name is **listable by everyone** (`\BaseNamedObjects`), but the name of `.1` is independent of it, so nobody can squat `.1` between the two creates. If a create still finds a name taken (an event, an object of another type, an object we may not open), the writer retries with two new nonces, 3 attempts, then runs without events. | Polling, as today |
| App dies without `close`; somebody who listed the names creates them again, or puts an object symlink under them | The header keeps the advert and the events vanish with their last handle (the writer's), so the names are free again. Another user can create them with a DACL of their own (or link them to an object of their own); a pump that opens in the rest of the fresh-heartbeat window (up to 1 s), or restarts and finds that heartbeat, would wait on their objects: spurious wake-ups (demotion) or none (the pump's own deadline). The DACL cannot prevent it: it exists only on objects the writer made. **Phase 3 checks the owner of each opened event** against the user ACE of the `frames.bin` DACL (see "DLL changes"); a mismatch is `OpenFailed`. Without the check: bounded by the demotion rule and open pacing. | With the check: polling. Without it: ≤ 8 extra polls, then polling for 30 s |
| Hostile process of another user lists the names and opens the events | The names are public, but the DACL has no ACE for another user: access denied, so it can neither signal nor wait. It can still create objects under our prefix with other names, which the pump never opens (it opens only the two names in the file). The reader's guards (events opened only while the writer is Live with a fresh heartbeat, the demotion rule, open pacing) stay hard requirements against other users too, not only against the installing user's processes. | none |
| Low-integrity process of the user | It can list the names, but the label refuses `EVENT_MODIFY_STATE`. `SYNCHRONIZE` lets it wait, which on a manual-reset event changes nothing for anybody. | none |
| Another LocalService service waits on the events | It can wait and read the owner; on manual-reset events it cannot take a wake-up away. | none |
| Hostile writer (malware running as the installing user) sets both events and never resets them | Every event wake-up finds `signal_seq` unchanged, so they are spurious. More than 8 in 1 s demotes to polling for 30 s. The 1 ms gap applies in the meantime. | At most 8 extra polls, then today's polling; again every 30 s |
| Hostile writer signals constantly and bumps `signal_seq` (not spurious) | The 1 ms gap: at most 1000 event-driven polls/s. That is today's polling ceiling, which a stalled writer already reaches today. The seqlock and the decimation decide what is emitted. | ≤ 1 kHz polls |
| Hostile writer advertises events and never signals | At the deadline polls a moved `writer_frames` with the awaited event unset counts as missed. 3 in 10 s demote to polling. Before that, each such frame is taken at the next deadline: at most half a period late, never dropped. | ≤ 3 late frames per 30 s |
| Hostile writer changes the nonce constantly | Open attempts at most once per 500 ms. Meanwhile polling. | ≤ 4 opens/s per pump |
| Hostile writer puts nonces that name a foreign object | The name is always `Global\BetterCam.FrameEvent.<hex>.<i>`, so only objects under our prefix. Another type fails to open. A squatted event is only waited on, and its owner is checked (the row above). | Polling, or spurious leads to demotion |
| Header re-init while the pump reads the advert | Both nonces are read twice around `features`; a disagreement gives no advert this time. `init_writer` clears `features` (Release) before the wipe, so on a weakly ordered CPU a half-wiped nonce is never accepted next to the old bit. | One poll in polling mode |
| App crash between publish and `SetEvent`, or between `ResetEvent` and `SetEvent` | No wake-up for that frame. The pump wakes at its deadline (≤ half a period later) and takes the frame or a repeat. The heartbeat goes stale and gives the slate after 1 s. The events stay alive as long as pumps hold handles, then go away. | One late frame, then the slate as today |
| App crash, new app starts | `init_writer` zeroes the advert and a new nonce is published. The pump closes the old handles at the nonce change. | none |
| Frame Server restart or crash | Pumps end and close their handles. The new pumps open by the nonce in the file. The app is not affected: its handles keep the events alive. | none |
| Frame Server cannot open `Global\` objects (namespace isolation, unverified) | `OpenFailed`: polling, logged once, retried every 30 s or at a new nonce. | Today's cost |
| Event handle leak | Handles are owned values of the pump; every way out closes them. A pump detached after `STOP_WAIT` keeps its handles (2 events, stop event, timer) until its thread ends. Checked by a handle-count test over 200 start/stop cycles. | Bounded by pump lifetime |
| App runs elevated | The token user's SID is in the DACL and set as owner. A High-integrity creator can set the medium label. Same behaviour as unelevated. | none |
| App unelevated | Creation in `Global\` needs no privilege (checked). | none |
| Second Better Cam instance | Same user: the single-instance plugin hands over to the first. Another user cannot open `frames.bin`, so it never becomes a writer. | none |
| Stop while waiting | The stop event is first in the wait array, so the stop takes effect at once (today: ≤ 1 ms or the condvar). | < 10 ms (test) |
| Panic in the wait code | Runs on the pump thread inside its `catch_unwind`: `MEError` to the consumer (existing), handles closed by drop. | as today |
| DLL loaded in a consumer process (probe) | No pump, so nothing is opened. | none |
| High-resolution timer unavailable | Polling mode with `sleep(1 ms)`, as today. | as today |

## Expected CPU

Reasoning (all per stream, numbers from `SPIKE-VCAM.md` P-perf unless marked):

* **Wake-ups.** Today (derived from the code, see above): about 360/s at 1080p60 and about 330/s at 4K30 with a matching
  writer, plus `timeBeginPeriod(1)`. Event mode: two per emitted frame (`Until(next_ok)` and the event), so **120/s at
  1080p60 and 60/s at 4K30**. With a writer slower than the consumer (live camera in a dim room): today about 1000/s,
  event mode about consumer fps + writer fps (e.g. 60 + 14).
* **Cost of a wake-up** (timer arm, wait, context switch in and out, the poll's ~60 atomics, one allocation, the slot
  check): an estimated 3 to 8 µs **(unverified; Phase 0 measures it)**. The measured kernel time of the whole pump at
  4K30 is 0.20 % of a core, which caps the kernel part of today's polling there.
* **Copy.** One NV12 copy per frame: 3.1 MB x 60 = 186 MB/s at 1080p60, 12.4 MB x 30 = 373 MB/s at 4K30. The pump's
  user time at 4K30 (1.16 %) is mostly that copy, so about 0.3 % per 100 MB/s: **about 0.55 % at 1080p60**.
* **Frame Server's own threads.** 0.54 % at 4K30 (30 samples/s). If that cost is per sample, it is **about 1 % at 60
  samples/s** **(unverified; this is the biggest unknown)**.

| | Today (measured, 3 runs) | Polling part removed (estimate) | Expected with the event |
|---|---|---|---|
| 1080p60, writer at 60 | 1.78, 1.95, 1.98 % | 240 wake-ups/s x 3-8 µs, plus the poll work: 0.1-0.25 % | **1.5-1.8 %** (target 1 %: probably missed) |
| 4K30, writer at 30 | 1.97, 2.03, 2.68 % | 270 wake-ups/s: 0.1-0.25 % | **1.8-2.4 %** (target 2 %: met or at the target, as today) |
| 1080p60 consumer, live camera at 14.4 fps | 2.1-2.5 % (`PERF.md`, includes the physical camera) | ~900 wake-ups/s: 0.3-0.7 % | **1.6-2.0 %** |

The app's side gains two syscalls per published frame (well under 0.05 % of a core at 60 fps, estimate). Latency: today's
detection waits up to 1 ms (p50 about 0.5 ms of the measured 1.1-2.4 ms). With the event it drops to one cross-process
wake-up (tens of µs, estimate), so the header's `latency_p50_us` should fall by about 0.5 ms.

**What must be measured** (Phase 0 before, Phase 4 after):

1. Pump wake-ups per second and their causes. WPR context switches of the `bettercam-pump` thread; after the change,
   also the new header counters.
2. CPU per wake-up and per frame of the pump thread, split into user and kernel time (per-thread counters plus WPR CPU
   Usage (Precise)).
3. The svchost split at **1080p60**: pump thread vs. Frame Server's own threads (only measured at 4K30 so far). This
   decides whether 1 % is reachable at all.
4. Copy share: WPR sampled stacks of the pump (`memcpy` inside `better_cam_vcam_1.*.dll`, which links the CRT
   statically).
5. Timer resolution requested by the Frame Server process before and after (`powercfg /energy`, elevated).
6. The engine's render-thread CPU with and without signalling (expected noise-level).

## Measurement plan

1. **PERF method** (unchanged, `PERF.md` "Method"): `perf_synth1080_none.json` and `perf_4k30_none.json` through
   `perf_batch.ps1`, 3 runs each, 20 s windows, consumer `p6b-consumer.exe`. Camera-service CPU = delta of the svchost
   process time minus the 10 s idle baseline. Same for the app process. Also one run of the live camera (`4a`) for the
   slow-writer case.
2. **Per-thread split**: `Win32_PerfRawData_PerfProc_Thread` for the FrameServer svchost pid (readable without admin, as
   the process counters), once a second over the same window. The pump is the thread named `bettercam-pump` (Rust sets
   the thread description). If the counter set does not show the name, map it by thread id from the DLL log
   **(unverified)**.
3. **WPR trace of the camera service** (elevated shell; the owner runs it, or a session that is allowed to elevate):
   `wpr -start CPU -filemode`, start the same script, wait for the settle time, 30 s, then
   `wpr -stop fs_<config>_<build>.etl`. In WPA: *CPU Usage (Precise)* filtered to the FrameServer svchost, grouped by
   thread, giving context-switch count, CPU ms, readying process and thread (the event wake-ups come from the app's
   render thread, the timer wake-ups from the idle thread / DPC). *CPU Usage (Sampled)* with stacks for the pump thread:
   share in `memcpy`, in `KeWaitForMultipleObjects` / timer paths, and in `validate_header`. Export with `wpaexporter`
   to CSV for the doc. Profile name and table names as of the WPR in Windows 11 **(unverified)**.
4. **Header counters** (after the change): `vcam-tool stats` prints `features`, `signal_seq`, `wake_mode`,
   `wake_events`, `wake_missed`, `wake_spurious`. Expected at 1080p60: `wake_events` ≈ 60/s, missed and spurious ≈ 0.
5. Same machine and the same caveats as `PERF.md` (busy machine: ±0.5 % per window). Report every run, not a mean.

## Test plan

### Protocol (`vcam-protocol`, unit tests)

* Layout: `offset_of!` asserts for every new field; u64 fields 8-aligned; all below `HEADER_SIZE`.
* Parsing: an all-zero header extension gives no advert; bit 0 with a zero nonce (either of the two) gives none; unknown
  feature bits are ignored; a header with a broken advert still validates (never a slate); either nonce read twice
  disagreeing gives none.
* Names: format, length, lowercase hex only, fixed prefix. The function takes a `u128` and cannot be given a string.
* `FrameSignal` create: both events exist with the SD above, read back through `GetKernelObjectSecurity` on the
  creator's handle and checked ACE by ACE with `EqualSid` (as `acl::check` does): owner, three ACEs, protected, medium
  no-write-up label. An `AccessCheck` with a **low-integrity duplicate of the test's own token** is refused
  `EVENT_MODIFY_STATE` and granted `SYNCHRONIZE`. An `AccessCheck` for LocalService (well-known SID in a constructed
  check, or by reading the DACL) grants `SYNCHRONIZE` only.
* Collisions: an injected nonce source that returns a name the test created first (an event, an object we may not
  open, or a mutex: `ERROR_INVALID_HANDLE`) gets two new nonces. Three collisions give `features = 0`, and no handle
  leaks (process handle count before and after). Each event has its own nonce.
* Signal order: a recording stand-in for the event calls shows `Reset(E[(n+1)%2])`, then `store(n)`, then
  `Set(E[n%2])`, for publish, `set_state` and close. `close` clears `features` before `Closed`. `heartbeat` and
  `clear_latest` do not signal.
* **Interleaving model test** (pure Rust, in-memory events): every interleaving of 3 writer signals with a reader's
  load/poll/wait steps. Asserts: no spurious wake-up ever; a lost wake-up only when at least two signals fall inside one
  load-to-wait window; after any lost wake-up the next deadline poll sees the frame.
* Real threads: a writer thread at 60 Hz and at 1 kHz, a reader thread using the real events and the reader rules, for
  2 s. At 60 Hz every signal is seen with no miss and no spurious wake-up; at 1 kHz the reader's polls are ≤ 1000/s.
* `init_writer` (old and new) zeroes 448..528 and clears `features` before the wipe; `Writer::drop` closes the event handles.

### DLL (`vcam-source`, the in-process rig of `tests.rs` and the `frames`/`stream` tests)

* `FrameSource`: `Frame(t)` is returned exactly where `Soon` was (except torn repeats), with `t` never past the repeat
  deadline (the existing `the_wait_never_sleeps_past_the_repeat_deadline` extended), never past `last_emit + period`
  while a repeat is due, and never past the "starting" point. `Until` cases are unchanged. In polling mode the sequence
  of wake times over a recorded 60 fps and a 14 fps writer equals today's (equivalence test).
* `Wake` with a fake clock and fake events: mode transitions (no advert to Poll; advert to Event; > 8 spurious in 1 s
  to Demoted, Event again after 30 s; 3 misses in 10 s to Demoted; open failure to OpenFailed with its retry pacing; a
  nonce that flips every millisecond gives ≤ 2 opens per 500 ms); `EVENT_MIN_GAP` under an always-set event (≤ 1000
  polls in 1 s); every wait ≤ 100 ms; stop ends a wait in < 10 ms.
* Rig (DLL through `DllGetClassObject`, a real `Writer` feeder):
  * 60 fps feeder into a 60 fps NV12 stream for 5 s: `wake_mode` 1, every frame delivered, 0 repeats, `wake_events`
    within ±5 % of the frames, wake-ups per emitted frame ≤ 2.5, latency p95 not worse than in polling mode;
  * 60 fps feeder into a 30 fps stream: still halved exactly (existing decimation test in event mode);
  * a feeder with signalling switched off (an old writer): `wake_mode` 0, identical results;
  * a hostile signaller thread (opens the feeder's events with the user's rights and sets both in a loop): frames still
    correct; the pump thread's CPU time (`GetThreadTimes`, exposed to tests) ≤ 1.2 x that of polling mode; `wake_mode`
    2 within 1 s;
  * a feeder that advertises and never signals: `wake_mode` 3 within 10 s, no frame lost (repeats ≤ the misses);
  * feeder closes: slate within one period; feeder dies (`stop_publishing` + no heartbeat): slate 1.0-1.1 s after the
    last heartbeat (existing test, now in event mode);
  * nonce changes (feeder re-created): event mode again within 1 s, old handles closed;
  * 200 start/stop cycles: process handle count back to the baseline (±2); `timeBeginPeriod` balanced (the
    `OPEN_PERIODS` counter);
  * stop while the pump waits on the event: `Pump::stop` returns in < 50 ms.
* With the old behaviour put back by hand, one at a time, the tests must fail: no `EVENT_MIN_GAP` fails the hostile
  test; no demotion fails the never-signal test (late frames keep coming); no stop event fails the stop-latency test;
  waiting on `E[s % 2]` instead of `E[(s+1) % 2]` fails the model test.
* The DLL suite run 7 times in a row with no flake (as in P2).

### Live (Phase 4)

1. Update from the app; the registered DLL is byte-identical to the build. `vcam-tool stats` with the app Live and a
   consumer: `features 1`, `wake_mode 1`, `signal_seq` growing, `wake_missed`/`wake_spurious` 0. **This is the check
   that LocalService in session 0 can open the user's `Global\` events.**
2. `capture-test --expect feed` at 1080p60 NV12, 720p30, RGB32 1080p60 and a 60-to-30 decimation: PASS, 0 torn.
   `feed-to-slate` (clean close: slate within one frame) and a killed feeder (slate about 1.0 s after the last
   heartbeat). `--expect slate` with no writer.
3. Windows Camera and Edge (the `PLAN` test page) at the same time on Better Cam for 2 minutes: both smooth, no PANIC
   in the logs.
4. Mixed versions: a feeder built from the commit before Phase 1 (old writer) with the new DLL gives `wake_mode 0` and
   PASS. The new app with the old DLL still registered (before Update) gives PASS.
5. Hostile, as the standard user: the dev command `vcam-tool signal-storm --seconds 20` (Phase 2) sets the live events
   in a loop. Frame Server CPU stays at or under the polling-mode figure, `wake_mode` shows 2, and `capture-test` still
   PASSes. If a second local account exists: as that user, opening the event by name is access denied.
6. Frame Server restart (an Update stops the service, as in P-hardening 2) while the app stays Live: the next stream is
   in event mode again, with the same nonce.
7. Measurements: the measurement plan above, before (Phase 0 numbers) and after.

## Build phases

Each phase is one coding session. **Privileged** = code that runs inside the camera service (Frame Server). Each such
phase needs an independent audit session afterwards, before the next phase builds on it.

| # | Phase | Files | Decisions taken in it | Checks | Privileged |
|---|---|---|---|---|---|
| 0 | **Measure first** (gate) | none in the product; harness scripts in `%TEMP%\opencode\perf*`; results added to `docs/PERF.md` | none | PERF method at 1080p60 and 4K30 (3 runs each) plus the live 14 fps run; per-thread split of the FrameServer svchost; WPR trace (elevated) of one 1080p60 run: pump wake-ups/s, CPU per wake-up, copy share, Frame Server's own share. Report whether 1 % is reachable and what the event can save. | no |
| 1 | **Protocol and writer** | `crates/vcam-protocol/src/header.rs` (fields, offsets, layout asserts, `FEATURE_FRAME_EVENT`), `validate.rs` (`WakeAdvert`, `writer_frames` in `ValidHeader`, double nonce read), `seqlock.rs` (loads, writer setters, DLL diagnostic setters; `init_writer` clears `features` and fences first, wipes 448..528 with atomic stores and the rest with `write_bytes`; `advertise_frame_event` clears `features` and fences first), new `signal.rs` (name formatting, `FrameSignal` create/notify/close with the SD (LOCAL SERVICE `SYNCHRONIZE | READ_CONTROL`), nonce source with a test hook, reader `open_frame_events(nonces)` with `SYNCHRONIZE`, the in-memory model and its test), `RawHeader::from_bytes` (a plain read never carries the advert: `features` is 0), `map.rs` (`Writer` holds `Option<FrameSignal>`; notify in `publish`/`set_state`/`close`; `features = 0` on close; `WriterOptions { signal, scratch }` for tools and tests; a path other than the default one never opens a file in `%ProgramData%\BetterCam`, and `scratch` takes an existing file only if it is empty or already a frame file), `lib.rs`, `Cargo.toml` (two `windows` features) | SD exactly as above; 3 attempts, two nonces each; notify under a mutex | `cargo test -p vcam-protocol` (all the protocol tests above), `cargo clippy -p vcam-protocol --all-targets` 0 warnings, `cargo fmt -p vcam-protocol --check`, `cargo check -p vcam-source -p vcam-tool -p better-cam` | **yes**: header parsing and `open_frame_events` run in the service; the writer half does not, but lives in the same crate. **Audit after.** |
| 2 | **App and tool** | `crates/vcam-tool/src/stats.rs` (print the new fields), `feed.rs` (`--no-signal`), new dev subcommand `signal-storm` (opens the live events with the user's rights and sets them in a loop for N seconds; refuses without a live advert), `src-tauri/src/engine.rs` (one log line at writer creation: events on or off and why; no behaviour change) | `signal-storm` ships in the dev tool only | `cargo test -p vcam-tool`, `cargo test -p better-cam` (the engine's writer tests create real events in temp-file writers), clippy, fmt | no |
| 3 | **DLL pump** (built 2026-11-19, uncommitted, awaiting audit; also `vcam-protocol` `signal/owner.rs`, `secure.rs`, `seqlock.rs`, `map.rs`, one `vcam-install` test) | `crates/vcam-source/src/frames.rs` (`Wait::Frame`, the Live/advert/`signal_seq`/`writer_frames` report, diagnostics written only while `header_ok`), new `wake.rs` (modes, constants, stop event, high-resolution timer for every timed wait, `timeBeginPeriod` only in polling mode, the owner check of the opened events), `stream.rs` (pump loop uses `Wake`; `Pump::stop` sets the stop event), `log.rs` (two sampled slots), `tests.rs` (rig tests above) | the constants table; events only while Live | `cargo test -p vcam-source` 7 runs with no flake; the "old behaviour put back" runs; clippy, fmt; `cargo test -p vcam-protocol` again | **yes**, the whole phase. **Audit after.** |
| 4 | **Live, measure, document** | `docs/SPIKE-VCAM.md` (new section "P-frame-event": what changed, live results, machine state), `docs/PERF.md` (after-numbers next to Phase 0), `docs/PLAN.md` 2.5 "Sync" paragraph (now: event plus polling fallback) | none; reports against the gate | Update from the app (one UAC prompt); the live checks above; the measurement plan; nothing left running | no new code; touches the machine's registration |

Phases 1 and 3 can only be audited separately if Phase 1 keeps the reader half of `signal.rs` small (format a name,
open with `SYNCHRONIZE`, nothing else) and the DLL's whole decision logic stays in `wake.rs`.

**Phase 3 hard requirements (the nonces are public, see "Security descriptor"):** the pump opens the events only while the
writer is Live with a fresh heartbeat (never from a slate state, never at activation); it applies the demotion rule
(spurious and missed wake-ups) and the open pacing (one attempt per 500 ms per changed pair, 30 s for the same pair); it
never lets an event replace the seqlock check; and it opens exactly the two names built from the two nonces in the file.
They hold against other users and low-integrity processes as much as against the installing user's own processes.

**Phase 3 as built** (details of the DLL's `wake.rs`; the tables above are the design, this is what the code does):

* **States.** `Poll` (no advert, writer not Live, no high-resolution timer, or between open attempts), `Event`, `Demoted`
  (30 s, then an open is tried again) and `OpenFailed` (retry at a changed nonce pair after 500 ms, at the same pair after
  30 s). `wake_mode` in the header: 0 while the pump polls or the writer is not Live, 1 only while it waits on the events,
  2 and 3 while demoted for spurious or missed wake-ups, 4 after a failed open or an owner check that failed (**also
  when the DACL of `frames.bin` names no single user: then no open is made at all**).
* **Wait set.** `Frame(t)` in `Event`: `[stop event, E[awaited_event(signal_seq)], high-resolution timer]` with the timer at
  `min(t, now + 100 ms)`; `Until(t)` in `Event` is `[stop, timer]` only (decimation, slate pacing and the no-buffer
  back-off are never woken by a frame). `Soon` and, in polling, `Frame` are one millisecond, as the pump always waited.
  `Until` in polling is the timer to `t` (it was the condvar). The stop event is **manual-reset** (the design said
  auto-reset): once set it stays set, and `Pump::stop` sets it after the flag. Without a stop event no wait lasts over 20 ms.
* **Gates.** The report that the source gives the wait (`Live`, the advert, `signal_seq` and `writer_frames` of the very
  header the poll loaded) is empty in every slate state, for a stale heartbeat, a writer that is not Live, a read-only file
  and a header that does not validate. `update()` opens only from a non-empty report, so the only call to the DACL read and
  to `OpenEventW` is made from the pump thread, from a wait, with a Live writer.
* **Gap and demotion.** After an event wake-up the next wait leaves the event out for 1 ms (`EVENT_MIN_GAP`: at most 1000
  event-driven polls a second). Spurious wake-ups (the count is where the poll loaded it) are counted in a rolling window of
  the last 9 (more than 8 within 1 s demotes); missed wake-ups in the last 3 (3 within 10 s demotes). A timer wake-up with
  a newer frame and the awaited event not set gets one more millisecond (`MISS_SETTLE`: the writer bumps the frame count a
  few microseconds before it sets the event) and counts as missed only if the event is still not set.
* **Timer resolution.** `Wake::new` takes `timeBeginPeriod(1)` (the pump always did); it is given back while the mode is
  `Event` and taken again on every other mode. It lives in `Os` (`TimerPeriod`), so a panic ends it.
* **Diagnostics.** `set_wake_stats` is called from the source, only while the last header validated: at once on a change
  of `wake_mode`, otherwise at most four times a second, and again whenever the file is reopened and when the pump ends.
* **Reading `frames.bin`'s DACL.** A second handle on the path with exactly `READ_CONTROL | FILE_READ_ATTRIBUTES` (the
  pinned, link-refusing open of `secure`, one attempt per open attempt), whose `FileIdentity` must equal the mapped
  handle's: a file that was replaced since the mapping is refused. LOCAL SERVICE has both rights in `0x12008f`.

**Owner check as built** (`vcam_protocol::signal::owner`): the expected owner is `expected_owner(sids of the access-allowed
ACEs)`: the single trustee that is none of `S-1-5-18`, `S-1-5-32-544`, `S-1-5-19` (the same SID twice counts once), and only
if it has the installer's `valid_sid` shape (`S-1-5-21-` or `S-1-12-1-`, four plain 32-bit parts); else no event is trusted.
`vcam-install` has a test that runs it on `acl::expected(FramesFile, machine(Some(u)))` (gives `u`) and on `machine(None)`
(gives none) and compares the two shape tests, so an installer ACL change cannot silently turn events off. Each event is
opened with `SYNCHRONIZE | READ_CONTROL` and read through the handle with `GetSecurityInfo(SE_KERNEL_OBJECT)`: the **owner
must equal the expected user** and the SACL must hold **exactly one explicit mandatory label of medium integrity or
higher** (`S-1-16-8192` and up; none, one below medium, or two labels all fail). Why the label and not the whole descriptor
compared with `descriptor_text(expected)`: a process cannot give an object a label above its own integrity level, so the
label is what a low-integrity process of the same user (which can make an event owned by the user) cannot imitate, and the
DACL adds nothing against a medium-integrity creator, who can write exactly that DACL (the demotion rule answers that case).
A whole-descriptor comparison would also turn every later change of the descriptor into "every older DLL polls". Both
events must pass; either failing is `OpenFailed`.

It also **checks the owner of each event it opened**, with `SYNCHRONIZE | READ_CONTROL`: the owner must be the single
non-system user ACE in the DACL of `frames.bin` (the installer's), else `OpenFailed` and polling. The names can be taken
over after an app that died without `close` (see "Behaviour under failure and hostile cases"). A test creates events
as another owner where the account may (an `O:` owner the token can assign, such as Administrators from an elevated
test) and one as the right owner, and the fake-owner case must poll.

## Open questions for the owner

1. **The 1 % target at 1080p60.** By the estimate above, the event leaves 1080p60 at about 1.5 to 1.8 %. The rest is the
   per-frame copy and Frame Server's own per-sample work, which is not our code. *Recommendation:* run Phase 0 first.
   If it confirms this, re-state the target as "the DLL's own threads ≤ 1 % of a core at 1080p60, ≤ 2 % at 4K30", read
   per thread. The whole svchost is not ours to tune.
2. **Build the event at all if Phase 0 shows polls cost under ~0.3 %?** The other gains are about 0.5 ms lower p50
   latency, a big cut for cameras slower than the consumer (~1000 to ~75 wake-ups/s), and no 1 kHz timer request from
   the service. The cost is a change to the shared protocol, about 300 lines in privileged code, and two audits.
   *Recommendation:* build it. The slow-camera case is the common real one (dim rooms, auto exposure), and every failure
   ends in today's behaviour. If the owner wants no protocol change, the DLL-only cadence prediction (option K) is the
   fallback, with smaller gains.
3. **Demotion policy** (8 spurious per second or 3 misses per 10 s give 30 s of polling, then a retry). *Recommendation:*
   keep it. It never makes things worse than today, and a correct writer triggers neither.
4. **Diagnostics in the header and in `vcam-tool stats`** (`wake_mode` and three counters, 32 bytes). *Recommendation:*
   keep them in release builds. They are the only way to see from the user's side that the service opened the events.
5. **`timeBeginPeriod(1)` only in polling mode.** *Recommendation:* yes. The high-resolution waitable timer gives the
   precision without it, and the service then stops forcing a 1 kHz system timer for as long as a stream runs.
6. **Signal on writer-state changes too** (Live/Paused/Closed). It costs two syscalls per change and makes a clean
   close show the slate at once instead of at the repeat deadline. *Recommendation:* yes, as designed.

## Related findings (not in this design's scope)

* `SPIKE-VCAM.md` P-perf says "60 wake-ups plus 940 timer polls a second". By the code it is about 330 to 360 polls/s
  with a matching writer, and about 1000/s with a slower one. Phase 0 settles it.
* Every poll allocates (`validate_advertised` builds a `Vec` inside `validate_header`) and reloads about 60 header
  atomics. The pump needs neither the advertised list nor most fields per poll. A DLL-only trim (validate once per
  `writer_frames`/state change, or a lighter `validate_header` without the list) would cut the cost per wake-up in both
  modes. It is worth a small privileged change of its own, measured in the same Phase 0 trace.
* With the event in place, the pump's two wake-ups per frame could become one for writers at the consumer's rate (wait
  on the event straight after an emission and drop early events below `next_ok`). Not worth the extra state until
  Phase 4 shows the remaining wake-up cost.

## Unverified facts (collected)

1. Frame Server's pump (LocalService, session 0) can open a `Global\` event created by the user in session 1 (first live
   check, Phase 4).
2. Standard users can list `\BaseNamedObjects` (only affects whether a name can be seen after creation; access is
   guarded by the DACL either way).
3. A low-integrity process of the user can read `frames.bin` (implicit medium label without no-read-up).
4. Setting the medium mandatory label in `CreateEventExW`'s security attributes needs no privilege (Phase 1 reads it
   back).
5. Pump wake-up counts today (330-360/s matching, ~1000/s slower writer) and the cost per wake-up (3-8 µs): derived from
   the code, not measured.
6. Frame Server's own work scales per sample (about 1 % at 60 fps); measured only at 4K30 (0.54 %).
7. The effect of dropping `timeBeginPeriod(1)` on the measured process CPU.
8. WPR profile and WPA table names as written in the measurement plan; per-thread counters showing the Rust thread
   name.
9. Change notifications are unreliable for writes through a mapped view (option I's rejection does not depend on it).
