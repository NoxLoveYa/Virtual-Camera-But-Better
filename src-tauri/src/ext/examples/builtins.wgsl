// params: radius i32
fn weight(i: i32) -> f32 {
  return 1.0 - abs(f32(i)) * 0.2;
}

@fragment
fn main(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
  let uv = pos.xy * frame.inv_size;
  // implicit derivatives: only in uniform control flow, so before any branch on the pixel position
  let implicit_sample = textureSample(image, smp, uv);
  let explicit_sample = textureSampleLevel(image, smp, uv, 0.0);

  var acc = vec3<f32>(0.0);
  var total = 0.0;
  for (var i = -params.radius; i <= params.radius; i = i + 1) {
    let w = weight(i);
    acc = acc + tex(image, uv + vec2<f32>(f32(i) * 1.5 * frame.inv_size.x, 0.0)).rgb * w;
    total = total + w;
  }
  var rgb = acc / total;
  if (uv.x > 0.5) {
    rgb = mix(rgb, vec3<f32>(1.0) - rgb, 0.5);
  } else {
    rgb = pow(rgb, vec3<f32>(0.8));
  }
  let s = smoothstep(0.2, 0.8, rgb.g);
  rgb.r = clamp(rgb.r + 0.1 * sin(uv.y * 20.0), 0.0, 1.0);
  rgb.b = fract(rgb.b * 3.0);
  rgb.g = s;
  // the two sampling forms agree (one mip level), so this adds exactly 0
  let same = abs(implicit_sample.r - explicit_sample.r);
  return vec4<f32>(rgb + vec3<f32>(same), explicit_sample.a);
}
