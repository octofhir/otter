//! `JSON` namespace — hand-rolled `stringify` and `parse`.
//!
//! Slice 32 picks the hot-path implementation deliberately: we
//! **do not** depend on `serde_json`. Serde's parser is general-
//! purpose and ~3–5× slower than a focused byte-cursor parser on
//! the JS shapes we actually serialize. Stringify and parse both
//! own their own buffer/cursor so we can keep them branch-free on
//! the common case (ASCII strings, small integers, dense arrays).
//!
//! # Contents
//! - [`JSON_SPEC`] — static namespace spec used by bootstrap.
//! - [`call`] — namespace-call dispatcher (used by `Op::JsonCall`).
//! - [`stringify`] / [`parse`] — public entry points.
//! - [`stringify_with_options`] — programmable `space` + `replacer`.
//! - [`JsonError`] — failure mode the dispatcher converts to
//!   `VmError`.
//!
//! # Invariants
//! - **No recursion.** Both serializer and parser walk an explicit
//!   stack capped at `MAX_NESTING_DEPTH` (1024) so adversarial
//!   nested input cannot blow the host stack.
//! - **Cycle detection.** Stringify carries an
//!   `Rc::ptr_eq`-comparing visit set; revisiting an active node
//!   raises `JsonError::Cyclic`.
//! - **Deterministic key order.** Object properties enumerate in
//!   shape insertion order (`JsObject::borrow_props`).
//! - **NaN / ±Infinity / -0 → `null`** per spec §25.5.2.4.
//! - **BigInt → `TypeError`-equivalent** ([`JsonError::BigInt`]).
//! - **Strict parse.** Trailing commas, comments, single quotes,
//!   leading zeros, leading `+`, NaN/Infinity literals — all
//!   rejected. The parser walks `&[u8]` for the ASCII fast path
//!   and falls back to WTF-16 unescape only inside string literals.
//!
//! # See also
//! - [`docs/new-engine/tasks/32-json-stringify-parse.md`](
//!     ../../../docs/new-engine/tasks/32-json-stringify-parse.md
//!   )

mod parse;
mod stringify;

pub use parse::{ParseError, parse};
pub use stringify::{StringifyOptions, stringify, stringify_with_options};

use crate::js_surface::{Attr, MethodSpec, NamespaceSpec};
use crate::string::JsString;
use crate::{NativeCall, NativeCtx, NativeError, Value};

/// Static namespace spec installed by the centralized bootstrap
/// registry.
pub static JSON_SPEC: NamespaceSpec = NamespaceSpec {
    name: "JSON",
    methods: JSON_METHODS,
    accessors: &[],
    constants: &[],
    attrs: Attr::global_binding(),
};

const JSON_METHODS: &[MethodSpec] = &[
    MethodSpec {
        name: "parse",
        length: 2,
        attrs: Attr::builtin_function(),
        call: NativeCall::Static(native_parse),
    },
    MethodSpec {
        name: "stringify",
        length: 3,
        attrs: Attr::builtin_function(),
        call: NativeCall::Static(native_stringify),
    },
];

/// Hard cap on nesting depth. Both stringify and parse abort with
/// `JsonError::TooDeep` once exceeded — keeps adversarial input
/// from blowing the host stack and matches V8's `JSON.stringify`
/// behaviour for very deep objects.
pub const MAX_NESTING_DEPTH: usize = 1024;

/// Failure modes for [`call`].
#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
pub enum JsonError {
    /// `JSON.<name>` is not a registered function.
    #[error("JSON.{0} is not defined")]
    UnknownMember(String),
    /// Argument failed type / value coercion.
    #[error("JSON.{name} argument {index} {reason}")]
    BadArgument {
        /// Function name.
        name: &'static str,
        /// Argument index (0-based).
        index: u16,
        /// Short reason.
        reason: &'static str,
    },
    /// Cycle detected while serialising.
    #[error("JSON.stringify cannot serialize cyclic structures.")]
    Cyclic,
    /// Nesting exceeded [`MAX_NESTING_DEPTH`].
    #[error("JSON nesting exceeded {limit} levels.")]
    TooDeep {
        /// Configured cap.
        limit: usize,
    },
    /// `BigInt` cannot be serialised per spec §25.5.2.4.
    #[error("JSON.stringify cannot serialize BigInt values.")]
    BigInt,
    /// Underlying string-heap allocation failed.
    #[error("out of memory: requested {requested_bytes} bytes, heap limit {heap_limit_bytes}")]
    OutOfMemory {
        /// Bytes requested.
        requested_bytes: u64,
        /// Heap cap (`0` = unlimited).
        heap_limit_bytes: u64,
    },
    /// Strict-mode parse failure.
    #[error("JSON.parse: {message} at byte {position}")]
    ParseFailed {
        /// Diagnostic body.
        message: String,
        /// 0-based byte offset.
        position: usize,
    },
}

impl From<crate::string::StringError> for JsonError {
    fn from(err: crate::string::StringError) -> Self {
        match err {
            crate::string::StringError::OutOfMemory {
                requested_bytes,
                heap_limit_bytes,
            } => Self::OutOfMemory {
                requested_bytes,
                heap_limit_bytes,
            },
        }
    }
}

impl From<otter_gc::OutOfMemory> for JsonError {
    fn from(err: otter_gc::OutOfMemory) -> Self {
        Self::OutOfMemory {
            requested_bytes: err.requested_bytes(),
            heap_limit_bytes: err.heap_limit_bytes(),
        }
    }
}

impl From<ParseError> for JsonError {
    fn from(err: ParseError) -> Self {
        Self::ParseFailed {
            message: err.message,
            position: err.position,
        }
    }
}

/// Dispatch a `JSON.<name>(args...)` call. Mirrors
/// [`crate::math::call`]; see the receiver-type guards below.
pub fn call(
    name: &str,
    args: &[Value],
    string_heap: &crate::string::StringHeap,
    gc_heap: &mut otter_gc::GcHeap,
) -> Result<Value, JsonError> {
    match name {
        "stringify" => json_stringify(args, string_heap, gc_heap),
        "parse" => json_parse(args, string_heap, gc_heap),
        _ => Err(JsonError::UnknownMember(name.to_string())),
    }
}

fn native_parse(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    native_json_call(ctx, "parse", args)
}

fn native_stringify(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    native_json_call(ctx, "stringify", args)
}

fn native_json_call(
    ctx: &mut NativeCtx<'_>,
    name: &'static str,
    args: &[Value],
) -> Result<Value, NativeError> {
    let interp = ctx.interp_mut();
    let string_heap = interp.string_heap.clone();
    call(name, args, &string_heap, interp.gc_heap_mut()).map_err(|err| NativeError::TypeError {
        name,
        reason: err.to_string(),
    })
}

fn json_stringify(
    args: &[Value],
    heap: &crate::string::StringHeap,
    gc_heap: &mut otter_gc::GcHeap,
) -> Result<Value, JsonError> {
    let value = args.first().cloned().unwrap_or(Value::Undefined);
    let space = args.get(2).cloned().unwrap_or(Value::Undefined);
    let opts = StringifyOptions::from_space(&space)?;
    match stringify_with_options(&value, &opts, gc_heap)? {
        Some(text) => Ok(Value::String(JsString::from_str(&text, heap)?)),
        None => Ok(Value::Undefined),
    }
}

fn json_parse(
    args: &[Value],
    heap: &crate::string::StringHeap,
    gc_heap: &mut otter_gc::GcHeap,
) -> Result<Value, JsonError> {
    let text = match args.first() {
        Some(Value::String(s)) => s.to_lossy_string(),
        _ => {
            return Err(JsonError::BadArgument {
                name: "parse",
                index: 0,
                reason: "must be a string",
            });
        }
    };
    let value = parse(&text, heap, gc_heap)?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::number::NumberValue;
    use crate::string::StringHeap;

    fn n(v: i32) -> Value {
        Value::Number(NumberValue::from_i32(v))
    }

    fn make_heap() -> otter_gc::GcHeap {
        otter_gc::GcHeap::new().expect("gc heap")
    }

    #[test]
    fn stringify_primitives() {
        let heap = make_heap();
        assert_eq!(stringify(&Value::Null, &heap).unwrap().unwrap(), "null");
        assert_eq!(
            stringify(&Value::Boolean(true), &heap).unwrap().unwrap(),
            "true"
        );
        assert_eq!(stringify(&n(42), &heap).unwrap().unwrap(), "42");
        // Undefined → omitted (returns None).
        assert!(stringify(&Value::Undefined, &heap).unwrap().is_none());
    }

    #[test]
    fn stringify_nan_and_infinity_become_null() {
        let heap = make_heap();
        let nan = Value::Number(NumberValue::Double(f64::NAN));
        let inf = Value::Number(NumberValue::Double(f64::INFINITY));
        assert_eq!(stringify(&nan, &heap).unwrap().unwrap(), "null");
        assert_eq!(stringify(&inf, &heap).unwrap().unwrap(), "null");
    }

    #[test]
    fn stringify_object_preserves_insertion_order() {
        let mut heap = make_heap();
        let obj = crate::object::alloc_object(&mut heap).unwrap();
        crate::object::set(obj, &mut heap, "b", n(1));
        crate::object::set(obj, &mut heap, "a", n(2));
        let s = stringify(&Value::Object(obj), &heap).unwrap().unwrap();
        assert_eq!(s, "{\"b\":1,\"a\":2}");
    }

    #[test]
    fn stringify_rejects_bigint() {
        let heap = make_heap();
        let bi = Value::BigInt(crate::bigint::BigIntValue::from_decimal("1").unwrap());
        assert!(matches!(stringify(&bi, &heap), Err(JsonError::BigInt)));
    }

    #[test]
    fn parse_round_trip() {
        let mut heap = make_heap();
        let sheap = StringHeap::default();
        let v = parse("{\"x\":[1,2,3]}", &sheap, &mut heap).unwrap();
        if let Value::Object(o) = v {
            if let Some(Value::Array(arr)) = crate::object::get(o, &heap, "x") {
                assert_eq!(crate::array::len(arr, &heap), 3);
                assert_eq!(crate::array::get(arr, &heap, 1).display_string(), "2");
            } else {
                panic!("expected x: array");
            }
        } else {
            panic!("expected object");
        }
    }

    #[test]
    fn error_messages_are_specific() {
        // Cyclic — no path walk (cheap identity-pointer set on the
        // hot path; full path tracking can layer on later).
        let mut heap = make_heap();
        let obj = crate::object::alloc_object(&mut heap).unwrap();
        crate::object::set(obj, &mut heap, "self", Value::Object(obj));
        let err = stringify(&Value::Object(obj), &heap).unwrap_err();
        assert!(matches!(err, JsonError::Cyclic));
        assert_eq!(
            err.to_string(),
            "JSON.stringify cannot serialize cyclic structures.",
        );

        // BigInt.
        let bi = Value::BigInt(crate::bigint::BigIntValue::from_decimal("1").unwrap());
        let err = stringify(&bi, &heap).unwrap_err();
        assert_eq!(
            err.to_string(),
            "JSON.stringify cannot serialize BigInt values.",
        );

        // Parse with byte position.
        let mut gc_heap = make_heap();
        let sheap = StringHeap::default();
        let err = parse("[1, 2,]", &sheap, &mut gc_heap).unwrap_err();
        assert_eq!(err.position, 6);
        assert_eq!(err.message, "trailing comma");
    }
}
