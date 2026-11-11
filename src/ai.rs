use bevy::prelude::*;

use super::utils;
use super::{Ally, Enemy, AI};

const INITIAL_PLANE_ALTITUDE: f32 = 2000.0;
const TARGET_VELOCITY: f32 = 50.0;

pub struct AIPlugin;

impl Plugin for AIPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, setup).add_systems(
            Update,
            (
                ally_targeting_system,
                enemy_targeting_system,
                ally_ai_system,
                enemy_ai_system,
            ),
        );
    }
}

fn setup(mut commands: Commands, asset_server: Res<AssetServer>) {
    let f22_raptor = asset_server.load(GltfAssetLabel::Scene(0).from_asset("models/f22-raptor/scene.gltf"));

    for i in 0..10 {
        commands
            .spawn((
                Transform::from_translation(Vec3::new(
                    i as f32 * 100.0,
                    INITIAL_PLANE_ALTITUDE,
                    0.0,
                )),
                Visibility::default(),
                AI::default(),
                Enemy,
            ))
            .with_children(|parent| {
                // center of the plane is not at 0,0 so offset slightly
                parent.spawn((
                    WorldAssetRoot(f22_raptor.clone()),
                    Transform::from_translation(Vec3::new(0.0, -5.0, 0.0))
                        .with_rotation(Quat::from_rotation_y(-std::f32::consts::FRAC_PI_2)),
                ));
            });
    }

    for i in 0..10 {
        commands
            .spawn((
                Transform::from_translation(Vec3::new(
                    i as f32 * 100.0,
                    INITIAL_PLANE_ALTITUDE,
                    1000.0,
                )),
                Visibility::default(),
                AI::default(),
                Ally,
            ))
            .with_children(|parent| {
                // center of the plane is not at 0,0 so offset slightly
                parent.spawn((
                    WorldAssetRoot(f22_raptor.clone()),
                    Transform::from_translation(Vec3::new(0.0, -5.0, 0.0))
                        .with_rotation(Quat::from_rotation_y(-std::f32::consts::FRAC_PI_2)),
                ));
            });
    }
}

fn ally_targeting_system(
    mut allies: Query<(&Transform, &mut AI), (With<Ally>, Without<Enemy>)>,
    enemies: Query<(Entity, &Transform), (With<Enemy>, Without<Ally>)>,
) {
    for (transform, mut ai) in allies.iter_mut() {
        if ai.target == None {
            let mut targets = Vec::new();

            for (target, target_transform) in enemies.iter() {
                targets.push((target, target_transform));
            }

            ai.target = utils::find_closest_target(targets, transform.translation)
        }
    }
}

fn enemy_targeting_system(
    allies: Query<(Entity, &Transform), (With<Ally>, Without<Enemy>)>,
    mut enemies: Query<(&Transform, &mut AI), (With<Enemy>, Without<Ally>)>,
) {
    for (transform, mut ai) in enemies.iter_mut() {
        if ai.target == None {
            let mut targets = Vec::new();

            for (target, target_transform) in allies.iter() {
                targets.push((target, target_transform));
            }

            ai.target = utils::find_closest_target(targets, transform.translation)
        }
    }
}

fn ally_ai_system(
    mut query: Query<(&mut Transform, &mut AI), (With<Ally>, Without<Enemy>)>,
    targets: Query<&Transform, (With<Enemy>, Without<Ally>)>,
) {
    for (transform, mut ai) in query.iter_mut() {
        let mut target_vec = transform.forward().as_vec3();

        if let Some(target) = ai.target {
            if let Ok(transform) = targets.get(target) {
                target_vec = transform.translation
            } else {
                error!("Target not found!");
                ai.target = None;
            }
        }

        ai_follow_target(transform, ai, target_vec)
    }
}

fn enemy_ai_system(
    mut query: Query<(&mut Transform, &mut AI), (With<Enemy>, Without<Ally>)>,
    targets: Query<&Transform, (With<Ally>, Without<Enemy>)>,
) {
    for (transform, mut ai) in query.iter_mut() {
        let mut target_vec = transform.forward().as_vec3();

        if let Some(target) = ai.target {
            if let Ok(transform) = targets.get(target) {
                target_vec = transform.translation + transform.forward().as_vec3() * -100.0
            } else {
                error!("Target not found!");
                ai.target = None;
            }
        }

        ai_follow_target(transform, ai, target_vec)
    }
}

fn ai_follow_target(mut transform: Mut<Transform>, mut ai: Mut<AI>, target_vec: Vec3) {
    let forwards = transform.forward().as_vec3();

    let axis_deviance = ai
        .velocity
        .normalize_or_zero()
        .dot(forwards.normalize_or_zero())
        / forwards.length();

    let angle = Vec3::Z;
    transform.look_at(target_vec, angle);

    // thrust
    let normal = ai.velocity.normalize();
    let speed = ai.velocity.length();
    ai.velocity = normal * utils::lerp(speed, TARGET_VELOCITY, 0.1);

    // nudge velocity vector towards the forwards vector
    ai.velocity = ai.velocity.lerp(
        transform.forward().as_vec3() * ai.velocity.length(),
        utils::clamp(axis_deviance, 0.1, 1.0) * 0.05,
    );

    transform.translation += ai.velocity;

    if transform.translation.y < 0.0 {
        transform.translation.y = 0.0;
    }
}
