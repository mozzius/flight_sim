// Heat haze behind the nozzles: refracts the opaque scene (the view
// transmission texture) with noise rushing downstream.

import bevy_pbr::render::mesh_view_bindings::{view, globals, view_transmission_texture, view_transmission_sampler};
import bevy_pbr::render::mesh_functions::get_world_from_local;

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
    @location(1) world: vec3<f32>,
    @location(2) world_normal: vec3<f32>,
};

@vertex
fn vertex(v: Vertex) -> VOut {
    var out: VOut;
    let m = get_world_from_local(v.instance_index);
    let world = (m * vec4(v.position, 1.0)).xyz;
    out.position = view.clip_from_world * vec4(world, 1.0);
    out.local_pos = v.position;
    out.world = world;
    out.world_normal = (m * vec4(v.position.x, 0.0, v.position.z, 0.0)).xyz;
    return out;
}

@if(DEPTH_PREPASS)
fn sample_is_nearer(coord: vec2<f32>, depth: f32) -> bool {
    let d = textureLoad(bevy_pbr::render::mesh_view_bindings::depth_prepass_texture, vec2<i32>(coord), 0);
    return d > depth;
}
@if(!DEPTH_PREPASS)
fn sample_is_nearer(coord: vec2<f32>, depth: f32) -> bool {
    return false;
}

@fragment
fn fragment(in: VOut) -> @location(0) vec4<f32> {
    let strength = plume.state.x;
    let half_len = plume.shape.x;
    let s = in.local_pos.y + half_len;
    // strongest down the middle of the jet, nothing at the shell's silhouette
    let to_cam = normalize(view.world_position - in.world);
    let facing = abs(dot(normalize(in.world_normal), to_cam));
    let along = smoothstep(0.5, 2.5, s) * (1.0 - smoothstep(half_len * 0.5, half_len * 1.9, s));
    let w = clamp(strength * along * smoothstep(0.1, 0.7, facing), 0.0, 1.0);
    if w < 0.002 {
        discard;
    }
    let t = globals.time;
    let uv_n = vec2(atan2(in.local_pos.z, in.local_pos.x) * 0.5, s * 0.08 - t * 2.4);
    let a = textureSample(noise_tex, noise_sampler, uv_n);
    let b = textureSample(noise_tex, noise_sampler, uv_n * 2.7 + vec2(0.3, -t * 1.3));
    let offset = (vec2(a.r, b.g) - 0.5) * 2.0;
    let dist = length(view.world_position - in.world);
    // a few pixels of shimmer at chase-camera distance, scaled with distance
    let size = vec2<f32>(textureDimensions(view_transmission_texture));
    let px = w * 3.5 * clamp(30.0 / dist, 0.0, 1.5) * (size.y / 1440.0);
    var coord = in.position.xy + offset * px;
    // never drag nearer things (the jet itself) into the shimmer
    if sample_is_nearer(coord, in.position.z) {
        coord = in.position.xy;
    }
    let col = textureSampleLevel(view_transmission_texture, view_transmission_sampler, coord / size, 0.0);
    return vec4(col.rgb, smoothstep(0.0, 0.25, w));
}
