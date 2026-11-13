// Water depth prepass (src/terrain_gen/water.rs): no water past the
// atmosphere planet's horizon (`past_horizon`, as in water.wesl). The flat sea
// carries on past the round planet Bevy's atmosphere assumes, and its aerial
// perspective out there comes out as a dark blue strip under the sky; discarded
// here (and in the main pass), those pixels get the atmosphere's horizon instead.

import bevy_pbr::{
    prepass::io::{VertexOutput, FragmentOutput},
    render::mesh_view_bindings::view,
};

struct Water {
    shallow: vec4<f32>,
    deep: vec4<f32>,
    horizon: vec4<f32>,
    zenith: vec4<f32>,
    glint: vec4<f32>,
    sun: vec4<f32>,
    mist: vec4<f32>,
    // w: radius of the atmosphere's planet (0 = no atmosphere)
    params: vec4<f32>,
    near_frame: vec4<f32>,
    fine_frame: vec4<f32>,
    coarse_frame: vec4<f32>,
}

@group(constants::MATERIAL_BIND_GROUP) @binding(100)
var<uniform> water: Water;

// keep in sync with water.wesl
fn past_horizon(pos: vec3<f32>) -> bool {
    let radius = water.params.w;
    let cam = view.world_position;
    // the planet's centre is `radius` below the origin
    let from_centre = vec3<f32>(cam.x, cam.y + radius, cam.z);
    let r = length(from_centre);
    let up = from_centre / r;
    let h = max(r - radius, 1.0);
    let horizon_mu = -sqrt(h * (2.0 * radius + h)) / r;
    let mu = dot(normalize(pos - cam), up);
    return radius > 0.0 && mu > horizon_mu;
}

@if(PREPASS_FRAGMENT)
@fragment
fn fragment(in: VertexOutput) -> FragmentOutput {
    if (past_horizon(in.world_position.xyz)) {
        discard;
    }
    var out: FragmentOutput;
@if(NORMAL_PREPASS)
    out.normal = vec4<f32>(0.5, 1.0, 0.5, 1.0);
@if(MOTION_VECTOR_PREPASS)
    out.motion_vector = vec2<f32>(0.0);
@if(UNCLIPPED_DEPTH_ORTHO_EMULATION)
    out.frag_depth = in.unclipped_depth;
    return out;
}
@else
@fragment
fn fragment(in: VertexOutput) {
    if (past_horizon(in.world_position.xyz)) {
        discard;
    }
}
