//! Proxy constructor implementation (ES2026)
//!
//! ## Constructor:
//! - `new Proxy(target, handler)` — §28.2.1
//!
//! ## Static methods:
//! - `Proxy.revocable(target, handler)` — §28.2.2.1

use std::sync::Arc;

use crate::error::VmError;
use crate::gc::GcRef;
use crate::memory::MemoryManager;
use crate::object::{JsObject, PropertyDescriptor, PropertyKey};
use crate::proxy::JsProxy;
use crate::string::JsString;
use crate::value::Value;

fn is_object_like(value: &Value) -> bool {
    value.as_object().is_some() || value.as_proxy().is_some()
}

/// Initialize the Proxy constructor and its static methods
///
/// The Proxy constructor is special - it doesn't have a prototype chain for instances.
/// Proxy objects are exotic and their behavior is entirely determined by the handler.
pub fn init_proxy_constructor(
    proxy_ctor: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
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

    // ====================================================================
    // Proxy.revocable(target, handler) — §28.2.2.1
    // ====================================================================
    // Returns { proxy, revoke } where revoke is a function that revokes the proxy
    let revocable_fn = Value::native_function_with_proto(
        move |_this_val, args, ncx| {
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

            // Create result object { proxy, revoke }
            let result = GcRef::new(JsObject::new(None, ncx.memory_manager().clone()));
            result.set("proxy".into(), Value::proxy(revocable.proxy));

            // Create revoke function
            let revoke_fn = revocable.revoke;
            let revoke_value = Value::native_function_with_proto(
                move |_this: &Value, _args: &[Value], _ncx: &mut crate::context::NativeContext<'_>| {
                    revoke_fn();
                    Ok(Value::undefined())
                },
                ncx.memory_manager().clone(),
                fn_proto.clone(),
            );
            if let Some(revoke_obj) = revoke_value.as_object() {
                revoke_obj.define_property(
                    PropertyKey::string("__non_constructor"),
                    PropertyDescriptor::builtin_data(Value::boolean(true)),
                );
            }
            result.set("revoke".into(), revoke_value);

            Ok(Value::object(result))
        },
        mm.clone(),
        fn_proto.clone(),
    );
    if let Some(obj) = revocable_fn.as_object() {
        obj.define_property(
            PropertyKey::string("length"),
            PropertyDescriptor::function_length(Value::number(2.0)),
        );
        obj.define_property(
            PropertyKey::string("name"),
            PropertyDescriptor::function_length(Value::string(JsString::intern("revocable"))),
        );
        obj.define_property(
            PropertyKey::string("__non_constructor"),
            PropertyDescriptor::builtin_data(Value::boolean(true)),
        );
    }
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
