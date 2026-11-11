//! Dev helpers for headless-ish verification.
//!
//! `FLIGHT_SIM_SCREENSHOT=out.png cargo run` saves a screenshot after a few
//! seconds and exits. `FLIGHT_SIM_SCREENSHOT_AFTER=<secs>` changes the delay.

use bevy::{
    prelude::*,
    render::view::screenshot::{Screenshot, save_to_disk},
};

pub struct DevPlugin;

/// Screenshot runs render headless (no window), into this image.
pub const HEADLESS_SIZE: UVec2 = UVec2::new(2560, 1440);

pub fn headless() -> bool {
    std::env::var("FLIGHT_SIM_SCREENSHOT").is_ok()
}

/// The offscreen image the camera renders to in headless runs.
#[derive(Resource, Clone)]
pub struct HeadlessTarget(pub Handle<Image>);

#[derive(Resource)]
struct AutoScreenshot {
    path: String,
    after: f32,
    taken_at: Option<f32>,
}

impl Plugin for DevPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, log_state);
        if std::env::var("FLIGHT_SIM_FX_TEST").is_ok() {
            app.add_systems(
                FixedUpdate,
                force_afterburner
                    .after(crate::flight::FlightSystems::Pilots)
                    .before(crate::flight::FlightSystems::Physics),
            )
            .add_systems(Update, fx_test_explosion);
        }
        if std::env::var("FLIGHT_SIM_AUTOPILOT").is_ok() {
            app.add_systems(Update, autopilot_player);
        }
        if let Ok(path) = std::env::var("FLIGHT_SIM_SCREENSHOT") {
            let after = std::env::var("FLIGHT_SIM_SCREENSHOT_AFTER")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(5.0);
            app.insert_resource(AutoScreenshot {
                path,
                after,
                taken_at: None,
            })
            .add_systems(Update, auto_screenshot);
        }
    }
}

fn auto_screenshot(
    mut commands: Commands,
    target: Option<Res<HeadlessTarget>>,
    time: Res<Time>,
    mut state: ResMut<AutoScreenshot>,
    mut exit: MessageWriter<AppExit>,
) {
    let now = time.elapsed_secs();
    match state.taken_at {
        None if now >= state.after => {
            let screenshot = match &target {
                Some(t) => Screenshot::image(t.0.clone()),
                None => Screenshot::primary_window(),
            };
            commands
                .spawn(screenshot)
                .observe(save_to_disk(state.path.clone()));
            state.taken_at = Some(now);
        }
        // give the capture a moment to be written before quitting
        Some(t) if now - t > 1.0 => {
            exit.write(AppExit::Success);
        }
        _ => {}
    }
}

/// `FLIGHT_SIM_LOG=1`: print player and camera state once a second.
pub fn log_state(
    time: Res<Time>,
    mut last: Local<f32>,
    player: Query<(&Transform, &crate::flight::FlightState, &crate::flight::ControlInput), With<crate::game::Player>>,
    camera: Query<&Transform, With<crate::camera::MainCamera>>,
    aircraft: Query<(), With<crate::flight::Aircraft>>,
    diagnostics: Res<bevy::diagnostic::DiagnosticsStore>,
) {
    if std::env::var("FLIGHT_SIM_LOG").is_err() || time.elapsed_secs() - *last < 1.0 {
        return;
    }
    *last = time.elapsed_secs();
    let fps = diagnostics
        .get(&bevy::diagnostic::FrameTimeDiagnosticsPlugin::FPS)
        .and_then(|d| d.smoothed())
        .unwrap_or(0.0);
    info!("fps {fps:.0}");
    let cam = camera.single().map(|t| t.translation).ok();
    match player.single() {
        Ok((t, s, i)) => info!(
            "t={:.0} pos={:.0} fwd={:.2} spd={:.0} aoa={:.1} g={:.1} in=({:.2},{:.2},{:.2},thr {:.2}) cam={:?} aircraft={}",
            time.elapsed_secs(), t.translation, t.forward().as_vec3(), s.airspeed, s.alpha.to_degrees(), s.g_load,
            i.pitch, i.roll, i.yaw, i.throttle, cam, aircraft.iter().count()
        ),
        Err(_) => info!("t={:.0} no player, cam={:?} aircraft={}", time.elapsed_secs(), cam, aircraft.iter().count()),
    }
}

/// `FLIGHT_SIM_AUTOPILOT=1`: the AI flies (and fights) the player's jet, so
/// automated screenshot runs show real combat.
fn autopilot_player(
    mut commands: Commands,
    new: Query<Entity, (Added<crate::game::Player>, Without<crate::ai::Pilot>)>,
) {
    for e in &new {
        commands.entity(e).insert(crate::ai::Pilot::new());
    }
}

/// `FLIGHT_SIM_FX_TEST=1`: full afterburner, plus an explosion just ahead of
/// the player shortly before the screenshot.
fn force_afterburner(mut player: Query<&mut crate::flight::ControlInput, With<crate::game::Player>>) {
    for mut input in &mut player {
        input.throttle = 1.0;
        input.afterburner = true;
    }
}

fn fx_test_explosion(
    time: Res<Time>,
    shot: Option<Res<AutoScreenshot>>,
    player: Query<&Transform, With<crate::game::Player>>,
    mut explosions: MessageWriter<crate::game::Explosion>,
    mut done: Local<bool>,
) {
    let Some(shot) = shot else { return };
    if *done || time.elapsed_secs() < shot.after - 0.35 {
        return;
    }
    *done = true;
    if let Ok(t) = player.single() {
        let p = t.translation + t.forward() * 120.0 + t.right() * 25.0;
        explosions.write(crate::game::Explosion { position: p, size: 12.0 });
    }
}
