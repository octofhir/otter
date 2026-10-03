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
//!   after every tagged one. One safepoint record per code object roots every
//!   tagged slot; entries publish it on the frame record once.
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
use otter_vm::deopt::{DeoptLocation, DeoptRepr, DeoptSlot};
use otter_vm::native_abi::{self as abi, ExitAction, ExitReason, NativeResultStatus};
use otter_vm::value::tag;
use rustc_hash::FxHashMap;

use super::builder::Built;
use super::ir::{BlockId, BranchKind, Condition, DeoptReason, Graph, Kind, NodeId, Repr};
use super::regalloc::{Allocation, FP_REGISTERS, GP_REGISTERS, Location, Move};
use crate::Unsupported;
use crate::artifact::relocation::{RelocationCapture, RelocationTarget};
use crate::entry::{
    NATIVE_FRAME_SELF_OFFSET, NATIVE_FRAME_THIS_OFFSET, THREAD_OFFSET,
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
}

fn exit_reason(reason: DeoptReason) -> (ExitReason, ExitAction) {
    match reason {
        DeoptReason::WrongType => (ExitReason::TypeMismatch, ExitAction::Recompile),
        DeoptReason::WrongShape => (ExitReason::ShapeGuard, ExitAction::Recompile),
        DeoptReason::WrongValue => (ExitReason::IdentityGuard, ExitAction::Recompile),
        DeoptReason::Overflow => (ExitReason::Int32Overflow, ExitAction::Recompile),
        DeoptReason::MinusZero => (ExitReason::NegativeZero, ExitAction::Recompile),
        DeoptReason::OutOfBounds => (ExitReason::BoundsGuard, ExitAction::Recompile),
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
}

/// The safepoint every graph code object publishes: it roots all tagged
/// spill slots.
pub(crate) const SLOT_SAFEPOINT: abi::SafepointId = abi::NO_SAFEPOINT - 1;

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
) -> Result<Emission, Unsupported> {
    let mut ops = Assembler::new()
        .map_err(|_| Unsupported::Backend(crate::BackendFailure::AssemblerAllocation))?;
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
    let spill = crate::arm64::frame::SpillArea {
        bytes: slots.bytes(),
        tagged_slots: slots.tagged,
        safepoint: SLOT_SAFEPOINT,
    };
    let shape = crate::arm64::frame::EntryShape::of(
        view,
        code_object_id,
        abi::NativeFrameKind::Optimizing,
        true,
    )?;
    let mut codegen = Codegen {
        ops,
        relocations: RelocationCapture::new(capture_relocations),
        view,
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
        load_ic_cells: vec![crate::entry::PropertySourceCell::default(); plan.load_property_count]
            .into_boxed_slice(),
        next_load_ic: 0,
        store_ic_cells: vec![
            crate::entry::PropertySourceCell::default();
            plan.store_property_count
        ]
        .into_boxed_slice(),
        next_store_ic: 0,
        no_direct_call_events: None,
        no_code_map: None,
        node_offsets: Vec::new(),
        threw,
        committed_throw,
        propagate,
        materialize,
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
    let (call_entry, call_entry_cold) =
        crate::arm64::frame::emit_call_entry(&mut codegen.ops, view, shape, spill);
    let call_entry = call_entry.0;
    dynasm!(codegen.ops ; .arch aarch64 ; =>body);
    codegen.emit_body()?;
    while let Some(deferred) = codegen.deferred.pop() {
        deferred(&mut codegen);
    }
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
    crate::arm64::frame::emit_call_entry_cold(
        &mut codegen.ops,
        &mut codegen.relocations,
        transitions,
        activation,
        call_entry_cold,
    );
    let Codegen {
        ops,
        relocations,
        exits,
        load_ic_cells,
        store_ic_cells,
        node_offsets,
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
    fn emit_parallel_moves(&mut self, mut moves: Vec<Move>) {
        moves.retain(|m| m.from != m.to);
        // Repeatedly emit a move whose destination no pending move reads.
        while !moves.is_empty() {
            let ready = moves
                .iter()
                .position(|candidate| !moves.iter().any(|other| other.from == candidate.to));
            if let Some(index) = ready {
                let m = moves.remove(index);
                self.emit_move(m.from, m.to);
                continue;
            }
            // Every destination is still read: break a cycle through a
            // scratch register of the right class.
            let m = moves.remove(0);
            let scratch = match m.from {
                Location::Fp(_) => Location::Fp(31),
                _ => Location::Gp(16),
            };
            match (m.from, scratch) {
                (Location::Gp(_) | Location::Fp(_), _) => self.emit_move(m.from, scratch),
                (slot, Location::Gp(_)) => {
                    // A slot source in a cycle: hold it in x17 so x16 stays
                    // free for slot-to-slot moves.
                    self.emit_move(slot, Location::Gp(17));
                    for other in &mut moves {
                        if other.from == m.from {
                            other.from = Location::Gp(17);
                        }
                    }
                    moves.push(Move {
                        from: Location::Gp(17),
                        to: m.to,
                    });
                    continue;
                }
                _ => unreachable!("fp slot cycles go through d31"),
            }
            for other in &mut moves {
                if other.from == m.from {
                    other.from = scratch;
                }
            }
            moves.push(Move {
                from: scratch,
                to: m.to,
            });
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
        if let Some(site) = self
            .exits
            .iter()
            .find(|site| site.node == node && !site.lazy && site.reason == reason)
        {
            return site.label;
        }
        let label = self.ops.new_dynamic_label();
        self.exits.push(ExitSite {
            label,
            node,
            lazy: false,
            reason,
            action,
        });
        label
    }

    // ------------------------------------------------------------------
    // Body
    // ------------------------------------------------------------------

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
            Kind::TaggedEqual => {
                let (a, b) = (Self::gp(input(0)), Self::gp(input(1)));
                let destination = Self::gp(result.expect("a result"));
                dynasm!(self.ops ; .arch aarch64 ; cmp X(a), X(b));
                self.emit_cset_bool(destination, Condition::Equal, false);
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
                let (a, b) = (Self::gp(input(0)), Self::gp(input(1)));
                let exit = self.eager_exit(node, DeoptReason::OutOfBounds);
                dynasm!(self.ops ; .arch aarch64 ; cmp W(a), W(b) ; b.hs =>exit);
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
            Kind::Generic { pc, registers } => {
                let (pc, registers) = (*pc, registers.clone());
                self.emit_generic(node, pc, &registers)?;
            }
            other => {
                return Err(Unsupported::OperandShape(node_name(other)));
            }
        }
        Ok(())
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
                dynasm!(codegen.ops
                    ; .arch aarch64
                    ; mov x1, X(object)
                    ; mov x2, X(value)
                    ; ldr x0, [x20, THREAD_OFFSET]
                    ; ldr x0, [x0, VM_THREAD_GC_HEAP_OFFSET]
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
    fn emit_generic(
        &mut self,
        node: NodeId,
        pc: u32,
        registers: &[u16],
    ) -> Result<(), Unsupported> {
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
        // A throw the frame may catch rebuilds the interpreter frame first;
        // the interpreter then enters the handler.
        let (threw, committed_throw) = if self.in_exception_region(pc) {
            let index = self.exit_index(node, ExitReason::RuntimeTransition, ExitAction::Resume);
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
        };
        let exit = |codegen: &mut Self, reason: ExitReason, action: ExitAction| {
            codegen.typed_exit(node, reason, action)
        };
        let runtime_transition = exit(self, ExitReason::RuntimeTransition, ExitAction::Resume);
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
            returned: self.returned,
            committed_throw,
            threw,
            propagate_throw: self.propagate,
            fatal: self.fatal,
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
            let fatal = self.fatal;
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
                let true_label = self.labels[&if_true];
                let false_label = self.labels[&if_false];
                match kind {
                    BranchKind::Int32(condition) => {
                        let (a, b) = (
                            Self::gp(allocation.inputs[0]),
                            Self::gp(allocation.inputs[1]),
                        );
                        dynasm!(self.ops ; .arch aarch64 ; cmp W(a), W(b));
                        self.emit_branch_condition(condition, false, true_label);
                    }
                    BranchKind::Float64(condition) => {
                        let (a, b) = (
                            Self::fp(allocation.inputs[0]),
                            Self::fp(allocation.inputs[1]),
                        );
                        dynasm!(self.ops ; .arch aarch64 ; fcmp D(a), D(b));
                        self.emit_branch_condition(condition, true, true_label);
                    }
                    BranchKind::TaggedEqual => {
                        let (a, b) = (
                            Self::gp(allocation.inputs[0]),
                            Self::gp(allocation.inputs[1]),
                        );
                        dynasm!(self.ops ; .arch aarch64 ; cmp X(a), X(b) ; b.eq =>true_label);
                    }
                    BranchKind::Nullish => {
                        let a = Self::gp(allocation.inputs[0]);
                        dynasm!(self.ops
                            ; .arch aarch64
                            ; cmp XSP(a), VALUE_NULL as u32
                            ; b.eq =>true_label
                            ; cmp XSP(a), VALUE_UNDEFINED as u32
                            ; b.eq =>true_label
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
        dynasm!(self.ops
            ; .arch aarch64
            ; cmp XSP(value), VALUE_TRUE as u32
            ; b.eq =>if_true
            ; cmp XSP(value), VALUE_FALSE as u32
            ; b.eq =>if_false
            ; cmp XSP(value), VALUE_UNDEFINED as u32
            ; b.eq =>if_false
            ; cmp XSP(value), VALUE_NULL as u32
            ; b.eq =>if_false
        );
        self.load_immediate(16, NUMBER_TAG);
        dynasm!(self.ops
            ; .arch aarch64
            ; cmp X(value), x16
            ; b.lo =>not_int
            ; cmp WSP(value), #0
            ; b.eq =>if_false
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
                dynasm!(codegen.ops
                    ; .arch aarch64
                    ; cmp x16, VALUE_TRUE as u32
                    ; b.eq =>if_true
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
                for &(location, repr) in &live {
                    let slot = codegen.slots.snapshot_slot(location, repr);
                    codegen.emit_move(location, slot);
                }
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
        let deopt = self.deopt;
        for (index, site) in self.exits.clone().into_iter().enumerate() {
            dynasm!(self.ops
                ; .arch aarch64
                ; =>site.label
                ; movz w17, index as u32
                ; b =>deopt
            );
        }
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
            ; mov x0, x20
            ; mov w1, w17
        );
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
        dynasm!(self.ops
            ; .arch aarch64
            ; mov x0, x20
            ; mov w1, w17
        );
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

/// Lower one frame state to the VM's deopt slot recipe.
pub(crate) fn deopt_slots(
    graph: &Graph,
    slots: SlotLayout,
    state: &super::ir::FrameState,
    locations: &[Location],
) -> Box<[DeoptSlot]> {
    let mut recipe: Vec<DeoptSlot> = (0..state.register_count)
        .map(|_| DeoptSlot::physical(DeoptLocation::Literal(VALUE_UNDEFINED), DeoptRepr::Tagged))
        .collect();
    for (&(register, value), &location) in state.registers.iter().zip(locations) {
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
        recipe[usize::from(register)] = DeoptSlot::physical(deopt_location, repr);
    }
    recipe.into_boxed_slice()
}

fn node_name(kind: &Kind) -> &'static str {
    match kind {
        Kind::CallJs { .. } => "graph CallJs",
        Kind::Generic { .. } => "graph Generic",
        Kind::Float64Mod => "graph Float64Mod",
        Kind::ToBoolean | Kind::LogicalNot => "graph ToBoolean",
        _ => "graph node",
    }
}
