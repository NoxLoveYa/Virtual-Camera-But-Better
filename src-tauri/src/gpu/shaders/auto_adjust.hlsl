// Auto Adjust (Better Cam's own filter, src/fx/auto_adjust.rs): per-channel white balance gains, an exposure gain with
// a highlight shoulder, a soft contrast curve around the mean brightness and a saturation gain around luma. The
// gains come from the CPU (the control loop in auto_adjust.rs); this shader only applies them.
//
// The pipeline texture is gamma-encoded sRGB with straight alpha. Order of the stages (the CPU predicts the picture in
// the same order): exposure and white balance in linear light, then contrast and saturation on the gamma-encoded
// colour. Alpha passes through unchanged.
//
// Exposure gain `gain` multiplies the white-balanced colour. A gain above 1 would clip bright parts to flat white, so
// above `knee` (a linear level, 1 when the gain is 1 or less) the curve bends toward 1 with the slope 1 at the
// knee: y = knee + (1 - knee) * (1 - exp(-(v - knee) / (1 - knee))). `knee` is 1 at gain 1, so a gain of 1 is the
// exact identity, and it falls to 0.7 as the gain grows (the CPU computes it with the same formula as its brightness
// prediction).
//
// Contrast: each encoded channel x goes through a curve that keeps 0, 1 and the pivot `pivot` fixed and has the
// slope `contrast` at the pivot. Either side of the pivot is stretched to u in [-1, 1] and bent with
// g(u) = k u + (1 - k) u |u|, which has the slope k at 0 and 2 - k at the ends, so for k up to 2 it never reverses
// and never clips: nothing leaves [0, 1] and the ends fade in smoothly instead of being cut. `pivot` is chosen on the
// CPU so the mean brightness of the picture stays where it was. `contrast` 1 is the identity.
//
// Saturation: c' = Y + s (c - Y) with Y the Rec. 709 luma of the encoded colour; a gray pixel does not change.
// `saturation` 1 is the identity. When both are 1 the result is bit for bit what the filter gave before they existed.

Texture2D<float4> image : register(t0);
SamplerState linear_clamp : register(s0);

cbuffer Params : register(b0)
{
    float3 wb_gain;   // linear-light gains per channel
    float gain;       // exposure gain
    float knee;       // start of the highlight shoulder, linear
    float contrast;   // slope of the contrast curve at the pivot, 1 = off
    float pivot;      // the encoded brightness the contrast curve turns around, in (0, 1)
    float saturation; // saturation gain, 1 = off
};

float3 srgb_to_linear(float3 c)
{
    return (c <= 0.04045) ? c / 12.92 : pow((c + 0.055) / 1.055, 2.4);
}

float3 linear_to_srgb(float3 c)
{
    c = saturate(c);
    return (c <= 0.0031308) ? 12.92 * c : 1.055 * pow(c, 1.0 / 2.4) - 0.055;
}

float3 contrast_curve(float3 x)
{
    float3 below = (x - pivot) / pivot;
    float3 above = (x - pivot) / (1.0 - pivot);
    float3 u = (x < pivot) ? below : above;
    float3 room = (x < pivot) ? float3(pivot, pivot, pivot) : float3(1.0 - pivot, 1.0 - pivot, 1.0 - pivot);
    float3 a = abs(u);
    return pivot + u * (a + contrast * (1.0 - a)) * room;
}

float4 main(float4 pos : SV_Position) : SV_Target
{
    float4 p = image.Load(int3(int2(pos.xy), 0));
    float3 v = srgb_to_linear(p.rgb) * wb_gain * gain;
    float3 y = v;
    if (knee < 1.0)
    {
        float room = 1.0 - knee;
        float3 shoulder = knee + room * (1.0 - exp(-(v - knee) / room));
        y = (v <= knee) ? v : shoulder;
    }
    float3 c = linear_to_srgb(y);
    if (contrast != 1.0)
    {
        c = saturate(contrast_curve(c));
    }
    if (saturation != 1.0)
    {
        float luma = dot(c, float3(0.2126, 0.7152, 0.0722));
        c = saturate(luma + saturation * (c - luma));
    }
    return float4(c, p.a);
}
