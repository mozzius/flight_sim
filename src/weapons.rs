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

const MISSILE_THRUST: f32 = 300.0; // m/s^2, boost phase
const MISSILE_BURN: f32 = 3.0;
/// After the boost, a weaker sustainer keeps it fast (and smoking) out to here.
const MISSILE_SUSTAIN: f32 = 10.0;
const MISSILE_SUSTAIN_THRUST: f32 = 50.0;
const MISSILE_LIFE: f32 = 16.0;
const MISSILE_DRAG: f32 = 2.6e-4;
/// Proportional navigation gain.
const MISSILE_NAV_GAIN: f32 = 3.0;
/// Peak lateral acceleration (G) at full dynamic pressure, fresh off the rail.
const MISSILE_MAX_G: f32 = 45.0;
/// How quickly the missile's airframe follows its guidance commands (s),
/// fresh and at the end of its life. This lag is what lets a well-timed
/// last-second break make it overshoot, and it grows as the missile tires.
const MISSILE_AUTOPILOT_LAG: (f32, f32) = (0.22, 0.9);
/// Energy cost of turning: forward deceleration per unit of lateral accel.
/// Augmented PN strength (0 = plain PN, 0.5 = classic APN).
const MISSILE_APN: f32 = 0.5;
/// How long the seeker's tracker takes to notice the target's acceleration (s).
/// (fresh, tired), like the autopilot lag.
const MISSILE_TRACKER_LAG: (f32, f32) = (0.5, 1.1);
const MISSILE_TURN_DRAG: f32 = 0.05;
const MISSILE_FUSE: f32 = 14.0;
const MISSILE_DAMAGE: f32 = 95.0;
/// How far off its nose the seeker can look before it loses the target.
const SEEKER_GIMBAL: f32 = 0.87; // ~50 degrees

pub const MAX_MISSILES: u32 = 6;
/// A jet only carries a few flares, so wasting them early costs you later.
pub const MAX_FLARES: u32 = 4;
/// Flares per salvo (one press, or every [`FLARE_INTERVAL`] while held).
pub const FLARE_SALVO: u32 = 2;
/// Seconds between salvos while the flare button is held.
const FLARE_INTERVAL: f32 = 0.4;
/// Seconds to regenerate one missile.
pub const MISSILE_RELOAD_TIME: f32 = 12.0;
/// Seconds to regenerate one flare (while below [`MAX_FLARES`]).
pub const FLARE_RELOAD_TIME: f32 = 8.0;
pub const MAX_GUN_AMMO: u32 = 600;
/// Tracer rounds regained per second while the trigger is released.
const GUN_RELOAD_RATE: f32 = 30.0;
/// The gun only starts reloading after this long off the trigger (s).
const GUN_RELOAD_DELAY: f32 = 1.5;

pub const LOCK_CONE: f32 = 0.6; // radians off the nose (~34°)
pub const LOCK_RANGE: f32 = 5_500.0;
const LOCK_TIME: f32 = 1.2;

pub struct WeaponsPlugin;

impl Plugin for WeaponsPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<HitConfirmed>()
            .add_systems(Startup, setup_assets)
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

/// Someone's bullet or missile hurt an aircraft (for the hit marker).
#[derive(Message, Clone, Copy)]
pub struct HitConfirmed {
    pub attacker: Entity,
}

/// Weapon loadout and the pilot's intents for this tick.
#[derive(Component)]
pub struct Weapons {
    pub gun_ammo: u32,
    pub missiles: u32,
    pub flares: u32,
    /// Progress (0..1) toward the next missile being reloaded.
    pub missile_reload: f32,
    /// Progress (0..1) toward the next flare being reloaded.
    pub flare_reload: f32,
    gun_reload: f32,
    gun_idle: f32,
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
            gun_ammo: MAX_GUN_AMMO,
            missiles: MAX_MISSILES,
            flares: MAX_FLARES,
            missile_reload: 0.0,
            flare_reload: 0.0,
            gun_reload: 0.0,
            gun_idle: 0.0,
            gun_cooldown: 0.0,
            missile_cooldown: 0.0,
            flare_cooldown: 0.0,
            rail: false,
            trigger_gun: false,
            fire_missile: false,
            drop_flares: false,
        }
    }

    /// Back to a full load of flares (a cleared challenge wave).
    pub fn refill_flares(&mut self) {
        self.flares = MAX_FLARES;
        self.flare_reload = 0.0;
    }

    /// One flare every [`FLARE_RELOAD_TIME`] while below [`MAX_FLARES`].
    fn regen_flares(&mut self, dt: f32) {
        if self.flares < MAX_FLARES {
            self.flare_reload += dt / FLARE_RELOAD_TIME;
            if self.flare_reload >= 1.0 {
                self.flare_reload -= 1.0;
                self.flares += 1;
            }
        }
        if self.flares >= MAX_FLARES {
            self.flare_reload = 0.0;
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
    /// Lateral acceleration the airframe is currently pulling (lags the command).
    pub lateral: Vec3,
    /// Target velocity seen last step, to estimate how hard it's turning.
    pub last_target_velocity: Option<Vec3>,
    /// Filtered estimate of the target's acceleration (the seeker's tracker
    /// takes time to notice a sudden break: that's the window to dodge in).
    pub target_accel: Vec3,
}

impl Missile {
    /// Boost phase: full thrust and thick smoke.
    pub fn motor_burning(&self) -> bool {
        self.age < MISSILE_BURN
    }

    /// Boost or sustainer still lit.
    pub fn motor_lit(&self) -> bool {
        self.age < MISSILE_SUSTAIN
    }

    /// How agile the missile still is (1 fresh, falling to 0 late in its
    /// flight): it can pull hard right after launch but bleeds energy and
    /// control authority the longer it flies.
    pub fn agility(&self) -> f32 {
        ((-(self.age - 1.5).max(0.0) / 4.0).exp() - 0.1).max(0.0) / 0.9
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
    // a tap drops one salvo; holding the button keeps them coming
    weapons.drop_flares = std::mem::take(&mut actions.drop_flares) || actions.holding_flares;
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
        // missiles and flares regenerate over time
        if weapons.missiles < MAX_MISSILES {
            weapons.missile_reload += dt / MISSILE_RELOAD_TIME;
            if weapons.missile_reload >= 1.0 {
                weapons.missile_reload = 0.0;
                weapons.missiles += 1;
            }
        } else {
            weapons.missile_reload = 0.0;
        }
        weapons.regen_flares(dt);
        // the gun reloads once you let go of the trigger for a moment
        if weapons.trigger_gun {
            weapons.gun_idle = 0.0;
        } else {
            weapons.gun_idle += dt;
            if weapons.gun_idle > GUN_RELOAD_DELAY && weapons.gun_ammo < MAX_GUN_AMMO {
                weapons.gun_reload += dt * GUN_RELOAD_RATE;
                let whole = weapons.gun_reload.floor();
                weapons.gun_reload -= whole;
                weapons.gun_ammo = (weapons.gun_ammo + whole as u32).min(MAX_GUN_AMMO);
            }
        }
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
                        lateral: Vec3::ZERO,
                        last_target_velocity: None,
                        target_accel: Vec3::ZERO,
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
            weapons.flare_cooldown = FLARE_INTERVAL;
            let mut decoy = None;
            for i in 0..FLARE_SALVO {
                if weapons.flares == 0 {
                    break;
                }
                weapons.flares -= 1;
                let kick = transform.rotation
                    * Vec3::new(if i % 2 == 0 { 25.0 } else { -25.0 }, -15.0, 10.0 + i as f32 * 16.0);
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
    mut hits: MessageWriter<HitConfirmed>,
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
                hits.write(HitConfirmed { attacker: bullet.owner });
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
    mut hits: MessageWriter<HitConfirmed>,
) {
    let dt = time.delta_secs();
    for (entity, mut missile, mut transform) in &mut missiles {
        missile.age += dt;
        let pos = transform.translation;

        let target_state = missile.target.and_then(|t| {
            aircraft
                .get(t)
                .ok()
                .map(|(_, tt, fs, _, _)| (tt.translation, fs.velocity))
                .or_else(|| flares.get(t).ok().map(|(ft, f)| (ft.translation, f.velocity)))
        });
        let (accel, lost) = missile_guidance(&mut missile, pos, target_state, dt);
        if lost {
            missile.target = None;
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
                    hits.write(HitConfirmed { attacker: missile.owner });
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

/// One step of missile flight: motor, drag, gravity and proportional
/// navigation toward `target` (position, velocity). Returns the acceleration
/// to apply and whether the seeker lost the target.
pub fn missile_guidance(
    missile: &mut Missile,
    pos: Vec3,
    target: Option<(Vec3, Vec3)>,
    dt: f32,
) -> (Vec3, bool) {
    let speed = missile.velocity.length();
    let dir = missile.velocity.normalize_or(Vec3::NEG_Z);

    // motor, drag, gravity
    let mut accel = Vec3::NEG_Y * GRAVITY - missile.velocity * speed * MISSILE_DRAG;
    if missile.motor_burning() && missile.age > 0.25 {
        accel += dir * MISSILE_THRUST;
    } else if missile.motor_lit() {
        accel += dir * MISSILE_SUSTAIN_THRUST;
    }

    // guidance command
    let mut lost = false;
    let mut command = Vec3::ZERO;
    if missile.age > 0.25 {
        match target {
            Some((tpos, tvel)) => {
                let los = tpos - pos;
                let los_hat = los.normalize_or_zero();
                if los_hat.angle_between(dir) > SEEKER_GIMBAL {
                    lost = true; // target left the seeker's field of view
                } else {
                    let rel_v = tvel - missile.velocity;
                    let omega = los.cross(rel_v) / los.length_squared().max(1.0);
                    let closing = -rel_v.dot(los_hat);
                    command = MISSILE_NAV_GAIN * closing.max(100.0) * omega.cross(los_hat)
                        + Vec3::Y * GRAVITY;
                    // augmented PN: lead a target that's pulling a sustained turn,
                    // so turning early doesn't help; only a sharp late break does
                    if let Some(last) = missile.last_target_velocity {
                        let measured = (tvel - last) / dt.max(1e-4);
                        let (fresh, tired) = MISSILE_TRACKER_LAG;
                        let lag = tired + (fresh - tired) * missile.agility();
                        let k = 1.0 - (-dt / lag).exp();
                        missile.target_accel += (measured - missile.target_accel) * k;
                        let a = missile.target_accel;
                        let perp = a - los_hat * a.dot(los_hat);
                        command += perp * (MISSILE_NAV_GAIN * MISSILE_APN);
                    }
                    missile.last_target_velocity = Some(tvel);
                }
            }
            None => lost = true,
        }
    }
    // only lateral, limited by the lift available at this speed
    command -= dir * command.dot(dir);
    let lift = (speed / 600.0).powi(2).min(1.0);
    let agility = missile.agility();
    let max_g = MISSILE_MAX_G * (0.25 + 0.75 * agility);
    let command = command.clamp_length_max(max_g * GRAVITY * lift);

    // the airframe follows the command with a lag
    let (fresh, tired) = MISSILE_AUTOPILOT_LAG;
    let lag = tired + (fresh - tired) * agility;
    let k = 1.0 - (-dt / lag).exp();
    missile.lateral += (command - missile.lateral) * k;
    missile.lateral -= dir * missile.lateral.dot(dir);
    accel += missile.lateral;
    // turning costs energy
    accel -= dir * missile.lateral.length() * MISSILE_TURN_DRAG;

    (accel, lost)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// How the simulated target flies.
    #[derive(Clone, Copy, Debug)]
    enum Evasion {
        Straight,
        /// Max-G break perpendicular to the missile, starting when the
        /// missile is this many seconds from impact.
        BreakAt(f32),
    }

    /// Fly one engagement; returns the closest approach in metres.
    fn engage(range: f32, aspect_deg: f32, evasion: Evasion, break_up: bool) -> f32 {
        let dt = 1.0 / 60.0;
        let target_speed = 270.0;
        let break_g = 8.0 * GRAVITY;
        // target flies -Z; the launcher sits `range` away at `aspect` from the target's tail
        let mut tpos = Vec3::new(0.0, 3_000.0, 0.0);
        let mut tvel = Vec3::NEG_Z * target_speed;
        let a = aspect_deg.to_radians();
        let mpos0 = tpos + Vec3::new(a.sin(), 0.0, a.cos()) * range;
        let mut mpos = mpos0;
        let launch_dir = (tpos - mpos).normalize();
        let mut missile = Missile {
            velocity: launch_dir * 270.0,
            target: None,
            owner: Entity::PLACEHOLDER,
            team: Team::Red,
            age: 0.0,
            lateral: Vec3::ZERO,
            last_target_velocity: None,
            target_accel: Vec3::ZERO,
        };
        let mut breaking: Option<Vec3> = None;
        let mut closest = f32::MAX;
        for _ in 0..(MISSILE_LIFE / dt) as usize {
            missile.age += dt;
            let (accel, lost) = missile_guidance(&mut missile, mpos, Some((tpos, tvel)), dt);
            if lost {
                // keeps flying, unguided
            }
            missile.velocity += accel * dt;
            let next = mpos + missile.velocity * dt;
            closest = closest.min(segment_sphere_distance(mpos, next, tpos));
            mpos = next;

            // target
            let to_missile = mpos - tpos;
            let closing = (missile.velocity - tvel).dot(-to_missile.normalize_or_zero()).max(1.0);
            let tti = to_missile.length() / closing;
            if let Evasion::BreakAt(t) = evasion {
                if breaking.is_none() && tti < t {
                    // turn across the missile's line of sight
                    let side = to_missile.cross(Vec3::Y).normalize_or(Vec3::X);
                    breaking = Some(if break_up { Vec3::Y } else { side });
                }
            }
            if let Some(dir) = breaking {
                let lateral = (dir - tvel.normalize() * dir.dot(tvel.normalize())).normalize_or_zero();
                tvel = (tvel + lateral * break_g * dt).normalize() * target_speed;
            }
            tpos += tvel * dt;
            if closest < 1.0 || missile.age > 2.0 && (mpos - tpos).dot(missile.velocity) > 400.0 {
                break; // hit, or flew well past
            }
        }
        closest
    }

    const SHOTS: [(f32, f32); 6] = [
        (2_500.0, 0.0),   // tail chase
        (4_000.0, 0.0),
        (2_500.0, 60.0),  // off the tail quarter
        (3_500.0, 90.0),  // beam
        (3_000.0, 150.0), // front quarter
        (5_000.0, 180.0), // head on
    ];

    /// Close-range shots, fired while the missile is still fresh.
    const CLOSE_SHOTS: [(f32, f32); 3] = [(1_200.0, 0.0), (1_400.0, 60.0), (1_800.0, 120.0)];

    fn hit_rate(evasion: Evasion, up: bool) -> (usize, Vec<f32>) {
        let misses: Vec<f32> = SHOTS.iter().map(|(r, a)| engage(*r, *a, evasion, up)).collect();
        (misses.iter().filter(|m| **m < MISSILE_FUSE).count(), misses)
    }

    #[test]
    fn flares_regenerate_one_at_a_time_up_to_the_cap() {
        let mut w = Weapons::fighter();
        assert_eq!(w.flares, MAX_FLARES);
        w.flares = 0;
        let dt = 1.0 / 60.0;
        let mut t = 0.0;
        let mut first = None;
        while w.flares < MAX_FLARES {
            let before = w.flares;
            w.regen_flares(dt);
            t += dt;
            if before == 0 && w.flares == 1 {
                first = Some(t);
            }
            assert!(t < 100.0);
        }
        let first = first.unwrap();
        assert!((first - FLARE_RELOAD_TIME).abs() < 0.05, "first flare after {first}s");
        assert!((t - FLARE_RELOAD_TIME * MAX_FLARES as f32).abs() < 0.1, "full after {t}s");
        // and it stops at the cap with no banked progress
        for _ in 0..600 {
            w.regen_flares(dt);
        }
        assert_eq!(w.flares, MAX_FLARES);
        assert_eq!(w.flare_reload, 0.0);
        w.flares = 1;
        w.flare_reload = 0.5;
        w.refill_flares();
        assert_eq!((w.flares, w.flare_reload), (MAX_FLARES, 0.0));
    }

    #[test]
    fn missiles_hit_targets_that_fly_straight() {
        let (hits, misses) = hit_rate(Evasion::Straight, false);
        println!("straight: {hits}/{} hit, miss distances {misses:.0?}", SHOTS.len());
        assert!(hits >= SHOTS.len() - 1, "a non-manoeuvring target should be hit");
    }

    #[test]
    fn a_well_timed_break_usually_defeats_a_missile() {
        // breaks 1-1.5 s before impact, sideways and up, over every launch geometry
        let mut total_hits = 0;
        let mut total = 0;
        for t in [1.0, 1.5] {
            for up in [false, true] {
                let (hits, misses) = hit_rate(Evasion::BreakAt(t), up);
                println!("break {} at {t}s: {hits}/{} hit, miss distances {misses:.0?}", if up { "up" } else { "sideways" }, SHOTS.len());
                total_hits += hits;
                total += SHOTS.len() - 1; // the 4 km tail chase is out of range anyway
            }
        }
        println!("well-timed breaks: {total_hits}/{total} hit");
        assert!(total_hits * 100 <= total * 45, "a well-timed break should work more often than not");
    }

    /// Not a pass/fail test: prints how the evasion window looks.
    #[test]
    fn evasion_window_report() {
        for t in [0.3, 0.6, 1.0, 1.5, 2.0, 2.5, 3.0, 4.0, 6.0] {
            let (hits, _) = hit_rate(Evasion::BreakAt(t), false);
            println!("window: break {t:.1}s before impact -> {hits}/{} hit", SHOTS.len());
        }
    }

    #[test]
    fn close_range_shots_are_hard_to_dodge() {
        let mut hits = 0;
        let mut total = 0;
        for t in [0.6, 1.0, 1.5, 2.0] {
            for up in [false, true] {
                for (r, a) in CLOSE_SHOTS {
                    let miss = engage(r, a, Evasion::BreakAt(t), up);
                    total += 1;
                    hits += (miss < MISSILE_FUSE) as usize;
                }
            }
        }
        println!("close range with a break: {hits}/{total} hit");
        assert!(hits * 2 >= total, "a fresh missile should usually get through: {hits}/{total}");
    }

    #[test]
    fn breaking_too_early_does_not_work() {
        let (hits, misses) = hit_rate(Evasion::BreakAt(6.0), false);
        println!("break at 6s: {hits}/{} hit, miss distances {misses:.0?}", SHOTS.len());
        assert!(hits >= SHOTS.len() / 2, "an early break gives the missile time to correct: {hits} hits");
    }
}
