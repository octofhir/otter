//! Error-object opcode helpers.
//!
//! Error constructors are fixed-width bytecodes and should stay on the compact
//! executable operand path instead of the fallback operand-slice path.
//!
//! # Contents
//! - `new Error(message)` object allocation.
//! - Native error constructor allocation (`TypeError`, `RangeError`, ...).
//! - Native error constructor loading for identifier reads.
//!
//! # Invariants
//! - Error kind names are compiler-emitted string constants.
//! - Allocated instances come from the interpreter's `ErrorClassRegistry` so
//!   prototype identity matches `instanceof`.
//!
//! # See also
//! - [`crate::error_classes`]
//! - [`crate::executable`]

use crate::holt_stack::HoltStack;
use smallvec::SmallVec;

use crate::{
    ErrorKind, ExecutionContext, Frame, Interpreter, JsString, NativeError, StackFrameSnapshot,
    Value, VmError, error_classes, object, read_register, symbol_dispatch, write_register,
};

impl Interpreter {
    pub(crate) fn run_new_error_regs(
        &mut self,
        context: &ExecutionContext,
        stack: &mut HoltStack,
        top_idx: usize,
        dst: u16,
        msg_reg: u16,
    ) -> Result<(), VmError> {
        let frame = &stack[top_idx];
        let value = *read_register(frame, msg_reg)?;
        let owned_message = self.coerce_error_message(context, &value)?;
        let obj = self.make_error_instance_with_stack_roots(
            stack,
            ErrorKind::Error,
            owned_message,
            &value,
        )?;
        self.capture_error_stack_frames(context, stack, obj);
        let frame = &mut stack[top_idx];
        write_register(frame, dst, Value::object(obj))?;
        frame.advance_pc(self.current_byte_len)?;
        Ok(())
    }

    pub(crate) fn run_new_builtin_error_regs(
        &mut self,
        context: &ExecutionContext,
        stack: &mut HoltStack,
        top_idx: usize,
        dst: u16,
        kind_idx: u32,
        msg_reg: u16,
    ) -> Result<(), VmError> {
        let kind_name = context
            .string_constant_str(kind_idx)
            .ok_or(VmError::InvalidOperand)?;
        let kind = ErrorKind::from_class_name(kind_name).ok_or(VmError::InvalidOperand)?;
        let frame = &stack[top_idx];
        let value = *read_register(frame, msg_reg)?;
        let owned_message = self.coerce_error_message(context, &value)?;
        let obj = self.make_error_instance_with_stack_roots(stack, kind, owned_message, &value)?;
        self.capture_error_stack_frames(context, stack, obj);
        let frame = &mut stack[top_idx];
        write_register(frame, dst, Value::object(obj))?;
        frame.advance_pc(self.current_byte_len)?;
        Ok(())
    }

    /// Record the construction-site JS call stack (top-of-stack first,
    /// bounded by `Error.stackTraceLimit`) onto a freshly built error
    /// instance for `Error.prototype.stack`. No-op when the limit is 0
    /// or the stack is empty.
    fn capture_error_stack_frames(
        &mut self,
        context: &ExecutionContext,
        stack: &HoltStack,
        obj: object::JsObject,
    ) {
        let limit = self.current_stack_trace_limit();
        if limit == 0 {
            return;
        }
        let mut frames = snapshot_frames(context, stack);
        if frames.len() > limit {
            frames.truncate(limit);
        }
        if !frames.is_empty() {
            object::set_error_stack_frames(obj, self.gc_heap_mut(), frames);
        }
    }

    /// §20.5.1.1 step 3 — coerce the `message` argument through full
    /// §7.1.17 `ToString`. Returns `None` when the argument is
    /// `undefined` (the spec skips step 3 in that case, leaving
    /// `message` inherited from the prototype). Delegates the spec
    /// ladder to [`Interpreter::coerce_to_string`].
    fn coerce_error_message(
        &mut self,
        context: &ExecutionContext,
        value: &Value,
    ) -> Result<Option<String>, VmError> {
        if value.is_undefined() {
            return Ok(None);
        }
        Ok(Some(self.coerce_to_string(context, value)?))
    }

    fn make_error_instance_with_stack_roots(
        &mut self,
        stack: &HoltStack,
        kind: ErrorKind,
        message: Option<String>,
        message_value: &Value,
    ) -> Result<object::JsObject, VmError> {
        let message_gc_value = message
            .as_ref()
            .map(|text| JsString::from_str(text, self.gc_heap_mut()).map(Value::string))
            .transpose()?;
        let mut extra_roots: SmallVec<[&Value; 4]> = smallvec::smallvec![message_value];
        if let Some(ref message_gc_value) = message_gc_value {
            extra_roots.push(message_gc_value);
        }
        let obj = self.alloc_stack_rooted_object_with_extra_roots(stack, &extra_roots)?;
        // Fetch the prototype only after every allocation in this function:
        // the message-string and object allocs above can each trigger a major
        // GC that relocates the (old-gen) error prototype. The class registry
        // is a GC root whose handles the collector forwards in place, so it
        // always yields the live pointer — a handle captured earlier would be
        // stale and silently corrupt the new instance's `[[Prototype]]`.
        let proto = self.error_classes.prototype(kind);
        object::set_prototype(obj, &mut self.gc_heap, Some(proto));
        // §20.5.* — mark the `[[ErrorData]]` internal slot.
        object::set_error_data(obj, &mut self.gc_heap);
        if let Some(message_gc_value) = message_gc_value {
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
            );
        }
        Ok(obj)
    }

    pub(crate) fn run_load_builtin_error_reg(
        &self,
        context: &ExecutionContext,
        frame: &mut Frame,
        dst: u16,
        kind_idx: u32,
    ) -> Result<(), VmError> {
        let kind_name = context
            .string_constant_str(kind_idx)
            .ok_or(VmError::InvalidOperand)?;
        let kind = ErrorKind::from_class_name(kind_name).ok_or(VmError::InvalidOperand)?;
        let ctor = self.error_classes.constructor(kind);
        write_register(frame, dst, Value::object(ctor))?;
        frame.advance_pc(self.current_byte_len)?;
        Ok(())
    }
    /// Build a freshly-allocated `TypeError` instance through the live frame
    /// stack. Mirrors the shape produced by
    /// [`Self::vm_error_to_throwable_with_stack_roots`] for `VmError::TypeError`
    /// but skips the `VmError` wrapping.
    pub(crate) fn make_type_error_with_stack_roots(
        &mut self,
        stack: &HoltStack,
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

    /// `Error` instance. Returns `None` for variants that should
    /// keep propagating as host errors (StackOverflow, etc.).
    pub(crate) fn vm_error_to_throwable_with_stack_roots(
        &mut self,
        stack: &HoltStack,
        err: &VmError,
    ) -> Option<Value> {
        use crate::run_control::ErrorDetail;
        let is_oom = matches!(err, VmError::OutOfMemory { .. });
        // Node-style `.code` to stamp on the instance after it is built.
        let mut node_code: Option<&'static str> = None;
        // `VmError` is `Copy`; its dynamic message/payload lives in the isolate
        // pending-error slot. Pull it out once, paired with the discriminant.
        let detail = self.error_detail();
        let msg_detail = || match &detail {
            Some(ErrorDetail::Message(m)) => m.to_string(),
            Some(ErrorDetail::Name(m)) => m.to_string(),
            Some(ErrorDetail::Uncaught(m)) => m.to_string(),
            _ => String::new(),
        };
        let dynamic_message: String;
        let (kind, message): (error_classes::ErrorKind, &str) = match err {
            VmError::Coded => {
                if let Some(ErrorDetail::Coded(payload)) = &detail {
                    node_code = Some(payload.code);
                    dynamic_message = payload.message.clone();
                    (payload.kind, dynamic_message.as_str())
                } else {
                    dynamic_message = msg_detail();
                    (error_classes::ErrorKind::Error, dynamic_message.as_str())
                }
            }
            VmError::TypeMismatch => (
                error_classes::ErrorKind::TypeError,
                "type mismatch: this operation does not accept a value of this type",
            ),
            VmError::TypeMismatchAt => {
                dynamic_message = match &detail {
                    Some(ErrorDetail::Mismatch(p)) => {
                        format!("{}: cannot operate on a value of type {}", p.op, p.kind)
                    }
                    _ => "TypeError".to_string(),
                };
                (
                    error_classes::ErrorKind::TypeError,
                    dynamic_message.as_str(),
                )
            }
            VmError::TypeError => {
                dynamic_message = msg_detail();
                (
                    error_classes::ErrorKind::TypeError,
                    dynamic_message.as_str(),
                )
            }
            VmError::RangeError => {
                dynamic_message = msg_detail();
                (
                    error_classes::ErrorKind::RangeError,
                    dynamic_message.as_str(),
                )
            }
            VmError::SyntaxError => {
                dynamic_message = msg_detail();
                (
                    error_classes::ErrorKind::SyntaxError,
                    dynamic_message.as_str(),
                )
            }
            VmError::URIError => {
                dynamic_message = msg_detail();
                (error_classes::ErrorKind::URIError, dynamic_message.as_str())
            }
            VmError::NotCallable => (
                error_classes::ErrorKind::TypeError,
                "value is not a function",
            ),
            VmError::TemporalDeadZone { .. } => (
                error_classes::ErrorKind::ReferenceError,
                "cannot access binding before initialization",
            ),
            VmError::ThisUninitialized => {
                dynamic_message = msg_detail();
                (
                    error_classes::ErrorKind::ReferenceError,
                    dynamic_message.as_str(),
                )
            }
            VmError::UndefinedIdentifier => {
                dynamic_message = match &detail {
                    Some(ErrorDetail::Name(name)) => format!("{name} is not defined"),
                    _ => "identifier is not defined".to_string(),
                };
                (
                    error_classes::ErrorKind::ReferenceError,
                    dynamic_message.as_str(),
                )
            }
            VmError::UnknownIntrinsic => (
                error_classes::ErrorKind::TypeError,
                "unknown intrinsic method",
            ),
            VmError::OutOfMemory { .. } => {
                dynamic_message = err.to_string();
                (
                    error_classes::ErrorKind::RangeError,
                    dynamic_message.as_str(),
                )
            }
            // §25.5 JSON.parse / JSON.stringify spec-mandated
            // exception classes:
            //   parse failures → SyntaxError (§25.5.1.1 step 2),
            //   cyclic / BigInt / depth / bad-arg → TypeError.
            VmError::JsonError => {
                let (jkind, jmsg) = match &detail {
                    Some(ErrorDetail::Json(payload)) => {
                        let kind = if payload.code == "JSON_PARSE" {
                            error_classes::ErrorKind::SyntaxError
                        } else {
                            error_classes::ErrorKind::TypeError
                        };
                        (kind, payload.message.clone())
                    }
                    _ => (error_classes::ErrorKind::TypeError, String::new()),
                };
                dynamic_message = jmsg;
                (jkind, dynamic_message.as_str())
            }
            // Hard / structural errors stay as host failures so the
            // caller surfaces them through `RunError` rather than
            // catching them as `try { ... } catch`.
            _ => return None,
        };
        let mut obj = if is_oom {
            crate::object::alloc_diagnostic_object(&mut self.gc_heap).ok()?
        } else {
            self.make_error_instance_with_stack_roots(
                stack,
                kind,
                Some(message.to_string()),
                &Value::undefined(),
            )
            .ok()?
        };
        if is_oom {
            let proto = match crate::object::get(
                self.error_classes.constructor(kind),
                &self.gc_heap,
                "prototype",
            ) {
                Some(v) if let Some(proto) = v.as_object() => proto,
                _ => self.error_classes.prototype(kind),
            };
            crate::object::set_prototype(obj, &mut self.gc_heap, Some(proto));
        }
        if is_oom && let Ok(message_str) = JsString::from_str(message, self.gc_heap_mut()) {
            crate::object::set(
                &mut obj,
                &mut self.gc_heap,
                "message",
                Value::string(message_str),
            );
        }
        // Stamp the Node-style `.code` as an own, non-enumerable, writable,
        // configurable property (matches Node's error.code descriptor).
        if let Some(code) = node_code
            && let Ok(code_str) = JsString::from_str(code, self.gc_heap_mut())
        {
            crate::object::define_own_property(
                obj,
                &mut self.gc_heap,
                "code",
                crate::object::PropertyDescriptor::data(Value::string(code_str), true, false, true),
            );
            if code == "ERR_SYSTEM_ERROR" {
                if let Ok(name_str) = JsString::from_str("SystemError", self.gc_heap_mut()) {
                    crate::object::define_own_property(
                        obj,
                        &mut self.gc_heap,
                        "name",
                        crate::object::PropertyDescriptor::data(
                            Value::string(name_str),
                            true,
                            false,
                            true,
                        ),
                    );
                }
                if let Ok(mut info) = object::alloc_object_old(self.gc_heap_mut()) {
                    if let Ok(info_code) =
                        JsString::from_str(system_error_code(message), self.gc_heap_mut())
                    {
                        crate::object::set(
                            &mut info,
                            &mut self.gc_heap,
                            "code",
                            Value::string(info_code),
                        );
                    }
                    crate::object::define_own_property(
                        obj,
                        &mut self.gc_heap,
                        "info",
                        crate::object::PropertyDescriptor::data(
                            Value::object(info),
                            true,
                            false,
                            true,
                        ),
                    );
                }
            }
        }
        Some(Value::object(obj))
    }
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
pub(crate) fn snapshot_frames(
    context: &ExecutionContext,
    stack: &HoltStack,
) -> Vec<StackFrameSnapshot> {
    stack
        .iter()
        .rev()
        .map(|f| {
            let function = context.function(f.function_id);
            let exec_function = context.exec_function(f.function_id);
            let function_name = function
                .map(|fun| fun.name.clone())
                .unwrap_or_else(|| "<unknown>".to_string());
            // `byte_spans` is sorted by `pc`. `partition_point` finds
            // the predecessor entry (largest `pc <= f.pc`), so
            // `idx - 1` is the matching span.
            let span = exec_function
                .and_then(|fun| {
                    let spans = fun.byte_spans();
                    let idx = spans.partition_point(|s| s.pc <= f.pc);
                    if idx == 0 {
                        spans.first().map(|s| s.span)
                    } else {
                        Some(spans[idx - 1].span)
                    }
                })
                .or_else(|| function.map(|fun| fun.span))
                .unwrap_or((0, 0));
            let module_url = function
                .filter(|fun| !fun.module_url.is_empty())
                .map(|fun| fun.module_url.clone())
                .unwrap_or_else(|| context.module_name().to_string());
            StackFrameSnapshot {
                function_id: f.function_id,
                function_name,
                module: module_url,
                span,
            }
        })
        .collect()
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

pub(crate) fn native_to_vm_error(interp: &crate::Interpreter, err: NativeError) -> VmError {
    match err {
        NativeError::Thrown { name: _, message } => interp.err_uncaught(message.into()),
        NativeError::Coded {
            kind,
            code,
            message,
        } => interp.err_coded(kind, code, message),
        NativeError::TypeError { name, reason } => {
            interp.err_type(format!("{name}: {reason}").into())
        }
        NativeError::SyntaxError { name, reason } => {
            interp.err_syntax(format!("{name}: {reason}").into())
        }
        NativeError::RangeError { name, reason } => {
            interp.err_range(format!("{name}: {reason}").into())
        }
        NativeError::URIError { name, reason } => {
            interp.err_uri(format!("{name}: {reason}").into())
        }
        // Round-trips back to a ReferenceError-classed VmError so a TDZ
        // error raised behind a native boundary keeps its class.
        NativeError::ReferenceError { name, reason } => {
            interp.err_this_uninit(format!("{name}: {reason}").into())
        }
        NativeError::Exit { code } => VmError::Exit { code },
        NativeError::Interrupted => VmError::Interrupted,
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

/// Convert a `VmError` into a JS `Value` used as a rejection
/// reason for promise reactions. Foundation: a plain string is
/// fine; once the full Error hierarchy is in we'll synthesize a
/// real `TypeError` / `RangeError` instance.
pub(crate) fn vm_err_to_value(interp: &mut crate::Interpreter, err: &VmError) -> Value {
    let message = interp.render_vm_error(err);
    let heap = interp.gc_heap_mut();
    Value::string(
        crate::JsString::from_str(&message, heap).unwrap_or_else(|_| {
            // Allocator failure here is exceptional; substitute
            // an empty string rather than panicking.
            crate::JsString::from_str("", heap).expect("empty string allocates")
        }),
    )
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
        let function_id = ctor.as_function()?;
        let chunk = self.code_space.chunk_for(function_id)?;
        let local = function_id.checked_sub(chunk.function_base)? as usize;
        let name = chunk.module.functions.get(local)?.name.clone();
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
