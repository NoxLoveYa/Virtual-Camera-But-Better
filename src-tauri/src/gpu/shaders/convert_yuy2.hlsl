// YUY2 (Y0 U Y1 V per two pixels) -> RGBA. The texture is an R8G8B8A8 view that is half as wide as the image:
// one texel = Y0, U, Y1, V. Needs convert_common.hlsl.

Texture2D<float4> packed : register(t0);

float4 main(float4 pos : SV_Position) : SV_Target
{
    int2 p = source_pixel(pos);
    float4 t = packed.Load(int3(p.x >> 1, p.y, 0));
    float y = (p.x & 1) != 0 ? t.z : t.x;
    return yuv_to_rgba(float3(y, t.y, t.w));
}
