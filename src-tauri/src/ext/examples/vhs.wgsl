@fragment
fn main(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
  var uv = pos.xy * frame.inv_size;

  // Horizontal tearing in thin bands while the script reports a glitch.
  let band = step(0.97, fract(uv.y * 7.0 + frame.time * 3.0));
  let jitter = hash(vec2<f32>(floor(uv.y * 90.0), frame.time)) - 0.5;
  uv.x = uv.x + params.glitch * band * 0.03 * jitter;

  // Colour bleed: red and blue sampled a few pixels apart.
  let dx = vec2<f32>(params.bleed * frame.inv_size.x, 0.0);
  let base = tex(image, uv);
  var rgb = vec3<f32>(tex(image, uv + dx).r, base.g, tex(image, uv - dx).b);

  // Scanlines on every other output row, then grain that changes every frame.
  let line = 0.5 + 0.5 * sin(pos.y * 3.14159265);
  rgb = rgb * (1.0 - params.scanlines * 0.35 * line);
  rgb = rgb + (hash(pos.xy + vec2<f32>(frame.seed * 1000.0, 0.0)) - 0.5) * params.noise * 0.25;

  return vec4<f32>(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)), base.a);
}
