//! Samples for the consumer: a bounded pool that gets its buffers back, and the [`FrameSink`] the pump copies into.
//!
//! A fresh sample over 1 MB is fresh memory: the first write into it page-faults every page in again, for every
//! frame. [`SamplePool`] therefore keeps its samples: each is a tracked sample (`MFCreateTrackedSample`) whose
//! owner callback is invoked when Frame Server and the consumers have let go of it, and the callback puts the
//! sample, buffer and all, back on a free list for the next frame.
//!
//! Media Foundation's own video sample allocator (`MFCreateVideoSampleAllocatorEx`, system memory) was tried and
//! does not do this job: when all its samples are out, `AllocateSample` blocks for a full second before it gives
//! up, and what it hands out after a release is not the same memory again.
//!
//! The pool never makes the pump wait. It makes at most [`POOL_MAX`] samples, and only when the free list is
//! empty; when all of them are still held (Frame Server or a consumer keeping samples longer than expected) or a
//! pooled sample cannot be made, the next sample is a plain one (`MFCreateSample` + `MFCreate2DMediaBuffer`),
//! freed when it is let go. The pool belongs to one pump thread and ends with it, so it ends with the stream: its
//! `Drop` closes it and empties the free list, and a sample that a consumer still holds is freed when released. A
//! stream started with another format makes a new pump and with it a new pool.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use windows::Win32::Foundation::{E_NOTIMPL, E_UNEXPECTED};
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer2, IMFAsyncCallback, IMFAsyncCallback_Impl, IMFAsyncResult, IMFMediaBuffer,
    IMFSample, IMFTrackedSample, MF2DBuffer_LockFlags_Write, MFCreate2DMediaBuffer, MFCreateSample,
    MFCreateTrackedSample, MFSampleExtension_CleanPoint, MFSampleExtension_Token,
    MF_E_INVALIDMEDIATYPE,
};
use windows_core::{implement, Error, IUnknown, Interface, Ref, Result, GUID};

use crate::frames::{FrameBuf, FrameSink};
use crate::guard::{guard, LockExt};
use crate::mediatype::FrameFormat;

/// Samples the pool makes at most. A stream needs up to three in flight: `dest()` runs on every live poll, so the
/// sample for the next frame is already locked and pending through the inter-frame gap; the consumer holds the last
/// frame posted, and the one before it until the next arrives and its callback has brought it back. At 4K NV12
/// (12.4 MB a sample) the cap bounds the pool to 75 MB, and only when the consumer keeps every sample.
pub const POOL_MAX: usize = 6;

/// What the pool did: samples taken from the free list, pooled samples made (each one allocates a buffer), and
/// samples made outside the pool because every pooled one was out.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PoolStats {
    pub reused: u64,
    pub created: u64,
    pub plain: u64,
}

/// Attribute of a pooled sample that the pool gave up (see `SamplePool::retire`).
const RETIRED: GUID = GUID::from_u128(0x6b0f_5f1a_7a31_4c6e_9d52_b3c0_0000_0001);

/// A sample that the free list owns. Media Foundation samples are free-threaded; windows-rs does not say so.
struct Idle(IMFSample);
// SAFETY: IMFSample objects are free-threaded (they are handed to arbitrary MF work threads).
unsafe impl Send for Idle {}

struct PoolInner {
    free: Mutex<Vec<Idle>>,
    closed: AtomicBool,
    /// Pooled samples that exist: on the free list or out.
    owned: AtomicUsize,
}

/// The owner callback of the pooled samples: puts a sample that was let go back, unless the pool has ended.
#[implement(IMFAsyncCallback)]
struct Recycler {
    inner: Arc<PoolInner>,
}

impl Recycler {
    /// The callback is code of this DLL that a consumer's sample can still point at after the stream ended, so it
    /// counts as a live object: the DLL is not unloaded under it.
    fn new(inner: Arc<PoolInner>) -> Recycler {
        crate::object_created();
        Recycler { inner }
    }
}

impl Drop for Recycler {
    fn drop(&mut self) {
        crate::object_dropped();
    }
}

impl IMFAsyncCallback_Impl for Recycler_Impl {
    fn GetParameters(&self, _flags: *mut u32, _queue: *mut u32) -> Result<()> {
        // the defaults: no flags, the standard work queue
        Err(Error::from(E_NOTIMPL))
    }

    fn Invoke(&self, result: Ref<IMFAsyncResult>) -> Result<()> {
        guard("Pool::Invoke", || {
            let result = result.ok()?;
            let sample: IMFSample = unsafe { result.GetObject()? }.cast()?;
            if unsafe { sample.GetUINT32(&RETIRED) }.is_ok() {
                // the pool gave this one up: it is let go here and freed
                self.inner.owned.fetch_sub(1, Ordering::AcqRel);
                return Ok(());
            }
            // What a consumer or Frame Server set on the sample is not for the next frame: attributes (and with
            // them foreign objects and the token, which would otherwise live until the sample is reused or the pool
            // ends) and flags go now.
            unsafe {
                let _ = sample.DeleteAllItems();
                let _ = sample.SetSampleFlags(0);
            }
            // closed is set and the list emptied under this lock: a sample comes back before that (and is emptied
            // out) or after it (and is let go here)
            let mut free = self.inner.free.lock_safe();
            if !self.inner.closed.load(Ordering::Acquire) {
                free.push(Idle(sample));
            }
            Ok(())
        })
    }
}

/// See the module documentation.
pub struct SamplePool {
    inner: Arc<PoolInner>,
    recycler: IMFAsyncCallback,
    fmt: FrameFormat,
    max: usize,
    stats: PoolStats,
}

impl SamplePool {
    /// A pool for samples of `fmt`. Nothing is allocated until the first sample is asked for.
    pub fn new(fmt: FrameFormat) -> SamplePool {
        SamplePool::with_max(fmt, POOL_MAX)
    }

    pub fn with_max(fmt: FrameFormat, max: usize) -> SamplePool {
        let inner = Arc::new(PoolInner {
            free: Mutex::new(Vec::new()),
            closed: AtomicBool::new(false),
            owned: AtomicUsize::new(0),
        });
        let recycler: IMFAsyncCallback = Recycler::new(inner.clone()).into();
        SamplePool {
            inner,
            recycler,
            fmt,
            max,
            stats: PoolStats::default(),
        }
    }

    /// Next sample: one that was let go, else a new pooled one while the pool is below its size, else a plain one.
    /// Never waits.
    pub fn sample(&mut self) -> Result<IMFSample> {
        // a sample that is on the free list has had its owner callback cleared when it was called
        loop {
            let idle = self.inner.free.lock_safe().pop();
            let Some(Idle(s)) = idle else { break };
            if Self::still_ours(&s) && self.arm(&s).is_ok() {
                self.stats.reused += 1;
                return Ok(s);
            }
            // a sample that was changed while it was out is not used again, and not kept: it is freed here (it is
            // not armed, so nothing brings it back)
            self.inner.owned.fetch_sub(1, Ordering::AcqRel);
        }
        if self.inner.owned.load(Ordering::Acquire) < self.max {
            match self.create() {
                Ok(s) => {
                    self.inner.owned.fetch_add(1, Ordering::AcqRel);
                    self.stats.created += 1;
                    return Ok(s);
                }
                Err(e) => {
                    crate::log::log_sampled(crate::log::slot::POOL_FALLBACK, 600, || {
                        format!("sample pool: no pooled sample ({e}), plain sample instead")
                    });
                }
            }
        } else {
            crate::log::log_sampled(crate::log::slot::POOL_FALLBACK, 600, || {
                "sample pool: every sample is out, plain sample instead".to_string()
            });
        }
        self.plain()
    }

    /// Make `sample` come back to the pool when everybody has let go of it.
    fn arm(&self, sample: &IMFSample) -> Result<()> {
        let tracked: IMFTrackedSample = sample.cast()?;
        unsafe { tracked.SetAllocator(&self.recycler, None) }
    }

    /// A sample from the free list still has the one 2D buffer it was made with (a consumer can have added or
    /// swapped buffers).
    fn still_ours(sample: &IMFSample) -> bool {
        unsafe {
            sample.GetBufferCount().is_ok_and(|n| n == 1)
                && sample
                    .GetBufferByIndex(0)
                    .is_ok_and(|b| b.cast::<IMF2DBuffer2>().is_ok())
        }
    }

    /// `sample` came from [`sample`](Self::sample) and cannot be used (its buffer cannot be locked, say): if it is
    /// a pooled one, it leaves the pool. It is marked, and `Recycler::Invoke` frees it instead of keeping it when
    /// it is let go. (The callback of a tracked sample cannot be taken off again.)
    ///
    /// `SetUINT32` can fail only when memory runs out. The sample then stays unmarked, comes back to the free list
    /// once, fails `Locked::new` again on its next use and is retired again: at worst one more plain sample, never
    /// a stall. That is accepted rather than tracked.
    fn retire(&self, sample: &IMFSample) {
        if sample.cast::<IMFTrackedSample>().is_ok() {
            let _ = unsafe { sample.SetUINT32(&RETIRED, 1) };
        }
    }

    /// A new pooled sample, armed.
    fn create(&self) -> Result<IMFSample> {
        let tracked = unsafe { MFCreateTrackedSample()? };
        let sample: IMFSample = tracked.cast()?;
        unsafe { sample.AddBuffer(&self.new_buffer()?)? };
        self.arm(&sample)?;
        Ok(sample)
    }

    fn new_buffer(&self) -> Result<IMFMediaBuffer> {
        unsafe {
            MFCreate2DMediaBuffer(
                self.fmt.width,
                self.fmt.height,
                self.fmt.pixel.buffer_fourcc(),
                false,
            )
        }
    }

    /// A sample that is not from the pool.
    pub fn plain(&mut self) -> Result<IMFSample> {
        self.stats.plain += 1;
        unsafe {
            let sample = MFCreateSample()?;
            sample.AddBuffer(&self.new_buffer()?)?;
            Ok(sample)
        }
    }

    pub fn stats(&self) -> PoolStats {
        self.stats
    }

    /// A handle that says whether anything of the pool is still alive (tests).
    #[cfg(test)]
    pub fn watch(&self) -> PoolWatch {
        PoolWatch(Arc::downgrade(&self.inner))
    }
}

/// See [`SamplePool::watch`].
#[cfg(test)]
pub struct PoolWatch(std::sync::Weak<PoolInner>);

#[cfg(test)]
impl PoolWatch {
    /// The pool, or a sample that it made and a consumer still holds, is alive.
    pub fn alive(&self) -> bool {
        self.0.strong_count() > 0
    }
}

impl Drop for SamplePool {
    fn drop(&mut self) {
        let mut free = self.inner.free.lock_safe();
        self.inner.closed.store(true, Ordering::Release);
        free.clear();
    }
}

/// A sample whose buffer is locked for writing.
struct Locked {
    sample: IMFSample,
    buffer: IMFMediaBuffer,
    b2: Option<IMF2DBuffer2>,
    scan0: *mut u8,
    pitch: usize,
    len: usize,
    /// Bytes to declare as the buffer's content.
    content: u32,
}

impl Locked {
    /// Lock the buffer of `sample` and check that a frame of `fmt` fits.
    fn new(sample: IMFSample, fmt: &FrameFormat) -> Result<Locked> {
        let buffer = unsafe {
            if sample.GetBufferCount()? != 1 {
                return Err(Error::from(MF_E_INVALIDMEDIATYPE));
            }
            sample.GetBufferByIndex(0)?
        };
        let row_bytes = fmt.pixel.stride(fmt.width) as usize;
        let rows = fmt.pixel.rows(fmt.height) as usize;
        let b2 = buffer.cast::<IMF2DBuffer2>().ok();
        let (mut scan0, mut pitch_i, mut start, mut len32) = (
            std::ptr::null_mut::<u8>(),
            0i32,
            std::ptr::null_mut::<u8>(),
            0u32,
        );
        let (pitch, len, content) = match &b2 {
            Some(b2) => {
                unsafe {
                    b2.Lock2DSize(
                        MF2DBuffer_LockFlags_Write,
                        &mut scan0,
                        &mut pitch_i,
                        &mut start,
                        &mut len32,
                    )?
                };
                let content = unsafe { b2.GetContiguousLength() }.unwrap_or(len32);
                (pitch_i.max(0) as usize, len32 as usize, content)
            }
            None => {
                // a plain memory buffer: the frame is tight
                let mut max = 0u32;
                unsafe { buffer.Lock(&mut scan0, Some(&mut max as *mut u32), None)? };
                start = scan0;
                (row_bytes, max as usize, (row_bytes * rows) as u32)
            }
        };
        let l = Locked {
            sample,
            buffer,
            b2,
            scan0,
            pitch,
            len,
            content,
        };
        // an invalid layout drops `l`, which unlocks
        let ok = !l.scan0.is_null()
            && l.pitch >= row_bytes
            && l.pitch.checked_mul(rows).is_some_and(|n| n <= l.len)
            && start == l.scan0;
        if !ok {
            return Err(Error::from(MF_E_INVALIDMEDIATYPE));
        }
        Ok(l)
    }

    fn data(&mut self) -> &mut [u8] {
        // SAFETY: `scan0` is the start of `len` bytes that stay valid and exclusively ours while the lock is held,
        // and the lock is held until `unlock` (`self.scan0` is null after it).
        unsafe { std::slice::from_raw_parts_mut(self.scan0, self.len) }
    }

    fn unlock(&mut self) -> Result<()> {
        if self.scan0.is_null() {
            return Ok(());
        }
        self.scan0 = std::ptr::null_mut();
        match &self.b2 {
            Some(b2) => unsafe { b2.Unlock2D() },
            None => unsafe { self.buffer.Unlock() },
        }
    }

    fn finish(mut self, fmt: &FrameFormat, ts: i64, token: Option<IUnknown>) -> Result<IMFSample> {
        self.unlock()?;
        unsafe {
            self.buffer.SetCurrentLength(self.content)?;
            self.sample.SetSampleTime(ts)?;
            self.sample.SetSampleDuration(fmt.frame_duration())?;
            self.sample.SetUINT32(&MFSampleExtension_CleanPoint, 1)?;
            // (a pooled sample comes back without the previous frame's token: `Recycler::Invoke` cleared it)
            if let Some(t) = token {
                self.sample.SetUnknown(&MFSampleExtension_Token, &t)?;
            }
        }
        Ok(self.sample.clone())
    }
}

impl Drop for Locked {
    fn drop(&mut self) {
        let _ = self.unlock();
    }
}

/// The pump's [`FrameSink`]: a sample with a locked buffer, from the pool. [`finish`](Self::finish) turns the
/// filled buffer into a sample ready to post.
pub struct SampleSink {
    fmt: FrameFormat,
    pool: SamplePool,
    pending: Option<Locked>,
}

impl SampleSink {
    pub fn new(fmt: FrameFormat) -> SampleSink {
        SampleSink::with_pool(SamplePool::new(fmt), fmt)
    }

    pub fn with_pool(pool: SamplePool, fmt: FrameFormat) -> SampleSink {
        SampleSink {
            fmt,
            pool,
            pending: None,
        }
    }

    pub fn stats(&self) -> PoolStats {
        self.pool.stats()
    }

    #[cfg(test)]
    pub fn pool(&self) -> &SamplePool {
        &self.pool
    }

    fn lock_next(&mut self) -> Result<Locked> {
        let s = self.pool.sample()?;
        match Locked::new(s.clone(), &self.fmt) {
            Ok(l) => Ok(l),
            Err(e) => {
                // a sample of a shape we cannot write (a pooled one stays out of the pool, or it would fail every
                // frame): do not depend on the pool for this frame
                self.pool.retire(&s);
                drop(s);
                crate::log::log_sampled(crate::log::slot::POOL_FALLBACK, 600, || {
                    format!("sample pool: sample not usable ({e}), plain sample instead")
                });
                Locked::new(self.pool.plain()?, &self.fmt)
            }
        }
    }

    /// The sample the last poll filled, stamped `ts` (100 ns units) and with `token`. Its buffer is unlocked.
    pub fn finish(&mut self, ts: i64, token: Option<IUnknown>) -> Result<IMFSample> {
        let l = self
            .pending
            .take()
            .ok_or_else(|| Error::from(E_UNEXPECTED))?;
        l.finish(&self.fmt, ts, token)
    }
}

impl FrameSink for SampleSink {
    fn buffer(&mut self) -> Option<FrameBuf<'_>> {
        if self.pending.is_none() {
            match self.lock_next() {
                Ok(l) => self.pending = Some(l),
                Err(e) => {
                    crate::log::log_sampled(crate::log::slot::SAMPLE_FAILED, 600, || {
                        format!("no sample could be made: {e}")
                    });
                    return None;
                }
            }
        }
        let l = self.pending.as_mut()?;
        let pitch = l.pitch;
        Some(FrameBuf {
            data: l.data(),
            pitch,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mediatype::PixelFormat;
    use std::collections::HashSet;
    use std::time::{Duration, Instant};
    use windows::Win32::Media::MediaFoundation::{
        IMFAttributes, MFCreateAttributes, MFStartup, MFSTARTUP_FULL, MF_VERSION,
    };

    fn mf() {
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL).unwrap() };
    }

    fn fmt(pixel: PixelFormat, w: u32, h: u32) -> FrameFormat {
        FrameFormat {
            pixel,
            width: w,
            height: h,
            fps: 30,
        }
    }

    /// Fill the first byte of the next sample with `n` and take the sample, the way the pump does.
    fn take(sink: &mut SampleSink, n: u8) -> IMFSample {
        let b = sink.buffer().expect("a buffer");
        b.data[0] = n;
        sink.finish(i64::from(n), None).unwrap()
    }

    /// Where the sample's pixels live, and the first byte.
    fn address(s: &IMFSample) -> (usize, u8) {
        unsafe {
            // through the 2D interface: a 1D `Lock` of a 2D buffer may hand out a temporary copy
            let b2: IMF2DBuffer2 = s.GetBufferByIndex(0).unwrap().cast().unwrap();
            let (mut p, mut pitch) = (std::ptr::null_mut::<u8>(), 0i32);
            let (mut start, mut len) = (std::ptr::null_mut::<u8>(), 0u32);
            b2.Lock2DSize(
                MF2DBuffer_LockFlags_Write,
                &mut p,
                &mut pitch,
                &mut start,
                &mut len,
            )
            .unwrap();
            let r = (p as usize, *p);
            b2.Unlock2D().unwrap();
            r
        }
    }

    /// Give the work queue that returns samples a moment: the callback is not run by the thread that lets go.
    fn until(what: &str, mut done: impl FnMut() -> bool) {
        let began = Instant::now();
        while !done() {
            assert!(
                began.elapsed() < Duration::from_secs(3),
                "never happened: {what}"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn a_pool_hands_the_same_buffers_out_again_and_allocates_nothing_in_steady_state() {
        mf();
        for f in [
            fmt(PixelFormat::Nv12, 1920, 1080),
            fmt(PixelFormat::Rgb32, 1280, 720),
        ] {
            let mut sink = SampleSink::new(f);
            let mut buffers = HashSet::new();
            for n in 0..300u32 {
                // each sample is let go before the next one is asked for, like a consumer that keeps up
                let s = take(&mut sink, n as u8);
                let (addr, first) = address(&s);
                assert_eq!(first, n as u8, "the frame is in the sample");
                buffers.insert(addr);
                drop(s);
                // the sample comes back on a work queue
                let want = sink.pool.inner.owned.load(Ordering::Acquire);
                until("the sample came back", || {
                    sink.pool.inner.free.lock_safe().len() == want
                });
            }
            let st = sink.stats();
            assert_eq!(st.plain, 0, "{f:?}: a plain sample was made: {st:?}");
            assert_eq!(
                st.created, 1,
                "{f:?}: more than one buffer allocated: {st:?}"
            );
            assert_eq!(st.reused, 299);
            assert_eq!(buffers.len(), 1, "{f:?}: the buffer moved");
        }
    }

    #[test]
    fn with_a_consumer_that_holds_one_sample_two_buffers_are_enough_for_ever() {
        mf();
        let f = fmt(PixelFormat::Nv12, 1280, 720);
        let mut sink = SampleSink::new(f);
        let mut previous: Option<IMFSample> = None;
        let mut buffers = HashSet::new();
        for n in 0..200u32 {
            let s = take(&mut sink, n as u8);
            buffers.insert(address(&s).0);
            // the consumer is still reading the last one when the next arrives
            drop(previous.replace(s));
            let want = sink.pool.inner.owned.load(Ordering::Acquire) - 1;
            until("the sample came back", || {
                sink.pool.inner.free.lock_safe().len() >= want
            });
        }
        let st = sink.stats();
        assert_eq!((st.plain, st.created), (0, 2), "{st:?}");
        assert_eq!(buffers.len(), 2);
    }

    /// The pump's rhythm with a consumer that keeps the last sample until the next one arrives: `dest()` asks for
    /// the buffer on a live poll, the sample stays locked and pending through the inter-frame gap, then the frame is
    /// copied in, finished and posted, and the consumer lets go of the one before. Nothing waits for the callback
    /// that brings a sample back.
    #[test]
    fn a_pending_sample_through_the_gap_and_a_consumer_holding_the_last_one_need_three_pooled_samples(
    ) {
        mf();
        let f = fmt(PixelFormat::Nv12, 1280, 720);
        let mut sink = SampleSink::new(f);
        let mut previous: Option<IMFSample> = None;
        for n in 0..120u32 {
            // the poll that finds no frame yet: the buffer is taken and held
            let b = sink.buffer().expect("a buffer");
            std::thread::sleep(Duration::from_millis(2));
            // the frame comes
            b.data[0] = n as u8;
            let s = sink.finish(i64::from(n), None).unwrap();
            assert_eq!(address(&s).1, n as u8, "the frame is in the sample");
            // it reaches the consumer, which then lets the last one go
            drop(previous.replace(s));
        }
        let st = sink.stats();
        assert_eq!(st.plain, 0, "a plain sample was made: {st:?}");
        assert!(
            (2..=3).contains(&st.created),
            "pool samples made: {st:?}, wanted at most 3"
        );
        assert!(st.reused >= 117, "{st:?}");
    }

    #[test]
    fn samples_carry_their_time_length_and_the_token_of_their_own_frame_only() {
        mf();
        let f = fmt(PixelFormat::Nv12, 640, 360);
        let mut sink = SampleSink::new(f);
        let token: IUnknown = {
            let mut a: Option<IMFAttributes> = None;
            unsafe { MFCreateAttributes(&mut a, 0).unwrap() };
            a.unwrap().cast().unwrap()
        };
        for round in 0..6i64 {
            // an odd round has a token, an even one has none: the pooled sample must not show the one before
            let t = (round % 2 == 1).then(|| token.clone());
            let had = t.is_some();
            let b = sink.buffer().unwrap();
            b.data[0] = 1;
            let s = sink.finish(1000 + round, t).unwrap();
            unsafe {
                assert_eq!(s.GetSampleTime().unwrap(), 1000 + round);
                assert_eq!(s.GetSampleDuration().unwrap(), f.frame_duration());
                assert_eq!(s.GetUINT32(&MFSampleExtension_CleanPoint).unwrap(), 1);
                assert_eq!(
                    s.GetUnknown::<IUnknown>(&MFSampleExtension_Token).is_ok(),
                    had,
                    "round {round}: token"
                );
                assert_eq!(s.GetTotalLength().unwrap(), 640 * 360 * 3 / 2);
            }
            drop(s);
            let want = sink.pool.inner.owned.load(Ordering::Acquire);
            until("the sample came back", || {
                sink.pool.inner.free.lock_safe().len() == want
            });
        }
        assert_eq!(sink.stats().plain, 0);
        assert_eq!(sink.stats().created, 1);
    }

    #[test]
    fn a_pool_with_every_sample_held_falls_back_to_plain_samples_and_never_waits() {
        mf();
        let f = fmt(PixelFormat::Nv12, 1280, 720);
        let mut sink = SampleSink::with_pool(SamplePool::with_max(f, 2), f);
        let began = Instant::now();
        let held: Vec<IMFSample> = (0..6).map(|n| take(&mut sink, n)).collect();
        assert!(
            began.elapsed() < Duration::from_millis(500),
            "a sample waited for the pool: {:?}",
            began.elapsed()
        );
        let st = sink.stats();
        assert_eq!((st.created, st.plain, st.reused), (2, 4, 0), "{st:?}");
        // every one of them is a whole, distinct frame
        let seen: HashSet<_> = held.iter().map(address).collect();
        assert_eq!(seen.len(), 6);
        // let go: the pooled two come back, the plain four are freed; the next samples are the pooled ones again
        drop(held);
        until("the pooled samples came back", || {
            sink.pool.inner.free.lock_safe().len() == 2
        });
        let a = take(&mut sink, 1);
        let b = take(&mut sink, 2);
        let st = sink.stats();
        assert_eq!((st.created, st.plain, st.reused), (2, 4, 2), "{st:?}");
        drop((a, b));
    }

    #[test]
    fn a_pool_that_may_make_nothing_gives_plain_samples() {
        mf();
        let f = fmt(PixelFormat::Nv12, 640, 360);
        let mut sink = SampleSink::with_pool(SamplePool::with_max(f, 0), f);
        for n in 0..5 {
            take(&mut sink, n);
        }
        assert_eq!(
            sink.stats(),
            PoolStats {
                reused: 0,
                created: 0,
                plain: 5
            }
        );
    }

    #[test]
    fn the_pool_is_emptied_when_it_ends_and_a_sample_that_is_still_held_stays_whole_and_goes_when_released(
    ) {
        mf();
        let f = fmt(PixelFormat::Nv12, 1280, 720);
        let mut sink = SampleSink::new(f);
        let watch = sink.pool.watch();
        let held = take(&mut sink, 77);
        let other = take(&mut sink, 1);
        drop(other);
        until("a sample is on the free list", || {
            !sink.pool.inner.free.lock_safe().is_empty()
        });
        drop(sink);
        // the pool is closed: the free list is empty, and the shared state lives only for the sample still out
        {
            let inner = watch
                .0
                .upgrade()
                .expect("a held sample still points at the pool");
            assert!(inner.closed.load(Ordering::Acquire));
            assert!(inner.free.lock_safe().is_empty(), "the free list was kept");
        }
        // the consumer that held a sample reads it whole
        assert_eq!(address(&held).1, 77);
        // and letting it go frees it for good: nothing of the pool is left
        drop(held);
        until("the pool was released", || !watch.alive());
    }

    /// Let `s` go and wait until the pool has it back.
    fn give_back(pool: &SamplePool, s: IMFSample) {
        let want = pool.inner.free.lock_safe().len() + 1;
        drop(s);
        until("the sample came back", || {
            pool.inner.free.lock_safe().len() == want
        });
    }

    #[test]
    fn a_sample_comes_back_without_what_the_consumer_set_on_it() {
        mf();
        let f = fmt(PixelFormat::Nv12, 640, 360);
        let mut pool = SamplePool::new(f);
        let key = windows_core::GUID::from_u128(0x6b0f_5f1a_0000_4000_8000_0000_0000_0001);
        let obj_key = windows_core::GUID::from_u128(0x6b0f_5f1a_0000_4000_8000_0000_0000_0002);
        let foreign: IUnknown = {
            let mut a: Option<IMFAttributes> = None;
            unsafe { MFCreateAttributes(&mut a, 0).unwrap() };
            a.unwrap().cast().unwrap()
        };
        let s = pool.sample().unwrap();
        unsafe {
            s.SetGUID(&key, &key).unwrap();
            s.SetUnknown(&obj_key, &foreign).unwrap();
            s.SetUnknown(&MFSampleExtension_Token, &foreign).unwrap();
            s.SetSampleFlags(7).unwrap();
            assert_eq!(s.GetSampleFlags().unwrap(), 7);
        }
        give_back(&pool, s);
        let s = pool.sample().unwrap();
        assert_eq!(pool.stats().reused, 1);
        unsafe {
            assert!(s.GetGUID(&key).is_err(), "an attribute was kept");
            assert!(s.GetUnknown::<IUnknown>(&obj_key).is_err());
            assert!(s.GetUnknown::<IUnknown>(&MFSampleExtension_Token).is_err());
            assert_eq!(s.GetSampleFlags().unwrap(), 0, "the flags were kept");
            assert_eq!(s.GetCount().unwrap(), 0);
        }
    }

    #[test]
    fn a_sample_that_was_made_and_never_posted_goes_back_to_the_pool_clean() {
        mf();
        let f = fmt(PixelFormat::Nv12, 640, 360);
        let mut sink = SampleSink::new(f);
        let token: IUnknown = {
            let mut a: Option<IMFAttributes> = None;
            unsafe { MFCreateAttributes(&mut a, 0).unwrap() };
            a.unwrap().cast().unwrap()
        };
        for n in 0..10u8 {
            // the pump stopped between "sample ready" and "posted": the sample is dropped
            let b = sink.buffer().unwrap();
            b.data[0] = n;
            let s = sink.finish(i64::from(n), Some(token.clone())).unwrap();
            give_back(&sink.pool, s);
        }
        let st = sink.stats();
        assert_eq!((st.created, st.reused, st.plain), (1, 9, 0), "{st:?}");
        let s = sink.pool.sample().unwrap();
        unsafe {
            assert!(s.GetUnknown::<IUnknown>(&MFSampleExtension_Token).is_err());
        }
    }

    #[test]
    fn a_pooled_sample_that_came_back_with_another_buffer_is_not_kept() {
        mf();
        let f = fmt(PixelFormat::Nv12, 640, 360);
        let mut pool = SamplePool::new(f);
        // a second buffer
        let s = pool.sample().unwrap();
        unsafe { s.AddBuffer(&pool.new_buffer().unwrap()).unwrap() };
        give_back(&pool, s);
        let s = pool.sample().unwrap();
        let st = pool.stats();
        assert_eq!((st.created, st.reused, st.plain), (2, 0, 0), "{st:?}");
        assert_eq!(pool.inner.owned.load(Ordering::Acquire), 1);
        assert!(pool.inner.free.lock_safe().is_empty());
        unsafe { assert_eq!(s.GetBufferCount().unwrap(), 1) };
        // a buffer that is not a 2D buffer
        unsafe {
            s.RemoveAllBuffers().unwrap();
            s.AddBuffer(
                &windows::Win32::Media::MediaFoundation::MFCreateMemoryBuffer(4096).unwrap(),
            )
            .unwrap();
        }
        give_back(&pool, s);
        let s = pool.sample().unwrap();
        let st = pool.stats();
        assert_eq!((st.created, st.reused, st.plain), (3, 0, 0), "{st:?}");
        assert_eq!(pool.inner.owned.load(Ordering::Acquire), 1);
        // and an untouched one is kept
        give_back(&pool, s);
        let _s = pool.sample().unwrap();
        assert_eq!(pool.stats().reused, 1);
    }

    #[test]
    fn a_pooled_sample_with_a_buffer_that_cannot_hold_a_frame_leaves_the_pool_and_the_frame_goes_out_plain(
    ) {
        mf();
        let f = fmt(PixelFormat::Nv12, 640, 360);
        let mut sink = SampleSink::new(f);
        let s = take(&mut sink, 1);
        // a consumer swapped the buffer for another 2D one, too small for a frame: one 2D buffer, so it passes the
        // checks of the free list and fails when it is locked for a frame
        unsafe {
            s.RemoveAllBuffers().unwrap();
            s.AddBuffer(
                &MFCreate2DMediaBuffer(32, 32, PixelFormat::Nv12.buffer_fourcc(), false).unwrap(),
            )
            .unwrap();
        }
        give_back(&sink.pool, s);
        // the frame still gets a sample, and the pool no longer counts the bad one
        let s2 = take(&mut sink, 2);
        let st = sink.stats();
        assert_eq!((st.created, st.plain, st.reused), (1, 1, 1), "{st:?}");
        until("the pool let the bad sample go", || {
            sink.pool.inner.owned.load(Ordering::Acquire) == 0
        });
        assert_eq!(address(&s2).1, 2);
        drop(s2);
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            sink.pool.inner.free.lock_safe().is_empty(),
            "the retired sample came back"
        );
        // the next frame makes a new pooled sample
        let _s3 = take(&mut sink, 3);
        assert_eq!(sink.stats().created, 2);
    }
    #[test]
    fn a_sink_with_a_locked_sample_that_is_never_finished_gives_it_back() {
        mf();
        let f = fmt(PixelFormat::Nv12, 640, 360);
        let mut sink = SampleSink::with_pool(SamplePool::with_max(f, 1), f);
        let watch = sink.pool.watch();
        // the pump stops between "buffer" and "finish": the sink is dropped with a locked sample in it
        let first = sink.buffer().unwrap().data.as_ptr() as usize;
        // the same buffer comes back until the owner took the frame
        let again = sink.buffer().unwrap().data.as_ptr() as usize;
        assert_eq!(first, again);
        drop(sink);
        until("the pool was released", || !watch.alive());
    }
}
