use bevy::{
    diagnostic::{DiagnosticsStore, FrameTimeDiagnosticsPlugin},
    prelude::*,
};

use super::{Controls, Enemy, MainCamera, Player};

#[derive(Component)]
struct FpsText;

#[derive(Component)]
struct StallWarningText;

#[derive(Component)]
struct SpeedText;

#[derive(Component)]
#[allow(dead_code)]
struct Reticule(Entity);

pub struct HUDPlugin;

impl Plugin for HUDPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(FrameTimeDiagnosticsPlugin::default())
            .add_systems(Startup, setup)
            .add_systems(
                Update,
                (
                    stall_warning_system,
                    speed_system,
                    // reticule_system,
                    fps_system,
                ),
            );
    }
}

fn setup(mut commands: Commands, asset_server: Res<AssetServer>) {
    let font = asset_server.load("fonts/RobotoMono/RobotoMono-Regular.ttf");

    // fps counter, top right
    commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                top: px(5.0),
                right: px(5.0),
                ..default()
            },
            BackgroundColor(Color::BLACK),
        ))
        .with_child((
            Text::new(""),
            TextFont {
                font: font.clone().into(),
                font_size: FontSize::Px(18.0),
                ..default()
            },
            TextColor(Color::WHITE),
            TextLayout::justify(Justify::Right),
            Node {
                margin: UiRect::horizontal(px(3.0)),
                ..default()
            },
            FpsText,
        ));

    // throttle readout, bottom left
    let throttle_font = TextFont {
        font: font.clone().into(),
        font_size: FontSize::Px(30.0),
        ..default()
    };
    commands
        .spawn(Node {
            position_type: PositionType::Absolute,
            bottom: percent(10.0),
            left: percent(20.0),
            ..default()
        })
        .with_child((
            Text::new("Throttle: "),
            throttle_font.clone(),
            TextColor(Color::WHITE),
            children![(TextSpan::default(), throttle_font, TextColor(Color::WHITE), SpeedText)],
        ));

    // stall warning, centered
    commands
        .spawn(Node {
            width: percent(100.0),
            height: percent(60.0),
            position_type: PositionType::Absolute,
            justify_content: JustifyContent::Center,
            align_items: AlignItems::FlexEnd,
            bottom: px(0.0),
            ..default()
        })
        .with_child((
            Text::new("!! STALL WARNING !!"),
            TextFont {
                font: font.into(),
                font_size: FontSize::Px(40.0),
                ..default()
            },
            TextColor(Color::srgb(1.0, 0.0, 0.0)),
            TextLayout::justify(Justify::Center),
            Node {
                align_self: AlignSelf::Center,
                ..default()
            },
            StallWarningText,
        ));
}

fn fps_system(diagnostics: Res<DiagnosticsStore>, mut query: Query<&mut Text, With<FpsText>>) {
    for mut text in query.iter_mut() {
        if let Some(average) = diagnostics
            .get(&FrameTimeDiagnosticsPlugin::FPS)
            .and_then(|fps| fps.average())
        {
            text.0 = format!("{:.0}", average);
        }
    }
}

fn stall_warning_system(
    mut text_query: Query<&mut Node, With<StallWarningText>>,
    player_query: Query<&Player>,
) {
    let Ok(player) = player_query.single() else {
        return;
    };
    for mut node in text_query.iter_mut() {
        node.display = if player.stalling {
            Display::Flex
        } else {
            Display::None
        };
    }
}

fn speed_system(mut span_query: Query<&mut TextSpan, With<SpeedText>>, controls: Res<Controls>) {
    for mut span in span_query.iter_mut() {
        span.0 = format!("{:.0}", controls.thrust);
    }
}

#[allow(dead_code)]
fn reticule_system(
    mut reticule_query: Query<(&mut Node, &Reticule)>,
    targets_query: Query<&GlobalTransform, With<Enemy>>,
    camera_query: Query<(&Camera, &GlobalTransform), With<MainCamera>>,
) {
    let Ok((camera, camera_transform)) = camera_query.single() else {
        return;
    };
    for (mut node, reticule) in reticule_query.iter_mut() {
        if let Ok(transform) = targets_query.get(reticule.0) {
            match camera.world_to_viewport(camera_transform, transform.translation()) {
                Ok(screen_position) => {
                    node.display = Display::Flex;
                    node.left = px(screen_position.x);
                    node.top = px(screen_position.y);
                }
                Err(_) => {
                    node.display = Display::None;
                }
            }
        } else {
            warn!("Could not find reticule for target");
        }
    }
}
