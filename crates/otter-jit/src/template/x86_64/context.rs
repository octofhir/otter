//! Per-scope binding-context emission for Template x86-64.
//!
//! # Contents
//! - `LoadClosureContext`: SELF's closure context word.
//! - Unchecked `LoadContextSlot` / `StoreContextSlot` over a static hop count.
//! - The inline hit path of the checked (TDZ) slot accesses.
//! - `CreateContext` / `CopyContext` carved inline from the linear
//!   allocation buffer, with the VM-owned allocating ABI as the slow path.
//!
//! # Invariants
//! - A context is a heap `Value` whose word is its `GcHeader` address, so a
//!   parent hop, a slot read and a slot write are single moves at the
//!   [`otter_vm::jit::JitContextLayout`] offsets; no cage arithmetic applies.
//! - Every operation reloads its context from the traced register window. No
//!   derived pointer survives the operation.
//! - An unchecked access is proven well-formed by the bytecode verifier: the
//!   register holds a context whose chain is at least `depth` hops deep. It
//!   never throws and never publishes a PC.
//! - A slot store of a cell value runs the canonical write barrier.
//! - An inline carve cannot collect. The slow path publishes a full-window
//!   safepoint; a refused allocation exits before any effect at the original
//!   opcode, which the interpreter re-executes (and which owns the
//!   heap-limit `RangeError`).
//! - A checked access completes inline when its slot holds a value; only the
//!   `hole` (TDZ) outcome branches to the committed binding boundary, which
//!   owns the `ReferenceError`. Lookup accesses always complete there.
//!
//! # See also
//! - `crate::template::arm64::context` — the peer target emitter.
//! - `otter_vm::context` — the context body and its initialization contract.

use super::*;

/// Hops unrolled inline; a deeper chain walks in a counted loop.
const MAX_UNROLLED_HOPS: u16 = 8;

fn displacement(offset: u32) -> Result<i32, Unsupported> {
    i32::try_from(offset).map_err(|_| Unsupported::OperandShape("context layout displacement"))
}

/// Byte offset of `slot` from a context's header address.
fn slot_offset(view: &JitCompileSnapshot, slot: u16) -> Result<i32, Unsupported> {
    view.context_layout
        .slots_byte
        .checked_add(u32::from(slot) * 8)
        .ok_or(Unsupported::OperandShape("context slot offset"))
        .and_then(displacement)
}

/// Follow `depth` parent links from the context in `Rq(register)`. Clobbers
/// `rcx` for a counted walk.
fn emit_parent_hops(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    register: u8,
    depth: u16,
) -> Result<(), Unsupported> {
    let parent = displacement(view.context_layout.parent_byte)?;
    if depth <= MAX_UNROLLED_HOPS {
        for _ in 0..depth {
            dynasm!(ops ; .arch x64 ; mov Rq(register), [Rq(register) + parent]);
        }
        return Ok(());
    }
    let hop = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch x64
        ; mov ecx, i32::from(depth)
        ; =>hop
        ; mov Rq(register), [Rq(register) + parent]
        ; sub ecx, 1
        ; jnz =>hop
    );
    Ok(())
}

/// `r<dst>` = SELF's closure context, or `undefined` for a bare function
/// value. Clobbers `rax`, `r11`.
pub(super) fn emit_load_closure_context(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    dst: u16,
) -> Result<(), Unsupported> {
    let context = displacement(view.closure_call_layout.context_byte)?;
    let no_context = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops ; .arch x64 ; mov rax, [r14 + NATIVE_FRAME_SELF_OFFSET as i32]);
    emit_load_u64(ops, 11, NOT_CELL_MASK);
    dynasm!(ops
        ; .arch x64
        ; test rax, r11
        ; jnz =>no_context
        ; cmp BYTE [rax], otter_vm::closure::JS_CLOSURE_BODY_TYPE_TAG as i8
        ; jne =>no_context
        ; mov rax, [rax + context]
        ; jmp =>done
        ; =>no_context
    );
    emit_load_u64(ops, 0, VALUE_UNDEFINED);
    dynasm!(ops ; .arch x64 ; =>done);
    emit_store_reg(ops, 0, dst);
    Ok(())
}

/// `rax = r<context>.parent^depth.slots[slot]` without a store back.
/// Clobbers `rcx` for a counted walk.
pub(super) fn emit_read_context_slot_rax(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    context: u16,
    depth: u16,
    slot: u16,
) -> Result<(), Unsupported> {
    let offset = slot_offset(view, slot)?;
    emit_load_reg(ops, 0, context);
    emit_parent_hops(ops, view, 0, depth)?;
    dynasm!(ops ; .arch x64 ; mov rax, [rax + offset]);
    Ok(())
}

/// Unchecked `r<dst> = r<context>.parent^depth.slots[slot]`. Clobbers `rax`,
/// `rcx`.
pub(super) fn emit_load_context_slot(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    dst: u16,
    context: u16,
    depth: u16,
    slot: u16,
) -> Result<(), Unsupported> {
    let offset = slot_offset(view, slot)?;
    emit_load_reg(ops, 0, context);
    emit_parent_hops(ops, view, 0, depth)?;
    dynasm!(ops ; .arch x64 ; mov rax, [rax + offset]);
    emit_store_reg(ops, 0, dst);
    Ok(())
}

/// Unchecked, write-barriered `r<context>.parent^depth.slots[slot] = r<src>`.
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
    emit_load_reg(ops, 10, context);
    emit_parent_hops(ops, view, 10, depth)?;
    emit_load_reg(ops, 2, src);
    dynasm!(ops ; .arch x64 ; mov [r10 + offset], rdx);
    emit_template_value_barrier(ops, relocations, view, 10, 2);
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
/// with no effect; otherwise the access completes and falls through.
/// Clobbers `rax`, `rcx`, `rdx`, `r10`, `r11` and the barrier's scratch.
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
) -> Result<(), Unsupported> {
    let offset = slot_offset(view, slot)?;
    emit_load_reg(ops, 10, context);
    emit_parent_hops(ops, view, 10, depth)?;
    dynasm!(ops ; .arch x64 ; mov rax, [r10 + offset]);
    emit_load_u64(ops, 11, VALUE_HOLE);
    dynasm!(ops ; .arch x64 ; cmp rax, r11 ; je =>miss);
    match access {
        CheckedContextAccess::Load { dst } => emit_store_reg(ops, 0, dst),
        CheckedContextAccess::Store { src } => {
            emit_load_reg(ops, 2, src);
            dynasm!(ops ; .arch x64 ; mov [r10 + offset], rdx);
            emit_template_value_barrier(ops, relocations, view, 10, 2);
        }
    }
    Ok(())
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

/// Allocate one context and commit it to `r<dst>`: carved inline from the
/// linear allocation buffer when it fits, otherwise through the
/// `AllocValue3` boundary. A refused allocation exits at the original opcode
/// before any effect.
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
    let slow = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    match allocation {
        ContextAllocation::Create { parent, scope } => {
            let Some(plan) = view.context_allocations.get(&(view.code_block.id, scope)) else {
                return emit_context_allocation_call(
                    ops,
                    relocations,
                    view,
                    dst,
                    allocation,
                    descriptor,
                    stub_addr,
                    safepoint,
                    miss,
                );
            };
            emit_load_reg(ops, 2, parent);
            crate::x86_64::allocation::emit_create_context(ops, view, plan, slow);
        }
        ContextAllocation::Copy { source } => {
            emit_load_reg(ops, 2, source);
            crate::x86_64::allocation::emit_copy_context(ops, view, slow);
        }
    }
    emit_store_reg(ops, 0, dst);
    dynasm!(ops ; .arch x64 ; jmp =>done ; =>slow);
    emit_context_allocation_call(
        ops,
        relocations,
        view,
        dst,
        allocation,
        descriptor,
        stub_addr,
        safepoint,
        miss,
    )?;
    dynasm!(ops ; .arch x64 ; =>done);
    Ok(())
}

/// The allocating-call half of [`emit_context_allocation`].
#[allow(clippy::too_many_arguments)]
fn emit_context_allocation_call(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    dst: u16,
    allocation: ContextAllocation,
    descriptor: abi::RuntimeStubDescriptor,
    stub_addr: usize,
    safepoint: abi::SafepointId,
    miss: DynamicLabel,
) -> Result<(), Unsupported> {
    dynasm!(ops
        ; .arch x64
        ; sub rsp, ALLOC_CTX_STACK_SIZE as i32
        ; mov r11, [r15 + THREAD_OFFSET as i32]
        ; mov [rsp + ALLOC_CTX_THREAD_OFFSET as i32], r11
        ; mov DWORD [rsp + ALLOC_CTX_SAFEPOINT_ID_OFFSET as i32], safepoint as i32
        ; mov QWORD [rsp + ALLOC_CTX_SPILL_SLOTS_OFFSET as i32], 0
        ; mov WORD [rsp + ALLOC_CTX_SPILL_SLOT_COUNT_OFFSET as i32], 0
        ; mov rdi, rsp
        ; mov esi, safepoint as i32
    );
    match allocation {
        ContextAllocation::Create { parent, scope } => {
            let function_id = i32::try_from(view.code_block.id)
                .map_err(|_| Unsupported::OperandShape("CreateContext function id"))?;
            let scope = i32::try_from(scope)
                .map_err(|_| Unsupported::OperandShape("CreateContext scope index"))?;
            emit_load_reg(ops, 2, parent);
            emit_load_u64(ops, 1, otter_vm::Value::number_i32(function_id).to_bits());
            emit_load_u64(ops, 8, otter_vm::Value::number_i32(scope).to_bits());
        }
        ContextAllocation::Copy { source } => {
            emit_load_reg(ops, 2, source);
            emit_load_u64(ops, 1, VALUE_UNDEFINED);
            emit_load_u64(ops, 8, VALUE_UNDEFINED);
        }
    }
    emit_load_runtime_stub(ops, relocations, stub_addr as u64, descriptor);
    dynasm!(ops
        ; .arch x64
        ; call r11
        ; add rsp, ALLOC_CTX_STACK_SIZE as i32
        ; test rdx, rdx
        ; jne =>miss
    );
    emit_store_reg(ops, 0, dst);
    Ok(())
}
