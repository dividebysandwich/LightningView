#version 450

// Video fragment shader. Samples a native YUV 4:2:0 frame (separate luma +
// interleaved chroma planes uploaded as GPU textures) and converts to display
// RGB on the GPU. Handles both 8-bit SDR (NV12, BT.709) and 10-bit HDR (P010,
// BT.2020 with PQ or HLG transfer) — the planes sample as normalised floats
// either way, so one shader serves both, branching on the `mode` uniform.
//
// HDR path: YUV->R'G'B' -> linearise (PQ/HLG EOTF) -> luminance tone-map
// (extended Reinhard) -> BT.2020->BT.709 gamut -> sRGB OETF, producing a
// correct SDR image for the SDR swapchain. True HDR passthrough is a later phase.
//
// SDL_GPU SPIR-V model: fragment samplers at set 2, fragment uniforms at set 3.

layout(location = 0) in vec2 v_uv;
layout(location = 1) in vec4 v_color;
layout(location = 0) out vec4 o_color;

layout(set = 2, binding = 0) uniform sampler2D u_y;   // luma  (R8 or R16)
layout(set = 2, binding = 1) uniform sampler2D u_uv;  // chroma (R8G8 or R16G16)

layout(set = 3, binding = 0) uniform Params {
    // x = transfer (0 SDR, 1 PQ, 2 HLG), y = BT.2020 primaries (0/1),
    // z = full range (0/1), w = output mode (0 SDR tone-map, 1 HDR10 PQ, 2 scRGB).
    ivec4 mode;
    // x = source peak luminance (nits), y = SDR tone-map white (nits),
    // z = display SDR-white level (nits, for HDR output), w = unused.
    vec4 lum;
} u;

// BT.2020 -> BT.709 in linear light (column-major for `M * v`).
const mat3 BT2020_TO_709 = mat3(
     1.660491, -0.124550, -0.018151,
    -0.587641,  1.132900, -0.100579,
    -0.072850, -0.008349,  1.118730
);

const vec3 LUMA_2020 = vec3(0.2627, 0.6780, 0.0593);

float pq_eotf(float e) {
    const float m1 = 0.1593017578125;
    const float m2 = 78.84375;
    const float c1 = 0.8359375;
    const float c2 = 18.8515625;
    const float c3 = 18.6875;
    float p = pow(max(e, 0.0), 1.0 / m2);
    float num = max(p - c1, 0.0);
    float den = c2 - c3 * p;
    return pow(num / den, 1.0 / m1); // normalised: 1.0 == 10000 nits
}

float hlg_inv_oetf(float x) {
    const float a = 0.17883277;
    const float b = 0.28466892;
    const float c = 0.55991073;
    return (x <= 0.5) ? (x * x / 3.0) : ((exp((x - c) / a) + b) / 12.0);
}

vec3 srgb_oetf(vec3 c) {
    vec3 lo = 12.92 * c;
    vec3 hi = 1.055 * pow(max(c, 0.0), vec3(1.0 / 2.4)) - 0.055;
    return mix(lo, hi, step(vec3(0.0031308), c));
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
    float Y = texture(u_y, v_uv).r;
    vec2 C = texture(u_uv, v_uv).rg;

    // YUV (limited or full range) -> non-linear R'G'B'.
    float y, cu, cv;
    if (u.mode.z == 1) {
        y = Y;
        cu = C.x - 0.5;
        cv = C.y - 0.5;
    } else {
        y = (Y - 0.0627451) * 1.1643835;
        cu = (C.x - 0.5019608) * 1.1383929;
        cv = (C.y - 0.5019608) * 1.1383929;
    }

    vec3 rgb;
    if (u.mode.y == 1) { // BT.2020 non-constant luminance
        rgb = vec3(y + 1.47460 * cv, y - 0.16455 * cu - 0.57135 * cv, y + 1.88140 * cu);
    } else {             // BT.709
        rgb = vec3(y + 1.57480 * cv, y - 0.18733 * cu - 0.46813 * cv, y + 1.85560 * cu);
    }

    int transfer = u.mode.x;
    if (transfer == 0) {
        // SDR: already display-gamma encoded — pass straight through.
        o_color = vec4(clamp(rgb, 0.0, 1.0), 1.0) * v_color;
        return;
    }

    // --- HDR: linearise to nits ---
    vec3 lin;
    if (transfer == 1) { // PQ (SMPTE 2084)
        lin = vec3(pq_eotf(rgb.r), pq_eotf(rgb.g), pq_eotf(rgb.b)) * 10000.0;
    } else {             // HLG (ARIB STD-B67)
        vec3 scene = vec3(
            hlg_inv_oetf(clamp(rgb.r, 0.0, 1.0)),
            hlg_inv_oetf(clamp(rgb.g, 0.0, 1.0)),
            hlg_inv_oetf(clamp(rgb.b, 0.0, 1.0))
        );
        float ys = dot(scene, LUMA_2020);
        float peak = max(u.lum.x, 1.0);
        // OOTF: display-linear = scene * Ys^(gamma-1), scaled to peak nits.
        lin = scene * pow(max(ys, 1e-6), 1.2 - 1.0) * peak;
    }

    // `lin` is now linear BT.2020 light in nits. On an HDR swapchain we pass it
    // through (no tone-map, no gamut reduction); on SDR we tone-map below.
    int omode = u.mode.w;
    if (omode == 1) {
        // HDR10 PQ: encode absolute BT.2020 nits directly (PQ passthrough).
        o_color = vec4(pq_oetf(lin / 10000.0), 1.0) * v_color;
        return;
    }
    if (omode == 2) {
        // scRGB extended-linear (BT.709), 1.0 == display SDR white.
        vec3 l709 = BT2020_TO_709 * lin;
        vec3 s = max(l709 / max(u.lum.z, 1.0), 0.0);
        o_color = vec4(s, 1.0) * v_color;
        return;
    }

    // --- SDR output: tone-map BT.2020 nits down into the SDR range ---
    // Normalise so SDR diffuse white maps to 1.0.
    float white = max(u.lum.y, 1.0);
    vec3 c = lin / white;

    // Luminance-based tone mapping (extended Reinhard) so highlights up to the
    // source peak compress into the SDR range without per-channel hue shifts.
    float lw = max(u.lum.x / white, 1.0);
    float l = dot(c, LUMA_2020);
    float lt = l * (1.0 + l / (lw * lw)) / (1.0 + l);
    float scale = (l > 1e-6) ? (lt / l) : 0.0;
    c *= scale;

    // Gamut-map BT.2020 -> BT.709, then encode to sRGB for the SDR swapchain.
    c = clamp(BT2020_TO_709 * c, 0.0, 1.0);
    o_color = vec4(srgb_oetf(c), 1.0) * v_color;
}
