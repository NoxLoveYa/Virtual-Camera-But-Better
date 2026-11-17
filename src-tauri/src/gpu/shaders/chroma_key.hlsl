/*****************************************************************************
Copyright (C) OBS Studio contributors (https://github.com/obsproject/obs-studio)

Ported to HLSL for Better Cam from OBS Studio,
plugins/obs-filters/data/chroma_key_filter_v2.effect (technique Draw, PSChromaKeyRGBA). The constants come from
chroma_key_update_v2 in plugins/obs-filters/chroma-key-filter.c (src/fx/keys.rs).

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

// Chroma Key (OBS "chroma_key_filter" v2).
//
// OBS runs the effect on an sRGB texture with an sRGB framebuffer, so `image.Sample` hands the shader linear light
// and the output merger encodes it again. The working texture here is gamma-encoded, so the shader decodes and
// encodes itself. The chroma distance is measured on the encoded colour (OBS converts back with GetNonlinearColor).
//
// OBS's box filter reads four neighbours with the hardware's bilinear filter at offsets of one and a half pixels;
// each of those samples sits exactly between two texels, so it is the mean of two texels in linear light. They are
// taken here with Load and averaged the same way. OBS's texture holds premultiplied alpha, so its neighbour samples
// are premultiplied (the centre sample is un-premultiplied first); the neighbours are multiplied by their alpha
// here to match. For an opaque picture, which is what a camera delivers, none of this matters.
//
// OBS un-premultiplies, keys, and premultiplies again. This pipeline keeps straight alpha, so the colour goes out
// as it is and only alpha carries the key. One difference remains for translucent pixels: OBS clamps the premultiplied
// colour, which lets a channel that overshoots 1 (brightness, contrast) survive at low alpha; the straight colour is
// clamped to 1 before it is composited.

Texture2D<float4> image : register(t0);
SamplerState linear_clamp : register(s0);

cbuffer Params : register(b0)
{
    float2 chroma_key;
    float opacity;
    float contrast;
    float brightness;
    float gamma;
    float similarity;
    float smoothness;
    float spill;
    float3 pad;
};

static const float3 cb_v3 = float3(-0.100644, -0.338572, 0.439216);
static const float cb_off = 0.501961;
static const float3 cr_v3 = float3(0.439216, -0.398942, -0.040274);
static const float cr_off = 0.501961;

float3 srgb_to_linear(float3 c)
{
    return (c <= 0.04045) ? c / 12.92 : pow((c + 0.055) / 1.055, 2.4);
}

float get_nonlinear_channel(float u)
{
    return (u <= 0.0031308) ? (12.92 * u) : ((1.055 * pow(u, 1.0 / 2.4)) - 0.055);
}

float3 get_nonlinear_color(float3 rgb)
{
    return float3(get_nonlinear_channel(rgb.r), get_nonlinear_channel(rgb.g), get_nonlinear_channel(rgb.b));
}

float3 linear_to_srgb(float3 c)
{
    return get_nonlinear_color(saturate(c));
}

float get_chroma_dist(float3 rgb)
{
    float cb = dot(rgb, cb_v3) + cb_off;
    float cr = dot(rgb, cr_v3) + cr_off;
    return distance(chroma_key, float2(cb, cr));
}

// One texel as OBS's sampler would give it: linear light, premultiplied by alpha. The coordinate is clamped to the
// picture (the sampler's address mode).
float3 texel_premultiplied(int2 p, int2 size)
{
    float4 t = image.Load(int3(clamp(p, int2(0, 0), size - 1), 0));
    return srgb_to_linear(t.rgb) * t.a;
}

// A sample halfway between two texels, as an encoded colour (SampleTexture in the effect).
float3 sample_between(int2 a, int2 b, int2 size)
{
    return get_nonlinear_color(0.5 * (texel_premultiplied(a, size) + texel_premultiplied(b, size)));
}

float4 main(float4 pos : SV_Position) : SV_Target
{
    int2 p = int2(pos.xy);
    uint w, h;
    image.GetDimensions(w, h);
    int2 size = int2(w, h);

    float4 t = image.Load(int3(p, 0));
    float3 rgb = (t.a > 0.0) ? srgb_to_linear(t.rgb) : float3(0.0, 0.0, 0.0);

    // GetBoxFilteredChromaDist: the offsets are (1, 0.5) and (0.5, -1) pixels.
    float dist_val = get_chroma_dist(sample_between(p + int2(-1, -1), p + int2(-1, 0), size));   // uv - point_0
    dist_val += get_chroma_dist(sample_between(p + int2(1, 0), p + int2(1, 1), size));           // uv + point_0
    dist_val += get_chroma_dist(sample_between(p + int2(-1, 1), p + int2(0, 1), size));          // uv - point_1
    dist_val += get_chroma_dist(sample_between(p + int2(0, -1), p + int2(1, -1), size));         // uv + point_1
    dist_val *= 2.0;
    dist_val += get_chroma_dist(get_nonlinear_color(rgb));
    float chroma_dist = dist_val / 9.0;

    // ProcessChromaKey
    float base_mask = chroma_dist - similarity;
    float full_mask = pow(saturate(base_mask / smoothness), 1.5);
    float spill_val = pow(saturate(base_mask / spill), 1.5);

    float a = t.a * opacity;
    a *= full_mask;

    float desat = dot(rgb, float3(0.2126, 0.7152, 0.0722));
    rgb = lerp(float3(desat, desat, desat), rgb, spill_val);

    // CalcColor
    rgb = pow(rgb, float3(gamma, gamma, gamma)) * contrast + brightness;

    return float4(linear_to_srgb(rgb), saturate(a));
}
