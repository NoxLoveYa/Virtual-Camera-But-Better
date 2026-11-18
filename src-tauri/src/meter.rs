//! The engine's meter (docs/AUTO-CONTROLS.md 2.4, 2.8 and 5.5): a statistics pass over the camera picture **before**
//! the filter chain, for the software auto of the camera's own controls.
//!
//! * [`Meter`] runs on the render thread right after the input convert. While [`Shared::metering`] is set it reduces
//!   the converted picture to the 16x16 image of linear-light means that Auto Adjust uses (the same
//!   `stats_downsample.hlsl` chain, [`StatsChain`]), copies it into its own [`ReadbackRing`] and maps it a frame or two
//!   later without waiting. At most one pass per [`MIN_GAP`] (about 15 Hz). Each mapped result is a [`MeterSample`]
//!   that goes into the one-slot [`Mailbox`]; the newest wins.
//! * [`SessionFacts`] is what the controller reads each tick besides the samples: the attached camera's token, the
//!   rate it was opened at, the capture rate over the last 2 s, whether the session is paused, and the QPC time of
//!   the last resume. [`FactsFeed`] keeps it current from the render loop.
//!
//! The GPU resources belong to the session's render loop and go with it: a lost GPU device ends the session, the
//! next session builds a new [`Meter`] on the new device, and samples resume as soon as `metering` is still set.
//!
//! [`Shared::metering`]: crate::engine::Engine::set_metering

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Direct3D11::ID3D11PixelShader;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R16G16B16A16_UNORM;

use crate::fx::auto_adjust::{Stats, StatsChain, GRID, STATS_HLSL};
use crate::gpu::readback::{PlaneDesc, ReadbackRing};
use crate::gpu::{GpuResult, Pass, Renderer, RgbaTexture};

/// Fewest time between two measurements the meter starts (about 15 Hz): the controller decides at 10 Hz.
pub const MIN_GAP: Duration = Duration::from_millis(66);

/// The span of the capture rate in [`SessionFacts::capture_fps`].
pub const FPS_SPAN: Duration = Duration::from_secs(2);

/// A rate needs at least this much data; until then [`SessionFacts::capture_fps`] is 0.
const FPS_MIN_SPAN: Duration = Duration::from_millis(500);

/// How often the capture rate is looked at (the render loop calls [`FactsFeed::update`] on every pass).
const FPS_STEP: Duration = Duration::from_millis(100);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------------------------------------------------------
// Samples

/// One measurement of the camera picture before the filter chain (5.5). `auto_controls::MeterSample` is this type.
#[derive(Clone, Debug)]
pub struct MeterSample {
    /// The hub token of the camera session that produced it (`0` when the session has no device controls).
    pub token: u64,
    /// Capture time of the measured frame (QPC, the clock of `vcam_protocol::qpc_now`).
    pub capture_qpc: u64,
    /// 16x16 linear means + coverage.
    pub stats: Stats,
}

/// A one-slot mailbox for the newest [`MeterSample`]: the render thread pushes, the controller's thread waits.
#[derive(Default)]
pub struct Mailbox {
    slot: Mutex<Option<MeterSample>>,
    cv: Condvar,
}

impl Mailbox {
    /// Put a sample in the slot (it replaces an older one) and wake the waiter.
    pub fn push(&self, sample: MeterSample) {
        *lock(&self.slot) = Some(sample);
        self.cv.notify_all();
    }

    /// The sample in the slot, if any; the slot is empty afterwards.
    pub fn take(&self) -> Option<MeterSample> {
        lock(&self.slot).take()
    }

    /// Wait up to `timeout` for a sample and take it.
    pub fn wait(&self, timeout: Duration) -> Option<MeterSample> {
        let deadline = Instant::now() + timeout;
        let mut slot = lock(&self.slot);
        loop {
            if let Some(s) = slot.take() {
                return Some(s);
            }
            let left = deadline.checked_duration_since(Instant::now())?;
            slot = self
                .cv
                .wait_timeout(slot, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// Empty the slot.
    pub fn clear(&self) {
        lock(&self.slot).take();
    }
}

// ---------------------------------------------------------------------------------------------------------------
// The measurement (render thread)

/// Admits at most one measurement per [`MIN_GAP`].
#[derive(Default)]
struct Throttle {
    last: Option<Instant>,
}

impl Throttle {
    fn admit(&mut self, now: Instant) -> bool {
        match self.last {
            Some(t) if now.saturating_duration_since(t) < MIN_GAP => false,
            _ => {
                self.last = Some(now);
                true
            }
        }
    }
}

/// What travels with a copy in the ring.
#[derive(Clone, Copy)]
struct Tag {
    token: u64,
    capture_qpc: u64,
}

struct Gpu {
    ps: ID3D11PixelShader,
    ring: ReadbackRing<Tag>,
    chain: Option<StatsChain>,
}

/// Compile the statistics shader with the others, so the first measurement does not.
pub fn warm_up(r: &Renderer) -> GpuResult<()> {
    r.shaders.pixel_source("stats_downsample", STATS_HLSL)?;
    Ok(())
}

/// The statistics pass of one render session. Made empty; its GPU resources are made when first needed and are dropped
/// with it (a session ends when the GPU device is lost).
#[derive(Default)]
pub struct Meter {
    gpu: Option<Gpu>,
    throttle: Throttle,
    /// The last frame was seen with metering on (so the switch to off is noticed once).
    active: bool,
}

impl Meter {
    pub fn new() -> Meter {
        Meter::default()
    }

    /// Called for every frame, with the converted picture, before the filter chain. With `on` false it does nothing
    /// (after dropping whatever is in flight, once). With `on` true it maps the measurements that finished (into
    /// `out`, without waiting) and, when [`MIN_GAP`] has passed since the last one, starts a new one.
    ///
    /// An error is the GPU's: the caller decides whether it is a lost device.
    #[allow(clippy::too_many_arguments)]
    pub fn frame(
        &mut self,
        r: &Renderer,
        picture: &RgbaTexture,
        on: bool,
        token: u64,
        capture_qpc: u64,
        now: Instant,
        out: &Mailbox,
    ) -> GpuResult<()> {
        if !on {
            if self.active {
                self.reset(out);
            }
            return Ok(());
        }
        self.active = true;
        if self.gpu.is_none() {
            self.gpu = Some(Gpu {
                ps: r.shaders.pixel_source("stats_downsample", STATS_HLSL)?,
                ring: ReadbackRing::new(
                    r.gpu.clone(),
                    &[PlaneDesc {
                        format: DXGI_FORMAT_R16G16B16A16_UNORM,
                        width: GRID as u32,
                        height: GRID as u32,
                        bytes_per_texel: 8,
                    }],
                )?,
                chain: None,
            });
        }
        let gpu = self.gpu.as_mut().expect("made above");
        while let Some(sample) = gpu.ring.poll(false, |planes, tag| MeterSample {
            token: tag.token,
            capture_qpc: tag.capture_qpc,
            stats: Stats::from_plane(&planes[0]),
        })? {
            out.push(sample);
        }
        if !self.throttle.admit(now) {
            return Ok(());
        }
        let size = (picture.width, picture.height);
        if gpu.chain.as_ref().map(|c| c.input) != Some(size) {
            gpu.chain = Some(StatsChain::new(r, size)?);
        }
        let chain = gpu.chain.as_ref().expect("made above");
        let mut source = picture.srv.clone();
        for stage in &chain.stages {
            r.draw(&Pass {
                ps: &gpu.ps,
                srvs: &[Some(source)],
                constants: Some(&stage.constants),
                target: &stage.rtv,
                width: stage.size.0,
                height: stage.size.1,
            });
            source = stage.srv.clone();
        }
        let last = chain.stages.last().expect("at least one stage");
        gpu.ring.submit(&[&last.texture], Tag { token, capture_qpc })?;
        Ok(())
    }

    /// Forget what is in flight and what waits in `out` (metering went off, or the session paused): a measurement
    /// taken before must not reach the controller after.
    pub fn reset(&mut self, out: &Mailbox) {
        if let Some(g) = self.gpu.as_mut() {
            g.ring.clear();
        }
        self.throttle = Throttle::default();
        self.active = false;
        out.clear();
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Session facts

/// What the controller reads each tick besides the samples (5.5). The engine gives the raw data: `resumed_qpc` is a QPC
/// time, not seconds on the controller's clock; the runner (A4) converts it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SessionFacts {
    /// The hub token of the attached camera, `None` while no camera session with device controls runs.
    pub token: Option<u64>,
    /// `F`: the rate the camera was opened at (the media type's), 0 while no session runs.
    pub target_fps: f64,
    /// Frames the camera delivered per second over the last [`FPS_SPAN`]; 0 until half a second of data exists.
    pub capture_fps: f64,
    /// The session is paused (frames are read and dropped; nothing is measured).
    pub paused: bool,
    /// QPC time of the last resume from a pause; samples captured before it are invalid. 0 when it never paused.
    pub resumed_qpc: u64,
}

impl SessionFacts {
    /// No session.
    pub const CLOSED: SessionFacts = SessionFacts {
        token: None,
        target_fps: 0.0,
        capture_fps: 0.0,
        paused: false,
        resumed_qpc: 0,
    };
}

/// The rate of a counter over a sliding window.
#[derive(Default)]
struct FpsWindow {
    marks: VecDeque<(Instant, u64)>,
}

impl FpsWindow {
    /// Note the counter `captured` at `now`.
    fn note(&mut self, now: Instant, captured: u64) {
        self.marks.push_back((now, captured));
        // The oldest mark stays while the one after it is not yet a whole span old.
        while self.marks.len() > 2 && now.saturating_duration_since(self.marks[1].0) >= FPS_SPAN {
            self.marks.pop_front();
        }
    }

    /// Per second, over what is kept; 0 with less than [`FPS_MIN_SPAN`] of data.
    fn fps(&self) -> f64 {
        let (Some(&(t0, c0)), Some(&(t1, c1))) = (self.marks.front(), self.marks.back()) else {
            return 0.0;
        };
        let span = t1.saturating_duration_since(t0);
        if span < FPS_MIN_SPAN {
            return 0.0;
        }
        c1.saturating_sub(c0) as f64 / span.as_secs_f64()
    }
}

/// Keeps a [`SessionFacts`] cell current for one render session, and resets it to [`SessionFacts::CLOSED`] when the
/// session ends.
pub struct FactsFeed<'a> {
    cell: &'a Mutex<SessionFacts>,
    window: FpsWindow,
    facts: SessionFacts,
    last: Option<Instant>,
}

impl<'a> FactsFeed<'a> {
    /// A session of the camera `token` opened at `target_fps`.
    pub fn new(cell: &'a Mutex<SessionFacts>, token: Option<u64>, target_fps: f64) -> FactsFeed<'a> {
        let facts = SessionFacts {
            token,
            target_fps,
            ..SessionFacts::CLOSED
        };
        *lock(cell) = facts;
        FactsFeed {
            cell,
            window: FpsWindow::default(),
            facts,
            last: None,
        }
    }

    /// The camera token of this session.
    pub fn token(&self) -> Option<u64> {
        self.facts.token
    }

    /// One pass of the render loop: `captured` reads the camera's frame counter (called only when the rate is due),
    /// `paused` is the state the loop acts on, `qpc` the time now. A change of the pause state is published at once;
    /// the rate every [`FPS_STEP`].
    pub fn update(&mut self, now: Instant, captured: impl FnOnce() -> u64, paused: bool, qpc: u64) {
        let mut changed = false;
        if paused != self.facts.paused {
            self.facts.paused = paused;
            if !paused {
                self.facts.resumed_qpc = qpc;
            }
            changed = true;
        }
        if self
            .last
            .is_none_or(|t| now.saturating_duration_since(t) >= FPS_STEP)
        {
            self.last = Some(now);
            self.window.note(now, captured());
            let fps = self.window.fps();
            if fps != self.facts.capture_fps {
                self.facts.capture_fps = fps;
                changed = true;
            }
        }
        if changed {
            *lock(self.cell) = self.facts;
        }
    }
}

impl Drop for FactsFeed<'_> {
    fn drop(&mut self) {
        *lock(self.cell) = SessionFacts::CLOSED;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    fn sample(n: u64) -> MeterSample {
        MeterSample {
            token: 1,
            capture_qpc: n,
            stats: Stats { cells: Vec::new() },
        }
    }

    #[test]
    fn the_throttle_admits_one_measurement_per_66_ms_at_most() {
        let base = Instant::now();
        let mut t = Throttle::default();
        assert!(t.admit(at(base, 0)), "the first one goes at once");
        assert!(!t.admit(at(base, 16)));
        assert!(!t.admit(at(base, 33)));
        assert!(!t.admit(at(base, 65)));
        assert!(t.admit(at(base, 66)));
        assert!(!t.admit(at(base, 100)), "the gap counts from the last admitted one");
        assert!(t.admit(at(base, 132)));
        // 60 fps for 10 s: 1 frame in 4 or 5, 15 a second or fewer
        let mut t = Throttle::default();
        let n = (0..600)
            .filter(|i| t.admit(base + Duration::from_secs_f64(*i as f64 / 60.0)))
            .count();
        assert!((140..=150).contains(&n), "{n} measurements in 10 s");
    }

    #[test]
    fn the_mailbox_keeps_the_newest_and_wakes_a_waiter() {
        let mb = std::sync::Arc::new(Mailbox::default());
        assert!(mb.take().is_none());
        mb.push(sample(1));
        mb.push(sample(2));
        assert_eq!(mb.take().unwrap().capture_qpc, 2);
        assert!(mb.take().is_none(), "taking empties the slot");
        let t = Instant::now();
        assert!(mb.wait(Duration::from_millis(30)).is_none());
        assert!(t.elapsed() >= Duration::from_millis(30));
        let (m, started) = (mb.clone(), Instant::now());
        let h = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(40));
            m.push(sample(3));
        });
        assert_eq!(mb.wait(Duration::from_secs(5)).unwrap().capture_qpc, 3);
        assert!(started.elapsed() < Duration::from_secs(2));
        h.join().unwrap();
        mb.push(sample(4));
        mb.clear();
        assert!(mb.take().is_none());
    }

    #[test]
    fn the_capture_rate_is_measured_over_two_seconds() {
        let base = Instant::now();
        let mut w = FpsWindow::default();
        w.note(base, 0);
        assert_eq!(w.fps(), 0.0, "one mark says nothing");
        w.note(at(base, 300), 9);
        assert_eq!(w.fps(), 0.0, "less than half a second of data");
        w.note(at(base, 600), 18);
        assert!((w.fps() - 30.0).abs() < 1e-9);
        // 30 fps for 5 s, then the camera stops: the rate falls to 0 within the window
        for i in 1..=50u64 {
            w.note(at(base, 100 * i), i * 3);
        }
        assert!((w.fps() - 30.0).abs() < 0.5, "{}", w.fps());
        assert!(w.marks.len() <= 24, "the window keeps about 2 s of marks");
        for i in 1..=21u64 {
            w.note(at(base, 5000 + 100 * i), 150);
        }
        assert!(w.fps() < 1.0, "{}", w.fps());
    }

    #[test]
    fn facts_follow_the_session_and_reset_when_it_ends() {
        let cell = Mutex::new(SessionFacts::CLOSED);
        let base = Instant::now();
        {
            let mut feed = FactsFeed::new(&cell, Some(7), 30.0);
            let f = *lock(&cell);
            assert_eq!(f.token, Some(7));
            assert_eq!(f.target_fps, 30.0);
            assert_eq!(f.capture_fps, 0.0);
            assert!(!f.paused);
            for i in 0..=30u64 {
                feed.update(at(base, i * 33), || i, false, 1_000);
            }
            let f = *lock(&cell);
            assert!((f.capture_fps - 30.3).abs() < 1.0, "{}", f.capture_fps);
            // a pause is published at once; the resume time is the time of the resume
            feed.update(at(base, 1_010), || 31, true, 5_000);
            assert!(lock(&cell).paused);
            assert_eq!(lock(&cell).resumed_qpc, 0);
            feed.update(at(base, 1_020), || 31, false, 9_000);
            let f = *lock(&cell);
            assert!(!f.paused);
            assert_eq!(f.resumed_qpc, 9_000);
            feed.update(at(base, 1_030), || 31, false, 9_500);
            assert_eq!(lock(&cell).resumed_qpc, 9_000, "only a resume moves it");
        }
        assert_eq!(*lock(&cell), SessionFacts::CLOSED);
    }
}
