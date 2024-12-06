//! Manages and solves contacts, joints, and other constraints.
//!
//! See [`SolverPlugin`].

pub mod contact;
pub mod joints;
pub mod softness_parameters;
pub mod xpbd;

use crate::prelude::*;
use bevy::prelude::*;

use self::{
    contact::ContactConstraint,
    dynamics::integrator::IntegrationSet,
    softness_parameters::{SoftnessCoefficients, SoftnessParameters},
};

/// Manages and solves contacts, joints, and other constraints.
///
/// Note that the [`ContactConstraints`] are currently generated by tbe [`NarrowPhasePlugin`].
///
/// # Implementation
///
/// The solver primarily uses TGS Soft, an impulse-based solver with substepping and [soft constraints](softness_parameters).
/// Warm starting is used to improve convergence, along with a relaxation pass to reduce overshooting.
///
/// [Speculative collision](dynamics::ccd#speculative-collision) is used by default to prevent tunneling.
/// Optional [sweep-based Continuous Collision Detection (CCD)](dynamics::ccd#swept-ccd) is handled by the [`CcdPlugin`].
///
/// [Joints](joints) and user constraints are currently solved using [Extended Position-Based Dynamics (XPBD)](xpbd).
/// In the future, they may transition to an impulse-based approach as well.
///
/// # Steps
///
/// Below are the main steps of the `SolverPlugin`.
///
/// 1. [Generate and prepare constraints](collision::narrow_phase::NarrowPhaseSet::GenerateConstraints)
/// 2. Substepping loop (runs the [`SubstepSchedule`] [`SubstepCount`] times)
///     1. [Integrate velocities](IntegrationSet::Velocity)
///     2. [Warm start](SubstepSolverSet::WarmStart)
///     3. [Solve constraints with bias](SubstepSolverSet::SolveConstraints)
///     4. [Integrate positions](IntegrationSet::Position)
///     5. [Solve constraints without bias to relax velocities](SubstepSolverSet::Relax)
///     6. [Solve XPBD constraints (joints)](SubstepSolverSet::SolveXpbdConstraints)
///     7. [Solve user-defined constraints](SubstepSolverSet::SolveUserConstraints)
///     8. [Update velocities after XPBD constraint solving.](SubstepSolverSet::XpbdVelocityProjection)
/// 3. [Apply restitution](SolverSet::Restitution)
/// 4. [Finalize positions by applying](SolverSet::ApplyTranslation) [`AccumulatedTranslation`]
/// 5. [Store contact impulses for next frame's warm starting](SolverSet::StoreContactImpulses)
pub struct SolverPlugin {
    length_unit: Scalar,
}

impl Default for SolverPlugin {
    fn default() -> Self {
        Self::new_with_length_unit(1.0)
    }
}

impl SolverPlugin {
    /// Creates a [`SolverPlugin`] with the given approximate dimensions of most objects.
    ///
    /// The length unit will be used for initializing the [`PhysicsLengthUnit`]
    /// resource unless it already exists.
    pub fn new_with_length_unit(unit: Scalar) -> Self {
        Self { length_unit: unit }
    }
}

impl Plugin for SolverPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<SolverConfig>()
            .init_resource::<ContactSoftnessCoefficients>()
            .init_resource::<ContactConstraints>();

        if !app.world().contains_resource::<PhysicsLengthUnit>() {
            app.insert_resource(PhysicsLengthUnit(self.length_unit));
        }

        // Get the `PhysicsSchedule`, and panic if it doesn't exist.
        let physics = app
            .get_schedule_mut(PhysicsSchedule)
            .expect("add PhysicsSchedule first");

        physics.add_systems(update_contact_softness.before(PhysicsStepSet::NarrowPhase));

        // See `SolverSet` for what each system set is responsible for.
        physics.configure_sets(
            (
                SolverSet::PreSubstep,
                SolverSet::Substep,
                SolverSet::PostSubstep,
                SolverSet::Restitution,
                SolverSet::ApplyTranslation,
                SolverSet::StoreContactImpulses,
            )
                .chain()
                .in_set(PhysicsStepSet::Solver),
        );

        // Update previous rotations before the substepping loop.
        physics.add_systems(
            (|mut query: Query<(&Rotation, &mut PreviousRotation)>| {
                for (rot, mut prev_rot) in &mut query {
                    prev_rot.0 = *rot;
                }
            })
            .in_set(SolverSet::PreSubstep),
        );

        // Finalize the positions of bodies by applying the `AccumulatedTranslation`.
        // This runs after the substepping loop.
        physics.add_systems(
            apply_translation
                .chain()
                .in_set(SolverSet::ApplyTranslation),
        );

        // Apply restitution.
        physics.add_systems(solve_restitution.in_set(SolverSet::Restitution));

        // Store the current contact impulses for the next frame's warm starting.
        physics.add_systems(store_contact_impulses.in_set(SolverSet::StoreContactImpulses));

        // Get the `SubstepSchedule`, and panic if it doesn't exist.
        let substeps = app
            .get_schedule_mut(SubstepSchedule)
            .expect("add SubstepSchedule first");

        // See `SolverSet` for what each system set is responsible for.
        substeps.configure_sets(
            (
                IntegrationSet::Velocity,
                SubstepSolverSet::WarmStart,
                SubstepSolverSet::SolveConstraints,
                IntegrationSet::Position,
                SubstepSolverSet::Relax,
                SubstepSolverSet::SolveXpbdConstraints,
                SubstepSolverSet::SolveUserConstraints,
                SubstepSolverSet::XpbdVelocityProjection,
            )
                .chain(),
        );

        // Warm start the impulses.
        // This applies the impulses stored from the previous substep,
        // which improves convergence.
        substeps.add_systems(warm_start.in_set(SubstepSolverSet::WarmStart));

        // Solve velocities using a position bias.
        substeps.add_systems(
            (
                |mut bodies: Query<RigidBodyQuery>,
                 mut constraints: ResMut<ContactConstraints>,
                 solver_config: Res<SolverConfig>,
                 length_unit: Res<PhysicsLengthUnit>,
                 time: Res<Time>| {
                    solve_contacts(
                        &mut bodies,
                        &mut constraints.0,
                        time.delta_seconds_adjusted(),
                        1,
                        true,
                        solver_config.max_overlap_solve_speed * length_unit.0,
                    );
                },
            )
                .in_set(SubstepSolverSet::SolveConstraints),
        );

        // Relax biased velocities and impulses.
        // This reduces overshooting caused by warm starting.
        substeps.add_systems(
            (
                |mut bodies: Query<RigidBodyQuery>,
                 mut constraints: ResMut<ContactConstraints>,
                 solver_config: Res<SolverConfig>,
                 length_unit: Res<PhysicsLengthUnit>,
                 time: Res<Time>| {
                    solve_contacts(
                        &mut bodies,
                        &mut constraints.0,
                        time.delta_seconds_adjusted(),
                        1,
                        false,
                        solver_config.max_overlap_solve_speed * length_unit.0,
                    );
                },
            )
                .in_set(SubstepSolverSet::Relax),
        );

        // Solve joints with XPBD.
        substeps.add_systems(
            (
                |mut query: Query<(
                    &AccumulatedTranslation,
                    &mut PreSolveAccumulatedTranslation,
                    &Rotation,
                    &mut PreSolveRotation,
                )>| {
                    for (translation, mut pre_solve_translation, rotation, mut previous_rotation) in
                        &mut query
                    {
                        pre_solve_translation.0 = translation.0;
                        previous_rotation.0 = *rotation;
                    }
                },
                xpbd::solve_constraint::<FixedJoint, 2>,
                xpbd::solve_constraint::<RevoluteJoint, 2>,
                #[cfg(feature = "3d")]
                xpbd::solve_constraint::<SphericalJoint, 2>,
                xpbd::solve_constraint::<PrismaticJoint, 2>,
                xpbd::solve_constraint::<DistanceJoint, 2>,
            )
                .chain()
                .in_set(SubstepSolverSet::SolveXpbdConstraints),
        );

        // Perform XPBD velocity updates after constraint solving.
        substeps.add_systems(
            (
                xpbd::project_linear_velocity,
                xpbd::project_angular_velocity,
                joint_damping::<FixedJoint>,
                joint_damping::<RevoluteJoint>,
                #[cfg(feature = "3d")]
                joint_damping::<SphericalJoint>,
                joint_damping::<PrismaticJoint>,
                joint_damping::<DistanceJoint>,
            )
                .chain()
                .in_set(SubstepSolverSet::XpbdVelocityProjection),
        );
    }
}

// TODO: Where should this type be and which plugin should initialize it?
/// A units-per-meter scaling factor that adjusts the engine's internal properties
/// to the scale of the world.
///
/// For example, a 2D game might use pixels as units and have an average object size
/// of around 100 pixels. By setting the length unit to `100.0`, the physics engine
/// will interpret 100 pixels as 1 meter for internal thresholds, improving stability.
///
/// Note that this is *not* used to scale forces or any other user-facing inputs or outputs.
/// Instead, the value is only used to scale some internal length-based tolerances, such as
/// [`SleepingThreshold::linear`] and [`NarrowPhaseConfig::default_speculative_margin`],
/// as well as the scale used for [debug rendering](PhysicsDebugPlugin).
///
/// Choosing the appropriate length unit can help improve stability and robustness.
///
/// Default: `1.0`
///
/// # Example
///
/// The [`PhysicsLengthUnit`] can be inserted as a resource like normal,
/// but it can also be specified through the [`PhysicsPlugins`] plugin group.
///
/// ```no_run
/// # #[cfg(feature = "2d")]
/// use avian2d::prelude::*;
/// use bevy::prelude::*;
///
/// # #[cfg(feature = "2d")]
/// fn main() {
///     App::new()
///         .add_plugins((
///             DefaultPlugins,
///             // A 2D game with 100 pixels per meter
///             PhysicsPlugins::default().with_length_unit(100.0),
///         ))
///         .run();
/// }
/// # #[cfg(not(feature = "2d"))]
/// # fn main() {} // Doc test needs main
/// ```
#[derive(Resource, Clone, Debug, Deref, DerefMut, PartialEq, Reflect)]
#[reflect(Resource)]
pub struct PhysicsLengthUnit(pub Scalar);

impl Default for PhysicsLengthUnit {
    fn default() -> Self {
        Self(1.0)
    }
}

/// System sets for the constraint solver.
///
/// # Steps
///
/// Below is the core solver loop.
///
/// 1. Generate and prepare constraints ([`NarrowPhaseSet::GenerateConstraints`](collision::narrow_phase::NarrowPhaseSet::GenerateConstraints))
/// 2. Substepping loop (runs the [`SubstepSchedule`] [`SubstepCount`] times; see [`SolverSet::Substep`])
/// 3. Apply restitution ([`SolverSet::Restitution`])
/// 4. Finalize positions by applying [`AccumulatedTranslation`] ([`SolverSet::ApplyTranslation`])
/// 5. Store contact impulses for next frame's warm starting ([`SolverSet::StoreContactImpulses`])
#[derive(SystemSet, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SolverSet {
    /// A system set for systems running just before the substepping loop.
    PreSubstep,
    /// A system set for the substepping loop.
    Substep,
    /// A system set for systems running just after the substepping loop.
    PostSubstep,
    /// Applies [restitution](Restitution) for bodies after solving overlap.
    Restitution,
    /// Finalizes the positions of bodies by applying the [`AccumulatedTranslation`].
    ///
    /// Constraints don't modify the positions of bodies directly and instead adds
    /// to this translation to improve numerical stability when bodies are far from the world origin.
    ApplyTranslation,
    /// Copies contact impulses from [`ContactConstraints`] to the contacts in [`Collisions`].
    /// They will be used for [warm starting](SubstepSolverSet::WarmStart) the next frame or substep.
    StoreContactImpulses,
}

/// System sets for the substepped part of the constraint solver.
///
/// # Steps
///
/// 1. Integrate velocity ([`IntegrationSet::Velocity`])
/// 2. Warm start ([`SubstepSolverSet::WarmStart`])
/// 3. Solve constraints with bias ([`SubstepSolverSet::SolveConstraints`])
/// 4. Integrate positions ([`IntegrationSet::Position`])
/// 5. Solve constraints without bias to relax velocities ([`SubstepSolverSet::Relax`])
/// 6. Solve joints using Extended Position-Based Dynamics (XPBD). ([`SubstepSolverSet::SolveXpbdConstraints`])
/// 7. Solve user-defined constraints. ([`SubstepSolverSet::SolveUserConstraints`])
/// 8. Update velocities after XPBD constraint solving. ([`SubstepSolverSet::XpbdVelocityProjection`])
#[derive(SystemSet, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SubstepSolverSet {
    /// Warm starts the solver by applying the impulses from the previous frame or substep.
    ///
    /// This significantly improves convergence, but by itself can lead to overshooting.
    /// Overshooting is reduced by [relaxing](SubstepSolverSet::Relax) the biased velocities
    /// by running the solver a second time *without* bias.
    WarmStart,
    /// Solves velocity constraints using a position bias that boosts the response
    /// to account for the constraint error.
    SolveConstraints,
    /// Solves velocity constraints without a position bias to relax the biased velocities
    /// and impulses. This reduces overshooting caused by [warm starting](SubstepSolverSet::WarmStart).
    Relax,
    /// Solves joints using Extended Position-Based Dynamics (XPBD).
    SolveXpbdConstraints,
    /// A system set for user constraints.
    SolveUserConstraints,
    /// Performs velocity updates after XPBD constraint solving.
    XpbdVelocityProjection,
}

/// Configuration parameters for the constraint solver that handles
/// things like contacts and joints.
///
/// These are tuned to give good results for most applications, but can
/// be configured if more control over the simulation behavior is needed.
#[derive(Resource, Clone, Debug, PartialEq, Reflect)]
#[reflect(Resource)]
pub struct SolverConfig {
    /// The damping ratio used for contact stabilization.
    ///
    /// Lower values make contacts more compliant or "springy",
    /// allowing more visible penetration before overlap has been
    /// resolved and the contact has been stabilized.
    ///
    /// Consider using a higher damping ratio if contacts seem too soft.
    /// Note that making the value too large can cause instability.
    ///
    /// Default: `10.0`.
    pub contact_damping_ratio: Scalar,

    /// Scales the frequency used for contacts. A higher frequency
    /// makes contact responses faster and reduces visible springiness,
    /// but can hurt stability.
    ///
    /// The solver computes the frequency using the time step and substep count,
    /// and limits the maximum frequency to be at most half of the time step due to
    /// [Nyquist's theorem](https://en.wikipedia.org/wiki/Nyquist%E2%80%93Shannon_sampling_theorem).
    /// This factor scales the resulting frequency, which can lead to unstable behavior
    /// if the factor is too large.
    ///
    /// Default: `1.5`
    pub contact_frequency_factor: Scalar,

    /// The maximum speed at which overlapping bodies are pushed apart by the solver.
    ///
    /// With a small value, overlap is resolved gently and gradually, while large values
    /// can result in more snappy behavior.
    ///
    /// This is implicitly scaled by the [`PhysicsLengthUnit`].
    ///
    /// Default: `4.0`
    pub max_overlap_solve_speed: Scalar,

    /// The coefficient in the `[0, 1]` range applied to
    /// [warm start](SubstepSolverSet::WarmStart) impulses.
    ///
    /// Warm starting uses the impulses from the previous frame as the initial
    /// solution for the current frame. This helps the solver reach the desired
    /// state much faster, meaning that *convergence* is improved.
    ///
    /// The coefficient should typically be set to `1.0`.
    ///
    /// Default: `1.0`
    pub warm_start_coefficient: Scalar,

    /// The minimum speed along the contact normal in units per second
    /// for [restitution](Restitution) to be applied.
    ///
    /// An appropriate threshold should typically be small enough that objects
    /// keep bouncing until the bounces are effectively unnoticeable,
    /// but large enough that restitution is not applied unnecessarily,
    /// improving performance and stability.
    ///
    /// This is implicitly scaled by the [`PhysicsLengthUnit`].
    ///
    /// Default: `1.0`
    pub restitution_threshold: Scalar,

    /// The number of iterations used for applying [restitution](Restitution).
    ///
    /// A higher number of iterations can result in more accurate bounces,
    /// but it only makes a difference when there are more than one contact point.
    ///
    /// For example, with just one iteration, a cube falling flat on the ground
    /// might bounce and rotate to one side, because the impulses are applied
    /// to the corners sequentially, and some of the impulses are likely to be larger
    /// than the others. With multiple iterations, the impulses are applied more evenly.
    ///
    /// Default: `1`
    pub restitution_iterations: usize,
}

impl Default for SolverConfig {
    fn default() -> Self {
        Self {
            contact_damping_ratio: 10.0,
            contact_frequency_factor: 1.5,
            max_overlap_solve_speed: 4.0,
            warm_start_coefficient: 1.0,
            restitution_threshold: 1.0,
            restitution_iterations: 1,
        }
    }
}

/// The [`SoftnessCoefficients`] used for contacts.
///
/// **Note**: This resource is updated automatically and not intended to be modified manually.
/// Use the [`SolverConfig`] resource instead for tuning contact behavior.
#[derive(Resource, Clone, Copy, PartialEq, Reflect)]
#[reflect(Resource)]
pub struct ContactSoftnessCoefficients {
    /// The [`SoftnessCoefficients`] used for contacts against dynamic bodies.
    pub dynamic: SoftnessCoefficients,
    /// The [`SoftnessCoefficients`] used for contacts against static or kinematic bodies.
    pub non_dynamic: SoftnessCoefficients,
}

impl Default for ContactSoftnessCoefficients {
    fn default() -> Self {
        Self {
            dynamic: SoftnessParameters::new(10.0, 30.0).compute_coefficients(1.0 / 60.0),
            non_dynamic: SoftnessParameters::new(10.0, 60.0).compute_coefficients(1.0 / 60.0),
        }
    }
}

fn update_contact_softness(
    mut coefficients: ResMut<ContactSoftnessCoefficients>,
    solver_config: Res<SolverConfig>,
    physics_time: Res<Time<Physics>>,
    substep_time: Res<Time<Substeps>>,
) {
    if solver_config.is_changed() || physics_time.is_changed() || substep_time.is_changed() {
        let dt = physics_time.delta_secs_f64() as Scalar;
        let h = substep_time.delta_secs_f64() as Scalar;

        // The contact frequency should at most be half of the time step due to Nyquist's theorem.
        // https://en.wikipedia.org/wiki/Nyquist%E2%80%93Shannon_sampling_theorem
        let max_hz = 1.0 / (dt * 2.0);
        let hz = solver_config.contact_frequency_factor * max_hz.min(0.25 / h);

        coefficients.dynamic = SoftnessParameters::new(solver_config.contact_damping_ratio, hz)
            .compute_coefficients(h);

        // TODO: Perhaps the non-dynamic softness should be configurable separately.
        // Make contacts against static and kinematic bodies stiffer to avoid clipping through the environment.
        coefficients.non_dynamic =
            SoftnessParameters::new(solver_config.contact_damping_ratio, 2.0 * hz)
                .compute_coefficients(h);
    }
}

/// A resource that stores the contact constraints.
#[derive(Resource, Default, Deref, DerefMut)]
pub struct ContactConstraints(pub Vec<ContactConstraint>);

/// Warm starts the solver by applying the impulses from the previous frame or substep.
///
/// See [`SubstepSolverSet::WarmStart`] for more information.
fn warm_start(
    mut bodies: Query<RigidBodyQuery>,
    mut constraints: ResMut<ContactConstraints>,
    solver_config: Res<SolverConfig>,
) {
    for constraint in constraints.iter_mut() {
        debug_assert!(!constraint.points.is_empty());

        let Ok([mut body1, mut body2]) =
            bodies.get_many_mut([constraint.entity1, constraint.entity2])
        else {
            continue;
        };

        let normal = constraint.normal;
        let tangent_directions =
            constraint.tangent_directions(body1.linear_velocity.0, body2.linear_velocity.0);

        constraint.warm_start(
            &mut body1,
            &mut body2,
            normal,
            tangent_directions,
            solver_config.warm_start_coefficient,
        );
    }
}

/// Solves contacts by iterating through the given contact constraints
/// and applying impulses to colliding rigid bodies.
///
/// This solve is done `iterations` times. With a substepped solver,
/// `iterations` should typically be `1`, as substeps will handle the iteration.
///
/// If `use_bias` is `true`, the impulses will be boosted to account for overlap.
/// The solver should often be run twice per frame or substep: first with the bias,
/// and then without it to *relax* the velocities and reduce overshooting caused by
/// [warm starting](SubstepSolverSet::WarmStart).
///
/// See [`SubstepSolverSet::SolveConstraints`] and [`SubstepSolverSet::Relax`] for more information.
#[allow(clippy::too_many_arguments)]
#[allow(clippy::type_complexity)]
fn solve_contacts(
    bodies: &mut Query<RigidBodyQuery>,
    constraints: &mut [ContactConstraint],
    delta_secs: Scalar,
    iterations: usize,
    use_bias: bool,
    max_overlap_solve_speed: Scalar,
) {
    for _ in 0..iterations {
        for constraint in &mut *constraints {
            let Ok([mut body1, mut body2]) =
                bodies.get_many_mut([constraint.entity1, constraint.entity2])
            else {
                continue;
            };

            constraint.solve(
                &mut body1,
                &mut body2,
                delta_secs,
                use_bias,
                max_overlap_solve_speed,
            );
        }
    }
}

/// Iterates through contact constraints and applies impulses to account for [`Restitution`].
///
/// Note that restitution with TGS Soft and speculative contacts may not be perfectly accurate.
/// This is a tradeoff, but cheap CCD is often more important than perfect restitution.
///
/// The number of iterations can be increased with [`SolverConfig::restitution_iterations`]
/// to apply restitution for multiple contact points more evenly.
#[allow(clippy::too_many_arguments)]
#[allow(clippy::type_complexity)]
fn solve_restitution(
    mut bodies: Query<RigidBodyQuery>,
    mut constraints: ResMut<ContactConstraints>,
    solver_config: Res<SolverConfig>,
    length_unit: Res<PhysicsLengthUnit>,
) {
    // The restitution threshold determining the speed required for restitution to be applied.
    let threshold = solver_config.restitution_threshold * length_unit.0;

    for constraint in constraints.iter_mut() {
        let restitution = constraint.restitution.coefficient;

        if restitution == 0.0 {
            continue;
        }

        let Ok([mut body1, mut body2]) =
            bodies.get_many_mut([constraint.entity1, constraint.entity2])
        else {
            continue;
        };

        // Performing multiple iterations can result in more accurate restitution,
        // but only if there are more than one contact point.
        let restitution_iterations = if constraint.points.len() > 1 {
            solver_config.restitution_iterations
        } else {
            1
        };

        for _ in 0..restitution_iterations {
            constraint.apply_restitution(&mut body1, &mut body2, threshold);
        }
    }
}

/// Copies contact impulses from [`ContactConstraints`] to the contacts in [`Collisions`].
/// They will be used for [warm starting](SubstepSolverSet::WarmStart).
fn store_contact_impulses(
    constraints: Res<ContactConstraints>,
    mut collisions: ResMut<Collisions>,
) {
    for constraint in constraints.iter() {
        let Some(contacts) =
            collisions.get_mut(constraint.collider_entity1, constraint.collider_entity2)
        else {
            continue;
        };

        let manifold = &mut contacts.manifolds[constraint.manifold_index];

        for (contact, constraint_point) in
            manifold.contacts.iter_mut().zip(constraint.points.iter())
        {
            contact.normal_impulse = constraint_point.normal_part.impulse;
            contact.tangent_impulse = constraint_point
                .tangent_part
                .as_ref()
                .map_or(default(), |part| part.impulse);
        }
    }
}

/// Finalizes the positions of bodies by applying the [`AccumulatedTranslation`].
#[allow(clippy::type_complexity)]
fn apply_translation(
    mut bodies: Query<
        (
            &RigidBody,
            &mut Position,
            &Rotation,
            &PreviousRotation,
            &mut AccumulatedTranslation,
            &ComputedCenterOfMass,
        ),
        Changed<AccumulatedTranslation>,
    >,
) {
    for (rb, mut pos, rot, prev_rot, mut translation, center_of_mass) in &mut bodies {
        if rb.is_static() {
            continue;
        }

        // We must also account for the translation caused by rotations around the center of mass,
        // as it may be offset from `Position`.
        pos.0 += crate::utils::get_pos_translation(&translation, prev_rot, rot, center_of_mass);
        translation.0 = Vector::ZERO;
    }
}

/// Applies velocity corrections caused by joint damping.
#[allow(clippy::type_complexity)]
pub fn joint_damping<T: Joint>(
    mut bodies: Query<
        (
            &RigidBody,
            &mut LinearVelocity,
            &mut AngularVelocity,
            &ComputedMass,
            Option<&Dominance>,
        ),
        Without<Sleeping>,
    >,
    joints: Query<&T, Without<RigidBody>>,
    time: Res<Time>,
) {
    let delta_secs = time.delta_seconds_adjusted();

    for joint in &joints {
        if let Ok(
            [(rb1, mut lin_vel1, mut ang_vel1, mass1, dominance1), (rb2, mut lin_vel2, mut ang_vel2, mass2, dominance2)],
        ) = bodies.get_many_mut(joint.entities())
        {
            let delta_omega =
                (ang_vel2.0 - ang_vel1.0) * (joint.damping_angular() * delta_secs).min(1.0);

            if rb1.is_dynamic() {
                ang_vel1.0 += delta_omega;
            }
            if rb2.is_dynamic() {
                ang_vel2.0 -= delta_omega;
            }

            let delta_v =
                (lin_vel2.0 - lin_vel1.0) * (joint.damping_linear() * delta_secs).min(1.0);

            let w1 = if rb1.is_dynamic() {
                mass1.inverse()
            } else {
                0.0
            };
            let w2 = if rb2.is_dynamic() {
                mass2.inverse()
            } else {
                0.0
            };

            if w1 + w2 <= Scalar::EPSILON {
                continue;
            }

            let p = delta_v / (w1 + w2);

            let dominance1 = dominance1.map_or(0, |dominance| dominance.0);
            let dominance2 = dominance2.map_or(0, |dominance| dominance.0);

            if rb1.is_dynamic() && (!rb2.is_dynamic() || dominance1 <= dominance2) {
                lin_vel1.0 += p * mass1.inverse();
            }
            if rb2.is_dynamic() && (!rb1.is_dynamic() || dominance2 <= dominance1) {
                lin_vel2.0 -= p * mass2.inverse();
            }
        }
    }
}
