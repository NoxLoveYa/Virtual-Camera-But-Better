/*****************************************************************************
Ported to HLSL for Better Cam from OBS Studio, libobs/data/area.effect (techniques Draw: DrawArea, and DrawUpscale:
DrawAreaUpscale). Which of the two runs is decided in plugins/obs-filters/scale-filter.c (src/fx/geometry/scale.rs).

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

// Area scaling: every output pixel is the average of the input over the rectangle it covers, each input texel
// weighted by the area of its overlap. OBS reads the size of an output pixel in uv from ddx/ddy of the quad's uv; for
// a full-target quad that is exactly 1 / output size, passed in directly.

cbuffer Params : register(b0)
{
    float2 out_dim;
    float upscale; // 1 when the output is at least as large as the input in both directions (DrawAreaUpscale)
    float pad;
};

float4 main(float4 pos : SV_Position) : SV_Target
{
    init_dims();
    float2 base_dimension = float2(g_dim);
    float2 base_dimension_i = 1.0 / base_dimension;
    float2 uv = pos.xy / out_dim;
    float2 uv_delta = 1.0 / out_dim;

    float2 uv_min = uv - 0.5 * uv_delta;
    float2 uv_max = uv_min + uv_delta;

    if (upscale > 0.5)
    {
        float2 load_index_first = floor(uv_min * base_dimension);
        float2 load_index_last = ceil(uv_max * base_dimension) - 1.0;

        if (load_index_first.x < load_index_last.x)
        {
            float uv_boundary_x = load_index_last.x * base_dimension_i.x;
            uv.x = ((uv.x - uv_boundary_x) / uv_delta.x) * base_dimension_i.x + uv_boundary_x;
        }
        else
            uv.x = (load_index_first.x + 0.5) * base_dimension_i.x;
        if (load_index_first.y < load_index_last.y)
        {
            float uv_boundary_y = load_index_last.y * base_dimension_i.y;
            uv.y = ((uv.y - uv_boundary_y) / uv_delta.y) * base_dimension_i.y + uv_boundary_y;
        }
        else
            uv.y = (load_index_first.y + 0.5) * base_dimension_i.y;

        return finish(sample_bilinear(uv * base_dimension));
    }

    float2 load_index_begin = floor(uv_min * base_dimension);
    float2 load_index_end = ceil(uv_max * base_dimension);

    float2 target_dimension = 1.0 / uv_delta;
    float2 target_pos = uv * target_dimension;
    float2 target_pos_min = target_pos - 0.5;
    float2 target_pos_max = target_pos + 0.5;
    float2 scale = base_dimension_i * target_dimension;

    float4 total_color = float4(0.0, 0.0, 0.0, 0.0);

    // At most about three texels per direction: below half size OBS takes the low-res bilinear path instead. The
    // loop bound only keeps a wrong size from running away.
    int y_count = clamp(int(load_index_end.y - load_index_begin.y), 0, 8);
    int x_count = clamp(int(load_index_end.x - load_index_begin.x), 0, 8);
    [loop]
    for (int jy = 0; jy < y_count; jy++)
    {
        float load_index_y = load_index_begin.y + float(jy);
        float source_y_min = load_index_y * scale.y;
        float source_y_max = source_y_min + scale.y;
        float y_min = max(source_y_min, target_pos_min.y);
        float y_max = min(source_y_max, target_pos_max.y);
        float height = max(y_max - y_min, 0.0);

        [loop]
        for (int ix = 0; ix < x_count; ix++)
        {
            float load_index_x = load_index_begin.x + float(ix);
            float source_x_min = load_index_x * scale.x;
            float source_x_max = source_x_min + scale.x;
            float x_min = max(source_x_min, target_pos_min.x);
            float x_max = min(source_x_max, target_pos_max.x);
            float width = max(x_max - x_min, 0.0);
            float area = width * height;

            total_color += area * fetch(int2(load_index_x, load_index_y));
        }
    }

    return finish(total_color);
}
