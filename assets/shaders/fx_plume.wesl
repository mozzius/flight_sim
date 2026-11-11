// Volumetric engine plume, raymarched inside a cylinder. Local +Y runs down
// the exhaust; y = -half_length is just inside the nozzle. Additive.

import bevy_pbr::render::mesh_view_bindings::{view, globals};
import bevy_pbr::render::mesh_functions::get_world_from_local;
import bevy_pbr::render::view_transformations::depth_ndc_to_view_z;

struct PlumeParams {
    state: vec4<f32>,
    shape: vec4<f32>,
};

@group(constants::MATERIAL_BIND_GROUP) @binding(0) var<uniform> plume: PlumeParams;
@group(constants::MATERIAL_BIND_GROUP) @binding(1) var noise_tex: texture_2d<f32>;
@group(constants::MATERIAL_BIND_GROUP) @binding(2) var noise_sampler: sampler;

struct Vertex {
    @builtin(instance_index) instance_index: u32,
    @location(0) position: vec3<f32>,
};

struct VOut {
    @builtin(position) position: vec4<f32>,
    @location(0) local_pos: vec3<f32>,
    @location(1) local_cam: vec3<f32>,
    @location(2) world: vec3<f32>,
};

@vertex
fn vertex(v: Vertex) -> VOut {
    var out: VOut;
    let m = get_world_from_local(v.instance_index);
    let world = (m * vec4(v.position, 1.0)).xyz;
    out.position = view.clip_from_world * vec4(world, 1.0);
    let r = mat3x3<f32>(m[0].xyz, m[1].xyz, m[2].xyz);
    out.local_cam = transpose(r) * (view.world_position - m[3].xyz);
    out.local_pos = v.position;
    out.world = world;
    return out;
}

@if(DEPTH_PREPASS)
fn scene_view_z(frag: vec4<f32>) -> f32 {
    let d = textureLoad(bevy_pbr::render::mesh_view_bindings::depth_prepass_texture, vec2<i32>(frag.xy), 0);
    if d <= 0.0 {
        return 1e9;
    }
    return -depth_ndc_to_view_z(d);
}
@if(!DEPTH_PREPASS)
fn scene_view_z(frag: vec4<f32>) -> f32 {
    return 1e9;
}

fn hash3(p: vec3<f32>) -> f32 {
    var q = fract(p * 0.1031);
    q += dot(q, q.zyx + 31.32);
    return fract((q.x + q.y) * q.z);
}

fn vnoise(p: vec3<f32>) -> f32 {
    let i = floor(p);
    let f = fract(p);
    let u = f * f * (3.0 - 2.0 * f);
    let a = mix(mix(hash3(i), hash3(i + vec3(1.0, 0.0, 0.0)), u.x),
                mix(hash3(i + vec3(0.0, 1.0, 0.0)), hash3(i + vec3(1.0, 1.0, 0.0)), u.x), u.y);
    let b = mix(mix(hash3(i + vec3(0.0, 0.0, 1.0)), hash3(i + vec3(1.0, 0.0, 1.0)), u.x),
                mix(hash3(i + vec3(0.0, 1.0, 1.0)), hash3(i + vec3(1.0, 1.0, 1.0)), u.x), u.y);
    return mix(a, b, u.z);
}

const EXIT: f32 = 2.4; // distance from the mesh start to the nozzle exit plane

/// Emission at plume-space point `p`.
fn emission(p: vec3<f32>, t: f32) -> vec3<f32> {
    let dry = plume.state.x;
    let ab = plume.state.y;
    let seed = plume.state.z;
    let flick = plume.state.w;
    let s = p.y + plume.shape.x;     // distance from the mesh start
    let z = s - EXIT;                // distance past the nozzle exit
    // the 2D nozzle's rectangular jet rounds out and spreads downstream
    let spread = 1.0 + max(z, 0.0) * 0.07;
    let q = vec2(p.x / (plume.shape.z * spread), p.z / (plume.shape.w * spread * (1.0 + max(z, 0.0) * 0.05)));
    let r2 = dot(q, q);

    var e = vec3(0.0);
    // most samples in the hull are well outside the jet: skip them cheaply
    if r2 > 5.0 {
        return e;
    }

    // ---- inside the nozzle: glowing hot metal / flame front, seen through the mouth
    if z < 0.0 {
        let inside = smoothstep(1.0, 0.45, r2) * smoothstep(-EXIT, -EXIT + 0.8, z);
        let hot = mix(vec3(1.0, 0.3, 0.07) * (0.15 + dry * 0.6), vec3(1.0, 0.7, 0.38) * 5.0, ab);
        return hot * inside * 1.6;
    }

    // ---- turbulence that rushes downstream
    // tileable noise wrapped round the jet, rushing downstream
    let ang = atan2(p.z, p.x) * 0.159;
    let tuv = vec2(ang * 2.0 + seed * 0.37, p.y * 0.035 - t * 2.2);
    let nz = textureSampleLevel(noise_tex, noise_sampler, tuv + vec2(sqrt(r2) * 0.12, 0.0), 0.0);
    let turb = nz.r * 0.65 + nz.b * 0.35;

    // ---- afterburner
    if ab > 0.001 {
        let len = 6.5 * flick;
        let core_len = 3.5 * flick;
        // faint blue-violet sheath right at the exit
        let sheath = exp(-r2 * 1.6) * exp(-z * 1.4) * vec3(0.45, 0.35, 1.0) * 1.2;
        // the hot core
        let core = exp(-r2 * 3.2) * exp(-z * 0.45) * (1.0 - smoothstep(core_len * 0.4, core_len, z));
        let core_col = mix(vec3(1.0, 0.55, 0.2), vec3(1.0, 0.32, 0.06), clamp(z / core_len, 0.0, 1.0));
        // shock diamonds: lozenges with a bright Mach disk at each crossing
        let spacing = 1.0 + z * 0.06;
        let zz = z - 0.45;
        let k = floor(zz / spacing + 0.5);
        let ds = zz - k * spacing;
        let half = spacing * 0.5;
        let rr = sqrt(r2);
        let lozenge = 0.45 * (1.0 - abs(ds) / half);
        var diamonds = 0.0;
        if k >= 0.0 && k < 5.0 && zz > -0.2 {
            let fall = exp(-k * 0.6) * (1.0 - smoothstep(2.5, 4.6, k));
            let body = smoothstep(0.1, -0.1, rr - lozenge) * 0.3;
            let disk = exp(-(ds * ds) / 0.006) * smoothstep(0.5, 0.1, rr) * 1.4;
            diamonds = (body + disk) * fall;
        }
        // turbulent outer flame, ragged at the tail
        let outer_fade = 1.0 - smoothstep(len * 0.25, len * (0.75 + 0.25 * turb), z);
        let outer = exp(-r2 * 2.4) * outer_fade * (0.05 + 1.1 * turb * turb);
        let outer_col = mix(vec3(1.0, 0.3, 0.05), vec3(0.75, 0.1, 0.015), clamp(z / len, 0.0, 1.0));
        e += ab * (sheath * 0.6 + core * core_col * 2.0 + diamonds * vec3(1.0, 0.68, 0.32) * 6.0 + outer * outer_col * 0.35);
    }

    // ---- dry power: barely visible, a dull glow just past the exit
    let dry_glow = exp(-r2 * 2.0) * exp(-z * 2.2) * dry * dry * (0.6 + 0.6 * turb);
    e += (1.0 - ab) * dry_glow * vec3(1.0, 0.35, 0.1) * 0.35;
    return e;
}

@fragment
fn fragment(in: VOut) -> @location(0) vec4<f32> {
    let half_len = plume.shape.x;
    let radius = plume.shape.y;
    let ro = in.local_cam;
    let rd = normalize(in.local_pos - in.local_cam);

    // ray vs. capped cylinder around the Y axis
    let a = dot(rd.xz, rd.xz);
    let b = dot(ro.xz, rd.xz);
    let c = dot(ro.xz, ro.xz) - radius * radius;
    let disc = b * b - a * c;
    if disc <= 0.0 || a < 1e-6 {
        discard;
    }
    let sq = sqrt(disc);
    var t0 = (-b - sq) / a;
    var t1 = (-b + sq) / a;
    let ty0 = (-half_len - ro.y) / rd.y;
    let ty1 = (half_len - ro.y) / rd.y;
    t0 = max(t0, min(ty0, ty1));
    t1 = min(t1, max(ty0, ty1));
    t0 = max(t0, 0.0);

    // rays that never come near the jet axis see nothing: skip the march
    let closest = abs(ro.x * rd.z - ro.z * rd.x) / sqrt(a);
    if closest > radius * 0.85 {
        discard;
    }

    // stop at whatever opaque geometry is in front (the airframe)
    let world_rd = normalize(in.world - view.world_position);
    let fwd = -view.world_from_view[2].xyz;
    let scene_t = scene_view_z(in.position) / max(dot(world_rd, fwd), 1e-3);
    t1 = min(t1, scene_t);
    if t1 <= t0 {
        discard;
    }

    let steps = 20;
    let dt = (t1 - t0) / f32(steps);
    // interleaved gradient noise: far less visible grain than a white hash
    let jitter = fract(52.9829189 * fract(dot(in.position.xy, vec2(0.06711056, 0.00583715))));
    var sum = vec3(0.0);
    for (var i = 0; i < steps; i++) {
        let p = ro + rd * (t0 + (f32(i) + jitter) * dt);
        sum += emission(p, globals.time);
    }
    return vec4(sum * dt * 0.32, 0.0);
}
