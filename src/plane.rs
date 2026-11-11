use bevy::prelude::*;

use super::utils;
use super::{Ally, Controls, MainCamera, Player};

const TIME_STEP: f32 = 1.0 / 60.0;
const INITIAL_PLANE_ALTITUDE: f32 = 1000.0;

const MINIMUM_THRUST: f32 = 0.0;
const MAXIMUM_THRUST: f32 = 75.0;
const AFTERBURNER_THRUST: f32 = 100.0;

const CAMERA_X: f32 = 0.0;
const CAMERA_Y: f32 = 5.0;
const CAMERA_Z: f32 = 20.0;

const GRAVITY: f32 = -9.81 * TIME_STEP;

pub struct PlanePlugin;

impl Plugin for PlanePlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(Time::<Fixed>::from_seconds(TIME_STEP as f64))
            .add_systems(Startup, setup)
            .add_systems(FixedUpdate, (controls_system, plane_system).chain());
    }
}

fn setup(mut commands: Commands, asset_server: Res<AssetServer>) {
    let f22_raptor = asset_server.load(GltfAssetLabel::Scene(0).from_asset("models/f22-raptor/scene.gltf"));

    commands
        .spawn((
            Transform::from_translation(Vec3::new(0.0, INITIAL_PLANE_ALTITUDE, 0.0)),
            Visibility::default(),
            Player::default(),
            Ally,
        ))
        .with_children(|parent| {
            // center of the plane is not at 0,0 so offset slightly
            parent.spawn((
                WorldAssetRoot(f22_raptor),
                Transform::from_translation(Vec3::new(0.0, -5.0, 0.0))
                    .with_rotation(Quat::from_rotation_y(-std::f32::consts::FRAC_PI_2)),
            ));
        });
}

fn controls_system(keyboard_input: Res<ButtonInput<KeyCode>>, mut controls: ResMut<Controls>) {
    let mut pitch = 0.0;
    let mut roll = 0.0;
    let mut yaw = 0.0;
    let mut thrust = 0.0;

    if keyboard_input.pressed(KeyCode::ArrowLeft) || keyboard_input.pressed(KeyCode::KeyA) {
        roll += 1.0;
    }
    if keyboard_input.pressed(KeyCode::ArrowRight) || keyboard_input.pressed(KeyCode::KeyD) {
        roll -= 1.0;
    }
    if keyboard_input.pressed(KeyCode::ArrowUp) || keyboard_input.pressed(KeyCode::KeyW) {
        pitch -= 1.0;
    }
    if keyboard_input.pressed(KeyCode::ArrowDown) || keyboard_input.pressed(KeyCode::KeyS) {
        pitch += 1.0;
    }
    if keyboard_input.pressed(KeyCode::KeyQ) {
        yaw += 1.0;
    }
    if keyboard_input.pressed(KeyCode::KeyE) {
        yaw -= 1.0
    }
    if keyboard_input.pressed(KeyCode::ShiftLeft) || keyboard_input.pressed(KeyCode::ShiftRight) {
        thrust += 1.0;
    }
    if keyboard_input.pressed(KeyCode::ControlLeft) || keyboard_input.pressed(KeyCode::ControlRight) {
        thrust -= 1.0;
    }

    controls.yaw = utils::lerp(controls.yaw, yaw, 0.1);
    controls.pitch = utils::lerp(controls.pitch, pitch, 0.1);
    controls.roll = utils::lerp(controls.roll, roll, 0.1);
    controls.thrust = utils::clamp(controls.thrust + thrust, MINIMUM_THRUST, AFTERBURNER_THRUST);

    if thrust != 1.0 && controls.thrust > MAXIMUM_THRUST {
        controls.thrust -= 0.5
    }

    controls.airbrakes = thrust == -1.0 && controls.thrust == 0.0;
}

fn plane_system(
    controls: Res<Controls>,
    mut query: Query<(&mut Transform, &mut Player), With<Player>>,
    mut camera_query: Query<&mut Transform, (With<MainCamera>, Without<Player>)>,
) {
    let Ok(mut camera) = camera_query.single_mut() else {
        return;
    };
    let Ok((mut player_transform, mut player)) = query.single_mut() else {
        return;
    };

    let forwards = player_transform.forward().as_vec3();

    let axis_deviance = Vec3::normalize_or_zero(player.velocity)
        .dot(Vec3::normalize_or_zero(forwards))
        / forwards.length();
    let on_axis_speed = player.velocity.length() * axis_deviance;

    let yaw = controls.yaw * 0.01 * axis_deviance;
    let pitch = controls.pitch * 0.03 * axis_deviance;
    let roll = controls.roll * 0.05 * axis_deviance;

    // many thanks to rchar
    let player_x = player_transform.local_x().as_vec3();
    let player_y = player_transform.local_y().as_vec3();
    let player_z = player_transform.local_z().as_vec3();
    let rot = Quat::from_axis_angle(player_x, pitch)
        * Quat::from_axis_angle(player_y, yaw)
        * Quat::from_axis_angle(player_z, roll);
    player_transform.rotate(rot);

    // thrust
    let speed = player.velocity.length_squared();
    let thrust = if controls.thrust > MAXIMUM_THRUST {
        MAXIMUM_THRUST + (controls.thrust - MAXIMUM_THRUST).powf(2.0)
    } else {
        controls.thrust
    };

    player.velocity += (forwards * (thrust)) / speed;

    // airbrakes
    if controls.airbrakes {
        let velocity_norm = player.velocity.normalize_or_zero();
        player.velocity -= velocity_norm * 0.1;
    }

    // nudge velocity vector towards the forwards vector
    player.velocity = player.velocity.lerp(
        forwards * player.velocity.length(),
        utils::clamp(axis_deviance, 0.1, 1.0) * 0.05,
    );

    // drag
    // should be done depending on axis
    player.velocity *= utils::clamp(axis_deviance + 0.3, 0.99, 1.0);

    // gravity
    player.velocity += Vec3::new(0.0, GRAVITY, 0.0);

    // lift
    if on_axis_speed > 30.0 {
        player.velocity += Vec3::new(0.0, -GRAVITY, 0.0) * axis_deviance;
    } else {
        player.velocity += Vec3::new(0.0, -GRAVITY, 0.0) * axis_deviance * (on_axis_speed / 30.0);
    }

    player_transform.translation += player.velocity;

    if player_transform.translation.y < 0.0 {
        player_transform.translation.y = 0.0;
    }

    camera.translation = camera.translation.lerp(player_transform.translation, 0.2)
        + player_transform
            .rotation
            .mul_vec3(Vec3::new(CAMERA_X, 0.0, CAMERA_Z))
        + Vec3::new(0.0, CAMERA_Y, 0.0);

    camera.look_at(player_transform.translation + (forwards * 100.0), Vec3::Y);

    // set player stalling for UI
    player.stalling = axis_deviance < 0.5;
}
