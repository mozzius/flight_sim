//! Title screen and map loading: `Title` (pick a map) -> `Loading` (bake or
//! download it) -> `Playing` (spawn the aircraft, the intro shot runs).
//!
//! Two maps: the built-in one (a fixed seed; the web build downloads it
//! pre-baked from `assets/maps/builtin.map`) and a random one generated on the
//! spot. `FLIGHT_SIM_MAP=builtin|random` or `FLIGHT_SIM_SEED=<n>` skip the
//! title screen (as do screenshot runs, unless `FLIGHT_SIM_MAP=title`).

use bevy::{picking::hover::Hovered, prelude::*};

use crate::{
    intro::KeepDuringIntro,
    terrain_gen::{self, MapBake, MapFile, SceneryMaterial, TerrainMaterial},
    world::Terrain,
};

pub struct TitlePlugin;

impl Plugin for TitlePlugin {
    fn build(&self, app: &mut App) {
        let seed: Option<u32> = std::env::var("FLIGHT_SIM_SEED").ok().and_then(|s| s.parse().ok());
        let preset = match std::env::var("FLIGHT_SIM_MAP").as_deref() {
            Ok("builtin") => Some(MapChoice::Builtin),
            Ok("random") => Some(MapChoice::Random(seed.unwrap_or_else(rand::random))),
            Ok("title") => None,
            _ => seed.map(MapChoice::Random).or_else(|| {
                // screenshots/tests fly the built-in map unless told otherwise
                crate::dev::headless().then_some(MapChoice::Builtin)
            }),
        };
        if let Some(choice) = preset {
            app.insert_resource(choice).insert_state(GameState::Loading);
        } else {
            app.init_state::<GameState>();
        }
        app.add_systems(OnEnter(GameState::Title), spawn_title)
            .add_systems(OnExit(GameState::Title), despawn::<TitleRoot>)
            .add_systems(OnEnter(GameState::Loading), spawn_loading)
            .add_systems(OnExit(GameState::Loading), despawn::<LoadingRoot>)
            .add_systems(Update, (title_input.run_if(in_state(GameState::Title)), load_map.run_if(in_state(GameState::Loading))));
    }
}

#[derive(States, Default, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GameState {
    #[default]
    Title,
    Loading,
    Playing,
}

/// Which map to fly.
#[derive(Resource, Clone, Copy, Debug)]
pub enum MapChoice {
    Builtin,
    Random(u32),
}

/// The built-in map's seed (the original map from before maps were random).
pub const BUILTIN_SEED: u32 = 1337;
pub const BUILTIN_FILE: &str = "maps/builtin.map";

#[derive(Component)]
struct TitleRoot;

#[derive(Component)]
struct LoadingRoot;

#[derive(Component, Clone, Copy)]
struct MapButton;

/// Set by a button's click observer, read by `title_input`.
#[derive(Resource)]
struct Picked(MapChoice);

const GREEN: Color = Color::srgb(0.62, 0.9, 0.63);
const DIM: Color = Color::srgb(0.37, 0.55, 0.39);
const PANEL: Color = Color::srgba(0.02, 0.05, 0.04, 0.82);

fn despawn<T: Component>(mut commands: Commands, roots: Query<Entity, With<T>>) {
    for e in &roots {
        commands.entity(e).despawn();
    }
}

fn font(asset_server: &AssetServer, size: f32) -> TextFont {
    TextFont {
        font: asset_server.load("fonts/RobotoMono/RobotoMono-Medium.ttf").into(),
        font_size: FontSize::Px(size),
        ..default()
    }
}

fn full_screen() -> Node {
    Node {
        position_type: PositionType::Absolute,
        width: percent(100),
        height: percent(100),
        flex_direction: FlexDirection::Column,
        justify_content: JustifyContent::Center,
        align_items: AlignItems::Center,
        row_gap: px(14),
        padding: UiRect::all(px(16)),
        ..default()
    }
}

fn spawn_title(mut commands: Commands, asset_server: Res<AssetServer>) {
    let random_note = if cfg!(target_arch = "wasm32") {
        "new terrain every time (takes ~15 s to generate)"
    } else {
        "new terrain every time"
    };
    let button = |title: &str, note: &str| {
        (
            MapButton,
            Hovered::default(),
            Node {
                width: px(340),
                max_width: percent(90),
                flex_direction: FlexDirection::Column,
                align_items: AlignItems::Center,
                padding: UiRect::axes(px(16), px(12)),
                border: UiRect::all(px(2)),
                border_radius: BorderRadius::all(px(6)),
                ..default()
            },
            BorderColor::all(GREEN),
            BackgroundColor(PANEL),
            children![
                (Text::new(title), font(&asset_server, 22.0), TextColor(GREEN), Pickable::IGNORE),
                (
                    Text::new(note),
                    font(&asset_server, 13.0),
                    TextColor(DIM),
                    TextLayout::justify(Justify::Center),
                    Pickable::IGNORE,
                ),
            ],
        )
    };
    commands
        .spawn((TitleRoot, KeepDuringIntro, full_screen(), BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.35)), GlobalZIndex(5)))
        .with_children(|root| {
            root.spawn((Text::new("FLIGHT SIM"), font(&asset_server, 44.0), TextColor(GREEN)));
            root.spawn((Text::new("CHOOSE A MAP"), font(&asset_server, 15.0), TextColor(DIM)));
            root.spawn(button("VALLEY", "the built-in map  [1]"))
                .observe(|_: On<PointerClick>, mut commands: Commands| {
                    commands.insert_resource(Picked(MapChoice::Builtin));
                });
            root.spawn(button("RANDOM", &format!("{random_note}  [2]")))
                .observe(|_: On<PointerClick>, mut commands: Commands| {
                    commands.insert_resource(Picked(MapChoice::Random(rand::random())));
                });
        });
}

fn title_input(
    mut commands: Commands,
    keys: Res<ButtonInput<KeyCode>>,
    gamepads: Query<&Gamepad>,
    picked: Option<Res<Picked>>,
    mut buttons: Query<(&Hovered, &mut BackgroundColor), With<MapButton>>,
    mut next: ResMut<NextState<GameState>>,
) {
    for (hovered, mut bg) in &mut buttons {
        bg.0 = if hovered.get() { Color::srgba(0.12, 0.3, 0.14, 0.9) } else { PANEL };
    }
    let pad = |b| gamepads.iter().any(|g| g.just_pressed(b));
    let choice = if let Some(p) = picked {
        Some(p.0)
    } else if keys.just_pressed(KeyCode::Digit1) || keys.just_pressed(KeyCode::Enter) || pad(GamepadButton::South) {
        Some(MapChoice::Builtin)
    } else if keys.just_pressed(KeyCode::Digit2) || pad(GamepadButton::West) {
        Some(MapChoice::Random(rand::random()))
    } else {
        None
    };
    if let Some(choice) = choice {
        commands.remove_resource::<Picked>();
        commands.insert_resource(choice);
        next.set(GameState::Loading);
    }
}

fn spawn_loading(mut commands: Commands, asset_server: Res<AssetServer>, choice: Res<MapChoice>) {
    let text = match *choice {
        MapChoice::Builtin => "LOADING VALLEY…".to_string(),
        MapChoice::Random(seed) if cfg!(target_arch = "wasm32") => {
            format!("GENERATING MAP {seed}…\n(about 15 s; the page freezes meanwhile)")
        }
        MapChoice::Random(seed) => format!("GENERATING MAP {seed}…"),
    };
    commands.spawn((LoadingRoot, KeepDuringIntro, full_screen(), BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.5)), GlobalZIndex(5), children![(
        Text::new(text),
        font(&asset_server, 20.0),
        TextColor(GREEN),
        TextLayout::justify(Justify::Center),
    )]));
}

/// Waits a few frames so the loading text is on screen (generating blocks the
/// main thread), gets the map (downloaded on the web for the built-in one),
/// spawns it and starts the round.
#[allow(clippy::too_many_arguments)]
fn load_map(
    mut frames: Local<u32>,
    mut file: Local<Option<Handle<MapFile>>>,
    choice: Res<MapChoice>,
    asset_server: Res<AssetServer>,
    files: Res<Assets<MapFile>>,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<TerrainMaterial>>,
    mut scenery: ResMut<Assets<SceneryMaterial>>,
    mut images: ResMut<Assets<Image>>,
    mut terrain: ResMut<Terrain>,
    mut next: ResMut<NextState<GameState>>,
) {
    *frames += 1;
    if *frames < 3 {
        return;
    }
    let bake = match *choice {
        // the web build is single-threaded: download the built-in map pre-baked
        MapChoice::Builtin if cfg!(target_arch = "wasm32") => {
            let handle = file.get_or_insert_with(|| asset_server.load(BUILTIN_FILE)).clone();
            if let Some(f) = files.get(&handle) {
                MapBake::from_bytes(&f.0).unwrap_or_else(|e| {
                    warn!("built-in map file is unusable ({e}); generating it");
                    MapBake::bake(BUILTIN_SEED, terrain_gen::TEXTURE_DIV)
                })
            } else if asset_server.load_state(&handle).is_failed() {
                warn!("couldn't load {BUILTIN_FILE}; generating it");
                MapBake::bake(BUILTIN_SEED, terrain_gen::TEXTURE_DIV)
            } else {
                return;
            }
        }
        MapChoice::Builtin => MapBake::bake(BUILTIN_SEED, terrain_gen::TEXTURE_DIV),
        MapChoice::Random(seed) => MapBake::bake(seed, terrain_gen::TEXTURE_DIV),
    };
    let generated =
        terrain_gen::spawn_map(bake, &mut commands, &mut meshes, &mut materials, &mut scenery, &mut images);
    terrain.set(generated);
    *frames = 0;
    *file = None;
    next.set(GameState::Playing);
}
