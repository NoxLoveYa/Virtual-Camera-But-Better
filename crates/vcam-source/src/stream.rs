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

use windows::Win32::Foundation::{CloseHandle, E_POINTER, HANDLE, S_OK};
use windows::Win32::Media::timeBeginPeriod;
use windows::Win32::Media::timeEndPeriod;
use windows::Win32::Media::KernelStreaming::{
    IKsControl, IKsControl_Impl, KSIDENTIFIER, PINNAME_VIDEO_CAPTURE,
};
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer2, IMFAsyncCallback, IMFAsyncResult, IMFAttributes, IMFMediaEvent,
    IMFMediaEventGenerator_Impl, IMFMediaEventQueue, IMFMediaSource, IMFMediaStream2,
    IMFMediaStream2_Impl, IMFMediaStream_Impl, IMFMediaType, IMFSample, IMFStreamDescriptor,
    MEMediaSample, MEStreamStarted, MEStreamStopped, MF2DBuffer_LockFlags_Write,
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

struct Pump {
    shared: Arc<PumpShared>,
    thread: Option<JoinHandle<()>>,
}

impl Pump {
    fn spawn(
        queue: IMFMediaEventQueue,
        fmt: FrameFormat,
        frames_out: Arc<AtomicU64>,
        path: PathBuf,
    ) -> Result<Pump> {
        let shared = Arc::new(PumpShared {
            inner: Mutex::new(PumpInner {
                tokens: VecDeque::new(),
                stop: false,
            }),
            cv: Condvar::new(),
        });
        let s2 = shared.clone();
        let queue = Agile(queue);
        pump_thread_spawned();
        let thread = std::thread::Builder::new()
            .name("bettercam-pump".into())
            .spawn(move || {
                let queue = queue; // capture the whole Agile wrapper, not its field
                let r = catch_unwind(AssertUnwindSafe(|| {
                    pump_loop(&s2, &queue.0, fmt, &frames_out, path)
                }));
                if r.is_err() {
                    vlog!("PANIC contained in pump thread");
                }
            })
            .map_err(|_| Error::from(windows::Win32::Foundation::E_OUTOFMEMORY))?;
        Ok(Pump {
            shared,
            thread: Some(thread),
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

    fn stop(&mut self) {
        {
            let mut g = self.shared.inner.lock_safe();
            g.stop = true;
        }
        self.shared.cv.notify_all();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
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

fn pump_loop(
    shared: &PumpShared,
    queue: &IMFMediaEventQueue,
    fmt: FrameFormat,
    frames_out: &AtomicU64,
    path: PathBuf,
) {
    unsafe { timeBeginPeriod(1) };
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
        // timestamps must grow: a late live frame can be older than a repeat posted before it
        let ts = vcam_protocol::qpc_to_hns(emit.sample_qpc()).max(prev_ts + 1);
        match make_sample(&fmt, src.data(), ts, token.map(|t| t.0)) {
            Ok(sample) => {
                let r = unsafe {
                    queue.QueueEventParamUnk(MEMediaSample.0 as u32, &GUID::zeroed(), S_OK, &sample)
                };
                match r {
                    Ok(()) => {
                        prev_ts = ts;
                        src.finish_emit(emit, vcam_protocol::qpc_now());
                        let n = frames_out.fetch_add(1, Ordering::Relaxed) + 1;
                        if n <= 3 || n % 900 == 0 {
                            vlog!("sample #{n} {:?} ts={ts}", emit.kind);
                        }
                    }
                    Err(e) => {
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
    unsafe { timeEndPeriod(1) };
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
        let mut st = self.state.lock_safe();
        if st.shutdown {
            return Err(Error::from(MF_E_SHUTDOWN));
        }
        if let Some(mut old) = st.pump.take() {
            old.stop();
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
        let mut st = self.state.lock_safe();
        if st.shutdown {
            return Err(Error::from(MF_E_SHUTDOWN));
        }
        if let Some(mut p) = st.pump.take() {
            p.stop();
        }
        let was = st.state;
        st.state = MF_STREAM_STATE_STOPPED;
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
        let mut st = self.state.lock_safe();
        if st.shutdown {
            return;
        }
        st.shutdown = true;
        if let Some(mut p) = st.pump.take() {
            p.stop();
        }
        st.state = MF_STREAM_STATE_STOPPED;
        drop(st);
        let _ = unsafe { self.queue.Shutdown() };
        self.set_source(None);
        vlog!("Stream::shutdown");
    }
}

impl Drop for MediaStream {
    fn drop(&mut self) {
        let st = match self.state.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        // normally already stopped by shutdown(); make sure the thread never outlives the object
        let mut st = st;
        if let Some(mut p) = st.pump.take() {
            p.stop();
        }
        drop(st);
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
