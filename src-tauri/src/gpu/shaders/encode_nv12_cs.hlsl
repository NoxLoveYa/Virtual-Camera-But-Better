// RGBA -> both NV12 planes in one dispatch, BT.709 limited range (the same maths as encode_nv12.hlsl, which draws the
// two planes in two passes and stays for sizes this one cannot take).
//
// Two render targets of one draw must have the same size, and luma is W x H while chroma is W/2 x H/2, so one pixel
// shader pass cannot write both planes. A compute shader can: every thread owns a block of 4 x 2 pixels, reads each of
// them once, and writes
//   luma:   two texels, the 4 luma bytes of each row of the block packed little endian into an R32_UINT texel
//           (the texture is W/4 x H; its bytes are the rows of the R8 plane),
//   chroma: one texel, [Cb Cr Cb Cr] of the two 2 x 2 blocks, each the mean of its four RGB pixels
//           (the texture is W/4 x H/2; its bytes are the rows of the R8G8 plane, W/2 samples of 2 bytes per row).
// R32_UINT is the one typed UAV format every feature level 11.0 device can store to. The byte values are what the
// R8_UNORM / R8G8_UNORM render targets of the two-pass version store: saturate, scale by 255, round to nearest.
// The rows come from `gpu::convert::rgb_to_yuv_rows` (range scaling and offsets included).

Texture2D<float4> image : register(t0);
RWTexture2D<uint> luma : register(u0);
RWTexture2D<uint> chroma : register(u1);

cbuffer EncodeParams : register(b0)
{
    float4 row_y; // Y  = dot(row_y, float4(r, g, b, 1))
    float4 row_u;
    float4 row_v;
    uint2 blocks; // W/4, H/2: the blocks of 4 x 2 pixels in a row and in a column
    uint2 pad;
};

uint byte_of(float v)
{
    return uint(saturate(v) * 255.0 + 0.5);
}

[numthreads(8, 8, 1)]
void main(uint3 id : SV_DispatchThreadID)
{
    if (id.x >= blocks.x || id.y >= blocks.y)
        return;

    int3 p = int3(int(id.x) * 4, int(id.y) * 2, 0);
    float3 c[8];
    [unroll]
    for (int j = 0; j < 2; j++)
    {
        [unroll]
        for (int i = 0; i < 4; i++)
            c[j * 4 + i] = image.Load(p + int3(i, j, 0)).rgb;
    }

    uint top = 0;
    uint bottom = 0;
    [unroll]
    for (int i = 0; i < 4; i++)
    {
        top |= byte_of(dot(row_y, float4(c[i], 1.0))) << (8 * i);
        bottom |= byte_of(dot(row_y, float4(c[4 + i], 1.0))) << (8 * i);
    }
    luma[uint2(id.x, id.y * 2)] = top;
    luma[uint2(id.x, id.y * 2 + 1)] = bottom;

    // Two chroma samples: the pixels 0, 1, 4, 5 and the pixels 2, 3, 6, 7.
    float4 left = float4((c[0] + c[1] + c[4] + c[5]) * 0.25, 1.0);
    float4 right = float4((c[2] + c[3] + c[6] + c[7]) * 0.25, 1.0);
    chroma[id.xy] = byte_of(dot(row_u, left)) | (byte_of(dot(row_v, left)) << 8)
        | (byte_of(dot(row_u, right)) << 16) | (byte_of(dot(row_v, right)) << 24);
}
