//! Runtime-selected forwarding through shared native call completion.
//!
//! # Contents
//! - Leaf admission into the existing call-plan metadata in native scratch.
//! - Bounded dynamic frame construction, live argument copy and native entry.
//!
//! # Invariants
//! - Metadata scratch owns no GC references. Temporary callable/receiver copies
//!   are consumed before any allocation; the caller retains authoritative roots.
//! - The fixed control prefix is identical to bounded direct linkage. Its size
//!   slot releases both the dynamic callee frame and metadata scratch.
//! - Every root is initialized before capture allocation and frame publication.
//! - Shared completion owns return, exception, deopt and rejection cleanup.
//!
//! # See also
//! - [`super::completion`] — the single native result lifecycle.
//! - `otter_vm::runtime_activation` — target admission and live actual semantics.

use super::*;
use otter_vm::jit::JitDirectCallPlan;

const PLAN_BYTES: u32 = (std::mem::size_of::<JitDirectCallPlan>() as u32 + 15) & !15;
const SCRATCH_BYTES: u32 = PLAN_BYTES + 16;
const PLAN_ENTRY: u32 = std::mem::offset_of!(JitDirectCallPlan, entry_cell) as u32;
const PLAN_THIS: u32 = std::mem::offset_of!(JitDirectCallPlan, this_mode) as u32;
const PLAN_PARAMS: u32 = std::mem::offset_of!(JitDirectCallPlan, param_count) as u32;
const PLAN_REGISTERS: u32 = std::mem::offset_of!(JitDirectCallPlan, register_count) as u32;
const PLAN_OWN: u32 = std::mem::offset_of!(JitDirectCallPlan, own_upvalue_count) as u32;
const PLAN_INHERITED: u32 = std::mem::offset_of!(JitDirectCallPlan, inherited_upvalue_count) as u32;
const PLAN_ACTUALS: u32 = std::mem::offset_of!(JitDirectCallPlan, needs_incoming_arguments) as u32;

#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_runtime_forward<Load, Store, Restore, LoadBinding>(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    table: &crate::entry::TransitionTable,
    [dst, method, callee, receiver]: [u16; 4],
    logical_pc: u32,
    byte_pc: u32,
    mut code_map: Option<&mut CodeMapCapture>,
    bail: DynamicLabel,
    finish_error: DynamicLabel,
    throw_value: DynamicLabel,
    fatal: DynamicLabel,
    done: DynamicLabel,
    context_register: u8,
    mut load: Load,
    store: Store,
    restore_roots: Restore,
    load_binding: LoadBinding,
) -> Result<(), Unsupported>
where
    Load: FnMut(&mut Assembler, u16, u8, u32) -> Result<(), Unsupported>,
    Store: FnMut(&mut Assembler, u16, u8, u32) -> Result<(), Unsupported>,
    Restore: FnMut(&mut Assembler) -> Result<(), Unsupported>,
    LoadBinding: FnMut(&mut Assembler, u16, u8) -> Result<(), Unsupported>,
{
    let layout = StackLayout::dynamic_prefix();
    let size_slot = layout.allocation_size.expect("dynamic prefix");
    let caller_bail = ops.new_dynamic_label();
    let scratch_miss = ops.new_dynamic_label();
    let uncommitted_rejected = ops.new_dynamic_label();
    let entry_rejected = ops.new_dynamic_label();
    let prepare_error = ops.new_dynamic_label();
    let prepare_throw = ops.new_dynamic_label();
    let prepare_fatal = ops.new_dynamic_label();
    let start = ops.offset().0;
    dynasm!(ops
        ; .arch aarch64
        ; ldr x9, [X(context_register), ACTIVATION_TOP_PTR_OFFSET]
        ; ldr x10, [x9]
        ; ldr x11, [X(context_register), ACTIVATION_LIMIT_OFFSET]
        ; cmp x10, x11
        ; b.hs =>caller_bail
    );
    load(ops, method, 1, 0)?;
    load(ops, callee, 2, 0)?;
    load(ops, receiver, 12, 0)?;
    dynasm!(ops
        ; .arch aarch64
        ; sub sp, sp, SCRATCH_BYTES
        ; str x2, [sp, PLAN_BYTES]
        ; str x12, [sp, PLAN_BYTES + 8]
        ; mov x3, sp
        ; mov x0, X(context_register)
    );
    emit_runtime_stub(
        ops,
        relocations,
        16,
        table.entry(abi::STUB_JIT_FORWARD_CALL_PLAN),
        abi::STUB_JIT_FORWARD_CALL_PLAN,
    );
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; tbnz x0, #63, =>scratch_miss
        ; mov x8, x0
        ; mov x17, sp
        ; ldrh w9, [x17, PLAN_OWN]
        ; ldrh w10, [x17, PLAN_INHERITED]
        ; add x9, x9, x10
        ; lsl x9, x9, #2
        ; add x9, x9, layout.upvalue_base + 7
        ; and x9, x9, 0xffff_ffff_ffff_fff8u64
        ; ldrh w10, [x17, PLAN_REGISTERS]
        ; add x16, x9, x10, lsl #3
        ; ldrb w11, [x17, PLAN_ACTUALS]
        ; cmp w11, #0
        ; csel x8, x8, xzr, ne
        ; add x16, x16, x8, lsl #3
        ; add x16, x16, #15
        ; and x16, x16, 0xffff_ffff_ffff_fff0u64
        ; add x15, x16, SCRATCH_BYTES
        ; cmp x15, MAX_DIRECT_CALL_FRAME_BYTES
        ; b.hi =>scratch_miss
        ; sub sp, sp, x16
        ; str w15, [sp, size_slot]
        ; str x25, [sp, layout.saved_x25]
        // Metadata pointer is dead before this slot becomes caller code id.
        ; str x17, [sp, layout.caller_code_object_id]
        ; str w8, [sp, abi::NATIVE_FRAME_ARGUMENT_COUNT_OFFSET]
        ; add x9, sp, x9
        ; str x9, [sp, NATIVE_FRAME_REGISTER_BASE_OFFSET]
        ; ldr x1, [x17, PLAN_ENTRY]
        ; add x1, x1, FUNCTION_ENTRY_GENERATION_CELL_OFFSET
        ; ldar x25, [x1]
        ; cbz x25, =>uncommitted_rejected
        ; str x25, [sp, layout.target_cell]
        ; ldr w15, [x25, CODE_ENTRY_GENERATED_STACK_FRAME_BYTES_OFFSET]
        ; cbz w15, =>uncommitted_rejected
        ; subs x12, sp, x15
        ; b.lo =>uncommitted_rejected
        ; ldr x11, [X(context_register), NATIVE_STACK_LIMIT_OFFSET]
        ; cmp x12, x11
        ; b.lo =>uncommitted_rejected
        ; ldr x16, [x25]
        ; cbz x16, =>entry_rejected
        ; str x16, [sp, layout.entry_addr]
        ; ldr x13, [x25, CODE_ENTRY_NATIVE_FRAME_HEADER_OFFSET]
        ; ldr w14, [x25, CODE_ENTRY_NATIVE_FRAME_HEADER_OFFSET + 8]
        ; str x13, [sp]
    );
    let header_ready = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64 ; ldrb w11, [x17, PLAN_ACTUALS] ; cbz w11, =>header_ready);
    // Actual windows require the complete register span in the traced header.
    dynasm!(ops
        ; .arch aarch64
        ; ldr w13, [x25, CODE_ENTRY_FLAGS_OFFSET]
        ; tbnz w13, #PARAMETER_PREFIX_FLAG_BIT, =>uncommitted_rejected
        ; orr w14, w14, INCOMING_ARGUMENTS_HEADER_WORD
        ; =>header_ready
        ; str w14, [sp, 8]
        ; ldr x9, [x17, PLAN_BYTES]
        ; ldr x12, [x17, PLAN_BYTES + 8]
        ; str x9, [sp, NATIVE_FRAME_SELF_OFFSET]
    );
    let direct_function = ops.new_dynamic_label();
    let closure_ready = ops.new_dynamic_label();
    emit_cell_test(ops, 9, 10, CellTest::IsNotCell, direct_function);
    dynasm!(ops
        ; .arch aarch64
        ; ldr x10, [x9, view.closure_call_layout.upvalue_base_byte]
        ; ldr w11, [x9, view.closure_call_layout.upvalue_count_byte]
        ; ldr w15, [x9, view.closure_call_layout.eval_env_byte]
        ; ldr w13, [x9, view.closure_call_layout.flags_byte]
    );
    let no_bound_this = ops.new_dynamic_label();
    emit_load_u64(ops, 14, u64::from(view.closure_call_layout.bound_this_flag));
    dynasm!(ops
        ; .arch aarch64
        ; tst w13, w14
        ; b.eq =>no_bound_this
        ; ldr x12, [x9, view.closure_call_layout.bound_this_byte]
        ; =>no_bound_this
        ; b =>closure_ready
        ; =>direct_function
        ; mov x10, xzr
        ; mov w11, wzr
        ; mov w15, wzr
        ; =>closure_ready
        ; str x10, [sp, NATIVE_FRAME_UPVALUE_BASE_OFFSET]
        ; str w11, [sp, NATIVE_FRAME_UPVALUE_COUNT_OFFSET]
        ; str w15, [sp, abi::NATIVE_FRAME_EVAL_ENV_OFFSET]
    );
    let this_ready = ops.new_dynamic_label();
    let global_this = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; ldrb w13, [x17, PLAN_THIS]
        ; cmp w13, JitDirectCallThisMode::StrictOrLexical as u32
        ; b.eq =>this_ready
    );
    emit_load_u64(ops, 14, VALUE_UNDEFINED);
    dynasm!(ops ; .arch aarch64 ; cmp x12, x14 ; b.eq =>global_this);
    emit_load_u64(ops, 14, VALUE_NULL);
    dynasm!(ops ; .arch aarch64 ; cmp x12, x14 ; b.eq =>global_this);
    emit_object_type_branch(
        ops,
        relocations,
        view,
        12,
        [14, 16, 17],
        this_ready,
        uncommitted_rejected,
    );
    dynasm!(ops ; .arch aarch64 ; =>global_this);
    emit_load_sloppy_global_this(ops, relocations, view, context_register);
    dynasm!(ops ; .arch aarch64 ; =>this_ready ; str x12, [sp, NATIVE_FRAME_THIS_OFFSET]);
    emit_load_u64(ops, 13, VALUE_UNDEFINED);
    dynasm!(ops
        ; .arch aarch64
        ; str x13, [sp, NATIVE_FRAME_NEW_TARGET_OFFSET]
        ; ldr x17, [sp, layout.caller_code_object_id]
        ; ldrh w13, [x17, PLAN_REGISTERS]
        ; ldr w14, [sp, abi::NATIVE_FRAME_ARGUMENT_COUNT_OFFSET]
        ; add w13, w13, w14
        ; ldr x14, [sp, NATIVE_FRAME_REGISTER_BASE_OFFSET]
    );
    let initialized = ops.new_dynamic_label();
    let initialize = ops.new_dynamic_label();
    emit_load_u64(ops, 15, VALUE_UNDEFINED);
    dynasm!(ops
        ; .arch aarch64
        ; cbz w13, =>initialized
        ; =>initialize
        ; str x15, [x14], #8
        ; subs w13, w13, #1
        ; b.ne =>initialize
        ; =>initialized
        ; ldrh w2, [x17, PLAN_OWN]
    );
    let captures_ready = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; cbz w2, =>captures_ready
        ; ldrh w3, [x17, PLAN_INHERITED]
        ; add x13, sp, layout.upvalue_base
        ; str x13, [sp, NATIVE_FRAME_UPVALUE_BASE_OFFSET]
        ; str wzr, [sp, NATIVE_FRAME_UPVALUE_COUNT_OFFSET]
        ; mov x0, X(context_register)
        ; mov x1, sp
    );
    emit_runtime_stub(
        ops,
        relocations,
        16,
        table.entry(abi::STUB_JIT_INITIALIZE_UPVALUES),
        abi::STUB_JIT_INITIALIZE_UPVALUES,
    );
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; cmp x1, abi::NativeResultStatus::Success as u32
        ; b.eq =>captures_ready
        ; cmp x1, abi::NativeResultStatus::SideExit as u32
        ; b.eq =>uncommitted_rejected
        ; cmp x1, abi::NativeResultStatus::Throw as u32
        ; b.eq =>prepare_error
        ; b =>prepare_fatal
        ; =>captures_ready
        ; ldr x17, [sp, layout.caller_code_object_id]
        ; ldrh w2, [x17, PLAN_PARAMS]
        ; mov x0, X(context_register)
        ; mov x1, sp
    );
    emit_runtime_stub(
        ops,
        relocations,
        16,
        table.entry(abi::STUB_JIT_COPY_FORWARDED_ARGUMENTS),
        abi::STUB_JIT_COPY_FORWARDED_ARGUMENTS,
    );
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; tbnz x0, #63, =>uncommitted_rejected
        ; ldr x17, [sp, layout.caller_code_object_id]
        ; ldrh w2, [x17, PLAN_PARAMS]
        ; ldrh w3, [x17, PLAN_REGISTERS]
    );
    forward_bindings::emit(ops, view, layout, load_binding)?;
    dynasm!(ops
        ; .arch aarch64
        ; ldr x13, [X(context_register), NATIVE_FRAME_OFFSET]
        ; ldr x14, [X(context_register), THREAD_OFFSET]
        ; ldr x15, [x14, VM_THREAD_CODE_OBJECT_ID_OFFSET]
        ; str x13, [sp, layout.caller_frame]
        ; str x15, [sp, layout.caller_code_object_id]
        ; ldr x9, [X(context_register), ACTIVATION_TOP_PTR_OFFSET]
        ; ldr x10, [x9]
        ; ldr x11, [X(context_register), ACTIVATION_BASE_OFFSET]
        ; add x12, x11, x10, lsl #3
        ; mov x15, sp
        ; str x15, [x12]
        ; add x10, x10, #1
        ; str x10, [x9]
        ; str x15, [X(context_register), NATIVE_FRAME_OFFSET]
        ; ldr x13, [x25, CODE_ENTRY_CODE_OBJECT_ID_OFFSET]
        ; stp x15, x13, [x14, VM_THREAD_CURRENT_FRAME_OFFSET as i32]
        ; ldr x16, [sp, layout.entry_addr]
    );
    let entry_start = ops.offset().0;
    emit_increment_feedback_u64(ops, CODE_ENTRY_GENERATED_ENTRIES_OFFSET);
    let cold_callee = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; cmp x14, otter_vm::tier_policy::OPTIMIZING_HOTNESS_THRESHOLD
        ; b.lo =>cold_callee
        ; ldr x14, [X(context_register), THREAD_OFFSET]
        ; ldr x14, [x14, VM_THREAD_CODE_REGISTRY_OFFSET]
        ; ldr w15, [sp, std::mem::offset_of!(abi::NativeFrame, header) as u32 + std::mem::offset_of!(abi::VmFrameHeader, function_id) as u32]
        ; add x15, x15, #1
        ; str x15, [x14, CODE_REGISTRY_VIEW_HOT_FUNCTION_OFFSET]
        ; =>cold_callee
        ; str xzr, [X(context_register), GENERATED_FEEDBACK_CLEAN_OFFSET]
        ; mov x0, X(context_register)
        ; blr x16
    );
    let mut record = |kind: &'static str, start, end| {
        if let Some(map) = code_map.as_deref_mut() {
            map.record(CodeRegion::call_structural(
                kind,
                start,
                end,
                view.code_block.id,
                logical_pc,
                byte_pc,
                None::<DirectCallArtifact>,
            ));
        }
    };
    record("runtimeForwardCallFrameSetup", start, entry_start);
    record("runtimeForwardCallNativeEntry", entry_start, ops.offset().0);
    completion::emit(
        ops,
        relocations,
        view,
        DirectCallForm::CallWithThis {
            callable: callee,
            receiver,
        },
        dst,
        view.code_block.id,
        logical_pc,
        layout,
        table.entry(abi::STUB_JIT_DEOPT_STACK_CALL),
        0,
        &mut record,
        bail,
        finish_error,
        throw_value,
        fatal,
        done,
        uncommitted_rejected,
        entry_rejected,
        caller_bail,
        prepare_error,
        prepare_throw,
        prepare_fatal,
        context_register,
        store,
        restore_roots,
    )?;
    dynasm!(ops ; .arch aarch64 ; =>scratch_miss ; add sp, sp, SCRATCH_BYTES ; b =>caller_bail);
    Ok(())
}
