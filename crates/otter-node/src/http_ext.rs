//! HTTP extension module using the new architecture.
//!
//! This module provides the node:http extension as a JavaScript wrapper
//! over Otter.serve().
//!
//! ## Architecture
//!
//! - `http.js` - JavaScript HTTP module implementation
//! - `http_ext.rs` - Extension creation
//!
//! Note: This is a pure JavaScript wrapper that doesn't require additional Rust ops.

use otter_runtime::Extension;

/// Create the http extension.
///
/// This extension provides Node.js-compatible HTTP server API built on top of Otter.serve().
/// It's a pure JavaScript wrapper that doesn't require additional Rust ops.
pub fn extension() -> Extension {
    Extension::new("http").with_js(include_str!("http.js"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extension_creation() {
        let ext = extension();
        assert_eq!(ext.name(), "http");
        assert!(ext.js_code().is_some());
    }
}
