//! Execution feedback and current operand proofs at graph construction.
//!
//! # Contents
//! - Cold property sites retain exact insufficient-feedback exits.
//! - Executed sites without CacheIR programs lower to committed runtime probes.
//! - Concat speculation guards the actual String before converting a Number.
//!
//! # Invariants
//! The tests inspect the graph before allocation or emission, so their result
//! depends only on source-owned execution feedback, never host timing or GC.
//!
//! # See also
//! - [`super::build`] for the production graph builder.

use std::rc::Rc;

use otter_bytecode::{Op, Operand};
use otter_vm::{JitCompileSnapshot, jit::JitTestInstruction};

use super::{DeoptReason, Kind, build};
use crate::graph::{BaselineSupport, bytecode::Analysis};

fn property_graph(op: Op, attempted: bool) -> super::Built {
    let operands = match op {
        Op::LoadProperty => vec![
            Operand::Register(2),
            Operand::Register(0),
            Operand::ConstIndex(0),
        ],
        Op::StoreProperty | Op::StorePropertyStrict => vec![
            Operand::Register(0),
            Operand::ConstIndex(0),
            Operand::Register(1),
            Operand::Register(2),
        ],
        _ => panic!("named property fixture"),
    };
    let mut view = JitCompileSnapshot::without_feedback(
        90,
        2,
        3,
        vec![
            JitTestInstruction::new(op, 0, 0, operands),
            JitTestInstruction::new(Op::ReturnUndefined, 1, 4, vec![]),
        ],
    );
    if attempted {
        view.seed_property_attempted_for_test(0);
    }
    let analysis = Rc::new(Analysis::build(&view).expect("property analysis"));
    let baseline = BaselineSupport {
        supported: vec![true; view.instructions.len()],
    };
    build(&view, &analysis, &baseline, None).expect("property graph")
}

#[test]
fn cold_property_sites_leave_before_the_operation_for_feedback() {
    for op in [Op::LoadProperty, Op::StoreProperty, Op::StorePropertyStrict] {
        let built = property_graph(op, false);
        assert!(
            built.graph.nodes.iter().any(|node| {
                matches!(node.kind, Kind::Deopt(DeoptReason::InsufficientFeedback))
            })
        );
    }
}

#[test]
fn executed_empty_cache_sites_use_committed_runtime_probes() {
    for op in [Op::LoadProperty, Op::StoreProperty, Op::StorePropertyStrict] {
        let built = property_graph(op, true);
        assert!(
            !built.graph.nodes.iter().any(|node| {
                matches!(node.kind, Kind::Deopt(DeoptReason::InsufficientFeedback))
            })
        );
        assert!(built.graph.nodes.iter().any(|node| matches!(
            (op, &node.kind),
            (Op::LoadProperty, Kind::LoadPropertyCached { .. })
                | (Op::StoreProperty, Kind::StorePropertyCached { .. })
                | (Op::StorePropertyStrict, Kind::Generic { .. })
        )));
    }
}

#[test]
fn concat_number_conversion_requires_a_current_string_guard() {
    use otter_vm::jit_feedback::{ARITH_INT32, ARITH_STRING, ArithFeedback};

    for number_first in [false, true] {
        let pair = if number_first { [2, 0] } else { [0, 2] };
        let mut view = JitCompileSnapshot::without_feedback(
            90,
            1,
            4,
            vec![
                JitTestInstruction::new(
                    Op::LoadInt32,
                    0,
                    0,
                    vec![Operand::Register(2), Operand::Imm32(1)],
                ),
                JitTestInstruction::new(
                    Op::Add,
                    1,
                    4,
                    vec![
                        Operand::Register(3),
                        Operand::Register(pair[0]),
                        Operand::Register(pair[1]),
                    ],
                ),
                JitTestInstruction::new(Op::ReturnValue, 2, 8, vec![Operand::Register(3)]),
            ],
        );
        view.seed_arith_feedback_for_test(1, ArithFeedback::from_bits(ARITH_INT32 | ARITH_STRING));
        let analysis = Rc::new(Analysis::build(&view).expect("concat analysis"));
        let baseline = BaselineSupport {
            supported: vec![true; 3],
        };
        let built = build(&view, &analysis, &baseline, None).expect("concat graph");
        let nodes = &built.graph.nodes;
        let guard = nodes
            .iter()
            .position(|node| node.kind == Kind::CheckString)
            .expect("guard the unknown operand, not the historical feedback");
        let conversion = nodes
            .iter()
            .position(|node| node.kind == Kind::NumberToString)
            .expect("retain cached number conversion");
        let add = nodes
            .iter()
            .position(|node| matches!(node.kind, Kind::PrimitiveAdd { .. }))
            .expect("retain native primitive addition");
        assert!(guard < conversion && conversion < add);
        assert_eq!(nodes[guard].pc, 1);
        assert_eq!(nodes[guard].eager, nodes[conversion].eager);
        assert!(!nodes[guard].kind.properties().may_collect);
        assert!(matches!(
            nodes[nodes[guard].inputs[0].0 as usize].kind,
            Kind::InitialRegister(0)
        ));
    }
}
