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
    AdapterKind, GpuDevice, GpuError, GpuTimer, Renderer, RgbaTexture, ShaderId,
};
use crate::capture::PixelFormat;

fn renderer(kind: AdapterKind) -> Option<Arc<Renderer>> {
    let gpu = GpuDevice::new(kind).ok()?;
    Some(Arc::new(Renderer::new(Arc::new(gpu)).expect("renderer")))
}

pub(crate) fn warp() -> Arc<Renderer> {
    renderer(AdapterKind::Warp).expect("WARP device")
}

/// Copy a texture through a staging texture and return tight rows (`w * bpt` bytes each).
pub(crate) fn read_texture(r: &Renderer, tex: &ID3D11Texture2D, w: u32, h: u32, bpt: u32) -> Vec<u8> {
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

pub(crate) fn read_rgba(r: &Renderer, t: &RgbaTexture) -> Vec<u8> {
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
    assert_eq!(super::precompiled::tracking::runtime_compiles(), 0);
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

/// What engine creation and the GPU rebuild warm up (`Engine::build`, `Engine::rebuild_gpu`).
fn warm_everything(r: &Renderer) {
    r.shaders.warm_up().expect("pipeline shaders");
    crate::fx::warm_up(r).expect("filter shaders");
    crate::meter::warm_up(r).expect("meter shaders");
}

#[test]
fn built_in_shaders_are_not_compiled_at_run_time() {
    use super::precompiled::{tracking, PRECOMPILED};
    let r = warp();
    warm_everything(&r);
    assert_eq!(tracking::runtime_compiles(), 0, "a built-in shader missed the embedded table");
    // Every program of the table is one the engine asks for (a stale entry would be dead weight).
    let served = tracking::served();
    for p in PRECOMPILED {
        assert!(
            served.iter().any(|(src, entry, target)| {
                super::precompiled::find(src, entry, target).is_some_and(|f| std::ptr::eq(f, p))
            }),
            "{} {} is in the table but nothing uses it",
            p.name,
            p.entry
        );
    }
}

/// The embedded bytecode is what the run-time path produces for the same source: same compiler, profile, entry
/// point and flags, so the pixels are the same.
#[test]
fn precompiled_bytecode_equals_a_run_time_compile() {
    use super::precompiled::{find, tracking};
    let r = warp();
    warm_everything(&r);
    let served = tracking::served();
    assert!(!served.is_empty());
    for (src, entry, target) in served {
        let table = find(&src, &entry, &target).expect("served from the table");
        let fresh = super::compile_shader(table.name, &src, &entry, &target).expect("run-time compile");
        assert!(fresh == table.code, "{} {entry}: the embedded bytecode differs from a run-time compile", table.name);
    }
}

/// Not a check: prints what the engine's shader setup costs with the embedded bytecode and with the old run-time
/// compile. `cargo test --release -p better-cam --lib shader_setup_time -- --ignored --nocapture`
#[test]
#[ignore = "measurement"]
fn shader_setup_time() {
    use super::precompiled::tracking;
    use std::time::Instant;
    for round in 0..3 {
        for forced in [true, false] {
            tracking::force_runtime(forced);
            let t = Instant::now();
            let gpu = Arc::new(GpuDevice::new(AdapterKind::Hardware).expect("hardware device"));
            let device = t.elapsed();
            let t = Instant::now();
            let r = Arc::new(Renderer::new(gpu).expect("renderer"));
            let create = t.elapsed();
            let t = Instant::now();
            warm_everything(&r);
            let shaders = t.elapsed();
            eprintln!(
                "round {round} {:>10}: device {device:>9.1?}  Renderer::new {create:>9.1?}  shader warm-up {shaders:>9.1?}  (engine create / GPU rebuild shader share)",
                if forced { "run time" } else { "embedded" },
            );
        }
    }
    tracking::force_runtime(false);
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

pub(crate) fn fill_rgba(r: &Renderer, tex: &RgbaTexture, value: impl Fn(u32, u32) -> [u8; 4]) {
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

#[test]
fn translucent_pixels_blend_over_the_background_in_linear_light() {
    let r = warp();
    let stage = OutputStage::new(r.clone()).unwrap();
    let src = r.texture(4, 1).unwrap();
    let px = [
        [255u8, 255, 255, 0],
        [255, 255, 255, 255],
        [40, 200, 90, 128],
        [255, 255, 255, 64],
    ];
    fill_rgba(&r, &src, |x, _| px[x as usize]);
    let out = stage
        .run(
            &src,
            &OutputParams {
                width: 4,
                height: 1,
                fit: FitMode::Fit,
                background: [0.2, 0.4, 0.6],
            },
        )
        .unwrap();
    let got = read_rgba(&r, &out);
    let dec = |c: f64| {
        let u = c / 255.0;
        if u <= 0.04045 {
            u / 12.92
        } else {
            ((u + 0.055) / 1.055).powf(2.4)
        }
    };
    let enc = |l: f64| {
        let u = if l <= 0.0031308 {
            12.92 * l
        } else {
            1.055 * l.powf(1.0 / 2.4) - 0.055
        };
        u * 255.0
    };
    let bg = [51.0, 102.0, 153.0];
    // clear shows the background exactly, opaque shows the picture exactly
    assert_eq!(&got[0..4], &[51, 102, 153, 255]);
    assert_eq!(&got[4..8], &[255, 255, 255, 255]);
    for x in [2usize, 3] {
        let a = px[x][3] as f64 / 255.0;
        for k in 0..3 {
            let want = enc(a * dec(px[x][k] as f64) + (1.0 - a) * dec(bg[k]));
            assert!(
                (got[x * 4 + k] as f64 - want).abs() <= 1.5,
                "pixel {x} channel {k}: {} vs {want}",
                got[x * 4 + k]
            );
        }
    }
}

#[test]
fn scaling_weights_colour_by_alpha_so_clear_texels_do_not_leak() {
    let r = warp();
    let stage = OutputStage::new(r.clone()).unwrap();
    // left: opaque red, right: fully clear blue. A plain bilinear filter would paint purple between them.
    let src = r.texture(2, 1).unwrap();
    fill_rgba(&r, &src, |x, _| if x == 0 { [255, 0, 0, 255] } else { [0, 0, 255, 0] });
    let out = stage
        .run(
            &src,
            &OutputParams {
                width: 16,
                height: 1,
                fit: FitMode::Stretch,
                background: [0.0, 0.0, 0.0],
            },
        )
        .unwrap();
    let got = read_rgba(&r, &out);
    for x in 0..16usize {
        assert_eq!(got[x * 4 + 2], 0, "pixel {x} has blue in it: {:?}", &got[x * 4..x * 4 + 4]);
    }
    assert_eq!(&got[0..3], &[255, 0, 0]);
    assert_eq!(&got[15 * 4..15 * 4 + 3], &[0, 0, 0]);
    // the edge fades from red to the background
    assert!(got[7 * 4] > got[9 * 4], "red fades towards the clear side");
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

// ---------------------------------------------------------------------------------------------------------------
// sRGB views of the pool textures

const COPY_PS: &str = "Texture2D<float4> t : register(t0);\n\
    float4 main(float4 p : SV_Position) : SV_Target { return t.Load(int3(int2(p.xy), 0)); }";

fn srgb_decode(code: f64) -> f64 {
    let u = code / 255.0;
    if u <= 0.04045 {
        u / 12.92
    } else {
        ((u + 0.055) / 1.055).powf(2.4)
    }
}

fn srgb_encode(lin: f64) -> f64 {
    let c = lin.clamp(0.0, 1.0);
    let u = if c <= 0.0031308 { 12.92 * c } else { 1.055 * c.powf(1.0 / 2.4) - 0.055 };
    u * 255.0
}

/// Every code value, every channel (alpha equal to the red code).
fn ramp(r: &Arc<Renderer>) -> super::Pooled {
    let t = r.texture(256, 1).unwrap();
    fill_rgba(r, &t, |x, _| [x as u8, x as u8, (255 - x) as u8, x as u8]);
    t
}

fn view_test_on(r: &Arc<Renderer>) {
    let ps = r.shaders.pixel_source("test_copy_load", COPY_PS).unwrap();
    use windows::Win32::Graphics::Direct3D11::{ID3D11RenderTargetView, ID3D11ShaderResourceView};
    let draw = |src: &ID3D11ShaderResourceView, target: &ID3D11RenderTargetView| {
        r.draw(&super::Pass {
            ps: &ps,
            srvs: &[Some(src.clone())],
            constants: None,
            target,
            width: 256,
            height: 1,
        });
    };
    let input = ramp(r);

    // The decoded values in float: what the sampler hands the shader.
    {
        use windows::Win32::Graphics::Direct3D11::D3D11_BIND_RENDER_TARGET;
        use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R32G32B32A32_FLOAT;
        let tex = r
            .gpu
            .create_texture(&D3D11_TEXTURE2D_DESC {
                Width: 256,
                Height: 1,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_R32G32B32A32_FLOAT,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            })
            .unwrap();
        let rtv = r.gpu.create_rtv(&tex).unwrap();
        draw(&input.srv_srgb, &rtv);
        let bytes = read_texture(r, &tex, 256, 1, 16);
        let mut worst = 0.0f64;
        for x in 0..256usize {
            let v = f32::from_le_bytes(bytes[x * 16..x * 16 + 4].try_into().unwrap()) as f64;
            worst = worst.max((v - srgb_decode(x as f64)).abs());
        }
        // The curve is exact on WARP. NVIDIA's texture unit hands over about 7 bits of mantissa for a decoded texel
        // (0.9375, 0.945312, 0.953125 ... for the codes 248 to 250: steps of 1/128 near 1), so up to 0.004 off at the
        // top: less than half a code once the value is encoded again.
        eprintln!("{}: worst decode error in float {worst:.6}", r.gpu.adapter_name);
        assert!(worst <= 0.004, "decoded values are {worst} from the sRGB curve");
    }

    // Reading through the sRGB view hands the shader linear light (a `Load` too, not only a `Sample`); alpha is not
    // touched. The result is stored as plain 8 bit linear values through the UNORM view, so it is the decode table.
    let out = r.texture(256, 1).unwrap();
    draw(&input.srv_srgb, &out.rtv);
    let got = read_rgba(r, &out);
    let mut worst = (0.0f64, 0usize);
    for x in 0..256usize {
        for (c, code) in [x, x, 255 - x].into_iter().enumerate() {
            let want = srgb_decode(code as f64) * 255.0;
            let d = (got[x * 4 + c] as f64 - want).abs();
            if d > worst.0 {
                worst = (d, code);
            }
        }
        assert_eq!(got[x * 4 + 3], x as u8, "alpha is never converted");
    }
    // (one 8 bit linear code is allowed for the texture unit's precision above; WARP is within the rounding)
    assert!(worst.0 <= 1.05, "decode on read is {} codes from the table (code {})", worst.0, worst.1);

    // Writing through the sRGB view encodes what the shader returns: the codes of a plain 8 bit copy are the encode.
    let out = r.texture(256, 1).unwrap();
    draw(&input.srv, &out.rtv_srgb);
    let got = read_rgba(r, &out);
    let mut worst = 0.0f64;
    for x in 0..256usize {
        for (c, code) in [x, x, 255 - x].into_iter().enumerate() {
            let want = srgb_encode(code as f64 / 255.0);
            worst = worst.max((got[x * 4 + c] as f64 - want).abs());
        }
        assert_eq!(got[x * 4 + 3], x as u8, "alpha is never converted");
    }
    assert!(worst <= 0.6, "encode on write is {worst} codes from the formula");

    // Decoding on read and encoding on write give back the codes exactly: a filter that leaves a texel alone leaves
    // its code alone.
    let out = r.texture(256, 1).unwrap();
    draw(&input.srv_srgb, &out.rtv_srgb);
    assert_eq!(read_rgba(r, &out), read_rgba(r, &input), "the round trip through both sRGB views is exact");
}

#[test]
fn pool_textures_read_linear_and_write_encoded_through_their_srgb_views_warp() {
    view_test_on(&warp());
}

#[test]
fn pool_textures_read_linear_and_write_encoded_through_their_srgb_views_hardware() {
    match renderer(AdapterKind::Hardware) {
        Some(r) => view_test_on(&r),
        None => eprintln!("no hardware adapter here"),
    }
}

#[test]
fn a_typeless_pool_texture_copies_to_and_from_typed_textures() {
    // The readback rings, the preview and NVIDIA's interop copy pool textures into `R8G8B8A8_UNORM` textures.
    let r = warp();
    let a = r.texture(8, 2).unwrap();
    fill_rgba(&r, &a, |x, y| [x as u8 * 30, y as u8 * 100, 7, 255]);
    let typed = r
        .gpu
        .create_texture(&D3D11_TEXTURE2D_DESC {
            Width: 8,
            Height: 2,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        })
        .unwrap();
    unsafe { r.gpu.context.CopyResource(&typed, &a.texture) };
    assert_eq!(read_texture(&r, &typed, 8, 2, 4), read_rgba(&r, &a));
    let b = r.texture(8, 2).unwrap();
    unsafe { r.gpu.context.CopyResource(&b.texture, &typed) };
    assert_eq!(read_rgba(&r, &b), read_rgba(&r, &a));
}

// ---------------------------------------------------------------------------------------------------------------
// NV12 in one dispatch

/// A picture with every kind of value: noise, ramps, saturated colours, black and white.
fn noisy(w: u32, h: u32) -> Vec<[u8; 4]> {
    let mut seed = 0x9e37_79b9u32;
    (0..w * h)
        .map(|i| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let n = (seed >> 24) as u8;
            let (x, y) = (i % w, i / w);
            match i % 7 {
                0 => [n, (x * 255 / w) as u8, (y * 255 / h) as u8, 255],
                1 => [255, 255, 255, 255],
                2 => [0, 0, 0, 255],
                3 => [255, 0, 0, 255],
                _ => [n, n.wrapping_mul(3), n.wrapping_add(91), 255],
            }
        })
        .collect()
}

fn encode_both_ways(r: &Arc<Renderer>, w: u32, h: u32) {
    let px = noisy(w, h);
    let src = r.texture(w, h).unwrap();
    fill_rgba(r, &src, |x, y| px[(y * w + x) as usize]);
    let planes = |enc: &mut Encoder| -> (bool, Vec<Vec<u8>>) {
        let t = enc.encode(&src, EncodeFormat::Nv12).unwrap();
        let bytes = t
            .planes
            .iter()
            .map(|p| read_texture(r, &p.texture, p.desc.width, p.desc.height, p.desc.bytes_per_texel))
            .collect();
        (t.packed, bytes)
    };
    let mut one = Encoder::new(r.clone()).unwrap();
    let mut two = Encoder::new(r.clone()).unwrap();
    two.force_two_passes = true;
    let (packed, a) = planes(&mut one);
    let (unpacked, b) = planes(&mut two);
    assert!(!unpacked, "the two-pass encoder never packs");
    assert_eq!(packed, w.is_multiple_of(4) && h.is_multiple_of(2), "{w}x{h}: packed only where a thread's block fits");
    // The same bytes, plane for plane (the planes may be different textures: R32_UINT or R8 / R8G8).
    assert_eq!(a.len(), 2);
    for (i, (x, y)) in a.iter().zip(&b).enumerate() {
        assert_eq!(x.len(), y.len(), "{w}x{h} plane {i}: size");
        let worst = x.iter().zip(y).map(|(p, q)| p.abs_diff(*q)).max().unwrap();
        let differing = x.iter().zip(y).filter(|(p, q)| p != q).count();
        // The render target converts float to 8 bit in the output merger (with the hardware's own tolerance), the
        // shader rounds by hand: a value within a hair of .5 may land on either side. One code at most, and rarely.
        assert!(worst <= 1 && differing * 100 <= x.len() * 4, "{w}x{h} plane {i}: worst {worst}, {differing} of {} differ", x.len());
    }
    // And the bytes are what a CPU calculation of BT.709 limited range gives, within a code.
    let rows = rgb_to_yuv_rows(ColorSpace::Bt709, ColorRange::Limited);
    let y_at = |x: u32, y: u32| {
        let p = px[(y * w + x) as usize];
        let v = rows[0][0] * p[0] as f64 / 255.0 + rows[0][1] * p[1] as f64 / 255.0 + rows[0][2] * p[2] as f64 / 255.0 + rows[0][3];
        (v.clamp(0.0, 1.0) * 255.0).round()
    };
    for (x, y) in [(0, 0), (w - 1, 0), (w / 2, h / 2), (w - 1, h - 1), (1, h - 1)] {
        let got = a[0][(y * w + x) as usize] as f64;
        assert!((got - y_at(x, y)).abs() <= 1.0, "{w}x{h} luma at ({x},{y}): {got} against {}", y_at(x, y));
    }
}

fn nv12_in_one_dispatch_on(r: &Arc<Renderer>) {
    // The packed path (width a multiple of 4), at sizes that fill a thread group exactly, leave a partial one, and
    // are as small as one block; and the sizes that fall back to two draws (a width of 2 mod 4).
    for (w, h) in [(64, 32), (48, 34), (68, 18), (4, 2), (8, 2), (1920, 1080), (50, 34), (2, 2), (642, 360)] {
        encode_both_ways(r, w, h);
    }
}

#[test]
fn nv12_in_one_dispatch_matches_the_two_draws_warp() {
    nv12_in_one_dispatch_on(&warp());
}

#[test]
fn nv12_in_one_dispatch_matches_the_two_draws_hardware() {
    match renderer(AdapterKind::Hardware) {
        Some(r) => nv12_in_one_dispatch_on(&r),
        None => eprintln!("no hardware adapter here"),
    }
}

#[test]
fn packed_nv12_planes_read_back_through_the_ring_with_the_rows_of_the_nv12_planes() {
    let r = warp();
    let (w, h) = (64u32, 32u32);
    let px = noisy(w, h);
    let src = r.texture(w, h).unwrap();
    fill_rgba(&r, &src, |x, y| px[(y * w + x) as usize]);
    let mut enc = Encoder::new(r.clone()).unwrap();
    let t = enc.encode(&src, EncodeFormat::Nv12).unwrap();
    assert!(t.packed);
    let descs = t.plane_descs();
    // The rows hold w bytes in both planes, whatever the texel format, and the heights are those of NV12.
    assert_eq!((descs[0].row_bytes(), descs[0].height), (w as usize, h));
    assert_eq!((descs[1].row_bytes(), descs[1].height), (w as usize, h / 2));
    let mut ring: ReadbackRing<()> = ReadbackRing::new(r.gpu.clone(), &descs).unwrap();
    ring.submit(&t.textures(), ()).unwrap();
    let want_y = read_texture(&r, &t.planes[0].texture, descs[0].width, descs[0].height, descs[0].bytes_per_texel);
    let want_uv = read_texture(&r, &t.planes[1].texture, descs[1].width, descs[1].height, descs[1].bytes_per_texel);
    ring.poll(true, |planes, ()| {
        let mut y = vec![0u8; (w * h) as usize];
        planes[0].copy_to(&mut y, w as usize);
        let mut uv = vec![0u8; (w * h / 2) as usize];
        planes[1].copy_to(&mut uv, w as usize);
        assert_eq!(y, want_y);
        assert_eq!(uv, want_uv);
    })
    .unwrap()
    .expect("a frame");
}
