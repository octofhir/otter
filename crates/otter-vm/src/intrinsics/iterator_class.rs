//! Iterator protocol intrinsics.
//!
//! Spec references (ECMAScript 2024 / ES15):
//! - %IteratorPrototype%:       <https://tc39.es/ecma262/#sec-%iteratorprototype%-object>
//! - %ArrayIteratorPrototype%:  <https://tc39.es/ecma262/#sec-%arrayiteratorprototype%-object>
//! - %StringIteratorPrototype%: <https://tc39.es/ecma262/#sec-%stringiteratorprototype%-object>
//! - %MapIteratorPrototype%:    <https://tc39.es/ecma262/#sec-%mapiteratorprototype%-object>
//! - %SetIteratorPrototype%:    <https://tc39.es/ecma262/#sec-%setiteratorprototype%-object>
//! - CreateIterResultObject:    <https://tc39.es/ecma262/#sec-createiterresultobject>

use crate::descriptors::{NativeFunctionDescriptor, VmNativeCallError};
use crate::object::{
    ArrayIteratorKind, HeapValueKind, MapIteratorKind, ObjectHandle, PropertyAttributes,
    PropertyValue, SetIteratorKind,
};
use crate::value::RegisterValue;

use super::install::{IntrinsicInstallContext, IntrinsicInstaller, install_function_length_name};
use super::{IntrinsicsError, VmIntrinsics, WellKnownSymbol};

pub(super) static ITERATOR_INTRINSIC: IteratorIntrinsic = IteratorIntrinsic;

pub(super) struct IteratorIntrinsic;

impl IntrinsicInstaller for IteratorIntrinsic {
    fn init(
        &self,
        intrinsics: &mut VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        // ─── §27.1.2 %IteratorPrototype% ───────────────────────────────
        // %IteratorPrototype%[@@iterator]() — returns `this`.
        install_symbol_method(
            intrinsics.iterator_prototype(),
            WellKnownSymbol::Iterator,
            "[Symbol.iterator]",
            0,
            iterator_prototype_symbol_iterator,
            intrinsics.function_prototype(),
            cx,
        )?;
        // @@toStringTag = "Iterator" (not in ES2024, but matches V8/SpiderMonkey)

        // ─── §23.1.5.1 %ArrayIteratorPrototype% ────────────────────────
        install_next_method(
            intrinsics.array_iterator_prototype(),
            array_iterator_next,
            intrinsics.function_prototype(),
            cx,
        )?;
        install_to_string_tag(intrinsics.array_iterator_prototype(), "Array Iterator", cx)?;

        // ─── §22.1.5.1 %StringIteratorPrototype% ───────────────────────
        install_next_method(
            intrinsics.string_iterator_prototype(),
            string_iterator_next,
            intrinsics.function_prototype(),
            cx,
        )?;
        install_to_string_tag(
            intrinsics.string_iterator_prototype(),
            "String Iterator",
            cx,
        )?;

        // ─── §24.1.5.1 %MapIteratorPrototype% ──────────────────────────
        install_next_method(
            intrinsics.map_iterator_prototype(),
            map_iterator_next,
            intrinsics.function_prototype(),
            cx,
        )?;
        install_to_string_tag(intrinsics.map_iterator_prototype(), "Map Iterator", cx)?;

        // ─── §24.2.5.1 %SetIteratorPrototype% ──────────────────────────
        install_next_method(
            intrinsics.set_iterator_prototype(),
            set_iterator_next,
            intrinsics.function_prototype(),
            cx,
        )?;
        install_to_string_tag(intrinsics.set_iterator_prototype(), "Set Iterator", cx)?;

        Ok(())
    }

    fn install_on_global(
        &self,
        _intrinsics: &VmIntrinsics,
        _cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        // Iterator prototypes are not directly exposed as globals.
        Ok(())
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Helpers
// ═══════════════════════════════════════════════════════════════════════════

type NativeFn = fn(
    &RegisterValue,
    &[RegisterValue],
    &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError>;

/// Installs a `.next()` method on a prototype object.
fn install_next_method(
    prototype: ObjectHandle,
    f: NativeFn,
    function_prototype: ObjectHandle,
    cx: &mut IntrinsicInstallContext<'_>,
) -> Result<(), IntrinsicsError> {
    let desc = NativeFunctionDescriptor::method("next", 0, f);
    let host_fn = cx.native_functions.register(desc);
    let handle = cx.alloc_intrinsic_host_function(host_fn, function_prototype)?;
    install_function_length_name(handle, 0, "next", cx)?;
    let prop = cx.property_names.intern("next");
    cx.heap.define_own_property(
        prototype,
        prop,
        PropertyValue::data_with_attrs(
            RegisterValue::from_object_handle(handle.0),
            PropertyAttributes::builtin_method(),
        ),
    )?;
    Ok(())
}

/// Installs a `[@@iterator]() { return this; }` symbol method.
fn install_symbol_method(
    target: ObjectHandle,
    symbol: WellKnownSymbol,
    js_name: &str,
    arity: u16,
    f: NativeFn,
    function_prototype: ObjectHandle,
    cx: &mut IntrinsicInstallContext<'_>,
) -> Result<(), IntrinsicsError> {
    let desc = NativeFunctionDescriptor::method(js_name, arity, f);
    let host_fn = cx.native_functions.register(desc);
    let handle = cx.alloc_intrinsic_host_function(host_fn, function_prototype)?;
    install_function_length_name(handle, arity, js_name, cx)?;
    let sym_prop = cx.property_names.intern_symbol(symbol.stable_id());
    cx.heap.define_own_property(
        target,
        sym_prop,
        PropertyValue::data_with_attrs(
            RegisterValue::from_object_handle(handle.0),
            PropertyAttributes::builtin_method(),
        ),
    )?;
    Ok(())
}

/// Installs `@@toStringTag` as a non-writable, non-enumerable, configurable string.
/// ES2024 §23.1.5.2.2, §22.1.5.2.2, §24.1.5.2.2, §24.2.5.2.2.
fn install_to_string_tag(
    target: ObjectHandle,
    tag: &str,
    cx: &mut IntrinsicInstallContext<'_>,
) -> Result<(), IntrinsicsError> {
    let sym_tag = cx
        .property_names
        .intern_symbol(WellKnownSymbol::ToStringTag.stable_id());
    let tag_str = cx.heap.alloc_string(tag);
    cx.heap.define_own_property(
        target,
        sym_tag,
        PropertyValue::data_with_attrs(
            RegisterValue::from_object_handle(tag_str.0),
            // {W:false, E:false, C:true} per spec
            PropertyAttributes::from_flags(false, false, true),
        ),
    )?;
    Ok(())
}

/// ES2024 §7.4.14 CreateIterResultObject(value, done).
/// Spec: <https://tc39.es/ecma262/#sec-createiterresultobject>
///
/// Creates a plain object `{ value, done }` used as the return value of
/// iterator `.next()` calls.
pub(crate) fn create_iter_result_object(
    value: RegisterValue,
    done: bool,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let obj = runtime.alloc_object();
    let value_prop = runtime.intern_property_name("value");
    let done_prop = runtime.intern_property_name("done");
    runtime
        .objects_mut()
        .set_property(obj, value_prop, value)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    runtime
        .objects_mut()
        .set_property(obj, done_prop, RegisterValue::from_bool(done))
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    Ok(RegisterValue::from_object_handle(obj.0))
}

// ═══════════════════════════════════════════════════════════════════════════
//  §27.1.2 %IteratorPrototype%[@@iterator]()
// ═══════════════════════════════════════════════════════════════════════════

/// %IteratorPrototype% \[ @@iterator \] ()
/// Spec: <https://tc39.es/ecma262/#sec-%iteratorprototype%-@@iterator>
/// Returns `this`.
fn iterator_prototype_symbol_iterator(
    this: &RegisterValue,
    _args: &[RegisterValue],
    _runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    Ok(*this)
}

// ═══════════════════════════════════════════════════════════════════════════
//  §23.1.5.1 %ArrayIteratorPrototype%.next()
// ═══════════════════════════════════════════════════════════════════════════

/// %ArrayIteratorPrototype%.next()
/// Spec: <https://tc39.es/ecma262/#sec-%arrayiteratorprototype%.next>
fn array_iterator_next(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = this
        .as_object_handle()
        .map(ObjectHandle)
        .ok_or_else(|| VmNativeCallError::Internal("next requires an iterator receiver".into()))?;

    if runtime.objects().kind(handle) != Ok(HeapValueKind::Iterator) {
        return Err(VmNativeCallError::Internal(
            "next called on non-ArrayIterator".into(),
        ));
    }

    // Read cursor state directly.
    let cursor = runtime
        .objects()
        .iterator_cursor(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    let kind = runtime
        .objects()
        .array_iterator_kind(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;

    if cursor.closed() {
        return create_iter_result_object(RegisterValue::undefined(), true, runtime);
    }

    let iterable = cursor.iterable();
    let index = cursor.next_index();

    // Check array bounds.
    let length = runtime
        .objects()
        .array_length(iterable)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?
        .unwrap_or(0);

    if index >= length {
        // Close the iterator.
        runtime
            .objects_mut()
            .advance_iterator_cursor(handle, true)
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
        return create_iter_result_object(RegisterValue::undefined(), true, runtime);
    }

    // Read the element value (needed for values and entries kinds).
    let elem = runtime
        .objects_mut()
        .get_index(iterable, index)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?
        .unwrap_or(RegisterValue::undefined());

    // Advance cursor (increments next_index by 1).
    runtime
        .objects_mut()
        .advance_iterator_cursor(handle, false)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;

    match kind {
        ArrayIteratorKind::Values => create_iter_result_object(elem, false, runtime),
        ArrayIteratorKind::Keys => {
            create_iter_result_object(RegisterValue::from_i32(index as i32), false, runtime)
        }
        ArrayIteratorKind::Entries => {
            let pair =
                runtime.alloc_array_with_elements(&[RegisterValue::from_i32(index as i32), elem]);
            create_iter_result_object(RegisterValue::from_object_handle(pair.0), false, runtime)
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  §22.1.5.1 %StringIteratorPrototype%.next()
// ═══════════════════════════════════════════════════════════════════════════

/// %StringIteratorPrototype%.next()
/// Spec: <https://tc39.es/ecma262/#sec-%stringiteratorprototype%.next>
fn string_iterator_next(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = this
        .as_object_handle()
        .map(ObjectHandle)
        .ok_or_else(|| VmNativeCallError::Internal("next requires an iterator receiver".into()))?;

    if runtime.objects().kind(handle) != Ok(HeapValueKind::Iterator) {
        return Err(VmNativeCallError::Internal(
            "next called on non-StringIterator".into(),
        ));
    }

    let step = runtime
        .objects_mut()
        .iterator_next(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;

    if step.is_done() {
        return create_iter_result_object(RegisterValue::undefined(), true, runtime);
    }

    create_iter_result_object(step.value(), false, runtime)
}

// ═══════════════════════════════════════════════════════════════════════════
//  §24.1.5.1 %MapIteratorPrototype%.next()
// ═══════════════════════════════════════════════════════════════════════════

/// %MapIteratorPrototype%.next()
/// Spec: <https://tc39.es/ecma262/#sec-%mapiteratorprototype%.next>
fn map_iterator_next(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = this
        .as_object_handle()
        .map(ObjectHandle)
        .ok_or_else(|| VmNativeCallError::Internal("next requires an iterator receiver".into()))?;

    if runtime.objects().kind(handle) != Ok(HeapValueKind::MapIterator) {
        return Err(VmNativeCallError::Internal(
            "next called on non-MapIterator".into(),
        ));
    }

    // Read cursor state and advance.
    let (iterable, next_index, closed, kind) = runtime
        .objects()
        .map_iterator_state(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;

    if closed {
        return create_iter_result_object(RegisterValue::undefined(), true, runtime);
    }

    // Walk the Map entries, skipping deleted (None) slots.
    // Uses raw entries to preserve index positions for lazy iteration.
    let entries = runtime
        .objects()
        .map_entries_raw(iterable)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;

    let mut idx = next_index;
    while idx < entries.len() {
        if let Some((key, value)) = &entries[idx] {
            let key = *key;
            let value = *value;
            // Advance cursor past this entry.
            runtime
                .objects_mut()
                .set_map_iterator_index(handle, idx + 1)
                .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;

            let result_value = match kind {
                MapIteratorKind::Keys => key,
                MapIteratorKind::Values => value,
                MapIteratorKind::Entries => {
                    let pair = runtime.alloc_array_with_elements(&[key, value]);
                    RegisterValue::from_object_handle(pair.0)
                }
            };
            return create_iter_result_object(result_value, false, runtime);
        }
        idx += 1;
    }

    // Exhausted — close the iterator.
    runtime
        .objects_mut()
        .iterator_close(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    // Also advance past end so re-calls stay done.
    runtime
        .objects_mut()
        .set_map_iterator_index(handle, idx)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    create_iter_result_object(RegisterValue::undefined(), true, runtime)
}

// ═══════════════════════════════════════════════════════════════════════════
//  §24.2.5.1 %SetIteratorPrototype%.next()
// ═══════════════════════════════════════════════════════════════════════════

/// %SetIteratorPrototype%.next()
/// Spec: <https://tc39.es/ecma262/#sec-%setiteratorprototype%.next>
fn set_iterator_next(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = this
        .as_object_handle()
        .map(ObjectHandle)
        .ok_or_else(|| VmNativeCallError::Internal("next requires an iterator receiver".into()))?;

    if runtime.objects().kind(handle) != Ok(HeapValueKind::SetIterator) {
        return Err(VmNativeCallError::Internal(
            "next called on non-SetIterator".into(),
        ));
    }

    let (iterable, next_index, closed, kind) = runtime
        .objects()
        .set_iterator_state(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;

    if closed {
        return create_iter_result_object(RegisterValue::undefined(), true, runtime);
    }

    let entries = runtime
        .objects()
        .set_entries(iterable)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;

    let mut idx = next_index;
    while idx < entries.len() {
        if let Some(value) = &entries[idx] {
            let value = *value;
            runtime
                .objects_mut()
                .set_set_iterator_index(handle, idx + 1)
                .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;

            let result_value = match kind {
                SetIteratorKind::Values => value,
                SetIteratorKind::Entries => {
                    let pair = runtime.alloc_array_with_elements(&[value, value]);
                    RegisterValue::from_object_handle(pair.0)
                }
            };
            return create_iter_result_object(result_value, false, runtime);
        }
        idx += 1;
    }

    runtime
        .objects_mut()
        .iterator_close(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    runtime
        .objects_mut()
        .set_set_iterator_index(handle, idx)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    create_iter_result_object(RegisterValue::undefined(), true, runtime)
}
