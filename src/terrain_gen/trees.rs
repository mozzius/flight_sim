//! Forests over the high-res grid.
//!
//! Trees are scattered on a jittered lattice wherever the baked terrain colour
//! says "forest" (the same rules as `terrain_color`: wooded slopes below the
//! tree line, patches on the valley floors, none on rock, snow or roads).
//! Trees are planted and meshed per 512 m chunk only when the camera comes
//! near (several chunks at once on worker threads), in two levels of detail
//! that dither into each other, and dropped again when it leaves.

use std::collections::HashMap;

use bevy::{
    asset::RenderAssetUsages,
    camera::visibility::VisibilityRange,
    light::NotShadowCaster,
    mesh::{Indices, PrimitiveTopology},
    prelude::*,
};

use super::{
    EROSION_OCTAVES, SceneryMaterial, noised, octaves_at, roads::RoadIndex, sample_terrain,
    smoothstep, srgb,
};
use crate::{camera::MainCamera, world::Terrain};

const CANDIDATE_SPACING: f32 = 13.0;
const CHUNK: f32 = 512.0;
/// Chunks are built within this (horizontal) distance of the camera, and dropped beyond the second.
const LOAD_RADIUS: f32 = 6_400.0;
const UNLOAD_RADIUS: f32 = 7_200.0;
const CHUNKS_PER_FRAME: usize = 16;
/// Detailed trees out to here, simple ones beyond, none past `FAR_END`. Hard
/// switches (zero-width margins): Bevy's dithered crossfade renders chunks
/// black mid-fade with this custom material, and stipples from above anyway.
const LOD_SWITCH: std::ops::Range<f32> = 1_800.0..1_800.0;
const FAR_END: std::ops::Range<f32> = 6_200.0..6_200.0;

#[derive(Clone, Copy, Default)]
struct Tree {
    /// Relative to the chunk centre.
    pos: Vec3,
    height: f32,
    /// Crown radius as a fraction of the height.
    width: f32,
    yaw: f32,
    conifer: bool,
    color: Vec3,
}

#[derive(Resource)]
pub(super) struct TreeField {
    roads: RoadIndex,
    material: Handle<SceneryMaterial>,
    /// The square trees grow in (the high-res grid, minus a margin).
    min: Vec2,
    max: Vec2,
}

impl TreeField {
    pub(super) fn new(roads: RoadIndex, material: Handle<SceneryMaterial>, center_half_extent: f32) -> Self {
        // stay off the grid's edge, where it meets the coarser terrain
        let half = center_half_extent - 300.0;
        Self { roads, material, min: Vec2::splat(-half), max: Vec2::splat(half) }
    }
}

#[derive(Resource, Default)]
pub(super) struct LoadedTrees(HashMap<(i32, i32), Vec<Entity>>);

fn hash(i: u32, j: u32, seed: u32) -> u32 {
    let mut h = i.wrapping_mul(0x8da6_b343) ^ j.wrapping_mul(0xd816_3841) ^ seed.wrapping_mul(0xcb1a_b31f);
    h ^= h >> 13;
    h = h.wrapping_mul(0x5bd1_e995);
    h ^ (h >> 15)
}

fn rand01(i: u32, j: u32, seed: u32) -> f32 {
    // different maps scatter their trees differently too
    let seed = seed ^ super::terrain_seed().rotate_left(7);
    (hash(i, j, seed) >> 8) as f32 / (1u32 << 24) as f32
}

fn chunk_key(xz: Vec2) -> (i32, i32) {
    ((xz.x / CHUNK).floor() as i32, (xz.y / CHUNK).floor() as i32)
}

fn chunk_center(key: (i32, i32)) -> Vec2 {
    (Vec2::new(key.0 as f32, key.1 as f32) + 0.5) * CHUNK
}

/// How wooded the ground is (0..1), mirroring `terrain_color`'s forest and rock rules.
fn forest_cover(h: f32, normal: Vec3, ridge: f32, jitter: f32) -> f32 {
    let flatness = normal.y;
    let ridge = ridge.clamp(-1.0, 1.0);
    let crease = (-ridge).max(0.0);
    let tree_line = 1650.0 + jitter * 250.0 + crease * 250.0 - ridge.max(0.0) * 200.0;
    let woods = smoothstep(0.15, 0.45, jitter + crease * 0.3);
    let forest = smoothstep(0.985, 0.94 - crease * 0.05, flatness).max(woods)
        * smoothstep(tree_line + 150.0, tree_line - 150.0, h);
    let steep_start = 0.70 + ridge * 0.06 + jitter * 0.04 + smoothstep(1500.0, 3000.0, h) * 0.08;
    let rock = smoothstep(steep_start, steep_start - 0.10, flatness);
    // a few lone trees in the meadows too
    let scattered = 0.03 * smoothstep(tree_line, tree_line - 300.0, h);
    (forest * (1.0 - rock)).max(scattered * (1.0 - rock))
}

/// Plants one chunk's trees (positions relative to the chunk centre).
fn plant_chunk(key: (i32, i32), field: &TreeField, terrain: &Terrain) -> Vec<Tree> {
    let c = chunk_center(key);
    let lo = c - CHUNK * 0.5;
    // global lattice indices, so neighbouring chunks agree
    let (i0, j0) = ((lo.x / CANDIDATE_SPACING).floor() as i32, (lo.y / CANDIDATE_SPACING).floor() as i32);
    let per_side = (CHUNK / CANDIDATE_SPACING).ceil() as i32 + 1;
    let mut trees = Vec::new();
    for j in j0..j0 + per_side {
        for i in i0..i0 + per_side {
            let (iu, ju) = (i as u32, j as u32);
            let jit = Vec2::new(rand01(iu, ju, 1), rand01(iu, ju, 2)) - 0.5;
            let xz = (Vec2::new(i as f32, j as f32) + 0.5 + jit * 0.9) * CANDIDATE_SPACING;
            if chunk_key(xz) != key || xz.cmplt(field.min).any() || xz.cmpgt(field.max).any() {
                continue;
            }
            if let Some(mut tree) = plant_one(iu, ju, xz, field, terrain) {
                tree.pos -= Vec3::new(c.x, 0.0, c.y);
                trees.push(tree);
            }
        }
    }
    trees
}

fn plant_one(iu: u32, ju: u32, xz: Vec2, field: &TreeField, terrain: &Terrain) -> Option<Tree> {
    let octaves = octaves_at(xz.x, xz.y).max(EROSION_OCTAVES as f32);
    let t = sample_terrain(xz.x, xz.y, octaves);
    let step = 8.0;
    let hx = sample_terrain(xz.x + step, xz.y, octaves).height;
    let hz = sample_terrain(xz.x, xz.y + step, octaves).height;
    let normal = Vec3::new(-(hx - t.height) / step, 1.0, -(hz - t.height) / step).normalize();
    let jitter = noised(xz / 900.0, 77).x + 0.5 * noised(xz / 310.0, 78).x;
    let cover = forest_cover(t.height, normal, t.ridge_map, jitter);
    if rand01(iu, ju, 3) >= cover * 0.92 {
        return None;
    }
    if field.roads.nearest(xz).is_some_and(|(_, d)| d < super::roads::ROAD_HALF_WIDTH + 7.0) {
        return None;
    }
    let ground = terrain.height_at(xz.x, xz.y);
    // broadleaf woods on the valley floors, conifers up the slopes
    let conifer = rand01(iu, ju, 4) < smoothstep(500.0, 1100.0, ground) * 0.85 + 0.15;
    let r = rand01(iu, ju, 5);
    let shade = 0.8 + 0.35 * rand01(iu, ju, 6);
    let color = if conifer {
        srgb(0.15, 0.22, 0.11) * shade
    } else if r < 0.05 {
        srgb(0.48, 0.38, 0.12) * shade // turning early
    } else {
        srgb(0.22, 0.31, 0.12) * shade
    };
    Some(Tree {
        pos: Vec3::new(xz.x, ground, xz.y),
        height: if conifer { 14.0 + 12.0 * r } else { 10.0 + 8.0 * r },
        width: if conifer { 0.28 + 0.08 * rand01(iu, ju, 7) } else { 0.45 + 0.12 * rand01(iu, ju, 7) },
        yaw: rand01(iu, ju, 8) * std::f32::consts::TAU,
        conifer,
        color,
    })
}

// ---------------------------------------------------------------------------------------------
// Meshes

#[derive(Default)]
struct MeshBuilder {
    positions: Vec<[f32; 3]>,
    normals: Vec<[f32; 3]>,
    colors: Vec<[f32; 4]>,
    indices: Vec<u32>,
}

impl MeshBuilder {
    fn vert(&mut self, p: Vec3, n: Vec3, c: Vec3) -> u32 {
        self.positions.push(p.to_array());
        self.normals.push(n.normalize_or(Vec3::Y).to_array());
        self.colors.push([c.x, c.y, c.z, 1.0]);
        (self.positions.len() - 1) as u32
    }

    /// A cone (or, with `bottom`, a double cone) around the tree's axis.
    #[allow(clippy::too_many_arguments)]
    fn crown(&mut self, t: &Tree, sides: usize, base: f32, top: f32, radius: f32, bottom: Option<f32>, color: Vec3) {
        let (h, r) = (t.height, radius * t.height);
        let apex = self.vert(t.pos + Vec3::Y * top * h, Vec3::Y, color * 1.15);
        let low = bottom.map(|b| self.vert(t.pos + Vec3::Y * b * h, Vec3::NEG_Y, color * 0.6));
        let first = self.positions.len() as u32;
        for k in 0..sides {
            let a = t.yaw + k as f32 / sides as f32 * std::f32::consts::TAU;
            let radial = Vec3::new(a.cos(), 0.0, a.sin());
            let up = if bottom.is_some() { 0.25 } else { 0.6 };
            self.vert(t.pos + Vec3::Y * base * h + radial * r, radial + Vec3::Y * up, color);
        }
        for k in 0..sides as u32 {
            let (a, b) = (first + k, first + (k + 1) % sides as u32);
            // counter-clockwise seen from outside
            self.indices.extend_from_slice(&[apex, b, a]);
            if let Some(low) = low {
                self.indices.extend_from_slice(&[low, a, b]);
            }
        }
    }

    fn trunk(&mut self, t: &Tree, top: f32) {
        let color = srgb(0.24, 0.17, 0.11);
        let r = 0.022 * t.height;
        let first = self.positions.len() as u32;
        for k in 0..3 {
            let a = t.yaw + k as f32 / 3.0 * std::f32::consts::TAU;
            let radial = Vec3::new(a.cos(), 0.0, a.sin());
            // sunk into the ground so it never floats on a slope
            self.vert(t.pos + radial * r - Vec3::Y * 2.0, radial, color * 0.7);
            self.vert(t.pos + radial * r + Vec3::Y * top * t.height, radial, color);
        }
        for k in 0..3u32 {
            let (b0, t0) = (first + k * 2, first + k * 2 + 1);
            let (b1, t1) = (first + (k + 1) % 3 * 2, first + (k + 1) % 3 * 2 + 1);
            self.indices.extend_from_slice(&[b0, t0, b1, b1, t0, t1]);
        }
    }

    fn build(self) -> Mesh {
        Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::RENDER_WORLD)
            .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, self.positions)
            .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, self.normals)
            .with_inserted_attribute(Mesh::ATTRIBUTE_COLOR, self.colors)
            .with_inserted_indices(Indices::U32(self.indices))
    }
}

fn chunk_meshes(trees: &[Tree]) -> (Mesh, Mesh) {
    let mut near = MeshBuilder::default();
    let mut far = MeshBuilder::default();
    for t in trees {
        if t.conifer {
            near.trunk(t, 0.25);
            near.crown(t, 6, 0.15, 0.7, t.width, None, t.color);
            near.crown(t, 6, 0.42, 1.0, t.width * 0.7, None, t.color * 1.08);
            far.crown(t, 4, 0.1, 1.0, t.width, None, t.color);
        } else {
            near.trunk(t, 0.4);
            near.crown(t, 6, 0.62, 1.0, t.width, Some(0.3), t.color);
            far.crown(t, 4, 0.6, 1.0, t.width, Some(0.3), t.color);
        }
    }
    (near.build(), far.build())
}

pub(super) fn stream_trees(
    mut commands: Commands,
    field: Option<Res<TreeField>>,
    mut loaded: ResMut<LoadedTrees>,
    mut meshes: ResMut<Assets<Mesh>>,
    terrain: Res<Terrain>,
    camera: Query<&GlobalTransform, With<MainCamera>>,
) {
    let (Some(field), Ok(cam)) = (field, camera.single()) else {
        return;
    };
    let cam = cam.translation().xz();

    loaded.0.retain(|&key, entities| {
        if chunk_center(key).distance(cam) > UNLOAD_RADIUS {
            for e in entities.iter() {
                commands.entity(*e).despawn();
            }
            false
        } else {
            true
        }
    });

    let reach = (LOAD_RADIUS / CHUNK).ceil() as i32;
    let center = chunk_key(cam);
    let mut wanted: Vec<((i32, i32), f32)> = Vec::new();
    for dj in -reach..=reach {
        for di in -reach..=reach {
            let key = (center.0 + di, center.1 + dj);
            let d = chunk_center(key).distance(cam);
            let (lo, hi) = (chunk_center(key) - CHUNK * 0.5, chunk_center(key) + CHUNK * 0.5);
            let overlaps = hi.cmpgt(field.min).all() && lo.cmplt(field.max).all();
            if d <= LOAD_RADIUS && overlaps && !loaded.0.contains_key(&key) {
                wanted.push((key, d));
            }
        }
    }
    wanted.sort_by(|a, b| a.1.total_cmp(&b.1));
    let batch: Vec<(i32, i32)> = wanted.into_iter().take(CHUNKS_PER_FRAME).map(|(k, _)| k).collect();
    // plant and mesh this frame's chunks on worker threads
    let built: Vec<Option<(Mesh, Mesh)>> = std::thread::scope(|scope| {
        let handles: Vec<_> = batch
            .iter()
            .map(|&key| {
                let (field, terrain) = (&*field, &*terrain);
                scope.spawn(move || {
                    let trees = plant_chunk(key, field, terrain);
                    (!trees.is_empty()).then(|| chunk_meshes(&trees))
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    for (key, meshes_for_chunk) in batch.into_iter().zip(built) {
        // an empty chunk is still "loaded", it just has nothing to draw
        let Some((near, far)) = meshes_for_chunk else {
            loaded.0.insert(key, Vec::new());
            continue;
        };
        let c = chunk_center(key);
        let transform = Transform::from_xyz(c.x, 0.0, c.y);
        let near = commands
            .spawn((
                Mesh3d(meshes.add(near)),
                MeshMaterial3d(field.material.clone()),
                transform,
                VisibilityRange {
                    start_margin: 0.0..0.0,
                    end_margin: LOD_SWITCH,
                    use_aabb: true,
                },
            ))
            .id();
        let far = commands
            .spawn((
                Mesh3d(meshes.add(far)),
                MeshMaterial3d(field.material.clone()),
                transform,
                VisibilityRange {
                    start_margin: LOD_SWITCH,
                    end_margin: FAR_END,
                    use_aabb: true,
                },
                // far trees only receive shadows (casting them is costly and invisible out here)
                NotShadowCaster,
            ))
            .id();
        loaded.0.insert(key, vec![near, far]);
    }
}
