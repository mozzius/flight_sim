//! The mode menu (before every round), plus the buttons shared by the menus:
//! the mode menu, the pause menu and the challenge score screen.
//!
//! Buttons work with the mouse and touch (click / tap), and with the
//! keyboard or a gamepad: arrows / D-pad / left stick move the focus,
//! Enter / Space / (A) press, left and right cycle a picker, Esc / (B) closes
//! the endless pickers.
//!
//! In a multiplayer room that hasn't started, the menu is the lobby: the
//! room code, who's in (and who hosts), the friends list, a thin bar for the
//! map baking meanwhile, START (the host) and LEAVE (Esc / (B) too).

use bevy::{picking::hover::Hovered, prelude::*};

use crate::{
    environment::{Biome, Picks, TimeOfDay, Weather},
    loading::{GameState, begin_round},
    modes::GameMode,
    pause::Paused,
    respawn::RespawnPrompt,
};

pub struct MenuPlugin;

impl Plugin for MenuPlugin {
    fn build(&self, app: &mut App) {
        let open = crate::environment::setting("FLIGHT_SIM_MODE").is_some_and(|m| m.eq_ignore_ascii_case("menu-endless"));
        let panel = match crate::environment::setting("FLIGHT_SIM_MODE").map(|m| m.to_ascii_lowercase()).as_deref() {
            _ if open => Panel::Endless,
            Some("menu-multiplayer") => Panel::Multi,
            Some("menu-create") => Panel::Create,
            Some("menu-join") => Panel::Join,
            Some("menu-settings") => Panel::Settings,
            _ => Panel::Closed,
        };
        app.init_resource::<Pressed>()
            .init_resource::<Focus>()
            .init_resource::<JoinCode>()
            .insert_resource(MenuPicks(Picks::from_settings()))
            .insert_resource(panel)
            .add_systems(OnEnter(GameState::Menu), spawn_menu)
            .add_systems(OnExit(GameState::Menu), despawn_menu)
            .add_systems(
                Update,
                (lobby_focus, navigate, crate::leaderboard::handle_pressed, handle_pressed, style_buttons, layout_menu, layout_lobby, layout_account)
                    .chain()
                    .in_set(MenuSystems),
            )
            .add_systems(Update, crate::friends::invite_shortcut.before(handle_pressed));
    }
}

/// The menu's systems (navigation, presses, layout).
#[derive(SystemSet, Clone, PartialEq, Eq, Hash, Debug)]
pub struct MenuSystems;

/// Which set of buttons takes the keyboard / gamepad.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Screen {
    Modes,
    Pause,
    Score,
    /// The native sign-in prompt (over the mode menu).
    SignIn,
    /// The challenge leaderboard (`leaderboard`).
    Leaderboard,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pick {
    Location,
    Weather,
    Time,
    Difficulty,
    Graphics,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UiAction {
    Endless,
    Challenge,
    Pick(Pick),
    Start,
    /// The main menu's SETTINGS, and its APPLY (web: saves GRAPHICS and reloads).
    Settings,
    Apply,
    Resume,
    Menu,
    Retry,
    Multiplayer,
    /// QUICK PLAY: matchmaking into a public room.
    QuickPlay,
    /// HOST GAME (the pickers for a new hosted room) and JOIN GAME (the code keypad).
    Create,
    Join,
    SignIn,
    /// A letter of the room code (an index into `CODE_ALPHABET`).
    Key(u8),
    Delete,
    /// Sign in or out (the account corner).
    Account,
    /// INVITE on a friends row (an index into `Net::friends`).
    Invite(u8),
    /// JOIN on a friends row: into their public room, or ask their room's host.
    RequestJoin(u8),
    /// The card's JOIN / ACCEPT and ✕ (an invite, or a join request when we host).
    AcceptInvite,
    DismissInvite,
    /// The lobby's START (the host) and LEAVE.
    StartRoom,
    Leave,
    /// The native sign-in prompt's SIGN IN and CANCEL.
    SignInSubmit,
    SignInCancel,
    /// The leaderboard's buttons (handled in `leaderboard`).
    Leaderboard,
    BoardTab(crate::net::protocol::BoardKind),
    CloseBoard,
}

/// A menu button. `order` is the focus order on its screen; buttons sharing a
/// `row` sit side by side (left / right move between them).
#[derive(Component, Clone, Copy)]
pub struct UiButton {
    pub screen: Screen,
    pub order: u8,
    pub row: u8,
    pub action: UiAction,
}

/// Presses this frame: (action, step). Clicks step forward; left / right on a picker step ±1.
#[derive(Resource, Default)]
pub(crate) struct Pressed(pub(crate) Vec<(UiAction, i32)>);

/// The focused button (keyboard / gamepad / last hovered).
#[derive(Resource, Default)]
struct Focus {
    screen: Option<Screen>,
    order: u8,
}

/// The endless pickers.
#[derive(Resource)]
pub struct MenuPicks(pub Picks);

/// Which part of the mode menu is open.
#[derive(Resource, Clone, Copy, PartialEq, Eq, Debug)]
enum Panel {
    Closed,
    /// The endless pickers.
    Endless,
    /// QUICK PLAY / HOST GAME / JOIN GAME (or SIGN IN).
    Multi,
    /// The pickers, for a new hosted room.
    Create,
    /// The code keypad.
    Join,
    /// GRAPHICS.
    Settings,
}

impl Panel {
    fn multiplayer(self) -> bool {
        matches!(self, Self::Multi | Self::Create | Self::Join)
    }
}

/// The room code typed so far.
#[derive(Resource, Default)]
struct JoinCode(String);

// Focus orders and rows on the mode menu (the leaderboard and account corner
// buttons are orders 9 and 10 on row 9; up / down go by row).
const ORDER_PICKERS: u8 = 12;
const ORDER_START: u8 = 16;
const ROW_START: u8 = 7;
const ORDER_SETTINGS: u8 = 50;
const ROW_SETTINGS: u8 = 16;
const ORDER_KEYS: u8 = 20;
const ROW_KEYS: u8 = 11;
const KEYS_PER_ROW: usize = 8;
const ORDER_LOBBY: u8 = 60;
const ROW_LOBBY: u8 = 25;
const ORDER_FRIENDS: u8 = 100;
const ROW_FRIENDS: u8 = 30;

pub(crate) const BG: Color = Color::srgb(0.043, 0.059, 0.078);
pub(crate) const GREEN: Color = Color::srgb(0.4, 1.0, 0.55);
pub(crate) const DIM: Color = Color::srgb(0.65, 0.8, 0.7);
pub(crate) const BUTTON: Color = Color::srgba(0.05, 0.18, 0.09, 0.9);
const BUTTON_FOCUS: Color = Color::srgba(0.1, 0.32, 0.16, 0.95);
const BUTTON_ON: Color = Color::srgba(0.16, 0.45, 0.24, 0.95);
pub(crate) const BORDER: Color = Color::srgba(0.4, 1.0, 0.55, 0.45);

pub(crate) fn font_of(font: &Handle<Font>, size: f32, color: Color) -> (TextFont, TextColor) {
    (TextFont { font: font.clone().into(), font_size: FontSize::Px(size), ..default() }, TextColor(color))
}

/// Spawns a text button (at least 48 px tall, for fingers).
pub fn button(parent: &mut ChildSpawnerCommands, font: &Handle<Font>, label: &str, screen: Screen, order: u8, row: u8, action: UiAction) {
    parent
        .spawn((
            UiButton { screen, order, row, action },
            Hovered::default(),
            Node {
                min_width: px(180),
                min_height: px(48),
                padding: UiRect::axes(px(24), px(10)),
                border: UiRect::all(px(2)),
                border_radius: BorderRadius::all(px(6)),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                ..default()
            },
            BorderColor::all(BORDER),
            BackgroundColor(BUTTON),
        ))
        .with_child((Text::new(label), font_of(font, 20.0, GREEN), Pickable::IGNORE))
        .observe(move |_: On<PointerClick>, mut pressed: ResMut<Pressed>| pressed.0.push((action, 1)));
}

#[derive(Component)]
struct MenuRoot;

#[derive(Component)]
struct EndlessPanel;

/// The account corner (top right): the signed-in handle and SIGN OUT, or SIGN IN.
#[derive(Component)]
struct AccountCorner;

#[derive(Component)]
struct AccountHandle;

#[derive(Component)]
struct AccountLabel;

#[derive(Component)]
struct PickValue(Pick);

/// DIFFICULTY (endless only: hidden when the pickers set up a room).
#[derive(Component)]
struct EndlessOnly;

#[derive(Component)]
struct SettingsButton;

#[derive(Component)]
struct SettingsPanel;

/// Wraps APPLY (the web, with a GRAPHICS pick waiting).
#[derive(Component)]
struct ApplyButton;

/// A picker row: label on the left, `< VALUE >` on the right.
fn picker(parent: &mut ChildSpawnerCommands, font: &Handle<Font>, label: &str, pick: Pick, order: u8, row: u8) -> Entity {
    let action = UiAction::Pick(pick);
    parent
        .spawn((
            UiButton { screen: Screen::Modes, order, row, action },
            Hovered::default(),
            Node {
                width: px(374),
                min_height: px(44),
                padding: UiRect::axes(px(16), px(8)),
                border: UiRect::all(px(1)),
                border_radius: BorderRadius::all(px(6)),
                justify_content: JustifyContent::SpaceBetween,
                align_items: AlignItems::Center,
                ..default()
            },
            BorderColor::all(BORDER),
            BackgroundColor(BUTTON),
        ))
        .with_children(|b| {
            b.spawn((Text::new(label), font_of(font, 14.0, DIM), Pickable::IGNORE));
            b.spawn((PickValue(pick), Text::new(""), font_of(font, 17.0, GREEN), Pickable::IGNORE));
        })
        .observe(move |_: On<PointerClick>, mut pressed: ResMut<Pressed>| pressed.0.push((action, 1)))
        .id()
}

/// ENDLESS / CHALLENGE (hidden while the multiplayer panels are open).
#[derive(Component)]
struct ModesRow;

#[derive(Component)]
struct MultiButton;

#[derive(Component)]
struct MultiRow;

/// Shown when signed in / signed out.
#[derive(Component)]
struct SignedIn(bool);

#[derive(Component)]
struct JoinPanel;

#[derive(Component)]
struct CodeText;

#[derive(Component)]
struct StatusText;

#[derive(Component)]
struct LobbyPanel;

#[derive(Component)]
struct LobbyCode;

#[derive(Component)]
struct LobbyPlayers;

#[derive(Component)]
struct LobbyBar;

/// Wraps START (only the host sees it).
#[derive(Component)]
struct LobbyStart;

/// A small square keypad key.
fn key(parent: &mut ChildSpawnerCommands, font: &Handle<Font>, label: &str, order: u8, row: u8, action: UiAction) {
    parent
        .spawn((
            UiButton { screen: Screen::Modes, order, row, action },
            Hovered::default(),
            Node {
                width: px(38),
                height: px(40),
                border: UiRect::all(px(1)),
                border_radius: BorderRadius::all(px(5)),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                ..default()
            },
            BorderColor::all(BORDER),
            BackgroundColor(BUTTON),
        ))
        .with_child((Text::new(label), font_of(font, 17.0, GREEN), Pickable::IGNORE))
        .observe(move |_: On<PointerClick>, mut pressed: ResMut<Pressed>| pressed.0.push((action, 1)));
}

fn spawn_menu(mut commands: Commands, asset_server: Res<AssetServer>) {
    let font: Handle<Font> = asset_server.load("fonts/RobotoMono/RobotoMono-Medium.ttf");
    commands
        .spawn((
            MenuRoot,
            crate::intro::KeepDuringIntro,
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
            BackgroundColor(BG),
            GlobalZIndex(6),
        ))
        .with_children(|root| {
            root.spawn((Text::new("FLIGHT SIM"), font_of(&font, 18.0, Color::srgb(0.62, 0.9, 0.63))));
            root.spawn((ModesRow, Node { column_gap: px(14), ..default() })).with_children(|row| {
                button(row, &font, "ENDLESS", Screen::Modes, 0, 0, UiAction::Endless);
                button(row, &font, "CHALLENGE", Screen::Modes, 1, 0, UiAction::Challenge);
            });
            if crate::net::auth::available() {
                root.spawn((MultiButton, Node::default())).with_children(|row| {
                    button(row, &font, "MULTIPLAYER", Screen::Modes, 2, 1, UiAction::Multiplayer);
                });
                root.spawn((MultiRow, Node { column_gap: px(14), ..default() })).with_children(|row| {
                    let wrap = Node {
                        column_gap: px(14),
                        row_gap: px(10),
                        flex_wrap: FlexWrap::Wrap,
                        justify_content: JustifyContent::Center,
                        ..default()
                    };
                    row.spawn((SignedIn(true), wrap)).with_children(|row| {
                        button(row, &font, "QUICK PLAY", Screen::Modes, 3, 2, UiAction::QuickPlay);
                        button(row, &font, "HOST GAME", Screen::Modes, 4, 2, UiAction::Create);
                        button(row, &font, "JOIN GAME", Screen::Modes, 5, 2, UiAction::Join);
                    });
                    row.spawn((SignedIn(false), Node::default())).with_children(|row| {
                        button(row, &font, "SIGN IN", Screen::Modes, 3, 2, UiAction::SignIn);
                    });
                });
            }
            root.spawn((
                EndlessPanel,
                Node { flex_direction: FlexDirection::Column, align_items: AlignItems::Center, row_gap: px(8), ..default() },
            ))
            .with_children(|panel| {
                for (i, (pick, label)) in [
                    (Pick::Location, "LOCATION"),
                    (Pick::Weather, "WEATHER"),
                    (Pick::Time, "TIME"),
                    (Pick::Difficulty, "DIFFICULTY"),
                ]
                .into_iter()
                .enumerate()
                {
                    let e = picker(panel, &font, label, pick, ORDER_PICKERS + i as u8, 3 + i as u8);
                    if pick == Pick::Difficulty {
                        panel.commands().entity(e).insert(EndlessOnly);
                    }
                }
                panel.spawn(Node { height: px(4), ..default() });
                button(panel, &font, "START", Screen::Modes, ORDER_START, ROW_START, UiAction::Start);
            });
            root.spawn((
                JoinPanel,
                Node { flex_direction: FlexDirection::Column, align_items: AlignItems::Center, row_gap: px(4), ..default() },
            ))
            .with_children(|panel| {
                panel.spawn((CodeText, Text::new(""), font_of(&font, 28.0, GREEN), Node { margin: UiRect::bottom(px(6)), ..default() }));
                let alphabet = crate::net::protocol::CODE_ALPHABET;
                let keys: Vec<(String, UiAction)> = alphabet
                    .iter()
                    .enumerate()
                    .map(|(i, c)| ((*c as char).to_string(), UiAction::Key(i as u8)))
                    .chain([("<".to_string(), UiAction::Delete)])
                    .collect();
                for (r, chunk) in keys.chunks(KEYS_PER_ROW).enumerate() {
                    panel.spawn(Node { column_gap: px(4), ..default() }).with_children(|row| {
                        for (i, (label, action)) in chunk.iter().enumerate() {
                            let order = ORDER_KEYS + (r * KEYS_PER_ROW + i) as u8;
                            key(row, &font, label, order, ROW_KEYS + r as u8, *action);
                        }
                    });
                }
            });
            root.spawn((
                LobbyPanel,
                Node {
                    flex_direction: FlexDirection::Column,
                    align_items: AlignItems::Center,
                    row_gap: px(14),
                    display: Display::None,
                    ..default()
                },
            ))
            .with_children(|panel| {
                panel.spawn((LobbyCode, Text::new(""), font_of(&font, 64.0, GREEN)));
                panel.spawn((LobbyPlayers, Text::new(""), font_of(&font, 16.0, GREEN)));
                panel
                    .spawn((Node { width: px(220), height: px(2), ..default() }, BackgroundColor(GREEN.with_alpha(0.15))))
                    .with_child((LobbyBar, Node { width: percent(0), height: percent(100), ..default() }, BackgroundColor(GREEN.with_alpha(0.6))));
                panel.spawn(Node { column_gap: px(14), ..default() }).with_children(|row| {
                    row.spawn((LobbyStart, Node::default())).with_children(|w| {
                        button(w, &font, "START", Screen::Modes, ORDER_LOBBY, ROW_LOBBY, UiAction::StartRoom);
                    });
                    button(row, &font, "LEAVE", Screen::Modes, ORDER_LOBBY + 1, ROW_LOBBY, UiAction::Leave);
                });
            });
            root.spawn((SettingsButton, Node::default())).with_children(|row| {
                button(row, &font, "SETTINGS", Screen::Modes, ORDER_SETTINGS, ROW_SETTINGS, UiAction::Settings);
            });
            root.spawn((
                SettingsPanel,
                Node { flex_direction: FlexDirection::Column, align_items: AlignItems::Center, row_gap: px(8), ..default() },
            ))
            .with_children(|panel| {
                picker(panel, &font, "GRAPHICS", Pick::Graphics, ORDER_SETTINGS + 1, ROW_SETTINGS + 1);
                panel.spawn((ApplyButton, Node { margin: UiRect::top(px(4)), ..default() })).with_children(|w| {
                    button(w, &font, "APPLY", Screen::Modes, ORDER_SETTINGS + 2, ROW_SETTINGS + 2, UiAction::Apply);
                });
            });
            root.spawn((StatusText, Text::new(""), font_of(&font, 15.0, DIM)));
            if crate::net::auth::available() {
                crate::friends::spawn_list(root, &font, Screen::Modes, ORDER_FRIENDS, ROW_FRIENDS);
            }
            root.spawn((
                AccountCorner,
                Node {
                    position_type: PositionType::Absolute,
                    top: px(12),
                    right: px(12),
                    column_gap: px(12),
                    align_items: AlignItems::Center,
                    display: Display::None,
                    ..default()
                },
            ))
            .with_children(|corner| {
                corner.spawn((AccountHandle, Text::new(""), font_of(&font, 13.0, DIM)));
                let action = UiAction::Account;
                corner
                    .spawn((
                        UiButton { screen: Screen::Modes, order: 10, row: 9, action },
                        Hovered::default(),
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
                    .with_child((AccountLabel, Text::new(""), font_of(&font, 13.0, GREEN), Pickable::IGNORE))
                    .observe(move |_: On<PointerClick>, mut pressed: ResMut<Pressed>| pressed.0.push((action, 1)));
            });
        });
}

fn despawn_menu(mut commands: Commands, roots: Query<Entity, With<MenuRoot>>) {
    for e in &roots {
        commands.entity(e).despawn();
    }
}

/// The screen whose buttons are live, if any.
pub(crate) fn active_screen(state: GameState, paused: bool, challenge_over: bool) -> Option<Screen> {
    match state {
        GameState::Menu => Some(Screen::Modes),
        GameState::Playing if paused => Some(Screen::Pause),
        GameState::Playing if challenge_over => Some(Screen::Score),
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn navigate(
    state: Res<State<GameState>>,
    paused: Res<Paused>,
    mode: Res<GameMode>,
    prompt: Res<RespawnPrompt>,
    keys: Res<ButtonInput<KeyCode>>,
    gamepads: Query<&Gamepad>,
    mut stick_latch: Local<bool>,
    mut focus: ResMut<Focus>,
    mut pressed: ResMut<Pressed>,
    mut panel: ResMut<Panel>,
    buttons: Query<(&UiButton, &ComputedNode)>,
    hovered: Query<(&UiButton, &Hovered), Changed<Hovered>>,
    mut moved: MessageReader<CursorMoved>,
    sign_in: Res<crate::signin::SignInPrompt>,
    net: Res<crate::net::Net>,
    board: Res<crate::leaderboard::BoardUi>,
) {
    // (only when the mouse moves: not when the layout shifts under it)
    let mouse_moved = moved.read().count() > 0;
    let screen = if sign_in.open && *state.get() == GameState::Menu {
        Some(Screen::SignIn)
    } else if board.open {
        Some(Screen::Leaderboard)
    } else {
        active_screen(*state.get(), paused.0, crate::modes::challenge_over(&mode, &prompt))
    };
    if focus.screen != screen {
        focus.screen = screen;
        focus.order = 0;
    }
    let Some(screen) = screen else { return };
    let mut live: Vec<UiButton> = buttons
        .iter()
        .filter(|(b, node)| b.screen == screen && node.size().min_element() > 0.0)
        .map(|(b, _)| *b)
        .collect();
    live.sort_by_key(|b| b.order);
    // hovering moves the focus too, so there's one highlight
    if let Some((b, _)) = hovered.iter().find(|(b, h)| mouse_moved && b.screen == screen && h.get()) {
        focus.order = b.order;
    }
    let Some(current) = live.iter().position(|b| b.order == focus.order).or((!live.is_empty()).then_some(0)) else {
        return;
    };

    let pad = |b: GamepadButton| gamepads.iter().any(|g| g.just_pressed(b));
    let key = |k: &[KeyCode]| k.iter().any(|k| keys.just_pressed(*k));
    // typing a room code: letters type (so WASD don't steer the focus)
    let typing = screen == Screen::Modes && *panel == Panel::Join;
    if typing {
        for k in keys.get_just_pressed() {
            if let Some(i) = letter(*k).and_then(|c| crate::net::protocol::CODE_ALPHABET.iter().position(|a| *a == c)) {
                pressed.0.push((UiAction::Key(i as u8), 1));
            }
        }
        if keys.just_pressed(KeyCode::Backspace) {
            pressed.0.push((UiAction::Delete, 1));
        }
    }
    let wasd = |k: KeyCode| if typing { KeyCode::ArrowUp } else { k };
    // left stick: one step per push
    let stick = gamepads.iter().map(|g| g.left_stick()).find(|s| s.length() > 0.2).unwrap_or_default();
    let mut stick_dir = IVec2::ZERO;
    if stick.length() > 0.6 {
        if !*stick_latch {
            *stick_latch = true;
            stick_dir = if stick.x.abs() > stick.y.abs() {
                IVec2::new(stick.x.signum() as i32, 0)
            } else {
                IVec2::new(0, -stick.y.signum() as i32)
            };
        }
    } else if stick.length() < 0.3 {
        *stick_latch = false;
    }
    let up = key(&[KeyCode::ArrowUp, wasd(KeyCode::KeyW)]) || pad(GamepadButton::DPadUp) || stick_dir.y < 0;
    let down = key(&[KeyCode::ArrowDown, wasd(KeyCode::KeyS)]) || pad(GamepadButton::DPadDown) || stick_dir.y > 0;
    let left = key(&[KeyCode::ArrowLeft, wasd(KeyCode::KeyA)]) || pad(GamepadButton::DPadLeft) || stick_dir.x < 0;
    let right = key(&[KeyCode::ArrowRight, wasd(KeyCode::KeyD)]) || pad(GamepadButton::DPadRight) || stick_dir.x > 0;
    let press = key(&[KeyCode::Enter, KeyCode::NumpadEnter]) || (!typing && key(&[KeyCode::Space])) || pad(GamepadButton::South);
    let back = matches!(screen, Screen::Modes | Screen::SignIn) && (key(&[KeyCode::Escape]) || pad(GamepadButton::East));

    let b = live[current];
    let is_picker = matches!(b.action, UiAction::Pick(_));
    let mut target = current;
    if up || down {
        // the first button of the previous / next row
        let row = if up { b.row.checked_sub(1) } else { Some(b.row + 1) };
        if let Some(i) = row.and_then(|r| {
            let mut rows: Vec<u8> = live.iter().map(|x| x.row).collect();
            rows.sort();
            rows.dedup();
            // skip rows that aren't there
            let r = if up { rows.iter().rev().find(|x| **x <= r) } else { rows.iter().find(|x| **x >= r) }?;
            live.iter().position(|x| x.row == *r)
        }) {
            target = i;
        }
    } else if (left || right) && is_picker {
        pressed.0.push((b.action, if left { -1 } else { 1 }));
    } else if left && current > 0 && live[current - 1].row == b.row {
        target = current - 1;
    } else if right && current + 1 < live.len() && live[current + 1].row == b.row {
        target = current + 1;
    }
    focus.order = live[target].order;
    if press {
        pressed.0.push((live[target].action, 1));
    }
    if back && screen == Screen::SignIn {
        pressed.0.push((UiAction::SignInCancel, 1));
    } else if back && net.in_lobby() {
        pressed.0.push((UiAction::Leave, 1));
    } else if back {
        *panel = match *panel {
            Panel::Create | Panel::Join => Panel::Multi,
            _ => Panel::Closed,
        };
    }
}

fn letter(k: KeyCode) -> Option<u8> {
    use KeyCode::*;
    let all = [
        KeyA, KeyB, KeyC, KeyD, KeyE, KeyF, KeyG, KeyH, KeyI, KeyJ, KeyK, KeyL, KeyM, KeyN, KeyO, KeyP, KeyQ, KeyR, KeyS,
        KeyT, KeyU, KeyV, KeyW, KeyX, KeyY, KeyZ,
    ];
    all.iter().position(|x| *x == k).map(|i| b'A' + i as u8)
}

fn cycle<T: Copy + PartialEq>(all: &[T], current: Option<T>, step: i32) -> Option<T> {
    // None (random) sits before the first value
    let n = all.len() as i32 + 1;
    let i = current.and_then(|c| all.iter().position(|x| *x == c)).map_or(0, |i| i as i32 + 1);
    let j = (i + step).rem_euclid(n);
    (j > 0).then(|| all[j as usize - 1])
}

/// Steps through `all` (no random slot), wrapping.
fn step<T: Copy + PartialEq>(all: &[T], current: T, step: i32) -> T {
    let i = all.iter().position(|x| *x == current).unwrap_or(0) as i32;
    all[(i + step).rem_euclid(all.len() as i32) as usize]
}

#[allow(clippy::too_many_arguments)]
fn handle_pressed(
    mut commands: Commands,
    mut pressed: ResMut<Pressed>,
    mut focus: ResMut<Focus>,
    mut panel: ResMut<Panel>,
    mut picks: ResMut<MenuPicks>,
    mut paused: ResMut<Paused>,
    mut code: ResMut<JoinCode>,
    mut net: ResMut<crate::net::Net>,
    mut next: ResMut<NextState<GameState>>,
    time: Res<Time<Real>>,
    mut sign_in: ResMut<crate::signin::SignInPrompt>,
    handle_field: Query<&bevy::text::EditableText, With<crate::signin::HandleField>>,
    mut settings: ResMut<crate::settings::Settings>,
    mut applied: ResMut<crate::settings::Applied>,
) {
    for (action, step_by) in std::mem::take(&mut pressed.0) {
        match action {
            UiAction::Endless => {
                *panel = if *panel == Panel::Endless { Panel::Closed } else { Panel::Endless };
                if *panel == Panel::Endless {
                    focus.order = ORDER_START;
                }
            }
            UiAction::Settings => {
                *panel = if *panel == Panel::Settings { Panel::Closed } else { Panel::Settings };
                if *panel == Panel::Settings {
                    focus.order = ORDER_SETTINGS + 1;
                }
            }
            UiAction::Apply => crate::settings::apply_graphics(&settings, &mut applied),
            UiAction::Pick(Pick::Difficulty) => {
                settings.difficulty = step(&crate::difficulty::Difficulty::ALL, settings.difficulty, step_by);
                crate::settings::save_difficulty(settings.difficulty);
            }
            UiAction::Pick(Pick::Graphics) => {
                settings.graphics = step(&crate::settings::Graphics::ALL, settings.graphics, step_by);
                // natively it applies at once; the web waits for APPLY (it reloads)
                if !cfg!(target_arch = "wasm32") {
                    crate::settings::apply_graphics(&settings, &mut applied);
                }
            }
            UiAction::Multiplayer => {
                *panel = if panel.multiplayer() { Panel::Closed } else { Panel::Multi };
            }
            UiAction::QuickPlay => net.quick_play(),
            UiAction::Create => {
                *panel = if *panel == Panel::Create { Panel::Multi } else { Panel::Create };
                if *panel == Panel::Create {
                    focus.order = ORDER_START;
                }
            }
            UiAction::Join => {
                *panel = if *panel == Panel::Join { Panel::Multi } else { Panel::Join };
                code.0.clear();
                if *panel == Panel::Join {
                    focus.order = ORDER_KEYS;
                }
            }
            UiAction::SignIn => crate::signin::sign_in(&mut sign_in),
            UiAction::SignInSubmit => crate::signin::submit(crate::signin::typed(&handle_field).trim()),
            UiAction::SignInCancel => crate::signin::cancel(&mut sign_in),
            UiAction::Key(i) => {
                let alphabet = crate::net::protocol::CODE_ALPHABET;
                if code.0.len() >= crate::net::protocol::CODE_LEN {
                    code.0.clear();
                }
                code.0.push(alphabet[i as usize % alphabet.len()] as char);
                if code.0.len() == crate::net::protocol::CODE_LEN {
                    net.join(&code.0);
                }
            }
            UiAction::Delete => {
                code.0.pop();
            }
            UiAction::Challenge | UiAction::Retry => {
                begin_round(&mut commands, &mut next, GameMode::Challenge, Picks::from_settings());
            }
            UiAction::Start if *panel == Panel::Create => net.create(picks.0),
            UiAction::Start => begin_round(&mut commands, &mut next, GameMode::Endless, picks.0),
            UiAction::Pick(Pick::Location) => picks.0.biome = cycle(&Biome::ALL, picks.0.biome, step_by),
            UiAction::Pick(Pick::Weather) => picks.0.weather = cycle(&Weather::ALL, picks.0.weather, step_by),
            UiAction::Pick(Pick::Time) => picks.0.time = cycle(&TimeOfDay::ALL, picks.0.time, step_by),
            UiAction::Resume => paused.0 = false,
            UiAction::Menu => next.set(GameState::Menu),
            UiAction::Invite(i) => {
                // a room made for the invite takes the pickers if they're open
                let p = if *panel == Panel::Create { picks.0 } else { Picks::from_settings() };
                if let Some(did) = net.friends().get(i as usize).map(|f| f.did.clone()) {
                    net.invite(&did, p, time.elapsed_secs_f64());
                }
            }
            UiAction::RequestJoin(i) => {
                if let Some(did) = net.friends().get(i as usize).map(|f| f.did.clone()) {
                    net.press_join(&did, time.elapsed_secs_f64());
                }
            }
            // (a join request for our room comes first: it's what the card shows)
            UiAction::AcceptInvite if net.pending_request().is_some() => net.answer_request(true),
            UiAction::DismissInvite if net.pending_request().is_some() => net.answer_request(false),
            UiAction::AcceptInvite => {
                paused.0 = false;
                net.accept_invite();
            }
            UiAction::DismissInvite => net.dismiss_invite(),
            UiAction::StartRoom => net.start_game(),
            UiAction::Leave => net.leave(),
            // (taken by `leaderboard::handle_pressed` first)
            UiAction::Leaderboard | UiAction::BoardTab(_) | UiAction::CloseBoard => {}
            UiAction::Account => {
                if crate::identity::current_user().is_some() {
                    crate::identity::sign_out();
                } else {
                    crate::signin::sign_in(&mut sign_in);
                }
            }
        }
    }
}

fn style_buttons(
    focus: Res<Focus>,
    panel: Res<Panel>,
    mut buttons: Query<(&UiButton, &mut BackgroundColor, &mut BorderColor)>,
) {
    for (b, mut bg, mut border) in &mut buttons {
        let focused = focus.screen == Some(b.screen) && focus.order == b.order;
        let on = match b.action {
            UiAction::Endless => *panel == Panel::Endless,
            UiAction::Multiplayer => panel.multiplayer(),
            UiAction::Create => *panel == Panel::Create,
            UiAction::Join => *panel == Panel::Join,
            UiAction::Settings => *panel == Panel::Settings,
            _ => false,
        };
        let color = if focused {
            BUTTON_FOCUS
        } else if on {
            BUTTON_ON
        } else {
            BUTTON
        };
        if bg.0 != color {
            bg.0 = color;
        }
        let edge = if focused || on { GREEN } else { BORDER };
        if border.top != edge {
            *border = BorderColor::all(edge);
        }
    }
}

#[allow(clippy::type_complexity, clippy::too_many_arguments)]
fn layout_menu(
    panel: Res<Panel>,
    picks: Res<MenuPicks>,
    code: Res<JoinCode>,
    net: Res<crate::net::Net>,
    mut root: Query<&mut Node, (With<MenuRoot>, Without<EndlessPanel>)>,
    mut sections: Query<
        (
            &mut Node,
            Has<EndlessPanel>,
            Has<ModesRow>,
            Has<MultiRow>,
            Has<JoinPanel>,
            Option<&crate::friends::FriendsSection>,
            Option<&SignedIn>,
            Has<MultiButton>,
            Has<LobbyPanel>,
            (Has<EndlessOnly>, Has<SettingsButton>, Has<SettingsPanel>, Has<ApplyButton>),
        ),
        (
            Without<MenuRoot>,
            Or<(
                With<EndlessPanel>,
                With<ModesRow>,
                With<MultiRow>,
                With<JoinPanel>,
                With<SignedIn>,
                With<crate::friends::FriendsSection>,
                With<MultiButton>,
                With<LobbyPanel>,
                Or<(With<EndlessOnly>, With<SettingsButton>, With<SettingsPanel>, With<ApplyButton>)>,
            )>,
        ),
    >,
    mut values: Query<(&PickValue, &mut Text), (Without<CodeText>, Without<StatusText>)>,
    mut code_text: Query<&mut Text, (With<CodeText>, Without<StatusText>)>,
    mut status: Query<&mut Text, (With<StatusText>, Without<CodeText>)>,
    settings: Res<crate::settings::Settings>,
    applied: Res<crate::settings::Applied>,
) {
    let signed_in = crate::identity::current_user().is_some();
    let lobby = net.in_lobby();
    for (mut node, endless, modes, multi, join, friends, signed, multi_button, lobby_panel, extra) in &mut sections {
        let (endless_only, settings_button, settings_panel, apply) = extra;
        let show = if endless_only {
            *panel == Panel::Endless
        } else if apply {
            settings.graphics != applied.0
        } else if let Some(f) = friends {
            // (the pause menu's list is the friends module's)
            if f.0 != Screen::Modes {
                continue;
            }
            (panel.multiplayer() || lobby) && signed_in
        } else if lobby_panel {
            lobby
        } else if lobby {
            // the lobby replaces the rest of the menu
            false
        } else if multi_button {
            true
        } else if settings_button {
            !panel.multiplayer()
        } else if settings_panel {
            *panel == Panel::Settings
        } else if endless {
            matches!(*panel, Panel::Endless | Panel::Create)
        } else if modes {
            !panel.multiplayer()
        } else if multi {
            panel.multiplayer()
        } else if join {
            *panel == Panel::Join
        } else {
            signed.is_some_and(|s| s.0 == signed_in)
        };
        let d = if show { Display::Flex } else { Display::None };
        if node.display != d {
            node.display = d;
        }
    }
    if let Ok(mut text) = code_text.single_mut() {
        let slots: String = (0..crate::net::protocol::CODE_LEN)
            .map(|i| code.0.chars().nth(i).unwrap_or('_').to_string())
            .collect::<Vec<_>>()
            .join(" ");
        if text.0 != slots {
            text.0 = slots;
        }
    }
    if let Ok(mut text) = status.single_mut() {
        let value = if panel.multiplayer() || net.in_lobby() { net.label().unwrap_or("") } else { "" };
        if text.0 != value {
            text.0 = value.into();
        }
    }
    if let Ok(mut node) = root.single_mut() {
        // keep clear of the notch and the home indicator
        let s = crate::platform::safe_area();
        let padding = UiRect { left: px(s.x), top: px(s.y), right: px(s.z), bottom: px(s.w) };
        if node.padding != padding {
            node.padding = padding;
        }
    }
    for (value, mut text) in &mut values {
        let name = match value.0 {
            Pick::Location => picks.0.biome.map(Biome::name),
            Pick::Weather => picks.0.weather.map(Weather::name),
            Pick::Time => picks.0.time.map(TimeOfDay::name),
            Pick::Difficulty => Some(settings.difficulty.name()),
            Pick::Graphics => Some(settings.graphics.name()),
        };
        let s = format!("< {} >", name.unwrap_or("random").to_ascii_uppercase());
        if text.0 != s {
            text.0 = s;
        }
    }
}

/// Into the lobby: the multiplayer panel underneath (for when we leave), and
/// the focus on START (the host) or LEAVE.
fn lobby_focus(net: Res<crate::net::Net>, mut was: Local<bool>, mut panel: ResMut<Panel>, mut focus: ResMut<Focus>) {
    let lobby = net.in_lobby();
    if lobby && !*was {
        *panel = Panel::Multi;
        focus.order = if net.is_host() { ORDER_LOBBY } else { ORDER_LOBBY + 1 };
    }
    *was = lobby;
}

/// The lobby's code, players, bake bar and START.
#[allow(clippy::type_complexity)]
fn layout_lobby(
    net: Res<crate::net::Net>,
    pre: Res<crate::loading::Prebake>,
    mut code: Query<&mut Text, (With<LobbyCode>, Without<LobbyPlayers>)>,
    mut players: Query<&mut Text, (With<LobbyPlayers>, Without<LobbyCode>)>,
    mut bar: Query<&mut Node, (With<LobbyBar>, Without<LobbyStart>)>,
    mut start: Query<&mut Node, (With<LobbyStart>, Without<LobbyBar>)>,
) {
    if !net.in_lobby() {
        return;
    }
    if let Ok(mut t) = code.single_mut() {
        let c = net.room_code().unwrap_or("");
        if t.0 != c {
            t.0 = c.into();
        }
    }
    if let Ok(mut t) = players.single_mut() {
        let rows: Vec<String> = net
            .players()
            .iter()
            .map(|(h, host, me)| format!("{}@{h:<24}{}", if *me { "> " } else { "  " }, if *host { "HOST" } else { "    " }))
            .collect();
        let s = rows.join("\n");
        if t.0 != s {
            t.0 = s;
        }
    }
    if let Ok(mut node) = bar.single_mut() {
        let w = percent(pre.progress() * 100.0);
        if node.width != w {
            node.width = w;
        }
    }
    if let Ok(mut node) = start.single_mut() {
        let d = if net.is_host() { Display::Flex } else { Display::None };
        if node.display != d {
            node.display = d;
        }
    }
}

/// Keeps the account corner in step with the page's sign-in state.
fn layout_account(
    mut corner: Query<&mut Node, With<AccountCorner>>,
    mut handle: Query<(&mut Text, &mut Node), (With<AccountHandle>, Without<AccountCorner>, Without<UiButton>)>,
    mut label: Query<&mut Text, (With<AccountLabel>, Without<AccountHandle>)>,
    mut buttons: Query<(&UiButton, &mut Node), (Without<AccountCorner>, Without<AccountHandle>)>,
) {
    let Ok(mut corner) = corner.single_mut() else { return };
    let user = crate::identity::current_user();
    // a dev user's handle shows, without a button
    let can = crate::identity::can_sign_in();
    let d = if user.is_some() || can { Display::Flex } else { Display::None };
    if corner.display != d {
        corner.display = d;
    }
    let s = crate::platform::safe_area();
    let (top, right) = (px(s.y + 12.0), px(s.z + 12.0));
    if corner.top != top || corner.right != right {
        corner.top = top;
        corner.right = right;
    }
    if let Ok((mut text, mut node)) = handle.single_mut() {
        let h = user.as_ref().map(|u| format!("@{}", u.handle)).unwrap_or_default();
        if text.0 != h {
            text.0 = h;
        }
        let d = if user.is_some() { Display::Flex } else { Display::None };
        if node.display != d {
            node.display = d;
        }
    }
    if let Ok(mut text) = label.single_mut() {
        let l = if user.is_some() { "SIGN OUT" } else { "SIGN IN" };
        if text.0 != l {
            text.0 = l.into();
        }
    }
    for (b, mut node) in &mut buttons {
        if b.action == UiAction::Account {
            let d = if can { Display::Flex } else { Display::None };
            if node.display != d {
                node.display = d;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::cycle;

    #[test]
    fn pickers_cycle_through_random() {
        let all = [1, 2, 3];
        assert_eq!(cycle(&all, None, 1), Some(1));
        assert_eq!(cycle(&all, Some(3), 1), None);
        assert_eq!(cycle(&all, None, -1), Some(3));
        assert_eq!(cycle(&all, Some(1), -1), None);
    }
}
