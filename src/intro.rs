//! Opening shot: when the round starts the camera looks at the jet from the
//! front, then swings smoothly round to the chase position, the letterbox
//! bars slide away and the HUD flickers on. Any key, click, tap or gamepad A skips it.
//!
//! The camera path itself lives in `camera::follow_player` (it ends exactly on
//! the gameplay camera pose); this module owns the clock, the letterbox and
//! hiding the HUD.

use bevy::{input::mouse::MouseButton, prelude::*};

use crate::{game::Player, pause::Paused};

pub struct IntroPlugin;

impl Plugin for IntroPlugin {
    fn build(&self, app: &mut App) {
        let skip = std::env::var("FLIGHT_SIM_NO_INTRO").is_ok();
        app.insert_resource(Intro { t: if skip { INTRO_LEN } else { 0.0 } })
            .add_systems(Startup, spawn_letterbox)
            .add_systems(Update, (advance, hide_hud, letterbox).chain());
    }
}

/// Total length of the intro (s).
pub const INTRO_LEN: f32 = 5.5;
/// The opening front shot is held this long before the camera starts moving.
pub const INTRO_HOLD: f32 = 1.2;
/// The HUD flickers on over the last this-many seconds.
const HUD_BOOT: f32 = 0.45;

#[derive(Resource)]
pub struct Intro {
    /// Seconds since the round started.
    pub t: f32,
}

impl Intro {
    pub fn playing(&self) -> bool {
        self.t < INTRO_LEN
    }

    /// Camera move progress, 0 during the hold, eased to 1 at the end.
    pub fn sweep(&self) -> f32 {
        let u = ((self.t - INTRO_HOLD) / (INTRO_LEN - INTRO_HOLD)).clamp(0.0, 1.0);
        // smootherstep: gentle start and a soft landing on the chase pose
        u * u * u * (u * (u * 6.0 - 15.0) + 10.0)
    }
}

/// UI that stays visible during the intro (menus, prompts).
#[derive(Component)]
pub struct KeepDuringIntro;

#[derive(Component)]
struct Letterbox;

fn advance(
    time: Res<Time>,
    paused: Res<Paused>,
    keys: Res<ButtonInput<KeyCode>>,
    mouse: Res<ButtonInput<MouseButton>>,
    touches: Res<Touches>,
    gamepads: Query<&Gamepad>,
    player: Query<(), With<Player>>,
    mut intro: ResMut<Intro>,
) {
    if !intro.playing() || paused.0 || player.is_empty() {
        return;
    }
    let skip = keys.get_just_pressed().any(|k| *k != KeyCode::Escape)
        || mouse.get_just_pressed().next().is_some()
        || touches.any_just_pressed()
        || gamepads.iter().any(|g| g.just_pressed(GamepadButton::South));
    if skip {
        // jump to the HUD boot so it still lands nicely
        intro.t = intro.t.max(INTRO_LEN - HUD_BOOT);
    }
    // the first frames after the terrain bake can be long: don't let them eat the shot
    intro.t += time.delta_secs().min(1.0 / 20.0);
}

fn hide_hud(
    intro: Res<Intro>,
    mut was_playing: Local<bool>,
    mut roots: Query<
        &mut Visibility,
        (With<Node>, Without<ChildOf>, Without<KeepDuringIntro>, Without<Letterbox>),
    >,
) {
    let playing = intro.playing();
    if !playing && !*was_playing {
        return;
    }
    *was_playing = playing;
    // a quick boot flicker over the last moment, then on for good
    let boot = intro.t - (INTRO_LEN - HUD_BOOT);
    let show = !playing || (boot > 0.0 && (boot / 0.07) as i32 % 2 == 1);
    let target = if show { Visibility::Inherited } else { Visibility::Hidden };
    for mut v in &mut roots {
        if *v != target {
            *v = target;
        }
    }
}

fn spawn_letterbox(mut commands: Commands) {
    for top in [true, false] {
        commands.spawn((
            Letterbox,
            Node {
                position_type: PositionType::Absolute,
                left: px(0),
                width: percent(100),
                height: percent(11),
                top: if top { percent(0) } else { auto() },
                bottom: if top { auto() } else { percent(0) },
                ..default()
            },
            BackgroundColor(Color::BLACK),
            GlobalZIndex(4),
        ));
    }
}

fn letterbox(
    intro: Res<Intro>,
    state: Res<State<crate::title::GameState>>,
    mut bars: Query<(&mut Node, &mut Visibility), With<Letterbox>>) {
    // slide out alongside the end of the camera move
    let out = ((intro.t - (INTRO_LEN - 1.0)) / 0.8).clamp(0.0, 1.0);
    let mut h = 11.0 * (1.0 - out * out * (3.0 - 2.0 * out));
    // no bars over the title screen
    if *state.get() != crate::title::GameState::Playing {
        h = 0.0;
    }
    for (mut node, mut vis) in &mut bars {
        let target = if h <= 0.01 { Visibility::Hidden } else { Visibility::Inherited };
        if *vis != target {
            *vis = target;
        }
        if node.height != percent(h) {
            node.height = percent(h);
        }
    }
}
