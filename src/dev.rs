//! Dev helpers for headless-ish verification.
//!
//! `FLIGHT_SIM_SCREENSHOT=out.png cargo run` saves a screenshot after a few
//! seconds and exits. `FLIGHT_SIM_SCREENSHOT_AFTER=<secs>` changes the delay.
//! `FLIGHT_SIM_SCREENSHOT_BURST=<count>,<secs>` saves `count` frames `secs`
//! apart instead (`out_00.png`, `out_01.png`, ...), for catching flicker.
//! `FLIGHT_SIM_DIE_AT=<secs>` shoots the player down that many (real) seconds in.
//! `FLIGHT_SIM_SCREENSHOT_EXTRA=<event>:<path>[,...]` takes more shots on the
//! way (without exiting), each once, about a second after its event:
//! `t<secs>`, `lobby<n>` (in a lobby with n players), `room<n>` (flying in
//! a room with n players, 7 s in), `friendjoin` (a friend in a public room,
//! their row showing JOIN), `request` (a join request card), `kill` (our kill banner), `death` (the multiplayer death
//! screen, killer named), `rank` (the challenge score screen with our rank),
//! `board` (the leaderboard screen with its rows).

use bevy::{
    prelude::*,
    render::view::screenshot::{Screenshot, save_to_disk},
};

pub struct DevPlugin;

/// Screenshot runs render headless (no window), into this image.
pub const HEADLESS_SIZE: UVec2 = UVec2::new(2560, 1440);

pub fn headless() -> bool {
    std::env::var("FLIGHT_SIM_SCREENSHOT").is_ok()
}

/// The offscreen image the camera renders to in headless runs.
#[derive(Resource, Clone)]
pub struct HeadlessTarget(pub Handle<Image>);

#[derive(Resource)]
struct AutoScreenshot {
    path: String,
    after: f32,
    taken_at: Option<f32>,
    /// (shots left after the current one, seconds between shots)
    burst: Option<(u32, f32)>,
    index: u32,
}

impl AutoScreenshot {
    fn current_path(&self) -> String {
        if self.burst.is_none() && self.index == 0 {
            return self.path.clone();
        }
        let (stem, ext) = self.path.rsplit_once('.').unwrap_or((&self.path, "png"));
        format!("{stem}_{:02}.{ext}", self.index)
    }
}

impl Plugin for DevPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, log_state);
        if std::env::var("FLIGHT_SIM_FX_TEST").is_ok() {
            app.add_systems(
                FixedUpdate,
                force_afterburner
                    .after(crate::flight::FlightSystems::Pilots)
                    .before(crate::flight::FlightSystems::Physics),
            )
            .add_systems(Update, fx_test_explosion);
        }
        if std::env::var("FLIGHT_SIM_CAM").is_ok() {
            app.add_systems(
                PostUpdate,
                orbit_camera
                    .after(TransformSystems::Propagate)
                    .before(crate::effects::EffectsSystems),
            );
        }
        if std::env::var("FLIGHT_SIM_KILL_TEST").is_ok() {
            app.add_systems(Update, kill_test);
        }
        if let Some(at) = std::env::var("FLIGHT_SIM_DIE_AT").ok().and_then(|s| s.parse::<f32>().ok()) {
            app.add_systems(Update, move |time: Res<Time<Real>>, mut player: Query<&mut crate::game::Health, With<crate::game::Player>>| {
                if time.elapsed_secs() >= at {
                    for mut health in &mut player {
                        health.current = 0.0;
                    }
                }
            });
        }
        if std::env::var("FLIGHT_SIM_NO_HUD").is_ok() {
            app.add_systems(Update, hide_player_model);
        }
        if std::env::var("FLIGHT_SIM_AUTOPILOT").is_ok() {
            app.add_systems(Update, autopilot_player);
        }
        if let Ok(list) = std::env::var("FLIGHT_SIM_SCREENSHOT_EXTRA") {
            let shots = list
                .split(',')
                .filter_map(|e| e.split_once(':'))
                .map(|(event, path)| ExtraShot { event: event.to_string(), path: path.to_string(), since: None, done: false })
                .collect();
            app.insert_resource(ExtraShots(shots)).add_systems(Update, extra_screenshots);
        }
        if let Ok(path) = std::env::var("FLIGHT_SIM_SCREENSHOT") {
            let after = std::env::var("FLIGHT_SIM_SCREENSHOT_AFTER")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(5.0);
            app.insert_resource(AutoScreenshot {
                path,
                after,
                taken_at: None,
                burst: std::env::var("FLIGHT_SIM_SCREENSHOT_BURST").ok().and_then(|s| {
                    let (n, dt) = s.split_once(',')?;
                    Some((n.trim().parse::<u32>().ok()?.saturating_sub(1), dt.trim().parse().ok()?))
                }),
                index: 0,
            })
            .add_systems(Update, auto_screenshot);
        }
    }
}

fn auto_screenshot(
    mut commands: Commands,
    target: Option<Res<HeadlessTarget>>,
    // real time: game time stops while paused
    time: Res<Time<Real>>,
    mut state: ResMut<AutoScreenshot>,
    mut exit: MessageWriter<AppExit>,
) {
    let now = time.elapsed_secs();
    match state.taken_at {
        None if now >= state.after => {
            let screenshot = match &target {
                Some(t) => Screenshot::image(t.0.clone()),
                None => Screenshot::primary_window(),
            };
            let path = state.current_path();
            // (a stale file would end the wait below early)
            let _ = std::fs::remove_file(&path);
            commands.spawn(screenshot).observe(save_to_disk(path));
            state.taken_at = Some(now);
        }
        Some(t) => {
            if let Some((left, dt)) = state.burst
                && left > 0
            {
                if now - t >= dt {
                    state.burst = Some((left - 1, dt));
                    state.index += 1;
                    state.taken_at = None;
                    state.after = now;
                }
                return;
            }
            // wait for the capture to be written before quitting (it can take a
            // while when the machine is busy), giving up after 20 s
            if (now - t > 1.0 && std::path::Path::new(&state.current_path()).exists()) || now - t > 20.0 {
                exit.write(AppExit::Success);
            }
        }
        _ => {}
    }
}

struct ExtraShot {
    event: String,
    path: String,
    /// When its event was first seen (real seconds).
    since: Option<f32>,
    done: bool,
}

#[derive(Resource)]
struct ExtraShots(Vec<ExtraShot>);

fn extra_screenshots(
    mut commands: Commands,
    target: Option<Res<HeadlessTarget>>,
    time: Res<Time<Real>>,
    mut shots: ResMut<ExtraShots>,
    net: Res<crate::net::Net>,
    prompt: Res<crate::respawn::RespawnPrompt>,
    mut kills: MessageReader<crate::game::PlayerKill>,
    mut last_kill: Local<Option<f32>>,
    board: Res<crate::leaderboard::BoardUi>,
    state: Res<State<crate::loading::GameState>>,
) {
    let now = time.elapsed_secs();
    if kills.read().count() > 0 {
        *last_kill = Some(now);
    }
    for shot in shots.0.iter_mut().filter(|s| !s.done) {
        let e = shot.event.as_str();
        let happening = if let Some(t) = e.strip_prefix('t') {
            t.parse::<f32>().is_ok_and(|t| now >= t)
        } else if let Some(n) = e.strip_prefix("lobby") {
            net.in_lobby() && net.players().len() >= n.parse().unwrap_or(1)
        } else if let Some(n) = e.strip_prefix("room") {
            // flying in a room with at least n players
            *state.get() == crate::loading::GameState::Playing
                && net.in_room()
                && !net.in_lobby()
                && net.players().len() >= n.parse().unwrap_or(1)
        } else {
            match e {
                "request" => net.pending_request().is_some(),
                // a friend in a public room (their row shows JOIN)
                "friendjoin" => net.friends().iter().any(|f| f.status == crate::net::protocol::FriendStatus::InPublic && !net.in_my_room(&f.did)),
                "kill" => last_kill.is_some_and(|t| now - t < 2.0),
                "death" => prompt.ready() && net.death().is_some(),
                "rank" => prompt.ready() && net.standing().is_some(),
                "board" => board.open && net.board(board.kind).is_some(),
                _ => false,
            }
        };
        if !happening {
            shot.since = None;
            continue;
        }
        let since = *shot.since.get_or_insert(now);
        // (time events go at once; the others wait for the screen to settle)
        let wait = if e.starts_with('t') {
            0.0
        } else if e == "kill" {
            0.6
        } else if e.starts_with("room") {
            // (past the intro)
            7.0
        } else {
            1.0
        };
        if now - since >= wait {
            shot.done = true;
            info!("dev: screenshot {} ({e})", shot.path);
            let screenshot = match &target {
                Some(t) => Screenshot::image(t.0.clone()),
                None => Screenshot::primary_window(),
            };
            commands.spawn(screenshot).observe(save_to_disk(shot.path.clone()));
        }
    }
}

/// `FLIGHT_SIM_LOG=1`: print player and camera state once a second.
pub fn log_state(
    time: Res<Time>,
    mut last: Local<f32>,
    player: Query<(&Transform, &crate::flight::FlightState, &crate::flight::ControlInput), With<crate::game::Player>>,
    camera: Query<&Transform, With<crate::camera::MainCamera>>,
    aircraft: Query<(), With<crate::flight::Aircraft>>,
    diagnostics: Res<bevy::diagnostic::DiagnosticsStore>,
) {
    if std::env::var("FLIGHT_SIM_LOG").is_err() || time.elapsed_secs() - *last < 1.0 {
        return;
    }
    *last = time.elapsed_secs();
    let fps = diagnostics
        .get(&bevy::diagnostic::FrameTimeDiagnosticsPlugin::FPS)
        .and_then(|d| d.smoothed())
        .unwrap_or(0.0);
    info!("fps {fps:.0}");
    let cam = camera.single().map(|t| t.translation).ok();
    match player.single() {
        Ok((t, s, i)) => info!(
            "t={:.0} pos={:.0} fwd={:.2} spd={:.0} aoa={:.1} g={:.1} in=({:.2},{:.2},{:.2},thr {:.2}) cam={:?} aircraft={}",
            time.elapsed_secs(), t.translation, t.forward().as_vec3(), s.airspeed, s.alpha.to_degrees(), s.g_load,
            i.pitch, i.roll, i.yaw, i.throttle, cam, aircraft.iter().count()
        ),
        Err(_) => info!("t={:.0} no player, cam={:?} aircraft={}", time.elapsed_secs(), cam, aircraft.iter().count()),
    }
}

/// `FLIGHT_SIM_NO_HUD=1`: hide the player's own jet (the HUD itself is
/// skipped in `hud.rs`), for clean landscape screenshots.
fn hide_player_model(mut player: Query<&mut Visibility, With<crate::game::Player>>) {
    for mut vis in &mut player {
        *vis = Visibility::Hidden;
    }
}

/// `FLIGHT_SIM_AUTOPILOT=1`: the AI flies (and fights) the player's jet, so
/// automated screenshot runs show real combat.
fn autopilot_player(
    mut commands: Commands,
    new: Query<Entity, (Added<crate::game::Player>, Without<crate::ai::Pilot>)>,
) {
    for e in &new {
        commands.entity(e).insert(crate::ai::Pilot::new());
    }
}

/// `FLIGHT_SIM_FX_TEST=<mode>` stages one effect shortly before the screenshot
/// (`FLIGHT_SIM_FX_LEAD` seconds before it, default 0.35):
/// - `1` / `explosion`: full afterburner plus an explosion ahead of the player
///   (`FLIGHT_SIM_FX_SIZE`, default 12)
/// - `ground`: a crash-sized explosion on the ground ahead
/// - `engines` / `dry`: afterburner / full dry power only
/// - `missile`: fires a missile
/// - `vapour`: a hard pull
/// - `damage`: the player is badly damaged from the start
/// - `burning`: the player is shot down
/// - `gun`: holds the trigger
/// - `stress`: big explosions every 0.8 s ahead of the jet
/// - `contrail`: the player starts 5 km higher, where contrails form
fn fx_mode() -> String {
    std::env::var("FLIGHT_SIM_FX_TEST").unwrap_or_default()
}

fn fx_lead() -> f32 {
    std::env::var("FLIGHT_SIM_FX_LEAD").ok().and_then(|s| s.parse().ok()).unwrap_or(0.35)
}

fn force_afterburner(
    time: Res<Time>,
    shot: Option<Res<AutoScreenshot>>,
    mut player: Query<&mut crate::flight::ControlInput, With<crate::game::Player>>,
    mut actions: ResMut<crate::controls::PlayerActions>,
    mut fired: Local<bool>,
) {
    let mode = fx_mode();
    let staged = shot.is_some_and(|s| time.elapsed_secs() >= s.after - fx_lead());
    if mode == "gun" {
        actions.firing_gun = true;
    }
    if mode == "missile" && staged && !*fired {
        *fired = true;
        actions.fire_missile = true;
    }
    for mut input in &mut player {
        input.throttle = 1.0;
        input.afterburner = !matches!(mode.as_str(), "dry" | "vapour" | "missile" | "gun");
        input.pitch = 0.0;
        input.roll = 0.0;
        if mode == "vapour" && staged {
            input.pitch = 1.0;
        }
    }
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn fx_test_explosion(
    time: Res<Time<Real>>,
    shot: Option<Res<AutoScreenshot>>,
    terrain: Res<crate::world::Terrain>,
    mut player: Query<(&mut Transform, &crate::flight::FlightState, &mut crate::game::Health), With<crate::game::Player>>,
    mut explosions: MessageWriter<crate::game::Explosion>,
    mut done: Local<bool>,
    mut stress_count: Local<u32>,
    mut lifted: Local<bool>,
) {
    let Some(shot) = shot else { return };
    let mode = fx_mode();
    if mode == "contrail"
        && !*lifted
        && let Ok((mut t, _, _)) = player.single_mut()
    {
        t.translation.y += 5_000.0;
        *lifted = true;
    }
    if mode == "damage" {
        for (_, _, mut health) in &mut player {
            health.current = health.current.min(health.max * 0.2);
        }
    }
    if *done || time.elapsed_secs() < shot.after - fx_lead() {
        return;
    }
    *done = true;
    // `stress`: a big air burst every 0.8 s from 3 s on, for frame-time checks
    if mode == "stress" {
        *done = false;
        if time.elapsed_secs() < 3.0 + *stress_count as f32 * 0.8 {
            return;
        }
        *stress_count += 1;
        if let Ok((t, _, _)) = player.single() {
            let side = if stress_count.is_multiple_of(2) { 1.0 } else { -1.0 };
            let p = t.translation + t.forward() * 250.0 + t.right() * side * 60.0;
            let size = [12.0, 9.0, 25.0, 6.0][*stress_count as usize % 4];
            explosions.write(crate::game::Explosion { position: p, size });
        }
        return;
    }
    let Ok((t, state, mut health)) = player.single_mut() else { return };
    // where the jet will be when the screenshot is taken
    let ahead = t.translation + state.velocity * fx_lead();
    match mode.as_str() {
        "1" | "explosion" => {
            let size = std::env::var("FLIGHT_SIM_FX_SIZE").ok().and_then(|s| s.parse().ok()).unwrap_or(12.0);
            let p = ahead + t.forward() * 130.0 + t.right() * 30.0;
            explosions.write(crate::game::Explosion { position: p, size });
        }
        "ground" => {
            // on the line of sight of FLIGHT_SIM_CAM=0,60,40 (looking steeply down past the jet)
            let mut p = ahead + t.forward() * (t.translation.y / 60f32.to_radians().tan()) + t.right() * 350.0;
            p.y = terrain.height_at(p.x, p.z);
            explosions.write(crate::game::Explosion { position: p, size: 25.0 });
        }
        "burning" => health.current = 0.0,
        _ => {}
    }
}

/// `FLIGHT_SIM_CAM=<yaw°>,<pitch°>,<distance m>`: park the camera at a fixed
/// spot relative to the player's jet (yaw 0 = straight behind), for close-ups.
/// A fourth value `<x>,<y>,<z>` (three more numbers) moves the focus point
/// (aircraft-local, default 0,0,6). `FLIGHT_SIM_CAM_MISSILE=1` orbits the
/// newest missile instead (yaw 0 = behind it, along its flight path).
#[allow(clippy::type_complexity)]
fn orbit_camera(
    player: Query<&Transform, (With<crate::game::Player>, Without<crate::camera::MainCamera>)>,
    missiles: Query<(&GlobalTransform, &crate::weapons::Missile), Without<crate::camera::MainCamera>>,
    mut camera: Query<(&mut Transform, &mut GlobalTransform), With<crate::camera::MainCamera>>,
) {
    let v: Vec<f32> = std::env::var("FLIGHT_SIM_CAM")
        .unwrap_or_default()
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    let (yaw, pitch, dist, offset) = match v[..] {
        [yaw, pitch, dist] => (yaw, pitch, dist, Vec3::new(0.0, 0.0, 6.0)),
        [yaw, pitch, dist, x, y, z] => (yaw, pitch, dist, Vec3::new(x, y, z)),
        _ => return,
    };
    let Ok((mut cam, mut global)) = camera.single_mut() else { return };
    let missile = std::env::var("FLIGHT_SIM_CAM_MISSILE").is_ok();
    let frame = if missile {
        // the youngest missile, framed like a jet: -Z along its flight path
        let Some((gt, m)) = missiles.iter().min_by(|a, b| a.1.age.total_cmp(&b.1.age)) else {
            return;
        };
        Transform::from_translation(gt.translation()).looking_to(m.velocity, Vec3::Y)
    } else {
        let Ok(plane) = player.single() else { return };
        *plane
    };
    let dir = Quat::from_rotation_y(yaw.to_radians()) * Quat::from_rotation_x(-pitch.to_radians()) * Vec3::Z;
    let focus = frame.transform_point(if missile { Vec3::ZERO } else { offset });
    cam.translation = focus + frame.rotation * dir * dist;
    cam.look_at(focus, frame.up());
    // runs after propagation (so it wins over the chase camera): the camera
    // has no parent, so its global transform is just its transform
    *global = GlobalTransform::from(*cam);
}

/// `FLIGHT_SIM_KILL_TEST=1`: fake a hit and a kill just before the screenshot.
fn kill_test(
    time: Res<Time<Real>>,
    shot: Option<Res<AutoScreenshot>>,
    player: Query<Entity, With<crate::game::Player>>,
    mut kills: MessageWriter<crate::game::PlayerKill>,
    mut hits: MessageWriter<crate::weapons::HitConfirmed>,
    mut done: Local<bool>,
) {
    let (Some(shot), Ok(me)) = (shot, player.single()) else { return };
    let t = time.elapsed_secs();
    if t > shot.after - 0.4 && !*done {
        *done = true;
        kills.write(crate::game::PlayerKill { total: 3, friendly: false, name: None });
    }
    if t > shot.after - 0.1 {
        hits.write(crate::weapons::HitConfirmed { attacker: me });
    }
}
