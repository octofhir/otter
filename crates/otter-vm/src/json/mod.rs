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
//! - <https://tc39.es/ecma262/#sec-json-object>

mod parse;
pub mod scan;
mod stringify;

pub use parse::{ParseError, parse};
pub use stringify::{StringifyOptions, stringify, stringify_with_options};

use crate::js_surface::{Attr, MethodSpec, NamespaceSpec};
use crate::runtime_cx::visit_native_roots;
use crate::string::JsString;
use crate::{NativeCall, NativeCtx, NativeError, Value};
use otter_gc::heap::RootSlotVisitor;
use otter_gc::raw::RawGc;

/// Static namespace spec installed by the centralized bootstrap
/// registry.
pub static JSON_SPEC: NamespaceSpec = NamespaceSpec {
    name: "JSON",
    methods: JSON_METHODS,
    accessors: &[],
    constants: &[],
    attrs: Attr::global_binding(),
};

/// `BuiltinIntrinsic` adapter for the global `JSON` namespace.
pub struct Intrinsic;

impl crate::intrinsic_install::BuiltinIntrinsic for Intrinsic {
    const NAME: &'static str = JSON_SPEC.name;
    const FEATURE: crate::bootstrap::BootstrapFeatures = crate::bootstrap::BootstrapFeatures::CORE;

    fn install(
        heap: &mut otter_gc::GcHeap,
        global: crate::object::JsObject,
    ) -> Result<(), crate::js_surface::JsSurfaceError> {
        let global_root = crate::Value::Object(global);
        let namespace = crate::js_surface::NamespaceBuilder::from_spec_with_value_roots(
            heap,
            &JSON_SPEC,
            vec![global_root],
        )?
        .build()?;
        crate::bootstrap::define_global_value(
            global,
            heap,
            <Self as crate::intrinsic_install::BuiltinIntrinsic>::NAME,
            crate::Value::Object(namespace),
        );
        Ok(())
    }
}

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

/// Dispatch a `JSON.<method>(args...)` call. Routes via the
/// typed method id emitted by the compiler — no string-table
/// indirection or per-call name match.
pub fn call(
    method: otter_bytecode::method_id::JsonMethod,
    args: &[Value],
    string_heap: &crate::string::StringHeap,
    gc_heap: &mut otter_gc::GcHeap,
) -> Result<Value, JsonError> {
    use otter_bytecode::method_id::JsonMethod;
    match method {
        JsonMethod::Stringify => json_stringify(args, string_heap, gc_heap),
        JsonMethod::Parse => json_parse(args, string_heap, gc_heap),
    }
}

/// Dispatch a `JSON.<method>` call with an explicit root visitor for callers
/// that have a live VM stack or native root set.
pub(crate) fn call_with_roots(
    method: otter_bytecode::method_id::JsonMethod,
    args: &[Value],
    string_heap: &crate::string::StringHeap,
    gc_heap: &mut otter_gc::GcHeap,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<Value, JsonError> {
    use otter_bytecode::method_id::JsonMethod;
    match method {
        JsonMethod::Stringify => json_stringify(args, string_heap, gc_heap),
        JsonMethod::Parse => json_parse_with_roots(args, string_heap, gc_heap, external_visit),
    }
}

fn native_parse(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let coerced = coerce_json_parse_args(ctx, args)?;
    native_json_call(ctx, otter_bytecode::method_id::JsonMethod::Parse, &coerced)
}

/// §25.5.1 step 1 — `JText = ? ToString(text)`. Non-string `text`
/// arguments coerce through the spec ToPrimitive (hint:string) +
/// ToString ladder so a `JSON.parse({ toString(){ return '...' } })`
/// observes the user hook.
fn coerce_json_parse_args(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<smallvec::SmallVec<[Value; 4]>, NativeError> {
    let string_heap = ctx.interp_mut().string_heap_clone();
    let mut out: smallvec::SmallVec<[Value; 4]> = args.iter().cloned().collect();
    if let Some(slot) = out.first_mut() {
        let s = match slot {
            Value::String(s) => s.clone(),
            Value::Symbol(_) => {
                return Err(NativeError::TypeError {
                    name: "parse",
                    reason: "cannot convert a Symbol to a string".to_string(),
                });
            }
            ref primitive if crate::abstract_ops::is_primitive(primitive) => {
                let text = primitive.display_string(ctx.heap());
                JsString::from_str(&text, &string_heap).map_err(|_| NativeError::TypeError {
                    name: "parse",
                    reason: "out of memory".to_string(),
                })?
            }
            ref non_primitive => {
                let (interp, exec) = ctx.interp_mut_and_context();
                let exec = exec.ok_or_else(|| NativeError::TypeError {
                    name: "parse",
                    reason: "missing execution context".to_string(),
                })?;
                let primitive = interp
                    .evaluate_to_primitive(
                        &exec,
                        non_primitive,
                        crate::abstract_ops::ToPrimitiveHint::String,
                    )
                    .map_err(|e| NativeError::TypeError {
                        name: "parse",
                        reason: e.to_string(),
                    })?;
                match primitive {
                    Value::String(s) => s,
                    Value::Symbol(_) => {
                        return Err(NativeError::TypeError {
                            name: "parse",
                            reason: "cannot convert a Symbol to a string".to_string(),
                        });
                    }
                    other => {
                        let text = other.display_string(ctx.heap());
                        JsString::from_str(&text, &string_heap).map_err(|_| {
                            NativeError::TypeError {
                                name: "parse",
                                reason: "out of memory".to_string(),
                            }
                        })?
                    }
                }
            }
        };
        *slot = Value::String(s);
    }
    Ok(out)
}

fn native_stringify(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    native_json_call(ctx, otter_bytecode::method_id::JsonMethod::Stringify, args)
}

fn native_json_call(
    ctx: &mut NativeCtx<'_>,
    method: otter_bytecode::method_id::JsonMethod,
    args: &[Value],
) -> Result<Value, NativeError> {
    let string_heap = ctx.interp_mut().string_heap.clone();
    let runtime_roots = ctx.collect_native_roots();
    let this_value = ctx.this_value().clone();
    let new_target = ctx.new_target().cloned();
    let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        visit_native_roots(
            visitor,
            &runtime_roots,
            &this_value,
            new_target.as_ref(),
            &[],
            &[args],
        );
    };
    call_with_roots(
        method,
        args,
        &string_heap,
        ctx.heap_mut(),
        &mut external_visit,
    )
    .map_err(|err| NativeError::TypeError {
        name: method.name(),
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
    let opts = StringifyOptions::from_space_with_heap(&space, Some(gc_heap))?;
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

fn json_parse_with_roots(
    args: &[Value],
    heap: &crate::string::StringHeap,
    gc_heap: &mut otter_gc::GcHeap,
    external_visit: &mut RootSlotVisitor<'_>,
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
    let value = parse::parse_with_roots(&text, heap, gc_heap, external_visit)?;
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
        let mut heap = make_heap();
        assert_eq!(stringify(&Value::Null, &mut heap).unwrap().unwrap(), "null");
        assert_eq!(
            stringify(&Value::Boolean(true), &mut heap)
                .unwrap()
                .unwrap(),
            "true"
        );
        assert_eq!(stringify(&n(42), &mut heap).unwrap().unwrap(), "42");
        // Undefined → omitted (returns None).
        assert!(stringify(&Value::Undefined, &mut heap).unwrap().is_none());
    }

    #[test]
    fn stringify_nan_and_infinity_become_null() {
        let mut heap = make_heap();
        let nan = Value::Number(NumberValue::Double(f64::NAN));
        let inf = Value::Number(NumberValue::Double(f64::INFINITY));
        assert_eq!(stringify(&nan, &mut heap).unwrap().unwrap(), "null");
        assert_eq!(stringify(&inf, &mut heap).unwrap().unwrap(), "null");
    }

    #[test]
    fn stringify_object_preserves_insertion_order() {
        let mut heap = make_heap();
        let obj = crate::object::alloc_object_old_for_fixture(&mut heap).unwrap();
        crate::object::set(obj, &mut heap, "b", n(1));
        crate::object::set(obj, &mut heap, "a", n(2));
        let s = stringify(&Value::Object(obj), &mut heap).unwrap().unwrap();
        assert_eq!(s, "{\"b\":1,\"a\":2}");
    }

    #[test]
    fn stringify_rejects_bigint() {
        let mut heap = make_heap();
        let bi = Value::BigInt(
            crate::bigint::BigIntValue::from_decimal(&mut heap, "1")
                .unwrap()
                .unwrap(),
        );
        assert!(matches!(stringify(&bi, &mut heap), Err(JsonError::BigInt)));
    }

    #[test]
    fn stringify_uses_source_bytes_for_unmutated_parsed_array() {
        let mut heap = make_heap();
        let sheap = StringHeap::default();
        let parsed = parse("[1,2,3,4]", &sheap, &mut heap).unwrap();
        // Re-stringify should reproduce the input verbatim.
        let s = stringify(&parsed, &mut heap).unwrap().unwrap();
        assert_eq!(s, "[1,2,3,4]");
    }

    #[test]
    fn stringify_observably_equivalent_when_fast_path_disqualified() {
        let mut heap = make_heap();
        let sheap = StringHeap::default();
        let Value::Array(arr) = parse("[1,2,3]", &sheap, &mut heap).unwrap() else {
            panic!("parsed value should be an array")
        };
        // Mutating the array invalidates source_bytes; stringify must
        // still produce a correct (though re-rendered) result.
        crate::array::set(arr, &mut heap, 0, Value::Number(NumberValue::from_i32(99))).unwrap();
        let s = stringify(&Value::Array(arr), &mut heap).unwrap().unwrap();
        assert_eq!(s, "[99,2,3]");
    }

    #[test]
    fn stringify_pretty_disables_source_bytes_fast_path() {
        let mut heap = make_heap();
        let sheap = StringHeap::default();
        let parsed = parse("[1,2,3]", &sheap, &mut heap).unwrap();
        let opts = StringifyOptions::from_space(&Value::Number(NumberValue::from_i32(2))).unwrap();
        let s = stringify_with_options(&parsed, &opts, &mut heap)
            .unwrap()
            .unwrap();
        // Pretty output must reflect the indent setting, proving we
        // did not short-circuit through the captured raw bytes.
        assert_eq!(s, "[\n  1,\n  2,\n  3\n]");
    }

    #[test]
    fn parse_round_trip() {
        let mut heap = make_heap();
        let sheap = StringHeap::default();
        let v = parse("{\"x\":[1,2,3]}", &sheap, &mut heap).unwrap();
        if let Value::Object(o) = v {
            if let Some(Value::Array(arr)) = crate::object::get(o, &heap, "x") {
                assert_eq!(crate::array::len(arr, &heap), 3);
                assert_eq!(crate::array::get(arr, &heap, 1).display_string(&heap), "2");
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
        let obj = crate::object::alloc_object_old_for_fixture(&mut heap).unwrap();
        crate::object::set(obj, &mut heap, "self", Value::Object(obj));
        let err = stringify(&Value::Object(obj), &mut heap).unwrap_err();
        assert!(matches!(err, JsonError::Cyclic));
        assert_eq!(
            err.to_string(),
            "JSON.stringify cannot serialize cyclic structures.",
        );

        // BigInt.
        let bi = Value::BigInt(
            crate::bigint::BigIntValue::from_decimal(&mut heap, "1")
                .unwrap()
                .unwrap(),
        );
        let err = stringify(&bi, &mut heap).unwrap_err();
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
