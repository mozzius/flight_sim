//! Online Bluesky mutuals and invites (see `net` for presence).
//!
//! - [`FriendsList`]: a column of rows (status dot, `@handle`, a button) in
//!   the MULTIPLAYER menu, the lobby and the multiplayer pause menu, rebuilt
//!   whenever the list changes; a dash when nobody's online. The button is
//!   INVITE (to our room, made on the fly if need be), or JOIN for a friend
//!   who's in another room (straight in for a public room, a request to the
//!   host for a hosted one); none for a full room or our own.
//! - The card: bottom centre, wherever we are (menus, single player,
//!   multiplayer), `@handle` with JOIN and X for an invite, or ACCEPT and X
//!   for someone asking to join the room we host (that comes first); gone
//!   after `net::INVITE_SECS` / `JOIN_REQUEST_SECS`. Click / tap; in menus
//!   its buttons are the last row of the focus order (down, then A / Enter);
//!   in flight J or D-pad down accepts.
//!
//! Avatars aren't drawn: cdn.bsky.app sends no CORS header, so the web
//! build couldn't load them.

use bevy::{picking::hover::Hovered, prelude::*};

use crate::{
    menu::{BORDER, BUTTON, DIM, GREEN, Pressed, Screen, UiAction, UiButton, font_of},
    net::{Net, protocol::FriendStatus},
};

pub struct FriendsPlugin;

impl Plugin for FriendsPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, spawn_card).add_systems(Update, (sync_lists, sync_labels, sync_card));
    }
}

/// Rows shown at most.
const MAX_ROWS: usize = 8;
/// The card's buttons: last in every menu's focus order.
const ORDER_CARD: u8 = 250;
const ROW_CARD: u8 = 250;

/// A list of online friends whose INVITE buttons belong to `screen`.
#[derive(Component)]
pub struct FriendsList {
    screen: Screen,
    order: u8,
    row: u8,
    /// The `Net::friends_version` the rows were built from.
    built: Option<u32>,
}

/// The FRIENDS heading plus its list (what the menus show and hide).
#[derive(Component)]
pub struct FriendsSection(pub Screen);

/// A row button's label (INVITE / JOIN / SENT) for this friend (true: JOIN).
#[derive(Component)]
struct InviteLabel(String, bool);

#[derive(Component)]
struct Card;

#[derive(Component)]
struct CardHandle;

/// The card's JOIN / ACCEPT label.
#[derive(Component)]
struct CardAccept;

/// Spawns an (empty) friends list; `sync_lists` fills it.
pub fn spawn_list(parent: &mut ChildSpawnerCommands, font: &Handle<Font>, screen: Screen, order: u8, row: u8) {
    parent
        .spawn((
            FriendsSection(screen),
            Node { flex_direction: FlexDirection::Column, align_items: AlignItems::Center, row_gap: px(6), ..default() },
        ))
        .with_children(|col| {
            col.spawn((Text::new("FRIENDS"), font_of(font, 14.0, DIM)));
            col.spawn((
                FriendsList { screen, order, row, built: None },
                FontHandle(font.clone()),
                Node { flex_direction: FlexDirection::Column, align_items: AlignItems::Stretch, row_gap: px(6), ..default() },
            ));
        });
}

#[derive(Component)]
struct FontHandle(Handle<Font>);

fn small_button(parent: &mut ChildSpawnerCommands, font: &Handle<Font>, label: &str, b: UiButton) -> Entity {
    let action = b.action;
    let mut id = Entity::PLACEHOLDER;
    parent
        .spawn((
            b,
            Hovered::default(),
            Node {
                min_width: px(96),
                min_height: px(40),
                padding: UiRect::axes(px(14), px(6)),
                border: UiRect::all(px(1)),
                border_radius: BorderRadius::all(px(6)),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                ..default()
            },
            BorderColor::all(BORDER),
            BackgroundColor(BUTTON),
        ))
        .with_children(|b| {
            id = b.spawn((Text::new(label), font_of(font, 15.0, GREEN), Pickable::IGNORE)).id();
        })
        .observe(move |_: On<PointerClick>, mut pressed: ResMut<Pressed>| pressed.0.push((action, 1)));
    id
}

fn dot_color(status: FriendStatus) -> Color {
    match status {
        FriendStatus::Online => Color::srgb(0.4, 1.0, 0.55),
        FriendStatus::InRoom => Color::srgb(1.0, 0.75, 0.3),
        FriendStatus::RoomFull => Color::srgb(0.55, 0.55, 0.55),
        FriendStatus::InPublic => Color::srgb(0.35, 0.75, 1.0),
    }
}

/// Rebuilds the rows when the list changes; the pause menu's only shows in a room.
fn sync_lists(
    mut commands: Commands,
    net: Res<Net>,
    time: Res<Time<Real>>,
    mut lists: Query<(Entity, &mut FriendsList, &FontHandle, &ChildOf)>,
    mut nodes: Query<&mut Node>,
) {
    let now = time.elapsed_secs_f64();
    for (entity, mut list, font, parent) in &mut lists {
        if list.screen == Screen::Pause
            && let Ok(mut node) = nodes.get_mut(parent.parent())
        {
            let d = if net.in_room() { Display::Flex } else { Display::None };
            if node.display != d {
                node.display = d;
            }
        }
        if list.built == Some(net.friends_version()) {
            continue;
        }
        list.built = Some(net.friends_version());
        let font = &font.0;
        let (screen, order, row) = (list.screen, list.order, list.row);
        commands.entity(entity).despawn_children().with_children(|col| {
            if net.friends().is_empty() {
                col.spawn((Text::new("-"), font_of(font, 15.0, DIM), Node { align_self: AlignSelf::Center, ..default() }));
            }
            for (i, f) in net.friends().iter().take(MAX_ROWS).enumerate() {
                col.spawn(Node { column_gap: px(12), align_items: AlignItems::Center, ..default() }).with_children(|r| {
                    r.spawn((
                        Node { width: px(10), height: px(10), border_radius: BorderRadius::all(px(5)), ..default() },
                        BackgroundColor(dot_color(f.status)),
                    ));
                    r.spawn((Text::new(format!("@{}", f.handle)), font_of(font, 15.0, GREEN), Node { min_width: px(220), ..default() }));
                    let join = matches!(f.status, FriendStatus::InRoom | FriendStatus::InPublic);
                    if f.status == FriendStatus::RoomFull || net.in_my_room(&f.did) {
                        // (nothing to do: keep the column lined up)
                        r.spawn(Node { min_width: px(96), ..default() });
                        return;
                    }
                    let action = if join { UiAction::RequestJoin(i as u8) } else { UiAction::Invite(i as u8) };
                    let b = UiButton { screen, order: order + i as u8, row: row + i as u8, action };
                    let text = small_button(r, font, label(&net, &f.did, join, now), b);
                    r.commands().entity(text).insert(InviteLabel(f.did.clone(), join));
                });
            }
        });
    }
}

fn label(net: &Net, did: &str, join: bool, now: f64) -> &'static str {
    match join {
        true if net.request_sent(did, now) => "SENT",
        true => "JOIN",
        false if net.invite_sent(did, now) => "SENT",
        false => "INVITE",
    }
}

/// SENT goes back to INVITE / JOIN once the server would take another (or said no).
fn sync_labels(net: Res<Net>, time: Res<Time<Real>>, mut labels: Query<(&InviteLabel, &mut Text)>) {
    let now = time.elapsed_secs_f64();
    for (l, mut text) in &mut labels {
        let s = label(&net, &l.0, l.1, now);
        if text.0 != s {
            text.0 = s.into();
        }
    }
}

fn spawn_card(mut commands: Commands, asset_server: Res<AssetServer>) {
    let font: Handle<Font> = asset_server.load("fonts/RobotoMono/RobotoMono-Medium.ttf");
    commands
        .spawn((
            Card,
            crate::intro::KeepDuringIntro,
            Node {
                position_type: PositionType::Absolute,
                left: px(0),
                right: px(0),
                bottom: px(28),
                justify_content: JustifyContent::Center,
                display: Display::None,
                ..default()
            },
            Pickable::IGNORE,
            GlobalZIndex(20),
        ))
        .with_children(|root| {
            root.spawn((
                Node {
                    column_gap: px(12),
                    align_items: AlignItems::Center,
                    padding: UiRect::axes(px(18), px(10)),
                    border: UiRect::all(px(1)),
                    border_radius: BorderRadius::all(px(10)),
                    ..default()
                },
                BorderColor::all(GREEN),
                BackgroundColor(Color::srgba(0.02, 0.04, 0.03, 0.95)),
            ))
            .with_children(|card| {
                card.spawn((CardHandle, Text::new(""), font_of(&font, 17.0, GREEN), Node { margin: UiRect::right(px(6)), ..default() }));
                let b = |order, action| UiButton { screen: Screen::Modes, order, row: ROW_CARD, action };
                let accept = small_button(card, &font, "JOIN", b(ORDER_CARD, UiAction::AcceptInvite));
                card.commands().entity(accept).insert(CardAccept);
                small_button(card, &font, "X", b(ORDER_CARD + 1, UiAction::DismissInvite));
            });
        });
}

/// Shows the card while an invite waits, and gives its buttons to whichever menu is up.
#[allow(clippy::type_complexity, clippy::too_many_arguments)]
fn sync_card(
    net: Res<Net>,
    state: Res<State<crate::loading::GameState>>,
    paused: Res<crate::pause::Paused>,
    mode: Res<crate::modes::GameMode>,
    prompt: Res<crate::respawn::RespawnPrompt>,
    mut card: Query<&mut Node, With<Card>>,
    mut handle: Query<&mut Text, (With<CardHandle>, Without<CardAccept>)>,
    mut accept: Query<&mut Text, (With<CardAccept>, Without<CardHandle>)>,
    mut buttons: Query<&mut UiButton>,
) {
    // a request to join our room first, else an invite
    let shown = match (net.pending_request(), net.pending_invite()) {
        (Some(r), _) => Some((&r.from.handle, "ACCEPT")),
        (None, Some(i)) => Some((&i.from.handle, "JOIN")),
        (None, None) => None,
    };
    if let (Ok(mut t), Some((_, l))) = (accept.single_mut(), shown)
        && t.0 != l
    {
        t.0 = l.into();
    }
    if let Ok(mut node) = card.single_mut() {
        let d = if shown.is_some() { Display::Flex } else { Display::None };
        if node.display != d {
            node.display = d;
        }
        let bottom = px(crate::platform::safe_area().w + 28.0);
        if node.bottom != bottom {
            node.bottom = bottom;
        }
    }
    if let (Ok(mut text), Some((h, _))) = (handle.single_mut(), shown) {
        let s = format!("@{h}");
        if text.0 != s {
            text.0 = s;
        }
    }
    let screen = crate::menu::active_screen(*state.get(), paused.0, crate::modes::challenge_over(&mode, &prompt));
    for mut b in &mut buttons {
        if matches!(b.action, UiAction::AcceptInvite | UiAction::DismissInvite)
            && let Some(s) = screen
            && b.screen != s
        {
            b.screen = s;
        }
    }
}

/// In flight (no menu up), J or D-pad down accepts the card (joins the invite, or lets the asker in).
#[allow(clippy::too_many_arguments)]
pub fn invite_shortcut(
    net: Res<Net>,
    state: Res<State<crate::loading::GameState>>,
    paused: Res<crate::pause::Paused>,
    mode: Res<crate::modes::GameMode>,
    prompt: Res<crate::respawn::RespawnPrompt>,
    keys: Res<ButtonInput<KeyCode>>,
    gamepads: Query<&Gamepad>,
    mut pressed: ResMut<Pressed>,
) {
    if (net.pending_invite().is_none() && net.pending_request().is_none())
        || crate::menu::active_screen(*state.get(), paused.0, crate::modes::challenge_over(&mode, &prompt)).is_some()
    {
        return;
    }
    if keys.just_pressed(KeyCode::KeyJ) || gamepads.iter().any(|g| g.just_pressed(GamepadButton::DPadDown)) {
        pressed.0.push((UiAction::AcceptInvite, 1));
    }
}
