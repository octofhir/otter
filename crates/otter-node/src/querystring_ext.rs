//! Querystring extension module.
//!
//! Provides node:querystring compatible query string parsing/formatting.
//!
//! This module is purely JavaScript-based and doesn't require native ops.
//! All parsing logic is implemented in JavaScript.

use otter_runtime::Extension;

/// Create the querystring extension.
///
/// Provides Node.js-compatible querystring APIs:
/// - querystring.parse(str, sep, eq, options)
/// - querystring.stringify(obj, sep, eq, options)
/// - querystring.encode (alias for stringify)
/// - querystring.decode (alias for parse)
/// - querystring.escape(str)
/// - querystring.unescape(str)
pub fn extension() -> Extension {
    Extension::new("querystring").with_js(include_str!("querystring.js"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extension_creation() {
        let ext = extension();
        assert_eq!(ext.name(), "querystring");
        assert!(ext.js_code().is_some());
    }

    #[test]
    fn test_js_code_contains_querystring() {
        let ext = extension();
        let js = ext.js_code().unwrap();
        assert!(js.contains("parse"));
        assert!(js.contains("stringify"));
        assert!(js.contains("escape"));
        assert!(js.contains("unescape"));
        assert!(js.contains("__registerNodeBuiltin"));
    }
}
