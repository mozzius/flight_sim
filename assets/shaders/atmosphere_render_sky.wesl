// Bevy 0.20.1's atmosphere `render_sky.wesl`, swapped in for the original at startup
// (`camera::patch_atmosphere`; keep it in step with Bevy's when upgrading). One change:
// the transmittance from the camera to opaque geometry.
//
// Bevy reads it off the transmittance LUT as a ratio of two transmittances to the top
// of the atmosphere (Bruneton's `sample_transmittance_lut_segment`), with a different
// formula for rays that dip below the round planet's horizon. Near that horizon both
// are ratios of tiny, grazing-angle values (blue ~1e-4 in a 16-bit float LUT), and the
// two formulas disagree: the flat world's distant islands and peaks that straddle the
// sea horizon came out with a sharp horizontal seam there (a violet line natively, a
// washed-out lower half on phones, where the blend uses the mean transmittance). Here
// it's integrated along the actual segment instead (a few samples of the medium's
// density LUT), which is continuous across the horizon and matches the aerial
// perspective LUT's own march.

import package::atmosphere::{
    bindings::{view, settings},
    functions::{
        sample_transmittance_lut,
        sample_sky_view_lut, direction_world_to_atmosphere,
        uv_to_ray_direction, uv_to_ndc, sample_aerial_view_lut,
        sample_sun_radiance, ndc_to_camera_dist, raymarch_atmosphere,
        get_view_position, max_atmosphere_distance, sample_density_lut,
        ABSORPTION_DENSITY, SCATTERING_DENSITY,
    },
};

import bevy_core_pipeline::fullscreen_vertex_shader::FullscreenVertexOutput;

@if(DUAL_SOURCE_BLENDING)
enable dual_source_blending;

@if(MULTISAMPLED)
@group(0) @binding(13) var depth_texture: texture_depth_multisampled_2d;
@else
@group(0) @binding(13) var depth_texture: texture_depth_2d;

struct RenderSkyOutput {
    @if(DUAL_SOURCE_BLENDING)
    @location(0) @blend_src(0) inscattering: vec4<f32>,
    @if(DUAL_SOURCE_BLENDING)
    @location(0) @blend_src(1) transmittance: vec4<f32>,
    @else
    @location(0) inscattering: vec4<f32>,
}

// Midpoint samples along the segment: the density falls off exponentially with
// altitude, which changes nearly linearly along a straight ray this short.
const SEGMENT_SAMPLES: u32 = 8u;

// Transmittance along `t` of the ray from `pos` (atmosphere space) towards `dir`.
fn segment_transmittance(pos: vec3<f32>, dir: vec3<f32>, t: f32) -> vec3<f32> {
    var optical_depth = vec3(0.0);
    let dt = t / f32(SEGMENT_SAMPLES);
    for (var i = 0u; i < SEGMENT_SAMPLES; i++) {
        let r = length(pos + dir * (dt * (f32(i) + 0.5)));
        optical_depth += sample_density_lut(r, ABSORPTION_DENSITY) + sample_density_lut(r, SCATTERING_DENSITY);
    }
    return exp(-optical_depth * dt);
}

@fragment
fn main(in: FullscreenVertexOutput) -> RenderSkyOutput {
    let depth = textureLoad(depth_texture, vec2<i32>(in.position.xy), 0);

    let ray_dir_ws = uv_to_ray_direction(in.uv);
    let world_pos = get_view_position();
    let r = length(world_pos);
    let up = normalize(world_pos);
    let mu = dot(ray_dir_ws, up);
    let max_samples = settings.sky_max_samples;
    let should_raymarch = settings.rendering_method == 1u;

    var transmittance: vec3<f32>;
    var inscattering: vec3<f32>;

    let sun_radiance = sample_sun_radiance(ray_dir_ws);

    if depth == 0.0 {
        let ray_dir_as = direction_world_to_atmosphere(ray_dir_ws);
        transmittance = sample_transmittance_lut(r, mu);
        inscattering = sample_sky_view_lut(r, ray_dir_as);
        if should_raymarch {
            let t_max = max_atmosphere_distance(r, mu);
            let result = raymarch_atmosphere(world_pos, ray_dir_ws, t_max, max_samples, in.uv, true);
            inscattering = result.inscattering;
            transmittance = result.transmittance;
        }
        inscattering += sun_radiance * transmittance;
    } else {
        let t = ndc_to_camera_dist(vec3(uv_to_ndc(in.uv), depth));
        inscattering = sample_aerial_view_lut(in.uv, t);
        transmittance = segment_transmittance(world_pos, ray_dir_ws, t);
        if should_raymarch {
            let result = raymarch_atmosphere(world_pos, ray_dir_ws, t, max_samples, in.uv, false);
            inscattering = result.inscattering;
            transmittance = result.transmittance;
        }
    }

    // exposure compensation
    inscattering *= view.exposure;

    @if(DUAL_SOURCE_BLENDING)
    return RenderSkyOutput(vec4(inscattering, 0.0), vec4(transmittance, 1.0));
    @if(!DUAL_SOURCE_BLENDING)
    let mean_transmittance = (transmittance.r + transmittance.g + transmittance.b) / 3.0;
    @if(!DUAL_SOURCE_BLENDING)
    return RenderSkyOutput(vec4(inscattering, mean_transmittance));

}
