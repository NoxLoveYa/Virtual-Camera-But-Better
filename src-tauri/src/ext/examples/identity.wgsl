@fragment
fn main(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
  return tex(image, pos.xy * frame.inv_size);
}
