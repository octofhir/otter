//! `Boolean` built-in installer.
//!
//! Routes through `couch!`. Boolean is callable-only (no
//! `[[Construct]]` slot per §20.3.1.1 — `new Boolean(x)` and bare
//! `Boolean(x)` both dispatch into `boolean_ctor_call`, which does
//! the construct/call split itself). The prototype carries
//! `toString` / `valueOf` (from `BOOLEAN_PROTOTYPE_METHODS`) and a
//! `[[BooleanData]]` slot of `false` (§20.3.4), pinned by the
//! `post_install` hook.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-boolean-objects>
//! - <https://tc39.es/ecma262/#sec-boolean-constructor>

use crate::js_surface::JsSurfaceError;
use crate::object::{self, JsObject};
use crate::{NativeCtx, NativeError, Value};

otter_macros::couch! {
    name = "Boolean",
    feature = CORE,
    constructor = (length = 1, call = boolean_ctor_call),
    prototype = {
        method_specs = [super::prototype::BOOLEAN_PROTOTYPE_METHODS],
    },
    post_install = pin_boolean_data,
}

/// §20.3.4 — `Boolean.prototype` is itself a Boolean object whose
/// `[[BooleanData]]` is `false`. Pinned after `couch!` creates the
/// prototype so the slot exists for `Boolean.prototype.valueOf` /
/// `toString`.
fn pin_boolean_data(
    heap: &mut otter_gc::GcHeap,
    _global: JsObject,
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
    object::set_boolean_data(prototype, heap, false);
    Ok(())
}

/// `Boolean(value)` / `new Boolean(value)` — §20.3.1.
fn boolean_ctor_call(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let value = args.first().is_some_and(|v| v.to_boolean(ctx.heap()));
    if ctx.is_construct_call() {
        let this = *ctx.this_value();
        if let Some(obj) = this.as_object() {
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
