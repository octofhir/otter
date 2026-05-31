//! `%TypedArray%.prototype.<name>` per ECMA-262 §23.2.3.
//!
//! All eleven concrete `TypedArray` constructors share one prototype
//! at the spec level; the runtime models that with one bootstrap
//! native surface whose impls read the receiver's
//! [`crate::binary::TypedArrayKind`] off the value to pick
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

use crate::string::JsString;
use crate::{NativeCtx, NativeError, Value};

use super::typed_array::{JsTypedArray, TypedArrayKind};
use super::{number_value, smi};

const NAME: &str = "TypedArray.prototype";

fn type_error(reason: impl Into<String>) -> NativeError {
    NativeError::TypeError {
        name: NAME,
        reason: reason.into(),
    }
}

fn range_error(reason: impl Into<String>) -> NativeError {
    NativeError::RangeError {
        name: NAME,
        reason: reason.into(),
    }
}

fn receiver(ctx: &NativeCtx<'_>) -> Result<JsTypedArray, NativeError> {
    ctx.this_value()
        .as_typed_array(ctx.heap())
        .ok_or_else(|| type_error("expected typedarray"))
}

fn check_not_detached(t: &JsTypedArray, heap: &otter_gc::GcHeap) -> Result<(), NativeError> {
    if t.buffer(heap).is_detached(heap) {
        return Err(type_error("expected non-detached typedarray"));
    }
    Ok(())
}

/// §22.1.3.27 / §23.2.3.34 helper — clamp a relative integer to
/// `[0, len]` per §7.1.5 ToIntegerOrInfinity then offset-from-end
/// for negative values.
fn relative_index(arg: Option<&Value>, default: i64, len: i64) -> i64 {
    let Some(v) = arg else {
        return default;
    };
    if v.is_undefined() {
        return default;
    }
    let n = if let Some(num) = v.as_number() {
        num.as_f64()
    } else if let Some(b) = v.as_boolean() {
        if b { 1.0 } else { 0.0 }
    } else if v.is_null() {
        0.0
    } else {
        return default;
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
    let Some(v) = arg else {
        return default;
    };
    if v.is_undefined() {
        return default;
    }
    let n = if let Some(num) = v.as_number() {
        num.as_f64()
    } else if let Some(b) = v.as_boolean() {
        if b { 1.0 } else { 0.0 }
    } else if v.is_null() {
        0.0
    } else {
        return default;
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

fn native_oom(_: otter_gc::OutOfMemory) -> NativeError {
    type_error("out of memory")
}

fn copy_view(
    t: &JsTypedArray,
    heap: &mut otter_gc::GcHeap,
) -> Result<Vec<Value>, otter_gc::OutOfMemory> {
    let len = t.length(heap);
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        out.push(t.get(heap, i)?);
    }
    Ok(out)
}

fn build_subarray(
    t: &JsTypedArray,
    heap: &mut otter_gc::GcHeap,
    start: usize,
    len: usize,
) -> Result<Value, NativeError> {
    let bpe = t.kind().bytes_per_element();
    let buffer = t.buffer(heap);
    let byte_offset = t.byte_offset(heap) + start * bpe;
    let view = JsTypedArray::new(heap, buffer, t.kind(), byte_offset, len).map_err(native_oom)?;
    Ok(Value::typed_array(view))
}

fn build_new_typed_array(
    ctx: &mut NativeCtx<'_>,
    kind: TypedArrayKind,
    values: &[Value],
) -> Result<Value, NativeError> {
    let bpe = kind.bytes_per_element();
    let byte_len = values
        .len()
        .checked_mul(bpe)
        .ok_or_else(|| range_error("byte length overflow"))?;
    let buf = ctx
        .alloc_array_buffer_zeroed(byte_len, &[], &[values])
        .map_err(native_oom)?
        .ok_or_else(|| type_error("allocation failed"))?;
    let view = JsTypedArray::new(ctx.heap_mut(), buf, kind, 0, values.len()).map_err(native_oom)?;
    for (i, v) in values.iter().enumerate() {
        view.set(ctx.heap_mut(), i, v);
    }
    Ok(Value::typed_array(view))
}

// ---- pure-functional methods --------------------------------------------

fn impl_at(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let t = receiver(ctx)?;
    check_not_detached(&t, ctx.heap())?;
    let len = t.length(ctx.heap_mut()) as i64;
    let idx = if let Some(n) = args.first().and_then(|v| v.as_number()) {
        let f = n.as_f64();
        if !f.is_finite() {
            return Ok(Value::undefined());
        }
        f.trunc() as i64
    } else {
        0
    };
    let resolved = if idx < 0 { len + idx } else { idx };
    if resolved < 0 || resolved >= len {
        return Ok(Value::undefined());
    }
    t.get(ctx.heap_mut(), resolved as usize).map_err(native_oom)
}

fn impl_subarray(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let t = receiver(ctx)?;
    let len = t.length(ctx.heap_mut()) as i64;
    let start = relative_index(args.first(), 0, len);
    let end = relative_index(args.get(1), len, len);
    let final_start = start.clamp(0, len) as usize;
    let final_end = end.clamp(start, len) as usize;
    let new_len = final_end.saturating_sub(final_start);
    build_subarray(&t, ctx.heap_mut(), final_start, new_len)
}

fn impl_slice(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let t = receiver(ctx)?;
    check_not_detached(&t, ctx.heap())?;
    let len = t.length(ctx.heap_mut()) as i64;
    let start = relative_index(args.first(), 0, len);
    let end = relative_index(args.get(1), len, len);
    let final_start = start.clamp(0, len) as usize;
    let final_end = end.clamp(start, len) as usize;
    let new_len = final_end.saturating_sub(final_start);
    let bpe = t.kind().bytes_per_element();
    let byte_len = new_len
        .checked_mul(bpe)
        .ok_or_else(|| range_error("byte length overflow"))?;
    let new_buf = ctx
        .alloc_array_buffer_zeroed(byte_len, &[], &[])
        .map_err(native_oom)?
        .ok_or_else(|| type_error("allocation failed"))?;
    {
        let abs_offset = t.byte_offset(ctx.heap()) + final_start * bpe;
        let buffer = t.buffer(ctx.heap());
        let snapshot: Vec<u8> = buffer.with_bytes(ctx.heap(), |b| {
            b[abs_offset..abs_offset + new_len * bpe].to_vec()
        });
        new_buf.with_bytes_mut(ctx.heap_mut(), |dst| dst.copy_from_slice(&snapshot));
    }
    let view =
        JsTypedArray::new(ctx.heap_mut(), new_buf, t.kind(), 0, new_len).map_err(native_oom)?;
    Ok(Value::typed_array(view))
}

fn impl_fill(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let t = receiver(ctx)?;
    check_not_detached(&t, ctx.heap())?;
    let len = t.length(ctx.heap_mut()) as i64;
    let value = args.first().cloned().unwrap_or(Value::undefined());
    if t.kind().is_bigint() && !value.is_big_int() {
        return Err(type_error("must be a BigInt"));
    }
    if !t.kind().is_bigint() && value.is_big_int() {
        return Err(type_error("must be a Number"));
    }
    let start = relative_index(args.get(1), 0, len);
    let end = relative_index(args.get(2), len, len);
    let s = start.clamp(0, len) as usize;
    let e = end.clamp(start, len) as usize;
    for i in s..e {
        t.set(ctx.heap_mut(), i, &value);
    }
    Ok(Value::typed_array(t))
}

fn impl_copy_within(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let t = receiver(ctx)?;
    check_not_detached(&t, ctx.heap())?;
    let len = t.length(ctx.heap_mut()) as i64;
    let target = relative_index(args.first(), 0, len);
    let start = relative_index(args.get(1), 0, len);
    let end = relative_index(args.get(2), len, len);
    let to = target.clamp(0, len);
    let from = start.clamp(0, len);
    let final_end = end.clamp(start, len);
    let count = (final_end - from).min(len - to).max(0) as usize;
    if count == 0 {
        return Ok(Value::typed_array(t));
    }
    // Memmove by raw bytes through the backing buffer to handle
    // overlap correctly.
    let bpe = t.kind().bytes_per_element();
    let src_off = t.byte_offset(ctx.heap()) + from as usize * bpe;
    let dst_off = t.byte_offset(ctx.heap()) + to as usize * bpe;
    let byte_count = count * bpe;
    let buffer = t.buffer(ctx.heap());
    buffer.with_bytes_mut(ctx.heap_mut(), |buf| {
        buf.copy_within(src_off..src_off + byte_count, dst_off);
    });
    Ok(Value::typed_array(t))
}

fn impl_reverse(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let t = receiver(ctx)?;
    check_not_detached(&t, ctx.heap())?;
    let len = t.length(ctx.heap_mut());
    if len > 1 {
        let mut i = 0usize;
        let mut j = len - 1;
        while i < j {
            let a = t.get(ctx.heap_mut(), i).map_err(native_oom)?;
            let b = t.get(ctx.heap_mut(), j).map_err(native_oom)?;
            t.set(ctx.heap_mut(), i, &b);
            t.set(ctx.heap_mut(), j, &a);
            i += 1;
            j -= 1;
        }
    }
    Ok(Value::typed_array(t))
}

fn impl_index_of(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let t = receiver(ctx)?;
    check_not_detached(&t, ctx.heap())?;
    let len = t.length(ctx.heap_mut()) as i64;
    if len == 0 {
        return Ok(smi(-1));
    }
    let target = args.first().cloned().unwrap_or(Value::undefined());
    let start = integer_arg(args.get(1), 0);
    let from = if start < 0 {
        (len + start).max(0)
    } else {
        start.min(len)
    } as usize;
    for i in from..(len as usize) {
        if values_equal_strict(&t.get(ctx.heap_mut(), i).map_err(native_oom)?, &target) {
            return Ok(smi(i as i32));
        }
    }
    Ok(smi(-1))
}

fn impl_last_index_of(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let t = receiver(ctx)?;
    check_not_detached(&t, ctx.heap())?;
    let len = t.length(ctx.heap_mut()) as i64;
    if len == 0 {
        return Ok(smi(-1));
    }
    let target = args.first().cloned().unwrap_or(Value::undefined());
    let start = integer_arg(args.get(1), len - 1);
    let from = if start < 0 {
        (len + start).max(-1)
    } else {
        start.min(len - 1)
    };
    let mut i = from;
    while i >= 0 {
        if values_equal_strict(
            &t.get(ctx.heap_mut(), i as usize).map_err(native_oom)?,
            &target,
        ) {
            return Ok(smi(i as i32));
        }
        i -= 1;
    }
    Ok(smi(-1))
}

fn impl_includes(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let t = receiver(ctx)?;
    check_not_detached(&t, ctx.heap())?;
    let len = t.length(ctx.heap_mut()) as i64;
    let target = args.first().cloned().unwrap_or(Value::undefined());
    let start = integer_arg(args.get(1), 0);
    let from = if start < 0 {
        (len + start).max(0)
    } else {
        start.min(len)
    } as usize;
    for i in from..(len as usize) {
        if values_equal_zero(&t.get(ctx.heap_mut(), i).map_err(native_oom)?, &target) {
            return Ok(Value::boolean(true));
        }
    }
    Ok(Value::boolean(false))
}

fn impl_join(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let t = receiver(ctx)?;
    check_not_detached(&t, ctx.heap())?;
    let separator = if let Some(v) = args.first() {
        if v.is_undefined() {
            ",".to_string()
        } else if let Some(s) = v.as_string(ctx.heap_mut()) {
            s.to_lossy_string(ctx.heap_mut())
        } else {
            v.display_string(ctx.heap_mut())
        }
    } else {
        ",".to_string()
    };
    join_into_string(&t, &separator, ctx.heap_mut())
}

fn join_into_string(
    t: &crate::binary::JsTypedArray,
    separator: &str,
    gc_heap: &mut otter_gc::GcHeap,
) -> Result<Value, NativeError> {
    let mut out = String::new();
    let len = t.length(gc_heap);
    for i in 0..len {
        if i > 0 {
            out.push_str(separator);
        }
        let v = t.get(gc_heap, i).map_err(native_oom)?;
        if !(v.is_undefined() || v.is_null()) {
            out.push_str(&v.display_string(gc_heap));
        }
    }
    Ok(Value::string(JsString::from_str(&out, gc_heap)?))
}

fn impl_to_string(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let t = receiver(ctx)?;
    check_not_detached(&t, ctx.heap())?;
    join_into_string(&t, ",", ctx.heap_mut())
}

fn impl_to_locale_string(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    // Foundation simplification: locale-aware rendering deferred to
    // Intl integration. Falls through to `toString`.
    impl_to_string(ctx, args)
}

fn impl_set(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let t = receiver(ctx)?;
    check_not_detached(&t, ctx.heap())?;
    let offset = integer_arg(args.get(1), 0);
    if offset < 0 {
        return Err(type_error("must be non-negative"));
    }
    let off = offset as usize;
    let source = args.first().cloned().unwrap_or(Value::undefined());
    let kind = t.kind();
    fn coerce(
        kind: TypedArrayKind,
        v: &Value,
        gc_heap: &mut otter_gc::GcHeap,
    ) -> Result<Value, NativeError> {
        crate::binary::dispatch::coerce_element_for_store(gc_heap, kind, v)
            .map_err(|_| type_error("element type mismatch"))
    }
    if let Some(src) = source.as_typed_array(ctx.heap_mut()) {
        let src_len = src.length(ctx.heap_mut());
        if off + src_len > t.length(ctx.heap_mut()) {
            return Err(range_error("source overruns destination"));
        }
        // Snapshot first to handle aliasing of the same buffer.
        let snapshot: Vec<Value> = {
            let mut tmp = Vec::with_capacity(src_len);
            for i in 0..src_len {
                tmp.push(src.get(ctx.heap_mut(), i).map_err(native_oom)?);
            }
            tmp
        };
        for (i, v) in snapshot.iter().enumerate() {
            let coerced = coerce(kind, v, ctx.heap_mut())?;
            t.set(ctx.heap_mut(), off + i, &coerced);
        }
    } else if let Some(arr) = source.as_array() {
        let src_len = crate::array::len(arr, ctx.heap_mut());
        if off + src_len > t.length(ctx.heap_mut()) {
            return Err(range_error("source overruns destination"));
        }
        for i in 0..src_len {
            let v = crate::array::get(arr, ctx.heap_mut(), i);
            let coerced = coerce(kind, &v, ctx.heap_mut())?;
            t.set(ctx.heap_mut(), off + i, &coerced);
        }
    } else if let Some(obj) = source.as_object() {
        // §22.2.3.23.1 step 14 — array-like Object source.
        let len_value =
            crate::object::get(obj, ctx.heap_mut(), "length").unwrap_or(Value::undefined());
        let len_n = crate::number::to_number_value(&len_value, ctx.heap_mut());
        let src_len = if len_n.is_nan() || len_n <= 0.0 {
            0
        } else {
            len_n.min(9_007_199_254_740_991.0) as usize
        };
        if off + src_len > t.length(ctx.heap_mut()) {
            return Err(range_error("source overruns destination"));
        }
        for i in 0..src_len {
            let key = i.to_string();
            let v = crate::object::get(obj, ctx.heap_mut(), &key).unwrap_or(Value::undefined());
            let coerced = coerce(kind, &v, ctx.heap_mut())?;
            t.set(ctx.heap_mut(), off + i, &coerced);
        }
    } else if let Some(s) = source.as_string(ctx.heap_mut()) {
        // §22.2.3.23.1 step 14 — String indexed-char wrapper.
        let units = s.to_utf16_vec(ctx.heap_mut());
        let src_len = units.len();
        if off + src_len > t.length(ctx.heap_mut()) {
            return Err(range_error("source overruns destination"));
        }
        for (i, unit) in units.iter().enumerate() {
            let ch = char::from_u32(*unit as u32).unwrap_or('\u{FFFD}');
            let s_one = ch.to_string();
            let v = Value::string(JsString::from_str(&s_one, ctx.heap_mut())?);
            let coerced = coerce(kind, &v, ctx.heap_mut())?;
            t.set(ctx.heap_mut(), off + i, &coerced);
        }
    } else if source.is_number()
        || source.is_boolean()
        || source.is_symbol()
        || source.is_big_int()
        || source.is_null()
        || source.is_undefined()
    {
        // Primitive wrappers have no own indexed properties — no-op.
    } else {
        return Err(type_error("must be a TypedArray or array-like"));
    }
    Ok(Value::undefined())
}

fn impl_to_reversed(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let t = receiver(ctx)?;
    check_not_detached(&t, ctx.heap())?;
    let mut snapshot = copy_view(&t, ctx.heap_mut()).map_err(native_oom)?;
    snapshot.reverse();
    build_new_typed_array(ctx, t.kind(), &snapshot)
}

fn impl_to_sorted_default(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let t = receiver(ctx)?;
    check_not_detached(&t, ctx.heap())?;
    let mut snapshot = copy_view(&t, ctx.heap_mut()).map_err(native_oom)?;
    sort_default(&mut snapshot, t.kind().is_bigint(), ctx.heap_mut());
    build_new_typed_array(ctx, t.kind(), &snapshot)
}

fn impl_sort_default(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let t = receiver(ctx)?;
    check_not_detached(&t, ctx.heap())?;
    let mut snapshot = copy_view(&t, ctx.heap_mut()).map_err(native_oom)?;
    sort_default(&mut snapshot, t.kind().is_bigint(), ctx.heap_mut());
    for (i, v) in snapshot.iter().enumerate() {
        t.set(ctx.heap_mut(), i, v);
    }
    Ok(Value::typed_array(t))
}

fn impl_with(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let t = receiver(ctx)?;
    check_not_detached(&t, ctx.heap())?;
    let len = t.length(ctx.heap_mut()) as i64;
    let raw_idx = integer_arg(args.first(), 0);
    let resolved = if raw_idx < 0 { len + raw_idx } else { raw_idx };
    if resolved < 0 || resolved >= len {
        return Err(range_error("index out of range"));
    }
    let value = args.get(1).cloned().unwrap_or(Value::undefined());
    let mut snapshot = copy_view(&t, ctx.heap_mut()).map_err(native_oom)?;
    snapshot[resolved as usize] = value;
    build_new_typed_array(ctx, t.kind(), &snapshot)
}

/// §23.2.5.1 CreateArrayIterator — a *live* TypedArray iterator whose
/// `next()` reads the element at the current index on each step
/// (observing mutations and buffer detachment), unlike a one-shot
/// snapshot.
fn live_typed_array_iterator(
    ctx: &mut NativeCtx<'_>,
    t: crate::binary::typed_array::JsTypedArray,
    kind: crate::iterator_state::ArrayIterKind,
) -> Result<Value, NativeError> {
    let state = crate::IteratorState::TypedArray {
        typed_array: t,
        index: 0,
        kind,
    };
    let root = *ctx.this_value();
    Ok(Value::iterator(
        ctx.alloc_iterator_state(state, &[&root], &[])
            .map_err(native_oom)?,
    ))
}

fn impl_keys(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let t = receiver(ctx)?;
    check_not_detached(&t, ctx.heap())?;
    live_typed_array_iterator(ctx, t, crate::iterator_state::ArrayIterKind::Key)
}

fn impl_values(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let t = receiver(ctx)?;
    check_not_detached(&t, ctx.heap())?;
    live_typed_array_iterator(ctx, t, crate::iterator_state::ArrayIterKind::Value)
}

fn impl_entries(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let t = receiver(ctx)?;
    check_not_detached(&t, ctx.heap())?;
    live_typed_array_iterator(ctx, t, crate::iterator_state::ArrayIterKind::Entry)
}

// ---- comparison helpers -------------------------------------------------

fn values_equal_strict(a: &Value, b: &Value) -> bool {
    if let (Some(x), Some(y)) = (a.as_number(), b.as_number()) {
        crate::number::equals(x, y)
    } else if let (Some(x), Some(y)) = (a.as_big_int(), b.as_big_int()) {
        x == y
    } else {
        false
    }
}

/// SameValueZero — like strict equality but `NaN === NaN`.
fn values_equal_zero(a: &Value, b: &Value) -> bool {
    if let (Some(x), Some(y)) = (a.as_number(), b.as_number()) {
        if x.is_nan() && y.is_nan() {
            return true;
        }
        crate::number::equals(x, y)
    } else if let (Some(x), Some(y)) = (a.as_big_int(), b.as_big_int()) {
        x == y
    } else {
        false
    }
}

/// Default sort: numeric ascending for number kinds, BigInt
/// ascending for BigInt kinds. Per §23.2.3.30 step 4.
fn sort_default(values: &mut [Value], bigint_kind: bool, heap: &otter_gc::GcHeap) {
    if bigint_kind {
        values.sort_by(|a, b| {
            if let (Some(x), Some(y)) = (a.as_big_int(), b.as_big_int()) {
                x.with_inner(heap, |xb| y.with_inner(heap, |yb| xb.cmp(yb)))
            } else {
                std::cmp::Ordering::Equal
            }
        });
    } else {
        values.sort_by(|a, b| {
            let x = a.as_number().map(|n| n.as_f64()).unwrap_or(0.0);
            let y = b.as_number().map(|n| n.as_f64()).unwrap_or(0.0);
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

// ---- native dispatch ----------------------------------------------------

type TypedArrayNativeFn = fn(&mut NativeCtx<'_>, &[Value]) -> Result<Value, NativeError>;

/// Resolve a `%TypedArray%.prototype` method implemented in this
/// module. Callback-driven methods live in `bootstrap_typed_array`.
#[must_use]
pub fn method_impl(name: &str) -> Option<TypedArrayNativeFn> {
    Some(match name {
        "at" => impl_at,
        "subarray" => impl_subarray,
        "slice" => impl_slice,
        "fill" => impl_fill,
        "copyWithin" => impl_copy_within,
        "reverse" => impl_reverse,
        "indexOf" => impl_index_of,
        "lastIndexOf" => impl_last_index_of,
        "includes" => impl_includes,
        "join" => impl_join,
        "toString" => impl_to_string,
        "toLocaleString" => impl_to_locale_string,
        "set" => impl_set,
        "toReversed" => impl_to_reversed,
        "toSorted" => impl_to_sorted_default,
        "sort" => impl_sort_default,
        "with" => impl_with,
        "keys" => impl_keys,
        "values" => impl_values,
        "entries" => impl_entries,
        _ => return None,
    })
}

/// `%TypedArray%.prototype` getter access for `buffer`, `byteLength`,
/// `byteOffset`, `length`, `BYTES_PER_ELEMENT`, and `Symbol.toStringTag`.
/// Routed through `Op::LoadProperty`. `BYTES_PER_ELEMENT` is reported as
/// the receiver's kind value per §23.2.5 step 1.
#[must_use]
pub fn load_property(t: &JsTypedArray, heap: &otter_gc::GcHeap, name: &str) -> Value {
    match name {
        "buffer" => Value::array_buffer(t.buffer(heap)),
        "byteLength" => smi(t.byte_length(heap) as i32),
        "byteOffset" => smi(t.byte_offset(heap) as i32),
        "length" => smi(t.length(heap) as i32),
        "BYTES_PER_ELEMENT" => smi(t.kind().bytes_per_element() as i32),
        _ => {
            let _ = number_value;
            Value::undefined()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::array_buffer::JsArrayBuffer;
    use super::*;
    use crate::{Interpreter, NativeCallInfo};

    #[test]
    fn typed_array_entries_uses_native_rooted_young_allocation() {
        let mut interp = Interpreter::new();
        let receiver = {
            let heap = interp.gc_heap_mut();
            let buffer = JsArrayBuffer::new(heap, 2).expect("array buffer");
            let view =
                JsTypedArray::new(heap, buffer, TypedArrayKind::Int8, 0, 2).expect("typed array");
            Value::typed_array(view)
        };
        let before = interp.gc_heap().stats().new_allocated_bytes;

        let result = {
            let mut ctx =
                NativeCtx::new_with_call_info(&mut interp, NativeCallInfo::call(receiver));
            impl_entries(&mut ctx, &[]).expect("entries")
        };

        let after = interp.gc_heap().stats().new_allocated_bytes;
        assert!(
            after > before,
            "TypedArray.prototype.entries should allocate pair arrays, snapshot array, and iterator state in young space"
        );
        assert!(result.is_iterator());
    }

    #[test]
    fn typed_array_slice_uses_native_rooted_backing_store() {
        let mut interp = Interpreter::new();
        let receiver = {
            let heap = interp.gc_heap_mut();
            let buffer = JsArrayBuffer::new(heap, 4).expect("array buffer");
            let source =
                JsTypedArray::new(heap, buffer, TypedArrayKind::Int16, 0, 2).expect("typed array");
            source.set(heap, 0, &smi(7));
            source.set(heap, 1, &smi(11));
            Value::typed_array(source)
        };

        let result = {
            let mut ctx =
                NativeCtx::new_with_call_info(&mut interp, NativeCallInfo::call(receiver));
            impl_slice(&mut ctx, &[]).expect("slice")
        };

        assert!(result.is_typed_array());
        let result_view = result
            .as_typed_array(interp.gc_heap())
            .expect("slice result typed array");
        assert_eq!(result_view.length(interp.gc_heap()), 2);
        assert_eq!(
            result_view
                .get(interp.gc_heap_mut(), 0)
                .expect("first element")
                .as_number()
                .expect("number")
                .as_smi(),
            Some(7)
        );
        assert_eq!(
            result_view
                .get(interp.gc_heap_mut(), 1)
                .expect("second element")
                .as_number()
                .expect("number")
                .as_smi(),
            Some(11)
        );
        let _ = result;
        interp.gc_heap_mut().collect_full(&mut |_| {});
    }
}
