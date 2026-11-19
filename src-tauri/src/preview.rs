//! Live preview (plan 4.4): the engine's output frames, shrunk on the GPU, on their way to the webview.
//!
//! ```text
//! render thread: PreviewTap::on_frame   skip if busy | downscale (GPU) -> NV12 (one dispatch) -> staging ring
//!                PreviewTap::frame_done  (after the engine's own readback: the GPU is done) map + pack header + planes
//!                      |  offer
//!                      v
//!                    Gate  --- one frame in flight; `ack` releases the next; a lost ack times out after 1 s
//!                      |  take
//!                      v
//! sender thread: sink(bytes)  =  Channel::send(Raw) in the app (commands.rs), a collector in the tests
//! ```
//!
//! The engine never sees Tauri: [`Preview::start`] installs the tap with `Msg::SetTap`, and the app turns the frames
//! into channel messages through the [`Sink`] callback. The render thread only copies rows into a `Vec` and hands it
//! over; it never waits for the webview.
//!
//! Wire format of one frame (little endian): `width`, `height`, `stride`, `frame` number, `source_width`,
//! `source_height` (the size of the picture before the downscale: the output frame, or the picture at the preview
//! point, see [`crate::fx::PreviewPoint`]) and `format` as seven `u32`, then the pixels. The only format is
//! [`FORMAT_NV12`]: BT.709 limited range (the matrix the engine encodes for the virtual camera, see
//! `gpu::output::dispatch_packed_nv12`), 4:2:0 with each chroma sample the mean of its 2 x 2 pixels, top row first.
//! The payload is the Y plane, `height` rows of `stride` bytes (`width` of them are pixels), then the UV plane,
//! `height / 2` rows of `stride` bytes (`width / 2` interleaved `Cb Cr` pairs): `stride * height * 3 / 2` bytes.
//! While the preview point is Compare the `format` is [`FORMAT_NV12_PAIR`] and the payload is two such pictures of the
//! header's size, one after the other: the camera picture before the chain (fitted to the output size, as the output
//! stage does), then the output picture; the page draws both and splits them where the user put the bar. `width`
//! is a multiple of 4 and `height` is even (the encoder's block). `stride` equals `width` today; it is the pitch of both
//! planes, so the layout can change without a protocol change. The page uploads Y as an `R8` and UV as an `RG8`
//! texture and converts in its fragment shader (`src/ui/yuv.ts` holds the one set of constants).
//!
//! Set `BETTER_CAM_PREVIEW_LOG=1` to get one line per second on stderr with the frame counters.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::engine::{Engine, FrameTap, Msg};
use crate::gpu::downscale::{fit_within, Downscaler};
use crate::gpu::output::{
    dispatch_packed_nv12, nv12_cs_constants, packed_nv12_targets, EncodeTargets,
};
use crate::gpu::readback::{MappedPlane, ReadbackRing};
use crate::gpu::{ConstantBuffer, GpuResult, Renderer, RgbaTexture};

/// Largest preview picture: 960x540 (plan 4.4).
pub const MAX_SIZE: (u32, u32) = (960, 540);
/// Bytes in front of the pixels.
pub const HEADER_LEN: usize = 28;
/// The `format` word of the header: NV12, BT.709 limited range.
pub const FORMAT_NV12: u32 = 1;
/// The `format` word of a Compare frame ([`crate::fx::PreviewPoint::Compare`]): two [`FORMAT_NV12`] pictures of the
/// header's `width` x `height` one after the other, the camera picture before the chain first, then the output.
pub const FORMAT_NV12_PAIR: u32 = 2;
/// At most 30 frames per second.
pub const FRAME_INTERVAL: Duration = Duration::from_micros(33_333);
/// A frame this much before its slot still counts, so a camera at exactly 30 fps with timing jitter loses nothing.
const EARLY: Duration = Duration::from_millis(4);
/// How long the sender waits for an acknowledgement before it sends the next frame anyway.
pub const ACK_TIMEOUT: Duration = Duration::from_secs(1);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Delivers one packed frame to the webview. Returns false when it could not be delivered.
pub type Sink = Arc<dyn Fn(Vec<u8>) -> bool + Send + Sync>;

// ---------------------------------------------------------------------------------------------------------------
// Wire format

/// The 28 bytes in front of every frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub width: u32,
    pub height: u32,
    /// Bytes per row of both planes (the Y rows and the UV rows).
    pub stride: u32,
    /// Counts up per frame made; gaps mean frames that were skipped. What `preview_ack` carries.
    pub frame: u32,
    /// The size of the tapped picture before the downscale: what a point on the preview maps back to.
    pub source_width: u32,
    pub source_height: u32,
    /// How the pixels are stored: [`FORMAT_NV12`].
    pub format: u32,
}

impl Header {
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        for (i, v) in [
            self.width,
            self.height,
            self.stride,
            self.frame,
            self.source_width,
            self.source_height,
            self.format,
        ]
        .iter()
        .enumerate()
        {
            out[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
        }
        out
    }

    /// Read the header of a packed frame; `None` when it is shorter than a header.
    pub fn decode(bytes: &[u8]) -> Option<Header> {
        let word = |i: usize| Some(u32::from_le_bytes(bytes.get(i * 4..i * 4 + 4)?.try_into().ok()?));
        Some(Header {
            width: word(0)?,
            height: word(1)?,
            stride: word(2)?,
            frame: word(3)?,
            source_width: word(4)?,
            source_height: word(5)?,
            format: word(6)?,
        })
    }

    /// Bytes of pixels after the header: the Y plane and the half height UV plane, twice for [`FORMAT_NV12_PAIR`].
    pub fn payload_len(&self) -> usize {
        let one = self.stride as usize * self.height as usize * 3 / 2;
        if self.format == FORMAT_NV12_PAIR {
            one * 2
        } else {
            one
        }
    }
}

/// A packed frame: header and pixels in one buffer, ready to send.
pub struct Frame {
    pub number: u32,
    /// Capture time of the engine frame this picture was made from (not on the wire): tests check that the picture
    /// is the frame the engine just output, not the one before.
    pub capture_qpc: u64,
    pub bytes: Vec<u8>,
}

/// Header, the rows of the mapped Y plane and the rows of the mapped UV plane (padding between rows left out) of a
/// `width` x `height` frame. `source` is the size of the picture it was shrunk from.
fn pack(
    planes: &[MappedPlane<'_>],
    width: u32,
    height: u32,
    source: (u32, u32),
    number: u32,
    capture_qpc: u64,
) -> Frame {
    let stride = width;
    let header = Header {
        width,
        height,
        stride,
        frame: number,
        source_width: source.0,
        source_height: source.1,
        format: FORMAT_NV12,
    };
    let mut bytes = Vec::with_capacity(HEADER_LEN + header.payload_len());
    bytes.extend_from_slice(&header.encode());
    for y in 0..height as usize {
        bytes.extend_from_slice(planes[0].row(y));
    }
    for y in 0..height as usize / 2 {
        bytes.extend_from_slice(planes[1].row(y));
    }
    Frame {
        number,
        capture_qpc,
        bytes,
    }
}

/// The two halves of a Compare frame as one [`FORMAT_NV12_PAIR`] frame: the header of `after` with the pair format, the
/// pixels of `before`, then those of `after`. `None` when they are not two pictures of one size (then the output alone is
/// what is shown).
fn pair(before: &Frame, after: &Frame) -> Option<Frame> {
    let (hb, ha) = (Header::decode(&before.bytes)?, Header::decode(&after.bytes)?);
    let same = (hb.width, hb.height, hb.stride, hb.format) == (ha.width, ha.height, ha.stride, ha.format);
    if !same || ha.format != FORMAT_NV12 || before.bytes.len() != after.bytes.len() {
        return None;
    }
    let mut bytes = Vec::with_capacity(HEADER_LEN + 2 * ha.payload_len());
    bytes.extend_from_slice(&Header { format: FORMAT_NV12_PAIR, ..ha }.encode());
    bytes.extend_from_slice(&before.bytes[HEADER_LEN..]);
    bytes.extend_from_slice(&after.bytes[HEADER_LEN..]);
    Some(Frame {
        number: after.number,
        capture_qpc: after.capture_qpc,
        bytes,
    })
}

/// The preview picture size for a `source` picture and the window's request: [`fit_within`], then the width down to a
/// multiple of 4 and the height down to an even number (the NV12 encoder's block of 4 x 2 pixels). At most 3 pixels of
/// width and 1 of height smaller than the fit; the header's source size maps a point back either way.
pub fn fit_preview(source: (u32, u32), bound: (u32, u32)) -> (u32, u32) {
    let (w, h) = fit_within(source, bound);
    ((w & !3).max(4), (h & !1).max(2))
}

// ---------------------------------------------------------------------------------------------------------------
// Backpressure

/// One frame in flight. The tap offers frames; the sender takes one only when the previous one was acknowledged (or
/// [`ACK_TIMEOUT`] passed). Until then the newest offer waits in a single slot and every newer offer replaces it.
pub struct Gate {
    in_flight: Option<(u32, Instant)>,
    pending: Option<Frame>,
    timeout: Duration,
    closed: bool,
    /// Offers that were replaced before they were sent.
    pub overwritten: u64,
    /// Frames sent because the acknowledgement of the previous one never came.
    pub timeouts: u64,
    /// How long the last acknowledged frame took from the send to its ack: the IPC, the page's upload and draw.
    pub last_round_trip: Option<Duration>,
}

impl Gate {
    pub fn new(timeout: Duration) -> Gate {
        Gate {
            in_flight: None,
            pending: None,
            timeout,
            closed: false,
            overwritten: 0,
            timeouts: 0,
            last_round_trip: None,
        }
    }

    /// A frame waits for the one in flight to be acknowledged. The tap makes no further frame while this holds: it
    /// would only replace the one that waits.
    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Put `frame` in the pending slot, replacing what was there.
    pub fn offer(&mut self, frame: Frame) {
        if self.closed {
            return;
        }
        if self.pending.replace(frame).is_some() {
            self.overwritten += 1;
        }
    }

    /// The webview drew frame `number`. Returns false for a number that is not the one in flight (a late ack for a
    /// frame that already timed out, or one from an earlier stream).
    pub fn ack(&mut self, number: u32) -> bool {
        match self.in_flight {
            Some((n, sent)) if n == number => {
                self.last_round_trip = Some(sent.elapsed());
                self.in_flight = None;
                true
            }
            _ => false,
        }
    }

    /// The frame to send now, if one is pending and nothing is in flight (or the ack is overdue).
    pub fn take(&mut self, now: Instant) -> Option<Frame> {
        self.pending.as_ref()?;
        if let Some((_, sent)) = self.in_flight {
            if now < sent + self.timeout {
                return None;
            }
            self.timeouts += 1;
        }
        let frame = self.pending.take()?;
        self.in_flight = Some((frame.number, now));
        Some(frame)
    }

    /// Forget the frame in flight (it could not be delivered, so no ack will come).
    pub fn release(&mut self, number: u32) {
        self.ack(number);
    }

    /// How long until [`Gate::take`] could succeed without an ack or an offer; `None` when only an ack or an offer
    /// can change that.
    pub fn wait_hint(&self, now: Instant) -> Option<Duration> {
        let (_, sent) = self.in_flight?;
        self.pending.as_ref()?;
        Some((sent + self.timeout).saturating_duration_since(now))
    }

    /// Drop everything; later offers are ignored.
    pub fn close(&mut self) {
        self.closed = true;
        self.pending = None;
        self.in_flight = None;
    }
}

/// The 30 fps limit: lets a frame through when its slot on a 33.3 ms grid has come.
#[derive(Default)]
pub struct Pacer {
    due: Option<Instant>,
}

impl Pacer {
    /// Whether [`Pacer::accept`] would let a frame at `now` through.
    pub fn due(&self, now: Instant) -> bool {
        self.due.is_none_or(|due| now + EARLY >= due)
    }

    pub fn accept(&mut self, now: Instant) -> bool {
        let Some(due) = self.due else {
            self.due = Some(now + FRAME_INTERVAL);
            return true;
        };
        if now + EARLY < due {
            return false;
        }
        // Frames that keep arriving on time stay on the grid; after a gap the grid restarts from this frame.
        self.due = Some(if now > due + FRAME_INTERVAL {
            now + FRAME_INTERVAL
        } else {
            due + FRAME_INTERVAL
        });
        true
    }

    pub fn reset(&mut self) {
        self.due = None;
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Sender

/// What the render thread, the sender thread and the commands share.
struct Shared {
    gate: Mutex<Gate>,
    wake: Condvar,
    /// Frames the tap packed.
    made: AtomicU64,
    /// Frames the tap did not make because one was already waiting for the webview or still on the GPU.
    skipped: AtomicU64,
    /// Timing samples of the last second, kept only while `BETTER_CAM_PREVIEW_LOG` is set (see [`Probe`]).
    probe: Option<Mutex<Probe>>,
}

/// What the log line of `BETTER_CAM_PREVIEW_LOG` says about the last second besides the counters, in ms: the age of a
/// frame when it was handed to the sink (from the camera's capture time: the pipeline and the wait for the page) and
/// the round trip of a frame (sink to acknowledgement: IPC, upload and draw in the page).
#[derive(Default)]
struct Probe {
    age: Vec<f64>,
    round_trip: Vec<f64>,
}

impl Probe {
    fn take(&mut self) -> String {
        fn pct(v: &mut Vec<f64>) -> String {
            if v.is_empty() {
                return "-".into();
            }
            v.sort_by(|a, b| a.total_cmp(b));
            let s = format!("{:.1}/{:.1}", v[v.len() / 2], v[v.len() - 1]);
            v.clear();
            s
        }
        format!(
            "age_ms p50/max={} round_trip_ms p50/max={}",
            pct(&mut self.age),
            pct(&mut self.round_trip)
        )
    }
}

/// The preview's counters, for the log line and the checks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PreviewStats {
    /// Frames packed.
    pub made: u64,
    /// Frames delivered to the sink (since the process started).
    pub sent: u64,
    /// Frames not made because one waited in the gate or was still on the GPU (the 30 fps limit is not counted).
    pub skipped: u64,
    /// Offers that replaced a waiting frame (the tap does not offer while one waits, so this stays 0 in the app).
    pub overwritten: u64,
    /// Frames sent because the acknowledgement of the one before never came.
    pub ack_timeouts: u64,
}

impl Shared {
    fn new(timeout: Duration) -> Arc<Shared> {
        Arc::new(Shared {
            gate: Mutex::new(Gate::new(timeout)),
            wake: Condvar::new(),
            made: AtomicU64::new(0),
            skipped: AtomicU64::new(0),
            probe: std::env::var_os("BETTER_CAM_PREVIEW_LOG").map(|_| Mutex::new(Probe::default())),
        })
    }

    fn stats(&self, sent: u64) -> PreviewStats {
        let g = lock(&self.gate);
        PreviewStats {
            made: self.made.load(Ordering::Relaxed),
            sent,
            skipped: self.skipped.load(Ordering::Relaxed),
            overwritten: g.overwritten,
            ack_timeouts: g.timeouts,
        }
    }

    fn offer(&self, frame: Frame) {
        self.made.fetch_add(1, Ordering::Relaxed);
        lock(&self.gate).offer(frame);
        self.wake.notify_one();
    }

    fn ack(&self, number: u32) {
        let mut g = lock(&self.gate);
        if g.ack(number) {
            if let (Some(p), Some(rtt)) = (&self.probe, g.last_round_trip) {
                lock(p).round_trip.push(rtt.as_secs_f64() * 1000.0);
            }
            drop(g);
            self.wake.notify_one();
        }
    }

    fn close(&self) {
        lock(&self.gate).close();
        self.wake.notify_all();
    }

    fn is_closed(&self) -> bool {
        lock(&self.gate).closed
    }
}

/// Failed deliveries in a row after which the sender gives up (the webview is gone).
const MAX_SEND_FAILURES: u32 = 30;

/// The sender thread: waits for a frame the gate lets through, hands it to the sink, counts it.
fn spawn_sender(shared: Arc<Shared>, sink: Sink, sent: Arc<AtomicU64>) -> std::io::Result<JoinHandle<()>> {
    let log = std::env::var_os("BETTER_CAM_PREVIEW_LOG").is_some();
    std::thread::Builder::new()
        .name("preview-sender".into())
        .spawn(move || {
            let mut failures = 0u32;
            let mut last_log = Instant::now();
            loop {
                let frame = {
                    let mut g = lock(&shared.gate);
                    loop {
                        if g.closed {
                            return;
                        }
                        let now = Instant::now();
                        if let Some(f) = g.take(now) {
                            break f;
                        }
                        // Wake at least once a second so the log line keeps coming while nothing flows.
                        let wait = g.wait_hint(now).map_or(Duration::from_secs(1), |d| d.min(Duration::from_secs(1)));
                        g = shared
                            .wake
                            .wait_timeout(g, wait)
                            .unwrap_or_else(|e| e.into_inner())
                            .0;
                        if log && last_log.elapsed() >= Duration::from_secs(1) {
                            last_log = Instant::now();
                            log_counters(&shared, &g, sent.load(Ordering::Relaxed));
                        }
                    }
                };
                let number = frame.number;
                let capture_qpc = frame.capture_qpc;
                if sink(frame.bytes) {
                    failures = 0;
                    sent.fetch_add(1, Ordering::Relaxed);
                    if let (Some(p), true) = (&shared.probe, capture_qpc != 0) {
                        let age = vcam_protocol::qpc_now().saturating_sub(capture_qpc) as f64
                            * 1000.0
                            / vcam_protocol::qpc_frequency() as f64;
                        lock(p).age.push(age);
                    }
                } else {
                    // No ack will come for a frame that was not delivered.
                    lock(&shared.gate).release(number);
                    failures += 1;
                    if failures >= MAX_SEND_FAILURES {
                        eprintln!("[preview] the webview does not take frames; the preview stops");
                        shared.close();
                        return;
                    }
                }
                if log && last_log.elapsed() >= Duration::from_secs(1) {
                    last_log = Instant::now();
                    log_counters(&shared, &lock(&shared.gate), sent.load(Ordering::Relaxed));
                }
            }
        })
}

fn log_counters(shared: &Shared, gate: &Gate, sent: u64) {
    let probe = shared
        .probe
        .as_ref()
        .map(|p| lock(p).take())
        .unwrap_or_default();
    eprintln!(
        "[preview] made={} sent={} skipped={} overwritten={} ack_timeouts={} {probe}",
        shared.made.load(Ordering::Relaxed),
        sent,
        shared.skipped.load(Ordering::Relaxed),
        gate.overwritten,
        gate.timeouts
    );
}

// ---------------------------------------------------------------------------------------------------------------
// Tap

/// What travels with a frame through the staging ring: its number and the capture time of the engine frame.
type Tag = (u32, u64);

/// The staging ring of the tap with the NV12 planes the dispatch writes into it, the picture size they were made for
/// and the size of the source picture that is shrunk into them.
struct Staging {
    ring: ReadbackRing<Tag>,
    targets: EncodeTargets,
    size: (u32, u32),
    source: (u32, u32),
}

/// The engine's frame tap: runs on the render thread with the picture the preview shows (the output frame, or the
/// picture at the preview point).
struct PreviewTap {
    shared: Arc<Shared>,
    numbers: Arc<AtomicU32>,
    hidden: Arc<AtomicBool>,
    /// What the window asked for, in pixels (already limited to [`MAX_SIZE`] when used).
    request: (u32, u32),
    down: Downscaler,
    /// The staging ring, the picture size it was made for and the size of the source picture that is shrunk into it.
    /// A change of either starts a new ring: the pictures of the old one would say the wrong source size.
    ring: Option<Staging>,
    /// Compare only: the staging ring of the camera picture (`ring` has the output). `None` in every other mode.
    before: Option<Staging>,
    /// Compare only: the pictures that were mapped and wait for the other half of their frame (matched by number).
    held: Held,
    /// The constants of the NV12 dispatch, made on first use.
    cs_constants: Option<ConstantBuffer>,
    pacer: Pacer,
    complained: bool,
}

/// The mapped halves of a Compare frame that wait for each other.
#[derive(Default)]
struct Held {
    before: Option<Frame>,
    after: Option<Frame>,
}

/// Downscale `frame` into a small texture, write it as NV12 into `slot`'s targets (made, or made again when the size
/// of the picture or of its source changed) and queue the copy of the planes to the CPU. Never waits for the GPU.
#[allow(clippy::too_many_arguments)]
fn shrink_into(
    slot: &mut Option<Staging>,
    down: &mut Downscaler,
    cs_constants: &ConstantBuffer,
    r: &Renderer,
    frame: &RgbaTexture,
    bound: (u32, u32),
    tag: Tag,
) -> GpuResult<()> {
    let size = fit_preview((frame.width, frame.height), bound);
    let source = (frame.width, frame.height);
    if slot
        .as_ref()
        .is_none_or(|s| s.size != size || s.source != source)
    {
        let targets = packed_nv12_targets(&r.gpu, size.0, size.1)?;
        *slot = Some(Staging {
            ring: ReadbackRing::new(r.gpu.clone(), &targets.plane_descs())?,
            targets,
            size,
            source,
        });
    }
    // `frame.srv` (UNORM): the codes as they are stored, which is how the output stage reads them too, so the
    // colours of a picture tapped inside the chain match the ones of the output.
    let small = down.run(r, frame, size.0, size.1)?;
    let staging = slot.as_mut().expect("made above");
    dispatch_packed_nv12(r, cs_constants, &small, &staging.targets)?;
    // The tap makes no frame while one is on the GPU, so the ring never overwrites here.
    staging.ring.submit(&staging.targets.textures(), tag)?;
    // `small` goes back to the pool; the dispatch above was queued first, so the GPU reads it before any reuse.
    Ok(())
}

/// The newest picture of `slot`'s ring that the GPU has finished (all of them with `wait`), packed. More than one is
/// pending only after a stall; the older ones would be overwritten anyway.
fn newest_of(slot: &mut Option<Staging>, wait: bool) -> Result<Option<Frame>, String> {
    let Some(Staging {
        ring,
        size: (w, h),
        source,
        ..
    }) = slot.as_mut()
    else {
        return Ok(None);
    };
    let (w, h, source) = (*w, *h, *source);
    let mut newest = None;
    for _ in 0..ReadbackRing::<Tag>::SLOTS {
        match ring.poll(wait, |planes, (number, qpc)| pack(planes, w, h, source, number, qpc)) {
            Ok(Some(f)) => newest = Some(f),
            Ok(None) => break,
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(newest)
}

impl PreviewTap {
    fn new(shared: Arc<Shared>, numbers: Arc<AtomicU32>, hidden: Arc<AtomicBool>, request: (u32, u32)) -> PreviewTap {
        PreviewTap {
            shared,
            numbers,
            hidden,
            request,
            down: Downscaler::new(),
            ring: None,
            before: None,
            held: Held::default(),
            cs_constants: None,
            pacer: Pacer::default(),
            complained: false,
        }
    }

    /// Whether a frame made now would have nowhere to go: one waits in the gate for the webview's acknowledgement
    /// (a newer one would only replace it), or one is still on the GPU (the ring is not mapped yet).
    fn busy(&self) -> bool {
        lock(&self.shared.gate).has_pending()
            || self.ring.as_ref().is_some_and(|s| s.ring.pending() > 0)
            || self.before.as_ref().is_some_and(|s| s.ring.pending() > 0)
    }

    /// What the window asked for, limited to [`MAX_SIZE`]: the bound of one picture.
    fn bound(&self) -> (u32, u32) {
        (
            self.request.0.clamp(1, MAX_SIZE.0),
            self.request.1.clamp(1, MAX_SIZE.1),
        )
    }

    /// Downscale `frame` into a small texture, write it as NV12 and queue the copy of the planes to the CPU. Never
    /// waits for the GPU. `capture_qpc` is the capture time of the engine frame.
    fn capture(&mut self, r: &Renderer, frame: &RgbaTexture, capture_qpc: u64) -> GpuResult<()> {
        // One picture: whatever the Compare mode had is of no use now.
        self.before = None;
        self.held = Held::default();
        if self.cs_constants.is_none() {
            self.cs_constants = Some(nv12_cs_constants(r)?);
        }
        let number = self.numbers.fetch_add(1, Ordering::Relaxed);
        let bound = self.bound();
        shrink_into(
            &mut self.ring,
            &mut self.down,
            self.cs_constants.as_ref().expect("made above"),
            r,
            frame,
            bound,
            (number, capture_qpc),
        )
    }

    /// [`PreviewTap::capture`] for Compare: both pictures, with one frame number, each in its own ring; `collect` joins
    /// them into one [`FORMAT_NV12_PAIR`] frame.
    fn capture_pair(&mut self, r: &Renderer, before: &RgbaTexture, after: &RgbaTexture, capture_qpc: u64) -> GpuResult<()> {
        if self.cs_constants.is_none() {
            self.cs_constants = Some(nv12_cs_constants(r)?);
        }
        let number = self.numbers.fetch_add(1, Ordering::Relaxed);
        let bound = self.bound();
        let cs = self.cs_constants.as_ref().expect("made above");
        shrink_into(&mut self.before, &mut self.down, cs, r, before, bound, (number, capture_qpc))?;
        shrink_into(&mut self.ring, &mut self.down, cs, r, after, bound, (number, capture_qpc))
    }

    /// Map the frames on the GPU and offer the newest. With `wait` the call blocks until the GPU has them (right
    /// after the engine's own readback, which waited for the same work, that is no wait at all); without it a frame
    /// the GPU has not finished stays for the next call.
    fn collect(&mut self, wait: bool) {
        let after = newest_of(&mut self.ring, wait);
        if self.before.is_none() {
            match after {
                Ok(Some(f)) => self.shared.offer(f),
                Ok(None) => {}
                Err(e) => self.complain(&e),
            }
            return;
        }
        // Compare: a picture waits for its other half; they are the same frame when their numbers are.
        let before = newest_of(&mut self.before, wait);
        for half in [(before, true), (after, false)] {
            match half {
                (Ok(Some(f)), is_before) => {
                    if is_before {
                        self.held.before = Some(f);
                    } else {
                        self.held.after = Some(f);
                    }
                }
                (Ok(None), _) => {}
                (Err(e), _) => self.complain(&e),
            }
        }
        let numbers = (self.held.before.as_ref().map(|f| f.number), self.held.after.as_ref().map(|f| f.number));
        match numbers {
            (Some(b), Some(a)) if b == a => {
                let (before, after) = (self.held.before.take().expect("seen"), self.held.after.take().expect("seen"));
                // Two pictures that do not fit together (cannot happen: both are the output's size): the output alone.
                self.shared.offer(pair(&before, &after).unwrap_or(after));
            }
            // One half is from an older frame (a stall, or the frame before the mode changed): it never gets its partner.
            (Some(b), Some(a)) if b < a => self.held.before = None,
            (Some(_), Some(_)) => self.held.after = None,
            _ => {}
        }
    }

    /// Whether this frame makes no picture: nobody sees it, the 30 fps slot has not come, or the last one is not out yet.
    /// Does not take the slot (see [`PreviewTap::skip_frame`]).
    fn would_skip(&mut self, now: Instant) -> bool {
        if self.hidden.load(Ordering::Relaxed) || self.shared.is_closed() {
            // Nobody sees the picture: no GPU work, and the frames in the ring would be stale on return.
            for s in [self.ring.as_mut(), self.before.as_mut()].into_iter().flatten() {
                s.ring.clear();
            }
            self.held = Held::default();
            self.pacer.reset();
            return true;
        }
        if !self.pacer.due(now) {
            return true;
        }
        if self.busy() {
            // Not a frame the 30 fps limit would have dropped: counted, and the slot stays free for the next one.
            self.shared.skipped.fetch_add(1, Ordering::Relaxed);
            return true;
        }
        false
    }

    /// [`PreviewTap::would_skip`], and takes the slot when it does not skip.
    fn skip_frame(&mut self) -> bool {
        let now = Instant::now();
        if self.would_skip(now) {
            return true;
        }
        self.pacer.accept(now);
        false
    }

    fn complain(&mut self, what: &str) {
        if !self.complained {
            self.complained = true;
            eprintln!("[preview] {what}");
        }
    }
}

impl FrameTap for PreviewTap {
    /// The engine rebuilt its GPU device: the staging ring and the downscale constants belong to the old one.
    fn device_changed(&mut self) {
        self.ring = None;
        self.before = None;
        self.held = Held::default();
        self.cs_constants = None;
        self.down = Downscaler::new();
        self.pacer.reset();
    }

    fn on_frame(&mut self, renderer: &Renderer, frame: &RgbaTexture, capture_qpc: u64) {
        if self.skip_frame() {
            return;
        }
        if let Err(e) = self.capture(renderer, frame, capture_qpc) {
            self.complain(&e.to_string());
        }
    }

    /// The engine asks before it fits the camera picture: a frame this tap would skip costs no GPU pass. The slot is
    /// taken by the `on_compare` (or `on_frame`) that follows.
    fn wants_compare(&mut self) -> bool {
        !self.would_skip(Instant::now())
    }

    fn on_compare(&mut self, renderer: &Renderer, before: &RgbaTexture, after: &RgbaTexture, capture_qpc: u64) {
        if self.skip_frame() {
            return;
        }
        if let Err(e) = self.capture_pair(renderer, before, after, capture_qpc) {
            self.complain(&e.to_string());
        }
    }

    /// After the engine's readback of the same frame: map the picture `on_frame` queued, so the page gets the frame the
    /// apps get and not the one before it.
    fn frame_done(&mut self, wait: bool) {
        if self.hidden.load(Ordering::Relaxed) || self.shared.is_closed() {
            return;
        }
        self.collect(wait);
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Controller

struct Running {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

/// Starts and stops the preview and carries the acknowledgements. One lives in [`crate::api::Service`].
#[derive(Default)]
pub struct Preview {
    run: Mutex<Option<Running>>,
    /// Frame numbers, kept across restarts so an ack from an earlier stream never matches a new frame.
    numbers: Arc<AtomicU32>,
    /// The app window is hidden or minimized: no frames are made, whatever the page says.
    hidden: Arc<AtomicBool>,
    /// Frames delivered to the sink since the process started.
    sent: Arc<AtomicU64>,
}

impl Preview {
    pub fn new() -> Preview {
        Preview::default()
    }

    /// Start (or restart) the preview: frames of at most `request` pixels (and [`MAX_SIZE`]) go to `sink`.
    pub fn start(&self, engine: &Engine, request: (u32, u32), sink: Sink) -> Result<(), String> {
        let mut run = lock(&self.run);
        Self::end(&mut run);
        let shared = Shared::new(ACK_TIMEOUT);
        let thread = spawn_sender(shared.clone(), sink, self.sent.clone()).map_err(|e| e.to_string())?;
        let tap = PreviewTap::new(shared.clone(), self.numbers.clone(), self.hidden.clone(), request);
        engine.send(Msg::SetTap(Some(Box::new(tap))));
        *run = Some(Running {
            shared,
            thread: Some(thread),
        });
        Ok(())
    }

    /// Stop the frames and drop the tap. `engine` is `None` when it does not exist (nothing to unhook).
    pub fn stop(&self, engine: Option<&Engine>) {
        let mut run = lock(&self.run);
        if run.is_some() {
            if let Some(e) = engine {
                e.send(Msg::SetTap(None));
            }
        }
        Self::end(&mut run);
    }

    fn end(run: &mut Option<Running>) {
        if let Some(mut r) = run.take() {
            r.shared.close();
            if let Some(t) = r.thread.take() {
                let _ = t.join();
            }
        }
    }

    /// `preview_ack(frame)`.
    pub fn ack(&self, frame: u32) {
        if let Some(r) = lock(&self.run).as_ref() {
            r.shared.ack(frame);
        }
    }

    /// The window cannot be seen (hidden to the tray, minimized): the tap stops making frames.
    pub fn set_hidden(&self, hidden: bool) {
        self.hidden.store(hidden, Ordering::Relaxed);
    }

    /// Frames delivered to the sink since the process started (the counter the checks read).
    pub fn frames_sent(&self) -> u64 {
        self.sent.load(Ordering::Relaxed)
    }

    /// The counters of the running stream (`made`, `skipped`, ... restart with each [`Preview::start`]; `sent` is the
    /// total since the process started). All zero but `sent` while no stream runs.
    pub fn stats(&self) -> PreviewStats {
        match lock(&self.run).as_ref() {
            Some(r) => r.shared.stats(self.frames_sent()),
            None => PreviewStats {
                sent: self.frames_sent(),
                ..PreviewStats::default()
            },
        }
    }
}

impl Drop for Preview {
    fn drop(&mut self) {
        Self::end(&mut lock(&self.run));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gpu::convert::{yuv_to_rgb_rows, ColorRange, ColorSpace};
    use crate::gpu::{AdapterKind, GpuDevice};

    fn frame(n: u32) -> Frame {
        Frame {
            number: n,
            capture_qpc: 0,
            bytes: vec![n as u8; 4],
        }
    }

    // --- wire format ---------------------------------------------------------------------------------------

    #[test]
    fn the_header_is_seven_little_endian_words() {
        let h = Header {
            width: 960,
            height: 540,
            stride: 960,
            frame: 0x0102_0304,
            source_width: 1920,
            source_height: 1080,
            format: FORMAT_NV12,
        };
        let b = h.encode();
        assert_eq!(b.len(), 28);
        assert_eq!(HEADER_LEN, 28);
        assert_eq!(&b[0..4], &[0xC0, 0x03, 0, 0]);
        assert_eq!(&b[4..8], &[0x1C, 0x02, 0, 0]);
        assert_eq!(&b[8..12], &[0xC0, 0x03, 0, 0]);
        assert_eq!(&b[12..16], &[4, 3, 2, 1]);
        assert_eq!(&b[16..20], &[0x80, 0x07, 0, 0]);
        assert_eq!(&b[20..24], &[0x38, 0x04, 0, 0]);
        assert_eq!(
            &b[24..28],
            &[1, 0, 0, 0],
            "the format word: 1 is NV12 (BT.709 limited)"
        );
        assert_eq!(Header::decode(&b), Some(h));
        assert_eq!(Header::decode(&b[..27]), None);
        assert_eq!(Header::decode(&[]), None);
        // the Y plane and the half height UV plane
        assert_eq!(h.payload_len(), 960 * 540 * 3 / 2);
    }

    #[test]
    fn pack_writes_the_y_rows_then_the_uv_rows_without_the_padding() {
        // 4 x 2 pixels: the Y plane is 2 rows of 4 bytes 12 bytes apart in the mapped memory, the UV plane is 1 row of
        // 4 bytes (2 pairs).
        let (w, h) = (4u32, 2u32);
        let mut y_mem = vec![0xEEu8; 12 + 4];
        let mut uv_mem = vec![0xEEu8; 4];
        for y in 0..2usize {
            for i in 0..4 {
                y_mem[y * 12 + i] = (y * 100 + i) as u8;
            }
        }
        uv_mem.copy_from_slice(&[200, 201, 202, 203]);
        let planes = [
            MappedPlane::new(&y_mem, 12, 4, 2),
            MappedPlane::new(&uv_mem, 9, 4, 1),
        ];
        let f = pack(&planes, w, h, (30, 20), 7, 99);
        assert_eq!((f.number, f.capture_qpc), (7, 99));
        assert_eq!(f.bytes.len(), HEADER_LEN + 12);
        let header = Header::decode(&f.bytes).unwrap();
        assert_eq!(
            header,
            Header {
                width: 4,
                height: 2,
                stride: 4,
                frame: 7,
                source_width: 30,
                source_height: 20,
                format: FORMAT_NV12,
            }
        );
        assert_eq!(header.payload_len(), 12);
        assert_eq!(
            &f.bytes[HEADER_LEN..],
            &[0, 1, 2, 3, 100, 101, 102, 103, 200, 201, 202, 203]
        );
    }

    /// The page converts with the rows of `src/ui/yuv.ts` (pinned in `eyedropper.test.ts` to these same numbers): they
    /// must be the inverse of what the encoder writes, BT.709 limited range.
    #[test]
    fn the_page_matrix_is_the_inverse_of_the_encoders() {
        let rows = yuv_to_rgb_rows(ColorSpace::Bt709, ColorRange::Limited, 8);
        let page = [
            [1.164384, 0.0, 1.792741, -0.972945],
            [1.164384, -0.213249, -0.532909, 0.301483],
            [1.164384, 2.112402, 0.0, -1.133402],
        ];
        for (row, want) in rows.iter().zip(page) {
            for (v, w) in row.iter().zip(want) {
                assert!((v - w).abs() < 1e-5, "{rows:?} vs {page:?}");
            }
        }
        // and the encoder's rows send a colour to samples that the page matrix sends back
        let enc = crate::gpu::convert::rgb_to_yuv_rows(ColorSpace::Bt709, ColorRange::Limited);
        for rgb in [
            [0.0, 0.0, 0.0],
            [1.0, 1.0, 1.0],
            [1.0, 0.0, 0.0],
            [0.2, 0.7, 0.4],
            [0.05, 0.1, 0.95],
        ] {
            let s = enc.map(|e| e[0] * rgb[0] + e[1] * rgb[1] + e[2] * rgb[2] + e[3]);
            for c in 0..3 {
                let back = rows[c][0] * s[0] + rows[c][1] * s[1] + rows[c][2] * s[2] + rows[c][3];
                assert!((back - rgb[c]).abs() < 1e-4, "{rgb:?} channel {c}: {back}");
            }
        }
    }

    #[test]
    fn the_preview_size_is_a_block_of_4_by_2_and_stays_inside_the_request() {
        // 16:9 and the cap
        assert_eq!(fit_preview((1920, 1080), (960, 540)), (960, 540));
        assert_eq!(fit_preview((3840, 2160), (960, 540)), (960, 540));
        // 4:3
        assert_eq!(fit_preview((1920, 1440), (960, 540)), (720, 540));
        // an odd height and a width that is not a multiple of 4 round down
        assert_eq!(fit_preview((1920, 1080), (560, 315)), (560, 314));
        assert_eq!(fit_preview((1920, 1080), (555, 311)), (552, 310));
        // never upscaled
        assert_eq!(fit_preview((642, 361), (960, 540)), (640, 360));
        for (src, bound) in [
            ((1920, 1080), (777, 433)),
            ((1280, 720), (601, 599)),
            ((640, 480), (333, 333)),
            ((1000, 1000), (960, 540)),
            ((7, 5), (960, 540)),
        ] {
            let (w, h) = fit_preview(src, bound);
            assert!(
                w % 4 == 0 && h % 2 == 0 && w >= 4 && h >= 2,
                "{src:?} in {bound:?} gave {w}x{h}"
            );
            assert!(
                w <= bound.0.max(4) && h <= bound.1.max(2),
                "{src:?} in {bound:?} gave {w}x{h}"
            );
        }
    }

    // --- backpressure --------------------------------------------------------------------------------------

    #[test]
    fn frames_overwrite_while_unacknowledged_and_an_ack_releases_the_next() {
        let t0 = Instant::now();
        let mut g = Gate::new(Duration::from_secs(1));
        assert!(g.take(t0).is_none(), "nothing offered yet");
        g.offer(frame(1));
        assert_eq!(g.take(t0).map(|f| f.number), Some(1));
        // Frame 1 is in flight: 2, 3 and 4 pile up in one slot, only the newest survives.
        g.offer(frame(2));
        g.offer(frame(3));
        g.offer(frame(4));
        assert_eq!(g.overwritten, 2);
        assert!(g.take(t0 + Duration::from_millis(100)).is_none());
        // An ack for another frame changes nothing.
        assert!(!g.ack(3));
        assert!(g.take(t0 + Duration::from_millis(200)).is_none());
        assert!(g.ack(1));
        assert!(!g.ack(1), "a second ack of the same frame is ignored");
        assert_eq!(g.take(t0 + Duration::from_millis(300)).map(|f| f.number), Some(4));
        assert!(g.take(t0 + Duration::from_millis(300)).is_none(), "the slot is empty");
        assert_eq!(g.timeouts, 0);
    }

    #[test]
    fn a_lost_ack_times_out_and_a_late_ack_is_ignored() {
        let t0 = Instant::now();
        let mut g = Gate::new(Duration::from_secs(1));
        g.offer(frame(1));
        assert_eq!(g.take(t0).map(|f| f.number), Some(1));
        g.offer(frame(2));
        // Just before the timeout nothing moves, and the hint says how long to wait.
        let almost = t0 + Duration::from_millis(999);
        assert!(g.take(almost).is_none());
        assert_eq!(g.wait_hint(almost), Some(Duration::from_millis(1)));
        // At the timeout the frame goes out although frame 1 was never acknowledged.
        assert_eq!(g.take(t0 + Duration::from_secs(1)).map(|f| f.number), Some(2));
        assert_eq!(g.timeouts, 1);
        // The ack of frame 1 arrives late: it must not release frame 2's slot.
        assert!(!g.ack(1));
        g.offer(frame(3));
        assert!(g.take(t0 + Duration::from_millis(1500)).is_none());
        assert!(g.ack(2));
        assert_eq!(g.take(t0 + Duration::from_millis(1500)).map(|f| f.number), Some(3));
    }

    #[test]
    fn the_wait_hint_is_none_when_only_an_ack_or_an_offer_can_help() {
        let t0 = Instant::now();
        let mut g = Gate::new(Duration::from_secs(1));
        assert_eq!(g.wait_hint(t0), None, "idle");
        g.offer(frame(1));
        assert_eq!(g.wait_hint(t0), None, "pending but free to send");
        g.take(t0);
        assert_eq!(g.wait_hint(t0), None, "in flight, nothing pending");
        g.offer(frame(2));
        assert_eq!(g.wait_hint(t0), Some(Duration::from_secs(1)));
    }

    #[test]
    fn a_closed_gate_takes_nothing() {
        let t0 = Instant::now();
        let mut g = Gate::new(Duration::from_secs(1));
        g.offer(frame(1));
        g.close();
        g.offer(frame(2));
        assert!(g.take(t0).is_none());
    }

    #[test]
    fn the_pacer_caps_at_thirty_per_second_and_loses_nothing_below_it() {
        let t0 = Instant::now();
        let at = |ms: f64| t0 + Duration::from_secs_f64(ms / 1000.0);
        // A 60 fps camera: every other frame.
        let mut p = Pacer::default();
        let accepted = (0..60).filter(|i| p.accept(at(*i as f64 * 1000.0 / 60.0))).count();
        assert_eq!(accepted, 30);
        // A 30 fps camera with up to 3 ms of jitter: all of them.
        let mut p = Pacer::default();
        let jitter = [0.0, 3.0, -3.0, 2.0, -2.0, 3.0];
        let accepted = (0..90)
            .filter(|i| p.accept(at(*i as f64 * 1000.0 / 30.0 + jitter[*i as usize % 6])))
            .count();
        assert_eq!(accepted, 90);
        // A 16 fps camera: all of them. After a pause the grid restarts.
        let mut p = Pacer::default();
        assert!((0..16).all(|i| p.accept(at(i as f64 * 62.5))));
        assert!(p.accept(at(5000.0)));
        assert!(!p.accept(at(5010.0)));
        assert!(p.accept(at(5033.0)));
    }

    #[test]
    fn the_sender_keeps_one_frame_in_flight_and_recovers_from_a_lost_ack() {
        let shared = Shared::new(Duration::from_millis(300));
        let got = Arc::new(Mutex::new(Vec::<u32>::new()));
        let sink: Sink = {
            let got = got.clone();
            Arc::new(move |bytes: Vec<u8>| {
                lock(&got).push(Header::decode(&bytes).unwrap().frame);
                true
            })
        };
        let sent = Arc::new(AtomicU64::new(0));
        let thread = spawn_sender(shared.clone(), sink, sent.clone()).unwrap();
        let wait_for = |n: usize| {
            let end = Instant::now() + Duration::from_secs(5);
            while lock(&got).len() < n && Instant::now() < end {
                std::thread::sleep(Duration::from_millis(5));
            }
            lock(&got).clone()
        };
        let packed = |n: u32| Frame {
            number: n,
            capture_qpc: 0,
            bytes: Header {
                width: 4,
                height: 0,
                stride: 4,
                frame: n,
                source_width: 4,
                source_height: 0,
                format: FORMAT_NV12,
            }
            .encode()
            .to_vec(),
        };

        shared.offer(packed(1));
        assert_eq!(wait_for(1), vec![1]);
        // 2..5 arrive while 1 is unacknowledged: nothing is sent for 150 ms.
        for n in 2..6 {
            shared.offer(packed(n));
        }
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(*lock(&got), vec![1]);
        // The ack releases the newest one only.
        shared.ack(1);
        assert_eq!(wait_for(2), vec![1, 5]);
        // Frame 5 is never acknowledged: after the timeout the next offer goes out by itself.
        shared.offer(packed(6));
        assert_eq!(wait_for(3), vec![1, 5, 6]);
        assert_eq!(lock(&shared.gate).timeouts, 1);
        assert_eq!(sent.load(Ordering::Relaxed), 3);

        shared.close();
        thread.join().unwrap();
    }

    #[test]
    fn a_failed_delivery_does_not_block_the_next_frame() {
        let shared = Shared::new(Duration::from_secs(60));
        let calls = Arc::new(AtomicU32::new(0));
        let sink: Sink = {
            let calls = calls.clone();
            Arc::new(move |_| calls.fetch_add(1, Ordering::SeqCst) >= 1)
        };
        let sent = Arc::new(AtomicU64::new(0));
        let thread = spawn_sender(shared.clone(), sink, sent.clone()).unwrap();
        let packed = |n: u32| Frame {
            number: n,
            capture_qpc: 0,
            bytes: vec![0; 16],
        };
        shared.offer(packed(1));
        let end = Instant::now() + Duration::from_secs(5);
        while calls.load(Ordering::SeqCst) < 1 && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(5));
        }
        // Frame 1 failed, so the 60 s ack timeout must not hold frame 2 back.
        shared.offer(packed(2));
        while sent.load(Ordering::Relaxed) < 1 && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(sent.load(Ordering::Relaxed), 1);
        shared.close();
        thread.join().unwrap();
    }

    // --- the tap on WARP -----------------------------------------------------------------------------------

    fn warp() -> Arc<Renderer> {
        let gpu = GpuDevice::new(AdapterKind::Warp).expect("WARP device");
        Arc::new(Renderer::new(Arc::new(gpu)).expect("renderer"))
    }

    fn fill(r: &Renderer, tex: &RgbaTexture, value: impl Fn(u32, u32) -> [u8; 4]) {
        let mut data = Vec::with_capacity((tex.width * tex.height * 4) as usize);
        for y in 0..tex.height {
            for x in 0..tex.width {
                data.extend_from_slice(&value(x, y));
            }
        }
        // SAFETY: `data` holds `height` rows of `width * 4` bytes, as the call is told.
        unsafe {
            r.gpu
                .context
                .UpdateSubresource(&tex.texture, 0, None, data.as_ptr() as *const _, tex.width * 4, 0)
        };
    }

    /// Run the tap on `frame` until its picture arrives in the gate, and return it.
    fn preview_of(r: &Renderer, frame: &RgbaTexture, request: (u32, u32)) -> Frame {
        let shared = Shared::new(ACK_TIMEOUT);
        let mut tap = PreviewTap::new(
            shared.clone(),
            Arc::new(AtomicU32::new(41)),
            Arc::new(AtomicBool::new(false)),
            request,
        );
        tap.capture(r, frame, 7).expect("capture");
        let end = Instant::now() + Duration::from_secs(5);
        loop {
            tap.collect(false);
            if let Some(f) = lock(&shared.gate).take(Instant::now()) {
                return f;
            }
            assert!(Instant::now() < end, "the picture never arrived");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// BT.709 limited range, the way the page's shader and `gpu::convert::yuv_to_rgb_rows` read it: the RGB of the
    /// pixel (`x`, `y`) of an NV12 preview frame, as 0..255 floats before rounding.
    fn rgb_f(f: &Frame, x: u32, y: u32) -> [f64; 3] {
        let h = Header::decode(&f.bytes).unwrap();
        assert_eq!(h.format, FORMAT_NV12);
        let (stride, height) = (h.stride as usize, h.height as usize);
        let plane = &f.bytes[HEADER_LEN..];
        let luma = plane[y as usize * stride + x as usize] as f64 / 255.0;
        let uv_at = height * stride + (y as usize / 2) * stride + (x as usize / 2) * 2;
        let (cb, cr) = (plane[uv_at] as f64 / 255.0, plane[uv_at + 1] as f64 / 255.0);
        let rows = yuv_to_rgb_rows(ColorSpace::Bt709, ColorRange::Limited, 8);
        [0, 1, 2].map(|c| {
            let v = rows[c][0] * luma + rows[c][1] * cb + rows[c][2] * cr + rows[c][3];
            v.clamp(0.0, 1.0) * 255.0
        })
    }

    /// The pixel as RGB bytes (alpha 255, as the old RGBA frames had it).
    fn pixel(f: &Frame, x: u32, y: u32) -> [u8; 4] {
        let [r, g, b] = rgb_f(f, x, y);
        [r.round() as u8, g.round() as u8, b.round() as u8, 255]
    }

    /// Every colour channel of `got` is within `tol` codes of `want`.
    fn near(got: [u8; 4], want: [u8; 4], tol: i32, what: &str) {
        for c in 0..3 {
            assert!(
                (got[c] as i32 - want[c] as i32).abs() <= tol,
                "{what}: {got:?} vs {want:?}"
            );
        }
    }

    #[test]
    fn a_sixteen_by_nine_frame_becomes_960_by_540_nv12_in_rgb_order() {
        let r = warp();
        let src = r.texture(1920, 1080).unwrap();
        // Left half red, right half blue, top row marked green: tells order, orientation and size apart.
        fill(&r, &src, |x, y| {
            if y == 0 {
                [0, 255, 0, 255]
            } else if x < 960 {
                [255, 0, 0, 255]
            } else {
                [0, 0, 255, 255]
            }
        });
        let f = preview_of(&r, &src, (3000, 2000));
        let h = Header::decode(&f.bytes).unwrap();
        assert_eq!(
            (h.width, h.height, h.stride, h.frame, h.format),
            (960, 540, 960, 41, FORMAT_NV12)
        );
        assert_eq!(
            (h.source_width, h.source_height),
            (1920, 1080),
            "the size before the downscale"
        );
        // The Y plane and the half height UV plane: 1.5 bytes a pixel instead of the 4 of RGBA.
        assert_eq!(f.bytes.len(), HEADER_LEN + 960 * 540 * 3 / 2);
        assert_eq!(f.capture_qpc, 7);
        // Red on the left, blue on the right: no channel swap anywhere.
        near(pixel(&f, 100, 300), [255, 0, 0, 255], 3, "red");
        near(pixel(&f, 800, 300), [0, 0, 255, 255], 3, "blue");
        // The top row of the source is the first row of the payload (not flipped): the green marker row averaged with
        // the red row below it is a yellow-brown, where the bottom row is plain red.
        let top = pixel(&f, 100, 0);
        assert!(
            top[0] > 150 && top[1] > 50 && top[2] < 60,
            "the first row mixes the green marker row into the red: {top:?}"
        );
        near(pixel(&f, 100, 539), [255, 0, 0, 255], 3, "the last row");
    }

    #[test]
    fn a_four_by_three_frame_keeps_its_aspect_without_bars() {
        let r = warp();
        let src = r.texture(1280, 960).unwrap();
        fill(&r, &src, |x, _| if x < 640 { [200, 10, 10, 255] } else { [10, 10, 200, 255] });
        let f = preview_of(&r, &src, (960, 540));
        let h = Header::decode(&f.bytes).unwrap();
        assert_eq!((h.width, h.height), (720, 540));
        // The picture reaches every edge: no black bars at the sides.
        for (x, y) in [(0, 0), (0, 539), (719, 0), (719, 539)] {
            let p = pixel(&f, x, y);
            assert!(p[0] > 150 || p[2] > 150, "bar at ({x},{y}): {p:?}");
        }
        // 640x480 stays 640x480 (never upscaled), and a small window asks for less.
        let small = r.texture(640, 480).unwrap();
        fill(&r, &small, |_, _| [9, 8, 7, 255]);
        let f = preview_of(&r, &small, (960, 540));
        assert_eq!((Header::decode(&f.bytes).unwrap().width, Header::decode(&f.bytes).unwrap().height), (640, 480));
        let f = preview_of(&r, &src, (400, 300));
        let h = Header::decode(&f.bytes).unwrap();
        assert_eq!((h.width, h.height), (400, 300));
    }

    /// The colour of the NV12 picture against the RGBA picture the old path sent (the downscale output read back as it
    /// is), on bars, ramps and a noisy patch. Chroma is shared by 2 x 2 pixels, so a hard colour edge is compared one
    /// pixel away from it; everywhere else the error is the 8 bit rounding of the two conversions.
    #[test]
    fn the_nv12_picture_matches_the_rgba_reference_within_two_codes_away_from_colour_edges() {
        let r = warp();
        let (w, h) = (640u32, 360u32);
        let src = r.texture(w, h).unwrap();
        // Blocks of 40 x 40 smooth colours (a different hue each), a horizontal grey ramp in the bottom third.
        let value = |x: u32, y: u32| -> [u8; 4] {
            if y >= 240 {
                let g = (x * 255 / (w - 1)) as u8;
                [g, g, g, 255]
            } else {
                let (bx, by) = (x / 40, y / 40);
                let seed = bx * 7 + by * 13;
                [
                    (seed * 37 % 256) as u8,
                    (seed * 91 % 256) as u8,
                    (seed * 59 % 256) as u8,
                    255,
                ]
            }
        };
        fill(&r, &src, value);
        // Same size as the source: the downscale is the identity, so the reference is `value` itself.
        let f = preview_of(&r, &src, (640, 360));
        let hd = Header::decode(&f.bytes).unwrap();
        assert_eq!((hd.width, hd.height), (640, 360));
        let mut worst = 0.0f64;
        let mut worst_at = (0, 0);
        for y in 0..h {
            for x in 0..w {
                // a pixel next to a block edge shares chroma with its neighbour across the edge
                let edge = x % 40 == 0 || x % 40 == 39 || y % 40 == 0 || y % 40 == 39;
                if edge && y < 240 {
                    continue;
                }
                let want = value(x, y);
                let got = rgb_f(&f, x, y);
                for c in 0..3 {
                    let e = (got[c] - want[c] as f64).abs();
                    if e > worst {
                        worst = e;
                        worst_at = (x, y);
                    }
                }
            }
        }
        // 8 bit Y, Cb and Cr in limited range: one code of Y is 1.16 codes of RGB and a code of chroma up to 2.2, so
        // the round trip is within about 2 to 3 codes of the original; the chroma of 2 x 2 pixels averages the rest.
        assert!(worst <= 3.0, "worst error {worst:.2} codes at {worst_at:?}");
    }

    #[test]
    fn the_downscale_averages_instead_of_dropping_texels() {
        let r = warp();
        let src = r.texture(1920, 1080).unwrap();
        // Alternating black and white columns: a point sample would give 0 or 255, an average about 127.
        fill(&r, &src, |x, _| if x % 2 == 0 { [0, 0, 0, 255] } else { [255, 255, 255, 255] });
        for request in [(960, 540), (480, 270)] {
            let f = preview_of(&r, &src, request);
            let h = Header::decode(&f.bytes).unwrap();
            assert_eq!(h.width, request.0);
            for x in [0, 1, 17, h.width - 1] {
                let v = pixel(&f, x, 5)[0] as i32;
                assert!((v - 127).abs() <= 3, "{request:?} column {x}: {v}");
            }
        }
    }

    // --- Compare: two pictures in one frame ----------------------------------------------------------------

    /// A one picture NV12 frame of `w` x `h` whose payload bytes are all `fill`.
    fn flat(number: u32, w: u32, h: u32, fill: u8) -> Frame {
        let header = Header { width: w, height: h, stride: w, frame: number, source_width: 640, source_height: 360, format: FORMAT_NV12 };
        let mut bytes = header.encode().to_vec();
        bytes.resize(HEADER_LEN + header.payload_len(), fill);
        Frame { number, capture_qpc: 5, bytes }
    }

    #[test]
    fn a_pair_frame_says_so_in_its_header_and_holds_the_camera_picture_then_the_output() {
        let h = Header { width: 8, height: 4, stride: 8, frame: 9, source_width: 1920, source_height: 1080, format: FORMAT_NV12_PAIR };
        assert_eq!(&h.encode()[24..28], &[2, 0, 0, 0], "the format word: 2 is two NV12 pictures");
        assert_eq!(Header::decode(&h.encode()), Some(h));
        assert_eq!(h.payload_len(), 2 * 8 * 4 * 3 / 2, "two pictures");
        assert_eq!(Header { format: FORMAT_NV12, ..h }.payload_len(), 8 * 4 * 3 / 2);

        let (before, after) = (flat(9, 8, 4, 0x11), flat(9, 8, 4, 0x22));
        let p = pair(&before, &after).expect("two pictures of one size pair up");
        let ph = Header::decode(&p.bytes).unwrap();
        assert_eq!((ph.width, ph.height, ph.stride, ph.frame, ph.format), (8, 4, 8, 9, FORMAT_NV12_PAIR));
        assert_eq!((ph.source_width, ph.source_height), (640, 360));
        assert_eq!((p.number, p.capture_qpc), (9, 5));
        assert_eq!(p.bytes.len(), HEADER_LEN + ph.payload_len());
        let one = 8 * 4 * 3 / 2;
        assert!(p.bytes[HEADER_LEN..HEADER_LEN + one].iter().all(|&b| b == 0x11), "the camera picture first");
        assert!(p.bytes[HEADER_LEN + one..].iter().all(|&b| b == 0x22), "the output second");

        // pictures of two sizes, or one that is already a pair, are not paired
        assert!(pair(&flat(9, 8, 4, 0), &flat(9, 4, 2, 0)).is_none());
        assert!(pair(&p, &p).is_none());
    }

    /// The pixel (`x`, `y`) of picture `which` (0 the camera picture, 1 the output) of a pair frame.
    fn pair_pixel(f: &Frame, which: usize, x: u32, y: u32) -> [u8; 4] {
        let h = Header::decode(&f.bytes).unwrap();
        assert_eq!(h.format, FORMAT_NV12_PAIR);
        let one = h.payload_len() / 2;
        let mut bytes = Header { format: FORMAT_NV12, ..h }.encode().to_vec();
        bytes.extend_from_slice(&f.bytes[HEADER_LEN + which * one..HEADER_LEN + (which + 1) * one]);
        pixel(&Frame { number: f.number, capture_qpc: f.capture_qpc, bytes }, x, y)
    }

    /// Run `step` (a call of the tap) until the gate has a frame, and take it.
    fn next_frame(tap: &mut PreviewTap, shared: &Arc<Shared>, mut step: impl FnMut(&mut PreviewTap)) -> Frame {
        let end = Instant::now() + Duration::from_secs(5);
        loop {
            step(tap);
            tap.frame_done(true);
            if let Some(f) = lock(&shared.gate).take(Instant::now()) {
                return f;
            }
            assert!(Instant::now() < end, "no frame came");
            std::thread::sleep(FRAME_INTERVAL);
        }
    }

    #[test]
    fn the_tap_sends_both_pictures_in_one_frame_in_compare_and_one_again_after() {
        let r = warp();
        let (before, after) = (r.texture(640, 360).unwrap(), r.texture(640, 360).unwrap());
        fill(&r, &before, |_, _| [220, 30, 30, 255]);
        fill(&r, &after, |_, _| [90, 90, 90, 255]);
        let shared = Shared::new(ACK_TIMEOUT);
        let mut tap = PreviewTap::new(shared.clone(), Arc::new(AtomicU32::new(0)), Arc::new(AtomicBool::new(false)), (640, 360));

        let f = next_frame(&mut tap, &shared, |t| t.on_compare(&r, &before, &after, 3));
        let h = Header::decode(&f.bytes).unwrap();
        assert_eq!((h.width, h.height, h.stride, h.format), (640, 360, 640, FORMAT_NV12_PAIR));
        assert_eq!((h.source_width, h.source_height), (640, 360));
        assert_eq!(f.bytes.len(), HEADER_LEN + 2 * 640 * 360 * 3 / 2);
        assert_eq!(f.capture_qpc, 3);
        near(pair_pixel(&f, 0, 100, 100), [220, 30, 30, 255], 3, "the camera picture is the first");
        near(pair_pixel(&f, 1, 100, 100), [90, 90, 90, 255], 3, "the output is the second");
        assert!(tap.before.is_some());
        assert!(lock(&shared.gate).ack(f.number));

        // Compare is over: plain frames of one picture, and the second ring is gone.
        std::thread::sleep(FRAME_INTERVAL);
        let f = next_frame(&mut tap, &shared, |t| t.on_frame(&r, &after, 4));
        assert_eq!(Header::decode(&f.bytes).unwrap().format, FORMAT_NV12);
        assert_eq!(f.bytes.len(), HEADER_LEN + 640 * 360 * 3 / 2);
        near(pixel(&f, 100, 100), [90, 90, 90, 255], 3, "the output");
        assert!(tap.before.is_none() && tap.held.before.is_none() && tap.held.after.is_none());
    }

    #[test]
    fn the_tap_makes_no_frames_while_hidden_and_resumes() {
        let r = warp();
        let src = r.texture(640, 360).unwrap();
        fill(&r, &src, |_, _| [1, 2, 3, 255]);
        let shared = Shared::new(ACK_TIMEOUT);
        let hidden = Arc::new(AtomicBool::new(true));
        let mut tap = PreviewTap::new(shared.clone(), Arc::new(AtomicU32::new(0)), hidden.clone(), (640, 360));
        for _ in 0..5 {
            tap.on_frame(&r, &src, 0);
            tap.frame_done(true);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(shared.made.load(Ordering::Relaxed), 0);
        assert!(tap.ring.is_none(), "no GPU work while hidden");
        hidden.store(false, Ordering::Relaxed);
        let end = Instant::now() + Duration::from_secs(5);
        while shared.made.load(Ordering::Relaxed) == 0 {
            tap.on_frame(&r, &src, 0);
            tap.frame_done(true);
            assert!(Instant::now() < end, "no frame after the window came back");
            std::thread::sleep(Duration::from_millis(40));
        }
    }

    /// One frame waiting in the gate (behind the one in flight) and nothing is made: no downscale, no NV12 dispatch, no
    /// copy. The skip is counted, the pacer's slot is kept for the next frame, and the first ack lets a frame through.
    #[test]
    fn no_frame_is_made_while_one_waits_and_one_is_in_flight() {
        let r = warp();
        let src = r.texture(640, 360).unwrap();
        fill(&r, &src, |_, _| [90, 90, 90, 255]);
        let shared = Shared::new(ACK_TIMEOUT);
        let mut tap = PreviewTap::new(
            shared.clone(),
            Arc::new(AtomicU32::new(0)),
            Arc::new(AtomicBool::new(false)),
            (640, 360),
        );
        let made = |s: &Shared| s.made.load(Ordering::Relaxed);
        let skipped = |s: &Shared| s.skipped.load(Ordering::Relaxed);
        let step = |tap: &mut PreviewTap| {
            tap.on_frame(&r, &src, 0);
            tap.frame_done(true);
            // the 30 fps pacer: wait out the slot so only the back-pressure can say no
            std::thread::sleep(FRAME_INTERVAL + Duration::from_millis(2));
        };

        // Nothing in flight, nothing waiting: frame 0 is made and goes straight in flight (what the sender does).
        step(&mut tap);
        assert_eq!((made(&shared), skipped(&shared)), (1, 0));
        let first = lock(&shared.gate).take(Instant::now()).expect("frame 0");
        // One in flight, none waiting: the next frame is made and waits.
        step(&mut tap);
        assert_eq!((made(&shared), skipped(&shared)), (2, 0));
        assert!(lock(&shared.gate).has_pending());
        let ring_slots = tap.ring.as_ref().unwrap().ring.allocated();
        // One in flight and one waiting: nothing is made, however many frames come.
        for _ in 0..4 {
            step(&mut tap);
        }
        assert_eq!(
            (made(&shared), skipped(&shared)),
            (2, 4),
            "four frames skipped, none captured"
        );
        assert_eq!(
            tap.ring.as_ref().unwrap().ring.pending(),
            0,
            "nothing was copied to the staging ring either"
        );
        assert_eq!(
            tap.ring.as_ref().unwrap().ring.allocated(),
            ring_slots,
            "and no slot was made for it"
        );
        assert_eq!(
            lock(&shared.gate).overwritten,
            0,
            "no offer ever replaced a waiting frame"
        );
        // The ack of frame 0 puts the waiting frame in flight; the slot is free again, the very next frame is made.
        assert!(lock(&shared.gate).ack(first.number));
        let second = lock(&shared.gate).take(Instant::now()).expect("frame 1");
        assert_eq!(
            second.number,
            first.number + 1,
            "the frames that were skipped took no numbers"
        );
        step(&mut tap);
        assert_eq!((made(&shared), skipped(&shared)), (3, 4));
        assert_eq!(shared.stats(0).skipped, 4);
    }

    /// The ring is mapped before the next frame is made: a frame the GPU has not handed over yet (next-frame readback)
    /// holds the tap back like a waiting frame does, and it is delivered by a later `frame_done`.
    #[test]
    fn a_frame_still_on_the_gpu_holds_the_next_one_back() {
        let r = warp();
        let src = r.texture(640, 360).unwrap();
        fill(&r, &src, |_, _| [90, 90, 90, 255]);
        let shared = Shared::new(ACK_TIMEOUT);
        let mut tap = PreviewTap::new(
            shared.clone(),
            Arc::new(AtomicU32::new(0)),
            Arc::new(AtomicBool::new(false)),
            (640, 360),
        );
        tap.on_frame(&r, &src, 0);
        assert_eq!(tap.ring.as_ref().unwrap().ring.pending(), 1);
        std::thread::sleep(FRAME_INTERVAL + Duration::from_millis(2));
        // the frame is not mapped (the engine did not call `frame_done`): the next one is skipped
        tap.on_frame(&r, &src, 0);
        assert_eq!(tap.ring.as_ref().unwrap().ring.pending(), 1);
        assert_eq!(shared.skipped.load(Ordering::Relaxed), 1);
        tap.frame_done(true);
        assert_eq!(shared.made.load(Ordering::Relaxed), 1);
        assert_eq!(tap.ring.as_ref().unwrap().ring.pending(), 0);
    }

    /// The whole path inside a real engine on WARP: synthetic colour bars, `Preview::start`, the one-in-flight rule,
    /// the ack, and `stop` ending the stream.
    #[test]
    fn the_tap_runs_inside_the_engine_and_stop_ends_the_stream() {
        use crate::capture::PixelFormat;
        use crate::engine::{OutputSettings, ReadbackPolicy, Settings, SourceSpec, SyntheticSpec};
        use crate::gpu::convert::{ColorRange, ColorSettings, ColorSpace};
        use crate::gpu::pattern::smpte_bars;

        let gpu = Arc::new(GpuDevice::new(AdapterKind::Warp).expect("WARP"));
        let dir = std::env::temp_dir().join(format!("bettercam-preview-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("engine.bin");
        let _ = std::fs::remove_file(&path);
        let writer = Arc::new(vcam_protocol::Writer::create(&path).unwrap());
        let engine = Engine::with_parts(gpu, writer).unwrap();
        engine.start(Settings {
            source: SourceSpec::Synthetic(SyntheticSpec {
                width: 640,
                height: 360,
                fps: 30,
                format: PixelFormat::Nv12,
                color: ColorSettings {
                    space: Some(ColorSpace::Bt709),
                    range: Some(ColorRange::Limited),
                },
            }),
            output: OutputSettings {
                width: Some(640),
                height: Some(360),
                ..OutputSettings::default()
            },
            flip_vertical: false,
            color: ColorSettings::default(),
            readback: ReadbackPolicy::Immediate,
            deactivate_when_not_showing: false,
        });
        engine.wait_live(Duration::from_secs(20)).unwrap();

        let got = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let sink: Sink = {
            let got = got.clone();
            Arc::new(move |bytes| {
                lock(&got).push(bytes);
                true
            })
        };
        let wait_for = |n: usize| {
            let end = Instant::now() + Duration::from_secs(20);
            while lock(&got).len() < n {
                assert!(Instant::now() < end, "frame {n} never came");
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        let preview = Preview::new();
        preview.start(&engine, (480, 270), sink).unwrap();

        // The first frame: 480x270 (the 16:9 output scaled to the request), colour bars, header agrees.
        wait_for(1);
        let first = lock(&got)[0].clone();
        let h = Header::decode(&first).unwrap();
        assert_eq!(
            (h.width, h.height, h.stride, h.format),
            (480, 270, 480, FORMAT_NV12)
        );
        assert_eq!(first.len(), HEADER_LEN + 480 * 270 * 3 / 2);
        let bars = smpte_bars(640, 360);
        let reference = &bars[(10 * 640 + 10) * 3..(10 * 640 + 10) * 3 + 3];
        near(
            pixel(
                &Frame {
                    number: h.frame,
                    capture_qpc: 0,
                    bytes: first.clone(),
                },
                7,
                7,
            ),
            [reference[0], reference[1], reference[2], 255],
            5,
            "the colour bars",
        );

        // Nothing is acknowledged: one frame is in flight, one more is made and waits, and then the tap makes nothing.
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(lock(&got).len(), 1, "one frame in flight");
        assert_eq!(preview.frames_sent(), 1);
        let held = preview.stats();
        assert_eq!(held.made, 2, "the one in flight and the one that waits");
        assert!(
            held.skipped >= 5,
            "the frames that came after were skipped, not made: {held:?}"
        );
        assert_eq!(held.overwritten, 0, "and none replaced the waiting one");
        preview.ack(h.frame);
        wait_for(2);
        let second = Header::decode(&lock(&got)[1]).unwrap();
        assert_eq!(
            second.frame,
            h.frame + 1,
            "the waiting frame is the next one made: skipped frames take no number"
        );

        // Stop: no more frames, and the tap is gone from the engine.
        preview.stop(Some(&engine));
        preview.ack(second.frame);
        std::thread::sleep(Duration::from_millis(300));
        let n = lock(&got).len();
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(lock(&got).len(), n, "no frames after stop");
        engine.close();
    }

    /// Collect order: inside a real engine, the picture the tap hands to the gate after a frame is the frame the engine
    /// just rendered and output (its capture time is the one the output frame carries), never the one before it. Each
    /// frame is checked (the wrapper takes and acknowledges the frame at once, so back-pressure never skips one).
    #[test]
    fn the_preview_frame_is_the_frame_the_engine_outputs_in_every_readback_policy() {
        use crate::capture::PixelFormat;
        use crate::engine::{OutputSettings, ReadbackPolicy, Settings, SourceSpec, SyntheticSpec};
        use crate::gpu::convert::ColorSettings;

        /// Wraps the preview tap: remembers the capture time `on_frame` got, and after `frame_done` looks at what the
        /// tap handed to the gate.
        struct Order {
            inner: PreviewTap,
            shared: Arc<Shared>,
            qpc: u64,
            made_before: u64,
            checked: Arc<AtomicU64>,
            wrong: Arc<AtomicU64>,
        }
        impl FrameTap for Order {
            fn on_frame(&mut self, r: &Renderer, frame: &RgbaTexture, capture_qpc: u64) {
                self.qpc = capture_qpc;
                self.made_before = self.shared.made.load(Ordering::Relaxed);
                self.inner.on_frame(r, frame, capture_qpc);
            }
            fn frame_done(&mut self, wait: bool) {
                self.inner.frame_done(wait);
                if self.shared.made.load(Ordering::Relaxed) == self.made_before {
                    return; // no picture this frame (the 30 fps limit)
                }
                let mut g = lock(&self.shared.gate);
                match g.pending.as_ref().map(|f| f.capture_qpc) {
                    Some(q) if q == self.qpc => {
                        self.checked.fetch_add(1, Ordering::Relaxed);
                    }
                    _ => {
                        self.wrong.fetch_add(1, Ordering::Relaxed);
                    }
                }
                // like a page that acknowledges at once
                if let Some(f) = g.take(Instant::now()) {
                    g.ack(f.number);
                }
            }
        }

        for policy in [ReadbackPolicy::Immediate, ReadbackPolicy::NextFrame] {
            let gpu = Arc::new(GpuDevice::new(AdapterKind::Warp).expect("WARP"));
            let dir = std::env::temp_dir().join(format!(
                "bettercam-preview-order-{}-{policy:?}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("engine.bin");
            let _ = std::fs::remove_file(&path);
            let writer = Arc::new(vcam_protocol::Writer::create(&path).unwrap());
            let engine = Engine::with_parts(gpu, writer).unwrap();
            engine.start(Settings {
                source: SourceSpec::Synthetic(SyntheticSpec {
                    width: 640,
                    height: 360,
                    fps: 30,
                    format: PixelFormat::Nv12,
                    color: ColorSettings::default(),
                }),
                output: OutputSettings::default(),
                flip_vertical: false,
                color: ColorSettings::default(),
                readback: policy,
                deactivate_when_not_showing: false,
            });
            engine.wait_live(Duration::from_secs(20)).unwrap();
            let shared = Shared::new(ACK_TIMEOUT);
            let (checked, wrong) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
            let tap = Order {
                inner: PreviewTap::new(
                    shared.clone(),
                    Arc::new(AtomicU32::new(0)),
                    Arc::new(AtomicBool::new(false)),
                    (320, 180),
                ),
                shared: shared.clone(),
                qpc: 0,
                made_before: 0,
                checked: checked.clone(),
                wrong: wrong.clone(),
            };
            engine.send(Msg::SetTap(Some(Box::new(tap))));
            let end = Instant::now() + Duration::from_secs(30);
            while checked.load(Ordering::Relaxed) < 8 {
                assert!(
                    Instant::now() < end,
                    "{policy:?}: only {} frames checked",
                    checked.load(Ordering::Relaxed)
                );
                std::thread::sleep(Duration::from_millis(20));
            }
            engine.send(Msg::SetTap(None));
            assert_eq!(
                wrong.load(Ordering::Relaxed),
                0,
                "{policy:?}: a preview frame was not the frame the engine output"
            );
            engine.close();
        }
    }

    #[test]
    fn a_size_change_rebuilds_the_ring_and_drops_the_old_pictures() {
        let r = warp();
        let src = r.texture(1920, 1080).unwrap();
        fill(&r, &src, |_, _| [5, 5, 5, 255]);
        let shared = Shared::new(ACK_TIMEOUT);
        let mut tap = PreviewTap::new(
            shared.clone(),
            Arc::new(AtomicU32::new(0)),
            Arc::new(AtomicBool::new(false)),
            (960, 540),
        );
        tap.capture(&r, &src, 0).unwrap();
        assert_eq!(tap.ring.as_ref().unwrap().size, (960, 540));
        tap.request = (480, 270);
        tap.capture(&r, &src, 0).unwrap();
        assert_eq!(tap.ring.as_ref().unwrap().size, (480, 270));
        assert_eq!(tap.ring.as_ref().unwrap().ring.pending(), 1, "the 960x540 picture is gone");
    }

    /// L1: the header says how big the tapped picture was, also when it is not the output size, and a change of the
    /// source size alone (the same shrunk size) never leaves a picture with the old one in the ring.
    #[test]
    fn the_header_carries_the_size_of_the_tapped_picture_and_a_source_change_starts_a_new_ring() {
        let r = warp();
        let big = r.texture(1280, 720).unwrap();
        fill(&r, &big, |_, _| [10, 20, 30, 255]);
        let f = preview_of(&r, &big, (320, 180));
        let h = Header::decode(&f.bytes).unwrap();
        assert_eq!((h.width, h.height, h.source_width, h.source_height), (320, 180, 1280, 720));
        // 640x360 and 1280x720 both shrink to 320x180 here: the sizes of the preview are equal, the sources are not
        let shared = Shared::new(ACK_TIMEOUT);
        let mut tap = PreviewTap::new(
            shared.clone(),
            Arc::new(AtomicU32::new(0)),
            Arc::new(AtomicBool::new(false)),
            (320, 180),
        );
        tap.capture(&r, &big, 0).unwrap();
        let small = r.texture(640, 360).unwrap();
        fill(&r, &small, |_, _| [30, 20, 10, 255]);
        tap.capture(&r, &small, 0).unwrap();
        assert_eq!(tap.ring.as_ref().unwrap().size, (320, 180));
        assert_eq!(tap.ring.as_ref().unwrap().source, (640, 360));
        assert_eq!(tap.ring.as_ref().unwrap().ring.pending(), 1, "the picture of the other source is gone");
        let end = Instant::now() + Duration::from_secs(5);
        let f = loop {
            tap.collect(false);
            if let Some(f) = lock(&shared.gate).take(Instant::now()) {
                break f;
            }
            assert!(Instant::now() < end, "the picture never arrived");
            std::thread::sleep(Duration::from_millis(2));
        };
        let h = Header::decode(&f.bytes).unwrap();
        assert_eq!((h.source_width, h.source_height), (640, 360));
        near(
            pixel(&f, 5, 5),
            [30, 20, 10, 255],
            3,
            "the picture of the new source",
        );
    }

    /// L9: the staging ring and the downscale constants belong to the GPU device; the engine tells the tap when it
    /// built a new one, and the tap makes pictures again on it.
    #[test]
    fn the_tap_starts_again_on_a_new_device() {
        let old = warp();
        let src = old.texture(640, 360).unwrap();
        fill(&old, &src, |_, _| [9, 9, 9, 255]);
        let shared = Shared::new(ACK_TIMEOUT);
        let mut tap = PreviewTap::new(
            shared.clone(),
            Arc::new(AtomicU32::new(0)),
            Arc::new(AtomicBool::new(false)),
            (640, 360),
        );
        tap.capture(&old, &src, 0).unwrap();
        assert!(tap.ring.is_some());
        tap.device_changed();
        assert!(tap.ring.is_none(), "the ring of the old device is gone");
        assert!(
            tap.cs_constants.is_none(),
            "so are the constants of the NV12 dispatch"
        );
        // the old device is gone for good: only the new one is used from now on
        drop(src);
        drop(old);
        let new = warp();
        let src = new.texture(640, 360).unwrap();
        fill(&new, &src, |_, _| [7, 7, 7, 255]);
        let end = Instant::now() + Duration::from_secs(5);
        while shared.made.load(Ordering::Relaxed) == 0 {
            tap.on_frame(&new, &src, 0);
            tap.frame_done(true);
            assert!(Instant::now() < end, "no picture from the new device");
            std::thread::sleep(Duration::from_millis(40));
        }
        let picture = lock(&shared.gate).take(Instant::now()).expect("a picture");
        near(
            pixel(&picture, 3, 3),
            [7, 7, 7, 255],
            3,
            "the picture of the new device",
        );
    }

    /// L9: inside a real engine, a lost GPU device does not end the preview: it goes on with frames of the new one.
    #[test]
    fn the_preview_goes_on_after_the_engine_rebuilds_its_device() {
        use crate::capture::PixelFormat;
        use crate::engine::{OutputSettings, ReadbackPolicy, Settings, SourceSpec, SyntheticSpec};
        use crate::gpu::convert::ColorSettings;

        let gpu = Arc::new(GpuDevice::new(AdapterKind::Warp).expect("WARP"));
        let dir = std::env::temp_dir().join(format!("bettercam-preview-loss-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("engine.bin");
        let _ = std::fs::remove_file(&path);
        let writer = Arc::new(vcam_protocol::Writer::create(&path).unwrap());
        let engine = Engine::with_parts(gpu, writer).unwrap();
        engine.start(Settings {
            source: SourceSpec::Synthetic(SyntheticSpec {
                width: 640,
                height: 360,
                fps: 30,
                format: PixelFormat::Nv12,
                color: ColorSettings::default(),
            }),
            output: OutputSettings::default(),
            flip_vertical: false,
            color: ColorSettings::default(),
            readback: ReadbackPolicy::Immediate,
            deactivate_when_not_showing: false,
        });
        engine.wait_live(Duration::from_secs(20)).unwrap();

        let got = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let sink: Sink = {
            let got = got.clone();
            Arc::new(move |bytes| {
                lock(&got).push(bytes);
                true
            })
        };
        let preview = Preview::new();
        preview.start(&engine, (480, 270), sink).unwrap();
        // every frame that arrives is acknowledged, like the page does
        let ack_until = |sent: u64, what: &str| {
            let end = Instant::now() + Duration::from_secs(30);
            while preview.frames_sent() < sent {
                let last = lock(&got).last().and_then(|b| Header::decode(b));
                if let Some(h) = last {
                    preview.ack(h.frame);
                }
                assert!(Instant::now() < end, "{what}: {} frames", preview.frames_sent());
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        ack_until(3, "before the loss");

        engine.inject_device_loss(1);
        let end = Instant::now() + Duration::from_secs(30);
        while engine.gpu_generation() < 2 {
            assert!(Instant::now() < end, "the device was never rebuilt");
            std::thread::sleep(Duration::from_millis(10));
        }
        engine.wait_live(Duration::from_secs(30)).unwrap();
        let at_rebuild = preview.frames_sent();
        ack_until(at_rebuild + 5, "after the rebuild");
        preview.stop(Some(&engine));
        engine.close();
    }
}
