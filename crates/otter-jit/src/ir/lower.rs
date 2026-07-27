//! Lowering of bytecode nodes into the SSA guard vocabulary.
//!
//! A bytecode opcode says what a site *means*; a cache program says what the
//! site *does* once its feedback has settled. This module turns the second into
//! primitive nodes, so the guard a site runs is an ordinary instruction an
//! optimization pass can see, move, or delete.
//!
//! # Contents
//! - [`lower_settled_property_accesses`] — a monomorphic settled `LoadProperty`
//!   or `StoreProperty` becomes [`SsaOp::LoadHeader`], [`SsaOp::CheckShape`] and
//!   [`SsaOp::LoadField`] or [`SsaOp::StoreField`].
//! - [`eliminate_redundant_checks`] — a second derivation of a holder, or a
//!   second proof of a hidden class already established, is deleted.
//!
//! # Invariants
//! - A replaced instruction keeps its result value, its result register and its
//!   canonical PC, so every later analysis keyed by `(frame, PC)` still finds
//!   the site. Creating or deleting a definition renumbers identities back into
//!   canonical order.
//! - The nodes a site lowers to carry that site's PC, which is the exact-PC
//!   deopt state the whole group resumes at.
//! - Only the root frame lowers: the settled-site maps are keyed by the root
//!   body's byte PC, and a spliced callee's byte PCs collide with them.
//! - A site whose cache program reaches past the receiver — a prototype hop, a
//!   polymorphic chain, an exotic `length` — keeps its bytecode node. Those
//!   need the cache cell the node vocabulary does not describe.
//! - A store lowers only when its value is a proven `Int32`. Anything else may
//!   be a heap cell, which owes the generational write barrier, or a double,
//!   which owes a heap number: both are calls, and a call is what the lowered
//!   form exists to avoid.
//!
//! # See also
//! - [`super::ssa`] — the vocabulary and its verifier.
//! - [`otter_vm::JitInlinePropertyLoad`] — the settled site description.

use std::collections::BTreeMap;

use otter_bytecode::{Op, opcode_schema::opcode_schema};
use otter_vm::JitCompileSnapshot;
use smallvec::SmallVec;

use super::{
    cfg::ControlFlowGraph,
    inline::{InlineId, InlineTree},
    repr::{ReprMap, Representation},
    ssa::{SsaFunction, SsaInstr, SsaOp, ValueData, ValueDef, ValueId},
};

/// Replace every settled monomorphic property access with its primitive nodes.
pub fn lower_settled_property_accesses(
    ssa: &mut SsaFunction,
    cfg: &ControlFlowGraph,
    view: &JitCompileSnapshot,
    tree: &InlineTree,
    reprs: &ReprMap,
) {
    if view.cage_base == 0 || (view.property_loads.is_empty() && view.property_stores.is_empty()) {
        return;
    }
    let root = &tree.frames[InlineId::ROOT.0 as usize];
    let mut lowered_any = false;
    for block_index in 0..ssa.blocks.len() {
        if !ssa.blocks[block_index]
            .instrs
            .iter()
            .any(|instruction| settled_slot(view, root, reprs, instruction).is_some())
        {
            continue;
        }
        lowered_any = true;
        let source = std::mem::take(&mut ssa.blocks[block_index].instrs);
        let mut lowered = Vec::with_capacity(source.len() + 2);
        for instruction in source {
            let Some((shape, byte, writes)) = settled_slot(view, root, reprs, &instruction) else {
                lowered.push(instruction);
                continue;
            };
            let result = instruction
                .result
                .expect("a settled property access writes one register");
            // The holder address is a value of its own, so the guard and the
            // access that trust it need not be adjacent and the allocator, not a
            // register convention, decides where it lives.
            let header = append_header_value(ssa, &instruction, block_index);
            let access = if writes {
                SsaOp::StoreField { byte }
            } else {
                SsaOp::LoadField { byte }
            };
            let mut operands: SmallVec<[ValueId; 4]> = SmallVec::from_slice(&[header]);
            let mut operand_registers = SmallVec::new();
            if writes {
                // The stored value keeps its operand and its source register;
                // only the receiver folds into the holder.
                operands.push(instruction.inputs[1]);
                operand_registers.push(instruction.input_registers[1]);
            }
            let ValueDef::Op { op, inputs, .. } = &mut ssa.values[result.0 as usize].def else {
                unreachable!("an instruction result is defined by its instruction");
            };
            *op = access;
            *inputs = operands.to_vec().into_boxed_slice();
            lowered.push(SsaInstr {
                op: SsaOp::LoadHeader,
                inputs: SmallVec::from_slice(&[instruction.inputs[0]]),
                input_registers: SmallVec::from_slice(&[instruction.input_registers[0]]),
                result: Some(header),
                result_register: None,
                ..instruction.clone()
            });
            lowered.push(SsaInstr {
                op: SsaOp::CheckShape { shape },
                inputs: SmallVec::from_slice(&[header]),
                input_registers: SmallVec::new(),
                result: None,
                result_register: None,
                ..instruction.clone()
            });
            lowered.push(SsaInstr {
                op: access,
                inputs: operands,
                input_registers: operand_registers,
                ..instruction
            });
        }
        ssa.blocks[block_index].instrs = lowered;
    }
    if lowered_any {
        ssa.renumber_values(cfg);
        eliminate_redundant_checks(ssa, cfg);
    }
}

/// Append the holder-address value one lowered site defines.
///
/// Its identity is provisional: [`SsaFunction::renumber_values`] restores the
/// canonical dense order once every site is lowered.
fn append_header_value(
    ssa: &mut SsaFunction,
    instruction: &SsaInstr,
    block_index: usize,
) -> ValueId {
    let id = ValueId(
        u32::try_from(ssa.values.len()).expect("a lowered graph fits the value identity space"),
    );
    ssa.values.push(ValueData {
        id,
        def: ValueDef::Op {
            inline: instruction.inline,
            pc: instruction.pc,
            op: SsaOp::LoadHeader,
            inputs: Box::new([instruction.inputs[0]]),
        },
        def_block: ssa.blocks[block_index].id,
    });
    id
}

/// Delete a holder derivation, or a hidden-class proof, that a preceding one in
/// the same block already established.
///
/// Straight-line scope on purpose: a raw holder address may not outlive its
/// block, and anything that can allocate, call, or write the heap can move the
/// object or change its hidden class, so both facts die at such an instruction.
/// What survives is exactly what a receiver touched several times in one
/// effect-free stretch would otherwise re-prove.
pub fn eliminate_redundant_checks(ssa: &mut SsaFunction, cfg: &ControlFlowGraph) {
    let mut deleted_any = false;
    for block_index in 0..ssa.blocks.len() {
        // A holder available for a receiver, and the classes already proven of
        // each available holder.
        let mut holder_of = BTreeMap::<ValueId, ValueId>::new();
        let mut proven = BTreeMap::<ValueId, u32>::new();
        let mut replacement = BTreeMap::<ValueId, ValueId>::new();
        let source = std::mem::take(&mut ssa.blocks[block_index].instrs);
        let mut kept = Vec::with_capacity(source.len());
        for mut instruction in source {
            for input in &mut instruction.inputs {
                if let Some(&existing) = replacement.get(input) {
                    *input = existing;
                }
            }
            match instruction.op {
                SsaOp::LoadHeader => {
                    let receiver = instruction.inputs[0];
                    let header = instruction
                        .result
                        .expect("a holder derivation defines its address");
                    if let Some(&existing) = holder_of.get(&receiver) {
                        replacement.insert(header, existing);
                        deleted_any = true;
                        continue;
                    }
                    holder_of.insert(receiver, header);
                }
                SsaOp::CheckShape { shape } => {
                    let header = instruction.inputs[0];
                    if proven.get(&header) == Some(&shape) {
                        deleted_any = true;
                        continue;
                    }
                    proven.insert(header, shape);
                }
                // A settled write keeps the class it wrote through, and cannot
                // move the object: both facts survive it.
                SsaOp::LoadField { .. } | SsaOp::StoreField { .. } => {}
                SsaOp::Bytecode(op) => {
                    // Anything that can run arbitrary code, allocate, or write
                    // the heap invalidates both a raw address and a proven
                    // class.
                    if opcode_schema(op).effects.safepoint_required {
                        holder_of.clear();
                        proven.clear();
                    }
                }
            }
            if let Some(result) = instruction.result
                && let ValueDef::Op { inputs, .. } = &mut ssa.values[result.0 as usize].def
            {
                *inputs = instruction.inputs.to_vec().into_boxed_slice();
            }
            kept.push(instruction);
        }
        ssa.blocks[block_index].instrs = kept;
    }
    if deleted_any {
        ssa.renumber_values(cfg);
    }
}

/// The single own slot a property site has settled on, and whether it writes.
fn settled_slot(
    view: &JitCompileSnapshot,
    root: &super::inline::InlineFrame,
    reprs: &ReprMap,
    instruction: &SsaInstr,
) -> Option<(u32, u32, bool)> {
    if instruction.inline != InlineId::ROOT
        || instruction.result.is_none()
        || instruction.result_register.is_none()
    {
        return None;
    }
    let writes = match instruction.op {
        SsaOp::Bytecode(Op::LoadProperty)
            if instruction.inputs.len() == 1 && instruction.input_registers.len() == 1 =>
        {
            false
        }
        // Only a proven `Int32` may be written inline: the compressed slot
        // takes it whole, so there is no box to allocate and no cell to
        // barrier.
        SsaOp::Bytecode(Op::StoreProperty)
            if instruction.inputs.len() == 2
                && instruction.input_registers.len() == 2
                && reprs.representation(instruction.inputs[1]) == Representation::Int32 =>
        {
            true
        }
        _ => return None,
    };
    let metadata = root.instructions.get(instruction.pc as usize)?;
    // A dense array's or a primitive string's `.length` is not an own data
    // slot, so no settled slot can stand in for it.
    if metadata.load_array_length {
        return None;
    }
    let sites = if writes {
        &view.property_stores
    } else {
        &view.property_loads
    };
    let [only] = sites.get(&metadata.byte_pc)?.as_slice() else {
        return None;
    };
    // Zero is the empty-hidden-class sentinel: no receiver ever carries it, so
    // a check against it could never hold.
    (only.receiver_shape != 0).then_some((only.receiver_shape, only.value_byte, writes))
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
        let reprs = ReprMap::compute(&tree, &ssa);
        lower_settled_property_accesses(&mut ssa, &cfg, &view, &tree, &reprs);
        (cfg, ssa, tree)
    }

    #[test]
    fn a_monomorphic_settled_site_becomes_a_holder_a_check_and_a_read() {
        let (cfg, ssa, _tree) = analyzed(vec![JitInlinePropertyLoad {
            receiver_shape: 7,
            value_byte: 24,
        }]);
        let instrs = &ssa.blocks[0].instrs;
        assert_eq!(instrs[0].op, SsaOp::LoadHeader);
        assert_eq!(instrs[1].op, SsaOp::CheckShape { shape: 7 });
        assert_eq!(instrs[2].op, SsaOp::LoadField { byte: 24 });
        // Every node carries the site's PC; the guard and the read consume the
        // holder the first node defines, and only the read writes a register.
        assert_eq!((instrs[0].pc, instrs[1].pc, instrs[2].pc), (0, 0, 0));
        let header = instrs[0].result.expect("the holder is a value");
        assert_eq!(instrs[0].result_register, None);
        assert_eq!(instrs[1].inputs.as_slice(), [header]);
        assert_eq!(instrs[2].inputs.as_slice(), [header]);
        assert_eq!(instrs[1].result, None);
        assert_eq!(instrs[2].result_register, Some(1));
        // Identities stay dense and in construction order after the insertion.
        assert!(
            ssa.values
                .iter()
                .enumerate()
                .all(|(index, value)| value.id == ValueId(index as u32))
        );

        ssa.verify(&cfg, &DominatorTree::compute(&cfg))
            .expect("the lowered graph verifies");
    }

    #[test]
    fn a_second_access_to_one_receiver_keeps_one_holder_and_one_check() {
        let mut view = JitCompileSnapshot::without_feedback(
            0,
            1,
            3,
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
                JitTestInstruction::new(
                    Op::LoadProperty,
                    1,
                    4,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::ConstIndex(1),
                    ],
                ),
                JitTestInstruction::new(Op::ReturnValue, 2, 8, vec![Operand::Register(2)]),
            ],
        );
        view.cage_base = 0x1000;
        view.property_loads.insert(
            0,
            vec![JitInlinePropertyLoad {
                receiver_shape: 7,
                value_byte: 24,
            }],
        );
        view.property_loads.insert(
            4,
            vec![JitInlinePropertyLoad {
                receiver_shape: 7,
                value_byte: 32,
            }],
        );
        let tree = InlineTree::trivial(&view);
        let cfg = ControlFlowGraph::build_inlined(&tree).expect("CFG builds");
        let mut ssa = SsaFunction::build_inlined(&tree, &cfg).expect("SSA builds");
        let reprs = ReprMap::compute(&tree, &ssa);
        lower_settled_property_accesses(&mut ssa, &cfg, &view, &tree, &reprs);

        let ops: Vec<_> = ssa.blocks[0].instrs.iter().map(|i| i.op).collect();
        assert_eq!(
            ops,
            vec![
                SsaOp::LoadHeader,
                SsaOp::CheckShape { shape: 7 },
                SsaOp::LoadField { byte: 24 },
                SsaOp::LoadField { byte: 32 },
                SsaOp::Bytecode(Op::ReturnValue),
            ]
        );
        // The second read consumes the first holder.
        let header = ssa.blocks[0].instrs[0]
            .result
            .expect("the holder is a value");
        assert_eq!(ssa.blocks[0].instrs[3].inputs.as_slice(), [header]);

        ssa.verify(&cfg, &DominatorTree::compute(&cfg))
            .expect("the eliminated graph verifies");
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
        ssa.blocks[0].instrs.remove(1);
        assert_eq!(
            ssa.verify(&cfg, &DominatorTree::compute(&cfg)),
            Err(SsaError::LoadFieldWithoutCheckShape { pc: 0 })
        );
    }
}
