//! `%Function%` constructor installer.
//!
//! Routes through `couch!`. The constructor body builds a fresh
//! function via `build_function_constructor_with_roots` and applies
//! the new-target prototype override (§20.2). Prototype methods come
//! from `FUNCTION_PROTOTYPE_METHODS` via the prototype `method_specs`
//! field. `post_install` wires up:
//! - the `[[Call]]` slot on `Function.prototype` so calling
//!   `Function.prototype()` returns `undefined` (§20.2.3),
//! - the §20.2.3 `length = 0` / `name = ""` properties on the
//!   prototype,
//! - the §AddRestrictedFunctionProperties `caller` / `arguments`
//!   accessor pair on the prototype (both routed to
//!   `%ThrowTypeError%`).
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-function-objects>

use crate::function_prototype;
use crate::js_surface::JsSurfaceError;
use crate::object::{self, JsObject, PropertyDescriptor};
use crate::{NativeCtx, NativeError, Value};

otter_macros::couch! {
    name = "Function",
    feature = CORE,
    constructor = (length = 1, call = function_ctor_call),
    prototype = {
        method_specs = [function_prototype::FUNCTION_PROTOTYPE_METHODS],
    },
    post_install = install_function_prototype_callable_and_accessors,
}

/// `Function(...)` / `new Function(...)` — ECMA-262 §20.2.1.1.
fn function_ctor_call(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let new_target_proto = crate::bootstrap::native_new_target_prototype(ctx, "Function")?;
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

/// Post-bootstrap fixup for §20.2.3 `%Function.prototype%`:
/// - Pin a `[[Call]]` slot on the prototype that returns `undefined`
///   so `Function.prototype()` is a legal no-op call site.
/// - Replace the `length` / `name` data properties (couch! defaults
///   would inherit from the ctor's `length = 1` / `name = "Function"`,
///   but the spec requires `length = 0` / `name = ""` on the
///   prototype itself).
/// - Install the `caller` and `arguments` accessor pair, both routed
///   to a single `%ThrowTypeError%` callable (§AddRestrictedFunctionProperties).
fn install_function_prototype_callable_and_accessors(
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
    let prototype_root = Value::object(prototype);
    let ctor_root = Value::native_function(ctor);
    let prototype_call = crate::bootstrap::native_static_with_value_roots(
        heap,
        "",
        0,
        function_prototype_call,
        &[&global_root, &prototype_root, &ctor_root],
    )
    .map_err(|_| JsSurfaceError::OutOfMemory)?;
    object::set_call_native(prototype, heap, Value::native_function(prototype_call));

    let prototype_length = PropertyDescriptor::data(Value::number_i32(0), false, false, true);
    object::define_own_property(prototype, heap, "length", prototype_length);
    let prototype_name_value = Value::string(
        crate::string::JsString::from_str("", heap).map_err(|_| JsSurfaceError::OutOfMemory)?,
    );
    let prototype_name = PropertyDescriptor::data(prototype_name_value, false, false, true);
    object::define_own_property(prototype, heap, "name", prototype_name);

    function_prototype::install_restricted_accessors(heap, prototype, &[&global_root, &ctor_root])
}

/// `Function.prototype()` — returns `undefined` per §20.2.3.
fn function_prototype_call(_ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    Ok(Value::undefined())
}
