//! Source attribution and collector metadata shared by Graph instruction encoders.
//!
//! # Contents
//! - Baseline operation indexing.
//! - Source-body lookup and complete inline frame chains at collecting sites.
//! - Shared validation that baseline Generic nodes own a physical window.
//! - [`SitePlan`]: exact per-boundary root records, interned by content.
//! - [`recipe_record`]: one record per deduplicated deopt recipe.
//!
//! # Invariants
//! - Every record roots exactly the tagged homes written for live values at
//!   its boundary ([`super::regalloc::NodeAllocation::rooted_tagged_homes`]),
//!   plus the exception scratch the prologue zeroes. No home is ever cleared;
//!   a record never names a home its boundary did not write.
//! - The entry and throw-routing record roots only the exception scratch.
//! - Active outer C helpers carry no call PC; generated JS returns and inline
//!   helpers retain their source PC and inline frame chain.
//! - Site records descend from [`FIRST_SITE_SAFEPOINT`]; recipe records
//!   continue below them. Writeback publishes its exit's recipe record.
//! - These compile-time plans never read native registers or moving heap cells.
//! - Backend instruction selection does not change source or collector identity.
//! - A Generic node belongs to the outer source: inlined bodies have no
//!   independent baseline window and are abandoned by the builder.
//!
//! # See also
//! - [`super::frame`] for canonical home geometry and recovery recipes.
//! - [`super::emission`] for finalized metadata ownership.

use std::sync::Arc;

use otter_vm::JitCompileSnapshot;
use otter_vm::deopt::DeoptFrame;
use otter_vm::native_abi::{self as abi, SafepointRecord, SpillRoots};
use rustc_hash::FxHashMap;

use super::ir::{Graph, Kind, NodeId};
use super::regalloc::Allocation;
use crate::template::TemplatePlan;

/// The entry record and first descending id of this code object's own sites.
pub(crate) const FIRST_SITE_SAFEPOINT: abi::SafepointId = abi::NO_SAFEPOINT - 1;

/// Reject an invalid baseline-window recipe before either encoder creates or
/// publishes executable code. An inline source needs its own activation,
/// which the builder preserves by abandoning that candidate.
pub(crate) fn validate_generic_sources(graph: &Graph) -> Result<(), crate::Unsupported> {
    if graph
        .nodes
        .iter()
        .any(|node| node.origin != 0 && matches!(node.kind, Kind::Generic { .. }))
    {
        return Err(crate::Unsupported::OperandShape(
            "inlined Generic has no baseline register window",
        ));
    }
    Ok(())
}

/// Every operation of one canonical bytecode instruction, in source order.
pub(crate) fn operation_index(plan: &TemplatePlan) -> FxHashMap<u32, Vec<usize>> {
    let mut index: FxHashMap<u32, Vec<usize>> = FxHashMap::default();
    for (position, instruction) in plan.instructions.iter().enumerate() {
        index.entry(instruction.pc).or_default().push(position);
    }
    index
}

fn spill_roots(homes: impl IntoIterator<Item = u32>) -> SpillRoots {
    SpillRoots::from_slots(
        homes
            .into_iter()
            .map(|slot| u16::try_from(slot).expect("validated tagged spill region")),
    )
}

/// The entry and throw-routing record: only the zeroed exception scratch.
pub(crate) fn entry_safepoint(scratch: u32) -> SafepointRecord {
    SafepointRecord {
        id: FIRST_SITE_SAFEPOINT,
        frame_state: abi::NO_FRAME_STATE,
        spill_roots: spill_roots([scratch]),
        call_pc: abi::NO_CALL_PC,
        inline_frames: Box::default(),
    }
}

/// Interning key: `None` source for an active outer helper.
type SiteKey = (Option<(u16, u32)>, Box<[u32]>);

/// Exact root records of one code object's collecting boundaries.
pub(crate) struct SitePlan {
    records: Vec<SafepointRecord>,
    ids: FxHashMap<SiteKey, abi::SafepointId>,
    scratch: u32,
}

impl SitePlan {
    pub(crate) fn new(scratch: u32) -> Self {
        Self {
            records: vec![entry_safepoint(scratch)],
            ids: FxHashMap::default(),
            scratch,
        }
    }

    pub(crate) fn into_records(self) -> Vec<SafepointRecord> {
        self.records
    }

    fn rooted(
        &self,
        allocation: &Allocation,
        node: NodeId,
    ) -> Result<Box<[u32]>, crate::Unsupported> {
        allocation
            .node(node)
            .rooted_tagged_homes
            .clone()
            .ok_or(crate::Unsupported::OperandShape(
                "graph safepoint at a boundary without a root plan",
            ))
    }

    fn intern(
        &mut self,
        key: SiteKey,
        call_pc: u32,
        inline_frames: impl FnOnce() -> Box<[DeoptFrame<Option<u16>>]>,
    ) -> abi::SafepointId {
        if let Some(&id) = self.ids.get(&key) {
            return id;
        }
        let id = FIRST_SITE_SAFEPOINT - self.records.len() as abi::SafepointId;
        let spill_roots = spill_roots(key.1.iter().copied().chain(std::iter::once(self.scratch)));
        self.records.push(SafepointRecord {
            id,
            frame_state: abi::NO_FRAME_STATE,
            spill_roots,
            call_pc,
            inline_frames: inline_frames(),
        });
        self.ids.insert(key, id);
        id
    }

    /// Collector identity for an active C helper: the exact boundary roots,
    /// with the outer source or the exact inline source.
    pub(crate) fn helper(
        &mut self,
        view: &JitCompileSnapshot,
        inline_views: &[Arc<JitCompileSnapshot>],
        graph: &Graph,
        allocation: &Allocation,
        node: NodeId,
    ) -> Result<abi::SafepointId, crate::Unsupported> {
        if graph.node(node).origin == 0 {
            let rooted = self.rooted(allocation, node)?;
            Ok(self.intern((None, rooted), abi::NO_CALL_PC, Box::default))
        } else {
            self.source(view, inline_views, graph, allocation, node)
        }
    }

    /// Each JS return has source-bearing state, including outermost call sites.
    pub(crate) fn source(
        &mut self,
        view: &JitCompileSnapshot,
        inline_views: &[Arc<JitCompileSnapshot>],
        graph: &Graph,
        allocation: &Allocation,
        node: NodeId,
    ) -> Result<abi::SafepointId, crate::Unsupported> {
        let rooted = self.rooted(allocation, node)?;
        let data = graph.node(node);
        let key = (Some((data.origin, data.pc)), rooted);
        Ok(self.intern(key, graph.outer_pc(node), || {
            let mut frames = Vec::new();
            let mut origin = data.origin;
            let mut byte_pc = source_view(view, inline_views, graph, node)
                .instructions
                .get(data.pc as usize)
                .map_or(0, |instruction| instruction.byte_pc);
            while origin != 0 {
                let body = graph.inlined[usize::from(origin) - 1];
                frames.push(DeoptFrame {
                    function_id: body.function_id,
                    byte_pc,
                    entry: None,
                    register_count: 0,
                    slots: Box::new([]),
                });
                byte_pc = body.call_byte_pc;
                origin = body.parent;
            }
            frames.reverse();
            frames.into_boxed_slice()
        }))
    }
}

/// The record of one deopt recipe: the recipe's tagged homes, which every
/// exit using it wrote with its cold moves, and the exception scratch.
/// Writeback publishes it before anything may collect.
pub(crate) fn recipe_record(
    id: abi::SafepointId,
    locations: &[super::regalloc::Location],
    scratch: u32,
) -> SafepointRecord {
    SafepointRecord {
        id,
        frame_state: abi::NO_FRAME_STATE,
        spill_roots: spill_roots(
            Allocation::recipe_tagged_homes(locations)
                .into_iter()
                .chain(std::iter::once(scratch)),
        ),
        call_pc: abi::NO_CALL_PC,
        inline_frames: Box::default(),
    }
}

/// The one snapshot that owns this node's semantic source and realm.
pub(crate) fn source_view<'a>(
    view: &'a JitCompileSnapshot,
    inline_views: &'a [Arc<JitCompileSnapshot>],
    graph: &Graph,
    node: NodeId,
) -> &'a JitCompileSnapshot {
    match graph.node(node).origin {
        0 => view,
        origin => &inline_views[usize::from(origin) - 1],
    }
}
