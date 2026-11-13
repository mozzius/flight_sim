//! The mode menu (before every round), plus the buttons shared by the menus:
//! the mode menu, the pause menu and the challenge score screen.
//!
//! Buttons work with the mouse and touch (click / tap), and with the
//! keyboard or a gamepad: arrows / D-pad / left stick move the focus,
//! Enter / Space / (A) press, left and right cycle a picker, Esc / (B) closes
//! the endless pickers.

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
        app.init_resource::<Pressed>()
            .init_resource::<Focus>()
            .insert_resource(MenuPicks(Picks::from_settings()))
            .insert_resource(EndlessOpen(open))
            .add_systems(OnEnter(GameState::Menu), spawn_menu)
            .add_systems(OnExit(GameState::Menu), despawn_menu)
            .add_systems(Update, (navigate, handle_pressed, style_buttons, layout_menu).chain());
    }
}

/// Which set of buttons takes the keyboard / gamepad.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Screen {
    Modes,
    Pause,
    Score,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pick {
    Location,
    Weather,
    Time,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UiAction {
    Endless,
    Challenge,
    Pick(Pick),
    Start,
    Resume,
    Menu,
    Retry,
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
struct Pressed(Vec<(UiAction, i32)>);

/// The focused button (keyboard / gamepad / last hovered).
#[derive(Resource, Default)]
struct Focus {
    screen: Option<Screen>,
    order: u8,
}

/// The endless pickers.
#[derive(Resource)]
pub struct MenuPicks(pub Picks);

/// The endless pickers are open.
#[derive(Resource)]
struct EndlessOpen(bool);

const BG: Color = Color::srgb(0.043, 0.059, 0.078);
const GREEN: Color = Color::srgb(0.4, 1.0, 0.55);
const DIM: Color = Color::srgb(0.65, 0.8, 0.7);
const BUTTON: Color = Color::srgba(0.05, 0.18, 0.09, 0.9);
const BUTTON_FOCUS: Color = Color::srgba(0.1, 0.32, 0.16, 0.95);
const BUTTON_ON: Color = Color::srgba(0.16, 0.45, 0.24, 0.95);
const BORDER: Color = Color::srgba(0.4, 1.0, 0.55, 0.45);

fn font_of(font: &Handle<Font>, size: f32, color: Color) -> (TextFont, TextColor) {
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

#[derive(Component)]
struct PickValue(Pick);

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
            root.spawn(Node { column_gap: px(14), ..default() }).with_children(|row| {
                button(row, &font, "ENDLESS", Screen::Modes, 0, 0, UiAction::Endless);
                button(row, &font, "CHALLENGE", Screen::Modes, 1, 0, UiAction::Challenge);
            });
            root.spawn((
                EndlessPanel,
                Node { flex_direction: FlexDirection::Column, align_items: AlignItems::Center, row_gap: px(8), ..default() },
            ))
            .with_children(|panel| {
                for (i, (pick, label)) in
                    [(Pick::Location, "LOCATION"), (Pick::Weather, "WEATHER"), (Pick::Time, "TIME")].into_iter().enumerate()
                {
                    let action = UiAction::Pick(pick);
                    panel
                        .spawn((
                            UiButton { screen: Screen::Modes, order: 2 + i as u8, row: 1 + i as u8, action },
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
                            b.spawn((Text::new(label), font_of(&font, 14.0, DIM), Pickable::IGNORE));
                            b.spawn((PickValue(pick), Text::new(""), font_of(&font, 17.0, GREEN), Pickable::IGNORE));
                        })
                        .observe(move |_: On<PointerClick>, mut pressed: ResMut<Pressed>| pressed.0.push((action, 1)));
                }
                panel.spawn(Node { height: px(4), ..default() });
                button(panel, &font, "START", Screen::Modes, 5, 4, UiAction::Start);
            });
        });
}

fn despawn_menu(mut commands: Commands, roots: Query<Entity, With<MenuRoot>>) {
    for e in &roots {
        commands.entity(e).despawn();
    }
}

/// The screen whose buttons are live, if any.
fn active_screen(state: GameState, paused: bool, challenge_over: bool) -> Option<Screen> {
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
    mut open: ResMut<EndlessOpen>,
    buttons: Query<(&UiButton, &ComputedNode)>,
    hovered: Query<(&UiButton, &Hovered), Changed<Hovered>>,
    mut moved: MessageReader<CursorMoved>,
) {
    // (only when the mouse moves: not when the layout shifts under it)
    let mouse_moved = moved.read().count() > 0;
    let screen = active_screen(*state.get(), paused.0, crate::modes::challenge_over(&mode, &prompt));
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
    let up = key(&[KeyCode::ArrowUp, KeyCode::KeyW]) || pad(GamepadButton::DPadUp) || stick_dir.y < 0;
    let down = key(&[KeyCode::ArrowDown, KeyCode::KeyS]) || pad(GamepadButton::DPadDown) || stick_dir.y > 0;
    let left = key(&[KeyCode::ArrowLeft, KeyCode::KeyA]) || pad(GamepadButton::DPadLeft) || stick_dir.x < 0;
    let right = key(&[KeyCode::ArrowRight, KeyCode::KeyD]) || pad(GamepadButton::DPadRight) || stick_dir.x > 0;
    let press = key(&[KeyCode::Enter, KeyCode::NumpadEnter, KeyCode::Space]) || pad(GamepadButton::South);
    let back = (screen == Screen::Modes) && (key(&[KeyCode::Escape]) || pad(GamepadButton::East));

    let b = live[current];
    let is_picker = matches!(b.action, UiAction::Pick(_));
    let mut target = current;
    if up || down {
        // the first button of the previous / next row
        let row = if up { b.row.checked_sub(1) } else { Some(b.row + 1) };
        if let Some(i) = row.and_then(|r| {
            let mut rows: Vec<u8> = live.iter().map(|x| x.row).collect();
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
    if back {
        open.0 = false;
    }
}

fn cycle<T: Copy + PartialEq>(all: &[T], current: Option<T>, step: i32) -> Option<T> {
    // None (random) sits before the first value
    let n = all.len() as i32 + 1;
    let i = current.and_then(|c| all.iter().position(|x| *x == c)).map_or(0, |i| i as i32 + 1);
    let j = (i + step).rem_euclid(n);
    (j > 0).then(|| all[j as usize - 1])
}

#[allow(clippy::too_many_arguments)]
fn handle_pressed(
    mut commands: Commands,
    mut pressed: ResMut<Pressed>,
    mut focus: ResMut<Focus>,
    mut open: ResMut<EndlessOpen>,
    mut picks: ResMut<MenuPicks>,
    mut paused: ResMut<Paused>,
    mut next: ResMut<NextState<GameState>>,
) {
    for (action, step) in std::mem::take(&mut pressed.0) {
        match action {
            UiAction::Endless => {
                open.0 = !open.0;
                if open.0 {
                    focus.order = 5;
                }
            }
            UiAction::Challenge | UiAction::Retry => {
                begin_round(&mut commands, &mut next, GameMode::Challenge, Picks::from_settings());
            }
            UiAction::Start => begin_round(&mut commands, &mut next, GameMode::Endless, picks.0),
            UiAction::Pick(Pick::Location) => picks.0.biome = cycle(&Biome::ALL, picks.0.biome, step),
            UiAction::Pick(Pick::Weather) => picks.0.weather = cycle(&Weather::ALL, picks.0.weather, step),
            UiAction::Pick(Pick::Time) => picks.0.time = cycle(&TimeOfDay::ALL, picks.0.time, step),
            UiAction::Resume => paused.0 = false,
            UiAction::Menu => next.set(GameState::Menu),
        }
    }
}

fn style_buttons(
    focus: Res<Focus>,
    open: Res<EndlessOpen>,
    mut buttons: Query<(&UiButton, &mut BackgroundColor, &mut BorderColor)>,
) {
    for (b, mut bg, mut border) in &mut buttons {
        let focused = focus.screen == Some(b.screen) && focus.order == b.order;
        let on = b.action == UiAction::Endless && open.0;
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

fn layout_menu(
    open: Res<EndlessOpen>,
    picks: Res<MenuPicks>,
    mut root: Query<&mut Node, (With<MenuRoot>, Without<EndlessPanel>)>,
    mut panel: Query<&mut Node, (With<EndlessPanel>, Without<MenuRoot>)>,
    mut values: Query<(&PickValue, &mut Text)>,
) {
    if let Ok(mut node) = root.single_mut() {
        // keep clear of the notch and the home indicator
        let s = crate::platform::safe_area();
        let padding = UiRect { left: px(s.x), top: px(s.y), right: px(s.z), bottom: px(s.w) };
        if node.padding != padding {
            node.padding = padding;
        }
    }
    if let Ok(mut node) = panel.single_mut() {
        let d = if open.0 { Display::Flex } else { Display::None };
        if node.display != d {
            node.display = d;
        }
    }
    for (value, mut text) in &mut values {
        let name = match value.0 {
            Pick::Location => picks.0.biome.map(Biome::name),
            Pick::Weather => picks.0.weather.map(Weather::name),
            Pick::Time => picks.0.time.map(TimeOfDay::name),
        };
        let s = format!("< {} >", name.unwrap_or("random").to_ascii_uppercase());
        if text.0 != s {
            text.0 = s;
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
