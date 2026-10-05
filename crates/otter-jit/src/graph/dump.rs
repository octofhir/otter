//! Complete deterministic Graph declarations for opt-in compile artifacts.
//!
//! # Contents
//! - [`Graph::dump`] declares constants and lists blocks in emission order.
//! - Tests cover literal representations, recovery-only inputs and rollback.
//!
//! # Invariants
//! - Constants are declared once in arena order, before their block consumers.
//! - Block phis, body and control retain their layout and eager frame chains.
//! - Constant declarations describe values materialized at uses; they do not
//!   fabricate instruction regions or native code offsets.
//! - Formatting runs only for a requested artifact or an explicit test dump.
//!
//! # See also
//! - [`super::ir`] owns the graph and canonical constant interning.
//! - [`super::frame`] owns recovery locations, separately from readable IR.

use super::ir::{BlockId, Graph, Kind};

impl Graph {
    /// Declare constants, then list the blocks in `layout`, for tests and
    /// opt-in artifacts. Constants have no block or standalone emitted region.
    pub(crate) fn dump(&self, layout: &[BlockId]) -> String {
        use std::fmt::Write;
        let mut out = String::from("; otter graph\n");
        // Constants live outside blocks. Declare them in arena order so every
        // input and recovery-only literal has a definition in the artifact.
        for (index, node) in self.nodes.iter().enumerate() {
            if matches!(
                node.kind,
                Kind::ConstTagged(_) | Kind::ConstInt32(_) | Kind::ConstFloat64(_)
            ) {
                debug_assert!(node.block.is_none() && node.inputs.is_empty());
                let _ = writeln!(out, "  v{index} = {:?} [] {:?}", node.kind, node.repr);
            }
        }
        for (index, group) in self.allocation_groups.iter().enumerate() {
            let _ = writeln!(
                out,
                "; allocation-group {index} realm={} bytes={}",
                group.realm, group.bytes
            );
            for member in &group.members {
                let _ = writeln!(
                    out,
                    "; member v{} offset={} layout={:?}",
                    member.node.0, member.byte, member.layout
                );
            }
        }
        for &block in layout {
            let data = self.block(block);
            let _ = writeln!(
                out,
                "b{}{} preds={:?}",
                block.0,
                if data.is_loop { " loop" } else { "" },
                data.predecessors.iter().map(|b| b.0).collect::<Vec<_>>()
            );
            for &node in data
                .phis
                .iter()
                .chain(&data.body)
                .chain(data.control.iter())
            {
                let n = self.node(node);
                let _ = writeln!(
                    out,
                    "  v{} = {:?} {:?} {:?}{}",
                    node.0,
                    n.kind,
                    n.inputs.iter().map(|i| i.0).collect::<Vec<_>>(),
                    n.repr,
                    n.eager.map_or(String::new(), |s| format!(
                        " eager={:?}",
                        self.state_chain(s)
                            .iter()
                            .map(|&state| self
                                .frame_state(state)
                                .registers
                                .iter()
                                .map(|(r, v)| (*r, v.0))
                                .collect::<Vec<_>>())
                            .collect::<Vec<_>>()
                    ))
                );
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::super::ir::{DeoptReason, FrameState, Repr};
    use super::*;

    #[test]
    fn literals_are_defined_once_before_block_consumers_in_arena_order() {
        let mut graph = Graph::default();
        let tagged = graph.constant(Kind::ConstTagged(42));
        let int = graph.constant(Kind::ConstInt32(-7));
        let negative_zero = graph.constant(Kind::ConstFloat64((-0.0_f64).to_bits()));
        let nan = graph.constant(Kind::ConstFloat64(0x7ff8_0000_0000_0137));
        assert_eq!(graph.constant(Kind::ConstTagged(42)), tagged);
        let block = graph.new_block();
        let literal = graph.add_node(
            Kind::NewArrayLiteral,
            &[tagged, int, negative_zero, nan],
            Repr::Tagged,
        );
        graph.node_mut(literal).block = Some(block);
        graph.block_mut(block).body.push(literal);
        let dump = graph.dump(&[block]);
        let expected = format!(
            "; otter graph\n  v0 = ConstTagged(42) [] Tagged\n  v1 = ConstInt32(-7) [] Int32\n  v2 = ConstFloat64({}) [] Float64\n  v3 = ConstFloat64({}) [] Float64\nb0 preds=[]\n  v4 = NewArrayLiteral [0, 1, 2, 3] Tagged\n",
            (-0.0_f64).to_bits(),
            0x7ff8_0000_0000_0137_u64
        );
        assert_eq!(dump, expected);
        assert_eq!(graph.dump(&[block]), dump);
    }

    #[test]
    fn recovery_only_constant_is_declared_without_a_machine_region() {
        let mut graph = Graph::default();
        let constant = graph.constant(Kind::ConstTagged(73));
        let block = graph.new_block();
        let state = graph.add_frame_state(FrameState {
            function_id: 7,
            pc: 3,
            byte_pc: 5,
            register_count: 1,
            registers: vec![(0, constant)],
            caller: None,
        });
        let node = graph.add_node(Kind::Deopt(DeoptReason::WrongType), &[], Repr::None);
        graph.node_mut(node).eager = Some(state);
        graph.node_mut(node).block = Some(block);
        graph.block_mut(block).control = Some(node);
        let dump = graph.dump(&[block]);
        assert!(dump.contains("v0 = ConstTagged(73) [] Tagged\n"));
        assert!(dump.contains("eager=[[(0, 0)]]"));
        assert_eq!(graph.block(block).body.len(), 0);
        assert_eq!(graph.block(block).control, Some(node));
    }

    #[test]
    fn rollback_replaces_the_discarded_literal_declaration() {
        let mut graph = Graph::default();
        let block = graph.new_block();
        let checkpoint = graph.checkpoint(block);
        let discarded = graph.constant(Kind::ConstInt32(111));
        graph.rollback(checkpoint);
        let replacement = graph.constant(Kind::ConstInt32(222));
        assert_eq!(replacement, discarded);
        assert_eq!(
            graph.dump(&[block]),
            "; otter graph\n  v0 = ConstInt32(222) [] Int32\nb0 preds=[]\n"
        );
    }
}
