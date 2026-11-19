// params (manifest order): shadow vec4<f32>, highlight vec4<f32>, amount f32
// The mix happens on the gamma-encoded codes, like the picture itself, so black gives exactly `shadow` and white
// exactly `highlight`.
@fragment
fn main(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
  let c = tex(image, pos.xy * frame.inv_size);
  let tone = mix(params.shadow.rgb, params.highlight.rgb, luma(c.rgb));
  return vec4<f32>(mix(c.rgb, tone, params.amount), c.a);
}
