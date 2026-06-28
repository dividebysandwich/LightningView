#version 450

// Single fragment shader serving every 2D draw in the app:
//  - textured RGBA images / video frames / tiles  (sample * white vertex colour)
//  - solid rectangles                              (1x1 white texture * colour)
//  - text glyphs                                   (RGBA coverage atlas * colour)
//
// Inputs are sRGB-encoded. The `mode` uniform selects how to encode the result
// for the active swapchain: plain sRGB for an SDR swapchain, or — when drawn on
// top of HDR video on an HDR-capable display — re-encoded so this SDR UI sits at
// the display's SDR-white level (HDR10 PQ, or scRGB extended-linear).
//
// SDL_GPU SPIR-V model: fragment samplers at set 2, fragment uniforms at set 3.

layout(location = 0) in vec2 v_uv;
layout(location = 1) in vec4 v_color;

layout(location = 0) out vec4 o_color;

layout(set = 2, binding = 0) uniform sampler2D u_tex;

layout(set = 3, binding = 0) uniform Params {
    ivec4 mode; // x = output mode (0 SDR sRGB, 1 HDR10 PQ, 2 scRGB linear)
    vec4 lum;   // x = SDR white level in nits (for HDR10)
} u;

// BT.709 -> BT.2020 in linear light (column-major for `M * v`).
const mat3 BT709_TO_2020 = mat3(
    0.627404, 0.069097, 0.016391,
    0.329283, 0.919541, 0.088013,
    0.043313, 0.011362, 0.895595
);

vec3 srgb_eotf(vec3 c) {
    vec3 lo = c / 12.92;
    vec3 hi = pow((c + 0.055) / 1.055, vec3(2.4));
    return mix(lo, hi, step(vec3(0.04045), c));
}

vec3 pq_oetf(vec3 l) { // l normalised so 1.0 == 10000 nits
    const float m1 = 0.1593017578125;
    const float m2 = 78.84375;
    const float c1 = 0.8359375;
    const float c2 = 18.8515625;
    const float c3 = 18.6875;
    vec3 lm = pow(max(l, 0.0), vec3(m1));
    return pow((c1 + c2 * lm) / (1.0 + c3 * lm), vec3(m2));
}

void main() {
    vec4 c = texture(u_tex, v_uv) * v_color;

    int om = u.mode.x;
    if (om == 0) {
        o_color = c; // SDR swapchain: values are already sRGB-encoded.
        return;
    }

    // Decode sRGB to linear, where 1.0 == SDR diffuse white.
    vec3 lin = srgb_eotf(clamp(c.rgb, 0.0, 1.0));

    if (om == 2) {
        // scRGB extended-linear: 1.0 already == SDR white, BT.709 primaries.
        o_color = vec4(lin, c.a);
        return;
    }

    // HDR10 PQ: lift to the display's SDR-white nits, BT.709 -> BT.2020, PQ encode.
    float white = max(u.lum.x, 1.0);
    vec3 nits = BT709_TO_2020 * (lin * white);
    o_color = vec4(pq_oetf(nits / 10000.0), c.a);
}
