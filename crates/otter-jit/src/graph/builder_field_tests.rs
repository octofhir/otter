//! Own-field specialization retains banks without movable address nodes.
//!
//! # Contents
//! - Loads and stores specialize exact CacheIR field locations in both banks.
//! - Store barriers retain the rooted receiver and value rather than slab bases.
//! - Inline appends stay native; suffix appends use the collecting cached store.
//!
//! # Invariants
//! Fake shape offsets are compile-time geometry only; no fake object executes.
//! Native execution of the access sequence is covered in `arm64::own_fields`.
//!
//! # See also
//! - [`super::build`] and `otter_vm::object::FieldLocation`.

use std::rc::Rc;

use otter_bytecode::{Op, Operand};
use otter_vm::{
    JitCompileSnapshot,
    jit::{JitCacheIrOp, JitCacheIrProgram, JitTestInstruction},
    object::FieldLocation,
};

use super::{Kind, Repr, build};
use crate::graph::{BaselineSupport, bytecode::Analysis};

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
                .filter(|node| matches!(node.kind, Kind::LoadOwnField(_) | Kind::StoreOwnField(_)))
                .collect();
            assert_eq!(accesses.len(), 1);
            let access = accesses[0];
            assert_eq!(
                access.kind,
                if store {
                    Kind::StoreOwnField(field)
                } else {
                    Kind::LoadOwnField(field)
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
        assert_eq!(store.kind, Kind::StorePropertyCached { pc: 0, atom: None });
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
                .any(|node| matches!(node.kind, Kind::StoreNamedProperty(_) | Kind::Deopt(_))),
            "an allocation-required append completes without an eager shape miss"
        );
        let barrier = built
            .graph
            .nodes
            .iter()
            .find(|node| node.kind == Kind::WriteBarrier)
            .unwrap();
        assert_eq!(barrier.inputs, store.inputs);
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
        .find(|node| matches!(node.kind, Kind::StoreNamedProperty(_)))
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
        assert!(
            built
                .graph
                .nodes
                .iter()
                .any(|node| node.kind == Kind::StoreOwnField(field))
        );
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
            Kind::StoreNamedProperty(_) | Kind::StorePropertyCached { .. } | Kind::Deopt(_)
        )),
        "the ordinary cached-store contract cannot replace an explicit strict store"
    );
}
