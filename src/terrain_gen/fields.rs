//! Farmland: a patchwork of fields on the flat valley floors.
//!
//! The ground is split into "farms" (Voronoi cells of a jittered ~700 m
//! lattice). Each farm lays out a grid of rectangular fields (staggered rows,
//! like brickwork) turned to line up with the nearest road, or with a
//! slowly varying direction where there's none. Each field gets a crop. The
//! bake paints them (with tramlines and hedges along the edges), and the tree
//! planter puts hedgerow trees on the edges. A few farms are left as woodland.
//!
//! Everything is a pure function of position (a few hashes), apart from the
//! road directions, which [`FieldMap`] splats onto a coarse grid once after
//! the roads are planned.

use std::sync::LazyLock;

use bevy::prelude::*;

use super::{hash2, noised, roads::Road, smoothstep, srgb, terrain_seed};

/// Farm lattice spacing (m).
const FARM: f32 = 720.0;
/// Road-direction grid cell (m) and how far a road turns the fields (m).
const CELL: f32 = 200.0;
const ROAD_REACH: f32 = 1_600.0;
/// Hedge half-width (m).
const HEDGE: f32 = 4.0;

/// The direction of the nearest road on a coarse grid (zero where none is near).
pub(super) struct FieldMap {
    origin: Vec2,
    n: usize,
    dir: Vec<Vec2>,
}

impl FieldMap {
    pub(super) fn empty() -> Self {
        Self { origin: Vec2::ZERO, n: 0, dir: Vec::new() }
    }

    /// Splats each road's direction onto the cells within `ROAD_REACH` (keeping the nearest).
    pub(super) fn new(roads: &[Road], half_extent: f32) -> Self {
        let n = (2.0 * half_extent / CELL).ceil() as usize;
        let origin = Vec2::splat(-half_extent);
        let mut dir = vec![Vec2::ZERO; n * n];
        let mut dist = vec![f32::MAX; n * n];
        let r = (ROAD_REACH / CELL).ceil() as i32;
        for road in roads {
            // a sample every ~48 m along the road
            for w in road.windows(7).step_by(6) {
                let (a, b) = (w[0].xz(), w[6].xz());
                let Some(d) = (b - a).try_normalize() else { continue };
                let mid = (a + b) * 0.5;
                let c = ((mid - origin) / CELL).floor();
                for dj in -r..=r {
                    for di in -r..=r {
                        let (i, j) = (c.x as i32 + di, c.y as i32 + dj);
                        if i < 0 || j < 0 || i >= n as i32 || j >= n as i32 {
                            continue;
                        }
                        let k = j as usize * n + i as usize;
                        let centre = origin + (Vec2::new(i as f32, j as f32) + 0.5) * CELL;
                        let dd = centre.distance(mid);
                        if dd < ROAD_REACH && dd < dist[k] {
                            dist[k] = dd;
                            dir[k] = d;
                        }
                    }
                }
            }
        }
        Self { origin, n, dir }
    }

    fn road_dir(&self, p: Vec2) -> Option<Vec2> {
        let c = ((p - self.origin) / CELL).floor();
        if c.x < 0.0 || c.y < 0.0 || c.x >= self.n as f32 || c.y >= self.n as f32 {
            return None;
        }
        let d = self.dir[c.y as usize * self.n + c.x as usize];
        (d != Vec2::ZERO).then_some(d)
    }
}

/// The field at a point.
pub(super) struct Field {
    /// Index into the crop table.
    crop: usize,
    /// Distance to the field's edge (m).
    pub edge: f32,
    /// Coordinate across the field's rows (m), for tramlines.
    across: f32,
    /// 0..1 per field, for colour variation.
    shade: f32,
    /// The farm is woodland.
    pub wood: bool,
}

#[inline]
fn rand(i: i32, j: i32, salt: u32) -> Vec2 {
    hash2(i, j, terrain_seed() ^ salt) * 0.5 + 0.5
}

pub(super) fn field_at(p: Vec2, map: &FieldMap) -> Field {
    // nearest and second-nearest farm centres
    let g = (p / FARM).floor();
    let (gi, gj) = (g.x as i32, g.y as i32);
    let mut best = (f32::MAX, Vec2::ZERO, (0, 0));
    let mut second = (f32::MAX, Vec2::ZERO);
    for dj in -1..=1 {
        for di in -1..=1 {
            let (i, j) = (gi + di, gj + dj);
            let c = (Vec2::new(i as f32, j as f32) + 0.5 + (rand(i, j, 0xFA12) - 0.5) * 0.8) * FARM;
            let d = c.distance_squared(p);
            if d < best.0 {
                second = (best.0, best.1);
                best = (d, c, (i, j));
            } else if d < second.0 {
                second = (d, c);
            }
        }
    }
    let (d1, c1, (fi, fj)) = best;
    let (d2, c2) = second;
    // distance to the bisector between the two nearest centres
    let farm_edge = (d2 - d1) / (2.0 * c1.distance(c2).max(1.0));

    let h = rand(fi, fj, 0xFA13);
    let h2 = rand(fi, fj, 0xFA14);
    // line up with the road, else a direction that wanders slowly across the map
    let base = map.road_dir(c1).unwrap_or_else(|| {
        let a = noised(c1 / 9000.0, terrain_seed() ^ 0xFA15).x * std::f32::consts::PI;
        Vec2::from_angle(a)
    });
    // half the farms run their rows across the road instead of along it
    let base = if h2.y < 0.4 { base.perp() } else { base };
    let dir = base.rotate(Vec2::from_angle((h.x - 0.5) * 0.15));
    let d = p - c1;
    let (u, v) = (d.dot(dir.perp()), d.dot(dir));

    // rows `su` wide, fields `sv` long, each row staggered
    let su = 80.0 + 130.0 * h.y;
    let sv = 170.0 + 260.0 * h2.x;
    let row = (u / su).floor();
    let stagger = rand(fi * 64 + row as i32, fj, 0xFA16).x * sv;
    let col = ((v + stagger) / sv).floor();
    let fu = u / su - row;
    let fv = (v + stagger) / sv - col;
    let edge = (fu.min(1.0 - fu) * su).min(fv.min(1.0 - fv) * sv).min(farm_edge);

    let r = rand(fi * 64 + row as i32, fj * 64 + col as i32, 0xFA17);
    Field {
        crop: pick_crop(r.x),
        edge,
        across: u,
        shade: r.y,
        wood: rand(fi, fj, 0xFA18).x < 0.07,
    }
}

struct Crop {
    color: Vec3,
    weight: f32,
    /// Tramline / furrow contrast.
    stripes: f32,
    /// Stripe period (m).
    period: f32,
}

static CROPS: LazyLock<Vec<Crop>> = LazyLock::new(|| {
    let crop = |r, g, b, weight, stripes, period| Crop { color: srgb(r, g, b), weight, stripes, period };
    vec![
        // pasture and grass leys
        crop(0.37, 0.47, 0.21, 3.5, 0.0, 24.0),
        crop(0.30, 0.41, 0.17, 2.5, 0.0, 24.0),
        crop(0.42, 0.50, 0.25, 1.5, 0.0, 24.0),
        // cut hay / silage
        crop(0.55, 0.55, 0.34, 1.2, 0.10, 12.0),
        // wheat, barley
        crop(0.66, 0.58, 0.35, 1.8, 0.06, 24.0),
        crop(0.63, 0.60, 0.42, 1.2, 0.06, 24.0),
        // ploughed, bare soil
        crop(0.53, 0.43, 0.29, 1.4, 0.12, 12.0),
        crop(0.45, 0.37, 0.25, 0.8, 0.10, 12.0),
        // young crop on soil
        crop(0.44, 0.46, 0.27, 1.4, 0.15, 18.0),
        // rapeseed
        crop(0.74, 0.69, 0.26, 0.25, 0.0, 24.0),
    ]
});

fn pick_crop(u: f32) -> usize {
    let total: f32 = CROPS.iter().map(|c| c.weight).sum();
    let mut acc = 0.0;
    CROPS
        .iter()
        .position(|c| {
            acc += c.weight / total;
            u < acc
        })
        .unwrap_or(0)
}

/// The field's colour (linear) and the hedge coverage over a texel `step` metres wide.
pub(super) fn field_color(f: &Field, step: f32) -> (Vec3, f32) {
    let crop = &CROPS[f.crop];
    let mut c = crop.color * (0.92 + 0.16 * f.shade);
    // tramlines / furrows, faded out where the texels are too coarse to show them
    let visible = smoothstep(2.5 * step, 4.0 * step, crop.period);
    if crop.stripes > 0.0 && visible > 0.0 {
        let s = (f.across / crop.period * std::f32::consts::TAU).cos();
        c *= 1.0 - crop.stripes * visible * (0.5 + 0.5 * s);
    }
    // a hedge along every edge (area coverage of the texel)
    let w = step.max(1.0);
    let hedge = ((HEDGE + w * 0.5 - f.edge) / w).clamp(0.0, 1.0) * (2.0 * HEDGE / w).min(1.0);
    (c, hedge)
}
