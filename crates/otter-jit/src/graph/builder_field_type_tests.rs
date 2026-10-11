//! SSA field-bound proofs at the graph construction boundary.
//!
//! # Contents
//! - Nullable refinement and owner/target checks across effects.
//! - Receiver-specific store authority and loop guard cache lifetime.
//! - Demand for owner guards across SSA merges and distinct receivers.
//! - Nullish comparisons specialize only under independently consumed bounds.
//!
//! # Invariants
//! - Synthetic summaries authorize only the explicit target shapes.
//! - Graph checks retain their original eager source instruction.
//!
//! # See also
//! - `super::super::field_type` for the production value-bound helpers.

use super::*;

fn field_type_view(ops: Vec<(Op, Vec<Operand>)>, nullish_mask: u8) -> JitCompileSnapshot {
    let instructions = ops
        .into_iter()
        .enumerate()
        .map(|(pc, (op, operands))| JitTestInstruction::new(op, pc as u32, pc as u32 * 4, operands))
        .collect();
    let mut view = JitCompileSnapshot::without_feedback(90, 2, 7, instructions);
    view.stable_shapes.extend([128, 256]);
    view.field_types.insert(
        (128, FieldLocation::inline(0)),
        JitFieldType {
            guard: JitFieldTypeGuard {
                owner_shape: 128,
                expected: field_type::merge(
                    field_type::shape_id_sample(256),
                    u64::from(nullish_mask),
                ),
            },
            shapes: smallvec::smallvec![256],
            stored_shapes: smallvec::smallvec![256],
            nullish_mask,
        },
    );
    view
}

fn field_load(destination: u16, receiver: u16) -> (Op, Vec<Operand>) {
    (
        Op::LoadProperty,
        vec![
            Operand::Register(destination),
            Operand::Register(receiver),
            Operand::ConstIndex(0),
        ],
    )
}

#[test]
fn a_returned_field_does_not_require_its_owner_guard() {
    let mut view = field_type_view(
        vec![
            field_load(2, 0),
            (Op::ReturnValue, vec![Operand::Register(2)]),
        ],
        3,
    );
    view.property_programs
        .insert(0, vec![load_program(128, FieldLocation::inline(0), false)]);
    let built = load_graph(&view);
    let guard = built
        .graph
        .nodes
        .iter()
        .find(|node| matches!(node.kind, Kind::CheckFieldTypes(_)))
        .unwrap();
    assert_eq!(guard.block, None);
    assert_eq!(guard.eager, None);
    assert!(built.graph.blocks.iter().all(|block| {
        block
            .body
            .iter()
            .all(|&node| !matches!(built.graph.node(node).kind, Kind::CheckFieldTypes(_)))
    }));
}

#[test]
fn nullish_comparisons_reuse_consumed_bounds_without_demanding_owner_guards() {
    for consumed in [false, true] {
        let mut ops = vec![
            field_load(2, 0),
            (Op::LoadNull, vec![Operand::Register(3)]),
            (
                Op::LooseEqual,
                vec![
                    Operand::Register(4),
                    Operand::Register(2),
                    Operand::Register(3),
                ],
            ),
        ];
        if consumed {
            ops.push(field_load(5, 2));
        }
        ops.push((Op::ReturnValue, vec![Operand::Register(4)]));
        let mut view = field_type_view(ops, 3);
        view.property_programs
            .insert(0, vec![load_program(128, FieldLocation::inline(0), false)]);
        if consumed {
            view.property_programs
                .insert(12, vec![load_program(256, FieldLocation::inline(0), false)]);
        }
        let built = load_graph(&view);
        assert!(built.graph.nodes.iter().any(|node| {
            node.kind
                == Kind::NullishEqual {
                    negate: false,
                    object_or_nullish: consumed,
                }
        }));
        let guards: Vec<_> = built
            .graph
            .nodes
            .iter()
            .filter(|node| node.block.is_some() && matches!(node.kind, Kind::CheckFieldTypes(_)))
            .collect();
        assert_eq!(guards.len(), usize::from(consumed));
        if let [guard] = guards.as_slice() {
            let state = built.graph.frame_state(guard.eager.unwrap());
            assert_eq!((state.function_id, state.pc, state.byte_pc), (90, 0, 0));
        }
    }
}

#[test]
fn nullish_comparison_of_a_proved_receiver_needs_no_owner_guard() {
    let mut view = field_type_view(
        vec![
            field_load(2, 0),
            (Op::LoadNull, vec![Operand::Register(3)]),
            (
                Op::LooseNotEqual,
                vec![
                    Operand::Register(4),
                    Operand::Register(0),
                    Operand::Register(3),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(4)]),
        ],
        3,
    );
    view.property_programs
        .insert(0, vec![load_program(128, FieldLocation::inline(0), false)]);
    let built = load_graph(&view);
    assert!(built.graph.nodes.iter().any(|node| {
        node.kind
            == Kind::NullishEqual {
                negate: true,
                object_or_nullish: true,
            }
    }));
    assert!(
        built
            .graph
            .nodes
            .iter()
            .all(|node| { node.block.is_none() || !matches!(node.kind, Kind::CheckFieldTypes(_)) })
    );
}

#[test]
fn a_later_consumed_load_retains_the_first_receivers_guard_and_source_frame() {
    let mut view = field_type_view(
        vec![
            field_load(2, 0),
            field_load(3, 1),
            field_load(4, 3),
            (Op::ReturnValue, vec![Operand::Register(4)]),
        ],
        0,
    );
    for pc in [0, 4] {
        view.property_programs
            .insert(pc, vec![load_program(128, FieldLocation::inline(0), false)]);
    }
    view.property_programs
        .insert(8, vec![load_program(256, FieldLocation::inline(0), false)]);
    let built = load_graph(&view);
    let guards: Vec<_> = built
        .graph
        .nodes
        .iter()
        .filter(|node| node.block.is_some() && matches!(node.kind, Kind::CheckFieldTypes(_)))
        .collect();
    assert_eq!(guards.len(), 1);
    let state = built.graph.frame_state(guards[0].eager.unwrap());
    assert_eq!((state.function_id, state.pc, state.byte_pc), (90, 0, 0));
    assert!(state.caller.is_none());
}

#[test]
fn a_non_loop_phi_keeps_pending_fields_and_current_stable_receiver_arms() {
    for stable_receiver in [false, true] {
        let mut view = field_type_view(
            vec![
                field_load(2, 0),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(2), Operand::Register(1)],
                ),
                (Op::LoadLocal, vec![Operand::Register(3), Operand::Imm32(2)]),
                (Op::Jump, vec![Operand::Imm32(1)]),
                (Op::LoadLocal, vec![Operand::Register(3), Operand::Imm32(0)]),
                field_load(4, 3),
                (Op::ReturnValue, vec![Operand::Register(4)]),
            ],
            0,
        );
        if !stable_receiver {
            view.stable_shapes.remove(&128);
        }
        view.property_programs
            .insert(0, vec![load_program(128, FieldLocation::inline(0), false)]);
        view.property_programs.insert(
            20,
            [128, 256]
                .into_iter()
                .map(|shape| load_program(shape, FieldLocation::inline(1), false))
                .collect(),
        );
        let built = load_graph(&view);
        let owner_guards: Vec<_> = built
            .graph
            .nodes
            .iter()
            .filter(|node| node.block.is_some() && matches!(node.kind, Kind::CheckFieldTypes(_)))
            .collect();
        assert_eq!(owner_guards.len(), usize::from(stable_receiver));
        if let [guard] = owner_guards.as_slice() {
            let state = built.graph.frame_state(guard.eager.unwrap());
            assert_eq!((state.function_id, state.pc, state.byte_pc), (90, 0, 0));
        }
        let shape_guards = built
            .graph
            .nodes
            .iter()
            .filter(|node| matches!(node.kind, Kind::CheckShapes { .. }))
            .count();
        assert_eq!(shape_guards, if stable_receiver { 1 } else { 2 });
        let stable_guards: Vec<_> = built
            .graph
            .nodes
            .iter()
            .filter(|node| matches!(node.kind, Kind::CheckStableShapes(_)))
            .collect();
        assert_eq!(stable_guards.len(), usize::from(stable_receiver));
        if let [guard] = stable_guards.as_slice() {
            assert_eq!(
                guard.kind,
                Kind::CheckStableShapes(smallvec::smallvec![128, 256])
            );
            assert_eq!(built.graph.frame_state(guard.eager.unwrap()).byte_pc, 20);
        }
    }
}

#[test]
fn caller_consumption_keeps_an_inlined_loads_original_eager_chain() {
    let mut bytes = FunctionCodeBuilder::new();
    bytes.push(
        Op::LoadProperty,
        &[
            Operand::Register(1),
            Operand::Register(0),
            Operand::ConstIndex(0),
        ],
    );
    bytes.push(Op::ReturnValue, &[Operand::Register(1)]);
    let mut entry = FunctionCodeBuilder::new();
    entry.push(Op::ReturnUndefined, &[]);
    let context = ExecutionContext::from_module(
        BytecodeModule {
            module: "field-bound-inline-source".into(),
            source_kind: SourceKind::JavaScript,
            functions: vec![
                Function {
                    id: 0,
                    code: entry.finish(),
                    ..Function::default()
                },
                Function {
                    id: 1,
                    param_count: 1,
                    locals: 2,
                    is_strict: true,
                    ignores_this: true,
                    code: bytes.finish(),
                    ..Function::default()
                },
            ],
            constants: vec![Constant::String {
                utf16: "child".encode_utf16().collect(),
            }],
            function_source: None,
            template_sites: vec![],
            module_resolutions: vec![],
            module_inits: vec![],
        },
        Default::default(),
    )
    .expect("verified inline field source");
    let mut body = context.jit_compile_snapshot(1).unwrap();
    let body_pc = body.instructions[0].byte_pc;
    body.property_programs.insert(
        body_pc,
        vec![load_program(128, FieldLocation::inline(0), false)],
    );
    body.field_types.insert(
        (128, FieldLocation::inline(0)),
        JitFieldType {
            guard: JitFieldTypeGuard {
                owner_shape: 128,
                expected: field_type::shape_id_sample(256),
            },
            shapes: smallvec::smallvec![256],
            stored_shapes: smallvec::smallvec![256],
            nullish_mask: 0,
        },
    );
    let mut caller = field_type_view(
        vec![
            (
                Op::Call,
                vec![
                    Operand::Register(2),
                    Operand::Register(1),
                    Operand::ConstIndex(1),
                    Operand::Register(0),
                ],
            ),
            field_load(3, 2),
            (Op::ReturnValue, vec![Operand::Register(3)]),
        ],
        0,
    );
    let call_pc = caller.instructions[0].byte_pc;
    caller.instructions[0].call_attempted = true;
    caller.direct_callees.insert(
        call_pc,
        vec![JitDirectCallee {
            plan: JitDirectCallPlan {
                function_id: body.code_block.id,
                code_object_id: 1,
                entry_cell: 0,
                tier: NativeFrameKind::Baseline,
                this_mode: JitDirectCallThisMode::StrictOrLexical,
                is_derived_constructor: false,
                call_flags: body.code_block.call_flags(),
                callee_cell: 0,
            },
            receiver_allocation: None,
        }],
    );
    caller.inline_callees.insert(
        call_pc,
        JitInlineCallee {
            body: Arc::new(body),
        },
    );
    caller
        .property_programs
        .insert(4, vec![load_program(256, FieldLocation::inline(0), false)]);
    let built = load_graph(&caller);
    assert_eq!(built.graph.inlined.len(), 1);
    let guard = built
        .graph
        .nodes
        .iter()
        .find(|node| node.block.is_some() && matches!(node.kind, Kind::CheckFieldTypes(_)))
        .unwrap();
    let inner = built.graph.frame_state(guard.eager.unwrap());
    assert_eq!(
        (inner.function_id, inner.pc, inner.byte_pc),
        (1, 0, body_pc)
    );
    let entry = inner.caller.as_ref().unwrap();
    assert_eq!(entry.return_register, 2);
    let outer = built.graph.frame_state(entry.state);
    assert_eq!((outer.function_id, outer.pc, outer.byte_pc), (90, 1, 4));
}

#[test]
fn field_summary_proofs_remove_target_checks_after_a_nullable_guard() {
    for mask in [0, 3] {
        let mut view = field_type_view(
            vec![
                field_load(1, 0),
                field_load(2, 1),
                field_load(3, 1),
                (Op::ReturnValue, vec![Operand::Register(3)]),
            ],
            mask,
        );
        view.property_programs
            .insert(0, vec![load_program(128, FieldLocation::inline(0), false)]);
        view.property_programs
            .insert(4, vec![load_program(256, FieldLocation::inline(0), false)]);
        view.property_programs
            .insert(8, vec![load_program(256, FieldLocation::inline(1), false)]);
        let built = load_graph(&view);
        let nodes = &built.graph.nodes;
        assert_eq!(
            nodes
                .iter()
                .filter(|n| matches!(n.kind, Kind::CheckShapes { .. }))
                .count(),
            1
        );
        assert_eq!(
            nodes
                .iter()
                .filter(|n| matches!(n.kind, Kind::CheckFieldTypes(_)))
                .count(),
            1
        );
        assert_eq!(
            nodes
                .iter()
                .filter(|n| matches!(n.kind, Kind::CheckStableShapes(_)))
                .count(),
            1
        );
        let nullish: Vec<_> = nodes
            .iter()
            .filter(|n| matches!(n.kind, Kind::CheckNotNullish { .. }))
            .collect();
        assert_eq!(nullish.len(), usize::from(mask != 0));
        if let [guard] = nullish.as_slice() {
            assert_eq!(guard.kind, Kind::CheckNotNullish { mask });
            assert_eq!(built.graph.frame_state(guard.eager.unwrap()).byte_pc, 4);
        }
    }
}

#[test]
fn collecting_effects_repeat_owner_and_target_guards_without_rechecking_targets() {
    let mut view = field_type_view(
        vec![
            field_load(1, 0),
            field_load(2, 1),
            (Op::NewObject, vec![Operand::Register(5)]),
            field_load(3, 0),
            field_load(4, 3),
            (Op::ReturnValue, vec![Operand::Register(4)]),
        ],
        0,
    );
    for pc in [0, 12] {
        view.property_programs
            .insert(pc, vec![load_program(128, FieldLocation::inline(0), false)]);
    }
    for pc in [4, 16] {
        view.property_programs
            .insert(pc, vec![load_program(256, FieldLocation::inline(0), false)]);
    }
    let built = load_graph(&view);
    assert_eq!(
        built
            .graph
            .nodes
            .iter()
            .filter(|n| matches!(n.kind, Kind::CheckFieldTypes(_)))
            .count(),
        2
    );
    let frames: Vec<_> = built
        .graph
        .nodes
        .iter()
        .filter(|node| node.block.is_some() && matches!(node.kind, Kind::CheckFieldTypes(_)))
        .map(|node| {
            let state = built.graph.frame_state(node.eager.unwrap());
            (state.function_id, state.pc, state.byte_pc)
        })
        .collect();
    assert_eq!(frames, [(90, 0, 0), (90, 3, 12)]);
    assert_eq!(
        built
            .graph
            .nodes
            .iter()
            .filter(|n| matches!(&n.kind, Kind::CheckStableShapes(shapes) if shapes.contains(&256)))
            .count(),
        2
    );
    assert_eq!(
        built
            .graph
            .nodes
            .iter()
            .filter(|n| matches!(n.kind, Kind::CheckShapes { .. }))
            .count(),
        1
    );
}

#[test]
fn strict_null_refinement_keeps_undefined_while_loose_null_removes_it() {
    for (comparison, remaining) in [
        (Op::NotEqual, Some(field_type::UNDEFINED as u8)),
        (Op::LooseNotEqual, None),
    ] {
        let mut view = field_type_view(
            vec![
                field_load(1, 0),
                (Op::LoadNull, vec![Operand::Register(2)]),
                (
                    comparison,
                    vec![
                        Operand::Register(3),
                        Operand::Register(1),
                        Operand::Register(2),
                    ],
                ),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(2), Operand::Register(3)],
                ),
                field_load(4, 1),
                (Op::ReturnValue, vec![Operand::Register(4)]),
                (Op::ReturnUndefined, vec![]),
            ],
            3,
        );
        view.property_programs
            .insert(0, vec![load_program(128, FieldLocation::inline(0), false)]);
        view.property_programs
            .insert(16, vec![load_program(256, FieldLocation::inline(0), false)]);
        let built = load_graph(&view);
        let masks: Vec<_> = built
            .graph
            .nodes
            .iter()
            .filter_map(|n| match n.kind {
                Kind::CheckNotNullish { mask } => Some(mask),
                _ => None,
            })
            .collect();
        assert_eq!(masks, remaining.into_iter().collect::<Vec<_>>());
        if comparison == Op::LooseNotEqual {
            assert!(built.graph.nodes.iter().any(|node| node.kind
                == Kind::NullishEqual {
                    negate: true,
                    object_or_nullish: true,
                }));
            assert_eq!(
                built
                    .graph
                    .nodes
                    .iter()
                    .filter(|node| node.block.is_some()
                        && matches!(node.kind, Kind::CheckFieldTypes(_)))
                    .count(),
                1
            );
            assert_eq!(
                built
                    .graph
                    .nodes
                    .iter()
                    .filter(|node| matches!(node.kind, Kind::CheckShapes { .. }))
                    .count(),
                1
            );
        }
    }
}

#[test]
fn strict_nullish_refinement_preserves_polarity_and_live_comparison_values() {
    for (constant, remaining) in [
        (Op::LoadNull, field_type::UNDEFINED as u8),
        (Op::LoadUndefined, field_type::NULL as u8),
    ] {
        for (comparison, jump) in [(Op::Equal, Op::JumpIfTrue), (Op::NotEqual, Op::JumpIfFalse)] {
            for result_live in [false, true] {
                let mut view = field_type_view(
                    vec![
                        field_load(1, 0),
                        (constant, vec![Operand::Register(2)]),
                        (
                            comparison,
                            vec![
                                Operand::Register(3),
                                Operand::Register(2),
                                Operand::Register(1),
                            ],
                        ),
                        (jump, vec![Operand::Imm32(2), Operand::Register(3)]),
                        field_load(4, 1),
                        (
                            Op::ReturnValue,
                            vec![Operand::Register(if result_live { 3 } else { 4 })],
                        ),
                        (Op::ReturnUndefined, vec![]),
                    ],
                    (field_type::UNDEFINED | field_type::NULL) as u8,
                );
                view.property_programs
                    .insert(0, vec![load_program(128, FieldLocation::inline(0), false)]);
                view.property_programs
                    .insert(16, vec![load_program(256, FieldLocation::inline(0), false)]);
                let built = load_graph(&view);
                let guards: Vec<_> = built
                    .graph
                    .nodes
                    .iter()
                    .filter_map(|node| match node.kind {
                        Kind::CheckNotNullish { mask } => {
                            let frame = built.graph.frame_state(node.eager.unwrap());
                            Some((mask, frame.pc, frame.byte_pc))
                        }
                        _ => None,
                    })
                    .collect();
                assert_eq!(guards, [(remaining, 4, 16)]);
                assert!(built.graph.nodes.iter().all(|node| {
                    !matches!(
                        node.kind,
                        Kind::CheckedTaggedToInt32 | Kind::Int32Compare(_)
                    )
                }));
            }
        }
    }
}

#[test]
fn polymorphic_own_stores_select_each_defining_owner() {
    let field = FieldLocation::inline(0);
    let mut view = field_type_view(
        vec![
            (
                Op::StoreProperty,
                vec![
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                    Operand::Register(1),
                    Operand::Register(2),
                ],
            ),
            (Op::ReturnUndefined, vec![]),
        ],
        0,
    );
    view.property_programs.insert(
        0,
        [128, 256]
            .into_iter()
            .map(|shape| JitCacheIrProgram {
                ops: vec![
                    JitCacheIrOp::GuardShape { object: 0, shape },
                    JitCacheIrOp::GuardAtomSlot {
                        object: 0,
                        atom: 1,
                        field,
                        writable: true,
                    },
                    JitCacheIrOp::StoreField { object: 0, field },
                ]
                .into_boxed_slice(),
            })
            .collect(),
    );
    for (receiver, owner, target, nullish_mask) in [(128, 384, 640, 1), (256, 512, 768, 2)] {
        view.field_types.insert(
            (receiver, field),
            JitFieldType {
                guard: JitFieldTypeGuard {
                    owner_shape: owner,
                    expected: field_type::merge(
                        field_type::shape_id_sample(u64::from(target)),
                        u64::from(nullish_mask),
                    ),
                },
                shapes: smallvec::SmallVec::new(),
                stored_shapes: smallvec::smallvec![target],
                nullish_mask,
            },
        );
    }
    let built = load_graph(&view);
    let owners = built
        .graph
        .nodes
        .iter()
        .find_map(|n| match &n.kind {
            Kind::StoreOwnField { owners, .. } => Some(owners),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        owners.as_slice(),
        &[
            (
                128,
                crate::graph::ir::FieldStoreOwner {
                    owner_shape: 384,
                    stored_shapes: smallvec::smallvec![640],
                    nullish_mask: 1,
                },
            ),
            (
                256,
                crate::graph::ir::FieldStoreOwner {
                    owner_shape: 512,
                    stored_shapes: smallvec::smallvec![768],
                    nullish_mask: 2,
                },
            ),
        ]
    );
}

#[test]
fn partially_elided_stores_retain_every_admitted_receiver_arm() {
    let field = FieldLocation::inline(0);
    for second_expected in [0, field_type::NULL] {
        let mut view = field_type_view(
            vec![
                (Op::LoadNull, vec![Operand::Register(1)]),
                (
                    Op::StoreProperty,
                    vec![
                        Operand::Register(0),
                        Operand::ConstIndex(0),
                        Operand::Register(1),
                        Operand::Register(2),
                    ],
                ),
                (Op::ReturnUndefined, vec![]),
            ],
            0,
        );
        view.property_programs.insert(
            4,
            [128, 256]
                .into_iter()
                .map(|shape| JitCacheIrProgram {
                    ops: vec![
                        JitCacheIrOp::GuardShape { object: 0, shape },
                        JitCacheIrOp::GuardAtomSlot {
                            object: 0,
                            atom: 1,
                            field,
                            writable: true,
                        },
                        JitCacheIrOp::StoreField { object: 0, field },
                    ]
                    .into_boxed_slice(),
                })
                .collect(),
        );
        for (receiver, owner, expected) in
            [(128, 384, field_type::NULL), (256, 512, second_expected)]
        {
            view.field_types.insert(
                (receiver, field),
                JitFieldType {
                    guard: JitFieldTypeGuard {
                        owner_shape: owner,
                        expected,
                    },
                    shapes: smallvec::SmallVec::new(),
                    stored_shapes: smallvec::SmallVec::new(),
                    nullish_mask: expected as u8,
                },
            );
        }
        let built = load_graph(&view);
        let owners = built
            .graph
            .nodes
            .iter()
            .find_map(|node| match &node.kind {
                Kind::StoreOwnField { owners, .. } => Some(owners),
                _ => None,
            })
            .unwrap();
        if second_expected == 0 {
            assert_eq!(
                owners.as_slice(),
                &[
                    (
                        128,
                        crate::graph::ir::FieldStoreOwner {
                            owner_shape: 0,
                            stored_shapes: smallvec::SmallVec::new(),
                            nullish_mask: 2,
                        },
                    ),
                    (
                        256,
                        crate::graph::ir::FieldStoreOwner {
                            owner_shape: 512,
                            stored_shapes: smallvec::SmallVec::new(),
                            nullish_mask: 0,
                        },
                    ),
                ]
            );
        } else {
            assert!(owners.is_empty());
        }
    }
}

#[test]
fn loop_headers_recheck_target_bytes_checked_on_the_forward_path() {
    let mut view = field_type_view(
        vec![
            field_load(1, 0),
            field_load(2, 0),
            field_load(3, 2),
            (Op::LoadInt32, vec![Operand::Register(4), Operand::Imm32(0)]),
            (
                Op::LessThanImm,
                vec![
                    Operand::Register(5),
                    Operand::Register(4),
                    Operand::Imm32(3),
                ],
            ),
            (
                Op::JumpIfFalse,
                vec![Operand::Imm32(4), Operand::Register(5)],
            ),
            field_load(3, 1),
            (
                Op::StoreProperty,
                vec![
                    Operand::Register(1),
                    Operand::ConstIndex(0),
                    Operand::Register(4),
                    Operand::Register(6),
                ],
            ),
            (
                Op::AddImm,
                vec![
                    Operand::Register(4),
                    Operand::Register(4),
                    Operand::Imm32(1),
                ],
            ),
            (Op::Jump, vec![Operand::Imm32(-6)]),
            (Op::ReturnValue, vec![Operand::Register(3)]),
        ],
        0,
    );
    view.field_types.insert(
        (128, FieldLocation::inline(1)),
        JitFieldType {
            guard: JitFieldTypeGuard {
                owner_shape: 136,
                expected: field_type::shape_id_sample(256),
            },
            shapes: smallvec::smallvec![256],
            stored_shapes: smallvec::smallvec![256],
            nullish_mask: 0,
        },
    );
    view.property_programs
        .insert(0, vec![load_program(128, FieldLocation::inline(0), false)]);
    view.property_programs
        .insert(4, vec![load_program(128, FieldLocation::inline(1), false)]);
    for pc in [8, 24] {
        view.property_programs
            .insert(pc, vec![load_program(256, FieldLocation::inline(0), false)]);
    }
    view.property_programs
        .insert(28, vec![transition_program(256, FieldLocation::inline(2))]);
    seed_top_fields(&mut view);
    for pc in [4, 8] {
        view.seed_arith_feedback_for_test(
            pc,
            otter_vm::jit_feedback::ArithFeedback::from_bits(otter_vm::jit_feedback::ARITH_INT32),
        );
    }
    let built = load_graph(&view);
    let checks: Vec<_> = built
        .graph
        .nodes
        .iter()
        .filter(|n| matches!(&n.kind, Kind::CheckStableShapes(shapes) if shapes.contains(&256)))
        .collect();
    assert_eq!(checks.len(), 2);
    assert_ne!(checks[0].block, checks[1].block);
    assert_eq!(
        built.graph.frame_state(checks[1].eager.unwrap()).byte_pc,
        24
    );
}

/// One canonical non-Top record of defining owner 384 whose captured word
/// holds `nullish` and whose resolvable historical targets are `stored`.
fn store_record(stored: &[u32], nullish: u64) -> JitFieldType {
    JitFieldType {
        guard: JitFieldTypeGuard {
            owner_shape: 384,
            expected: field_type::merge(field_type::shape_id_sample(7), nullish),
        },
        shapes: smallvec::SmallVec::new(),
        stored_shapes: stored.iter().copied().collect(),
        nullish_mask: nullish as u8,
    }
}

/// Retained field-summary and stability checks of a built graph.
fn retained_checks(built: &super::super::Built) -> usize {
    built
        .graph
        .nodes
        .iter()
        .filter(|node| {
            node.block.is_some()
                && matches!(
                    node.kind,
                    Kind::CheckFieldTypes(_) | Kind::CheckStableShapes(_)
                )
        })
        .count()
}

/// The selected owner of one native store `r<receiver>.f = r<value>` through
/// `program`, after optional value proofs: an own load of `r1` guarded by
/// `value_shape`, or `null` written to `r1`.
fn named_store_owner(
    program: JitCacheIrProgram,
    record_shape: u32,
    record: JitFieldType,
    value_shape: Option<u32>,
    null_value: bool,
    receiver_register: u16,
    value_register: u16,
) -> (crate::graph::ir::FieldStoreOwner, usize) {
    let mut ops = Vec::new();
    if let Some(shape) = value_shape {
        ops.push((shape, field_load(3, 1)));
    }
    if null_value {
        ops.push((0, (Op::LoadNull, vec![Operand::Register(1)])));
    }
    let store_pc = ops.len() as u32;
    let mut view = JitCompileSnapshot::without_feedback(
        90,
        2,
        5,
        ops.iter()
            .map(|(_, op)| op.clone())
            .chain([
                (
                    Op::StoreProperty,
                    vec![
                        Operand::Register(receiver_register),
                        Operand::ConstIndex(0),
                        Operand::Register(value_register),
                        Operand::Register(2),
                    ],
                ),
                (Op::ReturnUndefined, vec![]),
            ])
            .enumerate()
            .map(|(pc, (op, operands))| {
                JitTestInstruction::new(op, pc as u32, pc as u32 * 4, operands)
            })
            .collect(),
    );
    for (pc, (shape, (op, _))) in ops.iter().enumerate() {
        if *op == Op::LoadProperty {
            view.property_programs.insert(
                pc as u32 * 4,
                vec![load_program(*shape, FieldLocation::inline(0), false)],
            );
        }
    }
    view.seed_property_attempted_for_test(store_pc);
    view.property_programs.insert(store_pc * 4, vec![program]);
    let (record_stored, record_nullish) = (record.stored_shapes.clone(), record.nullish_mask);
    view.field_types
        .insert((record_shape, FieldLocation::inline(3)), record);
    let built = load_graph(&view);
    // An overwrite is an own store; an append stays a named program.
    let owner = built
        .graph
        .nodes
        .iter()
        .find_map(|node| match &node.kind {
            Kind::StoreNamedProperty { owners, .. } => {
                assert_eq!(owners.len(), 1);
                Some(owners[0].clone())
            }
            Kind::StoreOwnField { owners, .. } => Some(match owners.as_slice() {
                [] => crate::graph::ir::FieldStoreOwner {
                    owner_shape: 0,
                    stored_shapes: record_stored.clone(),
                    nullish_mask: record_nullish,
                },
                [(_, owner)] => owner.clone(),
                _ => panic!("one admitted receiver"),
            }),
            _ => None,
        })
        .expect("native store");
    (owner, retained_checks(&built))
}

fn overwrite_program(shape: u32, field: FieldLocation) -> JitCacheIrProgram {
    JitCacheIrProgram {
        ops: vec![
            JitCacheIrOp::GuardShape { object: 0, shape },
            JitCacheIrOp::GuardAtomSlot {
                object: 0,
                atom: 1,
                field,
                writable: true,
            },
            JitCacheIrOp::StoreField { object: 0, field },
        ]
        .into_boxed_slice(),
    }
}

#[test]
fn named_appends_judge_the_value_after_publishing_the_destination_shape() {
    let field = FieldLocation::inline(3);
    let append = || transition_program(128, field);
    // `S = 128` publishes `T = 136`; authority is always the destination's.
    let cases: [(&[u32], Option<u32>, u16, bool); 7] = [
        // The receiver stored into itself is exactly `T` afterwards.
        (&[136], None, 0, false),
        (&[128], None, 0, true),
        // A value proved to have another shape is not the receiver.
        (&[256], Some(256), 1, false),
        // A value of shape `S` may be the receiver and may now be `T`.
        (&[128], Some(128), 1, true),
        (&[128, 136], Some(128), 1, false),
        // No proof: observe unless the word is Top.
        (&[128, 256], None, 1, true),
        (&[136, 256], None, 1, true),
    ];
    for (stored, value_shape, value_register, observes) in cases {
        let (owner, checks) = named_store_owner(
            append(),
            136,
            store_record(stored, 0),
            value_shape,
            false,
            0,
            value_register,
        );
        assert_eq!(owner.observes(), observes, "{stored:?} {value_shape:?}");
        assert_eq!(owner.stored_shapes.as_slice(), stored);
        assert_eq!(checks, 0, "selection adds no field or stability check");
    }
    let mut top = store_record(&[], 0);
    top.guard.expected = field_type::TOP;
    let (owner, _) = named_store_owner(append(), 136, top, None, false, 0, 1);
    assert!(!owner.observes());
}

#[test]
fn own_overwrites_keep_the_value_bound_and_nullish_constants() {
    let field = FieldLocation::inline(3);
    for (stored, nullish, value_shape, null_value, observes) in [
        (&[256][..], 0, Some(256), false, false),
        (&[256][..], 0, Some(128), false, true),
        (&[][..], field_type::NULL, None, true, false),
        (&[][..], field_type::UNDEFINED, None, true, true),
        (&[256][..], field_type::NULL, None, false, true),
    ] {
        let (owner, checks) = named_store_owner(
            overwrite_program(128, field),
            128,
            store_record(stored, nullish),
            value_shape,
            null_value,
            0,
            1,
        );
        assert_eq!(
            owner.observes(),
            observes,
            "{stored:?} {nullish} {value_shape:?}"
        );
        assert_eq!(checks, 0);
    }
}

#[test]
fn literal_fields_select_records_before_the_collecting_birth() {
    let shape = 4096;
    let fields = otter_vm::object::FieldLayout::current();
    let mut view = JitCompileSnapshot::without_feedback(
        90,
        2,
        6,
        [
            field_load(3, 1),
            (Op::LoadNull, vec![Operand::Register(4)]),
            (
                Op::NewObjectLiteral,
                vec![
                    Operand::Register(5),
                    Operand::ConstIndex(3),
                    Operand::ConstIndex(0),
                    Operand::Register(1),
                    Operand::Register(4),
                    Operand::Register(0),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(5)]),
        ]
        .into_iter()
        .enumerate()
        .map(|(pc, (op, operands))| JitTestInstruction::new(op, pc as u32, pc as u32 * 4, operands))
        .collect(),
    );
    view.property_programs
        .insert(0, vec![load_program(256, FieldLocation::inline(0), false)]);
    let plan = otter_vm::jit::JitObjectLiteralAllocationPlan {
        shape,
        shape_byte: 8,
        cell_bytes: fields.cell_bytes(4) as u32,
        header_word: 0,
        inline_capacity: 4,
        value_count: 3,
        fields,
    };
    let records = [
        store_record(&[256], 0),
        store_record(&[], field_type::NULL),
        store_record(&[256], 0),
    ];
    for (index, record) in records.iter().enumerate() {
        view.field_types
            .insert((shape, FieldLocation::inline(index as u32)), record.clone());
    }
    let literal = |view: &JitCompileSnapshot| {
        load_graph(view)
            .graph
            .nodes
            .iter()
            .find_map(|node| match &node.kind {
                Kind::NewObjectLiteral { fields } => Some(fields.clone()),
                _ => None,
            })
            .expect("object literal")
    };
    // No geometry: the committed birth alone.
    assert_eq!(literal(&view), None);
    view.literal_allocations.objects.insert(2, plan);
    let selected = literal(&view).expect("native literal plan");
    let observes: Vec<_> = selected.iter().map(|owner| owner.observes()).collect();
    assert_eq!(observes, [false, false, true]);
    assert!(
        selected
            .iter()
            .all(|owner| owner.owner_shape == 384 || !owner.observes())
    );
    // A field without authority keeps only the committed birth.
    view.field_types.remove(&(shape, FieldLocation::inline(2)));
    assert_eq!(literal(&view), None);
}
