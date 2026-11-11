//! The static world: city, ground, sun, sky, and a height grid used for
//! collision (aircraft, bullets, missiles) and AI terrain avoidance.

use bevy::{
    light::{
        CascadeShadowConfigBuilder, light_consts::lux,
        atmosphere::{Atmosphere, ScatteringMedium},
    },
    mesh::{Indices, VertexAttributeValues},
    prelude::*,
};

pub struct WorldPlugin;

impl Plugin for WorldPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Terrain>()
            .add_systems(Startup, setup)
            .add_systems(Update, build_terrain_when_loaded);
    }
}

/// Marks the root of the city scene so we can find its meshes once loaded.
#[derive(Component)]
struct City;

/// Cell size of the collision height grid, in metres.
const CELL: f32 = 8.0;

/// Max-height grid over the city. Everything outside it is flat ground at y = 0.
#[derive(Resource, Default)]
pub struct Terrain {
    ready: bool,
    origin: Vec2,
    width: usize,
    depth: usize,
    heights: Vec<f32>,
}

impl Terrain {
    /// Height of the ground or rooftop at a world (x, z) position.
    pub fn height_at(&self, x: f32, z: f32) -> f32 {
        if !self.ready {
            return 0.0;
        }
        let cx = ((x - self.origin.x) / CELL).floor();
        let cz = ((z - self.origin.y) / CELL).floor();
        if cx < 0.0 || cz < 0.0 || cx as usize >= self.width || cz as usize >= self.depth {
            return 0.0;
        }
        self.heights[cz as usize * self.width + cx as usize]
    }

    /// Whether a world position is inside the ground or a building.
    pub fn is_solid(&self, p: Vec3) -> bool {
        p.y <= self.height_at(p.x, p.z)
    }

    /// Highest obstacle along a horizontal-ish line, sampled every cell.
    /// Used by the AI to look ahead for buildings.
    pub fn max_height_along(&self, from: Vec3, to: Vec3) -> f32 {
        let steps = ((to - from).xz().length() / CELL).ceil().max(1.0) as usize;
        (0..=steps)
            .map(|i| {
                let p = from.lerp(to, i as f32 / steps as f32);
                self.height_at(p.x, p.z)
            })
            .fold(0.0, f32::max)
    }

    /// First point along a segment that is inside terrain, if any.
    pub fn segment_hit(&self, from: Vec3, to: Vec3) -> Option<Vec3> {
        let steps = ((to - from).length() / (CELL * 0.5)).ceil().max(1.0) as usize;
        (1..=steps)
            .map(|i| from.lerp(to, i as f32 / steps as f32))
            .find(|p| self.is_solid(*p))
    }
}

fn setup(
    mut commands: Commands,
    asset_server: Res<AssetServer>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut scattering_mediums: ResMut<Assets<ScatteringMedium>>,
) {
    // the city model is authored in metres (~16 x 10 km, towers up to ~430 m)
    commands.spawn((
        City,
        WorldAssetRoot(
            asset_server.load(GltfAssetLabel::Scene(0).from_asset("models/cityscape/scene.gltf")),
        ),
    ));

    // flat ground stretching out past the city
    commands.spawn((
        Mesh3d(meshes.add(Plane3d::default().mesh().size(400_000.0, 400_000.0))),
        MeshMaterial3d(materials.add(StandardMaterial {
            base_color: Color::srgb(0.42, 0.45, 0.38),
            perceptual_roughness: 1.0,
            ..default()
        })),
        Transform::from_xyz(0.0, -0.5, 0.0),
    ));

    // sun
    commands.spawn((
        DirectionalLight {
            shadow_maps_enabled: true,
            illuminance: lux::RAW_SUNLIGHT,
            ..default()
        },
        Transform::from_xyz(4.0, 10.0, 10.0).looking_at(Vec3::ZERO, Vec3::Y),
        CascadeShadowConfigBuilder {
            num_cascades: 3,
            maximum_distance: 3_000.0,
            first_cascade_far_bound: 120.0,
            ..default()
        }
        .build(),
    ));

    // the planet's atmosphere (centered below the origin by default)
    commands.spawn(Atmosphere::earth(
        scattering_mediums.add(ScatteringMedium::default()),
    ));
}

/// Size of the render tiles the city is re-chunked into, in metres.
const TILE: f32 = 1_000.0;

/// Once the city meshes are loaded:
/// 1. re-chunk them into 1 km tiles. The model ships as a few meshes that
///    each span the whole city, which defeats frustum culling (every frame,
///    and every shadow cascade, drew all 1.5M triangles);
/// 2. rasterise their top surfaces into the collision height grid.
#[allow(clippy::too_many_arguments)]
fn build_terrain_when_loaded(
    mut commands: Commands,
    mut terrain: ResMut<Terrain>,
    city: Query<Entity, With<City>>,
    children: Query<&Children>,
    mesh_query: Query<(&Mesh3d, &GlobalTransform, &MeshMaterial3d<StandardMaterial>)>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut frames_seen: Local<u32>,
) {
    if terrain.ready {
        return;
    }
    let Ok(city) = city.single() else {
        return;
    };
    let parts: Vec<_> = children
        .iter_descendants(city)
        .filter_map(|e| mesh_query.get(e).ok())
        .map(|(m, gt, mat)| (m.0.clone(), *gt, mat.0.clone()))
        .collect();
    if parts.is_empty() || parts.iter().any(|(m, _, _)| meshes.get(m).is_none()) {
        return;
    }
    // wait a couple of frames so GlobalTransforms have propagated
    *frames_seen += 1;
    if *frames_seen < 3 {
        return;
    }

    // collect world-space triangles (with normals), grouped into tiles by material
    let mut tris: Vec<[Vec3; 3]> = Vec::new();
    let mut tiles: std::collections::HashMap<(i32, i32, AssetId<StandardMaterial>), (Handle<StandardMaterial>, Vec<[f32; 3]>, Vec<[f32; 3]>)> =
        std::collections::HashMap::new();
    for (mesh_handle, gt, material) in &parts {
        let mesh = meshes.get(mesh_handle).unwrap();
        let Some(VertexAttributeValues::Float32x3(positions)) =
            mesh.attribute(Mesh::ATTRIBUTE_POSITION)
        else {
            continue;
        };
        let normals = match mesh.attribute(Mesh::ATTRIBUTE_NORMAL) {
            Some(VertexAttributeValues::Float32x3(n)) => Some(n),
            _ => None,
        };
        let normal_matrix = gt.affine().matrix3.inverse().transpose();
        let world: Vec<Vec3> = positions
            .iter()
            .map(|p| gt.transform_point(Vec3::from(*p)))
            .collect();
        let world_normals: Vec<Vec3> = normals.map_or_else(Vec::new, |n| {
            n.iter()
                .map(|n| (normal_matrix * Vec3A::from(*n)).normalize_or_zero().into())
                .collect()
        });
        let indices: Vec<usize> = match mesh.indices() {
            Some(Indices::U16(i)) => i.iter().map(|&i| i as usize).collect(),
            Some(Indices::U32(i)) => i.iter().map(|&i| i as usize).collect(),
            None => (0..world.len()).collect(),
        };
        for t in indices.chunks_exact(3) {
            let tri = [world[t[0]], world[t[1]], world[t[2]]];
            tris.push(tri);
            let centre = (tri[0] + tri[1] + tri[2]) / 3.0;
            let key = (
                (centre.x / TILE).floor() as i32,
                (centre.z / TILE).floor() as i32,
                material.id(),
            );
            let tile = tiles
                .entry(key)
                .or_insert_with(|| (material.clone(), Vec::new(), Vec::new()));
            for &i in t {
                tile.1.push(world[i].to_array());
                tile.2.push(
                    world_normals
                        .get(i)
                        .copied()
                        .unwrap_or(Vec3::Y)
                        .to_array(),
                );
            }
        }
    }

    let tile_count = tiles.len();
    for (_, (material, positions, normals)) in tiles {
        let mut mesh = Mesh::new(
            bevy::mesh::PrimitiveTopology::TriangleList,
            bevy::asset::RenderAssetUsages::RENDER_WORLD,
        );
        mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, positions);
        mesh.insert_attribute(Mesh::ATTRIBUTE_NORMAL, normals);
        commands.spawn((
            Mesh3d(meshes.add(mesh)),
            MeshMaterial3d(material),
            Transform::IDENTITY,
        ));
    }
    // the original whole-city meshes are no longer drawn
    commands.entity(city).insert(Visibility::Hidden);
    info!("re-chunked city into {tile_count} tiles");

    let (mut mn, mut mx) = (Vec2::splat(f32::MAX), Vec2::splat(f32::MIN));
    for t in &tris {
        for p in t {
            mn = mn.min(p.xz());
            mx = mx.max(p.xz());
        }
    }
    let width = ((mx.x - mn.x) / CELL).ceil() as usize + 1;
    let depth = ((mx.y - mn.y) / CELL).ceil() as usize + 1;
    let mut heights = vec![0.0f32; width * depth];

    for [a, b, c] in &tris {
        // skip walls: they have no footprint, roofs cover the cells
        let (a2, b2, c2) = (a.xz(), b.xz(), c.xz());
        let area = (b2 - a2).perp_dot(c2 - a2);
        if area.abs() < 1e-3 {
            continue;
        }
        let lo = a2.min(b2).min(c2);
        let hi = a2.max(b2).max(c2);
        let x0 = ((lo.x - mn.x) / CELL).floor() as usize;
        let x1 = (((hi.x - mn.x) / CELL).ceil() as usize).min(width - 1);
        let z0 = ((lo.y - mn.y) / CELL).floor() as usize;
        let z1 = (((hi.y - mn.y) / CELL).ceil() as usize).min(depth - 1);
        for cz in z0..=z1 {
            for cx in x0..=x1 {
                let p = mn + (Vec2::new(cx as f32, cz as f32) + 0.5) * CELL;
                // barycentric coordinates
                let w0 = (b2 - p).perp_dot(c2 - p) / area;
                let w1 = (c2 - p).perp_dot(a2 - p) / area;
                let w2 = 1.0 - w0 - w1;
                if w0 < -0.01 || w1 < -0.01 || w2 < -0.01 {
                    continue;
                }
                let y = a.y * w0 + b.y * w1 + c.y * w2;
                let cell = &mut heights[cz * width + cx];
                *cell = cell.max(y);
            }
        }
    }

    info!(
        "built terrain grid {width}x{depth} from {} triangles, tallest {:.0} m",
        tris.len(),
        heights.iter().cloned().fold(0.0, f32::max)
    );
    *terrain = Terrain {
        ready: true,
        origin: mn,
        width,
        depth,
        heights,
    };
}
