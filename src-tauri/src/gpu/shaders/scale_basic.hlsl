/*****************************************************************************
Ported to HLSL for Better Cam from OBS Studio: libobs/data/default.effect (technique Draw, PSDrawBare: the Point and
Bilinear scaling of the Scaling/Aspect Ratio filter) and libobs/data/bilinear_lowres_scale.effect (technique Draw,
DrawLowresBilinear: what OBS uses for every filtering mode but Point when the output is below half the input size).
The mode is chosen in plugins/obs-filters/scale-filter.c (src/fx/geometry/scale.rs).

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

cbuffer Params : register(b0)
{
    float2 out_dim;
    float undistort_factor; // unused here
    float mode;             // 0 point, 1 bilinear, 2 low-res bilinear
};

float4 main(float4 pos : SV_Position) : SV_Target
{
    init_dims();
    // OBS draws a quad whose uv runs 0..1 over the output, so the pixel centre's uv is (x + 0.5) / width.
    float2 uv = pos.xy / out_dim;
    float2 dim = float2(g_dim);

    if (mode < 0.5)
    {
        // Point sampler (all-zero gs_sampler_info: point filter, clamp): the texel the uv falls in. Copied raw, so
        // a pixel keeps its exact code values.
        int2 p = clamp(int2(floor(uv * dim)), int2(0, 0), g_dim - 1);
        return image.Load(int3(p, 0));
    }
    if (mode < 1.5)
    {
        return finish(sample_bilinear(uv * dim));
    }

    // DrawLowresBilinear: eight bilinear samples in the pattern Direct3D uses for 8x multisampling, within one
    // output pixel (stepxy is one output pixel in uv).
    float2 stepxy = 1.0 / out_dim;
    float2 stepxy1 = stepxy * 0.0625;
    float2 stepxy3 = stepxy * 0.1875;
    float2 stepxy5 = stepxy * 0.3125;
    float2 stepxy7 = stepxy * 0.4375;
    float4 sum = sample_bilinear((uv + float2( stepxy1.x, -stepxy3.y)) * dim);
    sum += sample_bilinear((uv + float2(-stepxy1.x,  stepxy3.y)) * dim);
    sum += sample_bilinear((uv + float2( stepxy5.x,  stepxy1.y)) * dim);
    sum += sample_bilinear((uv + float2(-stepxy3.x, -stepxy5.y)) * dim);
    sum += sample_bilinear((uv + float2(-stepxy5.x,  stepxy5.y)) * dim);
    sum += sample_bilinear((uv + float2(-stepxy7.x, -stepxy1.y)) * dim);
    sum += sample_bilinear((uv + float2( stepxy3.x,  stepxy7.y)) * dim);
    sum += sample_bilinear((uv + float2( stepxy7.x, -stepxy7.y)) * dim);
    return finish(sum * 0.125);
}
