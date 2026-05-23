//! `%Object%` constructor installer.
//!
//! Implements ECMA-262 §20.1 Object Objects: the `Object()` constructor,
//! every static reflection helper, and the wiring that makes
//! `Object.prototype` reachable as `%Object.prototype%` for downstream
//! intrinsic installers.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-object-constructor>

use crate::bootstrap::{
    BootstrapFeatures, alloc_object_with_value_roots, define_global, native_static_with_value_roots,
};
use crate::intrinsic_install::BuiltinIntrinsic;
use crate::js_surface::{JsSurfaceError, ObjectBuilder};
use crate::number::NumberValue;
use crate::object::{self, JsObject, PropertyDescriptor};
use crate::{Value, object_statics};

fn install_object(heap: &mut otter_gc::GcHeap, global: JsObject) -> Result<(), JsSurfaceError> {
    use crate::{NativeCtx, NativeError};

    /// §20.1.1.1 Object ( [ value ] ).
    ///
    /// 1. If `NewTarget` is neither `undefined` nor the active
    ///    `Object` function, return `OrdinaryCreateFromConstructor(NewTarget,
    ///    %Object.prototype%)`. (Subclass path — `class C extends Object {}`.)
    /// 2. If `value` is `undefined` or `null`, return
    ///    `OrdinaryObjectCreate(%Object.prototype%)`.
    /// 3. Return `! ToObject(value)`.
    ///
    /// `ToObject(value)` wraps a primitive with the appropriate
    /// `[[BooleanData]]` / `[[NumberData]]` / `[[StringData]]` /
    /// `[[SymbolData]]` / `[[BigIntData]]` slot so the wrapper's
    /// inherited `toString` / `valueOf` observe the original value.
    /// Object-typed operands return as-is.
    ///
    /// <https://tc39.es/ecma262/#sec-object-value>
    fn object_ctor_call(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
        if ctx.is_construct_call() && !ctx.new_target().is_some_and(|v| v.is_object()) {
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
        // §7.1.18 ToObject — wrap a primitive with its %X.prototype%
        // and the matching internal data slot. Object-typed operands
        // fall through and return unchanged.
        let v = *value;
        if let Some(b) = v.as_boolean() {
            let interp = ctx.interp_mut();
            let proto = interp
                .primitive_wrapper_prototype("Boolean")
                .map_err(|err| NativeError::TypeError {
                    name: "Object",
                    reason: err.to_string(),
                })?;
            let obj = interp
                .alloc_runtime_rooted_object_with_proto(proto, &[&v], &[])
                .map_err(|err| NativeError::TypeError {
                    name: "Object",
                    reason: err.to_string(),
                })?;
            crate::object::set_boolean_data(obj, &mut interp.gc_heap, b);
            return Ok(Value::object(obj));
        }
        if let Some(n) = v.as_number() {
            let interp = ctx.interp_mut();
            let proto = interp
                .primitive_wrapper_prototype("Number")
                .map_err(|err| NativeError::TypeError {
                    name: "Object",
                    reason: err.to_string(),
                })?;
            let obj = interp
                .alloc_runtime_rooted_object_with_proto(proto, &[&v], &[])
                .map_err(|err| NativeError::TypeError {
                    name: "Object",
                    reason: err.to_string(),
                })?;
            crate::object::set_number_data(obj, &mut interp.gc_heap, n);
            return Ok(Value::object(obj));
        }
        if let Some(s) = v.as_string(ctx.heap()) {
            let interp = ctx.interp_mut();
            let proto = interp
                .primitive_wrapper_prototype("String")
                .map_err(|err| NativeError::TypeError {
                    name: "Object",
                    reason: err.to_string(),
                })?;
            let obj = interp
                .alloc_runtime_rooted_object_with_proto(proto, &[&v], &[])
                .map_err(|err| NativeError::TypeError {
                    name: "Object",
                    reason: err.to_string(),
                })?;
            crate::object::set_string_data(obj, &mut interp.gc_heap, s);
            return Ok(Value::object(obj));
        }
        if let Some(sym) = v.as_symbol(ctx.heap()) {
            let interp = ctx.interp_mut();
            let proto = interp
                .primitive_wrapper_prototype("Symbol")
                .map_err(|err| NativeError::TypeError {
                    name: "Object",
                    reason: err.to_string(),
                })?;
            let obj = interp
                .alloc_runtime_rooted_object_with_proto(proto, &[&v], &[])
                .map_err(|err| NativeError::TypeError {
                    name: "Object",
                    reason: err.to_string(),
                })?;
            crate::object::set_symbol_data(obj, &mut interp.gc_heap, sym);
            return Ok(Value::object(obj));
        }
        if let Some(bigint) = v.as_big_int() {
            let interp = ctx.interp_mut();
            let proto = interp
                .primitive_wrapper_prototype("BigInt")
                .map_err(|err| NativeError::TypeError {
                    name: "Object",
                    reason: err.to_string(),
                })?;
            let obj = interp
                .alloc_runtime_rooted_object_with_proto(proto, &[&v], &[])
                .map_err(|err| NativeError::TypeError {
                    name: "Object",
                    reason: err.to_string(),
                })?;
            crate::object::set_bigint_data(obj, &mut interp.gc_heap, bigint);
            return Ok(Value::object(obj));
        }
        Ok(v)
    }

    let global_root = Value::object(global);
    let object = alloc_object_with_value_roots(heap, &[&global_root])?;
    let object_root = Value::object(object);
    let prototype = alloc_object_with_value_roots(heap, &[&global_root, &object_root])?;
    let prototype_root = Value::object(prototype);
    let ctor_native = native_static_with_value_roots(
        heap,
        "Object",
        1,
        object_ctor_call,
        &[&global_root, &object_root, &prototype_root],
    )
    .map_err(|_| JsSurfaceError::OutOfMemory)?;
    object::set_constructor_native(object, heap, Value::native_function(ctor_native));
    let length_desc =
        PropertyDescriptor::data(Value::number(NumberValue::from_i32(1)), false, false, true);
    if !object::define_own_property(object, heap, "length", length_desc) {
        return Err(JsSurfaceError::DefinePropertyFailed("length"));
    }
    let name_value =
        crate::JsString::from_latin1(b"Object", heap).map_err(|_| JsSurfaceError::OutOfMemory)?;
    let name_desc = PropertyDescriptor::data(Value::string(name_value), false, false, true);
    if !object::define_own_property(object, heap, "name", name_desc) {
        return Err(JsSurfaceError::DefinePropertyFailed("name"));
    }
    let prototype_desc = PropertyDescriptor::data(Value::object(prototype), false, false, false);
    if !object::define_own_property(object, heap, "prototype", prototype_desc) {
        return Err(JsSurfaceError::DefinePropertyFailed("prototype"));
    }
    {
        let mut builder = ObjectBuilder::from_object_with_value_roots(
            heap,
            object,
            vec![global_root, prototype_root],
        );
        for method in object_statics::OBJECT_SPEC.methods {
            builder.method_from_spec(method)?;
        }
    }
    {
        let mut builder =
            ObjectBuilder::from_object_with_value_roots(heap, prototype, vec![global_root]);
        for method in object_statics::OBJECT_PROTOTYPE_METHODS {
            builder.method_from_spec(method)?;
        }
    }
    // §B.2.2.1 Object.prototype.__proto__ — accessor pair.
    // <https://tc39.es/ecma262/#sec-object.prototype.__proto__>
    {
        let proto_root = Value::object(prototype);
        let getter = native_static_with_value_roots(
            heap,
            "get __proto__",
            0,
            object_statics::native_prototype_proto_get,
            &[&global_root, &proto_root],
        )
        .map_err(|_| JsSurfaceError::OutOfMemory)?;
        let setter = native_static_with_value_roots(
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
    }
    let _ = object::define_own_property(
        prototype,
        heap,
        "constructor",
        PropertyDescriptor::data(Value::object(object), true, false, true),
    );
    define_global(global, heap, "Object", Value::object(object));
    Ok(())
}

/// `BuiltinIntrinsic` adapter for the global `Object` constructor.
pub struct Intrinsic;

impl BuiltinIntrinsic for Intrinsic {
    const NAME: &'static str = "Object";
    const FEATURE: BootstrapFeatures = BootstrapFeatures::CORE;
    fn install(heap: &mut otter_gc::GcHeap, global: JsObject) -> Result<(), JsSurfaceError> {
        install_object(heap, global)
    }
}
