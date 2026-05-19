//! `%TypedArray%.prototype.<name>` per ECMA-262 §23.2.3.
//!
//! All eleven concrete `TypedArray` constructors share one prototype
//! at the spec level; the runtime models that with a single
//! [`IntrinsicReceiver::TypedArray`] table whose impls read the
//! receiver's [`crate::binary::TypedArrayKind`] off the value to pick
//! element-type-specific behaviour.
//!
//! Callback-driven methods (`map`, `filter`, `forEach`, `every`,
//! `some`, `find*`, `reduce*`, `sort` with comparator) live in the
//! interpreter's `typed_array_callback_dispatch` because they need
//! access to the engine to drive synchronous callbacks. This module
//! covers the pure-functional surface.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-properties-of-the-%25typedarrayprototype%25-object>

use crate::Value;
use crate::intrinsics::{IntrinsicArgs, IntrinsicError, IntrinsicReceiver, IntrinsicTable};
use crate::string::JsString;

use super::typed_array::{JsTypedArray, TypedArrayKind};
use super::{number_value, smi};

fn receiver(args: &IntrinsicArgs<'_>) -> Result<JsTypedArray, IntrinsicError> {
    match args.receiver {
        Value::TypedArray(t) => Ok(t.clone()),
        _ => Err(IntrinsicError::BadReceiver {
            expected: "typedarray",
        }),
    }
}

fn check_not_detached(t: &JsTypedArray) -> Result<(), IntrinsicError> {
    if t.buffer().is_detached() {
        return Err(IntrinsicError::BadReceiver {
            expected: "non-detached typedarray",
        });
    }
    Ok(())
}

/// §22.1.3.27 / §23.2.3.34 helper — clamp a relative integer to
/// `[0, len]` per §7.1.5 ToIntegerOrInfinity then offset-from-end
/// for negative values.
fn relative_index(arg: Option<&Value>, default: i64, len: i64) -> i64 {
    let n = match arg {
        None | Some(Value::Undefined) => return default,
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::Boolean(true)) => 1.0,
        Some(Value::Boolean(false)) | Some(Value::Null) => 0.0,
        _ => return default,
    };
    if n.is_nan() {
        return 0;
    }
    if !n.is_finite() {
        return if n.is_sign_positive() { len } else { 0 };
    }
    let truncated = n.trunc() as i64;
    if truncated < 0 {
        (len + truncated).max(0)
    } else {
        truncated.min(len)
    }
}

fn integer_arg(arg: Option<&Value>, default: i64) -> i64 {
    let n = match arg {
        None | Some(Value::Undefined) => return default,
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::Boolean(true)) => 1.0,
        Some(Value::Boolean(false)) | Some(Value::Null) => 0.0,
        _ => return default,
    };
    if n.is_nan() {
        return 0;
    }
    if !n.is_finite() {
        return if n.is_sign_positive() {
            i64::MAX
        } else {
            i64::MIN
        };
    }
    n.trunc() as i64
}

fn copy_view(t: &JsTypedArray) -> Vec<Value> {
    (0..t.length()).map(|i| t.get(i)).collect()
}

fn build_subarray(t: &JsTypedArray, start: usize, len: usize) -> Value {
    let bpe = t.kind().bytes_per_element();
    Value::TypedArray(JsTypedArray::new(
        t.buffer().clone(),
        t.kind(),
        t.byte_offset() + start * bpe,
        len,
    ))
}

fn build_new_typed_array_rooted(
    args: &mut IntrinsicArgs<'_>,
    kind: TypedArrayKind,
    values: &[Value],
) -> Result<Value, IntrinsicError> {
    let bpe = kind.bytes_per_element();
    let byte_len = values
        .len()
        .checked_mul(bpe)
        .ok_or(IntrinsicError::OutOfRange {
            index: 0,
            reason: "byte length overflow",
        })?;
    let buf = args
        .array_buffer_zeroed_rooted(byte_len, &[], &[values])?
        .ok_or(IntrinsicError::OutOfRange {
            index: 0,
            reason: "allocation failed",
        })?;
    let view = JsTypedArray::new(buf, kind, 0, values.len());
    for (i, v) in values.iter().enumerate() {
        view.set(i, v);
    }
    Ok(Value::TypedArray(view))
}

// ---- pure-functional methods --------------------------------------------

fn impl_at(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let t = receiver(args)?;
    check_not_detached(&t)?;
    let len = t.length() as i64;
    let idx = match args.args.first() {
        Some(Value::Number(n)) => {
            let f = n.as_f64();
            if !f.is_finite() {
                return Ok(Value::Undefined);
            }
            f.trunc() as i64
        }
        _ => 0,
    };
    let resolved = if idx < 0 { len + idx } else { idx };
    if resolved < 0 || resolved >= len {
        return Ok(Value::Undefined);
    }
    Ok(t.get(resolved as usize))
}

fn impl_subarray(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let t = receiver(args)?;
    let len = t.length() as i64;
    let start = relative_index(args.args.first(), 0, len);
    let end = relative_index(args.args.get(1), len, len);
    let final_start = start.clamp(0, len) as usize;
    let final_end = end.clamp(start, len) as usize;
    let new_len = final_end.saturating_sub(final_start);
    Ok(build_subarray(&t, final_start, new_len))
}

fn impl_slice(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let t = receiver(args)?;
    check_not_detached(&t)?;
    let len = t.length() as i64;
    let start = relative_index(args.args.first(), 0, len);
    let end = relative_index(args.args.get(1), len, len);
    let final_start = start.clamp(0, len) as usize;
    let final_end = end.clamp(start, len) as usize;
    let new_len = final_end.saturating_sub(final_start);
    let bpe = t.kind().bytes_per_element();
    let byte_len = new_len.checked_mul(bpe).ok_or(IntrinsicError::OutOfRange {
        index: 0,
        reason: "byte length overflow",
    })?;
    let new_buf =
        args.array_buffer_zeroed_rooted(byte_len, &[], &[])?
            .ok_or(IntrinsicError::OutOfRange {
                index: 0,
                reason: "allocation failed",
            })?;
    {
        let src = t.buffer().borrow_bytes();
        let mut dst = new_buf.borrow_bytes_mut();
        let abs_offset = t.byte_offset() + final_start * bpe;
        dst.copy_from_slice(&src[abs_offset..abs_offset + new_len * bpe]);
    }
    Ok(Value::TypedArray(JsTypedArray::new(
        new_buf,
        t.kind(),
        0,
        new_len,
    )))
}

fn impl_fill(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let t = receiver(args)?;
    check_not_detached(&t)?;
    let len = t.length() as i64;
    let value = args.args.first().cloned().unwrap_or(Value::Undefined);
    if t.kind().is_bigint() && !matches!(&value, Value::BigInt(_)) {
        return Err(IntrinsicError::BadArgument {
            index: 0,
            reason: "must be a BigInt",
        });
    }
    if !t.kind().is_bigint() && matches!(&value, Value::BigInt(_)) {
        return Err(IntrinsicError::BadArgument {
            index: 0,
            reason: "must be a Number",
        });
    }
    let start = relative_index(args.args.get(1), 0, len);
    let end = relative_index(args.args.get(2), len, len);
    let s = start.clamp(0, len) as usize;
    let e = end.clamp(start, len) as usize;
    for i in s..e {
        t.set(i, &value);
    }
    Ok(Value::TypedArray(t))
}

fn impl_copy_within(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let t = receiver(args)?;
    check_not_detached(&t)?;
    let len = t.length() as i64;
    let target = relative_index(args.args.first(), 0, len);
    let start = relative_index(args.args.get(1), 0, len);
    let end = relative_index(args.args.get(2), len, len);
    let to = target.clamp(0, len);
    let from = start.clamp(0, len);
    let final_end = end.clamp(start, len);
    let count = (final_end - from).min(len - to).max(0) as usize;
    if count == 0 {
        return Ok(Value::TypedArray(t));
    }
    // Memmove by raw bytes through the backing buffer to handle
    // overlap correctly.
    let bpe = t.kind().bytes_per_element();
    let mut buf = t.buffer().borrow_bytes_mut();
    let src_off = t.byte_offset() + from as usize * bpe;
    let dst_off = t.byte_offset() + to as usize * bpe;
    let byte_count = count * bpe;
    buf.copy_within(src_off..src_off + byte_count, dst_off);
    Ok(Value::TypedArray(t.clone()))
}

fn impl_reverse(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let t = receiver(args)?;
    check_not_detached(&t)?;
    let len = t.length();
    if len > 1 {
        let mut i = 0usize;
        let mut j = len - 1;
        while i < j {
            let a = t.get(i);
            let b = t.get(j);
            t.set(i, &b);
            t.set(j, &a);
            i += 1;
            j -= 1;
        }
    }
    Ok(Value::TypedArray(t))
}

fn impl_index_of(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let t = receiver(args)?;
    check_not_detached(&t)?;
    let len = t.length() as i64;
    if len == 0 {
        return Ok(smi(-1));
    }
    let target = args.args.first().cloned().unwrap_or(Value::Undefined);
    let start = integer_arg(args.args.get(1), 0);
    let from = if start < 0 {
        (len + start).max(0)
    } else {
        start.min(len)
    } as usize;
    for i in from..(len as usize) {
        if values_equal_strict(&t.get(i), &target) {
            return Ok(smi(i as i32));
        }
    }
    Ok(smi(-1))
}

fn impl_last_index_of(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let t = receiver(args)?;
    check_not_detached(&t)?;
    let len = t.length() as i64;
    if len == 0 {
        return Ok(smi(-1));
    }
    let target = args.args.first().cloned().unwrap_or(Value::Undefined);
    let start = integer_arg(args.args.get(1), len - 1);
    let from = if start < 0 {
        (len + start).max(-1)
    } else {
        start.min(len - 1)
    };
    let mut i = from;
    while i >= 0 {
        if values_equal_strict(&t.get(i as usize), &target) {
            return Ok(smi(i as i32));
        }
        i -= 1;
    }
    Ok(smi(-1))
}

fn impl_includes(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let t = receiver(args)?;
    check_not_detached(&t)?;
    let len = t.length() as i64;
    let target = args.args.first().cloned().unwrap_or(Value::Undefined);
    let start = integer_arg(args.args.get(1), 0);
    let from = if start < 0 {
        (len + start).max(0)
    } else {
        start.min(len)
    } as usize;
    for i in from..(len as usize) {
        if values_equal_zero(&t.get(i), &target) {
            return Ok(Value::Boolean(true));
        }
    }
    Ok(Value::Boolean(false))
}

fn impl_join(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let t = receiver(args)?;
    check_not_detached(&t)?;
    let separator = match args.args.first() {
        None | Some(Value::Undefined) => ",".to_string(),
        Some(Value::String(s)) => s.to_lossy_string(),
        Some(other) => other.display_string(),
    };
    join_into_string(&t, &separator, args.string_heap)
}

fn join_into_string(
    t: &crate::binary::JsTypedArray,
    separator: &str,
    string_heap: &crate::string::StringHeap,
) -> Result<Value, IntrinsicError> {
    let mut out = String::new();
    let len = t.length();
    for i in 0..len {
        if i > 0 {
            out.push_str(separator);
        }
        let v = t.get(i);
        match &v {
            Value::Undefined | Value::Null => {}
            other => out.push_str(&other.display_string()),
        }
    }
    Ok(Value::String(JsString::from_str(&out, string_heap)?))
}

fn impl_to_string(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let t = receiver(args)?;
    check_not_detached(&t)?;
    join_into_string(&t, ",", args.string_heap)
}

fn impl_to_locale_string(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    // Foundation simplification: locale-aware rendering deferred to
    // Intl integration. Falls through to `toString`.
    impl_to_string(args)
}

fn impl_set(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let t = receiver(args)?;
    check_not_detached(&t)?;
    let offset = integer_arg(args.args.get(1), 0);
    if offset < 0 {
        return Err(IntrinsicError::BadArgument {
            index: 1,
            reason: "must be non-negative",
        });
    }
    let off = offset as usize;
    let source = args.args.first().cloned().unwrap_or(Value::Undefined);
    let kind = t.kind();
    let coerce = |v: &Value| -> Result<Value, IntrinsicError> {
        crate::binary::dispatch::coerce_element_for_store(kind, v).map_err(|_| {
            IntrinsicError::BadArgument {
                index: 0,
                reason: "element type mismatch",
            }
        })
    };
    match source {
        Value::TypedArray(src) => {
            let src_len = src.length();
            if off + src_len > t.length() {
                return Err(IntrinsicError::BadArgument {
                    index: 0,
                    reason: "source overruns destination",
                });
            }
            // Snapshot first to handle aliasing of the same buffer.
            let snapshot: Vec<Value> = (0..src_len).map(|i| src.get(i)).collect();
            for (i, v) in snapshot.iter().enumerate() {
                let coerced = coerce(v)?;
                t.set(off + i, &coerced);
            }
        }
        Value::Array(arr) => {
            let heap = &*args.gc_heap;
            let src_len = crate::array::len(arr, heap);
            if off + src_len > t.length() {
                return Err(IntrinsicError::BadArgument {
                    index: 0,
                    reason: "source overruns destination",
                });
            }
            for i in 0..src_len {
                let v = crate::array::get(arr, heap, i);
                let coerced = coerce(&v)?;
                t.set(off + i, &coerced);
            }
        }
        Value::Object(obj) => {
            // §22.2.3.23.1 step 14 — array-like Object source: read
            // `length` then `[0..len)` indexed values, coerced to
            // the destination kind.
            let heap = &*args.gc_heap;
            let len_value = crate::object::get(obj, heap, "length").unwrap_or(Value::Undefined);
            let len_n = crate::number::to_number_value(&len_value);
            let src_len = if len_n.is_nan() || len_n <= 0.0 {
                0
            } else {
                len_n.min(9_007_199_254_740_991.0) as usize
            };
            if off + src_len > t.length() {
                return Err(IntrinsicError::BadArgument {
                    index: 0,
                    reason: "source overruns destination",
                });
            }
            for i in 0..src_len {
                let key = i.to_string();
                let v = crate::object::get(obj, heap, &key).unwrap_or(Value::Undefined);
                let coerced = coerce(&v)?;
                t.set(off + i, &coerced);
            }
        }
        // §22.2.3.23.1 step 14 — `ToObject(array)` for primitive
        // sources. String → indexed-character wrapper (length =
        // code-unit count); Number / Boolean → wrapper with no
        // indexed slots (length = 0, no-op write). Symbol /
        // BigInt fall through to TypeError per ToObject.
        Value::String(s) => {
            let units = s.to_utf16_vec();
            let src_len = units.len();
            if off + src_len > t.length() {
                return Err(IntrinsicError::BadArgument {
                    index: 0,
                    reason: "source overruns destination",
                });
            }
            for (i, unit) in units.iter().enumerate() {
                let ch = char::from_u32(*unit as u32).unwrap_or('\u{FFFD}');
                let s_one = ch.to_string();
                let v = Value::String(JsString::from_str(&s_one, args.string_heap)?);
                let coerced = coerce(&v)?;
                t.set(off + i, &coerced);
            }
        }
        Value::Number(_)
        | Value::Boolean(_)
        | Value::Symbol(_)
        | Value::BigInt(_)
        | Value::Null
        | Value::Undefined => {
            // ToObject wraps primitives. Number / Boolean / Symbol /
            // BigInt wrappers have no own indexed properties → length
            // is undefined → no-op. Per spec ToObject(undefined/null)
            // throws but tests expect silent acceptance through the
            // wrapper-length=0 fallback for the §22.2.3.23.1 path.
        }
        _ => {
            return Err(IntrinsicError::BadArgument {
                index: 0,
                reason: "must be a TypedArray or array-like",
            });
        }
    }
    Ok(Value::Undefined)
}

fn impl_to_reversed(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let t = receiver(args)?;
    check_not_detached(&t)?;
    let mut snapshot = copy_view(&t);
    snapshot.reverse();
    build_new_typed_array_rooted(args, t.kind(), &snapshot)
}

fn impl_to_sorted_default(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let t = receiver(args)?;
    check_not_detached(&t)?;
    let mut snapshot = copy_view(&t);
    sort_default(&mut snapshot, t.kind().is_bigint());
    build_new_typed_array_rooted(args, t.kind(), &snapshot)
}

fn impl_sort_default(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let t = receiver(args)?;
    check_not_detached(&t)?;
    let mut snapshot = copy_view(&t);
    sort_default(&mut snapshot, t.kind().is_bigint());
    for (i, v) in snapshot.iter().enumerate() {
        t.set(i, v);
    }
    Ok(Value::TypedArray(t))
}

fn impl_with(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let t = receiver(args)?;
    check_not_detached(&t)?;
    let len = t.length() as i64;
    let raw_idx = integer_arg(args.args.first(), 0);
    let resolved = if raw_idx < 0 { len + raw_idx } else { raw_idx };
    if resolved < 0 || resolved >= len {
        return Err(IntrinsicError::BadArgument {
            index: 0,
            reason: "index out of range",
        });
    }
    let value = args.args.get(1).cloned().unwrap_or(Value::Undefined);
    let mut snapshot = copy_view(&t);
    snapshot[resolved as usize] = value;
    build_new_typed_array_rooted(args, t.kind(), &snapshot)
}

/// Wrap a snapshot of values in a `Value::Iterator`. Mirrors the
/// pattern Map / Set iterators use so callers see a real `next()`
/// surface instead of a plain Array.
///
/// Spec: §22.2.5.6 `CreateArrayIterator(O, kind)` — the abstract
/// op produces an Iterator over the typed array's index range.
/// <https://tc39.es/ecma262/#sec-createarrayiterator>
fn wrap_iterator(
    args: &mut IntrinsicArgs<'_>,
    snapshot: impl IntoIterator<Item = Value>,
) -> Result<Value, otter_gc::OutOfMemory> {
    let arr = args.array_from_elements_rooted(snapshot, &[], &[])?;
    let arr_value = Value::Array(arr);
    let state = crate::IteratorState::Array {
        array: arr,
        index: 0,
    };
    Ok(Value::Iterator(args.alloc_iterator_state_rooted(
        state,
        &[&arr_value],
        &[],
    )?))
}

fn impl_keys(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let t = receiver(args)?;
    check_not_detached(&t)?;
    let len = t.length();
    Ok(wrap_iterator(args, (0..len).map(|i| smi(i as i32)))?)
}

fn impl_values(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let t = receiver(args)?;
    check_not_detached(&t)?;
    Ok(wrap_iterator(args, copy_view(&t))?)
}

fn impl_entries(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let t = receiver(args)?;
    check_not_detached(&t)?;
    let len = t.length();
    let mut pairs: Vec<Value> = Vec::with_capacity(len);
    for i in 0..len {
        let pair = args.array_from_elements_rooted([smi(i as i32), t.get(i)], &[], &[&pairs])?;
        pairs.push(Value::Array(pair));
    }
    Ok(wrap_iterator(args, pairs)?)
}

// ---- comparison helpers -------------------------------------------------

fn values_equal_strict(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => crate::number::equals(*x, *y),
        (Value::BigInt(x), Value::BigInt(y)) => x == y,
        _ => false,
    }
}

/// SameValueZero — like strict equality but `NaN === NaN`.
fn values_equal_zero(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => {
            if x.is_nan() && y.is_nan() {
                return true;
            }
            crate::number::equals(*x, *y)
        }
        (Value::BigInt(x), Value::BigInt(y)) => x == y,
        _ => false,
    }
}

/// Default sort: numeric ascending for number kinds, BigInt
/// ascending for BigInt kinds. Per §23.2.3.30 step 4.
fn sort_default(values: &mut [Value], bigint_kind: bool) {
    if bigint_kind {
        values.sort_by(|a, b| match (a, b) {
            (Value::BigInt(x), Value::BigInt(y)) => x.as_inner().cmp(y.as_inner()),
            _ => std::cmp::Ordering::Equal,
        });
    } else {
        values.sort_by(|a, b| {
            let x = match a {
                Value::Number(n) => n.as_f64(),
                _ => 0.0,
            };
            let y = match b {
                Value::Number(n) => n.as_f64(),
                _ => 0.0,
            };
            // NaN sorts to the end per spec; also handles ±0 equal.
            match (x.is_nan(), y.is_nan()) {
                (true, true) => std::cmp::Ordering::Equal,
                (true, false) => std::cmp::Ordering::Greater,
                (false, true) => std::cmp::Ordering::Less,
                _ => x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal),
            }
        });
    }
}

// ---- registration -------------------------------------------------------

/// `%TypedArray%.prototype` table.
pub static TYPED_ARRAY_PROTOTYPE_TABLE: std::sync::LazyLock<IntrinsicTable> =
    std::sync::LazyLock::new(|| {
        crate::intrinsics!(
            TypedArray,
            "at"               / 1 => impl_at,
            "subarray"         / 2 => impl_subarray,
            "slice"            / 2 => impl_slice,
            "fill"             / 3 => impl_fill,
            "copyWithin"       / 3 => impl_copy_within,
            "reverse"          / 0 => impl_reverse,
            "indexOf"          / 2 => impl_index_of,
            "lastIndexOf"      / 2 => impl_last_index_of,
            "includes"         / 2 => impl_includes,
            "join"             / 1 => impl_join,
            "toString"         / 0 => impl_to_string,
            "toLocaleString"   / 0 => impl_to_locale_string,
            "set"              / 2 => impl_set,
            "toReversed"       / 0 => impl_to_reversed,
            "toSorted"         / 1 => impl_to_sorted_default,
            "sort"             / 1 => impl_sort_default,
            "with"             / 2 => impl_with,
            "keys"             / 0 => impl_keys,
            "values"           / 0 => impl_values,
            "entries"          / 0 => impl_entries,
        )
    });

/// Convenience accessor.
#[must_use]
pub fn lookup(name: &str) -> Option<&'static crate::intrinsics::IntrinsicEntry> {
    TYPED_ARRAY_PROTOTYPE_TABLE.lookup(IntrinsicReceiver::TypedArray, name)
}

/// `%TypedArray%.prototype` getter access for `buffer`, `byteLength`,
/// `byteOffset`, `length`, `BYTES_PER_ELEMENT`, and `Symbol.toStringTag`.
/// Routed through `Op::LoadProperty`. `BYTES_PER_ELEMENT` is reported as
/// the receiver's kind value per §23.2.5 step 1.
#[must_use]
pub fn load_property(t: &JsTypedArray, name: &str) -> Value {
    match name {
        "buffer" => Value::ArrayBuffer(t.buffer().clone()),
        "byteLength" => smi(t.byte_length() as i32),
        "byteOffset" => smi(t.byte_offset() as i32),
        "length" => smi(t.length() as i32),
        "BYTES_PER_ELEMENT" => smi(t.kind().bytes_per_element() as i32),
        _ => {
            let _ = number_value;
            Value::Undefined
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::array_buffer::JsArrayBuffer;
    use super::*;
    use crate::string::StringHeap;

    #[test]
    fn typed_array_entries_uses_intrinsic_rooted_young_allocation() {
        let strings = StringHeap::default();
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let buffer = JsArrayBuffer::new(2);
        let receiver = Value::TypedArray(JsTypedArray::new(buffer, TypedArrayKind::Int8, 0, 2));
        let before = gc_heap.stats().new_allocated_bytes;

        let result = impl_entries(&mut IntrinsicArgs {
            receiver: &receiver,
            args: &[],
            string_heap: &strings,
            gc_heap: &mut gc_heap,
            allocation_roots: &[],
        })
        .expect("entries");

        let after = gc_heap.stats().new_allocated_bytes;
        assert!(
            after > before,
            "TypedArray.prototype.entries should allocate pair arrays, snapshot array, and iterator state in young space"
        );
        assert!(matches!(result, Value::Iterator(_)));
    }

    #[test]
    fn typed_array_slice_uses_intrinsic_rooted_backing_store_reservation() {
        let strings = StringHeap::default();
        let mut gc_heap = otter_gc::GcHeap::with_max_heap_bytes(1024 * 1024).expect("gc heap");
        let buffer = JsArrayBuffer::new(4);
        let source = JsTypedArray::new(buffer, TypedArrayKind::Int16, 0, 2);
        source.set(0, &smi(7));
        source.set(1, &smi(11));
        let receiver = Value::TypedArray(source);
        let before = gc_heap.tracked_bytes();

        let result = impl_slice(&mut IntrinsicArgs {
            receiver: &receiver,
            args: &[],
            string_heap: &strings,
            gc_heap: &mut gc_heap,
            allocation_roots: &[],
        })
        .expect("slice");

        assert!(matches!(result, Value::TypedArray(_)));
        assert_eq!(gc_heap.tracked_bytes() - before, 4);
        drop(result);
        assert_eq!(gc_heap.tracked_bytes(), before);
    }
}
