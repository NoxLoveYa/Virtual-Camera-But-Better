//! The preview point (UX-PASS L1): `Chain::run_tapped` shows the picture at a chosen place in the chain and leaves
//! the result of the run alone.

use std::sync::Arc;

use serde_json::json;

use super::*;
use crate::gpu::pipeline_tests::{fill_rgba, read_rgba, warp};
use crate::gpu::{GpuResult, Renderer, RgbaTexture};

/// A filter that halves the size (a changed size flows on, so the tapped sizes tell the places apart).
struct Half;

impl GpuFilter for Half {
    fn update(&mut self, _r: &Renderer, _p: &Params) {}

    fn output_size(&self, input: (u32, u32)) -> (u32, u32) {
        (input.0 / 2, input.1 / 2)
    }

    fn render(&mut self, r: &Renderer, input: &RgbaTexture, output: &RgbaTexture) -> GpuResult<()> {
        let ps = r.shaders.pixel_source(
            "test_half",
            "Texture2D<float4> t : register(t0);\nfloat4 main(float4 p : SV_Position) : SV_Target { return t.Load(int3(int2(p.xy) * 2, 0)); }",
        )?;
        r.draw(&crate::gpu::Pass {
            ps: &ps,
            srvs: &[Some(input.srv.clone())],
            constants: None,
            target: &output.rtv,
            width: output.width,
            height: output.height,
        });
        Ok(())
    }
}

fn kinds(kind: &str) -> Option<Box<dyn GpuFilter>> {
    match kind {
        "test_half" => Some(Box::new(Half)),
        other => make(other),
    }
}

fn cfg(id: &str, kind: &str, enabled: bool, params: serde_json::Value) -> FilterCfg {
    FilterCfg {
        id: id.into(),
        kind: kind.into(),
        enabled,
        params: params.as_object().cloned().unwrap_or_default(),
    }
}

/// What the tap saw: the size and the first pixel of the picture (the test picture is one flat colour).
type Seen = Vec<((u32, u32), [u8; 4])>;

const RED: [u8; 4] = [220, 30, 30, 255];

/// Run a flat red 64x32 picture through `chain` with `point`; returns what the tap saw and the output's size and pixels.
fn run(r: &Arc<Renderer>, chain: &mut Chain, point: &PreviewPoint) -> (Seen, (u32, u32), Vec<u8>) {
    let tex = r.texture(64, 32).unwrap();
    fill_rgba(r, &tex, |_, _| RED);
    let mut seen = Seen::new();
    let out = chain
        .run_tapped(r, tex, std::time::Instant::now(), point, &mut |t| {
            let px = read_rgba(r, t);
            seen.push(((t.width, t.height), [px[0], px[1], px[2], px[3]]));
        })
        .unwrap();
    let size = (out.width, out.height);
    (seen, size, read_rgba(r, &out))
}

fn is_gray(p: [u8; 4]) -> bool {
    p[0].abs_diff(p[1]) <= 1 && p[1].abs_diff(p[2]) <= 1
}

fn chain_of(r: &Arc<Renderer>, cfgs: &[FilterCfg]) -> Chain {
    let mut chain = Chain::with_factory(kinds);
    chain.apply(r, cfgs);
    chain
}

/// gray (on) -> half (off) -> gray again (on, a second filter that changes nothing visible): three places to look at.
fn three() -> Vec<FilterCfg> {
    vec![
        cfg("gray", "color_filter", true, json!({ "saturation": -1 })),
        cfg("half", "test_half", false, json!({})),
        cfg("last", "color_filter", true, json!({ "gamma": 0.5 })),
    ]
}

#[test]
fn the_output_point_never_taps() {
    let r = warp();
    let mut chain = chain_of(&r, &three());
    let (seen, size, _) = run(&r, &mut chain, &PreviewPoint::Output);
    assert!(seen.is_empty());
    assert_eq!(size, (64, 32));
}

#[test]
fn the_input_point_is_the_picture_before_the_first_filter() {
    let r = warp();
    let mut chain = chain_of(&r, &three());
    let (seen, _, out) = run(&r, &mut chain, &PreviewPoint::Input);
    assert_eq!(seen, vec![((64, 32), RED)], "once, untouched by the gray filter");
    assert!(is_gray([out[0], out[1], out[2], out[3]]), "the result still went through the chain");
    // an empty chain has an input too
    let mut empty = chain_of(&r, &[]);
    let (seen, _, out) = run(&r, &mut empty, &PreviewPoint::Input);
    assert_eq!(seen, vec![((64, 32), RED)]);
    assert_eq!(&out[..4], &RED);
}

#[test]
fn a_filter_point_is_the_picture_entering_that_filter_on_or_off() {
    let r = warp();
    let mut chain = chain_of(&r, &three());
    // before the first filter: the camera's picture
    let (seen, _, _) = run(&r, &mut chain, &PreviewPoint::Filter("gray".into()));
    assert_eq!(seen, vec![((64, 32), RED)]);
    // before "half", which is switched off: after "gray" and not halved, whether the filter is on or not
    let (seen, _, _) = run(&r, &mut chain, &PreviewPoint::Filter("half".into()));
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, (64, 32));
    assert!(is_gray(seen[0].1), "after the gray filter: {:?}", seen[0].1);
    // switched on, "half" gets the same picture; the filter after it gets the halved one
    let mut on = three();
    on[1].enabled = true;
    chain.apply(&r, &on);
    let (seen, size, _) = run(&r, &mut chain, &PreviewPoint::Filter("half".into()));
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, (64, 32));
    assert!(is_gray(seen[0].1));
    let (seen, size_after, _) = run(&r, &mut chain, &PreviewPoint::Filter("last".into()));
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, (32, 16), "the size of the picture that enters the filter, not the camera's");
    assert_eq!(size, (32, 16));
    assert_eq!(size_after, (32, 16));
}

#[test]
fn a_filter_the_chain_does_not_have_taps_nothing() {
    let r = warp();
    let mut chain = chain_of(&r, &three());
    let (seen, size, _) = run(&r, &mut chain, &PreviewPoint::Filter("nope".into()));
    assert!(seen.is_empty(), "the caller falls back to the output");
    assert_eq!(size, (64, 32));
    // a kind the engine does not know is skipped, so it has no place in the chain either
    let mut chain = chain_of(&r, &[cfg("x", "no_such_filter", true, json!({}))]);
    assert!(!chain.has_filter("x"));
    let (seen, _, _) = run(&r, &mut chain, &PreviewPoint::Filter("x".into()));
    assert!(seen.is_empty());
}

#[test]
fn every_point_gives_the_same_output() {
    let r = warp();
    let mut chain = chain_of(&r, &three());
    let (_, size, plain) = run(&r, &mut chain, &PreviewPoint::Output);
    for point in [
        PreviewPoint::Input,
        PreviewPoint::Compare,
        PreviewPoint::Filter("gray".into()),
        PreviewPoint::Filter("half".into()),
        PreviewPoint::Filter("last".into()),
        PreviewPoint::Filter("nope".into()),
    ] {
        let (_, s, out) = run(&r, &mut chain, &point);
        assert_eq!(s, size, "{point:?}");
        assert_eq!(out, plain, "{point:?}");
    }
}

#[test]
fn the_point_argument_parses_null_input_compare_and_a_filter_object() {
    assert_eq!(PreviewPoint::parse(&json!(null)), Ok(PreviewPoint::Output));
    assert_eq!(PreviewPoint::parse(&json!("input")), Ok(PreviewPoint::Input));
    assert_eq!(PreviewPoint::parse(&json!("compare")), Ok(PreviewPoint::Compare));
    assert_eq!(
        PreviewPoint::parse(&json!({ "filter": "abc" })),
        Ok(PreviewPoint::Filter("abc".into()))
    );
    for bad in [
        json!("output"),
        json!("Compare"),
        json!(""),
        json!(3),
        json!([]),
        json!({}),
        json!({ "filter": "" }),
        json!({ "filter": 3 }),
        json!({ "filter": "a", "extra": 1 }),
        json!({ "input": "a" }),
    ] {
        assert!(PreviewPoint::parse(&bad).is_err(), "{bad}");
    }
}
