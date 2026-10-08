//! Error-object opcode helpers.
//!
//! Error constructors are fixed-width bytecodes and should stay on the compact
//! executable operand path instead of the fallback operand-slice path.
//!
//! # Contents
//! - `new Error(message)` object allocation.
//! - Native error constructor allocation (`TypeError`, `RangeError`, ...).
//! - Native error constructor loading for identifier reads.
//! - One-shot native completion projection for direct semantic opcodes.
//!
//! # Invariants
//! - Error kind names are compiler-emitted string constants.
//! - Allocated instances come from the interpreter's `ErrorClassRegistry` so
//!   prototype identity matches `instanceof`.
//! - VM-raised errors use the top bytecode frame's `[[Realm]]`; linked
//!   function metadata carries only scalar realm ids, and the existing traced
//!   realm swap owns all moving-GC state.
//! - Message coercion and error allocation share one rooted value kernel.
//!   Construction stacks include published native frames and deopt owners once.
//! - Native failures synthesized inside a runtime turn retain that turn's
//!   activation stack as the allocation root set; their uncaught display is
//!   the materialized error's own rendering, class name included. Final owned
//!   execution failures restore their original detail/frames without
//!   materialization.
//! - Direct-source VM OOM retains its catchable RangeError projection. A
//!   completed execution failure bypasses projection, while explicitly authored
//!   native OOM projects once; failure of that build escapes unchanged.
//!
//! # See also
//! - [`crate::error_classes`]
//! - [`crate::executable`]

#[cfg(test)]
mod boundary_tests;
#[cfg(test)]
mod completion_tests;
#[cfg(test)]
mod semantic_completion_tests;
mod throwable;

use crate::activation_stack::ActivationStack;
use crate::native_abi::CommittedValueError;
use crate::rooting::RootScopeExt;

use crate::{
    ActiveFrameMut, ErrorKind, ExecutionContext, Frame, Interpreter, JsString, NativeError, Value,
    VmError, error_classes, object, read_register, symbol_dispatch, write_register,
};

impl Interpreter {
    pub(crate) fn run_new_error_regs(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        top_idx: usize,
        dst: u16,
        msg_reg: u16,
    ) -> Result<(), CommittedValueError> {
        let frame = &stack[top_idx];
        let value = *read_register(frame, msg_reg)
            .map_err(|error| CommittedValueError::Fatal(error.into()))?;
        let value = self.new_error_value(context, stack, ErrorKind::Error, value)?;
        let frame = &mut stack[top_idx];
        write_register(frame, dst, value)
            .map_err(|error| CommittedValueError::Fatal(error.into()))?;
        frame
            .advance_pc()
            .map_err(|error| CommittedValueError::Fatal(error.into()))?;
        Ok(())
    }

    pub(crate) fn run_new_builtin_error_regs(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        top_idx: usize,
        dst: u16,
        kind_idx: u32,
        msg_reg: u16,
    ) -> Result<(), CommittedValueError> {
        // Resolve the kind constant against the frame's own function so a
        // reentrant compiled cross-chunk entry decodes it against the owning
        // chunk, not the caller's constant pool.
        let kind_name = context
            .string_constant_str_for_function(stack[top_idx].function_id, kind_idx)
            .ok_or(VmError::InvalidOperand)
            .map_err(|error| CommittedValueError::Fatal(error.into()))?;
        let kind = ErrorKind::from_class_name(kind_name)
            .ok_or(VmError::InvalidOperand)
            .map_err(|error| CommittedValueError::Fatal(error.into()))?;
        let frame = &stack[top_idx];
        let value = *read_register(frame, msg_reg)
            .map_err(|error| CommittedValueError::Fatal(error.into()))?;
        let value = self.new_error_value(context, stack, kind, value)?;
        let frame = &mut stack[top_idx];
        write_register(frame, dst, value)
            .map_err(|error| CommittedValueError::Fatal(error.into()))?;
        frame
            .advance_pc()
            .map_err(|error| CommittedValueError::Fatal(error.into()))?;
        Ok(())
    }

    /// Construct an intrinsic Error from a current boxed message. The message
    /// stays rooted through observable ToString and the canonical allocation.
    pub(crate) fn new_error_value(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        kind: ErrorKind,
        mut message: Value,
    ) -> Result<Value, CommittedValueError> {
        let mut roots = otter_gc::RootScope::new(&mut self.gc_heap);
        // SAFETY: message predates the scope and stays stationary until return.
        unsafe {
            roots.add_value(&mut message);
        }
        let owned_message = self.coerce_error_message(stack, context, &message)?;
        let mut obj = self
            .make_error_instance_with_stack_roots(stack, kind, owned_message, &message)
            .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
        self.capture_error_stack_frames(context, &mut obj)
            .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
        Ok(Value::object(obj))
    }

    /// Record the construction-site JS call stack (top-of-stack first,
    /// bounded by `Error.stackTraceLimit`) onto a freshly built error
    /// instance for `Error.prototype.stack`. No-op when the limit is 0
    /// or the stack is empty.
    fn capture_error_stack_frames(
        &mut self,
        context: &ExecutionContext,
        obj: &mut object::JsObject,
    ) -> Result<(), VmError> {
        let limit = self.current_stack_trace_limit();
        if limit == 0 {
            return Ok(());
        }
        let draft = self.error_stack_draft(context, 0, limit);
        if !draft.is_empty() {
            object::set_error_stack(obj, self.gc_heap_mut(), &draft)?;
        }
        Ok(())
    }

    /// §20.5.1.1 step 3 — coerce the `message` argument through full
    /// §7.1.17 `ToString`. Returns `None` when the argument is
    /// `undefined` (the spec skips step 3 in that case, leaving
    /// `message` inherited from the prototype). Delegates the spec
    /// ladder to [`Interpreter::coerce_to_string`].
    fn coerce_error_message(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        value: &Value,
    ) -> Result<Option<String>, CommittedValueError> {
        if value.is_undefined() {
            return Ok(None);
        }
        Ok(Some(self.coerce_to_string(stack, context, value)?))
    }

    pub(crate) fn make_error_instance_with_stack_roots(
        &mut self,
        stack: &ActivationStack,
        kind: ErrorKind,
        message: Option<String>,
        message_value: &Value,
    ) -> Result<object::JsObject, VmError> {
        let message_gc_value = message
            .as_ref()
            .map(|text| JsString::from_str(text, self.gc_heap_mut()).map(Value::string))
            .transpose()?;
        let has_message = message_gc_value.is_some();
        let mut pending = [
            *message_value,
            message_gc_value.unwrap_or_else(Value::undefined),
        ];
        let mut obj = self.alloc_stack_rooted_object_with_pending_values(stack, &mut pending)?;
        // Fetch the prototype only after every allocation in this function:
        // the message-string and object allocs above can each trigger a major
        // GC that relocates the (old-gen) error prototype. The class registry
        // is a GC root whose handles the collector forwards in place, so it
        // always yields the live pointer — a handle captured earlier would be
        // stale and silently corrupt the new instance's `[[Prototype]]`.
        let proto = self.error_classes.prototype(kind);
        let mut message_gc_value = pending[1];
        let mut roots = otter_gc::RootScope::new(&mut self.gc_heap);
        // SAFETY: both locals are declared before `roots` and remain stable
        // until the scope is dropped below.
        unsafe {
            roots.add_object(&mut obj);
            roots.add_value(&mut message_gc_value);
        }
        if !object::set_prototype(&mut obj, &mut self.gc_heap, Some(proto))? {
            return Err(VmError::TypeError);
        }
        // §20.5.* — mark the `[[ErrorData]]` internal slot.
        object::set_error_data(&mut obj, &mut self.gc_heap)?;
        if has_message {
            // §20.5.1.1 step 4.c — `msgDesc` is `{ [[Value]]: msg,
            // [[Writable]]: true, [[Enumerable]]: false,
            // [[Configurable]]: true }`. Ordinary `set` would install
            // an enumerable slot; route through `define_own_property`
            // so reflective probes match the spec.
            object::define_own_property(
                obj,
                &mut self.gc_heap,
                "message",
                object::PropertyDescriptor::data(message_gc_value, true, false, true),
            )?;
        }
        drop(roots);
        Ok(obj)
    }

    pub(crate) fn run_load_builtin_error_reg(
        &self,
        context: &ExecutionContext,
        frame: &mut Frame,
        dst: u16,
        kind_idx: u32,
    ) -> Result<(), VmError> {
        let mut frame = ActiveFrameMut::from_frame(frame);
        self.run_load_builtin_error_active(context, &mut frame, dst, kind_idx)
    }

    /// Load one realm error constructor through a representation-neutral
    /// activation. No cold frame state or allocation is required.
    pub(crate) fn run_load_builtin_error_active(
        &self,
        context: &ExecutionContext,
        frame: &mut ActiveFrameMut<'_>,
        dst: u16,
        kind_idx: u32,
    ) -> Result<(), VmError> {
        let kind_name = context
            .string_constant_str(kind_idx)
            .ok_or(VmError::InvalidOperand)?;
        let kind = ErrorKind::from_class_name(kind_name).ok_or(VmError::InvalidOperand)?;
        let ctor = self.error_classes.constructor(kind);
        frame.write(dst, Value::object(ctor))?;
        frame.advance_pc()?;
        Ok(())
    }
    /// Build a freshly-allocated `TypeError` instance through the live frame
    /// stack. Mirrors the shape produced by
    /// [`Self::vm_error_to_throwable_with_stack_roots`] for `VmError::TypeError`
    /// but skips the `VmError` wrapping.
    pub(crate) fn make_type_error_with_stack_roots(
        &mut self,
        stack: &ActivationStack,
        message: &str,
    ) -> Result<Value, VmError> {
        let message_root = Value::undefined();
        let obj = self.make_error_instance_with_stack_roots(
            stack,
            ErrorKind::TypeError,
            Some(message.to_string()),
            &message_root,
        )?;
        Ok(Value::object(obj))
    }
}

/// Own `toString` for coded errors: `Name [CODE]: message`, degrading the
/// same way `Error.prototype.toString` does when parts are absent.
fn coded_error_to_string(
    ctx: &mut crate::NativeCtx<'_>,
    _args: &[Value],
) -> Result<Value, NativeError> {
    let this = *ctx.this_value();
    let read = |ctx: &crate::NativeCtx<'_>, key: &str| -> Option<String> {
        let object = this.as_object()?;
        crate::object::get(object, ctx.heap(), key)
            .and_then(|value| value.as_string(ctx.heap()))
            .map(|value| value.to_lossy_string(ctx.heap()))
    };
    let name = read(ctx, "name").unwrap_or_else(|| "Error".to_string());
    let code = read(ctx, "code");
    let message = read(ctx, "message").unwrap_or_default();
    let head = match code {
        Some(code) if !code.is_empty() => format!("{name} [{code}]"),
        _ => name,
    };
    let rendered = if message.is_empty() {
        head
    } else {
        format!("{head}: {message}")
    };
    ctx.scope(|mut scope| {
        let rendered = scope.string(&rendered)?;
        Ok(scope.finish(rendered))
    })
}

fn system_error_code(message: &str) -> &str {
    message
        .split_once(" returned ")
        .and_then(|(_, rest)| rest.split_once(' ').map(|(code, _)| code))
        .unwrap_or("UNKNOWN")
}

/// Walk a live frame stack top-down and build a snapshot the
/// runtime / CLI can render. Top-of-stack first.
///
/// # Source mapping
///
/// Each frame's `span` is the **original source byte range** for
/// the bytecode instruction the frame was about to execute. The
/// compiler populates [`otter_bytecode::Function::spans`] with
/// `(pc, span)` pairs in PC order, where `span` is the byte range
/// the lowered instruction came from in the source text.
///
/// The frame's PC may not have an exact entry in the spans table
/// (the compiler emits sparse `SpanEntry`s — one per source
/// statement / expression boundary, not one per instruction). We
/// therefore look up the predecessor entry: the largest `pc <=
/// frame.pc`. Falls back to the enclosing function's source span
/// when the table has no eligible predecessor (defensive — every
/// non-empty function body emits at least one span).
///
/// Each frame's `module` field is the per-function
/// [`otter_bytecode::Function::module_url`] when populated. The
/// linker stamps that field during module-fragment merging
/// (`function.module_url = "file:///path/to/other.ts"`), so
/// multi-module bytecode produces frames pointing at the original
/// source URL rather than the bytecode module's synthesized name
/// (`<entry>`).
/// How many frames, counting from the innermost, sit at or above the
/// topmost frame executing `callee`.
///
/// `Error.captureStackTrace(target, constructorOpt)` hides that function
/// and everything it called. The match is on the exact function object the
/// frame is running, because a bytecode function id means nothing outside
/// the program it was numbered in — an entry script and the modules it
/// requires are numbered separately, so comparing ids across them both
/// misses the real frame and matches unrelated ones.
pub(crate) fn frames_above_callee(stack: &ActivationStack, callee: Value) -> Option<usize> {
    stack
        .iter()
        .rev()
        .position(|frame| frame.self_value == callee)
        .map(|index| index + 1)
}

pub(crate) fn symbol_to_vm_error(
    interp: &crate::Interpreter,
    err: symbol_dispatch::SymbolError,
) -> VmError {
    match err {
        symbol_dispatch::SymbolError::UnknownMember(name) => {
            interp.err_unknown_intrinsic(format!("Symbol.{name}").into())
        }
        symbol_dispatch::SymbolError::BadArgument { .. } => VmError::TypeMismatch,
        symbol_dispatch::SymbolError::OutOfMemory {
            requested_bytes,
            heap_limit_bytes,
        } => VmError::OutOfMemory {
            requested_bytes,
            heap_limit_bytes,
        },
    }
}

pub(crate) fn native_to_vm_error(interp: &mut crate::Interpreter, err: NativeError) -> VmError {
    native_to_vm_error_with_stack(interp, &ActivationStack::new(), err)
}

/// Project one native failure into an exception value using the existing VM
/// error owner. An allocation that failed while synthesizing the native error
/// must escape as its exact OOM, rather than initiate another rejection build.
/// An original native OOM follows ordinary catchable RangeError projection;
/// deferred-result owners apply their own no-materialization OOM policy.
pub(crate) fn native_error_to_throwable_with_stack(
    interp: &mut Interpreter,
    stack: &ActivationStack,
    context: Option<&ExecutionContext>,
    error: NativeError,
) -> Result<Value, VmError> {
    if let NativeError::ExecutionFailure(failure) = error {
        return Err(restore_execution_failure(interp, failure));
    }
    let original_oom = matches!(error, NativeError::OutOfMemory { .. });
    let error = native_to_vm_error_with_stack(interp, stack, error);
    if error.is_fatal() || (!original_oom && matches!(error, VmError::OutOfMemory { .. })) {
        return Err(error);
    }
    interp.vm_error_to_throwable_with_stack_roots(context, stack, &error)
}

/// Finish a native operation at its live source/root boundary exactly once.
///
/// A boxed exception follows source handlers through `Uncaught`. An imported
/// execution failure or refusal while building that exception is already
/// terminal; its exact pending detail/frames must bypass source materialization.
pub(crate) fn native_error_to_committed_with_stack(
    interp: &mut Interpreter,
    stack: &ActivationStack,
    context: Option<&ExecutionContext>,
    error: NativeError,
) -> crate::CommittedValueError {
    match native_error_to_throwable_with_stack(interp, stack, context, error) {
        Ok(exception) => {
            interp.set_pending_uncaught_throw(exception);
            crate::CommittedValueError::JavaScript(VmError::Uncaught)
        }
        Err(error) => crate::CommittedValueError::Fatal(error),
    }
}

/// Restore the one owned execution failure without rematerializing a value.
fn restore_execution_failure(interp: &mut Interpreter, failure: crate::RunError) -> VmError {
    interp.pending_uncaught_throw = None;
    if !failure.is_fatal() {
        let _ = interp.take_error_detail();
        interp.clear_throw_provenance();
        return VmError::InvalidOperand;
    }
    *interp.pending_error_detail.borrow_mut() = failure.detail;
    interp.pending_throw_provenance = (!failure.frames.is_empty()).then_some(
        crate::native_stack_snapshot::ThrowProvenance::Frames(failure.frames),
    );
    failure.error
}

/// Convert a native failure while retaining the current activation stack as an
/// allocation root for the synthesized JavaScript error object.
pub(crate) fn native_to_vm_error_with_stack(
    interp: &mut crate::Interpreter,
    stack: &ActivationStack,
    err: NativeError,
) -> VmError {
    fn native_spec_error(
        interp: &mut crate::Interpreter,
        stack: &ActivationStack,
        kind: ErrorKind,
        message: String,
    ) -> VmError {
        match interp.make_error_instance_with_stack_roots(
            stack,
            kind,
            Some(message.clone()),
            &Value::undefined(),
        ) {
            Ok(obj) => {
                let thrown = Value::object(obj);
                interp.set_pending_uncaught_throw(thrown);
                // The uncaught display is the thrown value's own rendering,
                // `Name: message` first, as for every other escaping throw.
                let display = interp.render_thrown(&thrown);
                interp.err_uncaught(display.into())
            }
            Err(err) => err,
        }
    }

    match err {
        NativeError::Resource { error } => interp.err_resource(error),
        NativeError::ExecutionFailure(failure) => restore_execution_failure(interp, failure),
        NativeError::MissingReturn => VmError::MissingReturn,
        NativeError::InvalidOperand => VmError::InvalidOperand,
        NativeError::Thrown { name: _, message } => interp.err_uncaught(message.into()),
        NativeError::Error { message } => {
            native_spec_error(interp, stack, ErrorKind::Error, message)
        }
        NativeError::SpecError { kind, message } => native_spec_error(interp, stack, kind, message),
        NativeError::Coded {
            kind,
            code,
            message,
        } => interp.err_coded(kind, code, message),
        NativeError::Syscall {
            code,
            message,
            syscall,
            path,
            dest,
            errno,
        } => interp.err_syscall(crate::run_control::VmSyscallError {
            code,
            message,
            syscall,
            path,
            dest,
            errno,
        }),
        NativeError::TypeError { name, reason } => native_spec_error(
            interp,
            stack,
            ErrorKind::TypeError,
            format!("{name}: {reason}"),
        ),
        NativeError::SyntaxError { name, reason } => native_spec_error(
            interp,
            stack,
            ErrorKind::SyntaxError,
            format!("{name}: {reason}"),
        ),
        NativeError::RangeError { name, reason } => native_spec_error(
            interp,
            stack,
            ErrorKind::RangeError,
            format!("{name}: {reason}"),
        ),
        NativeError::URIError { name, reason } => native_spec_error(
            interp,
            stack,
            ErrorKind::URIError,
            format!("{name}: {reason}"),
        ),
        // Round-trips back to a ReferenceError-classed VmError so a TDZ
        // error raised behind a native boundary keeps its class.
        NativeError::ReferenceError { name, reason } => native_spec_error(
            interp,
            stack,
            ErrorKind::ReferenceError,
            format!("{name}: {reason}"),
        ),
        NativeError::Exit { code } => VmError::Exit { code },
        NativeError::Interrupted => VmError::Interrupted,
        NativeError::BudgetExceeded { reason } => interp.err_budget(reason.into()),
        NativeError::OutOfMemory {
            name: _,
            requested_bytes,
            heap_limit_bytes,
        } => VmError::OutOfMemory {
            requested_bytes,
            heap_limit_bytes,
        },
    }
}

impl crate::Interpreter {
    /// Render a thrown JS value for diagnostics, with a
    /// constructor-name fallback over the heap-only
    /// [`render_thrown_value`]: an error-shaped object whose class
    /// never set a `name` property (e.g. the test262 harness's
    /// `Test262Error`) renders under its constructor function's name
    /// instead of the generic `Error`.
    pub(crate) fn render_thrown(&self, value: &Value) -> String {
        let heap = &self.gc_heap;
        if let Some(obj) = value.as_object() {
            let has_real_name = crate::object::get(obj, heap, "name")
                .is_some_and(|name| !name.is_undefined() && !name.is_null());
            let message = crate::object::get(obj, heap, "message");
            // Node prints an uncaught Error's `stack`: the `Name: message`
            // header followed by its frames. An own `stack` data property
            // (user-assigned, or written by `Error.captureStackTrace`) wins;
            // otherwise the frames captured at construction render the same
            // string the `Error.prototype.stack` getter would. Anything
            // without either falls back to the name/message rendering.
            if let Some(stack) = crate::object::get(obj, heap, "stack")
                && let Some(text) = stack.as_string(heap)
            {
                let rendered = text.to_lossy_string(heap);
                if !rendered.is_empty() {
                    return rendered;
                }
            }
            if crate::object::has_error_data(obj, heap) {
                let mut rendered = error_classes::render_error_to_string(value, heap);
                if !rendered.is_empty()
                    && crate::object::visit_error_stack_frames(
                        obj,
                        heap,
                        |name, module, position| {
                            error_classes::append_stack_frame(
                                &mut rendered,
                                name,
                                module,
                                position,
                            );
                        },
                    )
                {
                    return rendered;
                }
            }
            if !has_real_name && let Some(ctor_name) = self.thrown_constructor_name(obj) {
                let message = message
                    .filter(|v| !v.is_undefined())
                    .map(|v| {
                        v.as_string(heap)
                            .map_or_else(|| v.display_string(heap), |s| s.to_lossy_string(heap))
                    })
                    .unwrap_or_default();
                return if message.is_empty() {
                    ctor_name
                } else {
                    format!("{ctor_name}: {message}")
                };
            }
        }
        render_thrown_value(value, heap)
    }

    /// Resolve the bytecode function name of an object's
    /// `constructor`. `None` for missing/native/anonymous
    /// constructors.
    fn thrown_constructor_name(&self, obj: crate::object::JsObject) -> Option<String> {
        let ctor = crate::object::get(obj, &self.gc_heap, "constructor")?;
        // The constructor may be interned (`Value::function`) or a
        // closure instance — both carry the same template id.
        let function_id = ctor
            .as_function()
            .or_else(|| ctor.as_closure(&self.gc_heap).map(|c| c.cached_function_id))?;
        let crate::code_space::ChunkResolution::Live {
            function_base,
            payload,
        } = self.code_space.resolve_chunk(function_id)
        else {
            return None;
        };
        let local = function_id.checked_sub(function_base)? as usize;
        let name = payload.module.functions.get(local)?.name.clone();
        (!name.is_empty()).then_some(name)
    }
}

/// Render an uncaught JS value for diagnostic output. Routes
/// Error-shaped objects through [`error_classes::render_error_to_string`]
/// so the unwind printout matches what `e.toString()` returns at
/// the JS surface (§20.5.3.4).
pub(crate) fn render_thrown_value(value: &Value, gc_heap: &otter_gc::GcHeap) -> String {
    if let Some(obj) = value.as_object() {
        // Treat anything with both `name` and `message` data slots
        // as an Error instance. Plain objects fall through to
        // `[object Object]` via `display_string`.
        let has_name = crate::object::get(obj, gc_heap, "name").is_some();
        let has_message = crate::object::get(obj, gc_heap, "message").is_some();
        if has_name || has_message {
            let rendered = error_classes::render_error_to_string(value, gc_heap);
            if !rendered.is_empty() {
                return rendered;
            }
        }
    }
    value.display_string(gc_heap)
}
