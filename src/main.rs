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
    App::new()
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title: "Flight Sim".to_string(),
                ..default()
            }),
            ..default()
        }))
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
            audio::AudioFxPlugin,
            dev::DevPlugin,
        ))
        .run();
}
