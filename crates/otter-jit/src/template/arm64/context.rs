//! Per-scope binding-context emission for Template AArch64.
//!
//! # Contents
//! - `LoadClosureContext`: SELF's closure context word.
//! - Unchecked `LoadContextSlot` / `StoreContextSlot` over a static hop count.
//! - The inline hit path of the checked (TDZ) slot accesses.
//! - `CreateContext` / `CopyContext` through the VM-owned allocating ABI.
//!
//! # Invariants
//! - A context is a heap `Value` whose word is its `GcHeader` address, so a
//!   parent hop, a slot read and a slot write are single loads/stores at the
//!   [`otter_vm::jit::JitContextLayout`] offsets; no cage arithmetic applies.
//! - Every operation reloads its context from the traced register window. No
//!   derived pointer survives the operation, so a moving collection between
//!   two operations is always observed.
//! - An unchecked access is proven well-formed by the bytecode verifier: the
//!   register holds a context whose chain is at least `depth` hops deep. It
//!   never throws and never publishes a PC.
//! - A slot store of a cell value runs the canonical write barrier: contexts
//!   are young and movable, and incremental marking needs the insertion half.
//! - Allocation publishes a full-window safepoint; a refused allocation exits
//!   before any effect at the original opcode, which the interpreter
//!   re-executes (and which owns the heap-limit `RangeError`).
//! - A checked access completes inline when its slot holds a value; only the
//!   `hole` (TDZ) outcome branches to the committed binding boundary in
//!   [`super::binding`], which owns the `ReferenceError`. Lookup accesses
//!   always complete there.
//!
//! # See also
//! - `otter_vm::context` — the context body and its initialization contract.
//! - [`super::transitions`] — the shared allocating call packet.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::runtime_stubs::alloc_value_stub_by_id;
use otter_vm::{JitCompileSnapshot, closure::JS_CLOSURE_BODY_TYPE_TAG, native_abi as abi};

use super::values::{
    CellTest, emit_cell_test, emit_load_reg, emit_load_runtime_stub, emit_load_u64, emit_store_reg,
    emit_write_barrier,
};
use crate::artifact::relocation::RelocationCapture;
use crate::entry::{
    ALLOC_CTX_SAFEPOINT_ID_OFFSET, ALLOC_CTX_SPILL_SLOT_COUNT_OFFSET, ALLOC_CTX_SPILL_SLOTS_OFFSET,
    ALLOC_CTX_STACK_SIZE, ALLOC_CTX_THREAD_OFFSET, NATIVE_FRAME_SELF_OFFSET, THREAD_OFFSET,
    Unsupported, VALUE_HOLE, VALUE_UNDEFINED,
};

/// Largest scaled unsigned 64-bit `ldr`/`str` displacement.
const MAX_SCALED_OFFSET: u32 = 32_760;

/// Hops unrolled inline; a deeper chain walks in a counted loop.
const MAX_UNROLLED_HOPS: u16 = 8;

/// `X(target) = [X(base) + offset]` for any 8-byte-aligned offset.
fn emit_load_at(ops: &mut Assembler, target: u8, base: u8, offset: u32, scratch: u8) {
    if offset <= MAX_SCALED_OFFSET {
        dynasm!(ops ; .arch aarch64 ; ldr X(target), [X(base), offset]);
    } else {
        emit_load_u64(ops, scratch, u64::from(offset));
        dynasm!(ops ; .arch aarch64 ; ldr X(target), [X(base), X(scratch)]);
    }
}

fn aligned(offset: u32) -> Result<u32, Unsupported> {
    if offset.is_multiple_of(8) {
        Ok(offset)
    } else {
        Err(Unsupported::OperandShape("unaligned context layout word"))
    }
}

/// Byte offset of `slot` from a context's header address.
fn slot_offset(view: &JitCompileSnapshot, slot: u16) -> Result<u32, Unsupported> {
    aligned(view.context_layout.slots_byte)?
        .checked_add(u32::from(slot) * 8)
        .ok_or(Unsupported::OperandShape("context slot offset"))
}

/// Follow `depth` parent links from the context in `X(register)`. Clobbers
/// `x10` for a counted walk.
fn emit_parent_hops(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    register: u8,
    depth: u16,
) -> Result<(), Unsupported> {
    let parent = aligned(view.context_layout.parent_byte)?;
    if depth <= MAX_UNROLLED_HOPS {
        for _ in 0..depth {
            emit_load_at(ops, register, register, parent, 10);
        }
        return Ok(());
    }
    let hop = ops.new_dynamic_label();
    emit_load_u64(ops, 10, u64::from(depth));
    dynasm!(ops ; .arch aarch64 ; =>hop);
    emit_load_at(ops, register, register, parent, 11);
    dynasm!(ops
        ; .arch aarch64
        ; subs w10, w10, #1
        ; b.ne =>hop
    );
    Ok(())
}

/// `r<dst>` = SELF's closure context, or `undefined` for a bare function
/// value (a function created with no context). Clobbers `x9`, `x10`.
pub(super) fn emit_load_closure_context(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    dst: u16,
) -> Result<(), Unsupported> {
    let context_byte = aligned(view.closure_call_layout.context_byte)?;
    let no_context = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64 ; ldr x9, [x21, NATIVE_FRAME_SELF_OFFSET]);
    emit_cell_test(ops, 9, 10, CellTest::IsNotCell, no_context);
    dynasm!(ops
        ; .arch aarch64
        ; ldrb w10, [x9]
        ; cmp w10, JS_CLOSURE_BODY_TYPE_TAG as u32
        ; b.ne =>no_context
    );
    emit_load_at(ops, 9, 9, context_byte, 10);
    dynasm!(ops ; .arch aarch64 ; b =>done ; =>no_context);
    emit_load_u64(ops, 9, VALUE_UNDEFINED);
    dynasm!(ops ; .arch aarch64 ; =>done);
    emit_store_reg(ops, 9, dst)
}

/// `X(target) = r<context>.parent^depth.slots[slot]` without a store back.
/// `target` must not be `x10`/`x11`, the counted-walk scratch registers.
pub(super) fn emit_read_context_slot(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    target: u8,
    context: u16,
    depth: u16,
    slot: u16,
) -> Result<(), Unsupported> {
    let offset = slot_offset(view, slot)?;
    emit_load_reg(ops, target, context)?;
    emit_parent_hops(ops, view, target, depth)?;
    emit_load_at(ops, target, target, offset, 10);
    Ok(())
}

/// Unchecked `r<dst> = r<context>.parent^depth.slots[slot]`. Clobbers
/// `x9`–`x11`.
pub(super) fn emit_load_context_slot(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    dst: u16,
    context: u16,
    depth: u16,
    slot: u16,
) -> Result<(), Unsupported> {
    let offset = slot_offset(view, slot)?;
    emit_load_reg(ops, 9, context)?;
    emit_parent_hops(ops, view, 9, depth)?;
    emit_load_at(ops, 9, 9, offset, 10);
    emit_store_reg(ops, 9, dst)
}

/// Unchecked, write-barriered `r<context>.parent^depth.slots[slot] = r<src>`.
/// The fast path clobbers `x9`–`x16`; the barrier's slow path is an AAPCS
/// leaf call.
pub(super) fn emit_store_context_slot(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    src: u16,
    context: u16,
    depth: u16,
    slot: u16,
) -> Result<(), Unsupported> {
    let offset = slot_offset(view, slot)?;
    let done = ops.new_dynamic_label();
    emit_load_reg(ops, 13, context)?;
    emit_parent_hops(ops, view, 13, depth)?;
    emit_store_into_target(ops, relocations, view, src, offset, done)?;
    dynasm!(ops ; .arch aarch64 ; =>done);
    Ok(())
}

/// `[x13 + offset] = r<src>` with the canonical write barrier, then branch
/// to `done`. Clobbers `x9`, `x11`, `x12` and the barrier's scratch.
fn emit_store_into_target(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    src: u16,
    offset: u32,
    done: DynamicLabel,
) -> Result<(), Unsupported> {
    emit_load_reg(ops, 9, src)?;
    if offset <= MAX_SCALED_OFFSET {
        dynasm!(ops ; .arch aarch64 ; str x9, [x13, offset]);
    } else {
        emit_load_u64(ops, 12, u64::from(offset));
        dynasm!(ops ; .arch aarch64 ; str x9, [x13, x12]);
    }
    emit_cell_test(ops, 9, 11, CellTest::IsNotCell, done);
    emit_write_barrier(ops, relocations, view, 13, 9);
    dynasm!(ops ; .arch aarch64 ; b =>done);
    Ok(())
}

/// One checked (TDZ) context-slot access whose hit path completes inline.
#[derive(Debug, Clone, Copy)]
pub(super) enum CheckedContextAccess {
    /// `LoadContextSlotChecked`: `r<dst> = slot`.
    Load { dst: u16 },
    /// `StoreContextSlotChecked`: `slot = r<src>`.
    Store { src: u16 },
}

/// Inline hit path of a checked context-slot access over
/// `r<context>.parent^depth.slots[slot]`: a `hole` slot branches to `miss`
/// with no effect; otherwise the access completes and branches to `done`.
/// Returns the code offset where the hole guard ends and the hit begins.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_checked_context_slot(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    access: CheckedContextAccess,
    context: u16,
    depth: u16,
    slot: u16,
    miss: DynamicLabel,
    done: DynamicLabel,
) -> Result<usize, Unsupported> {
    let offset = slot_offset(view, slot)?;
    emit_load_reg(ops, 13, context)?;
    emit_parent_hops(ops, view, 13, depth)?;
    emit_load_at(ops, 9, 13, offset, 12);
    emit_load_u64(ops, 11, VALUE_HOLE);
    dynasm!(ops ; .arch aarch64 ; cmp x9, x11 ; b.eq =>miss);
    let guard_end = ops.offset().0;
    match access {
        CheckedContextAccess::Load { dst } => {
            emit_store_reg(ops, 9, dst)?;
            dynasm!(ops ; .arch aarch64 ; b =>done);
        }
        CheckedContextAccess::Store { src } => {
            emit_store_into_target(ops, relocations, view, src, offset, done)?;
        }
    }
    Ok(guard_end)
}

/// Which context allocation one call performs.
#[derive(Debug, Clone, Copy)]
pub(super) enum ContextAllocation {
    /// `CreateContext`: a fresh context of this function's scope `scope`
    /// under the context (or `undefined`) in register `parent`.
    Create { parent: u16, scope: u32 },
    /// `CopyContext`: a per-iteration copy of the context in register
    /// `source`.
    Copy { source: u16 },
}

/// Allocate one context through the `AllocValue3` boundary and commit it to
/// `r<dst>`. A refused allocation exits at the original opcode before any
/// effect.
pub(super) fn emit_context_allocation(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    dst: u16,
    allocation: ContextAllocation,
    safepoint: abi::SafepointId,
    miss: DynamicLabel,
) -> Result<(), Unsupported> {
    let descriptor = match allocation {
        ContextAllocation::Create { .. } => abi::STUB_CREATE_CONTEXT_ALLOC,
        ContextAllocation::Copy { .. } => abi::STUB_COPY_CONTEXT_ALLOC,
    };
    let stub_addr = alloc_value_stub_by_id(descriptor.id)
        .and_then(|stub| stub.entry_addr())
        .ok_or(Unsupported::OperandShape("context allocating stub entry"))?;
    dynasm!(ops
        ; .arch aarch64
        ; sub sp, sp, ALLOC_CTX_STACK_SIZE
        ; ldr x9, [x20, THREAD_OFFSET]
        ; str x9, [sp, ALLOC_CTX_THREAD_OFFSET]
    );
    emit_load_u64(ops, 9, u64::from(safepoint));
    dynasm!(ops
        ; .arch aarch64
        ; str w9, [sp, ALLOC_CTX_SAFEPOINT_ID_OFFSET]
        ; strh wzr, [sp, ALLOC_CTX_SPILL_SLOT_COUNT_OFFSET]
        ; str xzr, [sp, ALLOC_CTX_SPILL_SLOTS_OFFSET]
        ; mov x0, sp
    );
    emit_load_u64(ops, 1, u64::from(safepoint));
    match allocation {
        ContextAllocation::Create { parent, scope } => {
            let function_id = i32::try_from(view.code_block.id)
                .map_err(|_| Unsupported::OperandShape("CreateContext function id"))?;
            let scope = i32::try_from(scope)
                .map_err(|_| Unsupported::OperandShape("CreateContext scope index"))?;
            emit_load_reg(ops, 2, parent)?;
            emit_load_u64(ops, 3, otter_vm::Value::number_i32(function_id).to_bits());
            emit_load_u64(ops, 4, otter_vm::Value::number_i32(scope).to_bits());
        }
        ContextAllocation::Copy { source } => {
            emit_load_reg(ops, 2, source)?;
            emit_load_u64(ops, 3, VALUE_UNDEFINED);
            emit_load_u64(ops, 4, VALUE_UNDEFINED);
        }
    }
    emit_load_runtime_stub(ops, relocations, 16, stub_addr as u64, descriptor);
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; mov x5, x1
        ; add sp, sp, ALLOC_CTX_STACK_SIZE
        ; cbnz x5, =>miss
    );
    emit_store_reg(ops, 0, dst)
}
