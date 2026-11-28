// The device body under a studio: metallic/roughness shading against an analytic environment
// (soft boxes and a sky-to-floor gradient fixed to the camera), Fresnel on everything, and a soft
// contact shadow on the floor under the body. The live screen shows the flat view's picture
// (sampled from Rune's backdrop copy) exactly, with only the cover glass's reflection added over
// it: the scene's light never multiplies a device pixel, and face on the reflection is off.

struct Frame {
    view_proj: mat4x4f,
    // xyz: the eye (world); w: how much of the glass's reflection the live screen shows (0 face on).
    eye: vec4f,
    // The camera's right, up and toward-the-viewer axes in the world: the studio turns with it.
    cam_x: vec4f,
    cam_y: vec4f,
    cam_z: vec4f,
    // The screen's rect in the backdrop, in its UVs: x, y, w, h.
    uv_rect: vec4f,
    // x: the backdrop holds sRGB-encoded values; y: quarter turns the body is rolled by.
    flags: vec4f,
    // rgb: the stage's colour (linear); a: how dark the contact shadow gets at its middle.
    stage: vec4f,
    // The body's footprint on the floor: centre x, centre z, half width, half depth (world, mm).
    foot: vec4f,
}

struct Draw {
    model: mat4x4f,
    // rgb: linear base colour; a: 0 a lit surface, 1 the live screen, 2 the floor's shadow,
    // 3 a camera lens (a lit surface whose UVs run 0..1 across the lens).
    base: vec4f,
    // x: metallic, y: roughness, z: a dielectric's reflectance at normal incidence,
    // w: the shadow's blur (mm).
    params: vec4f,
}

@group(0) @binding(0) var<uniform> fr: Frame;
@group(0) @binding(1) var screen: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;
@group(1) @binding(0) var<uniform> dr: Draw;

struct V {
    @builtin(position) pos: vec4f,
    @location(0) world: vec3f,
    @location(1) n: vec3f,
    @location(2) uv: vec2f,
}

@vertex fn vs(@location(0) p: vec3f, @location(1) n: vec3f, @location(2) uv: vec2f) -> V {
    let w = dr.model * vec4f(p, 1.0);
    var o: V;
    o.pos = fr.view_proj * w;
    o.world = w.xyz;
    o.n = (dr.model * vec4f(n, 0.0)).xyz;
    o.uv = uv;
    return o;
}

fn decode(c: vec3f) -> vec3f {
    return select(pow((c + 0.055) / 1.055, vec3f(2.4)), c / 12.92, c <= vec3f(0.04045));
}

// Where a screen-face UV falls in the upright frame with the body rolled q quarters clockwise
// (scene::frame_uv).
fn turned(uv: vec2f, q: f32) -> vec2f {
    if (q < 0.5) { return uv; }
    if (q < 1.5) { return vec2f(1.0 - uv.y, uv.x); }
    if (q < 2.5) { return vec2f(1.0 - uv.x, 1.0 - uv.y); }
    return vec2f(uv.y, 1.0 - uv.x);
}

// A world direction in the camera's frame: x right, y up, z toward the viewer.
fn to_camera(d: vec3f) -> vec3f {
    return vec3f(dot(d, fr.cam_x.xyz), dot(d, fr.cam_y.xyz), dot(d, fr.cam_z.xyz));
}

// A rectangular soft box seen in direction d (camera frame): k points at its middle, `half` is its
// half size as tangents of the angles it spans, `soft` how far its edges fade.
fn soft_box(d: vec3f, k: vec3f, half: vec2f, soft: f32) -> f32 {
    let c = dot(d, k);
    if (c <= 0.02) { return 0.0; }
    let right = normalize(cross(vec3f(0.0, 1.0, 0.0), k) + vec3f(1e-4, 0.0, 0.0));
    let up = cross(k, right);
    let p = abs(vec2f(dot(d, right), dot(d, up)) / c);
    let e = smoothstep(half + vec2f(soft), half - vec2f(soft), p);
    return e.x * e.y;
}

const KEY: vec3f = vec3f(-0.50, 0.62, 0.60);
const RIM: vec3f = vec3f(0.80, 0.22, -0.56);
const BOUNCE: vec3f = vec3f(-0.72, -0.40, 0.56);
const FILL: vec3f = vec3f(0.62, 0.05, 0.78);
// Across the bounce card: the side of this plane the card is lit on.
const SLANT: vec3f = vec3f(0.0, 0.66, -0.752);
const WINDOW: vec3f = vec3f(-0.10, 0.20, 0.97);

// The studio's radiance in camera-frame direction d, blurred for a surface of `rough`ness.
fn studio(d: vec3f, rough: f32) -> f32 {
    let soft = 0.012 + rough;
    // A dim room: darker toward the floor, a little lighter overhead.
    var l = mix(0.035, 0.30, smoothstep(-0.7, 0.9, d.y));
    // The key: a large box up and to the left of the camera.
    l += 3.2 * soft_box(d, normalize(KEY), vec2f(0.55, 0.42), soft);
    // A strip behind and to the right: the rim along the body's far edges.
    l += 2.6 * soft_box(d, normalize(RIM), vec2f(0.22, 0.95), soft);
    // A panel overhead, reaching back: the top edge.
    l += 1.5 * soft_box(d, vec3f(0.0, 1.0, 0.0), vec2f(1.6, 1.9), soft + 0.15);
    // A card low on each side, its upper edge running on a slant: what a face turned toward a
    // 3/4 camera mirrors, so a sheet of glass there shows a sheen with an edge across it.
    let low = vec3f(-abs(d.x), d.y, d.z);
    let card = soft_box(low, normalize(BOUNCE), vec2f(0.9, 0.8), soft + 0.25);
    l += 1.1 * card * smoothstep(-soft, soft, dot(low, normalize(SLANT)) + 0.58);
    // A window behind the camera's left shoulder: the glint in the corner of a face seen square on.
    l += 1.2 * soft_box(d, normalize(WINDOW), vec2f(0.10, 0.13), soft);
    // A weak fill from the right front.
    l += 0.35 * soft_box(d, normalize(FILL), vec2f(0.5, 0.7), soft + 0.3);
    return l;
}

// What a matte surface facing n (camera frame) gathers from the studio.
fn gathered(n: vec3f) -> f32 {
    let sky = mix(0.06, 0.34, n.y * 0.5 + 0.5);
    let key = 0.95 * max(dot(n, normalize(KEY)), 0.0);
    let rim = 0.30 * max(dot(n, normalize(RIM)), 0.0);
    let fill = 0.22 * max(dot(n, normalize(FILL)), 0.0) + 0.16 * max(dot(n, normalize(BOUNCE)), 0.0);
    return sky + key + rim + fill;
}

// How much of what its cover glass mirrors a lit screen shows: a screen's own light drowns most
// of a reflection.
const SCREEN_SHEEN: f32 = 0.45;

// How far p is outside a box of half size b round the origin (negative inside).
fn outside(p: vec2f, b: vec2f) -> f32 {
    let q = abs(p) - b;
    return length(max(q, vec2f(0.0))) + min(max(q.x, q.y), 0.0);
}

@fragment fn fs(v: V, @builtin(front_facing) facing: bool) -> @location(0) vec4f {
    let t = clamp(turned(v.uv, fr.flags.y), vec2f(0.0), vec2f(1.0));
    var frame = textureSample(screen, samp, fr.uv_rect.xy + t * fr.uv_rect.zw).rgb;
    if (fr.flags.x > 0.5) { frame = decode(frame); }

    if (dr.base.a > 1.5) {
        // The floor: the stage's colour, darkened under the body. Seen from below there is none.
        let blur = max(dr.params.w, 1.0);
        let d = outside(v.world.xz - fr.foot.xy, fr.foot.zw) / blur;
        let fall = clamp(1.0 - d, 0.0, 1.0);
        let a = fr.stage.a * fall * fall * (3.0 - 2.0 * fall);
        if (!facing || a < 0.004) { discard; }
        return vec4f(fr.stage.rgb * (1.0 - a), 1.0);
    }

    var n = normalize(v.n);
    if (!facing) { n = -n; }
    var base = dr.base.rgb;
    var reflectance = dr.params.z;
    if (dr.base.a > 2.5) {
        // A lens: its front element bulges, so what it mirrors bends across it; a dark barrel
        // round a coated element, with a faint ring where the two meet.
        let c = (v.uv - vec2f(0.5)) * 2.0;
        let r = length(c);
        let bulge = normalize(dr.model[0].xyz) * c.x - normalize(dr.model[1].xyz) * c.y;
        n = normalize(n + 0.55 * bulge);
        let element = 1.0 - smoothstep(0.50, 0.58, r);
        base = mix(vec3f(0.0015, 0.0015, 0.002), vec3f(0.004, 0.007, 0.022), element);
        base += vec3f(0.02) * (1.0 - smoothstep(0.0, 0.035, abs(r - 0.66)));
        reflectance = mix(0.04, dr.params.z, element);
    }
    let e = normalize(fr.eye.xyz - v.world);
    let nv = max(dot(n, e), 0.0);
    let rough = clamp(dr.params.y, 0.02, 1.0);
    let mirrored = to_camera(reflect(-e, n));
    let grazing = pow(1.0 - nv, 5.0);

    if (dr.base.a > 0.5) {
        // The live screen: its own pixels, and over them what the cover glass mirrors.
        let glass = (reflectance + (1.0 - reflectance) * grazing) * studio(mirrored, rough);
        return vec4f(frame + vec3f(min(glass, 0.2) * SCREEN_SHEEN * fr.eye.w), 1.0);
    }

    let metallic = dr.params.x;
    let f0 = mix(vec3f(reflectance), base, metallic);
    // A rough surface's grazing reflection is weaker than a mirror's.
    let fresnel = f0 + (max(vec3f(1.0 - rough), f0) - f0) * grazing;
    let diffuse = base * (1.0 - metallic) * gathered(to_camera(n));
    let lit = diffuse + fresnel * studio(mirrored, rough);
    // Highlights roll off instead of clipping.
    return vec4f(lit / (1.0 + 0.18 * lit), 1.0);
}
