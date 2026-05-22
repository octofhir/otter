//! `Boolean` built-in installer.
//!
//! Owns the full installation of the global `Boolean` constructor:
//! prototype object with the `[[BooleanData]]` slot, prototype
//! methods (`toString`, `valueOf`), prototype chain to
//! `Object.prototype`, and the call/construct bridge for the
//! `Boolean(...)` / `new Boolean(...)` surface.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-boolean-objects>
//! - <https://tc39.es/ecma262/#sec-boolean-constructor>

use crate::bootstrap::{
    BootstrapFeatures, alloc_object_with_value_roots, native_static_with_value_roots,
};
use crate::intrinsic_install::BuiltinIntrinsic;
use crate::js_surface::{JsSurfaceError, ObjectBuilder};
use crate::object::{self, JsObject};
use crate::{NativeCtx, NativeError, Value};

/// Zero-sized marker used to install the global `Boolean`
/// constructor through [`BuiltinIntrinsic`].
pub struct Intrinsic;

impl BuiltinIntrinsic for Intrinsic {
    const NAME: &'static str = "Boolean";
    const FEATURE: BootstrapFeatures = BootstrapFeatures::CORE;

    fn install(heap: &mut otter_gc::GcHeap, global: JsObject) -> Result<(), JsSurfaceError> {
        install(heap, global)
    }
}

fn install(heap: &mut otter_gc::GcHeap, global: JsObject) -> Result<(), JsSurfaceError> {
    let global_root = Value::object(global);
    let prototype = alloc_object_with_value_roots(heap, &[&global_root])?;
    {
        let mut builder =
            ObjectBuilder::from_object_with_value_roots(heap, prototype, vec![global_root]);
        for method in super::prototype::BOOLEAN_PROTOTYPE_METHODS {
            builder.method_from_spec(method)?;
        }
    }
    crate::object::set_boolean_data(prototype, heap, false);
    if let Some(Value::Object(object_ctor)) = object::get(global, heap, "Object")
        && let Some(Value::Object(object_proto)) = object::get(object_ctor, heap, "prototype")
    {
        object::set_prototype(prototype, heap, Some(object_proto));
    }

    let prototype_root = Value::object(prototype);
    let ctor_native = native_static_with_value_roots(
        heap,
        "Boolean",
        1,
        boolean_ctor_call,
        &[&global_root, &prototype_root],
    )
    .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let ctor_native_root = Value::native_function(ctor_native);
    let statics =
        alloc_object_with_value_roots(heap, &[&global_root, &prototype_root, &ctor_native_root])?;
    if let Some(Value::Object(object_ctor)) = object::get(global, heap, "Object")
        && let Some(Value::Object(object_proto)) = object::get(object_ctor, heap, "prototype")
    {
        object::set_prototype(statics, heap, Some(object_proto));
    }
    object::set_constructor_native(statics, heap, ctor_native_root);
    // §20.3.2.1 — `Boolean.prototype` is a non-writable, non-enumerable,
    // non-configurable data property.
    let _ = object::define_own_property(
        statics,
        heap,
        "prototype",
        crate::object::PropertyDescriptor::data(Value::object(prototype), false, false, false),
    );
    // §20.3.2 — `Boolean.length` is a non-writable, non-enumerable,
    // configurable data property whose value matches the constructor
    // declared formal-parameter count (1).
    let _ = object::define_own_property(
        statics,
        heap,
        "length",
        crate::object::PropertyDescriptor::data(
            Value::Number(crate::number::NumberValue::from_i32(1)),
            false,
            false,
            true,
        ),
    );
    // §20.3.2 — `Boolean.name` is `"Boolean"`, non-writable,
    // non-enumerable, configurable.
    let name_value = Value::string(
        crate::string::JsString::from_str("Boolean", heap)
            .map_err(|_| JsSurfaceError::OutOfMemory)?,
    );
    let _ = object::define_own_property(
        statics,
        heap,
        "name",
        crate::object::PropertyDescriptor::data(name_value, false, false, true),
    );
    let boolean_value = Value::object(statics);
    let _ = object::define_own_property(
        prototype,
        heap,
        "constructor",
        crate::object::PropertyDescriptor::data(boolean_value, true, false, true),
    );
    crate::bootstrap::define_global_value(
        global,
        heap,
        <Intrinsic as BuiltinIntrinsic>::NAME,
        boolean_value,
    );
    Ok(())
}

/// `Boolean(value)` / `new Boolean(value)` — §20.3.1.
///
/// The call form returns `ToBoolean(value)`. The construct form
/// wraps the receiver object's `[[BooleanData]]` slot with the
/// coerced value.
fn boolean_ctor_call(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let value = args.first().is_some_and(|v| v.to_boolean(ctx.heap()));
    if ctx.is_construct_call() {
        let this = *ctx.this_value();
        if let Value::Object(obj) = this {
            crate::object::set_boolean_data(obj, ctx.heap_mut(), value);
            Ok(Value::object(obj))
        } else {
            Err(NativeError::TypeError {
                name: "Boolean",
                reason: "expected object receiver in `new Boolean(...)`".to_string(),
            })
        }
    } else {
        Ok(Value::boolean(value))
    }
}
