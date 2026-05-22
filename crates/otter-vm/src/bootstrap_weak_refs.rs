//! ECMA-262 §26.1 / §26.2 bootstrap installers for `WeakRef` and
//! `FinalizationRegistry`.
//!
//! Each global gets a real callable + constructible
//! [`crate::native_function::NativeFunction`] with the spec
//! prototype attached, replacing the bootstrap placeholders.
//!
//! # Contents
//! - [`install_weak_ref`] — bootstrap entry for `WeakRef`.
//! - [`install_finalization_registry`] — bootstrap entry for
//!   `FinalizationRegistry`.
//! - [`install_weak_well_knowns_post_bootstrap`] —
//!   `@@toStringTag` fixup driven by the post-bootstrap hook.
//!
//! # Invariants
//! - `new WeakRef(target)` requires `target` to be an Object (any
//!   heap-allocated value kind). Calling without `new` or with a
//!   primitive target throws `TypeError`.
//! - `new FinalizationRegistry(cleanup)` requires `cleanup` to be
//!   callable; otherwise `TypeError`.
//! - `.deref()` / `.register()` / `.unregister()` are real own
//!   data properties on the prototype.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-weak-ref-constructor>
//! - <https://tc39.es/ecma262/#sec-finalization-registry-constructor>

use crate::js_surface::{Attr, JsSurfaceError, ObjectBuilder};
use crate::native_function::NativeCall;
use crate::object::{self, JsObject, PartialPropertyDescriptor, PropertyDescriptor};
use crate::weak_refs::{self, JsFinalizationRegistry, JsWeakRef};
use crate::{NativeCtx, NativeError, Value};

/// `BuiltinIntrinsic` adapter for `WeakRef`.
pub struct WeakRefIntrinsic;

impl crate::intrinsic_install::BuiltinIntrinsic for WeakRefIntrinsic {
    const NAME: &'static str = "WeakRef";
    const FEATURE: crate::bootstrap::BootstrapFeatures = crate::bootstrap::BootstrapFeatures::CORE;
    fn install(heap: &mut otter_gc::GcHeap, global: JsObject) -> Result<(), JsSurfaceError> {
        install_weak_ref(heap, global)
    }
}

/// §26.1 WeakRef — installer body, called through [`WeakRefIntrinsic`].
fn install_weak_ref(heap: &mut otter_gc::GcHeap, global: JsObject) -> Result<(), JsSurfaceError> {
    let global_root = Value::object(global);
    let prototype = crate::bootstrap::alloc_object_with_value_roots(heap, &[&global_root])?;
    link_object_prototype(heap, prototype, global);
    {
        let mut builder =
            ObjectBuilder::from_object_with_value_roots(heap, prototype, vec![global_root]);
        builder.method(
            "deref",
            0,
            NativeCall::Static(weak_ref_proto_deref),
            Attr::builtin_function(),
        )?;
    }
    let prototype_root = Value::object(prototype);
    let ctor = crate::bootstrap::native_constructor_static_with_value_roots(
        heap,
        "WeakRef",
        1,
        weak_ref_ctor_call,
        &[&global_root, &prototype_root],
    )
    .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let proto_desc = PropertyDescriptor::data(Value::Object(prototype), false, false, false);
    if !ctor.define_own_property(heap, "prototype", proto_desc) {
        return Err(JsSurfaceError::DefinePropertyFailed("prototype"));
    }
    object::define_own_property(
        prototype,
        heap,
        "constructor",
        PropertyDescriptor::data(Value::NativeFunction(ctor), true, false, true),
    );
    crate::bootstrap::define_global_value(
        global,
        heap,
        <WeakRefIntrinsic as crate::intrinsic_install::BuiltinIntrinsic>::NAME,
        Value::NativeFunction(ctor),
    );
    Ok(())
}

/// `BuiltinIntrinsic` adapter for `FinalizationRegistry`.
pub struct FinalizationRegistryIntrinsic;

impl crate::intrinsic_install::BuiltinIntrinsic for FinalizationRegistryIntrinsic {
    const NAME: &'static str = "FinalizationRegistry";
    const FEATURE: crate::bootstrap::BootstrapFeatures = crate::bootstrap::BootstrapFeatures::CORE;
    fn install(heap: &mut otter_gc::GcHeap, global: JsObject) -> Result<(), JsSurfaceError> {
        install_finalization_registry(heap, global)
    }
}

/// §26.2 FinalizationRegistry — installer body, called through
/// [`FinalizationRegistryIntrinsic`].
fn install_finalization_registry(
    heap: &mut otter_gc::GcHeap,
    global: JsObject,
) -> Result<(), JsSurfaceError> {
    let global_root = Value::object(global);
    let prototype = crate::bootstrap::alloc_object_with_value_roots(heap, &[&global_root])?;
    link_object_prototype(heap, prototype, global);
    {
        let mut builder =
            ObjectBuilder::from_object_with_value_roots(heap, prototype, vec![global_root]);
        builder.method(
            "register",
            2,
            NativeCall::Static(fr_proto_register),
            Attr::builtin_function(),
        )?;
        builder.method(
            "unregister",
            1,
            NativeCall::Static(fr_proto_unregister),
            Attr::builtin_function(),
        )?;
    }
    let prototype_root = Value::object(prototype);
    let ctor = crate::bootstrap::native_constructor_static_with_value_roots(
        heap,
        "FinalizationRegistry",
        1,
        fr_ctor_call,
        &[&global_root, &prototype_root],
    )
    .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let proto_desc = PropertyDescriptor::data(Value::Object(prototype), false, false, false);
    if !ctor.define_own_property(heap, "prototype", proto_desc) {
        return Err(JsSurfaceError::DefinePropertyFailed("prototype"));
    }
    object::define_own_property(
        prototype,
        heap,
        "constructor",
        PropertyDescriptor::data(Value::NativeFunction(ctor), true, false, true),
    );
    crate::bootstrap::define_global_value(
        global,
        heap,
        <FinalizationRegistryIntrinsic as crate::intrinsic_install::BuiltinIntrinsic>::NAME,
        Value::NativeFunction(ctor),
    );
    Ok(())
}

/// Install `@@toStringTag` on both prototypes once the per-realm
/// well-known table exists.
pub fn install_weak_well_knowns_post_bootstrap(
    heap: &mut otter_gc::GcHeap,
    global: JsObject,
    well_known: &crate::symbol::WellKnownSymbols,
) -> Result<(), JsSurfaceError> {
    use crate::symbol::WellKnown;

    let tag_sym = well_known.get(WellKnown::ToStringTag);
    for (ctor_name, tag_value) in [
        ("WeakRef", "WeakRef"),
        ("FinalizationRegistry", "FinalizationRegistry"),
    ] {
        let Some(prototype) = ctor_prototype(global, heap, ctor_name) else {
            continue;
        };
        let tag = crate::string::JsString::from_str(tag_value, heap)
            .map_err(|_| JsSurfaceError::OutOfMemory)?;
        object::define_own_symbol_property_partial(
            prototype,
            heap,
            &tag_sym,
            PartialPropertyDescriptor {
                value: Some(Value::String(tag)),
                writable: Some(false),
                enumerable: Some(false),
                configurable: Some(true),
                ..Default::default()
            },
        );
    }
    Ok(())
}

// ---------------------------------------------------------------
// Constructor bodies
// ---------------------------------------------------------------

fn weak_ref_ctor_call(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    if !ctx.is_construct_call() {
        return Err(NativeError::TypeError {
            name: "WeakRef",
            reason: "constructor requires 'new'".to_string(),
        });
    }
    let target = args.first().cloned().unwrap_or(Value::undefined());
    if !target_can_be_weak(&target) {
        return Err(NativeError::TypeError {
            name: "WeakRef",
            reason: "target must be an object".to_string(),
        });
    }
    let weak_ref = ctx
        .alloc_weak_ref(&target, &[], &[args])
        .map_err(|_| oom("WeakRef"))?;
    if let Some(proto) = crate::bootstrap::native_new_target_prototype(ctx, "WeakRef")? {
        weak_refs::set_weak_ref_prototype_override(weak_ref, ctx.heap_mut(), Some(proto));
    }
    Ok(Value::WeakRef(weak_ref))
}

fn fr_ctor_call(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    if !ctx.is_construct_call() {
        return Err(NativeError::TypeError {
            name: "FinalizationRegistry",
            reason: "constructor requires 'new'".to_string(),
        });
    }
    let cleanup = args.first().cloned().unwrap_or(Value::undefined());
    if !ctx.interp_mut().is_callable_runtime(&cleanup) {
        return Err(NativeError::TypeError {
            name: "FinalizationRegistry",
            reason: "cleanup must be a function".to_string(),
        });
    }
    let context = ctx.execution_context().cloned();
    let registry = ctx
        .alloc_finalization_registry(cleanup, context, &[], &[args])
        .map_err(|_| oom("FinalizationRegistry"))?;
    if let Some(proto) = crate::bootstrap::native_new_target_prototype(ctx, "FinalizationRegistry")?
    {
        weak_refs::set_finalization_registry_prototype_override(
            registry,
            ctx.heap_mut(),
            Some(proto),
        );
    }
    Ok(Value::FinalizationRegistry(registry))
}

// ---------------------------------------------------------------
// Prototype method bodies
// ---------------------------------------------------------------

fn weak_ref_proto_deref(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let weak_ref = receiver_weak_ref(ctx, "WeakRef.prototype.deref")?;
    Ok(weak_refs::weak_ref_deref(weak_ref, ctx.heap()))
}

fn fr_proto_register(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let registry = receiver_finalization_registry(ctx, "FinalizationRegistry.prototype.register")?;
    let target = args.first().cloned().unwrap_or(Value::undefined());
    let held_value = args.get(1).cloned().unwrap_or(Value::undefined());
    let unregister_token = args.get(2).cloned();
    if !target_can_be_weak(&target) {
        return Err(NativeError::TypeError {
            name: "FinalizationRegistry.prototype.register",
            reason: "target must be an object".to_string(),
        });
    }
    weak_refs::finalization_registry_register(
        registry,
        ctx.heap_mut(),
        &target,
        held_value,
        unregister_token.as_ref(),
    )
    .map_err(|e| vm_to_native(e, "FinalizationRegistry.prototype.register"))?;
    Ok(Value::undefined())
}

fn fr_proto_unregister(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let registry =
        receiver_finalization_registry(ctx, "FinalizationRegistry.prototype.unregister")?;
    let token = args.first().cloned().unwrap_or(Value::undefined());
    let removed = weak_refs::finalization_registry_unregister(registry, ctx.heap_mut(), &token)
        .map_err(|e| vm_to_native(e, "FinalizationRegistry.prototype.unregister"))?;
    Ok(Value::Boolean(removed))
}

// ---------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------

fn link_object_prototype(heap: &mut otter_gc::GcHeap, prototype: JsObject, global: JsObject) {
    if let Some(Value::Object(object_ctor)) = object::get(global, heap, "Object")
        && let Some(Value::Object(object_proto)) = object::get(object_ctor, heap, "prototype")
    {
        object::set_prototype(prototype, heap, Some(object_proto));
    }
}

fn ctor_prototype(
    global: JsObject,
    heap: &mut otter_gc::GcHeap,
    ctor_name: &str,
) -> Option<JsObject> {
    let Some(Value::NativeFunction(f)) = object::get(global, heap, ctor_name) else {
        return None;
    };
    let descriptor = f
        .own_property_descriptor(&mut *heap, "prototype")
        .ok()
        .flatten()?;
    match descriptor.kind {
        crate::object::DescriptorKind::Data {
            value: Value::Object(p),
        } => Some(p),
        _ => None,
    }
}

fn receiver_weak_ref(ctx: &NativeCtx<'_>, name: &'static str) -> Result<JsWeakRef, NativeError> {
    match ctx.this_value() {
        Value::WeakRef(w) => Ok(*w),
        _ => Err(NativeError::TypeError {
            name,
            reason: "this is not a WeakRef".to_string(),
        }),
    }
}

fn receiver_finalization_registry(
    ctx: &NativeCtx<'_>,
    name: &'static str,
) -> Result<JsFinalizationRegistry, NativeError> {
    match ctx.this_value() {
        Value::FinalizationRegistry(r) => Ok(*r),
        _ => Err(NativeError::TypeError {
            name,
            reason: "this is not a FinalizationRegistry".to_string(),
        }),
    }
}

fn target_can_be_weak(v: &Value) -> bool {
    matches!(
        v,
        Value::Object(_)
            | Value::Array(_)
            | Value::Function { .. }
            | Value::Closure(_)
            | Value::NativeFunction(_)
            | Value::BoundFunction(_)
            | Value::ClassConstructor(_)
            | Value::Promise(_)
            | Value::Iterator(_)
            | Value::RegExp(_)
            | Value::Map(_)
            | Value::Set(_)
            | Value::WeakMap(_)
            | Value::WeakSet(_)
            | Value::WeakRef(_)
            | Value::FinalizationRegistry(_)
            | Value::Temporal(_)
            | Value::Intl(_)
            | Value::ArrayBuffer(_)
            | Value::DataView(_)
            | Value::TypedArray(_)
            | Value::Generator(_)
            | Value::Proxy(_)
            | Value::Symbol(_)
    )
}

fn oom(name: &'static str) -> NativeError {
    NativeError::TypeError {
        name,
        reason: "out of memory".to_string(),
    }
}

fn vm_to_native(err: crate::VmError, name: &'static str) -> NativeError {
    match err {
        crate::VmError::TypeError { message } => NativeError::TypeError {
            name,
            reason: message,
        },
        crate::VmError::NotCallable => NativeError::TypeError {
            name,
            reason: "value is not callable".to_string(),
        },
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
