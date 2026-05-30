//! `Boolean.prototype.*` native implementations.
//!
//! Boolean methods accept both primitive booleans and Boolean
//! wrapper objects. Wrapper objects carry their `[[BooleanData]]`
//! internal slot inside the object payload, not as a JS-visible own
//! property.
//!
//! # Contents
//! - [`BOOLEAN_PROTOTYPE_METHODS`] — native method specs installed
//!   on the global `Boolean.prototype`.
//! - One `fn(&mut NativeCtx, &[Value]) -> Result<Value, NativeError>`
//!   per method.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-properties-of-the-boolean-prototype-object>
use crate::Value;
use crate::js_surface::{Attr, MethodSpec};
use crate::string::JsString;
use crate::{NativeCall, NativeCtx, NativeError};

/// §20.3.3 `thisBooleanValue(value)` — unwrap a primitive boolean or
/// a Boolean wrapper's `[[BooleanData]]`; otherwise `TypeError`.
fn this_boolean_value(ctx: &NativeCtx<'_>, name: &'static str) -> Result<bool, NativeError> {
    let this = *ctx.this_value();
    if let Some(b) = this.as_boolean() {
        return Ok(b);
    }
    if let Some(obj) = this.as_object()
        && let Some(b) = crate::object::boolean_data(obj, ctx.heap())
    {
        return Ok(b);
    }
    Err(NativeError::TypeError {
        name,
        reason: "Boolean.prototype method called on incompatible receiver".to_string(),
    })
}

/// §20.3.3.2 `Boolean.prototype.toString()` — `"true"` / `"false"`.
fn boolean_to_string(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let s = if this_boolean_value(ctx, "Boolean.prototype.toString")? {
        "true"
    } else {
        "false"
    };
    Ok(Value::string(JsString::from_str(s, ctx.heap_mut())?))
}

/// §20.3.3.3 `Boolean.prototype.valueOf()` — the unwrapped boolean.
fn boolean_value_of(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    Ok(Value::boolean(this_boolean_value(
        ctx,
        "Boolean.prototype.valueOf",
    )?))
}

/// `MethodSpec` list installed on `Boolean.prototype` by the
/// `Boolean` `couch!` surface.
pub static BOOLEAN_PROTOTYPE_METHODS: &[MethodSpec] = &[
    method("toString", 0, boolean_to_string),
    method("valueOf", 0, boolean_value_of),
];

const fn method(
    name: &'static str,
    length: u8,
    call: for<'rt> fn(&mut NativeCtx<'rt>, &[Value]) -> Result<Value, NativeError>,
) -> MethodSpec {
    MethodSpec {
        name,
        length,
        attrs: Attr::builtin_function(),
        call: NativeCall::Static(call),
    }
}
