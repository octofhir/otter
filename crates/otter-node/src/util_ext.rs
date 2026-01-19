//! Util extension module using the new architecture.
//!
//! This module provides the node:util extension using the cleaner
//! `include_str!` approach for JavaScript code.
//!
//! ## Architecture
//!
//! - `util_ext.rs` - Extension creation
//! - `util.js` - JavaScript util module implementation

use otter_runtime::Extension;

/// Create the util extension.
///
/// This extension provides Node.js-compatible util functions:
/// - `util.promisify`
/// - `util.format`
/// - `util.inspect`
///
/// The implementation is purely JavaScript-based.
pub fn extension() -> Extension {
    Extension::new("util").with_js(include_str!("util.js"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extension_creation() {
        let ext = extension();
        assert_eq!(ext.name(), "util");
        assert!(ext.js_code().is_some());
    }
}
