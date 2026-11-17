// Preview downscale: box-filter the output frame into a smaller texture of the same aspect ratio.
//
// Every destination pixel covers `scale` x `scale` source texels. The footprint is split into `taps.x` x `taps.y`
// equal cells and each cell is read with one bilinear sample at its centre, so a sample averages about 2x2 texels
// and the cells together cover the footprint without gaps. With a scale of 1 (same size) the single tap lands on
// the texel centre and the copy is exact. The frame is opaque by then; alpha is forced to 1.

Texture2D<float4> image : register(t0);
SamplerState linear_clamp : register(s0);

cbuffer DownParams : register(b0)
{
    float2 src_size; // source size in texels
    float2 dst_size; // destination size in pixels
    uint2 taps;      // cells per axis, >= 1
    uint2 pad;
};

float4 main(float4 pos : SV_Position) : SV_Target
{
    float2 scale = src_size / dst_size;
    float2 start = (pos.xy - 0.5) * scale; // top-left of the footprint in source texels
    float3 sum = 0.0;
    for (uint j = 0; j < taps.y; j++)
    {
        for (uint i = 0; i < taps.x; i++)
        {
            float2 cell = (float2(i, j) + 0.5) / float2(taps);
            sum += image.SampleLevel(linear_clamp, (start + cell * scale) / src_size, 0).rgb;
        }
    }
    return float4(sum / float(taps.x * taps.y), 1.0);
}
