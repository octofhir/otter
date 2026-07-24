//! Backend-neutral planning for speculative loop-invariant property reads.
//!
//! # Contents
//! - [`PropertyLoopCachePlan`] — cache groups and sites keyed by loop header.
//! - [`analyze_property_loop_caches`] — reducible-loop safety analysis.
//! - [`natural_loop_blocks`] — deterministic natural-loop membership.
//! - [`transparent_origin`] — local-move stripping for invariant receivers.
//!
//! # Invariants
//! - A candidate loop contains no heap-observing operation except property
//!   reads, and every receiver is defined outside its natural loop.
//! - Nested candidates sharing a site are rejected instead of choosing an
//!   arbitrary activation boundary.
//! - This plan moves no JS operation. A backend must train through the original
//!   property semantics and activate only after one complete all-fast-hit
//!   iteration.

use std::collections::{BTreeMap, BTreeSet};

use otter_bytecode::Op;

use crate::ir::{
    cfg::{BlockId, ControlFlowGraph},
    inline::InlineId,
    ssa::{SsaFunction, SsaInstr, ValueDef, ValueId},
};

#[derive(Debug, Clone, Copy)]
pub(crate) struct PropertyLoopCacheSite {
    pub(crate) header: BlockId,
    pub(crate) value_slot: u32,
}

#[derive(Debug, Clone)]
pub(crate) struct PropertyLoopCache {
    pub(crate) ready_slot: u32,
    pub(crate) value_slots: Box<[u32]>,
}

#[derive(Debug, Default)]
pub(crate) struct PropertyLoopCachePlan {
    pub(crate) sites: BTreeMap<(InlineId, u32), PropertyLoopCacheSite>,
    pub(crate) loops: BTreeMap<BlockId, PropertyLoopCache>,
    pub(crate) slot_count: u32,
}

pub(crate) fn natural_loop_blocks(
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

pub(crate) fn transparent_origin(ssa: &SsaFunction, mut value: ValueId) -> Option<ValueId> {
    loop {
        let data = ssa.values.get(value.0 as usize)?;
        match &data.def {
            ValueDef::Op {
                op: Op::LoadLocal | Op::StoreLocal,
                inputs,
                ..
            } if inputs.len() == 1 => value = inputs[0],
            _ => return Some(value),
        }
    }
}

fn safe_instruction(instruction: &SsaInstr) -> bool {
    matches!(
        instruction.op,
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

/// Find reducible loops whose only heap-observing operations are property
/// reads from receivers defined outside the loop.
pub(crate) fn analyze_property_loop_caches(
    cfg: &ControlFlowGraph,
    ssa: &SsaFunction,
    back_edges: &BTreeSet<(BlockId, BlockId)>,
) -> PropertyLoopCachePlan {
    let mut candidate_groups = Vec::new();
    for &(latch, header) in back_edges {
        let blocks = natural_loop_blocks(cfg, latch, header);
        if cfg.blocks[header.0 as usize].inline != InlineId::ROOT
            || back_edges
                .iter()
                .any(|&(other_latch, other_header)| other_header == header && other_latch != latch)
            || blocks.iter().any(|block| {
                cfg.blocks[block.0 as usize].inline != InlineId::ROOT
                    || ssa.blocks[block.0 as usize]
                        .instrs
                        .iter()
                        .any(|instruction| !safe_instruction(instruction))
            })
        {
            continue;
        }
        let mut sites = Vec::new();
        let mut receivers_are_invariant = true;
        for block in &blocks {
            for instruction in &ssa.blocks[block.0 as usize].instrs {
                if instruction.op != Op::LoadProperty {
                    continue;
                }
                let Some(receiver) = instruction
                    .inputs
                    .first()
                    .copied()
                    .and_then(|value| transparent_origin(ssa, value))
                else {
                    receivers_are_invariant = false;
                    continue;
                };
                if blocks.contains(&ssa.values[receiver.0 as usize].def_block) {
                    receivers_are_invariant = false;
                }
                sites.push((instruction.inline, instruction.pc));
            }
        }
        if receivers_are_invariant && !sites.is_empty() {
            sites.sort_unstable();
            sites.dedup();
            candidate_groups.push((header, sites));
        }
    }

    let mut occurrences = BTreeMap::new();
    for (_, sites) in &candidate_groups {
        for site in sites {
            *occurrences.entry(*site).or_insert(0u32) += 1;
        }
    }

    let mut plan = PropertyLoopCachePlan::default();
    for (header, sites) in candidate_groups {
        if sites.iter().any(|site| occurrences[site] != 1) {
            continue;
        }
        let ready_slot = plan.slot_count;
        let Some(next_slot) = plan.slot_count.checked_add(1) else {
            return PropertyLoopCachePlan::default();
        };
        plan.slot_count = next_slot;
        let mut value_slots = Vec::with_capacity(sites.len());
        for site in sites {
            let value_slot = plan.slot_count;
            let Some(next_slot) = plan.slot_count.checked_add(1) else {
                return PropertyLoopCachePlan::default();
            };
            plan.slot_count = next_slot;
            value_slots.push(value_slot);
            plan.sites
                .insert(site, PropertyLoopCacheSite { header, value_slot });
        }
        plan.loops.insert(
            header,
            PropertyLoopCache {
                ready_slot,
                value_slots: value_slots.into_boxed_slice(),
            },
        );
    }
    plan
}
