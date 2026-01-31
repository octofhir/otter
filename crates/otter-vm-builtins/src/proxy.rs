//! Proxy built-in
//!
//! Provides Proxy constructor and methods:
//! - `new Proxy(target, handler)`
//! - `Proxy.revocable(target, handler)`
//!
//! Proxy traps are called from JavaScript via the handler object.

use otter_vm_core::gc::GcRef;
use otter_vm_core::memory;
use otter_vm_core::object::JsObject;
use otter_vm_core::proxy::JsProxy;
use otter_vm_core::value::Value as VmValue;
use otter_vm_runtime::{Op, op_native_with_mm as op_native};
use std::sync::Arc;

/// Get Proxy ops for extension registration
pub fn ops() -> Vec<Op> {
    vec![
        op_native("__Proxy_create", native_proxy_create),
        op_native("__Proxy_revocable", native_proxy_revocable),
        op_native("__Proxy_getTarget", native_proxy_get_target),
        op_native("__Proxy_getHandler", native_proxy_get_handler),
        op_native("__Proxy_isRevoked", native_proxy_is_revoked),
    ]
}

// ============================================================================
// Native Operations
// ============================================================================

/// Create a new proxy
/// Args: [target, handler]
fn native_proxy_create(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, String> {
    let target = args.first().ok_or("Proxy requires a target argument")?;
    let handler = args.get(1).ok_or("Proxy requires a handler argument")?;

    // Validate target is an object (not null/undefined/primitive)
    let target_obj = target.as_object().ok_or("Proxy target must be an object")?;

    // Validate handler is an object (not null/undefined/primitive)
    let handler_obj = handler
        .as_object()
        .ok_or("Proxy handler must be an object")?;

    let proxy = JsProxy::new(target_obj, handler_obj);
    Ok(VmValue::proxy(proxy))
}

/// Create a revocable proxy
/// Args: [target, handler]
/// Returns: { proxy, revoke }
fn native_proxy_revocable(
    args: &[VmValue],
    mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, String> {
    let target = args
        .first()
        .ok_or("Proxy.revocable requires a target argument")?;
    let handler = args
        .get(1)
        .ok_or("Proxy.revocable requires a handler argument")?;

    let target_obj = target.as_object().ok_or("Proxy target must be an object")?;

    let handler_obj = handler
        .as_object()
        .ok_or("Proxy handler must be an object")?;

    let revocable = JsProxy::revocable(target_obj, handler_obj);

    let result = GcRef::new(JsObject::new(None, Arc::clone(&mm)));
    result.set("proxy".into(), VmValue::proxy(revocable.proxy));

    // Create native function for revoke
    let revoke_fn = revocable.revoke;
    result.set(
        "revoke".into(),
        VmValue::native_function(
            move |_this: &VmValue, _args: &[VmValue], _mm: Arc<memory::MemoryManager>| {
                revoke_fn();
                Ok(VmValue::undefined())
            },
            Arc::clone(&mm),
        ),
    );

    Ok(VmValue::object(result))
}

/// Get proxy target
/// Args: [proxy]
/// Returns: target object or undefined if revoked
fn native_proxy_get_target(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, String> {
    let proxy_val = args.first().ok_or("Missing proxy argument")?;

    let proxy = proxy_val.as_proxy().ok_or("Argument must be a proxy")?;

    match proxy.target() {
        Some(target) => Ok(VmValue::object(target)),
        None => Err("Cannot perform operation on a revoked proxy".to_string()),
    }
}

/// Get proxy handler
/// Args: [proxy]
/// Returns: handler object or throws if revoked
fn native_proxy_get_handler(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, String> {
    let proxy_val = args.first().ok_or("Missing proxy argument")?;

    let proxy = proxy_val.as_proxy().ok_or("Argument must be a proxy")?;

    match proxy.handler() {
        Some(handler) => Ok(VmValue::object(handler)),
        None => Err("Cannot perform operation on a revoked proxy".to_string()),
    }
}

/// Check if proxy is revoked
/// Args: [proxy]
/// Returns: boolean
fn native_proxy_is_revoked(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, String> {
    let proxy_val = args.first().ok_or("Missing proxy argument")?;

    let proxy = proxy_val.as_proxy().ok_or("Argument must be a proxy")?;

    Ok(VmValue::boolean(proxy.is_revoked()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_proxy_create() {
        let mm = Arc::new(memory::MemoryManager::test());
        let target = GcRef::new(JsObject::new(None, Arc::clone(&mm)));
        let handler = GcRef::new(JsObject::new(None, Arc::clone(&mm)));

        let result =
            native_proxy_create(&[VmValue::object(target), VmValue::object(handler)], mm).unwrap();

        assert!(result.is_proxy());
        assert!(!result.as_proxy().unwrap().is_revoked());
    }

    #[test]
    fn test_proxy_create_invalid_target() {
        let mm = Arc::new(memory::MemoryManager::test());
        let handler = GcRef::new(JsObject::new(None, Arc::clone(&mm)));
        let result = native_proxy_create(
            &[VmValue::number(42.0), VmValue::object(handler)],
            Arc::clone(&mm),
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("target must be an object"));
    }

    #[test]
    fn test_proxy_revocable() {
        let mm = Arc::new(memory::MemoryManager::test());
        let target = GcRef::new(JsObject::new(None, Arc::clone(&mm)));
        let handler = GcRef::new(JsObject::new(None, Arc::clone(&mm)));

        let result =
            native_proxy_revocable(&[VmValue::object(target), VmValue::object(handler)], mm)
                .unwrap();

        assert!(result.is_object());
        let obj = result.as_object().unwrap();

        let proxy = obj.get(&"proxy".into()).unwrap();
        assert!(proxy.is_proxy());

        let revoke = obj.get(&"revoke".into()).unwrap();
        assert!(revoke.is_native_function());
    }

    #[test]
    fn test_proxy_is_revoked() {
        let mm = Arc::new(memory::MemoryManager::test());
        let target = GcRef::new(JsObject::new(None, Arc::clone(&mm)));
        let handler = GcRef::new(JsObject::new(None, Arc::clone(&mm)));
        let proxy = JsProxy::new(target, handler);
        let proxy_val = VmValue::proxy(proxy.clone());

        let result =
            native_proxy_is_revoked(std::slice::from_ref(&proxy_val), Arc::clone(&mm)).unwrap();
        assert_eq!(result.as_boolean(), Some(false));

        proxy.revoke();
        let result = native_proxy_is_revoked(&[proxy_val], Arc::clone(&mm)).unwrap();
        assert_eq!(result.as_boolean(), Some(true));
    }

    #[test]
    fn test_proxy_get_target() {
        let mm = Arc::new(memory::MemoryManager::test());
        let target = GcRef::new(JsObject::new(None, Arc::clone(&mm)));
        target.set("x".into(), VmValue::number(42.0));
        let handler = GcRef::new(JsObject::new(None, Arc::clone(&mm)));
        let proxy = JsProxy::new(target, handler);

        let result = native_proxy_get_target(&[VmValue::proxy(proxy)], Arc::clone(&mm)).unwrap();
        assert!(result.is_object());
        let obj = result.as_object().unwrap();
        assert_eq!(obj.get(&"x".into()).unwrap().as_number(), Some(42.0));
    }

    #[test]
    fn test_proxy_get_target_revoked() {
        let mm = Arc::new(memory::MemoryManager::test());
        let target = GcRef::new(JsObject::new(None, Arc::clone(&mm)));
        let handler = GcRef::new(JsObject::new(None, Arc::clone(&mm)));
        let proxy = JsProxy::new(target, handler);
        proxy.revoke();

        let result = native_proxy_get_target(&[VmValue::proxy(proxy)], Arc::clone(&mm));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("revoked"));
    }
}
