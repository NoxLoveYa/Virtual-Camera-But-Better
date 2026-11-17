//! `gen-slates [--preview DIR]`: render the three slates (1920x1080, dark gray, Better Cam mark, one line of text)
//! with GDI, convert to NV12 (BT.709 limited), RLE-compress and write `crates/vcam-source/assets/slates/*.rle`.
//! The DLL embeds those files; rerun this command (and rebuild) only when the look of a slate changes.

use std::path::{Path, PathBuf};

use vcam_protocol::slate::{self, SlateKind, HEIGHT, WIDTH};
use windows::Win32::Foundation::{COLORREF, RECT};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreateFontW, CreatePen, CreateSolidBrush, DeleteDC,
    DeleteObject, DrawTextW, Ellipse, FillRect, GdiFlush, GetStockObject, RoundRect, SelectObject,
    SetBkMode, SetTextColor, ANTIALIASED_QUALITY, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DIB_RGB_COLORS, DT_CENTER, DT_SINGLELINE, DT_VCENTER,
    HGDIOBJ, NULL_BRUSH, OUT_DEFAULT_PRECIS, PS_SOLID, TRANSPARENT,
};
use windows_core::w;

pub const USAGE: &str = "usage: vcam-tool gen-slates [--preview DIR]";

const BACKGROUND: (u8, u8, u8) = (30, 31, 36);
const INK: (u8, u8, u8) = (236, 237, 241);
const ACCENT: (u8, u8, u8) = (90, 150, 255);

fn rgb(c: (u8, u8, u8)) -> COLORREF {
    COLORREF(c.0 as u32 | (c.1 as u32) << 8 | (c.2 as u32) << 16)
}

/// Render a slate as tightly packed BGRA, `WIDTH x HEIGHT`, top-down.
pub fn render_bgra(kind: SlateKind) -> Result<Vec<u8>, String> {
    let (w, h) = (WIDTH as i32, HEIGHT as i32);
    unsafe {
        let dc = CreateCompatibleDC(None);
        if dc.is_invalid() {
            return Err("CreateCompatibleDC failed".into());
        }
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let bitmap = CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0)
            .map_err(|e| format!("CreateDIBSection: {e}"))?;
        let old_bitmap = SelectObject(dc, HGDIOBJ(bitmap.0));

        // background
        let bg = CreateSolidBrush(rgb(BACKGROUND));
        FillRect(
            dc,
            &RECT {
                left: 0,
                top: 0,
                right: w,
                bottom: h,
            },
            bg,
        );

        // mark: a camera lens (ring + dot) on a rounded body
        let (cx, cy) = (w / 2, 400);
        let ink_pen = CreatePen(PS_SOLID, 16, rgb(INK));
        let old_pen = SelectObject(dc, HGDIOBJ(ink_pen.0));
        let old_brush = SelectObject(dc, GetStockObject(NULL_BRUSH));
        let _ = RoundRect(dc, cx - 190, cy - 130, cx + 190, cy + 130, 70, 70);
        let _ = Ellipse(dc, cx - 90, cy - 90, cx + 90, cy + 90);
        // viewfinder bump
        let _ = RoundRect(dc, cx + 90, cy - 170, cx + 150, cy - 130, 14, 14);
        SelectObject(dc, old_brush);
        let accent = CreateSolidBrush(rgb(ACCENT));
        let accent_pen = CreatePen(PS_SOLID, 1, rgb(ACCENT));
        SelectObject(dc, HGDIOBJ(accent_pen.0));
        let old_b2 = SelectObject(dc, HGDIOBJ(accent.0));
        let _ = Ellipse(dc, cx - 36, cy - 36, cx + 36, cy + 36);
        SelectObject(dc, old_b2);

        // one line of text
        let font = CreateFontW(
            -72,
            0,
            0,
            0,
            600,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            ANTIALIASED_QUALITY,
            0,
            w!("Segoe UI"),
        );
        let old_font = SelectObject(dc, HGDIOBJ(font.0));
        SetBkMode(dc, TRANSPARENT);
        SetTextColor(dc, rgb(INK));
        let mut text: Vec<u16> = kind.text().encode_utf16().collect();
        let mut rect = RECT {
            left: 0,
            top: 640,
            right: w,
            bottom: 780,
        };
        DrawTextW(
            dc,
            &mut text,
            &mut rect,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE,
        );

        let _ = GdiFlush();
        let out = std::slice::from_raw_parts(bits as *const u8, (w * h * 4) as usize).to_vec();

        SelectObject(dc, old_font);
        SelectObject(dc, old_pen);
        SelectObject(dc, old_bitmap);
        for o in [
            HGDIOBJ(font.0),
            HGDIOBJ(ink_pen.0),
            HGDIOBJ(accent_pen.0),
            HGDIOBJ(accent.0),
            HGDIOBJ(bg.0),
            HGDIOBJ(bitmap.0),
        ] {
            let _ = DeleteObject(o);
        }
        let _ = DeleteDC(dc);
        Ok(out)
    }
}

/// BT.709 limited-range Y'CbCr for 8-bit full-range R'G'B'.
fn yuv709(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let y = 16.0 + 219.0 * (0.2126 * r + 0.7152 * g + 0.0722 * b) / 255.0;
    let u =
        128.0 + 224.0 * (-0.2126 / 1.8556 * r - 0.7152 / 1.8556 * g + 0.9278 / 1.8556 * b) / 255.0;
    let v =
        128.0 + 224.0 * (0.7874 / 1.5748 * r - 0.7152 / 1.5748 * g - 0.0722 / 1.5748 * b) / 255.0;
    (y, u, v)
}

/// BGRA (tight) to tight NV12; chroma is the mean of each 2x2 block.
pub fn bgra_to_nv12(bgra: &[u8], width: u32, height: u32) -> Vec<u8> {
    let (w, h) = (width as usize, height as usize);
    let mut out = vec![0u8; w * h * 3 / 2];
    let (yp, uv) = out.split_at_mut(w * h);
    for y in 0..h {
        for x in 0..w {
            let p = &bgra[(y * w + x) * 4..];
            yp[y * w + x] = yuv709(p[2] as f32, p[1] as f32, p[0] as f32).0.round() as u8;
        }
    }
    for cy in 0..h / 2 {
        for cx in 0..w / 2 {
            let (mut u, mut v) = (0.0, 0.0);
            for (dy, dx) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
                let p = &bgra[((cy * 2 + dy) * w + cx * 2 + dx) * 4..];
                let (_, pu, pv) = yuv709(p[2] as f32, p[1] as f32, p[0] as f32);
                u += pu;
                v += pv;
            }
            uv[cy * w + cx * 2] = (u / 4.0).round() as u8;
            uv[cy * w + cx * 2 + 1] = (v / 4.0).round() as u8;
        }
    }
    out
}

/// Where the DLL's `include_bytes!` looks for the slates.
pub fn assets_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("vcam-source")
        .join("assets")
        .join("slates")
}

// ---- minimal PNG writer (stored deflate blocks) for previews ----------------------------------------------------

fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
    }
    !c
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &d in data {
        a = (a + d as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// RGB (tight, 3 bytes per pixel) as an uncompressed PNG.
pub fn png_rgb(rgb: &[u8], width: u32, height: u32) -> Vec<u8> {
    let row = width as usize * 3;
    let mut raw = Vec::with_capacity((row + 1) * height as usize);
    for y in 0..height as usize {
        raw.push(0);
        raw.extend_from_slice(&rgb[y * row..(y + 1) * row]);
    }
    let mut z = vec![0x78, 0x01];
    let mut chunks = raw.chunks(65535).peekable();
    while let Some(c) = chunks.next() {
        z.push(if chunks.peek().is_none() { 1 } else { 0 });
        z.extend_from_slice(&(c.len() as u16).to_le_bytes());
        z.extend_from_slice(&(!(c.len() as u16)).to_le_bytes());
        z.extend_from_slice(c);
    }
    z.extend_from_slice(&adler32(&raw).to_be_bytes());
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}

pub fn run(args: &[String]) -> i32 {
    let mut preview: Option<PathBuf> = None;
    let mut it = args.iter();
    while let Some(k) = it.next() {
        match k.as_str() {
            "--preview" => match it.next() {
                Some(d) => preview = Some(d.into()),
                None => {
                    eprintln!("--preview needs a directory\n{USAGE}");
                    return 2;
                }
            },
            o => {
                eprintln!("unknown argument '{o}'\n{USAGE}");
                return 2;
            }
        }
    }
    let dir = assets_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("cannot create {}: {e}", dir.display());
        return 1;
    }
    if let Some(p) = &preview {
        let _ = std::fs::create_dir_all(p);
    }
    for kind in SlateKind::ALL {
        let bgra = match render_bgra(kind) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("{kind:?}: {e}");
                return 1;
            }
        };
        let nv12 = bgra_to_nv12(&bgra, WIDTH, HEIGHT);
        let enc = slate::encode(&nv12, WIDTH, HEIGHT);
        let path = dir.join(format!("{}.rle", kind.file_stem()));
        if let Err(e) = std::fs::write(&path, &enc) {
            eprintln!("write {}: {e}", path.display());
            return 1;
        }
        println!(
            "{:<10} '{}' -> {} ({} bytes)",
            format!("{kind:?}"),
            kind.text(),
            path.display(),
            enc.len()
        );
        if let Some(p) = &preview {
            let s = slate::decode(&enc).expect("just encoded");
            let (pw, ph) = (960u32, 540u32);
            let bgrx = s.scale_bgrx(pw, ph);
            let mut rgb = Vec::with_capacity((pw * ph * 3) as usize);
            for px in bgrx.chunks_exact(4) {
                rgb.extend_from_slice(&[px[2], px[1], px[0]]);
            }
            let png = p.join(format!("{}.png", kind.file_stem()));
            if let Err(e) = std::fs::write(&png, png_rgb(&rgb, pw, ph)) {
                eprintln!("write {}: {e}", png.display());
                return 1;
            }
            println!("           preview {}", png.display());
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendered_slate_is_dark_with_light_text_and_survives_the_codec() {
        let bgra = render_bgra(SlateKind::NotRunning).unwrap();
        assert_eq!(bgra.len(), (WIDTH * HEIGHT * 4) as usize);
        let nv12 = bgra_to_nv12(&bgra, WIDTH, HEIGHT);
        let y = &nv12[..(WIDTH * HEIGHT) as usize];
        let dark = y.iter().filter(|&&v| v < 70).count();
        assert!(dark > y.len() * 9 / 10, "background must dominate");
        // text row band has bright pixels
        let band = &y[(700 * WIDTH) as usize..(720 * WIDTH) as usize];
        assert!(band.iter().any(|&v| v > 180), "no text found");
        let enc = slate::encode(&nv12, WIDTH, HEIGHT);
        assert!(enc.len() < 400_000, "slate is {} bytes", enc.len());
        assert_eq!(slate::decode(&enc).unwrap().nv12, nv12);
    }

    #[test]
    fn the_three_slates_differ() {
        let a = render_bgra(SlateKind::NotRunning).unwrap();
        let b = render_bgra(SlateKind::Paused).unwrap();
        let c = render_bgra(SlateKind::Starting).unwrap();
        assert_ne!(a, b);
        assert_ne!(b, c);
    }

    #[test]
    fn png_has_valid_structure() {
        let png = png_rgb(&[255, 0, 0, 0, 255, 0], 2, 1);
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
        assert_eq!(&png[12..16], b"IHDR");
        assert_eq!(&png[png.len() - 8..png.len() - 4], b"IEND");
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
    }
}
