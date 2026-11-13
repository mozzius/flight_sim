//! AI pilots. They fly the same flight model as the player through
//! [`steer_towards`], so they have to manage energy and can stall too.
//!
//! Priorities each tick, highest first:
//! 1. don't hit the ground or buildings
//! 2. defend against incoming missiles (beam, flares, afterburner)
//! 3. break if someone is on our tail, or panic (flare and break) when a
//!    locked pursuer has sat in a strong pouncing position for a while
//! 4. don't collide with other aircraft
//! 5. attack the current target (lead pursuit for guns, lock and fire missiles)

use bevy::prelude::*;
use rand::RngExt;

use crate::{
    flight::{Aircraft, ControlInput, Disabled, FlightState, FlightSystems, steer_towards},
    game::{Player, Team},
    weapons::{
        LOCK_RANGE, Missile, Targeting, Weapons, gun_lead_point,
    },
    world::Terrain,
};

pub struct AIPlugin;

impl Plugin for AIPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            FixedUpdate,
            (measure_pressure, ai_pilots).chain().in_set(FlightSystems::Pilots),
        );
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Attack,
    Defend,
    Evade,
    AvoidTerrain,
}

#[derive(Component)]
pub struct Pilot {
    /// 0.5 (rookie) .. 1.0 (ace): aim, reactions, how hard they pull.
    pub skill: f32,
    pub mode: Mode,
    retarget_timer: f32,
    /// Direction for the current defensive break, held for a while.
    break_dir: Option<Vec3>,
    break_timer: f32,
    jink_phase: f32,
    missile_cooldown: f32,
    /// Trigger discipline: short bursts.
    burst_timer: f32,
    /// Enemies favour attacking the player.
    hunts_player: bool,
    /// 0..1: builds while a hostile holds a lock from a strong position
    /// behind us, decays otherwise. High stress makes the pilot panic-flare.
    pub stress: f32,
    /// Where the strongest locked pursuer is (if any), for a panic break.
    pressure_from: Option<Vec3>,
    /// Counts down after a panic so one scare dumps one salvo.
    panic_cooldown: f32,
    /// A panic break in progress (s).
    panic_timer: f32,
}

/// Seconds of a perfect pounce (locked, dead astern, in range) to max out stress.
const STRESS_BUILD_TIME: f32 = 6.0;
/// Seconds for full stress to drain away once nobody is pressing.
const STRESS_DECAY_TIME: f32 = 10.0;
/// Panic hazard per second at full stress, before the skill factor.
const PANIC_RATE: f32 = 1.2;
/// Seconds between panics (so a scare is one salvo, not the whole load).
const PANIC_COOLDOWN: (f32, f32) = (3.0, 5.0);
/// How long a panic break lasts (s).
const PANIC_BREAK: f32 = 2.0;

/// How strong a pouncing position a locked attacker has on `me` (0..1): close
/// to our tail (full inside ~30°, nothing past ~50°), at a good missile range
/// (900-2800 m, fading out by 500 m and 4 km), nose on and not falling behind.
pub fn pounce_quality(
    me_pos: Vec3,
    me_fwd: Vec3,
    me_vel: Vec3,
    att_pos: Vec3,
    att_fwd: Vec3,
    att_vel: Vec3,
) -> f32 {
    let to_att = att_pos - me_pos;
    let dist = to_att.length();
    if dist < 1.0 {
        return 0.0;
    }
    let dir = to_att / dist;
    // angle off our tail
    let tail_cos = (-me_fwd).dot(dir);
    let (full, none) = (30f32.to_radians().cos(), 50f32.to_radians().cos());
    let rear = ((tail_cos - none) / (full - none)).clamp(0.0, 1.0);
    let range = if dist < 900.0 {
        ((dist - 500.0) / 400.0).clamp(0.0, 1.0)
    } else {
        ((4_000.0 - dist) / 1_200.0).clamp(0.0, 1.0)
    };
    // can we shake it? not if its nose is on us and it's keeping up
    let nose = ((att_fwd.dot(-dir) - 0.82) / (0.97 - 0.82)).clamp(0.0, 1.0);
    let closure = (att_vel - me_vel).dot(-dir);
    let keeping_up = ((closure + 60.0) / 60.0).clamp(0.0, 1.0);
    rear * range * (0.4 + 0.6 * nose) * keeping_up
}

/// Stress builds with locked time and position quality, and decays when not pressured.
pub fn update_stress(stress: f32, quality: f32, dt: f32) -> f32 {
    let next = if quality > 0.05 {
        stress + dt * quality / STRESS_BUILD_TIME
    } else {
        stress - dt / STRESS_DECAY_TIME
    };
    next.clamp(0.0, 1.0)
}

/// Panic hazard per second: grows with stress squared, rookies (0.55) crack
/// ~10x as readily as aces (1.0).
pub fn panic_rate(stress: f32, skill: f32) -> f32 {
    PANIC_RATE * stress * stress * (1.05 - skill).max(0.0)
}

impl Pilot {
    pub fn new() -> Self {
        let mut rng = rand::rng();
        Pilot {
            skill: rng.random_range(0.55..1.0),
            mode: Mode::Attack,
            retarget_timer: 0.0,
            break_dir: None,
            break_timer: 0.0,
            jink_phase: rng.random_range(0.0..10.0),
            missile_cooldown: rng.random_range(3.0..8.0),
            burst_timer: 0.0,
            hunts_player: rng.random_bool(0.6),
            stress: 0.0,
            pressure_from: None,
            panic_cooldown: 0.0,
            panic_timer: 0.0,
        }
    }
}

struct Contact {
    entity: Entity,
    pos: Vec3,
    vel: Vec3,
    fwd: Vec3,
    team: Team,
    player: bool,
}

/// How hard each AI is being pounced on by hostiles that hold a missile lock.
fn measure_pressure(
    time: Res<Time>,
    seekers: Query<(&Transform, &FlightState, &Team, &Targeting), (With<Aircraft>, Without<Disabled>)>,
    mut pilots: Query<(Entity, &Transform, &FlightState, &Team, &mut Pilot), Without<Disabled>>,
) {
    let dt = time.delta_secs();
    for (me, transform, state, team, mut pilot) in &mut pilots {
        let pos = transform.translation;
        let fwd = transform.forward().as_vec3();
        let (quality, from) = seekers
            .iter()
            .filter(|(_, _, t, tg)| **t != *team && tg.target == Some(me) && tg.locked())
            .map(|(t, fs, ..)| {
                let q = pounce_quality(
                    pos,
                    fwd,
                    state.velocity,
                    t.translation,
                    t.forward().as_vec3(),
                    fs.velocity,
                );
                (q, t.translation)
            })
            .max_by(|a, b| a.0.total_cmp(&b.0))
            .unzip();
        let quality = quality.unwrap_or(0.0);
        pilot.stress = update_stress(pilot.stress, quality, dt);
        pilot.pressure_from = from.filter(|_| quality > 0.05);
    }
}

#[allow(clippy::type_complexity)]
fn ai_pilots(
    time: Res<Time>,
    terrain: Res<Terrain>,
    mut pilots: Query<
        (
            Entity,
            &mut Pilot,
            &Transform,
            &FlightState,
            &Team,
            &mut ControlInput,
            &mut Weapons,
            &mut Targeting,
        ),
        Without<Disabled>,
    >,
    all: Query<(Entity, &Transform, &FlightState, &Team, Has<Player>), (With<Aircraft>, Without<Disabled>)>,
    missiles: Query<(&Missile, &Transform)>,
) {
    let dt = time.delta_secs();
    let t_now = time.elapsed_secs();
    let mut rng = rand::rng();

    let contacts: Vec<Contact> = all
        .iter()
        .map(|(entity, t, fs, team, player)| Contact {
            entity,
            pos: t.translation,
            vel: fs.velocity,
            fwd: t.forward().as_vec3(),
            team: *team,
            player,
        })
        .collect();

    for (me, mut pilot, transform, state, team, mut input, mut weapons, mut targeting) in
        &mut pilots
    {
        let pos = transform.translation;
        let fwd = transform.forward().as_vec3();
        let speed = state.airspeed;
        weapons.trigger_gun = false;
        pilot.missile_cooldown -= dt;
        pilot.break_timer -= dt;
        pilot.retarget_timer -= dt;
        pilot.panic_cooldown -= dt;
        pilot.panic_timer -= dt;

        // ---- choose a target ----
        let target_alive = targeting
            .target
            .is_some_and(|t| contacts.iter().any(|c| c.entity == t));
        if !target_alive || pilot.retarget_timer <= 0.0 {
            pilot.retarget_timer = rng.random_range(3.0..6.0) / pilot.skill;
            let best = contacts
                .iter()
                .filter(|c| c.team != *team)
                .min_by(|a, b| {
                    score_target(pos, fwd, a, pilot.hunts_player)
                        .total_cmp(&score_target(pos, fwd, b, pilot.hunts_player))
                })
                .map(|c| c.entity);
            if best != targeting.target {
                targeting.target = best;
                targeting.lock = 0.0;
            }
        }
        let target = targeting
            .target
            .and_then(|t| contacts.iter().find(|c| c.entity == t));

        // ---- threats ----
        let incoming = missiles
            .iter()
            .filter(|(m, _)| m.target == Some(me))
            .map(|(m, t)| (t.translation, m.velocity))
            .min_by(|a, b| a.0.distance(pos).total_cmp(&b.0.distance(pos)));
        let tail_threat = contacts.iter().find(|c| {
            c.team != *team && {
                let to_me = pos - c.pos;
                let d = to_me.length();
                d < 1_400.0 && d > 1.0 && c.fwd.dot(to_me / d) > 0.93 && fwd.dot(to_me / d) > 0.0
            }
        });

        // ---- terrain look-ahead ----
        let look_ahead = state.velocity * 6.0;
        let ground = terrain
            .max_height_along(pos, pos + look_ahead)
            .max(terrain.height_at(pos.x, pos.z));
        let clearance = pos.y - ground;
        let sink = (-state.velocity.y).max(0.0);
        let terrain_danger = clearance < 250.0 + sink * 5.0 || (pos + look_ahead).y < ground + 150.0;

        // ---- nerve: a locked pursuer sat on our tail may make us flare early ----
        let real_missile = incoming.is_some_and(|(mp, _)| mp.distance(pos) < 4_500.0);
        if let Some(from) = pilot.pressure_from
            && !terrain_danger
            && !real_missile
            && pilot.panic_cooldown <= 0.0
            && weapons.flares > 0
            && rng.random_bool((panic_rate(pilot.stress, pilot.skill) * dt).clamp(0.0, 1.0) as f64)
        {
            weapons.drop_flares = true;
            pilot.panic_cooldown = rng.random_range(PANIC_COOLDOWN.0..PANIC_COOLDOWN.1);
            pilot.panic_timer = PANIC_BREAK;
            // hard break across the pursuer's line of sight
            let to_att = (from - pos).normalize_or_zero();
            let lateral = to_att.cross(Vec3::Y).normalize_or(Vec3::X)
                * if rng.random_bool(0.5) { 1.0 } else { -1.0 };
            let vertical = if clearance > 800.0 { -0.2 } else { 0.3 };
            pilot.break_dir = Some((lateral + Vec3::Y * vertical).normalize());
            pilot.break_timer = PANIC_BREAK;
        }
        let defend_from = tail_threat
            .map(|c| c.pos)
            .or(pilot.pressure_from.filter(|_| pilot.panic_timer > 0.0));

        let mut throttle = 0.85;
        let mut afterburner = false;
        let mut aggression = 0.6 + 0.4 * pilot.skill;

        let desired: Vec3 = if terrain_danger {
            pilot.mode = Mode::AvoidTerrain;
            afterburner = true;
            // climb away, keeping heading
            let flat = Vec3::new(fwd.x, 0.0, fwd.z).normalize_or(Vec3::NEG_Z);
            (flat + Vec3::Y * 1.2).normalize()
        } else if let Some((mpos, mvel)) = incoming.filter(|(mp, _)| mp.distance(pos) < 4_500.0) {
            pilot.mode = Mode::Evade;
            afterburner = true;
            aggression = 1.0;
            let to_missile = (mpos - pos).normalize_or_zero();
            let closing = (mvel - state.velocity).dot(-to_missile).max(1.0);
            let tti = mpos.distance(pos) / closing;
            if mpos.distance(pos) < 1_800.0 && rng.random_bool((dt * 4.0 * pilot.skill) as f64) {
                weapons.drop_flares = true;
            }
            let side = to_missile.cross(Vec3::Y).normalize_or(Vec3::X);
            let side = if side.dot(fwd) > 0.0 { side } else { -side };
            // good pilots time a hard break for the last couple of seconds;
            // until then, beam it (and dive if there's room)
            let break_at = 2.6 - 1.2 * pilot.skill; // the best pilots wait for ~1.4 s
            if tti < break_at {
                aggression = 1.2;
                (side + Vec3::Y * if clearance > 700.0 { -0.2 } else { 0.4 }).normalize()
            } else {
                let dive = if clearance > 900.0 { -0.35 } else { 0.15 };
                (side * 0.8 + fwd * 0.2 + Vec3::Y * dive).normalize()
            }
        } else if let Some(threat_pos) = defend_from {
            pilot.mode = Mode::Defend;
            afterburner = speed < 260.0;
            aggression = 1.0;
            if pilot.break_timer <= 0.0 || pilot.break_dir.is_none() {
                // break hard across the attacker's nose, randomly left/right/up
                let to_threat = (threat_pos - pos).normalize_or_zero();
                let lateral = to_threat.cross(Vec3::Y).normalize_or(Vec3::X)
                    * if rng.random_bool(0.5) { 1.0 } else { -1.0 };
                let vertical = if clearance > 800.0 {
                    rng.random_range(-0.4..0.5)
                } else {
                    0.3
                };
                pilot.break_dir = Some((lateral + Vec3::Y * vertical).normalize());
                pilot.break_timer = rng.random_range(1.5..3.0);
            }
            // jink so a steady gun solution is hard
            let jink = (t_now * 2.2 + pilot.jink_phase).sin() * 0.35;
            let dir = pilot.break_dir.unwrap();
            (dir + fwd.cross(Vec3::Y) * jink).normalize()
        } else if let Some(target) = target {
            pilot.mode = Mode::Attack;
            let to = target.pos - pos;
            let dist = to.length();
            let lead = gun_lead_point(pos, state.velocity, target.pos, target.vel);
            let lead_dir = (lead - pos).normalize_or(fwd);
            let off_bore = lead_dir.angle_between(fwd);

            // energy: go fast when far, match speed when close, never get slow
            if dist > 2_500.0 {
                afterburner = speed < 330.0;
                throttle = 1.0;
            } else {
                let closure = (state.velocity - target.vel).dot(to / dist.max(1.0));
                throttle = (0.75 - closure / 150.0).clamp(0.3, 1.0);
                if dist < 300.0 && closure > 30.0 {
                    throttle = 0.0;
                }
            }
            if speed < 180.0 {
                throttle = 1.0;
                afterburner = true;
                aggression *= 0.6; // don't pull into a stall
            }

            // guns: lead pursuit when close, pure pursuit otherwise
            let aim = if dist < 1_500.0 { lead_dir } else { to / dist.max(1.0) };
            let aim_tolerance = 0.06 - 0.03 * pilot.skill;
            if dist < 1_000.0 && off_bore < aim_tolerance {
                pilot.burst_timer += dt;
                // short bursts with pauses
                weapons.trigger_gun = pilot.burst_timer % 1.4 < 0.7;
            } else {
                pilot.burst_timer = 0.0;
            }

            // missiles: when locked and in a sensible envelope
            // missile range depends on aspect: a tail chase needs to be close,
            // a head-on shot works from much further out
            let head_on = (-target.vel.normalize_or_zero().dot(to / dist.max(1.0))).clamp(0.0, 1.0);
            let max_range = (2_500.0 + 2_500.0 * head_on).min(LOCK_RANGE * 0.9);
            if targeting.locked()
                && pilot.missile_cooldown <= 0.0
                && (900.0..max_range).contains(&dist)
            {
                weapons.fire_missile = true;
                pilot.missile_cooldown = rng.random_range(7.0..14.0) / pilot.skill;
            }

            // bad aim for rookies: wobble the aim point
            let wobble = Vec3::new(
                (t_now * 1.3 + pilot.jink_phase).sin(),
                (t_now * 1.7 + pilot.jink_phase).cos(),
                0.0,
            ) * (1.0 - pilot.skill) * 0.08;
            (aim + transform.rotation * wobble).normalize()
        } else {
            // nothing to do: cruise in a gentle orbit over the city
            throttle = 0.7;
            let to_centre = Vec3::new(-pos.x, 0.0, -pos.z).normalize_or_zero();
            (fwd + to_centre * 0.3 + Vec3::Y * ((1_500.0 - pos.y) / 3_000.0)).normalize()
        };

        // ---- separation from other aircraft ----
        let mut desired = desired;
        if pilot.mode != Mode::AvoidTerrain {
            for c in &contacts {
                if c.entity == me {
                    continue;
                }
                let away = pos - c.pos;
                let d = away.length();
                // only worry about things roughly ahead of us that are close
                if d < 180.0 && d > 0.1 && (c.pos - pos).dot(fwd) > 0.0 {
                    desired = (desired + away / d * (1.0 - d / 180.0) * 2.0).normalize();
                }
            }
        }

        // never ask for a dive straight into the ground
        if clearance < 600.0 && desired.y < 0.0 {
            desired.y *= (clearance / 600.0).max(0.0);
            desired = desired.normalize_or(fwd);
        }

        let (pitch, roll, yaw) = steer_towards(transform, state, desired, aggression);
        input.pitch = pitch;
        input.roll = roll;
        input.yaw = yaw;
        input.throttle = throttle;
        input.afterburner = afterburner;
        input.airbrake = false;

    }
}

/// Lower is better: close, in front of us, and (sometimes) the player.
fn score_target(pos: Vec3, fwd: Vec3, c: &Contact, hunts_player: bool) -> f32 {
    let to = c.pos - pos;
    let dist = to.length();
    let angle = to.normalize_or_zero().angle_between(fwd);
    let mut score = dist + angle * 1_500.0;
    if c.player && hunts_player {
        score *= 0.35;
    }
    score
}

#[cfg(test)]
mod tests {
    use super::*;

    const DT: f32 = 1.0 / 60.0;

    /// An AI flying -Z at 250 m/s, with an attacker `dist` behind at `off_deg` off its tail.
    fn quality(dist: f32, off_deg: f32, closure: f32) -> f32 {
        let me_pos = Vec3::new(0.0, 3_000.0, 0.0);
        let me_vel = Vec3::NEG_Z * 250.0;
        let a = off_deg.to_radians();
        let att_pos = me_pos + Vec3::new(a.sin(), 0.0, a.cos()) * dist;
        let att_fwd = (me_pos - att_pos).normalize();
        let att_vel = me_vel + att_fwd * closure;
        pounce_quality(me_pos, Vec3::NEG_Z, me_vel, att_pos, att_fwd, att_vel)
    }

    #[test]
    fn pounce_quality_wants_rear_aspect_good_range_and_pursuit() {
        assert!(quality(1_800.0, 0.0, 20.0) > 0.95);
        assert!(quality(1_800.0, 45.0, 20.0) > 0.0 && quality(1_800.0, 45.0, 20.0) < 0.5);
        assert_eq!(quality(1_800.0, 70.0, 20.0), 0.0, "beam isn't a pounce");
        assert_eq!(quality(1_800.0, 180.0, 20.0), 0.0, "head on isn't a pounce");
        assert_eq!(quality(300.0, 0.0, 20.0), 0.0, "too close for a missile");
        assert_eq!(quality(5_000.0, 0.0, 20.0), 0.0, "too far");
        assert_eq!(quality(1_800.0, 0.0, -100.0), 0.0, "falling behind: we're shaking it");
    }

    #[test]
    fn stress_builds_under_pressure_and_decays_after() {
        let mut s = 0.0;
        for _ in 0..(STRESS_BUILD_TIME / 2.0 / DT) as usize {
            s = update_stress(s, 1.0, DT);
        }
        assert!((s - 0.5).abs() < 0.02, "half built after half the build time: {s}");
        // a weaker position builds it more slowly
        let mut weak = 0.0;
        for _ in 0..(STRESS_BUILD_TIME / 2.0 / DT) as usize {
            weak = update_stress(weak, 0.3, DT);
        }
        assert!(weak < s * 0.4);
        for _ in 0..(STRESS_BUILD_TIME / DT) as usize {
            s = update_stress(s, 1.0, DT);
        }
        assert_eq!(s, 1.0, "capped");
        for _ in 0..(STRESS_DECAY_TIME / 2.0 / DT) as usize {
            s = update_stress(s, 0.0, DT);
        }
        assert!((s - 0.5).abs() < 0.02, "half gone after half the decay time: {s}");
        for _ in 0..(STRESS_DECAY_TIME / DT) as usize {
            s = update_stress(s, 0.0, DT);
        }
        assert_eq!(s, 0.0);
    }

    /// Chance of panicking within `secs` of a perfect pounce starting.
    fn panic_odds(skill: f32, secs: f32) -> f32 {
        let (mut s, mut survive) = (0.0, 1.0);
        for _ in 0..(secs / DT) as usize {
            s = update_stress(s, 1.0, DT);
            survive *= 1.0 - panic_rate(s, skill) * DT;
        }
        1.0 - survive
    }

    #[test]
    fn rookies_crack_sooner_than_aces() {
        for skill in [0.55, 0.75, 1.0] {
            println!(
                "skill {skill}: panic within 3 s {:.0}%, 6 s {:.0}%, 10 s {:.0}%",
                panic_odds(skill, 3.0) * 100.0,
                panic_odds(skill, 6.0) * 100.0,
                panic_odds(skill, 10.0) * 100.0
            );
        }
        assert_eq!(panic_rate(0.0, 0.55), 0.0, "no stress, no panic");
        assert!(panic_odds(0.55, 6.0) > 0.5, "rookies usually crack under a held pounce");
        assert!(panic_odds(1.0, 6.0) < 0.15, "aces rarely do");
        assert!(panic_odds(0.55, 3.0) > panic_odds(0.75, 3.0));
    }
}
