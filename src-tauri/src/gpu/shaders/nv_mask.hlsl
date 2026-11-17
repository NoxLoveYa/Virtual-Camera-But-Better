/*****************************************************************************
Copyright (C) OBS Studio contributors (https://github.com/obsproject/obs-studio)

Ported to HLSL for Better Cam from OBS Studio, plugins/nv-filters/data/rtx_greenscreen.effect (technique Draw,
PSMask / Mask). The threshold comes from nvvfx draw_greenscreen_blur in plugins/nv-filters/nvidia-videofx-filter.c
(`min(filter->threshold, 0.95f)`, applied in src/fx/nvidia/filter.rs).

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

// NVIDIA Background Removal: the picture times a smooth step of the model's mask.
//
// OBS: rgba *= smoothstep(threshold - 0.1, threshold, mask.a), on a premultiplied picture, so colour and alpha both
// shrink. This pipeline keeps straight alpha: the colour stays and only the alpha is scaled; the output stage
// composites the result over the output background colour. The mask texture (A8) has the picture's size, so both
// are read with Load at the same pixel, which is what OBS's linear sample at the pixel centre comes to.

Texture2D<float4> image : register(t0);
Texture2D<float4> mask : register(t1);
SamplerState linear_clamp : register(s0);

cbuffer Params : register(b0)
{
    float threshold;
    float3 pad;
};

float4 main(float4 pos : SV_Position) : SV_Target
{
    int3 p = int3(int2(pos.xy), 0);
    float4 rgba = image.Load(p);
    rgba.a *= smoothstep(threshold - 0.1, threshold, mask.Load(p).a);
    return rgba;
}
