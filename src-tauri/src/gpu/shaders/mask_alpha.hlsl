/*****************************************************************************
Copyright (C) OBS Studio contributors (https://github.com/obsproject/obs-studio)

Ported to HLSL for Better Cam from OBS Studio,
plugins/obs-filters/data/mask_alpha_filter.effect (technique Draw, PSAlphaMaskRGBA). The constants come from
mask_filter_update_v2 and mask_filter_render in plugins/obs-filters/mask-filter.c (src/fx/mask.rs).

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

// Alpha Mask (Alpha Channel) (OBS "mask_filter" v2, type mask_alpha_filter.effect).
//
// OBS runs the effect on an sRGB texture with an sRGB framebuffer, and binds the mask picture with
// gs_effect_set_texture_srgb: both pictures are linear light, filtered in linear light. The textures here are
// gamma-encoded, so the shader decodes and encodes itself, and filters the mask by hand (four Loads, decoded, then
// the bilinear weights) instead of with the sampler. OBS un-premultiplies the picture first and premultiplies the
// result; this pipeline keeps straight alpha, so both steps are left out. The mask picture is straight alpha too (OBS
// loads it with GS_IMAGE_ALPHA_STRAIGHT). For a translucent pixel whose colour overshoots 1 OBS clamps the
// premultiplied colour, this pipeline the straight one: a small difference at low alpha.
//
// uv2 = uv * mul_val + add_val places the mask over the picture (identity when the image is stretched; otherwise the
// image is fitted and its edge pixels repeat, as the clamped sampler of the effect does). `color` is the picked colour
// in linear light with the opacity in w.

Texture2D<float4> image : register(t0);
Texture2D<float4> target : register(t1);
SamplerState linear_clamp : register(s0);

cbuffer Params : register(b0)
{
    float4 color;
    float2 mul_val;
    float2 add_val;
    float2 target_size;
    float2 pad;
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

// One texel of the mask picture: linear colour, straight alpha.
float4 mask_texel(int2 p)
{
    int2 q = clamp(p, int2(0, 0), int2(target_size) - 1);
    float4 t = target.Load(int3(q, 0));
    return float4(srgb_to_linear(t.rgb), t.a);
}

// target.Sample(textureSampler, uv2): bilinear, clamped, filtered in linear light.
float4 sample_target(float2 uv)
{
    float2 tc = uv * target_size - 0.5;
    tc = clamp(tc, float2(-1.0, -1.0), target_size);
    float2 f = floor(tc);
    float2 w = tc - f;
    int2 i = int2(f);
    float4 top = lerp(mask_texel(i), mask_texel(i + int2(1, 0)), w.x);
    float4 bottom = lerp(mask_texel(i + int2(0, 1)), mask_texel(i + int2(1, 1)), w.x);
    return lerp(top, bottom, w.y);
}

float4 main(float4 pos : SV_Position) : SV_Target
{
    uint iw, ih;
    image.GetDimensions(iw, ih);
    float2 uv = pos.xy / float2(iw, ih);

    float4 t = image.Load(int3(int2(pos.xy), 0));
    float4 rgba = float4((t.a > 0.0) ? srgb_to_linear(t.rgb) : float3(0.0, 0.0, 0.0), t.a);
    rgba *= color;

    float4 target_rgba = sample_target(uv * mul_val + add_val);
    rgba.a *= target_rgba.a;

    return float4(linear_to_srgb(rgba.rgb), saturate(rgba.a));
}
