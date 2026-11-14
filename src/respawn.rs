//! After the player is shot down (or crashes), a RESPAWN button appears
//! after a few seconds. Click it, or press Enter / gamepad A, to fly again.
//! (Endless only: a challenge shows its score screen instead, see `modes`.)
//! In multiplayer it also names who got us, with our head-to-head record
//! against them this room (ours – theirs).

use bevy::{picking::hover::Hovered, prelude::*};

use crate::{
    ai::Pilot,
    controls::MouseAim,
    flight::Disabled,
    game::{Models, Player, Team, spawn_aircraft},
    pause::Paused,
    world::Terrain,
};

pub struct RespawnPlugin;

impl Plugin for RespawnPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<RespawnPrompt>()
            .init_resource::<ButtonClicked>()
            .add_systems(Startup, spawn_prompt)
            .add_systems(
                Update,
                (track_player, update_prompt, respawn_on_request)
                    .chain()
                    .run_if(in_state(crate::loading::GameState::Playing)),
            );
    }
}

/// Seconds after going down before the button appears.
const RESPAWN_DELAY: f32 = 3.0;

#[derive(Resource, Default)]
pub struct RespawnPrompt {
    /// How long the player has been shot down / dead (0 while flying).
    down_for: f32,
    shot_down: bool,
}

impl RespawnPrompt {
    /// The button is up (the cursor is freed so it can be clicked).
    pub fn ready(&self) -> bool {
        self.down_for >= RESPAWN_DELAY
    }
}

/// Set by the button's click observer, consumed by `respawn_on_request`.
#[derive(Resource, Default)]
struct ButtonClicked(bool);

#[derive(Component)]
struct PromptRoot;

#[derive(Component)]
struct PromptTitle;

#[derive(Component)]
struct RespawnButton;

/// Multiplayer: `@killer`, then the record.
#[derive(Component)]
enum KillerLine {
    Name,
    Record,
}

const GREEN: Color = Color::srgb(0.4, 1.0, 0.55);
const BUTTON: Color = Color::srgba(0.05, 0.18, 0.09, 0.9);
const BUTTON_HOVER: Color = Color::srgba(0.1, 0.32, 0.16, 0.95);

fn spawn_prompt(mut commands: Commands, asset_server: Res<AssetServer>) {
    let font: Handle<Font> = asset_server.load("fonts/RobotoMono/RobotoMono-Medium.ttf");
    let text = |size: f32, color: Color| {
        (
            TextFont {
                font: font.clone().into(),
                font_size: FontSize::Px(size),
                ..default()
            },
            TextColor(color),
        )
    };
    commands
        .spawn((
            PromptRoot,
            crate::intro::KeepDuringIntro,
            Node {
                position_type: PositionType::Absolute,
                width: percent(100),
                top: percent(58),
                flex_direction: FlexDirection::Column,
                align_items: AlignItems::Center,
                row_gap: px(14),
                display: Display::None,
                ..default()
            },
            GlobalZIndex(5),
        ))
        .with_children(|root| {
            root.spawn((Text::new("SHOT DOWN"), text(34.0, Color::srgb(1.0, 0.35, 0.3)), PromptTitle));
            root.spawn((KillerLine::Name, Text::new(""), text(22.0, Color::srgb(1.0, 0.55, 0.5)), Node { display: Display::None, ..default() }));
            root.spawn((KillerLine::Record, Text::new(""), text(26.0, GREEN), Node { display: Display::None, ..default() }));
            root.spawn((
                RespawnButton,
                Hovered::default(),
                Node {
                    padding: UiRect::axes(px(36), px(12)),
                    border: UiRect::all(px(2)),
                    border_radius: BorderRadius::all(px(6)),
                    ..default()
                },
                BorderColor::all(GREEN),
                BackgroundColor(BUTTON),
            ))
            .with_child((Text::new("RESPAWN"), text(26.0, GREEN)))
            .observe(|_: On<PointerClick>, mut clicked: ResMut<ButtonClicked>| clicked.0 = true);
            root.spawn((
                crate::pause::DesktopOnly,
                Node::default(),
                Text::new("click, Enter or (A)"),
                text(15.0, Color::srgb(0.65, 0.8, 0.7)),
            ));
        });
}

fn track_player(
    time: Res<Time>,
    mut prompt: ResMut<RespawnPrompt>,
    player: Query<Has<Disabled>, With<Player>>,
) {
    match player.single() {
        Ok(false) => {
            prompt.down_for = 0.0;
            prompt.shot_down = false;
        }
        Ok(true) => {
            prompt.shot_down = true;
            prompt.down_for += time.delta_secs();
        }
        // no player at all: crashed (or the wreck hit the ground)
        Err(_) => prompt.down_for += time.delta_secs(),
    }
}

fn update_prompt(
    prompt: Res<RespawnPrompt>,
    paused: Res<Paused>,
    mode: Res<crate::modes::GameMode>,
    mut root: Query<&mut Node, With<PromptRoot>>,
    mut title: Query<&mut Text, (With<PromptTitle>, Without<KillerLine>)>,
    mut button: Query<(&Hovered, &mut BackgroundColor), With<RespawnButton>>,
    mut lines: Query<(&KillerLine, &mut Text, &mut Node), (Without<PromptRoot>, Without<PromptTitle>)>,
    net: Res<crate::net::Net>,
) {
    let death = net.death().filter(|_| *mode == crate::modes::GameMode::Multiplayer);
    for (line, mut text, mut node) in &mut lines {
        let s = match (line, death) {
            (_, None) => String::new(),
            (KillerLine::Name, Some(d)) => format!("@{}", d.killer),
            (KillerLine::Record, Some(d)) => format!("{} \u{2013} {}", d.record.0, d.record.1),
        };
        let display = if s.is_empty() { Display::None } else { Display::Flex };
        if node.display != display {
            node.display = display;
        }
        if text.0 != s {
            text.0 = s;
        }
    }
    // (the tutorial puts you back in the air itself)
    let show = prompt.ready()
        && !paused.0
        && !matches!(*mode, crate::modes::GameMode::Challenge | crate::modes::GameMode::Tutorial);
    if let Ok(mut node) = root.single_mut() {
        let display = if show { Display::Flex } else { Display::None };
        if node.display != display {
            node.display = display;
        }
    }
    if let Ok(mut text) = title.single_mut() {
        let value = if prompt.shot_down { "SHOT DOWN" } else { "CRASHED" };
        if text.0 != value {
            text.0 = value.into();
        }
    }
    if let Ok((hovered, mut bg)) = button.single_mut() {
        bg.0 = if hovered.get() { BUTTON_HOVER } else { BUTTON };
    }
}

#[allow(clippy::too_many_arguments)]
fn respawn_on_request(
    mut commands: Commands,
    mut prompt: ResMut<RespawnPrompt>,
    paused: Res<Paused>,
    mode: Res<crate::modes::GameMode>,
    keys: Res<ButtonInput<KeyCode>>,
    gamepads: Query<&Gamepad>,
    mut clicked: ResMut<ButtonClicked>,
    models: Res<Models>,
    terrain: Res<Terrain>,
    mut aim: ResMut<MouseAim>,
    wrecks: Query<(Entity, &Transform), With<Player>>,
    friendlies: Query<(&Transform, &Team), (With<Pilot>, Without<Player>)>,
) {
    let clicked = std::mem::take(&mut clicked.0);
    if !prompt.ready() || paused.0 || matches!(*mode, crate::modes::GameMode::Challenge | crate::modes::GameMode::Tutorial) {
        return;
    }
    let pressed = keys.just_pressed(KeyCode::Enter)
        || keys.just_pressed(KeyCode::NumpadEnter)
        || gamepads.iter().any(|g| g.just_pressed(GamepadButton::South));
    if !(clicked || pressed) {
        return;
    }

    // rejoin next to a friendly if there is one, otherwise near where we fell
    let wreck = wrecks.iter().next().map(|(_, t)| *t);
    let anchor = friendlies
        .iter()
        .find(|(_, team)| **team == Team::Blue)
        .map(|(t, _)| *t)
        .or(wreck)
        .map(|t| (t.translation, t.forward().as_vec3()))
        .unwrap_or((Vec3::new(0.0, 1800.0, 6000.0), Vec3::NEG_Z));
    for (entity, _) in &wrecks {
        commands.entity(entity).despawn();
    }
    if *mode == crate::modes::GameMode::Multiplayer {
        let (pos, heading) = crate::net::spawn_point();
        spawn_aircraft(&mut commands, &models, &terrain, Team::Blue, pos, heading, true);
        aim.reset = true;
        *prompt = RespawnPrompt::default();
        return;
    }
    let heading = Vec3::new(anchor.1.x, 0.0, anchor.1.z).normalize_or(Vec3::NEG_Z);
    let pos = Vec3::new(anchor.0.x, anchor.0.y.max(1800.0), anchor.0.z) + heading.cross(Vec3::Y) * 400.0;
    spawn_aircraft(&mut commands, &models, &terrain, Team::Blue, pos, heading, true);
    aim.reset = true;
    *prompt = RespawnPrompt::default();
}
