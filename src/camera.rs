//! Chase camera, or the pilot's seat ([`View`]: V or the right stick's click).
//!
//! - Direct mode: sits behind and above the plane and rolls with it, with a
//!   little lag so manoeuvres read. The right stick (or C) looks around.
//! - Mouse-aim mode: orbits behind the *aim direction* with the horizon kept
//!   level, so the plane visibly turns to chase where you point.

use bevy::{
    camera::Exposure,
    core_pipeline::tonemapping::Tonemapping,
    light::{AtmosphereEnvironmentMapLight, EnvironmentMapLight},
    pbr::AtmosphereSettings,
    post_process::bloom::Bloom,
    prelude::*,
};

use crate::{
    controls::{ControlMode, LookAround, MouseAim},
    game::Player,
};

pub struct CameraPlugin;

impl Plugin for CameraPlugin {
    fn build(&self, app: &mut App) {
        patch_atmosphere(app);
        app.insert_resource(View::from_env())
            .add_systems(Startup, spawn_camera)
            .add_systems(Update, (toggle_view.run_if(in_state(crate::loading::GameState::Playing)), apply_view).chain())
            .add_systems(Update, (apply_environment.run_if(resource_changed::<crate::environment::Environment>), freeze_sky_light).chain())
            .add_systems(PostUpdate, follow_player.before(TransformSystems::Propagate));
    }
}

/// Swaps Bevy's atmosphere compositing shader for our copy, which integrates the
/// transmittance to opaque geometry along the ray instead of reading it off the
/// transmittance LUT (see `assets/shaders/atmosphere_render_sky.wesl`: the LUT ratio
/// put a seam across distant islands at the sea horizon). The atmosphere plugin
/// embeds its shaders at build time and they're loaded later, so overwriting the
/// embedded bytes here (after `DefaultPlugins`) is enough.
fn patch_atmosphere(app: &mut App) {
    use bevy::asset::io::embedded::EmbeddedAssetRegistry;
    let Some(registry) = app.world().get_resource::<EmbeddedAssetRegistry>() else {
        return;
    };
    registry.insert_asset(
        std::path::PathBuf::new(),
        std::path::Path::new("bevy_pbr/atmosphere/render_sky.wesl"),
        include_bytes!("../assets/shaders/atmosphere_render_sky.wesl").as_slice(),
    );
}

/// Marker for the main 3D camera.
#[derive(Component)]
pub struct MainCamera;

fn spawn_camera(
    mut commands: Commands,
    mut images: ResMut<Assets<Image>>,
    env: Res<crate::environment::Environment>,
) {
    let look = env.look();
    let mut camera = commands.spawn((
        Camera3d::default(),
        MainCamera,
        Projection::Perspective(PerspectiveProjection {
            // (`FLIGHT_SIM_FOV=<degrees>`: a longer lens for screenshots)
            fov: std::env::var("FLIGHT_SIM_FOV").ok().and_then(|v| v.parse::<f32>().ok()).unwrap_or(65.0).to_radians(),
            near: 0.5,
            far: 120_000.0,
            ..default()
        }),
        exposure(&look),
        Tonemapping::AcesFitted,
        Bloom::NATURAL,
        Transform::from_xyz(0.0, 1510.0, 6030.0),
        // distance haze on top of the atmosphere's aerial perspective: far ridges
        // fade into blue-grey (or grey, or warm haze: see `environment`)
        look.distance_fog(),
        // the HUD draws onto whatever this camera renders to (window or offscreen image)
        bevy::ui::IsDefaultUiCamera,
    ));
    if crate::platform::atmosphere() {
        camera.insert((
            // more samples than the defaults: 16 per ray leaves visible stripes in the sky
            AtmosphereSettings {
                sky_view_lut_size: UVec2::new(512, 256),
                sky_view_lut_samples: 48,
                aerial_view_lut_samples: 20,
                aerial_view_lut_max_distance: 6.0e4,
                ..default()
            },
        ));
    }
    // web: no MSAA. Its multisampled colour, depth and prepass targets cost a
    // lot of GPU memory, which phones run out of
    if cfg!(target_arch = "wasm32") {
        camera.insert(bevy::render::view::Msaa::Off);
    }
    if crate::dev::headless() {
        use bevy::render::render_resource::{TextureFormat, TextureUsages};
        let size = crate::dev::headless_size();
        let mut image =
            Image::new_target_texture(size.x, size.y, TextureFormat::Rgba8UnormSrgb, None);
        image.texture_descriptor.usage |= TextureUsages::COPY_SRC;
        let handle = images.add(image);
        camera.insert(bevy::camera::RenderTarget::Image(bevy::camera::ImageRenderTarget {
            handle: handle.clone(),
            scale_factor: crate::dev::headless_dpr(),
        }));
        commands.insert_resource(crate::dev::HeadlessTarget(handle));
    }
}

fn exposure(look: &crate::environment::Look) -> Exposure {
    Exposure { ev100: if crate::platform::atmosphere() { look.exposure } else { look.fallback_exposure } }
}

/// Exposure, fog and sky light follow the round's environment.
fn apply_environment(
    mut commands: Commands,
    env: Res<crate::environment::Environment>,
    mut camera: Query<(Entity, &mut Exposure, &mut DistanceFog), With<MainCamera>>,
    probes: Query<Entity, With<SkyLightProbe>>,
) {
    let look = env.look();
    for (entity, mut exp, mut fog) in &mut camera {
        *exp = exposure(&look);
        *fog = look.distance_fog();
        if crate::platform::atmosphere() {
            // a new sky: generate its light afresh (see `freeze_sky_light`)
            commands.entity(entity).remove::<EnvironmentMapLight>();
            for probe in &probes {
                commands.entity(probe).despawn();
            }
            commands.spawn((
                SkyLightProbe(0),
                AtmosphereEnvironmentMapLight { intensity: look.env_light, ..default() },
            ));
        }
    }
}

/// Generates the sky's environment light (frames it has been at it).
#[derive(Component)]
struct SkyLightProbe(u32);

/// Frames the generated sky light gets to settle before it's frozen.
const SKY_LIGHT_SETTLE: u32 = 30;

/// Bevy renders the sky into a cubemap and filters it into the environment
/// light every frame (measured: ~57 -> ~60 fps in a fight once it stops). The sky doesn't
/// change during a round (a new one comes with a new environment, and a new
/// probe), so it's generated on a probe of its own and, once settled, the
/// result moves onto the camera and the probe goes, which stops both passes.
fn freeze_sky_light(
    mut commands: Commands,
    mut probes: Query<(Entity, &mut SkyLightProbe, Option<&EnvironmentMapLight>)>,
    camera: Query<Entity, With<MainCamera>>,
) {
    for (probe, mut age, light) in &mut probes {
        age.0 += 1;
        let Some(light) = light.filter(|_| age.0 >= SKY_LIGHT_SETTLE) else { continue };
        if std::env::var("FLIGHT_SIM_LOG").is_ok() {
            info!("sky light frozen after {} frames", age.0);
        }
        for cam in &camera {
            commands.entity(cam).insert(light.clone());
        }
        commands.entity(probe).despawn();
    }
}

/// Where the camera sits.
#[derive(Resource, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum View {
    #[default]
    Chase,
    /// In the seat, at the jet's [`crate::jets::JetSpec::eye`].
    Cockpit,
}

impl View {
    /// `FLIGHT_SIM_VIEW=cockpit` starts in the seat (dev).
    fn from_env() -> Self {
        if std::env::var("FLIGHT_SIM_VIEW").is_ok_and(|v| v == "cockpit") { View::Cockpit } else { View::Chase }
    }
}

/// Field of view (degrees) and near plane (m) for each view: wider in the
/// seat, and a near plane close enough not to clip the instrument panel.
const CHASE_LENS: (f32, f32) = (65.0, 0.5);
const COCKPIT_LENS: (f32, f32) = (68.0, 0.05);
/// How far (rad) the seat view turns off the nose toward the mouse aim:
/// either side, down, up.
const COCKPIT_AIM_LIMIT: Vec3 = Vec3::new(0.45, -0.1, 0.55);
/// How far the pilot's head sinks per g (m): about 10 cm at 9 g.
const HEAD_SAG: f32 = 0.012;
/// Head shake (m) in full buffet near the stall.
const BUFFET_SHAKE: f32 = 0.006;

fn toggle_view(keys: Res<ButtonInput<KeyCode>>, gamepads: Query<&Gamepad>, mut view: ResMut<View>) {
    let pad = gamepads.iter().any(|g| g.just_pressed(GamepadButton::RightThumb));
    if keys.just_pressed(KeyCode::KeyV) || pad {
        *view = match *view {
            View::Chase => View::Cockpit,
            View::Cockpit => View::Chase,
        };
    }
}

/// The lens for the view, and the inside of the player's canopy glass (only
/// drawn from the seat: from outside it would double the glass).
fn apply_view(
    view: Res<View>,
    intro: Res<crate::intro::Intro>,
    mut camera: Query<&mut Projection, With<MainCamera>>,
    player: Query<Entity, With<Player>>,
    mut glass: Query<(&crate::jets::Cockpit, &mut Visibility)>,
) {
    let seat = *view == View::Cockpit && !intro.playing();
    let (fov, near) = if seat { COCKPIT_LENS } else { CHASE_LENS };
    let fov = std::env::var("FLIGHT_SIM_FOV").ok().and_then(|v| v.parse::<f32>().ok()).unwrap_or(fov).to_radians();
    for mut projection in &mut camera {
        if let Projection::Perspective(p) = projection.as_mut()
            && (p.fov != fov || p.near != near)
        {
            p.fov = fov;
            p.near = near;
        }
    }
    let me = player.single().ok();
    for (part, mut vis) in &mut glass {
        if !part.inside_only {
            continue;
        }
        let want = if seat && Some(part.aircraft) == me { Visibility::Inherited } else { Visibility::Hidden };
        vis.set_if_neq(want);
    }
}

/// The player's jet, and which one it is (the chase distance depends on it).
type PlayerJet = (
    &'static Transform,
    Option<&'static crate::jets::JetChoice>,
    Option<&'static crate::flight::FlightState>,
);

#[allow(clippy::too_many_arguments)]
fn follow_player(
    time: Res<Time>,
    mode: Res<ControlMode>,
    aim: Res<MouseAim>,
    look: Res<LookAround>,
    intro: Res<crate::intro::Intro>,
    view: Res<View>,
    player: Query<PlayerJet, (With<Player>, Without<MainCamera>)>,
    mut camera: Query<&mut Transform, With<MainCamera>>,
    mut boom: Local<Option<Quat>>,
    mut head: Local<Vec3>,
) {
    // terrain_gen's debug camera owns the camera when it's enabled
    if std::env::var("TERRAIN_DEBUG_CAM").is_ok() {
        return;
    }
    let (Ok((plane, jet, state)), Ok(mut cam)) = (player.single(), camera.single_mut()) else {
        *boom = None;
        return;
    };
    let scale = jet.map_or(1.0, |j| j.airframe().chase);
    let dt = time.delta_secs();

    if intro.playing() {
        intro_shot(&intro, plane, scale, &mut cam);
        // hand over to the chase camera already settled on its target
        *boom = Some(Transform::IDENTITY.looking_to(plane.forward().as_vec3(), Vec3::Y).rotation);
        return;
    }

    if *view == View::Cockpit {
        // the pilot's eye, looking where the mouse aims (mouse aim) or along
        // the nose with the look-around (direct); the horizon rolls with the jet
        let eye = jet.map_or(crate::jets::F22_EYE, |j| j.airframe().eye);
        // the head sinks under g (and lifts a little under negative g), eased
        // so it lags like a neck; near the stall the airframe buffets
        let (g, alpha) = state.map_or((1.0, 0.0), |s| (s.g_load, s.alpha.to_degrees()));
        let sag = Vec3::new(0.0, -(g - 1.0).clamp(-3.0, 9.0) * HEAD_SAG, 0.0);
        *head = head.lerp(sag, 1.0 - (-6.0 * dt).exp());
        let buffet = ((alpha - 14.0) / 12.0).clamp(0.0, 1.0);
        let t = time.elapsed_secs_wrapped();
        let shake = Vec3::new((t * 53.0).sin() + (t * 31.0).sin() * 0.6, (t * 47.0).sin() + (t * 23.0).cos() * 0.6, 0.0)
            * (BUFFET_SHAKE * buffet);
        cam.translation = plane.transform_point(eye + *head + shake);
        cam.rotation = match *mode {
            ControlMode::MouseAim => {
                // toward the aim, as a pilot looks into the turn, but only so far
                // off the nose (and hardly down: past that it's the instrument
                // panel and the seat while the jet catches up)
                let local = plane.rotation.inverse() * aim.dir.normalize_or(plane.forward().as_vec3());
                let yaw = (-local.x).atan2(-local.z).clamp(-COCKPIT_AIM_LIMIT.x, COCKPIT_AIM_LIMIT.x);
                let pitch = local.y.clamp(-1.0, 1.0).asin().clamp(COCKPIT_AIM_LIMIT.y, COCKPIT_AIM_LIMIT.z);
                plane.rotation * Quat::from_rotation_y(yaw) * Quat::from_rotation_x(pitch)
            }
            ControlMode::Direct => {
                plane.rotation * Quat::from_rotation_y(look.0.x) * Quat::from_rotation_x(look.0.y.min(0.6))
            }
        };
        // back out of the seat onto a boom already behind the jet
        *boom = None;
        return;
    }

    // The camera hangs off a "boom" whose *orientation* is smoothed, but whose
    // pivot is exactly the plane's (interpolated) position. Smoothing the
    // position itself makes the plane shake at 250 m/s as frame times vary.
    let (target, offset, stiffness) = match *mode {
        ControlMode::MouseAim => (
            Transform::IDENTITY.looking_to(aim.dir, Vec3::Y).rotation,
            Vec3::new(0.0, 7.0, 32.0),
            30.0,
        ),
        ControlMode::Direct => {
            // free look rotates the boom around the plane; the view rolls with the plane
            let look_rot = Quat::from_rotation_y(look.0.x) * Quat::from_rotation_x(-look.0.y);
            (plane.rotation * look_rot, Vec3::new(0.0, 6.0, 30.0), 9.0)
        }
    };
    let k = 1.0 - (-stiffness * dt).exp();
    let smoothed = boom.map_or(target, |b| b.slerp(target, k)).normalize();
    *boom = Some(smoothed);

    cam.translation = plane.translation + smoothed * (offset * scale);
    // tilt down a touch so the plane sits below the centre of the screen
    cam.rotation = smoothed * Quat::from_rotation_x(-0.06);
}

/// The intro camera: starts low off the jet's nose and swings round (over the
/// top of the wing, a little to the side) to exactly the mouse-aim chase pose.
fn intro_shot(intro: &crate::intro::Intro, plane: &Transform, scale: f32, cam: &mut Transform) {
    use std::f32::consts::PI;
    let u = intro.sweep();
    // the chase camera's frame: the plane's heading, without its roll
    let frame = Transform::IDENTITY.looking_to(plane.forward().as_vec3(), Vec3::Y).rotation;
    // orbit angle around the plane's up axis: front three-quarter to dead astern
    let yaw = (PI * 0.88) * (1.0 - u);
    let lerp = |a: f32, b: f32| a + (b - a) * u;
    // boom: low and close in front, the chase offset (0, 7, 32) at the end;
    // it rises in the middle of the swing so it passes over the wing
    let lift = (u * PI).sin() * 6.0;
    let offset = Quat::from_rotation_y(yaw) * Vec3::new(0.0, lerp(1.5, 7.0) + lift, lerp(20.0, 32.0));
    cam.translation = plane.translation + frame * (offset * scale);
    // look at the jet, ending on the chase camera's aim point (which makes the
    // final rotation identical to `follow_player`'s, so there's no snap)
    let aim = plane.translation + frame * Vec3::new(0.0, lerp(0.8, 7.0 - 32.0 * 0.06f32.tan()) * scale, 0.0);
    cam.look_at(aim, frame * Vec3::Y);
}
