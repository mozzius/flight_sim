//! Visual effects.
//!
//! - `particles`: one CPU-simulated, GPU-shaded particle system (lit smoke,
//!   fire that cools into smoke, sparks, glows, trail tubes) in a single
//!   sorted draw call.
//! - `plume`: raymarched afterburner plumes and heat haze.
//! - `textures`: procedurally baked puff atlas and noise.
//!
//! This module decides *what* to emit: explosions (scaled by size), burning
//! and damaged aircraft, wing vapour and vapour cones, contrails and wingtip
//! vortices, missile and flare smoke, muzzle flashes, impact sparks and dust.

mod particles;
mod plume;
mod textures;

use std::collections::{HashMap, VecDeque};

use bevy::{
    camera::visibility::NoFrustumCulling,
    light::NotShadowCaster,
    pbr::MaterialPlugin,
    prelude::*,
};
use rand::{Rng, RngExt};

use crate::{
    camera::MainCamera,
    flight::{Aircraft, Disabled, FlightState},
    game::{Explosion, Health},
    weapons::{Flare, Missile, MissileFlame, Weapons},
};
use particles::{Particle, ParticleMaterial, Particles, Quad};
use plume::{HazeMaterial, PlumeMaterial, PlumeParams};

pub struct EffectsPlugin;

/// The render-side effect systems (after transform propagation).
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct EffectsSystems;

impl Plugin for EffectsPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins((
            MaterialPlugin::<ParticleMaterial>::default(),
            MaterialPlugin::<PlumeMaterial>::default(),
            MaterialPlugin::<HazeMaterial>::default(),
        ))
        .init_resource::<Bursts>()
        .init_resource::<Ribbons>()
        .add_systems(Startup, (particles::setup, setup_assets).chain())
        .add_systems(
            Update,
            (
                particles::enable_depth_prepass,
                plume::install_shaders,
                attach_aircraft_effects,
                spawn_explosions,
                update_flashes,
            ),
        )
        .add_systems(
            PostUpdate,
            (
                particles::simulate,
                update_engines,
                update_missile_flames,
                aircraft_fx,
                projectile_fx,
                update_bursts,
                update_ribbons,
                particles::update_globals,
                particles::build_mesh,
            )
                .chain()
                .in_set(EffectsSystems)
                .after(TransformSystems::Propagate)
                // the particle mesh and plume materials are rewritten here: their
                // change events must go out this frame, or the render world draws
                // last frame's particles (a missile's glow trailed ~10 m behind it)
                .before(bevy::asset::AssetEventSystems),
        );
    }
}

#[derive(Resource)]
struct EffectAssets {
    plume: Handle<Mesh>,
    haze: Handle<Mesh>,
    noise: Handle<Image>,
}

fn setup_assets(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    render: Res<particles::ParticleRender>,
) {
    commands.insert_resource(EffectAssets {
        plume: meshes.add(
            Cylinder::new(plume::RADIUS, plume::HALF_LENGTH * 2.0)
                .mesh()
                .resolution(20),
        ),
        haze: meshes.add(
            Cylinder::new(plume::HAZE_RADIUS, plume::HAZE_HALF_LENGTH * 2.0)
                .mesh()
                .resolution(20),
        ),
        noise: render.noise.clone(),
    });
}

// ---------------------------------------------------------------- helpers

fn rand_dir(rng: &mut impl Rng) -> Vec3 {
    loop {
        let v = Vec3::new(
            rng.random_range(-1.0..1.0),
            rng.random_range(-1.0..1.0),
            rng.random_range(-1.0..1.0),
        );
        let l = v.length_squared();
        if l <= 1.0 && l > 0.01 {
            return v / l.sqrt();
        }
    }
}

fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Dust and soil colours for rocky, grassy mountain terrain.
fn dust_color(rng: &mut impl Rng) -> Vec3 {
    let palette = [
        Vec3::new(0.47, 0.41, 0.33), // dry earth
        Vec3::new(0.40, 0.38, 0.35), // crushed rock
        Vec3::new(0.34, 0.32, 0.24), // soil with grass
        Vec3::new(0.52, 0.47, 0.40), // pale dust
    ];
    let a = palette[rng.random_range(0..palette.len())];
    a * rng.random_range(0.85..1.1)
}

/// Emits along the path travelled since last frame at a fixed spacing, so
/// trails stay continuous at any speed and frame rate. Returns the points.
fn spaced(last: &mut Option<Vec3>, now: Vec3, spacing: f32, out: &mut Vec<Vec3>) {
    out.clear();
    let Some(prev) = *last else {
        *last = Some(now);
        out.push(now);
        return;
    };
    let d = prev.distance(now);
    if d > 400.0 {
        // teleported (respawn): restart the trail
        *last = Some(now);
        return;
    }
    let n = (d / spacing) as usize;
    for i in 1..=n {
        out.push(prev.lerp(now, (i as f32 * spacing) / d));
    }
    if n > 0 {
        *last = Some(prev.lerp(now, (n as f32 * spacing) / d));
    }
}

// ---------------------------------------------------------------- aircraft

/// Exhaust nozzle positions in aircraft-local space (metres).
// measured from the model: nose at z=-12, tail fins reach z=+8.9, nozzle discs ~5.0, exit lip ~6.6-7.0. The flame starts deep inside the nozzle (the plume shader treats the first 2.4 m as the nozzle interior) so it always emerges from within, with no gap from any angle.
// The 2D nozzle openings (from the glTF's vertices, through the model's
// transform in `game.rs`) span x 0.50..1.08 and y -0.13..0.68 inside the
// flaps, so they're centred at x ±0.79, y 0.27.
const NOZZLES: [Vec3; 2] = [Vec3::new(0.79, 0.27, 4.4), Vec3::new(-0.79, 0.27, 4.4)];
const WINGTIPS: [Vec3; 2] = [Vec3::new(7.3, 0.0, 2.0), Vec3::new(-7.3, 0.0, 2.0)];
const GUN_PORT: Vec3 = Vec3::new(0.9, 0.9, -7.5);
/// Upper wing surface (right wing) as a quad: root LE, tip LE, tip TE, root TE.
const WING: [Vec3; 4] = [
    Vec3::new(1.9, 0.55, -3.2),
    Vec3::new(6.7, 0.2, 1.1),
    Vec3::new(7.1, 0.2, 2.7),
    Vec3::new(1.9, 0.55, 4.0),
];

#[derive(Component)]
struct Engine {
    seed: f32,
    afterburner: f32,
    haze: Entity,
}

#[derive(Component)]
struct AfterburnerLight;

#[derive(Component)]
struct MuzzleLight;

#[derive(Component, Default)]
struct AircraftFx {
    smoke: Option<Vec3>,
    fire: Option<Vec3>,
    seed: f32,
}

#[allow(clippy::too_many_arguments)]
fn attach_aircraft_effects(
    mut commands: Commands,
    assets: Res<EffectAssets>,
    mut plumes: ResMut<Assets<PlumeMaterial>>,
    mut hazes: ResMut<Assets<HazeMaterial>>,
    mut ribbons: ResMut<Ribbons>,
    new: Query<Entity, Added<Aircraft>>,
) {
    let mut rng = rand::rng();
    let shape = Vec4::new(plume::HALF_LENGTH, plume::RADIUS, 0.5, 0.36);
    let along = Quat::from_rotation_x(std::f32::consts::FRAC_PI_2);
    for aircraft in &new {
        commands.entity(aircraft).insert(AircraftFx {
            seed: rng.random_range(0.0..100.0),
            ..default()
        });
        let haze = commands
            .spawn((
                Mesh3d(assets.haze.clone()),
                MeshMaterial3d(hazes.add(HazeMaterial {
                    params: PlumeParams {
                        state: Vec4::ZERO,
                        shape: Vec4::new(plume::HAZE_HALF_LENGTH, plume::HAZE_RADIUS, 0.0, 0.0),
                    },
                    noise: assets.noise.clone(),
                })),
                Transform::from_translation(Vec3::new(0.0, NOZZLES[0].y, 6.8 + plume::HAZE_HALF_LENGTH))
                    .with_rotation(along),
                NotShadowCaster,
                ChildOf(aircraft),
            ))
            .id();
        commands.entity(aircraft).with_children(|p| {
            for (i, nozzle) in NOZZLES.into_iter().enumerate() {
                p.spawn((
                    Engine {
                        seed: rng.random_range(0.0..100.0) + i as f32 * 37.0,
                        afterburner: 0.0,
                        haze,
                    },
                    Mesh3d(assets.plume.clone()),
                    MeshMaterial3d(plumes.add(PlumeMaterial {
                        params: PlumeParams {
                            state: Vec4::ZERO,
                            shape,
                        },
                        noise: assets.noise.clone(),
                    })),
                    Transform::from_translation(nozzle + Vec3::Z * plume::HALF_LENGTH)
                        .with_rotation(along),
                    NotShadowCaster,
                    NoFrustumCulling,
                ));
            }
            // the afterburner lights up the tail and anything close behind
            p.spawn((
                AfterburnerLight,
                PointLight {
                    intensity: 0.0,
                    range: 60.0,
                    color: Color::srgb(1.0, 0.6, 0.3),
                    shadow_maps_enabled: false,
                    ..default()
                },
                Transform::from_xyz(0.0, 0.3, 10.0),
            ));
            p.spawn((
                MuzzleLight,
                PointLight {
                    intensity: 0.0,
                    range: 30.0,
                    color: Color::srgb(1.0, 0.75, 0.45),
                    shadow_maps_enabled: false,
                    ..default()
                },
                Transform::from_translation(GUN_PORT + Vec3::new(0.0, 0.3, -1.0)),
            ));
        });
        for tip in WINGTIPS {
            ribbons.0.push(Trail::new(aircraft, tip, RibbonKind::Vortex));
        }
        for nozzle in NOZZLES {
            ribbons.0.push(Trail::new(
                aircraft,
                Vec3::new(nozzle.x, nozzle.y, 8.5),
                RibbonKind::Contrail,
            ));
        }
    }
}

#[allow(clippy::type_complexity)]
fn update_engines(
    time: Res<Time>,
    aircraft: Query<(&FlightState, Has<Disabled>)>,
    mut engines: Query<(&ChildOf, &mut Engine, &MeshMaterial3d<PlumeMaterial>, &mut Visibility)>,
    haze_handles: Query<&MeshMaterial3d<HazeMaterial>>,
    mut plumes: ResMut<Assets<PlumeMaterial>>,
    mut hazes: ResMut<Assets<HazeMaterial>>,
    mut lights: Query<(&ChildOf, &mut PointLight), With<AfterburnerLight>>,
) {
    let t = time.elapsed_secs();
    let dt = time.delta_secs();
    for (parent, mut engine, material, mut visibility) in &mut engines {
        let Ok((state, disabled)) = aircraft.get(parent.parent()) else {
            continue;
        };
        let target = if state.afterburner_on && !disabled { 1.0 } else { 0.0 };
        // light-off is quick, shutdown a touch slower
        let rate = if target > engine.afterburner { 6.0 } else { 4.0 };
        engine.afterburner += (target - engine.afterburner) * (1.0 - (-rate * dt).exp());
        let power = if disabled { 0.0 } else { state.spool };
        *visibility = if disabled { Visibility::Hidden } else { Visibility::Inherited };
        // turbulent length flicker (layered value noise, not a sine)
        let flick = plume::turbulence(t, engine.seed);
        if let Some(mut m) = plumes.get_mut(&material.0) {
            m.params.state = Vec4::new(power, engine.afterburner, engine.seed, flick);
        }
        if let Ok(h) = haze_handles.get(engine.haze)
            && let Some(mut m) = hazes.get_mut(&h.0)
        {
            m.params.state.x = if disabled { 0.0 } else { 0.25 + 0.55 * power + 0.2 * engine.afterburner };
        }
    }
    for (parent, mut light) in &mut lights {
        let Ok((state, disabled)) = aircraft.get(parent.parent()) else {
            continue;
        };
        let on = state.afterburner_on && !disabled;
        let target = if on { 2.5e6 * plume::turbulence(t * 0.7, 3.0) } else { 0.0 };
        light.intensity += (target - light.intensity) * (1.0 - (-8.0 * dt).exp());
    }
}

fn update_missile_flames(
    missiles: Query<&Missile>,
    mut flames: Query<(&ChildOf, &mut Visibility), With<MissileFlame>>,
) {
    for (parent, mut visibility) in &mut flames {
        let burning = missiles.get(parent.parent()).is_ok_and(|m| m.motor_burning());
        *visibility = if burning { Visibility::Inherited } else { Visibility::Hidden };
    }
}

#[allow(clippy::type_complexity)]
fn aircraft_fx(
    time: Res<Time>,
    mut particles: ResMut<Particles>,
    mut aircraft: Query<(
        Entity,
        &GlobalTransform,
        &FlightState,
        &Health,
        &Weapons,
        Has<Disabled>,
        &mut AircraftFx,
    )>,
    mut muzzle_lights: Query<(&ChildOf, &mut PointLight), With<MuzzleLight>>,
    mut points: Local<Vec<Vec3>>,
) {
    let dt = time.delta_secs();
    if dt <= 0.0 {
        return;
    }
    let t = time.elapsed_secs();
    let mut rng = rand::rng();
    for (entity, gt, state, health, weapons, disabled, mut fx) in &mut aircraft {
        let pos = gt.translation();

        // ---- vapour: wing-surface sheets under G / AoA, and the transonic cone
        if !disabled {
            let humidity = 1.0 - 0.6 * smoothstep(2_000.0, 9_000.0, pos.y);
            let g = smoothstep(4.0, 8.0, state.g_load.abs());
            let aoa = smoothstep(11.0, 22.0, state.alpha.to_degrees().abs());
            let sheet = g.max(aoa) * humidity * smoothstep(60.0, 120.0, state.airspeed);
            let mut n = sheet * 650.0 * dt;
            while n > 0.0 {
                if n < 1.0 && rng.random::<f32>() > n {
                    break;
                }
                n -= 1.0;
                let (u, v) = (rng.random::<f32>(), rng.random::<f32>().powf(0.7));
                let lead = WING[0].lerp(WING[1], u);
                let trail = WING[3].lerp(WING[2], u);
                let mut p = lead.lerp(trail, v * 0.85);
                p.y += rng.random_range(0.1..0.4);
                if rng.random_bool(0.5) {
                    p.x = -p.x;
                }
                particles.add(
                    Particle {
                        vel: Vec3::new(0.0, 0.6, rng.random_range(14.0..30.0)),
                        life: rng.random_range(0.08..0.18),
                        size: (rng.random_range(0.7..1.1), rng.random_range(1.5..2.3)),
                        albedo: Vec3::splat(1.0),
                        opacity: 0.28 * (0.4 + 0.6 * sheet),
                        soft: 0.15,
                        drag: 0.0,
                        fade_in: 0.03,
                        dissolve: 0.0,
                        frame: Some(entity),
                        ..Particle::puff(p)
                    },
                    0.6,
                );
            }
            // shock collar around Mach 1
            let cone = smoothstep(0.9, 0.97, state.mach)
                * (1.0 - smoothstep(1.03, 1.15, state.mach))
                * humidity;
            let mut n = cone * 320.0 * dt;
            while n > 0.0 {
                if n < 1.0 && rng.random::<f32>() > n {
                    break;
                }
                n -= 1.0;
                let a = rng.random_range(0.0..std::f32::consts::TAU);
                let r = rng.random_range(1.2..4.8);
                let z = rng.random_range(-1.0..1.8) + r * 0.35;
                let p = Vec3::new(a.cos() * r * 1.4, a.sin() * r * 0.75 + 0.4, z);
                particles.add(
                    Particle {
                        vel: Vec3::new(0.0, 0.0, 10.0),
                        life: rng.random_range(0.08..0.16),
                        size: (2.0, 3.4),
                        albedo: Vec3::splat(0.97),
                        opacity: 0.3 * cone,
                        soft: 0.2,
                        drag: 0.0,
                        fade_in: 0.03,
                        dissolve: 0.2,
                        frame: Some(entity),
                        ..Particle::puff(p)
                    },
                    0.6,
                );
            }
        }

        // ---- damage smoke, and fire once badly hit or shot down
        let damage = 1.0 - (health.current / health.max).clamp(0.0, 1.0);
        let tail = gt.transform_point(Vec3::new(0.0, 0.3, 6.5));
        if disabled || damage > 0.45 {
            let spacing = if disabled { 1.5 } else { 3.0 };
            spaced(&mut fx.smoke, tail, spacing, &mut points);
            for &p in points.iter() {
                let grey = if disabled { rng.random_range(0.035..0.07) } else { 0.32 - damage * 0.22 };
                let big = if disabled { 1.0 } else { 0.5 + damage * 0.5 };
                particles.add(
                    Particle {
                        vel: state.velocity * 0.06 + rand_dir(&mut rng) * 2.0,
                        life: rng.random_range(5.0..8.0) * big,
                        size: (3.5 * big, rng.random_range(12.0..18.0) * big),
                        grow: 3.5,
                        albedo: Vec3::splat(grey),
                        opacity: if disabled { 0.9 } else { 0.5 + 0.35 * damage },
                        drag: 1.4,
                        buoyancy: 1.2,
                        dissolve: 0.4,
                        ..Particle::puff(p + rand_dir(&mut rng) * 0.5)
                    },
                    0.7,
                );
            }
        } else {
            fx.smoke = None;
        }
        if disabled || damage > 0.75 {
            // shot-down jets trail a torch of burning fuel
            let spacing = if disabled { 0.9 } else { 1.6 };
            spaced(&mut fx.fire, tail, spacing, &mut points);
            let flick = plume::turbulence(t, fx.seed);
            for &p in points.iter() {
                // a short, dense torch: hot puffs that cool to soot within metres
                let size = if disabled { rng.random_range(1.8..2.6) * flick } else { 0.9 * flick };
                particles.add(
                    Particle {
                        vel: state.velocity * rng.random_range(0.75..0.9) + rand_dir(&mut rng) * 2.0,
                        life: if disabled { rng.random_range(0.2..0.32) } else { rng.random_range(0.12..0.2) },
                        size: (size, size * 2.4),
                        grow: 3.0,
                        heat: rng.random_range(1.0..1.25),
                        cool: if disabled { rng.random_range(0.07..0.11) } else { 0.06 },
                        glow: 2.8,
                        albedo: Vec3::splat(0.05),
                        opacity: 0.9,
                        drag: 2.5,
                        buoyancy: 3.0,
                        dissolve: 0.5,
                        ..Particle::puff(p + rand_dir(&mut rng) * 0.4)
                    },
                    0.8,
                );
            }
            if disabled && rng.random_bool((dt * 25.0).min(1.0) as f64) {
                let p = tail + rand_dir(&mut rng) * 1.0;
                particles.add(
                    Particle {
                        life: rng.random_range(0.5..1.2),
                        ..Particle::spark(p, state.velocity * 0.8 + rand_dir(&mut rng) * 15.0)
                    },
                    0.5,
                );
            }
        } else {
            fx.fire = None;
        }

        // ---- gun: flickering star flash, a light, and a little smoke
        let firing = weapons.trigger_gun && weapons.gun_ammo > 0 && !disabled;
        if firing {
            let flash = rng.random_range(0.6..1.0);
            particles.add(
                Particle {
                    life: 0.03,
                    size: (2.6 * flash, 2.6 * flash),
                    glow: 5.5,
                    frame: Some(entity),
                    ..Particle::glow(GUN_PORT + Vec3::new(0.0, 0.0, -0.8))
                },
                1.0,
            );
            for _ in 0..3 {
                let dir = (Vec3::NEG_Z + rand_dir(&mut rng) * 0.35).normalize();
                particles.add(
                    Particle {
                        life: 0.03,
                        size: (0.25, 0.25),
                        stretch: 0.012,
                        glow: 7.5,
                        gravity: 0.0,
                        frame: Some(entity),
                        ..Particle::spark(GUN_PORT + dir * 0.8, dir * rng.random_range(120.0..220.0))
                    },
                    1.0,
                );
            }
            if rng.random_bool(0.6) {
                let p = gt.transform_point(GUN_PORT + Vec3::new(0.0, 0.0, 0.5));
                particles.add(
                    Particle {
                        vel: state.velocity * 0.75 + rand_dir(&mut rng) * 2.0,
                        life: rng.random_range(0.8..1.4),
                        size: (0.5, rng.random_range(2.2..3.2)),
                        grow: 3.0,
                        albedo: Vec3::splat(0.7),
                        opacity: 0.18,
                        drag: 3.0,
                        dissolve: 0.2,
                        ..Particle::puff(p)
                    },
                    0.5,
                );
            }
        }
        for (parent, mut light) in &mut muzzle_lights {
            if parent.parent() == entity {
                light.intensity = if firing { rng.random_range(2e5..6e5) } else { 0.0 };
            }
        }
    }
}

// ---------------------------------------------------------------- missiles & flares

fn projectile_fx(
    time: Res<Time>,
    mut particles: ResMut<Particles>,
    missiles: Query<(Entity, &GlobalTransform, &Missile)>,
    flares: Query<(Entity, &GlobalTransform), With<Flare>>,
    camera: Query<&GlobalTransform, With<crate::camera::MainCamera>>,
    mut last: Local<HashMap<Entity, Option<Vec3>>>,
    mut points: Local<Vec<Vec3>>,
    mut trails: Local<TrailLog>,
) {
    let mut rng = rand::rng();
    let t = time.elapsed_secs();
    let cam = camera.single().map_or(Vec3::ZERO, |c| c.translation());
    // world size of ~one pixel at a distance (65° fov, ~1440 px tall)
    let pixel = |p: Vec3| p.distance(cam) * 0.00088;
    last.retain(|e, _| missiles.contains(*e) || flares.contains(*e));
    trails.report(|e| missiles.contains(e));
    for (entity, gt, missile) in &missiles {
        // missile meshes point along local -Y (looking_to(velocity) then +90°
        // about X takes -Y to -Z); the motor is at +Y
        const MOTOR: Vec3 = Vec3::new(0.0, 2.0, 0.0);
        let motor = gt.transform_point(MOTOR);
        let flick = plume::turbulence(t * 1.7, entity.index().index() as f32);
        // never let it shrink below a few pixels, so missiles stay visible far out
        let min_size = pixel(motor) * 4.0;
        if !missile.motor_lit() {
            // burnt out: just the hot motor casing glinting
            let s = (0.8_f32).max(min_size * 0.7);
            particles.add(
                Particle {
                    life: particles::ONE_FRAME,
                    size: (s, s),
                    glow: 1.5,
                    heat: 0.3,
                    frame: Some(entity),
                    ..Particle::glow(MOTOR)
                },
                1.0,
            );
            last.insert(entity, None);
            continue;
        }
        let boost = missile.motor_burning();
        // hot exhaust flame just behind the nozzle
        let s = (if boost { 2.2 } else { 1.4 } * flick).max(min_size);
        // (riding on the missile, for exactly one frame: a world-space glow
        // that outlived its frame hung in the air ~10 m behind, a second light)
        particles.add(
            Particle {
                life: particles::ONE_FRAME,
                size: (s, s),
                glow: if boost { 6.5 } else { 4.0 },
                frame: Some(entity),
                ..Particle::glow(MOTOR + Vec3::Y * 0.8)
            },
            1.0,
        );
        // thick, billowing smoke, laid down every couple of metres
        let entry = last.entry(entity).or_insert(None);
        let age = missile.age;
        // the sustainer leaves a thinner trail
        spaced(entry, motor, if boost { 1.1 } else { 1.6 }, &mut points);
        let thin = if boost { 1.0 } else { 0.75 };
        let stats = trails.missiles.entry(entity).or_insert((0, 0, particles.list.len()));
        for &p in points.iter() {
            stats.1 += 1;
            stats.0 += particles.add(
                Particle {
                    vel: missile.velocity * 0.04 + rand_dir(&mut rng) * 2.5,
                    life: rng.random_range(10.0..14.0),
                    size: (
                        rng.random_range(1.3..1.8) * thin,
                        rng.random_range(13.0..19.0) * thin,
                    ),
                    grow: 3.5,
                    albedo: Vec3::splat(rng.random_range(0.95..1.0)),
                    opacity: 0.8 * thin.sqrt(),
                    heat: if age < 0.3 { 0.0 } else { 0.9 },
                    cool: 0.05,
                    glow: 3.5,
                    drag: 1.8,
                    buoyancy: 0.4,
                    dissolve: 0.3,
                    fade_in: 0.05,
                    ..Particle::puff(p + rand_dir(&mut rng) * 0.3)
                },
                0.75,
            ) as u32;
        }
    }
    for (entity, gt) in &flares {
        let pos = gt.translation();
        let flick = plume::turbulence(t * 2.0, entity.index().index() as f32);
        particles.add(
            Particle {
                life: particles::ONE_FRAME,
                size: (7.0 * flick, 7.0 * flick),
                glow: 3.5,
                heat: 1.3,
                frame: Some(entity),
                ..Particle::glow(Vec3::ZERO)
            },
            1.0,
        );
        let entry = last.entry(entity).or_insert(None);
        spaced(entry, pos, 2.0, &mut points);
        for &p in points.iter() {
            particles.add(
                Particle {
                    vel: rand_dir(&mut rng) * 1.5,
                    life: rng.random_range(3.0..4.5),
                    size: (0.9, rng.random_range(5.0..7.0)),
                    grow: 3.0,
                    albedo: Vec3::splat(0.9),
                    opacity: 0.6,
                    drag: 2.0,
                    buoyancy: 0.5,
                    dissolve: 0.3,
                    ..Particle::puff(p)
                },
                0.5,
            );
        }
        if rng.random_bool(0.3) {
            particles.add(
                Particle {
                    life: rng.random_range(0.3..0.7),
                    ..Particle::spark(pos, rand_dir(&mut rng) * 12.0)
                },
                0.4,
            );
        }
    }
}

/// `FLIGHT_SIM_LOG_MISSILES=1`: logs how much of each missile's smoke trail
/// made it into the particle budget once the missile is gone.
#[derive(Default)]
struct TrailLog {
    /// puffs added, puffs tried, live particles when it first emitted
    missiles: HashMap<Entity, (u32, u32, usize)>,
    count: u32,
    bare: u32,
}

impl TrailLog {
    fn report(&mut self, alive: impl Fn(Entity) -> bool) {
        let log = std::env::var("FLIGHT_SIM_LOG_MISSILES").is_ok();
        let (count, bare) = (&mut self.count, &mut self.bare);
        self.missiles.retain(|&e, &mut (added, tried, at_launch)| {
            if alive(e) {
                return true;
            }
            if log {
                *count += 1;
                // most of its trail missing reads as no plume at all
                let missing = added * 10 < tried;
                *bare += missing as u32;
                info!(
                    "missile {count}: smoke {added}/{tried} puffs{} ({at_launch} particles at launch; {bare} of {count} bare so far)",
                    if missing { " NO PLUME" } else { "" },
                );
            }
            false
        });
    }
}

// ---------------------------------------------------------------- explosions

#[derive(Component)]
struct Flash {
    age: f32,
    life: f32,
    peak: f32,
}

/// Burning fragments thrown out by big explosions, trailing smoke.
struct Debris {
    pos: Vec3,
    vel: Vec3,
    age: f32,
    life: f32,
    size: f32,
    last: Option<Vec3>,
}

/// A fire with a smoke column that keeps going after a big impact.
struct Column {
    pos: Vec3,
    age: f32,
    life: f32,
    size: f32,
    fire: bool,
    smoke_acc: f32,
    fire_acc: f32,
}

#[derive(Resource, Default)]
struct Bursts {
    debris: Vec<Debris>,
    columns: Vec<Column>,
}

/// Explosion sizes in use (see weapons.rs and game.rs): 1.0 bullet into
/// terrain, 1.5 bullet hit on an aircraft, 6 missile into terrain, 9 missile
/// proximity detonation, 12 aircraft shot down, 25 aircraft crashing.
fn hits_ground(size: f32) -> bool {
    (size - 1.0).abs() < 0.01 || (size - 6.0).abs() < 0.01 || size >= 20.0
}

fn spawn_explosions(
    mut commands: Commands,
    mut explosions: MessageReader<Explosion>,
    mut particles: ResMut<Particles>,
    mut bursts: ResMut<Bursts>,
) {
    let mut rng = rand::rng();
    for e in explosions.read() {
        let (p, s) = (e.position, e.size);
        if s < 1.2 {
            ground_bullet_hit(&mut particles, &mut rng, p);
        } else if s < 3.0 {
            bullet_hit(&mut particles, &mut rng, p, s);
        } else {
            blast(&mut commands, &mut particles, &mut bursts, &mut rng, p, s, hits_ground(s));
        }
    }
}

fn bullet_hit(particles: &mut Particles, rng: &mut impl Rng, p: Vec3, s: f32) {
    particles.add(
        Particle {
            life: 0.07,
            size: (2.8 * s, 1.5 * s),
            glow: 7.5,
            ..Particle::glow(p)
        },
        1.0,
    );
    for _ in 0..12 {
        particles.add(
            Particle {
                life: rng.random_range(0.12..0.45),
                size: (0.09, 0.06),
                ..Particle::spark(p, rand_dir(rng) * rng.random_range(35.0..130.0))
            },
            1.0,
        );
    }
    particles.add(
        Particle {
            vel: rand_dir(rng) * 2.0,
            life: rng.random_range(1.0..1.6),
            size: (0.8, 4.0),
            grow: 3.0,
            albedo: Vec3::splat(0.25),
            opacity: 0.55,
            heat: 0.6,
            cool: 0.05,
            glow: 2.5,
            drag: 2.0,
            dissolve: 0.2,
            ..Particle::puff(p)
        },
        0.9,
    );
}

fn ground_bullet_hit(particles: &mut Particles, rng: &mut impl Rng, p: Vec3) {
    particles.add(
        Particle {
            life: 0.05,
            size: (1.6, 1.0),
            glow: 4.5,
            ..Particle::glow(p + Vec3::Y * 0.3)
        },
        1.0,
    );
    for _ in 0..3 {
        particles.add(
            Particle {
                vel: Vec3::Y * rng.random_range(3.0..8.0) + rand_dir(rng) * 2.0,
                life: rng.random_range(1.8..3.2),
                size: (0.9, rng.random_range(4.5..7.0)),
                grow: 3.0,
                albedo: dust_color(rng),
                opacity: 0.7,
                drag: 2.2,
                gravity: 1.0,
                dissolve: 0.25,
                ..Particle::puff(p + rand_dir(rng) * 0.4)
            },
            0.8,
        );
    }
    // clods of soil and chips of rock
    for _ in 0..7 {
        let dir = (Vec3::Y * 1.3 + rand_dir(rng)).normalize();
        particles.add(
            Particle {
                vel: dir * rng.random_range(8.0..22.0),
                life: rng.random_range(0.7..1.4),
                size: (rng.random_range(0.25..0.5), 0.3),
                albedo: dust_color(rng) * 0.4,
                opacity: 1.0,
                drag: 0.2,
                wind: 0.0,
                gravity: 9.8,
                dissolve: 0.8,
                ..Particle::puff(p)
            },
            0.8,
        );
    }
}

fn blast(
    commands: &mut Commands,
    particles: &mut Particles,
    bursts: &mut Bursts,
    rng: &mut impl Rng,
    p: Vec3,
    s: f32,
    ground: bool,
) {
    // how long things last scales gently with the size
    let k = (s / 12.0).sqrt();

    // ---- flash: light, a blinding core and a broad glow
    commands.spawn((
        PointLight {
            intensity: 0.0,
            range: s * 60.0,
            color: Color::srgb(1.0, 0.68, 0.35),
            shadow_maps_enabled: false,
            ..default()
        },
        Transform::from_translation(p),
        Flash {
            age: 0.0,
            life: 0.35 + s * 0.02,
            peak: s * s * 6e7,
        },
    ));
    particles.add(
        Particle { life: 0.1, size: (s * 2.5, s * 4.0), glow: 15.0, heat: 1.4, ..Particle::glow(p) },
        1.0,
    );
    particles.add(
        Particle { life: 0.35, size: (s * 1.5, s * 2.5), glow: 3.5, ..Particle::glow(p) },
        1.0,
    );

    // ---- fireball: hot puffs thrown out that stall, rise, cool and turn to soot
    let n = (14.0 + s * 1.6).min(56.0) as usize;
    for _ in 0..n {
        let mut dir = rand_dir(rng);
        if ground {
            dir.y = dir.y.abs() * 0.8 + 0.2;
        }
        let dir = dir.normalize();
        let shade = rng.random_range(0.13..0.24);
        // puffs thrown furthest cool first; the core burns longest
        let reach: f32 = rng.random_range(0.0..1.0);
        particles.add(
            Particle {
                vel: dir * s * (0.6 + 2.6 * reach),
                life: rng.random_range(6.0..10.0) * k,
                size: (s * rng.random_range(0.5..0.7), s * rng.random_range(1.3..1.9)),
                grow: 5.0,
                heat: rng.random_range(1.05..1.35),
                cool: (0.8 - 0.45 * reach) * k * rng.random_range(0.8..1.2),
                glow: 4.0,
                albedo: Vec3::splat(shade),
                opacity: 0.95,
                drag: 3.0,
                buoyancy: 3.5,
                dissolve: 0.4,
                spin: rng.random_range(-0.25..0.25),
                ..Particle::puff(p + dir * s * rng.random_range(0.0..0.25))
            },
            1.0,
        );
    }
    // ---- billows of soot that roll out after the fire
    let n = (5.0 + s * 0.5) as usize;
    for _ in 0..n {
        let dir = rand_dir(rng);
        let shade = rng.random_range(0.13..0.24);
        particles.add(
            Particle {
                age: -rng.random_range(0.15..0.6),
                vel: dir * s * rng.random_range(0.4..1.0) + Vec3::Y * 2.0,
                life: rng.random_range(7.0..12.0) * k,
                size: (s * 0.8, s * rng.random_range(2.0..2.8)),
                grow: 2.5,
                albedo: Vec3::splat(shade),
                opacity: 0.75,
                heat: 0.4,
                cool: 0.3,
                glow: 2.5,
                drag: 1.5,
                buoyancy: 1.5,
                dissolve: 0.35,
                fade_in: 0.3,
                ..Particle::puff(p + dir * s * 0.4)
            },
            1.0,
        );
    }
    // ---- sparks and fragments
    let n = (14.0 + s * 4.0).min(100.0) as usize;
    for _ in 0..n {
        let speed = rng.random_range(40.0..170.0) * (0.6 + s / 30.0);
        particles.add(
            Particle {
                life: rng.random_range(0.4..1.4),
                size: (0.2, 0.12),
                drag: 0.9,
                ..Particle::spark(p, rand_dir(rng) * speed)
            },
            1.0,
        );
    }
    // ---- burning debris arcing away
    if s >= 6.0 {
        let n = ((s * 0.3) as usize).min(8);
        for _ in 0..n {
            let mut dir = rand_dir(rng);
            dir.y = dir.y.abs() * 0.7 + 0.3;
            bursts.debris.push(Debris {
                pos: p,
                vel: dir.normalize() * rng.random_range(25.0..60.0) * (0.7 + s / 40.0),
                age: 0.0,
                life: rng.random_range(1.5..3.5),
                size: rng.random_range(0.7..1.4),
                last: None,
            });
        }
    }
    // ---- ground: a skirt of dust, thrown soil and rock, and a lasting fire
    if ground {
        let n = (10.0 + s) as usize;
        for _ in 0..n {
            let a = rng.random_range(0.0..std::f32::consts::TAU);
            let out = Vec3::new(a.cos(), 0.0, a.sin());
            particles.add(
                Particle {
                    vel: out * s * rng.random_range(1.5..3.0) + Vec3::Y * rng.random_range(1.0..4.0),
                    life: rng.random_range(5.0..9.0) * k,
                    size: (s * 0.4, s * rng.random_range(1.4..2.0)),
                    grow: 3.0,
                    albedo: dust_color(rng),
                    opacity: 0.85,
                    drag: 1.6,
                    buoyancy: 0.4,
                    dissolve: 0.3,
                    ..Particle::puff(p + out * s * 0.3)
                },
                1.0,
            );
        }
        let n = (20.0 + s * 1.5) as usize;
        for _ in 0..n {
            let dir = (Vec3::Y * 1.2 + rand_dir(rng)).normalize();
            particles.add(
                Particle {
                    vel: dir * rng.random_range(15.0..45.0) * k.max(0.8),
                    life: rng.random_range(1.5..3.0),
                    size: (rng.random_range(0.4..1.0) * k.max(0.7), 0.5),
                    albedo: dust_color(rng) * 0.35,
                    opacity: 1.0,
                    drag: 0.15,
                    wind: 0.0,
                    gravity: 9.8,
                    dissolve: 0.85,
                    ..Particle::puff(p)
                },
                1.0,
            );
        }
        bursts.columns.push(Column {
            pos: p,
            age: 0.0,
            life: if s >= 20.0 { 30.0 } else { 10.0 },
            size: s * 0.45,
            fire: true,
            smoke_acc: 0.0,
            fire_acc: 0.0,
        });
    }
}

fn update_flashes(
    mut commands: Commands,
    time: Res<Time>,
    mut flashes: Query<(Entity, &mut Flash, &mut PointLight)>,
) {
    for (entity, mut flash, mut light) in &mut flashes {
        flash.age += time.delta_secs();
        if flash.age >= flash.life {
            commands.entity(entity).despawn();
            continue;
        }
        let t = flash.age / flash.life;
        // near-instant peak, then a fast fall with a flickering tail
        light.intensity = flash.peak * (1.0 - t).powi(3) * plume::turbulence(flash.age * 3.0, 1.0);
    }
}

fn update_bursts(
    time: Res<Time>,
    mut bursts: ResMut<Bursts>,
    mut particles: ResMut<Particles>,
    mut points: Local<Vec<Vec3>>,
) {
    let dt = time.delta_secs();
    if dt <= 0.0 {
        return;
    }
    let mut rng = rand::rng();
    let bursts = &mut *bursts;
    bursts.debris.retain_mut(|d| {
        d.age += dt;
        if d.age >= d.life {
            return false;
        }
        d.vel.y -= 9.8 * dt;
        d.vel *= (-0.25 * dt).exp();
        d.pos += d.vel * dt;
        let burning = 1.0 - d.age / d.life;
        particles.add(
            Particle {
                life: 0.025,
                size: (d.size * 1.6, d.size * 1.6),
                glow: 3.5 * burning,
                ..Particle::glow(d.pos)
            },
            0.9,
        );
        spaced(&mut d.last, d.pos, 1.1, &mut points);
        for &p in points.iter() {
            particles.add(
                Particle {
                    vel: d.vel * 0.1,
                    life: rng.random_range(2.5..4.0),
                    size: (d.size * 2.2, d.size * rng.random_range(6.0..9.0)),
                    grow: 2.5,
                    albedo: Vec3::splat(0.14),
                    opacity: 0.7 * (0.3 + 0.7 * burning),
                    heat: 0.9 * burning,
                    cool: 0.12,
                    glow: 3.0,
                    drag: 1.5,
                    buoyancy: 1.0,
                    dissolve: 0.35,
                    ..Particle::puff(p)
                },
                0.8,
            );
        }
        true
    });
    bursts.columns.retain_mut(|c| {
        c.age += dt;
        if c.age >= c.life {
            return false;
        }
        let strength = (1.0 - c.age / c.life).sqrt();
        c.smoke_acc += dt * 4.0 * strength;
        while c.smoke_acc >= 1.0 {
            c.smoke_acc -= 1.0;
            let shade = rng.random_range(0.05..0.12);
            particles.add(
                Particle {
                    vel: Vec3::Y * rng.random_range(5.0..9.0) * c.size.sqrt() * 0.5,
                    life: rng.random_range(10.0..15.0),
                    size: (c.size * 0.8, c.size * rng.random_range(2.4..3.2)),
                    grow: 1.6,
                    albedo: Vec3::splat(shade),
                    opacity: 0.85 * strength.max(0.4),
                    heat: 0.5,
                    cool: 0.4,
                    glow: 2.5,
                    drag: 0.5,
                    buoyancy: 1.2,
                    dissolve: 0.45,
                    fade_in: 0.4,
                    ..Particle::puff(c.pos + rand_dir(&mut rng) * c.size * 0.4)
                },
                0.7,
            );
        }
        if c.fire {
            c.fire_acc += dt * 22.0 * strength;
            while c.fire_acc >= 1.0 {
                c.fire_acc -= 1.0;
                let mut off = rand_dir(&mut rng) * c.size * 0.6;
                off.y = off.y.abs() * 0.3;
                particles.add(
                    Particle {
                        vel: Vec3::Y * rng.random_range(4.0..9.0),
                        life: rng.random_range(0.7..1.3),
                        size: (c.size * 0.35, c.size * rng.random_range(0.7..1.0)),
                        heat: rng.random_range(1.0..1.2),
                        cool: 0.35,
                        glow: 3.0,
                        albedo: Vec3::splat(0.05),
                        opacity: 0.9,
                        drag: 1.0,
                        buoyancy: 4.0,
                        dissolve: 0.45,
                        ..Particle::puff(c.pos + off)
                    },
                    0.8,
                );
            }
        }
        true
    });
}

// ---------------------------------------------------------------- ribbon trails

#[derive(Clone, Copy, PartialEq, Eq)]
enum RibbonKind {
    Vortex,
    Contrail,
}

struct TrailPoint {
    pos: Vec3,
    age: f32,
    intensity: f32,
    /// distance along the trail (m), for stable noise
    s: f32,
}

struct Trail {
    source: Option<Entity>,
    offset: Vec3,
    kind: RibbonKind,
    points: VecDeque<TrailPoint>,
    total: f32,
    seed: f32,
}

impl Trail {
    fn new(source: Entity, offset: Vec3, kind: RibbonKind) -> Self {
        Trail {
            source: Some(source),
            offset,
            kind,
            points: VecDeque::new(),
            total: 0.0,
            seed: rand::rng().random_range(0.0..1.0),
        }
    }

    /// (lifetime, start width, end width, opacity, dissolve start, albedo)
    fn style(&self) -> (f32, f32, f32, f32, f32, f32) {
        match self.kind {
            RibbonKind::Vortex => (0.9, 0.2, 0.8, 0.5, 0.15, 0.97),
            RibbonKind::Contrail => (16.0, 0.6, 9.0, 0.8, 0.25, 0.97),
        }
    }
}

#[derive(Resource, Default)]
struct Ribbons(Vec<Trail>);

fn update_ribbons(
    time: Res<Time>,
    mut ribbons: ResMut<Ribbons>,
    mut particles: ResMut<Particles>,
    camera: Query<&GlobalTransform, With<MainCamera>>,
    sources: Query<(&GlobalTransform, &FlightState, Has<Disabled>)>,
) {
    let dt = time.delta_secs();
    let Ok(cam) = camera.single() else {
        return;
    };
    let cam_pos = cam.translation();
    ribbons.0.retain_mut(|trail| {
        let (life, w0, w1, opacity, dissolve_start, albedo) = trail.style();
        for p in trail.points.iter_mut() {
            p.age += dt;
            // the vortex core and the contrail both drift with the wind
            p.pos += particles::WIND * dt;
        }
        while trail.points.front().is_some_and(|p| p.age > life) {
            trail.points.pop_front();
        }
        match trail.source.and_then(|s| sources.get(s).ok()) {
            Some((gt, state, disabled)) => {
                let intensity = match trail.kind {
                    RibbonKind::Vortex => {
                        let g = smoothstep(3.5, 7.5, state.g_load.abs());
                        let aoa = smoothstep(10.0, 20.0, state.alpha.to_degrees().abs());
                        g.max(aoa) * if disabled { 0.0 } else { 1.0 }
                    }
                    RibbonKind::Contrail => {
                        smoothstep(3_500.0, 5_000.0, gt.translation().y) * if disabled { 0.0 } else { 1.0 }
                    }
                };
                let pos = gt.transform_point(trail.offset);
                let step = trail.points.back().map_or(0.0, |p| p.pos.distance(pos));
                if step > 300.0 {
                    trail.points.clear();
                }
                let far_enough = trail
                    .points
                    .back()
                    .is_none_or(|p| p.pos.distance(pos) > 5.0 || p.age > 0.06);
                if far_enough {
                    trail.total += step.min(300.0);
                    trail.points.push_back(TrailPoint { pos, age: 0.0, intensity, s: trail.total });
                } else if let Some(last) = trail.points.back_mut() {
                    let d = last.pos.distance(pos);
                    last.pos = pos;
                    last.intensity = intensity;
                    last.s += d;
                    trail.total = last.s;
                }
            }
            None => {
                trail.source = None;
                if trail.points.is_empty() {
                    return false;
                }
            }
        }

        let n = trail.points.len();
        if n < 2 || trail.points.iter().all(|p| p.intensity < 0.01) {
            return true;
        }
        let head_s = trail.total;
        let side_at = |i: usize| {
            let prev = trail.points[i.saturating_sub(1)].pos;
            let next = trail.points[(i + 1).min(n - 1)].pos;
            let tangent = (next - prev).normalize_or(Vec3::Z);
            let to_cam = (cam_pos - trail.points[i].pos).normalize_or(Vec3::Y);
            tangent.cross(to_cam).normalize_or(Vec3::X)
        };
        let width_at = |p: &TrailPoint| {
            let t = (p.age / life).clamp(0.0, 1.0);
            w0 + (w1 - w0) * t.sqrt()
        };
        let alpha_at = |i: usize, p: &TrailPoint| {
            let t = (p.age / life).clamp(0.0, 1.0);
            let head = if i == n - 1 { 0.0 } else { 1.0 };
            // contrails condense a little way behind the engines
            let gap = match trail.kind {
                RibbonKind::Contrail => smoothstep(15.0, 60.0, head_s - p.s),
                RibbonKind::Vortex => smoothstep(0.0, 6.0, head_s - p.s),
            };
            opacity * p.intensity * (1.0 - t).powf(1.2) * head * gap
        };
        let mut prev_side = side_at(0);
        for i in 0..n - 1 {
            let (a, b) = (&trail.points[i], &trail.points[i + 1]);
            let next_side = side_at(i + 1);
            let (wa, wb) = (width_at(a) * 0.5, width_at(b) * 0.5);
            let (oa, ob) = (alpha_at(i, a), alpha_at(i + 1, b));
            if oa < 0.002 && ob < 0.002 {
                prev_side = next_side;
                continue;
            }
            let t = (a.age / life).clamp(0.0, 1.0);
            particles.quads.push(Quad {
                corners: [
                    a.pos - prev_side * wa,
                    a.pos + prev_side * wa,
                    b.pos + next_side * wb,
                    b.pos - next_side * wb,
                ],
                uv: [
                    Vec2::new(0.0, a.s),
                    Vec2::new(1.0, a.s),
                    Vec2::new(1.0, b.s),
                    Vec2::new(0.0, b.s),
                ],
                albedo: Vec3::splat(albedo),
                opacity: [oa, oa, ob, ob],
                dissolve: smoothstep(dissolve_start, 1.0, t),
                seed: trail.seed,
                side: prev_side,
                glow: 0.0,
                heat: 0.0,
            });
            prev_side = next_side;
        }
        true
    });
}
