// Terrain detail shading.
//
// The baked textures (25 m/texel) give the large-scale look: erosion
// gullies, forest/rock/snow zones, cavity AO. On top of that, every pixel:
//  - perturbs the normal with world-space noise from ~60 m down to ~1.5 m,
//    fading octaves out once they're smaller than a pixel (no shimmer);
//  - re-decides rock / vegetation / snow from the *detailed* slope and height,
//    with noisy boundaries, so zones are crisp instead of 25 m blobs;
//  - adds colour variation (grass patches, rock strata, snow drifts).

import bevy_pbr::render::{
    pbr_fragment::pbr_input_from_standard_material,
    pbr_functions::alpha_discard,
    mesh_view_bindings::view,
};

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

struct TerrainDetail {
    // x: normal detail strength, y: snow line (m), z: rock slope threshold, w: mist brightness
    params: vec4<f32>,
    // rgb: mist colour, a: mist density multiplier (src/environment.rs)
    mist: vec4<f32>,
    // xyz: direction to the sun
    sun: vec4<f32>,
}

@group(constants::MATERIAL_BIND_GROUP) @binding(100)
var<uniform> terrain: TerrainDetail;

fn hash2(p: vec2<f32>) -> f32 {
    var q = fract(p * vec2<f32>(0.1031, 0.1030));
    q += dot(q, q.yx + 33.33);
    return fract((q.x + q.y) * q.x);
}

// Value noise with analytic derivatives: returns (value in -1..1, d/dx, d/dy).
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

// fBm height detail in metres and its gradient, from `wavelength` down,
// with octaves faded out once they're below ~2 pixels (`footprint` m/pixel).
fn detail_height(p: vec2<f32>, wavelength: f32, amplitude: f32, footprint: f32) -> vec3<f32> {
    var sum = vec3<f32>(0.0);
    var wl = wavelength;
    var amp = amplitude;
    // rotate each octave to hide the grid
    let rot = mat2x2<f32>(0.8, 0.6, -0.6, 0.8);
    var q = p;
    var basis = mat2x2<f32>(1.0, 0.0, 0.0, 1.0);
    for (var o = 0; o < 6; o++) {
        let fade = clamp((wl / (footprint * 3.0)) - 1.0, 0.0, 1.0);
        if (fade <= 0.0) { break; }
        let n = noised(q / wl);
        // derivative w.r.t. world xz: chain rule through scale and rotation
        let grad = (transpose(basis) * n.yz) / wl;
        sum += vec3<f32>(n.x, grad) * amp * fade;
        wl *= 0.45;
        amp *= 0.42;
        q = rot * q;
        basis = rot * basis;
    }
    return sum;
}

// Valley mist: exponential height fog that pools in the valleys.
const MIST_FLOOR: f32 = 250.0;     // where it's densest (valley floors are ~200-400 m)
const MIST_SCALE: f32 = 220.0;     // e-folding height: mostly gone ~700 m up
const MIST_DENSITY: f32 = 0.00035; // extinction per metre at the floor

/// Fraction of the view to `p` hidden by mist (integrated along the ray).
fn valley_mist(cam: vec3<f32>, p: vec3<f32>, density: f32) -> f32 {
    let d = distance(cam, p);
    let k = (p.y - cam.y) / max(d, 1.0); // vertical component of the view ray
    let at_cam = exp(-(cam.y - MIST_FLOOR) / MIST_SCALE);
    var depth: f32;
    if (abs(k) < 1e-3) {
        depth = MIST_DENSITY * density * at_cam * d;
    } else {
        depth = MIST_DENSITY * density * at_cam * MIST_SCALE / k * (1.0 - exp(-k * d / MIST_SCALE));
    }
    return clamp(1.0 - exp(-depth), 0.0, 0.85);
}

fn smoothstep1(a: f32, b: f32, x: f32) -> f32 {
    let t = clamp((x - a) / (b - a), 0.0, 1.0);
    return t * t * (3.0 - 2.0 * t);
}

@fragment
fn fragment(
    in: VertexOutput,
    @builtin(front_facing) is_front: bool,
) -> FragmentOutput {
    var pbr_input = pbr_input_from_standard_material(in, is_front);

    let pos = in.world_position.xyz;
    // metres per pixel here: drives how much detail we can show
    let footprint = max(length(dpdx(pos)), length(dpdy(pos)));

    // ---- detail normal ----
    let strength = terrain.params.x;
    let h = detail_height(pos.xz, 60.0, 4.0 * strength, footprint);
    let macro_n = normalize(pbr_input.N);
    // perturb the macro normal by the detail heightfield gradient
    var n = normalize(macro_n - vec3<f32>(h.y, 0.0, h.z) * macro_n.y);

    // ---- material zones, per pixel ----
    let slope = 1.0 - clamp(n.y, 0.0, 1.0);
    let breakup = noised(pos.xz / 35.0).x * 0.5 + noised(pos.xz / 9.0).x * 0.25;
    let rock = smoothstep1(terrain.params.z - 0.06, terrain.params.z + 0.06, slope + breakup * 0.08);
    let snow_line = terrain.params.y + breakup * 140.0;
    // snow settles on gentler slopes
    let snow = smoothstep1(snow_line - 60.0, snow_line + 120.0, pos.y)
        * (1.0 - smoothstep1(0.55, 0.75, slope));

    let macro_col = pbr_input.material.base_color.rgb;
    // vegetation: keep the baked palette, add patchy variation
    let patches = noised(pos.xz / 18.0).x * 0.5 + noised(pos.xz / 5.0).x * 0.3;
    var veg = macro_col * (0.85 + 0.25 * patches);
    // rock: grey-brown with strata that follow altitude, tinted by the macro colour
    // (warped by the breakup noise so the layers aren't ruler-straight)
    let strata = noised(vec2<f32>((pos.y + breakup * 12.0) / 6.0, (pos.x + pos.z) / 150.0)).x;
    var rock_col = mix(vec3<f32>(0.34, 0.31, 0.28), vec3<f32>(0.42, 0.39, 0.35), strata * 0.5 + 0.5);
    rock_col = mix(rock_col, macro_col, 0.25) * (0.85 + 0.2 * breakup);
    let snow_col = vec3<f32>(0.92, 0.94, 0.98) * (0.92 + 0.08 * patches);

    var col = mix(veg, rock_col, rock);
    col = mix(col, snow_col, snow);

    // fade the per-pixel detail out far away; the baked macro look takes over
    // (keyed on pixel footprint: ~10 m/pixel is several km away at 1440p)
    let far = smoothstep1(8.0, 30.0, footprint);
    col = mix(col, macro_col, far);
    n = normalize(mix(n, macro_n, far));

    pbr_input.material.base_color = vec4<f32>(col, 1.0);
    pbr_input.N = n;
    pbr_input.material.perceptual_roughness = mix(mix(0.92, 0.8, rock), 0.55, snow);

    pbr_input.material.base_color = alpha_discard(
        pbr_input.material.flags,
        pbr_input.material.alpha_cutoff,
        pbr_input.material.base_color
    );

    var out: FragmentOutput;
@if(PREPASS_PIPELINE) {
    out = deferred_output(in, pbr_input);
} @else {
    out.color = apply_pbr_lighting(pbr_input);
    // mist settles in the valleys; lit a soft blue-white, brighter toward the sun
    let cam = view.world_position;
    let mist = valley_mist(cam, pos, terrain.mist.a);
    let to_frag = normalize(pos - cam);
    let sun_dir = terrain.sun.xyz;
    let toward_sun = pow(max(dot(to_frag, -sun_dir) * -1.0, 0.0), 6.0);
    let mist_col = terrain.mist.rgb * (1.0 + 0.6 * toward_sun) * terrain.params.w;
    out.color = vec4<f32>(mix(out.color.rgb, mist_col, mist), out.color.a);
    out.color = main_pass_post_lighting_processing(pbr_input, out.color);
}
    return out;
}
