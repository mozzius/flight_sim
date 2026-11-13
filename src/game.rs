//! Teams, health, spawning, death and respawn, and the waves (bigger and
//! sharper ones in challenge mode, see `modes`).

use bevy::prelude::*;
use bevy_transform_interpolation::prelude::*;
use rand::RngExt;

use crate::{
    ai::Pilot,
    controls::MouseAim,
    modes::{GameMode, KILL_POINTS, WAVE_POINTS},
    flight::{Aircraft, AircraftCrashed, ControlInput, Disabled, FlightState, FlightSystems},
    weapons::{Targeting, Weapons},
    world::Terrain,
};

pub struct GamePlugin;

impl Plugin for GamePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Score>()
            .init_resource::<RespawnQueue>()
            .add_message::<Explosion>()
            .add_message::<AircraftDestroyed>()
            .add_message::<PlayerKill>()
            .add_systems(Startup, load_models)
            .add_systems(OnEnter(crate::loading::GameState::Playing), initial_spawn)
            .add_systems(OnExit(crate::loading::GameState::Playing), (end_round, crate::terrain_gen::clear_map))
            .add_systems(
                FixedUpdate,
                (
                    handle_damage,
                    handle_crashes,
                    respawn,
                    // (multiplayer: other players, no waves)
                    wave_system.run_if(|mode: Res<GameMode>| *mode != GameMode::Multiplayer),
                )
                    .chain()
                    .after(FlightSystems::Combat)
                    .run_if(in_state(crate::loading::GameState::Playing)),
            );
    }
}

#[derive(Component, Clone, Copy, PartialEq, Eq, Debug)]
pub enum Team {
    Blue,
    Red,
}

#[derive(Component)]
pub struct Player;

#[derive(Component)]
pub struct Health {
    pub current: f32,
    pub max: f32,
    /// Who last hurt us, for kill credit.
    pub last_attacker: Option<Entity>,
}

impl Health {
    pub fn new(max: f32) -> Self {
        Health {
            current: max,
            max,
            last_attacker: None,
        }
    }
}

/// Visual explosion request, handled by the effects module.
#[derive(Message, Clone, Copy)]
pub struct Explosion {
    pub position: Vec3,
    pub size: f32,
}

/// The player got a kill (drives the HUD banner and the kill chime).
#[derive(Message, Clone, Copy)]
pub struct PlayerKill {
    pub total: u32,
    pub friendly: bool,
}

/// An aircraft was shot down (it still falls until it hits the ground).
#[derive(Message, Clone, Copy)]
pub struct AircraftDestroyed {
    pub entity: Entity,
    pub killer: Option<Entity>,
}

#[derive(Resource, Default)]
pub struct Score {
    pub kills: u32,
    pub deaths: u32,
    pub wave: u32,
    /// Challenge score: kills and cleared waves.
    pub points: u32,
    /// Shown briefly on the HUD ("TARGET DESTROYED").
    pub last_event: Option<(String, f32)>,
}

#[derive(Resource, Default)]
struct RespawnQueue {
    /// (seconds remaining, team, is player)
    pending: Vec<(f32, Team, bool)>,
}

#[derive(Resource)]
pub struct Models {
    pub fighter: Handle<WorldAsset>,
}

/// The F-22 model is authored ~6x too big and facing +X.
const MODEL_SCALE: f32 = 0.166;

fn load_models(mut commands: Commands, asset_server: Res<AssetServer>) {
    commands.insert_resource(Models {
        fighter: asset_server
            .load(GltfAssetLabel::Scene(0).from_asset("models/f22-raptor/scene.gltf")),
    });
}

/// Marker for the visual model child of an aircraft (effects attach here).
#[derive(Component)]
pub struct AircraftModel;

#[allow(clippy::too_many_arguments)]
pub fn spawn_aircraft(
    commands: &mut Commands,
    models: &Models,
    terrain: &Terrain,
    team: Team,
    position: Vec3,
    heading: Vec3,
    player: bool,
) -> Entity {
    // never spawn inside (or skimming) a mountain: clear the ground ahead by 500 m
    let ahead = position + heading.normalize_or(Vec3::NEG_Z) * 4_000.0;
    let ground = terrain.max_height_along(position, ahead);
    let position = Vec3::new(position.x, position.y.max(ground + 500.0), position.z);
    let transform = Transform::from_translation(position).looking_to(heading, Vec3::Y);
    let speed = 240.0;
    let mut entity = commands.spawn((
        Name::new(if player { "Player".to_string() } else { format!("{team:?} fighter") }),
        transform,
        Visibility::default(),
        TransformInterpolation,
        Aircraft::fighter(),
        FlightState::new(heading.normalize() * speed),
        ControlInput {
            throttle: 0.75,
            ..default()
        },
        team,
        Health::new(100.0),
        Weapons::fighter(),
        Targeting::default(),
    ));
    entity.with_child((
        AircraftModel,
        WorldAssetRoot(models.fighter.clone()),
        Transform::from_xyz(0.0, -0.8, 0.0)
            .with_rotation(Quat::from_rotation_y(-std::f32::consts::FRAC_PI_2))
            .with_scale(Vec3::splat(MODEL_SCALE)),
    ));
    if player {
        entity.insert(Player);
    } else {
        entity.insert(Pilot::new());
    }
    entity.id()
}

/// Another player's jet (multiplayer): flown by their game, so no physics
/// or pilot here, just what the effects, HUD and weapons look at.
pub fn spawn_remote_aircraft(commands: &mut Commands, models: &Models, transform: Transform, velocity: Vec3, remote: crate::net::Remote, name: &str) -> Entity {
    commands
        .spawn((
            Name::new(format!("{name} (remote)")),
            transform,
            Visibility::default(),
            Aircraft::fighter(),
            FlightState::new(velocity),
            Team::Red,
            Health::new(crate::net::protocol::MAX_HEALTH),
            Weapons::fighter(),
            remote,
            crate::net::RemoteName(name.to_string()),
        ))
        .with_child((
            AircraftModel,
            WorldAssetRoot(models.fighter.clone()),
            Transform::from_xyz(0.0, -0.8, 0.0)
                .with_rotation(Quat::from_rotation_y(-std::f32::consts::FRAC_PI_2))
                .with_scale(Vec3::splat(MODEL_SCALE)),
        ))
        .id()
}

fn initial_spawn(
    mut commands: Commands,
    models: Res<Models>,
    terrain: Res<Terrain>,
    mode: Res<GameMode>,
    mut score: ResMut<Score>,
    mut aim: ResMut<MouseAim>,
) {
    let north = Vec3::NEG_Z;
    if *mode == GameMode::Multiplayer {
        let (pos, heading) = crate::net::spawn_point();
        spawn_aircraft(&mut commands, &models, &terrain, Team::Blue, pos, heading, true);
        aim.reset = true;
        return;
    }
    spawn_aircraft(&mut commands, &models, &terrain, Team::Blue, Vec3::new(0.0, 1500.0, 6000.0), north, true);
    aim.reset = true;
    // wingmen in a loose finger-four (a pair in a challenge, and they don't come back)
    let wingmen = if *mode == GameMode::Challenge { 2 } else { 3 };
    for offset in [
        Vec3::new(-150.0, 20.0, 120.0),
        Vec3::new(150.0, -20.0, 120.0),
        Vec3::new(300.0, 0.0, 240.0),
    ]
    .into_iter()
    .take(wingmen)
    {
        spawn_aircraft(&mut commands, &models, &terrain, Team::Blue, Vec3::new(0.0, 1500.0, 6000.0) + offset, north, false);
    }
    spawn_enemy_wave(&mut commands, &models, &terrain, *mode, &mut score, Vec3::new(0.0, 1500.0, 6000.0), north);
}

/// Resets the round's state when it ends (back to the menu, or a retry).
#[allow(clippy::too_many_arguments)]
fn end_round(
    mut commands: Commands,
    leftovers: Query<
        Entity,
        Or<(With<Aircraft>, With<crate::weapons::Bullet>, With<crate::weapons::Missile>, With<crate::weapons::Flare>)>,
    >,
    mut score: ResMut<Score>,
    mut queue: ResMut<RespawnQueue>,
    mut prompt: ResMut<crate::respawn::RespawnPrompt>,
    mut intro: ResMut<crate::intro::Intro>,
    mut paused: ResMut<crate::pause::Paused>,
    mut time: ResMut<Time<Virtual>>,
) {
    for e in &leftovers {
        commands.entity(e).despawn();
    }
    *score = Score::default();
    queue.pending.clear();
    *prompt = default();
    *intro = crate::intro::Intro::new();
    paused.0 = false;
    time.unpause();
}

fn spawn_enemy_wave(
    commands: &mut Commands,
    models: &Models,
    terrain: &Terrain,
    mode: GameMode,
    score: &mut Score,
    player_pos: Vec3,
    player_heading: Vec3,
) {
    score.wave += 1;
    let challenge = mode == GameMode::Challenge;
    // challenge: 3, 5, 7... hostiles, sharper each wave
    let count = if challenge { (1 + score.wave * 2).min(25) } else { (4 + score.wave * 2).min(12) };
    let mut rng = rand::rng();
    let ahead = Vec3::new(player_heading.x, 0.0, player_heading.z).normalize_or(Vec3::NEG_Z);
    let centre = Vec3::new(player_pos.x, 0.0, player_pos.z) + ahead * 10_000.0;
    let side = ahead.cross(Vec3::Y);
    for i in 0..count {
        let lateral = (i as f32 - count as f32 / 2.0) * 220.0;
        let pos = centre
            + side * lateral
            + ahead * rng.random_range(-300.0..300.0)
            + Vec3::Y * rng.random_range(1300.0..2600.0);
        let e = spawn_aircraft(commands, models, terrain, Team::Red, pos, -ahead, false);
        if challenge {
            let mut pilot = Pilot::new();
            let base = 0.4 + 0.06 * (score.wave - 1) as f32;
            pilot.skill = (base + rng.random_range(0.0..0.3)).min(1.0);
            commands.entity(e).insert(pilot);
        }
    }
    score.last_event = Some((format!("WAVE {}: {} HOSTILES", score.wave, count), 4.0));
}

fn handle_damage(
    mut commands: Commands,
    query: Query<(Entity, &Health), Without<Disabled>>,
    mut destroyed: MessageWriter<AircraftDestroyed>,
    mut explosions: MessageWriter<Explosion>,
    transforms: Query<&Transform>,
) {
    for (entity, health) in &query {
        if health.current <= 0.0 {
            commands.entity(entity).insert(Disabled).remove::<Pilot>();
            destroyed.write(AircraftDestroyed {
                entity,
                killer: health.last_attacker,
            });
            if let Ok(t) = transforms.get(entity) {
                explosions.write(Explosion {
                    position: t.translation,
                    size: 12.0,
                });
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_crashes(
    mut commands: Commands,
    mut crashes: MessageReader<AircraftCrashed>,
    mut destroyed_reader: MessageReader<AircraftDestroyed>,
    mut explosions: MessageWriter<Explosion>,
    mut score: ResMut<Score>,
    mut queue: ResMut<RespawnQueue>,
    mode: Res<GameMode>,
    aircraft: Query<(&Team, Has<Player>, Has<Disabled>, &Health)>,
    players: Query<(), With<Player>>,
    mut kills: MessageWriter<PlayerKill>,
) {
    // kills and deaths are counted when a plane is shot down
    for d in destroyed_reader.read() {
        let Ok((team, is_player, _, _)) = aircraft.get(d.entity) else {
            continue;
        };
        if is_player {
            score.deaths += 1;
            score.last_event = Some(("YOU WERE SHOT DOWN".into(), 4.0));
        } else if d.killer.is_some_and(|k| players.contains(k)) {
            score.kills += 1;
            if *team == Team::Red {
                score.points += KILL_POINTS;
            }
            score.last_event = Some((
                if *team == Team::Red { "TARGET DESTROYED" } else { "FRIENDLY FIRE!" }.into(),
                2.5,
            ));
            kills.write(PlayerKill {
                total: score.kills,
                friendly: *team != Team::Red,
            });
        }
    }

    let mut seen = Vec::new();
    for crash in crashes.read() {
        if seen.contains(&crash.entity) {
            continue;
        }
        seen.push(crash.entity);
        let Ok((team, is_player, disabled, health)) = aircraft.get(crash.entity) else {
            continue;
        };
        explosions.write(Explosion {
            position: crash.position,
            size: 25.0,
        });
        if is_player && !disabled {
            score.deaths += 1;
            score.last_event = Some(("CRASHED".into(), 4.0));
        } else if !disabled && health.last_attacker.is_some_and(|k| players.contains(k)) {
            // chased into the ground counts
            score.kills += 1;
            if *team == Team::Red {
                score.points += KILL_POINTS;
            }
            score.last_event = Some(("TARGET DESTROYED".into(), 2.5));
            kills.write(PlayerKill {
                total: score.kills,
                friendly: *team != Team::Red,
            });
        }
        commands.entity(crash.entity).despawn();
        // the player respawns from the RESPAWN button (see `respawn`)
        if !is_player && *team == Team::Blue && *mode == GameMode::Endless {
            queue.pending.push((20.0, *team, false));
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn respawn(
    mut commands: Commands,
    time: Res<Time>,
    models: Res<Models>,
    terrain: Res<Terrain>,
    mut queue: ResMut<RespawnQueue>,
    mut score: ResMut<Score>,
    mut aim: ResMut<MouseAim>,
    player: Query<&Transform, With<Player>>,
    blues: Query<&Transform, (With<Pilot>, Without<Player>)>,
) {
    let dt = time.delta_secs();
    if let Some((_, t)) = &mut score.last_event {
        *t -= dt;
        if *t <= 0.0 {
            score.last_event = None;
        }
    }

    // anchor respawns on the player (or any friendly if the player is dead)
    let anchor = player
        .single()
        .ok()
        .or_else(|| blues.iter().next())
        .map(|t| (t.translation, t.forward().as_vec3()))
        .unwrap_or((Vec3::new(0.0, 1500.0, 6000.0), Vec3::NEG_Z));

    let mut ready = Vec::new();
    queue.pending.retain_mut(|(t, team, is_player)| {
        *t -= dt;
        if *t <= 0.0 {
            ready.push((*team, *is_player));
            false
        } else {
            true
        }
    });
    for (team, is_player) in ready {
        let heading = Vec3::new(anchor.1.x, 0.0, anchor.1.z).normalize_or(Vec3::NEG_Z);
        let back = if is_player { 0.0 } else { 1500.0 };
        let pos = Vec3::new(anchor.0.x, 1800.0, anchor.0.z) - heading * back
            + heading.cross(Vec3::Y) * if is_player { 0.0 } else { 300.0 };
        spawn_aircraft(&mut commands, &models, &terrain, team, pos, heading, is_player);
        if is_player {
            aim.reset = true;
        }
    }

}

/// Spawns a new enemy wave when no hostile is left alive.
pub fn wave_system(
    mut commands: Commands,
    models: Res<Models>,
    terrain: Res<Terrain>,
    mode: Res<GameMode>,
    mut score: ResMut<Score>,
    reds: Query<&Team, Without<Disabled>>,
    mut player: Query<(&Transform, &mut Health, &mut Weapons), (With<Player>, Without<Disabled>)>,
    mut cooldown: Local<f32>,
    mut cleared: Local<bool>,
    time: Res<Time>,
) {
    if reds.iter().any(|t| *t == Team::Red) {
        *cooldown = 5.0;
        *cleared = false;
        return;
    }
    // a challenge wave cleared (alive): bonus points, and patched up for the next
    if !*cleared && score.wave > 0 && *mode == GameMode::Challenge {
        if let Ok((_, mut health, mut weapons)) = player.single_mut() {
            score.points += WAVE_POINTS * score.wave;
            health.current = health.max;
            weapons.refill_flares();
        }
        *cleared = true;
    }
    *cooldown -= time.delta_secs();
    if *cooldown > 0.0 {
        return;
    }
    let Ok((t, ..)) = player.single() else {
        return;
    };
    spawn_enemy_wave(&mut commands, &models, &terrain, *mode, &mut score, t.translation, t.forward().as_vec3());
    *cooldown = 5.0;
}
