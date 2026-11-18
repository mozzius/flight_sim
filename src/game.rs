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
    jets::{JetChoice, Stick},
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
            .add_systems(Update, (fall_back_to_f22.run_if(resource_exists::<Models>), warm_jet_shaders).chain())
            .add_systems(OnEnter(crate::loading::GameState::Playing), initial_spawn)
            .add_systems(OnExit(crate::loading::GameState::Playing), (end_round, crate::terrain_gen::clear_map))
            .add_systems(
                FixedUpdate,
                (
                    handle_damage,
                    wreck_pops,
                    handle_crashes,
                    respawn,
                    // (multiplayer: other players; the tutorial brings its own)
                    wave_system.run_if(|mode: Res<GameMode>| matches!(*mode, GameMode::Endless | GameMode::Challenge)),
                )
                    .chain()
                    .in_set(RoundSystems)
                    .after(FlightSystems::Combat)
                    .run_if(in_state(crate::loading::GameState::Playing)),
            );
    }
}

/// Damage, crashes, respawns and waves (after the weapons, in `FixedUpdate`).
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct RoundSystems;

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
#[derive(Message, Clone)]
pub struct PlayerKill {
    pub total: u32,
    pub friendly: bool,
    /// Another player's handle (multiplayer).
    pub name: Option<String>,
    /// Where it blew up (the boom is played from there).
    pub position: Option<Vec3>,
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

/// Every jet in every livery, loaded up front (half-size textures on phones),
/// plus the F-22 for when the jets are missing (`jets::fallback`).
#[derive(Resource)]
pub struct Models {
    jets: Vec<(JetChoice, Handle<WorldAsset>)>,
    f22: Handle<WorldAsset>,
}

impl Models {
    pub fn scene(&self, choice: JetChoice) -> Handle<WorldAsset> {
        if crate::jets::fallback() {
            return self.f22.clone();
        }
        self.jets.iter().find(|(c, _)| *c == choice).unwrap_or(&self.jets[0]).1.clone()
    }

    fn all_loaded(&self, asset_server: &AssetServer) -> bool {
        if crate::jets::fallback() {
            return asset_server.is_loaded_with_dependencies(&self.f22);
        }
        self.jets.iter().all(|(_, h)| asset_server.is_loaded_with_dependencies(h))
    }
}

fn load_models(mut commands: Commands, asset_server: Res<AssetServer>) {
    let low = crate::platform::low_memory();
    commands.insert_resource(Models {
        jets: JetChoice::all()
            .map(|c| (c, asset_server.load(GltfAssetLabel::Scene(0).from_asset(c.path(low)))))
            .collect(),
        f22: asset_server.load(GltfAssetLabel::Scene(0).from_asset("models/f22-raptor/scene.gltf")),
    });
}

/// No jets folder (a public clone, without the private assets repo): fly the
/// F-22, swapping any jet already spawned.
fn fall_back_to_f22(
    models: Res<Models>,
    asset_server: Res<AssetServer>,
    mut spawned: Query<(&mut WorldAssetRoot, &mut Transform), With<AircraftModel>>,
) {
    if crate::jets::fallback() || !asset_server.load_state(&models.jets[0].1).is_failed() {
        return;
    }
    warn!("no jets in assets/models/jets (run scripts/fetch-assets.sh): flying the F-22");
    crate::jets::set_fallback();
    for (mut root, mut transform) in &mut spawned {
        root.0 = models.f22.clone();
        *transform = model_transform();
    }
}

/// The jets' glTFs are authored in aircraft space; the F-22 ~6x too big and facing +X.
fn model_transform() -> Transform {
    if crate::jets::fallback() {
        Transform::from_xyz(0.0, -0.8, 0.0)
            .with_rotation(Quat::from_rotation_y(-std::f32::consts::FRAC_PI_2))
            .with_scale(Vec3::splat(0.166))
    } else {
        Transform::IDENTITY
    }
}

/// A jet's model, as a child of its aircraft.
fn model(models: &Models, choice: JetChoice) -> impl Bundle {
    (AircraftModel, WorldAssetRoot(models.scene(choice)), model_transform())
}

/// A speck of a jet held in front of the camera for a few frames once the
/// model has loaded, so its shaders compile then rather than when the first
/// jet flies into view (a visible freeze on macOS and the web).
#[derive(Component)]
struct ShaderWarmer(u32);

fn warm_jet_shaders(
    mut commands: Commands,
    models: Option<Res<Models>>,
    asset_server: Res<AssetServer>,
    camera: Query<Entity, With<crate::camera::MainCamera>>,
    mut warmer: Query<(Entity, &mut ShaderWarmer)>,
    mut done: Local<bool>,
) {
    if *done {
        return;
    }
    let Some(models) = models else { return };
    if let Ok((e, mut w)) = warmer.single_mut() {
        if models.all_loaded(&asset_server) {
            w.0 += 1;
            if w.0 > 8 {
                commands.entity(e).despawn();
                *done = true;
                // the page's loading overlay waits for this (web/index.html)
                #[cfg(target_arch = "wasm32")]
                let _ = js_sys::Reflect::set(&js_sys::global(), &"FLIGHT_SIM_WARM".into(), &true.into());
            }
        }
        return;
    }
    let Ok(cam) = camera.single() else { return };
    let warm = commands
        .spawn((
            ShaderWarmer(0),
            // far too small to see, but in view, so it's drawn
            Transform::from_xyz(0.0, 0.0, -30.0).with_scale(Vec3::splat(0.0004)),
            Visibility::default(),
        ))
        .with_children(|p| {
            if crate::jets::fallback() {
                p.spawn((WorldAssetRoot(models.f22.clone()), Transform::IDENTITY));
            }
            for (_, scene) in models.jets.iter().filter(|_| !crate::jets::fallback()) {
                p.spawn((WorldAssetRoot(scene.clone()), Transform::IDENTITY));
            }
        })
        .id();
    commands.entity(cam).add_child(warm);
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
    // `FLIGHT_SIM_ALT=<m>` (dev): start the player's jet at that altitude
    let position = match crate::dev::start_altitude() {
        Some(alt) if player => Vec3::new(position.x, alt, position.z),
        _ => position,
    };
    // never spawn inside (or skimming) a mountain: clear the ground ahead by 500 m
    let ahead = position + heading.normalize_or(Vec3::NEG_Z) * 4_000.0;
    let ground = terrain.max_height_along(position, ahead);
    let position = Vec3::new(position.x, position.y.max(ground + 500.0), position.z);
    let transform = Transform::from_translation(position).looking_to(heading, Vec3::Y);
    let jet = if player {
        crate::jets::player()
    } else {
        // (mixed flights: count the AI as they spawn)
        static AI: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        crate::jets::ai_jet(team == Team::Red, AI.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
    };
    let speed = jet.airframe().cruise;
    let mut entity = commands.spawn((
        Name::new(if player { "Player".to_string() } else { format!("{team:?} fighter") }),
        transform,
        Visibility::default(),
        TransformInterpolation,
        jet.aircraft(),
        FlightState::new(heading.normalize() * speed),
        ControlInput {
            throttle: 0.75,
            ..default()
        },
        team,
        Health::new(100.0),
        Weapons::for_jet(jet),
        Targeting::default(),
        (jet, Stick::default()),
    ));
    entity.with_child(model(models, jet));
    if player {
        entity.insert(Player);
    } else {
        entity.insert(Pilot::new());
    }
    entity.id()
}

/// Another player's jet (multiplayer): flown by their game, so no physics
/// or pilot here, just what the effects, HUD and weapons look at.
pub fn spawn_remote_aircraft(commands: &mut Commands, models: &Models, transform: Transform, velocity: Vec3, remote: crate::net::Remote, name: &str, jet: JetChoice) -> Entity {
    commands
        .spawn((
            Name::new(format!("{name} (remote)")),
            transform,
            Visibility::default(),
            jet.aircraft(),
            FlightState::new(velocity),
            Team::Red,
            Health::new(crate::net::protocol::MAX_HEALTH),
            Weapons::for_jet(jet),
            remote,
            crate::net::RemoteName(name.to_string()),
            (jet, Stick::default()),
        ))
        .with_child(model(models, jet))
        .id()
}

fn initial_spawn(
    mut commands: Commands,
    models: Res<Models>,
    terrain: Res<Terrain>,
    mode: Res<GameMode>,
    mut score: ResMut<Score>,
    mut aim: ResMut<MouseAim>,
    settings: Res<crate::settings::Settings>,
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
    // (the tutorial brings its own targets, and no wingmen)
    if *mode == GameMode::Tutorial {
        return;
    }
    // wingmen in loose echelons either side (a pair in a challenge, and they don't come back;
    // a scenario takes the ALLIES pick)
    let wingmen = if *mode == GameMode::Challenge { 2 } else { settings.allies as usize };
    for k in 1..=wingmen {
        let side = if k % 2 == 1 { -1.0 } else { 1.0 };
        let rank = k.div_ceil(2) as f32;
        let offset = Vec3::new(side * 150.0 * rank, if k % 2 == 1 { 20.0 } else { -20.0 }, 120.0 * rank);
        spawn_aircraft(&mut commands, &models, &terrain, Team::Blue, Vec3::new(0.0, 1500.0, 6000.0) + offset, north, false);
    }
    let difficulty = crate::difficulty::active(*mode, settings.difficulty);
    spawn_enemy_wave(&mut commands, &models, &terrain, *mode, difficulty, settings.enemies, &mut score, Vec3::new(0.0, 1500.0, 6000.0), north);
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

#[allow(clippy::too_many_arguments)]
fn spawn_enemy_wave(
    commands: &mut Commands,
    models: &Models,
    terrain: &Terrain,
    mode: GameMode,
    difficulty: &crate::difficulty::DifficultyParams,
    // the ENEMIES pick (0: by the difficulty)
    enemies: u8,
    score: &mut Score,
    player_pos: Vec3,
    player_heading: Vec3,
) {
    score.wave += 1;
    let challenge = mode == GameMode::Challenge;
    // challenge: 3, 5, 7... hostiles, sharper each wave; a scenario: the ENEMIES pick, or by difficulty
    let count = if challenge {
        (1 + score.wave * 2).min(25)
    } else if enemies > 0 {
        enemies as u32
    } else {
        difficulty.wave_size(score.wave)
    };
    let mut skills = 0.0;
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
        let mut pilot = Pilot::new();
        pilot.skill = if challenge {
            let base = 0.4 + 0.06 * (score.wave - 1) as f32;
            (base + rng.random_range(0.0..0.3)).min(1.0)
        } else {
            difficulty.skill_at(rng.random_range(0.0..1.0))
        };
        skills += pilot.skill;
        commands.entity(e).insert(pilot);
    }
    info!("wave {}: {count} hostiles, mean skill {:.2}", score.wave, skills / count as f32);
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
                    size: KILL_BLAST,
                });
            }
            let mut rng = rand::rng();
            commands.entity(entity).insert(Wreck { pops: rng.random_range(2..=4), next: rng.random_range(0.2..0.45) });
        }
    }
}

/// The blast when a jet is shot down (under 20, which `effects` reads as
/// hitting the ground).
pub const KILL_BLAST: f32 = 18.0;

/// A shot-down jet still blowing apart as it falls: a few more blasts.
#[derive(Component)]
struct Wreck {
    pops: u32,
    next: f32,
}

fn wreck_pops(
    mut commands: Commands,
    time: Res<Time>,
    mut wrecks: Query<(Entity, &Transform, &mut Wreck)>,
    mut explosions: MessageWriter<Explosion>,
) {
    let mut rng = rand::rng();
    for (e, t, mut w) in &mut wrecks {
        w.next -= time.delta_secs();
        if w.next > 0.0 {
            continue;
        }
        let jitter = Vec3::new(rng.random_range(-4.0..4.0), rng.random_range(-2.0..3.0), rng.random_range(-6.0..6.0));
        explosions.write(Explosion { position: t.translation + t.rotation * jitter, size: rng.random_range(7.5..11.0) });
        w.pops -= 1;
        w.next = rng.random_range(0.25..0.6);
        if w.pops == 0 {
            commands.entity(e).remove::<Wreck>();
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
    aircraft: Query<(&Team, Has<Player>, Has<Disabled>, &Health, Option<&crate::net::RemoteName>)>,
    players: Query<(), With<Player>>,
    transforms: Query<&Transform>,
    mut kills: MessageWriter<PlayerKill>,
) {
    // kills and deaths are counted when a plane is shot down
    for d in destroyed_reader.read() {
        let Ok((team, is_player, _, _, name)) = aircraft.get(d.entity) else {
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
                name: name.map(|n| n.0.clone()),
                position: transforms.get(d.entity).ok().map(|t| t.translation),
            });
        }
    }

    let mut seen = Vec::new();
    for crash in crashes.read() {
        if seen.contains(&crash.entity) {
            continue;
        }
        seen.push(crash.entity);
        let Ok((team, is_player, disabled, health, name)) = aircraft.get(crash.entity) else {
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
                name: name.map(|n| n.0.clone()),
                position: Some(crash.position),
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
    settings: Res<crate::settings::Settings>,
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
    let difficulty = crate::difficulty::active(*mode, settings.difficulty);
    spawn_enemy_wave(&mut commands, &models, &terrain, *mode, difficulty, settings.enemies, &mut score, t.translation, t.forward().as_vec3());
    *cooldown = 5.0;
}
