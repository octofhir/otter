//! ES2024 §25.4 The Atomics Object
//!
//! Implements the Atomics namespace — a non-constructor, non-callable object
//! with methods for atomic operations on SharedArrayBuffer-backed typed arrays.
//!
//! Phase 1 (single-threaded): All operations are trivially correct because
//! there is no concurrent access. `Atomics.wait` returns "not-equal" or
//! "timed-out"; `Atomics.notify` always returns 0.
//!
//! Spec: <https://tc39.es/ecma262/#sec-atomics-object>

use crate::builders::NamespaceBuilder;
use crate::descriptors::{
    NativeBindingDescriptor, NativeBindingTarget, NativeFunctionDescriptor, VmNativeCallError,
};
use crate::interpreter::RuntimeState;
use crate::object::{HeapValueKind, ObjectHandle, TypedArrayKind};
use crate::value::RegisterValue;

use super::{
    IntrinsicsError, VmIntrinsics,
    install::{IntrinsicInstallContext, IntrinsicInstaller, install_object_plan},
};

pub(super) static ATOMICS_INTRINSIC: AtomicsIntrinsic = AtomicsIntrinsic;

pub(super) struct AtomicsIntrinsic;

// ─── Installer ───────────────────────────────────────────────────────────────

impl IntrinsicInstaller for AtomicsIntrinsic {
    fn init(
        &self,
        intrinsics: &mut VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        let atomics = cx.alloc_intrinsic_object(Some(intrinsics.object_prototype()))?;

        // §25.4 Atomics[@@toStringTag] = "Atomics"
        let tag_prop = cx.property_names.intern("@@toStringTag");
        let tag_handle = cx.heap.alloc_string("Atomics");
        cx.heap.set_property(
            atomics,
            tag_prop,
            RegisterValue::from_object_handle(tag_handle.0),
        )?;

        // Install all methods.
        let plan = NamespaceBuilder::from_bindings(&atomics_bindings())
            .expect("Atomics namespace descriptors should normalize")
            .build();
        install_object_plan(atomics, &plan, intrinsics.function_prototype(), cx)?;

        intrinsics.set_namespace("Atomics", atomics);
        Ok(())
    }

    fn install_on_global(
        &self,
        intrinsics: &VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        let atomics = intrinsics
            .namespace("Atomics")
            .expect("Atomics namespace should be installed during init_core");
        cx.install_global_value(
            intrinsics,
            "Atomics",
            RegisterValue::from_object_handle(atomics.0),
        )
    }
}

// ─── Bindings ────────────────────────────────────────────────────────────────

fn method(
    name: &str,
    length: u16,
    callback: crate::descriptors::VmNativeFunction,
) -> NativeBindingDescriptor {
    NativeBindingDescriptor::new(
        NativeBindingTarget::Namespace,
        NativeFunctionDescriptor::method(name, length, callback),
    )
}

fn atomics_bindings() -> Vec<NativeBindingDescriptor> {
    vec![
        method("add", 3, atomics_add),
        method("and", 3, atomics_and),
        method("compareExchange", 4, atomics_compare_exchange),
        method("exchange", 3, atomics_exchange),
        method("isLockFree", 1, atomics_is_lock_free),
        method("load", 2, atomics_load),
        method("notify", 3, atomics_notify),
        method("or", 3, atomics_or),
        method("store", 3, atomics_store),
        method("sub", 3, atomics_sub),
        method("wait", 4, atomics_wait),
        method("xor", 3, atomics_xor),
    ]
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Valid integer typed array kinds for Atomics operations (§25.4.1.1 step 1).
/// Excludes Float32Array, Float64Array, and Uint8ClampedArray.
fn is_valid_integer_typed_array_kind(kind: TypedArrayKind) -> bool {
    matches!(
        kind,
        TypedArrayKind::Int8
            | TypedArrayKind::Uint8
            | TypedArrayKind::Int16
            | TypedArrayKind::Uint16
            | TypedArrayKind::Int32
            | TypedArrayKind::Uint32
            | TypedArrayKind::BigInt64
            | TypedArrayKind::BigUint64
    )
}

/// Allocates and returns a TypeError as `VmNativeCallError::Thrown`.
fn type_error(
    runtime: &mut RuntimeState,
    message: &str,
) -> Result<VmNativeCallError, VmNativeCallError> {
    let error = runtime.alloc_type_error(message).map_err(|error| {
        VmNativeCallError::Internal(format!("TypeError allocation failed: {error}").into())
    })?;
    Ok(VmNativeCallError::Thrown(
        RegisterValue::from_object_handle(error.0),
    ))
}

/// Allocates and returns a RangeError as `VmNativeCallError::Thrown`.
fn range_error(runtime: &mut RuntimeState, message: &str) -> VmNativeCallError {
    let error = runtime.alloc_range_error(message);
    match error {
        Ok(handle) => VmNativeCallError::Thrown(RegisterValue::from_object_handle(handle.0)),
        Err(e) => VmNativeCallError::Internal(format!("RangeError allocation failed: {e}").into()),
    }
}

/// §25.4.1.1 ValidateIntegerTypedArray ( typedArray, waitable )
/// <https://tc39.es/ecma262/#sec-validateintegertypedarray>
///
/// Returns `(typed_array_handle, kind)`.
fn validate_integer_typed_array(
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
    waitable: bool,
) -> Result<(ObjectHandle, TypedArrayKind), VmNativeCallError> {
    // 1. If waitable is not present, set waitable to false.
    // 2. Perform ? ValidateTypedArray(typedArray, unordered).
    let ta_val = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let Some(handle) = ta_val.as_object_handle().map(ObjectHandle) else {
        return Err(type_error(
            runtime,
            "Atomics: first argument must be a TypedArray",
        )?);
    };
    if !matches!(
        runtime.objects().kind(handle),
        Ok(HeapValueKind::TypedArray)
    ) {
        return Err(type_error(
            runtime,
            "Atomics: first argument must be a TypedArray",
        )?);
    }

    let kind = runtime
        .objects()
        .typed_array_kind(handle)
        .map_err(|_| type_error(runtime, "Atomics: invalid TypedArray").unwrap_or_else(|e| e))?;

    // 3. If waitable is true, then
    //    a. If typeName is not "Int32Array" or "BigInt64Array", throw TypeError.
    if waitable {
        if !matches!(kind, TypedArrayKind::Int32 | TypedArrayKind::BigInt64) {
            return Err(type_error(
                runtime,
                "Atomics.wait/notify requires Int32Array or BigInt64Array",
            )?);
        }
    } else {
        // 4. Otherwise, if IsValidIntegerTypedArray is false, throw TypeError.
        if !is_valid_integer_typed_array_kind(kind) {
            return Err(type_error(
                runtime,
                "Atomics: TypedArray must be an integer typed array (not Float or Uint8Clamped)",
            )?);
        }
    }

    Ok((handle, kind))
}

/// §25.4.1.2 ValidateAtomicAccess ( iieoRecord, requestIndex )
/// <https://tc39.es/ecma262/#sec-validateatomicaccess>
///
/// Returns the validated byte index offset into the buffer.
fn validate_atomic_access(
    args: &[RegisterValue],
    arg_index: usize,
    ta_handle: ObjectHandle,
    runtime: &mut RuntimeState,
) -> Result<usize, VmNativeCallError> {
    let idx_val = args
        .get(arg_index)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);

    // 1. Let length be iieoRecord.[[Object]].[[ArrayLength]].
    let array_length = runtime
        .objects()
        .typed_array_length(ta_handle)
        .map_err(|_| type_error(runtime, "Atomics: invalid TypedArray").unwrap_or_else(|e| e))?;

    // 2. Let accessIndex be ? ToIndex(requestIndex).
    let access_index = to_index(idx_val, runtime)?;

    // 3. Assert: accessIndex ≥ 0.
    // 4. If accessIndex ≥ length, throw a RangeError.
    if access_index >= array_length {
        return Err(range_error(runtime, "Atomics: index out of range"));
    }

    // 5. Return byte offset.
    let byte_offset = runtime
        .objects()
        .typed_array_byte_offset(ta_handle)
        .map_err(|_| type_error(runtime, "Atomics: invalid TypedArray").unwrap_or_else(|e| e))?;

    let kind = runtime
        .objects()
        .typed_array_kind(ta_handle)
        .map_err(|_| type_error(runtime, "Atomics: invalid TypedArray").unwrap_or_else(|e| e))?;

    Ok(byte_offset + access_index * kind.element_size())
}

/// §7.1.22 ToIndex — convert to a non-negative integer index.
/// <https://tc39.es/ecma262/#sec-toindex>
fn to_index(value: RegisterValue, runtime: &mut RuntimeState) -> Result<usize, VmNativeCallError> {
    if value == RegisterValue::undefined() {
        return Ok(0);
    }
    let n = runtime
        .js_to_number(value)
        .map_err(|e| VmNativeCallError::Internal(format!("ToIndex: {e}").into()))?;
    if n.is_nan() || n.is_infinite() || n < 0.0 || n != n.trunc() {
        return Err(range_error(runtime, "Invalid index"));
    }
    let index = n as usize;
    if index as f64 != n {
        return Err(range_error(runtime, "Invalid index"));
    }
    Ok(index)
}

/// Get the viewed buffer handle from a typed array.
fn get_buffer(
    ta_handle: ObjectHandle,
    runtime: &mut RuntimeState,
) -> Result<ObjectHandle, VmNativeCallError> {
    runtime
        .objects()
        .typed_array_buffer(ta_handle)
        .map_err(|_| type_error(runtime, "Atomics: invalid TypedArray").unwrap_or_else(|e| e))
}

/// Convert argument to numeric value for the given kind.
/// For BigInt kinds, the argument must be a BigInt.
/// For numeric kinds, the argument is coerced via ToNumber then truncated.
///
/// §25.4.1.3 AtomicReadModifyWrite ( typedArray, index, value, op )
/// <https://tc39.es/ecma262/#sec-atomicreadmodifywrite>
fn coerce_atomic_value(
    args: &[RegisterValue],
    arg_index: usize,
    kind: TypedArrayKind,
    runtime: &mut RuntimeState,
) -> Result<AtomicValue, VmNativeCallError> {
    let val = args
        .get(arg_index)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);

    if kind.is_bigint_kind() {
        // §25.4.1.3 step 6: If typedArray.[[ContentType]] is bigint, let v be ? ToBigInt(value).
        let Some(bigint_handle) = val.as_bigint_handle() else {
            return Err(type_error(
                runtime,
                "Atomics: BigInt typed arrays require BigInt values",
            )?);
        };
        let bigint_str = match runtime.objects().bigint_value(ObjectHandle(bigint_handle)) {
            Ok(Some(s)) => s.to_string(),
            _ => return Err(type_error(runtime, "Atomics: invalid BigInt")?),
        };

        match kind {
            TypedArrayKind::BigInt64 => {
                let n: i64 = bigint_str.parse().unwrap_or(0);
                Ok(AtomicValue::I64(n))
            }
            TypedArrayKind::BigUint64 => {
                let n: u64 = bigint_str.parse().unwrap_or(0);
                Ok(AtomicValue::U64(n))
            }
            _ => unreachable!(),
        }
    } else {
        // §25.4.1.3 step 7: Otherwise, let v be ? ToIntegerOrInfinity(value).
        let n = runtime
            .js_to_number(val)
            .map_err(|e| VmNativeCallError::Internal(format!("Atomics: ToNumber: {e}").into()))?;
        Ok(AtomicValue::F64(n))
    }
}

/// Internal representation of an atomic value (before/after operation).
#[derive(Debug, Clone, Copy)]
enum AtomicValue {
    F64(f64),
    I64(i64),
    U64(u64),
}

/// Read bytes from buffer at the given byte offset for the given kind, returning
/// the old value as `AtomicValue`.
fn read_atomic(
    buffer: ObjectHandle,
    byte_offset: usize,
    kind: TypedArrayKind,
    runtime: &RuntimeState,
) -> Result<AtomicValue, VmNativeCallError> {
    let data = runtime
        .objects()
        .array_buffer_or_shared_data(buffer)
        .map_err(|_| VmNativeCallError::Internal("Atomics: buffer access failed".into()))?;
    let elem_size = kind.element_size();
    if byte_offset + elem_size > data.len() {
        return Err(VmNativeCallError::Internal(
            "Atomics: byte offset out of range".into(),
        ));
    }
    let bytes = &data[byte_offset..byte_offset + elem_size];
    Ok(read_bytes(kind, bytes))
}

/// Write AtomicValue into buffer at the given byte offset for the given kind.
fn write_atomic(
    buffer: ObjectHandle,
    byte_offset: usize,
    kind: TypedArrayKind,
    value: AtomicValue,
    runtime: &mut RuntimeState,
) -> Result<(), VmNativeCallError> {
    let data = runtime
        .objects_mut()
        .array_buffer_or_shared_data_mut(buffer)
        .map_err(|_| VmNativeCallError::Internal("Atomics: buffer access failed".into()))?;
    let elem_size = kind.element_size();
    if byte_offset + elem_size > data.len() {
        return Err(VmNativeCallError::Internal(
            "Atomics: byte offset out of range".into(),
        ));
    }
    let bytes = &mut data[byte_offset..byte_offset + elem_size];
    write_bytes(kind, value, bytes);
    Ok(())
}

/// Read raw bytes into `AtomicValue` based on kind.
fn read_bytes(kind: TypedArrayKind, bytes: &[u8]) -> AtomicValue {
    match kind {
        TypedArrayKind::Int8 => AtomicValue::F64(f64::from(i8::from_ne_bytes([bytes[0]]))),
        TypedArrayKind::Uint8 => AtomicValue::F64(f64::from(bytes[0])),
        TypedArrayKind::Int16 => {
            AtomicValue::F64(f64::from(i16::from_le_bytes([bytes[0], bytes[1]])))
        }
        TypedArrayKind::Uint16 => {
            AtomicValue::F64(f64::from(u16::from_le_bytes([bytes[0], bytes[1]])))
        }
        TypedArrayKind::Int32 => AtomicValue::F64(f64::from(i32::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3],
        ]))),
        TypedArrayKind::Uint32 => AtomicValue::F64(f64::from(u32::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3],
        ]))),
        TypedArrayKind::BigInt64 => AtomicValue::I64(i64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ])),
        TypedArrayKind::BigUint64 => AtomicValue::U64(u64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ])),
        // Float32/Float64/Uint8Clamped excluded by validation
        _ => AtomicValue::F64(0.0),
    }
}

/// Write `AtomicValue` as raw bytes based on kind.
fn write_bytes(kind: TypedArrayKind, value: AtomicValue, bytes: &mut [u8]) {
    match (kind, value) {
        (TypedArrayKind::Int8, AtomicValue::F64(n)) => {
            bytes[0] = (n.trunc() as i64 as i8) as u8;
        }
        (TypedArrayKind::Uint8, AtomicValue::F64(n)) => {
            bytes[0] = n.trunc() as i64 as u8;
        }
        (TypedArrayKind::Int16, AtomicValue::F64(n)) => {
            bytes[..2].copy_from_slice(&(n.trunc() as i64 as i16).to_le_bytes());
        }
        (TypedArrayKind::Uint16, AtomicValue::F64(n)) => {
            bytes[..2].copy_from_slice(&(n.trunc() as i64 as u16).to_le_bytes());
        }
        (TypedArrayKind::Int32, AtomicValue::F64(n)) => {
            bytes[..4].copy_from_slice(&(n.trunc() as i64 as i32).to_le_bytes());
        }
        (TypedArrayKind::Uint32, AtomicValue::F64(n)) => {
            bytes[..4].copy_from_slice(&(n.trunc() as i64 as u32).to_le_bytes());
        }
        (TypedArrayKind::BigInt64, AtomicValue::I64(n)) => {
            bytes[..8].copy_from_slice(&n.to_le_bytes());
        }
        (TypedArrayKind::BigUint64, AtomicValue::U64(n)) => {
            bytes[..8].copy_from_slice(&n.to_le_bytes());
        }
        _ => {}
    }
}

/// Convert `AtomicValue` to `RegisterValue` for return to JS.
fn atomic_value_to_register(
    value: AtomicValue,
    _kind: TypedArrayKind,
    runtime: &mut RuntimeState,
) -> RegisterValue {
    match value {
        AtomicValue::F64(n) => RegisterValue::from_number(n),
        AtomicValue::I64(n) => {
            let handle = runtime.alloc_bigint(&n.to_string());
            RegisterValue::from_bigint_handle(handle.0)
        }
        AtomicValue::U64(n) => {
            let handle = runtime.alloc_bigint(&n.to_string());
            RegisterValue::from_bigint_handle(handle.0)
        }
    }
}

// ─── Read-Modify-Write core ──────────────────────────────────────────────────

/// Generic read-modify-write for all Atomics RMW operations.
///
/// §25.4.1.3 AtomicReadModifyWrite ( typedArray, index, value, op )
/// <https://tc39.es/ecma262/#sec-atomicreadmodifywrite>
fn atomic_rmw(
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
    op: fn(AtomicValue, AtomicValue) -> AtomicValue,
) -> Result<RegisterValue, VmNativeCallError> {
    // 1. Let (typedArray, kind) = ValidateIntegerTypedArray(args[0]).
    let (ta, kind) = validate_integer_typed_array(args, runtime, false)?;
    // 2. Let byteIndex = ValidateAtomicAccess(typedArray, args[1]).
    let byte_idx = validate_atomic_access(args, 1, ta, runtime)?;
    // 3. Let value = coerce args[2].
    let new_val = coerce_atomic_value(args, 2, kind, runtime)?;
    // 4. Get buffer.
    let buffer = get_buffer(ta, runtime)?;
    // 5. Read old value.
    let old_val = read_atomic(buffer, byte_idx, kind, runtime)?;
    // 6. Compute new value.
    let result = op(old_val, new_val);
    // 7. Write new value.
    write_atomic(buffer, byte_idx, kind, result, runtime)?;
    // 8. Return old value.
    Ok(atomic_value_to_register(old_val, kind, runtime))
}

// ─── RMW operation implementations ──────────────────────────────────────────

fn rmw_add(old: AtomicValue, new: AtomicValue) -> AtomicValue {
    match (old, new) {
        (AtomicValue::F64(a), AtomicValue::F64(b)) => {
            AtomicValue::F64((a as i64).wrapping_add(b as i64) as f64)
        }
        (AtomicValue::I64(a), AtomicValue::I64(b)) => AtomicValue::I64(a.wrapping_add(b)),
        (AtomicValue::U64(a), AtomicValue::U64(b)) => AtomicValue::U64(a.wrapping_add(b)),
        _ => old,
    }
}

fn rmw_sub(old: AtomicValue, new: AtomicValue) -> AtomicValue {
    match (old, new) {
        (AtomicValue::F64(a), AtomicValue::F64(b)) => {
            AtomicValue::F64((a as i64).wrapping_sub(b as i64) as f64)
        }
        (AtomicValue::I64(a), AtomicValue::I64(b)) => AtomicValue::I64(a.wrapping_sub(b)),
        (AtomicValue::U64(a), AtomicValue::U64(b)) => AtomicValue::U64(a.wrapping_sub(b)),
        _ => old,
    }
}

fn rmw_and(old: AtomicValue, new: AtomicValue) -> AtomicValue {
    match (old, new) {
        (AtomicValue::F64(a), AtomicValue::F64(b)) => {
            AtomicValue::F64(((a as i64) & (b as i64)) as f64)
        }
        (AtomicValue::I64(a), AtomicValue::I64(b)) => AtomicValue::I64(a & b),
        (AtomicValue::U64(a), AtomicValue::U64(b)) => AtomicValue::U64(a & b),
        _ => old,
    }
}

fn rmw_or(old: AtomicValue, new: AtomicValue) -> AtomicValue {
    match (old, new) {
        (AtomicValue::F64(a), AtomicValue::F64(b)) => {
            AtomicValue::F64(((a as i64) | (b as i64)) as f64)
        }
        (AtomicValue::I64(a), AtomicValue::I64(b)) => AtomicValue::I64(a | b),
        (AtomicValue::U64(a), AtomicValue::U64(b)) => AtomicValue::U64(a | b),
        _ => old,
    }
}

fn rmw_xor(old: AtomicValue, new: AtomicValue) -> AtomicValue {
    match (old, new) {
        (AtomicValue::F64(a), AtomicValue::F64(b)) => {
            AtomicValue::F64(((a as i64) ^ (b as i64)) as f64)
        }
        (AtomicValue::I64(a), AtomicValue::I64(b)) => AtomicValue::I64(a ^ b),
        (AtomicValue::U64(a), AtomicValue::U64(b)) => AtomicValue::U64(a ^ b),
        _ => old,
    }
}

fn rmw_exchange(_old: AtomicValue, new: AtomicValue) -> AtomicValue {
    new
}

// ─── §25.4.1 Atomics.add ( typedArray, index, value ) ───────────────────────

fn atomics_add(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    atomic_rmw(args, runtime, rmw_add)
}

// ─── §25.4.2 Atomics.and ( typedArray, index, value ) ───────────────────────

fn atomics_and(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    atomic_rmw(args, runtime, rmw_and)
}

// ─── §25.4.4 Atomics.compareExchange ( typedArray, index, expected, replacement ) ──

/// §25.4.4 Atomics.compareExchange
/// <https://tc39.es/ecma262/#sec-atomics.compareexchange>
fn atomics_compare_exchange(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let (ta, kind) = validate_integer_typed_array(args, runtime, false)?;
    let byte_idx = validate_atomic_access(args, 1, ta, runtime)?;
    let expected = coerce_atomic_value(args, 2, kind, runtime)?;
    let replacement = coerce_atomic_value(args, 3, kind, runtime)?;
    let buffer = get_buffer(ta, runtime)?;

    let old_val = read_atomic(buffer, byte_idx, kind, runtime)?;

    // Compare — if equal, write replacement; otherwise leave unchanged.
    let equal = match (old_val, expected) {
        (AtomicValue::F64(a), AtomicValue::F64(b)) => {
            // Compare as raw bytes of the element type (§25.4.4 step 14).
            atomic_bytes_equal(kind, a, b)
        }
        (AtomicValue::I64(a), AtomicValue::I64(b)) => a == b,
        (AtomicValue::U64(a), AtomicValue::U64(b)) => a == b,
        _ => false,
    };

    if equal {
        write_atomic(buffer, byte_idx, kind, replacement, runtime)?;
    }

    Ok(atomic_value_to_register(old_val, kind, runtime))
}

/// Compare two f64 values as if they were stored in the typed array's element type.
/// This handles the integer truncation that happens during coercion.
fn atomic_bytes_equal(kind: TypedArrayKind, a: f64, b: f64) -> bool {
    match kind {
        TypedArrayKind::Int8 => (a as i64 as i8) == (b as i64 as i8),
        TypedArrayKind::Uint8 => (a as i64 as u8) == (b as i64 as u8),
        TypedArrayKind::Int16 => (a as i64 as i16) == (b as i64 as i16),
        TypedArrayKind::Uint16 => (a as i64 as u16) == (b as i64 as u16),
        TypedArrayKind::Int32 => (a as i64 as i32) == (b as i64 as i32),
        TypedArrayKind::Uint32 => (a as i64 as u32) == (b as i64 as u32),
        _ => false,
    }
}

// ─── §25.4.5 Atomics.exchange ( typedArray, index, value ) ──────────────────

fn atomics_exchange(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    atomic_rmw(args, runtime, rmw_exchange)
}

// ─── §25.4.6 Atomics.isLockFree ( size ) ────────────────────────────────────

/// §25.4.6 Atomics.isLockFree ( size )
/// <https://tc39.es/ecma262/#sec-atomics.islockfree>
///
/// Returns true if atomic operations on size-byte values are lock-free.
/// On x86-64 and aarch64, sizes 1, 2, 4, 8 are all lock-free.
fn atomics_is_lock_free(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let val = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let n = runtime
        .js_to_number(val)
        .map_err(|e| VmNativeCallError::Internal(format!("isLockFree: {e}").into()))?;

    let lock_free = matches!(n as u64, 1 | 2 | 4 | 8);
    Ok(RegisterValue::from_bool(lock_free))
}

// ─── §25.4.7 Atomics.load ( typedArray, index ) ─────────────────────────────

/// §25.4.7 Atomics.load ( typedArray, index )
/// <https://tc39.es/ecma262/#sec-atomics.load>
fn atomics_load(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let (ta, kind) = validate_integer_typed_array(args, runtime, false)?;
    let byte_idx = validate_atomic_access(args, 1, ta, runtime)?;
    let buffer = get_buffer(ta, runtime)?;
    let value = read_atomic(buffer, byte_idx, kind, runtime)?;
    Ok(atomic_value_to_register(value, kind, runtime))
}

// ─── §25.4.8 Atomics.notify ( typedArray, index, count ) ────────────────────

/// §25.4.8 Atomics.notify ( typedArray, index, count )
/// <https://tc39.es/ecma262/#sec-atomics.notify>
///
/// Single-threaded: always returns 0 (no waiters to wake).
fn atomics_notify(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    // 1. Let (typedArray, kind) = ValidateIntegerTypedArray(args[0], waitable: true).
    let (ta, _kind) = validate_integer_typed_array(args, runtime, true)?;
    // 2. Validate index.
    let _byte_idx = validate_atomic_access(args, 1, ta, runtime)?;
    // 3. If count is undefined, let c be +∞.
    //    Otherwise, let intCount be ? ToIntegerOrInfinity(count), c be max(intCount, 0).
    // In single-threaded mode, we just validate and return 0.
    let _count_val = args
        .get(2)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    // Don't need to actually use count — no waiters exist.

    // 4. Single-threaded: return +0𝔽 (no waiters).
    Ok(RegisterValue::from_i32(0))
}

// ─── §25.4.9 Atomics.or ( typedArray, index, value ) ────────────────────────

fn atomics_or(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    atomic_rmw(args, runtime, rmw_or)
}

// ─── §25.4.11 Atomics.store ( typedArray, index, value ) ────────────────────

/// §25.4.11 Atomics.store ( typedArray, index, value )
/// <https://tc39.es/ecma262/#sec-atomics.store>
///
/// Returns the coerced value (not the value read from the array).
fn atomics_store(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let (ta, kind) = validate_integer_typed_array(args, runtime, false)?;
    let byte_idx = validate_atomic_access(args, 1, ta, runtime)?;
    let value = coerce_atomic_value(args, 2, kind, runtime)?;
    let buffer = get_buffer(ta, runtime)?;
    write_atomic(buffer, byte_idx, kind, value, runtime)?;

    // §25.4.11 step 8: Return v (the coerced value, not the value read).
    Ok(atomic_value_to_register(value, kind, runtime))
}

// ─── §25.4.12 Atomics.sub ( typedArray, index, value ) ──────────────────────

fn atomics_sub(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    atomic_rmw(args, runtime, rmw_sub)
}

// ─── §25.4.13 Atomics.wait ( typedArray, index, value [, timeout] ) ─────────

/// §25.4.13 Atomics.wait ( typedArray, index, value [, timeout] )
/// <https://tc39.es/ecma262/#sec-atomics.wait>
///
/// Single-threaded: Can never actually wait (would deadlock).
/// Returns "not-equal" if current value ≠ expected, "timed-out" otherwise.
fn atomics_wait(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    // 1. ValidateIntegerTypedArray(typedArray, waitable: true).
    let (ta, kind) = validate_integer_typed_array(args, runtime, true)?;

    // 2. ValidateAtomicAccess.
    let byte_idx = validate_atomic_access(args, 1, ta, runtime)?;

    // 3. Coerce expected value.
    let expected = coerce_atomic_value(args, 2, kind, runtime)?;

    // 4. Get timeout (default: +Infinity).
    let timeout_val = args
        .get(3)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let timeout = if timeout_val == RegisterValue::undefined() {
        f64::INFINITY
    } else {
        runtime
            .js_to_number(timeout_val)
            .map_err(|e| VmNativeCallError::Internal(format!("Atomics.wait: {e}").into()))?
    };

    // 5. If timeout is NaN, set to +Infinity.
    let _timeout = if timeout.is_nan() {
        f64::INFINITY
    } else {
        timeout
    };

    // 6. Read current value.
    let buffer = get_buffer(ta, runtime)?;
    let current = read_atomic(buffer, byte_idx, kind, runtime)?;

    // 7. Compare current with expected.
    let equal = match (current, expected) {
        (AtomicValue::F64(a), AtomicValue::F64(b)) => atomic_bytes_equal(kind, a, b),
        (AtomicValue::I64(a), AtomicValue::I64(b)) => a == b,
        (AtomicValue::U64(a), AtomicValue::U64(b)) => a == b,
        _ => false,
    };

    if !equal {
        // §25.4.13 step 14: If they are not equal, return "not-equal".
        let s = runtime.alloc_string("not-equal");
        return Ok(RegisterValue::from_object_handle(s.0));
    }

    // Single-threaded: we can never be woken by another thread.
    // If timeout ≤ 0, return "timed-out" immediately.
    // If timeout > 0, we would deadlock — so also return "timed-out".
    let s = runtime.alloc_string("timed-out");
    Ok(RegisterValue::from_object_handle(s.0))
}

// ─── §25.4.14 Atomics.xor ( typedArray, index, value ) ──────────────────────

fn atomics_xor(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    atomic_rmw(args, runtime, rmw_xor)
}
