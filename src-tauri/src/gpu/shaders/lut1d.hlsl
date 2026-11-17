/*****************************************************************************
Ported to HLSL for Better Cam from OBS Studio, plugins/obs-filters/data/color_grade_filter.effect (technique Draw1D,
pixel shader LUT1D). The constants come from color_grade_filter_update in plugins/obs-filters/color-grade-filter.c
(src/fx/lut.rs).

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

// Apply LUT, 1D tables (OBS "clut_filter"): every channel goes through its own curve, a size x 1 linear-light
// RGBA16F texture read with linear filtering, and is blended with the original by `clut_amount`. A channel outside
// DOMAIN_MIN..DOMAIN_MAX is left alone. See lut3d.hlsl for how OBS's linear light and premultiplied alpha map to
// this pipeline. OBS un-premultiplies here and does not premultiply again, which is the straight alpha this
// pipeline keeps anyway. The Params block has the layout of lut3d.hlsl; `mode` is not used.

Texture2D<float4> image : register(t0);
Texture2D<float4> clut_1d : register(t1);
SamplerState linear_clamp : register(s0);

cbuffer Params : register(b0)
{
    float3 clut_scale;
    float clut_amount;
    float3 clut_offset;
    uint mode;
    float3 domain_min;
    float pad0;
    float3 domain_max;
    float pad1;
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
    float3 code = p.rgb;
    float3 color = srgb_to_linear(code);

    if (code.r >= domain_min.r && code.r <= domain_max.r) {
        float u = code.r * clut_scale.r + clut_offset.r;
        float channel = clut_1d.SampleLevel(linear_clamp, float2(u, 0.5), 0).r;
        color.r = lerp(color.r, channel, clut_amount);
    }

    if (code.g >= domain_min.g && code.g <= domain_max.g) {
        float u = code.g * clut_scale.g + clut_offset.g;
        float channel = clut_1d.SampleLevel(linear_clamp, float2(u, 0.5), 0).g;
        color.g = lerp(color.g, channel, clut_amount);
    }

    if (code.b >= domain_min.b && code.b <= domain_max.b) {
        float u = code.b * clut_scale.b + clut_offset.b;
        float channel = clut_1d.SampleLevel(linear_clamp, float2(u, 0.5), 0).b;
        color.b = lerp(color.b, channel, clut_amount);
    }

    return float4(linear_to_srgb(color), p.a);
}
