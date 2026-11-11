//! Visual effects: engine flames, ribbon trails (wingtip vortices, contrails,
//! missile smoke), particles (explosions, damage smoke and fire, impact
//! sparks), muzzle flashes.
//!
//! Particles are small CPU-simulated spheres with their own fading material;
//! there are only ever a few hundred, so this stays cheap. Trails are one
//! camera-facing ribbon mesh per trail, rebuilt every frame.

use std::collections::VecDeque;

use bevy::{
    asset::RenderAssetUsages,
    camera::visibility::NoFrustumCulling,
    mesh::{Indices, PrimitiveTopology},
    prelude::*,
};
use rand::RngExt;

use crate::{
    camera::MainCamera,
    flight::{Aircraft, Disabled, FlightState},
    game::{Explosion, Health},
    weapons::{Missile, MissileFlame, Weapons},
};

const MAX_PARTICLES: usize = 1_500;

pub struct EffectsPlugin;

impl Plugin for EffectsPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, setup_assets)
            .add_systems(
                Update,
                (
                    attach_aircraft_effects,
                    attach_missile_trails,
                    spawn_explosions,
                    damage_smoke,
                    update_particles,
                    update_flashes,
                ),
            )
            .add_systems(
                PostUpdate,
                (update_engine_flames, update_muzzle_flashes, update_missile_flames, update_trails)
                    .after(TransformSystems::Propagate),
            );
    }
}

#[derive(Resource)]
struct EffectAssets {
    sphere: Handle<Mesh>,
    /// 2x2 quad, faced at the camera each frame
    billboard: Handle<Mesh>,
    /// soft radial falloff used by every particle
    blob: Handle<Image>,
    flame_cone: Handle<Mesh>,
    flame_dry: Handle<StandardMaterial>,
    flame_ab: Handle<StandardMaterial>,
    muzzle: Handle<StandardMaterial>,
}

fn additive(materials: &mut Assets<StandardMaterial>, color: LinearRgba) -> Handle<StandardMaterial> {
    // unlit materials output only base_color (emissive is ignored), so the
    // HDR glow colour goes there
    materials.add(StandardMaterial {
        base_color: color.into(),
        unlit: true,
        alpha_mode: AlphaMode::Add,
        ..default()
    })
}

fn soft_blob() -> Image {
    use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};
    const N: u32 = 64;
    let mut data = Vec::with_capacity((N * N * 4) as usize);
    for y in 0..N {
        for x in 0..N {
            let d = Vec2::new(x as f32 + 0.5, y as f32 + 0.5) / N as f32 * 2.0 - 1.0;
            let a = (1.0 - d.length_squared()).max(0.0).powf(1.6);
            data.extend_from_slice(&[255, 255, 255, (a * 255.0) as u8]);
        }
    }
    Image::new(
        Extent3d { width: N, height: N, depth_or_array_layers: 1 },
        TextureDimension::D2,
        data,
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::RENDER_WORLD,
    )
}

fn setup_assets(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut images: ResMut<Assets<Image>>,
) {
    commands.insert_resource(EffectAssets {
        sphere: meshes.add(Sphere::new(1.0).mesh().ico(2).unwrap()),
        billboard: meshes.add(Rectangle::new(2.0, 2.0)),
        blob: images.add(soft_blob()),
        flame_cone: meshes.add(Cone::new(0.5, 1.0).mesh().resolution(16)),
        flame_dry: additive(&mut materials, LinearRgba::new(2.0, 1.4, 1.6, 0.35)),
        flame_ab: additive(&mut materials, LinearRgba::new(4.0, 1.5, 0.45, 0.5)),
        muzzle: additive(&mut materials, LinearRgba::new(30.0, 18.0, 5.0, 0.9)),
    });
}

// ---------------------------------------------------------------- aircraft

/// Exhaust nozzle positions in aircraft-local space (metres).
// measured from the model: nose at z=-12, tail fins reach z=+8.9, nozzle exits ~6.6-7.0; flames start just inside so there is never a gap
const NOZZLES: [Vec3; 2] = [Vec3::new(0.75, 0.05, 5.4), Vec3::new(-0.75, 0.05, 5.4)];
const WINGTIPS: [Vec3; 2] = [Vec3::new(7.3, 0.0, 2.0), Vec3::new(-7.3, 0.0, 2.0)];
const GUN_PORT: Vec3 = Vec3::new(0.9, 0.9, -7.5);

#[derive(Component)]
struct EngineFlame {
    nozzle: Vec3,
    flicker: f32,
}

#[derive(Component)]
struct MuzzleFlash;

fn attach_aircraft_effects(
    mut commands: Commands,
    assets: Res<EffectAssets>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut meshes: ResMut<Assets<Mesh>>,
    new: Query<Entity, Added<Aircraft>>,
) {
    let mut rng = rand::rng();
    for aircraft in &new {
        commands.entity(aircraft).with_children(|p| {
            for nozzle in NOZZLES {
                p.spawn((
                    EngineFlame {
                        nozzle,
                        flicker: rng.random_range(0.0..10.0),
                    },
                    Mesh3d(assets.flame_cone.clone()),
                    MeshMaterial3d(assets.flame_dry.clone()),
                    Transform::from_translation(nozzle),
                    bevy::light::NotShadowCaster,
                ));
            }
            p.spawn((
                MuzzleFlash,
                Mesh3d(assets.sphere.clone()),
                MeshMaterial3d(assets.muzzle.clone()),
                Transform::from_translation(GUN_PORT).with_scale(Vec3::splat(0.6)),
                Visibility::Hidden,
                bevy::light::NotShadowCaster,
            ));
        });
        // wingtip vortices: visible under high G or high AoA
        for tip in WINGTIPS {
            spawn_trail(
                &mut commands,
                &mut meshes,
                &mut materials,
                Trail::new(aircraft, tip, TrailKind::Vortex),
            );
        }
        // contrails from the engines at altitude
        spawn_trail(
            &mut commands,
            &mut meshes,
            &mut materials,
            Trail::new(aircraft, Vec3::new(0.0, -0.25, 9.5), TrailKind::Contrail),
        );
    }
}

fn update_engine_flames(
    time: Res<Time>,
    assets: Res<EffectAssets>,
    aircraft: Query<(&FlightState, Has<Disabled>)>,
    mut flames: Query<(
        &ChildOf,
        &EngineFlame,
        &mut Transform,
        &mut MeshMaterial3d<StandardMaterial>,
        &mut Visibility,
    )>,
) {
    let t = time.elapsed_secs();
    for (parent, flame, mut transform, mut material, mut visibility) in &mut flames {
        let Ok((state, disabled)) = aircraft.get(parent.parent()) else {
            continue;
        };
        if disabled {
            *visibility = Visibility::Hidden;
            continue;
        }
        *visibility = Visibility::Inherited;
        let flicker = 1.0 + 0.08 * (t * 40.0 + flame.flicker).sin() + 0.05 * (t * 67.0).sin();
        let (length, width, handle) = if state.afterburner_on {
            (5.0 * flicker, 0.9, &assets.flame_ab)
        } else {
            (0.6 + 2.2 * state.spool * flicker, 0.75, &assets.flame_dry)
        };
        if material.0 != *handle {
            material.0 = handle.clone();
        }
        // cone points +Y by default; turn it to point backwards (+Z), base at the nozzle
        transform.translation = flame.nozzle + Vec3::Z * length * 0.5;
        transform.rotation = Quat::from_rotation_x(std::f32::consts::FRAC_PI_2);
        transform.scale = Vec3::new(width, length, width);
    }
}

fn update_muzzle_flashes(
    time: Res<Time>,
    aircraft: Query<&Weapons>,
    mut flashes: Query<(&ChildOf, &mut Visibility, &mut Transform), With<MuzzleFlash>>,
) {
    let flicker = (time.elapsed_secs() * 45.0).sin() > -0.3;
    for (parent, mut visibility, mut transform) in &mut flashes {
        let firing = aircraft
            .get(parent.parent())
            .is_ok_and(|w| w.trigger_gun && w.gun_ammo > 0);
        *visibility = if firing && flicker { Visibility::Inherited } else { Visibility::Hidden };
        transform.scale = Vec3::new(0.5, 0.5, 1.4) * if flicker { 1.0 } else { 0.7 };
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

// ---------------------------------------------------------------- trails

#[derive(Clone, Copy, PartialEq, Eq)]
enum TrailKind {
    Vortex,
    Contrail,
    MissileSmoke,
}

#[derive(Component)]
struct Trail {
    source: Option<Entity>,
    offset: Vec3,
    kind: TrailKind,
    /// (position, age, intensity)
    points: VecDeque<(Vec3, f32, f32)>,
    mesh: Handle<Mesh>,
}

impl Trail {
    fn new(source: Entity, offset: Vec3, kind: TrailKind) -> Self {
        Trail {
            source: Some(source),
            offset,
            kind,
            points: VecDeque::new(),
            mesh: Handle::default(),
        }
    }

    /// (lifetime, start width, end width, base colour)
    fn style(&self) -> (f32, f32, f32, LinearRgba) {
        match self.kind {
            TrailKind::Vortex => (0.9, 0.25, 1.2, LinearRgba::new(1.0, 1.0, 1.0, 0.55)),
            TrailKind::Contrail => (9.0, 1.5, 9.0, LinearRgba::new(1.0, 1.0, 1.0, 0.5)),
            TrailKind::MissileSmoke => (6.0, 1.2, 8.0, LinearRgba::new(0.92, 0.92, 0.9, 0.75)),
        }
    }
}

fn spawn_trail(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
    mut trail: Trail,
) {
    // start with a degenerate triangle: empty meshes make the allocator complain
    let mesh = meshes.add(
        Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::default())
            .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, vec![[0.0f32; 3]; 3])
            .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, vec![[0.0f32, 1.0, 0.0]; 3])
            .with_inserted_attribute(Mesh::ATTRIBUTE_COLOR, vec![[0.0f32; 4]; 3])
            .with_inserted_indices(Indices::U32(vec![0, 1, 2])),
    );
    trail.mesh = mesh.clone();
    let material = materials.add(StandardMaterial {
        base_color: Color::WHITE,
        unlit: matches!(trail.kind, TrailKind::Vortex),
        alpha_mode: AlphaMode::Blend,
        double_sided: true,
        cull_mode: None,
        perceptual_roughness: 1.0,
        ..default()
    });
    commands.spawn((
        trail,
        Mesh3d(mesh),
        MeshMaterial3d(material),
        Transform::IDENTITY,
        Visibility::Hidden,
        NoFrustumCulling,
        bevy::light::NotShadowCaster,
    ));
}

fn attach_missile_trails(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    new: Query<Entity, Added<Missile>>,
) {
    for missile in &new {
        // missile meshes point along local +Y (see weapons.rs); the motor is at -Y
        spawn_trail(
            &mut commands,
            &mut meshes,
            &mut materials,
            Trail::new(missile, Vec3::new(0.0, -2.2, 0.0), TrailKind::MissileSmoke),
        );
    }
}

#[allow(clippy::type_complexity)]
fn update_trails(
    mut commands: Commands,
    time: Res<Time>,
    camera: Query<&GlobalTransform, With<MainCamera>>,
    sources: Query<(&GlobalTransform, Option<&FlightState>, Option<&Missile>, Has<Disabled>)>,
    mut trails: Query<(Entity, &mut Trail, &mut Visibility)>,
    mut meshes: ResMut<Assets<Mesh>>,
) {
    let dt = time.delta_secs();
    let Ok(cam) = camera.single() else {
        return;
    };
    let cam_pos = cam.translation();

    for (entity, mut trail, mut visibility) in &mut trails {
        let (life, w0, w1, color) = trail.style();
        for p in trail.points.iter_mut() {
            p.1 += dt;
        }
        while trail.points.front().is_some_and(|p| p.1 > life) {
            trail.points.pop_front();
        }

        // emit
        match trail.source.and_then(|s| sources.get(s).ok()) {
            Some((gt, state, missile, disabled)) => {
                let intensity = match (trail.kind, state, missile) {
                    (TrailKind::Vortex, Some(s), _) => {
                        let g = ((s.g_load.abs() - 4.0) / 4.0).clamp(0.0, 1.0);
                        let aoa = ((s.alpha.abs().to_degrees() - 12.0) / 10.0).clamp(0.0, 1.0);
                        g.max(aoa) * if disabled { 0.0 } else { 1.0 }
                    }
                    (TrailKind::Contrail, Some(_), _) => {
                        ((gt.translation().y - 3_500.0) / 1_500.0).clamp(0.0, 1.0)
                    }
                    (TrailKind::MissileSmoke, _, Some(m)) => {
                        if m.motor_burning() { 1.0 } else { 0.0 }
                    }
                    _ => 0.0,
                };
                let pos = gt.transform_point(trail.offset);
                // newest point always tracks the source; drop a new one every few metres
                let far_enough = trail
                    .points
                    .back()
                    .is_none_or(|p| p.0.distance(pos) > 6.0 || p.1 > 0.08);
                if far_enough {
                    trail.points.push_back((pos, 0.0, intensity));
                } else if let Some(last) = trail.points.back_mut() {
                    *last = (pos, 0.0, intensity);
                }
            }
            None => {
                // source is gone: let the trail fade out, then remove it
                trail.source = None;
                if trail.points.is_empty() {
                    commands.entity(entity).despawn();
                    continue;
                }
            }
        }

        let visible = trail.points.len() >= 2 && trail.points.iter().any(|p| p.2 > 0.01);
        *visibility = if visible { Visibility::Inherited } else { Visibility::Hidden };
        if !visible {
            continue;
        }

        // rebuild the ribbon, facing the camera
        let n = trail.points.len();
        let mut positions = Vec::with_capacity(n * 2);
        let mut colors = Vec::with_capacity(n * 2);
        let mut normals = Vec::with_capacity(n * 2);
        let mut indices = Vec::with_capacity((n - 1) * 6);
        let pts: Vec<_> = trail.points.iter().copied().collect();
        for i in 0..n {
            let (p, age, intensity) = pts[i];
            let prev = pts[i.saturating_sub(1)].0;
            let next = pts[(i + 1).min(n - 1)].0;
            let tangent = (next - prev).normalize_or(Vec3::Z);
            let to_cam = (cam_pos - p).normalize_or(Vec3::Y);
            let side = tangent.cross(to_cam).normalize_or(Vec3::X);
            let t = age / life;
            let width = w0 + (w1 - w0) * t.sqrt();
            // fade with age, and soften the very newest point so it doesn't pop
            let head = if i == n - 1 { 0.0 } else { 1.0 };
            let alpha = color.alpha * intensity * (1.0 - t).powf(1.5) * head;
            for s in [-1.0, 1.0] {
                positions.push((p + side * width * 0.5 * s).to_array());
                colors.push([color.red, color.green, color.blue, alpha]);
                normals.push(to_cam.to_array());
            }
            if i + 1 < n {
                let a = (i * 2) as u32;
                indices.extend_from_slice(&[a, a + 1, a + 2, a + 1, a + 3, a + 2]);
            }
        }
        if let Some(mut mesh) = meshes.get_mut(&trail.mesh) {
            mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, positions);
            mesh.insert_attribute(Mesh::ATTRIBUTE_NORMAL, normals);
            mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, colors);
            mesh.insert_indices(Indices::U32(indices));
        }
    }
}

// ---------------------------------------------------------------- particles

#[derive(Component)]
struct Particle {
    velocity: Vec3,
    age: f32,
    life: f32,
    size: (f32, f32),
    /// colour at start and end; values above 1 glow (and bloom)
    color: (LinearRgba, LinearRgba),
    drag: f32,
    /// upward acceleration (smoke rises, sparks fall)
    lift: f32,
}

#[derive(Component)]
struct Flash {
    age: f32,
    life: f32,
    peak: f32,
}

#[allow(clippy::too_many_arguments)]
fn spawn_particle(
    commands: &mut Commands,
    assets: &EffectAssets,
    materials: &mut Assets<StandardMaterial>,
    pos: Vec3,
    particle: Particle,
) {
    let material = materials.add(StandardMaterial {
        base_color: particle.color.0.into(),
        base_color_texture: Some(assets.blob.clone()),
        alpha_mode: AlphaMode::Blend,
        unlit: true,
        ..default()
    });
    commands.spawn((
        Mesh3d(assets.billboard.clone()),
        MeshMaterial3d(material),
        Transform::from_translation(pos).with_scale(Vec3::splat(particle.size.0)),
        bevy::light::NotShadowCaster,
        particle,
    ));
}

fn rand_dir(rng: &mut impl rand::Rng) -> Vec3 {
    loop {
        let v = Vec3::new(
            rng.random_range(-1.0..1.0),
            rng.random_range(-1.0..1.0),
            rng.random_range(-1.0..1.0),
        );
        if v.length_squared() <= 1.0 && v.length_squared() > 0.01 {
            return v.normalize();
        }
    }
}

fn spawn_explosions(
    mut commands: Commands,
    mut explosions: MessageReader<Explosion>,
    assets: Res<EffectAssets>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    particles: Query<(), With<Particle>>,
) {
    let mut rng = rand::rng();
    let mut budget = MAX_PARTICLES.saturating_sub(particles.iter().count());
    for e in explosions.read() {
        let s = e.size;
        let small = s < 3.0;
        let fire = if small { 2 } else { (6.0 + s * 0.5) as usize };
        let smoke = if small { 1 } else { (4.0 + s * 0.35) as usize };
        let sparks = if small { 3 } else { 10 };
        if budget < fire + smoke + sparks {
            continue;
        }
        budget -= fire + smoke + sparks;

        // light flash
        if !small {
            commands.spawn((
                PointLight {
                    intensity: 0.0,
                    range: s * 40.0,
                    color: Color::srgb(1.0, 0.65, 0.3),
                    shadow_maps_enabled: false,
                    ..default()
                },
                Transform::from_translation(e.position),
                Flash {
                    age: 0.0,
                    life: 0.5,
                    peak: s * s * 4e7,
                },
            ));
        }
        for _ in 0..fire {
            let life = rng.random_range(0.4..1.0) * if small { 0.4 } else { 1.0 };
            spawn_particle(&mut commands, &assets, &mut materials, e.position + rand_dir(&mut rng) * s * 0.3, Particle {
                velocity: rand_dir(&mut rng) * s * rng.random_range(1.0..4.0),
                age: 0.0,
                life,
                size: (s * 0.25, s * rng.random_range(0.7..1.3)),
                color: (LinearRgba::new(5.0, 1.6, 0.3, 0.75), LinearRgba::new(0.1, 0.04, 0.03, 0.0)),
                drag: 2.5,
                lift: 3.0,
            });
        }
        for _ in 0..smoke {
            let grey = rng.random_range(0.08..0.25);
            spawn_particle(&mut commands, &assets, &mut materials, e.position + rand_dir(&mut rng) * s * 0.4, Particle {
                velocity: rand_dir(&mut rng) * s * 0.8 + Vec3::Y * 2.0,
                age: 0.0,
                life: rng.random_range(2.5..5.0) * if small { 0.3 } else { 1.0 },
                size: (s * 0.4, s * rng.random_range(1.8..2.8)),
                color: (LinearRgba::new(grey, grey, grey, 0.75), LinearRgba::new(grey * 1.6, grey * 1.6, grey * 1.6, 0.0)),
                drag: 1.2,
                lift: 2.5,
            });
        }
        for _ in 0..sparks {
            spawn_particle(&mut commands, &assets, &mut materials, e.position, Particle {
                velocity: rand_dir(&mut rng) * rng.random_range(30.0..90.0) * if small { 0.5 } else { 1.0 },
                age: 0.0,
                life: rng.random_range(0.3..0.9),
                size: (0.25, 0.1),
                color: (LinearRgba::new(60.0, 35.0, 10.0, 1.0), LinearRgba::new(8.0, 2.0, 0.2, 0.0)),
                drag: 0.5,
                lift: -9.8,
            });
        }
    }
}

/// Damaged aircraft trail smoke; shot-down ones burn.
fn damage_smoke(
    mut commands: Commands,
    time: Res<Time>,
    assets: Res<EffectAssets>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    aircraft: Query<(&Transform, &FlightState, &Health, Has<Disabled>)>,
    particles: Query<(), With<Particle>>,
    mut accumulator: Local<f32>,
) {
    *accumulator += time.delta_secs();
    // emit at a fixed rate regardless of frame rate
    let step = 0.05;
    if *accumulator < step {
        return;
    }
    *accumulator %= step;
    let mut budget = MAX_PARTICLES.saturating_sub(particles.iter().count());
    let mut rng = rand::rng();
    for (transform, state, health, disabled) in &aircraft {
        let damage = 1.0 - health.current / health.max;
        if damage < 0.45 && !disabled {
            continue;
        }
        if budget < 2 {
            break;
        }
        budget -= 2;
        let tail = transform.transform_point(Vec3::new(0.0, 0.0, 6.0));
        let grey = if disabled { 0.05 } else { 0.35 - damage * 0.25 };
        spawn_particle(&mut commands, &assets, &mut materials, tail, Particle {
            velocity: state.velocity * 0.15 + rand_dir(&mut rng) * 3.0,
            age: 0.0,
            life: if disabled { 4.0 } else { 2.0 },
            size: (1.5, if disabled { 9.0 } else { 5.0 }),
            color: (LinearRgba::new(grey, grey, grey, 0.7), LinearRgba::new(grey, grey, grey, 0.0)),
            drag: 1.5,
            lift: 1.0,
        });
        if disabled {
            spawn_particle(&mut commands, &assets, &mut materials, tail, Particle {
                velocity: state.velocity * 0.4 + rand_dir(&mut rng) * 4.0,
                age: 0.0,
                life: 0.5,
                size: (2.5, 4.0),
                color: (LinearRgba::new(25.0, 9.0, 1.5, 0.9), LinearRgba::new(0.3, 0.05, 0.0, 0.0)),
                drag: 2.0,
                lift: 2.0,
            });
        }
    }
}

fn update_particles(
    mut commands: Commands,
    time: Res<Time>,
    camera: Query<&Transform, (With<MainCamera>, Without<Particle>)>,
    mut particles: Query<(Entity, &mut Particle, &mut Transform, &MeshMaterial3d<StandardMaterial>)>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let dt = time.delta_secs();
    let facing = camera.single().map_or(Quat::IDENTITY, |c| c.rotation);
    for (entity, mut p, mut transform, material) in &mut particles {
        transform.rotation = facing;
        p.age += dt;
        if p.age >= p.life {
            materials.remove(&material.0);
            commands.entity(entity).despawn();
            continue;
        }
        let t = p.age / p.life;
        let drag = p.drag;
        let lift = p.lift;
        p.velocity *= (1.0 - drag * dt).max(0.0);
        p.velocity.y += lift * dt;
        transform.translation += p.velocity * dt;
        transform.scale = Vec3::splat(p.size.0 + (p.size.1 - p.size.0) * t.sqrt());
        if let Some(mut m) = materials.get_mut(&material.0) {
            m.base_color = lerp_color(p.color.0, p.color.1, t).into();
        }
    }
}

fn lerp_color(a: LinearRgba, b: LinearRgba, t: f32) -> LinearRgba {
    LinearRgba::new(
        a.red + (b.red - a.red) * t,
        a.green + (b.green - a.green) * t,
        a.blue + (b.blue - a.blue) * t,
        a.alpha + (b.alpha - a.alpha) * t,
    )
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
        light.intensity = flash.peak * (1.0 - t).powi(2);
    }
}
