//! A road network across the high-res grid.
//!
//! A handful of flat, low "sites" (where villages would be) are picked from the
//! height grid and joined one at a time: each new site is routed to the nearest
//! point of the network so far with a Dijkstra search whose cost climbs steeply
//! with the grade, so roads follow valley floors and switch back up passes.
//! Each lattice path is then pulled taut wherever a straight line is no dearer
//! than the zig-zag (so flat ground loses the grid's 45° steps but mountain
//! switchbacks stay), given a gentle meander on long flat legs, and rounded into a
//! B-spline. Turn radii follow the leg lengths (wide on the plains, tight hairpins
//! where the legs are short), and a slope-weighted blur opens up any bend left
//! tight on gentle ground. Finally the roads get a gently graded elevation
//! profile, and the terrain is levelled under them before the meshes are built.

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
/// Longest straight leg a path is pulled into (m).
const MAX_LEG: f32 = 3_000.0;
/// A straight leg replaces the lattice path if it costs at most this much more.
const PULL_SLACK: f32 = 1.02;
/// Long legs are cut into pieces about this long (m), each end nudged sideways
/// by up to `MEANDER` of it where the ground allows.
const MEANDER_PIECE: f32 = 600.0;
const MEANDER: f32 = 0.2;
/// Smoothing passes over the finished centreline, averaging over ±`EASE_REACH`
/// points: they open tight bends up to a radius of a few hundred metres on
/// gentle ground only.
const EASE_PASSES: usize = 40;
const EASE_REACH: usize = 8;

/// A road centreline: points ~8 m apart, y = road surface height.
pub(super) type Road = Vec<Vec3>;

/// Plans the network and levels `grid` under it. Returns the centrelines.
pub(super) fn build_roads(grid: &mut HeightGrid) -> Vec<Road> {
    let dry = dry_level();
    // per road: its waypoints, its centreline, and whether its end meets another road
    let mut ways: Vec<Vec<Vec2>> = Vec::new();
    let mut roads: Vec<Road> = Vec::new();
    let mut joins: Vec<bool> = Vec::new();
    for path in plan(grid) {
        let mut way = straighten(grid, &path.nodes, dry);
        // end exactly on the road it joins: that road was pulled off the lattice,
        // sometimes by a few hundred metres where it cut a corner
        let target = path.joins_road.then(|| nearest_on_roads(&roads, *way.last().unwrap())).flatten();
        if let Some((_, q, _)) = target {
            *way.last_mut().unwrap() = q.xz();
        }
        meander(grid, &mut way, dry);
        // arriving at the village at either end of a road: carry on along it as one
        // road, so the two don't meet in a sharp V
        if let Some((r, q, _)) = target {
            let at_start = q.xz().distance(ways[r][0]) < 1.0;
            let at_end = !joins[r] && q.xz().distance(*ways[r].last().unwrap()) < 1.0;
            let rest: Option<Vec<Vec2>> = if at_start {
                Some(ways[r].clone())
            } else if at_end {
                Some(ways[r].iter().rev().copied().collect())
            } else {
                None
            };
            if let Some(rest) = rest {
                // (carrying on backwards along it, the road now ends at its start: a village)
                if !at_start {
                    joins[r] = false;
                }
                way.extend_from_slice(&rest[1..]);
                roads[r] = finish(grid, &way, dry);
                ways[r] = way;
                continue;
            }
        }
        roads.push(finish(grid, &way, dry));
        ways.push(way);
        joins.push(target.is_some());
    }
    // tie each joining road's end into the road it meets, in plan and in height
    // (once all are final: a merge may have nudged the road it meets)
    for i in 0..roads.len() {
        if !joins[i] {
            continue;
        }
        let end = roads[i].last().unwrap().xz();
        let others: Vec<Road> = roads.iter().enumerate().filter(|&(j, _)| j != i).map(|(_, r)| r.clone()).collect();
        let Some((_, q, _)) = nearest_on_roads(&others, end) else { continue };
        let road = &mut roads[i];
        let len = road.len();
        let n = len.min(25);
        let shift = q.xz() - end;
        for k in 0..n {
            let t = smoothstep(0.0, 1.0, k as f32 / n as f32); // 0 at the join
            let p = &mut road[len - 1 - k];
            let xz = p.xz() + shift * (1.0 - t);
            *p = Vec3::new(xz.x, q.y + (p.y - q.y) * t, xz.y);
        }
    }
    flatten(grid, &roads);
    plot(grid, &roads);
    roads
}

/// Waypoints to a finished centreline: splined, eased, evenly spaced, graded.
fn finish(grid: &HeightGrid, way: &[Vec2], dry: f32) -> Road {
    let mut line = spline(way);
    ease(grid, &mut line, dry);
    profile(grid, &resample(&line))
}

/// Nearest point on any road to `p`: the road's index, the point, and its distance.
fn nearest_on_roads(roads: &[Road], p: Vec2) -> Option<(usize, Vec3, f32)> {
    let mut best: Option<(usize, Vec3, f32)> = None;
    for (r, road) in roads.iter().enumerate() {
        for w in road.windows(2) {
            let (q, d) = closest_on_segment(w[0], w[1], p);
            if best.is_none_or(|(.., bd)| d < bd) {
                best = Some((r, q, d));
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

/// A planned route through lattice nodes, and whether it ends on an earlier route
/// (rather than at a lone site).
struct Route {
    nodes: Vec<Vec2>,
    joins_road: bool,
}

fn plan(grid: &HeightGrid) -> Vec<Route> {
    let m = (grid.n - 1) / STRIDE + 1;
    let margin = (EDGE_MARGIN / (grid.spacing * STRIDE as f32)).ceil() as usize;
    let lat = Lattice { grid, m, lo: margin, hi: m - 1 - margin };
    // no sites on the shore or under water
    let dry = super::biome::params().water.as_ref().map_or(f32::MIN, |w| w.level + w.beach + 4.0);

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
                    if h > 750.0 || h < dry || s > 0.08 {
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
    // (the subset of `on_road` that's actually on a route, not a lone site)
    let mut routed = vec![false; m * m];
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
            let &(ei, ej) = nodes.last().unwrap();
            let joins_road = routed[ej * m + ei];
            for &(i, j) in &nodes {
                on_road[j * m + i] = true;
                routed[j * m + i] = true;
            }
            paths.push(Route { nodes: nodes.iter().map(|&(i, j)| lat.pos(i, j)).collect(), joins_road });
        } else {
            // cut off (another island): later sites there can join this one
            on_road[start.1 * m + start.0] = true;
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
    let dry = dry_level();
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
            if lat.h(ni, nj) < dry {
                continue;
            }
            let len = step * if di != 0 && dj != 0 { std::f32::consts::SQRT_2 } else { 1.0 };
            let nc = c + weight(len, h, lat.h(ni, nj));
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

/// Roads don't cross water (or run along the very edge of it).
fn dry_level() -> f32 {
    super::biome::params().water.as_ref().map_or(f32::MIN, |w| w.level + 2.0)
}

/// What a stretch of road `len` long from height `h0` to `h1` costs: gentle
/// grades are cheap, steep ones very expensive, and a little pull toward low
/// ground keeps roads in the valleys.
fn weight(len: f32, h0: f32, h1: f32) -> f32 {
    let grade = (h1 - h0).abs() / len;
    let mut w = len * (1.0 + 60.0 * grade * grade + h1.max(0.0) * 0.0006);
    if grade > 0.14 {
        w += len * 40.0 * (grade - 0.14) / 0.1;
    }
    w
}

/// The cost of a straight leg from `a` to `b`, sampled every grid cell along it
/// (infinite if it dips below `dry`).
fn leg_cost(grid: &HeightGrid, a: Vec2, b: Vec2, dry: f32) -> f32 {
    leg_cost_within(grid, a, b, dry, f32::INFINITY)
}

/// `leg_cost`, giving up (infinite) as soon as it passes `budget`.
fn leg_cost_within(grid: &HeightGrid, a: Vec2, b: Vec2, dry: f32, budget: f32) -> f32 {
    let n = (a.distance(b) / grid.spacing).ceil().max(1.0) as usize;
    let step = a.distance(b) / n as f32;
    let mut h0 = grid.sample_clamped(a.x, a.y);
    let mut cost = 0.0;
    for s in 1..=n {
        let p = a.lerp(b, s as f32 / n as f32);
        let h1 = grid.sample_clamped(p.x, p.y);
        cost += weight(step, h0, h1);
        if h1 < dry || cost > budget {
            return f32::INFINITY;
        }
        h0 = h1;
    }
    cost
}

/// String-pulls a lattice path: from each waypoint, jump to the farthest later
/// node (within `MAX_LEG`) whose straight line costs no more than the path does.
/// Flat ground loses the lattice's 0/45/90° zig-zags; on steep ground a straight
/// line climbs too hard, so the switchbacks stay.
fn straighten(grid: &HeightGrid, path: &[Vec2], dry: f32) -> Vec<Vec2> {
    let mut along = vec![0.0; path.len()];
    for k in 1..path.len() {
        along[k] = along[k - 1] + leg_cost(grid, path[k - 1], path[k], f32::MIN);
    }
    let mut out = vec![path[0]];
    let mut a = 0;
    while a + 1 < path.len() {
        let next = (a + 2..path.len())
            .rev()
            .filter(|&k| path[a].distance(path[k]) <= MAX_LEG)
            .find(|&k| leg_cost_within(grid, path[a], path[k], dry, (along[k] - along[a]) * PULL_SLACK).is_finite())
            .unwrap_or(a + 1);
        out.push(path[next]);
        a = next;
    }
    out
}

/// Cuts long legs into pieces and nudges the cuts sideways a little where that
/// costs next to nothing (flat, dry ground), so long straights become lazy bends.
fn meander(grid: &HeightGrid, way: &mut Vec<Vec2>, dry: f32) {
    let seed = super::terrain_seed() ^ 0x6d65_616e;
    let mut out = vec![way[0]];
    for w in way.windows(2) {
        let (a, b) = (w[0], w[1]);
        let len = a.distance(b);
        let n = (len / MEANDER_PIECE).round() as usize;
        if n >= 2 {
            let dir = (b - a) / len;
            let side = Vec2::new(-dir.y, dir.x) * (len / n as f32) * MEANDER;
            let cuts: Vec<Vec2> = (1..n)
                .map(|k| {
                    let p = a.lerp(b, k as f32 / n as f32);
                    p + side * super::hash2(p.x as i32, p.y as i32, seed).x
                })
                .collect();
            // keep the bends only if the leg stays as cheap (try half as wide first)
            let budget = leg_cost(grid, a, b, dry) * 1.05;
            let cost = |scale: f32| {
                let pts: Vec<Vec2> = cuts.iter().enumerate().map(|(k, &c)| {
                    let straight = a.lerp(b, (k + 1) as f32 / n as f32);
                    straight + (c - straight) * scale
                }).collect();
                let mut c = 0.0;
                let mut prev = a;
                for &p in pts.iter().chain(std::iter::once(&b)) {
                    c += leg_cost(grid, prev, p, dry);
                    prev = p;
                }
                (c <= budget).then_some(pts)
            };
            if let Some(pts) = cost(1.0).or_else(|| cost(0.5)) {
                out.extend(&pts[..]);
            } else {
                out.extend((1..n).map(|k| a.lerp(b, k as f32 / n as f32)));
            }
        }
        out.push(b);
    }
    *way = out;
}

/// Relaxes the centreline toward its neighbours where the ground is gentle,
/// which widens tight bends there; switchbacks on steep ground stay put. The ends
/// are fixed, and no point is moved into water.
fn ease(grid: &HeightGrid, line: &mut [Vec2], dry: f32) {
    let n = line.len();
    if n < 3 {
        return;
    }
    let d = 4.0 * grid.spacing;
    let give: Vec<f32> = line
        .iter()
        .map(|p| {
            let dx = grid.sample_clamped(p.x + d, p.y) - grid.sample_clamped(p.x - d, p.y);
            let dz = grid.sample_clamped(p.x, p.y + d) - grid.sample_clamped(p.x, p.y - d);
            let slope = Vec2::new(dx, dz).length() / (2.0 * d);
            // (switchbacks only happen where the ground is steeper than a road climbs)
            1.0 - smoothstep(0.08, 0.18, slope)
        })
        .collect();
    // repeated box averages over ±EASE_REACH points (via prefix sums): a blur
    // about a quarter of a kilometre wide where the ground is gentle
    let r = EASE_REACH;
    let mut sum = vec![Vec2::ZERO; n + 1];
    for _ in 0..EASE_PASSES {
        for k in 0..n {
            sum[k + 1] = sum[k] + line[k];
        }
        for k in 1..n - 1 {
            let (lo, hi) = (k.saturating_sub(r), (k + r).min(n - 1));
            let avg = (sum[hi + 1] - sum[lo]) / (hi - lo + 1) as f32;
            let p = line[k] + (avg - line[k]) * give[k];
            if grid.sample_clamped(p.x, p.y) >= dry {
                line[k] = p;
            }
        }
    }
}

/// Chaikin corner cutting (converging on a quadratic B-spline through the
/// waypoints' legs), resampled to even spacing. Each bend is spread over half of
/// each leg either side of it: sweeping where the legs are long (the plains),
/// tight hairpins where they're short, and the lattice's 64 m jags rounded off.
fn spline(way: &[Vec2]) -> Vec<Vec2> {
    let mut pts = way.to_vec();
    for _ in 0..5 {
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
    resample(&pts)
}

/// Points `POINT_SPACING` apart along a polyline (keeping both ends).
fn resample(pts: &[Vec2]) -> Vec<Vec2> {
    let mut out = vec![pts[0]];
    let mut carry = 0.0;
    for w in pts.windows(2) {
        let seg = w[1] - w[0];
        let len = seg.length();
        if len < 1e-6 {
            continue;
        }
        let mut t = POINT_SPACING - carry;
        while t <= len {
            out.push(w[0] + seg * (t / len));
            t += POINT_SPACING;
        }
        carry = len - (t - POINT_SPACING);
    }
    let end = *pts.last().unwrap();
    if out.last().unwrap().distance(end) > 1.0 {
        out.push(end);
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
/// The road's ribbon: gravel shoulders, asphalt, and a dashed centre line.
///
/// `surface`: the height grid that's actually drawn under the road when it isn't
/// the (road-levelled) near grid (the low-memory scene draws the 64 m grid), so
/// the ribbon is lifted clear of it.
pub(super) fn road_mesh(road: &Road, surface: Option<&HeightGrid>) -> Mesh {
    let gravel = [0.36, 0.33, 0.28, 1.0];
    let asphalt = [0.075, 0.075, 0.08, 1.0];
    let paint = [0.3, 0.29, 0.24, 1.0];
    let w = ROAD_HALF_WIDTH;
    const LINE: f32 = 0.15; // centre line half-width
    // offsets across the road, left to right
    let offsets = [-(w + SHOULDER), -w, -LINE, LINE, w, w + SHOULDER];
    let cols = offsets.len();
    let rows = road.len();
    let mut positions: Vec<[f32; 3]> = Vec::with_capacity(rows * cols + rows * 4);
    let mut colors: Vec<[f32; 4]> = Vec::with_capacity(positions.capacity());
    for (k, &p) in road.iter().enumerate() {
        let a = road[k.saturating_sub(1)];
        let b = road[(k + 1).min(rows - 1)];
        let dir = (b - a).xz().normalize_or(Vec2::X);
        let side = Vec2::new(-dir.y, dir.x);
        for (c, &o) in offsets.iter().enumerate() {
            let xz = p.xz() + side * o;
            // shoulders drop a little so they don't float over banked ground
            let lift = if c == 0 || c == cols - 1 { 0.05 } else { 0.3 };
            let ground = surface.and_then(|g| g.sample_inside(xz.x, xz.y)).map_or(p.y, |h| h.max(p.y));
            positions.push([xz.x, ground + lift, xz.y]);
            colors.push(match c {
                0 | 5 => gravel,
                _ => asphalt,
            });
        }
    }
    let mut indices = Vec::with_capacity((rows - 1) * (cols - 1) * 6);
    let quad = |indices: &mut Vec<u32>, a: u32, b: u32, d: u32, e: u32| indices.extend_from_slice(&[a, b, d, b, e, d]);
    for k in 0..rows as u32 - 1 {
        for c in 0..cols as u32 - 1 {
            let a = k * cols as u32 + c;
            if c == 2 {
                // the centre line gets its own corners, so each segment is all paint
                // or all asphalt (shared corners would smear the paint across the road)
                let dash = (k / 2) % 3 == 0;
                let base = positions.len() as u32;
                for v in [a, a + 1, a + cols as u32, a + cols as u32 + 1] {
                    positions.push(positions[v as usize]);
                    colors.push(if dash { paint } else { asphalt });
                }
                quad(&mut indices, base, base + 1, base + 2, base + 3);
            } else {
                quad(&mut indices, a, a + 1, a + cols as u32, a + cols as u32 + 1);
            }
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

/// Dev: `FLIGHT_SIM_ROAD_PLOT=/abs/out.ppm` writes a plan view of the near grid
/// (hillshade, water, roads in red) at 16 m per pixel after the roads are built.
fn plot(grid: &HeightGrid, roads: &[Road]) {
    let Ok(path) = std::env::var("FLIGHT_SIM_ROAD_PLOT") else { return };
    let px = 16.0;
    let size = ((grid.n - 1) as f32 * grid.spacing / px) as usize;
    let origin = grid.origin;
    let water = super::biome::params().water.as_ref().map(|w| w.level);
    let mut img = vec![0u8; size * size * 3];
    for y in 0..size {
        for x in 0..size {
            let (wx, wz) = (origin.x + x as f32 * px, origin.y + y as f32 * px);
            let h = grid.sample_clamped(wx, wz);
            let (hx, hz) = (grid.sample_clamped(wx + px, wz), grid.sample_clamped(wx, wz + px));
            let n = Vec3::new(h - hx, px, h - hz).normalize();
            let shade = n.dot(Vec3::new(-0.5, 0.7, -0.5).normalize()).clamp(0.0, 1.0);
            let base = 60.0 + 160.0 * shade + (h / 2_000.0).clamp(0.0, 1.0) * 35.0;
            let c = if water.is_some_and(|l| h <= l + 0.5) { [40.0, 70.0, 140.0] } else { [base; 3] };
            img[(y * size + x) * 3..][..3].copy_from_slice(&c.map(|v| v.min(255.0) as u8));
        }
    }
    for road in roads {
        for w in road.windows(2) {
            let steps = (w[0].xz().distance(w[1].xz()) / (px * 0.5)).ceil().max(1.0) as usize;
            for s in 0..=steps {
                let p = w[0].xz().lerp(w[1].xz(), s as f32 / steps as f32);
                let (x, y) = (((p.x - origin.x) / px) as isize, ((p.y - origin.y) / px) as isize);
                if (0..size as isize).contains(&x) && (0..size as isize).contains(&y) {
                    img[(y as usize * size + x as usize) * 3..][..3].copy_from_slice(&[230, 30, 30]);
                }
            }
        }
    }
    let mut out = format!("P6\n{size} {size}\n255\n").into_bytes();
    out.extend_from_slice(&img);
    if let Err(e) = std::fs::write(&path, out) {
        warn!("road plot: {e}");
    }
}
