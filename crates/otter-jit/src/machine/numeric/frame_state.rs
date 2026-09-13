//! Exact HIR activation chains, liveness and source-owned deopt positions.
//!
//! # Contents
//! - VM-owned caller/callee frames with SSA register and activation operands.
//! - Complete frame operand enumeration for deopt and moving-GC liveness.
//! - Resume-PC lookup over the root and its prepared inline candidate graph.
//!
//! # Invariants
//! - Function identity selects bytecode, never another function's equal byte PC.
//! - This lookup reads no feedback, binding, property or literal metadata. Those
//!   facts belong to each inline site's own snapshot during body lowering.
//! - Iterative traversal deduplicates snapshot identities, not function ids:
//!   two contexts may prepare different nested candidates for the same function.
//!
//! # See also
//! - `hir` — the graph whose SSA values these frames keep alive.
//! - `super::super::deopt` — post-allocation lowering of the same VM schema.

use super::hir::NumericValue;
use otter_vm::JitCompileSnapshot;
use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NumericFrameSlot {
    Value(NumericValue),
    Undefined,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct NumericFrameState {
    pub(super) point: NumericFramePoint,
    pub(super) frames: Box<[otter_vm::deopt::DeoptFrame<NumericFrameSlot>]>,
}

impl NumericFrameState {
    /// All semantic frame operands, including activation-only bindings.
    pub(super) fn frame_slots(&self) -> impl Iterator<Item = &NumericFrameSlot> {
        self.frames.iter().flat_map(|frame| {
            frame.slots.iter().chain(
                frame
                    .entry
                    .iter()
                    .flat_map(|entry| [&entry.this, &entry.closure]),
            )
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum NumericFramePoint {
    Node(NumericValue),
    Backedge { predecessor: usize, edge: usize },
}

/// How selection consumes a HIR frame state attached to one node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NumericFrameStatePurpose {
    /// A reachable pre-effect exact-deopt exit reconstructs the VM window.
    ExactDeopt,
    /// A committed effect needs complete live tagged roots but no deopt.
    TaggedRoots,
    /// An effect-once emitter uses the state only to publish source identity.
    RuntimeMetadata,
}

pub(super) fn resume_pc(root: &JitCompileSnapshot, function_id: u32, byte_pc: u32) -> Option<u32> {
    let mut pending = vec![root];
    let mut seen = BTreeSet::new();
    while let Some(view) = pending.pop() {
        if !seen.insert(std::ptr::from_ref(view)) {
            continue;
        }
        if view.code_block.id == function_id {
            return view
                .instructions
                .iter()
                .position(|instruction| instruction.byte_pc == byte_pc)
                .and_then(|pc| u32::try_from(pc).ok());
        }
        pending.extend(
            view.inline_callees
                .values()
                .map(|callee| callee.body.as_ref()),
        );
        let mut methods = view.inline_methods.values().collect::<Vec<_>>();
        while let Some(method) = methods.pop() {
            pending.push(method.body.as_ref());
            methods.extend(method.nested_methods.values());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_bytecode::Op;
    use otter_vm::jit::{JitInlineCallee, JitInlineMethod, JitMethodGuard, JitTestInstruction};
    use std::sync::Arc;

    #[test]
    fn inline_resume_positions_use_their_own_bytecode() {
        let instruction = |logical_pc, byte_pc| {
            JitTestInstruction::new(Op::ReturnUndefined, logical_pc, byte_pc, vec![])
        };
        let mut root = JitCompileSnapshot::without_feedback(
            1,
            0,
            0,
            vec![instruction(0, 0), instruction(1, 8)],
        );
        let child = JitCompileSnapshot::without_feedback(2, 0, 0, vec![instruction(0, 8)]);
        root.inline_callees.insert(
            8,
            JitInlineCallee {
                body: Arc::new(child),
            },
        );
        assert_eq!(resume_pc(&root, 1, 8), Some(1));
        assert_eq!(resume_pc(&root, 2, 8), Some(0));
        assert_eq!(resume_pc(&root, 2, 0), None);
        assert_eq!(resume_pc(&root, 3, 8), None);
        let method = |function_id, byte_pc| JitInlineMethod {
            body: Arc::new(JitCompileSnapshot::without_feedback(
                function_id,
                0,
                0,
                vec![instruction(0, byte_pc)],
            )),
            guard: JitMethodGuard {
                method_fid: function_id,
                recv_shape: 0,
                proto_chain: vec![],
                method_value_byte: 0,
            },
            prop_offsets: Default::default(),
            prop_shapes: Default::default(),
            nested_methods: Default::default(),
        };
        let mut parent = method(3, 12);
        parent.nested_methods.insert(12, method(4, 16));
        root.inline_methods.insert(8, parent);
        assert_eq!(resume_pc(&root, 3, 12), Some(0));
        assert_eq!(resume_pc(&root, 4, 16), Some(0));
        assert_eq!(resume_pc(&root, 4, 8), None);
    }
}
