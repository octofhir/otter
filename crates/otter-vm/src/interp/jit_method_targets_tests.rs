//! Canonical method-body ownership in the real compile-snapshot producer.
//!
//! # Contents
//! - Direct call linkage and Template field proofs share each prepared body.
//! - Three nested method edges consume three body admissions and keep their
//!   exact allocations through recursive field-type baking.
//! - Equal function ids on different receiver shapes remain separate targets.
//!
//! # Invariants
//! Fixtures use linked verified bytecode, live ordinary receiver slots and the
//! production method-lookup/feedback owners. Pointer observations never clone a
//! snapshot Arc: a field-baking copy would therefore expose an extra owner or a
//! changed allocation. Budget checks measure work charged by the real baker.
//!
//! # See also
//! - `jit_compile::bake_call_site_plans` owns target and body preparation.
//! - `jit_field_types::bake_field_type_tree` finalizes the same owned tree.
//! - Runtime native-method tests cover emitted guard and call semantics.

use super::*;
use otter_bytecode::{Constant, Function, FunctionCodeBuilder, Operand};
use std::collections::BTreeSet;
use std::sync::Arc;

const METHOD_NAMES: [&str; 3] = ["first", "second", "leaf"];
const VALUE_NAME: &str = "value";

fn method_module() -> otter_bytecode::BytecodeModule {
    let mut module = crate::test_support::minimal_bytecode_module("method-body-owner.js");
    module.constants = METHOD_NAMES
        .into_iter()
        .chain([VALUE_NAME])
        .map(|name| Constant::String {
            utf16: name.encode_utf16().collect(),
        })
        .collect();
    module.functions = (0..=3)
        .map(|id| {
            let mut code = FunctionCodeBuilder::new();
            code.push(Op::LoadThis, &[Operand::Register(0)]);
            if id < 3 {
                code.push(
                    Op::CallMethodValue,
                    &[
                        Operand::Register(1),
                        Operand::Register(0),
                        Operand::ConstIndex(id),
                        Operand::ConstIndex(0),
                    ],
                );
            } else {
                code.push(
                    Op::LoadProperty,
                    &[
                        Operand::Register(1),
                        Operand::Register(0),
                        Operand::ConstIndex(3),
                    ],
                );
            }
            code.push(Op::ReturnValue, &[Operand::Register(1)]);
            Function {
                id,
                name: format!("method{id}"),
                locals: 2,
                is_strict: true,
                code: code.finish(),
                ..Function::default()
            }
        })
        .collect();
    module
}

/// Observe physical body and instruction-buffer ownership without retaining it.
fn body_allocations(view: &jit::JitCompileSnapshot) -> Vec<(u32, usize, usize)> {
    let mut pending = vec![view];
    let mut allocations = Vec::new();
    let mut unique = BTreeSet::new();
    while let Some(view) = pending.pop() {
        assert!(
            view.inline_callees.is_empty(),
            "fixture has only method edges"
        );
        let mut sites: Vec<_> = view.method_targets.iter().collect();
        sites.sort_by_key(|(byte_pc, _)| **byte_pc);
        for (_, targets) in sites {
            for target in targets {
                let body = target.body.as_ref().expect("admitted small method body");
                assert_eq!(
                    Arc::strong_count(body),
                    1,
                    "a target's call plan and Template proof own no second body"
                );
                assert_eq!(target.guard.method_fid, body.code_block.id);
                let callee = target.callee.as_ref().expect("published real entry plan");
                assert_eq!(callee.plan.function_id, body.code_block.id);
                assert_ne!(callee.plan.entry_cell, 0);
                assert!(target.inline_fields.is_some(), "same-body Template proof");
                let identity = Arc::as_ptr(body) as usize;
                assert!(unique.insert(identity), "one allocation per source target");
                allocations.push((
                    body.code_block.id,
                    identity,
                    body.instructions.as_ptr() as usize,
                ));
                pending.push(body.as_ref());
            }
        }
    }
    allocations
}

fn remaining_body_admissions(budget: &mut InlineSnapshotBudget) -> usize {
    let mut count = 0;
    while budget.enter(Some(0), 0, 1, None) {
        count += 1;
        assert!(count <= 64, "root snapshot work remains bounded");
        budget.leave();
    }
    count
}

#[test]
fn method_targets_keep_one_body_through_real_nested_baking() {
    for two_receiver_shapes in [false, true] {
        let mut vm = Interpreter::new().expect("fixture interpreter bootstrap");
        let context = vm
            .link_module(
                method_module(),
                crate::source_registry::SourceRegistry::default(),
            )
            .expect("verified method sources");
        let base = context.function_base();
        vm.ensure_method_feedback_context(&context);
        let mut stack = ActivationStack::new();
        vm.with_runtime_turn(&mut stack, |turn| {
            let (vm, stack) = turn.into_parts();
            vm.with_handle_scope(|vm, scope| {
                let receiver = vm.scoped_object_bare(scope).expect("ordinary receiver");
                for (index, name) in METHOD_NAMES.into_iter().enumerate() {
                    let mut object = vm.escape_scoped(receiver).as_object().unwrap();
                    vm.create_data_property(
                        &mut object,
                        name,
                        Value::function(base + index as u32 + 1),
                    )
                    .expect("live method slot");
                }
                let mut object = vm.escape_scoped(receiver).as_object().unwrap();
                vm.create_data_property(&mut object, VALUE_NAME, Value::number_i32(317))
                    .expect("live data field");

                // Train the leaf's actual property program, so final recursive
                // baking populates a real field bound, not merely empty maps.
                assert_eq!(
                    vm.jit_runtime_load_property_value(
                        stack,
                        &context,
                        base + 3,
                        1,
                        vm.escape_scoped(receiver),
                    )
                    .expect("committed leaf load"),
                    Value::number_i32(317)
                );

                let mut root_call_pc = None;
                for (caller, name_index) in (base..base + 3).zip(0..3) {
                    let view = context.jit_compile_snapshot(caller).unwrap();
                    let instruction = view
                        .instructions
                        .iter()
                        .find(|instruction| instruction.op(&view.code_block) == Op::CallMethodValue)
                        .unwrap();
                    let feedback_site = instruction.property_ic_site(&view.code_block).unwrap();
                    let mut value = vm.escape_scoped(receiver);
                    let site = vm
                        .method_site_for_receiver(&context, caller, name_index, &mut value)
                        .expect("real own-method descriptor and receiver shape");
                    assert!(vm.note_method_target(feedback_site, caller + 1, site));
                    if caller == base {
                        root_call_pc = Some((instruction.byte_pc, feedback_site));
                    }
                }

                if two_receiver_shapes {
                    let other = vm.scoped_object_bare(scope).expect("second receiver");
                    // Different order gives the same method function a different
                    // receiver guard and holder-slot location at this site.
                    let mut object = vm.escape_scoped(other).as_object().unwrap();
                    vm.create_data_property(&mut object, VALUE_NAME, Value::number_i32(719))
                        .expect("second receiver's first field");
                    for (index, name) in METHOD_NAMES.into_iter().enumerate() {
                        let mut object = vm.escape_scoped(other).as_object().unwrap();
                        vm.create_data_property(
                            &mut object,
                            name,
                            Value::function(base + index as u32 + 1),
                        )
                        .expect("same method on a different shape");
                    }
                    let mut value = vm.escape_scoped(other);
                    let site = vm
                        .method_site_for_receiver(&context, base, 0, &mut value)
                        .expect("second exact receiver target");
                    assert!(vm.note_method_target(root_call_pc.unwrap().1, base + 1, site));
                }

                let mut view = context.jit_compile_snapshot(base).unwrap();
                let mut budget = InlineSnapshotBudget::new();
                super::Baker::new(&crate::interp::jit_bake_env::BakeEnv::capture(vm), None)
                    .bake_call_site_plans(
                        &mut view,
                        &context,
                        base,
                        jit_debug::JitDebugTier::Optimizing,
                        &mut budget,
                    );
                let targets = &view.method_targets[&root_call_pc.unwrap().0];
                let population = if two_receiver_shapes { 2 } else { 1 };
                assert_eq!(targets.len(), population);
                for (index, target) in targets.iter().enumerate() {
                    assert_eq!(target.target_index, index as u32);
                    assert_eq!(target.target_count, population as u32);
                    assert_eq!(target.guard.method_fid, base + 1);
                }
                if two_receiver_shapes {
                    assert_ne!(targets[0].guard.recv_shape, targets[1].guard.recv_shape);
                    assert_ne!(targets[0].guard.method_field, targets[1].guard.method_field);
                }
                let before = body_allocations(&view);
                assert_eq!(
                    before.len(),
                    3 * population,
                    "three edges per real root target"
                );
                assert_eq!(
                    remaining_body_admissions(&mut budget),
                    64 - before.len(),
                    "call linkage and Template proof do not charge another body bake"
                );
                super::Baker::new(&crate::interp::jit_bake_env::BakeEnv::capture(vm), None)
                    .bake_field_type_tree(&mut view);
                assert_eq!(
                    body_allocations(&view),
                    before,
                    "field finalization must not copy a prepared body or its instructions"
                );
                let mut pending = vec![&view];
                let mut bounded_leaves = 0;
                while let Some(view) = pending.pop() {
                    for target in view.method_targets.values().flatten() {
                        let body = target.body.as_deref().unwrap();
                        if body.code_block.id == base + 3 {
                            assert!(
                                !body.field_types.is_empty(),
                                "real leaf field was finalized"
                            );
                            bounded_leaves += 1;
                        }
                        pending.push(body);
                    }
                }
                assert_eq!(bounded_leaves, population);
            });
        });
    }
}
