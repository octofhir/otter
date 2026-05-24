//! Shared installer helpers used by every `intrinsics/<name>.rs`
//! module.
//!
//! These are the small, value-rooted allocation / native-function /
//! global-property primitives that the per-intrinsic installers
//! compose. They live here (not in `bootstrap.rs`) so the registry
//! file stays focused on entry-list bookkeeping.

use smallvec::SmallVec;

use crate::js_surface::{Attr, JsSurfaceError};
use crate::object::{self, JsObject, PropertyDescriptor};
use crate::{NativeCtx, NativeError, Value, VmGetOutcome, VmPropertyKey, descriptor_value};

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
    let proto = if let Some(exec) = ctx.execution_context().cloned() {
        let key = VmPropertyKey::String("prototype");
        let (interp, _) = ctx.interp_mut_and_context();
        match interp
            .ordinary_get_value(&exec, new_target, new_target, &key, 0)
            .map_err(|err| NativeError::TypeError {
                name,
                reason: err.to_string(),
            })? {
            VmGetOutcome::Value(value) => Some(value),
            VmGetOutcome::InvokeGetter { getter } => Some(
                interp
                    .run_callable_sync(&exec, &getter, new_target, SmallVec::new())
                    .map_err(|err| native_new_target_error(name, err))?,
            ),
        }
    } else if let Some(class) = new_target.as_class_constructor() {
        Some(Value::object(class.prototype(ctx.heap())))
    } else if let Some(obj) = new_target.as_object() {
        object::get(obj, ctx.heap(), "prototype")
    } else if let Some(native) = new_target.as_native_function() {
        native
            .own_property_descriptor(ctx.heap_mut(), "prototype")
            .map_err(|err| NativeError::TypeError {
                name,
                reason: err.to_string(),
            })?
            .map(|descriptor| descriptor_value(&descriptor))
    } else {
        None
    };
    Ok(proto.filter(|value| value.is_object_type() || value.is_proxy()))
}

fn native_new_target_error(name: &'static str, err: crate::VmError) -> NativeError {
    match err {
        crate::VmError::Uncaught { value } => NativeError::Thrown {
            name,
            message: value,
        },
        other => NativeError::TypeError {
            name,
            reason: other.to_string(),
        },
    }
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
    let placeholder = alloc_object_with_value_roots(heap, &[&global_root])?;
    let placeholder_root = Value::object(placeholder);
    let proto = alloc_object_with_value_roots(heap, &[&global_root, &placeholder_root])?;
    object::set(placeholder, heap, "prototype", Value::object(proto));
    define_global(global, heap, name, Value::object(placeholder));
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
