//! Node.js async_hooks module extension (Phase 1 implementation).
//!
//! Provides async context tracking APIs that Express dependencies need.
//! The async context state is tracked in Rust and exposed via #[dive] ops,
//! while the JS shim provides the Node.js API surface.
//!
//! # APIs Provided
//!
//! - `executionAsyncId()` - Returns current async context ID
//! - `triggerAsyncId()` - Returns trigger async context ID
//! - `executionAsyncResource()` - Returns current async resource
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

use otter_macros::dive;
use otter_runtime::Extension;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct AsyncIdSnapshot {
    async_id: u64,
    trigger_async_id: u64,
}

#[derive(Debug, Clone, Copy)]
struct AsyncHooksState {
    next_async_id: u64,
    current_async_id: u64,
    current_trigger_async_id: u64,
}

impl Default for AsyncHooksState {
    fn default() -> Self {
        Self {
            next_async_id: 1,
            current_async_id: 1,
            current_trigger_async_id: 0,
        }
    }
}

thread_local! {
    static ASYNC_HOOKS_STATE: RefCell<AsyncHooksState> = RefCell::new(AsyncHooksState::default());
}

// ============================================================================
// Dive Functions - Each becomes a callable JS function
// ============================================================================

#[dive(swift)]
fn __otter_async_hooks_execution_async_id() -> u64 {
    ASYNC_HOOKS_STATE.with(|state| state.borrow().current_async_id)
}

#[dive(swift)]
fn __otter_async_hooks_trigger_async_id() -> u64 {
    ASYNC_HOOKS_STATE.with(|state| state.borrow().current_trigger_async_id)
}

#[dive(swift)]
fn __otter_async_hooks_next_async_id() -> u64 {
    ASYNC_HOOKS_STATE.with(|state| {
        let mut state = state.borrow_mut();
        state.next_async_id += 1;
        state.next_async_id
    })
}

#[dive(swift)]
fn __otter_async_hooks_set_current(async_id: u64, trigger_async_id: u64) -> AsyncIdSnapshot {
    ASYNC_HOOKS_STATE.with(|state| {
        let mut state = state.borrow_mut();
        let previous = AsyncIdSnapshot {
            async_id: state.current_async_id,
            trigger_async_id: state.current_trigger_async_id,
        };
        state.current_async_id = async_id;
        state.current_trigger_async_id = trigger_async_id;
        previous
    })
}

/// Create the async_hooks extension.
///
/// This extension provides Node.js-compatible async_hooks APIs.
/// Async context IDs are managed in Rust with #[dive] ops.
pub fn extension() -> Extension {
    Extension::new("async_hooks")
        .with_ops(vec![
            __otter_async_hooks_execution_async_id_dive_decl(),
            __otter_async_hooks_trigger_async_id_dive_decl(),
            __otter_async_hooks_next_async_id_dive_decl(),
            __otter_async_hooks_set_current_dive_decl(),
        ])
        .with_js(include_str!("async_hooks.js"))
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
        assert!(js.contains("executionAsyncResource"));
        assert!(js.contains("createHook"));
    }
}
