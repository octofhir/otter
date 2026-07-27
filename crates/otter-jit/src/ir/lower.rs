//! Lowering of bytecode nodes into the SSA guard vocabulary.
//!
//! A bytecode opcode says what a site *means*; a cache program says what the
//! site *does* once its feedback has settled. This module turns the second into
//! primitive nodes, so the guard a site runs is an ordinary instruction an
//! optimization pass can see, move, or delete.
//!
//! # Contents
//! - [`lower_settled_property_loads`] — a monomorphic settled `LoadProperty`
//!   becomes [`SsaOp::CheckShape`] followed by [`SsaOp::LoadField`].
//!
//! # Invariants
//! - Lowering preserves dense value identity and order: a replaced instruction
//!   keeps its result value, its result register, and its canonical PC, so
//!   every later analysis keyed by `(frame, PC)` still finds the site.
//! - The nodes a site lowers to are adjacent and carry that site's PC, which is
//!   the exact-PC deopt state the whole group resumes at.
//! - Only the root frame lowers: the settled-site maps are keyed by the root
//!   body's byte PC, and a spliced callee's byte PCs collide with them.
//! - A site whose cache program reaches past the receiver — a prototype hop, a
//!   polymorphic chain, an exotic `length` — keeps its bytecode node. Those
//!   need the cache cell the node vocabulary does not describe.
//!
//! # See also
//! - [`super::ssa`] — the vocabulary and its verifier.
//! - [`otter_vm::JitInlinePropertyLoad`] — the settled site description.

use otter_bytecode::Op;
use otter_vm::JitCompileSnapshot;

use super::{
    inline::{InlineId, InlineTree},
    ssa::{SsaFunction, SsaInstr, SsaOp, ValueDef},
};

/// Replace every settled monomorphic property load with its primitive nodes.
pub fn lower_settled_property_loads(
    ssa: &mut SsaFunction,
    view: &JitCompileSnapshot,
    tree: &InlineTree,
) {
    if view.cage_base == 0 || view.property_loads.is_empty() {
        return;
    }
    let root = &tree.frames[InlineId::ROOT.0 as usize];
    for block in &mut ssa.blocks {
        if !block
            .instrs
            .iter()
            .any(|instruction| settled_slot(view, root, instruction).is_some())
        {
            continue;
        }
        let mut lowered = Vec::with_capacity(block.instrs.len() + 1);
        for instruction in block.instrs.drain(..) {
            let Some((shape, byte)) = settled_slot(view, root, &instruction) else {
                lowered.push(instruction);
                continue;
            };
            let result = instruction
                .result
                .expect("a settled property load writes one register");
            let ValueDef::Op { op, .. } = &mut ssa.values[result.0 as usize].def else {
                unreachable!("an instruction result is defined by its instruction");
            };
            *op = SsaOp::LoadField { byte };
            lowered.push(SsaInstr {
                op: SsaOp::CheckShape { shape },
                result: None,
                result_register: None,
                ..instruction.clone()
            });
            lowered.push(SsaInstr {
                op: SsaOp::LoadField { byte },
                ..instruction
            });
        }
        block.instrs = lowered;
    }
}

/// The single own slot a property-load site has settled on, if it has one.
fn settled_slot(
    view: &JitCompileSnapshot,
    root: &super::inline::InlineFrame,
    instruction: &SsaInstr,
) -> Option<(u32, u32)> {
    if instruction.inline != InlineId::ROOT
        || instruction.op != SsaOp::Bytecode(Op::LoadProperty)
        || instruction.inputs.len() != 1
        || instruction.input_registers.len() != 1
        || instruction.result.is_none()
        || instruction.result_register.is_none()
    {
        return None;
    }
    let metadata = root.instructions.get(instruction.pc as usize)?;
    // A dense array's or a primitive string's `.length` is not an own data
    // slot, so no settled slot can stand in for it.
    if metadata.load_array_length {
        return None;
    }
    let [only] = view.property_loads.get(&metadata.byte_pc)?.as_slice() else {
        return None;
    };
    // Zero is the empty-hidden-class sentinel: no receiver ever carries it, so
    // a check against it could never hold.
    (only.receiver_shape != 0).then_some((only.receiver_shape, only.value_byte))
}

#[cfg(test)]
mod tests {
    use otter_bytecode::Operand;
    use otter_vm::{JitInlinePropertyLoad, jit::JitTestInstruction};

    use super::*;
    use crate::ir::{cfg::ControlFlowGraph, dom::DominatorTree, ssa::SsaError};

    /// `r1 = r0.name; return r1`, with the site's settled slot supplied.
    fn analyzed(
        settled: Vec<JitInlinePropertyLoad>,
    ) -> (ControlFlowGraph, SsaFunction, InlineTree) {
        let mut view = JitCompileSnapshot::without_feedback(
            0,
            1,
            2,
            vec![
                JitTestInstruction::new(
                    Op::LoadProperty,
                    0,
                    0,
                    vec![
                        Operand::Register(1),
                        Operand::Register(0),
                        Operand::ConstIndex(0),
                    ],
                ),
                JitTestInstruction::new(Op::ReturnValue, 1, 4, vec![Operand::Register(1)]),
            ],
        );
        view.cage_base = 0x1000;
        if !settled.is_empty() {
            view.property_loads.insert(0, settled);
        }
        let tree = InlineTree::trivial(&view);
        let cfg = ControlFlowGraph::build_inlined(&tree).expect("CFG builds");
        let mut ssa = SsaFunction::build_inlined(&tree, &cfg).expect("SSA builds");
        lower_settled_property_loads(&mut ssa, &view, &tree);
        (cfg, ssa, tree)
    }

    #[test]
    fn a_monomorphic_settled_site_becomes_a_check_and_a_field_read() {
        let (cfg, ssa, _tree) = analyzed(vec![JitInlinePropertyLoad {
            receiver_shape: 7,
            value_byte: 24,
        }]);
        let instrs = &ssa.blocks[0].instrs;
        assert_eq!(instrs[0].op, SsaOp::CheckShape { shape: 7 });
        assert_eq!(instrs[1].op, SsaOp::LoadField { byte: 24 });
        // Both nodes carry the site's PC and read the same receiver, and only
        // the read defines the site's value.
        assert_eq!((instrs[0].pc, instrs[1].pc), (0, 0));
        assert_eq!(instrs[0].inputs, instrs[1].inputs);
        assert_eq!(instrs[0].result, None);
        assert!(instrs[1].result.is_some());
        assert_eq!(instrs[1].result_register, Some(1));

        ssa.verify(&cfg, &DominatorTree::compute(&cfg))
            .expect("the lowered graph verifies");
    }

    #[test]
    fn a_polymorphic_site_keeps_its_bytecode_node() {
        let (_cfg, ssa, _tree) = analyzed(vec![
            JitInlinePropertyLoad {
                receiver_shape: 7,
                value_byte: 24,
            },
            JitInlinePropertyLoad {
                receiver_shape: 9,
                value_byte: 32,
            },
        ]);
        assert_eq!(
            ssa.blocks[0].instrs[0].op,
            SsaOp::Bytecode(Op::LoadProperty)
        );
    }

    #[test]
    fn an_unsettled_site_keeps_its_bytecode_node() {
        let (_cfg, ssa, _tree) = analyzed(Vec::new());
        assert_eq!(
            ssa.blocks[0].instrs[0].op,
            SsaOp::Bytecode(Op::LoadProperty)
        );
    }

    #[test]
    fn a_field_read_without_its_shape_check_is_rejected() {
        let (cfg, mut ssa, _tree) = analyzed(vec![JitInlinePropertyLoad {
            receiver_shape: 7,
            value_byte: 24,
        }]);
        ssa.blocks[0].instrs.remove(0);
        assert_eq!(
            ssa.verify(&cfg, &DominatorTree::compute(&cfg)),
            Err(SsaError::LoadFieldWithoutCheckShape { pc: 0 })
        );
    }
}
