//! Assert extension module.
//!
//! Provides node:assert compatible assertion utilities.
//!
//! This module is purely JavaScript-based and doesn't require native ops.
//! All assertion logic is implemented in JavaScript.

use otter_runtime::Extension;

/// Create the assert extension.
///
/// Provides Node.js-compatible assertion APIs:
/// - assert(value, message)
/// - assert.ok, assert.equal, assert.strictEqual
/// - assert.deepEqual, assert.deepStrictEqual
/// - assert.throws, assert.rejects
/// - assert.fail, assert.ifError
/// - assert.match, assert.doesNotMatch
/// - AssertionError class
pub fn extension() -> Extension {
    Extension::new("assert").with_js(include_str!("assert.js"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extension_creation() {
        let ext = extension();
        assert_eq!(ext.name(), "assert");
        assert!(ext.js_code().is_some());
    }

    #[test]
    fn test_js_code_contains_assert() {
        let ext = extension();
        let js = ext.js_code().unwrap();
        assert!(js.contains("AssertionError"));
        assert!(js.contains("strictEqual"));
        assert!(js.contains("deepEqual"));
        assert!(js.contains("throws"));
        assert!(js.contains("__registerNodeBuiltin"));
    }
}
