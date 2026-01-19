//! Node.js async_hooks module extension (stub implementation)
//!
//! Provides async context tracking APIs that Express dependencies need.
//! This is a Phase 1 stub - actual async context tracking can be added later.
//!
//! # APIs Provided
//!
//! - `executionAsyncId()` - Returns current async context ID
//! - `triggerAsyncId()` - Returns trigger async context ID
//! - `AsyncResource` - Class for async resource tracking
//! - `AsyncLocalStorage` - Class for context propagation
//! - `createHook()` - Creates async hooks (stub)
//!
//! # Example
//!
//! ```javascript
//! import { AsyncLocalStorage } from 'node:async_hooks';
//!
//! const als = new AsyncLocalStorage();
//! als.run({ user: 'alice' }, () => {
//!     console.log(als.getStore()); // { user: 'alice' }
//! });
//! ```

use otter_runtime::Extension;

/// Create the async_hooks extension.
///
/// This extension provides Node.js-compatible async_hooks APIs.
/// The implementation is purely JavaScript-based (Phase 1 stub).
pub fn extension() -> Extension {
    Extension::new("async_hooks").with_js(include_str!("async_hooks.js"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extension_creation() {
        let ext = extension();
        assert_eq!(ext.name(), "async_hooks");
        assert!(ext.js_code().is_some());
    }

    #[test]
    fn test_js_contains_async_resource() {
        let ext = extension();
        let js = ext.js_code().expect("JS code should exist");
        assert!(js.contains("class AsyncResource"));
        assert!(js.contains("class AsyncLocalStorage"));
        assert!(js.contains("executionAsyncId"));
        assert!(js.contains("createHook"));
    }
}
