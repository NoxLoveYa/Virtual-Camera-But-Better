/*****************************************************************************
Copyright (C) OBS Studio contributors (https://github.com/obsproject/obs-studio)

Ported to Rust for Better Cam from OBS Studio, plugins/nv-filters/nvvfx-load.h (the function table of
NVVideoEffects.dll and NVCVImage.dll, the status codes, the selector strings and the pixel format enums).

The struct layout of `NvCVImage`, the enum values and the selector strings are NVIDIA's, from nvCVImage.h and
nvVideoEffects.h of the Maxine Video Effects SDK (https://github.com/NVIDIA-Maxine/Maxine-VFX-SDK, branch main as
fetched on 2026-11-17; the installed runtime reports version 0.7.6), which NVIDIA publishes under the MIT licence
(Copyright 2020-2021 NVIDIA Corporation). The tests at the end of this file check every struct field offset, enum
value and selector against the text of those headers.

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

//! The raw interface to NVIDIA's Video Effects runtime: the types, the selector strings and the table of exported
//! functions ([`Api`]). Nothing here owns anything; the owning wrappers are in `effect.rs`. This is the only file
//! (with `effect.rs`) that holds `unsafe` calls into NVIDIA's DLLs.
//!
//! Every function is called through a pointer taken from the loaded runtime ([`Api::load`]); the table lives inside
//! the [`Runtime`] it came from ([`Runtime::api`]), so a pointer cannot outlive its DLL.

use std::collections::VecDeque;
use std::ffi::{c_char, c_void, CStr};
use std::sync::Mutex;

use super::{Runtime, NVCV_SUCCESS};

// ---- types -------------------------------------------------------------------------------------------------------

/// `NvVFX_Handle`: one instance of an effect.
pub type EffectHandle = *mut c_void;
/// `CUstream`.
pub type CuStream = *mut c_void;
/// `NvVFX_StateObjectHandle`.
pub type StateHandle = *mut c_void;

/// `NvCVImage`, the image descriptor of NVIDIA's runtime (nvCVImage.h). `#[repr(C)]` and exactly 64 bytes on x64;
/// the enums are 4-byte ints (MSVC). Never build one by hand with a pixel pointer: ask the runtime
/// (`NvCVImage_Create`, `NvCVImage_InitFromD3D11Texture`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct NvCvImage {
    pub width: u32,
    pub height: u32,
    pub pitch: i32,
    pub pixel_format: i32,
    pub component_type: i32,
    pub pixel_bytes: u8,
    pub component_bytes: u8,
    pub num_components: u8,
    pub planar: u8,
    pub gpu_mem: u8,
    pub colorspace: u8,
    pub reserved: [u8; 2],
    pub pixels: *mut c_void,
    pub delete_ptr: *mut c_void,
    pub delete_proc: *mut c_void,
    pub buffer_bytes: u64,
}

impl NvCvImage {
    /// An empty descriptor, what the C++ default constructor leaves (all zero, no buffer).
    pub fn zeroed() -> NvCvImage {
        // SAFETY: the struct holds integers and raw pointers only; all-zero is its empty state.
        unsafe { std::mem::zeroed() }
    }
}

// `NvCVImage_PixelFormat` (the values of the build without `RTX_CAMERA_IMAGE`, which is what the runtime uses).
pub const NVCV_A: i32 = 2;
pub const NVCV_BGR: i32 = 5;
pub const NVCV_RGBA: i32 = 6;
pub const NVCV_BGRA: i32 = 7;
// `NvCVImage_ComponentType`.
pub const NVCV_U8: i32 = 1;
// Layout and memory space.
pub const NVCV_CHUNKY: u32 = 0;
pub const NVCV_GPU: u32 = 1;

// `NvCV_Status` values the code looks at (nvvfx-load.h in OBS, nvCVStatus.h).
pub const NVCV_ERR_MEMORY: i32 = -3;
pub const NVCV_ERR_MODEL: i32 = -10;
pub const NVCV_ERR_FILE: i32 = -13;
pub const NVCV_ERR_RESOLUTION: i32 = -16;
pub const NVCV_ERR_UNSUPPORTEDGPU: i32 = -17;
pub const NVCV_ERR_UNSUPPORTEDDRIVER: i32 = -19;
pub const NVCV_ERR_MODELDEPENDENCIES: i32 = -20;
pub const NVCV_ERR_TRT_ENGINE: i32 = -30;
pub const NVCV_ERR_CUDA_MEMORY: i32 = -102;
pub const NVCV_ERR_CUDA: i32 = -1099;

/// `NVCV_LOG_ERROR`, the verbosity OBS asks the runtime's logger for.
pub const NVCV_LOG_ERROR: i32 = 1;

// ---- selector strings (nvVideoEffects.h) -------------------------------------------------------------------------

pub const FX_GREEN_SCREEN: &CStr = c"GreenScreen";
pub const FX_BGBLUR: &CStr = c"BackgroundBlur";
pub const INPUT_IMAGE: &CStr = c"SrcImage0";
pub const INPUT_IMAGE_1: &CStr = c"SrcImage1";
pub const OUTPUT_IMAGE: &CStr = c"DstImage0";
pub const MODEL_DIRECTORY: &CStr = c"ModelDir";
pub const CUDA_STREAM: &CStr = c"CudaStream";
pub const MAX_INPUT_WIDTH: &CStr = c"MaxInputWidth";
pub const MAX_INPUT_HEIGHT: &CStr = c"MaxInputHeight";
pub const STRENGTH: &CStr = c"Strength";
pub const MODE: &CStr = c"Mode";
pub const STATE: &CStr = c"State";

// ---- the function table ------------------------------------------------------------------------------------------

type LogCallback = unsafe extern "C" fn(*mut c_void, *const c_char);

/// The exports of `NVVideoEffects.dll` and `NVCVImage.dll` that the filters call (the ones OBS loads for them).
pub struct Api {
    pub create_effect: unsafe extern "C" fn(*const c_char, *mut EffectHandle) -> i32,
    pub destroy_effect: unsafe extern "C" fn(EffectHandle),
    pub set_u32: unsafe extern "C" fn(EffectHandle, *const c_char, u32) -> i32,
    pub set_f32: unsafe extern "C" fn(EffectHandle, *const c_char, f32) -> i32,
    pub set_string: unsafe extern "C" fn(EffectHandle, *const c_char, *const c_char) -> i32,
    pub set_image: unsafe extern "C" fn(EffectHandle, *const c_char, *mut NvCvImage) -> i32,
    pub set_cuda_stream: unsafe extern "C" fn(EffectHandle, *const c_char, CuStream) -> i32,
    pub set_state_array: unsafe extern "C" fn(EffectHandle, *const c_char, *mut StateHandle) -> i32,
    pub allocate_state: unsafe extern "C" fn(EffectHandle, *mut StateHandle) -> i32,
    pub deallocate_state: unsafe extern "C" fn(EffectHandle, StateHandle) -> i32,
    /// `NvVFX_ResetState` (0.7.0 and newer; the runtime is at least 0.7.6, but a missing export is not fatal).
    pub reset_state: Option<unsafe extern "C" fn(EffectHandle, StateHandle) -> i32>,
    pub run: unsafe extern "C" fn(EffectHandle, i32) -> i32,
    pub load: unsafe extern "C" fn(EffectHandle) -> i32,
    pub stream_create: unsafe extern "C" fn(*mut CuStream) -> i32,
    pub stream_destroy: unsafe extern "C" fn(CuStream) -> i32,
    error_string: unsafe extern "C" fn(i32) -> *const c_char,
    pub image_create: unsafe extern "C" fn(u32, u32, i32, i32, u32, u32, u32, *mut *mut NvCvImage) -> i32,
    pub image_destroy: unsafe extern "C" fn(*mut NvCvImage),
    pub image_dealloc: unsafe extern "C" fn(*mut NvCvImage),
    pub image_transfer: unsafe extern "C" fn(*const NvCvImage, *mut NvCvImage, f32, CuStream, *mut NvCvImage) -> i32,
    pub map_resource: unsafe extern "C" fn(*mut NvCvImage, CuStream) -> i32,
    pub unmap_resource: unsafe extern "C" fn(*mut NvCvImage, CuStream) -> i32,
    pub init_from_d3d11: unsafe extern "C" fn(*mut NvCvImage, *mut c_void) -> i32,
}

impl Api {
    /// Looks every function up in `rt`'s DLLs and points the runtime's logger at [`log_tail`]. The error names the
    /// first export that is missing.
    pub fn load(rt: &Runtime) -> Result<Api, String> {
        // SAFETY: every type below is the signature the header gives for that export (see the module comment); the
        // pointers are only ever called through `&Api`, which lives inside the `Runtime` that owns the DLLs.
        let api = unsafe {
            Api {
                create_effect: rt.videofx_fn("NvVFX_CreateEffect")?,
                destroy_effect: rt.videofx_fn("NvVFX_DestroyEffect")?,
                set_u32: rt.videofx_fn("NvVFX_SetU32")?,
                set_f32: rt.videofx_fn("NvVFX_SetF32")?,
                set_string: rt.videofx_fn("NvVFX_SetString")?,
                set_image: rt.videofx_fn("NvVFX_SetImage")?,
                set_cuda_stream: rt.videofx_fn("NvVFX_SetCudaStream")?,
                set_state_array: rt.videofx_fn("NvVFX_SetStateObjectHandleArray")?,
                allocate_state: rt.videofx_fn("NvVFX_AllocateState")?,
                deallocate_state: rt.videofx_fn("NvVFX_DeallocateState")?,
                reset_state: rt.videofx_fn("NvVFX_ResetState").ok(),
                run: rt.videofx_fn("NvVFX_Run")?,
                load: rt.videofx_fn("NvVFX_Load")?,
                stream_create: rt.videofx_fn("NvVFX_CudaStreamCreate")?,
                stream_destroy: rt.videofx_fn("NvVFX_CudaStreamDestroy")?,
                error_string: rt.cvimage_fn("NvCV_GetErrorStringFromCode")?,
                image_create: rt.cvimage_fn("NvCVImage_Create")?,
                image_destroy: rt.cvimage_fn("NvCVImage_Destroy")?,
                image_dealloc: rt.cvimage_fn("NvCVImage_Dealloc")?,
                image_transfer: rt.cvimage_fn("NvCVImage_Transfer")?,
                map_resource: rt.cvimage_fn("NvCVImage_MapResource")?,
                unmap_resource: rt.cvimage_fn("NvCVImage_UnmapResource")?,
                init_from_d3d11: rt.cvimage_fn("NvCVImage_InitFromD3D11Texture")?,
            }
        };
        // `NvVFX_ConfigureLogger` exists since 0.7.5 (OBS: `MIN_VFX_LOGGER_VERSION`); the runtime is at least 0.7.6.
        // It is optional here: without it the error texts just have no extra line from NVIDIA.
        type Configure = unsafe extern "C" fn(i32, *const c_char, Option<LogCallback>, *mut c_void) -> i32;
        // SAFETY: signature from nvvfx-load.h (`NvVFX_ConfigureLogger_t`); the callback is a function of this program,
        // valid for as long as the program runs.
        if let Ok(configure) = unsafe { rt.videofx_fn::<Configure>("NvVFX_ConfigureLogger") } {
            unsafe { configure(NVCV_LOG_ERROR, std::ptr::null(), Some(log_callback), std::ptr::null_mut()) };
        }
        Ok(api)
    }

    /// NVIDIA's own words for a status code (`NvCV_GetErrorStringFromCode`).
    pub fn describe(&self, code: i32) -> String {
        // SAFETY: the function takes any int and returns a static C string or null.
        let p = unsafe { (self.error_string)(code) };
        if p.is_null() {
            return format!("status {code}");
        }
        // SAFETY: non-null and NUL terminated, owned by the runtime and never freed.
        unsafe { CStr::from_ptr(p) }.to_string_lossy().trim().to_string()
    }

    /// A sentence for the card when `what` (an action, "load the model") failed with `code`: NVIDIA's text, the code,
    /// a hint for the codes that have a known remedy, and the last line the runtime's logger printed.
    pub fn fail(&self, what: &str, code: i32) -> String {
        let hint = match code {
            NVCV_ERR_UNSUPPORTEDGPU => " This GPU is not supported by NVIDIA's video effects.",
            NVCV_ERR_UNSUPPORTEDDRIVER => " Update the NVIDIA graphics driver.",
            NVCV_ERR_MEMORY | NVCV_ERR_CUDA_MEMORY => " The GPU may be out of memory.",
            NVCV_ERR_MODEL | NVCV_ERR_FILE | NVCV_ERR_MODELDEPENDENCIES | NVCV_ERR_TRT_ENGINE => {
                " Reinstalling the NVIDIA Video Effects runtime may fix this."
            }
            NVCV_ERR_RESOLUTION => " The picture size is not supported.",
            NVCV_ERR_CUDA => " The GPU driver reported an error.",
            _ => "",
        };
        let logged = log_tail().map(|l| format!(" NVIDIA says: {l}")).unwrap_or_default();
        format!("NVIDIA's runtime could not {what}: {} (code {code}).{hint}{logged}", self.describe(code))
    }

    /// `Ok` for `NVCV_SUCCESS`, else [`Api::fail`].
    pub fn check(&self, code: i32, what: &str) -> Result<(), String> {
        if code == NVCV_SUCCESS {
            Ok(())
        } else {
            Err(self.fail(what, code))
        }
    }
}

// ---- the runtime's log ---------------------------------------------------------------------------------------------

static LOG: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());

/// What `NvVFX_ConfigureLogger` calls with each error line. It may come from any thread and must never unwind into
/// the runtime.
unsafe extern "C" fn log_callback(_data: *mut c_void, msg: *const c_char) {
    let _ = std::panic::catch_unwind(|| {
        if msg.is_null() {
            return;
        }
        // SAFETY: the runtime passes a NUL terminated string that is valid during the call.
        let line = unsafe { CStr::from_ptr(msg) }.to_string_lossy().trim().to_string();
        if line.is_empty() {
            return;
        }
        let mut log = LOG.lock().unwrap_or_else(|e| e.into_inner());
        if log.len() >= 8 {
            log.pop_front();
        }
        log.push_back(line);
    });
}

/// The newest line the runtime logged since the last call; the log is emptied.
pub fn log_tail() -> Option<String> {
    let mut log = LOG.lock().unwrap_or_else(|e| e.into_inner());
    let last = log.pop_back();
    log.clear();
    last
}

// ---- tests ---------------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{align_of, offset_of, size_of};

    /// The declaration of `NvCVImage` as the header prints it (nvCVImage.h, Maxine-VFX-SDK main), comments removed.
    const HEADER_STRUCT: &str = "
  unsigned int              width;
  unsigned int              height;
  signed int                pitch;
  NvCVImage_PixelFormat     pixelFormat;
  NvCVImage_ComponentType   componentType;
  unsigned char             pixelBytes;
  unsigned char             componentBytes;
  unsigned char             numComponents;
  unsigned char             planar;
  unsigned char             gpuMem;
  unsigned char             colorspace;
  unsigned char             reserved[2];
  void                      *pixels;
  void                      *deletePtr;
  void                      (*deleteProc)(void *p);
  unsigned long long        bufferBytes;
";

    /// Size and alignment of a header type on x64 Windows (MSVC): enums are 4-byte ints, pointers 8 bytes.
    fn c_type(t: &str) -> (usize, usize) {
        match t {
            "unsigned int" | "signed int" | "NvCVImage_PixelFormat" | "NvCVImage_ComponentType" => (4, 4),
            "unsigned char" => (1, 1),
            "unsigned long long" => (8, 8),
            p if p.starts_with("void") => (8, 8),
            other => panic!("unknown header type {other}"),
        }
    }

    /// `(name, offset)` of every field and the total size, by the C layout rules.
    fn header_layout() -> (Vec<(String, usize)>, usize) {
        let (mut off, mut max_align, mut fields) = (0usize, 1usize, Vec::new());
        for line in HEADER_STRUCT.lines().map(str::trim).filter(|l| !l.is_empty()) {
            let decl = line.trim_end_matches(';');
            let (ty, name, count) = if decl.contains("(*") {
                let name = decl.split("(*").nth(1).unwrap().split(')').next().unwrap();
                ("void *", name.to_string(), 1)
            } else {
                let (ty, rest) = decl.rsplit_once(char::is_whitespace).unwrap();
                let (name, count) = match rest.split_once('[') {
                    Some((n, c)) => (n, c.trim_end_matches(']').parse().unwrap()),
                    None => (rest, 1),
                };
                let ty = ty.trim().trim_end_matches('*').trim();
                (if rest.starts_with('*') || line.contains('*') { "void *" } else { ty }, name.trim_start_matches('*').to_string(), count)
            };
            let (size, align) = c_type(ty);
            off = off.next_multiple_of(align);
            fields.push((name, off));
            off += size * count;
            max_align = max_align.max(align);
        }
        (fields, off.next_multiple_of(max_align))
    }

    #[test]
    fn nvidia_nvcvimage_layout_matches_the_header() {
        let (fields, size) = header_layout();
        let ours: Vec<(&str, usize)> = vec![
            ("width", offset_of!(NvCvImage, width)),
            ("height", offset_of!(NvCvImage, height)),
            ("pitch", offset_of!(NvCvImage, pitch)),
            ("pixelFormat", offset_of!(NvCvImage, pixel_format)),
            ("componentType", offset_of!(NvCvImage, component_type)),
            ("pixelBytes", offset_of!(NvCvImage, pixel_bytes)),
            ("componentBytes", offset_of!(NvCvImage, component_bytes)),
            ("numComponents", offset_of!(NvCvImage, num_components)),
            ("planar", offset_of!(NvCvImage, planar)),
            ("gpuMem", offset_of!(NvCvImage, gpu_mem)),
            ("colorspace", offset_of!(NvCvImage, colorspace)),
            ("reserved", offset_of!(NvCvImage, reserved)),
            ("pixels", offset_of!(NvCvImage, pixels)),
            ("deletePtr", offset_of!(NvCvImage, delete_ptr)),
            ("deleteProc", offset_of!(NvCvImage, delete_proc)),
            ("bufferBytes", offset_of!(NvCvImage, buffer_bytes)),
        ];
        assert_eq!(fields.len(), ours.len());
        for ((hn, ho), (on, oo)) in fields.iter().zip(&ours) {
            assert_eq!(hn, on, "field order");
            assert_eq!(ho, oo, "offset of {hn}");
        }
        assert_eq!(size, size_of::<NvCvImage>(), "size of NvCVImage");
        assert_eq!(size, 64);
        assert_eq!(align_of::<NvCvImage>(), 8);
        // The hand-written numbers the layout comes to, so a mistake in the little calculator above shows up too.
        assert_eq!((offset_of!(NvCvImage, pixel_format), offset_of!(NvCvImage, pixel_bytes), offset_of!(NvCvImage, pixels)), (12, 20, 32));
    }

    /// The enum and `#define` lines the code depends on, verbatim from the headers.
    const HEADER_DEFINES: &str = r#"
  NVCV_A               = 2,    //!< Alpha (opacity)
  NVCV_BGR             = 5,    //!< { Red, Green, Blue }
  NVCV_RGBA            = 6,    //!< { Red, Green, Blue, Alpha }
  NVCV_BGRA            = 7,    //!< { Red, Green, Blue, Alpha }
  NVCV_U8            = 1,      //!< Unsigned 8-bit integer.
#define NVCV_CHUNKY        0   //!< All components of pixel(x,y) are adjacent (same as interleaved).
#define NVCV_GPU          1   //!< The buffer is stored in CUDA memory.
#define NVVFX_FX_GREEN_SCREEN           "GreenScreen"         // Green Screen 
#define NVVFX_FX_BGBLUR                 "BackgroundBlur"     // Background blur
#define NVVFX_INPUT_IMAGE_0             "SrcImage0"           //!< There may be multiple input images
#define NVVFX_INPUT_IMAGE_1             "SrcImage1"           //!< Source Image 1
#define NVVFX_OUTPUT_IMAGE_0            "DstImage0"           //!< There may be multiple output images
#define NVVFX_MODEL_DIRECTORY           "ModelDir"            //!< The directory where the model may be found
#define NVVFX_CUDA_STREAM               "CudaStream"          //!< The CUDA stream to use
#define NVVFX_MAX_INPUT_WIDTH           "MaxInputWidth"       //!< Maximum width of the input supported
#define NVVFX_MAX_INPUT_HEIGHT          "MaxInputHeight"      //!< Maximum height of the input supported
#define NVVFX_STRENGTH                  "Strength"            //!< Strength for different filters
#define NVVFX_MODE                      "Mode"                //!< Mode for different filters
#define NVVFX_STATE                     "State"               //!< State variable  
"#;

    /// The header's value of a name: a number for an enum or number `#define`, the text for a string `#define`.
    fn header_value(name: &str) -> String {
        for line in HEADER_DEFINES.lines().map(str::trim) {
            let rest = if let Some(r) = line.strip_prefix("#define ") {
                r.strip_prefix(name).filter(|r| r.starts_with(char::is_whitespace))
            } else {
                line.strip_prefix(name).filter(|r| r.trim_start().starts_with('='))
            };
            let Some(rest) = rest else { continue };
            let rest = rest.trim_start().trim_start_matches('=').trim_start();
            if let Some(q) = rest.strip_prefix('"') {
                return q.split('"').next().unwrap().to_string();
            }
            return rest.split(|c: char| !c.is_ascii_digit()).next().unwrap().to_string();
        }
        panic!("{name} is not in the header excerpt");
    }

    #[test]
    fn nvidia_constants_and_selectors_match_the_header() {
        for (name, ours) in [
            ("NVCV_A", NVCV_A),
            ("NVCV_BGR", NVCV_BGR),
            ("NVCV_RGBA", NVCV_RGBA),
            ("NVCV_BGRA", NVCV_BGRA),
            ("NVCV_U8", NVCV_U8),
            ("NVCV_CHUNKY", NVCV_CHUNKY as i32),
            ("NVCV_GPU", NVCV_GPU as i32),
        ] {
            assert_eq!(header_value(name), ours.to_string(), "{name}");
        }
        for (name, ours) in [
            ("NVVFX_FX_GREEN_SCREEN", FX_GREEN_SCREEN),
            ("NVVFX_FX_BGBLUR", FX_BGBLUR),
            ("NVVFX_INPUT_IMAGE_0", INPUT_IMAGE),
            ("NVVFX_INPUT_IMAGE_1", INPUT_IMAGE_1),
            ("NVVFX_OUTPUT_IMAGE_0", OUTPUT_IMAGE),
            ("NVVFX_MODEL_DIRECTORY", MODEL_DIRECTORY),
            ("NVVFX_CUDA_STREAM", CUDA_STREAM),
            ("NVVFX_MAX_INPUT_WIDTH", MAX_INPUT_WIDTH),
            ("NVVFX_MAX_INPUT_HEIGHT", MAX_INPUT_HEIGHT),
            ("NVVFX_STRENGTH", STRENGTH),
            ("NVVFX_MODE", MODE),
            ("NVVFX_STATE", STATE),
        ] {
            assert_eq!(header_value(name), ours.to_str().unwrap(), "{name}");
        }
    }

    /// The status codes, from nvvfx-load.h of OBS (the list of nvCVStatus.h).
    const STATUS_CODES: &str = "
	NVCV_ERR_MEMORY = -3,             //!< There is not enough memory for the requested operation.
	NVCV_ERR_MODEL = -10,             //!< Error while loading the TRT model.
	NVCV_ERR_FILE = -13,              //!< The file could not be found.
	NVCV_ERR_RESOLUTION = -16,        //!< The specified image resolution is not supported.
	NVCV_ERR_UNSUPPORTEDGPU = -17,    //!< The GPU is not supported
	NVCV_ERR_UNSUPPORTEDDRIVER = -19, //!< The currently installed graphics driver is not supported
	NVCV_ERR_MODELDEPENDENCIES = -20, //!< There is no model with dependencies that match this system
	NVCV_ERR_TRT_ENGINE = -30,        ///< There was a problem deserializing the inference runtime engine.
	NVCV_ERR_CUDA_MEMORY = -102,      //!< There is not enough CUDA memory for the requested operation.
	NVCV_ERR_CUDA = -1099,                //!< An otherwise unspecified CUDA error has been reported.
";

    #[test]
    fn nvidia_status_codes_match_the_header() {
        let value = |name: &str| -> i32 {
            STATUS_CODES
                .lines()
                .map(str::trim)
                .find_map(|l| l.strip_prefix(name).and_then(|r| r.trim_start().strip_prefix('=')))
                .map(|r| r.split(',').next().unwrap().trim().parse().unwrap())
                .unwrap_or_else(|| panic!("{name}"))
        };
        assert_eq!(value("NVCV_ERR_MEMORY"), NVCV_ERR_MEMORY);
        assert_eq!(value("NVCV_ERR_MODEL"), NVCV_ERR_MODEL);
        assert_eq!(value("NVCV_ERR_FILE"), NVCV_ERR_FILE);
        assert_eq!(value("NVCV_ERR_RESOLUTION"), NVCV_ERR_RESOLUTION);
        assert_eq!(value("NVCV_ERR_UNSUPPORTEDGPU"), NVCV_ERR_UNSUPPORTEDGPU);
        assert_eq!(value("NVCV_ERR_UNSUPPORTEDDRIVER"), NVCV_ERR_UNSUPPORTEDDRIVER);
        assert_eq!(value("NVCV_ERR_MODELDEPENDENCIES"), NVCV_ERR_MODELDEPENDENCIES);
        assert_eq!(value("NVCV_ERR_TRT_ENGINE"), NVCV_ERR_TRT_ENGINE);
        assert_eq!(value("NVCV_ERR_CUDA_MEMORY"), NVCV_ERR_CUDA_MEMORY);
        assert_eq!(value("NVCV_ERR_CUDA"), NVCV_ERR_CUDA);
    }
}
