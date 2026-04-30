//! `DataView.prototype.<name>` per ECMA-262 §25.3.4.
//!
//! Each `getX` / `setX` accepts an optional trailing `littleEndian`
//! flag. The default byte order is **big-endian** per §25.3.4.5
//! step 11; this matches V8 / SpiderMonkey behaviour.
//!
//! Detached-buffer guard runs on every method per §25.3.1.1
//! `DataView` step 5 and §25.3.4.5 step 5.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-properties-of-the-dataview-prototype-object>
//! - <https://tc39.es/ecma262/#sec-getviewvalue>
//! - <https://tc39.es/ecma262/#sec-setviewvalue>

use num_bigint::BigInt;

use crate::Value;
use crate::bigint::BigIntValue;
use crate::intrinsics::{IntrinsicArgs, IntrinsicError, IntrinsicReceiver, IntrinsicTable};
use crate::number::NumberValue;

use super::data_view::JsDataView;
use super::{number_value, smi, to_index, to_little_endian_flag};

fn receiver(args: &IntrinsicArgs<'_>) -> Result<JsDataView, IntrinsicError> {
    match args.receiver {
        Value::DataView(v) => Ok(v.clone()),
        _ => Err(IntrinsicError::BadReceiver {
            expected: "dataview",
        }),
    }
}

/// §25.3.1.1 step 5 — every read / write must guard the backing
/// buffer's detached state.
fn check_not_detached(view: &JsDataView) -> Result<(), IntrinsicError> {
    if view.buffer().is_detached() {
        return Err(IntrinsicError::BadReceiver {
            expected: "non-detached dataview",
        });
    }
    Ok(())
}

fn read_byte_offset(args: &IntrinsicArgs<'_>) -> Result<usize, IntrinsicError> {
    match to_index(args.args.first().unwrap_or(&Value::Undefined)) {
        Some(n) => Ok(n as usize),
        None => Err(IntrinsicError::BadArgument {
            index: 0,
            reason: "must be a non-negative integer",
        }),
    }
}

fn ensure_within(
    view: &JsDataView,
    offset: usize,
    byte_count: usize,
) -> Result<(), IntrinsicError> {
    if offset + byte_count > view.byte_length() {
        return Err(IntrinsicError::BadArgument {
            index: 0,
            reason: "out of bounds",
        });
    }
    Ok(())
}

fn read_bytes<F, R>(
    args: &IntrinsicArgs<'_>,
    byte_count: usize,
    le_arg: usize,
    f: F,
) -> Result<Value, IntrinsicError>
where
    F: FnOnce(&[u8], bool) -> R,
    R: Into<Value>,
{
    let view = receiver(args)?;
    check_not_detached(&view)?;
    let offset = read_byte_offset(args)?;
    ensure_within(&view, offset, byte_count)?;
    let little_endian = to_little_endian_flag(args.args.get(le_arg));
    let buf = view.buffer().borrow_bytes();
    let abs_offset = view.byte_offset() + offset;
    let slice = &buf[abs_offset..abs_offset + byte_count];
    Ok(f(slice, little_endian).into())
}

fn write_bytes<F>(
    args: &IntrinsicArgs<'_>,
    byte_count: usize,
    f: F,
) -> Result<Value, IntrinsicError>
where
    F: FnOnce(&mut [u8], &Value, bool),
{
    let view = receiver(args)?;
    check_not_detached(&view)?;
    let offset = read_byte_offset(args)?;
    ensure_within(&view, offset, byte_count)?;
    let value = args.args.get(1).cloned().unwrap_or(Value::Undefined);
    let little_endian = to_little_endian_flag(args.args.get(2));
    let mut buf = view.buffer().borrow_bytes_mut();
    let abs_offset = view.byte_offset() + offset;
    f(
        &mut buf[abs_offset..abs_offset + byte_count],
        &value,
        little_endian,
    );
    Ok(Value::Undefined)
}

// ---- getX implementations -----------------------------------------------

fn impl_get_int8(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    read_bytes(args, 1, 1, |s, _| smi(i8::from_le_bytes([s[0]]) as i32))
}

fn impl_get_uint8(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    read_bytes(args, 1, 1, |s, _| smi(s[0] as i32))
}

fn impl_get_int16(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    read_bytes(args, 2, 1, |s, le| {
        let v = if le {
            i16::from_le_bytes([s[0], s[1]])
        } else {
            i16::from_be_bytes([s[0], s[1]])
        };
        smi(v as i32)
    })
}

fn impl_get_uint16(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    read_bytes(args, 2, 1, |s, le| {
        let v = if le {
            u16::from_le_bytes([s[0], s[1]])
        } else {
            u16::from_be_bytes([s[0], s[1]])
        };
        smi(v as i32)
    })
}

fn impl_get_int32(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    read_bytes(args, 4, 1, |s, le| {
        let buf = [s[0], s[1], s[2], s[3]];
        let v = if le {
            i32::from_le_bytes(buf)
        } else {
            i32::from_be_bytes(buf)
        };
        smi(v)
    })
}

fn impl_get_uint32(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    read_bytes(args, 4, 1, |s, le| {
        let buf = [s[0], s[1], s[2], s[3]];
        let v = if le {
            u32::from_le_bytes(buf)
        } else {
            u32::from_be_bytes(buf)
        };
        number_value(v as f64)
    })
}

fn impl_get_float32(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    read_bytes(args, 4, 1, |s, le| {
        let buf = [s[0], s[1], s[2], s[3]];
        let v = if le {
            f32::from_le_bytes(buf)
        } else {
            f32::from_be_bytes(buf)
        };
        number_value(v as f64)
    })
}

fn impl_get_float64(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    read_bytes(args, 8, 1, |s, le| {
        let mut buf = [0u8; 8];
        buf.copy_from_slice(s);
        let v = if le {
            f64::from_le_bytes(buf)
        } else {
            f64::from_be_bytes(buf)
        };
        number_value(v)
    })
}

fn impl_get_bigint64(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    read_bytes(args, 8, 1, |s, le| {
        let mut buf = [0u8; 8];
        buf.copy_from_slice(s);
        let v = if le {
            i64::from_le_bytes(buf)
        } else {
            i64::from_be_bytes(buf)
        };
        Value::BigInt(BigIntValue::from_inner(BigInt::from(v)))
    })
}

fn impl_get_biguint64(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    read_bytes(args, 8, 1, |s, le| {
        let mut buf = [0u8; 8];
        buf.copy_from_slice(s);
        let v = if le {
            u64::from_le_bytes(buf)
        } else {
            u64::from_be_bytes(buf)
        };
        Value::BigInt(BigIntValue::from_inner(BigInt::from(v)))
    })
}

// ---- setX implementations -----------------------------------------------

fn coerce_number(value: &Value) -> NumberValue {
    match value {
        Value::Number(n) => *n,
        Value::Boolean(true) => NumberValue::from_i32(1),
        Value::Boolean(false) | Value::Null => NumberValue::from_i32(0),
        Value::Undefined => NumberValue::from_f64(f64::NAN),
        Value::String(s) => crate::number::to_number_from_string(&s.to_lossy_string()),
        _ => NumberValue::from_f64(f64::NAN),
    }
}

fn coerce_int(value: &Value) -> i64 {
    let n = coerce_number(value).as_f64();
    if !n.is_finite() {
        return 0;
    }
    n.trunc() as i64
}

fn coerce_bigint64(value: &Value) -> i64 {
    let big = match value {
        Value::BigInt(b) => b.as_inner().clone(),
        _ => BigInt::from(0),
    };
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

fn coerce_biguint64(value: &Value) -> u64 {
    let big = match value {
        Value::BigInt(b) => b.as_inner().clone(),
        _ => BigInt::from(0),
    };
    use num_traits::Signed;
    let modulus: BigInt = BigInt::from(1u64) << 64;
    let mut wrapped: BigInt = &big % &modulus;
    if wrapped.is_negative() {
        wrapped += &modulus;
    }
    use num_traits::ToPrimitive;
    wrapped.to_u64().unwrap_or(0)
}

fn impl_set_int8(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    write_bytes(args, 1, |b, v, _| {
        let n = coerce_int(v) as i8;
        b[0] = n as u8;
    })
}

fn impl_set_uint8(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    write_bytes(args, 1, |b, v, _| {
        let n = coerce_int(v) as u8;
        b[0] = n;
    })
}

fn impl_set_int16(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    write_bytes(args, 2, |b, v, le| {
        let n = coerce_int(v) as i16;
        let bytes = if le { n.to_le_bytes() } else { n.to_be_bytes() };
        b.copy_from_slice(&bytes);
    })
}

fn impl_set_uint16(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    write_bytes(args, 2, |b, v, le| {
        let n = coerce_int(v) as u16;
        let bytes = if le { n.to_le_bytes() } else { n.to_be_bytes() };
        b.copy_from_slice(&bytes);
    })
}

fn impl_set_int32(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    write_bytes(args, 4, |b, v, le| {
        let n = crate::number::bitwise::to_int32(coerce_number(v));
        let bytes = if le { n.to_le_bytes() } else { n.to_be_bytes() };
        b.copy_from_slice(&bytes);
    })
}

fn impl_set_uint32(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    write_bytes(args, 4, |b, v, le| {
        let n = crate::number::bitwise::to_uint32(coerce_number(v));
        let bytes = if le { n.to_le_bytes() } else { n.to_be_bytes() };
        b.copy_from_slice(&bytes);
    })
}

fn impl_set_float32(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    write_bytes(args, 4, |b, v, le| {
        let n = coerce_number(v).as_f64() as f32;
        let bytes = if le { n.to_le_bytes() } else { n.to_be_bytes() };
        b.copy_from_slice(&bytes);
    })
}

fn impl_set_float64(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    write_bytes(args, 8, |b, v, le| {
        let n = coerce_number(v).as_f64();
        let bytes = if le { n.to_le_bytes() } else { n.to_be_bytes() };
        b.copy_from_slice(&bytes);
    })
}

fn impl_set_bigint64(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    write_bytes(args, 8, |b, v, le| {
        let n = coerce_bigint64(v);
        let bytes = if le { n.to_le_bytes() } else { n.to_be_bytes() };
        b.copy_from_slice(&bytes);
    })
}

fn impl_set_biguint64(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    write_bytes(args, 8, |b, v, le| {
        let n = coerce_biguint64(v);
        let bytes = if le { n.to_le_bytes() } else { n.to_be_bytes() };
        b.copy_from_slice(&bytes);
    })
}

/// `DataView.prototype` table.
pub static DATA_VIEW_PROTOTYPE_TABLE: std::sync::LazyLock<IntrinsicTable> =
    std::sync::LazyLock::new(|| {
        crate::intrinsics!(
            DataView,
            "getInt8"      / 1 => impl_get_int8,
            "getUint8"     / 1 => impl_get_uint8,
            "getInt16"     / 1 => impl_get_int16,
            "getUint16"    / 1 => impl_get_uint16,
            "getInt32"     / 1 => impl_get_int32,
            "getUint32"    / 1 => impl_get_uint32,
            "getFloat32"   / 1 => impl_get_float32,
            "getFloat64"   / 1 => impl_get_float64,
            "getBigInt64"  / 1 => impl_get_bigint64,
            "getBigUint64" / 1 => impl_get_biguint64,
            "setInt8"      / 2 => impl_set_int8,
            "setUint8"     / 2 => impl_set_uint8,
            "setInt16"     / 2 => impl_set_int16,
            "setUint16"    / 2 => impl_set_uint16,
            "setInt32"     / 2 => impl_set_int32,
            "setUint32"    / 2 => impl_set_uint32,
            "setFloat32"   / 2 => impl_set_float32,
            "setFloat64"   / 2 => impl_set_float64,
            "setBigInt64"  / 2 => impl_set_bigint64,
            "setBigUint64" / 2 => impl_set_biguint64,
        )
    });

/// Convenience accessor used by the dispatcher.
#[must_use]
pub fn lookup(name: &str) -> Option<&'static crate::intrinsics::IntrinsicEntry> {
    DATA_VIEW_PROTOTYPE_TABLE.lookup(IntrinsicReceiver::DataView, name)
}

/// `DataView.prototype` getter access for `buffer`, `byteLength`,
/// `byteOffset`. Routed through `Op::LoadProperty` (see §25.3.4.1
/// — accessor properties exposed at runtime).
#[must_use]
pub fn load_property(view: &JsDataView, name: &str) -> Value {
    if view.buffer().is_detached() {
        return match name {
            "buffer" => Value::ArrayBuffer(view.buffer().clone()),
            _ => smi(0),
        };
    }
    match name {
        "buffer" => Value::ArrayBuffer(view.buffer().clone()),
        "byteLength" => smi(view.byte_length() as i32),
        "byteOffset" => smi(view.byte_offset() as i32),
        _ => Value::Undefined,
    }
}
