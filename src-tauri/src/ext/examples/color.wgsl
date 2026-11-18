// params: gain f32, tint vec3<f32>, level f32, off vec2<f32>, mul vec4<f32>, invert u32
@fragment
fn main(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
  let c = tex(image, pos.xy * frame.inv_size);
  var rgb = c.rgb * params.gain * params.tint;
  rgb = rgb + vec3<f32>(params.off.x, params.off.y, params.level);
  rgb = rgb * params.mul.rgb;
  if (params.invert != 0u) {
    rgb = vec3<f32>(1.0) - rgb;
  }
  return vec4<f32>(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)), c.a * params.mul.a);
}
