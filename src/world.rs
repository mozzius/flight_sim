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
            .add_systems(Startup, setup);
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


fn setup(mut commands: Commands, mut scattering_mediums: ResMut<Assets<ScatteringMedium>>) {
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

    // the planet's atmosphere (centered below the origin by default)
    commands.spawn(Atmosphere::earth(
        scattering_mediums.add(ScatteringMedium::default()),
    ));
}
