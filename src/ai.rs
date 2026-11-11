//! AI pilots. They fly the same flight model as the player through
//! [`steer_towards`], so they have to manage energy and can stall too.
//!
//! Priorities each tick, highest first:
//! 1. don't hit the ground or buildings
//! 2. defend against incoming missiles (beam, flares, afterburner)
//! 3. break if someone is on our tail
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
        app.add_systems(FixedUpdate, ai_pilots.in_set(FlightSystems::Pilots));
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

        let mut throttle = 0.85;
        let mut afterburner = false;
        let mut aggression = 0.6 + 0.4 * pilot.skill;

        let desired: Vec3 = if terrain_danger {
            pilot.mode = Mode::AvoidTerrain;
            afterburner = true;
            // climb away, keeping heading
            let flat = Vec3::new(fwd.x, 0.0, fwd.z).normalize_or(Vec3::NEG_Z);
            (flat + Vec3::Y * 1.2).normalize()
        } else if let Some((mpos, _)) = incoming.filter(|(mp, _)| mp.distance(pos) < 4_500.0) {
            pilot.mode = Mode::Evade;
            afterburner = true;
            aggression = 1.0;
            let to_missile = (mpos - pos).normalize_or_zero();
            if mpos.distance(pos) < 1_800.0 && rng.random_bool((dt * 4.0 * pilot.skill) as f64) {
                weapons.drop_flares = true;
            }
            // beam the missile: fly perpendicular to it, and down if there's room
            let side = to_missile.cross(Vec3::Y).normalize_or(Vec3::X);
            let side = if side.dot(fwd) > 0.0 { side } else { -side };
            let dive = if clearance > 900.0 { -0.35 } else { 0.15 };
            (side + Vec3::Y * dive).normalize()
        } else if let Some(threat) = tail_threat {
            pilot.mode = Mode::Defend;
            afterburner = speed < 260.0;
            aggression = 1.0;
            if pilot.break_timer <= 0.0 || pilot.break_dir.is_none() {
                // break hard across the attacker's nose, randomly left/right/up
                let to_threat = (threat.pos - pos).normalize_or_zero();
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
            if targeting.locked()
                && pilot.missile_cooldown <= 0.0
                && (700.0..LOCK_RANGE * 0.85).contains(&dist)
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
