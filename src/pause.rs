//! Pause menu: Esc (gamepad: Start) freezes game time, frees the mouse and
//! lists the controls, plus the score and stats that used to clutter the HUD.

use bevy::{
    diagnostic::{DiagnosticsStore, FrameTimeDiagnosticsPlugin},
    prelude::*,
};

use crate::{controls::ControlMode, game::Score};

pub struct PausePlugin;

impl Plugin for PausePlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(Paused(std::env::var("FLIGHT_SIM_PAUSE_TEST").is_ok()))
            .add_systems(Startup, spawn_menu)
            .add_systems(Update, (toggle_pause, update_menu).chain());
    }
}

#[derive(Resource, Default)]
pub struct Paused(pub bool);

#[derive(Component)]
struct PauseMenu;

#[derive(Component)]
struct PauseStats;

const PANEL: Color = Color::srgba(0.02, 0.04, 0.03, 0.97);
const GREEN: Color = Color::srgb(0.4, 1.0, 0.55);
const DIM: Color = Color::srgb(0.65, 0.8, 0.7);

const KEYBOARD: &[(&str, &str)] = &[
    ("Mouse", "aim (mouse-aim mode)"),
    ("A / D", "roll"),
    ("W / S", "pitch down / up"),
    ("Q / E", "rudder"),
    ("Shift / Ctrl", "throttle (hold: afterburner / airbrake)"),
    ("Left click / Space", "gun"),
    ("Right click / F", "missile (lock first)"),
    ("X", "flares"),
    ("Tab / Shift+Tab", "next / previous target"),
    ("R / middle click", "target nearest the crosshair"),
    ("C", "look back"),
    ("M", "switch mouse-aim / direct control"),
    ("N", "mute"),
    ("Esc", "pause"),
];

const GAMEPAD: &[(&str, &str)] = &[
    ("Left stick", "pitch / roll"),
    ("LB / RB", "rudder"),
    ("RT / LT", "throttle (fully: afterburner / airbrake)"),
    ("A", "gun"),
    ("B", "missile"),
    ("X", "flares"),
    ("D-pad left / right", "previous / next target"),
    ("Y / D-pad up", "target nearest the crosshair"),
    ("Right stick", "look around"),
    ("Select", "switch control mode"),
    ("Start", "pause"),
];

fn spawn_menu(mut commands: Commands, asset_server: Res<AssetServer>) {
    let font: Handle<Font> = asset_server.load("fonts/RobotoMono/RobotoMono-Medium.ttf");
    let text = |s: &str, size: f32, color: Color| {
        (
            Text::new(s),
            TextFont {
                font: font.clone().into(),
                font_size: FontSize::Px(size),
                ..default()
            },
            TextColor(color),
        )
    };
    let column = |title: &str, rows: &[(&str, &str)]| {
        let mut children = vec![];
        children.push(text(title, 20.0, GREEN));
        for (key, action) in rows {
            children.push(text(&format!("{key:<20} {action}"), 15.0, DIM));
        }
        children
    };

    commands
        .spawn((
            PauseMenu,
            Node {
                position_type: PositionType::Absolute,
                width: percent(100),
                height: percent(100),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                display: Display::None,
                ..default()
            },
            BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.45)),
            // draw over the HUD
            GlobalZIndex(10),
        ))
        .with_children(|root| {
            root.spawn((
                Node {
                    flex_direction: FlexDirection::Column,
                    padding: UiRect::all(px(32)),
                    row_gap: px(20),
                    border_radius: BorderRadius::all(px(10)),
                    ..default()
                },
                BackgroundColor(PANEL),
            ))
            .with_children(|panel| {
                panel.spawn(text("PAUSED", 34.0, GREEN));
                panel
                    .spawn(Node {
                        column_gap: px(48),
                        ..default()
                    })
                    .with_children(|cols| {
                        for (title, rows) in [("KEYBOARD & MOUSE", KEYBOARD), ("GAMEPAD", GAMEPAD)] {
                            cols.spawn(Node {
                                flex_direction: FlexDirection::Column,
                                row_gap: px(6),
                                ..default()
                            })
                            .with_children(|col| {
                                for t in column(title, rows) {
                                    col.spawn(t);
                                }
                            });
                        }
                    });
                panel.spawn((text("", 16.0, GREEN), PauseStats));
                panel.spawn(text("Esc / Start: resume      Q: quit", 16.0, DIM));
            });
        });
}

fn toggle_pause(
    keys: Res<ButtonInput<KeyCode>>,
    gamepads: Query<&Gamepad>,
    mut paused: ResMut<Paused>,
    mut time: ResMut<Time<Virtual>>,
    mut exit: MessageWriter<AppExit>,
) {
    let pressed = keys.just_pressed(KeyCode::Escape)
        || gamepads.iter().any(|g| g.just_pressed(GamepadButton::Start));
    // keep game time in step with the flag (it can start paused in tests)
    if paused.0 != time.is_paused() && !pressed {
        if paused.0 { time.pause() } else { time.unpause() }
    }
    if pressed {
        paused.0 = !paused.0;
        if paused.0 {
            time.pause();
        } else {
            time.unpause();
        }
    }
    if paused.0 && keys.just_pressed(KeyCode::KeyQ) {
        exit.write(AppExit::Success);
    }
}

fn update_menu(
    paused: Res<Paused>,
    score: Res<Score>,
    mode: Res<ControlMode>,
    diagnostics: Res<DiagnosticsStore>,
    mut menu: Query<&mut Node, With<PauseMenu>>,
    mut stats: Query<&mut Text, With<PauseStats>>,
) {
    if let Ok(mut node) = menu.single_mut() {
        node.display = if paused.0 { Display::Flex } else { Display::None };
    }
    if !paused.0 {
        return;
    }
    let fps = diagnostics
        .get(&FrameTimeDiagnosticsPlugin::FPS)
        .and_then(|d| d.smoothed())
        .unwrap_or(0.0);
    if let Ok(mut text) = stats.single_mut() {
        text.0 = format!(
            "WAVE {}   KILLS {}   DEATHS {}      control: {}      {fps:.0} fps",
            score.wave,
            score.kills,
            score.deaths,
            match *mode {
                ControlMode::MouseAim => "mouse aim",
                ControlMode::Direct => "direct",
            },
        );
    }
}
