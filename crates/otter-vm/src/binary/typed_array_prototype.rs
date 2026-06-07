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

/// `ToIntegerOrInfinity(fromIndex)` for the `indexOf` / `lastIndexOf` /
/// `includes` search methods. Unlike [`integer_arg`] this runs the full
/// `ToNumber` so a `valueOf` / `@@toPrimitive` hook fires (and may detach
/// the buffer) and a Symbol / BigInt argument throws. The detached / range
/// checks that follow are the caller's responsibility, matching the spec
/// order (`ValidateTypedArray` precedes `ToIntegerOrInfinity`).
fn integer_index_arg(
    ctx: &mut NativeCtx<'_>,
    arg: Option<&Value>,
    default: i64,
) -> Result<i64, NativeError> {
    let Some(v) = arg else {
        return Ok(default);
    };
    if v.is_undefined() {
        return Ok(default);
    }
    let exec = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| type_error("missing execution context"))?;
    let n = crate::coerce::to_number_or_throw(ctx.interp_mut(), &exec, v)
        .map_err(|e| crate::native_function::vm_to_native_error(e, "TypedArray.prototype"))?
        .as_f64();
    if n.is_nan() {
        return Ok(0);
    }
    if !n.is_finite() {
        return Ok(if n.is_sign_positive() {
            i64::MAX
        } else {
            i64::MIN
        });
    }
    Ok(n.trunc() as i64)
}

/// Integer-indexed `[[Get]]` for a possibly-detached view. After a
/// `fromIndex` coercion detaches (or a resizable buffer shrinks) the
/// backing buffer, `align-detached-buffer-semantics-with-web-reality`
/// makes the index read yield `undefined` rather than throwing, so the
/// search loops keep running against the length captured before coercion.
fn ta_element_or_undefined(t: &JsTypedArray, ctx: &mut NativeCtx<'_>, i: usize) -> Value {
    if t.buffer(ctx.heap()).is_detached(ctx.heap()) || i >= t.length(ctx.heap_mut()) {
        return Value::undefined();
    }
    t.get(ctx.heap_mut(), i).unwrap_or(Value::undefined())
}

/// §23.2.3.26 step 5 — `ToIntegerOrInfinity(offset)` for
/// `%TypedArray%.prototype.set`. Runs `ToNumber` through the
/// interpreter so a `valueOf` / `@@toPrimitive` hook fires (it may
/// detach the target buffer) and its abrupt completion propagates,
/// then truncates toward zero (`NaN` → 0, `±Infinity` preserved). The
/// caller maps a negative or out-of-bounds result to a `RangeError`.
fn ta_set_offset(ctx: &mut NativeCtx<'_>, arg: Option<&Value>) -> Result<f64, NativeError> {
    let undefined = Value::undefined();
    let value = arg.unwrap_or(&undefined);
    let exec = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| type_error("missing execution context"))?;
    let number = crate::coerce::to_number_or_throw(ctx.interp_mut(), &exec, value)
        .map_err(|e| crate::native_function::vm_to_native_error(e, "TypedArray.prototype.set"))?;
    let n = number.as_f64();
    Ok(if n.is_nan() { 0.0 } else { n.trunc() })
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
        if f.is_nan() {
            // §23.2.3.1 — ToIntegerOrInfinity(NaN) is +0.
            0
        } else if !f.is_finite() {
            return Ok(Value::undefined());
        } else {
            f.trunc() as i64
        }
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
    // §23.2.3.16 — ValidateTypedArray and the length read precede
    // ToIntegerOrInfinity(fromIndex), so `len` is captured before the
    // coercion's `valueOf` can detach the buffer.
    check_not_detached(&t, ctx.heap())?;
    let len = t.length(ctx.heap_mut()) as i64;
    if len == 0 {
        return Ok(smi(-1));
    }
    let target = args.first().cloned().unwrap_or(Value::undefined());
    let start = integer_index_arg(ctx, args.get(1), 0)?;
    let from = if start < 0 {
        (len + start).max(0)
    } else {
        start.min(len)
    } as usize;
    for i in from..(len as usize) {
        if values_equal_strict(&ta_element_or_undefined(&t, ctx, i), &target, ctx.heap()) {
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
    // §23.2.3.20 — a PRESENT-but-undefined fromIndex still runs
    // ToIntegerOrInfinity (yielding +0); only an absent argument
    // defaults to len-1.
    let start = if args.len() > 1 && args[1].is_undefined() {
        0
    } else {
        integer_index_arg(ctx, args.get(1), len - 1)?
    };
    let from = if start < 0 {
        (len + start).max(-1)
    } else {
        start.min(len - 1)
    };
    let mut i = from;
    while i >= 0 {
        if values_equal_strict(
            &ta_element_or_undefined(&t, ctx, i as usize),
            &target,
            ctx.heap(),
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
    if len == 0 {
        return Ok(Value::boolean(false));
    }
    let target = args.first().cloned().unwrap_or(Value::undefined());
    let start = integer_index_arg(ctx, args.get(1), 0)?;
    let from = if start < 0 {
        (len + start).max(0)
    } else {
        start.min(len)
    } as usize;
    for i in from..(len as usize) {
        if values_equal_zero(&ta_element_or_undefined(&t, ctx, i), &target, ctx.heap()) {
            return Ok(Value::boolean(true));
        }
    }
    Ok(Value::boolean(false))
}

fn impl_join(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let t = receiver(ctx)?;
    check_not_detached(&t, ctx.heap())?;
    // §23.2.3.18 step 2 — the length is captured BEFORE
    // ToString(separator): a separator coercion that detaches the
    // buffer still yields len-1 separators with empty elements.
    let len = t.length(ctx.heap_mut());
    // §23.2.3.18 step 3 — ToString(separator) runs user
    // toString / valueOf for object separators.
    let separator = match args.first() {
        None => ",".to_string(),
        Some(v) if v.is_undefined() => ",".to_string(),
        Some(v) => {
            let exec_ctx =
                ctx.execution_context()
                    .cloned()
                    .ok_or_else(|| NativeError::TypeError {
                        name: "TypedArray.prototype.join",
                        reason: "missing execution context".to_string(),
                    })?;
            crate::coerce::to_string_or_throw(ctx.cx.interp, &exec_ctx, v).map_err(|e| {
                crate::native_function::vm_to_native_error(e, "TypedArray.prototype.join")
            })?
        }
    };
    join_into_string(&t, len, &separator, ctx.heap_mut())
}

fn join_into_string(
    t: &crate::binary::JsTypedArray,
    len: usize,
    separator: &str,
    gc_heap: &mut otter_gc::GcHeap,
) -> Result<Value, NativeError> {
    let mut out = String::new();
    let live_len = t.length(gc_heap);
    for i in 0..len {
        if i > 0 {
            out.push_str(separator);
        }
        if i >= live_len {
            // Detached mid-coercion — elements read as undefined
            // (empty), only the separators remain.
            continue;
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
    let len = t.length(ctx.heap_mut());
    join_into_string(&t, len, ",", ctx.heap_mut())
}

fn impl_to_locale_string(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    // §23.2.3.32 — join each element's `Invoke(element,
    // "toLocaleString")` (not a raw numeric render) with "," so a
    // user-overridden `Number.prototype.toLocaleString` runs and its
    // abrupt completion propagates.
    let t = receiver(ctx)?;
    check_not_detached(&t, ctx.heap())?;
    let exec = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| type_error("missing execution context"))?;
    let len = t.length(ctx.heap_mut());
    let mut parts: Vec<String> = Vec::new();
    for i in 0..len {
        let element = t.get(ctx.heap_mut(), i).map_err(native_oom)?;
        let method = ta_get(ctx, element, crate::VmPropertyKey::String("toLocaleString"))?;
        if !ctx.cx.interp.is_callable_runtime(&method) {
            return Err(type_error("element toLocaleString is not callable"));
        }
        let rendered = ctx
            .cx
            .interp
            .run_callable_sync(&exec, &method, element, smallvec::smallvec![])
            .map_err(|e| crate::native_function::vm_to_native_error(e, NAME))?;
        let s = crate::coerce::to_string_or_throw(ctx.cx.interp, &exec, &rendered)
            .map_err(|e| crate::native_function::vm_to_native_error(e, NAME))?;
        parts.push(s);
    }
    let joined = parts.join(",");
    Ok(Value::string(JsString::from_str(&joined, ctx.heap_mut())?))
}

fn impl_set(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let t = receiver(ctx)?;
    // §23.2.3.26 step 5 — `targetOffset = ToIntegerOrInfinity(offset)`
    // runs before the detached / range checks, so its `valueOf` fires
    // (and may detach the buffer), its abrupt completion propagates,
    // and a negative result is a RangeError rather than a TypeError.
    let offset_f = ta_set_offset(ctx, args.get(1))?;
    if offset_f < 0.0 {
        return Err(range_error("Start offset is out of bounds"));
    }
    check_not_detached(&t, ctx.heap())?;
    // A `+Infinity` or past-the-end offset overruns any source; reject
    // before the `usize` cast so the per-source bound checks stay
    // overflow-free.
    if offset_f > t.length(ctx.heap_mut()) as f64 {
        return Err(range_error("Start offset is out of bounds"));
    }
    let off = offset_f as usize;
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
        // §23.2.3.26.1 SetTypedArrayFromTypedArray — the offset
        // coercion above may have detached the SOURCE buffer too.
        if src.buffer(ctx.heap()).is_detached(ctx.heap()) {
            return Err(type_error(
                "Cannot set from a TypedArray backed by a detached buffer",
            ));
        }
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
    } else {
        // §23.2.3.26.2 SetTypedArrayFromArrayLike — every non-TypedArray
        // source is coerced through ToObject, then `length` and each
        // element are read with the ordinary `[[Get]]` (firing getters)
        // and converted via ToNumber / ToBigInt, so user side effects
        // run and abrupt completions propagate in spec order.
        if source.is_null() || source.is_undefined() {
            return Err(type_error("cannot set a TypedArray from null or undefined"));
        }
        let src_len = ta_array_like_length(ctx, source)?;
        if off + src_len > t.length(ctx.heap_mut()) {
            return Err(range_error("source overruns destination"));
        }
        for i in 0..src_len {
            let v = ta_get(
                ctx,
                source,
                crate::VmPropertyKey::OwnedString(i.to_string()),
            )?;
            let coerced = ta_coerce_value(ctx, kind, &v)?;
            // A getter may have detached the target mid-loop; §23.2.3.26.2
            // step 6 still converts every value but a store into a
            // detached view is a no-op.
            if !t.buffer(ctx.heap()).is_detached(ctx.heap()) {
                t.set(ctx.heap_mut(), off + i, &coerced);
            }
        }
    }
    Ok(Value::undefined())
}

/// §7.3.3 Get + run an accessor — read `source[key]` through the
/// ordinary `[[Get]]` so array / string indexing and user getters all
/// fire, propagating an abrupt completion verbatim.
fn ta_get(
    ctx: &mut NativeCtx<'_>,
    source: Value,
    key: crate::VmPropertyKey<'_>,
) -> Result<Value, NativeError> {
    let exec = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| type_error("missing execution context"))?;
    let outcome = ctx
        .interp_mut()
        .ordinary_get_value(&exec, source, source, &key, 0)
        .map_err(|e| crate::native_function::vm_to_native_error(e, NAME))?;
    match outcome {
        crate::VmGetOutcome::Value(v) => Ok(v),
        crate::VmGetOutcome::InvokeGetter { getter } => ctx
            .interp_mut()
            .run_callable_sync(&exec, &getter, source, smallvec::smallvec![])
            .map_err(|e| crate::native_function::vm_to_native_error(e, NAME)),
    }
}

/// §7.3.20 LengthOfArrayLike — `ToLength(Get(source, "length"))`.
fn ta_array_like_length(ctx: &mut NativeCtx<'_>, source: Value) -> Result<usize, NativeError> {
    let len_value = ta_get(ctx, source, crate::VmPropertyKey::String("length"))?;
    let exec = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| type_error("missing execution context"))?;
    let number = crate::coerce::to_number_or_throw(ctx.interp_mut(), &exec, &len_value)
        .map_err(|e| crate::native_function::vm_to_native_error(e, NAME))?;
    let n = number.as_f64();
    let len = if n.is_nan() || n <= 0.0 {
        0.0
    } else {
        n.trunc().min(9_007_199_254_740_991.0)
    };
    Ok(len as usize)
}

/// §23.2.3.26.2 step 6.c — convert a source element with `ToBigInt`
/// for BigInt element types and `ToNumber` otherwise (firing the
/// operand's coercion and throwing for a Symbol / cross-numeric type),
/// then narrow it to the destination element representation.
fn ta_coerce_value(
    ctx: &mut NativeCtx<'_>,
    kind: TypedArrayKind,
    value: &Value,
) -> Result<Value, NativeError> {
    let exec = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| type_error("missing execution context"))?;
    let converted = if kind.is_bigint() {
        let big = crate::coerce::to_big_int_or_throw(ctx.interp_mut(), &exec, value)
            .map_err(|e| crate::native_function::vm_to_native_error(e, NAME))?;
        Value::big_int(big)
    } else {
        let number = crate::coerce::to_number_or_throw(ctx.interp_mut(), &exec, value)
            .map_err(|e| crate::native_function::vm_to_native_error(e, NAME))?;
        number_value(number.as_f64())
    };
    crate::binary::dispatch::coerce_element_for_store(ctx.heap_mut(), kind, &converted)
        .map_err(|_| type_error("element type mismatch"))
}

fn impl_to_reversed(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let t = receiver(ctx)?;
    check_not_detached(&t, ctx.heap())?;
    let mut snapshot = copy_view(&t, ctx.heap_mut()).map_err(native_oom)?;
    snapshot.reverse();
    build_new_typed_array(ctx, t.kind(), &snapshot)
}

fn impl_to_sorted_default(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    // §23.2.3.32 step 1 — comparator must be undefined or callable,
    // checked before ValidateTypedArray.
    let comparefn = args.first().copied().filter(|v| !v.is_undefined());
    if let Some(cmp) = comparefn
        && !crate::abstract_ops::is_callable(&cmp)
    {
        return Err(NativeError::TypeError {
            name: "TypedArray.prototype.toSorted",
            reason: "comparefn must be a function or undefined".to_string(),
        });
    }
    let t = receiver(ctx)?;
    check_not_detached(&t, ctx.heap())?;
    let mut snapshot = copy_view(&t, ctx.heap_mut()).map_err(native_oom)?;
    match comparefn {
        None => sort_default(&mut snapshot, t.kind().is_bigint(), ctx.heap_mut()),
        Some(cmp) => sort_with_comparefn(ctx, &mut snapshot, &cmp)?,
    }
    build_new_typed_array(ctx, t.kind(), &snapshot)
}

fn impl_sort_default(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    // §23.2.3.29 step 1 — comparator must be undefined or callable,
    // checked before ValidateTypedArray.
    let comparefn = args.first().copied().filter(|v| !v.is_undefined());
    if let Some(cmp) = comparefn
        && !crate::abstract_ops::is_callable(&cmp)
    {
        return Err(NativeError::TypeError {
            name: "TypedArray.prototype.sort",
            reason: "comparefn must be a function or undefined".to_string(),
        });
    }
    let t = receiver(ctx)?;
    check_not_detached(&t, ctx.heap())?;
    let mut snapshot = copy_view(&t, ctx.heap_mut()).map_err(native_oom)?;
    match comparefn {
        None => sort_default(&mut snapshot, t.kind().is_bigint(), ctx.heap_mut()),
        Some(cmp) => sort_with_comparefn(ctx, &mut snapshot, &cmp)?,
    }
    for (i, v) in snapshot.iter().enumerate() {
        t.set(ctx.heap_mut(), i, v);
    }
    Ok(Value::typed_array(t))
}

/// §23.2.3.29 SortCompare with a user comparator — stable bottom-up
/// merge that tolerates inconsistent comparators (no Ord panic),
/// propagating abrupt completions and mapping NaN results to 0.
fn sort_with_comparefn(
    ctx: &mut NativeCtx<'_>,
    items: &mut Vec<Value>,
    cmp: &Value,
) -> Result<(), NativeError> {
    let exec_ctx = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| NativeError::TypeError {
            name: "TypedArray.prototype.sort",
            reason: "missing execution context".to_string(),
        })?;
    let n = items.len();
    let mut buf: Vec<Value> = items.clone();
    let mut width = 1usize;
    while width < n {
        let mut lo = 0usize;
        while lo < n {
            let mid = usize::min(lo + width, n);
            let hi = usize::min(lo + 2 * width, n);
            let (mut i, mut j, mut k) = (lo, mid, lo);
            while i < mid && j < hi {
                let mut argv: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::new();
                argv.push(items[i]);
                argv.push(items[j]);
                let raw = ctx
                    .cx
                    .interp
                    .run_callable_sync(&exec_ctx, cmp, Value::undefined(), argv)
                    .map_err(|e| {
                        crate::native_function::vm_to_native_error(e, "TypedArray.prototype.sort")
                    })?;
                let v = ctx
                    .cx
                    .interp
                    .coerce_to_number(&exec_ctx, &raw)
                    .map_err(|e| {
                        crate::native_function::vm_to_native_error(e, "TypedArray.prototype.sort")
                    })?
                    .as_f64();
                if v > 0.0 {
                    buf[k] = items[j];
                    j += 1;
                } else {
                    buf[k] = items[i];
                    i += 1;
                }
                k += 1;
            }
            while i < mid {
                buf[k] = items[i];
                i += 1;
                k += 1;
            }
            while j < hi {
                buf[k] = items[j];
                j += 1;
                k += 1;
            }
            lo = hi;
        }
        std::mem::swap(items, &mut buf);
        width *= 2;
    }
    Ok(())
}

fn impl_with(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let t = receiver(ctx)?;
    check_not_detached(&t, ctx.heap())?;
    let len = t.length(ctx.heap_mut()) as i64;
    let raw_idx = integer_arg(args.first(), 0);
    let resolved = if raw_idx < 0 { len + raw_idx } else { raw_idx };
    // §23.2.3.36 step 5 — the VALUE coerces (ToBigInt / ToNumber,
    // firing user valueOf) BEFORE the step-6 RangeError for an
    // out-of-range index.
    let raw_value = args.get(1).cloned().unwrap_or(Value::undefined());
    let exec = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| type_error("missing execution context"))?;
    let kind = t.kind();
    let value = ctx
        .interp_mut()
        .typed_array_coerce_element(&exec, kind, raw_value)
        .map_err(|e| crate::native_function::vm_to_native_error(e, "TypedArray.prototype.with"))?;
    // §23.2.3.36 step 9 — IsValidIntegerIndex(O, actualIndex) is
    // re-checked against the CURRENT state: the value's coercion can
    // detach or resize the backing buffer, so a once-valid index may
    // now be out of range. `actualIndex` itself stays relative to the
    // original length (step 5/6).
    check_not_detached(&t, ctx.heap())?;
    let cur_len = t.length(ctx.heap_mut()) as i64;
    if resolved < 0 || resolved >= cur_len {
        return Err(range_error("index out of range"));
    }
    // §23.2.3.36 steps 10-12 — the result is created with the ORIGINAL
    // length and filled by reading O[k] for each k < len (a now
    // out-of-bounds index reads as `undefined`, coerced on store),
    // substituting numericValue at actualIndex.
    let cur_snapshot = copy_view(&t, ctx.heap_mut()).map_err(native_oom)?;
    let mut out: Vec<Value> = Vec::with_capacity(len.max(0) as usize);
    for k in 0..len {
        if k == resolved {
            out.push(value);
        } else if (k as usize) < cur_snapshot.len() {
            out.push(cur_snapshot[k as usize]);
        } else {
            out.push(Value::undefined());
        }
    }
    build_new_typed_array(ctx, t.kind(), &out)
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

fn values_equal_strict(a: &Value, b: &Value, heap: &otter_gc::GcHeap) -> bool {
    if let (Some(x), Some(y)) = (a.as_number(), b.as_number()) {
        crate::number::equals(x, y)
    } else if let (Some(x), Some(y)) = (a.as_big_int(), b.as_big_int()) {
        // §7.2.12 — BigInt equality compares the NUMERIC values;
        // two heap cells holding the same integer must match.
        x.numeric_eq(y, heap)
    } else {
        false
    }
}

/// SameValueZero — like strict equality but `NaN === NaN`.
fn values_equal_zero(a: &Value, b: &Value, heap: &otter_gc::GcHeap) -> bool {
    // SameValueZero(undefined, undefined) is true. `includes` reads each
    // index with `[[Get]]`, which yields `undefined` on a detached view,
    // so `includes(undefined, …)` after a mid-call detach matches.
    if a.is_undefined() && b.is_undefined() {
        return true;
    }
    if let (Some(x), Some(y)) = (a.as_number(), b.as_number()) {
        if x.is_nan() && y.is_nan() {
            return true;
        }
        crate::number::equals(x, y)
    } else if let (Some(x), Some(y)) = (a.as_big_int(), b.as_big_int()) {
        x.numeric_eq(y, heap)
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
            // §23.2.4.7 CompareTypedArrayElements — NaN sorts to the
            // end, and -0 orders before +0.
            match (x.is_nan(), y.is_nan()) {
                (true, true) => std::cmp::Ordering::Equal,
                (true, false) => std::cmp::Ordering::Greater,
                (false, true) => std::cmp::Ordering::Less,
                _ if x == 0.0 && y == 0.0 => {
                    let xneg = x.is_sign_negative();
                    let yneg = y.is_sign_negative();
                    yneg.cmp(&xneg)
                }
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
    fn typed_array_entries_uses_old_iterator_state_allocation() {
        let mut interp = Interpreter::new();
        let receiver = {
            let heap = interp.gc_heap_mut();
            let buffer = JsArrayBuffer::new(heap, 2).expect("array buffer");
            let view =
                JsTypedArray::new(heap, buffer, TypedArrayKind::Int8, 0, 2).expect("typed array");
            Value::typed_array(view)
        };
        let before = interp.gc_heap().stats().old_allocated_bytes;

        let result = {
            let mut ctx =
                NativeCtx::new_with_call_info(&mut interp, NativeCallInfo::call(receiver));
            impl_entries(&mut ctx, &[]).expect("entries")
        };

        let after = interp.gc_heap().stats().old_allocated_bytes;
        assert!(
            after > before,
            "TypedArray.prototype.entries should allocate its iterator state in non-moving old space"
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
