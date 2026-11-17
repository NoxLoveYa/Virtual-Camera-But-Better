/*****************************************************************************
Copyright (C) 2016 by c3r1c3 <c3r1c3@nevermindonline.com>

Ported to HLSL for Better Cam from OBS Studio,
plugins/obs-filters/data/color_correction_filter.effect (technique Draw, PSColorFilterRGBA). The matrix and the gamma
exponent come from color_correction_filter_update_v2 in plugins/obs-filters/color-correction-filter.c (src/fx/color.rs).

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

// Color Correction (OBS "color_filter" v2).
//
// OBS runs the effect on an sRGB texture with an sRGB framebuffer (gs_effect_set_texture_srgb, framebuffer sRGB on),
// so the sampler hands the shader linear light and the output merger encodes it again: gamma, the matrix, the colour
// multiply and the colour add all work on linear values. The working texture here is gamma-encoded, so the shader does
// both conversions itself.
//
// OBS also un-premultiplies the sample before the maths and premultiplies the result afterwards, because its
// textures hold premultiplied alpha. This pipeline keeps straight alpha, so both steps are left out; the matrix
// sees the same straight colour and the same alpha, and the output stage composites the straight result.
//
// color_matrix holds OBS's matrix4 rows (x, y, z, t) in memory order. OBS multiplies the pixel as a row vector from
// the left: out = (r, g, b, a) * M, so the translation row t is scaled by alpha.

Texture2D<float4> image : register(t0);
SamplerState linear_clamp : register(s0);

cbuffer Params : register(b0)
{
    row_major float4x4 color_matrix;
    float gamma;
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

float4 main(float4 pos : SV_Position) : SV_Target
{
    float4 p = image.Load(int3(int2(pos.xy), 0));
    p.rgb = srgb_to_linear(p.rgb);

    // Always address the gamma first.
    p.rgb = pow(p.rgb, float3(gamma, gamma, gamma));

    p = mul(p, color_matrix);

    return float4(linear_to_srgb(p.rgb), saturate(p.a));
}
