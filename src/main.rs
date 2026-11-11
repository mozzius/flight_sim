use bevy::{
    camera::Exposure,
    core_pipeline::tonemapping::Tonemapping,
    light::{
        AtmosphereEnvironmentMapLight, light_consts::lux,
        atmosphere::{Atmosphere, ScatteringMedium},
    },
    pbr::AtmosphereSettings,
    post_process::bloom::Bloom,
    prelude::*,
};

mod ai;
mod dev;
mod hud;
mod plane;
mod utils;

const INITIAL_PLANE_SPEED: f32 = 40.0;

#[derive(Resource)]
pub struct Controls {
    pitch: f32,
    yaw: f32,
    roll: f32,
    thrust: f32,
    airbrakes: bool,
}

impl Default for Controls {
    fn default() -> Self {
        Controls {
            pitch: 0.0,
            yaw: 0.0,
            roll: 0.0,
            thrust: 50.0,
            airbrakes: false,
        }
    }
}

#[derive(Component)]
pub struct Player {
    velocity: Vec3,
    stalling: bool,
}

impl Default for Player {
    fn default() -> Self {
        Player {
            velocity: Vec3::new(0.0, 0.0, -INITIAL_PLANE_SPEED),
            stalling: false,
        }
    }
}

#[derive(Component)]
#[allow(dead_code)]
pub struct AI {
    health: f32,
    velocity: Vec3,
    target: Option<Entity>,
}

impl Default for AI {
    fn default() -> Self {
        AI {
            health: 100.0,
            velocity: Vec3::new(0.0, 0.0, -1.0),
            target: None,
        }
    }
}

#[derive(Component)]
pub struct Ally;

#[derive(Component)]
pub struct Enemy;

/// Marker for the main 3D camera that follows the player.
#[derive(Component)]
pub struct MainCamera;

fn main() {
    App::new()
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title: "Flight Sim".to_string(),
                ..default()
            }),
            ..default()
        }))
        .insert_resource(Controls::default())
        .add_plugins((hud::HUDPlugin, plane::PlanePlugin, ai::AIPlugin, dev::DevPlugin))
        .add_systems(Startup, setup)
        .run();
}

/// set up the background
fn setup(
    mut commands: Commands,
    asset_server: Res<AssetServer>,
    mut scattering_mediums: ResMut<Assets<ScatteringMedium>>,
) {
    let cityscape = asset_server.load(GltfAssetLabel::Scene(0).from_asset("models/cityscape/scene.gltf"));

    commands.spawn((
        WorldAssetRoot(cityscape),
        Transform::from_scale(Vec3::splat(10.0)),
    ));

    // sun
    commands.spawn((
        DirectionalLight {
            shadow_maps_enabled: true,
            illuminance: lux::RAW_SUNLIGHT,
            ..default()
        },
        Transform::from_xyz(4.0, 10.0, 10.0).looking_at(Vec3::ZERO, Vec3::Y),
    ));

    // the planet's atmosphere (centered below the origin by default)
    commands.spawn(Atmosphere::earth(
        scattering_mediums.add(ScatteringMedium::default()),
    ));

    commands.spawn((
        Camera3d::default(),
        MainCamera,
        AtmosphereSettings::default(),
        AtmosphereEnvironmentMapLight::default(),
        Exposure::SUNLIGHT,
        Tonemapping::AcesFitted,
        Bloom::NATURAL,
    ));
}
