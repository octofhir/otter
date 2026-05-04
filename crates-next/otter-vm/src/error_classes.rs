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
use crate::object::JsObject;
use crate::string::{JsString, StringError, StringHeap};

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
        let error_name = JsString::from_str("Error", heap)?;
        let empty = JsString::from_str("", heap)?;
        crate::object::set(error_proto, gc_heap, "name", Value::String(error_name));
        crate::object::set(error_proto, gc_heap, "message", Value::String(empty));
        // §20.5.3.4 Error.prototype.toString is intercepted by
        // `object_prototype_intercept` in the dispatcher when the
        // receiver's prototype chain includes any error prototype.
        // The single source of truth lives in
        // [`render_error_to_string`] below — both `e.toString()`
        // dispatch and the unwind diagnostic call it.
        // <https://tc39.es/ecma262/#sec-error.prototype.tostring>

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
        crate::object::set(error_ctor, gc_heap, "prototype", Value::Object(error_proto));
        crate::object::set(
            error_proto,
            gc_heap,
            "constructor",
            Value::Object(error_ctor),
        );
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
            crate::object::set_prototype(proto, gc_heap, Some(error_proto));
            let class_name = JsString::from_str(kind.class_name(), heap)?;
            crate::object::set(proto, gc_heap, "name", Value::String(class_name));
            let ctor =
                crate::object::alloc_object(gc_heap).map_err(|_| StringError::OutOfMemory {
                    requested_bytes: 0,
                    heap_limit_bytes: 0,
                })?;
            crate::object::set(ctor, gc_heap, "prototype", Value::Object(proto));
            crate::object::set(proto, gc_heap, "constructor", Value::Object(ctor));
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
        let arr = crate::array::JsArray::from_elements(errors);
        crate::object::set(obj, gc_heap, "errors", Value::Array(arr));
        Ok(obj)
    }
}
