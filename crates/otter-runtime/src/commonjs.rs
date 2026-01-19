//! CommonJS module support and ESM/CJS interoperability.
//!
//! This module provides runtime helpers for CommonJS modules:
//! - `__commonJS` - Lazy wrapper for CJS modules with caching
//! - `__toESM` - Convert CJS module to ESM format
//! - `__toCommonJS` - Convert ESM module to CJS format
//! - `__createRequire` - Create a require function for a module context

use crate::JscResult;
use crate::bindings::*;
use std::ffi::CString;
use std::ptr;

/// The CommonJS runtime JavaScript code
const COMMONJS_RUNTIME: &str = include_str!("commonjs_runtime.js");

/// Register CommonJS runtime helpers in the global context.
///
/// This must be called before any CommonJS module loading.
pub fn register_commonjs_runtime(ctx: JSContextRef) -> JscResult<()> {
    unsafe {
        let script_cstr =
            CString::new(COMMONJS_RUNTIME).expect("COMMONJS_RUNTIME contains null byte");
        let script_ref = JSStringCreateWithUTF8CString(script_cstr.as_ptr());

        let source_cstr = CString::new("<otter_commonjs_runtime>").unwrap();
        let source_ref = JSStringCreateWithUTF8CString(source_cstr.as_ptr());

        let mut exception: JSValueRef = ptr::null_mut();
        JSEvaluateScript(
            ctx,
            script_ref,
            ptr::null_mut(),
            source_ref,
            1,
            &mut exception,
        );

        JSStringRelease(script_ref);
        JSStringRelease(source_ref);

        if !exception.is_null() {
            return Err(crate::value::extract_exception(ctx, exception).into());
        }
    }

    Ok(())
}

/// Transform a CommonJS module source to use the wrapper pattern.
///
/// # Input
/// ```javascript
/// const fs = require('fs');
/// module.exports = { foo: 1 };
/// ```
///
/// # Output
/// ```javascript
/// var require_mymodule = __commonJS((exports, module) => {
///     const fs = require('fs');
///     module.exports = { foo: 1 };
/// });
/// ```
///
/// The `dependencies` map is passed to `__createRequire` so that bare specifiers
/// like `require('combined-stream')` can be resolved to their full URLs.
pub fn wrap_commonjs_module(
    module_id: &str,
    source: &str,
    dirname: &str,
    filename: &str,
    dependencies: &std::collections::HashMap<String, String>,
) -> String {
    // Serialize dependencies as JSON object for runtime resolution
    let deps_json = serde_json::to_string(dependencies).unwrap_or_else(|_| "{}".to_string());

    format!(
        r#"globalThis.__otter_cjs_modules["{module_id}"] = __commonJS(function(exports, module) {{
    var __dirname = "{dirname}";
    var __filename = "{filename}";
    var __deps = {deps_json};
    var require = __createRequire(__dirname, __filename, __deps);
    {source}
}});
"#,
        module_id = escape_string(module_id),
        dirname = escape_string(dirname),
        filename = escape_string(filename),
        deps_json = deps_json,
        source = source,
    )
}

/// Create a sanitized identifier from a module path
#[cfg(test)]
fn sanitize_module_id(id: &str) -> String {
    let mut result = String::with_capacity(id.len());
    for c in id.chars() {
        match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '_' => result.push(c),
            '/' | '\\' | '-' | '.' | '@' => result.push('_'),
            _ => {}
        }
    }
    // Ensure it doesn't start with a digit
    if result.starts_with(|c: char| c.is_ascii_digit()) {
        result.insert(0, '_');
    }
    if result.is_empty() {
        result = "_module".to_string();
    }
    result
}

/// Escape a string for use in JavaScript
fn escape_string(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => result.push_str("\\\""),
            '\\' => result.push_str("\\\\"),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            _ => result.push(c),
        }
    }
    result
}

/// Generate code to require a CommonJS module (for use as entry point)
pub fn require_module(module_id: &str) -> String {
    format!(
        r#"(function() {{
    var mod = globalThis.__otter_cjs_modules["{}"];
    if (mod) return mod();
    throw new Error("Module not found: {}");
}})();
"#,
        escape_string(module_id),
        escape_string(module_id)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_module_id() {
        assert_eq!(sanitize_module_id("foo"), "foo");
        assert_eq!(sanitize_module_id("foo/bar"), "foo_bar");
        assert_eq!(sanitize_module_id("./foo.js"), "__foo_js");
        assert_eq!(sanitize_module_id("@scope/pkg"), "_scope_pkg");
        assert_eq!(sanitize_module_id("123"), "_123");
    }

    #[test]
    fn test_escape_string() {
        assert_eq!(escape_string("hello"), "hello");
        assert_eq!(escape_string("hello\"world"), "hello\\\"world");
        assert_eq!(escape_string("path\\to\\file"), "path\\\\to\\\\file");
        assert_eq!(escape_string("line\nbreak"), "line\\nbreak");
    }

    #[test]
    fn test_wrap_commonjs_module() {
        let source = "module.exports = { foo: 1 };";
        let wrapped = wrap_commonjs_module(
            "file:///project/lib.cjs",
            source,
            "/project",
            "/project/lib.cjs",
        );

        assert!(wrapped.contains("__otter_cjs_modules[\"file:///project/lib.cjs\"]"));
        assert!(wrapped.contains("__commonJS"));
        assert!(wrapped.contains("var __dirname = \"/project\""));
        assert!(wrapped.contains("var __filename = \"/project/lib.cjs\""));
        assert!(wrapped.contains("var require = __createRequire"));
        assert!(wrapped.contains("module.exports = { foo: 1 };"));
    }

    #[test]
    fn test_require_module() {
        let code = require_module("file:///project/main.cjs");
        assert!(code.contains("__otter_cjs_modules[\"file:///project/main.cjs\"]"));
    }
}
