//! Touchscreen controls (phones and tablets). They stay hidden until the first
//! touch, so mouse/keyboard players never see them.
//!
//! - Swipe anywhere to move the aim point; an autopilot flies to it, banking
//!   into turns and levelling the wings once the nose is on it
//!   (`controls::player_flight_input`).
//! - Left: the buttons. GUN (hold), MSL, FLR, TGT (nearest the crosshair), and
//!   pause in the top corner. A finger that lands on a button belongs to it:
//!   dragging it doesn't aim.
//! - Right edge: the throttle slider (top end = afterburner, bottom = airbrake).
//!
//! Each finger gets its role from where it first lands and keeps it until
//! lifted. This feeds the same resources as the other inputs: `PlayerActions`
//! (buttons), `MouseAim` (via `TouchInput::pan`, applied in
//! `controls::update_mouse_aim`) and `TouchInput::throttle`.

use bevy::{platform::collections::HashMap, prelude::*, window::PrimaryWindow};

use crate::{
    controls::{PlayerActions, ThrottleLever},
    intro::Intro,
    pause::Paused,
    respawn::RespawnPrompt,
};

pub struct TouchPlugin;

impl Plugin for TouchPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<TouchInput>()
            .add_systems(Startup, spawn_ui)
            .add_systems(
                Update,
                (read_touches.run_if(in_state(crate::loading::GameState::Playing)), layout_ui)
                    .chain()
                    .in_set(TouchSystems),
            );
    }
}

/// Runs after `controls::gather_actions` and before the aim update.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct TouchSystems;

/// What the touch controls want this frame.
#[derive(Resource, Default)]
pub struct TouchInput {
    /// A touch has been seen: show the controls and fly by autopilot.
    pub active: bool,
    /// Aim drag accumulated since the aim last consumed it, in pixels.
    pub pan: Vec2,
    /// The throttle slider is being held at this lever position.
    pub throttle: Option<f32>,
    pub afterburner: bool,
    pub airbrake: bool,
    fingers: HashMap<u64, Role>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Role {
    Flare,
    Gun,
    Missile,
    Target,
    Pause,
    Throttle,
    Pan,
}

/// Swipe-to-aim sensitivity relative to the mouse's.
pub const PAN_SCALE: f32 = 1.8;

/// Where everything sits for a given window size, in logical pixels.
struct Layout {
    scale: f32,
    flare: Vec2,
    gun: Vec2,
    gun_r: f32,
    missile: Vec2,
    target: Vec2,
    pause: Vec2,
    small_r: f32,
    /// Throttle track: x centre, top y, bottom y.
    throttle_x: f32,
    throttle_top: f32,
    throttle_bottom: f32,
}

impl Layout {
    fn new(size: Vec2) -> Self {
        let s = (size.min_element() / 420.0).clamp(0.7, 1.3);
        // keep clear of the notch and the home indicator
        let safe = crate::platform::safe_area();
        let (left, top, right, bottom) = (safe.x, safe.y, size.x - safe.z, size.y - safe.w);
        Layout {
            scale: s,
            gun: Vec2::new(left + 90.0 * s, bottom - 90.0 * s),
            gun_r: 56.0 * s,
            missile: Vec2::new(left + 205.0 * s, bottom - 62.0 * s),
            flare: Vec2::new(left + 70.0 * s, bottom - 215.0 * s),
            target: Vec2::new(left + 180.0 * s, bottom - 175.0 * s),
            small_r: 32.0 * s,
            pause: Vec2::new(left + 34.0 * s, top + 34.0 * s),
            throttle_x: right - 46.0 * s,
            throttle_top: top + (bottom - top) * 0.25,
            throttle_bottom: bottom - 40.0 * s,
        }
    }

    fn buttons(&self) -> [(Role, Vec2, f32); 5] {
        [
            (Role::Gun, self.gun, self.gun_r),
            (Role::Missile, self.missile, self.small_r * 1.15),
            (Role::Flare, self.flare, self.small_r),
            (Role::Target, self.target, self.small_r),
            (Role::Pause, self.pause, self.small_r * 0.8),
        ]
    }

    fn role_at(&self, p: Vec2) -> Role {
        // generous hit areas: fingers are imprecise and nothing else is nearby
        if let Some((role, ..)) = self.buttons().into_iter().find(|(_, c, r)| p.distance(*c) <= r * 1.3) {
            return role;
        }
        if (p.x - self.throttle_x).abs() < 40.0 * self.scale
            && p.y > self.throttle_top - 30.0
            && p.y < self.throttle_bottom + 30.0
        {
            Role::Throttle
        } else {
            Role::Pan
        }
    }

    /// Throttle slider position -> (lever, afterburner, airbrake). The top
    /// 12% of the track is the afterburner detent, the bottom 8% the airbrake.
    fn throttle_at(&self, y: f32) -> (f32, bool, bool) {
        let u = 1.0 - (y - self.throttle_top) / (self.throttle_bottom - self.throttle_top);
        if u >= 0.88 {
            (1.0, true, false)
        } else if u <= 0.08 {
            (0.0, false, true)
        } else {
            ((u - 0.08) / 0.8, false, false)
        }
    }

    /// Lever value -> y on the track (inverse of `throttle_at` over the middle).
    fn throttle_y(&self, lever: f32) -> f32 {
        let u = 0.08 + lever.clamp(0.0, 1.0) * 0.8;
        self.throttle_top + (1.0 - u) * (self.throttle_bottom - self.throttle_top)
    }
}

#[allow(clippy::too_many_arguments)]
fn read_touches(
    touches: Res<Touches>,
    windows: Query<&Window, With<PrimaryWindow>>,
    intro: Res<Intro>,
    prompt: Res<RespawnPrompt>,
    mut paused: ResMut<Paused>,
    mut actions: ResMut<PlayerActions>,
    mut input: ResMut<TouchInput>,
) {
    let Ok(window) = windows.single() else {
        return;
    };
    let layout = Layout::new(window.size());
    if touches.any_just_pressed() {
        input.active = true;
    }
    for t in touches.iter_just_released().chain(touches.iter_just_canceled()) {
        input.fingers.remove(&t.id());
    }
    // the controls are hidden during the intro, while paused (bar the pause
    // button) and while the RESPAWN button is up, so those touches go to the UI
    let controls_live = !intro.playing() && !paused.0 && !prompt.ready();
    for t in touches.iter_just_pressed() {
        let role = match layout.role_at(t.position()) {
            Role::Pause => Role::Pause,
            _ if !controls_live => continue,
            r => r,
        };
        input.fingers.insert(t.id(), role);
        match role {
            Role::Pause => paused.0 = !paused.0,
            Role::Flare => actions.drop_flares = true,
            Role::Missile => actions.fire_missile = true,
            Role::Target => actions.target_under_crosshair = true,
            _ => {}
        }
    }
    if !controls_live {
        // drop any held fingers so nothing sticks on when play resumes
        input.fingers.retain(|_, r| *r == Role::Pause);
    }

    input.throttle = None;
    input.afterburner = false;
    input.airbrake = false;
    let mut firing = false;
    let mut flaring = false;
    let fingers: Vec<(u64, Role)> = input.fingers.iter().map(|(k, v)| (*k, *v)).collect();
    for (id, role) in fingers {
        let Some(t) = touches.get_pressed(id) else {
            continue;
        };
        match role {
            Role::Gun => firing = true,
            Role::Flare => flaring = true,
            Role::Pan => input.pan += t.delta(),
            Role::Throttle => {
                let (lever, ab, brake) = layout.throttle_at(t.position().y);
                input.throttle = Some(lever);
                input.afterburner = ab;
                input.airbrake = brake;
            }
            _ => {}
        }
    }
    if controls_live {
        actions.firing_gun |= firing;
        actions.holding_flares |= flaring;
    }
}

// ---------------------------------------------------------------------------------------------
// On-screen UI
// ---------------------------------------------------------------------------------------------

#[derive(Component)]
struct TouchRoot;

#[derive(Component, Clone, Copy)]
enum Widget {
    Button(Role),
    ThrottleTrack,
    ThrottleFill,
    ThrottleHandle,
    ThrottleAb,
}

const FG: Color = Color::srgba(0.62, 0.9, 0.63, 0.85);
const FILL: Color = Color::srgba(0.62, 0.9, 0.63, 0.12);
const FILL_HOT: Color = Color::srgba(0.62, 0.9, 0.63, 0.4);
const AB_COLOR: Color = Color::srgba(1.0, 0.55, 0.2, 0.35);

fn spawn_ui(mut commands: Commands, asset_server: Res<AssetServer>) {
    let font: Handle<Font> = asset_server.load("fonts/RobotoMono/RobotoMono-Medium.ttf");
    let circle = |w: Widget, label: &str| {
        (
            w,
            Node {
                position_type: PositionType::Absolute,
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                border: UiRect::all(px(2)),
                border_radius: BorderRadius::MAX,
                ..default()
            },
            BorderColor::all(FG),
            BackgroundColor(FILL),
            children![(
                Text::new(label),
                TextFont { font: font.clone().into(), font_size: FontSize::Px(14.0), ..default() },
                TextColor(FG),
            )],
        )
    };
    let bar = |w: Widget, color: Color, border: bool| {
        (
            w,
            Node {
                position_type: PositionType::Absolute,
                border: UiRect::all(px(if border { 2 } else { 0 })),
                border_radius: BorderRadius::all(px(6)),
                ..default()
            },
            BorderColor::all(FG),
            BackgroundColor(color),
        )
    };
    commands.spawn((
        TouchRoot,
        Node {
            position_type: PositionType::Absolute,
            width: percent(100),
            height: percent(100),
            display: Display::None,
            ..default()
        },
        // under the menus and the respawn prompt
        GlobalZIndex(1),
        Pickable::IGNORE,
        children![
            circle(Widget::Button(Role::Gun), "GUN"),
            circle(Widget::Button(Role::Missile), "MSL"),
            circle(Widget::Button(Role::Flare), "FLR"),
            circle(Widget::Button(Role::Target), "TGT"),
            circle(Widget::Button(Role::Pause), "II"),
            bar(Widget::ThrottleTrack, FILL, true),
            bar(Widget::ThrottleAb, AB_COLOR, false),
            bar(Widget::ThrottleFill, FILL_HOT, false),
            bar(Widget::ThrottleHandle, FG, false),
        ],
    ));
}

#[allow(clippy::too_many_arguments)]
fn layout_ui(
    input: Res<TouchInput>,
    lever: Res<ThrottleLever>,
    intro: Res<Intro>,
    paused: Res<Paused>,
    prompt: Res<RespawnPrompt>,
    state: Res<State<crate::loading::GameState>>,
    windows: Query<&Window, With<PrimaryWindow>>,
    mut root: Query<&mut Node, (With<TouchRoot>, Without<Widget>)>,
    mut widgets: Query<(&Widget, &mut Node, &mut BackgroundColor, &mut Visibility)>,
) {
    let (Ok(window), Ok(mut root)) = (windows.single(), root.single_mut()) else {
        return;
    };
    let playing = *state.get() == crate::loading::GameState::Playing;
    let display = if input.active && playing && !intro.playing() { Display::Flex } else { Display::None };
    if root.display != display {
        root.display = display;
    }
    if display == Display::None {
        return;
    }
    let l = Layout::new(window.size());
    // while paused or waiting to respawn only the pause button stays
    let only_pause = paused.0 || prompt.ready();
    let held = |role: Role| input.fingers.values().any(|r| *r == role);
    let track_w = 22.0 * l.scale;
    let rect = |node: &mut Node, x: f32, top: f32, w: f32, h: f32| {
        node.left = px(x - w * 0.5);
        node.top = px(top);
        node.width = px(w);
        node.height = px(h.max(0.0));
    };
    let lever_y = l.throttle_y(input.throttle.unwrap_or(lever.0));
    let ab_y = l.throttle_top + 0.12 * (l.throttle_bottom - l.throttle_top);
    let buttons = l.buttons();
    for (widget, mut node, mut bg, mut vis) in &mut widgets {
        let show = !only_pause || matches!(widget, Widget::Button(Role::Pause));
        let target_vis = if show { Visibility::Inherited } else { Visibility::Hidden };
        if *vis != target_vis {
            *vis = target_vis;
        }
        match *widget {
            Widget::Button(role) => {
                let Some(&(_, c, r)) = buttons.iter().find(|(b, ..)| *b == role) else { continue };
                rect(&mut node, c.x, c.y - r, 2.0 * r, 2.0 * r);
                bg.0 = if held(role) { FILL_HOT } else { FILL };
            }
            Widget::ThrottleTrack => {
                rect(&mut node, l.throttle_x, l.throttle_top, track_w, l.throttle_bottom - l.throttle_top)
            }
            Widget::ThrottleAb => {
                rect(&mut node, l.throttle_x, l.throttle_top, track_w, ab_y - l.throttle_top);
                bg.0 = if input.afterburner { Color::srgba(1.0, 0.55, 0.2, 0.8) } else { AB_COLOR };
            }
            Widget::ThrottleFill => {
                rect(&mut node, l.throttle_x, lever_y, track_w * 0.6, l.throttle_bottom - lever_y)
            }
            Widget::ThrottleHandle => {
                let h = 10.0 * l.scale;
                rect(&mut node, l.throttle_x, lever_y - h * 0.5, track_w * 2.2, h)
            }
        }
    }
}
