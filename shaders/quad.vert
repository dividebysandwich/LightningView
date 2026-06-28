#version 450

// Immediate-mode 2D vertex shader. Positions arrive already in normalised
// device coordinates (computed on the CPU from pixel-space rects), so this is a
// pass-through that just forwards UV and per-vertex colour to the fragment stage.

layout(location = 0) in vec2 a_pos;
layout(location = 1) in vec2 a_uv;
layout(location = 2) in vec4 a_color;

layout(location = 0) out vec2 v_uv;
layout(location = 1) out vec4 v_color;

void main() {
    v_uv = a_uv;
    v_color = a_color;
    gl_Position = vec4(a_pos, 0.0, 1.0);
}
