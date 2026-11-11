//! Chase camera.
//!
//! - Direct mode: sits behind and above the plane and rolls with it, with a
//!   little lag so manoeuvres read. The right stick (or C) looks around.
//! - Mouse-aim mode: orbits behind the *aim direction* with the horizon kept
//!   level, so the plane visibly turns to chase where you point.

use bevy::{
    camera::Exposure,
    core_pipeline::tonemapping::Tonemapping,
    light::AtmosphereEnvironmentMapLight,
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
        app.add_systems(Startup, spawn_camera)
            .add_systems(PostUpdate, follow_player.before(TransformSystems::Propagate));
    }
}

/// Marker for the main 3D camera.
#[derive(Component)]
pub struct MainCamera;

fn spawn_camera(mut commands: Commands, mut images: ResMut<Assets<Image>>) {
    let mut camera = commands.spawn((
        Camera3d::default(),
        MainCamera,
        Projection::Perspective(PerspectiveProjection {
            fov: 65f32.to_radians(),
            near: 0.5,
            far: 120_000.0,
            ..default()
        }),
        // more samples than the defaults: 16 per ray leaves visible stripes in the sky
        AtmosphereSettings {
            sky_view_lut_size: UVec2::new(512, 256),
            sky_view_lut_samples: 48,
            aerial_view_lut_samples: 20,
            aerial_view_lut_max_distance: 6.0e4,
            ..default()
        },
        AtmosphereEnvironmentMapLight::default(),
        Exposure::SUNLIGHT,
        Tonemapping::AcesFitted,
        Bloom::NATURAL,
        Transform::from_xyz(0.0, 1510.0, 6030.0),
        // the HUD draws onto whatever this camera renders to (window or offscreen image)
        bevy::ui::IsDefaultUiCamera,
    ));
    if crate::dev::headless() {
        use bevy::render::render_resource::{TextureFormat, TextureUsages};
        let size = crate::dev::HEADLESS_SIZE;
        let mut image =
            Image::new_target_texture(size.x, size.y, TextureFormat::Rgba8UnormSrgb, None);
        image.texture_descriptor.usage |= TextureUsages::COPY_SRC;
        let handle = images.add(image);
        camera.insert(bevy::camera::RenderTarget::Image(handle.clone().into()));
        commands.insert_resource(crate::dev::HeadlessTarget(handle));
    }
}

fn follow_player(
    time: Res<Time>,
    mode: Res<ControlMode>,
    aim: Res<MouseAim>,
    look: Res<LookAround>,
    player: Query<&Transform, (With<Player>, Without<MainCamera>)>,
    mut camera: Query<&mut Transform, With<MainCamera>>,
    mut boom: Local<Option<Quat>>,
) {
    // terrain_gen's debug camera owns the camera when it's enabled
    if std::env::var("TERRAIN_DEBUG_CAM").is_ok() {
        return;
    }
    let (Ok(plane), Ok(mut cam)) = (player.single(), camera.single_mut()) else {
        *boom = None;
        return;
    };
    let dt = time.delta_secs();

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

    cam.translation = plane.translation + smoothed * offset;
    // tilt down a touch so the plane sits below the centre of the screen
    cam.rotation = smoothed * Quat::from_rotation_x(-0.06);
}
