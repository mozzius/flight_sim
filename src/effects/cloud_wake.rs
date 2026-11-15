//! Cloud wake: a jet flying out of a cloud drags some of it along, a plume of
//! torn wisps pulled out behind the exit point that follow the jet for a
//! moment, then hang in the air and dissolve.
//!
//! Each jet charges up a `carried` amount while [`CloudField`] says it is in
//! cloud and spends it in clear air, emitting white puffs along its path at a
//! rate proportional to what's left. The puffs are lit like any smoke and
//! hidden behind (and inside) the real clouds by the occlusion map.

use std::collections::HashMap;

use bevy::prelude::*;
use rand::RngExt;

use super::{particles::Particle, particles::Particles, rand_dir};
use crate::{camera::MainCamera, clouds::CloudField, flight::FlightState, game::Player};

/// `density_at` above this counts as being in cloud.
const IN_CLOUD: f32 = 0.03;
/// Charge per second in cloud, plus this much more per unit density.
const CHARGE: f32 = 1.2;
const CHARGE_DENSE: f32 = 3.0;
/// Time constant (s) of the drain in clear air, and where emission stops.
const DRAIN: f32 = 0.55;
const EMPTY: f32 = 0.05;
/// Puffs per second at full charge (fewer on the phone path).
const RATE: f32 = 120.0;
/// Jets further than this from the camera don't emit (they still charge).
const MAX_DISTANCE: f32 = 6_000.0;

#[derive(Default)]
pub struct Wake {
    carried: f32,
    /// fractional puffs owed
    owed: f32,
    /// where the wake point was last frame
    last: Option<Vec3>,
    in_cloud: bool,
}

#[allow(clippy::too_many_arguments)]
pub fn cloud_wake(
    time: Res<Time>,
    real: Res<Time<Real>>,
    field: Option<Res<CloudField>>,
    mut particles: ResMut<Particles>,
    aircraft: Query<(Entity, &GlobalTransform, &FlightState, Has<Player>)>,
    camera: Query<&GlobalTransform, With<MainCamera>>,
    mut wakes: Local<HashMap<Entity, Wake>>,
) {
    let dt = time.delta_secs();
    let Some(field) = field else { return };
    if dt <= 0.0 {
        return;
    }
    let cam = camera.single().map(|c| c.translation()).ok();
    let low = crate::platform::low_memory();
    let (rate, scale) = if low { (RATE * 0.6, 1.25) } else { (RATE, 1.0) };
    let log = std::env::var("FLIGHT_SIM_LOG").is_ok();
    wakes.retain(|e, _| aircraft.contains(*e));
    let mut rng = rand::rng();
    for (entity, gt, state, player) in &aircraft {
        let pos = gt.translation();
        let wake = wakes.entry(entity).or_default();
        let density = field.density_at(pos);
        let in_cloud = density > IN_CLOUD;
        if log && player && in_cloud != wake.in_cloud {
            info!(
                "cloud wake: {} cloud at real t={:.2} pos={:.0} carried {:.2}",
                if in_cloud { "entered" } else { "left" },
                real.elapsed_secs(),
                pos,
                wake.carried
            );
        }
        wake.in_cloud = in_cloud;
        // the wake point, just behind the wings
        let tail = gt.transform_point(Vec3::new(0.0, 0.2, 4.0));
        let last = wake.last.replace(tail);
        if in_cloud {
            wake.carried = (wake.carried + dt * (CHARGE + CHARGE_DENSE * density)).min(1.0);
            continue;
        }
        if wake.carried < EMPTY {
            wake.carried = 0.0;
            wake.owed = 0.0;
            continue;
        }
        let c = wake.carried;
        wake.carried *= (-dt / DRAIN).exp();
        // far away, or teleported (respawn): nothing to see
        let Some(last) = last.filter(|l| l.distance(tail) < 200.0) else { continue };
        if cam.is_some_and(|cam| cam.distance(pos) > MAX_DISTANCE) {
            continue;
        }
        // a steady stream that thins out (fewer puffs would leave beads at the
        // tail), spread evenly over the path moved: by distance, not time, as
        // the transform can move in uneven steps
        let moved = last.distance(tail);
        wake.owed += rate * c.sqrt() * moved / state.velocity.length().max(50.0);
        let rot = gt.rotation();
        while wake.owed >= 1.0 {
            wake.owed -= 1.0;
            let along = last.lerp(tail, rng.random::<f32>());
            let vel = state.velocity * rng.random_range(0.6..0.92) + rand_dir(&mut rng) * 7.0;
            let spread = Vec3::new(
                rng.random_range(-10.0..10.0),
                rng.random_range(-5.0..6.0),
                rng.random_range(-6.0..6.0),
            ) * (0.6 + 0.4 * c);
            let s = (0.6 + 0.4 * c) * scale;
            particles.add(
                Particle {
                    // dragged along with the jet, then left hanging
                    vel,
                    life: rng.random_range(3.0..5.0),
                    size: (rng.random_range(12.0..20.0) * s, rng.random_range(28.0..48.0) * s),
                    grow: 3.0,
                    drag: rng.random_range(0.8..1.2),
                    spin: rng.random_range(-0.3..0.3),
                    // far brighter than smoke (cloud scatters light many times over), to
                    // match the lit cloud it came from
                    albedo: Vec3::new(1.75, 1.8, 1.9) * rng.random_range(0.95..1.05),
                    opacity: 0.14 * c.powf(0.7),
                    // faint while still beside the jet (and the chase camera)
                    fade_in: 0.35,
                    // eroding from birth: torn wisps rather than round puffs
                    dissolve: 0.05,
                    near: 1.6,
                    ..Particle::puff(along + rot * spread)
                },
                0.5,
            );
        }
    }
}
