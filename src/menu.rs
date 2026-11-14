//! The mode menu (before every round), plus the buttons shared by the menus:
//! the mode menu, the pause menu and the challenge score screen.
//!
//! The mode menu is a title (left in landscape, on top in portrait) and one
//! column of buttons: ENDLESS / CHALLENGE / MULTIPLAYER, then the quieter
//! TUTORIAL / LEADERBOARD / SETTINGS. Each opens a page in the column (with
//! BACK); the account corner is `@handle` or SIGN IN. Behind it the camera
//! turns slowly under the sky ([`menu_camera`]) under a dark gradient
//! ([`backdrop`]). The focus is the one highlight: a bright border and `>`.
//!
//! Buttons work with the mouse and touch (click / tap), and with the
//! keyboard or a gamepad: arrows / D-pad / left stick move the focus,
//! Enter / Space / (A) press, left and right cycle a picker, Esc / (B) closes
//! the endless pickers.
//!
//! In a multiplayer room that hasn't started, the menu is the lobby: the
//! room code, who's in (and who hosts), the friends list, a thin bar for the
//! map baking meanwhile, START (the host) and LEAVE (Esc / (B) too).
//!
//! Waiting in the quick-play queue, the menu is SEARCHING (animated dots),
//! the online count, the friends list and CANCEL (Esc / (B) too), plus
//! PLAY SOLO (an endless round) after [`SOLO_AFTER`] seconds. The
//! MULTIPLAYER panel shows the online count too.

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
                (lobby_focus, navigate, crate::leaderboard::handle_pressed, crate::tutorial::handle_pressed, handle_pressed, style_buttons, layout_menu, layout_lobby, layout_queue, layout_account)
                    .chain()
                    .in_set(MenuSystems),
            )
            .add_systems(Update, crate::friends::invite_shortcut.before(handle_pressed))
            .add_systems(PostUpdate, menu_camera.run_if(in_state(GameState::Menu)).before(TransformSystems::Propagate));
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
    /// The quick-play queue's CANCEL and PLAY SOLO.
    CancelQueue,
    PlaySolo,
    /// The native sign-in prompt's SIGN IN and CANCEL.
    SignInSubmit,
    SignInCancel,
    /// The leaderboard's buttons (handled in `leaderboard`).
    Leaderboard,
    BoardTab(crate::net::protocol::BoardKind),
    CloseBoard,
    /// TUTORIAL (the main menu), SKIP (its pause menu) and ENDLESS (its
    /// last screen), handled in `tutorial`.
    Tutorial,
    SkipStep,
    /// A sub-page's BACK (Esc / (B) too).
    Back,
    TutorialEndless,
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

// Focus orders and rows on the mode menu (up / down go by row). Home is
// orders / rows 0-5, the multiplayer page 6-8; each page's BACK comes last,
// then the account corner, then the friends list.
const ORDER_PICKERS: u8 = 12;
const ROW_PICKERS: u8 = 12;
const ORDER_START: u8 = 16;
const ROW_START: u8 = 16;
const ORDER_BACK: u8 = 89;
const ROW_BACK: u8 = 28;
const ORDER_CORNER: u8 = 90;
const ROW_CORNER: u8 = 29;
const ORDER_SETTINGS: u8 = 50;
const ROW_SETTINGS: u8 = 16;
const ORDER_KEYS: u8 = 20;
const ROW_KEYS: u8 = 11;
const KEYS_PER_ROW: usize = 8;
const ORDER_LOBBY: u8 = 60;
const ROW_LOBBY: u8 = 25;
const ORDER_QUEUE: u8 = 70;
const ROW_QUEUE: u8 = 26;
/// PLAY SOLO shows after this long alone in the queue.
pub const SOLO_AFTER: f64 = 30.0;
const ORDER_FRIENDS: u8 = 100;
const ROW_FRIENDS: u8 = 30;

pub(crate) const BG: Color = Color::srgb(0.043, 0.059, 0.078);
pub(crate) const GREEN: Color = Color::srgb(0.4, 1.0, 0.55);
pub(crate) const DIM: Color = Color::srgb(0.65, 0.8, 0.7);
pub(crate) const BUTTON: Color = Color::srgba(0.04, 0.13, 0.07, 0.82);
const BUTTON_FOCUS: Color = Color::srgba(0.09, 0.3, 0.15, 0.92);
/// The quiet (secondary) buttons: outline only.
const QUIET: Color = Color::srgba(0.02, 0.05, 0.04, 0.35);
const QUIET_FOCUS: Color = Color::srgba(0.07, 0.22, 0.12, 0.8);
pub(crate) const BORDER: Color = Color::srgba(0.4, 1.0, 0.55, 0.4);
const QUIET_BORDER: Color = Color::srgba(0.4, 1.0, 0.55, 0.2);
/// The menu column's width (logical px).
const COLUMN_WIDTH: f32 = 300.0;

pub(crate) fn font_of(font: &Handle<Font>, size: f32, color: Color) -> (TextFont, TextColor) {
    (TextFont { font: font.clone().into(), font_size: FontSize::Px(size), ..default() }, TextColor(color))
}

/// A secondary button (outline only, smaller).
#[derive(Component)]
struct Quiet;

/// The `>` at a focused button's left.
#[derive(Component)]
struct FocusMarker;

/// A button's label (quiet ones brighten when focused).
#[derive(Component)]
pub(crate) struct ButtonLabel;

/// A quiet button whose label stays full brightness (the home page's
/// TUTORIAL / LEADERBOARD / SETTINGS: outlined and smaller, but not greyed out).
#[derive(Component)]
struct BrightLabel;

/// TUTORIAL's dot, until it's been finished once.
#[derive(Component)]
struct TutorialDot;

fn spawn_button(
    parent: &mut ChildSpawnerCommands,
    font: &Handle<Font>,
    label: &str,
    b: UiButton,
    quiet: bool,
) -> Entity {
    let action = b.action;
    let mut e = parent.spawn((
        b,
        Hovered::default(),
        Node {
            min_width: px(180),
            min_height: px(if quiet { 40 } else { 48 }),
            padding: UiRect::axes(px(32), px(if quiet { 6 } else { 10 })),
            border: UiRect::all(px(1)),
            border_radius: BorderRadius::all(px(4)),
            justify_content: JustifyContent::Center,
            align_items: AlignItems::Center,
            ..default()
        },
        BorderColor::all(if quiet { QUIET_BORDER } else { BORDER }),
        BackgroundColor(if quiet { QUIET } else { BUTTON }),
    ));
    if quiet {
        e.insert(Quiet);
    }
    e.with_children(|b| {
        b.spawn((
            FocusMarker,
            Text::new(">"),
            font_of(font, if quiet { 15.0 } else { 18.0 }, GREEN),
            Node { position_type: PositionType::Absolute, left: px(14), ..default() },
            Visibility::Hidden,
            Pickable::IGNORE,
        ));
        b.spawn((
            ButtonLabel,
            Text::new(label),
            font_of(font, if quiet { 15.0 } else { 19.0 }, if quiet { DIM } else { GREEN }),
            bevy::text::LetterSpacing::Px(if quiet { 1.5 } else { 2.0 }),
            Pickable::IGNORE,
        ));
    })
    .observe(move |_: On<PointerClick>, mut pressed: ResMut<Pressed>| pressed.0.push((action, 1)))
    .id()
}

/// Spawns a text button (at least 48 px tall, for fingers).
pub fn button(parent: &mut ChildSpawnerCommands, font: &Handle<Font>, label: &str, screen: Screen, order: u8, row: u8, action: UiAction) -> Entity {
    spawn_button(parent, font, label, UiButton { screen, order, row, action }, false)
}

/// A secondary button: outline only, a little smaller.
pub(crate) fn quiet_button(parent: &mut ChildSpawnerCommands, font: &Handle<Font>, label: &str, screen: Screen, order: u8, row: u8, action: UiAction) -> Entity {
    spawn_button(parent, font, label, UiButton { screen, order, row, action }, true)
}

#[derive(Component)]
struct MenuRoot;


/// The account corner (top right): the signed-in handle and SIGN OUT, or SIGN IN.
#[derive(Component)]
struct AccountCorner;

#[derive(Component)]
struct AccountHandle;

#[derive(Component)]
struct AccountLabel;

#[derive(Component)]
struct PickValue(Pick);





/// A picker row: label on the left, `< VALUE >` on the right.
fn picker(parent: &mut ChildSpawnerCommands, font: &Handle<Font>, label: &str, pick: Pick, order: u8, row: u8) -> Entity {
    let action = UiAction::Pick(pick);
    parent
        .spawn((
            UiButton { screen: Screen::Modes, order, row, action },
            Hovered::default(),
            Node {
                min_height: px(42),
                padding: UiRect::axes(px(16), px(6)),
                border: UiRect::all(px(1)),
                border_radius: BorderRadius::all(px(4)),
                justify_content: JustifyContent::SpaceBetween,
                align_items: AlignItems::Center,
                ..default()
            },
            BorderColor::all(BORDER),
            BackgroundColor(BUTTON),
        ))
        .with_children(|b| {
            b.spawn((Text::new(label), font_of(font, 13.0, DIM), bevy::text::LetterSpacing::Px(1.5), Pickable::IGNORE));
            b.spawn((PickValue(pick), Text::new(""), font_of(font, 16.0, GREEN), Pickable::IGNORE));
        })
        .observe(move |_: On<PointerClick>, mut pressed: ResMut<Pressed>| pressed.0.push((action, 1)))
        .id()
}






#[derive(Component)]
struct CodeText;

#[derive(Component)]
struct StatusText;


#[derive(Component)]
struct LobbyCode;

#[derive(Component)]
struct LobbyPlayers;

#[derive(Component)]
struct LobbyBar;

/// Wraps START (only the host sees it).
#[derive(Component)]
struct LobbyStart;


/// SEARCHING and its dots.
#[derive(Component)]
struct QueueSearching;

/// Wraps PLAY SOLO (after a while).
#[derive(Component)]
struct QueueSolo;

/// `N ONLINE` (the MULTIPLAYER panel's, or the queue's).
#[derive(Component)]
struct OnlineText {
    queue: bool,
}

/// A small square keypad key.
fn key(parent: &mut ChildSpawnerCommands, font: &Handle<Font>, label: &str, order: u8, row: u8, action: UiAction) {
    parent
        .spawn((
            UiButton { screen: Screen::Modes, order, row, action },
            Hovered::default(),
            Node {
                width: px(34),
                height: px(40),
                border: UiRect::all(px(1)),
                border_radius: BorderRadius::all(px(4)),
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

/// The dark wash over the live sky behind the menus: light at the top (the
/// sky shows), dark at the bottom (over the ground), darker at the edges.
pub(crate) fn backdrop() -> BackgroundGradient {
    BackgroundGradient(vec![
        LinearGradient::new(
            LinearGradient::TO_BOTTOM,
            vec![
                ColorStop::percent(BG.with_alpha(0.35), 0.0),
                ColorStop::percent(BG.with_alpha(0.55), 45.0),
                ColorStop::percent(BG.with_alpha(0.93), 75.0),
                ColorStop::percent(BG.with_alpha(0.97), 100.0),
            ],
        )
        .into(),
        RadialGradient::new(
            UiPosition::CENTER,
            RadialGradientShape::FarthestCorner,
            vec![ColorStop::percent(BG.with_alpha(0.0), 35.0), ColorStop::percent(BG.with_alpha(0.6), 100.0)],
        )
        .into(),
    ])
}

/// The title, its rule (and the friends list, in multiplayer): the left of
/// the menu in landscape, the top in portrait.
#[derive(Component)]
struct TitleBlock;

#[derive(Component)]
struct TitleText;

/// The buttons' column.
#[derive(Component)]
struct MainColumn;



/// The small label over a sub-page (ENDLESS, MULTIPLAYER, ...).
#[derive(Component)]
struct PageHeading;





fn spawn_menu(mut commands: Commands, asset_server: Res<AssetServer>) {
    let font: Handle<Font> = asset_server.load("fonts/RobotoMono/RobotoMono-Medium.ttf");
    let net = crate::net::auth::available();
    let column = |gap: f32| Node {
        flex_direction: FlexDirection::Column,
        align_items: AlignItems::Stretch,
        row_gap: px(gap),
        ..default()
    };
    commands
        .spawn((
            MenuRoot,
            crate::intro::KeepDuringIntro,
            Node {
                position_type: PositionType::Absolute,
                width: percent(100),
                height: percent(100),
                flex_direction: FlexDirection::Row,
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                column_gap: px(72),
                row_gap: px(24),
                ..default()
            },
            backdrop(),
            GlobalZIndex(6),
        ))
        .with_children(|root| {
            root.spawn((
                TitleBlock,
                Node { flex_direction: FlexDirection::Column, align_items: AlignItems::Center, row_gap: px(14), ..default() },
            ))
            .with_children(|block| {
                block.spawn((
                    TitleText,
                    Text::new("FLIGHT SIM"),
                    font_of(&font, 46.0, GREEN),
                    bevy::text::LetterSpacing::Px(8.0),
                    // (the letter spacing trails the last letter too)
                    Node { margin: UiRect::left(px(8)), ..default() },
                ));
                // a thin rule, brighter in the middle
                block.spawn(Node { align_items: AlignItems::Center, column_gap: px(6), ..default() }).with_children(|rule| {
                    rule.spawn((Node { width: px(72), height: px(1), ..default() }, BackgroundColor(GREEN.with_alpha(0.3))));
                    rule.spawn((Node { width: px(18), height: px(2), ..default() }, BackgroundColor(GREEN.with_alpha(0.85))));
                    rule.spawn((Node { width: px(72), height: px(1), ..default() }, BackgroundColor(GREEN.with_alpha(0.3))));
                });
                if net {
                    block.spawn(Node { height: px(8), ..default() });
                    crate::friends::spawn_list(block, &font, Screen::Modes, ORDER_FRIENDS, ROW_FRIENDS);
                }
            });
            root.spawn((MainColumn, Node { width: px(COLUMN_WIDTH), ..column(10.0) })).with_children(|main| {
                main.spawn((
                    PageHeading,
                    Text::new(""),
                    font_of(&font, 13.0, DIM),
                    bevy::text::LetterSpacing::Px(3.0),
                    Node { margin: UiRect::bottom(px(2)), align_self: AlignSelf::Center, ..default() },
                ));
                main.spawn((Sec::HomePrimary, column(10.0))).with_children(|col| {
                    button(col, &font, "ENDLESS", Screen::Modes, 0, 0, UiAction::Endless);
                    button(col, &font, "CHALLENGE", Screen::Modes, 1, 1, UiAction::Challenge);
                    if net {
                        button(col, &font, "MULTIPLAYER", Screen::Modes, 2, 2, UiAction::Multiplayer);
                    }
                });
                main.spawn((Sec::HomeSecondary, Node { margin: UiRect::top(px(8)), ..column(8.0) })).with_children(|col| {
                    let tutorial = quiet_button(col, &font, "TUTORIAL", Screen::Modes, 3, 3, UiAction::Tutorial);
                    col.commands().entity(tutorial).insert(BrightLabel).with_child((
                        TutorialDot,
                        Node {
                            position_type: PositionType::Absolute,
                            right: px(16),
                            width: px(7),
                            height: px(7),
                            border_radius: BorderRadius::MAX,
                            ..default()
                        },
                        BackgroundColor(GREEN),
                        Pickable::IGNORE,
                    ));
                    if net {
                        col.spawn((Sec::Leaderboard, column(0.0))).with_children(|w| {
                            let e = quiet_button(w, &font, "LEADERBOARD", Screen::Modes, 4, 4, UiAction::Leaderboard);
                            w.commands().entity(e).insert(BrightLabel);
                        });
                    }
                    let settings = quiet_button(col, &font, "SETTINGS", Screen::Modes, 5, 5, UiAction::Settings);
                    col.commands().entity(settings).insert(BrightLabel);
                });
                if net {
                    main.spawn((Sec::Multi, column(10.0))).with_children(|col| {
                        col.spawn((Sec::SignedIn(true), column(10.0))).with_children(|col| {
                            button(col, &font, "QUICK PLAY", Screen::Modes, 6, 6, UiAction::QuickPlay);
                            button(col, &font, "HOST GAME", Screen::Modes, 7, 7, UiAction::Create);
                            button(col, &font, "JOIN GAME", Screen::Modes, 8, 8, UiAction::Join);
                        });
                        col.spawn((Sec::SignedIn(false), column(0.0))).with_children(|col| {
                            button(col, &font, "SIGN IN", Screen::Modes, 6, 6, UiAction::SignIn);
                        });
                    });
                    main.spawn((
                        OnlineText { queue: false },
                        Text::new(""),
                        font_of(&font, 13.0, DIM),
                        Node { display: Display::None, align_self: AlignSelf::Center, ..default() },
                    ));
                }
                main.spawn((Sec::Endless, column(8.0))).with_children(|panel| {
                    for (i, (pick, label)) in [
                        (Pick::Location, "LOCATION"),
                        (Pick::Weather, "WEATHER"),
                        (Pick::Time, "TIME"),
                        (Pick::Difficulty, "DIFFICULTY"),
                    ]
                    .into_iter()
                    .enumerate()
                    {
                        let e = picker(panel, &font, label, pick, ORDER_PICKERS + i as u8, ROW_PICKERS + i as u8);
                        if pick == Pick::Difficulty {
                            panel.commands().entity(e).insert(Sec::EndlessOnly);
                        }
                    }
                });
                main.spawn((Sec::Start, Node { margin: UiRect::top(px(4)), ..column(0.0) })).with_children(|w| {
                    button(w, &font, "START", Screen::Modes, ORDER_START, ROW_START, UiAction::Start);
                });
                main.spawn((Sec::Join, Node { align_items: AlignItems::Center, ..column(4.0) })).with_children(|panel| {
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
                main.spawn((Sec::Settings, column(8.0))).with_children(|panel| {
                    picker(panel, &font, "GRAPHICS", Pick::Graphics, ORDER_SETTINGS + 1, ROW_SETTINGS + 1);
                    panel.spawn((Sec::Apply, column(0.0))).with_children(|w| {
                        button(w, &font, "APPLY", Screen::Modes, ORDER_SETTINGS + 2, ROW_SETTINGS + 2, UiAction::Apply);
                    });
                    panel.spawn((Sec::SignOut, column(0.0))).with_children(|w| {
                        quiet_button(w, &font, "SIGN OUT", Screen::Modes, ORDER_SETTINGS + 3, ROW_SETTINGS + 3, UiAction::Account);
                    });
                });
                main.spawn((
                    Sec::Lobby,
                    Node { align_items: AlignItems::Center, display: Display::None, ..column(14.0) },
                ))
                .with_children(|panel| {
                    panel.spawn((LobbyCode, Text::new(""), font_of(&font, 56.0, GREEN), bevy::text::LetterSpacing::Px(6.0)));
                    panel.spawn((LobbyPlayers, Text::new(""), font_of(&font, 15.0, GREEN)));
                    panel
                        .spawn((Node { width: px(220), height: px(2), ..default() }, BackgroundColor(GREEN.with_alpha(0.15))))
                        .with_child((LobbyBar, Node { width: percent(0), height: percent(100), ..default() }, BackgroundColor(GREEN.with_alpha(0.6))));
                    panel.spawn(Node { width: percent(100), ..column(8.0) }).with_children(|col| {
                        col.spawn((LobbyStart, column(0.0))).with_children(|w| {
                            button(w, &font, "START", Screen::Modes, ORDER_LOBBY, ROW_LOBBY, UiAction::StartRoom);
                        });
                        quiet_button(col, &font, "LEAVE", Screen::Modes, ORDER_LOBBY + 1, ROW_LOBBY + 1, UiAction::Leave);
                    });
                });
                main.spawn((
                    Sec::Queue,
                    Node { align_items: AlignItems::Center, display: Display::None, ..column(14.0) },
                ))
                .with_children(|panel| {
                    panel.spawn((QueueSearching, Text::new("SEARCHING"), font_of(&font, 26.0, GREEN)));
                    panel.spawn((OnlineText { queue: true }, Text::new(""), font_of(&font, 14.0, DIM)));
                    panel.spawn(Node { width: percent(100), ..column(8.0) }).with_children(|col| {
                        col.spawn((QueueSolo, Node { display: Display::None, ..column(0.0) })).with_children(|w| {
                            button(w, &font, "PLAY SOLO", Screen::Modes, ORDER_QUEUE, ROW_QUEUE, UiAction::PlaySolo);
                        });
                        quiet_button(col, &font, "CANCEL", Screen::Modes, ORDER_QUEUE + 1, ROW_QUEUE + 1, UiAction::CancelQueue);
                    });
                });
                main.spawn((Sec::Back, Node { margin: UiRect::top(px(4)), ..column(0.0) })).with_children(|w| {
                    quiet_button(w, &font, "BACK", Screen::Modes, ORDER_BACK, ROW_BACK, UiAction::Back);
                });
                main.spawn((
                    StatusText,
                    Text::new(""),
                    font_of(&font, 14.0, DIM),
                    Node { align_self: AlignSelf::Center, ..default() },
                ));
            });
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
                let e = quiet_button(corner, &font, "SIGN IN", Screen::Modes, ORDER_CORNER, ROW_CORNER, UiAction::Account);
                corner.commands().entity(e).insert((AccountLabel, Node {
                    min_height: px(34),
                    padding: UiRect::axes(px(14), px(6)),
                    border: UiRect::all(px(1)),
                    border_radius: BorderRadius::all(px(4)),
                    justify_content: JustifyContent::Center,
                    align_items: AlignItems::Center,
                    ..default()
                }));
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
        // (the tutorial's last screen shares the score screen's buttons)
        GameState::Playing if challenge_over || crate::tutorial::complete_screen() => Some(Screen::Score),
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
    panel: Res<Panel>,
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
    } else if back && net.queued() {
        pressed.0.push((UiAction::CancelQueue, 1));
    } else if back {
        pressed.0.push((UiAction::Back, 1));
    }
}

/// Up a page, with the focus on the button that opened the one we leave.
fn go_back(panel: &mut Panel, focus: &mut Focus) {
    let (to, order) = match *panel {
        Panel::Closed => return,
        Panel::Endless => (Panel::Closed, 0),
        Panel::Multi => (Panel::Closed, 2),
        Panel::Settings => (Panel::Closed, 5),
        Panel::Create => (Panel::Multi, 7),
        Panel::Join => (Panel::Multi, 8),
    };
    *panel = to;
    focus.order = order;
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
                *panel = Panel::Endless;
                focus.order = ORDER_START;
            }
            UiAction::Settings => {
                *panel = Panel::Settings;
                focus.order = ORDER_SETTINGS + 1;
            }
            UiAction::Back => go_back(&mut panel, &mut focus),
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
                *panel = Panel::Multi;
                focus.order = 6;
            }
            UiAction::QuickPlay => net.quick_play(),
            UiAction::Create => {
                *panel = Panel::Create;
                focus.order = ORDER_START;
            }
            UiAction::Join => {
                *panel = Panel::Join;
                code.0.clear();
                focus.order = ORDER_KEYS;
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
            UiAction::CancelQueue => net.cancel_queue(),
            UiAction::PlaySolo => {
                net.cancel_queue();
                begin_round(&mut commands, &mut next, GameMode::Endless, Picks::from_settings());
            }
            // (taken by `leaderboard::handle_pressed` first)
            UiAction::Leaderboard | UiAction::BoardTab(_) | UiAction::CloseBoard => {}
            // (taken by `tutorial::handle_pressed` first)
            UiAction::Tutorial | UiAction::SkipStep | UiAction::TutorialEndless => {}
            UiAction::Account => {
                if crate::identity::current_user().is_some() {
                    crate::identity::sign_out();
                    focus.order = ORDER_SETTINGS + 1;
                } else {
                    crate::signin::sign_in(&mut sign_in);
                }
            }
        }
    }
}

#[allow(clippy::type_complexity)]
fn style_buttons(
    focus: Res<Focus>,
    settings: Res<crate::settings::Settings>,
    device: Res<crate::controls::InputDevice>,
    mut buttons: Query<(&UiButton, Has<Quiet>, Has<BrightLabel>, &Children, &mut BackgroundColor, &mut BorderColor)>,
    mut markers: Query<&mut Visibility, With<FocusMarker>>,
    mut labels: Query<&mut TextColor, With<ButtonLabel>>,
    mut dots: Query<&mut Node, With<TutorialDot>>,
) {
    // no focus highlight (or `>`) under touch: there's nothing to steer
    let touch = *device == crate::controls::InputDevice::Touch;
    for (b, quiet, bright, children, mut bg, mut border) in &mut buttons {
        // one highlight: the focus (the open panel's button is hidden)
        let focused = !touch && focus.screen == Some(b.screen) && focus.order == b.order;
        let color = match (quiet, focused) {
            (false, false) => BUTTON,
            (false, true) => BUTTON_FOCUS,
            (true, false) => QUIET,
            (true, true) => QUIET_FOCUS,
        };
        if bg.0 != color {
            bg.0 = color;
        }
        let edge = if focused { GREEN } else if quiet { QUIET_BORDER } else { BORDER };
        if border.top != edge {
            *border = BorderColor::all(edge);
        }
        for child in children.iter() {
            if let Ok(mut v) = markers.get_mut(child) {
                let want = if focused { Visibility::Inherited } else { Visibility::Hidden };
                if *v != want {
                    *v = want;
                }
            }
            if quiet && let Ok(mut c) = labels.get_mut(child) {
                let want = if focused || bright { GREEN } else { DIM };
                if c.0 != want {
                    c.0 = want;
                }
            }
        }
    }
    for mut node in &mut dots {
        let d = if settings.tutorial_done { Display::None } else { Display::Flex };
        if node.display != d {
            node.display = d;
        }
    }
}

/// A part of the mode menu that shows on some pages only.
#[derive(Component, Clone, Copy, PartialEq, Eq, Debug)]
enum Sec {
    HomePrimary,
    HomeSecondary,
    Leaderboard,
    Multi,
    SignedIn(bool),
    Endless,
    /// DIFFICULTY (endless only: hidden when the pickers set up a room).
    EndlessOnly,
    Start,
    Join,
    Settings,
    Apply,
    SignOut,
    Lobby,
    Queue,
    Back,
}

#[allow(clippy::type_complexity, clippy::too_many_arguments)]
fn layout_menu(
    panel: Res<Panel>,
    picks: Res<MenuPicks>,
    code: Res<JoinCode>,
    net: Res<crate::net::Net>,
    board: Res<crate::leaderboard::BoardUi>,
    sign_in: Res<crate::signin::SignInPrompt>,
    mut root: Query<(&mut Node, &ComputedNode, &mut Visibility), With<MenuRoot>>,
    mut sections: Query<(&Sec, &mut Node), Without<MenuRoot>>,
    mut friends: Query<(&crate::friends::FriendsSection, &mut Node), (Without<MenuRoot>, Without<Sec>)>,
    mut title: Query<&mut TextFont, With<TitleText>>,
    mut heading: Query<&mut Text, (With<PageHeading>, Without<CodeText>, Without<StatusText>, Without<PickValue>)>,
    mut values: Query<(&PickValue, &mut Text), (Without<CodeText>, Without<StatusText>)>,
    mut code_text: Query<&mut Text, (With<CodeText>, Without<StatusText>)>,
    mut status: Query<&mut Text, (With<StatusText>, Without<CodeText>)>,
    settings: Res<crate::settings::Settings>,
    applied: Res<crate::settings::Applied>,
) {
    let signed_in = crate::identity::current_user().is_some();
    let in_lobby = net.in_lobby();
    let queued = net.queued();
    // (the lobby and the queue each replace the pages)
    let away = in_lobby || queued;
    let page = |p: Panel| !away && *panel == p;
    for (sec, mut node) in &mut sections {
        let show = match *sec {
            Sec::HomePrimary | Sec::HomeSecondary => page(Panel::Closed),
            Sec::Leaderboard => signed_in,
            Sec::Multi => page(Panel::Multi),
            Sec::SignedIn(s) => s == signed_in,
            Sec::Endless | Sec::Start => page(Panel::Endless) || page(Panel::Create),
            Sec::EndlessOnly => *panel == Panel::Endless,
            Sec::Join => page(Panel::Join),
            Sec::Settings => page(Panel::Settings),
            Sec::Apply => settings.graphics != applied.0,
            Sec::SignOut => signed_in && crate::identity::can_sign_in(),
            Sec::Lobby => in_lobby,
            Sec::Queue => queued,
            Sec::Back => !away && *panel != Panel::Closed,
        };
        let d = if show { Display::Flex } else { Display::None };
        if node.display != d {
            node.display = d;
        }
    }
    for (f, mut node) in &mut friends {
        // (the pause menu's list is the friends module's)
        if f.0 != Screen::Modes {
            continue;
        }
        let d = if (panel.multiplayer() || away) && signed_in { Display::Flex } else { Display::None };
        if node.display != d {
            node.display = d;
        }
    }
    if let Ok(mut text) = heading.single_mut() {
        let h = match *panel {
            _ if away => "",
            Panel::Closed => "",
            Panel::Endless => "ENDLESS",
            Panel::Multi => "MULTIPLAYER",
            Panel::Create => "HOST GAME",
            Panel::Join => "JOIN GAME",
            Panel::Settings => "SETTINGS",
        };
        if text.0 != h {
            text.0 = h.into();
        }
    }
    if let Ok((mut node, computed, mut vis)) = root.single_mut() {
        // the leaderboard and the sign-in prompt cover the menu
        let v = if board.open || sign_in.open { Visibility::Hidden } else { Visibility::Inherited };
        if *vis != v {
            *vis = v;
        }
        // landscape: the title on the left, the buttons on the right;
        // portrait: stacked. Short screens (landscape phones) get a smaller title.
        let size = computed.size() * computed.inverse_scale_factor();
        let landscape = size.x > size.y * 1.1;
        let dir = if landscape { FlexDirection::Row } else { FlexDirection::Column };
        if node.flex_direction != dir {
            node.flex_direction = dir;
        }
        let gap = px(if size.x < 1000.0 { 40.0 } else { 88.0 });
        if node.column_gap != gap {
            node.column_gap = gap;
        }
        if let Ok(mut font) = title.single_mut() {
            let want = FontSize::Px(if size.y < 520.0 || size.x < 600.0 { 34.0 } else { 48.0 });
            if font.font_size != want {
                font.font_size = want;
            }
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
    if let Ok((mut node, ..)) = root.single_mut() {
        // keep clear of the notch and the home indicator
        let s = crate::platform::safe_area();
        let padding = UiRect { left: px(s.x + 16.0), top: px(s.y + 12.0), right: px(s.z + 16.0), bottom: px(s.w + 12.0) };
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
fn lobby_focus(net: Res<crate::net::Net>, mut was: Local<(bool, bool)>, mut panel: ResMut<Panel>, mut focus: ResMut<Focus>) {
    let lobby = net.in_lobby();
    if lobby && !was.0 {
        *panel = Panel::Multi;
        focus.order = if net.is_host() { ORDER_LOBBY } else { ORDER_LOBBY + 1 };
    }
    // (the queue: CANCEL; PLAY SOLO shows above it later)
    let queued = net.queued();
    if queued && !was.1 {
        *panel = Panel::Multi;
        focus.order = ORDER_QUEUE + 1;
    }
    *was = (lobby, queued);
}

/// The queue's dots and PLAY SOLO, and the online counts.
#[allow(clippy::type_complexity)]
fn layout_queue(
    net: Res<crate::net::Net>,
    panel: Res<Panel>,
    time: Res<Time<Real>>,
    mut searching: Query<&mut Text, (With<QueueSearching>, Without<OnlineText>)>,
    mut online: Query<(&OnlineText, &mut Text, &mut Node), Without<QueueSearching>>,
    mut solo: Query<&mut Node, (With<QueueSolo>, Without<OnlineText>)>,
) {
    let now = time.elapsed_secs_f64();
    let waited = net.queued_for(now);
    if let (Some(w), Ok(mut t)) = (waited, searching.single_mut()) {
        // (monospace: the padding keeps it from shuffling)
        let s = format!("SEARCHING{:<3}", ".".repeat((w / 0.4) as usize % 4));
        if t.0 != s {
            t.0 = s;
        }
    }
    if let Ok(mut node) = solo.single_mut() {
        let d = if waited.is_some_and(|w| w >= SOLO_AFTER) { Display::Flex } else { Display::None };
        if node.display != d {
            node.display = d;
        }
    }
    let count = net.online_count().map(|n| format!("{n} ONLINE"));
    let signed_in = crate::identity::current_user().is_some();
    for (o, mut text, mut node) in &mut online {
        let show = count.is_some()
            && if o.queue { net.queued() } else { *panel == Panel::Multi && signed_in && !net.queued() && !net.in_lobby() };
        let d = if show { Display::Flex } else { Display::None };
        if node.display != d {
            node.display = d;
        }
        if let Some(c) = &count
            && text.0 != *c
        {
            text.0 = c.clone();
        }
    }
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

/// The account corner: `@handle` signed in (SIGN OUT is in SETTINGS), a
/// small SIGN IN signed out.
#[allow(clippy::type_complexity)]
fn layout_account(
    net: Res<crate::net::Net>,
    mut corner: Query<&mut Node, With<AccountCorner>>,
    mut handle: Query<(&mut Text, &mut Node), (With<AccountHandle>, Without<AccountCorner>, Without<AccountLabel>)>,
    mut sign_in: Query<&mut Node, (With<AccountLabel>, Without<AccountCorner>, Without<AccountHandle>)>,
) {
    let Ok(mut corner) = corner.single_mut() else { return };
    let user = crate::identity::current_user();
    let can = crate::identity::can_sign_in() && user.is_none();
    let d = if (user.is_some() || can) && !net.in_lobby() && !net.queued() { Display::Flex } else { Display::None };
    if corner.display != d {
        corner.display = d;
    }
    let s = crate::platform::safe_area();
    let (top, right) = (px(s.y + 12.0), px(s.z + 16.0));
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
    if let Ok(mut node) = sign_in.single_mut() {
        let d = if can { Display::Flex } else { Display::None };
        if node.display != d {
            node.display = d;
        }
    }
}

/// The menu camera's tilt up (rad): with the 65° vertical FOV the horizon sits
/// just under the frame's bottom edge, so there's only sky and cloud whether or
/// not a map is loaded.
const MENU_PITCH: f32 = 0.6;

/// The menu camera's eye and yaw at a time (the backdrop dogfight follows it).
pub(crate) fn menu_view(secs: f32, terrain: Option<&crate::world::Terrain>) -> (Vec3, f32) {
    let ground = terrain.map_or(0.0, |t| t.height_at(0.0, 0.0));
    (Vec3::new(0.0, (ground + 700.0).max(900.0), 0.0), secs * 0.015)
}

/// Behind the menu the camera turns slowly under the sky (a fixed spot above
/// the last map, or the bare planet before the first round).
fn menu_camera(
    time: Res<Time<Real>>,
    terrain: Option<Res<crate::world::Terrain>>,
    mut camera: Query<&mut Transform, With<crate::camera::MainCamera>>,
) {
    if std::env::var("TERRAIN_DEBUG_CAM").is_ok() {
        return;
    }
    let Ok(mut cam) = camera.single_mut() else { return };
    let (eye, yaw) = menu_view(time.elapsed_secs(), terrain.as_deref());
    *cam = Transform::from_translation(eye).with_rotation(Quat::from_euler(EulerRot::YXZ, yaw, MENU_PITCH, 0.0));
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
