#version 450

// Video fragment shader: samples a native NV12 frame (separate Y and interleaved
// UV planes uploaded as GPU textures) and converts YUV -> RGB on the GPU,
// replacing the old CPU sws_scale-to-RGB24 path.
//
// This is the 8-bit SDR path (BT.709 limited range). The HDR phases extend it
// with 10-bit input and PQ/HLG transfer handling.
//
// SDL_GPU's SPIR-V model binds fragment samplers to set 2.

layout(location = 0) in vec2 v_uv;
layout(location = 1) in vec4 v_color;

layout(location = 0) out vec4 o_color;

layout(set = 2, binding = 0) uniform sampler2D u_y;   // luma, R8
layout(set = 2, binding = 1) uniform sampler2D u_uv;  // chroma, R8G8 (U,V)

void main() {
    float Y = texture(u_y, v_uv).r;
    vec2 C = texture(u_uv, v_uv).rg;

    // BT.709 limited-range YUV -> RGB.
    float y = 1.1643835 * (Y - 0.0627451);
    float u = C.x - 0.5019608;
    float v = C.y - 0.5019608;
    vec3 rgb = vec3(
        y + 1.7927411 * v,
        y - 0.2132486 * u - 0.5329093 * v,
        y + 2.1124018 * u
    );

    o_color = vec4(clamp(rgb, 0.0, 1.0), 1.0) * v_color;
}
