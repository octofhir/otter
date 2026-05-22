//! `ArrayBuffer.prototype.<name>` intrinsic table per ECMA-262
//! §25.1.5.
//!
//! Wired through the same [`crate::intrinsics`] surface the other
//! prototype tables use. Detached-buffer guards live here per
//! §25.1.3.1 `IsDetachedBuffer`.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-properties-of-the-arraybuffer-prototype-object>

use crate::Value;
use crate::intrinsics::{IntrinsicArgs, IntrinsicError, IntrinsicReceiver, IntrinsicTable};

use super::array_buffer::JsArrayBuffer;
use super::{number_value, smi};

fn receiver(args: &IntrinsicArgs<'_>) -> Result<JsArrayBuffer, IntrinsicError> {
    match args.receiver {
        Value::ArrayBuffer(b) => Ok(*b),
        _ => Err(IntrinsicError::BadReceiver {
            expected: "arraybuffer",
        }),
    }
}

/// §25.1.5.4 `slice(start, end)` — half-open range, clamps to
/// `[0, byteLength]`, returns a fresh fixed-length buffer.
fn impl_slice(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let buf = receiver(args)?;
    if buf.is_detached(args.gc_heap) {
        return Err(IntrinsicError::BadReceiver {
            expected: "non-detached arraybuffer",
        });
    }
    let len = buf.byte_length(args.gc_heap) as i64;
    let start = clamp_relative_index(args.args.first(), 0, len);
    let end = clamp_relative_index(args.args.get(1), len, len);
    let clamped_start = start.clamp(0, len) as usize;
    let clamped_end = end.clamp(clamped_start as i64, len) as usize;
    let copy: Vec<u8> = buf.with_bytes(args.gc_heap, |b| b[clamped_start..clamped_end].to_vec());
    Ok(Value::ArrayBuffer(args.array_buffer_from_bytes_rooted(
        copy,
        &[],
        &[],
    )?))
}

/// §25.1.5.6 `resize(newByteLength)` — only valid for resizable
/// buffers; otherwise raises `TypeError`. Throws `RangeError` when
/// `newByteLength > maxByteLength`.
fn impl_resize(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let buf = receiver(args)?;
    if !buf.is_resizable(args.gc_heap) || buf.is_detached(args.gc_heap) {
        return Err(IntrinsicError::BadReceiver {
            expected: "resizable non-detached arraybuffer",
        });
    }
    let new_len =
        match super::to_index(args.args.first().unwrap_or(&Value::Undefined), args.gc_heap) {
            Some(n) => n as usize,
            None => {
                return Err(IntrinsicError::BadArgument {
                    index: 0,
                    reason: "must be a non-negative integer",
                });
            }
        };
    if !buf.resize(args.gc_heap, new_len) {
        return Err(IntrinsicError::BadArgument {
            index: 0,
            reason: "exceeds maxByteLength",
        });
    }
    Ok(Value::Undefined)
}

/// §25.1.5.8 `transfer(newLength?)` — copy + detach. The new buffer
/// is resizable iff this one was; the new `maxByteLength` carries
/// over.
fn impl_transfer(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    transfer_inner(args, /* fixed = */ false)
}

/// §25.1.5.9 `transferToFixedLength(newLength?)` — same as
/// [`impl_transfer`] but the resulting buffer is fixed-length.
fn impl_transfer_to_fixed_length(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    transfer_inner(args, /* fixed = */ true)
}

fn transfer_inner(args: &mut IntrinsicArgs<'_>, fixed: bool) -> Result<Value, IntrinsicError> {
    let buf = receiver(args)?;
    if buf.is_detached(args.gc_heap) {
        return Err(IntrinsicError::BadReceiver {
            expected: "non-detached arraybuffer",
        });
    }
    let cur_len = buf.byte_length(args.gc_heap);
    let new_len = match args.args.first() {
        None | Some(Value::Undefined) => cur_len,
        Some(v) => match super::to_index(v, args.gc_heap) {
            Some(n) => n as usize,
            None => {
                return Err(IntrinsicError::BadArgument {
                    index: 0,
                    reason: "must be a non-negative integer",
                });
            }
        },
    };
    let mut new_bytes = vec![0u8; new_len];
    let copy_len = new_len.min(cur_len);
    buf.with_bytes(args.gc_heap, |src| {
        new_bytes[..copy_len].copy_from_slice(&src[..copy_len]);
    });
    let resizable = buf.is_resizable(args.gc_heap);
    let max = if resizable {
        buf.max_byte_length(args.gc_heap).max(new_len)
    } else {
        0
    };
    let new_buffer = if fixed {
        args.array_buffer_from_bytes_rooted(new_bytes, &[], &[])?
    } else if resizable {
        let result = args
            .array_buffer_resizable_rooted(new_len, max, &[], &[])?
            .ok_or(IntrinsicError::OutOfRange {
                index: 0,
                reason: "allocation failed",
            })?;
        result.with_bytes_mut(args.gc_heap, |dst| dst.copy_from_slice(&new_bytes));
        result
    } else {
        args.array_buffer_from_bytes_rooted(new_bytes, &[], &[])?
    };
    buf.detach(args.gc_heap);
    Ok(Value::ArrayBuffer(new_buffer))
}

fn clamp_relative_index(arg: Option<&Value>, default: i64, len: i64) -> i64 {
    let n = match arg {
        None | Some(Value::Undefined) => return default,
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::Boolean(true)) => 1.0,
        Some(Value::Boolean(false)) | Some(Value::Null) => 0.0,
        _ => return default,
    };
    if !n.is_finite() {
        if n.is_nan() {
            return 0;
        }
        return if n.is_sign_positive() { len } else { 0 };
    }
    let truncated = n.trunc() as i64;
    if truncated < 0 {
        (len + truncated).max(0)
    } else {
        truncated.min(len)
    }
}

/// §25.2.5.4 — `SharedArrayBuffer.prototype.grow(newByteLength)`.
fn impl_grow(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let buf = receiver(args)?;
    if !buf.is_shared() {
        return Err(IntrinsicError::BadReceiver {
            expected: "growable shared arraybuffer",
        });
    }
    let new_len =
        match super::to_index(args.args.first().unwrap_or(&Value::Undefined), args.gc_heap) {
            Some(n) => n as usize,
            None => {
                return Err(IntrinsicError::BadArgument {
                    index: 0,
                    reason: "must be a non-negative integer",
                });
            }
        };
    if !buf.grow(args.gc_heap, new_len) {
        return Err(IntrinsicError::BadArgument {
            index: 0,
            reason: "cannot grow",
        });
    }
    Ok(Value::Undefined)
}

/// `ArrayBuffer.prototype` / `SharedArrayBuffer.prototype` table.
pub static ARRAY_BUFFER_PROTOTYPE_TABLE: std::sync::LazyLock<IntrinsicTable> =
    std::sync::LazyLock::new(|| {
        crate::intrinsics!(
            ArrayBuffer,
            "slice"                  / 2 => impl_slice,
            "resize"                 / 1 => impl_resize,
            "transfer"               / 1 => impl_transfer,
            "transferToFixedLength"  / 1 => impl_transfer_to_fixed_length,
            "grow"                   / 1 => impl_grow,
        )
    });

/// Convenience accessor used by the dispatcher.
#[must_use]
pub fn lookup(name: &str) -> Option<&'static crate::intrinsics::IntrinsicEntry> {
    ARRAY_BUFFER_PROTOTYPE_TABLE.lookup(IntrinsicReceiver::ArrayBuffer, name)
}

/// §25.1.5 — getter access for `byteLength` / `maxByteLength` /
/// `resizable` / `detached`. Routed through `Op::LoadProperty` since
/// these are accessor properties in spec but the foundation surface
/// just synthesises a value at read time.
#[must_use]
pub fn load_property(buf: JsArrayBuffer, heap: &otter_gc::GcHeap, name: &str) -> Value {
    match name {
        "byteLength" => smi(buf.byte_length(heap) as i32),
        "maxByteLength" => smi(buf.max_byte_length(heap) as i32),
        "resizable" => Value::Boolean(buf.is_resizable(heap)),
        // §25.2.4.2 — `growable` for SAB; mirrors `resizable` on
        // ordinary ArrayBuffer.
        "growable" => Value::Boolean(buf.is_growable(heap)),
        "detached" => Value::Boolean(buf.is_detached(heap)),
        // Diagnostics-only: spec exposes byteLength as 0 when
        // detached but the foundation already does that inside
        // `byte_length`.
        _ => {
            let _ = number_value;
            Value::Undefined
        }
    }
}
