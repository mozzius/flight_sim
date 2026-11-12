//! Sound. Every sound is synthesised at startup (no audio assets): filtered
//! noise, oscillators and envelopes, encoded to in-memory WAV clips.
//!
//! - The player's own jet: engine roar + turbine whine (pitch follows spool),
//!   afterburner rumble, wind (airspeed), gun, cockpit tones (seeker growl,
//!   lock tone, missile warning, stall horn). Not spatial.
//! - Everything else is spatial (stereo pan + distance falloff): other jets'
//!   engines and guns with Doppler shift, missile motors, explosions (delayed
//!   by the speed of sound), hits, flares, launches.
//!
//! `N` mutes. Automated test runs (`FLIGHT_SIM_SCREENSHOT`) don't load this
//! plugin at all.

use std::{f32::consts::TAU, sync::Arc};

use bevy::{
    audio::{AudioSinkPlayback, PlaybackMode, SpatialScale, Volume},
    prelude::*,
};

use crate::{
    camera::MainCamera,
    flight::{Aircraft, Disabled, FlightState},
    game::{Explosion, Player},
    weapons::{Flare, Missile, Targeting, Weapons},
};

const SAMPLE_RATE: u32 = 44_100;
const SPEED_OF_SOUND: f32 = 340.0;

pub struct AudioFxPlugin;

impl Plugin for AudioFxPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<DelayedSounds>()
            .init_resource::<Muted>()
            .add_systems(Startup, (synthesise, setup_player_loops).chain())
            .add_systems(
                Update,
                (
                    attach_listener,
                    toggle_mute,
                    player_loops,
                    attach_aircraft_loops,
                    attach_missile_loops,
                    remote_loops,
                    one_shots,
                    play_delayed,
                    feedback_sounds,
                ),
            );
    }
}

#[derive(Resource)]
struct Sounds {
    engine: Handle<AudioSource>,
    afterburner: Handle<AudioSource>,
    wind: Handle<AudioSource>,
    gun: Handle<AudioSource>,
    missile_motor: Handle<AudioSource>,
    launch: Handle<AudioSource>,
    explosion: Handle<AudioSource>,
    explosion_far: Handle<AudioSource>,
    hit: Handle<AudioSource>,
    flare: Handle<AudioSource>,
    growl: Handle<AudioSource>,
    lock: Handle<AudioSource>,
    warning: Handle<AudioSource>,
    stall: Handle<AudioSource>,
    hit_tick: Handle<AudioSource>,
    kill_chime: Handle<AudioSource>,
}

// ---------------------------------------------------------------- synthesis

struct Rng(u32);
impl Rng {
    fn next(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        (self.0 as f32 / u32::MAX as f32) * 2.0 - 1.0
    }
}

/// One-pole low-pass filter.
struct LowPass {
    a: f32,
    y: f32,
}
impl LowPass {
    fn new(cutoff: f32) -> Self {
        LowPass {
            a: 1.0 - (-TAU * cutoff / SAMPLE_RATE as f32).exp(),
            y: 0.0,
        }
    }
    fn run(&mut self, x: f32) -> f32 {
        self.y += self.a * (x - self.y);
        self.y
    }
}

/// Resonant band-pass (state-variable filter).
struct BandPass {
    f: f32,
    q: f32,
    low: f32,
    band: f32,
}
impl BandPass {
    fn new(centre: f32, q: f32) -> Self {
        let mut b = BandPass { f: 0.0, q: 1.0 / q, low: 0.0, band: 0.0 };
        b.set(centre);
        b
    }
    fn set(&mut self, centre: f32) {
        self.f = 2.0 * (std::f32::consts::PI * centre / SAMPLE_RATE as f32).sin();
    }
    fn run(&mut self, x: f32) -> f32 {
        self.low += self.f * self.band;
        let high = x - self.low - self.q * self.band;
        self.band += self.f * high;
        self.band
    }
}

fn samples(seconds: f32) -> usize {
    (seconds * SAMPLE_RATE as f32) as usize
}

fn t_of(i: usize) -> f32 {
    i as f32 / SAMPLE_RATE as f32
}

/// Normalise to a peak, then make the clip loop seamlessly by crossfading its
/// tail into its head.
fn finish(mut s: Vec<f32>, peak: f32, loop_fade: Option<f32>) -> Vec<f32> {
    if let Some(fade) = loop_fade {
        let n = samples(fade).min(s.len() / 2);
        let len = s.len() - n;
        for i in 0..n {
            let w = i as f32 / n as f32;
            s[i] = s[i] * w + s[len + i] * (1.0 - w);
        }
        s.truncate(len);
    }
    let max = s.iter().fold(0.0f32, |m, x| m.max(x.abs())).max(1e-6);
    s.iter_mut().for_each(|x| *x *= peak / max);
    s
}

fn wav(samples: &[f32]) -> AudioSource {
    let data_len = (samples.len() * 2) as u32;
    let mut b = Vec::with_capacity(44 + data_len as usize);
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data_len).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes()); // PCM
    b.extend_from_slice(&1u16.to_le_bytes()); // mono
    b.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    b.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes());
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        b.extend_from_slice(&((s.clamp(-1.0, 1.0) * 32_000.0) as i16).to_le_bytes());
    }
    AudioSource { bytes: Arc::from(b) }
}

/// Jet engine: broadband roar plus a turbine whine with a little wobble.
fn synth_engine() -> Vec<f32> {
    let mut rng = Rng(0x1234_5678);
    let (mut rumble, mut mid, mut hiss) = (LowPass::new(180.0), BandPass::new(900.0, 0.8), BandPass::new(3_500.0, 1.5));
    let s = (0..samples(3.2))
        .map(|i| {
            let t = t_of(i);
            let n = rng.next();
            let wobble = 1.0 + 0.004 * (t * TAU * 0.7).sin();
            let whine = (t * TAU * 2_350.0 * wobble).sin() * 0.05
                + (t * TAU * 4_700.0 * wobble).sin() * 0.025
                + (t * TAU * 1_175.0 * wobble).sin() * 0.02;
            rumble.run(n) * 2.6 + mid.run(n) * 0.35 + hiss.run(n) * 0.12 + whine
        })
        .collect();
    finish(s, 0.9, Some(0.4))
}

/// Afterburner: deep rumble with random crackle.
fn synth_afterburner() -> Vec<f32> {
    let mut rng = Rng(0xdead_beef);
    let (mut low, mut crack_f) = (LowPass::new(90.0), BandPass::new(1_400.0, 0.7));
    let mut crack_env = 0.0f32;
    let s = (0..samples(3.0))
        .map(|_| {
            let n = rng.next();
            if rng.next() > 0.9985 {
                crack_env = 1.0;
            }
            crack_env *= 0.996;
            low.run(n) * 4.0 + crack_f.run(n) * crack_env * 0.9
        })
        .collect();
    finish(s, 0.9, Some(0.4))
}

/// Wind: band-passed noise with slow gusting.
fn synth_wind() -> Vec<f32> {
    let mut rng = Rng(0x0bad_cafe);
    let mut bp = BandPass::new(700.0, 0.6);
    let mut low = LowPass::new(250.0);
    let s = (0..samples(4.0))
        .map(|i| {
            let t = t_of(i);
            bp.set(600.0 + 250.0 * (t * TAU * 0.31).sin() + 120.0 * (t * TAU * 0.83).sin());
            let n = rng.next();
            bp.run(n) * (0.8 + 0.2 * (t * TAU * 0.5).sin()) + low.run(n) * 0.8
        })
        .collect();
    finish(s, 0.8, Some(0.5))
}

/// 20 mm rotary cannon: 100 rounds/s, so a coarse buzzing "brrrt".
fn synth_gun() -> Vec<f32> {
    let mut rng = Rng(0x00c0_ffee);
    let mut lp = LowPass::new(2_200.0);
    let rate = 100.0;
    let s = (0..samples(0.5))
        .map(|i| {
            let t = t_of(i);
            let phase = (t * rate).fract() / rate; // seconds since last round
            let crack = (-phase * 900.0).exp() * rng.next();
            let thump = (-phase * 250.0).exp() * (phase * TAU * 140.0).sin();
            lp.run(crack * 0.9 + thump * 0.8)
        })
        .collect();
    finish(s, 0.85, None)
}

/// Missile motor: hissing roar.
fn synth_missile_motor() -> Vec<f32> {
    let mut rng = Rng(0x5eed_1234);
    let (mut bp, mut low) = (BandPass::new(1_800.0, 0.5), LowPass::new(300.0));
    let s = (0..samples(2.0)).map(|_| {
        let n = rng.next();
        bp.run(n) * 0.8 + low.run(n) * 1.5
    }).collect();
    finish(s, 0.8, Some(0.3))
}

/// Missile launch: thump, then a whoosh that falls away.
fn synth_launch() -> Vec<f32> {
    let mut rng = Rng(0x1aa7_c4ed);
    let (mut bp, mut low) = (BandPass::new(2_500.0, 0.7), LowPass::new(200.0));
    let s = (0..samples(2.2))
        .map(|i| {
            let t = t_of(i);
            bp.set(2_500.0 - 1_600.0 * (t / 2.2));
            let n = rng.next();
            let env = (t / 0.04).min(1.0) * (-t * 1.6).exp();
            let thump = (-t * 18.0).exp() * (t * TAU * 70.0).sin();
            bp.run(n) * env * 1.2 + low.run(n) * env * 2.0 + thump * 0.8
        })
        .collect();
    finish(s, 0.9, None)
}

/// Explosion: sharp crack, a rolling low boom, and a sub-bass thump.
fn synth_explosion(distant: bool) -> Vec<f32> {
    let mut rng = Rng(if distant { 0xfa12_0001 } else { 0xb00b_5000 });
    let (mut low, mut mid) = (LowPass::new(if distant { 160.0 } else { 400.0 }), LowPass::new(2_500.0));
    let length = if distant { 4.5 } else { 3.5 };
    let s = (0..samples(length))
        .map(|i| {
            let t = t_of(i);
            let n = rng.next();
            // rumble with a slightly uneven decay
            let env = (t / 0.01).min(1.0) * (-t * 1.3).exp() * (1.0 + 0.3 * (t * TAU * 3.0).sin());
            let crack = if distant { 0.0 } else { (-t * 40.0).exp() * mid.run(n) };
            let sub_f = 55.0 * (-t * 0.8).exp() + 22.0;
            let sub = (t * TAU * sub_f).sin() * (-t * 2.5).exp();
            low.run(n) * env * 3.0 + crack * 1.2 + sub * 0.9
        })
        .collect();
    finish(s, 0.95, None)
}

/// Bullet hit on an airframe: short metallic clank.
fn synth_hit() -> Vec<f32> {
    let mut rng = Rng(0x0417_0417);
    let mut bp = BandPass::new(3_000.0, 1.2);
    let s = (0..samples(0.3))
        .map(|i| {
            let t = t_of(i);
            let ring = ((t * TAU * 1_230.0).sin() * 0.6 + (t * TAU * 2_710.0).sin() * 0.4)
                * (-t * 22.0).exp();
            bp.run(rng.next()) * (-t * 60.0).exp() * 1.5 + ring * 0.5
        })
        .collect();
    finish(s, 0.8, None)
}

/// Flare: pop, then a short fizz.
fn synth_flare() -> Vec<f32> {
    let mut rng = Rng(0xf1a4_e000);
    let mut bp = BandPass::new(4_000.0, 0.6);
    let s = (0..samples(0.6))
        .map(|i| {
            let t = t_of(i);
            let pop = (-t * 80.0).exp() * (t * TAU * 180.0).sin();
            pop * 0.9 + bp.run(rng.next()) * (-t * 5.0).exp() * 0.6
        })
        .collect();
    finish(s, 0.7, None)
}

/// Seeker growl while acquiring: a buzzy tone, warbling.
fn synth_growl() -> Vec<f32> {
    let s = (0..samples(1.0))
        .map(|i| {
            let t = t_of(i);
            let f = 620.0 + 40.0 * (t * TAU * 6.0).sin();
            let tone = (t * TAU * f).sin().signum() * 0.4 + (t * TAU * f).sin() * 0.6;
            tone * (0.6 + 0.4 * (t * TAU * 12.0).sin().abs())
        })
        .collect();
    finish(s, 0.5, None)
}

/// Solid lock tone.
fn synth_lock() -> Vec<f32> {
    let s = (0..samples(1.0))
        .map(|i| {
            let t = t_of(i);
            (t * TAU * 1_100.0).sin() * 0.8 + (t * TAU * 2_200.0).sin() * 0.2
        })
        .collect();
    finish(s, 0.5, None)
}

/// Missile warning: fast beeping, 8 beeps per second.
fn synth_warning() -> Vec<f32> {
    let s = (0..samples(1.0))
        .map(|i| {
            let t = t_of(i);
            let on = (t * 8.0).fract() < 0.55;
            let edge = ((t * 8.0).fract() * 200.0).min(1.0);
            if on { (t * TAU * 1_650.0).sin() * edge } else { 0.0 }
        })
        .collect();
    finish(s, 0.55, None)
}

/// Stall horn: low two-tone.
fn synth_stall() -> Vec<f32> {
    let s = (0..samples(1.0))
        .map(|i| {
            let t = t_of(i);
            let f = if (t * 2.0).fract() < 0.5 { 420.0 } else { 350.0 };
            (t * TAU * f).sin() * 0.7 + (t * TAU * f * 2.0).sin() * 0.3
        })
        .collect();
    finish(s, 0.5, None)
}

/// Confirmation tick when your rounds land.
fn synth_hit_tick() -> Vec<f32> {
    let s = (0..samples(0.06))
        .map(|i| {
            let t = t_of(i);
            ((t * TAU * 2_600.0).sin() * 0.7 + (t * TAU * 5_200.0).sin() * 0.3) * (-t * 70.0).exp()
        })
        .collect();
    finish(s, 0.5, None)
}

/// Kill confirmed: two quick rising tones.
fn synth_kill_chime() -> Vec<f32> {
    let s = (0..samples(0.45))
        .map(|i| {
            let t = t_of(i);
            let (f, t0) = if t < 0.12 { (880.0, 0.0) } else { (1_320.0, 0.12) };
            let local = t - t0;
            let env = (local / 0.01).min(1.0) * (-local * 9.0).exp();
            ((t * TAU * f).sin() * 0.8 + (t * TAU * f * 2.0).sin() * 0.2) * env
        })
        .collect();
    finish(s, 0.5, None)
}

fn synthesise(mut commands: Commands, mut sources: ResMut<Assets<AudioSource>>) {
    let start = std::time::Instant::now();
    let mut add = |s: Vec<f32>| sources.add(wav(&s));
    commands.insert_resource(Sounds {
        engine: add(synth_engine()),
        afterburner: add(synth_afterburner()),
        wind: add(synth_wind()),
        gun: add(synth_gun()),
        missile_motor: add(synth_missile_motor()),
        launch: add(synth_launch()),
        explosion: add(synth_explosion(false)),
        explosion_far: add(synth_explosion(true)),
        hit: add(synth_hit()),
        flare: add(synth_flare()),
        growl: add(synth_growl()),
        lock: add(synth_lock()),
        warning: add(synth_warning()),
        stall: add(synth_stall()),
        hit_tick: add(synth_hit_tick()),
        kill_chime: add(synth_kill_chime()),
    });
    info!("synthesised sounds in {:.0?}", start.elapsed());
}

// ---------------------------------------------------------------- playback

#[derive(PartialEq, Eq)]
enum LockState {
    None,
    Acquiring,
    Locked,
}

fn lock_state(targeting: &Targeting) -> LockState {
    if targeting.locked() {
        LockState::Locked
    } else if targeting.lock > 0.05 {
        LockState::Acquiring
    } else {
        LockState::None
    }
}

/// Loops for the player's own aircraft and cockpit, faded in and out.
#[derive(Component, Clone, Copy, PartialEq, Eq)]
enum PlayerLoop {
    Engine,
    Afterburner,
    Wind,
    Gun,
    Growl,
    Lock,
    Warning,
    Stall,
}

/// Spatial loops attached to other aircraft and missiles.
#[derive(Component, Clone, Copy, PartialEq, Eq)]
enum RemoteLoop {
    Engine,
    Gun,
    MissileMotor,
}

fn looping(volume: f32, spatial: bool) -> PlaybackSettings {
    PlaybackSettings {
        mode: PlaybackMode::Loop,
        volume: Volume::Linear(volume),
        spatial,
        ..default()
    }
}

fn setup_player_loops(mut commands: Commands, sounds: Res<Sounds>) {
    for (kind, handle) in [
        (PlayerLoop::Engine, &sounds.engine),
        (PlayerLoop::Afterburner, &sounds.afterburner),
        (PlayerLoop::Wind, &sounds.wind),
        (PlayerLoop::Gun, &sounds.gun),
        (PlayerLoop::Growl, &sounds.growl),
        (PlayerLoop::Lock, &sounds.lock),
        (PlayerLoop::Warning, &sounds.warning),
        (PlayerLoop::Stall, &sounds.stall),
    ] {
        commands.spawn((kind, AudioPlayer(handle.clone()), looping(0.0, false)));
    }
}

fn attach_listener(
    mut commands: Commands,
    camera: Query<Entity, (With<MainCamera>, Without<SpatialListener>)>,
) {
    for camera in &camera {
        // scaled world units: see the spatial scales on each sound
        commands.entity(camera).insert(SpatialListener::new(0.4));
    }
}

/// `N` mutes everything. (Setting a sink's volume overrides the global
/// volume, so muting is applied by the systems that drive the sinks.)
#[derive(Resource, Default)]
struct Muted(bool);

fn toggle_mute(keys: Res<ButtonInput<KeyCode>>, mut muted: ResMut<Muted>) {
    if keys.just_pressed(KeyCode::KeyN) {
        muted.0 = !muted.0;
    }
}

/// Exponential approach so volume changes never click.
fn approach(current: f32, target: f32, dt: f32, rate: f32) -> f32 {
    current + (target - current) * (1.0 - (-rate * dt).exp())
}

#[allow(clippy::type_complexity)]
fn player_loops(
    time: Res<Time>,
    muted: Res<Muted>,
    player: Query<(Entity, &FlightState, &Aircraft, &Weapons, &Targeting, Has<Disabled>), With<Player>>,
    missiles: Query<&Missile>,
    mut loops: Query<(&PlayerLoop, &mut AudioSink)>,
) {
    let dt = time.delta_secs();
    let player = player.single().ok();
    let incoming = player.is_some_and(|(e, ..)| missiles.iter().any(|m| m.target == Some(e)));
    for (kind, mut sink) in &mut loops {
        let current = sink.volume().to_linear();
        let (target, speed) = match player {
            None => (0.0, 1.0),
            Some((_, state, aircraft, weapons, targeting, disabled)) => {
                let alive = if disabled { 0.0 } else { 1.0 };
                match kind {
                    PlayerLoop::Engine => (
                        (0.25 + 0.35 * state.spool) * alive,
                        0.75 + 0.45 * state.spool,
                    ),
                    PlayerLoop::Afterburner => (
                        if state.afterburner_on { 0.55 } else { 0.0 },
                        1.0,
                    ),
                    PlayerLoop::Wind => {
                        let v = (state.airspeed / 350.0).clamp(0.0, 1.3);
                        // buffeting near the stall and under G
                        let buffet = (state.g_load.abs() / 9.0) * 0.25;
                        (v * v * 0.35 + buffet, 0.7 + 0.5 * v)
                    }
                    PlayerLoop::Gun => (
                        if weapons.trigger_gun && weapons.gun_ammo > 0 && !disabled { 0.55 } else { 0.0 },
                        1.0,
                    ),
                    PlayerLoop::Growl => (
                        if lock_state(targeting) == LockState::Acquiring && weapons.missiles > 0 { 0.12 } else { 0.0 },
                        1.0,
                    ),
                    PlayerLoop::Lock => (
                        if lock_state(targeting) == LockState::Locked && weapons.missiles > 0 { 0.12 } else { 0.0 },
                        1.0,
                    ),
                    PlayerLoop::Warning => (if incoming { 0.2 } else { 0.0 }, 1.0),
                    PlayerLoop::Stall => (
                        if state.stalling(aircraft) && !disabled { 0.15 } else { 0.0 },
                        1.0,
                    ),
                }
            }
        };
        // tones snap on/off; engine sounds glide
        let rate = match kind {
            PlayerLoop::Gun | PlayerLoop::Lock | PlayerLoop::Growl | PlayerLoop::Warning => 40.0,
            _ => 4.0,
        };
        let target = if muted.0 { 0.0 } else { target };
        sink.set_volume(Volume::Linear(approach(current, target, dt, rate)));
        let current_speed = sink.speed();
        sink.set_speed(approach(current_speed, speed, dt, 3.0));
    }
}

fn attach_aircraft_loops(
    mut commands: Commands,
    sounds: Res<Sounds>,
    new: Query<Entity, (Added<Aircraft>, Without<Player>)>,
) {
    for aircraft in &new {
        commands.entity(aircraft).with_children(|p| {
            p.spawn((
                RemoteLoop::Engine,
                AudioPlayer(sounds.engine.clone()),
                PlaybackSettings {
                    spatial_scale: Some(SpatialScale::new(1.0 / 120.0)),
                    ..looping(0.0, true)
                },
                Transform::from_xyz(0.0, 0.0, 6.0),
            ));
            p.spawn((
                RemoteLoop::Gun,
                AudioPlayer(sounds.gun.clone()),
                PlaybackSettings {
                    spatial_scale: Some(SpatialScale::new(1.0 / 200.0)),
                    ..looping(0.0, true)
                },
                Transform::from_xyz(0.0, 0.0, -7.0),
            ));
        });
    }
}

fn attach_missile_loops(mut commands: Commands, sounds: Res<Sounds>, new: Query<Entity, Added<Missile>>) {
    for missile in &new {
        commands.entity(missile).with_child((
            RemoteLoop::MissileMotor,
            AudioPlayer(sounds.missile_motor.clone()),
            PlaybackSettings {
                spatial_scale: Some(SpatialScale::new(1.0 / 90.0)),
                ..looping(0.0, true)
            },
        ));
    }
}

/// Doppler factor for a source heard by a listener.
fn doppler(listener_pos: Vec3, listener_vel: Vec3, source_pos: Vec3, source_vel: Vec3) -> f32 {
    let dir = (source_pos - listener_pos).normalize_or_zero();
    let toward_source = listener_vel.dot(dir);
    let away_from_listener = source_vel.dot(dir);
    ((SPEED_OF_SOUND + toward_source) / (SPEED_OF_SOUND + away_from_listener).max(40.0)).clamp(0.4, 2.5)
}

#[allow(clippy::type_complexity)]
fn remote_loops(
    time: Res<Time>,
    muted: Res<Muted>,
    camera: Query<&GlobalTransform, With<MainCamera>>,
    player: Query<&FlightState, With<Player>>,
    aircraft: Query<(&GlobalTransform, &FlightState, &Weapons, Has<Disabled>)>,
    missiles: Query<(&GlobalTransform, &Missile)>,
    mut loops: Query<(&ChildOf, &RemoteLoop, &mut SpatialAudioSink)>,
) {
    let dt = time.delta_secs();
    let Ok(cam) = camera.single() else {
        return;
    };
    let listener_pos = cam.translation();
    let listener_vel = player.single().map_or(Vec3::ZERO, |s| s.velocity);
    for (parent, kind, mut sink) in &mut loops {
        let (target, speed) = match kind {
            RemoteLoop::Engine | RemoteLoop::Gun => {
                let Ok((gt, state, weapons, disabled)) = aircraft.get(parent.parent()) else {
                    continue;
                };
                let d = doppler(listener_pos, listener_vel, gt.translation(), state.velocity);
                match kind {
                    RemoteLoop::Engine => (
                        if disabled { 0.0 } else { 0.5 + 0.5 * state.spool + if state.afterburner_on { 0.5 } else { 0.0 } },
                        d * (0.75 + 0.45 * state.spool),
                    ),
                    _ => (
                        if weapons.trigger_gun && weapons.gun_ammo > 0 && !disabled { 0.9 } else { 0.0 },
                        d,
                    ),
                }
            }
            RemoteLoop::MissileMotor => {
                let Ok((gt, missile)) = missiles.get(parent.parent()) else {
                    continue;
                };
                (
                    if missile.motor_burning() { 1.0 } else { 0.0 },
                    doppler(listener_pos, listener_vel, gt.translation(), missile.velocity),
                )
            }
        };
        let target = if muted.0 { 0.0 } else { target };
        let current = sink.volume().to_linear();
        let rate = if *kind == RemoteLoop::Gun { 40.0 } else { 5.0 };
        sink.set_volume(Volume::Linear(approach(current, target, dt, rate)));
        let s = sink.speed();
        sink.set_speed(approach(s, speed, dt, 6.0));
    }
}

/// One-shot sounds waiting for their sound to "arrive" (speed of sound).
#[derive(Resource, Default)]
struct DelayedSounds(Vec<(f32, Handle<AudioSource>, Vec3, f32, f32)>);

#[allow(clippy::too_many_arguments)]
fn one_shots(
    sounds: Res<Sounds>,
    mut delayed: ResMut<DelayedSounds>,
    camera: Query<&GlobalTransform, With<MainCamera>>,
    mut explosions: MessageReader<Explosion>,
    new_missiles: Query<&GlobalTransform, Added<Missile>>,
    new_flares: Query<&GlobalTransform, Added<Flare>>,
    mut flare_cooldown: Local<f32>,
    time: Res<Time>,
) {
    let Ok(cam) = camera.single() else {
        explosions.clear();
        return;
    };
    let listener = cam.translation();
    let mut queue = |handle: &Handle<AudioSource>, pos: Vec3, volume: f32, scale: f32| {
        let delay = listener.distance(pos) / SPEED_OF_SOUND;
        delayed.0.push((delay, handle.clone(), pos, volume, scale));
    };

    for e in explosions.read() {
        let d = listener.distance(e.position);
        if e.size < 2.0 {
            // bullet impacts: only audible close by
            if d < 600.0 {
                queue(&sounds.hit, e.position, 0.6, 1.0 / 40.0);
            }
            continue;
        }
        // big explosions carry further; far ones lose their high end
        let reach = 60.0 * e.size.sqrt();
        let handle = if d > 2_500.0 { &sounds.explosion_far } else { &sounds.explosion };
        queue(handle, e.position, (e.size / 12.0).clamp(0.3, 1.0), 1.0 / reach);
    }
    for gt in &new_missiles {
        queue(&sounds.launch, gt.translation(), 0.9, 1.0 / 60.0);
    }
    *flare_cooldown -= time.delta_secs();
    for gt in &new_flares {
        // one pop per salvo
        if *flare_cooldown <= 0.0 {
            *flare_cooldown = 0.2;
            queue(&sounds.flare, gt.translation(), 0.6, 1.0 / 50.0);
        }
    }
}

fn play_delayed(
    mut commands: Commands,
    time: Res<Time>,
    muted: Res<Muted>,
    mut delayed: ResMut<DelayedSounds>,
) {
    let dt = time.delta_secs();
    delayed.0.retain_mut(|(t, handle, pos, volume, scale)| {
        *t -= dt;
        if *t > 0.0 {
            return true;
        }
        if muted.0 {
            return false;
        }
        commands.spawn((
            AudioPlayer(handle.clone()),
            PlaybackSettings {
                mode: PlaybackMode::Despawn,
                volume: Volume::Linear(*volume),
                spatial: true,
                spatial_scale: Some(SpatialScale::new(*scale)),
                ..default()
            },
            Transform::from_translation(*pos),
        ));
        false
    });
}

/// Hit ticks and the kill chime (cockpit sounds, not spatial).
fn feedback_sounds(
    mut commands: Commands,
    sounds: Res<Sounds>,
    muted: Res<Muted>,
    mut hits: MessageReader<crate::weapons::HitConfirmed>,
    mut kills: MessageReader<crate::game::PlayerKill>,
    player: Query<Entity, With<Player>>,
    time: Res<Time>,
    mut tick_cooldown: Local<f32>,
) {
    *tick_cooldown -= time.delta_secs();
    let me = player.single().ok();
    let hit = hits.read().any(|h| Some(h.attacker) == me);
    let killed = kills.read().count() > 0;
    if muted.0 {
        return;
    }
    let mut play = |handle: &Handle<AudioSource>, volume: f32| {
        commands.spawn((
            AudioPlayer(handle.clone()),
            PlaybackSettings {
                mode: PlaybackMode::Despawn,
                volume: Volume::Linear(volume),
                ..default()
            },
        ));
    };
    // rate-limit ticks so a long burst doesn't machine-gun them
    if hit && *tick_cooldown <= 0.0 {
        *tick_cooldown = 0.07;
        play(&sounds.hit_tick, 0.35);
    }
    if killed {
        play(&sounds.kill_chime, 0.45);
    }
}
