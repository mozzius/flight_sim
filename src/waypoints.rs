//! Waypoint gates: rings in the world to fly through in order, with a HUD
//! marker on the next one (a diamond and its distance) that turns into an
//! arrow at the edge of the screen when it's off screen or behind.
//!
//! Set a course with [`Course::set`]; the player's jet is checked against the
//! next gate every physics step ([`passes_through`]: the path crosses the
//! gate's plane inside its radius, either way through), and [`GatePassed`]
//! goes out for each one. The rings are unlit and additive (so they glow,
//! skip the fog and read from far away) and the next one pulses. The tutorial
//! uses it (`tutorial`), but nothing in here knows about that.

use bevy::{light::NotShadowCaster, prelude::*};

use crate::{
    camera::MainCamera,
    flight::{Disabled, FlightSystems},
    game::Player,
};

pub struct WaypointsPlugin;

impl Plugin for WaypointsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Course>()
            .add_message::<GatePassed>()
            .add_systems(Startup, (setup_assets, spawn_indicator))
            .add_systems(
                FixedUpdate,
                track_player
                    .after(FlightSystems::Physics)
                    .before(FlightSystems::Combat)
                    .run_if(in_state(crate::loading::GameState::Playing)),
            )
            .add_systems(OnExit(crate::loading::GameState::Playing), |mut course: ResMut<Course>| course.clear())
            .add_systems(Update, (sync_rings, animate_rings).chain())
            .add_systems(PostUpdate, update_indicator.after(TransformSystems::Propagate));
    }
}

/// One gate: a ring at `pos` facing along `normal` (the way through it).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Gate {
    pub pos: Vec3,
    pub normal: Vec3,
    /// The ring's radius; passing anywhere inside it counts.
    pub radius: f32,
}

impl Gate {
    pub fn new(pos: Vec3, normal: Vec3, radius: f32) -> Self {
        Gate { pos, normal: normal.normalize_or(Vec3::NEG_Z), radius }
    }
}

/// Whether the path `from` -> `to` (one step of flight) goes through `gate`:
/// it crosses the gate's plane (either way) inside its radius.
pub fn passes_through(gate: &Gate, from: Vec3, to: Vec3) -> bool {
    let a = (from - gate.pos).dot(gate.normal);
    let b = (to - gate.pos).dot(gate.normal);
    if (a > 0.0) == (b > 0.0) || a == b {
        return false;
    }
    let t = a / (a - b);
    from.lerp(to, t).distance(gate.pos) <= gate.radius
}

/// A gate was flown through (`index` into the course; `last` when it was the final one).
#[derive(Message, Clone, Copy, Debug)]
pub struct GatePassed {
    pub index: usize,
    pub last: bool,
}

/// The gates to fly through, in order.
#[derive(Resource, Default)]
pub struct Course {
    gates: Vec<Gate>,
    next: usize,
    /// Bumped on every new course (the rings are respawned).
    generation: u32,
    /// The flyer's position last step.
    last_pos: Option<Vec3>,
}

/// A jump bigger than this between steps is a respawn, not flight.
const TELEPORT: f32 = 600.0;

impl Course {
    pub fn set(&mut self, gates: Vec<Gate>) {
        self.gates = gates;
        self.next = 0;
        self.generation = self.generation.wrapping_add(1);
        self.last_pos = None;
    }

    pub fn clear(&mut self) {
        if !self.gates.is_empty() {
            self.set(Vec::new());
        }
    }

    pub fn gates(&self) -> &[Gate] {
        &self.gates
    }

    /// The gate to fly through next.
    pub fn next_gate(&self) -> Option<&Gate> {
        self.gates.get(self.next)
    }

    /// Every gate flown through (false for an empty course).
    pub fn finished(&self) -> bool {
        !self.gates.is_empty() && self.next >= self.gates.len()
    }

    /// The flyer jumped (a respawn): don't join the old and new positions up.
    pub fn reset_tracking(&mut self) {
        self.last_pos = None;
    }

    /// Feeds the flyer's position; returns the gate passed on the way here, if any.
    pub fn advance(&mut self, pos: Vec3) -> Option<usize> {
        let last = self.last_pos.replace(pos)?;
        if last.distance(pos) > TELEPORT {
            return None;
        }
        let gate = self.gates.get(self.next)?;
        if passes_through(gate, last, pos) {
            self.next += 1;
            Some(self.next - 1)
        } else {
            None
        }
    }
}

fn track_player(
    mut course: ResMut<Course>,
    player: Query<&Transform, (With<Player>, Without<Disabled>)>,
    mut passed: MessageWriter<GatePassed>,
) {
    let Ok(t) = player.single() else {
        course.reset_tracking();
        return;
    };
    if let Some(index) = course.advance(t.translation) {
        let last = index + 1 == course.gates.len();
        info!("waypoints: gate {} of {} passed", index + 1, course.gates.len());
        passed.write(GatePassed { index, last });
    }
}

// ---------------------------------------------------------------------------------------------
// rings

/// HDR colour of the next gate (unlit: base_color is the output, so it glows).
const NEXT: LinearRgba = LinearRgba::rgb(5.0, 2.6, 0.45);
/// The ones after it, fainter.
const LATER: LinearRgba = LinearRgba::rgb(1.2, 0.65, 0.12);
/// HUD colour for the marker and arrow.
const HUD_GOLD: Color = Color::srgb(1.0, 0.82, 0.3);
/// The ring tube's radius, as a share of the ring's.
const TUBE: f32 = 0.06;

#[derive(Resource)]
struct RingAssets {
    mesh: Handle<Mesh>,
    next: Handle<StandardMaterial>,
    later: Handle<StandardMaterial>,
}

/// One ring in the world.
#[derive(Component)]
struct RingVisual {
    index: usize,
    radius: f32,
}

fn setup_assets(mut commands: Commands, mut meshes: ResMut<Assets<Mesh>>, mut materials: ResMut<Assets<StandardMaterial>>) {
    let glow = |materials: &mut Assets<StandardMaterial>, c: LinearRgba| {
        materials.add(StandardMaterial {
            base_color: c.into(),
            unlit: true,
            // additive: glows, and comes after the atmosphere's haze
            alpha_mode: AlphaMode::Add,
            fog_enabled: false,
            ..default()
        })
    };
    // a unit ring (radius 1) lying in XZ, scaled per gate
    let mesh = Torus { minor_radius: TUBE, major_radius: 1.0 }.mesh().minor_resolution(10).major_resolution(48);
    commands.insert_resource(RingAssets {
        mesh: meshes.add(mesh),
        next: glow(&mut materials, NEXT),
        later: glow(&mut materials, LATER),
    });
}

/// Respawns the rings when the course changes.
fn sync_rings(
    mut commands: Commands,
    course: Res<Course>,
    assets: Res<RingAssets>,
    rings: Query<Entity, With<RingVisual>>,
    mut generation: Local<Option<u32>>,
) {
    if *generation == Some(course.generation) {
        return;
    }
    *generation = Some(course.generation);
    for e in &rings {
        commands.entity(e).despawn();
    }
    for (index, gate) in course.gates.iter().enumerate() {
        commands.spawn((
            Name::new(format!("Gate {}", index + 1)),
            RingVisual { index, radius: gate.radius },
            Mesh3d(assets.mesh.clone()),
            MeshMaterial3d(assets.later.clone()),
            // the torus's axis is +Y: turn it to face along the gate
            Transform::from_translation(gate.pos)
                .with_rotation(Quat::from_rotation_arc(Vec3::Y, gate.normal))
                .with_scale(Vec3::splat(gate.radius)),
            NotShadowCaster,
            Visibility::Hidden,
        ));
    }
}

/// Passed rings go, the next one pulses, the rest wait dimly.
fn animate_rings(
    time: Res<Time>,
    course: Res<Course>,
    assets: Res<RingAssets>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut rings: Query<(&RingVisual, &mut Transform, &mut Visibility, &mut MeshMaterial3d<StandardMaterial>)>,
) {
    let t = time.elapsed_secs();
    let pulse = 0.5 + 0.5 * (t * 3.0).sin();
    if let Some(mut m) = materials.get_mut(&assets.next) {
        m.base_color = (NEXT * (0.7 + 0.5 * pulse)).into();
    }
    for (ring, mut transform, mut vis, mut material) in &mut rings {
        let target_vis = if ring.index < course.next { Visibility::Hidden } else { Visibility::Inherited };
        if *vis != target_vis {
            *vis = target_vis;
        }
        let is_next = ring.index == course.next;
        let want = if is_next { &assets.next } else { &assets.later };
        if material.0 != *want {
            material.0 = want.clone();
        }
        let scale = ring.radius * if is_next { 1.0 + 0.04 * pulse } else { 1.0 };
        transform.scale = Vec3::splat(scale);
    }
}

// ---------------------------------------------------------------------------------------------
// HUD marker

#[derive(Component, Clone, Copy, PartialEq, Eq)]
enum Indicator {
    /// A diamond on the gate when it's on screen.
    Marker,
    /// An arrow at the screen edge pointing at it when it isn't.
    Arrow,
    /// The distance, under whichever is showing.
    Distance,
}

fn spawn_indicator(mut commands: Commands, asset_server: Res<AssetServer>) {
    let font: Handle<Font> = asset_server.load("fonts/RobotoMono/RobotoMono-Medium.ttf");
    let abs = |size: f32| Node {
        position_type: PositionType::Absolute,
        width: px(size),
        height: px(size),
        display: Display::None,
        ..default()
    };
    commands.spawn((
        Indicator::Marker,
        Node { border: UiRect::all(px(2)), ..abs(16.0) },
        BorderColor::all(HUD_GOLD),
        UiTransform::from_rotation(Rot2::degrees(45.0)),
        Pickable::IGNORE,
    ));
    commands.spawn((
        Indicator::Arrow,
        Node { border: UiRect::all(px(4)), ..abs(26.0) },
        BorderColor { top: HUD_GOLD, right: HUD_GOLD, bottom: Color::NONE, left: Color::NONE },
        UiTransform::default(),
        Pickable::IGNORE,
    ));
    commands.spawn((
        Indicator::Distance,
        Text::new(""),
        TextFont { font: font.into(), font_size: FontSize::Px(14.0), ..default() },
        TextColor(HUD_GOLD),
        TextShadow { offset: Vec2::splat(1.5), color: Color::srgba(0.0, 0.0, 0.0, 0.65) },
        TextLayout::justify(Justify::Center),
        Node { position_type: PositionType::Absolute, width: px(90), display: Display::None, ..default() },
        Pickable::IGNORE,
    ));
}

/// "850 m", "2.4 km".
pub fn format_distance(m: f32) -> String {
    if m < 1_000.0 { format!("{:.0} m", m) } else { format!("{:.1} km", m / 1_000.0) }
}

#[allow(clippy::type_complexity)]
fn update_indicator(
    course: Res<Course>,
    paused: Res<crate::pause::Paused>,
    camera: Query<(&Camera, &GlobalTransform), With<MainCamera>>,
    player: Query<&Transform, With<Player>>,
    mut parts: Query<(&Indicator, &mut Node, &mut UiTransform, Option<&mut Text>)>,
) {
    let gate = course.next_gate();
    let shown = gate.zip(camera.single().ok()).zip(player.single().ok()).filter(|_| !paused.0);
    let Some(((gate, (camera, cam_gt)), plane)) = shown else {
        for (_, mut node, ..) in &mut parts {
            if node.display != Display::None {
                node.display = Display::None;
            }
        }
        return;
    };
    let viewport = camera.logical_viewport_size().unwrap_or(Vec2::new(1280.0, 720.0));
    let in_front = (gate.pos - cam_gt.translation()).dot(cam_gt.forward().as_vec3()) > 0.0;
    let on_screen = camera
        .world_to_viewport(cam_gt, gate.pos)
        .ok()
        .filter(|s| in_front && s.x > 0.0 && s.y > 0.0 && s.x < viewport.x && s.y < viewport.y);
    // off screen: an arrow on a ring around the centre (inside the target arrow's)
    let (anchor, dir) = match on_screen {
        Some(s) => (s, Vec2::ZERO),
        None => {
            let rel = cam_gt.affine().inverse().transform_point3(gate.pos);
            let dir = Vec2::new(rel.x, -rel.y).normalize_or(Vec2::Y);
            (viewport / 2.0 + dir * (viewport.min_element() * 0.32), dir)
        }
    };
    let distance = format_distance(plane.translation.distance(gate.pos));
    for (part, mut node, mut ui_transform, text) in &mut parts {
        let show = match part {
            Indicator::Marker => on_screen.is_some(),
            Indicator::Arrow => on_screen.is_none(),
            Indicator::Distance => true,
        };
        node.display = if show { Display::Flex } else { Display::None };
        if !show {
            continue;
        }
        match part {
            Indicator::Marker => {
                node.left = px(anchor.x - 8.0);
                node.top = px(anchor.y - 8.0);
            }
            Indicator::Arrow => {
                node.left = px(anchor.x - 13.0);
                node.top = px(anchor.y - 13.0);
                // the chevron's corner points up-right (-45° in screen space)
                ui_transform.rotation = Rot2::radians(dir.to_angle() + std::f32::consts::FRAC_PI_4);
            }
            Indicator::Distance => {
                // under the marker, or on the inside of the arrow
                let at = if on_screen.is_some() { anchor + Vec2::new(0.0, 22.0) } else { anchor - dir * 34.0 - Vec2::new(0.0, 8.0) };
                node.left = px(at.x - 45.0);
                node.top = px(at.y);
                if let Some(mut text) = text
                    && text.0 != distance
                {
                    text.0 = distance.clone();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate() -> Gate {
        Gate::new(Vec3::new(0.0, 1_000.0, 0.0), Vec3::NEG_Z, 60.0)
    }

    #[test]
    fn flying_through_the_middle_passes() {
        let g = gate();
        assert!(passes_through(&g, Vec3::new(0.0, 1_000.0, 4.0), Vec3::new(0.0, 1_000.0, -4.0)));
        // near the rim, and on a slant
        assert!(passes_through(&g, Vec3::new(50.0, 1_010.0, 5.0), Vec3::new(58.0, 1_012.0, -5.0)));
        // the other way through counts too
        assert!(passes_through(&g, Vec3::new(0.0, 1_000.0, -4.0), Vec3::new(0.0, 1_000.0, 4.0)));
        // a step ending exactly on the plane counts on the next one
        let on = Vec3::new(0.0, 1_000.0, 0.0);
        assert!(!passes_through(&g, Vec3::new(0.0, 1_000.0, 4.0), on) || !passes_through(&g, on, Vec3::new(0.0, 1_000.0, -4.0)));
        assert!(passes_through(&g, Vec3::new(0.0, 1_000.0, 4.0), on) || passes_through(&g, on, Vec3::new(0.0, 1_000.0, -4.0)));
    }

    #[test]
    fn missing_the_ring_does_not() {
        let g = gate();
        // outside the radius
        assert!(!passes_through(&g, Vec3::new(70.0, 1_000.0, 4.0), Vec3::new(70.0, 1_000.0, -4.0)));
        assert!(!passes_through(&g, Vec3::new(0.0, 1_100.0, 4.0), Vec3::new(0.0, 1_100.0, -4.0)));
        // short of the plane, or already past it
        assert!(!passes_through(&g, Vec3::new(0.0, 1_000.0, 40.0), Vec3::new(0.0, 1_000.0, 10.0)));
        assert!(!passes_through(&g, Vec3::new(0.0, 1_000.0, -10.0), Vec3::new(0.0, 1_000.0, -40.0)));
        // skimming along the plane
        assert!(!passes_through(&g, Vec3::new(-5.0, 1_000.0, 0.0), Vec3::new(5.0, 1_000.0, 0.0)));
    }

    #[test]
    fn a_fast_step_through_still_counts() {
        // 250 m/s at 60 Hz is ~4 m a step; a 300 m step straight through the middle still crosses
        let g = gate();
        assert!(passes_through(&g, Vec3::new(0.0, 1_000.0, 150.0), Vec3::new(0.0, 1_000.0, -150.0)));
    }

    #[test]
    fn course_goes_gate_by_gate() {
        let mut course = Course::default();
        assert!(!course.finished(), "an empty course is never finished");
        course.set(vec![
            Gate::new(Vec3::new(0.0, 1_000.0, -1_000.0), Vec3::NEG_Z, 60.0),
            Gate::new(Vec3::new(0.0, 1_000.0, -2_000.0), Vec3::NEG_Z, 60.0),
        ]);
        let mut passed = vec![];
        // fly straight down -Z at 5 m a step, through both
        for i in 0..500 {
            if let Some(g) = course.advance(Vec3::new(0.0, 1_000.0, -(i as f32) * 5.0)) {
                passed.push(g);
            }
        }
        assert_eq!(passed, vec![0, 1]);
        assert!(course.finished());
        assert!(course.next_gate().is_none());
    }

    #[test]
    fn gates_go_in_order() {
        let mut course = Course::default();
        course.set(vec![
            Gate::new(Vec3::new(500.0, 1_000.0, -1_000.0), Vec3::NEG_Z, 60.0),
            Gate::new(Vec3::new(0.0, 1_000.0, -2_000.0), Vec3::NEG_Z, 60.0),
        ]);
        // the second gate first: it doesn't count until the first is done
        for i in 0..500 {
            assert_eq!(course.advance(Vec3::new(0.0, 1_000.0, -(i as f32) * 5.0)), None);
        }
        assert_eq!(course.next_gate().unwrap().pos.x, 500.0);
    }

    #[test]
    fn a_respawn_is_not_a_pass() {
        let mut course = Course::default();
        course.set(vec![Gate::new(Vec3::new(0.0, 1_000.0, 0.0), Vec3::NEG_Z, 60.0)]);
        course.advance(Vec3::new(0.0, 1_000.0, 2_000.0));
        // teleported to the far side: the jump crosses the ring, but isn't flight
        assert_eq!(course.advance(Vec3::new(0.0, 1_000.0, -10.0)), None);
        assert!(!course.finished());
    }

    #[test]
    fn distances_read_naturally() {
        assert_eq!(format_distance(850.4), "850 m");
        assert_eq!(format_distance(2_430.0), "2.4 km");
    }
}
