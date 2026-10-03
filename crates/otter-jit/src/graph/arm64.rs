//! AArch64 code generation for the graph tier: every node on the locations
//! the allocator chose, slow paths and exits out of line.
//!
//! # Contents
//! - [`emit`] — the whole code object: the tier entry over a published
//!   interpreter frame, the JavaScript call entry, the body in layout order,
//!   deferred slow paths, deopt exits and the shared deopt handler.
//! - Moves: sequential gap moves before nodes and parallel edge moves at the
//!   end of a predecessor.
//! - Node emitters for frame, arithmetic, conversion, check, memory, generic
//!   and control nodes.
//!
//! # Invariants
//! - Pinned registers: `x19` register window, `x20` context, `x21` published
//!   frame record, `x29` frame pointer. The only emitter scratch is `x16`,
//!   `x17` and `d31`; no node writes an allocatable register other than its
//!   result and declared temporaries.
//! - Spill slots live at `sp`: tagged slot `k` at `sp + 8k`, untagged slots
//!   after every tagged one. Each point that may collect names its own
//!   safepoint on the frame record before the runtime runs, rooting exactly
//!   the tagged slots live across it, all written by then; entries publish
//!   none and leave slots unwritten.
//! - A value is stored to its slot right after it is defined (a phi at its
//!   block's start), never at a later spill point.
//! - A deopt exit is `movz w17, index ; b deopt`: the shared handler dumps
//!   every allocatable register and the writeback stub rebuilds the frame
//!   from the exit's recipe.
//! - Conditional branches to deopt exits and slow paths use the ±1 MiB
//!   conditional form; nothing branches with `tbz`/`tbnz` to a far label.
//!
//! # See also
//! - [`super::regalloc`] — the locations consumed here.
//! - [`crate::arm64::activation`] — the call-entry and exit protocol shared
//!   with the baseline tier.

#![allow(clippy::useless_conversion)]

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::JitCompileSnapshot;
use otter_vm::deopt::{DeoptFrame, DeoptFrameEntry, DeoptLocation, DeoptRepr, DeoptSlot};
use otter_vm::jit::{JitBodyGuard, JitElementRepr, JitGuardWidth, JitHoleBitmap};
use otter_vm::native_abi::{
    self as abi, ExitAction, ExitReason, NO_CALL_PC, NO_FRAME_STATE, NativeResultStatus,
    SafepointRecord, TaggedLocation, TaggedLocationKind,
};
use otter_vm::value::tag;
use rustc_hash::FxHashMap;

use super::builder::Built;
use super::ir::{
    BlockId, BranchKind, Condition, DeoptReason, FrameStateId, Graph, Kind, NodeId, Repr,
};
use super::regalloc::{Allocation, FP_REGISTERS, GP_REGISTERS, Location, Move};
use crate::Unsupported;
use crate::artifact::relocation::{RelocationCapture, RelocationTarget};
use crate::entry::{
    NATIVE_FRAME_SELF_OFFSET, NATIVE_FRAME_THIS_OFFSET, THREAD_OFFSET, VALUE_HOLE,
    VM_THREAD_BACKEDGE_FUEL_CELL_OFFSET, VM_THREAD_GC_HEAP_OFFSET, VM_THREAD_INTERRUPT_CELL_OFFSET,
    VM_THREAD_MARKING_FLAG_CELL_OFFSET,
};
use crate::template::arm64::values::{emit_load_symbol_u64, emit_load_u64};

const NUMBER_TAG: u64 = tag::NUMBER_TAG;
const NOT_CELL_MASK: u64 = tag::NOT_CELL_MASK;
const VALUE_TRUE: u64 = tag::VALUE_TRUE;
const VALUE_FALSE: u64 = tag::VALUE_FALSE;
const VALUE_UNDEFINED: u64 = tag::VALUE_UNDEFINED;
const VALUE_NULL: u64 = tag::VALUE_NULL;
const DOUBLE_OFFSET: u64 = tag::DOUBLE_ENCODE_OFFSET;
const NEW_TARGET_OFFSET: u32 = abi::NATIVE_FRAME_NEW_TARGET_OFFSET;
const DETACH_PROTECTOR: u32 = crate::entry::VM_THREAD_ARRAY_BUFFER_DETACH_PROTECTOR_CELL_OFFSET;
const ARRAY_INDEX_PROTECTOR: u32 = crate::entry::VM_THREAD_ARRAY_INDEX_PROTECTOR_CELL_OFFSET;

/// Index of an allocatable register in the deopt dump and in deopt
/// locations: general registers by number, then floating-point registers in
/// [`FP_REGISTERS`] order.
pub(crate) fn dump_index(location: Location) -> Option<u16> {
    match location {
        Location::Gp(register) => GP_REGISTERS
            .iter()
            .position(|&r| r == register)
            .map(|index| index as u16),
        Location::Fp(register) => FP_REGISTERS
            .iter()
            .position(|&r| r == register)
            .map(|index| (GP_REGISTERS.len() + index) as u16),
        _ => None,
    }
}

/// Words in the deopt register dump.
pub(crate) const DUMP_WORDS: usize = GP_REGISTERS.len() + FP_REGISTERS.len();

/// Frame-slot geometry of one code object.
///
/// Tagged slots come first (the allocator's, one exception scratch slot,
/// then one snapshot slot per general register), untagged slots after them
/// (the allocator's, then one snapshot slot per allocatable register).
#[derive(Debug, Clone, Copy)]
pub(crate) struct SlotLayout {
    /// Tagged slots the allocator uses.
    pub(crate) spill_tagged: u32,
    /// Untagged slots the allocator uses.
    pub(crate) spill_untagged: u32,
    /// All tagged slots, rooted by the code object's safepoint.
    pub(crate) tagged: u32,
    /// All untagged slots.
    pub(crate) untagged: u32,
}

impl SlotLayout {
    pub(crate) fn of(allocation: &Allocation) -> Self {
        let spill_tagged = allocation.tagged_slots;
        let spill_untagged = allocation.untagged_slots;
        Self {
            spill_tagged,
            spill_untagged,
            tagged: spill_tagged + 1 + GP_REGISTERS.len() as u32,
            untagged: spill_untagged + DUMP_WORDS as u32,
        }
    }

    /// Bytes of the spill area, a multiple of 16: the slots, then the 16
    /// bytes a tier entry keeps the interpreter record's root words in.
    pub(crate) fn bytes(self) -> u32 {
        ((self.tagged + self.untagged) * 8).next_multiple_of(16) + 16
    }

    /// Byte offset of a slot from the slot base.
    pub(crate) fn offset(self, location: Location) -> u32 {
        match location {
            Location::TaggedSlot(index) => index * 8,
            Location::UntaggedSlot(index) => (self.tagged + index) * 8,
            _ => unreachable!("not a slot"),
        }
    }

    /// The tagged slot holding an exception across a frame rebuild.
    pub(crate) fn exception_scratch(self) -> Location {
        Location::TaggedSlot(self.spill_tagged)
    }

    /// Where a slow path that can collect saves a live register.
    pub(crate) fn snapshot_slot(self, register: Location, repr: Repr) -> Location {
        let index = dump_index(register).expect("an allocatable register");
        match (register, repr) {
            (Location::Gp(_), Repr::Tagged) => {
                Location::TaggedSlot(self.spill_tagged + 1 + u32::from(index))
            }
            _ => Location::UntaggedSlot(self.spill_untagged + u32::from(index)),
        }
    }
}

/// One deopt exit site.
#[derive(Debug, Clone)]
pub(crate) struct ExitSite {
    pub(crate) label: DynamicLabel,
    pub(crate) node: NodeId,
    /// `true` for the node's lazy (after-call) state.
    pub(crate) lazy: bool,
    pub(crate) reason: ExitReason,
    pub(crate) action: ExitAction,
    /// Where the state's values are at this exit when that is not where the
    /// allocator put them for the node.
    pub(crate) locations: Option<Box<[Location]>>,
}

/// A slow path emitted after the body.
type Deferred<'a> = Box<dyn FnOnce(&mut Codegen<'a>) + 'a>;

/// What [`emit`] produced besides the bytes.
pub(crate) struct Emission {
    pub(crate) buffer: dynasmrt::ExecutableBuffer,
    pub(crate) tier_entry: usize,
    pub(crate) call_entry: usize,
    pub(crate) exits: Vec<ExitSite>,
    pub(crate) relocations: RelocationCapture,
    /// Inline-cache cells of generic property accesses; their addresses are
    /// baked into the code.
    pub(crate) load_ic_cells: Box<[crate::entry::PropertySourceCell]>,
    pub(crate) store_ic_cells: Box<[crate::entry::PropertySourceCell]>,
    /// Code offset where each emitted node (and block) starts.
    pub(crate) node_offsets: Vec<(usize, NodeId)>,
    /// Code offset where the body ends and the out-of-line code begins.
    pub(crate) body_end: usize,
    /// One safepoint record per distinct set of slots a point that may
    /// collect roots.
    pub(crate) site_records: Vec<SafepointRecord>,
}

fn exit_reason(reason: DeoptReason) -> (ExitReason, ExitAction) {
    match reason {
        DeoptReason::WrongType => (ExitReason::TypeMismatch, ExitAction::Recompile),
        DeoptReason::WrongShape => (ExitReason::ShapeGuard, ExitAction::Recompile),
        DeoptReason::WrongValue => (ExitReason::IdentityGuard, ExitAction::Recompile),
        DeoptReason::Overflow => (ExitReason::Int32Overflow, ExitAction::Recompile),
        DeoptReason::MinusZero => (ExitReason::NegativeZero, ExitAction::Recompile),
        DeoptReason::OutOfBounds => (ExitReason::BoundsGuard, ExitAction::Recompile),
        DeoptReason::InvalidIndex => (ExitReason::InvalidElementIndex, ExitAction::Recompile),
        DeoptReason::LostPrecision => (ExitReason::TypeMismatch, ExitAction::Recompile),
        DeoptReason::InsufficientFeedback => {
            (ExitReason::InsufficientFeedback, ExitAction::Recompile)
        }
        DeoptReason::Unsupported => (ExitReason::UnsupportedOperation, ExitAction::Resume),
    }
}

struct Codegen<'a> {
    ops: Assembler,
    relocations: RelocationCapture,
    view: &'a JitCompileSnapshot,
    /// The snapshot of each inlined body, by origin minus one.
    inline_views: &'a [std::sync::Arc<JitCompileSnapshot>],
    graph: &'a Graph,
    allocation: &'a Allocation,
    layout: &'a [BlockId],
    slots: SlotLayout,
    /// Bytes pushed below the slot base during a call sequence.
    sp_delta: u32,
    labels: FxHashMap<BlockId, DynamicLabel>,
    exits: Vec<ExitSite>,
    /// Deferred slow paths, emitted after the body.
    deferred: Vec<Deferred<'a>>,
    /// Shared labels.
    returned: DynamicLabel,
    deopt: DynamicLabel,
    fatal: DynamicLabel,
    activation: crate::arm64::frame::ActivationExits,
    spill: crate::arm64::frame::SpillArea,
    transitions: &'a crate::entry::TransitionTable,
    deopt_runtime: u64,
    /// The baseline operations generic nodes run, one sequence per PC.
    plan: &'a crate::template::TemplatePlan,
    plan_index: FxHashMap<u32, Vec<usize>>,
    load_ic_cells: Box<[crate::entry::PropertySourceCell]>,
    next_load_ic: usize,
    store_ic_cells: Box<[crate::entry::PropertySourceCell]>,
    next_store_ic: usize,
    no_direct_call_events: Option<crate::template::DirectCallEvents>,
    no_code_map: Option<crate::artifact::CodeMapCapture>,
    node_offsets: Vec<(usize, NodeId)>,
    /// Status-word error parked in the context: finish and route it.
    threw: DynamicLabel,
    /// Exception value in `x0`: route it.
    committed_throw: DynamicLabel,
    /// Exception value in `x0` leaves this frame.
    propagate: DynamicLabel,
    /// Subroutine rebuilding the interpreter frame of the exit in `w17`
    /// without leaving; returns through `x30`.
    materialize: DynamicLabel,
    /// A body past conditional-branch reach is emitted in segments: each
    /// conditional branch to a block or a shared label leaves through a
    /// veneer of its segment, and an island after the segment holds the
    /// segment's slow paths, exit stubs and veneers.
    far: bool,
    island_base: usize,
    exits_flushed: usize,
    veneers: Vec<(DynamicLabel, DynamicLabel)>,
    /// Safepoint records of the points that may collect, by their rooted
    /// slot sets.
    site_records: Vec<SafepointRecord>,
    site_ids: FxHashMap<Vec<u32>, abi::SafepointId>,
}

/// Body bytes between islands: a segment plus its island stays well inside
/// the ±1 MiB a conditional branch reaches.
const ISLAND_INTERVAL: usize = 256 * 1024;

/// Prototype links an inline `instanceof` follows before the runtime
/// completes the walk.
const INSTANCEOF_CHAIN_BOUND: u32 = 32;

/// The first id of a code object's own safepoints; each point that may
/// collect names the next one down.
const FIRST_SITE_SAFEPOINT: abi::SafepointId = abi::NO_SAFEPOINT - 1;

/// The property source cells the code object's runtime completions name:
/// one per shared-table access node, and one per baseline property operation
/// a generic node runs. Nodes no block emits only leave a cell unused.
fn property_cell_counts(built: &Built, plan: &crate::template::TemplatePlan) -> (usize, usize) {
    use crate::template::TemplateOp;
    let (mut loads, mut stores) = (0, 0);
    for node in &built.graph.nodes {
        match node.kind {
            Kind::LoadPropertyCached { .. } => loads += 1,
            Kind::StorePropertyCached { .. } => stores += 1,
            Kind::Generic { pc, .. } => {
                for instruction in plan.instructions.iter().filter(|op| op.pc == pc) {
                    match instruction.op {
                        TemplateOp::LoadProperty { .. } => loads += 1,
                        TemplateOp::StoreProperty { .. } => stores += 1,
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    (loads, stores)
}

/// Emit the code object for `built` allocated by `allocation`.
pub(crate) fn emit(
    view: &JitCompileSnapshot,
    built: &Built,
    allocation: &Allocation,
    transitions: &crate::entry::TransitionTable,
    code_object_id: u64,
    deopt_runtime: *const otter_vm::deopt::DeoptRuntime,
    plan: &crate::template::TemplatePlan,
    slots: SlotLayout,
    capture_relocations: bool,
    far: bool,
) -> Result<Emission, Unsupported> {
    let mut ops = Assembler::new()
        .map_err(|_| Unsupported::Backend(crate::BackendFailure::AssemblerAllocation))?;
    let (load_cells, store_cells) = property_cell_counts(built, plan);
    let labels = built
        .layout
        .iter()
        .map(|&block| (block, ops.new_dynamic_label()))
        .collect();
    let returned = ops.new_dynamic_label();
    let deopt = ops.new_dynamic_label();
    let fatal = ops.new_dynamic_label();
    let activation = crate::arm64::frame::ActivationExits {
        construct: ops.new_dynamic_label(),
        side_exit: ops.new_dynamic_label(),
    };
    let threw = ops.new_dynamic_label();
    let committed_throw = ops.new_dynamic_label();
    let propagate = ops.new_dynamic_label();
    let materialize = ops.new_dynamic_label();
    // No slot is rooted before a point that may collect names its own
    // safepoint, which lists only slots already written: the entries
    // publish no safepoint and leave the slots unwritten.
    let spill = crate::arm64::frame::SpillArea {
        bytes: slots.bytes(),
        tagged_slots: 0,
        safepoint: abi::NO_SAFEPOINT,
    };
    // A body that never runs a baseline operation on the window leaves it
    // to the exits that rebuild the interpreter frame.
    let lazy_window = !built.graph.nodes.iter().any(|node| {
        matches!(
            node.kind,
            Kind::Generic { .. } | Kind::LoadWindow(_) | Kind::StoreWindow(_)
        )
    });
    let shape = crate::arm64::frame::EntryShape::of(
        view,
        code_object_id,
        abi::NativeFrameKind::Optimizing,
        true,
    )?
    .with_lazy_window(lazy_window);
    let mut codegen = Codegen {
        ops,
        relocations: RelocationCapture::new(capture_relocations),
        view,
        inline_views: &built.inline_views,
        graph: &built.graph,
        allocation,
        layout: &built.layout,
        slots,
        sp_delta: 0,
        labels,
        exits: Vec::new(),
        deferred: Vec::new(),
        returned,
        deopt,
        fatal,
        activation,
        spill,
        transitions,
        deopt_runtime: deopt_runtime as u64,
        plan,
        plan_index: {
            let mut index: FxHashMap<u32, Vec<usize>> = FxHashMap::default();
            for (position, instruction) in plan.instructions.iter().enumerate() {
                index.entry(instruction.pc).or_default().push(position);
            }
            index
        },
        load_ic_cells: vec![crate::entry::PropertySourceCell::default(); load_cells]
            .into_boxed_slice(),
        next_load_ic: 0,
        store_ic_cells: vec![crate::entry::PropertySourceCell::default(); store_cells]
            .into_boxed_slice(),
        next_store_ic: 0,
        no_direct_call_events: None,
        no_code_map: None,
        site_records: Vec::new(),
        site_ids: FxHashMap::default(),
        node_offsets: Vec::new(),
        threw,
        committed_throw,
        propagate,
        materialize,
        far,
        island_base: 0,
        exits_flushed: 0,
        veneers: Vec::new(),
    };
    let tier_entry = codegen.ops.offset().0;
    let body = codegen.ops.new_dynamic_label();
    crate::arm64::frame::emit_tier_prologue(&mut codegen.ops, spill);
    if let Some(osr) = built.osr_entry {
        // An OSR entry carries the frame flag; the dispatch consumes it so
        // the frame continues as an ordinary optimizing frame.
        let osr_label = codegen.labels[&osr];
        let dispatch = codegen.ops.new_dynamic_label();
        dynasm!(codegen.ops
            ; .arch aarch64
            ; ldrb w16, [x21, crate::entry::NATIVE_FRAME_FLAGS_OFFSET]
            ; tst w16, u32::from(abi::NativeFrameFlags::OSR_ENTRY)
            ; b.ne =>dispatch
            ; b =>body
            ; =>dispatch
            ; and w16, w16, !u32::from(abi::NativeFrameFlags::OSR_ENTRY)
            ; strb w16, [x21, crate::entry::NATIVE_FRAME_FLAGS_OFFSET]
            ; b =>osr_label
        );
    } else {
        dynasm!(codegen.ops ; .arch aarch64 ; b =>body);
    }
    let call_entry_cold = crate::arm64::frame::CallEntryCold::new(&mut codegen.ops, shape);
    crate::arm64::frame::emit_call_entry_cold(
        &mut codegen.ops,
        &mut codegen.relocations,
        transitions,
        activation,
        call_entry_cold,
    );
    let call_entry =
        crate::arm64::frame::emit_call_entry(&mut codegen.ops, view, shape, spill, call_entry_cold)
            .0;
    dynasm!(codegen.ops ; .arch aarch64 ; =>body);
    codegen.emit_body()?;
    let body_end = codegen.ops.offset().0;
    codegen.flush_segment();
    codegen.emit_exit_stubs();
    crate::arm64::frame::emit_exits(
        &mut codegen.ops,
        &mut codegen.relocations,
        transitions,
        view,
        shape.derived,
        activation,
        spill,
    );
    let Codegen {
        ops,
        relocations,
        exits,
        load_ic_cells,
        store_ic_cells,
        node_offsets,
        site_records,
        ..
    } = codegen;
    let buffer = crate::entry::finalize_assembler(ops)?;
    Ok(Emission {
        buffer,
        tier_entry,
        call_entry,
        exits,
        relocations,
        load_ic_cells,
        store_ic_cells,
        node_offsets,
        body_end,
        site_records,
    })
}

impl<'a> Codegen<'a> {
    // ------------------------------------------------------------------
    // Slots and moves
    // ------------------------------------------------------------------

    /// Run `access(ops, base, offset)` against `sp + offset`, materializing
    /// the address in `x17` when the offset does not encode.
    fn emit_sp_access(&mut self, offset: u32, access: impl FnOnce(&mut Assembler, u8, u32)) {
        let offset = offset + self.sp_delta;
        if offset <= 32760 && offset.is_multiple_of(8) {
            access(&mut self.ops, 31, offset);
        } else {
            emit_load_u64(&mut self.ops, 17, u64::from(offset));
            dynasm!(self.ops ; .arch aarch64 ; add x17, sp, x17);
            access(&mut self.ops, 17, 0);
        }
    }

    fn load_slot_gp(&mut self, register: u8, slot: Location) {
        let offset = self.slots.offset(slot);
        self.emit_sp_access(offset, |ops, base, off| {
            dynasm!(ops ; .arch aarch64 ; ldr X(register), [XSP(base), off]);
        });
    }

    fn store_slot_gp(&mut self, register: u8, slot: Location) {
        let offset = self.slots.offset(slot);
        self.emit_sp_access(offset, |ops, base, off| {
            dynasm!(ops ; .arch aarch64 ; str X(register), [XSP(base), off]);
        });
    }

    fn load_slot_fp(&mut self, register: u8, slot: Location) {
        let offset = self.slots.offset(slot);
        self.emit_sp_access(offset, |ops, base, off| {
            dynasm!(ops ; .arch aarch64 ; ldr D(register), [XSP(base), off]);
        });
    }

    fn store_slot_fp(&mut self, register: u8, slot: Location) {
        let offset = self.slots.offset(slot);
        self.emit_sp_access(offset, |ops, base, off| {
            dynasm!(ops ; .arch aarch64 ; str D(register), [XSP(base), off]);
        });
    }

    /// The bits a constant node materializes as, and whether it belongs in
    /// a floating-point register.
    fn constant_bits(&self, node: NodeId) -> (u64, bool) {
        match self.graph.node(node).kind {
            Kind::ConstTagged(bits) => (bits, false),
            Kind::ConstInt32(value) => (u64::from(value as u32), false),
            Kind::ConstFloat64(bits) => (bits, true),
            _ => unreachable!("not a constant"),
        }
    }

    /// Materialize `bits` into general register `register`.
    fn load_immediate(&mut self, register: u8, bits: u64) {
        emit_load_u64(&mut self.ops, register, bits);
    }

    fn emit_move(&mut self, from: Location, to: Location) {
        if from == to {
            return;
        }
        match (from, to) {
            (Location::Gp(a), Location::Gp(b)) => {
                dynasm!(self.ops ; .arch aarch64 ; mov X(b), X(a))
            }
            (Location::Fp(a), Location::Fp(b)) => {
                dynasm!(self.ops ; .arch aarch64 ; fmov D(b), D(a))
            }
            (Location::Gp(a), slot @ (Location::TaggedSlot(_) | Location::UntaggedSlot(_))) => {
                self.store_slot_gp(a, slot);
            }
            (Location::Fp(a), slot @ (Location::TaggedSlot(_) | Location::UntaggedSlot(_))) => {
                self.store_slot_fp(a, slot);
            }
            (slot @ (Location::TaggedSlot(_) | Location::UntaggedSlot(_)), Location::Gp(b)) => {
                self.load_slot_gp(b, slot);
            }
            (slot @ (Location::TaggedSlot(_) | Location::UntaggedSlot(_)), Location::Fp(b)) => {
                self.load_slot_fp(b, slot);
            }
            (
                from @ (Location::TaggedSlot(_) | Location::UntaggedSlot(_)),
                to @ (Location::TaggedSlot(_) | Location::UntaggedSlot(_)),
            ) => {
                self.load_slot_gp(16, from);
                self.store_slot_gp(16, to);
            }
            (Location::Constant(node), Location::Gp(b)) => {
                let (bits, _) = self.constant_bits(node);
                self.load_immediate(b, bits);
            }
            (Location::Constant(node), Location::Fp(b)) => {
                let (bits, _) = self.constant_bits(node);
                if bits == 0 {
                    dynasm!(self.ops ; .arch aarch64 ; fmov D(b), xzr);
                } else {
                    self.load_immediate(16, bits);
                    dynasm!(self.ops ; .arch aarch64 ; fmov D(b), x16);
                }
            }
            (Location::Constant(node), slot) => {
                let (bits, _) = self.constant_bits(node);
                self.load_immediate(16, bits);
                self.store_slot_gp(16, slot);
            }
            (from, to) => unreachable!("move {from:?} -> {to:?}"),
        }
    }

    /// Run `moves` as one parallel assignment.
    ///
    /// A move is emitted once no pending move still reads its destination;
    /// read counts per location make that a linear worklist. What remains
    /// then is cycles: one source of a cycle is parked on the stack, which
    /// frees its location, and the move that read it is replayed from the
    /// stack when its own destination is free. Parking on the stack keeps
    /// every scratch register available to the slot and constant moves of
    /// the same assignment.
    fn emit_parallel_moves(&mut self, moves: Vec<Move>) {
        let moves: Vec<Move> = moves.into_iter().filter(|m| m.from != m.to).collect();
        if moves.is_empty() {
            return;
        }
        let mut readers: FxHashMap<Location, u32> = FxHashMap::default();
        let mut writer: FxHashMap<Location, usize> = FxHashMap::default();
        for (index, m) in moves.iter().enumerate() {
            *readers.entry(m.from).or_default() += 1;
            writer.insert(m.to, index);
        }
        let mut done = vec![false; moves.len()];
        let mut parked: Option<usize> = None;
        let mut ready: Vec<usize> = (0..moves.len())
            .filter(|&index| !readers.contains_key(&moves[index].to))
            .collect();
        let mut remaining = moves.len();
        while remaining != 0 {
            while let Some(index) = ready.pop() {
                let m = moves[index];
                if parked == Some(index) {
                    self.emit_unpark(m.to);
                    parked = None;
                } else {
                    self.emit_move(m.from, m.to);
                    let count = readers.get_mut(&m.from).expect("a read source");
                    *count -= 1;
                    if *count == 0
                        && let Some(&next) = writer.get(&m.from)
                        && !done[next]
                    {
                        ready.push(next);
                    }
                }
                done[index] = true;
                remaining -= 1;
            }
            if remaining == 0 {
                break;
            }
            // Only cycles are left: park one source.
            let index = (0..moves.len())
                .find(|&index| !done[index])
                .expect("a pending move");
            let source = moves[index].from;
            self.emit_park(source);
            parked = Some(index);
            let count = readers.get_mut(&source).expect("a read source");
            *count -= 1;
            if *count == 0
                && let Some(&next) = writer.get(&source)
                && !done[next]
            {
                ready.push(next);
            }
        }
    }

    /// Push `source` below the stack pointer, keeping slot offsets exact.
    fn emit_park(&mut self, source: Location) {
        match source {
            Location::Gp(register) => {
                dynasm!(self.ops ; .arch aarch64 ; str X(register), [sp, #-16]!);
            }
            Location::Fp(register) => {
                dynasm!(self.ops ; .arch aarch64 ; str D(register), [sp, #-16]!);
            }
            slot @ (Location::TaggedSlot(_) | Location::UntaggedSlot(_)) => {
                self.load_slot_gp(16, slot);
                dynasm!(self.ops ; .arch aarch64 ; str x16, [sp, #-16]!);
            }
            Location::Constant(_) => unreachable!("a constant is never a move destination"),
        }
        self.sp_delta += 16;
    }

    /// Pop the parked value into `to`.
    fn emit_unpark(&mut self, to: Location) {
        self.sp_delta -= 16;
        match to {
            Location::Gp(register) => {
                dynasm!(self.ops ; .arch aarch64 ; ldr X(register), [sp], #16);
            }
            Location::Fp(register) => {
                dynasm!(self.ops ; .arch aarch64 ; ldr D(register), [sp], #16);
            }
            slot @ (Location::TaggedSlot(_) | Location::UntaggedSlot(_)) => {
                dynasm!(self.ops ; .arch aarch64 ; ldr x16, [sp], #16);
                self.store_slot_gp(16, slot);
            }
            Location::Constant(_) => unreachable!("a constant is never a move destination"),
        }
    }

    fn gp(location: Location) -> u8 {
        match location {
            Location::Gp(register) => register,
            other => unreachable!("expected a general register, got {other:?}"),
        }
    }

    fn fp(location: Location) -> u8 {
        match location {
            Location::Fp(register) => register,
            other => unreachable!("expected a floating-point register, got {other:?}"),
        }
    }

    // ------------------------------------------------------------------
    // Deopt exits
    // ------------------------------------------------------------------

    /// A label that leaves through `node`'s eager deopt.
    fn eager_exit(&mut self, node: NodeId, reason: DeoptReason) -> DynamicLabel {
        let (reason, action) = exit_reason(reason);
        self.typed_exit(node, reason, action)
    }

    /// A label that leaves through `node`'s eager deopt with this reason and
    /// action.
    fn typed_exit(&mut self, node: NodeId, reason: ExitReason, action: ExitAction) -> DynamicLabel {
        if let Some(site) = self.exits.iter().find(|site| {
            site.node == node && !site.lazy && site.reason == reason && site.locations.is_none()
        }) {
            return site.label;
        }
        let label = self.ops.new_dynamic_label();
        self.exits.push(ExitSite {
            label,
            node,
            lazy: false,
            reason,
            action,
            locations: None,
        });
        label
    }

    // ------------------------------------------------------------------
    // Body
    // ------------------------------------------------------------------

    /// The label a conditional branch to `target` names: `target` itself,
    /// or in a far body the veneer of the current segment that jumps there.
    fn cond_target(&mut self, target: DynamicLabel) -> DynamicLabel {
        if !self.far {
            return target;
        }
        if let Some(&(_, veneer)) = self.veneers.iter().find(|(known, _)| *known == target) {
            return veneer;
        }
        let veneer = self.ops.new_dynamic_label();
        self.veneers.push((target, veneer));
        veneer
    }

    /// Close the current segment with an island once it is long enough.
    fn maybe_island(&mut self) {
        if !self.far || self.ops.offset().0 - self.island_base < ISLAND_INTERVAL {
            return;
        }
        let skip = self.ops.new_dynamic_label();
        dynasm!(self.ops ; .arch aarch64 ; b =>skip);
        self.flush_segment();
        dynasm!(self.ops ; .arch aarch64 ; =>skip);
        self.island_base = self.ops.offset().0;
    }

    /// The slow paths, exit stubs and veneers the segment so far named.
    fn flush_segment(&mut self) {
        while let Some(deferred) = self.deferred.pop() {
            deferred(self);
        }
        let deopt = self.deopt;
        for index in self.exits_flushed..self.exits.len() {
            let label = self.exits[index].label;
            dynasm!(self.ops ; .arch aarch64 ; =>label ; movz w17, index as u32 ; b =>deopt);
        }
        self.exits_flushed = self.exits.len();
        for (target, veneer) in std::mem::take(&mut self.veneers) {
            dynasm!(self.ops ; .arch aarch64 ; =>veneer ; b =>target);
        }
    }

    fn emit_body(&mut self) -> Result<(), Unsupported> {
        for (position, &block) in self.layout.iter().enumerate() {
            let label = self.labels[&block];
            dynasm!(self.ops ; .arch aarch64 ; =>label);
            let data = self.graph.block(block);
            for &phi in &data.phis {
                let allocation = self.allocation.node(phi);
                if allocation.skipped {
                    continue;
                }
                if let (Some(result), Some(&slot)) =
                    (allocation.result, self.allocation.spill.get(&phi))
                    && result != slot
                {
                    self.emit_move(result, slot);
                }
            }
            for &node in &data.body {
                let allocation = self.allocation.node(node);
                if allocation.skipped {
                    continue;
                }
                self.maybe_island();
                self.node_offsets.push((self.ops.offset().0, node));
                for m in allocation.moves.clone() {
                    self.emit_move(m.from, m.to);
                }
                self.emit_node(node)?;
                if let (Some(result), Some(&slot)) =
                    (allocation.result, self.allocation.spill.get(&node))
                    && result != slot
                {
                    self.emit_move(result, slot);
                }
            }
            let control = data.control.expect("a terminated block");
            self.node_offsets.push((self.ops.offset().0, control));
            for m in self.allocation.node(control).moves.clone() {
                self.emit_move(m.from, m.to);
            }
            let next = self.layout.get(position + 1).copied();
            self.emit_control(block, control, next)?;
        }
        Ok(())
    }

    fn emit_node(&mut self, node: NodeId) -> Result<(), Unsupported> {
        let data = self.graph.node(node);
        let allocation = self.allocation.node(node);
        let input = |index: usize| allocation.inputs[index];
        let result = allocation.result;
        match &data.kind {
            Kind::InitialRegister(register) | Kind::LoadWindow(register) => {
                let destination = Self::gp(result.expect("a result"));
                let offset = u32::from(*register) * 8;
                dynasm!(self.ops ; .arch aarch64 ; ldr X(destination), [x19, offset]);
            }
            Kind::StoreWindow(register) => {
                let source = Self::gp(input(0));
                let offset = u32::from(*register) * 8;
                dynasm!(self.ops ; .arch aarch64 ; str X(source), [x19, offset]);
            }
            Kind::LoadThis => {
                let destination = Self::gp(result.expect("a result"));
                dynasm!(self.ops ; .arch aarch64 ; ldr X(destination), [x21, NATIVE_FRAME_THIS_OFFSET]);
            }
            Kind::LoadClosure => {
                let destination = Self::gp(result.expect("a result"));
                dynasm!(self.ops ; .arch aarch64 ; ldr X(destination), [x21, NATIVE_FRAME_SELF_OFFSET]);
            }
            Kind::LoadStringConstant(byte_pc) => {
                let destination = Self::gp(result.expect("a result"));
                let byte_pc = *byte_pc;
                let cell = self
                    .view_of(node)
                    .string_constant_cells
                    .get(&byte_pc)
                    .ok_or(Unsupported::OperandShape("graph LoadString cell"))?
                    .cell_addr;
                let function_id = self.view_of(node).code_block.id;
                emit_load_symbol_u64(
                    &mut self.ops,
                    &mut self.relocations,
                    destination,
                    cell as u64,
                    RelocationTarget::StringConstantCell {
                        function_id,
                        byte_pc,
                    },
                );
                dynasm!(self.ops ; .arch aarch64 ; ldr X(destination), [X(destination)]);
            }
            Kind::ToBoolean | Kind::LogicalNot => {
                let value = Self::gp(input(0));
                let destination = Self::gp(result.expect("a result"));
                let negate = data.kind == Kind::LogicalNot;
                if self.graph.node(data.inputs[0]).kind.produces_boolean() {
                    if negate {
                        dynasm!(self.ops ; .arch aarch64 ; eor XSP(destination), X(value), 1);
                    } else if destination != value {
                        dynasm!(self.ops ; .arch aarch64 ; mov X(destination), X(value));
                    }
                } else {
                    self.emit_to_boolean(node, value, destination, negate);
                }
            }
            Kind::LoadNewTarget => {
                let destination = Self::gp(result.expect("a result"));
                dynasm!(self.ops ; .arch aarch64 ; ldr X(destination), [x21, NEW_TARGET_OFFSET]);
            }
            Kind::Int32Add | Kind::Int32Sub => {
                let (a, b) = (Self::gp(input(0)), Self::gp(input(1)));
                let destination = Self::gp(result.expect("a result"));
                let exit = self.eager_exit(node, DeoptReason::Overflow);
                if data.kind == Kind::Int32Add {
                    dynasm!(self.ops ; .arch aarch64 ; adds w16, W(a), W(b));
                } else {
                    dynasm!(self.ops ; .arch aarch64 ; subs w16, W(a), W(b));
                }
                dynasm!(self.ops ; .arch aarch64 ; b.vs =>exit ; mov W(destination), w16);
            }
            Kind::Int32Mul => {
                let (a, b) = (Self::gp(input(0)), Self::gp(input(1)));
                let destination = Self::gp(result.expect("a result"));
                let overflow = self.eager_exit(node, DeoptReason::Overflow);
                let minus_zero = self.eager_exit(node, DeoptReason::MinusZero);
                let done = self.ops.new_dynamic_label();
                dynasm!(self.ops
                    ; .arch aarch64
                    ; smull x16, W(a), W(b)
                    ; cmp x16, w16, sxtw
                    ; b.ne =>overflow
                    ; cbnz w16, =>done
                    ; orr w17, W(a), W(b)
                    ; cmp w17, #0
                    ; b.lt =>minus_zero
                    ; =>done
                    ; mov W(destination), w16
                );
            }
            Kind::Int32Div => {
                let (a, b) = (Self::gp(input(0)), Self::gp(input(1)));
                let destination = Self::gp(result.expect("a result"));
                let exit = self.eager_exit(node, DeoptReason::LostPrecision);
                let minus_zero = self.eager_exit(node, DeoptReason::MinusZero);
                let overflow = self.eager_exit(node, DeoptReason::Overflow);
                let nonzero = self.ops.new_dynamic_label();
                let no_overflow = self.ops.new_dynamic_label();
                dynasm!(self.ops
                    ; .arch aarch64
                    ; cmp WSP(b), #0
                    ; b.eq =>exit
                    // 0 / negative is -0.
                    ; cbnz W(a), =>nonzero
                    ; cmp WSP(b), #0
                    ; b.lt =>minus_zero
                    ; =>nonzero
                    // i32::MIN / -1 overflows.
                    ; cmn WSP(b), #1
                    ; b.ne =>no_overflow
                    ; movz w16, #0x8000, lsl #16
                    ; cmp W(a), w16
                    ; b.eq =>overflow
                    ; =>no_overflow
                    ; sdiv w16, W(a), W(b)
                    ; msub w17, w16, W(b), W(a)
                    ; cmp w17, #0
                    ; b.ne =>exit
                    ; mov W(destination), w16
                );
            }
            Kind::Int32Mod => {
                let (a, b) = (Self::gp(input(0)), Self::gp(input(1)));
                let destination = Self::gp(result.expect("a result"));
                let exit = self.eager_exit(node, DeoptReason::LostPrecision);
                let minus_zero = self.eager_exit(node, DeoptReason::MinusZero);
                let done = self.ops.new_dynamic_label();
                dynasm!(self.ops
                    ; .arch aarch64
                    ; cmp WSP(b), #0
                    ; b.eq =>exit
                    ; sdiv w16, W(a), W(b)
                    ; msub w16, w16, W(b), W(a)
                    ; cbnz w16, =>done
                    // A zero remainder of a negative dividend is -0.
                    ; cmp WSP(a), #0
                    ; b.lt =>minus_zero
                    ; =>done
                    ; mov W(destination), w16
                );
            }
            Kind::Int32Negate => {
                let a = Self::gp(input(0));
                let destination = Self::gp(result.expect("a result"));
                let minus_zero = self.eager_exit(node, DeoptReason::MinusZero);
                let overflow = self.eager_exit(node, DeoptReason::Overflow);
                dynasm!(self.ops
                    ; .arch aarch64
                    ; cmp WSP(a), #0
                    ; b.eq =>minus_zero
                    ; negs w16, W(a)
                    ; b.vs =>overflow
                    ; mov W(destination), w16
                );
            }
            Kind::Int32BitAnd
            | Kind::Int32BitOr
            | Kind::Int32BitXor
            | Kind::Int32ShiftLeft
            | Kind::Int32ShiftRight => {
                let (a, b) = (Self::gp(input(0)), Self::gp(input(1)));
                let destination = Self::gp(result.expect("a result"));
                match data.kind {
                    Kind::Int32BitAnd => {
                        dynasm!(self.ops ; .arch aarch64 ; and W(destination), W(a), W(b))
                    }
                    Kind::Int32BitOr => {
                        dynasm!(self.ops ; .arch aarch64 ; orr W(destination), W(a), W(b))
                    }
                    Kind::Int32BitXor => {
                        dynasm!(self.ops ; .arch aarch64 ; eor W(destination), W(a), W(b))
                    }
                    Kind::Int32ShiftLeft => {
                        dynasm!(self.ops ; .arch aarch64 ; lsl W(destination), W(a), W(b))
                    }
                    _ => dynasm!(self.ops ; .arch aarch64 ; asr W(destination), W(a), W(b)),
                }
            }
            Kind::Int32ShiftRightLogical => {
                let (a, b) = (Self::gp(input(0)), Self::gp(input(1)));
                let destination = Self::gp(result.expect("a result"));
                let exit = self.eager_exit(node, DeoptReason::LostPrecision);
                dynasm!(self.ops
                    ; .arch aarch64
                    ; lsr w16, W(a), W(b)
                    ; cmp w16, #0
                    ; b.lt =>exit
                    ; mov W(destination), w16
                );
            }
            Kind::Uint32ShiftRightToFloat64 => {
                let (a, b) = (Self::gp(input(0)), Self::gp(input(1)));
                let destination = Self::fp(result.expect("a result"));
                dynasm!(self.ops
                    ; .arch aarch64
                    ; lsr w16, W(a), W(b)
                    ; ucvtf D(destination), w16
                );
            }
            Kind::Int32BitNot => {
                let a = Self::gp(input(0));
                let destination = Self::gp(result.expect("a result"));
                dynasm!(self.ops ; .arch aarch64 ; mvn W(destination), W(a));
            }
            Kind::Int32Compare(condition) => {
                let (a, b) = (Self::gp(input(0)), Self::gp(input(1)));
                let destination = Self::gp(result.expect("a result"));
                dynasm!(self.ops ; .arch aarch64 ; cmp W(a), W(b));
                self.emit_cset_bool(destination, *condition, false);
            }
            Kind::Float64Add | Kind::Float64Sub | Kind::Float64Mul | Kind::Float64Div => {
                let (a, b) = (Self::fp(input(0)), Self::fp(input(1)));
                let destination = Self::fp(result.expect("a result"));
                match data.kind {
                    Kind::Float64Add => {
                        dynasm!(self.ops ; .arch aarch64 ; fadd D(destination), D(a), D(b))
                    }
                    Kind::Float64Sub => {
                        dynasm!(self.ops ; .arch aarch64 ; fsub D(destination), D(a), D(b))
                    }
                    Kind::Float64Mul => {
                        dynasm!(self.ops ; .arch aarch64 ; fmul D(destination), D(a), D(b))
                    }
                    _ => dynasm!(self.ops ; .arch aarch64 ; fdiv D(destination), D(a), D(b)),
                }
            }
            Kind::Float64Mod => {
                let (a, b) = (Self::fp(input(0)), Self::fp(input(1)));
                let destination = Self::fp(result.expect("a result"));
                // `fmod` through the VM's typed leaf, every live register
                // saved around it: the leaf follows the platform ABI.
                let live = self.allocation.node(node).live_registers.clone();
                let saved = self.emit_save_registers(&live);
                dynasm!(self.ops
                    ; .arch aarch64
                    ; fmov d31, D(b)
                    ; fmov d0, D(a)
                    ; fmov d1, d31
                );
                emit_load_symbol_u64(
                    &mut self.ops,
                    &mut self.relocations,
                    16,
                    otter_vm::runtime_stubs::NUMBER_REM_F64_LEAF.entry_addr() as u64,
                    RelocationTarget::runtime_stub(abi::STUB_NUMBER_REM_F64_LEAF),
                );
                dynasm!(self.ops ; .arch aarch64 ; blr x16 ; fmov d31, d0);
                self.emit_restore_registers(&live, saved);
                dynasm!(self.ops ; .arch aarch64 ; fmov D(destination), d31);
            }
            Kind::Float64Negate => {
                let a = Self::fp(input(0));
                let destination = Self::fp(result.expect("a result"));
                dynasm!(self.ops ; .arch aarch64 ; fneg D(destination), D(a));
            }
            Kind::Float64Compare(condition) => {
                let (a, b) = (Self::fp(input(0)), Self::fp(input(1)));
                let destination = Self::gp(result.expect("a result"));
                dynasm!(self.ops ; .arch aarch64 ; fcmp D(a), D(b));
                self.emit_cset_bool(destination, *condition, true);
            }
            Kind::CheckedTaggedToInt32 => {
                let a = Self::gp(input(0));
                let destination = Self::gp(result.expect("a result"));
                let exit = self.eager_exit(node, DeoptReason::WrongType);
                self.load_immediate(16, NUMBER_TAG);
                dynasm!(self.ops
                    ; .arch aarch64
                    ; cmp X(a), x16
                    ; b.lo =>exit
                    ; mov W(destination), W(a)
                );
            }
            Kind::CheckedTaggedToFloat64 => {
                let a = Self::gp(input(0));
                let destination = Self::fp(result.expect("a result"));
                let exit = self.eager_exit(node, DeoptReason::WrongType);
                let int = self.ops.new_dynamic_label();
                let done = self.ops.new_dynamic_label();
                self.load_immediate(16, NUMBER_TAG);
                dynasm!(self.ops
                    ; .arch aarch64
                    ; cmp X(a), x16
                    ; b.hs =>int
                    ; tst X(a), x16
                    ; b.eq =>exit
                );
                self.load_immediate(17, DOUBLE_OFFSET);
                dynasm!(self.ops
                    ; .arch aarch64
                    ; sub x16, X(a), x17
                    ; fmov D(destination), x16
                    ; b =>done
                    ; =>int
                    ; scvtf D(destination), W(a)
                    ; =>done
                );
            }
            Kind::Int32ToTagged => {
                let a = Self::gp(input(0));
                let destination = Self::gp(result.expect("a result"));
                self.load_immediate(16, NUMBER_TAG);
                dynasm!(self.ops
                    ; .arch aarch64
                    ; mov W(destination), W(a)
                    ; orr X(destination), X(destination), x16
                );
            }
            Kind::Float64ToTagged => {
                let a = Self::fp(input(0));
                let destination = Self::gp(result.expect("a result"));
                self.emit_box_float64(a, destination);
            }
            Kind::Int32ToFloat64 => {
                let a = Self::gp(input(0));
                let destination = Self::fp(result.expect("a result"));
                dynasm!(self.ops ; .arch aarch64 ; scvtf D(destination), W(a));
            }
            Kind::TruncateFloat64ToInt32 => {
                let a = Self::fp(input(0));
                let destination = Self::gp(result.expect("a result"));
                if crate::arm64::has_javascript_conversion() {
                    crate::arm64::emit_fjcvtzs(&mut self.ops, a, destination);
                } else {
                    return Err(Unsupported::OperandShape("ToInt32 without FJCVTZS"));
                }
            }
            Kind::CheckedFloat64ToInt32 => {
                let a = Self::fp(input(0));
                let destination = Self::gp(result.expect("a result"));
                let exit = self.eager_exit(node, DeoptReason::LostPrecision);
                let minus_zero = self.eager_exit(node, DeoptReason::MinusZero);
                let done = self.ops.new_dynamic_label();
                dynasm!(self.ops
                    ; .arch aarch64
                    ; fcvtzs w16, D(a)
                    ; scvtf d31, w16
                    ; fcmp D(a), d31
                    ; b.ne =>exit
                    ; cbnz w16, =>done
                    ; fmov x17, D(a)
                    ; cmp x17, #0
                    ; b.lt =>minus_zero
                    ; =>done
                    ; mov W(destination), w16
                );
            }
            Kind::CheckedTaggedToIndex => {
                let a = Self::gp(input(0));
                let destination = Self::gp(result.expect("a result"));
                let double = allocation.fp_temps[0];
                let exit = self.eager_exit(node, DeoptReason::InvalidIndex);
                let int = self.ops.new_dynamic_label();
                let done = self.ops.new_dynamic_label();
                self.load_immediate(16, NUMBER_TAG);
                dynasm!(self.ops
                    ; .arch aarch64
                    ; cmp X(a), x16
                    ; b.hs =>int
                    ; tst X(a), x16
                    ; b.eq =>exit
                );
                self.load_immediate(17, DOUBLE_OFFSET);
                dynasm!(self.ops
                    ; .arch aarch64
                    ; sub x17, X(a), x17
                    ; fmov D(double), x17
                    ; fcvtzs w16, D(double)
                    ; scvtf d31, w16
                    ; fcmp D(double), d31
                    ; b.ne =>exit
                    ; mov W(destination), w16
                    ; b =>done
                    ; =>int
                    ; mov W(destination), W(a)
                    ; =>done
                );
            }
            Kind::CheckedFloat64ToIndex => {
                let a = Self::fp(input(0));
                let destination = Self::gp(result.expect("a result"));
                let exit = self.eager_exit(node, DeoptReason::InvalidIndex);
                dynasm!(self.ops
                    ; .arch aarch64
                    ; fcvtzs w16, D(a)
                    ; scvtf d31, w16
                    ; fcmp D(a), d31
                    ; b.ne =>exit
                    ; mov W(destination), w16
                );
            }
            Kind::TaggedEqual => {
                let (a, b) = (Self::gp(input(0)), Self::gp(input(1)));
                let destination = Self::gp(result.expect("a result"));
                dynasm!(self.ops ; .arch aarch64 ; cmp X(a), X(b));
                self.emit_cset_bool(destination, Condition::Equal, false);
            }
            Kind::StrictEqual { negate } => {
                let (a, b) = (Self::gp(input(0)), Self::gp(input(1)));
                let destination = Self::gp(result.expect("a result"));
                let condition = if *negate {
                    Condition::NotEqual
                } else {
                    Condition::Equal
                };
                let oddball = |input: NodeId| match self.graph.node(input).kind {
                    Kind::ConstTagged(bits) => !tag::is_number_bits(bits),
                    _ => false,
                };
                if oddball(data.inputs[0]) || oddball(data.inputs[1]) {
                    dynasm!(self.ops ; .arch aarch64 ; cmp X(a), X(b));
                    self.emit_cset_bool(destination, condition, false);
                } else {
                    let double = allocation.fp_temps[0];
                    self.emit_strict_equal(node, [a, b], double, destination, condition);
                }
            }
            Kind::CheckHeapObject => {
                let a = Self::gp(input(0));
                let exit = self.eager_exit(node, DeoptReason::WrongType);
                self.load_immediate(16, NOT_CELL_MASK);
                dynasm!(self.ops ; .arch aarch64 ; tst X(a), x16 ; b.ne =>exit);
            }
            Kind::CheckNumber => {
                let a = Self::gp(input(0));
                let exit = self.eager_exit(node, DeoptReason::WrongType);
                self.load_immediate(16, NUMBER_TAG);
                dynasm!(self.ops ; .arch aarch64 ; tst X(a), x16 ; b.eq =>exit);
            }
            Kind::CheckShapes { shapes, writable } => {
                let a = Self::gp(input(0));
                let exit = self.eager_exit(node, DeoptReason::WrongShape);
                let (shapes, writable) = (shapes.clone(), *writable);
                self.emit_check_shapes(a, &shapes, writable, exit);
            }
            Kind::CheckValue(bits) => {
                let a = Self::gp(input(0));
                let exit = self.eager_exit(node, DeoptReason::WrongValue);
                self.load_immediate(16, *bits);
                dynasm!(self.ops ; .arch aarch64 ; cmp X(a), x16 ; b.ne =>exit);
            }
            Kind::CheckBounds => {
                let (index, length) = (Self::gp(input(0)), Self::gp(input(1)));
                let exit = self.eager_exit(node, DeoptReason::OutOfBounds);
                // Sign-extended, a negative index is above every length.
                dynasm!(self.ops
                    ; .arch aarch64
                    ; cmp XSP(length), W(index), sxtw
                    ; b.ls =>exit
                );
            }
            Kind::CheckElements {
                type_tag,
                guards,
                holes,
                cached_base,
            } => {
                let receiver = Self::gp(input(0));
                let exit = self.eager_exit(node, DeoptReason::WrongShape);
                let (type_tag, guards, holes, cached_base) =
                    (*type_tag, *guards, *holes, *cached_base);
                self.load_immediate(16, NOT_CELL_MASK);
                dynasm!(self.ops
                    ; .arch aarch64
                    ; tst X(receiver), x16
                    ; b.ne =>exit
                    ; ldrb w17, [X(receiver)]
                    ; cmp w17, u32::from(type_tag)
                    ; b.ne =>exit
                );
                for guard in guards.iter().flatten() {
                    self.emit_body_guard(receiver, *guard, exit);
                }
                if let Some(cached) = cached_base {
                    // The cached base stands for the buffer's liveness and
                    // extent only while no buffer was ever detached.
                    dynasm!(self.ops
                        ; .arch aarch64
                        ; ldr x16, [X(receiver), cached]
                        ; cbz x16, =>exit
                        ; ldr x17, [x20, THREAD_OFFSET]
                        ; ldr x17, [x17, DETACH_PROTECTOR]
                        ; cbz x17, =>exit
                        ; ldrb w17, [x17]
                        ; cbnz w17, =>exit
                    );
                }
                if let Some(holes) = holes {
                    let kind = holes.kind_byte;
                    dynasm!(self.ops
                        ; .arch aarch64
                        ; ldrb w17, [X(receiver), kind]
                        ; cmp w17, u32::from(holes.packed_kind)
                        ; ccmp w17, u32::from(holes.holey_kind), 4, ne
                        ; b.ne =>exit
                    );
                }
            }
            Kind::CheckElementPresent => {
                let (base, index) = (Self::gp(input(0)), Self::gp(input(1)));
                let exit = self.eager_exit(node, DeoptReason::OutOfBounds);
                dynasm!(self.ops ; .arch aarch64 ; ldr x16, [X(base), W(index), uxtw #3]);
                self.load_immediate(17, VALUE_HOLE);
                dynasm!(self.ops ; .arch aarch64 ; cmp x16, x17 ; b.eq =>exit);
            }
            Kind::LoadElementsLength { byte, width } => {
                let receiver = Self::gp(input(0));
                let destination = Self::gp(result.expect("a result"));
                let byte = *byte;
                match width {
                    JitGuardWidth::Byte => {
                        dynasm!(self.ops ; .arch aarch64 ; ldrb W(destination), [X(receiver), byte])
                    }
                    JitGuardWidth::Word32 => {
                        dynasm!(self.ops ; .arch aarch64 ; ldr W(destination), [X(receiver), byte])
                    }
                    JitGuardWidth::Word64 => {
                        dynasm!(self.ops ; .arch aarch64 ; ldr X(destination), [X(receiver), byte])
                    }
                }
            }
            Kind::LoadElementsBase(byte) => {
                let receiver = Self::gp(input(0));
                let destination = Self::gp(result.expect("a result"));
                let byte = *byte;
                dynasm!(self.ops ; .arch aarch64 ; ldr X(destination), [X(receiver), byte]);
            }
            Kind::LoadElement(element) => {
                let (base, index) = (Self::gp(input(0)), Self::gp(input(1)));
                let element = *element;
                let result = result.expect("a result");
                self.emit_load_element(node, element, base, index, result);
            }
            Kind::LoadHoleyFloat64Element(holes) => {
                let (base, index) = (Self::gp(input(0)), Self::gp(input(1)));
                let destination = Self::gp(result.expect("a result"));
                let double = allocation.fp_temps[0];
                let holes = *holes;
                self.emit_load_holey_float64(node, holes, [base, index], double, destination);
            }
            Kind::CheckHoleyElementPresent(holes) => {
                let (base, index) = (Self::gp(input(0)), Self::gp(input(1)));
                let exit = self.eager_exit(node, DeoptReason::OutOfBounds);
                let holes = *holes;
                self.emit_hole_bit(holes, base, index);
                dynasm!(self.ops ; .arch aarch64 ; b.ne =>exit);
            }
            Kind::LoadElementUint32ToFloat64 => {
                let (base, index) = (Self::gp(input(0)), Self::gp(input(1)));
                let destination = Self::fp(result.expect("a result"));
                dynasm!(self.ops
                    ; .arch aarch64
                    ; ldr w16, [X(base), W(index), uxtw #2]
                    ; ucvtf D(destination), w16
                );
            }
            Kind::StoreElement(element) => {
                let (base, index) = (Self::gp(input(0)), Self::gp(input(1)));
                let element = *element;
                self.emit_store_element(element, base, index, input(2))?;
            }
            Kind::LoadSlotBase => {
                let object = Self::gp(input(0));
                let destination = Self::gp(result.expect("a result"));
                let slab = self.view.object_slab_handle_byte;
                let words = self.view.object_slab_words_byte;
                let inline = self.view.object_inline_values_byte;
                dynasm!(self.ops
                    ; .arch aarch64
                    ; ldr w16, [X(object), slab]
                    ; and x17, X(object), #0xffff_ffff_0000_0000
                    ; orr x17, x17, x16
                    ; add x17, x17, words
                    ; cmp w16, #0
                    ; add x16, XSP(object), inline
                    ; csel X(destination), x16, x17, eq
                );
            }
            Kind::LoadTaggedField(offset) => {
                let base = Self::gp(input(0));
                let destination = Self::gp(result.expect("a result"));
                let offset = *offset;
                if (0..=32760).contains(&offset) && offset % 8 == 0 {
                    dynasm!(self.ops ; .arch aarch64 ; ldr X(destination), [X(base), offset as u32]);
                } else {
                    self.load_immediate(16, offset as i64 as u64);
                    dynasm!(self.ops ; .arch aarch64 ; ldr X(destination), [X(base), x16]);
                }
            }
            Kind::StoreTaggedField(offset) => {
                let (base, value) = (Self::gp(input(0)), Self::gp(input(1)));
                let offset = *offset;
                if (0..=32760).contains(&offset) && offset % 8 == 0 {
                    dynasm!(self.ops ; .arch aarch64 ; str X(value), [X(base), offset as u32]);
                } else {
                    self.load_immediate(16, offset as i64 as u64);
                    dynasm!(self.ops ; .arch aarch64 ; str X(value), [X(base), x16]);
                }
            }
            Kind::StoreShape(shape) => {
                let object = Self::gp(input(0));
                let byte = self.view.object_shape_byte;
                self.load_immediate(16, u64::from(*shape));
                dynasm!(self.ops ; .arch aarch64 ; str w16, [X(object), byte]);
            }
            Kind::LoadClosureContext => {
                let closure = Self::gp(input(0));
                let destination = Self::gp(result.expect("a result"));
                let context = self.view.closure_call_layout.context_byte;
                let bare = self.ops.new_dynamic_label();
                let done = self.ops.new_dynamic_label();
                self.load_immediate(16, NOT_CELL_MASK);
                dynasm!(self.ops
                    ; .arch aarch64
                    ; tst X(closure), x16
                    ; b.ne =>bare
                    ; ldrb w16, [X(closure)]
                    ; cmp w16, otter_vm::closure::JS_CLOSURE_BODY_TYPE_TAG as u32
                    ; b.ne =>bare
                    ; ldr X(destination), [X(closure), context]
                    ; b =>done
                    ; =>bare
                );
                self.load_immediate(destination, VALUE_UNDEFINED);
                dynasm!(self.ops ; .arch aarch64 ; =>done);
            }
            Kind::LoadNamedProperty(byte_pc) => {
                let receiver = Self::gp(input(0));
                let destination = Self::gp(result.expect("a result"));
                let [holder, scratch] = [allocation.gp_temps[0], allocation.gp_temps[1]];
                let byte_pc = *byte_pc;
                self.emit_load_named_property(
                    node,
                    byte_pc,
                    receiver,
                    [holder, scratch],
                    destination,
                )?;
            }
            Kind::StoreNamedProperty(byte_pc) => {
                let (receiver, value) = (Self::gp(input(0)), Self::gp(input(1)));
                let [holder, scratch] = [allocation.gp_temps[0], allocation.gp_temps[1]];
                let byte_pc = *byte_pc;
                self.emit_store_named_property(
                    node,
                    byte_pc,
                    [receiver, value],
                    [holder, scratch],
                )?;
            }
            Kind::LoadPropertyCached { pc, atom } => {
                let receiver = Self::gp(input(0));
                let destination = Self::gp(result.expect("a result"));
                let temps = [
                    allocation.gp_temps[0],
                    allocation.gp_temps[1],
                    allocation.gp_temps[2],
                    allocation.gp_temps[3],
                ];
                let (pc, atom) = (*pc, *atom);
                self.emit_load_property_cached(node, pc, atom, receiver, temps, destination)?;
            }
            Kind::StorePropertyCached { pc, atom } => {
                let (receiver, value) = (Self::gp(input(0)), Self::gp(input(1)));
                let temps = [
                    allocation.gp_temps[0],
                    allocation.gp_temps[1],
                    allocation.gp_temps[2],
                    allocation.gp_temps[3],
                ];
                let (pc, atom) = (*pc, *atom);
                self.emit_store_property_cached(node, pc, atom, [receiver, value], temps)?;
            }
            Kind::LoadContextParent => {
                let context = Self::gp(input(0));
                let destination = Self::gp(result.expect("a result"));
                let parent = self.view.context_layout.parent_byte;
                dynasm!(self.ops ; .arch aarch64 ; ldr X(destination), [X(context), parent]);
            }
            Kind::WriteBarrier => {
                let (object, value) = (Self::gp(input(0)), Self::gp(input(1)));
                self.emit_write_barrier(node, object, value);
            }
            Kind::ElementWriteBarrier => {
                let (base, index, value) =
                    (Self::gp(input(0)), Self::gp(input(1)), Self::gp(input(2)));
                self.emit_element_write_barrier(node, base, index, value);
            }
            Kind::Generic { pc, registers } => {
                let (pc, registers) = (*pc, registers.clone());
                self.emit_generic(node, pc, &registers)?;
            }
            Kind::CallJs {
                pc,
                plan,
                construct,
                receiver,
                allocation,
            } => {
                let (pc, plan, construct, receiver, allocation) =
                    (*pc, *plan, *construct, *receiver, *allocation);
                self.emit_call_js(node, pc, plan, construct, receiver, allocation)?;
            }
            Kind::CheckFunction { function_id, cell } => {
                let value = Self::gp(input(0));
                let (function_id, cell) = (*function_id, *cell);
                self.emit_check_function(node, function_id, cell, value);
            }
            Kind::Instanceof => {
                let (value, target) = (Self::gp(input(0)), Self::gp(input(1)));
                let destination = Self::gp(result.expect("a result"));
                let temps = [
                    allocation.gp_temps[0],
                    allocation.gp_temps[1],
                    allocation.gp_temps[2],
                    allocation.gp_temps[3],
                    allocation.gp_temps[4],
                ];
                self.emit_instanceof(node, [value, target], temps, destination);
            }
            Kind::CheckNotHole => {
                let value = Self::gp(input(0));
                let exit = self.eager_exit(node, DeoptReason::WrongValue);
                self.load_immediate(16, tag::VALUE_HOLE);
                dynasm!(self.ops ; .arch aarch64 ; cmp X(value), x16 ; b.eq =>exit);
            }
            Kind::CheckFunctionPrototypeCall(byte_pc) => {
                let value = Self::gp(input(0));
                let temp = allocation.gp_temps[0];
                let byte_pc = *byte_pc;
                self.emit_check_function_prototype_call(node, byte_pc, value, temp)?;
            }
            Kind::LoadGuardedMethod {
                byte_pc,
                target,
                receiver_proved,
            } => {
                let receiver = Self::gp(input(0));
                let destination = Self::gp(result.expect("a result"));
                let holder = allocation.gp_temps[0];
                let (byte_pc, target, receiver_proved) = (*byte_pc, *target, *receiver_proved);
                self.emit_load_guarded_method(
                    node,
                    (byte_pc, target),
                    receiver_proved,
                    receiver,
                    holder,
                    destination,
                )?;
            }
            Kind::LoadReceiverShape => {
                let receiver = Self::gp(input(0));
                let destination = Self::gp(result.expect("a result"));
                let other = self.ops.new_dynamic_label();
                let done = self.ops.new_dynamic_label();
                self.emit_ordinary_receiver(receiver, 0, other);
                let shape_byte = self.view.object_shape_byte;
                dynasm!(self.ops
                    ; .arch aarch64
                    ; ldr W(destination), [X(receiver), shape_byte]
                    ; b =>done
                    ; =>other
                    ; mov X(destination), xzr
                    ; =>done
                );
            }
            other => {
                return Err(Unsupported::OperandShape(node_name(other)));
            }
        }
        Ok(())
    }

    /// `X(destination)` = the tagged boolean of `X(a) === X(b)` under
    /// `condition` (`Equal` for `===`, `NotEqual` for `!==`). Numbers compare
    /// by value, identical words are equal, two cells of one string or BigInt
    /// type compare by content through the leaf probe, and anything else
    /// differs. Reads both inputs before it writes the result.
    fn emit_strict_equal(
        &mut self,
        node: NodeId,
        [a, b]: [u8; 2],
        double: u8,
        destination: u8,
        condition: Condition,
    ) {
        let lhs_non_number = self.ops.new_dynamic_label();
        let a_int = self.ops.new_dynamic_label();
        let a_done = self.ops.new_dynamic_label();
        let b_int = self.ops.new_dynamic_label();
        let b_done = self.ops.new_dynamic_label();
        let cells = self.ops.new_dynamic_label();
        let leaf = self.ops.new_dynamic_label();
        let equal = self.ops.new_dynamic_label();
        let differ = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        self.load_immediate(16, NUMBER_TAG);
        dynasm!(self.ops
            ; .arch aarch64
            ; tst X(a), x16
            ; b.eq =>lhs_non_number
            ; tst X(b), x16
            ; b.eq =>differ
            // Two Numbers: compare their values; NaN is unordered.
            ; cmp X(a), x16
            ; b.hs =>a_int
        );
        self.load_immediate(17, DOUBLE_OFFSET);
        dynasm!(self.ops
            ; .arch aarch64
            ; sub x17, X(a), x17
            ; fmov d31, x17
            ; b =>a_done
            ; =>a_int
            ; scvtf d31, W(a)
            ; =>a_done
            ; cmp X(b), x16
            ; b.hs =>b_int
        );
        self.load_immediate(17, DOUBLE_OFFSET);
        dynasm!(self.ops
            ; .arch aarch64
            ; sub x17, X(b), x17
            ; fmov D(double), x17
            ; b =>b_done
            ; =>b_int
            ; scvtf D(double), W(b)
            ; =>b_done
            ; fcmp d31, D(double)
        );
        self.emit_cset_bool(destination, condition, true);
        dynasm!(self.ops
            ; .arch aarch64
            ; b =>done
            ; =>lhs_non_number
            ; tst X(b), x16
            ; b.ne =>differ
        );
        self.load_immediate(16, NOT_CELL_MASK);
        dynasm!(self.ops
            ; .arch aarch64
            ; tst X(a), x16
            ; b.eq =>cells
            ; tst X(b), x16
            ; b.eq =>cells
            // Two immediates: identity.
            ; cmp X(a), X(b)
            ; b.eq =>equal
            ; b =>differ
            ; =>cells
            ; cmp X(a), X(b)
            ; b.eq =>equal
            // A cell differs from an immediate, and two cells of different
            // types, or of a type compared by identity, differ.
            ; tst X(a), x16
            ; b.ne =>differ
            ; tst X(b), x16
            ; b.ne =>differ
            ; ldrb w16, [X(a)]
            ; ldrb w17, [X(b)]
            ; cmp w16, w17
            ; b.ne =>differ
            ; cmp w16, u32::from(otter_vm::string::JS_STRING_BODY_TYPE_TAG)
            ; b.eq =>leaf
            ; cmp w16, u32::from(otter_vm::bigint::BIG_INT_BODY_TYPE_TAG)
            ; b.eq =>leaf
            ; =>differ
        );
        let (when_equal, when_differ) = if condition == Condition::Equal {
            (VALUE_TRUE, VALUE_FALSE)
        } else {
            (VALUE_FALSE, VALUE_TRUE)
        };
        self.load_immediate(destination, when_differ);
        dynasm!(self.ops ; .arch aarch64 ; b =>done ; =>equal);
        self.load_immediate(destination, when_equal);
        dynasm!(self.ops ; .arch aarch64 ; =>done);
        let live = self.allocation.node(node).live_registers.clone();
        self.deferred
            .push(Box::new(move |codegen: &mut Codegen<'a>| {
                dynasm!(codegen.ops ; .arch aarch64 ; =>leaf);
                let saved = codegen.emit_save_registers(&live);
                // The probe reads two string or BigInt bodies and allocates
                // nothing; its only miss is a null heap, which generated
                // code never passes.
                dynasm!(codegen.ops
                    ; .arch aarch64
                    ; mov x16, X(a)
                    ; mov x17, X(b)
                    ; ldr x0, [x20, THREAD_OFFSET]
                    ; ldr x0, [x0, VM_THREAD_GC_HEAP_OFFSET]
                    ; mov x1, x16
                    ; mov x2, x17
                );
                emit_load_symbol_u64(
                    &mut codegen.ops,
                    &mut codegen.relocations,
                    16,
                    otter_vm::runtime_stubs::STRICT_EQ_LEAF.entry_addr() as u64,
                    RelocationTarget::runtime_stub(abi::STUB_STRICT_EQ_LEAF),
                );
                dynasm!(codegen.ops ; .arch aarch64 ; blr x16 ; mov x16, x0);
                codegen.emit_restore_registers(&live, saved);
                dynasm!(codegen.ops
                    ; .arch aarch64
                    ; cmp x16, VALUE_TRUE as u32
                    ; b.eq =>equal
                    ; b =>differ
                );
            }));
    }

    /// `X(destination)` = the tagged boolean of the flags under `condition`.
    fn emit_cset_bool(&mut self, destination: u8, condition: Condition, float: bool) {
        match (condition, float) {
            (Condition::Equal, _) => dynasm!(self.ops ; .arch aarch64 ; cset W(destination), eq),
            (Condition::NotEqual, _) => dynasm!(self.ops ; .arch aarch64 ; cset W(destination), ne),
            (Condition::Less, false) => dynasm!(self.ops ; .arch aarch64 ; cset W(destination), lt),
            (Condition::Less, true) => dynasm!(self.ops ; .arch aarch64 ; cset W(destination), mi),
            (Condition::LessEqual, false) => {
                dynasm!(self.ops ; .arch aarch64 ; cset W(destination), le)
            }
            (Condition::LessEqual, true) => {
                dynasm!(self.ops ; .arch aarch64 ; cset W(destination), ls)
            }
            (Condition::Greater, _) => dynasm!(self.ops ; .arch aarch64 ; cset W(destination), gt),
            (Condition::GreaterEqual, _) => {
                dynasm!(self.ops ; .arch aarch64 ; cset W(destination), ge)
            }
        }
        dynasm!(self.ops ; .arch aarch64 ; orr XSP(destination), X(destination), VALUE_FALSE);
    }

    /// Box `D(source)` into `X(destination)` in the canonical encoding.
    fn emit_box_float64(&mut self, source: u8, destination: u8) {
        let double = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        let boxed = self.ops.new_dynamic_label();
        dynasm!(self.ops
            ; .arch aarch64
            ; fcvtzs w16, D(source)
            ; scvtf d31, w16
            ; fcmp D(source), d31
            ; b.ne =>double
            ; cbnz w16, =>boxed
            ; fmov x17, D(source)
            ; cmp x17, #0
            ; b.lt =>double
            ; =>boxed
        );
        self.load_immediate(17, NUMBER_TAG);
        dynasm!(self.ops
            ; .arch aarch64
            ; mov w16, w16
            ; orr X(destination), x16, x17
            ; b =>done
            ; =>double
            ; fmov x16, D(source)
        );
        // NaN boxes as the one canonical pattern.
        let not_nan = self.ops.new_dynamic_label();
        dynasm!(self.ops ; .arch aarch64 ; fcmp D(source), D(source) ; b.vc =>not_nan);
        self.load_immediate(16, tag::CANONICAL_NAN);
        dynasm!(self.ops ; .arch aarch64 ; =>not_nan);
        self.load_immediate(17, DOUBLE_OFFSET);
        dynasm!(self.ops
            ; .arch aarch64
            ; add X(destination), x16, x17
            ; =>done
        );
    }

    /// Exit unless `X(object)` is an ordinary object with one of `shapes`
    /// and no object-local state overriding its shape's slots.
    fn emit_check_shapes(
        &mut self,
        object: u8,
        shapes: &[u32],
        writable: bool,
        exit: DynamicLabel,
    ) {
        let matched = self.ops.new_dynamic_label();
        let shape_byte = self.view.object_shape_byte;
        let flags_byte = self.view.object_flags_byte;
        self.load_immediate(16, NOT_CELL_MASK);
        dynasm!(self.ops
            ; .arch aarch64
            ; tst X(object), x16
            ; b.ne =>exit
            ; ldrb w16, [X(object)]
            ; cmp w16, crate::entry::OBJECT_BODY_TYPE_TAG
            ; b.ne =>exit
            ; ldrb w16, [X(object), flags_byte]
        );
        let mask = u32::from(otter_vm::jit::JIT_OBJECT_ORDINARY_LOOKUP_MASK)
            | if writable {
                u32::from(otter_vm::jit::JIT_OBJECT_FLAG_USED_AS_PROTOTYPE)
            } else {
                0
            };
        dynasm!(self.ops
            ; .arch aarch64
            ; movz w17, mask
            ; tst w16, w17
            ; b.ne =>exit
            ; ldr w16, [X(object), shape_byte]
        );
        for (index, &shape) in shapes.iter().enumerate() {
            self.load_immediate(17, u64::from(shape));
            dynasm!(self.ops ; .arch aarch64 ; cmp w16, w17);
            if index + 1 == shapes.len() {
                dynasm!(self.ops ; .arch aarch64 ; b.ne =>exit);
            } else {
                dynasm!(self.ops ; .arch aarch64 ; b.eq =>matched);
            }
        }
        dynasm!(self.ops ; .arch aarch64 ; =>matched);
    }

    /// The generational barrier for storing `X(value)` into `X(object)`. The
    /// slow path preserves every live register itself.
    /// Branch to `miss` unless `X(value)` is a native function whose
    /// external reference is `native_ref`.
    fn emit_native_identity(&mut self, value: u8, native_ref: u32, miss: DynamicLabel) {
        let native_type_tag = u32::from(self.view.collection_layout.native_function_type_tag);
        let identity_byte = self.view.native_call_layout.identity_byte;
        self.load_immediate(16, NOT_CELL_MASK);
        dynasm!(self.ops
            ; .arch aarch64
            ; tst X(value), x16
            ; b.ne =>miss
            ; cbz X(value), =>miss
            ; ldrb w16, [X(value)]
            ; cmp w16, native_type_tag
            ; b.ne =>miss
            ; ldr w16, [X(value), identity_byte]
        );
        self.load_immediate(17, u64::from(native_ref));
        dynasm!(self.ops ; .arch aarch64 ; cmp w16, w17 ; b.ne =>miss);
    }

    /// Prove the callee of the `f.call` site at `byte_pc` is
    /// `%Function.prototype.call%`: `X(value)` itself, or the `call` slot
    /// the closure receiver `X(value)` reads through the pinned
    /// `%Function.prototype%`.
    fn emit_check_function_prototype_call(
        &mut self,
        node: NodeId,
        byte_pc: u32,
        value: u8,
        temp: u8,
    ) -> Result<(), Unsupported> {
        let site = self
            .view_of(node)
            .function_prototype_calls
            .get(&byte_pc)
            .ok_or(Unsupported::OperandShape("graph f.call site"))?;
        let native_ref = site.proof.call_native_ref;
        let lookup = site.proof.lookup;
        let exit = self.eager_exit(node, DeoptReason::WrongValue);
        let Some(lookup) = lookup else {
            self.emit_native_identity(value, native_ref, exit);
            return Ok(());
        };
        let target = lookup.receiver;
        let view = self.view;
        self.load_immediate(16, NOT_CELL_MASK);
        dynasm!(self.ops
            ; .arch aarch64
            ; tst X(value), x16
            ; b.ne =>exit
            ; cbz X(value), =>exit
            ; ldrb w16, [X(value)]
            ; cmp w16, u32::from(target.type_tag)
            ; b.ne =>exit
        );
        if let Some(guard) = target.guard {
            self.emit_body_guard(value, guard, exit);
        }
        if let Some(realm) = target.active_realm {
            dynasm!(self.ops
                ; .arch aarch64
                ; ldr x16, [x20, THREAD_OFFSET]
                ; ldr x16, [x16, crate::entry::VM_THREAD_ACTIVE_REALM_CELL_OFFSET]
                ; cbz x16, =>exit
                ; ldr w16, [x16]
            );
            self.load_immediate(17, u64::from(realm));
            dynasm!(self.ops ; .arch aarch64 ; cmp w16, w17 ; b.ne =>exit);
        }
        // `%Function.prototype%`, its shape, and the native in its `call`
        // slot.
        emit_load_symbol_u64(
            &mut self.ops,
            &mut self.relocations,
            temp,
            u64::from(target.proto_offset),
            RelocationTarget::GuardedHeapReference {
                component: crate::artifact::relocation::GuardedHeapComponent::Prototype,
                byte_pc,
                runtime_stub_id: abi::STUB_JIT_RESOLVE_METHOD.id,
            },
        );
        let ordinary_mask = u32::from(otter_vm::jit::JIT_OBJECT_ORDINARY_LOOKUP_MASK);
        dynasm!(self.ops
            ; .arch aarch64
            ; and x16, X(value), #0xffff_ffff_0000_0000
            ; add X(temp), X(temp), x16
            ; ldrb w16, [X(temp), view.object_flags_byte]
            ; tst w16, ordinary_mask
            ; b.ne =>exit
            ; ldr w16, [X(temp), view.object_shape_byte]
        );
        self.load_immediate(17, u64::from(lookup.holder_shape));
        let slab = view.object_slab_handle_byte;
        let words = view.object_slab_words_byte;
        let inline = view.object_inline_values_byte;
        dynasm!(self.ops
            ; .arch aarch64
            ; cmp w16, w17
            ; b.ne =>exit
            ; ldr w16, [X(temp), slab]
            ; and x17, X(temp), #0xffff_ffff_0000_0000
            ; orr x17, x17, x16
            ; add x17, x17, words
            ; cmp w16, #0
            ; add x16, XSP(temp), inline
            ; csel x16, x16, x17, eq
            ; ldr X(temp), [x16, lookup.call_value_byte]
        );
        self.emit_native_identity(temp, native_ref, exit);
        Ok(())
    }

    /// The method of the guarded method call at `byte_pc` into
    /// `X(destination)`: the receiver `X(receiver)` has the guarded shape,
    /// the prototype chain proof holds, and the method is the holder's slot.
    /// Eager deopt unless `X(value)` is the function `function_id`: its
    /// function-id immediate, or a closure of it that needs no runtime
    /// setup.
    fn emit_check_function(&mut self, node: NodeId, function_id: u32, cell: u64, value: u8) {
        let exit = self.eager_exit(node, DeoptReason::WrongValue);
        let proved = self.ops.new_dynamic_label();
        let layout = self.view.closure_call_layout;
        let cell_target = RelocationTarget::CalleeIdentityCell {
            function_id,
            call_pc: self.graph.node(node).pc,
        };
        if cell != 0 {
            let start = self.ops.offset().0;
            self.load_immediate(16, cell);
            self.relocations
                .record_mov_wide(start, self.ops.offset().0, 16, cell_target.clone());
            dynasm!(self.ops ; .arch aarch64 ; ldr x16, [x16] ; cmp X(value), x16 ; b.eq =>proved);
        }
        self.load_immediate(16, tag::box_function_id(function_id));
        dynasm!(self.ops ; .arch aarch64 ; cmp X(value), x16 ; b.eq =>proved);
        self.load_immediate(16, NOT_CELL_MASK);
        dynasm!(self.ops
            ; .arch aarch64
            ; tst X(value), x16
            ; b.ne =>exit
            ; cbz X(value), =>exit
            ; ldrb w16, [X(value)]
            ; cmp w16, otter_vm::closure::JS_CLOSURE_BODY_TYPE_TAG as u32
            ; b.ne =>exit
        );
        if layout.runtime_setup_flags != 0 {
            dynasm!(self.ops ; .arch aarch64 ; ldr w16, [X(value), layout.flags_byte]);
            self.load_immediate(17, u64::from(layout.runtime_setup_flags));
            dynasm!(self.ops ; .arch aarch64 ; tst w16, w17 ; b.ne =>exit);
        }
        dynasm!(self.ops ; .arch aarch64 ; ldr w16, [X(value), layout.function_id_byte]);
        self.load_immediate(17, u64::from(function_id));
        dynasm!(self.ops ; .arch aarch64 ; cmp w16, w17 ; b.ne =>exit);
        if cell != 0 {
            // The proved value is the one the next check compares first.
            let start = self.ops.offset().0;
            self.load_immediate(16, cell);
            self.relocations
                .record_mov_wide(start, self.ops.offset().0, 16, cell_target);
            dynasm!(self.ops ; .arch aarch64 ; str X(value), [x16]);
        }
        dynasm!(self.ops ; .arch aarch64 ; =>proved);
    }

    fn emit_load_guarded_method(
        &mut self,
        node: NodeId,
        (byte_pc, target): (u32, u8),
        receiver_proved: bool,
        receiver: u8,
        holder: u8,
        destination: u8,
    ) -> Result<(), Unsupported> {
        let guard = self
            .view_of(node)
            .direct_methods
            .get(&byte_pc)
            .and_then(|methods| methods.get(usize::from(target)))
            .map(|method| method.guard.clone())
            .ok_or(Unsupported::OperandShape("graph guarded method"))?;
        let exit = self.eager_exit(node, DeoptReason::WrongShape);
        let view = self.view;
        if !receiver_proved {
            self.emit_ordinary_receiver(receiver, 0, exit);
            dynasm!(self.ops
                ; .arch aarch64
                ; ldr w16, [X(receiver), view.object_shape_byte]
            );
            self.load_immediate(17, u64::from(guard.recv_shape));
            dynasm!(self.ops ; .arch aarch64 ; cmp w16, w17 ; b.ne =>exit);
        }
        if let Some(validity) = guard.prototype_validity {
            crate::template::arm64::values::emit_prototype_validity_guard(
                &mut self.ops,
                &mut self.relocations,
                validity,
                holder,
                exit,
            );
        }
        if guard.holder_root == 0 {
            dynasm!(self.ops ; .arch aarch64 ; mov X(holder), X(receiver));
        } else {
            self.load_immediate(17, u64::from(guard.holder_root));
            dynasm!(self.ops
                ; .arch aarch64
                ; and x16, X(receiver), #0xffff_ffff_0000_0000
                ; add x17, x16, x17
                ; ldr W(holder), [x17, view.shape_prototype_byte]
                ; add X(holder), x16, X(holder)
            );
        }
        let slab = view.object_slab_handle_byte;
        let words = view.object_slab_words_byte;
        let inline = view.object_inline_values_byte;
        let value_byte = guard.method_value_byte;
        dynasm!(self.ops
            ; .arch aarch64
            ; ldr w16, [X(holder), slab]
            ; and x17, X(holder), #0xffff_ffff_0000_0000
            ; orr x17, x17, x16
            ; add x17, x17, words
            ; cmp w16, #0
            ; add x16, XSP(holder), inline
            ; csel x16, x16, x17, eq
        );
        if value_byte <= 32760 && value_byte % 8 == 0 {
            dynasm!(self.ops ; .arch aarch64 ; ldr X(destination), [x16, value_byte]);
        } else {
            self.load_immediate(17, u64::from(value_byte));
            dynasm!(self.ops ; .arch aarch64 ; ldr X(destination), [x16, x17]);
        }
        Ok(())
    }

    /// The named store at `byte_pc` through its feedback programs. The
    /// receiver in `X(receiver)` must be an ordinary object that no chain
    /// proof subscribes to; each program then proves its shape and chain and
    /// writes `X(value)` into an existing slot, or appends the slot within
    /// the receiver's storage and publishes the child shape. No program
    /// matching leaves through the eager deopt before any effect.
    fn emit_store_named_property(
        &mut self,
        node: NodeId,
        byte_pc: u32,
        [receiver, value]: [u8; 2],
        [holder, scratch]: [u8; 2],
    ) -> Result<(), Unsupported> {
        use otter_vm::JitCacheIrOp as Op;
        let programs = self
            .view_of(node)
            .property_programs
            .get(&byte_pc)
            .ok_or(Unsupported::OperandShape("graph named store programs"))?
            .clone();
        let exit = self.eager_exit(node, DeoptReason::WrongShape);
        let done = self.ops.new_dynamic_label();
        let view = self.view;
        let ordinary_mask = u32::from(otter_vm::jit::JIT_OBJECT_ORDINARY_LOOKUP_MASK);
        let link_mask = u32::from(otter_vm::jit::JIT_OBJECT_SHAPE_STATE_MASK);
        let used_as_prototype = u32::from(otter_vm::jit::JIT_OBJECT_FLAG_USED_AS_PROTOTYPE);
        let extensible = u32::from(otter_vm::jit::JIT_OBJECT_FLAG_EXTENSIBLE);
        self.load_immediate(16, NOT_CELL_MASK);
        dynasm!(self.ops
            ; .arch aarch64
            ; tst X(receiver), x16
            ; b.ne =>exit
            ; ldrb w16, [X(receiver)]
            ; cmp w16, crate::entry::OBJECT_BODY_TYPE_TAG
            ; b.ne =>exit
            ; ldrb w16, [X(receiver), view.object_flags_byte]
            ; tst w16, ordinary_mask | used_as_prototype
            ; b.ne =>exit
        );
        for program in programs.iter() {
            let next = self.ops.new_dynamic_label();
            let transition = program
                .ops
                .iter()
                .any(|op| matches!(op, Op::PublishShape { .. }));
            let mut stored_byte = None;
            for op in program.ops.iter() {
                let object = |operand: u8| if operand == 0 { receiver } else { holder };
                match *op {
                    Op::LoadPrototypeHolder { root, .. } => {
                        self.load_immediate(17, u64::from(root));
                        dynasm!(self.ops
                            ; .arch aarch64
                            ; and x16, X(receiver), #0xffff_ffff_0000_0000
                            ; add x17, x16, x17
                            ; ldr W(holder), [x17, view.shape_prototype_byte]
                            ; add X(holder), x16, X(holder)
                        );
                    }
                    Op::GuardPrototypeValidity { validity } => {
                        crate::template::arm64::values::emit_prototype_validity_guard(
                            &mut self.ops,
                            &mut self.relocations,
                            validity,
                            scratch,
                            next,
                        );
                    }
                    Op::GuardShape {
                        object: operand,
                        shape,
                    } => {
                        let header = object(operand);
                        // A transition's chain link only has to keep its
                        // shape authoritative for the keys it owns.
                        let mask = if operand == 1 && transition {
                            link_mask
                        } else {
                            ordinary_mask
                        };
                        dynasm!(self.ops
                            ; .arch aarch64
                            ; ldrb w16, [X(header), view.object_flags_byte]
                            ; tst w16, mask
                            ; b.ne =>next
                            ; ldr w16, [X(header), view.object_shape_byte]
                            ; cbz w16, =>next
                        );
                        self.load_immediate(17, u64::from(shape));
                        dynasm!(self.ops ; .arch aarch64 ; cmp w16, w17 ; b.ne =>next);
                    }
                    Op::GuardPrototypeNull { object: operand } => {
                        let header = object(operand);
                        dynasm!(self.ops
                            ; .arch aarch64
                            ; and x17, X(header), #0xffff_ffff_0000_0000
                            ; ldr w16, [X(header), view.object_shape_byte]
                            ; add x16, x17, x16
                            ; ldr w16, [x16, view.shape_prototype_byte]
                            ; cbnz w16, =>next
                        );
                    }
                    Op::GuardAtomSlot { .. } => {
                        // The receiver's shape guard proves the immutable
                        // atom/slot mapping and the live override state.
                    }
                    Op::GuardExtensible { value_byte, .. } => {
                        // The appended slot fits the storage the receiver
                        // already has, and the receiver may still grow.
                        let slot = value_byte / 8;
                        let inline_storage = self.ops.new_dynamic_label();
                        let fits = self.ops.new_dynamic_label();
                        dynasm!(self.ops
                            ; .arch aarch64
                            ; ldr w16, [X(receiver), view.object_slab_handle_byte]
                            ; cbz w16, =>inline_storage
                            ; and x17, X(receiver), #0xffff_ffff_0000_0000
                            ; add x16, x17, x16
                            ; ldr w16, [x16, view.object_slab_capacity_byte]
                        );
                        self.load_immediate(17, u64::from(slot));
                        dynasm!(self.ops
                            ; .arch aarch64
                            ; cmp w17, w16
                            ; b.hs =>next
                            ; b =>fits
                            ; =>inline_storage
                            ; ldrb w16, [X(receiver), view.object_inline_capacity_byte]
                        );
                        self.load_immediate(17, u64::from(slot));
                        dynasm!(self.ops
                            ; .arch aarch64
                            ; cmp w17, w16
                            ; b.hs =>next
                            ; =>fits
                            ; ldrb w16, [X(receiver), view.object_flags_byte]
                            ; tst w16, extensible
                            ; b.eq =>next
                        );
                    }
                    Op::StoreField { value_byte, .. } => {
                        // Every guard has passed: the slot base, then the
                        // value word.
                        let slab = view.object_slab_handle_byte;
                        let words = view.object_slab_words_byte;
                        let inline = view.object_inline_values_byte;
                        dynasm!(self.ops
                            ; .arch aarch64
                            ; ldr w16, [X(receiver), slab]
                            ; and x17, X(receiver), #0xffff_ffff_0000_0000
                            ; orr x17, x17, x16
                            ; add x17, x17, words
                            ; cmp w16, #0
                            ; add x16, XSP(receiver), inline
                            ; csel X(scratch), x16, x17, eq
                        );
                        if value_byte <= 32760 {
                            dynasm!(self.ops ; .arch aarch64 ; str X(value), [X(scratch), value_byte]);
                        } else {
                            self.load_immediate(17, u64::from(value_byte));
                            dynasm!(self.ops ; .arch aarch64 ; str X(value), [X(scratch), x17]);
                        }
                        stored_byte = Some(value_byte);
                    }
                    Op::PublishShape { shape, .. } => {
                        self.load_immediate(16, u64::from(shape));
                        dynasm!(self.ops
                            ; .arch aarch64
                            ; str w16, [X(receiver), view.object_shape_byte]
                        );
                        self.emit_shape_child_barrier(node, receiver, shape, scratch);
                    }
                    _ => return Err(Unsupported::OperandShape("graph named store operation")),
                }
            }
            if stored_byte.is_none() {
                return Err(Unsupported::OperandShape(
                    "graph named store without a store",
                ));
            }
            dynasm!(self.ops ; .arch aarch64 ; b =>done ; =>next);
        }
        dynasm!(self.ops ; .arch aarch64 ; b =>exit ; =>done);
        Ok(())
    }

    /// Leave `X(shape_address)` the decompressed hidden class of the
    /// ordinary object in `X(receiver)` and `X(shape_id)` its table identity,
    /// branching to `miss` for anything else or for a dictionary shape, which
    /// no shared table entry names. `X(receiver)` must already be a proved
    /// ordinary object.
    fn emit_shape_identity(
        &mut self,
        receiver: u8,
        [shape_address, shape_id]: [u8; 2],
        shape_id_byte: u32,
        miss: DynamicLabel,
    ) {
        let view = self.view;
        dynasm!(self.ops
            ; .arch aarch64
            ; and x16, X(receiver), #0xffff_ffff_0000_0000
            ; ldr W(shape_address), [X(receiver), view.object_shape_byte]
            ; add X(shape_address), x16, X(shape_address)
            ; ldrb w17, [X(shape_address), view.shape_kind_byte]
            ; tst w17, 1u32 << crate::template::arm64::values::SHAPE_KIND_DICTIONARY_BIT
            ; b.ne =>miss
            ; ldr X(shape_id), [X(shape_address), shape_id_byte]
        );
    }

    /// Branch to `miss` unless `X(receiver)` is an ordinary object whose
    /// named lookup its shape fully describes.
    fn emit_ordinary_receiver(&mut self, receiver: u8, extra_flags: u32, miss: DynamicLabel) {
        let view = self.view;
        let mask = u32::from(otter_vm::jit::JIT_OBJECT_ORDINARY_LOOKUP_MASK) | extra_flags;
        self.load_immediate(16, NOT_CELL_MASK);
        dynasm!(self.ops
            ; .arch aarch64
            ; tst X(receiver), x16
            ; b.ne =>miss
            ; ldrb w16, [X(receiver)]
            ; cmp w16, crate::entry::OBJECT_BODY_TYPE_TAG
            ; b.ne =>miss
            ; ldrb w16, [X(receiver), view.object_flags_byte]
            ; tst w16, mask
            ; b.ne =>miss
        );
    }

    /// `X(entry)` = the address of the shared-table bucket for the shape
    /// identity in `X(shape_id)` and `atom`: `((id * Ms) ^ (atom * Ma)) >>
    /// shift & mask`, scaled by `bucket_bytes`.
    #[allow(clippy::too_many_arguments)]
    fn emit_table_bucket(
        &mut self,
        shape_id: u8,
        atom: u32,
        [shape_multiplier, atom_multiplier]: [u64; 2],
        hash_shift: u8,
        index_mask: u32,
        bucket_bytes: u64,
        table: (usize, RelocationTarget),
        entry: u8,
    ) {
        self.load_immediate(17, shape_multiplier);
        dynasm!(self.ops ; .arch aarch64 ; mul X(entry), X(shape_id), x17);
        self.load_immediate(17, u64::from(atom).wrapping_mul(atom_multiplier));
        dynasm!(self.ops
            ; .arch aarch64
            ; eor X(entry), X(entry), x17
            ; lsr X(entry), X(entry), u32::from(hash_shift)
        );
        self.load_immediate(17, u64::from(index_mask));
        dynasm!(self.ops ; .arch aarch64 ; and X(entry), X(entry), x17);
        self.load_immediate(17, bucket_bytes);
        dynasm!(self.ops ; .arch aarch64 ; mul X(entry), X(entry), x17);
        emit_load_symbol_u64(
            &mut self.ops,
            &mut self.relocations,
            17,
            table.0 as u64,
            table.1,
        );
        dynasm!(self.ops ; .arch aarch64 ; add X(entry), X(entry), x17);
    }

    /// `X(base)` = the slot storage of the ordinary object in `X(holder)`
    /// when slot `X(slot)` lies inside it; `miss` otherwise.
    fn emit_slot_storage(&mut self, holder: u8, slot: u8, base: u8, miss: DynamicLabel) {
        let view = self.view;
        let inline = self.ops.new_dynamic_label();
        let ready = self.ops.new_dynamic_label();
        dynasm!(self.ops
            ; .arch aarch64
            ; ldr W(base), [X(holder), view.object_slab_handle_byte]
            ; cbz W(base), =>inline
            ; and x16, X(holder), #0xffff_ffff_0000_0000
            ; add X(base), x16, X(base)
            ; ldr w16, [X(base), view.object_slab_capacity_byte]
            ; cmp W(slot), w16
            ; b.hs =>miss
            ; add XSP(base), XSP(base), view.object_slab_words_byte
            ; b =>ready
            ; =>inline
            ; ldrb w16, [X(holder), view.object_inline_capacity_byte]
            ; cmp W(slot), w16
            ; b.hs =>miss
            ; add XSP(base), XSP(holder), view.object_inline_values_byte
            ; =>ready
        );
    }

    /// `[[Get]]` of the load site at `pc` from `X(receiver)` into
    /// `X(destination)`: an own or chain-proved inherited data slot the
    /// isolate's shared lookup table names for `atom`, else the runtime.
    fn emit_load_property_cached(
        &mut self,
        node: NodeId,
        pc: u32,
        atom: Option<u32>,
        receiver: u8,
        [shape_address, shape_id, entry, base]: [u8; 4],
        destination: u8,
    ) -> Result<(), Unsupported> {
        let slow = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        let cache = self.view.property_lookup_cache.filter(|cache| {
            cache.table_addr != 0 && cache.entry_bytes != 0 && cache.hash_shift < 64
        });
        if let (Some(atom), Some(cache)) = (atom, cache) {
            let holder_ready = self.ops.new_dynamic_label();
            self.emit_ordinary_receiver(receiver, 0, slow);
            self.emit_shape_identity(
                receiver,
                [shape_address, shape_id],
                cache.shape_id_byte,
                slow,
            );
            self.emit_table_bucket(
                shape_id,
                atom,
                [cache.hash_shape_multiplier, cache.hash_atom_multiplier],
                cache.hash_shift,
                cache.index_mask,
                u64::from(cache.entry_bytes),
                (cache.table_addr, RelocationTarget::PropertyLookupCacheTable),
                entry,
            );
            dynasm!(self.ops
                ; .arch aarch64
                ; ldr x16, [X(entry), cache.receiver_shape_id_byte]
                ; cmp x16, X(shape_id)
                ; b.ne =>slow
                ; ldr w16, [X(entry), cache.atom_byte]
            );
            self.load_immediate(17, u64::from(atom));
            let view = self.view;
            dynasm!(self.ops
                ; .arch aarch64
                ; cmp w16, w17
                ; b.ne =>slow
                ; ldrb w16, [X(entry), cache.is_data_byte]
                ; cmp w16, #1
                ; b.ne =>slow
                ; ldrb w16, [X(entry), cache.hops_byte]
                ; cmp w16, #1
                ; b.hi =>slow
                ; mov X(shape_address), X(receiver)
                ; cbz w16, =>holder_ready
                // One hop: the chain proof, then the holder its pinned
                // instance-root shape names.
                ; ldr x16, [X(entry), cache.validity_byte]
                ; cbz x16, =>slow
                ; ldar w16, [x16]
                ; cbz w16, =>slow
                ; and x17, X(receiver), #0xffff_ffff_0000_0000
                ; ldr w16, [X(entry), cache.holder_root_byte]
                ; add x16, x17, x16
                ; ldr W(shape_address), [x16, view.shape_prototype_byte]
                ; add X(shape_address), x17, X(shape_address)
                ; =>holder_ready
                ; ldrh W(shape_id), [X(entry), cache.slot_byte]
            );
            self.emit_slot_storage(shape_address, shape_id, base, slow);
            dynasm!(self.ops
                ; .arch aarch64
                ; ldr X(destination), [X(base), X(shape_id), lsl #3]
                ; b =>done
            );
        } else {
            dynasm!(self.ops ; .arch aarch64 ; b =>slow);
        }
        dynasm!(self.ops ; .arch aarch64 ; =>slow);
        self.emit_property_runtime(
            node,
            pc,
            crate::artifact::relocation::PropertySourceAccess::Load,
            [receiver, receiver],
            Some(destination),
        )?;
        dynasm!(self.ops ; .arch aarch64 ; =>done);
        Ok(())
    }

    /// `[[Set]]` of `X(value)` at the store site at `pc` on `X(receiver)`:
    /// an existing writable own slot the shared lookup table names for
    /// `atom`, or an add transition the shared transition table names, else
    /// the runtime.
    fn emit_store_property_cached(
        &mut self,
        node: NodeId,
        pc: u32,
        atom: Option<u32>,
        [receiver, value]: [u8; 2],
        [shape_address, shape_id, entry, base]: [u8; 4],
    ) -> Result<(), Unsupported> {
        let slow = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        let used_as_prototype = u32::from(otter_vm::jit::JIT_OBJECT_FLAG_USED_AS_PROTOTYPE);
        let lookup = self.view.property_lookup_cache.filter(|cache| {
            cache.table_addr != 0 && cache.entry_bytes != 0 && cache.hash_shift < 64
        });
        let transitions = self.view.store_transition_cache.filter(|cache| {
            cache.table_addr != 0
                && cache.entry_bytes != 0
                && cache.entry_bytes < 4096
                && cache.ways != 0
                && cache.ways <= u32::from(u16::MAX)
                && cache.hash_shift < 64
        });
        let view = self.view;
        if let (Some(atom), Some(cache)) = (atom, lookup) {
            let transition = self.ops.new_dynamic_label();
            self.emit_ordinary_receiver(receiver, used_as_prototype, slow);
            self.emit_shape_identity(
                receiver,
                [shape_address, shape_id],
                cache.shape_id_byte,
                slow,
            );
            self.emit_table_bucket(
                shape_id,
                atom,
                [cache.hash_shape_multiplier, cache.hash_atom_multiplier],
                cache.hash_shift,
                cache.index_mask,
                u64::from(cache.entry_bytes),
                (cache.table_addr, RelocationTarget::PropertyLookupCacheTable),
                entry,
            );
            dynasm!(self.ops
                ; .arch aarch64
                ; ldr x16, [X(entry), cache.receiver_shape_id_byte]
                ; cmp x16, X(shape_id)
                ; b.ne =>transition
                ; ldr w16, [X(entry), cache.atom_byte]
            );
            self.load_immediate(17, u64::from(atom));
            dynasm!(self.ops
                ; .arch aarch64
                ; cmp w16, w17
                ; b.ne =>transition
                ; ldrb w16, [X(entry), cache.hops_byte]
                ; cbnz w16, =>transition
                ; ldrb w16, [X(entry), cache.is_data_byte]
                ; cmp w16, #1
                ; b.ne =>transition
                ; ldrb w16, [X(entry), cache.is_writable_byte]
                ; cmp w16, #1
                ; b.ne =>transition
                // The entry's holder shape is the receiver's own.
                ; ldr w16, [X(entry), cache.holder_shape_byte]
                ; cbz w16, =>transition
                ; ldr w17, [X(receiver), view.object_shape_byte]
                ; cmp w16, w17
                ; b.ne =>transition
                ; ldrh W(shape_id), [X(entry), cache.slot_byte]
            );
            self.emit_slot_storage(receiver, shape_id, base, transition);
            dynasm!(self.ops
                ; .arch aarch64
                ; str X(value), [X(base), X(shape_id), lsl #3]
                ; b =>done
                ; =>transition
            );
            if let Some(cache) = transitions {
                // The receiver is a proved ordinary non-prototype object.
                let way = self.ops.new_dynamic_label();
                let next_way = self.ops.new_dynamic_label();
                let found = self.ops.new_dynamic_label();
                let chain_done = self.ops.new_dynamic_label();
                self.emit_shape_identity(
                    receiver,
                    [shape_address, shape_id],
                    cache.shape_id_byte,
                    slow,
                );
                self.emit_table_bucket(
                    shape_id,
                    atom,
                    [cache.hash_shape_multiplier, cache.hash_atom_multiplier],
                    cache.hash_shift,
                    cache.index_mask,
                    u64::from(cache.entry_bytes) * u64::from(cache.ways),
                    (
                        cache.table_addr,
                        RelocationTarget::StoreTransitionCacheTable,
                    ),
                    entry,
                );
                // Probe the set's ways in recording order; `X(entry)` ends on
                // the match.
                self.load_immediate(17, u64::from(atom));
                dynasm!(self.ops
                    ; .arch aarch64
                    ; movz W(base), cache.ways
                    ; =>way
                    ; ldr x16, [X(entry), cache.receiver_shape_byte]
                    ; cmp x16, X(shape_id)
                    ; b.ne =>next_way
                    ; ldr w16, [X(entry), cache.atom_byte]
                    ; cmp w16, w17
                    ; b.eq =>found
                    ; =>next_way
                    ; add XSP(entry), XSP(entry), cache.entry_bytes
                    ; subs W(base), WSP(base), #1
                    ; b.ne =>way
                    ; b =>slow
                    ; =>found
                    ; ldr w16, [X(entry), cache.target_shape_byte]
                    ; cbz w16, =>slow
                    ; ldr x16, [X(entry), cache.validity_byte]
                    ; cbz x16, =>chain_done
                    ; ldar w16, [x16]
                    ; cbz w16, =>slow
                    ; =>chain_done
                    ; ldrb w16, [X(receiver), view.object_flags_byte]
                    ; tst w16, u32::from(otter_vm::jit::JIT_OBJECT_FLAG_EXTENSIBLE)
                    ; b.eq =>slow
                    // The matched receiver shape has exactly `slot` slots:
                    // the append index is its property count.
                    ; ldrh W(shape_id), [X(entry), cache.slot_byte]
                );
                self.emit_slot_storage(receiver, shape_id, base, slow);
                dynasm!(self.ops
                    ; .arch aarch64
                    ; str X(value), [X(base), X(shape_id), lsl #3]
                    ; ldr W(shape_id), [X(entry), cache.target_shape_byte]
                    ; str W(shape_id), [X(receiver), view.object_shape_byte]
                );
                self.emit_dynamic_shape_child_barrier(node, receiver, shape_id, shape_address);
                dynasm!(self.ops ; .arch aarch64 ; b =>done);
            } else {
                dynasm!(self.ops ; .arch aarch64 ; b =>slow);
            }
        } else {
            dynasm!(self.ops ; .arch aarch64 ; b =>slow);
        }
        dynasm!(self.ops ; .arch aarch64 ; =>slow);
        self.emit_property_runtime(
            node,
            pc,
            crate::artifact::relocation::PropertySourceAccess::Store,
            [receiver, value],
            None,
        )?;
        dynasm!(self.ops ; .arch aarch64 ; =>done);
        Ok(())
    }

    /// The full property operation at `pc` in the runtime, with every live
    /// register saved in a rooted snapshot slot around the call: the load
    /// stub from `X(receiver)` into `X(destination)`, or the store stub of
    /// `X(value)`. A throw leaves through the node's throw routing.
    fn emit_property_runtime(
        &mut self,
        node: NodeId,
        pc: u32,
        access: crate::artifact::relocation::PropertySourceAccess,
        [receiver, value]: [u8; 2],
        destination: Option<u8>,
    ) -> Result<(), Unsupported> {
        use crate::artifact::relocation::PropertySourceAccess;
        let function_id = self.view_of(node).code_block.id;
        let (cell_address, ordinal, stub) = match access {
            PropertySourceAccess::Load => {
                let ordinal = self.next_load_ic;
                let cell = self
                    .load_ic_cells
                    .get_mut(ordinal)
                    .ok_or(Unsupported::OperandShape("graph load property cell"))?;
                cell.set_source(function_id, pc);
                self.next_load_ic += 1;
                (
                    cell as *mut crate::entry::PropertySourceCell as u64,
                    ordinal,
                    abi::STUB_JIT_LOAD_PROPERTY,
                )
            }
            PropertySourceAccess::Store => {
                let ordinal = self.next_store_ic;
                let cell = self
                    .store_ic_cells
                    .get_mut(ordinal)
                    .ok_or(Unsupported::OperandShape("graph store property cell"))?;
                cell.set_source(function_id, pc);
                self.next_store_ic += 1;
                (
                    cell as *mut crate::entry::PropertySourceCell as u64,
                    ordinal,
                    abi::STUB_JIT_STORE_PROPERTY,
                )
            }
        };
        let values: &[u8] = match access {
            PropertySourceAccess::Load => &[receiver],
            PropertySourceAccess::Store => &[receiver, value],
        };
        self.emit_committed_call(
            node,
            stub,
            values,
            Some((
                cell_address,
                RelocationTarget::PropertySourceCell {
                    access,
                    ordinal: ordinal as u32,
                },
            )),
            destination,
        );
        Ok(())
    }

    /// Call the committed runtime entry `stub` with the context, the values
    /// in `values` and then `immediate`, with every live register saved in a
    /// rooted snapshot slot around the call and the node's own safepoint and
    /// position published. The result goes to `X(destination)`; a thrown
    /// exception leaves through the node's throw routing, any other abrupt
    /// completion is an engine failure.
    fn emit_committed_call(
        &mut self,
        node: NodeId,
        stub: abi::RuntimeStubDescriptor,
        values: &[u8],
        immediate: Option<(u64, RelocationTarget)>,
        destination: Option<u8>,
    ) {
        debug_assert!(values.len() <= 2, "two values pass through x16/x17");
        let live = self.allocation.node(node).live_registers.clone();
        let mut roots = self
            .allocation
            .node(node)
            .gc_roots
            .clone()
            .expect("a collecting node has its roots");
        for &(location, repr) in &live {
            let slot = self.slots.snapshot_slot(location, repr);
            self.emit_move(location, slot);
            if let Location::TaggedSlot(index) = slot {
                roots.push(index);
            }
        }
        roots.sort_unstable();
        roots.dedup();
        self.stamp_safepoint(node, roots);
        let outer_pc = self.graph.outer_pc(node);
        self.load_immediate(16, u64::from(outer_pc));
        dynasm!(self.ops ; .arch aarch64 ; str w16, [x21, crate::entry::NATIVE_FRAME_PC_OFFSET]);
        // The values may sit in the argument registers they move between.
        for (index, &value) in values.iter().enumerate() {
            dynasm!(self.ops ; .arch aarch64 ; mov X(16 + index as u8), X(value));
        }
        dynasm!(self.ops ; .arch aarch64 ; mov x0, x20);
        for index in 0..values.len() {
            dynasm!(self.ops ; .arch aarch64 ; mov X(1 + index as u8), X(16 + index as u8));
        }
        if let Some((bits, target)) = immediate {
            let register = 1 + values.len() as u8;
            let start = self.ops.offset().0;
            self.load_immediate(register, bits);
            self.relocations
                .record_mov_wide(start, self.ops.offset().0, register, target);
        }
        emit_load_symbol_u64(
            &mut self.ops,
            &mut self.relocations,
            16,
            self.transitions.entry(stub),
            RelocationTarget::runtime_stub(stub),
        );
        // An abrupt completion leaves while the live values are still in
        // their snapshot slots, which its frame rebuild reads them from.
        let snapshot_locations: Box<[Location]> = {
            let state_values = self
                .graph
                .node(node)
                .eager
                .map(|state| self.graph.state_values(state))
                .unwrap_or_default();
            self.allocation
                .node(node)
                .eager
                .iter()
                .zip(state_values)
                .map(|(&location, value)| match location {
                    Location::Gp(_) | Location::Fp(_) => self
                        .slots
                        .snapshot_slot(location, self.graph.node(value).repr),
                    other => other,
                })
                .collect()
        };
        let (_, committed_throw) = self.throw_targets_at(node, Some(snapshot_locations));
        let committed_throw = self.cond_target(committed_throw);
        let fatal = self.cond_target(self.fatal);
        let error = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        dynasm!(self.ops ; .arch aarch64 ; blr x16 ; cbnz x1, =>error);
        // The result waits on the stack while the live registers come back
        // from their snapshot slots.
        dynasm!(self.ops ; .arch aarch64 ; str x0, [sp, #-16]!);
        self.sp_delta += 16;
        for &(location, repr) in &live {
            let slot = self.slots.snapshot_slot(location, repr);
            self.emit_move(slot, location);
        }
        match destination {
            Some(destination) => {
                dynasm!(self.ops ; .arch aarch64 ; ldr X(destination), [sp], #16);
            }
            None => dynasm!(self.ops ; .arch aarch64 ; add sp, sp, #16),
        }
        self.sp_delta -= 16;
        dynasm!(self.ops
            ; .arch aarch64
            ; b =>done
            ; =>error
            ; cmp x1, NativeResultStatus::Throw as u32
            ; b.ne =>fatal
            ; b =>committed_throw
            ; =>done
        );
    }

    /// `X(destination)` = `X(value) instanceof X(target)`. A target that is
    /// an ordinary closure without symbol-keyed own properties uses the
    /// default `@@hasInstance`, OrdinaryHasInstance: its `prototype` (an
    /// ordinary object) is searched on the value's prototype chain, a
    /// primitive answers false. Every other case, an opaque chain link and a
    /// chain longer than the walk bound complete in the runtime.
    fn emit_instanceof(
        &mut self,
        node: NodeId,
        [value, target]: [u8; 2],
        [cage, rare, prototype, cursor, budget]: [u8; 5],
        destination: u8,
    ) {
        use crate::template::arm64::values::{CellTest, emit_cell_test};
        let view = self.view;
        let layout = view.closure_call_layout;
        let yes = self.ops.new_dynamic_label();
        let no = self.ops.new_dynamic_label();
        let miss = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        let walk = self.ops.new_dynamic_label();
        let step = self.ops.new_dynamic_label();
        let symbols_absent = self.ops.new_dynamic_label();
        if view.cage_base == 0 || layout.prototype_byte == 0 {
            dynasm!(self.ops ; .arch aarch64 ; b =>miss);
        } else {
            let named_lookup = otter_vm::closure::CLOSURE_NAMED_LOOKUP_BYTE;
            emit_load_symbol_u64(
                &mut self.ops,
                &mut self.relocations,
                cage,
                view.cage_base as u64,
                RelocationTarget::GcCageBase,
            );
            // Target: an ordinary closure whose own properties hold no
            // `@@hasInstance` override.
            emit_cell_test(&mut self.ops, target, 16, CellTest::IsNotCell, miss);
            dynasm!(self.ops
                ; .arch aarch64
                ; cbz X(target), =>miss
                ; mov w16, W(target)
                ; add X(rare), X(cage), x16
                ; ldrb w16, [X(rare)]
                ; cmp w16, u32::from(otter_vm::closure::JS_CLOSURE_BODY_TYPE_TAG)
                ; b.ne =>miss
                ; ldrb w16, [X(rare), named_lookup]
                ; and w16, w16, !u32::from(otter_vm::closure::CLOSURE_LOOKUP_OWN_PROPS)
                ; cmp w16, u32::from(otter_vm::closure::CLOSURE_LOOKUP_ORDINARY)
                ; b.ne =>miss
                // `prototype` lives in the rare record; without one, or
                // while it holds the hole, the runtime allocates it.
                ; ldr W(rare), [X(rare), layout.rare_byte]
                ; cbz W(rare), =>miss
                ; add X(rare), X(cage), X(rare)
                ; ldr w16, [X(rare), layout.own_props_byte]
                ; cbz w16, =>symbols_absent
                ; add x16, X(cage), x16
                ; ldr w16, [x16, view.object_exotic_handle_byte]
                ; cbz w16, =>symbols_absent
                ; add x16, X(cage), x16
                ; ldr w16, [x16, otter_vm::object::EXOTIC_SLOTS_SYMBOL_PROPS_BYTE]
                ; cbnz w16, =>miss
                ; =>symbols_absent
                ; ldr X(prototype), [X(rare), layout.prototype_byte]
            );
            // The prototype must be an ordinary object; anything else throws.
            emit_cell_test(&mut self.ops, prototype, 16, CellTest::IsNotCell, miss);
            dynasm!(self.ops
                ; .arch aarch64
                ; cbz X(prototype), =>miss
                ; mov w16, W(prototype)
                ; add X(prototype), X(cage), x16
                ; ldrb w16, [X(prototype)]
                ; cmp w16, crate::entry::OBJECT_BODY_TYPE_TAG
                ; b.ne =>miss
            );
            // Value: a non-cell answers false, a primitive cell answers
            // false, an ordinary object is walked, any other cell misses.
            emit_cell_test(&mut self.ops, value, 16, CellTest::IsNotCell, no);
            dynasm!(self.ops
                ; .arch aarch64
                ; cbz X(value), =>miss
                ; mov w16, W(value)
                ; add X(cursor), X(cage), x16
                ; ldrb w16, [X(cursor)]
                ; cmp w16, crate::entry::OBJECT_BODY_TYPE_TAG
                ; b.eq =>walk
            );
            for primitive_tag in view.primitive_cell_type_tags {
                dynasm!(self.ops ; .arch aarch64 ; cmp w16, u32::from(primitive_tag) ; b.eq =>no);
            }
            let opaque = otter_vm::jit::JIT_OBJECT_FLAG_CHAIN_LINK_OPAQUE.trailing_zeros();
            dynasm!(self.ops
                ; .arch aarch64
                ; b =>miss
                ; =>walk
                ; movz W(budget), INSTANCEOF_CHAIN_BOUND
                ; =>step
                ; ldrb w16, [X(cursor), view.object_flags_byte]
                ; tst w16, 1u32 << opaque
                ; b.ne =>miss
            );
            crate::template::arm64::values::emit_load_prototype(
                &mut self.ops,
                view,
                16,
                cursor,
                cage,
            );
            dynasm!(self.ops
                ; .arch aarch64
                ; cbz w16, =>no
                ; add X(cursor), X(cage), x16
                ; cmp X(cursor), X(prototype)
                ; b.eq =>yes
                ; ldrb w16, [X(cursor)]
                ; cmp w16, crate::entry::OBJECT_BODY_TYPE_TAG
                ; b.ne =>miss
                ; subs W(budget), WSP(budget), #1
                ; b.ne =>step
                ; b =>miss
            );
        }
        dynasm!(self.ops ; .arch aarch64 ; =>yes);
        self.load_immediate(destination, otter_vm::Value::boolean(true).to_bits());
        dynasm!(self.ops ; .arch aarch64 ; b =>done ; =>no);
        self.load_immediate(destination, otter_vm::Value::boolean(false).to_bits());
        dynasm!(self.ops ; .arch aarch64 ; b =>done ; =>miss);
        self.emit_committed_call(
            node,
            abi::STUB_JIT_OBJECT_PROTOCOL_VALUE,
            &[value, target],
            None,
            Some(destination),
        );
        dynasm!(self.ops ; .arch aarch64 ; =>done);
    }

    /// [`Self::emit_shape_child_barrier`] for a child shape whose compressed
    /// handle is in `W(shape)`; `X(child)` is clobbered.
    fn emit_dynamic_shape_child_barrier(
        &mut self,
        node: NodeId,
        receiver: u8,
        shape: u8,
        child: u8,
    ) {
        let flags_byte = self.view.gc_barrier.header_flags_byte;
        let young = self.view.gc_barrier.young_flag;
        let settled = young | self.view.gc_barrier.remembered_flag;
        let slow = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        dynasm!(self.ops
            ; .arch aarch64
            ; and XSP(child), X(receiver), #0xffff_ffff_0000_0000
            ; add X(child), X(child), X(shape)
            ; ldr x16, [x20, THREAD_OFFSET]
            ; ldr x16, [x16, VM_THREAD_MARKING_FLAG_CELL_OFFSET]
            ; ldrb w16, [x16]
            ; cbnz w16, =>slow
            ; ldrb w16, [X(receiver), flags_byte]
            ; movz w17, u32::from(settled)
            ; tst w16, w17
            ; b.ne =>done
            ; ldrb w16, [X(child), flags_byte]
            ; movz w17, u32::from(young)
            ; tst w16, w17
            ; b.ne =>slow
            ; =>done
        );
        let live = self.allocation.node(node).live_registers.clone();
        self.deferred
            .push(Box::new(move |codegen: &mut Codegen<'a>| {
                dynasm!(codegen.ops ; .arch aarch64 ; =>slow);
                let saved = codegen.emit_save_registers(&live);
                dynasm!(codegen.ops
                    ; .arch aarch64
                    ; mov x16, X(receiver)
                    ; mov x17, X(child)
                    ; ldr x0, [x20, THREAD_OFFSET]
                    ; ldr x0, [x0, VM_THREAD_GC_HEAP_OFFSET]
                    ; mov x1, x16
                    ; mov x2, x17
                );
                emit_load_symbol_u64(
                    &mut codegen.ops,
                    &mut codegen.relocations,
                    16,
                    otter_vm::runtime_stubs::WRITE_BARRIER_MUTATING.entry_addr() as u64,
                    RelocationTarget::runtime_stub(abi::STUB_WRITE_BARRIER),
                );
                dynasm!(codegen.ops ; .arch aarch64 ; blr x16);
                codegen.emit_restore_registers(&live, saved);
                dynasm!(codegen.ops ; .arch aarch64 ; b =>done);
            }));
    }

    /// The generational and marking barrier for the edge from the receiver
    /// to the child shape a transition just published.
    fn emit_shape_child_barrier(&mut self, node: NodeId, receiver: u8, shape: u32, child: u8) {
        let flags_byte = self.view.gc_barrier.header_flags_byte;
        let young = self.view.gc_barrier.young_flag;
        let settled = young | self.view.gc_barrier.remembered_flag;
        let slow = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        self.load_immediate(17, u64::from(shape));
        dynasm!(self.ops
            ; .arch aarch64
            ; and XSP(child), X(receiver), #0xffff_ffff_0000_0000
            ; add X(child), X(child), x17
            ; ldr x16, [x20, THREAD_OFFSET]
            ; ldr x16, [x16, VM_THREAD_MARKING_FLAG_CELL_OFFSET]
            ; ldrb w16, [x16]
            ; cbnz w16, =>slow
            ; ldrb w16, [X(receiver), flags_byte]
            ; movz w17, u32::from(settled)
            ; tst w16, w17
            ; b.ne =>done
            ; ldrb w16, [X(child), flags_byte]
            ; movz w17, u32::from(young)
            ; tst w16, w17
            ; b.ne =>slow
            ; =>done
        );
        let live = self.allocation.node(node).live_registers.clone();
        self.deferred
            .push(Box::new(move |codegen: &mut Codegen<'a>| {
                dynasm!(codegen.ops ; .arch aarch64 ; =>slow);
                let saved = codegen.emit_save_registers(&live);
                dynasm!(codegen.ops
                    ; .arch aarch64
                    ; mov x16, X(receiver)
                    ; mov x17, X(child)
                    ; ldr x0, [x20, THREAD_OFFSET]
                    ; ldr x0, [x0, VM_THREAD_GC_HEAP_OFFSET]
                    ; mov x1, x16
                    ; mov x2, x17
                );
                emit_load_symbol_u64(
                    &mut codegen.ops,
                    &mut codegen.relocations,
                    16,
                    otter_vm::runtime_stubs::WRITE_BARRIER_MUTATING.entry_addr() as u64,
                    RelocationTarget::runtime_stub(abi::STUB_WRITE_BARRIER),
                );
                dynasm!(codegen.ops ; .arch aarch64 ; blr x16);
                codegen.emit_restore_registers(&live, saved);
                dynasm!(codegen.ops ; .arch aarch64 ; b =>done);
            }));
    }

    /// The named load at `byte_pc` through its feedback programs: each
    /// proves the receiver in `X(receiver)` and reads one data slot of it or
    /// of its holder; the first program that matches delivers the value to
    /// `X(destination)`, and none matching leaves through the eager deopt.
    fn emit_load_named_property(
        &mut self,
        node: NodeId,
        byte_pc: u32,
        receiver: u8,
        [holder, scratch]: [u8; 2],
        destination: u8,
    ) -> Result<(), Unsupported> {
        use otter_vm::JitCacheIrOp as Op;
        let programs = self
            .view_of(node)
            .property_programs
            .get(&byte_pc)
            .ok_or(Unsupported::OperandShape("graph named load programs"))?
            .clone();
        let exit = self.eager_exit(node, DeoptReason::WrongShape);
        let done = self.ops.new_dynamic_label();
        let ordinary_mask = u32::from(otter_vm::jit::JIT_OBJECT_ORDINARY_LOOKUP_MASK);
        for program in programs.iter() {
            let next = self.ops.new_dynamic_label();
            for op in program.ops.iter() {
                let object = |operand: u8| if operand == 0 { receiver } else { holder };
                match *op {
                    Op::LoadIntrinsicPrototype { target, .. } => {
                        self.load_immediate(16, NOT_CELL_MASK);
                        dynasm!(self.ops
                            ; .arch aarch64
                            ; tst X(receiver), x16
                            ; b.ne =>next
                            ; cbz X(receiver), =>next
                            ; ldrb w16, [X(receiver)]
                            ; cmp w16, u32::from(target.type_tag)
                            ; b.ne =>next
                        );
                        if let Some(guard) = target.guard {
                            self.emit_body_guard(receiver, guard, next);
                        }
                        if let Some(realm) = target.active_realm {
                            dynasm!(self.ops
                                ; .arch aarch64
                                ; ldr x16, [x20, THREAD_OFFSET]
                                ; ldr x16, [x16, crate::entry::VM_THREAD_ACTIVE_REALM_CELL_OFFSET]
                                ; cbz x16, =>next
                                ; ldr w16, [x16]
                            );
                            self.load_immediate(17, u64::from(realm));
                            dynasm!(self.ops ; .arch aarch64 ; cmp w16, w17 ; b.ne =>next);
                        }
                        // The pinned prototype: cage base plus its offset.
                        emit_load_symbol_u64(
                            &mut self.ops,
                            &mut self.relocations,
                            holder,
                            u64::from(target.proto_offset),
                            RelocationTarget::GuardedHeapReference {
                                component:
                                    crate::artifact::relocation::GuardedHeapComponent::Prototype,
                                byte_pc,
                                runtime_stub_id: abi::STUB_JIT_LOAD_PROPERTY.id,
                            },
                        );
                        dynasm!(self.ops
                            ; .arch aarch64
                            ; and x16, X(receiver), #0xffff_ffff_0000_0000
                            ; add X(holder), X(holder), x16
                        );
                    }
                    Op::LoadPrototypeHolder { root, .. } => {
                        // The holder the pinned instance-root shape names.
                        self.load_immediate(17, u64::from(root));
                        let prototype = self.view.shape_prototype_byte;
                        dynasm!(self.ops
                            ; .arch aarch64
                            ; and x16, X(receiver), #0xffff_ffff_0000_0000
                            ; add x17, x16, x17
                            ; ldr W(holder), [x17, prototype]
                            ; add X(holder), x16, X(holder)
                        );
                    }
                    Op::GuardPrototypeValidity { validity } => {
                        crate::template::arm64::values::emit_prototype_validity_guard(
                            &mut self.ops,
                            &mut self.relocations,
                            validity,
                            scratch,
                            next,
                        );
                    }
                    Op::GuardShape {
                        object: operand,
                        shape,
                    } => {
                        let header = object(operand);
                        if operand == 0 {
                            self.load_immediate(16, NOT_CELL_MASK);
                            dynasm!(self.ops
                                ; .arch aarch64
                                ; tst X(header), x16
                                ; b.ne =>next
                                ; ldrb w16, [X(header)]
                                ; cmp w16, crate::entry::OBJECT_BODY_TYPE_TAG
                                ; b.ne =>next
                            );
                        }
                        let flags = self.view.object_flags_byte;
                        let shape_byte = self.view.object_shape_byte;
                        dynasm!(self.ops
                            ; .arch aarch64
                            ; ldrb w16, [X(header), flags]
                            ; tst w16, ordinary_mask
                            ; b.ne =>next
                            ; ldr w16, [X(header), shape_byte]
                            ; cbz w16, =>next
                        );
                        self.load_immediate(17, u64::from(shape));
                        dynasm!(self.ops ; .arch aarch64 ; cmp w16, w17 ; b.ne =>next);
                    }
                    Op::GuardDictionaryLayout { layout, .. } => {
                        let shape_byte = self.view.object_shape_byte;
                        let kind_byte = self.view.shape_kind_byte;
                        let exotic_byte = self.view.object_exotic_handle_byte;
                        let layout_byte = self.view.exotic_dictionary_layout_byte;
                        dynasm!(self.ops
                            ; .arch aarch64
                            ; and x17, X(holder), #0xffff_ffff_0000_0000
                            ; ldr w16, [X(holder), shape_byte]
                            ; add x16, x17, x16
                            ; ldrb w16, [x16, kind_byte]
                            ; tst w16, 1u32 << crate::template::arm64::values::SHAPE_KIND_DICTIONARY_BIT
                            ; b.eq =>next
                            ; ldr w16, [X(holder), exotic_byte]
                            ; cbz w16, =>next
                            ; add x16, x17, x16
                            ; ldr w16, [x16, layout_byte]
                        );
                        self.load_immediate(17, layout);
                        dynasm!(self.ops ; .arch aarch64 ; cmp w16, w17 ; b.ne =>next);
                    }
                    Op::GuardAtomSlot { .. } => {
                        // The shape guard before it already proved no
                        // object-local state overrides the slot; the atom's
                        // slot is immutable in that shape.
                    }
                    Op::LoadField {
                        object: operand,
                        value_byte,
                    } => {
                        let header = object(operand);
                        let slab = self.view.object_slab_handle_byte;
                        let words = self.view.object_slab_words_byte;
                        let inline = self.view.object_inline_values_byte;
                        dynasm!(self.ops
                            ; .arch aarch64
                            ; ldr w16, [X(header), slab]
                            ; and x17, X(header), #0xffff_ffff_0000_0000
                            ; orr x17, x17, x16
                            ; add x17, x17, words
                            ; cmp w16, #0
                            ; add x16, XSP(header), inline
                            ; csel x16, x16, x17, eq
                        );
                        if value_byte <= 32760 && value_byte % 8 == 0 {
                            dynasm!(self.ops ; .arch aarch64 ; ldr X(destination), [x16, value_byte]);
                        } else {
                            self.load_immediate(17, u64::from(value_byte));
                            dynasm!(self.ops ; .arch aarch64 ; ldr X(destination), [x16, x17]);
                        }
                        dynasm!(self.ops ; .arch aarch64 ; b =>done);
                    }
                    _ => return Err(Unsupported::OperandShape("graph named load operation")),
                }
            }
            dynasm!(self.ops ; .arch aarch64 ; =>next);
        }
        dynasm!(self.ops ; .arch aarch64 ; b =>exit ; =>done);
        Ok(())
    }

    /// Branch to `exit` unless the body word `guard` names holds its
    /// expected value.
    fn emit_body_guard(&mut self, receiver: u8, guard: JitBodyGuard, exit: DynamicLabel) {
        let byte = guard.byte;
        match guard.width {
            JitGuardWidth::Byte => {
                dynasm!(self.ops ; .arch aarch64 ; ldrb w17, [X(receiver), byte])
            }
            JitGuardWidth::Word32 => {
                dynasm!(self.ops ; .arch aarch64 ; ldr w17, [X(receiver), byte])
            }
            JitGuardWidth::Word64 => {
                dynasm!(self.ops ; .arch aarch64 ; ldr x17, [X(receiver), byte])
            }
        }
        if guard.expect == 0 {
            dynasm!(self.ops ; .arch aarch64 ; cmp x17, #0 ; b.ne =>exit);
        } else if guard.expect < 4096 {
            dynasm!(self.ops ; .arch aarch64 ; cmp x17, guard.expect ; b.ne =>exit);
        } else {
            self.load_immediate(16, u64::from(guard.expect));
            dynasm!(self.ops ; .arch aarch64 ; cmp x17, x16 ; b.ne =>exit);
        }
    }

    /// Set the flags `ne` exactly when the hole bitmap of the numeric storage
    /// at `X(base)` marks `W(index)`. Clobbers `x16`, `x17`.
    fn emit_hole_bit(&mut self, holes: JitHoleBitmap, base: u8, index: u8) {
        let capacity = holes.capacity_byte;
        dynasm!(self.ops
            ; .arch aarch64
            ; ldur w16, [X(base), capacity]
            ; add x16, X(base), x16, lsl #3
            ; lsr w17, W(index), #6
            ; ldr x16, [x16, x17, lsl #3]
            // The shift reads the index's low six bits: its bit in the word.
            ; lsr x16, x16, X(index)
            ; tst x16, #1
        );
    }

    /// The double element at `[X(base) + W(index) << 3]` boxed into
    /// `X(destination)`, or `undefined` for a hole while the array-index
    /// protector holds.
    fn emit_load_holey_float64(
        &mut self,
        node: NodeId,
        holes: JitHoleBitmap,
        [base, index]: [u8; 2],
        double: u8,
        destination: u8,
    ) {
        let exit = self.eager_exit(node, DeoptReason::OutOfBounds);
        let hole = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        self.emit_hole_bit(holes, base, index);
        dynasm!(self.ops
            ; .arch aarch64
            ; b.ne =>hole
            ; ldr D(double), [X(base), W(index), uxtw #3]
        );
        self.emit_box_float64(double, destination);
        dynasm!(self.ops ; .arch aarch64 ; =>done);
        self.deferred
            .push(Box::new(move |codegen: &mut Codegen<'a>| {
                dynasm!(codegen.ops
                    ; .arch aarch64
                    ; =>hole
                    ; ldr x16, [x20, THREAD_OFFSET]
                    ; ldr x16, [x16, ARRAY_INDEX_PROTECTOR]
                    ; ldrb w16, [x16]
                    ; cbnz w16, =>exit
                );
                codegen.load_immediate(destination, VALUE_UNDEFINED);
                dynasm!(codegen.ops ; .arch aarch64 ; b =>done);
            }));
    }

    /// The element at `[X(base) + W(index) << stride]` into `result`.
    fn emit_load_element(
        &mut self,
        node: NodeId,
        element: JitElementRepr,
        base: u8,
        index: u8,
        result: Location,
    ) {
        use JitElementRepr as E;
        match element {
            E::Boxed => {
                let destination = Self::gp(result);
                let exit = self.eager_exit(node, DeoptReason::OutOfBounds);
                let hole = self.ops.new_dynamic_label();
                let done = self.ops.new_dynamic_label();
                dynasm!(self.ops ; .arch aarch64 ; ldr x16, [X(base), W(index), uxtw #3]);
                self.load_immediate(17, VALUE_HOLE);
                dynasm!(self.ops
                    ; .arch aarch64
                    ; cmp x16, x17
                    ; b.eq =>hole
                    ; mov X(destination), x16
                    ; =>done
                );
                // A hole of a plain array reads `undefined` while no indexed
                // property exists on the realm's element prototypes and no
                // indexed accessor exists anywhere.
                self.deferred
                    .push(Box::new(move |codegen: &mut Codegen<'a>| {
                        dynasm!(codegen.ops
                            ; .arch aarch64
                            ; =>hole
                            ; ldr x16, [x20, THREAD_OFFSET]
                            ; ldr x16, [x16, ARRAY_INDEX_PROTECTOR]
                            ; ldrb w16, [x16]
                            ; cbnz w16, =>exit
                        );
                        codegen.load_immediate(destination, VALUE_UNDEFINED);
                        dynasm!(codegen.ops ; .arch aarch64 ; b =>done);
                    }));
            }
            E::Int8 => {
                let destination = Self::gp(result);
                dynasm!(self.ops ; .arch aarch64 ; ldrsb W(destination), [X(base), W(index), uxtw]);
            }
            E::Uint8 | E::Uint8Clamped => {
                let destination = Self::gp(result);
                dynasm!(self.ops ; .arch aarch64 ; ldrb W(destination), [X(base), W(index), uxtw]);
            }
            E::Int16 => {
                let destination = Self::gp(result);
                dynasm!(self.ops
                    ; .arch aarch64
                    ; ldrsh W(destination), [X(base), W(index), uxtw #1]
                );
            }
            E::Uint16 => {
                let destination = Self::gp(result);
                dynasm!(self.ops
                    ; .arch aarch64
                    ; ldrh W(destination), [X(base), W(index), uxtw #1]
                );
            }
            E::Int32 => {
                let destination = Self::gp(result);
                dynasm!(self.ops ; .arch aarch64 ; ldr W(destination), [X(base), W(index), uxtw #2]);
            }
            E::Uint32 => {
                let destination = Self::gp(result);
                let exit = self.eager_exit(node, DeoptReason::LostPrecision);
                dynasm!(self.ops
                    ; .arch aarch64
                    ; ldr w16, [X(base), W(index), uxtw #2]
                    ; cmp w16, #0
                    ; b.lt =>exit
                    ; mov W(destination), w16
                );
            }
            E::Float32 => {
                let destination = Self::fp(result);
                dynasm!(self.ops
                    ; .arch aarch64
                    ; ldr s31, [X(base), W(index), uxtw #2]
                    ; fcvt D(destination), s31
                );
            }
            E::Float64 => {
                let destination = Self::fp(result);
                dynasm!(self.ops ; .arch aarch64 ; ldr D(destination), [X(base), W(index), uxtw #3]);
            }
        }
    }

    /// Store `value` at `[X(base) + W(index) << stride]`; the builder already
    /// converted it to the element's representation.
    fn emit_store_element(
        &mut self,
        element: JitElementRepr,
        base: u8,
        index: u8,
        value: Location,
    ) -> Result<(), Unsupported> {
        use JitElementRepr as E;
        match (element, value) {
            (E::Boxed, Location::Gp(value)) => {
                dynasm!(self.ops ; .arch aarch64 ; str X(value), [X(base), W(index), uxtw #3]);
            }
            (E::Int8 | E::Uint8, Location::Gp(value)) => {
                dynasm!(self.ops ; .arch aarch64 ; strb W(value), [X(base), W(index), uxtw]);
            }
            (E::Int16 | E::Uint16, Location::Gp(value)) => {
                dynasm!(self.ops ; .arch aarch64 ; strh W(value), [X(base), W(index), uxtw #1]);
            }
            (E::Int32 | E::Uint32, Location::Gp(value)) => {
                dynasm!(self.ops ; .arch aarch64 ; str W(value), [X(base), W(index), uxtw #2]);
            }
            (E::Uint8Clamped, Location::Gp(value)) => dynasm!(self.ops
                ; .arch aarch64
                ; cmp WSP(value), #0
                ; csel w16, wzr, W(value), lt
                ; mov w17, #255
                ; cmp w16, w17
                ; csel w16, w17, w16, gt
                ; strb w16, [X(base), W(index), uxtw]
            ),
            // §7.1.12 ToUint8Clamp of a double: round half to even, NaN and
            // negatives to 0, saturated to 255.
            (E::Uint8Clamped, Location::Fp(value)) => dynasm!(self.ops
                ; .arch aarch64
                ; fcvtnu w16, D(value)
                ; mov w17, #255
                ; cmp w16, w17
                ; csel w16, w17, w16, hi
                ; strb w16, [X(base), W(index), uxtw]
            ),
            (E::Float32, Location::Fp(value)) => dynasm!(self.ops
                ; .arch aarch64
                ; fcvt s31, D(value)
                ; str s31, [X(base), W(index), uxtw #2]
            ),
            (E::Float64, Location::Fp(value)) => {
                dynasm!(self.ops ; .arch aarch64 ; str D(value), [X(base), W(index), uxtw #3]);
            }
            _ => return Err(Unsupported::OperandShape("graph element store operand")),
        }
        Ok(())
    }

    /// The barrier after a tagged element store: a non-cell needs nothing;
    /// a cell while marking, or a young cell, reaches the slab barrier, which
    /// marks the slot dirty and remembers the slab.
    fn emit_element_write_barrier(&mut self, node: NodeId, base: u8, index: u8, value: u8) {
        let flags_byte = self.view.gc_barrier.header_flags_byte;
        let young = self.view.gc_barrier.young_flag;
        let slow = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        self.load_immediate(16, NOT_CELL_MASK);
        dynasm!(self.ops
            ; .arch aarch64
            ; tst X(value), x16
            ; b.ne =>done
            ; ldr x16, [x20, THREAD_OFFSET]
            ; ldr x16, [x16, VM_THREAD_MARKING_FLAG_CELL_OFFSET]
            ; ldrb w16, [x16]
            ; cbnz w16, =>slow
            ; ldrb w16, [X(value), flags_byte]
            ; movz w17, u32::from(young)
            ; tst w16, w17
            ; b.ne =>slow
            ; =>done
        );
        let live = self.allocation.node(node).live_registers.clone();
        self.deferred
            .push(Box::new(move |codegen: &mut Codegen<'a>| {
                dynasm!(codegen.ops ; .arch aarch64 ; =>slow);
                let saved = codegen.emit_save_registers(&live);
                // The operands may sit in any argument register: park them
                // before the arguments are written.
                dynasm!(codegen.ops
                    ; .arch aarch64
                    ; stp X(base), X(index), [sp, #-16]!
                    ; mov x17, X(value)
                    ; ldr x0, [x20, THREAD_OFFSET]
                    ; ldr x0, [x0, VM_THREAD_GC_HEAP_OFFSET]
                    ; ldp x1, x2, [sp], #16
                    ; mov w2, w2
                    ; mov x3, x17
                );
                emit_load_symbol_u64(
                    &mut codegen.ops,
                    &mut codegen.relocations,
                    16,
                    otter_vm::runtime_stubs::ELEMENT_WRITE_BARRIER_MUTATING.entry_addr() as u64,
                    RelocationTarget::runtime_stub(abi::STUB_ELEMENT_WRITE_BARRIER),
                );
                dynasm!(codegen.ops ; .arch aarch64 ; blr x16);
                codegen.emit_restore_registers(&live, saved);
                dynasm!(codegen.ops ; .arch aarch64 ; b =>done);
            }));
    }

    fn emit_write_barrier(&mut self, node: NodeId, object: u8, value: u8) {
        let flags_byte = self.view.gc_barrier.header_flags_byte;
        let young = self.view.gc_barrier.young_flag;
        let settled = young | self.view.gc_barrier.remembered_flag;
        let slow = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        self.load_immediate(16, NOT_CELL_MASK);
        dynasm!(self.ops
            ; .arch aarch64
            ; tst X(value), x16
            ; b.ne =>done
            ; ldr x16, [x20, THREAD_OFFSET]
            ; ldr x16, [x16, VM_THREAD_MARKING_FLAG_CELL_OFFSET]
            ; ldrb w16, [x16]
            ; cbnz w16, =>slow
            ; ldrb w16, [X(object), flags_byte]
            ; movz w17, u32::from(settled)
            ; tst w16, w17
            ; b.ne =>done
            ; ldrb w16, [X(value), flags_byte]
            ; movz w17, u32::from(young)
            ; tst w16, w17
            ; b.ne =>slow
            ; =>done
        );
        let live = self.allocation.node(node).live_registers.clone();
        self.deferred
            .push(Box::new(move |codegen: &mut Codegen<'a>| {
                dynasm!(codegen.ops ; .arch aarch64 ; =>slow);
                let saved = codegen.emit_save_registers(&live);
                // The operands may sit in any argument register: park them
                // in the scratch pair before the arguments are written.
                dynasm!(codegen.ops
                    ; .arch aarch64
                    ; mov x16, X(object)
                    ; mov x17, X(value)
                    ; ldr x0, [x20, THREAD_OFFSET]
                    ; ldr x0, [x0, VM_THREAD_GC_HEAP_OFFSET]
                    ; mov x1, x16
                    ; mov x2, x17
                );
                emit_load_symbol_u64(
                    &mut codegen.ops,
                    &mut codegen.relocations,
                    16,
                    otter_vm::runtime_stubs::WRITE_BARRIER_MUTATING.entry_addr() as u64,
                    RelocationTarget::runtime_stub(abi::STUB_WRITE_BARRIER),
                );
                dynasm!(codegen.ops ; .arch aarch64 ; blr x16);
                codegen.emit_restore_registers(&live, saved);
                dynasm!(codegen.ops ; .arch aarch64 ; b =>done);
            }));
    }

    /// Push every live caller-saved register; returns the bytes pushed.
    /// Only for slow paths that cannot collect garbage.
    fn emit_save_registers(&mut self, live: &[(Location, Repr)]) -> u32 {
        let bytes = (live.len() as u32 * 8).next_multiple_of(16);
        if bytes == 0 {
            return 0;
        }
        dynasm!(self.ops ; .arch aarch64 ; sub sp, sp, bytes);
        for (index, (location, _)) in live.iter().enumerate() {
            let offset = index as u32 * 8;
            match *location {
                Location::Gp(register) => {
                    dynasm!(self.ops ; .arch aarch64 ; str X(register), [sp, offset])
                }
                Location::Fp(register) => {
                    dynasm!(self.ops ; .arch aarch64 ; str D(register), [sp, offset])
                }
                _ => {}
            }
        }
        bytes
    }

    fn emit_restore_registers(&mut self, live: &[(Location, Repr)], bytes: u32) {
        if bytes == 0 {
            return;
        }
        for (index, (location, _)) in live.iter().enumerate() {
            let offset = index as u32 * 8;
            match *location {
                Location::Gp(register) => {
                    dynasm!(self.ops ; .arch aarch64 ; ldr X(register), [sp, offset])
                }
                Location::Fp(register) => {
                    dynasm!(self.ops ; .arch aarch64 ; ldr D(register), [sp, offset])
                }
                _ => {}
            }
        }
        dynasm!(self.ops ; .arch aarch64 ; add sp, sp, bytes);
    }

    // ------------------------------------------------------------------
    // Generic operations
    // ------------------------------------------------------------------

    /// Whether a throw at `pc` may be caught in this frame.
    fn in_exception_region(&self, pc: u32) -> bool {
        self.view.code_block.control_flow().handler_at(pc).is_some()
    }

    /// Run the baseline operation of the instruction at `pc` on the frame
    /// window. Every allocatable register is free here (a generic node is a
    /// call); its inputs are stored into their window registers first.
    /// Name, in the frame record, the safepoint that roots `slots` while the
    /// next call runs: the collector traces exactly those tagged slots.
    fn stamp_safepoint(&mut self, node: NodeId, slots: Vec<u32>) {
        // A point inside an inlined body names the frames it runs in, so
        // stack walks and runtime operations see the callee's position.
        let inline_frames = self.inline_frames(node);
        let call_pc = if inline_frames.is_empty() {
            NO_CALL_PC
        } else {
            self.graph.outer_pc(node)
        };
        let shared = inline_frames
            .is_empty()
            .then(|| self.site_ids.get(&slots).copied())
            .flatten();
        let id = match shared {
            Some(id) => id,
            None => {
                let id = FIRST_SITE_SAFEPOINT - self.site_records.len() as abi::SafepointId;
                self.site_records.push(SafepointRecord {
                    id,
                    frame_state: NO_FRAME_STATE,
                    tagged_locations: slots
                        .iter()
                        .map(|&index| TaggedLocation {
                            kind: TaggedLocationKind::SpillSlot,
                            index: index as u16,
                        })
                        .collect(),
                    call_pc,
                    inline_frames,
                });
                if call_pc == NO_CALL_PC {
                    self.site_ids.insert(slots, id);
                }
                id
            }
        };
        self.load_immediate(16, u64::from(id));
        dynasm!(self.ops
            ; .arch aarch64
            ; str w16, [x21, abi::NATIVE_FRAME_CALL_SITE_OFFSET]
        );
    }

    /// Stamp the safepoint of a call node: the tagged slots of every value
    /// live across it.
    fn stamp_node_safepoint(&mut self, node: NodeId) {
        let slots = self
            .allocation
            .node(node)
            .gc_roots
            .clone()
            .expect("a call node has its roots");
        self.stamp_safepoint(node, slots);
    }

    /// The inlined frames `node` runs in, outermost first: each body's
    /// function standing on its call into the next, the innermost at the
    /// node's own instruction. Empty for a node of the compiled function.
    fn inline_frames(&self, node: NodeId) -> Box<[DeoptFrame<Option<u16>>]> {
        let data = self.graph.node(node);
        let mut frames = Vec::new();
        let mut origin = data.origin;
        let mut byte_pc = self
            .view_of(node)
            .instructions
            .get(data.pc as usize)
            .map_or(0, |instruction| instruction.byte_pc);
        while origin != 0 {
            let body = self.graph.inlined[usize::from(origin) - 1];
            frames.push(DeoptFrame {
                function_id: body.function_id,
                byte_pc,
                entry: None,
                slots: Box::new([]),
            });
            byte_pc = body.call_byte_pc;
            origin = body.parent;
        }
        frames.reverse();
        frames.into_boxed_slice()
    }

    /// The snapshot of the body `node` belongs to.
    fn view_of(&self, node: NodeId) -> &'a JitCompileSnapshot {
        match self.graph.node(node).origin {
            0 => self.view,
            origin => &self.inline_views[usize::from(origin) - 1],
        }
    }

    /// `[[Call]]` of the callee in `x1` (its `[[Construct]]` with itself as
    /// `new.target` when `construct`), the node's other inputs its
    /// arguments, with the result in `x0`. The actual span is pushed first,
    /// so the identity proof's scratch cannot overwrite an argument; a callee
    /// the proof refuses takes the generic entry with the same span. A callee
    /// that retired itself for a tail call it staged returns `Continue`, and
    /// the staged call is entered in its place.
    fn emit_call_js(
        &mut self,
        node: NodeId,
        pc: u32,
        plan: Option<otter_vm::jit::JitDirectCallPlan>,
        construct: bool,
        receiver: bool,
        allocation: Option<otter_vm::jit::JitReceiverAllocationPlan>,
    ) -> Result<(), Unsupported> {
        // `new.target` is the callee itself, already in `x1`; an explicit
        // receiver is in `x2`.
        let new_target = construct.then_some(1);
        let receiver_register = receiver.then_some(2);
        let first_argument = if receiver { 2 } else { 1 };
        use crate::arm64::js_call::{CallTarget, emit_call, emit_enter_staged};
        self.stamp_node_safepoint(node);
        let outer_pc = self.graph.outer_pc(node);
        self.load_immediate(16, u64::from(outer_pc));
        dynasm!(self.ops ; .arch aarch64 ; str w16, [x21, crate::entry::NATIVE_FRAME_PC_OFFSET]);
        let inputs = self.allocation.node(node).inputs.clone();
        let arguments = &inputs[first_argument..];
        let count = u32::try_from(arguments.len())
            .map_err(|_| Unsupported::OperandShape("call actual count"))?;
        let pushed = plan.map_or(arguments.len(), |plan| {
            arguments.len().max(usize::from(plan.param_count))
        });
        let bytes = self.emit_push_actuals(arguments, pushed)?;
        let generic = self.ops.new_dynamic_label();
        let returned = self.ops.new_dynamic_label();
        if let Some(plan) = plan {
            dynasm!(self.ops ; .arch aarch64 ; mov x9, x1);
            crate::arm64::inline_guard::emit_cached_identity(
                &mut self.ops,
                &mut self.relocations,
                self.view,
                plan,
                pc,
                generic,
            );
            // The proven constructor's receiver, allocated before it is
            // entered; a probe miss leaves it to the constructor. The callee
            // stays in `x9`, which the probe does not touch, and nothing
            // between allocation and the call can collect.
            let known_receiver = if let Some(allocation) = allocation {
                let missed = self.ops.new_dynamic_label();
                let allocated = self.ops.new_dynamic_label();
                dynasm!(self.ops ; .arch aarch64 ; mov x2, x9);
                crate::arm64::emit_receiver_candidate_probe(
                    &mut self.ops,
                    &mut self.relocations,
                    self.view,
                    allocation,
                    20,
                );
                dynasm!(self.ops ; .arch aarch64 ; cbz x1, =>missed);
                crate::arm64::emit_receiver_publication_effect(&mut self.ops, self.view, 20);
                dynasm!(self.ops ; .arch aarch64 ; b =>allocated ; =>missed);
                self.load_immediate(0, VALUE_UNDEFINED);
                dynasm!(self.ops
                    ; .arch aarch64
                    ; =>allocated
                    ; mov x2, x0
                    ; mov x1, x9
                );
                Some(2)
            } else {
                receiver_register
            };
            emit_call(
                &mut self.ops,
                &mut self.relocations,
                self.transitions,
                20,
                1,
                known_receiver,
                new_target,
                count,
                CallTarget::Known {
                    entry_cell: plan.entry_cell,
                    function_id: plan.function_id,
                },
            );
            dynasm!(self.ops ; .arch aarch64 ; b =>returned);
        }
        dynasm!(self.ops ; .arch aarch64 ; =>generic);
        emit_call(
            &mut self.ops,
            &mut self.relocations,
            self.transitions,
            20,
            1,
            receiver_register,
            new_target,
            count,
            CallTarget::Generic,
        );
        dynasm!(self.ops ; .arch aarch64 ; =>returned);
        self.emit_pop_actuals(bytes);
        let (threw, committed_throw) = self.throw_targets(node);
        let threw = self.cond_target(threw);
        let committed_throw = self.cond_target(committed_throw);
        let completion = self.ops.new_dynamic_label();
        let error = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        dynasm!(self.ops
            ; .arch aarch64
            ; =>completion
            ; cbz x1, =>done
            ; cmp x1, abi::NativeResultStatus::Continue as u32
            ; b.ne =>error
        );
        emit_enter_staged(&mut self.ops, &mut self.relocations, self.transitions, 20);
        dynasm!(self.ops
            ; .arch aarch64
            ; b =>completion
            ; =>error
            ; cmp x1, abi::NativeResultStatus::Throw as u32
            ; b.eq =>committed_throw
            ; b =>threw
            ; =>done
        );
        Ok(())
    }

    /// Reserve an actual span of `pushed` words below `sp` and fill it from
    /// `arguments`, padding with `undefined`. Returns the reserved bytes.
    fn emit_push_actuals(
        &mut self,
        arguments: &[Location],
        pushed: usize,
    ) -> Result<u32, Unsupported> {
        let bytes = crate::call_linkage::pushed_argument_bytes(pushed)?;
        if bytes == 0 {
            return Ok(0);
        }
        dynasm!(self.ops ; .arch aarch64 ; sub sp, sp, bytes);
        self.sp_delta += bytes;
        for index in 0..pushed {
            match arguments.get(index) {
                Some(&location) => self.emit_move(location, Location::Gp(16)),
                None => self.load_immediate(16, VALUE_UNDEFINED),
            }
            let offset = (index * 8) as u32;
            dynasm!(self.ops ; .arch aarch64 ; str x16, [sp, offset]);
        }
        Ok(bytes)
    }

    /// Release a span [`Self::emit_push_actuals`] reserved.
    fn emit_pop_actuals(&mut self, bytes: u32) {
        if bytes != 0 {
            dynasm!(self.ops ; .arch aarch64 ; add sp, sp, bytes);
            self.sp_delta -= bytes;
        }
    }

    /// Where a throw at `node` goes: the shared throw paths, or, inside a
    /// region the frame may catch, paths that first rebuild the interpreter
    /// frame so the interpreter enters the handler. The first label takes a
    /// parked error, the second an exception in `x0`.
    fn throw_targets(&mut self, node: NodeId) -> (DynamicLabel, DynamicLabel) {
        self.throw_targets_at(node, None)
    }

    /// [`Self::throw_targets`] for a point where the node's eager values
    /// are at `locations` rather than where the allocator put them.
    fn throw_targets_at(
        &mut self,
        node: NodeId,
        locations: Option<Box<[Location]>>,
    ) -> (DynamicLabel, DynamicLabel) {
        if self.in_exception_region(self.graph.outer_pc(node)) {
            let index = self.exit_index(node, ExitReason::RuntimeTransition, ExitAction::Resume);
            self.exits[index as usize].locations = locations;
            let threw = self.ops.new_dynamic_label();
            let committed_throw = self.ops.new_dynamic_label();
            let shared_threw = self.threw;
            let shared_committed = self.committed_throw;
            let materialize = self.materialize;
            let scratch = self.slots.exception_scratch();
            self.deferred
                .push(Box::new(move |codegen: &mut Codegen<'a>| {
                    dynasm!(codegen.ops
                        ; .arch aarch64
                        ; =>threw
                        ; movz w17, index
                        ; bl =>materialize
                        ; b =>shared_threw
                        ; =>committed_throw
                    );
                    codegen.store_slot_gp(0, scratch);
                    dynasm!(codegen.ops ; .arch aarch64 ; movz w17, index ; bl =>materialize);
                    codegen.load_slot_gp(0, scratch);
                    dynasm!(codegen.ops ; .arch aarch64 ; b =>shared_committed);
                }));
            (threw, committed_throw)
        } else {
            (self.threw, self.committed_throw)
        }
    }

    fn emit_generic(
        &mut self,
        node: NodeId,
        pc: u32,
        registers: &[u16],
    ) -> Result<(), Unsupported> {
        self.stamp_node_safepoint(node);
        self.load_immediate(16, u64::from(pc));
        dynasm!(self.ops ; .arch aarch64 ; str w16, [x21, crate::entry::NATIVE_FRAME_PC_OFFSET]);
        let inputs = self.allocation.node(node).inputs.clone();
        for (&register, &location) in registers.iter().zip(&inputs) {
            let offset = u32::from(register) * 8;
            let source = match location {
                Location::Gp(source) => source,
                other => {
                    self.emit_move(other, Location::Gp(16));
                    16
                }
            };
            if offset <= 32760 {
                dynasm!(self.ops ; .arch aarch64 ; str X(source), [x19, offset]);
            } else {
                self.load_immediate(17, u64::from(offset));
                dynasm!(self.ops ; .arch aarch64 ; str X(source), [x19, x17]);
            }
        }
        let (threw, committed_throw) = self.throw_targets(node);
        let exit = |codegen: &mut Self, reason: ExitReason, action: ExitAction| {
            codegen.typed_exit(node, reason, action)
        };
        let runtime_transition = exit(self, ExitReason::RuntimeTransition, ExitAction::Resume);
        // The baseline operation branches to these conditionally.
        let threw = self.cond_target(threw);
        let committed_throw = self.cond_target(committed_throw);
        let (returned, propagate, fatal) = (self.returned, self.propagate, self.fatal);
        let returned = self.cond_target(returned);
        let propagate = self.cond_target(propagate);
        let fatal = self.cond_target(fatal);
        let exits = crate::template::arm64::OperationExits {
            type_mismatch_exit: exit(self, ExitReason::TypeMismatch, ExitAction::Recompile),
            identity_guard_exit: exit(self, ExitReason::IdentityGuard, ExitAction::Recompile),
            allocation_miss_exit: exit(self, ExitReason::AllocationMiss, ExitAction::Resume),
            unsupported_exit: exit(
                self,
                ExitReason::UnsupportedOperation,
                ExitAction::Recompile,
            ),
            runtime_transition_exit: runtime_transition,
            backedge_relink_exit: exit(self, ExitReason::Interrupt, ExitAction::Resume),
            bail: runtime_transition,
            returned,
            committed_throw,
            threw,
            propagate_throw: propagate,
            fatal,
        };
        let positions = self.plan_index.get(&pc).cloned().unwrap_or_default();
        let labels = std::collections::BTreeMap::new();
        let mut numeric_slow_paths = Vec::new();
        let mut coercion_slow_paths = Vec::new();
        for position in positions {
            let instruction = &self.plan.instructions[position];
            crate::template::arm64::emit_operation(
                crate::template::arm64::OperationContext {
                    ops: &mut self.ops,
                    relocations: &mut self.relocations,
                    transitions: self.transitions,
                    view: self.view,
                    plan: self.plan,
                    labels: &labels,
                    exits,
                    poll_entry: self.transitions.entry(abi::STUB_JIT_BACKEDGE_POLL),
                    far_branches: true,
                    load_ic_cells: &mut self.load_ic_cells,
                    next_load_ic: &mut self.next_load_ic,
                    store_ic_cells: &mut self.store_ic_cells,
                    next_store_ic: &mut self.next_store_ic,
                    numeric_slow_paths: &mut numeric_slow_paths,
                    coercion_slow_paths: &mut coercion_slow_paths,
                    direct_call_events: &mut self.no_direct_call_events,
                    code_map: &mut self.no_code_map,
                },
                instruction,
                false,
            )?;
        }
        if !numeric_slow_paths.is_empty() || !coercion_slow_paths.is_empty() {
            let fatal = self.cond_target(self.fatal);
            let transitions = self.transitions;
            self.deferred
                .push(Box::new(move |codegen: &mut Codegen<'a>| {
                    crate::template::arm64::arith::emit_numeric_slow_paths(
                        &mut codegen.ops,
                        &mut codegen.relocations,
                        transitions,
                        numeric_slow_paths,
                        threw,
                        fatal,
                    );
                    crate::template::arm64::arith::emit_coercion_slow_paths(
                        &mut codegen.ops,
                        &mut codegen.relocations,
                        transitions,
                        coercion_slow_paths,
                        threw,
                        fatal,
                    );
                }));
        }
        Ok(())
    }

    /// An exit recipe for `node`'s eager state that no branch leaves
    /// through: the frame is rebuilt and the code continues.
    fn exit_index(&mut self, node: NodeId, reason: ExitReason, action: ExitAction) -> u32 {
        let label = self.ops.new_dynamic_label();
        self.exits.push(ExitSite {
            label,
            node,
            lazy: false,
            reason,
            action,
            locations: None,
        });
        (self.exits.len() - 1) as u32
    }

    // ------------------------------------------------------------------
    // Control
    // ------------------------------------------------------------------

    fn edge_moves(&self, from: BlockId, to: BlockId) -> Vec<Move> {
        let Some(edge) = self.allocation.edges.get(&(from, to)) else {
            return Vec::new();
        };
        let mut moves = edge.moves.clone();
        for &(phi, location) in &edge.phi_inputs {
            let phi_allocation = self.allocation.node(phi);
            if phi_allocation.skipped {
                continue;
            }
            if let Some(target) = phi_allocation.result {
                moves.push(Move {
                    from: location,
                    to: target,
                });
            }
        }
        moves
    }

    fn emit_jump(&mut self, from: BlockId, to: BlockId, next: Option<BlockId>) {
        let moves = self.edge_moves(from, to);
        self.emit_parallel_moves(moves);
        if next != Some(to) {
            let label = self.labels[&to];
            dynasm!(self.ops ; .arch aarch64 ; b =>label);
        }
    }

    fn emit_control(
        &mut self,
        block: BlockId,
        control: NodeId,
        next: Option<BlockId>,
    ) -> Result<(), Unsupported> {
        let data = self.graph.node(control);
        let allocation = self.allocation.node(control);
        match data.kind {
            Kind::Jump(target) => self.emit_jump(block, target, next),
            Kind::JumpLoop(target) => {
                self.emit_backedge_poll(control);
                let moves = self.edge_moves(block, target);
                self.emit_parallel_moves(moves);
                let label = self.labels[&target];
                dynasm!(self.ops ; .arch aarch64 ; b =>label);
            }
            Kind::Branch {
                kind,
                if_true,
                if_false,
            } => {
                let false_label = self.labels[&if_false];
                let true_label = self.labels[&if_true];
                let true_target = self.cond_target(true_label);
                match kind {
                    BranchKind::Int32(condition) => {
                        let (a, b) = (
                            Self::gp(allocation.inputs[0]),
                            Self::gp(allocation.inputs[1]),
                        );
                        dynasm!(self.ops ; .arch aarch64 ; cmp W(a), W(b));
                        self.emit_branch_condition(condition, false, true_target);
                    }
                    BranchKind::Float64(condition) => {
                        let (a, b) = (
                            Self::fp(allocation.inputs[0]),
                            Self::fp(allocation.inputs[1]),
                        );
                        dynasm!(self.ops ; .arch aarch64 ; fcmp D(a), D(b));
                        self.emit_branch_condition(condition, true, true_target);
                    }
                    BranchKind::TaggedEqual => {
                        let (a, b) = (
                            Self::gp(allocation.inputs[0]),
                            Self::gp(allocation.inputs[1]),
                        );
                        dynasm!(self.ops ; .arch aarch64 ; cmp X(a), X(b) ; b.eq =>true_target);
                    }
                    BranchKind::WordEqual(constant) => {
                        let a = Self::gp(allocation.inputs[0]);
                        self.load_immediate(16, u64::from(constant));
                        dynasm!(self.ops ; .arch aarch64 ; cmp X(a), x16 ; b.eq =>true_target);
                    }
                    BranchKind::Nullish => {
                        let a = Self::gp(allocation.inputs[0]);
                        dynasm!(self.ops
                            ; .arch aarch64
                            ; cmp XSP(a), VALUE_NULL as u32
                            ; b.eq =>true_target
                            ; cmp XSP(a), VALUE_UNDEFINED as u32
                            ; b.eq =>true_target
                        );
                    }
                    BranchKind::Truthy
                        if self.graph.node(data.inputs[0]).kind.produces_boolean() =>
                    {
                        let a = Self::gp(allocation.inputs[0]);
                        dynasm!(self.ops
                            ; .arch aarch64
                            ; cmp XSP(a), VALUE_TRUE as u32
                            ; b.eq =>true_target
                        );
                    }
                    BranchKind::Truthy => {
                        let a = Self::gp(allocation.inputs[0]);
                        self.emit_truthy_branch(control, a, true_label, false_label);
                    }
                }
                if next != Some(if_false) {
                    dynasm!(self.ops ; .arch aarch64 ; b =>false_label);
                }
            }
            Kind::Return => {
                let returned = self.returned;
                dynasm!(self.ops ; .arch aarch64 ; b =>returned);
            }
            Kind::Deopt(reason) => {
                let exit = self.eager_exit(control, reason);
                dynasm!(self.ops ; .arch aarch64 ; b =>exit);
            }
            ref other => return Err(Unsupported::OperandShape(node_name(other))),
        }
        Ok(())
    }

    fn emit_branch_condition(&mut self, condition: Condition, float: bool, target: DynamicLabel) {
        match (condition, float) {
            (Condition::Equal, _) => dynasm!(self.ops ; .arch aarch64 ; b.eq =>target),
            (Condition::NotEqual, _) => dynasm!(self.ops ; .arch aarch64 ; b.ne =>target),
            (Condition::Less, false) => dynasm!(self.ops ; .arch aarch64 ; b.lt =>target),
            (Condition::Less, true) => dynasm!(self.ops ; .arch aarch64 ; b.mi =>target),
            (Condition::LessEqual, false) => dynasm!(self.ops ; .arch aarch64 ; b.le =>target),
            (Condition::LessEqual, true) => dynasm!(self.ops ; .arch aarch64 ; b.ls =>target),
            (Condition::Greater, _) => dynasm!(self.ops ; .arch aarch64 ; b.gt =>target),
            (Condition::GreaterEqual, _) => dynasm!(self.ops ; .arch aarch64 ; b.ge =>target),
        }
    }

    /// `X(destination)` = the tagged boolean `ToBoolean(X(value))`, negated
    /// for `!`: immediates and int32 inline, every other value through the
    /// VM's leaf predicate with live registers saved.
    fn emit_to_boolean(&mut self, node: NodeId, value: u8, destination: u8, negate: bool) {
        let truthy = self.ops.new_dynamic_label();
        let falsy = self.ops.new_dynamic_label();
        let slow = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        let not_int = self.ops.new_dynamic_label();
        dynasm!(self.ops
            ; .arch aarch64
            ; cmp XSP(value), VALUE_TRUE as u32
            ; b.eq =>truthy
            ; cmp XSP(value), VALUE_FALSE as u32
            ; b.eq =>falsy
            ; cmp XSP(value), VALUE_UNDEFINED as u32
            ; b.eq =>falsy
            ; cmp XSP(value), VALUE_NULL as u32
            ; b.eq =>falsy
        );
        self.load_immediate(16, NUMBER_TAG);
        dynasm!(self.ops
            ; .arch aarch64
            ; cmp X(value), x16
            ; b.lo =>not_int
            ; cmp WSP(value), #0
            ; b.eq =>falsy
            ; b =>truthy
            ; =>not_int
            ; b =>slow
        );
        let (when_truthy, when_falsy) = if negate {
            (VALUE_FALSE, VALUE_TRUE)
        } else {
            (VALUE_TRUE, VALUE_FALSE)
        };
        dynasm!(self.ops ; .arch aarch64 ; =>truthy);
        self.load_immediate(destination, when_truthy);
        dynasm!(self.ops ; .arch aarch64 ; b =>done ; =>falsy);
        self.load_immediate(destination, when_falsy);
        dynasm!(self.ops ; .arch aarch64 ; =>done);
        let live = self.allocation.node(node).live_registers.clone();
        self.deferred
            .push(Box::new(move |codegen: &mut Codegen<'a>| {
                dynasm!(codegen.ops ; .arch aarch64 ; =>slow);
                let saved = codegen.emit_save_registers(&live);
                dynasm!(codegen.ops
                    ; .arch aarch64
                    ; mov x1, X(value)
                    ; ldr x0, [x20, THREAD_OFFSET]
                    ; ldr x0, [x0, VM_THREAD_GC_HEAP_OFFSET]
                );
                emit_load_symbol_u64(
                    &mut codegen.ops,
                    &mut codegen.relocations,
                    16,
                    otter_vm::runtime_stubs::TO_BOOLEAN_LEAF.entry_addr() as u64,
                    RelocationTarget::runtime_stub(abi::STUB_TO_BOOLEAN_LEAF),
                );
                dynasm!(codegen.ops ; .arch aarch64 ; blr x16 ; mov x16, x0);
                codegen.emit_restore_registers(&live, saved);
                dynasm!(codegen.ops
                    ; .arch aarch64
                    ; cmp x16, VALUE_TRUE as u32
                    ; b.eq =>truthy
                    ; b =>falsy
                );
            }));
    }

    /// Branch on `ToBoolean(X(value))`: immediates and int32 inline, every
    /// other value through the VM's leaf predicate with live registers saved.
    fn emit_truthy_branch(
        &mut self,
        node: NodeId,
        value: u8,
        if_true: DynamicLabel,
        if_false: DynamicLabel,
    ) {
        let not_int = self.ops.new_dynamic_label();
        let slow = self.ops.new_dynamic_label();
        let true_target = self.cond_target(if_true);
        let false_target = self.cond_target(if_false);
        dynasm!(self.ops
            ; .arch aarch64
            ; cmp XSP(value), VALUE_TRUE as u32
            ; b.eq =>true_target
            ; cmp XSP(value), VALUE_FALSE as u32
            ; b.eq =>false_target
            ; cmp XSP(value), VALUE_UNDEFINED as u32
            ; b.eq =>false_target
            ; cmp XSP(value), VALUE_NULL as u32
            ; b.eq =>false_target
        );
        self.load_immediate(16, NUMBER_TAG);
        dynasm!(self.ops
            ; .arch aarch64
            ; cmp X(value), x16
            ; b.lo =>not_int
            ; cmp WSP(value), #0
            ; b.eq =>false_target
            ; b =>if_true
            ; =>not_int
            ; b =>slow
        );
        let live = self.allocation.node(node).live_registers.clone();
        self.deferred
            .push(Box::new(move |codegen: &mut Codegen<'a>| {
                dynasm!(codegen.ops ; .arch aarch64 ; =>slow);
                let saved = codegen.emit_save_registers(&live);
                dynasm!(codegen.ops
                    ; .arch aarch64
                    ; mov x1, X(value)
                    ; ldr x0, [x20, THREAD_OFFSET]
                    ; ldr x0, [x0, VM_THREAD_GC_HEAP_OFFSET]
                );
                emit_load_symbol_u64(
                    &mut codegen.ops,
                    &mut codegen.relocations,
                    16,
                    otter_vm::runtime_stubs::TO_BOOLEAN_LEAF.entry_addr() as u64,
                    RelocationTarget::runtime_stub(abi::STUB_TO_BOOLEAN_LEAF),
                );
                dynasm!(codegen.ops ; .arch aarch64 ; blr x16 ; mov x16, x0);
                codegen.emit_restore_registers(&live, saved);
                let true_target = codegen.cond_target(if_true);
                dynasm!(codegen.ops
                    ; .arch aarch64
                    ; cmp x16, VALUE_TRUE as u32
                    ; b.eq =>true_target
                    ; b =>if_false
                );
            }));
    }

    /// The interrupt and work-budget poll of a loop back edge, before the
    /// edge's moves. Its slow path saves every occupied register in snapshot
    /// slots the collector sees, since the poll may run JavaScript or
    /// collect.
    fn emit_backedge_poll(&mut self, control: NodeId) {
        let slow = self.ops.new_dynamic_label();
        let resume = self.ops.new_dynamic_label();
        dynasm!(self.ops
            ; .arch aarch64
            ; ldr x16, [x20, THREAD_OFFSET]
            ; ldr x17, [x16, VM_THREAD_INTERRUPT_CELL_OFFSET]
            ; ldrb w17, [x17]
            ; cbnz w17, =>slow
            ; ldr x16, [x16, VM_THREAD_BACKEDGE_FUEL_CELL_OFFSET]
            ; ldr x17, [x16]
            ; subs x17, x17, #1
            ; str x17, [x16]
            ; b.le =>slow
            ; =>resume
        );
        let live = self.allocation.node(control).live_registers.clone();
        let header_pc = self
            .graph
            .node(control)
            .eager
            .map(|state| self.graph.frame_state(state).pc)
            .unwrap_or(0);
        let exit = self.typed_exit(control, ExitReason::Interrupt, ExitAction::Resume);
        let threw = if self.in_exception_region(header_pc) {
            let index = self.exit_index(control, ExitReason::RuntimeTransition, ExitAction::Resume);
            let threw = self.ops.new_dynamic_label();
            let shared = self.threw;
            let materialize = self.materialize;
            self.deferred
                .push(Box::new(move |codegen: &mut Codegen<'a>| {
                    dynasm!(codegen.ops
                        ; .arch aarch64
                        ; =>threw
                        ; movz w17, index
                        ; bl =>materialize
                        ; b =>shared
                    );
                }));
            threw
        } else {
            self.threw
        };
        let fatal = self.fatal;
        let poll = self.transitions.entry(abi::STUB_JIT_BACKEDGE_POLL);
        self.deferred
            .push(Box::new(move |codegen: &mut Codegen<'a>| {
                let restore_resume = codegen.ops.new_dynamic_label();
                let restore_exit = codegen.ops.new_dynamic_label();
                let restore_threw = codegen.ops.new_dynamic_label();
                dynasm!(codegen.ops ; .arch aarch64 ; =>slow);
                let mut roots = codegen
                    .allocation
                    .node(control)
                    .gc_roots
                    .clone()
                    .expect("a back edge has its roots");
                for &(location, repr) in &live {
                    let slot = codegen.slots.snapshot_slot(location, repr);
                    codegen.emit_move(location, slot);
                    if let Location::TaggedSlot(index) = slot {
                        roots.push(index);
                    }
                }
                roots.sort_unstable();
                roots.dedup();
                codegen.stamp_safepoint(control, roots);
                codegen.load_immediate(16, u64::from(header_pc));
                dynasm!(codegen.ops
                    ; .arch aarch64
                    ; str w16, [x21, crate::entry::NATIVE_FRAME_PC_OFFSET]
                    ; mov x0, x20
                );
                emit_load_symbol_u64(
                    &mut codegen.ops,
                    &mut codegen.relocations,
                    16,
                    poll,
                    RelocationTarget::runtime_stub(abi::STUB_JIT_BACKEDGE_POLL),
                );
                dynasm!(codegen.ops
                    ; .arch aarch64
                    ; blr x16
                    ; cmp x0, NativeResultStatus::Success as u32
                    ; b.eq =>restore_resume
                    ; cmp x0, NativeResultStatus::Yield as u32
                    ; b.eq =>restore_resume
                    ; cmp x0, NativeResultStatus::Throw as u32
                    ; b.eq =>restore_threw
                    ; cmp x0, NativeResultStatus::SideExit as u32
                    ; b.eq =>restore_exit
                    ; b =>fatal
                );
                for (label, target) in [
                    (restore_resume, resume),
                    (restore_exit, exit),
                    (restore_threw, threw),
                ] {
                    dynasm!(codegen.ops ; .arch aarch64 ; =>label);
                    for &(location, repr) in &live {
                        let slot = codegen.slots.snapshot_slot(location, repr);
                        codegen.emit_move(slot, location);
                    }
                    dynasm!(codegen.ops ; .arch aarch64 ; b =>target);
                }
            }));
    }

    // ------------------------------------------------------------------
    // Exits
    // ------------------------------------------------------------------

    fn emit_exit_stubs(&mut self) {
        let returned = self.returned;
        let fatal = self.fatal;
        let activation = self.activation;
        let spill = self.spill;
        dynasm!(self.ops
            ; .arch aarch64
            ; =>returned
            ; movz x1, NativeResultStatus::Success as u32
        );
        crate::arm64::frame::emit_epilogue(&mut self.ops, activation, spill);
        dynasm!(self.ops ; .arch aarch64 ; =>fatal);
        self.load_immediate(0, VALUE_UNDEFINED);
        dynasm!(self.ops ; .arch aarch64 ; movz x1, NativeResultStatus::Fatal as u32);
        crate::arm64::frame::emit_epilogue(&mut self.ops, activation, spill);
        self.emit_throw_handlers();
        self.emit_materialize();
        // The handlers above may name exits of their own.
        self.flush_segment();
        self.emit_deopt_handler();
    }

    /// Route a parked error (`threw`) or an exception value in `x0`
    /// (`committed_throw`): a handler of this frame resumes the interpreter
    /// at its entry, otherwise the exception leaves the frame.
    fn emit_throw_handlers(&mut self) {
        let (threw, committed, propagate) = (self.threw, self.committed_throw, self.propagate);
        let (fatal, side_exit) = (self.fatal, self.activation.side_exit);
        let (activation, spill) = (self.activation, self.spill);
        dynasm!(self.ops ; .arch aarch64 ; =>threw ; mov x0, x20);
        emit_load_symbol_u64(
            &mut self.ops,
            &mut self.relocations,
            16,
            self.transitions.entry(abi::STUB_JIT_FINISH_ERROR),
            RelocationTarget::runtime_stub(abi::STUB_JIT_FINISH_ERROR),
        );
        let dispatch = self.ops.new_dynamic_label();
        dynasm!(self.ops
            ; .arch aarch64
            ; blr x16
            ; b =>dispatch
            ; =>committed
            ; mov x1, x0
            ; mov x0, x20
        );
        emit_load_symbol_u64(
            &mut self.ops,
            &mut self.relocations,
            16,
            self.transitions.entry(abi::STUB_JIT_ROUTE_THROW),
            RelocationTarget::runtime_stub(abi::STUB_JIT_ROUTE_THROW),
        );
        dynasm!(self.ops
            ; .arch aarch64
            ; blr x16
            ; =>dispatch
            ; cmp x1, NativeResultStatus::SideExit as u32
            ; b.eq =>side_exit
            ; cmp x1, NativeResultStatus::Throw as u32
            ; b.ne =>fatal
            ; =>propagate
            ; movz x1, NativeResultStatus::Throw as u32
        );
        crate::arm64::frame::emit_epilogue(&mut self.ops, activation, spill);
    }

    /// Rebuild the interpreter frame of the exit in `w17` and return to the
    /// caller (`bl`). Values live only in slots and constants at every user
    /// of this subroutine.
    fn emit_materialize(&mut self) {
        let materialize = self.materialize;
        let fatal = self.fatal;
        let dump_bytes = (DUMP_WORDS as u32 * 8).next_multiple_of(16);
        dynasm!(self.ops
            ; .arch aarch64
            ; =>materialize
            ; str x30, [sp, #-16]!
            ; sub sp, sp, dump_bytes
            ; mov w1, w17
        );
        crate::arm64::frame::emit_publish_lazy_window(
            &mut self.ops,
            self.view.code_block.register_count,
        );
        dynasm!(self.ops ; .arch aarch64 ; mov x0, x20);
        emit_load_symbol_u64(
            &mut self.ops,
            &mut self.relocations,
            2,
            self.deopt_runtime,
            RelocationTarget::DeoptRuntimeData,
        );
        dynasm!(self.ops
            ; .arch aarch64
            ; mov x3, sp
            ; add x4, sp, dump_bytes + 16
            ; mov x5, x19
        );
        emit_load_symbol_u64(
            &mut self.ops,
            &mut self.relocations,
            16,
            self.transitions
                .variadic_entry(abi::STUB_JIT_DEOPT_WRITEBACK),
            RelocationTarget::runtime_stub(abi::STUB_JIT_DEOPT_WRITEBACK),
        );
        dynasm!(self.ops
            ; .arch aarch64
            ; blr x16
            ; add sp, sp, dump_bytes
            ; ldr x30, [sp], #16
            ; cmp x1, NativeResultStatus::SideExit as u32
            ; b.ne =>fatal
            ; ret
        );
    }

    /// The shared deopt handler: dump every allocatable register, rebuild
    /// the interpreter frame from the exit's recipe, then continue in the
    /// interpreter (a called record) or hand the exit to the interpreter
    /// that entered this frame. The exit index is in `w17`.
    fn emit_deopt_handler(&mut self) {
        let deopt = self.deopt;
        let dump_bytes = (DUMP_WORDS as u32 * 8).next_multiple_of(16);
        dynasm!(self.ops ; .arch aarch64 ; =>deopt ; sub sp, sp, dump_bytes);
        for pair in GP_REGISTERS.chunks(2) {
            let offset = dump_index(Location::Gp(pair[0])).expect("a dump slot") as u32 * 8;
            if pair.len() == 2 {
                dynasm!(self.ops ; .arch aarch64 ; stp X(pair[0]), X(pair[1]), [sp, offset as i32]);
            } else {
                dynasm!(self.ops ; .arch aarch64 ; str X(pair[0]), [sp, offset]);
            }
        }
        for &register in FP_REGISTERS {
            let offset = dump_index(Location::Fp(register)).expect("a dump slot") as u32 * 8;
            dynasm!(self.ops ; .arch aarch64 ; str D(register), [sp, offset]);
        }
        dynasm!(self.ops ; .arch aarch64 ; mov w1, w17);
        crate::arm64::frame::emit_publish_lazy_window(
            &mut self.ops,
            self.view.code_block.register_count,
        );
        dynasm!(self.ops ; .arch aarch64 ; mov x0, x20);
        emit_load_symbol_u64(
            &mut self.ops,
            &mut self.relocations,
            2,
            self.deopt_runtime,
            RelocationTarget::DeoptRuntimeData,
        );
        dynasm!(self.ops
            ; .arch aarch64
            ; mov x3, sp
            ; add x4, sp, dump_bytes
            ; mov x5, x19
        );
        emit_load_symbol_u64(
            &mut self.ops,
            &mut self.relocations,
            16,
            self.transitions
                .variadic_entry(abi::STUB_JIT_DEOPT_WRITEBACK),
            RelocationTarget::runtime_stub(abi::STUB_JIT_DEOPT_WRITEBACK),
        );
        let side_exit = self.activation.side_exit;
        let activation = self.activation;
        let spill = self.spill;
        dynasm!(self.ops
            ; .arch aarch64
            ; blr x16
            ; add sp, sp, dump_bytes
            ; cmp x1, NativeResultStatus::SideExit as u32
            ; b.eq =>side_exit
        );
        // An inline chain already completed, or the writeback failed.
        crate::arm64::frame::emit_epilogue(&mut self.ops, activation, spill);
    }
}

/// Lower the frame states of `state`'s inline chain to the VM's frames,
/// outermost first. `locations` holds the chain's values in
/// [`Graph::state_values`] order.
pub(crate) fn deopt_frames(
    graph: &Graph,
    slots: SlotLayout,
    state: FrameStateId,
    locations: &[Location],
) -> Box<[DeoptFrame]> {
    let mut locations = locations.iter().copied();
    let mut slot = |value: NodeId| {
        let location = locations.next().expect("a location per state value");
        deopt_slot(graph, slots, value, location)
    };
    graph
        .state_chain(state)
        .into_iter()
        .map(|state| {
            let data = graph.frame_state(state);
            let entry = data.caller.map(|caller| DeoptFrameEntry {
                return_register: caller.return_register,
                this: slot(caller.this),
                closure: slot(caller.closure),
                new_target: slot(caller.new_target),
            });
            let mut registers: Vec<DeoptSlot> = (0..data.register_count)
                .map(|_| {
                    DeoptSlot::physical(DeoptLocation::Literal(VALUE_UNDEFINED), DeoptRepr::Tagged)
                })
                .collect();
            for &(register, value) in &data.registers {
                registers[usize::from(register)] = slot(value);
            }
            DeoptFrame {
                function_id: data.function_id,
                byte_pc: data.byte_pc,
                entry,
                slots: registers.into_boxed_slice(),
            }
        })
        .collect()
}

/// The VM's slot for `value` at `location`.
fn deopt_slot(graph: &Graph, slots: SlotLayout, value: NodeId, location: Location) -> DeoptSlot {
    let repr = match graph.node(value).repr {
        Repr::Int32 => DeoptRepr::Int32,
        Repr::Float64 => DeoptRepr::Float64,
        _ => DeoptRepr::Tagged,
    };
    let deopt_location = match location {
        Location::Gp(_) | Location::Fp(_) => {
            DeoptLocation::Register(dump_index(location).expect("an allocatable register"))
        }
        Location::TaggedSlot(_) | Location::UntaggedSlot(_) => {
            DeoptLocation::StackSlot(slots.offset(location) as i32)
        }
        Location::Constant(node) => match graph.node(node).kind {
            Kind::ConstTagged(bits) | Kind::ConstFloat64(bits) => DeoptLocation::Literal(bits),
            Kind::ConstInt32(int) => DeoptLocation::Literal(u64::from(int as u32)),
            _ => unreachable!("a constant"),
        },
    };
    DeoptSlot::physical(deopt_location, repr)
}

fn node_name(kind: &Kind) -> &'static str {
    match kind {
        Kind::Generic { .. } => "graph Generic",
        _ => "graph node",
    }
}
