//! Tileable Perlin / Worley noise, baked on the CPU at startup into the
//! textures the cloud raymarcher samples (and the weather map the CPU-side
//! [`super::CloudField`] reads).
//!
//! Everything here works in "tile space": coordinates in `[0, 1)` that wrap,
//! so the textures repeat seamlessly.

use bevy::math::{Vec2, Vec3};

/// Cheap integer hash (a 3-round mix of the lowbias32 family).
#[inline]
fn hash_u32(mut x: u32) -> u32 {
    x ^= x >> 16;
    x = x.wrapping_mul(0x7feb_352d);
    x ^= x >> 15;
    x = x.wrapping_mul(0x846c_a68b);
    x ^= x >> 16;
    x
}

#[inline]
fn hash3(x: u32, y: u32, z: u32, seed: u32) -> u32 {
    hash_u32(
        x.wrapping_mul(0x8da6_b343)
            ^ y.wrapping_mul(0xd816_3841)
            ^ z.wrapping_mul(0xcb1a_b31f)
            ^ seed.wrapping_mul(0x9e37_79b9),
    )
}

#[inline]
fn unit(h: u32) -> f32 {
    (h >> 8) as f32 / (1u32 << 24) as f32
}

#[inline]
fn fade(t: f32) -> f32 {
    t * t * t * (t * (t * 6.0 - 15.0) + 10.0)
}

#[inline]
fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// One of 12 edge-direction gradients (Perlin's improved noise set).
#[inline]
fn gradient(h: u32, x: f32, y: f32, z: f32) -> f32 {
    match h % 12 {
        0 => x + y,
        1 => -x + y,
        2 => x - y,
        3 => -x - y,
        4 => x + z,
        5 => -x + z,
        6 => x - z,
        7 => -x - z,
        8 => y + z,
        9 => -y + z,
        10 => y - z,
        _ => -y - z,
    }
}

/// Tileable 3D Perlin noise with `freq` lattice cells per tile. Returns roughly [-1, 1].
pub fn perlin3(p: Vec3, freq: u32, seed: u32) -> f32 {
    let f = freq as f32;
    let s = p * f;
    let fl = s.floor();
    let fr = s - fl;
    let (ix, iy, iz) = (fl.x as i32, fl.y as i32, fl.z as i32);
    let wrap = |v: i32| v.rem_euclid(freq as i32) as u32;
    let (u, v, w) = (fade(fr.x), fade(fr.y), fade(fr.z));

    let g = |dx: i32, dy: i32, dz: i32| {
        let h = hash3(wrap(ix + dx), wrap(iy + dy), wrap(iz + dz), seed);
        gradient(h, fr.x - dx as f32, fr.y - dy as f32, fr.z - dz as f32)
    };
    let x00 = lerp(g(0, 0, 0), g(1, 0, 0), u);
    let x10 = lerp(g(0, 1, 0), g(1, 1, 0), u);
    let x01 = lerp(g(0, 0, 1), g(1, 0, 1), u);
    let x11 = lerp(g(0, 1, 1), g(1, 1, 1), u);
    let y0 = lerp(x00, x10, v);
    let y1 = lerp(x01, x11, v);
    lerp(y0, y1, w)
}

/// Tileable 2D Perlin noise, `freq` cells per tile. Returns roughly [-1, 1].
pub fn perlin2(p: Vec2, freq: u32, seed: u32) -> f32 {
    let f = freq as f32;
    let s = p * f;
    let fl = s.floor();
    let fr = s - fl;
    let (ix, iy) = (fl.x as i32, fl.y as i32);
    let wrap = |v: i32| v.rem_euclid(freq as i32) as u32;
    let (u, v) = (fade(fr.x), fade(fr.y));
    let g = |dx: i32, dy: i32| {
        let h = hash3(wrap(ix + dx), wrap(iy + dy), 7, seed);
        let a = unit(h) * core::f32::consts::TAU;
        let (sa, ca) = a.sin_cos();
        ca * (fr.x - dx as f32) + sa * (fr.y - dy as f32)
    };
    let x0 = lerp(g(0, 0), g(1, 0), u);
    let x1 = lerp(g(0, 1), g(1, 1), u);
    lerp(x0, x1, v)
}

/// Perlin FBM in [0, 1].
pub fn perlin3_fbm(p: Vec3, base_freq: u32, octaves: u32, seed: u32) -> f32 {
    let mut sum = 0.0;
    let mut amp = 1.0;
    let mut norm = 0.0;
    let mut freq = base_freq;
    for i in 0..octaves {
        sum += perlin3(p, freq, seed + i) * amp;
        norm += amp;
        amp *= 0.5;
        freq *= 2;
    }
    (sum / norm * 0.5 + 0.5).clamp(0.0, 1.0)
}

/// Perlin FBM in [0, 1].
pub fn perlin2_fbm(p: Vec2, base_freq: u32, octaves: u32, seed: u32) -> f32 {
    let mut sum = 0.0;
    let mut amp = 1.0;
    let mut norm = 0.0;
    let mut freq = base_freq;
    for i in 0..octaves {
        sum += perlin2(p, freq, seed + i) * amp;
        norm += amp;
        amp *= 0.5;
        freq *= 2;
    }
    (sum / norm * 0.5 + 0.5).clamp(0.0, 1.0)
}

/// Feature points for tileable Worley noise, one per cell, precomputed so the
/// per-voxel cost is just 27 distance checks.
pub struct WorleyCells {
    freq: u32,
    points: Vec<Vec3>,
}

impl WorleyCells {
    pub fn new(freq: u32, seed: u32) -> Self {
        let n = freq as usize;
        let mut points = Vec::with_capacity(n * n * n);
        for z in 0..freq {
            for y in 0..freq {
                for x in 0..freq {
                    let h = hash3(x, y, z, seed);
                    points.push(Vec3::new(
                        unit(h),
                        unit(hash_u32(h ^ 0x1234_5678)),
                        unit(hash_u32(h ^ 0x9abc_def0)),
                    ));
                }
            }
        }
        Self { freq, points }
    }

    /// Inverted Worley (1 at feature points, 0 far from them), the "billowy" look.
    pub fn sample(&self, p: Vec3) -> f32 {
        let f = self.freq as f32;
        let s = p * f;
        let fl = s.floor();
        let fr = s - fl;
        let n = self.freq as i32;
        let (ix, iy, iz) = (fl.x as i32, fl.y as i32, fl.z as i32);
        let mut best = f32::MAX;
        for dz in -1..=1 {
            let cz = (iz + dz).rem_euclid(n) as usize;
            for dy in -1..=1 {
                let cy = (iy + dy).rem_euclid(n) as usize;
                for dx in -1..=1 {
                    let cx = (ix + dx).rem_euclid(n) as usize;
                    let fp = self.points[(cz * n as usize + cy) * n as usize + cx]
                        + Vec3::new(dx as f32, dy as f32, dz as f32);
                    let d = (fp - fr).length_squared();
                    if d < best {
                        best = d;
                    }
                }
            }
        }
        (1.0 - best.sqrt()).clamp(0.0, 1.0)
    }
}

/// Three-octave inverted Worley FBM in [0, 1].
pub struct WorleyFbm([WorleyCells; 3]);

impl WorleyFbm {
    pub fn new(base_freq: u32, seed: u32) -> Self {
        Self([
            WorleyCells::new(base_freq, seed),
            WorleyCells::new(base_freq * 2, seed + 11),
            WorleyCells::new(base_freq * 4, seed + 23),
        ])
    }

    pub fn sample(&self, p: Vec3) -> f32 {
        self.0[0].sample(p) * 0.625 + self.0[1].sample(p) * 0.25 + self.0[2].sample(p) * 0.125
    }
}

#[inline]
pub fn remap(v: f32, lo: f32, hi: f32, nlo: f32, nhi: f32) -> f32 {
    nlo + (v - lo) / (hi - lo) * (nhi - nlo)
}

/// Runs `f(z)` for every slice of a volume (or row of an image) on all cores,
/// returning the bytes in order.
fn parallel_slices(slices: usize, bytes_per_slice: usize, f: impl Fn(usize, &mut [u8]) + Sync) -> Vec<u8> {
    let mut data = vec![0u8; slices * bytes_per_slice];
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(slices);
    let per_thread = slices.div_ceil(threads);
    std::thread::scope(|scope| {
        for (t, chunk) in data.chunks_mut(per_thread * bytes_per_slice).enumerate() {
            let f = &f;
            scope.spawn(move || {
                for (i, slice) in chunk.chunks_mut(bytes_per_slice).enumerate() {
                    f(t * per_thread + i, slice);
                }
            });
        }
    });
    data
}

/// RGBA8 `size³` base-shape texture: R = Perlin-Worley, GBA = Worley FBM at
/// rising frequencies (the Nubis / "Real-time volumetric cloudscapes" layout).
pub fn base_shape(size: u32) -> Vec<u8> {
    let pw_worley = WorleyFbm::new(4, 101);
    let w_g = WorleyFbm::new(6, 202);
    let w_b = WorleyFbm::new(12, 303);
    let w_a = WorleyFbm::new(24, 404);
    let n = size as usize;
    parallel_slices(n, n * n * 4, |z, slice| {
        for y in 0..n {
            for x in 0..n {
                let p = Vec3::new(x as f32 + 0.5, y as f32 + 0.5, z as f32 + 0.5) / size as f32;
                let perlin = perlin3_fbm(p, 4, 6, 1);
                let pw = remap(perlin, 0.0, 1.0, pw_worley.sample(p), 1.0).clamp(0.0, 1.0);
                let o = (y * n + x) * 4;
                slice[o] = (pw * 255.0) as u8;
                slice[o + 1] = (w_g.sample(p) * 255.0) as u8;
                slice[o + 2] = (w_b.sample(p) * 255.0) as u8;
                slice[o + 3] = (w_a.sample(p) * 255.0) as u8;
            }
        }
    })
}

/// RGBA8 `size³` erosion-detail texture: Worley FBM at three frequencies.
pub fn detail_shape(size: u32) -> Vec<u8> {
    let w_r = WorleyFbm::new(2, 505);
    let w_g = WorleyFbm::new(4, 606);
    let w_b = WorleyFbm::new(8, 707);
    let n = size as usize;
    parallel_slices(n, n * n * 4, |z, slice| {
        for y in 0..n {
            for x in 0..n {
                let p = Vec3::new(x as f32 + 0.5, y as f32 + 0.5, z as f32 + 0.5) / size as f32;
                let o = (y * n + x) * 4;
                slice[o] = (w_r.sample(p) * 255.0) as u8;
                slice[o + 1] = (w_g.sample(p) * 255.0) as u8;
                slice[o + 2] = (w_b.sample(p) * 255.0) as u8;
                slice[o + 3] = 255;
            }
        }
    })
}

/// Weather map texel, as floats in [0, 1].
#[derive(Clone, Copy, Debug, Default)]
pub struct Weather {
    /// Fraction of the sky covered at this point.
    pub coverage: f32,
    /// 0 = flat strato-cumulus, 1 = tall cumulus.
    pub cloud_type: f32,
}

/// `size²` weather map (coverage, type) generated from 2D Perlin FBM.
pub fn weather_map(size: u32, coverage_bias: f32) -> Vec<Weather> {
    let n = size as usize;
    let bytes = parallel_slices(n, n * 8, |y, row| {
        for x in 0..n {
            let p = Vec2::new(x as f32 + 0.5, y as f32 + 0.5) / size as f32;
            let w = weather_at(p, coverage_bias);
            row[x * 8..x * 8 + 4].copy_from_slice(&w.coverage.to_le_bytes());
            row[x * 8 + 4..x * 8 + 8].copy_from_slice(&w.cloud_type.to_le_bytes());
        }
    });
    bytes
        .chunks_exact(8)
        .map(|c| Weather {
            coverage: f32::from_le_bytes([c[0], c[1], c[2], c[3]]),
            cloud_type: f32::from_le_bytes([c[4], c[5], c[6], c[7]]),
        })
        .collect()
}

/// The weather function itself: big soft cloud banks with clear gaps between
/// them, plus a slowly varying cloud type so some banks tower and others
/// stay flat.
pub fn weather_at(p: Vec2, coverage_bias: f32) -> Weather {
    let big = perlin2_fbm(p, 3, 5, 41);
    let medium = perlin2_fbm(p, 9, 4, 42);
    let raw = big * 0.62 + medium * 0.38;
    // sharpen into banks and gaps; bias shifts the overall cloudiness
    let coverage = smoothstep(0.42 - coverage_bias, 0.68 - coverage_bias, raw);
    // tall cumulus vs flat strato-cumulus: a regional trend plus per-bank variation
    let regional = perlin2_fbm(p + Vec2::new(0.37, 0.61), 2, 2, 43);
    let local = perlin2_fbm(p + Vec2::new(0.11, 0.83), 7, 3, 44);
    let cloud_type = smoothstep(0.3, 0.72, regional * 0.55 + local * 0.45);
    Weather {
        coverage: coverage.clamp(0.0, 1.0),
        cloud_type: cloud_type.clamp(0.0, 1.0),
    }
}

#[inline]
pub fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perlin_tiles_seamlessly() {
        let a = perlin3(Vec3::new(0.001, 0.4, 0.7), 4, 9);
        let b = perlin3(Vec3::new(1.001, 0.4, 0.7), 4, 9);
        assert!((a - b).abs() < 1e-4, "{a} vs {b}");
    }

    #[test]
    fn worley_tiles_seamlessly() {
        let w = WorleyCells::new(5, 3);
        let a = w.sample(Vec3::new(0.002, 0.3, 0.9));
        let b = w.sample(Vec3::new(1.002, 0.3, 0.9));
        assert!((a - b).abs() < 1e-4, "{a} vs {b}");
    }

    #[test]
    fn weather_has_both_clouds_and_gaps() {
        let w = weather_map(64, 0.0);
        let mean = w.iter().map(|w| w.coverage).sum::<f32>() / w.len() as f32;
        assert!(mean > 0.2 && mean < 0.8, "mean coverage {mean}");
    }
}
