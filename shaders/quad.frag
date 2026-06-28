#version 450

// Single fragment shader serving every 2D draw in the app:
//  - textured RGBA images / video frames / tiles  (sample * white vertex colour)
//  - solid rectangles                              (1x1 white texture * colour)
//  - text glyphs                                   (RGBA coverage atlas * colour)
//
// SDL_GPU's SPIR-V resource model binds fragment-stage samplers to set 2.

layout(location = 0) in vec2 v_uv;
layout(location = 1) in vec4 v_color;

layout(location = 0) out vec4 o_color;

layout(set = 2, binding = 0) uniform sampler2D u_tex;

void main() {
    o_color = texture(u_tex, v_uv) * v_color;
}
