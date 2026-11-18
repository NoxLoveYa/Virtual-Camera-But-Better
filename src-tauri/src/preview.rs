//! Live preview (plan 4.4): the engine's output frames, shrunk on the GPU, on their way to the webview.
//!
//! ```text
//! render thread: PreviewTap::on_frame   downscale (GPU) -> staging ring -> pack header + pixels
//!                      |  offer (overwrites the pending frame)
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
//! Wire format of one frame (little endian): `width`, `height`, `stride` (bytes per row), `frame` number as four
//! `u32`, then `stride * height` bytes of pixels. Pixels are `R8G8B8A8`, top row first, alpha 255, which is exactly
//! what WebGL's `texImage2D(RGBA, UNSIGNED_BYTE)` reads: the GPU renders in that order, so neither side swaps bytes.
//! `stride` equals `width * 4` today; the field is there so the layout can change without a protocol change.
//!
//! Set `BETTER_CAM_PREVIEW_LOG=1` to get one line per second on stderr with the frame counters.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R8G8B8A8_UNORM;

use crate::engine::{Engine, FrameTap, Msg};
use crate::gpu::downscale::{fit_within, Downscaler};
use crate::gpu::readback::{MappedPlane, PlaneDesc, ReadbackRing};
use crate::gpu::{GpuResult, Renderer, RgbaTexture};

/// Largest preview picture: 960x540 (plan 4.4).
pub const MAX_SIZE: (u32, u32) = (960, 540);
/// Bytes in front of the pixels.
pub const HEADER_LEN: usize = 16;
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

/// The 16 bytes in front of every frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub width: u32,
    pub height: u32,
    /// Bytes per row of pixels.
    pub stride: u32,
    /// Counts up per frame made; gaps mean frames that were overwritten. What `preview_ack` carries.
    pub frame: u32,
}

impl Header {
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        for (i, v) in [self.width, self.height, self.stride, self.frame].iter().enumerate() {
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
        })
    }
}

/// A packed frame: header and pixels in one buffer, ready to send.
pub struct Frame {
    pub number: u32,
    pub bytes: Vec<u8>,
}

/// Header plus the rows of a mapped `width` x `height` RGBA plane (padding between rows left out).
fn pack(plane: &MappedPlane<'_>, width: u32, height: u32, number: u32) -> Frame {
    let stride = width * 4;
    let mut bytes = Vec::with_capacity(HEADER_LEN + stride as usize * height as usize);
    bytes.extend_from_slice(
        &Header {
            width,
            height,
            stride,
            frame: number,
        }
        .encode(),
    );
    for y in 0..height as usize {
        bytes.extend_from_slice(plane.row(y));
    }
    Frame { number, bytes }
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
        }
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
            Some((n, _)) if n == number => {
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
}

impl Shared {
    fn new(timeout: Duration) -> Arc<Shared> {
        Arc::new(Shared {
            gate: Mutex::new(Gate::new(timeout)),
            wake: Condvar::new(),
            made: AtomicU64::new(0),
        })
    }

    fn offer(&self, frame: Frame) {
        self.made.fetch_add(1, Ordering::Relaxed);
        lock(&self.gate).offer(frame);
        self.wake.notify_one();
    }

    fn ack(&self, number: u32) {
        if lock(&self.gate).ack(number) {
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
                if sink(frame.bytes) {
                    failures = 0;
                    sent.fetch_add(1, Ordering::Relaxed);
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
    eprintln!(
        "[preview] made={} sent={} overwritten={} ack_timeouts={}",
        shared.made.load(Ordering::Relaxed),
        sent,
        gate.overwritten,
        gate.timeouts
    );
}

// ---------------------------------------------------------------------------------------------------------------
// Tap

/// The engine's frame tap: runs on the render thread with the finished output frame.
struct PreviewTap {
    shared: Arc<Shared>,
    numbers: Arc<AtomicU32>,
    hidden: Arc<AtomicBool>,
    /// What the window asked for, in pixels (already limited to [`MAX_SIZE`] when used).
    request: (u32, u32),
    down: Downscaler,
    /// The staging ring and the picture size it was made for.
    ring: Option<(ReadbackRing<u32>, (u32, u32))>,
    pacer: Pacer,
    complained: bool,
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
            pacer: Pacer::default(),
            complained: false,
        }
    }

    /// Downscale `frame` into a small texture and queue its copy to the CPU. Never waits for the GPU.
    fn capture(&mut self, r: &Renderer, frame: &RgbaTexture) -> GpuResult<()> {
        let bound = (self.request.0.clamp(1, MAX_SIZE.0), self.request.1.clamp(1, MAX_SIZE.1));
        let size = fit_within((frame.width, frame.height), bound);
        if self.ring.as_ref().is_none_or(|(_, s)| *s != size) {
            let plane = PlaneDesc {
                format: DXGI_FORMAT_R8G8B8A8_UNORM,
                width: size.0,
                height: size.1,
                bytes_per_texel: 4,
            };
            self.ring = Some((ReadbackRing::new(r.gpu.clone(), &[plane])?, size));
        }
        let small = self.down.run(r, frame, size.0, size.1)?;
        let number = self.numbers.fetch_add(1, Ordering::Relaxed);
        let (ring, _) = self.ring.as_mut().expect("made above");
        // A full ring drops its oldest frame, which is what an overwrite means here.
        ring.submit(&[&small.texture], number)?;
        // `small` goes back to the pool; the copy above was queued first, so the GPU reads it before any reuse.
        Ok(())
    }

    /// Map the frames the GPU finished and offer the newest. Never waits.
    fn collect(&mut self) {
        let Some((ring, (w, h))) = self.ring.as_mut() else {
            return;
        };
        let (w, h) = (*w, *h);
        let mut newest = None;
        // More than one is ready only after a stall; the older ones would be overwritten anyway.
        for _ in 0..ReadbackRing::<u32>::SLOTS {
            match ring.poll(false, |planes, number| pack(&planes[0], w, h, number)) {
                Ok(Some(f)) => newest = Some(f),
                Ok(None) => break,
                Err(e) => {
                    self.complain(&e.to_string());
                    break;
                }
            }
        }
        if let Some(f) = newest {
            self.shared.offer(f);
        }
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
        self.down = Downscaler::new();
        self.pacer.reset();
    }

    fn on_frame(&mut self, renderer: &Renderer, frame: &RgbaTexture, _capture_qpc: u64) {
        if self.hidden.load(Ordering::Relaxed) || self.shared.is_closed() {
            // Nobody sees the picture: no GPU work, and the frames in the ring would be stale on return.
            if let Some((ring, _)) = self.ring.as_mut() {
                ring.clear();
            }
            self.pacer.reset();
            return;
        }
        self.collect();
        if !self.pacer.accept(Instant::now()) {
            return;
        }
        if let Err(e) = self.capture(renderer, frame) {
            self.complain(&e.to_string());
        }
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
}

impl Drop for Preview {
    fn drop(&mut self) {
        Self::end(&mut lock(&self.run));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gpu::{AdapterKind, GpuDevice};

    fn frame(n: u32) -> Frame {
        Frame {
            number: n,
            bytes: vec![n as u8; 4],
        }
    }

    // --- wire format ---------------------------------------------------------------------------------------

    #[test]
    fn the_header_is_four_little_endian_words() {
        let h = Header {
            width: 960,
            height: 540,
            stride: 3840,
            frame: 0x0102_0304,
        };
        let b = h.encode();
        assert_eq!(b.len(), 16);
        assert_eq!(&b[0..4], &[0xC0, 0x03, 0, 0]);
        assert_eq!(&b[4..8], &[0x1C, 0x02, 0, 0]);
        assert_eq!(&b[8..12], &[0x00, 0x0F, 0, 0]);
        assert_eq!(&b[12..16], &[4, 3, 2, 1]);
        assert_eq!(Header::decode(&b), Some(h));
        assert_eq!(Header::decode(&b[..15]), None);
        assert_eq!(Header::decode(&[]), None);
    }

    #[test]
    fn pack_drops_the_row_padding() {
        // 3 pixels per row, rows 20 bytes apart in the mapped memory.
        let (pitch, w, h) = (20usize, 3u32, 2u32);
        let mut mem = vec![0xEEu8; pitch + 12];
        for y in 0..h as usize {
            for i in 0..12 {
                mem[y * pitch + i] = (y * 100 + i) as u8;
            }
        }
        let plane = MappedPlane::new(&mem, pitch, 12, h as usize);
        let f = pack(&plane, w, h, 7);
        assert_eq!(f.number, 7);
        assert_eq!(f.bytes.len(), HEADER_LEN + 24);
        assert_eq!(
            Header::decode(&f.bytes),
            Some(Header {
                width: 3,
                height: 2,
                stride: 12,
                frame: 7
            })
        );
        let pixels = &f.bytes[HEADER_LEN..];
        assert_eq!(&pixels[..12], &(0..12).collect::<Vec<u8>>()[..]);
        assert_eq!(&pixels[12..], &(100..112).collect::<Vec<u8>>()[..]);
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
            bytes: Header {
                width: 1,
                height: 0,
                stride: 4,
                frame: n,
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
        tap.capture(r, frame).expect("capture");
        let end = Instant::now() + Duration::from_secs(5);
        loop {
            tap.collect();
            if let Some(f) = lock(&shared.gate).take(Instant::now()) {
                return f;
            }
            assert!(Instant::now() < end, "the picture never arrived");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn pixel(f: &Frame, x: u32, y: u32) -> [u8; 4] {
        let h = Header::decode(&f.bytes).unwrap();
        let at = HEADER_LEN + (y * h.stride + x * 4) as usize;
        f.bytes[at..at + 4].try_into().unwrap()
    }

    #[test]
    fn a_sixteen_by_nine_frame_becomes_960_by_540_in_rgba_order() {
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
        assert_eq!((h.width, h.height, h.stride, h.frame), (960, 540, 3840, 41));
        assert_eq!(f.bytes.len(), HEADER_LEN + 3840 * 540);
        // R first, then G, B, A: no swap anywhere.
        assert_eq!(pixel(&f, 100, 300), [255, 0, 0, 255]);
        assert_eq!(pixel(&f, 800, 300), [0, 0, 255, 255]);
        // The top row of the source is the first row of the payload (not flipped).
        let top = pixel(&f, 100, 0);
        assert!(
            (top[0] as i32 - 128).abs() <= 2 && (top[1] as i32 - 128).abs() <= 2 && top[2] == 0,
            "the first row is the average of the green marker row and the red row below it: {top:?}"
        );
        assert_eq!(pixel(&f, 100, 539), [255, 0, 0, 255]);
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
                assert!((v - 127).abs() <= 2, "{request:?} column {x}: {v}");
            }
        }
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
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(shared.made.load(Ordering::Relaxed), 0);
        assert!(tap.ring.is_none(), "no GPU work while hidden");
        hidden.store(false, Ordering::Relaxed);
        let end = Instant::now() + Duration::from_secs(5);
        while shared.made.load(Ordering::Relaxed) == 0 {
            tap.on_frame(&r, &src, 0);
            assert!(Instant::now() < end, "no frame after the window came back");
            std::thread::sleep(Duration::from_millis(40));
        }
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
        assert_eq!((h.width, h.height, h.stride), (480, 270, 1920));
        assert_eq!(first.len(), HEADER_LEN + 1920 * 270);
        let bars = smpte_bars(640, 360);
        let reference = &bars[(10 * 640 + 10) * 3..(10 * 640 + 10) * 3 + 3];
        let at = HEADER_LEN + (7 * 1920 + 7 * 4);
        for c in 0..3 {
            assert!(
                (first[at + c] as i32 - reference[c] as i32).abs() <= 4,
                "channel {c}: preview {} vs bars {}",
                first[at + c],
                reference[c]
            );
        }
        assert_eq!(first[at + 3], 255);

        // Nothing is acknowledged, so the engine's frames pile up behind the one in flight.
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(lock(&got).len(), 1, "one frame in flight");
        assert_eq!(preview.frames_sent(), 1);
        preview.ack(h.frame);
        wait_for(2);
        let second = Header::decode(&lock(&got)[1]).unwrap();
        assert!(second.frame > h.frame, "newer frames overwrite, so numbers skip ahead");

        // Stop: no more frames, and the tap is gone from the engine.
        preview.stop(Some(&engine));
        preview.ack(second.frame);
        std::thread::sleep(Duration::from_millis(300));
        let n = lock(&got).len();
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(lock(&got).len(), n, "no frames after stop");
        engine.close();
    }

    #[test]
    fn a_size_change_rebuilds_the_ring_and_drops_the_old_pictures() {
        let r = warp();
        let src = r.texture(1920, 1080).unwrap();
        fill(&r, &src, |_, _| [5, 5, 5, 255]);
        let shared = Shared::new(ACK_TIMEOUT);
        let mut tap = PreviewTap::new(shared.clone(), Arc::new(AtomicU32::new(0)), Arc::new(AtomicBool::new(false)), (960, 540));
        tap.capture(&r, &src).unwrap();
        assert_eq!(tap.ring.as_ref().unwrap().1, (960, 540));
        tap.request = (480, 270);
        tap.capture(&r, &src).unwrap();
        assert_eq!(tap.ring.as_ref().unwrap().1, (480, 270));
        assert_eq!(tap.ring.as_ref().unwrap().0.pending(), 1, "the 960x540 picture is gone");
    }
}
