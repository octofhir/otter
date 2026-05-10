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
//! - The registry never re-allocates after construction — calling
//!   `make_instance` reuses the per-kind prototype object as the
//!   instance's `[[Prototype]]`.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-error-objects>
//! - <https://tc39.es/ecma262/#sec-native-error-types-used-in-this-standard>
//! - <https://tc39.es/ecma262/#sec-error-message>
//! - <https://tc39.es/ecma262/#sec-error.prototype.tostring>

use crate::Value;
use crate::gc_trace::{GcRootVisitor, GcTrace};
use crate::native_function::NativeFunction;
use crate::number::NumberValue;
use crate::object::{self, JsObject, PropertyDescriptor};
use crate::string::{JsString, StringError, StringHeap};
use crate::{NativeCtx, NativeError};

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
#[must_use]
pub fn render_error_to_string(value: &Value, gc_heap: &otter_gc::GcHeap) -> String {
    let Value::Object(obj) = value else {
        return value.display_string();
    };
    let name = match crate::object::get(*obj, gc_heap, "name") {
        Some(Value::String(s)) => s.to_lossy_string(),
        Some(other) => other.display_string(),
        None => String::new(),
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
            entry.prototype.trace_gc_roots(visitor);
            entry.constructor.trace_gc_roots(visitor);
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
        let error_proto =
            crate::object::alloc_object(gc_heap).map_err(|_| StringError::OutOfMemory {
                requested_bytes: 0,
                heap_limit_bytes: 0,
            })?;
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
            // Step 2: Type(O) is not Object → TypeError.
            let Value::Object(_) = &receiver else {
                return Err(NativeError::TypeError {
                    name: "Error.prototype.toString",
                    reason: "receiver must be an Object".to_string(),
                });
            };
            let string_heap = ctx.interp_mut().string_heap_clone();
            let display = render_error_to_string(&receiver, ctx.heap_mut());
            let s = JsString::from_str(&display, &string_heap).map_err(|err| {
                NativeError::TypeError {
                    name: "Error.prototype.toString",
                    reason: err.to_string(),
                }
            })?;
            Ok(Value::String(s))
        }
        let to_string_native =
            NativeFunction::new_static(gc_heap, "toString", 0, error_prototype_to_string).map_err(
                |_| StringError::OutOfMemory {
                    requested_bytes: 0,
                    heap_limit_bytes: 0,
                },
            )?;
        let _ = object::define_own_property(
            error_proto,
            gc_heap,
            "toString",
            PropertyDescriptor::data(Value::NativeFunction(to_string_native), true, false, true),
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
        // and stamps `message` (when provided). The seven static
        // dispatchers below close over their `ErrorKind` so the
        // shared `make_instance_native` body can look up the realm
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
            let interp = ctx.interp_mut();
            let registry = interp.error_classes_clone();
            let string_heap = interp.string_heap_clone();
            let obj = registry
                .make_instance(kind, message.as_deref(), &string_heap, ctx.heap_mut())
                .map_err(|err| NativeError::TypeError {
                    name: kind.class_name(),
                    reason: err.to_string(),
                })?;
            Ok(Value::Object(obj))
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
        fn ctor_aggregate(c: &mut NativeCtx<'_>, a: &[Value]) -> Result<Value, NativeError> {
            make_instance_native(c, ErrorKind::AggregateError, a)
        }

        let mut entries: Vec<(ErrorKind, ClassEntry)> = Vec::with_capacity(7);
        // Error itself. §20.5.3 — `Error.prototype.constructor`
        // is the Error constructor, with attribute
        // `[[Configurable]]: true`, `[[Writable]]: true`,
        // `[[Enumerable]]: false`.
        let error_ctor =
            crate::object::alloc_object(gc_heap).map_err(|_| StringError::OutOfMemory {
                requested_bytes: 0,
                heap_limit_bytes: 0,
            })?;
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
        let error_call = NativeFunction::new_constructor_static(gc_heap, "Error", 1, ctor_error)
            .map_err(|_| StringError::OutOfMemory {
                requested_bytes: 0,
                heap_limit_bytes: 0,
            })?;
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
            let proto =
                crate::object::alloc_object(gc_heap).map_err(|_| StringError::OutOfMemory {
                    requested_bytes: 0,
                    heap_limit_bytes: 0,
                })?;
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
            let ctor =
                crate::object::alloc_object(gc_heap).map_err(|_| StringError::OutOfMemory {
                    requested_bytes: 0,
                    heap_limit_bytes: 0,
                })?;
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
            let native = NativeFunction::new_constructor_static(
                gc_heap,
                kind.class_name(),
                length as u8,
                dispatcher,
            )
            .map_err(|_| StringError::OutOfMemory {
                requested_bytes: 0,
                heap_limit_bytes: 0,
            })?;
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
    /// callers that build instances out-of-band (e.g. the
    /// dispatcher's TypeMismatch → throwable conversion in a
    /// later slice).
    #[must_use]
    pub fn prototype(&self, kind: ErrorKind) -> JsObject {
        self.entry(kind).prototype
    }

    /// Allocate a fresh error instance of the given kind with the
    /// supplied message.
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
    pub fn make_instance(
        &self,
        kind: ErrorKind,
        message: Option<&str>,
        heap: &StringHeap,
        gc_heap: &mut otter_gc::GcHeap,
    ) -> Result<JsObject, StringError> {
        let obj = crate::object::alloc_object(gc_heap).map_err(|_| StringError::OutOfMemory {
            requested_bytes: 0,
            heap_limit_bytes: 0,
        })?;
        crate::object::set_prototype(obj, gc_heap, Some(self.prototype(kind)));
        if let Some(text) = message {
            let s = JsString::from_str(text, heap)?;
            crate::object::set(obj, gc_heap, "message", Value::String(s));
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
    pub fn make_aggregate_instance(
        &self,
        errors: Vec<Value>,
        message: Option<&str>,
        heap: &StringHeap,
        gc_heap: &mut otter_gc::GcHeap,
    ) -> Result<JsObject, StringError> {
        let obj = self.make_instance(ErrorKind::AggregateError, message, heap, gc_heap)?;
        let arr =
            crate::array::from_elements(gc_heap, errors).map_err(|_| StringError::OutOfMemory {
                requested_bytes: 0,
                heap_limit_bytes: gc_heap.max_heap_bytes(),
            })?;
        crate::object::set(obj, gc_heap, "errors", Value::Array(arr));
        Ok(obj)
    }
}
