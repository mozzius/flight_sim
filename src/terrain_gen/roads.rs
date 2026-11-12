//! A road network across the high-res grid.
//!
//! A handful of flat, low "sites" (where villages would be) are picked from the
//! height grid and joined one at a time: each new site is routed to the nearest
//! point of the network so far with a Dijkstra search whose cost climbs steeply
//! with the grade, so roads follow valley floors and switch back up passes.
//! The paths are then smoothed, given a gently graded elevation profile, and
//! the terrain is levelled under them before the meshes are built.

use std::{cmp::Reverse, collections::BinaryHeap};

use bevy::{
    asset::RenderAssetUsages,
    mesh::{Indices, PrimitiveTopology},
    prelude::*,
};

use super::{HeightGrid, smoothstep};

/// Pathfinding works on every `STRIDE`th grid vertex (64 m on the 32 m grid).
const STRIDE: usize = 2;
/// Roads stay this far inside the grid's edge (m).
const EDGE_MARGIN: f32 = 1_200.0;
const MAX_SITES: usize = 9;
const SITE_SPACING: f32 = 3_800.0;
/// Spacing of the final centreline points (m).
const POINT_SPACING: f32 = 8.0;
pub(super) const ROAD_HALF_WIDTH: f32 = 4.5;
const SHOULDER: f32 = 2.5;
/// Terrain within this distance of the centreline is set to the road's level,
/// then blended back to the natural ground by `BLEND_OUTER`.
const BLEND_INNER: f32 = 20.0;
const BLEND_OUTER: f32 = 75.0;

/// A road centreline: points ~8 m apart, y = road surface height.
pub(super) type Road = Vec<Vec3>;

/// Plans the network and levels `grid` under it. Returns the centrelines.
pub(super) fn build_roads(grid: &mut HeightGrid) -> Vec<Road> {
    let paths = plan(grid);
    let mut roads: Vec<Road> = Vec::new();
    for path in paths {
        let mut road = profile(grid, &smooth(&path));
        // tie the end into the road it joins
        if let Some(&end) = road.last() {
            if let Some(join_y) = nearest_on_roads(&roads, end.xz()).map(|(p, _)| p.y) {
                let n = road.len().min(25);
                let len = road.len();
                for k in 0..n {
                    let t = k as f32 / n as f32; // 0 at the join
                    let idx = len - 1 - k;
                    road[idx].y = join_y + (road[idx].y - join_y) * smoothstep(0.0, 1.0, t);
                }
            }
        }
        roads.push(road);
    }
    flatten(grid, &roads);
    roads
}

/// Nearest point on any road to `p`, and its distance.
fn nearest_on_roads(roads: &[Road], p: Vec2) -> Option<(Vec3, f32)> {
    let mut best: Option<(Vec3, f32)> = None;
    for road in roads {
        for w in road.windows(2) {
            let (q, d) = closest_on_segment(w[0], w[1], p);
            if best.is_none_or(|(_, bd)| d < bd) {
                best = Some((q, d));
            }
        }
    }
    best
}

fn closest_on_segment(a: Vec3, b: Vec3, p: Vec2) -> (Vec3, f32) {
    let ab = b.xz() - a.xz();
    let t = ((p - a.xz()).dot(ab) / ab.length_squared().max(1e-6)).clamp(0.0, 1.0);
    let q = a.lerp(b, t);
    (q, q.xz().distance(p))
}

/// Spatial hash of road segments for "how far is the nearest road" queries.
pub(super) struct RoadIndex {
    cell: f32,
    cells: std::collections::HashMap<(i32, i32), Vec<(Vec3, Vec3)>>,
}

impl RoadIndex {
    pub(super) fn new(roads: &[Road], reach: f32) -> Self {
        let cell = 128.0;
        let mut cells: std::collections::HashMap<(i32, i32), Vec<(Vec3, Vec3)>> = default();
        for road in roads {
            for w in road.windows(2) {
                let (lo, hi) = (w[0].xz().min(w[1].xz()) - reach, w[0].xz().max(w[1].xz()) + reach);
                for cj in (lo.y / cell).floor() as i32..=(hi.y / cell).floor() as i32 {
                    for ci in (lo.x / cell).floor() as i32..=(hi.x / cell).floor() as i32 {
                        cells.entry((ci, cj)).or_default().push((w[0], w[1]));
                    }
                }
            }
        }
        Self { cell, cells }
    }

    /// Nearest road point within the reach given to `new`, and its distance.
    pub(super) fn nearest(&self, p: Vec2) -> Option<(Vec3, f32)> {
        let key = ((p.x / self.cell).floor() as i32, (p.y / self.cell).floor() as i32);
        self.cells.get(&key)?.iter().fold(None, |best, &(a, b)| {
            let (q, d) = closest_on_segment(a, b, p);
            if best.is_none_or(|(_, bd)| d < bd) { Some((q, d)) } else { best }
        })
    }
}

/// Coarse pathfinding lattice over the grid.
struct Lattice<'a> {
    grid: &'a HeightGrid,
    m: usize,
    lo: usize,
    hi: usize,
}

impl Lattice<'_> {
    fn h(&self, i: usize, j: usize) -> f32 {
        self.grid.get(i * STRIDE, j * STRIDE)
    }
    fn pos(&self, i: usize, j: usize) -> Vec2 {
        self.grid.world_pos(i * STRIDE, j * STRIDE)
    }
    fn slope(&self, i: usize, j: usize) -> f32 {
        let s = self.grid.spacing * STRIDE as f32;
        let dx = (self.h(i + 1, j) - self.h(i - 1, j)) / (2.0 * s);
        let dz = (self.h(i, j + 1) - self.h(i, j - 1)) / (2.0 * s);
        Vec2::new(dx, dz).length()
    }
}

fn plan(grid: &HeightGrid) -> Vec<Vec<Vec2>> {
    let m = (grid.n - 1) / STRIDE + 1;
    let margin = (EDGE_MARGIN / (grid.spacing * STRIDE as f32)).ceil() as usize;
    let lat = Lattice { grid, m, lo: margin, hi: m - 1 - margin };

    // ---- sites: the flattest low spot in each block of a coarse lattice
    let block = 24; // ~1.5 km
    let mut candidates = Vec::new();
    let mut bj = lat.lo + 1;
    while bj + block < lat.hi {
        let mut bi = lat.lo + 1;
        while bi + block < lat.hi {
            let mut best: Option<(f32, usize, usize)> = None;
            for j in bj..bj + block {
                for i in bi..bi + block {
                    let (h, s) = (lat.h(i, j), lat.slope(i, j));
                    if h > 750.0 || s > 0.08 {
                        continue;
                    }
                    let score = h + s * 4_000.0;
                    if best.is_none_or(|(b, ..)| score < b) {
                        best = Some((score, i, j));
                    }
                }
            }
            candidates.extend(best);
            bi += block;
        }
        bj += block;
    }
    candidates.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut sites: Vec<(usize, usize)> = Vec::new();
    for (_, i, j) in candidates {
        let p = lat.pos(i, j);
        if sites.iter().all(|&(si, sj)| lat.pos(si, sj).distance(p) > SITE_SPACING) {
            sites.push((i, j));
            if sites.len() == MAX_SITES {
                break;
            }
        }
    }
    if sites.len() < 2 {
        return Vec::new();
    }

    // ---- join them up, nearest first, each to the nearest point of the network
    let mut on_road = vec![false; m * m];
    let mut connected = vec![sites[0]];
    on_road[sites[0].1 * m + sites[0].0] = true;
    let mut remaining: Vec<_> = sites[1..].to_vec();
    let mut paths = Vec::new();
    while !remaining.is_empty() {
        let (k, _) = remaining
            .iter()
            .enumerate()
            .map(|(k, &(i, j))| {
                let d = connected
                    .iter()
                    .map(|&(ci, cj)| lat.pos(ci, cj).distance(lat.pos(i, j)))
                    .fold(f32::MAX, f32::min);
                (k, d)
            })
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .unwrap();
        let start = remaining.swap_remove(k);
        if let Some(nodes) = route(&lat, start, &on_road) {
            for &(i, j) in &nodes {
                on_road[j * m + i] = true;
            }
            paths.push(nodes.iter().map(|&(i, j)| lat.pos(i, j)).collect());
        }
        connected.push(start);
    }
    paths
}

/// Dijkstra from `start` to the nearest lattice node already on a road.
fn route(lat: &Lattice, start: (usize, usize), goal: &[bool]) -> Option<Vec<(usize, usize)>> {
    let m = lat.m;
    let mut cost = vec![f32::INFINITY; m * m];
    let mut prev = vec![u32::MAX; m * m];
    let mut heap = BinaryHeap::new();
    let s = start.1 * m + start.0;
    cost[s] = 0.0;
    heap.push(Reverse((0u32, s as u32)));
    let step = lat.grid.spacing * STRIDE as f32;
    while let Some(Reverse((c_bits, node))) = heap.pop() {
        let node = node as usize;
        let c = f32::from_bits(c_bits);
        if c > cost[node] {
            continue;
        }
        let (i, j) = (node % m, node / m);
        if goal[node] && node != s {
            let mut out = vec![(i, j)];
            let mut n = node;
            while prev[n] != u32::MAX {
                n = prev[n] as usize;
                out.push((n % m, n / m));
            }
            out.reverse();
            return Some(out);
        }
        let h = lat.h(i, j);
        for (di, dj) in [(-1i32, 0i32), (1, 0), (0, -1), (0, 1), (-1, -1), (1, -1), (-1, 1), (1, 1)] {
            let (ni, nj) = (i as i32 + di, j as i32 + dj);
            if ni < lat.lo as i32 || nj < lat.lo as i32 || ni > lat.hi as i32 || nj > lat.hi as i32 {
                continue;
            }
            let (ni, nj) = (ni as usize, nj as usize);
            let len = step * if di != 0 && dj != 0 { std::f32::consts::SQRT_2 } else { 1.0 };
            let grade = (lat.h(ni, nj) - h).abs() / len;
            // gentle grades are cheap, steep ones very expensive; a little
            // pull toward low ground keeps roads in the valleys
            let mut w = len * (1.0 + 60.0 * grade * grade + lat.h(ni, nj).max(0.0) * 0.0006);
            if grade > 0.14 {
                w += len * 40.0 * (grade - 0.14) / 0.1;
            }
            let nc = c + w;
            let n = nj * m + ni;
            if nc < cost[n] {
                cost[n] = nc;
                prev[n] = node as u32;
                heap.push(Reverse((nc.to_bits(), n as u32)));
            }
        }
    }
    None
}

/// Chaikin corner cutting, then resampled to even spacing.
fn smooth(path: &[Vec2]) -> Vec<Vec2> {
    let mut pts = path.to_vec();
    for _ in 0..4 {
        if pts.len() < 3 {
            break;
        }
        let mut next = vec![pts[0]];
        for w in pts.windows(2) {
            next.push(w[0].lerp(w[1], 0.25));
            next.push(w[0].lerp(w[1], 0.75));
        }
        next.push(*pts.last().unwrap());
        pts = next;
    }
    // resample
    let mut out = vec![pts[0]];
    let mut carry = 0.0;
    for w in pts.windows(2) {
        let seg = w[1] - w[0];
        let len = seg.length();
        let mut t = POINT_SPACING - carry;
        while t <= len {
            out.push(w[0] + seg * (t / len));
            t += POINT_SPACING;
        }
        carry = len - (t - POINT_SPACING);
    }
    if out.last().unwrap().distance(*pts.last().unwrap()) > 1.0 {
        out.push(*pts.last().unwrap());
    }
    out
}

/// Road surface heights: the ground along the path, smoothed into gentle grades.
fn profile(grid: &HeightGrid, path: &[Vec2]) -> Road {
    let mut h: Vec<f32> = path.iter().map(|p| grid.sample_clamped(p.x, p.y)).collect();
    for _ in 0..3 {
        let r = 10; // ±80 m
        let src = h.clone();
        for k in 0..h.len() {
            let (a, b) = (k.saturating_sub(r), (k + r).min(src.len() - 1));
            h[k] = src[a..=b].iter().sum::<f32>() / (b - a + 1) as f32;
        }
    }
    path.iter().zip(h).map(|(p, y)| Vec3::new(p.x, y, p.y)).collect()
}

/// Levels the terrain under the roads (blending back to the natural ground).
fn flatten(grid: &mut HeightGrid, roads: &[Road]) {
    let index = RoadIndex::new(roads, BLEND_OUTER);
    let n = grid.n;
    for j in 0..n {
        for i in 0..n {
            let p = grid.world_pos(i, j);
            if let Some((q, d)) = index.nearest(p) {
                if d < BLEND_OUTER {
                    let w = 1.0 - smoothstep(BLEND_INNER, BLEND_OUTER, d);
                    let k = j * n + i;
                    grid.heights[k] += (q.y - grid.heights[k]) * w;
                }
            }
        }
    }
}

/// One ribbon mesh per road: gravel shoulder, asphalt, dashed centre line.
pub(super) fn road_mesh(road: &Road) -> Mesh {
    let gravel = Vec4::new(0.36, 0.33, 0.28, 1.0);
    let asphalt = Vec4::new(0.075, 0.075, 0.08, 1.0);
    let paint = Vec4::new(0.3, 0.29, 0.24, 1.0);
    let w = ROAD_HALF_WIDTH;
    // offsets across the road, left to right
    let offsets = [-(w + SHOULDER), -w, -0.12, 0.12, w, w + SHOULDER];
    let cols = offsets.len();
    let mut positions = Vec::with_capacity(road.len() * cols);
    let mut colors = Vec::with_capacity(road.len() * cols);
    for (k, &p) in road.iter().enumerate() {
        let a = road[k.saturating_sub(1)];
        let b = road[(k + 1).min(road.len() - 1)];
        let dir = (b - a).xz().normalize_or(Vec2::X);
        let side = Vec2::new(-dir.y, dir.x);
        let dash = (k / 2) % 3 == 0;
        for (c, &o) in offsets.iter().enumerate() {
            let xz = p.xz() + side * o;
            // shoulders drop a little so they don't float over banked ground
            let y = p.y + if c == 0 || c == cols - 1 { 0.05 } else { 0.3 };
            positions.push([xz.x, y, xz.y]);
            let col = match c {
                0 | 5 => gravel,
                2 | 3 if dash => paint,
                _ => asphalt,
            };
            colors.push(col.to_array());
        }
    }
    let mut indices = Vec::with_capacity((road.len() - 1) * (cols - 1) * 6);
    for k in 0..road.len() as u32 - 1 {
        for c in 0..cols as u32 - 1 {
            let a = k * cols as u32 + c;
            let b = a + 1;
            let d = a + cols as u32;
            let e = d + 1;
            indices.extend_from_slice(&[a, b, d, b, e, d]);
        }
    }
    let count = positions.len();
    // (counter-clockwise seen from above: `side` points left of travel)
    Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::RENDER_WORLD)
        .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, positions)
        .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, vec![[0.0f32, 1.0, 0.0]; count])
        .with_inserted_attribute(Mesh::ATTRIBUTE_COLOR, colors)
        .with_inserted_indices(Indices::U32(indices))
}
