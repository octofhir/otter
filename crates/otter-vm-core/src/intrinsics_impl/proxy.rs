//! Proxy constructor implementation (ES2026)
//!
//! ## Constructor:
//! - `new Proxy(target, handler)` — §28.2.1
//!
//! ## Static methods:
//! - `Proxy.revocable(target, handler)` — §28.2.2.1

use std::sync::Arc;

use crate::context::NativeContext;
use crate::error::VmError;
use crate::gc::GcRef;
use crate::memory::MemoryManager;
use crate::object::{JsObject, PropertyDescriptor, PropertyKey};
use crate::proxy::JsProxy;
use crate::string::JsString;
use crate::value::Value;
use otter_macros::dive;

fn is_object_like(value: &Value) -> bool {
    value.as_object().is_some() || value.as_proxy().is_some()
}

#[dive(name = "revocable", length = 2)]
fn proxy_revocable(
    _this_val: &Value,
    args: &[Value],
    ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let target = args
        .first()
        .ok_or_else(|| VmError::type_error("Proxy.revocable requires a target argument"))?;
    let handler = args
        .get(1)
        .ok_or_else(|| VmError::type_error("Proxy.revocable requires a handler argument"))?;

    if !is_object_like(target) {
        return Err(VmError::type_error("Proxy target must be an object"));
    }
    if !is_object_like(handler) {
        return Err(VmError::type_error("Proxy handler must be an object"));
    }

    let revocable = JsProxy::revocable(target.clone(), handler.clone());

    let result = GcRef::new(JsObject::new(Value::null(), ncx.memory_manager().clone()));
    let _ = result.set("proxy".into(), Value::proxy(revocable.proxy));

    let revoke_fn = revocable.revoke.clone();
    let revoke_value = if let Some(fn_proto) = ncx.ctx.function_prototype() {
        let revoke_fn = revoke_fn.clone();
        Value::native_function_with_proto(
            move |_this: &Value, _args: &[Value], _ncx: &mut NativeContext<'_>| {
                revoke_fn();
                Ok(Value::undefined())
            },
            ncx.memory_manager().clone(),
            fn_proto,
        )
    } else {
        Value::native_function(
            move |_this: &Value, _args: &[Value], _ncx: &mut NativeContext<'_>| {
                revoke_fn();
                Ok(Value::undefined())
            },
            ncx.memory_manager().clone(),
        )
    };

    if let Some(revoke_obj) = revoke_value.as_object() {
        revoke_obj.define_property(
            PropertyKey::string("__non_constructor"),
            PropertyDescriptor::builtin_data(Value::boolean(true)),
        );
    }
    let _ = result.set("revoke".into(), revoke_value);

    Ok(Value::object(result))
}

/// Initialize the Proxy constructor and its static methods
///
/// The Proxy constructor is special - it doesn't have a prototype chain for instances.
/// Proxy objects are exotic and their behavior is entirely determined by the handler.
pub fn init_proxy_constructor(
    proxy_ctor: GcRef<JsObject>,
    _fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
) {
    // Proxy.length = 2 (target, handler)
    proxy_ctor.define_property(
        PropertyKey::string("length"),
        PropertyDescriptor::function_length(Value::number(2.0)),
    );

    // Proxy.name = "Proxy"
    proxy_ctor.define_property(
        PropertyKey::string("name"),
        PropertyDescriptor::function_length(Value::string(JsString::intern("Proxy"))),
    );

    // Proxy.revocable(target, handler) — §28.2.2.1
    let (name, native_fn, length) = proxy_revocable_decl();
    let revocable_fn = Value::native_function_from_decl(name, native_fn, length, mm.clone());
    proxy_ctor.define_property(
        PropertyKey::string("revocable"),
        PropertyDescriptor::builtin_method(revocable_fn),
    );
}

/// Create a Proxy constructor function
///
/// This is called when `new Proxy(target, handler)` is executed.
/// It validates the arguments and creates a new proxy.
pub fn proxy_constructor(
    _this_val: &Value,
    args: &[Value],
    ncx: &mut crate::context::NativeContext<'_>,
) -> Result<Value, VmError> {
    let _ = ncx;
    let target = args
        .first()
        .ok_or_else(|| VmError::type_error("Proxy constructor requires a target argument"))?;
    let handler = args
        .get(1)
        .ok_or_else(|| VmError::type_error("Proxy constructor requires a handler argument"))?;

    if !is_object_like(target) {
        return Err(VmError::type_error("Proxy target must be an object"));
    }
    if !is_object_like(handler) {
        return Err(VmError::type_error("Proxy handler must be an object"));
    }

    let proxy = JsProxy::new(target.clone(), handler.clone());
    Ok(Value::proxy(proxy))
}
