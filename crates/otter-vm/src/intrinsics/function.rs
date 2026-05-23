//! `%Function%` constructor installer.
//!
//! Implements ECMA-262 §20.2 Function Objects: the `Function()`
//! constructor and `Function.prototype` wiring (excluding
//! `apply/call/bind`, which live in `function_prototype.rs`).
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-function-objects>

use crate::bootstrap::{
    BootstrapFeatures, alloc_object_with_value_roots, define_global,
    native_constructor_static_with_value_roots, native_new_target_prototype,
    native_static_with_value_roots,
};
use crate::function_prototype;
use crate::intrinsic_install::BuiltinIntrinsic;
use crate::js_surface::{JsSurfaceError, ObjectBuilder};
use crate::object::{self, JsObject, PropertyDescriptor};
use crate::Value;

fn install_function(heap: &mut otter_gc::GcHeap, global: JsObject) -> Result<(), JsSurfaceError> {
    use crate::{NativeCtx, NativeError};

    fn function_prototype_call(
        _ctx: &mut NativeCtx<'_>,
        _args: &[Value],
    ) -> Result<Value, NativeError> {
        Ok(Value::undefined())
    }

    fn function_ctor_call(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
        let new_target_proto = native_new_target_prototype(ctx, "Function")?;
        let (interp, context) = ctx.interp_mut_and_context();
        let Some(context) = context else {
            return Err(NativeError::TypeError {
                name: "Function",
                reason: "missing execution context for Function constructor".to_string(),
            });
        };
        let result = interp
            .build_function_constructor_with_roots(&context, args, None, &[], &[args])
            .map_err(|err| {
                let reason = format!("{err}");
                match err {
                    crate::VmError::SyntaxError { .. } => NativeError::SyntaxError {
                        name: "Function",
                        reason,
                    },
                    _ => NativeError::TypeError {
                        name: "Function",
                        reason,
                    },
                }
            })?;
        if let (Some(native), Some(proto)) = (result.as_native_function(), new_target_proto) {
            native.set_prototype_override(interp.gc_heap_mut(), Some(proto));
        }
        Ok(result)
    }

    let global_root = Value::object(global);
    let function = alloc_object_with_value_roots(heap, &[&global_root])?;
    let function_root = Value::object(function);
    let prototype = alloc_object_with_value_roots(heap, &[&global_root, &function_root])?;
    let prototype_root = Value::object(prototype);
    if let Some(object_ctor) = object::get(global, heap, "Object").and_then(|v| v.as_object())
        && let Some(object_proto) =
            object::get(object_ctor, heap, "prototype").and_then(|v| v.as_object())
    {
        object::set_prototype(prototype, heap, Some(object_proto));
    }
    object::set_prototype(function, heap, Some(prototype));
    let ctor_native = native_constructor_static_with_value_roots(
        heap,
        "Function",
        1,
        function_ctor_call,
        &[&global_root, &function_root, &prototype_root],
    )
    .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let ctor_native_root = Value::native_function(ctor_native);
    object::set_constructor_native(function, heap, ctor_native_root);
    let prototype_call = native_static_with_value_roots(
        heap,
        "",
        0,
        function_prototype_call,
        &[
            &global_root,
            &function_root,
            &prototype_root,
            &ctor_native_root,
        ],
    )
    .map_err(|_| JsSurfaceError::OutOfMemory)?;
    object::set_call_native(prototype, heap, Value::native_function(prototype_call));
    let length = PropertyDescriptor::data(Value::number_i32(1), false, false, true);
    let _ = object::define_own_property(function, heap, "length", length);
    let name_value = Value::string(
        crate::string::JsString::from_str("Function", heap)
            .map_err(|_| JsSurfaceError::OutOfMemory)?,
    );
    let name = PropertyDescriptor::data(name_value, false, false, true);
    let _ = object::define_own_property(function, heap, "name", name);
    let prototype_descriptor =
        PropertyDescriptor::data(Value::object(prototype), false, false, false);
    let _ = object::define_own_property(function, heap, "prototype", prototype_descriptor);
    let prototype_length = PropertyDescriptor::data(Value::number_i32(0), false, false, true);
    let _ = object::define_own_property(prototype, heap, "length", prototype_length);
    let prototype_name_value = Value::string(
        crate::string::JsString::from_str("", heap).map_err(|_| JsSurfaceError::OutOfMemory)?,
    );
    let prototype_name = PropertyDescriptor::data(prototype_name_value, false, false, true);
    let _ = object::define_own_property(prototype, heap, "name", prototype_name);
    {
        let mut builder = ObjectBuilder::from_object_with_value_roots(
            heap,
            prototype,
            vec![global_root, function_root],
        );
        for method in function_prototype::FUNCTION_PROTOTYPE_METHODS {
            builder.method_from_spec(method)?;
        }
    }
    function_prototype::install_restricted_accessors(
        heap,
        prototype,
        &[&global_root, &function_root],
    )?;
    let constructor = PropertyDescriptor::data(Value::object(function), true, false, true);
    let _ = object::define_own_property(prototype, heap, "constructor", constructor);
    define_global(global, heap, "Function", Value::object(function));
    Ok(())
}

// `Math` installer migrated to [`crate::math::Intrinsic`].
// `JSON` installer migrated to [`crate::json::Intrinsic`].



/// `BuiltinIntrinsic` adapter for the global `Function` constructor.
pub struct Intrinsic;

impl BuiltinIntrinsic for Intrinsic {
    const NAME: &'static str = "Function";
    const FEATURE: BootstrapFeatures = BootstrapFeatures::CORE;
    fn install(heap: &mut otter_gc::GcHeap, global: JsObject) -> Result<(), JsSurfaceError> {
        install_function(heap, global)
    }
}
