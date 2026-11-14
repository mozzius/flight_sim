//! The dogfight behind the menu: a small furball of AI jets (3v3) in front of the menu camera, below the cloud base.
//! They use the real AI, flight model and weapons, but nothing here touches
//! the round: no score, no HUD boxes, no player, no network. Shot-down jets
//! fall for a few seconds, blow up, and a replacement flies in from its
//! side, out of frame, so the fight never ends. It's spawned while the menu
//! is up (not in a lobby) and cleared when it goes; it's silent (see `audio`).
//! It opens like a round: the two sides spawn head-on a few km apart either
//! side of the arena, lock up and trade missiles within a few seconds, then
//! furball. The opening spawn is fixed, so menu screenshots show the same merge.
//! `FLIGHT_SIM_NO_BACKDROP_FIGHT=1` turns it off; `FLIGHT_SIM_LOG_BACKDROP=1`
//! logs where the jets are in the camera's view.

use bevy::prelude::*;

use crate::{
    ai::{Arena, Pilot},
    flight::{AircraftCrashed, Disabled, FlightSystems},
    game::{Explosion, Health, Models, Team},
    loading::GameState,
    weapons::{Bullet, Flare, Missile},
    world::Terrain,
};

pub struct BackdropPlugin;

impl Plugin for BackdropPlugin {
    fn build(&self, app: &mut App) {
        if std::env::var("FLIGHT_SIM_NO_BACKDROP_FIGHT").is_ok() {
            return;
        }
        app.init_resource::<Fight>()
            .add_systems(Update, (sync, follow_camera).chain())
            .add_systems(FixedUpdate, eager.before(FlightSystems::Pilots))
            .add_systems(FixedUpdate, (sharpen_missiles, damage, crashes, respawn).chain().after(FlightSystems::Combat));
    }
}

/// A jet in the menu's backdrop fight.
#[derive(Component)]
pub struct MenuJet {
    /// Seconds since it was shot down.
    down: Option<f32>,
    /// An opener, still closing for the merge: the arena leash is off until
    /// it has passed through (it would only bend the head-on pass).
    opening: bool,
}

#[derive(Resource, Default)]
struct Fight {
    on: bool,
    /// (seconds left, team) for replacements on the way.
    pending: Vec<(f32, Team)>,
    /// Spawn counter (varies the replacements' entry points and skills).
    spawned: u32,
    /// When (real seconds) the current fight started, and whether its first
    /// missile has gone yet (logged with `FLIGHT_SIM_LOG_BACKDROP=1`).
    started: f32,
    first_missile: bool,
}

/// How far ahead of the camera the fight is centred (horizontally, m).
const RANGE: f32 = 600.0;
/// Altitude band above the camera's eye: under the cloud base (~1.9 km).
const FLOOR: f32 = 350.0;
const CENTRE: f32 = 600.0;
const CEILING: f32 = 850.0;
const RADIUS: f32 = 350.0;
/// A shot-down jet falls this long before it blows up.
const FALL: f32 = 5.0;
/// Then its replacement arrives after this long.
const RESPAWN: f32 = 4.0;
/// The opening merge: the sides start this far apart (m), head-on through
/// the arena's centre, well inside head-on missile range (~5 km)...
const OPENING_GAP: f32 = 4_400.0;
/// ...each pair's first missile is due this long after the start (plus a
/// stagger per pair; the lock takes 1.2 s)...
const OPENING_SALVO: f32 = 3.0;
/// ...and an opener rejoins the arena once it's passed through the merge
/// (inside the radius), or after this long at most.
const OPENING_LEASH: f32 = 12.0;

fn jets_per_side() -> usize {
    3
}

/// The arena in front of the menu camera now.
fn arena(time: &Time<Real>, terrain: &Terrain) -> (Arena, Vec3, Vec3) {
    let (eye, yaw) = crate::menu::menu_view(time.elapsed_secs(), Some(terrain));
    let fwd = Quat::from_rotation_y(yaw) * Vec3::NEG_Z;
    let centre = eye + fwd * RANGE + Vec3::Y * CENTRE;
    let arena = Arena { centre, radius: RADIUS, floor: eye.y + FLOOR, ceiling: eye.y + CEILING };
    (arena, eye, fwd)
}

/// A cheap deterministic hash in 0..1 (screenshot runs show the same fight).
fn hash(n: u32) -> f32 {
    let mut x = n.wrapping_mul(0x9E37_79B9) ^ 0x85EB_CA6B;
    x ^= x >> 15;
    x = x.wrapping_mul(0x2C1B_3C6D);
    x ^= x >> 12;
    (x & 0xFFFF) as f32 / 65_535.0
}

/// A menu jet; `opening` = the first missile's cooldown for an opener (which
/// flies without the arena leash until the merge), `None` for a replacement.
#[allow(clippy::too_many_arguments)]
fn spawn_jet(
    commands: &mut Commands,
    models: &Models,
    terrain: &Terrain,
    fight: &mut Fight,
    team: Team,
    pos: Vec3,
    heading: Vec3,
    arena: Arena,
    opening: Option<f32>,
) {
    let n = fight.spawned;
    fight.spawned += 1;
    let e = crate::game::spawn_aircraft(commands, models, terrain, team, pos, heading, false);
    let mut pilot = Pilot::new();
    pilot.skill = 0.5 + 0.25 * hash(n * 7 + 3);
    if let Some(cooldown) = opening {
        pilot.missile_cooldown = cooldown;
    }
    // 40% of the round's health (see also `eager` and `sharpen_missiles`):
    // left alone the AI's fights mostly stalemate, and the kills are the show
    let mut jet = commands.entity(e);
    jet.insert((pilot, MenuJet { down: None, opening: opening.is_some() }, Health::new(40.0)));
    if opening.is_none() {
        jet.insert(arena);
    }
}

/// Spawns the fight while the menu is up (and not in a lobby), clears it otherwise.
#[allow(clippy::too_many_arguments)]
fn sync(
    mut commands: Commands,
    state: Res<State<GameState>>,
    net: Option<Res<crate::net::Net>>,
    time: Res<Time<Real>>,
    terrain: Res<Terrain>,
    models: Res<Models>,
    mut fight: ResMut<Fight>,
    jets: Query<Entity, With<MenuJet>>,
    shots: Query<Entity, Or<(With<Bullet>, With<Missile>, With<Flare>)>>,
) {
    let want = *state.get() == GameState::Menu && !net.is_some_and(|n| n.in_lobby());
    if want == fight.on {
        return;
    }
    fight.on = want;
    fight.pending.clear();
    fight.started = time.elapsed_secs();
    fight.first_missile = false;
    if !want {
        for e in jets.iter().chain(shots.iter()) {
            commands.entity(e).despawn();
        }
        return;
    }
    // the opening merge, like a round's: the sides in line abreast (in depth,
    // stepped up), head-on across the camera's view through the arena's
    // centre, OPENING_GAP apart. They lock up at once, a missile each from
    // ~3 s (staggered by pair), meet in front of the camera and furball
    let (arena, _, fwd) = arena(&time, &terrain);
    let side = fwd.cross(Vec3::Y);
    for i in 0..jets_per_side() {
        let off = fwd * ((i as f32 - 1.0) * 220.0) + Vec3::Y * ((i as f32 - 1.0) * 70.0);
        let a = arena.centre + side * (OPENING_GAP * 0.5) + off;
        let b = arena.centre - side * (OPENING_GAP * 0.5) + off;
        let salvo = |k: u32| Some(OPENING_SALVO + 0.7 * i as f32 + 0.3 * hash(k));
        spawn_jet(&mut commands, &models, &terrain, &mut fight, Team::Blue, a, -side, arena, salvo(i as u32 * 2));
        spawn_jet(&mut commands, &models, &terrain, &mut fight, Team::Red, b, side, arena, salvo(i as u32 * 2 + 1));
    }
}

/// The arena turns with the camera. `FLIGHT_SIM_LOG_BACKDROP=1` logs where
/// each jet is in the camera's view once a second.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn follow_camera(
    mut commands: Commands,
    time: Res<Time<Real>>,
    terrain: Res<Terrain>,
    mut fight: ResMut<Fight>,
    mut arenas: Query<&mut Arena, With<MenuJet>>,
    mut openers: Query<(Entity, &Transform, &mut MenuJet), Without<Arena>>,
    new_missiles: Query<&Missile, Added<Missile>>,
    jets: Query<(&Team, &Transform, Has<Disabled>), With<MenuJet>>,
    mut next_log: Local<f32>,
    shots: Query<(Has<Bullet>, Has<Missile>, Has<Flare>), Or<(With<Bullet>, With<Missile>, With<Flare>)>>,
    health: Query<&Health, With<MenuJet>>,
) {
    let (now, eye, fwd) = arena(&time, &terrain);
    for mut a in &mut arenas {
        *a = now;
    }
    // openers rejoin the arena once they've come through the merge
    let since = time.elapsed_secs() - fight.started;
    for (e, t, mut jet) in &mut openers {
        let d = t.translation - now.centre;
        if jet.opening && (Vec2::new(d.x, d.z).length() < now.radius || since > OPENING_LEASH) {
            jet.opening = false;
            commands.entity(e).insert(now);
        }
    }
    let log = std::env::var("FLIGHT_SIM_LOG_BACKDROP").is_ok();
    if fight.on && !fight.first_missile && new_missiles.iter().any(|m| jets.contains(m.owner)) {
        fight.first_missile = true;
        if log {
            info!("backdrop: first missile {since:.2} s after the menu opened (at t{:.1})", fight.started);
        }
    }
    if time.elapsed_secs() >= *next_log && log {
        *next_log = time.elapsed_secs() + 1.0;
        let side = fwd.cross(Vec3::Y);
        let line: Vec<String> = jets
            .iter()
            .map(|(team, t, down)| {
                let d = t.translation - eye;
                let bearing = d.dot(side).atan2(d.dot(fwd)).to_degrees();
                let elev = d.y.atan2(d.xz().length()).to_degrees();
                format!("{team:?}{} b{bearing:.0} e{elev:.0} r{:.0}", if down { "(down)" } else { "" }, d.length())
            })
            .collect();
        let (b, m, f) = shots.iter().fold((0, 0, 0), |(b, m, f), (x, y, z)| (b + x as u32, m + y as u32, f + z as u32));
        let hp: Vec<String> = health.iter().map(|h| format!("{:.0}", h.current)).collect();
        info!("backdrop t{:.0}: {} || bullets {b} missiles {m} flares {f} hp {}", time.elapsed_secs(), line.join(" | "), hp.join(","));
    }
}

/// Gap between a menu jet's missiles, at most (the round's AI waits 7-14 s / skill).
const MISSILE_GAP: f32 = 4.0;
/// Menu jets fire the EASY difficulty's seeker (the player's best: ~89% hits).
const SEEKER: crate::weapons::Seeker = crate::difficulty::EASY.seeker;

/// Menu jets fire missiles more often than the round's AI.
fn eager(mut pilots: Query<&mut Pilot, With<MenuJet>>) {
    for mut p in &mut pilots {
        p.missile_cooldown = p.missile_cooldown.min(MISSILE_GAP);
    }
}

/// ...and their missiles are harder to flare off.
fn sharpen_missiles(mut new: Query<&mut Missile, Added<Missile>>, jets: Query<(), With<MenuJet>>) {
    for mut m in &mut new {
        if jets.contains(m.owner) {
            m.seeker = SEEKER;
        }
    }
}

/// Shot down: out of the fight, falling (the round's `handle_damage` only
/// runs while playing).
fn damage(
    mut commands: Commands,
    time: Res<Time>,
    mut jets: Query<(Entity, &Health, &Transform, &mut MenuJet)>,
    mut explosions: MessageWriter<Explosion>,
) {
    let dt = time.delta_secs();
    for (e, health, t, mut jet) in &mut jets {
        match &mut jet.down {
            None if health.current <= 0.0 => {
                jet.down = Some(0.0);
                commands.entity(e).insert(Disabled).remove::<Pilot>();
                explosions.write(Explosion { position: t.translation, size: 12.0 });
            }
            Some(secs) => {
                *secs += dt;
                if *secs > FALL {
                    explosions.write(Explosion { position: t.translation, size: 25.0 });
                    commands.entity(e).despawn();
                }
            }
            None => {}
        }
    }
}

fn crashes(
    mut commands: Commands,
    mut crashed: MessageReader<AircraftCrashed>,
    jets: Query<(), With<MenuJet>>,
    mut explosions: MessageWriter<Explosion>,
) {
    for c in crashed.read() {
        if jets.contains(c.entity) {
            explosions.write(Explosion { position: c.position, size: 25.0 });
            commands.entity(c.entity).try_despawn();
        }
    }
}

/// Tops each side back up: a replacement joins from behind the camera once
/// the downed jet has fallen and blown up, `FALL + RESPAWN` s after the kill.
#[allow(clippy::too_many_arguments)]
fn respawn(
    mut commands: Commands,
    time: Res<Time>,
    real: Res<Time<Real>>,
    terrain: Res<Terrain>,
    models: Res<Models>,
    mut fight: ResMut<Fight>,
    jets: Query<(&Team, Has<Disabled>), With<MenuJet>>,
) {
    if !fight.on {
        return;
    }
    let dt = time.delta_secs();
    // alive per side, plus those already on the way
    for team in [Team::Blue, Team::Red] {
        let alive = jets.iter().filter(|(t, down)| **t == team && !down).count();
        let coming = fight.pending.iter().filter(|(_, t)| *t == team).count();
        for _ in alive + coming..jets_per_side() {
            fight.pending.push((RESPAWN + FALL, team));
        }
    }
    let mut ready = Vec::new();
    fight.pending.retain_mut(|(t, team)| {
        *t -= dt;
        if *t <= 0.0 {
            ready.push(*team);
            false
        } else {
            true
        }
    });
    let (arena, eye, fwd) = arena(&real, &terrain);
    for team in ready {
        // in from its side, out of frame (beyond a landscape screen's edge;
        // blue from the right, red from the left), heading for the fight
        let n = fight.spawned;
        let sign = if team == Team::Blue { 1.0 } else { -1.0 };
        let angle = sign * (75.0 + 20.0 * hash(n * 13 + 1)).to_radians();
        let dir = Quat::from_rotation_y(-angle) * fwd;
        let pos = Vec3::new(eye.x, arena.centre.y, eye.z) + dir * 1_400.0;
        let heading = (arena.centre - pos).normalize();
        spawn_jet(&mut commands, &models, &terrain, &mut fight, team, pos, heading, arena, None);
    }
}
