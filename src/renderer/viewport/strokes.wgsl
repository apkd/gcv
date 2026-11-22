struct Camera {
    right: vec4f, up: vec4f, back: vec4f, center: vec4f,
    viewport: vec4f, style: vec4f, bed: vec4f, origin: vec4f,
}
@group(0) @binding(0) var<uniform> cam: Camera;
@group(0) @binding(1) var<uniform> palette: array<vec4u, 384>;

fn eye(point: vec3f) -> vec3f {
    let d = point - cam.center.xyz;
    return vec3f(dot(d, cam.right.xyz), dot(d, cam.up.xyz), cam.center.w - dot(d, cam.back.xyz));
}
fn project(p: vec3f) -> vec3f {
    let perspective = cam.style.z != 0.;
    let scale = cam.right.w / select(1., p.z, perspective);
    let depth = select(1. - p.z * cam.back.w, cam.up.w / p.z, perspective);
    return vec3f(cam.viewport.zw + scale * vec2f(p.x, -p.y), depth);
}
fn clip(pixel: vec2f, depth: f32) -> vec4f {
    return vec4f((pixel / cam.viewport.xy * 2. - 1.) * vec2f(1., -1.), depth, 1.);
}
struct Stroke {
    @builtin(position) position: vec4f,
    @location(0) color: vec4f,
    @location(1) local: vec2f,
    @location(2) size: vec2f,
    @location(3) @interpolate(flat) depths: vec2f,
}
@vertex fn stroke(@builtin(vertex_index) v: u32, @location(0) a: vec3f, @location(1) b: vec3f,
                  @location(2) lo: vec4u, @location(3) hi: vec4u) -> Stroke {
    var start = eye(a);
    var end = eye(b);
    let near = cam.up.w;
    if max(start.z, end.z) < near {
        return Stroke(vec4f(2., 2., 0., 1.), vec4f(0.), vec2f(0.), vec2f(0.), vec2f(0.));
    }
    if min(start.z, end.z) < near {
        var intersection = mix(start, end, (near - start.z) / (end.z - start.z));
        intersection.z = near;
        if start.z < near { start = intersection; } else { end = intersection; }
    }
    var p = project(start);
    var q = project(end);
    var width = cam.style.x;
    var color: vec4f;
    // Stroke kinds: opaque extrusion, support, travel, bed, axis label.
    if hi.w >= 3u {
        color = vec4f(vec3f(lo.xyz), f32(lo.w)) / 255.;
        width = f32(hi.z) / 10.;
        if hi.w == 4u {
            p = vec3f(floor(p.xy) + vec2f(hi.xy) + 0.5, p.z);
            q = p;
        }
    } else {
        let mode = u32(cam.style.y);
        var index = lo[mode % 4u];
        if mode >= 4u { index = hi[mode - 4u]; }
        color = unpack4x8unorm(palette[mode * 64u + index / 4u][index % 4u]);
        color = vec4f(color.rgb * (f32(hi.z) / 255.), 1.);
        if hi.w == 1u { color.a = cam.style.w; }
        if hi.w == 2u { width = max(width * 0.45, 0.5); color.a = 0.15; }
    }
    let delta = q.xy - p.xy;
    let len = length(delta);
    let dir = select(vec2f(1.,0.), delta / max(len, 1e-9), len > 1e-9);
    let radius = width * 0.5;
    let margin = radius + 0.5;
    let at_end = v >= 2u;
    let dx = select(-margin, margin, at_end);
    let dy = select(-margin, margin, (v & 1u) != 0u);
    let base = select(p, q, at_end);
    let pos = base.xy + dir * dx + vec2f(-dir.y, dir.x) * dy;
    return Stroke(clip(pos, base.z), color, vec2f(select(0., len, at_end) + dx, dy), vec2f(len, radius),vec2f(p.z,q.z));
}
struct Ink { @location(0) color: vec4f, @builtin(frag_depth) depth: f32 }
@fragment fn ink(s: Stroke) -> Ink {
    let outside = max(max(-s.local.x, s.local.x - s.size.x), 0.);
    let coverage = clamp(s.size.y + 0.5 - length(vec2f(outside, s.local.y)), 0., 1.);
    if coverage <= 0. { discard; }
    // The caps keep the depth of their endpoint. Only the centerline slopes.
    let t=select(0., clamp(s.local.x / max(s.size.x, 1e-9), 0., 1.), s.size.x > 1e-9);
    return Ink(vec4f(s.color.rgb, s.color.a * coverage),mix(s.depths.x,s.depths.y,t));
}

@vertex fn plate(@builtin(vertex_index) v: u32) -> @builtin(position) vec4f {
    let corner = vec2f(f32(v & 1u), f32(v >> 1u)) * cam.bed.xy;
    let p = project(eye(vec3f(corner, 0.) - cam.origin.xyz));
    return clip(p.xy, p.z);
}
@fragment fn plate_color() -> @location(0) vec4f {
    return vec4f(86., 86., 89., 255.) / 255.;
}
