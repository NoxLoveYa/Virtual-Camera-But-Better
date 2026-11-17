// Statistics downsample for Auto Adjust (Better Cam's own filter, src/fx/auto_adjust.rs).
//
// One stage of a chain that shrinks the filter's input to a 16x16 image of linear-light mean colours. Every
// destination pixel covers `src_size / dst_size` source texels (at most 8 per axis per stage). The footprint is split
// into `taps.x` x `taps.y` equal cells and each cell is read with one bilinear sample at its centre, as the preview
// downscale does, so a sample averages about 2x2 texels and the cells cover the footprint without gaps.
//
// The output holds the alpha-weighted mean colour in linear light in rgb and the mean alpha in a, so a translucent
// pixel counts for what it shows and a cleared area (alpha 0) counts for nothing. The first stage reads the
// gamma-encoded pipeline texture (`linearize` = 1: each tap is converted to linear light before it is averaged); the
// later stages read the previous stage's output, already linear (`linearize` = 0). The destination is
// R16G16B16A16_UNORM, so dark scenes keep their precision.

Texture2D<float4> image : register(t0);
SamplerState linear_clamp : register(s0);

cbuffer Params : register(b0)
{
    float2 src_size; // source size in texels
    float2 dst_size; // destination size in pixels
    uint2 taps;      // cells per axis, >= 1
    uint linearize;  // 1: the source is gamma encoded
    uint pad;
};

float3 srgb_to_linear(float3 c)
{
    return (c <= 0.04045) ? c / 12.92 : pow((c + 0.055) / 1.055, 2.4);
}

float4 main(float4 pos : SV_Position) : SV_Target
{
    float2 scale = src_size / dst_size;
    float2 start = (pos.xy - 0.5) * scale; // top-left of the footprint in source texels
    float3 sum = 0.0;
    float alpha_sum = 0.0;
    for (uint j = 0; j < taps.y; j++)
    {
        for (uint i = 0; i < taps.x; i++)
        {
            float2 cell = (float2(i, j) + 0.5) / float2(taps);
            float4 s = image.SampleLevel(linear_clamp, (start + cell * scale) / src_size, 0);
            float3 c = (linearize != 0) ? srgb_to_linear(s.rgb) : s.rgb;
            sum += c * s.a;
            alpha_sum += s.a;
        }
    }
    float3 mean = (alpha_sum > 1e-5) ? sum / alpha_sum : float3(0.0, 0.0, 0.0);
    return float4(mean, alpha_sum / float(taps.x * taps.y));
}
