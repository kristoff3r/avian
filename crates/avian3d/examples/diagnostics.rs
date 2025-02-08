//! This example demonstrates how to enable and display diagnostics for physics,
//! allowing you to monitor the performance of the physics simulation.

#![allow(clippy::unnecessary_cast)]

use avian3d::{math::*, prelude::*};
use bevy::{diagnostic::FrameTimeDiagnosticsPlugin, prelude::*};

fn main() {
    App::new()
        .add_plugins((
            DefaultPlugins,
            PhysicsPlugins::default(),
            // Add the `PhysicsDiagnosticsPlugin` to write physics diagnostics
            // to the `DiagnosticsStore` resource in `bevy_diagnostic`.
            // Requires the `bevy_diagnostic` feature.
            PhysicsDiagnosticsPlugin,
            // Add the `PhysicsDiagnosticsUiPlugin` to display physics diagnostics
            // in a debug UI. Requires the `diagnostic_ui` feature.
            PhysicsDiagnosticsUiPlugin,
            // Optional: Add the `FrameTimeDiagnosticsPlugin` to display frame time.
            FrameTimeDiagnosticsPlugin,
        ))
        // The `PhysicsDiagnosticsUiSettings` resource can be used to configure the diagnostics UI.
        //
        // .insert_resource(PhysicsDiagnosticsUiSettings {
        //     enabled: false,
        //     ..default()
        // })
        .insert_resource(ClearColor(Color::srgb(0.05, 0.05, 0.1)))
        .add_systems(Startup, setup)
        .add_systems(Update, movement)
        .run();
}

// The rest of this example is just setting up a simple scene with cubes that can be moved around.

/// The acceleration used for movement.
#[derive(Component)]
struct MovementAcceleration(Scalar);

fn setup(
    mut commands: Commands,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut meshes: ResMut<Assets<Mesh>>,
) {
    let cube_mesh = meshes.add(Cuboid::default());

    // Ground
    commands.spawn((
        Mesh3d(cube_mesh.clone()),
        MeshMaterial3d(materials.add(Color::srgb(0.7, 0.7, 0.8))),
        Transform::from_xyz(0.0, -2.0, 0.0).with_scale(Vec3::new(100.0, 1.0, 100.0)),
        RigidBody::Static,
        Collider::cuboid(1.0, 1.0, 1.0),
    ));

    let cube_size = 2.0;

    // Spawn cube stacks
    for x in -3..3 {
        for y in -3..15 {
            for z in -3..3 {
                let position = Vec3::new(x as f32, y as f32 + 3.0, z as f32) * (cube_size + 0.05);
                commands.spawn((
                    Mesh3d(cube_mesh.clone()),
                    MeshMaterial3d(materials.add(Color::srgb(0.2, 0.7, 0.9))),
                    Transform::from_translation(position).with_scale(Vec3::splat(cube_size as f32)),
                    RigidBody::Dynamic,
                    Collider::cuboid(1.0, 1.0, 1.0),
                    MovementAcceleration(10.0),
                ));
            }
        }
    }

    // Directional light
    commands.spawn((
        DirectionalLight {
            illuminance: 5000.0,
            shadows_enabled: true,
            ..default()
        },
        Transform::default().looking_at(Vec3::new(-1.0, -2.5, -1.5), Vec3::Y),
    ));

    // Camera
    commands.spawn((
        Camera3d::default(),
        Transform::from_translation(Vec3::new(0.0, 35.0, 80.0)).looking_at(Vec3::Y * 10.0, Vec3::Y),
    ));
}

fn movement(
    time: Res<Time>,
    keyboard_input: Res<ButtonInput<KeyCode>>,
    mut query: Query<(&MovementAcceleration, &mut LinearVelocity)>,
) {
    // Precision is adjusted so that the example works with
    // both the `f32` and `f64` features. Otherwise you don't need this.
    let delta_time = time.delta_secs_f64().adjust_precision();

    for (movement_acceleration, mut linear_velocity) in &mut query {
        let up = keyboard_input.any_pressed([KeyCode::KeyW, KeyCode::ArrowUp]);
        let down = keyboard_input.any_pressed([KeyCode::KeyS, KeyCode::ArrowDown]);
        let left = keyboard_input.any_pressed([KeyCode::KeyA, KeyCode::ArrowLeft]);
        let right = keyboard_input.any_pressed([KeyCode::KeyD, KeyCode::ArrowRight]);

        let horizontal = right as i8 - left as i8;
        let vertical = down as i8 - up as i8;
        let direction =
            Vector::new(horizontal as Scalar, 0.0, vertical as Scalar).normalize_or_zero();

        // Move in input direction
        if direction != Vector::ZERO {
            linear_velocity.x += direction.x * movement_acceleration.0 * delta_time;
            linear_velocity.z += direction.z * movement_acceleration.0 * delta_time;
        }
    }
}
