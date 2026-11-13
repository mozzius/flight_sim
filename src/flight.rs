//! Sim-lite flight model.
//!
//! Translational motion is real rigid-body physics: lift and drag from angle
//! of attack (with a proper stall), sideslip force, thrust that falls off with
//! altitude, and gravity. Rotation goes through a fly-by-wire layer: the stick
//! commands a rotation *rate*, limited by a G limiter and an AoA limiter, and
//! control authority scales with dynamic pressure. So a slow plane is
//! sluggish, and a stalled plane weathervanes nose-down into the airflow.
//!
//! Conventions (Bevy): forward is -Z, up is +Y, right is +X. Body angular
//! velocity: +x = nose up, +y = nose left, +z = roll left.

use bevy::prelude::*;

use crate::world::Terrain;

pub const PHYSICS_HZ: f64 = 60.0;
pub const GRAVITY: f32 = 9.81;
const SEA_LEVEL_DENSITY: f32 = 1.225;
const SPEED_OF_SOUND: f32 = 340.0;

pub struct FlightPlugin;

impl Plugin for FlightPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(Time::<Fixed>::from_hz(PHYSICS_HZ))
            .add_message::<AircraftCrashed>()
            .add_systems(
                FixedUpdate,
                (flight_physics, ground_collision)
                    .chain()
                    .in_set(FlightSystems::Physics),
            );
    }
}

/// Ordering for everything that runs in the fixed timestep.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub enum FlightSystems {
    /// Player input and AI decide `ControlInput`.
    Pilots,
    /// Integrate aircraft.
    Physics,
    /// Weapons, projectiles, damage.
    Combat,
}

/// Fired when an aircraft hits the ground or a building.
#[derive(Message)]
pub struct AircraftCrashed {
    pub entity: Entity,
    pub position: Vec3,
}

/// Static performance numbers for an airframe.
#[derive(Component, Clone)]
pub struct Aircraft {
    pub mass: f32,
    pub wing_area: f32,
    /// Lift slope per radian of AoA.
    pub cl_alpha: f32,
    /// AoA where the wing stalls (radians).
    pub stall_alpha: f32,
    pub cd0: f32,
    /// Induced drag factor (CD += k * CL^2).
    pub induced_k: f32,
    pub dry_thrust: f32,
    pub afterburner_thrust: f32,
    /// Max commanded rates at full authority (rad/s): pitch, yaw, roll.
    pub max_rates: Vec3,
    /// Fly-by-wire G limit.
    pub g_limit: f32,
    /// Fly-by-wire AoA limit (radians).
    pub alpha_limit: f32,
    /// Collision / hit radius (metres).
    pub radius: f32,
}

impl Aircraft {
    /// Roughly F-22 shaped, tuned for fun rather than accuracy.
    pub fn fighter() -> Self {
        Aircraft {
            mass: 20_000.0,
            wing_area: 78.0,
            cl_alpha: 4.5,
            stall_alpha: 20f32.to_radians(),
            cd0: 0.021,
            induced_k: 0.12,
            dry_thrust: 150_000.0,
            afterburner_thrust: 290_000.0,
            max_rates: Vec3::new(1.5, 0.5, 4.7),
            g_limit: 9.0,
            alpha_limit: 24f32.to_radians(),
            radius: 9.0,
        }
    }

    /// Lift coefficient as a function of AoA, including the post-stall drop.
    pub fn lift_coefficient(&self, alpha: f32) -> f32 {
        let a = alpha.abs();
        let cl = if a <= self.stall_alpha {
            self.cl_alpha * a
        } else {
            // fall from CLmax toward a flat-plate curve past the stall
            let cl_max = self.cl_alpha * self.stall_alpha;
            let flat_plate = 1.1 * (2.0 * a).sin();
            let t = ((a - self.stall_alpha) / 10f32.to_radians()).clamp(0.0, 1.0);
            cl_max * (1.0 - t) + flat_plate * 0.75 * t
        };
        cl * alpha.signum()
    }
}

/// Pilot inputs. Axes are -1..1, throttle is 0..1.
#[derive(Component, Default, Clone, Debug)]
pub struct ControlInput {
    /// +1 = pull (nose up).
    pub pitch: f32,
    /// +1 = roll right.
    pub roll: f32,
    /// +1 = yaw right.
    pub yaw: f32,
    pub throttle: f32,
    pub afterburner: bool,
    pub airbrake: bool,
}

/// Dynamic state, plus derived values the HUD and AI like to read.
#[derive(Component, Clone, Debug)]
pub struct FlightState {
    pub velocity: Vec3,
    /// Body-frame angular velocity.
    pub angular_velocity: Vec3,
    /// Engine output 0..1 (dry), lags the throttle.
    pub spool: f32,
    pub afterburner_on: bool,
    pub alpha: f32,
    pub beta: f32,
    /// Load factor felt by the pilot (G).
    pub g_load: f32,
    pub airspeed: f32,
    pub mach: f32,
}

impl FlightState {
    pub fn new(velocity: Vec3) -> Self {
        FlightState {
            velocity,
            angular_velocity: Vec3::ZERO,
            spool: 0.7,
            afterburner_on: false,
            alpha: 0.0,
            beta: 0.0,
            g_load: 1.0,
            airspeed: velocity.length(),
            mach: velocity.length() / SPEED_OF_SOUND,
        }
    }

    pub fn stalling(&self, aircraft: &Aircraft) -> bool {
        self.alpha.abs() > aircraft.stall_alpha * 0.92 || self.airspeed < 70.0
    }
}

/// Aircraft with this are no longer controlled (shot down): they fall.
#[derive(Component)]
pub struct Disabled;

pub fn air_density(altitude: f32) -> f32 {
    SEA_LEVEL_DENSITY * (-altitude.max(0.0) / 8_500.0).exp()
}

fn flight_physics(
    time: Res<Time>,
    mut query: Query<(
        &Aircraft,
        &ControlInput,
        &mut FlightState,
        &mut Transform,
        Has<Disabled>,
    )>,
) {
    let dt = time.delta_secs();
    for (ac, input, mut state, mut transform, disabled) in &mut query {
        let rot = transform.rotation;
        let forward = rot * Vec3::NEG_Z;
        let right = rot * Vec3::X;

        let rho = air_density(transform.translation.y);
        let speed = state.velocity.length();
        let q = 0.5 * rho * speed * speed;
        let v_local = rot.inverse() * state.velocity;

        // angles of attack and sideslip
        let (alpha, beta) = if speed > 1.0 {
            (
                (-v_local.y).atan2(-v_local.z),
                v_local.x.atan2(-v_local.z),
            )
        } else {
            (0.0, 0.0)
        };

        // ---- forces ----
        let vel_dir = state.velocity.normalize_or(forward);
        let mut force = Vec3::NEG_Y * GRAVITY * ac.mass;

        let cl = ac.lift_coefficient(alpha);
        let lift_dir = right.cross(vel_dir).normalize_or_zero();
        force += lift_dir * cl * q * ac.wing_area;

        // drag: parasitic (+ transonic rise), induced, high-AoA, airbrake
        let mach = speed / SPEED_OF_SOUND;
        let wave = 0.025 * ((mach - 0.85) / 0.25).clamp(0.0, 1.0);
        let mut cd = ac.cd0 + wave + ac.induced_k * cl * cl + 0.5 * alpha.sin().powi(2);
        if input.airbrake && !disabled {
            cd += 0.08;
        }
        force -= vel_dir * cd * q * ac.wing_area;

        // sideslip pushes back against the slip
        let side = (right - vel_dir * right.dot(vel_dir)).normalize_or_zero();
        force -= side * 1.2 * beta * q * ac.wing_area * 0.3;

        // engine: spools toward throttle, afterburner kicks in at full throttle
        let target = if disabled { 0.0 } else { input.throttle.clamp(0.0, 1.0) };
        state.spool += (target - state.spool) * (dt * 1.2).min(1.0);
        state.afterburner_on = !disabled && input.afterburner && state.spool > 0.95;
        let density_factor = (rho / SEA_LEVEL_DENSITY).powf(0.7);
        let thrust = if state.afterburner_on {
            ac.afterburner_thrust
        } else {
            ac.dry_thrust * state.spool
        } * density_factor;
        force += forward * thrust;

        let accel = force / ac.mass;
        state.velocity += accel * dt;

        // G felt by the pilot: everything except gravity, along body up
        let body_up = rot * Vec3::Y;
        state.g_load = (accel + Vec3::Y * GRAVITY).dot(body_up) / GRAVITY;

        // ---- rotation: fly-by-wire rate command ----
        // control surfaces lose authority as dynamic pressure drops
        let q_ref = 0.5 * SEA_LEVEL_DENSITY * 160.0 * 160.0;
        let authority = (q / q_ref).clamp(0.0, 1.0);
        let effective = authority.max(0.04);

        let (pitch_in, yaw_in, roll_in) = if disabled {
            (0.3, 0.0, 0.6) // a damaged plane spirals down
        } else {
            (input.pitch, input.yaw, input.roll)
        };

        // G limiter: max pitch rate so that v * omega <= g_limit * g
        let g_rate_limit = (ac.g_limit * GRAVITY) / speed.max(30.0);
        // AoA limiter: back off the pull as AoA approaches the limit
        let alpha_margin = ((ac.alpha_limit - alpha.abs()) / 6f32.to_radians()).clamp(0.0, 1.0);
        let pitch_rate_cap = ac.max_rates.x.min(g_rate_limit);
        let mut target_pitch = pitch_in.clamp(-1.0, 1.0) * pitch_rate_cap;
        if target_pitch * alpha.signum() > 0.0 {
            target_pitch *= alpha_margin;
        }

        let target = Vec3::new(
            target_pitch,
            -yaw_in.clamp(-1.0, 1.0) * ac.max_rates.y,
            -roll_in.clamp(-1.0, 1.0) * ac.max_rates.z * (0.4 + 0.6 * authority),
        );
        // how fast the controller can change rates (rad/s^2) at full authority
        let max_accel = Vec3::new(9.0, 3.0, 28.0) * effective;
        let rate_error = target - state.angular_velocity;
        let mut ang_accel = (rate_error * 11.0).clamp(-max_accel, max_accel);

        // weathervane stability: the airflow pulls the nose toward the velocity
        // vector. Always there, but the FBW masks it while it has authority.
        let stability = q / q_ref;
        ang_accel.x -= alpha * 2.5 * stability.min(3.0) * (1.0 - authority * 0.8);
        // +beta = air from the right, so yaw right (negative body y)
        ang_accel.y -= beta * 2.0 * stability.min(3.0);
        // aerodynamic damping
        ang_accel -= state.angular_velocity * Vec3::new(0.6, 0.6, 0.8) * stability.min(2.0);
        if speed < 60.0 || alpha.abs() > ac.stall_alpha * 1.3 {
            // deep stall / tailslide: gravity rotates the nose down to the airflow
            let fall = rot.inverse() * Vec3::NEG_Y;
            ang_accel.x += fall.y.atan2(-fall.z) * 0.8;
            ang_accel.y -= fall.x.atan2(-fall.z) * 0.4;
        }

        state.angular_velocity += ang_accel * dt;
        transform.rotation =
            (rot * Quat::from_scaled_axis(state.angular_velocity * dt)).normalize();
        transform.translation += state.velocity * dt;

        state.alpha = alpha;
        state.beta = beta;
        state.airspeed = speed;
        state.mach = mach;
    }
}

fn ground_collision(
    terrain: Res<Terrain>,
    // (other players' games decide their own crashes)
    query: Query<(Entity, &Transform, &Aircraft), Without<crate::net::Remote>>,
    mut crashed: MessageWriter<AircraftCrashed>,
) {
    for (entity, transform, ac) in &query {
        // check the centre and the wingtips/nose so clipping a tower counts
        let r = ac.radius * 0.6;
        let points = [
            Vec3::ZERO,
            Vec3::new(r, 0.0, 0.0),
            Vec3::new(-r, 0.0, 0.0),
            Vec3::new(0.0, 0.0, -r),
        ];
        if points
            .iter()
            .map(|p| transform.transform_point(*p))
            .any(|p| p.y < 1.0 || terrain.is_solid(p))
        {
            crashed.write(AircraftCrashed {
                entity,
                position: transform.translation,
            });
        }
    }
}

/// Mouse-aim "fine control": nudge the nose toward a direction using only
/// pitch and rudder, never roll. Rolling stays with the player, so the mouse
/// feels like trimming the nose rather than a hands-off autopilot. Returns
/// (pitch, yaw) inputs in -1..1.
pub fn fine_aim(transform: &Transform, state: &FlightState, desired_dir: Vec3) -> (f32, f32) {
    let local = (transform.rotation.inverse() * desired_dir).normalize_or(Vec3::NEG_Z);
    let pitch_err = local.y.atan2(-local.z);
    let yaw_err = local.x.atan2(-local.z);
    let w = state.angular_velocity;
    // gentle gains with rate damping: precise, no overshoot
    let pitch = (pitch_err * 7.0 - w.x * 0.9).clamp(-1.0, 1.0);
    let yaw = (yaw_err * 7.0 + w.y * 1.0).clamp(-1.0, 1.0);
    (pitch, yaw)
}

/// Fly toward a world-space direction. Shared by the AI and the mouse-aim
/// autopilot. Returns (pitch, roll, yaw) inputs in -1..1.
///
/// Bank-to-turn: roll so the target sits "above" the canopy, then pull. When
/// the target is nearly dead ahead, level the wings instead and use rudder
/// for the fine correction.
pub fn steer_towards(
    transform: &Transform,
    state: &FlightState,
    desired_dir: Vec3,
    aggression: f32,
) -> (f32, f32, f32) {
    let inv = transform.rotation.inverse();
    let local = (inv * desired_dir).normalize_or(Vec3::NEG_Z);
    let off_angle = local.angle_between(Vec3::NEG_Z);

    let pitch_err = local.y.atan2(-local.z);
    let yaw_err = local.x.atan2(-local.z);

    // For targets slightly below the nose, put them *under* the canopy and
    // push a little, rather than rolling inverted to pull (which overshoots
    // and makes the plane flip back and forth).
    let push = local.y < 0.0 && off_angle < 0.35;
    let roll_to_target = if push {
        (-local.x).atan2(-local.y)
    } else {
        local.x.atan2(local.y)
    };
    let up_local = inv * Vec3::Y;
    let roll_to_level = up_local.x.atan2(up_local.y);
    let blend = ((off_angle - 0.03) / 0.2).clamp(0.0, 1.0);
    let roll_err = roll_to_level * (1.0 - blend) + roll_to_target * blend;

    let w = state.angular_velocity;
    let roll = (roll_err * 2.0 + w.z * 0.4).clamp(-1.0, 1.0);
    // only pull hard once the lift vector roughly points at the target
    let alignment = (1.0 - (roll_to_target.abs() / 1.2)).clamp(0.15, 1.0);
    let alignment = if blend < 0.5 { 1.0 } else { alignment };
    let min_pitch = if push { -0.5 } else { -0.15 };
    let pitch =
        ((pitch_err * 3.0 - w.x * 0.4) * alignment * aggression).clamp(min_pitch, 1.0);
    let yaw = (yaw_err * 3.0 + w.y * 0.3).clamp(-1.0, 1.0) * (1.0 - blend);
    (pitch, roll, yaw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::time::TimeUpdateStrategy;
    use std::time::Duration;

    /// Minimal app: just the flight model, stepped at exactly one physics tick per update.
    fn app() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
                1.0 / PHYSICS_HZ,
            )))
            .init_resource::<Terrain>()
            .add_plugins(FlightPlugin);
        app
    }

    fn spawn(app: &mut App, speed: f32, input: ControlInput) -> Entity {
        app.world_mut()
            .spawn((
                Transform::from_xyz(0.0, 3_000.0, 0.0),
                Aircraft::fighter(),
                FlightState::new(Vec3::NEG_Z * speed),
                input,
            ))
            .id()
    }

    fn run(app: &mut App, seconds: f32) {
        for _ in 0..(seconds * PHYSICS_HZ as f32) as usize {
            app.update();
        }
    }

    fn state(app: &App, e: Entity) -> (Transform, FlightState) {
        let w = app.world();
        (*w.get::<Transform>(e).unwrap(), w.get::<FlightState>(e).unwrap().clone())
    }

    #[test]
    fn trims_into_roughly_level_flight() {
        let mut app = app();
        let e = spawn(&mut app, 230.0, ControlInput { throttle: 0.7, ..default() });
        run(&mut app, 20.0);
        let (t, s) = state(&app, e);
        println!("alt {} speed {} alpha {}", t.translation.y, s.airspeed, s.alpha.to_degrees());
        assert!(s.airspeed > 150.0 && s.airspeed < 400.0, "speed {}", s.airspeed);
        // hands-off, the weathervane + lift should keep it flying, not plummeting
        assert!(t.translation.y > 1_500.0, "alt {}", t.translation.y);
    }

    #[test]
    fn g_limiter_holds_in_a_max_pull() {
        let mut app = app();
        let e = spawn(&mut app, 300.0, ControlInput { pitch: 1.0, throttle: 1.0, afterburner: true, ..default() });
        let mut max_g: f32 = 0.0;
        for _ in 0..(3.0 * PHYSICS_HZ) as usize {
            app.update();
            max_g = max_g.max(state(&app, e).1.g_load);
        }
        println!("max g {max_g}");
        assert!(max_g > 6.0, "should pull hard: {max_g}");
        assert!(max_g < 10.5, "G limiter: {max_g}");
    }

    #[test]
    fn sustained_turn_bleeds_energy_and_full_ab_accelerates() {
        let mut app = app();
        let turning = spawn(&mut app, 300.0, ControlInput { pitch: 1.0, throttle: 1.0, ..default() });
        let accelerating = spawn(&mut app, 200.0, ControlInput { throttle: 1.0, afterburner: true, ..default() });
        run(&mut app, 6.0);
        let (_, t) = state(&app, turning);
        let (_, a) = state(&app, accelerating);
        println!("turning {} accelerating {}", t.airspeed, a.airspeed);
        assert!(t.airspeed < 290.0, "max-G turn should bleed speed: {}", t.airspeed);
        assert!(a.airspeed > 230.0, "afterburner should accelerate: {}", a.airspeed);
    }

    #[test]
    fn slow_and_idle_stalls() {
        let mut app = app();
        let e = spawn(&mut app, 120.0, ControlInput { pitch: 1.0, throttle: 0.0, ..default() });
        let mut stalled = false;
        for _ in 0..(10.0 * PHYSICS_HZ) as usize {
            app.update();
            let (_, s) = state(&app, e);
            stalled |= s.stalling(&Aircraft::fighter());
        }
        assert!(stalled);
    }

    #[test]
    fn roll_rate_is_fighter_like() {
        let mut app = app();
        let e = spawn(&mut app, 250.0, ControlInput { roll: 1.0, throttle: 0.8, ..default() });
        run(&mut app, 1.0);
        let (_, s) = state(&app, e);
        println!("roll rate {} deg/s", s.angular_velocity.z.to_degrees());
        // rolling right = negative body z
        assert!(s.angular_velocity.z < -2.5, "roll rate {}", s.angular_velocity.z);
    }

    #[test]
    fn steer_towards_slightly_low_target_does_not_flip() {
        let mut app = app();
        let e = spawn(&mut app, 250.0, ControlInput { throttle: 0.8, ..default() });
        // 6 degrees below and a little left
        let desired = Vec3::new(-0.05, -0.1, -1.0).normalize();
        let mut max_roll_input: f32 = 0.0;
        let mut sign_flips = 0;
        let mut last_sign = 0.0;
        for _ in 0..(6.0 * PHYSICS_HZ) as usize {
            let (t, s) = state(&app, e);
            let (pitch, roll, yaw) = steer_towards(&t, &s, desired, 1.0);
            max_roll_input = max_roll_input.max(roll.abs());
            if roll.abs() > 0.3 && roll.signum() != last_sign {
                sign_flips += 1;
                last_sign = roll.signum();
            }
            let mut input = app.world_mut().get_mut::<ControlInput>(e).unwrap();
            input.pitch = pitch;
            input.roll = roll;
            input.yaw = yaw;
            app.update();
        }
        let (t, _) = state(&app, e);
        let err = t.forward().as_vec3().angle_between(desired).to_degrees();
        let bank = t.up().as_vec3().angle_between(Vec3::Y).to_degrees();
        println!("low target: err {err} bank {bank} max roll {max_roll_input} flips {sign_flips}");
        assert!(err < 3.0, "err {err}");
        assert!(bank < 30.0, "should not roll inverted: {bank}");
        assert!(sign_flips <= 2, "roll oscillation: {sign_flips}");
    }

    #[test]
    fn controls_respond_quickly() {
        let mut app = app();
        let e = spawn(&mut app, 250.0, ControlInput { roll: 1.0, pitch: 1.0, throttle: 0.8, ..default() });
        run(&mut app, 0.25);
        let (_, s) = state(&app, e);
        println!("after 0.25 s: roll {:.0} deg/s pitch {:.0} deg/s", -s.angular_velocity.z.to_degrees(), s.angular_velocity.x.to_degrees());
        assert!(-s.angular_velocity.z > 3.0, "roll rate builds fast: {}", s.angular_velocity.z);
        assert!(s.angular_velocity.x > 0.3, "pitch rate builds fast: {}", s.angular_velocity.x);
    }

    #[test]
    fn fine_aim_settles_quickly_without_overshoot() {
        let mut app = app();
        let e = spawn(&mut app, 250.0, ControlInput { throttle: 0.8, ..default() });
        // 8 degrees up and 4 right
        let desired = Vec3::new(0.07, 0.14, -1.0).normalize();
        let mut errs = Vec::new();
        for _ in 0..(3.0 * PHYSICS_HZ) as usize {
            let (t, s) = state(&app, e);
            let (pitch, yaw) = fine_aim(&t, &s, desired);
            let mut input = app.world_mut().get_mut::<ControlInput>(e).unwrap();
            input.pitch = pitch;
            input.yaw = yaw;
            app.update();
            errs.push(state(&app, e).0.forward().as_vec3().angle_between(desired).to_degrees());
        }
        let at_1s = errs[PHYSICS_HZ as usize];
        let final_err = *errs.last().unwrap();
        println!("fine aim: err at 1 s {at_1s:.2} deg, at 3 s {final_err:.2} deg");
        assert!(at_1s < 2.5, "should mostly be there within a second: {at_1s}");
        assert!(final_err < 1.0, "settles: {final_err}");
    }

    #[test]
    fn steer_towards_turns_onto_a_heading() {
        let mut app = app();
        let e = spawn(&mut app, 250.0, ControlInput { throttle: 0.9, ..default() });
        let desired = Vec3::X; // 90 degrees right
        for _ in 0..(12.0 * PHYSICS_HZ) as usize {
            let (t, s) = state(&app, e);
            let (pitch, roll, yaw) = steer_towards(&t, &s, desired, 1.0);
            let mut input = app.world_mut().get_mut::<ControlInput>(e).unwrap();
            input.pitch = pitch;
            input.roll = roll;
            input.yaw = yaw;
            app.update();
        }
        let (t, s) = state(&app, e);
        let heading_err = t.forward().as_vec3().angle_between(desired).to_degrees();
        let up_err = t.up().as_vec3().angle_between(Vec3::Y).to_degrees();
        println!("heading err {heading_err} wings err {up_err} alt {} speed {}", t.translation.y, s.airspeed);
        assert!(heading_err < 10.0, "heading err {heading_err}");
        assert!(up_err < 20.0, "should roll out wings-level: {up_err}");
    }
}
