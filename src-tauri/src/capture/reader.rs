//! Source Reader setup, the async callback, format selection and frame delivery (plan 4.1).
//!
//! # Threading
//! * [`CaptureReader::open`] and [`CaptureReader::close`] may run on any thread. `close` also runs from `Drop`.
//! * Samples arrive on a Media Foundation work queue thread. That callback only wraps the sample in a [`Frame`],
//!   pushes it into a bounded queue (dropping the oldest frame when the consumer is slow) and asks for the next
//!   sample. It never waits for the consumer.
//! * [`CaptureReader::recv_timeout`] and [`CaptureReader::try_recv`] may be called from one consumer thread.
//!
//! # Frames
//! A [`Frame`] keeps its `IMFSample` alive. For decoder textures that pins one surface of the decoder's pool, so
//! a consumer must copy what it needs and drop the frame promptly. The default queue length is 3 for the same
//! reason.

use std::collections::VecDeque;
use std::fmt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use windows::core::{implement, Interface, Ref, GUID, HRESULT};
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Media::MediaFoundation::{
    eAVEncH265VProfile_Main_420_10, IMF2DBuffer, IMFDXGIBuffer, IMFMediaBuffer, IMFMediaEvent,
    IMFMediaSource, IMFMediaType, IMFSample, IMFSourceReader, IMFSourceReaderCallback,
    IMFSourceReaderCallback_Impl, IMFSourceReaderEx, MEError, MEVideoCaptureDevicePreempted,
    MEVideoCaptureDeviceRemoved, MFCreateMediaType, MFCreateSourceReaderFromMediaSource,
    MFMediaType_Video, MFT_ENUM_HARDWARE_URL_Attribute, MFT_ENUM_HARDWARE_VENDOR_ID_Attribute,
    MFT_FRIENDLY_NAME_Attribute, MFT_CATEGORY_VIDEO_DECODER, MF_E_INVALIDINDEX,
    MF_E_VIDEO_RECORDING_DEVICE_INVALIDATED, MF_E_VIDEO_RECORDING_DEVICE_PREEMPTED,
    MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_MPEG2_PROFILE, MF_MT_SUBTYPE,
    MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED,
    MF_SOURCE_READERF_ENDOFSTREAM, MF_SOURCE_READERF_ERROR, MF_SOURCE_READER_ALL_STREAMS,
    MF_SOURCE_READER_ASYNC_CALLBACK, MF_SOURCE_READER_D3D_MANAGER,
    MF_SOURCE_READER_DISCONNECT_MEDIASOURCE_ON_SHUTDOWN,
};

use super::{
    attributes, choose_format, classify_hresult, create_source, hr_text, native_formats,
    qpc_now_100ns, Agile, CaptureError, ErrorClass, FormatChoice, FormatRequest, MfSession,
    NativeFormat, PixelFormat, Ratio, FIRST_VIDEO_STREAM,
};
use crate::gpu::GpuDevice;

// ---------------------------------------------------------------------------------------------------------------
// Public types

/// How a camera is opened.
#[derive(Clone)]
pub struct OpenOptions {
    /// Symbolic link of the camera (`DeviceKey::symbolic_link`).
    pub link: String,
    pub request: FormatRequest,
    /// Allow hardware decoder MFTs (`MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS`) and hand the reader the D3D manager
    /// of `gpu`. When off, decoding is software and frames arrive as CPU buffers.
    pub hw_decode: bool,
    /// The shared device. `None` means CPU frames only.
    pub gpu: Option<Arc<GpuDevice>>,
    /// Frames the queue holds before the oldest is dropped.
    pub queue_len: usize,
    /// How long `open` waits for the first frame or a terminal error.
    pub first_frame_timeout: Duration,
}

impl OpenOptions {
    pub fn new(link: &str, request: FormatRequest) -> OpenOptions {
        OpenOptions {
            link: link.to_string(),
            request,
            hw_decode: true,
            gpu: None,
            queue_len: 3,
            first_frame_timeout: Duration::from_secs(5),
        }
    }
}

/// Whether and how the reader decodes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeKind {
    /// Raw format straight from the camera.
    None,
    Hardware,
    Software,
}

impl fmt::Display for DecodeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            DecodeKind::None => "none",
            DecodeKind::Hardware => "hw",
            DecodeKind::Software => "sw",
        })
    }
}

/// What was negotiated.
#[derive(Clone, Debug)]
pub struct StreamInfo {
    /// The camera's native type that was selected.
    pub native: NativeFormat,
    /// The rate the camera was asked for.
    pub fps: Ratio,
    /// The requested rate did not exist; `fps` is the nearest.
    pub fps_approximate: bool,
    /// What the reader delivers (the native format, or NV12/P010 after decoding).
    pub output_subtype: GUID,
    pub output_format: Option<PixelFormat>,
    pub width: u32,
    pub height: u32,
    pub decode: DecodeKind,
    /// Friendly name of the decoder MFT, when it reports one.
    pub decoder_name: Option<String>,
    /// The reader was given a D3D manager (frames may be textures).
    pub d3d_manager: bool,
}

/// Why the stream stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopReason {
    /// Unplugged or invalidated. The HRESULT is `None` when only a removal notice said so.
    DeviceLost(Option<HRESULT>),
    /// Another process took the camera.
    InUse(HRESULT),
    EndOfStream,
    Error(HRESULT),
    /// `close` was called.
    Closed,
}

impl fmt::Display for StopReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StopReason::DeviceLost(Some(hr)) => write!(f, "device lost ({})", hr_text(*hr)),
            StopReason::DeviceLost(None) => write!(f, "device lost (removal notice)"),
            StopReason::InUse(hr) => write!(f, "camera taken by another app ({})", hr_text(*hr)),
            StopReason::EndOfStream => write!(f, "end of stream"),
            StopReason::Error(hr) => write!(f, "stream error ({})", hr_text(*hr)),
            StopReason::Closed => write!(f, "closed"),
        }
    }
}

/// Result of waiting for a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecvError {
    /// Nothing arrived in time; the stream is still running.
    Timeout,
    /// The stream ended and the queue is empty.
    Stopped(StopReason),
}

/// Counters since open.
#[derive(Clone, Copy, Debug, Default)]
pub struct ReaderStats {
    /// Frames the callback produced.
    pub produced: u64,
    /// Frames dropped because the consumer was slow (oldest first).
    pub dropped: u64,
    /// Frames waiting in the queue now.
    pub queued: usize,
}

/// The pixels of a frame.
pub enum FrameData {
    /// A decoder surface. `subresource` is the array slice (`IMFDXGIBuffer::GetSubresourceIndex`). The texture may
    /// lack `D3D11_BIND_SHADER_RESOURCE`; copy the slice into your own texture with `CopySubresourceRegion`.
    Texture {
        texture: ID3D11Texture2D,
        subresource: u32,
    },
    /// System memory, locked for the life of the frame.
    Cpu(CpuBuffer),
}

/// One captured frame.
pub struct Frame {
    /// Counts frames the callback produced, starting at 0 (gaps mean dropped frames).
    pub sequence: u64,
    /// Capture time from the source, QPC in 100 ns units (the clock of [`qpc_now_100ns`]).
    pub timestamp_100ns: i64,
    /// [`qpc_now_100ns`] when the callback received the sample. `arrival - timestamp` is the pipeline latency.
    pub arrival_100ns: i64,
    pub width: u32,
    pub height: u32,
    pub subtype: GUID,
    /// `None` for subtypes outside [`PixelFormat`].
    pub format: Option<PixelFormat>,
    pub data: FrameData,
    _sample: Agile<IMFSample>,
}

// SAFETY: a frame holds an MF sample, a D3D11 texture (device objects are free-threaded) and a locked MF buffer;
// none of them is tied to the thread that made it. Using the texture still needs the device's context rules.
unsafe impl Send for Frame {}
unsafe impl Sync for Frame {}

impl Frame {
    pub fn is_texture(&self) -> bool {
        matches!(self.data, FrameData::Texture { .. })
    }
}

impl fmt::Debug for Frame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Frame")
            .field("sequence", &self.sequence)
            .field("timestamp_100ns", &self.timestamp_100ns)
            .field("size", &(self.width, self.height))
            .field("format", &self.format)
            .field("texture", &self.is_texture())
            .finish()
    }
}

/// A locked system-memory frame.
///
/// For planar 4:2:0 formats the chroma plane follows the luma plane in the same allocation. Where it starts is
/// the next phase's question (`stride * height` in practice; confirm against `MF_MT_DEFAULT_STRIDE` and the
/// buffer length when adding the converter).
pub struct CpuBuffer {
    buffer: IMFMediaBuffer,
    two_d: Option<IMF2DBuffer>,
    /// Lowest address of the image.
    base: *const u8,
    len: usize,
    stride: i32,
}

// SAFETY: the locked memory is plain bytes owned by a free-threaded MF buffer that stays locked until drop.
unsafe impl Send for CpuBuffer {}
unsafe impl Sync for CpuBuffer {}

impl CpuBuffer {
    fn lock(
        buffer: IMFMediaBuffer,
        width: u32,
        height: u32,
        format: Option<PixelFormat>,
    ) -> windows::core::Result<CpuBuffer> {
        let rows = format.map_or(height, |f| f.total_rows(height)) as usize;
        // SAFETY: Lock2D/Lock hand out a pointer valid until the matching unlock in Drop.
        unsafe {
            if let Ok(b2) = buffer.cast::<IMF2DBuffer>() {
                let mut scan0 = std::ptr::null_mut();
                let mut pitch = 0i32;
                b2.Lock2D(&mut scan0, &mut pitch)?;
                let tight = pitch.unsigned_abs() as usize * rows;
                let max = buffer.GetMaxLength().map(|m| m as usize).unwrap_or(tight);
                let len = tight.min(max.max(1));
                // Bottom-up images (negative pitch) start rows-1 pitches before the first scanline.
                let base = if pitch < 0 {
                    scan0.offset(pitch as isize * (rows as isize - 1))
                } else {
                    scan0
                };
                return Ok(CpuBuffer {
                    buffer,
                    two_d: Some(b2),
                    base,
                    len,
                    stride: pitch,
                });
            }
            let (mut data, mut cur) = (std::ptr::null_mut(), 0u32);
            buffer.Lock(&mut data, None, Some(&mut cur))?;
            let stride = format.map_or(0, |f| f.min_stride(width)) as i32;
            Ok(CpuBuffer {
                buffer,
                two_d: None,
                base: data,
                len: cur as usize,
                stride,
            })
        }
    }

    /// The image bytes from its lowest address. Rows are `stride().unsigned_abs()` bytes apart.
    pub fn data(&self) -> &[u8] {
        // SAFETY: base/len describe the locked region, valid until drop.
        unsafe { std::slice::from_raw_parts(self.base, self.len) }
    }

    /// Bytes from one row to the next. Negative means the image is stored bottom-up and `data()` starts with
    /// the last row.
    pub fn stride(&self) -> i32 {
        self.stride
    }
}

impl Drop for CpuBuffer {
    fn drop(&mut self) {
        // SAFETY: balances the lock taken in `lock`.
        unsafe {
            match &self.two_d {
                Some(b2) => {
                    let _ = b2.Unlock2D();
                }
                None => {
                    let _ = self.buffer.Unlock();
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Shared state between the callback thread and the consumer

#[derive(Default)]
struct QueueState {
    frames: VecDeque<Frame>,
    stop: Option<StopReason>,
    flushed: bool,
    first_frame: bool,
}

#[derive(Clone, Copy)]
struct Output {
    width: u32,
    height: u32,
    subtype: GUID,
}

struct Shared {
    state: Mutex<QueueState>,
    cv: Condvar,
    reader: Mutex<Option<Agile<IMFSourceReader>>>,
    output: Mutex<Output>,
    capacity: usize,
    produced: AtomicU64,
    dropped: AtomicU64,
    stopping: AtomicBool,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Shared {
    fn finish(&self, reason: StopReason) {
        let mut st = lock(&self.state);
        if st.stop.is_none() {
            st.stop = Some(reason);
        }
        self.cv.notify_all();
    }

    fn push(&self, frame: Frame) {
        let evicted = {
            let mut st = lock(&self.state);
            let evicted = if st.frames.len() >= self.capacity {
                st.frames.pop_front()
            } else {
                None
            };
            st.frames.push_back(frame);
            st.first_frame = true;
            self.cv.notify_all();
            evicted
        };
        if evicted.is_some() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        // `evicted` (an MF sample, possibly a locked buffer) is released here, outside the queue lock.
    }

    fn request_read(&self) -> windows::core::Result<()> {
        let reader = lock(&self.reader).as_ref().map(|r| r.0.clone());
        match reader {
            // SAFETY: async ReadSample; results arrive in OnReadSample.
            Some(r) => unsafe { r.ReadSample(FIRST_VIDEO_STREAM, 0, None, None, None, None) },
            None => Ok(()),
        }
    }

    fn refresh_output(&self) {
        let reader = lock(&self.reader).as_ref().map(|r| r.0.clone());
        let Some(reader) = reader else { return };
        // SAFETY: plain call.
        if let Ok(t) = unsafe { reader.GetCurrentMediaType(FIRST_VIDEO_STREAM) } {
            *lock(&self.output) = output_of(&t);
        }
    }

    fn build_frame(
        &self,
        sample: &IMFSample,
        timestamp: i64,
        arrival: i64,
    ) -> windows::core::Result<Frame> {
        let out = *lock(&self.output);
        let format = PixelFormat::from_guid(&out.subtype);
        // SAFETY: plain COM calls on a sample the reader handed us.
        let data = unsafe {
            let buffer = if sample.GetBufferCount()? == 1 {
                sample.GetBufferByIndex(0)?
            } else {
                sample.ConvertToContiguousBuffer()?
            };
            if let Ok(dxgi) = buffer.cast::<IMFDXGIBuffer>() {
                let mut raw = std::ptr::null_mut();
                dxgi.GetResource(&ID3D11Texture2D::IID, &mut raw)?;
                let texture = ID3D11Texture2D::from_raw(raw);
                FrameData::Texture {
                    texture,
                    subresource: dxgi.GetSubresourceIndex()?,
                }
            } else {
                FrameData::Cpu(CpuBuffer::lock(buffer, out.width, out.height, format)?)
            }
        };
        Ok(Frame {
            sequence: self.produced.fetch_add(1, Ordering::Relaxed),
            timestamp_100ns: timestamp,
            arrival_100ns: arrival,
            width: out.width,
            height: out.height,
            subtype: out.subtype,
            format,
            data,
            _sample: Agile(sample.clone()),
        })
    }
}

fn output_of(t: &IMFMediaType) -> Output {
    // SAFETY: plain attribute reads.
    let (size, subtype) = unsafe {
        (
            t.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0),
            t.GetGUID(&MF_MT_SUBTYPE).unwrap_or_default(),
        )
    };
    Output {
        width: (size >> 32) as u32,
        height: size as u32,
        subtype,
    }
}

// ---------------------------------------------------------------------------------------------------------------
// The Source Reader callback

#[implement(IMFSourceReaderCallback)]
struct ReaderCallback {
    shared: Arc<Shared>,
}

impl ReaderCallback {
    fn on_sample(&self, hr: HRESULT, flags: u32, timestamp: i64, sample: Option<&IMFSample>) {
        let sh = &self.shared;
        if sh.stopping.load(Ordering::Acquire) {
            return;
        }
        let arrival = qpc_now_100ns();
        if hr.is_err() {
            sh.finish(stop_reason_for(hr));
            return;
        }
        let flags = flags as i32;
        if flags & MF_SOURCE_READERF_ERROR.0 != 0 {
            sh.finish(StopReason::Error(hr));
            return;
        }
        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 != 0 {
            sh.finish(StopReason::EndOfStream);
            return;
        }
        if flags & MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 != 0 {
            sh.refresh_output();
        }
        if let Some(s) = sample {
            match sh.build_frame(s, timestamp, arrival) {
                Ok(frame) => sh.push(frame),
                Err(e) => {
                    sh.finish(stop_reason_for(e.code()));
                    return;
                }
            }
        }
        if let Err(e) = sh.request_read() {
            sh.finish(stop_reason_for(e.code()));
        }
    }
}

fn stop_reason_for(hr: HRESULT) -> StopReason {
    match classify_hresult(hr) {
        ErrorClass::DeviceLost => StopReason::DeviceLost(Some(hr)),
        ErrorClass::InUse => StopReason::InUse(hr),
        ErrorClass::Other => StopReason::Error(hr),
    }
}

impl IMFSourceReaderCallback_Impl for ReaderCallback_Impl {
    fn OnReadSample(
        &self,
        hrstatus: HRESULT,
        _stream: u32,
        flags: u32,
        timestamp: i64,
        sample: Ref<IMFSample>,
    ) -> windows::core::Result<()> {
        // A panic must not unwind into Media Foundation.
        let r = catch_unwind(AssertUnwindSafe(|| {
            self.on_sample(hrstatus, flags, timestamp, sample.as_ref())
        }));
        if r.is_err() {
            self.shared
                .finish(StopReason::Error(windows::Win32::Foundation::E_UNEXPECTED));
        }
        Ok(())
    }

    fn OnFlush(&self, _stream: u32) -> windows::core::Result<()> {
        let mut st = lock(&self.shared.state);
        st.flushed = true;
        self.shared.cv.notify_all();
        Ok(())
    }

    fn OnEvent(&self, _stream: u32, event: Ref<IMFMediaEvent>) -> windows::core::Result<()> {
        let _ = catch_unwind(AssertUnwindSafe(|| {
            let Some(ev) = event.as_ref() else { return };
            // SAFETY: plain calls.
            let kind = unsafe { ev.GetType() }.unwrap_or(0);
            if kind == MEVideoCaptureDeviceRemoved.0 as u32 {
                self.shared.finish(StopReason::DeviceLost(Some(
                    MF_E_VIDEO_RECORDING_DEVICE_INVALIDATED,
                )));
            } else if kind == MEVideoCaptureDevicePreempted.0 as u32 {
                self.shared
                    .finish(StopReason::InUse(MF_E_VIDEO_RECORDING_DEVICE_PREEMPTED));
            } else if kind == MEError.0 as u32 {
                let hr = unsafe { ev.GetStatus() }.unwrap_or(windows::Win32::Foundation::E_FAIL);
                if hr.is_err() {
                    self.shared.finish(stop_reason_for(hr));
                }
            }
        }));
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------------------------
// The reader

/// An open camera streaming into a bounded frame queue.
pub struct CaptureReader {
    source: Agile<IMFMediaSource>,
    reader: Agile<IMFSourceReader>,
    shared: Arc<Shared>,
    info: StreamInfo,
    closed: bool,
    _gpu: Option<Arc<GpuDevice>>,
    // Last: Media Foundation shuts down after everything above is released.
    _mf: MfSession,
}

impl CaptureReader {
    /// Open the camera, select the format, start streaming, and wait for the first frame.
    ///
    /// Errors: [`CaptureError::InUse`] when another process holds the camera (the error surfaces on the first
    /// read, so it can only be seen here), [`CaptureError::FormatUnavailable`] when the camera lacks the format,
    /// [`CaptureError::Timeout`] when nothing happens within `first_frame_timeout`.
    pub fn open(opts: OpenOptions) -> Result<CaptureReader, CaptureError> {
        let mf = MfSession::start()?;
        let source = create_source(&opts.link)?;
        match Self::open_on(mf, source.clone(), &opts) {
            Ok(r) => Ok(r),
            Err(e) => {
                // SAFETY: plain call; frees the camera again.
                let _ = unsafe { source.Shutdown() };
                Err(e)
            }
        }
    }

    fn open_on(
        mf: MfSession,
        source: IMFMediaSource,
        opts: &OpenOptions,
    ) -> Result<CaptureReader, CaptureError> {
        let use_d3d = opts.hw_decode && opts.gpu.is_some();
        let shared = Arc::new(Shared {
            state: Mutex::new(QueueState::default()),
            cv: Condvar::new(),
            reader: Mutex::new(None),
            output: Mutex::new(Output {
                width: 0,
                height: 0,
                subtype: GUID::zeroed(),
            }),
            capacity: opts.queue_len.max(1),
            produced: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            stopping: AtomicBool::new(false),
        });
        let callback: IMFSourceReaderCallback = ReaderCallback {
            shared: shared.clone(),
        }
        .into();

        let call = |what: &'static str, r: windows::core::Result<()>| {
            r.map_err(|e| CaptureError::from_call(what, e))
        };
        let attrs = attributes(5)?;
        // SAFETY: plain attribute setters; MF takes its own references.
        unsafe {
            call(
                "SetUnknown(ASYNC_CALLBACK)",
                attrs.SetUnknown(&MF_SOURCE_READER_ASYNC_CALLBACK, &callback),
            )?;
            call(
                "SetUINT32(DISCONNECT_MEDIASOURCE_ON_SHUTDOWN)",
                attrs.SetUINT32(&MF_SOURCE_READER_DISCONNECT_MEDIASOURCE_ON_SHUTDOWN, 1),
            )?;
            if use_d3d {
                if let Some(gpu) = &opts.gpu {
                    call(
                        "SetUnknown(D3D_MANAGER)",
                        attrs.SetUnknown(&MF_SOURCE_READER_D3D_MANAGER, &gpu.manager),
                    )?;
                }
                call(
                    "SetUINT32(ENABLE_HARDWARE_TRANSFORMS)",
                    attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1),
                )?;
            }
        }
        // SAFETY: plain call.
        let reader = unsafe { MFCreateSourceReaderFromMediaSource(&source, &attrs) }
            .map_err(|e| CaptureError::from_call("MFCreateSourceReaderFromMediaSource", e))?;

        // Formats and choice.
        let formats = native_formats(&reader)?;
        let choice = choose_format(&formats, &opts.request)?;
        // SAFETY: plain calls.
        unsafe {
            call(
                "SetStreamSelection(all, false)",
                reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false),
            )?;
            call(
                "SetStreamSelection(video, true)",
                reader.SetStreamSelection(FIRST_VIDEO_STREAM, true),
            )?;
        }
        let native_type = select_native_type(&reader, &choice)?;

        // Output type: raw formats keep their subtype; compressed ones are decoded to NV12 (P010 for 10-bit HEVC).
        let out_type = output_type(&native_type, &choice)?;
        // SAFETY: plain call.
        call("SetCurrentMediaType", unsafe {
            reader.SetCurrentMediaType(FIRST_VIDEO_STREAM, None, &out_type)
        })?;
        // SAFETY: plain call.
        let current = unsafe { reader.GetCurrentMediaType(FIRST_VIDEO_STREAM) }
            .map_err(|e| CaptureError::from_call("GetCurrentMediaType", e))?;
        let out = output_of(&current);
        *lock(&shared.output) = out;
        let (decode, decoder_name) = detect_decode(&reader);

        let info = StreamInfo {
            native: choice.native.clone(),
            fps: choice.fps,
            fps_approximate: choice.fps_approximate,
            output_subtype: out.subtype,
            output_format: PixelFormat::from_guid(&out.subtype),
            width: out.width,
            height: out.height,
            decode,
            decoder_name,
            d3d_manager: use_d3d,
        };

        *lock(&shared.reader) = Some(Agile(reader.clone()));
        let mut this = CaptureReader {
            source: Agile(source),
            reader: Agile(reader),
            shared: shared.clone(),
            info,
            closed: false,
            _gpu: opts.gpu.clone(),
            _mf: mf,
        };
        if let Err(e) = shared.request_read() {
            this.close();
            return Err(CaptureError::from_call("ReadSample", e));
        }
        if let Err(e) = this.wait_first_frame(opts.first_frame_timeout) {
            this.close();
            return Err(e);
        }
        Ok(this)
    }

    /// Block until a frame is queued or the stream stopped before delivering any.
    fn wait_first_frame(&self, timeout: Duration) -> Result<(), CaptureError> {
        let deadline = Instant::now() + timeout;
        let mut st = lock(&self.shared.state);
        loop {
            if st.first_frame {
                return Ok(());
            }
            if let Some(reason) = st.stop {
                let hr = match reason {
                    StopReason::DeviceLost(Some(h))
                    | StopReason::InUse(h)
                    | StopReason::Error(h) => h,
                    _ => windows::Win32::Foundation::E_ABORT,
                };
                let err = windows::core::Error::from(hr);
                return Err(match reason {
                    StopReason::InUse(_) => CaptureError::InUse(err),
                    StopReason::DeviceLost(_) => CaptureError::DeviceLost(err),
                    _ => CaptureError::from_call("first ReadSample", err),
                });
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(CaptureError::Timeout("the first frame"));
            }
            st = self
                .shared
                .cv
                .wait_timeout(st, deadline - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// What was negotiated.
    pub fn info(&self) -> &StreamInfo {
        &self.info
    }

    /// The camera's media source. The next phase queries `IKsControl`, `IAMVideoProcAmp` and `IAMCameraControl`
    /// on it. It stays valid until `close`/drop.
    pub fn media_source(&self) -> &IMFMediaSource {
        &self.source.0
    }

    /// The Source Reader (for `GetServiceForStream`, diagnostics).
    pub fn source_reader(&self) -> &IMFSourceReader {
        &self.reader.0
    }

    pub fn stats(&self) -> ReaderStats {
        ReaderStats {
            produced: self.shared.produced.load(Ordering::Relaxed),
            dropped: self.shared.dropped.load(Ordering::Relaxed),
            queued: lock(&self.shared.state).frames.len(),
        }
    }

    /// Wait for the next frame. Queued frames are delivered before a stop is reported.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<Frame, RecvError> {
        let deadline = Instant::now() + timeout;
        let mut st = lock(&self.shared.state);
        loop {
            if let Some(f) = st.frames.pop_front() {
                return Ok(f);
            }
            if let Some(reason) = st.stop {
                return Err(RecvError::Stopped(reason));
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(RecvError::Timeout);
            }
            st = self
                .shared
                .cv
                .wait_timeout(st, deadline - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// The next frame if one is queued (`RecvError::Timeout` when empty).
    pub fn try_recv(&self) -> Result<Frame, RecvError> {
        self.recv_timeout(Duration::ZERO)
    }

    /// Why the stream stopped, if it has.
    pub fn stop_reason(&self) -> Option<StopReason> {
        lock(&self.shared.state).stop
    }

    /// Report a removal learned elsewhere (a hotplug notice for this camera's link). The stream then reports
    /// `DeviceLost`; the caller still calls `close`.
    pub fn report_device_removed(&self) {
        self.shared.finish(StopReason::DeviceLost(None));
    }

    /// Stop streaming and release the camera. Safe to call twice; `Drop` calls it.
    pub fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        self.shared.stopping.store(true, Ordering::Release);
        self.shared.finish(StopReason::Closed);

        // Flush so no read is in flight, then wait for OnFlush (short: a dead device may never answer).
        // SAFETY: plain call.
        if unsafe { self.reader.0.Flush(MF_SOURCE_READER_ALL_STREAMS.0 as u32) }.is_ok() {
            let deadline = Instant::now() + Duration::from_secs(2);
            let mut st = lock(&self.shared.state);
            while !st.flushed {
                let now = Instant::now();
                if now >= deadline {
                    break;
                }
                st = self
                    .shared
                    .cv
                    .wait_timeout(st, deadline - now)
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
        }
        // Break the reader -> callback -> reader cycle, release queued frames, then free the camera.
        *lock(&self.shared.reader) = None;
        let frames = std::mem::take(&mut lock(&self.shared.state).frames);
        drop(frames);
        // SAFETY: plain call. Required: DISCONNECT_MEDIASOURCE_ON_SHUTDOWN leaves the source to us.
        let _ = unsafe { self.source.0.Shutdown() };
    }
}

impl Drop for CaptureReader {
    fn drop(&mut self) {
        self.close();
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Format negotiation helpers

/// Select the chosen native type on the source (plan 4.1) with `IMFSourceReaderEx::SetNativeMediaType`.
///
/// The reader forwards this to the source's media type handler, but also records it as the type to decode from.
/// Setting the handler directly is not enough on Windows 11: Frame Server cameras list NV12 natively next to
/// MJPEG, so a later `SetCurrentMediaType(NV12)` on the reader matches a native NV12 type of another size and
/// silently abandons the MJPEG choice.
fn select_native_type(
    reader: &IMFSourceReader,
    choice: &FormatChoice,
) -> Result<IMFMediaType, CaptureError> {
    let api = |what: &'static str| move |e: windows::core::Error| CaptureError::from_call(what, e);
    // SAFETY: plain COM calls.
    unsafe {
        let native = reader
            .GetNativeMediaType(FIRST_VIDEO_STREAM, choice.native.index)
            .map_err(api("GetNativeMediaType"))?;
        let chosen = if choice.override_fps {
            let copy = MFCreateMediaType().map_err(api("MFCreateMediaType"))?;
            native.CopyAllItems(&copy).map_err(api("CopyAllItems"))?;
            copy.SetUINT64(&MF_MT_FRAME_RATE, choice.fps.pack())
                .map_err(api("SetUINT64(FRAME_RATE)"))?;
            copy
        } else {
            native
        };
        let ex = reader
            .cast::<IMFSourceReaderEx>()
            .map_err(api("QueryInterface(IMFSourceReaderEx)"))?;
        ex.SetNativeMediaType(FIRST_VIDEO_STREAM, &chosen)
            .map_err(api("IMFSourceReaderEx::SetNativeMediaType"))?;
        Ok(chosen)
    }
}

/// The type to ask the reader for: the native type itself for raw formats, NV12/P010 for compressed ones.
fn output_type(native: &IMFMediaType, choice: &FormatChoice) -> Result<IMFMediaType, CaptureError> {
    let format = choice.native.format;
    if !format.is_some_and(PixelFormat::is_compressed) {
        return Ok(native.clone());
    }
    let ten_bit = format == Some(PixelFormat::Hevc)
        // SAFETY: plain attribute read.
        && unsafe { native.GetUINT32(&MF_MT_MPEG2_PROFILE) }.ok() == Some(eAVEncH265VProfile_Main_420_10.0 as u32);
    let want = if ten_bit {
        PixelFormat::P010
    } else {
        PixelFormat::Nv12
    };
    let api = |what: &'static str| move |e: windows::core::Error| CaptureError::from_call(what, e);
    // SAFETY: plain COM calls.
    unsafe {
        let t = MFCreateMediaType().map_err(api("MFCreateMediaType"))?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(api("SetGUID(MAJOR_TYPE)"))?;
        t.SetGUID(&MF_MT_SUBTYPE, &want.guid())
            .map_err(api("SetGUID(SUBTYPE)"))?;
        Ok(t)
    }
}

/// Look at the transforms the reader inserted: a video decoder means decoding; a hardware URL or vendor id on it
/// means hardware.
fn detect_decode(reader: &IMFSourceReader) -> (DecodeKind, Option<String>) {
    let Ok(ex) = reader.cast::<IMFSourceReaderEx>() else {
        return (DecodeKind::None, None);
    };
    for i in 0..16u32 {
        let mut category = GUID::zeroed();
        let mut transform = None;
        // SAFETY: out pointers are locals.
        let r = unsafe {
            ex.GetTransformForStream(FIRST_VIDEO_STREAM, i, Some(&mut category), &mut transform)
        };
        match r {
            Ok(()) => {}
            Err(e) if e.code() == MF_E_INVALIDINDEX => break,
            Err(_) => break,
        }
        if category != MFT_CATEGORY_VIDEO_DECODER {
            continue;
        }
        let Some(t) = transform else { continue };
        // SAFETY: plain calls.
        unsafe {
            let attrs = t.GetAttributes().ok();
            let hardware = attrs.as_ref().is_some_and(|a| {
                a.GetItemType(&MFT_ENUM_HARDWARE_URL_Attribute).is_ok()
                    || a.GetItemType(&MFT_ENUM_HARDWARE_VENDOR_ID_Attribute)
                        .is_ok()
            });
            let name = attrs.as_ref().and_then(|a| {
                let mut p = windows::core::PWSTR::null();
                let mut len = 0u32;
                a.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut p, &mut len)
                    .ok()?;
                let s = p.to_string().ok();
                windows::Win32::System::Com::CoTaskMemFree(Some(p.0 as *const _));
                s
            });
            return (
                if hardware {
                    DecodeKind::Hardware
                } else {
                    DecodeKind::Software
                },
                name,
            );
        }
    }
    (DecodeKind::None, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::{list_camera_keys, FormatRequest};

    fn shared(capacity: usize) -> Arc<Shared> {
        Arc::new(Shared {
            state: Mutex::new(QueueState::default()),
            cv: Condvar::new(),
            reader: Mutex::new(None),
            output: Mutex::new(Output {
                width: 0,
                height: 0,
                subtype: GUID::zeroed(),
            }),
            capacity,
            produced: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            stopping: AtomicBool::new(false),
        })
    }

    /// A CPU frame backed by a real Media Foundation buffer.
    fn cpu_frame(sh: &Shared, seq: u64) -> Frame {
        let _mf = MfSession::start().unwrap();
        let buffer =
            unsafe { windows::Win32::Media::MediaFoundation::MFCreateMemoryBuffer(16) }.unwrap();
        let sample = unsafe { windows::Win32::Media::MediaFoundation::MFCreateSample() }.unwrap();
        unsafe { sample.AddBuffer(&buffer).unwrap() };
        let _ = sh;
        Frame {
            sequence: seq,
            timestamp_100ns: seq as i64,
            arrival_100ns: seq as i64,
            width: 4,
            height: 4,
            subtype: PixelFormat::Yuy2.guid(),
            format: Some(PixelFormat::Yuy2),
            data: FrameData::Cpu(CpuBuffer::lock(buffer, 4, 4, Some(PixelFormat::Yuy2)).unwrap()),
            _sample: Agile(sample),
        }
    }

    #[test]
    fn queue_drops_the_oldest_and_counts() {
        let sh = shared(2);
        for i in 0..5 {
            sh.push(cpu_frame(&sh, i));
        }
        assert_eq!(sh.dropped.load(Ordering::Relaxed), 3);
        let st = lock(&sh.state);
        let seqs: Vec<u64> = st.frames.iter().map(|f| f.sequence).collect();
        assert_eq!(seqs, vec![3, 4], "the newest frames survive");
    }

    #[test]
    fn cpu_buffer_is_locked_and_readable() {
        let sh = shared(1);
        let f = cpu_frame(&sh, 0);
        let FrameData::Cpu(cpu) = &f.data else {
            panic!("expected a CPU frame")
        };
        // A plain memory buffer has no 2D interface: tight stride for the format, current length (0) of data.
        assert_eq!(cpu.stride(), 8);
        assert!(cpu.data().is_empty());
        assert!(!f.is_texture());
    }

    #[test]
    fn stop_is_reported_after_queued_frames_drain() {
        let sh = shared(4);
        sh.push(cpu_frame(&sh, 0));
        sh.finish(StopReason::DeviceLost(Some(
            MF_E_VIDEO_RECORDING_DEVICE_INVALIDATED,
        )));
        sh.finish(StopReason::EndOfStream); // the first reason wins
        let mut st = lock(&sh.state);
        assert!(st.frames.pop_front().is_some());
        assert_eq!(
            st.stop,
            Some(StopReason::DeviceLost(Some(
                MF_E_VIDEO_RECORDING_DEVICE_INVALIDATED
            )))
        );
    }

    #[test]
    fn stop_reasons_follow_the_hresult_class() {
        assert_eq!(
            stop_reason_for(MF_E_VIDEO_RECORDING_DEVICE_INVALIDATED),
            StopReason::DeviceLost(Some(MF_E_VIDEO_RECORDING_DEVICE_INVALIDATED))
        );
        assert!(matches!(
            stop_reason_for(
                windows::Win32::Media::MediaFoundation::MF_E_HW_MFT_FAILED_START_STREAMING
            ),
            StopReason::InUse(_)
        ));
        assert!(matches!(
            stop_reason_for(windows::Win32::Foundation::E_FAIL),
            StopReason::Error(_)
        ));
        assert_eq!(
            StopReason::DeviceLost(None).to_string(),
            "device lost (removal notice)"
        );
    }

    #[test]
    fn opening_the_own_virtual_camera_is_refused() {
        let link = "\\\\?\\swd#vcamdevapi#abc#{e5323777-f976-4f5b-9b55-b94699c46e44}\\{fcebba03-9d13-4c13-9940-cc84fcd132d1}";
        let req = FormatRequest {
            format: PixelFormat::Nv12,
            size: None,
            fps: None,
        };
        assert!(matches!(
            CaptureReader::open(OpenOptions::new(link, req)),
            Err(CaptureError::OwnVirtualCamera)
        ));
    }

    #[test]
    fn opening_a_missing_camera_fails_cleanly() {
        let req = FormatRequest {
            format: PixelFormat::Nv12,
            size: None,
            fps: None,
        };
        let link =
            "\\\\?\\usb#vid_dead&pid_beef#none#{e5323777-f976-4f5b-9b55-b94699c46e44}\\global";
        assert!(CaptureReader::open(OpenOptions::new(link, req)).is_err());
        let _ = list_camera_keys();
    }
}
