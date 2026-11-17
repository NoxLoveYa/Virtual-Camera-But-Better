/*****************************************************************************
Based on the libretro shader sharpness.cg, converted to an OBS effect file by Nibbles.

Ported to HLSL for Better Cam from OBS Studio, plugins/obs-filters/data/sharpness.effect (technique Draw, PSDrawBare).
The settings come from plugins/obs-filters/sharpness-filter.c (src/fx/sharpen.rs).

This program is free software: you can redistribute it and/or modify
it under the terms of the GNU General Public License as published by
the Free Software Foundation, either version 2 of the License, or
(at your option) any later version.

This program is distributed in the hope that it will be useful,
but WITHOUT ANY WARRANTY; without even the implied warranty of
MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
GNU General Public License for more details.

You should have received a copy of the GNU General Public License
along with this program.  If not, see <http://www.gnu.org/licenses/>.
*****************************************************************************/

// Sharpen (OBS "sharpness_filter" v2).
//
// The kernel is OBS's: eight times the pixel minus its eight neighbours (a Laplacian), scaled by `sharpness` and added
// back, but only where the pixel differs from both neighbours of a pair (left and right, or above and below).
// OBS reads the neighbours with a linear sampler at offsets of exactly one texel, which are texel centres, so a
// plain load of the neighbouring texel is the same value; the clamped address mode becomes a clamp of the
// coordinate.
//
// OBS runs the effect on an sRGB texture with an sRGB framebuffer, so the maths is in linear light, and its textures
// hold premultiplied alpha, so the kernel runs on (r, g, b) * a and a together (alpha is sharpened as well). The
// working texture here is gamma-encoded with straight alpha, so the shader linearizes and premultiplies on load and
// does the opposite on the way out.

Texture2D<float4> image : register(t0);
SamplerState linear_clamp : register(s0);

cbuffer Params : register(b0)
{
    float sharpness;
    float3 pad;
};

float3 srgb_to_linear(float3 c)
{
    return (c <= 0.04045) ? c / 12.92 : pow((c + 0.055) / 1.055, 2.4);
}

float3 linear_to_srgb(float3 c)
{
    c = saturate(c);
    return (c <= 0.0031308) ? 12.92 * c : 1.055 * pow(c, 1.0 / 2.4) - 0.055;
}

// The texel at `p` (clamped into the picture) as OBS sees it: linear light, premultiplied.
float4 fetch(int2 p, int2 size)
{
    p = clamp(p, int2(0, 0), size - 1);
    float4 s = image.Load(int3(p, 0));
    return float4(srgb_to_linear(s.rgb) * s.a, s.a);
}

float4 main(float4 pos : SV_Position) : SV_Target
{
    uint w, h;
    image.GetDimensions(w, h);
    int2 size = int2(w, h);
    int2 c = int2(pos.xy);

    //  A  B  C
    //  D  E  F
    //  G  H  I
    float4 E = fetch(c, size);
    float4 A = fetch(c + int2(-1, -1), size);
    float4 B = fetch(c + int2(0, -1), size);
    float4 C = fetch(c + int2(1, -1), size);
    float4 D = fetch(c + int2(-1, 0), size);
    float4 F = fetch(c + int2(1, 0), size);
    float4 G = fetch(c + int2(-1, 1), size);
    float4 H = fetch(c + int2(0, 1), size);
    float4 I = fetch(c + int2(1, 1), size);

    float4 color = 8 * E;
    color -= A;
    color -= B;
    color -= C;
    color -= D;
    color -= F;
    color -= G;
    color -= H;
    color -= I;

    color = ((distance(E, F) != 0.0 && distance(E, D) != 0.0) || (distance(E, B) != 0.0 && distance(E, H) != 0.0))
        ? saturate(E + color * sharpness)
        : E;

    // Back to straight alpha and gamma-encoded light.
    float a = color.a;
    float3 rgb = (a > 0.0) ? color.rgb / a : float3(0.0, 0.0, 0.0);
    return float4(linear_to_srgb(rgb), a);
}
