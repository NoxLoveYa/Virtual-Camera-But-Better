/*****************************************************************************
Ported to HLSL for Better Cam from OBS Studio, libobs/data/lanczos_scale.effect (techniques Draw and
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

// Lanczos 3 over 6 x 6 texels with the edge texels repeated, the weights of a row normalised to sum to one.
//
// OBS merges the two middle taps into one hardware bilinear sample and loads the rest; that is the same sum as 36
// weighted texels, written out here because each texel has to be decoded to linear light and premultiplied (see
// geometry_common.hlsl). With undistort every tap is a bilinear sample at a bent x coordinate, as in OBS.

cbuffer Params : register(b0)
{
    float2 out_dim;
    float undistort_factor;
    float undistort;
};

float weight(float x)
{
    float x_pi = x * 3.141592654;
    // OBS divides 0 by 0 here and replaces the NaN with 1.0 at the call site (min(1.0, NaN)); do it explicitly.
    if (abs(x_pi) < 1e-5)
        return 1.0;
    return 3.0 * sin(x_pi) * sin(x_pi * (1.0 / 3.0)) / (x_pi * x_pi);
}

// The six weights for the distance `f_neg` (<= 0) from the sample position to the centre of the tap-2 texel,
// normalised: tap k lies at f_neg + k - 2.
void weight6(float f_neg, out float w[6])
{
    float sum = 0.0;
    [unroll]
    for (int k = 0; k < 6; k++)
    {
        w[k] = weight(f_neg + float(k - 2));
        sum += w[k];
    }
    float sum_i = 1.0 / sum;
    [unroll]
    for (int k2 = 0; k2 < 6; k2++)
        w[k2] *= sum_i;
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
    float2 pos2 = floor(pos - 0.5) + 0.5;
    float2 f_neg = pos2 - pos;

    float rowtap[6];
    float coltap[6];
    weight6(f_neg.x, rowtap);
    weight6(f_neg.y, coltap);
    int2 first = int2(pos2 - 0.5) - 2; // the texel of tap 0

    float4 total = float4(0.0, 0.0, 0.0, 0.0);
    [loop]
    for (int j = 0; j < 6; j++)
    {
        float4 line_sum = float4(0.0, 0.0, 0.0, 0.0);
        [unroll]
        for (int i = 0; i < 6; i++)
        {
            if (undistort > 0.5)
                line_sum += undistort_pixel(pos2.x + float(i - 2), pos2.y + float(j - 2)) * rowtap[i];
            else
                line_sum += fetch(first + int2(i, j)) * rowtap[i];
        }
        total += line_sum * coltap[j];
    }
    return finish(total);
}
