//! JavaScript Proxy implementation for the VM
//!
//! Proxies allow custom behavior for fundamental operations on objects.
//!
//! ## Usage
//!
//! ```ignore
//! // ...
//! ```

use crate::gc::GcRef;
use crate::object::JsObject;
use crate::value::Value;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// A JavaScript Proxy object
///
/// Proxies intercept fundamental operations on target objects
/// through handler traps.
pub struct JsProxy {
    /// The target object being proxied
    pub(crate) target: Value,
    /// The handler object containing traps
    pub(crate) handler: Value,
    /// Whether this proxy has been revoked
    revoked: AtomicBool,
}

impl std::fmt::Debug for JsProxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_revoked() {
            write!(f, "Proxy {{ <revoked> }}")
        } else {
            write!(f, "Proxy {{ target: {:?} }}", self.target)
        }
    }
}

/// Result of creating a revocable proxy
pub struct RevocableProxy {
    /// The proxy object
    pub proxy: Arc<JsProxy>,
    /// Function to revoke the proxy (internally calls `proxy.revoke()`)
    pub revoke: Arc<dyn Fn() + Send + Sync>,
}

impl JsProxy {
    /// Create a new proxy
    pub fn new(target: Value, handler: Value) -> Arc<Self> {
        Arc::new(Self {
            target,
            handler,
            revoked: AtomicBool::new(false),
        })
    }

    /// Create a revocable proxy
    pub fn revocable(target: Value, handler: Value) -> RevocableProxy {
        let proxy = Self::new(target, handler);
        let proxy_for_revoke = proxy.clone();

        RevocableProxy {
            proxy,
            revoke: Arc::new(move || {
                proxy_for_revoke.revoke();
            }),
        }
    }

    /// Get the target object
    ///
    /// Returns `None` if the proxy has been revoked.
    pub fn target(&self) -> Option<Value> {
        if self.is_revoked() {
            None
        } else {
            Some(self.target.clone())
        }
    }

    /// Get the raw target value without revocation checks.
    pub fn target_raw(&self) -> &Value {
        &self.target
    }

    /// Get the handler object
    ///
    /// Returns `None` if the proxy has been revoked.
    pub fn handler(&self) -> Option<Value> {
        if self.is_revoked() {
            None
        } else {
            Some(self.handler.clone())
        }
    }

    /// Check if this proxy has been revoked
    pub fn is_revoked(&self) -> bool {
        self.revoked.load(Ordering::Acquire)
    }

    /// Revoke this proxy
    ///
    /// After revocation, all trap operations will throw a TypeError.
    pub fn revoke(&self) {
        self.revoked.store(true, Ordering::Release);
    }

    /// Get a trap from the handler
    ///
    /// Returns `None` if:
    /// - The proxy is revoked
    /// - The handler doesn't have the trap
    /// - The trap is `undefined` or `null`
    pub fn get_trap(&self, trap_name: &str) -> Option<Value> {
        if self.is_revoked() {
            return None;
        }

        let handler = self.handler.as_object()?;
        let trap = handler.get(&trap_name.into())?;

        // Return None for undefined/null traps (allows fallthrough to target)
        if trap.is_undefined() || trap.is_null() {
            return None;
        }

        Some(trap)
    }

    /// Check if the proxy has a specific trap
    pub fn has_trap(&self, trap_name: &str) -> bool {
        self.get_trap(trap_name).is_some()
    }

    /// Extract values from this proxy and clear its state.
    /// Used for iterative destruction to prevent stack overflow.
    pub fn clear_and_extract_values(&self) -> Vec<Value> {
        self.revoke();
        vec![self.target.clone(), self.handler.clone()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_proxy_creation() {
        let memory_manager = Arc::new(crate::memory::MemoryManager::test());
        let target = GcRef::new(JsObject::new(Value::null(), memory_manager.clone()));
        let handler = GcRef::new(JsObject::new(Value::null(), memory_manager.clone()));
        let proxy = JsProxy::new(Value::object(target), Value::object(handler));

        assert!(!proxy.is_revoked());
        assert!(proxy.target().is_some());
        assert!(proxy.handler().is_some());
    }

    #[test]
    fn test_proxy_revoke() {
        let memory_manager = Arc::new(crate::memory::MemoryManager::test());
        let target = GcRef::new(JsObject::new(Value::null(), memory_manager.clone()));
        let handler = GcRef::new(JsObject::new(Value::null(), memory_manager.clone()));
        let proxy = JsProxy::new(Value::object(target), Value::object(handler));

        assert!(!proxy.is_revoked());
        proxy.revoke();
        assert!(proxy.is_revoked());
        assert!(proxy.target().is_none());
        assert!(proxy.handler().is_none());
    }

    #[test]
    fn test_revocable_proxy() {
        let memory_manager = Arc::new(crate::memory::MemoryManager::test());
        let target = GcRef::new(JsObject::new(Value::null(), memory_manager.clone()));
        let handler = GcRef::new(JsObject::new(Value::null(), memory_manager.clone()));
        let RevocableProxy { proxy, revoke } =
            JsProxy::revocable(Value::object(target), Value::object(handler));

        assert!(!proxy.is_revoked());
        revoke();
        assert!(proxy.is_revoked());
    }

    #[test]
    fn test_get_trap_missing() {
        let memory_manager = Arc::new(crate::memory::MemoryManager::test());
        let target = GcRef::new(JsObject::new(Value::null(), memory_manager.clone()));
        let handler = GcRef::new(JsObject::new(Value::null(), memory_manager.clone()));
        let proxy = JsProxy::new(Value::object(target), Value::object(handler));

        assert!(proxy.get_trap("get").is_none());
        assert!(!proxy.has_trap("get"));
    }
}
