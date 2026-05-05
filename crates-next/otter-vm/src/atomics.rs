//! ECMA-262 §25.4 `Atomics` namespace.
//!
//! Foundation slice ships the single-threaded subset:
//! `load` / `store` / `add` / `sub` / `and` / `or` / `xor` /
//! `exchange` / `compareExchange` / `isLockFree`. The
//! cross-thread `wait` / `notify` / `waitAsync` family is
//! deferred until the worker / SharedArrayBuffer cross-isolate
//! plumbing lands.
//!
//! Each operation reads or writes a single TypedArray element
//! through the kind-specific element-type rules in
//! [`super::binary::TypedArrayKind`]. On single-thread the ops
//! are equivalent to the corresponding non-atomic indexed
//! accesses; the API surface still validates the element-kind
//! restriction (atomics only operate on integer kinds: Int8 /
//! Uint8 / Int16 / Uint16 / Int32 / Uint32 / BigInt64 /
//! BigUint64).
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-atomics-object>

use crate::binary::{JsTypedArray, TypedArrayKind};
use crate::number::NumberValue;
use crate::promise::JsPromiseHandle;
use crate::string::{JsString, StringHeap};
use crate::{Value, VmError};

/// Dispatch `Atomics.<name>(args...)`.
///
/// # Errors
/// - [`VmError::TypeMismatch`] for unsupported element kinds /
///   non-TypedArray receivers / out-of-range indices.
/// - [`VmError::UnknownIntrinsic`] for unrecognised method names.
pub fn call(
    name: &str,
    args: &[Value],
    string_heap: &StringHeap,
    gc_heap: &mut otter_gc::GcHeap,
) -> Result<Value, VmError> {
    match name {
        // §25.4.13 Atomics.isLockFree(size) — true for the four
        // sizes the spec mandates (1, 2, 4, 8).
        // <https://tc39.es/ecma262/#sec-atomics.islockfree>
        "isLockFree" => {
            let n = match args.first() {
                Some(Value::Number(n)) => n.as_f64(),
                Some(Value::Boolean(true)) => 1.0,
                Some(Value::Boolean(false)) | Some(Value::Null) => 0.0,
                _ => return Ok(Value::Boolean(false)),
            };
            let supported = matches!(n as i32, 1 | 2 | 4 | 8) && n.fract() == 0.0;
            Ok(Value::Boolean(supported))
        }
        // §25.4.5 Atomics.load(typedArray, index)
        // <https://tc39.es/ecma262/#sec-atomics.load>
        "load" => {
            let (ta, idx) = read_indexed_args(args)?;
            ensure_int_kind(ta.kind())?;
            Ok(ta.get(idx))
        }
        // §25.4.10 Atomics.store(typedArray, index, value)
        // <https://tc39.es/ecma262/#sec-atomics.store>
        "store" => {
            let (ta, idx) = read_indexed_args(args)?;
            ensure_int_kind(ta.kind())?;
            let value = args.get(2).cloned().unwrap_or(Value::Undefined);
            ta.set(idx, &value);
            Ok(value)
        }
        "add" => atomic_modify(args, |a, b| a.wrapping_add(b)),
        "sub" => atomic_modify(args, |a, b| a.wrapping_sub(b)),
        "and" => atomic_modify(args, |a, b| a & b),
        "or" => atomic_modify(args, |a, b| a | b),
        "xor" => atomic_modify(args, |a, b| a ^ b),
        // §25.4.7 Atomics.exchange(typedArray, index, value)
        // <https://tc39.es/ecma262/#sec-atomics.exchange>
        "exchange" => {
            let (ta, idx) = read_indexed_args(args)?;
            ensure_int_kind(ta.kind())?;
            let prev = ta.get(idx);
            let value = args.get(2).cloned().unwrap_or(Value::Undefined);
            ta.set(idx, &value);
            Ok(prev)
        }
        // §25.4.6 Atomics.compareExchange(typedArray, index,
        //                                 expectedValue, replacementValue)
        // <https://tc39.es/ecma262/#sec-atomics.compareexchange>
        "compareExchange" => {
            let (ta, idx) = read_indexed_args(args)?;
            ensure_int_kind(ta.kind())?;
            let expected = args.get(2).cloned().unwrap_or(Value::Undefined);
            let replacement = args.get(3).cloned().unwrap_or(Value::Undefined);
            let current = ta.get(idx);
            if values_equal_strict(&current, &expected) {
                ta.set(idx, &replacement);
            }
            Ok(current)
        }
        // §25.4.11 Atomics.wait(typedArray, index, value, timeout?).
        // Single-thread foundation: the current value is read; if
        // it does not match the expected `value`, we return
        // "not-equal" immediately. Otherwise we cannot ever
        // observe a notify (no other thread to fire it), so
        // returning "timed-out" matches spec semantics for
        // timeout=0 and is observably correct for any positive
        // timeout in a single-threaded VM.
        // <https://tc39.es/ecma262/#sec-atomics.wait>
        "wait" => {
            let (ta, idx) = read_indexed_args(args)?;
            ensure_int_kind(ta.kind())?;
            let current = ta.get(idx);
            let expected = args.get(2).cloned().unwrap_or(Value::Undefined);
            let label = if values_equal_strict(&current, &expected) {
                "timed-out"
            } else {
                "not-equal"
            };
            Ok(Value::String(JsString::from_str(label, string_heap)?))
        }
        // §25.4.12 Atomics.notify(typedArray, index, count?). No
        // other thread can ever be waiting on a single-thread VM,
        // so the count of woken waiters is always `0`.
        // <https://tc39.es/ecma262/#sec-atomics.notify>
        "notify" => {
            let (ta, idx) = read_indexed_args(args)?;
            ensure_int_kind(ta.kind())?;
            let _ = idx;
            Ok(Value::Number(NumberValue::from_i32(0)))
        }
        // ECMA-402 / Atomics.waitAsync(typedArray, index, value, timeout?).
        // Single-thread foundation: returns
        // `{ async: false, value: <result> }` per the spec result
        // shape, where `<result>` is a synchronously-fulfilled
        // promise of the equivalent `wait` outcome.
        // <https://tc39.es/ecma262/#sec-atomics.waitasync>
        "waitAsync" => {
            let (ta, idx) = read_indexed_args(args)?;
            ensure_int_kind(ta.kind())?;
            let current = ta.get(idx);
            let expected = args.get(2).cloned().unwrap_or(Value::Undefined);
            let label = if values_equal_strict(&current, &expected) {
                "timed-out"
            } else {
                "not-equal"
            };
            let promise = JsPromiseHandle::fulfilled(
                gc_heap,
                Value::String(JsString::from_str(label, string_heap)?),
            )?;
            let result = crate::object::alloc_object(gc_heap)?;
            crate::object::set(result, gc_heap, "async", Value::Boolean(false));
            crate::object::set(result, gc_heap, "value", Value::Promise(promise));
            Ok(Value::Object(result))
        }
        other => Err(VmError::UnknownIntrinsic {
            name: format!("Atomics.{other}"),
        }),
    }
}

/// Single-thread arithmetic / bitwise modify-and-return-old.
fn atomic_modify(args: &[Value], op: fn(i64, i64) -> i64) -> Result<Value, VmError> {
    let (ta, idx) = read_indexed_args(args)?;
    ensure_int_kind(ta.kind())?;
    let value = args.get(2).cloned().unwrap_or(Value::Undefined);
    let prev = ta.get(idx);
    if ta.kind().is_bigint() {
        let prev_b = match &prev {
            Value::BigInt(b) => b.as_inner().clone(),
            _ => num_bigint::BigInt::from(0),
        };
        let v_b = match &value {
            Value::BigInt(b) => b.as_inner().clone(),
            _ => return Err(VmError::TypeMismatch),
        };
        // Foundation: do the arithmetic in i128 + wrap on store.
        // The kind's write helper handles the modular reduction.
        use num_traits::ToPrimitive;
        let prev_i = prev_b.to_i64().unwrap_or(0);
        let v_i = v_b.to_i64().unwrap_or(0);
        let new_i = op(prev_i, v_i);
        let new_b = num_bigint::BigInt::from(new_i);
        ta.set(
            idx,
            &Value::BigInt(crate::bigint::BigIntValue::from_inner(new_b)),
        );
        return Ok(prev);
    }
    let prev_n = match &prev {
        Value::Number(n) => n.as_f64() as i64,
        _ => 0,
    };
    let v_n = match &value {
        Value::Number(n) => n.as_f64() as i64,
        _ => return Err(VmError::TypeMismatch),
    };
    let new_n = op(prev_n, v_n);
    ta.set(idx, &Value::Number(NumberValue::from_f64(new_n as f64)));
    Ok(prev)
}

fn read_indexed_args(args: &[Value]) -> Result<(JsTypedArray, usize), VmError> {
    let ta = match args.first() {
        Some(Value::TypedArray(t)) => t.clone(),
        _ => return Err(VmError::TypeMismatch),
    };
    let idx = match args.get(1) {
        Some(Value::Number(n)) => {
            let f = n.as_f64();
            if !f.is_finite() || f < 0.0 || f.fract() != 0.0 {
                return Err(VmError::TypeMismatch);
            }
            f as usize
        }
        _ => return Err(VmError::TypeMismatch),
    };
    if idx >= ta.length() {
        return Err(VmError::TypeMismatch);
    }
    Ok((ta, idx))
}

/// §25.4.3.1 ValidateIntegerTypedArray — atomics only operate on
/// integer kinds. Float32 / Float64 raise TypeError.
fn ensure_int_kind(kind: TypedArrayKind) -> Result<(), VmError> {
    match kind {
        TypedArrayKind::Float32 | TypedArrayKind::Float64 => Err(VmError::TypeMismatch),
        _ => Ok(()),
    }
}

fn values_equal_strict(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => crate::number::equals(*x, *y),
        (Value::BigInt(x), Value::BigInt(y)) => x == y,
        _ => false,
    }
}
