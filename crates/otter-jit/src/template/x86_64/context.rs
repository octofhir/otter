//! Per-scope binding-context emission for Template x86-64.
//!
//! # Contents
//! - `LoadClosureContext`: SELF's closure context word.
//! - Unchecked `LoadContextSlot` / `StoreContextSlot` over a static hop count.
//! - The inline hit path of the checked (TDZ) slot accesses.
//! - Context and explicit lexical closure construction carved from the linear
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
//! - The allocating slow path passes five physical words through the shared
//!   platform C boundary and restores its stack-owned allocation context.
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
) -> Result<usize, Unsupported> {
    let offset = slot_offset(view, slot)?;
    emit_load_reg(ops, 10, context);
    emit_parent_hops(ops, view, 10, depth)?;
    dynasm!(ops ; .arch x64 ; mov rax, [r10 + offset]);
    emit_load_u64(ops, 11, VALUE_HOLE);
    dynasm!(ops ; .arch x64 ; cmp rax, r11 ; je =>miss);
    let guard_end = ops.offset().0;
    match access {
        CheckedContextAccess::Load { dst } => emit_store_reg(ops, 0, dst),
        CheckedContextAccess::Store { src } => {
            emit_load_reg(ops, 2, src);
            dynasm!(ops ; .arch x64 ; mov [r10 + offset], rdx);
            emit_template_value_barrier(ops, relocations, view, 10, 2);
        }
    }
    Ok(guard_end)
}

/// One context or closure construction through the typed allocating boundary.
#[derive(Debug, Clone, Copy)]
pub(super) enum ContextAllocation {
    Create { parent: u16, scope: u32 },
    Copy { source: u16 },
    Function,
    Closure { context: u16 },
}

/// Fully initialize a native LAB fit or call the canonical rooted Probe stub.
/// The published source PC owns function constants; refused allocations leave
/// through the original before-state without assigning the destination.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_context_allocation(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    pc: u32,
    byte_pc: u32,
    dst: u16,
    allocation: ContextAllocation,
    safepoint: abi::SafepointId,
    miss: DynamicLabel,
    fatal: DynamicLabel,
) -> Result<(), Unsupported> {
    use crate::allocation::{AllocationValue, LabRegisters};
    let owns_this = match allocation {
        ContextAllocation::Create { scope, .. } => view
            .context_allocations
            .get(&(view.code_block.id, scope))
            .is_some_and(|plan| plan.derived_this_slot.is_some()),
        _ => false,
    };
    let descriptor = match allocation {
        ContextAllocation::Create { .. } => abi::STUB_CREATE_CONTEXT_ALLOC,
        ContextAllocation::Copy { .. } => abi::STUB_COPY_CONTEXT_ALLOC,
        ContextAllocation::Function => abi::STUB_JIT_MAKE_FN,
        ContextAllocation::Closure { .. } => abi::STUB_JIT_MAKE_CLOSURE,
    };
    let stub_addr = alloc_value_stub_by_id(descriptor.id)
        .and_then(|stub| stub.entry_addr())
        .ok_or(Unsupported::OperandShape("lexical allocating stub entry"))?;
    let slow = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    let regs = LabRegisters {
        buffer: 11,
        candidate: 0,
        end: 10,
        scratch: 7,
        size: 9,
    };
    let context_register = 15;
    match allocation {
        ContextAllocation::Create { parent, scope } => {
            if let Some(plan) = view.context_allocations.get(&(view.code_block.id, scope)) {
                emit_load_reg(ops, 2, parent);
                crate::x86_64::allocation::emit_create_context(
                    ops,
                    view,
                    plan,
                    context_register,
                    AllocationValue::Register(2),
                    regs,
                    slow,
                );
            } else {
                dynasm!(ops ; .arch x64 ; jmp =>slow);
            }
        }
        ContextAllocation::Copy { source } => {
            emit_load_reg(ops, 2, source);
            crate::x86_64::allocation::emit_copy_context(
                ops,
                view,
                context_register,
                AllocationValue::Register(2),
                regs,
                slow,
            );
        }
        ContextAllocation::Function | ContextAllocation::Closure { .. } => {
            if let Some(&plan) = view.closure_allocations.get(&byte_pc) {
                let values = match allocation {
                    ContextAllocation::Closure { context } => {
                        emit_load_reg(ops, 2, context);
                        dynasm!(ops ; .arch x64 ; mov rsi, [r14 + crate::entry::NATIVE_FRAME_THIS_OFFSET as i32] ; mov r8, [r14 + crate::entry::NATIVE_FRAME_NEW_TARGET_OFFSET as i32]);
                        [
                            AllocationValue::Register(2),
                            AllocationValue::Register(6),
                            AllocationValue::Register(8),
                        ]
                    }
                    _ => [AllocationValue::Constant(crate::entry::VALUE_UNDEFINED); 3],
                };
                crate::x86_64::allocation::emit_closure(
                    ops,
                    view,
                    plan,
                    context_register,
                    values,
                    regs,
                    slow,
                );
            } else {
                dynasm!(ops ; .arch x64 ; jmp =>slow);
            }
        }
    }
    emit_store_reg(ops, regs.candidate, dst);
    dynasm!(ops ; .arch x64 ; jmp =>done ; =>slow);
    dynasm!(ops ; .arch x64 ; mov DWORD [r14 + crate::entry::NATIVE_FRAME_PC_OFFSET as i32], pc as i32
        ; sub rsp, ALLOC_CTX_STACK_SIZE as i32 ; mov r11, [r15 + THREAD_OFFSET as i32]
        ; mov [rsp + ALLOC_CTX_THREAD_OFFSET as i32], r11
        ; mov DWORD [rsp + ALLOC_CTX_SAFEPOINT_ID_OFFSET as i32], safepoint as i32
        ; mov QWORD [rsp + ALLOC_CTX_SPILL_SLOTS_OFFSET as i32], 0
        ; mov WORD [rsp + ALLOC_CTX_SPILL_SLOT_COUNT_OFFSET as i32], 0
        ; mov rdi, rsp ; mov esi, safepoint as i32);
    match allocation {
        ContextAllocation::Create { parent, scope } => {
            let function_id = i32::try_from(view.code_block.id)
                .map_err(|_| Unsupported::OperandShape("context source function id"))?;
            let scope = i32::try_from(scope)
                .map_err(|_| Unsupported::OperandShape("context scope index"))?;
            emit_load_reg(ops, 2, parent);
            emit_load_u64(ops, 1, otter_vm::Value::number_i32(function_id).to_bits());
            emit_load_u64(ops, 8, otter_vm::Value::number_i32(scope).to_bits());
        }
        ContextAllocation::Copy { source } => {
            emit_load_reg(ops, 2, source);
            emit_load_u64(ops, 1, VALUE_UNDEFINED);
            emit_load_u64(ops, 8, VALUE_UNDEFINED);
        }
        ContextAllocation::Function => {
            emit_load_u64(ops, 2, VALUE_UNDEFINED);
            emit_load_u64(ops, 1, VALUE_UNDEFINED);
            emit_load_u64(ops, 8, VALUE_UNDEFINED);
        }
        ContextAllocation::Closure { context } => {
            emit_load_reg(ops, 2, context);
            dynasm!(ops ; .arch x64 ; mov rcx, [r14 + crate::entry::NATIVE_FRAME_THIS_OFFSET as i32] ; mov r8, [r14 + crate::entry::NATIVE_FRAME_NEW_TARGET_OFFSET as i32]);
        }
    }
    emit_load_runtime_stub(ops, relocations, stub_addr as u64, descriptor);
    crate::x86_64::call_abi::emit_runtime_call(ops, descriptor);
    let success = ops.new_dynamic_label();
    dynasm!(ops ; .arch x64 ; add rsp, ALLOC_CTX_STACK_SIZE as i32 ; test rdx, rdx ; jz =>success
        ; cmp rdx, abi::NativeResultStatus::SideExit as i32 ; je =>miss
        ; cmp rdx, abi::NativeResultStatus::OutOfMemory as i32 ; je =>miss ; jmp =>fatal ; =>success);
    emit_store_reg(ops, 0, dst);
    dynasm!(ops ; .arch x64 ; =>done);
    if owns_this {
        // Reload from the just-committed window for both fit and collecting
        // success; no stale pre-call input or interior pointer is retained.
        emit_load_reg(ops, 0, dst);
        crate::x86_64::allocation::emit_publish_derived_this_context(
            ops,
            14,
            0,
            view.code_block.id,
            [10, 11],
        );
    }
    Ok(())
}
