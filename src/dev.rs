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
