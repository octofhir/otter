//! ECMA-262 §19.3 / §20.5 Error class hierarchy.
//!
//! Each interpreter holds one [`ErrorClassRegistry`] populated at
//! construction. The registry stores the seven canonical error
//! classes — `Error` and its six native subclasses (`TypeError`,
//! `RangeError`, `SyntaxError`, `ReferenceError`, `URIError`,
//! `EvalError`) — as constructor [`JsObject`]s with a proper
//! prototype chain so spec-faithful patterns work without bespoke
//! handling at every call site:
//!
//! - `e instanceof TypeError` and `e instanceof Error` both hold on
//!   any instance produced through the registry, because each
//!   subclass's prototype's `[[Prototype]]` points back to
//!   `Error.prototype`.
//! - `e.name` returns the matching class name; `e.message` returns
//!   the constructor-passed string (or `""` when omitted).
//! - The constructor `JsObject` itself carries a `prototype` own
//!   property that the runtime's `Op::Instanceof` walker reads to
//!   compare against the candidate's `[[Prototype]]` chain.
//!
//! # Contents
//! - [`ErrorKind`] — the seven canonical kinds.
//! - [`ErrorClassRegistry`] — Interpreter-owned table of constructor
//!   + prototype objects keyed by [`ErrorKind`].
//!
//! # Invariants
//! - Every registry built through [`ErrorClassRegistry::new`] is
//!   self-consistent: subclass prototypes' `[[Prototype]]` always
//!   resolves to `Error.prototype`, and every constructor's
//!   `prototype` own property points to the matching prototype
//!   object.
//! - The registry never re-allocates its prototype/constructor table after
//!   construction. Active native constructor paths allocate instances through
//!   `NativeCtx` so receiver, `new.target`, arguments, pending causes, and
//!   AggregateError element buffers stay visible across young allocation.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-error-objects>
//! - <https://tc39.es/ecma262/#sec-native-error-types-used-in-this-standard>
//! - <https://tc39.es/ecma262/#sec-error-message>
//! - <https://tc39.es/ecma262/#sec-error.prototype.tostring>

use crate::gc_trace::GcRootVisitor;
use crate::native_function::NativeFunction;
use crate::number::NumberValue;
use crate::object::{self, JsObject, PropertyDescriptor};
use crate::rooting::RootScopeExt;
use crate::string::JsString;
use crate::{ExecutionContext, Value};
use crate::{NativeCtx, NativeError};
use otter_gc::raw::RawGc;

/// One of the seven canonical native error classes.
///
/// `Error` is the base; the other six derive from it both
/// structurally (their prototype chains pass through
/// `Error.prototype`) and behaviourally (they all share
/// `Error.prototype.toString`).
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-error-objects>
/// - <https://tc39.es/ecma262/#sec-native-error-types-used-in-this-standard>
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub enum ErrorKind {
    /// `Error` — root of the hierarchy. §20.5.1.
    Error,
    /// `TypeError` — operand has the wrong type for an operation.
    /// §20.5.5.5.
    TypeError,
    /// `RangeError` — value is outside the allowed range. §20.5.5.2.
    RangeError,
    /// `SyntaxError` — parse failure for `eval` / regex /
    /// JSON-parse / template-tag input. §20.5.5.3.
    SyntaxError,
    /// `ReferenceError` — read of a non-existent binding /
    /// temporal-dead-zone access. §20.5.5.4.
    ReferenceError,
    /// `URIError` — `decodeURI` / `encodeURI` malformed input.
    /// §20.5.5.6.
    URIError,
    /// `EvalError` — historically `eval` errors; the spec keeps the
    /// constructor as a no-op subclass for backward compatibility.
    /// §20.5.5.1.
    EvalError,
    /// `AggregateError` — wraps a collection of errors per ECMA-262
    /// §20.5.7. Produced by [`Promise.any`](https://tc39.es/ecma262/#sec-promise.any)
    /// when every input rejects, and exposed for direct
    /// construction.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-aggregate-error-objects>
    AggregateError,
}

impl ErrorKind {
    /// Spec-canonical class name.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-error.prototype.name>
    #[must_use]
    pub const fn class_name(self) -> &'static str {
        match self {
            Self::Error => "Error",
            Self::TypeError => "TypeError",
            Self::RangeError => "RangeError",
            Self::SyntaxError => "SyntaxError",
            Self::ReferenceError => "ReferenceError",
            Self::URIError => "URIError",
            Self::EvalError => "EvalError",
            Self::AggregateError => "AggregateError",
        }
    }

    /// Look up a kind from its identifier name.
    ///
    /// Used by the compiler to decide whether `new Foo("msg")` /
    /// the bare identifier `Foo` should lower to the dedicated
    /// error opcodes.
    #[must_use]
    pub fn from_class_name(name: &str) -> Option<Self> {
        match name {
            "Error" => Some(Self::Error),
            "TypeError" => Some(Self::TypeError),
            "RangeError" => Some(Self::RangeError),
            "SyntaxError" => Some(Self::SyntaxError),
            "ReferenceError" => Some(Self::ReferenceError),
            "URIError" => Some(Self::URIError),
            "EvalError" => Some(Self::EvalError),
            "AggregateError" => Some(Self::AggregateError),
            _ => None,
        }
    }

    /// Iterator over every variant in declaration order. Stable
    /// for the lifetime of the crate so registry construction
    /// stays deterministic.
    pub fn all() -> &'static [Self] {
        &[
            Self::Error,
            Self::TypeError,
            Self::RangeError,
            Self::SyntaxError,
            Self::ReferenceError,
            Self::URIError,
            Self::EvalError,
            Self::AggregateError,
        ]
    }
}

/// Per-class entry in the registry — pair of prototype +
/// constructor [`JsObject`]s.
#[derive(Debug, Clone)]
struct ClassEntry {
    /// The class's `prototype` object — shared by every instance
    /// of the class as its `[[Prototype]]`.
    prototype: JsObject,
    /// The class's constructor object. Carries the matching
    /// `prototype` own property so `Op::Instanceof` can resolve
    /// `instance instanceof Class`.
    constructor: JsObject,
}

impl ClassEntry {
    fn trace_roots(&self, visitor: &mut GcRootVisitor<'_>) {
        visitor(std::ptr::addr_of!(self.prototype) as *mut RawGc);
        visitor(std::ptr::addr_of!(self.constructor) as *mut RawGc);
    }
}

unsafe fn trace_class_entries(slot: *mut (), visitor: &mut dyn FnMut(*mut RawGc)) {
    // SAFETY: `ErrorClassRegistry::new` registers the address of its live
    // `Vec<(ErrorKind, ClassEntry)>` for exactly the lifetime of the enclosing
    // `RootScope`. The vector value itself does not move while the scope is
    // active; reallocating its backing buffer is fine because it is read here
    // on every trace.
    let entries = unsafe { &mut *slot.cast::<Vec<(ErrorKind, ClassEntry)>>() };
    for (_, entry) in entries {
        visitor(std::ptr::addr_of_mut!(entry.prototype).cast::<RawGc>());
        visitor(std::ptr::addr_of_mut!(entry.constructor).cast::<RawGc>());
    }
}

fn oom() -> otter_gc::OutOfMemory {
    otter_gc::OutOfMemory::HeapCapExceeded {
        requested_bytes: 0,
        heap_limit_bytes: 0,
    }
}

fn trace_value_roots(roots: &[&Value], visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)) {
    for value in roots {
        value.trace_value_slots(visitor);
    }
}

fn alloc_registry_object(
    gc_heap: &mut otter_gc::GcHeap,
    roots: &[&Value],
) -> Result<JsObject, otter_gc::OutOfMemory> {
    let mut external_visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
        trace_value_roots(roots, visitor);
    };
    crate::object::alloc_object_with_roots(gc_heap, &mut external_visit).map_err(|_| oom())
}

/// Allocate a `JsString` while keeping `roots` live across the allocation.
///
/// Bootstrap builds a prototype's `name` / `message` strings one at a time; the
/// string body allocation can drive a collection that relocates the young
/// prototype (and sweep a prior, still-unrooted string), so the caller passes
/// the prototype and any earlier string as roots. Mirrors [`alloc_registry_object`].
fn from_str_rooted(
    s: &str,
    gc_heap: &mut otter_gc::GcHeap,
    roots: &[&Value],
) -> Result<JsString, otter_gc::OutOfMemory> {
    let mut external_visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
        trace_value_roots(roots, visitor);
    };
    JsString::from_str_with_roots(s, gc_heap, &mut external_visit)
}

fn native_static_with_roots(
    gc_heap: &mut otter_gc::GcHeap,
    name: &'static str,
    length: u8,
    call: crate::native_function::NativeFastFn,
    roots: &[&Value],
) -> Result<NativeFunction, otter_gc::OutOfMemory> {
    let mut external_visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
        trace_value_roots(roots, visitor);
    };
    NativeFunction::new_static_with_roots(gc_heap, name, length, call, &mut external_visit)
        .map_err(|_| oom())
}

fn native_constructor_static_with_roots(
    gc_heap: &mut otter_gc::GcHeap,
    name: &'static str,
    length: u8,
    call: crate::native_function::NativeFastFn,
    roots: &[&Value],
) -> Result<NativeFunction, otter_gc::OutOfMemory> {
    let mut external_visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
        trace_value_roots(roots, visitor);
    };
    NativeFunction::new_constructor_static_with_roots(
        gc_heap,
        name,
        length,
        call,
        &mut external_visit,
    )
    .map_err(|_| oom())
}

/// Per-interpreter registry of the seven canonical error classes.
///
/// Constructed once at interpreter startup and threaded through
/// the dispatch loop so [`Op::NewBuiltinError`] /
/// [`Op::LoadBuiltinError`] / [`Op::NewError`] all see the same
/// prototype chain.
#[derive(Debug, Clone)]
pub struct ErrorClassRegistry {
    error: ClassEntry,
    type_error: ClassEntry,
    range_error: ClassEntry,
    syntax_error: ClassEntry,
    reference_error: ClassEntry,
    uri_error: ClassEntry,
    eval_error: ClassEntry,
    aggregate_error: ClassEntry,
}

/// §20.5.3.4 Error.prototype.toString — single source of truth for
/// rendering an Error-shaped value. Reads `name` (default
/// `"Error"`) and `message` (default empty) off the receiver and
/// returns:
///
/// - `""` when both fields stringify to empty.
/// - `<name>` when `message` is empty.
/// - `<message>` when `name` is empty.
/// - `<name>: <message>` otherwise.
///
/// Used by both the user-facing `e.toString()` interception in
/// `do_call_method_value` and the unwind-diagnostic path that
/// renders uncaught throws.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-error.prototype.tostring>
///
/// §20.5.3.4 `Error.prototype.toString` — accessor-aware spec
/// implementation. Walks `Get(O, "name")` / `Get(O, "message")`
/// through the interpreter so user-defined getters fire and any
/// abrupt completion (e.g. `Symbol` message → TypeError, throwing
/// `valueOf` / `toString`) propagates.
///
/// Defaults follow §20.5.3.4: `name` defaults to `"Error"` when
/// `Get` returns `undefined`; `message` defaults to the empty
/// string. Both non-undefined values go through full `ToString`.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-error.prototype.tostring>
pub(crate) fn render_error_to_string_spec(
    interp: &mut crate::Interpreter,
    context: &crate::ExecutionContext,
    receiver: &Value,
) -> Result<String, crate::VmError> {
    fn coerce(
        interp: &mut crate::Interpreter,
        context: &crate::ExecutionContext,
        receiver: &Value,
        key: &'static str,
        default: &str,
    ) -> Result<String, crate::VmError> {
        let vm_key = crate::VmPropertyKey::String(key);
        let outcome = interp.ordinary_get_value(context, *receiver, *receiver, &vm_key, 0)?;
        let value = match outcome {
            crate::VmGetOutcome::Value(v) => v,
            crate::VmGetOutcome::InvokeGetter { getter } => {
                let args: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::new();
                interp.run_callable_sync(context, &getter, *receiver, args)?
            }
        };
        if value.is_undefined() {
            return Ok(default.to_string());
        }
        if value.is_symbol() {
            return Err(interp.err_type(
                (format!("Cannot convert a Symbol value to a string ('{key}')")).into(),
            ));
        }
        if let Some(s) = value.as_string(interp.gc_heap()) {
            return Ok(s.to_lossy_string(interp.gc_heap()));
        }
        if value.is_null() || value.is_boolean() || value.is_number() || value.is_big_int() {
            return Ok(value.display_string(interp.gc_heap()));
        }
        let primitive = interp.evaluate_to_primitive(
            context,
            &value,
            crate::abstract_ops::ToPrimitiveHint::String,
        )?;
        if primitive.is_symbol() {
            return Err(interp.err_type(
                (format!("Cannot convert a Symbol value to a string ('{key}')")).into(),
            ));
        }
        if let Some(s) = primitive.as_string(interp.gc_heap()) {
            Ok(s.to_lossy_string(interp.gc_heap()))
        } else {
            Ok(primitive.display_string(interp.gc_heap()))
        }
    }

    let name = coerce(interp, context, receiver, "name", "Error")?;
    let message = coerce(interp, context, receiver, "message", "")?;
    Ok(match (name.is_empty(), message.is_empty()) {
        (true, true) => String::new(),
        (false, true) => name,
        (true, false) => message,
        (false, false) => format!("{name}: {message}"),
    })
}

/// Synchronous (non-spec) renderer used by the runtime's
/// uncaught-throw diagnostic path. Cannot invoke accessors /
/// `@@toPrimitive`; callers that need the spec semantics should use
/// [`render_error_to_string_spec`].
pub fn render_error_to_string(value: &Value, gc_heap: &otter_gc::GcHeap) -> String {
    let Some(obj) = value.as_object() else {
        return value.display_string(gc_heap);
    };
    // §20.5.3.4 defaults: name → "Error", message → "" when absent/undefined.
    let name = match crate::object::get(obj, gc_heap, "name") {
        None => "Error".to_string(),
        Some(v) if v.is_undefined() => "Error".to_string(),
        Some(v) => v
            .as_string(gc_heap)
            .map(|s| s.to_lossy_string(gc_heap))
            .unwrap_or_else(|| v.display_string(gc_heap)),
    };
    let message = match crate::object::get(obj, gc_heap, "message") {
        None => String::new(),
        Some(v) if v.is_undefined() => String::new(),
        Some(v) => v
            .as_string(gc_heap)
            .map(|s| s.to_lossy_string(gc_heap))
            .unwrap_or_else(|| v.display_string(gc_heap)),
    };
    match (name.is_empty(), message.is_empty()) {
        (true, true) => String::new(),
        (false, true) => name,
        (true, false) => message,
        (false, false) => format!("{name}: {message}"),
    }
}

/// V8's default `Error.stackTraceLimit`: capture at most 10 frames.
pub(crate) const DEFAULT_STACK_TRACE_LIMIT: usize = 10;

/// Read the live `Error.stackTraceLimit` and translate it to a frame
/// cap, matching V8's coercion at capture time: a non-negative finite
/// number caps the count, `Infinity` keeps every frame, and a missing
/// property falls back to the default 10. A non-number or a value `<= 0`
/// (or `NaN`) disables capture entirely.
pub(crate) fn stack_trace_limit(ctx: &mut NativeCtx<'_>) -> usize {
    let Some(error_ctor) = ctx.global_value("Error") else {
        return DEFAULT_STACK_TRACE_LIMIT;
    };
    let Some(obj) = error_ctor.as_object() else {
        return DEFAULT_STACK_TRACE_LIMIT;
    };
    match crate::object::get(obj, ctx.heap(), "stackTraceLimit") {
        None => DEFAULT_STACK_TRACE_LIMIT,
        Some(v) => match v.as_f64() {
            Some(n) if n.is_infinite() && n > 0.0 => usize::MAX,
            Some(n) if n.is_finite() && n >= 1.0 => n as usize,
            // NaN, non-positive, or non-number → no capture.
            _ => 0,
        },
    }
}

/// `true` when a frame's function name is one of the interpreter's
/// synthetic placeholders (`<main>`, `<anonymous>`, `<arrow>`, …),
/// which V8 renders without a leading function name.
fn is_anonymous_frame_name(name: &str) -> bool {
    name.is_empty() || name.starts_with('<')
}

/// Append V8-style frame lines to an error's stack string. Each line is
/// `    at <fn> (<module>:<line>:<col>)`, or `    at <module>:<line>:<col>`
/// for anonymous/top-level frames. Line/column come from the registered
/// module source (1-based, UTF-16 columns); when the source is unknown
/// the module URL is emitted without a position.
pub(crate) fn append_stack_frames(
    out: &mut String,
    frames: &[crate::run_control::StackFrameSnapshot],
    interp: &crate::Interpreter,
) {
    use std::fmt::Write as _;
    for frame in frames {
        let location = match interp.source_line_col(&frame.module, frame.span.0) {
            Some((line, col)) => format!("{}:{}:{}", frame.module, line, col),
            None => frame.module.clone(),
        };
        if is_anonymous_frame_name(&frame.function_name) {
            let _ = write!(out, "\n    at {location}");
        } else {
            let _ = write!(out, "\n    at {} ({location})", frame.function_name);
        }
    }
}

impl ErrorClassRegistry {
    /// Walk every GC-managed object held by the registry.
    ///
    /// The interpreter roots this registry across every full-GC
    /// cycle. Each entry owns two `JsObject` handles — the
    /// prototype and constructor — and those objects in turn trace
    /// their prototype/property graph through `ObjectBody`.
    pub(crate) fn trace_gc_roots(&self, visitor: &mut GcRootVisitor<'_>) {
        for entry in [
            &self.error,
            &self.type_error,
            &self.range_error,
            &self.syntax_error,
            &self.reference_error,
            &self.uri_error,
            &self.eval_error,
            &self.aggregate_error,
        ] {
            entry.trace_roots(visitor);
        }
    }

    /// Build the seven prototypes + constructors and link the
    /// inheritance chains.
    ///
    /// # Algorithm
    /// 1. Allocate `Error.prototype` and stamp `name = "Error"` and
    ///    `message = ""` (§20.5.3.4 / §20.5.3.5).
    /// 2. For each of the six native subclasses, allocate a fresh
    ///    `prototype` object and link its `[[Prototype]]` to
    ///    `Error.prototype`. Stamp its own `name` to the class
    ///    name.
    /// 3. Allocate a constructor `JsObject` per class with a
    ///    `prototype` own property pointing to the matching
    ///    prototype. The constructor itself isn't callable
    ///    (foundation slice — `new TypeError(...)` lowers to a
    ///    dedicated opcode); the constructor's only role is to
    ///    surface as the right-hand side of `instanceof`.
    ///
    /// # Errors
    /// Returns [`StringError::OutOfMemory`] if `heap` cannot
    /// accommodate the per-class `name` / `message` strings.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-error-objects>
    /// - <https://tc39.es/ecma262/#sec-native-error-types-used-in-this-standard>
    #[allow(unused_assignments)] // RootScope observes canonical slots through raw pointers.
    pub fn new(gc_heap: &mut otter_gc::GcHeap) -> Result<Self, otter_gc::OutOfMemory> {
        // Bootstrap allocations run under GC stress from the first allocation.
        // Keep exactly one mutable slot for each value that can cross an
        // allocation; the collector rewrites those slots in place. Completed
        // class entries are rooted directly instead of copied into temporary
        // `Vec<Value>` snapshots that become stale after a moving collection.
        let mut error_proto_root = Value::undefined();
        let mut error_name_root = Value::undefined();
        let mut empty_root = Value::undefined();
        let mut to_string_root = Value::undefined();
        let mut stack_get_root = Value::undefined();
        let mut stack_set_root = Value::undefined();
        let mut error_ctor_root = Value::undefined();
        let mut proto_root = Value::undefined();
        let mut class_name_root = Value::undefined();
        let mut ctor_root = Value::undefined();
        let mut native_root = Value::undefined();
        let mut entries: Vec<(ErrorKind, ClassEntry)> = Vec::with_capacity(8);
        let mut roots = otter_gc::RootScope::new(gc_heap);
        // SAFETY: every slot above is declared before `roots`, so it outlives
        // the scope and remains at a stable stack address until construction
        // completes (or unwinds).
        unsafe {
            roots.add_value(&mut error_proto_root);
            roots.add_value(&mut error_name_root);
            roots.add_value(&mut empty_root);
            roots.add_value(&mut to_string_root);
            roots.add_value(&mut stack_get_root);
            roots.add_value(&mut stack_set_root);
            roots.add_value(&mut error_ctor_root);
            roots.add_value(&mut proto_root);
            roots.add_value(&mut class_name_root);
            roots.add_value(&mut ctor_root);
            roots.add_value(&mut native_root);
            roots.add_erased(
                (&mut entries as *mut Vec<(ErrorKind, ClassEntry)>).cast::<()>(),
                trace_class_entries,
            );
        }

        error_proto_root = Value::object(alloc_registry_object(gc_heap, &[])?);
        // §20.5.3.{4,5} — `Error.prototype.name = "Error"` and
        // `Error.prototype.message = ""` are data properties with
        // attributes `{ writable: true, enumerable: false,
        // configurable: true }`. The plain `set` path leaves
        // `enumerable: true` which fails every `name`/`message`
        // descriptor test in `built-ins/{Error,NativeErrors}/prototype/*`.
        error_name_root = Value::string(from_str_rooted("Error", gc_heap, &[&error_proto_root])?);
        empty_root = Value::string(from_str_rooted(
            "",
            gc_heap,
            &[&error_proto_root, &error_name_root],
        )?);
        // The string allocations above may have relocated the young prototype;
        // take its current handle from the rooted Value, and write through
        // `_in_place` so each define reflects any further relocation.
        let mut error_proto = error_proto_root
            .as_object()
            .expect("error prototype is an object");
        let _ = object::define_own_property_in_place(
            &mut error_proto,
            gc_heap,
            "name",
            PropertyDescriptor::data(error_name_root, true, false, true),
        );
        error_proto_root = Value::object(error_proto);
        let _ = object::define_own_property_in_place(
            &mut error_proto,
            gc_heap,
            "message",
            PropertyDescriptor::data(empty_root, true, false, true),
        );
        error_proto_root = Value::object(error_proto);

        // §20.5.3.4 Error.prototype.toString — install as a real
        // function-valued data property so `Error.prototype.toString`
        // is reachable, callable, and enforces the spec's
        // `Type(O) is not Object → TypeError` receiver check. The
        // single source-of-truth body lives in `error_prototype_to_string`.
        fn error_prototype_to_string(
            ctx: &mut NativeCtx<'_>,
            _args: &[Value],
        ) -> Result<Value, NativeError> {
            let receiver = *ctx.this_value();
            // §20.5.3.4 step 2 — Type(O) is not Object → TypeError.
            if !receiver.is_object() {
                return Err(NativeError::TypeError {
                    name: "Error.prototype.toString",
                    reason: "receiver must be an Object".to_string(),
                });
            }

            let context =
                ctx.execution_context()
                    .cloned()
                    .ok_or_else(|| NativeError::TypeError {
                        name: "Error.prototype.toString",
                        reason: "missing execution context".to_string(),
                    })?;
            let (interp, _) = ctx.interp_mut_and_context();
            let display = render_error_to_string_spec(interp, &context, &receiver).map_err(
                |err| match err {
                    crate::VmError::Uncaught => {
                        let value = match interp.take_error_detail() {
                            Some(crate::run_control::ErrorDetail::Uncaught(m)) => m,
                            _ => Default::default(),
                        };
                        NativeError::Thrown {
                            name: "Error.prototype.toString",
                            message: value.into(),
                        }
                    }
                    other => NativeError::TypeError {
                        name: "Error.prototype.toString",
                        reason: other.to_string(),
                    },
                },
            )?;
            let s = JsString::from_str(&display, ctx.heap_mut()).map_err(|err| {
                NativeError::TypeError {
                    name: "Error.prototype.toString",
                    reason: err.to_string(),
                }
            })?;
            Ok(Value::string(s))
        }
        to_string_root = Value::native_function(native_static_with_roots(
            gc_heap,
            "toString",
            0,
            error_prototype_to_string,
            &[&error_proto_root],
        )?);
        let mut error_proto = error_proto_root
            .as_object()
            .expect("error prototype is an object");
        let _ = object::define_own_property_in_place(
            &mut error_proto,
            gc_heap,
            "toString",
            PropertyDescriptor::data(to_string_root, true, false, true),
        );
        error_proto_root = Value::object(error_proto);
        // `get`/`set Error.prototype.stack` — the Error Stacks proposal
        // (`sec-get-error.prototype.stack`). `stack` is an accessor on
        // `%Error.prototype%` (non-enumerable, configurable). The getter
        // returns an implementation-defined string for objects with an
        // `[[ErrorData]]` internal slot (modelled by an error prototype
        // in the chain) and `undefined` otherwise; the setter installs an
        // own data property on the receiver via
        // SetterThatIgnoresPrototypeProperties.
        fn error_stack_get(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
            let receiver = *ctx.this_value();
            // step 2 — E is not an Object → TypeError. A Proxy is an
            // Object even though it is not an ordinary `JsObject`.
            if !receiver.is_object_type() {
                return Err(NativeError::TypeError {
                    name: "get Error.prototype.stack",
                    reason: "receiver must be an Object".to_string(),
                });
            }
            // step 3 — no [[ErrorData]] → undefined. The slot is an exact
            // per-instance marker set by an error constructor, so a
            // `Proxy` / non-error object and a plain
            // `Object.create(Error.prototype)` all return undefined.
            let has_error_data = receiver
                .as_object()
                .is_some_and(|obj| crate::object::has_error_data(obj, ctx.heap()));
            if !has_error_data {
                return Ok(Value::undefined());
            }
            // step 4 — implementation-defined stack string. The header
            // is the error's `toString` form (`Name: message`); when
            // construction captured a call stack, append V8-style frame
            // lines `    at <fn> (<module>:<line>:<col>)`.
            let mut rendered = render_error_to_string(&receiver, ctx.heap());
            if let Some(obj) = receiver.as_object()
                && let Some(frames) = crate::object::error_stack_frames(obj, ctx.heap())
            {
                append_stack_frames(&mut rendered, &frames, ctx.interp_mut());
            }
            let s = JsString::from_str(&rendered, ctx.heap_mut()).map_err(|err| {
                NativeError::TypeError {
                    name: "get Error.prototype.stack",
                    reason: err.to_string(),
                }
            })?;
            Ok(Value::string(s))
        }
        fn error_stack_set(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
            let receiver = *ctx.this_value();
            let value = args.first().copied().unwrap_or_else(Value::undefined);
            // step 2 — E is not an Object → TypeError.
            if !receiver.is_object_type() {
                return Err(NativeError::TypeError {
                    name: "set Error.prototype.stack",
                    reason: "receiver must be an Object".to_string(),
                });
            }
            // step 3 — v is not a String → TypeError.
            if !value.is_string() {
                return Err(NativeError::TypeError {
                    name: "set Error.prototype.stack",
                    reason: "stack value must be a String".to_string(),
                });
            }
            let context =
                ctx.execution_context()
                    .cloned()
                    .ok_or_else(|| NativeError::TypeError {
                        name: "set Error.prototype.stack",
                        reason: "missing execution context".to_string(),
                    })?;
            let interp = ctx.interp_mut();
            // SetterThatIgnoresPrototypeProperties step 2 — setting on
            // %Error.prototype% itself throws.
            let home = interp.constructor_prototype_value("Error").ok();
            if let Some(home) = home
                && crate::abstract_ops::same_value(&home, &receiver, interp.gc_heap())
            {
                return Err(NativeError::TypeError {
                    name: "set Error.prototype.stack",
                    reason: "cannot set stack on Error.prototype".to_string(),
                });
            }
            fn map_err(interp: &mut crate::Interpreter, err: crate::VmError) -> NativeError {
                match err {
                    crate::VmError::Uncaught => {
                        let value = match interp.take_error_detail() {
                            Some(crate::run_control::ErrorDetail::Uncaught(m)) => m,
                            _ => Default::default(),
                        };
                        NativeError::Thrown {
                            name: "set Error.prototype.stack",
                            message: value.into(),
                        }
                    }
                    other => NativeError::TypeError {
                        name: "set Error.prototype.stack",
                        reason: other.to_string(),
                    },
                }
            }
            // SetterThatIgnoresPrototypeProperties steps 3-5.
            let existing = match interp.ordinary_get_own_property_descriptor_value_runtime_rooted(
                &context,
                receiver,
                &crate::VmPropertyKey::String("stack"),
                0,
                &[&receiver, &value],
                &[],
            ) {
                Ok(v) => v,
                Err(e) => return Err(map_err(interp, e)),
            };
            match existing {
                // step 4 — no own "stack": CreateDataPropertyOrThrow.
                None => {
                    if let Err(e) =
                        interp.create_data_property_or_throw(&context, receiver, "stack", value)
                    {
                        return Err(map_err(interp, e));
                    }
                }
                // step 5 — own "stack" exists: Set(this, p, v, true).
                Some(_) => {
                    if let Err(e) =
                        interp.array_set_property_throwing(&context, receiver, "stack", value)
                    {
                        return Err(map_err(interp, e));
                    }
                }
            }
            Ok(Value::undefined())
        }
        stack_get_root = Value::native_function(native_static_with_roots(
            gc_heap,
            "get stack",
            0,
            error_stack_get,
            &[&error_proto_root],
        )?);
        stack_set_root = Value::native_function(native_static_with_roots(
            gc_heap,
            "set stack",
            1,
            error_stack_set,
            &[&error_proto_root],
        )?);
        let mut error_proto = error_proto_root
            .as_object()
            .expect("error prototype is an object");
        let _ = object::define_own_property_in_place(
            &mut error_proto,
            gc_heap,
            "stack",
            PropertyDescriptor::accessor(Some(stack_get_root), Some(stack_set_root), false, true),
        );
        error_proto_root = Value::object(error_proto);

        // §20.5.3.4 Error.prototype.toString is intercepted by
        // `object_prototype_intercept` in the dispatcher when the
        // receiver's prototype chain includes any error prototype.
        // The single source of truth lives in
        // [`render_error_to_string`] below — both `e.toString()`
        // dispatch and the unwind diagnostic call it.
        // <https://tc39.es/ecma262/#sec-error.prototype.tostring>

        // §20.5.3 / §20.5.6 — every native error constructor has
        // own `length` (the formal-parameter count, default `1` for
        // `Error` and each subclass; `2` for `AggregateError`) and
        // `name` (the class name) as non-enumerable, non-writable,
        // configurable data properties. Same shape as every other
        // built-in function object per §17 ("Built-in Function
        // Objects" general property requirements).
        //
        // Without these descriptors `TypeError.name === undefined`
        // and the test262 `assert.throws(TypeError, …)` harness
        // can't distinguish thrown constructors, breaking ~28+
        // strict-mode caller / arguments tests.
        fn install_ctor_metadata(
            ctor: &mut JsObject,
            name: &str,
            length: i32,
            gc_heap: &mut otter_gc::GcHeap,
        ) -> Result<(), otter_gc::OutOfMemory> {
            let mut name_root = Value::undefined();
            let mut roots = otter_gc::RootScope::new(gc_heap);
            // SAFETY: `ctor` belongs to the caller and `name_root` is declared
            // before the scope, so both slots remain live and stable until the
            // guard is dropped.
            unsafe {
                roots.add_object(ctor);
                roots.add_value(&mut name_root);
            }
            name_root = Value::string(JsString::from_str(name, gc_heap)?);
            let _ = object::define_own_property_in_place(
                ctor,
                gc_heap,
                "name",
                PropertyDescriptor::data(name_root, false, false, true),
            );
            let _ = object::define_own_property_in_place(
                ctor,
                gc_heap,
                "length",
                PropertyDescriptor::data(
                    Value::number(NumberValue::from_i32(length)),
                    false,
                    false,
                    true,
                ),
            );
            Ok(())
        }

        // §20.5.1.1 / §20.5.6.1.1 NativeError(message, options) —
        // each constructor allocates an instance with its prototype
        // and stamps `message` (when provided), then performs
        // [`InstallErrorCause`] when `options` is an object with
        // an own `cause` property. The seven static dispatchers
        // below close over their `ErrorKind` so the shared
        // [`make_instance_native`] body can look up the realm
        // registry from the live `NativeCtx`.
        fn make_instance_native(
            ctx: &mut NativeCtx<'_>,
            kind: ErrorKind,
            args: &[Value],
        ) -> Result<Value, NativeError> {
            fn map_vm_err(
                ctx: &mut NativeCtx<'_>,
                kind: ErrorKind,
                err: crate::VmError,
            ) -> NativeError {
                match err {
                    crate::VmError::Uncaught => {
                        let value = match ctx.interp_mut().take_error_detail() {
                            Some(crate::run_control::ErrorDetail::Uncaught(m)) => m,
                            _ => Default::default(),
                        };
                        NativeError::Thrown {
                            name: kind.class_name(),
                            message: value.into(),
                        }
                    }
                    other => NativeError::TypeError {
                        name: kind.class_name(),
                        reason: other.to_string(),
                    },
                }
            }
            let context =
                ctx.execution_context()
                    .cloned()
                    .ok_or_else(|| NativeError::TypeError {
                        name: kind.class_name(),
                        reason: "missing execution context".to_string(),
                    })?;
            // §20.5.1.1 step 3 — when `message` is not undefined,
            // `msg = ? ToString(message)`.
            let message = if let Some(v) = args.first() {
                if v.is_undefined() {
                    None
                } else {
                    Some(match ctx.interp_mut().coerce_to_string(&context, v) {
                        Ok(v) => v,
                        Err(e) => return Err(map_vm_err(ctx, kind, e)),
                    })
                }
            } else {
                None
            };
            // §20.5.6.1.1 step 4 — install cause from
            // `options[1]` (or `options[2]` for AggregateError).
            let options = match kind {
                ErrorKind::AggregateError => args.get(2),
                _ => args.get(1),
            };
            let cause = match read_options_cause(ctx, &context, options) {
                Ok(v) => v,
                Err(e) => return Err(map_vm_err(ctx, kind, e)),
            };
            let registry = ctx.interp_mut().error_classes_clone();
            let mut extra_roots = Vec::with_capacity(1);
            if let Some(cause) = &cause {
                extra_roots.push(cause);
            }
            let obj = registry
                .make_instance_native_rooted(ctx, kind, message.as_deref(), &extra_roots, &[args])
                .map_err(|err| NativeError::TypeError {
                    name: kind.class_name(),
                    reason: err.to_string(),
                })?;
            if let Some(proto) =
                crate::bootstrap::native_new_target_prototype(ctx, kind.class_name())?
            {
                let _ = object::set_prototype_value(obj, ctx.heap_mut(), Some(proto));
            }
            if let Some(cause) = cause {
                install_error_cause(obj, cause, ctx.heap_mut());
            }
            // Capture the construction-site JS call stack for
            // `Error.prototype.stack` (matches V8: frames recorded
            // eagerly at construction, bounded by `Error.stackTraceLimit`,
            // formatted lazily on first `.stack` access).
            let limit = stack_trace_limit(ctx);
            if limit > 0 {
                let mut frames = ctx.interp_mut().capture_active_frames(&context);
                if frames.len() > limit {
                    frames.truncate(limit);
                }
                if !frames.is_empty() {
                    object::set_error_stack_frames(obj, ctx.heap_mut(), frames);
                }
            }
            Ok(Value::object(obj))
        }

        /// §7.3.13 HasProperty + §7.3.2 Get for the `cause` field
        /// of the constructor's options bag. Returns `None` when
        /// `options` is missing / non-object, or when `cause` is
        /// not an own / inherited property of the options bag.
        fn read_options_cause(
            ctx: &mut NativeCtx<'_>,
            context: &ExecutionContext,
            options: Option<&Value>,
        ) -> Result<Option<Value>, crate::VmError> {
            let Some(options) = options else {
                return Ok(None);
            };
            if !options.is_object_type() {
                return Ok(None);
            }
            let key = crate::VmPropertyKey::String("cause");
            let interp = ctx.interp_mut();
            if !interp.ordinary_has_property_value(context, *options, &key, 0)? {
                return Ok(None);
            }
            match interp.ordinary_get_value(context, *options, *options, &key, 0)? {
                crate::VmGetOutcome::Value(value) => Ok(Some(value)),
                crate::VmGetOutcome::InvokeGetter { getter } => {
                    let value = interp.run_callable_sync(
                        context,
                        &getter,
                        *options,
                        smallvec::SmallVec::<[Value; 8]>::new(),
                    )?;
                    Ok(Some(value))
                }
            }
        }

        /// §20.5.6.1.1 InstallErrorCause step 1.b —
        /// `CreateNonEnumerableDataPropertyOrThrow(O, "cause", cause)`.
        /// Spec property descriptor: writable, **non-enumerable**,
        /// configurable.
        fn install_error_cause(obj: JsObject, cause: Value, gc_heap: &mut otter_gc::GcHeap) {
            let _ = object::define_own_property(
                obj,
                gc_heap,
                "cause",
                PropertyDescriptor::data(cause, true, false, true),
            );
        }

        fn ctor_error(c: &mut NativeCtx<'_>, a: &[Value]) -> Result<Value, NativeError> {
            make_instance_native(c, ErrorKind::Error, a)
        }
        fn ctor_type(c: &mut NativeCtx<'_>, a: &[Value]) -> Result<Value, NativeError> {
            make_instance_native(c, ErrorKind::TypeError, a)
        }
        fn ctor_range(c: &mut NativeCtx<'_>, a: &[Value]) -> Result<Value, NativeError> {
            make_instance_native(c, ErrorKind::RangeError, a)
        }
        fn ctor_syntax(c: &mut NativeCtx<'_>, a: &[Value]) -> Result<Value, NativeError> {
            make_instance_native(c, ErrorKind::SyntaxError, a)
        }
        fn ctor_reference(c: &mut NativeCtx<'_>, a: &[Value]) -> Result<Value, NativeError> {
            make_instance_native(c, ErrorKind::ReferenceError, a)
        }
        fn ctor_uri(c: &mut NativeCtx<'_>, a: &[Value]) -> Result<Value, NativeError> {
            make_instance_native(c, ErrorKind::URIError, a)
        }
        fn ctor_eval(c: &mut NativeCtx<'_>, a: &[Value]) -> Result<Value, NativeError> {
            make_instance_native(c, ErrorKind::EvalError, a)
        }
        /// §20.5.7.1 AggregateError(errors, message [, options]).
        /// Differs from the regular native-error constructors:
        ///   - `errors` (arg 0) is an iterable to materialise as a
        ///     non-enumerable readonly `errors` own property,
        ///   - `message` is arg 1,
        ///   - `options.cause` lives at arg 2.
        fn ctor_aggregate(c: &mut NativeCtx<'_>, a: &[Value]) -> Result<Value, NativeError> {
            fn map_vm_err(c: &mut NativeCtx<'_>, err: crate::VmError) -> NativeError {
                match err {
                    crate::VmError::Uncaught => {
                        let value = match c.interp_mut().take_error_detail() {
                            Some(crate::run_control::ErrorDetail::Uncaught(m)) => m,
                            _ => Default::default(),
                        };
                        NativeError::Thrown {
                            name: "AggregateError",
                            message: value.into(),
                        }
                    }
                    other => NativeError::TypeError {
                        name: "AggregateError",
                        reason: other.to_string(),
                    },
                }
            }
            let context = c
                .execution_context()
                .cloned()
                .ok_or_else(|| NativeError::TypeError {
                    name: "AggregateError",
                    reason: "missing execution context".to_string(),
                })?;
            let message = if let Some(v) = a.get(1) {
                if v.is_undefined() {
                    None
                } else {
                    Some(match c.interp_mut().coerce_to_string(&context, v) {
                        Ok(v) => v,
                        Err(e) => return Err(map_vm_err(c, e)),
                    })
                }
            } else {
                None
            };
            let errors_arg = a.first().cloned().unwrap_or(Value::undefined());
            let cause = match read_options_cause(c, &context, a.get(2)) {
                Ok(v) => v,
                Err(e) => return Err(map_vm_err(c, e)),
            };

            // §20.5.7.1 step 4 — IterableToList(errors). Spec
            // throws `TypeError` for `null`/`undefined`. Spread
            // through a dense array fast path before falling back
            // to the iterator protocol.
            let errors_list = iterable_to_value_list(c, &context, &errors_arg)?;
            let registry = c.interp_mut().error_classes_clone();
            let mut extra_roots = Vec::with_capacity(2);
            extra_roots.push(&errors_arg);
            if let Some(cause) = &cause {
                extra_roots.push(cause);
            }
            let obj = registry
                .make_aggregate_instance_native_rooted(
                    c,
                    errors_list.as_slice(),
                    message.as_deref(),
                    &extra_roots,
                    &[a],
                )
                .map_err(|err| NativeError::TypeError {
                    name: "AggregateError",
                    reason: err.to_string(),
                })?;
            if let Some(proto) = crate::bootstrap::native_new_target_prototype(c, "AggregateError")?
            {
                let _ = object::set_prototype_value(obj, c.heap_mut(), Some(proto));
            }
            if let Some(cause) = cause {
                install_error_cause(obj, cause, c.heap_mut());
            }
            Ok(Value::object(obj))
        }

        /// IterableToList helper for AggregateError.
        ///
        /// Spec §7.4.3 IterableToList allocates an iterator and
        /// drains it through IteratorStep/IteratorValue.
        fn iterable_to_value_list(
            ctx: &mut NativeCtx<'_>,
            context: &ExecutionContext,
            value: &Value,
        ) -> Result<Vec<Value>, NativeError> {
            fn map_err(interp: &mut crate::Interpreter, err: crate::VmError) -> NativeError {
                match err {
                    crate::VmError::Uncaught => {
                        let value = match interp.take_error_detail() {
                            Some(crate::run_control::ErrorDetail::Uncaught(m)) => m,
                            _ => Default::default(),
                        };
                        NativeError::Thrown {
                            name: "AggregateError",
                            message: value.into(),
                        }
                    }
                    other => NativeError::TypeError {
                        name: "AggregateError",
                        reason: other.to_string(),
                    },
                }
            }
            if value.is_undefined() || value.is_null() {
                return Err(NativeError::TypeError {
                    name: "AggregateError",
                    reason: "errors argument is not iterable".to_string(),
                });
            }
            if let Some(arr) = value.as_array() {
                let heap = ctx.heap();
                return Ok(crate::array::with_elements(arr, heap, <[Value]>::to_vec));
            }
            let interp = ctx.interp_mut();
            let anchor_base = interp.push_iteration_anchor(*value) - 1;
            let result = (|interp: &mut crate::Interpreter| -> Result<Vec<Value>, crate::VmError> {
                let value = interp.iteration_anchor(anchor_base);
                let (iterator, next_method) = interp.get_iterator_sync(context, &value)?;
                let iterator_anchor = interp.push_iteration_anchor(iterator) - 1;
                let next_method_anchor = interp.push_iteration_anchor(next_method) - 1;
                let values_start = next_method_anchor + 1;
                let mut out_count = 0usize;
                loop {
                    let iterator = interp.iteration_anchor(iterator_anchor);
                    let next_method = interp.iteration_anchor(next_method_anchor);
                    match interp.iterator_step_sync(context, &iterator, &next_method)? {
                        Some(value) => {
                            interp.push_iteration_anchor(value);
                            out_count += 1;
                        }
                        None => break,
                    }
                }
                let mut out = Vec::with_capacity(out_count);
                for index in values_start..values_start + out_count {
                    out.push(interp.iteration_anchor(index));
                }
                Ok(out)
            })(interp);
            interp.pop_iteration_anchors_to(anchor_base);
            result.map_err(|e| map_err(interp, e))
        }

        // Error itself. §20.5.3 — `Error.prototype.constructor`
        // is the Error constructor, with attribute
        // `[[Configurable]]: true`, `[[Writable]]: true`,
        // `[[Enumerable]]: false`.
        error_ctor_root = Value::object(alloc_registry_object(
            gc_heap,
            &[&error_proto_root, &to_string_root],
        )?);
        // §20.5.2 — `Error.prototype` lives on the constructor as
        // `{ writable: false, enumerable: false, configurable: false }`.
        // §20.5.3 — `Error.prototype.constructor` is
        // `{ writable: true, enumerable: false, configurable: true }`.
        let mut error_ctor = error_ctor_root
            .as_object()
            .expect("Error constructor is an object");
        let error_proto = error_proto_root
            .as_object()
            .expect("error prototype is an object");
        let _ = object::define_own_property_in_place(
            &mut error_ctor,
            gc_heap,
            "prototype",
            PropertyDescriptor::data(Value::object(error_proto), false, false, false),
        );
        error_ctor_root = Value::object(error_ctor);
        let mut error_proto = error_proto_root
            .as_object()
            .expect("error prototype is an object");
        let _ = object::define_own_property_in_place(
            &mut error_proto,
            gc_heap,
            "constructor",
            PropertyDescriptor::data(Value::object(error_ctor), true, false, true),
        );
        error_proto_root = Value::object(error_proto);
        native_root = Value::native_function(native_constructor_static_with_roots(
            gc_heap,
            "Error",
            1,
            ctor_error,
            &[&error_proto_root, &error_ctor_root],
        )?);
        error_ctor = error_ctor_root
            .as_object()
            .expect("Error constructor is an object");
        object::set_constructor_native(error_ctor, gc_heap, native_root);
        install_ctor_metadata(&mut error_ctor, "Error", 1, gc_heap)?;
        error_ctor_root = Value::object(error_ctor);
        // §20.5.8.1 `Error.isError(arg)` — IsError(arg): an ordinary
        // Object carrying the `[[ErrorData]]` internal slot. Proxies and
        // plain objects shaped like errors return `false`; the marker is
        // per-instance, set only by an error constructor.
        fn error_is_error(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
            let arg = args.first().copied().unwrap_or_else(Value::undefined);
            let is_error = arg
                .as_object()
                .is_some_and(|obj| crate::object::has_error_data(obj, ctx.heap()));
            Ok(Value::boolean(is_error))
        }
        native_root = Value::native_function(native_static_with_roots(
            gc_heap,
            "isError",
            1,
            error_is_error,
            &[],
        )?);
        error_ctor = error_ctor_root
            .as_object()
            .expect("Error constructor is an object");
        let _ = object::define_own_property_in_place(
            &mut error_ctor,
            gc_heap,
            "isError",
            PropertyDescriptor::data(native_root, true, false, true),
        );
        error_ctor_root = Value::object(error_ctor);
        // V8 extension `Error.captureStackTrace(target[, constructorOpt])`:
        // record the current call stack onto `target.stack`. When
        // `constructorOpt` is a function, every frame at or above the
        // topmost frame named like that function is omitted (skip-until-
        // function), so a subclass constructor can hide its own frame.
        fn error_capture_stack_trace(
            ctx: &mut NativeCtx<'_>,
            args: &[Value],
        ) -> Result<Value, NativeError> {
            let target = args.first().copied().unwrap_or_else(Value::undefined);
            let Some(target_obj) = target.as_object() else {
                return Err(NativeError::TypeError {
                    name: "Error.captureStackTrace",
                    reason: "target must be an object".to_string(),
                });
            };
            let context =
                ctx.execution_context()
                    .cloned()
                    .ok_or_else(|| NativeError::TypeError {
                        name: "Error.captureStackTrace",
                        reason: "missing execution context".to_string(),
                    })?;
            let limit = stack_trace_limit(ctx);
            let mut frames = if limit > 0 {
                ctx.interp_mut().capture_active_frames(&context)
            } else {
                Vec::new()
            };
            // skip-until-function: when `constructorOpt` is a function,
            // drop every frame at or above the topmost frame belonging to
            // it (matched by function identity, the V8 semantics). This
            // lets a subclass constructor hide its own and inner frames.
            if let Some(ctor) = args.get(1).copied() {
                let skip_fid = ctor
                    .as_function()
                    .or_else(|| ctor.as_closure(ctx.heap()).map(|c| c.function_id()));
                if let Some(fid) = skip_fid
                    && let Some(pos) = frames.iter().position(|f| f.function_id == fid)
                {
                    frames.drain(0..=pos);
                }
            }
            if frames.len() > limit {
                frames.truncate(limit);
            }
            if crate::object::has_error_data(target_obj, ctx.heap()) {
                // Target inherits the `Error.prototype.stack` getter:
                // store frames and let it format lazily (V8 model).
                crate::object::set_error_stack_frames(target_obj, ctx.heap_mut(), frames);
            } else {
                // Plain object: install an own formatted `stack` data
                // property (the JSC-style eager shape, acceptable per the
                // TC39 capture-stack-trace proposal).
                let mut rendered = render_error_to_string(&target, ctx.heap());
                append_stack_frames(&mut rendered, &frames, ctx.interp_mut());
                let s = JsString::from_str(&rendered, ctx.heap_mut()).map_err(|err| {
                    NativeError::TypeError {
                        name: "Error.captureStackTrace",
                        reason: err.to_string(),
                    }
                })?;
                let _ = object::define_own_property(
                    target_obj,
                    ctx.heap_mut(),
                    "stack",
                    PropertyDescriptor::data(Value::string(s), true, false, true),
                );
            }
            Ok(Value::undefined())
        }
        native_root = Value::native_function(native_static_with_roots(
            gc_heap,
            "captureStackTrace",
            2,
            error_capture_stack_trace,
            &[],
        )?);
        error_ctor = error_ctor_root
            .as_object()
            .expect("Error constructor is an object");
        let _ = object::define_own_property_in_place(
            &mut error_ctor,
            gc_heap,
            "captureStackTrace",
            PropertyDescriptor::data(native_root, true, false, true),
        );
        error_ctor_root = Value::object(error_ctor);
        // V8 extension `Error.stackTraceLimit` (default 10): the maximum
        // number of frames captured for `Error.prototype.stack`. Writable
        // so user code can raise, lower, or disable (`0`) capture.
        let _ = object::define_own_property_in_place(
            &mut error_ctor,
            gc_heap,
            "stackTraceLimit",
            PropertyDescriptor::data(
                Value::number(NumberValue::from_i32(DEFAULT_STACK_TRACE_LIMIT as i32)),
                true,
                false,
                true,
            ),
        );
        error_ctor_root = Value::object(error_ctor);
        entries.push((
            ErrorKind::Error,
            ClassEntry {
                prototype: error_proto_root
                    .as_object()
                    .expect("error prototype is an object"),
                constructor: error_ctor_root
                    .as_object()
                    .expect("Error constructor is an object"),
            },
        ));

        // Subclasses. §20.5.6 — each `<NativeError>.prototype`
        // has a `constructor` own data property pointing back
        // to its constructor, mirroring the Error case.
        for &kind in &[
            ErrorKind::TypeError,
            ErrorKind::RangeError,
            ErrorKind::SyntaxError,
            ErrorKind::ReferenceError,
            ErrorKind::URIError,
            ErrorKind::EvalError,
            ErrorKind::AggregateError,
        ] {
            proto_root = Value::object(alloc_registry_object(gc_heap, &[])?);
            let mut proto = proto_root
                .as_object()
                .expect("native error prototype is an object");
            let error_proto = error_proto_root
                .as_object()
                .expect("error prototype is an object");
            object::set_prototype(proto, gc_heap, Some(error_proto));
            // §20.5.6.3.{2,3} — `<NativeError>.prototype.{name,message}`
            // share the same descriptor shape as `Error.prototype`'s.
            class_name_root =
                Value::string(from_str_rooted(kind.class_name(), gc_heap, &[&proto_root])?);
            empty_root = Value::string(from_str_rooted(
                "",
                gc_heap,
                &[&proto_root, &class_name_root],
            )?);
            // The string allocations may have relocated the young prototype;
            // refresh it from the rooted Value and write through `_in_place`.
            proto = proto_root
                .as_object()
                .expect("native error prototype is an object");
            let _ = object::define_own_property_in_place(
                &mut proto,
                gc_heap,
                "name",
                PropertyDescriptor::data(class_name_root, true, false, true),
            );
            proto_root = Value::object(proto);
            let _ = object::define_own_property_in_place(
                &mut proto,
                gc_heap,
                "message",
                PropertyDescriptor::data(empty_root, true, false, true),
            );
            proto_root = Value::object(proto);
            ctor_root = Value::object(alloc_registry_object(gc_heap, &[])?);
            // §20.5.6.{2,3} — same prototype/constructor shape.
            let mut ctor = ctor_root
                .as_object()
                .expect("native error constructor is an object");
            proto = proto_root
                .as_object()
                .expect("native error prototype is an object");
            let _ = object::define_own_property_in_place(
                &mut ctor,
                gc_heap,
                "prototype",
                PropertyDescriptor::data(Value::object(proto), false, false, false),
            );
            ctor_root = Value::object(ctor);
            let _ = object::define_own_property_in_place(
                &mut proto,
                gc_heap,
                "constructor",
                PropertyDescriptor::data(Value::object(ctor), true, false, true),
            );
            proto_root = Value::object(proto);
            // §20.5.7.2 — `AggregateError(errors, message?)` has
            // `length` 2; every other native error has `length` 1.
            let length = if kind == ErrorKind::AggregateError {
                2
            } else {
                1
            };
            let dispatcher: crate::native_function::NativeFastFn = match kind {
                ErrorKind::Error => ctor_error,
                ErrorKind::TypeError => ctor_type,
                ErrorKind::RangeError => ctor_range,
                ErrorKind::SyntaxError => ctor_syntax,
                ErrorKind::ReferenceError => ctor_reference,
                ErrorKind::URIError => ctor_uri,
                ErrorKind::EvalError => ctor_eval,
                ErrorKind::AggregateError => ctor_aggregate,
            };
            native_root = Value::native_function(native_constructor_static_with_roots(
                gc_heap,
                kind.class_name(),
                length as u8,
                dispatcher,
                &[],
            )?);
            ctor = ctor_root
                .as_object()
                .expect("native error constructor is an object");
            object::set_constructor_native(ctor, gc_heap, native_root);
            install_ctor_metadata(&mut ctor, kind.class_name(), length, gc_heap)?;
            ctor_root = Value::object(ctor);
            entries.push((
                kind,
                ClassEntry {
                    prototype: proto_root
                        .as_object()
                        .expect("native error prototype is an object"),
                    constructor: ctor_root
                        .as_object()
                        .expect("native error constructor is an object"),
                },
            ));
        }

        let mut take = |target: ErrorKind| -> ClassEntry {
            let pos = entries
                .iter()
                .position(|(k, _)| *k == target)
                .expect("ErrorKind variants enumerated above");
            entries.swap_remove(pos).1
        };
        Ok(Self {
            error: take(ErrorKind::Error),
            type_error: take(ErrorKind::TypeError),
            range_error: take(ErrorKind::RangeError),
            syntax_error: take(ErrorKind::SyntaxError),
            reference_error: take(ErrorKind::ReferenceError),
            uri_error: take(ErrorKind::URIError),
            eval_error: take(ErrorKind::EvalError),
            aggregate_error: take(ErrorKind::AggregateError),
        })
    }

    fn entry(&self, kind: ErrorKind) -> &ClassEntry {
        match kind {
            ErrorKind::Error => &self.error,
            ErrorKind::TypeError => &self.type_error,
            ErrorKind::RangeError => &self.range_error,
            ErrorKind::SyntaxError => &self.syntax_error,
            ErrorKind::ReferenceError => &self.reference_error,
            ErrorKind::URIError => &self.uri_error,
            ErrorKind::EvalError => &self.eval_error,
            ErrorKind::AggregateError => &self.aggregate_error,
        }
    }

    /// Wire the realm-level prototype chain that requires
    /// `%Function.prototype%` and `%Object.prototype%` (which only
    /// exist after bootstrap), and register every native error
    /// constructor as an own data property of `globalThis`.
    ///
    /// # Algorithm
    /// Per ECMA-262 §20.5.6:
    ///   - `Error.[[Prototype]]` is `%Function.prototype%`.
    ///   - `Error.prototype.[[Prototype]]` is `%Object.prototype%`.
    ///   - `<NativeError>.[[Prototype]]` is `%Error%` (the constructor).
    ///   - `<NativeError>.prototype.[[Prototype]]` is `%Error.prototype%`
    ///     (already linked at registry-construction time).
    ///
    /// Constructors land on `globalThis` as `{ writable: true,
    /// enumerable: false, configurable: true }` per §17.
    pub(crate) fn finalize_after_bootstrap(
        &self,
        gc_heap: &mut otter_gc::GcHeap,
        function_prototype: JsObject,
        object_prototype: JsObject,
        global_this: JsObject,
    ) {
        // Link Error -> Function.prototype.
        object::set_prototype(self.error.constructor, gc_heap, Some(function_prototype));
        // Link Error.prototype -> Object.prototype.
        object::set_prototype(self.error.prototype, gc_heap, Some(object_prototype));
        // Link each subclass constructor -> Error.
        for entry in [
            &self.type_error,
            &self.range_error,
            &self.syntax_error,
            &self.reference_error,
            &self.uri_error,
            &self.eval_error,
            &self.aggregate_error,
        ] {
            object::set_prototype(entry.constructor, gc_heap, Some(self.error.constructor));
        }
        // Register globals.
        for (name, entry) in [
            ("Error", &self.error),
            ("TypeError", &self.type_error),
            ("RangeError", &self.range_error),
            ("SyntaxError", &self.syntax_error),
            ("ReferenceError", &self.reference_error),
            ("URIError", &self.uri_error),
            ("EvalError", &self.eval_error),
            ("AggregateError", &self.aggregate_error),
        ] {
            let _ = object::define_own_property(
                global_this,
                gc_heap,
                name,
                PropertyDescriptor::data(Value::object(entry.constructor), true, false, true),
            );
        }
    }

    /// Borrow the constructor `JsObject` for `kind`. Used to back
    /// `Op::LoadBuiltinError` so `e instanceof TypeError` finds a
    /// real constructor with a `prototype` own property.
    #[must_use]
    pub fn constructor(&self, kind: ErrorKind) -> JsObject {
        self.entry(kind).constructor
    }

    /// Borrow the `prototype` object for `kind`. Exposed for
    /// callers that build instances out-of-band through an explicit root
    /// contract (e.g. stack-rooted VM error throwable conversion).
    #[must_use]
    pub fn prototype(&self, kind: ErrorKind) -> JsObject {
        self.entry(kind).prototype
    }

    /// Allocate a fresh error instance of the given kind with the supplied
    /// message through the native root contract.
    ///
    /// # Algorithm
    /// 1. Allocate a plain `JsObject`.
    /// 2. Link its `[[Prototype]]` to `kind`'s prototype, so it
    ///    inherits `name` and `message` defaults plus
    ///    `Error.prototype.toString` once that is wired (task 61
    ///    backfills the toString implementation).
    /// 3. When `message` is `Some`, stamp it as an own property
    ///    so `e.message` resolves to the constructor argument
    ///    rather than the empty default. `None` leaves the
    ///    inherited empty-string default in place per
    ///    §20.5.1.1 step 4 (omitted argument).
    ///
    /// # Errors
    /// Returns [`StringError::OutOfMemory`] if the message string
    /// allocation fails.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-error-message>
    pub(crate) fn make_instance_native_rooted(
        &self,
        ctx: &mut NativeCtx<'_>,
        kind: ErrorKind,
        message: Option<&str>,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<JsObject, otter_gc::OutOfMemory> {
        let proto = self.prototype(kind);
        let proto_value = Value::object(proto);
        let mut roots = Vec::with_capacity(value_roots.len() + 1);
        roots.push(&proto_value);
        roots.extend_from_slice(value_roots);
        let obj = ctx
            .alloc_object_with_roots(roots.as_slice(), slice_roots)
            .map_err(|_| otter_gc::OutOfMemory::HeapCapExceeded {
                requested_bytes: 0,
                heap_limit_bytes: ctx.heap().max_heap_bytes(),
            })?;
        crate::object::set_prototype(obj, ctx.heap_mut(), Some(proto));
        // §20.5.* — the instance carries the `[[ErrorData]]` internal slot.
        crate::object::set_error_data(obj, ctx.heap_mut());
        if let Some(text) = message {
            let s = JsString::from_str(text, ctx.heap_mut())?;
            // §20.5.1.1 step 4.c — `msgDesc` is `{ [[Value]]: msg,
            // [[Writable]]: true, [[Enumerable]]: false,
            // [[Configurable]]: true }`. Going through ordinary
            // `set_property` would install an enumerable slot;
            // route through `define_own_property` so reflective
            // property descriptors match the spec.
            let _ = crate::object::define_own_property(
                obj,
                ctx.heap_mut(),
                "message",
                crate::object::PropertyDescriptor::data(Value::string(s), true, false, true),
            );
            let _ = value_roots;
            let _ = slice_roots;
        }
        Ok(obj)
    }

    /// §20.5.7.1 `AggregateError(errors, message?)` — allocate an
    /// AggregateError with the supplied errors list attached as the
    /// `errors` own property. The `errors` argument's elements are
    /// shallow-copied into a fresh `JsArray`.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-aggregate-error>
    pub(crate) fn make_aggregate_instance_native_rooted(
        &self,
        ctx: &mut NativeCtx<'_>,
        errors: &[Value],
        message: Option<&str>,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<JsObject, otter_gc::OutOfMemory> {
        let mut object_slice_roots = Vec::with_capacity(slice_roots.len() + 1);
        object_slice_roots.push(errors);
        object_slice_roots.extend_from_slice(slice_roots);
        let obj = self.make_instance_native_rooted(
            ctx,
            ErrorKind::AggregateError,
            message,
            value_roots,
            object_slice_roots.as_slice(),
        )?;
        let obj_value = Value::object(obj);
        let mut array_roots = Vec::with_capacity(value_roots.len() + 1);
        array_roots.push(&obj_value);
        array_roots.extend_from_slice(value_roots);
        let arr = ctx
            .array_from_elements_with_roots(
                errors.iter().cloned(),
                array_roots.as_slice(),
                slice_roots,
            )
            .map_err(|_| otter_gc::OutOfMemory::HeapCapExceeded {
                requested_bytes: 0,
                heap_limit_bytes: ctx.heap().max_heap_bytes(),
            })?;
        let _ = object::define_own_property(
            obj,
            ctx.heap_mut(),
            "errors",
            PropertyDescriptor::data(Value::array(arr), true, false, true),
        );
        Ok(obj)
    }
}
