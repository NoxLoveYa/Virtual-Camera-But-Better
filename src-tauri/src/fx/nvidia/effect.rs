/*****************************************************************************
Copyright (C) OBS Studio contributors (https://github.com/obsproject/obs-studio)

Ported to Rust for Better Cam from OBS Studio, plugins/nv-filters/nvidia-videofx-filter.c: the effect set-up of
`nvvfx_filter_create_internal`, `nvvfx_filter_update`, `create_alpha_texture`, `create_blur_texture`,
`init_blur_images`, `init_images` and the per-frame transfers of `process_texture`.

This program is free software: you can redistribute it and/or modify
it under the terms of the GNU General Public License as published by
the Free Software Foundation, either version 2 of the License, or
(at your option) any later version.

This program is distributed in the hope that it will be useful,
but WITHOUT ANY WARRANTY; without even the implied warranty of
MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
GNU General Public License for more details.

You should have received a copy of the GNU General Public License
along with this program.  If not, see <http://www.gnu.org/licenses/>.
*****************************************************************************/

//! The GPU side of the NVIDIA filters: owning wrappers for NVIDIA's handles and [`Pipeline`], everything one filter
//! needs for one picture size and one mode (effects, CUDA stream, images, the three Direct3D textures that carry the
//! picture in and the mask or blurred picture out).
//!
//! Ownership rules, all enforced by `Drop`:
//! * every wrapper holds an `Arc<Runtime>`, so the DLLs outlive the handle they made;
//! * a [`Pipeline`] releases in the order OBS's `nvvfx_filter_actual_destroy` does: the effects (each with its state
//!   object), then the images (a texture image unregisters its CUDA resource), the stream, the textures, and last of
//!   all the runtime;
//! * a pipeline is built on the filter's loader thread (the model takes seconds to load) and then used by the render
//!   thread alone, never by both at once: [`Pipeline`] is `Send` for that hand-over.

use std::ffi::CStr;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11DeviceContext, ID3D11ShaderResourceView, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_FORMAT_A8_UNORM, DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC};

use super::ffi::{self, Api, CuStream, EffectHandle, NvCvImage, StateHandle};
use super::Runtime;
use crate::gpu::GpuDevice;

/// The smallest picture the segmentation models take (OBS: "minimum size supported by SDK is (512,288)").
pub const MIN_SEGMENTATION_SIZE: (u32, u32) = (512, 288);

/// Which of the three filters a pipeline serves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `nv_greenscreen_filter`: the segmentation effect makes a mask, the background goes transparent.
    Removal,
    /// `nv_blur_filter`: the blur effect on the whole picture.
    Blur,
    /// `nv_background_blur_filter`: segmentation (always the Performance model) feeds the blur effect.
    BackgroundBlur,
}

impl Kind {
    /// The segmentation effect (`NVVFX_FX_GREEN_SCREEN`) runs.
    pub fn segments(self) -> bool {
        self != Kind::Blur
    }
    /// The blur effect (`NVVFX_FX_BGBLUR`) runs.
    pub fn blurs(self) -> bool {
        self != Kind::Removal
    }
}

/// What a pipeline is built for. Two equal specs make interchangeable pipelines.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Spec {
    pub kind: Kind,
    /// `NVVFX_MODE` of the segmentation effect (0 to 3); unused by [`Kind::Blur`].
    pub mode: u32,
    pub width: u32,
    pub height: u32,
}

/// Pipelines alive now (tests count them to prove that dropping a filter releases everything).
static LIVE: AtomicUsize = AtomicUsize::new(0);

pub fn live_pipelines() -> usize {
    LIVE.load(Ordering::SeqCst)
}

// ---- owning wrappers -----------------------------------------------------------------------------------------------

/// A CUDA stream made by `NvVFX_CudaStreamCreate`.
struct Stream {
    rt: Arc<Runtime>,
    ptr: CuStream,
}

impl Stream {
    fn create(rt: &Arc<Runtime>) -> Result<Stream, String> {
        let api = rt.api()?;
        let mut ptr: CuStream = null_mut();
        // SAFETY: `ptr` is a local that the call fills in.
        api.check(unsafe { (api.stream_create)(&mut ptr) }, "create a CUDA stream")?;
        Ok(Stream { rt: rt.clone(), ptr })
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        if let Ok(api) = self.rt.api() {
            // SAFETY: the stream came from `stream_create` and nothing uses it any more (the effects and images that
            // were given it are declared before it in `Pipeline` and are gone).
            unsafe { (api.stream_destroy)(self.ptr) };
        }
    }
}

/// An effect instance (`NvVFX_CreateEffect`), with its state object when it has one.
struct Effect {
    rt: Arc<Runtime>,
    handle: EffectHandle,
    /// The state object's handle, in a box of its own: `NvVFX_SetStateObjectHandleArray` keeps the *address* of the
    /// handle (OBS passes a field of its heap allocated filter), so the address must not change when the `Effect`
    /// moves.
    state: Box<StateHandle>,
}

impl Effect {
    fn create(rt: &Arc<Runtime>, selector: &CStr, what: &str) -> Result<Effect, String> {
        let api = rt.api()?;
        let mut handle: EffectHandle = null_mut();
        // SAFETY: the selector is a NUL terminated string and `handle` a local the call fills in.
        api.check(unsafe { (api.create_effect)(selector.as_ptr(), &mut handle) }, what)?;
        if handle.is_null() {
            return Err(format!("NVIDIA's runtime could not {what}: it returned no effect."));
        }
        Ok(Effect { rt: rt.clone(), handle, state: Box::new(null_mut()) })
    }

    fn api(&self) -> Result<&Api, String> {
        self.rt.api()
    }

    fn set_u32(&self, selector: &CStr, value: u32, what: &str) -> Result<(), String> {
        let api = self.api()?;
        // SAFETY: valid handle and selector.
        api.check(unsafe { (api.set_u32)(self.handle, selector.as_ptr(), value) }, what)
    }

    fn set_f32(&self, selector: &CStr, value: f32, what: &str) -> Result<(), String> {
        let api = self.api()?;
        // SAFETY: valid handle and selector.
        api.check(unsafe { (api.set_f32)(self.handle, selector.as_ptr(), value) }, what)
    }

    fn set_string(&self, selector: &CStr, value: &CStr, what: &str) -> Result<(), String> {
        let api = self.api()?;
        // SAFETY: valid handle, selector and NUL terminated value (the effect copies it).
        api.check(unsafe { (api.set_string)(self.handle, selector.as_ptr(), value.as_ptr()) }, what)
    }

    fn set_stream(&self, stream: &Stream, what: &str) -> Result<(), String> {
        let api = self.api()?;
        // SAFETY: valid handle; the stream outlives the effect (it is declared after it in `Pipeline`).
        api.check(unsafe { (api.set_cuda_stream)(self.handle, ffi::CUDA_STREAM.as_ptr(), stream.ptr) }, what)
    }

    /// `NvVFX_SetImage`: the effect keeps a shallow copy of the descriptor; the buffer stays ours.
    fn set_image(&self, selector: &CStr, image: *mut NvCvImage, what: &str) -> Result<(), String> {
        let api = self.api()?;
        // SAFETY: valid handle and selector; `image` points at a live descriptor.
        api.check(unsafe { (api.set_image)(self.handle, selector.as_ptr(), image) }, what)
    }

    /// The state object the segmentation effect keeps between frames (OBS allocates it from 0.7.6 on).
    fn allocate_state(&mut self) -> Result<(), String> {
        let api = self.rt.api()?;
        // SAFETY: valid handle; the call fills in the boxed handle.
        api.check(unsafe { (api.allocate_state)(self.handle, &mut *self.state) }, "allocate the effect's state")?;
        // SAFETY: valid handle; the effect keeps the pointer to the boxed handle, which lives as long as the effect.
        api.check(unsafe { (api.set_state_array)(self.handle, ffi::STATE.as_ptr(), &mut *self.state) }, "attach the effect's state")
    }

    fn load(&self, what: &str) -> Result<(), String> {
        let api = self.api()?;
        // SAFETY: valid handle.
        api.check(unsafe { (api.load)(self.handle) }, what)
    }

    /// Forgets what the state object remembers of earlier frames (`NvVFX_ResetState`), if the runtime can.
    fn reset_state(&self) -> Result<(), String> {
        let api = self.api()?;
        match (api.reset_state, self.state.is_null()) {
            // SAFETY: valid handle and state object.
            (Some(reset), false) => api.check(unsafe { reset(self.handle, *self.state) }, "reset the effect's state"),
            _ => Ok(()),
        }
    }

    /// `NvVFX_Run(handle, SYNC)`: returns when the effect has finished on its stream.
    fn run(&self, what: &str) -> Result<(), String> {
        let api = self.api()?;
        // SAFETY: valid handle; images and stream were set before.
        api.check(unsafe { (api.run)(self.handle, 0) }, what)
    }
}

impl Drop for Effect {
    fn drop(&mut self) {
        if let Ok(api) = self.rt.api() {
            // SAFETY: valid handle and state, released once, state first (OBS does the same).
            unsafe {
                if !self.state.is_null() {
                    (api.deallocate_state)(self.handle, *self.state);
                }
                (api.destroy_effect)(self.handle);
            }
        }
    }
}

/// A buffer on the GPU in NVIDIA's own memory (`NvCVImage_Create`, one byte row alignment as the effects require).
struct GpuImage {
    rt: Arc<Runtime>,
    ptr: *mut NvCvImage,
}

impl GpuImage {
    fn create(rt: &Arc<Runtime>, width: u32, height: u32, format: i32, what: &str) -> Result<GpuImage, String> {
        let api = rt.api()?;
        let mut ptr: *mut NvCvImage = null_mut();
        // SAFETY: `ptr` is a local the call fills in.
        api.check(
            unsafe { (api.image_create)(width, height, format, ffi::NVCV_U8, ffi::NVCV_CHUNKY, ffi::NVCV_GPU, 1, &mut ptr) },
            what,
        )?;
        if ptr.is_null() {
            return Err(format!("NVIDIA's runtime could not {what}: it returned no image."));
        }
        Ok(GpuImage { rt: rt.clone(), ptr })
    }
}

impl Drop for GpuImage {
    fn drop(&mut self) {
        if let Ok(api) = self.rt.api() {
            // SAFETY: made by `image_create`, destroyed once.
            unsafe { (api.image_destroy)(self.ptr) };
        }
    }
}

/// A Direct3D 11 texture seen as an image (`NvCVImage_InitFromD3D11Texture`): it registers the texture with CUDA.
/// Between [`TexImage::map`] and [`TexImage::unmap`] CUDA owns the texture; outside them Direct3D does.
struct TexImage {
    rt: Arc<Runtime>,
    image: Box<NvCvImage>,
    /// Keeps the texture alive for as long as the registration exists.
    _texture: ID3D11Texture2D,
}

impl TexImage {
    fn wrap(rt: &Arc<Runtime>, texture: &ID3D11Texture2D, what: &str) -> Result<TexImage, String> {
        let api = rt.api()?;
        let mut image = Box::new(NvCvImage::zeroed());
        // SAFETY: `image` is an empty descriptor; the texture is a live ID3D11Texture2D.
        api.check(unsafe { (api.init_from_d3d11)(&mut *image, texture.as_raw()) }, what)?;
        Ok(TexImage { rt: rt.clone(), image, _texture: texture.clone() })
    }

    fn map(&mut self, stream: &Stream, what: &str) -> Result<(), String> {
        let api = self.rt.api()?;
        // SAFETY: a registered texture image and a live stream.
        api.check(unsafe { (api.map_resource)(&mut *self.image, stream.ptr) }, what)
    }

    fn unmap(&mut self, stream: &Stream, what: &str) -> Result<(), String> {
        let api = self.rt.api()?;
        // SAFETY: as `map`; must follow every successful map.
        api.check(unsafe { (api.unmap_resource)(&mut *self.image, stream.ptr) }, what)
    }
}

impl Drop for TexImage {
    fn drop(&mut self) {
        if let Ok(api) = self.rt.api() {
            // SAFETY: the descriptor was filled by `init_from_d3d11`; `Dealloc` unregisters the CUDA resource.
            unsafe { (api.image_dealloc)(&mut *self.image) };
        }
    }
}

/// `NvCVImage_Transfer(src, dst, 1.0, stream, NULL)`: converts and copies between two images on the GPU.
fn transfer(rt: &Runtime, src: *const NvCvImage, dst: *mut NvCvImage, stream: &Stream, what: &str) -> Result<(), String> {
    let api = rt.api()?;
    // SAFETY: both descriptors are live and, for texture images, currently mapped; no temporary image is needed for
    // GPU to GPU conversions.
    api.check(unsafe { (api.image_transfer)(src, dst, 1.0, stream.ptr, null_mut()) }, what)
}

/// Maps `image`, runs `f` on its descriptor, and unmaps again whatever `f` returned.
fn with_mapped(image: &mut TexImage, stream: &Stream, what: &str, f: impl FnOnce(&mut NvCvImage) -> Result<(), String>) -> Result<(), String> {
    image.map(stream, what)?;
    let done = f(&mut image.image);
    let unmapped = image.unmap(stream, what);
    done.and(unmapped)
}

/// A texture of the pipeline: the Direct3D texture and, for the ones the shader reads, its view.
pub struct Tex {
    pub texture: ID3D11Texture2D,
    pub srv: ID3D11ShaderResourceView,
}

fn make_texture(gpu: &GpuDevice, width: u32, height: u32, format: DXGI_FORMAT, render_target: bool) -> Result<Tex, String> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | if render_target { D3D11_BIND_RENDER_TARGET.0 } else { 0 }) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let texture = gpu.create_texture(&desc).map_err(|e| format!("Could not create a texture for the NVIDIA effect: {e}"))?;
    let srv = gpu.create_srv(&texture, format).map_err(|e| format!("Could not create a texture view for the NVIDIA effect: {e}"))?;
    Ok(Tex { texture, srv })
}

// ---- the pipeline --------------------------------------------------------------------------------------------------

/// One filter's effects and images for one [`Spec`].
///
/// The fields are dropped in declaration order: effects, texture images, buffers, stream, textures, runtime.
pub struct Pipeline {
    pub spec: Spec,
    /// How long the build took: effects, model, images and the first run.
    pub load_time: Duration,
    segment: Option<Effect>,
    blur: Option<Effect>,
    mask_image: Option<TexImage>,
    blur_image: Option<TexImage>,
    src_image: TexImage,
    /// The picture as BGR bytes in NVIDIA's memory: the input of both effects.
    bgr_src: GpuImage,
    /// The segmentation effect's mask (A, 8 bit).
    a_dst: Option<GpuImage>,
    /// The blur effect's result (BGR) and the RGBA copy of it that goes into the texture.
    blur_bgr: Option<GpuImage>,
    rgba_dst: Option<GpuImage>,
    stream: Stream,
    src_texture: Tex,
    /// The mask the shader reads (`A8_UNORM`), for the segmentation kinds.
    pub mask: Option<Tex>,
    /// The blurred picture the shader reads, for the blur kinds.
    pub blurred: Option<Tex>,
    rt: Arc<Runtime>,
    _live: LiveGuard,
}

struct LiveGuard;

impl LiveGuard {
    fn new() -> LiveGuard {
        LIVE.fetch_add(1, Ordering::SeqCst);
        LiveGuard
    }
}

impl Drop for LiveGuard {
    fn drop(&mut self) {
        LIVE.fetch_sub(1, Ordering::SeqCst);
    }
}

// SAFETY: a pipeline holds raw pointers into NVIDIA's runtime (effect handles, images, a CUDA stream) and Direct3D
// objects. It is built on one thread, handed over whole, and from then on used by one thread at a time; the runtime
// does not bind its handles to the thread that made them (OBS builds them on its graphics thread and its loader
// threads alike).
unsafe impl Send for Pipeline {}

impl Pipeline {
    /// Creates the effects, loads the model (seconds) and allocates every image for `spec`. `strength` is the blur
    /// intensity. Runs on the loader thread, never on the render thread.
    pub fn build(rt: &Arc<Runtime>, gpu: &GpuDevice, spec: Spec, strength: f32) -> Result<Pipeline, String> {
        let started = Instant::now();
        let Spec { kind, mode, width, height } = spec;
        rt.api()?;
        let stream = Stream::create(rt)?;

        // 1 and 2 (`nvvfx_filter_create_internal`): create the effects, point them at the models and the stream.
        let mut segment = None;
        if kind.segments() {
            let mut fx = Effect::create(rt, ffi::FX_GREEN_SCREEN, "create the segmentation effect")?;
            let models = std::ffi::CString::new(rt.models_dir().to_string_lossy().into_owned())
                .map_err(|_| "The NVIDIA runtime folder has a character that cannot be passed to it.".to_string())?;
            fx.set_string(ffi::MODEL_DIRECTORY, &models, "set the model folder")?;
            fx.set_stream(&stream, "give the effect its CUDA stream")?;
            fx.allocate_state()?;
            // `nvvfx_filter_update`: the mode (Background Blur always uses the Performance model, 1).
            fx.set_u32(ffi::MODE, mode, "select the model")?;
            segment = Some(fx);
        }
        let mut blur = None;
        if kind.blurs() {
            let fx = Effect::create(rt, ffi::FX_BGBLUR, "create the blur effect")?;
            fx.set_stream(&stream, "give the effect its CUDA stream")?;
            blur = Some(fx);
        }

        // The textures that carry the picture in (render target, like OBS's `render_unorm`) and the results out.
        let src_texture = make_texture(gpu, width, height, DXGI_FORMAT_R8G8B8A8_UNORM, true)?;
        let mask = kind.segments().then(|| make_texture(gpu, width, height, DXGI_FORMAT_A8_UNORM, false)).transpose()?;
        let blurred = kind.blurs().then(|| make_texture(gpu, width, height, DXGI_FORMAT_R8G8B8A8_UNORM, false)).transpose()?;

        // `create_alpha_texture` / `create_blur_texture` / `init_images`: the images.
        let src_image = TexImage::wrap(rt, &src_texture.texture, "use the picture as a CUDA image")?;
        let mask_image = mask.as_ref().map(|m| TexImage::wrap(rt, &m.texture, "use the mask texture as a CUDA image")).transpose()?;
        let blur_image = blurred.as_ref().map(|b| TexImage::wrap(rt, &b.texture, "use the blur texture as a CUDA image")).transpose()?;
        let bgr_src = GpuImage::create(rt, width, height, ffi::NVCV_BGR, "allocate the BGR picture")?;
        let a_dst = kind.segments().then(|| GpuImage::create(rt, width, height, ffi::NVCV_A, "allocate the mask")).transpose()?;
        let blur_bgr = kind.blurs().then(|| GpuImage::create(rt, width, height, ffi::NVCV_BGR, "allocate the blurred picture")).transpose()?;
        let rgba_dst = kind.blurs().then(|| GpuImage::create(rt, width, height, ffi::NVCV_RGBA, "allocate the RGBA blurred picture")).transpose()?;

        // `init_blur_images`: input, mask input (Background Blur only), output; then load.
        if let (Some(fx), Some(out)) = (&blur, &blur_bgr) {
            fx.set_f32(ffi::STRENGTH, strength, "set the blur strength")?;
            fx.set_image(ffi::INPUT_IMAGE, bgr_src.ptr, "give the blur effect its picture")?;
            if let Some(mask) = &a_dst {
                fx.set_image(ffi::INPUT_IMAGE_1, mask.ptr, "give the blur effect its mask")?;
            }
            fx.set_image(ffi::OUTPUT_IMAGE, out.ptr, "give the blur effect its output")?;
            fx.load("load the blur effect")?;
        }

        // `init_images` step 7: the segmentation effect's images and maximum size, then the model load.
        if let (Some(fx), Some(a)) = (&segment, &a_dst) {
            fx.set_image(ffi::INPUT_IMAGE, bgr_src.ptr, "give the segmentation effect its picture")?;
            fx.set_image(ffi::OUTPUT_IMAGE, a.ptr, "give the segmentation effect its output")?;
            fx.set_u32(ffi::MAX_INPUT_WIDTH, width, "set the largest picture width")?;
            fx.set_u32(ffi::MAX_INPUT_HEIGHT, height, "set the largest picture height")?;
            fx.load("load the background model")?;
        }

        let mut pipeline = Pipeline {
            spec,
            load_time: Duration::ZERO,
            segment,
            blur,
            mask_image,
            blur_image,
            src_image,
            bgr_src,
            a_dst,
            blur_bgr,
            rgba_dst,
            stream,
            src_texture,
            mask,
            blurred,
            rt: rt.clone(),
            _live: LiveGuard::new(),
        };

        // The effects finish their set-up lazily, on the first run (seconds on a PC that has never run them: CUDA
        // compiles its kernels for the GPU). Do that run here, on the loader thread, on the still empty picture, so
        // the first frame of the render thread is as fast as the rest; then forget what the run left in the state.
        pipeline.run_effects(true)?;
        if let Some(fx) = &pipeline.segment {
            fx.reset_state()?;
        }
        pipeline.load_time = started.elapsed();
        Ok(pipeline)
    }

    /// `nvvfx_filter_update` for the blur effect: a new intensity (`NVVFX_STRENGTH`) and a reload of the effect.
    pub fn set_strength(&mut self, strength: f32) -> Result<(), String> {
        match &self.blur {
            Some(fx) => {
                fx.set_f32(ffi::STRENGTH, strength, "set the blur strength")?;
                fx.load("load the blur effect")
            }
            None => Ok(()),
        }
    }

    /// `process_texture`: copies `input` into the pipeline's own texture and runs the effects on it. With
    /// `run_mask` false the segmentation effect is skipped and the mask of the last run stays (`processing_interval`).
    /// Render thread only. `input` must have the pipeline's size.
    pub fn process(&mut self, ctx: &ID3D11DeviceContext, input: &ID3D11Texture2D, run_mask: bool) -> Result<(), String> {
        // The pooled input texture changes from frame to frame; the registered texture is ours and stays.
        // SAFETY: both textures are live, have the same size and format, and belong to one device.
        unsafe { ctx.CopyResource(&self.src_texture.texture, input) };
        self.run_effects(run_mask)
    }

    /// Steps 1 to 5 of `process_texture` on whatever the pipeline's own picture texture holds.
    fn run_effects(&mut self, run_mask: bool) -> Result<(), String> {
        let rt = self.rt.clone();
        let stream = &self.stream;

        // 1 and 2: the picture to BGR in NVIDIA's memory.
        let bgr = self.bgr_src.ptr;
        with_mapped(&mut self.src_image, stream, "hand the picture to CUDA", |src| {
            transfer(&rt, src, bgr, stream, "convert the picture to BGR")
        })?;

        // 3 and 4: the mask.
        if run_mask {
            if let (Some(fx), Some(a), Some(mask_image)) = (&self.segment, &self.a_dst, &mut self.mask_image) {
                fx.run("run the segmentation effect")?;
                with_mapped(mask_image, stream, "hand the mask texture to CUDA", |dst| {
                    transfer(&rt, a.ptr, dst, stream, "copy the mask into its texture")
                })?;
            }
        }

        // 5: the blur.
        if let (Some(fx), Some(blur_bgr), Some(rgba), Some(blur_image)) = (&self.blur, &self.blur_bgr, &self.rgba_dst, &mut self.blur_image) {
            fx.run("run the blur effect")?;
            transfer(&rt, blur_bgr.ptr, rgba.ptr, stream, "convert the blurred picture to RGBA")?;
            with_mapped(blur_image, stream, "hand the blur texture to CUDA", |dst| {
                transfer(&rt, rgba.ptr, dst, stream, "copy the blurred picture into its texture")
            })?;
        }
        Ok(())
    }

    /// What NVIDIA made of the texture formats: `(pixelFormat, componentType, width, height)` of the picture image.
    #[cfg(test)]
    pub fn describe_source(&self) -> (i32, i32, u32, u32) {
        let i = &*self.src_image.image;
        (i.pixel_format, i.component_type, i.width, i.height)
    }
}

