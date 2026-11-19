//! The jets: F-16, F/A-18, Su-37 and A-10 from the Modern Fighter Jets pack,
//! converted by `scripts/jets/build.py` into `assets/models/jets/` (one
//! `.gltf` per livery over a shared `.bin`, plus `_low` variants with
//! half-size textures for phones).
//!
//! Every jet flies the same flight model with its own numbers
//! ([`JetSpec::flight`]: thrust, drag, roll and pitch rates, G and AoA
//! limits) and has the same hit radius. The fighters differ a little; the
//! A-10 is slow, with no afterburner, and carries the GAU-8
//! (`protocol::gun`: the server applies its damage). The rest of
//! [`JetSpec`] is placement: nozzles (plumes), wingtips, gun port, the wing
//! the vapour streams over, and the control surfaces.
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
//! The converted pack lives in a private repo (the licence doesn't allow
//! publishing it on its own): `scripts/fetch-assets.sh` clones it into
//! `assets/models/jets/`. Without it (a public clone) every jet loads as the
//! old F-22 ([`fallback`], [`JetChoice::airframe`]) and the menu hides JET / LIVERY.
//!
//! The cockpit is kept for a first-person view: `cockpit` (the instrument
//! panel), `hud` (its glass), `canopy` (frame and outer glass),
//! `canopy_inner` (the glass seen from inside: hidden in third person) and the
//! Su-37's `cockpit_tub` are their own nodes, tagged [`Cockpit`]; the pilot's
//! eye is [`JetSpec::eye`].

use std::sync::atomic::{AtomicU8, Ordering};

use bevy::{camera::visibility::VisibilityRange, prelude::*};

use crate::flight::{Aircraft, ControlInput, Disabled, FlightState};

pub struct JetsPlugin;

impl Plugin for JetsPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, (follow_controls, tag_parts, animate_parts, set_lods).chain());
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, Default)]
pub enum Jet {
    #[default]
    F16,
    F18,
    Su37,
    /// Wire id `protocol::JET_A10`.
    A10,
}

impl Jet {
    pub const ALL: [Jet; 4] = [Jet::F16, Jet::F18, Jet::Su37, Jet::A10];

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
    /// Half width and half height of a nozzle exit (the plume's size).
    pub nozzle_size: Vec2,
    /// The right wingtip (the left mirrors it).
    pub wingtip: Vec3,
    pub gun_port: Vec3,
    /// Missile stations on the right wing (centre of a hung missile), inboard
    /// to the tip; the left wing mirrors them. Empty: internal bays (the F-22).
    /// Each sits on its launch rail's midpoint, 0.08 m under the rail: the
    /// hung (0.8x) Sidewinder's lug against it.
    pub hardpoints: &'static [Vec3],
    /// The right wing's upper surface: root LE, tip LE, tip TE, root TE.
    pub wing: [Vec3; 4],
    /// Nozzles that swivel with the stick (thrust vectoring).
    pub vectoring: bool,
    /// The rudders toe in as the airbrake.
    pub rudder_brake: bool,
    /// The pilot's eye (read off the canopy and instrument panel).
    #[allow(dead_code)] // for the first-person view
    pub eye: Vec3,
    /// Chase camera distance (1 = the old 21 m F-22's): a little closer to
    /// small jets, so they don't shrink to a speck, but not all the way.
    pub chase: f32,
    /// Leading-edge extensions: the LEX vortices stream off them at high AoA.
    pub lex: bool,
    /// How much wing vapour it pulls (1 = a fighter's).
    pub vapour: f32,
    /// How it flies.
    pub flight: fn() -> Aircraft,
    /// Speed it spawns at (m/s).
    pub cruise: f32,
}

/// The F-22's eye, for a player whose jet type isn't known yet.
pub const F22_EYE: Vec3 = Vec3::new(0.0, 1.2, -6.2);

/// The F-22 (`models/f22-raptor`), flown by every jet when the pack is missing.
const F22: JetSpec = JetSpec {
    key: "f22",
    name: "F-22",
    liveries: &["default"],
    nozzles: &[Vec3::new(0.79, 0.27, 6.8), Vec3::new(-0.79, 0.27, 6.8)],
    // a 2D nozzle: wide and flat
    nozzle_size: Vec2::new(0.5, 0.36),
    wingtip: Vec3::new(7.3, 0.0, 2.0),
    gun_port: Vec3::new(0.9, 0.9, -7.5),
    hardpoints: &[],
    wing: [Vec3::new(1.9, 0.55, -3.2), Vec3::new(6.7, 0.2, 1.1), Vec3::new(7.1, 0.2, 2.7), Vec3::new(1.9, 0.55, 4.0)],
    vectoring: false,
    rudder_brake: false,
    eye: F22_EYE,
    chase: 1.0,
    lex: true,
    vapour: 1.0,
    flight: Aircraft::fighter,
    cruise: 240.0,
};

// ---- how each jet flies (small differences between the fighters) ----

/// Light and nimble: the quickest roll, good acceleration.
fn f16() -> Aircraft {
    Aircraft { mass: 18_500.0, max_rates: Vec3::new(1.5, 0.5, 5.4), agility: 1.1, ..Aircraft::fighter() }
}

/// Happiest slow and at high AoA (the controls bite earlier, it pulls past
/// where the others stall), with the slowest top end.
fn f18() -> Aircraft {
    Aircraft {
        cd0: 0.0235,
        afterburner_thrust: 280_000.0,
        max_rates: Vec3::new(1.55, 0.55, 4.3),
        g_limit: 8.5,
        stall_alpha: 25f32.to_radians(),
        alpha_limit: 32f32.to_radians(),
        authority_speed: 135.0,
        min_speed: 60.0,
        post_stall: 0.12,
        ..Aircraft::fighter()
    }
}

/// Thrust vectoring: the most pitch, keeps pointing its nose when slow,
/// and the most thrust for the highest top speed.
fn su37() -> Aircraft {
    Aircraft {
        mass: 21_000.0,
        cd0: 0.020,
        dry_thrust: 160_000.0,
        afterburner_thrust: 315_000.0,
        max_rates: Vec3::new(1.8, 0.6, 4.5),
        g_limit: 9.5,
        stall_alpha: 22f32.to_radians(),
        alpha_limit: 28f32.to_radians(),
        authority_speed: 150.0,
        post_stall: 0.25,
        ..Aircraft::fighter()
    }
}

/// A brick: two turbofans and no afterburner (Mach 0.55 flat out), slow
/// roll, 5.5 G. A big straight wing makes it stable and forgiving slow.
fn a10() -> Aircraft {
    Aircraft {
        wing_area: 88.0,
        cl_alpha: 5.0,
        stall_alpha: 18f32.to_radians(),
        cd0: 0.038,
        induced_k: 0.1,
        dry_thrust: 68_000.0,
        afterburner_thrust: 68_000.0,
        afterburner: false,
        max_rates: Vec3::new(0.9, 0.35, 1.9),
        g_limit: 5.5,
        alpha_limit: 20f32.to_radians(),
        authority_speed: 110.0,
        min_speed: 50.0,
        agility: 0.45,
        ..Aircraft::fighter()
    }
}

static FALLBACK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The jets folder is missing (a public clone): everything flies the F-22.
pub fn fallback() -> bool {
    FALLBACK.load(Ordering::Relaxed)
}

pub fn set_fallback() {
    FALLBACK.store(true, Ordering::Relaxed);
}

const SPECS: [JetSpec; 4] = [
    JetSpec {
        key: "f16",
        name: "F-16",
        liveries: &["military", "arctic", "thunderbirds"],
        nozzles: &[Vec3::new(0.0, 0.11, 6.56)],
        nozzle_size: Vec2::splat(0.5),
        wingtip: Vec3::new(4.97, 0.03, 2.3),
        gun_port: Vec3::new(-0.75, 0.5, -4.6),
        // under-wing pylons and the wingtip rail
        hardpoints: &[Vec3::new(2.93, -0.52, 0.32), Vec3::new(4.33, -0.52, 1.29), Vec3::new(5.1, -0.05, 1.5)],
        wing: [Vec3::new(1.5, 0.3, -2.0), Vec3::new(4.5, 0.08, 0.9), Vec3::new(4.5, 0.08, 2.0), Vec3::new(1.5, 0.3, 2.6)],
        vectoring: false,
        rudder_brake: false,
        eye: Vec3::new(0.0, 0.9, -4.57),
        chase: 0.88,
        lex: true,
        vapour: 1.0,
        flight: f16,
        cruise: 240.0,
    },
    JetSpec {
        key: "f18",
        name: "F/A-18",
        liveries: &["military", "tiger"],
        nozzles: &[Vec3::new(0.55, 0.11, 7.47), Vec3::new(-0.56, 0.11, 7.47)],
        nozzle_size: Vec2::splat(0.42),
        wingtip: Vec3::new(7.24, 0.25, 2.2),
        gun_port: Vec3::new(0.0, 0.75, -10.4),
        hardpoints: &[Vec3::new(3.7, -0.08, 1.02), Vec3::new(5.22, 0.03, 1.65), Vec3::new(7.24, 0.06, 1.8)],
        wing: [Vec3::new(2.2, 0.68, -2.5), Vec3::new(6.5, 0.3, 1.6), Vec3::new(6.5, 0.3, 3.0), Vec3::new(2.2, 0.68, 3.2)],
        vectoring: false,
        rudder_brake: true,
        eye: Vec3::new(0.0, 1.455, -5.4),
        chase: 0.98,
        lex: true,
        vapour: 1.0,
        flight: f18,
        cruise: 240.0,
    },
    JetSpec {
        key: "su37",
        name: "Su-37",
        liveries: &["military"],
        nozzles: &[Vec3::new(1.29, 0.11, 9.02), Vec3::new(-1.29, 0.11, 9.02)],
        nozzle_size: Vec2::splat(0.52),
        wingtip: Vec3::new(7.94, 0.44, 4.4),
        gun_port: Vec3::new(1.1, 0.75, -5.8),
        hardpoints: &[Vec3::new(4.89, 0.02, 2.92), Vec3::new(6.62, 0.02, 3.73), Vec3::new(7.9, 0.24, 4.2)],
        wing: [Vec3::new(2.4, 1.0, -2.5), Vec3::new(7.2, 0.57, 3.0), Vec3::new(7.2, 0.57, 4.6), Vec3::new(2.4, 1.0, 5.5)],
        vectoring: true,
        rudder_brake: false,
        eye: Vec3::new(0.0, 1.645, -6.73),
        chase: 1.05,
        lex: true,
        vapour: 1.0,
        flight: su37,
        cruise: 240.0,
    },
    JetSpec {
        key: "a10",
        name: "A-10",
        liveries: &["military", "forest"],
        // the back of the engine pods. Cold turbofans: a pinprick of a
        // plume (no reheat glow), leaving the heat haze
        nozzles: &[Vec3::new(1.6, 0.68, 4.4), Vec3::new(-1.6, 0.68, 4.4)],
        nozzle_size: Vec2::splat(0.03),
        wingtip: Vec3::new(9.55, 0.03, -0.88),
        // the GAU-8's muzzle, under the nose
        gun_port: Vec3::new(-0.1, -0.5, -9.4),
        hardpoints: &[Vec3::new(3.93, -1.03, -1.57), Vec3::new(5.19, -0.86, -1.47), Vec3::new(6.48, -0.7, -1.36)],
        wing: [Vec3::new(2.9, 1.0, -3.6), Vec3::new(8.65, 0.36, -2.07), Vec3::new(8.65, 0.36, 0.35), Vec3::new(2.9, 1.0, 0.9)],
        vectoring: false,
        rudder_brake: false,
        eye: Vec3::new(0.0, 1.15, -6.13),
        chase: 1.08,
        lex: false,
        vapour: 0.45,
        flight: a10,
        cruise: 170.0,
    },
];

impl JetSpec {
    /// Where missile `slot` hangs: pairs from the pylons closest to the body
    /// outward (the stations past the last pair, wingtip rails included,
    /// stay empty), left then right. A jet fires its highest slot first
    /// (outermost first, alternating sides) and reloads from the inboard
    /// end. None past the stations, and for internal bays.
    pub fn hardpoint(&self, slot: u32) -> Option<Vec3> {
        let p = *self.hardpoints.get(slot as usize / 2)?;
        Some(if slot.is_multiple_of(2) { p * Vec3::new(-1.0, 1.0, 1.0) } else { p })
    }
}

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

    /// What the model and effects follow: the jet's spec, or the F-22's
    /// when the pack is missing.
    pub fn airframe(self) -> &'static JetSpec {
        if fallback() { &F22 } else { self.spec() }
    }

    /// How it flies (the F-22's numbers when the pack is missing: it looks like one).
    pub fn aircraft(self) -> Aircraft {
        (self.airframe().flight)()
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

/// A jet's name from its wire byte (the leaderboard's); empty if unknown.
pub fn wire_name(b: u8) -> &'static str {
    Jet::ALL.get((b >> 4) as usize).map_or("", |j| j.spec().name)
}

/// The player's pick (mirrors `Settings::jet`, for spawning without the resource).
static PLAYER: AtomicU8 = AtomicU8::new(0);

pub fn player() -> JetChoice {
    JetChoice::from_wire(PLAYER.load(Ordering::Relaxed))
}

pub fn set_player(choice: JetChoice) {
    PLAYER.store(choice.to_wire(), Ordering::Relaxed);
}

/// What the AI flies: hostiles are Su-37s with the odd A-10, wingmen F-16s
/// and F/A-18s in military grey (by `n`, so a flight is mixed).
pub fn ai_jet(hostile: bool, n: usize) -> JetChoice {
    if hostile {
        JetChoice::new(if n % 4 == 3 { Jet::A10 } else { Jet::Su37 }, 0)
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
    /// A turbofan's blades (the A-10): spin.
    Fan,
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

/// A cockpit node (see the module docs), for a first-person view to show or hide.
#[derive(Component, Clone, Copy, Debug)]
pub struct Cockpit {
    pub aircraft: Entity,
    /// Only seen from inside (hidden in third person).
    pub inside_only: bool,
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
        "fan" => PartKind::Fan,
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
        let name = name.as_str();
        let cockpit = matches!(name, "cockpit" | "hud" | "canopy" | "canopy_inner" | "cockpit_tub");
        let part = part_of(name);
        if !cockpit && part.is_none() {
            continue;
        }
        let Some(aircraft) = parents.iter_ancestors(entity).find(|a| jets.contains(*a)) else { continue };
        if let Some((kind, side)) = part {
            commands.entity(entity).insert(Part { kind, side, aircraft, base: transform.rotation });
        }
        if cockpit {
            let inside_only = name == "canopy_inner";
            commands.entity(entity).insert(Cockpit { aircraft, inside_only });
            if inside_only {
                commands.entity(entity).insert(Visibility::Hidden);
            }
        }
    }
}

/// Past this distance a jet draws its `lod1` node (one decimated mesh, ~2k
/// triangles, 1-3 draws) instead of its parts (~10k triangles, ~15 draws);
/// closer on phones. `FLIGHT_SIM_LOD_DIST=<m>` overrides it (dev).
pub fn lod_distance() -> f32 {
    if let Some(d) = std::env::var("FLIGHT_SIM_LOD_DIST").ok().and_then(|v| v.parse().ok()) {
        return d;
    }
    if crate::platform::low_memory() { 180.0 } else { 350.0 }
}

/// The cockpit's inside only draws this close (it's only seen through the canopy).
fn interior_distance() -> f32 {
    if crate::platform::low_memory() { 40.0 } else { 70.0 }
}

/// Gives every mesh of a jet's model its distance band (abrupt switches: a
/// dithered crossfade renders the jets' extended material black mid-fade).
fn set_lods(
    mut commands: Commands,
    meshes: Query<Entity, Added<Mesh3d>>,
    names: Query<&Name>,
    parents: Query<&ChildOf>,
    models: Query<(), With<crate::game::AircraftModel>>,
) {
    if fallback() {
        return;
    }
    for mesh in &meshes {
        // only the model's meshes (not the plumes, which glow from afar)
        if !parents.iter_ancestors(mesh).any(|a| models.contains(a)) {
            continue;
        }
        let mut band = None;
        for a in parents.iter_ancestors(mesh) {
            if models.contains(a) {
                break;
            }
            if band.is_none() && let Ok(name) = names.get(a) {
                band = match name.as_str() {
                    "lod1" => Some(VisibilityRange::abrupt(lod_distance(), f32::MAX)),
                    "cockpit" | "hud" | "canopy_inner" | "cockpit_tub" => Some(VisibilityRange::abrupt(0.0, interior_distance())),
                    _ => None,
                };
            }
        }
        commands.entity(mesh).insert(band.unwrap_or(VisibilityRange::abrupt(0.0, lod_distance())));
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
        PartKind::Nozzle | PartKind::Fan => 0.0,
    }
}

/// The Su-37's nozzle vector angle for the nozzle at `x` (the plumes follow it).
pub fn vector_angle(choice: JetChoice, stick: &Stick, x: f32) -> f32 {
    let spec = choice.airframe();
    if !spec.vectoring {
        return 0.0;
    }
    deflection(PartKind::Nozzle, x.signum(), stick, 0.0, spec)
}

fn animate_parts(
    time: Res<Time>,
    jets: Query<(&JetChoice, &Stick, &FlightState, Has<Disabled>)>,
    mut parts: Query<(&Part, &mut Transform)>,
) {
    for (part, mut transform) in &mut parts {
        let Ok((choice, stick, state, disabled)) = jets.get(part.aircraft) else { continue };
        if part.kind == PartKind::Fan {
            if !disabled {
                // a steady spin (a spool-scaled rate would jump as the
                // throttle moves); opposite ways, so the pair doesn't strobe in step
                let spin = time.elapsed_secs_wrapped() * 9.0 * part.side;
                transform.rotation = part.base * Quat::from_rotation_z(spin);
            }
            continue;
        }
        let spec = choice.airframe();
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
        assert_eq!(JetChoice::new(Jet::A10, 1).to_wire() >> 4, crate::net::protocol::JET_A10);
    }

    #[test]
    fn missiles_pair_up_inboard_to_tip() {
        let spec = Jet::F16.spec();
        // slots alternate left / right, outward
        let (l, r) = (spec.hardpoint(0).unwrap(), spec.hardpoint(1).unwrap());
        assert!(l.x < 0.0 && r.x > 0.0 && l.x == -r.x);
        assert!(spec.hardpoint(2).unwrap().x.abs() > spec.hardpoint(0).unwrap().x.abs());
        for jet in Jet::ALL {
            let spec = jet.spec();
            assert!(spec.hardpoints.len() * 2 >= crate::weapons::MAX_MISSILES as usize);
            // the pylons closest to the body are loaded first
            assert_eq!(spec.hardpoint(1), Some(spec.hardpoints[0]));
        }
        assert_eq!(F22.hardpoint(0), None, "internal bays");
    }

    #[test]
    fn eight_liveries_no_blue_angels() {
        assert_eq!(JetChoice::all().count(), 8);
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
