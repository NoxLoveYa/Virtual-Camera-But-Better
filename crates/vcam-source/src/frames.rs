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
//! | writer `paused` | slate "Camera paused" |
//! | writer `starting`, or live but no usable frame yet | slate "Starting cameraâ€¦" |
//! | live, new frame of the negotiated size | the frame (never scaled) |
//! | live, no new frame within 1.5 frame durations | the last frame again (counted in `repeated`) |
//!
//! One copy per frame: the pixels go from the slot of the shared file straight into the buffer a [`FrameSink`]
//! hands out (in the stream: the buffer of the sample that is about to be posted), and only then is the slot's
//! sequence number checked. A copy the check rejects is never delivered. The source keeps no frame of its own: a
//! repeat copies the slot of the last frame again (or, when the writer has reused it for something else of the
//! right size, that); when no slot holds a frame of the negotiated size any more it shows the "starting" slate.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use vcam_protocol::slate::{Slate, SlateKind};
use vcam_protocol::{
    is_stale, qpc_frequency, validate_header, Access, Disposition, Format, MappedFile, Read,
    ReadError, SlotData, ValidHeader, WriterState,
};

use crate::mediatype::{FrameFormat, PixelFormat};
use crate::vlog;

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
    /// QPC of the capture for a live frame, else 0.
    pub capture_qpc: u64,
    /// QPC when the decision was taken.
    pub at: u64,
}

impl Emit {
    /// QPC the sample is stamped with: the capture time of a live frame, else the time of the decision.
    pub fn sample_qpc(&self) -> u64 {
        if self.kind == Kind::Live {
            self.capture_qpc
        } else {
            self.at
        }
    }
}

/// When to call [`FrameSource::poll`] again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wait {
    /// Not before this QPC.
    Until(u64),
    /// In about a millisecond.
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
    /// The slot descriptor was stable but invalid.
    Invalid,
}

/// What a repeat found.
enum RepeatResult {
    /// The frame is in the sink.
    Copied,
    /// The copy was torn, or there was no buffer to copy into: try again at the next poll.
    Retry,
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
    last_emit: u64,

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
            last_emit: 0,
            slate_kind: None,
            slate_buf: Vec::new(),
            lat: Vec::with_capacity(LAT_RING),
            lat_next: 0,
            delivered: 0,
            repeated: 0,
            live_since_stats: 0,
            shown: Shown::Nothing,
        }
    }

    /// The stream started: count it as a consumer.
    pub fn start(&mut self) {
        if self.running {
            return;
        }
        self.running = true;
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
        self.publish_stats();
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
        self.header_ok = matches!(checked, Some(Ok(_)));
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

    fn leave_live(&mut self) {
        self.have_frame = false;
        self.last_id = None;
        self.live_since = 0;
        self.last_new_seen = 0;
        self.last_capture = None;
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
            return Poll::Wait(Wait::Soon);
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
            at: now,
        })
    }

    /// A live frame is on its way: the scaled slate is not needed until the next slate.
    fn drop_slate_cache(&mut self) {
        if self.slate_kind.take().is_some() {
            self.slate_buf = Vec::new();
        }
    }

    fn try_read(&mut self, v: &ValidHeader, now: u64, sink: &mut dyn FrameSink) -> ReadResult {
        let Some(map) = &self.map else {
            return ReadResult::Nothing;
        };
        let l = Layout::of(&self.fmt);
        let subtype = self.fmt.pixel.protocol_subtype();
        let last_id = self.last_id;
        let last_capture = self.last_capture;
        let min_gap = self.min_capture_gap();
        let freq = self.freq;
        let pid = v.writer_pid;
        // The slot is copied straight into the sink's buffer and the sequence check follows: a read it rejects
        // leaves a mixed frame in a buffer that no sample has been made of, and that the next copy overwrites.
        let r = map.shared().read_latest(v, |m, data| {
            if m.width as usize != l.w || m.height as usize != l.h || m.subtype != subtype {
                return Disposition::Skip;
            }
            if Some((m.index, pid, m.seq)) == last_id {
                return Disposition::Skip;
            }
            // a faster writer is decimated by capture time: skip a frame captured too soon after the last one
            if let Some(c) = last_capture {
                let cq = clamp_capture(m.capture_qpc, now, freq);
                if cq >= c && cq - c < min_gap {
                    return Disposition::Skip;
                }
            }
            let Some(mut dst) = dest(&mut *sink, &l) else {
                return Disposition::Skip;
            };
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

    /// Copy the last frame again: the slot of `last_id` while it still holds a frame of the negotiated size (the
    /// writer reuses a slot only for a newer frame, so that is then the newest we can show), else the newest slot.
    fn try_repeat(&mut self, v: &ValidHeader, sink: &mut dyn FrameSink) -> RepeatResult {
        let (Some(map), Some((slot, _, _))) = (&self.map, self.last_id) else {
            return RepeatResult::Lost;
        };
        let l = Layout::of(&self.fmt);
        let subtype = self.fmt.pixel.protocol_subtype();
        let pid = v.writer_pid;
        let no_buffer = std::cell::Cell::new(false);
        let mut copy = |m: &vcam_protocol::SlotMeta, data: &SlotData<'_>| {
            if m.width as usize != l.w || m.height as usize != l.h || m.subtype != subtype {
                return Disposition::Skip;
            }
            let Some(mut dst) = dest(&mut *sink, &l) else {
                no_buffer.set(true);
                return Disposition::Skip;
            };
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
        if matches!(r, Ok(Read::Skipped(_)) | Ok(Read::NoFrame)) && !no_buffer.get() {
            r = shared.read_latest(v, &mut copy);
        }
        match r {
            Ok(Read::Taken(m)) => {
                // what was shown is now what the slot holds: it is not a new frame when it comes round again
                self.last_id = Some((m.index, pid, m.seq));
                RepeatResult::Copied
            }
            // no sample to copy into right now: the same as a torn copy, ask again
            Ok(_) if no_buffer.get() => RepeatResult::Retry,
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
                self.not_running = kind == SlateKind::NotRunning;
                self.leave_live();
                return self.slate_poll(kind, now, sink);
            }
            Status::Live(v) => v,
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
        if now >= next_ok {
            match self.try_read(&v, now, &mut *sink) {
                ReadResult::New(capture_qpc) => {
                    self.have_frame = true;
                    self.last_capture = Some(capture_qpc);
                    self.last_new_seen = now;
                    self.drop_slate_cache();
                    self.note_shown(Shown::Live);
                    return Poll::Ready(Emit {
                        kind: Kind::Live,
                        capture_qpc,
                        at: now,
                    });
                }
                ReadResult::Invalid => {
                    self.not_running = true;
                    return self.slate_poll(SlateKind::NotRunning, now, sink);
                }
                ReadResult::Nothing => {}
            }
        }
        if self.have_frame {
            // live but late: the last frame again
            if now >= self.last_new_seen + period * 3 / 2 && now >= self.last_emit + period {
                match self.try_repeat(&v, &mut *sink) {
                    RepeatResult::Copied => {
                        self.drop_slate_cache();
                        return Poll::Ready(Emit {
                            kind: Kind::Repeat,
                            capture_qpc: 0,
                            at: now,
                        });
                    }
                    RepeatResult::Retry => {}
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
        Poll::Wait(if now >= next_ok {
            Wait::Soon
        } else {
            Wait::Until(next_ok)
        })
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
mod tests {
    use super::*;
    use std::time::{Duration, Instant};
    use vcam_protocol::pattern;
    use vcam_protocol::{qpc_now, SlotFormat, Writer, SUBTYPE_NV12, SUBTYPE_RGB32};

    fn temp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bettercam-frames-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
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
            Rig::with_pitch(path, fmt, pitch)
        }

        fn with_pitch(path: PathBuf, fmt: FrameFormat, pitch: usize) -> Rig {
            let rows = fmt.pixel.rows(fmt.height) as usize;
            Rig {
                src: FrameSource::new(path, fmt),
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
        set_test_path(Some(path.clone()));
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
        let mut src = Rig::new(path, nv12(640, 360, 30));
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
        let mut src = Rig::new(path, nv12(320, 180, 30));
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
        let mut src = Rig::new(path, nv12(fw, fh, 30));
        src.start();
        // live but no frame yet: after 1.5 frame durations the "starting" slate
        let e = next_ready(&mut src, Duration::from_secs(1));
        assert_eq!(e.kind, Kind::Slate(SlateKind::Starting));
        publish_pattern(&w, fw, fh, 5);
        let e = next_ready(&mut src, Duration::from_secs(1));
        assert_eq!(e.kind, Kind::Live);
        let a = pattern::analyse(
            |r| &src.data()[r as usize * fw as usize..(r as usize + 1) * fw as usize],
            SUBTYPE_NV12,
            fw,
            fh,
        );
        assert_eq!(a.slot(), Some(5 % pattern::slots(fw)));
        assert!(a.board_ok);
        // no new frame: repeat the last one after about 1.5 frame durations, counted
        let t0 = Instant::now();
        let e = next_ready(&mut src, Duration::from_secs(1));
        assert_eq!(e.kind, Kind::Repeat);
        assert!(
            t0.elapsed() >= Duration::from_millis(30),
            "repeated after only {:?}",
            t0.elapsed()
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
        let mut src = Rig::new(path, nv12(320, 180, 30));
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
        let mut src = Rig::new(path.clone(), nv12(320, 180, 30));
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
        let mut src = Rig::new(path, nv12(fw, fh, consumer_fps));
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
        let mut src = Rig::new(
            path,
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
        let mut src = Rig::new(path.to_path_buf(), nv12(320, 180, 30));
        src.start();
        let began = Instant::now();
        let mut kinds = Vec::new();
        while began.elapsed() < Duration::from_millis(ms) {
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
            drop(w); // the writer's own close writes to the header: take the snapshot after it
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
        let mut src = Rig::new(path, nv12(320, 180, 30));
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
        let direct = run_source_for(&real.join("frames.bin"), 100);
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
        let p = path.clone();
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
        publish_pattern(&w, 320, 180, 2);
        let kinds = run_source_for(&path, 150);
        assert!(kinds.contains(&Kind::Live), "{kinds:?}");
    }

    #[test]
    fn hostile_capture_times_are_clamped_and_never_break_the_stream() {
        let path = temp_path("captimes.bin");
        let _ = std::fs::remove_file(&path);
        let w = Writer::create(&path).unwrap();
        w.set_state(WriterState::Live);
        let mut src = Rig::new(path, nv12(320, 180, 30));
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
        let mut src = Rig::new(path, nv12(fw, fh, 30));
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
        let mut src = Rig::new(path, nv12(320, 180, 30));
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
            let mut src = Rig::with_pitch(path, fmt, rb + sink_pad);
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
            let mut src = Rig::with_pitch(path, fmt, pixel.stride(w) as usize + pad);
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
        let mut src = Rig::new(path, nv12(640, 360, 30));
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
        let mut src = Rig::new(path, nv12(fw, fh, 30));
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
        let mut src = Rig::new(path, nv12(fw, fh, 30));
        src.start();
        publish_pattern(&w, fw, fh, 5);
        assert_eq!(
            next_ready(&mut src, Duration::from_secs(1)).kind,
            Kind::Live
        );
        // the "writer" scribbles over the slot of the last frame while the repeat copies it, and finishes
        // another frame there (seq changes): the sequence check rejects every copy
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
        let began = Instant::now();
        while began.elapsed() < Duration::from_millis(300) {
            w.heartbeat();
            if let Poll::Ready(e) = src.poll(qpc_now()) {
                panic!("{:?} was delivered from a torn copy", e.kind);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        COPY_HOOK.with(|h| *h.borrow_mut() = None);
        // the slot now holds a settled frame (all 0xEE luma): it is shown whole
        w.heartbeat();
        let e = next_ready(&mut src, Duration::from_secs(1));
        assert!(matches!(e.kind, Kind::Repeat | Kind::Live), "{:?}", e.kind);
        let luma = &src.data()[..(fw * fh) as usize];
        assert!(luma.iter().all(|&b| b == 0xEE), "a mix of two frames");
    }

    #[test]
    fn a_sink_without_a_buffer_costs_no_frame() {
        let path = temp_path("no-buffer.bin");
        let _ = std::fs::remove_file(&path);
        let w = Writer::create(&path).unwrap();
        w.set_state(WriterState::Live);
        let (fw, fh) = (320u32, 180u32);
        let mut src = Rig::new(path, nv12(fw, fh, 30));
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
        let mut src = FrameSource::new(path, nv12(320, 180, 30));
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
}
