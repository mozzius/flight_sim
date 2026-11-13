//! Game modes.
//!
//! - Endless: respawns, waves forever, the environment picked in the menu.
//! - Challenge: a random map, one life and ever bigger, sharper waves
//!   (`game::spawn_enemy_wave`). Kills and cleared waves score points; when
//!   the player goes down the score screen shows the score and the best
//!   (kept in `localStorage` on the web, a small file natively).
//!
//! `FLIGHT_SIM_MODE=endless|challenge` (`?mode=` on the web) skips the menu;
//! screenshot runs default to endless. `FLIGHT_SIM_MODE=menu` (or
//! `menu-endless`, with the pickers open) shows the menu in screenshot runs.

use bevy::prelude::*;

use crate::{
    game::Score,
    loading::GameState,
    menu::{Screen, UiAction, button},
    pause::Paused,
    respawn::RespawnPrompt,
};

pub struct ModesPlugin;

impl Plugin for ModesPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(Best { value: load_best(), saved: false })
            .add_systems(Startup, spawn_ui)
            .add_systems(OnEnter(GameState::Playing), |mut best: ResMut<Best>| best.saved = false)
            .add_systems(Update, (record_best, update_ui).chain());
    }
}

#[derive(Resource, Clone, Copy, PartialEq, Eq, Debug)]
pub enum GameMode {
    Endless,
    Challenge,
    /// A free-for-all with other players (see `net`).
    Multiplayer,
}

impl GameMode {
    /// The mode to start straight into, skipping the menu (`FLIGHT_SIM_MODE`;
    /// screenshot runs default to endless).
    pub fn from_settings() -> Option<Self> {
        // (multiplayer dev runs start from the menu, see `net`)
        if crate::net::dev_auto().is_some() {
            return None;
        }
        match crate::environment::setting("FLIGHT_SIM_MODE").map(|s| s.to_ascii_lowercase()).as_deref() {
            Some("endless") => Some(Self::Endless),
            Some("challenge") => Some(Self::Challenge),
            Some(m) if m.starts_with("menu") => None,
            Some(m) => {
                warn!("unknown FLIGHT_SIM_MODE {m:?}");
                None
            }
            None => crate::dev::headless().then_some(Self::Endless),
        }
    }
}

/// Points per kill, and per cleared wave (times the wave number).
pub const KILL_POINTS: u32 = 100;
pub const WAVE_POINTS: u32 = 250;

/// The best challenge score.
#[derive(Resource)]
struct Best {
    value: u32,
    /// This round's score has been recorded.
    saved: bool,
}

/// The challenge is over (the player went down) and the score screen is up.
pub fn challenge_over(mode: &GameMode, prompt: &RespawnPrompt) -> bool {
    *mode == GameMode::Challenge && prompt.ready()
}

#[derive(Component)]
struct ScoreScreen;

#[derive(Component)]
struct FinalScore;

#[derive(Component)]
struct BestText;

/// Top-right score during a challenge.
#[derive(Component)]
struct LiveScore;

/// "WAVE N", briefly, as each wave arrives.
#[derive(Component)]
struct WaveBanner;

const GREEN: Color = Color::srgb(0.4, 1.0, 0.55);
const DIM: Color = Color::srgb(0.65, 0.8, 0.7);
const PANEL: Color = Color::srgba(0.02, 0.04, 0.03, 0.97);
const WAVE_BANNER_SECS: f32 = 2.5;

fn spawn_ui(mut commands: Commands, asset_server: Res<AssetServer>) {
    let font: Handle<Font> = asset_server.load("fonts/RobotoMono/RobotoMono-Medium.ttf");
    let text = |size: f32, color: Color| {
        (
            TextFont { font: font.clone().into(), font_size: FontSize::Px(size), ..default() },
            TextColor(color),
        )
    };
    let shadow = TextShadow { offset: Vec2::splat(1.5), color: Color::srgba(0.0, 0.0, 0.0, 0.65) };

    commands.spawn((
        LiveScore,
        Text::new(""),
        text(18.0, GREEN),
        shadow,
        Node { position_type: PositionType::Absolute, top: px(20), right: px(24), display: Display::None, ..default() },
    ));
    commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                width: percent(100),
                top: percent(14),
                justify_content: JustifyContent::Center,
                ..default()
            },
            Pickable::IGNORE,
        ))
        .with_child((WaveBanner, Text::new(""), text(24.0, GREEN), shadow, Visibility::Hidden));

    commands
        .spawn((
            ScoreScreen,
            crate::intro::KeepDuringIntro,
            Node {
                position_type: PositionType::Absolute,
                width: percent(100),
                height: percent(100),
                flex_direction: FlexDirection::Column,
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                row_gap: px(14),
                display: Display::None,
                ..default()
            },
            BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.45)),
            GlobalZIndex(9),
        ))
        .with_children(|root| {
            root.spawn((
                Node {
                    flex_direction: FlexDirection::Column,
                    align_items: AlignItems::Center,
                    row_gap: px(14),
                    padding: UiRect::axes(px(40), px(28)),
                    border_radius: BorderRadius::all(px(10)),
                    ..default()
                },
                BackgroundColor(PANEL),
            ))
            .with_children(|panel| {
                panel.spawn((FinalScore, Text::new("0"), text(56.0, GREEN)));
                panel.spawn((BestText, Text::new(""), text(18.0, DIM)));
                panel.spawn(Node { column_gap: px(14), margin: UiRect::top(px(12)), ..default() }).with_children(|row| {
                    button(row, &font, "RETRY", Screen::Score, 0, 0, UiAction::Retry);
                    button(row, &font, "MENU", Screen::Score, 1, 0, UiAction::Menu);
                });
            });
        });
}

/// Saves the score as the best when a challenge ends.
fn record_best(mode: Res<GameMode>, prompt: Res<RespawnPrompt>, score: Res<Score>, mut best: ResMut<Best>) {
    if best.saved || !challenge_over(&mode, &prompt) {
        return;
    }
    best.saved = true;
    if score.points > best.value {
        best.value = score.points;
        save_best(best.value);
    }
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn update_ui(
    time: Res<Time>,
    state: Res<State<GameState>>,
    mode: Res<GameMode>,
    prompt: Res<RespawnPrompt>,
    paused: Res<Paused>,
    score: Res<Score>,
    best: Res<Best>,
    intro: Res<crate::intro::Intro>,
    mut banner_left: Local<f32>,
    mut last_wave: Local<u32>,
    mut screen: Query<&mut Node, (With<ScoreScreen>, Without<LiveScore>)>,
    mut live: Query<(&mut Node, &mut Text), (With<LiveScore>, Without<ScoreScreen>)>,
    mut texts: Query<(&mut Text, Has<FinalScore>, Has<BestText>), (Without<LiveScore>, Without<WaveBanner>)>,
    mut banner: Query<(&mut Text, &mut Visibility), (With<WaveBanner>, Without<LiveScore>, Without<FinalScore>, Without<BestText>)>,
) {
    let playing = *state.get() == GameState::Playing;
    let challenge = playing && *mode == GameMode::Challenge;
    let over = challenge && challenge_over(&mode, &prompt);
    let set_display = |node: &mut Node, show: bool| {
        let d = if show { Display::Flex } else { Display::None };
        if node.display != d {
            node.display = d;
        }
    };
    if let Ok(mut node) = screen.single_mut() {
        set_display(&mut node, over && !paused.0);
    }
    if let Ok((mut node, mut text)) = live.single_mut() {
        set_display(&mut node, challenge && !over);
        let safe = crate::platform::safe_area();
        node.top = px(20.0 + safe.y);
        node.right = px(24.0 + safe.z);
        let value = score.points.to_string();
        if text.0 != value {
            text.0 = value;
        }
    }
    if over {
        for (mut text, is_score, is_best) in &mut texts {
            let value = if is_score {
                score.points.to_string()
            } else if is_best {
                format!("BEST {}", best.value.max(score.points))
            } else {
                continue;
            };
            if text.0 != value {
                text.0 = value;
            }
        }
    }

    // "WAVE N" as each challenge wave arrives (the first after the intro)
    if !challenge {
        *last_wave = 0;
        *banner_left = 0.0;
    } else if score.wave != *last_wave && !intro.playing() {
        *last_wave = score.wave;
        *banner_left = WAVE_BANNER_SECS;
    }
    *banner_left = (*banner_left - time.delta_secs()).max(0.0);
    if let Ok((mut text, mut vis)) = banner.single_mut() {
        let show = *banner_left > 0.0 && !paused.0 && !over;
        let target = if show { Visibility::Inherited } else { Visibility::Hidden };
        if *vis != target {
            *vis = target;
        }
        let value = format!("WAVE {}", score.wave);
        if text.0 != value {
            text.0 = value;
        }
    }
}

// ---------------------------------------------------------------------------
// best score storage

const BEST_KEY: &str = "flight_sim_best";

#[cfg(target_arch = "wasm32")]
fn storage() -> Option<(wasm_bindgen::JsValue, impl Fn(&str) -> Option<js_sys::Function>)> {
    use wasm_bindgen::JsCast;
    let ls = js_sys::Reflect::get(&js_sys::global(), &"localStorage".into()).ok().filter(|v| v.is_object())?;
    let ls2 = ls.clone();
    Some((ls, move |name: &str| js_sys::Reflect::get(&ls2, &name.into()).ok()?.dyn_into().ok()))
}

#[cfg(target_arch = "wasm32")]
fn load_best() -> u32 {
    (|| {
        let (ls, method) = storage()?;
        method("getItem")?.call1(&ls, &BEST_KEY.into()).ok()?.as_string()?.parse().ok()
    })()
    .unwrap_or(0)
}

#[cfg(target_arch = "wasm32")]
fn save_best(value: u32) {
    let _ = (|| {
        let (ls, method) = storage()?;
        method("setItem")?.call2(&ls, &BEST_KEY.into(), &value.to_string().into()).ok()
    })();
}

/// `<data dir>/flight_sim/best` (screenshot runs don't keep one).
#[cfg(not(target_arch = "wasm32"))]
fn best_path() -> Option<std::path::PathBuf> {
    if crate::dev::headless() {
        return None;
    }
    let home = |rest: &str| std::env::var_os("HOME").map(|h| std::path::Path::new(&h).join(rest));
    let dir = if cfg!(target_os = "macos") {
        home("Library/Application Support")
    } else if cfg!(windows) {
        std::env::var_os("APPDATA").map(Into::into)
    } else {
        std::env::var_os("XDG_DATA_HOME").map(Into::into).or_else(|| home(".local/share"))
    }?;
    Some(dir.join("flight_sim").join(BEST_KEY))
}

#[cfg(not(target_arch = "wasm32"))]
fn load_best() -> u32 {
    best_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

#[cfg(not(target_arch = "wasm32"))]
fn save_best(value: u32) {
    let Some(path) = best_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = std::fs::write(&path, value.to_string()) {
        warn!("couldn't save the best score to {}: {e}", path.display());
    }
}
