// BGRA / BGRX -> RGBA. The B8G8R8A8 texture format swizzles in the sampler, so the shader sees RGBA.
// Needs convert_common.hlsl (only `size`, `flip` and `opaque` matter).

Texture2D<float4> image : register(t0);

float4 main(float4 pos : SV_Position) : SV_Target
{
    float4 c = image.Load(int3(source_pixel(pos), 0));
    return float4(c.rgb, opaque != 0 ? 1.0 : c.a);
}
