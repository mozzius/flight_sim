use bevy::prelude::*;
use bevy_transform_interpolation::prelude::*;

mod ai;
mod audio;
mod camera;
mod controls;
mod dev;
mod effects;
mod flight;
mod game;
mod hud;
mod terrain_gen;
mod weapons;
mod world;

use flight::FlightSystems;

fn main() {
    let mut app = App::new();
    if dev::headless() {
        // Automated screenshot runs: no window at all (nothing pops up on
        // screen), render into an offscreen image instead. See dev.rs.
        app.add_plugins(
            DefaultPlugins
                .set(WindowPlugin {
                    primary_window: None,
                    exit_condition: bevy::window::ExitCondition::DontExit,
                    ..default()
                })
                .disable::<bevy::winit::WinitPlugin>(),
        )
        .add_plugins(bevy::app::ScheduleRunnerPlugin::run_loop(
            std::time::Duration::from_secs_f64(1.0 / 120.0),
        ));
    } else {
        app.add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title: "Flight Sim".to_string(),
                ..default()
            }),
            ..default()
        }))
        // test runs are silent; only real sessions get sound
        .add_plugins(audio::AudioFxPlugin);
    }
    app
        .add_plugins(TransformInterpolationPlugin::default())
        .configure_sets(
            FixedUpdate,
            (
                FlightSystems::Pilots,
                FlightSystems::Physics,
                FlightSystems::Combat,
            )
                .chain(),
        )
        .add_plugins((
            world::WorldPlugin,
            flight::FlightPlugin,
            controls::ControlsPlugin,
            camera::CameraPlugin,
            game::GamePlugin,
            weapons::WeaponsPlugin,
            ai::AIPlugin,
            hud::HUDPlugin,
            effects::EffectsPlugin,
            dev::DevPlugin,
        ))
        .run();
}
