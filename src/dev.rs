//! Dev helpers for headless-ish verification.
//!
//! `FLIGHT_SIM_SCREENSHOT=out.png cargo run` saves a screenshot after a few
//! seconds and exits. `FLIGHT_SIM_SCREENSHOT_AFTER=<secs>` changes the delay.

use bevy::{
    prelude::*,
    render::view::screenshot::{Screenshot, save_to_disk},
};

pub struct DevPlugin;

#[derive(Resource)]
struct AutoScreenshot {
    path: String,
    after: f32,
    taken_at: Option<f32>,
}

impl Plugin for DevPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, log_state);
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
            .add_systems(Update, (auto_screenshot, strip_bloom));
        }
    }
}

/// Screenshots of the HDR target come back black with bloom on, so the
/// screenshot hook renders without it.
fn strip_bloom(
    mut commands: Commands,
    cams: Query<Entity, With<bevy::post_process::bloom::Bloom>>,
) {
    for cam in &cams {
        commands.entity(cam).remove::<bevy::post_process::bloom::Bloom>();
    }
}

fn auto_screenshot(
    mut commands: Commands,
    time: Res<Time>,
    mut state: ResMut<AutoScreenshot>,
    mut exit: MessageWriter<AppExit>,
) {
    let now = time.elapsed_secs();
    match state.taken_at {
        None if now >= state.after => {
            commands
                .spawn(Screenshot::primary_window())
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
