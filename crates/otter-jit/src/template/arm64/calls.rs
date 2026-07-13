//! AArch64 call emitters for the template compiler.
//!
//! # Contents
//! - Plain-call and method-call lowering through the prepare transitions.
//! - The shared direct-call dispatch tail building the callee's `JitCtx`
//!   and publishing its own `NativeFrame`.
//!
//! # Invariants
//! - The caller's canonical PC is stamped before the prepare transition; a
//!   bailed callee reifies at its exact PC through the finish helpers and the
//!   caller's published frame survives untouched.
//! - The callee frame lives exactly as long as its machine-stack
//!   reservation; the caller's frame is republished before any exit path.
//! - Ineligible call resolutions complete through the descriptor-classified
//!   generic in-place transitions; only receivers whose opcode semantics the
//!   interpreter dispatches through bespoke branches take an exact side exit.
//!
//! # See also
//! - [`super::transitions`] — descriptor-resolved entries used here.
//! - `crates/otter-vm/src/native_abi/frame.rs` — the published activation.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::JitCompileSnapshot;
use otter_vm::native_abi as abi;

use super::collections::{
    MethodSite, emit_alloc_method_guarded_call, emit_leaf_method_guarded_call,
};
use super::transitions::TransitionTable;
use super::values::emit_load_u64;
use crate::entry::{
    CTX_PLUS_FRAME_STACK_SIZE, DIRECT_CODE_OBJECT_ID_OFFSET, DIRECT_ENTRY_OFFSET,
    DIRECT_FRAME_IDS_OFFSET, DIRECT_FRAME_INDEX_OFFSET, DIRECT_FRAME_META_OFFSET,
    DIRECT_REGS_OFFSET, DIRECT_SELF_OFFSET, DIRECT_THIS_OFFSET, DIRECT_UPVALUES_OFFSET,
    ERROR_SLOT_OFFSET, FRAME_INDEX_OFFSET, JIT_CTX_STACK_SIZE, NATIVE_FRAME_ARGUMENT_BASE_OFFSET,
    NATIVE_FRAME_CODE_OBJECT_ID_OFFSET, NATIVE_FRAME_FEEDBACK_BASE_OFFSET,
    NATIVE_FRAME_NEW_TARGET_OFFSET, NATIVE_FRAME_OFFSET, NATIVE_FRAME_PC_OFFSET,
    NATIVE_FRAME_PREVIOUS_OFFSET, NATIVE_FRAME_REGISTER_BASE_OFFSET,
    NATIVE_FRAME_RETURN_REGISTER_OFFSET, NATIVE_FRAME_TAIL_OFFSET, NATIVE_FRAME_THIS_OFFSET,
    REG_STACK_BASE_OFFSET, REG_TOP_PTR_OFFSET, STATUS_BAILED, STATUS_RETURNED, THREAD_OFFSET,
    UPVALUES_PTR_OFFSET, VALUE_UNDEFINED,
};

/// Emit `dst = callee(args…)` (plain `Op::Call`).
///
/// The prepare transition resolves the callee against installed code and
/// stages the callee window/identity in the entry context (`0`), throws
/// (`1`), or reports an ineligible callee (`2`), which then completes
/// through the generic in-place call transition — the compiled caller keeps
/// running for every callable. Only a non-callable value takes the exact
/// side exit so the interpreter owns the thrown error.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_call(
    ops: &mut Assembler,
    table: &TransitionTable,
    dst: u16,
    callee: u16,
    argc: u16,
    packed_args: u64,
    bail: DynamicLabel,
    threw: DynamicLabel,
) {
    let done = ops.new_dynamic_label();
    let generic = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; mov x0, x20
        ; movz x1, callee as u32
        ; movz x2, argc as u32
    );
    emit_load_u64(ops, 3, packed_args);
    emit_load_u64(ops, 16, table.entry(abi::STUB_JIT_PREPARE_DIRECT_CALL));
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; cmp x0, #1
        ; b.eq =>threw
        ; cmp x0, #2
        ; b.eq =>generic
    );
    emit_direct_call_tail(ops, table, dst, threw, done);

    // Ineligible callee: complete the whole opcode through the generic
    // in-place call transition; only its non-callable report (`2`)
    // side-exits to normal dispatch.
    dynasm!(ops
        ; .arch aarch64
        ; =>generic
        ; mov x0, x20
        ; movz x1, dst as u32
        ; movz x2, callee as u32
        ; movz x3, argc as u32
    );
    emit_load_u64(ops, 4, packed_args);
    emit_load_u64(ops, 16, table.entry(abi::STUB_JIT_CALL_GENERIC));
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; cmp x0, #1
        ; b.eq =>threw
        ; cmp x0, #2
        ; b.eq =>bail
        ; =>done
    );
}

/// Emit `dst = new callee(args…)` (`Op::New`).
///
/// The construct opcode has no direct-call fast path: it completes through
/// the single generic in-place construct transition, which runs the
/// interpreter's own `Construct` synchronously and writes `dst`. Status `0`
/// continues the compiled caller, `1` throws, and `2` (a non-constructor
/// callee) takes the exact side exit so the interpreter owns the `TypeError`.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_construct(
    ops: &mut Assembler,
    table: &TransitionTable,
    dst: u16,
    callee: u16,
    argc: u16,
    packed_args: u64,
    bail: DynamicLabel,
    threw: DynamicLabel,
) {
    dynasm!(ops
        ; .arch aarch64
        ; mov x0, x20
        ; movz x1, dst as u32
        ; movz x2, callee as u32
        ; movz x3, argc as u32
    );
    emit_load_u64(ops, 4, packed_args);
    emit_load_u64(ops, 16, table.entry(abi::STUB_JIT_CONSTRUCT));
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; cmp x0, #1
        ; b.eq =>threw
        ; cmp x0, #2
        ; b.eq =>bail
    );
}

/// Emit `dst = recv.name(args…)` (`Op::CallMethodValue`).
///
/// Layered dispatch: the collection-method IC transition completes hot
/// collection methods in place (`0`), throws (`1`), or misses (`2`); a miss
/// falls through to the direct-method prepare; an ineligible resolution
/// (polymorphic, native, accessor, or cold method) then completes through
/// the generic in-place method transition, so the compiled caller keeps
/// running for every ordinary receiver, including missing/non-callable
/// resolutions after an observable getter or proxy trap. Only receivers the
/// interpreter dispatches through bespoke opcode branches (generators,
/// iterators, pending bind continuations) take the exact side exit before
/// resolution begins.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_method_call(
    ops: &mut Assembler,
    table: &TransitionTable,
    view: &JitCompileSnapshot,
    dst: u16,
    receiver: u16,
    name: u32,
    site: u64,
    argc: u16,
    packed_args: u64,
    byte_pc: u32,
    arg0: Option<u16>,
    arg1: Option<u16>,
    bail: DynamicLabel,
    threw: DynamicLabel,
) -> Result<(), crate::entry::Unsupported> {
    let done = ops.new_dynamic_label();
    let method_site = MethodSite {
        dst,
        receiver,
        argc,
        arg0,
        arg1,
    };
    // Guarded monomorphic collection fast paths precede the shared bridge;
    // every guard miss lands on the next layer.
    if let Some(leaf) = view.collection_leaf_methods.get(&byte_pc) {
        let after_leaf = ops.new_dynamic_label();
        if emit_leaf_method_guarded_call(ops, view, leaf, &method_site, after_leaf, done)? {
            dynasm!(ops ; .arch aarch64 ; =>after_leaf);
        }
    }
    if let Some(alloc) = view.collection_alloc_methods.get(&byte_pc) {
        let after_alloc = ops.new_dynamic_label();
        if emit_alloc_method_guarded_call(ops, view, alloc, &method_site, after_alloc, done)? {
            dynasm!(ops ; .arch aarch64 ; =>after_alloc);
        }
    }
    dynasm!(ops
        ; .arch aarch64
        ; mov x0, x20
        ; movz x1, dst as u32
        ; movz x2, receiver as u32
    );
    emit_load_u64(ops, 3, site);
    dynasm!(ops ; .arch aarch64 ; movz x4, argc as u32);
    emit_load_u64(ops, 5, packed_args);
    emit_load_u64(ops, 16, table.entry(abi::STUB_JIT_COLLECTION_METHOD_IC));
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; cmp x0, #1
        ; b.eq =>threw
        ; cbz x0, =>done
    );

    let generic = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; mov x0, x20
        ; movz x1, receiver as u32
    );
    emit_load_u64(ops, 2, u64::from(name));
    emit_load_u64(ops, 3, site);
    dynasm!(ops ; .arch aarch64 ; movz x4, argc as u32);
    emit_load_u64(ops, 5, packed_args);
    emit_load_u64(
        ops,
        16,
        table.entry(abi::STUB_JIT_PREPARE_DIRECT_METHOD_CALL),
    );
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; cmp x0, #1
        ; b.eq =>threw
        ; cmp x0, #2
        ; b.eq =>generic
    );
    emit_direct_call_tail(ops, table, dst, threw, done);

    // Ineligible direct resolution: complete the whole opcode through the
    // generic in-place method transition; only its exotic-receiver report
    // (`2`) side-exits to normal dispatch.
    dynasm!(ops
        ; .arch aarch64
        ; =>generic
        ; mov x0, x20
        ; movz x1, dst as u32
        ; movz x2, receiver as u32
    );
    emit_load_u64(ops, 3, u64::from(name));
    emit_load_u64(ops, 4, site);
    dynasm!(ops ; .arch aarch64 ; movz x5, argc as u32);
    emit_load_u64(ops, 6, packed_args);
    emit_load_u64(ops, 16, table.entry(abi::STUB_JIT_CALL_METHOD_GENERIC));
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; cmp x0, #1
        ; b.eq =>threw
        ; cmp x0, #2
        ; b.eq =>bail
        ; =>done
    );
    Ok(())
}

/// Shared direct-call dispatch tail used after a prepare transition returned
/// status 0 (callee staged in the entry context's `direct_*` fields).
///
/// Builds the callee `JitCtx` on the machine stack with the callee's own
/// published `NativeFrame` above it (prepared identity/meta words, the
/// caller's frame as `previous_frame`, the callee window as `register_base`,
/// and the callee code-object id, so the isolate registry resolves the
/// callee's safepoints while the caller frame keeps its exact PC). Branches
/// to the compiled entry and dispatches the returned / bailed / threw finish
/// helpers, landing at `done`.
fn emit_direct_call_tail(
    ops: &mut Assembler,
    table: &TransitionTable,
    dst: u16,
    threw: DynamicLabel,
    done: DynamicLabel,
) {
    let direct_returned = ops.new_dynamic_label();
    let direct_bailed = ops.new_dynamic_label();
    let direct_threw = ops.new_dynamic_label();
    let push_activation = table.entry(abi::STUB_JIT_PUSH_NATIVE_ACTIVATION);
    let pop_activation = table.entry(abi::STUB_JIT_POP_NATIVE_ACTIVATION);
    dynasm!(ops
        ; .arch aarch64
        ; sub sp, sp, CTX_PLUS_FRAME_STACK_SIZE
        ; ldr x9, [x20, DIRECT_REGS_OFFSET]
        ; str x9, [sp]
        ; ldr x9, [x20, DIRECT_SELF_OFFSET]
        ; str x9, [sp, #8]
        ; ldr x9, [x20, DIRECT_THIS_OFFSET]
        ; str x9, [sp, #16]
        ; ldr x9, [x20, THREAD_OFFSET]
        ; str x9, [sp, THREAD_OFFSET]
        ; ldr x10, [x20, NATIVE_FRAME_OFFSET]
        ; add x15, sp, JIT_CTX_STACK_SIZE
        ; ldr x9, [x20, DIRECT_FRAME_IDS_OFFSET]
        ; str x9, [x15]
        ; ldr x9, [x20, DIRECT_FRAME_META_OFFSET]
        ; str x9, [x15, #8]
        ; str x10, [x15, NATIVE_FRAME_PREVIOUS_OFFSET]
        ; ldr x9, [x20, DIRECT_REGS_OFFSET]
        ; str x9, [x15, NATIVE_FRAME_REGISTER_BASE_OFFSET]
        ; str xzr, [x15, NATIVE_FRAME_ARGUMENT_BASE_OFFSET]
        ; str xzr, [x15, NATIVE_FRAME_FEEDBACK_BASE_OFFSET]
        ; ldr x9, [x20, DIRECT_CODE_OBJECT_ID_OFFSET]
        ; str x9, [x15, NATIVE_FRAME_CODE_OBJECT_ID_OFFSET]
        ; ldr x9, [x20, DIRECT_THIS_OFFSET]
        ; str x9, [x15, NATIVE_FRAME_THIS_OFFSET]
    );
    emit_load_u64(ops, 9, VALUE_UNDEFINED);
    dynasm!(ops
        ; .arch aarch64
        ; str x9, [x15, NATIVE_FRAME_NEW_TARGET_OFFSET]
        ; movn x9, #0
        ; str x9, [x15, NATIVE_FRAME_RETURN_REGISTER_OFFSET]
        ; str xzr, [x15, NATIVE_FRAME_TAIL_OFFSET]
        ; str x15, [sp, NATIVE_FRAME_OFFSET]
        ; ldr x9, [x20, THREAD_OFFSET]
        ; str x15, [x9]
        ; ldr x9, [x20, DIRECT_FRAME_INDEX_OFFSET]
        ; str x9, [sp, FRAME_INDEX_OFFSET]
        ; ldr x9, [x20, ERROR_SLOT_OFFSET]
        ; str x9, [sp, ERROR_SLOT_OFFSET]
        ; ldr x9, [x20, DIRECT_UPVALUES_OFFSET]
        ; str x9, [sp, UPVALUES_PTR_OFFSET]
        ; ldr x9, [x20, REG_STACK_BASE_OFFSET]
        ; str x9, [sp, REG_STACK_BASE_OFFSET]
        ; ldr x9, [x20, REG_TOP_PTR_OFFSET]
        ; str x9, [sp, REG_TOP_PTR_OFFSET]
        ; mov x0, sp
    );
    emit_load_u64(ops, 16, push_activation);
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; cbnz x0, =>threw
        ; mov x0, sp
        ; ldr x16, [x20, DIRECT_ENTRY_OFFSET]
        ; blr x16
        // The callee frame dies with this reservation: republish the caller's
        // frame before any exit path runs.
        ; ldr x10, [x20, NATIVE_FRAME_OFFSET]
        ; ldr x9, [x20, THREAD_OFFSET]
        ; str x10, [x9]
        ; cmp x1, STATUS_RETURNED as u32
        ; b.eq =>direct_returned
        ; cmp x1, STATUS_BAILED as u32
        ; b.eq =>direct_bailed
        ; b =>direct_threw
        ; =>direct_returned
        ; str x0, [sp, DIRECT_ENTRY_OFFSET]
        ; ldr x9, [x20, DIRECT_FRAME_INDEX_OFFSET]
        ; str x9, [sp, DIRECT_FRAME_INDEX_OFFSET]
        ; mov x0, sp
    );
    emit_load_u64(ops, 16, pop_activation);
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; ldr x2, [sp, DIRECT_FRAME_INDEX_OFFSET]
        ; ldr x3, [sp, DIRECT_ENTRY_OFFSET]
        ; add sp, sp, CTX_PLUS_FRAME_STACK_SIZE
        ; mov x0, x20
        ; movz x1, dst as u32
    );
    emit_load_u64(
        ops,
        16,
        table.entry(abi::STUB_JIT_FINISH_DIRECT_CALL_RETURNED),
    );
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; cbnz x0, =>threw
        ; b =>done
        ; =>direct_bailed
        // The callee stamped its exact bail PC into its own published frame;
        // park it in the spent entry slot across the activation pop.
        ; add x9, sp, JIT_CTX_STACK_SIZE
        ; ldr w9, [x9, NATIVE_FRAME_PC_OFFSET]
        ; str w9, [sp, DIRECT_ENTRY_OFFSET]
        ; ldr x9, [x20, DIRECT_FRAME_INDEX_OFFSET]
        ; str x9, [sp, DIRECT_FRAME_INDEX_OFFSET]
        ; mov x0, sp
    );
    emit_load_u64(ops, 16, pop_activation);
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; ldr x2, [sp, DIRECT_FRAME_INDEX_OFFSET]
        ; ldr w3, [sp, DIRECT_ENTRY_OFFSET]
        ; add sp, sp, CTX_PLUS_FRAME_STACK_SIZE
        ; mov x0, x20
        ; movz x1, dst as u32
    );
    emit_load_u64(
        ops,
        16,
        table.entry(abi::STUB_JIT_FINISH_DIRECT_CALL_BAILED),
    );
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; cbnz x0, =>threw
        ; b =>done
        ; =>direct_threw
        ; ldr x9, [x20, DIRECT_FRAME_INDEX_OFFSET]
        ; str x9, [sp, DIRECT_FRAME_INDEX_OFFSET]
        ; mov x0, sp
    );
    emit_load_u64(ops, 16, pop_activation);
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; ldr x1, [sp, DIRECT_FRAME_INDEX_OFFSET]
        ; add sp, sp, CTX_PLUS_FRAME_STACK_SIZE
        ; mov x0, x20
    );
    emit_load_u64(ops, 16, table.entry(abi::STUB_JIT_ABORT_DIRECT_CALL));
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; cbnz x0, =>threw
        ; b =>threw
    );
}
