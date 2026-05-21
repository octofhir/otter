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

use crate::Value;
use crate::gc_trace::GcRootVisitor;
use crate::native_function::NativeFunction;
use crate::number::NumberValue;
use crate::object::{self, JsObject, PropertyDescriptor};
use crate::string::{JsString, StringError, StringHeap};
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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

fn oom() -> StringError {
    StringError::OutOfMemory {
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
) -> Result<JsObject, StringError> {
    let mut external_visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
        trace_value_roots(roots, visitor);
    };
    crate::object::alloc_object_with_roots(gc_heap, &mut external_visit).map_err(|_| oom())
}

fn native_static_with_roots(
    gc_heap: &mut otter_gc::GcHeap,
    name: &'static str,
    length: u8,
    call: crate::native_function::NativeFastFn,
    roots: &[&Value],
) -> Result<NativeFunction, StringError> {
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
) -> Result<NativeFunction, StringError> {
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

fn class_entry_roots(entries: &[(ErrorKind, ClassEntry)]) -> Vec<Value> {
    let mut roots = Vec::with_capacity(entries.len() * 2);
    for (_, entry) in entries {
        roots.push(Value::Object(entry.prototype));
        roots.push(Value::Object(entry.constructor));
    }
    roots
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
        let outcome =
            interp.ordinary_get_value(context, receiver.clone(), receiver.clone(), &vm_key, 0)?;
        let value = match outcome {
            crate::VmGetOutcome::Value(v) => v,
            crate::VmGetOutcome::InvokeGetter { getter } => {
                let args: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::new();
                interp.run_callable_sync(context, &getter, receiver.clone(), args)?
            }
        };
        match value {
            Value::Undefined => Ok(default.to_string()),
            Value::Symbol(_) => Err(crate::VmError::TypeError {
                message: format!("Cannot convert a Symbol value to a string ('{key}')"),
            }),
            Value::String(s) => Ok(s.to_lossy_string()),
            Value::Null | Value::Boolean(_) | Value::Number(_) | Value::BigInt(_) => {
                Ok(value.display_string())
            }
            _ => {
                let primitive = interp.evaluate_to_primitive(
                    context,
                    &value,
                    crate::abstract_ops::ToPrimitiveHint::String,
                )?;
                match primitive {
                    Value::Symbol(_) => Err(crate::VmError::TypeError {
                        message: format!("Cannot convert a Symbol value to a string ('{key}')"),
                    }),
                    Value::String(s) => Ok(s.to_lossy_string()),
                    other => Ok(other.display_string()),
                }
            }
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
    let Value::Object(obj) = value else {
        return value.display_string();
    };
    // §20.5.3.4 defaults: `name` falls back to `"Error"` when
    // missing / `undefined`; `message` falls back to the empty
    // string. The synchronous render path (used by the unwind
    // diagnostic) cannot invoke accessors / `@@toPrimitive` —
    // `Error.prototype.toString` itself goes through
    // [`render_error_to_string_spec`] for that.
    let name = match crate::object::get(*obj, gc_heap, "name") {
        Some(Value::Undefined) | None => "Error".to_string(),
        Some(Value::String(s)) => s.to_lossy_string(),
        Some(other) => other.display_string(),
    };
    let message = match crate::object::get(*obj, gc_heap, "message") {
        Some(Value::String(s)) => s.to_lossy_string(),
        Some(Value::Undefined) | None => String::new(),
        Some(other) => other.display_string(),
    };
    match (name.is_empty(), message.is_empty()) {
        (true, true) => String::new(),
        (false, true) => name,
        (true, false) => message,
        (false, false) => format!("{name}: {message}"),
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
    pub fn new(heap: &StringHeap, gc_heap: &mut otter_gc::GcHeap) -> Result<Self, StringError> {
        let error_proto = alloc_registry_object(gc_heap, &[])?;
        let error_proto_root = Value::Object(error_proto);
        // §20.5.3.{4,5} — `Error.prototype.name = "Error"` and
        // `Error.prototype.message = ""` are data properties with
        // attributes `{ writable: true, enumerable: false,
        // configurable: true }`. The plain `set` path leaves
        // `enumerable: true` which fails every `name`/`message`
        // descriptor test in `built-ins/{Error,NativeErrors}/prototype/*`.
        let error_name = JsString::from_str("Error", heap)?;
        let empty = JsString::from_str("", heap)?;
        let _ = object::define_own_property(
            error_proto,
            gc_heap,
            "name",
            PropertyDescriptor::data(Value::String(error_name), true, false, true),
        );
        let _ = object::define_own_property(
            error_proto,
            gc_heap,
            "message",
            PropertyDescriptor::data(Value::String(empty), true, false, true),
        );

        // §20.5.3.4 Error.prototype.toString — install as a real
        // function-valued data property so `Error.prototype.toString`
        // is reachable, callable, and enforces the spec's
        // `Type(O) is not Object → TypeError` receiver check. The
        // single source-of-truth body lives in `error_prototype_to_string`.
        fn error_prototype_to_string(
            ctx: &mut NativeCtx<'_>,
            _args: &[Value],
        ) -> Result<Value, NativeError> {
            let receiver = ctx.this_value().clone();
            // §20.5.3.4 step 2 — Type(O) is not Object → TypeError.
            let Value::Object(_) = &receiver else {
                return Err(NativeError::TypeError {
                    name: "Error.prototype.toString",
                    reason: "receiver must be an Object".to_string(),
                });
            };
            let string_heap = ctx.interp_mut().string_heap_clone();
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
                    crate::VmError::Uncaught { value } => NativeError::Thrown {
                        name: "Error.prototype.toString",
                        message: value,
                    },
                    other => NativeError::TypeError {
                        name: "Error.prototype.toString",
                        reason: other.to_string(),
                    },
                },
            )?;
            let s = JsString::from_str(&display, &string_heap).map_err(|err| {
                NativeError::TypeError {
                    name: "Error.prototype.toString",
                    reason: err.to_string(),
                }
            })?;
            Ok(Value::String(s))
        }
        let to_string_native = native_static_with_roots(
            gc_heap,
            "toString",
            0,
            error_prototype_to_string,
            &[&error_proto_root],
        )?;
        let to_string_root = Value::NativeFunction(to_string_native);
        let _ = object::define_own_property(
            error_proto,
            gc_heap,
            "toString",
            PropertyDescriptor::data(to_string_root.clone(), true, false, true),
        );
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
            ctor: JsObject,
            name: &str,
            length: i32,
            heap: &StringHeap,
            gc_heap: &mut otter_gc::GcHeap,
        ) -> Result<(), StringError> {
            let name_str = JsString::from_str(name, heap)?;
            let _ = object::define_own_property(
                ctor,
                gc_heap,
                "name",
                PropertyDescriptor::data(Value::String(name_str), false, false, true),
            );
            let _ = object::define_own_property(
                ctor,
                gc_heap,
                "length",
                PropertyDescriptor::data(
                    Value::Number(NumberValue::from_i32(length)),
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
            // §20.5.1.1 step 3 — when `message` is not undefined,
            // `msg = ? ToString(message)`. Foundation: handle the
            // common primitive cases inline; full ToString
            // (with `Symbol.toPrimitive`) lands in a follow-up.
            let message = match args.first() {
                None | Some(Value::Undefined) => None,
                Some(Value::String(s)) => Some(s.to_lossy_string()),
                Some(Value::Symbol(_)) => {
                    return Err(NativeError::TypeError {
                        name: kind.class_name(),
                        reason: "Cannot convert a Symbol value to a string".to_string(),
                    });
                }
                Some(v) => Some(v.display_string()),
            };
            // §20.5.6.1.1 step 4 — install cause from
            // `options[1]` (or `options[2]` for AggregateError).
            let options = match kind {
                ErrorKind::AggregateError => args.get(2),
                _ => args.get(1),
            };
            let cause = read_options_cause(options, ctx.heap());
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
            Ok(Value::Object(obj))
        }

        /// §7.3.13 HasProperty + §7.3.2 Get for the `cause` field
        /// of the constructor's options bag. Returns `None` when
        /// `options` is missing / non-object, or when `cause` is
        /// not an own / inherited property of the options bag.
        fn read_options_cause(options: Option<&Value>, heap: &otter_gc::GcHeap) -> Option<Value> {
            let opt_obj = match options? {
                Value::Object(obj) => *obj,
                _ => return None,
            };
            // `get` walks the prototype chain, matching
            // HasProperty's behaviour per spec. A hole or missing
            // entry returns `None`.
            object::get(opt_obj, heap, "cause")
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
            let message = match a.get(1) {
                None | Some(Value::Undefined) => None,
                Some(Value::String(s)) => Some(s.to_lossy_string()),
                Some(Value::Symbol(_)) => {
                    return Err(NativeError::TypeError {
                        name: "AggregateError",
                        reason: "Cannot convert a Symbol value to a string".to_string(),
                    });
                }
                Some(v) => Some(v.display_string()),
            };
            let errors_arg = a.first().cloned().unwrap_or(Value::Undefined);
            let cause = read_options_cause(a.get(2), c.heap());

            // §20.5.7.1 step 4 — IterableToList(errors). Spec
            // throws `TypeError` for `null`/`undefined`. Spread
            // through a dense array fast path before falling back
            // to the iterator protocol.
            let errors_list = iterable_to_value_list(c, &errors_arg)?;
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
            Ok(Value::Object(obj))
        }

        /// IterableToList helper for AggregateError.
        ///
        /// Spec §7.4.3 IterableToList allocates an iterator and
        /// drains it through IteratorStep/IteratorValue. The
        /// foundation slice covers the common `Array` argument
        /// (the only shape the conformance corpus exercises
        /// extensively); other iterables fall back to a TypeError
        /// until the IteratorStep protocol lands as a
        /// reusable native helper.
        fn iterable_to_value_list(
            ctx: &mut NativeCtx<'_>,
            value: &Value,
        ) -> Result<Vec<Value>, NativeError> {
            match value {
                Value::Undefined | Value::Null => Err(NativeError::TypeError {
                    name: "AggregateError",
                    reason: "errors argument is not iterable".to_string(),
                }),
                Value::Array(arr) => {
                    let heap = ctx.heap();
                    Ok(crate::array::with_elements(*arr, heap, <[Value]>::to_vec))
                }
                _ => Err(NativeError::TypeError {
                    name: "AggregateError",
                    reason: "errors argument must be an Array (foundation slice)".to_string(),
                }),
            }
        }

        let mut entries: Vec<(ErrorKind, ClassEntry)> = Vec::with_capacity(7);
        // Error itself. §20.5.3 — `Error.prototype.constructor`
        // is the Error constructor, with attribute
        // `[[Configurable]]: true`, `[[Writable]]: true`,
        // `[[Enumerable]]: false`.
        let error_ctor = alloc_registry_object(gc_heap, &[&error_proto_root, &to_string_root])?;
        let error_ctor_root = Value::Object(error_ctor);
        // §20.5.2 — `Error.prototype` lives on the constructor as
        // `{ writable: false, enumerable: false, configurable: false }`.
        // §20.5.3 — `Error.prototype.constructor` is
        // `{ writable: true, enumerable: false, configurable: true }`.
        let _ = object::define_own_property(
            error_ctor,
            gc_heap,
            "prototype",
            PropertyDescriptor::data(Value::Object(error_proto), false, false, false),
        );
        let _ = object::define_own_property(
            error_proto,
            gc_heap,
            "constructor",
            PropertyDescriptor::data(Value::Object(error_ctor), true, false, true),
        );
        let error_call = native_constructor_static_with_roots(
            gc_heap,
            "Error",
            1,
            ctor_error,
            &[&error_proto_root, &error_ctor_root],
        )?;
        object::set_constructor_native(error_ctor, gc_heap, Value::NativeFunction(error_call));
        install_ctor_metadata(error_ctor, "Error", 1, heap, gc_heap)?;
        entries.push((
            ErrorKind::Error,
            ClassEntry {
                prototype: error_proto,
                constructor: error_ctor,
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
            let mut roots = class_entry_roots(&entries);
            roots.push(Value::Object(error_proto));
            let root_refs: Vec<&Value> = roots.iter().collect();
            let proto = alloc_registry_object(gc_heap, root_refs.as_slice())?;
            let proto_root = Value::Object(proto);
            object::set_prototype(proto, gc_heap, Some(error_proto));
            // §20.5.6.3.{2,3} — `<NativeError>.prototype.{name,message}`
            // share the same descriptor shape as `Error.prototype`'s.
            let class_name = JsString::from_str(kind.class_name(), heap)?;
            let _ = object::define_own_property(
                proto,
                gc_heap,
                "name",
                PropertyDescriptor::data(Value::String(class_name), true, false, true),
            );
            let empty = JsString::from_str("", heap)?;
            let _ = object::define_own_property(
                proto,
                gc_heap,
                "message",
                PropertyDescriptor::data(Value::String(empty), true, false, true),
            );
            let mut roots = class_entry_roots(&entries);
            roots.push(Value::Object(error_proto));
            roots.push(proto_root.clone());
            let root_refs: Vec<&Value> = roots.iter().collect();
            let ctor = alloc_registry_object(gc_heap, root_refs.as_slice())?;
            let ctor_root = Value::Object(ctor);
            // §20.5.6.{2,3} — same prototype/constructor shape.
            let _ = object::define_own_property(
                ctor,
                gc_heap,
                "prototype",
                PropertyDescriptor::data(Value::Object(proto), false, false, false),
            );
            let _ = object::define_own_property(
                proto,
                gc_heap,
                "constructor",
                PropertyDescriptor::data(Value::Object(ctor), true, false, true),
            );
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
            let mut roots = class_entry_roots(&entries);
            roots.push(Value::Object(error_proto));
            roots.push(proto_root);
            roots.push(ctor_root);
            let root_refs: Vec<&Value> = roots.iter().collect();
            let native = native_constructor_static_with_roots(
                gc_heap,
                kind.class_name(),
                length as u8,
                dispatcher,
                root_refs.as_slice(),
            )?;
            object::set_constructor_native(ctor, gc_heap, Value::NativeFunction(native));
            install_ctor_metadata(ctor, kind.class_name(), length, heap, gc_heap)?;
            entries.push((
                kind,
                ClassEntry {
                    prototype: proto,
                    constructor: ctor,
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
                PropertyDescriptor::data(Value::Object(entry.constructor), true, false, true),
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
    ) -> Result<JsObject, StringError> {
        let proto = self.prototype(kind);
        let proto_value = Value::Object(proto);
        let mut roots = Vec::with_capacity(value_roots.len() + 1);
        roots.push(&proto_value);
        roots.extend_from_slice(value_roots);
        let obj = ctx
            .alloc_object_with_roots(roots.as_slice(), slice_roots)
            .map_err(|_| StringError::OutOfMemory {
                requested_bytes: 0,
                heap_limit_bytes: ctx.heap().max_heap_bytes(),
            })?;
        crate::object::set_prototype(obj, ctx.heap_mut(), Some(proto));
        if let Some(text) = message {
            let heap = ctx.interp_mut().string_heap_clone();
            let s = JsString::from_str(text, &heap)?;
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
                crate::object::PropertyDescriptor::data(Value::String(s), true, false, true),
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
    ) -> Result<JsObject, StringError> {
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
        let obj_value = Value::Object(obj);
        let mut array_roots = Vec::with_capacity(value_roots.len() + 1);
        array_roots.push(&obj_value);
        array_roots.extend_from_slice(value_roots);
        let arr = ctx
            .array_from_elements_with_roots(
                errors.iter().cloned(),
                array_roots.as_slice(),
                slice_roots,
            )
            .map_err(|_| StringError::OutOfMemory {
                requested_bytes: 0,
                heap_limit_bytes: ctx.heap().max_heap_bytes(),
            })?;
        ctx.set_property(obj, "errors", Value::Array(arr))
            .map_err(|_| StringError::OutOfMemory {
                requested_bytes: 0,
                heap_limit_bytes: ctx.heap().max_heap_bytes(),
            })?;
        Ok(obj)
    }
}
