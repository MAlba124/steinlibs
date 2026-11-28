#version 460
#extension GL_EXT_buffer_reference : require

// The 2D quad path, see stein_gpu::quad. Solid and coverage quads follow
// stein-shell's paint.rs to the pixel, image quads are plain textured rects.

// Mirrors stein_gpu::quad::GpuQuad, 64 bytes, offsets pinned by a Rust test.
struct Quad {
    vec4 rect;  // x0 y0 x1 y1, pixels
    vec4 clip;  // x0 y0 x1 y1, pixels
    vec4 uv;    // u v w h, normalised
    uint color; // ARGB, top byte alpha
    uint slot;  // texture table index, image quads
    uint kind;  // 0 solid, 1 coverage, 2 image
    uint pad;
};

layout(buffer_reference, std430, buffer_reference_align = 16) readonly buffer Quads {
    Quad q[];
};

layout(buffer_reference, std430, buffer_reference_align = 4) readonly buffer Words {
    uint w[];
};

// Mirrors stein_gpu::quad::QuadRoot, 32 bytes.
layout(push_constant, std430) uniform Root {
    Quads quads;
    Words atlas;
    vec2 size;
    uvec2 atlas_size;
} root;

const uint SOLID = 0u;
const uint COVERAGE = 1u;

layout(location = 0) out vec2 v_uv;
layout(location = 1) out vec4 v_color;
layout(location = 2) flat out uint v_slot;
layout(location = 3) flat out uint v_kind;
layout(location = 4) flat out ivec2 v_origin;

// Two triangles over the pixel rect.
const vec2 CORNERS[6] = vec2[6](
    vec2(0.0, 0.0), vec2(1.0, 0.0), vec2(0.0, 1.0),
    vec2(0.0, 1.0), vec2(1.0, 0.0), vec2(1.0, 1.0)
);

// Rust's float to int casts saturate, GLSL's are undefined out of range.
// Clamping first changes no result the painter can produce.
ivec2 ifloor(vec2 v) { return ivec2(floor(clamp(v, -1e7, 1e7))); }
ivec2 iceil(vec2 v) { return ivec2(ceil(clamp(v, -1e7, 1e7))); }
// f32::round, half away from zero.
ivec2 iround(vec2 v) {
    vec2 c = clamp(v, -1e7, 1e7);
    return ivec2(sign(c) * floor(abs(c) + 0.5));
}

vec4 unpack_argb(uint c) {
    float a = float((c >> 24) & 0xffu);
    // 0 and 255 both mean opaque, as in paint.rs.
    a = (a == 0.0) ? 255.0 : a;
    return vec4(float((c >> 16) & 0xffu), float((c >> 8) & 0xffu), float(c & 0xffu), a) / 255.0;
}

void main() {
    Quad q = root.quads.q[gl_InstanceIndex];
    vec2 corner = CORNERS[gl_VertexIndex];
    ivec2 screen = ivec2(root.size);
    v_uv = vec2(0.0);
    v_origin = ivec2(0);
    vec2 p;
    if (q.kind == COVERAGE) {
        // paint.rs blend_glyph. One texel per pixel from rounded origins,
        // the near edges ceiled and far ones floored against the clip, and
        // clipped to the atlas so no texel outside it is read.
        ivec2 asz = ivec2(root.atlas_size);
        ivec2 d = iround(q.rect.xy);
        ivec2 s = iround(q.uv.xy * vec2(asz));
        ivec2 g = iround(q.uv.zw * vec2(asz));
        ivec2 lo = max(max(d, iceil(q.clip.xy)), max(ivec2(0), d - s));
        ivec2 hi = min(min(d + g, ifloor(q.clip.zw)), min(screen, d - s + asz));
        p = mix(vec2(lo), vec2(max(hi, lo)), corner);
        v_origin = d - s;
    } else if (q.kind == SOLID) {
        // paint.rs fill_quad. The clipped rect truncated to whole pixels.
        ivec2 lo = ifloor(max(max(q.rect.xy, q.clip.xy), vec2(0.0)));
        ivec2 hi = min(ifloor(max(min(q.rect.zw, q.clip.zw), vec2(0.0))), screen);
        p = mix(vec2(lo), vec2(max(hi, lo)), corner);
    } else {
        // Images, a textured rect with the uv following the clipped corner.
        vec2 lo = max(q.rect.xy, q.clip.xy);
        vec2 hi = max(min(q.rect.zw, q.clip.zw), lo);
        p = mix(lo, hi, corner);
        vec2 extent = max(q.rect.zw - q.rect.xy, vec2(1e-6));
        v_uv = q.uv.xy + (p - q.rect.xy) / extent * q.uv.zw;
    }
    v_color = unpack_argb(q.color);
    v_slot = q.slot;
    v_kind = q.kind;

    // Pixels to NDC. Vulkan's y points down, same as the layout's. An empty
    // rect collapses to zero area and rasterizes nothing.
    gl_Position = vec4(p / root.size * 2.0 - 1.0, 0.0, 1.0);
}
