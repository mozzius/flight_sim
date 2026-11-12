use bevy::prelude::*;
use bevy_transform_interpolation::prelude::*;

mod ai;
mod audio;
mod camera;
mod clouds;
mod controls;
mod dev;
mod effects;
mod flight;
mod game;
mod hud;
mod intro;
mod pause;
mod platform;
mod respawn;
mod terrain_gen;
mod title;
mod touch;
mod weapons;
mod world;

use flight::FlightSystems;

fn main() {
    // `FLIGHT_SIM_BAKE_MAP=<path>`: write the built-in map pre-baked for the
    // web build (web texture resolution) and exit. See scripts/build-web.sh.
    if let Ok(path) = std::env::var("FLIGHT_SIM_BAKE_MAP") {
        let bake = terrain_gen::MapBake::bake(title::BUILTIN_SEED, 2);
        let bytes = bake.to_bytes();
        std::fs::write(&path, &bytes).expect("writing the map file");
        println!("wrote {path} ({:.1} MB)", bytes.len() as f64 / 1e6);
        return;
    }
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
        app.add_plugins(DefaultPlugins.set(AssetPlugin {
            // no .meta files: skip probing for them (on the web each is a 404 request)
            meta_check: bevy::asset::AssetMetaCheck::Never,
            ..default()
        }).set(WindowPlugin {
            primary_window: Some(Window {
                title: "Flight Sim".to_string(),
                // web build: draw into the page's canvas and keep keys away from the browser
                canvas: Some("#bevy".to_string()),
                fit_canvas_to_parent: true,
                prevent_default_event_handling: true,
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
            clouds::CloudsPlugin,
            game::GamePlugin,
            weapons::WeaponsPlugin,
            ai::AIPlugin,
            hud::HUDPlugin,
            pause::PausePlugin,
            respawn::RespawnPlugin,
            intro::IntroPlugin,
            touch::TouchPlugin,
            effects::EffectsPlugin,
            dev::DevPlugin,
        ))
        .add_plugins(title::TitlePlugin)
        .run();
}
