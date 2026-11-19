//! In-process tests: load the class factory through `DllGetClassObject` and drive Activator -> MediaSource ->
//! MediaStream -> samples exactly as Frame Server would, without Frame Server. A writer thread feeds a temporary
//! `frames.bin` (the path is overridden for the test thread only; see `frames::set_test_path`).

use std::os::windows::io::OwnedHandle;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use vcam_protocol::pattern::{self, Analysis};
use vcam_protocol::slate::SlateKind;
use vcam_protocol::testdir::TestFile;
use vcam_protocol::{
    off, qpc_now, Format, SlotFormat, Writer, WriterOptions, WriterState, SUBTYPE_NV12,
    SUBTYPE_RGB32,
};
use windows::Win32::Foundation::{CLASS_E_CLASSNOTAVAILABLE, E_UNEXPECTED, S_FALSE};
use windows::Win32::Media::KernelStreaming::{IKsControl, KSIDENTIFIER, PINNAME_VIDEO_CAPTURE};
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer, IMFActivate, IMFAttributes, IMFGetService, IMFMediaEvent, IMFMediaEventGenerator,
    IMFMediaSource, IMFMediaSource2, IMFMediaSourceEx, IMFMediaStream2, IMFMediaType,
    IMFPresentationDescriptor, IMFSample, IMFSampleAllocatorControl, MEMediaSample, MENewStream,
    MESourceStarted, MESourceStopped, MEStreamStarted, MEStreamStopped, MFFrameSourceTypes_Color,
    MFSampleAllocatorUsage_UsesCustomAllocator, MFStartup, MFVideoFormat_NV12, MFVideoFormat_RGB32,
    MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS, MFSTARTUP_FULL,
    MF_DEVICESTREAM_ATTRIBUTE_FRAMESOURCE_TYPES, MF_DEVICESTREAM_FRAMESERVER_SHARED,
    MF_DEVICESTREAM_STREAM_CATEGORY, MF_DEVICESTREAM_STREAM_ID, MF_EVENT_FLAG_NO_WAIT,
    MF_E_INVALID_STATE_TRANSITION, MF_E_UNSUPPORTED_SERVICE, MF_MT_SUBTYPE, MF_VERSION,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{CoInitializeEx, IClassFactory, COINIT_MULTITHREADED};
use windows_core::{Interface, GUID, HRESULT};

use crate::frames::{decoded_slate, set_test_path};
use crate::mediatype::{FrameFormat, PixelFormat};

fn init() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, MFSTARTUP_FULL).unwrap();
    }
}

fn factory() -> IClassFactory {
    let mut out: *mut core::ffi::c_void = std::ptr::null_mut();
    let clsid = crate::clsid();
    let hr = unsafe { crate::DllGetClassObject(&clsid, &IClassFactory::IID, &mut out) };
    assert!(hr.is_ok(), "DllGetClassObject: {hr:?}");
    unsafe { IClassFactory::from_raw(out) }
}

fn activate_source() -> (IMFActivate, IMFMediaSource) {
    let activate: IMFActivate = unsafe { factory().CreateInstance(None).unwrap() };
    let source: IMFMediaSource = unsafe { activate.ActivateObject().unwrap() };
    (activate, source)
}

/// A temporary frame file path for this test, set as the override of the calling thread.
fn test_file(name: &str) -> TestFile {
    let p = TestFile::new("bettercam-dll-tests", name);
    set_test_path(Some(p.to_path_buf()));
    p
}

/// Wait for the next event with a timeout.
fn next_event(g: &IMFMediaEventGenerator, timeout: Duration) -> IMFMediaEvent {
    let start = Instant::now();
    loop {
        match unsafe {
            g.GetEvent(MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(
                MF_EVENT_FLAG_NO_WAIT.0,
            ))
        } {
            Ok(e) => return e,
            Err(_) if start.elapsed() < timeout => std::thread::sleep(Duration::from_millis(1)),
            Err(e) => panic!("no event within {timeout:?}: {e}"),
        }
    }
}

fn event_type(e: &IMFMediaEvent) -> u32 {
    unsafe { e.GetType().unwrap() }
}

fn event_unknown(e: &IMFMediaEvent) -> windows_core::IUnknown {
    let v: PROPVARIANT = unsafe { e.GetValue().unwrap() };
    windows_core::IUnknown::try_from(&v).expect("event value is not an IUnknown")
}

/// Run `f` on the luma/pixel rows of a sample.
fn with_rows<T>(
    sample: &IMFSample,
    fmt: &FrameFormat,
    f: impl FnOnce(&dyn Fn(u32) -> Vec<u8>) -> T,
) -> T {
    let buffer = unsafe { sample.ConvertToContiguousBuffer().unwrap() };
    let b2: IMF2DBuffer = buffer.cast().unwrap();
    let mut scan0 = std::ptr::null_mut();
    let mut pitch = 0i32;
    unsafe { b2.Lock2D(&mut scan0, &mut pitch).unwrap() };
    assert!(pitch > 0);
    let row_bytes = match fmt.pixel {
        PixelFormat::Nv12 => fmt.width as usize,
        PixelFormat::Rgb32 => fmt.width as usize * 4,
    };
    let rows = |r: u32| {
        unsafe { std::slice::from_raw_parts(scan0.add(r as usize * pitch as usize), row_bytes) }
            .to_vec()
    };
    let out = f(&rows);
    unsafe { b2.Unlock2D().unwrap() };
    out
}

fn analyse_sample(sample: &IMFSample, fmt: &FrameFormat) -> Analysis {
    with_rows(sample, fmt, |rows| {
        // analyse() wants borrowed rows; copy the handful it touches
        let cache: Vec<Vec<u8>> = (0..fmt.height).map(rows).collect();
        pattern::analyse(
            |r| &cache[r as usize],
            fmt.pixel.protocol_subtype(),
            fmt.width,
            fmt.height,
        )
    })
}

/// Mean luma difference between a sample and a slate scaled to the sample's size.
fn slate_diff(sample: &IMFSample, fmt: &FrameFormat, kind: SlateKind) -> f64 {
    let slate = decoded_slate(kind);
    let (w, h) = (fmt.width, fmt.height);
    let (want, bgrx) = match fmt.pixel {
        PixelFormat::Nv12 => (slate.scale_nv12(w, h), false),
        PixelFormat::Rgb32 => (slate.scale_bgrx(w, h), true),
    };
    let rb = if bgrx { w as usize * 4 } else { w as usize };
    with_rows(sample, fmt, |rows| {
        vcam_protocol::slate::mean_luma_diff(
            |y| want[y as usize * rb..(y as usize + 1) * rb].to_vec(),
            rows,
            w,
            h,
            bgrx,
            2,
        )
    })
}

/// A frame writer on its own thread, like the app's: it follows the format the DLL negotiated.
struct Feeder {
    writer: Arc<Writer>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<u64>>,
}

impl Feeder {
    fn start(path: &std::path::Path, width: u32, height: u32, fps: u32) -> Feeder {
        Feeder::start_with(
            path,
            &WriterOptions::default(),
            (width, height),
            (fps, fps),
            |_| {},
        )
    }

    /// A feeder that publishes `publish_fps` frames a second while it advertises types of `advertised_fps`
    /// (`prepare` runs on the new writer before it goes Live).
    fn start_with(
        path: &std::path::Path,
        opts: &WriterOptions,
        (width, height): (u32, u32),
        (advertised_fps, fps): (u32, u32),
        prepare: impl FnOnce(&Writer),
    ) -> Feeder {
        let writer = Arc::new(Writer::create_with(path, opts).unwrap());
        writer.set_advertised(&vcam_protocol::advertised_for_output(
            width,
            height,
            advertised_fps,
        ));
        prepare(&writer);
        writer.set_state(WriterState::Live);
        let stop = Arc::new(AtomicBool::new(false));
        let (w2, s2) = (writer.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            let period = Duration::from_secs_f64(1.0 / fps as f64);
            let began = Instant::now();
            let mut n = 0u64;
            while !s2.load(Ordering::Relaxed) {
                let due = began + period * n as u32;
                while Instant::now() < due {
                    if s2.load(Ordering::Relaxed) {
                        return n;
                    }
                    std::hint::spin_loop();
                }
                let f = w2
                    .negotiated()
                    .unwrap_or(Format::new(width, height, fps, SUBTYPE_NV12));
                let stride = if f.subtype == SUBTYPE_RGB32 {
                    f.width * 4
                } else {
                    f.width
                };
                let fmt = SlotFormat {
                    width: f.width,
                    height: f.height,
                    stride,
                    subtype: f.subtype,
                };
                let _ = w2.publish(fmt, qpc_now(), |d| {
                    if f.subtype == SUBTYPE_RGB32 {
                        pattern::fill_bgrx(n, f.width, f.height, d, stride as usize)
                    } else {
                        pattern::fill_nv12(n, f.width, f.height, d, stride as usize)
                    }
                });
                n += 1;
            }
            n
        });
        Feeder {
            writer,
            stop,
            thread: Some(thread),
        }
    }

    /// Stop publishing; the writer stays open (heartbeat no longer refreshed).
    fn stop_publishing(&mut self) -> u64 {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.take().map(|t| t.join().unwrap()).unwrap_or(0)
    }
}

impl Drop for Feeder {
    fn drop(&mut self) {
        self.stop_publishing();
    }
}

/// A started stream: the source, its event generators and the format that was selected.
struct Running {
    source: IMFMediaSource,
    src_gen: IMFMediaEventGenerator,
    stream: IMFMediaStream2,
    sgen: IMFMediaEventGenerator,
    fmt: FrameFormat,
    _activate: IMFActivate,
}

impl Running {
    /// Start the source with the advertised type matching `pixel`/`width`/`height` (first one if several).
    fn start(pixel: PixelFormat, width: u32, height: u32) -> Running {
        Running::start_at(pixel, width, height, None)
    }

    /// [`Running::start`] with the type of that frame rate.
    fn start_fps(pixel: PixelFormat, width: u32, height: u32, fps: u32) -> Running {
        Running::start_at(pixel, width, height, Some(fps))
    }

    fn start_at(pixel: PixelFormat, width: u32, height: u32, fps: Option<u32>) -> Running {
        let (activate, source) = activate_source();
        let pd: IMFPresentationDescriptor =
            unsafe { source.CreatePresentationDescriptor().unwrap() };
        let mut selected = windows_core::BOOL(0);
        let mut desc = None;
        unsafe {
            pd.GetStreamDescriptorByIndex(0, &mut selected, &mut desc)
                .unwrap()
        };
        let handler = unsafe { desc.unwrap().GetMediaTypeHandler().unwrap() };
        let count = unsafe { handler.GetMediaTypeCount().unwrap() };
        let mut chosen: Option<(IMFMediaType, FrameFormat)> = None;
        for i in 0..count {
            let t = unsafe { handler.GetMediaTypeByIndex(i).unwrap() };
            let f = FrameFormat::from_media_type(&t).unwrap();
            if (f.pixel, f.width, f.height) == (pixel, width, height)
                && fps.is_none_or(|r| r == f.fps)
            {
                chosen = Some((t, f));
                break;
            }
        }
        let (mt, fmt) = chosen
            .unwrap_or_else(|| panic!("type {pixel:?} {width}x{height} {fps:?} not advertised"));
        unsafe { handler.SetCurrentMediaType(&mt).unwrap() };
        unsafe {
            source
                .Start(&pd, std::ptr::null(), &PROPVARIANT::default())
                .unwrap()
        };
        let src_gen: IMFMediaEventGenerator = source.cast().unwrap();
        let mut stream: Option<IMFMediaStream2> = None;
        let mut got_started = false;
        for _ in 0..2 {
            let e = next_event(&src_gen, Duration::from_secs(5));
            match event_type(&e) {
                t if t == MESourceStarted.0 as u32 => got_started = true,
                t if t == MENewStream.0 as u32 => stream = Some(event_unknown(&e).cast().unwrap()),
                t => panic!("unexpected source event {t}"),
            }
        }
        assert!(got_started);
        let stream = stream.expect("MENewStream");
        let sgen: IMFMediaEventGenerator = stream.cast().unwrap();
        assert_eq!(
            event_type(&next_event(&sgen, Duration::from_secs(5))),
            MEStreamStarted.0 as u32
        );
        Running {
            source,
            src_gen,
            stream,
            sgen,
            fmt,
            _activate: activate,
        }
    }

    /// Request one sample and wait for it.
    fn pull(&self) -> IMFSample {
        unsafe { self.stream.RequestSample(None).unwrap() };
        let e = next_event(&self.sgen, Duration::from_secs(5));
        assert_eq!(event_type(&e), MEMediaSample.0 as u32);
        event_unknown(&e).cast().unwrap()
    }

    fn stop(self) {
        unsafe { self.source.Stop().unwrap() };
        assert_eq!(
            event_type(&next_event(&self.src_gen, Duration::from_secs(5))),
            MESourceStopped.0 as u32
        );
        assert_eq!(
            event_type(&next_event(&self.sgen, Duration::from_secs(5))),
            MEStreamStopped.0 as u32
        );
        unsafe { self.source.Shutdown().unwrap() };
    }
}

fn sample_time(s: &IMFSample) -> i64 {
    unsafe { s.GetSampleTime().unwrap() }
}

#[test]
fn exports_reject_foreign_clsid_and_report_unload_state() {
    init();
    let wrong = GUID::from_u128(0x1234);
    let mut out = std::ptr::null_mut();
    let hr = unsafe { crate::DllGetClassObject(&wrong, &IClassFactory::IID, &mut out) };
    assert_eq!(hr, CLASS_E_CLASSNOTAVAILABLE);
    assert!(out.is_null());
    let hr = unsafe { crate::DllGetClassObject(std::ptr::null(), &IClassFactory::IID, &mut out) };
    assert!(hr.is_err());
    // a factory exists and is alive -> must not unload
    let _f = factory();
    assert_eq!(crate::DllCanUnloadNow(), S_FALSE);
}

#[test]
fn guard_turns_panics_into_e_unexpected() {
    let r: windows_core::Result<()> = crate::guard::guard("test", || panic!("boom"));
    assert_eq!(r.unwrap_err().code(), E_UNEXPECTED);
    let hr = crate::guard::guard_hr("test", || panic!("boom"));
    assert_eq!(hr, E_UNEXPECTED);
}

#[test]
fn source_exposes_the_required_interfaces_and_descriptors() {
    init();
    set_test_path(None); // no file: the four default types
    let (activate, source) = activate_source();
    let _: IMFMediaSourceEx = source.cast().unwrap();
    let _: IMFMediaSource2 = source.cast().unwrap();
    let _: IMFGetService = source.cast().unwrap();
    let _: IKsControl = source.cast().unwrap();
    let _: IMFSampleAllocatorControl = source.cast().unwrap();

    // IMFActivate is an attribute store that the source shares
    let a: IMFAttributes = activate.cast().unwrap();
    unsafe { a.SetUINT32(&GUID::from_u128(0xabcdef), 42).unwrap() };
    let ex: IMFMediaSourceEx = source.cast().unwrap();
    let sa = unsafe { ex.GetSourceAttributes().unwrap() };
    assert_eq!(
        unsafe { sa.GetUINT32(&GUID::from_u128(0xabcdef)).unwrap() },
        42
    );

    assert_eq!(
        unsafe { source.GetCharacteristics().unwrap() },
        0x1 /* MFMEDIASOURCE_IS_LIVE */
    );

    let pd = unsafe { source.CreatePresentationDescriptor().unwrap() };
    assert_eq!(unsafe { pd.GetStreamDescriptorCount().unwrap() }, 1);
    let mut selected = windows_core::BOOL(0);
    let mut desc = None;
    unsafe {
        pd.GetStreamDescriptorByIndex(0, &mut selected, &mut desc)
            .unwrap()
    };
    let desc = desc.unwrap();
    let da: IMFAttributes = desc.cast().unwrap();
    unsafe {
        assert_eq!(
            da.GetGUID(&MF_DEVICESTREAM_STREAM_CATEGORY).unwrap(),
            PINNAME_VIDEO_CAPTURE
        );
        assert_eq!(da.GetUINT32(&MF_DEVICESTREAM_STREAM_ID).unwrap(), 0);
        assert_eq!(
            da.GetUINT32(&MF_DEVICESTREAM_FRAMESERVER_SHARED).unwrap(),
            1
        );
        assert_eq!(
            da.GetUINT32(&MF_DEVICESTREAM_ATTRIBUTE_FRAMESOURCE_TYPES)
                .unwrap(),
            MFFrameSourceTypes_Color.0 as u32
        );
        // same attributes through GetStreamAttributes
        let sa = ex.GetStreamAttributes(0).unwrap();
        assert_eq!(
            sa.GetGUID(&MF_DEVICESTREAM_STREAM_CATEGORY).unwrap(),
            PINNAME_VIDEO_CAPTURE
        );
        assert!(ex.GetStreamAttributes(7).is_err());
        let handler = desc.GetMediaTypeHandler().unwrap();
        assert_eq!(handler.GetMediaTypeCount().unwrap(), 4);
        let cur = FrameFormat::from_media_type(&handler.GetCurrentMediaType().unwrap()).unwrap();
        assert_eq!(
            (cur.pixel, cur.width, cur.height, cur.fps),
            (PixelFormat::Nv12, 1920, 1080, 30)
        );
        let t1 = handler.GetMediaTypeByIndex(2).unwrap();
        assert_eq!(t1.GetGUID(&MF_MT_SUBTYPE).unwrap(), MFVideoFormat_RGB32);
        let t0 = handler.GetMediaTypeByIndex(0).unwrap();
        assert_eq!(t0.GetGUID(&MF_MT_SUBTYPE).unwrap(), MFVideoFormat_NV12);
    }
    // services and pause are refused the documented way
    let svc: IMFGetService = source.cast().unwrap();
    let r: windows_core::Result<IKsControl> = unsafe { svc.GetService(&GUID::zeroed()) };
    assert_eq!(r.unwrap_err().code(), MF_E_UNSUPPORTED_SERVICE);
    assert_eq!(
        unsafe { source.Pause().unwrap_err().code() },
        MF_E_INVALID_STATE_TRANSITION
    );

    let alloc: IMFSampleAllocatorControl = source.cast().unwrap();
    let (mut input, mut usage) = (0u32, Default::default());
    unsafe { alloc.GetAllocatorUsage(0, &mut input, &mut usage).unwrap() };
    assert_eq!(usage, MFSampleAllocatorUsage_UsesCustomAllocator);

    unsafe { source.Shutdown().unwrap() };
    assert!(
        unsafe { source.GetCharacteristics() }.is_err(),
        "calls fail after Shutdown"
    );
    assert!(
        unsafe { source.Shutdown() }.is_err(),
        "second Shutdown reports MF_E_SHUTDOWN"
    );
}

#[test]
fn ks_control_returns_set_not_found() {
    init();
    set_test_path(None);
    let (_a, source) = activate_source();
    let ks: IKsControl = source.cast().unwrap();
    let ident = KSIDENTIFIER::default();
    let mut returned = 0u32;
    let r = unsafe {
        ks.KsProperty(
            &ident,
            std::mem::size_of::<KSIDENTIFIER>() as u32,
            std::ptr::null_mut(),
            0,
            &mut returned,
        )
    };
    assert_eq!(r.unwrap_err().code(), HRESULT(0x8007_0490u32 as i32));
    // null identifier must not crash
    let r = unsafe { ks.KsProperty(std::ptr::null(), 0, std::ptr::null_mut(), 0, &mut returned) };
    assert!(r.is_err());
    unsafe { source.Shutdown().unwrap() };
}

#[test]
fn advertised_types_follow_the_header_and_activation_stays_cheap() {
    init();
    let path = test_file("advertised.bin");
    let w = Writer::create(&path).unwrap();
    w.set_advertised(&vcam_protocol::advertised_for_output(1920, 1080, 60));
    let (_a, source) = activate_source();
    let pd = unsafe { source.CreatePresentationDescriptor().unwrap() };
    let mut selected = windows_core::BOOL(0);
    let mut desc = None;
    unsafe {
        pd.GetStreamDescriptorByIndex(0, &mut selected, &mut desc)
            .unwrap()
    };
    let handler = unsafe { desc.unwrap().GetMediaTypeHandler().unwrap() };
    let count = unsafe { handler.GetMediaTypeCount().unwrap() };
    assert_eq!(count, 8);
    let fmts: Vec<_> = (0..count)
        .map(|i| {
            FrameFormat::from_media_type(&unsafe { handler.GetMediaTypeByIndex(i).unwrap() })
                .unwrap()
        })
        .collect();
    assert_eq!(
        (fmts[0].pixel, fmts[0].width, fmts[0].height, fmts[0].fps),
        (PixelFormat::Nv12, 1920, 1080, 60),
        "the preferred type is the output size at the output rate"
    );
    assert_eq!((fmts[1].width, fmts[1].fps), (1920, 30));
    assert_eq!(fmts[4].pixel, PixelFormat::Rgb32);
    // activation and CreatePresentationDescriptor map nothing and announce no consumer
    assert_eq!(w.shared().header().consumer_active, 0);
    assert_eq!(w.shared().header().negotiated, Format::default());
    unsafe { source.Shutdown().unwrap() };
    set_test_path(None);
}

#[test]
fn nv12_1080p_live_frames_advance_and_are_paced_at_30fps() {
    init();
    let path = test_file("live1080.bin");
    let feeder = Feeder::start(&path, 1920, 1080, 30);
    let run = Running::start(PixelFormat::Nv12, 1920, 1080);
    assert_eq!(run.fmt.fps, 30);
    let began = Instant::now();
    let mut slots = Vec::new();
    let mut times = Vec::new();
    // skip the first frames: slates until the writer sees the negotiated format
    let mut live = 0;
    while live < 45 {
        let s = run.pull();
        let a = analyse_sample(&s, &run.fmt);
        if a.slot().is_none() && !a.torn() {
            assert!(
                began.elapsed() < Duration::from_secs(5),
                "never got a live frame"
            );
            continue;
        }
        assert!(a.board_ok && !a.black && !a.torn(), "{a:?}");
        slots.push(a.slot().unwrap());
        times.push(sample_time(&s));
        live += 1;
    }
    let n = pattern::slots(1920);
    for w in slots.windows(2) {
        let d = (w[1] + n - w[0]) % n;
        assert!((1..=2).contains(&d), "stripe moved {d} slots: {slots:?}");
    }
    assert!(
        times.windows(2).all(|w| w[1] > w[0]),
        "timestamps must grow"
    );
    let span = (times[44] - times[0]) as f64 / 1e7;
    assert!((1.2..1.9).contains(&span), "45 frames span {span:.3}s");
    let h = feeder.writer.shared().header();
    assert!(h.consumer_active == 1);
    assert_eq!(
        (h.negotiated.width, h.negotiated.height, h.negotiated.fps()),
        (1920, 1080, 30)
    );
    run.stop();
    assert_eq!(
        feeder.writer.shared().header().consumer_active,
        0,
        "cleared when the stream stops"
    );
    set_test_path(None);
}

#[test]
fn every_media_type_streams_live_frames() {
    init();
    for (pixel, w, h) in [
        (PixelFormat::Nv12, 1280, 720),
        (PixelFormat::Rgb32, 1920, 1080),
        (PixelFormat::Rgb32, 1280, 720),
        (PixelFormat::Nv12, 640, 360),
    ] {
        let path = test_file(&format!("types-{w}x{h}-{pixel:?}.bin"));
        let _feeder = Feeder::start(&path, 1920, 1080, 30);
        let run = Running::start(pixel, w, h);
        let began = Instant::now();
        let mut live = 0;
        let mut last = None;
        while live < 8 {
            let s = run.pull();
            let a = analyse_sample(&s, &run.fmt);
            if let Some(slot) = a.slot() {
                assert!(a.board_ok && !a.torn(), "{pixel:?} {w}x{h}: {a:?}");
                if let Some(prev) = last {
                    let n = pattern::slots(w);
                    assert!(
                        (1..=2).contains(&((slot + n - prev) % n)),
                        "{pixel:?} {w}x{h}"
                    );
                }
                last = Some(slot);
                live += 1;
            }
            assert!(
                began.elapsed() < Duration::from_secs(6),
                "{pixel:?} {w}x{h}: no live frames"
            );
        }
        run.stop();
    }
    set_test_path(None);
}

#[test]
fn no_writer_gives_the_not_running_slate_at_30fps() {
    init();
    let _path = test_file("nothing.bin"); // never created
    let run = Running::start(PixelFormat::Nv12, 1280, 720);
    let mut times = Vec::new();
    for _ in 0..12 {
        let s = run.pull();
        let d = slate_diff(&s, &run.fmt, SlateKind::NotRunning);
        assert_eq!(d, 0.0, "frame is not the 'not running' slate");
        times.push(sample_time(&s));
    }
    let span = (times[11] - times[0]) as f64 / 1e7;
    assert!((0.30..0.45).contains(&span), "11 intervals span {span:.3}s");
    run.stop();
    set_test_path(None);
}

#[test]
fn rgb32_slate_matches_the_converted_nv12_slate() {
    init();
    let _path = test_file("rgbslate.bin");
    let run = Running::start(PixelFormat::Rgb32, 1280, 720);
    let s = run.pull();
    assert_eq!(slate_diff(&s, &run.fmt, SlateKind::NotRunning), 0.0);
    run.stop();
    set_test_path(None);
}

#[test]
fn paused_and_starting_states_show_their_slates_and_closing_is_immediate() {
    init();
    let path = test_file("states.bin");
    let w = Writer::create(&path).unwrap();
    w.set_advertised(&vcam_protocol::advertised_for_output(1280, 720, 30));
    w.set_state(WriterState::Paused);
    let run = Running::start(PixelFormat::Nv12, 1280, 720);
    let check = |state: WriterState, kind: SlateKind| {
        w.set_state(state);
        // the stream may deliver one more frame of the old state while the change propagates
        for _ in 0..6 {
            let s = run.pull();
            if slate_diff(&s, &run.fmt, kind) == 0.0 {
                return;
            }
        }
        panic!("{state:?} never showed {kind:?}");
    };
    check(WriterState::Paused, SlateKind::Paused);
    check(WriterState::Starting, SlateKind::Starting);
    check(WriterState::Closed, SlateKind::NotRunning);
    run.stop();
    set_test_path(None);
}

#[test]
fn a_feeder_that_closes_or_dies_gives_the_slate() {
    init();
    let path = test_file("dies.bin");
    let mut feeder = Feeder::start(&path, 1280, 720, 30);
    let run = Running::start(PixelFormat::Nv12, 1280, 720);
    // live first
    let began = Instant::now();
    loop {
        let s = run.pull();
        if analyse_sample(&s, &run.fmt).slot().is_some() {
            break;
        }
        assert!(began.elapsed() < Duration::from_secs(5));
    }
    // the writer stops without closing: after the heartbeat timeout the slate appears; until then frames repeat
    feeder.stop_publishing();
    let t0 = Instant::now();
    let mut saw_repeat = false;
    loop {
        let s = run.pull();
        if slate_diff(&s, &run.fmt, SlateKind::NotRunning) == 0.0 {
            break;
        }
        saw_repeat |= analyse_sample(&s, &run.fmt).slot().is_some();
        assert!(t0.elapsed() < Duration::from_secs(3), "no slate within 3 s");
    }
    let took = t0.elapsed();
    assert!(
        saw_repeat,
        "the last frame is repeated while the writer is only late"
    );
    assert!(
        (Duration::from_millis(800)..Duration::from_millis(1600)).contains(&took),
        "slate after {took:?}"
    );
    assert!(feeder.writer.shared().header().repeated > 0);
    run.stop();
    set_test_path(None);
}

#[test]
fn a_corrupted_header_gives_the_slate_and_recovery_gives_frames_again() {
    init();
    let path = test_file("hostile.bin");
    let mut feeder = Feeder::start(&path, 1280, 720, 30);
    let run = Running::start(PixelFormat::Nv12, 1280, 720);
    let wait_for = |want_live: bool| {
        let began = Instant::now();
        loop {
            let s = run.pull();
            let live = analyse_sample(&s, &run.fmt).slot().is_some();
            let slate = slate_diff(&s, &run.fmt, SlateKind::NotRunning) == 0.0;
            if want_live && live {
                return;
            }
            if !want_live && slate {
                return;
            }
            assert!(
                began.elapsed() < Duration::from_secs(5),
                "want_live={want_live}: stuck"
            );
        }
    };
    wait_for(true);
    let writer = feeder.writer.clone();
    let sh = writer.shared();
    for (offset, bad, good) in [
        (off::MAGIC, 0xBAD0_BAD0u64, vcam_protocol::MAGIC as u64),
        (off::PROTOCOL, 7, 1),
        (off::HEADER_SIZE, 17, 4096),
        (off::SLOT_COUNT, 99, 3),
    ] {
        sh.poke32(offset, bad as u32);
        wait_for(false);
        sh.poke32(offset, good as u32);
        wait_for(true);
    }
    for (offset, bad, good) in [
        (off::FILE_SIZE, u64::MAX, vcam_protocol::FILE_SIZE),
        (off::SLOT_CAPACITY, u64::MAX, vcam_protocol::SLOT_CAPACITY),
        (off::QPC_FREQUENCY, 3, vcam_protocol::qpc_frequency()),
    ] {
        sh.poke64(offset, bad);
        wait_for(false);
        sh.poke64(offset, good);
        wait_for(true);
    }
    // writer_state garbage
    sh.poke32(off::WRITER_STATE, 77);
    wait_for(false);
    sh.poke32(off::WRITER_STATE, WriterState::Live as u32);
    wait_for(true);
    // a heartbeat from the far future (the feeder rewrites the heartbeat with every frame, so stop it first)
    feeder.stop_publishing();
    sh.poke64(off::WRITER_HEARTBEAT, u64::MAX - 5);
    wait_for(false);
    run.stop();
    set_test_path(None);
}

#[test]
fn hostile_slot_descriptors_give_the_slate() {
    init();
    let path = test_file("slots.bin");
    let mut feeder = Feeder::start(&path, 1280, 720, 30);
    let run = Running::start(PixelFormat::Nv12, 1280, 720);
    let began = Instant::now();
    while analyse_sample(&run.pull(), &run.fmt).slot().is_none() {
        assert!(began.elapsed() < Duration::from_secs(5));
    }
    // freeze the writer, then damage every slot descriptor and keep the heartbeat alive
    feeder.stop_publishing();
    let sh = feeder.writer.shared();
    for i in 0..3 {
        let so = vcam_protocol::slot_off(i);
        sh.poke64(so + off::SLOT_DATA_OFFSET, vcam_protocol::FILE_SIZE - 8);
        sh.poke32(so + off::SLOT_WIDTH, 1281);
    }
    let t0 = Instant::now();
    loop {
        feeder.writer.heartbeat();
        let s = run.pull();
        if slate_diff(&s, &run.fmt, SlateKind::NotRunning) == 0.0 {
            break;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(3),
            "hostile slots did not give the slate"
        );
    }
    run.stop();
    set_test_path(None);
}

#[test]
fn restart_after_stop_reports_updated_stream() {
    init();
    let _path = test_file("restart.bin");
    let (_a, source) = activate_source();
    let gen: IMFMediaEventGenerator = source.cast().unwrap();
    for round in 0..2 {
        let pd = unsafe { source.CreatePresentationDescriptor().unwrap() };
        unsafe {
            source
                .Start(&pd, std::ptr::null(), &PROPVARIANT::default())
                .unwrap()
        };
        let mut kinds = Vec::new();
        for _ in 0..2 {
            kinds.push(event_type(&next_event(&gen, Duration::from_secs(5))));
        }
        let expect_new = if round == 0 {
            MENewStream.0 as u32
        } else {
            windows::Win32::Media::MediaFoundation::MEUpdatedStream.0 as u32
        };
        assert!(kinds.contains(&expect_new), "round {round}: {kinds:?}");
        unsafe { source.Stop().unwrap() };
        assert_eq!(
            event_type(&next_event(&gen, Duration::from_secs(5))),
            MESourceStopped.0 as u32
        );
    }
    unsafe { source.Shutdown().unwrap() };
    set_test_path(None);
}

#[test]
fn a_restart_with_another_format_gives_whole_samples_of_the_new_format_from_a_new_pool() {
    init();
    let path = test_file("reformat.bin");
    let feeder = Feeder::start(&path, 1920, 1080, 30);
    let (_a, source) = activate_source();
    let src_gen: IMFMediaEventGenerator = source.cast().unwrap();
    let mut stream: Option<IMFMediaStream2> = None;
    // 720p NV12, small NV12, RGB32, and back to the first: every start is a new pump with a new pool
    for (round, (pixel, w, h)) in [
        (PixelFormat::Nv12, 1280u32, 720u32),
        (PixelFormat::Nv12, 640, 360),
        (PixelFormat::Rgb32, 1280, 720),
        (PixelFormat::Nv12, 1280, 720),
    ]
    .into_iter()
    .enumerate()
    {
        let pd = unsafe { source.CreatePresentationDescriptor().unwrap() };
        let mut selected = windows_core::BOOL(0);
        let mut desc = None;
        unsafe {
            pd.GetStreamDescriptorByIndex(0, &mut selected, &mut desc)
                .unwrap()
        };
        let handler = unsafe { desc.unwrap().GetMediaTypeHandler().unwrap() };
        let count = unsafe { handler.GetMediaTypeCount().unwrap() };
        let (mt, fmt) = (0..count)
            .map(|i| unsafe { handler.GetMediaTypeByIndex(i).unwrap() })
            .map(|t| {
                let f = FrameFormat::from_media_type(&t).unwrap();
                (t, f)
            })
            .find(|(_, f)| (f.pixel, f.width, f.height) == (pixel, w, h))
            .unwrap_or_else(|| panic!("{pixel:?} {w}x{h} not advertised"));
        unsafe { handler.SetCurrentMediaType(&mt).unwrap() };
        unsafe {
            source
                .Start(&pd, std::ptr::null(), &PROPVARIANT::default())
                .unwrap()
        };
        for _ in 0..2 {
            let e = next_event(&src_gen, Duration::from_secs(5));
            if event_type(&e) == MENewStream.0 as u32
                || event_type(&e)
                    == windows::Win32::Media::MediaFoundation::MEUpdatedStream.0 as u32
            {
                stream = Some(event_unknown(&e).cast().unwrap());
            }
        }
        let stream = stream.clone().expect("the stream");
        let sgen: IMFMediaEventGenerator = stream.cast().unwrap();
        assert_eq!(
            event_type(&next_event(&sgen, Duration::from_secs(5))),
            MEStreamStarted.0 as u32,
            "round {round}"
        );
        let began = Instant::now();
        let mut live = 0;
        while live < 5 {
            unsafe { stream.RequestSample(None).unwrap() };
            let e = next_event(&sgen, Duration::from_secs(5));
            assert_eq!(event_type(&e), MEMediaSample.0 as u32);
            let s: IMFSample = event_unknown(&e).cast().unwrap();
            let a = analyse_sample(&s, &fmt);
            if a.slot().is_some() {
                assert!(
                    a.board_ok && !a.torn(),
                    "round {round} {pixel:?} {w}x{h}: {a:?}"
                );
                live += 1;
            }
            assert!(
                began.elapsed() < Duration::from_secs(6),
                "round {round} {pixel:?} {w}x{h}: no live frames"
            );
        }
        unsafe { source.Stop().unwrap() };
        assert_eq!(
            event_type(&next_event(&src_gen, Duration::from_secs(5))),
            MESourceStopped.0 as u32
        );
        assert_eq!(
            event_type(&next_event(&sgen, Duration::from_secs(5))),
            MEStreamStopped.0 as u32
        );
    }
    assert_eq!(feeder.writer.shared().header().consumer_active, 0);
    unsafe { source.Shutdown().unwrap() };
    set_test_path(None);
}
#[test]
fn sample_timestamps_share_the_mf_system_time_base() {
    // Samples are stamped from QPC (the writer's capture time); consumers compare them with MF's clock.
    use windows::Win32::Media::MediaFoundation::MFGetSystemTime;
    let mf = unsafe { MFGetSystemTime() };
    let q = vcam_protocol::qpc_to_hns(qpc_now());
    let diff_ms = (mf - q).abs() as f64 / 10_000.0;
    assert!(
        diff_ms < 20.0,
        "MFGetSystemTime and QPC differ by {diff_ms} ms"
    );
}

mod wake_rig;
