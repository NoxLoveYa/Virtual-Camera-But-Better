// RGBA -> NV12 planes, BT.709 limited range. Two entry points, drawn into two render targets:
//   PSLuma   -> R8_UNORM   W x H     (Y)
//   PSChroma -> R8G8_UNORM W/2 x H/2 (Cb in R, Cr in G); each texel averages its 2x2 block of RGB pixels.
// The rows come from `gpu::convert::rgb_to_yuv_rows` (range scaling and offsets included).
// The conversion of OBS Studio's libobs/data/format_conversion.effect (PSNV12_Y / PSNV12_UV) does the same job.

Texture2D<float4> image : register(t0);

cbuffer EncodeParams : register(b0)
{
    float4 row_y; // Y  = dot(row_y, float4(r, g, b, 1))
    float4 row_u;
    float4 row_v;
};

float PSLuma(float4 pos : SV_Position) : SV_Target
{
    float3 c = image.Load(int3(int2(pos.xy), 0)).rgb;
    return dot(row_y, float4(c, 1.0));
}

float2 PSChroma(float4 pos : SV_Position) : SV_Target
{
    int3 p = int3(int2(pos.xy) * 2, 0);
    float3 c = image.Load(p).rgb;
    c += image.Load(p + int3(1, 0, 0)).rgb;
    c += image.Load(p + int3(0, 1, 0)).rgb;
    c += image.Load(p + int3(1, 1, 0)).rgb;
    float4 v = float4(c * 0.25, 1.0);
    return float2(dot(row_u, v), dot(row_v, v));
}
