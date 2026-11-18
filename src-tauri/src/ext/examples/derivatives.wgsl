@fragment
fn main(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
  let uv = pos.xy * frame.inv_size;
  let fw = fwidth(uv.x) * frame.size.x;
  let dx = abs(dpdx(uv.x)) * frame.size.x;
  let dy = abs(dpdy(uv.y)) * frame.size.y;
  return vec4<f32>(fw * 0.5, dx * 0.5, dy * 0.5, 1.0);
}
