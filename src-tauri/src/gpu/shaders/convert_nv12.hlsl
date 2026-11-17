// NV12 and P010 -> RGBA. Plane 0 is luma (R8 or R16), plane 1 is interleaved Cb/Cr at half size (R8G8 or R16G16).
// Chroma is point sampled, as OBS does. Needs convert_common.hlsl.

Texture2D<float> luma : register(t0);
Texture2D<float2> chroma : register(t1);

float4 main(float4 pos : SV_Position) : SV_Target
{
    int2 p = source_pixel(pos);
    float y = luma.Load(int3(p, 0)) * sample_scale;
    float2 uv = chroma.Load(int3(p >> 1, 0)) * sample_scale;
    return yuv_to_rgba(float3(y, uv.x, uv.y));
}
