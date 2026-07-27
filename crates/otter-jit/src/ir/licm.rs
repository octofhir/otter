//! Loop-invariant code motion for settled property accesses.
//!
//! A settled access on a receiver the loop never changes computes the same
//! value every iteration. This pass moves the whole access — the holder
//! derivation, the hidden-class guard and the field read — into the loop's
//! pre-header, so the loop body reads the value out of a register instead of
//! re-proving and re-loading it.
//!
//! # Contents
//! - [`hoist_loop_invariant_accesses`] — the pass.
//! - [`natural_loop_blocks`] — deterministic natural-loop membership.
//!
//! # Invariants
//! - The three nodes of an access move together. The holder is a raw interior
//!   pointer that may not cross a back edge, and moving the guard alone would
//!   leave the load in the loop, which is most of its cost.
//! - A hoisted node takes the PC of the instruction it is inserted before. The
//!   frame state of a PC is the state before the first node carrying it, so the
//!   group's deopt resumes at a PC that exists, re-executes that one
//!   instruction, and falls into the loop, where the interpreter performs the
//!   access itself.
//! - The holder derivation is re-pointed at the receiver's copy origin, which
//!   is defined outside the loop; the value it named inside the loop does not
//!   dominate the pre-header.
//! - Entering a hoisted loop by OSR would skip the pre-header that computes the
//!   values it reads, so the unit offers no OSR entry into a loop it hoisted.
//! - A loop body never post-dominates its pre-header, so a hoisted guard runs
//!   on paths that would not have reached it. That is speculation. The VM
//!   records the PCs a function's optimized code deoptimized at, and a group
//!   that would land on one is not moved: nothing else resumes at a
//!   pre-header's terminating PC, so such a record is this pass's own failure
//!   coming back.
//!
//! # See also
//! - [`super::lower`] — the lowering whose groups this moves.
//! - [`super::ssa::SsaFunction::copy_origin`] — the invariance question.

use std::collections::BTreeSet;

use otter_bytecode::Op;

use super::{
    cfg::{BlockId, ControlFlowGraph},
    dom::DominatorTree,
    inline::InlineId,
    ssa::{SsaFunction, SsaInstr, SsaOp, ValueDef, ValueId},
};

/// Blocks of the natural loop closed by the back edge `latch -> header`.
pub fn natural_loop_blocks(
    cfg: &ControlFlowGraph,
    latch: BlockId,
    header: BlockId,
) -> BTreeSet<BlockId> {
    let mut blocks = BTreeSet::from([header, latch]);
    let mut pending = vec![latch];
    while let Some(block) = pending.pop() {
        for predecessor in cfg.blocks[block.0 as usize].preds.iter().copied() {
            if blocks.insert(predecessor) && predecessor != header {
                pending.push(predecessor);
            }
        }
    }
    blocks
}

/// Move every loop-invariant settled access into its loop's pre-header.
///
/// `bail_pcs` are the PCs an earlier generation of this function deoptimized
/// at. A group placed at one of them already failed this speculation once, and
/// its loop is left alone.
pub fn hoist_loop_invariant_accesses(
    ssa: &mut SsaFunction,
    cfg: &ControlFlowGraph,
    dom: &DominatorTree,
    bail_pcs: &BTreeSet<u32>,
) -> BTreeSet<BlockId> {
    let mut hoisted = BTreeSet::new();
    for (latch, header) in back_edges(cfg, dom) {
        let Some(plan) = plan_loop(ssa, cfg, dom, latch, header) else {
            continue;
        };
        if bail_pcs.contains(&plan.insert_pc) {
            continue;
        }
        if hoist(ssa, &plan) {
            hoisted.insert(header);
        }
    }
    if !hoisted.is_empty() {
        ssa.renumber_values(cfg);
    }
    hoisted
}

/// Every `latch -> header` edge whose target dominates its source.
fn back_edges(cfg: &ControlFlowGraph, dom: &DominatorTree) -> Vec<(BlockId, BlockId)> {
    let mut edges = Vec::new();
    for block in &cfg.blocks {
        for &successor in &block.normal_succs {
            if dom.dominates(successor, block.id) {
                edges.push((block.id, successor));
            }
        }
    }
    edges.sort_unstable();
    edges.dedup();
    edges
}

/// One loop's pre-header and the holders that may move into it.
struct LoopPlan {
    header: BlockId,
    preheader: BlockId,
    /// Position of the pre-header among the header's normal predecessors.
    preheader_edge: usize,
    /// Index in the pre-header before which the holders are inserted.
    insert_at: usize,
    /// PC the inserted nodes take, which is the PC of the instruction at
    /// [`Self::insert_at`].
    insert_pc: u32,
    holders: Vec<HoistedHolder>,
}

/// One holder derivation and everything in the loop that trusts it.
struct HoistedHolder {
    block: BlockId,
    /// Ascending indices into the block: the derivation, its guard, and every
    /// read resolved against it. A holder is block-local, so they are all here.
    indices: Vec<usize>,
    /// The receiver value the derivation is re-pointed at.
    origin: ValueId,
    /// Register naming [`Self::origin`] at the insertion point.
    origin_register: u16,
}

/// Decide what, if anything, this loop may hoist.
fn plan_loop(
    ssa: &SsaFunction,
    cfg: &ControlFlowGraph,
    dom: &DominatorTree,
    latch: BlockId,
    header: BlockId,
) -> Option<LoopPlan> {
    if cfg.blocks[header.0 as usize].inline != InlineId::ROOT {
        return None;
    }
    let blocks = natural_loop_blocks(cfg, latch, header);
    if blocks
        .iter()
        .any(|block| cfg.blocks[block.0 as usize].inline != InlineId::ROOT)
    {
        return None;
    }
    // The pre-header is the one predecessor of the header the header does not
    // dominate. More than one, and there is no single place a hoisted node
    // would dominate the loop from.
    let mut preheaders = cfg.blocks[header.0 as usize]
        .preds
        .iter()
        .copied()
        .filter(|&predecessor| !dom.dominates(header, predecessor));
    let preheader = preheaders.next()?;
    if preheaders.next().is_some() || cfg.blocks[preheader.0 as usize].inline != InlineId::ROOT {
        return None;
    }
    // Nothing in the loop may write the heap or run anything: a hoisted read
    // must observe the same slot on every iteration.
    if blocks.iter().any(|block| {
        ssa.blocks[block.0 as usize]
            .instrs
            .iter()
            .any(|instruction| !preserves_hoisted_reads(instruction))
    }) {
        return None;
    }
    // The holders are inserted before the pre-header's last instruction and take
    // its PC, so that instruction's own operands must survive the insertion.
    let preheader_instrs = &ssa.blocks[preheader.0 as usize].instrs;
    let insert_at = preheader_instrs.len().checked_sub(1)?;
    let insert_pc = preheader_instrs[insert_at].pc;
    if preheader_instrs[insert_at].inline != InlineId::ROOT {
        return None;
    }
    let trailing_reads: BTreeSet<u16> = preheader_instrs[insert_at..]
        .iter()
        .flat_map(|instruction| instruction.input_registers.iter().copied())
        .collect();

    let mut holders = Vec::new();
    for &block in &blocks {
        for index in 0..ssa.blocks[block.0 as usize].instrs.len() {
            if let Some(holder) = hoistable_holder(
                ssa,
                dom,
                &blocks,
                header,
                preheader,
                insert_at,
                &trailing_reads,
                block,
                index,
            ) {
                holders.push(holder);
            }
        }
    }
    let preheader_edge = cfg.blocks[header.0 as usize]
        .preds
        .iter()
        .position(|&predecessor| predecessor == preheader)?;
    (!holders.is_empty()).then_some(LoopPlan {
        header,
        preheader,
        preheader_edge,
        insert_at,
        insert_pc,
        holders,
    })
}

/// Whether an instruction leaves every hoisted read observing the same slot.
///
/// A primitive read observes the heap no more than the bytecode load it came
/// from; a primitive write does, and disqualifies the loop outright. Everything
/// else must be an opcode that computes over values it already holds, which is
/// the same set the property loop cache proved safe before this pass replaced
/// it.
fn preserves_hoisted_reads(instruction: &SsaInstr) -> bool {
    let op = match instruction.op {
        SsaOp::LoadHeader
        | SsaOp::LoadPrototype
        | SsaOp::CheckShape { .. }
        | SsaOp::LoadField { .. }
        | SsaOp::Reuse => return true,
        SsaOp::StoreField { .. } => return false,
        SsaOp::Bytecode(op) => op,
    };
    matches!(
        op,
        Op::LoadInt32
            | Op::LoadNumber
            | Op::LoadUndefined
            | Op::LoadNull
            | Op::LoadTrue
            | Op::LoadFalse
            | Op::LoadLocal
            | Op::StoreLocal
            | Op::LoadThis
            | Op::ToPrimitive
            | Op::ToNumeric
            | Op::Add
            | Op::Sub
            | Op::Mul
            | Op::Div
            | Op::Rem
            | Op::Neg
            | Op::Increment
            | Op::AddImm
            | Op::SubImm
            | Op::BitwiseAndImm
            | Op::LessThanImm
            | Op::EqualImm
            | Op::NotEqualImm
            | Op::LogicalNot
            | Op::BitwiseAnd
            | Op::BitwiseOr
            | Op::BitwiseXor
            | Op::Shl
            | Op::Shr
            | Op::Equal
            | Op::NotEqual
            | Op::LessThan
            | Op::LessEq
            | Op::GreaterThan
            | Op::GreaterEq
            | Op::Jump
            | Op::JumpIfTrue
            | Op::JumpIfFalse
            | Op::LoadProperty
    )
}

/// The holder derivation at `index` and its dependents, when they may move.
#[allow(clippy::too_many_arguments)]
fn hoistable_holder(
    ssa: &SsaFunction,
    dom: &DominatorTree,
    blocks: &BTreeSet<BlockId>,
    header: BlockId,
    preheader: BlockId,
    insert_at: usize,
    trailing_reads: &BTreeSet<u16>,
    block: BlockId,
    index: usize,
) -> Option<HoistedHolder> {
    let instrs = &ssa.blocks[block.0 as usize].instrs;
    let derivation = instrs.get(index)?;
    if derivation.op != SsaOp::LoadHeader {
        return None;
    }
    // A holder never leaves its block, so everything trusting it is here — and
    // a hop derives another holder whose own dependents come along with it.
    let mut holders = vec![derivation.result?];
    let mut indices = vec![index];
    for (position, instruction) in instrs.iter().enumerate().skip(index + 1) {
        if !instruction
            .inputs
            .first()
            .is_some_and(|input| holders.contains(input))
        {
            continue;
        }
        match instruction.op {
            SsaOp::CheckShape { .. } => {}
            SsaOp::LoadPrototype => holders.push(instruction.result?),
            SsaOp::LoadField { .. } => {
                let result_register = instruction.result_register?;
                // Re-executing the pre-header's trailing instruction after a
                // hoisted guard deoptimizes must see the operands it had.
                if trailing_reads.contains(&result_register) {
                    return None;
                }
                // Construction is unpruned, so a register written in the loop
                // has a header phi whether or not it is live there. That one is
                // repaired when the holder moves; a phi anywhere else merges
                // this register with something this pass does not reason about.
                if ssa.blocks.iter().any(|other| {
                    other.id != header
                        && other.phis.iter().any(|&phi| {
                            matches!(&ssa.values[phi.0 as usize].def, ValueDef::Phi { register, .. }
                                if *register == result_register)
                        })
                }) {
                    return None;
                }
            }
            // A write through this holder keeps it in the loop, and the loop
            // test above already refuses one.
            _ => return None,
        }
        indices.push(position);
    }
    // The receiver's origin has to be available in the pre-header, and the
    // register naming it there has to still hold it at the insertion point.
    let origin = ssa.copy_origin(derivation.inputs[0])?;
    let origin_block = ssa.values[origin.0 as usize].def_block;
    if blocks.contains(&origin_block) || !dom.dominates(origin_block, preheader) {
        return None;
    }
    let origin_register = defined_register(ssa, origin)?;
    let redefined = ssa.blocks[preheader.0 as usize].instrs[..insert_at]
        .iter()
        .rev()
        .find(|instruction| instruction.result_register == Some(origin_register))
        .map(|instruction| instruction.result);
    match redefined {
        Some(last) if last != Some(origin) => return None,
        None if origin_block == preheader
            && !ssa.blocks[preheader.0 as usize].phis.contains(&origin) =>
        {
            return None;
        }
        _ => {}
    }
    Some(HoistedHolder {
        block,
        indices,
        origin,
        origin_register,
    })
}

/// The interpreter register a value defines, if any names it.
fn defined_register(ssa: &SsaFunction, value: ValueId) -> Option<u16> {
    match &ssa.values[value.0 as usize].def {
        ValueDef::Param { register, .. }
        | ValueDef::Uninitialized { register }
        | ValueDef::ExceptionInput { register, .. }
        | ValueDef::InlineResult { register, .. }
        | ValueDef::Phi { register, .. } => Some(*register),
        ValueDef::Op { .. } => ssa
            .blocks
            .iter()
            .flat_map(|block| block.instrs.iter())
            .find(|instruction| instruction.result == Some(value))
            .and_then(|instruction| instruction.result_register),
        ValueDef::InlineUndefinedReturn { .. } => None,
    }
}

/// Append the value one rebind defines, at the site's own PC and block.
///
/// Its identity is provisional: [`SsaFunction::renumber_values`] restores the
/// canonical dense order once the pass is done.
fn append_rebound_value(ssa: &mut SsaFunction, read: &SsaInstr, block: BlockId) -> ValueId {
    let id = ValueId(
        u32::try_from(ssa.values.len()).expect("a hoisted graph fits the value identity space"),
    );
    let loaded = read.result.expect("a field read defines a value");
    ssa.values.push(super::ssa::ValueData {
        id,
        def: ValueDef::Op {
            inline: read.inline,
            pc: read.pc,
            op: SsaOp::Reuse,
            inputs: Box::new([loaded]),
        },
        def_block: block,
    });
    id
}

/// Move the planned holders into the pre-header.
///
/// Each read leaves a rebind behind at its own PC, so the block keeps every
/// source instruction it had and the register the interpreter would have
/// written still gets written there.
fn hoist(ssa: &mut SsaFunction, plan: &LoopPlan) -> bool {
    let mut moved: Vec<SsaInstr> = Vec::new();
    let mut rebound_values: Vec<(ValueId, ValueId)> = Vec::new();
    for holder in &plan.holders {
        let source = std::mem::take(&mut ssa.blocks[holder.block.0 as usize].instrs);
        let mut kept = Vec::with_capacity(source.len());
        let mut rebinds = Vec::new();
        for (index, mut instruction) in source.into_iter().enumerate() {
            if !holder.indices.contains(&index) {
                kept.push(instruction);
                continue;
            }
            if matches!(instruction.op, SsaOp::LoadField { .. }) {
                rebinds.push((kept.len(), instruction.clone()));
                kept.push(instruction.clone());
            }
            if instruction.op == SsaOp::LoadHeader {
                instruction.inputs[0] = holder.origin;
                instruction.input_registers[0] = holder.origin_register;
            }
            instruction.pc = plan.insert_pc;
            moved.push(instruction);
        }
        ssa.blocks[holder.block.0 as usize].instrs = kept;
        for (position, read) in rebinds {
            let rebound = append_rebound_value(ssa, &read, holder.block);
            let loaded = read.result.expect("a field read defines a value");
            rebound_values.push((loaded, rebound));
            ssa.blocks[holder.block.0 as usize].instrs[position] = SsaInstr {
                op: SsaOp::Reuse,
                inputs: smallvec::smallvec![loaded],
                input_registers: smallvec::SmallVec::new(),
                result: Some(rebound),
                ..read
            };
        }
    }
    if moved.is_empty() {
        return false;
    }
    // Everything in the loop now reads the rebind rather than the value that
    // left, so a frame state naming the register agrees with the operand.
    for block in &mut ssa.blocks {
        for instruction in &mut block.instrs {
            if instruction.op == SsaOp::Reuse {
                continue;
            }
            for input in &mut instruction.inputs {
                if let Some(&(_, rebound)) =
                    rebound_values.iter().find(|&&(loaded, _)| loaded == *input)
                {
                    *input = rebound;
                }
            }
        }
    }
    for index in 0..ssa.values.len() {
        if matches!(
            ssa.values[index].def,
            ValueDef::Op {
                op: SsaOp::Reuse,
                ..
            }
        ) {
            continue;
        }
        match &mut ssa.values[index].def {
            ValueDef::Op { inputs, .. }
            | ValueDef::Phi { inputs, .. }
            | ValueDef::InlineResult { inputs, .. } => {
                for input in inputs.iter_mut() {
                    if let Some(&(_, rebound)) =
                        rebound_values.iter().find(|&&(loaded, _)| loaded == *input)
                    {
                        *input = rebound;
                    }
                }
            }
            _ => {}
        }
    }
    for instruction in &moved {
        let Some(result) = instruction.result else {
            continue;
        };
        let data = &mut ssa.values[result.0 as usize];
        data.def_block = plan.preheader;
        if let ValueDef::Op { pc, inputs, .. } = &mut data.def {
            *pc = plan.insert_pc;
            *inputs = instruction.inputs.to_vec().into_boxed_slice();
        }
    }
    // Repair the header phi for every register a moved read defines. Before the
    // move the loop wrote it and the entry edge carried whatever preceded the
    // loop; now the pre-header writes it, so both edges carry the same value.
    let header_phis = ssa.blocks[plan.header.0 as usize].phis.clone();
    for instruction in &moved {
        let (Some(result), Some(register)) = (instruction.result, instruction.result_register)
        else {
            continue;
        };
        for &phi in &header_phis {
            let ValueDef::Phi {
                register: merged,
                inputs,
                ..
            } = &mut ssa.values[phi.0 as usize].def
            else {
                continue;
            };
            if *merged == register {
                inputs[plan.preheader_edge] = result;
            }
        }
    }
    ssa.blocks[plan.preheader.0 as usize]
        .instrs
        .splice(plan.insert_at..plan.insert_at, moved);
    true
}

#[cfg(test)]
mod tests {
    use otter_bytecode::Operand;
    use otter_vm::{JitCompileSnapshot, JitInlinePropertyLoad, jit::JitTestInstruction};

    use super::*;
    use crate::ir::{inline::InlineTree, lower::lower_settled_property_accesses, repr::ReprMap};

    /// `acc = 0; i = 0; while (i < 4) { acc = acc + r0.slot; i = i + 1 }`
    fn loop_with_one_settled_load() -> (ControlFlowGraph, SsaFunction, InlineTree) {
        let mut view = JitCompileSnapshot::without_feedback(
            0,
            1,
            6,
            vec![
                JitTestInstruction::new(
                    Op::LoadInt32,
                    0,
                    0,
                    vec![Operand::Register(1), Operand::Imm32(0)],
                ),
                JitTestInstruction::new(
                    Op::LoadInt32,
                    1,
                    4,
                    vec![Operand::Register(2), Operand::Imm32(0)],
                ),
                JitTestInstruction::new(Op::Jump, 2, 8, vec![Operand::Imm32(0)]),
                // header
                JitTestInstruction::new(
                    Op::LessThanImm,
                    3,
                    12,
                    vec![
                        Operand::Register(3),
                        Operand::Register(2),
                        Operand::Imm32(4),
                    ],
                ),
                JitTestInstruction::new(
                    Op::JumpIfFalse,
                    4,
                    16,
                    vec![Operand::Imm32(4), Operand::Register(3)],
                ),
                // body
                JitTestInstruction::new(
                    Op::LoadProperty,
                    5,
                    20,
                    vec![
                        Operand::Register(4),
                        Operand::Register(0),
                        Operand::ConstIndex(0),
                    ],
                ),
                JitTestInstruction::new(
                    Op::Add,
                    6,
                    24,
                    vec![
                        Operand::Register(1),
                        Operand::Register(1),
                        Operand::Register(4),
                    ],
                ),
                JitTestInstruction::new(
                    Op::AddImm,
                    7,
                    28,
                    vec![
                        Operand::Register(2),
                        Operand::Register(2),
                        Operand::Imm32(1),
                    ],
                ),
                JitTestInstruction::new(Op::Jump, 8, 32, vec![Operand::Imm32(-6)]),
                JitTestInstruction::new(Op::ReturnValue, 9, 36, vec![Operand::Register(1)]),
            ],
        );
        view.cage_base = 0x1000;
        view.property_loads.insert(
            20,
            vec![JitInlinePropertyLoad {
                receiver_shape: 7,
                value_byte: 24,
            }],
        );
        let tree = InlineTree::trivial(&view);
        let cfg = ControlFlowGraph::build_inlined(&tree).expect("CFG builds");
        let mut ssa = SsaFunction::build_inlined(&tree, &cfg).expect("SSA builds");
        let reprs = ReprMap::compute(&tree, &ssa);
        lower_settled_property_accesses(&mut ssa, &cfg, &view, &tree, &reprs);
        (cfg, ssa, tree)
    }

    #[test]
    fn a_settled_load_on_an_invariant_receiver_leaves_the_loop() {
        let (cfg, mut ssa, tree) = loop_with_one_settled_load();
        let dom = DominatorTree::compute(&cfg);
        assert!(!hoist_loop_invariant_accesses(&mut ssa, &cfg, &dom, &BTreeSet::new()).is_empty());

        let body = ssa
            .blocks
            .iter()
            .find(|block| cfg.blocks[block.id.0 as usize].start_pc == 5)
            .expect("the loop body block");
        assert_eq!(
            body.instrs.iter().map(|i| (i.op, i.pc)).collect::<Vec<_>>(),
            vec![
                (SsaOp::Reuse, 5),
                (SsaOp::Bytecode(Op::Add), 6),
                (SsaOp::Bytecode(Op::AddImm), 7),
                (SsaOp::Bytecode(Op::Jump), 8),
            ],
            "the body keeps only the rebind at the site's own PC"
        );
        let preheader = ssa
            .blocks
            .iter()
            .find(|block| cfg.blocks[block.id.0 as usize].start_pc == 0)
            .expect("the pre-header block");
        // The group runs in the pre-header under the PC of the instruction it
        // was inserted before, so its deopt resumes somewhere that exists.
        let hoisted: Vec<_> = preheader
            .instrs
            .iter()
            .filter(|instruction| instruction.op.bytecode().is_none())
            .map(|instruction| (instruction.op, instruction.pc))
            .collect();
        assert_eq!(
            hoisted,
            vec![
                (SsaOp::LoadHeader, 2),
                (SsaOp::CheckShape { shape: 7 }, 2),
                (SsaOp::LoadField { byte: 24 }, 2),
            ]
        );
        ssa.verify(&cfg, &dom, &ReprMap::compute(&tree, &ssa))
            .expect("the hoisted graph verifies");
    }

    #[test]
    fn a_recorded_bail_leaves_the_loop_alone() {
        let (cfg, mut ssa, _tree) = loop_with_one_settled_load();
        let dom = DominatorTree::compute(&cfg);
        assert!(
            hoist_loop_invariant_accesses(&mut ssa, &cfg, &dom, &BTreeSet::from([2])).is_empty()
        );
    }
}
