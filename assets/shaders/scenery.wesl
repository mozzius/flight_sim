// Trees and roads: plain PBR (vertex colours) plus the same valley mist as
// the terrain (see terrain.wesl), so they sink into it together.

import bevy_pbr::render::{
    pbr_fragment::pbr_input_from_standard_material,
    pbr_functions::alpha_discard,
    mesh_view_bindings::{view, lights},
};

@if(VISIBILITY_RANGE_DITHER)
import bevy_pbr::render::pbr_functions::visibility_range_dither;

@if(PREPASS_PIPELINE)
import bevy_pbr::{
    prepass::io::{VertexOutput, FragmentOutput},
    deferred::functions::deferred_output,
};
@else
import bevy_pbr::render::{
    forward_io::{VertexOutput, FragmentOutput},
    pbr_functions::{apply_pbr_lighting, main_pass_post_lighting_processing},
};

struct Scenery {
    // x: mist brightness
    params: vec4<f32>,
    // rgb: mist colour, a: mist density multiplier (src/environment.rs)
    mist: vec4<f32>,
    // xyz: direction to the sun
    sun: vec4<f32>,
    // x, y: cloud layer bottom / top (m), z: cloud shadow map extent (m), w: 1 = clouds
    clouds: vec4<f32>,
}

@group(constants::MATERIAL_BIND_GROUP) @binding(100)
var<uniform> scenery: Scenery;

@group(constants::MATERIAL_BIND_GROUP) @binding(110)
var cloud_shadow_map: texture_2d<f32>;
@group(constants::MATERIAL_BIND_GROUP) @binding(111)
var cloud_shadow_sampler: sampler;

// Sunlight let through the clouds at `p` (1 = clear): a lookup in the clouds'
// top-down shadow map (fs_shadow in clouds.wgsl; keep the two in sync).
// Texels sit where the sun rays cross a reference altitude, so slide `p` along
// its sun ray to that altitude first. Only the part of the layer above `p`
// shades it; past the edge of the map (which follows the camera) there's none.
const CLOUD_SHADOW_REF: f32 = 3000.0;
fn cloud_shadow(p: vec3<f32>) -> f32 {
    let c = scenery.clouds;
    let sun = lights.directional_lights[0].direction_to_light;
    let slide = sun.xz / max(sun.y, 0.08);
    let texel = c.z / f32(textureDimensions(cloud_shadow_map).x);
    let origin = floor(view.world_position.xz / texel) * texel + slide * CLOUD_SHADOW_REF - 0.5 * c.z;
    let uv = (p.xz + slide * (CLOUD_SHADOW_REF - p.y) - origin) / c.z;
    let t = textureSampleLevel(cloud_shadow_map, cloud_shadow_sampler, uv, 0.0).r;
    let above = clamp((c.y - p.y) / max(c.y - c.x, 1.0), 0.0, 1.0);
    let edge = max(abs(uv.x - 0.5), abs(uv.y - 0.5));
    let s = exp(log(max(t, 0.002)) * above * (1.0 - smoothstep(0.4, 0.5, edge)));
    return mix(1.0, s, c.w);
}

// keep in sync with terrain.wesl
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

@fragment
fn fragment(
    in: VertexOutput,
    @builtin(front_facing) is_front: bool,
) -> FragmentOutput {
@if(VISIBILITY_RANGE_DITHER)
    visibility_range_dither(in.position, in.visibility_range_dither);

    var pbr_input = pbr_input_from_standard_material(in, is_front);
    pbr_input.material.base_color = alpha_discard(
        pbr_input.material.flags,
        pbr_input.material.alpha_cutoff,
        pbr_input.material.base_color
    );

    var out: FragmentOutput;
@if(PREPASS_PIPELINE) {
    out = deferred_output(in, pbr_input);
} @else {
    let pos = in.world_position.xyz;
    pbr_input.directional_shadow_factor = cloud_shadow(pos);
    out.color = apply_pbr_lighting(pbr_input);
    let cam = view.world_position;
    let mist = valley_mist(cam, pos, scenery.mist.a);
    let to_frag = normalize(pos - cam);
    let sun_dir = scenery.sun.xyz;
    let toward_sun = pow(max(dot(to_frag, -sun_dir) * -1.0, 0.0), 6.0);
    let mist_col = scenery.mist.rgb * (1.0 + 0.6 * toward_sun) * scenery.params.x;
    out.color = vec4<f32>(mix(out.color.rgb, mist_col, mist), out.color.a);
    out.color = main_pass_post_lighting_processing(pbr_input, out.color);
}
    return out;
}
