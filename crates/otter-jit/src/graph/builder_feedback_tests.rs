//! Property execution feedback at the graph construction boundary.
//!
//! # Contents
//! - Cold property sites retain exact insufficient-feedback exits.
//! - Executed sites without CacheIR programs lower to committed runtime probes.
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
            (
                Op::LoadProperty,
                Kind::LoadPropertyCached { atom: None, .. }
            ) | (
                Op::StoreProperty,
                Kind::StorePropertyCached { atom: None, .. }
            ) | (Op::StorePropertyStrict, Kind::Generic { .. })
        )));
    }
}
