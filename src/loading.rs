//! Loading screen: every round flies a freshly generated map. The map is
//! baked a slice per frame (`terrain_gen::MapBaker`) under a progress bar,
//! then spawned, and the round starts (`GameState::Playing`, the intro shot).
//!
//! Rounds start from the mode menu (`menu`), or straight away in screenshot
//! runs and with `FLIGHT_SIM_MODE=endless|challenge` ([`begin_round`] picks a
//! seed and the environment, then the bake starts).
//!
//! `FLIGHT_SIM_SEED=<n>` flies a given map again. Screenshot runs default to
//! seed 1337 so they're comparable. The round's time of day and weather
//! (`environment`) are rolled from the same seed unless picked.

use bevy::prelude::*;

use crate::{
    environment::{Environment, Picks},
    intro::KeepDuringIntro,
    modes::GameMode,
    terrain_gen::{self, MapBaker, SceneryMaterial, TerrainMaterial},
    world::Terrain,
};

pub struct LoadingPlugin;

impl Plugin for LoadingPlugin {
    fn build(&self, app: &mut App) {
        // the scene behind the menu (replaced by the round's own before the bake)
        app.insert_resource(Environment::default());
        match GameMode::from_settings() {
            // screenshot runs and FLIGHT_SIM_MODE skip the menu
            Some(mode) => {
                let seed = round_seed();
                app.insert_resource(MapSeed(seed))
                    .insert_resource(Environment::choose(seed, Picks::from_settings()))
                    .insert_resource(mode)
                    .insert_state(GameState::Loading);
            }
            None => {
                app.insert_resource(MapSeed(0)).insert_resource(GameMode::Endless).insert_state(GameState::Menu);
            }
        }
        app.add_systems(OnEnter(GameState::Loading), spawn_loading)
            .add_systems(OnExit(GameState::Loading), despawn_loading)
            .add_systems(Update, load_map.run_if(in_state(GameState::Loading)));
    }
}

#[derive(States, Default, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GameState {
    /// The mode menu.
    #[default]
    Menu,
    Loading,
    Playing,
}

/// A fresh map seed: `FLIGHT_SIM_SEED`, else random (fixed for screenshot runs).
pub fn round_seed() -> u32 {
    std::env::var("FLIGHT_SIM_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .or_else(|| crate::dev::headless().then_some(SCREENSHOT_SEED))
        .unwrap_or_else(rand::random)
}

/// Starts a new round: a fresh seed and the environment, then the bake.
pub fn begin_round(commands: &mut Commands, next: &mut NextState<GameState>, mode: GameMode, picks: Picks) {
    let seed = round_seed();
    commands.insert_resource(MapSeed(seed));
    commands.insert_resource(Environment::choose(seed, picks));
    commands.insert_resource(mode);
    next.set(GameState::Loading);
}

/// The seed of the map being flown.
#[derive(Resource, Clone, Copy)]
pub struct MapSeed(pub u32);

const SCREENSHOT_SEED: u32 = 1337;
/// Baking time per frame: the bar keeps moving and the page stays responsive.
const FRAME_BUDGET: std::time::Duration = std::time::Duration::from_millis(30);

#[derive(Component)]
struct LoadingRoot;

#[derive(Component)]
struct BarFill;

const GREEN: Color = Color::srgb(0.62, 0.9, 0.63);
const DIM: Color = Color::srgba(0.62, 0.9, 0.63, 0.18);

fn spawn_loading(mut commands: Commands, asset_server: Res<AssetServer>) {
    commands.spawn((
        LoadingRoot,
        KeepDuringIntro,
        Node {
            position_type: PositionType::Absolute,
            width: percent(100),
            height: percent(100),
            flex_direction: FlexDirection::Column,
            justify_content: JustifyContent::Center,
            align_items: AlignItems::Center,
            row_gap: px(18),
            ..default()
        },
        BackgroundColor(Color::srgb(0.043, 0.059, 0.078)),
        GlobalZIndex(5),
        children![
            (
                Text::new("FLIGHT SIM"),
                TextFont {
                    font: asset_server.load("fonts/RobotoMono/RobotoMono-Medium.ttf").into(),
                    font_size: FontSize::Px(18.0),
                    ..default()
                },
                TextColor(GREEN),
            ),
            (
                Node { width: px(220), height: px(3), ..default() },
                BackgroundColor(DIM),
                children![(BarFill, Node { width: percent(0), height: percent(100), ..default() }, BackgroundColor(GREEN))],
            ),
        ],
    ));
}

fn despawn_loading(mut commands: Commands, roots: Query<Entity, With<LoadingRoot>>) {
    for e in &roots {
        commands.entity(e).despawn();
    }
}

#[allow(clippy::too_many_arguments)]
fn load_map(
    mut baker: Local<Option<MapBaker>>,
    mut frames: Local<u32>,
    seed: Res<MapSeed>,
    mut bar: Query<&mut Node, With<BarFill>>,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<TerrainMaterial>>,
    mut scenery: ResMut<Assets<SceneryMaterial>>,
    mut images: ResMut<Assets<Image>>,
    mut terrain: ResMut<Terrain>,
    env: Res<Environment>,
    mut next: ResMut<NextState<GameState>>,
) {
    // let the loading screen draw before the first slice
    *frames += 1;
    if *frames < 3 {
        return;
    }
    let b = baker.get_or_insert_with(|| MapBaker::new(seed.0, env.biome, terrain_gen::TEXTURE_DIV));
    let done = b.step(FRAME_BUDGET);
    // the last tenth of the bar is building the meshes
    let progress = if done.is_some() { 1.0 } else { b.progress() * 0.9 };
    for mut node in &mut bar {
        node.width = percent(progress * 100.0);
    }
    let Some(bake) = done else { return };
    let generated =
        terrain_gen::spawn_map(bake, &env.look(), &mut commands, &mut meshes, &mut materials, &mut scenery, &mut images);
    terrain.set(generated);
    *baker = None;
    *frames = 0;
    next.set(GameState::Playing);
}
