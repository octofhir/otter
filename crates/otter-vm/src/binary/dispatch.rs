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

use crate::array::JsArray;
use crate::{Value, VmError};

use super::array_buffer::JsArrayBuffer;
use super::data_view::JsDataView;
use super::typed_array::{JsTypedArray, TypedArrayKind};
use super::{to_index, typed_array_prototype};

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
    gc_heap: &otter_gc::GcHeap,
) -> Result<Value, VmError> {
    use otter_bytecode::method_id::ArrayBufferMethod as M;
    match method {
        // §25.1.4 `new ArrayBuffer(length [, options])`. The second
        // argument is an options bag with an optional `maxByteLength`
        // property; when present, the buffer is resizable.
        M::Construct => {
            let length = match args.first() {
                None | Some(Value::Undefined) => 0u64,
                Some(v) => to_index(v).ok_or(VmError::TypeMismatch)?,
            };
            let max_byte_length = match args.get(1) {
                Some(Value::Object(opts)) => {
                    if let Some(v) = crate::object::get(*opts, gc_heap, "maxByteLength") {
                        Some(to_index(&v).ok_or(VmError::TypeMismatch)?)
                    } else {
                        None
                    }
                }
                _ => None,
            };
            let len = length as usize;
            let buf = match max_byte_length {
                Some(max) => {
                    let max = max as usize;
                    if max < len {
                        return Err(VmError::TypeMismatch);
                    }
                    JsArrayBuffer::new_resizable(len, max)
                }
                None => JsArrayBuffer::try_new(len).ok_or_else(|| VmError::RangeError {
                    message: format!(
                        "ArrayBuffer allocation of {len} bytes exceeds the available heap"
                    ),
                })?,
            };
            Ok(Value::ArrayBuffer(buf))
        }
        // §25.1.3.1 ArrayBuffer.isView(arg) — returns `true` when
        // arg is a TypedArray or DataView.
        M::IsView => {
            let v = args.first().cloned().unwrap_or(Value::Undefined);
            Ok(Value::Boolean(matches!(
                v,
                Value::TypedArray(_) | Value::DataView(_)
            )))
        }
    }
}

/// Dispatch `new SharedArrayBuffer(length [, options])` per
/// §25.2.1. Only the `maxByteLength` option (growable SAB) is
/// honoured; arbitrary key options are ignored.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-sharedarraybuffer-constructor>
pub fn shared_array_buffer_call(
    method: otter_bytecode::method_id::SharedArrayBufferMethod,
    args: &[Value],
    gc_heap: &otter_gc::GcHeap,
) -> Result<Value, VmError> {
    use otter_bytecode::method_id::SharedArrayBufferMethod as M;
    match method {
        M::Construct => {
            let length = match args.first() {
                None | Some(Value::Undefined) => 0u64,
                Some(v) => to_index(v).ok_or(VmError::TypeMismatch)?,
            };
            let max_byte_length = match args.get(1) {
                Some(Value::Object(opts)) => {
                    if let Some(v) = crate::object::get(*opts, gc_heap, "maxByteLength") {
                        Some(to_index(&v).ok_or(VmError::TypeMismatch)?)
                    } else {
                        None
                    }
                }
                _ => None,
            };
            let len = length as usize;
            let buf = match max_byte_length {
                Some(max) => {
                    let max = max as usize;
                    if max < len {
                        return Err(VmError::TypeMismatch);
                    }
                    JsArrayBuffer::new_shared_growable(len, max)
                }
                None => JsArrayBuffer::try_new_shared(len).ok_or_else(|| VmError::RangeError {
                    message: format!(
                        "SharedArrayBuffer allocation of {len} bytes exceeds the available heap"
                    ),
                })?,
            };
            Ok(Value::ArrayBuffer(buf))
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
) -> Result<Value, VmError> {
    use otter_bytecode::method_id::DataViewMethod as M;
    match method {
        M::Construct => {
            let buffer = match args.first() {
                Some(Value::ArrayBuffer(b)) => b.clone(),
                _ => return Err(VmError::TypeMismatch),
            };
            if buffer.is_detached() {
                return Err(VmError::TypeMismatch);
            }
            let buffer_byte_length = buffer.byte_length();
            let byte_offset = match args.get(1) {
                None | Some(Value::Undefined) => 0u64,
                Some(v) => to_index(v).ok_or(VmError::TypeMismatch)?,
            } as usize;
            if byte_offset > buffer_byte_length {
                return Err(VmError::TypeMismatch);
            }
            let byte_length = match args.get(2) {
                None | Some(Value::Undefined) => buffer_byte_length - byte_offset,
                Some(v) => {
                    let n = to_index(v).ok_or(VmError::TypeMismatch)? as usize;
                    if byte_offset + n > buffer_byte_length {
                        return Err(VmError::TypeMismatch);
                    }
                    n
                }
            };
            Ok(Value::DataView(JsDataView::new(
                buffer,
                byte_offset,
                byte_length,
            )))
        }
    }
}

// =========================================================================
// TypedArray
// =========================================================================

/// Dispatch `new <T>(...)` / `<T>.from(...)` / `<T>.of(...)` for one
/// of the eleven concrete TypedArray classes via the typed
/// [`TypedArrayMethod`].
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-typedarray-constructors>
/// - <https://tc39.es/ecma262/#sec-%25typedarray%25.from>
/// - <https://tc39.es/ecma262/#sec-%25typedarray%25.of>
pub fn typed_array_call(
    kind: TypedArrayKind,
    method: otter_bytecode::method_id::TypedArrayMethod,
    args: &[Value],
    gc_heap: &otter_gc::GcHeap,
) -> Result<Value, VmError> {
    use otter_bytecode::method_id::TypedArrayMethod as M;
    match method {
        M::Construct => construct_typed_array(kind, args, gc_heap),
        M::From => from_static(kind, args, gc_heap),
        M::Of => of_static(kind, args),
    }
}

fn construct_typed_array(
    kind: TypedArrayKind,
    args: &[Value],
    gc_heap: &otter_gc::GcHeap,
) -> Result<Value, VmError> {
    let bpe = kind.bytes_per_element();
    match args.first() {
        // §23.2.5.1.1 `new T()` / `new T(undefined)` — zero-length view.
        None | Some(Value::Undefined) => Ok(Value::TypedArray(JsTypedArray::new(
            JsArrayBuffer::new(0),
            kind,
            0,
            0,
        ))),
        // §23.2.5.1.4 `new T(buffer, byteOffset?, length?)`.
        Some(Value::ArrayBuffer(buf)) => {
            if buf.is_detached() {
                return Err(VmError::TypeMismatch);
            }
            let byte_offset = match args.get(1) {
                None | Some(Value::Undefined) => 0u64,
                Some(v) => to_index(v).ok_or(VmError::TypeMismatch)?,
            } as usize;
            if !byte_offset.is_multiple_of(bpe) {
                return Err(VmError::TypeMismatch);
            }
            let buf_len = buf.byte_length();
            if byte_offset > buf_len {
                return Err(VmError::TypeMismatch);
            }
            let length = match args.get(2) {
                None | Some(Value::Undefined) => {
                    let remaining = buf_len - byte_offset;
                    if !remaining.is_multiple_of(bpe) {
                        return Err(VmError::TypeMismatch);
                    }
                    remaining / bpe
                }
                Some(v) => {
                    let n = to_index(v).ok_or(VmError::TypeMismatch)? as usize;
                    if byte_offset + n * bpe > buf_len {
                        return Err(VmError::TypeMismatch);
                    }
                    n
                }
            };
            Ok(Value::TypedArray(JsTypedArray::new(
                buf.clone(),
                kind,
                byte_offset,
                length,
            )))
        }
        // §23.2.5.1.3 `new T(typedArray)` — copy elements with element-
        // type conversion.
        Some(Value::TypedArray(src)) => {
            if src.buffer().is_detached() {
                return Err(VmError::TypeMismatch);
            }
            let len = src.length();
            let new_buf = JsArrayBuffer::new(len * bpe);
            let view = JsTypedArray::new(new_buf, kind, 0, len);
            for i in 0..len {
                let v = src.get(i);
                let coerced = coerce_for_kind(kind, &v)?;
                view.set(i, &coerced);
            }
            Ok(Value::TypedArray(view))
        }
        // §23.2.5.1.5 `new T(object)` — array-like / iterable copy.
        Some(Value::Array(arr)) => {
            let len = crate::array::len(*arr, gc_heap);
            let new_buf = JsArrayBuffer::new(len * bpe);
            let view = JsTypedArray::new(new_buf, kind, 0, len);
            for i in 0..len {
                let v = crate::array::get(*arr, gc_heap, i);
                let coerced = coerce_for_kind(kind, &v)?;
                view.set(i, &coerced);
            }
            Ok(Value::TypedArray(view))
        }
        // §23.2.5.1.2 `new T(length)`.
        Some(Value::Number(_) | Value::Boolean(_) | Value::Null) => {
            let length = to_index(args.first().unwrap()).ok_or(VmError::TypeMismatch)? as usize;
            let new_buf = JsArrayBuffer::new(length * bpe);
            Ok(Value::TypedArray(JsTypedArray::new(
                new_buf, kind, 0, length,
            )))
        }
        // String "5" coerces to length 5 via ToIndex.
        Some(Value::String(_)) => {
            let length = to_index(args.first().unwrap()).ok_or(VmError::TypeMismatch)? as usize;
            let new_buf = JsArrayBuffer::new(length * bpe);
            Ok(Value::TypedArray(JsTypedArray::new(
                new_buf, kind, 0, length,
            )))
        }
        // Generic object — read `.length` then index 0..length per the
        // array-like path.
        Some(Value::Object(obj)) => {
            let length_value =
                crate::object::get(*obj, gc_heap, "length").unwrap_or(Value::Undefined);
            let len = to_index(&length_value).ok_or(VmError::TypeMismatch)? as usize;
            let new_buf = JsArrayBuffer::new(len * bpe);
            let view = JsTypedArray::new(new_buf, kind, 0, len);
            for i in 0..len {
                let v =
                    crate::object::get(*obj, gc_heap, &i.to_string()).unwrap_or(Value::Undefined);
                let coerced = coerce_for_kind(kind, &v)?;
                view.set(i, &coerced);
            }
            Ok(Value::TypedArray(view))
        }
        _ => Err(VmError::TypeMismatch),
    }
}

/// §23.2.2.1 `%TypedArray%.from(source)` — synchronous, no map fn.
/// Map function and `thisArg` are filed for the callback-driven
/// dispatcher in `lib.rs::typed_array_callback_dispatch`.
fn from_static(
    kind: TypedArrayKind,
    args: &[Value],
    gc_heap: &otter_gc::GcHeap,
) -> Result<Value, VmError> {
    let source = args.first().cloned().unwrap_or(Value::Undefined);
    match source {
        Value::TypedArray(src) => {
            if src.buffer().is_detached() {
                return Err(VmError::TypeMismatch);
            }
            let len = src.length();
            let mut values: Vec<Value> = Vec::with_capacity(len);
            for i in 0..len {
                let v = src.get(i);
                values.push(coerce_for_kind(kind, &v)?);
            }
            Ok(typed_array_prototype::from_values(kind, &values))
        }
        Value::Array(arr) => {
            let len = crate::array::len(arr, gc_heap);
            let mut values: Vec<Value> = Vec::with_capacity(len);
            for i in 0..len {
                let v = crate::array::get(arr, gc_heap, i);
                values.push(coerce_for_kind(kind, &v)?);
            }
            Ok(typed_array_prototype::from_values(kind, &values))
        }
        Value::String(s) => {
            // Spread the string by code units (code-point semantics
            // belong to the iterator path; the no-callback `from`
            // here matches the array-like length walk).
            let text = s.to_lossy_string();
            let chars: Vec<Value> = text
                .chars()
                .map(|c| {
                    if kind.is_bigint() {
                        Value::BigInt(crate::bigint::BigIntValue::from_i32(c as i32))
                    } else {
                        Value::Number(crate::number::NumberValue::from_i32(c as i32))
                    }
                })
                .collect();
            Ok(typed_array_prototype::from_values(kind, &chars))
        }
        Value::Object(obj) => {
            let len_value = crate::object::get(obj, gc_heap, "length").unwrap_or(Value::Undefined);
            let len = to_index(&len_value).ok_or(VmError::TypeMismatch)? as usize;
            let mut values: Vec<Value> = Vec::with_capacity(len);
            for i in 0..len {
                let v =
                    crate::object::get(obj, gc_heap, &i.to_string()).unwrap_or(Value::Undefined);
                values.push(coerce_for_kind(kind, &v)?);
            }
            Ok(typed_array_prototype::from_values(kind, &values))
        }
        _ => Err(VmError::TypeMismatch),
    }
}

/// §23.2.2.2 `%TypedArray%.of(...items)`.
fn of_static(kind: TypedArrayKind, args: &[Value]) -> Result<Value, VmError> {
    let mut values: Vec<Value> = Vec::with_capacity(args.len());
    for v in args {
        values.push(coerce_for_kind(kind, v)?);
    }
    Ok(typed_array_prototype::from_values(kind, &values))
}

/// §6.2.10 SetValueFromBuffer's element-type conversion gates: a
/// BigInt array rejects Number inputs and vice versa per §10.4.5.14.
fn coerce_for_kind(kind: TypedArrayKind, value: &Value) -> Result<Value, VmError> {
    if kind.is_bigint() {
        match value {
            Value::BigInt(_) => Ok(value.clone()),
            Value::Boolean(true) => Ok(Value::BigInt(crate::bigint::BigIntValue::from_i32(1))),
            Value::Boolean(false) => Ok(Value::BigInt(crate::bigint::BigIntValue::from_i32(0))),
            // Spec rejects Number → BigInt array store with TypeError.
            Value::Number(_) => Err(VmError::TypeMismatch),
            Value::String(s) => {
                let text = s.to_lossy_string();
                match crate::bigint::BigIntValue::from_decimal(text.trim()) {
                    Some(b) => Ok(Value::BigInt(b)),
                    None => Err(VmError::TypeMismatch),
                }
            }
            _ => Err(VmError::TypeMismatch),
        }
    } else {
        match value {
            // Spec rejects BigInt → Number array store with TypeError.
            Value::BigInt(_) => Err(VmError::TypeMismatch),
            _ => Ok(value.clone()),
        }
    }
}

/// Coerce a single TypedArray element write (used by `Op::StoreElement`
/// for indexed access). Raises [`VmError::TypeMismatch`] on
/// kind/value-type mismatch per §10.4.5.14
/// `IntegerIndexedElementSet` step 6.
pub fn coerce_element_for_store(kind: TypedArrayKind, value: &Value) -> Result<Value, VmError> {
    coerce_for_kind(kind, value)
}

/// Used by `Array.from` / spread paths — extract the underlying
/// values of a TypedArray as plain `Value`s.
#[must_use]
pub fn snapshot_elements(t: &JsTypedArray) -> Vec<Value> {
    (0..t.length()).map(|i| t.get(i)).collect()
}

/// Convenience: build a TypedArray from a slice of arrays. Used by
/// the iterator-based path in `Array.from`.
#[must_use]
pub fn from_arr(kind: TypedArrayKind, arr: &JsArray, gc_heap: &otter_gc::GcHeap) -> Value {
    let len = crate::array::len(*arr, gc_heap);
    let mut values: Vec<Value> = Vec::with_capacity(len);
    for i in 0..len {
        values.push(crate::array::get(*arr, gc_heap, i));
    }
    typed_array_prototype::from_values(kind, &values)
}
