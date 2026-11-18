/*****************************************************************************
Copyright (C) OBS Studio contributors (https://github.com/obsproject/obs-studio)

Ported to HLSL for Better Cam from OBS Studio,
plugins/obs-filters/data/color_key_filter_v2.effect (technique Draw, PSColorKeyRGBA). The constants come from
color_key_update_v2 in plugins/obs-filters/color-key-filter.c (src/fx/keys.rs).

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

// Color Key (OBS "color_key_filter" v2).
//
// OBS runs the effect on an sRGB texture with an sRGB framebuffer: the shader sees linear light, measures the colour
// distance on the encoded colour (GetNonlinearColor), and applies gamma, contrast and brightness to the linear colour.
// The working texture here is gamma-encoded: the filter binds its `_SRGB` shader resource view at t0 (a load gives
// linear light) and its `_SRGB` render target view (the value returned is encoded by the output merger), and the plain
// view of the same texels at t1 for the encoded colour, see gpu::Views::LINEAR_WITH_CODES. OBS un-premultiplies before and
// premultiplies after because its textures hold premultiplied alpha; this pipeline keeps straight alpha, so both steps
// are left out and only alpha carries the key. For a translucent pixel whose colour overshoots 1 (brightness,
// contrast) OBS clamps the premultiplied colour, this pipeline the straight one: a small difference at low alpha.

Texture2D<float4> image : register(t0);      // linear light (the `_SRGB` view)
Texture2D<float4> image_code : register(t1); // the same texels as stored (the codes)
SamplerState linear_clamp : register(s0);

cbuffer Params : register(b0)
{
    float4 key_color;
    float opacity;
    float contrast;
    float brightness;
    float gamma;
    float similarity;
    float smoothness;
    float2 pad;
};

float4 main(float4 pos : SV_Position) : SV_Target
{
    int3 p = int3(int2(pos.xy), 0);
    float4 t = image.Load(p);
    float3 rgb = (t.a > 0.0) ? t.rgb : float3(0.0, 0.0, 0.0);
    // The encoded colour OBS measures with (GetNonlinearColor of the decoded texel, which is the code itself).
    float3 encoded = (t.a > 0.0) ? image_code.Load(p).rgb : float3(0.0, 0.0, 0.0);
    float a = t.a * opacity;

    // ProcessColorKey
    float color_dist = distance(key_color.rgb, encoded);
    a *= saturate(max(color_dist - similarity, 0.0) / smoothness);

    // CalcColor
    rgb = pow(rgb, float3(gamma, gamma, gamma)) * contrast + brightness;

    // The `_SRGB` render target clamps to 0..1 and encodes.
    return float4(saturate(rgb), saturate(a));
}