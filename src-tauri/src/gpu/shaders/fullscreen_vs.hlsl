// One triangle that covers the whole render target. No vertex or index buffer: call Draw(3, 0).
// The pixel shaders read SV_Position (pixel centres, x + 0.5) and fetch texels with Load, so no UVs are passed.

float4 main(uint id : SV_VertexID) : SV_Position
{
    float2 uv = float2((id << 1) & 2, id & 2);
    return float4(uv * float2(2.0, -2.0) + float2(-1.0, 1.0), 0.0, 1.0);
}
