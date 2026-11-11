//! Heads-up display: flight data, weapons, warnings, and the targeting layer
//! (target boxes, lock indicator, gun lead pipper, flight path marker,
//! off-screen target arrow).

use bevy::{
    diagnostic::{DiagnosticsStore, FrameTimeDiagnosticsPlugin},
    prelude::*,
    window::PrimaryWindow,
};

use crate::{
    camera::MainCamera,
    controls::{ControlMode, MouseAim},
    flight::{Aircraft, ControlInput, Disabled, FlightState},
    game::{Health, Player, Score, Team},
    weapons::{LOCK_RANGE, Missile, Targeting, Weapons, gun_lead_point},
    world::Terrain,
};

const HUD_GREEN: Color = Color::srgb(0.4, 1.0, 0.55);
const HOSTILE: Color = Color::srgb(1.0, 0.3, 0.25);
const FRIENDLY: Color = Color::srgb(0.35, 0.8, 1.0);
const WARNING: Color = Color::srgb(1.0, 0.75, 0.2);

pub struct HUDPlugin;

impl Plugin for HUDPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(FrameTimeDiagnosticsPlugin::default())
            .add_systems(Startup, setup)
            .add_systems(
                PostUpdate,
                (
                    spawn_markers,
                    update_markers,
                    update_center_symbols,
                    update_readouts,
                    update_warnings,
                )
                    .chain()
                    .after(TransformSystems::Propagate),
            );
    }
}

/// Which readout a text element shows.
#[derive(Component, Clone, Copy)]
enum Readout {
    Fps,
    Speed,
    Altitude,
    Throttle,
    GLoad,
    Weapons,
    Health,
    Score,
    Event,
    Mode,
}

#[derive(Component, Clone, Copy, PartialEq, Eq)]
enum Warning {
    Stall,
    PullUp,
    Missile,
    Locked,
    Respawn,
}

/// A box that tracks one aircraft.
#[derive(Component)]
struct TargetMarker {
    target: Entity,
}

#[derive(Component)]
struct MarkerLabel;

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
    /// Gun lead pipper on the selected target.
    Pipper,
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

    let mut readout = |kind: Readout, node: Node, size: f32, justify: Justify| {
        commands.spawn((
            Text::new(""),
            text(&font, size, HUD_GREEN),
            TextLayout::justify(justify),
            shadow(),
            node,
            kind,
        ));
    };
    let at = |left: Option<Val>, right: Option<Val>, top: Option<Val>, bottom: Option<Val>| Node {
        position_type: PositionType::Absolute,
        left: left.unwrap_or(Val::Auto),
        right: right.unwrap_or(Val::Auto),
        top: top.unwrap_or(Val::Auto),
        bottom: bottom.unwrap_or(Val::Auto),
        ..default()
    };

    readout(Readout::Fps, at(None, Some(px(8)), Some(px(6)), None), 14.0, Justify::Right);
    // either side of the centre: speed + G on the left, altitude + throttle on the right
    readout(Readout::Speed, at(None, Some(percent(66)), Some(percent(42)), None), 20.0, Justify::Right);
    readout(Readout::GLoad, at(None, Some(percent(66)), Some(percent(52)), None), 16.0, Justify::Right);
    readout(Readout::Altitude, at(Some(percent(66)), None, Some(percent(42)), None), 20.0, Justify::Left);
    readout(Readout::Throttle, at(Some(percent(66)), None, Some(percent(52)), None), 16.0, Justify::Left);
    // bottom left: weapons and health
    readout(Readout::Weapons, at(Some(px(24)), None, None, Some(px(52))), 18.0, Justify::Left);
    readout(Readout::Health, at(Some(px(24)), None, None, Some(px(24))), 18.0, Justify::Left);

    // top centre: score, mode and events
    commands
        .spawn(Node {
            position_type: PositionType::Absolute,
            top: px(10),
            width: percent(100),
            flex_direction: FlexDirection::Column,
            align_items: AlignItems::Center,
            row_gap: px(4),
            ..default()
        })
        .with_children(|p| {
            for (kind, size) in [(Readout::Score, 16.0), (Readout::Mode, 13.0), (Readout::Event, 26.0)] {
                p.spawn((
                    Text::new(""),
                    text(&font, size, HUD_GREEN),
                    TextLayout::justify(Justify::Center),
                    shadow(),
                    kind,
                ));
            }
        });

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
                (Warning::Missile, "MISSILE", HOSTILE),
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
    commands
        .spawn((ring(44.0, 2.0, HOSTILE), CenterSymbol::Pipper))
        .with_child(dot(17.0, 6.0, HOSTILE));
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

/// Give every aircraft (except the player) a tracking box.
fn spawn_markers(
    mut commands: Commands,
    font: Res<HudFont>,
    new: Query<(Entity, &Team), (Added<Aircraft>, Without<Player>)>,
) {
    for (entity, team) in &new {
        let color = if *team == Team::Red { HOSTILE } else { FRIENDLY };
        commands
            .spawn((
                Node {
                    border: UiRect::all(px(2)),
                    display: Display::None,
                    ..abs_node(px(0), px(0), px(26), px(26))
                },
                BorderColor::all(color),
                TargetMarker { target: entity },
            ))
            .with_children(|p| {
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
    mut rings: Query<(&mut Node, &mut BorderColor), With<LockRing>>,
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
        let base = if *team == Team::Red { HOSTILE } else { FRIENDLY };
        *border = BorderColor::all(if selected && lock >= 1.0 { Color::WHITE } else { base });

        for child in children.iter() {
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
    window: Query<&Window, With<PrimaryWindow>>,
    mode: Res<ControlMode>,
    aim: Res<MouseAim>,
    player: Query<(&Transform, &FlightState, &Targeting), With<Player>>,
    targets: Query<(&Transform, &FlightState), (With<Aircraft>, Without<Player>)>,
    mut symbols: Query<(&CenterSymbol, &mut Node, &mut UiTransform)>,
) {
    let (Ok((camera, cam_gt)), Ok(window)) = (camera.single(), window.single()) else {
        return;
    };
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
            CenterSymbol::Pipper => {
                let lead = target.and_then(|(tt, ts)| {
                    (tt.translation.distance(pos) < 1_800.0).then(|| {
                        gun_lead_point(pos, state.velocity, tt.translation, ts.velocity)
                    })
                });
                place(&mut node, lead);
            }
            CenterSymbol::OffscreenArrow => {
                let Some((tt, _)) = target else {
                    node.display = Display::None;
                    continue;
                };
                let size = window.size();
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
    diagnostics: Res<DiagnosticsStore>,
    score: Res<Score>,
    mode: Res<ControlMode>,
    player: Query<(&Transform, &FlightState, &Weapons, &Health, &ControlInput), With<Player>>,
    mut readouts: Query<(&Readout, &mut Text, &mut TextColor)>,
) {
    let player = player.single().ok();
    for (kind, mut text, mut color) in &mut readouts {
        let value = match (kind, player) {
            (Readout::Fps, _) => diagnostics
                .get(&FrameTimeDiagnosticsPlugin::FPS)
                .and_then(|d| d.smoothed())
                .map_or(String::new(), |fps| format!("{fps:.0} FPS")),
            (Readout::Score, _) => format!(
                "WAVE {}   KILLS {}   DEATHS {}",
                score.wave, score.kills, score.deaths
            ),
            (Readout::Mode, _) => match *mode {
                ControlMode::MouseAim => "MOUSE AIM · M to switch".into(),
                ControlMode::Direct => "DIRECT · M to switch".into(),
            },
            (Readout::Event, _) => score
                .last_event
                .as_ref()
                .map_or(String::new(), |(s, _)| s.clone()),
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
            (Readout::Weapons, Some((_, _, w, ..))) => format!(
                "GUN {:>3}   MSL {}   FLR {}",
                w.gun_ammo, w.missiles, w.flares
            ),
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
    missiles: Query<&Missile>,
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
            (Warning::Respawn, None) => {
                let value = format!("RESPAWNING IN {:.0}", (4.0 - *dead_timer).max(0.0).ceil());
                if text.0 != value {
                    text.0 = value;
                }
                true
            }
            (_, None) | (Warning::Respawn, _) => false,
            (Warning::Stall, Some((_, _, s, ac, _))) => s.stalling(ac),
            (Warning::PullUp, Some((_, t, s, _, _))) => {
                // will we hit something in the next ~4 s on this path?
                let p = t.translation;
                let ahead = p + s.velocity * 4.0;
                blink && s.velocity.y < -5.0 && ahead.y < terrain.max_height_along(p, ahead) + 50.0
            }
            (Warning::Missile, Some((e, ..))) => {
                blink && missiles.iter().any(|m| m.target == Some(e))
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
