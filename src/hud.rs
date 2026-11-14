//! Heads-up display: flight data, weapons, warnings, and the targeting layer
//! (target boxes, lock indicator, gun lead pipper, flight path marker,
//! off-screen target arrow).

use bevy::{
    diagnostic::FrameTimeDiagnosticsPlugin,
    prelude::*,
};

use crate::{
    camera::MainCamera,
    controls::{ControlMode, MouseAim},
    flight::{Aircraft, ControlInput, Disabled, FlightState},
    game::{Health, Player, Team},
    weapons::{LOCK_RANGE, Missile, Targeting, Weapons, gun_lead_point},
    world::Terrain,
};

const HUD_GREEN: Color = Color::srgb(0.4, 1.0, 0.55);
const HOSTILE: Color = Color::srgb(1.0, 0.3, 0.25);
const FRIENDLY: Color = Color::srgb(0.35, 0.8, 1.0);
const WARNING: Color = Color::srgb(1.0, 0.75, 0.2);
/// Other players' jets (multiplayer).
const PLAYER: Color = Color::srgb(1.0, 0.45, 0.9);
/// Missiles coming for *you* get their own colour, distinct from hostile jets.
const INCOMING: Color = Color::srgb(1.0, 0.3, 0.95);

/// Break 1-3 s before impact: the missile's airframe can't follow a sudden
/// hard turn that late (see the missile tests in weapons.rs).
pub const BREAK_WINDOW: (f32, f32) = (0.8, 1.8);

/// Range and seconds-to-impact of a missile chasing the player.
fn time_to_impact(missile_pos: Vec3, missile_vel: Vec3, player_pos: Vec3, player_vel: Vec3) -> (f32, f32) {
    let to_player = player_pos - missile_pos;
    let dist = to_player.length();
    let closing = (missile_vel - player_vel).dot(to_player / dist.max(1.0));
    (dist, if closing > 1.0 { dist / closing } else { f32::INFINITY })
}

pub struct HUDPlugin;

impl Plugin for HUDPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(FrameTimeDiagnosticsPlugin::default());
        // FLIGHT_SIM_NO_HUD=1: skip the HUD entirely, for clean landscape screenshots.
        if std::env::var("FLIGHT_SIM_NO_HUD").is_ok() {
            return;
        }
        app
            // the HUD is plane-related only; score etc. live in the pause menu
            .add_systems(Startup, setup)
            .add_systems(
                PostUpdate,
                (
                    arrange,
                    spawn_markers,
                    update_markers,
                    update_center_symbols,
                    update_readouts,
                    update_warnings,
                    gun_cue,
                    target_panel,
                    spawn_missile_markers,
                    update_missile_markers,
                    kill_banner,
                    hit_marker,
                )
                    .chain()
                    .after(TransformSystems::Propagate),
            );
    }
}

/// Which readout a text element shows.
#[derive(Component, Clone, Copy, PartialEq, Eq, Debug)]
enum Readout {
    Speed,
    Altitude,
    Throttle,
    GLoad,
    Weapons,
    Health,
}

#[derive(Component, Clone, Copy, PartialEq, Eq)]
enum Warning {
    Stall,
    PullUp,
    Missile,
    Break,
    Locked,
    Respawn,
}

/// A box that tracks one aircraft.
#[derive(Component)]
struct TargetMarker {
    target: Entity,
    color: Color,
}

#[derive(Component)]
struct MarkerLabel;

/// Another player's handle, above their box.
#[derive(Component)]
struct NameTag;

const NAME_TAG_WIDTH: f32 = 200.0;

#[derive(Component)]
struct LockRing;

#[derive(Component)]
enum CenterSymbol {
    /// Where the nose points.
    Boresight,
    /// Where the plane is actually going (velocity vector).
    FlightPath,
    /// Mouse-aim point.
    AimCircle,
    /// Points at the selected target when it's off screen.
    OffscreenArrow,
}

#[derive(Resource)]
struct HudFont(Handle<Font>);

fn text(font: &Handle<Font>, size: f32, color: Color) -> (TextFont, TextColor) {
    (
        TextFont {
            font: font.clone().into(),
            font_size: FontSize::Px(size),
            ..default()
        },
        TextColor(color),
    )
}

fn abs_node(left: Val, top: Val, w: Val, h: Val) -> Node {
    Node {
        position_type: PositionType::Absolute,
        left,
        top,
        width: w,
        height: h,
        ..default()
    }
}

fn setup(mut commands: Commands, asset_server: Res<AssetServer>) {
    let font: Handle<Font> = asset_server.load("fonts/RobotoMono/RobotoMono-Medium.ttf");
    commands.insert_resource(HudFont(font.clone()));

    // (placed by `arrange`: see `Layout`)
    for (kind, justify) in [
        (Readout::Speed, Justify::Right),
        (Readout::GLoad, Justify::Right),
        (Readout::Altitude, Justify::Left),
        (Readout::Throttle, Justify::Left),
        (Readout::Weapons, Justify::Left),
        (Readout::Health, Justify::Left),
    ] {
        let spot = Layout::DESKTOP.spot(Some(kind));
        commands.spawn((
            Text::new(""),
            text(&font, spot.font, HUD_GREEN),
            TextLayout::justify(justify),
            shadow(),
            spot.node(),
            kind,
        ));
    }

    // centre warnings
    commands
        .spawn(Node {
            position_type: PositionType::Absolute,
            top: percent(62),
            width: percent(100),
            flex_direction: FlexDirection::Column,
            align_items: AlignItems::Center,
            row_gap: px(6),
            ..default()
        })
        .with_children(|p| {
            for (w, label, color) in [
                (Warning::Respawn, "", WARNING),
                (Warning::Break, "BREAK!", INCOMING),
                (Warning::Missile, "MISSILE", INCOMING),
                (Warning::PullUp, "PULL UP", HOSTILE),
                (Warning::Stall, "STALL", WARNING),
                (Warning::Locked, "LOCKED", HOSTILE),
            ] {
                p.spawn((
                    Text::new(label),
                    text(&font, 30.0, color),
                    shadow(),
                    Node { display: Display::None, ..default() },
                    w,
                ));
            }
        });

    // centre symbols
    let ring = |size: f32, border: f32, color: Color| {
        (
            Node {
                border: UiRect::all(px(border)),
                border_radius: BorderRadius::MAX,
                ..abs_node(px(0), px(0), px(size), px(size))
            },
            BorderColor::all(color),
        )
    };
    let dot = |offset: f32, size: f32, color: Color| {
        (
            Node {
                border_radius: BorderRadius::MAX,
                ..abs_node(px(offset), px(offset), px(size), px(size))
            },
            BackgroundColor(color),
        )
    };

    commands
        .spawn((ring(18.0, 2.0, HUD_GREEN), CenterSymbol::FlightPath))
        .with_children(|p| {
            // little wings and tail on the flight path marker
            for (l, t, w, h) in [(-14.0, 6.0, 12.0, 2.0), (16.0, 6.0, 12.0, 2.0), (6.0, -10.0, 2.0, 8.0)] {
                p.spawn((abs_node(px(l), px(t), px(w), px(h)), BackgroundColor(HUD_GREEN)));
            }
        });
    commands
        .spawn((abs_node(px(0), px(0), px(24), px(24)), CenterSymbol::Boresight))
        .with_children(|p| {
            p.spawn((abs_node(px(0), px(11), px(9), px(2)), BackgroundColor(HUD_GREEN)));
            p.spawn((abs_node(px(15), px(11), px(9), px(2)), BackgroundColor(HUD_GREEN)));
            p.spawn((abs_node(px(11), px(0), px(2), px(9)), BackgroundColor(HUD_GREEN)));
        });
    commands
        .spawn((ring(34.0, 1.5, Color::srgba(1.0, 1.0, 1.0, 0.8)), CenterSymbol::AimCircle))
        .with_child(dot(13.0, 5.0, Color::WHITE));
    // gun cue: lead pipper, a line from the target to it, and a SHOOT cue
    commands
        .spawn((
            Node {
                border: UiRect::all(px(2)),
                border_radius: BorderRadius::MAX,
                display: Display::None,
                // keep the centre dot centred whatever size the ring is
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                ..abs_node(px(0), px(0), px(44), px(44))
            },
            BorderColor::all(HOSTILE),
            GunCue::Pipper,
        ))
        .with_child((
            Node {
                width: px(6),
                height: px(6),
                border_radius: BorderRadius::MAX,
                ..default()
            },
            BackgroundColor(HOSTILE),
        ));
    commands.spawn((
        Node { display: Display::None, ..abs_node(px(0), px(0), px(10), px(2)) },
        BackgroundColor(HOSTILE.with_alpha(0.7)),
        UiTransform::default(),
        GunCue::LeadLine,
    ));
    commands.spawn((
        Text::new("SHOOT"),
        text(&font, 18.0, Color::WHITE),
        shadow(),
        Node { display: Display::None, ..abs_node(px(0), px(0), Val::Auto, Val::Auto) },
        GunCue::Shoot,
    ));
    // kill banner, above centre
    commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                top: percent(24),
                width: percent(100),
                flex_direction: FlexDirection::Column,
                align_items: AlignItems::Center,
                row_gap: px(2),
                display: Display::None,
                ..default()
            },
            KillBanner,
        ))
        .with_children(|p| {
            p.spawn((Text::new("SPLASH"), text(&font, 44.0, HUD_GREEN), shadow(), KillBannerText::Title));
            p.spawn((Text::new(""), text(&font, 18.0, HUD_GREEN), shadow(), KillBannerText::Detail));
        });
    // hit marker: four short ticks around the boresight
    commands
        .spawn((
            Node { display: Display::None, ..abs_node(px(0), px(0), px(40), px(40)) },
            UiTransform::from_rotation(Rot2::degrees(45.0)),
            HitMarker,
        ))
        .with_children(|p| {
            for (l, t, w, h) in [(0.0, 19.0, 12.0, 2.0), (28.0, 19.0, 12.0, 2.0), (19.0, 0.0, 2.0, 12.0), (19.0, 28.0, 2.0, 12.0)] {
                p.spawn((abs_node(px(l), px(t), px(w), px(h)), BackgroundColor(Color::WHITE)));
            }
        });
    // target info, bottom right
    let spot = Layout::DESKTOP.spot(None);
    commands.spawn((
        Text::new(""),
        text(&font, spot.font, HOSTILE),
        TextLayout::justify(Justify::Right),
        shadow(),
        spot.node(),
        TargetPanel,
    ));
    commands.spawn((
        Node {
            border: UiRect::all(px(3)),
            ..abs_node(px(0), px(0), px(18), px(18))
        },
        BorderColor {
            top: HOSTILE,
            right: HOSTILE,
            bottom: Color::NONE,
            left: Color::NONE,
        },
        UiTransform::from_rotation(Rot2::degrees(45.0)),
        CenterSymbol::OffscreenArrow,
    ));
}

/// Where a readout (or the target panel) sits: absolute insets and a font size.
#[derive(Clone, Copy, PartialEq, Debug)]
struct Spot {
    left: Val,
    right: Val,
    top: Val,
    bottom: Val,
    font: f32,
}

impl Spot {
    const fn new(left: Val, right: Val, top: Val, bottom: Val, font: f32) -> Self {
        Spot { left, right, top, bottom, font }
    }

    fn node(&self) -> Node {
        let mut node = Node { position_type: PositionType::Absolute, ..default() };
        self.apply(&mut node);
        node
    }

    fn apply(&self, node: &mut Node) {
        if (node.left, node.right, node.top, node.bottom) != (self.left, self.right, self.top, self.bottom) {
            (node.left, node.right, node.top, node.bottom) = (self.left, self.right, self.top, self.bottom);
        }
    }
}

/// Where the readouts and the target panel go. On the desktop: speed + G
/// left of centre, altitude + throttle right of it, weapons and health
/// bottom left, the target panel bottom right. With the touch controls up
/// they move clear of them (`Layout::touch`).
#[derive(Clone, Copy, PartialEq, Debug)]
struct Layout {
    speed: Spot,
    g: Spot,
    altitude: Spot,
    throttle: Spot,
    weapons: Spot,
    health: Spot,
    panel: Spot,
}

const AUTO: Val = Val::Auto;

/// A line of text at `font` px, in px (roughly).
fn line(font: f32) -> f32 {
    font * 1.2
}

/// `chars` characters of the (monospaced) HUD font at `font` px, in px.
fn text_width(chars: usize, font: f32) -> f32 {
    chars as f32 * font * 0.6
}

impl Layout {
    const DESKTOP: Layout = Layout {
        speed: Spot::new(AUTO, Val::Percent(66.0), Val::Percent(42.0), AUTO, 20.0),
        g: Spot::new(AUTO, Val::Percent(66.0), Val::Percent(52.0), AUTO, 16.0),
        altitude: Spot::new(Val::Percent(66.0), AUTO, Val::Percent(42.0), AUTO, 20.0),
        throttle: Spot::new(Val::Percent(66.0), AUTO, Val::Percent(52.0), AUTO, 16.0),
        weapons: Spot::new(Val::Px(24.0), AUTO, AUTO, Val::Px(52.0), 18.0),
        health: Spot::new(Val::Px(24.0), AUTO, AUTO, Val::Px(24.0), 18.0),
        panel: Spot::new(AUTO, Val::Px(24.0), AUTO, Val::Px(24.0), 17.0),
    };

    /// `None`: the target panel.
    fn spot(&self, readout: Option<Readout>) -> Spot {
        match readout {
            Some(Readout::Speed) => self.speed,
            Some(Readout::GLoad) => self.g,
            Some(Readout::Altitude) => self.altitude,
            Some(Readout::Throttle) => self.throttle,
            Some(Readout::Weapons) => self.weapons,
            Some(Readout::Health) => self.health,
            None => self.panel,
        }
    }

    /// With the touch controls up on a `size` screen (logical px; `safe`:
    /// the safe-area insets): the speed and G readouts go up clear of the
    /// buttons below them, weapons and health go just right of the buttons
    /// at the bottom, and the target panel left of the throttle slider
    /// (raised above weapons and health if they'd meet). All a little
    /// smaller, and in pixels, since phones are short.
    fn touch(size: Vec2, safe: Vec4, clear: &crate::touch::Clearance) -> Layout {
        let (w, h) = (size.x, size.y);
        // smaller type on a short screen
        let k = (h / 400.0).clamp(0.75, 1.0);
        // the tutorial's step count and prompt across the top (its status line is short and centred)
        let top_min = safe.y + 84.0;
        let under = |x0: f32, x1: f32, y0: f32, y1: f32| {
            clear.buttons.iter().filter(move |(c, r)| {
                let r = r * 1.15 + 6.0;
                c.x - r < x1 && c.x + r > x0 && c.y - r < y1 && c.y + r > y0
            })
        };
        // The speed and G readouts: (top, right edge, scale). Where they'd sit
        // on the desktop (in px: 42% down, right edge 34% across) if the
        // buttons leave room; else up clear of the buttons below, if that
        // stays clear of the prompt; else in toward the middle, right of the
        // buttons alongside; else all that again a size smaller.
        let place = |k: f32, margin: f32| {
            let (speed_h, g_h) = (2.0 * line(20.0 * k), 2.0 * line(16.0 * k));
            let stack_h = speed_h + 4.0 + g_h;
            let block_w = text_width(10, 20.0 * k);
            let (x1, top) = (w * 0.34, (h * 0.42).max(top_min));
            if under(x1 - block_w, x1, top, top + stack_h).next().is_none() {
                return Some((top, x1));
            }
            let clear_top = under(x1 - block_w, x1, 0.0, h).map(|(c, r)| c.y - r * 1.15 - 6.0).fold(h, f32::min);
            if clear_top - stack_h >= top_min {
                return Some((clear_top - stack_h, x1));
            }
            let right = under(0.0, w * 0.5, top, top + stack_h).map(|(c, r)| c.x + r * 1.15 + 8.0).fold(0.0, f32::max);
            let x1 = x1.max(right + block_w);
            (x1 < w * 0.5 - margin).then_some((top, x1))
        };
        // (last, a little closer to the middle still)
        let (speed_top, x1, k_read) = [(k, 40.0), (k * 0.88, 40.0), (k * 0.76, 40.0), (k * 0.76, 18.0)]
            .into_iter()
            .find_map(|(k, margin)| place(k, margin).map(|(top, x1)| (top, x1, k)))
            .unwrap_or((top_min, w * 0.34, k * 0.76));
        let (big, small) = (20.0 * k_read, 16.0 * k_read);
        let speed_h = 2.0 * line(big);
        let g_top = speed_top + speed_h + 4.0;
        // (the right-hand pair mirrors the left)
        let (left_x, right_x) = (Val::Px(w - x1), Val::Px(w - x1));

        let ammo_left = clear.buttons_right + 14.0;
        // "GUN 600   MSL 4 [===  ]   FLR 4/4", smaller on a narrow phone so it fits before the throttle
        let ammo_font = ((clear.throttle_left - 12.0 - ammo_left) / text_width(34, 1.0)).clamp(11.0, 15.0 * k.max(0.85));
        let health_bottom = safe.w + 10.0;
        let weapons_bottom = health_bottom + line(ammo_font) + 2.0;
        let ammo_w = text_width(34, ammo_font);
        let panel_font = 14.0 * k.max(0.85);
        let panel_right = w - clear.throttle_left + 12.0;
        // "1.7 km   +314 km/h closing" (no [TAB] on touch)
        let panel_w = text_width(27, panel_font);
        let panel_bottom = if w - panel_right - panel_w > ammo_left + ammo_w + 16.0 {
            health_bottom
        } else {
            weapons_bottom + line(ammo_font) + 12.0
        };
        Layout {
            speed: Spot::new(AUTO, left_x, px(speed_top), AUTO, big),
            g: Spot::new(AUTO, left_x, px(g_top), AUTO, small),
            altitude: Spot::new(right_x, AUTO, px(speed_top), AUTO, big),
            throttle: Spot::new(right_x, AUTO, px(g_top), AUTO, small),
            weapons: Spot::new(px(ammo_left), AUTO, AUTO, px(weapons_bottom), ammo_font),
            health: Spot::new(px(ammo_left), AUTO, AUTO, px(health_bottom), ammo_font),
            panel: Spot::new(AUTO, px(panel_right), AUTO, px(panel_bottom), panel_font),
        }
    }
}

/// Moves the readouts and the target panel clear of the touch controls while
/// they're up (and back for the desktop).
#[allow(clippy::type_complexity)]
fn arrange(
    touch: Res<crate::touch::TouchInput>,
    windows: Query<&Window, With<bevy::window::PrimaryWindow>>,
    mut texts: Query<(Option<&Readout>, &mut Node, &mut TextFont), Or<(With<Readout>, With<TargetPanel>)>>,
) {
    let size = crate::touch::screen_size(windows.single().ok());
    let layout = match size {
        Some(size) if touch.active => {
            Layout::touch(size, crate::platform::safe_area(), &crate::touch::clearance(size))
        }
        _ => Layout::DESKTOP,
    };
    for (readout, mut node, mut font) in &mut texts {
        let spot = layout.spot(readout.copied());
        spot.apply(&mut node);
        if font.font_size != FontSize::Px(spot.font) {
            font.font_size = FontSize::Px(spot.font);
        }
    }
}

/// Give every aircraft (except the player) a tracking box.
fn spawn_markers(
    mut commands: Commands,
    font: Res<HudFont>,
    new: Query<(Entity, &Team, Option<&crate::net::RemoteName>), (Added<Aircraft>, Without<Player>, Without<crate::backdrop::MenuJet>)>,
) {
    for (entity, team, name) in &new {
        let color = if name.is_some() {
            PLAYER
        } else if *team == Team::Red {
            HOSTILE
        } else {
            FRIENDLY
        };
        commands
            .spawn((
                Node {
                    border: UiRect::all(px(2)),
                    display: Display::None,
                    ..abs_node(px(0), px(0), px(26), px(26))
                },
                BorderColor::all(color),
                TargetMarker { target: entity, color },
            ))
            .with_children(|p| {
                if let Some(name) = name {
                    p.spawn((
                        Text::new(name.0.clone()),
                        text(&font.0, 12.0, color),
                        shadow(),
                        Node {
                            position_type: PositionType::Absolute,
                            bottom: percent(110),
                            width: px(NAME_TAG_WIDTH),
                            ..default()
                        },
                        TextLayout::justify(Justify::Center),
                        NameTag,
                    ));
                }
                p.spawn((
                    Text::new(""),
                    text(&font.0, 13.0, color),
                    shadow(),
                    Node {
                        position_type: PositionType::Absolute,
                        top: percent(105),
                        left: px(-30),
                        width: px(90),
                        ..default()
                    },
                    TextLayout::justify(Justify::Center),
                    MarkerLabel,
                ));
                // lock ring: closes in as lock builds, solid red when locked
                p.spawn((
                    Node {
                        border: UiRect::all(px(2)),
                        border_radius: BorderRadius::MAX,
                        display: Display::None,
                        ..abs_node(px(-10), px(-10), px(46), px(46))
                    },
                    BorderColor::all(HOSTILE),
                    LockRing,
                ));
            });
    }
}

fn in_front(cam: &GlobalTransform, p: Vec3) -> bool {
    (p - cam.translation()).dot(cam.forward().as_vec3()) > 0.0
}

#[allow(clippy::type_complexity)]
fn update_markers(
    mut commands: Commands,
    camera: Query<(&Camera, &GlobalTransform), With<MainCamera>>,
    player: Query<(&Transform, &Targeting), With<Player>>,
    aircraft: Query<(&GlobalTransform, &Team, Has<Disabled>), With<Aircraft>>,
    mut markers: Query<
        (Entity, &TargetMarker, &mut Node, &mut BorderColor, &Children),
        Without<LockRing>,
    >,
    mut labels: Query<&mut Text, With<MarkerLabel>>,
    mut rings: Query<(&mut Node, &mut BorderColor), (With<LockRing>, Without<NameTag>)>,
    mut tags: Query<&mut Node, (With<NameTag>, Without<LockRing>, Without<TargetMarker>)>,
) {
    let Ok((camera, cam_gt)) = camera.single() else {
        return;
    };
    let player = player.single().ok();
    for (marker_entity, marker, mut node, mut border, children) in &mut markers {
        let Ok((gt, team, disabled)) = aircraft.get(marker.target) else {
            commands.entity(marker_entity).despawn();
            continue;
        };
        let pos = gt.translation();
        let dist = player.map_or(0.0, |(t, _)| t.translation.distance(pos));
        let screen = camera.world_to_viewport(cam_gt, pos).ok();
        let (Some(screen), true, false) = (screen, in_front(cam_gt, pos), disabled) else {
            node.display = Display::None;
            continue;
        };
        let selected = player.is_some_and(|(_, t)| t.target == Some(marker.target));
        let lock = player.filter(|_| selected).map_or(0.0, |(_, t)| t.lock);

        // selected target gets a bigger box; distant contacts a smaller one
        let size = if selected {
            34.0
        } else if dist > 4_000.0 {
            14.0
        } else {
            22.0
        };
        node.display = Display::Flex;
        node.width = px(size);
        node.height = px(size);
        node.left = px(screen.x - size / 2.0);
        node.top = px(screen.y - size / 2.0);
        let _ = team;
        *border = BorderColor::all(if selected && lock >= 1.0 { Color::WHITE } else { marker.color });

        for child in children.iter() {
            if let Ok(mut tag) = tags.get_mut(child) {
                // centred over the box, whatever its size
                tag.left = px((size - NAME_TAG_WIDTH) / 2.0 - 2.0);
            }
            if let Ok(mut label) = labels.get_mut(child) {
                let value = if selected || dist < 3_000.0 {
                    format!("{:.1}", dist / 1000.0)
                } else {
                    String::new()
                };
                if label.0 != value {
                    label.0 = value;
                }
            }
            if let Ok((mut ring_node, mut ring_border)) = rings.get_mut(child) {
                if selected && lock > 0.0 {
                    let r = size + 12.0 + (1.0 - lock) * 50.0;
                    ring_node.display = Display::Flex;
                    ring_node.width = px(r);
                    ring_node.height = px(r);
                    ring_node.left = px((size - r) / 2.0 - 2.0);
                    ring_node.top = px((size - r) / 2.0 - 2.0);
                    *ring_border = BorderColor::all(if lock >= 1.0 { HOSTILE } else { WARNING });
                } else {
                    ring_node.display = Display::None;
                }
            }
        }
    }
}

#[allow(clippy::type_complexity)]
fn update_center_symbols(
    camera: Query<(&Camera, &GlobalTransform), With<MainCamera>>,
    mode: Res<ControlMode>,
    aim: Res<MouseAim>,
    player: Query<(&Transform, &FlightState, &Targeting), With<Player>>,
    targets: Query<(&Transform, &FlightState), (With<Aircraft>, Without<Player>)>,
    mut symbols: Query<(&CenterSymbol, &mut Node, &mut UiTransform)>,
) {
    let Ok((camera, cam_gt)) = camera.single() else {
        return;
    };
    // the camera may render to a window or (in headless test runs) an image
    let viewport = camera.logical_viewport_size().unwrap_or(Vec2::new(1280.0, 720.0));
    let player = player.single().ok();
    let place = |node: &mut Node, world: Option<Vec3>| {
        let size = match node.width {
            Val::Px(w) => w,
            _ => 0.0,
        };
        let screen = world
            .filter(|p| in_front(cam_gt, *p))
            .and_then(|p| camera.world_to_viewport(cam_gt, p).ok());
        match screen {
            Some(s) => {
                node.display = Display::Flex;
                node.left = px(s.x - size / 2.0);
                node.top = px(s.y - size / 2.0);
            }
            None => node.display = Display::None,
        }
    };

    for (symbol, mut node, mut ui_transform) in &mut symbols {
        let Some((transform, state, targeting)) = player else {
            node.display = Display::None;
            continue;
        };
        let pos = transform.translation;
        let target = targeting.target.and_then(|t| targets.get(t).ok());
        match symbol {
            CenterSymbol::Boresight => {
                place(&mut node, Some(pos + transform.forward() * 1_000.0))
            }
            CenterSymbol::FlightPath => place(
                &mut node,
                (state.airspeed > 20.0).then(|| pos + state.velocity.normalize() * 1_000.0),
            ),
            CenterSymbol::AimCircle => place(
                &mut node,
                (*mode == ControlMode::MouseAim).then(|| pos + aim.dir * 1_000.0),
            ),
            CenterSymbol::OffscreenArrow => {
                let Some((tt, _)) = target else {
                    node.display = Display::None;
                    continue;
                };
                let size = viewport;
                let screen = camera.world_to_viewport(cam_gt, tt.translation).ok();
                let on_screen = in_front(cam_gt, tt.translation)
                    && screen.is_some_and(|s| {
                        s.x > 0.0 && s.y > 0.0 && s.x < size.x && s.y < size.y
                    });
                if on_screen {
                    node.display = Display::None;
                    continue;
                }
                // direction on screen (y down), valid even when the target is behind us
                let rel = cam_gt.affine().inverse().transform_point3(tt.translation);
                let dir = Vec2::new(rel.x, -rel.y).normalize_or(Vec2::Y);
                let p = size / 2.0 + dir * (size.min_element() * 0.4);
                node.display = Display::Flex;
                node.left = px(p.x - 9.0);
                node.top = px(p.y - 9.0);
                // the chevron's corner points up-right (-45° in screen space)
                ui_transform.rotation =
                    Rot2::radians(dir.to_angle() + std::f32::consts::FRAC_PI_4);
            }
        }
    }
}

#[allow(clippy::type_complexity)]
fn update_readouts(
    player: Query<(&Transform, &FlightState, &Weapons, &Health, &ControlInput), With<Player>>,
    mut readouts: Query<(&Readout, &mut Text, &mut TextColor)>,
) {
    let player = player.single().ok();
    for (kind, mut text, mut color) in &mut readouts {
        let value = match (kind, player) {
            (_, None) => String::new(),
            (Readout::Speed, Some((_, s, ..))) => {
                format!("{:>4.0} km/h\nM {:.2}", s.airspeed * 3.6, s.mach)
            }
            (Readout::GLoad, Some((_, s, ..))) => {
                format!("G {:>4.1}\nAoA {:>3.0}", s.g_load, s.alpha.to_degrees())
            }
            (Readout::Altitude, Some((t, s, ..))) => {
                format!("{:>5.0} m\n{:>+4.0} m/s", t.translation.y, s.velocity.y)
            }
            (Readout::Throttle, Some((_, s, _, _, input))) => {
                let extra = if s.afterburner_on {
                    " AB"
                } else if input.airbrake {
                    " BRK"
                } else {
                    ""
                };
                format!("THR {:>3.0}%{extra}", s.spool * 100.0)
            }
            (Readout::Weapons, Some((_, _, w, ..))) => {
                // reload progress for the next missile as a little bar
                let reload = if w.missiles < crate::weapons::MAX_MISSILES {
                    let filled = (w.missile_reload * 5.0).floor() as usize;
                    format!(" [{}{}]", "=".repeat(filled), " ".repeat(5 - filled))
                } else {
                    String::new()
                };
                format!(
                    "GUN {:>3}   MSL {}{reload}   FLR {}/{}",
                    w.gun_ammo, w.missiles, w.flares, crate::weapons::MAX_FLARES
                )
            }
            (Readout::Health, Some((_, _, _, h, _))) => {
                let frac = (h.current / h.max).clamp(0.0, 1.0);
                let bars = (frac * 20.0).round() as usize;
                color.0 = if frac > 0.5 {
                    HUD_GREEN
                } else if frac > 0.25 {
                    WARNING
                } else {
                    HOSTILE
                };
                format!("HP [{}{}]", "|".repeat(bars), ".".repeat(20 - bars))
            }
        };
        if text.0 != value {
            text.0 = value;
        }
    }
}

#[allow(clippy::type_complexity)]
fn update_warnings(
    time: Res<Time>,
    terrain: Res<Terrain>,
    player: Query<(Entity, &Transform, &FlightState, &Aircraft, &Targeting), With<Player>>,
    targets: Query<&Transform, With<Aircraft>>,
    missiles: Query<(&GlobalTransform, &Missile)>,
    mut warnings: Query<(&Warning, &mut Node, &mut Text)>,
    mut dead_timer: Local<f32>,
) {
    let player = player.single().ok();
    let blink = (time.elapsed_secs() * 3.0).fract() < 0.6;
    if player.is_none() {
        *dead_timer += time.delta_secs();
    } else {
        *dead_timer = 0.0;
    }
    for (warning, mut node, mut text) in &mut warnings {
        let show = match (warning, player) {
            // the RESPAWN button (see `respawn`) replaced the countdown
            (Warning::Respawn, None) => false,
            (_, None) | (Warning::Respawn, _) => false,
            (Warning::Stall, Some((_, _, s, ac, _))) => s.stalling(ac),
            (Warning::PullUp, Some((_, t, s, _, _))) => {
                // will we hit something in the next ~4 s on this path?
                let p = t.translation;
                let ahead = p + s.velocity * 4.0;
                blink && s.velocity.y < -5.0 && ahead.y < terrain.max_height_along(p, ahead) + 50.0
            }
            (Warning::Missile | Warning::Break, Some((e, t, s, ..))) => {
                // the most urgent missile chasing us
                let soonest = missiles
                    .iter()
                    .filter(|(_, m)| m.target == Some(e))
                    .map(|(gt, m)| time_to_impact(gt.translation(), m.velocity, t.translation, s.velocity).1)
                    .fold(f32::INFINITY, f32::min);
                let incoming = missiles.iter().any(|(_, m)| m.target == Some(e));
                let in_window = (BREAK_WINDOW.0..BREAK_WINDOW.1).contains(&soonest);
                if *warning == Warning::Missile {
                    let value = if soonest.is_finite() {
                        format!("MISSILE {soonest:.1}s")
                    } else {
                        "MISSILE".to_string()
                    };
                    if text.0 != value {
                        text.0 = value;
                    }
                    incoming && !in_window
                } else {
                    in_window && blink
                }
            }
            (Warning::Locked, Some((_, t, _, _, tg))) => {
                tg.locked()
                    && tg
                        .target
                        .and_then(|e| targets.get(e).ok())
                        .is_some_and(|tt| tt.translation.distance(t.translation) < LOCK_RANGE)
            }
        };
        node.display = if show { Display::Flex } else { Display::None };
    }
}

/// A tight drop shadow so HUD text stays legible over the bright city.
fn shadow() -> TextShadow {
    TextShadow {
        offset: Vec2::splat(1.5),
        color: Color::srgba(0.0, 0.0, 0.0, 0.65),
    }
}

#[derive(Component, PartialEq, Eq, Clone, Copy)]
enum GunCue {
    Pipper,
    LeadLine,
    Shoot,
}

#[derive(Component)]
struct TargetPanel;

/// How far out the gun cue is shown, and where the gun is effective.
const GUN_CUE_RANGE: f32 = 3_000.0;
const GUN_RANGE: f32 = 1_200.0;

/// Gun lead pipper for the locked target: put your nose (the boresight cross)
/// on it and shoot. Shows a line from the target to where you need to aim,
/// and turns white with SHOOT when the solution is good.
#[allow(clippy::type_complexity)]
fn gun_cue(
    camera: Query<(&Camera, &GlobalTransform), With<MainCamera>>,
    player: Query<(&Transform, &FlightState, &Targeting, &Weapons), With<Player>>,
    targets: Query<(&Transform, &FlightState, &Aircraft), Without<Player>>,
    mut cues: Query<(&GunCue, &mut Node, &mut UiTransform, Option<&mut BorderColor>, Option<&Children>)>,
    mut dots: Query<&mut BackgroundColor>,
) {
    let Ok((camera, cam_gt)) = camera.single() else {
        return;
    };
    let to_screen = |p: Vec3| {
        in_front(cam_gt, p)
            .then(|| camera.world_to_viewport(cam_gt, p).ok())
            .flatten()
    };
    // work out the solution once
    let solution = player.single().ok().and_then(|(t, s, tg, w)| {
        let (tt, ts, ac) = tg.target.and_then(|e| targets.get(e).ok())?;
        let dist = tt.translation.distance(t.translation);
        if dist > GUN_CUE_RANGE {
            return None;
        }
        let lead = gun_lead_point(t.translation, s.velocity, tt.translation, ts.velocity);
        // on target: the nose points within the target's size of the lead point
        let lead_dir = (lead - t.translation).normalize_or_zero();
        let miss = lead_dir.angle_between(t.forward().as_vec3()) * dist;
        let good = miss < ac.radius * 1.2 && dist < GUN_RANGE && w.gun_ammo > 0;
        Some((to_screen(lead), to_screen(tt.translation), good, dist))
    });

    for (cue, mut node, mut ui_transform, border, children) in &mut cues {
        let Some((lead_screen, target_screen, good, dist)) = solution else {
            node.display = Display::None;
            continue;
        };
        let colour = if good {
            Color::WHITE
        } else if dist < GUN_RANGE {
            HOSTILE
        } else {
            HOSTILE.with_alpha(0.55)
        };
        match cue {
            GunCue::Pipper => {
                let Some(p) = lead_screen else {
                    node.display = Display::None;
                    continue;
                };
                // the ring shrinks as the target gets closer: a rough range cue
                let size = 30.0 + 30.0 * (dist / GUN_CUE_RANGE);
                node.display = Display::Flex;
                node.width = px(size);
                node.height = px(size);
                node.left = px(p.x - size / 2.0);
                node.top = px(p.y - size / 2.0);
                if let Some(mut b) = border {
                    *b = BorderColor::all(colour);
                }
                for child in children.into_iter().flatten() {
                    if let Ok(mut bg) = dots.get_mut(*child) {
                        bg.0 = colour;
                    }
                }
            }
            GunCue::LeadLine => {
                let (Some(a), Some(b)) = (target_screen, lead_screen) else {
                    node.display = Display::None;
                    continue;
                };
                let d = b - a;
                let len = d.length();
                if len < 25.0 {
                    node.display = Display::None;
                    continue;
                }
                // a thin bar centred between the two points, rotated onto the line
                let mid = (a + b) / 2.0;
                node.display = Display::Flex;
                node.width = px(len - 30.0);
                node.left = px(mid.x - (len - 30.0) / 2.0);
                node.top = px(mid.y - 1.0);
                ui_transform.rotation = Rot2::radians(d.to_angle());
            }
            GunCue::Shoot => {
                let (Some(p), true) = (lead_screen, good) else {
                    node.display = Display::None;
                    continue;
                };
                node.display = Display::Flex;
                node.left = px(p.x - 28.0);
                node.top = px(p.y + 30.0);
            }
        }
    }
}

/// Bottom-right: what you're locked on to.
#[allow(clippy::type_complexity)]
fn target_panel(
    touch: Res<crate::touch::TouchInput>,
    player: Query<(&Transform, &FlightState, &Targeting, &Team), With<Player>>,
    aircraft: Query<(&Transform, &FlightState, &Team), (With<Aircraft>, Without<Disabled>)>,
    mut panel: Query<&mut Text, With<TargetPanel>>,
) {
    let Ok(mut text) = panel.single_mut() else {
        return;
    };
    let value = player
        .single()
        .ok()
        .and_then(|(t, s, tg, team)| {
            let (tt, ts, _) = aircraft.get(tg.target?).ok()?;
            let hostiles: Vec<_> = aircraft.iter().filter(|(_, _, tm)| *tm != team).collect();
            let to = tt.translation - t.translation;
            let closure = (s.velocity - ts.velocity).dot(to.normalize_or_zero());
            let lock = if tg.locked() {
                "LOCKED".to_string()
            } else if tg.lock > 0.0 {
                format!("LOCKING {:>3.0}%", tg.lock * 100.0)
            } else {
                "NO LOCK".to_string()
            };
            Some(format!(
                "TGT  {} hostiles{}\n{:.1} km   {:>+5.0} km/h closing\n{:>5.0} m alt   {:>4.0} km/h\n{lock}",
                hostiles.len(),
                // (the keyboard's key; on touch the TGT button says it)
                if touch.active { "" } else { "  [TAB]" },
                to.length() / 1000.0,
                closure * 3.6,
                tt.translation.y,
                ts.airspeed * 3.6,
            ))
        })
        .unwrap_or_default();
    if text.0 != value {
        text.0 = value;
    }
}

/// A diamond on every missile in the air that isn't ours.
#[derive(Component)]
struct MissileMarker {
    missile: Entity,
}

fn spawn_missile_markers(
    mut commands: Commands,
    font: Res<HudFont>,
    new: Query<Entity, Added<Missile>>,
) {
    for missile in &new {
        commands
            .spawn((
                Node {
                    border: UiRect::all(px(2)),
                    display: Display::None,
                    ..abs_node(px(0), px(0), px(14), px(14))
                },
                BorderColor::all(WARNING),
                UiTransform::from_rotation(Rot2::degrees(45.0)),
                MissileMarker { missile },
            ))
            .with_child((
                Text::new(""),
                text(&font.0, 14.0, INCOMING),
                shadow(),
                Node {
                    position_type: PositionType::Absolute,
                    top: px(16),
                    left: px(16),
                    width: px(110),
                    ..default()
                },
                // undo the diamond's rotation for the label
                UiTransform::from_rotation(Rot2::degrees(-45.0)),
                MarkerLabel,
            ));
    }
}

#[allow(clippy::type_complexity)]
fn update_missile_markers(
    mut commands: Commands,
    time: Res<Time>,
    camera: Query<(&Camera, &GlobalTransform), With<MainCamera>>,
    player: Query<(Entity, &Transform, &Team, &FlightState), With<Player>>,
    missiles: Query<(&GlobalTransform, &Missile)>,
    mut markers: Query<(Entity, &MissileMarker, &mut Node, &mut BorderColor, &Children), Without<TargetMarker>>,
    mut labels: Query<&mut Text, With<MarkerLabel>>,
) {
    let Ok((camera, cam_gt)) = camera.single() else {
        return;
    };
    let player = player.single().ok();
    let blink = (time.elapsed_secs() * 5.0).fract() < 0.6;
    for (entity, marker, mut node, mut border, children) in &mut markers {
        let Ok((gt, missile)) = missiles.get(marker.missile) else {
            commands.entity(entity).despawn();
            continue;
        };
        let pos = gt.translation();
        let (threat, hostile, dist, tti) = match player {
            Some((me, t, team, state)) => {
                let (dist, tti) = time_to_impact(pos, missile.velocity, t.translation, state.velocity);
                (missile.target == Some(me), missile.team != *team, dist, tti)
            }
            None => (false, true, f32::MAX, f32::INFINITY),
        };
        if !hostile || !(threat || dist < 5_000.0) {
            node.display = Display::None;
            continue;
        }
        let viewport = camera.logical_viewport_size().unwrap_or(Vec2::new(1280.0, 720.0));
        let on_screen = in_front(cam_gt, pos)
            .then(|| camera.world_to_viewport(cam_gt, pos).ok())
            .flatten()
            .filter(|s| s.x > 0.0 && s.y > 0.0 && s.x < viewport.x && s.y < viewport.y);
        let screen = match (on_screen, threat) {
            (Some(s), _) => s,
            // missiles chasing you are usually behind: pin them to the screen edge
            (None, true) => {
                let rel = cam_gt.affine().inverse().transform_point3(pos);
                let dir = Vec2::new(rel.x, -rel.y).normalize_or(Vec2::Y);
                viewport / 2.0 + dir * (viewport.min_element() * 0.44)
            }
            (None, false) => {
                node.display = Display::None;
                continue;
            }
        };
        let in_window = (BREAK_WINDOW.0..BREAK_WINDOW.1).contains(&tti);
        let size = if threat { 22.0 } else { 11.0 };
        node.display = if threat && in_window && !blink { Display::None } else { Display::Flex };
        node.width = px(size);
        node.height = px(size);
        node.left = px(screen.x - size / 2.0);
        node.top = px(screen.y - size / 2.0);
        node.border = UiRect::all(px(if threat { 3.0 } else { 2.0 }));
        *border = BorderColor::all(if threat { INCOMING } else { WARNING.with_alpha(0.7) });
        for child in children.iter() {
            if let Ok(mut label) = labels.get_mut(child) {
                let value = if threat && tti.is_finite() {
                    format!("{:.1} km {:.1}s", dist / 1000.0, tti)
                } else if threat {
                    format!("{:.1} km", dist / 1000.0)
                } else {
                    String::new()
                };
                if label.0 != value {
                    label.0 = value;
                }
            }
        }
    }
}

#[derive(Component)]
struct KillBanner;

#[derive(Component, PartialEq, Eq)]
enum KillBannerText {
    Title,
    Detail,
}

#[derive(Component)]
struct HitMarker;

const KILL_BANNER_TIME: f32 = 2.5;

/// "SPLASH" when the player gets a kill ("SPLASH @bob" in multiplayer): pops in big, settles, fades out.
fn kill_banner(
    time: Res<Time>,
    mut kills: MessageReader<crate::game::PlayerKill>,
    mut banner: Query<(&mut Node, &mut UiTransform), With<KillBanner>>,
    mut texts: Query<(&KillBannerText, &mut Text, &mut TextColor, &mut TextFont)>,
    mut timer: Local<f32>,
) {
    if let Some(kill) = kills.read().last() {
        *timer = KILL_BANNER_TIME;
        for (kind, mut text, _, mut font) in &mut texts {
            text.0 = match (kind, kill.friendly, &kill.name) {
                (KillBannerText::Title, false, Some(name)) => format!("SPLASH @{name}"),
                (KillBannerText::Title, false, None) => "SPLASH".into(),
                (KillBannerText::Title, true, _) => "FRIENDLY FIRE".into(),
                (KillBannerText::Detail, ..) => format!("KILL {}", kill.total),
            };
            if *kind == KillBannerText::Title {
                // (long handles: smaller, so it stays on the screen)
                let size = if text.0.chars().count() > 16 { 30.0 } else { 44.0 };
                if font.font_size != FontSize::Px(size) {
                    font.font_size = FontSize::Px(size);
                }
            }
        }
    }
    *timer = (*timer - time.delta_secs()).max(0.0);
    let Ok((mut node, mut transform)) = banner.single_mut() else {
        return;
    };
    if *timer <= 0.0 {
        node.display = Display::None;
        return;
    }
    node.display = Display::Flex;
    let age = KILL_BANNER_TIME - *timer;
    // quick pop: 1.4x shrinking to 1x over 0.15 s
    transform.scale = Vec2::splat(1.0 + 0.4 * (1.0 - (age / 0.15).min(1.0)));
    let alpha = (*timer / 0.6).min(1.0);
    for (kind, _, mut color, _) in &mut texts {
        let base = if *kind == KillBannerText::Title { HUD_GREEN } else { HUD_GREEN.with_alpha(0.85) };
        color.0 = base.with_alpha(base.alpha() * alpha);
    }
}

/// Brief X around the boresight whenever the player's bullets or missiles hit.
fn hit_marker(
    time: Res<Time>,
    mut hits: MessageReader<crate::weapons::HitConfirmed>,
    camera: Query<(&Camera, &GlobalTransform), With<MainCamera>>,
    player: Query<(Entity, &Transform), With<Player>>,
    mut marker: Query<&mut Node, With<HitMarker>>,
    mut timer: Local<f32>,
) {
    let player = player.single().ok();
    if hits.read().any(|h| Some(h.attacker) == player.map(|(e, _)| e)) {
        *timer = 0.18;
    }
    *timer = (*timer - time.delta_secs()).max(0.0);
    let Ok(mut node) = marker.single_mut() else {
        return;
    };
    let (Some((_, t)), Ok((camera, cam_gt)), true) = (player, camera.single(), *timer > 0.0) else {
        node.display = Display::None;
        return;
    };
    let nose = t.translation + t.forward() * 1_000.0;
    match in_front(cam_gt, nose).then(|| camera.world_to_viewport(cam_gt, nose).ok()).flatten() {
        Some(p) => {
            node.display = Display::Flex;
            node.left = px(p.x - 20.0);
            node.top = px(p.y - 20.0);
        }
        None => node.display = Display::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (left, top, right, bottom) of a block of `lines` x `chars` text at a spot.
    fn rect(size: Vec2, s: Spot, chars: usize, lines: usize, right_aligned_at: Option<f32>) -> Rect {
        let (w, h) = (text_width(chars, s.font), lines as f32 * line(s.font));
        let left = match (s.left, s.right, right_aligned_at) {
            (_, _, Some(x)) => x - w,
            (Val::Px(l), ..) => l,
            (Val::Percent(p), ..) => size.x * p / 100.0,
            (_, Val::Px(r), _) => size.x - r - w,
            other => panic!("{other:?}"),
        };
        let top = match (s.top, s.bottom) {
            (Val::Px(t), _) => t,
            (_, Val::Px(b)) => size.y - b - h,
            other => panic!("{other:?}"),
        };
        Rect::new(left, top, left + w, top + h)
    }

    fn hits(r: Rect, (c, rad): (Vec2, f32)) -> bool {
        let nearest = c.clamp(r.min, r.max);
        nearest.distance(c) < rad
    }

    #[test]
    fn touch_hud_keeps_clear_of_the_buttons() {
        // (phones in landscape at DPR 1.5-3, a tablet; 574x265 is what headless Chrome's phone emulation gave)
        for size in [
            Vec2::new(852.0, 393.0),
            Vec2::new(667.0, 375.0),
            Vec2::new(932.0, 430.0),
            Vec2::new(574.0, 265.0),
            Vec2::new(640.0, 300.0),
            Vec2::new(1180.0, 820.0),
        ] {
            // (a notch's insets only on phone-sized screens: 574 px less 118 of insets has no room left)
            let notch = (size.x > 600.0).then_some(Vec4::new(59.0, 0.0, 59.0, 21.0));
            for safe in [Some(Vec4::ZERO), notch].into_iter().flatten() {
                let clear = {
                    // (the touch layout reads the safe area itself natively as zero: shift by hand)
                    let mut c = crate::touch::clearance(size - Vec2::new(safe.x + safe.z, safe.w));
                    for b in &mut c.buttons {
                        b.0.x += safe.x;
                    }
                    c.buttons_right += safe.x;
                    c.throttle_left += safe.x;
                    c
                };
                let l = Layout::touch(size, safe, &clear);
                let Val::Px(right) = l.speed.right else { panic!() };
                let x1 = size.x - right;
                assert!(x1 < size.x * 0.5 - 15.0, "{size} {safe}: speed into the middle ({x1})");
                let speed = rect(size, l.speed, 10, 2, Some(x1));
                let g = rect(size, l.g, 8, 2, Some(x1));
                let Val::Px(top) = l.speed.top else { panic!() };
                assert!(top >= safe.y + 84.0, "{size} {safe}: speed up in the prompt ({top})");
                let weapons = rect(size, l.weapons, 34, 1, None);
                let health = rect(size, l.health, 25, 1, None);
                let panel = rect(size, l.panel, 27, 4, None);
                for (name, r) in [("speed", speed), ("g", g), ("weapons", weapons), ("health", health), ("panel", panel)] {
                    for b in &clear.buttons {
                        assert!(!hits(r, *b), "{size} {safe}: {name} {r:?} on a button {b:?}");
                    }
                    assert!(r.max.x < clear.throttle_left, "{size} {safe}: {name} {r:?} on the throttle");
                    assert!(r.min.y >= 0.0 && r.max.y <= size.y - safe.w, "{size} {safe}: {name} off screen {r:?}");
                }
                assert!(speed.max.y <= g.min.y, "{size}: speed on G");
                assert!(panel.intersect(weapons).is_empty() && panel.intersect(health).is_empty(), "{size} {safe}: panel on ammo");
            }
        }
    }
}
