//! Constructor and static-method dispatchers for the three binary
//! opcodes — `Op::ArrayBufferCall`, `Op::DataViewCall`,
//! `Op::TypedArrayCall`.
//!
//! Each dispatcher uses the empty-name sentinel to mean "constructor"
//! (matches the existing `Op::DateCall` / `Op::BigIntCall` shape).
//! The TypedArray entry takes an additional `kind` parameter encoding
//! which of the eleven concrete classes was called.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-arraybuffer-constructor>
//! - <https://tc39.es/ecma262/#sec-dataview-constructor>
//! - <https://tc39.es/ecma262/#sec-typedarray-constructors>

use crate::{Value, VmError};

use super::array_buffer::JsArrayBuffer;
use super::data_view::JsDataView;
use super::to_index;
use super::typed_array::{JsTypedArray, TypedArrayKind};

/// Map a `to_index` failure to a spec-correct completion. §7.1.22
/// `ToIndex` throws **RangeError** on negative integers and on values
/// above 2^53-1, but the underlying `ToNumber` step (§7.1.4) throws
/// **TypeError** for `Symbol` and `BigInt` operands. The shared
/// `to_index` helper collapses both outcomes to `None`; this wrapper
/// recovers the spec error class from the original value.
fn to_index_error(value: &Value, what: &str) -> VmError {
    if value.is_symbol() || value.is_big_int() {
        VmError::TypeError {
            message: format!("Cannot convert {what} to a number"),
        }
    } else {
        VmError::RangeError {
            message: format!("Invalid {what}"),
        }
    }
}

// =========================================================================
// ArrayBuffer
// =========================================================================

/// Dispatch `ArrayBuffer(...)` ([`ArrayBufferMethod::Construct`]) /
/// `ArrayBuffer.<method>(...)` via the typed [`ArrayBufferMethod`].
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-arraybuffer-constructor>
pub fn array_buffer_call(
    method: otter_bytecode::method_id::ArrayBufferMethod,
    args: &[Value],
    _gc_heap: &otter_gc::GcHeap,
) -> Result<Value, VmError> {
    use otter_bytecode::method_id::ArrayBufferMethod as M;
    match method {
        M::Construct => Err(VmError::TypeError {
            message: "ArrayBuffer construction requires rooted dispatch".to_string(),
        }),
        // §25.1.3.1 ArrayBuffer.isView(arg) — returns `true` when
        // arg is a TypedArray or DataView.
        M::IsView => {
            let v = args.first().cloned().unwrap_or(Value::undefined());
            Ok(Value::boolean(v.is_typed_array() || v.is_data_view()))
        }
    }
}

/// Root-aware ArrayBuffer dispatcher for active VM/native constructor paths
/// that can expose live roots while reserving off-heap backing storage.
pub fn array_buffer_call_with_roots(
    method: otter_bytecode::method_id::ArrayBufferMethod,
    args: &[Value],
    gc_heap: &mut otter_gc::GcHeap,
    external_visit: &mut otter_gc::heap::RootSlotVisitor<'_>,
) -> Result<Value, VmError> {
    use otter_bytecode::method_id::ArrayBufferMethod as M;
    match method {
        M::Construct => {
            let length = match args.first() {
                None => 0u64,
                Some(v) if v.is_undefined() => 0u64,
                Some(v) => {
                    to_index(v, gc_heap).ok_or_else(|| to_index_error(v, "ArrayBuffer length"))?
                }
            };
            let max_byte_length = if let Some(opts) = args.get(1).and_then(|v| v.as_object()) {
                if let Some(v) = crate::object::get(opts, gc_heap, "maxByteLength") {
                    Some(
                        to_index(&v, gc_heap)
                            .ok_or_else(|| to_index_error(&v, "ArrayBuffer maxByteLength"))?,
                    )
                } else {
                    None
                }
            } else {
                None
            };
            let len = length as usize;
            let buf = match max_byte_length {
                Some(max) => {
                    let max = max as usize;
                    if max < len {
                        return Err(VmError::TypeMismatch);
                    }
                    JsArrayBuffer::new_resizable_with_roots(len, max, gc_heap, external_visit)
                        .map_err(oom_to_vm)?
                        .ok_or_else(|| VmError::RangeError {
                            message: format!(
                                "ArrayBuffer allocation of {max} bytes exceeds the available heap"
                            ),
                        })?
                }
                None => JsArrayBuffer::try_new_with_roots(len, gc_heap, external_visit)
                    .map_err(oom_to_vm)?
                    .ok_or_else(|| VmError::RangeError {
                        message: format!(
                            "ArrayBuffer allocation of {len} bytes exceeds the available heap"
                        ),
                    })?,
            };
            Ok(Value::array_buffer(buf))
        }
        M::IsView => {
            let v = args.first().cloned().unwrap_or(Value::undefined());
            Ok(Value::boolean(v.is_typed_array() || v.is_data_view()))
        }
    }
}

/// Root-aware dispatch for `new SharedArrayBuffer(length [, options])` per
/// §25.2.1. Only the `maxByteLength` option (growable SAB) is honoured;
/// arbitrary key options are ignored.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-sharedarraybuffer-constructor>
pub fn shared_array_buffer_call_with_roots(
    method: otter_bytecode::method_id::SharedArrayBufferMethod,
    args: &[Value],
    gc_heap: &mut otter_gc::GcHeap,
    external_visit: &mut otter_gc::heap::RootSlotVisitor<'_>,
) -> Result<Value, VmError> {
    use otter_bytecode::method_id::SharedArrayBufferMethod as M;
    match method {
        M::Construct => {
            external_visit(&mut |_| {});
            let length = match args.first() {
                None => 0u64,
                Some(v) if v.is_undefined() => 0u64,
                Some(v) => to_index(v, gc_heap)
                    .ok_or_else(|| to_index_error(v, "SharedArrayBuffer length"))?,
            };
            let max_byte_length = if let Some(opts) = args.get(1).and_then(|v| v.as_object()) {
                if let Some(v) = crate::object::get(opts, gc_heap, "maxByteLength") {
                    Some(
                        to_index(&v, gc_heap)
                            .ok_or_else(|| to_index_error(&v, "SharedArrayBuffer maxByteLength"))?,
                    )
                } else {
                    None
                }
            } else {
                None
            };
            let len = length as usize;
            let buf = match max_byte_length {
                Some(max) => {
                    let max = max as usize;
                    if max < len {
                        return Err(VmError::TypeMismatch);
                    }
                    JsArrayBuffer::new_shared_growable_with_roots(
                        len,
                        max,
                        gc_heap,
                        external_visit,
                    )
                    .map_err(oom_to_vm)?
                    .ok_or_else(|| VmError::RangeError {
                        message: format!(
                            "SharedArrayBuffer allocation of {max} bytes exceeds the available heap"
                        ),
                    })?
                }
                None => JsArrayBuffer::try_new_shared_with_roots(len, gc_heap, external_visit)
                    .map_err(oom_to_vm)?
                    .ok_or_else(|| VmError::RangeError {
                        message: format!(
                            "SharedArrayBuffer allocation of {len} bytes exceeds the available heap"
                        ),
                    })?,
            };
            Ok(Value::array_buffer(buf))
        }
    }
}

// =========================================================================
// DataView
// =========================================================================

/// Dispatch `new DataView(buffer, byteOffset?, byteLength?)` via
/// the typed [`DataViewMethod`].
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-dataview-buffer-byteoffset-bytelength>
pub fn data_view_call(
    method: otter_bytecode::method_id::DataViewMethod,
    args: &[Value],
    gc_heap: &mut otter_gc::GcHeap,
) -> Result<Value, VmError> {
    use otter_bytecode::method_id::DataViewMethod as M;
    match method {
        M::Construct => {
            let buffer = args
                .first()
                .and_then(|v| v.as_array_buffer())
                .ok_or(VmError::TypeMismatch)?;
            if buffer.is_detached(gc_heap) {
                return Err(VmError::TypeMismatch);
            }
            let buffer_byte_length = buffer.byte_length(gc_heap);
            let byte_offset = match args.get(1) {
                None => 0u64,
                Some(v) if v.is_undefined() => 0u64,
                Some(v) => to_index(v, gc_heap).ok_or(VmError::TypeMismatch)?,
            } as usize;
            if byte_offset > buffer_byte_length {
                return Err(VmError::TypeMismatch);
            }
            let byte_length = match args.get(2) {
                None => buffer_byte_length - byte_offset,
                Some(v) if v.is_undefined() => buffer_byte_length - byte_offset,
                Some(v) => {
                    let n = to_index(v, gc_heap).ok_or(VmError::TypeMismatch)? as usize;
                    if byte_offset + n > buffer_byte_length {
                        return Err(VmError::TypeMismatch);
                    }
                    n
                }
            };
            let view =
                JsDataView::new(gc_heap, buffer, byte_offset, byte_length).map_err(oom_to_vm)?;
            Ok(Value::data_view(view))
        }
    }
}

// =========================================================================
// TypedArray
// =========================================================================

/// Root-aware variant for active VM/native TypedArray constructor/static paths
/// that allocate fresh backing stores.
pub fn typed_array_call_with_roots(
    kind: TypedArrayKind,
    method: otter_bytecode::method_id::TypedArrayMethod,
    args: &[Value],
    gc_heap: &mut otter_gc::GcHeap,
    external_visit: &mut otter_gc::heap::RootSlotVisitor<'_>,
) -> Result<Value, VmError> {
    use otter_bytecode::method_id::TypedArrayMethod as M;
    match method {
        M::Construct => construct_typed_array_with_roots(kind, args, gc_heap, external_visit),
        M::From => from_static_with_roots(kind, args, gc_heap, external_visit),
        M::Of => of_static_with_roots(kind, args, gc_heap, external_visit),
    }
}

fn oom_to_vm(err: otter_gc::OutOfMemory) -> VmError {
    VmError::OutOfMemory {
        requested_bytes: err.requested_bytes(),
        heap_limit_bytes: err.heap_limit_bytes(),
    }
}

fn typed_array_byte_len(len: usize, bpe: usize) -> Result<usize, VmError> {
    len.checked_mul(bpe).ok_or_else(|| VmError::RangeError {
        message: "TypedArray byte length overflow".to_string(),
    })
}

fn typed_array_from_values_with_roots(
    kind: TypedArrayKind,
    values: &[Value],
    gc_heap: &mut otter_gc::GcHeap,
    external_visit: &mut otter_gc::heap::RootSlotVisitor<'_>,
) -> Result<Value, VmError> {
    let bpe = kind.bytes_per_element();
    let byte_len = typed_array_byte_len(values.len(), bpe)?;
    let new_buf = JsArrayBuffer::try_new_with_roots(byte_len, gc_heap, external_visit)
        .map_err(oom_to_vm)?
        .ok_or_else(|| VmError::RangeError {
            message: format!(
                "TypedArray allocation of {byte_len} bytes exceeds the available heap"
            ),
        })?;
    let view = JsTypedArray::new(gc_heap, new_buf, kind, 0, values.len()).map_err(oom_to_vm)?;
    for (i, value) in values.iter().enumerate() {
        view.set(gc_heap, i, value);
    }
    Ok(Value::typed_array(view))
}

fn new_zeroed_typed_array_with_roots(
    kind: TypedArrayKind,
    len: usize,
    gc_heap: &mut otter_gc::GcHeap,
    external_visit: &mut otter_gc::heap::RootSlotVisitor<'_>,
) -> Result<Value, VmError> {
    let byte_len = typed_array_byte_len(len, kind.bytes_per_element())?;
    let new_buf = JsArrayBuffer::try_new_with_roots(byte_len, gc_heap, external_visit)
        .map_err(oom_to_vm)?
        .ok_or_else(|| VmError::RangeError {
            message: format!(
                "TypedArray allocation of {byte_len} bytes exceeds the available heap"
            ),
        })?;
    let view = JsTypedArray::new(gc_heap, new_buf, kind, 0, len).map_err(oom_to_vm)?;
    Ok(Value::typed_array(view))
}

fn construct_typed_array_with_roots(
    kind: TypedArrayKind,
    args: &[Value],
    gc_heap: &mut otter_gc::GcHeap,
    external_visit: &mut otter_gc::heap::RootSlotVisitor<'_>,
) -> Result<Value, VmError> {
    let bpe = kind.bytes_per_element();
    let Some(first) = args.first() else {
        return new_zeroed_typed_array_with_roots(kind, 0, gc_heap, external_visit);
    };
    if first.is_undefined() {
        return new_zeroed_typed_array_with_roots(kind, 0, gc_heap, external_visit);
    }
    if let Some(buf) = first.as_array_buffer() {
        if buf.is_detached(gc_heap) {
            return Err(VmError::TypeMismatch);
        }
        // §23.2.5.1 InitializeTypedArrayFromArrayBuffer: byteOffset
        // coercion and alignment / bounds failures are RangeErrors
        // (TypeError only for the Symbol/BigInt ToNumber failures that
        // `to_index_error` distinguishes).
        let byte_offset = match args.get(1) {
            None => 0u64,
            Some(v) if v.is_undefined() => 0u64,
            Some(v) => to_index(v, gc_heap).ok_or_else(|| to_index_error(v, "byteOffset"))?,
        } as usize;
        if !byte_offset.is_multiple_of(bpe) {
            return Err(VmError::RangeError {
                message: format!("start offset must be a multiple of {bpe}"),
            });
        }
        let buf_len = buf.byte_length(gc_heap);
        if byte_offset > buf_len {
            return Err(VmError::RangeError {
                message: "start offset is outside the bounds of the buffer".to_string(),
            });
        }
        let length = match args.get(2) {
            None => {
                let remaining = buf_len - byte_offset;
                if !remaining.is_multiple_of(bpe) {
                    return Err(VmError::RangeError {
                        message: format!(
                            "buffer length minus the offset must be a multiple of {bpe}"
                        ),
                    });
                }
                remaining / bpe
            }
            Some(v) if v.is_undefined() => {
                let remaining = buf_len - byte_offset;
                if !remaining.is_multiple_of(bpe) {
                    return Err(VmError::RangeError {
                        message: format!(
                            "buffer length minus the offset must be a multiple of {bpe}"
                        ),
                    });
                }
                remaining / bpe
            }
            Some(v) => {
                let n = to_index(v, gc_heap).ok_or_else(|| to_index_error(v, "length"))? as usize;
                if byte_offset + n * bpe > buf_len {
                    return Err(VmError::RangeError {
                        message: "invalid typed array length".to_string(),
                    });
                }
                n
            }
        };
        let view = JsTypedArray::new(gc_heap, buf, kind, byte_offset, length).map_err(oom_to_vm)?;
        // §23.2.5.1 — absent length over a resizable buffer makes
        // [[ArrayLength]] AUTO (length-tracking).
        let length_absent =
            args.get(2).is_none() || args.get(2).is_some_and(|v| v.is_undefined());
        if length_absent && buf.is_resizable(gc_heap) {
            view.set_length_tracking(gc_heap);
        }
        return Ok(Value::typed_array(view));
    }
    if let Some(src) = first.as_typed_array(gc_heap) {
        if src.buffer(gc_heap).is_detached(gc_heap) {
            return Err(VmError::TypeMismatch);
        }
        let len = src.length(gc_heap);
        let mut values: Vec<Value> = Vec::with_capacity(len);
        for i in 0..len {
            let v = src.get(gc_heap, i).map_err(oom_to_vm)?;
            values.push(coerce_for_kind(gc_heap, kind, &v)?);
        }
        return typed_array_from_values_with_roots(kind, &values, gc_heap, external_visit);
    }
    if let Some(arr) = first.as_array() {
        let len = crate::array::len(arr, gc_heap);
        let mut values: Vec<Value> = Vec::with_capacity(len);
        for i in 0..len {
            let v = crate::array::get(arr, gc_heap, i);
            values.push(coerce_for_kind(gc_heap, kind, &v)?);
        }
        return typed_array_from_values_with_roots(kind, &values, gc_heap, external_visit);
    }
    if first.is_number() || first.is_boolean() || first.is_null() || first.is_string() {
        let length = to_index(first, gc_heap).ok_or(VmError::TypeMismatch)? as usize;
        return new_zeroed_typed_array_with_roots(kind, length, gc_heap, external_visit);
    }
    if let Some(obj) = first.as_object() {
        let length_value = crate::object::get(obj, gc_heap, "length").unwrap_or(Value::undefined());
        let len = to_index(&length_value, gc_heap).ok_or(VmError::TypeMismatch)? as usize;
        let mut values: Vec<Value> = Vec::with_capacity(len);
        for i in 0..len {
            let v = crate::object::get(obj, gc_heap, &i.to_string()).unwrap_or(Value::undefined());
            values.push(coerce_for_kind(gc_heap, kind, &v)?);
        }
        return typed_array_from_values_with_roots(kind, &values, gc_heap, external_visit);
    }
    Err(VmError::TypeMismatch)
}

fn from_static_with_roots(
    kind: TypedArrayKind,
    args: &[Value],
    gc_heap: &mut otter_gc::GcHeap,
    external_visit: &mut otter_gc::heap::RootSlotVisitor<'_>,
) -> Result<Value, VmError> {
    let source = args.first().cloned().unwrap_or(Value::undefined());
    if let Some(src) = source.as_typed_array(gc_heap) {
        if src.buffer(gc_heap).is_detached(gc_heap) {
            return Err(VmError::TypeMismatch);
        }
        let len = src.length(gc_heap);
        let mut values: Vec<Value> = Vec::with_capacity(len);
        for i in 0..len {
            let v = src.get(gc_heap, i).map_err(oom_to_vm)?;
            values.push(coerce_for_kind(gc_heap, kind, &v)?);
        }
        return typed_array_from_values_with_roots(kind, &values, gc_heap, external_visit);
    }
    if let Some(arr) = source.as_array() {
        let len = crate::array::len(arr, gc_heap);
        let mut values: Vec<Value> = Vec::with_capacity(len);
        for i in 0..len {
            let v = crate::array::get(arr, gc_heap, i);
            values.push(coerce_for_kind(gc_heap, kind, &v)?);
        }
        return typed_array_from_values_with_roots(kind, &values, gc_heap, external_visit);
    }
    if let Some(s) = source.as_string(gc_heap) {
        let text = s.to_lossy_string(gc_heap);
        let mut chars: Vec<Value> = Vec::with_capacity(text.chars().count());
        for c in text.chars() {
            if kind.is_bigint() {
                let h =
                    crate::bigint::BigIntValue::from_i32(gc_heap, c as i32).map_err(oom_to_vm)?;
                chars.push(Value::big_int(h));
            } else {
                chars.push(Value::number(crate::number::NumberValue::from_i32(
                    c as i32,
                )));
            }
        }
        return typed_array_from_values_with_roots(kind, &chars, gc_heap, external_visit);
    }
    if let Some(obj) = source.as_object() {
        let len_value = crate::object::get(obj, gc_heap, "length").unwrap_or(Value::undefined());
        let len = to_index(&len_value, gc_heap).ok_or(VmError::TypeMismatch)? as usize;
        let mut values: Vec<Value> = Vec::with_capacity(len);
        for i in 0..len {
            let v = crate::object::get(obj, gc_heap, &i.to_string()).unwrap_or(Value::undefined());
            values.push(coerce_for_kind(gc_heap, kind, &v)?);
        }
        return typed_array_from_values_with_roots(kind, &values, gc_heap, external_visit);
    }
    Err(VmError::TypeMismatch)
}

fn of_static_with_roots(
    kind: TypedArrayKind,
    args: &[Value],
    gc_heap: &mut otter_gc::GcHeap,
    external_visit: &mut otter_gc::heap::RootSlotVisitor<'_>,
) -> Result<Value, VmError> {
    let mut values: Vec<Value> = Vec::with_capacity(args.len());
    for v in args {
        values.push(coerce_for_kind(gc_heap, kind, v)?);
    }
    typed_array_from_values_with_roots(kind, &values, gc_heap, external_visit)
}

/// §6.2.10 SetValueFromBuffer's element-type conversion gates: a
/// BigInt array rejects Number inputs and vice versa per §10.4.5.14.
fn coerce_for_kind(
    gc_heap: &mut otter_gc::GcHeap,
    kind: TypedArrayKind,
    value: &Value,
) -> Result<Value, VmError> {
    if kind.is_bigint() {
        if value.is_big_int() {
            Ok(*value)
        } else if let Some(b) = value.as_boolean() {
            let n = if b { 1 } else { 0 };
            Ok(Value::big_int(
                crate::bigint::BigIntValue::from_i32(gc_heap, n).map_err(oom_to_vm)?,
            ))
        } else if value.is_number() {
            Err(VmError::TypeMismatch)
        } else if let Some(s) = value.as_string(gc_heap) {
            let text = s.to_lossy_string(gc_heap);
            match crate::bigint::BigIntValue::from_decimal(gc_heap, text.trim()) {
                Some(Ok(b)) => Ok(Value::big_int(b)),
                Some(Err(e)) => Err(oom_to_vm(e)),
                None => Err(VmError::TypeMismatch),
            }
        } else {
            Err(VmError::TypeMismatch)
        }
    } else if value.is_big_int() {
        Err(VmError::TypeMismatch)
    } else if value.is_symbol() {
        // §7.1.4 ToNumber(Symbol) is a TypeError; a Symbol element
        // stored into a numeric TypedArray must throw rather than
        // silently coercing to a number.
        Err(VmError::TypeMismatch)
    } else {
        Ok(*value)
    }
}

/// Coerce a single TypedArray element write (used by `Op::StoreElement`
/// for indexed access). Raises [`VmError::TypeMismatch`] on
/// kind/value-type mismatch per §10.4.5.14
/// `IntegerIndexedElementSet` step 6.
pub fn coerce_element_for_store(
    gc_heap: &mut otter_gc::GcHeap,
    kind: TypedArrayKind,
    value: &Value,
) -> Result<Value, VmError> {
    coerce_for_kind(gc_heap, kind, value)
}

/// Used by `Array.from` / spread paths — extract the underlying
/// values of a TypedArray as plain `Value`s.
pub fn snapshot_elements(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NumberValue;
    use otter_bytecode::method_id::{ArrayBufferMethod, SharedArrayBufferMethod, TypedArrayMethod};

    #[test]
    fn array_buffer_constructor_with_roots_accounts_backing_store() {
        let mut heap = otter_gc::GcHeap::with_max_heap_bytes(1024 * 1024).expect("heap");
        let args = [Value::number(NumberValue::from_i32(64))];
        let mut external_visit = |_visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {};

        let before = heap.tracked_bytes();
        let value = array_buffer_call_with_roots(
            ArrayBufferMethod::Construct,
            &args,
            &mut heap,
            &mut external_visit,
        )
        .expect("array buffer");

        assert!(value.is_array_buffer());
        // `tracked_bytes` is heap-allocated payload + external
        // reservations; the GC body adds its own header/payload
        // overhead on top of the 64-byte backing store.
        let after = heap.tracked_bytes();
        assert!(after - before >= 64);
        let _ = value;
        // The backing store stays accounted until the GC body that
        // owns the `ExternalMemory` token is collected. After full
        // GC, the external reservation is released even if the
        // body's own heap page is retained.
        heap.collect_full(&mut |_| {});
        assert!(heap.tracked_bytes() <= after - 64);
    }

    #[test]
    fn typed_array_constructor_with_roots_accounts_backing_store() {
        let mut heap = otter_gc::GcHeap::with_max_heap_bytes(1024 * 1024).expect("heap");
        let args = [Value::number(NumberValue::from_i32(4))];
        let mut external_visit = |_visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {};

        let before = heap.tracked_bytes();
        let value = typed_array_call_with_roots(
            TypedArrayKind::Int16,
            TypedArrayMethod::Construct,
            &args,
            &mut heap,
            &mut external_visit,
        )
        .expect("typed array");

        assert!(value.is_typed_array());
        let after = heap.tracked_bytes();
        assert!(after - before >= 8);
        let _ = value;
        heap.collect_full(&mut |_| {});
        assert!(heap.tracked_bytes() <= after - 8);
    }

    #[test]
    fn shared_array_buffer_constructor_uses_rooted_dispatch_boundary() {
        let mut heap = otter_gc::GcHeap::with_max_heap_bytes(1024 * 1024).expect("heap");
        let args = [Value::number(NumberValue::from_i32(64))];
        let mut visited_roots = false;
        let mut external_visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
            visited_roots = true;
            visitor(std::ptr::null_mut());
        };

        let value = shared_array_buffer_call_with_roots(
            SharedArrayBufferMethod::Construct,
            &args,
            &mut heap,
            &mut external_visit,
        )
        .expect("shared array buffer");

        assert!(visited_roots);
        let Some(buffer) = value.as_array_buffer() else {
            panic!("expected array buffer");
        };
        assert!(buffer.is_shared());
        assert_eq!(buffer.shared_external_bytes_for_test(&heap), Some(64));
        let after = heap.tracked_bytes();
        assert!(after >= 64);
        // `buffer` is a `Copy` GC handle; the body it points at is
        // unreachable from any root, so a full GC collects it and
        // releases the external reservation.
        heap.collect_full(&mut |_| {});
        assert!(heap.tracked_bytes() <= after - 64);
    }

    #[test]
    fn shared_array_buffer_growable_accounts_max_backing_store() {
        let mut heap = otter_gc::GcHeap::with_max_heap_bytes(1024 * 1024).expect("heap");
        let options = crate::object::alloc_object_old_for_fixture(&mut heap).expect("options");
        crate::object::set(
            options,
            &mut heap,
            "maxByteLength",
            Value::number(NumberValue::from_i32(128)),
        );
        let args = [
            Value::number(NumberValue::from_i32(64)),
            Value::object(options),
        ];
        let mut external_visit = |_visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {};

        let before = heap.tracked_bytes();
        let value = shared_array_buffer_call_with_roots(
            SharedArrayBufferMethod::Construct,
            &args,
            &mut heap,
            &mut external_visit,
        )
        .expect("shared array buffer");

        let Some(buffer) = value.as_array_buffer() else {
            panic!("expected array buffer");
        };
        assert!(buffer.is_shared());
        assert!(buffer.is_growable(&heap));
        assert_eq!(buffer.shared_external_bytes_for_test(&heap), Some(128));
        let after = heap.tracked_bytes();
        assert!(after - before >= 128);
        heap.collect_full(&mut |_| {});
        assert!(heap.tracked_bytes() <= after - 128);
    }
}
