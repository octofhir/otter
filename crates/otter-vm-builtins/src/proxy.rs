//! Proxy built-in
//!
//! Provides Proxy constructor and methods:
//! - `new Proxy(target, handler)`
//! - `Proxy.revocable(target, handler)`
//!
//! Proxy traps are called from JavaScript via the handler object.

use otter_vm_core::error::VmError;
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
) -> Result<VmValue, VmError> {
    let target = args.first().ok_or("Proxy requires a target argument")?;
    let handler = args.get(1).ok_or("Proxy requires a handler argument")?;

    // Validate target is an object (not null/undefined/primitive)
    if !target.is_object() {
        return Err(VmError::type_error("Proxy target must be an object"));
    }

    // Validate handler is an object (not null/undefined/primitive)
    if !handler.is_object() {
        return Err(VmError::type_error("Proxy handler must be an object"));
    }

    let proxy = JsProxy::new(target.clone(), handler.clone());
    Ok(VmValue::proxy(proxy))
}

/// Create a revocable proxy
/// Args: [target, handler]
/// Returns: { proxy, revoke }
fn native_proxy_revocable(
    args: &[VmValue],
    mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, VmError> {
    let target = args
        .first()
        .ok_or("Proxy.revocable requires a target argument")?;
    let handler = args
        .get(1)
        .ok_or("Proxy.revocable requires a handler argument")?;

    if !target.is_object() {
        return Err(VmError::type_error("Proxy target must be an object"));
    }
    if !handler.is_object() {
        return Err(VmError::type_error("Proxy handler must be an object"));
    }

    let revocable = JsProxy::revocable(target.clone(), handler.clone());

    let result = GcRef::new(JsObject::new(VmValue::null(), Arc::clone(&mm)));
    let _ = result.set("proxy".into(), VmValue::proxy(revocable.proxy));

    // Create native function for revoke
    let revoke_fn = revocable.revoke;
    let _ = result.set(
        "revoke".into(),
        VmValue::native_function(
            move |_this: &VmValue,
                  _args: &[VmValue],
                  _ncx: &mut otter_vm_core::context::NativeContext<'_>| {
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
) -> Result<VmValue, VmError> {
    let proxy_val = args.first().ok_or("Missing proxy argument")?;

    let proxy = proxy_val.as_proxy().ok_or("Argument must be a proxy")?;

    match proxy.target() {
        Some(target) => Ok(target),
        None => Err(VmError::type_error(
            "Cannot perform operation on a revoked proxy",
        )),
    }
}

/// Get proxy handler
/// Args: [proxy]
/// Returns: handler object or throws if revoked
fn native_proxy_get_handler(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, VmError> {
    let proxy_val = args.first().ok_or("Missing proxy argument")?;

    let proxy = proxy_val.as_proxy().ok_or("Argument must be a proxy")?;

    match proxy.handler() {
        Some(handler) => Ok(handler),
        None => Err(VmError::type_error(
            "Cannot perform operation on a revoked proxy",
        )),
    }
}

/// Check if proxy is revoked
/// Args: [proxy]
/// Returns: boolean
fn native_proxy_is_revoked(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, VmError> {
    let proxy_val = args.first().ok_or("Missing proxy argument")?;

    let proxy = proxy_val.as_proxy().ok_or("Argument must be a proxy")?;

    Ok(VmValue::boolean(proxy.is_revoked()))
}

// TODO: Tests need to be updated to use NativeContext instead of Arc<MemoryManager>
