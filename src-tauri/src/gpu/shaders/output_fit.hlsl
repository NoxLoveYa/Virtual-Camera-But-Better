// Output stage: place the filter chain result inside the output frame and composite it over the background colour.
// `dst_rect` is the destination rectangle in output pixels (it may reach past the frame for Fill, and may be
// smaller for Fit). Pixels outside it get the background.

Texture2D<float4> image : register(t0);
SamplerState linear_clamp : register(s0);

cbuffer FitParams : register(b0)
{
    float4 dst_rect;   // x, y, w, h in output pixels
    float4 background; // rgb
    uint point_mode;   // 1: the rectangle maps 1:1, copy texels exactly
    uint3 pad;
};

float4 main(float4 pos : SV_Position) : SV_Target
{
    float2 rel = pos.xy - dst_rect.xy;
    if (any(rel < 0.0) || any(rel >= dst_rect.zw))
        return float4(background.rgb, 1.0);
    float4 c = point_mode != 0 ? image.Load(int3(int2(rel), 0))
                               : image.SampleLevel(linear_clamp, rel / dst_rect.zw, 0);
    return float4(lerp(background.rgb, c.rgb, c.a), 1.0);
}
