//! Compiler-generated AArch64 call linkage.
//!
//! # Contents
//! - [`DirectCallSite`] — one baked target in a generated call-site chain.
//! - [`emit_direct_call`] — exact callable guard, stack-owned callee frame,
//!   native entry, return, and cold deoptimization.
//! - [`emit_direct_call_with_access`] — the same linkage over allocator-owned
//!   value locations and caller-provided root restoration.
//! - [`runtime_forward`] — actual-target frame setup after a leaf admission.
//! - [`completion`] — one return/throw/deopt/cleanup implementation for both.
//!
//! # Invariants
//! - Plain, method, and spread call entry/return execute entirely in generated code.
//!   Exact class and guarded closure construction probe a collector-published nursery
//!   window and enters the rooted allocator only on guard, capacity, GC, or
//!   heap-cap miss; observable prototype lookup remains a pre-effect sibling.
//!   Body entry, frame linkage, return substitution, and cleanup stay generated.
//! - Every failure before native entry is effect-free and branches to the
//!   caller's canonical completion while its original call PC is published.
//!   Base constructs finish generation/stack validation before receiver
//!   preparation. An own data prototype and exact class wrapper allocate
//!   without reentry; uncertain shapes reach the observable lookup, and no
//!   post-effect rejection can replay `New`.
//! - Callee registers published by the copied frame header are initialized
//!   tagged slots on the machine stack. The register-base field locates them;
//!   fixed control slots and the upvalue spine precede the tagged windows.
//!   Safepoint-free scalar generations may
//!   publish only their parameter prefix; every cold exit expands it before
//!   VM reentry. Moving GC therefore sees exactly the initialized window.
//! - A spread call copies only the target's declared parameter prefix from the
//!   compiler-created dense array, after receiver preparation and before frame
//!   publication. Eligibility excludes rest and
//!   `arguments`, so ignored trailing values are unobservable to the callee.
//!   The prepared receiver is parked in the unpublished frame while caller
//!   roots refresh, so a recycled destination may alias the callee register.
//! - Forwarded calls copy the caller's live argument bindings without GC or JS
//!   reentry. Targets consuming actuals use a bounded dynamic window with a
//!   fixed control prefix; every exit releases its recorded reservation. Operand
//!   callbacks preserve x8 until reservation and require no dynamic SP bias.
//! - A callee bailout is not replayed. The live published frame enters the
//!   cold stack-call deoptimizer, which resumes the already-started callee.
//! - Callers load the current generation through a stable per-function cell.
//!   Executable retirement is deferred while an outer native activation is
//!   published. Plain/method calls enter immediately. A cold receiver miss may
//!   reenter while retaining the already-selected generation, which then enters
//!   through its normal dependency guards.
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

mod completion;
mod receiver_allocation;
use receiver_allocation::emit_generated_receiver_allocation;
pub(crate) use receiver_allocation::emit_receiver_probe;
mod forward_bindings;
mod layout;
mod runtime_forward;
mod tiering;
pub(crate) use runtime_forward::emit_runtime_forward;

use layout::StackLayout;

use crate::template::arm64::values::{CellTest, emit_cell_test};
use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::{
    JitCompileSnapshot, JitDirectCallThisMode, JitDirectCallee, closure::JS_CLOSURE_BODY_TYPE_TAG,
    native_abi as abi, value::tag as value_tag,
};

use crate::{
    artifact::{
        CodeMapCapture, CodeRegion, DirectCallArgumentModeArtifact, DirectCallArtifact,
        DirectCallKindArtifact, DirectCallThisModeArtifact, DirectCallTierArtifact,
        relocation::{RelocationCapture, RelocationTarget},
    },
    entry::{
        ACTIVATION_BASE_OFFSET, ACTIVATION_LIMIT_OFFSET, ACTIVATION_TOP_PTR_OFFSET,
        CODE_ENTRY_CODE_OBJECT_ID_OFFSET, CODE_ENTRY_FLAGS_OFFSET,
        CODE_ENTRY_GENERATED_DEOPTS_OFFSET, CODE_ENTRY_GENERATED_ENTRIES_OFFSET,
        CODE_ENTRY_GENERATED_STACK_FRAME_BYTES_OFFSET, CODE_ENTRY_GENERATED_THROWS_OFFSET,
        CODE_ENTRY_NATIVE_FRAME_HEADER_OFFSET, CODE_REGISTRY_VIEW_HOT_FUNCTION_OFFSET,
        FUNCTION_ENTRY_GENERATION_CELL_OFFSET, GC_PAGE_SIZE, GENERATED_FEEDBACK_CLEAN_OFFSET,
        GLOBAL_THIS_OFFSET_PTR_OFFSET, NATIVE_FRAME_FLAGS_OFFSET, NATIVE_FRAME_NEW_TARGET_OFFSET,
        NATIVE_FRAME_OFFSET, NATIVE_FRAME_PC_OFFSET, NATIVE_FRAME_REGISTER_BASE_OFFSET,
        NATIVE_FRAME_SELF_OFFSET, NATIVE_FRAME_THIS_OFFSET, NATIVE_FRAME_UPVALUE_BASE_OFFSET,
        NATIVE_FRAME_UPVALUE_COUNT_OFFSET, NATIVE_STACK_LIMIT_OFFSET, NEW_FROM_SPACE_KIND,
        OBJECT_BODY_TYPE_TAG, PAGE_ALLOCATED_BYTES_OFFSET, PAGE_BUMP_CURSOR_OFFSET,
        PAGE_SPACE_OFFSET, RECEIVER_ALLOC_ATTEMPTS_OFFSET, RECEIVER_ALLOC_GENERATED_OFFSET,
        RECEIVER_ALLOC_GUARD_MISSES_OFFSET, RECEIVER_ALLOC_MAX_HEAP_BYTES_OFFSET,
        RECEIVER_ALLOC_PAGE_OFFSET, RECEIVER_ALLOC_SPACE_MISSES_OFFSET,
        RECEIVER_ALLOC_TRACKED_BYTES_OFFSET, RECEIVER_ALLOC_TYPE_BYTES_OFFSET,
        RECEIVER_ALLOC_TYPE_COUNT_OFFSET, RECEIVER_ALLOC_TYPE_LIVE_BYTES_OFFSET,
        RUNTIME_STATS_OFFSET, THREAD_OFFSET, Unsupported, VALUE_HOLE, VALUE_NULL, VALUE_UNDEFINED,
        VM_THREAD_CODE_OBJECT_ID_OFFSET, VM_THREAD_CODE_REGISTRY_OFFSET,
        VM_THREAD_CURRENT_FRAME_OFFSET, VM_THREAD_MARKING_FLAG_CELL_OFFSET, reg_offset,
    },
};

/// Maximum caller-owned linkage reservation accepted by one generated call.
/// The target's exact persistent prologue reservation is accounted separately
/// in the aggregate generated-call budget.
pub(crate) const MAX_DIRECT_CALL_FRAME_BYTES: u32 = 4_080;
const PARAMETER_PREFIX_FLAG_BIT: u32 = abi::CODE_ENTRY_PARAMETER_PREFIX.trailing_zeros();

fn emit_increment_runtime_counter(ops: &mut Assembler, context_register: u8, offset: u32) {
    dynasm!(ops
        ; .arch aarch64
        ; ldr x15, [X(context_register), RUNTIME_STATS_OFFSET]
        ; ldr x14, [x15, offset]
        ; add x14, x14, #1
        ; str x14, [x15, offset]
    );
}

/// Fully-typed source contract for generated linkage.
#[derive(Debug, Clone, Copy)]
pub(crate) enum DirectCallForm {
    /// Reload and validate an ordinary `Op::Call` callee.
    Plain { callable: u16 },
    /// Reload an ordinary callable and consume an explicit `CallSpread`
    /// receiver. The generated path currently accepts the canonical
    /// `undefined` receiver; other receiver coercions side-exit pre-effect.
    CallWithThis { callable: u16, receiver: u16 },
    /// Consume the exact callable and receiver proven by a method guard.
    Method { callable: u8, receiver: u16 },
    /// Reload an exact base-constructor callable and use one dedicated Machine
    /// root home for its prepared receiver.
    Construct { callable: u16, receiver: u16 },
    /// Enter a derived constructor with an uninitialized receiver and the
    /// callable itself as `new.target`.
    DerivedConstruct { callable: u16 },
    /// Enter a base superclass with a prepared receiver and the caller's
    /// `new.target`.
    SuperConstruct { callable: u16, receiver: u16 },
    /// Enter a derived superclass with the caller's `new.target`.
    DerivedSuperConstruct { callable: u16 },
}

impl DirectCallForm {
    const fn is_construct(self) -> bool {
        !matches!(
            self,
            Self::Plain { .. } | Self::CallWithThis { .. } | Self::Method { .. }
        )
    }

    const fn is_derived(self) -> bool {
        matches!(
            self,
            Self::DerivedConstruct { .. } | Self::DerivedSuperConstruct { .. }
        )
    }

    const fn inherits_new_target(self) -> bool {
        matches!(
            self,
            Self::SuperConstruct { .. } | Self::DerivedSuperConstruct { .. }
        )
    }

    const fn construct_callable(self) -> Option<u16> {
        match self {
            Self::Construct { callable, .. }
            | Self::DerivedConstruct { callable }
            | Self::SuperConstruct { callable, .. }
            | Self::DerivedSuperConstruct { callable } => Some(callable),
            Self::Plain { .. } | Self::CallWithThis { .. } | Self::Method { .. } => None,
        }
    }

    const fn prepared_receiver(self) -> Option<u16> {
        match self {
            Self::Construct { receiver, .. } | Self::SuperConstruct { receiver, .. } => {
                Some(receiver)
            }
            Self::Plain { .. }
            | Self::CallWithThis { .. }
            | Self::Method { .. }
            | Self::DerivedConstruct { .. }
            | Self::DerivedSuperConstruct { .. } => None,
        }
    }
}

/// One compiler-native call site.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DirectCallSite<'a> {
    pub(crate) target: &'a JitDirectCallee,
    pub(crate) target_index: u32,
    pub(crate) target_count: u32,
    pub(crate) caller_function_id: u32,
    pub(crate) logical_pc: u32,
    pub(crate) byte_pc: u32,
    pub(crate) dst: u16,
    pub(crate) form: DirectCallForm,
    pub(crate) arguments: DirectCallArguments<'a>,
}

/// Source of the declared parameter prefix for one shared generated call.
#[derive(Debug, Clone, Copy)]
pub(crate) enum DirectCallArguments<'a> {
    /// Statically enumerated allocator or interpreter register values.
    Fixed(&'a [u16]),
    /// One compiler-created dense array containing already-evaluated values.
    Spread(u16),
    /// Live actual count in an X register, after the committed intrinsic-apply
    /// probe. Operand loaders must preserve x8 until the frame is reserved.
    Forward { count: u8 },
}

fn emit_release_linkage(ops: &mut Assembler, layout: &StackLayout) {
    if let Some(offset) = layout.allocation_size {
        dynasm!(ops ; .arch aarch64 ; ldr w16, [sp, offset] ; add sp, sp, x16);
    } else {
        dynasm!(ops ; .arch aarch64 ; add sp, sp, layout.frame_bytes);
    }
}

/// Whether a baked target fits the bounded generated stack-call layout.
#[must_use]
pub(crate) fn target_is_supported(target: &JitDirectCallee) -> bool {
    StackLayout::for_site(target, 0).is_some()
}

/// Compare `w11` against a baked function id.
///
/// AArch64's `cmp` carries a 12-bit unsigned immediate, and a function id is
/// counted per program rather than drawn from a small fixed set: past 4095 the
/// assembler refuses the instruction outright. A wider id is materialised into
/// `w15` first, so the guard is emitted whatever the id is.
fn emit_compare_function_id(ops: &mut Assembler, function_id: u32) {
    if function_id <= 0xfff {
        dynasm!(ops ; .arch aarch64 ; cmp w11, function_id);
        return;
    }
    dynasm!(ops
        ; .arch aarch64
        ; movz w15, function_id & 0xffff
        ; movk w15, function_id >> 16, lsl 16
        ; cmp w11, w15
    );
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

/// Synthesize the one canonical structural-failure pair.
fn emit_fatal_pair(ops: &mut Assembler) {
    emit_load_u64(ops, 0, VALUE_UNDEFINED);
    dynasm!(ops ; .arch aarch64 ; mov x1, abi::NativeResultStatus::Fatal as u64);
}

/// Branch on the exact ECMAScript Object-vs-primitive split for one tagged
/// value. Function-id immediates and every non-primitive GC body are Objects;
/// strings, symbols, and bigints are the only primitive cell families. The
/// three scratch registers are clobbered; `value` is preserved.
pub(crate) fn emit_object_type_branch(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    value: u8,
    scratch: [u8; 3],
    object: DynamicLabel,
    primitive: DynamicLabel,
) {
    let [a, b, c] = scratch;
    let non_cell = ops.new_dynamic_label();
    emit_cell_test(ops, value, a, CellTest::IsNotCell, non_cell);
    dynasm!(ops ; .arch aarch64 ; mov W(b), W(value));
    crate::template::arm64::values::emit_load_symbol_u64(
        ops,
        relocations,
        c,
        view.cage_base as u64,
        RelocationTarget::GcCageBase,
    );
    dynasm!(ops
        ; .arch aarch64
        ; add X(b), X(c), X(b)
        ; ldrb W(b), [X(b)]
    );
    for tag in view.primitive_cell_type_tags {
        emit_load_u64(ops, c, u64::from(tag));
        dynasm!(ops ; .arch aarch64 ; cmp W(b), W(c) ; b.eq =>primitive);
    }
    dynasm!(ops ; .arch aarch64 ; b =>object ; =>non_cell ; lsr X(b), X(value), #48);
    dynasm!(ops ; .arch aarch64 ; cbnz X(b), =>primitive);
    emit_load_u64(ops, b, value_tag::FUNCTION_ID_TAG);
    emit_load_u64(ops, c, 0xffff);
    dynasm!(ops
        ; .arch aarch64
        ; and W(c), W(value), W(c)
        ; cmp W(c), W(b)
        ; b.eq =>object
        ; b =>primitive
    );
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

/// [`abi::NativeFrameFlags::INCOMING_ARGUMENTS`] positioned in the 32-bit
/// header word that holds register count, tier, and flags.
const INCOMING_ARGUMENTS_HEADER_WORD: u32 = (abi::NativeFrameFlags::INCOMING_ARGUMENTS as u32)
    << (8
        * (std::mem::offset_of!(abi::VmFrameHeader, flags)
            - std::mem::offset_of!(abi::VmFrameHeader, register_count)));

/// Copy the fixed actual arguments into the stack-owned callee frame.
///
/// The declared parameter prefix lands in the callee's first registers. When
/// the target materializes `arguments`, every actual argument is also stored
/// in the published window after the register window, so the callee's
/// `arguments` object sees the exact call-site list.
fn emit_copy_fixed_arguments<Load>(
    ops: &mut Assembler,
    layout: &StackLayout,
    site: DirectCallSite<'_>,
    context: &'static str,
    load: &mut Load,
) -> Result<(), Unsupported>
where
    Load: FnMut(&mut Assembler, u16, u8, u32) -> Result<(), Unsupported>,
{
    let DirectCallArguments::Fixed(arguments) = site.arguments else {
        return Ok(());
    };
    let param_count = usize::from(site.target.plan.param_count);
    for (argument, &source) in arguments.iter().enumerate() {
        let index = u32::try_from(argument).map_err(|_| Unsupported::OperandShape(context))?;
        let in_prefix = argument < param_count;
        let in_window = index < layout.incoming_count;
        if !in_prefix && !in_window {
            break;
        }
        load(ops, source, 15, layout.frame_bytes)?;
        if in_prefix {
            let destination_offset = layout.register_base + index * 8;
            dynasm!(ops ; .arch aarch64 ; str x15, [sp, destination_offset]);
        }
        if in_window {
            let destination_offset = layout.incoming_base + index * 8;
            dynasm!(ops ; .arch aarch64 ; str x15, [sp, destination_offset]);
        }
    }
    Ok(())
}

/// Initialize one compile-time register range in the stack-owned callee frame.
fn emit_initialize_register_range(
    ops: &mut Assembler,
    layout: &StackLayout,
    start: usize,
    count: usize,
) {
    if count == 0 {
        return;
    }
    let start_offset = layout.register_base + start as u32 * 8;
    let pair_count = count / 2;
    emit_load_u64(ops, 15, VALUE_UNDEFINED);
    dynasm!(ops
        ; .arch aarch64
        ; add x14, sp, start_offset
    );
    if pair_count != 0 {
        const MAX_UNROLLED_INIT_PAIRS: usize = 16;
        if pair_count <= MAX_UNROLLED_INIT_PAIRS {
            for _ in 0..pair_count {
                dynasm!(ops
                    ; .arch aarch64
                    ; stp x15, x15, [x14], #16
                );
            }
        } else {
            let init_loop = ops.new_dynamic_label();
            dynasm!(ops
                ; .arch aarch64
                ; movz w13, pair_count as u32
                ; =>init_loop
                ; stp x15, x15, [x14], #16
                ; subs w13, w13, #1
                ; b.ne =>init_loop
            );
        }
    }
    if count & 1 != 0 {
        dynasm!(ops
            ; .arch aarch64
            ; str x15, [x14]
        );
    }
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
pub(super) fn emit_load_sloppy_global_this(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    context_register: u8,
) {
    dynasm!(ops
        ; .arch aarch64
        ; ldr x14, [X(context_register), GLOBAL_THIS_OFFSET_PTR_OFFSET]
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
    let argument_count = match site.arguments {
        DirectCallArguments::Fixed(arguments) => u32::try_from(arguments.len())
            .map_err(|_| Unsupported::OperandShape("direct call argument count"))?,
        DirectCallArguments::Spread(_) if site.target.plan.needs_incoming_arguments => {
            return Err(Unsupported::OperandShape(
                "spread direct call publishing incoming arguments",
            ));
        }
        DirectCallArguments::Spread(_) | DirectCallArguments::Forward { .. } => 0,
    };
    if matches!(site.arguments, DirectCallArguments::Forward { .. })
        && !matches!(site.form, DirectCallForm::CallWithThis { .. })
    {
        return Err(Unsupported::OperandShape("forwarded call receiver form"));
    }
    let layout = if matches!(site.arguments, DirectCallArguments::Forward { .. }) {
        StackLayout::for_forward(site.target)
    } else {
        StackLayout::for_site(site.target, argument_count)
    }
    .ok_or(Unsupported::OperandShape("direct call stack frame"))?;
    if matches!(
        site.form,
        DirectCallForm::Plain { .. } | DirectCallForm::CallWithThis { .. }
    ) && site.target.plan.this_mode == JitDirectCallThisMode::SloppyGlobal
        && view.cage_base == 0
    {
        return Err(Unsupported::OperandShape("sloppy direct call cage base"));
    }
    if matches!(
        site.form,
        DirectCallForm::Plain { .. } | DirectCallForm::CallWithThis { .. }
    ) && site.target.plan.this_mode == JitDirectCallThisMode::MethodReceiver
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
            DirectCallForm::Plain { .. } | DirectCallForm::CallWithThis { .. } => {
                DirectCallKindArtifact::Plain
            }
            DirectCallForm::Method { .. } => DirectCallKindArtifact::Method,
            DirectCallForm::Construct { .. } => DirectCallKindArtifact::Construct,
            DirectCallForm::DerivedConstruct { .. } => DirectCallKindArtifact::DerivedConstruct,
            DirectCallForm::SuperConstruct { .. } => DirectCallKindArtifact::SuperConstruct,
            DirectCallForm::DerivedSuperConstruct { .. } => {
                DirectCallKindArtifact::DerivedSuperConstruct
            }
        },
        argument_mode: match site.arguments {
            DirectCallArguments::Fixed(_) => DirectCallArgumentModeArtifact::Fixed,
            DirectCallArguments::Spread(_) => DirectCallArgumentModeArtifact::Spread,
            DirectCallArguments::Forward { .. } => DirectCallArgumentModeArtifact::Forward,
        },
        target_function_id: site.target.plan.function_id,
        target_index: site.target_index,
        target_count: site.target_count,
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
            DirectCallForm::Construct { .. } => DirectCallThisModeArtifact::ConstructReceiver,
            DirectCallForm::SuperConstruct { .. } => DirectCallThisModeArtifact::ConstructReceiver,
            DirectCallForm::DerivedConstruct { .. }
            | DirectCallForm::DerivedSuperConstruct { .. } => {
                DirectCallThisModeArtifact::DerivedConstructor
            }
            DirectCallForm::Plain { .. } | DirectCallForm::CallWithThis { .. } => {
                match site.target.plan.this_mode {
                    JitDirectCallThisMode::StrictOrLexical => {
                        DirectCallThisModeArtifact::StrictOrLexical
                    }
                    JitDirectCallThisMode::SloppyGlobal => DirectCallThisModeArtifact::SloppyGlobal,
                    JitDirectCallThisMode::MethodReceiver => {
                        return Err(Unsupported::OperandShape("plain call receiver binding"));
                    }
                    JitDirectCallThisMode::ConstructReceiver => {
                        return Err(Unsupported::OperandShape(
                            "plain call constructor receiver binding",
                        ));
                    }
                    JitDirectCallThisMode::DerivedConstructor => {
                        return Err(Unsupported::OperandShape(
                            "plain call derived-constructor binding",
                        ));
                    }
                }
            }
        },
        callee_native_frame_bytes,
        linkage_bytes: layout
            .allocation_size
            .is_none()
            .then_some(layout.frame_bytes),
        reserved_stack_bytes: layout
            .allocation_size
            .is_none()
            .then_some(reserved_stack_bytes),
        callee_register_count: site.target.plan.register_count,
        own_upvalue_count: site.target.plan.own_upvalue_count,
        inherited_upvalue_count: site.target.plan.inherited_upvalue_count,
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
/// `bail` names the caller's exact pre-effect deopt exit. `finish_error`
/// normalizes a parked runtime error, `throw_value` carries a pure JavaScript
/// exception in `x0`, and `fatal` propagates only `ctx.error`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_direct_call(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    site: DirectCallSite<'_>,
    deopt_entry: u64,
    resolve_direct_entry: u64,
    initialize_upvalues_entry: u64,
    code_map: Option<&mut CodeMapCapture>,
    bail: DynamicLabel,
    finish_error: DynamicLabel,
    throw_value: DynamicLabel,
    fatal: DynamicLabel,
    done: DynamicLabel,
) -> Result<(), Unsupported> {
    emit_direct_call_with_access(
        ops,
        relocations,
        view,
        site,
        deopt_entry,
        resolve_direct_entry,
        0,
        0,
        0,
        0,
        initialize_upvalues_entry,
        0,
        code_map,
        bail,
        finish_error,
        throw_value,
        fatal,
        done,
        20,
        |ops, source, target, _| {
            let offset = reg_offset(source)?;
            dynasm!(ops ; .arch aarch64 ; ldr X(target), [x19, offset]);
            Ok(())
        },
        |ops, destination, source, _| {
            let offset = reg_offset(destination)?;
            dynasm!(ops ; .arch aarch64 ; str X(source), [x19, offset]);
            Ok(())
        },
        |_| Ok(()),
        |_, _, _| {
            Err(Unsupported::OperandShape(
                "template construct receiver root",
            ))
        },
        |_, _| Ok(()),
        |_, _, _| {
            Err(Unsupported::OperandShape(
                "forward bindings outside forwarding site",
            ))
        },
    )
}

/// Emit generated linkage whose values live outside the interpreter window.
///
/// `load` and `store` receive an opaque site value id and the current stack
/// bias introduced by the linkage frame. `restore_roots` runs after every
/// effect-free rejection, normal return, or throw, with the original stack
/// pointer restored and before the result is committed. `refresh_roots` may
/// rewrite every allocator-owned register after a moving collection; the
/// linkage therefore reloads its private `x25` generation handle from the
/// linkage frame after each refresh. `load_binding` receives a mapped VM register
/// and the stable caller-SP base register, writes x14 from a current rooted home,
/// and obeys the scratch contract in `forward_bindings`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_direct_call_with_access<
    Load,
    Store,
    Restore,
    RootReceiver,
    Refresh,
    LoadBinding,
>(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    site: DirectCallSite<'_>,
    deopt_entry: u64,
    resolve_direct_entry: u64,
    try_prepare_construct_entry: u64,
    prepare_construct_entry: u64,
    derived_construct_result_entry: u64,
    copy_spread_arguments_entry: u64,
    initialize_upvalues_entry: u64,
    copy_forwarded_arguments_entry: u64,
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
    mut root_receiver: RootReceiver,
    mut refresh_roots: Refresh,
    load_binding: LoadBinding,
) -> Result<(), Unsupported>
where
    Load: FnMut(&mut Assembler, u16, u8, u32) -> Result<(), Unsupported>,
    Store: FnMut(&mut Assembler, u16, u8, u32) -> Result<(), Unsupported>,
    Restore: FnMut(&mut Assembler) -> Result<(), Unsupported>,
    RootReceiver: FnMut(&mut Assembler, u8, u32) -> Result<(), Unsupported>,
    Refresh: FnMut(&mut Assembler, u32) -> Result<(), Unsupported>,
    LoadBinding: FnMut(&mut Assembler, u16, u8) -> Result<(), Unsupported>,
{
    let (layout, direct_call) = layout_and_artifact(view, site)?;

    let direct_function = ops.new_dynamic_label();
    let callable_ready = ops.new_dynamic_label();
    let generation_ready = ops.new_dynamic_label();
    let uncommitted_rejected = ops.new_dynamic_label();
    let entry_rejected = ops.new_dynamic_label();
    let caller_bail = ops.new_dynamic_label();
    let construct_prepare_error = ops.new_dynamic_label();
    let construct_prepare_throw = ops.new_dynamic_label();
    let construct_prepare_fatal = ops.new_dynamic_label();
    let construct_prepare_ready = ops.new_dynamic_label();

    let guard_start = ops.offset().0;
    // The effective activation limit combines physical publication capacity
    // with the outer entry's remaining recursion budget.
    dynasm!(ops
        ; .arch aarch64
        ; ldr x9, [X(context_register), ACTIVATION_TOP_PTR_OFFSET]
        ; ldr x10, [x9]
        ; ldr x11, [X(context_register), ACTIVATION_LIMIT_OFFSET]
        ; cmp x10, x11
        ; b.hs =>caller_bail
    );

    if let DirectCallArguments::Forward { count } = site.arguments {
        if copy_forwarded_arguments_entry == 0 {
            return Err(Unsupported::OperandShape(
                "forwarded call argument copy entry",
            ));
        }
        // x8 is outside the callable/receiver proof's scratch set. The caller
        // probes intrinsic apply and the complete actual count once per site.
        dynasm!(ops ; .arch aarch64 ; mov x8, X(count));
        if layout.allocation_size.is_some() {
            let capacity = (MAX_DIRECT_CALL_FRAME_BYTES - layout.incoming_base) / 8;
            dynasm!(ops ; .arch aarch64 ; cmp w8, capacity ; b.hi =>caller_bail);
        }
    }

    match site.form {
        DirectCallForm::Method { callable, receiver } => {
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
                ; ldr w15, [x9, view.closure_call_layout.eval_env_byte]
            );
            load(ops, receiver, 12, 0)?;
            dynasm!(ops
                ; .arch aarch64
                ; b =>callable_ready
                ; =>direct_function
                ; mov x10, xzr
                ; mov w11, wzr
                ; mov w15, wzr
            );
            load(ops, receiver, 12, 0)?;
        }
        DirectCallForm::Plain { callable } | DirectCallForm::CallWithThis { callable, .. } => {
            let explicit_receiver = match site.form {
                DirectCallForm::CallWithThis { receiver, .. } => Some(receiver),
                _ => None,
            };
            load(ops, callable, 9, 0)?;
            emit_load_u64(
                ops,
                10,
                value_tag::box_function_id(site.target.plan.function_id),
            );
            dynasm!(ops
                ; .arch aarch64
                ; cmp x9, x10
                ; b.eq =>direct_function
                ; cbz x9, =>caller_bail
            );
            emit_cell_test(ops, 9, 10, CellTest::IsNotCell, caller_bail);
            dynasm!(ops
                ; .arch aarch64
                ; ldrb w10, [x9]
                ; cmp w10, JS_CLOSURE_BODY_TYPE_TAG as u32
                ; b.ne =>caller_bail
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
                ; b.ne =>caller_bail
                ; ldr w10, [x9, view.closure_call_layout.function_id_byte]
            );
            emit_load_u64(ops, 11, u64::from(site.target.plan.function_id));
            dynasm!(ops
                ; .arch aarch64
                ; cmp w10, w11
                ; b.ne =>caller_bail
                ; ldr x10, [x9, view.closure_call_layout.upvalue_base_byte]
                ; ldr w11, [x9, view.closure_call_layout.upvalue_count_byte]
                ; ldr w15, [x9, view.closure_call_layout.eval_env_byte]
            );

            if site.target.plan.this_mode == JitDirectCallThisMode::StrictOrLexical {
                if let Some(receiver) = explicit_receiver {
                    load(ops, receiver, 12, 0)?;
                } else {
                    emit_load_u64(ops, 12, VALUE_UNDEFINED);
                }
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
                    ; mov w15, wzr
                );
                if let Some(receiver) = explicit_receiver {
                    load(ops, receiver, 12, 0)?;
                } else {
                    emit_load_u64(ops, 12, VALUE_UNDEFINED);
                }
            } else {
                // An explicitly bound closure may require primitive
                // `ToObject`; keep that uncommon case on the exact pre-effect
                // side exit. Both callable forms then share one sloppy
                // receiver binding below.
                emit_load_u64(ops, 14, u64::from(view.closure_call_layout.bound_this_flag));
                let sloppy_this = ops.new_dynamic_label();
                dynasm!(ops
                    ; .arch aarch64
                    ; tst w13, w14
                    ; b.ne =>caller_bail
                    ; b =>sloppy_this
                    ; =>direct_function
                    ; mov x10, xzr
                    ; mov w11, wzr
                    ; mov w15, wzr
                    ; =>sloppy_this
                );
                // OrdinaryCallBindThis for a sloppy callee: `undefined` and
                // `null` — including the implicit receiver of a plain
                // `Op::Call` — bind the active realm's global object, an
                // Object receiver binds itself, and a primitive receiver needs
                // `ToObject`, which stays an interpreter operation behind the
                // exact pre-effect side exit.
                if let Some(receiver) = explicit_receiver {
                    let global_this = ops.new_dynamic_label();
                    load(ops, receiver, 12, 0)?;
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
                        callable_ready,
                        caller_bail,
                    );
                    dynasm!(ops ; .arch aarch64 ; =>global_this);
                }
                emit_load_sloppy_global_this(ops, relocations, view, context_register);
            }
        }
        DirectCallForm::Construct { callable, .. }
        | DirectCallForm::DerivedConstruct { callable }
        | DirectCallForm::SuperConstruct { callable, .. }
        | DirectCallForm::DerivedSuperConstruct { callable } => {
            load(ops, callable, 9, 0)?;
            let construct_callable = ops.new_dynamic_label();
            let class_wrapper = ops.new_dynamic_label();
            dynasm!(ops
                ; .arch aarch64
                ; mov x14, x9
                ; =>construct_callable
            );
            emit_load_u64(
                ops,
                10,
                value_tag::box_function_id(site.target.plan.function_id),
            );
            dynasm!(ops
                ; .arch aarch64
                ; cmp x9, x10
                ; b.eq =>direct_function
                ; cbz x9, =>caller_bail
            );
            emit_cell_test(ops, 9, 10, CellTest::IsNotCell, caller_bail);
            dynasm!(ops
                ; .arch aarch64
                ; ldrb w10, [x9]
                ; cmp w10, view.class_constructor_layout.type_tag as u32
                ; b.eq =>class_wrapper
                ; cmp w10, JS_CLOSURE_BODY_TYPE_TAG as u32
                ; b.ne =>caller_bail
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
                ; b.ne =>caller_bail
                ; ldr w10, [x9, view.closure_call_layout.function_id_byte]
            );
            emit_load_u64(ops, 11, u64::from(site.target.plan.function_id));
            dynasm!(ops
                ; .arch aarch64
                ; cmp w10, w11
                ; b.ne =>caller_bail
                ; ldr x10, [x9, view.closure_call_layout.upvalue_base_byte]
                ; ldr w11, [x9, view.closure_call_layout.upvalue_count_byte]
                ; ldr w15, [x9, view.closure_call_layout.eval_env_byte]
                ; b =>callable_ready
                ; =>direct_function
                ; mov x10, xzr
                ; mov w11, wzr
                ; mov w15, wzr
                ; b =>callable_ready
                ; =>class_wrapper
                ; ldr x9, [x9, view.class_constructor_layout.callable_byte]
                ; b =>construct_callable
            );
        }
    }
    dynasm!(ops ; .arch aarch64 ; =>callable_ready);
    emit_load_u64(ops, 13, u64::from(site.target.plan.inherited_upvalue_count));
    dynasm!(ops
        ; .arch aarch64
        ; cmp w11, w13
        ; b.ne =>caller_bail
    );
    record_region(
        &mut code_map,
        "directCallGuard",
        guard_start,
        ops.offset().0,
        site,
        direct_call,
    );

    let setup_start = ops.offset().0;
    // Reserve the caller-owned linkage frame and root the callable
    // state before the cold resolver can clobber caller-saved registers. The
    // frame is not published and no shared resource accounting is committed
    // until the selected generation's dynamic stack contract is validated.
    if let Some(allocation_size) = layout.allocation_size {
        dynasm!(ops
            ; .arch aarch64
            ; lsl x16, x8, #3
            ; add x16, x16, layout.incoming_base + 15
            ; and x16, x16, 0xffff_ffff_ffff_fff0u64
            ; sub sp, sp, x16
            ; str w16, [sp, allocation_size]
        );
    } else {
        dynasm!(ops ; .arch aarch64 ; sub sp, sp, layout.frame_bytes);
    }
    if matches!(site.arguments, DirectCallArguments::Forward { .. })
        && site.target.plan.needs_incoming_arguments
    {
        dynasm!(ops ; .arch aarch64 ; str w8, [sp, abi::NATIVE_FRAME_ARGUMENT_COUNT_OFFSET]);
    }
    dynasm!(ops
        ; .arch aarch64
        ; str x25, [sp, layout.saved_x25]
        ; str x9, [sp, NATIVE_FRAME_SELF_OFFSET]
        ; str x10, [sp, NATIVE_FRAME_UPVALUE_BASE_OFFSET]
        ; str w15, [sp, abi::NATIVE_FRAME_EVAL_ENV_OFFSET]
    );
    match site.form {
        form if form.is_construct() => {
            emit_load_u64(ops, 13, VALUE_UNDEFINED);
            let initial_new_target: u8 = if form.inherits_new_target() { 13 } else { 14 };
            dynasm!(ops
                ; .arch aarch64
                ; str x13, [sp, NATIVE_FRAME_SELF_OFFSET]
                ; str x13, [sp, NATIVE_FRAME_THIS_OFFSET]
                ; str X(initial_new_target), [sp, NATIVE_FRAME_NEW_TARGET_OFFSET]
                ; str xzr, [sp, NATIVE_FRAME_UPVALUE_BASE_OFFSET]
                ; str wzr, [sp, NATIVE_FRAME_UPVALUE_COUNT_OFFSET]
            );
        }
        DirectCallForm::Plain { .. }
        | DirectCallForm::CallWithThis { .. }
        | DirectCallForm::Method { .. } => {
            emit_load_u64(ops, 13, VALUE_UNDEFINED);
            dynasm!(ops
                ; .arch aarch64
                ; stp x12, x13, [sp, NATIVE_FRAME_THIS_OFFSET as i32]
                ; str w11, [sp, NATIVE_FRAME_UPVALUE_COUNT_OFFSET]
            );
        }
        _ => unreachable!("construct forms handled above"),
    }
    if site.target.plan.own_upvalue_count != 0 {
        dynasm!(ops
            ; .arch aarch64
            ; add x13, sp, layout.upvalue_base
            ; str x13, [sp, NATIVE_FRAME_UPVALUE_BASE_OFFSET]
            ; str wzr, [sp, NATIVE_FRAME_UPVALUE_COUNT_OFFSET]
        );
    }

    // Copy arguments before the cold generation resolver can clobber
    // allocator-owned caller-saved locations. The target plan fixes the
    // parameter offsets even while its current generation is unpublished.
    let copied_argument_count = match site.arguments {
        DirectCallArguments::Fixed(arguments) => arguments
            .len()
            .min(usize::from(site.target.plan.param_count)),
        DirectCallArguments::Spread(_) | DirectCallArguments::Forward { .. } => 0,
    };
    emit_copy_fixed_arguments(ops, &layout, site, "direct call argument index", &mut load)?;

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
        ; mov x0, X(context_register)
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
        ; ldr x11, [X(context_register), NATIVE_STACK_LIMIT_OFFSET]
        ; cmp x12, x11
        ; b.lo =>uncommitted_rejected
        // The outer published activation defers executable retirement. Plain
        // and method calls enter immediately; constructs retain this selected
        // address across receiver preparation and then enter its normal guards.
        ; ldr x16, [x25]
        ; cbz x16, =>entry_rejected
        ; str x16, [sp, layout.entry_addr]
        ; ldr x13, [x25, CODE_ENTRY_NATIVE_FRAME_HEADER_OFFSET]
        ; ldr w14, [x25, CODE_ENTRY_NATIVE_FRAME_HEADER_OFFSET + 8]
        ; str x13, [sp]
    );
    // The header word covers register count, tier, and flags. A target that
    // materializes `arguments` gets its actual-argument window published in
    // the same store; every frame records its argument count so the field is
    // never read uninitialized.
    if site.target.plan.needs_incoming_arguments {
        dynasm!(ops
            ; .arch aarch64
            ; orr w14, w14, INCOMING_ARGUMENTS_HEADER_WORD
        );
    }
    dynasm!(ops
        ; .arch aarch64
        ; str w14, [sp, 8]
        ; add x14, sp, layout.register_base
        ; str x14, [sp, NATIVE_FRAME_REGISTER_BASE_OFFSET]
    );

    if layout.allocation_size.is_none() {
        dynasm!(ops
            ; .arch aarch64
            ; movz w13, layout.incoming_count
            ; str w13, [sp, abi::NATIVE_FRAME_ARGUMENT_COUNT_OFFSET]
        );
    } else {
        // Every actual slot is a valid root before upvalue allocation can GC.
        let initialized = ops.new_dynamic_label();
        let initialize = ops.new_dynamic_label();
        emit_load_u64(ops, 15, VALUE_UNDEFINED);
        dynasm!(ops
            ; .arch aarch64
            ; ldr w13, [sp, abi::NATIVE_FRAME_ARGUMENT_COUNT_OFFSET]
            ; cbz w13, =>initialized
            ; add x14, sp, layout.incoming_base
            ; =>initialize
            ; str x15, [x14], #8
            ; subs w13, w13, #1
            ; b.ne =>initialize
            ; =>initialized
        );
    }

    let param_count = usize::from(site.target.plan.param_count);
    emit_initialize_register_range(
        ops,
        &layout,
        copied_argument_count,
        param_count.saturating_sub(copied_argument_count),
    );
    let local_count = usize::from(site.target.plan.register_count).saturating_sub(param_count);
    if local_count != 0 {
        let locals_ready = ops.new_dynamic_label();
        dynasm!(ops
            ; .arch aarch64
            ; ldr w13, [x25, CODE_ENTRY_FLAGS_OFFSET]
            ; tbnz w13, #PARAMETER_PREFIX_FLAG_BIT, =>locals_ready
        );
        emit_initialize_register_range(ops, &layout, param_count, local_count);
        dynasm!(ops ; .arch aarch64 ; =>locals_ready);
    }

    if let (Some(callable), Some(_receiver)) = (
        site.form.construct_callable(),
        site.form.prepared_receiver(),
    ) {
        let prepare_start = ops.offset().0;
        load(ops, callable, 9, layout.frame_bytes)?;
        if site.form.inherits_new_target() {
            dynasm!(ops
                ; .arch aarch64
                ; ldr x13, [X(context_register), NATIVE_FRAME_OFFSET]
                ; ldr x2, [x13, NATIVE_FRAME_NEW_TARGET_OFFSET]
            );
        } else {
            dynasm!(ops ; .arch aarch64 ; mov x2, x9);
        }
        dynasm!(ops
            ; .arch aarch64
            ; mov x0, X(context_register)
            ; mov x1, x9
        );
        emit_load_u64(ops, 3, u64::from(site.target.plan.function_id));
        let fast_start = ops.offset().0;
        if let Some(allocation) = site.target.receiver_allocation {
            let guard_miss = ops.new_dynamic_label();
            let space_miss = ops.new_dynamic_label();
            let cold_prepare = ops.new_dynamic_label();
            let machine_start = ops.offset().0;
            emit_generated_receiver_allocation(
                ops,
                relocations,
                view,
                allocation,
                context_register,
                guard_miss,
                space_miss,
                construct_prepare_ready,
            );
            dynasm!(ops ; .arch aarch64 ; =>guard_miss);
            emit_increment_runtime_counter(
                ops,
                context_register,
                RECEIVER_ALLOC_GUARD_MISSES_OFFSET,
            );
            dynasm!(ops ; .arch aarch64 ; b =>cold_prepare ; =>space_miss);
            emit_increment_runtime_counter(
                ops,
                context_register,
                RECEIVER_ALLOC_SPACE_MISSES_OFFSET,
            );
            dynasm!(ops ; .arch aarch64 ; =>cold_prepare);
            record_region(
                &mut code_map,
                "directConstructReceiverAllocFast",
                machine_start,
                ops.offset().0,
                site,
                direct_call,
            );
            emit_load_u64(ops, 4, 1);
        } else {
            emit_load_u64(ops, 4, 0);
        }
        let cold_start = ops.offset().0;
        emit_runtime_stub(
            ops,
            relocations,
            16,
            try_prepare_construct_entry,
            abi::STUB_JIT_TRY_PREPARE_BASE_CONSTRUCT,
        );
        dynasm!(ops
            ; .arch aarch64
            ; blr x16
            ; cmp x1, abi::NativeResultStatus::Success as u32
            ; b.eq =>construct_prepare_ready
            ; cmp x1, abi::NativeResultStatus::SideExit as u32
            ; b.eq >observable_prepare
            ; cmp x1, abi::NativeResultStatus::Throw as u32
            ; b.eq =>construct_prepare_error
            ; b =>construct_prepare_fatal
            ; observable_prepare:
        );
        if site.target.receiver_allocation.is_some() {
            record_region(
                &mut code_map,
                "directConstructReceiverAllocCold",
                cold_start,
                ops.offset().0,
                site,
                direct_call,
            );
        }
        record_region(
            &mut code_map,
            "directConstructPrepareFast",
            fast_start,
            ops.offset().0,
            site,
            direct_call,
        );
        // The fast probe misses before effects. Rebuild its caller-clobbered
        // operands and enter the exact observable prototype path once.
        load(ops, callable, 9, layout.frame_bytes)?;
        if site.form.inherits_new_target() {
            dynasm!(ops
                ; .arch aarch64
                ; ldr x13, [X(context_register), NATIVE_FRAME_OFFSET]
                ; ldr x2, [x13, NATIVE_FRAME_NEW_TARGET_OFFSET]
            );
        } else {
            dynasm!(ops ; .arch aarch64 ; mov x2, x9);
        }
        dynasm!(ops
            ; .arch aarch64
            ; mov x0, X(context_register)
            ; mov x1, x9
        );
        emit_load_u64(ops, 3, u64::from(site.target.plan.function_id));
        let observable_start = ops.offset().0;
        emit_runtime_stub(
            ops,
            relocations,
            16,
            prepare_construct_entry,
            abi::STUB_JIT_PREPARE_BASE_CONSTRUCT,
        );
        dynasm!(ops
            ; .arch aarch64
            ; blr x16
            ; cmp x1, abi::NativeResultStatus::Success as u32
            ; b.eq =>construct_prepare_ready
            ; cmp x1, abi::NativeResultStatus::Throw as u32
            ; b.eq =>construct_prepare_throw
            ; b =>construct_prepare_fatal
            ; =>construct_prepare_ready
        );
        record_region(
            &mut code_map,
            "directConstructPrepareObservable",
            observable_start,
            ops.offset().0,
            site,
            direct_call,
        );
        // The bytecode destination may recycle the callable register (the
        // canonical `NewSpread r2 r2 r3` shape). Preserve the returned
        // receiver outside the caller window, refresh any values moved by
        // prototype lookup, and load the callable before publishing the
        // receiver in its caller-owned root home.
        dynasm!(ops ; .arch aarch64 ; str x0, [sp, NATIVE_FRAME_THIS_OFFSET]);
        refresh_roots(ops, layout.frame_bytes)?;
        dynasm!(ops ; .arch aarch64 ; ldr x25, [sp, layout.target_cell]);
        load(ops, callable, 9, layout.frame_bytes)?;
        dynasm!(ops ; .arch aarch64 ; ldr x12, [sp, NATIVE_FRAME_THIS_OFFSET]);
        root_receiver(ops, 12, layout.frame_bytes)?;
        if site.form.inherits_new_target() {
            dynasm!(ops
                ; .arch aarch64
                ; ldr x13, [X(context_register), NATIVE_FRAME_OFFSET]
                ; ldr x14, [x13, NATIVE_FRAME_NEW_TARGET_OFFSET]
            );
        } else {
            dynasm!(ops ; .arch aarch64 ; mov x14, x9);
        }
        let direct_constructor = ops.new_dynamic_label();
        let closure_constructor = ops.new_dynamic_label();
        let constructor_state_ready = ops.new_dynamic_label();
        emit_load_u64(
            ops,
            10,
            value_tag::box_function_id(site.target.plan.function_id),
        );
        dynasm!(ops
            ; .arch aarch64
            ; cmp x9, x10
            ; b.eq =>direct_constructor
            ; ldrb w13, [x9]
            ; cmp w13, view.class_constructor_layout.type_tag as u32
            ; b.ne =>closure_constructor
            ; ldr x9, [x9, view.class_constructor_layout.callable_byte]
            ; cmp x9, x10
            ; b.eq =>direct_constructor
            ; =>closure_constructor
            ; ldr x10, [x9, view.closure_call_layout.upvalue_base_byte]
            ; ldr w11, [x9, view.closure_call_layout.upvalue_count_byte]
            ; ldr w15, [x9, view.closure_call_layout.eval_env_byte]
            ; b =>constructor_state_ready
            ; =>direct_constructor
            ; mov x10, xzr
            ; mov w11, wzr
            ; mov w15, wzr
            ; =>constructor_state_ready
            ; str x9, [sp, NATIVE_FRAME_SELF_OFFSET]
            ; str x12, [sp, NATIVE_FRAME_THIS_OFFSET]
            ; str x14, [sp, NATIVE_FRAME_NEW_TARGET_OFFSET]
            ; str x10, [sp, NATIVE_FRAME_UPVALUE_BASE_OFFSET]
            ; str w11, [sp, NATIVE_FRAME_UPVALUE_COUNT_OFFSET]
            ; str w15, [sp, abi::NATIVE_FRAME_EVAL_ENV_OFFSET]
        );
        emit_copy_fixed_arguments(ops, &layout, site, "construct argument index", &mut load)?;
        record_region(
            &mut code_map,
            "directConstructPrepare",
            prepare_start,
            ops.offset().0,
            site,
            direct_call,
        );
    }
    if site.form.is_derived() {
        let callable = site
            .form
            .construct_callable()
            .expect("derived construct carries callable");
        load(ops, callable, 9, layout.frame_bytes)?;
        let direct_constructor = ops.new_dynamic_label();
        let closure_constructor = ops.new_dynamic_label();
        let constructor_state_ready = ops.new_dynamic_label();
        emit_load_u64(
            ops,
            13,
            value_tag::box_function_id(site.target.plan.function_id),
        );
        dynasm!(ops
            ; .arch aarch64
            ; mov x14, x9
            ; cmp x9, x13
            ; b.eq =>direct_constructor
            ; ldrb w15, [x9]
            ; cmp w15, view.class_constructor_layout.type_tag as u32
            ; b.ne =>closure_constructor
            ; ldr x9, [x9, view.class_constructor_layout.callable_byte]
            ; cmp x9, x13
            ; b.eq =>direct_constructor
            ; =>closure_constructor
            ; ldr x10, [x9, view.closure_call_layout.upvalue_base_byte]
            ; ldr w11, [x9, view.closure_call_layout.upvalue_count_byte]
            ; ldr w15, [x9, view.closure_call_layout.eval_env_byte]
            ; b =>constructor_state_ready
            ; =>direct_constructor
            ; mov x10, xzr
            ; mov w11, wzr
            ; mov w15, wzr
            ; =>constructor_state_ready
        );
        if site.form.inherits_new_target() {
            dynasm!(ops
                ; .arch aarch64
                ; ldr x13, [X(context_register), NATIVE_FRAME_OFFSET]
                ; ldr x14, [x13, NATIVE_FRAME_NEW_TARGET_OFFSET]
            );
        }
        emit_load_u64(ops, 12, VALUE_HOLE);
        dynasm!(ops
            ; .arch aarch64
            ; str x9, [sp, NATIVE_FRAME_SELF_OFFSET]
            ; str x12, [sp, NATIVE_FRAME_THIS_OFFSET]
            ; str x14, [sp, NATIVE_FRAME_NEW_TARGET_OFFSET]
            ; str x10, [sp, NATIVE_FRAME_UPVALUE_BASE_OFFSET]
            ; str w11, [sp, NATIVE_FRAME_UPVALUE_COUNT_OFFSET]
            ; str w15, [sp, abi::NATIVE_FRAME_EVAL_ENV_OFFSET]
            ; ldrb w15, [sp, NATIVE_FRAME_FLAGS_OFFSET]
            ; orr w15, w15, abi::NativeFrameFlags::DERIVED_CONSTRUCTOR as u32
            ; strb w15, [sp, NATIVE_FRAME_FLAGS_OFFSET]
        );
    }
    if site.target.plan.own_upvalue_count != 0 {
        if initialize_upvalues_entry == 0 {
            return Err(Unsupported::OperandShape(
                "direct call upvalue initialization transition",
            ));
        }
        dynasm!(ops
            ; .arch aarch64
            ; add x13, sp, layout.upvalue_base
            ; str x13, [sp, NATIVE_FRAME_UPVALUE_BASE_OFFSET]
            ; str wzr, [sp, NATIVE_FRAME_UPVALUE_COUNT_OFFSET]
        );
        dynasm!(ops
            ; .arch aarch64
            ; mov x0, X(context_register)
            ; mov x1, sp
        );
        emit_load_u64(ops, 2, u64::from(site.target.plan.own_upvalue_count));
        emit_load_u64(ops, 3, u64::from(site.target.plan.inherited_upvalue_count));
        emit_runtime_stub(
            ops,
            relocations,
            16,
            initialize_upvalues_entry,
            abi::STUB_JIT_INITIALIZE_UPVALUES,
        );
        let initialized = ops.new_dynamic_label();
        dynasm!(ops
            ; .arch aarch64
            ; blr x16
            ; cmp x1, abi::NativeResultStatus::Success as u32
            ; b.eq =>initialized
            ; cmp x1, abi::NativeResultStatus::SideExit as u32
            ; b.eq =>uncommitted_rejected
            ; cmp x1, abi::NativeResultStatus::Throw as u32
            ; b.eq =>construct_prepare_error
            ; b =>construct_prepare_fatal
            ; =>initialized
        );
    }
    if let DirectCallArguments::Spread(arguments) = site.arguments {
        if copy_spread_arguments_entry == 0 {
            return Err(Unsupported::OperandShape(
                "spread direct call argument transition",
            ));
        }
        // Resolve the target and prepare a base receiver before reading the
        // spread array again: either path may clobber allocator registers, and
        // receiver preparation may move the array. The Machine root record is
        // the sole owner of that live value across both transitions.
        refresh_roots(ops, layout.frame_bytes)?;
        dynasm!(ops ; .arch aarch64 ; ldr x25, [sp, layout.target_cell]);
        load(ops, arguments, 1, layout.frame_bytes)?;
        dynasm!(ops
            ; .arch aarch64
            ; mov x0, X(context_register)
            ; mov x2, sp
        );
        emit_load_u64(ops, 3, u64::from(site.target.plan.param_count));
        emit_runtime_stub(
            ops,
            relocations,
            16,
            copy_spread_arguments_entry,
            abi::STUB_JIT_COPY_SPREAD_ARGUMENTS,
        );
        dynasm!(ops
            ; .arch aarch64
            ; blr x16
            ; cbnz x0, =>uncommitted_rejected
        );
    }
    if matches!(site.arguments, DirectCallArguments::Forward { .. }) {
        dynasm!(ops ; .arch aarch64 ; mov x0, X(context_register) ; mov x1, sp);
        emit_load_u64(ops, 2, u64::from(site.target.plan.param_count));
        emit_runtime_stub(
            ops,
            relocations,
            16,
            copy_forwarded_arguments_entry,
            abi::STUB_JIT_COPY_FORWARDED_ARGUMENTS,
        );
        dynasm!(ops ; .arch aarch64 ; blr x16 ; tbnz x0, #63, =>uncommitted_rejected);
        emit_load_u64(ops, 2, u64::from(site.target.plan.param_count));
        emit_load_u64(ops, 3, u64::from(site.target.plan.register_count));
        forward_bindings::emit(ops, view, layout, load_binding)?;
    }
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
    record_region(
        &mut code_map,
        "directCallFrameSetup",
        setup_start,
        ops.offset().0,
        site,
        direct_call,
    );

    let enter_start = ops.offset().0;
    tiering::emit_entry(ops, context_register);
    dynasm!(ops
        ; .arch aarch64
        ; str xzr, [X(context_register), GENERATED_FEEDBACK_CLEAN_OFFSET]
        ; mov x0, X(context_register)
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

    completion::emit(
        ops,
        relocations,
        view,
        site.form,
        site.dst,
        site.caller_function_id,
        site.logical_pc,
        layout,
        deopt_entry,
        derived_construct_result_entry,
        |kind, start, end| record_region(&mut code_map, kind, start, end, site, direct_call),
        bail,
        finish_error,
        throw_value,
        fatal,
        done,
        uncommitted_rejected,
        entry_rejected,
        caller_bail,
        construct_prepare_error,
        construct_prepare_throw,
        construct_prepare_fatal,
        context_register,
        store,
        restore_roots,
    )
}
