//! A `bevy_editor_cam` extension that adds the ability to smoothly rotate the camera about its
//! anchor point until it is looking in the specified direction.

use std::{f32::consts::PI, time::Duration};

use bevy_app::prelude::*;
use bevy_ecs::prelude::*;
use bevy_math::{prelude::*, DAffine3, DQuat, DVec3};
use bevy_platform::{collections::HashMap, time::Instant};
use bevy_reflect::prelude::*;
use bevy_transform::prelude::*;
use bevy_window::RequestRedraw;

use crate::prelude::*;

/// See the [module](self) docs.
pub struct LookToPlugin;

impl Plugin for LookToPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<LookTo>()
            .add_event::<LookToTrigger>()
            .add_systems(
                PreUpdate,
                LookTo::update
                    .before(crate::controller::component::EditorCam::update_camera_positions),
            )
            .add_systems(PostUpdate, LookToTrigger::receive) // In PostUpdate so we don't miss users sending this in Update. LookTo::update will catch the changes next frame.
            .register_type::<LookTo>();
    }
}

/// Send this event to rotate the camera about its anchor until it is looking in the given direction
/// with the given up direction. Animation speed is configured with the [`LookTo`] resource.
#[derive(Debug, Event)]
pub struct LookToTrigger {
    /// The new direction to face.
    pub target_facing_direction: Dir3,
    /// The camera's "up" direction when finished moving.
    pub target_up_direction: Dir3,
    /// The camera to update.
    pub camera: Entity,
}

impl LookToTrigger {
    /// Constructs a [`LookToTrigger`] with the up direction automatically selected.
    ///
    /// If the camera is set to [`OrbitConstraint::Fixed`], the fixed up direction will be used, as
    /// long as it is not parallel to the facing direction. If set to [`OrbitConstraint::Free`] or
    /// the facing direction is parallel to the fixed up direction, the up direction will be
    /// automatically selected by choosing the axis that results in the least amount of rotation.
    pub fn auto_snap_up_direction(
        facing: Dir3,
        cam_entity: Entity,
        cam_rotation: &DQuat,
        cam_editor: &EditorCam,
    ) -> Self {
        const EPSILON: f32 = 0.01;
        let constraint = match cam_editor.orbit_constraint {
            OrbitConstraint::Fixed { up, .. } => Some(up),
            OrbitConstraint::Free => None,
        }
        .filter(|up| {
            let angle = facing.angle_between(*up).abs();
            angle > EPSILON && angle < PI - EPSILON
        });

        let up = constraint.unwrap_or_else(|| {
            let current = cam_rotation.as_quat();
            let options = [
                Vec3::X,
                Vec3::NEG_X,
                Vec3::Y,
                Vec3::NEG_Y,
                Vec3::Z,
                Vec3::NEG_Z,
            ];
            *options
                .iter()
                .map(|d| (d, Transform::default().looking_to(*facing, *d).rotation))
                .map(|(d, rot)| (d, rot.angle_between(current).abs()))
                .reduce(|acc, this| if this.1 < acc.1 { this } else { acc })
                .map(|nearest| nearest.0)
                .unwrap_or(&Vec3::Y)
        });

        LookToTrigger {
            target_facing_direction: facing,
            target_up_direction: Dir3::new_unchecked(up.normalize()),
            camera: cam_entity,
        }
    }
}

impl LookToTrigger {
    fn receive(
        mut events: EventReader<Self>,
        mut state: ResMut<LookTo>,
        mut camera_set: ParamSet<(Query<&mut EditorCam>, Query<EntityRef, With<EditorCam>>)>,
        mut redraw: EventWriter<RequestRedraw>,
        read_write: Option<Res<CustomReadWrite>>,
    ) {
        for event in events.read() {
            let camera_refs = camera_set.p1();
            let Some((_, camera_rotation)) = (if let Ok(camera_ref) = camera_refs.get(event.camera)
            {
                if let Some(ref read_write) = read_write {
                    let CustomReadWrite { read_transform, .. } = &**read_write;
                    read_transform(&camera_ref)
                } else {
                    EditorCam::default_read_transform(&camera_ref)
                }
            } else {
                None
            }) else {
                continue;
            };
            let mut cameras = camera_set.p0();
            let Ok(mut controller) = cameras.get_mut(event.camera) else {
                continue;
            };
            redraw.write(RequestRedraw);
            let camera_forward = Dir3::new_unchecked((camera_rotation * DVec3::NEG_Z).as_vec3());
            let camera_up = Dir3::new_unchecked((camera_rotation * DVec3::Y).as_vec3());
            state
                .map
                .entry(event.camera)
                .and_modify(|e| {
                    e.start = Instant::now();
                    e.initial_facing_direction = camera_forward;
                    e.initial_up_direction = camera_up;
                    e.target_facing_direction = event.target_facing_direction;
                    e.target_up_direction = event.target_up_direction;
                    e.complete = false;
                })
                .or_insert(LookToEntry {
                    start: Instant::now(),
                    initial_facing_direction: camera_forward,
                    initial_up_direction: camera_up,
                    target_facing_direction: event.target_facing_direction,
                    target_up_direction: event.target_up_direction,
                    complete: false,
                });

            controller.end_move();
            controller.current_motion = motion::CurrentMotion::Stationary;
        }
    }
}

struct LookToEntry {
    start: Instant,
    initial_facing_direction: Dir3,
    initial_up_direction: Dir3,
    target_facing_direction: Dir3,
    target_up_direction: Dir3,
    complete: bool,
}

/// Stores settings and state for the dolly zoom plugin.
#[derive(Resource, Reflect)]
pub struct LookTo {
    /// The duration of the "look to" transition animation.
    pub animation_duration: Duration,
    /// The cubic curve used to animate the camera during a "look to".
    #[reflect(ignore)]
    pub animation_curve: CubicSegment<Vec2>,
    #[reflect(ignore)]
    map: HashMap<Entity, LookToEntry>,
}

impl Default for LookTo {
    fn default() -> Self {
        Self {
            animation_duration: Duration::from_millis(400),
            animation_curve: CubicSegment::new_bezier_easing((0.25, 0.0), (0.25, 1.0)),
            map: Default::default(),
        }
    }
}

impl LookTo {
    fn update(
        mut state: ResMut<Self>,
        mut camera_set: ParamSet<(
            Query<&mut EditorCam>,
            Query<EntityRef, With<EditorCam>>,
            Query<EntityMut, With<EditorCam>>,
        )>,
        mut redraw: EventWriter<RequestRedraw>,
        read_write: Option<Res<CustomReadWrite>>,
    ) {
        let animation_duration = state.animation_duration;
        let animation_curve = state.animation_curve;
        for (
            camera,
            LookToEntry {
                start,
                initial_facing_direction,
                initial_up_direction,
                target_facing_direction,
                target_up_direction,
                complete,
            },
        ) in state.map.iter_mut()
        {
            let camera_refs = camera_set.p1();
            let Some((mut camera_translation, mut camera_rotation)) =
                (if let Ok(camera_ref) = camera_refs.get(*camera) {
                    if let Some(ref read_write) = read_write {
                        let CustomReadWrite { read_transform, .. } = &**read_write;
                        read_transform(&camera_ref)
                    } else {
                        EditorCam::default_read_transform(&camera_ref)
                    }
                } else {
                    None
                })
            else {
                continue;
            };
            let mut cameras = camera_set.p0();
            let Ok(controller) = cameras.get_mut(*camera) else {
                *complete = true;
                continue;
            };
            let progress_t =
                (start.elapsed().as_secs_f32() / animation_duration.as_secs_f32()).clamp(0.0, 1.0);
            let progress = animation_curve.ease(progress_t);

            let rotate_around = |trans_translation: &mut DVec3,
                                 trans_rotation: &mut DQuat,
                                 point: DVec3,
                                 rotation: DQuat| {
                // Following lines are f64 versions of Transform::rotate_around
                *trans_translation = point + rotation * (*trans_translation - point);
                *trans_rotation = (rotation * *trans_rotation).normalize();
            };

            let anchor_view_space = controller.anchor_view_space().unwrap_or(DVec3::new(
                0.0,
                0.0,
                controller.last_anchor_depth(),
            ));

            let anchor_world = {
                let (r, t) = (camera_rotation, camera_translation);
                r * anchor_view_space + t
            };

            let rot_init = Transform::default()
                .looking_to(**initial_facing_direction, **initial_up_direction)
                .rotation;
            let rot_target = Transform::default()
                .looking_to(**target_facing_direction, **target_up_direction)
                .rotation;

            let rot_next = rot_init.slerp(rot_target, progress);
            let rot_last = camera_rotation;
            let rot_delta = rot_next * rot_last.inverse().as_quat();

            let original_translation = camera_translation;
            let original_rotation = camera_rotation;
            rotate_around(
                &mut camera_translation,
                &mut camera_rotation,
                anchor_world,
                rot_delta.as_dquat(),
            );
            let (_, delta_rotation, delta_translation) = {
                let original =
                    DAffine3::from_rotation_translation(original_rotation, original_translation);
                let new = DAffine3::from_rotation_translation(camera_rotation, camera_translation);
                (original.inverse() * new).to_scale_rotation_translation()
            };

            let mut camera_muts = camera_set.p2();
            let mut camera_mut = camera_muts.get_mut(*camera).unwrap();
            if let Some(ref read_write) = read_write {
                let CustomReadWrite { apply_delta, .. } = &**read_write;
                apply_delta(&mut camera_mut, delta_translation, delta_rotation);
            } else {
                EditorCam::default_apply_delta(&mut camera_mut, delta_translation, delta_rotation);
            }
            if progress_t >= 1.0 {
                *complete = true;
            }
            redraw.write(RequestRedraw);
        }
        state.map.retain(|_, v| !v.complete);
    }
}
