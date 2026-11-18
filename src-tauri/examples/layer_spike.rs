//! Spike E2 of the extension API (docs/EXTENSIONS.md section 7): can Direct2D / DirectWrite draw into a texture on
//! the engine's own D3D11 device, on the render thread, without changing what the next `Renderer::draw` pass
//! produces, and what does rasterizing a scene cost?
//!
//! ```text
//! cargo run -p better-cam --release --example layer_spike [-- --warp] [--frames N] [--st-factory]
//! ```
//!
//! * `--warp`: run on the WARP adapter (times are printed but not judged).
//! * `--frames N`: frames per scene and mode (default 600, paced at 60 Hz).
//! * `--st-factory`: use `D2D1_FACTORY_TYPE_SINGLE_THREADED` instead of the multi-threaded factory (to see whether
//!   the factory type matters).
//! * `--cap-runs N` (text runs; characters scale with it unless `--cap-chars N`), `--cap-images N`,
//!   `--cap-shapes N`, `--cap-groups N`: size of the cap scene (defaults are the 6.3 caps: 112 / 4 096 / 16 / 376 /
//!   8 = 512 nodes). `--skip-typical`: time only the cap scene.
//! * `--high-priority`: run the process at high priority (less noise on a busy machine; times are a best case).
//!
//! Only the public engine API is used. The two scenes are hard-coded Rust calls, no scene types, no JSON.

use std::collections::HashMap;
use std::mem::ManuallyDrop;
use std::time::{Duration, Instant};

use better_cam::gpu::{AdapterKind, ConstantBuffer, GpuDevice, GpuTimer, Pass, Pooled, Renderer};
use windows::core::{w, Interface};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::{IDXGIDevice, IDXGISurface};
use windows::Win32::Media::{timeBeginPeriod, timeEndPeriod};

const W: u32 = 1920;
const H: u32 = 1080;
const FRAME: Duration = Duration::from_nanos(16_666_667);
const WARMUP_FRAMES: usize = 10;

// ---------------------------------------------------------------------------------------------------------------
// Types the `windows` crate takes from `windows-numerics` (a crate this package does not depend on directly). They
// are plain `#[repr(C)]` f32 structs; build them from arrays, with the target type inferred from the call.

fn from_f32s<T: Copy, const N: usize>(a: [f32; N]) -> T {
    assert_eq!(std::mem::size_of::<T>(), std::mem::size_of::<[f32; N]>());
    // SAFETY: `T` is a repr(C) struct of exactly N f32 (checked by the assert above).
    unsafe { std::mem::transmute_copy(&a) }
}

/// `Vector2 { X, Y }`.
fn v2<T: Copy>(x: f32, y: f32) -> T {
    from_f32s([x, y])
}

/// `Matrix3x2 { M11, M12, M21, M22, M31, M32 }` (row vectors: `p' = p * M`).
fn mat<T: Copy>(m: Mat) -> T {
    from_f32s(m)
}

type Mat = [f32; 6];
const IDENTITY: Mat = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];

/// `a` first, then `b`.
fn mat_mul(a: Mat, b: Mat) -> Mat {
    [
        a[0] * b[0] + a[1] * b[2],
        a[0] * b[1] + a[1] * b[3],
        a[2] * b[0] + a[3] * b[2],
        a[2] * b[1] + a[3] * b[3],
        a[4] * b[0] + a[5] * b[2] + b[4],
        a[4] * b[1] + a[5] * b[3] + b[5],
    ]
}

/// Scale by `s` and rotate by `deg` degrees around `(cx, cy)`.
fn mat_around(cx: f32, cy: f32, s: f32, deg: f32) -> Mat {
    let (sn, cs) = deg.to_radians().sin_cos();
    let (m11, m12, m21, m22) = (s * cs, s * sn, -s * sn, s * cs);
    [
        m11,
        m12,
        m21,
        m22,
        cx - (cx * m11 + cy * m21),
        cy - (cx * m12 + cy * m22),
    ]
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

fn rect(l: f32, t: f32, r: f32, b: f32) -> D2D_RECT_F {
    D2D_RECT_F {
        left: l,
        top: t,
        right: r,
        bottom: b,
    }
}

fn color(r: f32, g: f32, b: f32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r, g, b, a }
}

// ---------------------------------------------------------------------------------------------------------------
// Deterministic numbers for the cap scene

struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 33) as u32
    }
    fn unit(&mut self) -> f32 {
        (self.next() % 10_000) as f32 / 10_000.0
    }
    fn range(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (hi - lo) * self.unit()
    }
    fn below(&mut self, n: usize) -> usize {
        self.next() as usize % n
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Readback and comparison

/// Copy a texture through a staging texture and return tight rows (`w * bpt` bytes each). (Copy of the test-only
/// helper in `gpu/pipeline_tests.rs`.)
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

/// Largest per-byte difference and the first pixel where it happens, or `None` when the buffers are identical.
fn max_diff(a: &[u8], b: &[u8], width: usize) -> Option<(u8, usize, usize)> {
    assert_eq!(a.len(), b.len());
    if a == b {
        return None;
    }
    let mut best = (0u8, 0usize, 0usize);
    for (i, (x, y)) in a.iter().zip(b).enumerate() {
        let d = x.abs_diff(*y);
        if d > best.0 {
            let px = i / 4;
            best = (d, px % width, px / width);
        }
    }
    Some(best)
}

// ---------------------------------------------------------------------------------------------------------------
// D3D11 state snapshot

struct Snap {
    topo: D3D_PRIMITIVE_TOPOLOGY,
    il: Option<ID3D11InputLayout>,
    vb: Option<ID3D11Buffer>,
    vb_stride: u32,
    vb_offset: u32,
    ib: Option<ID3D11Buffer>,
    ib_format: DXGI_FORMAT,
    ib_offset: u32,
    vs: Option<ID3D11VertexShader>,
    hs: Option<ID3D11HullShader>,
    ds: Option<ID3D11DomainShader>,
    gs: Option<ID3D11GeometryShader>,
    ps: Option<ID3D11PixelShader>,
    cb: [Option<ID3D11Buffer>; 2],
    smp: [Option<ID3D11SamplerState>; 1],
    srv: [Option<ID3D11ShaderResourceView>; 6],
    rs: Option<ID3D11RasterizerState>,
    viewports: Vec<D3D11_VIEWPORT>,
    scissors: Vec<RECT>,
    blend: Option<ID3D11BlendState>,
    blend_factor: [f32; 4],
    sample_mask: u32,
    dss: Option<ID3D11DepthStencilState>,
    stencil_ref: u32,
    rtv: [Option<ID3D11RenderTargetView>; 8],
    dsv: Option<ID3D11DepthStencilView>,
}

fn id<T: Interface>(o: &Option<T>) -> String {
    match o {
        Some(x) => format!("{:p}", x.as_raw()),
        None => "null".to_string(),
    }
}

impl Snap {
    fn take(ctx: &ID3D11DeviceContext) -> Snap {
        // SAFETY: plain getters on the render thread's context; every out pointer is a local.
        unsafe {
            let topo = ctx.IAGetPrimitiveTopology();
            let il = ctx.IAGetInputLayout().ok();
            let mut vb = None;
            let (mut vb_stride, mut vb_offset) = (0u32, 0u32);
            ctx.IAGetVertexBuffers(
                0,
                1,
                Some(&mut vb),
                Some(&mut vb_stride),
                Some(&mut vb_offset),
            );
            let mut ib = None;
            let mut ib_format = DXGI_FORMAT(0);
            let mut ib_offset = 0u32;
            ctx.IAGetIndexBuffer(Some(&mut ib), Some(&mut ib_format), Some(&mut ib_offset));
            let mut vs = None;
            ctx.VSGetShader(&mut vs, None, None);
            let mut hs = None;
            ctx.HSGetShader(&mut hs, None, None);
            let mut ds = None;
            ctx.DSGetShader(&mut ds, None, None);
            let mut gs = None;
            ctx.GSGetShader(&mut gs, None, None);
            let mut ps = None;
            ctx.PSGetShader(&mut ps, None, None);
            let mut cb: [Option<ID3D11Buffer>; 2] = [None, None];
            ctx.PSGetConstantBuffers(0, Some(&mut cb[..]));
            let mut smp: [Option<ID3D11SamplerState>; 1] = [None];
            ctx.PSGetSamplers(0, Some(&mut smp[..]));
            let mut srv: [Option<ID3D11ShaderResourceView>; 6] = Default::default();
            ctx.PSGetShaderResources(0, Some(&mut srv[..]));
            let rs = ctx.RSGetState().ok();
            let mut n = 0u32;
            ctx.RSGetViewports(&mut n, None);
            let mut viewports = vec![D3D11_VIEWPORT::default(); n as usize];
            if n > 0 {
                ctx.RSGetViewports(&mut n, Some(viewports.as_mut_ptr()));
            }
            let mut n = 0u32;
            ctx.RSGetScissorRects(&mut n, None);
            let mut scissors = vec![RECT::default(); n as usize];
            if n > 0 {
                ctx.RSGetScissorRects(&mut n, Some(scissors.as_mut_ptr()));
            }
            let mut blend = None;
            let mut blend_factor = [0f32; 4];
            let mut sample_mask = 0u32;
            ctx.OMGetBlendState(
                Some(&mut blend),
                Some(&mut blend_factor),
                Some(&mut sample_mask),
            );
            let mut dss = None;
            let mut stencil_ref = 0u32;
            ctx.OMGetDepthStencilState(Some(&mut dss), Some(&mut stencil_ref));
            let mut rtv: [Option<ID3D11RenderTargetView>; 8] = Default::default();
            let mut dsv = None;
            ctx.OMGetRenderTargets(Some(&mut rtv[..]), Some(&mut dsv));
            Snap {
                topo,
                il,
                vb,
                vb_stride,
                vb_offset,
                ib,
                ib_format,
                ib_offset,
                vs,
                hs,
                ds,
                gs,
                ps,
                cb,
                smp,
                srv,
                rs,
                viewports,
                scissors,
                blend,
                blend_factor,
                sample_mask,
                dss,
                stencil_ref,
                rtv,
                dsv,
            }
        }
    }

    /// Every captured state as `(name, value)`.
    fn items(&self) -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> = vec![
            ("IA topology".into(), format!("{:?}", self.topo)),
            ("IA input layout".into(), id(&self.il)),
            (
                "IA vertex buffer".into(),
                format!(
                    "{} stride {} offset {}",
                    id(&self.vb),
                    self.vb_stride,
                    self.vb_offset
                ),
            ),
            (
                "IA index buffer".into(),
                format!(
                    "{} {:?} offset {}",
                    id(&self.ib),
                    self.ib_format,
                    self.ib_offset
                ),
            ),
            ("VS".into(), id(&self.vs)),
            ("HS".into(), id(&self.hs)),
            ("DS".into(), id(&self.ds)),
            ("GS".into(), id(&self.gs)),
            ("PS".into(), id(&self.ps)),
        ];
        for (i, b) in self.cb.iter().enumerate() {
            v.push((format!("PS cb{i}"), id(b)));
        }
        v.push(("PS sampler0".into(), id(&self.smp[0])));
        for (i, s) in self.srv.iter().enumerate() {
            v.push((format!("PS srv{i}"), id(s)));
        }
        v.push(("RS state".into(), id(&self.rs)));
        v.push(("RS viewports".into(), format!("{:?}", self.viewports)));
        v.push(("RS scissor".into(), format!("{:?}", self.scissors)));
        v.push((
            "OM blend".into(),
            format!(
                "{} factor {:?} mask {:#x}",
                id(&self.blend),
                self.blend_factor,
                self.sample_mask
            ),
        ));
        v.push((
            "OM depth-stencil".into(),
            format!("{} ref {}", id(&self.dss), self.stencil_ref),
        ));
        v.push((
            "OM render targets".into(),
            format!(
                "[{}] dsv {}",
                self.rtv.iter().map(id).collect::<Vec<_>>().join(", "),
                id(&self.dsv)
            ),
        ));
        v
    }

    /// Names whose value differs from `after`, with before and after.
    fn diff(&self, after: &Snap) -> Vec<(String, String, String)> {
        self.items()
            .into_iter()
            .zip(after.items())
            .filter(|((_, a), (_, b))| a != b)
            .map(|((n, a), (_, b))| (n, a, b))
            .collect()
    }

    /// Put the listed states back to what this snapshot holds.
    fn restore(&self, ctx: &ID3D11DeviceContext, names: &[String]) {
        // SAFETY: plain setters on the render thread's context; every pointer outlives the call.
        unsafe {
            for n in names {
                match n.as_str() {
                    "IA topology" => ctx.IASetPrimitiveTopology(self.topo),
                    "IA input layout" => ctx.IASetInputLayout(self.il.as_ref()),
                    "IA vertex buffer" => ctx.IASetVertexBuffers(
                        0,
                        1,
                        Some(&self.vb),
                        Some(&self.vb_stride),
                        Some(&self.vb_offset),
                    ),
                    "IA index buffer" => {
                        ctx.IASetIndexBuffer(self.ib.as_ref(), self.ib_format, self.ib_offset)
                    }
                    "VS" => ctx.VSSetShader(self.vs.as_ref(), None),
                    "HS" => ctx.HSSetShader(self.hs.as_ref(), None),
                    "DS" => ctx.DSSetShader(self.ds.as_ref(), None),
                    "GS" => ctx.GSSetShader(self.gs.as_ref(), None),
                    "PS" => ctx.PSSetShader(self.ps.as_ref(), None),
                    "PS cb0" => ctx.PSSetConstantBuffers(0, Some(&self.cb[0..1])),
                    "PS cb1" => ctx.PSSetConstantBuffers(1, Some(&self.cb[1..2])),
                    "PS sampler0" => ctx.PSSetSamplers(0, Some(&self.smp[..])),
                    "RS state" => ctx.RSSetState(self.rs.as_ref()),
                    "RS viewports" => ctx.RSSetViewports(Some(&self.viewports)),
                    "RS scissor" => ctx.RSSetScissorRects(Some(&self.scissors)),
                    "OM blend" => ctx.OMSetBlendState(
                        self.blend.as_ref(),
                        Some(&self.blend_factor),
                        self.sample_mask,
                    ),
                    "OM depth-stencil" => {
                        ctx.OMSetDepthStencilState(self.dss.as_ref(), self.stencil_ref)
                    }
                    "OM render targets" => {
                        ctx.OMSetRenderTargets(Some(&self.rtv), self.dsv.as_ref())
                    }
                    s if s.starts_with("PS srv") => {
                        let i: usize = s["PS srv".len()..].parse().unwrap();
                        ctx.PSSetShaderResources(i as u32, Some(&self.srv[i..i + 1]));
                    }
                    other => eprintln!("restore: unknown state {other}"),
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// The probe pass

const PROBE_HLSL: &str = r#"
Texture2D<float4> src : register(t0);
SamplerState smp : register(s0);
cbuffer P : register(b0) { float4 k; };
float4 main(float4 pos : SV_Position) : SV_Target
{
    // Half-texel offset: a changed sampler (filter, address mode) changes the result.
    float2 uv = pos.xy / float2(1920.0, 1080.0) + float2(0.5 / 64.0, 0.5 / 64.0);
    float3 c = src.Sample(smp, uv).rgb * k.x + k.yzw * 0.25;
    return float4(c, 0.5);
}
"#;

struct Probe<'a> {
    r: &'a Renderer,
    ps: ID3D11PixelShader,
    src: Pooled,
    cb: ConstantBuffer,
    target: Pooled,
}

impl<'a> Probe<'a> {
    fn new(r: &'a Renderer) -> Probe<'a> {
        let ps = r
            .shaders
            .pixel_source("e2_probe", PROBE_HLSL)
            .expect("probe shader");
        let src = r.texture(64, 64).expect("probe source");
        let mut data = vec![0u8; 64 * 64 * 4];
        for y in 0..64usize {
            for x in 0..64usize {
                let o = (y * 64 + x) * 4;
                data[o] = (x * 4) as u8;
                data[o + 1] = (y * 4) as u8;
                data[o + 2] = ((x ^ y) * 4) as u8;
                data[o + 3] = 255;
            }
        }
        unsafe {
            r.gpu.context.UpdateSubresource(
                &src.texture,
                0,
                None,
                data.as_ptr() as *const _,
                64 * 4,
                0,
            )
        };
        let cb = r.constants(16).expect("probe constants");
        cb.write(&r.gpu.context, &[0.9f32, 0.1, 0.2, 0.3])
            .expect("probe constants write");
        let target = r.texture(W, H).expect("probe target");
        Probe {
            r,
            ps,
            src,
            cb,
            target,
        }
    }

    /// Clear the target to a known colour and draw the probe over it.
    fn run(&self) {
        self.r.clear(&self.target.rtv, [0.2, 0.4, 0.6, 1.0]);
        self.r.draw(&Pass {
            ps: &self.ps,
            srvs: &[Some(self.src.srv.clone())],
            constants: Some(&self.cb),
            target: &self.target.rtv,
            width: W,
            height: H,
        });
    }

    fn read(&self) -> Vec<u8> {
        read_texture(self.r, &self.target.texture, W, H, 4)
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Direct2D side

#[derive(Clone, Copy, PartialEq, Eq)]
enum Scene {
    Typical,
    Cap,
}

enum Leaf {
    Text(usize),
    Image {
        dst: [f32; 4],
        src: Option<[f32; 4]>,
        opacity: f32,
    },
    /// kind: 0 fill rect, 1 stroke rect, 2 fill rounded rect, 3 stroke rounded rect, 4 fill ellipse,
    /// 5 stroke ellipse, 6 line.
    Shape {
        kind: u8,
        r: [f32; 4],
        brush: usize,
        stroke: f32,
    },
}

struct TextRun {
    text: Vec<u16>,
    size: u32,
    weight: u32,
    x: f32,
    y: f32,
    brush: usize,
}

struct Group {
    rect: [f32; 4],
    radius: f32,
    center: (f32, f32),
    scale: f32,
    degrees: f32,
    opacity: f32,
}

/// The cap scene as data: 512 nodes = 8 nested groups + 112 text runs (4 096 characters) + 16 images + 376 shapes,
/// leaves spread over the 9 levels (root plus the 8 nested groups), so the deepest leaf sits at depth 9.
struct CapSpec {
    texts: Vec<TextRun>,
    leaves: Vec<Leaf>,
    groups: Vec<Group>,
}

/// Size of the cap scene. The 6.3 caps are `runs 112` (4 096 characters), `images 16`, `shapes 376`, `groups 8`:
/// 512 nodes.
#[derive(Clone, Copy)]
struct CapCfg {
    runs: usize,
    /// Characters per run: 4 096 spread over 112 runs, scaled with the run count.
    chars: usize,
    images: usize,
    shapes: usize,
    groups: usize,
}

impl CapCfg {
    const CAPS: CapCfg = CapCfg {
        runs: 112,
        chars: 4096,
        images: 16,
        shapes: 376,
        groups: 8,
    };
    fn nodes(&self) -> usize {
        self.runs + self.images + self.shapes + self.groups
    }
}

impl CapSpec {
    fn new(cfg: CapCfg) -> CapSpec {
        let mut rng = Lcg(0x5EED_E2);
        let base = "The quick brown fox jumps over the lazy dog 0123456789 ";
        let base: Vec<char> = base.chars().collect();
        let mut texts = Vec::new();
        for i in 0..cfg.runs {
            let len = cfg.chars / cfg.runs + usize::from(i < cfg.chars % cfg.runs);
            let start = rng.below(base.len());
            let s: String = (0..len).map(|k| base[(start + k) % base.len()]).collect();
            texts.push(TextRun {
                text: wide(&s),
                size: [14, 16, 20, 24, 28][i % 5],
                weight: if i % 2 == 0 { 400 } else { 600 },
                x: rng.range(0.0, 1500.0),
                y: rng.range(0.0, 900.0),
                brush: i % 4,
            });
        }
        assert_eq!(texts.iter().map(|t| t.text.len()).sum::<usize>(), cfg.chars);

        let mut leaves: Vec<Leaf> = Vec::new();
        for i in 0..cfg.runs {
            leaves.push(Leaf::Text(i));
        }
        for i in 0..cfg.images {
            let x = rng.range(0.0, 1600.0);
            let y = rng.range(0.0, 800.0);
            let s = rng.range(48.0, 200.0);
            leaves.push(Leaf::Image {
                dst: [x, y, x + s, y + s],
                // Half of them are sprite cells: one quarter of the atlas.
                src: if i % 2 == 0 {
                    let cx = if (i / 2) % 2 == 0 { 0.0 } else { 128.0 };
                    let cy = if (i / 4) % 2 == 0 { 0.0 } else { 128.0 };
                    Some([cx, cy, cx + 128.0, cy + 128.0])
                } else {
                    None
                },
                opacity: if i % 3 == 0 { 0.7 } else { 1.0 },
            });
        }
        for _ in 0..cfg.shapes {
            let x = rng.range(0.0, 1700.0);
            let y = rng.range(0.0, 950.0);
            let w = rng.range(8.0, 220.0);
            let h = rng.range(8.0, 120.0);
            leaves.push(Leaf::Shape {
                kind: rng.below(7) as u8,
                r: [x, y, x + w, y + h],
                brush: rng.below(NBRUSH),
                stroke: rng.range(1.0, 4.0),
            });
        }
        // Deterministic shuffle so the kinds interleave.
        for i in (1..leaves.len()).rev() {
            let j = rng.below(i + 1);
            leaves.swap(i, j);
        }
        let mut groups = Vec::new();
        let (mut x0, mut y0, mut x1, mut y1) = (20.0f32, 20.0f32, 1900.0f32, 1060.0f32);
        for i in 0..cfg.groups {
            groups.push(Group {
                rect: [x0, y0, x1, y1],
                radius: 24.0,
                center: ((x0 + x1) / 2.0, (y0 + y1) / 2.0),
                scale: 0.93,
                degrees: if i % 2 == 0 { 2.0 } else { -2.0 },
                opacity: 0.95,
            });
            x0 += 30.0;
            y0 += 20.0;
            x1 -= 30.0;
            y1 -= 20.0;
        }
        assert_eq!(leaves.len() + groups.len(), cfg.nodes());
        CapSpec {
            texts,
            leaves,
            groups,
        }
    }

    fn level(&self, level: usize) -> &[Leaf] {
        let levels = self.groups.len() + 1;
        let per = self.leaves.len() / levels;
        let start = level * per;
        let end = if level + 1 == levels {
            self.leaves.len()
        } else {
            start + per
        };
        &self.leaves[start..end]
    }
}

const NBRUSH: usize = 12;

/// Layouts and geometries kept between rasters in warm mode.
#[derive(Default)]
struct Cache {
    clock: Option<IDWriteTextLayout>,
    layouts: Vec<IDWriteTextLayout>,
    geoms: Vec<ID2D1Geometry>,
}

struct D2d {
    factory: ID2D1Factory1,
    dw: IDWriteFactory,
    dc: ID2D1DeviceContext,
    brushes: Vec<ID2D1Brush>,
    formats: HashMap<(u32, u32), IDWriteTextFormat>,
    atlas: ID2D1Bitmap1,
    _target: ID2D1Bitmap1,
    layer: ID3D11Texture2D,
    spec: CapSpec,
}

struct InitTimes {
    d2d_ms: f64,
    first_layout_ms: f64,
}

impl D2d {
    fn new(
        r: &Renderer,
        single_threaded: bool,
        cap: CapCfg,
    ) -> windows::core::Result<(D2d, InitTimes)> {
        let t0 = Instant::now();
        unsafe {
            let ftype = if single_threaded {
                D2D1_FACTORY_TYPE_SINGLE_THREADED
            } else {
                D2D1_FACTORY_TYPE_MULTI_THREADED
            };
            let factory: ID2D1Factory1 = D2D1CreateFactory(ftype, None)?;
            let dxgi: IDXGIDevice = r.gpu.device.cast()?;
            let device = factory.CreateDevice(&dxgi)?;
            let dc = device.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?;
            let dw: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;

            let layer = r
                .gpu
                .create_texture(&D3D11_TEXTURE2D_DESC {
                    Width: W,
                    Height: H,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                    CPUAccessFlags: 0,
                    MiscFlags: 0,
                })
                .map_err(|e| match e {
                    better_cam::gpu::GpuError::Api(e) => e,
                    other => windows::core::Error::new(
                        windows::Win32::Foundation::E_FAIL,
                        other.to_string(),
                    ),
                })?;
            let surface: IDXGISurface = layer.cast()?;
            let props = D2D1_BITMAP_PROPERTIES1 {
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                dpiX: 96.0,
                dpiY: 96.0,
                bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
                colorContext: ManuallyDrop::new(None),
            };
            let target = dc.CreateBitmapFromDxgiSurface(&surface, Some(&props))?;
            dc.SetTarget(&target);
            dc.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);

            // Brushes: 0 white, 1 black 50 %, 2.. colours, some translucent.
            let palette: [[f32; 4]; NBRUSH] = [
                [1.0, 1.0, 1.0, 1.0],
                [0.0, 0.0, 0.0, 0.5],
                [1.0, 0.2, 0.2, 1.0],
                [0.2, 1.0, 0.3, 0.8],
                [0.2, 0.4, 1.0, 1.0],
                [1.0, 0.9, 0.1, 0.6],
                [0.9, 0.3, 0.9, 1.0],
                [0.1, 0.9, 0.9, 0.5],
                [0.5, 0.5, 0.5, 1.0],
                [1.0, 0.5, 0.0, 0.75],
                [0.0, 0.0, 0.0, 1.0],
                [1.0, 1.0, 1.0, 0.3],
            ];
            let mut brushes = Vec::new();
            for c in palette {
                let b = dc.CreateSolidColorBrush(&color(c[0], c[1], c[2], c[3]), None)?;
                brushes.push(b.cast::<ID2D1Brush>()?);
            }

            // One 256x256 bitmap, uploaded once.
            let mut px = vec![0u8; 256 * 256 * 4];
            for y in 0..256usize {
                for x in 0..256usize {
                    let o = (y * 256 + x) * 4;
                    let check = ((x / 32) + (y / 32)) % 2 == 0;
                    px[o] = if check { 255 } else { x as u8 };
                    px[o + 1] = y as u8;
                    px[o + 2] = if check { 40 } else { 200 };
                    px[o + 3] = 255;
                }
            }
            let aprops = D2D1_BITMAP_PROPERTIES1 {
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                dpiX: 96.0,
                dpiY: 96.0,
                bitmapOptions: D2D1_BITMAP_OPTIONS_NONE,
                colorContext: ManuallyDrop::new(None),
            };
            let atlas = dc.CreateBitmap(
                D2D_SIZE_U {
                    width: 256,
                    height: 256,
                },
                Some(px.as_ptr() as *const _),
                256 * 4,
                &aprops,
            )?;
            let d2d_ms = t0.elapsed().as_secs_f64() * 1000.0;

            let mut this = D2d {
                factory,
                dw,
                dc,
                brushes,
                formats: HashMap::new(),
                atlas,
                _target: target,
                layer,
                spec: CapSpec::new(cap),
            };
            // First text layout: loads the system font collection.
            let t1 = Instant::now();
            this.format(40, 600, false)?;
            let _first = this.dw.CreateTextLayout(
                &wide("12:34:56"),
                &this.formats[&(40, 600)],
                240.0,
                60.0,
            )?;
            let first_layout_ms = t1.elapsed().as_secs_f64() * 1000.0;
            for size in [14, 16, 20, 24, 28] {
                for weight in [400, 600] {
                    this.format(size, weight, true)?;
                }
            }
            Ok((
                this,
                InitTimes {
                    d2d_ms,
                    first_layout_ms,
                },
            ))
        }
    }

    /// A cached `IDWriteTextFormat`. The clock (size 40) does not wrap; the cap scene's runs do.
    fn format(&mut self, size: u32, weight: u32, wrap: bool) -> windows::core::Result<()> {
        if self.formats.contains_key(&(size, weight)) {
            return Ok(());
        }
        unsafe {
            let f = self.dw.CreateTextFormat(
                w!("Segoe UI"),
                None::<&IDWriteFontCollection>,
                DWRITE_FONT_WEIGHT(weight as i32),
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                size as f32,
                w!("en-us"),
            )?;
            if !wrap {
                f.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            }
            self.formats.insert((size, weight), f);
        }
        Ok(())
    }

    fn clock_layout(&self) -> windows::core::Result<IDWriteTextLayout> {
        unsafe {
            self.dw
                .CreateTextLayout(&wide("12:34:56"), &self.formats[&(40, 600)], 240.0, 60.0)
        }
    }

    fn cap_layouts(&self) -> windows::core::Result<Vec<IDWriteTextLayout>> {
        self.spec
            .texts
            .iter()
            .map(|t| unsafe {
                self.dw
                    .CreateTextLayout(&t.text, &self.formats[&(t.size, t.weight)], 320.0, 200.0)
            })
            .collect()
    }

    fn cap_geoms(&self) -> windows::core::Result<Vec<ID2D1Geometry>> {
        self.spec
            .groups
            .iter()
            .map(|g| unsafe {
                let rr = D2D1_ROUNDED_RECT {
                    rect: rect(g.rect[0], g.rect[1], g.rect[2], g.rect[3]),
                    radiusX: g.radius,
                    radiusY: g.radius,
                };
                self.factory
                    .CreateRoundedRectangleGeometry(&rr)?
                    .cast::<ID2D1Geometry>()
            })
            .collect()
    }

    /// Fill the warm-mode cache for `scene`.
    fn prepare(&self, scene: Scene) -> windows::core::Result<Cache> {
        Ok(match scene {
            Scene::Typical => Cache {
                clock: Some(self.clock_layout()?),
                ..Default::default()
            },
            Scene::Cap => Cache {
                layouts: self.cap_layouts()?,
                geoms: self.cap_geoms()?,
                ..Default::default()
            },
        })
    }

    /// One raster: `BeginDraw`, clear, scene, `EndDraw`. Returns the CPU wall time from `BeginDraw` to the return of
    /// `EndDraw` in ms. In cold mode (`cache` is `None`) every text layout and geometry is built inside the timed
    /// region; in warm mode they come from `cache`. Brushes, formats and the atlas are cached in both.
    fn raster(&self, scene: Scene, cache: Option<&Cache>) -> windows::core::Result<f64> {
        let t0 = Instant::now();
        unsafe {
            self.dc.BeginDraw();
            self.dc.SetTransform(&mat::<_>(IDENTITY));
            self.dc.Clear(Some(&color(0.0, 0.0, 0.0, 0.0)));
            match scene {
                Scene::Typical => {
                    let local;
                    let clock = match cache {
                        Some(c) => c.clock.as_ref().unwrap(),
                        None => {
                            local = self.clock_layout()?;
                            &local
                        }
                    };
                    self.draw_typical(clock);
                }
                Scene::Cap => {
                    let (l_local, g_local);
                    let (layouts, geoms) = match cache {
                        Some(c) => (&c.layouts, &c.geoms),
                        None => {
                            l_local = self.cap_layouts()?;
                            g_local = self.cap_geoms()?;
                            (&l_local, &g_local)
                        }
                    };
                    self.draw_level(0, IDENTITY, layouts, geoms);
                }
            }
            self.dc.EndDraw(None, None)?;
        }
        Ok(t0.elapsed().as_secs_f64() * 1000.0)
    }

    fn draw_typical(&self, clock: &IDWriteTextLayout) {
        unsafe {
            self.dc.FillRoundedRectangle(
                &D2D1_ROUNDED_RECT {
                    rect: rect(40.0, 40.0, 300.0, 120.0),
                    radiusX: 12.0,
                    radiusY: 12.0,
                },
                &self.brushes[1],
            );
            self.dc.DrawTextLayout(
                v2(60.0, 52.0),
                clock,
                &self.brushes[0],
                D2D1_DRAW_TEXT_OPTIONS_NONE,
            );
        }
    }

    fn draw_level(
        &self,
        level: usize,
        parent: Mat,
        layouts: &[IDWriteTextLayout],
        geoms: &[ID2D1Geometry],
    ) {
        unsafe {
            self.dc.SetTransform(&mat::<_>(parent));
            for leaf in self.spec.level(level) {
                self.draw_leaf(leaf, layouts);
            }
            if level < self.spec.groups.len() {
                let g = &self.spec.groups[level];
                let world = mat_mul(
                    mat_around(g.center.0, g.center.1, g.scale, g.degrees),
                    parent,
                );
                self.dc.SetTransform(&mat::<_>(world));
                let params = D2D1_LAYER_PARAMETERS1 {
                    contentBounds: rect(-f32::MAX, -f32::MAX, f32::MAX, f32::MAX),
                    geometricMask: ManuallyDrop::new(Some(geoms[level].clone())),
                    maskAntialiasMode: D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
                    maskTransform: mat::<_>(IDENTITY),
                    opacity: g.opacity,
                    opacityBrush: ManuallyDrop::new(None),
                    layerOptions: D2D1_LAYER_OPTIONS1_NONE,
                };
                self.dc.PushLayer(&params, None::<&ID2D1Layer>);
                drop(ManuallyDrop::into_inner(params.geometricMask));
                self.draw_level(level + 1, world, layouts, geoms);
                self.dc.PopLayer();
                self.dc.SetTransform(&mat::<_>(parent));
            }
        }
    }

    fn draw_leaf(&self, leaf: &Leaf, layouts: &[IDWriteTextLayout]) {
        unsafe {
            match leaf {
                Leaf::Text(i) => {
                    let t = &self.spec.texts[*i];
                    self.dc.DrawTextLayout(
                        v2(t.x, t.y),
                        &layouts[*i],
                        &self.brushes[t.brush],
                        D2D1_DRAW_TEXT_OPTIONS_NONE,
                    );
                }
                Leaf::Image { dst, src, opacity } => {
                    let d = rect(dst[0], dst[1], dst[2], dst[3]);
                    let s = src.map(|s| rect(s[0], s[1], s[2], s[3]));
                    self.dc.DrawBitmap(
                        &self.atlas,
                        Some(&d),
                        *opacity,
                        D2D1_INTERPOLATION_MODE_LINEAR,
                        s.as_ref().map(|s| s as *const _),
                        None,
                    );
                }
                Leaf::Shape {
                    kind,
                    r,
                    brush,
                    stroke,
                } => {
                    let b = &self.brushes[*brush];
                    let rc = rect(r[0], r[1], r[2], r[3]);
                    let none = None::<&ID2D1StrokeStyle>;
                    let rr = D2D1_ROUNDED_RECT {
                        rect: rc,
                        radiusX: 8.0,
                        radiusY: 8.0,
                    };
                    let el = D2D1_ELLIPSE {
                        point: v2((r[0] + r[2]) / 2.0, (r[1] + r[3]) / 2.0),
                        radiusX: (r[2] - r[0]) / 2.0,
                        radiusY: (r[3] - r[1]) / 2.0,
                    };
                    match kind {
                        0 => self.dc.FillRectangle(&rc, b),
                        1 => self.dc.DrawRectangle(&rc, b, *stroke, none),
                        2 => self.dc.FillRoundedRectangle(&rr, b),
                        3 => self.dc.DrawRoundedRectangle(&rr, b, *stroke, none),
                        4 => self.dc.FillEllipse(&el, b),
                        5 => self.dc.DrawEllipse(&el, b, *stroke, none),
                        _ => self
                            .dc
                            .DrawLine(v2(r[0], r[1]), v2(r[2], r[3]), b, *stroke, none),
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Checks

/// Layer sanity on the typical layer (BGRA, premultiplied): inside the text box alpha > 0 everywhere and the glyphs
/// are opaque white; the rounded rectangle is 50 % black; outside the rectangle (and its anti-aliasing margin) every
/// byte is zero.
fn layer_check(px: &[u8], text_box: (f32, f32, f32, f32)) -> Result<(), String> {
    let at = |x: usize, y: usize| -> [u8; 4] {
        let o = (y * W as usize + x) * 4;
        [px[o], px[o + 1], px[o + 2], px[o + 3]]
    };
    for y in 0..H as usize {
        for x in 0..W as usize {
            if (38..302).contains(&x) && (38..122).contains(&y) {
                continue;
            }
            if at(x, y) != [0, 0, 0, 0] {
                return Err(format!(
                    "pixel {:?} at ({x},{y}) outside the box is not 0",
                    at(x, y)
                ));
            }
        }
    }
    let bg = at(45, 80);
    if !(120..=136).contains(&bg[3]) || bg[0] != 0 || bg[1] != 0 || bg[2] != 0 {
        return Err(format!(
            "rectangle interior (45,80) is {bg:?}, expected black at alpha ~128"
        ));
    }
    let (l, t, w, h) = text_box;
    let (x0, y0) = ((60.0 + l).floor() as usize, (52.0 + t).floor() as usize);
    let (x1, y1) = (
        (60.0 + l + w).ceil() as usize,
        (52.0 + t + h).ceil() as usize,
    );
    let mut opaque_white = 0usize;
    for y in y0..y1 {
        for x in x0..x1 {
            let p = at(x, y);
            if p[3] == 0 {
                return Err(format!("alpha 0 at ({x},{y}) inside the text box"));
            }
            if p[3] == 255 && p[0] > 200 && p[1] > 200 && p[2] > 200 {
                opaque_white += 1;
            }
        }
    }
    if opaque_white < 100 {
        return Err(format!(
            "only {opaque_white} opaque white glyph pixels in the text box"
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------------
// Timing

struct Stats {
    cpu: Vec<f64>,
    gpu: Vec<f64>,
}

fn pct(v: &[f64], q: f64) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    Some(s[((s.len() - 1) as f64 * q).round() as usize])
}

fn fmt_opt(v: Option<f64>) -> String {
    v.map(|x| format!("{x:.2}")).unwrap_or_else(|| "n/a".into())
}

fn pace(next: &mut Instant) {
    let now = Instant::now();
    if *next > now {
        let d = *next - now;
        if d > Duration::from_millis(2) {
            std::thread::sleep(d - Duration::from_millis(2));
        }
        while Instant::now() < *next {
            std::hint::spin_loop();
        }
    } else if now - *next > Duration::from_millis(100) {
        *next = now;
    }
    *next += FRAME;
}

#[allow(clippy::too_many_arguments)]
fn measure(
    d2d: &D2d,
    probe: &Probe,
    timer: &mut GpuTimer,
    scene: Scene,
    warm: bool,
    frames: usize,
    reset: &Option<(Snap, Vec<String>)>,
) -> windows::core::Result<Stats> {
    let ctx = &probe.r.gpu.context;
    let cache = if warm {
        Some(d2d.prepare(scene)?)
    } else {
        None
    };
    let mut stats = Stats {
        cpu: Vec::new(),
        gpu: Vec::new(),
    };
    let mut next = Instant::now();
    for f in 0..WARMUP_FRAMES + frames {
        let timed = f >= WARMUP_FRAMES;
        pace(&mut next);
        probe.run();
        let began = timed && timer.begin(ctx);
        let ms = d2d.raster(scene, cache.as_ref())?;
        if began {
            timer.end(ctx);
        }
        if let Some((snap, names)) = reset {
            snap.restore(ctx, names);
        }
        probe.run();
        probe.r.flush();
        if timed {
            stats.cpu.push(ms);
        }
        if let Some(g) = timer.poll(ctx) {
            if timed {
                stats.gpu.push(g);
            }
        }
    }
    // Drain the timer: results arrive a frame or two late.
    for _ in 0..8 {
        std::thread::sleep(Duration::from_millis(5));
        if let Some(g) = timer.poll(ctx) {
            stats.gpu.push(g);
        }
    }
    Ok(stats)
}

fn report(name: &str, frames: usize, s: &Stats) {
    println!(
        "{name:<13} frames {frames}  raster cpu ms p50 {} p95 {} max {}  gpu ms p50 {} p95 {}",
        fmt_opt(pct(&s.cpu, 0.5)),
        fmt_opt(pct(&s.cpu, 0.95)),
        fmt_opt(
            s.cpu
                .iter()
                .cloned()
                .fold(None, |m: Option<f64>, x| Some(m.map_or(x, |m| m.max(x))))
        ),
        fmt_opt(pct(&s.gpu, 0.5)),
        fmt_opt(pct(&s.gpu, 0.95)),
    );
}

// ---------------------------------------------------------------------------------------------------------------

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let warp = args.iter().any(|a| a == "--warp");
    let st_factory = args.iter().any(|a| a == "--st-factory");
    let num = |flag: &str, default: usize| -> usize {
        args.windows(2)
            .find(|w| w[0] == flag)
            .and_then(|w| w[1].parse().ok())
            .unwrap_or(default)
    };
    let frames = num("--frames", 600);
    // Cap scene size (defaults: the 6.3 caps). `--cap-runs` scales the characters with it (36.6 per run) unless
    // `--cap-chars` is given.
    let runs = num("--cap-runs", CapCfg::CAPS.runs);
    let cap = CapCfg {
        runs,
        chars: num("--cap-chars", runs * CapCfg::CAPS.chars / CapCfg::CAPS.runs),
        images: num("--cap-images", CapCfg::CAPS.images),
        shapes: num("--cap-shapes", CapCfg::CAPS.shapes),
        groups: num("--cap-groups", CapCfg::CAPS.groups),
    };
    let skip_typical = args.iter().any(|a| a == "--skip-typical");

    unsafe { timeBeginPeriod(1) };
    if args.iter().any(|a| a == "--high-priority") {
        // Less scheduler noise from other processes on a busy machine; times are then a best case.
        unsafe {
            use windows::Win32::System::Threading::{
                GetCurrentProcess, SetPriorityClass, HIGH_PRIORITY_CLASS,
            };
            let _ = SetPriorityClass(GetCurrentProcess(), HIGH_PRIORITY_CLASS);
        }
        println!("[process priority: high]");
    }
    let code = match run(warp, st_factory, frames, cap, skip_typical) {
        Ok(pass) => i32::from(!pass),
        Err(e) => {
            println!("verdict: FAIL (error: {e})");
            2
        }
    };
    unsafe { timeEndPeriod(1) };
    std::process::exit(code);
}

fn run(
    warp: bool,
    st_factory: bool,
    frames: usize,
    cap: CapCfg,
    skip_typical: bool,
) -> Result<bool, String> {
    let e = |what: &str| {
        let what = what.to_string();
        move |err: windows::core::Error| format!("{what}: {err}")
    };
    let kind = if warp {
        AdapterKind::Warp
    } else {
        AdapterKind::Hardware
    };
    let gpu = std::sync::Arc::new(GpuDevice::new(kind).map_err(e("device"))?);
    let renderer = Renderer::new(gpu.clone()).map_err(|x| format!("renderer: {x}"))?;
    let ctx = gpu.context.clone();
    let probe = Probe::new(&renderer);
    let mut timer = GpuTimer::new(&gpu.device).map_err(|x| format!("timer: {x}"))?;

    // Reference: the probe before Direct2D has ever run on this device.
    probe.run();
    let reference = probe.read();

    let (d2d, init) = D2d::new(&renderer, st_factory, cap).map_err(e("direct2d init"))?;
    println!(
        "adapter: {} ({})   d2d init {:.1} ms   first text layout {:.1} ms{}",
        gpu.adapter_name,
        if warp { "warp" } else { "hardware" },
        init.d2d_ms,
        init.first_layout_ms,
        if st_factory {
            "   [single-threaded D2D factory]"
        } else {
            ""
        }
    );
    println!(
        "cap scene: {} nodes (text runs {} / {} chars, images {}, shapes {}, groups {})",
        cap.nodes(),
        cap.runs,
        cap.chars,
        cap.images,
        cap.shapes,
        cap.groups
    );

    // Self-test of the check itself: a scissor rectangle left set must show in the snapshot diff and change the
    // probe's pixels, so "none" and "identical" below cannot be a blind spot.
    {
        probe.run();
        let before = Snap::take(&ctx);
        unsafe {
            let mut desc = D3D11_RASTERIZER_DESC {
                FillMode: D3D11_FILL_SOLID,
                CullMode: D3D11_CULL_BACK,
                DepthClipEnable: true.into(),
                ScissorEnable: true.into(),
                ..Default::default()
            };
            desc.FrontCounterClockwise = false.into();
            let mut rs = None;
            gpu.device
                .CreateRasterizerState(&desc, Some(&mut rs))
                .map_err(e("self-test rasterizer"))?;
            ctx.RSSetState(rs.as_ref());
            ctx.RSSetScissorRects(Some(&[RECT {
                left: 0,
                top: 0,
                right: 100,
                bottom: 100,
            }]));
        }
        let after = Snap::take(&ctx);
        let names: Vec<String> = before.diff(&after).into_iter().map(|d| d.0).collect();
        probe.run();
        let changed = max_diff(&reference, &probe.read(), W as usize).is_some();
        before.restore(&ctx, &names);
        probe.run();
        let restored = max_diff(&reference, &probe.read(), W as usize).is_none();
        println!(
            "self-test: injected scissor -> diff [{}], probe pixels changed: {changed}, restored by reset: {restored}",
            names.join(", ")
        );
        if names.is_empty() || !changed || !restored {
            return Err("self-test failed: the state check cannot see a leaked state".into());
        }
    }

    // Reverse check: the layer from a fresh context against the layer right after a probe pass.
    unsafe {
        ctx.ClearState();
        ctx.Flush();
    }
    let first = d2d
        .raster(Scene::Typical, None)
        .map_err(e("first raster"))?;
    let layer_a = read_texture(&renderer, &d2d.layer, W, H, 4);
    probe.run();
    d2d.raster(Scene::Typical, None).map_err(e("raster"))?;
    let layer_b = read_texture(&renderer, &d2d.layer, W, H, 4);
    let reverse = max_diff(&layer_a, &layer_b, W as usize);
    println!("first raster {first:.2} ms (includes lazy D2D setup)");

    // Layer sanity on the typical layer.
    let layout = d2d.clock_layout().map_err(e("layout"))?;
    let mut tm = DWRITE_TEXT_METRICS::default();
    unsafe { layout.GetMetrics(&mut tm) }.map_err(e("metrics"))?;
    let layer_res = layer_check(&layer_a, (tm.left, tm.top, tm.width, tm.height));

    // State check, on the same device, before the timing loops.
    let mut union_diff: Vec<String> = Vec::new();
    let mut reset_used: Vec<String> = Vec::new();
    let mut state_result: Result<(), (u8, usize, usize)> = Ok(());
    let mut reset_snap: Option<Snap> = None;
    for scene in [Scene::Typical, Scene::Cap] {
        let label = if scene == Scene::Typical {
            "typical"
        } else {
            "cap"
        };
        probe.run();
        let before = Snap::take(&ctx);
        d2d.raster(scene, None).map_err(e("raster"))?;
        let after = Snap::take(&ctx);
        let diff = before.diff(&after);
        for (n, a, b) in &diff {
            println!("  {label}: {n}: {a} -> {b}");
            if !union_diff.contains(n) {
                union_diff.push(n.clone());
            }
        }
        probe.run();
        let after_pixels = probe.read();
        match max_diff(&reference, &after_pixels, W as usize) {
            None => println!("  {label}: probe pixels identical"),
            Some((m, x, y)) => {
                println!(
                    "  {label}: probe pixels DIFFER (max {m}/255 at {x},{y}) before any reset"
                );
                let names: Vec<String> = diff.iter().map(|d| d.0.clone()).collect();
                // The probe ran between the snapshots, so `before` holds exactly what the probe leaves behind.
                d2d.raster(scene, None).map_err(e("raster"))?;
                before.restore(&ctx, &names);
                probe.run();
                let fixed = probe.read();
                match max_diff(&reference, &fixed, W as usize) {
                    None => {
                        println!("  {label}: identical after reset of {}", names.join(", "));
                        for n in names {
                            if !reset_used.contains(&n) {
                                reset_used.push(n);
                            }
                        }
                    }
                    Some(d) => {
                        println!(
                            "  {label}: still DIFFERS after reset (max {}/255 at {},{})",
                            d.0, d.1, d.2
                        );
                        state_result = Err(d);
                    }
                }
            }
        }
        if reset_snap.is_none() {
            reset_snap = Some(before);
        }
    }
    let state_diff_line = if union_diff.is_empty() {
        "none".to_string()
    } else {
        union_diff.join(", ")
    };
    let state_line = match (&state_result, reset_used.is_empty()) {
        (Ok(()), true) => "identical".to_string(),
        (Ok(()), false) => format!("identical after reset of {}", reset_used.join(", ")),
        (Err((m, x, y)), _) => format!("DIFFERS (max {m}/255 at {x},{y})"),
    };

    // Timing loops. When the state check needed a reset, the loop applies it after every EndDraw.
    let reset = if reset_used.is_empty() {
        None
    } else {
        reset_snap.map(|s| (s, reset_used.clone()))
    };
    let mut results: Vec<(&str, Stats)> = Vec::new();
    for (name, scene, warm) in [
        ("typical cold", Scene::Typical, false),
        ("typical warm", Scene::Typical, true),
        ("cap cold", Scene::Cap, false),
        ("cap warm", Scene::Cap, true),
    ] {
        if skip_typical && scene == Scene::Typical {
            continue;
        }
        let s = measure(&d2d, &probe, &mut timer, scene, warm, frames, &reset).map_err(e(name))?;
        report(name, frames, &s);
        results.push((name, s));
    }
    println!("state diff after EndDraw: {state_diff_line}");
    println!("state check: {state_line}");
    println!(
        "reverse check: {}",
        match reverse {
            None => "identical".to_string(),
            Some((m, x, y)) => format!("DIFFERS (max {m}/255 at {x},{y})"),
        }
    );
    println!(
        "layer check: {}",
        match &layer_res {
            Ok(()) => "ok".to_string(),
            Err(s) => s.clone(),
        }
    );

    // Verdict.
    let mut reasons: Vec<String> = Vec::new();
    if state_result.is_err() {
        reasons.push("state cannot be made identical".into());
    }
    if reverse.is_some() {
        reasons.push("reverse check differs".into());
    }
    if let Err(s) = &layer_res {
        reasons.push(format!("layer check: {s}"));
    }
    if !warp {
        let p95 = |n: &str| {
            results
                .iter()
                .find(|(k, _)| *k == n)
                .and_then(|(_, s)| pct(&s.cpu, 0.95))
                .unwrap_or(f64::INFINITY)
        };
        if !skip_typical && p95("typical cold") > 0.5 {
            reasons.push(format!(
                "typical cold p95 {:.2} ms > 0.5 ms",
                p95("typical cold")
            ));
        }
        if p95("cap cold") > 3.0 {
            reasons.push(format!("cap cold p95 {:.2} ms > 3 ms", p95("cap cold")));
        }
    }
    if reasons.is_empty() {
        println!(
            "verdict: PASS{}",
            if warp {
                " (WARP: times not judged)"
            } else {
                ""
            }
        );
        Ok(true)
    } else {
        println!("verdict: FAIL ({})", reasons.join("; "));
        Ok(false)
    }
}
