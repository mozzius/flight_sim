//! The challenge leaderboard, client side.
//!
//! Signed in, a challenge round tells the server when it starts
//! (`ChallengeStart`, with the map seed) and submits the run when the player
//! goes down (score, wave, kills, game seconds); the server checks it and
//! answers with our best and global rank, which the score screen shows next
//! to BEST (`modes`). The server side is `src/server/leaderboard.rs`.
//!
//! The LEADERBOARD screen (from the score screen, or the menu's top-left
//! corner) has GLOBAL / FRIENDS tabs: `#  @handle  score  W<wave>` rows, ours
//! highlighted (and added under the top 20 if we're further down), a dash
//! when there's nothing. Signed out, the score screen's button says SIGN IN
//! (and the corner one hides: the account corner says it already).
//!
//! Dev: `FLIGHT_SIM_DEV_RUN=<kills>,<wave>` swaps in a fake (but plausible)
//! run when the challenge ends; `FLIGHT_SIM_DEV_BOARD=global|friends` opens
//! that board once online in the menu, or a few seconds after the score
//! screen gets our rank.

use bevy::prelude::*;

use crate::{
    game::Score,
    loading::{GameState, MapSeed},
    menu::{BG, BORDER, BUTTON, DIM, GREEN, Pressed, Screen, UiAction, UiButton, button, font_of},
    modes::{GameMode, challenge_over},
    net::{
        Net,
        protocol::{BoardKind, KILL_POINTS, Run, WAVE_POINTS},
    },
    respawn::RespawnPrompt,
};

pub struct LeaderboardPlugin;

impl Plugin for LeaderboardPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<BoardUi>()
            .init_resource::<RunClock>()
            .add_systems(Startup, spawn_board)
            .add_systems(OnEnter(GameState::Playing), start_run)
            .add_systems(OnEnter(GameState::Menu), spawn_corner)
            .add_systems(OnExit(GameState::Menu), (despawn_corner, close))
            .add_systems(OnExit(GameState::Playing), close)
            .add_systems(PreUpdate, back.after(bevy::input::InputSystems))
            .add_systems(Update, (submit_run, dev_board, label_buttons, layout_corner, layout_board).chain());
    }
}

/// Whether the leaderboard screen is up, and which board it shows.
#[derive(Resource)]
pub struct BoardUi {
    pub open: bool,
    pub kind: BoardKind,
    /// What the rows were last built from: (board version, kind, ours).
    built: Option<(u32, BoardKind, Option<String>)>,
}

impl Default for BoardUi {
    fn default() -> Self {
        Self { open: false, kind: BoardKind::Global, built: None }
    }
}

/// This challenge round, for the submission.
#[derive(Resource, Default)]
struct RunClock {
    /// (map seed, game seconds at the start), when signed in.
    start: Option<(u32, f64)>,
    submitted: bool,
}

#[derive(Component)]
struct BoardRoot;

#[derive(Component)]
struct BoardRows;

#[derive(Component)]
struct Corner;

const ME: Color = Color::srgb(0.9, 1.0, 0.92);
/// Handles longer than this are cut.
const HANDLE_COLS: usize = 24;

fn open_board(board: &mut BoardUi, net: &mut Net, kind: BoardKind) {
    board.open = true;
    board.kind = kind;
    net.request_board(kind);
}

fn close(mut board: ResMut<BoardUi>) {
    board.open = false;
}

fn start_run(mode: Res<GameMode>, seed: Res<MapSeed>, time: Res<Time<Virtual>>, mut clock: ResMut<RunClock>, mut net: ResMut<Net>) {
    clock.submitted = false;
    clock.start = None;
    if *mode == GameMode::Challenge && crate::identity::current_user().is_some() {
        clock.start = Some((seed.0, time.elapsed_secs_f64()));
        net.challenge_start(seed.0);
    }
}

/// `FLIGHT_SIM_DEV_RUN=<kills>,<wave>`.
fn dev_run() -> Option<(u32, u32)> {
    let s = std::env::var("FLIGHT_SIM_DEV_RUN").ok()?;
    let (k, w) = s.split_once(',')?;
    Some((k.trim().parse().ok()?, w.trim().parse::<u32>().ok()?.max(1)))
}

/// The run goes to the server when the challenge ends.
fn submit_run(
    mode: Res<GameMode>,
    prompt: Res<RespawnPrompt>,
    time: Res<Time<Virtual>>,
    mut score: ResMut<Score>,
    mut clock: ResMut<RunClock>,
    mut net: ResMut<Net>,
) {
    if clock.submitted || !challenge_over(&mode, &prompt) {
        return;
    }
    clock.submitted = true;
    let Some((seed, start)) = clock.start else { return };
    if let Some((kills, wave)) = dev_run() {
        // (as if every kill was a hostile and we went down in the last wave)
        score.kills = kills;
        score.wave = wave;
        score.points = KILL_POINTS * kills + WAVE_POINTS * (wave - 1) * wave / 2;
    }
    let secs = (time.elapsed_secs_f64() - start) as f32;
    net.submit_score(Run { seed, score: score.points, wave: score.wave, kills: score.kills, secs });
}

/// The leaderboard's buttons (taken out before the menu sees them: `menu` runs this
/// between its navigation and its own presses).
#[allow(clippy::too_many_arguments)]
pub fn handle_pressed(
    mut pressed: ResMut<Pressed>,
    mut board: ResMut<BoardUi>,
    mut net: ResMut<Net>,
    state: Res<State<GameState>>,
    mut next: ResMut<NextState<GameState>>,
    mut sign_in: ResMut<crate::signin::SignInPrompt>,
) {
    let mut mine = vec![];
    pressed.0.retain(|(a, _)| {
        let ours = matches!(a, UiAction::Leaderboard | UiAction::BoardTab(_) | UiAction::CloseBoard);
        if ours {
            mine.push(*a);
        }
        !ours
    });
    for action in mine {
        match action {
            UiAction::Leaderboard if crate::identity::current_user().is_some() => {
                let kind = board.kind;
                open_board(&mut board, &mut net, kind);
            }
            UiAction::Leaderboard => {
                // (natively the prompt is the menu's)
                if *state.get() != GameState::Menu {
                    next.set(GameState::Menu);
                }
                crate::signin::sign_in(&mut sign_in);
            }
            UiAction::BoardTab(kind) => open_board(&mut board, &mut net, kind),
            UiAction::CloseBoard => board.open = false,
            _ => {}
        }
    }
}

/// Esc / (B) closes the board (and nothing else: not the pause menu, not a panel).
fn back(mut board: ResMut<BoardUi>, mut keys: ResMut<ButtonInput<KeyCode>>, mut gamepads: Query<&mut Gamepad>) {
    if !board.open {
        return;
    }
    let pad = gamepads.iter().any(|g| g.just_pressed(GamepadButton::East));
    if keys.just_pressed(KeyCode::Escape) || pad {
        board.open = false;
        keys.clear_just_pressed(KeyCode::Escape);
        for mut g in &mut gamepads {
            g.digital_mut().clear_just_pressed(GamepadButton::East);
        }
    }
}

/// `FLIGHT_SIM_DEV_BOARD=global|friends`.
fn dev_board(
    state: Res<State<GameState>>,
    mode: Res<GameMode>,
    prompt: Res<RespawnPrompt>,
    time: Res<Time<Real>>,
    mut board: ResMut<BoardUi>,
    mut net: ResMut<Net>,
    mut ranked_at: Local<Option<f32>>,
    mut done: Local<bool>,
) {
    if *done {
        return;
    }
    let Some(kind) = std::env::var("FLIGHT_SIM_DEV_BOARD").ok().map(|k| match k.to_ascii_lowercase().as_str() {
        "friends" => BoardKind::Friends,
        _ => BoardKind::Global,
    }) else {
        *done = true;
        return;
    };
    let now = time.elapsed_secs();
    let ranked = challenge_over(&mode, &prompt) && net.standing().is_some();
    if !ranked {
        *ranked_at = None;
    } else if ranked_at.is_none() {
        *ranked_at = Some(now);
    }
    let go = (*state.get() == GameState::Menu && net.online()) || ranked_at.is_some_and(|t| now - t > 3.0);
    if go {
        *done = true;
        open_board(&mut board, &mut net, kind);
    }
}

fn spawn_board(mut commands: Commands, asset_server: Res<AssetServer>) {
    let font: Handle<Font> = asset_server.load("fonts/RobotoMono/RobotoMono-Medium.ttf");
    commands
        .spawn((
            BoardRoot,
            crate::intro::KeepDuringIntro,
            Node {
                position_type: PositionType::Absolute,
                width: percent(100),
                height: percent(100),
                flex_direction: FlexDirection::Column,
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                row_gap: px(16),
                display: Display::None,
                ..default()
            },
            BackgroundColor(BG),
            GlobalZIndex(10),
        ))
        .with_children(|root| {
            root.spawn(Node { column_gap: px(14), ..default() }).with_children(|row| {
                button(row, &font, "GLOBAL", Screen::Leaderboard, 0, 0, UiAction::BoardTab(BoardKind::Global));
                button(row, &font, "FRIENDS", Screen::Leaderboard, 1, 0, UiAction::BoardTab(BoardKind::Friends));
            });
            root.spawn((
                BoardRows,
                Node {
                    flex_direction: FlexDirection::Column,
                    align_items: AlignItems::Stretch,
                    min_width: px(520),
                    min_height: px(120),
                    justify_content: JustifyContent::Center,
                    ..default()
                },
            ));
            button(root, &font, "BACK", Screen::Leaderboard, 2, 1, UiAction::CloseBoard);
        });
}

/// The menu's LEADERBOARD corner (top left), while signed in.
fn spawn_corner(mut commands: Commands, asset_server: Res<AssetServer>) {
    let font: Handle<Font> = asset_server.load("fonts/RobotoMono/RobotoMono-Medium.ttf");
    let action = UiAction::Leaderboard;
    commands
        .spawn((
            Corner,
            crate::intro::KeepDuringIntro,
            Node { position_type: PositionType::Absolute, top: px(12), left: px(12), display: Display::None, ..default() },
            GlobalZIndex(7),
        ))
        .with_children(|corner| {
            corner
                .spawn((
                    // (focus: the account corner's row, to its left)
                    UiButton { screen: Screen::Modes, order: 9, row: 9, action },
                    bevy::picking::hover::Hovered::default(),
                    Node {
                        min_height: px(36),
                        padding: UiRect::axes(px(12), px(6)),
                        border: UiRect::all(px(1)),
                        border_radius: BorderRadius::all(px(6)),
                        justify_content: JustifyContent::Center,
                        align_items: AlignItems::Center,
                        ..default()
                    },
                    BorderColor::all(BORDER),
                    BackgroundColor(BUTTON),
                ))
                .with_child((Text::new("LEADERBOARD"), font_of(&font, 13.0, GREEN), Pickable::IGNORE))
                .observe(move |_: On<PointerClick>, mut pressed: ResMut<Pressed>| pressed.0.push((action, 1)));
        });
}

fn despawn_corner(mut commands: Commands, corners: Query<Entity, With<Corner>>) {
    for e in &corners {
        commands.entity(e).despawn();
    }
}

fn layout_corner(net: Res<Net>, mut corner: Query<&mut Node, With<Corner>>) {
    let Ok(mut node) = corner.single_mut() else { return };
    let show = crate::identity::current_user().is_some() && !net.in_lobby();
    let d = if show { Display::Flex } else { Display::None };
    if node.display != d {
        node.display = d;
    }
    let s = crate::platform::safe_area();
    let (top, left) = (px(s.y + 12.0), px(s.x + 12.0));
    if node.top != top || node.left != left {
        node.top = top;
        node.left = left;
    }
}

/// The score screen's LEADERBOARD says SIGN IN when signed out (and hides
/// where signing in isn't possible); the tabs light up the open board.
fn label_buttons(
    board: Res<BoardUi>,
    mut buttons: Query<(&UiButton, &Children, &mut Node)>,
    mut texts: Query<(&mut Text, &mut TextColor)>,
) {
    let signed_in = crate::identity::current_user().is_some();
    for (b, children, mut node) in &mut buttons {
        let (label, color) = match b.action {
            UiAction::Leaderboard if b.screen == Screen::Score => {
                let d = if signed_in || crate::identity::can_sign_in() { Display::Flex } else { Display::None };
                if node.display != d {
                    node.display = d;
                }
                (if signed_in { "LEADERBOARD" } else { "SIGN IN" }, GREEN)
            }
            UiAction::BoardTab(kind) => {
                let label = if kind == BoardKind::Global { "GLOBAL" } else { "FRIENDS" };
                (label, if kind == board.kind { GREEN } else { DIM })
            }
            _ => continue,
        };
        for child in children.iter() {
            if let Ok((mut text, mut tc)) = texts.get_mut(child) {
                if text.0 != label {
                    text.0 = label.into();
                }
                if tc.0 != color {
                    tc.0 = color;
                }
            }
        }
    }
}

/// `@handle`, cut to fit.
fn short_handle(handle: &str) -> String {
    if handle.chars().count() <= HANDLE_COLS {
        handle.to_string()
    } else {
        format!("{}…", handle.chars().take(HANDLE_COLS - 1).collect::<String>())
    }
}

/// One board line: `#  @handle  score  W<wave>`.
fn row_text(rank: u32, handle: &str, score: u32, wave: u32) -> String {
    format!("{rank:>3}  @{:<w$}  {score:>8}  W{wave}", short_handle(handle), w = HANDLE_COLS)
}

fn layout_board(
    mut commands: Commands,
    asset_server: Res<AssetServer>,
    net: Res<Net>,
    mut board: ResMut<BoardUi>,
    mut root: Query<&mut Node, With<BoardRoot>>,
    rows: Query<Entity, With<BoardRows>>,
) {
    let Ok(mut node) = root.single_mut() else { return };
    let d = if board.open { Display::Flex } else { Display::None };
    if node.display != d {
        node.display = d;
    }
    let s = crate::platform::safe_area();
    let padding = UiRect { left: px(s.x), top: px(s.y), right: px(s.z), bottom: px(s.w) };
    if node.padding != padding {
        node.padding = padding;
    }
    if !board.open {
        return;
    }
    let me = crate::identity::current_user();
    let key = (net.board_version(), board.kind, me.as_ref().map(|u| u.did.clone()));
    if board.built.as_ref() == Some(&key) {
        return;
    }
    board.built = Some(key);
    let Ok(rows) = rows.single() else { return };
    let font: Handle<Font> = asset_server.load("fonts/RobotoMono/RobotoMono-Medium.ttf");
    commands.entity(rows).despawn_children();
    // (nothing yet: blank until the board arrives)
    let Some((list, you)) = net.board(board.kind) else { return };
    let my_did = me.as_ref().map(|u| u.did.as_str());
    let mut lines: Vec<(String, bool)> =
        list.iter().map(|r| (row_text(r.rank, &r.handle, r.score, r.wave), Some(r.did.as_str()) == my_did)).collect();
    if let (Some(you), Some(me)) = (you, &me)
        && !lines.iter().any(|l| l.1)
    {
        lines.push(("...".into(), false));
        lines.push((row_text(you.rank, &me.handle, you.score, you.wave), true));
    }
    commands.entity(rows).with_children(|parent| {
        if lines.is_empty() {
            parent.spawn((Text::new("—"), font_of(&font, 18.0, DIM), Node { align_self: AlignSelf::Center, ..default() }));
        }
        for (line, mine) in lines {
            parent.spawn((
                Text::new(line),
                font_of(&font, 17.0, if mine { ME } else { GREEN }),
                Node { padding: UiRect::axes(px(10), px(3)), ..default() },
                BackgroundColor(if mine { GREEN.with_alpha(0.18) } else { Color::NONE }),
            ));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_line_up() {
        let a = row_text(1, "alice.bsky.social", 12_350, 9);
        let b = row_text(20, "a-very-long-handle-indeed.example.com", 550, 2);
        assert!(a.starts_with("  1  @alice.bsky.social ") && a.ends_with(" 12350  W9"), "{a:?}");
        assert_eq!(a.chars().count(), b.chars().count());
        assert!(b.contains("…"));
    }
}
