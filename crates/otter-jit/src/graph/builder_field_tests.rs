//! Shape-selected property programs and persistent field banks.
//!
//! # Contents
//! - Loads and stores specialize exact CacheIR field locations in both banks.
//! - Store barriers retain the rooted receiver and value rather than slab bases.
//! - Inline appends stay native; suffix appends use the collecting cached store.
//! - Known ordinary receivers select load programs across body splices.
//! - Uncovered known shapes keep guards and exact eager source chains.
//! - Field summaries retain nullable distinctions and recheck metadata after effects.
//! - Physical body proofs survive effects without preserving mutable layouts.
//!
//! # Invariants
//! Fake shape offsets are compile-time geometry only; no fake object executes.
//! Native execution of the access sequence is covered in `arm64::own_fields`.
//!
//! # See also
//! - [`super::build`] and `otter_vm::object::FieldLocation`.

use std::{rc::Rc, sync::Arc};

use otter_bytecode::{
    BytecodeModule, Constant, Function, FunctionCodeBuilder, Op, Operand, SourceKind,
};
use otter_vm::{
    ExecutionContext, JitCompileSnapshot, JitInlineCallee,
    jit::{
        JitCacheIrOp, JitCacheIrProgram, JitDirectCallPlan, JitDirectCallThisMode, JitDirectCallee,
        JitFieldType, JitFieldTypeGuard, JitPrototypeValidity, JitTestInstruction,
    },
    native_abi::NativeFrameKind,
    object::{FieldLocation, field_type},
};

use super::{Kind, Repr, build};
use crate::graph::{BaselineSupport, bytecode::Analysis};

#[path = "builder_field_type_tests.rs"]
mod field_type_tests;

#[path = "builder_body_kind_tests.rs"]
mod body_kind_tests;

fn seed_top_fields(view: &mut JitCompileSnapshot) {
    for programs in view.property_programs.values() {
        for program in programs {
            let Some(JitCacheIrOp::GuardShape { object: 0, shape }) = program.ops.first() else {
                continue;
            };
            let shape = program
                .ops
                .iter()
                .find_map(|op| match op {
                    JitCacheIrOp::PublishShape { object: 0, shape } => Some(*shape),
                    _ => None,
                })
                .unwrap_or(*shape);
            for op in &program.ops {
                if let JitCacheIrOp::StoreField { object: 0, field } = op {
                    view.field_types.insert(
                        (shape, *field),
                        JitFieldType {
                            guard: JitFieldTypeGuard {
                                owner_shape: shape,
                                expected: field_type::TOP,
                            },
                            shapes: smallvec::SmallVec::new(),
                            stored_shapes: smallvec::SmallVec::new(),
                            nullish_mask: 0,
                        },
                    );
                }
            }
        }
    }
}

#[test]
fn own_fields_keep_the_bank_in_a_single_access_and_barrier_the_receiver() {
    for field in [
        FieldLocation::inline(0),
        FieldLocation::inline(63),
        FieldLocation::overflow(0),
        FieldLocation::overflow(5000),
    ] {
        for store in [false, true] {
            let (op, operands) = if store {
                (
                    Op::StoreProperty,
                    vec![
                        Operand::Register(0),
                        Operand::ConstIndex(0),
                        Operand::Register(1),
                        Operand::Register(2),
                    ],
                )
            } else {
                (
                    Op::LoadProperty,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::ConstIndex(0),
                    ],
                )
            };
            let mut view = JitCompileSnapshot::without_feedback(
                90,
                2,
                3,
                vec![
                    JitTestInstruction::new(op, 0, 0, operands),
                    JitTestInstruction::new(Op::ReturnValue, 1, 4, vec![Operand::Register(2)]),
                ],
            );
            view.property_programs.insert(
                0,
                vec![JitCacheIrProgram {
                    ops: vec![
                        JitCacheIrOp::GuardShape {
                            object: 0,
                            shape: 128,
                        },
                        JitCacheIrOp::GuardAtomSlot {
                            object: 0,
                            atom: 1,
                            field,
                            writable: store,
                        },
                        if store {
                            JitCacheIrOp::StoreField { object: 0, field }
                        } else {
                            JitCacheIrOp::LoadField { object: 0, field }
                        },
                    ]
                    .into_boxed_slice(),
                }],
            );
            seed_top_fields(&mut view);
            let analysis = Rc::new(Analysis::build(&view).unwrap());
            let built = build(
                &view,
                &analysis,
                &BaselineSupport {
                    supported: vec![true; 2],
                },
                None,
            )
            .unwrap();
            let accesses: Vec<_> = built
                .graph
                .nodes
                .iter()
                .filter(|node| {
                    matches!(
                        node.kind,
                        Kind::LoadOwnField { .. } | Kind::StoreOwnField { .. }
                    )
                })
                .collect();
            assert_eq!(accesses.len(), 1);
            let access = accesses[0];
            assert_eq!(
                access.kind,
                if store {
                    Kind::StoreOwnField {
                        field,
                        atom: 1,
                        owners: smallvec::SmallVec::new(),
                    }
                } else {
                    Kind::LoadOwnField { field, atom: 1 }
                }
            );
            assert_eq!(built.graph.node(access.inputs[0]).repr, Repr::Tagged);
            assert!(
                !built.graph.nodes.iter().any(|node| node.repr == Repr::Word),
                "no suffix storage address is exposed to allocation or GC"
            );
            if store {
                let barrier = built
                    .graph
                    .nodes
                    .iter()
                    .find(|node| node.kind == Kind::WriteBarrier)
                    .unwrap();
                assert_eq!(barrier.inputs, access.inputs);
            }
        }
    }
}

fn transition_program(shape: u32, field: FieldLocation) -> JitCacheIrProgram {
    JitCacheIrProgram {
        ops: vec![
            JitCacheIrOp::GuardShape { object: 0, shape },
            JitCacheIrOp::GuardPrototypeNull { object: 0 },
            JitCacheIrOp::GuardExtensible { object: 0, field },
            JitCacheIrOp::StoreField { object: 0, field },
            JitCacheIrOp::PublishShape {
                object: 0,
                shape: shape + 8,
            },
        ]
        .into_boxed_slice(),
    }
}

fn store_graph(op: Op, programs: Vec<JitCacheIrProgram>) -> super::Built {
    let mut view = JitCompileSnapshot::without_feedback(
        90,
        2,
        3,
        vec![
            JitTestInstruction::new(
                op,
                0,
                0,
                vec![
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                    Operand::Register(1),
                    Operand::Register(2),
                ],
            ),
            JitTestInstruction::new(Op::ReturnUndefined, 1, 4, vec![]),
        ],
    );
    view.seed_property_attempted_for_test(0);
    view.property_programs.insert(0, programs);
    seed_top_fields(&mut view);
    let analysis = Rc::new(Analysis::build(&view).unwrap());
    build(
        &view,
        &analysis,
        &BaselineSupport {
            supported: vec![true; view.instructions.len()],
        },
        None,
    )
    .unwrap()
}

#[test]
fn overflow_appends_keep_a_collecting_completion_inside_the_graph() {
    let inline = transition_program(128, FieldLocation::inline(3));
    let overflow = transition_program(256, FieldLocation::overflow(0));
    let growing_overflow = transition_program(384, FieldLocation::overflow(17));
    for programs in [
        vec![overflow.clone()],
        vec![growing_overflow],
        vec![inline.clone(), overflow.clone()],
        vec![overflow, inline],
    ] {
        let built = store_graph(Op::StoreProperty, programs);
        let store = built
            .graph
            .nodes
            .iter()
            .find(|node| matches!(node.kind, Kind::StorePropertyCached { .. }))
            .expect("suffix append keeps its committed allocating miss");
        assert_eq!(
            store.kind,
            Kind::StorePropertyCached {
                pc: 0,
                atom: None,
                barrier: true,
            }
        );
        assert!(store.kind.properties().may_collect);
        assert!(store.kind.properties().can_throw);
        assert!(
            store.eager.is_some(),
            "the collecting store owns its recovery state"
        );
        assert!(
            store
                .inputs
                .iter()
                .all(|&input| built.graph.node(input).repr == Repr::Tagged)
        );
        assert!(
            !built
                .graph
                .nodes
                .iter()
                .any(|node| matches!(node.kind, Kind::StoreNamedProperty { .. } | Kind::Deopt(_))),
            "an allocation-required append completes without an eager shape miss"
        );
        assert!(
            !built
                .graph
                .nodes
                .iter()
                .any(|node| node.kind == Kind::WriteBarrier),
            "the cached store owns its hit's dynamic storage barrier; the source receiver is not necessarily its parent"
        );
    }
}

#[test]
fn inline_appends_and_existing_suffix_overwrites_still_specialize() {
    let built = store_graph(
        Op::StoreProperty,
        vec![transition_program(128, FieldLocation::inline(3))],
    );
    let append = built
        .graph
        .nodes
        .iter()
        .find(|node| matches!(node.kind, Kind::StoreNamedProperty { .. }))
        .expect("shape capacity proves the nonallocating inline append");
    assert!(!append.kind.properties().may_collect);
    assert!(!built.graph.nodes.iter().any(|node| matches!(
        node.kind,
        Kind::StorePropertyCached { .. } | Kind::Generic { .. }
    )));

    let field = FieldLocation::overflow(17);
    for op in [Op::StoreProperty, Op::StorePropertyStrict] {
        let built = store_graph(
            op,
            vec![JitCacheIrProgram {
                ops: vec![
                    JitCacheIrOp::GuardShape {
                        object: 0,
                        shape: 256,
                    },
                    JitCacheIrOp::GuardAtomSlot {
                        object: 0,
                        atom: 1,
                        field,
                        writable: true,
                    },
                    JitCacheIrOp::StoreField { object: 0, field },
                ]
                .into_boxed_slice(),
            }],
        );
        assert!(built.graph.nodes.iter().any(|node| node.kind
            == Kind::StoreOwnField {
                field,
                atom: 1,
                owners: smallvec::SmallVec::new()
            }));
        assert!(
            !built.graph.nodes.iter().any(|node| matches!(
                node.kind,
                Kind::StorePropertyCached { .. } | Kind::Generic { .. }
            )),
            "overwriting a resident suffix slot does not need allocation"
        );
    }
}

#[test]
fn overflow_append_preserves_the_strict_store_boundary() {
    let built = store_graph(
        Op::StorePropertyStrict,
        vec![transition_program(128, FieldLocation::overflow(0))],
    );
    assert!(
        built
            .graph
            .nodes
            .iter()
            .any(|node| matches!(node.kind, Kind::Generic { pc: 0, .. }))
    );
    assert!(
        !built.graph.nodes.iter().any(|node| matches!(
            node.kind,
            Kind::StoreNamedProperty { .. } | Kind::StorePropertyCached { .. } | Kind::Deopt(_)
        )),
        "the ordinary cached-store contract cannot replace an explicit strict store"
    );
}

fn load_program(shape: u32, field: FieldLocation, inherited: bool) -> JitCacheIrProgram {
    let mut ops = vec![JitCacheIrOp::GuardShape { object: 0, shape }];
    let object = if inherited {
        ops.extend([
            JitCacheIrOp::GuardPrototypeValidity {
                validity: JitPrototypeValidity {
                    address: 8,
                    identity: u64::from(shape),
                },
            },
            JitCacheIrOp::LoadPrototypeHolder {
                root: shape + 8,
                result: 1,
            },
            JitCacheIrOp::GuardShape {
                object: 1,
                shape: shape + 16,
            },
        ]);
        1
    } else {
        0
    };
    ops.extend([
        JitCacheIrOp::GuardAtomSlot {
            object,
            atom: 1,
            field,
            writable: false,
        },
        JitCacheIrOp::LoadField { object, field },
    ]);
    JitCacheIrProgram {
        ops: ops.into_boxed_slice(),
    }
}

fn load_instruction(pc: u32, destination: u16) -> JitTestInstruction {
    JitTestInstruction::new(
        Op::LoadProperty,
        pc,
        pc * 4,
        vec![
            Operand::Register(destination),
            Operand::Register(0),
            Operand::ConstIndex(0),
        ],
    )
}

fn load_graph(view: &JitCompileSnapshot) -> super::Built {
    let analysis = Rc::new(Analysis::build(view).unwrap());
    build(
        view,
        &analysis,
        &BaselineSupport {
            supported: vec![true; view.instructions.len()],
        },
        None,
    )
    .unwrap()
}

#[test]
fn prior_receiver_proofs_select_own_banks_and_preserve_uncovered_shape_checks() {
    for covered in [false, true] {
        let mut view = JitCompileSnapshot::without_feedback(
            90,
            1,
            3,
            vec![
                load_instruction(0, 1),
                load_instruction(1, 2),
                JitTestInstruction::new(Op::ReturnValue, 2, 8, vec![Operand::Register(2)]),
            ],
        );
        let mut anchors = vec![load_program(128, FieldLocation::inline(0), false)];
        if !covered {
            anchors.push(load_program(256, FieldLocation::inline(0), false));
        }
        view.property_programs
            .insert(view.instructions[0].byte_pc, anchors);
        view.property_programs.insert(
            view.instructions[1].byte_pc,
            if covered {
                vec![
                    load_program(256, FieldLocation::overflow(0), false),
                    load_program(128, FieldLocation::inline(1), false),
                ]
            } else {
                vec![load_program(128, FieldLocation::inline(1), false)]
            },
        );
        let built = load_graph(&view);
        assert!(built.graph.nodes.iter().any(|node| {
            node.kind
                == Kind::LoadOwnField {
                    field: FieldLocation::inline(1),
                    atom: 1,
                }
        }));
        let checks: Vec<_> = built
            .graph
            .nodes
            .iter()
            .filter(|node| matches!(node.kind, Kind::CheckShapes { .. }))
            .collect();
        assert_eq!(checks.len(), if covered { 1 } else { 2 });
        if !covered {
            assert!(matches!(
                &checks[1].kind,
                Kind::CheckShapes { shapes, cell_proved: true, .. }
                    if shapes.as_slice() == [128]
            ));
        }
    }
}

#[test]
fn selected_named_programs_remove_only_exact_receiver_guards() {
    for covered in [false, true] {
        let mut view = JitCompileSnapshot::without_feedback(
            90,
            1,
            3,
            vec![
                load_instruction(0, 1),
                load_instruction(1, 2),
                JitTestInstruction::new(Op::ReturnValue, 2, 8, vec![Operand::Register(2)]),
            ],
        );
        let mut anchors = vec![load_program(128, FieldLocation::inline(0), true)];
        if !covered {
            anchors.push(load_program(256, FieldLocation::inline(0), true));
        }
        view.property_programs
            .insert(view.instructions[0].byte_pc, anchors);
        let pc = view.instructions[1].byte_pc;
        view.property_programs.insert(
            pc,
            vec![
                load_program(384, FieldLocation::overflow(0), true),
                load_program(128, FieldLocation::inline(1), true),
            ],
        );
        let built = load_graph(&view);
        let access = built
            .graph
            .nodes
            .iter()
            .find(|node| matches!(node.kind, Kind::LoadNamedProperty { byte_pc, .. } if byte_pc == pc))
            .unwrap();
        assert_eq!(
            access.kind,
            Kind::LoadNamedProperty {
                byte_pc: pc,
                programs: smallvec::smallvec![1],
                receiver_shape: covered.then_some(128),
            }
        );
        let frame = built.graph.frame_state(access.eager.unwrap());
        assert_eq!((frame.function_id, frame.pc, frame.byte_pc), (90, 1, pc));
    }
}

#[test]
fn inlined_named_selection_uses_its_source_bank_and_full_eager_chain() {
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
            module: "named-load-source-geometry".into(),
            source_kind: SourceKind::JavaScript,
            functions: vec![
                Function {
                    id: 0,
                    code: entry.finish(),
                    ..Function::default()
                },
                Function {
                    id: 1,
                    name: "readValue".into(),
                    param_count: 1,
                    locals: 2,
                    is_strict: true,
                    ignores_this: true,
                    code: bytes.finish(),
                    ..Function::default()
                },
            ],
            constants: vec![Constant::String {
                utf16: "value".encode_utf16().collect(),
            }],
            function_source: None,
            template_sites: vec![],
            module_resolutions: vec![],
            module_inits: vec![],
            lazy_environments: Vec::new(),
        },
        Default::default(),
    )
    .expect("verified inline property source");
    let mut body = context.jit_compile_snapshot(1).unwrap();
    assert!(!body.code_block.observes_this());
    let body_pc = body.instructions[0].byte_pc;
    body.property_programs.insert(
        body_pc,
        vec![
            load_program(256, FieldLocation::overflow(0), true),
            load_program(128, FieldLocation::inline(1), true),
        ],
    );
    let mut caller = JitCompileSnapshot::without_feedback(
        90,
        2,
        4,
        vec![
            load_instruction(0, 2),
            JitTestInstruction::new(
                Op::Call,
                1,
                4,
                vec![
                    Operand::Register(3),
                    Operand::Register(1),
                    Operand::ConstIndex(1),
                    Operand::Register(0),
                ],
            ),
            JitTestInstruction::new(Op::ReturnValue, 2, 8, vec![Operand::Register(3)]),
        ],
    );
    assert_eq!(caller.instructions[0].byte_pc, body_pc);
    caller.property_programs.insert(
        body_pc,
        vec![load_program(128, FieldLocation::inline(0), true)],
    );
    let call_pc = caller.instructions[1].byte_pc;
    caller.instructions[1].call_attempted = true;
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
    let built = load_graph(&caller);
    assert_eq!(built.graph.inlined.len(), 1);
    let access = built
        .graph
        .nodes
        .iter()
        .find(|node| node.origin == 1 && matches!(node.kind, Kind::LoadNamedProperty { .. }))
        .unwrap();
    assert_eq!(
        access.kind,
        Kind::LoadNamedProperty {
            byte_pc: body_pc,
            programs: smallvec::smallvec![1],
            receiver_shape: Some(128),
        }
    );
    let frame = built.graph.frame_state(access.eager.unwrap());
    assert_eq!(
        (frame.function_id, frame.pc, frame.byte_pc),
        (1, 0, body_pc)
    );
    let outer = built
        .graph
        .frame_state(frame.caller.as_ref().unwrap().state);
    assert_eq!(
        (outer.function_id, outer.pc, outer.byte_pc),
        (90, 2, caller.instructions[2].byte_pc)
    );
}

#[test]
fn array_expando_fields_use_tagged_storage_while_eager_state_keeps_array() {
    for store in [false, true] {
        for field in [FieldLocation::inline(0), FieldLocation::overflow(3)] {
            let operands = if store {
                vec![
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                    Operand::Register(1),
                    Operand::Register(2),
                ]
            } else {
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                ]
            };
            let mut view = JitCompileSnapshot::without_feedback(
                90,
                2,
                3,
                vec![
                    JitTestInstruction::new(
                        if store {
                            Op::StoreProperty
                        } else {
                            Op::LoadProperty
                        },
                        0,
                        0,
                        operands,
                    ),
                    JitTestInstruction::new(Op::ReturnValue, 1, 4, vec![Operand::Register(2)]),
                ],
            );
            view.array_layout = otter_vm::jit::JitArrayLayout::current();
            view.property_programs.insert(
                0,
                vec![JitCacheIrProgram {
                    ops: vec![
                        JitCacheIrOp::LoadArrayExpando {
                            object: 0,
                            result: 1,
                            layout: view.array_layout,
                        },
                        JitCacheIrOp::GuardShape {
                            object: 1,
                            shape: 128,
                        },
                        JitCacheIrOp::GuardAtomSlot {
                            object: 1,
                            atom: 1,
                            field,
                            writable: store,
                        },
                        if store {
                            JitCacheIrOp::StoreField { object: 1, field }
                        } else {
                            JitCacheIrOp::LoadField { object: 1, field }
                        },
                    ]
                    .into_boxed_slice(),
                }],
            );
            view.field_types.insert(
                (128, field),
                JitFieldType {
                    guard: JitFieldTypeGuard {
                        owner_shape: 128,
                        expected: field_type::TOP,
                    },
                    shapes: smallvec::SmallVec::new(),
                    stored_shapes: smallvec::SmallVec::new(),
                    nullish_mask: 0,
                },
            );
            let built = load_graph(&view);
            let (bag_id, bag) = built
                .graph
                .nodes
                .iter()
                .enumerate()
                .find(|(_, node)| matches!(node.kind, Kind::LoadArrayExpando(_)))
                .unwrap();
            let bag_id = super::NodeId(bag_id as u32);
            assert_eq!(bag.repr, Repr::Tagged);
            let frame = built.graph.frame_state(bag.eager.unwrap());
            assert_eq!((frame.function_id, frame.pc), (90, 0));
            assert_eq!(
                frame
                    .registers
                    .iter()
                    .find(|(register, _)| *register == 0)
                    .unwrap()
                    .1,
                bag.inputs[0]
            );
            let access = built
                .graph
                .nodes
                .iter()
                .find(|node| {
                    matches!(
                        node.kind,
                        Kind::StoreOwnField { .. } | Kind::LoadOwnField { .. }
                    )
                })
                .unwrap();
            assert_eq!(access.inputs[0], bag_id);
            assert_ne!(access.inputs[0], bag.inputs[0]);
            if store {
                let barrier = built
                    .graph
                    .nodes
                    .iter()
                    .find(|node| node.kind == Kind::WriteBarrier)
                    .unwrap();
                assert_eq!(
                    barrier.inputs, access.inputs,
                    "old bag owns the RHS edge, not the array shell"
                );
            }
            assert!(!built.graph.nodes.iter().any(|node| matches!(
                node.kind,
                Kind::Generic { .. }
                    | Kind::LoadPropertyCached { .. }
                    | Kind::StorePropertyCached { .. }
            )));
        }
    }
}
