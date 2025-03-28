//! Computes contacts between entities.
//!
//! See [`NarrowPhasePlugin`].

use core::marker::PhantomData;

use crate::{
    dynamics::solver::{
        contact::ContactConstraint, ContactConstraints, ContactSoftnessCoefficients,
    },
    prelude::*,
};
#[cfg(feature = "parallel")]
use bevy::tasks::{ComputeTaskPool, ParallelSlice};
use bevy::{
    ecs::{
        intern::Interned,
        schedule::{ExecutorKind, LogLevel, ScheduleBuildSettings, ScheduleLabel},
        system::{StaticSystemParam, SystemParam, SystemParamItem},
    },
    prelude::*,
};
use dynamics::solver::SolverDiagnostics;

/// Computes contacts between entities and generates contact constraints for them.
///
/// Collisions are only checked between entities contained in [`BroadCollisionPairs`],
/// which is handled by the [`BroadPhasePlugin`].
///
/// The results of the narrow phase are added into [`Collisions`].
/// By default, a [`ContactConstraint`] is also generated for each contact manifold
/// and added to the [`ContactConstraints`] resource.
///
/// The plugin takes a collider type. This should be [`Collider`] for
/// the vast majority of applications, but for custom collisión backends
/// you may use any collider that implements the [`AnyCollider`] trait.
pub struct NarrowPhasePlugin<C: AnyCollider, H: CollisionHooks = ()> {
    schedule: Interned<dyn ScheduleLabel>,
    /// If `true`, the narrow phase will generate [`ContactConstraint`]s
    /// and add them to the [`ContactConstraints`] resource.
    ///
    /// Contact constraints are used by the [`SolverPlugin`] for solving contacts.
    generate_constraints: bool,
    _phantom: PhantomData<(C, H)>,
}

impl<C: AnyCollider, H: CollisionHooks> NarrowPhasePlugin<C, H> {
    /// Creates a [`NarrowPhasePlugin`] with the schedule used for running its systems
    /// and whether it should generate [`ContactConstraint`]s for the [`ContactConstraints`] resource.
    ///
    /// Contact constraints are used by the [`SolverPlugin`] for solving contacts.
    ///
    /// The default schedule is [`PhysicsSchedule`].
    pub fn new(schedule: impl ScheduleLabel, generate_constraints: bool) -> Self {
        Self {
            schedule: schedule.intern(),
            generate_constraints,
            _phantom: PhantomData,
        }
    }
}

impl<C: AnyCollider, H: CollisionHooks> Default for NarrowPhasePlugin<C, H> {
    fn default() -> Self {
        Self::new(PhysicsSchedule, true)
    }
}

impl<C: AnyCollider, H: CollisionHooks + 'static> Plugin for NarrowPhasePlugin<C, H>
where
    for<'w, 's> SystemParamItem<'w, 's, H>: CollisionHooks,
{
    fn build(&self, app: &mut App) {
        // For some systems, we only want one instance, even if there are multiple
        // NarrowPhasePlugin instances with different collider types.
        let is_first_instance = !app.world().is_resource_added::<NarrowPhaseInitialized>();

        app.init_resource::<NarrowPhaseInitialized>()
            .init_resource::<NarrowPhaseConfig>()
            .init_resource::<Collisions>()
            .init_resource::<DefaultFriction>()
            .init_resource::<DefaultRestitution>()
            .register_type::<(NarrowPhaseConfig, DefaultFriction, DefaultRestitution)>();

        if self.generate_constraints {
            app.init_resource::<ContactConstraints>();
        }

        app.configure_sets(
            self.schedule,
            (
                NarrowPhaseSet::First,
                NarrowPhaseSet::CollectCollisions,
                NarrowPhaseSet::PostProcess,
                NarrowPhaseSet::GenerateConstraints,
                NarrowPhaseSet::Last,
            )
                .chain()
                .in_set(PhysicsStepSet::NarrowPhase),
        );

        // Set up the PostProcessCollisions schedule for user-defined systems
        // that filter and modify collisions.
        app.edit_schedule(PostProcessCollisions, |schedule| {
            schedule
                .set_executor_kind(ExecutorKind::SingleThreaded)
                .set_build_settings(ScheduleBuildSettings {
                    ambiguity_detection: LogLevel::Error,
                    ..default()
                });
        });

        // Manage collision states like `during_current_frame` and remove old contacts.
        // Only one narrow phase instance should do this.
        // TODO: It would be nice not to have collision state logic in the narrow phase.
        if is_first_instance {
            app.add_systems(
                self.schedule,
                (
                    // Reset collision states.
                    reset_collision_states
                        .in_set(PhysicsStepSet::NarrowPhase)
                        .after(NarrowPhaseSet::First)
                        .before(NarrowPhaseSet::CollectCollisions),
                    // Remove ended collisions after contact reporting
                    remove_ended_collisions
                        .after(PhysicsStepSet::ReportContacts)
                        .before(PhysicsStepSet::Sleeping),
                ),
            );
        }

        // Collect contacts into `Collisions`.
        app.add_systems(
            self.schedule,
            collect_collisions::<C, H>
                .in_set(NarrowPhaseSet::CollectCollisions)
                // Allowing ambiguities is required so that it's possible
                // to have multiple collision backends at the same time.
                .ambiguous_with_all(),
        );

        if self.generate_constraints {
            if is_first_instance {
                // Clear contact constraints.
                app.add_systems(
                    self.schedule,
                    (|mut constraints: ResMut<ContactConstraints>| {
                        constraints.clear();
                    })
                    .after(NarrowPhaseSet::PostProcess)
                    .before(NarrowPhaseSet::GenerateConstraints),
                );
            }

            // Generate contact constraints.
            app.add_systems(
                self.schedule,
                generate_constraints::<C>
                    .in_set(NarrowPhaseSet::GenerateConstraints)
                    // Allowing ambiguities is required so that it's possible
                    // to have multiple collision backends at the same time.
                    .ambiguous_with_all(),
            );
        }

        if is_first_instance {
            app.add_systems(
                self.schedule,
                run_post_process_collisions_schedule.in_set(NarrowPhaseSet::PostProcess),
            );
        }
    }

    fn finish(&self, app: &mut App) {
        // Register timer and counter diagnostics for collision detection.
        app.register_physics_diagnostics::<CollisionDiagnostics>();
    }
}

#[derive(Resource, Default)]
struct NarrowPhaseInitialized;

/// A resource for configuring the [narrow phase](NarrowPhasePlugin).
#[derive(Resource, Reflect, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serialize", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serialize", reflect(Serialize, Deserialize))]
#[reflect(Debug, Resource, PartialEq)]
pub struct NarrowPhaseConfig {
    /// The default maximum [speculative margin](SpeculativeMargin) used for
    /// [speculative collisions](dynamics::ccd#speculative-collision). This can be overridden
    /// for individual entities with the [`SpeculativeMargin`] component.
    ///
    /// By default, the maximum speculative margin is unbounded, so contacts can be predicted
    /// from any distance, provided that the bodies are moving fast enough. As the prediction distance
    /// grows, the contact data becomes more and more approximate, and in rare cases, it can even cause
    /// [issues](dynamics::ccd#caveats-of-speculative-collision) such as ghost collisions.
    ///
    /// By limiting the maximum speculative margin, these issues can be mitigated, at the cost
    /// of an increased risk of tunneling. Setting it to `0.0` disables speculative collision
    /// altogether for entities without [`SpeculativeMargin`].
    ///
    /// This is implicitly scaled by the [`PhysicsLengthUnit`].
    ///
    /// Default: `MAX` (unbounded)
    pub default_speculative_margin: Scalar,

    /// A contact tolerance that acts as a minimum bound for the [speculative margin](dynamics::ccd#speculative-collision).
    ///
    /// A small, positive contact tolerance helps ensure that contacts are not missed
    /// due to numerical issues or solver jitter for objects that are in continuous
    /// contact, such as pushing against each other.
    ///
    /// Making the contact tolerance too large will have a negative impact on performance,
    /// as contacts will be computed even for objects that are not in close proximity.
    ///
    /// This is implicitly scaled by the [`PhysicsLengthUnit`].
    ///
    /// Default: `0.005`
    pub contact_tolerance: Scalar,

    /// If `true`, the current contacts will be matched with the previous contacts
    /// based on feature IDs or contact positions, and the contact impulses from
    /// the previous frame will be copied over for the new contacts.
    ///
    /// Using these impulses as the initial guess is referred to as *warm starting*,
    /// and it can help the contact solver resolve overlap and stabilize much faster.
    ///
    /// Default: `true`
    pub match_contacts: bool,
}

impl Default for NarrowPhaseConfig {
    fn default() -> Self {
        Self {
            default_speculative_margin: Scalar::MAX,
            contact_tolerance: 0.005,
            match_contacts: true,
        }
    }
}

/// System sets for systems running in [`PhysicsStepSet::NarrowPhase`].
#[derive(SystemSet, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NarrowPhaseSet {
    /// Runs at the start of the narrow phase. Empty by default.
    First,
    /// Computes contacts between entities and adds them to the [`Collisions`] resource.
    CollectCollisions,
    /// Responsible for running the [`PostProcessCollisions`] schedule to allow user-defined systems
    /// to filter and modify collisions.
    ///
    /// If you want to modify or remove collisions after [`NarrowPhaseSet::CollectCollisions`], you can
    /// add custom systems to this set, or to [`PostProcessCollisions`].
    PostProcess,
    /// Generates [`ContactConstraint`]s and adds them to [`ContactConstraints`].
    GenerateConstraints,
    /// Runs at the end of the narrow phase. Empty by default.
    Last,
}

fn collect_collisions<C: AnyCollider, H: CollisionHooks + 'static>(
    mut narrow_phase: NarrowPhase<C>,
    broad_collision_pairs: Res<BroadCollisionPairs>,
    time: Res<Time>,
    hooks: StaticSystemParam<H>,
    context: StaticSystemParam<C::Context>,
    #[cfg(not(feature = "parallel"))] commands: Commands,
    #[cfg(feature = "parallel")] commands: ParallelCommands,
    mut diagnostics: ResMut<CollisionDiagnostics>,
) where
    for<'w, 's> SystemParamItem<'w, 's, H>: CollisionHooks,
{
    let start = crate::utils::Instant::now();

    narrow_phase.update::<H>(
        &broad_collision_pairs,
        time.delta_seconds_adjusted(),
        &hooks,
        &context,
        commands,
    );

    diagnostics.narrow_phase = start.elapsed();
    diagnostics.contact_count = narrow_phase.collisions.get_internal().len() as u32;
}

// TODO: It'd be nice to generate the constraint in the same parallel loop as `collect_collisions`
//       to avoid the extra iteration and queries. This is possible, but it wouldn't work with the current
//       `PostProcessCollisions` setup.
fn generate_constraints<C: AnyCollider>(
    narrow_phase: NarrowPhase<C>,
    mut constraints: ResMut<ContactConstraints>,
    contact_softness: Res<ContactSoftnessCoefficients>,
    time: Res<Time>,
    mut collision_diagnostics: ResMut<CollisionDiagnostics>,
    solver_diagnostics: Option<ResMut<SolverDiagnostics>>,
) {
    let start = crate::utils::Instant::now();

    let delta_secs = time.delta_seconds_adjusted();

    // TODO: Parallelize.
    for contacts in narrow_phase.collisions.get_internal().values() {
        let Ok([collider1, collider2]) = narrow_phase
            .collider_query
            .get_many([contacts.entity1, contacts.entity2])
        else {
            continue;
        };

        let body1_bundle = collider1
            .rigid_body
            .and_then(|&ColliderOf { rigid_body }| narrow_phase.body_query.get(rigid_body).ok());
        let body2_bundle = collider2
            .rigid_body
            .and_then(|&ColliderOf { rigid_body }| narrow_phase.body_query.get(rigid_body).ok());
        if let (Some((body1, rb_collision_margin1)), Some((body2, rb_collision_margin2))) = (
            body1_bundle.map(|(body, rb_collision_margin1, _)| (body, rb_collision_margin1)),
            body2_bundle.map(|(body, rb_collision_margin2, _)| (body, rb_collision_margin2)),
        ) {
            // At least one of the bodies must be dynamic for contact constraints
            // to be generated.
            if !body1.rb.is_dynamic() && !body2.rb.is_dynamic() {
                continue;
            }

            // Use the collider's own collision margin if specified, and fall back to the body's
            // collision margin.
            //
            // The collision margin adds artificial thickness to colliders for performance
            // and stability. See the `CollisionMargin` documentation for more details.
            let collision_margin1 = collider1
                .collision_margin
                .or(rb_collision_margin1)
                .map_or(0.0, |margin| margin.0);
            let collision_margin2 = collider2
                .collision_margin
                .or(rb_collision_margin2)
                .map_or(0.0, |margin| margin.0);
            let collision_margin_sum = collision_margin1 + collision_margin2;

            // Generate contact constraints for the computed contacts
            // and add them to `constraints`.
            narrow_phase.generate_constraints(
                contacts,
                &mut constraints,
                &body1,
                &body2,
                &collider1,
                &collider2,
                collision_margin_sum,
                *contact_softness,
                delta_secs,
            );
        }
    }

    collision_diagnostics.generate_constraints = start.elapsed();

    if let Some(mut solver_diagnostics) = solver_diagnostics {
        solver_diagnostics.contact_constraint_count = constraints.len() as u32;
    }
}

/// A system parameter for managing the narrow phase.
///
/// The narrow phase computes contacts for each intersection pair
/// in [`BroadCollisionPairs`], adds them to the [`Collisions`] resource,
/// and generates [`ContactConstraints`] for the contacts.
#[derive(SystemParam)]
pub struct NarrowPhase<'w, 's, C: AnyCollider> {
    parallel_commands: ParallelCommands<'w, 's>,
    collider_query: Query<'w, 's, ColliderQuery<C>, Without<ColliderDisabled>>,
    body_query: Query<
        'w,
        's,
        (
            RigidBodyQueryReadOnly,
            Option<&'static CollisionMargin>,
            Option<&'static SpeculativeMargin>,
        ),
        Without<RigidBodyDisabled>,
    >,
    /// Contacts found by the narrow phase.
    pub collisions: ResMut<'w, Collisions>,
    /// Configuration options for the narrow phase.
    pub config: Res<'w, NarrowPhaseConfig>,
    default_friction: Res<'w, DefaultFriction>,
    default_restitution: Res<'w, DefaultRestitution>,
    length_unit: Res<'w, PhysicsLengthUnit>,
    // These are scaled by the length unit.
    default_speculative_margin: Local<'s, Scalar>,
    contact_tolerance: Local<'s, Scalar>,
}

impl<C: AnyCollider> NarrowPhase<'_, '_, C> {
    /// Updates the narrow phase by computing [`Contacts`] based on [`BroadCollisionPairs`]
    /// and adding them to [`Collisions`].
    fn update<H: CollisionHooks + 'static>(
        &mut self,
        broad_collision_pairs: &[(Entity, Entity)],
        delta_secs: Scalar,
        hooks: &SystemParamItem<H>,
        context: &SystemParamItem<C::Context>,
        #[cfg(not(feature = "parallel"))] mut commands: Commands,
        #[cfg(feature = "parallel")] par_commands: ParallelCommands,
    ) where
        for<'w, 's> SystemParamItem<'w, 's, H>: CollisionHooks,
    {
        // TODO: These scaled versions could be in their own resource
        //       and updated just before physics every frame.
        // Cache default margins scaled by the length unit.
        if self.config.is_changed() {
            *self.default_speculative_margin =
                self.length_unit.0 * self.config.default_speculative_margin;
            *self.contact_tolerance = self.length_unit.0 * self.config.contact_tolerance;
        }

        #[cfg(feature = "parallel")]
        {
            // TODO: Verify if `par_splat_map` is deterministic. If not, sort the constraints (and collisions).
            broad_collision_pairs
                .iter()
                .par_splat_map(ComputeTaskPool::get(), None, |_i, chunks| {
                    let mut new_collisions = Vec::<Contacts>::with_capacity(chunks.len());

                    // Compute contacts for this intersection pair and generate
                    // contact constraints for them.
                    par_commands.command_scope(|mut commands| {
                        for &(entity1, entity2) in chunks {
                            if let Some(contacts) = self.handle_entity_pair::<H>(
                                entity1,
                                entity2,
                                delta_secs,
                                hooks,
                                context,
                                &mut commands,
                            ) {
                                new_collisions.push(contacts);
                            }
                        }
                    });

                    new_collisions
                })
                .into_iter()
                .for_each(|new_collisions| {
                    // Add the collisions and constraints from each chunk.
                    self.collisions.extend(new_collisions);
                });
        }
        #[cfg(not(feature = "parallel"))]
        {
            // Compute contacts for this intersection pair and generate
            // contact constraints for them.
            for &(entity1, entity2) in broad_collision_pairs {
                if let Some(contacts) = self.handle_entity_pair::<H>(
                    entity1,
                    entity2,
                    delta_secs,
                    hooks,
                    context,
                    &mut commands,
                ) {
                    self.collisions.insert_collision_pair(contacts);
                }
            }
        }
    }

    /// Returns the [`Contacts`] between `entity1` and `entity2` if they are intersecting
    /// or expected to start intersecting within the next frame. This includes
    /// [speculative collision](dynamics::ccd#speculative-collision).
    #[allow(clippy::too_many_arguments)]
    pub fn handle_entity_pair<H: CollisionHooks>(
        &self,
        entity1: Entity,
        entity2: Entity,
        delta_secs: Scalar,
        hooks: &SystemParamItem<H>,
        context: &SystemParamItem<C::Context>,
        commands: &mut Commands,
    ) -> Option<Contacts>
    where
        for<'w, 's> SystemParamItem<'w, 's, H>: CollisionHooks,
    {
        let Ok([collider1, collider2]) = self.collider_query.get_many([entity1, entity2]) else {
            return None;
        };

        let body1_bundle = collider1
            .rigid_body
            .and_then(|&ColliderOf { rigid_body }| self.body_query.get(rigid_body).ok());
        let body2_bundle = collider2
            .rigid_body
            .and_then(|&ColliderOf { rigid_body }| self.body_query.get(rigid_body).ok());

        // The rigid body's collision margin and speculative margin will be used
        // if the collider doesn't have them specified.
        let (mut lin_vel1, rb_friction1, rb_collision_margin1, rb_speculative_margin1) =
            body1_bundle
                .as_ref()
                .map(|(body, collision_margin, speculative_margin)| {
                    (
                        body.linear_velocity.0,
                        body.friction,
                        *collision_margin,
                        *speculative_margin,
                    )
                })
                .unwrap_or_default();
        let (mut lin_vel2, rb_friction2, rb_collision_margin2, rb_speculative_margin2) =
            body2_bundle
                .as_ref()
                .map(|(body, collision_margin, speculative_margin)| {
                    (
                        body.linear_velocity.0,
                        body.friction,
                        *collision_margin,
                        *speculative_margin,
                    )
                })
                .unwrap_or_default();

        // Get combined friction and restitution coefficients of the colliders
        // or the bodies they are attached to. Fall back to the global defaults.
        let friction = collider1
            .friction
            .or(rb_friction1)
            .copied()
            .unwrap_or(self.default_friction.0)
            .combine(
                collider2
                    .friction
                    .or(rb_friction2)
                    .copied()
                    .unwrap_or(self.default_friction.0),
            )
            .dynamic_coefficient;
        let restitution = collider1
            .restitution
            .copied()
            .unwrap_or(self.default_restitution.0)
            .combine(
                collider2
                    .restitution
                    .copied()
                    .unwrap_or(self.default_restitution.0),
            )
            .coefficient;

        // Use the collider's own collision margin if specified, and fall back to the body's
        // collision margin.
        //
        // The collision margin adds artificial thickness to colliders for performance
        // and stability. See the `CollisionMargin` documentation for more details.
        let collision_margin1 = collider1
            .collision_margin
            .or(rb_collision_margin1)
            .map_or(0.0, |margin| margin.0);
        let collision_margin2 = collider2
            .collision_margin
            .or(rb_collision_margin2)
            .map_or(0.0, |margin| margin.0);
        let collision_margin_sum = collision_margin1 + collision_margin2;

        // Use the collider's own speculative margin if specified, and fall back to the body's
        // speculative margin.
        //
        // The speculative margin is used to predict contacts that might happen during the frame.
        // This is used for speculative collision. See the CCD and `SpeculativeMargin` documentation
        // for more details.
        let speculative_margin1 = collider1
            .speculative_margin
            .map_or(rb_speculative_margin1.map(|margin| margin.0), |margin| {
                Some(margin.0)
            });
        let speculative_margin2 = collider2
            .speculative_margin
            .map_or(rb_speculative_margin2.map(|margin| margin.0), |margin| {
                Some(margin.0)
            });

        // Compute the effective speculative margin, clamping it based on velocities and the maximum bound.
        let effective_speculative_margin = {
            let speculative_margin1 =
                speculative_margin1.unwrap_or(*self.default_speculative_margin);
            let speculative_margin2 =
                speculative_margin2.unwrap_or(*self.default_speculative_margin);
            let inv_delta_secs = delta_secs.recip();

            // Clamp velocities to the maximum speculative margins.
            if speculative_margin1 < Scalar::MAX {
                lin_vel1 = lin_vel1.clamp_length_max(speculative_margin1 * inv_delta_secs);
            }
            if speculative_margin2 < Scalar::MAX {
                lin_vel2 = lin_vel2.clamp_length_max(speculative_margin2 * inv_delta_secs);
            }

            // TODO: Check if AABBs intersect?

            // Compute the effective margin based on how much the bodies
            // are expected to move relative to each other.
            delta_secs * (lin_vel1 - lin_vel2).length()
        };

        // The maximum distance at which contacts are detected.
        // At least as large as the contact tolerance.
        let max_contact_distance =
            effective_speculative_margin.max(*self.contact_tolerance) + collision_margin_sum;

        self.compute_contact_pair::<H>(
            context,
            entity1,
            entity2,
            &collider1,
            &collider2,
            friction,
            restitution,
            max_contact_distance,
            hooks,
            commands,
        )
    }

    /// Computes contacts between `collider1` and `collider2`.
    /// Returns `None` if no contacts are found.
    ///
    /// The given `max_distance` determines the maximum distance for a contact
    /// to be detected. A value greater than zero means that contacts are generated
    /// based on the closest points even if the shapes are separated.
    #[allow(clippy::type_complexity, clippy::too_many_arguments)]
    pub fn compute_contact_pair<H: CollisionHooks>(
        &self,
        collider_context: &<C::Context as SystemParam>::Item<'_, '_>,
        entity1: Entity,
        entity2: Entity,
        collider1: &ColliderQueryItem<C>,
        collider2: &ColliderQueryItem<C>,
        friction: Scalar,
        restitution: Scalar,
        max_distance: Scalar,
        hooks: &H::Item<'_, '_>,
        commands: &mut Commands,
    ) -> Option<Contacts>
    where
        for<'w, 's> SystemParamItem<'w, 's, H>: CollisionHooks,
    {
        let position1 = collider1.current_position();
        let position2 = collider2.current_position();

        let context = ContactManifoldContext::new(entity1, entity2, collider_context);
        // TODO: It'd be good to persist the manifolds and let Parry match contacts.
        //       This isn't currently done because it requires using Parry's contact manifold type.
        // Compute the contact manifolds using the effective speculative margin.
        let mut manifolds = collider1.shape.contact_manifolds_with_context(
            collider2.shape,
            position1,
            *collider1.rotation,
            position2,
            *collider2.rotation,
            max_distance,
            context,
        );

        if manifolds.is_empty() {
            return None;
        }

        // Set the initial surface properties.
        // TODO: This could be done in `contact_manifolds` to avoid the extra iteration.
        manifolds.iter_mut().for_each(|manifold| {
            manifold.friction = friction;
            manifold.restitution = restitution;
            #[cfg(feature = "2d")]
            {
                manifold.tangent_speed = 0.0;
            }
            #[cfg(feature = "3d")]
            {
                manifold.tangent_velocity = Vector::ZERO;
            }
        });

        // Get the previous contacts if there are any.
        let previous_contacts = if collider1.entity < collider2.entity {
            self.collisions
                .get_internal()
                .get(&(collider1.entity, collider2.entity))
        } else {
            self.collisions
                .get_internal()
                .get(&(collider2.entity, collider1.entity))
        };

        let mut contacts = Contacts {
            entity1: collider1.entity,
            entity2: collider2.entity,
            body_entity1: collider1
                .rigid_body
                .map(|&ColliderOf { rigid_body }| rigid_body),
            body_entity2: collider2
                .rigid_body
                .map(|&ColliderOf { rigid_body }| rigid_body),
            during_current_frame: true,
            during_previous_frame: previous_contacts.is_some_and(|c| c.during_previous_frame),
            manifolds,
            is_sensor: collider1.is_sensor
                || collider2.is_sensor
                || !collider1.is_rb
                || !collider2.is_rb,
        };

        let active_hooks = collider1.active_hooks().union(collider2.active_hooks());
        if active_hooks.contains(ActiveCollisionHooks::MODIFY_CONTACTS) {
            let keep_contacts = hooks.modify_contacts(&mut contacts, commands);
            if !keep_contacts {
                return None;
            }
        }

        if contacts.manifolds.is_empty() {
            return None;
        }

        // Match contacts and copy previous contact impulses for warm starting the solver.
        // TODO: This condition is pretty arbitrary, mainly to skip dense trimeshes.
        //       If we let Parry handle contact matching, this wouldn't be needed.
        if contacts.manifolds.len() <= 4 && self.config.match_contacts {
            if let Some(previous_contacts) = previous_contacts {
                // TODO: Cache this?
                let distance_threshold = 0.1 * self.length_unit.0;

                for manifold in contacts.manifolds.iter_mut() {
                    for previous_manifold in previous_contacts.manifolds.iter() {
                        manifold.match_contacts(&previous_manifold.points, distance_threshold);
                    }
                }
            }
        }

        Some(contacts)
    }

    /// Generates [`ContactConstraint`]s for the given bodies and their corresponding colliders
    /// based on the given `contacts`. The constraints are added to the `constraints` vector.
    ///
    /// The `collision_margin` can be used to add artificial thickness to the colliders,
    /// which can improve performance and stability in some cases. See [`CollisionMargin`]
    /// for more details.
    ///
    /// The `contact_softness` is used to tune the damping and stiffness of the contact constraints.
    #[allow(clippy::too_many_arguments)]
    pub fn generate_constraints(
        &self,
        contacts: &Contacts,
        constraints: &mut Vec<ContactConstraint>,
        body1: &RigidBodyQueryReadOnlyItem,
        body2: &RigidBodyQueryReadOnlyItem,
        collider1: &ColliderQueryItem<C>,
        collider2: &ColliderQueryItem<C>,
        collision_margin: impl Into<CollisionMargin> + Copy,
        contact_softness: ContactSoftnessCoefficients,
        delta_secs: Scalar,
    ) {
        let inactive1 = body1.rb.is_static() || body1.is_sleeping;
        let inactive2 = body2.rb.is_static() || body2.is_sleeping;

        // No collision response if both bodies are static or sleeping
        // or if either of the colliders is a sensor collider.
        if (inactive1 && inactive2)
            || (collider1.is_sensor || body1.is_sensor)
            || (collider2.is_sensor || body2.is_sensor)
        {
            return;
        }

        // When an active body collides with a sleeping body, wake up the sleeping body.
        self.parallel_commands.command_scope(|mut commands| {
            if body1.is_sleeping {
                commands.queue(WakeUpBody(body1.entity));
            } else if body2.is_sleeping {
                commands.queue(WakeUpBody(body2.entity));
            }
        });

        let contact_softness = if !body1.rb.is_dynamic() || !body2.rb.is_dynamic() {
            contact_softness.non_dynamic
        } else {
            contact_softness.dynamic
        };

        // Generate contact constraints for each contact.
        for (i, contact_manifold) in contacts.manifolds.iter().enumerate() {
            let constraint = ContactConstraint::generate(
                i,
                contact_manifold,
                body1,
                body2,
                collider1.entity,
                collider2.entity,
                collider1.transform.copied(),
                collider2.transform.copied(),
                collision_margin,
                // TODO: Shouldn't this be the effective speculative margin?
                *self.default_speculative_margin,
                contact_manifold.friction,
                contact_manifold.restitution,
                #[cfg(feature = "2d")]
                contact_manifold.tangent_speed,
                #[cfg(feature = "3d")]
                contact_manifold.tangent_velocity,
                contact_softness,
                self.config.match_contacts,
                delta_secs,
            );

            if !constraint.points.is_empty() {
                constraints.push(constraint);
            }
        }
    }
}

fn remove_ended_collisions(mut collisions: ResMut<Collisions>) {
    collisions.retain(|contacts| contacts.during_current_frame);
}

// TODO: The collision state handling feels a bit confusing and error-prone.
//       Ideally, the narrow phase wouldn't need to handle it at all, or it would at least be simpler.
/// Resets collision states like `during_current_frame` and `during_previous_frame`.
pub fn reset_collision_states(
    mut collisions: ResMut<Collisions>,
    query: Query<(Option<&RigidBody>, Has<Sleeping>)>,
) {
    for contacts in collisions.get_internal_mut().values_mut() {
        if let Ok([(rb1, sleeping1), (rb2, sleeping2)]) = query.get_many([
            contacts.body_entity1.unwrap_or(contacts.entity1),
            contacts.body_entity2.unwrap_or(contacts.entity2),
        ]) {
            let active1 = !rb1.is_some_and(|rb| rb.is_static()) && !sleeping1;
            let active2 = !rb2.is_some_and(|rb| rb.is_static()) && !sleeping2;

            // Reset collision states if either of the bodies is active (not static or sleeping)
            // Otherwise, the bodies are still in contact.
            if active1 || active2 {
                contacts.during_previous_frame = true;
                contacts.during_current_frame = false;
            } else {
                contacts.during_previous_frame = true;
                contacts.during_current_frame = true;
            }
        } else {
            // One of the entities does not exist, so the collision has ended.
            contacts.during_previous_frame = true;
            contacts.during_current_frame = false;
        }
    }
}

/// Runs the [`PostProcessCollisions`] schedule.
fn run_post_process_collisions_schedule(world: &mut World) {
    trace!("running PostProcessCollisions");
    world.run_schedule(PostProcessCollisions);
}
