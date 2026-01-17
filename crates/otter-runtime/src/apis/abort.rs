//! AbortController/AbortSignal API implementation.
//!
//! Provides the Web Cancellation API for async operation cancellation:
//! - `EventTarget` - Base class for event handling
//! - `AbortSignal` - Represents a signal to abort an operation
//! - `AbortController` - Creates and controls an AbortSignal
//! - `DOMException` - Standard exception type for AbortError/TimeoutError
//!
//! # Example
//!
//! ```typescript
//! const controller = new AbortController();
//!
//! // Listen for abort
//! controller.signal.addEventListener('abort', () => {
//!     console.log('Operation aborted!');
//! });
//!
//! // Abort after 1 second
//! setTimeout(() => controller.abort('Timeout'), 1000);
//!
//! // Or use AbortSignal.timeout()
//! const signal = AbortSignal.timeout(5000);
//! fetch(url, { signal });
//! ```

use crate::bindings::*;
use crate::error::JscResult;
use std::ffi::CString;
use std::ptr;

/// JavaScript shim that implements EventTarget, AbortSignal, and AbortController
const ABORT_SHIM: &str = include_str!("abort_shim.js");

/// Register the AbortController API on a context.
///
/// This registers the following globals:
/// - `EventTarget` - Base class for event handling
/// - `AbortSignal` - Signal for aborting operations
/// - `AbortController` - Controller that creates AbortSignal
/// - `DOMException` - Exception type (if not already defined)
/// - `Event` - Basic event class (if not already defined)
pub fn register_abort_api(ctx: JSContextRef) -> JscResult<()> {
    unsafe {
        // Execute the JavaScript shim to register all classes
        let shim_cstr = CString::new(ABORT_SHIM).expect("ABORT_SHIM contains null byte");
        let shim_ref = JSStringCreateWithUTF8CString(shim_cstr.as_ptr());

        let source_cstr = CString::new("<otter_abort_shim>").unwrap();
        let source_ref = JSStringCreateWithUTF8CString(source_cstr.as_ptr());

        let mut exception: JSValueRef = ptr::null_mut();
        JSEvaluateScript(ctx, shim_ref, ptr::null_mut(), source_ref, 1, &mut exception);

        JSStringRelease(shim_ref);
        JSStringRelease(source_ref);

        if !exception.is_null() {
            return Err(crate::value::extract_exception(ctx, exception).into());
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    // Tests are run via JavaScript integration tests
    // See tests/abort_test.js
}
