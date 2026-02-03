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
    proxy_ctor.define_property(
        PropertyKey::string("revocable"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            move |_this_val, args, ncx| {
                let target = args
                    .first()
                    .ok_or_else(|| VmError::type_error("Proxy.revocable requires a target argument"))?;
                let handler = args
                    .get(1)
                    .ok_or_else(|| VmError::type_error("Proxy.revocable requires a handler argument"))?;

                // Validate target is an object
                let target_obj = target
                    .as_object()
                    .ok_or_else(|| VmError::type_error("Proxy target must be an object"))?;

                // Validate handler is an object
                let handler_obj = handler
                    .as_object()
                    .ok_or_else(|| VmError::type_error("Proxy handler must be an object"))?;

                let revocable = JsProxy::revocable(target_obj, handler_obj);

                // Create result object { proxy, revoke }
                let result = GcRef::new(JsObject::new(None, ncx.memory_manager().clone()));
                result.set("proxy".into(), Value::proxy(revocable.proxy));

                // Create revoke function
                let revoke_fn = revocable.revoke;
                result.set(
                    "revoke".into(),
                    Value::native_function(
                        move |_this: &Value, _args: &[Value], _ncx: &mut crate::context::NativeContext<'_>| {
                            revoke_fn();
                            Ok(Value::undefined())
                        },
                        ncx.memory_manager().clone(),
                    ),
                );

                Ok(Value::object(result))
            },
            mm.clone(),
            fn_proto,
        )),
    );
}

/// Create a Proxy constructor function
///
/// This is called when `new Proxy(target, handler)` is executed.
/// It validates the arguments and creates a new proxy.
pub fn proxy_constructor(
    _this_val: &Value,
    args: &[Value],
    _ncx: &mut crate::context::NativeContext<'_>,
) -> Result<Value, VmError> {
    let target = args
        .first()
        .ok_or_else(|| VmError::type_error("Proxy constructor requires a target argument"))?;
    let handler = args
        .get(1)
        .ok_or_else(|| VmError::type_error("Proxy constructor requires a handler argument"))?;

    // Validate target is an object (not null/undefined/primitive)
    let target_obj = target
        .as_object()
        .ok_or_else(|| VmError::type_error("Proxy target must be an object"))?;

    // Validate handler is an object (not null/undefined/primitive)
    let handler_obj = handler
        .as_object()
        .ok_or_else(|| VmError::type_error("Proxy handler must be an object"))?;

    let proxy = JsProxy::new(target_obj, handler_obj);
    Ok(Value::proxy(proxy))
}
