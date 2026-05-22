//! `TypedArray` value (ECMA-262 §23.2) and its element-kind enum.
//!
//! A `TypedArray` is a typed view over an [`super::JsArrayBuffer`].
//! The view records its element kind, byte offset into the buffer,
//! and length in elements; reads / writes coerce through the
//! kind-specific element-type rules per §6.2.10
//! `GetValueFromBuffer` / `SetValueFromBuffer`.
//!
//! All multi-byte integer / float access is little-endian on disk —
//! matching the §6.2.10 platform-default for `TypedArray` views per
//! §6.2.10.1 `IsBigEndian` (always `false`).
//!
//! # Contents
//! - [`TypedArrayKind`] — element-type tag with read / write helpers.
//! - [`JsTypedArray`] — cheap-to-clone view handle.
//! - [`TypedArrayBody`] — internal storage.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-typedarray-objects>
//! - <https://tc39.es/ecma262/#sec-getvaluefrombuffer>
//! - <https://tc39.es/ecma262/#sec-setvaluefrombuffer>

use num_bigint::BigInt;
use num_traits::ToPrimitive;

use crate::Value;
use crate::bigint::BigIntValue;
use crate::number::{NumberValue, bitwise};

use super::array_buffer::JsArrayBuffer;

/// One of the eleven concrete TypedArray element kinds.
///
/// Discriminants are stable so the compiler can encode a kind as
/// the leading [`Operand::ConstIndex`] payload of
/// [`Op::TypedArrayCall`] and the runtime can decode it via
/// [`Self::from_u32`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum TypedArrayKind {
    /// `Int8Array` — signed 1-byte integer.
    Int8 = 0,
    /// `Uint8Array` — unsigned 1-byte integer.
    Uint8 = 1,
    /// `Uint8ClampedArray` — unsigned 1-byte, clamped on store
    /// per §6.1.6 `ToUint8Clamp`.
    Uint8Clamped = 2,
    /// `Int16Array` — signed 2-byte integer.
    Int16 = 3,
    /// `Uint16Array` — unsigned 2-byte integer.
    Uint16 = 4,
    /// `Int32Array` — signed 4-byte integer.
    Int32 = 5,
    /// `Uint32Array` — unsigned 4-byte integer.
    Uint32 = 6,
    /// `Float32Array` — IEEE-754 single.
    Float32 = 7,
    /// `Float64Array` — IEEE-754 double.
    Float64 = 8,
    /// `BigInt64Array` — signed 8-byte integer; values are JS
    /// `BigInt`.
    BigInt64 = 9,
    /// `BigUint64Array` — unsigned 8-byte integer; values are JS
    /// `BigInt`.
    BigUint64 = 10,
}

impl TypedArrayKind {
    /// Parse a constructor name into a kind. Returns `None` for any
    /// other name; the dispatcher uses that to fall through.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "Int8Array" => Self::Int8,
            "Uint8Array" => Self::Uint8,
            "Uint8ClampedArray" => Self::Uint8Clamped,
            "Int16Array" => Self::Int16,
            "Uint16Array" => Self::Uint16,
            "Int32Array" => Self::Int32,
            "Uint32Array" => Self::Uint32,
            "Float32Array" => Self::Float32,
            "Float64Array" => Self::Float64,
            "BigInt64Array" => Self::BigInt64,
            "BigUint64Array" => Self::BigUint64,
            _ => return None,
        })
    }

    /// Decode a discriminant produced by [`as_u32`](Self::as_u32).
    #[must_use]
    pub fn from_u32(value: u32) -> Option<Self> {
        Some(match value {
            0 => Self::Int8,
            1 => Self::Uint8,
            2 => Self::Uint8Clamped,
            3 => Self::Int16,
            4 => Self::Uint16,
            5 => Self::Int32,
            6 => Self::Uint32,
            7 => Self::Float32,
            8 => Self::Float64,
            9 => Self::BigInt64,
            10 => Self::BigUint64,
            _ => return None,
        })
    }

    /// Encode as the `u32` carried by `Operand::ConstIndex`.
    #[must_use]
    #[inline]
    pub fn as_u32(self) -> u32 {
        self as u32
    }

    /// Constructor name for diagnostics and `[Symbol.toStringTag]`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Int8 => "Int8Array",
            Self::Uint8 => "Uint8Array",
            Self::Uint8Clamped => "Uint8ClampedArray",
            Self::Int16 => "Int16Array",
            Self::Uint16 => "Uint16Array",
            Self::Int32 => "Int32Array",
            Self::Uint32 => "Uint32Array",
            Self::Float32 => "Float32Array",
            Self::Float64 => "Float64Array",
            Self::BigInt64 => "BigInt64Array",
            Self::BigUint64 => "BigUint64Array",
        }
    }

    /// Bytes per element per §6.2.10 Table 71.
    #[must_use]
    pub const fn bytes_per_element(self) -> usize {
        match self {
            Self::Int8 | Self::Uint8 | Self::Uint8Clamped => 1,
            Self::Int16 | Self::Uint16 => 2,
            Self::Int32 | Self::Uint32 | Self::Float32 => 4,
            Self::Float64 | Self::BigInt64 | Self::BigUint64 => 8,
        }
    }

    /// `true` when the element-type is BigInt (`BigInt64Array` /
    /// `BigUint64Array`).
    #[must_use]
    pub const fn is_bigint(self) -> bool {
        matches!(self, Self::BigInt64 | Self::BigUint64)
    }

    /// Decode `bytes_per_element()` bytes at `offset` into a
    /// JavaScript value per §6.2.10 `GetValueFromBuffer` (always
    /// little-endian for TypedArray indexed access).
    ///
    /// The BigInt-kind arms allocate a fresh body on `heap`; numeric
    /// kinds short-circuit without touching the heap.
    pub fn read(
        self,
        heap: &mut otter_gc::GcHeap,
        bytes: &[u8],
        offset: usize,
    ) -> Result<Value, otter_gc::OutOfMemory> {
        let bpe = self.bytes_per_element();
        if offset + bpe > bytes.len() {
            return match self {
                Self::BigInt64 | Self::BigUint64 => {
                    let handle = BigIntValue::from_inner(heap, BigInt::from(0))?;
                    Ok(Value::big_int(handle))
                }
                _ => Ok(Value::number_i32(0)),
            };
        }
        let slice = &bytes[offset..offset + bpe];
        Ok(match self {
            Self::Int8 => Value::number_i32(i8::from_le_bytes([slice[0]]) as i32),
            Self::Uint8 | Self::Uint8Clamped => Value::number_i32(slice[0] as i32),
            Self::Int16 => {
                let v = i16::from_le_bytes([slice[0], slice[1]]);
                Value::number_i32(v as i32)
            }
            Self::Uint16 => {
                let v = u16::from_le_bytes([slice[0], slice[1]]);
                Value::number_i32(v as i32)
            }
            Self::Int32 => {
                let v = i32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]);
                Value::number_i32(v)
            }
            Self::Uint32 => {
                let v = u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]);
                Value::number_f64(v as f64)
            }
            Self::Float32 => {
                let v = f32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]);
                Value::number_f64(v as f64)
            }
            Self::Float64 => {
                let mut buf = [0u8; 8];
                buf.copy_from_slice(slice);
                Value::number_f64(f64::from_le_bytes(buf))
            }
            Self::BigInt64 => {
                let mut buf = [0u8; 8];
                buf.copy_from_slice(slice);
                let v = i64::from_le_bytes(buf);
                let handle = BigIntValue::from_inner(heap, BigInt::from(v))?;
                Value::big_int(handle)
            }
            Self::BigUint64 => {
                let mut buf = [0u8; 8];
                buf.copy_from_slice(slice);
                let v = u64::from_le_bytes(buf);
                let handle = BigIntValue::from_inner(heap, BigInt::from(v))?;
                Value::big_int(handle)
            }
        })
    }

    /// Encode `value` into the kind's element type and write it at
    /// `offset` per §6.2.10 `SetValueFromBuffer`. Out-of-range
    /// offsets silently no-op (the caller has already bounds-checked
    /// the index against `length`). The BigInt-kind arms clone the
    /// body payload out of `heap`; numeric kinds ignore the heap.
    pub fn write(self, heap: &otter_gc::GcHeap, bytes: &mut [u8], offset: usize, value: &Value) {
        let bpe = self.bytes_per_element();
        if offset + bpe > bytes.len() {
            return;
        }
        match self {
            Self::Int8 => {
                let n = number_to_int_truncated(value, heap);
                bytes[offset] = (n as i8) as u8;
            }
            Self::Uint8 => {
                let n = number_to_int_truncated(value, heap);
                bytes[offset] = n as u8;
            }
            Self::Uint8Clamped => {
                let n = to_uint8_clamp(value, heap);
                bytes[offset] = n;
            }
            Self::Int16 => {
                let n = number_to_int_truncated(value, heap) as i16;
                bytes[offset..offset + 2].copy_from_slice(&n.to_le_bytes());
            }
            Self::Uint16 => {
                let n = number_to_int_truncated(value, heap) as u16;
                bytes[offset..offset + 2].copy_from_slice(&n.to_le_bytes());
            }
            Self::Int32 => {
                let n = bitwise::to_int32(value_to_number(value, heap));
                bytes[offset..offset + 4].copy_from_slice(&n.to_le_bytes());
            }
            Self::Uint32 => {
                let n = bitwise::to_uint32(value_to_number(value, heap));
                bytes[offset..offset + 4].copy_from_slice(&n.to_le_bytes());
            }
            Self::Float32 => {
                let n = value_to_number(value, heap).as_f64() as f32;
                bytes[offset..offset + 4].copy_from_slice(&n.to_le_bytes());
            }
            Self::Float64 => {
                let n = value_to_number(value, heap).as_f64();
                bytes[offset..offset + 8].copy_from_slice(&n.to_le_bytes());
            }
            Self::BigInt64 => {
                let n = value_to_bigint64(value, heap);
                bytes[offset..offset + 8].copy_from_slice(&n.to_le_bytes());
            }
            Self::BigUint64 => {
                let n = value_to_biguint64(value, heap);
                bytes[offset..offset + 8].copy_from_slice(&n.to_le_bytes());
            }
        }
    }
}

/// §6.1.6.1.5 `Number::ToInt(value)` — truncate toward zero. NaN and
/// infinities map to `0`.
fn number_to_int_truncated(value: &Value, heap: &otter_gc::GcHeap) -> i64 {
    let n = value_to_number(value, heap).as_f64();
    if !n.is_finite() {
        return 0;
    }
    n.trunc() as i64
}

/// §6.1.6.1 `ToUint8Clamp(value)`.
fn to_uint8_clamp(value: &Value, heap: &otter_gc::GcHeap) -> u8 {
    let n = value_to_number(value, heap).as_f64();
    if n.is_nan() || n <= 0.0 {
        return 0;
    }
    if n >= 255.0 {
        return 255;
    }
    // Round to even on .5 ties per §6.1.6.1.5 step 8.
    let floor = n.floor();
    let frac = n - floor;
    if frac < 0.5 {
        return floor as u8;
    }
    if frac > 0.5 {
        return (floor + 1.0) as u8;
    }
    let f = floor as u64;
    if f.is_multiple_of(2) {
        floor as u8
    } else {
        (floor + 1.0) as u8
    }
}

/// Coerce a JS value to a Number per §7.1.4 ToNumber. BigInt → drop
/// to NaN (the per-kind path handles BigInt arrays separately).
fn value_to_number(value: &Value, heap: &otter_gc::GcHeap) -> NumberValue {
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
    if let Some(s) = value.as_string() {
        return crate::number::to_number_from_string(&s.to_lossy_string(heap));
    }
    NumberValue::from_f64(f64::NAN)
}

/// §6.1.6.2.4 `BigInt::toInt64` — wrap to signed 64-bit. Non-BigInt
/// values fall through `ToBigInt` (here approximated by 0 for
/// non-coercible inputs; the dispatcher rejects bad types upstream).
fn value_to_bigint64(value: &Value, heap: &otter_gc::GcHeap) -> i64 {
    let big = if let Some(b) = value.as_big_int() {
        b.clone_inner(heap)
    } else if let Some(b) = value.as_boolean() {
        BigInt::from(if b { 1 } else { 0 })
    } else {
        return 0;
    };
    let modulus: BigInt = BigInt::from(1u64) << 64;
    let mut wrapped: BigInt = &big % &modulus;
    use num_traits::Signed;
    if wrapped.is_negative() {
        wrapped += &modulus;
    }
    let half: BigInt = BigInt::from(1u64) << 63;
    if wrapped >= half {
        wrapped -= modulus;
    }
    wrapped.to_i64().unwrap_or(0)
}

/// §6.1.6.2.5 `BigInt::toUint64`.
fn value_to_biguint64(value: &Value, heap: &otter_gc::GcHeap) -> u64 {
    let big = if let Some(b) = value.as_big_int() {
        b.clone_inner(heap)
    } else if let Some(b) = value.as_boolean() {
        BigInt::from(if b { 1 } else { 0 })
    } else {
        return 0;
    };
    let modulus: BigInt = BigInt::from(1u64) << 64;
    let mut wrapped: BigInt = &big % &modulus;
    use num_traits::Signed;
    if wrapped.is_negative() {
        wrapped += &modulus;
    }
    wrapped.to_u64().unwrap_or(0)
}

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for [`TypedArrayBodyGc`].
pub const TYPED_ARRAY_BODY_TYPE_TAG: u8 = 0x2b;

/// GC body for `Value::TypedArray` per ECMA-262 §23.2.
///
/// Mutators flip `expando` through [`otter_gc::GcHeap::with_payload`]
/// (no interior mutability in GC bodies).
#[derive(Debug)]
pub struct TypedArrayBodyGc {
    /// Backing buffer.
    pub buffer: JsArrayBuffer,
    /// Element-type kind.
    pub kind: TypedArrayKind,
    /// Byte offset into the backing buffer at construction time.
    pub byte_offset: usize,
    /// Element count at construction time.
    pub length: usize,
    /// Lazy expando bag for non-canonical-numeric own properties.
    pub expando: Option<crate::object::JsObject>,
}

impl otter_gc::SafeTraceable for TypedArrayBodyGc {
    const TYPE_TAG: u8 = TYPED_ARRAY_BODY_TYPE_TAG;

    fn trace_slots_safe(&self, visitor: &mut otter_gc::raw::SlotVisitor<'_>) {
        // Forward to the buffer's GC handle so the backing ArrayBuffer
        // body survives the cycle.
        self.buffer.trace_value_slots(visitor);
        if let Some(expando) = &self.expando
            && !expando.is_null()
        {
            let p = expando as *const crate::object::JsObject as *mut otter_gc::raw::RawGc;
            visitor(p);
        }
    }
}

/// 4-byte compressed GC handle to a [`TypedArrayBodyGc`]. `Copy`.
/// Packs into [`crate::Value`] under `TAG_PTR_OBJECT`.
pub type TypedArrayHandle = otter_gc::Gc<TypedArrayBodyGc>;

/// Allocate a TypedArray body on the GC heap.
///
/// # Errors
///
/// Surfaces [`otter_gc::OutOfMemory`] verbatim.
pub fn alloc_typed_array(
    heap: &mut otter_gc::GcHeap,
    buffer: JsArrayBuffer,
    kind: TypedArrayKind,
    byte_offset: usize,
    length: usize,
) -> Result<TypedArrayHandle, otter_gc::OutOfMemory> {
    heap.alloc_old(TypedArrayBodyGc {
        buffer,
        kind,
        byte_offset,
        length,
        expando: None,
    })
}

/// Cheap-to-copy TypedArray view.
///
/// Backed by a 4-byte compressed GC handle; `Copy + Eq + Hash`.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub struct JsTypedArray {
    handle: TypedArrayHandle,
    /// Element kind is fixed at construction; cached here so hot
    /// paths (`kind()`, `bytes_per_element`) stay heap-free.
    cached_kind: TypedArrayKind,
}

impl JsTypedArray {
    /// Build a view at `byte_offset` over `length` elements of `kind`.
    /// Caller must already have validated alignment and bounds.
    ///
    /// # Errors
    ///
    /// Surfaces [`otter_gc::OutOfMemory`] verbatim.
    pub fn new(
        heap: &mut otter_gc::GcHeap,
        buffer: JsArrayBuffer,
        kind: TypedArrayKind,
        byte_offset: usize,
        length: usize,
    ) -> Result<Self, otter_gc::OutOfMemory> {
        let handle = alloc_typed_array(heap, buffer, kind, byte_offset, length)?;
        Ok(Self {
            handle,
            cached_kind: kind,
        })
    }

    /// Rewrap an existing handle. Caller must have read the kind out
    /// of the body separately (or know it by construction).
    #[must_use]
    pub fn from_handle(handle: TypedArrayHandle, kind: TypedArrayKind) -> Self {
        Self {
            handle,
            cached_kind: kind,
        }
    }

    /// Rewrap an existing handle by reading the element kind out of
    /// the body once. Used by `Value::as_typed_array` and call sites
    /// that recover the wrapper from a tagged `Value`.
    #[must_use]
    pub fn from_handle_with_heap(heap: &otter_gc::GcHeap, handle: TypedArrayHandle) -> Self {
        let cached_kind = heap.read_payload(handle, |body| body.kind);
        Self {
            handle,
            cached_kind,
        }
    }

    /// Underlying GC handle.
    #[must_use]
    pub fn handle(self) -> TypedArrayHandle {
        self.handle
    }

    /// Read the lazy expando bag, if one has been created.
    #[must_use]
    pub fn expando(self, heap: &otter_gc::GcHeap) -> Option<crate::object::JsObject> {
        heap.read_payload(self.handle, |body| body.expando)
    }

    /// Install / replace the lazy expando bag.
    pub fn set_expando(self, heap: &mut otter_gc::GcHeap, expando: crate::object::JsObject) {
        heap.with_payload(self.handle, |body| body.expando = Some(expando));
    }

    /// Backing buffer.
    #[must_use]
    pub fn buffer(self, heap: &otter_gc::GcHeap) -> JsArrayBuffer {
        heap.read_payload(self.handle, |body| body.buffer)
    }

    /// Element kind. Cached on the wrapper for heap-free access.
    #[must_use]
    pub fn kind(self) -> TypedArrayKind {
        self.cached_kind
    }

    /// Byte offset into the backing buffer. `0` when the backing
    /// buffer is detached per §10.4.5.10 `IntegerIndexedObjectByteOffset`.
    #[must_use]
    pub fn byte_offset(self, heap: &otter_gc::GcHeap) -> usize {
        let (buffer, off) = heap.read_payload(self.handle, |body| (body.buffer, body.byte_offset));
        if buffer.is_detached(heap) {
            return 0;
        }
        off
    }

    /// Construction-time byte offset, ignoring detached state. Used
    /// by internal paths that already gate on `is_detached`.
    #[must_use]
    pub fn raw_byte_offset(self, heap: &otter_gc::GcHeap) -> usize {
        heap.read_payload(self.handle, |body| body.byte_offset)
    }

    /// Element count. `0` when the backing buffer is detached.
    #[must_use]
    pub fn length(self, heap: &otter_gc::GcHeap) -> usize {
        let (buffer, off, len) = heap.read_payload(self.handle, |body| {
            (body.buffer, body.byte_offset, body.length)
        });
        if buffer.is_detached(heap) {
            return 0;
        }
        // Honour buffer shrinkage for resizable backing buffers: the
        // effective length clamps to whatever the backing buffer
        // currently holds at our offset.
        let bpe = self.cached_kind.bytes_per_element();
        let bytes_available = buffer.byte_length(heap).saturating_sub(off);
        let max_elems = bytes_available / bpe;
        len.min(max_elems)
    }

    /// Construction-time element count, ignoring detached state and
    /// buffer shrinkage.
    #[must_use]
    pub fn raw_length(self, heap: &otter_gc::GcHeap) -> usize {
        heap.read_payload(self.handle, |body| body.length)
    }

    /// Byte length (`length * bytes_per_element`).
    #[must_use]
    pub fn byte_length(self, heap: &otter_gc::GcHeap) -> usize {
        self.length(heap) * self.cached_kind.bytes_per_element()
    }

    /// Read element `index`. Returns `Value::Undefined` for an
    /// out-of-range read or a detached buffer per §10.4.5.13
    /// `IntegerIndexedElementGet`.
    pub fn get(
        self,
        heap: &mut otter_gc::GcHeap,
        index: usize,
    ) -> Result<Value, otter_gc::OutOfMemory> {
        if self.buffer(heap).is_detached(heap) || index >= self.length(heap) {
            return Ok(Value::undefined());
        }
        let bpe = self.cached_kind.bytes_per_element();
        let (buffer, off) = heap.read_payload(self.handle, |body| (body.buffer, body.byte_offset));
        let offset = off + index * bpe;
        let snapshot: Option<Vec<u8>> = buffer.with_bytes(heap, |bytes| {
            if offset + bpe <= bytes.len() {
                Some(bytes[offset..offset + bpe].to_vec())
            } else {
                None
            }
        });
        match snapshot {
            Some(b) => self.cached_kind.read(heap, &b, 0),
            None => Ok(Value::undefined()),
        }
    }

    /// Write `value` at element `index`. Out-of-range indices and
    /// detached buffers silently drop the write per §10.4.5.14
    /// `IntegerIndexedElementSet`.
    pub fn set(self, heap: &mut otter_gc::GcHeap, index: usize, value: &Value) {
        if self.buffer(heap).is_detached(heap) || index >= self.length(heap) {
            return;
        }
        let bpe = self.cached_kind.bytes_per_element();
        let (buffer, off) = heap.read_payload(self.handle, |body| (body.buffer, body.byte_offset));
        let offset = off + index * bpe;
        let kind = self.cached_kind;
        // Convert the Value to a raw byte snapshot first (BigInt
        // writes only need read access to the source body), then
        // commit under exclusive heap access.
        let mut staging = vec![0u8; bpe];
        kind.write(heap, &mut staging, 0, value);
        buffer.with_bytes_mut(heap, |bytes| {
            if offset + bpe <= bytes.len() {
                bytes[offset..offset + bpe].copy_from_slice(&staging);
            }
        });
    }

    /// Identity comparison via GC handle offset.
    #[must_use]
    pub fn ptr_eq(self, other: Self) -> bool {
        self.handle == other.handle
    }

    /// Backing-pointer for cycle / identity sets.
    #[must_use]
    pub fn identity_addr(self) -> *const () {
        self.handle.offset() as usize as *const ()
    }

    /// Visit the embedded GC handle slot during root tracing.
    pub fn trace_value_slots(&self, visitor: &mut otter_gc::raw::SlotVisitor<'_>) {
        let p = &self.handle as *const TypedArrayHandle as *mut otter_gc::raw::RawGc;
        visitor(p);
    }
}
