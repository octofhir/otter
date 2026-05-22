//! ECMA-262 §25.4 `Atomics` namespace.
//!
//! Single-threaded foundation: ships every spec method on the
//! integer-typed subset of TypedArrays. Wait / notify variants are
//! implemented to be observably correct on a single-threaded VM —
//! `wait` returns `"not-equal"` or `"timed-out"` per spec, `notify`
//! always returns `0` (no waiters exist).
//!
//! # Contents
//! - `ATOMICS_SPEC` / `ATOMICS_METHODS` — namespace registration.
//! - `validate_integer_typed_array` — §25.4.3.1
//!   ValidateIntegerTypedArray.
//! - `validate_atomic_access` — §25.4.3.2 ValidateAtomicAccess.
//! - `validate_atomic_access_on_int_or_bigint_typed_array` —
//!   §25.4.3.3 ValidateAtomicAccessOnIntegerTypedArray (waitable
//!   subset).
//! - per-method native handlers using [`NativeCtx`] for spec-faithful
//!   coercion of `index` and `value` arguments.
//! - legacy `call()` opcode entry kept for the AtomicsCall fast
//!   path that older bytecode files may still carry.
//!
//! # Invariants
//! - `Uint8ClampedArray`, `Float32Array`, `Float64Array` are never
//!   accepted as a `typedArray` argument to any atomic op.
//! - `wait` / `waitAsync` require a `SharedArrayBuffer`-backed view
//!   of `Int32Array` or `BigInt64Array`.
//! - Out-of-range indices surface as `RangeError`, **not**
//!   `TypeError`.
//! - View kind is validated before any other argument is coerced
//!   (validate-arraytype-before-value-coercion).
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-atomics-object>
//! - <https://tc39.es/ecma262/#sec-validateintegertypedarray>
//! - <https://tc39.es/ecma262/#sec-validateatomicaccess>

use crate::abstract_ops::{self, ToPrimitiveHint};
use crate::atomics_wait::{self, WaitOutcome};
use crate::bigint::BigIntValue;
use crate::binary::{JsTypedArray, TypedArrayKind};
use crate::js_surface::{Attr, MethodSpec, NamespaceSpec};
use crate::number::NumberValue;
use crate::number::parse::to_integer_or_infinity;
use crate::string::JsString;
use crate::{NativeCall, NativeCtx, NativeError, Value, VmError};
use std::time::Duration;

/// Static namespace spec installed by the centralized bootstrap
/// registry.
pub static ATOMICS_SPEC: NamespaceSpec = NamespaceSpec {
    name: "Atomics",
    methods: ATOMICS_METHODS,
    accessors: &[],
    constants: &[],
    attrs: Attr::global_binding(),
};

/// `BuiltinIntrinsic` adapter for the global `Atomics` namespace.
/// Wires the namespace through `NamespaceBuilder` and links its
/// `[[Prototype]]` to `%Object.prototype%` per §25.4.
pub struct Intrinsic;

impl crate::intrinsic_install::BuiltinIntrinsic for Intrinsic {
    const NAME: &'static str = ATOMICS_SPEC.name;
    const FEATURE: crate::bootstrap::BootstrapFeatures = crate::bootstrap::BootstrapFeatures::CORE;

    fn install(
        heap: &mut otter_gc::GcHeap,
        global: crate::object::JsObject,
    ) -> Result<(), crate::js_surface::JsSurfaceError> {
        let global_root = Value::object(global);
        let namespace = crate::js_surface::NamespaceBuilder::from_spec_with_value_roots(
            heap,
            &ATOMICS_SPEC,
            vec![global_root],
        )?
        .build()?;
        if let Some(Value::Object(object_ctor)) = crate::object::get(global, heap, "Object")
            && let Some(Value::Object(object_proto)) =
                crate::object::get(object_ctor, heap, "prototype")
        {
            crate::object::set_prototype(namespace, heap, Some(object_proto));
        }
        crate::bootstrap::define_global_value(
            global,
            heap,
            <Self as crate::intrinsic_install::BuiltinIntrinsic>::NAME,
            Value::Object(namespace),
        );
        Ok(())
    }
}

const ATOMICS_METHODS: &[MethodSpec] = &[
    method("add", 3, native_add),
    method("and", 3, native_and),
    method("compareExchange", 4, native_compare_exchange),
    method("exchange", 3, native_exchange),
    method("isLockFree", 1, native_is_lock_free),
    method("load", 2, native_load),
    method("notify", 3, native_notify),
    method("or", 3, native_or),
    method("pause", 0, native_pause),
    method("store", 3, native_store),
    method("sub", 3, native_sub),
    method("wait", 4, native_wait),
    method("waitAsync", 4, native_wait_async),
    method("xor", 3, native_xor),
];

const fn method(
    name: &'static str,
    length: u8,
    call: for<'rt> fn(&mut NativeCtx<'rt>, &[Value]) -> Result<Value, NativeError>,
) -> MethodSpec {
    MethodSpec {
        name,
        length,
        attrs: Attr::builtin_function(),
        call: NativeCall::Static(call),
    }
}

/// Whether `Atomics.add` / `sub` / `and` / `or` / `xor` / `exchange`
/// / `compareExchange` accept this kind. §25.4.3.1 step 2-3.
fn accepts_atomic_kind(kind: TypedArrayKind) -> bool {
    !matches!(
        kind,
        TypedArrayKind::Float32 | TypedArrayKind::Float64 | TypedArrayKind::Uint8Clamped
    )
}

/// §25.4.3.1 ValidateIntegerTypedArray ( typedArray, waitable ).
/// `waitable=true` restricts the kind to Int32Array / BigInt64Array.
fn validate_integer_typed_array(
    value: &Value,
    waitable: bool,
    method_name: &'static str,
) -> Result<JsTypedArray, NativeError> {
    let ta = match value {
        Value::TypedArray(t) => *t,
        _ => {
            return Err(type_err(
                method_name,
                "argument is not a TypedArray".to_string(),
            ));
        }
    };
    let kind = ta.kind();
    if !accepts_atomic_kind(kind) {
        return Err(type_err(
            method_name,
            format!("{} is not an integer-element TypedArray", kind.name()),
        ));
    }
    if waitable && !matches!(kind, TypedArrayKind::Int32 | TypedArrayKind::BigInt64) {
        return Err(type_err(
            method_name,
            format!(
                "{} is not a waitable TypedArray (Int32Array or BigInt64Array)",
                kind.name()
            ),
        ));
    }
    Ok(ta)
}

/// §25.4.3.4 ValidateAtomicAccess ( typedArray, requestIndex ).
/// Coerces `request_index` through ToIndex, then bounds-checks it
/// against `typedArray.length`. Out-of-range → `RangeError`.
fn validate_atomic_access(
    ctx: &mut NativeCtx<'_>,
    ta: &JsTypedArray,
    request_index: &Value,
    method_name: &'static str,
) -> Result<usize, NativeError> {
    let idx = coerce_to_index(ctx, request_index, method_name)?;
    let len = ta.length(ctx.heap());
    if idx >= len {
        return Err(range_err(
            method_name,
            format!(
                "index {idx} is out of range for {} of length {}",
                ta.kind().name(),
                len
            ),
        ));
    }
    Ok(idx)
}

/// §7.1.22 ToIndex with full coercion (Object → primitive →
/// integer). Returns `RangeError` for negative / non-integer /
/// non-finite values, `TypeError` for `Symbol` or `BigInt` input
/// (since `ToNumber` on those throws).
fn coerce_to_index(
    ctx: &mut NativeCtx<'_>,
    value: &Value,
    method_name: &'static str,
) -> Result<usize, NativeError> {
    let primitive = to_primitive_number(ctx, value, method_name)?;
    match &primitive {
        Value::Symbol(_) => {
            return Err(type_err(
                method_name,
                "cannot convert Symbol to a number".to_string(),
            ));
        }
        Value::BigInt(_) => {
            return Err(type_err(
                method_name,
                "cannot convert BigInt to a number".to_string(),
            ));
        }
        _ => {}
    }
    let n = to_integer_or_infinity(&primitive, ctx.heap());
    if !n.is_finite() {
        return Err(range_err(method_name, "index is not finite".to_string()));
    }
    if n < 0.0 {
        return Err(range_err(method_name, "index is negative".to_string()));
    }
    if n > 9_007_199_254_740_991.0 {
        return Err(range_err(method_name, "index exceeds 2^53 - 1".to_string()));
    }
    Ok(n as usize)
}

/// §7.1.1 ToPrimitive(value, "number"). Plain primitive values
/// pass through unchanged; Objects route through
/// `Interpreter::evaluate_to_primitive` so `Symbol.toPrimitive` /
/// `valueOf` / `toString` are observable. User-thrown exceptions
/// during coercion propagate as `NativeError::Thrown` so the
/// runtime mapper hands the original payload back to JS (this is
/// required by tests like
/// `Atomics/notify/symbol-for-index-throws.js` that assert on a
/// `Test262Error` thrown from a poisoned `valueOf`).
fn to_primitive_number(
    ctx: &mut NativeCtx<'_>,
    value: &Value,
    method_name: &'static str,
) -> Result<Value, NativeError> {
    if abstract_ops::is_primitive(value) {
        return Ok(*value);
    }
    let (interp, exec) = ctx.interp_mut_and_context();
    let exec = exec.ok_or_else(|| {
        type_err(
            method_name,
            "missing execution context for ToPrimitive".to_string(),
        )
    })?;
    interp
        .evaluate_to_primitive(&exec, value, ToPrimitiveHint::Number)
        .map_err(|e| vm_error_to_native(method_name, e))
}

/// Convert a [`VmError`] surfaced from re-entering the interpreter
/// into the matching [`NativeError`]. Crucially, `VmError::Uncaught`
/// maps to `NativeError::Thrown` so the original user-thrown value
/// rides through (the runtime mapper at `lib.rs:15616` reconstructs
/// the JS exception from the `message` field).
fn vm_error_to_native(method_name: &'static str, err: VmError) -> NativeError {
    match err {
        VmError::Uncaught { value } => NativeError::Thrown {
            name: spec_name(method_name),
            message: value,
        },
        other => type_err(method_name, other.to_string()),
    }
}

/// Coerce the third / fourth argument of a read-modify-write op to
/// the element representation. Numeric kinds use `ToIntegerOrInfinity`;
/// BigInt kinds use `ToBigInt`.
fn coerce_element_value(
    ctx: &mut NativeCtx<'_>,
    kind: TypedArrayKind,
    value: &Value,
    method_name: &'static str,
) -> Result<Value, NativeError> {
    let primitive = to_primitive_number(ctx, value, method_name)?;
    if kind.is_bigint() {
        let heap = ctx.interp_mut().gc_heap_mut();
        let oom_to_err = |err: otter_gc::OutOfMemory| {
            type_err(
                method_name,
                format!(
                    "out of memory: requested {} bytes, heap limit {}",
                    err.requested_bytes(),
                    err.heap_limit_bytes(),
                ),
            )
        };
        match primitive {
            Value::BigInt(b) => Ok(Value::big_int(b)),
            Value::Boolean(b) => {
                let handle = BigIntValue::from_inner(heap, num_bigint::BigInt::from(i64::from(b)))
                    .map_err(oom_to_err)?;
                Ok(Value::big_int(handle))
            }
            Value::String(s) => {
                let txt = s.to_lossy_string(heap);
                let trimmed = txt.trim();
                let parsed = trimmed.parse::<num_bigint::BigInt>().map_err(|_| {
                    type_err(method_name, format!("cannot convert {trimmed:?} to BigInt"))
                })?;
                let handle = BigIntValue::from_inner(heap, parsed).map_err(oom_to_err)?;
                Ok(Value::big_int(handle))
            }
            Value::Number(_) => Err(type_err(
                method_name,
                "cannot mix BigInt and Number".to_string(),
            )),
            _ => Err(type_err(
                method_name,
                "cannot convert value to BigInt".to_string(),
            )),
        }
    } else {
        match primitive {
            Value::BigInt(_) => Err(type_err(
                method_name,
                "cannot mix BigInt and Number".to_string(),
            )),
            Value::Symbol(_) => Err(type_err(
                method_name,
                "cannot convert Symbol to a number".to_string(),
            )),
            other => {
                let mut n = to_integer_or_infinity(&other, ctx.heap());
                // §7.1.5 step 2 — `ToIntegerOrInfinity` collapses
                // `+0` / `-0` / `NaN` to `0` (positive zero). Force
                // the sign here so `Atomics.store(view, 0, -0)`
                // returns `+0` per the test262
                // `expected-return-value-negative-zero` case.
                if n == 0.0 {
                    n = 0.0;
                }
                Ok(Value::number(NumberValue::from_f64(n)))
            }
        }
    }
}

fn type_err(name: &'static str, reason: String) -> NativeError {
    NativeError::TypeError {
        name: spec_name(name),
        reason,
    }
}

fn range_err(name: &'static str, reason: String) -> NativeError {
    NativeError::RangeError {
        name: spec_name(name),
        reason,
    }
}

/// Map "add" → "Atomics.add" etc. for diagnostic strings.
const fn spec_name(method: &'static str) -> &'static str {
    // Static strings; the runtime mapper only reads `.name` for
    // diagnostics so a plain method name is acceptable.
    method
}

// =====================================================================
// Native method handlers — primary entry points after the property
// lookup path resolves `Atomics.<method>` to a `Value::NativeFunction`.
// =====================================================================

fn native_load(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let ta = validate_integer_typed_array(
        args.first().unwrap_or(&Value::Undefined),
        false,
        "Atomics.load",
    )?;
    let idx = validate_atomic_access(
        ctx,
        &ta,
        args.get(1).unwrap_or(&Value::Undefined),
        "Atomics.load",
    )?;
    let heap = ctx.interp_mut().gc_heap_mut();
    ta.get(heap, idx).map_err(|e| {
        type_err(
            "Atomics.load",
            format!(
                "out of memory: {} requested, limit {}",
                e.requested_bytes(),
                e.heap_limit_bytes()
            ),
        )
    })
}

fn native_store(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let ta = validate_integer_typed_array(
        args.first().unwrap_or(&Value::Undefined),
        false,
        "Atomics.store",
    )?;
    let idx = validate_atomic_access(
        ctx,
        &ta,
        args.get(1).unwrap_or(&Value::Undefined),
        "Atomics.store",
    )?;
    let value = coerce_element_value(
        ctx,
        ta.kind(),
        args.get(2).unwrap_or(&Value::Undefined),
        "Atomics.store",
    )?;
    ta.set(ctx.interp_mut().gc_heap_mut(), idx, &value);
    Ok(value)
}

fn modify_op(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    method_name: &'static str,
    op: fn(i64, i64) -> i64,
    op_big: fn(&num_bigint::BigInt, &num_bigint::BigInt) -> num_bigint::BigInt,
) -> Result<Value, NativeError> {
    let ta = validate_integer_typed_array(
        args.first().unwrap_or(&Value::Undefined),
        false,
        method_name,
    )?;
    let idx = validate_atomic_access(
        ctx,
        &ta,
        args.get(1).unwrap_or(&Value::Undefined),
        method_name,
    )?;
    let coerced = coerce_element_value(
        ctx,
        ta.kind(),
        args.get(2).unwrap_or(&Value::Undefined),
        method_name,
    )?;
    let heap = ctx.interp_mut().gc_heap_mut();
    let oom_to_err = |err: otter_gc::OutOfMemory| {
        type_err(
            method_name,
            format!(
                "out of memory: requested {} bytes, heap limit {}",
                err.requested_bytes(),
                err.heap_limit_bytes(),
            ),
        )
    };
    let prev = ta.get(heap, idx).map_err(oom_to_err)?;
    if ta.kind().is_bigint() {
        let prev_b = match &prev {
            Value::BigInt(b) => b.clone_inner(heap),
            _ => num_bigint::BigInt::from(0),
        };
        let v_b = match &coerced {
            Value::BigInt(b) => b.clone_inner(heap),
            _ => num_bigint::BigInt::from(0),
        };
        let new_b = op_big(&prev_b, &v_b);
        let handle = BigIntValue::from_inner(heap, new_b).map_err(oom_to_err)?;
        ta.set(heap, idx, &Value::BigInt(handle));
    } else {
        let prev_n = match &prev {
            Value::Number(n) => n.as_f64() as i64,
            _ => 0,
        };
        let v_n = match &coerced {
            Value::Number(n) => n.as_f64() as i64,
            _ => 0,
        };
        let new_n = op(prev_n, v_n);
        ta.set(
            heap,
            idx,
            &Value::Number(NumberValue::from_f64(new_n as f64)),
        );
    }
    Ok(prev)
}

fn native_add(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    modify_op(
        ctx,
        args,
        "Atomics.add",
        |a, b| a.wrapping_add(b),
        |a, b| a + b,
    )
}

fn native_sub(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    modify_op(
        ctx,
        args,
        "Atomics.sub",
        |a, b| a.wrapping_sub(b),
        |a, b| a - b,
    )
}

fn native_and(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    modify_op(ctx, args, "Atomics.and", |a, b| a & b, |a, b| a & b)
}

fn native_or(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    modify_op(ctx, args, "Atomics.or", |a, b| a | b, |a, b| a | b)
}

fn native_xor(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    modify_op(ctx, args, "Atomics.xor", |a, b| a ^ b, |a, b| a ^ b)
}

fn native_exchange(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let ta = validate_integer_typed_array(
        args.first().unwrap_or(&Value::Undefined),
        false,
        "Atomics.exchange",
    )?;
    let idx = validate_atomic_access(
        ctx,
        &ta,
        args.get(1).unwrap_or(&Value::Undefined),
        "Atomics.exchange",
    )?;
    let value = coerce_element_value(
        ctx,
        ta.kind(),
        args.get(2).unwrap_or(&Value::Undefined),
        "Atomics.exchange",
    )?;
    let heap = ctx.interp_mut().gc_heap_mut();
    let prev = ta.get(heap, idx).map_err(|e| {
        type_err(
            "Atomics.exchange",
            format!(
                "out of memory: requested {} bytes, heap limit {}",
                e.requested_bytes(),
                e.heap_limit_bytes(),
            ),
        )
    })?;
    ta.set(heap, idx, &value);
    Ok(prev)
}

fn native_compare_exchange(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let ta = validate_integer_typed_array(
        args.first().unwrap_or(&Value::Undefined),
        false,
        "Atomics.compareExchange",
    )?;
    let idx = validate_atomic_access(
        ctx,
        &ta,
        args.get(1).unwrap_or(&Value::Undefined),
        "Atomics.compareExchange",
    )?;
    let expected = coerce_element_value(
        ctx,
        ta.kind(),
        args.get(2).unwrap_or(&Value::Undefined),
        "Atomics.compareExchange",
    )?;
    let replacement = coerce_element_value(
        ctx,
        ta.kind(),
        args.get(3).unwrap_or(&Value::Undefined),
        "Atomics.compareExchange",
    )?;
    // §25.4.3.5 step 8 — expected must be narrowed through the
    // element-type's RawBytes round-trip before the comparison
    // against the raw current value (e.g. an Int16 view stores
    // `123456789` as `-13035`, so expected `123456789` must compare
    // against `-13035`, not the original Number).
    let heap = ctx.interp_mut().gc_heap_mut();
    let oom_to_err = |err: otter_gc::OutOfMemory| {
        type_err(
            "Atomics.compareExchange",
            format!(
                "out of memory: requested {} bytes, heap limit {}",
                err.requested_bytes(),
                err.heap_limit_bytes(),
            ),
        )
    };
    let expected_narrow = narrow_through_kind(heap, ta.kind(), &expected).map_err(oom_to_err)?;
    let current = ta.get(heap, idx).map_err(oom_to_err)?;
    if values_equal_strict(&current, &expected_narrow, heap) {
        ta.set(heap, idx, &replacement);
    }
    Ok(current)
}

/// Round-trip `value` through the element type so the result matches
/// what the buffer would read back after a store. Used by
/// `compareExchange` to spec-equate `expected` with `current` per
/// §25.4.3.5.
fn narrow_through_kind(
    heap: &mut otter_gc::GcHeap,
    kind: TypedArrayKind,
    value: &Value,
) -> Result<Value, otter_gc::OutOfMemory> {
    let mut scratch = [0u8; 8];
    kind.write(heap, &mut scratch, 0, value);
    kind.read(heap, &scratch, 0)
}

fn native_is_lock_free(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    // Spec §25.4.13 — argument is coerced via ToIntegerOrInfinity
    // (no ToIndex), so negative / non-integer / NaN map to `false`
    // by failing the `matches!(.., 1|2|4|8)` filter.
    let arg = args.first().cloned().unwrap_or(Value::undefined());
    let primitive = to_primitive_number(ctx, &arg, "Atomics.isLockFree")?;
    if matches!(primitive, Value::Symbol(_)) {
        return Err(type_err(
            "Atomics.isLockFree",
            "cannot convert Symbol to a number".to_string(),
        ));
    }
    let n = to_integer_or_infinity(&primitive, ctx.heap());
    let supported = n.is_finite() && matches!(n as i64, 1 | 2 | 4 | 8) && (n as i64 as f64) == n;
    Ok(Value::boolean(supported))
}

fn native_pause(_ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    // §25.4.14 Atomics.pause ( iterationNumber )
    // <https://tc39.es/ecma262/#sec-atomics.pause>
    //
    // The argument, if present, must be an integral Number (not a
    // string or object coerced to one). The spec calls this
    // "integral Number" — Number whose ToIntegerOrInfinity equals
    // itself. Anything else throws TypeError. A single-threaded VM
    // can simply yield; we choose the trivial no-op.
    if let Some(v) = args.first()
        && !matches!(v, Value::Undefined)
    {
        let Value::Number(n) = v else {
            return Err(type_err(
                "Atomics.pause",
                "iterationNumber must be an integral Number".to_string(),
            ));
        };
        let f = n.as_f64();
        if !f.is_finite() || f.trunc() != f {
            return Err(type_err(
                "Atomics.pause",
                "iterationNumber must be an integral Number".to_string(),
            ));
        }
    }
    Ok(Value::undefined())
}

fn native_wait(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    do_wait(ctx, args, /* is_async */ false)
}

fn native_wait_async(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    do_wait(ctx, args, /* is_async */ true)
}

fn do_wait(ctx: &mut NativeCtx<'_>, args: &[Value], is_async: bool) -> Result<Value, NativeError> {
    let method_name = if is_async {
        "Atomics.waitAsync"
    } else {
        "Atomics.wait"
    };
    let ta =
        validate_integer_typed_array(args.first().unwrap_or(&Value::Undefined), true, method_name)?;
    // §25.4.3.13 Atomics.wait — buffer must be a SharedArrayBuffer.
    if !ta.buffer(ctx.heap()).is_shared() {
        return Err(type_err(
            method_name,
            "expected a SharedArrayBuffer-backed TypedArray".to_string(),
        ));
    }
    let buf_id = ta
        .buffer(ctx.heap())
        .shared_id(ctx.heap())
        .expect("is_shared() guards shared_id");
    let idx = validate_atomic_access(
        ctx,
        &ta,
        args.get(1).unwrap_or(&Value::Undefined),
        method_name,
    )?;
    // Coerce expected `value` before timeout for spec-faithful order.
    let expected = coerce_element_value(
        ctx,
        ta.kind(),
        args.get(2).unwrap_or(&Value::Undefined),
        method_name,
    )?;
    // §25.4.3.13 step 11 — timeout is `ToNumber(q)`; NaN → +∞;
    // observable side-effects fire even on a single-threaded VM.
    let timeout = match args.get(3) {
        None | Some(Value::Undefined) => f64::INFINITY,
        Some(v) => {
            let primitive = to_primitive_number(ctx, v, method_name)?;
            if matches!(primitive, Value::Symbol(_)) {
                return Err(type_err(
                    method_name,
                    "cannot convert Symbol to a number".to_string(),
                ));
            }
            let n = crate::number::parse::to_number_value(&primitive, ctx.heap());
            if n.is_nan() {
                f64::INFINITY
            } else {
                n.max(0.0)
            }
        }
    };
    let heap = ctx.interp_mut().gc_heap_mut();
    let current = ta.get(heap, idx).map_err(|e| {
        type_err(
            method_name,
            format!(
                "out of memory: requested {} bytes, heap limit {}",
                e.requested_bytes(),
                e.heap_limit_bytes(),
            ),
        )
    })?;
    let label = if !values_equal_strict(&current, &expected, heap) {
        "not-equal"
    } else if is_async {
        // §25.4.3.14 — `waitAsync` does not block the calling
        // thread; the single-thread foundation returns a
        // synchronously-fulfilled `"timed-out"` promise.
        "timed-out"
    } else {
        // §25.4.3.13 — block the calling thread on the shared-buffer
        // wait registry until either a notify or the timeout fires.
        let dur = if timeout.is_infinite() {
            None
        } else {
            // Clamp to u64::MAX milliseconds (~584 million years) to
            // stay inside `Duration::from_millis` range.
            let ms = timeout.min(u64::MAX as f64) as u64;
            Some(Duration::from_millis(ms))
        };
        match atomics_wait::park_until_notified(buf_id, idx, dur) {
            WaitOutcome::Ok => "ok",
            WaitOutcome::TimedOut => "timed-out",
        }
    };
    let string_heap = ctx.cx.interp.gc_heap_mut();
    let label_str = JsString::from_str(label, string_heap)
        .map_err(|e| type_err(method_name, format!("string allocation failed: {e}")))?;
    if is_async {
        let label_value = Value::string(label_str);
        let promise = ctx
            .fulfilled_promise_with_roots(label_value, &[], &[args])
            .map_err(|e| type_err(method_name, format!("promise allocation failed: {e}")))?;
        let promise_value = Value::promise(promise);
        let result = ctx
            .alloc_object_with_roots(&[&label_value, &promise_value], &[args])
            .map_err(|e| type_err(method_name, format!("object allocation failed: {e}")))?;
        ctx.set_property(result, "async", Value::Boolean(false))
            .map_err(|e| type_err(method_name, e.to_string()))?;
        ctx.set_property(result, "value", promise_value)
            .map_err(|e| type_err(method_name, e.to_string()))?;
        Ok(Value::object(result))
    } else {
        Ok(Value::string(label_str))
    }
}

fn native_notify(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    // §25.4.3.12 Atomics.notify ( typedArray, index, count ).
    // Allows Int32Array / BigInt64Array; no SharedArrayBuffer
    // requirement — the spec returns 0 for a non-shared buffer
    // because no thread can be waiting on a non-shared backing.
    let ta = validate_integer_typed_array(
        args.first().unwrap_or(&Value::Undefined),
        true,
        "Atomics.notify",
    )?;
    let idx = validate_atomic_access(
        ctx,
        &ta,
        args.get(1).unwrap_or(&Value::Undefined),
        "Atomics.notify",
    )?;
    // count: ToIntegerOrInfinity with negative clamped to 0 per
    // §25.4.3.12 step 5; +Infinity means "wake every waiter".
    let count = match args.get(2) {
        None | Some(Value::Undefined) => usize::MAX,
        Some(v) => {
            let primitive = to_primitive_number(ctx, v, "Atomics.notify")?;
            if matches!(primitive, Value::Symbol(_)) {
                return Err(type_err(
                    "Atomics.notify",
                    "cannot convert Symbol to a number".to_string(),
                ));
            }
            let n = to_integer_or_infinity(&primitive, ctx.heap());
            if n.is_infinite() && n.is_sign_positive() {
                usize::MAX
            } else if n.is_nan() || n.is_sign_negative() {
                0
            } else if n >= usize::MAX as f64 {
                usize::MAX
            } else {
                n as usize
            }
        }
    };
    let woken = match ta.buffer(ctx.heap()).shared_id(ctx.heap()) {
        Some(buf_id) => atomics_wait::notify_waiters(buf_id, idx, count),
        None => 0,
    };
    Ok(Value::number(NumberValue::from_f64(woken as f64)))
}

fn values_equal_strict(a: &Value, b: &Value, heap: &otter_gc::GcHeap) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => crate::number::equals(*x, *y),
        (Value::BigInt(x), Value::BigInt(y)) => x.numeric_eq(*y, heap),
        _ => false,
    }
}
