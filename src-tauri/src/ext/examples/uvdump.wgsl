// Writes the pixel's own coordinates: pins the origin (top-left), the direction of y and the pixel centre.
@fragment
fn main(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
  let uv = pos.xy * frame.inv_size;
  return vec4<f32>(uv.x, uv.y, fract(pos.x), 1.0);
}
