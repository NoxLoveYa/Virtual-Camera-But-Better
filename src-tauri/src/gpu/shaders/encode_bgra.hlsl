// RGBA -> BGRX for RGB32 consumers. The render target is B8G8R8A8_UNORM, so the swizzle happens in the output merger
// and the bytes land as B, G, R, 255.

Texture2D<float4> image : register(t0);

float4 main(float4 pos : SV_Position) : SV_Target
{
    return float4(image.Load(int3(int2(pos.xy), 0)).rgb, 1.0);
}
