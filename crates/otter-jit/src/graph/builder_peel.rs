//! A small innermost loop's first iteration, built ahead of the loop.
//!
//! # Contents
//! - [`plans`] selects the loops of the compiled function that may be
//!   peeled and the loop-invariant receivers each one reads.
//! - [`PeelPass`] names the loop whose first iteration is being built.
//! - `Builder::build_peeled_iteration` builds that iteration into each loop
//!   block's second slot, when the loop's entry leaves a receiver unproved;
//!   `Builder::edge_slot` routes an edge to its slot.
//!
//! # Invariants
//! - Every edge that enters a peeled loop from outside, the OSR entry
//!   included, enters its first iteration; that iteration's latches enter
//!   the loop as forward edges, and its exits are ordinary exits. A loop
//!   whose entry already proves every receiver it reads is entered directly.
//! - The first iteration is straight-line code over the loop's own PCs, so
//!   what it proves, a receiver's shape among it, reaches the loop header
//!   as the facts of the loop's entry.
//! - Only the compiled function's own innermost loops that call out and
//!   have no handlers, up to a size bound, are peeled: in a loop that calls
//!   out, no heap fact survives an iteration and nothing moves out of it.
//!   Inlined bodies keep one copy of each loop.
//! - The iteration's eager states and inlining budget are its own: the loop
//!   built after it makes the same inlining decisions.
//!
//! # See also
//! - `super::Builder::build_body` for the visit order.
//! - `super::Builder::start_block` for how a header merges its entry.
//! - `super::super::licm` for what leaves a loop that does not call out.

use std::rc::Rc;

use otter_bytecode::Op;
use rustc_hash::FxHashMap;

use super::{Analysis, BlockId, Builder, JitCompileSnapshot, Kind, NodeId};

/// Bytecode instructions a peeled loop may have.
const MAX_PEELED_INSTRUCTIONS: usize = 96;
/// Bytecode instructions one function may peel in all.
const MAX_PEELED_INSTRUCTIONS_TOTAL: usize = 192;

/// One loop that may be built with a peeled first iteration: its blocks,
/// the registers it reads as receivers without writing them, and whether
/// it reads `this`.
#[derive(Debug, Clone)]
pub(super) struct PeelPlan {
    pub(super) body: Rc<[usize]>,
    receivers: Rc<[u16]>,
    this: bool,
}

/// The loop whose first iteration is being built: its header, its blocks,
/// and the offset of their second slots.
#[derive(Debug, Clone)]
pub(super) struct PeelPass {
    pub(super) header: usize,
    pub(super) offset: usize,
    pub(super) body: Rc<[usize]>,
}

/// The loops of `view` that may be built with a peeled first iteration,
/// by header.
pub(super) fn plans(view: &JitCompileSnapshot, analysis: &Analysis) -> FxHashMap<usize, PeelPlan> {
    let mut plans = FxHashMap::default();
    let mut budget = MAX_PEELED_INSTRUCTIONS_TOTAL;
    let control_flow = view.code_block.control_flow();
    let mut headers: Vec<usize> = analysis.loops.keys().copied().collect();
    headers.sort_unstable();
    for header in headers {
        let info = &analysis.loops[&header];
        let pcs = || {
            info.body
                .iter()
                .flat_map(|&block| analysis.blocks[block].start..analysis.blocks[block].end)
        };
        let innermost = info
            .body
            .iter()
            .all(|&block| block == header || !analysis.loops.contains_key(&block));
        let instructions = pcs().count();
        let calls = pcs().any(|pc| {
            matches!(
                analysis.instructions[pc as usize].op,
                Op::Call
                    | Op::CallWithThis
                    | Op::CallMethodValue
                    | Op::CallForwardArguments
                    | Op::CallSpread
                    | Op::New
            )
        });
        let handled = pcs().any(|pc| control_flow.handler_at(pc).is_some());
        if !analysis.blocks[header].reachable
            || !innermost
            || !calls
            || handled
            || instructions > MAX_PEELED_INSTRUCTIONS
            || instructions > budget
        {
            continue;
        }
        let mut receivers: Vec<u16> = pcs()
            .filter_map(|pc| {
                let instruction = &analysis.instructions[pc as usize];
                let receiver = matches!(
                    instruction.op,
                    Op::LoadProperty
                        | Op::StoreProperty
                        | Op::StorePropertyStrict
                        | Op::CallMethodValue
                        | Op::LoadElement
                        | Op::StoreElement
                        | Op::StoreElementStrict
                )
                .then(|| instruction.reads.first().copied())
                .flatten()?;
                (!info.assigned.contains(receiver)).then_some(receiver)
            })
            .collect();
        receivers.sort_unstable();
        receivers.dedup();
        let this = pcs().any(|pc| analysis.instructions[pc as usize].op == Op::LoadThis);
        if receivers.is_empty() && !this {
            continue;
        }
        budget -= instructions;
        plans.insert(
            header,
            PeelPlan {
                body: Rc::from(info.body.as_slice()),
                receivers: receivers.into(),
                this,
            },
        );
    }
    plans
}

impl Builder<'_> {
    /// Build the first iteration of the loop `plan` heads at `header` into
    /// its blocks' second slots, appending its graph blocks to `layout`;
    /// nothing when no edge enters the loop. An entry that already proves
    /// every receiver the loop reads enters the loop itself instead.
    pub(super) fn build_peeled_iteration(
        &mut self,
        header: usize,
        plan: &PeelPlan,
        layout: &mut Vec<BlockId>,
    ) {
        let offset = self.analysis.blocks.len();
        if self.incoming[header + offset].is_empty() {
            return;
        }
        if self.entry_proves(header + offset, plan) {
            let target = self.block_map[header];
            for edge in std::mem::take(&mut self.incoming[header + offset]) {
                if let Some(control) = self.graph.block(edge.block).control {
                    self.graph.node_mut(control).kind = Kind::Jump(target);
                }
                self.incoming[header].push(edge);
            }
            return;
        }
        let analysis = self.analysis.clone();
        // States are cached by PC, and the loop revisits these PCs.
        let states = std::mem::take(&mut self.eager_states);
        self.peel = Some(PeelPass {
            header,
            offset,
            body: plan.body.clone(),
        });
        for &block in &analysis.order {
            if plan.body.contains(&block) {
                self.build_block(block, block + offset, layout);
            }
        }
        self.peel = None;
        self.eager_states = states;
    }

    /// Whether every edge into `slot` proves the shape or element layout of
    /// each receiver `plan` reads, and of `this` when it reads `this`.
    fn entry_proves(&self, slot: usize, plan: &PeelPlan) -> bool {
        let edges = &self.incoming[slot];
        let proved = |node: NodeId| {
            edges.iter().all(|edge| {
                edge.known
                    .layout(node)
                    .is_some_and(|layout| layout.shapes.is_some() || layout.elements.is_some())
                    || edge.known.stale_shapes.contains_key(&node)
            })
        };
        let receivers = plan.receivers.iter().all(|&register| {
            let value = edges[0].frame[usize::from(register)];
            edges
                .iter()
                .all(|edge| edge.frame[usize::from(register)] == value)
                && proved(value)
        });
        let this = !plan.this
            || edges[0].known.this.is_some_and(|this| {
                edges.iter().all(|edge| edge.known.this == Some(this)) && proved(this)
            });
        receivers && this
    }

    /// The slot an edge to bytecode block `target` enters: a peeled loop's
    /// first iteration from outside it, that iteration's own blocks from
    /// within it, else the block's own.
    pub(super) fn edge_slot(&self, target: usize, back_edge: bool) -> usize {
        if back_edge {
            return target;
        }
        match &self.peel {
            Some(peel) if target != peel.header && peel.body.contains(&target) => {
                target + peel.offset
            }
            Some(_) => target,
            None if self.peel_plans.contains_key(&target) => target + self.analysis.blocks.len(),
            None => target,
        }
    }
}
