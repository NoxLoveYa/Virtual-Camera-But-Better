/*****************************************************************************
Better Cam's own Transform filter (flip and rotate); written for Better Cam, so there is no OBS filter to port. It does
what OBS's Edit Transform does to a scene item (mirror with a negative scale, then rotate about the centre; a positive
angle turns the picture clockwise), as a filter. The bilinear sampling is the one of libobs/data/default.effect.

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

// Each output pixel finds its place in the input: it is turned back by the angle about the centre of the picture, then
// the mirror is undone. Multiples of 90 degrees (`exact`) land on texel centres and copy the texel raw. Other angles
// sample bilinearly in linear light with a transparent border, so the edge of the rotated picture is smooth and the
// corners stay empty. The filter binds the plain views of the pooled textures for `exact` (the codes in, the codes
// out) and the `_SRGB` views otherwise (the hardware decodes on read and encodes on write, see geometry_common.hlsl).

cbuffer Params : register(b0)
{
    float2 out_dim;
    float2 flip;  // 1 = mirror that axis
    float2 rot;   // cos and sin of the angle (exact 0 and +-1 for multiples of 90 degrees)
    float exact;
    float pad;
};

float4 main(float4 pos : SV_Position) : SV_Target
{
    init_dims();
    float2 d = pos.xy - 0.5 * out_dim;
    // Inverse of the clockwise rotation in a y-down picture.
    float2 s = float2(d.x * rot.x + d.y * rot.y, -d.x * rot.y + d.y * rot.x);
    float2 src = s + 0.5 * float2(g_dim);
    if (flip.x > 0.5)
        src.x = float(g_dim.x) - src.x;
    if (flip.y > 0.5)
        src.y = float(g_dim.y) - src.y;

    if (exact > 0.5)
    {
        int2 p = int2(floor(src));
        if (any(p < 0) || any(p >= g_dim))
            return float4(0.0, 0.0, 0.0, 0.0);
        return image.Load(int3(p, 0));
    }
    return finish(sample_bilinear_border(src));
}
