/*****************************************************************************
Ported to HLSL for Better Cam from OBS Studio, libobs/data/bicubic_scale.effect (techniques Draw and
DrawUndistort; adapted there from the GPL bsnes shader). The size, the sampling choice and the undistort factor come
from plugins/obs-filters/scale-filter.c (src/fx/geometry/scale.rs).

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

// Bicubic, the "sharper" cubic convolution kernel (B = 0, C = 0.75) over 4 x 4 texels with the edge texels repeated.
//
// OBS saves fetches by merging the two middle taps of a row (and column) into one hardware bilinear sample placed at
// their weighted centre, and by loading the four corner taps directly. That is the same sum as 16 weighted texels,
// which is written out here, because the texels have to be decoded to linear light and premultiplied one by one
// (see geometry_common.hlsl). With undistort every tap is a bilinear sample at a bent x coordinate, as in OBS.

cbuffer Params : register(b0)
{
    float2 out_dim;
    float undistort_factor;
    float undistort;
};

float4 weight4(float x)
{
    /* Sharper version.  May look better in some cases. B=0, C=0.75 */
    return float4(
        ((-0.75 * x + 1.5) * x - 0.75) * x,
        (1.25 * x - 2.25) * x * x + 1.0,
        ((-1.25 * x + 1.5) * x + 0.75) * x,
        (0.75 * x - 0.75) * x * x);
}

float AspectUndistortX(float x, float a)
{
    // The higher the power, the longer the linear part will be.
    return (1.0 - a) * (x * x * x * x * x) + a * x;
}

float AspectUndistortU(float u)
{
    // Normalize texture coord to -1.0 to 1.0 range, and back.
    return AspectUndistortX((u - 0.5) * 2.0, undistort_factor) * 0.5 + 0.5;
}

// One tap of the undistort path: bilinear sample of the bent column `xpos` (in texels) at row `ypos` (in texels).
float4 undistort_pixel(float xpos, float ypos)
{
    float u = AspectUndistortU(xpos / float(g_dim.x));
    return sample_bilinear(float2(u * float(g_dim.x), ypos));
}

float4 main(float4 pos_in : SV_Position) : SV_Target
{
    init_dims();
    float2 pos = pos_in.xy / out_dim * float2(g_dim);
    float2 pos1 = floor(pos - 0.5) + 0.5;
    float2 f = pos - pos1;

    float4 rowtaps = weight4(f.x);
    float4 coltaps = weight4(f.y);
    int2 first = int2(pos1 - 0.5) - 1; // the texel of tap 0

    float4 total = float4(0.0, 0.0, 0.0, 0.0);
    [unroll]
    for (int j = 0; j < 4; j++)
    {
        float4 line_sum = float4(0.0, 0.0, 0.0, 0.0);
        [unroll]
        for (int i = 0; i < 4; i++)
        {
            if (undistort > 0.5)
                line_sum += undistort_pixel(pos1.x + float(i - 1), pos1.y + float(j - 1)) * rowtaps[i];
            else
                line_sum += fetch(first + int2(i, j)) * rowtaps[i];
        }
        total += line_sum * coltaps[j];
    }
    return finish(total);
}
