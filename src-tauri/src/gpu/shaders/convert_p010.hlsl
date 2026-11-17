// P010 -> RGBA. Same layout as NV12 with 16 bit samples that hold a 10 bit code in the top bits. The 10 bit range
// constants live in the matrix rows (see gpu::convert), `sample_scale` turns the raw 16 bit value into code / 1023.
// Needs convert_common.hlsl.

Texture2D<float> luma : register(t0);    // R16_UNORM
Texture2D<float2> chroma : register(t1); // R16G16_UNORM

float4 main(float4 pos : SV_Position) : SV_Target
{
    int2 p = source_pixel(pos);
    float y = luma.Load(int3(p, 0)) * sample_scale;
    float2 uv = chroma.Load(int3(p >> 1, 0)) * sample_scale;
    return yuv_to_rgba(float3(y, uv.x, uv.y));
}
