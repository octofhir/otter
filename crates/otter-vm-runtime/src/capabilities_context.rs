//! Thread-local capabilities context for security checks in ops
//!
//! This module provides a mechanism to pass capabilities to ops during execution.
//! The runtime sets capabilities before executing ops, and ops can check permissions.
//!
//! # Usage
//!
//! In the runtime (before calling ops):
//! ```ignore
//! let _guard = CapabilitiesGuard::new(caps);
//! // Execute ops here - they can now check capabilities
//! // Guard drops when scope ends, clearing capabilities
//! ```
//!
//! In ops:
//! ```ignore
//! use otter_vm_runtime::capabilities_context;
//!
//! // Check net permission
//! if !capabilities_context::can_net(host) {
//!     return Err(format!("Network access denied for: {}", host));
//! }
//! ```

use crate::capabilities::Capabilities;
use std::cell::RefCell;

thread_local! {
    /// Current capabilities for this thread
    static CAPABILITIES: RefCell<Option<Capabilities>> = const { RefCell::new(None) };
}

/// Set the capabilities for the current thread
pub fn set_capabilities(caps: Capabilities) {
    CAPABILITIES.with(|c| {
        *c.borrow_mut() = Some(caps);
    });
}

/// Clear the capabilities for the current thread
pub fn clear_capabilities() {
    CAPABILITIES.with(|c| {
        *c.borrow_mut() = None;
    });
}

/// Execute a closure with access to current capabilities
///
/// Returns the default value if no capabilities are set (secure default: deny)
pub fn with_capabilities<F, R>(f: F) -> R
where
    F: FnOnce(&Capabilities) -> R,
    R: Default,
{
    CAPABILITIES.with(|c| {
        let borrowed = c.borrow();
        match borrowed.as_ref() {
            Some(caps) => f(caps),
            None => R::default(), // Secure default: return false/deny
        }
    })
}

/// Check if capabilities are set for the current thread
pub fn has_capabilities() -> bool {
    CAPABILITIES.with(|c| c.borrow().is_some())
}

/// Check if network access is allowed for a host
pub fn can_net(host: &str) -> bool {
    with_capabilities(|caps| caps.can_net(host))
}

/// Check if environment variable access is allowed
pub fn can_env(var: &str) -> bool {
    with_capabilities(|caps| caps.can_env(var))
}

/// Check if file read is allowed
pub fn can_read(path: &str) -> bool {
    with_capabilities(|caps| caps.can_read(path))
}

/// Check if file write is allowed
pub fn can_write(path: &str) -> bool {
    with_capabilities(|caps| caps.can_write(path))
}

/// RAII guard for capabilities - clears when dropped
pub struct CapabilitiesGuard {
    _private: (),
}

impl CapabilitiesGuard {
    /// Create a new guard that sets capabilities for the current scope
    pub fn new(caps: Capabilities) -> Self {
        set_capabilities(caps);
        Self { _private: () }
    }
}

impl Drop for CapabilitiesGuard {
    fn drop(&mut self) {
        clear_capabilities();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::CapabilitiesBuilder;

    #[test]
    fn test_no_capabilities_denies_all() {
        // No capabilities set = deny all
        assert!(!can_net("example.com"));
        assert!(!can_env("HOME"));
        assert!(!can_read("/etc/passwd"));
    }

    #[test]
    fn test_capabilities_guard() {
        let caps = CapabilitiesBuilder::new().allow_net_all().build();

        {
            let _guard = CapabilitiesGuard::new(caps);
            assert!(can_net("example.com"));
        }

        // After guard drops, capabilities are cleared
        assert!(!can_net("example.com"));
    }

    #[test]
    fn test_with_capabilities() {
        let caps = CapabilitiesBuilder::new()
            .allow_net(vec!["api.example.com".into()])
            .build();

        let _guard = CapabilitiesGuard::new(caps);

        // Can access allowed host
        assert!(with_capabilities(|c| c.can_net("api.example.com")));

        // Cannot access other hosts
        assert!(!with_capabilities(|c| c.can_net("other.com")));
    }
}
