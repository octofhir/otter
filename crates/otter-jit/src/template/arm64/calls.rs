//! AArch64 call emitters for the template compiler.
//!
//! # Contents
//! - Plain-call, method-call, and construct lowering through prepare and
//!   generic transitions.
//! - Guarded collection-method dispatch before ordinary method resolution.
//!
//! # Invariants
//! - The caller's canonical PC is stamped before the prepare transition; a
//!   bailed callee reifies at its exact PC through the finish helpers and the
//!   caller's published frame survives untouched.
//! - A callee throw caught by the compiled caller publishes the selected
//!   catch/finally PC and exits through the shared bailout epilogue.
//! - Prepared callees enter through the common owned AArch64 call trampoline,
//!   which owns frame publication and cleanup for every native tier.
//! - Ineligible call resolutions complete through the descriptor-classified
//!   generic in-place transitions; only receivers whose opcode semantics the
//!   interpreter dispatches through bespoke branches take an exact side exit.
//!
//! # See also
//! - [`crate::arm64::calls`] — shared compiled-to-compiled call emission.
//! - [`super::transitions`] — descriptor-resolved entries used here.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::JitCompileSnapshot;
use otter_vm::native_abi as abi;

use super::collections::{
    MethodSite, emit_alloc_method_guarded_call, emit_leaf_method_guarded_call,
};
use super::transitions::TransitionTable;
use super::values::{emit_load_runtime_stub, emit_load_symbol_u64, emit_load_u64};
use crate::arm64::{CallTrampoline, emit_prepared_call};
use crate::artifact::relocation::{
    RelocationCapture, RelocationTarget, TemplateOperandArena, TemplateOperandRole,
};
use crate::template::TemplateTail;

fn emit_packed_args(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    register: u8,
    packed_args: u64,
    tail: Option<TemplateTail>,
    role: TemplateOperandRole,
) {
    if let Some(tail) = tail {
        emit_load_symbol_u64(
            ops,
            relocations,
            register,
            packed_args,
            RelocationTarget::TemplateOperandSlice {
                arena: TemplateOperandArena::Registers,
                role,
                start: u32::try_from(tail.start).expect("template operand offset fits u32"),
                len: u32::try_from(tail.len).expect("template operand length fits u32"),
            },
        );
    } else {
        emit_load_u64(ops, register, packed_args);
    }
}

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
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    call_trampoline: &CallTrampoline,
    dst: u16,
    callee: u16,
    argc: u16,
    packed_args: u64,
    packed_args_tail: Option<TemplateTail>,
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
    emit_packed_args(
        ops,
        relocations,
        3,
        packed_args,
        packed_args_tail,
        TemplateOperandRole::CallArguments,
    );
    emit_load_runtime_stub(
        ops,
        relocations,
        16,
        table.entry(abi::STUB_JIT_PREPARE_DIRECT_CALL),
        abi::STUB_JIT_PREPARE_DIRECT_CALL,
    );
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; cmp x0, #1
        ; b.eq =>threw
        ; cmp x0, #2
        ; b.eq =>generic
    );
    emit_prepared_call(ops, relocations, call_trampoline, dst, bail, threw, done);

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
    emit_packed_args(
        ops,
        relocations,
        4,
        packed_args,
        packed_args_tail,
        TemplateOperandRole::CallArguments,
    );
    emit_load_runtime_stub(
        ops,
        relocations,
        16,
        table.entry(abi::STUB_JIT_CALL_GENERIC),
        abi::STUB_JIT_CALL_GENERIC,
    );
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
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    dst: u16,
    callee: u16,
    argc: u16,
    packed_args: u64,
    packed_args_tail: Option<TemplateTail>,
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
    emit_packed_args(
        ops,
        relocations,
        4,
        packed_args,
        packed_args_tail,
        TemplateOperandRole::ConstructArguments,
    );
    emit_load_runtime_stub(
        ops,
        relocations,
        16,
        table.entry(abi::STUB_JIT_CONSTRUCT),
        abi::STUB_JIT_CONSTRUCT,
    );
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
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    call_trampoline: &CallTrampoline,
    view: &JitCompileSnapshot,
    dst: u16,
    receiver: u16,
    name: u32,
    site: u64,
    argc: u16,
    packed_args: u64,
    packed_args_tail: Option<TemplateTail>,
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
        if emit_leaf_method_guarded_call(
            ops,
            relocations,
            view,
            leaf,
            byte_pc,
            &method_site,
            after_leaf,
            done,
        )? {
            dynasm!(ops ; .arch aarch64 ; =>after_leaf);
        }
    }
    if let Some(alloc) = view.collection_alloc_methods.get(&byte_pc) {
        let after_alloc = ops.new_dynamic_label();
        if emit_alloc_method_guarded_call(
            ops,
            relocations,
            view,
            alloc,
            byte_pc,
            &method_site,
            after_alloc,
            done,
        )? {
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
    emit_packed_args(
        ops,
        relocations,
        5,
        packed_args,
        packed_args_tail,
        TemplateOperandRole::MethodArguments,
    );
    emit_load_runtime_stub(
        ops,
        relocations,
        16,
        table.entry(abi::STUB_JIT_COLLECTION_METHOD_IC),
        abi::STUB_JIT_COLLECTION_METHOD_IC,
    );
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
    emit_packed_args(
        ops,
        relocations,
        5,
        packed_args,
        packed_args_tail,
        TemplateOperandRole::MethodArguments,
    );
    emit_load_runtime_stub(
        ops,
        relocations,
        16,
        table.entry(abi::STUB_JIT_PREPARE_DIRECT_METHOD_CALL),
        abi::STUB_JIT_PREPARE_DIRECT_METHOD_CALL,
    );
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; cmp x0, #1
        ; b.eq =>threw
        ; cmp x0, #2
        ; b.eq =>generic
    );
    emit_prepared_call(ops, relocations, call_trampoline, dst, bail, threw, done);

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
    emit_packed_args(
        ops,
        relocations,
        6,
        packed_args,
        packed_args_tail,
        TemplateOperandRole::MethodArguments,
    );
    emit_load_runtime_stub(
        ops,
        relocations,
        16,
        table.entry(abi::STUB_JIT_CALL_METHOD_GENERIC),
        abi::STUB_JIT_CALL_METHOD_GENERIC,
    );
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
