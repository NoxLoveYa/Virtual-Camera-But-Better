//! Direct3D 11 device creation (plan 4.2) and the Media Foundation DXGI device manager bound to it.
//!
//! This phase has no shaders, conversion or texture pool: only the device that decoders and the filter chain will
//! share.

use windows::core::{Interface, Result};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL,
    D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_FLAG, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
    D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::{IDXGIAdapter, IDXGIAdapter1, IDXGIDevice};
use windows::Win32::Media::MediaFoundation::{IMFDXGIDeviceManager, MFCreateDXGIDeviceManager};

/// Which adapter to create the device on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdapterKind {
    /// The default hardware adapter.
    Hardware,
    /// The Microsoft software rasterizer. For tests and machines without a usable GPU.
    Warp,
}

/// One D3D11 device, its immediate context, and the `IMFDXGIDeviceManager` that hands the device to Media
/// Foundation decoders.
///
/// Threading: the device is free-threaded. The immediate context is not, but multithread protection is switched on
/// at creation, so Media Foundation threads and the render thread may both use it. Callers still should keep their
/// own context use on one thread.
pub struct GpuDevice {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    /// Pass this to the Source Reader (`MF_SOURCE_READER_D3D_MANAGER`).
    pub manager: IMFDXGIDeviceManager,
    pub adapter_name: String,
    pub vendor_id: u32,
    pub feature_level: D3D_FEATURE_LEVEL,
    pub kind: AdapterKind,
    /// The device was created with `D3D11_CREATE_DEVICE_VIDEO_SUPPORT` (always true for `Hardware`; WARP may
    /// lack it, in which case decoders cannot use this device).
    pub video_support: bool,
}

// SAFETY: ID3D11Device and IMFDXGIDeviceManager are thread-safe objects. The immediate context is only reachable
// through this struct and multithread protection is enabled in `new`.
unsafe impl Send for GpuDevice {}
unsafe impl Sync for GpuDevice {}

impl GpuDevice {
    /// Create the device with `D3D11_CREATE_DEVICE_VIDEO_SUPPORT | BGRA_SUPPORT`, feature level 11_0 or higher,
    /// multithread protection on, and bind it to a new DXGI device manager.
    pub fn new(kind: AdapterKind) -> Result<GpuDevice> {
        let driver: D3D_DRIVER_TYPE = match kind {
            AdapterKind::Hardware => D3D_DRIVER_TYPE_HARDWARE,
            AdapterKind::Warp => D3D_DRIVER_TYPE_WARP,
        };
        let video = D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT;
        let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];

        let mut device = None;
        let mut context = None;
        let mut feature_level = D3D_FEATURE_LEVEL::default();
        let mut create = |flags: D3D11_CREATE_DEVICE_FLAG| {
            // SAFETY: out pointers reference locals that live through the call.
            unsafe {
                D3D11CreateDevice(
                    None::<&IDXGIAdapter>,
                    driver,
                    HMODULE::default(),
                    flags,
                    Some(&levels),
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    Some(&mut feature_level),
                    Some(&mut context),
                )
            }
        };
        let mut video_support = true;
        if let Err(e) = create(video) {
            // The software rasterizer has no video device; it only exists for tests, so run it without.
            if kind != AdapterKind::Warp {
                return Err(e);
            }
            video_support = false;
            create(D3D11_CREATE_DEVICE_BGRA_SUPPORT)?;
        }
        let device = device
            .ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_POINTER))?;
        let context = context
            .ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_POINTER))?;

        // SAFETY: plain COM calls on objects created above.
        unsafe {
            let _previous = device
                .cast::<ID3D11Multithread>()?
                .SetMultithreadProtected(true);

            let mut token = 0u32;
            let mut manager = None;
            MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
            let manager = manager
                .ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_POINTER))?;
            manager.ResetDevice(&device, token)?;

            let (adapter_name, vendor_id) = adapter_info(&device).unwrap_or_default();
            Ok(GpuDevice {
                device,
                context,
                manager,
                adapter_name,
                vendor_id,
                feature_level,
                kind,
                video_support,
            })
        }
    }
}

/// Description string and PCI vendor id of the adapter a device runs on.
///
/// # Safety
/// Plain COM calls; `device` must be a valid device.
unsafe fn adapter_info(device: &ID3D11Device) -> Result<(String, u32)> {
    let adapter: IDXGIAdapter1 = device.cast::<IDXGIDevice>()?.GetAdapter()?.cast()?;
    let desc = adapter.GetDesc1()?;
    let len = desc
        .Description
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(desc.Description.len());
    Ok((
        String::from_utf16_lossy(&desc.Description[..len]),
        desc.VendorId,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warp_device_and_manager() {
        let gpu = GpuDevice::new(AdapterKind::Warp).expect("WARP device");
        assert_eq!(gpu.kind, AdapterKind::Warp);
        assert!(gpu.feature_level.0 >= D3D_FEATURE_LEVEL_11_0.0);
        assert_eq!(
            gpu.vendor_id, 0x1414,
            "WARP reports the Microsoft vendor id"
        );
        // The manager hands the same device back to Media Foundation.
        unsafe {
            let handle = gpu.manager.OpenDeviceHandle().expect("OpenDeviceHandle");
            let mut p = std::ptr::null_mut();
            gpu.manager
                .LockDevice(handle, &ID3D11Device::IID, &mut p, true)
                .expect("LockDevice");
            let locked = ID3D11Device::from_raw(p);
            assert_eq!(locked.as_raw(), gpu.device.as_raw());
            gpu.manager
                .UnlockDevice(handle, false)
                .expect("UnlockDevice");
            gpu.manager
                .CloseDeviceHandle(handle)
                .expect("CloseDeviceHandle");
        }
        // Multithread protection stays on.
        let mt = gpu.device.cast::<ID3D11Multithread>().unwrap();
        assert!(unsafe { mt.GetMultithreadProtected() }.as_bool());
    }

    #[test]
    fn hardware_device_when_a_gpu_exists() {
        match GpuDevice::new(AdapterKind::Hardware) {
            Ok(gpu) => {
                assert!(!gpu.adapter_name.is_empty());
                assert!(gpu.video_support);
            }
            Err(e) => eprintln!("no hardware adapter in this environment: {e}"),
        }
    }
}
