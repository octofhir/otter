//! VM re-entry stubs for exceptions and non-call runtime operations.
//!
//! # Contents
//! - Cold stable function-entry repair for generated calls.
//! - Propagated-throw resumption in live compiled callers.
//! - Reentrant construction and closure/function creation.
//! - Fast allocating and observable base-constructor receiver preparation,
//!   plus return substitution.
//! - Stack-owned built-in Array iterator collection and spread-result append.
//! - Fixed committed boxed-value completion for binding, declaration,
//!   object-protocol, and scalar families, plus typed value-load/class
//!   operations.
//! - Pure-exception routing for Template propagation and local handlers.
//! - Reentrant equality, typed numeric-family, and unary-coercion completion.
//! - Cooperative backedge polling.
//!
//! # Invariants
//! Every entry receives a live JIT context whose canonical
//! [`NativeFrame`](otter_vm::native_abi::NativeFrame) publishes frame/register
//! roots for the entire call. Binding/declaration/object-protocol/scalar entries
//! receive only boxed values; the published function/PC selects a typed
//! operation before semantics begin. Their JavaScript throws return as pure
//! exception values, while only structural `Fatal` failures remain parked.
//! Numeric/load/class opcode words are decoded exactly once at their ABI edge.
//! Built-in Array spread collection accepts both materialized and stack-owned
//! frames; observable iterator overrides bail before effects.
//! Static stack-owned catch entry/removal use verified CodeBlock regions;
//! committed throws select their local catch before exact continuation.
//!
//! # See also
//! - `super::super::abi` — machine-visible entry context.
//! - `super::calls` — native activation and generated-call deoptimization.

use otter_bytecode::Op;
use otter_vm::{
    JitExceptionOutcome, NumericRuntimeOp, UnaryCoercionOp, Value, VmError,
    native_abi::{
        ClassRuntimeOp, CommittedValueError, IteratorRuntimeOutcome, NativeResultPair,
        NativeResultStatus, ValueLoadRuntimeOp,
    },
};

use super::super::JitCtx;
use super::decode_register;

pub(crate) fn park_jit_error(ctx: &mut JitCtx, err: VmError) {
    // SAFETY: every `JitCtx` is built with an initialized error slot that lives
    // for the compiled entry's dynamic extent; nested direct calls reuse the
    // same context and slot.
    unsafe {
        *ctx.error = Some(err);
    }
}

/// Encode one effect-once value completion. Catchable failures become a pure
/// JavaScript exception value with no parked propagation state; structural
/// host failures stay parked and use the distinct fatal status.
fn committed_value_result(
    ctx: &mut JitCtx,
    result: Result<Value, CommittedValueError>,
) -> NativeResultPair {
    match result {
        Ok(value) => NativeResultPair::success(value),
        Err(CommittedValueError::JavaScript(err)) => match ctx
            .runtime_call()
            .and_then(|mut runtime| runtime.take_js_throw(err))
        {
            Ok(exception) => NativeResultPair::throw_value(exception),
            Err(fatal) => {
                park_jit_error(ctx, fatal);
                NativeResultPair::fatal_internal()
            }
        },
        Err(CommittedValueError::Fatal(err)) => {
            park_jit_error(ctx, err);
            NativeResultPair::fatal_internal()
        }
    }
}

/// Convert one post-entry VM result into the committed boxed-value ABI.
pub(super) fn committed_vm_result(
    ctx: &mut JitCtx,
    result: Result<Value, VmError>,
) -> NativeResultPair {
    match result {
        Ok(value) => NativeResultPair::success(value),
        Err(error) => match ctx
            .runtime_call()
            .and_then(|mut runtime| runtime.take_js_throw(error))
        {
            Ok(exception) => NativeResultPair::throw_value(exception),
            Err(fatal) => {
                park_jit_error(ctx, fatal);
                NativeResultPair::fatal_internal()
            }
        },
    }
}

pub(super) fn compiled_fatal(ctx: &mut JitCtx, error: VmError) -> NativeResultPair {
    park_jit_error(ctx, error);
    NativeResultPair::fatal_internal()
}

pub(super) fn compiled_error(ctx: &mut JitCtx, error: VmError) -> NativeResultPair {
    match ctx
        .runtime_call()
        .and_then(|mut runtime| runtime.take_js_throw(error))
    {
        Ok(exception) => NativeResultPair::throw_value(exception),
        Err(fatal) => compiled_fatal(ctx, fatal),
    }
}

/// Route a pure exception through the activation's local handler or propagate
/// the same boxed value unchanged. Stack-owned compiled callers never stage a
/// pending throw between native frames.
fn route_throw_value(ctx: &mut JitCtx, exception: Value) -> NativeResultPair {
    let route = ctx
        .runtime_call()
        .and_then(|mut runtime| runtime.route_throw(exception));
    match route {
        Ok(Some(pc)) => {
            let Some(native_frame) = (unsafe { ctx.native_frame.as_mut() }) else {
                return compiled_fatal(ctx, VmError::InvalidOperand);
            };
            native_frame.header.pc = pc;
            NativeResultPair::side_exit(u64::from(pc))
        }
        Ok(None) => NativeResultPair::throw_value(exception),
        Err(error) => compiled_error(ctx, error),
    }
}

/// Repair one empty stable generated-call function cell.
///
/// This transition cannot compile, allocate, or reenter JavaScript. It only
/// asks the isolate registry to republish an already-installed fallback
/// generation and returns that generation-cell address.
pub(crate) extern "C" fn jit_resolve_direct_entry_stub(
    ctx: *mut JitCtx,
    function_entry_addr: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let Some(activation) = ctx.checked_activation() else {
        return 0;
    };
    // SAFETY: the compiled activation owns exclusive isolate execution for the
    // dynamic extent of this no-allocation resolver call.
    let vm = unsafe { &mut *activation.vm_ptr() };
    vm.jit_resolve_direct_entry(function_entry_addr)
}

/// Route one pure exception value returned by generated code.
///
/// Materialized Template frames may consume it in a local catch/finally and
/// stack-owned Template frames in a static catch, selecting the committed
/// landing PC. Unhandled frames publish the
/// value as the canonical uncaught throw. Machine local landings bypass this
/// entry and consume the exception value directly in SSA.
pub(crate) extern "C" fn jit_route_throw_stub(
    ctx: *mut JitCtx,
    exception_bits: u64,
) -> NativeResultPair {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let exception = Value::from_bits(exception_bits);
    route_throw_value(ctx, exception)
}

/// Clear diagnostic throw provenance after a Machine local catch absorbs the
/// exception SSA value. This leaf is called only on the exceptional edge.
pub(crate) extern "C" fn jit_acknowledge_caught_throw_stub(ctx: *mut JitCtx) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let Some(activation) = ctx.checked_activation() else {
        park_jit_error(ctx, VmError::InvalidOperand);
        return NativeResultStatus::Fatal as u64;
    };
    // SAFETY: the compiled activation owns exclusive isolate execution.
    unsafe { &mut *activation.vm_ptr() }.jit_acknowledge_caught_throw();
    NativeResultStatus::Success as u64
}

/// Rebuild every interpreter frame a deopt exit owes, from deopt metadata.
///
/// The generated exit site is an index and a branch; the shared handler dumps
/// the allocatable registers (GPRs in allocation order, then FP registers) at
/// `dump`, and this walks the code object's [`DeoptRuntime`] to reconstitute
/// every slot of every owed frame.
///
/// An exit that owes only the compiled function's own frame writes it back
/// into the published window and reports `Bail(exact_pc)`: the activation the
/// entry already owns resumes at the exact PC.
///
/// An exit inside a spliced body owes a whole chain, and that chain is
/// **constructed, not replayed**. Every frame — the compiled function's
/// included — is reconstituted into owned storage and the interpreter runs the
/// chain to completion, so the exit reports `Return(value)` with the
/// outermost frame's result. Nothing about it depends on how the compiled
/// function was entered, which is what lets a unit with spliced frames be a
/// generated direct-call target like any other.
///
/// A rebuilt spliced frame draws no upvalue spine: eligibility declines a
/// candidate whose body reads or writes a captured binding, so no instruction
/// the rebuilt body can reach observes one.
pub(crate) extern "C" fn jit_deopt_writeback_stub(
    ctx: *mut JitCtx,
    exit_index: u64,
    deopt_runtime: *const otter_vm::deopt::DeoptRuntime,
    dump: *const u64,
    frame_sp: u64,
    window: u64,
) -> NativeResultPair {
    use otter_vm::deopt::{DeoptFrame, DeoptLocation, DeoptRuntime};

    // SAFETY: the live `JitCtx` reentry contract; the deopt-runtime allocation
    // lives exactly as long as the code that baked its address.
    let ctx = unsafe { &mut *ctx };
    let runtime: &DeoptRuntime = unsafe { &*deopt_runtime };
    let Some(exit) = runtime.exits.get(exit_index as usize) else {
        return compiled_fatal(ctx, VmError::InvalidOperand);
    };
    let Some(state) = runtime.table.lookup(exit.state) else {
        return compiled_fatal(ctx, VmError::InvalidOperand);
    };
    if exit.resume_pcs.len() != state.frames.len() {
        return compiled_fatal(ctx, VmError::InvalidOperand);
    }

    let slot_raw = |location: DeoptLocation| -> u64 {
        match location {
            DeoptLocation::Register(register) => {
                let index = if register < runtime.gpr_budget {
                    usize::from(register)
                } else {
                    usize::from(runtime.gpr_budget) + usize::from(register - runtime.gpr_budget)
                };
                // SAFETY: the handler dumps every allocatable register in this
                // exact order before calling in.
                unsafe { *dump.add(index) }
            }
            DeoptLocation::StackSlot(offset) => {
                let address = frame_sp.wrapping_add(offset as i64 as u64);
                // SAFETY: the verified deopt table bounds every stack-slot
                // offset inside the live spill frame.
                unsafe { *(address as *const u64) }
            }
            DeoptLocation::Literal(raw) => raw,
        }
    };
    let write_frame = |frame: &DeoptFrame, window: *mut otter_vm::Value| {
        for (register, slot) in frame.slots.iter().enumerate() {
            let value = slot.repr.reconstitute(slot_raw(slot.location));
            // SAFETY: the window spans exactly the frame's declared registers
            // and stays rooted for the writeback.
            unsafe {
                window.add(register).write(value);
            }
        }
    };

    if state.is_single_frame() {
        let Ok(register_count) = u16::try_from(state.outermost().slots.len()) else {
            return compiled_fatal(ctx, VmError::InvalidOperand);
        };
        let Some(native_frame) = (unsafe { ctx.native_frame.as_ref() }) else {
            return compiled_fatal(ctx, VmError::InvalidOperand);
        };
        if native_frame.header.function_id != state.outermost().function_id {
            return compiled_fatal(ctx, VmError::InvalidOperand);
        }
        // A materialized entry owns an interpreter cold handler stack and
        // rebuilds it here. A generated STACK_REGISTERS callee owns no such
        // sidecar: it writes its exact PC/window, returns Bail, and the
        // caller's jit_deopt_stack_call transition reconstructs handlers while
        // materializing that frame. Treating both layouts alike makes every
        // Machine deopt from a generated callee fail before materialization.
        let stack_owned = native_frame
            .header
            .flags
            .contains(otter_vm::native_abi::NativeFrameFlags::STACK_REGISTERS);
        let rebuild_result = (|| {
            if stack_owned {
                return Ok(());
            }
            let activation = match ctx.checked_activation() {
                Some(activation) => activation,
                None => {
                    // Pure emitter fixtures intentionally execute arithmetic
                    // and deopt code without a VM runtime context. They cannot
                    // own a materialized catch stack. Production compiled
                    // entries always publish the activation used below.
                    #[cfg(test)]
                    return Ok(());
                    #[cfg(not(test))]
                    return Err(VmError::InvalidOperand);
                }
            };
            let frame_index = ctx.materialized_frame_index()?;
            // SAFETY: the live runtime activation owns these pointers for the
            // complete compiled-entry transaction. Handler reconstruction
            // mutates only the materialized frame's cold handler stack.
            let vm = unsafe { &mut *activation.vm_ptr() };
            let stack = unsafe { &mut *activation.stack_ptr() };
            let context = unsafe { &*activation.context_ptr() };
            vm.jit_rebuild_materialized_catch_handlers(
                context,
                stack,
                frame_index,
                exit.resume_pcs[0],
            )
        })();
        if let Err(error) = rebuild_result {
            return compiled_fatal(ctx, error);
        }
        write_frame(state.outermost(), window as *mut otter_vm::Value);
        // SAFETY: runtime-capable JIT contexts publish this native frame for
        // the full compiled entry dynamic extent.
        let Some(native_frame) = (unsafe { ctx.native_frame.as_mut() }) else {
            return compiled_fatal(ctx, VmError::InvalidOperand);
        };
        native_frame.header.register_count = register_count;
        native_frame.header.pc = exit.resume_pcs[0];
        let resume_pc = exit.resume_pcs[0];
        debug_assert_eq!(native_frame.header.pc, resume_pc);
        return NativeResultPair::side_exit(u64::from(resume_pc));
    }

    // The outermost frame keeps the entry's own bindings; every spliced frame
    // carries the ones its call would have established.
    let entry_this;
    let entry_self;
    match unsafe { ctx.native_frame.as_ref() } {
        Some(frame) => {
            entry_this = frame.this_value();
            entry_self = frame.self_value();
        }
        None => return compiled_fatal(ctx, VmError::InvalidOperand),
    }
    let mut frames = Vec::with_capacity(state.frames.len());
    for (depth, frame) in state.frames.iter().enumerate() {
        let registers = frame
            .slots
            .iter()
            .map(|slot| slot.repr.reconstitute(slot_raw(slot.location)))
            .collect::<Vec<_>>();
        let (return_register, this, closure) = match frame.entry {
            None => (0, entry_this, entry_self),
            Some(entry) => (
                entry.return_register,
                entry.this.repr.reconstitute(slot_raw(entry.this.location)),
                otter_vm::Value::undefined(),
            ),
        };
        frames.push(otter_vm::jit::JitDeoptFrame {
            callee_fid: frame.function_id,
            callee_pc: exit.resume_pcs[depth],
            return_register,
            this,
            closure,
            registers,
        });
    }

    // SAFETY: the live `JitCtx` reentry contract.
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    let stack = unsafe { &mut *ctx.activation().stack_ptr() };
    let context = unsafe { &*ctx.activation().context_ptr() };
    let Some(native) = (unsafe { ctx.native_frame.as_mut() }) else {
        return compiled_fatal(ctx, VmError::InvalidOperand);
    };
    match vm.jit_deopt_materialize_inline_frames(context, stack, native, &frames) {
        Ok(value) => NativeResultPair::success(value),
        Err(error) => compiled_error(ctx, error),
    }
}

/// Normalize one parked error at a compiled frame's canonical final
/// abrupt-completion boundary.
///
/// Runtime operations that return a status word park their [`VmError`] here.
/// This boundary consumes that state once, converts a
/// catchable failure into a pure exception value, and either selects a local
/// materialized handler or returns `Throw(exception)`. It never re-parks an
/// uncaught JavaScript exception for another compiled frame.
pub(crate) extern "C" fn jit_finish_error_stub(ctx: *mut JitCtx) -> NativeResultPair {
    // SAFETY: the live `JitCtx` reentry contract; the error slot is initialized
    // for the compiled entry's dynamic extent.
    let ctx = unsafe { &mut *ctx };
    let Some(error) = (unsafe { (*ctx.error).take() }) else {
        return compiled_fatal(ctx, VmError::InvalidOperand);
    };
    let exception = match ctx
        .runtime_call()
        .and_then(|mut runtime| runtime.take_js_throw(error))
    {
        Ok(exception) => exception,
        Err(fatal) => return compiled_fatal(ctx, fatal),
    };
    route_throw_value(ctx, exception)
}

/// Complete one structured-exception opcode, including TDZ `ReferenceError`
/// materialization. Unlike ordinary status-word transitions this returns the
/// full compiled-entry pair: a committed handler mutation may continue, resume
/// at a dynamic logical PC, return a value, or propagate a pure JavaScript
/// exception. Only structural failures remain parked behind Fatal.
pub(crate) extern "C" fn jit_exception_op_stub(
    ctx: *mut JitCtx,
    opcode: u64,
    arg0: u64,
    arg1: u64,
    arg2: u64,
) -> NativeResultPair {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let outcome = match ctx.try_runtime_call() {
        Ok(Some(mut runtime)) => runtime.exception_op(opcode as u8, arg0, arg1, arg2),
        Ok(None) => {
            return match ctx.active_frame() {
                Ok(frame) => NativeResultPair::side_exit(u64::from(frame.pc())),
                Err(error) => compiled_fatal(ctx, error),
            };
        }
        Err(error) => Err(error),
    };
    match outcome {
        Ok(JitExceptionOutcome::Continue) => NativeResultPair::continue_generated(),
        Ok(JitExceptionOutcome::Resume(pc)) => NativeResultPair::side_exit(u64::from(pc)),
        Ok(JitExceptionOutcome::Return(value)) => NativeResultPair::success(value),
        Ok(JitExceptionOutcome::Throw(exception)) => NativeResultPair::throw_value(exception),
        Err(error) => match ctx
            .runtime_call()
            .and_then(|mut runtime| runtime.take_js_throw(error))
        {
            Ok(exception) => NativeResultPair::throw_value(exception),
            Err(fatal) => {
                park_jit_error(ctx, fatal);
                NativeResultPair::fatal_internal()
            }
        },
    }
}

/// Complete one iterator-lifecycle opcode. `Success` means the VM committed
/// the opcode, `SideExit` is pre-effect, and `Throw` reports a parked error.
pub(crate) extern "C" fn jit_iterator_op_stub(
    ctx: *mut JitCtx,
    opcode: u64,
    arg0: u64,
    arg1: u64,
    arg2: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let frame_index = match ctx.materialized_frame_index() {
        Ok(index) => index,
        Err(_) => {
            let result = match ctx.try_runtime_call() {
                Ok(Some(mut runtime)) => {
                    runtime.iterator_op(opcode as u8, arg0 as u16, arg1 as u16, arg2 as u16)
                }
                Ok(None) => return NativeResultStatus::SideExit as u64,
                Err(err) => Err(err),
            };
            return match result {
                Ok(IteratorRuntimeOutcome::Completed) => NativeResultStatus::Success as u64,
                Ok(IteratorRuntimeOutcome::Bail) => NativeResultStatus::SideExit as u64,
                Err(err) => {
                    park_jit_error(ctx, err);
                    NativeResultStatus::Throw as u64
                }
            };
        }
    };
    let Some(activation) = ctx.checked_activation() else {
        return NativeResultStatus::SideExit as u64;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_iterator_op(context, stack, frame_index, opcode as u8, arg0, arg1, arg2) {
        Ok(()) => NativeResultStatus::Success as u64,
        Err(err) => {
            park_jit_error(ctx, err);
            NativeResultStatus::Throw as u64
        }
    }
}

/// Complete one static intrinsic-call opcode (`ArrayBufferCall`,
/// `SharedArrayBufferCall`, `BigIntCall`, `DataViewCall`). Returns `Success`,
/// pre-effect `SideExit`, or parked `Throw`.
pub(crate) extern "C" fn jit_static_call_op_stub(
    ctx: *mut JitCtx,
    opcode: u64,
    packed_head: u64,
    method: u64,
    packed_args: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let frame_index = match ctx.materialized_frame_index() {
        Ok(index) => index,
        Err(_) => return NativeResultStatus::SideExit as u64,
    };
    let Some(activation) = ctx.checked_activation() else {
        return NativeResultStatus::SideExit as u64;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_static_call_op(
        context,
        stack,
        frame_index,
        opcode as u8,
        packed_head,
        method,
        packed_args,
    ) {
        Ok(()) => NativeResultStatus::Success as u64,
        Err(err) => {
            park_jit_error(ctx, err);
            NativeResultStatus::Throw as u64
        }
    }
}

/// Complete one spread/call-family opcode. A synchronous callee throw is
/// parked once for the compiled frame's canonical final error boundary; the
/// committed call is never replayed.
pub(crate) extern "C" fn jit_spread_call_op_stub(
    ctx: *mut JitCtx,
    opcode: u64,
    arg0: u64,
    arg1: u64,
    arg2: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let frame_index = match ctx.materialized_frame_index() {
        Ok(index) => index,
        Err(_) => return NativeResultStatus::SideExit as u64,
    };
    let Some(activation) = ctx.checked_activation() else {
        return NativeResultStatus::SideExit as u64;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_spread_call_op(context, stack, frame_index, opcode as u8, arg0, arg1, arg2)
    {
        Ok(()) => NativeResultStatus::Success as u64,
        Err(err) => {
            park_jit_error(ctx, err);
            NativeResultStatus::Throw as u64
        }
    }
}

/// Complete one class/value-family opcode. Dynamic evaluation, function
/// construction, and numeric coercion may synchronously throw from JavaScript.
pub(crate) extern "C" fn jit_class_value_op_stub(
    ctx: *mut JitCtx,
    opcode: u64,
    arg0: u64,
    arg1: u64,
    arg2: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    if opcode == Op::MakeClass as u64 {
        let lane = |packed: u64, index: usize| ((packed >> (index * 16)) & 0xffff) as u16;
        let operation = ClassRuntimeOp::MakeClass {
            destination: lane(arg0, 0),
            constructor: lane(arg0, 1),
            prototype: lane(arg0, 2),
            statics: lane(arg0, 3),
            parent: Some(arg1 as u16),
        };
        match ctx
            .runtime_call()
            .and_then(|mut call| call.class_op(operation))
        {
            Ok(()) => return NativeResultStatus::Success as u64,
            Err(err) => {
                park_jit_error(ctx, err);
                return NativeResultStatus::Throw as u64;
            }
        }
    }
    if opcode == Op::Eval as u64 {
        if ctx.materialized_frame_index().is_err() {
            return NativeResultStatus::SideExit as u64;
        }
        let result = ctx
            .runtime_call()
            .and_then(|mut call| call.eval_op(arg0, arg1, arg2));
        return match result {
            Ok(()) => NativeResultStatus::Success as u64,
            Err(err) => {
                park_jit_error(ctx, err);
                NativeResultStatus::Throw as u64
            }
        };
    }
    let frame_index = match ctx.materialized_frame_index() {
        Ok(index) => index,
        Err(_) => return NativeResultStatus::SideExit as u64,
    };
    let Some(activation) = ctx.checked_activation() else {
        return NativeResultStatus::SideExit as u64;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_class_value_op(context, stack, frame_index, opcode as u8, arg0, arg1, arg2)
    {
        Ok(()) => NativeResultStatus::Success as u64,
        Err(err) => {
            park_jit_error(ctx, err);
            NativeResultStatus::Throw as u64
        }
    }
}

/// Complete one synchronous module-family opcode. Promise-producing module
/// evaluation/import opcodes never call this stub and remain exact side exits.
pub(crate) extern "C" fn jit_module_op_stub(
    ctx: *mut JitCtx,
    opcode: u64,
    arg0: u64,
    arg1: u64,
    arg2: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let frame_index = match ctx.materialized_frame_index() {
        Ok(index) => index,
        Err(_) => return NativeResultStatus::SideExit as u64,
    };
    let Some(activation) = ctx.checked_activation() else {
        return NativeResultStatus::SideExit as u64;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_module_op(context, stack, frame_index, opcode as u8, arg0, arg1, arg2) {
        Ok(()) => NativeResultStatus::Success as u64,
        Err(err) => {
            park_jit_error(ctx, err);
            NativeResultStatus::Throw as u64
        }
    }
}

/// Complete one variadic construction opcode (`ArrayConstruct`, `ArrayFrom`,
/// `ArrayOf`, `QueueMicrotask`). Returns committed `Success`, pre-effect
/// `SideExit`, or parked `Throw`.
pub(crate) extern "C" fn jit_variadic_op_stub(
    ctx: *mut JitCtx,
    opcode: u64,
    prefix: u64,
    count: u64,
    packed_args: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let frame_index = match ctx.materialized_frame_index() {
        Ok(index) => index,
        Err(_) => return NativeResultStatus::SideExit as u64,
    };
    let Some(activation) = ctx.checked_activation() else {
        return NativeResultStatus::SideExit as u64;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_variadic_op(
        context,
        stack,
        frame_index,
        opcode as u8,
        prefix,
        count,
        packed_args,
    ) {
        Ok(()) => NativeResultStatus::Success as u64,
        Err(err) => {
            park_jit_error(ctx, err);
            NativeResultStatus::Throw as u64
        }
    }
}

/// Complete one decoded class-construction operation through the canonical
/// stack-owned runtime boundary. An absent activation is the only pre-effect
/// side exit; a started operation either commits once or throws.
pub(crate) extern "C" fn jit_class_op_stub(
    ctx: *mut JitCtx,
    opcode: u64,
    arg0: u64,
    arg1: u64,
    arg2: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let operation = match opcode as u8 {
        value if value == otter_bytecode::Op::ClassCheck as u8 => ClassRuntimeOp::Check {
            register: arg0 as u16,
            kind: arg1 as u32,
        },
        value if value == otter_bytecode::Op::SetFunctionName as u8 => {
            ClassRuntimeOp::SetFunctionName {
                function: arg0 as u16,
                key: arg2 as u16,
                prefix_index: arg1 as u32,
            }
        }
        _ => {
            park_jit_error(ctx, VmError::InvalidOperand);
            return NativeResultStatus::Throw as u64;
        }
    };
    let result = match ctx.try_runtime_call() {
        Ok(Some(mut runtime)) => runtime.class_op(operation),
        Ok(None) => return NativeResultStatus::SideExit as u64,
        Err(err) => Err(err),
    };
    match result {
        Ok(()) => NativeResultStatus::Success as u64,
        Err(err) => {
            park_jit_error(ctx, err);
            NativeResultStatus::Throw as u64
        }
    }
}

/// Complete one structural object opcode (`ForInKeys`, `CopyDataProperties`).
/// Returns committed `Success`, pre-effect `SideExit`, or parked `Throw`.
pub(crate) extern "C" fn jit_structural_op_stub(
    ctx: *mut JitCtx,
    opcode: u64,
    arg0: u64,
    arg1: u64,
    arg2: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let frame_index = match ctx.materialized_frame_index() {
        Ok(index) => index,
        Err(_) => return NativeResultStatus::SideExit as u64,
    };
    let Some(activation) = ctx.checked_activation() else {
        return NativeResultStatus::SideExit as u64;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_structural_op(context, stack, frame_index, opcode as u8, arg0, arg1, arg2)
    {
        Ok(()) => NativeResultStatus::Success as u64,
        Err(err) => {
            park_jit_error(ctx, err);
            NativeResultStatus::Throw as u64
        }
    }
}

/// Complete one allocating-construction opcode (`CollectRest`, `ArrayPush`). Returns committed `Success`, pre-effect
/// `SideExit`, or parked `Throw`.
pub(crate) extern "C" fn jit_construct_op_stub(
    ctx: *mut JitCtx,
    opcode: u64,
    arg0: u64,
    arg1: u64,
    arg2: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let materialized_frame = ctx.materialized_frame_index();
    if materialized_frame.is_err() && opcode as u8 == otter_bytecode::Op::ArrayPush as u8 {
        let result = match ctx.try_runtime_call() {
            Ok(Some(mut runtime)) => runtime.spread_array_push(arg0 as u16, arg1 as u16),
            Ok(None) => return NativeResultStatus::SideExit as u64,
            Err(err) => Err(err),
        };
        return match result {
            Ok(()) => NativeResultStatus::Success as u64,
            Err(err) => {
                park_jit_error(ctx, err);
                NativeResultStatus::Throw as u64
            }
        };
    }
    let frame_index = match materialized_frame {
        Ok(index) => index,
        Err(_) => return NativeResultStatus::SideExit as u64,
    };
    let Some(activation) = ctx.checked_activation() else {
        return NativeResultStatus::SideExit as u64;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_construct_op(context, stack, frame_index, opcode as u8, arg0, arg1, arg2) {
        Ok(()) => NativeResultStatus::Success as u64,
        Err(err) => {
            park_jit_error(ctx, err);
            NativeResultStatus::Throw as u64
        }
    }
}

/// Complete one decoded static value load through the canonical stack-owned
/// runtime boundary. Allocation keeps the published source window rooted.
pub(crate) extern "C" fn jit_value_load_op_stub(
    ctx: *mut JitCtx,
    opcode: u64,
    arg0: u64,
    arg1: u64,
    arg2: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let operation = match opcode as u8 {
        value if value == otter_bytecode::Op::MathLoad as u8 => ValueLoadRuntimeOp::Math {
            dst: arg0 as u16,
            name_index: arg1 as u32,
        },
        value if value == otter_bytecode::Op::SymbolLoad as u8 => ValueLoadRuntimeOp::Symbol {
            dst: arg0 as u16,
            name_index: arg1 as u32,
        },
        value if value == otter_bytecode::Op::TemporalLoad as u8 => ValueLoadRuntimeOp::Temporal {
            dst: arg0 as u16,
            name_index: arg1 as u32,
        },
        value if value == otter_bytecode::Op::LoadBigInt as u8 => ValueLoadRuntimeOp::BigInt {
            dst: arg0 as u16,
            constant_index: arg1 as u32,
        },
        value if value == otter_bytecode::Op::GetStringIndex as u8 => {
            ValueLoadRuntimeOp::StringIndex {
                dst: arg0 as u16,
                receiver: arg1 as u16,
                index: arg2 as u16,
            }
        }
        _ => {
            park_jit_error(ctx, VmError::InvalidOperand);
            return NativeResultStatus::Throw as u64;
        }
    };
    let result = match ctx.try_runtime_call() {
        Ok(Some(mut runtime)) => runtime.value_load_op(operation),
        Ok(None) => return NativeResultStatus::SideExit as u64,
        Err(err) => Err(err),
    };
    match result {
        Ok(()) => NativeResultStatus::Success as u64,
        Err(err) => {
            park_jit_error(ctx, err);
            NativeResultStatus::Throw as u64
        }
    }
}

/// Complete one private-member opcode (`PrivateGet`, `PrivateSet`,
/// `PrivateBrandCheck`). Returns committed `Success`, pre-effect `SideExit`, or
/// parked `Throw`.
pub(crate) extern "C" fn jit_private_op_stub(
    ctx: *mut JitCtx,
    opcode: u64,
    arg0: u64,
    arg1: u64,
    arg2: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let frame_index = match ctx.materialized_frame_index() {
        Ok(index) => index,
        Err(_) => return NativeResultStatus::SideExit as u64,
    };
    let Some(activation) = ctx.checked_activation() else {
        return NativeResultStatus::SideExit as u64;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_private_op(context, stack, frame_index, opcode as u8, arg0, arg1, arg2) {
        Ok(()) => NativeResultStatus::Success as u64,
        Err(err) => {
            park_jit_error(ctx, err);
            NativeResultStatus::Throw as u64
        }
    }
}

/// Complete one `super` property opcode (`LoadSuperProperty`,
/// `LoadSuperElement`, `SetSuperProperty`, `SetSuperElement`). Returns
/// committed `Success`, pre-effect `SideExit`, or parked `Throw`.
pub(crate) extern "C" fn jit_super_op_stub(
    ctx: *mut JitCtx,
    opcode: u64,
    arg0: u64,
    arg1: u64,
    arg2: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let frame_index = match ctx.materialized_frame_index() {
        Ok(index) => index,
        Err(_) => return NativeResultStatus::SideExit as u64,
    };
    let Some(activation) = ctx.checked_activation() else {
        return NativeResultStatus::SideExit as u64;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_super_op(context, stack, frame_index, opcode as u8, arg0, arg1, arg2) {
        Ok(()) => NativeResultStatus::Success as u64,
        Err(err) => {
            park_jit_error(ctx, err);
            NativeResultStatus::Throw as u64
        }
    }
}

/// Complete the exact published scalar operation from two boxed values.
///
/// Function/PC identity selects the typed operation. No opcode, destination,
/// register index, materialized frame, miss, or replay state crosses this ABI.
pub(crate) extern "C" fn jit_scalar_value_stub(
    ctx: *mut JitCtx,
    value0_bits: u64,
    value1_bits: u64,
) -> NativeResultPair {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let result = ctx
        .runtime_call()
        .map_err(CommittedValueError::Fatal)
        .and_then(|mut runtime| {
            runtime.scalar_values(Value::from_bits(value0_bits), Value::from_bits(value1_bits))
        });
    committed_value_result(ctx, result)
}

/// Complete the exact published schema-owned binding access from two boxed
/// values. Function/PC identity selects the semantic operation and immutable
/// name/index/flag operands; this boundary never returns a replay request.
pub(crate) extern "C" fn jit_binding_value_stub(
    ctx: *mut JitCtx,
    value0_bits: u64,
    value1_bits: u64,
) -> NativeResultPair {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let result = ctx
        .runtime_call()
        .map_err(CommittedValueError::Fatal)
        .and_then(|mut runtime| {
            runtime.binding_values(Value::from_bits(value0_bits), Value::from_bits(value1_bits))
        });
    committed_value_result(ctx, result)
}

/// Complete the exact published schema-owned global declaration or
/// initialization from two boxed values. Declarations remain a separate
/// semantic family even though they share the committed physical ABI.
pub(crate) extern "C" fn jit_global_declaration_value_stub(
    ctx: *mut JitCtx,
    value0_bits: u64,
    value1_bits: u64,
) -> NativeResultPair {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let result = ctx
        .runtime_call()
        .map_err(CommittedValueError::Fatal)
        .and_then(|mut runtime| {
            runtime.global_declaration_values(
                Value::from_bits(value0_bits),
                Value::from_bits(value1_bits),
            )
        });
    committed_value_result(ctx, result)
}

/// Complete one object-property `delete` opcode (`DeleteProperty` or
/// `DeleteElement`). Binding deletion uses [`jit_binding_value_stub`].
pub(crate) extern "C" fn jit_delete_op_stub(
    ctx: *mut JitCtx,
    opcode: u64,
    arg0: u64,
    arg1: u64,
    arg2: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    if ctx.materialized_frame_index().is_err() {
        return NativeResultStatus::SideExit as u64;
    }
    let result = ctx
        .runtime_call()
        .and_then(|mut runtime| runtime.delete_op(opcode as u8, arg0, arg1, arg2));
    match result {
        Ok(()) => NativeResultStatus::Success as u64,
        Err(err) => {
            park_jit_error(ctx, err);
            NativeResultStatus::Throw as u64
        }
    }
}

/// Complete the exact published object-protocol operation from two boxed
/// values. Proxy traps and `@@hasInstance` either commit one result or return
/// one rooted JavaScript exception; no materialized fallback can replay them.
pub(crate) extern "C" fn jit_object_protocol_value_stub(
    ctx: *mut JitCtx,
    value0_bits: u64,
    value1_bits: u64,
) -> NativeResultPair {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let result = ctx
        .runtime_call()
        .map_err(CommittedValueError::Fatal)
        .and_then(|mut runtime| {
            runtime.object_protocol_values(
                Value::from_bits(value0_bits),
                Value::from_bits(value1_bits),
            )
        });
    committed_value_result(ctx, result)
}

/// Complete one `Op::BindFunction`. `packed_meta` is
/// `dst | callee<<16 | this<<32 | argc<<48`; `packed_args` holds the bound-arg
/// registers. Returns committed `Success`, pre-effect `SideExit`, or parked
/// `Throw`.
pub(crate) extern "C" fn jit_bind_function_stub(
    ctx: *mut JitCtx,
    packed_meta: u64,
    packed_args: u64,
    _reserved0: u64,
    _reserved1: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let frame_index = match ctx.materialized_frame_index() {
        Ok(index) => index,
        Err(_) => return NativeResultStatus::SideExit as u64,
    };
    let Some(activation) = ctx.checked_activation() else {
        return NativeResultStatus::SideExit as u64;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_bind_function(context, stack, frame_index, packed_meta, packed_args) {
        Ok(()) => NativeResultStatus::Success as u64,
        Err(err) => {
            park_jit_error(ctx, err);
            NativeResultStatus::Throw as u64
        }
    }
}

/// Complete one full fixed-arity `Op::New` or `Op::SuperConstruct` in the VM.
/// `super_construct` selects the entered frame's immutable `new.target`.
/// `Success` writes the destination, `Throw` parks an abrupt completion, and
/// `SideExit` reports a non-constructor or absent activation before effects.
pub(crate) extern "C" fn jit_construct_stub(
    ctx: *mut JitCtx,
    dst: u64,
    callee: u64,
    argc_and_mode: u64,
    packed_args: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let regs = match ctx.register_base() {
        Ok(regs) => regs,
        Err(err) => {
            park_jit_error(ctx, err);
            return NativeResultStatus::Throw as u64;
        }
    };
    let (caller_function_id, call_pc) = match ctx.active_frame() {
        Ok(frame) => (frame.function_id(), frame.pc()),
        Err(err) => {
            park_jit_error(ctx, err);
            return NativeResultStatus::Throw as u64;
        }
    };
    let super_construct = argc_and_mode >> 63 != 0;
    let argc = argc_and_mode & u64::from(u16::MAX);
    let inherited_new_target = if super_construct {
        match ctx.active_frame() {
            Ok(frame) => Some(frame.new_target_value()),
            Err(err) => {
                park_jit_error(ctx, err);
                return NativeResultStatus::Throw as u64;
            }
        }
    } else {
        None
    };
    let Some(activation) = ctx.checked_activation() else {
        return NativeResultStatus::SideExit as u64;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    let mut inline_args = [0u16; crate::entry::PACKED_REGISTER_LANES];
    let args = crate::entry::decode_register_list(argc as usize, packed_args, &mut inline_args);
    match vm.jit_runtime_construct_in_place(
        context,
        stack,
        dst as u16,
        callee as u16,
        args,
        regs,
        inherited_new_target,
        caller_function_id,
        call_pc,
    ) {
        Ok(true) => NativeResultStatus::Success as u64,
        Ok(false) => NativeResultStatus::SideExit as u64,
        Err(err) => {
            park_jit_error(ctx, err);
            NativeResultStatus::Throw as u64
        }
    }
}

/// Perform the observable receiver-preparation half of a generated base
/// construct. The constructor body remains unstarted; successful return hands
/// generated linkage one freshly allocated, prototype-linked receiver.
pub(crate) extern "C" fn jit_prepare_base_construct_stub(
    ctx: *mut JitCtx,
    callee_bits: u64,
    new_target_bits: u64,
    function_id: u64,
) -> NativeResultPair {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let Some(activation) = ctx.checked_activation() else {
        park_jit_error(ctx, VmError::InvalidOperand);
        return NativeResultPair::fatal_internal();
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    let Ok(function_id) = u32::try_from(function_id) else {
        park_jit_error(ctx, VmError::InvalidOperand);
        return NativeResultPair::fatal_internal();
    };
    let result = vm.jit_prepare_base_construct_receiver(
        stack,
        context,
        function_id,
        otter_vm::Value::from_bits(callee_bits),
        otter_vm::Value::from_bits(new_target_bits),
    );
    committed_vm_result(ctx, result)
}

/// Allocate a base-constructor receiver without re-entering JavaScript when
/// `new.target` exposes an exact own data prototype. A pre-effect miss asks
/// generated linkage to call the observable preparation sibling.
pub(crate) extern "C" fn jit_try_prepare_base_construct_stub(
    ctx: *mut JitCtx,
    callee_bits: u64,
    new_target_bits: u64,
    function_id: u64,
    planned_allocation: u64,
) -> NativeResultPair {
    // SAFETY: the live `JitCtx` allocation contract publishes the caller's
    // complete tagged window through its active native frame.
    let ctx = unsafe { &mut *ctx };
    let Some(activation) = ctx.checked_activation() else {
        park_jit_error(ctx, VmError::InvalidOperand);
        return NativeResultPair::fatal_internal();
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    let before_page = ctx.receiver_alloc.page_header;
    let before_cycles = vm.jit_gc_cycle_counts();
    if planned_allocation != 0 && !ctx.runtime_stats.is_null() {
        // SAFETY: `enter_compiled` publishes the interpreter-owned counter
        // record for the complete JIT context lifetime.
        let stats = unsafe { &mut *ctx.runtime_stats };
        stats.receiver_alloc_cold_transitions =
            stats.receiver_alloc_cold_transitions.saturating_add(1);
        stats.receiver_alloc_rust_transitions =
            stats.receiver_alloc_rust_transitions.saturating_add(1);
    }
    let Ok(function_id) = u32::try_from(function_id) else {
        return NativeResultPair::miss();
    };
    let result = vm.jit_try_prepare_base_construct_receiver(
        context,
        function_id,
        otter_vm::Value::from_bits(callee_bits),
        otter_vm::Value::from_bits(new_target_bits),
    );
    ctx.receiver_alloc = vm.jit_receiver_allocation_window();
    if planned_allocation != 0 && !ctx.runtime_stats.is_null() {
        // SAFETY: same stable counter record as above.
        let stats = unsafe { &mut *ctx.runtime_stats };
        if vm.jit_gc_cycle_counts() != before_cycles {
            stats.receiver_alloc_gc_transitions =
                stats.receiver_alloc_gc_transitions.saturating_add(1);
        }
        if ctx.receiver_alloc.page_header != before_page {
            stats.receiver_alloc_refills = stats.receiver_alloc_refills.saturating_add(1);
        }
        if matches!(result, Err(VmError::OutOfMemory { .. })) {
            stats.receiver_alloc_oom = stats.receiver_alloc_oom.saturating_add(1);
        }
    }
    match result {
        Ok(Some(receiver)) => NativeResultPair::success_bits(receiver.to_bits()),
        Ok(None) => NativeResultPair::miss(),
        Err(error) => {
            park_jit_error(ctx, error);
            NativeResultPair::throw_pending()
        }
    }
}

/// Apply derived-constructor return validation and park any abrupt completion.
pub(crate) extern "C" fn jit_derived_construct_result_stub(
    ctx: *mut JitCtx,
    result_bits: u64,
    this_bits: u64,
) -> NativeResultPair {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let Some(activation) = ctx.checked_activation() else {
        park_jit_error(ctx, VmError::InvalidOperand);
        return NativeResultPair::fatal_internal();
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    vm.record_jit_derived_construct_result_transition();
    let result = vm.jit_derived_construct_result(
        otter_vm::Value::from_bits(result_bits),
        otter_vm::Value::from_bits(this_bits),
    );
    committed_vm_result(ctx, result)
}

/// Read the live superclass from an exact class wrapper; hole means guard miss.
pub(crate) extern "C" fn jit_class_super_constructor_stub(
    ctx: *mut JitCtx,
    value_bits: u64,
    _reserved0: u64,
    _reserved1: u64,
    _reserved2: u64,
) -> u64 {
    // SAFETY: the leaf receives the current generated activation.
    if let Some(activation) = unsafe { &mut *ctx }.checked_activation() {
        let vm = unsafe { &mut *activation.vm_ptr() };
        vm.record_jit_class_super_resolution_transition();
    }
    // SAFETY: the live `JitCtx` entry contract.
    let ctx = unsafe { &mut *ctx };
    let Some(activation) = ctx.checked_activation() else {
        return otter_vm::Value::hole().to_bits();
    };
    let vm = unsafe { &*activation.vm_ptr() };
    vm.jit_class_super_constructor(otter_vm::Value::from_bits(value_bits))
        .to_bits()
}

/// Copy the compiler-created dense spread array into an unpublished generated
/// callee frame. `0` is success and `1` is an exact pre-entry guard miss.
pub(crate) extern "C" fn jit_copy_spread_arguments_stub(
    ctx: *mut JitCtx,
    arguments_bits: u64,
    frame: *mut otter_vm::native_abi::NativeFrame,
    parameter_count: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` entry contract.
    let ctx = unsafe { &mut *ctx };
    let Some(activation) = ctx.checked_activation() else {
        return 1;
    };
    let Ok(parameter_count) = u16::try_from(parameter_count) else {
        return 1;
    };
    let vm = unsafe { &*activation.vm_ptr() };
    // SAFETY: generated linkage owns and keeps the fully initialized but not
    // yet published stack frame live across this leaf call.
    u64::from(!unsafe {
        vm.jit_copy_spread_arguments(
            otter_vm::Value::from_bits(arguments_bits),
            frame,
            parameter_count,
        )
    })
}

/// Build a generated callee's stack-owned upvalue spine before publication.
/// Returns `Success`, pre-publication `SideExit`, or pending `Throw`.
pub(crate) extern "C" fn jit_initialize_upvalues_stub(
    ctx: *mut JitCtx,
    frame: u64,
    own: u64,
    inherited: u64,
) -> NativeResultPair {
    let ctx = unsafe { &mut *ctx };
    let Some(activation) = ctx.checked_activation() else {
        return NativeResultPair::miss();
    };
    let (Ok(own), Ok(inherited)) = (u16::try_from(own), u16::try_from(inherited)) else {
        return NativeResultPair::miss();
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &*activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    let frame = frame as *mut otter_vm::native_abi::NativeFrame;
    match unsafe { vm.jit_initialize_generated_upvalues(stack, context, frame, own, inherited) } {
        Ok(true) => NativeResultPair::success_bits(0),
        Ok(false) => NativeResultPair::miss(),
        Err(error) => {
            park_jit_error(ctx, error);
            NativeResultPair::throw_pending()
        }
    }
}

/// Complete one full loose-equality opcode in the VM. Returns `Success`,
/// coercion `Throw`, or an absent-activation `SideExit` before effects.
pub(crate) extern "C" fn jit_loose_eq_stub(
    ctx: *mut JitCtx,
    dst: u64,
    lhs: u64,
    rhs: u64,
    negate: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let regs = match ctx.register_base() {
        Ok(regs) => regs,
        Err(err) => {
            park_jit_error(ctx, err);
            return NativeResultStatus::Throw as u64;
        }
    };
    let Some(activation) = ctx.checked_activation() else {
        return NativeResultStatus::SideExit as u64;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_loose_equal_in_place(
        stack,
        context,
        dst as u16,
        lhs as u16,
        rhs as u16,
        negate != 0,
        regs,
    ) {
        Ok(()) => NativeResultStatus::Success as u64,
        Err(err) => {
            park_jit_error(ctx, err);
            NativeResultStatus::Throw as u64
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AbiUnaryCoercion {
    ToPrimitive { hint_index: u32 },
    ToNumeric,
}

fn decode_unary_coercion(numeric: u64, hint_index: u64) -> Result<AbiUnaryCoercion, VmError> {
    match numeric {
        0 => Ok(AbiUnaryCoercion::ToPrimitive {
            hint_index: u32::try_from(hint_index).map_err(|_| VmError::InvalidOperand)?,
        }),
        1 => Ok(AbiUnaryCoercion::ToNumeric),
        _ => Err(VmError::InvalidOperand),
    }
}

fn complete_unary_coercion(
    ctx: &mut JitCtx,
    dst: u16,
    src: u16,
    request: AbiUnaryCoercion,
) -> Result<(), VmError> {
    let mut runtime = ctx.runtime_call()?;
    match request {
        AbiUnaryCoercion::ToNumeric => runtime.coerce_unary(dst, src, UnaryCoercionOp::ToNumeric),
        AbiUnaryCoercion::ToPrimitive { hint_index } => {
            runtime.coerce_unary_hint(dst, src, hint_index)
        }
    }
}

/// Complete one `ToPrimitive`/`ToNumeric` opcode in the VM. Returns `Success`
/// after writing the destination or `Throw` after decoding/coercion fails. A
/// published canonical activation is part of the operation contract.
pub(crate) extern "C" fn jit_coerce_unary_stub(
    ctx: *mut JitCtx,
    dst: u64,
    src: u64,
    numeric: u64,
    hint_index: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let result = (|| {
        complete_unary_coercion(
            ctx,
            decode_register(dst)?,
            decode_register(src)?,
            decode_unary_coercion(numeric, hint_index)?,
        )
    })();
    match result {
        Ok(()) => NativeResultStatus::Success as u64,
        Err(err) => {
            park_jit_error(ctx, err);
            NativeResultStatus::Throw as u64
        }
    }
}

fn complete_numeric_op(
    ctx: &mut JitCtx,
    dst: u16,
    lhs: u16,
    operation: NumericRuntimeOp,
) -> Result<(), VmError> {
    ctx.runtime_call()?.numeric(dst, lhs, operation)
}

/// Complete one numeric-family opcode in the VM. Returns `Success` after
/// committing the destination or `Throw` after decoding/execution fails. This path has no
/// isolate-less/ActivationStack bailout mode: a published
/// [`NativeFrame`](otter_vm::native_abi::NativeFrame) and VM activation are
/// part of the runtime-op contract.
pub(crate) extern "C" fn jit_numeric_op_stub(
    ctx: *mut JitCtx,
    dst: u64,
    lhs: u64,
    rhs_or_delta: u64,
    opcode: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let decoded = (|| {
        Ok((
            decode_register(dst)?,
            decode_register(lhs)?,
            NumericRuntimeOp::decode_abi(opcode, rhs_or_delta)?,
        ))
    })();
    let (dst, lhs, operation) = match decoded {
        Ok(decoded) => decoded,
        Err(err) => {
            park_jit_error(ctx, err);
            return NativeResultStatus::Throw as u64;
        }
    };
    match complete_numeric_op(ctx, dst, lhs, operation) {
        Ok(()) => NativeResultStatus::Success as u64,
        Err(err) => {
            park_jit_error(ctx, err);
            NativeResultStatus::Throw as u64
        }
    }
}

/// Runtime stub: build a `MakeFunction` closure from compiled code. Returns
/// `Success` or parks the construction error and returns `Throw`.
pub(crate) extern "C" fn jit_make_fn_stub(ctx: *mut JitCtx, dst: u64, idx: u64) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let result = ctx.runtime_call().and_then(|mut runtime| {
        let function_id = runtime.function_id();
        runtime.make_function(function_id, dst as u16, idx as u32)
    });
    match result {
        Ok(()) => NativeResultStatus::Success as u64,
        Err(err) => {
            park_jit_error(ctx, err);
            NativeResultStatus::Throw as u64
        }
    }
}

/// Poll VM interrupts and runtime budget on compiled back-edges. Mirrors the
/// interpreter's cooperative checkpoint so watchdogs and budget rejection apply
/// equally after a loop tiers up through OSR.
pub(crate) extern "C" fn jit_backedge_poll_stub(ctx: *mut JitCtx) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let mut runtime = match ctx.try_runtime_call() {
        Ok(Some(runtime)) => runtime,
        Ok(None) => return NativeResultStatus::Success as u64,
        Err(err) => {
            park_jit_error(ctx, err);
            return NativeResultStatus::Throw as u64;
        }
    };
    let result = runtime.backedge_poll();
    match result {
        Ok(()) => NativeResultStatus::Success as u64,
        Err(err) => {
            park_jit_error(ctx, err);
            NativeResultStatus::Throw as u64
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_vm::{
        Value,
        native_abi::{NativeFrame, NativeFrameKind, NativeResultDomain, VmFrameHeader, VmThread},
    };

    fn with_frameless_ctx(test: impl FnOnce(&mut JitCtx, &mut Option<VmError>)) {
        let mut registers = [Value::undefined()];
        let mut frame = NativeFrame::new(
            VmFrameHeader {
                function_id: 7,
                pc: 0,
                register_count: 1,
                kind: NativeFrameKind::Baseline,
                flags: Default::default(),
            },
            registers.as_mut_ptr() as u64,
            Value::function(7),
            Value::undefined(),
        );
        frame.set_stack_registers();
        let mut thread = VmThread::empty();
        thread.current_frame = std::ptr::addr_of_mut!(frame) as u64;
        let mut error = None;
        let mut ctx = JitCtx {
            thread: std::ptr::addr_of_mut!(thread),
            native_frame: std::ptr::addr_of_mut!(frame),
            error: std::ptr::addr_of_mut!(error),
            activation_base: std::ptr::null_mut(),
            activation_top_ptr: std::ptr::null_mut(),
            activation_limit: 0,
            machine_roots_ptr: std::ptr::null_mut(),
            receiver_alloc: otter_vm::jit::JitMachineAllocationWindow::disabled(),
            runtime_stats: std::ptr::null_mut(),
            global_this_offset: std::ptr::null(),
            native_stack_limit: 0,
            generated_feedback_clean: 1,
        };
        test(&mut ctx, &mut error);
    }

    #[test]
    fn frameless_complex_transition_is_an_exact_pre_effect_bail() {
        with_frameless_ctx(|ctx, error| {
            let status = jit_iterator_op_stub(ctx, 0, 0, 0, 0);
            assert_eq!(status, NativeResultStatus::SideExit as u64);
            assert!(error.is_none());
            assert!(matches!(
                ctx.materialized_frame_index(),
                Err(VmError::InvalidOperand)
            ));
        });
    }

    #[test]
    fn frameless_exception_bail_preserves_the_stamped_opcode_pc() {
        with_frameless_ctx(|ctx, error| {
            // SAFETY: the fixture owns the live native frame for this call.
            unsafe {
                (*ctx.native_frame).header.pc = 37;
            }
            let result = jit_exception_op_stub(ctx, 0, 0, 0, 0);
            assert_eq!(
                result.validate(NativeResultDomain::ExceptionTransition),
                Some(otter_vm::native_abi::NativeResultStatus::SideExit)
            );
            assert_eq!(result.payload_bits(), 37);
            assert!(error.is_none());
        });
    }

    #[test]
    fn stack_owned_machine_deopt_writes_window_and_returns_exact_bail() {
        use otter_vm::deopt::{
            DeoptExitDescriptor, DeoptExitId, DeoptFrame, DeoptLocation, DeoptRepr, DeoptRuntime,
            DeoptSlot, DeoptTable, FrameState,
        };

        with_frameless_ctx(|ctx, error| {
            let value = Value::number_i32(41);
            let runtime = DeoptRuntime {
                table: DeoptTable::from_states(vec![FrameState {
                    frames: vec![DeoptFrame {
                        function_id: 7,
                        byte_pc: 91,
                        entry: None,
                        slots: vec![DeoptSlot {
                            location: DeoptLocation::Literal(value.to_bits()),
                            repr: DeoptRepr::Tagged,
                        }]
                        .into_boxed_slice(),
                    }]
                    .into_boxed_slice(),
                }]),
                exits: vec![DeoptExitDescriptor {
                    state: DeoptExitId(0),
                    resume_pcs: vec![37].into_boxed_slice(),
                }]
                .into_boxed_slice(),
                gpr_budget: 0,
            };
            // SAFETY: the fixture owns the one-slot published window.
            let window = unsafe { (*ctx.native_frame).register_base };
            let result = jit_deopt_writeback_stub(
                ctx,
                0,
                std::ptr::from_ref(&runtime),
                std::ptr::null(),
                0,
                window,
            );
            assert_eq!(
                result.validate(NativeResultDomain::Compiled),
                Some(otter_vm::native_abi::NativeResultStatus::SideExit)
            );
            assert_eq!(result.logical_pc(), Some(37));
            assert_eq!(unsafe { (*ctx.native_frame).header.pc }, 37);
            assert_eq!(unsafe { *(window as *const Value) }, value);
            assert!(error.is_none());
        });
    }

    #[test]
    fn frameless_final_error_boundary_preserves_structural_failure() {
        with_frameless_ctx(|ctx, error| {
            *error = Some(VmError::InvalidOperand);
            let result = jit_finish_error_stub(ctx);
            assert_eq!(
                result.validate(NativeResultDomain::Compiled),
                Some(otter_vm::native_abi::NativeResultStatus::Fatal)
            );
            assert!(matches!(error, Some(VmError::InvalidOperand)));
        });
    }

    #[test]
    fn invalid_committed_activation_is_fatal_not_javascript_throw() {
        with_frameless_ctx(|ctx, error| {
            let result = jit_scalar_value_stub(
                ctx,
                Value::undefined().to_bits(),
                Value::undefined().to_bits(),
            );
            assert_eq!(
                result.validate(NativeResultDomain::Committed),
                Some(otter_vm::native_abi::NativeResultStatus::Fatal)
            );
            assert!(matches!(error, Some(VmError::InvalidOperand)));
        });
    }

    #[test]
    fn unary_coercion_mode_is_decoded_once_at_the_abi_edge() {
        assert_eq!(
            decode_unary_coercion(1, u64::MAX).expect("ToNumeric mode"),
            AbiUnaryCoercion::ToNumeric
        );
        assert_eq!(
            decode_unary_coercion(0, 12).expect("ToPrimitive mode"),
            AbiUnaryCoercion::ToPrimitive { hint_index: 12 }
        );
    }

    #[test]
    fn unary_coercion_decoder_rejects_invalid_words() {
        assert!(matches!(
            decode_unary_coercion(2, 0),
            Err(VmError::InvalidOperand)
        ));
        assert!(matches!(
            decode_unary_coercion(0, u64::from(u32::MAX) + 1),
            Err(VmError::InvalidOperand)
        ));
        assert!(matches!(
            decode_register(u64::from(u16::MAX) + 1),
            Err(VmError::InvalidOperand)
        ));
    }
}
