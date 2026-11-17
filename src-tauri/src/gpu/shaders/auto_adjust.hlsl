// Auto Adjust (Better Cam's own filter, src/fx/auto_adjust.rs): per-channel white balance gains and an exposure
// gain with a highlight shoulder, applied in linear light. The gains come from the CPU (the control loop in
// auto_adjust.rs); this shader only applies them.
//
// The pipeline texture is gamma-encoded sRGB with straight alpha, so the shader converts to linear light, applies
// the gains, and converts back. Alpha passes through unchanged.
//
// Exposure gain `gain` multiplies the white-balanced colour. A gain above 1 would clip bright parts to flat white, so
// above `knee` (a linear level, 1 when the gain is 1 or less) the curve bends toward 1 with the slope 1 at the
// knee: y = knee + (1 - knee) * (1 - exp(-(v - knee) / (1 - knee))). `knee` is 1 at gain 1, so a gain of 1 is the
// exact identity, and it falls to 0.7 as the gain grows (the CPU computes it with the same formula as its brightness
// prediction).

Texture2D<float4> image : register(t0);
SamplerState linear_clamp : register(s0);

cbuffer Params : register(b0)
{
    float3 wb_gain; // linear-light gains per channel
    float gain;     // exposure gain
    float knee;     // start of the highlight shoulder, linear
    float3 pad;
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
    return float4(linear_to_srgb(y), p.a);
}
