//! The tutorial: TUTORIAL on the main menu (`FLIGHT_SIM_MODE=tutorial`).
//! A fixed map (farmland, a clear morning), no intro shot, and a list of
//! lessons ([`Step`]), each a one-line prompt at the top of the HUD naming the
//! controls of whatever the player last touched (keyboard and mouse, a
//! gamepad, or touch: `controls::InputDevice`). Each step finishes when its
//! condition holds in the game (a tick, then the next), or gives up after a
//! while ([`Step::timeout`]) so nothing can get stuck; the prompt flashes
//! again now and then while it waits. SKIP in the pause menu skips a step.
//!
//! Flying first (throttle, pitch, roll, rings to fly through with
//! `waypoints`, afterburner, airbrake, a low pass down a valley), then
//! fighting: cycling targets and locking on to slow drones that circle and
//! never shoot back (they come back if they crash), a gun kill, a missile
//! kill, dropping flares against a missile fired at you (one that always
//! takes the bait), and a last dogfight against two rookies. The player can't
//! be shot down (in the dogfight hits hurt but don't kill) and is put back in
//! the air after a crash. The end screen offers ENDLESS and MENU, and the
//! finish is remembered (the menu stops lighting TUTORIAL up).
//!
//! Dev: `FLIGHT_SIM_AUTOPILOT=1` flies the lessons (an autopilot that works
//! each step's condition with the player's own controls),
//! `FLIGHT_SIM_TUTORIAL_START=<step>` starts at a step, and
//! `FLIGHT_SIM_TUTORIAL_STEP_SECS=<n>` gives every step up after n seconds.
//! Every step change is logged (`tutorial: rings -> dive (done, 41.2 s)`).

use std::sync::atomic::{AtomicBool, Ordering};

use bevy::prelude::*;
use bevy_transform_interpolation::prelude::*;

use crate::{
    ai::Pilot,
    controls::{ControlMode, InputDevice, MouseAim, PlayerActions, ThrottleLever},
    environment::{Biome, Picks, TimeOfDay, Weather},
    flight::{Aircraft, ControlInput, Disabled, FlightState, FlightSystems, steer_towards},
    game::{Health, Models, Player, RoundSystems, Team, spawn_aircraft},
    loading::GameState,
    menu::{Pressed, Screen, UiAction, button},
    modes::GameMode,
    pause::Paused,
    waypoints::{Course, Gate},
    weapons::{Flare, Missile, ProjectileAssets, Seeker, Targeting, Weapons, gun_lead_point, spawn_missile},
    world::Terrain,
};

/// The tutorial's map.
pub const SEED: u32 = 4242;

/// Farmland on a clear morning: gentle hills and a soft light.
pub fn picks() -> Picks {
    Picks { biome: Some(Biome::Farmland), time: Some(TimeOfDay::Morning), weather: Some(Weather::Clear) }
}

pub struct TutorialPlugin;

impl Plugin for TutorialPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(Tutorial::new(Step::FIRST))
            .add_systems(Startup, spawn_ui)
            .add_systems(OnEnter(GameState::Playing), start.run_if(is_tutorial))
            .add_systems(OnExit(GameState::Playing), stop)
            .add_systems(FixedUpdate, fly_drones.in_set(FlightSystems::Pilots).run_if(active))
            .add_systems(
                FixedUpdate,
                (autopilot.run_if(autopilot_on), gate_inputs)
                    .chain()
                    .after(FlightSystems::Pilots)
                    .before(FlightSystems::Physics)
                    .run_if(active),
            )
            .add_systems(FixedUpdate, protect_player.after(FlightSystems::Combat).before(RoundSystems).run_if(active))
            .add_systems(
                Update,
                (track_targets, set_up_step, flare_drill, run_steps, keep_flying).chain().run_if(active),
            )
            .add_systems(Update, update_ui);
    }
}

fn is_tutorial(mode: Res<GameMode>) -> bool {
    *mode == GameMode::Tutorial
}

fn active(mode: Res<GameMode>, state: Res<State<GameState>>) -> bool {
    *mode == GameMode::Tutorial && *state.get() == GameState::Playing
}

fn autopilot_on() -> bool {
    std::env::var("FLIGHT_SIM_AUTOPILOT").is_ok()
}

// ---------------------------------------------------------------------------------------------
// the steps

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Step {
    Throttle,
    Pitch,
    Roll,
    /// Four rings round an easy right-hand curve.
    Rings,
    /// A ring well below.
    Dive,
    /// A ring well above.
    Climb,
    Afterburner,
    Airbrake,
    /// Throttle back up after the airbrake.
    Cruise,
    /// A low ring in the nearest valley.
    Valley,
    Targets,
    Lock,
    GunKill,
    MissileKill,
    Flares,
    Dogfight,
    Complete,
}

impl Step {
    pub const ALL: [Step; 17] = [
        Step::Throttle,
        Step::Pitch,
        Step::Roll,
        Step::Rings,
        Step::Dive,
        Step::Climb,
        Step::Afterburner,
        Step::Airbrake,
        Step::Cruise,
        Step::Valley,
        Step::Targets,
        Step::Lock,
        Step::GunKill,
        Step::MissileKill,
        Step::Flares,
        Step::Dogfight,
        Step::Complete,
    ];
    pub const FIRST: Step = Step::Throttle;

    pub fn index(self) -> usize {
        Self::ALL.iter().position(|s| *s == self).unwrap_or(0)
    }

    pub fn next(self) -> Option<Step> {
        Self::ALL.get(self.index() + 1).copied()
    }

    pub fn name(self) -> &'static str {
        match self {
            Step::Throttle => "throttle",
            Step::Pitch => "pitch",
            Step::Roll => "roll",
            Step::Rings => "rings",
            Step::Dive => "dive",
            Step::Climb => "climb",
            Step::Afterburner => "afterburner",
            Step::Airbrake => "airbrake",
            Step::Cruise => "cruise",
            Step::Valley => "valley",
            Step::Targets => "targets",
            Step::Lock => "lock",
            Step::GunKill => "gun",
            Step::MissileKill => "missile",
            Step::Flares => "flares",
            Step::Dogfight => "dogfight",
            Step::Complete => "complete",
        }
    }

    pub fn parse(s: &str) -> Option<Step> {
        Self::ALL.into_iter().find(|step| step.name().eq_ignore_ascii_case(s.trim()))
    }

    /// Seconds before the step gives up and moves on, so nothing can get stuck.
    pub fn timeout(self) -> f32 {
        match self {
            Step::Throttle | Step::Pitch | Step::Roll | Step::Afterburner | Step::Airbrake | Step::Cruise => 40.0,
            Step::Rings => 150.0,
            Step::Dive | Step::Climb | Step::Valley => 75.0,
            Step::Targets => 60.0,
            Step::Lock => 90.0,
            Step::GunKill => 180.0,
            Step::MissileKill => 150.0,
            Step::Flares => 120.0,
            Step::Dogfight => 300.0,
            Step::Complete => f32::INFINITY,
        }
    }

    /// Seconds before the prompt starts flashing again as a reminder.
    pub fn hint_after(self) -> f32 {
        (self.timeout() * 0.3).min(45.0)
    }

    /// Steps with gates to fly through.
    fn has_gates(self) -> bool {
        matches!(self, Step::Rings | Step::Dive | Step::Climb | Step::Valley)
    }

    /// Drones to keep in the air (they come back if they crash).
    fn drones(self) -> usize {
        match self {
            Step::Targets | Step::Lock => 2,
            Step::GunKill | Step::MissileKill => 1,
            _ => 0,
        }
    }

    /// A drone's health in this step: a few gun hits, one good missile.
    fn drone_health(self) -> f32 {
        match self {
            Step::GunKill => 35.0,
            Step::MissileKill => 60.0,
            _ => 100.0,
        }
    }

    /// Steps counted on the HUD (all but the end).
    pub const COUNTED: usize = Self::ALL.len() - 1;
}

/// The prompt for a step, naming the controls of `device` (`direct`: the
/// keyboard flies direct rather than mouse aim).
pub fn prompt(step: Step, device: InputDevice, direct: bool) -> &'static str {
    use InputDevice::*;
    match (step, device) {
        (Step::Throttle, Keyboard) => "Throttle up \u{2014} hold Shift",
        (Step::Throttle, Gamepad) => "Throttle up \u{2014} squeeze RT",
        (Step::Throttle, Touch) => "Throttle up \u{2014} slide the right edge up",
        (Step::Pitch, Keyboard) if direct => "Pitch up \u{2014} hold S",
        (Step::Pitch, Keyboard) => "Pitch up \u{2014} move the mouse up",
        (Step::Pitch, Gamepad) => "Pitch up \u{2014} pull back on the left stick",
        (Step::Pitch, Touch) => "Pitch up \u{2014} swipe up",
        (Step::Roll, Keyboard) => "Roll \u{2014} hold A or D",
        (Step::Roll, Gamepad) => "Roll \u{2014} push the left stick sideways",
        (Step::Roll, Touch) => "Bank \u{2014} swipe sideways",
        (Step::Rings, Keyboard) if direct => "Fly through the rings \u{2014} roll with A / D, pull with S",
        (Step::Rings, Keyboard) => "Fly through the rings \u{2014} roll with A / D, pull with the mouse",
        (Step::Rings, Gamepad) => "Fly through the rings \u{2014} roll and pull with the left stick",
        (Step::Rings, Touch) => "Fly through the rings \u{2014} swipe toward them",
        (Step::Dive, Keyboard) if direct => "Dive through the low ring \u{2014} hold W",
        (Step::Dive, Keyboard) => "Dive through the low ring \u{2014} move the mouse down",
        (Step::Dive, Gamepad) => "Dive through the low ring \u{2014} push the left stick forward",
        (Step::Dive, Touch) => "Dive through the low ring \u{2014} swipe down",
        (Step::Climb, Keyboard) if direct => "Climb through the high ring \u{2014} hold S",
        (Step::Climb, Keyboard) => "Climb through the high ring \u{2014} move the mouse up",
        (Step::Climb, Gamepad) => "Climb through the high ring \u{2014} pull back on the left stick",
        (Step::Climb, Touch) => "Climb through the high ring \u{2014} swipe up",
        (Step::Afterburner, Keyboard) => "Afterburner \u{2014} keep holding Shift at full throttle",
        (Step::Afterburner, Gamepad) => "Afterburner \u{2014} squeeze RT all the way",
        (Step::Afterburner, Touch) => "Afterburner \u{2014} slide the throttle to the top",
        (Step::Airbrake, Keyboard) => "Airbrake \u{2014} hold Ctrl until idle and beyond",
        (Step::Airbrake, Gamepad) => "Airbrake \u{2014} squeeze LT all the way",
        (Step::Airbrake, Touch) => "Airbrake \u{2014} slide the throttle to the bottom",
        (Step::Cruise, Keyboard) => "Throttle back up \u{2014} hold Shift",
        (Step::Cruise, Gamepad) => "Throttle back up \u{2014} squeeze RT",
        (Step::Cruise, Touch) => "Throttle back up \u{2014} slide the right edge up",
        (Step::Valley, _) => "Low pass \u{2014} follow the arrow down into the valley ring",
        (Step::Targets, Keyboard) => "Cycle targets \u{2014} press Tab",
        (Step::Targets, Gamepad) => "Cycle targets \u{2014} press D-pad right",
        (Step::Targets, Touch) => "Pick a target \u{2014} tap TGT",
        (Step::Lock, _) => "Lock on \u{2014} hold the target box near your nose",
        (Step::GunKill, Keyboard) if direct => "Gun kill \u{2014} put the cross on the pipper, hold Space",
        (Step::GunKill, Keyboard) => "Gun kill \u{2014} put the cross on the pipper, hold left click",
        (Step::GunKill, Gamepad) => "Gun kill \u{2014} put the cross on the pipper, hold A",
        (Step::GunKill, Touch) => "Gun kill \u{2014} put the cross on the pipper, hold GUN",
        (Step::MissileKill, Keyboard) if direct => "Missile kill \u{2014} lock on, then press F",
        (Step::MissileKill, Keyboard) => "Missile kill \u{2014} lock on, then right click",
        (Step::MissileKill, Gamepad) => "Missile kill \u{2014} lock on, then press B",
        (Step::MissileKill, Touch) => "Missile kill \u{2014} lock on, then tap MSL",
        (Step::Flares, Touch) => "Missile! Drop flares \u{2014} hold FLR",
        (Step::Flares, _) => "Missile! Drop flares \u{2014} hold X",
        (Step::Dogfight, _) => "Dogfight \u{2014} shoot down both",
        (Step::Complete, _) => "",
    }
}

/// Why a step ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Reason {
    Done,
    Skipped,
    TimedOut,
}

/// How long the tick shows before the next step.
pub const TICK_SECS: f32 = 1.2;
/// While waiting past `hint_after`, the prompt flashes for this long...
const HINT_FLASH: f32 = 1.5;
/// ...every this many seconds.
const HINT_EVERY: f32 = 12.0;

/// Where the lessons are: the step, how long it's been up, and whether it's
/// done (showing its tick).
#[derive(Clone, Debug, PartialEq)]
pub struct Progress {
    pub step: Step,
    /// Seconds in this step.
    pub t: f32,
    /// Seconds since it was done, and why.
    pub done: Option<(f32, Reason)>,
}

impl Progress {
    pub fn new(step: Step) -> Self {
        Progress { step, t: 0.0, done: None }
    }

    /// One update. `met`: the step's condition holds; `skip`: SKIP was
    /// pressed; `timeout`: give up after this long. Returns (from, to, why)
    /// when it moves on to the next step.
    pub fn tick(&mut self, dt: f32, met: bool, skip: bool, timeout: f32) -> Option<(Step, Step, Reason)> {
        if self.step == Step::Complete {
            return None;
        }
        self.t += dt;
        if skip {
            return Some(self.advance(Reason::Skipped));
        }
        match &mut self.done {
            None => {
                if met {
                    self.done = Some((0.0, Reason::Done));
                } else if self.t >= timeout {
                    self.done = Some((0.0, Reason::TimedOut));
                }
                None
            }
            Some((since, reason)) => {
                *since += dt;
                if *since >= TICK_SECS {
                    let reason = *reason;
                    return Some(self.advance(reason));
                }
                None
            }
        }
    }

    fn advance(&mut self, reason: Reason) -> (Step, Step, Reason) {
        let from = self.step;
        let to = from.next().unwrap_or(Step::Complete);
        *self = Progress::new(to);
        (from, to, reason)
    }

    /// The prompt should flash as a reminder now.
    pub fn hinting(&self) -> bool {
        let since = self.t - self.step.hint_after();
        self.done.is_none() && since >= 0.0 && since % HINT_EVERY < HINT_FLASH
    }
}

/// The tutorial's state for the round.
#[derive(Resource)]
pub struct Tutorial {
    progress: Progress,
    /// The current step's world (gates, targets) is set up.
    ready: bool,
    /// Seconds the current step's control has been held (throttle, afterburner...).
    held: f32,
    /// Target presses this step.
    presses: u32,
    /// Drones shot down this step.
    kills: u32,
    drones: Vec<Entity>,
    /// Drones the player has hit (a crash afterwards still counts as theirs).
    hit_by_player: Vec<Entity>,
    drone_in: f32,
    rookies: Vec<Entity>,
    /// The flare drill: the jet that fires at you, its missile, the countdown to the next.
    launcher: Option<Entity>,
    missile: Option<Entity>,
    launch_in: f32,
    /// The player took a hit this step (the flare drill goes again).
    player_hit: bool,
    drill_done: bool,
    /// A word under the prompt ("DECOYED"), for a few seconds.
    status: Option<(String, f32)>,
    /// Somewhere safe to put the player back after a crash (position, heading).
    safe: Option<(Vec3, Vec3)>,
    down_for: f32,
    skip: bool,
}

impl Tutorial {
    fn new(step: Step) -> Self {
        Tutorial {
            progress: Progress::new(step),
            ready: false,
            held: 0.0,
            presses: 0,
            kills: 0,
            drones: Vec::new(),
            hit_by_player: Vec::new(),
            drone_in: 0.0,
            rookies: Vec::new(),
            launcher: None,
            missile: None,
            launch_in: 0.0,
            player_hit: false,
            drill_done: false,
            status: None,
            safe: None,
            down_for: 0.0,
            skip: false,
        }
    }

    pub fn step(&self) -> Step {
        self.progress.step
    }
}

/// The end screen is up (its buttons take the keyboard / gamepad, the cursor is free).
static COMPLETE_SCREEN: AtomicBool = AtomicBool::new(false);

pub fn complete_screen() -> bool {
    COMPLETE_SCREEN.load(Ordering::Relaxed)
}

fn start_step() -> Step {
    crate::environment::setting("FLIGHT_SIM_TUTORIAL_START").and_then(|s| Step::parse(&s)).unwrap_or(Step::FIRST)
}

fn start(mut tutorial: ResMut<Tutorial>, mut intro: ResMut<crate::intro::Intro>, mut lever: ResMut<ThrottleLever>) {
    *tutorial = Tutorial::new(start_step());
    // no opening shot: straight to the first lesson, at half throttle
    intro.t = crate::intro::INTRO_LEN;
    lever.0 = 0.5;
    info!("tutorial: starting at {}", tutorial.step().name());
}

fn stop(mut tutorial: ResMut<Tutorial>) {
    *tutorial = Tutorial::new(Step::FIRST);
    COMPLETE_SCREEN.store(false, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------------------------
// setting up each step

/// A slow drone that flies a wide circle and never fights.
#[derive(Component)]
struct Drone {
    centre: Vec2,
    radius: f32,
    /// +1 / -1: which way round.
    turn: f32,
    altitude: f32,
}

const DRONE_SPEED: f32 = 175.0;
const DRONE_CIRCLE: f32 = 3_500.0;

fn flat(v: Vec3) -> Vec3 {
    Vec3::new(v.x, 0.0, v.z).normalize_or(Vec3::NEG_Z)
}

/// The tangent of a circle at `r_hat` (unit, from the centre), going round the `turn` way.
fn tangent(r_hat: Vec2, turn: f32) -> Vec2 {
    Vec2::new(-r_hat.y, r_hat.x) * turn
}

/// A drone `ahead` metres in front of `pos` (offset `side` metres to the
/// right), flying along `heading` round a circle of `radius`.
#[allow(clippy::too_many_arguments)]
fn spawn_drone(
    commands: &mut Commands,
    models: &Models,
    terrain: &Terrain,
    pos: Vec3,
    heading: Vec3,
    ahead: f32,
    side: f32,
    radius: f32,
    health: f32,
) -> Entity {
    let h = flat(heading);
    let right = h.cross(Vec3::Y);
    let mut at = pos + h * ahead + right * side;
    at.y = at.y.max(terrain.max_height_along(at, at + h * 4_000.0) + 500.0);
    // the circle's centre off to the right, so we start along `heading`
    let centre = at + right * radius;
    let r_hat = (at - centre).xz().normalize_or(Vec2::X);
    let turn = if tangent(r_hat, 1.0).dot(h.xz()) >= 0.0 { 1.0 } else { -1.0 };
    let e = spawn_aircraft(commands, models, terrain, Team::Red, at, h, false);
    commands.entity(e).remove::<Pilot>().insert((
        Name::new("Drone"),
        Drone { centre: centre.xz(), radius, turn, altitude: at.y },
        Health::new(health),
    ));
    e
}

/// Four rings round an easy right-hand curve, starting ahead of `pos`.
pub fn ring_course(terrain: &Terrain, pos: Vec3, heading: Vec3) -> Vec<Gate> {
    let mut gates = Vec::new();
    let mut prev = pos;
    let mut dir = flat(heading);
    for i in 0..4 {
        if i > 0 {
            // (negative about +Y: to the right)
            dir = Quat::from_rotation_y(-32f32.to_radians()) * dir;
        }
        let mut p = prev + dir * 1_800.0;
        p.y = clear_of_ground(terrain, prev, p, pos.y);
        gates.push(Gate::new(p, p - prev, 70.0));
        prev = p;
    }
    gates
}

/// `want`, or higher if the ground along `from` -> `to` needs it.
fn clear_of_ground(terrain: &Terrain, from: Vec3, to: Vec3, want: f32) -> f32 {
    let ground = terrain.max_height_along(from, to).max(terrain.height_at(to.x, to.z));
    want.max(ground + 250.0)
}

/// One ring `dist` ahead at `want` metres (lifted clear of the ground).
fn ring_ahead(terrain: &Terrain, pos: Vec3, heading: Vec3, dist: f32, want: f32) -> Gate {
    let mut p = pos + flat(heading) * dist;
    p.y = clear_of_ground(terrain, pos, p, want);
    Gate::new(p, p - pos, 75.0)
}

/// A low ring in the deepest bit of ground a few km ahead.
fn valley_ring(terrain: &Terrain, pos: Vec3, heading: Vec3) -> Gate {
    let h = flat(heading);
    let right = h.cross(Vec3::Y);
    let mut best = (f32::MAX, pos + h * 3_500.0);
    for ahead in [3_000.0, 3_800.0, 4_600.0] {
        for across in -3..=3 {
            let p = pos + h * ahead + right * (across as f32 * 700.0);
            let g = terrain.height_at(p.x, p.z);
            if g < best.0 {
                best = (g, p);
            }
        }
    }
    let mut p = best.1;
    // clear of the valley sides close by
    let sides = [Vec3::X, Vec3::NEG_X, Vec3::Z, Vec3::NEG_Z]
        .map(|d| terrain.height_at(p.x + d.x * 150.0, p.z + d.z * 150.0))
        .into_iter()
        .fold(best.0, f32::max);
    p.y = sides + 130.0;
    Gate::new(p, flat(p - pos), 100.0)
}

#[allow(clippy::too_many_arguments)]
fn set_up_step(
    mut commands: Commands,
    models: Res<Models>,
    terrain: Res<Terrain>,
    mut tutorial: ResMut<Tutorial>,
    mut course: ResMut<Course>,
    mut player: Query<(&Transform, &mut Weapons), With<Player>>,
    mut healths: Query<&mut Health, Without<Player>>,
    mut settings: ResMut<crate::settings::Settings>,
) {
    let tutorial = &mut *tutorial;
    if tutorial.ready {
        return;
    }
    let step = tutorial.step();
    // (everything is placed around the player: wait for them to be flying)
    let Ok((t, mut weapons)) = player.single_mut() else { return };
    let (pos, heading) = (t.translation, t.forward().as_vec3());
    tutorial.ready = true;
    if !step.has_gates() {
        course.clear();
    }
    match step {
        Step::Rings => course.set(ring_course(&terrain, pos, heading)),
        Step::Dive => course.set(vec![ring_ahead(&terrain, pos, heading, 2_400.0, pos.y - 450.0)]),
        Step::Climb => course.set(vec![ring_ahead(&terrain, pos, heading, 2_400.0, pos.y + 550.0)]),
        Step::Valley => course.set(vec![valley_ring(&terrain, pos, heading)]),
        Step::Targets | Step::Lock | Step::GunKill | Step::MissileKill => {
            // the leftovers of an earlier step go
            for e in tutorial.rookies.drain(..).chain(tutorial.launcher.take()) {
                commands.entity(e).despawn();
            }
            let hp = step.drone_health();
            for &e in &tutorial.drones {
                if let Ok(mut h) = healths.get_mut(e) {
                    *h = Health::new(hp);
                }
            }
            for i in tutorial.drones.len()..step.drones() {
                let e = spawn_drone(&mut commands, &models, &terrain, pos, heading, 2_000.0, i as f32 * 500.0 - 250.0, DRONE_CIRCLE, hp);
                tutorial.drones.push(e);
            }
        }
        Step::Flares => {
            for e in tutorial.drones.drain(..).chain(tutorial.rookies.drain(..)) {
                commands.entity(e).despawn();
            }
            weapons.refill_flares();
            tutorial.launch_in = 3.0;
        }
        Step::Dogfight => {
            for e in tutorial.drones.drain(..).chain(tutorial.launcher.take()) {
                commands.entity(e).despawn();
            }
            let h = flat(heading);
            for side in [-300.0, 300.0] {
                let at = pos + h * 6_000.0 + h.cross(Vec3::Y) * side;
                let e = spawn_aircraft(&mut commands, &models, &terrain, Team::Red, at, -h, false);
                let mut pilot = Pilot::new();
                // below the softest wave pilot (0.5): wobblier aim, slower to react
                pilot.skill = 0.35;
                commands.entity(e).insert((Name::new("Rookie"), pilot));
                tutorial.rookies.push(e);
            }
        }
        Step::Complete => {
            // (a rookie still about would keep shooting behind the end screen)
            for e in tutorial.drones.drain(..).chain(tutorial.rookies.drain(..)).chain(tutorial.launcher.take()) {
                commands.entity(e).despawn();
            }
            if !settings.tutorial_done {
                settings.tutorial_done = true;
                crate::settings::save_tutorial_done();
            }
        }
        _ => {}
    }
}

/// Counts drone kills, keeps enough drones up, and drops the rookies as they go down.
#[allow(clippy::too_many_arguments)]
fn track_targets(
    mut commands: Commands,
    time: Res<Time>,
    models: Res<Models>,
    terrain: Res<Terrain>,
    mut tutorial: ResMut<Tutorial>,
    targets: Query<(&Health, Has<Disabled>), Without<Player>>,
    player: Query<(Entity, &Transform), With<Player>>,
) {
    let me = player.single().ok();
    let tutorial = &mut *tutorial;
    let mut kills = 0;
    tutorial.drones.retain(|&e| {
        let (gone, disabled) = match targets.get(e) {
            Ok((health, disabled)) => {
                if health.last_attacker.is_some_and(|a| me.is_some_and(|(p, _)| p == a))
                    && !tutorial.hit_by_player.contains(&e)
                {
                    tutorial.hit_by_player.push(e);
                }
                (false, disabled)
            }
            Err(_) => (true, false),
        };
        if gone || disabled {
            if tutorial.hit_by_player.contains(&e) {
                kills += 1;
            } else {
                info!("tutorial: a drone crashed (it comes back)");
            }
            return false;
        }
        true
    });
    tutorial.kills += kills;
    tutorial.rookies.retain(|&e| targets.get(e).is_ok_and(|(_, disabled)| !disabled));

    // keep enough drones up (they come back a little ahead of us)
    let want = if tutorial.ready { tutorial.step().drones() } else { 0 };
    if tutorial.drones.len() >= want {
        tutorial.drone_in = 2.5;
        return;
    }
    tutorial.drone_in -= time.delta_secs();
    if tutorial.drone_in <= 0.0
        && let Some((_, t)) = me
    {
        tutorial.drone_in = 2.5;
        let hp = tutorial.step().drone_health();
        let e = spawn_drone(&mut commands, &models, &terrain, t.translation, t.forward().as_vec3(), 2_000.0, 0.0, DRONE_CIRCLE, hp);
        tutorial.drones.push(e);
    }
}

/// Drones fly their circles, slowly, keeping clear of the hills.
fn fly_drones(terrain: Res<Terrain>, mut drones: Query<(&Drone, &Transform, &FlightState, &mut ControlInput), Without<Disabled>>) {
    for (drone, t, state, mut input) in &mut drones {
        let pos = t.translation;
        let rel = pos.xz() - drone.centre;
        let d = rel.length().max(1.0);
        let r_hat = rel / d;
        // round the circle, easing back onto it
        let radial = r_hat * ((drone.radius - d) / drone.radius * 3.0).clamp(-1.0, 1.0);
        let along = (tangent(r_hat, drone.turn) + radial).normalize_or(Vec2::NEG_Y);
        let ground = terrain.max_height_along(pos, pos + state.velocity * 8.0).max(terrain.height_at(pos.x, pos.z));
        let want_y = drone.altitude.max(ground + 450.0);
        let climb = ((want_y - pos.y) / 800.0).clamp(-0.25, 0.6);
        let desired = Vec3::new(along.x, climb, along.y).normalize();
        let (pitch, roll, yaw) = steer_towards(t, state, desired, 0.7);
        input.pitch = pitch;
        input.roll = roll;
        input.yaw = yaw;
        input.throttle = (0.55 + (DRONE_SPEED - state.airspeed) / 60.0).clamp(0.15, 1.0);
        input.afterburner = false;
        input.airbrake = false;
    }
}

/// The flare drill: a jet behind fires one missile at the player (one that
/// always goes for a flare). Decoyed, or missed, and it's done; a hit and it
/// goes again.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn flare_drill(
    mut commands: Commands,
    time: Res<Time>,
    models: Res<Models>,
    terrain: Res<Terrain>,
    assets: Res<ProjectileAssets>,
    mut tutorial: ResMut<Tutorial>,
    mut player: Query<(Entity, &Transform, &mut Weapons), With<Player>>,
    jets: Query<(&Transform, &FlightState), (With<Aircraft>, Without<Player>, Without<Disabled>)>,
    missiles: Query<&Missile>,
    flares: Query<(), With<Flare>>,
) {
    if tutorial.step() != Step::Flares || !tutorial.ready || tutorial.drill_done {
        return;
    }
    let Ok((me, t, mut weapons)) = player.single_mut() else { return };
    if let Some(m) = tutorial.missile {
        match missiles.get(m) {
            Ok(missile) => {
                if missile.target.is_some_and(|f| flares.contains(f)) {
                    info!("tutorial: missile decoyed");
                    tutorial.drill_done = true;
                    tutorial.status = Some(("DECOYED".into(), 3.0));
                }
            }
            Err(_) => {
                tutorial.missile = None;
                if std::mem::take(&mut tutorial.player_hit) {
                    info!("tutorial: missile hit, again");
                    tutorial.status = Some(("HIT \u{2014} AGAIN".into(), 3.0));
                    tutorial.launch_in = 3.5;
                } else {
                    info!("tutorial: missile missed");
                    tutorial.drill_done = true;
                    tutorial.status = Some(("MISSED".into(), 3.0));
                }
            }
        }
        return;
    }
    // the launcher sits a few km behind and off to one side
    let h = flat(t.forward().as_vec3());
    let launcher = tutorial.launcher.and_then(|e| jets.get(e).ok().map(|j| (e, j)));
    let Some((launcher, (lt, ls))) = launcher else {
        let e = spawn_drone(&mut commands, &models, &terrain, t.translation, h, -3_500.0, -500.0, 100_000.0, 100.0);
        tutorial.launcher = Some(e);
        return;
    };
    tutorial.launch_in -= time.delta_secs();
    if tutorial.launch_in > 0.0 {
        return;
    }
    weapons.refill_flares();
    let to_player = (t.translation - lt.translation).normalize_or(lt.forward().as_vec3());
    let velocity = ls.velocity.length().max(150.0) * to_player;
    let missile = Missile {
        velocity,
        target: Some(me),
        owner: launcher,
        team: Team::Red,
        age: 0.0,
        lateral: Vec3::ZERO,
        last_target_velocity: None,
        target_accel: Vec3::ZERO,
        // gentler, and it always takes the bait: this is a lesson
        seeker: Seeker { max_g: 28.0, decoy_odds: 1.0, ..Seeker::NORMAL },
    };
    let pos = lt.translation + to_player * 12.0;
    let rotation = Transform::IDENTITY.looking_to(velocity, Vec3::Y).rotation * Quat::from_rotation_x(std::f32::consts::FRAC_PI_2);
    let e = spawn_missile(&mut commands, &assets, missile, Transform::from_translation(pos).with_rotation(rotation));
    commands.entity(e).insert(TransformInterpolation);
    tutorial.missile = Some(e);
    info!("tutorial: missile away");
}

/// The player can't be shot down: hits heal at once (in the dogfight they
/// hurt, but stop at a quarter and heal back).
fn protect_player(time: Res<Time>, mut tutorial: ResMut<Tutorial>, mut player: Query<&mut Health, With<Player>>) {
    let dogfight = tutorial.step() == Step::Dogfight;
    for mut health in &mut player {
        if health.current < health.max - 0.01 && tutorial.step() == Step::Flares && !tutorial.player_hit {
            tutorial.player_hit = true;
        }
        health.current = if dogfight {
            (health.current.max(health.max * 0.25) + 4.0 * time.delta_secs()).min(health.max)
        } else {
            health.max
        };
    }
}

/// After a crash, back in the air within a moment and pointed at the next
/// gate (no RESPAWN button).
#[allow(clippy::too_many_arguments)]
fn keep_flying(
    mut commands: Commands,
    time: Res<Time>,
    models: Res<Models>,
    terrain: Res<Terrain>,
    mut tutorial: ResMut<Tutorial>,
    mut course: ResMut<Course>,
    mut aim: ResMut<MouseAim>,
    player: Query<(&Transform, &FlightState), With<Player>>,
) {
    if let Ok((t, state)) = player.single() {
        tutorial.down_for = 0.0;
        let pos = t.translation;
        if pos.y - terrain.height_at(pos.x, pos.z) > 600.0 && state.airspeed > 150.0 {
            tutorial.safe = Some((pos, flat(t.forward().as_vec3())));
        }
        return;
    }
    tutorial.down_for += time.delta_secs();
    if tutorial.down_for < 1.5 {
        return;
    }
    tutorial.down_for = 0.0;
    let (pos, heading) = tutorial.safe.unwrap_or((Vec3::new(0.0, 1_500.0, 6_000.0), Vec3::NEG_Z));
    let heading = course.next_gate().map_or(heading, |g| flat(g.pos - pos));
    info!("tutorial: back in the air at {pos:.0}");
    spawn_aircraft(&mut commands, &models, &terrain, Team::Blue, pos, heading, true);
    aim.reset = true;
    course.reset_tracking();
}

/// Holds weapons back while they'd spoil a lesson, and counts target presses.
fn gate_inputs(mut actions: ResMut<PlayerActions>, mut tutorial: ResMut<Tutorial>) {
    let step = tutorial.step();
    if matches!(step, Step::Targets | Step::Lock | Step::MissileKill) {
        actions.firing_gun = false;
    }
    if matches!(step, Step::Targets | Step::Lock | Step::GunKill) {
        actions.fire_missile = false;
    }
    if step == Step::Targets && (actions.next_target || actions.previous_target || actions.target_under_crosshair) {
        tutorial.presses += 1;
    }
}

/// Checks the step's condition and moves the lessons on.
#[allow(clippy::too_many_arguments)]
fn run_steps(
    time: Res<Time>,
    mut tutorial: ResMut<Tutorial>,
    course: Res<Course>,
    device: Res<InputDevice>,
    player: Query<(&Transform, &FlightState, &ControlInput, &Targeting), With<Player>>,
    mut passed: MessageReader<crate::waypoints::GatePassed>,
    mut fast: Local<Option<Option<f32>>>,
) {
    let dt = time.delta_secs();
    // "2 / 4" as each ring of a course goes by
    if let Some(p) = passed.read().last()
        && course.gates().len() > 1
        && !p.last
    {
        tutorial.status = Some((format!("{} / {}", p.index + 1, course.gates().len()), 2.0));
    }
    let step = tutorial.step();
    let fast = *fast.get_or_insert_with(|| std::env::var("FLIGHT_SIM_TUTORIAL_STEP_SECS").ok().and_then(|s| s.parse().ok()));
    if let Some((_, left)) = &mut tutorial.status {
        *left -= dt;
        if *left <= 0.0 {
            tutorial.status = None;
        }
    }
    let (presses, kills, drill_done, no_rookies) =
        (tutorial.presses, tutorial.kills, tutorial.drill_done, tutorial.rookies.is_empty());
    let mut held_for = tutorial.held;
    let met = tutorial.ready
        && player.single().is_ok_and(|(t, state, input, targeting)| {
            let mut held = |on: bool, secs: f32| {
                if on {
                    held_for += dt;
                }
                held_for >= secs
            };
            match step {
                Step::Throttle => held(input.throttle >= 0.95, 0.4),
                Step::Pitch => t.forward().y > 12f32.to_radians().sin(),
                Step::Roll => t.right().y.abs() > 45f32.to_radians().sin(),
                Step::Rings | Step::Dive | Step::Climb | Step::Valley => course.finished(),
                Step::Afterburner => held(state.afterburner_on, 2.0),
                Step::Airbrake => held(input.airbrake, 1.5),
                Step::Cruise => held(input.throttle >= 0.7 && !input.airbrake, 0.3),
                // (TGT on touch picks the nearest: once is enough)
                Step::Targets => presses >= if *device == InputDevice::Touch { 1 } else { 2 },
                Step::Lock => targeting.locked() && targeting.target.is_some_and(|e| tutorial.drones.contains(&e)),
                Step::GunKill | Step::MissileKill => kills >= 1,
                Step::Flares => drill_done,
                Step::Dogfight => no_rookies,
                Step::Complete => false,
            }
        });
    tutorial.held = held_for;
    let skip = std::mem::take(&mut tutorial.skip);
    let timeout = fast.unwrap_or(step.timeout()).min(step.timeout());
    let secs = tutorial.progress.t + dt;
    if let Some((from, to, reason)) = tutorial.progress.tick(dt, met, skip, timeout) {
        info!("tutorial: {} -> {} ({reason:?}, {secs:.1} s)", from.name(), to.name());
        let t = &mut *tutorial;
        t.ready = false;
        t.held = 0.0;
        t.presses = 0;
        t.kills = 0;
        t.player_hit = false;
        t.drill_done = false;
        t.missile = None;
    }
}

// ---------------------------------------------------------------------------------------------
// the dev autopilot

/// `FLIGHT_SIM_AUTOPILOT=1` in the tutorial: flies each lesson with the
/// player's own controls (inputs and `PlayerActions`), so a headless run
/// checks every condition for real.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn autopilot(
    time: Res<Time>,
    terrain: Res<Terrain>,
    tutorial: Res<Tutorial>,
    course: Res<Course>,
    mut actions: ResMut<PlayerActions>,
    mut player: Query<(Entity, &Transform, &FlightState, &mut ControlInput, &Targeting), With<Player>>,
    others: Query<(Entity, &Transform, &FlightState, &Team), (With<Aircraft>, Without<Player>, Without<Disabled>)>,
    missiles: Query<(&Missile, &Transform)>,
    mut timer: Local<f32>,
    mut missile_wait: Local<f32>,
) {
    let Ok((me, t, state, mut input, targeting)) = player.single_mut() else { return };
    let dt = time.delta_secs();
    *timer += dt;
    *missile_wait -= dt;
    let pos = t.translation;
    let fwd = t.forward().as_vec3();
    let level = flat(fwd);
    let (mut throttle, mut afterburner, mut airbrake) = (1.0, false, false);
    let mut desired = level;
    let mut roll = None;
    let mut attack = |desired: &mut Vec3, throttle: &mut f32, afterburner: &mut bool| {
        let target = targeting
            .target
            .and_then(|e| others.get(e).ok())
            .or_else(|| others.iter().filter(|o| *o.3 == Team::Red).min_by(|a, b| a.1.translation.distance(pos).total_cmp(&b.1.translation.distance(pos))));
        let Some((_, tt, ts, _)) = target else { return };
        let to = tt.translation - pos;
        let dist = to.length();
        let lead = gun_lead_point(pos, state.velocity, tt.translation, ts.velocity);
        let lead_dir = (lead - pos).normalize_or(fwd);
        *desired = if dist < 1_500.0 { lead_dir } else { to / dist.max(1.0) };
        if dist > 1_500.0 {
            *afterburner = state.airspeed < 330.0;
        } else {
            let closure = (state.velocity - ts.velocity).dot(to / dist.max(1.0));
            *throttle = (0.75 - closure / 150.0).clamp(0.3, 1.0);
        }
        if state.airspeed < 190.0 {
            *throttle = 1.0;
            *afterburner = true;
        }
        if dist < 1_000.0 && lead_dir.angle_between(fwd) < 0.035 {
            actions.firing_gun = true;
        }
        if targeting.locked() && (700.0..4_500.0).contains(&dist) && *missile_wait <= 0.0 {
            actions.fire_missile = true;
            *missile_wait = 2.5;
        }
    };
    match tutorial.step() {
        Step::Throttle | Step::Cruise | Step::Complete => {}
        Step::Pitch => desired = (level + Vec3::Y * 0.5).normalize(),
        Step::Roll => roll = Some(1.0),
        Step::Rings | Step::Dive | Step::Climb | Step::Valley => {
            if let Some(g) = course.next_gate() {
                desired = (g.pos - pos).normalize_or(level);
            }
        }
        Step::Afterburner => afterburner = true,
        Step::Airbrake => {
            throttle = 0.0;
            airbrake = true;
        }
        Step::Targets => {
            if *timer > 1.0 {
                *timer = 0.0;
                actions.next_target = true;
            }
        }
        Step::Lock | Step::GunKill | Step::MissileKill | Step::Dogfight => attack(&mut desired, &mut throttle, &mut afterburner),
        Step::Flares => {}
    }
    // flares for anything coming at us
    if missiles.iter().any(|(m, mt)| m.target == Some(me) && mt.translation.distance(pos) < 2_500.0) {
        actions.holding_flares = true;
    }
    // don't fly into the hills
    let ground = terrain.max_height_along(pos, pos + state.velocity * 5.0).max(terrain.height_at(pos.x, pos.z));
    if (pos + state.velocity * 3.0).y < ground + 60.0 || pos.y < ground + 40.0 {
        desired = (level + Vec3::Y * 1.2).normalize();
        roll = None;
    }
    let (p, r, y) = steer_towards(t, state, desired, 1.2);
    input.pitch = p;
    input.roll = roll.unwrap_or(r);
    input.yaw = y;
    input.throttle = throttle;
    input.afterburner = afterburner;
    input.airbrake = airbrake;
}

// ---------------------------------------------------------------------------------------------
// UI

const GREEN: Color = Color::srgb(0.4, 1.0, 0.55);
const DIM: Color = Color::srgb(0.65, 0.8, 0.7);
const WHITE: Color = Color::WHITE;
const PANEL: Color = Color::srgba(0.02, 0.04, 0.03, 0.97);

#[derive(Component)]
struct PromptRoot;

#[derive(Component)]
struct PromptText;

/// The tick beside a finished step's prompt.
#[derive(Component)]
struct Tick;

#[derive(Component)]
struct StatusLine;

#[derive(Component)]
struct StepCount;

#[derive(Component)]
struct DoneScreen;

/// Shown only in the tutorial (the pause menu's SKIP).
#[derive(Component)]
pub struct TutorialOnly;

fn spawn_ui(mut commands: Commands, asset_server: Res<AssetServer>) {
    let font: Handle<Font> = asset_server.load("fonts/RobotoMono/RobotoMono-Medium.ttf");
    let text = |size: f32, color: Color| {
        (TextFont { font: font.clone().into(), font_size: FontSize::Px(size), ..default() }, TextColor(color))
    };
    let shadow = TextShadow { offset: Vec2::splat(1.5), color: Color::srgba(0.0, 0.0, 0.0, 0.65) };
    commands
        .spawn((
            PromptRoot,
            Node {
                position_type: PositionType::Absolute,
                width: percent(100),
                top: px(26),
                flex_direction: FlexDirection::Column,
                align_items: AlignItems::Center,
                row_gap: px(6),
                display: Display::None,
                ..default()
            },
            Pickable::IGNORE,
        ))
        .with_children(|root| {
            root.spawn((StepCount, Text::new(""), text(13.0, DIM), shadow));
            root.spawn(Node { column_gap: px(12), align_items: AlignItems::Center, ..default() }).with_children(|row| {
                // a tick: two sides of a box, turned 45°
                row.spawn((
                    Tick,
                    Node {
                        width: px(9),
                        height: px(18),
                        border: UiRect { right: px(3), bottom: px(3), ..default() },
                        margin: UiRect::bottom(px(4)),
                        display: Display::None,
                        ..default()
                    },
                    BorderColor::all(GREEN),
                    UiTransform::from_rotation(Rot2::degrees(45.0)),
                ));
                row.spawn((PromptText, Text::new(""), text(22.0, GREEN), shadow));
            });
            root.spawn((StatusLine, Text::new(""), text(16.0, WHITE), shadow));
        });
    commands
        .spawn((
            DoneScreen,
            crate::intro::KeepDuringIntro,
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
            GlobalZIndex(9),
        ))
        .with_children(|root| {
            root.spawn((
                Node {
                    flex_direction: FlexDirection::Column,
                    align_items: AlignItems::Center,
                    row_gap: px(22),
                    padding: UiRect::axes(px(40), px(28)),
                    border_radius: BorderRadius::all(px(10)),
                    ..default()
                },
                BackgroundColor(PANEL),
            ))
            .with_children(|panel| {
                panel.spawn((Text::new("TUTORIAL COMPLETE"), text(34.0, GREEN)));
                panel.spawn(Node { column_gap: px(14), ..default() }).with_children(|row| {
                    button(row, &font, "ENDLESS", Screen::Score, 0, 0, UiAction::TutorialEndless);
                    button(row, &font, "MENU", Screen::Score, 1, 0, UiAction::Menu);
                });
            });
        });
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn update_ui(
    time: Res<Time<Real>>,
    state: Res<State<GameState>>,
    mode: Res<GameMode>,
    paused: Res<Paused>,
    tutorial: Res<Tutorial>,
    device: Res<InputDevice>,
    control: Res<ControlMode>,
    mut nodes: Query<
        (&mut Node, Has<PromptRoot>, Has<Tick>, Has<DoneScreen>, Has<TutorialOnly>),
        Or<(With<PromptRoot>, With<Tick>, With<DoneScreen>, With<TutorialOnly>)>,
    >,
    mut texts: Query<(&mut Text, &mut TextColor, Has<PromptText>, Has<StatusLine>), Or<(With<PromptText>, With<StatusLine>, With<StepCount>)>>,
) {
    let on = *mode == GameMode::Tutorial && *state.get() == GameState::Playing;
    let step = tutorial.step();
    let done = on && step == Step::Complete;
    COMPLETE_SCREEN.store(done, Ordering::Relaxed);
    let ticked = tutorial.progress.done.is_some();
    for (mut node, prompt, tick, screen, only) in &mut nodes {
        let show = if prompt {
            on && !done && !paused.0
        } else if tick {
            ticked
        } else if screen {
            done && !paused.0
        } else {
            on && !done && only
        };
        let d = if show { Display::Flex } else { Display::None };
        if node.display != d {
            node.display = d;
        }
        if prompt {
            let top = px(26.0 + crate::platform::safe_area().y);
            if node.top != top {
                node.top = top;
            }
        }
    }
    if !on || done {
        return;
    }
    let flash = tutorial.progress.hinting() && (time.elapsed_secs() * 4.0).fract() < 0.5;
    for (mut text, mut color, is_prompt, is_status) in &mut texts {
        let (value, c) = if is_prompt {
            let c = if ticked || flash { WHITE } else { GREEN };
            (prompt(step, *device, *control == ControlMode::Direct).to_string(), c)
        } else if is_status {
            (tutorial.status.as_ref().map_or(String::new(), |(s, _)| s.clone()), WHITE)
        } else {
            (format!("{} / {}", step.index() + 1, Step::COUNTED), DIM)
        };
        if text.0 != value {
            text.0 = value;
        }
        if color.0 != c {
            color.0 = c;
        }
    }
}

/// TUTORIAL (the main menu), SKIP (the pause menu) and ENDLESS (the end screen).
pub fn handle_pressed(
    mut commands: Commands,
    mut pressed: ResMut<Pressed>,
    mut next: ResMut<NextState<GameState>>,
    mut tutorial: ResMut<Tutorial>,
    mut paused: ResMut<Paused>,
) {
    let mut mine = vec![];
    pressed.0.retain(|(a, _)| {
        let ours = matches!(a, UiAction::Tutorial | UiAction::SkipStep | UiAction::TutorialEndless);
        if ours {
            mine.push(*a);
        }
        !ours
    });
    for action in mine {
        match action {
            UiAction::Tutorial => crate::loading::begin_map(&mut commands, &mut next, GameMode::Tutorial, SEED, picks()),
            UiAction::SkipStep => {
                tutorial.skip = true;
                paused.0 = false;
            }
            UiAction::TutorialEndless => crate::loading::begin_round(&mut commands, &mut next, GameMode::Endless, Picks::from_settings()),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DT: f32 = 1.0 / 60.0;

    /// Runs `secs` of updates with the condition held at `met`; returns the transitions.
    fn run(p: &mut Progress, secs: f32, met: bool) -> Vec<(Step, Step, Reason)> {
        (0..(secs / DT).round() as usize).filter_map(|_| p.tick(DT, met, false, p.step.timeout())).collect()
    }

    #[test]
    fn steps_run_in_order_and_end_complete() {
        let mut p = Progress::new(Step::FIRST);
        let mut seen = vec![p.step];
        while p.step != Step::Complete {
            let moves = run(&mut p, TICK_SECS + 0.1, true);
            assert_eq!(moves.len(), 1, "one step per tick-and-a-bit at {:?}", seen.last());
            assert_eq!(moves[0].2, Reason::Done);
            seen.push(p.step);
        }
        assert_eq!(seen, Step::ALL.to_vec());
        // and it stays there
        assert!(run(&mut p, 10.0, true).is_empty());
    }

    #[test]
    fn a_met_step_shows_its_tick_before_moving_on() {
        let mut p = Progress::new(Step::Pitch);
        assert!(run(&mut p, 2.0, false).is_empty());
        assert!(p.done.is_none());
        p.tick(DT, true, false, 40.0);
        assert!(p.done.is_some(), "ticked");
        // the condition can lapse during the tick: it's still done
        let moves = run(&mut p, TICK_SECS + 0.05, false);
        assert_eq!(moves, vec![(Step::Pitch, Step::Roll, Reason::Done)]);
        assert!(p.t < 0.1, "the next step's clock starts afresh");
    }

    #[test]
    fn skip_moves_on_at_once() {
        let mut p = Progress::new(Step::Rings);
        assert_eq!(p.tick(DT, false, true, 150.0), Some((Step::Rings, Step::Dive, Reason::Skipped)));
        assert_eq!(p.step, Step::Dive);
    }

    #[test]
    fn every_step_gives_up_eventually() {
        for step in Step::ALL.into_iter().filter(|s| *s != Step::Complete) {
            let mut p = Progress::new(step);
            let moves = run(&mut p, step.timeout() + TICK_SECS + 0.5, false);
            assert_eq!(moves.len(), 1, "{step:?} never timed out");
            assert_eq!(moves[0].2, Reason::TimedOut);
            assert_eq!(Some(moves[0].1), step.next());
        }
    }

    #[test]
    fn the_prompt_flashes_while_waiting() {
        let mut p = Progress::new(Step::Throttle);
        assert!(!p.hinting());
        run(&mut p, Step::Throttle.hint_after() + 0.5, false);
        assert!(p.hinting());
        run(&mut p, HINT_FLASH, false);
        assert!(!p.hinting(), "only now and then");
    }

    #[test]
    fn every_step_has_a_prompt_for_every_device() {
        for step in Step::ALL.into_iter().filter(|s| *s != Step::Complete) {
            for device in [InputDevice::Keyboard, InputDevice::Gamepad, InputDevice::Touch] {
                for direct in [false, true] {
                    let p = prompt(step, device, direct);
                    assert!(!p.is_empty() && p.len() < 70, "{step:?} {device:?}: {p:?}");
                }
            }
        }
        // touch never mentions keys, and the keyboard never mentions sticks
        for step in Step::ALL {
            assert!(!prompt(step, InputDevice::Touch, false).contains("Shift"));
            assert!(!prompt(step, InputDevice::Keyboard, false).contains("stick"));
        }
    }

    #[test]
    fn step_names_round_trip() {
        for step in Step::ALL {
            assert_eq!(Step::parse(step.name()), Some(step));
        }
    }

    #[test]
    fn rings_curve_gently_and_clear_the_ground() {
        let terrain = Terrain::default();
        let gates = ring_course(&terrain, Vec3::new(0.0, 1_500.0, 0.0), Vec3::NEG_Z);
        assert_eq!(gates.len(), 4);
        let mut prev = Vec3::new(0.0, 1_500.0, 0.0);
        for g in &gates {
            assert!((g.pos.distance(prev) - 1_800.0).abs() < 1.0);
            assert!(g.pos.y >= 1_500.0);
            prev = g.pos;
        }
        // to the right (+X when heading -Z)
        assert!(gates[3].pos.x > 1_000.0);
        // a dive ring on flat ground keeps 250 m of air under it
        let low = ring_ahead(&terrain, Vec3::new(0.0, 300.0, 0.0), Vec3::NEG_Z, 2_400.0, -150.0);
        assert_eq!(low.pos.y, 250.0);
    }
}
