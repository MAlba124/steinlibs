#version 460
#extension GL_EXT_buffer_reference : require
#extension GL_EXT_nonuniform_qualifier : require

// The 2D quad path, see stein_gpu::quad.

struct Quad {
    vec4 rect;
    vec4 clip;
    vec4 uv;
    uint color;
    uint slot;
    uint kind;
    uint pad;
};

layout(buffer_reference, std430, buffer_reference_align = 16) readonly buffer Quads {
    Quad q[];
};

layout(buffer_reference, std430, buffer_reference_align = 4) readonly buffer Words {
    uint w[];
};

// The same block as the vertex stage.
layout(push_constant, std430) uniform Root {
    Quads quads;
    Words atlas;
    vec2 size;
    uvec2 atlas_size;
} root;

// The one global set. Samplers first, the unsized texture table must be
// the last binding.
layout(set = 0, binding = 0) uniform sampler samplers[4];
layout(set = 0, binding = 1) uniform texture2D textures[];

const uint NEAREST_CLAMP = 0u; // stein_gpu::Sampler::NearestClamp
const uint SOLID = 0u;
const uint COVERAGE = 1u;

layout(location = 0) in vec2 v_uv;
layout(location = 1) in vec4 v_color;
layout(location = 2) flat in uint v_slot;
layout(location = 3) flat in uint v_kind;
layout(location = 4) flat in ivec2 v_origin;

layout(location = 0) out vec4 o_color;

void main() {
    if (v_kind == SOLID) {
        o_color = v_color;
        return;
    }
    if (v_kind == COVERAGE) {
        // The atlas is plain bytes behind a pointer, read a word at a time.
        // The texel is the pixel minus the glyph's origin, no filtering.
        // Helper lanes of a 2x2 quad run this too, for pixels outside the
        // glyph, and still load, so the texel is clamped into the atlas.
        ivec2 last = max(ivec2(root.atlas_size) - 1, ivec2(0));
        ivec2 t = clamp(ivec2(gl_FragCoord.xy) - v_origin, ivec2(0), last);
        uint i = uint(t.y) * root.atlas_size.x + uint(t.x);
        uint word = root.atlas.w[i >> 2];
        float cov = float((word >> ((i & 3u) * 8u)) & 0xffu) / 255.0;
        // The colour's own alpha is ignored for glyphs, as in paint.rs.
        o_color = vec4(v_color.rgb, cov);
        return;
    }
    // Slots differ per quad, so the index is not uniform across a wave.
    vec4 t = texture(sampler2D(textures[nonuniformEXT(v_slot)], samplers[NEAREST_CLAMP]), v_uv);
    o_color = t * v_color;
}
