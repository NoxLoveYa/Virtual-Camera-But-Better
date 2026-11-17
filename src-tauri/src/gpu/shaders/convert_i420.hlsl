// I420 (and YV12 with the two chroma planes bound the other way round) -> RGBA: three R8 planes, chroma at half
// size in both directions. Needs convert_common.hlsl.

Texture2D<float> plane_y : register(t0);
Texture2D<float> plane_u : register(t1);
Texture2D<float> plane_v : register(t2);

float4 main(float4 pos : SV_Position) : SV_Target
{
    int2 p = source_pixel(pos);
    int3 c = int3(p >> 1, 0);
    return yuv_to_rgba(float3(plane_y.Load(int3(p, 0)), plane_u.Load(c), plane_v.Load(c)));
}
