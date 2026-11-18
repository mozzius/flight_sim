//! The jets: F-16, F/A-18 and Su-37 from the Modern Fighter Jets pack,
//! converted by `scripts/jets/build.py` into `assets/models/jets/` (one
//! `.gltf` per livery over a shared `.bin`, plus `_low` variants with
//! half-size textures for phones).
//!
//! Every jet flies the same flight model (`Aircraft::fighter`) and has the
//! same hit radius, so the pick is looks only. What differs is in
//! [`JetSpec`]: nozzles (plume placement), wingtips, gun port, the wing the
//! vapour streams over, and the control surfaces.
//!
//! Control surfaces are glTF nodes named `stab_l`, `flap_r`, `rudder`, ...:
//! the converter puts each node's origin on the hinge line with local +X
//! along it, oriented so a positive rotation moves the trailing edge down
//! (or to port, for rudders). [`tag_parts`] finds them when a scene spawns
//! and [`animate_parts`] swings them from [`Stick`] (the pilot's smoothed
//! inputs; remote jets get theirs over the network), with flaps drooping when
//! slow, the F/A-18's rudders toeing in as its airbrake, the nozzles opening
//! in afterburner and the Su-37's nozzles vectoring.
//!
//! The cockpit (`cockpit`) and canopy (`canopy`) are their own nodes, so a
//! first-person view can hide or open them later.

use std::sync::atomic::{AtomicU8, Ordering};

use bevy::prelude::*;

use crate::flight::{ControlInput, Disabled, FlightState};

pub struct JetsPlugin;

impl Plugin for JetsPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, (follow_controls, tag_parts, animate_parts).chain());
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, Default)]
pub enum Jet {
    #[default]
    F16,
    F18,
    Su37,
}

impl Jet {
    pub const ALL: [Jet; 3] = [Jet::F16, Jet::F18, Jet::Su37];

    pub fn spec(self) -> &'static JetSpec {
        &SPECS[self as usize]
    }
}

/// Where things are on an airframe (aircraft space: metres, forward -Z, right +X).
pub struct JetSpec {
    /// File stem (`f16_<livery>.gltf`).
    pub key: &'static str,
    pub name: &'static str,
    /// Livery names (the menu's, and the file suffix).
    pub liveries: &'static [&'static str],
    /// Nozzle exit centres.
    pub nozzles: &'static [Vec3],
    /// Inner radius of a nozzle exit (the plume's width).
    pub nozzle_radius: f32,
    /// The right wingtip (the left mirrors it).
    pub wingtip: Vec3,
    pub gun_port: Vec3,
    /// The right wing's upper surface: root LE, tip LE, tip TE, root TE.
    pub wing: [Vec3; 4],
    /// Nozzles that swivel with the stick (thrust vectoring).
    pub vectoring: bool,
    /// The rudders toe in as the airbrake.
    pub rudder_brake: bool,
    /// Chase camera distance (1 = the old 21 m F-22's): a little closer to
    /// small jets, so they don't shrink to a speck, but not all the way.
    pub chase: f32,
}

const SPECS: [JetSpec; 3] = [
    JetSpec {
        key: "f16",
        name: "F-16",
        liveries: &["military", "arctic", "thunderbirds"],
        nozzles: &[Vec3::new(0.0, 0.11, 6.56)],
        nozzle_radius: 0.5,
        wingtip: Vec3::new(4.97, 0.03, 2.3),
        gun_port: Vec3::new(-0.75, 0.5, -4.6),
        wing: [Vec3::new(1.5, 0.3, -2.0), Vec3::new(4.5, 0.08, 0.9), Vec3::new(4.5, 0.08, 2.0), Vec3::new(1.5, 0.3, 2.6)],
        vectoring: false,
        rudder_brake: false,
        chase: 0.88,
    },
    JetSpec {
        key: "f18",
        name: "F/A-18",
        liveries: &["military", "tiger"],
        nozzles: &[Vec3::new(0.55, 0.11, 7.47), Vec3::new(-0.56, 0.11, 7.47)],
        nozzle_radius: 0.42,
        wingtip: Vec3::new(7.24, 0.25, 2.2),
        gun_port: Vec3::new(0.0, 0.75, -10.4),
        wing: [Vec3::new(2.2, 0.68, -2.5), Vec3::new(6.5, 0.3, 1.6), Vec3::new(6.5, 0.3, 3.0), Vec3::new(2.2, 0.68, 3.2)],
        vectoring: false,
        rudder_brake: true,
        chase: 0.98,
    },
    JetSpec {
        key: "su37",
        name: "Su-37",
        liveries: &["military"],
        nozzles: &[Vec3::new(1.29, 0.11, 9.02), Vec3::new(-1.29, 0.11, 9.02)],
        nozzle_radius: 0.52,
        wingtip: Vec3::new(7.94, 0.44, 4.4),
        gun_port: Vec3::new(1.1, 0.75, -5.8),
        wing: [Vec3::new(2.4, 1.0, -2.5), Vec3::new(7.2, 0.57, 3.0), Vec3::new(7.2, 0.57, 4.6), Vec3::new(2.4, 1.0, 5.5)],
        vectoring: true,
        rudder_brake: false,
        chase: 1.05,
    },
];

/// A jet and its paint: on every aircraft, and what goes over the wire.
#[derive(Component, Clone, Copy, PartialEq, Eq, Debug, Hash, Default)]
pub struct JetChoice {
    pub jet: Jet,
    /// Index into the jet's `liveries`.
    pub livery: u8,
}

impl JetChoice {
    pub const fn new(jet: Jet, livery: u8) -> Self {
        Self { jet, livery }
    }

    /// Every jet in every livery.
    pub fn all() -> impl Iterator<Item = JetChoice> {
        Jet::ALL.into_iter().flat_map(|jet| (0..jet.spec().liveries.len() as u8).map(move |l| JetChoice::new(jet, l)))
    }

    pub fn spec(self) -> &'static JetSpec {
        self.jet.spec()
    }

    pub fn livery_name(self) -> &'static str {
        let l = self.spec().liveries;
        l[(self.livery as usize).min(l.len() - 1)]
    }

    /// The glTF scene to load.
    pub fn path(self, low: bool) -> String {
        format!("models/jets/{}_{}{}.gltf", self.spec().key, self.livery_name(), if low { "_low" } else { "" })
    }

    /// Packed for the wire: jet in the high nibble, livery in the low one.
    pub fn to_wire(self) -> u8 {
        (self.jet as u8) << 4 | self.livery
    }

    /// Unknown jets (a newer client) fall back to the default; a livery past
    /// the end to the first.
    pub fn from_wire(b: u8) -> Self {
        let Some(jet) = Jet::ALL.get((b >> 4) as usize).copied() else { return Self::default() };
        let livery = b & 15;
        Self { jet, livery: if (livery as usize) < jet.spec().liveries.len() { livery } else { 0 } }
    }

    /// `f18:tiger` (or just `f18`), as stored in the settings and in `FLIGHT_SIM_JET`.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().to_ascii_lowercase();
        let (jet, livery) = s.split_once(':').unwrap_or((&s, ""));
        let jet = Jet::ALL.into_iter().find(|j| j.spec().key == jet)?;
        let livery = jet.spec().liveries.iter().position(|l| *l == livery).unwrap_or(0) as u8;
        Some(Self { jet, livery })
    }

    pub fn key(self) -> String {
        format!("{}:{}", self.spec().key, self.livery_name())
    }

    /// The next jet (its first livery), stepping through `Jet::ALL`.
    pub fn step_jet(self, step: i32) -> Self {
        let n = Jet::ALL.len() as i32;
        let i = (self.jet as i32 + step).rem_euclid(n);
        Self::new(Jet::ALL[i as usize], 0)
    }

    pub fn step_livery(self, step: i32) -> Self {
        let n = self.spec().liveries.len() as i32;
        Self::new(self.jet, (self.livery as i32 + step).rem_euclid(n) as u8)
    }
}

/// The player's pick (mirrors `Settings::jet`, for spawning without the resource).
static PLAYER: AtomicU8 = AtomicU8::new(0);

pub fn player() -> JetChoice {
    JetChoice::from_wire(PLAYER.load(Ordering::Relaxed))
}

pub fn set_player(choice: JetChoice) {
    PLAYER.store(choice.to_wire(), Ordering::Relaxed);
}

/// What the AI flies: hostiles are Su-37s, wingmen F-16s and F/A-18s in
/// military grey (by `n`, so a flight is mixed).
pub fn ai_jet(hostile: bool, n: usize) -> JetChoice {
    if hostile {
        JetChoice::new(Jet::Su37, 0)
    } else if n.is_multiple_of(2) {
        JetChoice::new(Jet::F16, 0)
    } else {
        JetChoice::new(Jet::F18, 0)
    }
}

/// The pilot's controls as the surfaces see them, smoothed: pitch (+ pull),
/// roll (+ right), yaw (+ right), airbrake and afterburner 0..1. Local jets
/// follow their `ControlInput`; the network sets remote jets' `target`.
#[derive(Component, Default, Clone, Copy, Debug)]
pub struct Stick {
    pub target: Vec3,
    pub airbrake_target: bool,
    pub now: Vec3,
    pub airbrake: f32,
    pub afterburner: f32,
}

fn follow_controls(time: Res<Time>, mut jets: Query<(&mut Stick, Option<&ControlInput>, &FlightState, Has<Disabled>)>) {
    let dt = time.delta_secs();
    // actuators: quick, but not instant
    let k = 1.0 - (-14.0 * dt).exp();
    let kb = 1.0 - (-3.0 * dt).exp();
    for (mut stick, input, state, disabled) in &mut jets {
        if let Some(i) = input {
            stick.target = Vec3::new(i.pitch, i.roll, i.yaw).clamp(Vec3::NEG_ONE, Vec3::ONE);
            stick.airbrake_target = i.airbrake;
        }
        if disabled {
            // shot down: the controls go limp where they were
            continue;
        }
        let target = stick.target;
        stick.now = stick.now.lerp(target, k);
        let brake = if stick.airbrake_target { 1.0 } else { 0.0 };
        stick.airbrake += (brake - stick.airbrake) * kb;
        let ab = if state.afterburner_on { 1.0 } else { 0.0 };
        stick.afterburner += (ab - stick.afterburner) * kb;
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PartKind {
    /// All-moving tailplane: pitch, plus differential for roll.
    Stab,
    /// Flaperon: roll, plus flap droop when slow.
    Flaperon,
    Aileron,
    Canard,
    Rudder,
    Nozzle,
}

/// A moving part of a jet's model.
#[derive(Component, Clone, Copy, Debug)]
pub struct Part {
    kind: PartKind,
    /// +1 right side, -1 left, 0 on the centreline.
    side: f32,
    aircraft: Entity,
    base: Quat,
}

fn part_of(name: &str) -> Option<(PartKind, f32)> {
    let (stem, side) = match name.rsplit_once('_') {
        Some((s, "l")) => (s, -1.0),
        Some((s, "r")) => (s, 1.0),
        _ => (name, 0.0),
    };
    let kind = match stem {
        "stab" => PartKind::Stab,
        "flap" => PartKind::Flaperon,
        "aileron" => PartKind::Aileron,
        "canard" => PartKind::Canard,
        "rudder" => PartKind::Rudder,
        "nozzle" => PartKind::Nozzle,
        _ => return None,
    };
    Some((kind, side))
}

/// Tags the moving parts of a jet's model as its scene spawns.
fn tag_parts(
    mut commands: Commands,
    named: Query<(Entity, &Name, &Transform), Added<Name>>,
    parents: Query<&ChildOf>,
    jets: Query<(), With<JetChoice>>,
) {
    for (entity, name, transform) in &named {
        let Some((kind, side)) = part_of(name.as_str()) else { continue };
        let Some(aircraft) = parents.iter_ancestors(entity).find(|a| jets.contains(*a)) else { continue };
        commands.entity(entity).insert(Part { kind, side, aircraft, base: transform.rotation });
    }
}

/// Control surface throws (radians).
const STAB_PITCH: f32 = 0.33;
const STAB_ROLL: f32 = 0.12;
const AILERON: f32 = 0.38;
const FLAPERON_ROLL: f32 = 0.3;
const FLAP_DROOP: f32 = 0.35;
const CANARD: f32 = 0.3;
const RUDDER: f32 = 0.45;
const RUDDER_TOE: f32 = 0.6;
/// Thrust vectoring (the Su-37).
const VECTOR: f32 = 0.2;

/// How far the flaps droop: none at speed, full when slow or at high AoA.
fn droop(state: &FlightState) -> f32 {
    let slow = ((130.0 - state.airspeed) / 60.0).clamp(0.0, 1.0);
    let aoa = ((state.alpha.to_degrees() - 10.0) / 10.0).clamp(0.0, 1.0);
    slow.max(aoa * 0.6)
}

/// The deflection of a part (radians about its hinge; + = trailing edge down
/// or to port), or the nozzle's (vector angle, opening).
fn deflection(kind: PartKind, side: f32, stick: &Stick, droop: f32, spec: &JetSpec) -> f32 {
    let Vec3 { x: pitch, y: roll, z: yaw } = stick.now;
    match kind {
        // pull: trailing edges up; roll right: the right one up, the left one down
        PartKind::Stab => -pitch * STAB_PITCH - side * roll * STAB_ROLL,
        PartKind::Aileron => -side * roll * AILERON,
        PartKind::Flaperon => -side * roll * FLAPERON_ROLL + droop * FLAP_DROOP,
        // a canard lifts the nose with its trailing edge down
        PartKind::Canard => pitch * CANARD,
        PartKind::Rudder => {
            // yaw right: trailing edge to starboard. Toe-in brake: each rudder's
            // trailing edge swings toward the centreline.
            let toe = if spec.rudder_brake { side * stick.airbrake * RUDDER_TOE } else { 0.0 };
            -yaw * RUDDER + toe
        }
        // the exhaust points up to pull (and up on the side that rolls down)
        PartKind::Nozzle if spec.vectoring => -(pitch + side * roll * 0.5).clamp(-1.0, 1.0) * VECTOR,
        PartKind::Nozzle => 0.0,
    }
}

/// The Su-37's nozzle vector angle for the nozzle at `x` (the plumes follow it).
pub fn vector_angle(choice: JetChoice, stick: &Stick, x: f32) -> f32 {
    let spec = choice.spec();
    if !spec.vectoring {
        return 0.0;
    }
    deflection(PartKind::Nozzle, x.signum(), stick, 0.0, spec)
}

fn animate_parts(
    jets: Query<(&JetChoice, &Stick, &FlightState)>,
    mut parts: Query<(&Part, &mut Transform)>,
) {
    for (part, mut transform) in &mut parts {
        let Ok((choice, stick, state)) = jets.get(part.aircraft) else { continue };
        let spec = choice.spec();
        let angle = deflection(part.kind, part.side, stick, droop(state), spec);
        let rotation = part.base * Quat::from_rotation_x(angle);
        if part.kind == PartKind::Nozzle {
            // the petals open up in afterburner
            let open = 0.94 + 0.16 * stick.afterburner;
            let scale = Vec3::new(open, open, 1.0);
            if transform.scale != scale {
                transform.scale = scale;
            }
        }
        if transform.rotation != rotation {
            transform.rotation = rotation;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_round_trip() {
        for c in JetChoice::all() {
            assert_eq!(JetChoice::from_wire(c.to_wire()), c);
            assert_eq!(JetChoice::parse(&c.key()), Some(c));
        }
        // unknown jet, bad livery
        assert_eq!(JetChoice::from_wire(0xf0), JetChoice::default());
        assert_eq!(JetChoice::from_wire(0x2f), JetChoice::new(Jet::Su37, 0));
        assert_eq!(JetChoice::parse("f18"), Some(JetChoice::new(Jet::F18, 0)));
        assert_eq!(JetChoice::parse("mig"), None);
    }

    #[test]
    fn six_liveries_no_blue_angels() {
        assert_eq!(JetChoice::all().count(), 6);
        assert!(JetChoice::all().all(|c| !c.livery_name().contains("angel")));
    }

    #[test]
    fn surfaces_deflect_the_right_way() {
        let spec = Jet::Su37.spec();
        let stick = |p: f32, r: f32, y: f32| Stick { now: Vec3::new(p, r, y), ..default() };
        // pull: stabs trailing edge up, canards down, nozzles exhaust up
        assert!(deflection(PartKind::Stab, 1.0, &stick(1.0, 0.0, 0.0), 0.0, spec) < 0.0);
        assert!(deflection(PartKind::Canard, 1.0, &stick(1.0, 0.0, 0.0), 0.0, spec) > 0.0);
        assert!(deflection(PartKind::Nozzle, 1.0, &stick(1.0, 0.0, 0.0), 0.0, spec) < 0.0);
        // roll right: right flaperon up, left down
        assert!(deflection(PartKind::Flaperon, 1.0, &stick(0.0, 1.0, 0.0), 0.0, spec) < 0.0);
        assert!(deflection(PartKind::Flaperon, -1.0, &stick(0.0, 1.0, 0.0), 0.0, spec) > 0.0);
        // yaw right: rudder trailing edge to starboard (negative = not to port)
        assert!(deflection(PartKind::Rudder, 0.0, &stick(0.0, 0.0, 1.0), 0.0, spec) < 0.0);
        // the hornet's brake toes the trailing edges in
        let brake = Stick { airbrake: 1.0, ..default() };
        let f18 = Jet::F18.spec();
        assert!(deflection(PartKind::Rudder, 1.0, &brake, 0.0, f18) > 0.0, "right rudder TE to port");
        assert!(deflection(PartKind::Rudder, -1.0, &brake, 0.0, f18) < 0.0, "left rudder TE to starboard");
    }
}
