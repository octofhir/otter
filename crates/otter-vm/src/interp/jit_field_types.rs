//! Resolve shape-owned field bounds at each source compilation boundary.
//!
//! # Contents
//! - `bake_field_type_tree` captures defining words and rooted ordinary targets
//!   after the complete bounded source tree's allocation plans are prepared.
//! - Each physical storage location has one defining field-record authority.
//!
//! # Invariants
//! - Only ordinary own data fields receive result bounds; inherited and
//!   accessor results keep their existing load proofs.
//! - Each source view owns its storage-shape/location records. An Array bag
//!   supplies the field shape while the source operation retains the Array.
//!   An append names the destination shape's defining field, not its old receiver.
//! - Defining shapes and every admitted target enter the same compile roots
//!   retained when the native generation is installed.
//! - Missing, unconstrained or unstable historical targets grant no load shape
//!   proof. Each resolvable historical target independently grants store
//!   membership for an exact current RHS shape proof, even when unstable.
//! - Captured words are canonical. Their low byte changes on every widening,
//!   so a live byte comparison protects every admitted load's complete bound.
//! - Each method target owns one prepared body. The tree visits it once,
//!   independently of its generated-call plan and Template field proof.
//!
//! # See also
//! - [`crate::object::field_type`] owns the observed upper bounds.
//! - [`crate::jit::JitFieldType`] owns the borrow-free native DTO.

use std::collections::BTreeSet;

use crate::Interpreter;
use crate::jit::{JitCacheIrOp, JitCompileSnapshot, JitFieldType, JitFieldTypeGuard};
use crate::object::{FieldLocation, ShapeHandle, ShapeId, field_type, shape_body};

impl Interpreter {
    /// Capture each source after receiver plans have seeded their initial words.
    pub(super) fn bake_field_type_tree(&self, view: &mut JitCompileSnapshot) {
        self.bake_field_types(view);
        for callee in view.inline_callees.values_mut() {
            self.bake_field_type_tree(std::sync::Arc::make_mut(&mut callee.body));
        }
        for methods in view.method_targets.values_mut() {
            for method in methods {
                if let Some(body) = &mut method.body {
                    self.bake_field_type_tree(std::sync::Arc::make_mut(body));
                }
            }
        }
    }

    /// Prepare defining cells for every own property and native birth field.
    pub(super) fn bake_field_types(&self, view: &mut JitCompileSnapshot) {
        let mut locations = BTreeSet::new();
        let programs = view
            .property_programs
            .values()
            .flatten()
            .chain(view.proved_store_programs.values());
        {
            for program in programs {
                let operand = u8::from(matches!(
                    program.ops.first(),
                    Some(
                        JitCacheIrOp::LoadArrayExpando {
                            object: 0,
                            result: 1,
                            ..
                        } | JitCacheIrOp::LoadFunctionBag {
                            object: 0,
                            result: 1
                        }
                    )
                ));
                let Some(shape) = program.ops.iter().find_map(|op| match *op {
                    JitCacheIrOp::GuardShape { object, shape } if object == operand => Some(shape),
                    _ => None,
                }) else {
                    continue;
                };
                let destination = program.ops.iter().find_map(|op| match *op {
                    JitCacheIrOp::PublishShape { shape, .. } => Some(shape),
                    _ => None,
                });
                for op in &program.ops {
                    match *op {
                        JitCacheIrOp::LoadField { object, field } if object == operand => {
                            locations.insert((shape, field));
                        }
                        JitCacheIrOp::StoreField { object, field } if object == operand => {
                            locations.insert((destination.unwrap_or(shape), field));
                        }
                        _ => {}
                    }
                }
            }
        }
        for plan in view.literal_allocations.objects.values() {
            for slot in 0..plan.value_count {
                locations.insert((plan.shape, FieldLocation::inline(slot)));
            }
        }
        for callee in view.direct_constructs.values() {
            if let Some(plan) = callee.receiver_allocation
                && plan.receiver_shape != 0
            {
                for slot in 0..u32::from(plan.initial_field_count) {
                    locations.insert((plan.receiver_shape, FieldLocation::inline(slot)));
                }
            }
        }
        view.field_types.clear();
        for (receiver, location) in locations {
            if let Some(bound) = self.bake_field_type(receiver, location) {
                view.field_types.insert((receiver, location), bound);
            }
        }
    }

    fn bake_field_type(&self, receiver: u32, field: FieldLocation) -> Option<JitFieldType> {
        // SAFETY: these offsets come only from this session's rooted ordinary
        // shape baker, which prepared the source programs and literal plans.
        let shape = unsafe { ShapeHandle::from_offset(receiver) };
        let slot = field.logical_slot(shape_body::inline_capacity_of(shape));
        let owner = field_type::owner(shape, slot)?;
        let owner_shape = self.bake_shape(owner)?;
        let expected = field_type::snapshot(owner);
        if !field_type::is_canonical(expected) {
            return None;
        }
        let mut shapes = smallvec::SmallVec::new();
        let mut stored_shapes = smallvec::SmallVec::new();
        if expected != 0
            && expected != field_type::TOP
            && field_type::int32_constant(expected).is_none()
        {
            let mut complete_stable_bound =
                expected & (field_type::INT32 | field_type::DOUBLE) == 0;
            for shift in [
                field_type::FIRST_SHAPE_SHIFT,
                field_type::SECOND_SHAPE_SHIFT,
            ] {
                let id = (expected >> shift) & field_type::SHAPE_MASK;
                if id == 0 {
                    continue;
                }
                let target = self
                    .shape_runtime
                    .handle_for_id(&self.gc_heap, ShapeId::from_raw(id));
                let Some((target, baked)) =
                    target.and_then(|target| self.bake_shape(target).map(|baked| (target, baked)))
                else {
                    complete_stable_bound = false;
                    continue;
                };
                stored_shapes.push(baked);
                if shape_body::is_stable(target) {
                    shapes.push(baked);
                } else {
                    complete_stable_bound = false;
                }
            }
            if !complete_stable_bound {
                shapes.clear();
            }
        }
        Some(JitFieldType {
            guard: JitFieldTypeGuard {
                owner_shape,
                expected,
            },
            shapes,
            stored_shapes,
            nullish_mask: (expected & (field_type::NULL | field_type::UNDEFINED)) as u8,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Value, jit_roots::CompilationRoot, object};

    #[test]
    fn historical_store_membership_survives_partial_load_bound_refusal() {
        let mut interpreter = Interpreter::new().expect("fixture interpreter");
        interpreter.with_handle_scope(|interpreter, scope| {
            let first = interpreter.scoped_object_bare(scope).expect("first target");
            let second = interpreter
                .scoped_object_bare(scope)
                .expect("second target");
            let receiver = interpreter.scoped_object_bare(scope).expect("receiver");
            let mut first_object = interpreter.escape_scoped(first).as_object().unwrap();
            interpreter
                .create_data_property(&mut first_object, "first", Value::NULL)
                .unwrap();
            let mut second_object = interpreter.escape_scoped(second).as_object().unwrap();
            interpreter
                .create_data_property(&mut second_object, "second", Value::NULL)
                .unwrap();
            let mut receiver_object = interpreter.escape_scoped(receiver).as_object().unwrap();
            interpreter
                .create_data_property(
                    &mut receiver_object,
                    "value",
                    interpreter.escape_scoped(first),
                )
                .unwrap();
            interpreter
                .create_data_property(
                    &mut receiver_object,
                    "value",
                    interpreter.escape_scoped(second),
                )
                .unwrap();
            first_object = interpreter.escape_scoped(first).as_object().unwrap();
            second_object = interpreter.escape_scoped(second).as_object().unwrap();
            receiver_object = interpreter.escape_scoped(receiver).as_object().unwrap();
            let first_shape = object::shape(first_object, interpreter.gc_heap());
            let second_shape = object::shape(second_object, interpreter.gc_heap());
            let receiver_shape = object::shape(receiver_object, interpreter.gc_heap());
            let location = object::field_location(receiver_shape, 0);
            let owner = field_type::owner(receiver_shape, 0).unwrap();
            let stable = interpreter
                .bake_field_type(receiver_shape.offset(), location)
                .unwrap();
            assert_eq!(stable.shapes.len(), 2);
            assert_eq!(stable.stored_shapes, stable.shapes);

            interpreter
                .create_data_property(&mut second_object, "changed", Value::NULL)
                .unwrap();
            assert!(!shape_body::is_stable(second_shape));
            let mixed = interpreter
                .bake_field_type(receiver_shape.offset(), location)
                .unwrap();
            assert!(mixed.shapes.is_empty());
            assert_eq!(mixed.stored_shapes, stable.stored_shapes);
            for target in [owner, first_shape, second_shape] {
                assert!(
                    interpreter.jit_compile_roots.borrow().iter().any(
                        |root| matches!(root, CompilationRoot::Shape(shape) if *shape == target)
                    )
                );
            }

            let missing = ShapeId::from_raw(field_type::SHAPE_MASK);
            assert!(
                interpreter
                    .shape_runtime
                    .handle_for_id(&interpreter.gc_heap, missing)
                    .is_none()
            );
            let expected = field_type::merge(
                field_type::shape_id_sample(shape_body::id_of(first_shape).raw()),
                field_type::shape_id_sample(missing.raw()),
            );
            field_type::set_snapshot_for_test(owner, expected);
            let partial = interpreter
                .bake_field_type(receiver_shape.offset(), location)
                .unwrap();
            assert_eq!(partial.guard.expected, expected);
            assert!(partial.shapes.is_empty());
            assert_eq!(partial.stored_shapes.as_slice(), &[first_shape.offset()]);

            for malformed in [
                expected & !field_type::SHAPE_COUNT_MASK,
                expected | field_type::SHAPE_COUNT_MASK,
            ] {
                field_type::set_snapshot_for_test(owner, malformed);
                assert!(
                    interpreter
                        .bake_field_type(receiver_shape.offset(), location)
                        .is_none()
                );
            }
            field_type::observe_owner(owner, Value::TRUE);
            let top = interpreter
                .bake_field_type(receiver_shape.offset(), location)
                .unwrap();
            assert!(top.shapes.is_empty() && top.stored_shapes.is_empty());
        });
    }

    #[test]
    fn literal_birth_fields_capture_membership_without_property_programs() {
        let mut interpreter = Interpreter::new().expect("fixture interpreter");
        interpreter.with_handle_scope(|interpreter, scope| {
            let target = interpreter.scoped_object_bare(scope).expect("target");
            let receiver = interpreter.scoped_object_bare(scope).expect("receiver");
            let mut target_object = interpreter.escape_scoped(target).as_object().unwrap();
            interpreter
                .create_data_property(&mut target_object, "target", Value::NULL)
                .unwrap();
            let mut receiver_object = interpreter.escape_scoped(receiver).as_object().unwrap();
            interpreter
                .create_data_property(
                    &mut receiver_object,
                    "link",
                    interpreter.escape_scoped(target),
                )
                .unwrap();
            interpreter
                .create_data_property(&mut receiver_object, "count", Value::number_i32(7))
                .unwrap();
            target_object = interpreter.escape_scoped(target).as_object().unwrap();
            receiver_object = interpreter.escape_scoped(receiver).as_object().unwrap();
            let target_shape = object::shape(target_object, interpreter.gc_heap());
            let receiver_shape = object::shape(receiver_object, interpreter.gc_heap());
            let plan = crate::jit::JitObjectLiteralAllocationPlan::new(receiver_shape, 2)
                .expect("inline literal plan");
            let mut view = JitCompileSnapshot::without_feedback(
                90,
                0,
                0,
                vec![crate::jit::JitTestInstruction::new(
                    otter_bytecode::Op::ReturnUndefined,
                    0,
                    0,
                    Vec::new(),
                )],
            );
            view.literal_allocations.objects.insert(0, plan);
            assert!(view.property_programs.is_empty());
            interpreter.bake_field_types(&mut view);
            assert_eq!(view.field_types.len(), 2);
            let link = &view.field_types[&(receiver_shape.offset(), FieldLocation::inline(0))];
            assert_eq!(
                link.guard.owner_shape,
                field_type::owner(receiver_shape, 0).unwrap().offset()
            );
            assert_eq!(link.stored_shapes.as_slice(), &[target_shape.offset()]);
            assert_eq!(link.shapes, link.stored_shapes);
            assert_eq!(
                link.guard.expected as u8,
                1 << field_type::SHAPE_COUNT_SHIFT
            );
            let count = &view.field_types[&(receiver_shape.offset(), FieldLocation::inline(1))];
            assert_eq!(
                count.guard.owner_shape,
                field_type::owner(receiver_shape, 1).unwrap().offset()
            );
            assert_eq!(count.guard.expected, field_type::int32_sample(7));
            assert!(count.shapes.is_empty() && count.stored_shapes.is_empty());

            interpreter
                .create_data_property(&mut receiver_object, "link", Value::number_i32(3))
                .unwrap();
            interpreter.bake_field_types(&mut view);
            let mixed = &view.field_types[&(receiver_shape.offset(), FieldLocation::inline(0))];
            assert_eq!(mixed.guard.expected & field_type::INT32, field_type::INT32);
            assert!(
                mixed.shapes.is_empty(),
                "numeric arm denies object load proof"
            );
            assert_eq!(mixed.stored_shapes.as_slice(), &[target_shape.offset()]);
        });
    }
}
