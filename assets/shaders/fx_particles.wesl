// Particles: sun-lit smoke puffs, fire that cools into smoke, sparks, glows
// and trail tubes, all premultiplied alpha. See src/effects/particles.rs.

import bevy_pbr::render::mesh_view_bindings::{view, globals};
import bevy_pbr::render::view_transformations::depth_ndc_to_view_z;

struct FxGlobals {
    sun_dir: vec4<f32>,
    sun: vec4<f32>,
    ambient: vec4<f32>,
    misc: vec4<f32>,
};

@group(constants::MATERIAL_BIND_GROUP) @binding(0) var<uniform> fx: FxGlobals;
@group(constants::MATERIAL_BIND_GROUP) @binding(1) var atlas: texture_2d<f32>;
@group(constants::MATERIAL_BIND_GROUP) @binding(2) var atlas_sampler: sampler;
@group(constants::MATERIAL_BIND_GROUP) @binding(3) var noise_tex: texture_2d<f32>;
@group(constants::MATERIAL_BIND_GROUP) @binding(4) var noise_sampler: sampler;

const PI: f32 = 3.14159265;
const TILES: f32 = 4.0;

struct Vertex {
    @location(0) position: vec3<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) color: vec4<f32>,
    // heat, dissolve, tile, seed
    @location(3) a: vec4<f32>,
    // kind, size, glow, rotation
    @location(4) b: vec4<f32>,
};

struct VOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) a: vec4<f32>,
    @location(3) b: vec4<f32>,
    @location(4) world: vec3<f32>,
};

@vertex
fn vertex(v: Vertex) -> VOut {
    var out: VOut;
    out.position = view.clip_from_world * vec4(v.position, 1.0);
    out.uv = v.uv;
    out.color = v.color;
    out.a = v.a;
    out.b = v.b;
    out.world = v.position;
    return out;
}

@if(DEPTH_PREPASS && MULTISAMPLED)
fn scene_depth(frag: vec4<f32>) -> f32 {
    return textureLoad(bevy_pbr::render::mesh_view_bindings::depth_prepass_texture, vec2<i32>(frag.xy), 0);
}
@if(DEPTH_PREPASS && !MULTISAMPLED)
fn scene_depth(frag: vec4<f32>) -> f32 {
    return textureLoad(bevy_pbr::render::mesh_view_bindings::depth_prepass_texture, vec2<i32>(frag.xy), 0);
}
@if(!DEPTH_PREPASS)
fn scene_depth(frag: vec4<f32>) -> f32 {
    return 0.0;
}

/// Fade where the quad nears opaque geometry, and right in front of the camera.
fn soft_fade(frag: vec4<f32>, size: f32, softness: f32) -> f32 {
    let z = -depth_ndc_to_view_z(frag.z);
    // big puffs right in front of the lens fade out early: they'd only be a
    // full-screen smear (and a lot of overdraw)
    var fade = clamp((z - 0.5 - size * 0.25) / max(size * 0.75, 0.5), 0.0, 1.0);
    let d = scene_depth(frag);
    if d > 0.0 {
        let scene_z = -depth_ndc_to_view_z(d);
        fade *= clamp((scene_z - z) / max(size * 0.35 * softness, 0.05), 0.0, 1.0);
    }
    return fade;
}

/// Black body-ish ramp: deep red -> orange -> yellow -> pale yellow.
/// Kept moderate: ACES bleaches anything much brighter to white.
fn fire_color(t: f32) -> vec3<f32> {
    let x = clamp(t, 0.0, 1.4);
    var c = mix(vec3(0.5, 0.05, 0.01), vec3(1.0, 0.3, 0.04), smoothstep(0.1, 0.45, x));
    c = mix(c, vec3(1.0, 0.55, 0.14), smoothstep(0.45, 0.8, x));
    c = mix(c, vec3(1.0, 0.8, 0.45), smoothstep(0.8, 1.3, x));
    let intensity = x * x * (0.6 + 0.4 * x);
    return c * intensity;
}

fn hg(cos_theta: f32, g: f32) -> f32 {
    let g2 = g * g;
    return (1.0 - g2) / (4.0 * PI * pow(1.0 + g2 - 2.0 * g * cos_theta, 1.5));
}

/// Sun + sky lighting of a smoke surface with normal `n`.
/// `thick` is how deep into the puff this texel is, `density` its opacity.
fn light_smoke(albedo: vec3<f32>, n: vec3<f32>, to_cam: vec3<f32>, thick: f32, density: f32) -> vec3<f32> {
    let l = fx.sun_dir.xyz;
    let ndl = dot(n, l);
    // wrapped diffuse: smoke scatters light round into the terminator
    let diffuse = pow(clamp(ndl * 0.55 + 0.45, 0.0, 1.0), 1.4);
    // thick middles of puffs facing away from the sun sit in their own shadow
    let away = clamp(-dot(to_cam, l), 0.0, 1.0);
    let self_shadow = mix(1.0, 0.45, thick * away);
    // silver lining: thin edges glow when looking toward the sun
    let forward = hg(dot(-to_cam, l), 0.55) * (1.0 - density * 0.85) * 3.0;
    let sun = fx.sun.rgb * fx.sun.w * view.exposure;
    let sky = fx.ambient.rgb * fx.ambient.w * view.exposure * (0.55 + 0.45 * n.y);
    return albedo / PI * (sun * (diffuse * self_shadow + forward) + sky);
}

@fragment
fn fragment(in: VOut) -> @location(0) vec4<f32> {
    let kind = i32(in.b.x + 0.5);
    let size = in.b.y;
    let glow = in.b.z;
    let heat = in.a.x;
    let dissolve = in.a.y;
    let seed = in.a.w;
    let opacity = in.color.a;
    let albedo = in.color.rgb;
    let t = globals.time;

    let right = view.world_from_view[0].xyz;
    let up = view.world_from_view[1].xyz;
    let to_cam = normalize(view.world_position - in.world);

    if kind == 1 {
        // spark: thin hot streak, brightest at the head
        let u = in.uv.x * 2.0 - 1.0;
        let v = in.uv.y;
        let core = exp(-u * u * 9.0) * smoothstep(1.0, 0.75, v) * smoothstep(0.0, 0.25, v) * (0.35 + 0.65 * v);
        let e = fire_color(0.7 + heat * 0.4) * glow * core * opacity;
        return vec4(e, 0.0);
    }
    if kind == 2 {
        // glow: hot core with a wide soft halo
        let r = length(in.uv * 2.0 - 1.0);
        let g = exp(-r * r * 9.0) * 0.8 + exp(-r * r * 40.0) * 1.5;
        let e = fire_color(0.75 + heat * 0.35) * glow * g * opacity;
        return vec4(e, 0.0);
    }

    let rot = in.b.w;
    let x_axis = right * cos(rot) + up * sin(rot);
    let y_axis = up * cos(rot) - right * sin(rot);

    var density: f32;
    var n: vec3<f32>;
    var thick: f32;
    if kind == 3 {
        // tube: a lit cylinder impostor, its edge eaten by noise along the trail
        let across = in.uv.x * 2.0 - 1.0;
        let along = in.uv.y;
        let profile = sqrt(clamp(1.0 - across * across, 0.0, 1.0));
        let nz = textureSample(noise_tex, noise_sampler, vec2(along * 0.012 + seed, in.uv.x * 0.35 + seed * 3.1));
        let nz2 = textureSample(noise_tex, noise_sampler, vec2(along * 0.045, in.uv.x * 0.9 + seed * 1.3 + t * 0.01));
        let lumpy = 0.55 + 0.9 * (nz.r * 0.6 + nz2.b * 0.4) - 0.45;
        let erosion = dissolve * (0.6 + 0.8 * nz2.g);
        density = clamp((profile * lumpy - erosion) * 2.2, 0.0, 1.0) * profile;
        n = normalize(x_axis * across + to_cam * profile + up * (nz.g - 0.5) * 0.6);
        thick = profile;
    } else {
        // puff: atlas tile, gently churned by scrolling noise
        let nz = textureSample(noise_tex, noise_sampler, in.uv * 0.45 + vec2(seed * 7.0, seed * 3.0) + vec2(0.0, -t * 0.04));
        let warp = (nz.rg - 0.5) * (0.07 + heat * 0.06);
        let tile_index = floor(in.a.z);
        let tile = vec2(tile_index % TILES, floor(tile_index / TILES));
        let tuv = (tile + clamp(in.uv + warp, vec2(0.0), vec2(1.0))) / TILES;
        let s = textureSample(atlas, atlas_sampler, tuv);
        // dissolve from the thin rims inward, unevenly
        let erosion = dissolve * (0.55 + 0.9 * nz.b);
        density = clamp((s.a - erosion) / max(1.0 - erosion, 0.05), 0.0, 1.0) * (1.0 - dissolve * 0.35);
        let tn = vec2(s.r, s.g) * 2.0 - 1.0;
        let tz = sqrt(clamp(1.0 - dot(tn, tn), 0.0, 1.0));
        n = normalize(x_axis * tn.x + y_axis * tn.y + to_cam * tz);
        thick = s.b;
    }

    var alpha = density * opacity * soft_fade(in.position, size, fract(in.a.z) / 0.49);
    if alpha < 0.002 {
        discard;
    }
    let lit = light_smoke(albedo, n, to_cam, thick, density);

    // fire: hottest in the thick core of the puff, flickering with the noise
    var emission = vec3(0.0);
    if heat > 0.01 {
        let flick = textureSample(noise_tex, noise_sampler, in.uv * 0.8 + vec2(seed * 5.0, -t * 0.35 - seed)).a;
        let temp = heat * (0.45 + 0.55 * thick) * (0.8 + 0.4 * flick);
        emission = fire_color(temp) * glow;
    }
    // hot gas is less opaque than the soot it turns into
    let soot = clamp(1.0 - heat * 0.9, 0.0, 1.0);
    let rgb = (lit * soot + emission) * alpha;
    let a = alpha * mix(0.55, 1.0, soot);
    return vec4(rgb, a);
}
