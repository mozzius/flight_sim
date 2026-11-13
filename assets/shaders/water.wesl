// Lakes and the sea (src/terrain_gen/water.rs): a flat plane at the water level.
//
//  - colour from the depth below the surface (baked R8 textures of the height
//    grids): turquoise in the shallows to deep blue, a lighter band and foam at
//    the shore;
//  - animated ripple normals at five scales, each faded out once it's smaller
//    than a couple of pixels; what's lost widens the sun glint, so far water
//    turns into a soft glitter path rather than a mirror;
//  - a Fresnel reflection of the sky (the environment's horizon / zenith
//    colours) and the sun glint, both added after lighting (post-exposure),
//    shadowed by the terrain's shadow map;
//  - the valley mist and distance fog, as the terrain (and the atmosphere's
//    aerial perspective, as it's opaque);
//  - frozen (tundra): pale, cracked ice with drifted snow, no ripples or glint.
//
// Every texture is sampled with textureSampleLevel in uniform control flow
// (Chrome's WGSL rules), and no texture binding is aliased with `let` (Safari).

import bevy_pbr::render::{
    pbr_fragment::pbr_input_from_standard_material,
    mesh_view_bindings::{view, globals},
    forward_io::{VertexOutput, FragmentOutput},
    pbr_functions::{apply_pbr_lighting, main_pass_post_lighting_processing},
    shadows::fetch_directional_shadow,
};

struct Water {
    // rgb: shallow colour (linear), a: depth (m) to reach the deep colour
    shallow: vec4<f32>,
    // rgb: deep colour, a: foam width (m of depth)
    deep: vec4<f32>,
    // rgb: sky at the horizon (post-exposure), a: reflection strength
    horizon: vec4<f32>,
    // rgb: sky overhead, a: 1 = frozen
    zenith: vec4<f32>,
    // rgb: sun glint colour and strength
    glint: vec4<f32>,
    // xyz: direction to the sun, w: water level (m)
    sun: vec4<f32>,
    // rgb: mist colour, a: mist density multiplier
    mist: vec4<f32>,
    // x: mist brightness, y: ripple strength, z: ripple octaves,
    // w: radius of the atmosphere's planet (0 = no atmosphere)
    params: vec4<f32>,
    // xy: corner of the near / fine / coarse depth texture, z: 1 / extent
    near_frame: vec4<f32>,
    fine_frame: vec4<f32>,
    coarse_frame: vec4<f32>,
}

@group(constants::MATERIAL_BIND_GROUP) @binding(100)
var<uniform> water: Water;
@group(constants::MATERIAL_BIND_GROUP) @binding(101)
var near_depth: texture_2d<f32>;
@group(constants::MATERIAL_BIND_GROUP) @binding(102)
var fine_depth: texture_2d<f32>;
@group(constants::MATERIAL_BIND_GROUP) @binding(103)
var coarse_depth: texture_2d<f32>;
@group(constants::MATERIAL_BIND_GROUP) @binding(104)
var depth_sampler: sampler;

// keep in sync with src/terrain_gen/water.rs
const DEPTH_RANGE: f32 = 128.0;

fn hash2(p: vec2<f32>) -> f32 {
    var q = fract(p * vec2<f32>(0.1031, 0.1030));
    q += dot(q, q.yx + 33.33);
    return fract((q.x + q.y) * q.x);
}

// Value noise with analytic derivatives: (value in -1..1, d/dx, d/dy).
fn noised(p: vec2<f32>) -> vec3<f32> {
    let i = floor(p);
    let f = fract(p);
    let u = f * f * f * (f * (f * 6.0 - 15.0) + 10.0);
    let du = 30.0 * f * f * (f * (f - 2.0) + 1.0);
    let a = hash2(i);
    let b = hash2(i + vec2<f32>(1.0, 0.0));
    let c = hash2(i + vec2<f32>(0.0, 1.0));
    let d = hash2(i + vec2<f32>(1.0, 1.0));
    let k1 = b - a;
    let k2 = c - a;
    let k4 = a - b - c + d;
    let v = a + k1 * u.x + k2 * u.y + k4 * u.x * u.y;
    let dv = du * vec2<f32>(k1 + k4 * u.y, k2 + k4 * u.x);
    return vec3<f32>(v * 2.0 - 1.0, dv * 2.0);
}

fn smoothstep1(a: f32, b: f32, x: f32) -> f32 {
    let t = clamp((x - a) / (b - a), 0.0, 1.0);
    return t * t * (3.0 - 2.0 * t);
}

// Ripple slopes (xy) and the fraction of the slope faded out with distance (z).
// Each octave drifts its own way at about the speed of a wave that long.
fn ripples(p: vec2<f32>, t: f32, footprint: f32, octaves: f32, wind: f32) -> vec3<f32> {
    var slope = vec2<f32>(0.0);
    var total = 0.0;
    var kept = 0.0;
    var wl = 260.0;
    var k = 0.03;
    var angle = 0.4;
    for (var o = 0; o < 5; o++) {
        let on = select(0.0, 1.0, f32(o) < octaves);
        let fade = clamp(wl / (footprint * 3.0) - 1.0, 0.0, 1.0) * on;
        let c = cos(angle);
        let s = sin(angle);
        let dir = vec2<f32>(c, s);
        let speed = sqrt(1.56 * wl) * 0.55;
        // rotate each octave so the lattices don't line up
        let q = vec2<f32>(c * p.x - s * p.y, s * p.x + c * p.y) / wl + dir * (speed * t / wl);
        let n = noised(q);
        // back to world axes (the inverse rotation)
        let g = vec2<f32>(c * n.y + s * n.z, -s * n.y + c * n.z);
        // the big swells feel the wind patches most
        let gust = mix(wind, 1.0, f32(o) * 0.2);
        slope += g * k * fade * gust;
        total += k;
        kept += k * fade;
        wl *= 0.37;
        k *= 1.25;
        angle += 2.1;
    }
    return vec3<f32>(slope, 1.0 - kept / max(total, 1e-4));
}

// Valley mist (keep in sync with terrain.wesl).
const MIST_FLOOR: f32 = 250.0;
const MIST_SCALE: f32 = 220.0;
const MIST_DENSITY: f32 = 0.00035;

fn valley_mist(cam: vec3<f32>, p: vec3<f32>, density: f32) -> f32 {
    let d = distance(cam, p);
    let k = (p.y - cam.y) / max(d, 1.0);
    let at_cam = exp(-(cam.y - MIST_FLOOR) / MIST_SCALE);
    var depth: f32;
    if (abs(k) < 1e-3) {
        depth = MIST_DENSITY * density * at_cam * d;
    } else {
        depth = MIST_DENSITY * density * at_cam * MIST_SCALE / k * (1.0 - exp(-k * d / MIST_SCALE));
    }
    return clamp(1.0 - exp(-depth), 0.0, 0.85);
}

// Past the atmosphere planet's horizon (see water_prepass.wesl; keep in sync).
fn past_horizon(pos: vec3<f32>) -> bool {
    let radius = water.params.w;
    let cam = view.world_position;
    let from_centre = vec3<f32>(cam.x, cam.y + radius, cam.z);
    let r = length(from_centre);
    let up = from_centre / r;
    let h = max(r - radius, 1.0);
    let horizon_mu = -sqrt(h * (2.0 * radius + h)) / r;
    let mu = dot(normalize(pos - cam), up);
    return radius > 0.0 && mu > horizon_mu;
}

fn in_frame(uv: vec2<f32>) -> bool {
    return all(uv > vec2<f32>(0.001)) && all(uv < vec2<f32>(0.999));
}

@fragment
fn fragment(
    in: VertexOutput,
    @builtin(front_facing) is_front: bool,
) -> FragmentOutput {
    var pbr_input = pbr_input_from_standard_material(in, is_front);

    let pos = in.world_position.xyz;
    let cam = view.world_position;
    let footprint = max(length(dpdx(pos)), length(dpdy(pos)));
    let t = globals.time;
    let frozen = water.zenith.a > 0.5;

    // ---- depth below the surface (the finest grid that covers this point; past the map: deep)
    let uv_near = (pos.xz - water.near_frame.xy) * water.near_frame.z;
    let uv_fine = (pos.xz - water.fine_frame.xy) * water.fine_frame.z;
    let uv_coarse = (pos.xz - water.coarse_frame.xy) * water.coarse_frame.z;
    let e_near = textureSampleLevel(near_depth, depth_sampler, uv_near, 0.0).r;
    let e_fine = textureSampleLevel(fine_depth, depth_sampler, uv_fine, 0.0).r;
    let e_coarse = textureSampleLevel(coarse_depth, depth_sampler, uv_coarse, 0.0).r;
    var e = select(1.0, e_coarse, in_frame(uv_coarse));
    e = select(e, e_fine, in_frame(uv_fine));
    e = select(e, e_near, in_frame(uv_near));
    let e2 = e * 2.0 - 1.0;
    let depth = max(sign(e2) * e2 * e2 * DEPTH_RANGE, 0.0);

    // ---- surface normal
    // wind patches darken and roughen the water in places (breaks up big areas from up high)
    let wind = 0.35 + 0.65 * smoothstep1(-0.55, 0.5, noised(pos.xz / 1900.0 + vec2<f32>(t * 0.004, 0.0)).x);
    var strength = water.params.y * select(1.0, 0.0, frozen);
    // calmer in the shallows
    strength *= 0.45 + 0.55 * smoothstep1(0.0, 4.0, depth);
    let r = ripples(pos.xz, t, footprint, water.params.z, wind);
    let lost = r.z;
    let n = normalize(vec3<f32>(-r.x * strength, 1.0, -r.y * strength));
    let to_cam = cam - pos;
    let dist = length(to_cam);
    let v = to_cam / max(dist, 1e-3);

    // ---- body colour, foam and ice (lit by the PBR path)
    let ds = water.shallow.a;
    let deepness = 1.0 - exp(-depth / ds);
    var body = mix(water.shallow.rgb, water.deep.rgb, deepness);
    // sand showing through right at the shore
    body = mix(body, water.shallow.rgb * 1.35 + vec3<f32>(0.03, 0.03, 0.02), exp(-depth / (ds * 0.06)) * 0.55);
    // darker where the wind roughens it
    body *= 0.75 + 0.25 * wind;

    var foam = 0.0;
    if (!frozen && water.deep.a > 0.01) {
        // waves washing in and out, broken up
        let lap = 0.5 + 0.5 * sin(t * 0.9 + noised(pos.xz / 45.0).x * 5.0);
        let edge = water.deep.a * (0.55 + 0.45 * lap);
        let breakup = noised(pos.xz / 5.0 + vec2<f32>(t * 0.25, t * 0.1)).x * 0.5 + 0.5;
        let band = 1.0 - smoothstep1(edge * 0.3, edge, depth);
        foam = band * smoothstep1(0.3, 0.7, breakup + band * 0.35);
        // a pixel wider than the band can't show it
        foam *= smoothstep1(25.0, 6.0, footprint);
    }

    var albedo = mix(body, vec3<f32>(0.85, 0.88, 0.9), foam);
    var roughness = 0.6;
    var reflectance = 0.0;
    if (frozen) {
        // ice: pale over the shallows, darker and clearer over the deep, with cracks and snow
        let crack_n = noised(pos.xz / 14.0).x + 0.5 * noised(pos.xz / 5.0).x;
        let crack = 1.0 - smoothstep1(0.0, 0.06, abs(crack_n)) * smoothstep1(30.0, 4.0, footprint);
        let drift = smoothstep1(0.1, 0.5, noised(pos.xz / 60.0).x + noised(pos.xz / 13.0).x * 0.4 + exp(-depth / 1.5) * 0.6);
        albedo = mix(body * (1.0 - 0.35 * crack), vec3<f32>(0.9, 0.92, 0.96), drift);
        roughness = mix(0.35, 0.8, drift);
        reflectance = 0.35;
    }
    pbr_input.material.base_color = vec4<f32>(albedo, 1.0);
    pbr_input.material.perceptual_roughness = roughness;
    pbr_input.material.reflectance = vec3<f32>(reflectance);
    // the body is lit by a gentler normal than the reflections
    pbr_input.N = normalize(mix(vec3<f32>(0.0, 1.0, 0.0), n, 0.3));

    var out: FragmentOutput;
    out.color = apply_pbr_lighting(pbr_input);

    // ---- reflections (post-exposure)
    let cos_v = max(dot(n, v), 0.0);
    let fresnel = 0.02 + 0.98 * pow(1.0 - cos_v, 5.0);
    var refl = reflect(-v, n);
    refl.y = abs(refl.y);
    let sky = mix(water.horizon.rgb, water.zenith.rgb, pow(refl.y, 0.5)) * water.horizon.a;

    // terrain shadow on the glint (inside the shadow cascades; outside, lit)
    let view_z = dot(vec4<f32>(
        view.view_from_world[0].z,
        view.view_from_world[1].z,
        view.view_from_world[2].z,
        view.view_from_world[3].z
    ), vec4<f32>(pos, 1.0));
    let shadow = fetch_directional_shadow(0u, vec4<f32>(pos, 1.0), vec3<f32>(0.0, 1.0, 0.0), view_z, in.position.xy);

    let sun_dir = water.sun.xyz;
    let rl = max(dot(refl, sun_dir), 0.0);
    // sharp sparkles up close, a broad glitter path once the ripples are sub-pixel
    let shin = mix(1400.0, 50.0, sqrt(lost));
    let sparkle = pow(rl, shin) * shin * 0.05;
    let sheen = pow(rl, shin * 0.06) * shin * 0.004;
    let glint = water.glint.rgb * (sparkle + sheen) * fresnel * shadow;

    let wet = select(1.0 - foam, 0.25, frozen);
    var col = mix(out.color.rgb, sky, fresnel * wet) + glint * wet;

    // ---- mist and fog, as the terrain
    let mist = valley_mist(cam, pos, water.mist.a);
    let toward_sun = pow(max(dot(-v, sun_dir), 0.0), 6.0);
    let mist_col = water.mist.rgb * (1.0 + 0.6 * toward_sun) * water.params.x;
    col = mix(col, mist_col, mist);
    out.color = main_pass_post_lighting_processing(pbr_input, vec4<f32>(col, 1.0));
    // (last, so the derivatives and samples above stay in uniform control flow)
    if (past_horizon(pos)) {
        discard;
    }
    return out;
}
