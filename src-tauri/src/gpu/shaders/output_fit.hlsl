// Output stage: place the filter chain result inside the output frame and composite it over the background colour.
// `dst_rect` is the destination rectangle in output pixels (it may reach past the frame for Fill, and may be
// smaller for Fit). Pixels outside it get the background.
//
// The working texture holds gamma-encoded sRGB with straight (not premultiplied) alpha. Translucent pixels (the
// Color Correction opacity, later keys and masks) are composited over the background in linear light, the way OBS
// blends a source over what is behind it. Opaque pixels and pixels outside the picture are copied exactly. Where a
// scaled sample touches translucent texels, the four neighbours are weighted by their alpha, so the colour of a fully
// transparent texel never leaks into the edge of a picture.

Texture2D<float4> image : register(t0);
SamplerState linear_clamp : register(s0);

cbuffer FitParams : register(b0)
{
    float4 dst_rect;   // x, y, w, h in output pixels
    float4 background; // rgb
    uint point_mode;   // 1: the rectangle maps 1:1, copy texels exactly
    uint3 pad;
};

float3 srgb_to_linear(float3 c)
{
    return (c <= 0.04045) ? c / 12.92 : pow((c + 0.055) / 1.055, 2.4);
}

float3 linear_to_srgb(float3 c)
{
    c = saturate(c);
    return (c <= 0.0031308) ? 12.92 * c : 1.055 * pow(c, 1.0 / 2.4) - 0.055;
}

// Bilinear sample at `uv` (0..1) with the colour weighted by alpha; returns straight colour and the filtered alpha.
float4 sample_alpha_weighted(float2 uv)
{
    uint w, h;
    image.GetDimensions(w, h);
    float2 size = float2(w, h);
    float2 p = uv * size - 0.5;
    float2 base = floor(p);
    float2 f = p - base;
    int2 i0 = clamp(int2(base), int2(0, 0), int2(w - 1, h - 1));
    int2 i1 = clamp(int2(base) + 1, int2(0, 0), int2(w - 1, h - 1));
    float4 c00 = image.Load(int3(i0.x, i0.y, 0));
    float4 c10 = image.Load(int3(i1.x, i0.y, 0));
    float4 c01 = image.Load(int3(i0.x, i1.y, 0));
    float4 c11 = image.Load(int3(i1.x, i1.y, 0));
    float w00 = (1.0 - f.x) * (1.0 - f.y) * c00.a;
    float w10 = f.x * (1.0 - f.y) * c10.a;
    float w01 = (1.0 - f.x) * f.y * c01.a;
    float w11 = f.x * f.y * c11.a;
    float a = w00 + w10 + w01 + w11;
    float3 rgb = (c00.rgb * w00 + c10.rgb * w10 + c01.rgb * w01 + c11.rgb * w11) / max(a, 1e-6);
    return float4(rgb, a);
}

float4 main(float4 pos : SV_Position) : SV_Target
{
    float2 rel = pos.xy - dst_rect.xy;
    if (any(rel < 0.0) || any(rel >= dst_rect.zw))
        return float4(background.rgb, 1.0);
    float4 c;
    if (point_mode != 0)
    {
        c = image.Load(int3(int2(rel), 0));
    }
    else
    {
        float2 uv = rel / dst_rect.zw;
        c = image.SampleLevel(linear_clamp, uv, 0);
        if (c.a < 0.9999)
            c = sample_alpha_weighted(uv);
    }
    if (c.a >= 0.9999)
        return float4(c.rgb, 1.0);
    float3 lin = lerp(srgb_to_linear(background.rgb), srgb_to_linear(c.rgb), c.a);
    return float4(linear_to_srgb(lin), 1.0);
}
