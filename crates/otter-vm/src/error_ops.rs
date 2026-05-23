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

use smallvec::SmallVec;

use crate::{
    ErrorKind, ExecutionContext, Frame, Interpreter, IntrinsicError, JsString, NativeError,
    StackFrameSnapshot, Value, VmError, error_classes, json, math, object, read_register,
    symbol_dispatch, temporal, write_register,
};

impl Interpreter {
    pub(crate) fn run_new_error_regs(
        &mut self,
        context: &ExecutionContext,
        stack: &mut SmallVec<[Frame; 8]>,
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
        let frame = &mut stack[top_idx];
        write_register(frame, dst, Value::object(obj))?;
        frame.advance_pc(self.current_byte_len)?;
        Ok(())
    }

    pub(crate) fn run_new_builtin_error_regs(
        &mut self,
        context: &ExecutionContext,
        stack: &mut SmallVec<[Frame; 8]>,
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
        let frame = &mut stack[top_idx];
        write_register(frame, dst, Value::object(obj))?;
        frame.advance_pc(self.current_byte_len)?;
        Ok(())
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
        stack: &SmallVec<[Frame; 8]>,
        kind: ErrorKind,
        message: Option<String>,
        message_value: &Value,
    ) -> Result<object::JsObject, VmError> {
        let proto = self.error_classes.prototype(kind);
        let proto_value = Value::object(proto);
        let obj =
            self.alloc_stack_rooted_object_with_extra_roots(stack, &[message_value, &proto_value])?;
        object::set_prototype(obj, &mut self.gc_heap, Some(proto));
        if let Some(text) = message {
            let s = JsString::from_str(&text, self.gc_heap_mut())?;
            // §20.5.1.1 step 4.c — `msgDesc` is `{ [[Value]]: msg,
            // [[Writable]]: true, [[Enumerable]]: false,
            // [[Configurable]]: true }`. Ordinary `set` would install
            // an enumerable slot; route through `define_own_property`
            // so reflective probes match the spec.
            object::define_own_property(
                obj,
                &mut self.gc_heap,
                "message",
                object::PropertyDescriptor::data(Value::string(s), true, false, true),
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
        stack: &SmallVec<[Frame; 8]>,
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
        stack: &SmallVec<[Frame; 8]>,
        err: &VmError,
    ) -> Option<Value> {
        let dynamic_message: String;
        let is_oom = matches!(err, VmError::OutOfMemory { .. });
        let (kind, message) = match err {
            VmError::TypeMismatch => (
                error_classes::ErrorKind::TypeError,
                "type mismatch: this operation does not accept a value of this type",
            ),
            VmError::TypeMismatchAt { op, kind } => {
                dynamic_message = format!("{op}: cannot operate on a value of type {kind}");
                (
                    error_classes::ErrorKind::TypeError,
                    dynamic_message.as_str(),
                )
            }
            VmError::TypeError { message } => {
                dynamic_message = message.clone();
                (
                    error_classes::ErrorKind::TypeError,
                    dynamic_message.as_str(),
                )
            }
            VmError::RangeError { message } => {
                dynamic_message = message.clone();
                (
                    error_classes::ErrorKind::RangeError,
                    dynamic_message.as_str(),
                )
            }
            VmError::SyntaxError { message } => {
                dynamic_message = message.clone();
                (
                    error_classes::ErrorKind::SyntaxError,
                    dynamic_message.as_str(),
                )
            }
            VmError::NotCallable => (
                error_classes::ErrorKind::TypeError,
                "value is not a function",
            ),
            VmError::TemporalDeadZone { .. } => (
                error_classes::ErrorKind::ReferenceError,
                "cannot access binding before initialization",
            ),
            VmError::UndefinedIdentifier { name } => {
                dynamic_message = format!("{name} is not defined");
                (
                    error_classes::ErrorKind::ReferenceError,
                    dynamic_message.as_str(),
                )
            }
            VmError::UnknownIntrinsic { .. } => (
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
            VmError::JsonError { code, message } => {
                dynamic_message = message.clone();
                let kind = if *code == "JSON_PARSE" {
                    error_classes::ErrorKind::SyntaxError
                } else {
                    error_classes::ErrorKind::TypeError
                };
                (kind, dynamic_message.as_str())
            }
            // Hard / structural errors stay as host failures so the
            // caller surfaces them through `RunError` rather than
            // catching them as `try { ... } catch`.
            _ => return None,
        };
        let obj = if is_oom {
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
                obj,
                &mut self.gc_heap,
                "message",
                Value::string(message_str),
            );
        }
        Some(Value::object(obj))
    }
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
    stack: &[Frame],
) -> Vec<StackFrameSnapshot> {
    stack
        .iter()
        .rev()
        .map(|f| {
            let function = context.function(f.function_id);
            let function_name = function
                .map(|fun| fun.name.clone())
                .unwrap_or_else(|| "<unknown>".to_string());
            // Per-function `spans` is in PC order (compiler emits
            // entries in lowering order). Use `partition_point` to
            // locate the predecessor entry — the largest `pc <=
            // frame.pc`. `partition_point(|s| s.pc <= f.pc)`
            // returns the first index that violates the predicate,
            // so `idx - 1` is the predecessor.
            let span = function
                .and_then(|fun| {
                    let spans = fun.spans.as_slice();
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
                function_name,
                module: module_url,
                span,
            }
        })
        .collect()
}

pub(crate) fn math_to_vm_error(err: math::MathError) -> VmError {
    match err {
        math::MathError::UnknownMember(name) => VmError::UnknownIntrinsic {
            name: format!("Math.{name}"),
        },
        math::MathError::BadArgument { .. } => VmError::TypeMismatch,
    }
}

pub(crate) fn symbol_to_vm_error(err: symbol_dispatch::SymbolError) -> VmError {
    match err {
        symbol_dispatch::SymbolError::UnknownMember(name) => VmError::UnknownIntrinsic {
            name: format!("Symbol.{name}"),
        },
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

pub(crate) fn temporal_to_vm_error(err: temporal::TemporalError) -> VmError {
    match err {
        temporal::TemporalError::UnknownMember { class, method } => VmError::UnknownIntrinsic {
            name: format!("Temporal.{class}.{method}"),
        },
        temporal::TemporalError::BadArgument { .. } => VmError::TypeMismatch,
        temporal::TemporalError::Engine { message, .. } => VmError::Uncaught { value: message },
        temporal::TemporalError::OutOfMemory {
            requested_bytes,
            heap_limit_bytes,
        } => VmError::OutOfMemory {
            requested_bytes,
            heap_limit_bytes,
        },
    }
}

pub(crate) fn native_to_vm_error(err: NativeError) -> VmError {
    match err {
        NativeError::Thrown { name: _, message } => VmError::Uncaught { value: message },
        NativeError::TypeError { name, reason } => VmError::TypeError {
            message: format!("{name}: {reason}"),
        },
        NativeError::SyntaxError { name, reason } => VmError::SyntaxError {
            message: format!("{name}: {reason}"),
        },
        NativeError::RangeError { name, reason } => VmError::RangeError {
            message: format!("{name}: {reason}"),
        },
        NativeError::Exit { code } => VmError::Exit { code },
    }
}

/// Convert a `VmError` into a JS `Value` used as a rejection
/// reason for promise reactions. Foundation: a plain string is
/// fine; once the full Error hierarchy is in we'll synthesize a
/// real `TypeError` / `RangeError` instance.
pub(crate) fn vm_err_to_value(err: &VmError, heap: &mut otter_gc::GcHeap) -> Value {
    Value::string(
        crate::JsString::from_str(&err.to_string(), heap).unwrap_or_else(|_| {
            // Allocator failure here is exceptional; substitute
            // an empty string rather than panicking.
            crate::JsString::from_str("", heap).expect("empty string allocates")
        }),
    )
}

pub(crate) fn json_to_vm_error(err: json::JsonError) -> VmError {
    // Diagnostic strings stay short and spec-faithful (no cycle
    // path-walk) to match the identity-pointer visit set. Parse
    // errors additionally carry the byte position so users can
    // locate the offending token.
    match err {
        json::JsonError::UnknownMember(name) => VmError::UnknownIntrinsic {
            name: format!("JSON.{name}"),
        },
        json::JsonError::OutOfMemory {
            requested_bytes,
            heap_limit_bytes,
        } => VmError::OutOfMemory {
            requested_bytes,
            heap_limit_bytes,
        },
        json::JsonError::Cyclic => VmError::JsonError {
            code: "JSON_CYCLIC",
            message: "JSON.stringify cannot serialize cyclic structures.".to_string(),
        },
        json::JsonError::BigInt => VmError::JsonError {
            code: "JSON_BIGINT",
            message: "JSON.stringify cannot serialize BigInt values.".to_string(),
        },
        json::JsonError::TooDeep { limit } => VmError::JsonError {
            code: "JSON_DEPTH",
            message: format!("JSON nesting exceeded {limit} levels."),
        },
        json::JsonError::ParseFailed { message, position } => VmError::JsonError {
            code: "JSON_PARSE",
            message: format!("JSON Parse error: {message} at byte {position}"),
        },
        json::JsonError::BadArgument {
            name,
            index,
            reason,
        } => VmError::JsonError {
            code: "JSON_BAD_ARG",
            message: format!("JSON.{name} argument {index} {reason}"),
        },
    }
}

pub(crate) fn intrinsic_to_vm_error(err: IntrinsicError) -> VmError {
    match err {
        IntrinsicError::OutOfMemory {
            requested_bytes,
            heap_limit_bytes,
        } => VmError::OutOfMemory {
            requested_bytes,
            heap_limit_bytes,
        },
        IntrinsicError::BadReceiver { .. } | IntrinsicError::BadArgument { .. } => {
            VmError::TypeMismatch
        }
        IntrinsicError::OutOfRange { index, reason } => VmError::RangeError {
            message: format!("argument {index} out of range: {reason}"),
        },
        IntrinsicError::UnknownMethod { name } => VmError::UnknownIntrinsic {
            name: name.to_string(),
        },
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
