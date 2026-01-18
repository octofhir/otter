//! StringDecoder extension module.
//!
//! Provides the node:string_decoder module for decoding Buffer to strings
//! with proper handling of incomplete multi-byte sequences across chunk boundaries.

use otter_runtime::Extension;

/// Create the string_decoder extension.
///
/// This extension provides the Node.js-compatible StringDecoder class:
/// - `new StringDecoder(encoding)` - create a decoder
/// - `decoder.write(buffer)` - decode buffer to string
/// - `decoder.end(buffer?)` - finish decoding and return remaining bytes
///
/// The implementation is purely JavaScript-based using TextDecoder.
pub fn extension() -> Extension {
    Extension::new("string_decoder").with_js(include_str!("string_decoder.js"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extension_creation() {
        let ext = extension();
        assert_eq!(ext.name(), "string_decoder");
        assert!(ext.js_code().is_some());
    }
}
