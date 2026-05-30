//! `%Object%` constructor installer.
//!
//! Routes through `couch!`. Static reflection helpers come from
//! `OBJECT_SPEC.methods` (also consumed by the `Op::CallMethod`
//! native dispatch fast path) via `static_method_specs`; prototype
//! methods come from `OBJECT_PROTOTYPE_METHODS` via the prototype
//! `method_specs` field. The §B.2.2.1 `__proto__` accessor pair is
//! pinned via `post_install` (`couch!` accessor rows take static
//! get/set fn paths, but the __proto__ getter / setter need their
//! roots wired through `native_static_with_value_roots` to keep the
//! prototype alive across allocator GCs — same shape as RegExp's
//! legacy static accessors).
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-object-constructor>

use crate::js_surface::JsSurfaceError;
use crate::object::{self, JsObject, PropertyDescriptor};
use crate::{NativeCtx, NativeError, Value, object_statics};

otter_macros::couch! {
    name = "Object",
    feature = CORE,
    constructor = (length = 1, call = object_ctor_call),
    static_method_specs = [object_statics::OBJECT_STATIC_METHODS],
    prototype = {
        method_specs = [object_statics::OBJECT_PROTOTYPE_METHODS],
    },
    post_install = install_proto_proto_accessor,
}

/// §B.2.2.1 `Object.prototype.__proto__` — accessor pair. Pinned in
/// post_install because the getter / setter need the prototype value
/// passed through `value_roots` so the closure metadata keeps the
/// prototype alive across allocator GCs.
///
/// <https://tc39.es/ecma262/#sec-object.prototype.__proto__>
fn install_proto_proto_accessor(
    heap: &mut otter_gc::GcHeap,
    global: JsObject,
    ctor: crate::native_function::NativeFunction,
) -> Result<(), JsSurfaceError> {
    let descriptor = ctor
        .own_property_descriptor(heap, "prototype")
        .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let prototype = match descriptor.and_then(|d| match d.kind {
        crate::object::DescriptorKind::Data { value } => value.as_object(),
        _ => None,
    }) {
        Some(p) => p,
        None => return Ok(()),
    };

    let global_root = Value::object(global);
    let proto_root = Value::object(prototype);
    let getter = crate::bootstrap::native_static_with_value_roots(
        heap,
        "get __proto__",
        0,
        object_statics::native_prototype_proto_get,
        &[&global_root, &proto_root],
    )
    .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let setter = crate::bootstrap::native_static_with_value_roots(
        heap,
        "set __proto__",
        1,
        object_statics::native_prototype_proto_set,
        &[&global_root, &proto_root],
    )
    .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let desc = PropertyDescriptor::accessor(
        Some(Value::native_function(getter)),
        Some(Value::native_function(setter)),
        false,
        true,
    );
    if !object::define_own_property(prototype, heap, "__proto__", desc) {
        return Err(JsSurfaceError::DefinePropertyFailed("__proto__"));
    }
    Ok(())
}

/// §20.1.1.1 Object ( [ value ] ).
///
/// 1. If `NewTarget` is neither `undefined` nor the active `Object`
///    function, return `OrdinaryCreateFromConstructor(NewTarget,
///    %Object.prototype%)` (subclass path — `class C extends Object {}`).
/// 2. If `value` is `undefined` or `null`, return
///    `OrdinaryObjectCreate(%Object.prototype%)`.
/// 3. Return `! ToObject(value)`.
///
/// <https://tc39.es/ecma262/#sec-object-value>
fn object_ctor_call(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    // §20.1.1.1 step 1 — when `NewTarget` is neither undefined nor
    // the active `Object` function (i.e. `class C extends Object {};
    // new C(...)`), the dispatcher has already produced `this` via
    // `OrdinaryCreateFromConstructor(NewTarget, "%Object.prototype%")`
    // and we hand it back unchanged. Self-construction (`new
    // Object(value)`) falls through to steps 2–3 so primitive
    // arguments get wrapped in their `%X.prototype%` body instead of
    // being silently dropped.
    if ctx.is_construct_call() && !new_target_is_self(ctx) {
        return Ok(*ctx.this_value());
    }
    let first_is_nullish = args.first().is_none_or(|v| v.is_nullish());
    if first_is_nullish {
        let obj = ctx.alloc_object().map_err(|_| NativeError::TypeError {
            name: "Object",
            reason: "object allocation failed".to_string(),
        })?;
        let interp = ctx.interp_mut();
        if let Some(proto) = interp
            .constructor_prototype_value("Object")
            .ok()
            .and_then(|v| v.as_object())
        {
            crate::object::set_prototype(obj, &mut interp.gc_heap, Some(proto));
        }
        return Ok(Value::object(obj));
    }
    let Some(value) = args.first() else {
        unreachable!("first_is_nullish covers None path");
    };
    // §7.1.18 ToObject — wrap a primitive with its %X.prototype% and
    // the matching internal data slot. Object-typed operands fall
    // through and return unchanged.
    let v = *value;
    if let Some(b) = v.as_boolean() {
        return wrap_primitive(ctx, "Boolean", v, |obj, heap| {
            crate::object::set_boolean_data(obj, heap, b);
        });
    }
    if let Some(n) = v.as_number() {
        return wrap_primitive(ctx, "Number", v, |obj, heap| {
            crate::object::set_number_data(obj, heap, n);
        });
    }
    if let Some(s) = v.as_string(ctx.heap()) {
        return wrap_primitive(ctx, "String", v, |obj, heap| {
            crate::object::set_string_data(obj, heap, s);
        });
    }
    if let Some(sym) = v.as_symbol(ctx.heap()) {
        return wrap_primitive(ctx, "Symbol", v, |obj, heap| {
            crate::object::set_symbol_data(obj, heap, sym);
        });
    }
    if let Some(bigint) = v.as_big_int() {
        return wrap_primitive(ctx, "BigInt", v, |obj, heap| {
            crate::object::set_bigint_data(obj, heap, bigint);
        });
    }
    Ok(v)
}

/// `true` when this constructor call's `new.target` is the active
/// `Object` function itself (per §20.1.1.1 step 1's "active function
/// object" check). The comparison goes through `globalThis.Object`,
/// which is the same `NativeFunction` value `couch!` installed at
/// bootstrap.
fn new_target_is_self(ctx: &mut NativeCtx<'_>) -> bool {
    let Some(new_target) = ctx.new_target().copied() else {
        return false;
    };
    let interp = ctx.interp_mut();
    let Some(self_ctor) = crate::object::get(interp.global_this, &interp.gc_heap, "Object") else {
        return false;
    };
    new_target == self_ctor
}

fn wrap_primitive<F>(
    ctx: &mut NativeCtx<'_>,
    wrapper_name: &'static str,
    value: Value,
    apply_data_slot: F,
) -> Result<Value, NativeError>
where
    F: FnOnce(JsObject, &mut otter_gc::GcHeap),
{
    let interp = ctx.interp_mut();
    let proto = interp
        .primitive_wrapper_prototype(wrapper_name)
        .map_err(|err| NativeError::TypeError {
            name: "Object",
            reason: err.to_string(),
        })?;
    let obj = interp
        .alloc_runtime_rooted_object_with_proto(proto, &[&value], &[])
        .map_err(|err| NativeError::TypeError {
            name: "Object",
            reason: err.to_string(),
        })?;
    apply_data_slot(obj, &mut interp.gc_heap);
    Ok(Value::object(obj))
}
