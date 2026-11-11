//! Procedural mountain terrain using Rune Skovbo Johansen's erosion filter.
//!
//! Technique: <https://blog.runevision.com/2026/03/fast-and-gorgeous-erosion-filter.html>
//! (the "Advanced Terrain Erosion Filter" shadertoy, <https://www.shadertoy.com/view/wXcfWn>).
//! The erosion filter and Phacelle noise below are a CPU port of that work; the original is
//! copyright (c) 2025 Rune Skovbo Johansen and licensed under the Mozilla Public License 2.0,
//! <https://mozilla.org/MPL/2.0/>.
//!
//! Overview:
//! - A fractal gradient noise heightfield (with analytic derivatives) forms the mountains.
//!   A meandering valley is carved along the Z axis through the player spawn (0, 6000), and
//!   a hard cap keeps the ground within 1.5 km of the spawn below 900 m.
//! - The erosion filter then layers octaves of "Phacelle" noise on top: stripes aligned with
//!   the downhill direction of everything computed so far, so each finer octave of gullies
//!   branches off the slopes of the coarser ones. Gullies fade out towards a target value
//!   (ridge on peaks, crease in valleys) where the terrain is flat.
//! - The result is baked into two height grids (64 m out to ±25.6 km, 256 m out to ±76.8 km)
//!   which become chunked meshes and are kept in the [`GeneratedTerrain`] resource for O(1)
//!   CPU height queries. The shading detail comes from baked colour + normal map textures at
//!   a higher resolution than the meshes (colour from height, slope, the filter's ridge map,
//!   and a baked cavity term).
//!
//! Generation runs synchronously in a `Startup` system (multi-threaded with scoped std
//! threads, ~1.5 s), so `GeneratedTerrain` exists from the first `Update` onwards.
//! Valley floors sit at roughly 0..400 m, most peaks at 2000..3500 m (max ~3.9 km).
//!
//! Debug: `TERRAIN_DEBUG_CAM=<n>` overrides the `Camera3d` transform with one of a few fixed
//! viewpoints (or `x,y,z,tx,ty,tz`) for looking at the terrain.

use std::time::Instant;

use bevy::{
    asset::RenderAssetUsages,
    image::{ImageAddressMode, ImageFilterMode, ImageSampler, ImageSamplerDescriptor},
    mesh::{Indices, PrimitiveTopology},
    prelude::*,
    render::render_resource::{Extent3d, TextureDimension, TextureFormat},
};

pub struct TerrainGenPlugin;

impl Plugin for TerrainGenPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, generate_terrain);
        if let Some(cam) = std::env::var("TERRAIN_DEBUG_CAM")
            .ok()
            .and_then(|s| DebugCam::from_env(&s))
        {
            app.insert_resource(cam)
                .add_systems(PostUpdate, debug_cam.before(TransformSystems::Propagate));
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Parameters
// ---------------------------------------------------------------------------------------------

/// Metres per unit of "noise space". All the filter parameters below are expressed in that
/// space (they're the shadertoy's defaults), so this sets the overall horizontal scale.
const WORLD_SCALE: f32 = 30_000.0;
/// Vertical scale relative to horizontal. 1.0 keeps the filter's native proportions.
const VERTICAL_SCALE: f32 = 0.9;
/// Filter-space height mapped to y = 0 (tuned so valley floors sit around 0..400 m).
const SEA_LEVEL: f32 = 0.41;

/// The player spawn. A valley runs through it along Z (the direction of play), opening into
/// a basin around the spawn point, so the opening engagement happens between mountains
/// rather than inside one.
const SPAWN: Vec2 = Vec2::new(0.0, 6000.0);
/// The spawn valley's centre line (world x/z).
const SPAWN_VALLEY: [Vec2; 2] = [Vec2::new(0.0, 11000.0), Vec2::new(0.0, -9000.0)];
/// Half-width of the spawn valley's floor, and the distance over which it rises into the mountains.
const SPAWN_VALLEY_FLOOR: f32 = 700.0;
const SPAWN_VALLEY_BLEND: f32 = 5000.0;
/// How much low-frequency noise widens the valley (metres).
const SPAWN_VALLEY_WOBBLE: f32 = 1500.0;
/// Hard guarantee: within `SPAWN_CLEAR_RADIUS` of `SPAWN` terrain stays below `SPAWN_CLEAR_HEIGHT`.
const SPAWN_CLEAR_RADIUS: f32 = 1500.0;
const SPAWN_CLEAR_HEIGHT: f32 = 850.0;

// Base terrain (fractal gradient noise).
const TERRAIN_FREQ: f32 = 3.0;
const TERRAIN_AMP: f32 = 0.16;
const TERRAIN_OCTAVES: u32 = 3;
const TERRAIN_GAIN: f32 = 0.1;
const TERRAIN_LACUNARITY: f32 = 2.0;
/// Spawn valley floor level, in units of the normalised (pre-amplitude) noise.
const VALLEY_LEVEL: f32 = -0.22;
/// Slope of the spawn valley floor along Z (normalised noise units per noise-space unit).
const VALLEY_TILT: f32 = -0.15;
/// (constant offset, -fadeTarget mix) applied scaled by the erosion magnitude.
const HEIGHT_OFFSET: Vec2 = Vec2::new(-0.65, 0.0);

// Erosion filter.
const EROSION_SCALE: f32 = 0.15;
const EROSION_STRENGTH: f32 = 0.22;
const EROSION_GULLY_WEIGHT: f32 = 0.5;
const EROSION_DETAIL: f32 = 1.5;
/// (ridge rounding, crease rounding, input rounding multiplier, per-octave rounding multiplier)
const EROSION_ROUNDING: Vec4 = Vec4::new(0.1, 0.0, 0.1, 2.0);
/// (initial onset, octave onset, ridge-map initial onset, ridge-map octave onset)
const EROSION_ONSET: Vec4 = Vec4::new(1.25, 1.25, 2.8, 1.5);
/// (assumed slope value, mix between actual and assumed slope)
const EROSION_ASSUMED_SLOPE: Vec2 = Vec2::new(0.7, 1.0);
const EROSION_CELL_SCALE: f32 = 0.7;
const EROSION_OCTAVES: u32 = 5;
const EROSION_GAIN: f32 = 0.5;
const EROSION_LACUNARITY: f32 = 2.0;
const EROSION_NORMALIZATION: f32 = 0.5;

const SEED: u32 = 1337;

// Grids. The fine grid's extent is a whole number of coarse cells so their vertices line up.
const FINE_SPACING: f32 = 64.0;
const FINE_CELLS: usize = 800; // ±25.6 km
const COARSE_SPACING: f32 = 256.0;
const COARSE_CELLS: usize = 600; // ±76.8 km
const CHUNK_CELLS: usize = 100;
/// Colour / normal map resolution for each grid (the erosion detail lives in these).
const FINE_TEXTURE_RES: usize = 2048; // 25 m per texel
const COARSE_TEXTURE_RES: usize = 1024; // 150 m per texel
const SKIRT_DEPTH: f32 = 400.0;

// ---------------------------------------------------------------------------------------------
// Public resource
// ---------------------------------------------------------------------------------------------

/// The generated heightfield, kept on the CPU for collision queries.
#[derive(Resource)]
pub struct GeneratedTerrain {
    fine: HeightGrid,
    coarse: HeightGrid,
}

impl GeneratedTerrain {
    /// Terrain surface height at world (x, z), bilinearly interpolated, metres. Fast (O(1)).
    /// Uses the same height grids as the rendered mesh (64 m spacing within ±25.6 km, 256 m
    /// beyond), so it matches the mesh at its vertices; between them bilinear interpolation
    /// differs slightly from the mesh's triangles. Outside the generated area (±76.8 km) the
    /// edge height is extended.
    pub fn height_at(&self, x: f32, z: f32) -> f32 {
        if let Some(h) = self.fine.sample_inside(x, z) {
            h
        } else {
            self.coarse.sample_clamped(x, z)
        }
    }
}

struct HeightGrid {
    /// World position of sample (0, 0).
    origin: Vec2,
    spacing: f32,
    inv_spacing: f32,
    /// Samples per side (cells + 1).
    n: usize,
    heights: Vec<f32>,
}

impl HeightGrid {
    #[inline]
    fn get(&self, i: usize, j: usize) -> f32 {
        self.heights[j * self.n + i]
    }

    #[inline]
    fn bilinear(&self, fx: f32, fz: f32) -> f32 {
        let max = (self.n - 2) as f32;
        let fx = fx.clamp(0.0, max + 1.0);
        let fz = fz.clamp(0.0, max + 1.0);
        let i = (fx.floor()).min(max) as usize;
        let j = (fz.floor()).min(max) as usize;
        let tx = fx - i as f32;
        let tz = fz - j as f32;
        let row0 = j * self.n + i;
        let row1 = row0 + self.n;
        let h00 = self.heights[row0];
        let h10 = self.heights[row0 + 1];
        let h01 = self.heights[row1];
        let h11 = self.heights[row1 + 1];
        let a = h00 + (h10 - h00) * tx;
        let b = h01 + (h11 - h01) * tx;
        a + (b - a) * tz
    }

    #[inline]
    fn sample_inside(&self, x: f32, z: f32) -> Option<f32> {
        let fx = (x - self.origin.x) * self.inv_spacing;
        let fz = (z - self.origin.y) * self.inv_spacing;
        let max = (self.n - 1) as f32;
        if (0.0..=max).contains(&fx) && (0.0..=max).contains(&fz) {
            Some(self.bilinear(fx, fz))
        } else {
            None
        }
    }

    #[inline]
    fn sample_clamped(&self, x: f32, z: f32) -> f32 {
        let fx = (x - self.origin.x) * self.inv_spacing;
        let fz = (z - self.origin.y) * self.inv_spacing;
        self.bilinear(fx, fz)
    }

    fn world_pos(&self, i: usize, j: usize) -> Vec2 {
        self.origin + Vec2::new(i as f32, j as f32) * self.spacing
    }
}

// ---------------------------------------------------------------------------------------------
// Noise
// ---------------------------------------------------------------------------------------------

/// Integer hash of a lattice point to a vector in [-1, 1]^2. (The shadertoy uses a float sine-free
/// hash which loses precision at the coordinate magnitudes we need, so this uses PCG instead.)
#[inline]
fn hash2(x: i32, y: i32, seed: u32) -> Vec2 {
    // pcg2d, Jarzynski & Olano 2020
    let mut vx = (x as u32).wrapping_mul(1664525).wrapping_add(1013904223) ^ seed;
    let mut vy = (y as u32).wrapping_mul(1664525).wrapping_add(1013904223);
    vx = vx.wrapping_add(vy.wrapping_mul(1664525));
    vy = vy.wrapping_add(vx.wrapping_mul(1664525));
    vx ^= vx >> 16;
    vy ^= vy >> 16;
    vx = vx.wrapping_add(vy.wrapping_mul(1664525));
    vy = vy.wrapping_add(vx.wrapping_mul(1664525));
    vx ^= vx >> 16;
    vy ^= vy >> 16;
    Vec2::new(
        (vx >> 8) as f32 * (2.0 / 16_777_216.0) - 1.0,
        (vy >> 8) as f32 * (2.0 / 16_777_216.0) - 1.0,
    )
}

/// Gradient noise with analytic derivatives: returns (value, d/dx, d/dy).
/// From <https://www.shadertoy.com/view/XdXBRH> (Inigo Quilez).
#[inline]
fn noised(p: Vec2, seed: u32) -> Vec3 {
    let i = p.floor();
    let f = p - i;
    let (ix, iy) = (i.x as i32, i.y as i32);
    let u = f * f * f * (f * (f * 6.0 - 15.0) + 10.0);
    let du = 30.0 * f * f * (f * (f - 2.0) + 1.0);

    let ga = hash2(ix, iy, seed);
    let gb = hash2(ix + 1, iy, seed);
    let gc = hash2(ix, iy + 1, seed);
    let gd = hash2(ix + 1, iy + 1, seed);

    let va = ga.dot(f);
    let vb = gb.dot(f - Vec2::new(1.0, 0.0));
    let vc = gc.dot(f - Vec2::new(0.0, 1.0));
    let vd = gd.dot(f - Vec2::new(1.0, 1.0));

    let k = va - vb - vc + vd;
    let value = va + u.x * (vb - va) + u.y * (vc - va) + u.x * u.y * k;
    let deriv = ga
        + u.x * (gb - ga)
        + u.y * (gc - ga)
        + u.x * u.y * (ga - gb - gc + gd)
        + du * (Vec2::new(u.y, u.x) * k + Vec2::new(vb, vc) - va);
    Vec3::new(value, deriv.x, deriv.y)
}

fn fractal_noise(p: Vec2, freq: f32, octaves: u32, lacunarity: f32, gain: f32, seed: u32) -> Vec3 {
    let mut n = Vec3::ZERO;
    let mut nf = freq;
    let mut na = 1.0;
    for o in 0..octaves {
        let octave = noised(p * nf, seed.wrapping_add(o * 7919));
        n.x += octave.x * na;
        n.y += octave.y * na * nf;
        n.z += octave.z * na * nf;
        na *= gain;
        nf *= lacunarity;
    }
    n
}

/// Simple Phacelle noise: a stripe pattern aligned with `norm_dir`, made by blending cosine and
/// sine waves from a 4x4 neighbourhood of jittered cells. Returns
/// (cos, sin, side_dir.x, side_dir.y); `sin * side_dir` is the derivative of `cos`.
#[inline]
fn phacelle_noise(
    p: Vec2,
    norm_dir: Vec2,
    freq: f32,
    offset: f32,
    normalization: f32,
    seed: u32,
) -> Vec4 {
    use std::f32::consts::TAU;
    let side_dir = Vec2::new(-norm_dir.y, norm_dir.x) * freq * TAU;
    let offset = offset * TAU;

    let p_int = p.floor();
    let p_frac = p - p_int;
    let (ix, iy) = (p_int.x as i32, p_int.y as i32);
    let mut phase_dir = Vec2::ZERO;
    let mut weight_sum = 0.0;

    for j in -1..=2 {
        for i in -1..=2 {
            let grid_offset = Vec2::new(i as f32, j as f32);
            let random_offset = hash2(ix + i, iy + j, seed) * 0.5;
            let v = p_frac - grid_offset - random_offset;
            let sqr_dist = v.length_squared();
            // exp(-2 * 2.25) == 0.01111: weight is zero beyond distance 1.5, skip the trig.
            if sqr_dist >= 2.25 {
                continue;
            }
            let weight = ((-sqr_dist * 2.0).exp() - 0.01111).max(0.0);
            weight_sum += weight;
            let wave_input = v.dot(side_dir) + offset;
            let (s, c) = wave_input.sin_cos();
            phase_dir += Vec2::new(c, s) * weight;
        }
    }

    let interpolated = phase_dir / weight_sum.max(1e-6);
    let magnitude = interpolated.length().max(1.0 - normalization);
    let n = interpolated / magnitude;
    Vec4::new(n.x, n.y, side_dir.x, side_dir.y)
}

#[inline]
fn ease_out(t: f32) -> f32 {
    let v = 1.0 - t.clamp(0.0, 1.0);
    1.0 - v * v
}

#[inline]
fn smooth_start(t: f32, smoothing: f32) -> f32 {
    if t >= smoothing {
        t - 0.5 * smoothing
    } else {
        0.5 * t * t / smoothing
    }
}

#[inline]
fn pow_inv(t: f32, power: f32) -> f32 {
    1.0 - (1.0 - t.clamp(0.0, 1.0)).powf(power)
}

#[inline]
fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Derivative of `smoothstep` with respect to x.
#[inline]
fn smoothstep_deriv(e0: f32, e1: f32, x: f32) -> f32 {
    let t = (x - e0) / (e1 - e0);
    if !(0.0..=1.0).contains(&t) {
        return 0.0;
    }
    6.0 * t * (1.0 - t) / (e1 - e0)
}

struct ErosionOutput {
    /// (height delta, slope delta x, slope delta y)
    delta: Vec3,
    magnitude: f32,
    /// -1 on creases, 1 on ridges.
    ridge_map: f32,
}

/// The erosion filter ("Advanced Terrain Erosion Filter", R. S. Johansen).
///
/// `octaves` may be fractional: the last octave's contribution is scaled by the fractional
/// part. We use that to drop the finest gullies smoothly with distance where the mesh is too
/// coarse to show them (pure LOD, it doesn't change the look up close).
fn erosion_filter(
    p: Vec2,
    mut height_and_slope: Vec3,
    mut fade_target: f32,
    octaves: f32,
) -> ErosionOutput {
    let rounding = EROSION_ROUNDING;
    let onset = EROSION_ONSET;
    let assumed_slope = EROSION_ASSUMED_SLOPE;
    let gully_weight = EROSION_GULLY_WEIGHT;

    let mut strength = EROSION_STRENGTH * EROSION_SCALE;
    fade_target = fade_target.clamp(-1.0, 1.0);

    let input = height_and_slope;
    let mut freq = 1.0 / (EROSION_SCALE * EROSION_CELL_SCALE);
    let slope = Vec2::new(height_and_slope.y, height_and_slope.z);
    let slope_length = slope.length().max(1e-10);
    let mut magnitude = 0.0;
    let mut rounding_mult = 1.0;

    let rounding_for_input =
        lerp(rounding.y, rounding.x, (fade_target + 0.5).clamp(0.0, 1.0)) * rounding.z;
    // The combined accumulating mask, based first on initial slope, and later on the slope of
    // each octave too.
    let mut combi_mask = ease_out(smooth_start(
        slope_length * onset.x,
        rounding_for_input * onset.x,
    ));

    let mut ridge_map_combi_mask = ease_out(slope_length * onset.z);
    let mut ridge_map_fade_target = fade_target;

    // Initial slope used for gully directions: a mix of the actual and an assumed slope.
    let mut gully_slope = slope.lerp(slope / slope_length * assumed_slope.x, assumed_slope.y);

    let full_octaves = octaves.ceil() as u32;
    for i in 0..full_octaves.min(EROSION_OCTAVES) {
        let octave_weight = (octaves - i as f32).clamp(0.0, 1.0);
        let ph = phacelle_noise(
            p * freq,
            gully_slope.normalize_or_zero(),
            EROSION_CELL_SCALE,
            0.25,
            EROSION_NORMALIZATION,
            SEED.wrapping_add(1000 + i * 31),
        );
        // Multiply with freq since p was multiplied with freq; negate since slope directions
        // point down.
        let dir = Vec2::new(ph.z, ph.w) * -freq;
        let sloping = ph.y.abs();

        // Non-masked, normalised slope feeds the gully directions of subsequent octaves
        // ("straight gullies": sign of the sine instead of the sine itself).
        gully_slope += ph.y.signum() * dir * strength * gully_weight * octave_weight;

        // Gullies: height offset (-1..1) and its derivative.
        let gullies = Vec3::new(ph.x, ph.y * dir.x, ph.y * dir.y);
        // Fade gullies towards fade_target based on combi_mask.
        let faded = Vec3::new(fade_target, 0.0, 0.0).lerp(gullies * gully_weight, combi_mask);

        height_and_slope += faded * strength * octave_weight;
        magnitude += strength * octave_weight;

        fade_target = faded.x;

        let rounding_for_octave =
            lerp(rounding.y, rounding.x, (ph.x + 0.5).clamp(0.0, 1.0)) * rounding_mult;
        let new_mask = ease_out(smooth_start(
            sloping * onset.y,
            rounding_for_octave * onset.y,
        ));
        combi_mask = pow_inv(combi_mask, EROSION_DETAIL) * new_mask;

        ridge_map_fade_target = lerp(ridge_map_fade_target, gullies.x, ridge_map_combi_mask);
        ridge_map_combi_mask *= ease_out(sloping * onset.w);

        strength *= EROSION_GAIN;
        freq *= EROSION_LACUNARITY;
        rounding_mult *= rounding.w;
    }

    ErosionOutput {
        delta: height_and_slope - input,
        magnitude,
        ridge_map: ridge_map_fade_target * (1.0 - ridge_map_combi_mask),
    }
}

#[inline]
fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

// ---------------------------------------------------------------------------------------------
// Terrain function
// ---------------------------------------------------------------------------------------------

/// Distance (metres) from the spawn valley's (meandering) centre line, with its gradient.
fn spawn_valley_distance(x: f32, z: f32) -> (f32, Vec2) {
    // The valley meanders sideways; the meander is zero at the spawn point.
    const MEANDER: f32 = 2200.0;
    const MEANDER_LEN: f32 = 6500.0;
    let phase = (z - SPAWN.y) / MEANDER_LEN;
    let shift = MEANDER * phase.sin();
    let dshift = MEANDER / MEANDER_LEN * phase.cos();

    let [a, b] = SPAWN_VALLEY;
    let p = Vec2::new(x - shift, z);
    let ab = b - a;
    let t = ((p - a).dot(ab) / ab.length_squared()).clamp(0.0, 1.0);
    let d = p - (a + ab * t);
    let len = d.length();
    if len <= 1e-3 {
        return (0.0, Vec2::ZERO);
    }
    let dir = d / len;
    // Chain rule through the shift (x' = x - shift(z)).
    (len, Vec2::new(dir.x, dir.y - dir.x * dshift))
}

/// Polynomial smooth minimum (always <= min(a, b)).
fn smooth_min(a: f32, b: f32, k: f32) -> f32 {
    let h = (k - (a - b).abs()).max(0.0) / k;
    a.min(b) - h * h * k * 0.25
}

struct TerrainSample {
    /// World height, metres.
    height: f32,
    /// Erosion ridge map: -1 in creases, 1 on ridges.
    ridge_map: f32,
}

/// Evaluate the terrain at world (x, z). `octaves` is the (fractional) number of erosion octaves.
fn sample_terrain(x: f32, z: f32, octaves: f32) -> TerrainSample {
    let p = Vec2::new(x, z) / WORLD_SCALE;

    // Base fractal noise, normalised (pre-amplitude) units, derivatives w.r.t. p.
    let mountains = fractal_noise(
        p,
        TERRAIN_FREQ,
        TERRAIN_OCTAVES,
        TERRAIN_LACUNARITY,
        TERRAIN_GAIN,
        SEED,
    );

    // Spawn valley mask: 0 on the valley floor, 1 in the mountains, with a wobbly edge.
    // Everything carries derivatives, since the erosion filter is driven by the slope.
    let (dist, ddist) = spawn_valley_distance(x, z);
    let wobble = noised(p * 6.0, SEED ^ 0xBEEF);
    let wobble_amt = wobble.x * 0.5 + 0.5;
    let (wobble_amt, dwobble) = if (0.0..=1.0).contains(&wobble_amt) {
        (wobble_amt, Vec2::new(wobble.y, wobble.z) * 0.5)
    } else {
        (wobble_amt.clamp(0.0, 1.0), Vec2::ZERO)
    };
    let edge = dist - wobble_amt * SPAWN_VALLEY_WOBBLE;
    let dedge = ddist * WORLD_SCALE - dwobble * 6.0 * SPAWN_VALLEY_WOBBLE;
    let (e0, e1) = (SPAWN_VALLEY_FLOOR, SPAWN_VALLEY_FLOOR + SPAWN_VALLEY_BLEND);
    let m = smoothstep(e0, e1, edge);
    let dm = smoothstep_deriv(e0, e1, edge) * dedge;

    // The valley floor itself gently undulates, so water still has somewhere to flow.
    // It also tilts gently (down towards +Z), like a river valley.
    let floor = fractal_noise(p, 2.0, 2, 2.0, 0.5, SEED ^ 0xF100) * 0.08
        + Vec3::new(VALLEY_LEVEL + p.y * VALLEY_TILT, 0.0, VALLEY_TILT);

    // shaped = floor + m * (mountains - floor)
    let rel = mountains - floor;
    let shaped = Vec3::new(
        floor.x + m * rel.x,
        floor.y + m * rel.y + dm.x * rel.x,
        floor.z + m * rel.z + dm.y * rel.x,
    );

    let mut n = shaped * TERRAIN_AMP;
    // Fade target: -1 at valleys, 1 at peaks (overshooting is fine, it's clamped).
    let fade_target = (n.x / (TERRAIN_AMP * 0.6)).clamp(-1.0, 1.0);
    // Heights from [-1, 1] to [0, 1] (as in the reference).
    n = n * 0.5 + Vec3::new(0.5, 0.0, 0.0);

    let e = erosion_filter(p, n, fade_target, octaves);
    let offset = lerp(HEIGHT_OFFSET.x, -fade_target, HEIGHT_OFFSET.y) * e.magnitude;
    let eroded = n.x + e.delta.x + offset;

    let mut height = (eroded - SEA_LEVEL) * WORLD_SCALE * VERTICAL_SCALE;

    // Squash the deepest basins into broad, nearly flat floors that bottom out just below 0.
    const SOFT_FLOOR: f32 = 120.0;
    const SOFT_FLOOR_RANGE: f32 = 180.0;
    if height < SOFT_FLOOR {
        height = SOFT_FLOOR
            - SOFT_FLOOR_RANGE * (1.0 - ((height - SOFT_FLOOR) / SOFT_FLOOR_RANGE).exp());
    }

    // Hard spawn guarantee: a smooth cap that is SPAWN_CLEAR_HEIGHT within the clear radius
    // and rises steeply beyond it (smooth_min never exceeds the cap).
    let spawn_dist = Vec2::new(x, z).distance(SPAWN);
    let cap = SPAWN_CLEAR_HEIGHT + (spawn_dist - SPAWN_CLEAR_RADIUS).max(0.0) * 0.6;
    height = smooth_min(height, cap, 200.0);

    TerrainSample {
        height,
        ridge_map: e.ridge_map,
    }
}

/// Erosion octave count used at a world position (fewer far away, where the mesh is coarse).
fn octaves_at(x: f32, z: f32) -> f32 {
    let r = x.abs().max(z.abs());
    let fine_edge = FINE_SPACING * FINE_CELLS as f32 * 0.5;
    EROSION_OCTAVES as f32
        - smoothstep(fine_edge * 0.75, fine_edge, r)
        - 0.5 * smoothstep(fine_edge * 1.5, fine_edge * 2.8, r)
}

// ---------------------------------------------------------------------------------------------
// Generation
// ---------------------------------------------------------------------------------------------

/// Evaluates `f(i, j)` for every cell of a `width x height` lattice, spread over all cores.
/// Returns the results row-major.
fn par_map<T: Send + Clone + Default>(
    width: usize,
    height: usize,
    f: impl Fn(usize, usize) -> T + Sync,
) -> Vec<T> {
    let mut out = vec![T::default(); width * height];
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    let rows_per_band = height.div_ceil(threads * 4).max(1);
    // Hand out bands of rows to worker threads; each band is a disjoint slice of `out`.
    let bands = std::sync::Mutex::new(out.chunks_mut(rows_per_band * width).enumerate());
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                loop {
                    let Some((band, slice)) = bands.lock().unwrap().next() else {
                        break;
                    };
                    let j0 = band * rows_per_band;
                    for (k, v) in slice.iter_mut().enumerate() {
                        *v = f(k % width, j0 + k / width);
                    }
                }
            });
        }
    });
    out
}

/// Bakes the height grid used for both the mesh and collision queries.
fn bake_grid(spacing: f32, cells: usize) -> HeightGrid {
    let n = cells + 1;
    let origin = Vec2::splat(-spacing * cells as f32 * 0.5);
    let heights = par_map(n, n, |i, j| {
        let x = origin.x + i as f32 * spacing;
        let z = origin.y + j as f32 * spacing;
        sample_terrain(x, z, octaves_at(x, z)).height
    });
    HeightGrid {
        origin,
        spacing,
        inv_spacing: 1.0 / spacing,
        n,
        heights,
    }
}

fn srgb(r: f32, g: f32, b: f32) -> Vec3 {
    let c = Color::srgb(r, g, b).to_linear();
    Vec3::new(c.red, c.green, c.blue)
}

/// Surface colour (linear RGB) from height, normal and the erosion ridge map.
fn terrain_color(h: f32, normal: Vec3, ridge: f32, jitter: f32) -> Vec3 {
    let meadow = srgb(0.42, 0.47, 0.22);
    let dry_meadow = srgb(0.52, 0.50, 0.30);
    let forest = srgb(0.17, 0.25, 0.11);
    let alpine = srgb(0.46, 0.44, 0.30);
    let rock = srgb(0.52, 0.46, 0.36);
    let dark_rock = srgb(0.31, 0.27, 0.22);
    let snow = srgb(0.94, 0.95, 0.98);

    let flatness = normal.y; // 1 = flat
    let ridge = ridge.clamp(-1.0, 1.0);
    let crease = (-ridge).max(0.0);

    // Valley floors are a patchwork of meadow and woods, slopes are forested up to the tree
    // line (which reaches higher up the gullies, where the drainage is), then alpine grass.
    let tree_line = 1650.0 + jitter * 250.0 + crease * 250.0 - ridge.max(0.0) * 200.0;
    let woods = smoothstep(0.15, 0.45, jitter + crease * 0.3);
    let forest_amt = smoothstep(0.985, 0.94 - crease * 0.05, flatness).max(woods)
        * smoothstep(tree_line + 150.0, tree_line - 150.0, h);
    let floor_col = meadow.lerp(dry_meadow, smoothstep(-0.1, -0.5, jitter));
    let mut c = floor_col.lerp(forest, forest_amt);
    let alpine_amt = smoothstep(tree_line - 100.0, tree_line + 250.0, h);
    c = c.lerp(alpine, alpine_amt);

    // Rock on steep slopes, more exposed on ridges and higher up, darker in eroded creases.
    let steep_start = 0.70 + ridge * 0.06 + jitter * 0.04 + smoothstep(1500.0, 3000.0, h) * 0.08;
    let rock_amt = smoothstep(steep_start, steep_start - 0.10, flatness);
    let rock_col = rock.lerp(dark_rock, crease * 0.8 + jitter.max(0.0) * 0.2);
    c = c.lerp(rock_col, rock_amt);

    // Snow on high, not-too-steep ground; it lingers longer in creases.
    let snow_line = 2350.0 + jitter * 200.0 - crease * 200.0;
    let snow_amt = smoothstep(snow_line - 100.0, snow_line + 250.0, h)
        * smoothstep(0.55 - smoothstep(2500.0, 3500.0, h) * 0.15, 0.72, flatness);
    c.lerp(snow, snow_amt)
}

/// Builds a full mip chain by 2x2 box filtering. `renormalize` is for normal maps.
fn mip_chain(level0: Vec<Vec4>, res: usize, renormalize: bool) -> Vec<Vec<Vec4>> {
    let mut levels = vec![level0];
    let mut size = res;
    while size > 1 {
        let half = size / 2;
        let prev = levels.last().unwrap();
        let next = (0..half * half)
            .map(|k| {
                let (i, j) = (k % half * 2, k / half * 2);
                let v = prev[j * size + i]
                    + prev[j * size + i + 1]
                    + prev[(j + 1) * size + i]
                    + prev[(j + 1) * size + i + 1];
                if renormalize {
                    v.truncate().normalize_or(Vec3::Z).extend(1.0)
                } else {
                    v * 0.25
                }
            })
            .collect();
        levels.push(next);
        size = half;
    }
    levels
}

fn make_image(
    levels: &[Vec<Vec4>],
    res: usize,
    format: TextureFormat,
    encode: impl Fn(Vec4) -> [u8; 4],
) -> Image {
    let mut data = Vec::with_capacity(res * res * 4 * 4 / 3 + 16);
    for level in levels {
        for &v in level {
            data.extend_from_slice(&encode(v));
        }
    }
    let size = Extent3d {
        width: res as u32,
        height: res as u32,
        depth_or_array_layers: 1,
    };
    let mut image = Image::new_uninit(
        size,
        TextureDimension::D2,
        format,
        RenderAssetUsages::RENDER_WORLD,
    );
    image.data = Some(data);
    image.texture_descriptor.mip_level_count = levels.len() as u32;
    image.sampler = ImageSampler::Descriptor(ImageSamplerDescriptor {
        address_mode_u: ImageAddressMode::ClampToEdge,
        address_mode_v: ImageAddressMode::ClampToEdge,
        mag_filter: ImageFilterMode::Linear,
        min_filter: ImageFilterMode::Linear,
        mipmap_filter: ImageFilterMode::Linear,
        anisotropy_clamp: 16,
        ..default()
    });
    image
}

/// Bakes a colour texture and a normal map covering the square `origin .. origin + extent`.
///
/// These carry the erosion detail at a much higher resolution than the mesh: the shading of
/// the gullies and ridges comes from here, the mesh only provides the silhouette. The normal
/// map is in a tangent space where T = +X, B = +Z and N = +Y (the mesh supplies exactly those
/// as its normals and tangents), so it holds world-space normals.
fn bake_textures(origin: Vec2, extent: f32, res: usize) -> (Image, Image) {
    let step = extent / res as f32;
    // Texel-centred samples with a one texel border for the finite differences.
    let n = res + 2;
    let samples = par_map(n, n, |i, j| {
        let x = origin.x + (i as f32 - 0.5) * step;
        let z = origin.y + (j as f32 - 0.5) * step;
        let t = sample_terrain(x, z, EROSION_OCTAVES as f32);
        (t.height, t.ridge_map)
    });
    let texels = par_map(res, res, |i, j| {
        let k = (j + 1) * n + i + 1;
        let dx = (samples[k + 1].0 - samples[k - 1].0) / (2.0 * step);
        let dz = (samples[k + n].0 - samples[k - n].0) / (2.0 * step);
        let normal = Vec3::new(-dx, 1.0, -dz).normalize();
        let xz = origin + (Vec2::new(i as f32, j as f32) + 0.5) * step;
        let jitter = noised(xz / 900.0, 77).x + 0.5 * noised(xz / 310.0, 78).x;
        let mut color = terrain_color(samples[k].0, normal, samples[k].1, jitter);

        // Cheap baked cavity / ambient occlusion: compare the height with its surroundings at a
        // few radii. Gully floors (concave) get darker, ridge crests (convex) a little lighter,
        // which makes the dendritic erosion pattern read even under a high sun.
        let h_at = |di: isize, dj: isize| {
            let si = (i as isize + 1 + di).clamp(0, n as isize - 1) as usize;
            let sj = (j as isize + 1 + dj).clamp(0, n as isize - 1) as usize;
            samples[sj * n + si].0
        };
        let mut cavity = 0.0;
        for r in [2isize, 5, 12] {
            let mean = (h_at(r, 0) + h_at(-r, 0) + h_at(0, r) + h_at(0, -r)) * 0.25;
            cavity += (mean - samples[k].0) / (r as f32 * step) / 3.0;
        }
        color *= 1.0 - smoothstep(0.0, 0.25, cavity) * 0.55 + smoothstep(0.0, 0.25, -cavity) * 0.2;

        (
            color.extend(1.0),
            Vec4::new(normal.x, normal.z, normal.y, 1.0),
        )
    });
    let (colors, normals): (Vec<Vec4>, Vec<Vec4>) = texels.into_iter().unzip();

    let to_u8 = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
    let color_image = make_image(
        &mip_chain(colors, res, false),
        res,
        TextureFormat::Rgba8UnormSrgb,
        |v| {
            let s = Srgba::from(LinearRgba::rgb(v.x, v.y, v.z));
            [to_u8(s.red), to_u8(s.green), to_u8(s.blue), 255]
        },
    );
    let normal_image = make_image(
        &mip_chain(normals, res, true),
        res,
        TextureFormat::Rgba8Unorm,
        |v| {
            [
                to_u8(v.x * 0.5 + 0.5),
                to_u8(v.y * 0.5 + 0.5),
                to_u8(v.z * 0.5 + 0.5),
                255,
            ]
        },
    );
    (color_image, normal_image)
}

/// Builds one chunk mesh covering cells [i0, i1) x [j0, j1) of `grid`, with UVs mapping the
/// whole grid to 0..1. Adds skirts along edges that lie on the grid's outer border (to hide
/// cracks between LOD levels).
fn build_chunk(
    grid: &HeightGrid,
    i0: usize,
    i1: usize,
    j0: usize,
    j1: usize,
    triangles: &mut usize,
) -> Mesh {
    let w = i1 - i0 + 1;
    let hgt = j1 - j0 + 1;
    let uv_scale = 1.0 / (grid.n - 1) as f32;
    let mut positions = Vec::with_capacity(w * hgt);
    let mut uvs = Vec::with_capacity(w * hgt);
    let mut indices: Vec<u32> = Vec::with_capacity((w - 1) * (hgt - 1) * 6);

    for j in j0..=j1 {
        for i in i0..=i1 {
            let xz = grid.world_pos(i, j);
            positions.push([xz.x, grid.get(i, j), xz.y]);
            uvs.push([i as f32 * uv_scale, j as f32 * uv_scale]);
        }
    }
    let idx = |i: usize, j: usize| ((j - j0) * w + (i - i0)) as u32;
    for j in j0..j1 {
        for i in i0..i1 {
            let (a, b, c, d) = (idx(i, j), idx(i + 1, j), idx(i, j + 1), idx(i + 1, j + 1));
            // Counter-clockwise seen from above (+Y).
            indices.extend_from_slice(&[a, c, b, b, c, d]);
        }
    }

    // Skirts: duplicate border vertices pushed down.
    let last = grid.n - 1;
    let mut skirt = |edge: Vec<(usize, usize)>, flip: bool| {
        let base = positions.len() as u32;
        for &(i, j) in &edge {
            let k = idx(i, j) as usize;
            let mut p = positions[k];
            p[1] -= SKIRT_DEPTH;
            positions.push(p);
            uvs.push(uvs[k]);
        }
        for s in 0..edge.len() - 1 {
            let top0 = idx(edge[s].0, edge[s].1);
            let top1 = idx(edge[s + 1].0, edge[s + 1].1);
            let bot0 = base + s as u32;
            let bot1 = base + s as u32 + 1;
            if flip {
                indices.extend_from_slice(&[top0, top1, bot0, top1, bot1, bot0]);
            } else {
                indices.extend_from_slice(&[top0, bot0, top1, top1, bot0, bot1]);
            }
        }
    };
    if j0 == 0 {
        skirt((i0..=i1).map(|i| (i, 0)).collect(), true);
    }
    if j1 == last {
        skirt((i0..=i1).map(|i| (i, last)).collect(), false);
    }
    if i0 == 0 {
        skirt((j0..=j1).map(|j| (0, j)).collect(), false);
    }
    if i1 == last {
        skirt((j0..=j1).map(|j| (last, j)).collect(), true);
    }

    // The normal map holds world-space normals (see `bake_textures`), so the geometric frame
    // is the identity: N = +Y, T = +X, and w = -1 so that B = w * (N x T) = +Z.
    let count = positions.len();
    *triangles += indices.len() / 3;
    Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::RENDER_WORLD,
    )
    .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, positions)
    .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, vec![[0.0f32, 1.0, 0.0]; count])
    .with_inserted_attribute(
        Mesh::ATTRIBUTE_TANGENT,
        vec![[1.0f32, 0.0, 0.0, -1.0]; count],
    )
    .with_inserted_attribute(Mesh::ATTRIBUTE_UV_0, uvs)
    .with_inserted_indices(Indices::U32(indices))
}

pub(crate) fn generate_terrain(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut images: ResMut<Assets<Image>>,
) {
    let start = Instant::now();

    let fine = bake_grid(FINE_SPACING, FINE_CELLS);
    let coarse = bake_grid(COARSE_SPACING, COARSE_CELLS);
    let grids_done = start.elapsed();

    let root = commands
        .spawn((
            Name::new("Terrain"),
            Transform::default(),
            Visibility::default(),
        ))
        .id();
    let mut triangles = 0usize;
    let mut chunks = 0usize;
    let fine_half = FINE_SPACING * FINE_CELLS as f32 * 0.5;

    for (grid, cells, tex_res) in [
        (&fine, FINE_CELLS, FINE_TEXTURE_RES),
        (&coarse, COARSE_CELLS, COARSE_TEXTURE_RES),
    ] {
        let extent = grid.spacing * cells as f32;
        let (color, normal) = bake_textures(grid.origin, extent, tex_res);
        let material = materials.add(StandardMaterial {
            base_color_texture: Some(images.add(color)),
            normal_map_texture: Some(images.add(normal)),
            perceptual_roughness: 0.9,
            reflectance: 0.25,
            ..default()
        });

        let is_coarse = std::ptr::eq(grid, &coarse);
        let n_chunks = cells / CHUNK_CELLS;
        for cj in 0..n_chunks {
            for ci in 0..n_chunks {
                let (i0, j0) = (ci * CHUNK_CELLS, cj * CHUNK_CELLS);
                let (i1, j1) = (i0 + CHUNK_CELLS, j0 + CHUNK_CELLS);
                // Skip coarse chunks fully covered by the fine grid.
                let (a, b) = (grid.world_pos(i0, j0), grid.world_pos(i1, j1));
                if is_coarse && a.min_element() >= -fine_half && b.max_element() <= fine_half {
                    continue;
                }
                let mesh = build_chunk(grid, i0, i1, j0, j1, &mut triangles);
                let child = commands
                    .spawn((
                        Mesh3d(meshes.add(mesh)),
                        MeshMaterial3d(material.clone()),
                        Transform::default(),
                    ))
                    .id();
                commands.entity(root).add_child(child);
                chunks += 1;
            }
        }
    }

    let (mut min_h, mut max_h) = (f32::MAX, f32::MIN);
    for &h in fine.heights.iter().chain(coarse.heights.iter()) {
        min_h = min_h.min(h);
        max_h = max_h.max(h);
    }
    info!(
        "terrain: generated in {:.2?} (height grids {:.2?}); {chunks} chunks, {triangles} triangles, heights {min_h:.0}..{max_h:.0} m",
        start.elapsed(),
        grids_done,
    );

    commands.insert_resource(GeneratedTerrain { fine, coarse });
}

// ---------------------------------------------------------------------------------------------
// Debug camera
// ---------------------------------------------------------------------------------------------

/// `TERRAIN_DEBUG_CAM` is either a preset number or `eye_x,eye_y,eye_z,target_x,target_y,target_z`.
#[derive(Resource)]
struct DebugCam {
    eye: Vec3,
    target: Vec3,
}

impl DebugCam {
    fn from_env(s: &str) -> Option<Self> {
        let v: Vec<f32> = s.split(',').filter_map(|p| p.trim().parse().ok()).collect();
        let (eye, target) = match v.as_slice() {
            [x, y, z, tx, ty, tz] => (Vec3::new(*x, *y, *z), Vec3::new(*tx, *ty, *tz)),
            // the player's view at spawn, looking up the valley
            [1.0] => (
                Vec3::new(0.0, 1500.0, 6000.0),
                Vec3::new(0.0, 1000.0, -6000.0),
            ),
            // high oblique overview of the spawn area
            [2.0] => (
                Vec3::new(0.0, 9000.0, 22000.0),
                Vec3::new(0.0, 0.0, -4000.0),
            ),
            // overview from very high up
            [3.0] => (
                Vec3::new(0.0, 45000.0, 25000.0),
                Vec3::new(0.0, 0.0, -5000.0),
            ),
            // close to a range, at combat altitude
            [4.0] => (
                Vec3::new(-6000.0, 2200.0, -2000.0),
                Vec3::new(-16000.0, 1200.0, -12000.0),
            ),
            [_] => (
                Vec3::new(5000.0, 2500.0, -2000.0),
                Vec3::new(20000.0, 1200.0, -20000.0),
            ),
            _ => return None,
        };
        Some(Self { eye, target })
    }
}

fn debug_cam(
    view: Res<DebugCam>,
    terrain: Option<Res<GeneratedTerrain>>,
    mut cams: Query<&mut Transform, With<Camera3d>>,
) {
    let mut eye = view.eye;
    if let Some(t) = &terrain {
        eye.y = eye.y.max(t.height_at(eye.x, eye.z) + 150.0);
    }
    for mut tf in &mut cams {
        *tf = Transform::from_translation(eye).looking_at(view.target, Vec3::Y);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hillshade() {
        // Writes a hillshade of a region to $HILLSHADE (PGM), for eyeballing the filter.
        let Ok(path) = std::env::var("HILLSHADE") else {
            return;
        };
        let size = 1024usize;
        let (cx, cz, extent) = (0.0f32, 0.0f32, 40000.0f32);
        let step = extent / size as f32;
        let mut h = vec![0.0f32; size * size];
        for j in 0..size {
            for i in 0..size {
                let x = cx - extent * 0.5 + i as f32 * step;
                let z = cz - extent * 0.5 + j as f32 * step;
                h[j * size + i] = sample_terrain(x, z, 5.0).height;
            }
        }
        let light = Vec3::new(-1.0, 1.0, -1.0).normalize();
        let mut out = format!("P5 {size} {size} 255\n").into_bytes();
        for j in 0..size {
            for i in 0..size {
                let i1 = (i + 1).min(size - 1);
                let j1 = (j + 1).min(size - 1);
                let dx = (h[j * size + i1] - h[j * size + i]) / step;
                let dz = (h[j1 * size + i] - h[j * size + i]) / step;
                let n = Vec3::new(-dx, 1.0, -dz).normalize();
                out.push((n.dot(light).max(0.0) * 255.0) as u8);
            }
        }
        std::fs::write(path, out).unwrap();
    }

    #[test]
    fn no_nans() {
        let fine = bake_grid(FINE_SPACING, FINE_CELLS);
        let bad: Vec<_> = (0..fine.heights.len())
            .filter(|&k| !fine.heights[k].is_finite())
            .map(|k| fine.world_pos(k % fine.n, k / fine.n))
            .take(10)
            .collect();
        assert!(bad.is_empty(), "non-finite samples at {bad:?}");
    }

    #[test]
    fn spawn_is_clear() {
        for k in 0..2000 {
            let a = k as f32 * 0.137;
            let r = (k % 40) as f32 / 39.0 * SPAWN_CLEAR_RADIUS;
            let x = SPAWN.x + a.cos() * r;
            let z = SPAWN.y + a.sin() * r;
            let h = sample_terrain(x, z, EROSION_OCTAVES as f32).height;
            assert!(h < 900.0, "terrain at ({x}, {z}) is {h}");
        }
    }

    #[test]
    fn stats() {
        let axis: Vec<String> = (-12..=14)
            .map(|k| format!("{:.0}", sample_terrain(0.0, k as f32 * 1000.0, 5.0).height))
            .collect();
        println!("valley axis z=-12..14 km: {}", axis.join(" "));
        let mut hs = Vec::new();
        for j in -60..=60 {
            for i in -60..=60 {
                hs.push(sample_terrain(i as f32 * 1000.0, j as f32 * 1000.0, 5.0).height);
            }
        }
        hs.sort_by(f32::total_cmp);
        let pct = |p: f32| hs[((hs.len() - 1) as f32 * p) as usize];
        println!(
            "min {:.0} p5 {:.0} p25 {:.0} p50 {:.0} p75 {:.0} p95 {:.0} p99 {:.0} max {:.0}",
            pct(0.0),
            pct(0.05),
            pct(0.25),
            pct(0.5),
            pct(0.75),
            pct(0.95),
            pct(0.99),
            pct(1.0)
        );
    }
}
