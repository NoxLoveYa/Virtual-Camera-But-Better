//! Where the DLL gets its pixels: frames from the shared file `%ProgramData%\BetterCam\frames.bin`, or a slate.
//!
//! Cost rules (the DLL is also loaded in every consumer process for a short probe):
//! * [`advertised`] is one plain read of the 4096-byte header. No mapping, no thread.
//! * [`FrameSource`] maps the file; it exists only inside a running stream (the pump thread owns it).
//!
//! A [`FrameSource`] is a small state machine driven by the pump: [`FrameSource::poll`] says either "emit this"
//! ([`Poll::Ready`]) or "ask me again at this time" ([`Poll::Wait`]).
//!
//! | Situation | Output |
//! |---|---|
//! | file missing, bad magic or other invalid header, heartbeat older than 1 s, writer `closed`, hostile slot | slate "Better Cam is not running" |
//! | file only readable by this process (no write access: the DLL cannot announce itself, so the engine never publishes to it) | slate "Better Cam is not running", logged once |
//! | writer `paused` | slate "Camera paused" |
//! | writer `starting`, or live but no usable frame yet | slate "Starting cameraâ€¦" |
//! | live, new frame of the negotiated size, captured after this stream attached (see below) | the frame (never scaled) |
//! | live, no new frame within 1.5 frame durations | the last frame again (counted in `repeated`) |
//!
//! A frame captured before this stream attached is never shown (privacy): what the file holds when a consumer
//! attaches may be an old frame left by an earlier session, and the first poll must not hand it to a new app.
//! A frame counts when its capture time is at most [`attach_tolerance`] before the attach: the larger of 250 ms and
//! two frame periods. The newest frame in the file of a live writer is never as young as the attach: it was captured
//! one capture-to-publish latency before it was published (a USB camera, the engine's conversion and readback: tens
//! of milliseconds, 20 to 48 on the camera this was measured with, and seconds for a camera that lags), and the
//! attach lands anywhere in the gap before the next one (up to a whole frame period). A tolerance of the order of the
//! clock error (the 50 ms this was once) put the newest frame outside it in a fifth of the starts and showed the
//! "starting" slate for a frame or more. What the rule must keep out is a frame of an earlier session, seconds to
//! minutes old (the engine forgets the newest frame while no app reads, `Writer::clear_latest`, so that is the rare
//! case); so the tolerance is a few frame periods, not a few milliseconds. Until a frame inside it arrives the
//! "starting" slate is shown.
//! One copy per frame: the pixels go from the slot of the shared file straight into the buffer a [`FrameSink`]
//! hands out (in the stream: the buffer of the sample that is about to be posted), and only then is the slot's
//! sequence number checked. A copy the check rejects is never delivered. The source keeps no frame of its own: a
//! repeat copies the slot of the last frame again. A slot that holds a newer frame than the last one shown is never
//! a repeat: the live path takes it (a frame captured too soon after the last one is skipped only until the repeat
//! deadline, 1.5 frame durations after the last live frame, so a stalled camera cannot leave the stream with
//! nothing to show). Only a slot that cannot be read (a writer that died in the middle of it) or holds another
//! size sends a repeat to the newest slot. When no slot holds a frame of the negotiated size any more it shows the
//! "starting" slate.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use vcam_protocol::slate::{Slate, SlateKind};
use vcam_protocol::{
    is_stale, qpc_frequency, validate_header, wake_mode, Access, Disposition, Format, MappedFile,
    Read, ReadError, SlotData, ValidHeader, WriterState,
};

use crate::mediatype::{FrameFormat, PixelFormat};
use crate::vlog;
use crate::wake::{Diag, WakeReport, WakeSource};

#[cfg(test)]
thread_local! {
    static TEST_PATH: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// Make the calling thread use `path` instead of `%ProgramData%\BetterCam\frames.bin` (tests only). `None` goes
/// back to a path that never exists, so a test cannot touch a real feeder's file by accident.
#[cfg(test)]
pub fn set_test_path(path: Option<PathBuf>) {
    TEST_PATH.with(|p| *p.borrow_mut() = path);
}

/// The frame file this thread should use.
pub fn frames_path() -> PathBuf {
    #[cfg(test)]
    {
        TEST_PATH.with(|p| {
            p.borrow().clone().unwrap_or_else(|| {
                std::env::temp_dir()
                    .join("bettercam-test-no-such-dir")
                    .join(vcam_protocol::FRAMES_FILE_NAME)
            })
        })
    }
    #[cfg(not(test))]
    {
        vcam_protocol::frames_path()
    }
}

/// The folder `frames_path()` must lie in, as a verified location: `%ProgramData%` from the shell (a folder only an
/// administrator can change), or the temp folder in tests. The file is opened only when it really is
/// `<root>\BetterCam\frames.bin` on a local fixed disk, never through a link or junction.
pub fn trusted_root() -> PathBuf {
    #[cfg(test)]
    {
        std::env::temp_dir()
    }
    #[cfg(not(test))]
    {
        vcam_protocol::secure::program_data_dir()
    }
}

/// Media types to advertise: from the file's header, or the defaults when it is missing or invalid.
pub fn advertised() -> Vec<Format> {
    vcam_protocol::read_advertised_pinned(&frames_path(), &trusted_root())
}

/// Window in which a capture time read from the file is believed: this far before `now` and 1/20 s after it.
/// The file is not trusted, and one far-future value would hold every later sample time in the future.
const CAPTURE_PAST_SECS: u64 = 5;

/// The least a frame may have been captured before the stream attached and still be shown: 1/4 s ([`attach_tolerance`]).
const ATTACH_TOLERANCE_MIN_DIV: u64 = 4;

/// How long before the stream attached a frame may have been captured and still be shown (a frame older than that
/// is one left in the file by an earlier session, never shown): `max(250 ms, 2 x period)`, `period` in QPC ticks
/// of the negotiated frame rate. See the module docs for why it is not smaller.
pub fn attach_tolerance(freq: u64, period: u64) -> u64 {
    (freq / ATTACH_TOLERANCE_MIN_DIV).max(period.saturating_mul(2))
}

/// `capture_qpc` clamped to the window around `now` ([`CAPTURE_PAST_SECS`] back, 50 ms ahead). Never panics, for
/// any input.
pub fn clamp_capture(capture_qpc: u64, now: u64, freq: u64) -> u64 {
    let lo = now.saturating_sub(freq.saturating_mul(CAPTURE_PAST_SECS));
    let hi = now.saturating_add(freq / 20);
    capture_qpc.clamp(lo, hi.max(lo))
}

#[cfg(test)]
type CopyHook = Box<dyn FnMut()>;

#[cfg(test)]
thread_local! {
    /// Runs inside the read callback just before the frame is copied (tests: tear the read on purpose).
    static COPY_HOOK: std::cell::RefCell<Option<CopyHook>> = const { std::cell::RefCell::new(None) };
}

// ---- where the pixels go ---------------------------------------------------------------------------------------

/// The buffer one frame is copied into: rows of `pitch` bytes, `PixelFormat::rows` of them (NV12: the UV rows follow
/// the Y rows at the same pitch).
pub struct FrameBuf<'a> {
    pub data: &'a mut [u8],
    pub pitch: usize,
}

/// Where [`FrameSource::poll`] puts the pixels of an emission.
pub trait FrameSink {
    /// The buffer for the frame that is about to be copied, or `None` when there is none (the poll then emits
    /// nothing and is asked again). It is called again after a copy that was thrown away (a torn read) and must give
    /// the same buffer back until the owner has taken the frame. A buffer returned for a `Poll::Ready` holds the
    /// whole frame.
    fn buffer(&mut self) -> Option<FrameBuf<'_>>;
}

/// Rows and row width of the negotiated format, as the copies need them.
#[derive(Clone, Copy)]
struct Layout {
    w: usize,
    h: usize,
    row_bytes: usize,
    rows: usize,
}

impl Layout {
    fn of(f: &FrameFormat) -> Layout {
        Layout {
            w: f.width as usize,
            h: f.height as usize,
            row_bytes: f.pixel.stride(f.width) as usize,
            rows: f.pixel.rows(f.height) as usize,
        }
    }
}

/// The sink's buffer, if it can hold a frame of `l`. A sink that hands out a short buffer is a bug there; the frame
/// is not copied rather than written past the end.
fn dest<'a>(sink: &'a mut dyn FrameSink, l: &Layout) -> Option<FrameBuf<'a>> {
    let b = sink.buffer()?;
    let fits = b.pitch >= l.row_bytes && b.pitch.checked_mul(l.rows)? <= b.data.len();
    if !fits {
        crate::log::log_sampled(crate::log::slot::SINK_BUFFER, 600, || {
            format!(
                "frames: the sample buffer ({} bytes, pitch {}) is too small for a frame",
                b.data.len(),
                b.pitch
            )
        });
        return None;
    }
    Some(b)
}

/// Copy the frame in `data` (rows of `stride` bytes) into `dst`: one `memcpy` when both have the same pitch.
fn copy_slot(data: &SlotData<'_>, stride: usize, l: &Layout, dst: &mut FrameBuf<'_>) {
    let p = dst.pitch;
    if stride == p && l.rows * p <= data.len() && l.rows * p <= dst.data.len() {
        data.copy_to(0, &mut dst.data[..l.rows * p]);
    } else {
        for r in 0..l.rows {
            data.copy_to(r * stride, &mut dst.data[r * p..r * p + l.row_bytes]);
        }
    }
}

/// Copy a tight frame (`l.rows` rows of `l.row_bytes`) into `dst`.
fn copy_tight(src: &[u8], l: &Layout, dst: &mut FrameBuf<'_>) {
    let p = dst.pitch;
    if p == l.row_bytes {
        dst.data[..src.len()].copy_from_slice(src);
    } else {
        for r in 0..l.rows {
            dst.data[r * p..r * p + l.row_bytes]
                .copy_from_slice(&src[r * l.row_bytes..(r + 1) * l.row_bytes]);
        }
    }
}

// ---- slates --------------------------------------------------------------------------------------------------

static EMBEDDED: [&[u8]; 3] = [
    include_bytes!("../assets/slates/not_running.rle"),
    include_bytes!("../assets/slates/paused.rle"),
    include_bytes!("../assets/slates/starting.rle"),
];

fn kind_index(k: SlateKind) -> usize {
    match k {
        SlateKind::NotRunning => 0,
        SlateKind::Paused => 1,
        SlateKind::Starting => 2,
    }
}

/// The embedded 1920x1080 slate, decoded once per process. A decode failure (cannot happen with a built DLL) falls
/// back to a flat dark frame.
pub fn decoded_slate(kind: SlateKind) -> &'static Slate {
    static DECODED: [OnceLock<Slate>; 3] = [const { OnceLock::new() }; 3];
    DECODED[kind_index(kind)].get_or_init(|| {
        vcam_protocol::slate::decode(EMBEDDED[kind_index(kind)]).unwrap_or_else(|e| {
            vlog!("embedded slate {kind:?} does not decode: {e:?}");
            Slate::flat(vcam_protocol::slate::WIDTH, vcam_protocol::slate::HEIGHT)
        })
    })
}

// ---- consumer registry -----------------------------------------------------------------------------------------

/// Streams running in this process per frame file, so `consumer_active` goes to 0 only when the last one stops.
static ACTIVE: Mutex<Vec<(PathBuf, usize)>> = Mutex::new(Vec::new());

fn active_adjust(path: &PathBuf, delta: isize) -> usize {
    let mut g = match ACTIVE.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    if let Some(e) = g.iter_mut().find(|(p, _)| p == path) {
        e.1 = (e.1 as isize + delta).max(0) as usize;
        let n = e.1;
        if n == 0 {
            g.retain(|(p, _)| p != path);
        }
        n
    } else if delta > 0 {
        g.push((path.clone(), delta as usize));
        delta as usize
    } else {
        0
    }
}

// ---- the state machine ---------------------------------------------------------------------------------------

/// What an emission is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A new frame from the writer.
    Live,
    /// The previous live frame again.
    Repeat,
    Slate(SlateKind),
}

/// One frame to emit; its pixels are in the [`FrameSink`] buffer the poll filled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Emit {
    pub kind: Kind,
    /// QPC of the capture for a live frame, else 0 (latency statistics).
    pub capture_qpc: u64,
    /// QPC the sample of a live frame is stamped with, else 0: the capture time, moved later when a frame taken
    /// past the repeat deadline would otherwise start less than the minimum capture gap after the one before.
    pub stamp_qpc: u64,
    /// QPC when the decision was taken.
    pub at: u64,
}

impl Emit {
    /// QPC the sample is stamped with: the stamp of a live frame, else the time of the decision.
    pub fn sample_qpc(&self) -> u64 {
        if self.kind == Kind::Live {
            self.stamp_qpc
        } else {
            self.at
        }
    }
}

/// When to call [`FrameSource::poll`] again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wait {
    /// Not before this QPC, whatever the writer does: a timer only. Decimation (`next_ok`), slate pacing and the
    /// no-buffer back-off. A frame that arrives earlier cannot qualify, so it must not wake the pump.
    Until(u64),
    /// A new frame or state from the writer can change the answer; otherwise ask again at this QPC (the next
    /// time-based decision: the repeat deadline, the repeat gate, the "starting" point). With the writer's frame event
    /// the pump waits for it; without, it polls every millisecond, as it always did ([`crate::wake`]).
    Frame(u64),
    /// In about a millisecond: a torn repeat is tried again.
    Soon,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Poll {
    Ready(Emit),
    Wait(Wait),
}

enum Status {
    Slate(SlateKind),
    Live(ValidHeader),
}

enum ReadResult {
    New(u64),
    Nothing,
    /// The sink has no buffer right now: nothing was read.
    NoBuffer,
    /// The slot descriptor was stable but invalid.
    Invalid,
}

/// What a repeat found.
enum RepeatResult {
    /// The frame is in the sink.
    Copied,
    /// The copy was torn: try again at the next poll.
    Retry,
    /// The sink has no buffer right now: nothing was read.
    NoBuffer,
    /// The slot of the last frame holds a newer frame of the negotiated size (the writer reused the slot) and the
    /// newest slot did not give one the live path could take: that frame is in the sink and is a live frame, not a
    /// repeat. Carries its capture time, clamped.
    Newer(u64),
    /// No slot holds a frame of the negotiated size any more.
    Lost,
    /// The slot descriptor was stable but invalid.
    Invalid,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Shown {
    Nothing,
    Slate(SlateKind),
    Live,
}

const LAT_RING: usize = 256;

/// Per-stream view of the shared file.
pub struct FrameSource {
    path: PathBuf,
    fmt: FrameFormat,
    freq: u64,
    period: u64,
    map: Option<MappedFile>,
    last_open_try: u64,
    /// The last poll said "not running": reopen the file now and then in case the writer replaced it.
    not_running: bool,
    running: bool,

    /// The most recent `validate_header` of the mapped file passed. Everything the DLL writes into the file
    /// (consumer state, negotiated format, statistics) happens only while this is true: a foreign file or a file of
    /// another protocol version is never altered.
    header_ok: bool,

    live_since: u64,
    /// A live frame has been emitted and its slot (`last_id`) can still be shown again.
    have_frame: bool,
    /// (slot, writer pid, seq) of the last live frame taken: what a repeat copies again, and what is not taken twice.
    last_id: Option<(u32, u32, u64)>,
    last_new_seen: u64,
    /// Capture time (QPC) of the last live frame emitted: the next one must be captured a frame period later.
    last_capture: Option<u64>,
    /// Sample stamp (QPC) of the last live frame emitted: the next one is stamped a minimum capture gap later.
    last_stamp: Option<u64>,
    last_emit: u64,
    /// QPC when this source was made or started: a frame captured (more than the tolerance) before it is an old
    /// frame left in the file and is never shown.
    attach: u64,
    /// The "read-only" log line was written (once per source: the file is reopened every half second).
    read_only_logged: bool,

    /// The slate scaled to the negotiated size, kept while a slate is shown (a slate is shown every frame period)
    /// and freed as soon as a live frame is.
    slate_kind: Option<SlateKind>,
    slate_buf: Vec<u8>,

    lat: Vec<u32>,
    lat_next: usize,
    delivered: u64,
    repeated: u64,
    live_since_stats: u32,
    shown: Shown,

    /// What the last poll saw of the header for the pump's wait: Live or not, and the frame event advert.
    report: WakeReport,
    /// The pump's wait diagnostics as last given ([`WakeSource::publish_diag`]), written into the file while its
    /// header validates (and again whenever the file is reopened).
    wake_diag: Diag,
}

impl FrameSource {
    /// A source for the negotiated format `fmt`. Opens nothing yet.
    pub fn new(path: PathBuf, fmt: FrameFormat) -> FrameSource {
        let freq = qpc_frequency();
        let period = freq / fmt.fps.max(1) as u64;
        FrameSource {
            path,
            fmt,
            freq,
            period,
            map: None,
            last_open_try: 0,
            not_running: true,
            running: false,
            header_ok: false,
            live_since: 0,
            have_frame: false,
            last_id: None,
            last_new_seen: 0,
            last_capture: None,
            last_stamp: None,
            last_emit: 0,
            attach: vcam_protocol::qpc_now(),
            read_only_logged: false,
            slate_kind: None,
            slate_buf: Vec::new(),
            lat: Vec::with_capacity(LAT_RING),
            lat_next: 0,
            delivered: 0,
            repeated: 0,
            live_since_stats: 0,
            shown: Shown::Nothing,
            report: WakeReport::default(),
            wake_diag: Diag::default(),
        }
    }

    /// The stream started: count it as a consumer.
    pub fn start(&mut self) {
        if self.running {
            return;
        }
        self.running = true;
        self.attach = vcam_protocol::qpc_now();
        active_adjust(&self.path, 1);
        self.delivered = 0;
        self.repeated = 0;
        vlog!("frames: start {:?} path {}", self.fmt, self.path.display());
    }

    /// The stream stopped: publish final statistics and clear `consumer_active` when no other stream of this
    /// process uses the file.
    pub fn stop(&mut self) {
        if !self.running {
            return;
        }
        self.running = false;
        // what is written below is written only into a file whose header validates *now*
        self.header_ok = self.header_valid(vcam_protocol::qpc_now());
        self.publish_stats();
        // the pump normally ends with `Wake::finish`, which has put this in already; a pump that panicked has not
        // (this runs from `Drop` then): nothing waits on the events any more, so the header must not say it does
        self.wake_diag.mode = wake_mode::POLLING;
        self.write_wake_diag();
        let left = active_adjust(&self.path, -1);
        if left == 0 && self.header_ok {
            if let Some(m) = &self.map {
                m.shared().set_consumer_active(false);
            }
        }
        self.map = None;
        self.header_ok = false;
        vlog!(
            "frames: stop, delivered {} repeated {}",
            self.delivered,
            self.repeated
        );
    }

    pub fn delivered(&self) -> u64 {
        self.delivered
    }
    pub fn repeated(&self) -> u64 {
        self.repeated
    }

    fn open_map(&mut self) -> Option<MappedFile> {
        let root = trusted_root();
        match MappedFile::open_pinned(&self.path, &root, Access::ReadWrite) {
            Ok(m) => Some(m),
            Err(e) if vcam_protocol::secure::is_refused(&e) => {
                // a link, a junction, a hard-linked or misplaced file: nothing is mapped, read-only or not
                crate::log::log_sampled(crate::log::slot::FILE_REFUSED, 600, || {
                    format!("frames: file refused ({e})")
                });
                None
            }
            Err(e) => {
                // no write access (or file in use): a read-only view still shows frames, but the writer
                // learns nothing about us
                match MappedFile::open_pinned(&self.path, &root, Access::ReadOnly) {
                    Ok(m) => {
                        vlog!("frames: opened read-only ({e})");
                        Some(m)
                    }
                    Err(_) => None,
                }
            }
        }
    }

    /// Whether the mapped file's header passes [`validate_header`] at QPC `now`.
    fn header_valid(&self, now: u64) -> bool {
        self.map.as_ref().is_some_and(|m| {
            validate_header(
                &m.shared().header(),
                m.len() as u64,
                Some(self.freq),
                Some(now),
            )
            .is_ok()
        })
    }

    fn announce(&self) {
        if !self.header_ok {
            return;
        }
        if let Some(m) = &self.map {
            let s = m.shared();
            s.set_negotiated(&Format::new(
                self.fmt.width,
                self.fmt.height,
                self.fmt.fps,
                self.fmt.pixel.protocol_subtype(),
            ));
            s.set_consumer_active(self.running);
            s.set_consumer_heartbeat(vcam_protocol::qpc_now());
            s.set_stats(0, 0, self.delivered, self.repeated);
            self.write_wake_diag();
        }
    }

    /// The pump's wait diagnostics into the header: only into a file whose header validated at the last poll (like
    /// every other write of the DLL).
    fn write_wake_diag(&self) {
        if !self.header_ok {
            return;
        }
        if let Some(m) = &self.map {
            let d = &self.wake_diag;
            m.shared()
                .set_wake_stats(d.mode, d.events, d.missed, d.spurious);
        }
    }

    fn ensure_map(&mut self, now: u64) {
        let retry_gap = self.freq / 2;
        let want = self.map.is_none() || self.not_running;
        if want && (self.last_open_try == 0 || now >= self.last_open_try + retry_gap) {
            self.last_open_try = now;
            let m = self.open_map();
            let opened = m.is_some();
            if opened != self.map.is_some() {
                vlog!("frames: file {}", if opened { "opened" } else { "gone" });
            }
            self.map = m;
            self.last_id = None;
            // nothing is written before the header of this very file has been validated
            self.header_ok = self.header_valid(now);
            self.announce();
        }
    }

    fn status(&mut self, now: u64) -> Status {
        let checked = self.map.as_ref().map(|map| {
            validate_header(
                &map.shared().header(),
                map.len() as u64,
                Some(self.freq),
                Some(now),
            )
        });
        let was_ok = self.header_ok;
        self.header_ok = matches!(checked, Some(Ok(_)));
        if self.header_ok && !was_ok {
            // the header validates again (the writer started over and wiped it): what the pump published while it did
            // not, and what the wipe took out, goes back in
            self.write_wake_diag();
        }
        let v = match checked {
            None => return Status::Slate(SlateKind::NotRunning),
            Some(Ok(v)) => v,
            Some(Err(e)) => {
                crate::log::log_sampled(crate::log::slot::HEADER_INVALID, 600, || {
                    format!("frames: header invalid: {e:?}")
                });
                return Status::Slate(SlateKind::NotRunning);
            }
        };
        if v.state == WriterState::Closed || is_stale(&v, now) {
            return Status::Slate(SlateKind::NotRunning);
        }
        // A file this process can only read: it cannot announce itself, so the engine (which publishes only to a
        // file with a consumer) never sends a new frame, and what the file holds is an old one. Say so.
        if self.map.as_ref().is_some_and(|m| !m.shared().is_writable()) {
            if !self.read_only_logged {
                self.read_only_logged = true;
                vlog!("frames: the file is read-only for this process: no frames are shown (the engine cannot see this consumer)");
            }
            return Status::Slate(SlateKind::NotRunning);
        }
        match v.state {
            WriterState::Paused => Status::Slate(SlateKind::Paused),
            WriterState::Starting => Status::Slate(SlateKind::Starting),
            _ => Status::Live(v),
        }
    }

    /// Smallest capture-time distance between two emitted live frames: 3/4 of a frame period, so camera jitter of up
    /// to a quarter period never drops a frame at the writer's own rate, while a writer at twice the rate is halved.
    fn min_capture_gap(&self) -> u64 {
        self.period * 3 / 4
    }

    /// When the sink has no buffer: a sink that cannot make a sample (memory) has not got one a millisecond later
    /// either, and each try can allocate, so the next try is a quarter period away.
    fn no_buffer_wait(&self, now: u64) -> Wait {
        Wait::Until(now.saturating_add(self.period / 4))
    }

    fn leave_live(&mut self) {
        self.have_frame = false;
        self.last_id = None;
        self.live_since = 0;
        self.last_new_seen = 0;
        self.last_capture = None;
        self.last_stamp = None;
    }

    /// The oldest raw capture time a frame may have to be shown: the attach time less the tolerance. An older frame
    /// is one left in the file by an earlier session (see the module docs).
    fn oldest_capture(&self) -> u64 {
        self.attach
            .saturating_sub(attach_tolerance(self.freq, self.period))
    }

    /// The stamp for a live frame captured at `capture_qpc`, decided at `now`: the capture time, moved later when
    /// that is less than a minimum gap after the stamp of the frame before (or before it). A frame taken past the
    /// repeat deadline is not held back by the decimation, so it can be captured less than a gap after the last one
    /// (or before it), and its sample would overlap the one before (a sample lasts a frame period); it is stamped a
    /// gap after that one, never more than 50 ms after `now`. Every live frame goes through this, not only those:
    /// the decimation looks at the raw capture times, so a frame that follows a moved one can be captured before the
    /// moved stamp, and stamping it with its own capture time would run the stamps backwards.
    fn live_stamp(&self, capture_qpc: u64, now: u64) -> u64 {
        match self.last_stamp {
            Some(last) => {
                let wanted = last.saturating_add(self.min_capture_gap());
                if capture_qpc >= wanted {
                    capture_qpc
                } else {
                    wanted
                        .min(now.saturating_add(self.freq / 20))
                        .max(capture_qpc)
                        .max(last)
                }
            }
            None => capture_qpc,
        }
    }

    fn note_shown(&mut self, s: Shown) {
        if s != self.shown {
            vlog!("frames: showing {:?} (was {:?})", s, self.shown);
            self.shown = s;
        }
    }

    fn slate_poll(&mut self, kind: SlateKind, now: u64, sink: &mut dyn FrameSink) -> Poll {
        if self.last_emit != 0 && now < self.last_emit + self.period {
            return Poll::Wait(Wait::Until(self.last_emit + self.period));
        }
        let l = Layout::of(&self.fmt);
        let Some(mut dst) = dest(sink, &l) else {
            return Poll::Wait(self.no_buffer_wait(now));
        };
        if self.slate_kind != Some(kind) {
            let s = decoded_slate(kind);
            self.slate_buf = match self.fmt.pixel {
                PixelFormat::Nv12 => s.scale_nv12(self.fmt.width, self.fmt.height),
                PixelFormat::Rgb32 => s.scale_bgrx(self.fmt.width, self.fmt.height),
            };
            self.slate_kind = Some(kind);
        }
        copy_tight(&self.slate_buf, &l, &mut dst);
        self.note_shown(Shown::Slate(kind));
        Poll::Ready(Emit {
            kind: Kind::Slate(kind),
            capture_qpc: 0,
            stamp_qpc: 0,
            at: now,
        })
    }

    /// A live frame is on its way: the scaled slate is not needed until the next slate.
    fn drop_slate_cache(&mut self) {
        if self.slate_kind.take().is_some() {
            self.slate_buf = Vec::new();
        }
    }

    /// Take the newest frame if it is new. With `decimate` a frame captured less than [`min_capture_gap`] after the
    /// last one emitted is skipped (a faster writer is halved); without it the newest frame goes out whatever its
    /// capture time (the repeat deadline has passed: see [`FrameSource::poll_inner`]).
    ///
    /// [`min_capture_gap`]: FrameSource::min_capture_gap
    fn try_read(
        &mut self,
        v: &ValidHeader,
        now: u64,
        decimate: bool,
        sink: &mut dyn FrameSink,
    ) -> ReadResult {
        let Some(map) = &self.map else {
            return ReadResult::Nothing;
        };
        let l = Layout::of(&self.fmt);
        let subtype = self.fmt.pixel.protocol_subtype();
        let last_id = self.last_id;
        let last_capture = self.last_capture.filter(|_| decimate);
        let min_gap = self.min_capture_gap();
        let freq = self.freq;
        let oldest = self.oldest_capture();
        let pid = v.writer_pid;
        // The buffer is asked for before the read, not inside it: making a sample can allocate and log, and the
        // read must stay short.
        let Some(mut dst) = dest(&mut *sink, &l) else {
            return ReadResult::NoBuffer;
        };
        // The slot is copied straight into the sink's buffer and the sequence check follows: a read it rejects
        // leaves a mixed frame in a buffer that no sample has been made of, and that the next copy overwrites.
        let r = map.shared().read_latest(v, |m, data| {
            if m.width as usize != l.w || m.height as usize != l.h || m.subtype != subtype {
                return Disposition::Skip;
            }
            if Some((m.index, pid, m.seq)) == last_id {
                return Disposition::Skip;
            }
            // an old frame left in the file before this stream attached is never shown
            if m.capture_qpc < oldest {
                return Disposition::Skip;
            }
            // a faster writer is decimated by capture time: skip a frame captured too soon after the last one
            // (`last_capture` is `None` once the repeat deadline has passed: see `poll_inner`)
            if let Some(c) = last_capture {
                let cq = clamp_capture(m.capture_qpc, now, freq);
                if cq >= c && cq - c < min_gap {
                    return Disposition::Skip;
                }
            }
            #[cfg(test)]
            COPY_HOOK.with(|h| {
                if let Some(f) = h.borrow_mut().as_mut() {
                    f();
                }
            });
            copy_slot(data, m.stride as usize, &l, &mut dst);
            Disposition::Taken
        });
        match r {
            Ok(Read::Taken(m)) => {
                // confirmed by an unchanged even seq
                self.last_id = Some((m.index, pid, m.seq));
                ReadResult::New(clamp_capture(m.capture_qpc, now, freq))
            }
            Ok(_) | Err(ReadError::Torn) => ReadResult::Nothing,
            Err(ReadError::Invalid(e)) => {
                crate::log::log_sampled(crate::log::slot::SLOT_INVALID, 600, || {
                    format!("frames: slot invalid: {e:?}")
                });
                ReadResult::Invalid
            }
        }
    }

    /// Copy the last frame again: the slot of `last_id` while it still holds that very frame. A slot that holds a
    /// newer frame of the right size (the writer reused it) is not a repeat: the live path takes the newest frame
    /// past the repeat deadline whatever its capture time, and when it could not (the newest slot holds another
    /// size, or is torn) this is the best frame there is, so it is taken here as [`RepeatResult::Newer`], a live
    /// frame with its own capture time (a stalled writer would otherwise leave the stream with nothing to show). A
    /// slot that cannot be read (a writer that died in the middle of it, or another size in it) falls back to the
    /// newest slot, the best picture left.
    fn try_repeat(&mut self, v: &ValidHeader, now: u64, sink: &mut dyn FrameSink) -> RepeatResult {
        let (Some(map), Some((slot, _, last_seq))) = (&self.map, self.last_id) else {
            return RepeatResult::Lost;
        };
        let l = Layout::of(&self.fmt);
        let subtype = self.fmt.pixel.protocol_subtype();
        let pid = v.writer_pid;
        let oldest = self.oldest_capture();
        let freq = self.freq;
        // asked for before the reads, not inside them (see `try_read`)
        let Some(mut dst) = dest(&mut *sink, &l) else {
            return RepeatResult::NoBuffer;
        };
        // the pass over the slot of the last frame meets either that frame (a repeat) or, when the writer reused the
        // slot, a newer one (a live frame, never a repeat)
        let exact = std::cell::Cell::new(true);
        let moved = std::cell::Cell::new(false);
        // its capture time, clamped
        let newer = std::cell::Cell::new(0u64);
        let mut copy = |m: &vcam_protocol::SlotMeta, data: &SlotData<'_>| {
            if m.width as usize != l.w || m.height as usize != l.h || m.subtype != subtype {
                return Disposition::Skip;
            }
            if exact.get() {
                moved.set(m.seq != last_seq);
                if moved.get() {
                    newer.set(clamp_capture(m.capture_qpc, now, freq));
                }
            }
            // Any frame but the very one that is being repeated (a newer one in its slot, or the newest slot's after
            // the fall back) is new to this stream, so it must have been captured after the attach less the
            // tolerance (an old frame left in the file is never shown). The frame that was shown was checked when it
            // was taken.
            if (moved.get() || !exact.get()) && m.capture_qpc < oldest {
                return Disposition::Skip;
            }
            #[cfg(test)]
            COPY_HOOK.with(|h| {
                if let Some(f) = h.borrow_mut().as_mut() {
                    f();
                }
            });
            copy_slot(data, m.stride as usize, &l, &mut dst);
            Disposition::Taken
        };
        let shared = map.shared();
        let mut r = shared.read_slot(slot as usize, v, &mut copy);
        // the slot is gone (another size in it, nothing in it) or the writer is in the middle of it (a writer that
        // died there leaves it odd for good): the newest slot is the best picture left
        if !moved.get()
            && matches!(
                r,
                Ok(Read::Skipped(_)) | Ok(Read::NoFrame) | Err(ReadError::Torn)
            )
        {
            exact.set(false);
            r = shared.read_latest(v, &mut copy);
        }
        match r {
            Ok(Read::Taken(m)) => {
                // what was shown is now what the slot holds: it is not a new frame when it comes round again
                self.last_id = Some((m.index, pid, m.seq));
                if moved.get() {
                    RepeatResult::Newer(newer.get())
                } else {
                    RepeatResult::Copied
                }
            }
            Err(ReadError::Torn) => RepeatResult::Retry,
            Ok(_) => RepeatResult::Lost,
            Err(ReadError::Invalid(e)) => {
                crate::log::log_sampled(crate::log::slot::SLOT_INVALID, 600, || {
                    format!("frames: slot invalid: {e:?}")
                });
                RepeatResult::Invalid
            }
        }
    }

    /// Decide what to emit at QPC `now`; the pixels of an emission are in `sink` when this returns `Ready`. A panic
    /// inside is contained: the file is dropped and the "not running" slate is shown.
    pub fn poll(&mut self, now: u64, sink: &mut dyn FrameSink) -> Poll {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.poll_inner(now, &mut *sink)
        })) {
            Ok(p) => p,
            Err(_) => {
                vlog!("PANIC contained in FrameSource::poll");
                self.report = WakeReport::default();
                self.map = None;
                self.leave_live();
                self.not_running = true;
                self.slate_poll(SlateKind::NotRunning, now, sink)
            }
        }
    }

    fn poll_inner(&mut self, now: u64, sink: &mut dyn FrameSink) -> Poll {
        self.ensure_map(now);
        let v = match self.status(now) {
            Status::Slate(kind) => {
                self.report = WakeReport::default();
                self.not_running = kind == SlateKind::NotRunning;
                self.leave_live();
                return self.slate_poll(kind, now, sink);
            }
            Status::Live(v) => v,
        };
        // what the pump waits by: events are used only for a Live writer (with a fresh heartbeat, a writable file), and
        // `signal_seq` and `writer_frames` are the ones this very header load saw
        self.report = WakeReport {
            live: true,
            advert: v.wake,
            writer_frames: v.writer_frames,
        };
        self.not_running = false;
        if self.live_since == 0 {
            self.live_since = now;
        }
        let period = self.period;
        // A new frame is taken as soon as it appears (latency stays at a poll interval), but only one captured at
        // least min_capture_gap after the last one emitted: a faster writer is decimated on capture time, so
        // the cadence follows the frames and not the poll phase. No frame can qualify earlier than that, so wait.
        let next_ok = match self.last_capture {
            Some(c) => c
                .saturating_add(self.min_capture_gap())
                .min(now.saturating_add(period)),
            None => 0,
        };
        // Decimation holds only until the repeat deadline. Past it the newest frame goes out as a live frame
        // whatever its capture time: a stalled camera that left two decimated frames after the last one shown would
        // otherwise be skipped for good (the writer alternates between two slots, so the slot of the last frame holds
        // a newer one and cannot be repeated), and the stream would deliver nothing while the app keeps the
        // heartbeat alive. The capture time of such a frame can be after the take of the last one (up to 50 ms), so
        // `next_ok` can lie past the deadline: the read does not wait for it then. The sample time keeps the spacing
        // (see `live_stamp`).
        let deadline = self.last_new_seen.saturating_add(period * 3 / 2);
        let decimate = now < deadline;
        if now >= next_ok || !decimate {
            match self.try_read(&v, now, decimate, &mut *sink) {
                ReadResult::New(capture_qpc) => {
                    return Poll::Ready(self.emit_live(capture_qpc, now));
                }
                ReadResult::Invalid => {
                    self.not_running = true;
                    return self.slate_poll(SlateKind::NotRunning, now, sink);
                }
                // nothing can be copied into: a repeat or a slate would find no buffer either
                ReadResult::NoBuffer => return Poll::Wait(self.no_buffer_wait(now)),
                ReadResult::Nothing => {}
            }
        }
        if self.have_frame {
            // live but late: the last frame again
            if now >= self.last_new_seen + period * 3 / 2 && now >= self.last_emit + period {
                match self.try_repeat(&v, now, &mut *sink) {
                    RepeatResult::Copied => {
                        self.drop_slate_cache();
                        return Poll::Ready(Emit {
                            kind: Kind::Repeat,
                            capture_qpc: 0,
                            stamp_qpc: 0,
                            at: now,
                        });
                    }
                    // a newer frame in the slot of the last one, which the live path could not take: it is one
                    RepeatResult::Newer(capture_qpc) => {
                        return Poll::Ready(self.emit_live(capture_qpc, now));
                    }
                    RepeatResult::Retry => {}
                    RepeatResult::NoBuffer => return Poll::Wait(self.no_buffer_wait(now)),
                    RepeatResult::Invalid => {
                        self.not_running = true;
                        return self.slate_poll(SlateKind::NotRunning, now, sink);
                    }
                    // the picture is gone from the file: what a stream that never had a frame shows
                    RepeatResult::Lost => self.have_frame = false,
                }
            }
        }
        if !self.have_frame && now >= self.live_since + period * 3 / 2 {
            // live, but nothing usable yet (no frame, or frames of another size)
            return self.slate_poll(SlateKind::Starting, now, sink);
        }
        // Nothing to do before `next_ok` (no frame can qualify earlier), nor, with a frame on show, before the
        // repeat deadline: never sleep past it.
        let wake = if self.have_frame {
            next_ok.min(deadline)
        } else {
            next_ok
        };
        Poll::Wait(if now < wake {
            Wait::Until(wake)
        } else {
            self.frame_wait(now, deadline)
        })
    }

    /// What to wait for when a frame could be taken now but there is none: the writer's next frame or state change,
    /// or the next time-based decision `t`, whichever comes first. Where the old code said "a millisecond":
    ///
    /// * frames on show, before the repeat deadline: the deadline (also the take of a frame past it);
    /// * frames on show, past the deadline, before the repeat may go out (`last_emit + period`): that time;
    /// * no frame yet: the "starting" slate's point, `live_since + 1.5 periods`.
    ///
    /// A torn repeat is the one case with no time to wait for: the same slot is tried again in a millisecond.
    fn frame_wait(&self, now: u64, deadline: u64) -> Wait {
        let t = if self.have_frame {
            if now < deadline {
                deadline
            } else if now < self.last_emit.saturating_add(self.period) {
                self.last_emit.saturating_add(self.period)
            } else {
                return Wait::Soon;
            }
        } else {
            self.live_since.saturating_add(self.period * 3 / 2)
        };
        if t > now {
            Wait::Frame(t)
        } else {
            Wait::Soon
        }
    }

    /// A live frame (in the sink) captured at `capture_qpc` is on its way.
    fn emit_live(&mut self, capture_qpc: u64, now: u64) -> Emit {
        let stamp_qpc = self.live_stamp(capture_qpc, now);
        self.have_frame = true;
        self.last_capture = Some(capture_qpc);
        self.last_stamp = Some(stamp_qpc);
        self.last_new_seen = now;
        self.drop_slate_cache();
        self.note_shown(Shown::Live);
        Emit {
            kind: Kind::Live,
            capture_qpc,
            stamp_qpc,
            at: now,
        }
    }

    /// The pump posted the sample for `emit` at QPC `after`: count it and publish consumer state.
    pub fn finish_emit(&mut self, emit: Emit, after: u64) {
        self.last_emit = emit.at;
        match emit.kind {
            Kind::Live => {
                self.delivered += 1;
                let us = (after.saturating_sub(emit.capture_qpc) as u128 * 1_000_000
                    / self.freq as u128) as u32;
                if self.lat.len() < LAT_RING {
                    self.lat.push(us);
                } else {
                    self.lat[self.lat_next] = us;
                }
                self.lat_next = (self.lat_next + 1) % LAT_RING;
                self.live_since_stats += 1;
            }
            Kind::Repeat => {
                self.delivered += 1;
                self.repeated += 1;
                self.live_since_stats += 1;
            }
            Kind::Slate(_) => {}
        }
        // only into a file whose header validated at the last poll
        if let (true, Some(m)) = (self.header_ok, &self.map) {
            let s = m.shared();
            s.set_consumer_heartbeat(after);
            s.set_consumer_active(true);
            s.set_negotiated(&Format::new(
                self.fmt.width,
                self.fmt.height,
                self.fmt.fps,
                self.fmt.pixel.protocol_subtype(),
            ));
        }
        if self.live_since_stats >= 8 {
            self.live_since_stats = 0;
            self.publish_stats();
        }
    }

    fn publish_stats(&self) {
        if !self.header_ok {
            return;
        }
        let Some(m) = &self.map else { return };
        let mut v = self.lat.clone();
        v.sort_unstable();
        let pick = |q: usize| {
            v.get((v.len().saturating_sub(1) * q) / 100)
                .copied()
                .unwrap_or(0)
        };
        m.shared()
            .set_stats(pick(50), pick(95), self.delivered, self.repeated);
    }
}

impl Drop for FrameSource {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
pub(crate) mod test_owner {
    //! The user the frame events must belong to, per frame file, for the tests that need another answer than the real
    //! DACL of a file in `%TEMP%` gives.
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    static OWNERS: Mutex<Vec<(PathBuf, Option<String>)>> = Mutex::new(Vec::new());

    /// Make `expected_owner` answer `owner` for `path` (`None`: no single user).
    pub fn set(path: &Path, owner: Option<String>) {
        let mut g = OWNERS.lock().unwrap_or_else(|e| e.into_inner());
        g.retain(|e| e.0 != path);
        g.push((path.to_path_buf(), owner));
    }

    pub fn get(path: &Path) -> Option<Option<String>> {
        let g = OWNERS.lock().unwrap_or_else(|e| e.into_inner());
        g.iter().find(|e| e.0 == path).map(|e| e.1.clone())
    }
}

impl WakeSource for FrameSource {
    fn report(&self) -> WakeReport {
        self.report
    }

    fn counters(&self) -> Option<(u64, u64)> {
        self.map.as_ref().map(|m| m.shared().signal_counters())
    }

    fn expected_owner(&mut self) -> Option<String> {
        #[cfg(test)]
        if let Some(o) = test_owner::get(&self.path) {
            return o;
        }
        let map = self.map.as_ref()?;
        let id = match map.identity() {
            Ok(id) => id,
            Err(e) => {
                crate::log::log_sampled(crate::log::slot::WAKE_OWNER, 20, || {
                    format!("frames: cannot identify the mapped frame file: {e}")
                });
                return None;
            }
        };
        match vcam_protocol::signal::owner::frames_expected_owner(&self.path, &trusted_root(), id) {
            Ok(Some(owner)) => Some(owner),
            Ok(None) => {
                crate::log::log_sampled(crate::log::slot::WAKE_OWNER, 20, || {
                    "frames: the DACL of the frame file names no single user".into()
                });
                None
            }
            Err(e) => {
                crate::log::log_sampled(crate::log::slot::WAKE_OWNER, 20, || {
                    format!("frames: cannot read the DACL of the frame file: {e}")
                });
                None
            }
        }
    }

    fn publish_diag(&mut self, d: &Diag) {
        self.wake_diag = *d;
        self.write_wake_diag();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};
    use vcam_protocol::pattern;
    use vcam_protocol::testdir::TestFile;
    use vcam_protocol::{qpc_now, SlotFormat, Writer, SUBTYPE_NV12, SUBTYPE_RGB32};

    fn temp_path(name: &str) -> TestFile {
        TestFile::new("bettercam-frames", name)
    }

    fn nv12(w: u32, h: u32, fps: u32) -> FrameFormat {
        FrameFormat {
            pixel: PixelFormat::Nv12,
            width: w,
            height: h,
            fps,
        }
    }

    /// A frame buffer owned by the test, `pitch` bytes per row. It keeps whatever was last written, like a sample
    /// that is reused: what a test checks is what a ready emission leaves in it.
    struct VecSink {
        buf: Vec<u8>,
        pitch: usize,
        /// `buffer()` says "none" this many more times (a pool that has nothing right now).
        refuse: usize,
    }

    impl FrameSink for VecSink {
        fn buffer(&mut self) -> Option<FrameBuf<'_>> {
            if self.refuse > 0 {
                self.refuse -= 1;
                return None;
            }
            Some(FrameBuf {
                data: &mut self.buf,
                pitch: self.pitch,
            })
        }
    }

    /// A [`FrameSource`] with its sink: what the pump is, minus the samples.
    struct Rig {
        src: FrameSource,
        sink: VecSink,
        fmt: FrameFormat,
    }

    impl std::ops::Deref for Rig {
        type Target = FrameSource;
        fn deref(&self) -> &FrameSource {
            &self.src
        }
    }

    impl std::ops::DerefMut for Rig {
        fn deref_mut(&mut self) -> &mut FrameSource {
            &mut self.src
        }
    }

    impl Rig {
        fn new(path: PathBuf, fmt: FrameFormat) -> Rig {
            let pitch = fmt.pixel.stride(fmt.width) as usize;
            Rig::with_pitch(path.to_path_buf(), fmt, pitch)
        }

        fn with_pitch(path: PathBuf, fmt: FrameFormat, pitch: usize) -> Rig {
            let rows = fmt.pixel.rows(fmt.height) as usize;
            Rig {
                src: FrameSource::new(path.to_path_buf(), fmt),
                // a poisoned start: nothing may rely on the buffer being clean
                sink: VecSink {
                    buf: vec![0xA5; pitch * rows],
                    pitch,
                    refuse: 0,
                },
                fmt,
            }
        }

        fn poll(&mut self, now: u64) -> Poll {
            self.src.poll(now, &mut self.sink)
        }

        /// The frame of the last ready emission (a rig with a tight pitch).
        fn data(&self) -> &[u8] {
            assert_eq!(
                self.sink.pitch,
                self.fmt.pixel.stride(self.fmt.width) as usize
            );
            &self.sink.buf
        }

        /// The frame of the last ready emission with the padding of each row cut out.
        fn tight(&self) -> Vec<u8> {
            let rb = self.fmt.pixel.stride(self.fmt.width) as usize;
            let rows = self.fmt.pixel.rows(self.fmt.height) as usize;
            (0..rows)
                .flat_map(|r| self.sink.buf[r * self.sink.pitch..r * self.sink.pitch + rb].to_vec())
                .collect()
        }
    }

    /// Poll until something is ready (at most `limit`), sleeping 1 ms between polls like the pump does.
    fn next_ready(src: &mut Rig, limit: Duration) -> Emit {
        let began = Instant::now();
        loop {
            match src.poll(qpc_now()) {
                Poll::Ready(e) => {
                    src.finish_emit(e, qpc_now());
                    return e;
                }
                Poll::Wait(_) => {
                    assert!(began.elapsed() < limit, "nothing ready within {limit:?}");
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }
    }

    fn publish_pattern(w: &Writer, width: u32, height: u32, n: u64) {
        let fmt = SlotFormat {
            width,
            height,
            stride: width,
            subtype: SUBTYPE_NV12,
        };
        w.publish(fmt, qpc_now(), |d| {
            pattern::fill_nv12(n, width, height, d, width as usize)
        })
        .unwrap();
    }

    fn slate_diff(src: &Rig, kind: SlateKind, w: u32, h: u32) -> f64 {
        let want = decoded_slate(kind).scale_nv12(w, h);
        let got = src.data();
        vcam_protocol::slate::mean_luma_diff(
            |y| want[y as usize * w as usize..(y as usize + 1) * w as usize].to_vec(),
            |y| got[y as usize * w as usize..(y as usize + 1) * w as usize].to_vec(),
            w,
            h,
            false,
            1,
        )
    }

    #[test]
    fn embedded_slates_decode_and_differ() {
        let ks = SlateKind::ALL;
        for k in ks {
            let s = decoded_slate(k);
            assert_eq!((s.width, s.height), (1920, 1080), "{k:?}");
            // dark, with some bright text/mark pixels
            let y = &s.nv12[..1920 * 1080];
            assert!(y.iter().any(|&v| v > 150), "{k:?} has no text");
            let dark = y.iter().filter(|&&v| v < 80).count();
            assert!(dark > y.len() * 8 / 10, "{k:?} is not mostly dark");
        }
        assert_ne!(decoded_slate(ks[0]).nv12, decoded_slate(ks[1]).nv12);
        assert_ne!(decoded_slate(ks[1]).nv12, decoded_slate(ks[2]).nv12);
    }

    #[test]
    fn advertised_comes_from_the_header_or_defaults() {
        let path = temp_path("adv.bin");
        let _ = std::fs::remove_file(&path);
        set_test_path(Some(path.to_path_buf()));
        assert_eq!(advertised(), vcam_protocol::default_advertised());
        let w = Writer::create(&path).unwrap();
        w.set_advertised(&vcam_protocol::advertised_for_output(1920, 1080, 60));
        let a = advertised();
        assert_eq!(a.len(), 8);
        assert_eq!((a[0].width, a[0].fps()), (1920, 60));
        // corrupt the magic: back to defaults
        w.shared().poke32(vcam_protocol::off::MAGIC, 0);
        assert_eq!(advertised(), vcam_protocol::default_advertised());
        set_test_path(None);
    }

    #[test]
    fn missing_file_shows_the_not_running_slate() {
        let path = temp_path("missing.bin");
        let _ = std::fs::remove_file(&path);
        let mut src = Rig::new(path.to_path_buf(), nv12(640, 360, 30));
        src.start();
        let e = next_ready(&mut src, Duration::from_secs(1));
        assert_eq!(e.kind, Kind::Slate(SlateKind::NotRunning));
        assert_eq!(src.data().len(), 640 * 360 * 3 / 2);
        assert_eq!(slate_diff(&src, SlateKind::NotRunning, 640, 360), 0.0);
        // slates come at the frame rate, not faster
        let t0 = Instant::now();
        for _ in 0..10 {
            next_ready(&mut src, Duration::from_secs(1));
        }
        let el = t0.elapsed().as_secs_f64();
        assert!((0.25..0.45).contains(&el), "10 slates in {el}s");
    }

    #[test]
    fn writer_states_select_the_slate() {
        let path = temp_path("states.bin");
        let _ = std::fs::remove_file(&path);
        let w = Writer::create(&path).unwrap();
        let mut src = Rig::new(path.to_path_buf(), nv12(320, 180, 30));
        src.start();
        for (state, kind) in [
            (WriterState::Closed, SlateKind::NotRunning),
            (WriterState::Paused, SlateKind::Paused),
            (WriterState::Starting, SlateKind::Starting),
        ] {
            w.set_state(state);
            let e = next_ready(&mut src, Duration::from_secs(2));
            assert_eq!(e.kind, Kind::Slate(kind), "{state:?}");
            assert_eq!(slate_diff(&src, kind, 320, 180), 0.0);
        }
    }

    #[test]
    fn live_frames_flow_and_late_frames_repeat() {
        let path = temp_path("live.bin");
        let _ = std::fs::remove_file(&path);
        let w = Writer::create(&path).unwrap();
        w.set_state(WriterState::Live);
        let (fw, fh) = (640u32, 360u32);
        let mut src = Rig::new(path.to_path_buf(), nv12(fw, fh, 30));
        src.start();
        // live but no frame yet: after 1.5 frame durations the "starting" slate
        let e = next_ready(&mut src, Duration::from_secs(1));
        assert_eq!(e.kind, Kind::Slate(SlateKind::Starting));
        publish_pattern(&w, fw, fh, 5);
        let live = next_ready(&mut src, Duration::from_secs(1));
        assert_eq!(live.kind, Kind::Live);
        let a = pattern::analyse(
            |r| &src.data()[r as usize * fw as usize..(r as usize + 1) * fw as usize],
            SUBTYPE_NV12,
            fw,
            fh,
        );
        assert_eq!(a.slot(), Some(5 % pattern::slots(fw)));
        assert!(a.board_ok);
        // no new frame: repeat the last one after about 1.5 frame durations, counted. Measured between the two
        // decisions in QPC: the wall clock after `next_ready` also holds the time the test thread was not running
        let e = next_ready(&mut src, Duration::from_secs(1));
        assert_eq!(e.kind, Kind::Repeat);
        let period = vcam_protocol::qpc_frequency() / 30;
        assert!(
            e.at - live.at >= period * 3 / 2,
            "repeated after only {} of {} ticks",
            e.at - live.at,
            period * 3 / 2
        );
        assert_eq!(src.repeated(), 1);
        let a = pattern::analyse(
            |r| &src.data()[r as usize * fw as usize..(r as usize + 1) * fw as usize],
            SUBTYPE_NV12,
            fw,
            fh,
        );
        assert_eq!(
            a.slot(),
            Some(5 % pattern::slots(fw)),
            "a repeat shows the last frame"
        );
        // a frame of another size is never scaled or shown: still repeating
        publish_pattern(&w, 320, 180, 6);
        let e = next_ready(&mut src, Duration::from_secs(1));
        assert_eq!(e.kind, Kind::Repeat);
        // the right size again
        publish_pattern(&w, fw, fh, 7);
        let e = next_ready(&mut src, Duration::from_secs(1));
        assert_eq!(e.kind, Kind::Live);
        assert_eq!(src.delivered(), 4);
    }

    #[test]
    fn a_stale_heartbeat_shows_the_slate_and_closed_is_immediate() {
        let path = temp_path("stale.bin");
        let _ = std::fs::remove_file(&path);
        let w = Writer::create(&path).unwrap();
        w.set_state(WriterState::Live);
        let mut src = Rig::new(path.to_path_buf(), nv12(320, 180, 30));
        src.start();
        publish_pattern(&w, 320, 180, 1);
        assert_eq!(
            next_ready(&mut src, Duration::from_secs(1)).kind,
            Kind::Live
        );
        // the writer dies without closing: heartbeat stops
        let t0 = Instant::now();
        loop {
            let e = next_ready(&mut src, Duration::from_secs(3));
            if e.kind == Kind::Slate(SlateKind::NotRunning) {
                break;
            }
            assert!(matches!(e.kind, Kind::Repeat | Kind::Live), "{:?}", e.kind);
        }
        let took = t0.elapsed();
        assert!(
            (Duration::from_millis(900)..Duration::from_millis(1400)).contains(&took),
            "slate after {took:?}"
        );
        // back to life: live frames again, no stale repeat of the old frame
        w.set_state(WriterState::Live);
        publish_pattern(&w, 320, 180, 9);
        let e = next_ready(&mut src, Duration::from_secs(1));
        assert_eq!(e.kind, Kind::Live);
    }

    #[test]
    fn corrupted_files_give_the_slate_not_a_crash() {
        let path = temp_path("corrupt.bin");
        let _ = std::fs::remove_file(&path);
        let w = Writer::create(&path).unwrap();
        w.set_state(WriterState::Live);
        let mut src = Rig::new(path.to_path_buf(), nv12(320, 180, 30));
        src.start();
        let sh = w.shared();
        publish_pattern(&w, 320, 180, 1);
        assert_eq!(
            next_ready(&mut src, Duration::from_secs(1)).kind,
            Kind::Live
        );

        let expect_slate = |src: &mut Rig| {
            for _ in 0..40 {
                let e = next_ready(src, Duration::from_secs(2));
                if e.kind == Kind::Slate(SlateKind::NotRunning) {
                    return;
                }
                w.heartbeat();
            }
            panic!("never reached the slate");
        };
        let restore = |field: usize, value: u32| sh.poke32(field, value);

        // bad magic
        sh.poke32(vcam_protocol::off::MAGIC, 0xDEAD);
        expect_slate(&mut src);
        restore(vcam_protocol::off::MAGIC, vcam_protocol::MAGIC);
        // file_size past the real end of the file
        sh.poke64(vcam_protocol::off::FILE_SIZE, u64::MAX / 2);
        expect_slate(&mut src);
        sh.poke64(vcam_protocol::off::FILE_SIZE, vcam_protocol::FILE_SIZE);
        // slot descriptors: offset past the end, odd and oversized dimensions
        for (field, value) in [
            (
                vcam_protocol::off::SLOT_DATA_OFFSET,
                vcam_protocol::FILE_SIZE - 16,
            ),
            (vcam_protocol::off::SLOT_DATA_OFFSET, u64::MAX - 4),
            (vcam_protocol::off::SLOT_WIDTH, 321),
            (vcam_protocol::off::SLOT_WIDTH, 100_000),
            (vcam_protocol::off::SLOT_HEIGHT, 0),
        ] {
            // a fresh frame whose descriptor is then damaged
            publish_pattern(&w, 320, 180, 3);
            let slot = sh.header().latest_slot as usize;
            let so = vcam_protocol::slot_off(slot);
            if field == vcam_protocol::off::SLOT_DATA_OFFSET {
                sh.poke64(so + field, value);
            } else {
                sh.poke32(so + field, value as u32);
            }
            expect_slate(&mut src);
        }
        // everything restored and republished: live again
        publish_pattern(&w, 320, 180, 4);
        publish_pattern(&w, 320, 180, 5);
        let mut live = false;
        for _ in 0..10 {
            if next_ready(&mut src, Duration::from_secs(2)).kind == Kind::Live {
                live = true;
                break;
            }
        }
        assert!(live, "no recovery after the file became valid again");
    }

    /// Run a writer at `writer_fps` (capture stamps are its exact due times) against a source negotiated at
    /// `consumer_fps`; returns the stripe slots of `count` live frames and the time they took.
    fn run_rates(writer_fps: u32, consumer_fps: u32, count: usize) -> (Vec<u32>, Duration, u64) {
        let path = temp_path(&format!("rates-{writer_fps}-{consumer_fps}.bin"));
        let _ = std::fs::remove_file(&path);
        let w = std::sync::Arc::new(Writer::create(&path).unwrap());
        w.set_state(WriterState::Live);
        let (fw, fh) = (320u32, 180u32);
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let thread = {
            let (w, stop) = (w.clone(), stop.clone());
            std::thread::spawn(move || {
                let freq = qpc_frequency();
                let start = qpc_now();
                let mut n = 0u64;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let due = start + (n as u128 * freq as u128 / writer_fps as u128) as u64;
                    while qpc_now() < due {
                        std::hint::spin_loop();
                    }
                    let fmt = SlotFormat {
                        width: fw,
                        height: fh,
                        stride: fw,
                        subtype: SUBTYPE_NV12,
                    };
                    w.publish(fmt, due, |d| pattern::fill_nv12(n, fw, fh, d, fw as usize))
                        .unwrap();
                    n += 1;
                }
            })
        };
        let mut src = Rig::new(path.to_path_buf(), nv12(fw, fh, consumer_fps));
        src.start();
        let mut slots = Vec::new();
        let mut began = None;
        while slots.len() < count {
            let e = next_ready(&mut src, Duration::from_secs(3));
            if e.kind != Kind::Live {
                continue;
            }
            began.get_or_insert_with(Instant::now);
            let a = pattern::analyse(
                |r| &src.data()[r as usize * fw as usize..(r as usize + 1) * fw as usize],
                SUBTYPE_NV12,
                fw,
                fh,
            );
            slots.push(a.slot().expect("live frame has a stripe"));
        }
        let took = began.unwrap().elapsed();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        thread.join().unwrap();
        (slots, took, src.repeated())
    }

    #[test]
    fn a_faster_writer_is_decimated_on_capture_time_and_a_matching_one_is_not() {
        let n = pattern::slots(320);
        let steps =
            |s: &[u32]| -> Vec<u32> { s.windows(2).map(|p| (p[1] + n - p[0]) % n).collect() };
        // 60 fps writer, 30 fps consumer: every other frame, 30 fps out
        let (slots, took, repeated) = run_rates(60, 30, 61);
        let st = steps(&slots);
        assert!(st.iter().all(|&d| d == 2), "steps {st:?}");
        let fps = 60.0 / took.as_secs_f64();
        assert!((28.5..31.5).contains(&fps), "{fps} fps out");
        assert_eq!(repeated, 0);
        // 60/60 and 30/30: every frame
        for fps in [60u32, 30] {
            let (slots, _, repeated) = run_rates(fps, fps, 61);
            let st = steps(&slots);
            assert!(st.iter().all(|&d| d == 1), "{fps} fps: steps {st:?}");
            assert_eq!(repeated, 0, "{fps} fps");
        }
    }
    #[test]
    fn rgb32_and_padded_strides_are_copied_tight() {
        let path = temp_path("rgb.bin");
        let _ = std::fs::remove_file(&path);
        let w = Writer::create(&path).unwrap();
        w.set_state(WriterState::Live);
        let (fw, fh) = (320u32, 180u32);
        let mut src = Rig::new(path.to_path_buf(),
            FrameFormat {
                pixel: PixelFormat::Rgb32,
                width: fw,
                height: fh,
                fps: 30,
            },
        );
        src.start();
        let stride = fw * 4 + 64;
        let fmt = SlotFormat {
            width: fw,
            height: fh,
            stride,
            subtype: SUBTYPE_RGB32,
        };
        w.publish(fmt, qpc_now(), |d| {
            let mut tight = vec![0u8; fw as usize * 4 * fh as usize];
            pattern::fill_bgrx(11, fw, fh, &mut tight, fw as usize * 4);
            for r in 0..fh as usize {
                d[r * stride as usize..r * stride as usize + fw as usize * 4]
                    .copy_from_slice(&tight[r * fw as usize * 4..(r + 1) * fw as usize * 4]);
            }
        })
        .unwrap();
        assert_eq!(
            next_ready(&mut src, Duration::from_secs(1)).kind,
            Kind::Live
        );
        let a = pattern::analyse(
            |r| &src.data()[r as usize * fw as usize * 4..(r as usize + 1) * fw as usize * 4],
            SUBTYPE_RGB32,
            fw,
            fh,
        );
        assert_eq!(a.slot(), Some(11 % pattern::slots(fw)));
        assert!(a.board_ok);
        // the RGB32 slate is the converted NV12 slate
        w.set_state(WriterState::Paused);
        let e = next_ready(&mut src, Duration::from_secs(1));
        assert_eq!(e.kind, Kind::Slate(SlateKind::Paused));
        assert_eq!(
            src.data(),
            decoded_slate(SlateKind::Paused)
                .scale_bgrx(fw, fh)
                .as_slice()
        );
    }

    /// Run a source against `path` for a short time the way the pump does: start, poll for about `ms`, stop.
    fn run_source_for(path: &std::path::Path, ms: u64) -> Vec<Kind> {
        run_source_fed(path, ms, || {})
    }

    /// [`run_source_for`] with a writer that feeds the file: `feed` runs before every poll round (a frame captured
    /// before the source attached is never shown, so a frame published before the run does not count).
    fn run_source_fed(path: &std::path::Path, ms: u64, mut feed: impl FnMut()) -> Vec<Kind> {
        let mut src = Rig::new(path.to_path_buf(), nv12(320, 180, 30));
        src.start();
        let began = Instant::now();
        let mut kinds = Vec::new();
        while began.elapsed() < Duration::from_millis(ms) {
            feed();
            kinds.push(next_ready(&mut src, Duration::from_secs(2)).kind);
        }
        src.stop();
        kinds
    }

    #[test]
    fn foreign_files_and_other_protocol_versions_are_never_written_to() {
        use vcam_protocol::off;
        // (name, how to spoil a valid file)
        type Spoil = fn(&Writer);
        let cases: [(&str, Spoil); 3] = [
            ("wrong magic", |w| {
                w.shared().poke32(off::MAGIC, 0x1234_5678)
            }),
            // an old DLL still loaded in the service meets a file of a newer layout
            ("newer protocol", |w| {
                w.shared()
                    .poke32(off::PROTOCOL, vcam_protocol::PROTOCOL_VERSION + 1)
            }),
            ("bad header size", |w| {
                w.shared().poke32(off::HEADER_SIZE, 64)
            }),
        ];
        for (name, spoil) in cases {
            let path = temp_path(&format!("foreign-{}.bin", name.replace(' ', "-")));
            let _ = std::fs::remove_file(&path);
            let w = Writer::create(&path).unwrap();
            w.set_state(WriterState::Live);
            publish_pattern(&w, 320, 180, 1);
            spoil(&w);
            drop(w);
            let before = std::fs::read(&path).unwrap();
            let kinds = run_source_for(&path, 150);
            assert!(
                kinds
                    .iter()
                    .all(|k| *k == Kind::Slate(SlateKind::NotRunning)),
                "{name}: {kinds:?}"
            );
            assert!(
                std::fs::read(&path).unwrap() == before,
                "{name}: the DLL altered the file"
            );
        }
        // a file that was never ours: 8 KB of one byte value, and a small one that only just passes the size check
        for (name, size) in [("foreign-8k.bin", 8192usize), ("foreign-4k.bin", 4096)] {
            let path = temp_path(name);
            std::fs::write(&path, vec![0x5Au8; size]).unwrap();
            let kinds = run_source_for(&path, 150);
            assert!(kinds
                .iter()
                .all(|k| *k == Kind::Slate(SlateKind::NotRunning)));
            assert!(
                std::fs::read(&path).unwrap() == vec![0x5Au8; size],
                "{name}: the DLL altered the file"
            );
        }
    }

    #[test]
    fn a_valid_file_still_gets_consumer_state_after_it_became_valid() {
        // the header gate must not starve a good file: invalid first, then valid, then the DLL writes
        let path = temp_path("late-valid.bin");
        let _ = std::fs::remove_file(&path);
        let w = Writer::create(&path).unwrap();
        w.shared().poke32(vcam_protocol::off::MAGIC, 0);
        let mut src = Rig::new(path.to_path_buf(), nv12(320, 180, 30));
        src.start();
        let e = next_ready(&mut src, Duration::from_secs(1));
        assert_eq!(e.kind, Kind::Slate(SlateKind::NotRunning));
        assert_eq!(w.shared().header().consumer_active, 0);
        w.shared()
            .poke32(vcam_protocol::off::MAGIC, vcam_protocol::MAGIC);
        w.set_state(WriterState::Live);
        publish_pattern(&w, 320, 180, 2);
        for _ in 0..20 {
            if next_ready(&mut src, Duration::from_secs(1)).kind == Kind::Live {
                break;
            }
        }
        assert_eq!(w.shared().header().consumer_active, 1);
        assert_eq!(
            w.shared().header().negotiated,
            Format::new(320, 180, 30, SUBTYPE_NV12)
        );
    }

    fn junction(link: &std::path::Path, target: &std::path::Path) {
        let out = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .unwrap();
        assert!(out.status.success(), "mklink /J failed");
    }

    #[test]
    fn a_frame_file_behind_a_junction_or_hard_link_is_not_used_and_not_touched() {
        let base = temp_path("links");
        let _ = std::fs::remove_dir_all(&base);
        let real = base.join("real");
        std::fs::create_dir_all(&real).unwrap();
        // a live writer with frames: if the DLL followed a link it would show them
        let w = Writer::create(&real.join("frames.bin")).unwrap();
        w.set_state(WriterState::Live);
        w.set_advertised(&vcam_protocol::advertised_for_output(1280, 720, 30));
        publish_pattern(&w, 320, 180, 3);
        let direct = run_source_fed(&real.join("frames.bin"), 100, || {
            publish_pattern(&w, 320, 180, 3)
        });
        assert!(
            direct.contains(&Kind::Live),
            "control run without a link: {direct:?}"
        );
        let clean = w.shared().header();

        // 1. a junction in the path
        let link = base.join("link");
        junction(&link, &real);
        for _ in 0..3 {
            w.heartbeat();
            let kinds = run_source_for(&link.join("frames.bin"), 100);
            assert!(
                kinds
                    .iter()
                    .all(|k| *k == Kind::Slate(SlateKind::NotRunning)),
                "through the junction: {kinds:?}"
            );
        }
        set_test_path(Some(link.join("frames.bin")));
        assert_eq!(advertised(), vcam_protocol::default_advertised());
        set_test_path(None);

        // 2. a hard link to the file (the file then has two names: both are refused)
        let alias = base.join("alias.bin");
        std::fs::hard_link(real.join("frames.bin"), &alias).unwrap();
        w.heartbeat();
        let kinds = run_source_for(&alias, 100);
        assert!(
            kinds
                .iter()
                .all(|k| *k == Kind::Slate(SlateKind::NotRunning)),
            "through the hard link: {kinds:?}"
        );
        // nothing of the refused runs reached the file: the header still shows only the control run's state
        let after = w.shared().header();
        assert_eq!(after.consumer_active, clean.consumer_active);
        assert_eq!(after.negotiated, clean.negotiated);
        assert_eq!(after.consumer_heartbeat, clean.consumer_heartbeat);
        assert_eq!(after.delivered, clean.delivered);
        let _ = std::fs::remove_dir(&link);
    }

    /// A standard user may read `frames.bin`, and with that right they can hold an oplock on it that blocks every
    /// other open. The DLL's three opens (the media types at activation, the stream's mapping, and its retries)
    /// must not wait for them: the activation thread of the camera service would hang, and the pumps that wait in
    /// an open pile up. They return at once, as "not usable now", which shows the "not running" slate, and the
    /// camera works again as soon as the lock is gone.
    #[test]
    fn a_frame_file_with_somebody_elses_oplock_gives_the_slate_at_once() {
        use vcam_protocol::secure::oplock::Oplock;
        let path = temp_path("oplock.bin");
        let _ = std::fs::remove_file(&path);
        {
            let w = Writer::create(&path).unwrap();
            w.set_state(WriterState::Live);
            w.set_advertised(&vcam_protocol::advertised_for_output(1280, 720, 30));
            publish_pattern(&w, 320, 180, 1);
        }
        let Some(lock) = Oplock::take(&path).unwrap() else {
            eprintln!("this file system grants no oplock: skipped");
            return;
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let p = path.to_path_buf();
        std::thread::spawn(move || {
            let t = Instant::now();
            set_test_path(Some(p.clone()));
            let adv = advertised();
            let advertised_took = t.elapsed();
            let mut src = Rig::new(p, nv12(320, 180, 30));
            src.start();
            let t = Instant::now();
            let mut kinds = Vec::new();
            while t.elapsed() < Duration::from_millis(400) {
                kinds.push(next_ready(&mut src, Duration::from_secs(2)).kind);
            }
            src.stop();
            set_test_path(None);
            let _ = tx.send((adv, advertised_took, kinds, t.elapsed()));
        });
        let (adv, advertised_took, kinds, took) = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("opening the frame file waited for the oplock holder");
        assert!(
            advertised_took < Duration::from_secs(1),
            "{advertised_took:?}"
        );
        assert_eq!(adv, vcam_protocol::default_advertised());
        assert!(took < Duration::from_secs(2), "{took:?}");
        assert!(
            !kinds.is_empty()
                && kinds
                    .iter()
                    .all(|k| *k == Kind::Slate(SlateKind::NotRunning)),
            "{kinds:?}"
        );
        drop(lock);
        // the lock is gone: the same file works
        let w = Writer::create(&path).unwrap();
        w.set_state(WriterState::Live);
        let kinds = run_source_fed(&path, 150, || publish_pattern(&w, 320, 180, 2));
        assert!(kinds.contains(&Kind::Live), "{kinds:?}");
    }

    #[test]
    fn hostile_capture_times_are_clamped_and_never_break_the_stream() {
        let path = temp_path("captimes.bin");
        let _ = std::fs::remove_file(&path);
        let w = Writer::create(&path).unwrap();
        w.set_state(WriterState::Live);
        let mut src = Rig::new(path.to_path_buf(), nv12(320, 180, 30));
        src.start();
        let freq = qpc_frequency();
        let hostile = [
            u64::MAX,
            u64::MAX - 1,
            i64::MAX as u64,
            u64::MAX / 2,
            0,
            1,
            qpc_now() + 3600 * freq,
            qpc_now().saturating_sub(3600 * freq),
            u64::MAX,
        ];
        let mut prev_ts = 0i64;
        for (i, cap) in hostile.into_iter().enumerate() {
            let before = qpc_now();
            let fmt = SlotFormat {
                width: 320,
                height: 180,
                stride: 320,
                subtype: SUBTYPE_NV12,
            };
            w.publish(fmt, cap, |d| pattern::fill_nv12(i as u64, 320, 180, d, 320))
                .unwrap();
            if cap < before.saturating_sub(freq) {
                // a capture time long before the stream attached is an old frame: never shown, and the stream goes
                // on (the next frame, captured now, is shown)
                let t = Instant::now();
                while t.elapsed() < Duration::from_millis(120) {
                    w.heartbeat();
                    let e = next_ready(&mut src, Duration::from_secs(2));
                    assert_ne!(e.kind, Kind::Live, "capture {cap} is an old frame");
                }
                w.publish(fmt, qpc_now(), |d| pattern::fill_nv12(99, 320, 180, d, 320))
                    .unwrap();
                let mut live = None;
                for _ in 0..200 {
                    let e = next_ready(&mut src, Duration::from_secs(2));
                    if e.kind == Kind::Live {
                        live = Some(e);
                        break;
                    }
                    w.heartbeat();
                }
                let e = live.unwrap_or_else(|| panic!("no live frame after capture time {cap}"));
                prev_ts = crate::stream::next_timestamp(e.sample_qpc(), prev_ts);
                continue;
            }
            let mut live = None;
            for _ in 0..200 {
                let e = next_ready(&mut src, Duration::from_secs(2));
                if e.kind == Kind::Live {
                    live = Some(e);
                    break;
                }
                w.heartbeat();
            }
            let e = live.unwrap_or_else(|| panic!("no live frame for capture time {cap}"));
            let after = qpc_now();
            assert!(
                e.capture_qpc <= after + freq / 20,
                "capture {cap} became {} (now {after})",
                e.capture_qpc
            );
            assert!(
                e.capture_qpc + 5 * freq + freq / 100 >= before,
                "capture {cap} became {} (now {before})",
                e.capture_qpc
            );
            let ts = crate::stream::next_timestamp(e.sample_qpc(), prev_ts);
            assert!(ts > prev_ts, "timestamps must grow");
            assert!(
                ts <= vcam_protocol::qpc_to_hns(after + freq / 20).max(prev_ts + 1),
                "sample time {ts} is in the future for capture {cap}"
            );
            prev_ts = ts;
        }
        assert!(src.delivered() >= 9);
    }

    #[test]
    fn capture_times_are_clamped_for_every_input() {
        let f = 10_000_000u64;
        let now = 100 * f;
        // inside the window: unchanged
        assert_eq!(clamp_capture(now - f, now, f), now - f);
        assert_eq!(clamp_capture(now, now, f), now);
        // far future and far past
        assert_eq!(clamp_capture(u64::MAX, now, f), now + f / 20);
        assert_eq!(clamp_capture(now + 1, now, f), now + 1);
        assert_eq!(clamp_capture(now + 10 * f, now, f), now + f / 20);
        assert_eq!(clamp_capture(0, now, f), now - 5 * f);
        assert_eq!(clamp_capture(1, now, f), now - 5 * f);
        // the clock is younger than the window
        assert_eq!(clamp_capture(0, 2 * f, f), 0);
        assert_eq!(clamp_capture(u64::MAX, 0, f), f / 20);
        // arithmetic edges: no overflow, no panic from `clamp`
        assert_eq!(clamp_capture(u64::MAX, u64::MAX, f), u64::MAX);
        assert_eq!(clamp_capture(0, u64::MAX, f), u64::MAX - 5 * f);
        assert_eq!(clamp_capture(5, 5, 0), 5);
        assert_eq!(clamp_capture(u64::MAX, 10, u64::MAX), 10 + u64::MAX / 20);
        // whatever the inputs, the result lies in the window
        for cap in [0, 1, u64::MAX, u64::MAX / 2, now] {
            for n in [0, 1, now, u64::MAX] {
                for fr in [0, 1, f, u64::MAX] {
                    let lo = n.saturating_sub(fr.saturating_mul(5));
                    let hi = n.saturating_add(fr / 20);
                    let c = clamp_capture(cap, n, fr);
                    assert!((lo..=hi).contains(&c), "clamp({cap}, {n}, {fr}) = {c}");
                }
            }
        }
    }

    #[test]
    fn a_torn_copy_never_reaches_a_delivered_frame() {
        use vcam_protocol::{off, slot_off};
        let path = temp_path("torn.bin");
        let _ = std::fs::remove_file(&path);
        let w = std::rc::Rc::new(Writer::create(&path).unwrap());
        w.set_state(WriterState::Live);
        let (fw, fh) = (320u32, 180u32);
        let mut src = Rig::new(path.to_path_buf(), nv12(fw, fh, 30));
        src.start();
        publish_pattern(&w, fw, fh, 5);
        assert_eq!(
            next_ready(&mut src, Duration::from_secs(1)).kind,
            Kind::Live
        );
        let good = src.data().to_vec();
        let slot_of = |data: &[u8]| {
            pattern::analyse(
                |r| &data[r as usize * fw as usize..(r as usize + 1) * fw as usize],
                SUBTYPE_NV12,
                fw,
                fh,
            )
            .slot()
        };
        assert_eq!(slot_of(&good), Some(5 % pattern::slots(fw)));

        // The next frame is published (a frame period later, or capture-time decimation would skip it), but while
        // the DLL copies it the "writer" scribbles over it and finishes another frame (seq changes): the sequence
        // check rejects the copy, twice, so the read ends torn.
        std::thread::sleep(Duration::from_millis(40));
        publish_pattern(&w, fw, fh, 6);
        let w2 = w.clone();
        COPY_HOOK.with(|h| {
            *h.borrow_mut() = Some(Box::new(move || {
                let sh = w2.shared();
                let slot = sh.header().latest_slot as usize;
                let meta = sh.slot_meta(slot);
                sh.poke_bytes(meta.data_offset as usize, &vec![0xEE; (fw * fh) as usize]);
                sh.poke64(slot_off(slot) + off::SLOT_SEQ, meta.seq + 2);
            }))
        });
        // no new frame can be taken now; after 1.5 frame durations the last frame is repeated. The torn copy went
        // into the same buffer, which the repeat then overwrote whole
        let e = next_ready(&mut src, Duration::from_secs(1));
        assert_eq!(e.kind, Kind::Repeat);
        assert!(
            src.data() == good.as_slice(),
            "the repeat shows bytes of the torn copy"
        );
        let e = next_ready(&mut src, Duration::from_secs(1));
        assert_eq!(e.kind, Kind::Repeat);
        assert!(src.data() == good.as_slice());
        COPY_HOOK.with(|h| *h.borrow_mut() = None);

        // and a good frame afterwards goes through as usual
        std::thread::sleep(Duration::from_millis(40));
        publish_pattern(&w, fw, fh, 7);
        let e = next_ready(&mut src, Duration::from_secs(1));
        assert_eq!(e.kind, Kind::Live);
        assert_eq!(slot_of(src.data()), Some(7 % pattern::slots(fw)));
    }

    #[test]
    fn consumer_state_is_published_and_cleared() {
        let path = temp_path("consumer.bin");
        let _ = std::fs::remove_file(&path);
        let w = Writer::create(&path).unwrap();
        w.set_state(WriterState::Live);
        let mut src = Rig::new(path.to_path_buf(), nv12(320, 180, 30));
        src.start();
        publish_pattern(&w, 320, 180, 1);
        let _ = next_ready(&mut src, Duration::from_secs(1));
        assert!(w.consumer_active());
        assert_eq!(
            w.negotiated(),
            Some(Format::new(320, 180, 30, SUBTYPE_NV12))
        );
        let h = w.shared().header();
        assert_eq!(
            h.delivered, 0,
            "stats are published every 8 frames and at stop"
        );
        src.stop();
        let h = w.shared().header();
        assert_eq!(h.consumer_active, 0);
        assert_eq!(h.delivered, 1);
        assert!(h.latency_p95_us < 1_000_000);
    }

    /// The frame `n` as the pattern writes it, for a frame of `w` x `h`: what a delivered frame must equal.
    fn pattern_frame(n: u64, w: u32, h: u32) -> Vec<u8> {
        let mut v = vec![0u8; (w * h * 3 / 2) as usize];
        pattern::fill_nv12(n, w, h, &mut v, w as usize);
        v
    }

    #[test]
    fn a_frame_lands_whole_in_a_buffer_with_a_padded_pitch_whatever_the_slot_stride() {
        // sink pitch = slot stride (one copy of everything) and sink pitch != slot stride (row by row), NV12 and RGB32
        for (pixel, slot_pad, sink_pad) in [
            (PixelFormat::Nv12, 0usize, 0usize),
            (PixelFormat::Nv12, 64, 64),
            (PixelFormat::Nv12, 64, 32),
            (PixelFormat::Nv12, 0, 96),
            (PixelFormat::Rgb32, 0, 0),
            (PixelFormat::Rgb32, 128, 128),
            (PixelFormat::Rgb32, 64, 192),
        ] {
            let (fw, fh) = (320u32, 180u32);
            let path = temp_path(&format!("pitch-{pixel:?}-{slot_pad}-{sink_pad}.bin"));
            let _ = std::fs::remove_file(&path);
            let w = Writer::create(&path).unwrap();
            w.set_state(WriterState::Live);
            let fmt = FrameFormat {
                pixel,
                width: fw,
                height: fh,
                fps: 30,
            };
            let rb = pixel.stride(fw) as usize;
            let mut src = Rig::with_pitch(path.to_path_buf(), fmt, rb + sink_pad);
            src.start();
            let stride = (rb + slot_pad) as u32;
            let (subtype, rows) = match pixel {
                PixelFormat::Nv12 => (SUBTYPE_NV12, (fh * 3 / 2) as usize),
                PixelFormat::Rgb32 => (SUBTYPE_RGB32, fh as usize),
            };
            // the frame in tight rows, then written with the slot's stride
            let mut tight = vec![0u8; rb * rows];
            match pixel {
                PixelFormat::Nv12 => pattern::fill_nv12(9, fw, fh, &mut tight, rb),
                PixelFormat::Rgb32 => pattern::fill_bgrx(9, fw, fh, &mut tight, rb),
            }
            w.publish(
                SlotFormat {
                    width: fw,
                    height: fh,
                    stride,
                    subtype,
                },
                qpc_now(),
                |d| {
                    for r in 0..rows {
                        d[r * stride as usize..r * stride as usize + rb]
                            .copy_from_slice(&tight[r * rb..(r + 1) * rb]);
                    }
                },
            )
            .unwrap();
            assert_eq!(
                next_ready(&mut src, Duration::from_secs(1)).kind,
                Kind::Live
            );
            assert!(
                src.tight() == tight,
                "{pixel:?} slot padding {slot_pad} sink padding {sink_pad}"
            );
            // the same frame again as a repeat
            assert_eq!(
                next_ready(&mut src, Duration::from_secs(1)).kind,
                Kind::Repeat
            );
            assert!(
                src.tight() == tight,
                "{pixel:?} repeat, slot padding {slot_pad} sink padding {sink_pad}"
            );
        }
    }

    #[test]
    fn slates_fill_a_buffer_at_every_size_and_subtype_whatever_its_pitch() {
        for (pixel, w, h, pad) in [
            (PixelFormat::Nv12, 320u32, 180u32, 0usize),
            (PixelFormat::Nv12, 640, 360, 64),
            (PixelFormat::Nv12, 1280, 720, 0),
            (PixelFormat::Nv12, 1920, 1080, 0),
            (PixelFormat::Nv12, 3840, 2160, 128),
            (PixelFormat::Rgb32, 320, 180, 0),
            (PixelFormat::Rgb32, 1280, 720, 64),
            (PixelFormat::Rgb32, 1920, 1080, 0),
        ] {
            let path = temp_path("slates-nowhere.bin");
            let _ = std::fs::remove_file(&path);
            let fmt = FrameFormat {
                pixel,
                width: w,
                height: h,
                fps: 30,
            };
            let mut src = Rig::with_pitch(path.to_path_buf(), fmt, pixel.stride(w) as usize + pad);
            src.start();
            let e = next_ready(&mut src, Duration::from_secs(2));
            assert_eq!(e.kind, Kind::Slate(SlateKind::NotRunning));
            let want = match pixel {
                PixelFormat::Nv12 => decoded_slate(SlateKind::NotRunning).scale_nv12(w, h),
                PixelFormat::Rgb32 => decoded_slate(SlateKind::NotRunning).scale_bgrx(w, h),
            };
            assert!(
                src.tight() == want,
                "{pixel:?} {w}x{h} pitch +{pad}: not the slate"
            );
        }
    }

    #[test]
    fn the_scaled_slate_is_freed_once_a_live_frame_is_on_its_way() {
        let path = temp_path("slate-free.bin");
        let _ = std::fs::remove_file(&path);
        let w = Writer::create(&path).unwrap();
        w.set_state(WriterState::Live);
        let mut src = Rig::new(path.to_path_buf(), nv12(640, 360, 30));
        src.start();
        let e = next_ready(&mut src, Duration::from_secs(1));
        assert_eq!(e.kind, Kind::Slate(SlateKind::Starting));
        assert_eq!(src.slate_buf.len(), 640 * 360 * 3 / 2);
        publish_pattern(&w, 640, 360, 4);
        assert_eq!(
            next_ready(&mut src, Duration::from_secs(1)).kind,
            Kind::Live
        );
        assert_eq!(src.slate_buf.capacity(), 0, "the slate was kept");
        // and a later slate is made again, right
        w.set_state(WriterState::Paused);
        let e = next_ready(&mut src, Duration::from_secs(1));
        assert_eq!(e.kind, Kind::Slate(SlateKind::Paused));
        assert_eq!(slate_diff(&src, SlateKind::Paused, 640, 360), 0.0);
    }

    #[test]
    fn a_repeat_shows_the_last_good_picture_also_when_the_writer_sent_other_sizes_into_its_slots() {
        let path = temp_path("repeat-lost.bin");
        let _ = std::fs::remove_file(&path);
        let w = Writer::create(&path).unwrap();
        w.set_state(WriterState::Live);
        let (fw, fh) = (640u32, 360u32);
        let mut src = Rig::new(path.to_path_buf(), nv12(fw, fh, 30));
        src.start();
        publish_pattern(&w, fw, fh, 5);
        assert_eq!(
            next_ready(&mut src, Duration::from_secs(1)).kind,
            Kind::Live
        );
        let good = pattern_frame(5, fw, fh);
        assert!(src.data() == good.as_slice());
        // one frame of another size: the slot of the good frame is not the newest, and still holds it
        publish_pattern(&w, 320, 180, 6);
        for _ in 0..3 {
            let e = next_ready(&mut src, Duration::from_secs(1));
            assert_eq!(e.kind, Kind::Repeat);
            assert!(
                src.data() == good.as_slice(),
                "the repeat is not the last frame"
            );
        }
        assert_eq!(src.repeated(), 3);
        // a second one reuses the slot of the good frame: the picture is gone from the file. The source says
        // so with the "starting" slate, never with bytes of the other size
        publish_pattern(&w, 320, 180, 7);
        let e = next_ready(&mut src, Duration::from_secs(1));
        assert_eq!(e.kind, Kind::Slate(SlateKind::Starting));
        assert_eq!(slate_diff(&src, SlateKind::Starting, fw, fh), 0.0);
        assert_eq!(src.repeated(), 3, "a slate is not a repeat");
        // the right size again: live
        publish_pattern(&w, fw, fh, 8);
        let e = next_ready(&mut src, Duration::from_secs(1));
        assert_eq!(e.kind, Kind::Live);
        assert!(src.data() == pattern_frame(8, fw, fh).as_slice());
    }

    #[test]
    fn a_repeat_that_is_torn_delivers_nothing_and_the_next_one_is_whole() {
        use vcam_protocol::{off, slot_off};
        let path = temp_path("repeat-torn.bin");
        let _ = std::fs::remove_file(&path);
        let w = std::rc::Rc::new(Writer::create(&path).unwrap());
        w.set_state(WriterState::Live);
        let (fw, fh) = (320u32, 180u32);
        let mut src = Rig::new(path.to_path_buf(), nv12(fw, fh, 30));
        src.start();
        publish_pattern(&w, fw, fh, 5);
        assert_eq!(
            next_ready(&mut src, Duration::from_secs(1)).kind,
            Kind::Live
        );
        let shown = Stuck::of(&src);
        // the "writer" scribbles over the slot of the last frame while the repeat copies it and is in the middle of
        // writing it (seq odd) from then on: the sequence check rejects the copy, and the retry finds the slot
        // odd. The slot is the only one there is, so the fallback to the newest slot is torn too.
        let copies = std::rc::Rc::new(std::cell::Cell::new(0u32));
        let (w2, c2) = (w.clone(), copies.clone());
        COPY_HOOK.with(|h| {
            *h.borrow_mut() = Some(Box::new(move || {
                c2.set(c2.get() + 1);
                let sh = w2.shared();
                let slot = sh.header().latest_slot as usize;
                let meta = sh.slot_meta(slot);
                sh.poke_bytes(meta.data_offset as usize, &vec![0xEE; (fw * fh) as usize]);
                sh.poke64(slot_off(slot) + off::SLOT_SEQ, meta.seq + 1);
            }))
        });
        let began = Instant::now();
        while began.elapsed() < Duration::from_millis(300) {
            w.heartbeat();
            if let Poll::Ready(e) = src.poll(qpc_now()) {
                panic!("{:?} was delivered from a torn copy", e.kind);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        COPY_HOOK.with(|h| *h.borrow_mut() = None);
        assert_eq!(copies.get(), 1, "the repeat never copied the slot");
        // the writer finishes the frame it was writing: a settled frame (all 0xEE luma) other than the one that
        // was shown. It is a newer frame, not a repeat, and the repeat deadline is long past, so the live path
        // shows it whole, as itself
        shown.settle_as_another_frame(&w);
        let e = next_ready(&mut src, Duration::from_secs(1));
        assert_eq!(e.kind, Kind::Live);
        assert!(src.data()[..(fw * fh) as usize].iter().all(|&b| b == 0xEE));
        // and the next frame the writer sends goes through whole
        std::thread::sleep(Duration::from_millis(40));
        publish_pattern(&w, fw, fh, 6);
        let e = next_ready(&mut src, Duration::from_secs(1));
        assert_eq!(e.kind, Kind::Live);
        assert!(src.data() == pattern_frame(6, fw, fh).as_slice());
    }

    #[test]
    fn a_sink_without_a_buffer_costs_no_frame() {
        let path = temp_path("no-buffer.bin");
        let _ = std::fs::remove_file(&path);
        let w = Writer::create(&path).unwrap();
        w.set_state(WriterState::Live);
        let (fw, fh) = (320u32, 180u32);
        let mut src = Rig::new(path.to_path_buf(), nv12(fw, fh, 30));
        src.start();
        publish_pattern(&w, fw, fh, 3);
        // the pool has nothing for the next few asks: the frame stays in the file, nothing is emitted
        src.sink.refuse = 5;
        for _ in 0..5 {
            assert!(matches!(src.poll(qpc_now()), Poll::Wait(_)));
        }
        assert_eq!(src.sink.refuse, 0);
        let e = next_ready(&mut src, Duration::from_secs(1));
        assert_eq!(e.kind, Kind::Live);
        assert!(src.data() == pattern_frame(3, fw, fh).as_slice());
        assert_eq!(src.delivered(), 1);
        // a repeat that finds no buffer is asked again, and is not counted until it was copied
        src.sink.refuse = 4;
        let e = next_ready(&mut src, Duration::from_secs(1));
        assert_eq!(e.kind, Kind::Repeat);
        assert_eq!(src.sink.refuse, 0, "the four refusals were not met");
        assert_eq!(src.repeated(), 1);
        assert!(src.data() == pattern_frame(3, fw, fh).as_slice());
    }

    #[test]
    fn a_sink_without_a_buffer_is_asked_again_a_quarter_period_later_not_a_millisecond_later() {
        let (fw, fh) = (320u32, 180u32);
        let quarter = vcam_protocol::qpc_frequency() / 30 / 4;
        // a slate that finds no buffer
        let path = temp_path("no-buffer-wait-none.bin");
        let _ = std::fs::remove_file(&path);
        let mut src = Rig::new(path.to_path_buf(), nv12(fw, fh, 30));
        src.start();
        src.sink.refuse = 1;
        let now = qpc_now();
        assert_eq!(
            src.poll(now),
            Poll::Wait(Wait::Until(now + quarter)),
            "slate"
        );
        // a live frame that finds none
        let path = temp_path("no-buffer-wait.bin");
        let _ = std::fs::remove_file(&path);
        let w = Writer::create(&path).unwrap();
        w.set_state(WriterState::Live);
        publish_pattern(&w, fw, fh, 3);
        let mut src = Rig::new(path.to_path_buf(), nv12(fw, fh, 30));
        src.start();
        src.sink.refuse = 1;
        let now = qpc_now();
        assert_eq!(
            src.poll(now),
            Poll::Wait(Wait::Until(now + quarter)),
            "live"
        );
        // and the frame is still there for the next try
        let e = next_ready(&mut src, Duration::from_secs(1));
        assert_eq!(e.kind, Kind::Live);
    }

    /// The reader's slot of the last shown frame, and a way to put the writer's mess in it.
    struct Stuck {
        slot: usize,
        seq: u64,
    }

    impl Stuck {
        fn of(src: &Rig) -> Stuck {
            let (slot, _, seq) = src.last_id.expect("a frame was shown");
            Stuck {
                slot: slot as usize,
                seq,
            }
        }

        /// A writer that is in the middle of a write to the slot (its `seq` is odd).
        fn odd(&self, w: &Writer) {
            use vcam_protocol::{off, slot_off};
            w.shared()
                .poke64(slot_off(self.slot) + off::SLOT_SEQ, self.seq + 1);
        }

        /// The write is over.
        fn even(&self, w: &Writer) {
            use vcam_protocol::{off, slot_off};
            w.shared()
                .poke64(slot_off(self.slot) + off::SLOT_SEQ, self.seq);
        }

        /// The write that was under way ends with a settled frame other than the one that was shown.
        fn settle_as_another_frame(&self, w: &Writer) {
            use vcam_protocol::{off, slot_off};
            w.shared()
                .poke64(slot_off(self.slot) + off::SLOT_SEQ, self.seq + 2);
        }
    }

    /// One `try_repeat` at the current time, called directly (what `poll` does only past the repeat deadline).
    fn repeat_now(src: &mut Rig) -> RepeatResult {
        let v = match src.status(qpc_now()) {
            Status::Live(v) => v,
            _ => panic!("setup: not live"),
        };
        let Rig { src: s, sink, .. } = src;
        s.try_repeat(&v, qpc_now(), sink)
    }

    #[test]
    fn a_repeat_whose_slot_is_stuck_mid_write_falls_back_to_the_newest_slot() {
        let path = temp_path("repeat-stuck-newest.bin");
        let _ = std::fs::remove_file(&path);
        let w = Writer::create(&path).unwrap();
        w.set_state(WriterState::Live);
        let (fw, fh) = (320u32, 180u32);
        let mut src = Rig::new(path.to_path_buf(), nv12(fw, fh, 30));
        src.start();
        publish_pattern(&w, fw, fh, 5);
        assert_eq!(
            next_ready(&mut src, Duration::from_secs(1)).kind,
            Kind::Live
        );
        let stuck = Stuck::of(&src);
        // two slots: the next frame goes to the other one, whole
        publish_pattern(&w, fw, fh, 6);
        assert_ne!(w.shared().header().latest_slot as usize, stuck.slot);
        // the writer died in the middle of a write into the slot of the frame that was shown last
        stuck.odd(&w);
        w.heartbeat();
        // the slot of the last frame is torn (not "moved": nothing newer is in it), so the repeat takes the newest
        // slot, which is whole: the best picture left. Without the fallback this is a retry for ever.
        let r = repeat_now(&mut src);
        assert!(matches!(r, RepeatResult::Copied), "no fallback");
        assert!(
            src.data() == pattern_frame(6, fw, fh).as_slice(),
            "the repeat is not the newest frame"
        );
        // what was shown is what the slot holds: not a new frame when it comes round again
        assert_ne!(src.last_id.map(|(s, _, _)| s as usize), Some(stuck.slot));
    }

    /// The fall back to the newest slot is a frame that is new to this stream like any other: one captured before the
    /// attach (less the tolerance) must not be shown, as `try_read` does not show it.
    #[test]
    fn a_repeat_never_falls_back_to_a_frame_older_than_the_attach() {
        let path = temp_path("repeat-fallback-old.bin");
        let _ = std::fs::remove_file(&path);
        let w = Writer::create(&path).unwrap();
        w.set_state(WriterState::Live);
        let (fw, fh) = (320u32, 180u32);
        let mut src = Rig::new(path.to_path_buf(), nv12(fw, fh, 30));
        src.start();
        publish_pattern(&w, fw, fh, 5);
        assert_eq!(
            next_ready(&mut src, Duration::from_secs(1)).kind,
            Kind::Live
        );
        let stuck = Stuck::of(&src);
        // the next frame is in the other slot and an old one: captured ten seconds before the attach (something an
        // earlier session left); the slot of the frame that was shown is torn
        publish_at(&w, fw, fh, 6, src.attach - qpc_frequency() * 10);
        assert_ne!(w.shared().header().latest_slot as usize, stuck.slot);
        stuck.odd(&w);
        w.heartbeat();
        let r = repeat_now(&mut src);
        assert!(
            !matches!(r, RepeatResult::Copied | RepeatResult::Newer(_)),
            "an old frame left in the file was shown as a repeat"
        );
        assert!(
            src.data() != pattern_frame(6, fw, fh).as_slice(),
            "the old frame is in the sink"
        );
    }

    #[test]
    fn a_last_slot_stuck_mid_write_gets_the_newest_frame_as_a_live_frame_not_a_repeat() {
        let path = temp_path("repeat-stuck-live.bin");
        let _ = std::fs::remove_file(&path);
        let w = Writer::create(&path).unwrap();
        w.set_state(WriterState::Live);
        let (fw, fh) = (320u32, 180u32);
        let mut src = Rig::new(path.to_path_buf(), nv12(fw, fh, 30));
        src.start();
        publish_pattern(&w, fw, fh, 5);
        assert_eq!(
            next_ready(&mut src, Duration::from_secs(1)).kind,
            Kind::Live
        );
        let stuck = Stuck::of(&src);
        // the next frame comes right behind (too soon after the last one to be taken at once), and the writer dies
        // in the middle of a write into the slot of the frame that was shown last
        publish_pattern(&w, fw, fh, 6);
        stuck.odd(&w);
        w.heartbeat();
        // once the repeat deadline has passed the newest frame is the next live frame
        let e = next_ready(&mut src, Duration::from_secs(1));
        assert_eq!(e.kind, Kind::Live);
        assert!(src.data() == pattern_frame(6, fw, fh).as_slice());
    }

    #[test]
    fn frames_decimated_behind_the_last_one_are_shown_when_the_camera_stalls() {
        let path = temp_path("decimated-stall.bin");
        let _ = std::fs::remove_file(&path);
        let w = Writer::create(&path).unwrap();
        w.set_state(WriterState::Live);
        let (fw, fh) = (320u32, 180u32);
        let mut src = Rig::new(path.to_path_buf(), nv12(fw, fh, 30));
        src.start();
        publish_pattern(&w, fw, fh, 5);
        let first = next_ready(&mut src, Duration::from_secs(1));
        assert_eq!(first.kind, Kind::Live);
        // two frames right behind the shown one (a writer at 60+ fps, or a burst), both captured inside the gap:
        // the writer alternates between two slots, so the second one reuses the slot of frame 5. Then the camera
        // stalls: only the heartbeat goes on, as the app does on a capture timeout.
        let shown = Stuck::of(&src);
        publish_pattern(&w, fw, fh, 6);
        publish_pattern(&w, fw, fh, 7);
        assert_ne!(
            w.shared().slot_meta(shown.slot).seq,
            shown.seq,
            "setup: the slot of frame 5 was not reused"
        );
        let period = vcam_protocol::qpc_frequency() / 30;
        let t0 = Instant::now();
        let e = loop {
            w.heartbeat();
            match src.poll(qpc_now()) {
                Poll::Ready(e) => {
                    src.finish_emit(e, qpc_now());
                    break e;
                }
                Poll::Wait(_) => {
                    assert!(t0.elapsed() < Duration::from_secs(1), "the stream starved");
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        };
        // the newest frame, as a live frame, about two periods after the last one (the bound has room for a busy
        // test machine; a starved stream never gets here)
        assert_eq!(e.kind, Kind::Live);
        assert!(src.data() == pattern_frame(7, fw, fh).as_slice());
        assert!(
            e.at - first.at <= period * 3,
            "shown {} ticks after the last frame (period {period})",
            e.at - first.at
        );
        assert!(e.at - first.at >= period * 3 / 2, "decimation ended early");
        // its own capture time, and a sample time that rises
        assert!(e.capture_qpc > first.capture_qpc);
        let ts_first = crate::stream::next_timestamp(first.sample_qpc(), 0);
        assert!(crate::stream::next_timestamp(e.sample_qpc(), ts_first) > ts_first);
        // and the stall goes on: a repeat of that frame, not a hang
        let e = next_ready(&mut src, Duration::from_secs(1));
        assert_eq!(e.kind, Kind::Repeat);
        assert!(src.data() == pattern_frame(7, fw, fh).as_slice());
    }

    #[test]
    fn a_repeat_whose_slot_stays_mid_write_delivers_nothing_and_goes_on_when_it_settles() {
        let path = temp_path("repeat-stuck-odd.bin");
        let _ = std::fs::remove_file(&path);
        let w = Writer::create(&path).unwrap();
        w.set_state(WriterState::Live);
        let (fw, fh) = (320u32, 180u32);
        let mut src = Rig::new(path.to_path_buf(), nv12(fw, fh, 30));
        src.start();
        publish_pattern(&w, fw, fh, 5);
        assert_eq!(
            next_ready(&mut src, Duration::from_secs(1)).kind,
            Kind::Live
        );
        let stuck = Stuck::of(&src);
        let good = src.data().to_vec();
        // the only slot there is stays odd: no frame, no slate, no crash
        stuck.odd(&w);
        let began = Instant::now();
        while began.elapsed() < Duration::from_millis(250) {
            w.heartbeat();
            if let Poll::Ready(e) = src.poll(qpc_now()) {
                panic!(
                    "{:?} was delivered from a slot that is being written",
                    e.kind
                );
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        stuck.even(&w);
        w.heartbeat();
        let e = next_ready(&mut src, Duration::from_secs(1));
        assert_eq!(e.kind, Kind::Repeat);
        assert!(src.data() == good.as_slice());
    }

    #[test]
    fn a_slot_that_holds_a_newer_frame_than_the_last_shown_is_not_a_repeat() {
        let path = temp_path("repeat-newer.bin");
        let _ = std::fs::remove_file(&path);
        let w = Writer::create(&path).unwrap();
        w.set_state(WriterState::Live);
        let (fw, fh) = (320u32, 180u32);
        let mut src = Rig::new(path.to_path_buf(), nv12(fw, fh, 30));
        src.start();
        publish_pattern(&w, fw, fh, 5);
        assert_eq!(
            next_ready(&mut src, Duration::from_secs(1)).kind,
            Kind::Live
        );
        let shown = Stuck::of(&src);
        // two frames right behind (too soon after the last one to be live frames): the writer puts 6 in the other
        // slot and then reuses the slot of frame 5 for frame 7
        publish_pattern(&w, fw, fh, 6);
        publish_pattern(&w, fw, fh, 7);
        assert_ne!(
            w.shared().slot_meta(shown.slot).seq,
            shown.seq,
            "setup: the slot of frame 5 was not reused"
        );
        let r = repeat_now(&mut src);
        // the slot holds a newer frame: it is a live frame with its own capture time, never a plain repeat
        assert!(
            matches!(r, RepeatResult::Newer(_)),
            "a newer frame went out as a repeat"
        );
        assert!(src.data() == pattern_frame(7, fw, fh).as_slice());
        // (the live path shows the newest frame once the repeat deadline has passed: see
        // `frames_decimated_behind_the_last_one_are_shown_when_the_camera_stalls`)
    }

    /// Frame 5 in slot A (shown), 6 in B, 7 (right size, decimated) in A, 8 (another size) in B, then the writer
    /// stalls (the app only keeps the heartbeat alive). `try_read` skips 8 on its size and `try_repeat` found 7 in
    /// the slot of frame 5, called it "moved" and asked for a retry for ever: nothing was delivered until the writer
    /// published again.
    #[test]
    fn a_newer_frame_in_the_last_slot_goes_out_live_when_the_newest_frame_has_another_size() {
        let path = temp_path("repeat-moved-other-size.bin");
        let _ = std::fs::remove_file(&path);
        let w = Writer::create(&path).unwrap();
        w.set_state(WriterState::Live);
        let (fw, fh) = (320u32, 180u32);
        let mut src = Rig::new(path.to_path_buf(), nv12(fw, fh, 30));
        src.start();
        publish_pattern(&w, fw, fh, 5);
        let first = next_ready(&mut src, Duration::from_secs(1));
        assert_eq!(first.kind, Kind::Live);
        let shown = Stuck::of(&src);
        publish_pattern(&w, fw, fh, 6);
        publish_pattern(&w, fw, fh, 7);
        publish_pattern(&w, 160, 90, 8);
        assert_ne!(
            w.shared().slot_meta(shown.slot).seq,
            shown.seq,
            "setup: the slot of frame 5 was not reused"
        );
        let latest = w.shared().header().latest_slot as usize;
        assert_eq!(
            w.shared().slot_meta(latest).width,
            160,
            "setup: the newest slot does not hold the other size"
        );
        let t0 = Instant::now();
        let e = loop {
            w.heartbeat();
            match src.poll(qpc_now()) {
                Poll::Ready(e) => {
                    src.finish_emit(e, qpc_now());
                    break e;
                }
                Poll::Wait(_) => {
                    assert!(
                        t0.elapsed() < Duration::from_secs(1),
                        "the stream starved: a newer frame in the last slot was never taken"
                    );
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        };
        assert_eq!(e.kind, Kind::Live);
        assert!(src.data() == pattern_frame(7, fw, fh).as_slice());
        assert!(e.capture_qpc > first.capture_qpc, "its own capture time");
        // and the stall goes on: a repeat of that frame
        let e = next_ready(&mut src, Duration::from_secs(1));
        assert_eq!(e.kind, Kind::Repeat);
        assert!(src.data() == pattern_frame(7, fw, fh).as_slice());
    }

    fn publish_at(w: &Writer, width: u32, height: u32, n: u64, capture: u64) {
        let fmt = SlotFormat {
            width,
            height,
            stride: width,
            subtype: SUBTYPE_NV12,
        };
        w.publish(fmt, capture, |d| {
            pattern::fill_nv12(n, width, height, d, width as usize)
        })
        .unwrap();
    }

    /// A 60 fps source that took frame 5 at `t0` with a capture time 40 ms after the decision (a camera clock a
    /// little ahead: the clamp allows 50 ms), and frames 6 and 7 right behind it (the writer reuses the slot of
    /// frame 5 for 7), the writer stalled after them. The polls are made at chosen times, so nothing here depends on
    /// the speed of the machine.
    fn took_a_frame_captured_after_the_take(name: &str, reuse: bool) -> (Writer, Rig, u64, Emit) {
        let path = temp_path(name);
        let _ = std::fs::remove_file(&path);
        let w = Writer::create(&path).unwrap();
        w.set_state(WriterState::Live);
        let mut src = Rig::new(path.to_path_buf(), nv12(320, 180, 60));
        src.start();
        let freq = qpc_frequency();
        let t0 = qpc_now();
        publish_at(&w, 320, 180, 5, t0 + freq / 25);
        let Poll::Ready(first) = src.poll(t0) else {
            panic!("setup: frame 5 was not taken");
        };
        assert_eq!(first.kind, Kind::Live);
        assert_eq!(first.capture_qpc, t0 + freq / 25);
        src.finish_emit(first, t0);
        let shown = Stuck::of(&src);
        publish_at(&w, 320, 180, 6, qpc_now());
        if reuse {
            publish_at(&w, 320, 180, 7, qpc_now());
            assert_ne!(
                w.shared().slot_meta(shown.slot).seq,
                shown.seq,
                "setup: the slot of frame 5 was not reused"
            );
        }
        (w, src, t0, first)
    }

    #[test]
    fn the_wait_never_sleeps_past_the_repeat_deadline() {
        let (_w, mut src, t0, _) =
            took_a_frame_captured_after_the_take("late-capture-wait.bin", true);
        let freq = qpc_frequency();
        let period = freq / 60;
        // 20 ms after the take: before the deadline (25 ms), and the earliest a frame qualifies (capture + 3/4 of a
        // period, 52 ms after the take) is long after it
        match src.poll(t0 + freq / 50) {
            Poll::Wait(Wait::Until(t)) => assert!(
                t <= t0 + period * 3 / 2,
                "sleeps until {} ticks after the take, the deadline is at {}",
                t - t0,
                period * 3 / 2
            ),
            p => panic!("{p:?}"),
        }
    }

    #[test]
    fn a_frame_is_taken_at_the_repeat_deadline_also_when_its_capture_gate_is_later() {
        // frame 6 sits in the other slot, the slot of frame 5 still holds frame 5 (a repeat would be possible)
        let (_w, mut src, t0, _) =
            took_a_frame_captured_after_the_take("late-capture-take.bin", false);
        // 26 ms after the take: the deadline (25 ms) has passed, `next_ok` (capture + 3/4 of a period) has not. The
        // newest frame must go out as a live frame, not wait behind `next_ok` and be answered with a repeat of 5
        let e = match src.poll(t0 + qpc_frequency() * 26 / 1000) {
            Poll::Ready(e) => e,
            p => panic!("nothing delivered at the deadline: {p:?}"),
        };
        assert_eq!(e.kind, Kind::Live);
        assert!(src.data() == pattern_frame(6, 320, 180).as_slice());
    }

    #[test]
    fn a_frame_taken_past_the_deadline_is_stamped_a_gap_after_the_one_before_whatever_its_capture_time(
    ) {
        let (_w, mut src, t0, first) =
            took_a_frame_captured_after_the_take("late-capture-stamp.bin", true);
        let gap = qpc_frequency() / 60 * 3 / 4;
        let e = match src.poll(t0 + qpc_frequency() * 26 / 1000) {
            Poll::Ready(e) => e,
            p => panic!("nothing delivered at the deadline: {p:?}"),
        };
        assert_eq!(e.kind, Kind::Live);
        // frame 7 was captured before frame 5 (the camera clock was ahead): the real capture time stays for the
        // latency statistics, the sample is stamped a gap after the one before
        assert!(e.capture_qpc < first.capture_qpc, "setup");
        assert!(
            e.sample_qpc() >= first.sample_qpc() + gap,
            "the samples overlap: {} ticks apart, a gap is {gap}",
            e.sample_qpc().saturating_sub(first.sample_qpc())
        );
        // never more than 50 ms ahead of the decision
        assert!(e.sample_qpc() <= t0 + qpc_frequency() * 26 / 1000 + qpc_frequency() / 20);
        // and a frame that is far enough behind the last one is stamped with its own capture time
        let (_w2, mut src2, t0, first) =
            took_a_frame_captured_after_the_take("late-capture-stamp-own.bin", true);
        publish_at(&_w2, 320, 180, 8, t0 + qpc_frequency() / 10);
        let e = match src2.poll(t0 + qpc_frequency() * 26 / 1000) {
            Poll::Ready(e) => e,
            p => panic!("nothing delivered at the deadline: {p:?}"),
        };
        assert!(e.capture_qpc > first.sample_qpc() + gap);
        assert_eq!(e.sample_qpc(), e.capture_qpc);
    }

    /// The review's sequence: frame 5 stamped t0+40 ms; frame 7 taken past the repeat deadline, stamped a gap
    /// (12.5 ms at 60 fps) after it, t0+52.5 ms; then frame 8, captured at t0+20 ms (the decimation looks at raw capture
    /// times, and 20 ms after frame 7's own capture it passes). Its own capture time lies before the stamp of frame 7:
    /// stamped with it the sample times would run backwards.
    #[test]
    fn sample_stamps_never_run_backwards_after_a_frame_that_was_moved_later() {
        let (w, mut src, t0, first) =
            took_a_frame_captured_after_the_take("stamp-monotonic.bin", true);
        let freq = qpc_frequency();
        let gap = freq / 60 * 3 / 4;
        assert_eq!(
            first.sample_qpc(),
            t0 + freq / 25,
            "frame 5: its own capture time"
        );
        let now7 = t0 + freq * 26 / 1000;
        let Poll::Ready(e7) = src.poll(now7) else {
            panic!("frame 7 was not taken at the deadline");
        };
        src.finish_emit(e7, now7);
        assert_eq!(e7.kind, Kind::Live);
        assert!(
            e7.sample_qpc() >= first.sample_qpc() + gap,
            "frame 7 overlaps frame 5"
        );
        // frame 8: captured 20 ms after the take of frame 5, polled 45 ms after it
        publish_at(&w, 320, 180, 8, t0 + freq / 50);
        let now8 = t0 + freq * 45 / 1000;
        let Poll::Ready(e8) = src.poll(now8) else {
            panic!("frame 8 was not taken");
        };
        assert_eq!(e8.kind, Kind::Live);
        assert!(
            src.data() == pattern_frame(8, 320, 180).as_slice(),
            "setup: not frame 8"
        );
        assert_eq!(
            e8.capture_qpc,
            t0 + freq / 50,
            "setup: frame 8's capture time"
        );
        assert!(
            e8.sample_qpc() >= e7.sample_qpc() + gap,
            "frame 8 is stamped {} ticks after frame 7 (a gap is {gap}): the sample times ran backwards or overlap",
            e8.sample_qpc() as i64 - e7.sample_qpc() as i64
        );
        // never more than 50 ms ahead of the decision
        assert!(e8.sample_qpc() <= now8 + freq / 20);
    }

    #[test]
    fn a_frame_captured_before_the_stream_attached_is_never_shown() {
        let path = temp_path("old-frame.bin");
        let _ = std::fs::remove_file(&path);
        let w = Writer::create(&path).unwrap();
        w.set_state(WriterState::Live);
        let freq = qpc_frequency();
        // what an earlier session left: a frame captured a second ago, with a writer that is alive again
        publish_at(&w, 320, 180, 1, qpc_now() - freq);
        let mut src = Rig::new(path.to_path_buf(), nv12(320, 180, 30));
        src.start();
        let began = Instant::now();
        while began.elapsed() < Duration::from_millis(200) {
            w.heartbeat();
            let e = next_ready(&mut src, Duration::from_secs(1));
            assert_eq!(e.kind, Kind::Slate(SlateKind::Starting), "an old frame");
        }
        // a frame captured after the attach is shown
        publish_at(&w, 320, 180, 2, qpc_now());
        let e = next_ready(&mut src, Duration::from_secs(1));
        assert_eq!(e.kind, Kind::Live);
        assert!(src.data() == pattern_frame(2, 320, 180).as_slice());
    }

    #[test]
    fn the_attach_tolerance_is_the_larger_of_a_quarter_second_and_two_periods() {
        let f = 10_000_000u64;
        assert_eq!(attach_tolerance(f, f / 30), f / 4);
        assert_eq!(attach_tolerance(f, f / 60), f / 4);
        // a slow camera: two of its periods
        assert_eq!(attach_tolerance(f, f / 4), f / 2);
        assert_eq!(attach_tolerance(f, f / 2), f);
        // no overflow, for any input
        assert!(attach_tolerance(u64::MAX, u64::MAX) == u64::MAX);
    }

    /// M-3: the newest frame in the file of a live writer is as old as the capture-to-publish latency plus where
    /// the attach falls in the frame period: a stream that attaches to a steady 14 fps writer whose frames are
    /// 20 to 90 ms old when the file is read must get a frame at once and never flash the "starting" slate.
    #[test]
    fn a_steady_writer_never_gets_the_starting_slate_at_the_attach() {
        let freq = qpc_frequency();
        for age_ms in [20u64, 35, 48, 60, 71, 90, 140, 200] {
            let path = temp_path(&format!("attach-latency-{age_ms}.bin"));
            let _ = std::fs::remove_file(&path);
            let w = Writer::create(&path).unwrap();
            w.set_state(WriterState::Live);
            // the writer has been publishing at 14 fps: the newest frame is `age_ms` older than the attach
            let mut src = Rig::new(path.to_path_buf(), nv12(320, 180, 14));
            src.start();
            publish_at(&w, 320, 180, 3, src.attach - freq * age_ms / 1000);
            w.heartbeat();
            let e = next_ready(&mut src, Duration::from_secs(1));
            assert_eq!(
                e.kind,
                Kind::Live,
                "a frame captured {age_ms} ms before the attach showed the slate"
            );
            assert!(src.data() == pattern_frame(3, 320, 180).as_slice());
        }
    }

    /// The tolerance follows the frame period of a slow camera: two periods, here 2 x 250 ms.
    #[test]
    fn the_attach_tolerance_follows_a_slow_cameras_period() {
        let freq = qpc_frequency();
        // 4 fps: tolerance is 500 ms
        for (before_ms, shown) in [(450u64, true), (600, false)] {
            let path = temp_path(&format!("attach-slow-{shown}.bin"));
            let _ = std::fs::remove_file(&path);
            let w = Writer::create(&path).unwrap();
            w.set_state(WriterState::Live);
            let mut src = Rig::new(path.to_path_buf(), nv12(320, 180, 4));
            src.start();
            publish_at(&w, 320, 180, 3, src.attach - freq * before_ms / 1000);
            w.heartbeat();
            let e = next_ready(&mut src, Duration::from_secs(2));
            assert_eq!(
                e.kind == Kind::Live,
                shown,
                "{before_ms} ms before the attach"
            );
        }
    }

    #[test]
    fn the_attach_tolerance_keeps_a_frame_captured_right_at_the_start_and_not_one_before_it() {
        let freq = qpc_frequency();
        // 200 ms before the attach (the capture latency of a camera, a camera clock behind QPC): shown
        // 300 ms before: an old frame
        for (before, shown) in [(freq * 200 / 1000, true), (freq * 300 / 1000, false)] {
            let path = temp_path(&format!("attach-tolerance-{shown}.bin"));
            let _ = std::fs::remove_file(&path);
            let w = Writer::create(&path).unwrap();
            w.set_state(WriterState::Live);
            let mut src = Rig::new(path.to_path_buf(), nv12(320, 180, 30));
            src.start();
            publish_at(&w, 320, 180, 3, src.attach - before);
            let began = Instant::now();
            let mut live = false;
            while began.elapsed() < Duration::from_millis(150) {
                w.heartbeat();
                live |= next_ready(&mut src, Duration::from_secs(1)).kind == Kind::Live;
            }
            assert_eq!(
                live, shown,
                "a frame captured {before} ticks before the attach"
            );
        }
    }

    #[test]
    fn a_file_the_dll_can_only_read_shows_the_not_running_slate_and_recovers() {
        let path = temp_path("read-only.bin");
        let unlock = |ro: bool| {
            if let Ok(m) = std::fs::metadata(&path) {
                let mut p = m.permissions();
                p.set_readonly(ro);
                let _ = std::fs::set_permissions(&path, p);
            }
        };
        unlock(false);
        let _ = std::fs::remove_file(&path);
        let w = Writer::create(&path).unwrap();
        w.set_state(WriterState::Live);
        unlock(true);
        let mut src = Rig::new(path.to_path_buf(), nv12(320, 180, 30));
        src.start();
        // a writer that is alive and a frame captured after the attach: the DLL cannot announce itself, so the
        // engine would never publish like this: the slate, not the frame
        let began = Instant::now();
        while began.elapsed() < Duration::from_millis(250) {
            publish_at(&w, 320, 180, 4, qpc_now());
            let e = next_ready(&mut src, Duration::from_secs(1));
            assert_eq!(e.kind, Kind::Slate(SlateKind::NotRunning), "read-only view");
        }
        assert_eq!(w.shared().header().consumer_active, 0);
        assert!(src.read_only_logged);
        // write access again: the file is reopened (every half second) and frames flow
        unlock(false);
        let mut live = false;
        let began = Instant::now();
        while began.elapsed() < Duration::from_secs(3) && !live {
            publish_at(&w, 320, 180, 5, qpc_now());
            live = next_ready(&mut src, Duration::from_secs(1)).kind == Kind::Live;
        }
        assert!(live, "no recovery once the file was writable");
        assert_eq!(w.shared().header().consumer_active, 1);
    }

    #[test]
    fn a_sink_with_a_buffer_that_is_too_short_gets_nothing_written_past_its_end() {
        struct Short(Vec<u8>);
        impl FrameSink for Short {
            fn buffer(&mut self) -> Option<FrameBuf<'_>> {
                Some(FrameBuf {
                    data: &mut self.0,
                    pitch: 320,
                })
            }
        }
        let path = temp_path("short.bin");
        let _ = std::fs::remove_file(&path);
        let w = Writer::create(&path).unwrap();
        w.set_state(WriterState::Live);
        let mut src = FrameSource::new(path.to_path_buf(), nv12(320, 180, 30));
        src.start();
        publish_pattern(&w, 320, 180, 3);
        let mut sink = Short(vec![7u8; 320 * 180]); // a Y plane, no UV rows
        let began = Instant::now();
        while began.elapsed() < Duration::from_millis(200) {
            assert!(
                matches!(src.poll(qpc_now(), &mut sink), Poll::Wait(_)),
                "an emission into a buffer that cannot hold the frame"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(
            sink.0.iter().all(|&b| b == 7),
            "the short buffer was written"
        );
    }

    // ---- Wait::Frame: where "a millisecond" used to be ----------------------------------------------------------

    /// The wait the pump made before this change, recomputed from the state the poll left (the end of `poll_inner`):
    /// `Soon` when the earliest time a frame could qualify has passed, else `Until` it.
    fn old_wait(src: &FrameSource, now: u64) -> Wait {
        let next_ok = match src.last_capture {
            Some(c) => c
                .saturating_add(src.min_capture_gap())
                .min(now.saturating_add(src.period)),
            None => 0,
        };
        let deadline = src.last_new_seen.saturating_add(src.period * 3 / 2);
        let wake = if src.have_frame {
            next_ok.min(deadline)
        } else {
            next_ok
        };
        if now >= wake {
            Wait::Soon
        } else {
            Wait::Until(wake)
        }
    }

    /// A pump on a simulated clock for `secs` seconds: the writer publishes `writer_fps` frames a second, the stream
    /// runs at `stream_fps`, and the pump polls the way polling mode does (`Soon` and `Frame` are a millisecond, `Until`
    /// sleeps to its time). Returns every wait the source asked for, with the time and the oracle's answer, and the
    /// kinds of what was emitted.
    fn simulate(
        name: &str,
        writer_fps: u32,
        stream_fps: u32,
        secs: f64,
    ) -> (Vec<(u64, Wait, Wait)>, Vec<Kind>) {
        let path = temp_path(name);
        let _ = std::fs::remove_file(&path);
        let w = Writer::create(&path).unwrap();
        w.set_state(WriterState::Live);
        let (fw, fh) = (320u32, 180u32);
        let mut src = Rig::new(path.to_path_buf(), nv12(fw, fh, stream_fps));
        src.start();
        let freq = qpc_frequency();
        let t0 = qpc_now();
        let end = t0 + (freq as f64 * secs) as u64;
        let step = freq / 1000;
        let mut next_pub = 0u32;
        let mut now = t0;
        let (mut waits, mut kinds) = (Vec::new(), Vec::new());
        // the writer's frames are on the same clock: published when the simulated time reaches them
        while now < end {
            // (with its own capture time: a writer's frames are not bunched up because the pump slept)
            loop {
                let due = t0 + (freq as u128 * next_pub as u128 / writer_fps as u128) as u64;
                if due > now {
                    break;
                }
                publish_at(&w, fw, fh, next_pub as u64, due);
                next_pub += 1;
            }
            w.heartbeat();
            match src.poll(now) {
                Poll::Ready(e) => {
                    src.finish_emit(e, now);
                    kinds.push(e.kind);
                    now += 1;
                }
                Poll::Wait(got) => {
                    waits.push((now, got, old_wait(&src, now)));
                    now = match got {
                        Wait::Until(t) => t.max(now + 1),
                        Wait::Frame(_) | Wait::Soon => now + step,
                    };
                }
            }
        }
        (waits, kinds)
    }

    /// `Frame(t)` is returned exactly where `Soon` was, and never past the next time-based decision; `Until` is
    /// untouched. Writers at the stream's rate, slower and faster.
    #[test]
    fn frame_replaces_soon_exactly_and_never_waits_past_a_decision_time() {
        for (name, writer, stream) in [
            ("wf-60-60", 60u32, 60u32),
            ("wf-30-30", 30, 30),
            ("wf-14-30", 14, 30),
            ("wf-60-30", 60, 30),
            ("wf-5-60", 5, 60),
        ] {
            let (waits, kinds) = simulate(name, writer, stream, 0.8);
            assert!(!waits.is_empty() && !kinds.is_empty(), "{name}");
            let (mut frame, mut soon, mut until) = (0, 0, 0);
            for (now, got, old) in &waits {
                match (got, old) {
                    // the old pump slept until this time: unchanged
                    (Wait::Until(a), Wait::Until(b)) => {
                        assert_eq!(a, b, "{name}: Until changed");
                        until += 1;
                    }
                    // the old pump polled again in a millisecond: the new one says what it waits for
                    (Wait::Frame(t), Wait::Soon) => {
                        assert!(*t > *now, "{name}: Frame({t}) is not after now ({now})");
                        // never more than one and a half frame periods ahead: the repeat deadline, the repeat gate
                        // and the "starting" point are all at most that far
                        let period = qpc_frequency() / stream as u64;
                        assert!(
                            *t - *now <= period * 3 / 2 + 1,
                            "{name}: waits {} ticks",
                            t - now
                        );
                        frame += 1;
                    }
                    // the torn repeat, and the slate pacing a live stream shows before its first frame
                    (Wait::Soon, Wait::Soon) => soon += 1,
                    (Wait::Until(_), _) => {} // slate pacing (`Until` the next slate), not the end of `poll_inner`
                    other => panic!(
                        "{name}: the old pump waited {:?}, the new one {:?}",
                        other.1, other.0
                    ),
                }
            }
            // a writer faster than the stream always has a frame when one qualifies: it never waits for one
            assert!(
                frame > 0 || writer > stream,
                "{name}: no Frame wait at all ({until} until, {soon} soon)"
            );
            assert_eq!(soon, 0, "{name}: a Soon without a torn repeat");
        }
    }

    /// With the polling mapping of [`crate::wake`], a stream makes the polls it always made: the emitted kinds over a
    /// run are the same whether `Frame(t)` is mapped to a millisecond or the source were to answer `Soon` there.
    #[test]
    fn polling_mode_emits_what_the_old_pump_emitted() {
        for (name, writer, stream) in [
            ("pe-60-60", 60u32, 60u32),
            ("pe-14-30", 14, 30),
            ("pe-60-30", 60, 30),
        ] {
            let (_, kinds) = simulate(name, writer, stream, 0.8);
            let live = kinds.iter().filter(|k| **k == Kind::Live).count();
            let repeats = kinds.iter().filter(|k| **k == Kind::Repeat).count();
            let want = (0.8 * writer.min(stream) as f64) as usize;
            assert!(
                live + 2 >= want && live <= want + 2,
                "{name}: {live} live, wanted about {want}"
            );
            if writer >= stream {
                assert!(repeats <= 2, "{name}: {repeats} repeats");
            } else {
                // a slower writer is filled up to the stream's rate with repeats
                assert!(
                    repeats + live + 3 >= (0.8 * stream as f64) as usize,
                    "{name}: {} emitted",
                    live + repeats
                );
            }
        }
    }

    /// The three places `Frame(t)` can point at, one by one.
    #[test]
    fn frame_points_at_the_repeat_deadline_the_repeat_gate_and_the_starting_slate() {
        let freq = qpc_frequency();
        let period = freq / 30;
        // no frame yet: the "starting" slate's point, 1.5 periods after the stream went live
        let path = temp_path("wf-starting.bin");
        let _ = std::fs::remove_file(&path);
        let w = Writer::create(&path).unwrap();
        w.set_state(WriterState::Live);
        let mut src = Rig::new(path.to_path_buf(), nv12(320, 180, 30));
        src.start();
        let t0 = qpc_now();
        assert_eq!(src.poll(t0), Poll::Wait(Wait::Frame(t0 + period * 3 / 2)));
        // later polls keep the same point
        let t1 = t0 + period / 2;
        assert_eq!(src.poll(t1), Poll::Wait(Wait::Frame(t0 + period * 3 / 2)));

        // a frame on show: past `next_ok`, before the deadline: the deadline (1.5 periods after the take)
        let path = temp_path("wf-deadline.bin");
        let _ = std::fs::remove_file(&path);
        let w = Writer::create(&path).unwrap();
        w.set_state(WriterState::Live);
        let mut src = Rig::new(path.to_path_buf(), nv12(320, 180, 30));
        src.start();
        let t0 = qpc_now();
        publish_at(&w, 320, 180, 1, t0);
        let Poll::Ready(e) = src.poll(t0) else {
            panic!("no frame")
        };
        src.finish_emit(e, t0);
        // a decimated wait first: Until the earliest next frame (3/4 of a period)
        assert_eq!(
            src.poll(t0 + 1),
            Poll::Wait(Wait::Until(t0 + period * 3 / 4))
        );
        let now = t0 + period * 3 / 4 + 10;
        w.heartbeat();
        assert_eq!(src.poll(now), Poll::Wait(Wait::Frame(t0 + period * 3 / 2)));
        // at the deadline the last frame goes out again, and until `last_emit + period` allows another the wait is
        // for that time
        let at = t0 + period * 3 / 2;
        w.heartbeat();
        let Poll::Ready(r) = src.poll(at) else {
            panic!("no repeat at the deadline")
        };
        assert_eq!(r.kind, Kind::Repeat);
        src.finish_emit(r, at);
        w.heartbeat();
        assert_eq!(
            src.poll(at + 5),
            Poll::Wait(Wait::Frame(at + period)),
            "the repeat gate"
        );
    }

    /// A torn repeat is the one case with nothing to wait for: it is asked again at once.
    #[test]
    fn a_torn_repeat_is_asked_again_in_a_millisecond() {
        use vcam_protocol::{off, slot_off};
        let path = temp_path("wf-torn.bin");
        let _ = std::fs::remove_file(&path);
        let w = std::rc::Rc::new(Writer::create(&path).unwrap());
        w.set_state(WriterState::Live);
        let (fw, fh) = (320u32, 180u32);
        let mut src = Rig::new(path.to_path_buf(), nv12(fw, fh, 30));
        src.start();
        publish_pattern(&w, fw, fh, 5);
        assert_eq!(
            next_ready(&mut src, Duration::from_secs(1)).kind,
            Kind::Live
        );
        let w2 = w.clone();
        COPY_HOOK.with(|h| {
            *h.borrow_mut() = Some(Box::new(move || {
                let sh = w2.shared();
                let slot = sh.header().latest_slot as usize;
                let meta = sh.slot_meta(slot);
                sh.poke64(slot_off(slot) + off::SLOT_SEQ, meta.seq + 1);
            }))
        });
        let mut saw_soon = false;
        let began = Instant::now();
        while began.elapsed() < Duration::from_millis(300) {
            w.heartbeat();
            if let Poll::Wait(Wait::Soon) = src.poll(qpc_now()) {
                saw_soon = true;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        COPY_HOOK.with(|h| *h.borrow_mut() = None);
        assert!(saw_soon, "a torn repeat gave no Soon");
    }

    /// The source reports what the pump waits by: Live with the advert and counts of the very header it read, nothing
    /// outside Live, and the counters now.
    #[test]
    fn the_source_reports_the_advert_only_while_the_writer_is_live() {
        let path = temp_path("wf-report.bin");
        let _ = std::fs::remove_file(&path);
        let w = Writer::create(&path).unwrap();
        let mut src = Rig::new(path.to_path_buf(), nv12(320, 180, 30));
        src.start();
        w.set_state(WriterState::Paused);
        let _ = src.poll(qpc_now());
        assert_eq!(src.report(), WakeReport::default(), "paused");
        w.set_state(WriterState::Live);
        publish_pattern(&w, 320, 180, 1);
        let _ = src.poll(qpc_now());
        let r = src.report();
        assert!(r.live);
        let want = w.header();
        assert_eq!(r.advert.map(|a| a.nonces), w.frame_event_nonces());
        assert_eq!(r.advert.unwrap().signal_seq, want.signal_seq);
        assert_eq!(r.writer_frames, want.writer_frames);
        assert_eq!(src.counters(), Some((want.signal_seq, want.writer_frames)));
        w.set_state(WriterState::Closed);
        let _ = src.poll(qpc_now());
        assert_eq!(src.report(), WakeReport::default(), "closed");
        // a header that does not validate
        w.set_state(WriterState::Live);
        w.shared().poke32(vcam_protocol::off::PROTOCOL, 9);
        let _ = src.poll(qpc_now());
        assert_eq!(src.report(), WakeReport::default(), "foreign protocol");
    }

    /// The diagnostics go into the header only while it validates (like every other write of the DLL).
    #[test]
    fn the_wait_diagnostics_are_written_only_into_a_valid_header() {
        let path = temp_path("wf-diag.bin");
        let _ = std::fs::remove_file(&path);
        let w = Writer::create(&path).unwrap();
        w.set_state(WriterState::Live);
        let mut src = Rig::new(path.to_path_buf(), nv12(320, 180, 30));
        src.start();
        publish_pattern(&w, 320, 180, 1);
        let _ = src.poll(qpc_now());
        let d = Diag {
            mode: 1,
            events: 7,
            missed: 2,
            spurious: 3,
        };
        src.publish_diag(&d);
        let h = w.header();
        assert_eq!(
            (h.wake_mode, h.wake_events, h.wake_missed, h.wake_spurious),
            (1, 7, 2, 3)
        );
        // a foreign protocol version: the next poll sees it, and nothing is written
        w.shared().poke32(vcam_protocol::off::PROTOCOL, 9);
        let _ = src.poll(qpc_now());
        src.publish_diag(&Diag {
            mode: 4,
            events: 99,
            missed: 99,
            spurious: 99,
        });
        let h = w.header();
        assert_eq!(
            (h.wake_mode, h.wake_events),
            (1, 7),
            "written into a header that did not validate"
        );
        // a file that is not a frame file at all (the magic is wrong): the same
        w.shared().poke32(vcam_protocol::off::PROTOCOL, 1);
        w.shared().poke32(vcam_protocol::off::MAGIC, 0xBAD0_BAD0);
        let _ = src.poll(qpc_now());
        src.publish_diag(&Diag { mode: 3, ..d });
        assert_eq!(w.header().wake_mode, 1);
        // the header validates again: the last numbers the pump gave (mode 3, not the 4 and the 99s of the invalid
        // time that came before it) go in at that poll, and not only when the file is reopened
        w.shared()
            .poke32(vcam_protocol::off::MAGIC, vcam_protocol::MAGIC);
        let _ = src.poll(qpc_now());
        let h = w.header();
        assert_eq!(
            (h.wake_mode, h.wake_events, h.wake_missed, h.wake_spurious),
            (3, 7, 2, 3),
            "a change made while the header was invalid is lost"
        );
    }

    /// A pump that ends without `Wake::finish` (a panic) leaves the last diagnostics it published, `wake_mode` 1 among
    /// them: the stop of the source (its `Drop`) says that nothing waits any more, and only into a valid header.
    #[test]
    fn stopping_the_source_writes_wake_mode_zero_into_a_valid_header_only() {
        let events = Diag {
            mode: 1,
            events: 7,
            missed: 0,
            spurious: 0,
        };
        // the pump died with the mode at 1: the drop of the source writes 0
        let path = temp_path("wf-stop-diag.bin");
        let _ = std::fs::remove_file(&path);
        let w = Writer::create(&path).unwrap();
        w.set_state(WriterState::Live);
        let mut src = Rig::new(path.to_path_buf(), nv12(320, 180, 30));
        src.start();
        publish_pattern(&w, 320, 180, 1);
        let _ = src.poll(qpc_now());
        src.publish_diag(&events);
        assert_eq!(w.header().wake_mode, 1);
        drop(src);
        let h = w.header();
        assert_eq!((h.wake_mode, h.wake_events), (0, 7), "after the drop");

        // the file was replaced by a foreign one since the last poll: the stop checks the header itself and writes nothing
        let path = temp_path("wf-stop-foreign.bin");
        let _ = std::fs::remove_file(&path);
        let w = Writer::create(&path).unwrap();
        w.set_state(WriterState::Live);
        let mut src = Rig::new(path.to_path_buf(), nv12(320, 180, 30));
        src.start();
        publish_pattern(&w, 320, 180, 1);
        let _ = src.poll(qpc_now());
        src.publish_diag(&events);
        assert!(w.header().consumer_active != 0);
        w.shared().poke32(vcam_protocol::off::PROTOCOL, 9);
        src.stop();
        let h = w.header();
        assert_eq!(
            h.wake_mode, 1,
            "written into a header that did not validate"
        );
        assert!(
            h.consumer_active != 0,
            "consumer_active cleared in a foreign file"
        );
    }
}
