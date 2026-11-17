/*****************************************************************************
Ported to HLSL for Better Cam from OBS Studio, plugins/obs-filters/data/crop_filter.effect (technique Draw,
PSCrop). The size and the offset come from calc_crop_dimensions in plugins/obs-filters/crop-filter.c
(src/fx/geometry/crop.rs).

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

// Crop/Pad. OBS maps the output quad to the uv rectangle `uv * mul_val + add_val` of the input and samples it with a
// linear sampler and a transparent border. Output texel n then lies at input texel n + (left, top): a shift by whole
// texels, where every sample falls on a texel centre and the linear filter returns that texel itself. So the shader
// reads the texel with Load, and a position outside the input (negative crop = padding) is transparent.

Texture2D<float4> image : register(t0);

cbuffer Params : register(b0)
{
    int2 offset; // the input texel that output texel (0, 0) shows: (left, top)
    int2 pad;
};

float4 main(float4 pos : SV_Position) : SV_Target
{
    uint w, h;
    image.GetDimensions(w, h);
    int2 p = int2(pos.xy) + offset;
    if (any(p < 0) || any(p >= int2(w, h)))
        return float4(0.0, 0.0, 0.0, 0.0);
    return image.Load(int3(p, 0));
}
