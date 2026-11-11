//! Player controls. Two modes, toggled with `M` / gamepad Select:
//!
//! - **Direct**: keyboard or gamepad sticks drive pitch/roll/yaw.
//! - **Mouse aim**: the mouse makes fine pitch/rudder corrections toward the
//!   aim point (kept within 40° of the nose). It never rolls: bank with A/D
//!   (or the stick), then the mouse pulls you around the turn.
//!
//! Keyboard: S/W or ↓/↑ pitch, A/D or ←/→ roll, Q/E yaw, Shift/Ctrl
//! throttle (hold at the end for afterburner/airbrake), Space or left mouse =
//! gun, F or right mouse = missile, X = flares, Tab / Shift+Tab = next /
//! previous target, R or middle mouse = target nearest the crosshair.
//!
//! Gamepad: left stick pitch/roll, LB/RB yaw, RT/LT throttle (fully pressed =
//! afterburner/airbrake), A = gun, B = missile, X = flares, D-pad left/right =
//! previous/next target, Y or D-pad up = target nearest the crosshair, right
//! stick = look around.

use bevy::{
    input::mouse::AccumulatedMouseMotion,
    prelude::*,
    window::{CursorGrabMode, CursorOptions, PrimaryWindow},
};

use crate::{
    flight::{ControlInput, FlightState, FlightSystems, fine_aim},
    game::Player,
};

pub struct ControlsPlugin;

impl Plugin for ControlsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ControlMode>()
            .init_resource::<PlayerActions>()
            .init_resource::<ThrottleLever>()
            .init_resource::<MouseAim>()
            .init_resource::<LookAround>()
            .add_systems(
                Update,
                (toggle_mode, gather_actions, update_mouse_aim, update_look_around),
            )
            .add_systems(
                FixedUpdate,
                player_flight_input.in_set(FlightSystems::Pilots),
            );
    }
}

#[derive(Resource, Default, Clone, Copy, PartialEq, Eq, Debug)]
pub enum ControlMode {
    #[default]
    MouseAim,
    Direct,
}

/// Discrete actions captured in `Update` (so presses are never missed) and
/// consumed by the fixed-step systems.
#[derive(Resource, Default)]
pub struct PlayerActions {
    pub firing_gun: bool,
    pub fire_missile: bool,
    pub drop_flares: bool,
    pub next_target: bool,
    pub previous_target: bool,
    /// Select the hostile closest to the crosshair.
    pub target_under_crosshair: bool,
}

/// Persistent throttle setting (0..1).
#[derive(Resource)]
pub struct ThrottleLever(pub f32);

impl Default for ThrottleLever {
    fn default() -> Self {
        ThrottleLever(0.75)
    }
}

/// World-space direction the mouse is pointing (mouse-aim mode).
#[derive(Resource)]
pub struct MouseAim {
    pub dir: Vec3,
    /// Set when the aim needs to snap back to the plane's nose (respawn, mode switch).
    pub reset: bool,
}

impl Default for MouseAim {
    fn default() -> Self {
        MouseAim {
            dir: Vec3::NEG_Z,
            reset: true,
        }
    }
}

/// Free-look offset (yaw, pitch radians) from the right stick in direct mode.
#[derive(Resource, Default)]
pub struct LookAround(pub Vec2);

fn first_gamepad<'a>(gamepads: &'a Query<&Gamepad>) -> Option<&'a Gamepad> {
    gamepads.iter().next()
}

fn toggle_mode(
    keys: Res<ButtonInput<KeyCode>>,
    gamepads: Query<&Gamepad>,
    mut mode: ResMut<ControlMode>,
    mut aim: ResMut<MouseAim>,
    mut cursor: Query<&mut CursorOptions, With<PrimaryWindow>>,
    mouse: Res<ButtonInput<MouseButton>>,
) {
    let pad_toggle = gamepads
        .iter()
        .any(|g| g.just_pressed(GamepadButton::Select));
    if keys.just_pressed(KeyCode::KeyM) || pad_toggle {
        *mode = match *mode {
            ControlMode::MouseAim => ControlMode::Direct,
            ControlMode::Direct => ControlMode::MouseAim,
        };
        aim.reset = true;
        info!("control mode: {:?}", *mode);
    }

    // grab the cursor in mouse-aim mode; Esc lets go, clicking grabs again
    let Ok(mut cursor) = cursor.single_mut() else {
        return;
    };
    // automated screenshot runs shouldn't steal the mouse from whoever's at the desk
    let want_grab =
        *mode == ControlMode::MouseAim && std::env::var("FLIGHT_SIM_SCREENSHOT").is_err();
    if keys.just_pressed(KeyCode::Escape) {
        cursor.grab_mode = CursorGrabMode::None;
        cursor.visible = true;
    } else if want_grab && (mouse.just_pressed(MouseButton::Left) || mode.is_changed()) {
        cursor.grab_mode = CursorGrabMode::Locked;
        cursor.visible = false;
    } else if !want_grab && mode.is_changed() {
        cursor.grab_mode = CursorGrabMode::None;
        cursor.visible = true;
    }
}

fn gather_actions(
    keys: Res<ButtonInput<KeyCode>>,
    mouse: Res<ButtonInput<MouseButton>>,
    gamepads: Query<&Gamepad>,
    mode: Res<ControlMode>,
    mut actions: ResMut<PlayerActions>,
) {
    let pad = first_gamepad(&gamepads);
    let pad_pressed = |b| pad.is_some_and(|g| g.pressed(b));
    let pad_just = |b| pad.is_some_and(|g| g.just_pressed(b));
    let mouse_mode = *mode == ControlMode::MouseAim;

    actions.firing_gun = keys.pressed(KeyCode::Space)
        || (mouse_mode && mouse.pressed(MouseButton::Left))
        || pad_pressed(GamepadButton::South);
    actions.fire_missile |= keys.just_pressed(KeyCode::KeyF)
        || (mouse_mode && mouse.just_pressed(MouseButton::Right))
        || pad_just(GamepadButton::East);
    actions.drop_flares |= keys.just_pressed(KeyCode::KeyX) || pad_just(GamepadButton::West);
    let shift = keys.pressed(KeyCode::ShiftLeft) || keys.pressed(KeyCode::ShiftRight);
    actions.next_target |= (keys.just_pressed(KeyCode::Tab) && !shift)
        || pad_just(GamepadButton::DPadRight);
    actions.previous_target |=
        (keys.just_pressed(KeyCode::Tab) && shift) || pad_just(GamepadButton::DPadLeft);
    actions.target_under_crosshair |= keys.just_pressed(KeyCode::KeyR)
        || mouse.just_pressed(MouseButton::Middle)
        || pad_just(GamepadButton::DPadUp)
        || pad_just(GamepadButton::North);
}

fn update_mouse_aim(
    motion: Res<AccumulatedMouseMotion>,
    mode: Res<ControlMode>,
    mut aim: ResMut<MouseAim>,
    player: Query<&Transform, With<Player>>,
    cursor: Query<&CursorOptions, With<PrimaryWindow>>,
) {
    let Ok(transform) = player.single() else {
        return;
    };
    if aim.reset {
        aim.dir = transform.forward().as_vec3();
        aim.reset = false;
    }
    if *mode != ControlMode::MouseAim {
        return;
    }
    let nose = transform.forward().as_vec3();
    if aim.dir.angle_between(nose) > MAX_AIM_ANGLE.to_radians() {
        let axis = nose.cross(aim.dir).normalize_or(Vec3::Y);
        aim.dir = Quat::from_axis_angle(axis, MAX_AIM_ANGLE.to_radians()) * nose;
    }
    let grabbed = cursor
        .single()
        .is_ok_and(|c| c.grab_mode != CursorGrabMode::None);
    if !grabbed {
        return;
    }
    // rotate the aim direction around world up (yaw) and the camera's right (pitch)
    let sensitivity = 0.0025;
    let delta = motion.delta * sensitivity;
    let yaw = Quat::from_rotation_y(-delta.x);
    let right = aim.dir.cross(Vec3::Y).normalize_or(Vec3::X);
    let pitch = Quat::from_axis_angle(right, -delta.y);
    let new_dir = (yaw * pitch * aim.dir).normalize();
    // don't let the aim flip over the poles
    let new_dir = if new_dir.y.abs() < 0.98 {
        new_dir
    } else {
        (yaw * aim.dir).normalize()
    };
    // Keep the aim within reach of pitch + rudder: it can't wander behind the
    // plane. Big direction changes need a roll (keyboard/stick) first.
    let nose = transform.forward().as_vec3();
    let max_angle = MAX_AIM_ANGLE.to_radians();
    let angle = new_dir.angle_between(nose);
    aim.dir = if angle > max_angle {
        let axis = nose.cross(new_dir).normalize_or(Vec3::Y);
        Quat::from_axis_angle(axis, max_angle) * nose
    } else {
        new_dir
    };
}

/// How far (degrees) the mouse-aim point may get from the nose.
const MAX_AIM_ANGLE: f32 = 40.0;

fn update_look_around(
    gamepads: Query<&Gamepad>,
    keys: Res<ButtonInput<KeyCode>>,
    time: Res<Time>,
    mut look: ResMut<LookAround>,
) {
    let stick = first_gamepad(&gamepads).map_or(Vec2::ZERO, |g| g.right_stick());
    let stick = if stick.length() < 0.15 { Vec2::ZERO } else { stick };
    // C = look back
    let target = if keys.pressed(KeyCode::KeyC) {
        Vec2::new(std::f32::consts::PI, 0.1)
    } else {
        Vec2::new(-stick.x * 2.6, stick.y * 1.2)
    };
    let t = (time.delta_secs() * 8.0).min(1.0);
    look.0 = look.0.lerp(target, t);
}

fn axis(keys: &ButtonInput<KeyCode>, pos: &[KeyCode], neg: &[KeyCode]) -> f32 {
    let p = pos.iter().any(|k| keys.pressed(*k)) as i32 as f32;
    let n = neg.iter().any(|k| keys.pressed(*k)) as i32 as f32;
    p - n
}

fn deadzone(v: f32) -> f32 {
    if v.abs() < 0.12 {
        0.0
    } else {
        (v - 0.12 * v.signum()) / 0.88
    }
}

#[allow(clippy::too_many_arguments)]
fn player_flight_input(
    keys: Res<ButtonInput<KeyCode>>,
    gamepads: Query<&Gamepad>,
    time: Res<Time>,
    mode: Res<ControlMode>,
    aim: Res<MouseAim>,
    mut lever: ResMut<ThrottleLever>,
    mut player: Query<
        (&mut ControlInput, &Transform, &FlightState),
        (With<Player>, Without<crate::ai::Pilot>),
    >,
) {
    let Ok((mut input, transform, state)) = player.single_mut() else {
        return;
    };
    let dt = time.delta_secs();
    let pad = first_gamepad(&gamepads);

    // ---- stick ----
    let key_pitch = axis(&keys, &[KeyCode::KeyS, KeyCode::ArrowDown], &[KeyCode::KeyW, KeyCode::ArrowUp]);
    let key_roll = axis(&keys, &[KeyCode::KeyD, KeyCode::ArrowRight], &[KeyCode::KeyA, KeyCode::ArrowLeft]);
    let key_yaw = axis(&keys, &[KeyCode::KeyE], &[KeyCode::KeyQ]);

    let (pad_pitch, pad_roll, pad_yaw) = pad.map_or((0.0, 0.0, 0.0), |g| {
        let stick = g.left_stick();
        let yaw = g.pressed(GamepadButton::RightTrigger) as i32 as f32
            - g.pressed(GamepadButton::LeftTrigger) as i32 as f32;
        // stick forward = nose down
        (-deadzone(stick.y), deadzone(stick.x), yaw)
    });

    let manual_pitch = (key_pitch + pad_pitch).clamp(-1.0, 1.0);
    let manual_roll = (key_roll + pad_roll).clamp(-1.0, 1.0);
    let manual_yaw = (key_yaw + pad_yaw).clamp(-1.0, 1.0);

    match *mode {
        ControlMode::Direct => {
            input.pitch = manual_pitch;
            input.roll = manual_roll;
            input.yaw = manual_yaw;
        }
        ControlMode::MouseAim => {
            // the mouse trims pitch and rudder only; rolling is up to the player
            let (p, y) = fine_aim(transform, state, aim.dir);
            input.pitch = if manual_pitch != 0.0 { manual_pitch } else { p };
            input.roll = manual_roll;
            input.yaw = if manual_yaw != 0.0 { manual_yaw } else { y };
        }
    }

    // ---- throttle ----
    let up = keys.pressed(KeyCode::ShiftLeft) || keys.pressed(KeyCode::ShiftRight);
    let down = keys.pressed(KeyCode::ControlLeft) || keys.pressed(KeyCode::ControlRight);
    if up {
        lever.0 = (lever.0 + dt * 0.6).min(1.0);
    }
    if down {
        lever.0 = (lever.0 - dt * 0.6).max(0.0);
    }
    let mut throttle = lever.0;
    let mut afterburner = up && lever.0 >= 1.0;
    let mut airbrake = down && lever.0 <= 0.0;

    if let Some(g) = pad {
        let rt = g.get(GamepadButton::RightTrigger2).unwrap_or(0.0);
        let lt = g.get(GamepadButton::LeftTrigger2).unwrap_or(0.0);
        if rt > 0.05 {
            throttle = throttle.max(lever.0 + (1.0 - lever.0) * (rt / 0.9).min(1.0));
            afterburner |= rt > 0.9;
        }
        if lt > 0.05 {
            throttle = throttle.min(lever.0 * (1.0 - (lt / 0.9).min(1.0)));
            airbrake |= lt > 0.9;
        }
    }

    input.throttle = throttle;
    input.afterburner = afterburner;
    input.airbrake = airbrake;
}
