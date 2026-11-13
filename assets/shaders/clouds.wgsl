// Raymarched volumetric clouds.
//
// Two passes share this file:
//  * `fs_raymarch` runs at reduced resolution and marches a cumulus layer
//    (two spherical shells around the planet) through Perlin-Worley shape
//    noise eroded by Worley detail noise, lit with Beer-Lambert sun shadowing,
//    a multi-octave multiple-scattering approximation, dual-lobe
//    Henyey-Greenstein phase and a height-graded sky ambient. It writes
//    premultiplied radiance + opacity, plus the scene distance it clipped
//    against and the mean cloud distance.
//  * `fs_composite` upsamples that to full resolution with a depth-aware
//    (nearest-depth) filter and blends it over the scene, drawn as the first
//    item of the transparent pass so nearer effects still draw over it.

struct Clouds {
    world_from_clip: mat4x4<f32>,
    prev_clip_from_world: mat4x4<f32>,
    camera_pos: vec3<f32>,
    time: f32,
    sun_dir: vec3<f32>,
    exposure: f32,
    sun_color: vec3<f32>,
    planet_radius: f32,
    ambient_top: vec3<f32>,
    layer_bottom: f32,
    ambient_bottom: vec3<f32>,
    layer_top: f32,
    full_size: vec2<f32>,
    low_size: vec2<f32>,
    wind: vec2<f32>,
    weather_scale: f32,
    base_scale: f32,
    detail_scale: f32,
    coverage_bias: f32,
    extinction: f32,
    max_distance: f32,
    fade_distance: f32,
    sun_mult: f32,
    ambient_mult: f32,
    ms_albedo: f32,
    detail_fade_start: f32,
    detail_fade_end: f32,
    divisor: u32,
    max_steps: u32,
    step_min: f32,
    step_scale: f32,
    debug: u32,
    frame: u32,
    history_valid: u32,
    temporal_blend: f32,
    fade_start: f32,
}

@group(0) @binding(0) var<uniform> u: Clouds;

// raymarch pass
@group(0) @binding(1) var base_tex: texture_3d<f32>;
@group(0) @binding(2) var detail_tex: texture_3d<f32>;
@group(0) @binding(3) var weather_tex: texture_2d<f32>;
@group(0) @binding(4) var repeat_sampler: sampler;

// composite pass
@group(0) @binding(1) var cloud_color: texture_2d<f32>;
@group(0) @binding(2) var cloud_data: texture_2d<f32>;

// linearize pass. DEPTH_TEXTURE_TYPE is substituted by clouds.rs:
// texture_depth_2d or texture_depth_multisampled_2d, depending on the
// camera's MSAA setting.
@group(0) @binding(5) var scene_depth: DEPTH_TEXTURE_TYPE;

// raymarch + composite passes: full-res distance to the scene, metres
@group(0) @binding(6) var scene_dist: texture_2d<f32>;

// temporal resolve pass: last frame's resolved output
@group(0) @binding(7) var history_color: texture_2d<f32>;
@group(0) @binding(8) var history_data: texture_2d<f32>;
@group(0) @binding(9) var clamp_sampler: sampler;

const PI: f32 = 3.14159265;
const FAR_KM: f32 = 1000.0;

struct VsOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

@vertex
fn vs_fullscreen(@builtin(vertex_index) vi: u32) -> VsOut {
    let uv = vec2<f32>(f32((vi << 1u) & 2u), f32(vi & 2u));
    var out: VsOut;
    out.position = vec4<f32>(uv * vec2(2.0, -2.0) + vec2(-1.0, 1.0), 0.0, 1.0);
    out.uv = uv;
    return out;
}

// ---------------------------------------------------------------------------
// camera helpers

fn ndc_of(uv: vec2<f32>) -> vec2<f32> {
    return vec2(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0);
}

fn ray_dir(uv: vec2<f32>) -> vec3<f32> {
    let p = u.world_from_clip * vec4(ndc_of(uv), 0.5, 1.0);
    return normalize(p.xyz / p.w - u.camera_pos);
}

/// Distance along the view ray to the scene, in metres (huge when the depth
/// buffer holds the far-plane clear value).
fn depth_to_dist(uv: vec2<f32>, depth: f32) -> f32 {
    if depth <= 0.0 {
        return FAR_KM * 1000.0;
    }
    let p = u.world_from_clip * vec4(ndc_of(uv), depth, 1.0);
    return min(length(p.xyz / p.w - u.camera_pos), FAR_KM * 1000.0);
}

fn load_dist(px: vec2<i32>) -> f32 {
    let c = clamp(px, vec2(0), vec2<i32>(u.full_size) - 1);
    return textureLoad(scene_dist, c, 0).x;
}

// ---------------------------------------------------------------------------
// linearize pass: the depth buffer can't be sampled inside the transparent
// pass (it's that pass's depth attachment), so copy it out as distances first.

@fragment
fn fs_linearize(in: VsOut) -> @location(0) f32 {
    let depth = textureLoad(scene_depth, vec2<i32>(in.position.xy), 0);
    return depth_to_dist(in.uv, depth);
}

// ---------------------------------------------------------------------------
// shape

fn remap(v: f32, lo: f32, hi: f32, nlo: f32, nhi: f32) -> f32 {
    return nlo + (v - lo) / (hi - lo) * (nhi - nlo);
}

fn planet_centre() -> vec3<f32> {
    return vec3(0.0, -u.planet_radius, 0.0);
}

fn altitude(p: vec3<f32>) -> f32 {
    return length(p - planet_centre()) - u.planet_radius;
}

fn height_frac(p: vec3<f32>) -> f32 {
    return (altitude(p) - u.layer_bottom) / (u.layer_top - u.layer_bottom);
}

/// (coverage, cloud type) at a world position, wind-advected.
fn sample_weather(p: vec3<f32>) -> vec2<f32> {
    let uv = (p.xz + u.wind * u.time) / u.weather_scale;
    return textureSampleLevel(weather_tex, repeat_sampler, uv, 0.0).rg;
}

/// Vertical profile: everything fades in just above the base; strato-cumulus
/// (type 0) stops low, cumulus (type 1) towers to the top of the layer.
fn height_gradient(h: f32, cloud_type: f32) -> f32 {
    let top = mix(0.28, 1.0, cloud_type);
    return smoothstep(0.0, 0.08, h) * (1.0 - smoothstep(top * 0.65, top, h));
}

/// `detail` is how much erosion to apply (0 = none); it is faded out with
/// distance where the march steps are too coarse to resolve it.
fn density(p: vec3<f32>, h: f32, w: vec2<f32>, detail: f32) -> f32 {
    let coverage = saturate(w.x + u.coverage_bias);
    let grad = height_gradient(h, w.y);
    if coverage <= 0.0 || grad <= 0.0 {
        return 0.0;
    }
    let wind = vec3(u.wind.x, 0.0, u.wind.y) * u.time;
    // base shape, sheared slightly with height so towers lean downwind
    let bp = (p + wind + vec3(u.wind.x, 0.0, u.wind.y) * h * 60.0) / u.base_scale;
    let n = textureSampleLevel(base_tex, repeat_sampler, bp, 0.0);
    let low_fbm = n.g * 0.625 + n.b * 0.25 + n.a * 0.125;
    var d = remap(n.r, low_fbm - 1.0, 1.0, 0.0, 1.0);
    d *= grad;
    // carve the coverage out of the shape; multiplying by coverage keeps
    // the edges of banks wispy
    let cov = coverage * mix(1.0, 0.65, h);
    d = remap(d, 1.0 - cov, 1.0, 0.0, 1.0) * cov;
    if d <= 0.0 {
        return 0.0;
    }
    if detail > 0.0 {
        let dp = (p + wind * 1.4) / u.detail_scale;
        let dn = textureSampleLevel(detail_tex, repeat_sampler, dp, 0.0).rgb;
        let dfbm = dn.r * 0.625 + dn.g * 0.25 + dn.b * 0.125;
        // wispy near the base, billowy higher up
        let dmod = mix(dfbm, 1.0 - dfbm, saturate(h * 6.0));
        d = remap(d, dmod * 0.42 * detail, 1.0, 0.0, 1.0);
    }
    return saturate(d);
}

// ---------------------------------------------------------------------------
// geometry

/// Entry/exit distances of a ray with a sphere about the planet centre
/// (`ro` relative to the centre). (-1, -1) when missed.
fn sphere(ro: vec3<f32>, rd: vec3<f32>, r: f32) -> vec2<f32> {
    let b = dot(ro, rd);
    let c = dot(ro, ro) - r * r;
    let h = b * b - c;
    if h < 0.0 {
        return vec2(-1.0, -1.0);
    }
    let s = sqrt(h);
    return vec2(-b - s, -b + s);
}

/// The [start, end) span of the view ray that lies inside the cloud layer.
fn layer_span(ro: vec3<f32>, rd: vec3<f32>) -> vec2<f32> {
    let r = length(ro);
    let rb = u.planet_radius + u.layer_bottom;
    let rt = u.planet_radius + u.layer_top;
    let tb = sphere(ro, rd, rb);
    let tt = sphere(ro, rd, rt);
    if r < rb {
        // below: the ground blocks anything curving over the horizon
        let tg = sphere(ro, rd, u.planet_radius);
        if tg.x > 0.0 {
            return vec2(-1.0, -1.0);
        }
        return vec2(tb.y, tt.y);
    } else if r < rt {
        var end = tt.y;
        if tb.x > 0.0 {
            end = tb.x;
        }
        return vec2(0.0, end);
    } else {
        if tt.y < 0.0 || tt.x < 0.0 {
            return vec2(-1.0, -1.0);
        }
        var end = tt.y;
        if tb.x > 0.0 {
            end = tb.x;
        }
        return vec2(tt.x, end);
    }
}

// ---------------------------------------------------------------------------
// lighting

fn hg(cos_theta: f32, g: f32) -> f32 {
    let g2 = g * g;
    return (1.0 - g2) / (4.0 * PI * pow(max(1.0 + g2 - 2.0 * g * cos_theta, 1e-4), 1.5));
}

/// Dual-lobe phase: a strong forward peak for silver linings plus a little
/// back-scatter so sun-behind-camera clouds don't go flat.
fn phase(cos_theta: f32, atten: f32) -> f32 {
    return mix(hg(cos_theta, -0.3 * atten), hg(cos_theta, 0.78 * atten), 0.7);
}

/// Optical depth towards the sun from `p`: a few short detailed steps, then
/// longer cheap ones, then one far sample for shadowing from distant towers.
fn sun_optical_depth(p: vec3<f32>, jitter: f32, detail: f32) -> f32 {
    let thickness = u.layer_top - u.layer_bottom;
    var tau = 0.0;
    var t = 0.0;
    var step = thickness * 0.03;
    for (var i = 0; i < 4; i++) {
        t += step * (0.4 + 0.6 * jitter + f32(i) * 0.5);
        let sp = p + u.sun_dir * t;
        let h = height_frac(sp);
        if h > 1.0 || h < 0.0 {
            break;
        }
        let w = sample_weather(sp);
        let d = density(sp, h, w, select(0.0, detail, i < 2));
        tau += d * u.extinction * step * (1.0 + f32(i) * 0.5);
        step *= 1.3;
    }
    let far = p + u.sun_dir * thickness * 0.75;
    let hf = height_frac(far);
    if hf >= 0.0 && hf <= 1.0 {
        tau += density(far, hf, sample_weather(far), 0.0) * u.extinction * thickness * 0.25;
    }
    return tau;
}

/// Sun light scattered towards the eye per unit extinction, for sun optical
/// depth `tau`: three single-scattering octaves (Hillaire / Schneider: each
/// scatters less, is shadowed less and has a wider lobe) for the directional
/// look and silver linings, plus a near-isotropic, softly shadowed
/// multiple-scattering floor that makes front-lit cloud white instead of grey.
fn sun_energy(tau: f32, cos_theta: f32) -> f32 {
    var sum = 0.0;
    var a = 1.0;
    var b = 1.0;
    var c = 1.0;
    for (var i = 0; i < 3; i++) {
        sum += a * exp(-tau * b) * phase(cos_theta, c);
        a *= 0.5;
        b *= 0.5;
        c *= 0.5;
    }
    sum += u.ms_albedo / PI * exp(-tau * 0.25) * (0.85 + 0.15 * cos_theta);
    return sum;
}

fn ign(px: vec2<f32>) -> f32 {
    return fract(52.9829189 * fract(0.06711056 * px.x + 0.00583715 * px.y));
}

// ---------------------------------------------------------------------------
// raymarch pass (reduced resolution)

struct RaymarchOut {
    @location(0) color: vec4<f32>,
    @location(1) data: vec2<f32>,
}

@fragment
fn fs_raymarch(in: VsOut) -> RaymarchOut {
    let low_px = vec2<i32>(in.position.xy);
    let div = i32(u.divisor);

    // Scene distance for this block of full-res pixels. Alternate between the
    // nearest and farthest depth in a checkerboard so the depth-aware
    // upsample always has both candidates nearby.
    var d_near = FAR_KM * 1000.0;
    var d_far = 0.0;
    for (var y = 0; y < div; y++) {
        for (var x = 0; x < div; x++) {
            let d = load_dist(low_px * div + vec2(x, y));
            d_near = min(d_near, d);
            d_far = max(d_far, d);
        }
    }
    var scene_dist = d_far;
    if ((low_px.x + low_px.y) & 1) == 1 {
        scene_dist = d_near;
    }

    var out: RaymarchOut;
    out.color = vec4(0.0);
    out.data = vec2(scene_dist / 1000.0, FAR_KM);

    let rd = ray_dir(in.uv);
    let ro = u.camera_pos - planet_centre();
    var span = layer_span(ro, rd);
    span.x = max(span.x, 0.0);
    span.y = min(span.y, min(scene_dist, u.max_distance));
    if span.y <= span.x {
        return out;
    }

    // rotate the jitter every frame; the temporal resolve averages it out
    let jitter = ign(in.position.xy + f32(u.frame % 64u) * 5.588238);
    let cos_theta = dot(rd, u.sun_dir);
    let sun = u.sun_color * u.sun_mult;

    var radiance = vec3(0.0);
    var transmittance = 1.0;
    var dist_sum = 0.0;
    var weight_sum = 0.0;

    var t = span.x;
    var in_cloud = false;
    var empty_run = 0;
    var i = 0u;
    loop {
        if i >= u.max_steps || t >= span.y || transmittance < 0.02 {
            break;
        }
        i += 1u;
        // Steps grow with distance, but never so short that the rest of the
        // span can't be covered with the steps left. Inside the layer a
        // level ray stays in it for 100+ km, mostly through thin cloud that
        // needs fine steps; without this floor the budget ran out partway and
        // everything beyond vanished, and where that happened shifted with
        // the wind and the camera (far clouds flickering out in stripes).
        // Above or below the layer the spans are short and it never kicks in.
        // The floor is geometric (each of the steps left a fixed fraction of
        // the distance so far, just reaching the end), so nearby cloud keeps
        // its fine steps. A few steps are held back for re-entering cloud.
        let t_eff = max(t, u.step_min);
        let n_left = max(f32(u.max_steps - i) - 8.0, 1.0);
        let floor = t_eff * (pow(max(span.y / t_eff, 1.0), 1.0 / n_left) - 1.0);
        let ds = max(clamp(t * u.step_scale, u.step_min, u.step_min * 24.0), floor);

        if !in_cloud {
            // coarse probe: skip empty air in big strides
            let stride = max(ds * 4.0, floor);
            // sample somewhere in the stride, differently every frame, so
            // the strides don't slice far clouds into stripes
            let ts = t + stride * jitter;
            let p = u.camera_pos + rd * ts;
            if ts < span.y && density(p, saturate(height_frac(p)), sample_weather(p), 0.0) > 0.0 {
                // march this stride again in fine steps from its start, so
                // the cloud's near edge isn't skipped by however far into
                // the stride the probe landed
                in_cloud = true;
                empty_run = 0;
                continue;
            }
            t += stride;
            continue;
        }

        // (the last step can stick out past the scene or the layer)
        let seg = min(ds, span.y - t);
        let ts = t + seg * jitter;
        let p = u.camera_pos + rd * ts;
        let h = saturate(height_frac(p));
        let w = sample_weather(p);
        // erosion detail is only resolvable with steps well under its size
        let detail = (1.0 - smoothstep(u.detail_fade_start, u.detail_fade_end, ts))
            * (1.0 - smoothstep(0.5, 1.0, ds / u.detail_scale));
        let d = density(p, h, w, detail);
        if d <= 0.002 {
            empty_run += 1;
            if empty_run > 5 {
                in_cloud = false;
            }
            t += ds;
            continue;
        }
        empty_run = 0;

        let sigma_t = d * u.extinction;
        let step_t = exp(-sigma_t * seg);
        let tau_sun = sun_optical_depth(p, jitter, detail);
        // "powder": dark creases where the sun first enters dense cloud
        let powder = 1.0 - 0.6 * exp(-sigma_t * 180.0) * saturate(cos_theta * 0.5 + 0.5);
        let sun_light = sun * (sun_energy(tau_sun, cos_theta) * powder);
        let ambient = mix(u.ambient_bottom, u.ambient_top, smoothstep(0.0, 0.8, h))
            * (u.ambient_mult * mix(1.0, 0.55, d));
        let s = (sun_light + ambient) * sigma_t;
        let s_int = (s - s * step_t) / sigma_t;
        radiance += transmittance * s_int;
        let wgt = transmittance * (1.0 - step_t);
        dist_sum += wgt * ts;
        weight_sum += wgt;
        transmittance *= step_t;
        t += ds;
    }

    // the march stops at 2% transmittance; treat that as opaque, otherwise
    // the sun disc (tens of thousands of times brighter than cloud) still
    // burns through thick cloud
    let exhausted = i >= u.max_steps && t < span.y && transmittance >= 0.02;
    transmittance = saturate((transmittance - 0.02) / 0.98);
    var alpha = 1.0 - transmittance;
    if u.debug == 2u {
        // red: the march ran out of steps before the end of its span
        out.color = vec4(select(0.0, 1.0, exhausted), alpha * 0.5, f32(i) / f32(u.max_steps), 1.0);
        return out;
    }
    if alpha <= 0.0005 {
        return out;
    }
    let mean_dist = dist_sum / max(weight_sum, 1e-5);
    // aerial perspective: distant clouds dissolve into the sky behind them,
    // and are fully gone before the march limit so the layer has no edge.
    // Nothing within fade_start is touched, so nearby cloud stays fully
    // opaque (the sun disc would show through even a few percent).
    let fade = exp(-max(mean_dist - u.fade_start, 0.0) / u.fade_distance)
        * (1.0 - smoothstep(0.7 * u.max_distance, u.max_distance, mean_dist));
    radiance *= u.exposure * fade;
    alpha *= fade;

    out.color = vec4(radiance, alpha);
    out.data = vec2(scene_dist / 1000.0, mean_dist / 1000.0);
    return out;
}

// ---------------------------------------------------------------------------
// temporal resolve pass (reduced resolution)
//
// Reprojects last frame's result through the cloud's mean distance, clamps it
// to the current frame's 3x3 neighbourhood (so moving planes don't ghost) and
// blends it with the new, differently jittered sample.

struct ResolveOut {
    @location(0) color: vec4<f32>,
    @location(1) data: vec2<f32>,
}

@fragment
fn fs_resolve(in: VsOut) -> ResolveOut {
    let px = vec2<i32>(in.position.xy);
    let lo_max = vec2<i32>(u.low_size) - 1;
    let cur = textureLoad(cloud_color, px, 0);
    let cur_data = textureLoad(cloud_data, px, 0).xy;

    var out: ResolveOut;
    out.data = cur_data;
    out.color = cur;
    if u.history_valid == 0u {
        return out;
    }

    // where was this bit of cloud last frame?
    let rd = ray_dir(in.uv);
    let world = u.camera_pos + rd * (cur_data.y * 1000.0);
    let prev_clip = u.prev_clip_from_world * vec4(world, 1.0);
    if prev_clip.w <= 0.0 {
        return out;
    }
    let prev_ndc = prev_clip.xy / prev_clip.w;
    let prev_uv = vec2(prev_ndc.x * 0.5 + 0.5, 0.5 - prev_ndc.y * 0.5);
    if any(prev_uv < vec2(0.0)) || any(prev_uv > vec2(1.0)) {
        return out;
    }
    let hist = textureSampleLevel(history_color, clamp_sampler, prev_uv, 0.0);
    let hist_data = textureSampleLevel(history_data, clamp_sampler, prev_uv, 0.0).xy;

    // neighbourhood clamp
    var mn = cur;
    var mx = cur;
    for (var y = -1; y <= 1; y++) {
        for (var x = -1; x <= 1; x++) {
            let c = textureLoad(cloud_color, clamp(px + vec2(x, y), vec2(0), lo_max), 0);
            mn = min(mn, c);
            mx = max(mx, c);
        }
    }
    let clamped = clamp(hist, mn, mx);

    // geometry moved in front of / away from this cloud: trust the new sample
    var blend = u.temporal_blend;
    let tol = max(cur_data.x * 0.1, 0.05);
    if abs(hist_data.x - cur_data.x) > tol {
        blend = max(blend, 0.5);
    }
    out.color = mix(clamped, cur, blend);
    return out;
}

// ---------------------------------------------------------------------------
// composite pass (full resolution, premultiplied blend)

@fragment
fn fs_composite(in: VsOut) -> @location(0) vec4<f32> {
    let full_px = vec2<i32>(in.position.xy);
    let d_full = load_dist(full_px) / 1000.0;

    // bilinear footprint in the low-res texture
    let lp = in.position.xy / f32(u.divisor) - 0.5;
    let base = floor(lp);
    let f = fract(lp);
    let lo_max = vec2<i32>(u.low_size) - 1;
    let c00 = clamp(vec2<i32>(base), vec2(0), lo_max);
    let c11 = clamp(vec2<i32>(base) + 1, vec2(0), lo_max);
    let c10 = vec2(c11.x, c00.y);
    let c01 = vec2(c00.x, c11.y);

    let k00 = textureLoad(cloud_color, c00, 0);
    let k10 = textureLoad(cloud_color, c10, 0);
    let k01 = textureLoad(cloud_color, c01, 0);
    let k11 = textureLoad(cloud_color, c11, 0);
    let z00 = textureLoad(cloud_data, c00, 0).x;
    let z10 = textureLoad(cloud_data, c10, 0).x;
    let z01 = textureLoad(cloud_data, c01, 0).x;
    let z11 = textureLoad(cloud_data, c11, 0).x;

    let w00 = (1.0 - f.x) * (1.0 - f.y);
    let w10 = f.x * (1.0 - f.y);
    let w01 = (1.0 - f.x) * f.y;
    let w11 = f.x * f.y;

    let tol = max(d_full * 0.08, 0.03);
    let e00 = abs(z00 - d_full);
    let e10 = abs(z10 - d_full);
    let e01 = abs(z01 - d_full);
    let e11 = abs(z11 - d_full);
    let e_max = max(max(e00, e10), max(e01, e11));

    var color: vec4<f32>;
    if e_max < tol {
        color = k00 * w00 + k10 * w10 + k01 * w01 + k11 * w11;
    } else {
        // depth discontinuity: take the neighbour whose scene depth matches
        color = k00;
        var best = e00;
        if e10 < best { best = e10; color = k10; }
        if e01 < best { best = e01; color = k01; }
        if e11 < best { best = e11; color = k11; }
    }

    // the bilinear and temporal blends leave a sliver of transmittance on
    // solid cloud; the sun disc is bright enough to shine through even that
    if color.a > 0.995 {
        color.a = 1.0;
    }

    if u.debug == 2u {
        return vec4(color.rgb, 1.0);
    }
    if u.debug == 1u {
        // visualise opacity
        return vec4(color.a, color.a, color.a, 1.0);
    }
    return color;
}
