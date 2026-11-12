//! The static world: procedurally eroded mountains (see `terrain_gen`), sun
//! and sky, and the [`Terrain`] collision queries everything else uses
//! (aircraft crashes, bullets, missiles, AI terrain avoidance, PULL UP).

use bevy::{
    light::{
        CascadeShadowConfigBuilder, light_consts::lux,
        atmosphere::{Atmosphere, ScatteringMedium},
    },
    prelude::*,
};

use crate::terrain_gen::{GeneratedTerrain, TerrainGenPlugin};

pub struct WorldPlugin;

impl Plugin for WorldPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Terrain>()
            .add_plugins(TerrainGenPlugin)
            .add_systems(Startup, setup)
            .add_systems(Update, follow_sky_dome);
    }
}

/// Collision queries against the ground. Flat at y = 0 until the generated
/// terrain is available (and in headless tests).
#[derive(Resource, Default)]
pub struct Terrain {
    generated: Option<GeneratedTerrain>,
}

/// Sample spacing for queries along a line, in metres (the fine height grid is 64 m).
const STEP: f32 = 32.0;

impl Terrain {
    /// Swap in a newly loaded map (see `loading::load_map`).
    pub fn set(&mut self, generated: GeneratedTerrain) {
        self.generated = Some(generated);
    }

    /// Height of the ground at a world (x, z) position.
    pub fn height_at(&self, x: f32, z: f32) -> f32 {
        self.generated.as_ref().map_or(0.0, |t| t.height_at(x, z))
    }

    /// Whether a world position is inside the ground.
    pub fn is_solid(&self, p: Vec3) -> bool {
        p.y <= self.height_at(p.x, p.z)
    }

    /// Highest ground along a line (the AI and PULL UP warning look ahead with this).
    pub fn max_height_along(&self, from: Vec3, to: Vec3) -> f32 {
        let steps = ((to - from).xz().length() / STEP).ceil().max(1.0) as usize;
        (0..=steps)
            .map(|i| {
                let p = from.lerp(to, i as f32 / steps as f32);
                self.height_at(p.x, p.z)
            })
            .fold(f32::MIN, f32::max)
    }

    /// First point along a segment that is inside the ground, if any.
    pub fn segment_hit(&self, from: Vec3, to: Vec3) -> Option<Vec3> {
        let steps = ((to - from).length() / (STEP * 0.5)).ceil().max(1.0) as usize;
        (1..=steps)
            .map(|i| from.lerp(to, i as f32 / steps as f32))
            .find(|p| self.is_solid(*p))
    }
}


fn setup(
    mut commands: Commands,
    mut scattering_mediums: ResMut<Assets<ScatteringMedium>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    if crate::platform::low_memory() {
        commands.insert_resource(bevy::light::DirectionalLightShadowMap { size: 1024 });
    }
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

    if crate::platform::atmosphere() {
        // the planet's atmosphere (centered below the origin by default)
        commands.spawn(Atmosphere::earth(
            scattering_mediums.add(ScatteringMedium::default()),
        ));
    } else {
        spawn_sky_dome(&mut commands, &mut meshes, &mut materials);
    }
}

/// Fallback sky for GPUs that can't run the atmosphere (see
/// `platform::atmosphere`): a gradient dome around the camera, plus flat
/// ambient light standing in for the sky's environment lighting.
#[derive(Component)]
struct SkyDome;

const SKY_DOME_RADIUS: f32 = 100_000.0;

fn spawn_sky_dome(commands: &mut Commands, meshes: &mut Assets<Mesh>, materials: &mut Assets<StandardMaterial>) {
    let mut mesh = Sphere::new(1.0).mesh().uv(48, 24);
    let colors: Vec<[f32; 4]> = mesh
        .attribute(Mesh::ATTRIBUTE_POSITION)
        .and_then(|a| a.as_float3())
        .unwrap_or_default()
        .iter()
        .map(|p| {
            // pale at the horizon, deep blue overhead, grey-blue below
            let horizon = LinearRgba::from(Color::srgb(0.72, 0.8, 0.9));
            let zenith = LinearRgba::from(Color::srgb(0.2, 0.38, 0.7));
            let below = LinearRgba::from(Color::srgb(0.45, 0.52, 0.6));
            let y = p[1];
            let c = if y >= 0.0 {
                horizon.mix(&zenith, y.powf(0.6))
            } else {
                horizon.mix(&below, (-y * 4.0).min(1.0))
            };
            [c.red, c.green, c.blue, 1.0]
        })
        .collect();
    mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, colors);
    commands.spawn((
        SkyDome,
        Mesh3d(meshes.add(mesh)),
        MeshMaterial3d(materials.add(StandardMaterial {
            base_color: Color::WHITE,
            unlit: true,
            fog_enabled: false,
            cull_mode: None,
            ..default()
        })),
        Transform::from_scale(Vec3::splat(SKY_DOME_RADIUS)),
        bevy::light::NotShadowCaster,
        bevy::light::NotShadowReceiver,
    ));
    commands.insert_resource(GlobalAmbientLight {
        color: Color::srgb(0.7, 0.8, 1.0),
        brightness: SKY_AMBIENT,
        ..default()
    });
}

/// Sky light (cd/m²) on the fallback path, roughly what the atmosphere's environment light gives.
const SKY_AMBIENT: f32 = 12_000.0;

fn follow_sky_dome(
    camera: Query<&Transform, (With<crate::camera::MainCamera>, Without<SkyDome>)>,
    mut dome: Query<&mut Transform, With<SkyDome>>,
) {
    let (Ok(cam), Ok(mut dome)) = (camera.single(), dome.single_mut()) else {
        return;
    };
    dome.translation = cam.translation;
}
