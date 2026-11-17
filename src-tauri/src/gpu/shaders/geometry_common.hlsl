/*****************************************************************************
Better Cam helpers for the geometry filters (Crop/Pad, Scaling/Aspect Ratio, Transform). Written for Better Cam;
the scaling shaders that include it port libobs/data/{default,bilinear_lowres_scale,bicubic_scale,lanczos_scale,area}.effect
from OBS Studio.

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

// OBS filters the picture in linear light on premultiplied alpha: its textures are sRGB views, so the sampler decodes
// before it filters, and the framebuffer encodes again. The working texture here holds gamma-encoded sRGB with
// straight alpha, so the scaling shaders do both steps themselves: `fetch` returns a texel as premultiplied linear
// light, the shader weights and adds those, and `finish` turns the sum back into a gamma-encoded straight texel.
// Opaque pixels (every camera frame) see no difference from straight alpha; translucent ones (after a key or a
// mask) keep their colour at the edges instead of fading towards black.
//
// The shaders read the input with Load only, so the sampler is not used: every filtering mode is spelled out here.

Texture2D<float4> image : register(t0);

// Size of the input in texels, set by `init_dims`.
static int2 g_dim;

void init_dims()
{
    uint w, h;
    image.GetDimensions(w, h);
    g_dim = int2(w, h);
}

float3 srgb_to_linear(float3 c)
{
    return (c <= 0.04045) ? c / 12.92 : pow((c + 0.055) / 1.055, 2.4);
}

float3 linear_to_srgb(float3 c)
{
    c = saturate(c);
    return (c <= 0.0031308) ? 12.92 * c : 1.055 * pow(c, 1.0 / 2.4) - 0.055;
}

// Premultiplied linear texel `p`; coordinates outside the picture read the nearest edge texel (OBS: clamp).
float4 fetch(int2 p)
{
    p = clamp(p, int2(0, 0), g_dim - 1);
    float4 t = image.Load(int3(p, 0));
    return float4(srgb_to_linear(t.rgb) * t.a, t.a);
}

// The same, but a coordinate outside the picture is transparent (OBS: border colour 00000000).
float4 fetch_border(int2 p)
{
    if (any(p < 0) || any(p >= g_dim))
        return float4(0.0, 0.0, 0.0, 0.0);
    return fetch(p);
}

// Bilinear sample at `pos`, in texels (texel centres at +0.5), clamped to the edge: what a Linear sampler with Clamp
// addressing returns.
float4 sample_bilinear(float2 pos)
{
    float2 q = pos - 0.5;
    float2 fl = floor(q);
    float2 f = q - fl;
    int2 i = int2(fl);
    float4 top = lerp(fetch(i), fetch(i + int2(1, 0)), f.x);
    float4 bottom = lerp(fetch(i + int2(0, 1)), fetch(i + int2(1, 1)), f.x);
    return lerp(top, bottom, f.y);
}

// The same with a transparent border: a Linear sampler with Border addressing and BorderColor 00000000.
float4 sample_bilinear_border(float2 pos)
{
    float2 q = pos - 0.5;
    float2 fl = floor(q);
    float2 f = q - fl;
    int2 i = int2(fl);
    float4 top = lerp(fetch_border(i), fetch_border(i + int2(1, 0)), f.x);
    float4 bottom = lerp(fetch_border(i + int2(0, 1)), fetch_border(i + int2(1, 1)), f.x);
    return lerp(top, bottom, f.y);
}

// A premultiplied linear sum back to the working format: straight alpha, gamma-encoded.
float4 finish(float4 p)
{
    float a = saturate(p.a);
    float3 rgb = (a > 0.0) ? p.rgb / a : float3(0.0, 0.0, 0.0);
    return float4(linear_to_srgb(rgb), a);
}
