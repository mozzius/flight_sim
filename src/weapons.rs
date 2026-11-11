//! Guns, missiles, flares, lock-on, and damage. Shared by the player and AI:
//! pilots set intents on [`Weapons`] and choose a [`Targeting::target`], and
//! the systems here do the rest.

use bevy::prelude::*;
use bevy_transform_interpolation::prelude::*;
use rand::RngExt;

use crate::{
    controls::PlayerActions,
    flight::{Aircraft, Disabled, FlightState, FlightSystems, GRAVITY},
    game::{Explosion, Health, Player, Team},
    world::Terrain,
};

pub const BULLET_SPEED: f32 = 1_030.0;
const BULLET_LIFE: f32 = 2.2;
const BULLET_DAMAGE: f32 = 5.0;
const GUN_RATE: f32 = 25.0; // tracer entities per second (each stands for a few rounds)

const MISSILE_THRUST: f32 = 280.0; // m/s^2
const MISSILE_BURN: f32 = 3.5;
const MISSILE_LIFE: f32 = 16.0;
const MISSILE_DRAG: f32 = 2.6e-4;
const MISSILE_NAV_GAIN: f32 = 4.0;
const MISSILE_MAX_G: f32 = 35.0;
const MISSILE_FUSE: f32 = 16.0;
const MISSILE_DAMAGE: f32 = 95.0;

pub const LOCK_CONE: f32 = 0.6; // radians off the nose (~34°)
pub const LOCK_RANGE: f32 = 5_500.0;
const LOCK_TIME: f32 = 1.2;
const SEEKER_GIMBAL: f32 = 1.05;

pub struct WeaponsPlugin;

impl Plugin for WeaponsPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, setup_assets)
            .add_systems(
                FixedUpdate,
                (
                    player_weapon_intents,
                    update_locks,
                    fire_weapons,
                    move_bullets,
                    guide_missiles,
                    move_flares,
                )
                    .chain()
                    .in_set(FlightSystems::Combat),
            );
    }
}

/// Weapon loadout and the pilot's intents for this tick.
#[derive(Component)]
pub struct Weapons {
    pub gun_ammo: u32,
    pub missiles: u32,
    pub flares: u32,
    gun_cooldown: f32,
    missile_cooldown: f32,
    flare_cooldown: f32,
    /// Alternate missile rails.
    rail: bool,
    // intents, set by the pilot every tick
    pub trigger_gun: bool,
    pub fire_missile: bool,
    pub drop_flares: bool,
}

impl Weapons {
    pub fn fighter() -> Self {
        Weapons {
            gun_ammo: 600,
            missiles: 6,
            flares: 40,
            gun_cooldown: 0.0,
            missile_cooldown: 0.0,
            flare_cooldown: 0.0,
            rail: false,
            trigger_gun: false,
            fire_missile: false,
            drop_flares: false,
        }
    }
}

/// Current target and missile lock progress (0..1, locked at 1).
#[derive(Component, Default)]
pub struct Targeting {
    pub target: Option<Entity>,
    pub lock: f32,
}

impl Targeting {
    pub fn locked(&self) -> bool {
        self.lock >= 1.0
    }
}

#[derive(Component)]
pub struct Bullet {
    velocity: Vec3,
    life: f32,
    owner: Entity,
    team: Team,
}

#[derive(Component)]
pub struct Missile {
    pub velocity: Vec3,
    pub target: Option<Entity>,
    pub owner: Entity,
    pub team: Team,
    pub age: f32,
}

impl Missile {
    pub fn motor_burning(&self) -> bool {
        self.age < MISSILE_BURN
    }
}

#[derive(Component)]
pub struct Flare {
    velocity: Vec3,
    life: f32,
}

#[derive(Resource)]
struct ProjectileAssets {
    tracer_mesh: Handle<Mesh>,
    tracer_material: Handle<StandardMaterial>,
    missile_mesh: Handle<Mesh>,
    missile_material: Handle<StandardMaterial>,
    flame_mesh: Handle<Mesh>,
    flame_material: Handle<StandardMaterial>,
    flare_mesh: Handle<Mesh>,
    flare_material: Handle<StandardMaterial>,
}

fn setup_assets(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let glow = |materials: &mut Assets<StandardMaterial>, c: LinearRgba| {
        // unlit: base_color *is* the output, so HDR values here glow
        materials.add(StandardMaterial {
            base_color: c.into(),
            unlit: true,
            ..default()
        })
    };
    commands.insert_resource(ProjectileAssets {
        // long thin capsule along -Z; long enough to read as a tracer streak
        tracer_mesh: meshes.add(Capsule3d::new(0.12, 9.0).mesh().latitudes(4).longitudes(6)),
        tracer_material: glow(&mut materials, LinearRgba::rgb(25.0, 12.0, 2.5)),
        missile_mesh: meshes.add(Capsule3d::new(0.13, 3.4).mesh().latitudes(4).longitudes(8)),
        missile_material: materials.add(StandardMaterial {
            base_color: Color::srgb(0.85, 0.85, 0.82),
            metallic: 0.3,
            perceptual_roughness: 0.4,
            ..default()
        }),
        flame_mesh: meshes.add(Sphere::new(0.35).mesh().ico(1).unwrap()),
        flame_material: glow(&mut materials, LinearRgba::rgb(30.0, 14.0, 4.0)),
        flare_mesh: meshes.add(Sphere::new(0.6).mesh().ico(1).unwrap()),
        flare_material: glow(&mut materials, LinearRgba::rgb(40.0, 30.0, 18.0)),
    });
}

#[allow(clippy::type_complexity)]
fn player_weapon_intents(
    mut actions: ResMut<PlayerActions>,
    mode: Res<crate::controls::ControlMode>,
    aim: Res<crate::controls::MouseAim>,
    mut player: Query<
        (Entity, &Transform, &Team, &mut Weapons, &mut Targeting),
        (With<Player>, Without<crate::ai::Pilot>),
    >,
    candidates: Query<(Entity, &Transform, &Team), (With<Aircraft>, Without<Disabled>)>,
) {
    let Ok((me, transform, team, mut weapons, mut targeting)) = player.single_mut() else {
        // still consume one-shot actions so they don't fire on respawn
        actions.fire_missile = false;
        actions.drop_flares = false;
        actions.next_target = false;
        actions.previous_target = false;
        actions.target_under_crosshair = false;
        return;
    };
    weapons.trigger_gun = actions.firing_gun;
    weapons.fire_missile = std::mem::take(&mut actions.fire_missile);
    weapons.drop_flares = std::mem::take(&mut actions.drop_flares);
    let next = std::mem::take(&mut actions.next_target);
    let previous = std::mem::take(&mut actions.previous_target);
    let under_crosshair = std::mem::take(&mut actions.target_under_crosshair);

    let hostiles: Vec<(Entity, Vec3)> = candidates
        .iter()
        .filter(|(e, _, t)| *e != me && **t != *team)
        .map(|(e, t, _)| (e, t.translation - transform.translation))
        .collect();
    if hostiles.is_empty() {
        targeting.target = None;
        return;
    }
    let crosshair = match *mode {
        crate::controls::ControlMode::MouseAim => aim.dir,
        crate::controls::ControlMode::Direct => transform.forward().as_vec3(),
    };
    let nearest_crosshair = |list: &[(Entity, Vec3)]| {
        list.iter()
            .min_by(|a, b| {
                // mostly angle from the crosshair, a little distance
                let score = |v: Vec3| v.normalize_or_zero().angle_between(crosshair) + v.length() / 40_000.0;
                score(a.1).total_cmp(&score(b.1))
            })
            .map(|(e, _)| *e)
    };

    let current_valid = targeting.target.is_some_and(|t| hostiles.iter().any(|(e, _)| *e == t));
    let new_target = if under_crosshair || !current_valid {
        nearest_crosshair(&hostiles)
    } else if next || previous {
        // a stable sweep: sort by bearing around the nose, left to right
        let inv = transform.rotation.inverse();
        let mut by_bearing: Vec<(Entity, f32)> = hostiles
            .iter()
            .map(|(e, v)| {
                let local = inv * *v;
                (*e, local.x.atan2(-local.z))
            })
            .collect();
        by_bearing.sort_by(|a, b| a.1.total_cmp(&b.1));
        let idx = by_bearing
            .iter()
            .position(|(e, _)| Some(*e) == targeting.target)
            .unwrap_or(0);
        let n = by_bearing.len();
        let idx = if next { (idx + 1) % n } else { (idx + n - 1) % n };
        Some(by_bearing[idx].0)
    } else {
        targeting.target
    };
    if new_target != targeting.target {
        targeting.target = new_target;
        targeting.lock = 0.0;
    }
}

fn update_locks(
    time: Res<Time>,
    mut seekers: Query<(&Transform, &mut Targeting, Has<Disabled>)>,
    targets: Query<&Transform, (With<Aircraft>, Without<Disabled>)>,
) {
    let dt = time.delta_secs();
    for (transform, mut targeting, disabled) in &mut seekers {
        let in_cone = !disabled
            && targeting.target.and_then(|t| targets.get(t).ok()).is_some_and(|t| {
                let to = t.translation - transform.translation;
                to.length() < LOCK_RANGE
                    && to.normalize_or_zero().angle_between(transform.forward().as_vec3())
                        < LOCK_CONE
            });
        targeting.lock = if in_cone {
            (targeting.lock + dt / LOCK_TIME).min(1.0)
        } else {
            (targeting.lock - dt * 2.0).max(0.0)
        };
    }
}

#[allow(clippy::type_complexity)]
fn fire_weapons(
    mut commands: Commands,
    time: Res<Time>,
    assets: Res<ProjectileAssets>,
    mut shooters: Query<(
        Entity,
        &Transform,
        &FlightState,
        &Team,
        &mut Weapons,
        Option<&Targeting>,
        Has<Disabled>,
    )>,
    mut missiles: Query<&mut Missile>,
) {
    let dt = time.delta_secs();
    let mut rng = rand::rng();
    for (entity, transform, state, team, mut weapons, targeting, disabled) in &mut shooters {
        weapons.gun_cooldown -= dt;
        weapons.missile_cooldown -= dt;
        weapons.flare_cooldown -= dt;
        if disabled {
            continue;
        }
        let fwd = transform.forward().as_vec3();

        // ---- gun ----
        if weapons.trigger_gun && weapons.gun_ammo > 0 {
            while weapons.gun_cooldown <= 0.0 && weapons.gun_ammo > 0 {
                weapons.gun_cooldown += 1.0 / GUN_RATE;
                weapons.gun_ammo -= 1;
                let spread = Vec3::new(
                    rng.random_range(-1.0..1.0),
                    rng.random_range(-1.0..1.0),
                    rng.random_range(-1.0..1.0),
                ) * 0.004;
                let dir = (fwd + spread).normalize();
                let velocity = state.velocity + dir * BULLET_SPEED;
                // stagger spawn positions along the path so bursts don't clump
                let muzzle = transform.transform_point(Vec3::new(0.9, 0.4, -7.0))
                    + velocity * dt * rng.random_range(0.0..1.0);
                commands.spawn((
                    Bullet {
                        velocity,
                        life: BULLET_LIFE,
                        owner: entity,
                        team: *team,
                    },
                    Mesh3d(assets.tracer_mesh.clone()),
                    MeshMaterial3d(assets.tracer_material.clone()),
                    Transform::from_translation(muzzle)
                        .looking_to(velocity, Vec3::Y)
                        * Transform::from_rotation(Quat::from_rotation_x(
                            std::f32::consts::FRAC_PI_2,
                        )),
                    TransformInterpolation,
                ));
            }
        } else {
            weapons.gun_cooldown = weapons.gun_cooldown.max(0.0);
        }

        // ---- missile ----
        if std::mem::take(&mut weapons.fire_missile)
            && weapons.missiles > 0
            && weapons.missile_cooldown <= 0.0
        {
            weapons.missiles -= 1;
            weapons.missile_cooldown = 0.8;
            weapons.rail = !weapons.rail;
            let side = if weapons.rail { 2.2 } else { -2.2 };
            let target = targeting.filter(|t| t.locked()).and_then(|t| t.target);
            let pos = transform.transform_point(Vec3::new(side, -1.2, -1.0));
            commands
                .spawn((
                    Missile {
                        velocity: state.velocity - transform.up().as_vec3() * 8.0,
                        target,
                        owner: entity,
                        team: *team,
                        age: 0.0,
                    },
                    Mesh3d(assets.missile_mesh.clone()),
                    MeshMaterial3d(assets.missile_material.clone()),
                    Transform::from_translation(pos).with_rotation(
                        transform.rotation * Quat::from_rotation_x(std::f32::consts::FRAC_PI_2),
                    ),
                    TransformInterpolation,
                ))
                .with_child((
                    MissileFlame,
                    Mesh3d(assets.flame_mesh.clone()),
                    MeshMaterial3d(assets.flame_material.clone()),
                    Transform::from_xyz(0.0, -1.9, 0.0).with_scale(Vec3::new(1.0, 2.5, 1.0)),
                ));
        }

        // ---- flares ----
        if std::mem::take(&mut weapons.drop_flares)
            && weapons.flares > 0
            && weapons.flare_cooldown <= 0.0
        {
            weapons.flare_cooldown = 0.6;
            let mut decoy = None;
            for i in 0..4 {
                if weapons.flares == 0 {
                    break;
                }
                weapons.flares -= 1;
                let kick = transform.rotation
                    * Vec3::new(if i % 2 == 0 { 25.0 } else { -25.0 }, -15.0, 10.0 + i as f32 * 8.0);
                let flare = commands
                    .spawn((
                        Flare {
                            velocity: state.velocity * 0.7 + kick,
                            life: 3.5,
                        },
                        Mesh3d(assets.flare_mesh.clone()),
                        MeshMaterial3d(assets.flare_material.clone()),
                        Transform::from_translation(transform.translation),
                        TransformInterpolation,
                    ))
                    .id();
                decoy.get_or_insert(flare);
            }
            // each salvo has a decent chance to pull each incoming missile off
            if let Some(flare) = decoy {
                for mut m in &mut missiles {
                    if m.target == Some(entity) && rng.random_bool(0.55) {
                        m.target = Some(flare);
                    }
                }
            }
        }
    }
}

#[derive(Component)]
pub struct MissileFlame;

fn move_bullets(
    mut commands: Commands,
    time: Res<Time>,
    terrain: Res<Terrain>,
    mut bullets: Query<(Entity, &mut Bullet, &mut Transform), Without<Aircraft>>,
    mut aircraft: Query<(Entity, &Transform, &Aircraft, &Team, &mut Health)>,
    mut explosions: MessageWriter<Explosion>,
) {
    let dt = time.delta_secs();
    for (entity, mut bullet, mut transform) in &mut bullets {
        bullet.life -= dt;
        bullet.velocity.y -= GRAVITY * dt;
        let from = transform.translation;
        let to = from + bullet.velocity * dt;

        let mut hit = false;
        for (target, t, ac, team, mut health) in &mut aircraft {
            if target == bullet.owner || *team == bullet.team {
                continue;
            }
            if segment_sphere_distance(from, to, t.translation) < ac.radius {
                health.current -= BULLET_DAMAGE;
                health.last_attacker = Some(bullet.owner);
                explosions.write(Explosion {
                    position: t.translation + (from - t.translation).normalize_or_zero() * 3.0,
                    size: 1.5,
                });
                hit = true;
                break;
            }
        }
        if !hit {
            if let Some(p) = terrain.segment_hit(from, to) {
                explosions.write(Explosion {
                    position: p,
                    size: 1.0,
                });
                hit = true;
            }
        }

        if hit || bullet.life <= 0.0 {
            commands.entity(entity).despawn();
        } else {
            transform.translation = to;
            transform.rotation = Transform::IDENTITY
                .looking_to(bullet.velocity, Vec3::Y)
                .rotation
                * Quat::from_rotation_x(std::f32::consts::FRAC_PI_2);
        }
    }
}

#[allow(clippy::type_complexity)]
fn guide_missiles(
    mut commands: Commands,
    time: Res<Time>,
    terrain: Res<Terrain>,
    mut missiles: Query<(Entity, &mut Missile, &mut Transform), Without<Aircraft>>,
    mut aircraft: Query<(Entity, &Transform, &FlightState, &Team, &mut Health), With<Aircraft>>,
    flares: Query<(&Transform, &Flare), (Without<Missile>, Without<Aircraft>)>,
    mut explosions: MessageWriter<Explosion>,
) {
    let dt = time.delta_secs();
    for (entity, mut missile, mut transform) in &mut missiles {
        missile.age += dt;
        let pos = transform.translation;
        let speed = missile.velocity.length();
        let dir = missile.velocity.normalize_or(Vec3::NEG_Z);

        // ---- motor, drag, gravity ----
        let mut accel = Vec3::NEG_Y * GRAVITY - missile.velocity * speed * MISSILE_DRAG;
        if missile.motor_burning() && missile.age > 0.25 {
            accel += dir * MISSILE_THRUST;
        }

        // ---- proportional navigation ----
        let target_state = missile.target.and_then(|t| {
            aircraft
                .get(t)
                .ok()
                .map(|(_, tt, fs, _, _)| (tt.translation, fs.velocity))
                .or_else(|| flares.get(t).ok().map(|(ft, f)| (ft.translation, f.velocity)))
        });
        if missile.age > 0.25 {
            if let Some((tpos, tvel)) = target_state {
                let los = tpos - pos;
                let los_hat = los.normalize_or_zero();
                if los_hat.angle_between(dir) > SEEKER_GIMBAL {
                    missile.target = None; // target left the seeker's view
                } else {
                    let rel_v = tvel - missile.velocity;
                    let omega = los.cross(rel_v) / los.length_squared().max(1.0);
                    let closing = -rel_v.dot(los_hat);
                    let mut cmd = MISSILE_NAV_GAIN * closing.max(100.0) * omega.cross(los_hat);
                    cmd += Vec3::Y * GRAVITY;
                    // only lateral acceleration, limited by available lift
                    cmd -= dir * cmd.dot(dir);
                    let lift = (speed / 500.0).powi(2).min(1.0);
                    accel += cmd.clamp_length_max(MISSILE_MAX_G * GRAVITY * lift);
                }
            } else if missile.target.is_some() {
                missile.target = None;
            }
        }

        missile.velocity += accel * dt;
        let new_pos = pos + missile.velocity * dt;

        // ---- fuse: closest approach to any enemy aircraft this step ----
        let mut detonate = false;
        for (target, t, _, team, _) in &aircraft {
            if *team == missile.team || target == missile.owner {
                continue;
            }
            if segment_sphere_distance(pos, new_pos, t.translation) < MISSILE_FUSE {
                detonate = true;
                break;
            }
        }
        let hit_ground = terrain.segment_hit(pos, new_pos).is_some();
        let expired = missile.age > MISSILE_LIFE;

        if detonate || hit_ground || expired {
            for (_, t, _, team, mut health) in &mut aircraft {
                if *team == missile.team {
                    continue;
                }
                let d = t.translation.distance(new_pos);
                if d < MISSILE_FUSE * 1.5 {
                    health.current -= MISSILE_DAMAGE * (1.0 - d / (MISSILE_FUSE * 2.0)).max(0.3);
                    health.last_attacker = Some(missile.owner);
                }
            }
            explosions.write(Explosion {
                position: new_pos,
                size: if detonate { 9.0 } else { 6.0 },
            });
            commands.entity(entity).despawn();
            continue;
        }

        transform.translation = new_pos;
        transform.rotation = Transform::IDENTITY
            .looking_to(missile.velocity, Vec3::Y)
            .rotation
            * Quat::from_rotation_x(std::f32::consts::FRAC_PI_2);
    }
}

fn move_flares(
    mut commands: Commands,
    time: Res<Time>,
    mut flares: Query<(Entity, &mut Flare, &mut Transform)>,
) {
    let dt = time.delta_secs();
    for (entity, mut flare, mut transform) in &mut flares {
        flare.life -= dt;
        flare.velocity.y -= GRAVITY * dt;
        flare.velocity *= 1.0 - 0.8 * dt;
        transform.translation += flare.velocity * dt;
        transform.scale = Vec3::splat((flare.life / 3.5).clamp(0.2, 1.0));
        if flare.life <= 0.0 {
            commands.entity(entity).despawn();
        }
    }
}

/// Distance from `p` to the segment a..b.
pub fn segment_sphere_distance(a: Vec3, b: Vec3, p: Vec3) -> f32 {
    let ab = b - a;
    let t = ((p - a).dot(ab) / ab.length_squared().max(1e-6)).clamp(0.0, 1.0);
    (a + ab * t).distance(p)
}

/// Where to aim guns to hit a target (first-order lead, with bullet drop).
pub fn gun_lead_point(
    shooter_pos: Vec3,
    shooter_vel: Vec3,
    target_pos: Vec3,
    target_vel: Vec3,
) -> Vec3 {
    let mut t = shooter_pos.distance(target_pos) / BULLET_SPEED;
    let mut aim = target_pos;
    for _ in 0..3 {
        // bullets inherit the shooter's velocity
        aim = target_pos + (target_vel - shooter_vel) * t + Vec3::Y * 0.5 * GRAVITY * t * t;
        t = shooter_pos.distance(aim) / BULLET_SPEED;
    }
    aim
}
