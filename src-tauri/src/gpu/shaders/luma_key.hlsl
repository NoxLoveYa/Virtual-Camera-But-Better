/*****************************************************************************
Copyright (C) OBS Studio contributors (https://github.com/obsproject/obs-studio)

Ported to HLSL for Better Cam from OBS Studio,
plugins/obs-filters/data/luma_key_filter_v2.effect (technique Draw, PSALumaKeyRGBA). The constants come from
luma_key_update in plugins/obs-filters/luma-key-filter.c (src/fx/keys.rs).

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

// Luma Key (OBS "luma_key_filter" v2).
//
// OBS runs the effect on an sRGB texture: the luminance is the Rec. 709 weighting of the LINEAR colour. The working
// texture here is gamma-encoded: the filter binds its `_SRGB` shader resource view at t0, so a load gives the linear
// colour to measure, and the plain view of the same texels at t1 for the colour itself, which goes out untouched
// through the plain render target view (OBS premultiplies it by the new alpha, which the straight-alpha pipeline
// leaves to the output stage). See gpu::Views::LINEAR_IN_CODES_OUT.

Texture2D<float4> image : register(t0);      // linear light (the `_SRGB` view)
Texture2D<float4> image_code : register(t1); // the same texels as stored (the codes)
SamplerState linear_clamp : register(s0);

cbuffer Params : register(b0)
{
    float luma_max;
    float luma_min;
    float luma_max_smooth;
    float luma_min_smooth;
};

// HLSL's smoothstep. Written out so the case OBS runs into at its defaults is fixed: with no smoothing the two edges
// are equal and the quotient is infinite (or 0/0 exactly on the edge); D3D saturates that to a step that is 0 at the
// edge itself, which is also what this does.
float smooth_step(float lo, float hi, float x)
{
    float d = hi - lo;
    float t = (d > 0.0) ? saturate((x - lo) / d) : ((x > lo) ? 1.0 : 0.0);
    return t * t * (3.0 - 2.0 * t);
}

float4 main(float4 pos : SV_Position) : SV_Target
{
    int3 p = int3(int2(pos.xy), 0);
    float4 t = image.Load(p);
    float3 rgb = (t.a > 0.0) ? t.rgb : float3(0.0, 0.0, 0.0);

    float3 luma_coef = float3(0.2126, 0.7152, 0.0722);
    float luminance = dot(rgb, luma_coef);

    float clo = smooth_step(luma_min, luma_min + luma_min_smooth, luminance);
    float chi = 1.0 - smooth_step(luma_max - luma_max_smooth, luma_max, luminance);

    float amask = clo * chi;
    return float4(image_code.Load(p).rgb, saturate(t.a * amask));
}
