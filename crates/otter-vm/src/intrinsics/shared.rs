//! Shared installer helpers used by every `intrinsics/<name>.rs`
//! module.
//!
//! These are the small, value-rooted allocation / native-function /
//! global-property primitives that the per-intrinsic installers
//! compose. They live here (not in `bootstrap.rs`) so the registry
//! file stays focused on entry-list bookkeeping.

use crate::js_surface::{Attr, JsSurfaceError};
use crate::object::{self, JsObject, PropertyDescriptor};
use crate::rooting::RootScopeExt;
use crate::{NativeCtx, NativeError, Value, VmGetOutcome, VmPropertyKey, descriptor_value};

/// `pub` re-export of [`alloc_object_with_value_roots`] for use by
/// the macro-generated `install` bodies in `crates/otter-macros`
/// (the `couch!` macro allocates an empty prototype object before
/// pinning methods on it). Hand-written installers continue to use
/// the `pub(crate)` form directly.
pub fn alloc_object_with_value_roots_pub(
    heap: &mut otter_gc::GcHeap,
    value_roots: &[&Value],
) -> Result<JsObject, otter_gc::OutOfMemory> {
    alloc_object_with_value_roots(heap, value_roots)
}

/// Allocate an empty object while keeping the supplied `value_roots`
/// alive across the allocation.
pub(crate) fn alloc_object_with_value_roots(
    heap: &mut otter_gc::GcHeap,
    value_roots: &[&Value],
) -> Result<JsObject, otter_gc::OutOfMemory> {
    let mut external_visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
        for value in value_roots {
            value.trace_value_slots(visitor);
        }
    };
    object::alloc_object_with_roots(heap, &mut external_visit)
}

/// Allocate a static native constructor with `value_roots` kept live.
///
/// `pub` because the `couch!` macro expands generated `install`
/// bodies to call this helper from outside `otter-vm`. Hand-written
/// installers inside the crate continue to use the
/// `bootstrap::native_constructor_static_with_value_roots` re-export.
pub fn native_constructor_static_with_value_roots(
    heap: &mut otter_gc::GcHeap,
    name: &'static str,
    length: u8,
    call: crate::native_function::NativeFastFn,
    value_roots: &[&Value],
) -> Result<crate::native_function::NativeFunction, otter_gc::OutOfMemory> {
    let mut external_visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
        for value in value_roots {
            value.trace_value_slots(visitor);
        }
    };
    crate::native_function::NativeFunction::new_constructor_static_with_roots(
        heap,
        name,
        length,
        call,
        &mut external_visit,
    )
}

/// Allocate a static native non-constructor with `value_roots` kept live.
///
/// `pub` for the same reason as
/// [`native_constructor_static_with_value_roots`]: the `couch!` /
/// `lodge!` macros call it from their generated `install` bodies.
pub fn native_static_with_value_roots(
    heap: &mut otter_gc::GcHeap,
    name: &'static str,
    length: u8,
    call: crate::native_function::NativeFastFn,
    value_roots: &[&Value],
) -> Result<crate::native_function::NativeFunction, otter_gc::OutOfMemory> {
    let mut external_visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
        for value in value_roots {
            value.trace_value_slots(visitor);
        }
    };
    crate::native_function::NativeFunction::new_static_with_roots(
        heap,
        name,
        length,
        call,
        &mut external_visit,
    )
}

/// Allocate a native function from a full [`crate::NativeCall`] target with
/// `value_roots` kept live.
///
/// `pub` for the `couch!` macro's generated `install` body: unlike
/// [`native_static_with_value_roots`] (which only takes the `NativeFastFn` of a
/// `NativeCall::Static`), this accepts every `NativeCall` variant — including
/// the `VmIntrinsic` fast-path targets many builtin prototype methods use — so
/// the prototype install loop can build methods by reference without dropping to
/// the `ObjectBuilder`. Mirrors the allocation `crate::ObjectBuilder::method`
/// performs.
pub fn native_from_call_with_value_roots(
    heap: &mut otter_gc::GcHeap,
    name: &'static str,
    length: u8,
    call: crate::native_function::NativeCall,
    value_roots: &[&Value],
) -> Result<crate::native_function::NativeFunction, otter_gc::OutOfMemory> {
    let mut external_visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
        for value in value_roots {
            value.trace_value_slots(visitor);
        }
    };
    crate::native_function::NativeFunction::from_call_with_roots(
        heap,
        name,
        length,
        call,
        &mut external_visit,
    )
}

/// Resolve `new.target.prototype` for a native constructor — used by
/// every builtin whose `[[Construct]]` allocates via
/// `OrdinaryCreateFromConstructor`.
pub(crate) fn native_new_target_prototype(
    ctx: &mut NativeCtx<'_>,
    name: &'static str,
) -> Result<Option<Value>, NativeError> {
    let Some(new_target) = ctx.new_target().cloned() else {
        return Ok(None);
    };
    let exec = ctx.execution_context().cloned();
    ctx.scope(|mut scope| {
        let new_target = scope.value(new_target);
        let proto = if let Some(exec) = exec {
            let key = VmPropertyKey::String("prototype");
            let receiver = scope.raw(new_target);
            match scope
                .with_turn_parts(|interp, stack| {
                    interp.ordinary_get_value(stack, &exec, receiver, receiver, &key, 0)
                })
                .map_err(|err| NativeError::TypeError {
                    name,
                    reason: err.to_string(),
                })? {
                VmGetOutcome::Value(value) => Some(scope.value(value)),
                VmGetOutcome::InvokeGetter { getter } => {
                    let getter = scope.value(getter);
                    let result = scope.call(getter, new_target, &[])?;
                    Some(result)
                }
            }
        } else {
            let new_target_raw = scope.raw(new_target);
            let value = if let Some(class) = new_target_raw.as_class_constructor() {
                Some(Value::object(class.prototype(scope.context().heap())))
            } else if let Some(obj) = new_target_raw.as_object() {
                object::get(obj, scope.context().heap(), "prototype")
            } else if let Some(native) = new_target_raw.as_native_function() {
                native
                    .own_property_descriptor(scope.context().heap_mut(), "prototype")
                    .map_err(|err| NativeError::TypeError {
                        name,
                        reason: err.to_string(),
                    })?
                    .map(|descriptor| descriptor_value(&descriptor))
            } else {
                None
            };
            value.map(|value| scope.value(value))
        };
        let Some(proto) = proto else {
            return Ok(None);
        };
        let proto = scope.finish(proto);
        Ok(proto
            .is_object_type()
            .then_some(proto)
            .or_else(|| proto.is_proxy().then_some(proto)))
    })
}

/// Install an empty `<name>` global with a fresh prototype slot.
/// Used by `Intl` / `Temporal` / `AggregateError` until each ships a
/// real implementation.
pub(crate) fn install_placeholder(
    name: &'static str,
    heap: &mut otter_gc::GcHeap,
    global: JsObject,
) -> Result<(), JsSurfaceError> {
    let global_root = Value::object(global);
    let mut placeholder_root = Value::undefined();
    let mut proto_root = Value::undefined();
    let mut scope = otter_gc::RootScope::new(heap);
    // SAFETY: both canonical slots are declared before the scope and remain
    // live until the placeholder has been attached to the global object.
    unsafe {
        scope.add_value(&mut placeholder_root);
        scope.add_value(&mut proto_root);
    }
    placeholder_root = Value::object(alloc_object_with_value_roots(heap, &[&global_root])?);
    proto_root = Value::object(alloc_object_with_value_roots(
        heap,
        &[&global_root, &placeholder_root],
    )?);
    let mut placeholder = placeholder_root
        .as_object()
        .expect("placeholder stays rooted across prototype allocation");
    object::set(&mut placeholder, heap, "prototype", proto_root);
    placeholder_root = Value::object(placeholder);
    define_global(global, heap, name, placeholder_root);
    Ok(())
}

/// Install `name = value` on `global` with the standard
/// writable / non-enumerable / configurable global-binding attributes.
pub(crate) fn define_global(
    global: JsObject,
    heap: &mut otter_gc::GcHeap,
    name: &'static str,
    value: Value,
) {
    let descriptor = PropertyDescriptor::data(
        value,
        Attr::global_binding().writable,
        Attr::global_binding().enumerable,
        Attr::global_binding().configurable,
    );
    let _ = object::define_own_property(global, heap, name, descriptor);
}

/// Convenience wrapper around [`define_global`] for installer call sites.
///
/// `pub` because the `holt!` / `couch!` / `lodge!` macros expand
/// their generated `install` bodies to call this helper from outside
/// `otter-vm`. Hand-written installers inside the crate keep using
/// it through the `bootstrap::` re-export.
pub fn define_global_value(
    global: JsObject,
    heap: &mut otter_gc::GcHeap,
    name: &'static str,
    value: Value,
) {
    define_global(global, heap, name, value);
}
