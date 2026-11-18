// params: cell f32 (pixelate size in pixels), inner f32, outer f32, amount f32
@fragment
fn main(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
  let uv = pos.xy * frame.inv_size;
  let cell = max(params.cell, 1.0);
  // the first pixel of the cell: its centre, so the sample is an exact texel
  let p = floor(pos.xy / cell) * cell + vec2<f32>(0.5, 0.5);
  let c = tex(image, p * frame.inv_size);
  let d = distance(uv, vec2<f32>(0.5, 0.5));
  let v = 1.0 - smoothstep(params.inner, params.outer, d) * params.amount;
  return vec4<f32>(c.rgb * v, c.a);
}
