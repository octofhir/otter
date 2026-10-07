//! AArch64 code generation for the graph tier: every node on the locations
//! the allocator chose, slow paths and exits out of line.
//!
//! # Contents
//! - [`emit`] — the whole code object: the tier entry over a published
//!   interpreter frame, the JavaScript call entry, the body in layout order,
//!   deferred slow paths, deopt exits and the shared deopt handler.
//! - Sequential gap moves and target encoding of the shared parallel edge/exit
//!   assignments in [`super::moves`].
//! - Source-owned global binding reads with eager guards shared with Template.
//! - Node emitters for frame, arithmetic, conversion, check, memory, generic
//!   and control nodes.
//!
//! # Invariants
//! - Pinned registers: `x19` register window, `x20` context, `x21` published
//!   frame record, `x29` frame pointer. The only emitter scratch is `x16`,
//!   `x17` and `d31`; no node writes an allocatable register other than its
//!   result and declared temporaries.
//! - Spill slots live at `sp`: tagged homes first, then untagged homes. Each
//!   safepoint record roots exactly the tagged homes written for live values
//!   at its boundary ([`metadata::SitePlan`]), plus the exception scratch both
//!   entry forms zero before publishing the frame. No home is ever cleared.
//! - Memory spills are stored at definitions. Register-only values populate
//!   canonical homes in collecting slow paths or cold exits.
//! - Actual collecting calls expire tagged homes no live value or frame
//!   state can read, after live-register saves. The full tagged root region
//!   remains fixed; NoAlloc polls and leaf paths do not expire homes.
//! - Cold exits materialize their selected canonical recipe, expire homes
//!   outside it, then pass its index to writeback. Throw routing expires all
//!   allocator homes after catch writeback; the rooted window and reserved
//!   exception scratch own recovery and the exception payload.
//! - Exact finalized shapes prove ordinary lookup state. Dynamic shared-cache
//!   guards read the sole current ShapeState byte and permit provisional
//!   runtime feedback; stores reject prototype role before any effect.
//! - Conditional branches to deopt exits and slow paths use the ±1 MiB
//!   conditional form; nothing branches with `tbz`/`tbnz` to a far label.
//!
//! # See also
//! - [`super::regalloc`] — the locations consumed here.
//! - [`crate::arm64::activation`] — the call-entry and exit protocol shared
//!   with the baseline tier.

#![allow(clippy::useless_conversion)]

use crate::call_linkage::ForwardedBinding;
use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::jit::{JitBodyGuard, JitElementRepr, JitGuardWidth, JitHoleBitmap};
use otter_vm::native_abi::{self as abi, ExitAction, ExitReason, NativeResultStatus};
use otter_vm::value::tag;
use otter_vm::{JitCompileSnapshot, object::ShapeState};
use rustc_hash::FxHashMap;

use super::INSTANCEOF_CHAIN_BOUND;
use super::builder::Built;
use super::call::CommittedArgument;
use super::emission::{Emission, ExitSite};
use super::frame::{SlotLayout, exit_reason};
use super::ir::{BlockId, BranchKind, Condition, DeoptReason, Graph, Kind, NodeId, Repr};
use super::metadata::{self, FIRST_SITE_SAFEPOINT};
use super::regalloc::{Allocation, Location, Move};
use crate::Unsupported;
use crate::artifact::relocation::{RelocationCapture, RelocationTarget};
use crate::entry::{
    NATIVE_FRAME_SELF_OFFSET, NATIVE_FRAME_THIS_OFFSET, THREAD_OFFSET, VALUE_HOLE,
    VM_THREAD_BACKEDGE_FUEL_CELL_OFFSET, VM_THREAD_GC_HEAP_OFFSET, VM_THREAD_INTERRUPT_CELL_OFFSET,
    VM_THREAD_MARKING_FLAG_CELL_OFFSET,
};
use crate::template::arm64::values::{emit_load_symbol_u64, emit_load_u64};

mod allocation;
mod binding;
mod committed_call;
mod native_leaf;
mod own_fields;

const NUMBER_TAG: u64 = tag::NUMBER_TAG;
const NOT_CELL_MASK: u64 = tag::NOT_CELL_MASK;
const VALUE_TRUE: u64 = tag::VALUE_TRUE;
const VALUE_FALSE: u64 = tag::VALUE_FALSE;
const VALUE_UNDEFINED: u64 = tag::VALUE_UNDEFINED;
const VALUE_NULL: u64 = tag::VALUE_NULL;
const DOUBLE_OFFSET: u64 = tag::DOUBLE_ENCODE_OFFSET;
const DETACH_PROTECTOR: u32 = crate::entry::VM_THREAD_ARRAY_BUFFER_DETACH_PROTECTOR_CELL_OFFSET;
const ARRAY_INDEX_PROTECTOR: u32 = crate::entry::VM_THREAD_ARRAY_INDEX_PROTECTOR_CELL_OFFSET;

/// A slow path emitted after the body.
type Deferred<'a> = Box<dyn FnOnce(&mut Codegen<'a>) + 'a>;

struct Codegen<'a> {
    ops: Assembler,
    /// Literal-pool label of each float64 constant a move loads, emitted
    /// after the code.
    float_pool: FxHashMap<u64, DynamicLabel>,
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
    activation: crate::frame::ActivationExits,
    spill: crate::frame::SpillArea,
    transitions: &'a crate::entry::TransitionTable,
    deopt_runtime: u64,
    /// The baseline operations generic nodes run, one sequence per PC.
    plan: &'a crate::template::TemplatePlan,
    plan_index: FxHashMap<u32, Vec<usize>>,
    /// Shared property subroutines requested by Generic sites.
    shared_property: crate::template::arm64::shared_property::SharedPropertyProbes,
    no_direct_call_events: Option<crate::template::DirectCallEvents>,
    no_code_map: Option<crate::artifact::CodeMapCapture>,
    spliced_functions: std::collections::BTreeSet<u32>,
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
    /// Exact root records of the collecting boundaries emitted so far.
    sites: metadata::SitePlan,
    return_sites: Vec<abi::SafepointEntry>,
}

/// Body bytes between islands: a segment plus its island stays well inside
/// the ±1 MiB a conditional branch reaches.
const ISLAND_INTERVAL: usize = 256 * 1024;

/// The right operand of an int32 operation.
#[derive(Debug, Clone, Copy)]
enum Int32Operand {
    Register(u8),
    Constant(i32),
}

/// Whether `value` is an AArch64 32-bit logical immediate: a rotated run of
/// ones, repeated in an element of 2, 4, 8, 16 or 32 bits.
fn logical_immediate32(value: u32) -> bool {
    if value == 0 || value == u32::MAX {
        return false;
    }
    let mut size = 32u32;
    // The smallest element the value repeats.
    while size > 2 {
        let half = size / 2;
        let mask = (1u64 << half) - 1;
        if u64::from(value) & mask != (u64::from(value) >> half) & mask {
            break;
        }
        size = half;
    }
    let element = if size == 32 {
        value
    } else {
        value & ((1u32 << size) - 1)
    };
    (0..size).any(|rotation| {
        let rotated = if rotation == 0 {
            element
        } else {
            ((element >> rotation) | (element << (size - rotation)))
                & if size == 32 {
                    u32::MAX
                } else {
                    (1u32 << size) - 1
                }
        };
        rotated != 0 && rotated & rotated.wrapping_add(1) == 0
    })
}

/// An int32 word shifted arithmetically right by this is -1: its top 15
/// bits are all set, and no other word has them so.
const INT32_TAG_SHIFT: u32 = NUMBER_TAG.trailing_zeros();
const _: () = assert!(NUMBER_TAG == 0xfffe_0000_0000_0000);

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
    metadata::validate_generic_sources(&built.graph)?;
    slots.validate(view.code_block.register_count)?;
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
    let activation = crate::frame::ActivationExits {
        construct: ops.new_dynamic_label(),
        side_exit: ops.new_dynamic_label(),
    };
    let threw = ops.new_dynamic_label();
    let committed_throw = ops.new_dynamic_label();
    let propagate = ops.new_dynamic_label();
    let materialize = ops.new_dynamic_label();
    // Both entries zero the exception scratch, the only tagged word the
    // entry record roots; every other home is rooted only once written.
    let spill = crate::frame::SpillArea {
        bytes: slots.bytes(),
        scratch_slot: Some(slots.spill_tagged),
        safepoint: FIRST_SITE_SAFEPOINT,
        saved_pairs: crate::frame::SpillArea::saved_pairs_for(allocation.used_gp()),
    };
    // A body that never runs a baseline operation on the window leaves it
    // to the exits that rebuild the interpreter frame.
    let lazy_window = !built
        .graph
        .nodes
        .iter()
        .any(|node| matches!(node.kind, Kind::Generic { .. } | Kind::LoadWindow(_)));
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
        plan_index: metadata::operation_index(plan),
        shared_property: Default::default(),
        no_direct_call_events: None,
        no_code_map: None,
        spliced_functions: std::collections::BTreeSet::new(),
        sites: metadata::SitePlan::new(slots.spill_tagged),
        return_sites: Vec::new(),
        node_offsets: Vec::new(),
        threw,
        committed_throw,
        propagate,
        materialize,
        far,
        island_base: 0,
        exits_flushed: 0,
        float_pool: FxHashMap::default(),
        veneers: Vec::new(),
    };
    let tier_entry = codegen.ops.offset().0;
    let body = codegen.ops.new_dynamic_label();
    crate::arm64::frame::emit_tier_prologue(&mut codegen.ops, spill);
    let mut osr_dispatch_end = None;
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
        osr_dispatch_end = Some(codegen.ops.offset().0);
    } else {
        dynasm!(codegen.ops ; .arch aarch64 ; b =>body);
    }
    let call_entry_cold = crate::frame::CallEntryCold::new(&mut codegen.ops, shape);
    crate::arm64::frame::emit_call_entry_cold(
        &mut codegen.ops,
        &mut codegen.relocations,
        transitions,
        view,
        activation,
        shape,
        call_entry_cold,
    );
    let call_entry = crate::arm64::frame::emit_call_entry(
        &mut codegen.ops,
        &mut codegen.relocations,
        view,
        shape,
        spill,
        call_entry_cold,
    )
    .0;
    dynasm!(codegen.ops ; .arch aarch64 ; =>body);
    codegen.emit_body()?;
    let body_end = codegen.ops.offset().0;
    // Near code returns by falling out of its last block, into the return
    // path and the shared exits its epilogue branches to (`tbnz` reaches
    // 32 KiB); a far body keeps them all after its islands.
    let emit_shared_exits = |codegen: &mut Codegen<'_>| {
        crate::arm64::frame::emit_exits(
            &mut codegen.ops,
            &mut codegen.relocations,
            transitions,
            view,
            shape.derived,
            activation,
            spill,
        );
    };
    if !codegen.far {
        codegen.emit_return_path();
        emit_shared_exits(&mut codegen);
    }
    codegen.flush_segment();
    codegen.emit_exit_stubs();
    if codegen.far {
        emit_shared_exits(&mut codegen);
    }
    let shared_property = std::mem::take(&mut codegen.shared_property);
    shared_property.emit(
        &mut codegen.ops,
        &mut codegen.relocations,
        transitions,
        view,
    );
    codegen.emit_float_pool();
    codegen
        .return_sites
        .sort_by_key(|site| site.native_return_offset);
    let Codegen {
        ops,
        relocations,
        exits,
        node_offsets,
        sites,
        return_sites,
        spliced_functions,
        ..
    } = codegen;
    let buffer = crate::entry::finalize_assembler(ops)?;
    Ok(Emission {
        buffer,
        tier_entry,
        call_entry,
        exits,
        relocations,
        node_offsets,
        body_end,
        osr_dispatch_end,
        site_records: sites.into_records(),
        return_sites,
        spliced_functions,
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

    /// The float64 literal pool, after every instruction of the function.
    fn emit_float_pool(&mut self) {
        if self.float_pool.is_empty() {
            return;
        }
        let mut pool: Vec<(u64, DynamicLabel)> = self.float_pool.drain().collect();
        pool.sort_unstable_by_key(|&(bits, _)| bits);
        dynasm!(self.ops ; .arch aarch64 ; .align 8);
        for (bits, label) in pool {
            dynasm!(self.ops ; .arch aarch64 ; =>label ; .u64 bits);
        }
    }

    /// Materialize `bits` into general register `register`.
    fn load_immediate(&mut self, register: u8, bits: u64) {
        emit_load_u64(&mut self.ops, register, bits);
    }

    /// The int32 constant at `location`, if it is one.
    fn int32_constant(&self, location: Location) -> Option<i32> {
        match location {
            Location::Constant(node) => match self.graph.node(node).kind {
                Kind::ConstInt32(value) => Some(value),
                _ => None,
            },
            _ => None,
        }
    }

    /// The right int32 operand at `location`: a constant stays one; a slot
    /// is read into `x17`.
    fn int32_operand(&mut self, location: Location) -> Int32Operand {
        match location {
            Location::Gp(register) => Int32Operand::Register(register),
            Location::Constant(node) => match self.graph.node(node).kind {
                Kind::ConstInt32(value) => Int32Operand::Constant(value),
                _ => {
                    self.emit_move(location, Location::Gp(17));
                    Int32Operand::Register(17)
                }
            },
            _ => {
                self.emit_move(location, Location::Gp(17));
                Int32Operand::Register(17)
            }
        }
    }

    /// The register holding `operand`, a constant materialized in `x17`.
    fn int32_register(&mut self, operand: Int32Operand) -> u8 {
        match operand {
            Int32Operand::Register(register) => register,
            Int32Operand::Constant(value) => {
                self.load_word32(17, value as u32);
                17
            }
        }
    }

    /// `cmp W(a), operand`, an immediate when it fits.
    fn emit_int32_compare(&mut self, a: u8, operand: Int32Operand) {
        match operand {
            Int32Operand::Constant(value) if (0..=4095).contains(&value) => {
                dynasm!(self.ops ; .arch aarch64 ; cmp WSP(a), value as u32);
            }
            Int32Operand::Constant(value) if (-4095..0).contains(&value) => {
                dynasm!(self.ops ; .arch aarch64 ; cmn WSP(a), (-value) as u32);
            }
            other => {
                let b = self.int32_register(other);
                dynasm!(self.ops ; .arch aarch64 ; cmp W(a), W(b));
            }
        }
    }

    /// `W(register)` = `value`, in one instruction when the value or its
    /// complement fits sixteen bits.
    fn load_word32(&mut self, register: u8, value: u32) {
        if value <= 0xffff {
            dynasm!(self.ops ; .arch aarch64 ; movz W(register), value);
        } else if !value <= 0xffff {
            dynasm!(self.ops ; .arch aarch64 ; movn W(register), !value);
        } else {
            dynasm!(self.ops
                ; .arch aarch64
                ; movz W(register), value & 0xffff
                ; movk W(register), value >> 16, lsl #16
            );
        }
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
                } else if let Some(value) = fmov_immediate(bits) {
                    dynasm!(self.ops ; .arch aarch64 ; fmov D(b), value as f32);
                } else {
                    // One PC-relative load from the pool after the code.
                    let label = *self
                        .float_pool
                        .entry(bits)
                        .or_insert_with(|| self.ops.new_dynamic_label());
                    dynasm!(self.ops ; .arch aarch64 ; ldr D(b), =>label);
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

    /// Emit the shared parallel assignment on this target's value homes.
    fn emit_parallel_moves(&mut self, moves: Vec<Move>) {
        super::moves::emit(self, moves);
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
    ///
    /// An island is a jump over veneers, exit stubs and deferred slow paths:
    /// it reads and writes no register or flag, so any instruction boundary
    /// outside a parked-stack window may hold one. Block starts and edge moves
    /// check too, so a run of phi-only blocks cannot outgrow conditional reach.
    fn maybe_island(&mut self) {
        if !self.far
            || self.sp_delta != 0
            || self.ops.offset().0 - self.island_base < ISLAND_INTERVAL
        {
            return;
        }
        let skip = self.ops.new_dynamic_label();
        dynasm!(self.ops ; .arch aarch64 ; b =>skip);
        self.flush_segment();
        dynasm!(self.ops ; .arch aarch64 ; =>skip);
        self.island_base = self.ops.offset().0;
    }

    /// The slow paths, exit stubs and veneers the segment so far named.
    ///
    /// Veneers come first: they are the only targets of the segment's
    /// conditional branches, so they must sit nearest to them however much
    /// slow-path and exit code follows. Deferred code's own veneers follow it.
    fn flush_segment(&mut self) {
        self.flush_veneers();
        while let Some(deferred) = self.deferred.pop() {
            deferred(self);
        }
        self.flush_veneers();
        let deopt = self.deopt;
        for index in self.exits_flushed..self.exits.len() {
            let site = self.exits[index].clone();
            dynasm!(self.ops ; .arch aarch64 ; =>site.label);
            let allocation = self.allocation.node(site.node);
            let spills = if site.lazy {
                &allocation.lazy_spills
            } else {
                &allocation.eager_spills
            };
            self.emit_parallel_moves(spills.clone());
            dynasm!(self.ops ; .arch aarch64 ; movz w17, index as u32 ; b =>deopt);
        }
        self.exits_flushed = self.exits.len();
        self.flush_veneers();
    }

    fn flush_veneers(&mut self) {
        for (target, veneer) in std::mem::take(&mut self.veneers) {
            dynasm!(self.ops ; .arch aarch64 ; =>veneer ; b =>target);
        }
    }

    fn emit_body(&mut self) -> Result<(), Unsupported> {
        // A block that only jumps on emits nothing: its label names the
        // block it forwards to.
        let entry = self.layout.first().copied();
        let mut forwarders: FxHashMap<BlockId, Vec<BlockId>> = FxHashMap::default();
        let mut order = Vec::with_capacity(self.layout.len());
        for &block in self.layout {
            let target = self.forwarded(block);
            if !self.far
                && target != block
                && Some(block) != entry
                && !self.graph.block(block).is_loop
            {
                forwarders.entry(target).or_default().push(block);
            } else {
                order.push(block);
            }
        }
        for (position, &block) in order.iter().enumerate() {
            self.maybe_island();
            let label = self.labels[&block];
            dynasm!(self.ops ; .arch aarch64 ; =>label);
            for forwarder in forwarders.remove(&block).unwrap_or_default() {
                let label = self.labels[&forwarder];
                dynasm!(self.ops ; .arch aarch64 ; =>label);
            }
            let data = self.graph.block(block);
            for &phi in &data.phis {
                let allocation = self.allocation.node(phi);
                if allocation.skipped {
                    continue;
                }
                if let (Some(result), Some(&slot)) =
                    (allocation.result, self.allocation.spill.get(&phi))
                    && result != slot
                    && self.allocation.definition_spills.contains(&phi)
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
                    && self.allocation.definition_spills.contains(&node)
                {
                    self.emit_move(result, slot);
                }
            }
            let control = data.control.expect("a terminated block");
            self.node_offsets.push((self.ops.offset().0, control));
            for m in self.allocation.node(control).moves.clone() {
                self.emit_move(m.from, m.to);
            }
            let next = order.get(position + 1).copied();
            self.emit_control(block, control, next)?;
        }
        debug_assert!(forwarders.is_empty(), "every forwarding target is emitted");
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
            Kind::LoadThis => {
                let destination = Self::gp(result.expect("a result"));
                dynasm!(self.ops ; .arch aarch64 ; ldr X(destination), [x21, NATIVE_FRAME_THIS_OFFSET]);
            }
            Kind::LoadNewTarget => {
                let destination = Self::gp(result.expect("new.target result"));
                dynasm!(self.ops ; .arch aarch64 ; ldr X(destination), [x21, crate::entry::NATIVE_FRAME_NEW_TARGET_OFFSET]);
            }
            Kind::LoadClosure => {
                let destination = Self::gp(result.expect("a result"));
                dynasm!(self.ops ; .arch aarch64 ; ldr X(destination), [x21, NATIVE_FRAME_SELF_OFFSET]);
            }
            Kind::LoadGlobalBinding(byte_pc) => {
                let destination = Self::gp(result.expect("binding read result"));
                let temps = [allocation.gp_temps[0], allocation.gp_temps[1]];
                self.emit_global_binding(node, *byte_pc, destination, temps)?;
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
            Kind::Int32AddWrapping | Kind::Int32SubWrapping => {
                let a = Self::gp(input(0));
                let operand = self.int32_operand(input(1));
                let destination = Self::gp(result.expect("a result"));
                let add = data.kind == Kind::Int32AddWrapping;
                match operand {
                    Int32Operand::Constant(value) if (-4095..=4095).contains(&value) => {
                        let (add, magnitude) = if value < 0 {
                            (!add, (-value) as u32)
                        } else {
                            (add, value as u32)
                        };
                        if add {
                            dynasm!(self.ops ; .arch aarch64 ; add WSP(destination), WSP(a), magnitude);
                        } else {
                            dynasm!(self.ops ; .arch aarch64 ; sub WSP(destination), WSP(a), magnitude);
                        }
                    }
                    other => {
                        let b = self.int32_register(other);
                        if add {
                            dynasm!(self.ops ; .arch aarch64 ; add W(destination), W(a), W(b));
                        } else {
                            dynasm!(self.ops ; .arch aarch64 ; sub W(destination), W(a), W(b));
                        }
                    }
                }
            }
            Kind::Int32Add | Kind::Int32Sub => {
                let a = Self::gp(input(0));
                let operand = self.int32_operand(input(1));
                let destination = Self::gp(result.expect("a result"));
                let exit = self.eager_exit(node, DeoptReason::Overflow);
                // The exit reads the inputs: the result goes straight to its
                // register only when that is neither of them.
                let direct = destination != a
                    && !matches!(operand, Int32Operand::Register(b) if b == destination);
                let target = if direct { destination } else { 16 };
                let add = data.kind == Kind::Int32Add;
                match operand {
                    Int32Operand::Constant(value) if (-4095..=4095).contains(&value) => {
                        // Adding a negative constant subtracts its magnitude.
                        let (add, magnitude) = if value < 0 {
                            (!add, (-value) as u32)
                        } else {
                            (add, value as u32)
                        };
                        if add {
                            dynasm!(self.ops ; .arch aarch64 ; adds W(target), WSP(a), magnitude);
                        } else {
                            dynasm!(self.ops ; .arch aarch64 ; subs W(target), WSP(a), magnitude);
                        }
                    }
                    other => {
                        let b = self.int32_register(other);
                        if add {
                            dynasm!(self.ops ; .arch aarch64 ; adds W(target), W(a), W(b));
                        } else {
                            dynasm!(self.ops ; .arch aarch64 ; subs W(target), W(a), W(b));
                        }
                    }
                }
                dynasm!(self.ops ; .arch aarch64 ; b.vs =>exit);
                if !direct {
                    dynasm!(self.ops ; .arch aarch64 ; mov W(destination), w16);
                }
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
            Kind::Int32ShiftLeft | Kind::Int32ShiftRight
                if self.int32_constant(input(1)).is_some() =>
            {
                // A constant count is taken modulo 32, as the operator does.
                let a = Self::gp(input(0));
                let destination = Self::gp(result.expect("a result"));
                let count = self.int32_constant(input(1)).expect("a constant count");
                let count = (count & 31) as u32;
                if data.kind == Kind::Int32ShiftLeft {
                    dynasm!(self.ops ; .arch aarch64 ; lsl W(destination), W(a), count);
                } else {
                    dynasm!(self.ops ; .arch aarch64 ; asr W(destination), W(a), count);
                }
            }
            Kind::Int32BitAnd | Kind::Int32BitOr | Kind::Int32BitXor
                if self
                    .int32_constant(input(1))
                    .is_some_and(|value| logical_immediate32(value as u32)) =>
            {
                let a = Self::gp(input(0));
                let destination = Self::gp(result.expect("a result"));
                let value = self.int32_constant(input(1)).expect("a constant operand") as u32;
                match data.kind {
                    Kind::Int32BitAnd => {
                        dynasm!(self.ops ; .arch aarch64 ; and WSP(destination), W(a), value)
                    }
                    Kind::Int32BitOr => {
                        dynasm!(self.ops ; .arch aarch64 ; orr WSP(destination), W(a), value)
                    }
                    _ => dynasm!(self.ops ; .arch aarch64 ; eor WSP(destination), W(a), value),
                }
            }
            Kind::Int32BitAnd
            | Kind::Int32BitOr
            | Kind::Int32BitXor
            | Kind::Int32ShiftLeft
            | Kind::Int32ShiftRight => {
                let a = Self::gp(input(0));
                let operand = self.int32_operand(input(1));
                let b = self.int32_register(operand);
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
                let a = Self::gp(input(0));
                let operand = self.int32_operand(input(1));
                let destination = Self::gp(result.expect("a result"));
                match operand {
                    // A count past zero clears the sign bit: no check.
                    Int32Operand::Constant(count) if count & 31 != 0 => {
                        let count = (count & 31) as u32;
                        dynasm!(self.ops ; .arch aarch64 ; lsr W(destination), W(a), count);
                    }
                    other => {
                        let b = self.int32_register(other);
                        let exit = self.eager_exit(node, DeoptReason::LostPrecision);
                        dynasm!(self.ops
                            ; .arch aarch64
                            ; lsr w16, W(a), W(b)
                            ; cmp w16, #0
                            ; b.lt =>exit
                            ; mov W(destination), w16
                        );
                    }
                }
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
                let a = Self::gp(input(0));
                let operand = self.int32_operand(input(1));
                let destination = Self::gp(result.expect("a result"));
                let condition = *condition;
                self.emit_int32_compare(a, operand);
                self.emit_cset_bool(destination, condition, false);
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
                // An int32 word has the top 15 bits all set.
                dynasm!(self.ops
                    ; .arch aarch64
                    ; asr x16, X(a), INT32_TAG_SHIFT
                    ; cmn x16, 1
                    ; b.ne =>exit
                    ; mov W(destination), W(a)
                );
            }
            Kind::CheckedTaggedToFloat64 => {
                let a = Self::gp(input(0));
                let destination = Self::fp(result.expect("a result"));
                let exit = self.eager_exit(node, DeoptReason::WrongType);
                self.emit_tagged_to_float(a, destination, exit);
            }
            Kind::Int32ToTagged => {
                let a = Self::gp(input(0));
                let destination = Self::gp(result.expect("a result"));
                dynasm!(self.ops
                    ; .arch aarch64
                    ; mov W(destination), W(a)
                    ; orr XSP(destination), X(destination), NUMBER_TAG
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
                dynasm!(self.ops
                    ; .arch aarch64
                    ; asr x16, X(a), INT32_TAG_SHIFT
                    ; cmn x16, 1
                    ; b.eq =>int
                    ; tst X(a), NUMBER_TAG
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
            Kind::LoadOwnField(field) => {
                let object = Self::gp(input(0));
                let destination = Self::gp(result.expect("a result"));
                own_fields::emit(
                    &mut self.ops,
                    self.view.field_layout,
                    object,
                    destination,
                    *field,
                    false,
                );
            }
            Kind::StoreOwnField(field) => {
                let (object, value) = (Self::gp(input(0)), Self::gp(input(1)));
                own_fields::emit(
                    &mut self.ops,
                    self.view.field_layout,
                    object,
                    value,
                    *field,
                    true,
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
            Kind::LoadClosureContext => {
                // A running function is a closure, or a function-id
                // immediate that closes over no context.
                let closure = Self::gp(input(0));
                let destination = Self::gp(result.expect("a result"));
                let context = self.view.closure_call_layout.context_byte;
                let bare = self.ops.new_dynamic_label();
                let done = self.ops.new_dynamic_label();
                dynasm!(self.ops
                    ; .arch aarch64
                    ; and w16, W(closure), 0xffff
                    ; cmp w16, tag::FUNCTION_ID_TAG as u32
                    ; b.eq =>bare
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
            Kind::CallForward { plan, bindings, .. } => {
                let (plan, bindings) = (*plan, bindings.clone());
                let temps = [allocation.gp_temps[0], allocation.gp_temps[1]];
                self.emit_call_forward(node, plan, &bindings, temps)?;
            }
            Kind::NativeLeaf(stub) => self.emit_native_leaf(node, *stub)?,
            Kind::CheckNative(native_ref) => {
                let value = Self::gp(input(0));
                let native_ref = *native_ref;
                let exit = self.eager_exit(node, DeoptReason::WrongValue);
                self.emit_native_identity(value, native_ref, exit);
            }
            Kind::CheckArgumentsElided => {
                let exit = self.eager_exit(node, DeoptReason::WrongValue);
                dynasm!(self.ops
                    ; .arch aarch64
                    ; ldr w16, [x21, abi::NATIVE_FRAME_ARGUMENTS_OBJECT_OFFSET]
                    ; cbnz w16, =>exit
                );
            }
            Kind::CheckFunction { function_id, cell } => {
                let value = Self::gp(input(0));
                let (function_id, cell) = (*function_id, *cell);
                self.emit_check_function(node, function_id, cell, value);
            }
            Kind::AllocationGroup(index) => self.emit_allocation_group(node, *index)?,
            Kind::AllocationProjection(byte) => self.emit_allocation_projection(node, *byte),
            Kind::PrimitiveAdd => self.emit_primitive_add(node)?,
            Kind::PrimitiveCompare(condition) => self.emit_primitive_compare(node, *condition)?,
            Kind::NewObject | Kind::NewArrayEmpty => {
                let destination = Self::gp(result.expect("an allocation result"));
                self.emit_empty_allocation(node, destination)?;
            }
            Kind::NewObjectLiteral | Kind::NewArrayLiteral => {
                let destination = Self::gp(result.expect("an allocation result"));
                self.emit_literal_allocation(node, destination)?;
            }
            Kind::NativeNewContext(_) | Kind::CopyContext | Kind::NewClosure => {
                let destination = Self::gp(result.expect("lexical allocation result"));
                self.emit_lexical_allocation(node, destination)?;
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
                self.emit_instanceof(node, [value, target], temps, destination)?;
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
            Kind::BooleanToInt32 => {
                // `true` and `false` differ in the low bit alone.
                let value = Self::gp(input(0));
                let destination = Self::gp(result.expect("a result"));
                dynasm!(self.ops ; .arch aarch64 ; and WSP(destination), W(value), 1);
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

    fn emit_tagged_to_float(&mut self, a: u8, destination: u8, exit: DynamicLabel) {
        let int = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        dynasm!(self.ops
            ; .arch aarch64
            ; asr x16, X(a), INT32_TAG_SHIFT
            ; cmn x16, 1
            ; b.eq =>int
            ; tst X(a), NUMBER_TAG
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

    /// Prove an eligible baked shape. Shape identity fixes descriptor and
    /// lookup state; writable paths additionally reject prototype role because
    /// their stores do not enter the semantic invalidation owner.
    fn emit_check_shapes(
        &mut self,
        object: u8,
        shapes: &[u32],
        writable: bool,
        exit: DynamicLabel,
    ) {
        let matched = self.ops.new_dynamic_label();
        self.emit_object_receiver(object, exit);
        let shape_byte = self.view.object_shape_byte;
        dynasm!(self.ops ; .arch aarch64 ; ldr w16, [X(object), shape_byte]);
        for (index, &shape) in shapes.iter().enumerate() {
            self.load_immediate(17, u64::from(shape));
            dynasm!(self.ops ; .arch aarch64 ; cmp w16, w17);
            if index + 1 == shapes.len() {
                dynasm!(self.ops ; .arch aarch64 ; b.ne =>exit);
            } else {
                dynasm!(self.ops ; .arch aarch64 ; b.eq =>matched);
            }
        }
        if shapes.is_empty() {
            dynasm!(self.ops ; .arch aarch64 ; b =>exit);
        }
        dynasm!(self.ops ; .arch aarch64 ; =>matched);
        if writable {
            self.emit_shape_state(object);
            dynasm!(self.ops ; .arch aarch64
                ; tst w16, u32::from(ShapeState::PROTOTYPE_MASK)
                ; b.ne =>exit);
        }
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
        dynasm!(self.ops
            ; .arch aarch64
            ; and x16, X(value), #0xffff_ffff_0000_0000
            ; add X(temp), X(temp), x16
            ; ldr w16, [X(temp), view.object_shape_byte]
        );
        self.load_immediate(17, u64::from(lookup.holder_shape));
        dynasm!(self.ops ; .arch aarch64 ; cmp w16, w17 ; b.ne =>exit);
        self.emit_field_storage(temp, 16, lookup.call_field);
        let byte = lookup.call_field.byte_offset();
        if byte <= 32760 {
            dynasm!(self.ops ; .arch aarch64 ; ldr X(temp), [x16, byte]);
        } else {
            self.load_immediate(17, u64::from(byte));
            dynasm!(self.ops ; .arch aarch64 ; ldr X(temp), [x16, x17]);
        }
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
        let pc = self.graph.node(node).pc;
        self.emit_function_identity(function_id, cell, pc, value, exit);
    }

    /// Branch to `miss` unless `X(value)` is the function `function_id`:
    /// its function-id immediate, or a closure of it that needs no runtime
    /// setup. `cell`, when not zero, holds the last value proved here, and
    /// a new proof is stored there. Clobbers only `x16` and `x17`.
    fn emit_function_identity(
        &mut self,
        function_id: u32,
        cell: u64,
        call_pc: u32,
        value: u8,
        exit: DynamicLabel,
    ) {
        let proved = self.ops.new_dynamic_label();
        let layout = self.view.closure_call_layout;
        let cell_target = RelocationTarget::CalleeIdentityCell {
            function_id,
            call_pc,
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
            self.emit_object_receiver(receiver, exit);
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
        let value_byte = guard.method_field.byte_offset();
        self.emit_field_storage(holder, 16, guard.method_field);
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
        // Each successful ordinary program proves its exact finalized shape
        // before its field/effect. Prototype-role writes require the canonical
        // invalidation owner and cannot commit this direct store.
        self.emit_object_receiver(receiver, exit);
        self.emit_shape_state(receiver);
        dynasm!(self.ops ; .arch aarch64
            ; tst w16, u32::from(ShapeState::PROTOTYPE_MASK)
            ; b.ne =>exit);
        for program in programs.iter() {
            let next = self.ops.new_dynamic_label();
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
                        // The central baker validated this immutable shape's
                        // ordinary finalized state before publishing the program.
                        dynasm!(self.ops
                            ; .arch aarch64
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
                        // The exact shape proves descriptor authority and
                        // the immutable atom/slot mapping.
                    }
                    Op::GuardExtensible { field, .. } => {
                        // The appended slot fits the storage the receiver
                        // already has, and the receiver may still grow.
                        if !field.is_inline() {
                            dynasm!(self.ops ; .arch aarch64
                                ; ldr w16, [X(receiver), view.field_layout.slab_handle_byte]
                                ; cbz w16, =>next
                                ; and x17, X(receiver), #0xffff_ffff_0000_0000
                                ; add x16, x17, x16
                                ; ldr w16, [x16, view.field_layout.slab_capacity_byte]);
                            self.load_immediate(17, u64::from(field.index()));
                            dynasm!(self.ops ; .arch aarch64 ; cmp w17, w16 ; b.hs =>next);
                        }
                        self.emit_shape_state(receiver);
                        dynasm!(self.ops ; .arch aarch64
                            ; tst w16, u32::from(ShapeState::EXTENSIBLE_MASK)
                            ; b.eq =>next);
                    }
                    Op::StoreField { field, .. } => {
                        let value_byte = field.byte_offset();
                        // Every guard has passed: the slot base, then the
                        // value word.
                        self.emit_field_storage(receiver, scratch, field);
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

    /// Prove a published ordinary object cell; exact static shape guards
    /// supply its lookup-state proof before any field/effect.
    fn emit_object_receiver(&mut self, receiver: u8, miss: DynamicLabel) {
        self.load_immediate(16, NOT_CELL_MASK);
        dynasm!(self.ops ; .arch aarch64
            ; tst X(receiver), x16 ; b.ne =>miss
            ; cbz X(receiver), =>miss
            ; ldrb w16, [X(receiver)]
            ; cmp w16, crate::entry::OBJECT_BODY_TYPE_TAG ; b.ne =>miss);
    }

    /// Current immutable ShapeState in w16, clobbering x16/x17 only.
    /// Published objects own non-null shapes; the receiver stays unchanged.
    fn emit_shape_state(&mut self, receiver: u8) {
        let view = self.view;
        dynasm!(self.ops ; .arch aarch64
            ; and x17, X(receiver), #0xffff_ffff_0000_0000
            ; ldr w16, [X(receiver), view.object_shape_byte]
            ; add x16, x17, x16
            ; ldrb w16, [x16, view.shape_state_byte]);
    }

    /// Dynamic ordinary lookup, including provisional runtime feedback.
    /// Shared tables select live shape identity; they never bake that lineage.
    fn emit_ordinary_receiver(&mut self, receiver: u8, extra_state: u8, miss: DynamicLabel) {
        self.emit_object_receiver(receiver, miss);
        self.emit_shape_state(receiver);
        let mask = ShapeState::DICTIONARY_MASK | ShapeState::OPAQUE_LOOKUP_MASK | extra_state;
        // Composite state masks are not necessarily logical immediates.
        self.load_immediate(17, u64::from(mask));
        dynasm!(self.ops ; .arch aarch64 ; tst w16, w17 ; b.ne =>miss);
    }

    /// Materialize the immutable shape-selected bank of a known field.
    fn emit_field_storage(&mut self, holder: u8, base: u8, field: otter_vm::object::FieldLocation) {
        let layout = self.view.field_layout;
        if field.is_inline() {
            dynasm!(self.ops ; .arch aarch64 ; add XSP(base), XSP(holder), layout.inline_values_byte);
        } else {
            dynasm!(self.ops ; .arch aarch64
                ; ldr w17, [X(holder), layout.slab_handle_byte]
                ; and x16, X(holder), #0xffff_ffff_0000_0000
                ; add x17, x16, x17
                ; add XSP(base), x17, layout.slab_words_byte);
        }
    }

    /// A named load probes the one live action table, then completes its
    /// actual source once through the committed runtime owner on a miss.
    fn emit_load_property_cached(
        &mut self,
        node: NodeId,
        pc: u32,
        atom: Option<u32>,
        receiver: u8,
        temps: [u8; 4],
        destination: u8,
    ) -> Result<(), Unsupported> {
        let slow = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        let table = self.ops.new_dynamic_label();
        // V8's generic LoadIC: the site's own handlers first, then the
        // isolate's shared table, then the committed miss.
        if let Some((slot, function_id, byte_pc)) = self.property_ic_slot(node, pc) {
            let view = metadata::source_view(self.view, self.inline_views, self.graph, node);
            emit_load_symbol_u64(
                &mut self.ops,
                &mut self.relocations,
                temps[0],
                slot,
                RelocationTarget::PropertyIcSlot {
                    function_id,
                    byte_pc,
                },
            );
            crate::arm64::property_ic::emit_slot_load(
                &mut self.ops,
                view,
                receiver,
                temps[0],
                [temps[1], temps[2], temps[3]],
                destination,
                table,
                table,
                done,
            );
        }
        dynasm!(self.ops ; .arch aarch64 ; =>table);
        if let Some(atom) = atom {
            let view = metadata::source_view(self.view, self.inline_views, self.graph, node);
            crate::arm64::property_actions::emit_load(
                &mut self.ops,
                &mut self.relocations,
                view,
                crate::arm64::property_actions::AtomOperand::Immediate(atom),
                receiver,
                temps,
                destination,
                slow,
            );
            dynasm!(self.ops ; .arch aarch64 ; b =>done);
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

    /// Store actions are independent of the read fact under the shared key.
    /// The probe commits only after all guards; its optional child edge and
    /// the following WriteBarrier node retain the existing barrier owners.
    fn emit_store_property_cached(
        &mut self,
        node: NodeId,
        pc: u32,
        atom: Option<u32>,
        [receiver, value]: [u8; 2],
        temps @ [holder, child_shape, entry, index]: [u8; 4],
    ) -> Result<(), Unsupported> {
        let slow = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        let table = self.ops.new_dynamic_label();
        let stored = self.ops.new_dynamic_label();
        // V8's generic StoreIC: the site's own handlers, then the shared
        // table; both leave the compressed child (zero for an overwrite).
        let slot = self.property_ic_slot(node, pc);
        if let Some((slot, function_id, byte_pc)) = slot {
            let view = metadata::source_view(self.view, self.inline_views, self.graph, node);
            emit_load_symbol_u64(
                &mut self.ops,
                &mut self.relocations,
                holder,
                slot,
                RelocationTarget::PropertyIcSlot {
                    function_id,
                    byte_pc,
                },
            );
            crate::arm64::property_ic::emit_slot_store(
                &mut self.ops,
                view,
                receiver,
                value,
                holder,
                [entry, index, child_shape],
                table,
                table,
                stored,
            );
        }
        dynasm!(self.ops ; .arch aarch64 ; =>table);
        if let Some(atom) = atom {
            let view = metadata::source_view(self.view, self.inline_views, self.graph, node);
            crate::arm64::property_actions::emit_store(
                &mut self.ops,
                &mut self.relocations,
                view,
                crate::arm64::property_actions::AtomOperand::Immediate(atom),
                [receiver, value],
                temps,
                slow,
            );
        } else {
            dynasm!(self.ops ; .arch aarch64 ; b =>slow);
        }
        if atom.is_some() || slot.is_some() {
            let overwritten = self.ops.new_dynamic_label();
            dynasm!(self.ops ; .arch aarch64 ; =>stored ; cbz W(child_shape), =>overwritten);
            self.emit_dynamic_shape_child_barrier(node, receiver, child_shape, holder);
            dynasm!(self.ops ; .arch aarch64 ; =>overwritten ; b =>done);
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

    /// The native IC slot of the property site at `pc` of the body `node`
    /// belongs to: `(address, function id, byte pc)`. A site that was already
    /// megamorphic at compile time has none: that state is terminal, so its
    /// handlers stay empty and the shared table serves every receiver.
    fn property_ic_slot(&self, node: NodeId, pc: u32) -> Option<(u64, u32, u32)> {
        let view = self.view_of(node);
        let byte_pc = view.instructions.get(pc as usize)?.byte_pc;
        let access = view.property_accesses.get(&byte_pc)?;
        (access.ic_slot != 0 && !access.shared && view.cage_base != 0).then_some((
            access.ic_slot,
            view.code_block.id,
            byte_pc,
        ))
    }

    /// The full property operation at `pc` in the runtime, with every live
    /// register preserved through its canonical home around the call: the load
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
        let view = self.view_of(node);
        let function_id = view.code_block.id;
        let byte_pc = view
            .instructions
            .get(pc as usize)
            .map(|instruction| instruction.byte_pc)
            .ok_or(Unsupported::OperandShape("graph property site pc"))?;
        let slot = view
            .property_accesses
            .get(&byte_pc)
            .map(|access| access.ic_slot)
            .filter(|&slot| slot != 0)
            .ok_or(Unsupported::OperandShape(
                "graph property site without an IC slot",
            ))?;
        let stub = match access {
            PropertySourceAccess::Load => abi::STUB_JIT_LOAD_PROPERTY,
            PropertySourceAccess::Store => abi::STUB_JIT_STORE_PROPERTY,
        };
        let values: &[u8] = match access {
            PropertySourceAccess::Load => &[receiver],
            PropertySourceAccess::Store => &[receiver, value],
        };
        let mut arguments: smallvec::SmallVec<[CommittedArgument; 3]> = values
            .iter()
            .copied()
            .map(CommittedArgument::Value)
            .collect();
        arguments.push(CommittedArgument::Address(
            slot,
            RelocationTarget::PropertyIcSlot {
                function_id,
                byte_pc,
            },
        ));
        self.emit_committed_call(node, stub, &arguments, destination)
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
    ) -> Result<(), Unsupported> {
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
            dynasm!(self.ops
                ; .arch aarch64
                ; b =>miss
                ; =>walk
                ; movz W(budget), INSTANCEOF_CHAIN_BOUND
                ; =>step
            );
            self.emit_shape_state(cursor);
            dynasm!(self.ops ; .arch aarch64
                ; tst w16, u32::from(ShapeState::OPAQUE_LOOKUP_MASK)
                ; b.ne =>miss);
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
            &[
                CommittedArgument::Value(value),
                CommittedArgument::Value(target),
            ],
            Some(destination),
        )?;
        dynasm!(self.ops ; .arch aarch64 ; =>done);
        Ok(())
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
                        let shape_byte = self.view.object_shape_byte;
                        dynasm!(self.ops
                            ; .arch aarch64
                            ; ldr w16, [X(header), shape_byte]
                            ; cbz w16, =>next
                        );
                        self.load_immediate(17, u64::from(shape));
                        dynasm!(self.ops ; .arch aarch64 ; cmp w16, w17 ; b.ne =>next);
                    }
                    Op::GuardDictionaryLayout { layout, .. } => {
                        let shape_byte = self.view.object_shape_byte;
                        let state_byte = self.view.shape_state_byte;
                        let exotic_byte = self.view.object_exotic_handle_byte;
                        let layout_byte = self.view.exotic_dictionary_layout_byte;
                        dynasm!(self.ops
                            ; .arch aarch64
                            ; and x17, X(holder), #0xffff_ffff_0000_0000
                            ; ldr w16, [X(holder), shape_byte]
                            ; add x16, x17, x16
                            ; ldrb w16, [x16, state_byte]
                            ; tst w16, u32::from(ShapeState::DICTIONARY_MASK)
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
                        // The exact eligible shape before it proves ordinary
                        // descriptor authority and the immutable atom/slot map.
                    }
                    Op::LoadField {
                        object: operand,
                        field,
                    } => {
                        let value_byte = field.byte_offset();
                        let header = object(operand);
                        self.emit_field_storage(header, 16, field);
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

    /// Retain this JS site's exact roots and source.
    /// This compile-time operation emits no hot frame publication.
    fn node_safepoint(&mut self, node: NodeId) -> Result<abi::SafepointId, Unsupported> {
        self.sites.source(
            self.view,
            self.inline_views,
            self.graph,
            self.allocation,
            node,
        )
    }
    /// The exact record of an active C helper at `node`.
    fn helper_safepoint(&mut self, node: NodeId) -> Result<abi::SafepointId, Unsupported> {
        self.sites.helper(
            self.view,
            self.inline_views,
            self.graph,
            self.allocation,
            node,
        )
    }
    fn emit_stamp(&mut self, id: abi::SafepointId) {
        self.load_word32(16, id);
        dynasm!(self.ops ; .arch aarch64 ; str w16,[x21,abi::NATIVE_FRAME_CALL_SITE_OFFSET]);
    }
    fn stamp_node_safepoint(&mut self, node: NodeId) -> Result<abi::SafepointId, Unsupported> {
        let id = self.helper_safepoint(node)?;
        self.emit_stamp(id);
        Ok(id)
    }
    fn record_js_return(
        &mut self,
        node: NodeId,
        offset: dynasmrt::AssemblyOffset,
    ) -> Result<(), Unsupported> {
        let safepoint_id = self.node_safepoint(node)?;
        crate::return_sites::ReturnSiteRecorder {
            entries: &mut self.return_sites,
            safepoint_id,
            logical_pc: self.graph.outer_pc(node),
        }
        .record(offset)
    }

    /// The snapshot of the body `node` belongs to.
    fn view_of(&self, node: NodeId) -> &'a JitCompileSnapshot {
        metadata::source_view(self.view, self.inline_views, self.graph, node)
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
        plan: crate::call_linkage::CallPlan,
        construct: bool,
        receiver: bool,
        allocation: Option<otter_vm::jit::JitReceiverAllocationPlan>,
    ) -> Result<(), Unsupported> {
        // `new.target` is the callee itself, already in `x1`; an explicit
        // receiver is in `x2`.
        let new_target = construct.then_some(1);
        let receiver_register = receiver.then_some(2);
        let first_argument = if receiver { 2 } else { 1 };
        use crate::arm64::js_call::{CallTarget, emit_call};
        self.node_safepoint(node)?;
        let inputs = self.allocation.node(node).inputs.clone();
        let arguments = &inputs[first_argument..];
        let count = u32::try_from(arguments.len())
            .map_err(|_| Unsupported::OperandShape("call actual count"))?;
        let bytes = self.emit_push_actuals(arguments)?;
        // Construction must clear the request before receiver preparation.
        // Ordinary calls are cleared by the shared call emitter on every target.
        if construct {
            crate::arm64::js_call::emit_clear_construct_ticket(&mut self.ops, 20);
        }
        let generic = self.ops.new_dynamic_label();
        let returned = self.ops.new_dynamic_label();
        if let crate::call_linkage::CallPlan::Bytecode(plan) = plan {
            self.emit_function_identity(plan.function_id, plan.callee_cell, pc, 1, generic);
            // The proven constructor's receiver, allocated before it is
            // entered; a probe miss leaves it to the constructor. The callee
            // stays in `x9`, which the probe does not touch, and nothing
            // between allocation and the call can collect.
            let source_view = self.view_of(node);
            let known_receiver = if let Some(allocation) = allocation {
                let missed = self.ops.new_dynamic_label();
                let allocated = self.ops.new_dynamic_label();
                dynasm!(self.ops ; .arch aarch64 ; mov x9, x1 ; mov x2, x9);
                crate::arm64::emit_receiver_candidate_probe(
                    &mut self.ops,
                    &mut self.relocations,
                    source_view,
                    allocation,
                    20,
                );
                dynasm!(self.ops ; .arch aarch64 ; cbz x1, =>missed);
                crate::arm64::emit_receiver_publication_effect(&mut self.ops, source_view, 20);
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
            let return_pc = emit_call(
                &mut self.ops,
                &mut self.relocations,
                self.transitions,
                20,
                1,
                known_receiver,
                new_target,
                Some(count),
                CallTarget::Known {
                    entry_cell: plan.entry_cell,
                    function_id: plan.function_id,
                },
            );
            self.record_js_return(node, return_pc)?;
            dynasm!(self.ops ; .arch aarch64 ; b =>returned);
        }
        if matches!(plan, crate::call_linkage::CallPlan::Native) {
            crate::arm64::js_call::emit_native_kind_guard(&mut self.ops, 1, generic);

            let return_pc = emit_call(
                &mut self.ops,
                &mut self.relocations,
                self.transitions,
                20,
                1,
                receiver_register,
                new_target,
                Some(count),
                CallTarget::Native,
            );
            self.record_js_return(node, return_pc)?;
            dynasm!(self.ops ; .arch aarch64 ; b =>returned);
        }
        dynasm!(self.ops ; .arch aarch64 ; =>generic);
        let return_pc = emit_call(
            &mut self.ops,
            &mut self.relocations,
            self.transitions,
            20,
            1,
            receiver_register,
            new_target,
            Some(count),
            CallTarget::Generic,
        );
        self.record_js_return(node, return_pc)?;
        dynasm!(self.ops ; .arch aarch64 ; =>returned);
        self.emit_pop_actuals(bytes);
        self.emit_call_completion(node)?;
        Ok(())
    }

    /// The call of `CallForward` `node`: the activation's actual arguments,
    /// in a span below `sp`, each mapped formal in `bindings` replaced by its
    /// current value; then the call with the activation's argument count.
    /// `temps` keep the old stack pointer and the actuals' source across the
    /// copy.
    fn emit_call_forward(
        &mut self,
        node: NodeId,
        plan: crate::call_linkage::CallPlan,
        bindings: &[u16],
        temps: [u8; 2],
    ) -> Result<(), Unsupported> {
        use crate::arm64::js_call::{
            CallTarget, emit_call, emit_pop_forwarded, emit_push_forwarded,
        };
        let overflow = self.eager_exit(node, DeoptReason::WrongValue);
        self.node_safepoint(node)?;
        let inputs = self.allocation.node(node).inputs.clone();
        let old_sp = temps[0];
        let sources: Vec<(u16, ForwardedBinding)> = bindings
            .iter()
            .zip(&inputs[2..])
            .map(|(&index, &location)| {
                let binding = match location {
                    Location::Gp(register) => ForwardedBinding::Register(register),
                    // Slots are addressed from the stack pointer the span
                    // moves.
                    Location::TaggedSlot(_) | Location::UntaggedSlot(_) => ForwardedBinding::Load {
                        base: old_sp,
                        offset: self.slots.offset(location) + self.sp_delta,
                    },
                    Location::Constant(constant) => {
                        ForwardedBinding::Immediate(self.constant_bits(constant).0)
                    }
                    Location::Fp(_) => unreachable!("a forwarded actual is tagged"),
                };
                (index, binding)
            })
            .collect();
        emit_push_forwarded(&mut self.ops, 21, temps, overflow, &sources);
        let generic = self.ops.new_dynamic_label();
        let returned = self.ops.new_dynamic_label();
        if let crate::call_linkage::CallPlan::Bytecode(plan) = plan {
            let pc = self.graph.node(node).pc;
            self.emit_function_identity(plan.function_id, plan.callee_cell, pc, 1, generic);
            let return_pc = emit_call(
                &mut self.ops,
                &mut self.relocations,
                self.transitions,
                20,
                1,
                Some(2),
                None,
                None,
                CallTarget::Known {
                    entry_cell: plan.entry_cell,
                    function_id: plan.function_id,
                },
            );
            self.record_js_return(node, return_pc)?;
            dynasm!(self.ops ; .arch aarch64 ; b =>returned);
        }
        if matches!(plan, crate::call_linkage::CallPlan::Native) {
            crate::arm64::js_call::emit_native_kind_guard(&mut self.ops, 1, generic);

            let return_pc = emit_call(
                &mut self.ops,
                &mut self.relocations,
                self.transitions,
                20,
                1,
                Some(2),
                None,
                None,
                CallTarget::Native,
            );
            self.record_js_return(node, return_pc)?;
            dynasm!(self.ops ; .arch aarch64 ; b =>returned);
        }
        dynasm!(self.ops ; .arch aarch64 ; =>generic);
        let return_pc = emit_call(
            &mut self.ops,
            &mut self.relocations,
            self.transitions,
            20,
            1,
            Some(2),
            None,
            None,
            CallTarget::Generic,
        );
        self.record_js_return(node, return_pc)?;
        dynasm!(self.ops ; .arch aarch64 ; =>returned);
        emit_pop_forwarded(&mut self.ops, 21);
        self.emit_call_completion(node)?;
        Ok(())
    }

    /// Route the completion of the JavaScript call `node` made: success
    /// leaves the result in `x0`; a staged request is entered in its place;
    /// a throw or a parked error goes where `node` throws to.
    fn emit_call_completion(&mut self, node: NodeId) -> Result<(), Unsupported> {
        let (threw, committed_throw) = self.throw_targets(node);
        let threw = self.cond_target(threw);
        let committed_throw = self.cond_target(committed_throw);
        // A callee's Fatal is final: no handler may observe it and its parked
        // error must not be projected into an exception again.
        let fatal = self.cond_target(self.fatal);
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
        let return_pc = crate::arm64::js_call::emit_enter_staged(
            &mut self.ops,
            &mut self.relocations,
            self.transitions,
            20,
        );
        self.record_js_return(node, return_pc)?;
        dynasm!(self.ops
            ; .arch aarch64
            ; b =>completion
            ; =>error
        );
        self.stamp_node_safepoint(node)?;
        self.load_word32(16, self.graph.outer_pc(node));
        dynasm!(self.ops ; .arch aarch64 ; str w16,[x21,crate::entry::NATIVE_FRAME_PC_OFFSET]);
        dynasm!(self.ops ; .arch aarch64
            ; cmp x1, abi::NativeResultStatus::Throw as u32
            ; b.eq =>committed_throw
            ; cmp x1, abi::NativeResultStatus::Fatal as u32
            ; b.eq =>fatal
            ; b =>threw
            ; =>done
        );
        Ok(())
    }

    /// Reserve and fill only the actual arguments below `sp`.
    fn emit_push_actuals(&mut self, arguments: &[Location]) -> Result<u32, Unsupported> {
        let bytes = crate::call_linkage::pushed_argument_bytes(arguments.len())?;
        if bytes == 0 {
            return Ok(0);
        }
        if bytes == 16 {
            // One or two words: one pre-indexed store reserves and fills.
            let word = |codegen: &mut Self, index: usize, scratch: u8| -> u8 {
                match arguments[index] {
                    Location::Gp(register) => register,
                    location => {
                        codegen.emit_move(location, Location::Gp(scratch));
                        scratch
                    }
                }
            };
            let first = word(self, 0, 16);
            if arguments.len() == 2 {
                let second = word(self, 1, 17);
                dynasm!(self.ops ; .arch aarch64 ; stp X(first), X(second), [sp, #-16]!);
            } else {
                dynasm!(self.ops ; .arch aarch64 ; str X(first), [sp, #-16]!);
            }
            self.sp_delta += bytes;
            return Ok(bytes);
        }
        dynasm!(self.ops ; .arch aarch64 ; sub sp, sp, bytes);
        self.sp_delta += bytes;
        for (index, &location) in arguments.iter().enumerate() {
            self.emit_move(location, Location::Gp(16));
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
        if self.in_exception_region(self.graph.outer_pc(node)) {
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
        }
    }

    fn emit_generic(
        &mut self,
        node: NodeId,
        pc: u32,
        registers: &[u16],
    ) -> Result<(), Unsupported> {
        debug_assert_eq!(
            self.graph.node(node).origin,
            0,
            "validated baseline window/source owner"
        );
        debug_assert_eq!(
            pc,
            self.graph.outer_pc(node),
            "Generic physical source is outermost"
        );
        let call_safepoint = self.node_safepoint(node)?;
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
        let exits = crate::template::operation::OperationExits {
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
            let operation = self.plan.instructions[position].op;
            if !crate::template::operation_is_js_call(operation) {
                self.stamp_node_safepoint(node)?;
                self.load_word32(16, self.graph.outer_pc(node));
                dynasm!(self.ops ; .arch aarch64 ; str w16,[x21,crate::entry::NATIVE_FRAME_PC_OFFSET]);
            }

            crate::template::arm64::emit_operation(
                crate::template::arm64::OperationContext {
                    ops: &mut self.ops,
                    relocations: &mut self.relocations,
                    return_sites: &mut self.return_sites,
                    call_safepoint,
                    transitions: self.transitions,
                    view: self.view,
                    plan: self.plan,
                    spliced_functions: &mut self.spliced_functions,
                    labels: &labels,
                    exits,
                    poll_entry: self.transitions.entry(abi::STUB_JIT_BACKEDGE_POLL),
                    far_branches: true,
                    shared_property: &mut self.shared_property,
                    numeric_slow_paths: &mut numeric_slow_paths,
                    coercion_slow_paths: &mut coercion_slow_paths,
                    direct_call_events: &mut self.no_direct_call_events,
                    code_map: &mut self.no_code_map,
                    saved_pairs: self.spill.saved_pairs,
                },
                &self.plan.instructions[position],
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
        });
        (self.exits.len() - 1) as u32
    }

    // ------------------------------------------------------------------
    // Control
    // ------------------------------------------------------------------

    fn edge_moves(&self, from: BlockId, to: BlockId) -> Vec<Move> {
        super::emission::edge_moves(self.allocation, from, to)
    }

    fn emit_jump(&mut self, from: BlockId, to: BlockId, next: Option<BlockId>) {
        self.maybe_island();
        let moves = self.edge_moves(from, to);
        self.emit_parallel_moves(moves);
        let to = self.forwarded(to);
        if next != Some(to) {
            let label = self.labels[&to];
            dynasm!(self.ops ; .arch aarch64 ; b =>label);
        }
    }

    /// The block control reaching `block` really continues in: past every
    /// block that only jumps on, with no phi, node or move of its own.
    fn forwarded(&self, block: BlockId) -> BlockId {
        super::emission::forwarded(self.graph, self.allocation, block)
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
                self.emit_backedge_poll(control)?;
                self.maybe_island();
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
                // A far body keeps each conditional target next to its
                // branch: a forwarded block may lie past conditional reach.
                let (if_true, if_false) = if self.far {
                    (if_true, if_false)
                } else {
                    (self.forwarded(if_true), self.forwarded(if_false))
                };
                let false_label = self.labels[&if_false];
                let true_label = self.labels[&if_true];
                let true_target = self.cond_target(true_label);
                match kind {
                    BranchKind::Int32(condition) if next == Some(if_true) => {
                        // The true successor follows: leave for the false one.
                        let a = Self::gp(allocation.inputs[0]);
                        let operand = self.int32_operand(allocation.inputs[1]);
                        let false_target = self.cond_target(false_label);
                        self.emit_int32_compare(a, operand);
                        self.emit_branch_condition(condition.negate(), false, false_target);
                        return Ok(());
                    }
                    BranchKind::Int32(condition) => {
                        let a = Self::gp(allocation.inputs[0]);
                        let operand = self.int32_operand(allocation.inputs[1]);
                        self.emit_int32_compare(a, operand);
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
                // In near code the return path follows the last block.
                let returned = self.returned;
                if next.is_some() || self.far {
                    dynasm!(self.ops ; .arch aarch64 ; b =>returned);
                }
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
    /// edge's moves. The NoAlloc poll preserves exact-live registers through
    /// their canonical homes before returning or taking its cold exit.
    fn emit_backedge_poll(&mut self, control: NodeId) -> Result<(), Unsupported> {
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
        let live = self.allocation.node(control).live_homes.clone();
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
        let poll_safepoint = self.helper_safepoint(control)?;
        self.deferred
            .push(Box::new(move |codegen: &mut Codegen<'a>| {
                let restore_resume = codegen.ops.new_dynamic_label();
                let restore_exit = codegen.ops.new_dynamic_label();
                let restore_threw = codegen.ops.new_dynamic_label();
                dynasm!(codegen.ops ; .arch aarch64 ; =>slow);
                for &(location, home) in &live {
                    if !matches!(home, Location::Constant(_)) {
                        codegen.emit_move(location, home);
                    }
                }
                codegen.emit_stamp(poll_safepoint);
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
                    for &(location, home) in &live {
                        codegen.emit_move(home, location);
                    }
                    dynasm!(codegen.ops ; .arch aarch64 ; b =>target);
                }
            }));
        Ok(())
    }

    // ------------------------------------------------------------------
    // Exits
    // ------------------------------------------------------------------

    /// The return path, right after the body's last block.
    fn emit_return_path(&mut self) {
        let returned = self.returned;
        dynasm!(self.ops
            ; .arch aarch64
            ; =>returned
            ; movz x1, NativeResultStatus::Success as u32
        );
        crate::arm64::frame::emit_epilogue(&mut self.ops, self.activation, self.spill);
    }

    fn emit_exit_stubs(&mut self) {
        if self.far {
            self.emit_return_path();
        }
        let fatal = self.fatal;
        let activation = self.activation;
        let spill = self.spill;
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
        dynasm!(self.ops ; .arch aarch64 ; =>threw);
        // The body cannot resume: only the exception scratch stays rooted.
        self.emit_stamp(FIRST_SITE_SAFEPOINT);
        dynasm!(self.ops ; .arch aarch64 ; mov x0, x20);
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
        );
        // A pure thrown heap value may exist only in x0. Root it in the
        // scratch, the only home the routing record names.
        self.store_slot_gp(0, self.slots.exception_scratch());
        self.emit_stamp(FIRST_SITE_SAFEPOINT);
        dynasm!(self.ops ; .arch aarch64 ; mov x1, x0 ; mov x0, x20);
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
        dynasm!(self.ops
            ; .arch aarch64
            ; =>materialize
            ; str x30, [sp, #-16]!
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
            ; add x3, sp, #16
            ; mov x4, x19
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
            ; ldr x30, [sp], #16
            ; cmp x1, NativeResultStatus::SideExit as u32
            ; b.ne =>fatal
            ; ret
        );
    }

    /// The shared deopt handler rebuilds the interpreter frame from
    /// canonical homes, then resumes its interpreter continuation. The exit
    /// index is in `w17`; no register dump or secondary value home exists.
    fn emit_deopt_handler(&mut self) {
        let deopt = self.deopt;
        dynasm!(self.ops ; .arch aarch64 ; =>deopt);
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
            ; mov x4, x19
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
            ; cmp x1, NativeResultStatus::SideExit as u32
            ; b.eq =>side_exit
        );
        // An inline chain already completed, or the writeback failed.
        crate::arm64::frame::emit_epilogue(&mut self.ops, activation, spill);
    }
}

fn node_name(kind: &Kind) -> &'static str {
    match kind {
        Kind::Generic { .. } => "graph Generic",
        _ => "graph node",
    }
}

#[cfg(test)]
#[path = "frame_region_tests.rs"]
mod frame_region_tests;

impl super::moves::Emitter for Codegen<'_> {
    fn move_value(&mut self, from: Location, to: Location) {
        self.emit_move(from, to);
    }

    fn park(&mut self, from: Location) {
        self.emit_park(from);
    }

    fn unpark(&mut self, to: Location) {
        self.emit_unpark(to);
    }
}

/// The float64 `bits` as an `fmov` immediate: `±(1 + m/16) × 2^e` with
/// `m` in 0..=15 and `e` in -3..=4.
fn fmov_immediate(bits: u64) -> Option<f64> {
    let value = f64::from_bits(bits);
    let exponent = ((bits >> 52) & 0x7ff) as i64 - 1023;
    let fraction = bits & ((1 << 52) - 1);
    ((-3..=4).contains(&exponent) && fraction & ((1 << 48) - 1) == 0).then_some(value)
}
