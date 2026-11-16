//! Cloud wake: a jet flying out of a cloud tears some of it away, the cloud's
//! edge pulled out behind the exit point into soft wisps that drift to a
//! stop and dissolve within a few seconds.
//!
//! Each jet charges up a `carried` amount (and already plumes) while [`CloudField`] says it is in
//! the dense part of a cloud and spends it quickly in clear air, emitting many small, faint
//! vapour particles ([`Particle::vapour`]: outline-free, fog-shaded) along
//! its path at a rate that falls with what's left and gusts at random, plus
//! a few thicker clumps right at the exit. About half cling to the jet and
//! stream off it for a second, so the chase camera sees the plume too. They
//! are hidden behind (and
//! inside) the real clouds by the occlusion map.

use std::collections::HashMap;

use bevy::prelude::*;
use rand::RngExt;

use super::{particles::Particle, particles::Particles, rand_dir};
use crate::{camera::MainCamera, clouds::CloudField, flight::FlightState, game::Player};

/// In the dense part of a cloud above this `density_at` (charging), out
/// again (the wake starts) below `LEAVE`. `density_at` has no shape noise,
/// so below ~0.1 it's the thin haze and fluff round a cloud, which looks
/// like clear air: waiting for zero fired the plume seconds late.
const ENTER: f32 = 0.12;
const LEAVE: f32 = 0.08;
/// Thick clouds: out below this fraction of the thickest density seen on
/// the way through, back in above `REENTER` of it.
const RELEASE: f32 = 0.4;
const REENTER: f32 = 0.7;
/// Time constant (s) the remembered peak fades over once out, so the next
/// cloud is judged on its own.
const PEAK_FADE: f32 = 2.5;
/// Charge per second in cloud, plus this much more per unit density.
const CHARGE: f32 = 1.2;
const CHARGE_DENSE: f32 = 3.0;
/// Time constant (s) of the drain in clear air, and where emission stops:
/// most of the cloud comes away in the first ~200-300 m.
const DRAIN: f32 = 0.7;
const EMPTY: f32 = 0.05;
/// Wisps per second at full charge (fewer on the phone path); about 150-200
/// per exit with the clumps.
const RATE: f32 = 200.0;
/// Share of the wisps that cling to the jet (see [`streamer`]).
const CLING: f64 = 0.45;
/// Thick clumps of wisps left right at the exit point.
const CLUMPS: usize = 3;
const CLUMP_WISPS: usize = 6;
/// Least light on wisps shed inside a cloud (see `sunlight`).
const IN_CLOUD_LIGHT: f32 = 0.85;
/// ...and on those shed in the open after leaving it.
const OUT_LIGHT: f32 = 0.55;
/// Inside, a thinner stream (rate and opacity): you're in fog already, and
/// seconds of wisps clinging to the airframe pile up into a ball round it.
const IN_CLOUD_RATE: f32 = 0.35;
const IN_CLOUD_OPACITY: f32 = 0.45;
/// Jets further than this from the camera don't emit (they still charge).
const MAX_DISTANCE: f32 = 6_000.0;

#[derive(Default)]
pub struct Wake {
    carried: f32,
    /// fractional wisps owed
    owed: f32,
    /// emission gusts (gaps and thicker patches), re-rolled every so often
    gust: f32,
    gust_left: f32,
    /// where the wake point was last frame
    last: Option<Vec3>,
    in_cloud: bool,
    /// thickest density seen in this cloud
    peak: f32,
}

/// One wisp of torn cloud. `c` is the charge left (0..1): wisps shrink and
/// fade toward the jet end of the plume.
fn wisp(rng: &mut impl rand::Rng, pos: Vec3, jet_vel: Vec3, c: f32, scale: f32, light: f32) -> Particle {
    // left behind with a little of the jet's speed, which drag soon kills
    let vel = jet_vel * rng.random_range(0.15..0.5) + rand_dir(rng) * 5.0;
    let s = (0.45 + 0.55 * c) * scale;
    Particle {
        vel,
        life: rng.random_range(2.2..3.6),
        size: (rng.random_range(7.0..13.0) * s, rng.random_range(16.0..30.0) * s),
        grow: 2.0,
        drag: rng.random_range(1.6..2.4),
        spin: rng.random_range(-0.15..0.15),
        // smeared along its motion while it's still being dragged
        stretch: 0.12,
        // a touch above 1: thin vapour lit by the sun and sky, like the cloud
        // it came from (more than this glows at sunset)
        albedo: Vec3::new(0.88, 0.91, 0.98) * light * rng.random_range(0.97..1.03),
        opacity: 0.6 * c.powf(0.8),
        fade_in: 0.2,
        dissolve: 0.0,
        near: 1.6,
        ..Particle::vapour(pos)
    }
}

/// A wisp that clings to the airframe: it keeps most of the jet's speed and
/// streams back off the wings over a second or so, so the plume also shows
/// from the chase camera (the rest is left behind at the exit in a blink).
fn streamer(rng: &mut impl rand::Rng, pos: Vec3, jet_vel: Vec3, c: f32, scale: f32, light: f32) -> Particle {
    let s = (0.45 + 0.55 * c) * scale;
    Particle {
        vel: jet_vel * rng.random_range(0.86..0.97) + rand_dir(rng) * 3.0,
        life: rng.random_range(0.9..1.7),
        size: (rng.random_range(3.0..5.5) * s, rng.random_range(9.0..16.0) * s),
        drag: rng.random_range(0.3..0.7),
        stretch: 0.05,
        opacity: 0.42 * c.powf(0.8),
        fade_in: 0.12,
        // thin out well before they reach the chase camera
        near: 3.0,
        ..wisp(rng, pos, jet_vel, c, scale, light)
    }
}

/// How much sun reaches `p` through the cloud toward the sun (by
/// [`CloudField`]), as a light scale: the wisps don't get the clouds' own
/// shadows, so torn from the shaded side of a bank they'd glow against it
/// (worst at sunset, when the sun comes in low through the whole layer).
fn sunlight(field: &CloudField, p: Vec3, sun: Vec3) -> f32 {
    const STEP: f32 = 150.0;
    let depth: f32 = (1..=15).map(|i| field.density_at(p + sun * (i as f32 * STEP))).sum::<f32>() * STEP;
    let transmittance = (-depth * 0.04).exp();
    0.28 + 0.72 * transmittance
}

#[allow(clippy::too_many_arguments)]
pub fn cloud_wake(
    time: Res<Time>,
    real: Res<Time<Real>>,
    field: Option<Res<CloudField>>,
    mut particles: ResMut<Particles>,
    aircraft: Query<(Entity, &GlobalTransform, &FlightState, Has<Player>)>,
    camera: Query<&GlobalTransform, With<MainCamera>>,
    sun: Query<&GlobalTransform, With<DirectionalLight>>,
    mut wakes: Local<HashMap<Entity, Wake>>,
) {
    let dt = time.delta_secs();
    let Some(field) = field else { return };
    if dt <= 0.0 {
        return;
    }
    let cam = camera.single().map(|c| c.translation()).ok();
    let sun = sun.iter().next().map_or(Vec3::Y, |s| s.back().as_vec3());
    let low = crate::platform::low_memory();
    let (rate, scale) = if low { (RATE * 0.6, 1.25) } else { (RATE, 1.0) };
    let log = std::env::var("FLIGHT_SIM_LOG").is_ok();
    wakes.retain(|e, _| aircraft.contains(*e));
    let mut rng = rand::rng();
    for (entity, gt, state, player) in &aircraft {
        let pos = gt.translation();
        let wake = wakes.entry(entity).or_default();
        let density = field.density_at(pos);
        let in_cloud = if wake.in_cloud {
            density > LEAVE.max(wake.peak * RELEASE)
        } else {
            density > ENTER.max(wake.peak * REENTER)
        };
        if in_cloud {
            wake.peak = wake.peak.max(density);
        } else if density <= LEAVE {
            wake.peak = 0.0;
        } else {
            wake.peak *= (-dt / PEAK_FADE).exp();
        }
        if log && player && in_cloud != wake.in_cloud {
            info!(
                "cloud wake: {} cloud at real t={:.2} pos={:.0} carried {:.2}",
                if in_cloud { "entered" } else { "left" },
                real.elapsed_secs(),
                pos,
                wake.carried
            );
        }
        let just_left = wake.in_cloud && !in_cloud;
        wake.in_cloud = in_cloud;
        // the wake point, just behind the wings
        let tail = gt.transform_point(Vec3::new(0.0, 0.2, 4.0));
        let last = wake.last.replace(tail);
        // it plumes all the way through the dense part (building up over the
        // first half second or so), then tails off in clear air
        if in_cloud {
            wake.carried = (wake.carried + dt * (CHARGE + CHARGE_DENSE * density)).min(1.0);
        }
        if wake.carried < EMPTY {
            if !in_cloud {
                wake.carried = 0.0;
            }
            wake.owed = 0.0;
            continue;
        }
        let c = wake.carried;
        if !in_cloud {
            wake.carried *= (-dt / DRAIN).exp();
        }
        // far away, or teleported (respawn): nothing to see
        let Some(last) = last.filter(|l| l.distance(tail) < 200.0) else { continue };
        if cam.is_some_and(|cam| cam.distance(pos) > MAX_DISTANCE) {
            continue;
        }
        let rot = gt.rotation();
        // inside, the fog all round is lit by light scattered through the
        // cloud: wisps darkened by the sun's path read as a murky blob
        // (out in the open on the shaded side it can be darker, but not so
        // dark it turns murky next to the stream shed inside)
        let light = sunlight(&field, pos, sun).max(if in_cloud { IN_CLOUD_LIGHT } else { OUT_LIGHT });
        if just_left && log && player {
            info!("cloud wake: sunlight {light:.2} (sun {sun:.2})");
        }
        if just_left {
            // the cloud's edge, torn away in a few thick clumps
            for _ in 0..CLUMPS {
                let centre = last
                    + rot * Vec3::new(rng.random_range(-9.0..9.0), rng.random_range(-4.0..5.0), rng.random_range(-10.0..25.0));
                for _ in 0..CLUMP_WISPS {
                    let at = centre + rand_dir(&mut rng) * rng.random_range(0.0..8.0);
                    let mut p = wisp(&mut rng, at, state.velocity, c, scale * 1.15, light);
                    p.opacity *= 1.3;
                    particles.add(p, 0.5);
                }
            }
        }
        // gusts: patches of thicker wisps and near-gaps, so it isn't a tube
        wake.gust_left -= dt;
        if wake.gust_left <= 0.0 {
            wake.gust_left = rng.random_range(0.06..0.16);
            wake.gust = if rng.random_bool(0.2) { rng.random_range(0.0..0.25) } else { rng.random_range(0.6..1.6) };
        }
        // spread evenly over the path moved: by distance, not time, as the
        // transform can move in uneven steps (it clumped into beads)
        let moved = last.distance(tail);
        let thin = if in_cloud { IN_CLOUD_RATE } else { 1.0 };
        wake.owed += rate * thin * c.sqrt() * wake.gust * moved / state.velocity.length().max(50.0);
        let fade = if in_cloud { IN_CLOUD_OPACITY } else { 1.0 };
        while wake.owed >= 1.0 {
            wake.owed -= 1.0;
            let along = last.lerp(tail, rng.random::<f32>());
            let spread = Vec3::new(
                rng.random_range(-9.0..9.0),
                rng.random_range(-4.0..5.0),
                rng.random_range(-5.0..5.0),
            ) * (0.5 + 0.5 * c);
            if rng.random_bool(CLING) {
                // off the wings and the back of the fuselage
                let at = tail + rot * Vec3::new(rng.random_range(-6.5..6.5), rng.random_range(-0.6..1.2), rng.random_range(-7.0..1.0));
                let mut p = streamer(&mut rng, at, state.velocity, c, scale, light);
                p.opacity *= fade;
                particles.add(p, 0.5);
                continue;
            }
            let mut p = wisp(&mut rng, along + rot * spread, state.velocity, c, scale, light);
            p.opacity *= fade;
            particles.add(p, 0.5);
        }
    }
}
