/*****************************************************************************
Ported to HLSL for Better Cam from OBS Studio, plugins/obs-filters/data/color_grade_filter.effect (the techniques
Draw3D, DrawAlpha3D, DrawAmount3D and DrawDomain3D: pixel shaders LUT3D, LUTAlpha3D, LUTAmount3D and LUTDomain3D).
The constants come from color_grade_filter_update in plugins/obs-filters/color-grade-filter.c (src/fx/lut.rs).

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

// Apply LUT, 3D tables (OBS "clut_filter"). The table is a linear-light RGBA16F Texture3D, looked up by the hardware's
// trilinear filter, as in OBS.
//
// OBS runs the effect on an sRGB texture with an sRGB framebuffer: the picture arrives as linear light and is turned
// back into gamma-encoded values (`nonlinear`) for the lookup coordinate; the table holds linear values, and the
// result is encoded by the framebuffer. The working texture here is gamma-encoded, so `code` (what OBS recovers as
// `nonlinear`) is the loaded value itself, and the shader converts to linear for the blend and back at the end.
//
// OBS also un-premultiplies before the lookup and premultiplies after (its textures hold premultiplied alpha). The
// working texture here is straight, so those techniques need neither step. Only `mode 0` (Draw3D, "Passthrough
// Alpha") skips the un-premultiply in OBS: it looks up the premultiplied colour, and its result is premultiplied.
// This shader premultiplies on the way in and divides alpha out again on the way out for that mode.
//
// mode 0  Draw3D        passthrough_alpha on, amount 1, no domain: the lookup of the premultiplied colour
// mode 1  DrawAlpha3D   amount 1, no domain: the lookup of the colour
// mode 2  DrawAmount3D  amount below 1, no domain: lerp(colour, lookup, amount)
// mode 3  DrawDomain3D  a domain narrower than 0..1: as mode 2, but only inside the domain

Texture2D<float4> image : register(t0);
Texture3D<float4> clut_3d : register(t1);
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

float3 lookup(float3 nonlinear)
{
    return clut_3d.SampleLevel(linear_clamp, nonlinear * clut_scale + clut_offset, 0).rgb;
}

float4 main(float4 pos : SV_Position) : SV_Target
{
    float4 p = image.Load(int3(int2(pos.xy), 0));
    float3 code = p.rgb;
    float3 lit = srgb_to_linear(code);
    float3 result;

    if (mode == 0) {
        float3 premultiplied = lit * p.a;
        float3 looked = lookup(linear_to_srgb(premultiplied));
        result = (p.a > 0.0) ? looked / p.a : float3(0.0, 0.0, 0.0);
    } else if (mode == 1) {
        result = lookup(code);
    } else if (mode == 2) {
        result = lerp(lit, lookup(code), clut_amount);
    } else {
        bool inside = all(code >= domain_min) && all(code <= domain_max);
        result = inside ? lerp(lit, lookup(code), clut_amount) : lit;
    }

    return float4(linear_to_srgb(result), p.a);
}
