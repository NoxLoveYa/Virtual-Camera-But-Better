// Shared by every convert_*.hlsl: the constant buffer and the YUV -> RGB step. Prepended by the shader loader.
//
// The matrix rows come from `gpu::convert::yuv_to_rgb_rows` and already fold in the range expansion and the
// chroma offset (the same idea as the `color_matrix` of OBS Studio's libobs/data/format_conversion.effect), so a
// shader only feeds raw normalised samples: R = dot(row_r, float4(y, u, v, 1)).

cbuffer ConvertParams : register(b0)
{
    float4 row_r;
    float4 row_g;
    float4 row_b;
    uint2 size;          // image size in pixels
    uint flip;           // 1: the source is bottom-up, read it upside down
    uint opaque;         // 1: force alpha = 1 (RGB sources without an alpha channel)
    float sample_scale;  // multiplier for raw samples (P010: 65535 / 65472, so 10 bit codes map to code / 1023)
    float3 pad;
};

int2 source_pixel(float4 pos)
{
    int2 p = int2(pos.xy);
    return int2(p.x, flip != 0 ? int(size.y) - 1 - p.y : p.y);
}

float4 yuv_to_rgba(float3 yuv)
{
    float4 v = float4(yuv, 1.0);
    return float4(saturate(float3(dot(row_r, v), dot(row_g, v), dot(row_b, v))), 1.0);
}
