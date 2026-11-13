//! The static world: procedurally eroded mountains (see `terrain_gen`), sun
//! and sky (set by the round's `environment`), and the [`Terrain`] collision queries everything else uses
//! (aircraft crashes, bullets, missiles, AI terrain avoidance, PULL UP).

use bevy::{
    light::{
        CascadeShadowConfigBuilder, light_consts::lux,
        atmosphere::{Atmosphere, ScatteringMedium},
    },
    prelude::*,
};

use crate::{
    environment::{Environment, Look},
    terrain_gen::{GeneratedTerrain, TerrainGenPlugin},
};

pub struct WorldPlugin;

impl Plugin for WorldPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Terrain>()
            .add_plugins(TerrainGenPlugin)
            .add_systems(Update, sync_shadow_map)
            // the round's environment is chosen before each bake (and for the menu)
            .add_systems(Update, (apply_environment.run_if(resource_changed::<Environment>), follow_sky_dome).chain());
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


/// Smaller shadow maps in the low-memory scene (follows the GRAPHICS setting live).
fn sync_shadow_map(map: Option<ResMut<bevy::light::DirectionalLightShadowMap>>) {
    let size = if crate::platform::low_memory() { 1024 } else { 2048 };
    if let Some(mut map) = map
        && map.size != size
    {
        map.size = size;
    }
}

/// The sun and sky of the current environment (replaced when it changes).
#[derive(Component)]
struct SunAndSky;

fn apply_environment(
    mut commands: Commands,
    env: Res<Environment>,
    old: Query<Entity, With<SunAndSky>>,
    mut scattering_mediums: ResMut<Assets<ScatteringMedium>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    for e in &old {
        commands.entity(e).despawn();
    }
    let look = env.look();
    // sun: with the atmosphere, the GPU dims and reddens it by elevation; the
    // fallback sky gets an approximation of that baked into its colour
    let (color, illuminance) = if crate::platform::atmosphere() {
        (Color::WHITE, look.sun_illuminance)
    } else {
        let s = look.fallback_sun;
        let peak = s.max_element().max(1e-4);
        (Color::linear_rgb(s.x / peak, s.y / peak, s.z / peak), lux::RAW_SUNLIGHT * peak)
    };
    commands.spawn((
        SunAndSky,
        DirectionalLight {
            shadow_maps_enabled: true,
            illuminance,
            color,
            ..default()
        },
        Transform::from_translation(look.sun_dir).looking_at(Vec3::ZERO, Vec3::Y),
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
        // (with more aerosol in haze and under cloud)
        commands.spawn((SunAndSky, Atmosphere::earth(scattering_mediums.add(look.scattering_medium()))));
    } else {
        spawn_sky_dome(&look, &mut commands, &mut meshes, &mut materials);
    }
}

/// Fallback sky for GPUs that can't run the atmosphere (see
/// `platform::atmosphere`): a gradient dome around the camera, plus flat
/// ambient light standing in for the sky's environment lighting.
#[derive(Component)]
struct SkyDome;

const SKY_DOME_RADIUS: f32 = 100_000.0;

fn spawn_sky_dome(look: &Look, commands: &mut Commands, meshes: &mut Assets<Mesh>, materials: &mut Assets<StandardMaterial>) {
    let mut mesh = Sphere::new(1.0).mesh().uv(48, 24);
    // sRGB-authored colours (see `environment::Look`), mixed in linear
    let lin = |v: Vec3| LinearRgba::from(Color::srgb(v.x, v.y, v.z));
    let (horizon, zenith, below, glow) =
        (lin(look.sky_horizon), lin(look.sky_zenith), lin(look.sky_below), lin(look.sky_glow));
    let sun = look.sun_dir;
    let colors: Vec<[f32; 4]> = mesh
        .attribute(Mesh::ATTRIBUTE_POSITION)
        .and_then(|a| a.as_float3())
        .unwrap_or_default()
        .iter()
        .map(|p| {
            // pale at the horizon, deep blue overhead, grey-blue below, and a
            // glow around the sun (wider and warmer when it's low)
            let y = p[1];
            let mut c = if y >= 0.0 {
                horizon.mix(&zenith, y.powf(0.6))
            } else {
                horizon.mix(&below, (-y * 4.0).min(1.0))
            };
            let toward_sun = Vec3::from(*p).normalize_or_zero().dot(sun).max(0.0);
            let near_horizon = 1.0 - y.abs().min(1.0);
            let g = toward_sun.powf(6.0) * (0.4 + 0.6 * near_horizon) + toward_sun.powf(64.0);
            c = c + glow * g;
            [c.red, c.green, c.blue, 1.0]
        })
        .collect();
    mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, colors);
    commands.spawn((
        SunAndSky,
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
    let a = look.ambient_color;
    commands.insert_resource(GlobalAmbientLight {
        color: Color::srgb(a.x, a.y, a.z),
        brightness: look.ambient_brightness,
        ..default()
    });
}

fn follow_sky_dome(
    camera: Query<&Transform, (With<crate::camera::MainCamera>, Without<SkyDome>)>,
    mut dome: Query<&mut Transform, With<SkyDome>>,
) {
    let (Ok(cam), Ok(mut dome)) = (camera.single(), dome.single_mut()) else {
        return;
    };
    dome.translation = cam.translation;
}
