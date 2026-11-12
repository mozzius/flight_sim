//! Procedurally baked effect textures, generated on the CPU at startup.
//!
//! - **Puff atlas**: 4x4 tiles of billowy smoke puffs. Each puff is a union of
//!   hemispherical "cauliflower" lobes, domain-warped and roughened with fBm.
//!   RG holds the surface normal (so the shader can light the puff from the
//!   sun), B the thickness (used for self-shadowing, fire core temperature and
//!   back-lighting) and A the density.
//! - **Noise**: tileable fBm in four independent channels, used for
//!   distortion, flicker and dissolving.
//!
//! Both get a full box-filtered mip chain so distant puffs don't sparkle.

use bevy::{
    asset::RenderAssetUsages,
    image::{ImageAddressMode, ImageFilterMode, ImageSampler, ImageSamplerDescriptor},
    prelude::*,
    render::render_resource::{Extent3d, TextureDimension, TextureFormat},
};

pub const ATLAS_TILES: u32 = 4;
const TILE: u32 = 256;
const NOISE: u32 = 256;

fn hash(x: i32, y: i32, z: i32, seed: u32) -> f32 {
    let mut h = (x as u32).wrapping_mul(0x8da6_b343)
        ^ (y as u32).wrapping_mul(0xd816_3841)
        ^ (z as u32).wrapping_mul(0xcb1a_b31f)
        ^ seed.wrapping_mul(0x2c1b_3c6d);
    h ^= h >> 15;
    h = h.wrapping_mul(0x2c1b_3c6d);
    h ^= h >> 12;
    h = h.wrapping_mul(0x297a_2d39);
    h ^= h >> 15;
    (h & 0x00ff_ffff) as f32 / 16_777_216.0
}

fn smooth(t: f32) -> f32 {
    t * t * t * (t * (t * 6.0 - 15.0) + 10.0)
}

/// Value noise in [0, 1], tiling every `period` lattice cells (0 = no tiling).
fn value_noise(p: Vec2, period: i32, seed: u32) -> f32 {
    let i = p.floor();
    let f = p - i;
    let (ix, iy) = (i.x as i32, i.y as i32);
    let wrap = |v: i32| if period > 0 { v.rem_euclid(period) } else { v };
    let c = |dx: i32, dy: i32| hash(wrap(ix + dx), wrap(iy + dy), 0, seed);
    let (u, v) = (smooth(f.x), smooth(f.y));
    let a = c(0, 0) + (c(1, 0) - c(0, 0)) * u;
    let b = c(0, 1) + (c(1, 1) - c(0, 1)) * u;
    a + (b - a) * v
}

/// Fractal value noise in roughly [0, 1].
fn fbm(p: Vec2, period: i32, octaves: u32, seed: u32) -> f32 {
    let (mut sum, mut amp, mut norm, mut freq) = (0.0, 0.5, 0.0, 1.0);
    for o in 0..octaves {
        let per = if period > 0 { period * freq as i32 } else { 0 };
        sum += amp * value_noise(p * freq, per, seed.wrapping_add(o * 7919));
        norm += amp;
        amp *= 0.5;
        freq *= 2.0;
    }
    sum / norm
}

/// Appends box-filtered mips to a square RGBA8 image; returns the level count.
fn build_mips(data: &mut Vec<u8>, size: u32) -> u32 {
    let mut levels = 1;
    let mut src_offset = 0;
    let mut s = size;
    while s > 1 {
        let d = s / 2;
        let mut next = Vec::with_capacity((d * d * 4) as usize);
        for y in 0..d {
            for x in 0..d {
                for c in 0..4 {
                    let at = |xx: u32, yy: u32| data[src_offset + ((yy * s + xx) * 4 + c) as usize] as u32;
                    let v = at(2 * x, 2 * y) + at(2 * x + 1, 2 * y) + at(2 * x, 2 * y + 1) + at(2 * x + 1, 2 * y + 1);
                    next.push(((v + 2) / 4) as u8);
                }
            }
        }
        src_offset += (s * s * 4) as usize;
        data.extend_from_slice(&next);
        s = d;
        levels += 1;
    }
    levels
}

fn image(size: u32, mut data: Vec<u8>, address: ImageAddressMode) -> Image {
    let levels = build_mips(&mut data, size);
    let mut image = Image::new_uninit(
        Extent3d { width: size, height: size, depth_or_array_layers: 1 },
        TextureDimension::D2,
        TextureFormat::Rgba8Unorm,
        RenderAssetUsages::RENDER_WORLD,
    );
    image.data = Some(data);
    image.texture_descriptor.mip_level_count = levels;
    image.sampler = ImageSampler::Descriptor(ImageSamplerDescriptor {
        address_mode_u: address,
        address_mode_v: address,
        mag_filter: ImageFilterMode::Linear,
        min_filter: ImageFilterMode::Linear,
        mipmap_filter: ImageFilterMode::Linear,
        anisotropy_clamp: 4,
        ..default()
    });
    image
}

/// One puff: height field (thickness) of a union of hemispherical lobes.
fn puff_heights(tile: u32) -> Vec<f32> {
    let seed = 1000 + tile * 31;
    let r = |i: u32| hash(i as i32, tile as i32, 3, seed);
    // lobes: a big central one, then smaller ones around it
    let count = 8 + (r(0) * 6.0) as u32;
    let mut lobes = vec![(Vec2::new((r(1) - 0.5) * 0.06, (r(2) - 0.5) * 0.06), 0.3 + r(3) * 0.06)];
    for i in 0..count {
        let a = r(10 + i) * std::f32::consts::TAU;
        let d = 0.1 + r(40 + i) * 0.17;
        let rad = 0.1 + r(70 + i) * 0.12;
        lobes.push((Vec2::from_angle(a) * d, rad));
    }
    let mut h = vec![0.0f32; (TILE * TILE) as usize];
    for y in 0..TILE {
        for x in 0..TILE {
            // tile space: x right, y up, centred, [-0.5, 0.5]
            let p = Vec2::new((x as f32 + 0.5) / TILE as f32 - 0.5, 0.5 - (y as f32 + 0.5) / TILE as f32);
            let warp = Vec2::new(fbm(p * 5.0, 0, 4, seed + 1), fbm(p * 5.0 + 17.3, 0, 4, seed + 2)) - 0.5;
            let q = p + warp * 0.09;
            let mut best = 0.0f32;
            for (c, rad) in &lobes {
                let d2 = (q - *c).length_squared();
                if d2 < rad * rad {
                    best = best.max((rad * rad - d2).sqrt());
                }
            }
            // small-scale billows on the surface
            let bumps = fbm(q * 14.0, 0, 4, seed + 3) - 0.5;
            let v = if best > 0.0 { best + bumps * 0.05 } else { 0.0 };
            // keep a clear border so neighbouring tiles never bleed in
            let border = ((0.5 - p.abs().max_element()) / 0.04).clamp(0.0, 1.0);
            h[(y * TILE + x) as usize] = v.max(0.0) * border;
        }
    }
    h
}

pub fn puff_atlas() -> Image {
    let size = TILE * ATLAS_TILES;
    let mut data = vec![0u8; (size * size * 4) as usize];
    for tile in 0..ATLAS_TILES * ATLAS_TILES {
        let h = puff_heights(tile);
        let detail_seed = 5000 + tile;
        let (tx, ty) = (tile % ATLAS_TILES, tile / ATLAS_TILES);
        let at = |x: i32, y: i32| h[(y.clamp(0, TILE as i32 - 1) as u32 * TILE + x.clamp(0, TILE as i32 - 1) as u32) as usize];
        for y in 0..TILE as i32 {
            for x in 0..TILE as i32 {
                let v = at(x, y);
                // gradient in tile units (1 tile = 1.0), y up
                let px = 1.0 / TILE as f32;
                let dhdx = (at(x + 1, y) - at(x - 1, y)) / (2.0 * px);
                let dhdy = -(at(x, y + 1) - at(x, y - 1)) / (2.0 * px);
                let n = Vec3::new(-dhdx, -dhdy, 1.0).normalize();
                let p = Vec2::new(x as f32, y as f32) / TILE as f32;
                // density: soft edge, wispy detail eaten out near the rim
                let detail = fbm(p * 9.0, 0, 5, detail_seed);
                let edge = ((v - 0.035 * (detail - 0.3)) / 0.085).clamp(0.0, 1.0);
                let density = edge * edge * (3.0 - 2.0 * edge) * (0.82 + 0.18 * detail);
                let thick = (v / 0.33).clamp(0.0, 1.0);
                let i = (((ty * TILE + y as u32) * size + tx * TILE + x as u32) * 4) as usize;
                data[i] = ((n.x * 0.5 + 0.5) * 255.0) as u8;
                data[i + 1] = ((n.y * 0.5 + 0.5) * 255.0) as u8;
                data[i + 2] = (thick * 255.0) as u8;
                data[i + 3] = (density * 255.0) as u8;
            }
        }
    }
    image(size, data, ImageAddressMode::ClampToEdge)
}

pub fn noise() -> Image {
    let mut data = Vec::with_capacity((NOISE * NOISE * 4) as usize);
    for y in 0..NOISE {
        for x in 0..NOISE {
            let p = Vec2::new(x as f32, y as f32) / NOISE as f32;
            for (period, seed) in [(6, 11u32), (6, 23), (12, 37), (4, 41)] {
                let v = fbm(p * period as f32, period, 5, seed);
                // stretch the contrast: fBm bunches around 0.5
                let v = ((v - 0.5) * 1.8 + 0.5).clamp(0.0, 1.0);
                data.push((v * 255.0) as u8);
            }
        }
    }
    image(NOISE, data, ImageAddressMode::Repeat)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mip_chain_is_complete() {
        let mut data = vec![128u8; 8 * 8 * 4];
        let levels = build_mips(&mut data, 8);
        assert_eq!(levels, 4);
        assert_eq!(data.len(), (64 + 16 + 4 + 1) * 4);
    }

    #[test]
    fn noise_tiles() {
        let a = fbm(Vec2::new(0.0, 0.3) * 6.0, 6, 5, 11);
        let b = fbm(Vec2::new(1.0, 0.3) * 6.0, 6, 5, 11);
        assert!((a - b).abs() < 1e-4);
    }
}
