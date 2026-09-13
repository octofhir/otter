//! AArch64 emission for allocated scalar Machine IR.
//!
//! # Contents
//! - [`emit`] — emits one scalar function from allocator locations.
//! - Exact JavaScript Number decode, scalar coercion leaves, canonical boxing,
//!   and shared cold exits.
//! - regalloc2 edit emission between selected instructions.
//! - `value_span` — shared rooted packet emission for calls and literals.
//!
//! # Invariants
//! - Callee-saved `x19` retains the entry context; allocated `x20..x28` and
//!   `d8..d15` have one exact allocation-driven save/restore set.
//! - `x15..x18`, `d16..d31` are emitter/platform scratch excluded from
//!   allocation; cold deopt dumps use the target's complete register-id map.
//! - Spill storage and offsets come only from [`MachineFrameLayout`].
//! - Allocating calls copy late-use tagged roots into the layout's native save
//!   area before constructing the VM allocation packet. Direct base constructs
//!   probe non-reentrant receiver allocation before an observable fallback;
//!   reentrant direct calls link the same homes through the VM-owned root chain.
//!   Both reload every collector-rewritten value before success, throw, or
//!   exact deoptimization.
//! - A failed entry Number guard writes logical PC zero. Mid-function tagged
//!   numeric guards use allocator-driven exact deopt state at their owning
//!   bytecode operation, always before externally visible effects.
//! - Tagged loose comparisons against a static nullish operand return directly
//!   for null, undefined, non-cell primitives, and every cell that cannot carry
//!   HTMLDDA. Only a native-function cell exits at the original comparison
//!   before the Boolean result is defined, preserving the canonical decision.
//! - Checked integer overflow uses the allocator-driven VM [`DeoptRuntime`];
//!   the emitter owns no parallel reconstruction recipe.
//! - Settled tagged dense and typed element accesses keep receiver, fast index,
//!   and store value in allocator-owned late locations while one shared guard
//!   program proves the VM-baked layout. A miss branches before effects to one
//!   cold sibling that publishes the canonical roots, boxes a raw Int32/Uint32
//!   index from its late home, preserves the caller bank around that cold call,
//!   and completes through the fixed boxed-value VM boundary; it never
//!   deoptimizes and replays the operation. The generated hit cannot allocate,
//!   reenter, or pay a call-clobber allocation boundary.
//!   Packed-double accesses retain their exact pre-effect deopt boundary.
//!   Reducible non-reentrant packed-double loops may retain an untraced raw
//!   base/length pair in the Machine frame across backedges. Entry, OSR, and
//!   external loop-entry paths clear every pair before it can be observed.
//!   An unsupported or unprepared element access clears those raw caches,
//!   publishes exact moving roots, and calls the fixed boxed-value VM boundary.
//!   The operation either completes once or propagates its parked exception;
//!   no post-call status may deopt and replay it.
//! - Settled ordinary named properties consume the VM's complete monomorphic
//!   or polymorphic shape/slot chain directly. Loads remain tagged; stores
//!   guard the whole chain before one commit. Tagged values use the `x19`
//!   Machine context for the post-commit generational barrier, while values
//!   proven non-cell before boxing omit that barrier entirely. Dense-array and
//!   primitive-string `.length` reads use the shared exotic layout guard before
//!   that chain.
//! - Typed binding guards validate global-this cells, captured-cell spines,
//!   TDZ/writability, or baked global lexical/object proofs. A hit reads or
//!   writes directly and stores run the generated barrier. Every miss enters
//!   the single rooted committed binding call, whose payload and descriptor-
//!   owned status remain explicit SSA through Success/Throw/Fatal control;
//!   no emitter-hidden status branch, deopt, or replay exists.
//! - Backedge polls run before allocator edge edits and preserve every value
//!   live into the loop header across the leaf runtime call. An `x29` local
//!   countdown amortizes shared interrupt and fuel-cell traffic over the same
//!   bounded batch used by the general optimizing tier.
//! - OSR trampolines decode only live loop-header inputs into the exact
//!   late-use locations selected by regalloc2; rejection never mutates VM slots.
//! - Successful results use the VM's canonical tagged representation.
//! - Static-native calls use the shared bootstrap identity guard and leaf ABI;
//!   x9 owns the callee, x1/x2 the boxed arguments, and x0 the boxed result.
//!   A non-success leaf status exits at the exact pre-call state without roots
//!   or reentry, with every ABI clobber declared before allocation.
//! - Pure scalar leaves exchange unboxed scalars in fixed ABI operands;
//!   regalloc2 owns every argument/result move and no frame shuttle exists.
//! - Truthiness probes read immediate tags and non-primitive cell headers with
//!   reserved scratch registers; canonical leaf calls occupy separate blocks.
//! - Derived-this binding has explicit fast/cold Machine blocks. The fast
//!   probe writes only an unbound stack-owned derived frame; the committed
//!   cold call owns materialized state, duplicate-bind errors and local catches.
//! - Plain, guarded-method, and fixed-arity base/derived/super JavaScript calls
//!   reuse the shared generated linkage emitter; the allocator supplies only
//!   location-aware loads, stores, root reloads, and landing-pad result homes.
//!   A fixed-argument method chain's final pre-effect guard miss copies the
//!   receiver and every argument from published canonical root homes into one
//!   frame-owned raw packet, then completes through the fixed reentrant method
//!   boundary. Success or throw commits once; no post-entry path deoptimizes
//!   and replays the call. Local-catch and spread misses retain exact exits
//!   until their committed landing/iterator contracts are representable.

// dynasm's dynamic-register expansion calls `.into()` on the required u8
// register encoding. Clippy sees the macro expansion as an identity conversion.
#![allow(clippy::useless_conversion)]

mod forward_call;
mod loose_equality;
mod truthiness;
mod value_span;
use value_span::emit_value_span_arguments;

use dynasmrt::{AssemblyOffset, DynamicLabel, DynasmApi, DynasmLabelApi, dynasm};
use otter_bytecode::opcode_schema::{
    BindingRead, BindingSemantics, BindingWrite, BindingWriteCheck,
};
use otter_vm::{
    JitCompileSnapshot, JitElementAccess, UPVALUE_CELL_TYPE_TAG, Value,
    deopt::DeoptRuntime,
    native_abi::{
        NativeResultDomain, NativeResultStatus, RuntimeStubDescriptor, RuntimeStubResultAbi,
        RuntimeStubSignature, STUB_ARRAY_CONSTRUCT_ALLOC, STUB_JIT_ACKNOWLEDGE_CAUGHT_THROW,
        STUB_JIT_BACKEDGE_POLL, STUB_JIT_BINDING_VALUE, STUB_JIT_CALL_METHOD_VALUE,
        STUB_JIT_CALL_WITH_THIS_VALUE, STUB_JIT_CLASS_SUPER_CONSTRUCTOR, STUB_JIT_CONSTRUCT_VALUE,
        STUB_JIT_DEOPT_WRITEBACK, STUB_JIT_FINISH_ERROR, STUB_JIT_LOAD_ELEMENT,
        STUB_JIT_LOAD_PROPERTY, STUB_JIT_STORE_ELEMENT, STUB_JIT_STORE_PROPERTY,
        STUB_NUMBER_POW_F64_LEAF, STUB_NUMBER_REM_F64_LEAF, STUB_NUMBER_TO_INT32_F64_LEAF,
        STUB_STRICT_EQ_LEAF, STUB_STRING_CONCAT_ALLOC, STUB_TO_BOOLEAN_LEAF,
    },
};

use super::super::{
    AllocatedLocation, AllocatedSequence, AllocationEdit, AllocationPoint, CallDescriptor,
    CallTarget, DeoptId, DirectCallArgumentMode, DirectCallKind, ExceptionalEdge,
    InstructionSequence, MachineBindingTarget, MachineFrameLayout, MachineInstructionId,
    MachineOpcode, MachineOsrInput, MachineOsrType, MachineRepresentation, MachineSafepointSite,
    MachineSafepointTable, MachineValue, PackedDoubleViewCacheId, binding_target_matches_semantics,
    is_explicit_committed_runtime_call,
};
use crate::{
    CompiledCode, Unsupported,
    arm64::{
        DirectCallArguments, DirectCallForm, DirectCallSite, GENERATED_POLL_BATCH,
        emit_direct_call_with_access, emit_method_guard_from_tagged_register,
    },
    artifact::relocation::{PropertyIcAccess, RelocationCapture, RelocationTarget},
    entry::{
        ALLOC_CTX_SAFEPOINT_ID_OFFSET, ALLOC_CTX_SPILL_SLOT_COUNT_OFFSET,
        ALLOC_CTX_SPILL_SLOTS_OFFSET, ALLOC_CTX_STACK_SIZE, ALLOC_CTX_THREAD_OFFSET,
        CANONICAL_NAN_HI16, DOUBLE_OFFSET_HI16, GLOBAL_THIS_OFFSET_PTR_OFFSET,
        MACHINE_ROOT_RECORD_BASE_OFFSET, MACHINE_ROOT_RECORD_CODE_OBJECT_ID_OFFSET,
        MACHINE_ROOT_RECORD_COUNT_OFFSET, MACHINE_ROOT_RECORD_PREVIOUS_OFFSET,
        MACHINE_ROOT_RECORD_SAFEPOINT_ID_OFFSET, MACHINE_ROOT_RECORD_SIZE,
        MACHINE_ROOTS_PTR_OFFSET, NATIVE_FRAME_OFFSET, NATIVE_FRAME_PC_OFFSET,
        NATIVE_FRAME_REGISTER_BASE_OFFSET, NATIVE_FRAME_REGISTER_COUNT_OFFSET,
        NATIVE_FRAME_THIS_OFFSET, NATIVE_FRAME_UPVALUE_BASE_OFFSET,
        NATIVE_FRAME_UPVALUE_COUNT_OFFSET, NUMBER_TAG_HI16, OBJECT_BODY_TYPE_TAG, THREAD_OFFSET,
        TransitionTable, VALUE_HOLE, VALUE_NULL, VALUE_UNDEFINED,
        VM_THREAD_BACKEDGE_FUEL_CELL_OFFSET, VM_THREAD_CODE_OBJECT_ID_OFFSET,
        VM_THREAD_GC_HEAP_OFFSET, VM_THREAD_GLOBAL_LEXICAL_EPOCH_CELL_OFFSET,
        VM_THREAD_INTERRUPT_CELL_OFFSET, WhiskerIcCell,
    },
    template::arm64::ic_probe::{
        DenseIndexForm, element_access_for, emit_dense_element_view, emit_element_address,
        emit_element_address_from_dense_view, emit_element_read, emit_element_write,
        emit_exotic_length_fast, emit_property_ic_load, emit_property_ic_store_guard,
        emit_property_transition_shape_barrier, emit_settled_property_load,
        emit_settled_property_store_guard,
    },
    template::arm64::values::{
        CellTest, emit_cell_test, emit_html_dda_candidate_exit, emit_initialize_inline_values_ptr,
        emit_slab_base, emit_write_barrier_with_context,
    },
};
use std::collections::BTreeMap;

// The deopt namespace preserves physical encodings. It includes one unused
// x29 slot so the following FP bank remains naturally 16-byte aligned.
pub(super) const GPR_BUDGET: u16 = 30;
pub(super) const FP_BUDGET: u16 = 16;
const BASE_FIXED_FRAME_BYTES: u32 = 32;
const DEOPT_DUMP_BYTES: u32 = (GPR_BUDGET as u32 + FP_BUDGET as u32) * 8;
const COMMITTED_ELEMENT_CALLER_SAVE_BYTES: u32 = 144;
const COMMITTED_ELEMENT_ROOT_RECORD_BYTES: u32 = MACHINE_ROOT_RECORD_SIZE;
const COMMITTED_ELEMENT_COLD_STACK_BYTES: u32 =
    COMMITTED_ELEMENT_CALLER_SAVE_BYTES + COMMITTED_ELEMENT_ROOT_RECORD_BYTES;
const COMMITTED_ELEMENT_GPR_SAVE_PAIRS: [(u8, u8, u32); 5] =
    [(0, 1, 0), (2, 3, 16), (4, 5, 32), (6, 7, 48), (8, 10, 64)];
const COMMITTED_ELEMENT_FP_SAVE_PAIRS: [(u8, u8, u32); 4] =
    [(0, 1, 80), (2, 3, 96), (4, 5, 112), (6, 7, 128)];
const _: () = assert!(COMMITTED_ELEMENT_COLD_STACK_BYTES <= DEOPT_DUMP_BYTES);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SavedFrame {
    gpr_count: u8,
    fp_count: u8,
}

impl SavedFrame {
    fn from_allocation(allocation: &AllocatedSequence) -> Self {
        let mut gpr_count = 0;
        let mut fp_count = 0;
        for register in allocation
            .used_registers()
            .filter(|register| register.is_integer() && (20..=28).contains(&register.encoding()))
        {
            gpr_count = gpr_count.max(register.encoding() - 19);
        }
        for register in allocation
            .used_registers()
            .filter(|register| register.is_float() && (8..=15).contains(&register.encoding()))
        {
            fp_count = fp_count.max(register.encoding() - 7);
        }
        Self {
            gpr_count,
            fp_count,
        }
    }

    fn fixed_bytes(self) -> u32 {
        BASE_FIXED_FRAME_BYTES
            + u32::from(self.gpr_count.saturating_sub(1))
                .saturating_mul(8)
                .next_multiple_of(16)
            + u32::from(self.fp_count)
                .saturating_mul(8)
                .next_multiple_of(16)
    }
}

pub(super) struct Emission {
    pub(super) code: CompiledCode,
    pub(super) generated_stack_frame_bytes: u32,
    pub(super) relocations: RelocationCapture,
    pub(super) osr_entries: BTreeMap<u32, usize>,
    pub(super) osr_regions: Vec<(u32, usize, usize)>,
    pub(super) constructor_field_regions: Vec<(u32, usize, usize)>,
    pub(super) structural_regions: Vec<(&'static str, Option<u32>, usize, usize)>,
}

struct OsrSite {
    instruction: MachineInstructionId,
    logical_pc: u32,
    inputs: Vec<MachineOsrInput>,
    continuation: DynamicLabel,
}

fn instruction_deopt_label(
    deopt: Option<DeoptId>,
    labels: &[DynamicLabel],
) -> Result<DynamicLabel, Unsupported> {
    deopt
        .and_then(|deopt| labels.get(deopt.0 as usize).copied())
        .ok_or(Unsupported::OperandShape(
            "numeric checked operation deopt label",
        ))
}

fn emit_backedge_poll(
    ops: &mut dynasmrt::aarch64::Assembler,
    relocations: &mut RelocationCapture,
    poll_entry: u64,
    bailout: DynamicLabel,
    finish_error: DynamicLabel,
    fatal: DynamicLabel,
) {
    let batched = ops.new_dynamic_label();
    let slow = ops.new_dynamic_label();
    let cont = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; subs w29, w29, #1
        ; b.ne =>batched
        ; movz w29, GENERATED_POLL_BATCH
        ; ldr x17, [x19, THREAD_OFFSET]
        ; ldr x16, [x17, VM_THREAD_INTERRUPT_CELL_OFFSET]
        ; ldrb w16, [x16]
        ; cbnz w16, =>bailout
        ; ldr x16, [x17, VM_THREAD_BACKEDGE_FUEL_CELL_OFFSET]
        ; ldr x17, [x16]
        ; subs x17, x17, GENERATED_POLL_BATCH
        ; str x17, [x16]
        ; b.gt =>cont
        ; =>slow
        ; mov x0, x19
    );
    emit_load_symbolic_u64(
        ops,
        relocations,
        16,
        poll_entry,
        RelocationTarget::runtime_stub(STUB_JIT_BACKEDGE_POLL),
    );
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; cmp x0, NativeResultStatus::Success as u32
        ; b.eq =>cont
        ; cmp x0, NativeResultStatus::Throw as u32
        ; b.eq =>finish_error
        ; b =>fatal
        ; =>cont
        ; =>batched
    );
}

fn emit_float_leaf_binary(
    ops: &mut dynasmrt::aarch64::Assembler,
    relocations: &mut RelocationCapture,
    entry: u64,
    descriptor: RuntimeStubDescriptor,
) {
    emit_load_symbolic_u64(
        ops,
        relocations,
        16,
        entry,
        RelocationTarget::runtime_stub(descriptor),
    );
    dynasm!(ops ; .arch aarch64 ; blr x16);
}

fn emit_float_to_int32_leaf(
    ops: &mut dynasmrt::aarch64::Assembler,
    relocations: &mut RelocationCapture,
    entry: u64,
) {
    emit_load_symbolic_u64(
        ops,
        relocations,
        16,
        entry,
        RelocationTarget::runtime_stub(STUB_NUMBER_TO_INT32_F64_LEAF),
    );
    dynasm!(ops ; .arch aarch64 ; blr x16);
}

#[cfg(test)]
pub(super) fn frame_layout(
    allocation: &AllocatedSequence,
    root_slots: u16,
) -> Result<MachineFrameLayout, Unsupported> {
    MachineFrameLayout::new(
        allocation,
        root_slots,
        SavedFrame::from_allocation(allocation).fixed_bytes(),
        16,
    )
    .map_err(|_| Unsupported::OperandShape("scalar Machine IR frame layout"))
}

pub(super) fn frame_layout_with_raw_slots(
    allocation: &AllocatedSequence,
    root_slots: u16,
    raw_slots: u16,
) -> Result<MachineFrameLayout, Unsupported> {
    MachineFrameLayout::new_with_raw_slots(
        allocation,
        root_slots,
        raw_slots,
        SavedFrame::from_allocation(allocation).fixed_bytes(),
        16,
    )
    .map_err(|_| Unsupported::OperandShape("scalar Machine IR frame layout"))
}

fn packed_double_view_cache_offsets(
    frame: MachineFrameLayout,
    cache: PackedDoubleViewCacheId,
) -> Result<(u32, u32), Unsupported> {
    let base_slot = u16::try_from(cache.raw_word())
        .map_err(|_| Unsupported::OperandShape("packed-double view-cache base slot"))?;
    let length_slot = base_slot.checked_add(1).ok_or(Unsupported::OperandShape(
        "packed-double view-cache length slot",
    ))?;
    Ok((
        raw_offset(frame, base_slot)?,
        raw_offset(frame, length_slot)?,
    ))
}

fn emit_clear_packed_double_view_caches(
    ops: &mut dynasmrt::aarch64::Assembler,
    frame: MachineFrameLayout,
    cache_count: u8,
) -> Result<(), Unsupported> {
    for index in 0..usize::from(cache_count) {
        let cache = PackedDoubleViewCacheId::new(index).ok_or(Unsupported::OperandShape(
            "packed-double view-cache identity",
        ))?;
        let (base_offset, _) = packed_double_view_cache_offsets(frame, cache)?;
        // Clear instructions own no clobber. AArch64's scaled unsigned STR
        // reaches every bounded normal frame we admit without a scratch.
        if base_offset > 32_760 || !base_offset.is_multiple_of(8) {
            return Err(Unsupported::OperandShape(
                "packed-double view-cache clear offset",
            ));
        }
        dynasm!(ops ; .arch aarch64 ; str xzr, [sp, base_offset]);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn emit_packed_double_element_address<R, I>(
    ops: &mut dynasmrt::aarch64::Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    frame: MachineFrameLayout,
    access: &JitElementAccess,
    cache: Option<PackedDoubleViewCacheId>,
    load_receiver: R,
    load_index: I,
    index_form: DenseIndexForm,
    deopt: DynamicLabel,
) -> Result<(), Unsupported>
where
    R: FnOnce(&mut dynasmrt::aarch64::Assembler, u8) -> Result<(), Unsupported>,
    I: FnOnce(&mut dynasmrt::aarch64::Assembler, u8) -> Result<(), Unsupported>,
{
    let Some(cache) = cache else {
        return emit_element_address(
            ops,
            relocations,
            view,
            access,
            load_receiver,
            load_index,
            index_form,
            deopt,
        );
    };
    let (base_offset, length_offset) = packed_double_view_cache_offsets(frame, cache)?;
    if base_offset > 32_760
        || length_offset > 32_760
        || !base_offset.is_multiple_of(8)
        || !length_offset.is_multiple_of(8)
    {
        return Err(Unsupported::OperandShape(
            "packed-double view-cache access offset",
        ));
    }
    let cached = ops.new_dynamic_label();
    let ready = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; ldr x16, [sp, base_offset]
        ; cbnz x16, =>cached
    );
    emit_dense_element_view(ops, relocations, view, access, load_receiver, deopt)?;
    // Publish base last. A zero base remains the inactive sentinel even if a
    // cold proof exits before the pair is complete.
    dynasm!(ops
        ; .arch aarch64
        ; str x14, [sp, length_offset]
        ; str x16, [sp, base_offset]
        ; b =>ready
        ; =>cached
        ; ldr x14, [sp, length_offset]
        ; =>ready
    );
    emit_element_address_from_dense_view(ops, access, load_index, index_form, deopt)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommittedElementKey {
    Tagged(MachineValue),
    Int32(AllocatedLocation),
    Uint32(AllocatedLocation),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CommittedElementArguments {
    receiver: MachineValue,
    key: CommittedElementKey,
    value: Option<MachineValue>,
    preserve_caller_bank: bool,
}

fn committed_element_key(
    representation: MachineRepresentation,
    value: MachineValue,
    location: AllocatedLocation,
) -> Result<CommittedElementKey, Unsupported> {
    match representation {
        MachineRepresentation::Tagged => Ok(CommittedElementKey::Tagged(value)),
        MachineRepresentation::Int32 => Ok(CommittedElementKey::Int32(location)),
        MachineRepresentation::Uint32 => Ok(CommittedElementKey::Uint32(location)),
        _ => Err(Unsupported::OperandShape(
            "scalar committed element index representation",
        )),
    }
}

fn direct_committed_element_arguments(
    sequence: &InstructionSequence,
    instruction: &super::super::MachineInstruction,
    locations: &[AllocatedLocation],
    load: bool,
) -> Result<CommittedElementArguments, Unsupported> {
    let expected_operands = 3;
    if instruction.operands.len() < expected_operands || locations.len() < expected_operands {
        return Err(Unsupported::OperandShape(
            "scalar committed element operands",
        ));
    }
    let receiver = instruction.operands[0].value;
    let index = instruction.operands[1].value;
    let representation = sequence
        .representations()
        .get(index.0 as usize)
        .copied()
        .ok_or(Unsupported::OperandShape(
            "scalar committed element index representation",
        ))?;
    let key = committed_element_key(representation, index, locations[1])?;
    let scalar_key_home_survives_cold_entry = match key {
        CommittedElementKey::Tagged(_) => true,
        CommittedElementKey::Int32(AllocatedLocation::Stack(_))
        | CommittedElementKey::Uint32(AllocatedLocation::Stack(_)) => true,
        CommittedElementKey::Int32(AllocatedLocation::Register(register))
        | CommittedElementKey::Uint32(AllocatedLocation::Register(register)) => {
            register.is_integer() && !instruction.clobbers.contains(&register)
        }
    };
    if !scalar_key_home_survives_cold_entry {
        return Err(Unsupported::OperandShape(
            "scalar committed element late index home",
        ));
    }
    Ok(CommittedElementArguments {
        receiver,
        key,
        value: (!load).then_some(instruction.operands[2].value),
        preserve_caller_bank: true,
    })
}

#[allow(clippy::too_many_arguments)]
fn emit_committed_element_value_call(
    ops: &mut dynasmrt::aarch64::Assembler,
    relocations: &mut RelocationCapture,
    sequence: &InstructionSequence,
    frame: MachineFrameLayout,
    instruction: &super::super::MachineInstruction,
    id: MachineInstructionId,
    safepoints: &MachineSafepointTable,
    deopt_runtime: &DeoptRuntime,
    arguments: CommittedElementArguments,
    target: RuntimeStubDescriptor,
    entry: u64,
    throw_value: DynamicLabel,
    fatal: DynamicLabel,
) -> Result<u32, Unsupported> {
    let load = target == STUB_JIT_LOAD_ELEMENT;
    if !matches!(target, STUB_JIT_LOAD_ELEMENT | STUB_JIT_STORE_ELEMENT)
        || load != arguments.value.is_none()
        || sequence
            .representations()
            .get(arguments.receiver.0 as usize)
            .copied()
            != Some(MachineRepresentation::Tagged)
        || arguments.value.is_some_and(|value| {
            sequence.representations().get(value.0 as usize).copied()
                != Some(MachineRepresentation::Tagged)
        })
        || matches!(arguments.key, CommittedElementKey::Tagged(value)
            if sequence.representations().get(value.0 as usize).copied()
                != Some(MachineRepresentation::Tagged))
        || (!arguments.preserve_caller_bank
            && matches!(
                arguments.key,
                CommittedElementKey::Int32(_) | CommittedElementKey::Uint32(_)
            ))
    {
        return Err(Unsupported::OperandShape(
            "scalar committed element value call",
        ));
    }
    let deopt = instruction.deopt.ok_or(Unsupported::OperandShape(
        "scalar committed element frame state",
    ))?;
    let exit = deopt_runtime
        .exits
        .get(deopt.0 as usize)
        .ok_or(Unsupported::OperandShape(
            "scalar committed element deopt exit",
        ))?;
    let logical_pc = *exit.resume_pcs.first().ok_or(Unsupported::OperandShape(
        "scalar committed element logical PC",
    ))?;
    let byte_pc = deopt_runtime
        .table
        .lookup(otter_vm::deopt::DeoptExitId(deopt.0))
        .map(|state| state.innermost().byte_pc)
        .ok_or(Unsupported::OperandShape(
            "scalar committed element byte PC",
        ))?;
    let site = safepoints
        .site(id)
        .filter(|site| instruction.safepoint == Some(site.id))
        .ok_or(Unsupported::OperandShape(
            "scalar committed element safepoint",
        ))?;

    // Raw view caches contain host pointers and therefore cannot cross this
    // reentrant boundary. Canonical moving roots must be saved at the Machine
    // frame's original SP before the cold-only caller bank changes it.
    emit_clear_packed_double_view_caches(ops, frame, sequence.packed_double_view_cache_count())?;
    emit_save_safepoint_roots(ops, frame, site)?;
    let caller_save_bias = if arguments.preserve_caller_bank {
        emit_save_committed_element_caller_bank(ops);
        COMMITTED_ELEMENT_CALLER_SAVE_BYTES
    } else {
        0
    };
    match arguments.key {
        CommittedElementKey::Tagged(_) => {}
        CommittedElementKey::Int32(location) => {
            emit_load_allocated_integer(ops, frame, location, 2, caller_save_bias)?;
            // Integer producers define through W registers, but normalize the
            // upper word explicitly before installing the exact Int32 tag.
            dynasm!(ops
                ; .arch aarch64
                ; mov w2, w2
                ; movz x16, NUMBER_TAG_HI16, lsl #48
                ; orr x2, x2, x16
            );
        }
        CommittedElementKey::Uint32(location) => {
            emit_load_allocated_integer(ops, frame, location, 2, caller_save_bias)?;
            emit_box_uint32(ops, 2, 2);
        }
    }
    emit_publish_machine_roots_with_bias(ops, frame, site, caller_save_bias)?;
    let argument_bias = caller_save_bias
        .checked_add(MACHINE_ROOT_RECORD_SIZE)
        .ok_or(Unsupported::OperandShape(
            "scalar committed element argument stack bias",
        ))?;
    emit_load_safepoint_root(ops, frame, site, arguments.receiver, 1, argument_bias)?;
    match arguments.key {
        CommittedElementKey::Tagged(key) => {
            emit_load_safepoint_root(ops, frame, site, key, 2, argument_bias)?
        }
        CommittedElementKey::Int32(_) | CommittedElementKey::Uint32(_) => {}
    }
    if let Some(value) = arguments.value {
        emit_load_safepoint_root(ops, frame, site, value, 3, argument_bias)?;
    }
    emit_load_u64(ops, 15, u64::from(logical_pc));
    dynasm!(ops
        ; .arch aarch64
        ; ldr x16, [x19, NATIVE_FRAME_OFFSET]
        ; str w15, [x16, NATIVE_FRAME_PC_OFFSET]
        ; mov x0, x19
    );
    emit_load_symbolic_u64(
        ops,
        relocations,
        16,
        entry,
        RelocationTarget::runtime_stub(target),
    );
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        // x17 is outside the allocator's bank. Preserve the committed result
        // while the collector-rewritten canonical homes are restored.
        ; mov x17, x0
        ; mov x15, x1
    );
    emit_clear_machine_roots(ops);
    if arguments.preserve_caller_bank {
        emit_restore_committed_element_caller_bank(ops);
    }
    // Reload collector-rewritten roots last: their canonical values must win
    // over the exact pre-call register bank restored above.
    emit_reload_safepoint_roots(ops, frame, site)?;
    // The fixed committed element ABI completes exactly once. A JavaScript
    // throw is a pure value in x17; structural failures alone use ctx.error.
    let completed = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; cbz x15, =>completed
        ; cmp x15, NativeResultStatus::Throw as u32
        ; b.ne =>fatal
        ; mov x0, x17
        ; b =>throw_value
        ; =>completed
    );
    Ok(byte_pc)
}

fn emit_landing_pad_transfer(
    ops: &mut dynasmrt::aarch64::Assembler,
    frame: MachineFrameLayout,
    result_location: AllocatedLocation,
    result_register: u8,
    allocation_edits: &[AllocationEdit],
    instruction: MachineInstructionId,
    target: DynamicLabel,
) -> Result<(), Unsupported> {
    emit_store_allocated_tagged(ops, frame, result_location, result_register, 0)?;
    // A local throw branches around the instruction loop's normal fallthrough.
    // Apply the result live-range transition before entering the successor: a
    // catch block parameter is not required to share the call result's home.
    emit_edits(
        ops,
        allocation_edits,
        AllocationPoint::After(instruction),
        frame,
    )?;
    dynasm!(ops ; .arch aarch64 ; b =>target);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn emit_committed_runtime_call(
    ops: &mut dynasmrt::aarch64::Assembler,
    relocations: &mut RelocationCapture,
    transitions: &TransitionTable,
    sequence: &InstructionSequence,
    frame: MachineFrameLayout,
    instruction: &super::super::MachineInstruction,
    id: MachineInstructionId,
    descriptor: &CallDescriptor,
    locations: &[AllocatedLocation],
    allocation_edits: &[AllocationEdit],
    safepoints: &MachineSafepointTable,
    block_labels: &[DynamicLabel],
    throw_value: DynamicLabel,
    fatal_exit: DynamicLabel,
) -> Result<u32, Unsupported> {
    let (target, logical_pc, byte_pc, semantic_arity, literal) = match &descriptor.target {
        CallTarget::CommittedRuntime {
            target,
            logical_pc,
            byte_pc,
            semantic_arity,
        } => {
            if *semantic_arity > 2 || target.signature != RuntimeStubSignature::CommittedValue2 {
                return Err(Unsupported::OperandShape("scalar committed runtime ABI"));
            }
            (
                *target,
                *logical_pc,
                *byte_pc,
                usize::from(*semantic_arity),
                false,
            )
        }
        CallTarget::LiteralAllocation {
            target,
            logical_pc,
            byte_pc,
        } => {
            if target.signature != RuntimeStubSignature::ReentrantValueSpan {
                return Err(Unsupported::OperandShape("scalar literal allocation ABI"));
            }
            (
                *target,
                *logical_pc,
                *byte_pc,
                descriptor.arguments.len(),
                true,
            )
        }
        _ => return Err(Unsupported::OperandShape("scalar committed runtime target")),
    };
    if target.result_abi != RuntimeStubResultAbi::NativePair
        || target.result_domain != NativeResultDomain::Committed
        || descriptor.arguments.len() != semantic_arity
        || descriptor.results != [MachineRepresentation::Tagged]
        || instruction.deopt.is_some()
    {
        return Err(Unsupported::OperandShape(
            "scalar committed runtime contract",
        ));
    }
    let arguments = instruction
        .operands
        .iter()
        .filter(|operand| operand.purpose == super::super::OperandPurpose::Input)
        .map(|operand| operand.value)
        .collect::<Vec<_>>();
    let result_operand = instruction
        .operands
        .iter()
        .position(|operand| operand.purpose == super::super::OperandPurpose::Output)
        .ok_or(Unsupported::OperandShape("scalar committed runtime result"))?;
    let result_location = *locations
        .get(result_operand)
        .ok_or(Unsupported::OperandShape(
            "scalar committed runtime result allocation",
        ))?;
    if arguments.len() != semantic_arity {
        return Err(Unsupported::OperandShape(
            "scalar committed runtime semantic arity",
        ));
    }
    let site = safepoints
        .site(id)
        .filter(|site| instruction.safepoint == Some(site.id))
        .ok_or(Unsupported::OperandShape(
            "scalar committed runtime safepoint",
        ))?;

    // The typed boundary is effect-once. Keep the complete pre-call moving-root
    // record linked until either the result is committed locally or a pure
    // exception has been handed to the propagation router. No status from
    // either entry can request deoptimization or replay.
    emit_clear_packed_double_view_caches(ops, frame, sequence.packed_double_view_cache_count())?;
    emit_save_safepoint_roots(ops, frame, site)?;
    emit_publish_machine_roots(ops, frame, site)?;
    if literal {
        emit_value_span_arguments(ops, sequence, frame, site, arguments.iter().copied())?;
    } else {
        emit_load_u64(ops, 1, VALUE_UNDEFINED);
        dynasm!(ops ; .arch aarch64 ; mov x2, x1);
        for (index, value) in arguments.iter().copied().enumerate() {
            emit_load_safepoint_root(
                ops,
                frame,
                site,
                value,
                u8::try_from(index + 1)
                    .map_err(|_| Unsupported::OperandShape("scalar committed runtime argument"))?,
                MACHINE_ROOT_RECORD_SIZE,
            )?;
        }
    }
    emit_load_u64(ops, 15, u64::from(logical_pc));
    dynasm!(ops
        ; .arch aarch64
        ; ldr x16, [x19, NATIVE_FRAME_OFFSET]
        ; str w15, [x16, NATIVE_FRAME_PC_OFFSET]
        ; mov x0, x19
    );
    emit_load_symbolic_u64(
        ops,
        relocations,
        16,
        transitions.entry(target),
        RelocationTarget::runtime_stub(target),
    );
    let completed = ops.new_dynamic_label();
    let js_throw = ops.new_dynamic_label();
    let fatal = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        // x17/x15 are outside the allocator bank and survive root cleanup.
        ; mov x17, x0
        ; mov x15, x1
        ; cbz x15, =>completed
        ; cmp x15, NativeResultStatus::Throw as u32
        ; b.eq =>js_throw
        // The committed domain admits only Fatal beyond Success/Throw. Treat any corrupt
        // nonzero status the same way: never expose it to a JavaScript catch.
        ; b =>fatal
        ; =>js_throw
    );
    match descriptor.exceptional {
        ExceptionalEdge::LandingPad(target) => {
            emit_clear_machine_roots(ops);
            emit_reload_safepoint_roots(ops, frame, site)?;
            let target =
                block_labels
                    .get(target.0 as usize)
                    .copied()
                    .ok_or(Unsupported::OperandShape(
                        "scalar committed runtime landing pad",
                    ))?;
            emit_landing_pad_transfer(
                ops,
                frame,
                result_location,
                17,
                allocation_edits,
                id,
                target,
            )?;
        }
        ExceptionalEdge::Propagate => {
            emit_clear_machine_roots(ops);
            emit_reload_safepoint_roots(ops, frame, site)?;
            dynasm!(ops ; .arch aarch64 ; mov x0, x17 ; b =>throw_value);
        }
        ExceptionalEdge::None if literal => {
            // Literal allocation cannot throw JavaScript. An unexpected status
            // remains structural and cannot enter a source-level catch.
            dynasm!(ops ; .arch aarch64 ; b =>fatal);
        }
        ExceptionalEdge::None => {
            return Err(Unsupported::OperandShape(
                "scalar committed runtime exceptional edge",
            ));
        }
    }
    dynasm!(ops ; .arch aarch64 ; =>fatal);
    emit_clear_machine_roots(ops);
    emit_reload_safepoint_roots(ops, frame, site)?;
    dynasm!(ops ; .arch aarch64 ; b =>fatal_exit ; =>completed);
    emit_clear_machine_roots(ops);
    emit_reload_safepoint_roots(ops, frame, site)?;
    emit_store_allocated_tagged(ops, frame, result_location, 17, 0)?;
    Ok(byte_pc)
}

#[allow(clippy::too_many_arguments)]
fn emit_committed_pair_call(
    ops: &mut dynasmrt::aarch64::Assembler,
    relocations: &mut RelocationCapture,
    transitions: &TransitionTable,
    sequence: &InstructionSequence,
    frame: MachineFrameLayout,
    instruction: &super::super::MachineInstruction,
    id: MachineInstructionId,
    descriptor: &CallDescriptor,
    locations: &[AllocatedLocation],
    safepoints: &MachineSafepointTable,
) -> Result<u32, Unsupported> {
    let CallTarget::CommittedRuntime {
        target,
        logical_pc,
        byte_pc,
        semantic_arity,
    } = &descriptor.target
    else {
        return Err(Unsupported::OperandShape(
            "scalar committed pair runtime target",
        ));
    };
    let logical_pc = *logical_pc;
    let byte_pc = *byte_pc;
    let semantic_arity = usize::from(*semantic_arity);
    if !is_explicit_committed_runtime_call(descriptor)
        || semantic_arity > 2
        || target.signature != RuntimeStubSignature::CommittedValue2
        || target.result_abi != RuntimeStubResultAbi::NativePair
        || target.result_domain != NativeResultDomain::Committed
        || descriptor.arguments.len() != semantic_arity
        || instruction.deopt.is_some()
    {
        return Err(Unsupported::OperandShape(
            "scalar committed pair runtime contract",
        ));
    }
    let arguments = instruction
        .operands
        .iter()
        .filter(|operand| operand.purpose == super::super::OperandPurpose::Input)
        .map(|operand| operand.value)
        .collect::<Vec<_>>();
    let outputs = instruction
        .operands
        .iter()
        .enumerate()
        .filter(|(_, operand)| operand.purpose == super::super::OperandPurpose::Output)
        .collect::<Vec<_>>();
    let [payload, status] = outputs.as_slice() else {
        return Err(Unsupported::OperandShape(
            "scalar committed pair runtime result pair",
        ));
    };
    let payload_location = *locations.get(payload.0).ok_or(Unsupported::OperandShape(
        "scalar committed pair payload allocation",
    ))?;
    let status_location = *locations.get(status.0).ok_or(Unsupported::OperandShape(
        "scalar committed pair status allocation",
    ))?;
    if arguments.len() != semantic_arity {
        return Err(Unsupported::OperandShape(
            "scalar committed pair runtime semantic arity",
        ));
    }
    let site = safepoints
        .site(id)
        .filter(|site| instruction.safepoint == Some(site.id))
        .ok_or(Unsupported::OperandShape(
            "scalar committed pair runtime safepoint",
        ))?;

    emit_clear_packed_double_view_caches(ops, frame, sequence.packed_double_view_cache_count())?;
    emit_save_safepoint_roots(ops, frame, site)?;
    emit_publish_machine_roots(ops, frame, site)?;
    emit_load_u64(ops, 1, VALUE_UNDEFINED);
    dynasm!(ops ; .arch aarch64 ; mov x2, x1);
    for (index, value) in arguments.iter().copied().enumerate() {
        emit_load_safepoint_root(
            ops,
            frame,
            site,
            value,
            u8::try_from(index + 1)
                .map_err(|_| Unsupported::OperandShape("scalar committed pair runtime argument"))?,
            MACHINE_ROOT_RECORD_SIZE,
        )?;
    }
    emit_load_u64(ops, 15, u64::from(logical_pc));
    dynasm!(ops
        ; .arch aarch64
        ; ldr x16, [x19, NATIVE_FRAME_OFFSET]
        ; str w15, [x16, NATIVE_FRAME_PC_OFFSET]
        ; mov x0, x19
    );
    emit_load_symbolic_u64(
        ops,
        relocations,
        16,
        transitions.entry(*target),
        RelocationTarget::runtime_stub(*target),
    );
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        // Preserve the physical pair outside the allocator bank while moving
        // roots are restored. No status decision is legal in this emitter.
        ; mov x17, x0
        ; mov x15, x1
    );
    emit_clear_machine_roots(ops);
    emit_reload_safepoint_roots(ops, frame, site)?;
    emit_store_allocated_tagged(ops, frame, payload_location, 17, 0)?;
    emit_store_allocated_tagged(ops, frame, status_location, 15, 0)?;
    Ok(byte_pc)
}

fn binding_requires_live_cell(semantics: BindingSemantics) -> bool {
    matches!(
        semantics,
        BindingSemantics::Read(BindingRead::Global { .. } | BindingRead::Upvalue { .. })
            | BindingSemantics::Write(
                BindingWrite::Global { .. } | BindingWrite::GlobalChecked { .. }
            )
            | BindingSemantics::Write(BindingWrite::Upvalue {
                check: BindingWriteCheck::Checked,
                ..
            })
    )
}

#[allow(clippy::too_many_arguments)]
fn emit_binding_guard(
    ops: &mut dynasmrt::aarch64::Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    frame: MachineFrameLayout,
    semantics: BindingSemantics,
    target: MachineBindingTarget,
    byte_pc: u32,
    locations: &[AllocatedLocation],
) -> Result<(), Unsupported> {
    if locations.len() != 3 + semantics.value_operands().into_iter().flatten().count()
        || !binding_target_matches_semantics(semantics, target)
    {
        return Err(Unsupported::OperandShape("scalar binding guard"));
    }
    let condition = integer_register(locations[0])?;
    let owner = integer_register(locations[1])?;
    let storage = integer_register(locations[2])?;
    let upvalue_value_byte = view.upvalue_value_byte;
    let upvalue_cell_type_tag = UPVALUE_CELL_TYPE_TAG as u32;
    let miss = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();

    if matches!(
        semantics,
        BindingSemantics::Write(BindingWrite::GlobalChecked { .. })
    ) {
        emit_load_allocated_tagged(ops, frame, locations[4], 9, 0)?;
        emit_load_u64(ops, 11, Value::boolean(true).to_bits());
        dynasm!(ops ; .arch aarch64 ; cmp x9, x11 ; b.ne =>miss);
    }

    match target {
        MachineBindingTarget::Cold => dynasm!(ops ; .arch aarch64 ; b =>miss),
        MachineBindingTarget::GlobalThis => {
            if view.cage_base == 0 {
                return Err(Unsupported::OperandShape("scalar global-this binding cage"));
            }
            dynasm!(ops
                ; .arch aarch64
                ; ldr x14, [x19, GLOBAL_THIS_OFFSET_PTR_OFFSET]
                ; cbz x14, =>miss
                ; ldr w12, [x14]
                ; cbz w12, =>miss
            );
            emit_load_symbolic_u64(
                ops,
                relocations,
                13,
                view.cage_base as u64,
                RelocationTarget::GcCageBase,
            );
            emit_load_u64(ops, 16, u64::from(upvalue_value_byte));
            dynasm!(ops
                ; .arch aarch64
                ; add X(owner), x13, x12
                ; mov X(storage), x14
            );
        }
        MachineBindingTarget::Upvalue { index } => {
            if view.cage_base == 0 || index > 4095 {
                return Err(Unsupported::OperandShape("scalar upvalue binding target"));
            }
            dynasm!(ops
                ; .arch aarch64
                ; ldr x10, [x19, NATIVE_FRAME_OFFSET]
                ; ldr w11, [x10, NATIVE_FRAME_UPVALUE_COUNT_OFFSET]
                ; cmp w11, index
                ; b.ls =>miss
                ; ldr x9, [x10, NATIVE_FRAME_UPVALUE_BASE_OFFSET]
                ; cbz x9, =>miss
                ; ldr w9, [x9, index * 4]
                ; cbz w9, =>miss
            );
            emit_load_symbolic_u64(
                ops,
                relocations,
                13,
                view.cage_base as u64,
                RelocationTarget::GcCageBase,
            );
            emit_load_u64(ops, 16, u64::from(upvalue_value_byte));
            dynasm!(ops
                ; .arch aarch64
                ; add X(owner), x13, x9
                ; ldrb w10, [X(owner)]
                ; cmp w10, upvalue_cell_type_tag
                ; b.ne =>miss
                ; add X(storage), X(owner), x16
            );
            if binding_requires_live_cell(semantics) {
                dynasm!(ops ; .arch aarch64 ; ldr x9, [X(storage)]);
                emit_load_u64(ops, 11, VALUE_HOLE);
                dynasm!(ops ; .arch aarch64 ; cmp x9, x11 ; b.eq =>miss);
            }
        }
        MachineBindingTarget::Global(otter_vm::jit::BindingHitProof::GlobalLexical {
            cell_offset,
            writable,
        }) => {
            if view.cage_base == 0 {
                return Err(Unsupported::OperandShape(
                    "scalar global lexical binding cage",
                ));
            }
            if matches!(semantics, BindingSemantics::Write(_)) && !writable {
                return Err(Unsupported::OperandShape("scalar writable lexical binding"));
            }
            let cell_addr = view.cage_base.checked_add(cell_offset as usize).ok_or(
                Unsupported::OperandShape("scalar global lexical binding cell"),
            )?;
            emit_load_symbolic_u64(
                ops,
                relocations,
                13,
                cell_addr as u64,
                RelocationTarget::GlobalLexicalCell {
                    function_id: view.code_block.id,
                    byte_pc,
                },
            );
            emit_load_u64(ops, 16, u64::from(upvalue_value_byte));
            dynasm!(ops
                ; .arch aarch64
                ; mov X(owner), x13
                ; add X(storage), x13, x16
            );
            if binding_requires_live_cell(semantics) {
                dynasm!(ops ; .arch aarch64 ; ldr x9, [X(storage)]);
                emit_load_u64(ops, 11, VALUE_HOLE);
                dynasm!(ops ; .arch aarch64 ; cmp x9, x11 ; b.eq =>miss);
            }
        }
        MachineBindingTarget::Global(otter_vm::jit::BindingHitProof::GlobalObject {
            shape,
            dictionary,
            value_byte,
            global_lexical_epoch,
            writable,
        }) => {
            if view.cage_base == 0 {
                return Err(Unsupported::OperandShape(
                    "scalar global object binding cage",
                ));
            }
            if matches!(semantics, BindingSemantics::Write(_)) && !writable {
                return Err(Unsupported::OperandShape("scalar writable object binding"));
            }
            dynasm!(ops
                ; .arch aarch64
                ; ldr x14, [x19, THREAD_OFFSET]
                ; ldr x14, [x14, VM_THREAD_GLOBAL_LEXICAL_EPOCH_CELL_OFFSET]
                ; cbz x14, =>miss
                ; ldr x15, [x14]
            );
            emit_load_u64(ops, 11, global_lexical_epoch);
            dynasm!(ops
                ; .arch aarch64
                ; cmp x15, x11
                ; b.ne =>miss
                ; ldr x14, [x19, GLOBAL_THIS_OFFSET_PTR_OFFSET]
                ; cbz x14, =>miss
                ; ldr w12, [x14]
            );
            emit_load_symbolic_u64(
                ops,
                relocations,
                14,
                view.cage_base as u64,
                RelocationTarget::GcCageBase,
            );
            dynasm!(ops
                ; .arch aarch64
                ; add x13, x14, x12
                ; mov X(owner), x13
                ; ldr w14, [x13, view.object_shape_byte]
            );
            if dictionary {
                dynasm!(ops
                    ; .arch aarch64
                    ; cbnz w14, =>miss
                    ; ldr x14, [x13, view.object_dictionary_shape_id_byte]
                );
                emit_load_u64(ops, 11, shape);
                dynasm!(ops ; .arch aarch64 ; cmp x14, x11 ; b.ne =>miss);
            } else {
                emit_load_u64(ops, 11, shape);
                dynasm!(ops ; .arch aarch64 ; cmp w14, w11 ; b.ne =>miss);
            }
            emit_slab_base(ops, view, 13, 14);
            emit_load_u64(ops, 16, u64::from(value_byte));
            dynasm!(ops
                ; .arch aarch64
                ; cbz x13, =>miss
                ; add X(storage), x13, x16
            );
        }
    }
    dynasm!(ops
        ; .arch aarch64
        ; mov W(condition), #1
        ; b =>done
        ; =>miss
        ; mov W(condition), wzr
        ; mov X(owner), xzr
        ; mov X(storage), xzr
        ; =>done
    );
    Ok(())
}

fn emit_binding_hit(
    ops: &mut dynasmrt::aarch64::Assembler,
    frame: MachineFrameLayout,
    semantics: BindingSemantics,
    target: MachineBindingTarget,
    locations: &[AllocatedLocation],
) -> Result<(), Unsupported> {
    if matches!(target, MachineBindingTarget::Cold)
        || !binding_target_matches_semantics(semantics, target)
    {
        return Err(Unsupported::OperandShape("scalar binding hit target"));
    }
    match semantics {
        BindingSemantics::Read(read) => {
            if locations.len() != 3 {
                return Err(Unsupported::OperandShape("scalar binding read hit"));
            }
            match read {
                BindingRead::GlobalThis { .. } => {
                    // The guard rebased the published compressed offset into
                    // the owner address; that address is the object Value.
                    // The storage word alone is a bare cage offset and must
                    // never reach a tagged home.
                    emit_load_allocated_integer(ops, frame, locations[0], 9, 0)?;
                }
                BindingRead::Exists { .. } => {
                    emit_load_u64(ops, 9, Value::boolean(true).to_bits());
                }
                BindingRead::Global { .. } | BindingRead::Upvalue { .. } => {
                    emit_load_allocated_integer(ops, frame, locations[1], 13, 0)?;
                    dynasm!(ops ; .arch aarch64 ; ldr x9, [x13]);
                }
                BindingRead::Dynamic { .. }
                | BindingRead::ShadowedUpvalue { .. }
                | BindingRead::EvalBindingSeq { .. } => {
                    return Err(Unsupported::OperandShape("scalar dynamic binding hit"));
                }
            }
            emit_store_allocated_tagged(ops, frame, locations[2], 9, 0)?;
        }
        BindingSemantics::Write(_) => {
            if locations.len() != 3 {
                return Err(Unsupported::OperandShape("scalar binding write hit"));
            }
            emit_load_allocated_integer(ops, frame, locations[1], 13, 0)?;
            emit_load_allocated_tagged(ops, frame, locations[2], 9, 0)?;
            dynasm!(ops ; .arch aarch64 ; str x9, [x13]);
        }
        BindingSemantics::Delete(_) => {
            return Err(Unsupported::OperandShape("scalar delete binding hit"));
        }
    }
    Ok(())
}

fn emit_save_committed_element_caller_bank(ops: &mut dynasmrt::aarch64::Assembler) {
    dynasm!(ops ; .arch aarch64 ; sub sp, sp, COMMITTED_ELEMENT_CALLER_SAVE_BYTES);
    for &(first, second, offset) in &COMMITTED_ELEMENT_GPR_SAVE_PAIRS {
        dynasm!(ops ; .arch aarch64 ; stp X(first), X(second), [sp, offset as i32]);
    }
    for &(first, second, offset) in &COMMITTED_ELEMENT_FP_SAVE_PAIRS {
        dynasm!(ops ; .arch aarch64 ; stp D(first), D(second), [sp, offset as i32]);
    }
}

fn emit_restore_committed_element_caller_bank(ops: &mut dynasmrt::aarch64::Assembler) {
    for &(first, second, offset) in &COMMITTED_ELEMENT_GPR_SAVE_PAIRS {
        dynasm!(ops ; .arch aarch64 ; ldp X(first), X(second), [sp, offset as i32]);
    }
    for &(first, second, offset) in &COMMITTED_ELEMENT_FP_SAVE_PAIRS {
        dynasm!(ops ; .arch aarch64 ; ldp D(first), D(second), [sp, offset as i32]);
    }
    dynasm!(ops ; .arch aarch64 ; add sp, sp, COMMITTED_ELEMENT_CALLER_SAVE_BYTES);
}

pub(super) fn emit(
    view: &JitCompileSnapshot,
    sequence: &InstructionSequence,
    allocation: &AllocatedSequence,
    frame: MachineFrameLayout,
    deopt_runtime: &DeoptRuntime,
    safepoints: &MachineSafepointTable,
    transitions: &TransitionTable,
    poll_entry: u64,
    deopt_writeback_entry: u64,
    deopt_stack_call_entry: u64,
    resolve_direct_entry: u64,
    try_prepare_construct_entry: u64,
    prepare_construct_entry: u64,
    derived_construct_result_entry: u64,
    copy_spread_arguments_entry: u64,
    initialize_upvalues_entry: u64,
    string_concat_entry: u64,
    array_construct_entry: u64,
    number_rem_entry: u64,
    number_pow_entry: u64,
    number_to_int32_entry: u64,
    strict_eq_entry: u64,
    to_boolean_entry: u64,
    load_element_entry: u64,
    store_element_entry: u64,
    load_property_entry: u64,
    store_property_entry: u64,
    call_method_value_entry: u64,
    call_with_this_value_entry: u64,
    construct_value_entry: u64,
    load_ic_cells: &mut [WhiskerIcCell],
    store_ic_cells: &mut [WhiskerIcCell],
    vm_register_count: u16,
    capture_artifacts: bool,
) -> Result<Emission, Unsupported> {
    reject_unimplemented_locations(sequence, allocation)?;
    let saved = SavedFrame::from_allocation(allocation);
    if frame.fixed_bytes() != saved.fixed_bytes() {
        return Err(Unsupported::OperandShape(
            "scalar Machine IR saved frame layout",
        ));
    }
    let mut ops = dynasmrt::aarch64::Assembler::new()
        .map_err(|_| Unsupported::Backend(crate::BackendFailure::AssemblerAllocation))?;
    let bail = ops.new_dynamic_label();
    let finish_error = ops.new_dynamic_label();
    let throw_value = ops.new_dynamic_label();
    let fatal = ops.new_dynamic_label();
    let shared_deopt = ops.new_dynamic_label();
    let deopt_labels = deopt_runtime
        .exits
        .iter()
        .map(|_| ops.new_dynamic_label())
        .collect::<Vec<_>>();
    let mut relocations = RelocationCapture::new(capture_artifacts);
    let mut constructor_field_regions = Vec::new();
    let mut structural_regions = Vec::new();
    let mut next_load_ic = 0usize;
    let mut next_store_ic = 0usize;
    let block_labels = sequence
        .blocks()
        .iter()
        .map(|_| ops.new_dynamic_label())
        .collect::<Vec<_>>();
    let mut osr_sites = Vec::new();
    for (index, instruction) in sequence.instructions().iter().enumerate() {
        let MachineOpcode::OsrEntry {
            logical_pc,
            ref inputs,
        } = instruction.opcode
        else {
            continue;
        };
        if inputs.len() != instruction.operands.len() {
            return Err(Unsupported::OperandShape("scalar OSR input arity"));
        }
        osr_sites.push(OsrSite {
            instruction: MachineInstructionId(index as u32),
            logical_pc,
            inputs: inputs.clone(),
            continuation: ops.new_dynamic_label(),
        });
    }
    let has_backedge_poll = sequence
        .instructions()
        .iter()
        .any(|instruction| instruction.opcode == MachineOpcode::BackedgePoll);

    emit_prologue(&mut ops, frame, saved);
    emit_clear_packed_double_view_caches(
        &mut ops,
        frame,
        sequence.packed_double_view_cache_count(),
    )?;
    if has_backedge_poll {
        dynasm!(ops ; .arch aarch64 ; movz w29, GENERATED_POLL_BATCH);
    }

    for (index, instruction) in sequence.instructions().iter().enumerate() {
        let id = MachineInstructionId(index as u32);
        let block_index = block_for_instruction(sequence, id)?;
        if sequence.blocks()[block_index].first == id {
            let label = block_labels[block_index];
            dynasm!(ops ; .arch aarch64 ; =>label);
        }
        emit_edits(
            &mut ops,
            allocation.edits(),
            AllocationPoint::Before(id),
            frame,
        )?;
        let is_terminator = instruction.control != super::super::ControlFlow::None;
        if is_terminator {
            emit_edits(
                &mut ops,
                allocation.edits(),
                AllocationPoint::After(id),
                frame,
            )?;
        }
        let locations = allocation
            .instruction_locations(id)
            .ok_or(Unsupported::OperandShape("scalar Machine IR locations"))?;
        match instruction.opcode {
            MachineOpcode::OsrEntry { .. } => {
                let site = osr_sites
                    .iter()
                    .find(|site| site.instruction == id)
                    .ok_or(Unsupported::OperandShape("scalar OSR continuation"))?;
                let continuation = site.continuation;
                dynasm!(ops ; .arch aarch64 ; =>continuation);
            }
            MachineOpcode::EntryValue(parameter) => {
                let destination = integer_register(locations[0])?;
                let offset = u32::from(parameter)
                    .checked_mul(8)
                    .ok_or(Unsupported::OperandShape("numeric parameter offset"))?;
                dynasm!(ops ; .arch aarch64 ; ldr X(destination), [x17, offset]);
            }
            MachineOpcode::EntryThis => {
                let destination = integer_register(locations[0])?;
                dynasm!(ops
                    ; .arch aarch64
                    ; ldr x16, [x19, NATIVE_FRAME_OFFSET]
                    ; ldr X(destination), [x16, NATIVE_FRAME_THIS_OFFSET]
                );
            }
            MachineOpcode::TryBindDerivedThis { byte_pc } => {
                let start = ops.offset().0;
                let miss = ops.new_dynamic_label();
                let done = ops.new_dynamic_label();
                let flags = otter_vm::native_abi::NativeFrameFlags::STACK_REGISTERS
                    | otter_vm::native_abi::NativeFrameFlags::DERIVED_CONSTRUCTOR;
                dynasm!(ops ; .arch aarch64
                    ; ldr x16, [x19, NATIVE_FRAME_OFFSET]
                    ; ldrb w15, [x16, crate::entry::NATIVE_FRAME_FLAGS_OFFSET]
                    ; movz w17, flags as u32
                    ; and w15, w15, w17
                    ; cmp w15, w17
                    ; b.ne =>miss
                    ; ldr x15, [x16, NATIVE_FRAME_THIS_OFFSET]
                );
                emit_load_u64(&mut ops, 17, Value::hole().to_bits());
                dynasm!(ops ; .arch aarch64
                    ; cmp x15, x17
                    ; b.ne =>miss
                    ; str x1, [x16, NATIVE_FRAME_THIS_OFFSET]
                    ; movz w0, #1
                    ; b =>done
                    ; =>miss
                    ; movz w0, #0
                    ; =>done
                );
                structural_regions.push((
                    "machineDerivedThisBindFast",
                    Some(byte_pc),
                    start,
                    ops.offset().0,
                ));
            }
            MachineOpcode::StringConstantCellLoad { byte_pc, target } => {
                let start = ops.offset().0;
                emit_load_symbolic_u64(
                    &mut ops,
                    &mut relocations,
                    13,
                    target.cell_addr as u64,
                    RelocationTarget::StringConstantCell {
                        function_id: view.code_block.id,
                        byte_pc,
                    },
                );
                dynasm!(ops ; .arch aarch64 ; ldr x9, [x13]);
                emit_store_allocated_tagged(&mut ops, frame, locations[0], 9, 0)?;
                structural_regions.push((
                    "machineStringConstantLoad",
                    Some(byte_pc),
                    start,
                    ops.offset().0,
                ));
            }
            MachineOpcode::BindingGuard {
                byte_pc,
                semantics,
                target,
            } => {
                let start = ops.offset().0;
                emit_binding_guard(
                    &mut ops,
                    &mut relocations,
                    view,
                    frame,
                    semantics,
                    target,
                    byte_pc,
                    locations,
                )?;
                structural_regions.push((
                    "machineBindingGuard",
                    Some(byte_pc),
                    start,
                    ops.offset().0,
                ));
            }
            MachineOpcode::BindingHit {
                byte_pc,
                semantics,
                target,
            } => {
                let start = ops.offset().0;
                emit_binding_hit(&mut ops, frame, semantics, target, locations)?;
                structural_regions.push((
                    "machineBindingHit",
                    Some(byte_pc),
                    start,
                    ops.offset().0,
                ));
            }
            MachineOpcode::BindingWriteBarrier => {
                if locations.len() != 2 {
                    return Err(Unsupported::OperandShape("scalar binding write barrier"));
                }
                emit_load_allocated_integer(&mut ops, frame, locations[0], 12, 0)?;
                emit_load_allocated_tagged(&mut ops, frame, locations[1], 9, 0)?;
                let done = ops.new_dynamic_label();
                emit_cell_test(&mut ops, 9, 11, CellTest::IsNotCell, done);
                emit_write_barrier_with_context(&mut ops, &mut relocations, view, 12, 9, 19);
                dynasm!(ops ; .arch aarch64 ; =>done);
            }
            MachineOpcode::BindingJoin { byte_pc, .. } => {
                let offset = ops.offset().0;
                structural_regions.push(("machineBindingJoin", Some(byte_pc), offset, offset));
            }
            MachineOpcode::TaggedConstant(bits) => {
                emit_load_u64(&mut ops, integer_register(locations[0])?, bits);
            }
            MachineOpcode::InlineCallGuard {
                function_id,
                this_mode,
            } => {
                let start = ops.offset().0;
                let miss = instruction_deopt_label(instruction.deopt, &deopt_labels)?;
                emit_load_allocated_tagged(&mut ops, frame, locations[0], 9, 0)?;
                crate::arm64::inline_guard::emit_inline_identity(&mut ops, view, function_id, miss);
                crate::arm64::inline_guard::emit_inline_this(
                    &mut ops,
                    view,
                    this_mode,
                    &mut relocations,
                    19,
                    miss,
                );
                emit_store_allocated_tagged(&mut ops, frame, locations[1], 12, 0)?;
                structural_regions.push(("machineInlineCallGuard", None, start, ops.offset().0));
            }
            MachineOpcode::DecodeNumber => {
                let miss = if instruction.deopt.is_some() {
                    instruction_deopt_label(instruction.deopt, &deopt_labels)?
                } else {
                    bail
                };
                emit_decode_number(
                    &mut ops,
                    integer_register(locations[0])?,
                    float_register(locations[1])?,
                    miss,
                );
            }
            MachineOpcode::DecodeInt32 => {
                let source = integer_register(locations[0])?;
                let destination = integer_register(locations[1])?;
                if source != destination {
                    return Err(Unsupported::OperandShape(
                        "numeric Int32 decode reuse allocation",
                    ));
                }
                let miss = if instruction.deopt.is_some() {
                    instruction_deopt_label(instruction.deopt, &deopt_labels)?
                } else {
                    bail
                };
                emit_decode_int32(&mut ops, source, miss);
            }
            MachineOpcode::FloatConstant(bits) => {
                emit_load_u64(&mut ops, 16, bits);
                let destination = float_register(locations[0])?;
                dynasm!(ops ; .arch aarch64 ; fmov D(destination), x16);
            }
            MachineOpcode::FloatAdd
            | MachineOpcode::FloatSub
            | MachineOpcode::FloatMul
            | MachineOpcode::FloatDiv => {
                let left = float_register(locations[0])?;
                let right = float_register(locations[1])?;
                let destination = float_register(locations[2])?;
                match instruction.opcode {
                    MachineOpcode::FloatAdd => {
                        dynasm!(ops ; .arch aarch64 ; fadd D(destination), D(left), D(right));
                    }
                    MachineOpcode::FloatSub => {
                        dynasm!(ops ; .arch aarch64 ; fsub D(destination), D(left), D(right));
                    }
                    MachineOpcode::FloatMul => {
                        dynasm!(ops ; .arch aarch64 ; fmul D(destination), D(left), D(right));
                    }
                    MachineOpcode::FloatDiv => {
                        dynasm!(ops ; .arch aarch64 ; fdiv D(destination), D(left), D(right));
                    }
                    _ => unreachable!("matched floating binary operation"),
                }
            }
            MachineOpcode::FloatRem | MachineOpcode::FloatPow => {
                if float_register(locations[0])? != 0
                    || float_register(locations[1])? != 1
                    || float_register(locations[2])? != 0
                {
                    return Err(Unsupported::OperandShape("numeric FP leaf ABI"));
                }
                let (entry, descriptor) = if instruction.opcode == MachineOpcode::FloatRem {
                    (number_rem_entry, STUB_NUMBER_REM_F64_LEAF)
                } else {
                    (number_pow_entry, STUB_NUMBER_POW_F64_LEAF)
                };
                emit_float_leaf_binary(&mut ops, &mut relocations, entry, descriptor);
            }
            MachineOpcode::Float64ToInt32 => {
                if float_register(locations[0])? != 0 || integer_register(locations[1])? != 0 {
                    return Err(Unsupported::OperandShape("numeric ToInt32 leaf ABI"));
                }
                emit_float_to_int32_leaf(&mut ops, &mut relocations, number_to_int32_entry);
            }
            MachineOpcode::CheckedFloat64ToElementIndex(_) => {
                let source = float_register(locations[0])?;
                let destination = integer_register(locations[1])?;
                let deopt = instruction_deopt_label(instruction.deopt, &deopt_labels)?;
                // `fcvtzu` saturates non-finite, negative, fractional, and
                // out-of-range values away from an exact round trip. Positive
                // zero and negative zero compare equal, which is the ordinary
                // Array property-key behavior. The following element bounds
                // check rejects every converted index outside the live prefix.
                dynasm!(ops
                    ; .arch aarch64
                    ; fcvtzu w16, D(source)
                    ; ucvtf d31, w16
                    ; fcmp D(source), d31
                    ; b.ne =>deopt
                    ; mov W(destination), w16
                );
            }
            MachineOpcode::FloatNeg => {
                let source = float_register(locations[0])?;
                let destination = float_register(locations[1])?;
                dynasm!(ops ; .arch aarch64 ; fneg D(destination), D(source));
            }
            MachineOpcode::FloatEqual
            | MachineOpcode::FloatNotEqual
            | MachineOpcode::FloatLessThan
            | MachineOpcode::FloatLessEqual
            | MachineOpcode::FloatGreaterThan
            | MachineOpcode::FloatGreaterEqual => {
                let left = float_register(locations[0])?;
                let right = float_register(locations[1])?;
                let destination = integer_register(locations[2])?;
                dynasm!(ops ; .arch aarch64 ; fcmp D(left), D(right));
                match instruction.opcode {
                    MachineOpcode::FloatEqual => {
                        dynasm!(ops ; .arch aarch64 ; cset W(destination), eq);
                    }
                    MachineOpcode::FloatNotEqual => {
                        dynasm!(ops ; .arch aarch64 ; cset W(destination), ne);
                    }
                    MachineOpcode::FloatLessThan => {
                        dynasm!(ops ; .arch aarch64 ; cset W(destination), mi);
                    }
                    MachineOpcode::FloatLessEqual => {
                        dynasm!(ops ; .arch aarch64 ; cset W(destination), ls);
                    }
                    MachineOpcode::FloatGreaterThan => {
                        dynasm!(ops ; .arch aarch64 ; cset W(destination), gt);
                    }
                    MachineOpcode::FloatGreaterEqual => {
                        dynasm!(ops ; .arch aarch64 ; cset W(destination), ge);
                    }
                    _ => unreachable!("matched Float64 comparison"),
                }
            }
            MachineOpcode::IntegerConstant(value) => {
                emit_load_u64(
                    &mut ops,
                    integer_register(locations[0])?,
                    value as u32 as u64,
                );
            }
            MachineOpcode::Int32ToFloat64 => {
                let source = integer_register(locations[0])?;
                let destination = float_register(locations[1])?;
                dynasm!(ops ; .arch aarch64 ; scvtf D(destination), W(source));
            }
            MachineOpcode::Uint32ToFloat64 => {
                let source = integer_register(locations[0])?;
                let destination = float_register(locations[1])?;
                dynasm!(ops ; .arch aarch64 ; ucvtf D(destination), W(source));
            }
            MachineOpcode::BooleanToInt32 => {
                let source = integer_register(locations[0])?;
                let destination = integer_register(locations[1])?;
                dynasm!(ops ; .arch aarch64 ; mov W(destination), W(source));
            }
            MachineOpcode::IntegerAdd | MachineOpcode::IntegerSub => {
                let left = integer_register(locations[0])?;
                let right = integer_register(locations[1])?;
                let destination = integer_register(locations[2])?;
                let exit = instruction_deopt_label(instruction.deopt, &deopt_labels)?;
                if instruction.opcode == MachineOpcode::IntegerAdd {
                    dynasm!(ops
                        ; .arch aarch64
                        ; adds W(destination), W(left), W(right)
                        ; b.vs =>exit
                    );
                } else {
                    dynasm!(ops
                        ; .arch aarch64
                        ; subs W(destination), W(left), W(right)
                        ; b.vs =>exit
                    );
                }
            }
            MachineOpcode::IntegerMul => {
                let left = integer_register(locations[0])?;
                let right = integer_register(locations[1])?;
                let destination = integer_register(locations[2])?;
                let exit = instruction_deopt_label(instruction.deopt, &deopt_labels)?;
                let nonzero = ops.new_dynamic_label();
                // A test-bit branch reaches only 32 KiB, so the negative-zero
                // exit goes through a local skip and an unconditional branch
                // that reaches the deopt trampolines of any body size.
                dynasm!(ops
                    ; .arch aarch64
                    ; smull x16, W(left), W(right)
                    ; sxtw x17, w16
                    ; cmp x16, x17
                    ; b.ne =>exit
                    ; cbnz w16, =>nonzero
                    ; eor w17, W(left), W(right)
                    ; tbz w17, #31, =>nonzero
                    ; b =>exit
                    ; =>nonzero
                    ; mov W(destination), w16
                );
            }
            MachineOpcode::IntegerNeg => {
                let source = integer_register(locations[0])?;
                let destination = integer_register(locations[1])?;
                let exit = instruction_deopt_label(instruction.deopt, &deopt_labels)?;
                dynasm!(ops
                    ; .arch aarch64
                    ; negs w16, W(source)
                    ; b.vs =>exit
                    ; cbz W(source), =>exit
                    ; mov W(destination), w16
                );
            }
            MachineOpcode::IntegerAddImmediate(immediate)
            | MachineOpcode::IntegerSubImmediate(immediate) => {
                let source = integer_register(locations[0])?;
                let destination = integer_register(locations[1])?;
                let exit = instruction_deopt_label(instruction.deopt, &deopt_labels)?;
                emit_load_u64(&mut ops, 16, immediate as u32 as u64);
                if matches!(instruction.opcode, MachineOpcode::IntegerAddImmediate(_)) {
                    dynasm!(ops
                        ; .arch aarch64
                        ; adds W(destination), W(source), w16
                        ; b.vs =>exit
                    );
                } else {
                    dynasm!(ops
                        ; .arch aarch64
                        ; subs W(destination), W(source), w16
                        ; b.vs =>exit
                    );
                }
            }
            MachineOpcode::IntegerAnd
            | MachineOpcode::IntegerOr
            | MachineOpcode::IntegerXor
            | MachineOpcode::IntegerShiftLeft
            | MachineOpcode::IntegerShiftRight => {
                let left = integer_register(locations[0])?;
                let right = integer_register(locations[1])?;
                let destination = integer_register(locations[2])?;
                match instruction.opcode {
                    MachineOpcode::IntegerAnd => {
                        dynasm!(ops ; .arch aarch64 ; and W(destination), W(left), W(right));
                    }
                    MachineOpcode::IntegerOr => {
                        dynasm!(ops ; .arch aarch64 ; orr W(destination), W(left), W(right));
                    }
                    MachineOpcode::IntegerXor => {
                        dynasm!(ops ; .arch aarch64 ; eor W(destination), W(left), W(right));
                    }
                    // AArch64 variable 32-bit shifts use only the low five count bits,
                    // exactly matching JavaScript's shift-count normalization.
                    MachineOpcode::IntegerShiftLeft => {
                        dynasm!(ops ; .arch aarch64 ; lsl W(destination), W(left), W(right));
                    }
                    MachineOpcode::IntegerShiftRight => {
                        dynasm!(ops ; .arch aarch64 ; asr W(destination), W(left), W(right));
                    }
                    _ => unreachable!("matched binary integer operation"),
                }
            }
            MachineOpcode::IntegerNot => {
                let source = integer_register(locations[0])?;
                let destination = integer_register(locations[1])?;
                dynasm!(ops ; .arch aarch64 ; mvn W(destination), W(source));
            }
            MachineOpcode::IntegerShiftRightLogical => {
                let left = integer_register(locations[0])?;
                let right = integer_register(locations[1])?;
                let destination = integer_register(locations[2])?;
                dynasm!(ops ; .arch aarch64 ; lsr W(destination), W(left), W(right));
            }
            MachineOpcode::IntegerAndImmediate(immediate) => {
                let source = integer_register(locations[0])?;
                let destination = integer_register(locations[1])?;
                emit_load_u64(&mut ops, 16, immediate as u32 as u64);
                dynasm!(ops ; .arch aarch64 ; and W(destination), W(source), w16);
            }
            MachineOpcode::IntegerLessThanImmediate(immediate)
            | MachineOpcode::IntegerEqualImmediate(immediate)
            | MachineOpcode::IntegerNotEqualImmediate(immediate) => {
                let source = integer_register(locations[0])?;
                let destination = integer_register(locations[1])?;
                emit_load_u64(&mut ops, 16, immediate as u32 as u64);
                dynasm!(ops ; .arch aarch64 ; cmp W(source), w16);
                match instruction.opcode {
                    MachineOpcode::IntegerLessThanImmediate(_) => {
                        dynasm!(ops ; .arch aarch64 ; cset W(destination), lt);
                    }
                    MachineOpcode::IntegerEqualImmediate(_) => {
                        dynasm!(ops ; .arch aarch64 ; cset W(destination), eq);
                    }
                    MachineOpcode::IntegerNotEqualImmediate(_) => {
                        dynasm!(ops ; .arch aarch64 ; cset W(destination), ne);
                    }
                    _ => unreachable!("matched immediate integer comparison"),
                }
            }
            MachineOpcode::IntegerEqual
            | MachineOpcode::IntegerNotEqual
            | MachineOpcode::IntegerLessThan
            | MachineOpcode::IntegerLessEqual
            | MachineOpcode::IntegerGreaterThan
            | MachineOpcode::IntegerGreaterEqual => {
                let left = integer_register(locations[0])?;
                let right = integer_register(locations[1])?;
                let destination = integer_register(locations[2])?;
                dynasm!(ops ; .arch aarch64 ; cmp W(left), W(right));
                match instruction.opcode {
                    MachineOpcode::IntegerEqual => {
                        dynasm!(ops ; .arch aarch64 ; cset W(destination), eq);
                    }
                    MachineOpcode::IntegerNotEqual => {
                        dynasm!(ops ; .arch aarch64 ; cset W(destination), ne);
                    }
                    MachineOpcode::IntegerLessThan => {
                        dynasm!(ops ; .arch aarch64 ; cset W(destination), lt);
                    }
                    MachineOpcode::IntegerLessEqual => {
                        dynasm!(ops ; .arch aarch64 ; cset W(destination), le);
                    }
                    MachineOpcode::IntegerGreaterThan => {
                        dynasm!(ops ; .arch aarch64 ; cset W(destination), gt);
                    }
                    MachineOpcode::IntegerGreaterEqual => {
                        dynasm!(ops ; .arch aarch64 ; cset W(destination), ge);
                    }
                    _ => unreachable!("matched int32 comparison"),
                }
            }
            MachineOpcode::IntegerToBoolean => {
                let source = integer_register(locations[0])?;
                let destination = integer_register(locations[1])?;
                dynasm!(ops
                    ; .arch aarch64
                    ; tst W(source), W(source)
                    ; cset W(destination), ne
                );
            }
            MachineOpcode::FloatToBoolean => {
                let source = float_register(locations[0])?;
                let destination = integer_register(locations[1])?;
                dynasm!(ops
                    ; .arch aarch64
                    ; fmov d31, xzr
                    ; fcmp D(source), d31
                    ; cset W(destination), ne
                    ; cset w16, vc
                    ; and W(destination), W(destination), w16
                );
            }
            MachineOpcode::BooleanNot => {
                let source = integer_register(locations[0])?;
                let destination = integer_register(locations[1])?;
                dynasm!(ops
                    ; .arch aarch64
                    ; mov w16, #1
                    ; eor W(destination), W(source), w16
                );
            }
            MachineOpcode::TruthinessProbe => {
                let start = ops.offset().0;
                truthiness::emit(
                    &mut ops,
                    &mut relocations,
                    view,
                    integer_register(locations[0])?,
                    integer_register(locations[1])?,
                    integer_register(locations[2])?,
                );
                structural_regions.push(("machineTruthinessProbe", None, start, ops.offset().0));
            }
            MachineOpcode::LooseEqualityProbe { byte_pc, equal } => {
                let start = ops.offset().0;
                loose_equality::emit(
                    &mut ops,
                    &mut relocations,
                    view,
                    [
                        integer_register(locations[0])?,
                        integer_register(locations[1])?,
                    ],
                    [
                        integer_register(locations[2])?,
                        integer_register(locations[3])?,
                    ],
                    equal,
                );
                structural_regions.push((
                    "machineLooseEqualityProbe",
                    Some(byte_pc),
                    start,
                    ops.offset().0,
                ));
            }
            MachineOpcode::TaggedNullishEqual { byte_pc, equal } => {
                let source = integer_register(locations[0])?;
                let destination = integer_register(locations[1])?;
                let deopt = instruction_deopt_label(instruction.deopt, &deopt_labels)?;
                let nullish = ops.new_dynamic_label();
                let done = ops.new_dynamic_label();
                let start = ops.offset().0;

                // The two immediate members of the nullish equivalence class
                // and every other non-cell complete without reentry. Among
                // cells only a native-function body can carry HTMLDDA, so
                // that one kind exits to the canonical comparison before the
                // allocated result is written; every other cell is
                // definitively not nullish.
                emit_load_u64(&mut ops, 16, VALUE_NULL);
                dynasm!(ops ; .arch aarch64 ; cmp X(source), x16 ; b.eq =>nullish);
                emit_load_u64(&mut ops, 16, VALUE_UNDEFINED);
                dynasm!(ops ; .arch aarch64 ; cmp X(source), x16 ; b.eq =>nullish);
                emit_html_dda_candidate_exit(
                    &mut ops,
                    &mut relocations,
                    view,
                    source,
                    16,
                    15,
                    deopt,
                );
                if equal {
                    dynasm!(ops ; .arch aarch64 ; mov W(destination), wzr);
                } else {
                    dynasm!(ops ; .arch aarch64 ; mov W(destination), #1);
                }
                dynasm!(ops ; .arch aarch64 ; b =>done ; =>nullish);
                if equal {
                    dynasm!(ops ; .arch aarch64 ; mov W(destination), #1);
                } else {
                    dynasm!(ops ; .arch aarch64 ; mov W(destination), wzr);
                }
                dynasm!(ops ; .arch aarch64 ; =>done);
                structural_regions.push((
                    "machineTaggedNullishEqual",
                    Some(byte_pc),
                    start,
                    ops.offset().0,
                ));
            }
            MachineOpcode::BackedgePoll => {
                let exit = instruction_deopt_label(instruction.deopt, &deopt_labels)?;
                emit_backedge_poll(
                    &mut ops,
                    &mut relocations,
                    poll_entry,
                    exit,
                    finish_error,
                    fatal,
                );
            }
            MachineOpcode::BoxNumber => emit_box_number(
                &mut ops,
                float_register(locations[0])?,
                integer_register(locations[1])?,
            ),
            MachineOpcode::BoxInt32 => {
                let source = integer_register(locations[0])?;
                let destination = integer_register(locations[1])?;
                dynasm!(ops
                    ; .arch aarch64
                    ; mov W(destination), W(source)
                    ; movz x16, NUMBER_TAG_HI16, lsl #48
                    ; orr X(destination), X(destination), x16
                );
            }
            MachineOpcode::BoxUint32 => emit_box_uint32(
                &mut ops,
                integer_register(locations[0])?,
                integer_register(locations[1])?,
            ),
            MachineOpcode::BoxBoolean => emit_box_boolean(
                &mut ops,
                integer_register(locations[0])?,
                integer_register(locations[1])?,
            ),
            MachineOpcode::PropertyLoad {
                site: ref property,
                exotic_length,
            } => {
                let byte_pc = property.byte_pc;
                let logical_pc = property.logical_pc;
                let site = safepoints
                    .site(id)
                    .filter(|site| instruction.safepoint == Some(site.id))
                    .ok_or(Unsupported::OperandShape("scalar property load safepoint"))?;
                let cell_ordinal = u32::try_from(next_load_ic)
                    .map_err(|_| Unsupported::OperandShape("scalar property load IC ordinal"))?;
                let cell = load_ic_cells
                    .get_mut(next_load_ic)
                    .ok_or(Unsupported::OperandShape("scalar property load IC cell"))?;
                cell.set_source(property.function_id, logical_pc);
                let cell_addr = std::ptr::from_mut::<WhiskerIcCell>(cell) as usize;
                next_load_ic += 1;
                let start = ops.offset().0;
                let done = ops.new_dynamic_label();
                let probe_cell = ops.new_dynamic_label();
                let runtime = ops.new_dynamic_label();

                // Dense-array and primitive-string `.length` values do not
                // live in an ordinary own-property slab, so no settled shape
                // chain can describe them. Try that shared exotic program
                // first. A non-exotic receiver then reloads the same late
                // allocator location for the ordinary settled proof below.
                if view.cage_base != 0 && exotic_length {
                    let have_length = ops.new_dynamic_label();
                    let not_length = ops.new_dynamic_label();
                    emit_load_allocated_tagged(&mut ops, frame, locations[0], 9, 0)?;
                    emit_exotic_length_fast(
                        &mut ops,
                        &mut relocations,
                        view,
                        have_length,
                        not_length,
                    );
                    dynasm!(ops ; .arch aarch64 ; =>have_length);
                    emit_store_allocated_tagged(&mut ops, frame, locations[1], 9, 0)?;
                    dynasm!(ops
                        ; .arch aarch64
                        ; b =>done
                        ; =>not_length
                    );
                }

                if view.cage_base != 0 && !property.program.is_empty() {
                    let chain = &property.program;
                    emit_settled_property_load(
                        &mut ops,
                        &mut relocations,
                        view,
                        chain,
                        |ops, target| {
                            emit_load_allocated_tagged(ops, frame, locations[0], target, 0)
                        },
                        probe_cell,
                    )?;
                    emit_store_allocated_tagged(&mut ops, frame, locations[1], 9, 0)?;
                    dynasm!(ops ; .arch aarch64 ; b =>done);
                }

                // A baked program is only the first way. Its miss walks the
                // code-owned dynamic cell so a newly observed shape becomes a
                // generated hit after one canonical runtime completion.
                dynasm!(ops ; .arch aarch64 ; =>probe_cell);
                if view.cage_base != 0 {
                    emit_property_ic_load(
                        &mut ops,
                        &mut relocations,
                        view,
                        None,
                        |ops, target| {
                            emit_load_allocated_tagged(ops, frame, locations[0], target, 0)
                        },
                        cell_addr,
                        cell_ordinal,
                        runtime,
                    )?;
                    emit_store_allocated_tagged(&mut ops, frame, locations[1], 9, 0)?;
                    dynasm!(ops ; .arch aarch64 ; b =>done);
                }

                // Canonical `[[Get]]` owns every observable effect. Save and
                // publish the exact roots only on this cold path; direct and
                // dynamic-cell hits pay no root-spill or call overhead.
                dynasm!(ops ; .arch aarch64 ; =>runtime);
                emit_clear_packed_double_view_caches(
                    &mut ops,
                    frame,
                    sequence.packed_double_view_cache_count(),
                )?;
                emit_save_safepoint_roots(&mut ops, frame, site)?;
                emit_publish_machine_roots(&mut ops, frame, site)?;
                emit_load_safepoint_root(
                    &mut ops,
                    frame,
                    site,
                    instruction.operands[0].value,
                    1,
                    MACHINE_ROOT_RECORD_SIZE,
                )?;
                emit_load_symbolic_u64(
                    &mut ops,
                    &mut relocations,
                    2,
                    cell_addr as u64,
                    RelocationTarget::PropertyIcCell {
                        access: PropertyIcAccess::Load,
                        ordinal: cell_ordinal,
                    },
                );
                dynasm!(ops
                    ; .arch aarch64
                    ; ldr x16, [x19, NATIVE_FRAME_OFFSET]
                    ; movz w15, logical_pc
                    ; str w15, [x16, NATIVE_FRAME_PC_OFFSET]
                    ; mov x0, x19
                );
                emit_load_symbolic_u64(
                    &mut ops,
                    &mut relocations,
                    16,
                    load_property_entry,
                    RelocationTarget::runtime_stub(STUB_JIT_LOAD_PROPERTY),
                );
                dynasm!(ops
                    ; .arch aarch64
                    ; blr x16
                    ; mov x17, x0
                    ; mov x15, x1
                );
                emit_clear_machine_roots(&mut ops);
                emit_reload_safepoint_roots(&mut ops, frame, site)?;
                dynasm!(ops
                    ; .arch aarch64
                    ; cbz x15, >property_load_completed
                    ; cmp x15, NativeResultStatus::Throw as u32
                    ; b.ne =>fatal
                    ; mov x0, x17
                    ; b =>throw_value
                    ; property_load_completed:
                );
                emit_store_allocated_tagged(&mut ops, frame, locations[1], 17, 0)?;
                dynasm!(ops ; .arch aarch64 ; =>done);
                structural_regions.push((
                    "machinePropertyLoad",
                    Some(byte_pc),
                    start,
                    ops.offset().0,
                ));
            }
            MachineOpcode::PropertyStore {
                site: ref property,
                value_is_non_cell,
            } => {
                let byte_pc = property.byte_pc;
                let logical_pc = property.logical_pc;
                let site = safepoints
                    .site(id)
                    .filter(|site| instruction.safepoint == Some(site.id))
                    .ok_or(Unsupported::OperandShape("scalar property store safepoint"))?;
                let cell_ordinal = u32::try_from(next_store_ic)
                    .map_err(|_| Unsupported::OperandShape("scalar property store IC ordinal"))?;
                let cell = store_ic_cells
                    .get_mut(next_store_ic)
                    .ok_or(Unsupported::OperandShape("scalar property store IC cell"))?;
                cell.set_source(property.function_id, logical_pc);
                let cell_addr = std::ptr::from_mut::<WhiskerIcCell>(cell) as usize;
                next_store_ic += 1;
                let start = ops.offset().0;
                let probe_cell = ops.new_dynamic_label();
                let commit = ops.new_dynamic_label();
                let runtime = ops.new_dynamic_label();
                let done = ops.new_dynamic_label();
                if view.cage_base != 0 && !property.program.is_empty() {
                    let chain = &property.program;
                    emit_settled_property_store_guard(
                        &mut ops,
                        &mut relocations,
                        view,
                        chain,
                        |ops, target| {
                            emit_load_allocated_tagged(ops, frame, locations[0], target, 0)
                        },
                        probe_cell,
                    )?;
                    dynasm!(ops ; .arch aarch64 ; mov w16, wzr ; b =>commit);
                }

                dynasm!(ops ; .arch aarch64 ; =>probe_cell);
                if view.cage_base != 0 {
                    emit_property_ic_store_guard(
                        &mut ops,
                        &mut relocations,
                        view,
                        None,
                        |ops, target| {
                            emit_load_allocated_tagged(ops, frame, locations[0], target, 0)
                        },
                        cell_addr,
                        cell_ordinal,
                        runtime,
                    )?;
                    dynasm!(ops ; .arch aarch64 ; b =>commit);
                } else {
                    dynasm!(ops ; .arch aarch64 ; b =>runtime);
                }

                // Both settled and dynamic-cell guards leave the same parent,
                // slab, and slot registers. Nothing after this label may miss.
                dynasm!(ops ; .arch aarch64 ; =>commit);
                // x16 carries the add-transition child shape. A large stack
                // operand may use x16 as address scratch, so save the token
                // around value materialization and account for the SP bias.
                dynasm!(ops ; .arch aarch64 ; str x16, [sp, #-16]!);
                emit_load_allocated_tagged(&mut ops, frame, locations[1], 9, 16)?;
                dynasm!(ops ; .arch aarch64 ; ldr x16, [sp], #16);
                if value_is_non_cell {
                    dynasm!(ops ; .arch aarch64 ; str x9, [x13, x17]);
                    emit_property_transition_shape_barrier(&mut ops, &mut relocations, view, 19);
                } else {
                    let primitive = ops.new_dynamic_label();
                    let committed = ops.new_dynamic_label();
                    emit_cell_test(&mut ops, 9, 11, CellTest::IsNotCell, primitive);
                    dynasm!(ops ; .arch aarch64 ; str x9, [x13, x17]);
                    emit_property_transition_shape_barrier(&mut ops, &mut relocations, view, 19);
                    emit_write_barrier_with_context(&mut ops, &mut relocations, view, 12, 9, 19);
                    dynasm!(ops
                        ; .arch aarch64
                        ; b =>committed
                        ; =>primitive
                        ; str x9, [x13, x17]
                    );
                    emit_property_transition_shape_barrier(&mut ops, &mut relocations, view, 19);
                    dynasm!(ops ; .arch aarch64 ; =>committed);
                }
                dynasm!(ops ; .arch aarch64 ; b =>done ; =>runtime);

                emit_clear_packed_double_view_caches(
                    &mut ops,
                    frame,
                    sequence.packed_double_view_cache_count(),
                )?;
                emit_save_safepoint_roots(&mut ops, frame, site)?;
                emit_publish_machine_roots(&mut ops, frame, site)?;
                emit_load_safepoint_root(
                    &mut ops,
                    frame,
                    site,
                    instruction.operands[0].value,
                    1,
                    MACHINE_ROOT_RECORD_SIZE,
                )?;
                emit_load_safepoint_root(
                    &mut ops,
                    frame,
                    site,
                    instruction.operands[1].value,
                    2,
                    MACHINE_ROOT_RECORD_SIZE,
                )?;
                emit_load_symbolic_u64(
                    &mut ops,
                    &mut relocations,
                    3,
                    cell_addr as u64,
                    RelocationTarget::PropertyIcCell {
                        access: PropertyIcAccess::Store,
                        ordinal: cell_ordinal,
                    },
                );
                dynasm!(ops
                    ; .arch aarch64
                    ; ldr x16, [x19, NATIVE_FRAME_OFFSET]
                    ; movz w15, logical_pc
                    ; str w15, [x16, NATIVE_FRAME_PC_OFFSET]
                    ; mov x0, x19
                );
                emit_load_symbolic_u64(
                    &mut ops,
                    &mut relocations,
                    16,
                    store_property_entry,
                    RelocationTarget::runtime_stub(STUB_JIT_STORE_PROPERTY),
                );
                dynasm!(ops
                    ; .arch aarch64
                    ; blr x16
                    ; mov x17, x0
                    ; mov x15, x1
                );
                emit_clear_machine_roots(&mut ops);
                emit_reload_safepoint_roots(&mut ops, frame, site)?;
                dynasm!(ops
                    ; .arch aarch64
                    ; cbz x15, =>done
                    ; cmp x15, NativeResultStatus::Throw as u32
                    ; b.ne =>fatal
                    ; mov x0, x17
                    ; b =>throw_value
                    ; =>done
                );
                structural_regions.push((
                    "machinePropertyStore",
                    Some(byte_pc),
                    start,
                    ops.offset().0,
                ));
            }
            MachineOpcode::PackedDoubleElementLoad { byte_pc, cache } => {
                let access =
                    element_access_for(view, byte_pc)
                        .copied()
                        .ok_or(Unsupported::OperandShape(
                            "scalar packed-double element load access",
                        ))?;
                if !super::semantics::packed_double_element_access_is_exact(&access) {
                    return Err(Unsupported::OperandShape(
                        "scalar packed-double element load representation",
                    ));
                }
                let index_value = instruction
                    .operands
                    .get(1)
                    .ok_or(Unsupported::OperandShape(
                        "scalar packed-double element load index",
                    ))?
                    .value;
                let index_form =
                    dense_index_form(sequence, index_value).ok_or(Unsupported::OperandShape(
                        "scalar packed-double element load index representation",
                    ))?;
                let deopt = instruction_deopt_label(instruction.deopt, &deopt_labels)?;
                let start = ops.offset().0;
                emit_packed_double_element_address(
                    &mut ops,
                    &mut relocations,
                    view,
                    frame,
                    &access,
                    cache,
                    |ops, target| emit_load_allocated_tagged(ops, frame, locations[0], target, 0),
                    |ops, target| emit_load_allocated_integer(ops, frame, locations[1], target, 0),
                    index_form,
                    deopt,
                )?;
                let destination = float_register(locations[2])?;
                dynasm!(ops ; .arch aarch64 ; ldr D(destination), [x16]);
                structural_regions.push((
                    "machinePackedDoubleElementLoad",
                    Some(byte_pc),
                    start,
                    ops.offset().0,
                ));
            }
            MachineOpcode::PackedDoubleElementStore { byte_pc, cache } => {
                let access =
                    element_access_for(view, byte_pc)
                        .copied()
                        .ok_or(Unsupported::OperandShape(
                            "scalar packed-double element store access",
                        ))?;
                if !super::semantics::packed_double_element_access_is_exact(&access) {
                    return Err(Unsupported::OperandShape(
                        "scalar packed-double element store representation",
                    ));
                }
                let index_value = instruction
                    .operands
                    .get(1)
                    .ok_or(Unsupported::OperandShape(
                        "scalar packed-double element store index",
                    ))?
                    .value;
                let index_form =
                    dense_index_form(sequence, index_value).ok_or(Unsupported::OperandShape(
                        "scalar packed-double element store index representation",
                    ))?;
                let deopt = instruction_deopt_label(instruction.deopt, &deopt_labels)?;
                let start = ops.offset().0;
                emit_packed_double_element_address(
                    &mut ops,
                    &mut relocations,
                    view,
                    frame,
                    &access,
                    cache,
                    |ops, target| emit_load_allocated_tagged(ops, frame, locations[0], target, 0),
                    |ops, target| emit_load_allocated_integer(ops, frame, locations[1], target, 0),
                    index_form,
                    deopt,
                )?;
                let source = float_register(locations[2])?;
                // This is the first and final effect. Every receiver, kind,
                // index, bounds, and base proof is complete before the store.
                dynasm!(ops ; .arch aarch64 ; str D(source), [x16]);
                structural_regions.push((
                    "machinePackedDoubleElementStore",
                    Some(byte_pc),
                    start,
                    ops.offset().0,
                ));
            }
            MachineOpcode::ElementLoad(byte_pc) => {
                let access = element_access_for(view, byte_pc)
                    .copied()
                    .ok_or(Unsupported::OperandShape("scalar element load access"))?;
                if locations.len() < 3 {
                    return Err(Unsupported::OperandShape(
                        "scalar committed element load locations",
                    ));
                }
                let index_value = instruction
                    .operands
                    .get(1)
                    .ok_or(Unsupported::OperandShape("scalar element load index"))?
                    .value;
                let index_form = dense_index_form(sequence, index_value).ok_or(
                    Unsupported::OperandShape("scalar element load index representation"),
                )?;
                let start = ops.offset().0;
                let fast_start = start;
                let cold = ops.new_dynamic_label();
                let done = ops.new_dynamic_label();
                emit_element_address(
                    &mut ops,
                    &mut relocations,
                    view,
                    &access,
                    |ops, target| emit_load_allocated_tagged(ops, frame, locations[0], target, 0),
                    |ops, target| emit_load_allocated_integer(ops, frame, locations[1], target, 0),
                    index_form,
                    cold,
                )?;
                emit_element_read(&mut ops, access.element, cold);
                emit_store_allocated_tagged(&mut ops, frame, locations[2], 9, 0)?;
                dynasm!(ops ; .arch aarch64 ; b =>done);
                let fast_end = ops.offset().0;

                dynasm!(ops ; .arch aarch64 ; =>cold);
                let cold_start = ops.offset().0;
                let arguments =
                    direct_committed_element_arguments(sequence, instruction, locations, true)?;
                let semantic_byte_pc = emit_committed_element_value_call(
                    &mut ops,
                    &mut relocations,
                    sequence,
                    frame,
                    instruction,
                    id,
                    safepoints,
                    deopt_runtime,
                    arguments,
                    STUB_JIT_LOAD_ELEMENT,
                    load_element_entry,
                    throw_value,
                    fatal,
                )?;
                if semantic_byte_pc != byte_pc {
                    return Err(Unsupported::OperandShape(
                        "scalar committed element load byte PC",
                    ));
                }
                emit_store_allocated_tagged(&mut ops, frame, locations[2], 17, 0)?;
                let cold_end = ops.offset().0;
                dynasm!(ops ; .arch aarch64 ; =>done);
                let end = ops.offset().0;
                structural_regions.push(("machineElementLoad", Some(byte_pc), start, end));
                structural_regions.push((
                    "machineElementLoadFast",
                    Some(byte_pc),
                    fast_start,
                    fast_end,
                ));
                structural_regions.push((
                    "machineElementLoadCold",
                    Some(byte_pc),
                    cold_start,
                    cold_end,
                ));
            }
            MachineOpcode::ClearPackedDoubleViewCaches(_) => {
                let start = ops.offset().0;
                emit_clear_packed_double_view_caches(
                    &mut ops,
                    frame,
                    sequence.packed_double_view_cache_count(),
                )?;
                structural_regions.push((
                    "machinePackedDoubleViewCacheClear",
                    None,
                    start,
                    ops.offset().0,
                ));
            }
            MachineOpcode::ElementStore(byte_pc) => {
                let access = element_access_for(view, byte_pc)
                    .copied()
                    .ok_or(Unsupported::OperandShape("scalar element store access"))?;
                if locations.len() < 3 {
                    return Err(Unsupported::OperandShape(
                        "scalar committed element store locations",
                    ));
                }
                let index_value = instruction
                    .operands
                    .get(1)
                    .ok_or(Unsupported::OperandShape("scalar element store index"))?
                    .value;
                let index_form = dense_index_form(sequence, index_value).ok_or(
                    Unsupported::OperandShape("scalar element store index representation"),
                )?;
                let value = instruction
                    .operands
                    .get(2)
                    .ok_or(Unsupported::OperandShape("scalar element store value"))?;
                let value_representation = sequence
                    .representations()
                    .get(value.value.0 as usize)
                    .copied()
                    .ok_or(Unsupported::OperandShape(
                        "scalar element store value representation",
                    ))?;
                let start = ops.offset().0;
                let fast_start = start;
                let cold = ops.new_dynamic_label();
                let done = ops.new_dynamic_label();
                // The address/hole program owns every caller-saved guard
                // scratch. Preserve the fast-path value in non-allocatable
                // x17 before entering it; the cold sibling deliberately
                // ignores this copy and reloads the moving canonical root.
                emit_load_allocated_tagged(&mut ops, frame, locations[2], 17, 0)?;
                emit_element_address(
                    &mut ops,
                    &mut relocations,
                    view,
                    &access,
                    |ops, target| emit_load_allocated_tagged(ops, frame, locations[0], target, 0),
                    |ops, target| emit_load_allocated_integer(ops, frame, locations[1], target, 0),
                    index_form,
                    cold,
                )?;
                // A boxed hole is an absent property, so both reads and writes
                // must prove that the indexed slot already exists before the
                // generated store can commit its first effect.
                emit_element_read(&mut ops, access.element, cold);
                if value_representation != MachineRepresentation::Tagged {
                    return Err(Unsupported::OperandShape(
                        "scalar element store tagged value",
                    ));
                }
                dynasm!(ops ; .arch aarch64 ; mov x9, x17);
                emit_element_write(&mut ops, access.element, cold);
                dynasm!(ops ; .arch aarch64 ; b =>done);
                let fast_end = ops.offset().0;

                dynasm!(ops ; .arch aarch64 ; =>cold);
                let cold_start = ops.offset().0;
                let arguments =
                    direct_committed_element_arguments(sequence, instruction, locations, false)?;
                let semantic_byte_pc = emit_committed_element_value_call(
                    &mut ops,
                    &mut relocations,
                    sequence,
                    frame,
                    instruction,
                    id,
                    safepoints,
                    deopt_runtime,
                    arguments,
                    STUB_JIT_STORE_ELEMENT,
                    store_element_entry,
                    throw_value,
                    fatal,
                )?;
                if semantic_byte_pc != byte_pc {
                    return Err(Unsupported::OperandShape(
                        "scalar committed element store byte PC",
                    ));
                }
                let cold_end = ops.offset().0;
                dynasm!(ops ; .arch aarch64 ; =>done);
                let end = ops.offset().0;
                structural_regions.push(("machineElementStore", Some(byte_pc), start, end));
                structural_regions.push((
                    "machineElementStoreFast",
                    Some(byte_pc),
                    fast_start,
                    fast_end,
                ));
                structural_regions.push((
                    "machineElementStoreCold",
                    Some(byte_pc),
                    cold_start,
                    cold_end,
                ));
            }
            MachineOpcode::ConstructorFieldStore(byte_pc) => {
                let transition = view
                    .constructor_field_transitions
                    .get(&byte_pc)
                    .ok_or(Unsupported::OperandShape("constructor field transition"))?;
                let deopt = instruction_deopt_label(instruction.deopt, &deopt_labels)?;
                let start = ops.offset().0;
                // x9-x16 are emitter scratch registers, and regalloc may also
                // place the field value in x9. Preserve the early-use value
                // in the non-allocatable x17 before the receiver guards
                // overwrite any allocator-owned scratch home.
                emit_load_allocated_tagged(&mut ops, frame, locations[1], 17, 0)?;
                emit_load_allocated_tagged(&mut ops, frame, locations[0], 9, 0)?;
                dynasm!(ops
                    ; .arch aarch64
                    ; movz x11, NUMBER_TAG_HI16, lsl #48
                    ; orr x11, x11, #0x2
                    ; tst x9, x11
                    ; b.ne =>deopt
                    ; mov w11, w9
                );
                emit_load_symbolic_u64(
                    &mut ops,
                    &mut relocations,
                    12,
                    view.cage_base as u64,
                    RelocationTarget::GcCageBase,
                );
                dynasm!(ops
                    ; .arch aarch64
                    ; add x13, x12, x11
                    ; ldrb w16, [x13]
                    ; cmp w16, OBJECT_BODY_TYPE_TAG
                    ; b.ne =>deopt
                    ; ldr w16, [x13, view.object_shape_byte]
                );
                emit_load_u64(&mut ops, 15, u64::from(transition.from_shape));
                dynasm!(ops
                    ; .arch aarch64
                    ; cmp w16, w15
                    ; b.ne =>deopt
                    // An intervening call in a non-simple constructor may
                    // freeze `this` after the transition plan was baked.
                    // Adding an own field to a non-extensible receiver would
                    // bypass the canonical StoreProperty semantics, so prove
                    // the live flag before any shape/length mutation.
                    ; ldrb w16, [x13, view.object_extensible_byte]
                    ; cbz w16, =>deopt
                    ; ldrh w16, [x13, view.object_slab_len_byte]
                    ; cmp w16, transition.slot as u32
                    ; b.ne =>deopt
                );
                // The appended slot must already own a word. Receiver
                // preparation normally reserves the whole transition program's
                // capacity, but a receiver can reach this body from any
                // construct path — a foreign `new.target`, an interpreter
                // construct, a prepared receiver whose program was cut short —
                // so the live storage is proved here, before any mutation: a
                // spilled slab must hold the slot below its capacity and an
                // inline object must still have an inline word. Anything else
                // deopts to the canonical store, which grows the slab.
                let storage_fits = ops.new_dynamic_label();
                dynasm!(ops
                    ; .arch aarch64
                    ; ldr w16, [x13, view.object_slab_handle_byte]
                    ; cbz w16, >inline_storage
                    ; add x14, x12, x16
                    ; ldr w14, [x14, view.object_slab_capacity_byte]
                    ; cmp w14, transition.slot as u32
                    ; b.ls =>deopt
                    ; b =>storage_fits
                    ; inline_storage:
                );
                if u32::from(transition.slot) >= view.object_inline_slot_cap {
                    dynasm!(ops ; .arch aarch64 ; b =>deopt);
                }
                dynasm!(ops ; .arch aarch64 ; =>storage_fits);
                for &prototype_shape in &transition.prototype_shapes {
                    dynasm!(ops
                        ; .arch aarch64
                        ; ldr w11, [x13, view.jit_proto_byte]
                        ; cbz w11, =>deopt
                        ; add x13, x12, x11
                        ; ldrb w16, [x13]
                        ; cmp w16, OBJECT_BODY_TYPE_TAG
                        ; b.ne =>deopt
                        ; ldr w16, [x13, view.object_shape_byte]
                    );
                    emit_load_u64(&mut ops, 15, u64::from(prototype_shape));
                    dynasm!(ops ; .arch aarch64 ; cmp w16, w15 ; b.ne =>deopt);
                }
                dynasm!(ops
                    ; .arch aarch64
                    ; ldr w11, [x13, view.jit_proto_byte]
                    ; cbnz w11, =>deopt
                );

                // Recompute the receiver after walking the prototype chain;
                // every observable guard precedes the first mutation.
                emit_load_allocated_tagged(&mut ops, frame, locations[0], 9, 0)?;
                dynasm!(ops ; .arch aarch64 ; mov w11, w9 ; add x13, x12, x11);
                emit_load_u64(&mut ops, 14, u64::from(transition.to_shape));
                dynasm!(ops
                    ; .arch aarch64
                    ; str w14, [x13, view.object_shape_byte]
                    ; mov w15, transition.slot as u32 + 1
                    ; strh w15, [x13, view.object_slab_len_byte]
                    ; mov x12, x13
                );
                if transition.slot == 0 {
                    emit_initialize_inline_values_ptr(&mut ops, view, 13, 16);
                }
                dynasm!(ops ; .arch aarch64 ; mov x10, x17);
                dynasm!(ops ; .arch aarch64 ; mov x13, x12);
                emit_slab_base(&mut ops, view, 13, 14);
                let slot_byte = u32::from(transition.slot) * 8;
                dynasm!(ops
                    ; .arch aarch64
                    ; str x10, [x13, slot_byte]
                    ; sub sp, sp, #16
                    ; stp x12, x10, [sp]
                );
                // The barrier implementation owns x14-x16 as scratch, so the
                // raw shape handle must live in a disjoint argument register
                // if marking makes this call take its slow sibling.
                emit_load_u64(&mut ops, 3, u64::from(transition.to_shape));
                emit_write_barrier_with_context(&mut ops, &mut relocations, view, 12, 3, 19);
                dynasm!(ops ; .arch aarch64 ; ldp x12, x10, [sp] ; add sp, sp, #16);
                let value_barrier_done = ops.new_dynamic_label();
                emit_cell_test(&mut ops, 10, 14, CellTest::IsNotCell, value_barrier_done);
                emit_write_barrier_with_context(&mut ops, &mut relocations, view, 12, 10, 19);
                dynasm!(ops ; .arch aarch64 ; =>value_barrier_done);
                constructor_field_regions.push((byte_pc, start, ops.offset().0));
            }
            MachineOpcode::Return => {
                let source = integer_register(locations[0])?;
                dynasm!(ops
                    ; .arch aarch64
                    ; mov x0, X(source)
                    ; movz x1, NativeResultStatus::Success as u32
                );
                emit_epilogue(&mut ops, frame, saved);
            }
            MachineOpcode::Jump => {
                let block = &sequence.blocks()[block_index];
                let successor = block
                    .successors
                    .iter()
                    .copied()
                    .find(|&successor| !is_exceptional_successor(sequence, block_index, successor))
                    .ok_or(Unsupported::OperandShape("numeric jump successors"))?;
                let target = block_labels[successor.0 as usize];
                dynasm!(ops ; .arch aarch64 ; b =>target);
            }
            MachineOpcode::BranchIf(when_true) => {
                let condition = integer_register(locations[0])?;
                let block = &sequence.blocks()[block_index];
                let [taken, fallthrough] = block.successors.as_slice() else {
                    return Err(Unsupported::OperandShape("numeric branch successors"));
                };
                let taken = block_labels[taken.0 as usize];
                let fallthrough = block_labels[fallthrough.0 as usize];
                if when_true {
                    dynasm!(ops ; .arch aarch64 ; cbnz W(condition), =>taken);
                } else {
                    dynasm!(ops ; .arch aarch64 ; cbz W(condition), =>taken);
                }
                dynasm!(ops ; .arch aarch64 ; b =>fallthrough);
            }
            MachineOpcode::BranchNativeStatus => {
                let status = integer_register(locations[0])?;
                let throw_status = NativeResultStatus::Throw as u32;
                let block = &sequence.blocks()[block_index];
                let [success, throw, fatal_target] = block.successors.as_slice() else {
                    return Err(Unsupported::OperandShape("scalar native-status successors"));
                };
                let success = block_labels[success.0 as usize];
                let throw = block_labels[throw.0 as usize];
                let fatal_target = block_labels[fatal_target.0 as usize];
                emit_load_u64(&mut ops, 16, u64::from(throw_status));
                dynasm!(ops
                    ; .arch aarch64
                    ; cbz X(status), =>success
                    ; cmp X(status), x16
                    ; b.eq =>throw
                    ; b =>fatal_target
                );
            }
            MachineOpcode::Throw => {
                let exception = integer_register(locations[0])?;
                dynasm!(ops ; .arch aarch64 ; mov x0, X(exception) ; b =>throw_value);
            }
            MachineOpcode::Fatal => {
                dynasm!(ops ; .arch aarch64 ; b =>fatal);
            }
            MachineOpcode::Call(descriptor_index) => {
                let descriptor = sequence
                    .call_descriptors()
                    .get(descriptor_index as usize)
                    .ok_or(Unsupported::OperandShape("scalar call descriptor"))?;
                if let CallTarget::Direct {
                    kind,
                    argument_mode,
                    candidates,
                    caller_function_id,
                    logical_pc,
                    byte_pc,
                } = &descriptor.target
                {
                    let site = safepoints
                        .site(id)
                        .filter(|site| instruction.safepoint == Some(site.id))
                        .ok_or(Unsupported::OperandShape("scalar direct call safepoint"))?;
                    let deopt = instruction_deopt_label(instruction.deopt, &deopt_labels)?;
                    let direct_done = ops.new_dynamic_label();
                    let direct_threw = ops.new_dynamic_label();
                    let direct_bail = ops.new_dynamic_label();
                    let final_method_guard_miss = ops.new_dynamic_label();
                    // Any generated JavaScript call can reenter and collect.
                    // Raw packed-double view caches are neither roots nor
                    // stable across that boundary, so invalidate them before
                    // the root record changes SP.
                    emit_clear_packed_double_view_caches(
                        &mut ops,
                        frame,
                        sequence.packed_double_view_cache_count(),
                    )?;
                    emit_save_safepoint_roots(&mut ops, frame, site)?;
                    emit_publish_machine_roots(&mut ops, frame, site)?;
                    let result_index = descriptor.arguments.len();
                    // An explicit-receiver call carries its receiver as
                    // operand one; the linkage takes it through the call form
                    // rather than the argument list.
                    let first_argument = if *kind == DirectCallKind::CallWithThis {
                        2
                    } else {
                        1
                    };
                    let call_with_this_guard_miss = ops.new_dynamic_label();
                    if *kind == DirectCallKind::Forward {
                        let start = ops.offset().0;
                        forward_call::emit(
                            &mut ops,
                            &mut relocations,
                            view,
                            transitions,
                            instruction,
                            frame,
                            site,
                            locations,
                            result_index,
                            *logical_pc,
                            *byte_pc,
                            call_with_this_guard_miss,
                            finish_error,
                            direct_threw,
                            fatal,
                            direct_done,
                        )?;
                        structural_regions.push((
                            "machineForwardCall",
                            Some(*byte_pc),
                            start,
                            ops.offset().0,
                        ));
                    }
                    let arguments = (first_argument..result_index)
                        .map(|index| {
                            u16::try_from(index).map_err(|_| {
                                Unsupported::OperandShape("scalar direct call argument count")
                            })
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    let arguments = match argument_mode {
                        DirectCallArgumentMode::Fixed => DirectCallArguments::Fixed(&arguments),
                        DirectCallArgumentMode::Spread => DirectCallArguments::Spread(1),
                    };
                    for (candidate_index, candidate) in candidates.iter().enumerate() {
                        let candidate_start = ops.offset().0;
                        let next_method_candidate = (*kind == DirectCallKind::Method
                            && candidate_index + 1 != candidates.len())
                        .then(|| ops.new_dynamic_label());
                        let method_guard_miss =
                            next_method_candidate.unwrap_or(final_method_guard_miss);
                        let form = match kind {
                            DirectCallKind::Forward => {
                                return Err(Unsupported::OperandShape(
                                    "forward call has no baked candidates",
                                ));
                            }
                            DirectCallKind::Plain => DirectCallForm::Plain { callable: 0 },
                            DirectCallKind::CallWithThis => DirectCallForm::CallWithThis {
                                callable: 0,
                                receiver: 1,
                            },
                            DirectCallKind::Method => {
                                let guard = candidate.guard.as_ref().ok_or(
                                    Unsupported::OperandShape("scalar method candidate guard"),
                                )?;
                                let receiver = instruction
                                    .operands
                                    .first()
                                    .ok_or(Unsupported::OperandShape("scalar method receiver"))?;
                                let guard_start = ops.offset().0;
                                // The ordinary receiver input is an early use,
                                // while its TaggedRoot metadata is late. Root
                                // publication uses caller-saved scratch between
                                // those allocation points, so only the canonical
                                // published save home is valid here.
                                emit_load_safepoint_root(
                                    &mut ops,
                                    frame,
                                    site,
                                    receiver.value,
                                    9,
                                    MACHINE_ROOT_RECORD_SIZE,
                                )?;
                                emit_method_guard_from_tagged_register(
                                    &mut ops,
                                    &mut relocations,
                                    view,
                                    guard,
                                    9,
                                    17,
                                    None,
                                    true,
                                    method_guard_miss,
                                )?;
                                structural_regions.push((
                                    "machineDirectMethodGuard",
                                    Some(*byte_pc),
                                    guard_start,
                                    ops.offset().0,
                                ));
                                DirectCallForm::Method {
                                    callable: 17,
                                    receiver: 0,
                                }
                            }
                            DirectCallKind::Construct => DirectCallForm::Construct {
                                callable: 0,
                                receiver: u16::try_from(result_index + 1).map_err(|_| {
                                    Unsupported::OperandShape("scalar construct receiver root")
                                })?,
                            },
                            DirectCallKind::DerivedConstruct => {
                                DirectCallForm::DerivedConstruct { callable: 0 }
                            }
                            DirectCallKind::SuperConstruct => DirectCallForm::SuperConstruct {
                                callable: 0,
                                receiver: u16::try_from(result_index + 1).map_err(|_| {
                                    Unsupported::OperandShape("scalar super receiver root")
                                })?,
                            },
                            DirectCallKind::DerivedSuperConstruct => {
                                DirectCallForm::DerivedSuperConstruct { callable: 0 }
                            }
                        };
                        emit_direct_call_with_access(
                            &mut ops,
                            &mut relocations,
                            view,
                            DirectCallSite {
                                target: &candidate.callee,
                                target_index: candidate.target_index,
                                target_count: candidate.target_count,
                                caller_function_id: *caller_function_id,
                                logical_pc: *logical_pc,
                                byte_pc: *byte_pc,
                                dst: u16::try_from(result_index).map_err(|_| {
                                    Unsupported::OperandShape("scalar direct call result")
                                })?,
                                form,
                                arguments,
                            },
                            deopt_stack_call_entry,
                            resolve_direct_entry,
                            try_prepare_construct_entry,
                            prepare_construct_entry,
                            derived_construct_result_entry,
                            copy_spread_arguments_entry,
                            initialize_upvalues_entry,
                            0,
                            None,
                            if *kind == DirectCallKind::CallWithThis {
                                call_with_this_guard_miss
                            } else {
                                direct_bail
                            },
                            finish_error,
                            direct_threw,
                            fatal,
                            direct_done,
                            19,
                            |ops, source, target, sp_bias| {
                                let operand = instruction.operands.get(usize::from(source)).ok_or(
                                    Unsupported::OperandShape("scalar direct call source"),
                                )?;
                                // A moving safepoint rewrites the canonical save
                                // slot named by the late TaggedRoot metadata. Read
                                // that slot directly after receiver preparation;
                                // its allocator early/late homes may differ and
                                // either register may have been clobbered meanwhile.
                                if let Some(root) =
                                    site.roots.iter().find(|root| root.value == operand.value)
                                {
                                    let offset = root_offset(frame, root.save_slot)?
                                        .checked_add(sp_bias)
                                        .and_then(|offset| {
                                            offset.checked_add(MACHINE_ROOT_RECORD_SIZE)
                                        })
                                        .ok_or(Unsupported::OperandShape(
                                            "scalar direct call canonical root offset",
                                        ))?;
                                    emit_sp_ldr_x(ops, target, offset);
                                    return Ok(());
                                }
                                let location = locations[usize::from(source)];
                                emit_load_allocated_tagged(
                                    ops,
                                    frame,
                                    location,
                                    target,
                                    sp_bias.checked_add(MACHINE_ROOT_RECORD_SIZE).ok_or(
                                        Unsupported::OperandShape("scalar direct call stack bias"),
                                    )?,
                                )
                            },
                            |ops, destination, source, sp_bias| {
                                let location = *locations.get(usize::from(destination)).ok_or(
                                    Unsupported::OperandShape("scalar direct call destination"),
                                )?;
                                emit_store_allocated_tagged(ops, frame, location, source, sp_bias)
                            },
                            |ops| {
                                // x17 is outside regalloc2's allocatable bank and
                                // survives the activation-root descriptor cleanup.
                                dynasm!(ops ; .arch aarch64 ; mov x17, x0);
                                emit_clear_machine_roots(ops);
                                emit_reload_safepoint_roots(ops, frame, site)?;
                                dynasm!(ops ; .arch aarch64 ; mov x0, x17);
                                Ok(())
                            },
                            |ops, source, sp_bias| {
                                let receiver = instruction
                                    .operands
                                    .get(result_index + 1)
                                    .ok_or(Unsupported::OperandShape(
                                        "scalar construct receiver operand",
                                    ))?
                                    .value;
                                let root = site
                                    .roots
                                    .iter()
                                    .find(|root| root.value == receiver)
                                    .ok_or(Unsupported::OperandShape(
                                        "scalar construct receiver save home",
                                    ))?;
                                let offset = root_offset(frame, root.save_slot)?
                                    .checked_add(sp_bias)
                                    .ok_or(Unsupported::OperandShape(
                                        "scalar construct linkage root offset",
                                    ))?
                                    .checked_add(MACHINE_ROOT_RECORD_SIZE)
                                    .ok_or(Unsupported::OperandShape(
                                        "scalar construct receiver root offset",
                                    ))?;
                                dynasm!(ops ; .arch aarch64 ; str X(source), [sp, offset]);
                                Ok(())
                            },
                            |ops, sp_bias| {
                                emit_reload_safepoint_roots_with_bias(
                                    ops,
                                    frame,
                                    site,
                                    sp_bias.checked_add(MACHINE_ROOT_RECORD_SIZE).ok_or(
                                        Unsupported::OperandShape(
                                            "scalar construct root reload bias",
                                        ),
                                    )?,
                                )
                            },
                            |_, _, _| {
                                Err(Unsupported::OperandShape(
                                    "Machine forwarding operands not lowered",
                                ))
                            },
                        )?;
                        if *kind == DirectCallKind::Method {
                            structural_regions.push((
                                "machineDirectMethodCandidate",
                                Some(*byte_pc),
                                candidate_start,
                                ops.offset().0,
                            ));
                        }
                        if let Some(next_method_candidate) = next_method_candidate {
                            dynasm!(ops ; .arch aarch64 ; =>next_method_candidate);
                        }
                    }
                    if matches!(
                        kind,
                        DirectCallKind::Method
                            | DirectCallKind::CallWithThis
                            | DirectCallKind::Forward
                    ) || (*kind == DirectCallKind::Construct && candidates.is_empty())
                    {
                        if *kind == DirectCallKind::Method {
                            dynasm!(ops ; .arch aarch64 ; =>final_method_guard_miss);
                        } else if matches!(
                            kind,
                            DirectCallKind::CallWithThis | DirectCallKind::Forward
                        ) {
                            // A candidate reaches the loop's end with its
                            // roots still published. The linkage's guard miss
                            // restored the allocator registers and cleared
                            // the record, so it publishes again before the
                            // generic call.
                            let generic_ready = ops.new_dynamic_label();
                            dynasm!(ops
                                ; .arch aarch64
                                ; b =>generic_ready
                                ; =>call_with_this_guard_miss
                            );
                            emit_save_safepoint_roots(&mut ops, frame, site)?;
                            emit_publish_machine_roots(&mut ops, frame, site)?;
                            dynasm!(ops ; .arch aarch64 ; =>generic_ready);
                        }
                        if *kind == DirectCallKind::Forward {
                            forward_call::emit_cold_source_admission(
                                &mut ops,
                                &mut relocations,
                                transitions,
                                instruction,
                                frame,
                                site,
                                direct_bail,
                            )?;
                        }
                        let (generic_entry, generic_stub, generic_region) = match kind {
                            DirectCallKind::Forward => (
                                transitions
                                    .entry(otter_vm::native_abi::STUB_JIT_CALL_FORWARD_ARGUMENTS),
                                otter_vm::native_abi::STUB_JIT_CALL_FORWARD_ARGUMENTS,
                                "machineGenericForwardCall",
                            ),
                            DirectCallKind::Method => (
                                call_method_value_entry,
                                STUB_JIT_CALL_METHOD_VALUE,
                                "machineGenericMethodCall",
                            ),
                            DirectCallKind::Construct => (
                                construct_value_entry,
                                STUB_JIT_CONSTRUCT_VALUE,
                                "machineGenericConstruct",
                            ),
                            _ => (
                                call_with_this_value_entry,
                                STUB_JIT_CALL_WITH_THIS_VALUE,
                                "machineGenericCallWithThis",
                            ),
                        };
                        if *argument_mode == DirectCallArgumentMode::Fixed {
                            let generic_start = ops.offset().0;
                            emit_value_span_arguments(
                                &mut ops,
                                sequence,
                                frame,
                                site,
                                instruction.operands[..result_index]
                                    .iter()
                                    .map(|operand| operand.value),
                            )?;
                            emit_load_u64(&mut ops, 15, u64::from(*logical_pc));
                            dynasm!(ops
                                ; .arch aarch64
                                ; ldr x16, [x19, NATIVE_FRAME_OFFSET]
                                ; str w15, [x16, NATIVE_FRAME_PC_OFFSET]
                                ; mov x0, x19
                            );
                            emit_load_symbolic_u64(
                                &mut ops,
                                &mut relocations,
                                16,
                                generic_entry,
                                RelocationTarget::runtime_stub(generic_stub),
                            );
                            dynasm!(ops
                                ; .arch aarch64
                                ; blr x16
                                ; mov x17, x0
                                ; mov x15, x1
                            );
                            emit_clear_machine_roots(&mut ops);
                            emit_reload_safepoint_roots(&mut ops, frame, site)?;
                            let generic_threw = ops.new_dynamic_label();
                            dynasm!(ops
                                ; .arch aarch64
                                ; cbz x15, >generic_method_completed
                                ; cmp x15, NativeResultStatus::Throw as u32
                                ; b.eq =>generic_threw
                                ; b =>fatal
                                ; generic_method_completed:
                            );
                            emit_store_allocated_tagged(
                                &mut ops,
                                frame,
                                locations[result_index],
                                17,
                                0,
                            )?;
                            dynasm!(ops
                                ; .arch aarch64
                                ; b =>direct_done
                                ; =>generic_threw
                                ; mov x0, x17
                                ; b =>direct_threw
                            );
                            structural_regions.push((
                                generic_region,
                                Some(*byte_pc),
                                generic_start,
                                ops.offset().0,
                            ));
                        } else {
                            // Spread packets need canonical iterator expansion
                            // and remain exact pre-effect exits.
                            emit_clear_machine_roots(&mut ops);
                            emit_reload_safepoint_roots(&mut ops, frame, site)?;
                            dynasm!(ops ; .arch aarch64 ; b =>deopt);
                        }
                    }
                    dynasm!(ops
                        ; .arch aarch64
                        ; =>direct_bail
                        ; b =>deopt
                        ; =>direct_threw
                    );
                    match descriptor.exceptional {
                        super::super::ExceptionalEdge::LandingPad(target) => {
                            emit_landing_pad_transfer(
                                &mut ops,
                                frame,
                                locations[result_index],
                                0,
                                allocation.edits(),
                                id,
                                block_labels[target.0 as usize],
                            )?;
                        }
                        super::super::ExceptionalEdge::Propagate => {
                            dynasm!(ops ; .arch aarch64 ; b =>throw_value);
                        }
                        super::super::ExceptionalEdge::None => {
                            return Err(Unsupported::OperandShape(
                                "scalar direct call exceptional edge",
                            ));
                        }
                    }
                    dynasm!(ops ; .arch aarch64 ; =>direct_done);
                } else {
                    if let CallTarget::NativeLeaf { target, byte_pc } = &descriptor.target {
                        let deopt = instruction_deopt_label(instruction.deopt, &deopt_labels)?;
                        let start = ops.offset().0;
                        let int32 = descriptor.results == [MachineRepresentation::Int32];
                        if int32 {
                            crate::template::arm64::ic_probe::emit_native_leaf_guard(
                                &mut ops,
                                view,
                                target.builtin_native_ref,
                                9,
                                deopt,
                            )?;
                            super::super::native_leaf::arm64::emit_int32(
                                &mut ops,
                                target.leaf_stub_id,
                                deopt,
                            )?;
                        } else {
                            crate::template::arm64::ic_probe::emit_native_leaf_call(
                                &mut ops,
                                &mut relocations,
                                view,
                                target.leaf_stub_id,
                                target.builtin_native_ref,
                                9,
                                19,
                                |_ops, _index, _register| Ok(()),
                                deopt,
                            )?;
                        }
                        structural_regions.push((
                            if int32 {
                                "machineNativeInt32MathIntrinsic"
                            } else {
                                "machineNativeLeafCall"
                            },
                            Some(*byte_pc),
                            start,
                            ops.offset().0,
                        ));
                        if !is_terminator {
                            emit_edits(
                                &mut ops,
                                allocation.edits(),
                                AllocationPoint::After(id),
                                frame,
                            )?;
                        }
                        continue;
                    }
                    if let CallTarget::ColdCallExit { byte_pc, .. } = &descriptor.target {
                        let deopt = instruction_deopt_label(instruction.deopt, &deopt_labels)?;
                        let start = ops.offset().0;
                        dynasm!(ops ; .arch aarch64 ; b =>deopt);
                        structural_regions.push((
                            "machineColdCallExit",
                            Some(*byte_pc),
                            start,
                            ops.offset().0,
                        ));
                        continue;
                    }
                    if is_explicit_committed_runtime_call(descriptor) {
                        let start = ops.offset().0;
                        let byte_pc = emit_committed_pair_call(
                            &mut ops,
                            &mut relocations,
                            transitions,
                            sequence,
                            frame,
                            instruction,
                            id,
                            descriptor,
                            locations,
                            safepoints,
                        )?;
                        structural_regions.push((
                            if matches!(descriptor.target, CallTarget::CommittedRuntime { target, .. } if target == STUB_JIT_BINDING_VALUE) {
                                "machineBindingCold"
                            } else if super::super::derived_this::cold_byte_pc(sequence, block_index) == Some(byte_pc) {
                                "machineDerivedThisBindCold"
                            } else {
                                "machineCommittedValueEffect"
                            },
                            Some(byte_pc),
                            start,
                            ops.offset().0,
                        ));
                        if !is_terminator {
                            emit_edits(
                                &mut ops,
                                allocation.edits(),
                                AllocationPoint::After(id),
                                frame,
                            )?;
                        }
                        continue;
                    }
                    if matches!(
                        &descriptor.target,
                        CallTarget::CommittedRuntime { .. } | CallTarget::LiteralAllocation { .. }
                    ) {
                        let start = ops.offset().0;
                        let byte_pc = emit_committed_runtime_call(
                            &mut ops,
                            &mut relocations,
                            transitions,
                            sequence,
                            frame,
                            instruction,
                            id,
                            descriptor,
                            locations,
                            allocation.edits(),
                            safepoints,
                            &block_labels,
                            throw_value,
                            fatal,
                        )?;
                        if super::super::derived_this::cold_byte_pc(sequence, block_index)
                            == Some(byte_pc)
                        {
                            structural_regions.push((
                                "machineDerivedThisBindCold",
                                Some(byte_pc),
                                start,
                                ops.offset().0,
                            ));
                        }
                        structural_regions.push((
                            if matches!(descriptor.target, CallTarget::LiteralAllocation { .. }) {
                                "machineLiteralAllocation"
                            } else {
                                "machineCommittedValueEffect"
                            },
                            Some(byte_pc),
                            start,
                            ops.offset().0,
                        ));
                        if !is_terminator {
                            emit_edits(
                                &mut ops,
                                allocation.edits(),
                                AllocationPoint::After(id),
                                frame,
                            )?;
                        }
                        continue;
                    }
                    if let CallTarget::RuntimeStub(target) = descriptor.target
                        && target == STUB_JIT_ACKNOWLEDGE_CAUGHT_THROW
                    {
                        if !descriptor.arguments.is_empty()
                            || !descriptor.results.is_empty()
                            || descriptor.exceptional != ExceptionalEdge::None
                            || instruction.safepoint.is_some()
                            || !locations.is_empty()
                        {
                            return Err(Unsupported::OperandShape(
                                "scalar caught-throw acknowledgement",
                            ));
                        }
                        dynasm!(ops ; .arch aarch64 ; mov x0, x19);
                        emit_load_symbolic_u64(
                            &mut ops,
                            &mut relocations,
                            16,
                            transitions.entry(target),
                            RelocationTarget::runtime_stub(target),
                        );
                        dynasm!(ops
                            ; .arch aarch64
                            ; blr x16
                            ; cmp x0, NativeResultStatus::Success as u32
                            ; b.ne =>fatal
                        );
                        if !is_terminator {
                            emit_edits(
                                &mut ops,
                                allocation.edits(),
                                AllocationPoint::After(id),
                                frame,
                            )?;
                        }
                        continue;
                    }
                    if let CallTarget::RuntimeStub(target) = descriptor.target
                        && matches!(target, STUB_JIT_LOAD_ELEMENT | STUB_JIT_STORE_ELEMENT)
                    {
                        let load = target == STUB_JIT_LOAD_ELEMENT;
                        let argument_count = if load { 2 } else { 3 };
                        if descriptor.arguments.len() != argument_count
                            || locations.len() < argument_count + usize::from(load)
                        {
                            return Err(Unsupported::OperandShape(
                                "scalar generic element value call",
                            ));
                        }
                        let start = ops.offset().0;
                        let argument_values = instruction
                            .operands
                            .iter()
                            .take(argument_count)
                            .map(|operand| operand.value)
                            .collect::<Vec<_>>();
                        let arguments = CommittedElementArguments {
                            receiver: argument_values[0],
                            key: CommittedElementKey::Tagged(argument_values[1]),
                            value: if load { None } else { Some(argument_values[2]) },
                            preserve_caller_bank: false,
                        };
                        let entry = if load {
                            load_element_entry
                        } else {
                            store_element_entry
                        };
                        let byte_pc = emit_committed_element_value_call(
                            &mut ops,
                            &mut relocations,
                            sequence,
                            frame,
                            instruction,
                            id,
                            safepoints,
                            deopt_runtime,
                            arguments,
                            target,
                            entry,
                            throw_value,
                            fatal,
                        )?;
                        if load {
                            emit_store_allocated_tagged(
                                &mut ops,
                                frame,
                                locations[argument_count],
                                17,
                                0,
                            )?;
                        }
                        structural_regions.push((
                            if load {
                                "machineGenericElementLoad"
                            } else {
                                "machineGenericElementStore"
                            },
                            Some(byte_pc),
                            start,
                            ops.offset().0,
                        ));
                        if !is_terminator {
                            emit_edits(
                                &mut ops,
                                allocation.edits(),
                                AllocationPoint::After(id),
                                frame,
                            )?;
                        }
                        continue;
                    }
                    if matches!(
                        descriptor.target,
                        CallTarget::RuntimeStub(target)
                            if target == STUB_JIT_CLASS_SUPER_CONSTRUCTOR
                    ) {
                        if locations.len() < 2 {
                            return Err(Unsupported::OperandShape("scalar class-super load call"));
                        }
                        let deopt = instruction_deopt_label(instruction.deopt, &deopt_labels)?;
                        let start = ops.offset().0;
                        emit_load_allocated_tagged(&mut ops, frame, locations[0], 1, 0)?;
                        dynasm!(ops
                            ; .arch aarch64
                            ; movz x11, NUMBER_TAG_HI16, lsl #48
                            ; orr x11, x11, #0x2
                            ; tst x1, x11
                            ; b.ne =>deopt
                            ; mov w11, w1
                        );
                        emit_load_symbolic_u64(
                            &mut ops,
                            &mut relocations,
                            12,
                            view.cage_base as u64,
                            RelocationTarget::GcCageBase,
                        );
                        dynasm!(ops
                            ; .arch aarch64
                            ; add x13, x12, x11
                            ; ldrb w14, [x13]
                            ; cmp w14, view.class_constructor_layout.type_tag as u32
                            ; b.ne =>deopt
                            ; ldr x0, [x13, view.class_constructor_layout.super_constructor_byte]
                        );
                        emit_load_u64(&mut ops, 16, Value::hole().to_bits());
                        dynasm!(ops ; .arch aarch64 ; cmp x0, x16 ; b.eq =>deopt);
                        emit_store_allocated_tagged(&mut ops, frame, locations[1], 0, 0)?;
                        structural_regions.push((
                            "machineClassSuperLoad",
                            None,
                            start,
                            ops.offset().0,
                        ));
                        if !is_terminator {
                            emit_edits(
                                &mut ops,
                                allocation.edits(),
                                AllocationPoint::After(id),
                                frame,
                            )?;
                        }
                        continue;
                    }
                    let (target, entry, result_index, allocating) = match &descriptor.target {
                        CallTarget::RuntimeStub(target) if *target == STUB_TO_BOOLEAN_LEAF => {
                            if locations.len() < 3
                                || integer_register(locations[0])? != 1
                                || integer_register(locations[1])? != 2
                            {
                                return Err(Unsupported::OperandShape("scalar ToBoolean call"));
                            }
                            (*target, to_boolean_entry, 2, false)
                        }
                        CallTarget::RuntimeStub(target) if *target == STUB_STRICT_EQ_LEAF => {
                            if locations.len() < 3
                                || integer_register(locations[0])? != 1
                                || integer_register(locations[1])? != 2
                            {
                                return Err(Unsupported::OperandShape(
                                    "scalar strict equality call",
                                ));
                            }
                            (*target, strict_eq_entry, 2, false)
                        }
                        CallTarget::RuntimeStub(target) if *target == STUB_STRING_CONCAT_ALLOC => {
                            if locations.len() < 4
                                || integer_register(locations[0])? != 2
                                || integer_register(locations[1])? != 3
                                || integer_register(locations[2])? != 4
                            {
                                return Err(Unsupported::OperandShape("scalar string concat call"));
                            }
                            (*target, string_concat_entry, 3, true)
                        }
                        CallTarget::RuntimeStub(target)
                            if *target == STUB_ARRAY_CONSTRUCT_ALLOC =>
                        {
                            if locations.len() < 4
                                || integer_register(locations[0])? != 2
                                || integer_register(locations[1])? != 3
                                || integer_register(locations[2])? != 4
                            {
                                return Err(Unsupported::OperandShape(
                                    "scalar ArrayConstruct call",
                                ));
                            }
                            (*target, array_construct_entry, 3, true)
                        }
                        CallTarget::NativeLeaf { .. } => unreachable!("handled native leaf above"),
                        CallTarget::RuntimeStub(_) => {
                            return Err(Unsupported::OperandShape("scalar runtime call target"));
                        }
                        CallTarget::CommittedRuntime { .. }
                        | CallTarget::LiteralAllocation { .. } => {
                            unreachable!("handled committed runtime call above")
                        }
                        CallTarget::Direct { .. } => unreachable!("handled direct call above"),
                        CallTarget::ColdCallExit { .. } => {
                            unreachable!("handled cold call exit above")
                        }
                    };
                    if integer_register(locations[result_index])? != 0 {
                        return Err(Unsupported::OperandShape("scalar runtime call result"));
                    }
                    let deopt = instruction_deopt_label(instruction.deopt, &deopt_labels)?;
                    let array_construct_region = if target == STUB_ARRAY_CONSTRUCT_ALLOC {
                        let byte_pc = instruction
                            .deopt
                            .and_then(|deopt| {
                                deopt_runtime
                                    .table
                                    .lookup(otter_vm::deopt::DeoptExitId(deopt.0))
                            })
                            .map(|state| state.innermost().byte_pc)
                            .ok_or(Unsupported::OperandShape(
                                "scalar ArrayConstruct deopt state",
                            ))?;
                        Some((byte_pc, ops.offset().0))
                    } else {
                        None
                    };
                    if allocating {
                        let site = safepoints
                            .site(id)
                            .filter(|site| instruction.safepoint == Some(site.id))
                            .ok_or(Unsupported::OperandShape(
                                "scalar allocating call safepoint",
                            ))?;
                        emit_allocating_call(
                            &mut ops,
                            &mut relocations,
                            frame,
                            site,
                            entry,
                            target,
                            deopt,
                        )?;
                    } else {
                        dynasm!(ops
                            ; .arch aarch64
                            ; ldr x0, [x19, THREAD_OFFSET]
                            ; ldr x0, [x0, VM_THREAD_GC_HEAP_OFFSET]
                        );
                        emit_load_symbolic_u64(
                            &mut ops,
                            &mut relocations,
                            16,
                            entry,
                            RelocationTarget::runtime_stub(target),
                        );
                        dynasm!(ops
                            ; .arch aarch64
                            ; blr x16
                            ; cbnz x1, =>deopt
                        );
                        emit_load_u64(&mut ops, 16, Value::boolean(true).to_bits());
                        dynasm!(ops
                            ; .arch aarch64
                            ; cmp x0, x16
                            ; cset w0, eq
                        );
                    }
                    if let Some((byte_pc, start)) = array_construct_region {
                        structural_regions.push((
                            "machineArrayConstruct",
                            Some(byte_pc),
                            start,
                            ops.offset().0,
                        ));
                    }
                }
            }
        }
        if !is_terminator {
            emit_edits(
                &mut ops,
                allocation.edits(),
                AllocationPoint::After(id),
                frame,
            )?;
        }
    }

    if next_load_ic != load_ic_cells.len() || next_store_ic != store_ic_cells.len() {
        return Err(Unsupported::OperandShape(
            "scalar property IC ownership mismatch",
        ));
    }

    dynasm!(ops ; .arch aarch64 ; =>bail);
    emit_materialize_vm_window(&mut ops, vm_register_count);
    dynasm!(ops
        ; .arch aarch64
        ; ldr x17, [x19, NATIVE_FRAME_OFFSET]
        ; str wzr, [x17, NATIVE_FRAME_PC_OFFSET]
        ; mov w0, wzr
        ; movz x1, NativeResultStatus::SideExit as u32
    );
    emit_epilogue(&mut ops, frame, saved);

    let compiled_pair_exit = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; =>finish_error
        ; mov x0, x19
    );
    emit_load_symbolic_u64(
        &mut ops,
        &mut relocations,
        16,
        transitions.entry(STUB_JIT_FINISH_ERROR),
        RelocationTarget::runtime_stub(STUB_JIT_FINISH_ERROR),
    );
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; cmp x1, NativeResultStatus::SideExit as u32
        ; b.eq =>compiled_pair_exit
        ; cmp x1, NativeResultStatus::Throw as u32
        ; b.eq =>compiled_pair_exit
        ; cmp x1, NativeResultStatus::Fatal as u32
        ; b.eq =>compiled_pair_exit
        ; b =>fatal
        ; =>throw_value
        ; movz x1, NativeResultStatus::Throw as u32
        ; b =>compiled_pair_exit
        ; =>fatal
    );
    emit_load_u64(&mut ops, 0, VALUE_UNDEFINED);
    dynasm!(ops
        ; .arch aarch64
        ; movz x1, NativeResultStatus::Fatal as u32
        ; =>compiled_pair_exit
    );
    emit_epilogue(&mut ops, frame, saved);

    if !deopt_labels.is_empty() {
        for (index, &label) in deopt_labels.iter().enumerate() {
            let index = u32::try_from(index)
                .ok()
                .filter(|&index| index <= u32::from(u16::MAX))
                .ok_or(Unsupported::OperandShape("scalar deopt exit count"))?;
            dynasm!(ops
                ; .arch aarch64
                ; =>label
                ; movz w17, index
                ; b =>shared_deopt
            );
        }

        // Dump layout consumed by `jit_deopt_writeback_stub`: physical x0..x29
        // followed by d0..d15 at ascending addresses. x15..x18 and x29 are
        // namespace holes; x19 retains the context across the call.
        dynasm!(ops
            ; .arch aarch64
            ; =>shared_deopt
            ; stp d14, d15, [sp, #-16]!
            ; stp d12, d13, [sp, #-16]!
            ; stp d10, d11, [sp, #-16]!
            ; stp d8, d9, [sp, #-16]!
            ; stp d6, d7, [sp, #-16]!
            ; stp d4, d5, [sp, #-16]!
            ; stp d2, d3, [sp, #-16]!
            ; stp d0, d1, [sp, #-16]!
            ; stp x28, xzr, [sp, #-16]!
            ; stp x26, x27, [sp, #-16]!
            ; stp x24, x25, [sp, #-16]!
            ; stp x22, x23, [sp, #-16]!
            ; stp x20, x21, [sp, #-16]!
            ; stp x18, x19, [sp, #-16]!
            ; stp x16, x17, [sp, #-16]!
            ; stp x14, x15, [sp, #-16]!
            ; stp x12, x13, [sp, #-16]!
            ; stp x10, x11, [sp, #-16]!
            ; stp x8, x9, [sp, #-16]!
            ; stp x6, x7, [sp, #-16]!
            ; stp x4, x5, [sp, #-16]!
            ; stp x2, x3, [sp, #-16]!
            ; stp x0, x1, [sp, #-16]!
            ; mov x0, x19
            ; mov w1, w17
        );
        emit_load_symbolic_u64(
            &mut ops,
            &mut relocations,
            2,
            std::ptr::from_ref::<DeoptRuntime>(deopt_runtime) as u64,
            RelocationTarget::DeoptRuntimeData,
        );
        dynasm!(ops
            ; .arch aarch64
            ; mov x3, sp
            ; add x4, sp, DEOPT_DUMP_BYTES
            ; ldr x5, [x19, NATIVE_FRAME_OFFSET]
            ; ldr x5, [x5, NATIVE_FRAME_REGISTER_BASE_OFFSET]
        );
        emit_load_symbolic_u64(
            &mut ops,
            &mut relocations,
            16,
            deopt_writeback_entry,
            RelocationTarget::runtime_stub(STUB_JIT_DEOPT_WRITEBACK),
        );
        dynasm!(ops
            ; .arch aarch64
            ; blr x16
            ; add sp, sp, DEOPT_DUMP_BYTES
        );
        emit_epilogue(&mut ops, frame, saved);
    }

    let mut osr_entries = BTreeMap::new();
    let mut osr_regions = Vec::with_capacity(osr_sites.len());
    for site in &osr_sites {
        let offset = ops.offset().0;
        let representation_bail = ops.new_dynamic_label();
        emit_prologue(&mut ops, frame, saved);
        emit_clear_packed_double_view_caches(
            &mut ops,
            frame,
            sequence.packed_double_view_cache_count(),
        )?;
        if has_backedge_poll {
            dynasm!(ops ; .arch aarch64 ; movz w29, GENERATED_POLL_BATCH);
        }
        let locations = allocation
            .instruction_locations(site.instruction)
            .ok_or(Unsupported::OperandShape("scalar OSR allocation coverage"))?;
        for ((input, &location), operand) in site
            .inputs
            .iter()
            .zip(locations)
            .zip(&sequence.instructions()[site.instruction.0 as usize].operands)
        {
            emit_osr_materialization(
                &mut ops,
                frame,
                *input,
                sequence.representations()[operand.value.0 as usize],
                location,
                representation_bail,
            )?;
        }
        let continuation = site.continuation;
        dynasm!(ops
            ; .arch aarch64
            ; b =>continuation
            ; =>representation_bail
        );
        emit_load_u64(&mut ops, 16, u64::from(site.logical_pc));
        dynasm!(ops
            ; .arch aarch64
            ; ldr x17, [x19, NATIVE_FRAME_OFFSET]
            ; str w16, [x17, NATIVE_FRAME_PC_OFFSET]
            ; mov w0, w16
            ; movz x1, NativeResultStatus::SideExit as u32
        );
        emit_epilogue(&mut ops, frame, saved);
        let end = ops.offset().0;
        if osr_entries.insert(site.logical_pc, offset).is_some() {
            return Err(Unsupported::OperandShape("duplicate scalar OSR logical PC"));
        }
        osr_regions.push((site.logical_pc, offset, end));
    }

    let buffer = ops
        .finalize()
        .map_err(|_| Unsupported::Backend(crate::BackendFailure::Finalization))?;
    let deopt_cold_bytes = if deopt_labels.is_empty() {
        0
    } else {
        DEOPT_DUMP_BYTES
    };
    let committed_element_cold_bytes = if sequence.instructions().iter().any(|instruction| {
        matches!(
            instruction.opcode,
            MachineOpcode::ElementLoad(..) | MachineOpcode::ElementStore(..)
        )
    }) {
        COMMITTED_ELEMENT_COLD_STACK_BYTES
    } else {
        0
    };
    let committed_runtime_cold_bytes = if sequence.call_descriptors().iter().any(|descriptor| {
        matches!(
            &descriptor.target,
            CallTarget::CommittedRuntime { .. } | CallTarget::LiteralAllocation { .. }
        )
    }) {
        MACHINE_ROOT_RECORD_SIZE
    } else {
        0
    };
    let cold_dump_bytes = deopt_cold_bytes
        .max(committed_element_cold_bytes)
        .max(committed_runtime_cold_bytes);
    let generated_stack_frame_bytes =
        frame
            .frame_bytes()
            .checked_add(cold_dump_bytes)
            .ok_or(Unsupported::OperandShape(
                "numeric generated stack frame bytes",
            ))?;
    Ok(Emission {
        code: CompiledCode::new(buffer, AssemblyOffset(0)),
        generated_stack_frame_bytes,
        relocations,
        osr_entries,
        osr_regions,
        constructor_field_regions,
        structural_regions,
    })
}

fn is_exceptional_successor(
    sequence: &InstructionSequence,
    block_index: usize,
    successor: super::super::MachineBlock,
) -> bool {
    let block = &sequence.blocks()[block_index];
    (block.first.0..block.end.0).any(|instruction_index| {
        let instruction = &sequence.instructions()[instruction_index as usize];
        let MachineOpcode::Call(descriptor_index) = instruction.opcode else {
            return false;
        };
        sequence
            .call_descriptors()
            .get(descriptor_index as usize)
            .is_some_and(|descriptor| {
                descriptor.exceptional == super::super::ExceptionalEdge::LandingPad(successor)
            })
    })
}

fn emit_allocating_call(
    ops: &mut dynasmrt::aarch64::Assembler,
    relocations: &mut RelocationCapture,
    frame: MachineFrameLayout,
    site: &MachineSafepointSite,
    entry: u64,
    target: RuntimeStubDescriptor,
    deopt: DynamicLabel,
) -> Result<(), Unsupported> {
    emit_save_safepoint_roots(ops, frame, site)?;
    dynasm!(ops
        ; .arch aarch64
        ; sub sp, sp, ALLOC_CTX_STACK_SIZE
        ; ldr x9, [x19, THREAD_OFFSET]
        ; str x9, [sp, ALLOC_CTX_THREAD_OFFSET]
        ; movz w9, site.id.0
        ; str w9, [sp, ALLOC_CTX_SAFEPOINT_ID_OFFSET]
    );
    if frame.root_slots() == 0 {
        dynasm!(ops
            ; .arch aarch64
            ; str xzr, [sp, ALLOC_CTX_SPILL_SLOTS_OFFSET]
            ; strh wzr, [sp, ALLOC_CTX_SPILL_SLOT_COUNT_OFFSET]
        );
    } else {
        let root_base = ALLOC_CTX_STACK_SIZE
            .checked_add(root_offset(frame, 0)?)
            .ok_or(Unsupported::OperandShape("scalar root-save base"))?;
        emit_sp_address_x9(ops, root_base);
        dynasm!(ops
            ; .arch aarch64
            ; str x9, [sp, ALLOC_CTX_SPILL_SLOTS_OFFSET]
            ; movz w9, frame.root_slots() as u32
            ; strh w9, [sp, ALLOC_CTX_SPILL_SLOT_COUNT_OFFSET]
        );
    }
    dynasm!(ops
        ; .arch aarch64
        ; mov x0, sp
        ; movz w1, site.id.0
    );
    emit_load_symbolic_u64(
        ops,
        relocations,
        16,
        entry,
        RelocationTarget::runtime_stub(target),
    );
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; mov x6, x0
        ; mov x5, x1
        ; add sp, sp, ALLOC_CTX_STACK_SIZE
    );
    emit_reload_safepoint_roots(ops, frame, site)?;
    dynasm!(ops
        ; .arch aarch64
        ; mov x0, x6
        ; cbnz x5, =>deopt
    );
    Ok(())
}

fn emit_save_safepoint_roots(
    ops: &mut dynasmrt::aarch64::Assembler,
    frame: MachineFrameLayout,
    site: &MachineSafepointSite,
) -> Result<(), Unsupported> {
    for root in &site.roots {
        let destination = root_offset(frame, root.save_slot)?;
        match root.source {
            AllocatedLocation::Register(register) if register.is_integer() => {
                dynasm!(ops ; .arch aarch64 ; str X(register.encoding()), [sp, destination]);
            }
            AllocatedLocation::Stack(slot) => {
                let source = spill_offset(frame, slot)?;
                dynasm!(ops
                    ; .arch aarch64
                    ; ldr x16, [sp, source]
                    ; str x16, [sp, destination]
                );
            }
            AllocatedLocation::Register(_) => {
                return Err(Unsupported::OperandShape("scalar tagged root source"));
            }
        }
    }
    Ok(())
}

fn emit_load_safepoint_root(
    ops: &mut dynasmrt::aarch64::Assembler,
    frame: MachineFrameLayout,
    site: &MachineSafepointSite,
    value: super::super::MachineValue,
    target: u8,
    sp_bias: u32,
) -> Result<(), Unsupported> {
    let root =
        site.roots
            .iter()
            .find(|root| root.value == value)
            .ok_or(Unsupported::OperandShape(
                "scalar canonical safepoint argument root",
            ))?;
    let offset = root_offset(frame, root.save_slot)?
        .checked_add(sp_bias)
        .ok_or(Unsupported::OperandShape(
            "scalar safepoint argument root offset",
        ))?;
    emit_sp_ldr_x(ops, target, offset);
    Ok(())
}

fn emit_reload_safepoint_roots(
    ops: &mut dynasmrt::aarch64::Assembler,
    frame: MachineFrameLayout,
    site: &MachineSafepointSite,
) -> Result<(), Unsupported> {
    emit_reload_safepoint_roots_with_bias(ops, frame, site, 0)
}

fn emit_reload_safepoint_roots_with_bias(
    ops: &mut dynasmrt::aarch64::Assembler,
    frame: MachineFrameLayout,
    site: &MachineSafepointSite,
    sp_bias: u32,
) -> Result<(), Unsupported> {
    for root in &site.roots {
        let source = root_offset(frame, root.save_slot)?
            .checked_add(sp_bias)
            .ok_or(Unsupported::OperandShape("scalar root reload stack bias"))?;
        match root.source {
            AllocatedLocation::Register(register) if register.is_integer() => {
                dynasm!(ops ; .arch aarch64 ; ldr X(register.encoding()), [sp, source]);
            }
            AllocatedLocation::Stack(slot) => {
                let destination = spill_offset(frame, slot)?
                    .checked_add(sp_bias)
                    .ok_or(Unsupported::OperandShape("scalar spill reload stack bias"))?;
                dynasm!(ops
                    ; .arch aarch64
                    ; ldr x16, [sp, source]
                    ; str x16, [sp, destination]
                );
            }
            AllocatedLocation::Register(_) => {
                return Err(Unsupported::OperandShape("scalar tagged root reload"));
            }
        }
    }
    Ok(())
}

fn emit_publish_machine_roots(
    ops: &mut dynasmrt::aarch64::Assembler,
    frame: MachineFrameLayout,
    site: &MachineSafepointSite,
) -> Result<(), Unsupported> {
    emit_publish_machine_roots_with_bias(ops, frame, site, 0)
}

fn emit_publish_machine_roots_with_bias(
    ops: &mut dynasmrt::aarch64::Assembler,
    frame: MachineFrameLayout,
    site: &MachineSafepointSite,
    outer_sp_bias: u32,
) -> Result<(), Unsupported> {
    dynasm!(ops ; .arch aarch64 ; sub sp, sp, MACHINE_ROOT_RECORD_SIZE);
    if site.roots.is_empty() {
        dynasm!(ops ; .arch aarch64 ; mov x13, xzr);
    } else {
        let offset = root_offset(frame, 0)?
            .checked_add(MACHINE_ROOT_RECORD_SIZE)
            .and_then(|offset| offset.checked_add(outer_sp_bias))
            .ok_or(Unsupported::OperandShape("scalar machine root base"))?;
        if offset <= 4095 {
            dynasm!(ops ; .arch aarch64 ; add x13, sp, offset);
        } else {
            emit_load_u64(ops, 13, u64::from(offset));
            dynasm!(ops ; .arch aarch64 ; add x13, sp, x13);
        }
    }
    dynasm!(ops
        ; .arch aarch64
        ; ldr x9, [x19, MACHINE_ROOTS_PTR_OFFSET]
        ; ldr x10, [x9]
        ; str x10, [sp, MACHINE_ROOT_RECORD_PREVIOUS_OFFSET]
        ; str x13, [sp, MACHINE_ROOT_RECORD_BASE_OFFSET]
        ; movz w11, site.roots.len() as u32
        // One word initializes both root_count and the reserved zero field.
        ; str w11, [sp, MACHINE_ROOT_RECORD_COUNT_OFFSET]
        ; movz w11, site.id.0
        ; str w11, [sp, MACHINE_ROOT_RECORD_SAFEPOINT_ID_OFFSET]
        ; ldr x11, [x19, THREAD_OFFSET]
        ; ldr x11, [x11, VM_THREAD_CODE_OBJECT_ID_OFFSET]
        ; str x11, [sp, MACHINE_ROOT_RECORD_CODE_OBJECT_ID_OFFSET]
        ; mov x12, sp
        ; str x12, [x9]
    );
    Ok(())
}

fn emit_clear_machine_roots(ops: &mut dynasmrt::aarch64::Assembler) {
    dynasm!(ops
        ; .arch aarch64
        ; ldr x9, [x19, MACHINE_ROOTS_PTR_OFFSET]
        ; ldr x10, [sp, MACHINE_ROOT_RECORD_PREVIOUS_OFFSET]
        ; str x10, [x9]
        ; add sp, sp, MACHINE_ROOT_RECORD_SIZE
    );
}

fn emit_load_allocated_tagged(
    ops: &mut dynasmrt::aarch64::Assembler,
    frame: MachineFrameLayout,
    location: AllocatedLocation,
    target: u8,
    sp_bias: u32,
) -> Result<(), Unsupported> {
    emit_load_allocated_integer(ops, frame, location, target, sp_bias)
}

fn emit_load_allocated_integer(
    ops: &mut dynasmrt::aarch64::Assembler,
    frame: MachineFrameLayout,
    location: AllocatedLocation,
    target: u8,
    sp_bias: u32,
) -> Result<(), Unsupported> {
    match location {
        AllocatedLocation::Register(register) if register.is_integer() => {
            dynasm!(ops ; .arch aarch64 ; mov X(target), X(register.encoding()));
        }
        AllocatedLocation::Stack(slot) => {
            let offset = spill_offset(frame, slot)?
                .checked_add(sp_bias)
                .ok_or(Unsupported::OperandShape("scalar direct call spill offset"))?;
            if offset <= 32_760 {
                dynasm!(ops ; .arch aarch64 ; ldr X(target), [sp, offset]);
            } else {
                emit_load_u64(ops, 16, u64::from(offset));
                dynasm!(ops ; .arch aarch64 ; ldr X(target), [sp, x16]);
            }
        }
        AllocatedLocation::Register(_) => {
            return Err(Unsupported::OperandShape("scalar allocated integer source"));
        }
    }
    Ok(())
}

fn emit_store_allocated_tagged(
    ops: &mut dynasmrt::aarch64::Assembler,
    frame: MachineFrameLayout,
    location: AllocatedLocation,
    source: u8,
    sp_bias: u32,
) -> Result<(), Unsupported> {
    match location {
        AllocatedLocation::Register(register) if register.is_integer() => {
            dynasm!(ops ; .arch aarch64 ; mov X(register.encoding()), X(source));
        }
        AllocatedLocation::Stack(slot) => {
            let offset = spill_offset(frame, slot)?
                .checked_add(sp_bias)
                .ok_or(Unsupported::OperandShape("scalar direct call spill offset"))?;
            if offset <= 32_760 {
                dynasm!(ops ; .arch aarch64 ; str X(source), [sp, offset]);
            } else {
                emit_load_u64(ops, 16, u64::from(offset));
                dynasm!(ops ; .arch aarch64 ; str X(source), [sp, x16]);
            }
        }
        AllocatedLocation::Register(_) => {
            return Err(Unsupported::OperandShape(
                "scalar direct call tagged destination",
            ));
        }
    }
    Ok(())
}

/// Expand a parameter-prefix frame before shared VM machinery can observe it.
///
/// This is cold, allocation-free, and publishes the full count only after all
/// newly visible slots contain canonical tagged `undefined` values.
fn emit_materialize_vm_window(ops: &mut dynasmrt::aarch64::Assembler, register_count: u16) {
    let done = ops.new_dynamic_label();
    let loop_label = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; ldr x17, [x19, NATIVE_FRAME_OFFSET]
        ; ldrh w16, [x17, NATIVE_FRAME_REGISTER_COUNT_OFFSET]
        ; movz w15, register_count as u32
        ; cmp w16, w15
        ; b.eq =>done
        ; ldr x14, [x17, NATIVE_FRAME_REGISTER_BASE_OFFSET]
        ; add x14, x14, x16, lsl #3
        ; sub w15, w15, w16
    );
    emit_load_u64(ops, 16, VALUE_UNDEFINED);
    dynasm!(ops
        ; .arch aarch64
        ; =>loop_label
        ; str x16, [x14], #8
        ; subs w15, w15, #1
        ; b.ne =>loop_label
        ; movz w16, register_count as u32
        ; strh w16, [x17, NATIVE_FRAME_REGISTER_COUNT_OFFSET]
        ; =>done
    );
}

fn block_for_instruction(
    sequence: &InstructionSequence,
    instruction: MachineInstructionId,
) -> Result<usize, Unsupported> {
    sequence
        .blocks()
        .iter()
        .position(|block| block.first.0 <= instruction.0 && instruction.0 < block.end.0)
        .ok_or(Unsupported::OperandShape("numeric instruction block"))
}

fn reject_unimplemented_locations(
    sequence: &InstructionSequence,
    allocation: &AllocatedSequence,
) -> Result<(), Unsupported> {
    for instruction_index in 0..sequence.instructions().len() {
        let locations = allocation
            .instruction_locations(MachineInstructionId(instruction_index as u32))
            .ok_or(Unsupported::OperandShape("numeric allocation coverage"))?;
        for &location in locations {
            match location {
                AllocatedLocation::Register(register)
                    if register.is_integer()
                        && !((register.encoding() <= 14)
                            || (20..=28).contains(&register.encoding())) =>
                {
                    return Err(Unsupported::OperandShape("scalar Machine IR GPR"));
                }
                AllocatedLocation::Register(register)
                    if register.is_float() && register.encoding() > 15 =>
                {
                    return Err(Unsupported::OperandShape("scalar Machine IR FP register"));
                }
                AllocatedLocation::Register(_) | AllocatedLocation::Stack(_) => {}
            }
        }
    }
    Ok(())
}

fn emit_edits(
    ops: &mut dynasmrt::aarch64::Assembler,
    edits: &[AllocationEdit],
    point: AllocationPoint,
    frame: MachineFrameLayout,
) -> Result<(), Unsupported> {
    for edit in edits.iter().filter(|edit| edit.point == point) {
        if edit.from == edit.to {
            continue;
        }
        match (edit.from, edit.to) {
            (AllocatedLocation::Register(from), AllocatedLocation::Register(to))
                if from.is_integer() && to.is_integer() =>
            {
                dynasm!(ops ; .arch aarch64 ; mov X(to.encoding()), X(from.encoding()));
            }
            (AllocatedLocation::Register(from), AllocatedLocation::Register(to))
                if from.is_float() && to.is_float() =>
            {
                dynasm!(ops ; .arch aarch64 ; fmov D(to.encoding()), D(from.encoding()));
            }
            (AllocatedLocation::Register(from), AllocatedLocation::Stack(slot)) => {
                let offset = spill_offset(frame, slot)?;
                if from.is_integer() {
                    dynasm!(ops ; .arch aarch64 ; str X(from.encoding()), [sp, offset]);
                } else if from.is_float() {
                    dynasm!(ops ; .arch aarch64 ; str D(from.encoding()), [sp, offset]);
                } else {
                    return Err(Unsupported::OperandShape("numeric spill register class"));
                }
            }
            (AllocatedLocation::Stack(slot), AllocatedLocation::Register(to)) => {
                let offset = spill_offset(frame, slot)?;
                if to.is_integer() {
                    dynasm!(ops ; .arch aarch64 ; ldr X(to.encoding()), [sp, offset]);
                } else if to.is_float() {
                    dynasm!(ops ; .arch aarch64 ; ldr D(to.encoding()), [sp, offset]);
                } else {
                    return Err(Unsupported::OperandShape("numeric reload register class"));
                }
            }
            (AllocatedLocation::Stack(from), AllocatedLocation::Stack(to)) => {
                let from = spill_offset(frame, from)?;
                let to = spill_offset(frame, to)?;
                dynasm!(ops
                    ; .arch aarch64
                    ; ldr x16, [sp, from]
                    ; str x16, [sp, to]
                );
            }
            (AllocatedLocation::Register(_), AllocatedLocation::Register(_)) => {
                return Err(Unsupported::OperandShape("numeric edit register class"));
            }
        }
    }
    Ok(())
}

fn emit_decode_number(
    ops: &mut dynasmrt::aarch64::Assembler,
    source: u8,
    destination: u8,
    bail: dynasmrt::DynamicLabel,
) {
    let non_int = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; movz x16, NUMBER_TAG_HI16, lsl #48
        ; and x17, X(source), x16
        ; cmp x17, x16
        ; b.ne =>non_int
        ; scvtf D(destination), W(source)
        ; b =>done
        ; =>non_int
        ; tst X(source), x16
        ; b.eq =>bail
        ; movz x16, DOUBLE_OFFSET_HI16, lsl #48
        ; sub x17, X(source), x16
        ; fmov D(destination), x17
        ; =>done
    );
}

fn emit_decode_int32(
    ops: &mut dynasmrt::aarch64::Assembler,
    source: u8,
    bail: dynasmrt::DynamicLabel,
) {
    dynasm!(ops
        ; .arch aarch64
        ; movz x16, NUMBER_TAG_HI16, lsl #48
        ; and x17, X(source), x16
        ; cmp x17, x16
        ; b.ne =>bail
    );
}

fn emit_box_number(ops: &mut dynasmrt::aarch64::Assembler, source: u8, destination: u8) {
    let encode_double = ops.new_dynamic_label();
    let tag_integer = ops.new_dynamic_label();
    let double_ready = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; fcvtzs w16, D(source)
        ; scvtf d31, w16
        ; fcmp D(source), d31
        ; b.ne =>encode_double
        ; fmov x17, D(source)
        ; cbnz w16, =>tag_integer
        ; tbnz x17, #63, =>encode_double
        ; =>tag_integer
        ; mov w17, w16
        ; movz x16, NUMBER_TAG_HI16, lsl #48
        ; orr X(destination), x17, x16
        ; b =>done
        ; =>encode_double
        ; fmov X(destination), D(source)
        ; fcmp D(source), D(source)
        ; b.vc =>double_ready
        ; movz X(destination), CANONICAL_NAN_HI16, lsl #48
        ; =>double_ready
        ; movz x16, DOUBLE_OFFSET_HI16, lsl #48
        ; add X(destination), X(destination), x16
        ; =>done
    );
}

fn emit_box_uint32(ops: &mut dynasmrt::aarch64::Assembler, source: u8, destination: u8) {
    let encode_double = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; tbnz W(source), #31, =>encode_double
        ; mov W(destination), W(source)
        ; movz x16, NUMBER_TAG_HI16, lsl #48
        ; orr X(destination), X(destination), x16
        ; b =>done
        ; =>encode_double
        ; ucvtf d31, W(source)
        ; fmov X(destination), d31
        ; movz x16, DOUBLE_OFFSET_HI16, lsl #48
        ; add X(destination), X(destination), x16
        ; =>done
    );
}

fn emit_box_boolean(ops: &mut dynasmrt::aarch64::Assembler, source: u8, destination: u8) {
    let is_false = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64 ; cbz W(source), =>is_false);
    emit_load_u64(ops, destination, Value::boolean(true).to_bits());
    dynasm!(ops ; .arch aarch64 ; b =>done ; =>is_false);
    emit_load_u64(ops, destination, Value::boolean(false).to_bits());
    dynasm!(ops ; .arch aarch64 ; =>done);
}

fn emit_prologue(
    ops: &mut dynasmrt::aarch64::Assembler,
    frame: MachineFrameLayout,
    saved: SavedFrame,
) {
    dynasm!(ops
        ; .arch aarch64
        ; stp x29, x30, [sp, #-16]!
    );
    if saved.gpr_count == 0 {
        dynasm!(ops ; .arch aarch64 ; str x19, [sp, #-16]!);
    } else {
        dynasm!(ops ; .arch aarch64 ; stp x19, x20, [sp, #-16]!);
    }
    for pair in 0..(saved.gpr_count.saturating_sub(1) / 2) {
        let first = 21 + pair * 2;
        dynasm!(ops ; .arch aarch64 ; stp X(first), X(first + 1), [sp, #-16]!);
    }
    if !saved.gpr_count.saturating_sub(1).is_multiple_of(2) {
        let last = 19 + saved.gpr_count;
        dynasm!(ops ; .arch aarch64 ; str X(last), [sp, #-16]!);
    }
    for pair in 0..(saved.fp_count / 2) {
        let first = 8 + pair * 2;
        dynasm!(ops ; .arch aarch64 ; stp D(first), D(first + 1), [sp, #-16]!);
    }
    if !saved.fp_count.is_multiple_of(2) {
        let last = 7 + saved.fp_count;
        dynasm!(ops ; .arch aarch64 ; str D(last), [sp, #-16]!);
    }
    emit_reserve_spill_area(ops, frame.spill_area_bytes());
    dynasm!(ops
        ; .arch aarch64
        ; mov x19, x0
        ; ldr x17, [x19, NATIVE_FRAME_OFFSET]
        ; ldr x17, [x17, NATIVE_FRAME_REGISTER_BASE_OFFSET]
    );
}

fn emit_load_osr_source(ops: &mut dynasmrt::aarch64::Assembler, frame_register: u16) {
    let offset = u32::from(frame_register) * 8;
    dynasm!(ops
        ; .arch aarch64
        ; ldr x17, [x19, NATIVE_FRAME_OFFSET]
        ; ldr x17, [x17, NATIVE_FRAME_REGISTER_BASE_OFFSET]
        ; ldr x16, [x17, offset]
    );
}

fn emit_decode_osr_number(ops: &mut dynasmrt::aarch64::Assembler, bail: DynamicLabel) {
    let non_int = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; fmov d30, x16
        ; movz x16, NUMBER_TAG_HI16, lsl #48
        ; fmov x17, d30
        ; and x17, x17, x16
        ; cmp x17, x16
        ; b.ne =>non_int
        ; fmov x17, d30
        ; scvtf d31, w17
        ; b =>done
        ; =>non_int
        ; fmov x17, d30
        ; tst x17, x16
        ; b.eq =>bail
        ; movz x16, DOUBLE_OFFSET_HI16, lsl #48
        ; sub x17, x17, x16
        ; fmov d31, x17
        ; =>done
    );
}

fn emit_store_osr_integer(
    ops: &mut dynasmrt::aarch64::Assembler,
    frame: MachineFrameLayout,
    location: AllocatedLocation,
) -> Result<(), Unsupported> {
    match location {
        AllocatedLocation::Register(register) if register.is_integer() => {
            let destination = register.encoding();
            dynasm!(ops ; .arch aarch64 ; mov W(destination), w16);
        }
        AllocatedLocation::Stack(slot) => {
            let offset = spill_offset(frame, slot)?;
            dynasm!(ops ; .arch aarch64 ; str x16, [sp, offset]);
        }
        AllocatedLocation::Register(_) => {
            return Err(Unsupported::OperandShape("scalar OSR integer location"));
        }
    }
    Ok(())
}

fn emit_store_osr_tagged(
    ops: &mut dynasmrt::aarch64::Assembler,
    frame: MachineFrameLayout,
    location: AllocatedLocation,
) -> Result<(), Unsupported> {
    match location {
        AllocatedLocation::Register(register) if register.is_integer() => {
            let destination = register.encoding();
            dynasm!(ops ; .arch aarch64 ; mov X(destination), x16);
        }
        AllocatedLocation::Stack(slot) => {
            let offset = spill_offset(frame, slot)?;
            dynasm!(ops ; .arch aarch64 ; str x16, [sp, offset]);
        }
        AllocatedLocation::Register(_) => {
            return Err(Unsupported::OperandShape("scalar OSR tagged location"));
        }
    }
    Ok(())
}

fn emit_store_osr_float(
    ops: &mut dynasmrt::aarch64::Assembler,
    frame: MachineFrameLayout,
    location: AllocatedLocation,
) -> Result<(), Unsupported> {
    match location {
        AllocatedLocation::Register(register) if register.is_float() => {
            let destination = register.encoding();
            dynasm!(ops ; .arch aarch64 ; fmov D(destination), d31);
        }
        AllocatedLocation::Stack(slot) => {
            let offset = spill_offset(frame, slot)?;
            dynasm!(ops ; .arch aarch64 ; str d31, [sp, offset]);
        }
        AllocatedLocation::Register(_) => {
            return Err(Unsupported::OperandShape("scalar OSR float location"));
        }
    }
    Ok(())
}

fn emit_osr_materialization(
    ops: &mut dynasmrt::aarch64::Assembler,
    frame: MachineFrameLayout,
    input: MachineOsrInput,
    representation: MachineRepresentation,
    location: AllocatedLocation,
    bail: DynamicLabel,
) -> Result<(), Unsupported> {
    let expected = match input.value_type {
        MachineOsrType::Tagged => MachineRepresentation::Tagged,
        MachineOsrType::Int32 => MachineRepresentation::Int32,
        MachineOsrType::Uint32 => MachineRepresentation::Uint32,
        MachineOsrType::Float64 => MachineRepresentation::Float64,
        MachineOsrType::Boolean => MachineRepresentation::Boolean,
    };
    if representation != expected {
        return Err(Unsupported::OperandShape("scalar OSR representation"));
    }

    emit_load_osr_source(ops, input.frame_register);
    match input.value_type {
        MachineOsrType::Tagged => emit_store_osr_tagged(ops, frame, location),
        MachineOsrType::Int32 => {
            dynasm!(ops
                ; .arch aarch64
                ; movz x17, NUMBER_TAG_HI16, lsl #48
                ; and x16, x16, x17
                ; cmp x16, x17
                ; b.ne =>bail
            );
            emit_load_osr_source(ops, input.frame_register);
            dynasm!(ops ; .arch aarch64 ; mov w16, w16);
            emit_store_osr_integer(ops, frame, location)
        }
        MachineOsrType::Uint32 => {
            emit_decode_osr_number(ops, bail);
            dynasm!(ops
                ; .arch aarch64
                ; fcvtzu w16, d31
                ; ucvtf d30, w16
                ; fcmp d31, d30
                ; b.ne =>bail
            );
            emit_store_osr_integer(ops, frame, location)
        }
        MachineOsrType::Float64 => {
            emit_decode_osr_number(ops, bail);
            emit_store_osr_float(ops, frame, location)
        }
        MachineOsrType::Boolean => {
            let is_true = ops.new_dynamic_label();
            let ready = ops.new_dynamic_label();
            emit_load_u64(ops, 17, Value::boolean(true).to_bits());
            dynasm!(ops
                ; .arch aarch64
                ; cmp x16, x17
                ; b.eq =>is_true
            );
            emit_load_u64(ops, 17, Value::boolean(false).to_bits());
            dynasm!(ops
                ; .arch aarch64
                ; cmp x16, x17
                ; b.ne =>bail
                ; mov w16, wzr
                ; b =>ready
                ; =>is_true
                ; mov w16, #1
                ; =>ready
            );
            emit_store_osr_integer(ops, frame, location)
        }
    }
}

fn emit_epilogue(
    ops: &mut dynasmrt::aarch64::Assembler,
    frame: MachineFrameLayout,
    saved: SavedFrame,
) {
    emit_release_spill_area(ops, frame.spill_area_bytes());
    if !saved.fp_count.is_multiple_of(2) {
        let last = 7 + saved.fp_count;
        dynasm!(ops ; .arch aarch64 ; ldr D(last), [sp], #16);
    }
    for pair in (0..(saved.fp_count / 2)).rev() {
        let first = 8 + pair * 2;
        dynasm!(ops ; .arch aarch64 ; ldp D(first), D(first + 1), [sp], #16);
    }
    if !saved.gpr_count.saturating_sub(1).is_multiple_of(2) {
        let last = 19 + saved.gpr_count;
        dynasm!(ops ; .arch aarch64 ; ldr X(last), [sp], #16);
    }
    for pair in (0..(saved.gpr_count.saturating_sub(1) / 2)).rev() {
        let first = 21 + pair * 2;
        dynasm!(ops ; .arch aarch64 ; ldp X(first), X(first + 1), [sp], #16);
    }
    if saved.gpr_count == 0 {
        dynasm!(ops ; .arch aarch64 ; ldr x19, [sp], #16);
    } else {
        dynasm!(ops ; .arch aarch64 ; ldp x19, x20, [sp], #16);
    }
    dynasm!(ops
        ; .arch aarch64
        ; ldp x29, x30, [sp], #16
        ; ret
    );
}

fn emit_reserve_spill_area(ops: &mut dynasmrt::aarch64::Assembler, bytes: u32) {
    if bytes == 0 {
        return;
    }
    if bytes <= 4095 {
        dynasm!(ops ; .arch aarch64 ; sub sp, sp, bytes);
    } else {
        emit_load_u64(ops, 16, u64::from(bytes));
        dynasm!(ops ; .arch aarch64 ; sub sp, sp, x16);
    }
}

fn emit_release_spill_area(ops: &mut dynasmrt::aarch64::Assembler, bytes: u32) {
    if bytes == 0 {
        return;
    }
    if bytes <= 4095 {
        dynasm!(ops ; .arch aarch64 ; add sp, sp, bytes);
    } else {
        emit_load_u64(ops, 16, u64::from(bytes));
        dynasm!(ops ; .arch aarch64 ; add sp, sp, x16);
    }
}

fn spill_offset(frame: MachineFrameLayout, slot: u32) -> Result<u32, Unsupported> {
    frame
        .spill_offset(slot)
        .map_err(|_| Unsupported::OperandShape("scalar Machine IR spill offset"))
}

fn root_offset(frame: MachineFrameLayout, slot: u16) -> Result<u32, Unsupported> {
    frame
        .root_offset(slot)
        .map_err(|_| Unsupported::OperandShape("scalar Machine IR root-save offset"))
}

fn raw_offset(frame: MachineFrameLayout, slot: u16) -> Result<u32, Unsupported> {
    frame
        .raw_offset(slot)
        .map_err(|_| Unsupported::OperandShape("scalar Machine IR raw-cache offset"))
}

fn emit_sp_address_x9(ops: &mut dynasmrt::aarch64::Assembler, offset: u32) {
    if offset <= 4095 {
        dynasm!(ops ; .arch aarch64 ; add x9, sp, offset);
    } else {
        emit_load_u64(ops, 9, u64::from(offset));
        dynasm!(ops ; .arch aarch64 ; add x9, sp, x9);
    }
}

fn emit_sp_ldr_x(ops: &mut dynasmrt::aarch64::Assembler, register: u8, offset: u32) {
    if offset <= 32_760 && offset.is_multiple_of(8) {
        dynasm!(ops ; .arch aarch64 ; ldr X(register), [sp, offset]);
    } else {
        emit_sp_address_x9(ops, offset);
        dynasm!(ops ; .arch aarch64 ; ldr X(register), [x9]);
    }
}

fn emit_sp_str_x(ops: &mut dynasmrt::aarch64::Assembler, register: u8, offset: u32) {
    if offset <= 32_760 && offset.is_multiple_of(8) {
        dynasm!(ops ; .arch aarch64 ; str X(register), [sp, offset]);
    } else {
        emit_sp_address_x9(ops, offset);
        dynasm!(ops ; .arch aarch64 ; str X(register), [x9]);
    }
}

fn emit_load_u64(ops: &mut dynasmrt::aarch64::Assembler, register: u8, value: u64) {
    dynasm!(ops ; .arch aarch64 ; movz X(register), (value & 0xffff) as u32);
    if (value >> 16) & 0xffff != 0 {
        dynasm!(ops ; .arch aarch64 ; movk X(register), ((value >> 16) & 0xffff) as u32, lsl #16);
    }
    if (value >> 32) & 0xffff != 0 {
        dynasm!(ops ; .arch aarch64 ; movk X(register), ((value >> 32) & 0xffff) as u32, lsl #32);
    }
    if (value >> 48) & 0xffff != 0 {
        dynasm!(ops ; .arch aarch64 ; movk X(register), ((value >> 48) & 0xffff) as u32, lsl #48);
    }
}

fn emit_load_symbolic_u64(
    ops: &mut dynasmrt::aarch64::Assembler,
    relocations: &mut RelocationCapture,
    register: u8,
    value: u64,
    target: RelocationTarget,
) {
    let start = ops.offset().0;
    emit_load_u64(ops, register, value);
    relocations.record_mov_wide(start, ops.offset().0, register, target);
}

fn integer_register(location: AllocatedLocation) -> Result<u8, Unsupported> {
    match location {
        AllocatedLocation::Register(register) if register.is_integer() => Ok(register.encoding()),
        AllocatedLocation::Register(_) | AllocatedLocation::Stack(_) => {
            Err(Unsupported::OperandShape("numeric integer register"))
        }
    }
}

fn dense_index_form(sequence: &InstructionSequence, value: MachineValue) -> Option<DenseIndexForm> {
    match sequence.representations().get(value.0 as usize).copied()? {
        MachineRepresentation::Tagged => Some(DenseIndexForm::Tagged),
        MachineRepresentation::Int32 | MachineRepresentation::Uint32 => Some(DenseIndexForm::Int32),
        _ => None,
    }
}

fn float_register(location: AllocatedLocation) -> Result<u8, Unsupported> {
    match location {
        AllocatedLocation::Register(register) if register.is_float() => Ok(register.encoding()),
        AllocatedLocation::Register(_) | AllocatedLocation::Stack(_) => {
            Err(Unsupported::OperandShape("numeric floating register"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        COMMITTED_ELEMENT_CALLER_SAVE_BYTES, COMMITTED_ELEMENT_COLD_STACK_BYTES,
        COMMITTED_ELEMENT_FP_SAVE_PAIRS, COMMITTED_ELEMENT_GPR_SAVE_PAIRS,
        COMMITTED_ELEMENT_ROOT_RECORD_BYTES, CommittedElementKey, RelocationCapture,
        committed_element_key, emit, emit_binding_guard, emit_load_u64, frame_layout,
    };
    use crate::{
        entry::TransitionTable,
        machine::{
            AllocatedLocation, CallDescriptor, CallEffects, CallTarget, ControlFlow,
            ExceptionalEdge, InstructionSequence, MachineBindingTarget, MachineBlock,
            MachineBlockData, MachineInstruction, MachineInstructionId, MachineOpcode,
            MachineOperand, MachineRepresentation, MachineValue, PhysicalRegister, SafepointId,
            SafepointKind, TargetRegisterFile, lower_safepoints,
        },
    };
    use otter_bytecode::opcode_schema::{BindingMissing, BindingRead, BindingSemantics};
    use otter_vm::{JitCompileSnapshot, deopt::DeoptRuntime};

    fn committed_runtime_sequence(semantic_arity: u8) -> InstructionSequence {
        let unrelated_root = (semantic_arity == 0).then_some(MachineValue(0));
        let inputs = (0..u32::from(semantic_arity))
            .map(MachineValue)
            .collect::<Vec<_>>();
        let result = MachineValue(if semantic_arity == 0 {
            1
        } else {
            u32::from(semantic_arity)
        });
        let mut instructions = unrelated_root
            .into_iter()
            .chain(inputs.iter().copied())
            .enumerate()
            .map(|(index, value)| {
                MachineInstruction::plain(
                    MachineOpcode::EntryValue(index as u16),
                    vec![MachineOperand::register_output(value)],
                )
            })
            .collect::<Vec<_>>();
        let mut operands = inputs
            .iter()
            .copied()
            .map(MachineOperand::location_input)
            .collect::<Vec<_>>();
        operands.push(MachineOperand::register_output(result));
        operands.extend(inputs.iter().copied().map(MachineOperand::tagged_root));
        operands.extend(unrelated_root.map(MachineOperand::tagged_root));
        let mut call = MachineInstruction::plain(MachineOpcode::Call(0), operands);
        call.clobbers = TargetRegisterFile::aarch64_scalar_call_clobbers();
        call.safepoint = Some(SafepointId(0));
        instructions.push(call);
        let mut ret = MachineInstruction::plain(
            MachineOpcode::Return,
            vec![MachineOperand::register_input(result)],
        );
        ret.control = ControlFlow::Return;
        instructions.push(ret);
        let instruction_count = instructions.len() as u32;
        let complete_effects = CallEffects::READS_HEAP
            .union(CallEffects::WRITES_HEAP)
            .union(CallEffects::INVALIDATES_SHAPES)
            .union(CallEffects::REENTRANT);
        InstructionSequence::new(
            MachineBlock(0),
            vec![MachineRepresentation::Tagged; result.0 as usize + 1],
            vec![CallDescriptor {
                target: CallTarget::CommittedRuntime {
                    target: otter_vm::native_abi::STUB_JIT_SCALAR_VALUE,
                    logical_pc: 7,
                    byte_pc: 29,
                    semantic_arity,
                },
                arguments: vec![MachineRepresentation::Tagged; usize::from(semantic_arity)],
                results: vec![MachineRepresentation::Tagged],
                effects: complete_effects,
                clobbers: TargetRegisterFile::aarch64_scalar_call_clobbers(),
                exceptional: ExceptionalEdge::Propagate,
                safepoint: SafepointKind::Gc,
            }],
            vec![MachineBlockData {
                first: MachineInstructionId(0),
                end: MachineInstructionId(instruction_count),
                predecessors: Vec::new(),
                successors: Vec::new(),
                parameters: Vec::new(),
                successor_arguments: Vec::new(),
            }],
            instructions,
        )
        .expect("valid committed-runtime emitter sequence")
    }

    fn committed_runtime_landing_sequence() -> InstructionSequence {
        let source = MachineValue(0);
        let condition = MachineValue(1);
        let exception = MachineValue(2);
        let handler_value = MachineValue(3);
        let mut call = MachineInstruction::plain(
            MachineOpcode::Call(0),
            vec![
                MachineOperand::fixed_register_output(exception, PhysicalRegister::integer(20)),
                MachineOperand::tagged_root(source),
            ],
        );
        call.clobbers = TargetRegisterFile::aarch64_scalar_call_clobbers();
        call.safepoint = Some(SafepointId(0));
        let mut call_jump = MachineInstruction::plain(MachineOpcode::Jump, Vec::new());
        call_jump.control = ControlFlow::Branch;
        let mut normal_branch = MachineInstruction::plain(
            MachineOpcode::BranchIf(true),
            vec![MachineOperand::register_input(condition)],
        );
        normal_branch.control = ControlFlow::Branch;
        let mut acknowledge = MachineInstruction::plain(MachineOpcode::Call(1), Vec::new());
        acknowledge.clobbers = TargetRegisterFile::aarch64_scalar_call_clobbers();
        let mut exceptional_edge = MachineInstruction::plain(MachineOpcode::Jump, Vec::new());
        exceptional_edge.control = ControlFlow::Branch;
        let mut alternative_edge = MachineInstruction::plain(MachineOpcode::Jump, Vec::new());
        alternative_edge.control = ControlFlow::Branch;
        let mut handler_return = MachineInstruction::plain(
            MachineOpcode::Return,
            vec![MachineOperand::fixed_register_input(
                handler_value,
                PhysicalRegister::integer(21),
            )],
        );
        handler_return.control = ControlFlow::Return;
        let mut normal_return = MachineInstruction::plain(
            MachineOpcode::Return,
            vec![MachineOperand::register_input(source)],
        );
        normal_return.control = ControlFlow::Return;
        let complete_effects = CallEffects::READS_HEAP
            .union(CallEffects::WRITES_HEAP)
            .union(CallEffects::INVALIDATES_SHAPES)
            .union(CallEffects::REENTRANT);

        InstructionSequence::new(
            MachineBlock(0),
            vec![
                MachineRepresentation::Tagged,
                MachineRepresentation::Boolean,
                MachineRepresentation::Tagged,
                MachineRepresentation::Tagged,
            ],
            vec![
                CallDescriptor {
                    target: CallTarget::CommittedRuntime {
                        target: otter_vm::native_abi::STUB_JIT_SCALAR_VALUE,
                        logical_pc: 13,
                        byte_pc: 37,
                        semantic_arity: 0,
                    },
                    arguments: Vec::new(),
                    results: vec![MachineRepresentation::Tagged],
                    effects: complete_effects,
                    clobbers: TargetRegisterFile::aarch64_scalar_call_clobbers(),
                    exceptional: ExceptionalEdge::LandingPad(MachineBlock(2)),
                    safepoint: SafepointKind::Gc,
                },
                CallDescriptor {
                    target: CallTarget::RuntimeStub(
                        otter_vm::native_abi::STUB_JIT_ACKNOWLEDGE_CAUGHT_THROW,
                    ),
                    arguments: Vec::new(),
                    results: Vec::new(),
                    effects: CallEffects::WRITES_HEAP,
                    clobbers: TargetRegisterFile::aarch64_scalar_call_clobbers(),
                    exceptional: ExceptionalEdge::None,
                    safepoint: SafepointKind::None,
                },
            ],
            vec![
                MachineBlockData {
                    first: MachineInstructionId(0),
                    end: MachineInstructionId(4),
                    predecessors: Vec::new(),
                    successors: vec![MachineBlock(1), MachineBlock(2)],
                    parameters: Vec::new(),
                    successor_arguments: vec![Vec::new(), Vec::new()],
                },
                MachineBlockData {
                    first: MachineInstructionId(4),
                    end: MachineInstructionId(5),
                    predecessors: vec![MachineBlock(0)],
                    successors: vec![MachineBlock(3), MachineBlock(5)],
                    parameters: Vec::new(),
                    successor_arguments: vec![Vec::new(), Vec::new()],
                },
                MachineBlockData {
                    first: MachineInstructionId(5),
                    end: MachineInstructionId(7),
                    predecessors: vec![MachineBlock(0)],
                    successors: vec![MachineBlock(4)],
                    parameters: Vec::new(),
                    successor_arguments: vec![vec![exception]],
                },
                MachineBlockData {
                    first: MachineInstructionId(7),
                    end: MachineInstructionId(8),
                    predecessors: vec![MachineBlock(1)],
                    successors: vec![MachineBlock(4)],
                    parameters: Vec::new(),
                    successor_arguments: vec![vec![source]],
                },
                MachineBlockData {
                    first: MachineInstructionId(8),
                    end: MachineInstructionId(9),
                    predecessors: vec![MachineBlock(2), MachineBlock(3)],
                    successors: Vec::new(),
                    parameters: vec![handler_value],
                    successor_arguments: Vec::new(),
                },
                MachineBlockData {
                    first: MachineInstructionId(9),
                    end: MachineInstructionId(10),
                    predecessors: vec![MachineBlock(1)],
                    successors: Vec::new(),
                    parameters: Vec::new(),
                    successor_arguments: Vec::new(),
                },
            ],
            vec![
                MachineInstruction::plain(
                    MachineOpcode::EntryValue(0),
                    vec![MachineOperand::register_output(source)],
                ),
                MachineInstruction::plain(
                    MachineOpcode::IntegerConstant(1),
                    vec![MachineOperand::register_output(condition)],
                ),
                call,
                call_jump,
                normal_branch,
                acknowledge,
                exceptional_edge,
                alternative_edge,
                handler_return,
                normal_return,
            ],
        )
        .expect("valid committed-runtime landing sequence")
    }

    #[test]
    fn committed_element_key_boxes_scalar_late_home_only_on_cold_path() {
        let value = MachineValue(11);
        let location = AllocatedLocation::Stack(7);
        assert_eq!(
            committed_element_key(MachineRepresentation::Tagged, value, location)
                .expect("tagged key"),
            CommittedElementKey::Tagged(value)
        );
        assert_eq!(
            committed_element_key(MachineRepresentation::Int32, value, location)
                .expect("Int32 key"),
            CommittedElementKey::Int32(location)
        );
        assert_eq!(
            committed_element_key(MachineRepresentation::Uint32, value, location)
                .expect("Uint32 key"),
            CommittedElementKey::Uint32(location)
        );
    }

    #[test]
    fn committed_element_cold_stack_preserves_alignment_and_deopt_reservation() {
        assert_eq!(COMMITTED_ELEMENT_CALLER_SAVE_BYTES, 144);
        assert_eq!(COMMITTED_ELEMENT_ROOT_RECORD_BYTES, 32);
        assert_eq!(COMMITTED_ELEMENT_COLD_STACK_BYTES, 176);
        assert_eq!(COMMITTED_ELEMENT_CALLER_SAVE_BYTES % 16, 0);
        assert_eq!(COMMITTED_ELEMENT_COLD_STACK_BYTES % 16, 0);
        assert_eq!(
            COMMITTED_ELEMENT_GPR_SAVE_PAIRS
                .iter()
                .flat_map(|&(first, second, _)| [first, second])
                .collect::<Vec<_>>(),
            [0, 1, 2, 3, 4, 5, 6, 7, 8, 10]
        );
        assert_eq!(
            COMMITTED_ELEMENT_FP_SAVE_PAIRS
                .iter()
                .flat_map(|&(first, second, _)| [first, second])
                .collect::<Vec<_>>(),
            [0, 1, 2, 3, 4, 5, 6, 7]
        );
        assert_eq!(
            COMMITTED_ELEMENT_GPR_SAVE_PAIRS
                .iter()
                .chain(&COMMITTED_ELEMENT_FP_SAVE_PAIRS)
                .map(|&(_, _, offset)| offset)
                .collect::<Vec<_>>(),
            (0..COMMITTED_ELEMENT_CALLER_SAVE_BYTES)
                .step_by(16)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn committed_runtime_emitter_owns_effect_and_preserving_pc_resume_regions() {
        let transitions = TransitionTable::resolve();
        for semantic_arity in 0..=2 {
            let sequence = committed_runtime_sequence(semantic_arity);
            let allocation = sequence
                .allocate(&TargetRegisterFile::aarch64_scalar_function())
                .expect("committed-runtime allocation");
            let safepoints =
                lower_safepoints(&sequence, &allocation).expect("committed-runtime safepoints");
            let call_id = sequence
                .instructions()
                .iter()
                .position(|instruction| matches!(instruction.opcode, MachineOpcode::Call(0)))
                .map(|index| MachineInstructionId(index as u32))
                .expect("committed-runtime call");
            assert_eq!(
                safepoints
                    .site(call_id)
                    .expect("committed-runtime safepoint site")
                    .roots
                    .len(),
                usize::from(semantic_arity.max(1))
            );
            let frame = frame_layout(&allocation, safepoints.root_slot_count())
                .expect("committed-runtime frame");
            let view = JitCompileSnapshot::without_feedback(17, 2, 3, Vec::new());
            let mut load_ic_cells = [];
            let mut store_ic_cells = [];
            let emission = emit(
                &view,
                &sequence,
                &allocation,
                frame,
                &DeoptRuntime::default(),
                &safepoints,
                &transitions,
                1,
                1,
                1,
                1,
                1,
                1,
                1,
                1,
                1,
                1,
                1,
                1,
                1,
                1,
                1,
                1,
                1,
                1,
                1,
                1,
                1,
                1,
                1,
                &mut load_ic_cells,
                &mut store_ic_cells,
                3,
                true,
            )
            .expect("committed-runtime emission");
            assert_eq!(
                emission.generated_stack_frame_bytes,
                frame.frame_bytes() + super::MACHINE_ROOT_RECORD_SIZE
            );
            assert!(
                emission
                    .structural_regions
                    .iter()
                    .any(
                        |&(kind, byte_pc, start, end)| kind == "machineCommittedValueEffect"
                            && byte_pc == Some(29)
                            && start < end
                    )
            );
            let relocations = emission
                .relocations
                .render(emission.code.bytes())
                .expect("committed-runtime relocations");
            assert!(relocations.json.contains("jit_scalar_value"));
            assert!(!relocations.json.contains("jit_route_throw"));
        }
    }

    #[test]
    fn committed_runtime_landing_handles_distinct_exception_phi_home() {
        let sequence = committed_runtime_landing_sequence();
        let allocation = sequence
            .allocate(&TargetRegisterFile::aarch64_scalar_function())
            .expect("committed-runtime landing allocation");
        let call_id = MachineInstructionId(2);
        let call_result_home = allocation
            .instruction_locations(call_id)
            .expect("committed call allocation")[0];
        let handler_use_home = allocation
            .instruction_locations(MachineInstructionId(8))
            .expect("handler use allocation")[0];
        assert_eq!(
            call_result_home,
            AllocatedLocation::Register(PhysicalRegister::integer(20))
        );
        assert_eq!(
            handler_use_home,
            AllocatedLocation::Register(PhysicalRegister::integer(21))
        );
        assert_ne!(call_result_home, handler_use_home);
        let safepoints =
            lower_safepoints(&sequence, &allocation).expect("committed-runtime landing roots");
        assert_eq!(
            safepoints
                .site(call_id)
                .expect("committed-runtime landing safepoint")
                .roots
                .len(),
            1
        );
        let frame = frame_layout(&allocation, safepoints.root_slot_count())
            .expect("committed-runtime landing frame");
        let transitions = TransitionTable::resolve();
        let view = JitCompileSnapshot::without_feedback(18, 2, 4, Vec::new());
        let mut load_ic_cells = [];
        let mut store_ic_cells = [];
        let emission = emit(
            &view,
            &sequence,
            &allocation,
            frame,
            &DeoptRuntime::default(),
            &safepoints,
            &transitions,
            1,
            1,
            1,
            1,
            1,
            1,
            1,
            1,
            1,
            1,
            1,
            1,
            1,
            1,
            1,
            1,
            1,
            1,
            1,
            1,
            1,
            1,
            1,
            &mut load_ic_cells,
            &mut store_ic_cells,
            4,
            true,
        )
        .expect("committed-runtime landing emission");
        assert!(
            emission
                .structural_regions
                .iter()
                .any(
                    |&(kind, byte_pc, start, end)| kind == "machineCommittedValueEffect"
                        && byte_pc == Some(37)
                        && start < end
                )
        );
        let relocations = emission
            .relocations
            .render(emission.code.bytes())
            .expect("committed-runtime landing relocations");
        assert!(relocations.json.contains("jit_scalar_value"));
        assert!(relocations.json.contains("jit_acknowledge_caught_throw"));
        assert!(!relocations.json.contains("jit_route_throw"));
    }

    /// Rust's saturating float-to-Uint32 cast models AArch64 `fcvtzu`; the
    /// emitter's following `ucvtf`/`fcmp` is exactly this equality test.
    fn checked_element_index_model(value: f64) -> Option<u32> {
        let index = value as u32;
        (value == f64::from(index)).then_some(index)
    }

    #[test]
    fn checked_element_index_accepts_only_exact_uint32_values() {
        assert_eq!(checked_element_index_model(0.0), Some(0));
        assert_eq!(checked_element_index_model(-0.0), Some(0));
        assert_eq!(checked_element_index_model(1.0), Some(1));
        assert_eq!(
            checked_element_index_model(f64::from(u32::MAX)),
            Some(u32::MAX)
        );

        for rejected in [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            -1.0,
            0.5,
            f64::from(u32::MAX) + 1.0,
        ] {
            assert_eq!(checked_element_index_model(rejected), None, "{rejected}");
        }
    }

    #[test]
    fn upvalue_binding_guard_materializes_the_value_field_offset() {
        let sequence = committed_runtime_sequence(0);
        let allocation = sequence
            .allocate(&TargetRegisterFile::aarch64_scalar_function())
            .expect("fixture allocation");
        let frame = frame_layout(&allocation, 1).expect("fixture frame");
        let mut view = JitCompileSnapshot::without_feedback(19, 0, 1, Vec::new());
        view.cage_base = 0x1000;
        view.upvalue_value_byte = 0x38;
        let mut assembler = dynasmrt::aarch64::Assembler::new().expect("binding assembler");
        let mut relocations = RelocationCapture::default();
        emit_binding_guard(
            &mut assembler,
            &mut relocations,
            &view,
            frame,
            BindingSemantics::Read(BindingRead::Upvalue {
                destination: 0,
                index: 1,
            }),
            MachineBindingTarget::Upvalue { index: 3 },
            24,
            &[
                AllocatedLocation::Register(PhysicalRegister::integer(0)),
                AllocatedLocation::Register(PhysicalRegister::integer(1)),
                AllocatedLocation::Register(PhysicalRegister::integer(2)),
            ],
        )
        .expect("upvalue binding guard emission");
        let code = assembler.finalize().expect("binding guard code");

        let mut expected = dynasmrt::aarch64::Assembler::new().expect("expected assembler");
        emit_load_u64(&mut expected, 16, u64::from(view.upvalue_value_byte));
        let expected = expected.finalize().expect("offset load code");
        assert!(
            code.as_ref()
                .windows(expected.len())
                .any(|window| window == expected.as_ref()),
            "generated upvalue storage address must materialize upvalue_value_byte"
        );
    }

    #[test]
    fn global_binding_guards_defensively_reject_a_missing_cage() {
        let sequence = committed_runtime_sequence(0);
        let allocation = sequence
            .allocate(&TargetRegisterFile::aarch64_scalar_function())
            .expect("fixture allocation");
        let frame = frame_layout(&allocation, 1).expect("fixture frame");
        let view = JitCompileSnapshot::without_feedback(20, 0, 1, Vec::new());
        let semantics = BindingSemantics::Read(BindingRead::Global {
            destination: 0,
            name: 1,
            missing: BindingMissing::Throw,
        });
        let locations = [
            AllocatedLocation::Register(PhysicalRegister::integer(0)),
            AllocatedLocation::Register(PhysicalRegister::integer(1)),
            AllocatedLocation::Register(PhysicalRegister::integer(2)),
        ];
        for target in [
            MachineBindingTarget::Global(otter_vm::jit::BindingHitProof::GlobalLexical {
                cell_offset: 0x40,
                writable: true,
            }),
            MachineBindingTarget::Global(otter_vm::jit::BindingHitProof::GlobalObject {
                shape: 7,
                dictionary: false,
                value_byte: 16,
                global_lexical_epoch: 3,
                writable: true,
            }),
        ] {
            let mut assembler = dynasmrt::aarch64::Assembler::new().expect("binding assembler");
            let mut relocations = RelocationCapture::default();
            assert!(
                emit_binding_guard(
                    &mut assembler,
                    &mut relocations,
                    &view,
                    frame,
                    semantics,
                    target,
                    24,
                    &locations,
                )
                .is_err(),
                "global proof without a cage must not become generated code"
            );
        }
    }
}
