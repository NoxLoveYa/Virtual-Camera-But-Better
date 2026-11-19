/*****************************************************************************
Written for Better Cam following OBS Studio, plugins/obs-filters/color-grade-filter.c (`load_cube_file`,
`get_cube_entry`, `load_1d_lut`, `load_3d_lut`: the `.cube` reader, the RGBA16F tables and the sRGB to linear
conversion of the table values) and plugins/obs-filters/mask-filter.c (`mask_filter_tick`: the file's timestamp is
looked at about once a second and the image is loaded again when it changed).

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

//! Files that filters read: pictures and colour lookup tables (plan 4.5, last bullet).
//!
//! # How a filter uses it
//! The engine owns one [`Assets`] service with one loader thread. The chain hands every filter an [`AssetHandle`]
//! ([`super::GpuFilter::attach_assets`]). In `update` (cheap, never blocks) the filter says which file it wants:
//!
//! ```ignore
//! self.assets.request(AssetKind::Image, ps(p, "image_path", ""));   // "" = no asset
//! ```
//!
//! and in `render` it asks what is there:
//!
//! ```ignore
//! match self.assets.get().as_deref() {
//!     Some(Asset::Image(img)) => { /* img.srv is a shader resource view, img.width x img.height */ }
//!     _ => return r.copy(input, output),      // not loaded yet, or it failed: pass the picture through
//! }
//! ```
//!
//! [`ShaderFilter`](super::shader_filter::ShaderFilter) does exactly this for a definition whose `extra` list has an
//! [`Image`](super::shader_filter::ExtraInput::Image) or [`Lut3D`](super::shader_filter::ExtraInput::Lut3D) entry,
//! reading the path from the `image_path` setting.
//!
//! # What the service does
//! * Loading happens on the loader thread: files are read, decoded (Windows Imaging Component, or the `.cube`
//!   parser) and uploaded to immutable GPU textures there. The render thread never decodes and never waits on disk.
//! * The cache is keyed by kind and normalized path ([`normalize`]): two filters that ask for one file cause one
//!   load. An entry lives while some handle uses it.
//! * About once a second the loader looks at the modification time and size of every file in use and loads it again
//!   when they changed (as OBS's mask filter does). The old asset stays in use until the new one is ready.
//! * An empty path is "no asset" and is not an error.
//! * A failed load is reported through the event function of [`Assets::new`] as a
//!   [`FilterError`](super::FilterError) for each filter that asked: `could not load <file name>: <reason>`.
//!   It is reported once per failed load, not once per frame. When a load succeeds for a filter that had no
//!   success reported yet (or had an error), the same event goes out with an **empty message**: the card clears its
//!   error line.
//!
//! # Formats
//! * Images ([`decode_image`]): PNG, JPEG, BMP and GIF (first frame), decoded by WIC into straight-alpha RGBA, top
//!   row first, EXIF orientation ignored (as OBS). At most [`MAX_IMAGE_DIM`] in either direction. TGA is not
//!   supported.
//! * `.cube` tables ([`parse_cube`]): 1D or 3D, `DOMAIN_MIN` / `DOMAIN_MAX`, at most [`MAX_LUT_3D`] (3D) or
//!   [`MAX_LUT_1D`] (1D) entries per axis.
//!
//! # Asset kinds
//! * [`AssetKind::Image`]: any image file becomes [`Asset::Image`], an `R8G8B8A8_UNORM` texture.
//! * [`AssetKind::Lut`]: a file ending in `.cube` becomes [`Asset::Cube`] (parsed table plus its GPU texture);
//!   any other file is decoded as an image and becomes [`Asset::LutImage`] (pixels only, no texture): the `.png`
//!   LUT layouts are for the filter to build, with [`create_texture3d_rgba8`] and friends.
//! * [`AssetKind::ColorLut`]: what the Apply LUT filter reads. A `.cube` file becomes [`Asset::Cube`] as above; a
//!   picture is cut into a 64 cubed table the way OBS does it ([`png_lut_to_cube`]) and becomes an [`Asset::Cube`]
//!   too, so a picture that is not a table fails to load with a message instead of reaching the render thread.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows::core::HSTRING;
use windows::Win32::Foundation::GENERIC_READ;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11ShaderResourceView, D3D11_BIND_SHADER_RESOURCE, D3D11_SUBRESOURCE_DATA,
    D3D11_TEXTURE2D_DESC, D3D11_TEXTURE3D_DESC, D3D11_USAGE_IMMUTABLE,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_WICPixelFormat32bppRGBA, IWICImagingFactory,
    WICBitmapDitherTypeNone, WICBitmapPaletteTypeCustom, WICDecodeMetadataCacheOnDemand,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};

use super::color::srgb_to_linear;
use super::FilterError;
use crate::gpu::{GpuDevice, GpuError, GpuResult};

/// Largest picture, per side. WIC would decode more; a mask that big is a mistake and costs a quarter gigabyte.
pub const MAX_IMAGE_DIM: u32 = 8192;
/// Largest `LUT_3D_SIZE` (entries per axis). 256 cubed RGBA16F is 134 MB.
pub const MAX_LUT_3D: u32 = 256;
/// Largest `LUT_1D_SIZE`.
pub const MAX_LUT_1D: u32 = 65_536;
/// A `.cube` file bigger than this is refused before it is read (a full 256 cubed table is about 470 MB of text).
pub const MAX_CUBE_FILE_BYTES: u64 = 512 * 1024 * 1024;
/// Direct3D 11 cannot make a 2D texture wider than this, so a 1D table above it cannot be uploaded.
pub const MAX_TEXTURE_SIDE: u32 = 16_384;

// ---------------------------------------------------------------------------------------------------------------
// Types

/// What a filter asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AssetKind {
    /// A picture: PNG, JPEG, BMP or GIF.
    Image,
    /// A colour lookup table: a `.cube` file, or a picture (`.png`) holding one.
    Lut,
    /// A colour lookup table ready to use: a `.cube` file or a picture holding a 64 cubed table (see
    /// [`png_lut_to_cube`]), always loaded as [`Asset::Cube`]. A picture of any other layout fails to load.
    ColorLut,
}

/// A loaded asset. Cheap to share: hold the `Arc` the handle gives you.
pub enum Asset {
    Image(ImageAsset),
    Cube(CubeAsset),
    LutImage(DecodedImage),
}

impl Asset {
    /// The shader resource view of an asset that has one (not [`Asset::LutImage`]).
    pub fn srv(&self) -> Option<&ID3D11ShaderResourceView> {
        match self {
            Asset::Image(i) => Some(&i.srv),
            Asset::Cube(c) => Some(&c.srv),
            Asset::LutImage(_) => None,
        }
    }
}

/// A picture on the GPU: `R8G8B8A8` (typeless, so it has two views), straight alpha, top row first.
pub struct ImageAsset {
    pub width: u32,
    pub height: u32,
    /// The codes as stored (`R8G8B8A8_UNORM`).
    pub srv: ID3D11ShaderResourceView,
    /// The same texels read in linear light (`R8G8B8A8_UNORM_SRGB`), for a shader that works in linear light.
    pub srv_linear: ID3D11ShaderResourceView,
}

/// A `.cube` table and the texture made from it with [`CubeLut::create_texture`]`(gpu, true)`, which is what OBS
/// uploads: the table values converted from sRGB to linear, RGBA16F. 3D tables are `Texture3D`s (size cubed), 1D
/// tables are `Texture2D`s (size x 1).
pub struct CubeAsset {
    pub lut: CubeLut,
    pub srv: ID3D11ShaderResourceView,
}

/// Decoded pixels: `width * height * 4` bytes, R G B A, straight (not premultiplied) alpha, the top row first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Where an asset request stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AssetState {
    /// No file requested (the path is empty).
    None,
    /// Queued or being read.
    Loading,
    Ready,
    /// The message of the `could not load ...` error.
    Failed(String),
}

// ---------------------------------------------------------------------------------------------------------------
// Paths

/// The path a filter gave, without the blanks and quotation marks a hand-edited preset may carry.
fn clean(path: &str) -> &str {
    path.trim().trim_matches('"').trim()
}

/// The cache key of a path: `/` and `\` alike, `.` and `..` folded away, the `\\?\` prefix dropped, lower case
/// (Windows paths do not care about case). Purely textual: it never touches the disk, so it is safe on the render
/// thread.
pub fn normalize(path: &str) -> String {
    let p = clean(path).replace('/', "\\");
    let p = p.strip_prefix("\\\\?\\").unwrap_or(&p);
    let unc = p.starts_with("\\\\");
    let mut parts: Vec<&str> = Vec::new();
    for c in p.split('\\') {
        match c {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|l| *l != ".." && !l.ends_with(':')) {
                    parts.pop();
                } else if !parts.last().is_some_and(|l| l.ends_with(':')) {
                    parts.push("..");
                }
            }
            c => parts.push(c),
        }
    }
    let joined = parts.join("\\").to_lowercase();
    if unc {
        format!("\\\\{joined}")
    } else {
        joined
    }
}

/// The file name for messages.
fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// Modification time and size; `None` when the file cannot be looked at.
type Stamp = Option<(SystemTime, u64)>;

fn stamp_of(path: &Path) -> Stamp {
    let m = std::fs::metadata(path).ok()?;
    Some((m.modified().unwrap_or(UNIX_EPOCH), m.len()))
}

/// The reader's message for a file that is not there, a directory, empty, and so on.
fn check_file(path: &Path) -> Result<u64, String> {
    match std::fs::metadata(path) {
        Ok(m) if m.is_dir() => Err("this is a folder, not a file".into()),
        Ok(m) if m.len() == 0 => Err("the file is empty".into()),
        Ok(m) => Ok(m.len()),
        Err(e) => Err(match e.kind() {
            std::io::ErrorKind::NotFound => "file not found".into(),
            std::io::ErrorKind::PermissionDenied => "access denied".into(),
            _ => e.to_string(),
        }),
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Half floats

/// `f32` to IEEE half bits, rounded to nearest even; overflow gives infinity, NaN stays NaN.
pub fn f32_to_f16(f: f32) -> u16 {
    let x = f.to_bits();
    let sign = ((x >> 16) & 0x8000) as u16;
    let exp = ((x >> 23) & 0xff) as i32;
    let man = x & 0x007f_ffff;
    if exp == 0xff {
        return sign | 0x7c00 | if man != 0 { 0x200 } else { 0 };
    }
    let e = exp - 127 + 15;
    if e >= 0x1f {
        return sign | 0x7c00;
    }
    if e <= 0 {
        if e < -10 {
            return sign;
        }
        let m = man | 0x0080_0000;
        let shift = (14 - e) as u32;
        let mut h = (m >> shift) as u16;
        let rem = m & ((1 << shift) - 1);
        let mid = 1 << (shift - 1);
        if rem > mid || (rem == mid && h & 1 == 1) {
            h += 1;
        }
        return sign | h;
    }
    let mut h = (((e as u32) << 10) | (man >> 13)) as u16;
    let rem = man & 0x1fff;
    if rem > 0x1000 || (rem == 0x1000 && h & 1 == 1) {
        // A carry out of the mantissa raises the exponent, which is the right result.
        h += 1;
    }
    sign | h
}

// ---------------------------------------------------------------------------------------------------------------
// .cube

/// One dimension of a table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LutDim {
    OneD,
    ThreeD,
}

/// A parsed `.cube` file.
///
/// `data` holds the entries in file order, as the file states them (nothing converted): for a 1D table entry `i` is
/// the output for input `i / (size - 1)`; for a 3D table entry `x + y * size + z * size * size` belongs to the red
/// index `x`, green `y`, blue `z` (red changes fastest), which is also the memory order of a `Texture3D`.
/// Inputs run from `domain_min` to `domain_max` per channel.
#[derive(Clone, Debug, PartialEq)]
pub struct CubeLut {
    pub dim: LutDim,
    pub size: u32,
    pub domain_min: [f32; 3],
    pub domain_max: [f32; 3],
    pub data: Vec<[f32; 3]>,
}

impl CubeLut {
    /// Entries a table of this shape has.
    pub fn entries(dim: LutDim, size: u32) -> u64 {
        match dim {
            LutDim::OneD => size as u64,
            LutDim::ThreeD => (size as u64).pow(3),
        }
    }

    /// The table as RGBA16F texels (half bits, alpha 1), in `data`'s order. `to_linear` applies the sRGB to linear
    /// curve to each value first, as OBS does when it uploads a table.
    pub fn rgba16f(&self, to_linear: bool) -> Vec<u16> {
        let one = f32_to_f16(1.0);
        let mut out = Vec::with_capacity(self.data.len() * 4);
        for rgb in &self.data {
            for v in rgb {
                out.push(f32_to_f16(if to_linear { srgb_to_linear(*v) } else { *v }));
            }
            out.push(one);
        }
        out
    }

    /// Upload the table: a `Texture3D` (`size` cubed) for a 3D table, a `Texture2D` (`size` x 1) for a 1D table,
    /// both `R16G16B16A16_FLOAT`, immutable, shader resource only. A 1D table longer than [`MAX_TEXTURE_SIDE`]
    /// cannot be uploaded and gives an error.
    pub fn create_texture(
        &self,
        gpu: &GpuDevice,
        to_linear: bool,
    ) -> GpuResult<ID3D11ShaderResourceView> {
        let texels = self.rgba16f(to_linear);
        let bytes = u16_bytes(&texels);
        match self.dim {
            LutDim::ThreeD => create_texture3d(gpu, self.size, DXGI_FORMAT_R16G16B16A16_FLOAT, 8, &bytes),
            LutDim::OneD => {
                if self.size > MAX_TEXTURE_SIDE {
                    return Err(GpuError::Unsupported(format!(
                        "a 1D table of {} entries (the limit is {MAX_TEXTURE_SIDE})",
                        self.size
                    )));
                }
                create_texture2d(gpu, self.size, 1, DXGI_FORMAT_R16G16B16A16_FLOAT, 8, &bytes)
            }
        }
    }
}

fn u16_bytes(v: &[u16]) -> Vec<u8> {
    v.iter().flat_map(|h| h.to_le_bytes()).collect()
}

fn number_start(token: &str) -> bool {
    token
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_digit() || matches!(c, '-' | '+' | '.'))
}

/// Three finite floats from the tokens.
fn three_floats<'a>(
    tokens: impl Iterator<Item = &'a str>,
    line: usize,
    what: &str,
) -> Result<[f32; 3], String> {
    let mut v = [0.0f32; 3];
    let mut n = 0;
    for t in tokens {
        if n == 3 {
            return Err(format!("line {line}: {what} has more than three numbers"));
        }
        let x: f32 = t
            .parse()
            .map_err(|_| format!("line {line}: \"{t}\" is not a number"))?;
        if !x.is_finite() {
            return Err(format!("line {line}: \"{t}\" is not a finite number"));
        }
        v[n] = x;
        n += 1;
    }
    if n != 3 {
        return Err(format!("line {line}: {what} needs three numbers"));
    }
    Ok(v)
}

/// A table size: a whole number from 1 to `max`.
fn table_size(token: Option<&str>, line: usize, key: &str, max: u32) -> Result<u32, String> {
    let t = token.ok_or_else(|| format!("line {line}: {key} needs a number"))?;
    let n: u64 = t
        .parse()
        .map_err(|_| format!("line {line}: \"{t}\" is not a valid {key}"))?;
    if n == 0 {
        return Err(format!("line {line}: {key} is 0"));
    }
    if n > max as u64 {
        return Err(format!("line {line}: {key} {n} is above the limit of {max}"));
    }
    Ok(n as u32)
}

/// Read a `.cube` table the way OBS's `load_cube_file` does: lines before the first line of three numbers are
/// metadata (`LUT_1D_SIZE`, `LUT_3D_SIZE`, `DOMAIN_MIN`, `DOMAIN_MAX`; `TITLE`, comments starting with `#` and
/// other keywords are skipped); from there on every line holds the three numbers of one entry. A 1D size wins over
/// a 3D size, as in OBS. Stricter than OBS in one place: the number of entries must match the size exactly (OBS
/// ignores extra lines), and a line that is not an entry after the first entry is an error.
///
/// Sizes are checked against [`MAX_LUT_3D`] / [`MAX_LUT_1D`] before anything is allocated, and the entry list never
/// grows past what the file really contains, so a header that claims a huge table in a small file is cheap to
/// refuse. Never panics.
pub fn parse_cube(text: &str) -> Result<CubeLut, String> {
    if text.trim().is_empty() {
        return Err("the file is empty".into());
    }
    let mut size_1d: Option<u32> = None;
    let mut size_3d: Option<u32> = None;
    let mut domain_min = [0.0f32; 3];
    let mut domain_max = [1.0f32; 3];
    let mut entries: Vec<[f32; 3]> = Vec::new();
    let mut shape: Option<(LutDim, u32, u64)> = None;

    for (i, raw) in text.lines().enumerate() {
        let n = i + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut tokens = line.split_whitespace();
        let first = tokens.next().unwrap_or("");
        if number_start(first) {
            // An entry. The first one closes the metadata.
            let (_, _, expected) = match shape {
                Some(s) => s,
                None => {
                    let s = if let Some(s) = size_1d {
                        (LutDim::OneD, s, s as u64)
                    } else if let Some(s) = size_3d {
                        (LutDim::ThreeD, s, (s as u64).pow(3))
                    } else {
                        return Err(format!(
                            "line {n}: data starts but LUT_1D_SIZE or LUT_3D_SIZE is missing"
                        ));
                    };
                    for k in 0..3 {
                        if domain_min[k] >= domain_max[k] {
                            return Err(format!(
                                "invalid DOMAIN_MIN / DOMAIN_MAX: {:?} is not below {:?}",
                                domain_min, domain_max
                            ));
                        }
                    }
                    // Never trust the header for the allocation: no entry is shorter than two bytes.
                    entries.reserve((s.2).min(text.len() as u64 / 2 + 1) as usize);
                    shape = Some(s);
                    s
                }
            };
            if entries.len() as u64 >= expected {
                return Err(format!("line {n}: more entries than the {expected} the size says"));
            }
            entries.push(three_floats(std::iter::once(first).chain(tokens), n, "an entry")?);
            continue;
        }
        if shape.is_some() {
            return Err(format!("line {n}: unexpected text among the entries: \"{line}\""));
        }
        match first {
            "LUT_1D_SIZE" => size_1d = Some(table_size(tokens.next(), n, first, MAX_LUT_1D)?),
            "LUT_3D_SIZE" => size_3d = Some(table_size(tokens.next(), n, first, MAX_LUT_3D)?),
            "DOMAIN_MIN" => domain_min = three_floats(tokens, n, first)?,
            "DOMAIN_MAX" => domain_max = three_floats(tokens, n, first)?,
            // TITLE and any other keyword: OBS ignores what it does not know.
            _ => {}
        }
    }
    let Some((dim, size, expected)) = shape else {
        return Err(if size_1d.is_none() && size_3d.is_none() {
            "missing LUT_1D_SIZE or LUT_3D_SIZE".into()
        } else {
            "the file has no entries".into()
        });
    };
    if entries.len() as u64 != expected {
        return Err(format!(
            "expected {expected} entries for a size of {size}, found {}",
            entries.len()
        ));
    }
    Ok(CubeLut {
        dim,
        size,
        domain_min,
        domain_max,
        data: entries,
    })
}

// ---------------------------------------------------------------------------------------------------------------
// Images

thread_local! {
    /// COM for WIC on this thread (multithreaded apartment). A thread that is already in another apartment keeps
    /// it: WIC works in both.
    static COM: () = {
        // SAFETY: plain call; the result only says whether this thread was initialized before.
        unsafe { let _ = CoInitializeEx(None, COINIT_MULTITHREADED); }
    };
}

fn ensure_com() {
    COM.with(|_| {});
}

/// WINCODEC_ERR_* and Win32 codes WIC reports for the files that people feed it, as sentences.
fn wic_message(e: &windows::core::Error) -> String {
    let code = e.code().0 as u32;
    match code {
        0x8898_2F50 | 0x8898_2F07 => "not a supported picture (PNG, JPEG, BMP and GIF work)".into(),
        0x8898_2F60 | 0x8898_2F61 | 0x8898_2F70 | 0x8898_2F72 | 0x8898_2F78 => {
            "the picture is damaged or cut short".into()
        }
        0x8007_0002 | 0x8007_0003 => "file not found".into(),
        0x8007_0005 => "access denied".into(),
        _ => format!("{} (0x{code:08X})", e.message().trim()),
    }
}

/// WIC decodes a PNG, JPEG, GIF or BMP that was cut short without a word (the missing rows come out as zeros), so
/// look for the end of the file's own structure: PNG's `IEND` chunk, JPEG's end-of-image marker, GIF's trailer, the
/// size a BMP header states. Other content is left to the decoder.
fn check_complete(path: &Path, len: u64) -> Result<(), String> {
    use std::io::{Read, Seek, SeekFrom};
    let io = |e: std::io::Error| e.to_string();
    let mut f = std::fs::File::open(path).map_err(io)?;
    let mut head = [0u8; 8];
    let n = f.read(&mut head).map_err(io)?;
    let head = &head[..n];
    let tail_len = len.min(4096);
    let mut tail = vec![0u8; tail_len as usize];
    f.seek(SeekFrom::Start(len - tail_len)).map_err(io)?;
    f.read_exact(&mut tail).map_err(io)?;
    let complete = if head.starts_with(&[0x89, b'P', b'N', b'G']) {
        tail.windows(4).any(|w| w == b"IEND")
    } else if head.starts_with(&[0xFF, 0xD8]) {
        tail.windows(2).any(|w| w == [0xFF, 0xD9])
    } else if head.starts_with(b"GIF8") {
        tail.iter().rev().find(|b| **b != 0) == Some(&0x3B)
    } else if head.starts_with(b"BM") && head.len() >= 6 {
        let declared = u32::from_le_bytes([head[2], head[3], head[4], head[5]]) as u64;
        declared == 0 || len >= declared
    } else {
        true
    };
    if complete {
        Ok(())
    } else {
        Err("the picture is damaged or cut short".into())
    }
}

/// Decode a picture with WIC: PNG, JPEG, BMP and GIF (first frame). The result is straight-alpha RGBA, the top row
/// first; EXIF orientation is ignored, as in OBS. Larger than [`MAX_IMAGE_DIM`] per side is refused before the pixels
/// are read. The error is a sentence for the card.
pub fn decode_image(path: &Path) -> Result<DecodedImage, String> {
    ensure_com();
    let len = check_file(path)?;
    check_complete(path, len)?;
    let name = HSTRING::from(path.as_os_str());
    // SAFETY: plain COM calls; every buffer and pointer lives through its call.
    unsafe {
        let factory: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)
                .map_err(|e| wic_message(&e))?;
        let decoder = factory
            .CreateDecoderFromFilename(&name, None, GENERIC_READ, WICDecodeMetadataCacheOnDemand)
            .map_err(|e| wic_message(&e))?;
        let frame = decoder.GetFrame(0).map_err(|e| wic_message(&e))?;
        let (mut w, mut h) = (0u32, 0u32);
        frame.GetSize(&mut w, &mut h).map_err(|e| wic_message(&e))?;
        if w == 0 || h == 0 {
            return Err("the picture has no pixels".into());
        }
        if w > MAX_IMAGE_DIM || h > MAX_IMAGE_DIM {
            return Err(format!(
                "the picture is {w}x{h}; the largest size is {MAX_IMAGE_DIM}x{MAX_IMAGE_DIM}"
            ));
        }
        let converter = factory.CreateFormatConverter().map_err(|e| wic_message(&e))?;
        converter
            .Initialize(
                &frame,
                &GUID_WICPixelFormat32bppRGBA,
                WICBitmapDitherTypeNone,
                None,
                0.0,
                WICBitmapPaletteTypeCustom,
            )
            .map_err(|e| wic_message(&e))?;
        let mut rgba = vec![0u8; w as usize * h as usize * 4];
        converter
            .CopyPixels(std::ptr::null(), w * 4, &mut rgba)
            .map_err(|e| wic_message(&e))?;
        Ok(DecodedImage {
            width: w,
            height: h,
            rgba,
        })
    }
}

// ---------------------------------------------------------------------------------------------------------------
// GPU textures

/// An immutable `Texture2D` with one mip level, shader resource only. `texel` is the bytes per texel.
pub fn create_texture2d(
    gpu: &GpuDevice,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
    texel: usize,
    data: &[u8],
) -> GpuResult<ID3D11ShaderResourceView> {
    if width == 0 || height == 0 || data.len() != width as usize * height as usize * texel {
        return Err(GpuError::Invalid(format!(
            "{} bytes for a {width}x{height} texture of {texel}-byte texels",
            data.len()
        )));
    }
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_IMMUTABLE,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let init = D3D11_SUBRESOURCE_DATA {
        pSysMem: data.as_ptr() as *const _,
        SysMemPitch: (width as usize * texel) as u32,
        SysMemSlicePitch: 0,
    };
    let mut tex = None;
    let mut srv = None;
    // SAFETY: `desc`, `init` and `data` live through the calls; the data length was checked against the pitch.
    unsafe {
        gpu.device.CreateTexture2D(&desc, Some(&init), Some(&mut tex))?;
        let tex = tex.ok_or_else(null_pointer)?;
        gpu.device.CreateShaderResourceView(&tex, None, Some(&mut srv))?;
    }
    srv.ok_or_else(null_pointer)
}

/// The error for a COM call that succeeded and returned no object.
fn null_pointer() -> GpuError {
    windows::core::Error::from(windows::Win32::Foundation::E_POINTER).into()
}

/// An immutable cubic `Texture3D` (`size` texels per side, at most 256), one mip level, shader resource only.
/// `data` is `size` cubed texels, the red index (x) changing fastest, then green (y), then blue (z).
pub fn create_texture3d(
    gpu: &GpuDevice,
    size: u32,
    format: DXGI_FORMAT,
    texel: usize,
    data: &[u8],
) -> GpuResult<ID3D11ShaderResourceView> {
    if size == 0 || size > MAX_LUT_3D || data.len() != (size as usize).pow(3) * texel {
        return Err(GpuError::Invalid(format!(
            "{} bytes for a {size}x{size}x{size} texture of {texel}-byte texels",
            data.len()
        )));
    }
    let desc = D3D11_TEXTURE3D_DESC {
        Width: size,
        Height: size,
        Depth: size,
        MipLevels: 1,
        Format: format,
        Usage: D3D11_USAGE_IMMUTABLE,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let init = D3D11_SUBRESOURCE_DATA {
        pSysMem: data.as_ptr() as *const _,
        SysMemPitch: (size as usize * texel) as u32,
        SysMemSlicePitch: (size as usize * size as usize * texel) as u32,
    };
    let mut tex = None;
    let mut srv = None;
    // SAFETY: as in `create_texture2d`.
    unsafe {
        gpu.device.CreateTexture3D(&desc, Some(&init), Some(&mut tex))?;
        let tex = tex.ok_or_else(null_pointer)?;
        gpu.device.CreateShaderResourceView(&tex, None, Some(&mut srv))?;
    }
    srv.ok_or_else(null_pointer)
}

/// A picture as an `R8G8B8A8_UNORM` texture.
pub fn create_texture_rgba8(
    gpu: &GpuDevice,
    width: u32,
    height: u32,
    rgba: &[u8],
) -> GpuResult<ID3D11ShaderResourceView> {
    create_texture2d(gpu, width, height, DXGI_FORMAT_R8G8B8A8_UNORM, 4, rgba)
}

/// A picture as an `R8G8B8A8_TYPELESS` texture with two views: the codes as stored (`UNORM`) and the same texels read
/// in linear light (`UNORM_SRGB`: the hardware decodes on read, alpha untouched). Returns `(codes, linear)`.
pub fn create_image_rgba8(
    gpu: &GpuDevice,
    width: u32,
    height: u32,
    rgba: &[u8],
) -> GpuResult<(ID3D11ShaderResourceView, ID3D11ShaderResourceView)> {
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_R8G8B8A8_TYPELESS, DXGI_FORMAT_R8G8B8A8_UNORM_SRGB};
    if width == 0 || height == 0 || rgba.len() != width as usize * height as usize * 4 {
        return Err(GpuError::Invalid(format!(
            "{} bytes for a {width}x{height} texture of 4-byte texels",
            rgba.len()
        )));
    }
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_R8G8B8A8_TYPELESS,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_IMMUTABLE,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let init = D3D11_SUBRESOURCE_DATA {
        pSysMem: rgba.as_ptr() as *const _,
        SysMemPitch: width * 4,
        SysMemSlicePitch: 0,
    };
    let mut tex = None;
    // SAFETY: `desc`, `init` and `rgba` live through the call; the data length was checked against the pitch.
    unsafe { gpu.device.CreateTexture2D(&desc, Some(&init), Some(&mut tex))? };
    let tex = tex.ok_or_else(null_pointer)?;
    Ok((
        gpu.create_srv(&tex, DXGI_FORMAT_R8G8B8A8_UNORM)?,
        gpu.create_srv(&tex, DXGI_FORMAT_R8G8B8A8_UNORM_SRGB)?,
    ))
}

/// A cubic 3D texture of `R8G8B8A8_UNORM` texels (a `.png` table cut into `size` slices, for example).
pub fn create_texture3d_rgba8(
    gpu: &GpuDevice,
    size: u32,
    rgba: &[u8],
) -> GpuResult<ID3D11ShaderResourceView> {
    create_texture3d(gpu, size, DXGI_FORMAT_R8G8B8A8_UNORM, 4, rgba)
}

/// A cubic 3D texture of `R16G16B16A16_FLOAT` texels given as half bits (see [`f32_to_f16`]).
pub fn create_texture3d_rgba16f(
    gpu: &GpuDevice,
    size: u32,
    halves: &[u16],
) -> GpuResult<ID3D11ShaderResourceView> {
    create_texture3d(gpu, size, DXGI_FORMAT_R16G16B16A16_FLOAT, 8, &u16_bytes(halves))
}

// ---------------------------------------------------------------------------------------------------------------
// Picture tables

/// Entries per axis of a table held in a picture (`LUT_WIDTH` in OBS's color-grade-filter.c).
pub const PNG_LUT_SIZE: u32 = 64;

/// Cut a picture into the 64 cubed table it holds, the way OBS's `make_clut_texture_png` does.
///
/// OBS accepts a picture whose width and height are both multiples of 64 and whose pixel count is 64 cubed
/// (262144): 512 x 512 in practice. The picture is a grid of 64 x 64 tiles, `width / 64` tiles across and
/// `height / 64` down; tile `z` (the blue index) sits at column `z % (width / 64)` and row `z / (height / 64)`, and
/// within a tile the pixel at column `x`, row `y` is the entry for red `x`, green `y`. For 512 x 512 that is the usual
/// 8 x 8 grid, left to right, top to bottom. For the other shapes that pass OBS's size test (for example 1024 x 256)
/// OBS's own placement formula reads outside the picture, or repeats tiles: the same formula is used here, and a
/// picture it would read outside of is refused.
///
/// The table values are the pixels' code values divided by 255 (the picture is sRGB, as OBS samples it through an
/// sRGB view); alpha is ignored. The domain is 0 to 1, so the lookup constants are OBS's PNG ones
/// (`scale = 1 - 1/64`, `offset = 0.5/64`).
pub fn png_lut_to_cube(img: &DecodedImage) -> Result<CubeLut, String> {
    let n = PNG_LUT_SIZE;
    let (w, h) = (img.width, img.height);
    let refused = || {
        format!("the picture is {w}x{h}; a LUT picture is 512x512 pixels holding a {n}x{n}x{n} table")
    };
    if w % n != 0 || h % n != 0 || w as u64 * h as u64 != (n as u64).pow(3) {
        return Err(refused());
    }
    if img.rgba.len() != w as usize * h as usize * 4 {
        return Err("the picture is damaged".into());
    }
    let (macro_w, macro_h) = (w / n, h / n);
    let mut data = Vec::with_capacity((n as usize).pow(3));
    for z in 0..n {
        let z_x = (z % macro_w) * n;
        let z_y = (z / macro_h) * n;
        if z_x + n > w || z_y + n > h {
            return Err(refused());
        }
        for y in 0..n {
            let row = w as usize * (z_y + y) as usize;
            for x in 0..n {
                let i = 4 * (row + (z_x + x) as usize);
                let px = &img.rgba[i..i + 3];
                data.push([px[0], px[1], px[2]].map(|v| v as f32 / 255.0));
            }
        }
    }
    Ok(CubeLut {
        dim: LutDim::ThreeD,
        size: n,
        domain_min: [0.0; 3],
        domain_max: [1.0; 3],
        data,
    })
}

// ---------------------------------------------------------------------------------------------------------------
// Loading one file

fn is_cube(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.to_string_lossy().eq_ignore_ascii_case("cube"))
}

/// Read and decode one file and upload it. Runs on the loader thread (or in a test). The error is the reason only.
pub fn load_asset(gpu: &GpuDevice, kind: AssetKind, path: &Path) -> Result<Asset, String> {
    match kind {
        AssetKind::Image => {
            let img = decode_image(path)?;
            let (srv, srv_linear) = create_image_rgba8(gpu, img.width, img.height, &img.rgba)
                .map_err(|e| e.to_string())?;
            Ok(Asset::Image(ImageAsset {
                width: img.width,
                height: img.height,
                srv,
                srv_linear,
            }))
        }
        AssetKind::Lut | AssetKind::ColorLut if is_cube(path) => {
            let len = check_file(path)?;
            if len > MAX_CUBE_FILE_BYTES {
                return Err(format!(
                    "the file is {} MB; the limit is {} MB",
                    len / (1024 * 1024),
                    MAX_CUBE_FILE_BYTES / (1024 * 1024)
                ));
            }
            let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
            let lut = parse_cube(&String::from_utf8_lossy(&bytes))?;
            drop(bytes);
            let srv = lut.create_texture(gpu, true).map_err(|e| e.to_string())?;
            Ok(Asset::Cube(CubeAsset { lut, srv }))
        }
        AssetKind::Lut => Ok(Asset::LutImage(decode_image(path)?)),
        AssetKind::ColorLut => {
            let lut = png_lut_to_cube(&decode_image(path)?)?;
            let srv = lut.create_texture(gpu, true).map_err(|e| e.to_string())?;
            Ok(Asset::Cube(CubeAsset { lut, srv }))
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// The service

/// Where the service sends [`FilterError`]s (an empty `message` clears the card's line). Called on the loader thread
/// or on the thread that made a request, never while the service holds a lock.
pub type AssetEvents = Arc<dyn Fn(FilterError) + Send + Sync>;

type Key = (AssetKind, String);

enum Status {
    Loading,
    Ready(Arc<Asset>),
    Failed(String),
}

struct Entry {
    path: PathBuf,
    status: Status,
    /// What the file looked like when the current status was loaded.
    stamp: Stamp,
    owners: Vec<u64>,
    /// In the queue.
    queued: bool,
    /// The loader is working on it.
    loading: bool,
}

/// What the owner's card has been told.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Told {
    Nothing,
    /// An empty message went out: no error.
    Clean,
    Error,
}

struct Owner {
    /// The filter's id (the card's id).
    id: String,
    key: Option<Key>,
    told: Told,
}

#[derive(Default)]
struct State {
    entries: HashMap<Key, Entry>,
    owners: HashMap<u64, Owner>,
    queue: VecDeque<Key>,
    shutdown: bool,
}

impl State {
    /// Take `owner` off the entry it uses; an entry nobody uses goes away.
    fn detach(&mut self, owner: u64) {
        let Some(o) = self.owners.get_mut(&owner) else {
            return;
        };
        if let Some(key) = o.key.take() {
            if let Some(e) = self.entries.get_mut(&key) {
                e.owners.retain(|x| *x != owner);
                if e.owners.is_empty() {
                    self.entries.remove(&key);
                }
            }
        }
    }
}

struct Inner {
    gpu: Arc<GpuDevice>,
    state: Mutex<State>,
    wake: Condvar,
    emit: AssetEvents,
    scan_every: Duration,
    /// Files read so far (successful or not), for tests and diagnostics.
    loads: AtomicU64,
    next_owner: AtomicU64,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn clear_event(id: &str) -> FilterError {
    FilterError {
        id: id.to_string(),
        message: String::new(),
    }
}

struct Job {
    key: Key,
    path: PathBuf,
}

impl Inner {
    fn flush(&self, events: Vec<FilterError>) {
        for e in events {
            (self.emit)(e);
        }
    }

    /// `AssetHandle::request`: returns the events to send once the lock is released.
    fn request(&self, owner: u64, kind: AssetKind, raw: &str) -> Vec<FilterError> {
        let mut events = Vec::new();
        let mut st = lock(&self.state);
        st.detach(owner);
        let path = clean(raw);
        let State {
            entries,
            owners,
            queue,
            ..
        } = &mut *st;
        let Some(o) = owners.get_mut(&owner) else {
            return events;
        };
        if path.is_empty() {
            if o.told == Told::Error {
                events.push(clear_event(&o.id));
                o.told = Told::Clean;
            }
            return events;
        }
        let key = (kind, normalize(path));
        let mut fresh = false;
        let e = entries.entry(key.clone()).or_insert_with(|| {
            fresh = true;
            Entry {
                path: PathBuf::from(path),
                status: Status::Loading,
                stamp: None,
                owners: Vec::new(),
                queued: true,
                loading: false,
            }
        });
        e.owners.push(owner);
        match &e.status {
            Status::Ready(_) => {
                if o.told != Told::Clean {
                    events.push(clear_event(&o.id));
                    o.told = Told::Clean;
                }
            }
            Status::Failed(m) => {
                events.push(FilterError {
                    id: o.id.clone(),
                    message: m.clone(),
                });
                o.told = Told::Error;
            }
            Status::Loading => {}
        }
        o.key = Some(key.clone());
        if fresh {
            queue.push_back(key);
        }
        drop(st);
        if fresh {
            self.wake.notify_one();
        }
        events
    }

    fn release(&self, owner: u64) {
        let mut st = lock(&self.state);
        st.detach(owner);
        st.owners.remove(&owner);
    }

    fn status_of(&self, owner: u64) -> AssetState {
        let st = lock(&self.state);
        let Some(key) = st.owners.get(&owner).and_then(|o| o.key.as_ref()) else {
            return AssetState::None;
        };
        match st.entries.get(key).map(|e| &e.status) {
            Some(Status::Ready(_)) => AssetState::Ready,
            Some(Status::Failed(m)) => AssetState::Failed(m.clone()),
            Some(Status::Loading) | None => AssetState::Loading,
        }
    }

    fn get(&self, owner: u64) -> Option<Arc<Asset>> {
        let st = lock(&self.state);
        let key = st.owners.get(&owner)?.key.as_ref()?;
        match &st.entries.get(key)?.status {
            Status::Ready(a) => Some(a.clone()),
            _ => None,
        }
    }

    /// Wait for the next file to load; looks for changed files once per `scan_every` meanwhile. `None` on shutdown.
    fn next_job(&self, next_scan: &mut Instant) -> Option<Job> {
        let mut st = lock(&self.state);
        loop {
            if st.shutdown {
                return None;
            }
            while let Some(key) = st.queue.pop_front() {
                if let Some(e) = st.entries.get_mut(&key) {
                    if e.queued {
                        e.queued = false;
                        e.loading = true;
                        return Some(Job {
                            path: e.path.clone(),
                            key,
                        });
                    }
                }
            }
            let now = Instant::now();
            if now >= *next_scan {
                let targets: Vec<(Key, PathBuf, Stamp)> = st
                    .entries
                    .iter()
                    .filter(|(_, e)| !e.queued && !e.loading)
                    .map(|(k, e)| (k.clone(), e.path.clone(), e.stamp))
                    .collect();
                drop(st);
                // The disk is looked at without the lock: a slow share must not stall a filter's request.
                let changed: Vec<(Key, Stamp)> = targets
                    .into_iter()
                    .filter(|(_, p, was)| stamp_of(p) != *was)
                    .map(|(k, _, was)| (k, was))
                    .collect();
                st = lock(&self.state);
                for (key, was) in changed {
                    if let Some(e) = st.entries.get_mut(&key) {
                        if e.stamp == was && !e.queued && !e.loading {
                            e.queued = true;
                            st.queue.push_back(key);
                        }
                    }
                }
                *next_scan = Instant::now() + self.scan_every;
                continue;
            }
            let wait = *next_scan - now;
            st = self
                .wake
                .wait_timeout(st, wait)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// Store a finished load and tell the owners.
    fn finish(&self, key: &Key, stamp: Stamp, result: Result<Arc<Asset>, String>) -> Vec<FilterError> {
        let mut events = Vec::new();
        let mut st = lock(&self.state);
        let State { entries, owners, .. } = &mut *st;
        let Some(e) = entries.get_mut(key) else {
            return events;
        };
        e.loading = false;
        e.stamp = stamp;
        match result {
            Ok(a) => {
                e.status = Status::Ready(a);
                for id in &e.owners {
                    if let Some(o) = owners.get_mut(id) {
                        if o.told != Told::Clean {
                            events.push(clear_event(&o.id));
                            o.told = Told::Clean;
                        }
                    }
                }
            }
            Err(m) => {
                for id in &e.owners {
                    if let Some(o) = owners.get_mut(id) {
                        events.push(FilterError {
                            id: o.id.clone(),
                            message: m.clone(),
                        });
                        o.told = Told::Error;
                    }
                }
                e.status = Status::Failed(m);
            }
        }
        events
    }
}

fn loader_thread(inner: Arc<Inner>) {
    ensure_com();
    let mut next_scan = Instant::now() + inner.scan_every;
    while let Some(job) = inner.next_job(&mut next_scan) {
        inner.loads.fetch_add(1, Ordering::Relaxed);
        // Taken before the read: a change while the file is read shows up as a different stamp at the next look.
        let stamp = stamp_of(&job.path);
        let kind = job.key.0;
        let loaded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            load_asset(&inner.gpu, kind, &job.path)
        }))
        .unwrap_or_else(|_| Err("internal error while reading the file".into()));
        let result = loaded
            .map(Arc::new)
            .map_err(|reason| format!("could not load {}: {reason}", display_name(&job.path)));
        let events = inner.finish(&job.key, stamp, result);
        inner.flush(events);
    }
}

/// The asset service: one loader thread, the cache and the change watch. The engine owns it; dropping it stops the
/// thread. Filters reach it through [`AssetClient`] / [`AssetHandle`].
pub struct Assets {
    inner: Arc<Inner>,
    thread: Option<JoinHandle<()>>,
}

impl Assets {
    /// The service on `gpu`, with the one-second change check of OBS's mask filter. `events` receives the load
    /// failures and the clears (see the module documentation).
    pub fn new(gpu: Arc<GpuDevice>, events: AssetEvents) -> Assets {
        Assets::with_scan_interval(gpu, events, Duration::from_secs(1))
    }

    /// [`Assets::new`] with another interval between looks at the files (tests shorten it).
    pub fn with_scan_interval(gpu: Arc<GpuDevice>, events: AssetEvents, scan_every: Duration) -> Assets {
        let inner = Arc::new(Inner {
            gpu,
            state: Mutex::new(State::default()),
            wake: Condvar::new(),
            emit: events,
            scan_every,
            loads: AtomicU64::new(0),
            next_owner: AtomicU64::new(1),
        });
        let worker = inner.clone();
        let thread = std::thread::Builder::new()
            .name("asset loader".into())
            .spawn(move || loader_thread(worker))
            .ok();
        Assets { inner, thread }
    }

    /// A cloneable way to make handles; give it to whoever builds filters (the [`super::Chain`]).
    pub fn client(&self) -> AssetClient {
        AssetClient(self.inner.clone())
    }

    /// Files read so far, successfully or not (a reload counts).
    pub fn load_count(&self) -> u64 {
        self.inner.loads.load(Ordering::Relaxed)
    }
}

impl Drop for Assets {
    fn drop(&mut self) {
        lock(&self.inner.state).shutdown = true;
        self.inner.wake.notify_all();
        if let Some(t) = self.thread.take() {
            // A loader that is inside the read of a file that hangs (a share that does not answer) must not hold up
            // whoever drops this (a quit, a rebuild of the device): it sees the flag when the read returns and ends by
            // itself, and keeps `Inner` (the device) alive until then.
            let until = Instant::now() + LOADER_JOIN_CAP;
            while !t.is_finished() && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(2));
            }
            if t.is_finished() {
                let _ = t.join();
            }
        }
    }
}

/// How long dropping [`Assets`] waits for the loader thread to end before it leaves the thread behind.
const LOADER_JOIN_CAP: Duration = Duration::from_secs(2);

/// Makes [`AssetHandle`]s. Cheap to clone; does not keep the loader thread alive.
#[derive(Clone)]
pub struct AssetClient(Arc<Inner>);

impl AssetClient {
    /// A handle for the filter with this id (the card's id: errors are reported under it).
    pub fn handle(&self, filter_id: &str) -> AssetHandle {
        let owner = self.0.next_owner.fetch_add(1, Ordering::Relaxed);
        lock(&self.0.state).owners.insert(
            owner,
            Owner {
                id: filter_id.to_string(),
                key: None,
                told: Told::Nothing,
            },
        );
        AssetHandle {
            inner: self.0.clone(),
            owner,
            requested: None,
        }
    }

    /// Files read so far, as [`Assets::load_count`].
    pub fn load_count(&self) -> u64 {
        self.0.loads.load(Ordering::Relaxed)
    }
}

/// One filter's view of the service: one file at a time. Dropping it lets go of the file.
pub struct AssetHandle {
    inner: Arc<Inner>,
    owner: u64,
    /// What the last `request` asked for, so a repeated request costs a string comparison and no lock.
    requested: Option<(AssetKind, String)>,
}

impl AssetHandle {
    /// Ask for a file (call it from `update`: it is cheap, takes no disk access and never waits). The same
    /// arguments again do nothing. A different path lets go of the old file and loads the new one; an empty path
    /// means "no asset" and is not an error.
    pub fn request(&mut self, kind: AssetKind, path: &str) {
        if let Some((k, p)) = &self.requested {
            if *k == kind && p == path {
                return;
            }
        }
        self.requested = Some((kind, path.to_string()));
        let events = self.inner.request(self.owner, kind, path);
        self.inner.flush(events);
    }

    /// The asset when it is loaded, else `None` (still loading, failed, or no path). Cheap enough for every frame.
    pub fn get(&self) -> Option<Arc<Asset>> {
        self.inner.get(self.owner)
    }

    /// Where the request stands.
    pub fn state(&self) -> AssetState {
        self.inner.status_of(self.owner)
    }
}

impl Drop for AssetHandle {
    fn drop(&mut self) {
        self.inner.release(self.owner);
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Tests

#[cfg(test)]
pub(crate) mod testgen {
    //! Writes test pictures with WIC's encoders and `.cube` tables as text, so a test can make the file it needs
    //! in a temporary folder. The `write_committed_assets` test (ignored) refreshes `tests/assets/`.

    use super::*;
    use windows::core::GUID;
    use windows::Win32::Foundation::GENERIC_WRITE;
    use windows::Win32::Graphics::Imaging::{
        GUID_ContainerFormatBmp, GUID_ContainerFormatGif, GUID_ContainerFormatJpeg,
        GUID_ContainerFormatPng, GUID_WICPixelFormat24bppBGR, GUID_WICPixelFormat32bppBGRA,
        GUID_WICPixelFormat8bppIndexed, WICBitmapEncoderNoCache,
    };

    /// A temporary folder that goes away with the value.
    pub struct TempDir(pub vcam_protocol::testdir::TestDir);

    impl TempDir {
        pub fn new(name: &str) -> TempDir {
            TempDir(vcam_protocol::testdir::TestDir::new(&format!("better-cam-test-{name}")))
        }
        pub fn join(&self, f: &str) -> PathBuf {
            self.0.join(f)
        }
    }

    /// The test picture: 32 x 16, four flat blocks of 16 x 8. Top left red, top right green, bottom left blue,
    /// bottom right yellow with alpha `alpha` (255 for the formats that have none). Asymmetric on purpose: a flip or
    /// a channel swap shows.
    pub fn picture(alpha: u8) -> (u32, u32, Vec<u8>) {
        let (w, h) = (32u32, 16u32);
        let mut px = Vec::new();
        for y in 0..h {
            for x in 0..w {
                px.extend_from_slice(&match (x < 16, y < 8) {
                    (true, true) => [255, 0, 0, 255],
                    (false, true) => [0, 255, 0, 255],
                    (true, false) => [0, 0, 255, 255],
                    (false, false) => [255, 255, 0, alpha],
                });
            }
        }
        (w, h, px)
    }

    fn encode(
        path: &Path,
        container: &GUID,
        format: GUID,
        (w, h): (u32, u32),
        stride: u32,
        pixels: &[u8],
        palette: Option<&[u32]>,
    ) {
        ensure_com();
        let name = HSTRING::from(path.as_os_str());
        // SAFETY: plain COM calls; buffers live through their calls.
        unsafe {
            let factory: IWICImagingFactory =
                CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).unwrap();
            let stream = factory.CreateStream().unwrap();
            stream.InitializeFromFilename(&name, GENERIC_WRITE.0).unwrap();
            let encoder = factory.CreateEncoder(container, std::ptr::null()).unwrap();
            encoder.Initialize(&stream, WICBitmapEncoderNoCache).unwrap();
            let mut frame = None;
            encoder.CreateNewFrame(&mut frame, std::ptr::null_mut()).unwrap();
            let frame = frame.unwrap();
            frame.Initialize(None).unwrap();
            frame.SetSize(w, h).unwrap();
            let mut fmt = format;
            frame.SetPixelFormat(&mut fmt).unwrap();
            assert_eq!(fmt, format, "the encoder changed the pixel format");
            if let Some(colors) = palette {
                let pal = factory.CreatePalette().unwrap();
                pal.InitializeCustom(colors).unwrap();
                frame.SetPalette(&pal).unwrap();
            }
            frame.WritePixels(h, stride, pixels).unwrap();
            frame.Commit().unwrap();
            encoder.Commit().unwrap();
        }
    }

    fn bgra(rgba: &[u8]) -> Vec<u8> {
        rgba.chunks(4).flat_map(|p| [p[2], p[1], p[0], p[3]]).collect()
    }

    fn bgr(rgba: &[u8]) -> Vec<u8> {
        rgba.chunks(4).flat_map(|p| [p[2], p[1], p[0]]).collect()
    }

    /// PNG with straight alpha.
    pub fn write_png(path: &Path, w: u32, h: u32, rgba: &[u8]) {
        encode(path, &GUID_ContainerFormatPng, GUID_WICPixelFormat32bppBGRA, (w, h), w * 4, &bgra(rgba), None);
    }

    /// JPEG (alpha dropped).
    pub fn write_jpeg(path: &Path, w: u32, h: u32, rgba: &[u8]) {
        encode(path, &GUID_ContainerFormatJpeg, GUID_WICPixelFormat24bppBGR, (w, h), w * 3, &bgr(rgba), None);
    }

    /// 24-bit BMP (alpha dropped).
    pub fn write_bmp(path: &Path, w: u32, h: u32, rgba: &[u8]) {
        // BMP rows are padded to four bytes; WIC takes the stride, so pass a padded buffer.
        let stride = (w * 3).div_ceil(4) * 4;
        let mut buf = vec![0u8; (stride * h) as usize];
        for y in 0..h as usize {
            let row = bgr(&rgba[y * w as usize * 4..(y + 1) * w as usize * 4]);
            buf[y * stride as usize..y * stride as usize + row.len()].copy_from_slice(&row);
        }
        encode(path, &GUID_ContainerFormatBmp, GUID_WICPixelFormat24bppBGR, (w, h), stride, &buf, None);
    }

    /// GIF with the picture's own colours as the palette (at most 256 of them; alpha dropped).
    pub fn write_gif(path: &Path, w: u32, h: u32, rgba: &[u8]) {
        let mut colors: Vec<u32> = Vec::new();
        let mut index = Vec::with_capacity((w * h) as usize);
        for p in rgba.chunks(4) {
            let c = 0xFF00_0000 | (p[0] as u32) << 16 | (p[1] as u32) << 8 | p[2] as u32;
            let i = colors.iter().position(|x| *x == c).unwrap_or_else(|| {
                colors.push(c);
                colors.len() - 1
            });
            index.push(i as u8);
        }
        assert!(colors.len() <= 256);
        encode(path, &GUID_ContainerFormatGif, GUID_WICPixelFormat8bppIndexed, (w, h), w, &index, Some(&colors));
    }

    /// An identity 3D `.cube` text: `size` cubed entries, red changing fastest, values `i / (size - 1)`.
    pub fn identity_cube_3d(size: u32) -> String {
        let mut s = format!("LUT_3D_SIZE {size}\n");
        let d = (size.max(2) - 1) as f32;
        for b in 0..size {
            for g in 0..size {
                for r in 0..size {
                    s.push_str(&format!("{:.6} {:.6} {:.6}\n", r as f32 / d, g as f32 / d, b as f32 / d));
                }
            }
        }
        s
    }

    /// An identity 1D `.cube` text.
    pub fn identity_cube_1d(size: u32) -> String {
        let mut s = format!("LUT_1D_SIZE {size}\n");
        let d = (size.max(2) - 1) as f32;
        for i in 0..size {
            let v = i as f32 / d;
            s.push_str(&format!("{v:.6} {v:.6} {v:.6}\n"));
        }
        s
    }

    /// Refresh the small files in `tests/assets/` (run with `cargo test -p better-cam write_committed_assets --
    /// --ignored`).
    #[test]
    #[ignore = "writes into the source tree"]
    fn write_committed_assets() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("assets");
        std::fs::create_dir_all(&dir).unwrap();
        let (w, h, rgba) = picture(128);
        write_png(&dir.join("picture.png"), w, h, &rgba);
        let (w, h, opaque) = picture(255);
        write_jpeg(&dir.join("picture.jpg"), w, h, &opaque);
        write_bmp(&dir.join("picture.bmp"), w, h, &opaque);
        write_gif(&dir.join("picture.gif"), w, h, &opaque);
        std::fs::write(dir.join("identity_3d_9.cube"), identity_cube_3d(9)).unwrap();
        std::fs::write(dir.join("identity_1d_16.cube"), identity_cube_1d(16)).unwrap();
        // A table that is not the identity: the blue and red channels swapped, a custom domain.
        let mut swap = String::from("TITLE \"swap red and blue\"\nLUT_3D_SIZE 2\nDOMAIN_MIN 0 0 0\nDOMAIN_MAX 1 1 1\n");
        for b in 0..2 {
            for g in 0..2 {
                for r in 0..2 {
                    swap.push_str(&format!("{b} {g} {r}\n"));
                }
            }
        }
        std::fs::write(dir.join("swap_rb_2.cube"), swap).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::testgen::*;
    use super::*;
    use crate::fx::shader_filter::{ExtraInput, ShaderFilter, ShaderFilterDef};
    use crate::fx::{Chain, FilterCfg, GpuFilter, Params};
    use crate::gpu::pipeline_tests::{fill_rgba, read_rgba, warp};
    use serde_json::json;
    use windows::core::Interface;
    use windows::Win32::Graphics::Direct3D11::{ID3D11Texture2D, ID3D11Texture3D};

    // ---- half floats

    #[test]
    fn half_conversion_matches_ieee() {
        let cases: &[(f32, u16)] = &[
            (0.0, 0x0000),
            (-0.0, 0x8000),
            (1.0, 0x3c00),
            (0.5, 0x3800),
            (-2.0, 0xc000),
            (65504.0, 0x7bff),
            (65520.0, 0x7c00),
            (1.0e9, 0x7c00),
            (6.103_515_6e-5, 0x0400),
            (5.960_464_5e-8, 0x0001),
            (1.0e-9, 0x0000),
            (f32::INFINITY, 0x7c00),
            (f32::NEG_INFINITY, 0xfc00),
            // 1 + 2^-11 is the tie between 1.0 and the next half: even wins
            (1.000_488_3, 0x3c00),
            (1.001_464_8, 0x3c02),
            (0.333_333_34, 0x3555),
        ];
        for (f, h) in cases {
            assert_eq!(f32_to_f16(*f), *h, "{f}");
        }
        assert_eq!(f32_to_f16(f32::NAN) & 0x7c00, 0x7c00);
        assert_ne!(f32_to_f16(f32::NAN) & 0x03ff, 0);
    }

    // ---- paths

    #[test]
    fn paths_normalize_to_one_key() {
        let a = normalize(r"D:\Luts\..\Look\Warm.CUBE");
        assert_eq!(a, r"d:\look\warm.cube");
        assert_eq!(normalize("D:/Look/./warm.cube"), a);
        assert_eq!(normalize(r"\\?\D:\Look\warm.cube"), a);
        assert_eq!(normalize("  \"D:\\Look\\warm.cube\"  "), a);
        assert_eq!(normalize(r"\\server\share\x.png"), r"\\server\share\x.png");
        assert_ne!(normalize(r"D:\a\b.png"), normalize(r"D:\a\c.png"));
        assert_eq!(normalize(""), "");
    }

    // ---- .cube

    #[test]
    fn identity_3d_tables_parse() {
        for size in [2u32, 17, 33] {
            let lut = parse_cube(&identity_cube_3d(size)).unwrap();
            assert_eq!((lut.dim, lut.size), (LutDim::ThreeD, size));
            assert_eq!(lut.domain_min, [0.0; 3]);
            assert_eq!(lut.domain_max, [1.0; 3]);
            assert_eq!(lut.data.len() as u64, CubeLut::entries(LutDim::ThreeD, size));
            let d = (size - 1) as f32;
            // red changes fastest: entry 1 is red index 1, entry `size` is green index 1, entry size^2 blue index 1
            assert_eq!(lut.data[0], [0.0, 0.0, 0.0]);
            assert!((lut.data[1][0] - 1.0 / d).abs() < 1e-6 && lut.data[1][1] == 0.0);
            assert!((lut.data[size as usize][1] - 1.0 / d).abs() < 1e-6 && lut.data[size as usize][0] == 0.0);
            let slice = (size * size) as usize;
            assert!((lut.data[slice][2] - 1.0 / d).abs() < 1e-6 && lut.data[slice][0] == 0.0);
            assert_eq!(*lut.data.last().unwrap(), [1.0; 3]);
        }
    }

    #[test]
    fn a_1d_table_parses_and_wins_over_a_3d_size() {
        let lut = parse_cube(&identity_cube_1d(8)).unwrap();
        assert_eq!((lut.dim, lut.size, lut.data.len()), (LutDim::OneD, 8, 8));
        assert_eq!(lut.data[7], [1.0; 3]);
        // OBS looks at the 1D size first
        let both = "LUT_3D_SIZE 2\nLUT_1D_SIZE 2\n0 0 0\n1 1 1\n";
        assert_eq!(parse_cube(both).unwrap().dim, LutDim::OneD);
    }

    #[test]
    fn domain_comments_title_blank_lines_and_crlf() {
        let text = "# made by hand\r\n\r\nTITLE \"My look\"\r\nLUT_3D_SIZE 2\r\nDOMAIN_MIN -0.5 0 0.25\r\n\
                    DOMAIN_MAX 1.5 2 1\r\nLUT_3D_INPUT_RANGE 0 1\r\n\r\n# data\r\n\
                    0 0 0\r\n1 0 0\r\n0 1 0\r\n1 1 0\r\n\r\n0 0 1\r\n1 0 1\r\n0 1 1\r\n1e0 1.0 +1\r\n";
        let lut = parse_cube(text).unwrap();
        assert_eq!(lut.size, 2);
        assert_eq!(lut.domain_min, [-0.5, 0.0, 0.25]);
        assert_eq!(lut.domain_max, [1.5, 2.0, 1.0]);
        assert_eq!(lut.data.len(), 8);
        assert_eq!(lut.data[3], [1.0, 1.0, 0.0]);
        assert_eq!(lut.data[7], [1.0, 1.0, 1.0]);
        // LF only and a missing last newline work too
        assert!(parse_cube(text.replace("\r\n", "\n").trim_end()).is_ok());
    }

    #[test]
    fn broken_tables_are_errors_not_panics() {
        let ok = identity_cube_3d(2);
        let cases: Vec<(&str, String)> = vec![
            ("missing size", "0 0 0\n1 1 1\n".into()),
            ("only a title", "TITLE \"x\"\n".into()),
            ("size but no data", "LUT_3D_SIZE 2\n".into()),
            ("too few entries", ok.lines().take(8).collect::<Vec<_>>().join("\n")),
            ("too many entries", format!("{ok}0 0 0\n")),
            ("not a number", ok.replacen("0.000000 1.000000 0.000000", "0.000000 abc 0.000000", 1)),
            ("two numbers", ok.replacen("1.000000 1.000000 1.000000", "1.000000 1.000000", 1)),
            ("four numbers", ok.replacen("1.000000 1.000000 1.000000", "1 1 1 1", 1)),
            ("text among entries", ok.replacen("0.000000 1.000000 0.000000", "LUT_3D_SIZE 4", 1)),
            ("infinity", ok.replacen("0.000000 0.000000 1.000000", "0 0 -inf", 1)),
            ("size 0", "LUT_3D_SIZE 0\n0 0 0\n".into()),
            ("1D size 0", "LUT_1D_SIZE 0\n0 0 0\n".into()),
            ("3D size above the limit", "LUT_3D_SIZE 257\n0 0 0\n".into()),
            ("1D size above the limit", "LUT_1D_SIZE 65537\n0 0 0\n".into()),
            ("negative size", "LUT_3D_SIZE -2\n0 0 0\n".into()),
            ("size not a number", "LUT_3D_SIZE two\n0 0 0\n".into()),
            ("size without a value", "LUT_3D_SIZE\n0 0 0\n".into()),
            ("size beyond u64", "LUT_3D_SIZE 99999999999999999999999\n0 0 0\n".into()),
            ("inverted domain", format!("DOMAIN_MIN 1 1 1\nDOMAIN_MAX 0 0 0\n{ok}")),
            ("empty domain", format!("DOMAIN_MAX 0 1 1\n{ok}")),
            ("short domain", format!("DOMAIN_MIN 0 0\n{ok}")),
            ("empty file", String::new()),
            ("blanks only", " \n\r\n\t\n".into()),
            ("binary junk", "\u{0}\u{1}\u{2}\u{ff}".into()),
        ];
        for (name, text) in cases {
            let r = std::panic::catch_unwind(|| parse_cube(&text));
            let r = r.unwrap_or_else(|_| panic!("{name}: panicked"));
            assert!(r.is_err(), "{name}: accepted");
            assert!(!r.unwrap_err().is_empty(), "{name}: empty message");
        }
    }

    #[test]
    fn a_header_that_claims_a_gigabyte_in_a_tiny_file_is_cheap() {
        // 256 cubed entries claimed, one given: refused without building anything of that size.
        let t = Instant::now();
        let e = parse_cube("LUT_3D_SIZE 256\n0 0 0\n").unwrap_err();
        assert!(e.contains("16777216"), "{e}");
        assert!(t.elapsed() < Duration::from_secs(1));
        let e = parse_cube("LUT_1D_SIZE 65536\n0 0 0\n").unwrap_err();
        assert!(e.contains("65536"), "{e}");
    }

    #[test]
    fn table_texels_are_half_floats_with_alpha_one() {
        let lut = parse_cube("LUT_1D_SIZE 2\n0 0.5 1\n0.25 0.5 0.75\n").unwrap();
        let raw = lut.rgba16f(false);
        assert_eq!(raw, vec![0x0000, 0x3800, 0x3c00, 0x3c00, 0x3400, 0x3800, 0x3a00, 0x3c00]);
        // OBS converts the values from sRGB to linear: 0.5 becomes 0.2140...
        let lin = lut.rgba16f(true);
        assert_eq!(lin[0], 0x0000);
        assert_eq!(lin[2], 0x3c00);
        assert_eq!(lin[3], 0x3c00);
        assert_eq!(lin[1], f32_to_f16(srgb_to_linear(0.5)));
        assert!((srgb_to_linear(0.5) - 0.2140).abs() < 1e-3);
    }

    #[test]
    fn tables_become_textures_on_the_gpu() {
        let r = warp();
        let gpu = &r.gpu;
        for size in [2u32, 9, 33] {
            let lut = parse_cube(&identity_cube_3d(size)).unwrap();
            let srv = lut.create_texture(gpu, true).unwrap();
            let tex = unsafe { srv.GetResource() }.unwrap().cast::<ID3D11Texture3D>().unwrap();
            let mut desc = Default::default();
            unsafe { tex.GetDesc(&mut desc) };
            assert_eq!((desc.Width, desc.Height, desc.Depth), (size, size, size));
            assert_eq!(desc.Format, DXGI_FORMAT_R16G16B16A16_FLOAT);
        }
        let lut = parse_cube(&identity_cube_1d(64)).unwrap();
        let srv = lut.create_texture(gpu, false).unwrap();
        let tex = unsafe { srv.GetResource() }.unwrap().cast::<ID3D11Texture2D>().unwrap();
        let mut desc = Default::default();
        unsafe { tex.GetDesc(&mut desc) };
        assert_eq!((desc.Width, desc.Height), (64, 1));
        // a 1D table the hardware cannot hold is an error
        let big = CubeLut {
            dim: LutDim::OneD,
            size: 20_000,
            domain_min: [0.0; 3],
            domain_max: [1.0; 3],
            data: vec![[0.0; 3]; 20_000],
        };
        assert!(big.create_texture(gpu, false).is_err());
        // texture helpers refuse data of the wrong length
        assert!(create_texture3d_rgba8(gpu, 4, &[0u8; 10]).is_err());
        assert!(create_texture_rgba8(gpu, 4, 4, &[0u8; 10]).is_err());
        assert!(create_texture3d_rgba8(gpu, 4, &vec![0u8; 4 * 4 * 4 * 4]).is_ok());
        assert!(create_texture3d_rgba16f(gpu, 2, &[0u16; 2 * 2 * 2 * 4]).is_ok());
    }

    // ---- images

    fn px(img: &DecodedImage, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * img.width + x) * 4) as usize;
        [img.rgba[i], img.rgba[i + 1], img.rgba[i + 2], img.rgba[i + 3]]
    }

    /// The centre of each block of the test picture.
    fn blocks(img: &DecodedImage) -> [[u8; 4]; 4] {
        [px(img, 8, 4), px(img, 24, 4), px(img, 8, 12), px(img, 24, 12)]
    }

    fn close(a: [u8; 4], b: [u8; 4], tol: i32) -> bool {
        a.iter().zip(b).all(|(x, y)| (*x as i32 - y as i32).abs() <= tol)
    }

    #[test]
    fn png_keeps_straight_alpha_and_the_top_row_first() {
        let dir = TempDir::new("png");
        let (w, h, rgba) = picture(128);
        let f = dir.join("a.png");
        write_png(&f, w, h, &rgba);
        let img = decode_image(&f).unwrap();
        assert_eq!((img.width, img.height), (32, 16));
        assert_eq!(img.rgba.len(), 32 * 16 * 4);
        // exact, and the yellow stays (255, 255, 0): straight alpha, not premultiplied
        assert_eq!(
            blocks(&img),
            [[255, 0, 0, 255], [0, 255, 0, 255], [0, 0, 255, 255], [255, 255, 0, 128]]
        );
        assert_eq!(img.rgba, rgba);
    }

    #[test]
    fn jpeg_bmp_and_gif_decode_to_the_same_picture() {
        let dir = TempDir::new("others");
        let (w, h, rgba) = picture(255);
        let want = [[255, 0, 0, 255], [0, 255, 0, 255], [0, 0, 255, 255], [255, 255, 0, 255]];
        let (j, b, g) = (dir.join("a.jpg"), dir.join("a.bmp"), dir.join("a.gif"));
        write_jpeg(&j, w, h, &rgba);
        write_bmp(&b, w, h, &rgba);
        write_gif(&g, w, h, &rgba);
        for (name, f, tol) in [("jpeg", &j, 24), ("bmp", &b, 0), ("gif", &g, 0)] {
            let img = decode_image(f).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!((img.width, img.height), (32, 16), "{name}");
            for (got, want) in blocks(&img).into_iter().zip(want) {
                assert!(close(got, want, tol), "{name}: {got:?} vs {want:?}");
            }
        }
        // a .jpg named .bmp still decodes: WIC goes by content
        let odd = dir.join("jpeg-named.bmp");
        std::fs::copy(&j, &odd).unwrap();
        assert!(decode_image(&odd).is_ok());
    }

    #[test]
    fn damaged_and_missing_pictures_are_errors() {
        let dir = TempDir::new("bad");
        let (w, h, rgba) = picture(255);
        let good = dir.join("good.png");
        write_png(&good, w, h, &rgba);
        let bytes = std::fs::read(&good).unwrap();

        let cut = dir.join("cut.png");
        std::fs::write(&cut, &bytes[..bytes.len() / 2]).unwrap();
        let text = dir.join("text.png");
        std::fs::write(&text, "this is not a picture, only text\n").unwrap();
        let empty = dir.join("empty.png");
        std::fs::write(&empty, b"").unwrap();
        let folder = dir.join("folder.png");
        std::fs::create_dir(&folder).unwrap();

        for (name, p) in [("truncated", &cut), ("text", &text), ("empty", &empty), ("folder", &folder)] {
            let e = decode_image(p).expect_err(name);
            assert!(!e.is_empty(), "{name}");
        }
        assert!(decode_image(&cut).unwrap_err().contains("cut short"));
        // the other formats, cut in half
        let (w, h, opaque) = picture(255);
        let (j, b, g) = (dir.join("a.jpg"), dir.join("a.bmp"), dir.join("a.gif"));
        write_jpeg(&j, w, h, &opaque);
        write_bmp(&b, w, h, &opaque);
        write_gif(&g, w, h, &opaque);
        for f in [&j, &b, &g] {
            assert!(decode_image(f).is_ok(), "{f:?} whole");
            let bytes = std::fs::read(f).unwrap();
            let half = dir.join(&format!("half-{}", f.file_name().unwrap().to_string_lossy()));
            std::fs::write(&half, &bytes[..bytes.len() - 10]).unwrap();
            assert!(decode_image(&half).is_err(), "{half:?} cut");
        }
        assert!(decode_image(&text).unwrap_err().contains("not a supported"));
        assert_eq!(decode_image(&dir.join("missing.png")).unwrap_err(), "file not found");
    }

    #[test]
    fn a_picture_above_the_size_limit_is_refused() {
        let dir = TempDir::new("big");
        // 8193 x 1 is cheap to encode and over the limit
        let f = dir.join("wide.png");
        write_png(&f, 8193, 1, &vec![200u8; 8193 * 4]);
        let e = decode_image(&f).unwrap_err();
        assert!(e.contains("8193x1") && e.contains("8192"), "{e}");
        let f = dir.join("ok.png");
        write_png(&f, 8192, 1, &vec![200u8; 8192 * 4]);
        assert_eq!(decode_image(&f).unwrap().width, 8192);
    }

    #[test]
    fn the_committed_test_files_load() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("assets");
        let png = decode_image(&dir.join("picture.png")).unwrap();
        assert_eq!(blocks(&png)[3], [255, 255, 0, 128]);
        for f in ["picture.jpg", "picture.bmp", "picture.gif"] {
            let img = decode_image(&dir.join(f)).unwrap_or_else(|e| panic!("{f}: {e}"));
            assert_eq!((img.width, img.height), (32, 16), "{f}");
            assert!(close(blocks(&img)[0], [255, 0, 0, 255], 24), "{f}");
        }
        for (f, dim, size) in [
            ("identity_3d_9.cube", LutDim::ThreeD, 9),
            ("identity_1d_16.cube", LutDim::OneD, 16),
            ("swap_rb_2.cube", LutDim::ThreeD, 2),
        ] {
            let text = std::fs::read_to_string(dir.join(f)).unwrap();
            let lut = parse_cube(&text).unwrap_or_else(|e| panic!("{f}: {e}"));
            assert_eq!((lut.dim, lut.size), (dim, size), "{f}");
        }
        // swap_rb: red input (entry 1) gives blue output
        let swap = parse_cube(&std::fs::read_to_string(dir.join("swap_rb_2.cube")).unwrap()).unwrap();
        assert_eq!(swap.data[1], [0.0, 0.0, 1.0]);
    }

    // ---- the service

    type Log = Arc<Mutex<Vec<FilterError>>>;

    fn service(scan_ms: u64) -> (Assets, Log) {
        service_on(warp().gpu.clone(), scan_ms)
    }

    /// A service on this device (textures only work with the device that made them).
    fn service_on(gpu: Arc<GpuDevice>, scan_ms: u64) -> (Assets, Log) {
        let log: Log = Arc::default();
        let sink = log.clone();
        let a = Assets::with_scan_interval(
            gpu,
            Arc::new(move |e| lock(&sink).push(e)),
            Duration::from_millis(scan_ms),
        );
        (a, log)
    }

    fn wait_until(what: &str, secs: u64, mut f: impl FnMut() -> bool) {
        let end = Instant::now() + Duration::from_secs(secs);
        while !f() {
            assert!(Instant::now() < end, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn path_str(p: &Path) -> String {
        p.to_string_lossy().into_owned()
    }

    #[test]
    fn a_request_is_not_ready_at_once_and_ready_later() {
        let dir = TempDir::new("svc1");
        let (w, h, rgba) = picture(255);
        let f = dir.join("p.png");
        write_png(&f, w, h, &rgba);
        let (assets, log) = service(100);
        let mut hd = assets.client().handle("card1");
        assert_eq!(hd.state(), AssetState::None);
        assert!(hd.get().is_none());

        hd.request(AssetKind::Image, &path_str(&f));
        assert!(hd.get().is_none(), "the request returns before the file is read");
        wait_until("the image", 5, || hd.get().is_some());
        let asset = hd.get().unwrap();
        let Asset::Image(img) = &*asset else { panic!("not an image") };
        assert_eq!((img.width, img.height), (32, 16));
        assert_eq!(hd.state(), AssetState::Ready);
        assert_eq!(assets.load_count(), 1);
        // success tells the card there is no error: one event with an empty message
        assert_eq!(&*lock(&log), &[FilterError { id: "card1".into(), message: String::new() }]);

        // the same request again: nothing happens
        for _ in 0..50 {
            hd.request(AssetKind::Image, &path_str(&f));
        }
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(assets.load_count(), 1);
        assert_eq!(lock(&log).len(), 1);
    }

    #[test]
    fn two_filters_asking_for_one_path_cause_one_load() {
        let dir = TempDir::new("svc2");
        let (w, h, rgba) = picture(255);
        let f = dir.join("shared.png");
        write_png(&f, w, h, &rgba);
        let (assets, _log) = service(100);
        let client = assets.client();
        let (mut a, mut b) = (client.handle("a"), client.handle("b"));
        // the same file, spelled two ways
        a.request(AssetKind::Image, &path_str(&f));
        b.request(AssetKind::Image, &path_str(&f).replace('\\', "/").to_uppercase());
        wait_until("both", 5, || a.get().is_some() && b.get().is_some());
        assert_eq!(assets.load_count(), 1);
        assert!(Arc::ptr_eq(&a.get().unwrap(), &b.get().unwrap()));
        // one lets go: the other keeps its asset
        drop(a);
        assert!(b.get().is_some());
        // a different kind of the same path is a different asset
        let mut c = client.handle("c");
        c.request(AssetKind::Lut, &path_str(&f));
        wait_until("lut image", 5, || c.get().is_some());
        assert!(matches!(&*c.get().unwrap(), Asset::LutImage(i) if i.width == 32));
        assert_eq!(assets.load_count(), 2);
    }

    #[test]
    fn a_cube_file_loads_as_a_table() {
        let dir = TempDir::new("svc3");
        let f = dir.join("look.CUBE");
        std::fs::write(&f, identity_cube_3d(5)).unwrap();
        let (assets, _log) = service(100);
        let mut hd = assets.client().handle("lut");
        hd.request(AssetKind::Lut, &path_str(&f));
        wait_until("the table", 5, || hd.get().is_some());
        let asset = hd.get().unwrap();
        let Asset::Cube(c) = &*asset else { panic!("not a table") };
        assert_eq!((c.lut.dim, c.lut.size), (LutDim::ThreeD, 5));
        assert!(asset.srv().is_some());
    }

    #[test]
    fn a_changed_file_is_loaded_again_within_two_seconds() {
        let dir = TempDir::new("svc4");
        let f = dir.join("m.png");
        write_png(&f, 4, 4, &[255u8; 4 * 4 * 4]);
        // the real interval of one second
        let log: Log = Arc::default();
        let sink = log.clone();
        let assets = Assets::new(warp().gpu.clone(), Arc::new(move |e| lock(&sink).push(e)));
        let mut hd = assets.client().handle("mask");
        hd.request(AssetKind::Image, &path_str(&f));
        wait_until("the first load", 5, || hd.get().is_some());
        let first = hd.get().unwrap();
        assert!(matches!(&*first, Asset::Image(i) if i.width == 4));

        // not changed: still one load after a couple of scans
        std::thread::sleep(Duration::from_millis(2300));
        assert_eq!(assets.load_count(), 1);

        write_png(&f, 8, 2, &[128u8; 8 * 2 * 4]);
        let t = Instant::now();
        wait_until("the reload", 5, || hd.get().is_some_and(|a| !Arc::ptr_eq(&a, &first)));
        assert!(t.elapsed() < Duration::from_secs(2), "reload took {:?}", t.elapsed());
        assert!(matches!(&*hd.get().unwrap(), Asset::Image(i) if (i.width, i.height) == (8, 2)));
        assert_eq!(assets.load_count(), 2);
        assert!(lock(&log).iter().all(|e| e.message.is_empty()));
    }

    #[test]
    fn an_empty_path_is_no_asset_and_no_error() {
        let (assets, log) = service(50);
        let mut hd = assets.client().handle("x");
        hd.request(AssetKind::Image, "");
        hd.request(AssetKind::Lut, "   ");
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(hd.state(), AssetState::None);
        assert!(hd.get().is_none());
        assert_eq!(assets.load_count(), 0);
        assert!(lock(&log).is_empty());
    }

    #[test]
    fn a_failed_load_is_reported_once_and_a_fix_clears_it() {
        let dir = TempDir::new("svc5");
        let f = dir.join("mask.png");
        std::fs::write(&f, "not a picture").unwrap();
        let (assets, log) = service(50);
        let mut hd = assets.client().handle("card9");
        hd.request(AssetKind::Image, &path_str(&f));
        wait_until("the failure", 5, || matches!(hd.state(), AssetState::Failed(_)));
        std::thread::sleep(Duration::from_millis(100));
        {
            let l = lock(&log);
            assert_eq!(l.len(), 1);
            assert_eq!(l[0].id, "card9");
            assert!(l[0].message.starts_with("could not load mask.png: "), "{}", l[0].message);
        }
        assert!(hd.get().is_none(), "a failed file gives no asset (the filter passes through)");

        // many frames later, still one report: asking again and scanning do not repeat it
        for _ in 0..100 {
            hd.request(AssetKind::Image, &path_str(&f));
        }
        std::thread::sleep(Duration::from_millis(400));
        assert_eq!(lock(&log).len(), 1);
        assert_eq!(assets.load_count(), 1);

        // the file changes and fails again: reported again
        std::fs::write(&f, "still not a picture, but different").unwrap();
        wait_until("the second report", 5, || lock(&log).len() == 2);
        assert!(lock(&log)[1].message.starts_with("could not load mask.png: "));

        // the file is fixed: the card is told to clear its line
        let (w, h, rgba) = picture(255);
        write_png(&f, w, h, &rgba);
        wait_until("the fix", 5, || hd.get().is_some());
        wait_until("the clear", 5, || lock(&log).len() == 3);
        assert_eq!(lock(&log)[2], FilterError { id: "card9".into(), message: String::new() });

        // a second filter that asks for a file that failed is told too, once
        let g = dir.join("missing.cube");
        let mut other = assets.client().handle("card10");
        other.request(AssetKind::Lut, &path_str(&g));
        wait_until("the missing file", 5, || matches!(other.state(), AssetState::Failed(_)));
        let l = lock(&log);
        let last = l.last().unwrap();
        assert_eq!(last.id, "card10");
        assert_eq!(last.message, "could not load missing.cube: file not found");
        drop(l);
        let mut third = assets.client().handle("card11");
        third.request(AssetKind::Lut, &path_str(&g));
        let l = lock(&log);
        assert_eq!(l.last().unwrap().id, "card11");
        assert_eq!(l.last().unwrap().message, "could not load missing.cube: file not found");
    }

    #[test]
    fn leaving_a_failed_path_clears_the_card() {
        let dir = TempDir::new("svc6");
        let (assets, log) = service(50);
        let mut hd = assets.client().handle("c");
        hd.request(AssetKind::Image, &path_str(&dir.join("gone.png")));
        wait_until("the failure", 5, || matches!(hd.state(), AssetState::Failed(_)));
        hd.request(AssetKind::Image, "");
        let l = lock(&log);
        assert_eq!(l.len(), 2);
        assert!(l[1].message.is_empty());
        drop(l);
        assert_eq!(hd.state(), AssetState::None);
    }

    // ---- a filter that uses an image

    static MULTIPLY: ShaderFilterDef = ShaderFilterDef {
        kind: "test_assets_multiply",
        hlsl: "Texture2D<float4> image : register(t0);\n\
               Texture2D<float4> tint : register(t1);\n\
               SamplerState linear_clamp : register(s0);\n\
               cbuffer Params : register(b0) { float4 pad; };\n\
               float4 main(float4 pos : SV_Position) : SV_Target {\n\
                   return image.Load(int3(pos.xy, 0)) * tint.Load(int3(pos.xy, 0));\n\
               }\n",
        pack: |_p: &Params| vec![0u8; 16],
        extra: &[ExtraInput::Image],
    };

    fn multiply_factory(kind: &str) -> Option<Box<dyn GpuFilter>> {
        (kind == "test_assets_multiply").then(|| Box::new(ShaderFilter::new(&MULTIPLY)) as Box<dyn GpuFilter>)
    }

    #[test]
    fn a_shader_filter_passes_through_until_its_image_is_ready_then_uses_it() {
        let dir = TempDir::new("flt");
        // a 4 x 4 tint of (128, 255, 0, 255)
        let f = dir.join("tint.png");
        let tint: Vec<u8> = (0..16).flat_map(|_| [128u8, 255, 0, 255]).collect();
        write_png(&f, 4, 4, &tint);

        let r = warp();
        let (assets, log) = service_on(r.gpu.clone(), 100);
        let mut chain = Chain::with_factory(multiply_factory);
        chain.set_assets(assets.client());
        let cfg = |path: &str| {
            vec![FilterCfg {
                id: "t".into(),
                kind: "test_assets_multiply".into(),
                enabled: true,
                params: json!({ "image_path": path }).as_object().cloned().unwrap(),
            }]
        };
        let input = [200u8, 100, 50, 255];
        let run = |chain: &mut Chain| -> [u8; 4] {
            let tex = r.texture(4, 4).unwrap();
            fill_rgba(&r, &tex, |_, _| input);
            let out = chain.run(&r, tex).unwrap();
            let p = read_rgba(&r, &out);
            [p[0], p[1], p[2], p[3]]
        };

        // no path: the picture comes through unchanged
        chain.apply(&r, &cfg(""));
        assert_eq!(run(&mut chain), input);

        // a path: the filter has nothing yet (or the image, if the loader was quick), then multiplies
        chain.apply(&r, &cfg(&path_str(&f)));
        let end = Instant::now() + Duration::from_secs(5);
        let mut seen_pass_through = false;
        let multiplied = loop {
            let got = run(&mut chain);
            if got == input {
                seen_pass_through = true;
            } else {
                break got;
            }
            assert!(Instant::now() < end, "the image never arrived");
            std::thread::sleep(Duration::from_millis(5));
        };
        // 200 * 128 / 255 = 100.4, 100 * 1, 50 * 0, alpha 255
        assert!(close(multiplied, [100, 100, 0, 255], 1), "{multiplied:?}");
        let _ = seen_pass_through; // the first frames usually are; not guaranteed on a fast machine
        assert!(lock(&log).iter().all(|e| e.message.is_empty()));

        // the file goes: the filter falls back to pass-through and the card hears why
        std::fs::remove_file(&f).unwrap();
        wait_until("the failure", 5, || lock(&log).iter().any(|e| e.message.contains("file not found")));
        assert_eq!(run(&mut chain), input);

        // back to no path: the error clears
        chain.apply(&r, &cfg(""));
        assert!(lock(&log).last().unwrap().message.is_empty());
    }

    #[test]
    fn a_chain_without_a_service_passes_an_image_filter_through() {
        let r = warp();
        let mut chain = Chain::with_factory(multiply_factory);
        chain.apply(
            &r,
            &[FilterCfg {
                id: "t".into(),
                kind: "test_assets_multiply".into(),
                enabled: true,
                params: json!({ "image_path": "C:\\nowhere.png" }).as_object().cloned().unwrap(),
            }],
        );
        let tex = r.texture(2, 2).unwrap();
        fill_rgba(&r, &tex, |_, _| [9, 8, 7, 255]);
        let out = chain.run(&r, tex).unwrap();
        assert_eq!(&read_rgba(&r, &out)[..4], &[9, 8, 7, 255]);
    }
}
