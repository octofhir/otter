//! Events extension module using the new architecture.
//!
//! This module provides the node:events extension using the cleaner
//! `include_str!` approach for JavaScript code.
//!
//! ## Architecture
//!
//! - `events.rs` - Rust implementation of EventEmitter (optional native support)
//! - `events_ext.rs` - Extension creation
//! - `events.js` - JavaScript EventEmitter implementation

use otter_runtime::Extension;

/// Create the events extension.
///
/// This extension provides the Node.js-compatible EventEmitter class.
/// The implementation is purely JavaScript-based.
pub fn extension() -> Extension {
    Extension::new("events").with_js(include_str!("events.js"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extension_creation() {
        let ext = extension();
        assert_eq!(ext.name(), "events");
        assert!(ext.js_code().is_some());
    }
}
