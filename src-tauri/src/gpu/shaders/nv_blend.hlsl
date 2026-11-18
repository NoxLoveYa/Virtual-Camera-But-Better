/*****************************************************************************
Copyright (C) OBS Studio contributors (https://github.com/obsproject/obs-studio)

Ported to HLSL for Better Cam from OBS Studio, plugins/nv-filters/data/rtx_blur.effect (technique Draw, PSMask /
Mask).

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

// NVIDIA Blur and Background Blur: the blurred picture where the mask says "background", the sharp picture where it
// says "person", and a smooth step between them.
//
// OBS: m = smoothstep(0.85, 0.95, mask.a); out = lerp(blur, image, m), in linear light (OBS samples the picture as
// sRGB and converts the blurred texture with srgb_nonlinear_to_linear), alpha taken from the picture. The working
// texture here is gamma-encoded: the filter binds the picture through its `_SRGB` shader resource view (a load gives
// linear light) and writes through the `_SRGB` render target view (the value returned is encoded by the output
// merger), see gpu::Views::LINEAR. The blurred texture is NVIDIA's own `R8G8B8A8_UNORM` texture (the SDK wants a typed
// one), so the shader decodes that one by hand. The plain Blur filter never sets the mask in
// OBS (an unset texture reads as 0), so m is 0 and the result is the blurred picture: `use_mask` is 0 for it.

Texture2D<float4> image : register(t0);   // linear light (the `_SRGB` view)
Texture2D<float4> blurred : register(t1);  // codes (decoded below)
Texture2D<float4> mask : register(t2);
SamplerState linear_clamp : register(s0);

cbuffer Params : register(b0)
{
    float use_mask;
    float3 pad;
};

float3 srgb_to_linear(float3 c)
{
    return (c <= 0.04045) ? c / 12.92 : pow((c + 0.055) / 1.055, 2.4);
}

float4 main(float4 pos : SV_Position) : SV_Target
{
    int3 p = int3(int2(pos.xy), 0);
    float4 rgba = image.Load(p);
    float3 blur = srgb_to_linear(blurred.Load(p).rgb);
    float m = use_mask > 0.5 ? smoothstep(0.85, 0.95, mask.Load(p).a) : 0.0;
    float3 out_lin = lerp(blur, rgba.rgb, m);
    // The `_SRGB` render target clamps to 0..1 and encodes.
    return float4(saturate(out_lin), rgba.a);
}
