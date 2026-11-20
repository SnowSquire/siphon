#version 450
// The interface over the diagnostics fill.
//
// The fill is drawn by the render pass's clear, so anything this shader does
// not cover stays magenta. The interface texture is premultiplied and holds
// sRGB-encoded values; where it is not fully opaque the fill shows through, and
// where it was never written at all the fill shows through completely. That is
// the point: magenta on screen means this sample was short.
layout(location = 0) out vec4 out_color;

layout(set = 0, binding = 0) uniform sampler2D ui_tex;

layout(push_constant) uniform Params
{
    vec2 ui_size;  // interface texture size in pixels
    uint mode;     // 0 = SDR passthrough, 1 = PQ/Rec.2020 for the display white
    float white;   // SDR white in nits, the reference the interface is graded to
} pc;

vec3 srgb_to_linear(vec3 c)
{
    return mix(c / 12.92, pow((c + 0.055) / 1.055, vec3(2.4)), step(vec3(0.04045), c));
}

// ST 2084. `nits` is absolute luminance, so the SDR white point lands where
// Windows put it rather than at PQ's own 100-nit reference.
vec3 pq_encode(vec3 nits)
{
    const float m1 = 0.1593017578125;
    const float m2 = 78.84375;
    const float c1 = 0.8359375;
    const float c2 = 18.8515625;
    const float c3 = 18.6875;
    vec3 y = max(nits, vec3(0.0)) / 10000.0;
    vec3 ym = pow(y, vec3(m1));
    return pow((vec3(c1) + vec3(c2) * ym) / (vec3(1.0) + vec3(c3) * ym), vec3(m2));
}

void main()
{
    // Vulkan's framebuffer origin is the top-left, which is also the interface
    // texture's, so this needs no flip. Outside the interface the sampler
    // clamps, repeating its edge texel over the rest of the canvas.
    vec2 uv = gl_FragCoord.xy / pc.ui_size;
    vec4 ui = texture(ui_tex, uv);

    // Premultiplied over the magenta fill: alpha 0 leaves the fill untouched.
    vec3 rgb = ui.rgb + vec3(1.0, 0.0, 1.0) * (1.0 - ui.a);

    if (pc.mode == 1u)
    {
        // SDR sRGB -> linear, placed at the display's SDR white, BT.709 ->
        // BT.2020, then PQ. No tone curve: the interface never exceeds its own
        // white, so there is nothing to roll off.
        vec3 lin = srgb_to_linear(rgb) * pc.white;
        vec3 bt2020 = vec3(dot(lin, vec3(0.6274, 0.3293, 0.0433)),
                           dot(lin, vec3(0.0691, 0.9195, 0.0114)),
                           dot(lin, vec3(0.0164, 0.0880, 0.8956)));
        rgb = pq_encode(bt2020);
    }

    out_color = vec4(rgb, 1.0);
}
