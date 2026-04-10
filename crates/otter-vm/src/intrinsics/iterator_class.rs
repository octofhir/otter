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

const ITERATOR_INTERRUPT_POLL_INTERVAL: usize = 4096;

fn check_iterator_interrupt(
    runtime: &crate::interpreter::RuntimeState,
    index: usize,
) -> Result<(), VmNativeCallError> {
    if index.is_multiple_of(ITERATOR_INTERRUPT_POLL_INTERVAL) {
        runtime.check_interrupt()?;
    }
    Ok(())
}

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

        // ─── §27.1.4 %AsyncIteratorPrototype% ──────────────────────────
        // %AsyncIteratorPrototype%[@@asyncIterator]() — returns `this`.
        install_symbol_method(
            intrinsics.async_iterator_prototype(),
            WellKnownSymbol::AsyncIterator,
            "[Symbol.asyncIterator]",
            0,
            async_iterator_prototype_symbol_async_iterator,
            intrinsics.function_prototype(),
            cx,
        )?;

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

        // ─── §27.1.4 Iterator Helper methods on %IteratorPrototype% ────
        // ES2025 Iterator Helpers: consuming methods
        install_proto_method(
            intrinsics.iterator_prototype(),
            "toArray",
            0,
            iterator_to_array,
            intrinsics.function_prototype(),
            cx,
        )?;
        install_proto_method(
            intrinsics.iterator_prototype(),
            "forEach",
            1,
            iterator_for_each,
            intrinsics.function_prototype(),
            cx,
        )?;
        install_proto_method(
            intrinsics.iterator_prototype(),
            "some",
            1,
            iterator_some,
            intrinsics.function_prototype(),
            cx,
        )?;
        install_proto_method(
            intrinsics.iterator_prototype(),
            "every",
            1,
            iterator_every,
            intrinsics.function_prototype(),
            cx,
        )?;
        install_proto_method(
            intrinsics.iterator_prototype(),
            "find",
            1,
            iterator_find,
            intrinsics.function_prototype(),
            cx,
        )?;
        install_proto_method(
            intrinsics.iterator_prototype(),
            "reduce",
            1,
            iterator_reduce,
            intrinsics.function_prototype(),
            cx,
        )?;
        install_proto_method(
            intrinsics.iterator_prototype(),
            "map",
            1,
            iterator_map,
            intrinsics.function_prototype(),
            cx,
        )?;
        install_proto_method(
            intrinsics.iterator_prototype(),
            "filter",
            1,
            iterator_filter,
            intrinsics.function_prototype(),
            cx,
        )?;
        install_proto_method(
            intrinsics.iterator_prototype(),
            "take",
            1,
            iterator_take,
            intrinsics.function_prototype(),
            cx,
        )?;
        install_proto_method(
            intrinsics.iterator_prototype(),
            "drop",
            1,
            iterator_drop,
            intrinsics.function_prototype(),
            cx,
        )?;
        install_proto_method(
            intrinsics.iterator_prototype(),
            "flatMap",
            1,
            iterator_flat_map,
            intrinsics.function_prototype(),
            cx,
        )?;

        // ─── Iterator.from(o) — §27.1.2.1 ─────────────────────────────
        // Installed on global `Iterator` constructor.
        // We need an Iterator constructor for this. Create a minimal one.
        let iter_ctor_desc =
            NativeFunctionDescriptor::constructor("Iterator", 0, iterator_constructor);
        let iter_ctor_id = cx.native_functions.register(iter_ctor_desc);
        let iter_ctor =
            cx.alloc_intrinsic_host_function(iter_ctor_id, intrinsics.function_prototype())?;

        // Set Iterator.prototype = %IteratorPrototype%
        let prototype_prop = cx.property_names.intern("prototype");
        cx.heap.define_own_property(
            iter_ctor,
            prototype_prop,
            PropertyValue::data_with_attrs(
                RegisterValue::from_object_handle(intrinsics.iterator_prototype().0),
                PropertyAttributes::from_flags(false, false, false),
            ),
        )?;

        // Install Iterator.from
        let from_desc = NativeFunctionDescriptor::method("from", 1, iterator_from);
        let from_id = cx.native_functions.register(from_desc);
        let from_handle =
            cx.alloc_intrinsic_host_function(from_id, intrinsics.function_prototype())?;
        let from_prop = cx.property_names.intern("from");
        cx.heap.define_own_property(
            iter_ctor,
            from_prop,
            PropertyValue::data_with_attrs(
                RegisterValue::from_object_handle(from_handle.0),
                PropertyAttributes::builtin_method(),
            ),
        )?;

        // Store Iterator constructor for install_on_global
        intrinsics.iterator_constructor = Some(iter_ctor);

        Ok(())
    }

    fn install_on_global(
        &self,
        intrinsics: &VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        // Expose `Iterator` as a global (ES2025 §27.1.2).
        if let Some(ctor) = intrinsics.iterator_constructor {
            cx.install_global_value(
                intrinsics,
                "Iterator",
                RegisterValue::from_object_handle(ctor.0),
            )?;
        }
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

/// Installs a named method on a prototype object.
fn install_proto_method(
    prototype: ObjectHandle,
    name: &str,
    arity: u16,
    f: NativeFn,
    function_prototype: ObjectHandle,
    cx: &mut IntrinsicInstallContext<'_>,
) -> Result<(), IntrinsicsError> {
    let desc = NativeFunctionDescriptor::method(name, arity, f);
    let host_fn = cx.native_functions.register(desc);
    let handle = cx.alloc_intrinsic_host_function(host_fn, function_prototype)?;
    install_function_length_name(handle, arity, name, cx)?;
    let prop = cx.property_names.intern(name);
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

/// %AsyncIteratorPrototype% \[ @@asyncIterator \] ()
/// Spec: <https://tc39.es/ecma262/#sec-asynciteratorprototype-asynciterator>
/// Returns `this`.
fn async_iterator_prototype_symbol_async_iterator(
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
        check_iterator_interrupt(runtime, idx)?;
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
        check_iterator_interrupt(runtime, idx)?;
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

// ═══════════════════════════════════════════════════════════════════════════
//  Iterator Helper Utilities
// ═══════════════════════════════════════════════════════════════════════════

/// Calls `.next()` on an iterator and returns `(value, done)`.
/// Spec: <https://tc39.es/ecma262/#sec-iteratornext>
fn iter_step(
    iterator: ObjectHandle,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<(RegisterValue, bool), VmNativeCallError> {
    let next_prop = runtime.intern_property_name("next");
    let next_fn = runtime
        .property_lookup(iterator, next_prop)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?
        .and_then(|l| {
            if let PropertyValue::Data { value, .. } = l.value() {
                value.as_object_handle().map(ObjectHandle)
            } else {
                None
            }
        })
        .ok_or_else(|| VmNativeCallError::Internal("iterator.next is not a function".into()))?;

    let result =
        runtime.call_callable(next_fn, RegisterValue::from_object_handle(iterator.0), &[])?;

    let result_handle = result
        .as_object_handle()
        .map(ObjectHandle)
        .ok_or_else(|| VmNativeCallError::Internal("iterator result is not an object".into()))?;

    let done_prop = runtime.intern_property_name("done");
    let done = runtime
        .property_lookup(result_handle, done_prop)
        .ok()
        .flatten()
        .and_then(|l| {
            if let PropertyValue::Data { value, .. } = l.value() {
                value.as_bool()
            } else {
                None
            }
        })
        .unwrap_or(false);

    let value_prop = runtime.intern_property_name("value");
    let value = runtime
        .property_lookup(result_handle, value_prop)
        .ok()
        .flatten()
        .map(|l| {
            if let PropertyValue::Data { value, .. } = l.value() {
                value
            } else {
                RegisterValue::undefined()
            }
        })
        .unwrap_or_else(RegisterValue::undefined);

    Ok((value, done))
}

fn type_error(runtime: &mut crate::interpreter::RuntimeState, message: &str) -> VmNativeCallError {
    match runtime.alloc_type_error(message) {
        Ok(handle) => VmNativeCallError::Thrown(RegisterValue::from_object_handle(handle.0)),
        Err(error) => VmNativeCallError::Internal(format!("{error}").into()),
    }
}

fn require_callable(
    val: RegisterValue,
    runtime: &crate::interpreter::RuntimeState,
    method_name: &str,
) -> Result<ObjectHandle, VmNativeCallError> {
    val.as_object_handle()
        .map(ObjectHandle)
        .filter(|h| runtime.objects().is_callable(*h))
        .ok_or_else(|| {
            VmNativeCallError::Internal(format!("{method_name}: callback is not a function").into())
        })
}

/// Tries to get a `[Symbol.iterator]()` result from a value.
/// Returns `Ok(Some(iterator))` if iterable, `Ok(None)` otherwise.
fn try_get_iterator(
    value: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<Option<RegisterValue>, VmNativeCallError> {
    let obj_handle = match value.as_object_handle().map(ObjectHandle) {
        Some(h) => h,
        None => return Ok(None),
    };
    let sym_iterator = runtime.intern_symbol_property_name(WellKnownSymbol::Iterator.stable_id());
    let iter_fn = match runtime.property_lookup(obj_handle, sym_iterator) {
        Ok(Some(lookup)) => {
            if let PropertyValue::Data { value: v, .. } = lookup.value() {
                v.as_object_handle().map(ObjectHandle)
            } else {
                None
            }
        }
        _ => None,
    };
    match iter_fn {
        Some(fn_handle) => {
            let iterator = runtime.call_callable(fn_handle, value, &[])?;
            Ok(Some(iterator))
        }
        None => Ok(None),
    }
}

fn require_this_iterator(
    this: &RegisterValue,
    method_name: &str,
) -> Result<ObjectHandle, VmNativeCallError> {
    this.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal(format!("{method_name}: this is not an object").into())
    })
}

// ═══════════════════════════════════════════════════════════════════════════
//  §27.1.4.1 Iterator.prototype.map(mapper)
//  Spec: <https://tc39.es/ecma262/#sec-iteratorprototype.map>
// ═══════════════════════════════════════════════════════════════════════════

const SLOT_SOURCE_ITER: &str = "__otter_iter_source__";
const SLOT_CALLBACK: &str = "__otter_iter_callback__";
const SLOT_REMAINING: &str = "__otter_iter_remaining__";
const SLOT_KIND: &str = "__otter_iter_kind__";
const SLOT_INNER_ITER: &str = "__otter_iter_inner__";

const KIND_MAP: i32 = 1;
const KIND_FILTER: i32 = 2;
const KIND_TAKE: i32 = 3;
const KIND_DROP: i32 = 4;
const KIND_FLAT_MAP: i32 = 5;

/// Creates a wrapper iterator with internal slots stored as properties.
fn create_wrapper_iterator(
    source: ObjectHandle,
    callback: RegisterValue,
    kind: i32,
    remaining: i32,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let proto = runtime.intrinsics().iterator_prototype();
    let wrapper = runtime.alloc_object_with_prototype(Some(proto));

    // Store internal state as properties.
    let src_prop = runtime.intern_property_name(SLOT_SOURCE_ITER);
    runtime
        .objects_mut()
        .set_property(
            wrapper,
            src_prop,
            RegisterValue::from_object_handle(source.0),
        )
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;

    let cb_prop = runtime.intern_property_name(SLOT_CALLBACK);
    runtime
        .objects_mut()
        .set_property(wrapper, cb_prop, callback)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;

    let kind_prop = runtime.intern_property_name(SLOT_KIND);
    runtime
        .objects_mut()
        .set_property(wrapper, kind_prop, RegisterValue::from_i32(kind))
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;

    let rem_prop = runtime.intern_property_name(SLOT_REMAINING);
    runtime
        .objects_mut()
        .set_property(wrapper, rem_prop, RegisterValue::from_i32(remaining))
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;

    // Install .next() method
    let next_desc = NativeFunctionDescriptor::method("next", 0, wrapper_iterator_next);
    let next_id = runtime.register_native_function(next_desc);
    let next_handle = runtime.alloc_host_function(next_id);
    let next_prop = runtime.intern_property_name("next");
    runtime
        .objects_mut()
        .set_property(
            wrapper,
            next_prop,
            RegisterValue::from_object_handle(next_handle.0),
        )
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;

    Ok(RegisterValue::from_object_handle(wrapper.0))
}

/// Helper: read an internal slot property from a wrapper iterator.
fn read_slot(
    handle: ObjectHandle,
    slot: &str,
    runtime: &mut crate::interpreter::RuntimeState,
) -> RegisterValue {
    let prop = runtime.intern_property_name(slot);
    runtime
        .property_lookup(handle, prop)
        .ok()
        .flatten()
        .map(|l| {
            if let PropertyValue::Data { value, .. } = l.value() {
                value
            } else {
                RegisterValue::undefined()
            }
        })
        .unwrap_or_else(RegisterValue::undefined)
}

/// The `.next()` method for wrapper iterators (map, filter, take, drop, flatMap).
fn wrapper_iterator_next(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_this_iterator(this, "IteratorHelper.next")?;

    let kind_val = read_slot(handle, SLOT_KIND, runtime);
    let kind = kind_val.as_i32().unwrap_or(0);

    let source_val = read_slot(handle, SLOT_SOURCE_ITER, runtime);
    let source = source_val
        .as_object_handle()
        .map(ObjectHandle)
        .ok_or_else(|| VmNativeCallError::Internal("wrapper iterator: missing source".into()))?;

    let callback_val = read_slot(handle, SLOT_CALLBACK, runtime);

    match kind {
        KIND_MAP => {
            let (value, done) = iter_step(source, runtime)?;
            if done {
                return create_iter_result_object(RegisterValue::undefined(), true, runtime);
            }
            let cb = require_callable(callback_val, runtime, "Iterator.prototype.map")?;
            let mapped = runtime.call_callable(cb, RegisterValue::undefined(), &[value])?;
            create_iter_result_object(mapped, false, runtime)
        }
        KIND_FILTER => loop {
            runtime.check_interrupt()?;
            let (value, done) = iter_step(source, runtime)?;
            if done {
                return create_iter_result_object(RegisterValue::undefined(), true, runtime);
            }
            let cb = require_callable(callback_val, runtime, "Iterator.prototype.filter")?;
            let result = runtime.call_callable(cb, RegisterValue::undefined(), &[value])?;
            if to_boolean(result) {
                return create_iter_result_object(value, false, runtime);
            }
        },
        KIND_TAKE => {
            let remaining = read_slot(handle, SLOT_REMAINING, runtime)
                .as_i32()
                .unwrap_or(0);
            if remaining <= 0 {
                return create_iter_result_object(RegisterValue::undefined(), true, runtime);
            }
            let (value, done) = iter_step(source, runtime)?;
            if done {
                return create_iter_result_object(RegisterValue::undefined(), true, runtime);
            }
            // Decrement remaining.
            let rem_prop = runtime.intern_property_name(SLOT_REMAINING);
            runtime
                .objects_mut()
                .set_property(handle, rem_prop, RegisterValue::from_i32(remaining - 1))
                .ok();
            create_iter_result_object(value, false, runtime)
        }
        KIND_DROP => {
            let mut remaining = read_slot(handle, SLOT_REMAINING, runtime)
                .as_i32()
                .unwrap_or(0);
            // Skip `remaining` elements.
            while remaining > 0 {
                runtime.check_interrupt()?;
                let (_, done) = iter_step(source, runtime)?;
                if done {
                    return create_iter_result_object(RegisterValue::undefined(), true, runtime);
                }
                remaining -= 1;
            }
            // Store 0 so subsequent calls don't re-skip.
            let rem_prop = runtime.intern_property_name(SLOT_REMAINING);
            runtime
                .objects_mut()
                .set_property(handle, rem_prop, RegisterValue::from_i32(0))
                .ok();
            let (value, done) = iter_step(source, runtime)?;
            if done {
                return create_iter_result_object(RegisterValue::undefined(), true, runtime);
            }
            create_iter_result_object(value, false, runtime)
        }
        KIND_FLAT_MAP => {
            // Check for active inner iterator first.
            let inner_val = read_slot(handle, SLOT_INNER_ITER, runtime);
            if let Some(inner_handle) = inner_val.as_object_handle().map(ObjectHandle) {
                let (value, done) = iter_step(inner_handle, runtime)?;
                if !done {
                    return create_iter_result_object(value, false, runtime);
                }
                // Inner exhausted — clear it.
                let inner_prop = runtime.intern_property_name(SLOT_INNER_ITER);
                runtime
                    .objects_mut()
                    .set_property(handle, inner_prop, RegisterValue::undefined())
                    .ok();
            }
            // Pull from source and create new inner iterator.
            loop {
                runtime.check_interrupt()?;
                let (value, done) = iter_step(source, runtime)?;
                if done {
                    return create_iter_result_object(RegisterValue::undefined(), true, runtime);
                }
                let cb = require_callable(callback_val, runtime, "Iterator.prototype.flatMap")?;
                let mapped = runtime.call_callable(cb, RegisterValue::undefined(), &[value])?;
                // Try to get an iterator from the mapped value.
                if let Some(inner_iter) = try_get_iterator(mapped, runtime)?
                    && let Some(inner_handle) = inner_iter.as_object_handle().map(ObjectHandle)
                {
                    let (v, d) = iter_step(inner_handle, runtime)?;
                    if !d {
                        // Store inner for subsequent calls.
                        let inner_prop = runtime.intern_property_name(SLOT_INNER_ITER);
                        runtime
                            .objects_mut()
                            .set_property(handle, inner_prop, inner_iter)
                            .ok();
                        return create_iter_result_object(v, false, runtime);
                    }
                    // Inner was empty — continue to next source element.
                    continue;
                }
                // If not iterable, yield the value directly.
                return create_iter_result_object(mapped, false, runtime);
            }
        }
        _ => create_iter_result_object(RegisterValue::undefined(), true, runtime),
    }
}

fn iterator_map(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_this_iterator(this, "Iterator.prototype.map")?;
    let callback = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let _ = require_callable(callback, runtime, "Iterator.prototype.map")?;
    create_wrapper_iterator(handle, callback, KIND_MAP, 0, runtime)
}

fn iterator_filter(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_this_iterator(this, "Iterator.prototype.filter")?;
    let callback = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let _ = require_callable(callback, runtime, "Iterator.prototype.filter")?;
    create_wrapper_iterator(handle, callback, KIND_FILTER, 0, runtime)
}

fn iterator_take(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_this_iterator(this, "Iterator.prototype.take")?;
    let limit = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let n = limit
        .as_i32()
        .or_else(|| limit.as_number().map(|f| f as i32))
        .unwrap_or(0);
    if n < 0 {
        return Err(type_error(
            runtime,
            "Iterator.prototype.take: limit must be non-negative",
        ));
    }
    create_wrapper_iterator(handle, RegisterValue::undefined(), KIND_TAKE, n, runtime)
}

fn iterator_drop(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_this_iterator(this, "Iterator.prototype.drop")?;
    let limit = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let n = limit
        .as_i32()
        .or_else(|| limit.as_number().map(|f| f as i32))
        .unwrap_or(0);
    if n < 0 {
        return Err(type_error(
            runtime,
            "Iterator.prototype.drop: limit must be non-negative",
        ));
    }
    create_wrapper_iterator(handle, RegisterValue::undefined(), KIND_DROP, n, runtime)
}

fn iterator_flat_map(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_this_iterator(this, "Iterator.prototype.flatMap")?;
    let callback = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let _ = require_callable(callback, runtime, "Iterator.prototype.flatMap")?;
    create_wrapper_iterator(handle, callback, KIND_FLAT_MAP, 0, runtime)
}

// ═══════════════════════════════════════════════════════════════════════════
//  §27.1.4.7 Iterator.prototype.toArray()
//  Spec: <https://tc39.es/ecma262/#sec-iteratorprototype.toarray>
// ═══════════════════════════════════════════════════════════════════════════

fn iterator_to_array(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_this_iterator(this, "Iterator.prototype.toArray")?;
    let array = runtime.alloc_array();
    loop {
        runtime.check_interrupt()?;
        let (value, done) = iter_step(handle, runtime)?;
        if done {
            break;
        }
        runtime.objects_mut().push_element(array, value).ok();
    }
    Ok(RegisterValue::from_object_handle(array.0))
}

// ═══════════════════════════════════════════════════════════════════════════
//  §27.1.4.8 Iterator.prototype.forEach(fn)
//  Spec: <https://tc39.es/ecma262/#sec-iteratorprototype.foreach>
// ═══════════════════════════════════════════════════════════════════════════

fn iterator_for_each(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_this_iterator(this, "Iterator.prototype.forEach")?;
    let callback = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let cb = require_callable(callback, runtime, "Iterator.prototype.forEach")?;
    loop {
        runtime.check_interrupt()?;
        let (value, done) = iter_step(handle, runtime)?;
        if done {
            break;
        }
        runtime.call_callable(cb, RegisterValue::undefined(), &[value])?;
    }
    Ok(RegisterValue::undefined())
}

// ═══════════════════════════════════════════════════════════════════════════
//  §27.1.4.9 Iterator.prototype.some(fn)
//  Spec: <https://tc39.es/ecma262/#sec-iteratorprototype.some>
// ═══════════════════════════════════════════════════════════════════════════

fn iterator_some(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_this_iterator(this, "Iterator.prototype.some")?;
    let callback = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let cb = require_callable(callback, runtime, "Iterator.prototype.some")?;
    loop {
        runtime.check_interrupt()?;
        let (value, done) = iter_step(handle, runtime)?;
        if done {
            return Ok(RegisterValue::from_bool(false));
        }
        let result = runtime.call_callable(cb, RegisterValue::undefined(), &[value])?;
        if to_boolean(result) {
            return Ok(RegisterValue::from_bool(true));
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  §27.1.4.10 Iterator.prototype.every(fn)
//  Spec: <https://tc39.es/ecma262/#sec-iteratorprototype.every>
// ═══════════════════════════════════════════════════════════════════════════

fn iterator_every(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_this_iterator(this, "Iterator.prototype.every")?;
    let callback = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let cb = require_callable(callback, runtime, "Iterator.prototype.every")?;
    loop {
        runtime.check_interrupt()?;
        let (value, done) = iter_step(handle, runtime)?;
        if done {
            return Ok(RegisterValue::from_bool(true));
        }
        let result = runtime.call_callable(cb, RegisterValue::undefined(), &[value])?;
        if !to_boolean(result) {
            return Ok(RegisterValue::from_bool(false));
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  §27.1.4.11 Iterator.prototype.find(fn)
//  Spec: <https://tc39.es/ecma262/#sec-iteratorprototype.find>
// ═══════════════════════════════════════════════════════════════════════════

fn iterator_find(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_this_iterator(this, "Iterator.prototype.find")?;
    let callback = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let cb = require_callable(callback, runtime, "Iterator.prototype.find")?;
    loop {
        runtime.check_interrupt()?;
        let (value, done) = iter_step(handle, runtime)?;
        if done {
            return Ok(RegisterValue::undefined());
        }
        let result = runtime.call_callable(cb, RegisterValue::undefined(), &[value])?;
        if to_boolean(result) {
            return Ok(value);
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  §27.1.4.6 Iterator.prototype.reduce(reducer, initialValue)
//  Spec: <https://tc39.es/ecma262/#sec-iteratorprototype.reduce>
// ═══════════════════════════════════════════════════════════════════════════

fn iterator_reduce(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_this_iterator(this, "Iterator.prototype.reduce")?;
    let callback = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let cb = require_callable(callback, runtime, "Iterator.prototype.reduce")?;

    let mut accumulator = if args.len() >= 2 {
        args[1]
    } else {
        // No initial value — use first element.
        let (value, done) = iter_step(handle, runtime)?;
        if done {
            return Err(type_error(
                runtime,
                "Iterator.prototype.reduce: empty iterator with no initial value",
            ));
        }
        value
    };

    loop {
        runtime.check_interrupt()?;
        let (value, done) = iter_step(handle, runtime)?;
        if done {
            return Ok(accumulator);
        }
        accumulator =
            runtime.call_callable(cb, RegisterValue::undefined(), &[accumulator, value])?;
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  §27.1.2.1 Iterator.from(o)
//  Spec: <https://tc39.es/ecma262/#sec-iterator.from>
// ═══════════════════════════════════════════════════════════════════════════

fn iterator_constructor(
    _this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    // Iterator is not directly constructible.
    Err(type_error(runtime, "Iterator is not a constructor"))
}

fn iterator_from(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let obj = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);

    // If obj has [Symbol.iterator], call it to get an iterator.
    if let Some(iterator) = try_get_iterator(obj, runtime)? {
        return Ok(iterator);
    }

    // If obj itself has a `.next()` method, treat it as an iterator already.
    if let Some(obj_handle) = obj.as_object_handle().map(ObjectHandle) {
        let next_prop = runtime.intern_property_name("next");
        if let Ok(Some(lookup)) = runtime.property_lookup(obj_handle, next_prop)
            && let PropertyValue::Data { value, .. } = lookup.value()
            && value.as_object_handle().is_some()
        {
            return Ok(obj);
        }
    }

    Err(type_error(
        runtime,
        "Iterator.from: argument is not iterable",
    ))
}

// ═══════════════════════════════════════════════════════════════════════════
//  ToBoolean (§7.1.2) — inline helper
//  Spec: <https://tc39.es/ecma262/#sec-toboolean>
// ═══════════════════════════════════════════════════════════════════════════

fn to_boolean(value: RegisterValue) -> bool {
    if let Some(b) = value.as_bool() {
        return b;
    }
    if value == RegisterValue::undefined() || value == RegisterValue::null() {
        return false;
    }
    if let Some(n) = value.as_i32() {
        return n != 0;
    }
    if let Some(n) = value.as_number() {
        return n != 0.0 && !n.is_nan();
    }
    // Objects, strings (non-empty) are truthy.
    if value.as_object_handle().is_some() {
        return true;
    }
    if value.is_symbol() {
        return true;
    }
    false
}
