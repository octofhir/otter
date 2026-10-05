//! Native literal kinds and exact source-owned inlined allocation plans.
//!
//! # Contents
//! - Empty kinds and canonical-home initialized nonempty literals.
//! - Different caller/callee realms without forbidding the body splice.
//! - Moving element storage never keeps an interior pointer across collection.
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
