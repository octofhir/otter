//! Shared completion and cleanup for generated native calls.
//!
//! # Contents
//! - Native result classification and cold callee deoptimization.
//! - Constructor result selection and caller publication restoration.
//! - Exact release of fixed or dynamic linkage on every exit.
//!
//! # Invariants
//! - Callee deoptimization resumes an already-started call; no effect is replayed.
//! - Caller publication is restored before the callee frame is discarded.
//! - Pending-error preparation failures and pure exception values retain their
//!   distinct caller continuations. Every private-frame rejection releases SP.
//!
//! # See also
//! - [`super::emit_direct_call_with_access`] — target selection and frame setup.
//! - [`super::layout`] — authoritative control and window offsets.

use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) fn emit<Store, Restore, Record>(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    form: DirectCallForm,
    dst: u16,
    caller_function_id: u32,
    logical_pc: u32,
    layout: StackLayout,
    deopt_entry: u64,
    derived_construct_result_entry: u64,
    mut record: Record,
    bail: DynamicLabel,
    finish_error: DynamicLabel,
    throw_value: DynamicLabel,
    fatal: DynamicLabel,
    done: DynamicLabel,
    uncommitted_rejected: DynamicLabel,
    entry_rejected: DynamicLabel,
    caller_bail: DynamicLabel,
    construct_prepare_error: DynamicLabel,
    construct_prepare_throw: DynamicLabel,
    construct_prepare_fatal: DynamicLabel,
    context_register: u8,
    mut store: Store,
    mut restore_roots: Restore,
) -> Result<(), Unsupported>
where
    Store: FnMut(&mut Assembler, u16, u8, u32) -> Result<(), Unsupported>,
    Restore: FnMut(&mut Assembler) -> Result<(), Unsupported>,
    Record: FnMut(&'static str, usize, usize),
{
    let callee_returned = ops.new_dynamic_label();
    let callee_bailed = ops.new_dynamic_label();
    let callee_threw = ops.new_dynamic_label();
    let result_ready = ops.new_dynamic_label();
    let cleanup = ops.new_dynamic_label();
    let returned = ops.new_dynamic_label();
    let cleanup_abrupt = ops.new_dynamic_label();
    let return_start = ops.offset().0;
    let invalid_callee_result = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; cmp x1, abi::NativeResultStatus::SideExit as u32
        ; b.eq =>callee_bailed
        ; cmp x1, abi::NativeResultStatus::Success as u32
        ; b.eq =>callee_returned
        ; cmp x1, abi::NativeResultStatus::Throw as u32
        ; b.eq =>callee_threw
        ; cmp x1, abi::NativeResultStatus::Fatal as u32
        ; b.eq =>result_ready
        ; b =>invalid_callee_result
        ; =>callee_threw
    );
    emit_increment_feedback_u64(ops, CODE_ENTRY_GENERATED_THROWS_OFFSET);
    dynasm!(ops ; .arch aarch64 ; b =>result_ready ; =>invalid_callee_result);
    emit_fatal_pair(ops);
    dynasm!(ops ; .arch aarch64 ; b =>result_ready ; =>callee_returned);
    if form.prepared_receiver().is_some() {
        let result_start = ops.offset().0;
        let object = ops.new_dynamic_label();
        let primitive = ops.new_dynamic_label();
        let ready = ops.new_dynamic_label();
        emit_object_type_branch(ops, relocations, view, 0, [9, 10, 11], object, primitive);
        dynasm!(ops
            ; .arch aarch64
            ; =>primitive
            ; ldr x0, [sp, NATIVE_FRAME_THIS_OFFSET]
            ; b =>ready
            ; =>object
            ; =>ready
            ; mov x1, xzr
        );
        record("directConstructResultFast", result_start, ops.offset().0);
    }
    if form.is_derived() {
        let result_start = ops.offset().0;
        let object = ops.new_dynamic_label();
        let primitive = ops.new_dynamic_label();
        let cold = ops.new_dynamic_label();
        let ready = ops.new_dynamic_label();
        let invalid_construct_result = ops.new_dynamic_label();
        emit_object_type_branch(ops, relocations, view, 0, [9, 10, 11], object, primitive);
        dynasm!(ops ; .arch aarch64 ; =>primitive);
        emit_load_u64(ops, 9, VALUE_UNDEFINED);
        dynasm!(ops
            ; .arch aarch64
            ; cmp x0, x9
            ; b.ne =>cold
            ; ldr x2, [sp, NATIVE_FRAME_THIS_OFFSET]
        );
        emit_load_u64(ops, 9, VALUE_HOLE);
        dynasm!(ops
            ; .arch aarch64
            ; cmp x2, x9
            ; b.eq =>cold
            ; mov x0, x2
            ; b =>ready
            ; =>object
            ; b =>ready
        );
        record("directConstructResultFast", result_start, ops.offset().0);
        dynasm!(ops ; .arch aarch64 ; =>cold);
        let cold_start = ops.offset().0;
        dynasm!(ops
            ; .arch aarch64
            ; mov x1, x0
            ; ldr x2, [sp, NATIVE_FRAME_THIS_OFFSET]
            ; mov x0, X(context_register)
        );
        emit_runtime_stub(
            ops,
            relocations,
            16,
            derived_construct_result_entry,
            abi::STUB_JIT_DERIVED_CONSTRUCT_RESULT,
        );
        dynasm!(ops
            ; .arch aarch64
            ; blr x16
            ; cmp x1, abi::NativeResultStatus::Success as u32
            ; b.eq =>ready
            ; cmp x1, abi::NativeResultStatus::Throw as u32
            ; b.eq =>callee_threw
            ; cmp x1, abi::NativeResultStatus::Fatal as u32
            ; b.eq =>result_ready
            ; b =>invalid_construct_result
        );
        dynasm!(ops ; .arch aarch64 ; =>invalid_construct_result);
        emit_fatal_pair(ops);
        dynasm!(ops ; .arch aarch64 ; b =>result_ready);
        record("directConstructResultThrow", cold_start, ops.offset().0);
        dynasm!(ops ; .arch aarch64 ; =>ready);
    }
    dynasm!(ops
        ; .arch aarch64
        ; b =>result_ready
        ; =>callee_bailed
        ; mov x7, x0
        ; ldr w14, [sp, NATIVE_FRAME_PC_OFFSET]
        ; cmp w7, w14
        ; b.eq >callee_bail_pc_valid
    );
    emit_fatal_pair(ops);
    dynasm!(ops ; .arch aarch64 ; b =>result_ready ; callee_bail_pc_valid:);
    emit_increment_feedback_u64(ops, CODE_ENTRY_GENERATED_DEOPTS_OFFSET);
    let invalid_deopt_result = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; mov x0, X(context_register)
        ; mov x1, sp
    );
    emit_load_u64(ops, 2, u64::from(caller_function_id));
    emit_load_u64(ops, 3, u64::from(logical_pc));
    dynasm!(ops
        ; .arch aarch64
        ; ldr x4, [x25, CODE_ENTRY_CODE_OBJECT_ID_OFFSET]
        ; ldr x5, [sp, layout.caller_code_object_id]
    );
    emit_load_u64(
        ops,
        6,
        match form {
            DirectCallForm::Plain { .. } => 0,
            DirectCallForm::CallWithThis { .. } => 0,
            DirectCallForm::Method { .. } => 1,
            DirectCallForm::Construct { .. } => 2,
            DirectCallForm::DerivedConstruct { .. } => 3,
            DirectCallForm::SuperConstruct { .. } => 4,
            DirectCallForm::DerivedSuperConstruct { .. } => 5,
        },
    );
    emit_runtime_stub(
        ops,
        relocations,
        16,
        deopt_entry,
        abi::STUB_JIT_DEOPT_STACK_CALL,
    );
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; cmp x1, abi::NativeResultStatus::Success as u32
        ; b.eq =>result_ready
        ; cmp x1, abi::NativeResultStatus::Throw as u32
        ; b.eq =>callee_threw
        ; cmp x1, abi::NativeResultStatus::Fatal as u32
        ; b.eq =>result_ready
        ; b =>invalid_deopt_result
    );
    dynasm!(ops ; .arch aarch64 ; =>invalid_deopt_result);
    emit_fatal_pair(ops);
    dynasm!(ops ; .arch aarch64 ; b =>result_ready);
    dynasm!(ops ; .arch aarch64 ; =>result_ready ; b =>cleanup);
    record("directCallReturn", return_start, ops.offset().0);

    let cleanup_start = ops.offset().0;
    dynasm!(ops
        ; .arch aarch64
        ; =>cleanup
        // Restore caller publication before retiring the callee generation.
        ; ldr x13, [sp, layout.caller_frame]
        ; ldr x15, [sp, layout.caller_code_object_id]
        ; str x13, [X(context_register), NATIVE_FRAME_OFFSET]
        ; ldr x14, [X(context_register), THREAD_OFFSET]
        ; stp x13, x15, [x14, VM_THREAD_CURRENT_FRAME_OFFSET as i32]
        ; ldr x9, [X(context_register), ACTIVATION_TOP_PTR_OFFSET]
        ; ldr x10, [x9]
        ; sub x10, x10, #1
        ; str x10, [x9]
        ; ldr x11, [X(context_register), ACTIVATION_BASE_OFFSET]
        ; add x12, x11, x10, lsl #3
        ; str xzr, [x12]
    );
    dynasm!(ops
        ; .arch aarch64
        ; ldr x25, [sp, layout.saved_x25]
    );
    emit_release_linkage(ops, &layout);
    dynasm!(ops
        ; .arch aarch64
        ; cmp x1, abi::NativeResultStatus::Success as u32
        ; b.eq =>returned
        ; b =>cleanup_abrupt
    );
    dynasm!(ops ; .arch aarch64 ; =>returned);
    restore_roots(ops)?;
    store(ops, dst, 0, 0)?;
    dynasm!(ops ; .arch aarch64 ; b =>done ; =>cleanup_abrupt);
    let cleanup_throw = ops.new_dynamic_label();
    let cleanup_fatal = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; cmp x1, abi::NativeResultStatus::Throw as u32
        ; b.eq =>cleanup_throw
        ; cmp x1, abi::NativeResultStatus::Fatal as u32
        ; b.eq =>cleanup_fatal
    );
    emit_fatal_pair(ops);
    dynasm!(ops ; .arch aarch64 ; =>cleanup_fatal);
    restore_roots(ops)?;
    dynasm!(ops ; .arch aarch64 ; b =>fatal ; =>cleanup_throw ; mov x17, x0);
    restore_roots(ops)?;
    dynasm!(ops ; .arch aarch64 ; mov x0, x17 ; b =>throw_value);
    record("directCallCleanup", cleanup_start, ops.offset().0);

    // Entry rejection owns no JS effects or published callee frame.
    let entry_reject_start = ops.offset().0;
    dynasm!(ops
        ; .arch aarch64
        ; =>entry_rejected
        ; ldr x25, [sp, layout.saved_x25]
    );
    emit_release_linkage(ops, &layout);
    dynasm!(ops
        ; .arch aarch64
        ; b =>caller_bail
        ; =>uncommitted_rejected
        ; ldr x25, [sp, layout.saved_x25]
    );
    emit_release_linkage(ops, &layout);
    dynasm!(ops
        ; .arch aarch64
        ; b =>caller_bail
    );
    record("directCallEntryReject", entry_reject_start, ops.offset().0);

    dynasm!(ops ; .arch aarch64 ; =>caller_bail);
    restore_roots(ops)?;
    dynasm!(ops ; .arch aarch64 ; b =>bail);

    dynasm!(ops
        ; .arch aarch64
        ; =>construct_prepare_error
        ; ldr x25, [sp, layout.saved_x25]
    );
    emit_release_linkage(ops, &layout);
    restore_roots(ops)?;
    dynasm!(ops ; .arch aarch64 ; b =>finish_error);

    dynasm!(ops
        ; .arch aarch64
        ; =>construct_prepare_throw
        ; mov x17, x0
        ; ldr x25, [sp, layout.saved_x25]
    );
    emit_release_linkage(ops, &layout);
    restore_roots(ops)?;
    dynasm!(ops ; .arch aarch64 ; mov x0, x17 ; b =>throw_value);

    dynasm!(ops
        ; .arch aarch64
        ; =>construct_prepare_fatal
        ; ldr x25, [sp, layout.saved_x25]
    );
    emit_release_linkage(ops, &layout);
    restore_roots(ops)?;
    dynasm!(ops ; .arch aarch64 ; b =>fatal);

    Ok(())
}
