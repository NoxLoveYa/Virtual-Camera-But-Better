// params: k f32. `stamp` is a path param image (binding 4, t2); `history` is the card's previous output (t1).
@fragment
fn main(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
  let uv = pos.xy * frame.inv_size;
  let a = tex(image, uv);
  let b = tex(history, uv);
  let s = tex(stamp, uv);
  return vec4<f32>(a.rgb * 0.5 + b.rgb * 0.25 + s.rgb * 0.25 * params.k, a.a);
}
