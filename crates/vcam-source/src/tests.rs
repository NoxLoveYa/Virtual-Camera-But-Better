//! In-process tests: load the class factory through `DllGetClassObject` and drive Activator -> MediaSource ->
//! MediaStream -> samples exactly as Frame Server would, without Frame Server.

use std::time::{Duration, Instant};

use vcam_protocol::pattern::{box_x, box_y, slot_from_box_x, slots, BOX_SIZE, DETECT_LUMA};
use windows::Win32::Foundation::{CLASS_E_CLASSNOTAVAILABLE, E_UNEXPECTED, S_FALSE};
use windows::Win32::Media::KernelStreaming::{IKsControl, KSIDENTIFIER, PINNAME_VIDEO_CAPTURE};
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer, IMFActivate, IMFAttributes, IMFGetService, IMFMediaEvent, IMFMediaEventGenerator,
    IMFMediaSource, IMFMediaSource2, IMFMediaSourceEx, IMFMediaStream2, IMFPresentationDescriptor,
    IMFSample, IMFSampleAllocatorControl, MEMediaSample, MENewStream, MESourceStarted,
    MESourceStopped, MEStreamStarted, MEStreamStopped, MFFrameSourceTypes_Color,
    MFSampleAllocatorUsage_UsesCustomAllocator, MFStartup, MFVideoFormat_NV12, MFVideoFormat_RGB32,
    MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS, MFSTARTUP_FULL,
    MF_DEVICESTREAM_ATTRIBUTE_FRAMESOURCE_TYPES, MF_DEVICESTREAM_FRAMESERVER_SHARED,
    MF_DEVICESTREAM_STREAM_CATEGORY, MF_DEVICESTREAM_STREAM_ID, MF_EVENT_FLAG_NO_WAIT,
    MF_E_INVALID_STATE_TRANSITION, MF_E_UNSUPPORTED_SERVICE, MF_MT_SUBTYPE, MF_VERSION,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{CoInitializeEx, IClassFactory, COINIT_MULTITHREADED};
use windows_core::{Interface, GUID, HRESULT};

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

/// Frame number (mod slots) and the bright run found in an NV12 or RGB32 sample.
fn decode_sample(sample: &IMFSample, fmt: &FrameFormat) -> (u32, Vec<u8>) {
    let buffer = unsafe { sample.ConvertToContiguousBuffer().unwrap() };
    let b2: IMF2DBuffer = windows_core::Interface::cast(&buffer).unwrap();
    let mut scan0 = std::ptr::null_mut();
    let mut pitch = 0i32;
    unsafe { b2.Lock2D(&mut scan0, &mut pitch).unwrap() };
    assert!(pitch > 0);
    let row = box_y(fmt.height) as usize + BOX_SIZE as usize / 2;
    let line_bytes = match fmt.pixel {
        PixelFormat::Nv12 => fmt.width as usize,
        PixelFormat::Rgb32 => fmt.width as usize * 4,
    };
    let line =
        unsafe { std::slice::from_raw_parts(scan0.add(row * pitch as usize), line_bytes) }.to_vec();
    // first pixel row of the frame: used to check the bars move / are not black
    let first = unsafe { std::slice::from_raw_parts(scan0, line_bytes) }.to_vec();
    unsafe { b2.Unlock2D().unwrap() };
    let luma = |x: usize| match fmt.pixel {
        PixelFormat::Nv12 => line[x],
        PixelFormat::Rgb32 => line[x * 4].min(line[x * 4 + 1]).min(line[x * 4 + 2]),
    };
    let mut run = 0;
    let mut found = None;
    for x in 0..fmt.width as usize {
        if luma(x) >= DETECT_LUMA {
            run += 1;
            if run == BOX_SIZE / 2 {
                found = Some((x + 1) as u32 - run);
                break;
            }
        } else {
            run = 0;
        }
    }
    (slot_from_box_x(found.expect("white box not found")), first)
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

/// Start the source with media type `index` and pull `count` frames; returns (frame slots, timestamps).
fn stream_frames(index: u32, count: usize) -> (FrameFormat, Vec<u32>, Vec<i64>, Duration) {
    let (_activate, source) = activate_source();
    let pd: IMFPresentationDescriptor = unsafe { source.CreatePresentationDescriptor().unwrap() };
    let mut selected = windows_core::BOOL(0);
    let mut desc = None;
    unsafe {
        pd.GetStreamDescriptorByIndex(0, &mut selected, &mut desc)
            .unwrap()
    };
    let handler = unsafe { desc.unwrap().GetMediaTypeHandler().unwrap() };
    let mt = unsafe { handler.GetMediaTypeByIndex(index).unwrap() };
    unsafe { handler.SetCurrentMediaType(&mt).unwrap() };
    let fmt = FrameFormat::from_media_type(&mt).unwrap();

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

    let mut slots_seen = Vec::new();
    let mut times = Vec::new();
    let began = Instant::now();
    for _ in 0..count {
        unsafe { stream.RequestSample(None).unwrap() };
        let e = next_event(&sgen, Duration::from_secs(5));
        assert_eq!(event_type(&e), MEMediaSample.0 as u32);
        let sample: IMFSample = event_unknown(&e).cast().unwrap();
        let (slot, first_row) = decode_sample(&sample, &fmt);
        assert!(first_row.iter().any(|&b| b != 0), "frame is black");
        slots_seen.push(slot);
        times.push(unsafe { sample.GetSampleTime().unwrap() });
        assert_eq!(unsafe { sample.GetSampleDuration().unwrap() }, 333_333);
    }
    let elapsed = began.elapsed();

    unsafe { source.Stop().unwrap() };
    assert_eq!(
        event_type(&next_event(&src_gen, Duration::from_secs(5))),
        MESourceStopped.0 as u32
    );
    assert_eq!(
        event_type(&next_event(&sgen, Duration::from_secs(5))),
        MEStreamStopped.0 as u32
    );
    unsafe { source.Shutdown().unwrap() };
    (fmt, slots_seen, times, elapsed)
}

fn assert_consecutive(fmt: &FrameFormat, seen: &[u32]) {
    let n = slots(fmt.width);
    for w in seen.windows(2) {
        assert_eq!(
            (w[0] + 1) % n,
            w[1],
            "box did not advance by one slot: {seen:?}"
        );
    }
    assert_eq!(
        seen[0],
        box_x(fmt.width, 0) / vcam_protocol::pattern::BOX_STEP
    );
}

#[test]
fn nv12_1080p_frames_advance_and_are_paced_at_30fps() {
    init();
    let (fmt, seen, times, elapsed) = stream_frames(0, 45);
    assert_eq!(
        (fmt.pixel, fmt.width, fmt.height),
        (PixelFormat::Nv12, 1920, 1080)
    );
    assert_consecutive(&fmt, &seen);
    // 44 intervals of 33.33 ms = 1.467 s; allow 15 % either way
    let secs = elapsed.as_secs_f64();
    assert!((1.25..1.70).contains(&secs), "45 frames took {secs:.3}s");
    let span = (times[44] - times[0]) as f64 / 1e7;
    assert!(
        (1.40..1.54).contains(&span),
        "sample timestamps span {span:.3}s"
    );
}

#[test]
fn every_media_type_streams() {
    init();
    for index in 1..4 {
        let (fmt, seen, _t, _e) = stream_frames(index, 8);
        assert_consecutive(&fmt, &seen);
    }
}

#[test]
fn restart_after_stop_reports_updated_stream() {
    init();
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
}
