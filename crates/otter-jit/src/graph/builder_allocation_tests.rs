//! Native literal kinds and exact source-owned inlined allocation plans.
//!
//! # Contents
//! - Empty kinds and canonical-home initialized nonempty literals.
//! - Different caller/callee realms without forbidding the body splice.
//! - Moving element storage never keeps an interior pointer across collection.
//! - A collecting allocation keeps every layout proof, a constructed
//!   receiver's included, but not its youth.
//!
//! # Invariants
//! These tests inspect the production builder before emitting or executing
//! code. Scalar shape offsets are geometry fixtures, never published GC cells.
//! Every native kind requests four temporaries and a separate result.

use std::rc::Rc;
use std::sync::Arc;

use otter_bytecode::{Op, Operand};
use otter_vm::jit::{
    JitDirectCallPlan, JitDirectCallThisMode, JitDirectCallee, JitEmptyObjectAllocationPlan,
    JitTestInstruction,
};
use otter_vm::native_abi::NativeFrameKind;
use otter_vm::{JitCompileSnapshot, JitInlineCallee};

use super::{Kind, build};
use crate::graph::{BaselineSupport, bytecode::Analysis, ir::ResultPolicy};

#[path = "builder_element_storage_tests.rs"]
mod element_storage_tests;

#[path = "builder_lexical_allocation_tests.rs"]
mod lexical_allocation_tests;

fn snapshot(
    fid: u32,
    params: u16,
    registers: u16,
    code: Vec<(Op, Vec<Operand>)>,
) -> JitCompileSnapshot {
    let instructions = code
        .into_iter()
        .enumerate()
        .map(|(pc, (op, operands))| JitTestInstruction::new(op, pc as u32, pc as u32 * 4, operands))
        .collect();
    JitCompileSnapshot::without_feedback(fid, params, registers, instructions)
}

fn graph(view: &JitCompileSnapshot) -> super::Built {
    let analysis = Rc::new(Analysis::build(view).expect("literal analysis"));
    let baseline = BaselineSupport {
        supported: vec![true; view.instructions.len()],
    };
    build(view, &analysis, &baseline, None).expect("literal graph")
}

#[test]
fn literal_allocation_kinds_have_declared_temps_and_canonical_inputs() {
    let view = snapshot(
        90,
        1,
        4,
        vec![
            (Op::NewObject, vec![Operand::Register(1)]),
            (
                Op::NewArray,
                vec![Operand::Register(2), Operand::ConstIndex(0)],
            ),
            (
                Op::NewArray,
                vec![
                    Operand::Register(3),
                    Operand::ConstIndex(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(3)]),
        ],
    );
    let built = graph(&view);
    for kind in [Kind::NewObject, Kind::NewArrayEmpty] {
        let nodes: Vec<_> = built
            .graph
            .nodes
            .iter()
            .filter(|node| node.kind == kind)
            .collect();
        assert_eq!(nodes.len(), 1);
        assert!(nodes[0].inputs.is_empty());
        assert!(nodes[0].eager.is_some());
        assert!(nodes[0].lazy.is_none());
        let properties = kind.properties();
        assert!(properties.may_collect && properties.effectful && properties.can_throw);
        assert!(!properties.call);
        let constraints = kind.constraints(0, &crate::graph::registers::AARCH64);
        assert!(constraints.inputs.is_empty());
        assert_eq!(constraints.gp_temps, 4);
        assert_eq!(constraints.fp_temps, 0);
        assert_eq!(constraints.result, ResultPolicy::Register);
    }
    let array = built
        .graph
        .nodes
        .iter()
        .find(|node| node.kind == Kind::NewArrayLiteral)
        .expect("native nonempty array");
    assert_eq!(array.inputs.len(), 2);
    let constraints = array.kind.constraints(2, &crate::graph::registers::AARCH64);
    assert!(
        constraints
            .inputs
            .iter()
            .all(|input| *input == super::super::ir::InputPolicy::Home)
    );
    assert_eq!(constraints.gp_temps, 4);
    assert!(
        !built
            .graph
            .nodes
            .iter()
            .any(|node| matches!(node.kind, Kind::Generic { pc: 2, .. }))
    );
}

#[test]
fn inlined_literal_allocations_keep_callee_geometry_and_semantic_realm() {
    let mut callee = snapshot(
        91,
        0,
        2,
        vec![
            (Op::NewObject, vec![Operand::Register(0)]),
            (
                Op::NewArray,
                vec![Operand::Register(1), Operand::ConstIndex(0)],
            ),
            (Op::ReturnValue, vec![Operand::Register(1)]),
        ],
    );
    callee.literal_allocations.realm_id = 7;
    callee.literal_allocations.object = Some(JitEmptyObjectAllocationPlan::new(864));
    let callee = Arc::new(callee);
    let mut caller = snapshot(
        90,
        1,
        3,
        vec![
            (
                Op::Call,
                vec![
                    Operand::Register(1),
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                ],
            ),
            (Op::NewObject, vec![Operand::Register(2)]),
            (Op::ReturnValue, vec![Operand::Register(1)]),
        ],
    );
    caller.literal_allocations.realm_id = 0;
    caller.literal_allocations.object = Some(JitEmptyObjectAllocationPlan::new(432));
    caller.instructions[0].call_attempted = true;
    caller.direct_callees.insert(
        0,
        vec![JitDirectCallee {
            plan: JitDirectCallPlan {
                function_id: 91,
                code_object_id: 1,
                entry_cell: 0,
                tier: NativeFrameKind::Baseline,
                this_mode: JitDirectCallThisMode::StrictOrLexical,
                is_derived_constructor: false,
                call_flags: 0,
                callee_cell: 0,
            },
            receiver_allocation: None,
        }],
    );
    caller
        .inline_callees
        .insert(0, JitInlineCallee { body: callee });
    let built = graph(&caller);
    assert_eq!(
        built.inline_views.len(),
        1,
        "different source realms do not ban an otherwise valid splice"
    );
    assert_eq!(built.inline_views[0].literal_allocations.realm_id, 7);
    assert_eq!(
        built.inline_views[0]
            .literal_allocations
            .object
            .unwrap()
            .shape,
        864
    );
    let allocations: Vec<_> = built
        .graph
        .nodes
        .iter()
        .filter(|node| matches!(node.kind, Kind::NewObject | Kind::NewArrayEmpty))
        .collect();
    assert_eq!(allocations.len(), 3);
    assert_eq!(
        allocations.iter().filter(|node| node.origin == 1).count(),
        2
    );
    assert_eq!(
        allocations.iter().filter(|node| node.origin == 0).count(),
        1
    );
    for node in allocations.into_iter().filter(|node| node.origin == 1) {
        let state = built
            .graph
            .frame_state(node.eager.expect("own allocation recipe"));
        assert_eq!(state.function_id, 91);
        assert!(
            state.caller.is_some(),
            "cold source owner retains the outer continuation"
        );
    }
}

#[path = "allocation_group_tests.rs"]
mod allocation_group_tests;

#[test]
fn tenured_object_literals_barrier_each_possible_cell_input() {
    for mode in [
        otter_vm::jit::JitSiteMode::Young,
        otter_vm::jit::JitSiteMode::Track,
        otter_vm::jit::JitSiteMode::Old,
    ] {
        let mut view = snapshot(
            90,
            1,
            4,
            vec![
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(7)]),
                (
                    Op::NewObjectLiteral,
                    vec![
                        Operand::Register(2),
                        Operand::ConstIndex(2),
                        Operand::ConstIndex(0),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(2)]),
            ],
        );
        view.literal_allocations.sites.insert(
            1,
            otter_vm::jit::JitAllocationSite {
                id: 3,
                record: 64,
                mode,
            },
        );
        let built = graph(&view);
        let literal = built
            .graph
            .nodes
            .iter()
            .position(|node| matches!(node.kind, Kind::NewObjectLiteral { .. }))
            .expect("object literal");
        let barriers: Vec<_> = built
            .graph
            .nodes
            .iter()
            .filter(|node| node.kind == Kind::WriteBarrier)
            .collect();
        if mode == otter_vm::jit::JitSiteMode::Old {
            // The parameter may be a young cell; the int never is.
            assert_eq!(barriers.len(), 1);
            assert_eq!(barriers[0].inputs[0].0 as usize, literal);
        } else {
            assert!(barriers.is_empty(), "{mode:?} cells are born young");
        }
    }
}

#[test]
fn an_allocation_keeps_the_layout_of_a_constructed_receiver() {
    use otter_vm::jit::{JitCacheIrOp, JitCacheIrProgram, JitFieldType, JitFieldTypeGuard};
    use otter_vm::object::{FieldLocation, field_type};
    // `function C() { this.items = []; }` inlined into `new C()`: the array
    // allocation may collect, which changes no shape, so the store still
    // runs only the program of the receiver's proved shape.
    let mut constructor = snapshot(
        801,
        0,
        3,
        vec![
            (Op::LoadThis, vec![Operand::Register(0)]),
            (
                Op::NewArray,
                vec![Operand::Register(1), Operand::ConstIndex(0)],
            ),
            (
                Op::StoreProperty,
                vec![
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                    Operand::Register(1),
                    Operand::Register(2),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(0)]),
        ],
    );
    let field = FieldLocation::inline(0);
    constructor.seed_property_attempted_for_test(2);
    constructor.property_programs.insert(
        8,
        vec![JitCacheIrProgram {
            ops: vec![
                JitCacheIrOp::GuardShape {
                    object: 0,
                    shape: 864,
                },
                JitCacheIrOp::GuardExtensible { object: 0, field },
                JitCacheIrOp::StoreField { object: 0, field },
                JitCacheIrOp::PublishShape {
                    object: 0,
                    shape: 872,
                },
            ]
            .into_boxed_slice(),
        }],
    );
    constructor.field_types.insert(
        (872, field),
        JitFieldType {
            guard: JitFieldTypeGuard {
                owner_shape: 872,
                expected: field_type::TOP,
            },
            shapes: Default::default(),
            stored_shapes: Default::default(),
            nullish_mask: 0,
        },
    );
    let mut caller = snapshot(
        800,
        1,
        2,
        vec![
            (
                Op::New,
                vec![
                    Operand::Register(1),
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(1)]),
        ],
    );
    caller.instructions[0].call_attempted = true;
    caller.literal_allocations.group_allowed = true;
    caller.direct_constructs.insert(
        0,
        JitDirectCallee {
            plan: JitDirectCallPlan {
                function_id: 801,
                code_object_id: 1,
                entry_cell: 0,
                tier: NativeFrameKind::Baseline,
                this_mode: JitDirectCallThisMode::StrictOrLexical,
                is_derived_constructor: false,
                call_flags: otter_vm::native_abi::FUNCTION_CALL_CONSTRUCTIBLE,
                callee_cell: 0,
            },
            receiver_allocation: Some(otter_vm::jit::JitReceiverAllocationPlan {
                new_target_function_id: 801,
                new_target_is_class: false,
                family_id: 1,
                family_cell: 432,
                receiver_shape: 0,
                initial_field_count: 0,
                inline_capacity: 4,
                prototype_validity: None,
                prototype_root: 864,
            }),
        },
    );
    caller.inline_callees.insert(
        0,
        JitInlineCallee {
            body: Arc::new(constructor),
        },
    );
    let built = graph(&caller);
    let kinds: Vec<_> = built.graph.nodes.iter().map(|node| &node.kind).collect();
    let receiver = kinds
        .iter()
        .position(|kind| matches!(kind, Kind::NewReceiver(_)))
        .expect("an inlined receiver allocation");
    let array = kinds
        .iter()
        .position(|kind| **kind == Kind::NewArrayEmpty)
        .expect("a native array allocation");
    assert!(receiver < array);
    let store = built
        .graph
        .nodes
        .iter()
        .find(|node| matches!(node.kind, Kind::StoreNamedProperty { .. }))
        .expect("a store through the receiver's program");
    assert!(
        matches!(
            store.kind,
            Kind::StoreNamedProperty {
                receiver_shape: Some(864),
                fresh: false,
                ..
            }
        ),
        "{:?}",
        store.kind
    );
    assert_eq!(built.graph.node(store.inputs[0]).kind, kinds[receiver].clone());
    // The array may have collected: the receiver is no longer known young,
    // so the stored array is barriered.
    assert!(
        built
            .graph
            .nodes
            .iter()
            .any(|node| node.kind == Kind::WriteBarrier && node.inputs == store.inputs)
    );
}
