//! Direct3D 11 core (plan 4.2).
//!
//! * [`GpuDevice`]: the device, its immediate context and the Media Foundation DXGI device manager bound to it.
//! * [`ShaderCache`]: HLSL embedded with `include_str!`, compiled at runtime with `D3DCompile` (`ps_5_0`), the bytecode
//!   and the shader objects cached in memory.
//! * [`TexturePool`] / [`RgbaTexture`]: pooled `R8G8B8A8_UNORM` render targets keyed by size (the working format).
//! * [`Renderer`]: the device plus those caches and a fullscreen-triangle draw helper. Every pass of the pipeline
//!   (convert, filters, output, encode) is one [`Renderer::draw`].
//! * [`FilterChain`] / [`FilterPass`]: the (empty) filter stage between convert and output.
//! * [`GpuTimer`]: GPU time of the frame via timestamp queries.
//!
//! The pipeline modules are [`convert`] (input to RGBA), [`output`] (fit and encode) and [`readback`] (staging ring).
//! Threading: all draw calls belong on one render thread; the shader cache and the texture pool are thread safe.

pub mod convert;
pub mod output;
pub mod pattern;
#[cfg(test)]
mod pipeline_tests;
pub mod readback;

use std::collections::HashMap;
use std::ffi::CString;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};

use windows::core::{Interface, PCSTR};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::Fxc::{
    D3DCompile, D3DCOMPILE_ENABLE_STRICTNESS, D3DCOMPILE_OPTIMIZATION_LEVEL3,
};
use windows::Win32::Graphics::Direct3D::{
    ID3DBlob, ID3DInclude, D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP,
    D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
    D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Buffer, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread,
    ID3D11PixelShader, ID3D11Query, ID3D11RenderTargetView, ID3D11SamplerState,
    ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11VertexShader, D3D11_BIND_CONSTANT_BUFFER,
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_BUFFER_DESC,
    D3D11_COMPARISON_NEVER, D3D11_CPU_ACCESS_WRITE, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_FLAG, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_FILTER_MIN_MAG_MIP_LINEAR,
    D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_WRITE_DISCARD, D3D11_QUERY_DATA_TIMESTAMP_DISJOINT,
    D3D11_QUERY_DESC, D3D11_QUERY_TIMESTAMP, D3D11_QUERY_TIMESTAMP_DISJOINT, D3D11_SAMPLER_DESC,
    D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_USAGE_DEFAULT,
    D3D11_USAGE_DYNAMIC, D3D11_VIEWPORT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{IDXGIAdapter, IDXGIAdapter1, IDXGIDevice};
use windows::Win32::Media::MediaFoundation::{IMFDXGIDeviceManager, MFCreateDXGIDeviceManager};

use windows::core::Result;

// ---------------------------------------------------------------------------------------------------------------
// Errors

/// Everything the GPU layer can report.
#[derive(Debug, Clone)]
pub enum GpuError {
    /// A Direct3D call failed.
    Api(windows::core::Error),
    /// A shader did not compile; `log` is the compiler output.
    Shader { name: String, log: String },
    /// The input is something the pipeline cannot convert (format, layout).
    Unsupported(String),
    /// The caller passed something inconsistent (sizes, buffer lengths).
    Invalid(String),
}

pub type GpuResult<T> = std::result::Result<T, GpuError>;

impl From<windows::core::Error> for GpuError {
    fn from(e: windows::core::Error) -> Self {
        GpuError::Api(e)
    }
}

impl fmt::Display for GpuError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GpuError::Api(e) => write!(f, "{} (0x{:08X})", e.message(), e.code().0 as u32),
            GpuError::Shader { name, log } => write!(f, "shader {name} failed to compile: {log}"),
            GpuError::Unsupported(s) => write!(f, "unsupported: {s}"),
            GpuError::Invalid(s) => write!(f, "invalid: {s}"),
        }
    }
}

impl std::error::Error for GpuError {}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

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

// ---------------------------------------------------------------------------------------------------------------
// Device helpers

impl GpuDevice {
    /// `CreateTexture2D` without initial data.
    pub fn create_texture(&self, desc: &D3D11_TEXTURE2D_DESC) -> GpuResult<ID3D11Texture2D> {
        let mut t = None;
        // SAFETY: `desc` and the out pointer live through the call.
        unsafe { self.device.CreateTexture2D(desc, None, Some(&mut t))? };
        t.ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_POINTER).into())
    }

    /// A shader resource view of `texture`; `format` overrides the view format (plane views of NV12, ...).
    pub fn create_srv(
        &self,
        texture: &ID3D11Texture2D,
        format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
    ) -> GpuResult<ID3D11ShaderResourceView> {
        use windows::Win32::Graphics::Direct3D::D3D_SRV_DIMENSION_TEXTURE2D;
        use windows::Win32::Graphics::Direct3D11::{
            D3D11_SHADER_RESOURCE_VIEW_DESC, D3D11_SHADER_RESOURCE_VIEW_DESC_0, D3D11_TEX2D_SRV,
        };
        let desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
            Format: format,
            ViewDimension: D3D_SRV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_SRV {
                    MostDetailedMip: 0,
                    MipLevels: 1,
                },
            },
        };
        let mut v = None;
        // SAFETY: plain call; `desc` and the out pointer are locals.
        unsafe {
            self.device
                .CreateShaderResourceView(texture, Some(&desc), Some(&mut v))?
        };
        v.ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_POINTER).into())
    }

    /// A render target view of the whole texture in its own format.
    pub fn create_rtv(&self, texture: &ID3D11Texture2D) -> GpuResult<ID3D11RenderTargetView> {
        let mut v = None;
        // SAFETY: plain call; the out pointer is a local.
        unsafe {
            self.device
                .CreateRenderTargetView(texture, None, Some(&mut v))?
        };
        v.ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_POINTER).into())
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Shaders

/// Every shader of the pipeline. The sources are embedded; [`ShaderId::source`] adds the shared header where needed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ShaderId {
    FullscreenVs,
    ConvertNv12,
    ConvertP010,
    ConvertYuy2,
    ConvertI420,
    ConvertRgb,
    EncodeNv12,
    EncodeBgra,
    OutputFit,
}

impl ShaderId {
    pub const ALL: [ShaderId; 9] = [
        ShaderId::FullscreenVs,
        ShaderId::ConvertNv12,
        ShaderId::ConvertP010,
        ShaderId::ConvertYuy2,
        ShaderId::ConvertI420,
        ShaderId::ConvertRgb,
        ShaderId::EncodeNv12,
        ShaderId::EncodeBgra,
        ShaderId::OutputFit,
    ];

    pub fn name(self) -> &'static str {
        match self {
            ShaderId::FullscreenVs => "fullscreen_vs",
            ShaderId::ConvertNv12 => "convert_nv12",
            ShaderId::ConvertP010 => "convert_p010",
            ShaderId::ConvertYuy2 => "convert_yuy2",
            ShaderId::ConvertI420 => "convert_i420",
            ShaderId::ConvertRgb => "convert_rgb",
            ShaderId::EncodeNv12 => "encode_nv12",
            ShaderId::EncodeBgra => "encode_bgra",
            ShaderId::OutputFit => "output_fit",
        }
    }

    /// The HLSL text, with the shared convert header in front of the convert shaders.
    pub fn source(self) -> String {
        const COMMON: &str = include_str!("shaders/convert_common.hlsl");
        let body = match self {
            ShaderId::FullscreenVs => include_str!("shaders/fullscreen_vs.hlsl"),
            ShaderId::ConvertNv12 => include_str!("shaders/convert_nv12.hlsl"),
            ShaderId::ConvertP010 => include_str!("shaders/convert_p010.hlsl"),
            ShaderId::ConvertYuy2 => include_str!("shaders/convert_yuy2.hlsl"),
            ShaderId::ConvertI420 => include_str!("shaders/convert_i420.hlsl"),
            ShaderId::ConvertRgb => include_str!("shaders/convert_rgb.hlsl"),
            ShaderId::EncodeNv12 => include_str!("shaders/encode_nv12.hlsl"),
            ShaderId::EncodeBgra => include_str!("shaders/encode_bgra.hlsl"),
            ShaderId::OutputFit => include_str!("shaders/output_fit.hlsl"),
        };
        match self {
            ShaderId::ConvertNv12
            | ShaderId::ConvertP010
            | ShaderId::ConvertYuy2
            | ShaderId::ConvertI420
            | ShaderId::ConvertRgb => format!("{COMMON}\n{body}"),
            _ => body.to_string(),
        }
    }
}

/// Compile HLSL to bytecode with `D3DCompile` (`d3dcompiler_47.dll`, part of Windows). `target` is `vs_5_0` or
/// `ps_5_0`. The error carries the compiler's message.
pub fn compile_shader(name: &str, source: &str, entry: &str, target: &str) -> GpuResult<Vec<u8>> {
    let c_name = CString::new(name).unwrap_or_default();
    let c_entry = CString::new(entry).unwrap_or_default();
    let c_target = CString::new(target).unwrap_or_default();
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    // SAFETY: the source slice and the C strings outlive the call; out pointers are locals.
    let r = unsafe {
        D3DCompile(
            source.as_ptr() as *const _,
            source.len(),
            PCSTR(c_name.as_ptr() as *const u8),
            None,
            None::<&ID3DInclude>,
            PCSTR(c_entry.as_ptr() as *const u8),
            PCSTR(c_target.as_ptr() as *const u8),
            D3DCOMPILE_ENABLE_STRICTNESS | D3DCOMPILE_OPTIMIZATION_LEVEL3,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    let blob_bytes = |b: &ID3DBlob| -> Vec<u8> {
        // SAFETY: the blob owns `GetBufferSize()` bytes at `GetBufferPointer()` while it is alive.
        unsafe {
            std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize())
                .to_vec()
        }
    };
    match r {
        Ok(()) => {
            let code = code.ok_or_else(|| {
                GpuError::Api(windows::core::Error::from(
                    windows::Win32::Foundation::E_POINTER,
                ))
            })?;
            Ok(blob_bytes(&code))
        }
        Err(e) => {
            let log = errors
                .map(|b| String::from_utf8_lossy(&blob_bytes(&b)).trim().to_string())
                .unwrap_or_else(|| e.message());
            Err(GpuError::Shader {
                name: name.to_string(),
                log,
            })
        }
    }
}

/// Compiled shaders of one device. Each shader is compiled the first time it is asked for (`ps_5_0` / `vs_5_0`), then
/// the bytecode and the shader object stay in memory. Clone the returned COM pointers freely.
pub struct ShaderCache {
    device: ID3D11Device,
    vertex: Mutex<HashMap<ShaderId, ID3D11VertexShader>>,
    pixel: Mutex<HashMap<(ShaderId, &'static str), ID3D11PixelShader>>,
    bytecode: Mutex<Bytecodes>,
}

/// Compiled bytecode by shader and entry point.
type Bytecodes = HashMap<(ShaderId, &'static str), Arc<[u8]>>;

impl ShaderCache {
    pub fn new(device: ID3D11Device) -> ShaderCache {
        ShaderCache {
            device,
            vertex: Mutex::default(),
            pixel: Mutex::default(),
            bytecode: Mutex::default(),
        }
    }

    /// Bytecode of `id`'s `entry` point (`target` is `vs_5_0` or `ps_5_0`).
    pub fn bytecode(
        &self,
        id: ShaderId,
        entry: &'static str,
        target: &str,
    ) -> GpuResult<Arc<[u8]>> {
        if let Some(b) = lock(&self.bytecode).get(&(id, entry)) {
            return Ok(b.clone());
        }
        let bytes: Arc<[u8]> = compile_shader(id.name(), &id.source(), entry, target)?.into();
        lock(&self.bytecode).insert((id, entry), bytes.clone());
        Ok(bytes)
    }

    /// The vertex shader `id` (entry point `main`).
    pub fn vertex(&self, id: ShaderId) -> GpuResult<ID3D11VertexShader> {
        if let Some(v) = lock(&self.vertex).get(&id) {
            return Ok(v.clone());
        }
        let code = self.bytecode(id, "main", "vs_5_0")?;
        let mut vs = None;
        // SAFETY: plain call with valid bytecode.
        unsafe { self.device.CreateVertexShader(&code, None, Some(&mut vs))? };
        let vs = vs.ok_or_else(|| {
            GpuError::Api(windows::core::Error::from(
                windows::Win32::Foundation::E_POINTER,
            ))
        })?;
        lock(&self.vertex).insert(id, vs.clone());
        Ok(vs)
    }

    /// The pixel shader `id` with the given entry point.
    pub fn pixel(&self, id: ShaderId, entry: &'static str) -> GpuResult<ID3D11PixelShader> {
        if let Some(p) = lock(&self.pixel).get(&(id, entry)) {
            return Ok(p.clone());
        }
        let code = self.bytecode(id, entry, "ps_5_0")?;
        let mut ps = None;
        // SAFETY: plain call with valid bytecode.
        unsafe { self.device.CreatePixelShader(&code, None, Some(&mut ps))? };
        let ps = ps.ok_or_else(|| {
            GpuError::Api(windows::core::Error::from(
                windows::Win32::Foundation::E_POINTER,
            ))
        })?;
        lock(&self.pixel).insert((id, entry), ps.clone());
        Ok(ps)
    }

    /// Compile everything now (startup), so the first frame does not pay for it. Returns the first error.
    pub fn warm_up(&self) -> GpuResult<()> {
        self.vertex(ShaderId::FullscreenVs)?;
        for id in ShaderId::ALL {
            match id {
                ShaderId::FullscreenVs => {}
                ShaderId::EncodeNv12 => {
                    self.pixel(id, "PSLuma")?;
                    self.pixel(id, "PSChroma")?;
                }
                _ => {
                    self.pixel(id, "main")?;
                }
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Textures

/// A pooled working texture: `R8G8B8A8_UNORM`, render target and shader resource.
pub struct RgbaTexture {
    pub texture: ID3D11Texture2D,
    pub rtv: ID3D11RenderTargetView,
    pub srv: ID3D11ShaderResourceView,
    pub width: u32,
    pub height: u32,
}

/// Textures per size that stay in the pool when returned.
const POOL_KEEP: usize = 4;
/// Sizes the pool remembers; beyond this it forgets the oldest size (a resolution change leaves junk behind).
const POOL_SIZES: usize = 6;

/// Free [`RgbaTexture`]s keyed by size. [`TexturePool::acquire`] hands one out; dropping the [`Pooled`] returns it.
/// Filters ping-pong between pooled textures instead of owning their own.
pub struct TexturePool {
    gpu: Arc<GpuDevice>,
    free: Mutex<Vec<FreeList>>,
}

/// The free textures of one size.
type FreeList = ((u32, u32), Vec<RgbaTexture>);

impl TexturePool {
    pub fn new(gpu: Arc<GpuDevice>) -> Arc<TexturePool> {
        Arc::new(TexturePool {
            gpu,
            free: Mutex::new(Vec::new()),
        })
    }

    /// A texture of exactly `width` x `height` (its contents are undefined).
    pub fn acquire(self: &Arc<Self>, width: u32, height: u32) -> GpuResult<Pooled> {
        if width == 0 || height == 0 || width > 16384 || height > 16384 {
            return Err(GpuError::Invalid(format!("texture size {width}x{height}")));
        }
        let reused = {
            let mut free = lock(&self.free);
            free.iter_mut()
                .find(|(k, _)| *k == (width, height))
                .and_then(|(_, v)| v.pop())
        };
        let tex = match reused {
            Some(t) => t,
            None => self.create(width, height)?,
        };
        Ok(Pooled {
            tex: Some(tex),
            pool: self.clone(),
        })
    }

    fn create(&self, width: u32, height: u32) -> GpuResult<RgbaTexture> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let texture = self.gpu.create_texture(&desc)?;
        let rtv = self.gpu.create_rtv(&texture)?;
        let srv = self.gpu.create_srv(&texture, DXGI_FORMAT_R8G8B8A8_UNORM)?;
        Ok(RgbaTexture {
            texture,
            rtv,
            srv,
            width,
            height,
        })
    }

    fn give_back(&self, tex: RgbaTexture) {
        let key = (tex.width, tex.height);
        let mut free = lock(&self.free);
        if let Some(pos) = free.iter().position(|(k, _)| *k == key) {
            if free[pos].1.len() < POOL_KEEP {
                free[pos].1.push(tex);
            }
            return;
        }
        if free.len() >= POOL_SIZES {
            free.remove(0);
        }
        free.push((key, vec![tex]));
    }

    /// Drop every free texture (the output size changed).
    pub fn clear(&self) {
        lock(&self.free).clear();
    }

    /// Free textures held right now (tests, diagnostics).
    pub fn free_count(&self) -> usize {
        lock(&self.free).iter().map(|(_, v)| v.len()).sum()
    }
}

/// A texture on loan from a [`TexturePool`]; derefs to [`RgbaTexture`].
pub struct Pooled {
    tex: Option<RgbaTexture>,
    pool: Arc<TexturePool>,
}

impl std::ops::Deref for Pooled {
    type Target = RgbaTexture;
    fn deref(&self) -> &RgbaTexture {
        self.tex
            .as_ref()
            .expect("a pooled texture is present until drop")
    }
}

impl Drop for Pooled {
    fn drop(&mut self) {
        if let Some(t) = self.tex.take() {
            self.pool.give_back(t);
        }
    }
}

/// A dynamic constant buffer written with `WRITE_DISCARD` before each draw.
pub struct ConstantBuffer {
    pub buffer: ID3D11Buffer,
    size: usize,
}

impl ConstantBuffer {
    /// `size` is the size of the HLSL `cbuffer` and must be a multiple of 16.
    pub fn new(device: &ID3D11Device, size: usize) -> GpuResult<ConstantBuffer> {
        if size == 0 || size % 16 != 0 {
            return Err(GpuError::Invalid(format!(
                "constant buffer size {size} is not a multiple of 16"
            )));
        }
        let desc = D3D11_BUFFER_DESC {
            ByteWidth: size as u32,
            Usage: D3D11_USAGE_DYNAMIC,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
            MiscFlags: 0,
            StructureByteStride: 0,
        };
        let mut b = None;
        // SAFETY: plain call; `desc` and the out pointer are locals.
        unsafe { device.CreateBuffer(&desc, None, Some(&mut b))? };
        let buffer = b.ok_or_else(|| {
            GpuError::Api(windows::core::Error::from(
                windows::Win32::Foundation::E_POINTER,
            ))
        })?;
        Ok(ConstantBuffer { buffer, size })
    }

    /// Upload `value` (a `#[repr(C)]` struct with the layout of the cbuffer, exactly `size` bytes).
    pub fn write<T: Copy>(&self, context: &ID3D11DeviceContext, value: &T) -> GpuResult<()> {
        if std::mem::size_of::<T>() != self.size {
            return Err(GpuError::Invalid(format!(
                "constant buffer holds {} bytes, got {}",
                self.size,
                std::mem::size_of::<T>()
            )));
        }
        let mut m = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: the buffer is dynamic and `size` bytes long; the copy stays inside it.
        unsafe {
            context.Map(&self.buffer, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut m))?;
            std::ptr::copy_nonoverlapping(
                value as *const T as *const u8,
                m.pData as *mut u8,
                self.size,
            );
            context.Unmap(&self.buffer, 0);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Renderer

/// The device plus the shared caches, and the one draw primitive of the pipeline.
pub struct Renderer {
    pub gpu: Arc<GpuDevice>,
    pub shaders: ShaderCache,
    pub pool: Arc<TexturePool>,
    fullscreen: ID3D11VertexShader,
    linear: ID3D11SamplerState,
}

/// One fullscreen pass.
pub struct Pass<'a> {
    pub ps: &'a ID3D11PixelShader,
    /// Bound from slot 0.
    pub srvs: &'a [Option<ID3D11ShaderResourceView>],
    pub constants: Option<&'a ConstantBuffer>,
    pub target: &'a ID3D11RenderTargetView,
    /// Size of the render target (the viewport).
    pub width: u32,
    pub height: u32,
}

impl Renderer {
    pub fn new(gpu: Arc<GpuDevice>) -> GpuResult<Renderer> {
        let shaders = ShaderCache::new(gpu.device.clone());
        let fullscreen = shaders.vertex(ShaderId::FullscreenVs)?;
        let desc = D3D11_SAMPLER_DESC {
            Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
            AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
            ComparisonFunc: D3D11_COMPARISON_NEVER,
            MaxLOD: f32::MAX,
            ..Default::default()
        };
        let mut s = None;
        // SAFETY: plain call; `desc` and the out pointer are locals.
        unsafe { gpu.device.CreateSamplerState(&desc, Some(&mut s))? };
        let linear = s.ok_or_else(|| {
            GpuError::Api(windows::core::Error::from(
                windows::Win32::Foundation::E_POINTER,
            ))
        })?;
        let pool = TexturePool::new(gpu.clone());
        Ok(Renderer {
            gpu,
            shaders,
            pool,
            fullscreen,
            linear,
        })
    }

    /// A pooled RGBA texture of `width` x `height`.
    pub fn texture(&self, width: u32, height: u32) -> GpuResult<Pooled> {
        self.pool.acquire(width, height)
    }

    pub fn constants(&self, size: usize) -> GpuResult<ConstantBuffer> {
        ConstantBuffer::new(&self.gpu.device, size)
    }

    /// Draw one fullscreen triangle with `pass.ps` into `pass.target`. Inputs are unbound again afterwards so the
    /// next pass can render into a texture this one sampled.
    pub fn draw(&self, pass: &Pass<'_>) {
        let ctx = &self.gpu.context;
        let viewport = D3D11_VIEWPORT {
            TopLeftX: 0.0,
            TopLeftY: 0.0,
            Width: pass.width as f32,
            Height: pass.height as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        };
        let none: [Option<ID3D11ShaderResourceView>; 4] = [None, None, None, None];
        // SAFETY: plain state-setting calls on the render thread's context; every slice and COM pointer outlives
        // the calls.
        unsafe {
            // The render target goes first: the previous pass's target may be this pass's input, and a texture that
            // is still bound as a render target silently reads as black when bound as a shader resource.
            ctx.OMSetRenderTargets(Some(&[Some(pass.target.clone())]), None);
            ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            ctx.IASetInputLayout(None);
            ctx.VSSetShader(&self.fullscreen, None);
            ctx.PSSetShader(pass.ps, None);
            ctx.PSSetShaderResources(0, Some(pass.srvs));
            ctx.PSSetSamplers(0, Some(&[Some(self.linear.clone())]));
            if let Some(c) = pass.constants {
                ctx.PSSetConstantBuffers(0, Some(&[Some(c.buffer.clone())]));
            }
            ctx.RSSetViewports(Some(&[viewport]));
            ctx.Draw(3, 0);
            ctx.PSSetShaderResources(0, Some(&none[..pass.srvs.len().min(none.len())]));
        }
    }

    /// Clear a render target.
    pub fn clear(&self, target: &ID3D11RenderTargetView, rgba: [f32; 4]) {
        // SAFETY: plain call.
        unsafe { self.gpu.context.ClearRenderTargetView(target, &rgba) };
    }

    /// Submit what was recorded so far to the GPU.
    pub fn flush(&self) {
        // SAFETY: plain call.
        unsafe { self.gpu.context.Flush() };
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Filter chain

/// One filter. A pass reads `input` and writes `output` (both pooled RGBA textures; `output` has the size
/// [`FilterPass::output_size`] returned). Passes run in order on the render thread.
pub trait FilterPass: Send {
    fn name(&self) -> &str;

    /// Size of the result for an input of this size. Most filters keep it.
    fn output_size(&self, input: (u32, u32)) -> (u32, u32) {
        input
    }

    fn apply(&mut self, r: &Renderer, input: &RgbaTexture, output: &RgbaTexture) -> GpuResult<()>;
}

/// The filter stage between input convert and output. Empty means "identity": [`FilterChain::run`] hands the input
/// back untouched. Each pass ping-pongs through the texture pool.
#[derive(Default)]
pub struct FilterChain {
    passes: Vec<Box<dyn FilterPass>>,
}

impl FilterChain {
    pub fn new() -> FilterChain {
        FilterChain::default()
    }

    pub fn push(&mut self, pass: Box<dyn FilterPass>) {
        self.passes.push(pass);
    }

    pub fn clear(&mut self) {
        self.passes.clear();
    }

    pub fn len(&self) -> usize {
        self.passes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.passes.is_empty()
    }

    pub fn run(&mut self, r: &Renderer, mut image: Pooled) -> GpuResult<Pooled> {
        for pass in &mut self.passes {
            let (w, h) = pass.output_size((image.width, image.height));
            let out = r.texture(w, h)?;
            pass.apply(r, &image, &out)?;
            // `image` returns to the pool here and becomes the next pass's output.
            image = out;
        }
        Ok(image)
    }
}

// ---------------------------------------------------------------------------------------------------------------
// GPU timing

struct TimerSlot {
    disjoint: ID3D11Query,
    begin: ID3D11Query,
    end: ID3D11Query,
    pending: bool,
}

/// Measures how long the GPU spends on the commands between [`GpuTimer::begin`] and [`GpuTimer::end`] with
/// timestamp queries. Results arrive a frame or two later ([`GpuTimer::poll`]); nothing ever waits.
pub struct GpuTimer {
    slots: Vec<TimerSlot>,
    next: usize,
    /// Most recent finished measurement in milliseconds.
    pub last_ms: Option<f64>,
}

impl GpuTimer {
    pub fn new(device: &ID3D11Device) -> GpuResult<GpuTimer> {
        let make = |q| -> GpuResult<ID3D11Query> {
            let desc = D3D11_QUERY_DESC {
                Query: q,
                MiscFlags: 0,
            };
            let mut out = None;
            // SAFETY: plain call; `desc` and the out pointer are locals.
            unsafe { device.CreateQuery(&desc, Some(&mut out))? };
            out.ok_or_else(|| {
                GpuError::Api(windows::core::Error::from(
                    windows::Win32::Foundation::E_POINTER,
                ))
            })
        };
        let mut slots = Vec::new();
        for _ in 0..4 {
            slots.push(TimerSlot {
                disjoint: make(D3D11_QUERY_TIMESTAMP_DISJOINT)?,
                begin: make(D3D11_QUERY_TIMESTAMP)?,
                end: make(D3D11_QUERY_TIMESTAMP)?,
                pending: false,
            });
        }
        Ok(GpuTimer {
            slots,
            next: 0,
            last_ms: None,
        })
    }

    /// Start a measurement. Skipped (returns false) when every slot still waits for its result.
    pub fn begin(&mut self, ctx: &ID3D11DeviceContext) -> bool {
        let slot = &mut self.slots[self.next];
        if slot.pending {
            return false;
        }
        // SAFETY: plain query calls on the render thread's context.
        unsafe {
            ctx.Begin(&slot.disjoint);
            ctx.End(&slot.begin);
        }
        true
    }

    /// End the measurement started by a successful [`GpuTimer::begin`].
    pub fn end(&mut self, ctx: &ID3D11DeviceContext) {
        let slot = &mut self.slots[self.next];
        // SAFETY: plain query calls on the render thread's context.
        unsafe {
            ctx.End(&slot.end);
            ctx.End(&slot.disjoint);
        }
        slot.pending = true;
        self.next = (self.next + 1) % self.slots.len();
    }

    /// Collect finished measurements without waiting. Returns the newest one found.
    pub fn poll(&mut self, ctx: &ID3D11DeviceContext) -> Option<f64> {
        const DONOTFLUSH: u32 = 1; // D3D11_ASYNC_GETDATA_DONOTFLUSH
        let mut newest = None;
        let n = self.slots.len();
        for i in 0..n {
            let idx = (self.next + i) % n;
            let slot = &mut self.slots[idx];
            if !slot.pending {
                continue;
            }
            let mut d = D3D11_QUERY_DATA_TIMESTAMP_DISJOINT::default();
            // SAFETY: the data pointers reference locals of the exact query data size. S_FALSE (not ready) leaves
            // the zeroed struct untouched, which shows as Frequency == 0.
            let ready = unsafe {
                ctx.GetData(
                    &slot.disjoint,
                    Some(&mut d as *mut _ as *mut core::ffi::c_void),
                    std::mem::size_of::<D3D11_QUERY_DATA_TIMESTAMP_DISJOINT>() as u32,
                    DONOTFLUSH,
                )
            };
            if ready.is_err() || d.Frequency == 0 {
                continue;
            }
            let (mut t0, mut t1) = (0u64, 0u64);
            // SAFETY: as above, 8-byte timestamps.
            let ok = unsafe {
                ctx.GetData(
                    &slot.begin,
                    Some(&mut t0 as *mut _ as *mut core::ffi::c_void),
                    8,
                    DONOTFLUSH,
                )
                .is_ok()
                    && ctx
                        .GetData(
                            &slot.end,
                            Some(&mut t1 as *mut _ as *mut core::ffi::c_void),
                            8,
                            DONOTFLUSH,
                        )
                        .is_ok()
            };
            if !ok || t1 < t0 {
                continue;
            }
            slot.pending = false;
            if !d.Disjoint.as_bool() {
                newest = Some((t1 - t0) as f64 * 1000.0 / d.Frequency as f64);
            }
        }
        if newest.is_some() {
            self.last_ms = newest;
        }
        newest
    }
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
