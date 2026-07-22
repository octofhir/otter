//! Compiler-generated AArch64 call linkage.
//!
//! # Contents
//! - [`DirectCallSite`] — one baked monomorphic call-site description.
//! - [`emit_direct_call`] — exact callable guard, stack-owned callee frame,
//!   native entry, return, and cold deoptimization.
//!
//! # Invariants
//! - Normal call entry and return execute entirely in generated code. No
//!   resolver, prepare record, owner arena, generic call adapter, or shared
//!   machine trampoline participates in the hit path.
//! - Every failure before native entry is effect-free and branches to the
//!   caller's canonical deopt exit while its original `Call` PC is published.
//! - Callee registers are initialized tagged slots on the machine stack and
//!   are published with `NativeFrameFlags::STACK_REGISTERS` before any
//!   safepoint. Moving GC therefore rewrites them in place.
//! - A callee bailout is not replayed. The live published frame enters the
//!   cold stack-call deoptimizer, which resumes the already-started callee.
//! - Callers load the current generation through a stable per-function cell.
//!   Isolate execution is single-mutator: after selection no registry mutation
//!   can occur before native entry, and executable retirement is deferred while
//!   any native activation is published. Generated calls therefore need no
//!   per-entry exclusive lease loop.
//! - Tier publication patches the function cell; a missing target enters one
//!   no-allocation cold resolver and never invalidates the generated caller.
//! - The activation cursor is both the publication and generated-recursion
//!   bound. Prospective callee `sp` is compared with one immutable native-stack
//!   limit. Normal cleanup therefore restores frame publication only; it
//!   mutates no duplicate resource counters.
//!
//! # See also
//! - `otter-vm/src/native_abi/code_entry.rs` — stable generation leases.
//! - `otter-vm/src/native_abi/frame.rs` — stack-register root ownership.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::{
    JitCompileSnapshot, JitDirectCallThisMode, JitDirectCallee, closure::JS_CLOSURE_BODY_TYPE_TAG,
    native_abi as abi, value::tag as value_tag,
};

use crate::{
    artifact::{
        CodeMapCapture, CodeRegion, DirectCallArtifact, DirectCallKindArtifact,
        DirectCallThisModeArtifact, DirectCallTierArtifact,
        relocation::{RelocationCapture, RelocationTarget},
    },
    entry::{
        ACTIVATION_BASE_OFFSET, ACTIVATION_LIMIT_OFFSET, ACTIVATION_TOP_PTR_OFFSET,
        CODE_ENTRY_CODE_OBJECT_ID_OFFSET, CODE_ENTRY_GENERATED_BAIL_STREAK_OFFSET,
        CODE_ENTRY_GENERATED_DEOPTS_OFFSET, CODE_ENTRY_GENERATED_ENTRIES_OFFSET,
        CODE_ENTRY_GENERATED_STACK_FRAME_BYTES_OFFSET, CODE_ENTRY_GENERATED_THROWS_OFFSET,
        CODE_ENTRY_NATIVE_FRAME_HEADER_OFFSET, FUNCTION_ENTRY_GENERATION_CELL_OFFSET,
        GENERATED_FEEDBACK_CLEAN_OFFSET, GLOBAL_THIS_OFFSET_PTR_OFFSET, NATIVE_FRAME_OFFSET,
        NATIVE_FRAME_REGISTER_BASE_OFFSET, NATIVE_FRAME_SELF_OFFSET, NATIVE_FRAME_STACK_SIZE,
        NATIVE_FRAME_THIS_OFFSET, NATIVE_FRAME_UPVALUE_BASE_OFFSET,
        NATIVE_FRAME_UPVALUE_COUNT_OFFSET, NATIVE_STACK_LIMIT_OFFSET, STATUS_BAILED,
        STATUS_RETURNED, THREAD_OFFSET, Unsupported, VALUE_UNDEFINED,
        VM_THREAD_CODE_OBJECT_ID_OFFSET, VM_THREAD_CURRENT_FRAME_OFFSET, reg_offset,
    },
};

/// Maximum caller-owned linkage reservation accepted by one generated call.
/// The target's exact persistent prologue reservation is accounted separately
/// in the aggregate generated-call budget.
pub(crate) const MAX_DIRECT_CALL_FRAME_BYTES: u32 = 4_080;

/// Fully-typed source contract for generated linkage.
#[derive(Debug, Clone, Copy)]
pub(crate) enum DirectCallForm {
    /// Reload and validate an ordinary `Op::Call` callee.
    Plain { callable: u16 },
    /// Consume the exact callable and receiver proven by a method guard.
    Method { callable: u8, receiver: u16 },
}

/// One compiler-native call site.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DirectCallSite<'a> {
    pub(crate) target: &'a JitDirectCallee,
    pub(crate) caller_function_id: u32,
    pub(crate) logical_pc: u32,
    pub(crate) byte_pc: u32,
    pub(crate) dst: u16,
    pub(crate) form: DirectCallForm,
    pub(crate) arguments: &'a [u16],
}

#[derive(Debug, Clone, Copy)]
struct StackLayout {
    saved_x25: u32,
    entry_addr: u32,
    caller_frame: u32,
    caller_code_object_id: u32,
    target_cell: u32,
    frame_bytes: u32,
}

impl StackLayout {
    fn for_target(target: &JitDirectCallee) -> Option<Self> {
        let register_bytes = u32::from(target.plan.register_count).checked_mul(8)?;
        let spill = NATIVE_FRAME_STACK_SIZE.checked_add(register_bytes)?;
        target
            .plan
            .generated_stack_frame_bytes
            .filter(|bytes| *bytes != 0)?;
        let frame_bytes = spill.checked_add(40)?.checked_add(15)? & !15;
        (frame_bytes <= MAX_DIRECT_CALL_FRAME_BYTES).then_some(Self {
            saved_x25: spill,
            entry_addr: spill + 8,
            caller_frame: spill + 16,
            caller_code_object_id: spill + 24,
            target_cell: spill + 32,
            frame_bytes,
        })
    }
}

/// Whether a baked target fits the bounded generated stack-call layout.
#[must_use]
pub(crate) fn target_is_supported(target: &JitDirectCallee) -> bool {
    StackLayout::for_target(target).is_some()
}

fn emit_load_u64(ops: &mut Assembler, register: u8, value: u64) {
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

/// Increment one isolate-serial machine-visible `u64` feedback counter.
///
/// These counters cannot practically wrap within one process lifetime. Direct
/// addressing keeps exact call feedback to three straight-line instructions.
fn emit_increment_feedback_u64(ops: &mut Assembler, offset: u32) {
    dynasm!(ops
        ; .arch aarch64
        ; ldr x14, [x25, offset]
        ; add x14, x14, #1
        ; str x14, [x25, offset]
    );
}

/// Increment the cold `u32` consecutive-bail streak.
fn emit_increment_feedback_u32(ops: &mut Assembler, offset: u32) {
    dynasm!(ops
        ; .arch aarch64
        ; ldr w14, [x25, offset]
        ; add w14, w14, #1
        ; str w14, [x25, offset]
    );
}

fn emit_reset_generated_bail_streak(ops: &mut Assembler) {
    dynasm!(ops
        ; .arch aarch64
        ; str wzr, [x25, CODE_ENTRY_GENERATED_BAIL_STREAK_OFFSET]
    );
}

fn emit_symbol(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    register: u8,
    value: u64,
    target: RelocationTarget,
) {
    let start = ops.offset().0;
    emit_load_u64(ops, register, value);
    relocations.record_mov_wide(start, ops.offset().0, register, target);
}

fn emit_runtime_stub(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    register: u8,
    value: u64,
    descriptor: abi::RuntimeStubDescriptor,
) {
    emit_symbol(
        ops,
        relocations,
        register,
        value,
        RelocationTarget::runtime_stub(descriptor),
    );
}

/// Load the active realm's GC-rooted global object as full pointer-cheap
/// `Value` bits.
///
/// No allocation or safepoint occurs between reading the compressed root slot
/// and publishing it in the callee frame. A later moving collection rewrites
/// the published `NativeFrame::this_value` slot in place.
fn emit_load_sloppy_global_this(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
) {
    dynasm!(ops
        ; .arch aarch64
        ; ldr x14, [x20, GLOBAL_THIS_OFFSET_PTR_OFFSET]
        ; ldr w12, [x14]
    );
    emit_symbol(
        ops,
        relocations,
        14,
        view.cage_base as u64,
        RelocationTarget::GcCageBase,
    );
    dynasm!(ops ; .arch aarch64 ; orr x12, x14, x12);
}

fn record_region(
    code_map: &mut Option<&mut CodeMapCapture>,
    kind: &'static str,
    start: usize,
    end: usize,
    site: DirectCallSite<'_>,
    direct_call: DirectCallArtifact,
) {
    if let Some(code_map) = code_map.as_deref_mut() {
        code_map.record(CodeRegion::call_structural(
            kind,
            start,
            end,
            site.caller_function_id,
            site.logical_pc,
            site.byte_pc,
            direct_call,
        ));
    }
}

fn layout_and_artifact(
    view: &JitCompileSnapshot,
    site: DirectCallSite<'_>,
) -> Result<(StackLayout, DirectCallArtifact), Unsupported> {
    let layout = StackLayout::for_target(site.target)
        .ok_or(Unsupported::OperandShape("direct call stack frame"))?;
    if matches!(site.form, DirectCallForm::Plain { .. })
        && site.target.plan.this_mode == JitDirectCallThisMode::SloppyGlobal
        && view.cage_base == 0
    {
        return Err(Unsupported::OperandShape("sloppy direct call cage base"));
    }
    if matches!(site.form, DirectCallForm::Plain { .. })
        && site.target.plan.this_mode == JitDirectCallThisMode::MethodReceiver
    {
        return Err(Unsupported::OperandShape("plain call receiver binding"));
    }
    let callee_native_frame_bytes = site
        .target
        .plan
        .generated_stack_frame_bytes
        .ok_or(Unsupported::OperandShape("direct call target native frame"))?;
    let reserved_stack_bytes = layout
        .frame_bytes
        .checked_add(callee_native_frame_bytes)
        .ok_or(Unsupported::OperandShape("direct call stack reservation"))?;
    let direct_call = DirectCallArtifact {
        call_kind: match site.form {
            DirectCallForm::Plain { .. } => DirectCallKindArtifact::Plain,
            DirectCallForm::Method { .. } => DirectCallKindArtifact::Method,
        },
        target_function_id: site.target.plan.function_id,
        target_code_object_id: site.target.plan.code_object_id,
        target_tier: match site.target.plan.tier {
            abi::NativeFrameKind::Baseline => DirectCallTierArtifact::Template,
            abi::NativeFrameKind::Optimizing => DirectCallTierArtifact::Optimizing,
            abi::NativeFrameKind::Interpreter => {
                return Err(Unsupported::OperandShape("direct call target tier"));
            }
        },
        this_mode: match site.form {
            DirectCallForm::Method { .. } => DirectCallThisModeArtifact::MethodReceiver,
            DirectCallForm::Plain { .. } => match site.target.plan.this_mode {
                JitDirectCallThisMode::StrictOrLexical => {
                    DirectCallThisModeArtifact::StrictOrLexical
                }
                JitDirectCallThisMode::SloppyGlobal => DirectCallThisModeArtifact::SloppyGlobal,
                JitDirectCallThisMode::MethodReceiver => {
                    return Err(Unsupported::OperandShape("plain call receiver binding"));
                }
            },
        },
        callee_native_frame_bytes,
        linkage_bytes: layout.frame_bytes,
        reserved_stack_bytes,
        callee_register_count: site.target.plan.register_count,
    };
    Ok((layout, direct_call))
}

/// Build the typed identity attached to every region of one generated call.
pub(crate) fn direct_call_artifact(
    view: &JitCompileSnapshot,
    site: DirectCallSite<'_>,
) -> Result<DirectCallArtifact, Unsupported> {
    layout_and_artifact(view, site).map(|(_, artifact)| artifact)
}

/// Emit one complete generated call.
///
/// `bail` names the caller's exact pre-effect deopt exit. `threw` handles a
/// parked exception after the generated frame has been fully unwound.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_direct_call(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    site: DirectCallSite<'_>,
    deopt_entry: u64,
    resolve_direct_entry: u64,
    mut code_map: Option<&mut CodeMapCapture>,
    bail: DynamicLabel,
    threw: DynamicLabel,
    done: DynamicLabel,
) -> Result<(), Unsupported> {
    let (layout, direct_call) = layout_and_artifact(view, site)?;

    let direct_function = ops.new_dynamic_label();
    let closure = ops.new_dynamic_label();
    let callable_ready = ops.new_dynamic_label();
    let generation_ready = ops.new_dynamic_label();
    let uncommitted_rejected = ops.new_dynamic_label();
    let entry_rejected = ops.new_dynamic_label();
    let callee_returned = ops.new_dynamic_label();
    let callee_bailed = ops.new_dynamic_label();
    let result_ready = ops.new_dynamic_label();
    let cleanup = ops.new_dynamic_label();
    let returned = ops.new_dynamic_label();
    let cleanup_threw = ops.new_dynamic_label();

    let guard_start = ops.offset().0;
    // The effective activation limit combines physical publication capacity
    // with the outer entry's remaining recursion budget.
    dynasm!(ops
        ; .arch aarch64
        ; ldr x9, [x20, ACTIVATION_TOP_PTR_OFFSET]
        ; ldr x10, [x9]
        ; ldr x11, [x20, ACTIVATION_LIMIT_OFFSET]
        ; cmp x10, x11
        ; b.hs =>bail
    );

    match site.form {
        DirectCallForm::Method { callable, receiver } => {
            let receiver_offset = reg_offset(receiver)?;
            dynasm!(ops ; .arch aarch64 ; mov x9, X(callable));
            emit_load_u64(
                ops,
                10,
                value_tag::box_function_id(site.target.plan.function_id),
            );
            dynasm!(ops
                ; .arch aarch64
                ; cmp x9, x10
                ; b.eq =>direct_function
                // `method_guard` proved this exact value is a compatible
                // closure. No safepoint or mutation occurs between guards.
                ; ldr x10, [x9, view.closure_call_layout.upvalue_base_byte]
                ; ldr w11, [x9, view.closure_call_layout.upvalue_count_byte]
            );
            dynasm!(ops
                ; ldr x12, [x19, receiver_offset]
                ; b =>callable_ready
                ; =>direct_function
                ; mov x10, xzr
                ; mov w11, wzr
                ; ldr x12, [x19, receiver_offset]
            );
        }
        DirectCallForm::Plain { callable } => {
            let callee_offset = reg_offset(callable)?;
            dynasm!(ops ; .arch aarch64 ; ldr x9, [x19, callee_offset]);
            emit_load_u64(
                ops,
                10,
                value_tag::box_function_id(site.target.plan.function_id),
            );
            dynasm!(ops
                ; .arch aarch64
                ; cmp x9, x10
                ; b.eq =>direct_function
                ; cbz x9, =>bail
            );
            emit_load_u64(ops, 10, value_tag::NOT_CELL_MASK);
            dynasm!(ops
                ; .arch aarch64
                ; tst x9, x10
                ; b.eq =>closure
                ; b =>bail
                ; =>closure
                ; ldrb w10, [x9]
                ; cmp w10, JS_CLOSURE_BODY_TYPE_TAG as u32
                ; b.ne =>bail
                ; ldr w13, [x9, view.closure_call_layout.flags_byte]
            );
            emit_load_u64(
                ops,
                10,
                u64::from(view.closure_call_layout.runtime_setup_flags),
            );
            dynasm!(ops
                ; .arch aarch64
                ; tst w13, w10
                ; b.ne =>bail
                ; ldr w10, [x9, view.closure_call_layout.function_id_byte]
            );
            emit_load_u64(ops, 11, u64::from(site.target.plan.function_id));
            dynasm!(ops
                ; .arch aarch64
                ; cmp w10, w11
                ; b.ne =>bail
                ; ldr x10, [x9, view.closure_call_layout.upvalue_base_byte]
                ; ldr w11, [x9, view.closure_call_layout.upvalue_count_byte]
            );

            if site.target.plan.this_mode == JitDirectCallThisMode::StrictOrLexical {
                emit_load_u64(ops, 12, VALUE_UNDEFINED);
                emit_load_u64(ops, 14, u64::from(view.closure_call_layout.bound_this_flag));
                dynasm!(ops
                    ; .arch aarch64
                    ; tst w13, w14
                    ; b.eq =>callable_ready
                    ; ldr x12, [x9, view.closure_call_layout.bound_this_byte]
                    ; b =>callable_ready
                    ; =>direct_function
                    ; mov x10, xzr
                    ; mov w11, wzr
                );
                emit_load_u64(ops, 12, VALUE_UNDEFINED);
            } else {
                // Plain `Op::Call` supplies `undefined`, which sloppy call
                // binding normalizes to the active realm's global object. An
                // explicitly bound closure may require primitive `ToObject`;
                // keep that uncommon case on the exact pre-effect side exit.
                emit_load_u64(ops, 14, u64::from(view.closure_call_layout.bound_this_flag));
                dynasm!(ops
                    ; .arch aarch64
                    ; tst w13, w14
                    ; b.ne =>bail
                );
                emit_load_sloppy_global_this(ops, relocations, view);
                dynasm!(ops
                    ; .arch aarch64
                    ; b =>callable_ready
                    ; =>direct_function
                    ; mov x10, xzr
                    ; mov w11, wzr
                );
                emit_load_sloppy_global_this(ops, relocations, view);
            }
        }
    }
    dynasm!(ops ; .arch aarch64 ; =>callable_ready);
    record_region(
        &mut code_map,
        "directCallGuard",
        guard_start,
        ops.offset().0,
        site,
        direct_call,
    );

    let setup_start = ops.offset().0;
    // Reserve the fixed caller-owned linkage frame and root the callable
    // state before the cold resolver can clobber caller-saved registers. The
    // frame is not published and no shared resource accounting is committed
    // until the selected generation's dynamic stack contract is validated.
    dynasm!(ops
        ; .arch aarch64
        ; sub sp, sp, layout.frame_bytes
        ; str x25, [sp, layout.saved_x25]
        ; str x9, [sp, NATIVE_FRAME_SELF_OFFSET]
        ; str x10, [sp, NATIVE_FRAME_UPVALUE_BASE_OFFSET]
    );
    emit_load_u64(ops, 13, VALUE_UNDEFINED);
    dynasm!(ops
        ; .arch aarch64
        ; stp x12, x13, [sp, NATIVE_FRAME_THIS_OFFSET as i32]
        ; str x11, [sp, NATIVE_FRAME_UPVALUE_COUNT_OFFSET]
    );

    // Generated callers bake the permanent function-cell address. The hot
    // load selects its current generation; only an empty publication enters
    // the single no-allocation cold resolver.
    emit_symbol(
        ops,
        relocations,
        1,
        site.target.plan.entry_cell,
        RelocationTarget::DirectCallEntryCell {
            byte_pc: site.byte_pc,
            direct_call,
        },
    );
    dynasm!(ops
        ; .arch aarch64
        ; add x2, x1, FUNCTION_ENTRY_GENERATION_CELL_OFFSET
        ; ldar x25, [x2]
        ; cbnz x25, =>generation_ready
        ; mov x0, x20
    );
    emit_runtime_stub(
        ops,
        relocations,
        16,
        resolve_direct_entry,
        abi::STUB_JIT_RESOLVE_DIRECT_ENTRY,
    );
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; mov x25, x0
        ; cbz x25, =>uncommitted_rejected
        ; =>generation_ready
        ; str x25, [sp, layout.target_cell]
        ; ldr w15, [x25, CODE_ENTRY_GENERATED_STACK_FRAME_BYTES_OFFSET]
        ; cbz w15, =>uncommitted_rejected
    );
    dynasm!(ops
        ; .arch aarch64
        // `sp` already includes this caller's linkage frame. Subtracting the
        // target's persistent prologue reservation yields its prospective
        // deepest stack address without shared byte accounting.
        ; subs x12, sp, x15
        ; b.lo =>uncommitted_rejected
        ; ldr x11, [x20, NATIVE_STACK_LIMIT_OFFSET]
        ; cmp x12, x11
        ; b.lo =>uncommitted_rejected
        // The isolate is single-mutator. No VM transition occurs between the
        // stable generation load and this entry-address load, while the outer
        // published activation defers executable retirement across any later
        // reentry from the callee.
        ; ldr x16, [x25]
        ; cbz x16, =>entry_rejected
        ; str x16, [sp, layout.entry_addr]
        ; ldp x13, x14, [x25, CODE_ENTRY_NATIVE_FRAME_HEADER_OFFSET as i32]
        ; stp x13, x14, [sp]
        ; add x14, sp, NATIVE_FRAME_STACK_SIZE
        ; str x14, [sp, NATIVE_FRAME_REGISTER_BASE_OFFSET]
    );

    let copied_argument_count = site
        .arguments
        .len()
        .min(usize::from(site.target.plan.param_count));
    let initialized_register_count =
        usize::from(site.target.plan.register_count).saturating_sub(copied_argument_count);
    if initialized_register_count != 0 {
        let init_pair_count = initialized_register_count / 2;
        let init_offset = NATIVE_FRAME_STACK_SIZE
            + u32::try_from(copied_argument_count)
                .map_err(|_| Unsupported::OperandShape("direct call argument count"))?
                * 8;
        emit_load_u64(ops, 15, VALUE_UNDEFINED);
        dynasm!(ops
            ; .arch aarch64
            ; add x14, sp, init_offset
        );
        if init_pair_count != 0 {
            const MAX_UNROLLED_INIT_PAIRS: usize = 16;
            if init_pair_count <= MAX_UNROLLED_INIT_PAIRS {
                for _ in 0..init_pair_count {
                    dynasm!(ops
                        ; .arch aarch64
                        ; stp x15, x15, [x14], #16
                    );
                }
            } else {
                let init_loop = ops.new_dynamic_label();
                dynasm!(ops
                    ; .arch aarch64
                    ; movz w13, init_pair_count as u32
                    ; =>init_loop
                    ; stp x15, x15, [x14], #16
                    ; subs w13, w13, #1
                    ; b.ne =>init_loop
                );
            }
        }
        if initialized_register_count & 1 != 0 {
            dynasm!(ops
                ; .arch aarch64
                ; str x15, [x14]
            );
        }
    }
    for (argument, &source) in site
        .arguments
        .iter()
        .take(copied_argument_count)
        .enumerate()
    {
        let source_offset = reg_offset(source)?;
        let destination_offset = NATIVE_FRAME_STACK_SIZE
            + u32::try_from(argument)
                .map_err(|_| Unsupported::OperandShape("direct call argument index"))?
                * 8;
        dynasm!(ops
            ; .arch aarch64
            ; ldr x15, [x19, source_offset]
            ; str x15, [sp, destination_offset]
        );
    }

    dynasm!(ops
        ; .arch aarch64
        ; ldr x13, [x20, NATIVE_FRAME_OFFSET]
        ; ldr x14, [x20, THREAD_OFFSET]
        ; ldr x15, [x14, VM_THREAD_CODE_OBJECT_ID_OFFSET]
        ; str x13, [sp, layout.caller_frame]
        ; str x15, [sp, layout.caller_code_object_id]
        ; ldr x9, [x20, ACTIVATION_TOP_PTR_OFFSET]
        ; ldr x10, [x9]
        ; ldr x11, [x20, ACTIVATION_BASE_OFFSET]
        ; add x12, x11, x10, lsl #3
        ; mov x15, sp
        ; str x15, [x12]
        ; add x10, x10, #1
        ; str x10, [x9]
        ; str x15, [x20, NATIVE_FRAME_OFFSET]
        ; ldr x13, [x25, CODE_ENTRY_CODE_OBJECT_ID_OFFSET]
        ; stp x15, x13, [x14, VM_THREAD_CURRENT_FRAME_OFFSET as i32]
        ; ldr x16, [sp, layout.entry_addr]
    );
    record_region(
        &mut code_map,
        "directCallFrameSetup",
        setup_start,
        ops.offset().0,
        site,
        direct_call,
    );

    let enter_start = ops.offset().0;
    emit_increment_feedback_u64(ops, CODE_ENTRY_GENERATED_ENTRIES_OFFSET);
    dynasm!(ops
        ; .arch aarch64
        ; str xzr, [x20, GENERATED_FEEDBACK_CLEAN_OFFSET]
        ; mov x0, x20
        ; blr x16
    );
    record_region(
        &mut code_map,
        "directCallNativeEntry",
        enter_start,
        ops.offset().0,
        site,
        direct_call,
    );

    let return_start = ops.offset().0;
    dynasm!(ops
        ; .arch aarch64
        ; cmp x1, STATUS_BAILED as u32
        ; b.eq =>callee_bailed
        ; cmp x1, STATUS_RETURNED as u32
        ; b.eq =>callee_returned
    );
    emit_increment_feedback_u64(ops, CODE_ENTRY_GENERATED_THROWS_OFFSET);
    emit_reset_generated_bail_streak(ops);
    dynasm!(ops ; .arch aarch64 ; b =>result_ready ; =>callee_returned);
    emit_reset_generated_bail_streak(ops);
    let destination_offset = reg_offset(site.dst)?;
    dynasm!(ops
        ; .arch aarch64
        // Root the returned value in the still-live caller window before any
        // callee activation or lease is removed.
        ; str x0, [x19, destination_offset]
        ; b =>result_ready
        ; =>callee_bailed
    );
    emit_increment_feedback_u64(ops, CODE_ENTRY_GENERATED_DEOPTS_OFFSET);
    emit_increment_feedback_u32(ops, CODE_ENTRY_GENERATED_BAIL_STREAK_OFFSET);
    dynasm!(ops
        ; .arch aarch64
        ; mov x0, x20
        ; mov x1, sp
    );
    emit_load_u64(ops, 2, u64::from(site.caller_function_id));
    emit_load_u64(ops, 3, u64::from(site.logical_pc));
    dynasm!(ops
        ; .arch aarch64
        ; ldr x4, [x25, CODE_ENTRY_CODE_OBJECT_ID_OFFSET]
        ; ldr x5, [sp, layout.caller_code_object_id]
    );
    emit_load_u64(
        ops,
        6,
        match site.form {
            DirectCallForm::Plain { .. } => 0,
            DirectCallForm::Method { .. } => 1,
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
        ; cmp x1, STATUS_RETURNED as u32
        ; b.ne =>result_ready
        ; str x0, [x19, destination_offset]
        ; =>result_ready
        ; b =>cleanup
    );
    record_region(
        &mut code_map,
        "directCallReturn",
        return_start,
        ops.offset().0,
        site,
        direct_call,
    );

    let cleanup_start = ops.offset().0;
    dynasm!(ops
        ; .arch aarch64
        ; =>cleanup
        // Restore caller publication before retiring the callee generation.
        ; ldr x13, [sp, layout.caller_frame]
        ; ldr x15, [sp, layout.caller_code_object_id]
        ; str x13, [x20, NATIVE_FRAME_OFFSET]
        ; ldr x14, [x20, THREAD_OFFSET]
        ; stp x13, x15, [x14, VM_THREAD_CURRENT_FRAME_OFFSET as i32]
        ; ldr x9, [x20, ACTIVATION_TOP_PTR_OFFSET]
        ; ldr x10, [x9]
        ; sub x10, x10, #1
        ; str x10, [x9]
        ; ldr x11, [x20, ACTIVATION_BASE_OFFSET]
        ; add x12, x11, x10, lsl #3
        ; str xzr, [x12]
    );
    dynasm!(ops
        ; .arch aarch64
        ; ldr x25, [sp, layout.saved_x25]
        ; add sp, sp, layout.frame_bytes
        ; cmp x1, STATUS_RETURNED as u32
        ; b.eq =>returned
        ; b =>cleanup_threw
        ; =>returned
        ; b =>done
        ; =>cleanup_threw
        ; b =>threw
    );
    record_region(
        &mut code_map,
        "directCallCleanup",
        cleanup_start,
        ops.offset().0,
        site,
        direct_call,
    );

    // Entry rejection owns no JS effects or published callee frame.
    let entry_reject_start = ops.offset().0;
    dynasm!(ops
        ; .arch aarch64
        ; =>entry_rejected
        ; ldr x25, [sp, layout.saved_x25]
        ; add sp, sp, layout.frame_bytes
        ; b =>bail
        ; =>uncommitted_rejected
        ; ldr x25, [sp, layout.saved_x25]
        ; add sp, sp, layout.frame_bytes
        ; b =>bail
    );
    record_region(
        &mut code_map,
        "directCallEntryReject",
        entry_reject_start,
        ops.offset().0,
        site,
        direct_call,
    );

    Ok(())
}
