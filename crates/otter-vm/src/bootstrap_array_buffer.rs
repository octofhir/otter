//! ECMA-262 §25.1 ArrayBuffer bootstrap installer.
//!
//! Replaces the placeholder with a real callable + constructible
//! `NativeFunction`. Statics: `isView`. Prototype: `slice` +
//! `resize` + `transfer` + `transferToFixedLength`, plus accessor
//! getters for `byteLength`, `maxByteLength`, `resizable`,
//! `detached`.
//!
//! Prototype methods delegate to the existing
//! [`crate::binary::array_buffer_prototype`] intrinsic table.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-arraybuffer-constructor>
//! - <https://tc39.es/ecma262/#sec-properties-of-the-arraybuffer-prototype-object>

use otter_bytecode::method_id::{ArrayBufferMethod, SharedArrayBufferMethod};
use smallvec::SmallVec;

use crate::binary::{array_buffer_prototype, dispatch};
use crate::bootstrap::{
    alloc_object_with_value_roots, native_constructor_static_with_value_roots,
    native_static_with_value_roots,
};
use crate::intrinsics::IntrinsicArgs;
use crate::js_surface::{Attr, JsSurfaceError, ObjectBuilder};
use crate::native_function::NativeCall;
use crate::number::NumberValue;
use crate::object::{self, JsObject, PartialPropertyDescriptor, PropertyDescriptor};
use crate::{NativeCtx, NativeError, Value, VmError};

const AB_METHODS: &[(&str, u8, crate::native_function::NativeFastFn)] = &[
    ("slice", 2, ab_slice),
    ("resize", 1, ab_resize),
    ("transfer", 1, ab_transfer),
    ("transferToFixedLength", 1, ab_transfer_to_fixed_length),
];

/// `BuiltinIntrinsic` adapter for `ArrayBuffer`.
pub struct ArrayBufferIntrinsic;

impl crate::intrinsic_install::BuiltinIntrinsic for ArrayBufferIntrinsic {
    const NAME: &'static str = "ArrayBuffer";
    const FEATURE: crate::bootstrap::BootstrapFeatures = crate::bootstrap::BootstrapFeatures::CORE;
    fn install(heap: &mut otter_gc::GcHeap, global: JsObject) -> Result<(), JsSurfaceError> {
        install_array_buffer(Self::NAME, heap, global)
    }
}

/// §25.1 ArrayBuffer — installer body, called through
/// [`ArrayBufferIntrinsic`].
fn install_array_buffer(
    name: &'static str,
    heap: &mut otter_gc::GcHeap,
    global: JsObject,
) -> Result<(), JsSurfaceError> {
    let global_root = Value::Object(global);
    let prototype = alloc_object_with_value_roots(heap, &[&global_root])?;
    let prototype_root = Value::Object(prototype);
    if let Some(Value::Object(object_ctor)) = object::get(global, heap, "Object")
        && let Some(Value::Object(object_proto)) = object::get(object_ctor, heap, "prototype")
    {
        object::set_prototype(prototype, heap, Some(object_proto));
    }
    {
        let mut builder =
            ObjectBuilder::from_object_with_value_roots(heap, prototype, vec![global_root.clone()]);
        for (name, length, call) in AB_METHODS {
            builder.method(
                name,
                *length,
                NativeCall::Static(*call),
                Attr::builtin_function(),
            )?;
        }
    }
    install_accessor(
        heap,
        prototype,
        "byteLength",
        ab_byte_length,
        &[&global_root, &prototype_root],
    )?;
    install_accessor(
        heap,
        prototype,
        "maxByteLength",
        ab_max_byte_length,
        &[&global_root, &prototype_root],
    )?;
    install_accessor(
        heap,
        prototype,
        "resizable",
        ab_resizable,
        &[&global_root, &prototype_root],
    )?;
    install_accessor(
        heap,
        prototype,
        "detached",
        ab_detached,
        &[&global_root, &prototype_root],
    )?;

    let ctor = native_constructor_static_with_value_roots(
        heap,
        "ArrayBuffer",
        1,
        ab_ctor_call,
        &[&global_root, &prototype_root],
    )
    .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let ctor_root = Value::NativeFunction(ctor);
    let string_heap = crate::string::StringHeap::default();
    let proto_desc = PropertyDescriptor::data(Value::Object(prototype), false, false, false);
    if !ctor.define_own_property(heap, &string_heap, "prototype", proto_desc) {
        return Err(JsSurfaceError::DefinePropertyFailed("prototype"));
    }
    // §25.1.3.1 ArrayBuffer.isView(arg).
    let is_view_fn = native_static_with_value_roots(
        heap,
        "isView",
        1,
        ab_is_view,
        &[&global_root, &prototype_root, &ctor_root],
    )
    .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let attrs = Attr::builtin_function();
    let _ = ctor.define_own_property(
        heap,
        &string_heap,
        "isView",
        PropertyDescriptor::data(
            Value::NativeFunction(is_view_fn),
            attrs.writable,
            attrs.enumerable,
            attrs.configurable,
        ),
    );
    object::define_own_property(
        prototype,
        heap,
        "constructor",
        PropertyDescriptor::data(ctor_root.clone(), true, false, true),
    );
    crate::bootstrap::define_global_value(global, heap, name, ctor_root);
    Ok(())
}

/// `BuiltinIntrinsic` adapter for `SharedArrayBuffer`.
pub struct SharedArrayBufferIntrinsic;

impl crate::intrinsic_install::BuiltinIntrinsic for SharedArrayBufferIntrinsic {
    const NAME: &'static str = "SharedArrayBuffer";
    const FEATURE: crate::bootstrap::BootstrapFeatures = crate::bootstrap::BootstrapFeatures::CORE;
    fn install(heap: &mut otter_gc::GcHeap, global: JsObject) -> Result<(), JsSurfaceError> {
        install_shared_array_buffer(Self::NAME, heap, global)
    }
}

/// §25.2 SharedArrayBuffer — installer body, called through
/// [`SharedArrayBufferIntrinsic`]. Shares the underlying
/// `JsArrayBuffer` substrate with `ArrayBuffer` — `is_shared`
/// distinguishes the two on the value side.
fn install_shared_array_buffer(
    name: &'static str,
    heap: &mut otter_gc::GcHeap,
    global: JsObject,
) -> Result<(), JsSurfaceError> {
    let global_root = Value::Object(global);
    let prototype = alloc_object_with_value_roots(heap, &[&global_root])?;
    let prototype_root = Value::Object(prototype);
    if let Some(Value::Object(object_ctor)) = object::get(global, heap, "Object")
        && let Some(Value::Object(object_proto)) = object::get(object_ctor, heap, "prototype")
    {
        object::set_prototype(prototype, heap, Some(object_proto));
    }
    {
        let mut builder =
            ObjectBuilder::from_object_with_value_roots(heap, prototype, vec![global_root.clone()]);
        // `slice` + `grow` are the spec methods on SAB. `transfer`
        // / `transferToFixedLength` belong to ArrayBuffer only.
        builder.method(
            "slice",
            2,
            NativeCall::Static(ab_slice),
            Attr::builtin_function(),
        )?;
        builder.method(
            "grow",
            1,
            NativeCall::Static(sab_grow),
            Attr::builtin_function(),
        )?;
    }
    install_accessor(
        heap,
        prototype,
        "byteLength",
        ab_byte_length,
        &[&global_root, &prototype_root],
    )?;
    install_accessor(
        heap,
        prototype,
        "maxByteLength",
        ab_max_byte_length,
        &[&global_root, &prototype_root],
    )?;
    install_accessor(
        heap,
        prototype,
        "growable",
        sab_growable,
        &[&global_root, &prototype_root],
    )?;

    let ctor = native_constructor_static_with_value_roots(
        heap,
        "SharedArrayBuffer",
        1,
        sab_ctor_call,
        &[&global_root, &prototype_root],
    )
    .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let ctor_root = Value::NativeFunction(ctor);
    let string_heap = crate::string::StringHeap::default();
    let proto_desc = PropertyDescriptor::data(Value::Object(prototype), false, false, false);
    if !ctor.define_own_property(heap, &string_heap, "prototype", proto_desc) {
        return Err(JsSurfaceError::DefinePropertyFailed("prototype"));
    }
    object::define_own_property(
        prototype,
        heap,
        "constructor",
        PropertyDescriptor::data(ctor_root.clone(), true, false, true),
    );
    crate::bootstrap::define_global_value(global, heap, name, ctor_root);
    Ok(())
}

fn sab_ctor_call(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    if !ctx.is_construct_call() {
        return Err(NativeError::TypeError {
            name: "SharedArrayBuffer",
            reason: "constructor requires 'new'".to_string(),
        });
    }
    let roots = ctx.collect_native_roots();
    let this_value = ctx.this_value().clone();
    let new_target = ctx.new_target().cloned();
    let mut external_visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
        crate::runtime_cx::visit_native_roots(
            visitor,
            &roots,
            &this_value,
            new_target.as_ref(),
            &[],
            &[args],
        );
    };
    let value = dispatch::shared_array_buffer_call_with_roots(
        SharedArrayBufferMethod::Construct,
        args,
        ctx.heap_mut(),
        &mut external_visit,
    )
    .map_err(|e| vm_to_native(e, "SharedArrayBuffer"))?;
    // §10.1.13 GetPrototypeFromConstructor — derived `super()`
    // construction forwards `new.target`, so the allocated exotic
    // receives `Subclass.prototype` as its observable [[Prototype]].
    // <https://tc39.es/ecma262/#sec-getprototypefromconstructor>
    let needs_proto_override = !matches!(ctx.new_target(), Some(Value::NativeFunction(_)));
    if needs_proto_override
        && let Some(proto) =
            crate::bootstrap::native_new_target_prototype(ctx, "SharedArrayBuffer")?
    {
        ctx.interp_mut()
            .set_non_gc_exotic_prototype_override(&value, Some(proto));
    }
    Ok(value)
}

fn sab_grow(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    // SAB.prototype.grow shares the same intrinsic-table body as
    // AB.prototype.resize (both flow through `grow_buffer`).
    dispatch_method(ctx, args, "grow")
}

fn sab_growable(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let b = receiver_ab(ctx, "get SharedArrayBuffer.prototype.growable")?;
    Ok(Value::Boolean(b.is_shared() && b.is_resizable()))
}

/// Install `SharedArrayBuffer.prototype[@@toStringTag] = "SharedArrayBuffer"`.
pub fn install_shared_array_buffer_well_knowns_post_bootstrap(
    heap: &mut otter_gc::GcHeap,
    string_heap: &crate::string::StringHeap,
    global: JsObject,
    well_known: &crate::symbol::WellKnownSymbols,
) -> Result<(), JsSurfaceError> {
    use crate::symbol::WellKnown;

    let Some(Value::NativeFunction(ctor)) = object::get(global, heap, "SharedArrayBuffer") else {
        return Ok(());
    };
    let descriptor = ctor
        .own_property_descriptor(heap, string_heap, "prototype")
        .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let prototype = match descriptor.and_then(|d| match d.kind {
        crate::object::DescriptorKind::Data {
            value: Value::Object(p),
        } => Some(p),
        _ => None,
    }) {
        Some(p) => p,
        None => return Ok(()),
    };
    let tag = crate::string::JsString::from_str("SharedArrayBuffer", string_heap)
        .map_err(|_| JsSurfaceError::OutOfMemory)?;
    object::define_own_symbol_property_partial(
        prototype,
        heap,
        &well_known.get(WellKnown::ToStringTag),
        PartialPropertyDescriptor {
            value: Some(Value::String(tag)),
            writable: Some(false),
            enumerable: Some(false),
            configurable: Some(true),
            ..Default::default()
        },
    );
    Ok(())
}

/// Install `ArrayBuffer.prototype[@@toStringTag] = "ArrayBuffer"`.
pub fn install_array_buffer_well_knowns_post_bootstrap(
    heap: &mut otter_gc::GcHeap,
    string_heap: &crate::string::StringHeap,
    global: JsObject,
    well_known: &crate::symbol::WellKnownSymbols,
) -> Result<(), JsSurfaceError> {
    use crate::symbol::WellKnown;

    let Some(Value::NativeFunction(ctor)) = object::get(global, heap, "ArrayBuffer") else {
        return Ok(());
    };
    let descriptor = ctor
        .own_property_descriptor(heap, string_heap, "prototype")
        .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let prototype = match descriptor.and_then(|d| match d.kind {
        crate::object::DescriptorKind::Data {
            value: Value::Object(p),
        } => Some(p),
        _ => None,
    }) {
        Some(p) => p,
        None => return Ok(()),
    };
    let tag = crate::string::JsString::from_str("ArrayBuffer", string_heap)
        .map_err(|_| JsSurfaceError::OutOfMemory)?;
    object::define_own_symbol_property_partial(
        prototype,
        heap,
        &well_known.get(WellKnown::ToStringTag),
        PartialPropertyDescriptor {
            value: Some(Value::String(tag)),
            writable: Some(false),
            enumerable: Some(false),
            configurable: Some(true),
            ..Default::default()
        },
    );
    Ok(())
}

// ---------------------------------------------------------------
// Constructor + statics
// ---------------------------------------------------------------

fn ab_ctor_call(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    if !ctx.is_construct_call() {
        return Err(NativeError::TypeError {
            name: "ArrayBuffer",
            reason: "constructor requires 'new'".to_string(),
        });
    }
    let roots = ctx.collect_native_roots();
    let this_value = ctx.this_value().clone();
    let new_target = ctx.new_target().cloned();
    let mut external_visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
        crate::runtime_cx::visit_native_roots(
            visitor,
            &roots,
            &this_value,
            new_target.as_ref(),
            &[],
            &[args],
        );
    };
    let value = dispatch::array_buffer_call_with_roots(
        ArrayBufferMethod::Construct,
        args,
        ctx.heap_mut(),
        &mut external_visit,
    )
    .map_err(|e| vm_to_native(e, "ArrayBuffer"))?;
    // §10.1.13 GetPrototypeFromConstructor — derived `super()`
    // construction forwards `new.target`, so the allocated exotic
    // receives `Subclass.prototype` as its observable [[Prototype]].
    // <https://tc39.es/ecma262/#sec-getprototypefromconstructor>
    let needs_proto_override = !matches!(ctx.new_target(), Some(Value::NativeFunction(_)));
    if needs_proto_override
        && let Some(proto) = crate::bootstrap::native_new_target_prototype(ctx, "ArrayBuffer")?
    {
        ctx.interp_mut()
            .set_non_gc_exotic_prototype_override(&value, Some(proto));
    }
    Ok(value)
}

fn ab_is_view(_ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    dispatch::array_buffer_call(ArrayBufferMethod::IsView, args, _ctx.heap())
        .map_err(|e| vm_to_native(e, "ArrayBuffer.isView"))
}

// ---------------------------------------------------------------
// Prototype methods
// ---------------------------------------------------------------

fn ab_slice(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    dispatch_method(ctx, args, "slice")
}
fn ab_resize(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    dispatch_method(ctx, args, "resize")
}
fn ab_transfer(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    dispatch_method(ctx, args, "transfer")
}
fn ab_transfer_to_fixed_length(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    dispatch_method(ctx, args, "transferToFixedLength")
}

fn dispatch_method(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    method_name: &str,
) -> Result<Value, NativeError> {
    let entry =
        array_buffer_prototype::lookup(method_name).ok_or_else(|| NativeError::TypeError {
            name: "ArrayBuffer.prototype",
            reason: format!("method {method_name} missing"),
        })?;
    let receiver = ctx.this_value().clone();
    let small_args: SmallVec<[Value; 4]> = args.iter().cloned().collect();
    let (string_heap, allocation_roots) = {
        let interp = ctx.interp_mut();
        (interp.string_heap_clone(), interp.collect_runtime_roots())
    };
    let gc_heap = ctx.heap_mut();
    let mut intrinsic_args = IntrinsicArgs {
        receiver: &receiver,
        args: &small_args,
        string_heap: &string_heap,
        gc_heap,
        allocation_roots: allocation_roots.as_slice(),
    };
    (entry.impl_fn)(&mut intrinsic_args).map_err(|e| NativeError::TypeError {
        name: "ArrayBuffer.prototype",
        reason: e.to_string(),
    })
}

// ---------------------------------------------------------------
// Accessors
// ---------------------------------------------------------------

fn install_accessor(
    heap: &mut otter_gc::GcHeap,
    prototype: JsObject,
    name: &'static str,
    call: crate::native_function::NativeFastFn,
    value_roots: &[&Value],
) -> Result<(), JsSurfaceError> {
    let prototype_root = Value::Object(prototype);
    let mut roots = Vec::with_capacity(value_roots.len() + 1);
    roots.push(&prototype_root);
    roots.extend_from_slice(value_roots);
    let getter = native_static_with_value_roots(heap, name, 0, call, roots.as_slice())
        .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let desc = PropertyDescriptor::accessor(Some(Value::NativeFunction(getter)), None, false, true);
    if !object::define_own_property(prototype, heap, name, desc) {
        return Err(JsSurfaceError::DefinePropertyFailed(name));
    }
    Ok(())
}

fn ab_byte_length(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let b = receiver_ab(ctx, "get ArrayBuffer.prototype.byteLength")?;
    if b.is_detached() {
        return Ok(Value::Number(NumberValue::from_i32(0)));
    }
    Ok(Value::Number(NumberValue::from_i32(b.byte_length() as i32)))
}

fn ab_max_byte_length(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let b = receiver_ab(ctx, "get ArrayBuffer.prototype.maxByteLength")?;
    Ok(Value::Number(NumberValue::from_i32(
        b.max_byte_length() as i32
    )))
}

fn ab_resizable(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let b = receiver_ab(ctx, "get ArrayBuffer.prototype.resizable")?;
    Ok(Value::Boolean(b.is_resizable()))
}

fn ab_detached(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let b = receiver_ab(ctx, "get ArrayBuffer.prototype.detached")?;
    Ok(Value::Boolean(b.is_detached()))
}

fn receiver_ab(
    ctx: &NativeCtx<'_>,
    name: &'static str,
) -> Result<crate::binary::array_buffer::JsArrayBuffer, NativeError> {
    match ctx.this_value() {
        Value::ArrayBuffer(b) => Ok(b.clone()),
        _ => Err(NativeError::TypeError {
            name,
            reason: "this is not an ArrayBuffer".to_string(),
        }),
    }
}

fn vm_to_native(err: VmError, name: &'static str) -> NativeError {
    match err {
        VmError::TypeError { message } => NativeError::TypeError {
            name,
            reason: message,
        },
        VmError::TypeMismatch => NativeError::TypeError {
            name,
            reason: "type mismatch".to_string(),
        },
        VmError::RangeError { message } => NativeError::RangeError {
            name,
            reason: message,
        },
        other => NativeError::TypeError {
            name,
            reason: other.to_string(),
        },
    }
}
