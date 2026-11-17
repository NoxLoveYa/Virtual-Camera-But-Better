//! `MediaSource`: `IMFMediaSource2` (so also `IMFMediaSourceEx`, `IMFMediaSource`, `IMFMediaEventGenerator`),
//! `IMFGetService`, `IKsControl`, `IMFSampleAllocatorControl`. One live video stream.
//!
//! Structure follows Microsoft's "Frame Server Custom Media Source" guide and VCamSample (MIT, Copyright (c)
//! Simon Mourier): same interface set, sensor profile collection, event order and `ERROR_SET_NOT_FOUND` stubs.

use std::sync::Mutex;

use windows::Win32::Foundation::{E_INVALIDARG, E_POINTER, E_UNEXPECTED, S_OK};
use windows::Win32::Media::KernelStreaming::{
    IKsControl, IKsControl_Impl, KSCAMERAPROFILE_HighFrameRate, KSCAMERAPROFILE_Legacy,
    KSIDENTIFIER,
};
use windows::Win32::Media::MediaFoundation::{
    IMFAsyncCallback, IMFAsyncResult, IMFAttributes, IMFGetService, IMFGetService_Impl,
    IMFMediaEvent, IMFMediaEventGenerator_Impl, IMFMediaEventQueue, IMFMediaSource2,
    IMFMediaSource2_Impl, IMFMediaSourceEx_Impl, IMFMediaSource_Impl, IMFMediaType,
    IMFPresentationDescriptor, IMFSampleAllocatorControl, IMFSampleAllocatorControl_Impl,
    MENewStream, MESourceStarted, MESourceStopped, MEUpdatedStream, MFCreateEventQueue,
    MFCreatePresentationDescriptor, MFCreateSensorProfile, MFCreateSensorProfileCollection,
    MFGetSystemTime, MFSampleAllocatorUsage, MFSampleAllocatorUsage_UsesCustomAllocator,
    MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS, MFMEDIASOURCE_IS_LIVE,
    MF_DEVICEMFT_SENSORPROFILE_COLLECTION, MF_E_INVALIDSTREAMNUMBER, MF_E_INVALID_STATE_TRANSITION,
    MF_E_SHUTDOWN, MF_E_UNSUPPORTED_SERVICE, MF_E_UNSUPPORTED_TIME_FORMAT,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows_core::{
    implement, w, ComObject, Error, IUnknown, Ref, Result, BOOL, GUID, HRESULT, PCWSTR,
};

use crate::guard::{guard, LockExt};
use crate::stream::{log_ks, set_not_found, MediaStream};
use crate::{object_created, object_dropped, vlog};

struct SourceState {
    descriptor: Option<IMFPresentationDescriptor>,
    stream_was_selected: bool,
    shutdown: bool,
}

#[implement(IMFMediaSource2, IMFGetService, IKsControl, IMFSampleAllocatorControl)]
pub struct MediaSource {
    attributes: IMFAttributes,
    queue: IMFMediaEventQueue,
    state: Mutex<SourceState>,
    stream: ComObject<MediaStream>,
}

impl MediaSource {
    /// Create a source around `attributes` (the activator's store, shared so Frame Server sees what it set).
    pub fn create(attributes: IMFAttributes) -> Result<ComObject<MediaSource>> {
        let stream = MediaStream::create(0)?;

        // Camera profiles: the "legacy" profile is mandatory; the high-frame-rate one matches Microsoft's sample.
        unsafe {
            let collection = MFCreateSensorProfileCollection()?;
            let legacy = MFCreateSensorProfile(&KSCAMERAPROFILE_Legacy, 0, PCWSTR::null())?;
            legacy.AddProfileFilter(0, w!("((RES==;FRT<=30,1;SUT==))"))?;
            collection.AddProfile(&legacy)?;
            let hfr = MFCreateSensorProfile(&KSCAMERAPROFILE_HighFrameRate, 0, PCWSTR::null())?;
            hfr.AddProfileFilter(0, w!("((RES==;FRT>=60,1;SUT==))"))?;
            collection.AddProfile(&hfr)?;
            attributes.SetUnknown(&MF_DEVICEMFT_SENSORPROFILE_COLLECTION, &collection)?;
        }

        let descriptor = unsafe {
            let pd = MFCreatePresentationDescriptor(Some(&[Some(stream.descriptor().clone())]))?;
            pd.SelectStream(0)?;
            pd
        };
        let queue = unsafe { MFCreateEventQueue()? };
        object_created();
        let source = ComObject::new(MediaSource {
            attributes,
            queue,
            state: Mutex::new(SourceState {
                descriptor: Some(descriptor),
                stream_was_selected: false,
                shutdown: false,
            }),
            stream: stream.clone(),
        });
        stream.set_source(Some(source.to_interface::<IMFMediaSource2>().into()));
        Ok(source)
    }

    pub fn stream(&self) -> &ComObject<MediaStream> {
        &self.stream
    }

    fn check_shutdown(&self) -> Result<()> {
        if self.state.lock_safe().shutdown {
            Err(Error::from(MF_E_SHUTDOWN))
        } else {
            Ok(())
        }
    }

    fn do_shutdown(&self) -> Result<()> {
        let mut st = self.state.lock_safe();
        if st.shutdown {
            return Err(Error::from(MF_E_SHUTDOWN));
        }
        st.shutdown = true;
        st.descriptor = None;
        drop(st);
        self.stream.shutdown();
        let _ = unsafe { self.queue.Shutdown() };
        vlog!("Source::Shutdown");
        Ok(())
    }
}

impl Drop for MediaSource {
    fn drop(&mut self) {
        object_dropped();
    }
}

impl IMFMediaEventGenerator_Impl for MediaSource_Impl {
    fn GetEvent(&self, dwflags: MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS) -> Result<IMFMediaEvent> {
        // may block: no lock held
        guard("Source::GetEvent", || unsafe {
            self.queue.GetEvent(dwflags.0)
        })
    }

    fn BeginGetEvent(
        &self,
        pcallback: Ref<IMFAsyncCallback>,
        punkstate: Ref<IUnknown>,
    ) -> Result<()> {
        guard("Source::BeginGetEvent", || unsafe {
            self.queue
                .BeginGetEvent(pcallback.as_ref(), punkstate.as_ref())
        })
    }

    fn EndGetEvent(&self, presult: Ref<IMFAsyncResult>) -> Result<IMFMediaEvent> {
        guard("Source::EndGetEvent", || unsafe {
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
        guard("Source::QueueEvent", || unsafe {
            self.queue
                .QueueEventParamVar(met, guidextendedtype, hrstatus, pvvalue)
        })
    }
}

impl IMFMediaSource_Impl for MediaSource_Impl {
    fn GetCharacteristics(&self) -> Result<u32> {
        guard("Source::GetCharacteristics", || {
            self.check_shutdown()?;
            Ok(MFMEDIASOURCE_IS_LIVE.0 as u32)
        })
    }

    fn CreatePresentationDescriptor(&self) -> Result<IMFPresentationDescriptor> {
        guard("Source::CreatePresentationDescriptor", || {
            let st = self.state.lock_safe();
            let pd = st
                .descriptor
                .as_ref()
                .ok_or_else(|| Error::from(MF_E_SHUTDOWN))?;
            let clone = unsafe { pd.Clone()? };
            vlog!("Source::CreatePresentationDescriptor");
            Ok(clone)
        })
    }

    fn Start(
        &self,
        ppresentationdescriptor: Ref<IMFPresentationDescriptor>,
        pguidtimeformat: *const GUID,
        pvarstartposition: *const PROPVARIANT,
    ) -> Result<()> {
        guard("Source::Start", || {
            let pd = ppresentationdescriptor.ok()?;
            if pvarstartposition.is_null() {
                return Err(Error::from(E_INVALIDARG));
            }
            if !pguidtimeformat.is_null() && unsafe { *pguidtimeformat } != GUID::zeroed() {
                return Err(Error::from(MF_E_UNSUPPORTED_TIME_FORMAT));
            }
            // The source lock is held only to read the state: starting the stream can wait for an old pump thread
            // to end, and nothing the service calls in the meantime may block on this lock.
            let (own, was_selected) = {
                let st = self.state.lock_safe();
                if st.shutdown {
                    return Err(Error::from(MF_E_SHUTDOWN));
                }
                let own = st
                    .descriptor
                    .clone()
                    .ok_or_else(|| Error::from(MF_E_SHUTDOWN))?;
                (own, st.stream_was_selected)
            };

            let count = unsafe { pd.GetStreamDescriptorCount()? };
            if count != 1 {
                return Err(Error::from(E_INVALIDARG));
            }
            let mut selected = BOOL(0);
            let mut desc = None;
            unsafe { pd.GetStreamDescriptorByIndex(0, &mut selected, &mut desc)? };
            let desc = desc.ok_or_else(|| Error::from(E_UNEXPECTED))?;
            let id = unsafe { desc.GetStreamIdentifier()? };
            if id != self.stream.id() {
                return Err(Error::from(MF_E_INVALIDSTREAMNUMBER));
            }
            vlog!("Source::Start selected={}", selected.as_bool());

            let start_time = PROPVARIANT::from(unsafe { MFGetSystemTime() });
            unsafe {
                self.queue.QueueEventParamVar(
                    MESourceStarted.0 as u32,
                    &GUID::zeroed(),
                    S_OK,
                    &start_time,
                )?
            };

            if selected.as_bool() {
                let met = if was_selected {
                    MEUpdatedStream
                } else {
                    MENewStream
                };
                unsafe { own.SelectStream(0)? };
                let unk: IUnknown = self.stream.to_interface();
                unsafe {
                    self.queue
                        .QueueEventParamUnk(met.0 as u32, &GUID::zeroed(), S_OK, &unk)?
                };
                let mt: IMFMediaType =
                    unsafe { desc.GetMediaTypeHandler()?.GetCurrentMediaType()? };
                self.stream.start(Some(&mt))?;
            } else {
                unsafe { own.DeselectStream(0)? };
                self.stream.stop()?;
            }
            self.state.lock_safe().stream_was_selected = selected.as_bool();
            Ok(())
        })
    }

    fn Stop(&self) -> Result<()> {
        guard("Source::Stop", || {
            self.check_shutdown()?;
            // Microsoft's sample keeps the stream selected across Stop; a later Start then reports MEUpdatedStream.
            self.stream.stop()?;
            let t = PROPVARIANT::from(unsafe { MFGetSystemTime() });
            unsafe {
                self.queue.QueueEventParamVar(
                    MESourceStopped.0 as u32,
                    &GUID::zeroed(),
                    S_OK,
                    &t,
                )?
            };
            vlog!("Source::Stop");
            Ok(())
        })
    }

    fn Pause(&self) -> Result<()> {
        guard("Source::Pause", || {
            // Frame Server custom sources neither pause nor seek.
            self.check_shutdown()?;
            Err(Error::from(MF_E_INVALID_STATE_TRANSITION))
        })
    }

    fn Shutdown(&self) -> Result<()> {
        guard("Source::Shutdown", || self.do_shutdown())
    }
}

impl IMFMediaSourceEx_Impl for MediaSource_Impl {
    fn GetSourceAttributes(&self) -> Result<IMFAttributes> {
        guard("Source::GetSourceAttributes", || {
            self.check_shutdown()?;
            Ok(self.attributes.clone())
        })
    }

    fn GetStreamAttributes(&self, dwstreamidentifier: u32) -> Result<IMFAttributes> {
        guard("Source::GetStreamAttributes", || {
            self.check_shutdown()?;
            if dwstreamidentifier != self.stream.id() {
                return Err(Error::from(MF_E_INVALIDSTREAMNUMBER));
            }
            Ok(self.stream.to_interface::<IMFAttributes>())
        })
    }

    fn SetD3DManager(&self, pmanager: Ref<IUnknown>) -> Result<()> {
        guard("Source::SetD3DManager", || {
            // v1 always delivers system-memory samples, whatever the consumer offers.
            vlog!(
                "Source::SetD3DManager ignored (manager present: {})",
                pmanager.as_ref().is_some()
            );
            Ok(())
        })
    }
}

impl IMFMediaSource2_Impl for MediaSource_Impl {
    fn SetMediaType(&self, dwstreamid: u32, pmediatype: Ref<IMFMediaType>) -> Result<()> {
        guard("Source::SetMediaType", || {
            self.check_shutdown()?;
            if dwstreamid != self.stream.id() {
                return Err(Error::from(MF_E_INVALIDSTREAMNUMBER));
            }
            let mt = pmediatype.ok()?;
            vlog!(
                "Source::SetMediaType {:?}",
                crate::mediatype::FrameFormat::from_media_type(mt).ok()
            );
            unsafe {
                self.stream
                    .descriptor()
                    .GetMediaTypeHandler()?
                    .SetCurrentMediaType(mt)
            }
        })
    }
}

impl IMFGetService_Impl for MediaSource_Impl {
    fn GetService(
        &self,
        guidservice: *const GUID,
        riid: *const GUID,
        ppvobject: *mut *mut core::ffi::c_void,
    ) -> Result<()> {
        guard("Source::GetService", || {
            if ppvobject.is_null() {
                return Err(Error::from(E_POINTER));
            }
            unsafe { *ppvobject = std::ptr::null_mut() };
            let (svc, iid) = unsafe { (guidservice.as_ref().copied(), riid.as_ref().copied()) };
            vlog!(
                "Source::GetService service {:?} iid {:?} -> MF_E_UNSUPPORTED_SERVICE",
                svc,
                iid
            );
            Err(Error::from(MF_E_UNSUPPORTED_SERVICE))
        })
    }
}

impl IMFSampleAllocatorControl_Impl for MediaSource_Impl {
    fn SetDefaultAllocator(&self, dwoutputstreamid: u32, pallocator: Ref<IUnknown>) -> Result<()> {
        guard("Source::SetDefaultAllocator", || {
            vlog!(
                "Source::SetDefaultAllocator stream {dwoutputstreamid} allocator present: {}",
                pallocator.as_ref().is_some()
            );
            Ok(())
        })
    }

    fn GetAllocatorUsage(
        &self,
        dwoutputstreamid: u32,
        pdwinputstreamid: *mut u32,
        peusage: *mut MFSampleAllocatorUsage,
    ) -> Result<()> {
        guard("Source::GetAllocatorUsage", || {
            if pdwinputstreamid.is_null() || peusage.is_null() {
                return Err(Error::from(E_POINTER));
            }
            if dwoutputstreamid != self.stream.id() {
                return Err(Error::from(MF_E_INVALIDSTREAMNUMBER));
            }
            vlog!("Source::GetAllocatorUsage stream {dwoutputstreamid} -> UsesCustomAllocator");
            unsafe {
                *pdwinputstreamid = dwoutputstreamid;
                // the source makes its own samples with MFCreateSample + MFCreate2DMediaBuffer
                *peusage = MFSampleAllocatorUsage_UsesCustomAllocator;
            }
            Ok(())
        })
    }
}

impl IKsControl_Impl for MediaSource_Impl {
    fn KsProperty(
        &self,
        property: *const KSIDENTIFIER,
        propertylength: u32,
        _propertydata: *mut core::ffi::c_void,
        _datalength: u32,
        _bytesreturned: *mut u32,
    ) -> Result<()> {
        guard("Source::KsProperty", || {
            log_ks("Source::KsProperty", property, propertylength);
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
        guard("Source::KsMethod", || {
            log_ks("Source::KsMethod", method, methodlength);
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
        guard("Source::KsEvent", || {
            log_ks("Source::KsEvent", event, eventlength);
            Err(set_not_found())
        })
    }
}

// Interface types used only through `cast` / `to_interface`.
#[allow(unused)]
fn _assert_interfaces(s: &ComObject<MediaSource>) {
    let _: IKsControl = s.to_interface();
    let _: IMFGetService = s.to_interface();
    let _: IMFSampleAllocatorControl = s.to_interface();
    let _: IMFMediaSource2 = s.to_interface();
}
