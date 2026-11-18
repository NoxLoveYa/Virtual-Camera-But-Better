// params: speed f32
@fragment
fn main(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
  let uv = pos.xy * frame.inv_size;
  let wave = 0.5 + 0.5 * sin(frame.time * params.speed + uv.x * 6.2831853);
  return vec4<f32>(wave, f32(frame.frame) / 255.0, frame.seed, 1.0);
}
