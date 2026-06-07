//! `DataView.prototype.<name>` per ECMA-262 §25.3.4.
//!
//! Each `getX` / `setX` accepts an optional trailing `littleEndian`
//! flag. The default byte order is **big-endian** per §25.3.4.5 step
//! 11. Detached-buffer guard runs on every method per §25.3.1.1 step 5.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-properties-of-the-dataview-prototype-object>
//! - <https://tc39.es/ecma262/#sec-getviewvalue>
//! - <https://tc39.es/ecma262/#sec-setviewvalue>

use num_bigint::BigInt;

use crate::Value;
use crate::bigint::BigIntValue;
use crate::number::NumberValue;
use crate::{NativeCtx, NativeError};

use super::data_view::JsDataView;
use super::{number_value, smi, to_little_endian_flag};

const NAME: &str = "DataView.prototype";

fn bad(reason: &str) -> NativeError {
    NativeError::TypeError {
        name: NAME,
        reason: reason.to_string(),
    }
}

fn range_bad(reason: &str) -> NativeError {
    NativeError::RangeError {
        name: NAME,
        reason: reason.to_string(),
    }
}

fn receiver(ctx: &NativeCtx<'_>) -> Result<JsDataView, NativeError> {
    ctx.this_value()
        .as_data_view()
        .ok_or_else(|| bad("receiver is not a DataView"))
}

/// §25.3.1.1 step 5 — read / write guard the buffer's detached state.
fn check_not_detached(view: &JsDataView, heap: &otter_gc::GcHeap) -> Result<(), NativeError> {
    if view.buffer(heap).is_detached(heap) {
        return Err(bad("buffer is detached"));
    }
    Ok(())
}

fn ensure_within(
    view: &JsDataView,
    heap: &otter_gc::GcHeap,
    offset: usize,
    byte_count: usize,
) -> Result<(), NativeError> {
    // §25.3.1.1 step 13 / §25.3.1.2 step 15 — an access past the view's
    // byte length is a RangeError, not a TypeError.
    if offset + byte_count > view.byte_length(heap) {
        return Err(range_bad("Offset is outside the bounds of the DataView"));
    }
    Ok(())
}

/// §25.3.1.1 step 2 / §25.3.1.2 step 2 — `getIndex = ToIndex(requestIndex)`.
/// `ToIndex` runs `ToNumber` (firing `@@toPrimitive` / `valueOf` /
/// `toString`, and throwing for a Symbol / BigInt request), truncates
/// toward zero, then rejects a negative or `> 2**53 - 1` result with a
/// `RangeError`. Returning the abrupt completion verbatim preserves a
/// user `throw` (e.g. a `Test262Error` from a poisoned `valueOf`).
fn to_index_or_throw(ctx: &mut NativeCtx<'_>, value: &Value) -> Result<usize, NativeError> {
    let exec = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| bad("missing execution context"))?;
    let number = crate::coerce::to_number_or_throw(ctx.interp_mut(), &exec, value)
        .map_err(|e| crate::native_function::vm_to_native_error(e, NAME))?;
    let n = number.as_f64();
    let integer = if n.is_nan() { 0.0 } else { n.trunc() };
    if !(0.0..=9_007_199_254_740_991.0).contains(&integer) {
        return Err(range_bad("Invalid DataView access index"));
    }
    Ok(integer as usize)
}

/// §25.3.1.2 step 3 — `SetViewValue` converts the value with `ToBigInt`
/// for the BigInt element types and `ToNumber` otherwise, before the
/// detached-buffer and range checks. The conversion fires its operand's
/// observable coercion and throws (Symbol / cross-numeric-type), with
/// the abrupt completion propagated verbatim.
fn convert_set_value(
    ctx: &mut NativeCtx<'_>,
    is_bigint: bool,
    value: &Value,
) -> Result<Value, NativeError> {
    let exec = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| bad("missing execution context"))?;
    if is_bigint {
        let big = crate::coerce::to_big_int_or_throw(ctx.interp_mut(), &exec, value)
            .map_err(|e| crate::native_function::vm_to_native_error(e, NAME))?;
        Ok(Value::big_int(big))
    } else {
        let number = crate::coerce::to_number_or_throw(ctx.interp_mut(), &exec, value)
            .map_err(|e| crate::native_function::vm_to_native_error(e, NAME))?;
        Ok(number_value(number.as_f64()))
    }
}

fn read_view<F>(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    byte_count: usize,
    le_arg: usize,
    f: F,
) -> Result<Value, NativeError>
where
    F: FnOnce(&[u8], bool, &mut otter_gc::GcHeap) -> Result<Value, NativeError>,
{
    // §25.3.1.1 GetViewValue order: RequireInternalSlot, ToIndex(index),
    // ToBoolean(littleEndian), detached-buffer guard, range guard.
    let view = receiver(ctx)?;
    let request = args.first().cloned().unwrap_or_default();
    let offset = to_index_or_throw(ctx, &request)?;
    let little_endian = to_little_endian_flag(args.get(le_arg), ctx.heap());
    check_not_detached(&view, ctx.heap())?;
    ensure_within(&view, ctx.heap(), offset, byte_count)?;
    let abs_offset = view.byte_offset(ctx.heap()) + offset;
    let buffer = view.buffer(ctx.heap());
    let snapshot: Vec<u8> = buffer.with_bytes(ctx.heap(), |b| {
        b[abs_offset..abs_offset + byte_count].to_vec()
    });
    f(&snapshot, little_endian, ctx.heap_mut())
}

fn write_view<F>(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    byte_count: usize,
    is_bigint: bool,
    f: F,
) -> Result<Value, NativeError>
where
    F: FnOnce(&mut [u8], &Value, bool, &otter_gc::GcHeap),
{
    // §25.3.1.2 SetViewValue order: RequireInternalSlot, ToIndex(index),
    // numeric conversion of the value, ToBoolean(littleEndian),
    // detached-buffer guard, range guard, then the store.
    let view = receiver(ctx)?;
    let request = args.first().cloned().unwrap_or_default();
    let offset = to_index_or_throw(ctx, &request)?;
    let raw_value = args.get(1).cloned().unwrap_or_default();
    let value = convert_set_value(ctx, is_bigint, &raw_value)?;
    let little_endian = to_little_endian_flag(args.get(2), ctx.heap());
    check_not_detached(&view, ctx.heap())?;
    ensure_within(&view, ctx.heap(), offset, byte_count)?;
    let mut staging = vec![0u8; byte_count];
    f(&mut staging, &value, little_endian, ctx.heap());
    let abs_offset = view.byte_offset(ctx.heap()) + offset;
    let buffer = view.buffer(ctx.heap());
    buffer.with_bytes_mut(ctx.heap_mut(), |buf| {
        buf[abs_offset..abs_offset + byte_count].copy_from_slice(&staging);
    });
    Ok(Value::undefined())
}

// ---- getX --------------------------------------------------------------

pub(crate) fn dv_get_int8(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    read_view(ctx, args, 1, 1, |s, _, _| {
        Ok(smi(i8::from_le_bytes([s[0]]) as i32))
    })
}

pub(crate) fn dv_get_uint8(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    read_view(ctx, args, 1, 1, |s, _, _| Ok(smi(s[0] as i32)))
}

pub(crate) fn dv_get_int16(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    read_view(ctx, args, 2, 1, |s, le, _| {
        let v = if le {
            i16::from_le_bytes([s[0], s[1]])
        } else {
            i16::from_be_bytes([s[0], s[1]])
        };
        Ok(smi(v as i32))
    })
}

pub(crate) fn dv_get_uint16(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    read_view(ctx, args, 2, 1, |s, le, _| {
        let v = if le {
            u16::from_le_bytes([s[0], s[1]])
        } else {
            u16::from_be_bytes([s[0], s[1]])
        };
        Ok(smi(v as i32))
    })
}

pub(crate) fn dv_get_int32(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    read_view(ctx, args, 4, 1, |s, le, _| {
        let buf = [s[0], s[1], s[2], s[3]];
        let v = if le {
            i32::from_le_bytes(buf)
        } else {
            i32::from_be_bytes(buf)
        };
        Ok(smi(v))
    })
}

pub(crate) fn dv_get_uint32(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    read_view(ctx, args, 4, 1, |s, le, _| {
        let buf = [s[0], s[1], s[2], s[3]];
        let v = if le {
            u32::from_le_bytes(buf)
        } else {
            u32::from_be_bytes(buf)
        };
        Ok(number_value(v as f64))
    })
}

pub(crate) fn dv_get_float32(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    read_view(ctx, args, 4, 1, |s, le, _| {
        let buf = [s[0], s[1], s[2], s[3]];
        let v = if le {
            f32::from_le_bytes(buf)
        } else {
            f32::from_be_bytes(buf)
        };
        Ok(number_value(v as f64))
    })
}

pub(crate) fn dv_get_float64(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    read_view(ctx, args, 8, 1, |s, le, _| {
        let mut buf = [0u8; 8];
        buf.copy_from_slice(s);
        let v = if le {
            f64::from_le_bytes(buf)
        } else {
            f64::from_be_bytes(buf)
        };
        Ok(number_value(v))
    })
}

pub(crate) fn dv_get_float16(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    read_view(ctx, args, 2, 1, |s, le, _| {
        let buf = [s[0], s[1]];
        let bits = if le {
            u16::from_le_bytes(buf)
        } else {
            u16::from_be_bytes(buf)
        };
        Ok(number_value(crate::binary::typed_array::f16_bits_to_f64(
            bits,
        )))
    })
}

pub(crate) fn dv_get_bigint64(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    read_view(ctx, args, 8, 1, |s, le, heap| {
        let mut buf = [0u8; 8];
        buf.copy_from_slice(s);
        let v = if le {
            i64::from_le_bytes(buf)
        } else {
            i64::from_be_bytes(buf)
        };
        let handle = BigIntValue::from_inner(heap, BigInt::from(v))?;
        Ok(Value::big_int(handle))
    })
}

pub(crate) fn dv_get_biguint64(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    read_view(ctx, args, 8, 1, |s, le, heap| {
        let mut buf = [0u8; 8];
        buf.copy_from_slice(s);
        let v = if le {
            u64::from_le_bytes(buf)
        } else {
            u64::from_be_bytes(buf)
        };
        let handle = BigIntValue::from_inner(heap, BigInt::from(v))?;
        Ok(Value::big_int(handle))
    })
}

// ---- setX --------------------------------------------------------------

fn coerce_number(value: &Value, heap: &otter_gc::GcHeap) -> NumberValue {
    if let Some(n) = value.as_number() {
        return n;
    }
    if let Some(b) = value.as_boolean() {
        return NumberValue::from_i32(if b { 1 } else { 0 });
    }
    if value.is_null() {
        return NumberValue::from_i32(0);
    }
    if value.is_undefined() {
        return NumberValue::from_f64(f64::NAN);
    }
    if let Some(s) = value.as_string(heap) {
        return crate::number::to_number_from_string(&s.to_lossy_string(heap));
    }
    NumberValue::from_f64(f64::NAN)
}

fn coerce_int(value: &Value, heap: &otter_gc::GcHeap) -> i64 {
    let n = coerce_number(value, heap).as_f64();
    if !n.is_finite() {
        return 0;
    }
    n.trunc() as i64
}

fn coerce_bigint64(value: &Value, heap: &otter_gc::GcHeap) -> i64 {
    let big = value
        .as_big_int()
        .map_or_else(|| BigInt::from(0), |b| b.clone_inner(heap));
    use num_traits::Signed;
    let modulus: BigInt = BigInt::from(1u64) << 64;
    let mut wrapped: BigInt = &big % &modulus;
    if wrapped.is_negative() {
        wrapped += &modulus;
    }
    let half: BigInt = BigInt::from(1u64) << 63;
    if wrapped >= half {
        wrapped -= modulus;
    }
    use num_traits::ToPrimitive;
    wrapped.to_i64().unwrap_or(0)
}

fn coerce_biguint64(value: &Value, heap: &otter_gc::GcHeap) -> u64 {
    let big = value
        .as_big_int()
        .map_or_else(|| BigInt::from(0), |b| b.clone_inner(heap));
    use num_traits::Signed;
    let modulus: BigInt = BigInt::from(1u64) << 64;
    let mut wrapped: BigInt = &big % &modulus;
    if wrapped.is_negative() {
        wrapped += &modulus;
    }
    use num_traits::ToPrimitive;
    wrapped.to_u64().unwrap_or(0)
}

pub(crate) fn dv_set_int8(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    write_view(ctx, args, 1, false, |b, v, _, heap| {
        b[0] = coerce_int(v, heap) as i8 as u8;
    })
}

pub(crate) fn dv_set_uint8(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    write_view(ctx, args, 1, false, |b, v, _, heap| {
        b[0] = coerce_int(v, heap) as u8;
    })
}

pub(crate) fn dv_set_int16(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    write_view(ctx, args, 2, false, |b, v, le, heap| {
        let n = coerce_int(v, heap) as i16;
        let bytes = if le { n.to_le_bytes() } else { n.to_be_bytes() };
        b.copy_from_slice(&bytes);
    })
}

pub(crate) fn dv_set_uint16(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    write_view(ctx, args, 2, false, |b, v, le, heap| {
        let n = coerce_int(v, heap) as u16;
        let bytes = if le { n.to_le_bytes() } else { n.to_be_bytes() };
        b.copy_from_slice(&bytes);
    })
}

pub(crate) fn dv_set_int32(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    write_view(ctx, args, 4, false, |b, v, le, heap| {
        let n = crate::number::bitwise::to_int32(coerce_number(v, heap));
        let bytes = if le { n.to_le_bytes() } else { n.to_be_bytes() };
        b.copy_from_slice(&bytes);
    })
}

pub(crate) fn dv_set_uint32(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    write_view(ctx, args, 4, false, |b, v, le, heap| {
        let n = crate::number::bitwise::to_uint32(coerce_number(v, heap));
        let bytes = if le { n.to_le_bytes() } else { n.to_be_bytes() };
        b.copy_from_slice(&bytes);
    })
}

pub(crate) fn dv_set_float32(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    write_view(ctx, args, 4, false, |b, v, le, heap| {
        let n = coerce_number(v, heap).as_f64() as f32;
        let bytes = if le { n.to_le_bytes() } else { n.to_be_bytes() };
        b.copy_from_slice(&bytes);
    })
}

pub(crate) fn dv_set_float64(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    write_view(ctx, args, 8, false, |b, v, le, heap| {
        let n = coerce_number(v, heap).as_f64();
        let bytes = if le { n.to_le_bytes() } else { n.to_be_bytes() };
        b.copy_from_slice(&bytes);
    })
}

pub(crate) fn dv_set_float16(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    write_view(ctx, args, 2, false, |b, v, le, heap| {
        let n = coerce_number(v, heap).as_f64();
        let bits = crate::binary::typed_array::f64_to_f16_bits(n);
        let bytes = if le {
            bits.to_le_bytes()
        } else {
            bits.to_be_bytes()
        };
        b.copy_from_slice(&bytes);
    })
}

pub(crate) fn dv_set_bigint64(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    write_view(ctx, args, 8, true, |b, v, le, heap| {
        let n = coerce_bigint64(v, heap);
        let bytes = if le { n.to_le_bytes() } else { n.to_be_bytes() };
        b.copy_from_slice(&bytes);
    })
}

pub(crate) fn dv_set_biguint64(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    write_view(ctx, args, 8, true, |b, v, le, heap| {
        let n = coerce_biguint64(v, heap);
        let bytes = if le { n.to_le_bytes() } else { n.to_be_bytes() };
        b.copy_from_slice(&bytes);
    })
}

/// `DataView.prototype` getter access for `buffer`, `byteLength`,
/// `byteOffset`, routed through `Op::LoadProperty`.
#[must_use]
pub fn load_property(view: &JsDataView, heap: &otter_gc::GcHeap, name: &str) -> Value {
    let buffer = view.buffer(heap);
    if buffer.is_detached(heap) {
        return match name {
            "buffer" => Value::array_buffer(buffer),
            _ => smi(0),
        };
    }
    match name {
        "buffer" => Value::array_buffer(buffer),
        "byteLength" => smi(view.byte_length(heap) as i32),
        "byteOffset" => smi(view.byte_offset(heap) as i32),
        _ => Value::undefined(),
    }
}
