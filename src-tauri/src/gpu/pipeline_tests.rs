//! End-to-end tests of the GPU pipeline on WARP (and on the hardware adapter when there is one): convert, fit,
//! encode and readback against CPU references.

use std::sync::Arc;

use windows::Win32::Graphics::Direct3D11::{
    ID3D11Texture2D, D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_READ, D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_NV12, DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC,
};

use super::convert::{
    cpu_layout, rgb_to_yuv_rows, yuv_to_rgb_rows, ColorRange, ColorSettings, ColorSpace, Converter,
    SourceData, SourceFrame,
};
use super::output::{fit_rect, EncodeFormat, Encoder, FitMode, OutputParams, OutputStage};
use super::pattern::{self, smpte_bars};
use super::readback::{PlaneDesc, ReadbackRing};
use super::{
    AdapterKind, FilterChain, FilterPass, GpuDevice, GpuError, GpuResult, GpuTimer, Pass, Pooled,
    Renderer, RgbaTexture, ShaderId,
};
use crate::capture::PixelFormat;

fn renderer(kind: AdapterKind) -> Option<Arc<Renderer>> {
    let gpu = GpuDevice::new(kind).ok()?;
    Some(Arc::new(Renderer::new(Arc::new(gpu)).expect("renderer")))
}

fn warp() -> Arc<Renderer> {
    renderer(AdapterKind::Warp).expect("WARP device")
}

/// Copy a texture through a staging texture and return tight rows (`w * bpt` bytes each).
fn read_texture(r: &Renderer, tex: &ID3D11Texture2D, w: u32, h: u32, bpt: u32) -> Vec<u8> {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { tex.GetDesc(&mut desc) };
    let staging = r
        .gpu
        .create_texture(&D3D11_TEXTURE2D_DESC {
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
            ..desc
        })
        .expect("staging");
    let ctx = &r.gpu.context;
    let mut m = D3D11_MAPPED_SUBRESOURCE::default();
    let mut out = vec![0u8; (w * h * bpt) as usize];
    unsafe {
        ctx.CopyResource(&staging, tex);
        ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut m))
            .expect("map");
        for y in 0..h as usize {
            let src = (m.pData as *const u8).add(y * m.RowPitch as usize);
            std::ptr::copy_nonoverlapping(
                src,
                out.as_mut_ptr().add(y * (w * bpt) as usize),
                (w * bpt) as usize,
            );
        }
        ctx.Unmap(&staging, 0);
    }
    out
}

fn read_rgba(r: &Renderer, t: &RgbaTexture) -> Vec<u8> {
    read_texture(r, &t.texture, t.width, t.height, 4)
}

fn rgba_to_rgb(rgba: &[u8]) -> Vec<u8> {
    rgba.chunks(4).flat_map(|p| [p[0], p[1], p[2]]).collect()
}

/// Encode to NV12 and read the planes back as one NV12 buffer with stride = width.
fn encode_nv12(r: &Arc<Renderer>, rgba: &RgbaTexture) -> Vec<u8> {
    let mut enc = Encoder::new(r.clone()).unwrap();
    let t = enc.encode(rgba, EncodeFormat::Nv12).unwrap();
    let (w, h) = (rgba.width, rgba.height);
    let mut out = read_texture(r, &t.planes[0].texture, w, h, 1);
    out.extend(read_texture(r, &t.planes[1].texture, w / 2, h / 2, 2));
    out
}

const BT709_LIMITED: ColorSettings = ColorSettings {
    space: Some(ColorSpace::Bt709),
    range: Some(ColorRange::Limited),
};

fn cpu_frame<'a>(
    data: &'a [u8],
    w: u32,
    h: u32,
    format: PixelFormat,
    stride: usize,
    color: ColorSettings,
) -> SourceFrame<'a> {
    let mut f = SourceFrame::cpu(w, h, format, data, stride as i32);
    f.color = color;
    f
}

#[test]
fn every_shader_compiles() {
    let r = warp();
    r.shaders.warm_up().expect("all shaders compile");
    // The cache hands back the same bytecode.
    let a = r
        .shaders
        .bytecode(ShaderId::ConvertNv12, "main", "ps_5_0")
        .unwrap();
    let b = r
        .shaders
        .bytecode(ShaderId::ConvertNv12, "main", "ps_5_0")
        .unwrap();
    assert!(Arc::ptr_eq(&a, &b));
    // A broken shader reports the compiler's message.
    let err = super::compile_shader(
        "broken",
        "float4 main() : SV_Target { return nope; }",
        "main",
        "ps_5_0",
    )
    .unwrap_err();
    assert!(
        matches!(&err, GpuError::Shader { log, .. } if log.contains("nope")),
        "{err}"
    );
}

/// The convert matrices against hand-checked values: (space, range, [y, cb, cr], expected rgb).
const KNOWN: &[(ColorSpace, ColorRange, [u8; 3], [u8; 3])] = &[
    // limited: black, white, mid grey
    (
        ColorSpace::Bt709,
        ColorRange::Limited,
        [16, 128, 128],
        [0, 0, 0],
    ),
    (
        ColorSpace::Bt709,
        ColorRange::Limited,
        [235, 128, 128],
        [255, 255, 255],
    ),
    (
        ColorSpace::Bt709,
        ColorRange::Limited,
        [126, 128, 128],
        [128, 128, 128],
    ),
    (
        ColorSpace::Bt601,
        ColorRange::Limited,
        [235, 128, 128],
        [255, 255, 255],
    ),
    // limited primaries
    (
        ColorSpace::Bt709,
        ColorRange::Limited,
        [63, 102, 240],
        [255, 0, 0],
    ),
    (
        ColorSpace::Bt709,
        ColorRange::Limited,
        [173, 42, 26],
        [0, 255, 0],
    ),
    (
        ColorSpace::Bt709,
        ColorRange::Limited,
        [32, 240, 118],
        [0, 0, 255],
    ),
    (
        ColorSpace::Bt601,
        ColorRange::Limited,
        [81, 90, 240],
        [255, 0, 0],
    ),
    (
        ColorSpace::Bt601,
        ColorRange::Limited,
        [145, 54, 34],
        [0, 255, 0],
    ),
    (
        ColorSpace::Bt601,
        ColorRange::Limited,
        [41, 240, 110],
        [0, 0, 255],
    ),
    // full range: no 16/235 scaling
    (
        ColorSpace::Bt709,
        ColorRange::Full,
        [0, 128, 128],
        [0, 0, 0],
    ),
    (
        ColorSpace::Bt709,
        ColorRange::Full,
        [255, 128, 128],
        [255, 255, 255],
    ),
    (
        ColorSpace::Bt709,
        ColorRange::Full,
        [128, 128, 128],
        [128, 128, 128],
    ),
    (
        ColorSpace::Bt709,
        ColorRange::Full,
        [54, 99, 255],
        [254, 0, 0],
    ),
    (
        ColorSpace::Bt601,
        ColorRange::Full,
        [76, 85, 255],
        [254, 0, 0],
    ),
    (
        ColorSpace::Bt601,
        ColorRange::Full,
        [149, 43, 21],
        [0, 254, 0],
    ),
];

#[test]
fn matrix_rows_match_known_values() {
    for &(space, range, yuv, want) in KNOWN {
        let rows = yuv_to_rgb_rows(space, range, 8);
        let s = [
            yuv[0] as f64 / 255.0,
            yuv[1] as f64 / 255.0,
            yuv[2] as f64 / 255.0,
        ];
        for i in 0..3 {
            let v =
                (rows[i][0] * s[0] + rows[i][1] * s[1] + rows[i][2] * s[2] + rows[i][3]) * 255.0;
            let v = v.clamp(0.0, 255.0);
            assert!(
                (v - want[i] as f64).abs() <= 1.6,
                "{space:?} {range:?} {yuv:?}: channel {i} is {v:.2}, want {}",
                want[i]
            );
        }
    }
}

#[test]
fn encode_matrix_inverts_the_decode_matrix() {
    for space in [ColorSpace::Bt601, ColorSpace::Bt709] {
        for range in [ColorRange::Limited, ColorRange::Full] {
            let d = yuv_to_rgb_rows(space, range, 8);
            let e = rgb_to_yuv_rows(space, range);
            for rgb in [[0.2, 0.5, 0.9], [1.0, 0.0, 0.3], [0.0, 0.0, 0.0]] {
                let yuv: Vec<f64> = (0..3)
                    .map(|i| e[i][0] * rgb[0] + e[i][1] * rgb[1] + e[i][2] * rgb[2] + e[i][3])
                    .collect();
                for i in 0..3 {
                    let back = d[i][0] * yuv[0] + d[i][1] * yuv[1] + d[i][2] * yuv[2] + d[i][3];
                    assert!((back - rgb[i]).abs() < 1e-9, "{space:?} {range:?}");
                }
            }
        }
    }
}

/// Run the known values through the real shader (NV12, a 16x16 solid frame).
fn known_values_on(r: &Arc<Renderer>) {
    let mut conv = Converter::new(r.clone()).unwrap();
    for &(space, range, yuv, want) in KNOWN {
        let (w, h) = (16u32, 16u32);
        let mut buf = vec![yuv[0]; (w * h) as usize];
        buf.extend(std::iter::repeat_n([yuv[1], yuv[2]], (w * h / 4) as usize).flatten());
        let color = ColorSettings {
            space: Some(space),
            range: Some(range),
        };
        let out = conv
            .convert(
                &cpu_frame(&buf, w, h, PixelFormat::Nv12, w as usize, color),
                false,
            )
            .unwrap();
        let px = read_rgba(r, &out);
        for i in 0..3 {
            assert!(
                (px[i] as i32 - want[i] as i32).abs() <= 2,
                "{space:?} {range:?} {yuv:?}: got {:?}, want {want:?}",
                &px[..4]
            );
        }
        assert_eq!(px[3], 255);
    }
}

#[test]
fn known_values_through_the_shader_warp() {
    known_values_on(&warp());
}

/// Bars in every input format convert to RGBA near the original and encode back to NV12 within 2 code values of the
/// CPU reference NV12.
fn bars_round_trip_on(r: &Arc<Renderer>) {
    let (w, h) = (320u32, 180u32);
    let rgb = smpte_bars(w, h);
    let (space, range) = (ColorSpace::Bt709, ColorRange::Limited);
    let reference_nv12 = pattern::pack_nv12(&rgb, w, h, w as usize, h, space, range);
    let mut conv = Converter::new(r.clone()).unwrap();

    let cases: Vec<(PixelFormat, Vec<u8>, usize, i32)> = vec![
        (
            PixelFormat::Nv12,
            pattern::pack_nv12(&rgb, w, h, w as usize, h, space, range),
            w as usize,
            3,
        ),
        (
            PixelFormat::Yuy2,
            pattern::pack_yuy2(&rgb, w, h, w as usize * 2, space, range),
            w as usize * 2,
            3,
        ),
        (
            PixelFormat::I420,
            pattern::pack_i420(&rgb, w, h, w as usize, space, range, false),
            w as usize,
            3,
        ),
        (
            PixelFormat::Yv12,
            pattern::pack_i420(&rgb, w, h, w as usize, space, range, true),
            w as usize,
            3,
        ),
        (
            PixelFormat::P010,
            pattern::pack_p010(&rgb, w, h, w as usize * 2, space, range),
            w as usize * 2,
            2,
        ),
        (
            PixelFormat::Xrgb,
            pattern::pack_bgra(&rgb, w, h, w as usize * 4, 255),
            w as usize * 4,
            0,
        ),
        (
            PixelFormat::Argb,
            pattern::pack_bgra(&rgb, w, h, w as usize * 4, 255),
            w as usize * 4,
            0,
        ),
    ];
    for (format, data, stride, rgb_tolerance) in cases {
        let out = conv
            .convert(
                &cpu_frame(&data, w, h, format, stride, BT709_LIMITED),
                false,
            )
            .unwrap_or_else(|e| panic!("{format}: {e}"));
        let rgba = read_rgba(r, &out);
        let err = pattern::max_diff(&rgba_to_rgb(&rgba), &rgb, 3, |_| false) as i32;
        assert!(
            err <= rgb_tolerance,
            "{format}: RGB differs from the bars by {err}"
        );
        assert!(
            rgba.chunks(4).all(|p| p[3] == 255),
            "{format}: alpha must be 1"
        );

        let nv12 = encode_nv12(r, &out);
        let diff = pattern::max_diff(&nv12, &reference_nv12, 1, |_| false);
        assert!(diff <= 2, "{format}: NV12 round trip is off by {diff}");
    }
}

#[test]
fn bars_in_every_format_round_trip_to_nv12_warp() {
    bars_round_trip_on(&warp());
}

#[test]
fn known_values_and_bars_on_the_hardware_adapter() {
    match renderer(AdapterKind::Hardware) {
        Some(r) => {
            eprintln!("hardware adapter: {}", r.gpu.adapter_name);
            known_values_on(&r);
            bars_round_trip_on(&r);
        }
        None => eprintln!("no hardware adapter here"),
    }
}

#[test]
fn argb_keeps_alpha_and_xrgb_ignores_it() {
    let r = warp();
    let mut conv = Converter::new(r.clone()).unwrap();
    // two pixels: B,G,R,A
    let data = [10u8, 20, 30, 128, 40, 50, 60, 0];
    let argb = conv
        .convert(
            &cpu_frame(&data, 2, 1, PixelFormat::Argb, 8, ColorSettings::default()),
            false,
        )
        .unwrap();
    assert_eq!(read_rgba(&r, &argb), vec![30, 20, 10, 128, 60, 50, 40, 0]);
    let xrgb = conv
        .convert(
            &cpu_frame(&data, 2, 1, PixelFormat::Xrgb, 8, ColorSettings::default()),
            false,
        )
        .unwrap();
    assert_eq!(read_rgba(&r, &xrgb), vec![30, 20, 10, 255, 60, 50, 40, 255]);
}

#[test]
fn flip_and_bottom_up_images() {
    let r = warp();
    let mut conv = Converter::new(r.clone()).unwrap();
    // 2x2 BGRX: top row red, bottom row blue.
    let top_down = [
        0u8, 0, 255, 255, 0, 0, 255, 255, 255, 0, 0, 255, 255, 0, 0, 255,
    ];
    let up = conv
        .convert(
            &cpu_frame(
                &top_down,
                2,
                2,
                PixelFormat::Xrgb,
                8,
                ColorSettings::default(),
            ),
            false,
        )
        .unwrap();
    let px = read_rgba(&r, &up);
    assert_eq!(&px[0..3], &[255, 0, 0]);
    assert_eq!(&px[8..11], &[0, 0, 255]);
    let flipped = conv
        .convert(
            &cpu_frame(
                &top_down,
                2,
                2,
                PixelFormat::Xrgb,
                8,
                ColorSettings::default(),
            ),
            true,
        )
        .unwrap();
    let px = read_rgba(&r, &flipped);
    assert_eq!(&px[0..3], &[0, 0, 255]);
    assert_eq!(&px[8..11], &[255, 0, 0]);
    // A bottom-up buffer (negative stride, memory starts with the last row) shows upright: memory is blue, red.
    let bottom_up: Vec<u8> = [&top_down[8..16], &top_down[0..8]].concat();
    let mut f = SourceFrame::cpu(2, 2, PixelFormat::Xrgb, &bottom_up, -8);
    f.color = ColorSettings::default();
    let px = read_rgba(&r, &conv.convert(&f, false).unwrap());
    assert_eq!(&px[0..3], &[255, 0, 0], "top row must be red");
    assert_eq!(&px[8..11], &[0, 0, 255]);
}

#[test]
fn padded_pitches_and_luma_rows_are_honoured() {
    let r = warp();
    let (w, h) = (322u32, 180u32);
    let rgb = smpte_bars(w, h);
    let (space, range) = (ColorSpace::Bt709, ColorRange::Limited);
    let mut conv = Converter::new(r.clone()).unwrap();
    let tight = conv
        .convert(
            &cpu_frame(
                &pattern::pack_nv12(&rgb, w, h, w as usize, h, space, range),
                w,
                h,
                PixelFormat::Nv12,
                w as usize,
                BT709_LIMITED,
            ),
            false,
        )
        .unwrap();
    let want = read_rgba(&r, &tight);
    // Odd stride and 8 padding rows before the chroma plane (a decoder that aligns the height to 16).
    let (stride, luma_rows) = (w as usize + 13, h + 12);
    let padded = pattern::pack_nv12(&rgb, w, h, stride, luma_rows, space, range);
    let mut f = cpu_frame(&padded, w, h, PixelFormat::Nv12, stride, BT709_LIMITED);
    f.luma_rows = luma_rows;
    let got = read_rgba(&r, &conv.convert(&f, false).unwrap());
    assert_eq!(got, want);
    // Without telling the converter about the padding, the chroma is read from the wrong place.
    f.luma_rows = 0;
    let wrong = read_rgba(&r, &conv.convert(&f, false).unwrap());
    assert_ne!(wrong, want);

    let layout = cpu_layout(PixelFormat::Nv12, w, h, stride, luma_rows).unwrap();
    assert_eq!(layout[1].offset, stride * luma_rows as usize);
    assert_eq!((layout[1].rows, layout[1].row_bytes), (90, 322));
    // A buffer that is too short is refused, not read past.
    let short = &padded[..stride * luma_rows as usize + stride * 40];
    let mut f = cpu_frame(short, w, h, PixelFormat::Nv12, stride, BT709_LIMITED);
    f.luma_rows = luma_rows;
    assert!(matches!(conv.convert(&f, false), Err(GpuError::Invalid(_))));
}

#[test]
fn a_texture_frame_is_copied_into_our_own_texture() {
    // NV12 texture as a decoder would hand it (here also shader readable; the copy path is the same).
    let r = warp();
    let (w, h) = (64u32, 48u32);
    let rgb = smpte_bars(w, h);
    let nv12 = pattern::pack_nv12(
        &rgb,
        w,
        h,
        w as usize,
        h,
        ColorSpace::Bt709,
        ColorRange::Limited,
    );
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let init = D3D11_SUBRESOURCE_DATA {
        pSysMem: nv12.as_ptr() as *const _,
        SysMemPitch: w,
        SysMemSlicePitch: 0,
    };
    let mut tex = None;
    if unsafe {
        r.gpu
            .device
            .CreateTexture2D(&desc, Some(&init), Some(&mut tex))
    }
    .is_err()
    {
        eprintln!("this device cannot make NV12 textures; texture path not tested");
        return;
    }
    let tex = tex.unwrap();
    let mut conv = Converter::new(r.clone()).unwrap();
    let out = conv.convert(
        &SourceFrame {
            width: w,
            height: h,
            format: PixelFormat::Nv12,
            data: SourceData::Texture {
                texture: &tex,
                subresource: 0,
            },
            color: BT709_LIMITED,
            luma_rows: 0,
        },
        false,
    );
    match out {
        Ok(out) => {
            let err = pattern::max_diff(&rgba_to_rgb(&read_rgba(&r, &out)), &rgb, 3, |_| false);
            assert!(err <= 3, "texture path differs by {err}");
        }
        Err(e) => eprintln!("texture path unavailable on WARP: {e}"),
    }
}

#[test]
fn fit_fill_stretch_place_the_picture() {
    let r = warp();
    let mut conv = Converter::new(r.clone()).unwrap();
    let (sw, sh) = (160u32, 120u32); // 4:3
    let rgb = smpte_bars(sw, sh);
    let bgra = pattern::pack_bgra(&rgb, sw, sh, sw as usize * 4, 255);
    let src = conv
        .convert(
            &cpu_frame(
                &bgra,
                sw,
                sh,
                PixelFormat::Xrgb,
                sw as usize * 4,
                ColorSettings::default(),
            ),
            false,
        )
        .unwrap();
    let stage = OutputStage::new(r.clone()).unwrap();
    let (ow, oh) = (320u32, 180u32); // 16:9
    let bg = [0.2f32, 0.4, 0.6];
    let bg8 = [51u8, 102, 153];
    let run = |fit| {
        let out = stage
            .run(
                &src,
                &OutputParams {
                    width: ow,
                    height: oh,
                    fit,
                    background: bg,
                },
            )
            .unwrap();
        read_rgba(&r, &out)
    };
    let at = |px: &[u8], x: u32, y: u32| {
        let o = ((y * ow + x) * 4) as usize;
        [px[o], px[o + 1], px[o + 2]]
    };
    let src_at = |x: u32, y: u32| {
        let o = ((y * sw + x) * 3) as usize;
        [rgb[o], rgb[o + 1], rgb[o + 2]]
    };

    // Fit: 4:3 into 16:9 is 240x180 centred, 40 columns of background on each side.
    let rect = fit_rect((sw, sh), (ow, oh), FitMode::Fit);
    assert_eq!((rect.x, rect.w), (40, 240));
    let px = run(FitMode::Fit);
    for y in [0, 90, 179] {
        for x in [0, 39, 280, 319] {
            assert_eq!(at(&px, x, y), bg8, "bar at ({x},{y})");
        }
    }
    // inside the picture, bars sit where the scaled source puts them (centre of the first and last bars)
    for (ox, oy) in [(40 + 10, 10), (40 + 120, 20), (40 + 229, 100)] {
        let (sx, sy) = ((ox - 40) * sw / 240, oy * sh / 180);
        let got = at(&px, ox, oy);
        let want = src_at(sx, sy);
        assert!(
            got.iter()
                .zip(want)
                .all(|(a, b)| (*a as i32 - b as i32).abs() <= 3),
            "fit ({ox},{oy}): {got:?} vs {want:?}"
        );
    }

    // Fill: the picture covers everything; the first column shows the source's first bar, no background.
    let px = run(FitMode::Fill);
    assert!((0..oh).all(|y| at(&px, 0, y) != bg8 && at(&px, ow - 1, y) != bg8));
    assert_eq!(at(&px, 5, 5), src_at(0, 0));

    // Stretch: corners are the source corners.
    let px = run(FitMode::Stretch);
    assert_eq!(at(&px, 0, 0), src_at(0, 0));
    assert_eq!(at(&px, ow - 1, 0), src_at(sw - 1, 0));
    assert_eq!(at(&px, 0, oh - 1), src_at(0, sh - 1));
    assert_eq!(at(&px, ow - 1, oh - 1), src_at(sw - 1, sh - 1));
}

#[test]
fn identical_size_is_copied_exactly() {
    let r = warp();
    let mut conv = Converter::new(r.clone()).unwrap();
    let (w, h) = (96u32, 64u32);
    let rgb = smpte_bars(w, h);
    let bgra = pattern::pack_bgra(&rgb, w, h, w as usize * 4, 255);
    let src = conv
        .convert(
            &cpu_frame(
                &bgra,
                w,
                h,
                PixelFormat::Xrgb,
                w as usize * 4,
                ColorSettings::default(),
            ),
            false,
        )
        .unwrap();
    let stage = OutputStage::new(r.clone()).unwrap();
    let out = stage
        .run(
            &src,
            &OutputParams {
                width: w,
                height: h,
                fit: FitMode::Fit,
                background: [0.0; 3],
            },
        )
        .unwrap();
    assert_eq!(read_rgba(&r, &out), read_rgba(&r, &src));
}

#[test]
fn bgra_encode_swizzles_to_bgrx_bytes() {
    let r = warp();
    let mut conv = Converter::new(r.clone()).unwrap();
    // one orange pixel pair
    let data = [10u8, 120, 240, 255, 10, 120, 240, 255]; // B,G,R,X
    let src = conv
        .convert(
            &cpu_frame(&data, 2, 1, PixelFormat::Xrgb, 8, ColorSettings::default()),
            false,
        )
        .unwrap();
    let mut enc = Encoder::new(r.clone()).unwrap();
    let t = enc.encode(&src, EncodeFormat::Bgra).unwrap();
    let out = read_texture(&r, &t.planes[0].texture, 2, 1, 4);
    assert_eq!(out, vec![10, 120, 240, 255, 10, 120, 240, 255]);
}

fn fill_rgba(r: &Renderer, tex: &RgbaTexture, value: impl Fn(u32, u32) -> [u8; 4]) {
    let mut data = Vec::new();
    for y in 0..tex.height {
        for x in 0..tex.width {
            data.extend_from_slice(&value(x, y));
        }
    }
    unsafe {
        r.gpu.context.UpdateSubresource(
            &tex.texture,
            0,
            None,
            data.as_ptr() as *const _,
            tex.width * 4,
            0,
        )
    };
}

#[test]
fn the_readback_ring_returns_frame_n_after_frame_n_plus_one_is_queued() {
    ring_on(&warp());
    match renderer(AdapterKind::Hardware) {
        Some(r) => ring_on(&r),
        None => eprintln!("no hardware adapter here"),
    }
}

fn ring_on(r: &Arc<Renderer>) {
    let r = r.clone();
    // 301 pixels: 1204 bytes per row, which the driver pads to its own pitch.
    let (w, h) = (301u32, 17u32);
    let desc = PlaneDesc {
        format: DXGI_FORMAT_R8G8B8A8_UNORM,
        width: w,
        height: h,
        bytes_per_texel: 4,
    };
    let mut ring: ReadbackRing<u32> = ReadbackRing::new(r.gpu.clone(), &[desc]).unwrap();
    let tex = r.texture(w, h).unwrap();
    let pixel = |n: u32, x: u32, y: u32| [n as u8, (x % 251) as u8, (y % 251) as u8, 255];
    let check = |planes: &[super::readback::MappedPlane<'_>], n: u32| {
        assert_eq!(planes[0].row_bytes, (w * 4) as usize);
        assert!(planes[0].row_pitch >= planes[0].row_bytes);
        for y in 0..h {
            let row = planes[0].row(y as usize);
            for x in 0..w {
                assert_eq!(
                    &row[(x * 4) as usize..(x * 4 + 4) as usize],
                    &pixel(n, x, y),
                    "frame {n} at ({x},{y})"
                );
            }
        }
        planes[0].row_pitch
    };

    // frame 0 submitted, frame 1 submitted, then frame 0 is mapped: its pixels, not frame 1's
    fill_rgba(&r, &tex, |x, y| pixel(0, x, y));
    assert!(ring.submit(&[&tex.texture], 0).unwrap().is_none());
    fill_rgba(&r, &tex, |x, y| pixel(1, x, y));
    assert!(ring.submit(&[&tex.texture], 1).unwrap().is_none());
    assert_eq!(ring.pending(), 2);
    let pitch = ring
        .poll(true, |p, tag| {
            assert_eq!(tag, 0);
            check(p, 0)
        })
        .unwrap()
        .expect("frame 0");
    eprintln!("staging row pitch {pitch} for {} row bytes", w * 4);
    ring.poll(true, |p, tag| {
        assert_eq!(tag, 1);
        check(p, 1);
    })
    .unwrap()
    .expect("frame 1");
    assert!(
        ring.poll(false, |_, _| ()).unwrap().is_none(),
        "nothing pending"
    );

    // A full ring drops the oldest frame instead of growing.
    for n in 10..14u32 {
        fill_rgba(&r, &tex, |x, y| pixel(n, x, y));
        let dropped = ring.submit(&[&tex.texture], n).unwrap();
        assert_eq!(dropped, (n == 13).then_some(10));
    }
    assert_eq!(ring.dropped, 1);
    assert_eq!(ring.pending(), 3);
    for n in 11..14u32 {
        ring.poll(true, |p, tag| {
            assert_eq!(tag, n);
            check(p, n);
        })
        .unwrap()
        .expect("pending frame");
    }
    // Wrong plane count is an error, not a crash.
    assert!(ring.submit(&[], 0).is_err());
}

struct Invert;

impl FilterPass for Invert {
    fn name(&self) -> &str {
        "invert"
    }

    fn apply(&mut self, r: &Renderer, input: &RgbaTexture, output: &RgbaTexture) -> GpuResult<()> {
        let code = super::compile_shader(
            "invert",
            "Texture2D<float4> t : register(t0);\nfloat4 main(float4 p : SV_Position) : SV_Target { float4 c = t.Load(int3(int2(p.xy),0)); return float4(1.0 - c.rgb, c.a); }",
            "main",
            "ps_5_0",
        )?;
        let mut ps = None;
        unsafe { r.gpu.device.CreatePixelShader(&code, None, Some(&mut ps))? };
        r.draw(&Pass {
            ps: ps.as_ref().unwrap(),
            srvs: &[Some(input.srv.clone())],
            constants: None,
            target: &output.rtv,
            width: output.width,
            height: output.height,
        });
        Ok(())
    }
}

#[test]
fn a_filter_pass_plugs_in_between_convert_and_output() {
    let r = warp();
    let mut conv = Converter::new(r.clone()).unwrap();
    let data = [10u8, 120, 240, 255, 0, 0, 0, 255];
    let src = conv
        .convert(
            &cpu_frame(&data, 2, 1, PixelFormat::Xrgb, 8, ColorSettings::default()),
            false,
        )
        .unwrap();
    let mut chain = FilterChain::new();
    assert!(chain.is_empty());
    // identity: the very same texture comes back
    let same = chain.run(&r, src).unwrap();
    assert_eq!(read_rgba(&r, &same), vec![240, 120, 10, 255, 0, 0, 0, 255]);
    chain.push(Box::new(Invert));
    chain.push(Box::new(Invert));
    chain.push(Box::new(Invert));
    let out: Pooled = chain.run(&r, same).unwrap();
    assert_eq!(
        read_rgba(&r, &out),
        vec![15, 135, 245, 255, 255, 255, 255, 255]
    );
    // ping-pong textures went back to the pool
    drop(out);
    assert!(r.pool.free_count() >= 2);
}

#[test]
fn the_texture_pool_reuses_textures_by_size() {
    let r = warp();
    let a = r.texture(64, 32).unwrap();
    let raw = a.texture.clone();
    drop(a);
    let b = r.texture(64, 32).unwrap();
    assert_eq!(b.texture, raw, "same size comes back from the pool");
    let c = r.texture(64, 34).unwrap();
    assert_ne!(c.texture, raw);
    assert!(r.texture(0, 10).is_err());
    drop((b, c));
    r.pool.clear();
    assert_eq!(r.pool.free_count(), 0);
}

#[test]
fn the_gpu_timer_reports_a_time() {
    let r = warp();
    let Ok(mut timer) = GpuTimer::new(&r.gpu.device) else {
        eprintln!("no timestamp queries on this device");
        return;
    };
    let tex = r.texture(256, 256).unwrap();
    assert!(timer.begin(&r.gpu.context));
    r.clear(&tex.rtv, [1.0, 0.0, 0.0, 1.0]);
    timer.end(&r.gpu.context);
    r.flush();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while timer.poll(&r.gpu.context).is_none() {
        assert!(
            std::time::Instant::now() < deadline,
            "the timer never produced a result"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(timer.last_ms.unwrap() >= 0.0);
}
