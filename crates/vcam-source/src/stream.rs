//! `MediaStream`: `IMFMediaStream2` + `IKsControl` (+ `IMFAttributes` through an inner store).
//!
//! Frames come from a [`FrameSource`] (the shared file, or a slate). `RequestSample` queues a token; one pump thread
//! per stream waits until the source has something to emit, copies it into a `MFCreateSample` +
//! `MFCreate2DMediaBuffer` sample (system memory, always) and posts `MEMediaSample`. The thread exists only while the
//! stream runs; creating the source or its presentation descriptor starts nothing.

use std::collections::VecDeque;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use windows::Win32::Foundation::{CloseHandle, E_POINTER, E_UNEXPECTED, HANDLE, S_OK};
use windows::Win32::Media::timeBeginPeriod;
use windows::Win32::Media::timeEndPeriod;
use windows::Win32::Media::KernelStreaming::{
    IKsControl, IKsControl_Impl, KSIDENTIFIER, PINNAME_VIDEO_CAPTURE,
};
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer2, IMFAsyncCallback, IMFAsyncResult, IMFAttributes, IMFMediaEvent,
    IMFMediaEventGenerator_Impl, IMFMediaEventQueue, IMFMediaSource, IMFMediaStream2,
    IMFMediaStream2_Impl, IMFMediaStream_Impl, IMFMediaType, IMFSample, IMFStreamDescriptor,
    MEError, MEMediaSample, MEStreamStarted, MEStreamStopped, MF2DBuffer_LockFlags_Write,
    MFCreate2DMediaBuffer, MFCreateAttributes, MFCreateEventQueue, MFCreateSample,
    MFCreateStreamDescriptor, MFFrameSourceTypes_Color, MFGetSystemTime,
    MFSampleExtension_CleanPoint, MFSampleExtension_Token, MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS,
    MF_DEVICESTREAM_ATTRIBUTE_FRAMESOURCE_TYPES, MF_DEVICESTREAM_FRAMESERVER_SHARED,
    MF_DEVICESTREAM_STREAM_CATEGORY, MF_DEVICESTREAM_STREAM_ID, MF_E_INVALIDMEDIATYPE,
    MF_E_INVALID_STATE_TRANSITION, MF_E_SHUTDOWN, MF_STREAM_STATE, MF_STREAM_STATE_PAUSED,
    MF_STREAM_STATE_RUNNING, MF_STREAM_STATE_STOPPED,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Threading::{
    CreateWaitableTimerExW, SetWaitableTimer, WaitForSingleObject,
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS,
};
use windows_core::{implement, ComObject, Error, IUnknown, Interface, Ref, Result, GUID, HRESULT};

use crate::attrs::delegate_attributes;
use crate::frames::{FrameSource, Poll, Wait};
use crate::guard::{guard, LockExt};
use crate::mediatype::{advertised_types, FrameFormat, PixelFormat};
use crate::{object_created, object_dropped, pump_thread_spawned, vlog};

/// Media Foundation event queues and sample tokens are free-threaded (they are handed to arbitrary MF work
/// threads), but windows-rs does not mark interface pointers Send/Sync.
struct Agile<T>(T);
// SAFETY: only used for IMFMediaEventQueue and the opaque IUnknown token Frame Server passes to
// RequestSample; both are free-threaded by contract.
unsafe impl<T> Send for Agile<T> {}
unsafe impl<T> Sync for Agile<T> {}

/// Tokens waiting for a frame and the control flags of the pump thread.
struct PumpInner {
    tokens: VecDeque<Option<Agile<IUnknown>>>,
    stop: bool,
}

struct PumpShared {
    inner: Mutex<PumpInner>,
    cv: Condvar,
}

/// Set when the pump thread has finished, whatever way it ended.
struct Done {
    flag: Mutex<bool>,
    cv: Condvar,
}

struct DoneGuard(Arc<Done>);

impl Drop for DoneGuard {
    fn drop(&mut self) {
        *self.0.flag.lock_safe() = true;
        self.0.cv.notify_all();
    }
}

/// How long [`Pump::stop`] waits for the thread. A pump blocked in a file open does not hold up the caller longer;
/// the thread then ends on its own as soon as it is released.
const STOP_WAIT: Duration = Duration::from_secs(2);

struct Pump {
    shared: Arc<PumpShared>,
    thread: Option<JoinHandle<()>>,
    done: Arc<Done>,
}

/// `timeBeginPeriod(1)` for the life of the value, ended on every path out of the pump, a panic included.
struct TimerPeriod;

#[cfg(test)]
thread_local! {
    static OPEN_PERIODS: std::cell::Cell<i32> = const { std::cell::Cell::new(0) };
}

impl TimerPeriod {
    fn begin() -> TimerPeriod {
        unsafe { timeBeginPeriod(1) };
        #[cfg(test)]
        OPEN_PERIODS.with(|c| c.set(c.get() + 1));
        TimerPeriod
    }
}

impl Drop for TimerPeriod {
    fn drop(&mut self) {
        unsafe { timeEndPeriod(1) };
        #[cfg(test)]
        OPEN_PERIODS.with(|c| c.set(c.get() - 1));
    }
}

/// Tell the consumer the stream has failed: it gets an `MEError` instead of silence.
fn report_failure(queue: &IMFMediaEventQueue) {
    let r = unsafe {
        queue.QueueEventParamVar(
            MEError.0 as u32,
            &GUID::zeroed(),
            E_UNEXPECTED,
            std::ptr::null(),
        )
    };
    if let Err(e) = r {
        vlog!("could not queue MEError: {e}");
    }
}

impl Pump {
    fn spawn(
        queue: IMFMediaEventQueue,
        fmt: FrameFormat,
        frames_out: Arc<AtomicU64>,
        path: PathBuf,
    ) -> Result<Pump> {
        Pump::spawn_with(queue, move |shared, queue| {
            pump_loop(shared, queue, fmt, &frames_out, path)
        })
    }

    /// Run `body` on the pump thread. A panic in it is contained and reported to the consumer with an `MEError`
    /// event on `queue` (otherwise the stream would look running while no more samples arrive).
    fn spawn_with(
        queue: IMFMediaEventQueue,
        body: impl FnOnce(&PumpShared, &IMFMediaEventQueue) + Send + 'static,
    ) -> Result<Pump> {
        let shared = Arc::new(PumpShared {
            inner: Mutex::new(PumpInner {
                tokens: VecDeque::new(),
                stop: false,
            }),
            cv: Condvar::new(),
        });
        let done = Arc::new(Done {
            flag: Mutex::new(false),
            cv: Condvar::new(),
        });
        let s2 = shared.clone();
        let d2 = done.clone();
        let queue = Agile(queue);
        pump_thread_spawned();
        let thread = std::thread::Builder::new()
            .name("bettercam-pump".into())
            .spawn(move || {
                let _done = DoneGuard(d2);
                let queue = queue; // capture the whole Agile wrapper, not its field
                let r = catch_unwind(AssertUnwindSafe(|| body(&s2, &queue.0)));
                if r.is_err() {
                    vlog!("PANIC contained in pump thread");
                    report_failure(&queue.0);
                }
            })
            .map_err(|_| Error::from(windows::Win32::Foundation::E_OUTOFMEMORY))?;
        Ok(Pump {
            shared,
            thread: Some(thread),
            done,
        })
    }

    fn add_token(&self, token: Option<IUnknown>) {
        let mut g = self.shared.inner.lock_safe();
        // A consumer that never gets answers can pile tokens up; keep the queue bounded.
        if g.tokens.len() < 64 {
            g.tokens.push_back(token.map(Agile));
        }
        drop(g);
        self.shared.cv.notify_all();
    }

    /// Stop the thread and wait for it, at most [`STOP_WAIT`]. Call it without holding a lock the pump needs or a
    /// lock the service's control calls need.
    fn stop(&mut self) {
        self.stop_within(STOP_WAIT);
    }

    fn stop_within(&mut self, wait: Duration) {
        {
            let mut g = self.shared.inner.lock_safe();
            g.stop = true;
        }
        self.shared.cv.notify_all();
        let Some(t) = self.thread.take() else {
            return;
        };
        let finished = {
            let g = self.done.flag.lock_safe();
            match self.done.cv.wait_timeout_while(g, wait, |done| !*done) {
                Ok((g, _)) => *g,
                Err(p) => *p.into_inner().0,
            }
        };
        if finished {
            let _ = t.join();
        } else {
            // dropping the handle detaches the thread: it ends by itself once it is unblocked and sees `stop`
            vlog!("pump thread did not stop within {wait:?}; not waiting any longer");
        }
    }
}

/// Sample time for an emission captured at `sample_qpc`: its QPC time in 100 ns units, but always after `prev_ts`
/// (timestamps must grow: a late live frame can be older than a repeat posted before it). Saturates instead of
/// overflowing, whatever the inputs.
pub(crate) fn next_timestamp(sample_qpc: u64, prev_ts: i64) -> i64 {
    vcam_protocol::qpc_to_hns(sample_qpc).max(prev_ts.saturating_add(1))
}

/// A high-resolution waitable timer for the 1 ms polls (falls back to `sleep` when the OS lacks one).
struct HiResTimer(Option<HANDLE>);

impl HiResTimer {
    fn new() -> HiResTimer {
        let h = unsafe {
            CreateWaitableTimerExW(
                None,
                windows_core::PCWSTR::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS.0,
            )
        };
        HiResTimer(h.ok())
    }

    fn wait_1ms(&self) {
        if let Some(h) = self.0 {
            let due: i64 = -10_000; // 1 ms, relative, in 100 ns units
            if unsafe { SetWaitableTimer(h, &due, 0, None, None, false) }.is_ok() {
                unsafe { WaitForSingleObject(h, 50) };
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

impl Drop for HiResTimer {
    fn drop(&mut self) {
        if let Some(h) = self.0.take() {
            let _ = unsafe { CloseHandle(h) };
        }
    }
}

fn is_stopped(shared: &PumpShared) -> bool {
    shared.inner.lock_safe().stop
}

#[cfg(test)]
type PostHook = Box<dyn FnOnce(&PumpShared)>;

#[cfg(test)]
thread_local! {
    /// Runs on the pump thread between "a sample is ready" and "the sample is posted" (tests: stop the pump there).
    static BEFORE_POST_HOOK: std::cell::RefCell<Option<PostHook>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn run_before_post_hook(shared: &PumpShared) {
    if let Some(f) = BEFORE_POST_HOOK.with(|h| h.borrow_mut().take()) {
        f(shared);
    }
}

/// Post `sample` to the consumer unless the pump has been stopped. `None`: stopped, nothing was posted.
///
/// The check and the post happen under the lock that [`Pump::stop`] takes to set the flag, so a sample is either
/// posted before the stop (and so before the stream's stop event) or not at all. A check made only before the
/// poll is not enough: the poll opens the frame file, and a pump that was detached after waiting for such an open
/// for over two seconds would otherwise post a sample after the stop event, or after a restart of the stream.
fn post_sample(
    shared: &PumpShared,
    queue: &IMFMediaEventQueue,
    sample: &IMFSample,
) -> Option<Result<()>> {
    let g = shared.inner.lock_safe();
    if g.stop {
        return None;
    }
    let r =
        unsafe { queue.QueueEventParamUnk(MEMediaSample.0 as u32, &GUID::zeroed(), S_OK, sample) };
    drop(g);
    Some(r)
}

fn pump_loop(
    shared: &PumpShared,
    queue: &IMFMediaEventQueue,
    fmt: FrameFormat,
    frames_out: &AtomicU64,
    path: PathBuf,
) {
    let _period = TimerPeriod::begin();
    let timer = HiResTimer::new();
    let freq = vcam_protocol::qpc_frequency();
    let mut src = FrameSource::new(path, fmt);
    src.start();
    let mut prev_ts: i64 = 0;
    vlog!("pump start {:?}", fmt);
    'outer: loop {
        // wait for a token
        let token = {
            let mut g = shared.inner.lock_safe();
            loop {
                if g.stop {
                    break 'outer;
                }
                if let Some(t) = g.tokens.pop_front() {
                    break t;
                }
                g = match shared.cv.wait(g) {
                    Ok(g) => g,
                    Err(p) => p.into_inner(),
                };
            }
        };
        // wait until the source has something to emit
        let emit = loop {
            if is_stopped(shared) {
                break 'outer;
            }
            let now = vcam_protocol::qpc_now();
            match src.poll(now) {
                Poll::Ready(e) => break e,
                Poll::Wait(Wait::Soon) => timer.wait_1ms(),
                Poll::Wait(Wait::Until(t)) => {
                    let ns = (t.saturating_sub(now) as u128 * 1_000_000_000 / freq as u128) as u64;
                    let g = shared.inner.lock_safe();
                    let _ = shared
                        .cv
                        .wait_timeout_while(g, Duration::from_nanos(ns), |i| !i.stop);
                }
            }
        };
        let ts = next_timestamp(emit.sample_qpc(), prev_ts);
        match make_sample(&fmt, src.data(), ts, token.map(|t| t.0)) {
            Ok(sample) => {
                #[cfg(test)]
                run_before_post_hook(shared);
                match post_sample(shared, queue, &sample) {
                    // stopped while the poll (a file open) was under way: this sample is from before the stop
                    None => break 'outer,
                    Some(Ok(())) => {
                        prev_ts = ts;
                        src.finish_emit(emit, vcam_protocol::qpc_now());
                        let n = frames_out.fetch_add(1, Ordering::Relaxed) + 1;
                        if n <= 3 || n % 900 == 0 {
                            vlog!("sample #{n} {:?} ts={ts}", emit.kind);
                        }
                    }
                    Some(Err(e)) => {
                        vlog!("QueueEventParamUnk(MEMediaSample) failed: {e}");
                        break 'outer;
                    }
                }
            }
            Err(e) => {
                vlog!("make_sample failed: {e}");
                // keep going: the next token gets another try
            }
        }
    }
    src.stop();
    vlog!(
        "pump stop after {} samples",
        frames_out.load(Ordering::Relaxed)
    );
}

/// Copy a tight frame (`data`, NV12 or BGRX of `fmt`) into a fresh MF sample stamped `ts` (100 ns units).
pub fn make_sample(
    fmt: &FrameFormat,
    data: &[u8],
    ts: i64,
    token: Option<IUnknown>,
) -> Result<IMFSample> {
    let (w, h) = (fmt.width as usize, fmt.height as usize);
    if data.len() != fmt.pixel.frame_bytes(fmt.width, fmt.height) as usize {
        return Err(Error::from(MF_E_INVALIDMEDIATYPE));
    }
    unsafe {
        let sample = MFCreateSample()?;
        let buffer =
            MFCreate2DMediaBuffer(fmt.width, fmt.height, fmt.pixel.buffer_fourcc(), false)?;
        let b2: IMF2DBuffer2 = buffer.cast()?;

        let mut scan0: *mut u8 = std::ptr::null_mut();
        let mut pitch: i32 = 0;
        let mut start: *mut u8 = std::ptr::null_mut();
        let mut len: u32 = 0;
        b2.Lock2DSize(
            MF2DBuffer_LockFlags_Write,
            &mut scan0,
            &mut pitch,
            &mut start,
            &mut len,
        )?;
        let copied = (|| -> Result<()> {
            if scan0.is_null() || pitch <= 0 {
                return Err(Error::from(E_POINTER));
            }
            let pitch = pitch as usize;
            let row_bytes = match fmt.pixel {
                PixelFormat::Nv12 => w,
                PixelFormat::Rgb32 => w * 4,
            };
            let rows = match fmt.pixel {
                PixelFormat::Nv12 => h * 3 / 2,
                PixelFormat::Rgb32 => h,
            };
            let needed = pitch * rows;
            if pitch < row_bytes || (len as usize) < needed || start != scan0 {
                return Err(Error::from(MF_E_INVALIDMEDIATYPE));
            }
            let dst = std::slice::from_raw_parts_mut(scan0, len as usize);
            if pitch == row_bytes {
                dst[..data.len()].copy_from_slice(data);
            } else {
                // rows of both planes are `row_bytes` wide in `data`; the UV plane follows the Y plane at the
                // same pitch in the buffer
                for r in 0..rows {
                    dst[r * pitch..r * pitch + row_bytes]
                        .copy_from_slice(&data[r * row_bytes..(r + 1) * row_bytes]);
                }
            }
            Ok(())
        })();
        let unlocked = b2.Unlock2D();
        copied?;
        unlocked?;
        let contiguous = b2.GetContiguousLength()?;
        buffer.SetCurrentLength(contiguous)?;
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime(ts)?;
        sample.SetSampleDuration(fmt.frame_duration())?;
        sample.SetUINT32(&MFSampleExtension_CleanPoint, 1)?;
        if let Some(t) = token {
            sample.SetUnknown(&MFSampleExtension_Token, &t)?;
        }
        Ok(sample)
    }
}

struct StreamState {
    state: MF_STREAM_STATE,
    shutdown: bool,
    format: Option<FrameFormat>,
    pump: Option<Pump>,
}

/// The single video stream (id 0).
#[implement(
    IMFMediaStream2,
    IKsControl,
    windows::Win32::Media::MediaFoundation::IMFAttributes
)]
pub struct MediaStream {
    id: u32,
    queue: IMFMediaEventQueue,
    attributes: IMFAttributes,
    descriptor: IMFStreamDescriptor,
    source: Mutex<Option<IMFMediaSource>>,
    /// Serialises start, stop and shutdown. Held while an old pump is being stopped; `state` is not, so
    /// `RequestSample`, `GetStreamState` and friends never wait for a pump to end.
    control: Mutex<()>,
    state: Mutex<StreamState>,
    frames_out: Arc<AtomicU64>,
}

fn set_stream_attributes(a: &IMFAttributes, id: u32) -> Result<()> {
    unsafe {
        a.SetGUID(&MF_DEVICESTREAM_STREAM_CATEGORY, &PINNAME_VIDEO_CAPTURE)?;
        a.SetUINT32(&MF_DEVICESTREAM_STREAM_ID, id)?;
        a.SetUINT32(&MF_DEVICESTREAM_FRAMESERVER_SHARED, 1)?;
        a.SetUINT32(
            &MF_DEVICESTREAM_ATTRIBUTE_FRAMESOURCE_TYPES,
            MFFrameSourceTypes_Color.0 as u32,
        )?;
    }
    Ok(())
}

impl MediaStream {
    pub fn create(id: u32) -> Result<ComObject<MediaStream>> {
        let types = advertised_types()?;
        let typed: Vec<Option<IMFMediaType>> = types.iter().cloned().map(Some).collect();
        let descriptor = unsafe { MFCreateStreamDescriptor(id, &typed)? };
        unsafe {
            let handler = descriptor.GetMediaTypeHandler()?;
            handler.SetCurrentMediaType(&types[0])?;
        }
        // the same four attributes on the descriptor and on the stream's own store
        set_stream_attributes(&descriptor.cast::<IMFAttributes>()?, id)?;
        let mut store: Option<IMFAttributes> = None;
        unsafe { MFCreateAttributes(&mut store, 4)? };
        let attributes = store.ok_or_else(|| Error::from(MF_E_SHUTDOWN))?;
        set_stream_attributes(&attributes, id)?;
        let queue = unsafe { MFCreateEventQueue()? };
        object_created();
        Ok(ComObject::new(MediaStream {
            id,
            queue,
            attributes,
            descriptor,
            source: Mutex::new(None),
            control: Mutex::new(()),
            state: Mutex::new(StreamState {
                state: MF_STREAM_STATE_STOPPED,
                shutdown: false,
                format: None,
                pump: None,
            }),
            frames_out: Arc::new(AtomicU64::new(0)),
        }))
    }

    pub fn id(&self) -> u32 {
        self.id
    }

    pub fn descriptor(&self) -> &IMFStreamDescriptor {
        &self.descriptor
    }

    /// Attribute store returned by `IMFMediaSourceEx::GetStreamAttributes`.
    pub fn attribute_store(&self) -> IMFAttributes {
        self.attributes.clone()
    }

    pub fn set_source(&self, source: Option<IMFMediaSource>) {
        *self.source.lock_safe() = source;
    }

    pub fn frames_delivered(&self) -> u64 {
        self.frames_out.load(Ordering::Relaxed)
    }

    fn current_descriptor_type(&self) -> Result<IMFMediaType> {
        unsafe { self.descriptor.GetMediaTypeHandler()?.GetCurrentMediaType() }
    }

    /// Start (or restart with a new type) delivering frames. `media_type` is the type the consumer selected.
    pub fn start(&self, media_type: Option<&IMFMediaType>) -> Result<()> {
        let mt = match media_type {
            Some(t) => t.clone(),
            None => self.current_descriptor_type()?,
        };
        let fmt = FrameFormat::from_media_type(&mt)?;
        if !crate::mediatype::is_sane(&fmt) {
            vlog!("Stream::start rejects {:?}", fmt);
            return Err(Error::from(MF_E_INVALIDMEDIATYPE));
        }
        let _control = self.control.lock_safe();
        let old = {
            let mut st = self.state.lock_safe();
            if st.shutdown {
                return Err(Error::from(MF_E_SHUTDOWN));
            }
            st.pump.take()
        };
        if let Some(mut old) = old {
            // outside the state lock: a pump stuck in a file open must not stall the service's other calls
            old.stop();
        }
        let mut st = self.state.lock_safe();
        if st.shutdown {
            return Err(Error::from(MF_E_SHUTDOWN));
        }
        // remember the chosen type on our own descriptor so GetStreamDescriptor reflects it
        unsafe {
            if let Ok(h) = self.descriptor.GetMediaTypeHandler() {
                let _ = h.SetCurrentMediaType(&mt);
            }
        }
        st.pump = Some(Pump::spawn(
            self.queue.clone(),
            fmt,
            self.frames_out.clone(),
            crate::frames::frames_path(),
        )?);
        st.format = Some(fmt);
        st.state = MF_STREAM_STATE_RUNNING;
        let t = PROPVARIANT::from(unsafe { MFGetSystemTime() });
        unsafe {
            self.queue
                .QueueEventParamVar(MEStreamStarted.0 as u32, &GUID::zeroed(), S_OK, &t)?
        };
        vlog!("Stream::start {:?}", fmt);
        Ok(())
    }

    pub fn stop(&self) -> Result<()> {
        let _control = self.control.lock_safe();
        let (pump, was) = {
            let mut st = self.state.lock_safe();
            if st.shutdown {
                return Err(Error::from(MF_E_SHUTDOWN));
            }
            let was = st.state;
            st.state = MF_STREAM_STATE_STOPPED;
            (st.pump.take(), was)
        };
        if let Some(mut p) = pump {
            p.stop();
        }
        if was != MF_STREAM_STATE_STOPPED {
            unsafe {
                self.queue.QueueEventParamVar(
                    MEStreamStopped.0 as u32,
                    &GUID::zeroed(),
                    S_OK,
                    std::ptr::null(),
                )?
            };
        }
        vlog!("Stream::stop");
        Ok(())
    }

    pub fn shutdown(&self) {
        let _control = self.control.lock_safe();
        let pump = {
            let mut st = self.state.lock_safe();
            if st.shutdown {
                return;
            }
            st.shutdown = true;
            st.state = MF_STREAM_STATE_STOPPED;
            st.pump.take()
        };
        if let Some(mut p) = pump {
            p.stop();
        }
        let _ = unsafe { self.queue.Shutdown() };
        self.set_source(None);
        vlog!("Stream::shutdown");
    }
}

impl Drop for MediaStream {
    fn drop(&mut self) {
        // normally already stopped by shutdown(); make sure the thread never outlives the object
        let st = match self.state.get_mut() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if let Some(mut p) = st.pump.take() {
            p.stop();
        }
        object_dropped();
    }
}

delegate_attributes!(MediaStream_Impl, attributes);

impl IMFMediaEventGenerator_Impl for MediaStream_Impl {
    fn GetEvent(&self, dwflags: MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS) -> Result<IMFMediaEvent> {
        // not under a lock: GetEvent may block until an event arrives
        guard("Stream::GetEvent", || unsafe {
            self.queue.GetEvent(dwflags.0)
        })
    }

    fn BeginGetEvent(
        &self,
        pcallback: Ref<IMFAsyncCallback>,
        punkstate: Ref<IUnknown>,
    ) -> Result<()> {
        guard("Stream::BeginGetEvent", || unsafe {
            self.queue
                .BeginGetEvent(pcallback.as_ref(), punkstate.as_ref())
        })
    }

    fn EndGetEvent(&self, presult: Ref<IMFAsyncResult>) -> Result<IMFMediaEvent> {
        guard("Stream::EndGetEvent", || unsafe {
            self.queue.EndGetEvent(presult.as_ref())
        })
    }

    fn QueueEvent(
        &self,
        met: u32,
        guidextendedtype: *const GUID,
        hrstatus: HRESULT,
        pvvalue: *const PROPVARIANT,
    ) -> Result<()> {
        guard("Stream::QueueEvent", || unsafe {
            self.queue
                .QueueEventParamVar(met, guidextendedtype, hrstatus, pvvalue)
        })
    }
}

impl IMFMediaStream_Impl for MediaStream_Impl {
    fn GetMediaSource(&self) -> Result<IMFMediaSource> {
        guard("Stream::GetMediaSource", || {
            self.source
                .lock_safe()
                .clone()
                .ok_or_else(|| Error::from(MF_E_SHUTDOWN))
        })
    }

    fn GetStreamDescriptor(&self) -> Result<IMFStreamDescriptor> {
        guard("Stream::GetStreamDescriptor", || {
            if self.state.lock_safe().shutdown {
                return Err(Error::from(MF_E_SHUTDOWN));
            }
            Ok(self.descriptor.clone())
        })
    }

    fn RequestSample(&self, ptoken: Ref<IUnknown>) -> Result<()> {
        guard("Stream::RequestSample", || {
            let st = self.state.lock_safe();
            if st.shutdown {
                return Err(Error::from(MF_E_SHUTDOWN));
            }
            crate::log::log_sampled(crate::log::slot::REQUEST_SAMPLE, 900, || {
                "Stream::RequestSample".into()
            });
            match (&st.pump, st.state) {
                (Some(p), s) if s == MF_STREAM_STATE_RUNNING => {
                    p.add_token(ptoken.as_ref().cloned())
                }
                _ => vlog!("Stream::RequestSample ignored: stream not running"),
            }
            Ok(())
        })
    }
}

impl IMFMediaStream2_Impl for MediaStream_Impl {
    fn SetStreamState(&self, value: MF_STREAM_STATE) -> Result<()> {
        guard("Stream::SetStreamState", || {
            let current = self.state.lock_safe().state;
            vlog!("Stream::SetStreamState {} -> {}", current.0, value.0);
            if current == value {
                return Ok(());
            }
            match value {
                v if v == MF_STREAM_STATE_RUNNING => self.start(None),
                v if v == MF_STREAM_STATE_STOPPED => self.stop(),
                v if v == MF_STREAM_STATE_PAUSED => {
                    // Frame Server custom sources do not pause (docs); the transition is refused.
                    Err(Error::from(MF_E_INVALID_STATE_TRANSITION))
                }
                _ => Err(Error::from(MF_E_INVALID_STATE_TRANSITION)),
            }
        })
    }

    fn GetStreamState(&self) -> Result<MF_STREAM_STATE> {
        guard("Stream::GetStreamState", || {
            Ok(self.state.lock_safe().state)
        })
    }
}

/// `ERROR_SET_NOT_FOUND` as an HRESULT: what an AV Stream driver returns for a control it does not implement.
pub fn set_not_found() -> Error {
    Error::from(HRESULT(0x8007_0490u32 as i32))
}

/// Log which KS property set a caller asked for.
pub fn log_ks(what: &str, ident: *const KSIDENTIFIER, len: u32) {
    if ident.is_null() || (len as usize) < std::mem::size_of::<KSIDENTIFIER>() {
        vlog!("{what}: ident null or short (len {len})");
        return;
    }
    // SAFETY: the pointer is non-null and the caller promised `len` readable bytes (>= sizeof KSIDENTIFIER).
    let (set, id, flags) = unsafe {
        let a = &(*ident).Anonymous.Anonymous;
        (a.Set, a.Id, a.Flags)
    };
    vlog!("{what}: set {{{:?}}} id {id} flags {flags:#x}", set);
}

impl IKsControl_Impl for MediaStream_Impl {
    fn KsProperty(
        &self,
        property: *const KSIDENTIFIER,
        propertylength: u32,
        _propertydata: *mut core::ffi::c_void,
        _datalength: u32,
        _bytesreturned: *mut u32,
    ) -> Result<()> {
        guard("Stream::KsProperty", || {
            log_ks("Stream::KsProperty", property, propertylength);
            Err(set_not_found())
        })
    }

    fn KsMethod(
        &self,
        method: *const KSIDENTIFIER,
        methodlength: u32,
        _methoddata: *mut core::ffi::c_void,
        _datalength: u32,
        _bytesreturned: *mut u32,
    ) -> Result<()> {
        guard("Stream::KsMethod", || {
            log_ks("Stream::KsMethod", method, methodlength);
            Err(set_not_found())
        })
    }

    fn KsEvent(
        &self,
        event: *const KSIDENTIFIER,
        eventlength: u32,
        _eventdata: *mut core::ffi::c_void,
        _datalength: u32,
        _bytesreturned: *mut u32,
    ) -> Result<()> {
        guard("Stream::KsEvent", || {
            log_ks("Stream::KsEvent", event, eventlength);
            Err(set_not_found())
        })
    }
}

// keep PixelFormat in scope for rustdoc links
#[allow(unused)]
const _: Option<PixelFormat> = None;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Instant;
    use windows::Win32::Media::MediaFoundation::{
        MFStartup, MFSTARTUP_FULL, MF_EVENT_FLAG_NO_WAIT, MF_VERSION,
    };

    fn queue() -> IMFMediaEventQueue {
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL).unwrap() };
        unsafe { MFCreateEventQueue().unwrap() }
    }

    /// The first event of type `ty` within `timeout`.
    fn wait_for(q: &IMFMediaEventQueue, ty: u32, timeout: Duration) -> Option<IMFMediaEvent> {
        let began = Instant::now();
        while began.elapsed() < timeout {
            if let Ok(e) = unsafe { q.GetEvent(MF_EVENT_FLAG_NO_WAIT.0) } {
                if unsafe { e.GetType() }.unwrap() == ty {
                    return Some(e);
                }
            } else {
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        None
    }

    #[test]
    fn a_panic_in_the_pump_becomes_an_error_event_for_the_consumer() {
        let q = queue();
        let mut pump =
            Pump::spawn_with(q.clone(), |_, _| panic!("test: the pump blows up")).unwrap();
        let e = wait_for(&q, MEError.0 as u32, Duration::from_secs(3))
            .expect("the consumer got no MEError after the pump panicked");
        assert_eq!(unsafe { e.GetStatus() }.unwrap(), E_UNEXPECTED);
        // the thread ended: stopping is immediate
        let t0 = Instant::now();
        pump.stop_within(Duration::from_secs(2));
        assert!(t0.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn a_pump_that_ends_normally_reports_no_error() {
        let q = queue();
        let mut pump = Pump::spawn_with(q.clone(), |_, _| {}).unwrap();
        pump.stop_within(Duration::from_secs(2));
        assert!(wait_for(&q, MEError.0 as u32, Duration::from_millis(100)).is_none());
    }

    #[test]
    fn the_timer_resolution_is_given_back_when_the_pump_panics() {
        let r = catch_unwind(|| {
            let _p = TimerPeriod::begin();
            assert_eq!(OPEN_PERIODS.with(|c| c.get()), 1);
            panic!("test: unwinding with the timer period open");
        });
        assert!(r.is_err());
        assert_eq!(
            OPEN_PERIODS.with(|c| c.get()),
            0,
            "timeEndPeriod was skipped"
        );
    }

    #[test]
    fn stopping_a_pump_stuck_in_a_call_does_not_wait_for_ever() {
        let (tx, rx) = mpsc::channel::<()>();
        let mut pump = Pump::spawn_with(queue(), move |_, _| {
            let _ = rx.recv(); // stands for a file open that does not return
        })
        .unwrap();
        let t0 = Instant::now();
        pump.stop_within(Duration::from_millis(150));
        let took = t0.elapsed();
        assert!(took >= Duration::from_millis(140), "{took:?}");
        assert!(took < Duration::from_secs(1), "{took:?}");
        // the thread is left to finish by itself, and does once it is unblocked
        let done = pump.done.clone();
        drop(pump);
        assert!(!*done.flag.lock_safe());
        tx.send(()).unwrap();
        let began = Instant::now();
        while !*done.flag.lock_safe() {
            assert!(
                began.elapsed() < Duration::from_secs(2),
                "the thread never ended"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn the_stream_state_lock_is_free_while_an_old_pump_is_being_stopped() {
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL).unwrap() };
        let stream = MediaStream::create(0).unwrap();
        let (tx, rx) = mpsc::channel::<()>();
        {
            let mut st = stream.state.lock_safe();
            st.pump = Some(
                Pump::spawn_with(stream.queue.clone(), move |_, _| {
                    let _ = rx.recv();
                })
                .unwrap(),
            );
            st.state = MF_STREAM_STATE_RUNNING;
        }
        let state: &Mutex<StreamState> = &stream.state;
        let (probe, took) = std::thread::scope(|sc| {
            // While `stop` below waits for the stuck pump, the calls the service makes meanwhile
            // (RequestSample, GetStreamState, ...) need the state lock and must not wait for it. After the probe
            // the pump is released, so `stop` returns.
            let probe = sc.spawn(move || {
                std::thread::sleep(Duration::from_millis(300));
                let seen = state
                    .try_lock()
                    .ok()
                    .map(|st| (st.state == MF_STREAM_STATE_STOPPED, st.pump.is_none()));
                let _ = tx.send(());
                seen
            });
            let t0 = Instant::now();
            stream.stop().unwrap();
            (probe.join().unwrap(), t0.elapsed())
        });
        assert_eq!(
            probe,
            Some((true, true)),
            "the state lock was held while the old pump was being stopped"
        );
        assert!(
            took >= Duration::from_millis(250),
            "stop did not wait for the pump ({took:?}): the test did not exercise the wait"
        );
        assert!(took < STOP_WAIT, "{took:?}");
        stream.shutdown();
    }

    /// A pump that runs the real loop on a frame file that does not exist (so it posts slates), with one token to
    /// answer. `before_post` runs on the pump thread when the sample is ready and not yet posted.
    fn run_one_sample(before_post: Option<PostHook>) -> (IMFMediaEventQueue, Pump) {
        let q = queue();
        let fmt = FrameFormat {
            pixel: PixelFormat::Nv12,
            width: 1280,
            height: 720,
            fps: 30,
        };
        // the closure must be Send: smuggle the hook through a Mutex of a pointer-sized Option
        struct Hook(Option<PostHook>);
        // SAFETY: the hook is moved into the pump thread once and used only there.
        unsafe impl Send for Hook {}
        let hook = Hook(before_post);
        let pump = Pump::spawn_with(q.clone(), move |shared, queue| {
            let hook = hook;
            if let Some(h) = hook.0 {
                BEFORE_POST_HOOK.with(|c| *c.borrow_mut() = Some(h));
            }
            pump_loop(
                shared,
                queue,
                fmt,
                &AtomicU64::new(0),
                std::env::temp_dir()
                    .join("bettercam-test-no-such-dir")
                    .join("frames.bin"),
            );
        })
        .unwrap();
        pump.add_token(None);
        (q, pump)
    }

    /// The old pump checked `stop` only before it polled the source. A poll that lasted (a file open that was
    /// held up for over two seconds, after which the pump had been detached) then posted its sample after the
    /// stop. Stopping while the sample is ready but not posted must post nothing.
    #[test]
    fn a_pump_that_is_stopped_while_a_sample_is_ready_posts_nothing() {
        // control: nothing stops it, the sample arrives
        let (q, mut pump) = run_one_sample(None);
        assert!(
            wait_for(&q, MEMediaSample.0 as u32, Duration::from_secs(5)).is_some(),
            "the test setup posts no sample at all"
        );
        pump.stop_within(Duration::from_secs(2));

        // the stop arrives between the poll and the post
        let (q, mut pump) = run_one_sample(Some(Box::new(|shared: &PumpShared| {
            shared.inner.lock_safe().stop = true;
        })));
        // the pump ends by itself: it saw the stop
        let began = Instant::now();
        while !*pump.done.flag.lock_safe() {
            assert!(
                began.elapsed() < Duration::from_secs(5),
                "the pump never ended"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(
            wait_for(&q, MEMediaSample.0 as u32, Duration::from_millis(200)).is_none(),
            "a sample was posted after the stop"
        );
        pump.stop_within(Duration::from_secs(2));
    }

    #[test]
    fn sample_times_never_overflow_and_always_grow() {
        assert_eq!(next_timestamp(0, 0), 1);
        assert_eq!(next_timestamp(0, -5), 0);
        assert!(next_timestamp(u64::MAX, 0) > 0);
        assert_eq!(next_timestamp(u64::MAX, i64::MAX), i64::MAX);
        assert_eq!(next_timestamp(0, i64::MAX), i64::MAX);
        assert_eq!(next_timestamp(1, i64::MAX - 1), i64::MAX);
        let mut prev = 0;
        for q in [100u64, 50, 50, 200, u64::MAX, 3, 0] {
            let ts = next_timestamp(q, prev);
            assert!(ts > prev || ts == i64::MAX, "{q}: {prev} -> {ts}");
            prev = ts;
        }
    }
}
